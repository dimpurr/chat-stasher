# Redact an archived session: core design

This is design guidance for a future feature. Redaction, tombstones, and
selective archive purge are not implemented or shipped.

## Redaction lifecycle

Redaction applies to one exact archived session identity, including its platform
and machine partition, and to explicitly selected archive destinations. It is
an intentional, user-requested exception to append-only retention; ordinary
collection and source changes never trigger it. The preview, confirmation,
coverage rules, and per-destination results follow
[named-session-purge.md](named-session-purge.md).

After the user edits the session, the operation must prevent concurrent
collection from racing with the change. It records a durable tombstone for the
exact identity before removing prior archived occurrences. The tombstone is
checked by every collection path before content is sealed, including retries
and restored archives. It suppresses source copies of that identity so a later
scan, backfill, or stage push cannot silently put the deleted version back.

Once prior occurrences have been purged from a destination, the edited content
is sealed as a new archive state there. The tombstone remains active alongside
that state: it blocks recollection of the source version but does not hide or
remove the explicitly edited state. A later redaction is another explicit
operation, not an automatic consequence of collection.

The tombstone contains identity and suppression state only; it does not retain
deleted conversation content. Collectors must consult it before deduplication
or sealing, and must not treat an unreadable or unavailable tombstone stream as
empty. If the tombstone cannot be read or committed, collection of the affected
identity stops with an unknown/failure result. If purge or sealing is incomplete,
the operation reports that destination as partial or failed and can resume
without claiming success. It must never report redaction complete while an old
occurrence remains or the edited state was not sealed.

Archive redaction does not edit or delete the source conversation. Deleting a
conversation at its source does not request archive redaction; the two actions
remain separate. This lifecycle makes the archive change explicit while
preventing later collection from undoing it silently.
