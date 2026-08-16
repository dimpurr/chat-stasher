# chat-stasher

> Continuously archive every LLM conversation — across every harness and the web — to a destination **you** control.
>
> **Append-only. Not mirror-sync.**

🚧 **Pre-alpha.** Nothing to install yet.

## Why

A backup that deletes what your source deleted is not a backup. This project exists because a mirror-sync tool
(`rsync --delete` semantics) plus a 30-day local rotation quietly destroyed two weeks of session history —
on both sides at once.

## Design constraints

1. **Append-only archive, never mirror sync.** Deleting locally must never delete remotely.
2. **Capture never fails.** Raw bytes are archived independently of whether we can parse them —
   a format change makes the index stale, never the data lost.
3. **Archive-first, not sync-first.** Central archive + central search are first-class;
   "restore machine A onto machine B" is deliberately low priority.
4. **Your destination.** Local disk, your S3/WebDAV/git — the tool never requires our servers.

## Status

Design and research are complete; implementation has not started.
Planning documents are kept in a separate private repository.

## License

Not yet chosen — see the decision record before contributing.
