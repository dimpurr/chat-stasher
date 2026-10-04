# Named-session archive purge: interaction design

This note defines the user interaction for explicitly purging one archived
session. It is design guidance for future implementation; it does not describe
a command that is available today.

## Deliberate exception to append-only

Repositories are opened with `append_only:true`; that setting blocks the
delete-class operations a purge would require. Purge is therefore a deliberate,
explicit exception to the archive's append-only invariant, not ordinary archive
maintenance. A future implementation must make the exception visible and
require an opt-in operation scoped to the selected destinations and exact
session identity. It must act as a single writer: establish the target state
while no competing writer can change it, refuse if that state cannot be
established, and limit mutation to the selected session's archived occurrences.
It must not silently disable append-only for routine commands or turn the
archive into a mirror of source state. Until such an implementation exists,
archives remain append-only and this note specifies no executable purge flow.

## Target one archived session

The user names one complete archived session identity, with no prefix matching,
search filters, date ranges, or implicit “all matches” behavior. The identity
shown for confirmation includes the full session id and its machine partition,
so sessions with the same id in different partitions cannot be confused. The
user also chooses the destination or destinations to inspect. There is no
implicit destination set.

The target is the archived session. Purge removes that session's occurrences
from the archive copies selected for this operation. It does not delete or edit
the source conversation, its staged copy, or any other source data. In
particular, `push` archives the whole stage tree, so a later push can restore a
session whose staged copy remains; “purged” describes the selected archive
state at the time of the result and does not promise continued absence. Deleting
a conversation at its source does not request or cause archive deletion.
Source-side deletion and archive purge are separate user actions.

## Preview before confirmation

The first step is read-only. It resolves the exact target independently at each
selected destination and shows a bounded summary before any deletion is
possible:

- the full session identity and selected destinations;
- for each destination, whether the target was found, fully checked and ready
  for purge, completely absent, or not determined because the destination could
  not be read to completion;
- for a found target, the number of affected archived shard occurrences and
  their total bytes, across every snapshot in that destination that contains
  the target; and
- the number of selected destinations whose status is still unknown.

The preview contains no conversation text. It must enumerate snapshots
cumulatively, because a session can remain readable through an older snapshot
even when a newer snapshot also contains it. The counts cover every occurrence
in every snapshot in each selected destination, not only the newest snapshot or
the newest occurrence. “Absent” is reported only after a complete read of all
snapshots proves the target is not there. An offline, unreadable, or partially
read destination is **unknown**, never absent. The preview identifies which
destinations are reachable and which remain unchecked; it does not imply that
reachable copies are the only copies.

## Explicit, scoped confirmation

Purge requires a separate affirmative action after the preview. The confirmation
repeats the exact session identity and the destinations ready for purge. It
must not broaden the target or destination set, and it must not treat unknown
destinations as cleared. Before deletion, the operation rechecks every snapshot
and every occurrence of the selected target in each destination. If the
snapshot set or occurrence set changed, or complete coverage cannot be
established, it stops for that destination and reports why; it must never leave
an older readable occurrence while reporting that destination purged.

Confirmation authorizes deletion only on the destinations whose target was
found and whose state can be safely acted on. It is not a claim that every copy
has been removed. A later attempt can inspect destinations that were offline or
unknown without changing the already reported result of the first attempt.

## Report each destination honestly

The final summary gives one result per selected destination and accounts for
every snapshot and occurrence in that destination. It distinguishes purged,
completely absent, failed, and unknown because the destination could not be
read or acted on. “Purged” means all occurrences covered by the selected
destination were removed; any incomplete coverage or remaining occurrence
prevents that status. If some copies were purged while another destination was
offline, the overall result is **partial** and names the unresolved
destination; it does not say “purged everywhere” or present the reachable
subset as the complete archive. A read failure never becomes a zero count or a
successful absence.

The result concerns archive copies only. It makes no claim about source-side
copies, and it never propagates source deletion into the archive. This preserves
the distinction between removing a named archived session and managing the
conversation at its original source.
