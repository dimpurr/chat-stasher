# Node metadata: what a push stores, and what it refuses to

Every tree a push writes carries, for each file and directory, a *node*: the
name, the type, and the metadata `rustic_core` reads off the local filesystem.
Some of those fields are not properties of the content. Which of them a node
stores decides whether a push of an untouched stage is a genuine no-op.

## The defect

A push reuses a tree it already holds by serializing the new tree and comparing
its id with the parent snapshot's: a matching tree is not rewritten, and a tree
whose id the index already has is not written again
(`vendor/rustic_core/src/archiver/tree_archiver.rs:163-197`).
Serialization covers every stored field, so **a stored field that moves on its
own re-serializes that tree — and each tree above it, whose child reference
changed — on every push**, with no content change and nothing in the output that
separates those bytes from real work.

Measured on Windows, twice, on two pushes of one untouched stage:

| push | stored nodes carry | reported |
|---|---|---|
| before either field was dropped | a `ctime`, and directory times | `files_unmodified=13 data_blobs=0 data_added=1690` |
| after dropping the stored `ctime` alone | directory times | `files_unmodified=13 data_blobs=0 data_added=1602` |
| after dropping directory times too | neither | the two tests below require `data_added=0` here, on every platform |

Every staged file came back byte-identical and unre-written, no content blob was
written, and one tree was uploaded again. The middle row is what says the first
row's field was not the cause: removing a stored field from every node of the
rewritten tree took 88 bytes off it and left its churn in place. What it did
identify is the tree — the same 1602 bytes were added by a stage holding a
quarter of the shards, so the tree is one that does not depend on the shards at
all.

Only one stored field can do that, and the elimination is mechanical. A tree's
bytes change only if a node in it changed; every field a node stores is either
one the comparison reads or one it never reads. **For a file, the compared ones
are settled by the counters**: a file whose type, size or mtime moved is counted
`files_changed`, and the Windows run counted every staged file unmodified and
none changed, so no file node differed in any field the mapper pins. **A
directory is where that reasoning stops.** The comparison reads a directory's
mtime too, but nothing reports the result — the summary prints the file counters
and not `dirs_changed` — and a directory's mtime is not a property of the
directory's content in the first place: it is the time of the writes *into* the
directory, and Windows reports it lazily, so a directory written to shortly
before a walk can hand one value to that walk and another to the next one.

| stored field | why it cannot differ on an unchanged stage |
|---|---|
| name, type | from a walk sorted by path |
| mtime, size of a **file** | read by the comparison, so equal by construction, and `files_changed=0` says so |
| mtime, atime of a **directory** | not settled by anything: the time of the writes into the directory, reported lazily by the platform; `dirs_changed` is not printed, so a moved one is invisible |
| atime of a file | overwritten with the file's mtime |
| mode, uid, gid, user, group, inode, device_id, links, extended attributes | absent or 0 on Windows and skipped by serialization; stable per file on Unix |
| content | copied from the parent node when the comparison matches |
| subtree | the id of the child tree, which is unchanged unless one of the above moved |

A stored `ctime` was in the first row's column too: it is never compared, and on
Windows it is the file's *creation* time. Dropping it was right for its own
reason and wrong as a diagnosis — it is a field of the same class, not the one
that moved.

**What is not established here, and should not be read into the above:** the
mechanism by which Windows reports a moved directory time, and which of its
directories moves. The measurement is the counters and the tree bytes; the
argument is that a stored, never-compared field is the only place the difference
can live, that a directory node's times are the only such field a file counter
cannot clear, and that moving them reproduces the signature exactly.

## The event, reproduced

`crates/chat-stasher/tests/w242_interrupted_push_test.rs` runs the Windows
event on a host that does not have to be Windows:

- `a_metadata_field_the_archive_does_not_use_cannot_re_serialize_a_tree` moves
  `ctime` and nothing else: `chmod` to the mode a file already has, which POSIX
  requires to update the inode change time while leaving the mtime, the mode and
  every byte alone. It failed on macOS before `set_ctime(TimeOption::No)` with
  `files_unmodified=5 data_blobs=0 data_added=8973`.
- `a_directory_mtime_that_moves_cannot_re_serialize_a_tree` moves a directory's
  mtime and nothing else: a file created in the directory and removed again
  leaves the directory's children exactly as they were. It failed on macOS
  before the directory-time policy below with `files_unmodified=5
  data_blobs=0 data_added=6149`, and its failure report named `mtime` and
  `atime` on directory nodes — with the `subtree` ids of every tree above them
  moving with them.

