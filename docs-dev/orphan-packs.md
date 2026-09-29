# Interrupted pushes: the packs they strand, and what happens to them

A push writes its data and tree packs first and its index file last, so a push
killed in between leaves **complete packs that no index file names**. This note
records what that costs, what `rustic_core` 0.12.0 offers against it, what this
project does instead, and why the safety argument holds when a second machine is
pushing at the same time.

## The defect, as measured

A 560 MB payload was pushed and killed at three points (W242):

| kill point | stranded | old re-run uploaded | old final repository |
|---|---|---|---|
| 1 pack finalized | 33.6 MB | 560 MB (100 %) | 454 MB |
| 5 packs finalized | 171.3 MB | 560 MB (100 %) | 592 MB |
| 10 packs finalized | 343.0 MB | 560 MB (100 %) | 764 MB |

Every re-run reported `files_unmodified=0` and re-uploaded the whole payload, and
the stranded bytes were permanent. Both halves have the same cause: a stored
object's id covers bytes carrying a fresh AEAD nonce, so re-uploading the same
plaintext produces **different bytes under a different id**, and a killed push
leaves no parent snapshot for the file-level `files_unmodified` comparison to
work against. Nothing could ever match the stranded copy, and `append_only:true`
(ADR-001) blocks the one command that would remove it.

## What `rustic_core` 0.12.0 offers

Three options were read out of the pinned source before writing anything:

1. **Flush the index more often during a backup.** The indexer does flush on its
   own — every 300 s or 50 000 blobs — but both are private constants with no
   option, so the size of the window cannot be tuned from here. This bounds the
   waste to a flush interval rather than removing it, and the interval is a
   dependency's, not ours.
2. **`repair index`** re-indexes unindexed packs, which is exactly the mechanism
   wanted: it reads pack headers and writes index entries for packs the index
   does not name. It refuses an append-only repository outright, and it rewrites
   (removes) the index files it replaces.
3. **`prune`** handles unindexed packs by marking them for deletion, then
   deleting them on a later run after a grace period — and it too refuses an
   append-only repository.

ADR-001 opened every repository with `append_only:true` on purpose — a policy
marker in the config, costing nothing, that blocks the whole delete-class family
(`prune`, `forget`, `rewrite`, `repair`, `delete_snapshots`) — and ADR-016 raised
"we never prune" from a flag to an invariant, with real cleanup reserved for a
deliberately planned single-writer action. Options 2 and 3 are therefore not reachable from a daily
code path without weakening a recorded decision, and option 2's index rewrite is
the wrong shape for a scheduled run in any case.

## What this project does: adopt on open

`Repository::to_indexed_checked()` lists the packs and reads the header of any
pack the index does not name, so the index those packs are missing is rebuilt in
memory. That is the same re-indexing `repair index` performs, minus the index
rewrite and minus the `append_only` refusal — and it needs no delete path at all.
A later `backup` then finds every one of those blobs already present and uploads
none of them.

**A header is not a pack, so a header read is not a verification.** The checked
pass parses a pack's encrypted header and compares the lengths declared in it
with the file size; it never decrypts a blob. A pack whose *ciphertext* was
damaged in place — a torn write, a bit-flipping store — passes that check, and its
blobs would enter the dedup index, where a later backup would skip uploading
content no reader can decrypt. Before the checked pass runs, this project
therefore reads the header of every pack the index does not name *and every blob
that header declares* (`crates/chat-stasher/src/packcheck.rs:93`, called from
`crates/chat-stasher/src/orphans.rs:238`): the header has to decrypt with the
repository key, every blob has to decrypt and decompress to the length its header
gives, and every blob's plaintext has to hash to the id the header names — the
same three checks rustic's own `check --read-data` makes for a pack an index file
names, and the third is what a later backup's dedup entry rests on. The pack's
bytes must also hash to the id it is stored under, which is how rustic names a
pack and what its own check recomputes: that one catches damage that was left
alone, and the per-blob check is what catches damage that was **renamed to the
hash of its new bytes** — a name is a claim, not a verification.

**The adopting pass is given the set this open verified.** `to_indexed_checked()`
lists the packs itself, and rustic offers no way to hand it a set — this project
therefore checks the listing on both sides of that pass
(`crates/chat-stasher/src/orphans.rs:397`, `crates/chat-stasher/src/orphans.rs:408`):
the packs *no index file names* have to be the same set before and after, and any
difference refuses the adoption. A pack that appeared while the open ran is
therefore never adopted — the cost of that refusal is a re-upload, which is the
safe direction — and a pack that vanished cannot leave the index pointing at
bytes that are gone. rustic's own machinery is not reachable for this: the
function that installs a caller-built index (`into_indexed_with_index`) and the
types it takes (`GlobalIndex`, `Indexer`) are `pub(crate)` in the pinned version.

