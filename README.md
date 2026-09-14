# chat-stasher

`chat-stasher` continuously archives conversation history from AI coding harnesses—Claude Code, Codex CLI, Gemini CLI, opencode, and other registered sources—to storage that you control. It is an append-only archive. (Project description and append-only design: `crates/chat-stasher/src/main.rs:34`.)

## Support at a glance

Status as of **2026-09**. **Verified** means real sessions were archived end to end on a maintainer's machine; the version is the one installed there at that time. Anything else is stated as what it is.

**Local AI coding tools** (the `chat-stasher` CLI)

| Tool | Status | Verified version |
|---|---|---|
| Claude Code | ✅ Verified | 2.1.270 (2026-09) |
| OpenAI Codex CLI | ✅ Verified | 0.146.0 (2026-09) |
| opencode | ✅ Verified | 1.18.4 (2026-09) |
| Gemini CLI | ✅ Verified | 0.36.0 (2026-09) |
| Cursor | ✅ Verified | not recorded |
| Grok CLI | 🟡 Registered; installed, but no sessions archived yet | — |
| GitHub Copilot CLI · aider · crush · Zed · Continue | 🟡 Registered, not verified | — |
| Kimi Code | 📋 Planned | — |

**Web AI chats** (the browser extension)

| Platform | Live capture (conversations you open) | Backfill (past conversations) |
|---|---|---|
| ChatGPT | ✅ Verified (2026-09), including conversations over 8 MB | 🔄 In progress |
| DeepSeek | ✅ Verified (2026-09) | 🟡 Implemented (lists conversations and fetches their content); not yet verified in a real browser |
| Perplexity | 🟡 Coded; cannot recognise a session id, so delivers nothing | 🟡 Lists conversations only; saves no content |
| Gemini · Claude · Kimi | 🟡 Coded, not verified | ❌ Not available |
| Grok | 📋 Planned | 📋 Planned |

**Where the archive can go** (encrypted with `rustic`)

| Destination | Status |
|---|---|
| SFTP (e.g. Hetzner Storage Box) | ✅ Verified — in daily use on three machines (2026-09) |
| Local folder / disk | ✅ Supported |
| rustic REST server (`rest:`) | 🟡 Accepted by the config, not verified |
| S3 · Google Drive · others | 📋 Planned |


## Why this exists

Your harness may already be deleting history before you notice.

