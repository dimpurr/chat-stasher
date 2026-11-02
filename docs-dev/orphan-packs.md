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

On an open with unindexed packs, this project verifies each candidate header and
all its blobs, then constructs rustic `IndexPack` entries from the headers it
just verified. It also captures entries from the existing index files in the
same survey. It passes both sets to the pinned rustic API
`Repository::to_indexed_with_packs()`, which lists neither packs nor index files.
A later `backup` then finds the verified blobs already present and uploads none
of them.

**A header is not a pack, so a header read is not a verification.** This project
reads the header of every pack the index does not name *and every blob that
header declares* (`crates/chat-stasher/src/packcheck.rs:136`, called from
`crates/chat-stasher/src/orphans.rs:393`): the header has to decrypt with the
repository key, every blob has to decrypt and decompress to the length its
header gives, and every blob's plaintext has to hash to the id the header names.
The pack's bytes must also hash to the id it is stored under. That catches damage
left under the old name, while the per-blob check catches damage renamed to the
hash of its new bytes. A name is a claim, not a verification.

**Only the set this open verified is handed to rustic.** The backend listing is
compared before and after verification so an index file that arrives during the
survey window can be noticed. Then the returned `IndexPack`s are passed directly
to `to_indexed_with_packs()`; there is no later pack listing in the adoption
path. A pack that appears after the final comparison cannot enter the in-memory
index. The regression test
`a_pack_that_appears_at_the_former_relisting_point_is_not_adopted` injects valid
packs containing different staged content at that point and checks that the
retry uploads those content blobs rather than treating them as already stored.

The application needs one small addition to the pinned rustic API because
rustic 0.12.0 keeps the method that attaches a caller-built index private. The
local patch is in `vendor/rustic_core/src/repository.rs`; it accepts verified
`IndexPack`s and combines them with entries from existing index files without
listing packs.

So the bytes stop being waste because they **become the content**. The measured
effect at test scale (4 sessions × 500 000 bytes): the old retry added 1 047 572 B
over a stranded 1 045 700 B repository — a second full copy — while the adopting
retry adds only its snapshot, index and tree pack
(`crates/chat-stasher/tests/w245_interrupted_push_test.rs:639-650`).

Implementation: `crates/chat-stasher/src/orphans.rs:716`. Every read path reaches
it through `crates/chat-stasher/src/store.rs:631`, which is the whole point of
routing reads through it — after an adopting push, the snapshot's tree and its
shards exist **only** in packs no index file names, so a read that reads only
index files fails with "cannot ls tree" and exit 3.

The dedup entry point is rustic's file archiver, whose test is a plain
`index.has_data` on the blob id — it never consults a parent snapshot, which is
why an in-memory index entry is enough to stop the upload without one.

## W1066: the adoption survives the push that made it

Adoption as described above is per-open: it re-reads every stranded pack on
every later open. A killed push that leaves one complete stranded pack makes
every subsequent `push`, `read`, `mirror` and `doctor` pay for the re-read,
forever — measured live in the k100 fixture, where a complete stranded pack was
re-verified 38 times in a row and nothing on disk ever recorded the adoption.

The persist step closes that, on the push connection only: after a push wrote
its own index file and before it writes its snapshot (W1077, below), it names the packs its open verified in **one
new index file** (`crates/chat-stasher/src/orphans.rs:persist_verified`, written
through the pinned-rustic `Repository::append_index_packs`). From then on those
packs are ordinary indexed content and the survey that fences every open finds
nothing to adopt.

Why this stays inside the invariants that kept `repair index` out (ADR-001
`append_only:true`, ADR-016 "we never prune"):

* **append-only.** The write adds one file and touches no existing index file;
  `repair index` is out of reach because it *rewrites* (removes) them. Verify's
  usual "pack not referenced in index" warnings disappear for these packs
  because they are now genuinely referenced.
* **push-connection only.** The persist runs in `push_with`, beside the tmp
  debris sweep — the two places a push connection may already write. Read
  paths, `mirror` and `prune-orphans` still write nothing.
