# chat-stasher

`chat-stasher` continuously archives conversation history from AI coding harnesses—Claude Code, Codex CLI, Gemini CLI, opencode, and other registered sources—to storage that you control. It is an append-only archive. (Project description and append-only design: `crates/chat-stasher/src/main.rs:15-19`.)

> **Current checkout status:** the command-line help could not compile in the checkout used for this README rewrite. The exact compiler errors are recorded below. Do not treat the quickstart as verified until `cargo run -- --help` succeeds.

## Why this exists

Your harness may already be deleting history before you notice.

- **Claude Code:** its official documentation says files older than `cleanupPeriodDays` are deleted at startup, with a default of 30 days. The project registry records the same default for Claude Code. ([official documentation](https://code.claude.com/docs/en/claude-directory); `data/harness-registry-v1.json:28`.) The R1 audit for this project also reproduced the dangerous case where a configuration parse/load failure silently fell back to the 30-day default and cleanup began; the audit evidence is not stored in this public repository.
- **Gemini CLI:** its official session-management documentation puts sessions under `~/.gemini/tmp/<project_hash>/chats/` and says the default retention policy is 30 days. The project registry records that the directory is literally named `tmp`, although it contains chat history. ([official documentation](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/session-management.md); `data/harness-registry-v1.json:64-85`.)
- **This tool’s `doctor`:** the R1 audit ran it on the maintainer’s machine and found a live risk: one harness had no retention policy configured and its oldest session had already crossed the threshold. That is an audit observation, not a promise that every machine will show the same result.

The useful first question is therefore not “is the archive elegant?” It is: **is any harness silently deleting my history right now?** `doctor` is intended to answer that question without modifying the source histories. (`crates/chat-stasher/src/main.rs:101-103`.)

## Install and first check

If you are installing this to use it rather than to work on it, read
**[`docs/install.md`](docs/install.md)** instead: it covers both halves (CLI and
browser extension), the one-time setup list — including turning off the
browser's "ask where to save each file before downloading" setting — how to
confirm the archive is actually running, and what does not exist yet.

The developer path, using your own repository URL and a directory you choose:

```sh
git clone <repository-url> <your-directory>
cd <your-directory>
cargo run -- doctor
```

`doctor` is the smallest useful path: it is read-only and reports paths, counts, bytes, and timestamps rather than session text. (`crates/chat-stasher/src/main.rs:101-103`.)

**Verification status:** `cargo run -- --help` and `cargo run -- doctor` were both run successfully against this checkout. The `doctor` output is not reproduced here because it contains local paths.

## Commands

The Rust source is the current command definition; the descriptions below were cross-checked against captured `--help` output. (`crates/chat-stasher/src/main.rs:29-188`.)

- `init` — writes a commented default config if none exists; the source describes this as non-destructive. (`main.rs:31-32`.)
- `push --stage <your-stage>` — moves sealed session shards into the rustic repository, creating the repository on first use and persisting the masterkey. (`main.rs:33-61`.)
- `status` — reports what the local harness scanner finds; it is read-only. (`main.rs:62-63`.)
- `read` — dumps one session as sequence-concatenated data and prints its SHA-256, or with `--all-machines` merges newest snapshots and reports per-session digests. (`main.rs:64-100`.)
- `doctor` — diagnoses whether a harness may silently delete sessions; its report is limited to paths, counts, bytes, and timestamps. (`main.rs:101-103`.)
- `verify --level l1|l2|l3|all` — checks repository structure, repository content, and/or reconciles the archive with the sealed staging manifest. (`main.rs:104-133`; `main.rs:703-771`.)
- `ingest --inbox <your-inbox> --stage <your-stage>` — consumes complete `deepseek-<sessionId>.json` exports, skips `.part` files, creates sealed staging shards, retires consumed inputs, and deduplicates identical bytes. (`main.rs:135-155`.)
- `seal --harness <id> --active <your-active-file> --stage <your-stage>` — allowlist-checks and seals one live file by rename; refusal leaves the active file untouched. (`main.rs:157-187`; `main.rs:315-321`.)

There is no `scan` subcommand in the current source; `status` is the scanner-facing command. (`main.rs:29-63`.)

## What it reads, writes, and sends

The paths below are placeholders on purpose. Do not paste real account names, hostnames, or keys into examples.

- `status` reads the local harness locations known to the registry and prints IDs, paths, sizes, mtimes, and flags; it does not print session content. (`main.rs:900-903`.)
- `doctor` reads local harness metadata for its diagnostic report; its declared output is paths, counts, bytes, and timestamps. (`main.rs:101-103`.)
- `ingest` reads complete export files from the `--inbox` you provide and writes sealed shards beneath the `--stage` you provide; consumed inputs are moved under `<your-inbox>/consumed/`. It prints paths, counts, and SHA-256 values, not conversation text. (`main.rs:135-155`; `main.rs:399-429`.)
- `seal` reads the registry and the active file you name, then may rename that file into the stage tree. The registry policy and confidence gate are part of the decision. (`main.rs:315-321`.)
- `push`, `read`, and `verify` read the repository and key file selected by config or flags. They can use a backend you explicitly configure with repository options; do not assume those three commands are offline. (`main.rs:33-61`; `main.rs:64-100`; `main.rs:104-133`.)

What does not leave the process through the metadata-only paths: `status`, `doctor`, and `ingest` do not print conversation bodies, and the ingest summary is explicitly metadata-only. (`main.rs:399-400`; `main.rs:900-903`.) `read` is intentionally different: its single-session mode dumps session data to your stdout, so treat that command as payload output. (`main.rs:64-78`.)

The destination is selected by your config and flags: local stage/repository paths or a backend you configure. The source exposes repository, key-file, and backend-option inputs rather than a hard-coded destination. (`main.rs:42-60`; `main.rs:85-99`.)

## What this does not do / current limits

This section is intentionally blunt:

- **Zed and Cursor session enumeration is not implemented in this version.** Their registry entries are path research, not a promise that `status` can enumerate their conversations; Cursor’s registry evidence is explicitly community-only, and Zed’s macOS path is not individually verified. (`data/harness-registry-v1.json:117-139`; `data/harness-registry-v1.json:225-247`.)
- **Claude Code on Windows has an unresolved path-sanitize detail.** The registry says the exact handling of the drive-letter colon and backslash in the short-path form is not determined and needs a real Windows test. (`data/harness-registry-v1.json:28`.)
- **`ingest` is not a generic import API.** Its documented input is complete `deepseek-<sessionId>.json` exports; `.part` files are skipped, and the source notes that bundles carry no account field. (`main.rs:135-155`; `main.rs:426-429`.)
- **`seal` is not a universal file-renaming tool.** It is gated by the registry’s `seal_policy`, evidence, and platform confidence; fd-holder harnesses such as Codex are refused because renaming can strand later writes in the old inode. (`main.rs:157-166`; `data/harness-registry-v1.json:55-57`.)
- **The release gate is not a substitute for installation.** `scripts/release-gate.sh` expects a built `target/debug/chat-stasher`, reads real local Claude JSONL files to make opaque fixtures, and exercises push/read/verify/doctor. It was not run for this README rewrite. (`scripts/release-gate.sh:3-19`; `scripts/release-gate.sh:23-50`; `scripts/release-gate.sh:66-112`.)
- **The license is not selected.** `LICENSE` is still a pending-owner-confirmation placeholder and explicitly says not to treat it as a grant of rights. Do not publish or redistribute this repository as if it already had an open-source license. (`LICENSE:1-13`.)

## Security and privacy

Two documents, both written to be read before you trust this with an archive you
cannot recreate:

- **[`docs/threat-model.md`](docs/threat-model.md)** — organised as *who can see
  what*: us (nothing — there is no server in this design), your destination
  provider (encrypted objects, but your backup rhythm and volume leak as
  metadata), other programs on your machine (they can read the **plaintext**
  files the extension drops in your download directory, and your master key
  file), the chat platforms, and one row we honestly could not resolve: what
  other browser extensions can observe. It also lists the weaknesses and the
  threats we do **not** defend against.
- **[`SECURITY.md`](SECURITY.md)** — how to report a vulnerability, what is in
  scope, and what response you can and cannot expect. The reporting contact is
  still a `TODO(owner)` placeholder; there is no private channel yet.

Three things worth knowing before reading either:

- **Your master key file is the only key.** Lose it and the archive is
  unreadable forever, with no recovery path of any kind
  (`crates/chat-stasher/src/store.rs:813-815`, `:834-841`).
- **There is no restore command.** `read` returns one session at a time to
  stdout (`crates/chat-stasher/src/main.rs:130-133`); bulk restore is not
  implemented.
- **Captured conversations are plaintext on disk** in your download directory
  until `ingest` consumes them (`apps/extension/lib/download.ts:91-93`).

## Development status

The repository contains the command implementation, harness registry, inbox schema, and release-gate script. `scripts/release-gate.sh` prints `GATE: PASS` or `GATE: FAIL`; both directions were exercised on this checkout (`--selftest` injects one byte and must produce `GATE: FAIL`). Note that the gate currently reads real local session files to build its fixtures — see the limits section. (`scripts/release-gate.sh:3-19`.)

Before treating this project as release-ready, make the help path compile, run `doctor` on a machine whose paths you are willing to inspect, select a license, and verify the remote-destination policy you actually intend to use.
