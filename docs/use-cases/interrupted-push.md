# An interrupted push: is my backup still fine?

<!-- RELEASE GATE: this page describes the torn-debris classification (ISSUE-006 step A), merged after 0.5.0-rc.2. Ship it with the release that carries it. -->

A push can stop at any moment: you closed the laptop's lid, the Wi-Fi changed,
you pressed Ctrl-C, the machine powered off, the storage provider dropped the
connection. Nothing about that is an accident of the tool or a rare event — it
is a normal way for a backup to end, and the next push simply runs again.

This page is for the moment after that: some output has mentioned **torn**
debris, or `mirror sync` printed a line about a partial file, and you want to
know whether the archive is still sound. Short answer first:

> **Yes.** Torn debris is a leftover half of the interrupted push itself. It
> belongs to no backup, no snapshot references it, and nothing needs to be
> done about it.

The sentence every command uses, word for word, is:

> An interrupted push left a partial file. It belongs to no backup and can be ignored.

## What an interrupted push leaves behind

A push writes its pack files first and its index file last. An interruption
in between can leave two kinds of leftovers in the repository, and they are
worth telling apart:

| What it left | What it is | What happens to it |
|---|---|---|
| **Stranded** pack — complete, but its index file was never written | Finished content with no index entry yet | The next command that opens the repository verifies it and adopts it, and your next push uploads none of its content again. Not a failure, not debris. |
| **Torn** pack — half a file, under the name the finished pack would have had | The interrupted write itself: no index file names it, and its own length field describes a header the file cannot hold | Classified as debris and skipped by every scanning command, forever harmless. No tool action is needed or exists to clean it in this build. |

Neither kind appears in a snapshot. A snapshot only ever references content
through the index, so half a pack and an unwritten index entry are both, by
construction, outside every backup you have made.

## What you see

The line shapes below are the ones this build's regression suite pins, with
synthetic counts.

**`mirror sync` completes and says what it skipped.** The debris is never
downloaded into your local copy; the round's exit code stays `0`:

```
[mirror] torn         : 1 file(s) / 1.9 MiB skipped as interrupted-push debris — An interrupted push left a partial file. It belongs to no backup and can be ignored.
```

**`status` counts it in the mirror rows.** The copy's own summary stays its
normal self; the debris line rides beside it:

```
[mirror] 1 torn file(s) (1.9 MiB) sit in the source repository and are never copied into the local copy of `offsite` — An interrupted push left a partial file. It belongs to no backup and can be ignored.
```

**`doctor` reports the counts under the destination it dialed** — torn and
stranded as separate counts and byte totals, and no reliable date to attach
to either (the backend interface carries no modification time, so the
earliest-seen time is stated as unknown rather than invented):

```
  offsite       … — reachable; repository present
               debris: 1 torn pack(s) (1.9 MiB) — An interrupted push left a partial file. It belongs to no backup and can be ignored.
               debris: earliest-seen time unknown — the backend interface carries no modification time, so the earliest-seen time is unknown
```

**`verify` prints a reminder, not a finding.** The verdict is the archive's:

```
[verify] L1 structure  ok=true findings=… errors=0 …
[verify] torn           1 torn pack(s) (1.9 MiB) — An interrupted push left a partial file. It belongs to no backup and can be ignored.
[verify] RESULT         : OK
```

**`read` and `export` are unaffected** — they read what snapshots reference,
and no snapshot references debris, so there is nothing to see.

**`prune-orphans --destination <name>` is the survey.** It reports the
unindexed set torn-first, with byte totals, what the next push would do about
the rest, and the verifier's own reason per pack:

```
[prune] unindexed   : 3 (5.7 MiB) — 1 torn (1.9 MiB), 1 stranded (1.9 MiB), 1 unknown (1.9 MiB)
[prune] torn        : 1 file(s) belong to no backup — An interrupted push left a partial file. It belongs to no backup and can be ignored.
[prune] first seen  : unknown — the backend interface carries no modification time, so the earliest-seen time is unknown
```

## What is normal

- **The debris stays.** Nothing in this build deletes it, remotely or locally:
  the repository is append-only, and a safe delete would need capabilities
  (a repository-wide lock, a conditional delete, a trustworthy modification
  time) that this build deliberately does not pretend to have. A future change
  to the pack write path is what stops new debris from forming
  ([docs-dev/orphan-packs.md](../../docs-dev/orphan-packs.md) records the
  argument).
- **The debris counts on every surface, every time.** It is not a one-time
  notice: every scan re-classifies it from its bytes, so mirrors, reports and
  dashboards keep tallying it. The counts keep speaking because the bytes stay.
- **A second interrupted push adds another file.** Each interruption that
  lands mid-pack leaves at most one; the counts add up the same way.
- **The torn count is permission to close your laptop.** Interrupting a push
  costs the push's own time and, at most, some re-uploaded content on the next
  push. It never costs a backup.

## What would actually be bad

The debris classification has one boundary, and it is load-bearing:

- **A pack an index file names must verify.** If an index names bytes that do
  not match what the listing claims, that is corruption of content a snapshot
  *does* reference. `mirror sync` still exits `1` with
  `the source proved corrupt`, and `verify` still reports it as an error
  finding. Torn debris wording is never applied to a pack an index names —
  that is the line between "the interrupted push's own half file" and "a
  backup lost something".
- **An `unknown` pack is not debris.** In `prune-orphans`, `0 unknown` is the
  state worth wanting: an unclassified pack (one whose header reads but whose
  bytes do not verify, or one the backend would not serve) is reported as
  `unknown` with the reason and makes the survey incomplete — exit `3`, not a
  clean answer. Follow the reason it names instead of reading it as debris.
- **`missing_packs` in `prune-orphans --json` is a contradiction, not
  debris.** An index that names a pack the backend does not list means the
  repository disagrees with itself; that command exits `1`, and
  [troubleshooting.md](../troubleshooting.md#a-command-exits-3) is the place
  to start.

If any of those appears, the right reaction is the one
[troubleshooting.md](../troubleshooting.md) starts with: the two read-only
checks (`doctor`, `status`), then the reason the output names. Torn debris
itself is none of them: it is the tool working, telling you exactly what an
interrupted push left behind and promising that it is inert.