* **idempotent by re-survey, not by lock.** The survey runs once more
  immediately before the write, and any pack that some index file already names
  — this push's own indexer, or another machine that persisted the same
  adoption — is skipped. Two clients that survey before either writes still
  both write; that is the interleaving no lock can prevent, so it is tested
  directly (`two_pushes_that_both_persist_the_same_packs_stay_mergeable`): the
  two files' entries are identical (the same ids at the same offsets of the
  same pack), and whichever a reader's lookup hits resolves the same bytes.
* **a write failure fails the push before its snapshot (W1077).** See the next
  section: without the naming file a snapshot that reuses the packs must not be
  published at all. When no snapshot is going to be written (the tree is
  identical to the parent's) nothing depends on the packs, and a failed write
  is only reported as `NOT persisted (…)`, the pre-W1066 status quo.

The one re-opened ruling travels with the same change, because the production
event that motivated all of this was not cost but collateral: an old torn file
under a final name (a pre-ADR-063 half-written pack) refused every later
adoption — one unreadable header vetoing every complete pack, indefinitely. A
torn pack's bytes can never become content — no header means no verified blob
entries, and `crate::debris` already classifies exactly this shape — so
excluding it from the refusal adds no reliance on a partial verification. The
open adopts and the push names the packs that verify; the debris is reported
with the one interrupted-push sentence and left untouched, because deleting it
remains the deliberately planned one-off action ADR-016 reserves (it is not a
code path here, and `prune-orphans` still refuses to delete anything).

## W1077: the naming index file is published before the snapshot

W1066 wrote the naming index file after the snapshot. A snapshot that reuses a
stranded pack reaches trees and blobs only that pack holds, so a SIGKILL between
the two publishes left a snapshot whose trees no index file names; a plain index
read then failed with `Tree ID ... not found in index` until some later push
happened to name the pack. The kill test that showed it ran against the real
`push` path.

The order is now fixed: the push's own index file (written by rustic when the
backup finalizes), then the naming index file, then the snapshot. The pinned
rustic's `Repository::backup_before_snapshot` calls a gate after the backup's
data, trees and index are written and strictly before the snapshot file;
`push_with` runs `persist_verified` in that gate.

* **A failed persist fails the push, with no snapshot.** The alternative, drop
  the adoption and re-upload, is not available: by the time the snapshot is
  about to be written the backup has already deduplicated against the adopted
  packs, so redoing it would mean a second full backup against a plain index.
  Publishing anyway is the bug. So the push errors out (`no snapshot was
  written: ...`), the stranded packs stay as they were, and the next push
  verifies them again and retries. A persistent write failure would equally
  break the backup's own index write on the same backend, so failing here costs
  nothing the backend was not already costing.
* **Success has two shapes.** `Named` (this push wrote the file) and
  `AlreadyNamed` (another client's index file already names every verified pack
  at the pre-write survey) both cover the dependency. A verified pack that is
  neither listed nor named does not: a snapshot cannot depend on it.
* **A kill between the naming file and the snapshot is harmless.** The leftover
  is one extra index file naming packs this push verified, byte-for-byte what a
  later push would have written; no snapshot refers to anything new, and the
  next push finds the packs named and writes only its snapshot.
* **Torn debris is still never adopted or named.** The gate only names what the
  open verified; the W1066 exclusion is unchanged.

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
`crates/chat-stasher/src/orphans.rs:842-844` is where every non-torn failure to
validate sends the open back to the plain index, and
`crates/chat-stasher/src/orphans.rs:854-865` where a pack set that changed under
the open does — the same fallback for both refusers, because neither is a state
the index should be rebuilt from.

**A refusal costs the reuse, not the archive — and over content, the cost is
still the whole set.** Among packs that read as packs, the granularity is
unchanged: one pack that does not verify refuses the adoption, so no pack on
that open is adopted, including the ones that verified. Excluding only the bad
pack would make the dedup index depend on a partial verification result. The
cost is real and is worth naming: a damaged pack is permanent on this path, because
nothing here deletes or rewrites it and `append_only:true` blocks the command
that would — so the reuse those packs offered is gone until a deliberately
planned repair (`repair index`, single-writer, with `append_only` off, ADR-016)
re-indexes them. What is never at stake is the bytes: the content is uploaded
again and the archive is complete either way.

