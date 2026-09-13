# chat-stasher

`chat-stasher` continuously archives conversation history from AI coding harnesses—Claude Code, Codex CLI, Gemini CLI, opencode, and other registered sources—to storage that you control. It is an append-only archive. (Project description and append-only design: `crates/chat-stasher/src/main.rs:34`.)


## Why this exists

Your harness may already be deleting history before you notice.

- **Claude Code:** its official documentation says files older than `cleanupPeriodDays` are deleted at startup, with a default of 30 days. The project registry records the same default for Claude Code. ([official documentation](https://code.claude.com/docs/en/claude-directory); `crates/chat-stasher/data/harness-registry-v1.json:28`.) The R1 audit for this project also reproduced the dangerous case where a configuration parse/load failure silently fell back to the 30-day default and cleanup began; the audit evidence is not stored in this public repository.
- **Gemini CLI:** its official session-management documentation puts sessions under `~/.gemini/tmp/<project_hash>/chats/` and says the default retention policy is 30 days. The project registry records that the directory is literally named `tmp`, although it contains chat history. ([official documentation](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/session-management.md); `crates/chat-stasher/data/harness-registry-v1.json:84-124`.)
- **This tool’s `doctor`:** the R1 audit ran it on the maintainer’s machine and found a live risk: one harness had no retention policy configured and its oldest session had already crossed the threshold. That is an audit observation, not a promise that every machine will show the same result.

The useful first question is therefore not “is the archive elegant?” It is: **is any harness silently deleting my history right now?** `doctor` is intended to answer that question without modifying the source histories. (`crates/chat-stasher/src/main.rs:268-269`.)

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

`doctor` is the smallest useful path: it is read-only and reports paths, counts, bytes, and timestamps rather than session text. (`crates/chat-stasher/src/main.rs:268-269`.)

**Verification status:** `cargo run -- --help` and `cargo run -- doctor` were both run successfully against this checkout. The `doctor` output is not reproduced here because it contains local paths.

## Commands

The Rust source is the current command definition; the descriptions below were cross-checked against captured `--help` output. (`crates/chat-stasher/src/main.rs:44-827`.)

- `init` — writes a commented default config if none exists; non-destructive. (`main.rs:45-46`.)
- `run-once` — collects one pass from local sources, pushes when configured and changed, then exits. (`main.rs:48-85`.)
- `schedule` — renders a launchd plist or systemd user service/timer template; never installs it. (`main.rs:86-145`.)
- `push --stage <your-stage>` — moves sealed session shards into the rustic repository, creating the repository on first use and persisting the masterkey. (`main.rs:146-183`.)
- `status` — reports whether scheduled archiving is working and summarizes local scanner findings (read-only). (`main.rs:183-221`.)
- `read` — dumps one session as sequence-concatenated data and prints its SHA-256, or with `--all-machines` merges newest snapshots and reports per-session digests. (`main.rs:223-267`.)
- `doctor` — diagnoses whether a harness may silently delete sessions; report is limited to paths, counts, bytes, and timestamps. (`main.rs:267-278`.)
- `verify --level l1|l2|l3|all` — checks repository structure, repository content, and/or reconciles the archive with the sealed staging manifest. (`main.rs:280-318`.)
- `dest-init` — initialises a new destination as a full extra copy from local and existing destinations. (`main.rs:319-369`.)
- `search` — searches one destination's archive by session metadata. (`main.rs:370-420`.)
- `view` — opens an ephemeral local web view of one destination's session list on 127.0.0.1. (`main.rs:421-476`.)
- `ingest --inbox <your-inbox> --stage <your-stage>` — consumes complete `deepseek-<sessionId>.json` bundles, skips `.part` files, also accepts the multi-bundle `*.jsonl` files the extension's "export undelivered captures" button produces, creates sealed staging shards, retires consumed inputs, and deduplicates identical bytes. (`main.rs:476-498`; `crates/chat-stasher/src/inbox.rs:56-60`.)
- `collect --stage <your-stage>` — reads every scanner session into staging shards without mutating harness sources. (`main.rs:499-534`.)
- `seal --harness <id> --active <your-active-file> --stage <your-stage>` — seals one file already inside `--stage` into the next sealed-shard slot; never renames a harness-owned path. (`main.rs:535-567`.)
- `install-native-host --stage <absolute path>` — writes the Native Messaging host manifest into each installed browser's discovery directory so the extension can hand conversations to this binary directly, and records the stage the browser-spawned host is allowed to write to as `[native_host] stage` in your config; the stage must already exist, because the host never creates one. `--uninstall` removes exactly those files and nothing else, and every path touched is printed. Per-user, no elevation. (`main.rs:638-680`.)
- `native-host` — the host loop itself: it reads one length-prefixed request frame from stdin, writes exactly one response frame to stdout, and exits 0. A frame that ends early — EOF inside the length prefix or the body — is answered with silence and a non-zero exit, never with a response, because a stray byte on stdout corrupts the frame. `--self-test` is the smaller check that the process starts at all: one line of JSON, exit 0. (`main.rs:681-702`, `:1565-1575`.)

There is no `scan` subcommand in the current source; `status` is the scanner-facing command. (`main.rs:183-221`.)

## What it reads, writes, and sends

The paths below are placeholders on purpose. Do not paste real account names, hostnames, or keys into examples.

- `status` reads the local harness locations known to the registry and prints IDs, paths, sizes, mtimes, and flags; it does not print session content. (`main.rs:5918-5945`.)
- `doctor` reads local harness metadata for its diagnostic report; its declared output is paths, counts, bytes, and timestamps. (`main.rs:267-268`.)
- `ingest` reads complete export files from the `--inbox` you provide and writes sealed shards beneath the `--stage` you provide; consumed inputs are moved under `<your-inbox>/consumed/`. It prints paths, counts, and SHA-256 values, not conversation text. (`main.rs:476-498`.)
- `seal` reads the registry and the active file you name, then may rename that file into the stage tree. The registry policy and confidence gate are part of the decision. (`main.rs:535-567`.)
- `push`, `read`, and `verify` read the repository and key file selected by config or flags. They can use a backend you explicitly configure with repository options; do not assume those three commands are offline. (`main.rs:146-183`; `main.rs:223-267`; `main.rs:280-318`.)

What does not leave the process through the metadata-only paths: `status`, `doctor`, and `ingest` do not print conversation bodies, and the ingest summary is explicitly metadata-only. (`main.rs:476-498`; `main.rs:5918-5945`.) `read` is intentionally different: its single-session mode dumps session data to your stdout, so treat that command as payload output. (`main.rs:222-224,4294-4325`.)

The destination is selected by your config and flags: local stage/repository paths or a backend you configure. The source exposes repository, key-file, and backend-option inputs rather than a hard-coded destination. (`main.rs:48-85`; `main.rs:146-183`.)

## What this does not do / current limits

This section is intentionally blunt:

- **Zed and Cursor session enumeration is not implemented in this version.** Their registry entries are path research, not a promise that `status` can enumerate their conversations; Cursor’s registry evidence is explicitly community-only, and Zed’s macOS path is not individually verified. (`crates/chat-stasher/data/harness-registry-v1.json:362-397`; `crates/chat-stasher/data/harness-registry-v1.json:165-211`.)
- **Claude Code on Windows has an unresolved path-sanitize detail.** The registry says the exact handling of the drive-letter colon and backslash in the short-path form is not determined and needs a real Windows test. (`crates/chat-stasher/data/harness-registry-v1.json:28`.)
- **`ingest` is not a generic import API.** Its documented input is complete `deepseek-<sessionId>.json` exports; `.part` files are skipped, and the source notes that bundles carry no account field. (`main.rs:476-498`.)
- **The browser extension's history backfill works on exactly one platform, and "lists your conversations" is not the same as "saves them."** There are three tiers, and the middle one is the easy one to misread:
  - **Backfill can recover the actual conversation text: ChatGPT only.** (`apps/extension/lib/backfill/enumerate.ts:880-886`.)
  - **Backfill can list your conversations but saves none of their content: DeepSeek and Perplexity.** (`apps/extension/lib/backfill/enumerate.ts:894-900`.) With backfill enabled on these two, the extension enumerates your existing conversations and shows a pending count — **and then delivers nothing to the archive.** Nothing is queued in the outbox and no shard is sealed, so **your DeepSeek and Perplexity history is not backed up.** The reason is recorded in the code: the *list* endpoint for each has cross-checked open-source provenance, the *single-conversation* endpoint has none, and we will not guess one — a wrong guess would not error, it would silently archive the first few turns of every chat while you believed it had them all (`apps/extension/lib/backfill/enumerate.ts:545-556`, `:630-640`).
  - **Backfill is not implemented at all: Gemini, Claude, Kimi.** The leg halts before issuing any request (`apps/extension/lib/backfill/enumerate.ts:752`).
  The extension's popup states the same three tiers in the same terms (`apps/extension/lib/popup-view.ts:610-623`). This limit is about **backfill of past conversations**; passive capture of the conversation currently open in your browser is a separate leg with its own per-platform table (`apps/extension/lib/contract.ts:67-313`).
- **`seal` is not a universal file-renaming tool.** It is gated by the registry’s `seal_policy`, evidence, and platform confidence; fd-holder harnesses such as Codex are refused because renaming can strand later writes in the old inode. (`main.rs:535-567`; `crates/chat-stasher/data/harness-registry-v1.json:71-72`.)
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
  (`crates/chat-stasher/src/store.rs:1106-1113`, `:1064-1068`).
- **There is no restore command.** `read` returns one session at a time to
  stdout (`crates/chat-stasher/src/main.rs:223-225,4294-4325`); bulk restore is not
  implemented.
- **Captured conversations are plaintext until they are delivered.** A live
  capture is written into the extension's own IndexedDB outbox *before* any
  delivery is attempted, and is removed only when the host answers with a
  matching `ack` (`apps/extension/lib/outbox.ts:309-377`, `:379-394`;
  `apps/extension/entrypoints/background.ts:160-177`). A delivery the host never
  confirms stays there, and the popup's export file contains the same bodies.

## Development status
 
The repository contains the command implementation, harness registry, inbox schema, and release-gate script. `scripts/release-gate.sh` prints `GATE: PASS` or `GATE: FAIL`; both directions were exercised on this checkout (`--selftest` injects one byte and must produce `GATE: FAIL`).

Before using this project in production, run `doctor` on a machine whose paths you are willing to inspect, and verify the remote-destination policy you actually intend to use.