- **Claude Code:** its official documentation says files older than `cleanupPeriodDays` are deleted at startup, with a default of 30 days. The project registry records the same default for Claude Code. ([official documentation](https://code.claude.com/docs/en/claude-directory); `crates/chat-stasher/data/harness-registry-v1.json:28`.) The R1 audit for this project also reproduced the dangerous case where a configuration parse/load failure silently fell back to the 30-day default and cleanup began; the audit evidence is not stored in this public repository.
- **Gemini CLI:** its official session-management documentation puts sessions under `~/.gemini/tmp/<project_hash>/chats/` and says the default retention policy is 30 days. The project registry records that the directory is literally named `tmp`, although it contains chat history. ([official documentation](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/session-management.md); `crates/chat-stasher/data/harness-registry-v1.json:84-124`.)
- **This tool’s `doctor`:** the R1 audit ran it on the maintainer’s machine and found a live risk: one harness had no retention policy configured and its oldest session had already crossed the threshold. That is an audit observation, not a promise that every machine will show the same result.

The useful first question is therefore not “is the archive elegant?” It is: **is any harness silently deleting my history right now?** `doctor` is intended to answer that question without modifying the source histories. (`crates/chat-stasher/src/main.rs:310-311`.)

## Install and first check

If you are installing this to use it rather than to work on it, read
**[`docs/install.md`](docs/install.md)** instead: it covers both halves (CLI and
browser extension), the one-time setup list — including registering the Native
Messaging host with `chat-stasher install-native-host --stage <path>` — how to
confirm the archive is actually running, and what does not exist yet.

The developer path, using your own repository URL and a directory you choose:

```sh
git clone <repository-url> <your-directory>
cd <your-directory>
cargo run -- doctor
```

`doctor` is the smallest useful path: it is read-only and reports paths, counts, bytes, and timestamps rather than session text. (`crates/chat-stasher/src/main.rs:310-311`.)

**Verification status:** `cargo run -- --help` and `cargo run -- doctor` were both run successfully against this checkout. The `doctor` output is not reproduced here because it contains local paths.

## Commands

The Rust source is the current command definition; the descriptions below were cross-checked against captured `--help` output. (`crates/chat-stasher/src/main.rs:83-863`.)

- `init` — writes a commented default config if none exists; non-destructive. (`main.rs:87-88`.)
- `run-once` — collects one pass from local sources, pushes when configured and changed, then exits. (`main.rs:90-127`.)
- `schedule` — renders a launchd plist or systemd user service/timer template; never installs it. (`main.rs:128-187`.)
- `push --stage <your-stage>` — moves sealed session shards into the rustic repository, creating the repository on first use and persisting the masterkey. (`main.rs:188-225`.)
- `status` — reports whether scheduled archiving is working and summarizes local scanner findings (read-only). (`main.rs:225-263`.)
- `read` — dumps one session as sequence-concatenated data and prints its SHA-256, or with `--all-machines` merges newest snapshots and reports per-session digests. (`main.rs:265-309`.)
- `doctor` — diagnoses whether a harness may silently delete sessions; report is limited to paths, counts, bytes, and timestamps. (`main.rs:309-320`.)
- `verify --level l1|l2|l3|all` — checks repository structure, repository content, and/or reconciles the archive with the sealed staging manifest. (`main.rs:322-360`.)
- `dest-init` — initialises a new destination as a full extra copy from local and existing destinations. (`main.rs:361-411`.)
- `search` — searches one destination's archive by session metadata: session-id prefix, machine, harness, and a **conversation-time** window. The window is given as local calendar days — `--day 2026-01-15`, or `--since`/`--until` for a range — and a session matches when its own activity interval intersects it, so `--day` finds a conversation held that day even if the machine was last pushed months later. A session whose conversation time is unknown is never dropped and never silently counted as a non-match: it is listed separately with the reason, and while a window is active the exit code is `3` rather than `1`, because "0 matched" is then unproven. `--json` gives the matched, not-matched and could-not-be-placed groups as separate fields. (`main.rs:412-473`; the filters are shared with the planned `export` in `crates/chat-stasher/src/selector.rs:1-247`.)
- `ui` — opens the archive dashboard in a browser: totals, a per-machine health row, a machine × source matrix of session counts, and a weekly activity heatmap, all on `127.0.0.1` with an OS-assigned port. Clicking a matrix cell or a heatmap week opens a session list filtered through the same selector `search` uses, so a drill-down and a `search` with the same flags return the same sessions. The page is rendered from archive *metadata* only; a session's text is fetched and decrypted only after you click through to it, and the byte cost is printed first. `view` is a deprecated alias for one release: it prints a one-line notice on stderr and behaves identically. (`main.rs:474-499`.)
- `ingest --inbox <your-inbox> --stage <your-stage>` — consumes complete `deepseek-<sessionId>.json` bundles, skips `.part` files, also accepts the multi-bundle `*.jsonl` files the extension's "export undelivered captures" button produces, creates sealed staging shards, retires consumed inputs, and deduplicates identical bytes. (`main.rs:500-521`; `crates/chat-stasher/src/inbox.rs:56-60`.)
- `collect --stage <your-stage>` — reads every scanner session into staging shards without mutating harness sources. (`main.rs:522-557`.)
- `seal --harness <id> --active <your-active-file> --stage <your-stage>` — seals one file already inside `--stage` into the next sealed-shard slot; never renames a harness-owned path. (`main.rs:559-591`.)
- `install-native-host --stage <absolute path>` — writes the Native Messaging host manifest into each installed browser's discovery directory so the extension can hand conversations to this binary directly, and records the stage the browser-spawned host is allowed to write to as `[native_host] stage` in your config; the stage must already exist, because the host never creates one. `--uninstall` removes exactly those files and nothing else, and every path touched is printed. Per-user, no elevation. (`main.rs:661-703`.)
- `native-host` — the host loop itself: it reads one length-prefixed request frame from stdin, writes exactly one response frame to stdout, and exits 0. A frame that ends early — EOF inside the length prefix or the body — is answered with silence and a non-zero exit, never with a response, because a stray byte on stdout corrupts the frame. `--self-test` is the smaller check that the process starts at all: one line of JSON, exit 0. (`main.rs:704-725`, `:1563-1573`.)

There is no `scan` subcommand in the current source; `status` is the scanner-facing command. (`main.rs:225-263`.)

## What it reads, writes, and sends

The paths below are placeholders on purpose. Do not paste real account names, hostnames, or keys into examples.

- `status` reads the local harness locations known to the registry and prints IDs, paths, sizes, mtimes, and flags; it does not print session content. (`main.rs:6059-6086`.)
- `doctor` reads local harness metadata for its diagnostic report; its declared output is paths, counts, bytes, and timestamps. (`main.rs:309-310`.)
- `ingest` reads complete export files from the `--inbox` you provide and writes sealed shards beneath the `--stage` you provide; consumed inputs are moved under `<your-inbox>/consumed/`. It prints paths, counts, and SHA-256 values, not conversation text. (`main.rs:500-521`.)
- `seal` reads the registry and the active file you name, then may rename that file into the stage tree. The registry policy and confidence gate are part of the decision. (`main.rs:559-591`.)
- `push`, `read`, and `verify` read the repository and key file selected by config or flags. They can use a backend you explicitly configure with repository options; do not assume those three commands are offline. (`main.rs:188-225`; `main.rs:265-309`; `main.rs:322-360`.)

What does not leave the process through the metadata-only paths: `status`, `doctor`, and `ingest` do not print conversation bodies, and the ingest summary is explicitly metadata-only. (`main.rs:500-521`; `main.rs:6059-6086`.) `read` is intentionally different: its single-session mode dumps session data to your stdout, so treat that command as payload output. (`main.rs:264-266,4690-4808`.)

The destination is selected by your config and flags: local stage/repository paths or a backend you configure. The source exposes repository, key-file, and backend-option inputs rather than a hard-coded destination. (`main.rs:90-127`; `main.rs:188-225`.)

## What this does not do / current limits

This section is intentionally blunt:

- **Zed and Cursor session enumeration is not implemented in this version.** Their registry entries are path research, not a promise that `status` can enumerate their conversations; Cursor’s registry evidence is explicitly community-only, and Zed’s macOS path is not individually verified. (`crates/chat-stasher/data/harness-registry-v1.json:362-397`; `crates/chat-stasher/data/harness-registry-v1.json:165-211`.)
- **Claude Code on Windows has an unresolved path-sanitize detail.** The registry says the exact handling of the drive-letter colon and backslash in the short-path form is not determined and needs a real Windows test. (`crates/chat-stasher/data/harness-registry-v1.json:28`.)
- **`ingest` is not a generic import API.** Its documented input is complete `deepseek-<sessionId>.json` exports; `.part` files are skipped, and the source notes that bundles carry no account field. (`main.rs:500-521`.)
- **The browser extension's history backfill is implemented on two platforms and lists-only on a third, and "lists your conversations" is not the same as "saves them."** There are three tiers, and the middle one is the easy one to misread:
  - **Backfill implements listing conversations and fetching their content: ChatGPT and DeepSeek.** (`apps/extension/lib/backfill/enumerate.ts:1000-1006`.) Both are wired through to the local host, but **neither has been observed completing a backfill in a real browser**, so read this as *implemented*, not *verified*. DeepSeek's conversation body is requested with `GET /api/v0/chat/history_messages?chat_session_id=<id>` — the route DeepSeek's own page calls over XHR when a user opens a past conversation, observed in a real logged-in browser session, and the same route several mutually independent open-source exporters request (`apps/extension/lib/backfill/enumerate.ts:685-699`, `:748-751`). 🔴 **Unverified: whether that route pages or truncates a long conversation.** The extension does no paging, so a very long conversation may be stored as only its first part, and nothing in the response envelope read here distinguishes that from a complete one (`apps/extension/lib/backfill/enumerate.ts:701-714`).
  - **Backfill can list your conversations but saves none of their content: Perplexity.** (`apps/extension/lib/backfill/enumerate.ts:1014-1020`.) With backfill enabled, the extension enumerates your existing conversations and shows a pending count — **and then delivers nothing to the archive.** Nothing is queued in the outbox and no shard is sealed, so **your Perplexity history is not backed up.** The reason is recorded in the code: the *list* endpoint has cross-checked open-source provenance (`apps/extension/lib/backfill/enumerate.ts:835-846`), the *single-conversation* endpoint has none and is left as `null` rather than guessed (`:825-834`) — a wrong guess would not error, it would silently archive the first few turns of every chat while you believed it had them all.
  - **Backfill is not implemented at all: Gemini, Claude, Kimi.** The leg halts before issuing any request (`apps/extension/lib/backfill/enumerate.ts:872`).
  The extension's popup states the same three tiers in the same terms (`apps/extension/lib/popup-view.ts:628-641`). This limit is about **backfill of past conversations**; passive capture of the conversation currently open in your browser is a separate leg with its own per-platform table (`apps/extension/lib/contract.ts:66-312`).
- **`seal` is not a universal file-renaming tool.** It is gated by the registry’s `seal_policy`, evidence, and platform confidence; fd-holder harnesses such as Codex are refused because renaming can strand later writes in the old inode. (`main.rs:559-591`; `crates/chat-stasher/data/harness-registry-v1.json:71-72`.)
- **The release gate is not a substitute for installation.** `scripts/release-gate.sh` builds `target/debug/chat-stasher` if it is missing and generates its own synthetic opaque fixtures, so a contributor can run it with no arguments and no setup (`--real-data` opts into local Claude sessions instead). It exercises push/read/verify/doctor. (`scripts/release-gate.sh:3-21`.)
- **License.** The project is licensed under the Apache License 2.0 (`LICENSE:2-3`; `crates/chat-stasher/Cargo.toml:5`).

## Security and privacy

Two documents, both written to be read before you trust this with an archive you
cannot recreate:

- **[`docs/threat-model.md`](docs/threat-model.md)** — organised as *who can see
  what*: us (nothing — there is no server in this design), your destination
  provider (encrypted objects, but your backup rhythm and volume leak as
  metadata), other programs on your machine (they can read the **plaintext**
  captures waiting in the extension's outbox, the staged shards, and your master
  key file), the chat platforms, and one row we honestly could not resolve: what
  other browser extensions can observe. It also lists the weaknesses and the
  threats we do **not** defend against.
- **[`docs/privacy.md`](docs/privacy.md)** — the store-listing privacy policy:
  what is collected (nothing, and how you can verify that yourself), where data
  is stored, who it is shared with, how long it is kept, how to delete it, and
  the known plaintext window. Written for outside readers and store reviewers.
- **[`SECURITY.md`](SECURITY.md)** — how to report a vulnerability, what is in
  scope, and what response you can and cannot expect. Reports go to
  `work@team.iopho.com` rather than a public issue.

Three things worth knowing before reading either:

- **Your master key file is the only key.** Lose it and the archive is
  unreadable forever, with no recovery path of any kind
  (`crates/chat-stasher/src/store.rs:1189-1196`, `:1147-1151`).
- **There is no restore command.** `read` returns one session at a time to
  stdout (`crates/chat-stasher/src/main.rs:265-267,4690-4808`); bulk restore is not
  implemented.
- **Captured conversations are plaintext until they are delivered.** A live
  capture is written into the extension's own IndexedDB outbox *before* any
  delivery is attempted, and is removed only when the host answers with a
  matching `ack` (`apps/extension/lib/outbox.ts:309-377`, `:379-394`;
  `apps/extension/entrypoints/background.ts:186-203`). A delivery the host never
  confirms stays there, and the popup's export file contains the same bodies.

## Development status
 
The repository contains the command implementation, harness registry, inbox schema, and release-gate script. `scripts/release-gate.sh` prints `GATE: PASS` or `GATE: FAIL`; both directions were exercised on this checkout (`--selftest` injects one byte and must produce `GATE: FAIL`).

Before using this project in production, run `doctor` on a machine whose paths you are willing to inspect, and verify the remote-destination policy you actually intend to use.