**Torn debris does not get a vote (W1066).** The one exclusion from the whole-set
refusal is the candidate that cannot be read as a pack at all: its length field
declares a header the file cannot hold, or its header will not decrypt — the
shape an interrupted push leaves and the shape `crate::debris` classifies as
torn. Its bytes can never become content, so excluding it imports no partial
verification: the adoption set is exactly the packs that fully verified.
Before W1066, one such file under a final name refused every later adoption
indefinitely — the production case was a five-week-old torn pack vetoing a
complete 15 MiB pack beside it, whose content was then re-uploaded in full. The
debris itself is left exactly as found: never adopted, never named by any
index file, reported with the one interrupted-push sentence, and removed only
by the deliberately planned one-off action ADR-016 reserves.

**Adoption is fenced by a survey, so a healthy repository is untouched.**
`crates/chat-stasher/src/orphans.rs:328-366` diffs the backend's pack listing against
the packs the index files name, and
`crates/chat-stasher/src/orphans.rs:821-835` refuses to adopt when the index names a
pack the backend no longer has. That case matters: adopting from existing index
files plus only newly verified packs would otherwise drop that entry — and an
entry for a missing pack is one the local metadata cache can still serve, a documented
property that `tree_packs_are_only_really_gone_once_the_metadata_cache_is_gone`
in
`crates/chat-stasher/tests/search_cache_windows_shape_test.rs` exists to pin. A
repository whose index and backend agree gets exactly the index it got before this
module existed.

**Nothing is deleted, ever.** `append_only:true` is left on, `repair index` and
`prune` are not called, and no code path here removes a pack. `prune-orphans`
reports the same set the survey finds and refuses to delete any of it, naming the
capabilities a safe delete would need. The two failure modes are the safe ones: an
open either adopts (and reuses) or falls back to the plain index (and re-uploads,
which is what the tool did before this change).

## What it costs, and what the operator sees

* The stranded packs stay unindexed **on disk** only until the next push
  survives its adoption: that push names them in one new index file (W1066),
  so every later open skips the survey-to-adoption work for them and `verify
  --level l1` stops reporting them as `pack {id} not referenced in index`
  (rustic's own wording, which already says "can be a parallel backup job").
  A repository with no push left — read-only use only — keeps paying per open,
  which is the pre-W1066 behaviour exactly.
* Every open pays one survey: a pack listing plus one pass over the index files.
  With rustic's local metadata cache on (the default) that pass is served
  locally.
* When the survey finds packs no index file names, that open verifies all of
  them — a verification reads every byte, once, to decrypt each blob — before
  it may adopt anything, and lists the index files once more on each side of
  the adopting pass. A healthy repository pays nothing: a repository whose
  index names every pack in its backend skips the verification entirely,
  because no pack is a candidate. Torn debris stops its own probe closer than
  that: the verifier refuses those bytes at the header (the length field or
  the decrypt check), so it never reads the rest of a half-written pack. An
  archive that never pushes keeps paying the full read on every open for its
  complete stranded packs — with a push, the one adoption plus naming closes
  the account permanently.
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
  and "there was nothing to reuse" are different states. Torn debris rides its
  own count with the one interrupted-push sentence. What the push *named* rides
  a third line — `persisted - a new index file names the N verified pack(s)` —
  or, when nothing was written, `NOT persisted (…)` with the reason, because
  "named for good" and "still un-named" are different states too.
* Read paths print nothing about it. They do not need to: when a snapshot needs a
  pack it cannot reach, the existing machinery already answers PARTIAL / exit 3
  with "cannot ls tree" — the same answer it gave before this change, and one
  that never presents an absence as proof. A refusal reached from a read path is
  the same answer for the same reason, and it is the honest one: those packs did
  not read back as the bytes they are named for, so nothing in them is presented
  as content.

## Seeing the stranded set: `prune-orphans`

Adoption reclaims the bytes; W1066 makes the reclaim visible too, because the
push that adopts also names the packs in a new index file. What stays invisible
is the debris: torn candidates are never named by any index file, so an operator
asking "how much is stranded here, and could any of it be deleted" gets the
whole story from `prune-orphans` and nothing else:

```sh
chat-stasher prune-orphans --destination <name> [--dry-run | --apply] [--json]
```

`--dry-run` and `--apply` cannot be combined; omitting both also runs the
read-only inventory. `--json` writes one JSON object to stdout, with the same
counts and byte totals plus each candidate's full id and status — torn
(interrupted-push debris, with the probe's reason), stranded (verified, with
the next open's verdict) or unknown (with the verifier's reason).

It opens the repository read-only — no index is built, and nothing is adopted —
lists the backend once, reads the index files, and then verifies each unindexed
pack's bytes with the same verifier the adopting open uses. Every count it reports
comes from that one listing, so its totals and its unindexed set describe the same
moment. It reports the repository's own config id as its fingerprint (never the
path or host it was reached at), the backend family, pack and byte totals, the
unindexed packs by id prefix and size, how many of those are torn debris and how
many are stranded content (each with their own byte totals), how many are
unknown with the verifier's reason, what the next push would do about them, and
any pack an index names that the backend does not list. It writes nothing, and it
appears in no schedule, no `push`, no `run-once` and no browser host.

