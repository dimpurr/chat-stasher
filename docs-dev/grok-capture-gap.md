# Grok web capture gap: the measured recall and where it comes from

The recall oracle compared the official Grok takeout export
(`prod-grok-backend.json`) against one archive's Grok web sessions on
2026-10-07, joining the export's `conversation.id` to the stage's
session id (the `grok.` prefix stripped). This note records what it
measured, which capture path the archive's population is the output of,
what the measurement does not settle, and the state of the remedy.

## The measurement

- Export: 312 conversations, created 2025-09-04 through 2026-09-24.
- Archive: 61 web-captured conversations whose id matches an export
  conversation — recall 61/312 = 19.6%, RED against the 50% floor
  (the short floor is 90%).
- The 251 missing conversations are classed truly-missing on a
  logged-in account: the account held them, the archive does not.
- Every matched pair was created 2026-07-16 through 2026-09-23. Every
  missing conversation predates 2026-07-16. That boundary is the whole
  finding.
- On the comparable content axes (message count, characters) the 61
  matched pairs are complete: content-short 0. What the live leg
  captures, it captures whole.
- 147 local-harness sessions (Grok CLI and Grok Bot, read from local
  sources rather than the web) are excluded from the recall
  denominator; they are a different capture path and do not answer for
  the web gap.

## Which capture path produced this population

The extension reaches grok.com two ways.

1. **The live leg** captures exactly one response: the content POST
   `.../conversations/<id>/load-responses`
   (`apps/extension/lib/contract.ts:779-872`). The path hint is
   deliberately narrowed to that one route (`contract.ts:784-793`) so
   the conversation-list GET and the skeleton GET are not captured at
   all, and streaming frames are not captured (`contract.ts:864`). A
   conversation therefore enters the archive only when the user opens
   it on grok.com while the extension is active.
2. **The backfill** enumerates the conversation list
   (`GET .../conversations?pageSize=<n>[&pageToken=<t>]`, an opaque
   token) and then fetches each conversation's skeleton and content
   (`apps/extension/lib/backfill/enumerate.ts:3338-3399`). It is the
   only path that reaches conversations the user never opened.

The archive's web population — nothing created before 2026-07-16,
complete content after it — is the live leg's signature. The 61 are the
conversations opened during the capture window; the 251 are the
conversations that were never opened in it. The backfill recovered
none of the pre-window history on this account.

## What the measurement does not settle

The oracle's inputs do not include the stage's backfill state, so two
causes fit the numbers and the code cannot yet distinguish them:

- **The backfill never ran on this account.**
- **It ran and halted.** The plan's list cursor is from-source, not
  live-verified: two reference implementations hand back a `pageToken`
  and one sends an integer `page`
  (`apps/extension/lib/backfill/enumerate.ts:3386-3390`). If the real
  backend honours the integer form, the `pageToken` this plan sends is
  ignored, every "next" page repeats the first, and the engine's
  repeat-page guard halts the enumeration as `shape-changed` — a
  permanent halt (`apps/extension/lib/backfill/engine.ts:2368-2384`).
  An auth failure on the list request would halt just as early, before
  any page is archived.

Reading the stage's backfill halt record (its reason and its last
persisted cursor) is the one measurement that settles which cause
holds, and it is the next step. Whether the list endpoint enumerates
the account's full history or only a recent window is itself
unverified; the threat model already records the Grok backfill as
implemented but not yet observed completing in a real browser
(`docs-dev/threat-model.md:329`).

## The remedy, and its state

- **The historical gap closes by importing the official export** — the
  same `prod-grok-backend.json` the oracle measured against. The
  import seam already names Grok as a platform
  (`crates/chat-stasher/src/import.rs:128`, `:140`); the parser that
  makes `has_parser()` true for it (`import.rs:148-150`) lands on
  branch `w934-tko2-grok-export-parser`, not yet merged. Until it
  merges, `chat-stasher import --platform grok` is a named refusal,
  and the 251 are recoverable only by that merge or by a completed
  backfill.
- **The live leg needs no fix**: its captures are whole. The backfill
  is the designed path for history, and its cursor shape needs a
  logged-in session to verify — the same verification the threat model
  records as outstanding.

The gap is reported as a measurement, not folded into a zero: 19.6%
recall, RED, with the missing classed truly-missing rather than
unreadable — the three states stay distinct.