The Windows cell is still where the platform itself is exercised. When a no-op
push adds tree bytes there, `assert_no_tree_bytes` prints the field-by-field
node diff of the two snapshots, which names the field on a host nobody can run
by hand.

## The policy

All of it is stated in one place,
`crates/chat-stasher/src/store.rs:571-583`:

- **Comparison**: `ignore_ctime(true)` and `ignore_inode(true)`, so a file
  counts as changed on type, size and mtime alone.
- **Storage**: `set_atime(TimeOption::Mtime)`, `set_ctime(TimeOption::No)` and
  `set_dir_times(TimeOption::No)`.

`set_ctime(TimeOption::No)`: the field is not written, so it cannot move a tree.
The push comparison ignores ctime, and the local restore path applies stored
mtime and atime but not ctime (`vendor/rustic_core/src/archiver/parent.rs:172-188`,
`vendor/rustic_core/src/commands/restore.rs:391-404`,
`vendor/rustic_core/src/backend/local_destination.rs:290-304`). The diagnostic
node diff does read archived nodes and serializes their fields for comparison,
so it can still report a ctime present in an older snapshot
(`crates/chat-stasher/src/store.rs:722-739`).

`set_dir_times(TimeOption::No)`: a directory node stores no times at all, for
the reason in the table above. What makes a directory part of the archive is the
tree it holds, and a directory is compared through that tree's id, so dropping
its times drops nothing the comparison uses. **A file's mtime is unaffected**:
this option is about directory nodes only, so content change detection — which
is type, size and mtime, and the reason a shard is never re-read — is exactly
what it was.

`TimeOption::Mtime` would also have removed both churns, by pinning the dropped
field to the mtime the comparison validates; the tree would be stable either
way. It was not chosen because it makes the archive assert something false: a
reader would see ctime equal to mtime and take it to mean the file's metadata
has not changed since it was written, which that value cannot support. For a
directory it is worse — the mtime it would pin to is the same value that moved.
An absent field is honest, and this archive is append-only, so a fabricated
value written once stays. `TimeOption::No` is also the reading that matches the
exit-code rule the project holds itself to elsewhere: "we did not record this"
must not be spelled the same way as a value.

`set_atime(TimeOption::Mtime)` names a policy the library applies by default
today, and writing it out changes nothing about the tree. It is here because
this defect is the shape of an option left at its default: a stored time whose
value comes from the platform rather than from the mtime the comparison reads.
Relying on that default means a future library change to it would silently
reintroduce per-push tree churn; stating it means the same change shows up as a
diff here.

`set_dir_times` is a field this project added to its vendored `rustic_core`
(`vendor/rustic_core/src/backend/ignore/mapper.rs:68-86`). Its default is the
library's own behaviour — every node keeps the times it always had — so a
caller that does not set it sees no change; the push opts in. The vendor copy is
this project's, patched where the library's options cannot express a policy the
archive needs (as `to_indexed_with_packs` is, for orphan pack adoption).

`no_scan` was considered and is unrelated: it disables the pre-scan used for ETA
estimation and writes nothing different into a node. `set_devid` and
`set_xattrs` are the other knobs of the same option struct; they control the
fields the table above leaves alone.

## What this does not change

- **Nothing already archived is touched.** The policy applies to nodes written
  from now on; snapshots that stored a ctime or a directory time keep it, and no
  path rewrites them.
- **The first push after this change re-serializes each tree once**, because the
  nodes it writes no longer carry fields the parent snapshot's nodes do. That is
  one push's worth of tree bytes per machine, and after it the stage is stable
  again.
- **Change detection is unchanged.** The comparison never read ctime, and a
  directory is compared through its tree, so a file whose content or mtime
  changed is still detected exactly as before, and one that did not is still
  `files_unmodified`.
- **The fields left alone are the same class of hazard** — stored, and not read
  by the comparison for content. They are stable for a stage this project writes
  itself (a collector writes the shards; nothing else touches them), and each is
  a real filesystem property whose change is worth recording rather than
  dropping. If one is ever observed moving on an unchanged stage, it needs one
  of the two treatments: compare it, or stop storing it.

## The check

The plain case is asserted in
`crates/chat-stasher/tests/w242_interrupted_push_test.rs:329-340,520-540`: a
second push of an untouched stage adds no bytes at all, `data_added == 0` and
not merely `data_blobs == 0`, on every platform. The two tests in
"The event, reproduced" above are the single-field reproductions, and the
failure report they print is the instrument for a platform that cannot be run
locally: `BackupStore::node_metadata_diff` decodes both snapshots through
`rustic_core` and prints every node and every field that differs between them,
including the `subtree` entry whose change is what carried the churn upwards.