**Torn debris is a known state, not an incomplete read (ISSUE-006 step A).**
An unindexed candidate whose header cannot even be read — too short to hold
one, a length field describing a header the file cannot hold, or a header that
will not decrypt — is the shape an interrupted push's half-written pack takes,
and it is classified by the one function every scanning reader shares
(`crates/chat-stasher/src/debris.rs`: `mirror sync`, `verify`, `doctor` and
this command all call it, and all of them print the one sentence
"An interrupted push left a partial file. It belongs to no backup and can be
ignored."). Torn candidates make the survey neither incomplete nor a failure:
the exit stays `0` when everything else read, because being debris is an
answer. What stays `unknown` is the pack whose header reads but whose bytes
do not verify, and the pack the backend would not serve — those keep the
whole-survey `3`. The earliest-seen time of any candidate is reported as
unknown: the backend interface carries no modification time, and no output
invents a clock.

**Deletion is refused, and the refusal names what is missing.** `--apply` exits `3`
and prints the three capabilities a safe delete would need:

* a repository-wide reader/writer lock every client honors for its whole push or
  read. There is none: `.ingest.lock` is a local *stage* lock, and a local `flock`
  would not constrain another machine or another backend writer.
* a conditional delete carrying an object version or ETag, so exactly the object
  that was checked is the object removed. `rustic_core::ReadBackend` exposes
  `list_with_size` — a listing and a size — and nothing to condition on.
* a trustworthy backend modification time to age candidates by. The same interface
  carries no modification time at all, and the packing machine's clock is not the
  backend's clock.

The age floor and the two-pass mark that a real sweep would use are also not
sufficient on their own: they cannot see a *currently active* retry that is about
to adopt a pack, which is why the lock is first on the list rather than a detail.
The policy boundary is the fourth reason and the oldest — `append_only:true` is
left on, and disabling it to call rustic's broad `prune` would let that command
rewrite indexes and repack *indexed* data, which is not what this command is for.

**An unreadable pack is unknown, and it makes the whole survey incomplete —
except when it is classified as debris.** A pack whose header reads but whose
bytes do not read back as the id it is stored under is reported `unknown`
with the verifier's reason, never as empty and never as zero, and the command
exits `3` — the same third state `CLAUDE.md` keeps for "did not finish
reading". A torn candidate is not that case (see above): it is a
classification, and a survey that classified every pack is complete. An index
that names a pack the backend does not list is a contradiction rather than an
unknown: if every pack was readable, the finished survey exits `1`; if any
pack is unknown too, incompleteness takes precedence and the exit is `3`. A
run that read every pack, found no unindexed candidate, and found no
contradiction exits `0`, and that zero is a measurement made after a complete
survey.

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

`crates/chat-stasher/tests/w1066_orphan_persist_test.rs` carries the W1066 pins.
Four fail against the code before that change — a push that names what it
adopted in one new index file, a rerun that names nothing new, torn debris that
does not refuse the complete packs around it, and the two-client double-write —
and one holds before and after on purpose (`a_damaged_complete_pack_still_
refuses_the_whole_adoption`), so that the debris exclusion can never quietly
become a relaxation of the check on packs that could be content. The two w245
torn fixtures (`an_unreadable_stranded_pack_…`,
`an_incomplete_pack_in_a_remote_backends_listing_…`) now assert the
debris-exclusion outcome directly: debris reported and untouched, survivors
adopted and named.

`crates/chat-stasher/tests/w250_prune_orphans_test.rs` carries the read-only
report's tests: an empty repository, a healthy one, stranded packs reported
stranded and adoptable, a torn candidate reported as debris (not unknown, not
a failure), a damaged orphan reported unknown with its reason and exit
`3`, `--apply` refused and refused *before* the destination is dialled (proved
by pointing it at a path that is not a repository), the `--json` document's
full ids, and a destination named in the config reaching the same repository
`--repo` does. Every mode also compares the repository tree — every file's
contents hashed, not just its name — before and after, because "the survey
writes nothing" is the claim the whole command rests on.

**The torn classification's fixtures**
(`crates/chat-stasher/tests/w1040_torn_debris_test.rs`, ISSUE-006 step A)
pin the behaviour every scanning reader owes the debris: with one
unreferenced torn pack in the repository, `mirror sync`, `status`, `doctor`,
`verify`, `read` and `export` all exit `0`, the torn pack never enters the
mirror (no copy, no torn-write temp), `status` counts exactly it as torn, no
user-facing string about the archive calls it corrupt, and `prune-orphans`
phrase for `prune-orphans`
carries torn and stranded as separate counts with byte totals and an honest
unknown earliest-seen time. Two controls pin the boundary: the same bad bytes
under a name an index file lists still fail `mirror sync` with exit `1`, and
a complete unindexed pack still counts as stranded, adoptable, never torn.
W1066 changed one sentence of that fixture set and it is the one the tests
follow: the adopting open no longer refuses the whole set over a torn
candidate — it classifies the debris (the mirror was already the reader that
skips it) and adopts the packs that verify around it, while a pack that reads
as a pack but cannot be verified is still not adopted, by anybody, ever.

**The backend family a destination actually uses.** Before ADR-063, the tests
above ran on `rustic_backend`'s `LocalBackend`, which writes a pack to
`data/<xx>/<id>-tmp-` and renames it into place, while the OpenDAL services —
`sftp`, which this project ships, and S3 — wrote to the final path, so there a
listing *could* show a torn pack. **After ADR-063 (ISSUE-006 step C), on the
SFTP and local-directory families no listing can show a torn pack under a final
name:** the wrapper in `Store::backends()` (`src/atomic_pack.rs`) writes every
pack to `data/<xx>/<id>.tmp-<unix seconds>-<16 hex>`, reads it back from the
backend and hashes it to the id, and only then renames it into place. S3/R2 keep
their single atomic object upload. The truncated-pack and injected-pack
scenarios on `opendal:fs` therefore change shape, not meaning: the tests now
plant their torn bytes under a final name by hand (the state a pre-C push left),
because the product no longer produces it. One strands a pack and truncates it
(the push must fall back, report, and leave the in-flight file alone); one
injects a pack file into the survey-to-adoption window; and two put an index
file beside stranded packs — one present for the whole open, one injected into
that same window — where nothing may be adopted a second time and no content may
be uploaded again. The real SFTP server is not exercised in the gate (the
kill-100 fixture drives the same publication code through an IO with SFTP v3
rename semantics); one real storage-box run is the release blocker.

Tmp debris (a publication cut off before its rename) is deleted by name after
seven days, on a push connection only — see ADR-063 D3. It is never part of the
published namespace, so deleting it needs none of the reachability arguments
above.

**The window is a fixture, not a race.** `CHAT_STASHER_TEST_HOLD_OPEN_AFTER_SURVEY`
parks an open between those two listings (`crates/chat-stasher/src/orphans.rs:450-479`),
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
