# Node metadata: what a push stores, and what it refuses to

Every tree a push writes carries, for each file and directory, a *node*: the
name, the type, and the metadata `rustic_core` reads off the local filesystem.
Some of those fields are not properties of the content. Which of them a node
stores decides whether a push of an untouched stage is a genuine no-op.

## The defect

A push reuses a tree it already holds by serializing the new tree and comparing
its id with the parent snapshot's: a tree whose id the index already has is not
written at all (`vendor/rustic_core/src/archiver/tree_archiver.rs:195-197`).
Serialization covers every stored field, so **a stored field that moves on its
own re-serializes that tree — and each tree above it, whose child reference
changed — on every push**, with no content change and nothing in the output that
separates those bytes from real work.

Measured on Windows: a second push of an untouched stage reported
`files_unmodified=13 data_blobs=0 data_added=1690`. Every file unmodified, no
content blob, one tree uploaded again.

Only one stored field can do that, and the elimination is mechanical. A tree's
bytes change only if a node in it changed, and every field a node stores is
either one the comparison reads — a file whose compared fields moved is counted
`files_changed`, and the Windows run counted every staged file unmodified — or
one it never reads. Nothing writes into the stage between the two pushes, so the
second group is where the difference has to be. The comparison is what
`ignore_ctime`/`ignore_inode` speak to
(`vendor/rustic_core/src/archiver/parent.rs:172-190`). Of the fields it never
reads, all but one are pinned to values the push controls:

| stored field | why it cannot differ on an unchanged stage |
|---|---|
| name, type | from a walk sorted by path |
| mtime | read by the comparison, so equal by construction |
| size | read by the comparison on files, and 0 on directories |
| atime | overwritten with the mtime |
| mode, uid, gid, user, group, inode, device_id, links, extended attributes | absent or 0 on Windows and skipped by serialization; stable per file on Unix |
| content | copied from the parent node when the comparison matches |
| subtree | the id of the child tree, which is unchanged unless one of the above moved |

What is left is `ctime`, which on Windows is the file's **creation** time
(`vendor/rustic_core/src/backend/ignore/mapper.rs:249-253`) and on Unix the
inode change time. On Unix that value does not move unless metadata is written,
which is why the same push is a no-op on macOS and Linux.

**What is not established here, and should not be read into the above:** which
of the ways Windows can report a file's metadata differently on a later walk
actually applied, or why. The measurement is the counters and the tree bytes;
the argument is that a stored, never-compared field is the only place the
difference can live, and that a platform which reports one differently moves the
tree with nothing else changed. The test below shows that moving that one field
and nothing else is enough to produce the same signature, and it is the Windows
cell that exercises the platform rather than the model.

## The policy

Both halves are stated together in `crates/chat-stasher/src/store.rs:455-465`:

- **Comparison**: `ignore_ctime(true)` and `ignore_inode(true)`, so a file
  counts as changed on type, size and mtime alone.
- **Storage**: `set_atime(TimeOption::Mtime)` and `set_ctime(TimeOption::No)`.

`set_ctime(TimeOption::No)` is the fix: the field is not written, so it cannot
move a tree. Nothing this project reads is lost — the comparison already ignores
ctime, and no code in this crate reads one back out of a node.

`TimeOption::Mtime` would also have removed the churn, by pinning ctime to the
mtime the comparison validates; the tree would be stable either way. It was not
chosen because it makes the archive assert something false: a reader would see
ctime equal to mtime and take it to mean the file's metadata has not changed
since it was written, which that value cannot support. An absent field is
honest, and this archive is append-only, so a fabricated value written once
stays. `TimeOption::No` is also the reading that matches the exit-code rule the
project holds itself to elsewhere: "we did not record this" must not be spelled
the same way as a value.

`set_atime(TimeOption::Mtime)` names a policy the library applies by default
today, and writing it out changes nothing about the tree. It is here because
this defect is the shape of an option left at its default: a stored time whose
value comes from the platform rather than from the mtime the comparison reads.
Relying on that default means a future library change to it would silently
reintroduce per-push tree churn; stating it means the same change shows up as a
diff here.

`no_scan` was considered and is unrelated: it disables the pre-scan used for ETA
estimation and writes nothing different into a node. `set_devid` and
`set_xattrs` are the other knobs of the same option struct; they control the
fields the next section leaves alone.

## What this does not change

- **Nothing already archived is touched.** The policy applies to nodes written
  from now on; snapshots that stored a ctime keep it, and no path rewrites them.
- **The first push after this change re-serializes each tree once**, because the
  nodes it writes no longer carry a field the parent snapshot's nodes do. That
  is one push's worth of tree bytes per machine, and after it the stage is
  stable again.
- **Change detection is unchanged.** The comparison never read ctime here, so a
  file whose content or mtime changed is still detected exactly as before, and
  one that did not is still `files_unmodified`.
- **The fields left alone are the same class of hazard** — stored, and not read
  by the comparison. They are stable for a stage this project writes itself (a
  collector writes the shards; nothing else touches them), and each is a real
  filesystem property whose change is worth recording rather than dropping. If
  one is ever observed moving on an unchanged stage, it needs one of the two
  treatments: compare it, or stop storing it.

## The check

`crates/chat-stasher/tests/w242_interrupted_push_test.rs:474` reproduces the
Windows event on a host that does not have to be Windows. It moves ctime and
nothing else — `chmod` to the mode a file already has, which POSIX requires to
update the inode change time while leaving the mtime, the mode and every byte
alone — and then requires the second push to add zero bytes. Before the storage
policy above, that test fails here with `data_blobs=0` and a non-zero
`data_added`: the Windows report, locally.

The same file's `a_completed_push_makes_the_next_push_a_no_op` asserts the plain
case — `data_added == 0` for a genuinely untouched stage — on every platform.
The Windows CI cell is where the platform itself is exercised, so it is the one
that can prove the ctime reading rather than its model.
