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

So the bytes stop being waste because they **become the content**. The measured
effect at test scale (4 sessions × 500 000 bytes): the old retry added 1 047 572 B
over a stranded 1 045 700 B repository — a second full copy — while the adopting
retry adds only its snapshot, index and tree pack
(`crates/chat-stasher/tests/w245_interrupted_push_test.rs:450`).

Implementation: `crates/chat-stasher/src/orphans.rs:223`. Every read path reaches
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
age: a pack is adopted only if its header parses and its own length and blob sum
agree with the file size — which a pack still being streamed to the backend
fails. This is deliberately not an age gate. An age gate would have to outlast
the slowest concurrent upload (minutes for a large pack) and would then refuse to
reuse our own just-killed push's packs, which is the entire point of the change.
Completeness is also the stronger guard: it is a property of the bytes, not a
guess about who is writing them. `crates/chat-stasher/src/orphans.rs:265` is where
a failure to validate sends the open back to the plain index.

**Adoption is fenced by a survey, so a healthy repository is untouched.**
`crates/chat-stasher/src/orphans.rs:160` diffs the backend's pack listing against
the packs the index files name, and
`crates/chat-stasher/src/orphans.rs:250` refuses to adopt when the index names a
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
* `push` reports what it found and did, and `dest-init`'s compact line carries
  `stranded_packs=` and `stranded=`. Silence means the survey found nothing —
  a measurement, not a gap. A refusal is reported as `NOT adopted` with its
  reason, because "we could not reuse this" and "there was nothing to reuse" are
  different states.
* Read paths print nothing about it. They do not need to: when a snapshot needs a
  pack it cannot reach, the existing machinery already answers PARTIAL / exit 3
  with "cannot ls tree" — the same answer it gave before this change, and one
  that never presents an absence as proof.

## What pins it

`crates/chat-stasher/tests/w245_interrupted_push_test.rs` carries the tests. Four
of them fail against the unfixed code (`a_push_over_stranded_packs_…`,
`content_from_an_adopted_pack_…`, `an_unreadable_stranded_pack_…`, and the
`#[ignore]`d real-window race); the other two are safety properties that hold both
before and after (`an_interrupted_push_never_leaves_a_corrupt_archive` and the
kill-before-any-pack case), and are kept so that a fix for the cost can never be
mistaken for a licence to weaken them.