So the bytes stop being waste because they **become the content**. The measured
effect at test scale (4 sessions × 500 000 bytes): the old retry added 1 047 572 B
over a stranded 1 045 700 B repository — a second full copy — while the adopting
retry adds only its snapshot, index and tree pack
(`crates/chat-stasher/tests/w245_interrupted_push_test.rs:610`).

Implementation: `crates/chat-stasher/src/orphans.rs:333`. Every read path reaches
it through `crates/chat-stasher/src/store.rs:366`, which is the whole point of
routing reads through it — after an adopting push, the snapshot's tree and its
shards exist **only** in packs no index file names, so a read that reads only
index files fails with "cannot ls tree" and exit 3.

The dedup entry point is rustic's file archiver, whose test is a plain
`index.has_data` on the blob id — it never consults a parent snapshot, which is
why an in-memory index entry is enough to stop the upload without one.

## The safety argument

**A concurrent client's pack is never touched.** Adoption only ever *adds* an
in-memory index entry; it never writes, moves or deletes a pack. The worst a
second machine's in-flight push can suffer is that we index a pack it is about to
index too, which is idempotent: two clients indexing one pack write the same blob
ids at the same offsets, which is what rustic's own `repair index` does, and why
its `check` labels an unreferenced pack "can be a parallel backup job".

**A pack still being written is never adopted.** The guard is *completeness*, not
age: a pack is adopted only if its header parses, its own length and blob sum
agree with the file size, its bytes read back as the id it is stored under, and
every blob it declares decrypts and hashes to the id the header gives — which a
pack still being streamed to the backend fails on the length counts, and a
damaged one on the last two. This is deliberately not an age gate. An
age gate would have to outlast the slowest concurrent upload (minutes for a large
pack) and would then refuse to reuse our own just-killed push's packs, which is
the entire point of the change. Completeness is also the stronger guard: it is a
property of the bytes, not a guess about who is writing them.
`crates/chat-stasher/src/orphans.rs:305` is where every failure to validate sends
the open back to the plain index — the same fallback whether the refuser was a
pack this open could not verify or a pack set that changed under it.

**A refusal costs the reuse, not the archive — and the cost is the whole set.**
One pack that cannot be verified refuses the adoption, so no pack on that open is
adopted, including the ones that verified. That granularity is deliberate and is
the behavior the tests already pinned (`an_unreadable_stranded_pack_…` asserts
that no pack is reported as adopted when one is unreadable); excluding only the
bad pack would mean handing the checked pass a filtered listing, and which blobs
are reachable would then depend on which packs happened to verify. The cost is
real and is worth naming: a damaged pack is permanent on this path, because
nothing here deletes or rewrites it and `append_only:true` blocks the command
that would — so the reuse those packs offered is gone until a deliberately
planned repair (`repair index`, single-writer, with `append_only` off, ADR-016)
re-indexes them. What is never at stake is the bytes: the content is uploaded
again and the archive is complete either way.

**Adoption is fenced by a survey, so a healthy repository is untouched.**
`crates/chat-stasher/src/orphans.rs:191` diffs the backend's pack listing against
the packs the index files name, and
`crates/chat-stasher/src/orphans.rs:359` refuses to adopt when the index names a
pack the backend no longer has. That case matters: the checked pass rebuilds the
index from what the backend *lists*, so it would drop that entry — and an entry
for a missing pack is one the local metadata cache can still serve, a documented
property that `tree_packs_are_only_really_gone_once_the_metadata_cache_is_gone` in
`crates/chat-stasher/tests/search_cache_windows_shape_test.rs` exists to pin. A
repository whose index and backend agree gets exactly the index it got before this
module existed.

**Nothing is deleted, ever.** `append_only:true` is left on, `repair index` and
`prune` are not called, and no code path here removes a pack. The two failure
modes are the safe ones: an open either adopts (and reuses) or falls back to the
plain index (and re-uploads, which is what the tool did before this change).

## What it costs, and what the operator sees

* The stranded packs stay unindexed **on disk** for good: adoption is per-open,
  so every later open re-derives it, and `verify --level l1` keeps reporting them
  as `pack {id} not referenced in index` warnings (rustic's own wording, which
  already says "can be a parallel backup job"). The alternative is `repair index`,
  which needs `append_only` off — a deliberate single-writer action, not a
  scheduled one.
* Every open pays one survey: a pack listing plus one pass over the index files.
  With rustic's local metadata cache on (the default) that pass is served
  locally.
