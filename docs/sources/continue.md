<!-- BEGIN GENERATED -->
# Continue

| Field | Value |
|---|---|
| Id | `continue` |
| Surface | ide extension |
| Status | supported |
| Last verified end to end | not yet verified end to end |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/.continue/sessions/<id>.json + sessions.json` | json | source-confirmed | supported |
| Linux | `~/.continue/sessions/<id>.json + sessions.json` | json | source-confirmed | supported |
| Windows | `%USERPROFILE%\.continue\sessions\<id>.json + sessions.json` | json | source-confirmed | supported |
<!-- END GENERATED -->

## What it is

Continue is an open-source AI coding assistant for IDEs (upstream project `continuedev/continue`, Apache-2.0). It keeps one JSON file per chat session under `~/.continue/sessions/<id>.json`, with a `sessions.json` index beside them. The registry entry is source-confirmed against upstream (`core/util/paths.ts`, `core/util/history.ts`): on Windows the home is `%USERPROFILE%`, with no `%APPDATA%` or XDG variants unless `CONTINUE_GLOBAL_DIR` overrides the whole directory. The registry also notes that Continue's SQLite store under `~/.continue/dev_data/` is analytics, not chat history. The same template is declared for macOS, Linux and Windows.

## What we collect

The per-session JSON files, archived whole and read-only, with a `no-rename` seal policy (overwrite and create semantics, file-descriptor behaviour not confirmed).

Which file is a session is settled by its name: the cell declares `format` = `json` and `session_pattern` = `*-*-*-*-*.json`, so a `.json` file in that directory is a session only when its name carries four hyphens or more. Continue names a session file after the session's own id — a uuid, in the words of the change that added the pattern — so those files qualify, and the `sessions.json` index and any `config.json`, `settings.json` or `notes.json` written beside them carry no hyphen and are not read. The index is also named inside the path template above, which reads like a promise the cell does not make: nothing in the cell declares it as an extra file, so it is never sealed.

The root sits under your home rather than in a working directory, so this source is looked at with no `[harness_roots]` entry at all; the entry only redirects it ([`harness_roots`](../config.md#harness_roots)).

## Coverage and completeness

The generated header says `not yet verified end to end`, and no real Continue session has a recorded run. What this page records is one measured check of an earlier build and, for the current declaration, the shipped registry cell together with the tests that read it.

On 2026-11-01, with a debug build of this branch (base commit `2c8f0faf`) on one machine, an isolated home and synthetic files only, two files `~/.continue/sessions/s1.json` and `sessions.json` (later also `probe.json`) were present and `chat-stasher doctor --json` reported the Continue root as scanned with a session count of 0 and zero unreadable entries.

The cell then declared no `session_pattern`, and a JSON cell without one never yields a session, by design: that is the name-only fail-safe that keeps settings and state JSON out of a session count without reading or guessing from file contents (`build_record` in `crates/chat-stasher/src/scanner.rs`). Continue's session files had no distinguishing name to declare, so none was declared, and the count of 0 was that build reading the directory and finding nothing eligible — Continue's own sessions among them.

Later on 2026-11-01 the pattern was added (commit `a3880fd8`, merged into `main` on 2026-11-02); the template, the format, the root and the confidence are unchanged, and the header above is that cell. That is a change of declaration and of what the scan accepts, not a new measurement: no Continue session on a real machine was read after it, and the `run-once` collection path was not run against it. The acceptance and the negatives are pinned by `crates/chat-stasher/tests/w1064_registry_names_test.rs`, which plants two uuid-named files, the index and three config names in a synthetic `sessions` directory and drives the shipped registry through the scan and through `collect`: two records come back, with both bodies archived, and `sessions.json`, `config.json`, `settings.json` and `notes.json` come back as nothing.

Of aider, crush and Continue, this is the one source whose collection changes for an existing user with nothing touched in their config: aider's root has to be named by hand, crush is held back, and this root resolves on its own. The native id is the file's stem, which is the session id Continue itself wrote, so no new id scheme is introduced here and no id this tool has already written is renamed.

## Quirks

The index and the per-session files share one directory and one extension, which is why this cell carries a name pattern where an ordinary JSON cell carries none. The pattern is a name test, not a uuid check: any `.json` whose name holds four or more hyphens is a session as far as this cell is concerned, so a hyphenated `.json` file you put in that directory yourself would be archived. Whether `CONTINUE_GLOBAL_DIR` is honored by this build was not checked; the registry records the override as an upstream fact only. Each session file records one directory of its own, `workspaceDirectory`, and the archive copies it onto both the working-directory and the container dimension, as the [privacy notes](../../docs-dev/privacy.md) record. `[harness_roots]` can name another root ([config](../config.md#harness_roots)).

## Known issues

From the registry: there is no reader extractor yet, so archived Continue sessions render as the raw view only. The full-text index is the exception that does read them: with no extractor of its own, the shard goes to the index's generic JSON walk, which collects the turn text it finds in the values and reports `not indexable: json` only when it finds none (`structural_walk` in `crates/chat-stasher/src/fts.rs`). The eligibility gap recorded in the check above is closed at the registry level; whether it closes against a real Continue store has not been observed, so this page still does not claim collection works end to end.

## How to check it yourself

Put a copy of a Continue `sessions` directory in a throwaway home, or point `[harness_roots]` `continue` at the copy, and run `chat-stasher doctor`: a session file named `<uuid>.json` reads as one session, and the `sessions.json` index beside it does not add to the count. A file named `s1.json` is still counted as nothing — the pattern rejects it on its name, which is the cell doing its job rather than the 2026-11-01 gap, since `s1` is not a Continue session id. Do it on a copy, not on your live `~/.continue`.

## Changelog

2026-08-26 — the registry entry first appears in commit `12387a19`.
2026-11-01 — the scan check above was recorded for this page; no change to the scanner was made.
2026-11-02 — the json cell gained the `session_pattern` `*-*-*-*-*.json`, so the uuid-named files are the sessions and the index and any config JSON beside them are not (registry change `a3880fd8`, merged into `main` on 2026-11-02); this page is re-cut around that, and the 2026-11-01 counts are kept as the record of the build they were taken on.
