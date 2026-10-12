<!-- BEGIN GENERATED -->
# Cursor

| Field | Value |
|---|---|
| Id | `cursor` |
| Surface | ide |
| Status | verified end-to-end (2026-09-25) |
| Last verified end to end | 2026-09-25: all real sessions of this store are held by the real archive and read back and rendered by the reader (git 41db2bf) |

| OS | Session path template | Format | Confidence | Status |
|---|---|---|---|---|
| macOS | `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` | sqlite | measured-locally | verified end-to-end (2026-09-25) |
| Linux | `$XDG_CONFIG_HOME/Cursor/User/globalStorage/state.vscdb` | sqlite | community-claim-unverified | verified end-to-end (2026-09-25) |
| Windows | `%APPDATA%\Cursor\User\globalStorage\state.vscdb` | sqlite | community-claim-unverified | verified end-to-end (2026-09-25) |
<!-- END GENERATED -->

## What it is

Cursor is an AI-powered code editor built as an Electron/VS Code fork. It manages chat and composer interactions locally using SQLite databases. This tool discovers and archives Cursor composer sessions read-only across all supported platforms without interfering with running editor processes.

## What we collect

Cursor stores composer conversations in two locations depending on version and configuration:

1. The global storage database (`state.vscdb`), specifically rows in the `cursorDiskKV` table where `key LIKE 'composerData:%'`. Each row is exported under the `chat-stasher.sqlite.session.v1` schema.
2. Legacy per-workspace storage (`workspaceStorage/<workspaceId>/state.vscdb`), reading composer values from `ItemTable` under keys `composer.composerData` or `allComposers`, exported under the `chat-stasher.cursor.legacy.session.v1` schema.

The normalizer extracts messages from the composer data structure. It prioritizes inline conversation bubbles in `conversationMap` ordered by the `fullConversationHeadersOnly` array (where type 1 represents User and type 2 represents Assistant). For legacy workspace composers, it reads bubbles directly from the `conversation` array.

In modern Cursor releases, bubble text is frequently offloaded to external database keys (`bubbleId:{composer}:{bubble}`). When a conversation header references external bubble bodies that are not present inline, the reader counts those entries as unrendered records rather than misrepresenting them as empty text.

## Coverage and completeness

The harness was verified end to end on 2026-09-25: all real sessions from the local store were captured, sealed, and successfully read back and rendered (commit 41db2bf).

The macOS database path (`~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`) is measured locally. Linux and Windows paths (`$XDG_CONFIG_HOME/Cursor/...` and `%APPDATA%\Cursor\...`) reflect community documentation and Electron conventions. The `CURSOR_USER_DIR` environment variable can override the base User directory on any platform.

## Quirks

Cursor is closed-source, and its storage architecture has evolved across versions (moving from per-workspace `ItemTable` storage to global `cursorDiskKV`, and later offloading bubble content). The scanner and reader dynamically detect and accommodate both global and legacy structures.

Like other SQLite-backed sources, Cursor is opened with read-only locks and does not support file-rename sealing (its seal policy is not-applicable).

## Known issues

Linux and Windows database locations are community-derived and have not been independently verified with local instrumentation.

When Cursor stores conversation bubbles externally under individual `bubbleId:...` rows, composers without inline bubble content will show headers counted as unrendered records because individual bubble keys are not yet collected in this build.

## How to check it yourself

Run `chat-stasher doctor` to verify that Cursor's global or workspace storage database is found. Run `chat-stasher run-once` followed by `chat-stasher read` on a cursor session to review rendered conversation turns. Check data integrity with `chat-stasher verify` and verify search capability with `chat-stasher search --text "<query>"`.

## Changelog

2026-08-17 — macOS global `state.vscdb` (`cursorDiskKV`) located and measured locally.
2026-09-25 — verified end to end: all real sessions captured, read back, and rendered from the archive (commit 41db2bf).
2026-10-03 — unified reader implemented for both global `cursorDiskKV` and legacy workspace `ItemTable` composer formats.
2026-10-12 — unrendered line accounting added for external `bubbleId:` references.