* When the survey finds packs no index file names, that open also reads all of
  them — every byte, once, to decrypt each blob — before it may adopt anything,
  and lists the index files once more on each side of the adopting pass. A
  healthy repository pays nothing: a repository whose index names every pack in
  its backend skips the verification entirely, because no pack is a candidate.
  An archive that keeps stranded packs pays this on every open, on the read paths
  as well as on `push`, which is the price of never reusing a pack whose bytes
  were not read.
* A push whose adoption is refused because the pack set changed while it ran says
  so in the same place and the same shape: `NOT adopted (the packs no index file
  names changed while this open ran)`. It is the same state for the operator —
  the reuse was not taken, the content went up again — and it is worth its own
  wording because the cause is not the packs' bytes.
* `push` reports what it found and did, and a `dest-init` push appends
  `stranded_packs=` and `stranded=` to its one-line summary only when there is
  something to report; a healthy run prints the line it always printed. Silence
  means the survey found nothing — a measurement, not a gap. A refusal is
  reported as `NOT adopted` with its reason, because "we could not reuse this"
  and "there was nothing to reuse" are different states.
* Read paths print nothing about it. They do not need to: when a snapshot needs a
  pack it cannot reach, the existing machinery already answers PARTIAL / exit 3
  with "cannot ls tree" — the same answer it gave before this change, and one
  that never presents an absence as proof. A refusal reached from a read path is
  the same answer for the same reason, and it is the honest one: those packs did
  not read back as the bytes they are named for, so nothing in them is presented
  as content.

## What pins it

`crates/chat-stasher/tests/w245_interrupted_push_test.rs` carries the tests. Five
fail against the code before this feature: `a_push_over_stranded_packs_…`,
`content_from_an_adopted_pack_…`, `an_unreadable_stranded_pack_…`,
`an_incomplete_pack_in_a_remote_backends_listing_…`, and the `#[ignore]`d
real-window race.

Two more fail against a version that reads the packs but not their blobs, which
is what this feature looked like before the per-blob check:

* `a_pack_with_damaged_blob_bytes_…` (whole blob region damaged, name left alone)
  — caught by the pack's own hash.
* `a_damaged_pack_renamed_to_its_new_hash_…` — one byte flipped and the pack
  renamed to the hash of its new bytes, which the pack hash cannot see at all and
  which only decrypting the blobs catches. Measured against that version: it
  adopts, then reports `adopted`, and the read comes back with none of the staged
  sessions.

One more pins the adopting pass's set:
`a_pack_that_appears_while_the_open_runs_is_not_adopted` injects a pack file into
the window between the survey and that pass and requires `NOT adopted`, a
re-upload, and the injected file untouched.

The rest are safety properties that hold both before and after
(`an_interrupted_push_never_leaves_a_corrupt_archive`, the kill-before-any-pack
case, the two index-appears cases, and the Windows separator regression), and are
kept so that a fix for the cost can never be mistaken for a licence to weaken
them.

**The backend family a destination actually uses.** The tests above run on
`rustic_backend`'s `LocalBackend`, which writes a pack to `data/<xx>/<id>-tmp-`
and renames it into place, so its listings never show a pack that is not
finished. The OpenDAL services — `sftp`, which this project ships, and S3 — write
to the final path, so there a listing *can* show one. Four tests run the same
scenarios on `opendal:fs`, which shares that write path: one strands a pack and
truncates it (the push must fall back, report, and leave the in-flight file
alone); one injects a pack file into the survey-to-adoption window; and two put
an index file beside stranded packs — one present for the whole open, one
injected into that same window — where nothing may be adopted a second time and
no content may be uploaded again.

**The window is a fixture, not a race.** `CHAT_STASHER_TEST_HOLD_OPEN_AFTER_SURVEY`
parks an open between those two listings (`crates/chat-stasher/src/orphans.rs:259`),
which is how the injected-index and injected-pack tests put something there at
all: without it, a test can only inject on a timer and assert the properties that
hold whichever phase saw the file. The variable is read on every open and does
nothing when unset — no directory, no file, no wait — so the hold costs a real
run one environment lookup.

What is *not* pinned: real `sftp` and S3 remain untested here — they need a
server and credentials the gate does not have — so what is verified for them is
only that the code path is the same one `opendal:fs` takes; S3's multipart write
and SFTP's own listing semantics are argued from the shared implementation, not
measured. One state is also out of reach of the set fence by construction: a pack
that appears *and* is deleted again inside the open is in neither listing, and
nothing in an append-only repository deletes a pack, so that needs an out-of-band
deletion of the backend's data.
