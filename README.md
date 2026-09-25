# chat-stasher

`chat-stasher` continuously archives conversation history from AI coding harnesses (Claude Code, Codex CLI, Gemini CLI, opencode and other registered sources) to storage that you control. It is an append-only archive. (Project description and append-only design: `crates/chat-stasher/src/main.rs:82`.)

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
| Grok CLI | 🟡 Registered; a real local store was read (5 sessions, ids and times matched); not yet archived end to end | - |
| Kimi Code | 🟡 Registered (macOS-measured); a real local store was read (3 sessions, ids and times matched); not yet archived end to end | - |
| GitHub Copilot CLI · aider · crush · Zed · Continue | 🟡 Registered, not verified | - |

**Web AI chats** (the browser extension)

| Platform | Live capture (conversations you open) | Backfill (past conversations) |
|---|---|---|
| ChatGPT | ✅ Verified (2026-09), including conversations over 8 MB | 🔄 In progress |
| DeepSeek | ✅ Verified (2026-09) | 🟡 Implemented (lists conversations and fetches their content); not yet verified in a real browser |
| Perplexity | 🟡 Implemented (2026-09-14), not verified | 🟡 Implemented (lists conversations and fetches their content); not yet verified in a real browser |
| Gemini | 🟡 Implemented; **not verified since the 2026-09-19 measurement that found it not capturing**, a page whose hook does not install now says so instead of staying silent | 🟡 Implemented, not verified |
| Grok | 🟡 Implemented, not verified | 🟡 Implemented, not verified |
| Kimi | 🟡 Implemented; **not verified since the 2026-09-19 measurement that found it not capturing**, a page whose hook does not install now says so instead of staying silent | 🟡 Implemented, not verified |
| Claude | 🟡 Coded, not verified | 🟡 Implemented, not verified |

🔴 **Gemini's and Kimi's live capture did not work on the one page each was
measured on, this table does not say otherwise, and neither has been re-measured
since.** On 2026-09-19, in a real Chromium with the extension loaded: on a
logged-in `gemini.google.com/app/<id>` tab, `window.fetch` and
`XMLHttpRequest.prototype.open` were both still the browser's own functions,
nothing of ours had run in that document; on a logged-in
`www.kimi.com/chat/<id>` page, a page-context
`POST /apiv2/kimi.gateway.chat.v1.ChatService/ListMessages` was answered **200**
with a top-level `{messages}` body (the shape the capture row declares) and
**not one capture was produced**. What can be ruled out for both: a page that
enforces Trusted Types is **not** why a hook is missing. The declarative
MAIN-world script runs before any page script, so no setting the page adopts can
precede it, measured against seven page shapes, `require-trusted-types-for
'script'` (by header and by `<meta>`), a `trusted-types` allowlist, a page-created
default policy and a `sandbox` policy among them, on every one of which the hook
installed and captured (`apps/extension/e2e/main-world-hook.spec.ts`). What is
**fixed**: a subframe of a supported origin was never injected into at all
(`allFrames` off), and a URL-less one (`about:blank`, `about:srcdoc`) stayed
uninjected even after that was turned on, so a request from such a frame was
answered **200** and produced no capture message and no archive row, silently
(`apps/extension/e2e/frame-capture.spec.ts`,
`apps/extension/e2e/hook-install.spec.ts`, and the origin rule such frames needed
in `apps/extension/lib/page-hook.ts`). What **cannot be fixed from inside a
page**: a document that already existed when the extension was loaded or updated,
or before access to that site was granted. Chrome does not re-inject into it,
doing so would take host permissions this extension does not request, so
**reloading the tab resolves it**, and nothing else does. What changed about that
last case is that it is no longer silent: a page whose hook did not install
reports it, one record per origin is kept in `storage.local` (`cs_hook_v1:*`) and
the popup prints it, with the action that clears it (`apps/extension/lib/hook-status.ts`,
`apps/extension/lib/popup-view.ts`). Until a tab is re-measured, treat Gemini and
Kimi live capture as **not working** on a tab that was open before the extension
was loaded or updated.

The popup shows **one summary line**, what the host counts in the stage (the
last 24 hours, the total, and the split by harness) and when the last successful
push was, and an **"Open dashboard"** button that asks the host to start
`chat-stasher ui` and opens the URL it answers with. Every part of that line is
either a number the host measured or the word *unknown* with the reason printed
underneath; a host that is missing, unreachable or older than the extension is
said to be exactly that, and the button is disabled with the reason rather than
offering a click that cannot work. The line is fetched once, when the popup
opens, there is no polling and no timer
(`apps/extension/lib/popup-view.ts`, `apps/extension/entrypoints/popup/main.ts`).

**Where the archive can go** (encrypted with `rustic`)

| Destination | Status |
|---|---|
| SFTP (e.g. Hetzner Storage Box) | ✅ Verified, in daily use on three machines (2026-09) |
| Local folder / disk | ✅ Supported |
| rustic REST server (`rest:`) | 🟡 Accepted by the config, not verified |
| S3 · Google Drive · others | 📋 Planned |


## Why this exists

Your harness may already be deleting history before you notice.

- **Claude Code:** its official documentation says files older than `cleanupPeriodDays` are deleted at startup, with a default of 30 days. The project registry records the same default for Claude Code. ([official documentation](https://code.claude.com/docs/en/claude-directory); `crates/chat-stasher/data/harness-registry-v1.json:28`.) The R1 audit for this project also reproduced the dangerous case where a configuration parse/load failure silently fell back to the 30-day default and cleanup began; the audit evidence is not stored in this public repository.
- **Gemini CLI:** its official session-management documentation puts sessions under `~/.gemini/tmp/<project_hash>/chats/` and says the default retention policy is 30 days. The project registry records that the directory is literally named `tmp`, although it contains chat history. ([official documentation](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/session-management.md); `crates/chat-stasher/data/harness-registry-v1.json:84-124`.)
- **This tool’s `doctor`:** the R1 audit ran it on the maintainer’s machine and found a live risk: one harness had no retention policy configured and its oldest session had already crossed the threshold. That is an audit observation, not a promise that every machine will show the same result.

The useful first question is therefore not “is the archive elegant?” It is: **is any harness silently deleting my history right now?** `doctor` is intended to answer that question without modifying the source histories. (`crates/chat-stasher/src/main.rs:398-399`.)

## Install and first check

If you are installing this to use it rather than to work on it, read
**[`docs/install.md`](docs/install.md)** instead: it covers both halves (CLI and
browser extension), the one-time setup list, including registering the Native
Messaging host with `chat-stasher install-native-host --stage <path>`, how to
confirm the archive is actually running, and what does not exist yet.

### Browser extension from a release (no store listing yet)

Each release from 0.4.0 on attaches `chat-stasher-extension-X.Y.Z.zip`, built for
the stable channel (ChatGPT, Claude, DeepSeek, Gemini, Grok). It is not in any
extension store yet; load it yourself:

1. Install the CLI from the same release and register the Native Messaging host:
   `chat-stasher install-native-host --stage <your-stage>`. Without it the
   extension has nowhere to deliver captures.
2. Download the zip from the release page and unzip it into a folder you will keep
   (the browser loads it from there; deleting the folder removes the extension).
3. Open `chrome://extensions` (or your Chromium browser's equivalent), turn on
   **Developer mode**, choose **Load unpacked**, and select the unzipped folder.
4. Reload any AI chat tab that was already open, so the page picks up the
   extension. Then open the extension's popup to check it can reach the host.
5. Past conversations are a separate switch. Turn **backfill** on in the popup,
   and know the one precondition before you do: **it needs an open, logged-in
   page of that platform to fetch through.** The extension requests no host
   permissions, so every backfill request is made inside one of your own open
   platform pages rather than by the extension itself. With no such page open
   the popup says archiving is not running for want of a fetch channel and
   fetches nothing, it is not stuck, and it carries on by itself as soon as you
   open one. It does not have to be the conversation you want: any open page of
   that platform answers.

To update, unzip the new release over the same folder and press the reload
button on the extension's card; reload open chat tabs again. Full details, and
building from source: [`docs/install.md`](docs/install.md).

The developer path, using your own repository URL and a directory you choose:

```sh
git clone <repository-url> <your-directory>
cd <your-directory>
cargo run -- doctor
```

`doctor` is the smallest useful path: it is read-only and reports paths, counts, bytes, and timestamps rather than session text. (`crates/chat-stasher/src/main.rs:398-399`.)

**Verification status:** `cargo run -- --help` and `cargo run -- doctor` were both run successfully against this checkout. The `doctor` output is not reproduced here because it contains local paths.

## Commands

The Rust source is the current command definition; the descriptions below were cross-checked against captured `--help` output. (`crates/chat-stasher/src/main.rs:144-1087`.)

- `init`: writes a commented default config if none exists; non-destructive. (`crates/chat-stasher/src/main.rs:149-150`.)
- `run-once`: collects one pass from local sources, pushes when configured and changed, then exits. (`crates/chat-stasher/src/main.rs:151-188`.)
- `schedule`: renders a launchd plist or systemd user service/timer template; `schedule install` and `schedule uninstall` idempotently manage macOS launchd agents. Use `--destination` once per configured destination; the embedded binary must be an installed path outside `target/`. (`crates/chat-stasher/src/main.rs:189-256`.)
- `push --stage <your-stage>`: moves sealed session shards into the rustic repository, creating the repository on first use and persisting the masterkey. (`crates/chat-stasher/src/main.rs:257-294`.)
- `status`: reports whether scheduled archiving is working and summarizes local scanner findings (read-only). (`crates/chat-stasher/src/main.rs:295-355`.)
- `read`: dumps one session as sequence-concatenated data and prints its SHA-256, or with `--all-machines` merges newest snapshots and reports per-session digests. (`crates/chat-stasher/src/main.rs:353-397`.)
- `doctor`: diagnoses whether a harness may silently delete sessions; report is limited to paths, counts, bytes, and timestamps. (`crates/chat-stasher/src/main.rs:397-408`.)
- `verify --level l1|l2|l3|all`: checks repository structure, repository content, and/or reconciles the archive with the sealed staging manifest. (`crates/chat-stasher/src/main.rs:410-448`.)
- `dest-init`: initialises a new destination as a full extra copy from local and existing destinations. (`crates/chat-stasher/src/main.rs:449-499`.)
- `search`: searches one destination's archive by session metadata: session-id prefix, machine, harness, and a **conversation-time** window. The window is given as local calendar days (`--day 2026-01-15`, or `--since`/`--until` for a range) and a session matches when its own activity interval intersects it, so `--day` finds a conversation held that day even if the machine was last pushed months later. A session whose conversation time is unknown is never dropped and never silently counted as a non-match: it is listed separately with the reason, and while a window is active the exit code is `3` rather than `1`, because "0 matched" is then unproven. A third, distinct state covers sessions archived with **no conversation content**, an empty shard, or only metadata lines such as a summary, with no user or assistant message: it is counted separately from time-unknown, never placed in a time bucket, and does **not** make the answer incomplete, because there is no conversation whose time could be missing. Between those two sits a **partial span**: a session whose records were read but not all of them placed in time (a `kimi-code` journal holding an operation the reader does not classify) keeps the bounds it did measure, and a window those bounds do not reach is still reported as could-not-be-placed rather than as a miss, because the unplaced record may be in it, while a window they do overlap is a real match. `--json` gives the matched, not-matched and could-not-be-placed groups as separate fields. `search` is also the dry run of `export`: both build their filter from one shared set of flags, and both select through the same decision, so the same flags select the same sessions. (`crates/chat-stasher/src/main.rs:500-561`; the shared filters are `crates/chat-stasher/src/selector.rs:1-340`; the no-conversation-content and partial-span states are `crates/chat-stasher/src/activity.rs:99-144` and `crates/chat-stasher/src/overview.rs:510-533`.)
- `export --out <dir>`: writes those sessions to files: `<out>/<machine>/<harness>/<session-id>.jsonl` holds each session's archived lines in their native format, byte-identical to what `read` returns for it, and `<out>/manifest.json` records per session its machine, harness, id, first and last message time, shard count, bytes written, sha256 of the written file and the filters that were applied, plus, at the top level, the sessions no filter could place, the machines whose activity index could not be read, and the exit status the run returns. It selects **exactly** the set `search` reports for the same flags, and prints the cost (sessions, shards, data blobs, plaintext bytes, the numbers `search --cost` reports) before fetching anything; `--dry-run` stops after that price and creates nothing. `--turns user` keeps only the lines that are the user's own messages, and only for harnesses whose format makes that certain (Claude Code: `type == "user"` and not a tool result); for any other harness every line is written and the session records `turns_filter: "not-supported"`, so the flag is never silently ignored and content is never silently dropped. With a time window, `--trim-to-window` also drops lines whose own timestamp is outside it; a line whose time cannot be read is kept and counted in the manifest's `untimed_lines`. `--out` must be empty or absent unless `--force` is given, nothing is ever deleted, and nothing is written outside `--out`. Exit codes match `search`: `0` wrote at least one session and answered every session the query touched, `1` read the whole destination and selected nothing, `3` could not finish (so the output on disk is real but incomplete, and the manifest says what is missing), `2` usage error. (`crates/chat-stasher/src/main.rs:562-642`.)

- `ui`: opens the archive dashboard in a browser: totals, a per-machine health row, a machine × source matrix of session counts, and a weekly activity heatmap, all on `127.0.0.1` with an OS-assigned port. Clicking a matrix cell or a heatmap week opens a session list filtered through the same selector `search` uses, so a drill-down and a `search` with the same flags return the same sessions. The page is rendered from archive *metadata* only. The list names each session by its recorded label (the harness's own title, else the head of the session's first user line, capped at 100 characters) and keeps the honesty that runs through the tool: a session with no label says `no label recorded`, a machine whose archive's activity index predates labels is explained once and its rows say `label unknown`, and the one label line per session is the only conversation-derived text the metadata read carries; a session's full text is fetched and decrypted only after you click through to it, and the byte cost is printed first. `ui` needs no flag when the choice is unambiguous: with exactly one `[destinations.<name>]` declared it opens that one, and with several it opens the one `[native_host] destination` names, or lists the declared names and exits `2` when no default is recorded (`search`, `export` and `overview` still require naming the copy, always). `view` is a deprecated alias for one release: it prints a one-line notice on stderr and behaves identically. (`crates/chat-stasher/src/main.rs:643-670`.) The extension popup's **"Open dashboard"** button starts this same command and opens the returned URL, see `native-host` below and `docs/threat-model.md`.
- `ingest --inbox <your-inbox> --stage <your-stage>`: consumes complete `deepseek-<sessionId>.json` bundles, skips `.part` files, also accepts the multi-bundle `*.jsonl` files the extension's "export undelivered captures" button produces, creates sealed staging shards, retires consumed inputs, and deduplicates identical bytes. (`crates/chat-stasher/src/main.rs:671-692`; `crates/chat-stasher/src/inbox.rs:56-60`.)
- `collect --stage <your-stage>`: reads every scanner session into staging shards without mutating harness sources. (`crates/chat-stasher/src/main.rs:693-728`.)
- `seal --harness <id> --active <your-active-file> --stage <your-stage>`: seals one file already inside `--stage` into the next sealed-shard slot; never renames a harness-owned path. (`crates/chat-stasher/src/main.rs:730-762`.)
- `install-native-host --stage <absolute path>`: writes the Native Messaging host manifest into each installed browser's discovery directory so the extension can hand conversations to this binary directly, and records the stage the browser-spawned host is allowed to write to as `[native_host] stage` in your config; the stage must already exist, because the host never creates one. `--uninstall` removes exactly those files and nothing else, and every path touched is printed. Per-user, no elevation. (`crates/chat-stasher/src/main.rs:832-874`.)
- `native-host`: the host loop itself: it reads one length-prefixed request frame from stdin, writes exactly one response frame to stdout, and exits 0. A frame that ends early (EOF inside the length prefix or the body) is answered with silence and a non-zero exit, never with a response, because a stray byte on stdout corrupts the frame. `--self-test` is the smaller check that the process starts at all: one line of JSON, exit 0. (`crates/chat-stasher/src/main.rs:875-896`, `:1888-1898`.) Besides `hello` and `deliver`, the host answers three **read-only** requests, all of which write nothing: `summary` returns how many sessions are in the stage (in total, in the last 24 hours, and split by harness) plus when the last successful push was, as counts and timestamps only, never conversation text, titles or session ids; `open_dashboard` starts `chat-stasher ui` for the destination named by `[native_host] destination` and returns its per-launch URL (including the access token) **to the calling extension only**, it is never logged and never written to disk; `has` answers whether the stage already holds one conversation's exact content: the extension names the bundle's platform and session id plus the SHA-256 `fingerprint` it derived from the capture body, and the host answers `held` — true or false — with the matching shard's file name, or `null`, looking only in the directory a `deliver` of that bundle would write to and taking no lock. `summary` and `open_dashboard` are parameterless, and for them a request carrying any field beyond `protocol` and `type` is refused with `nack` `bad-request`; `has` carries exactly `request_id`, `platform`, `session_id` and `fingerprint`, and a request with one of those missing or malformed is refused the same way. None can be triggered by anything but an extension whose id is in the host manifest. (`crates/chat-stasher/src/nativehost.rs`; the contract is `contracts/nativehost-protocol.md` §6.4–§6.6.)
- `cache`: reports this machine's conversation-body cache: where it lives, the quota it must stay under (`[cache] max_bytes`, default 2 GiB) and how much of it is in use; `cache clear` deletes every cached block. The cache holds each destination's own **ciphertext**, is disposable, losing it costs only speed, and is never consulted by `verify`, `export`, `dest-init`, `push` or `read --all-machines`, which read to prove the destination itself is intact. (`crates/chat-stasher/src/main.rs:1074-1088`; the config section is `crates/chat-stasher/src/config.rs:196-220`.)

There is no `scan` subcommand in the current source; `status` is the scanner-facing command. (`crates/chat-stasher/src/main.rs:295-355`.)

## What it reads, writes, and sends

The paths below are placeholders on purpose. Do not paste real account names, hostnames, or keys into examples.

- `status` reads the local harness locations known to the registry and prints IDs, paths, sizes, mtimes, and flags; it does not print session content. (`crates/chat-stasher/src/main.rs:7709-7919,8057-8348`.)
- `doctor` reads local harness metadata for its diagnostic report; its declared output is paths, counts, bytes, and timestamps. (`crates/chat-stasher/src/main.rs:397-398`.)
- `ingest` reads complete export files from the `--inbox` you provide and writes sealed shards beneath the `--stage` you provide; consumed inputs are moved under `<your-inbox>/consumed/`. It prints paths, counts, and SHA-256 values, not conversation text. (`crates/chat-stasher/src/main.rs:671-692`.)
- `seal` reads the registry and the active file you name, then may rename that file into the stage tree. The registry policy and confidence gate are part of the decision. (`crates/chat-stasher/src/main.rs:730-762`.)
- `push`, `read`, and `verify` read the repository and key file selected by config or flags. They can use a backend you explicitly configure with repository options; do not assume those three commands are offline. (`crates/chat-stasher/src/main.rs:257-294`; `crates/chat-stasher/src/main.rs:353-397`; `crates/chat-stasher/src/main.rs:410-448`.)
- The browser-spawned host answers the extension's `summary` request from the stage's **directory entries and shard mtimes only** plus the local `run-state.json`: it does not open a shard, does not decrypt the repository and does not touch the network. What goes back is counts, one harness name per bucket, a window length and a timestamp, nothing that identifies a conversation. (`crates/chat-stasher/src/nativehost.rs`.)

What does not leave the process through the metadata-only paths: `status`, `doctor`, and `ingest` do not print conversation bodies, and the ingest summary is explicitly metadata-only. (`crates/chat-stasher/src/main.rs:295-355,400-414,678-704,7709-7919,8057-8348`.) `read` is intentionally different: its single-session mode dumps session data to your stdout, so treat that command as payload output. (`crates/chat-stasher/src/main.rs:6028-6167`.)

The destination is selected by your config and flags: local stage/repository paths or a backend you configure. The source exposes repository, key-file, and backend-option inputs rather than a hard-coded destination. (`crates/chat-stasher/src/main.rs:151-188`; `crates/chat-stasher/src/main.rs:257-294`.)

## What this does not do / current limits

This section is intentionally blunt:

- **Zed and Cursor session enumeration is not implemented in this version.** Their registry entries are path research, not a promise that `status` can enumerate their conversations; Cursor’s registry evidence is explicitly community-only, and Zed’s macOS path is not individually verified. (`crates/chat-stasher/data/harness-registry-v1.json:362-397`; `crates/chat-stasher/data/harness-registry-v1.json:165-211`.)
- **Claude Code on Windows has an unresolved path-sanitize detail.** The registry says the exact handling of the drive-letter colon and backslash in the short-path form is not determined and needs a real Windows test. (`crates/chat-stasher/data/harness-registry-v1.json:28`.)
- **`ingest` is not a generic import API.** Its documented input is complete `deepseek-<sessionId>.json` exports; `.part` files are skipped, and the source notes that bundles carry no account field. (`crates/chat-stasher/src/main.rs:671-692`.)
- **The browser extension's history backfill can list conversations and fetch their content on all seven supported platforms, and "lists your conversations" is not the same as "saves them."** There used to be a third, middle tier (a platform whose list works but whose bodies are not fetched); W84 (2026-09-23) closed it, so "lists but does not save" has no platform left:
  - **Backfill implements listing conversations and fetching their content: ChatGPT, DeepSeek, Gemini, Grok, Kimi, Claude and, since W84, Perplexity.** (`apps/extension/lib/backfill/enumerate.ts:4590-4621`.) All seven are wired through to the local host, but **none has been observed completing a backfill in a real browser**, so read this as *implemented*, not *verified*. DeepSeek's conversation body is requested with `GET /api/v0/chat/history_messages?chat_session_id=<id>`, the route DeepSeek's own page calls over XHR when a user opens a past conversation, observed in a real logged-in browser session, and the same route several mutually independent open-source exporters request (`apps/extension/lib/backfill/enumerate.ts:2742-2756`, `:2815-2818`). 🔴 **Whether that route pages or truncates a long conversation is not settled by any source, so the extension does not assume an answer; it checks the body before storing it.** The response is a tree: `chat_session.current_message_id` names the newest message of the branch the user was looking at, and every message names its `parent_id`. The extension walks that chain and archives the body **only if the walk closes at a root**, so a response that came back short (whether it lost its newest messages or its oldest) is **not archived**: it is recorded as a failure with its own reason code and the leg carries on with the next conversation (`apps/extension/lib/backfill/enumerate.ts:2758-2785`). A body whose tree pointers are absent altogether is a different fact and halts the leg instead, rather than being called incomplete (`apps/extension/lib/backfill/enumerate.ts:2758-2785`).
    **Grok is the least verified of these seven, and it is a different kind of unverified.** Its routes were read out of public open-source implementations, **not** measured in a logged-in grok.com session (nobody has opened grok.com with this code) so the route shapes and field names are source-backed rather than observed (`apps/extension/lib/backfill/enumerate.ts:3116-3177`). One conversation costs **two** same-origin requests: a skeleton call that names the message ids, then a content call whose body is built only from those ids (`apps/extension/lib/backfill/enumerate.ts:3034-3036`). Two things in that pair are explicitly **unverified**, and both stop the leg rather than being guessed away: whether the list cursor the sources disagree about is the one the real backend honours (a page that repeats what was already listed halts as "the response shape changed" instead of being read as "nothing left" (`apps/extension/lib/backfill/engine.ts:1836-1916`)) and whether one content call returns a whole long conversation, which no source pages and no response field would reveal.
    **Kimi's routes *were* measured, and it needs your browser's own login token.** Its list and conversation-body endpoints, their request bodies and their response field names were observed in a logged-in www.kimi.com session (2026-09-14), not read out of someone else's source code (`apps/extension/lib/backfill/enumerate.ts:3201-3263`). That same session showed why the token matters: a request carrying only cookies is answered with **HTTP 401**. So the page's own bearer token is read from that origin's local storage at the moment of the request, kept in memory only, and attached to those two endpoints and to nothing else (`apps/extension/lib/platform-auth.ts:268-305`), and when there is no token the request goes out without one, so the platform's real refusal is what the leg sees. A refusal is never rounded into "you have no conversations". One conversation costs **one** body request. 🔴 **Unverified: whether a long conversation's body response pages.** Five short conversations were sampled and none carried a page-token field; if one ever does, that conversation is **not** archived, it is recorded as a failure with its own reason code and the leg carries on with the next one (`apps/extension/lib/backfill/engine.ts:2617-2630`), because a truncated conversation stored as a complete one is exactly the silent loss this project refuses.
  - **Backfill lists conversations and fetches their content on all seven platforms now. Perplexity's body segment was filled in last, from a live probe.** Perplexity's list endpoint has cross-checked open-source provenance (`apps/extension/lib/backfill/enumerate.ts:3004-3023`), and its *single-conversation* route is `GET /rest/thread/<slug>` with a five-parameter set pinned by the plan (`apps/extension/lib/backfill/enumerate.ts:2919-2930`). 🔴 The reason the body leg was previously refused, and is now written, is the difference between guessing and observing: a 2026-09-23 logged-in probe of one thread returned a stated completeness signal at the top level (`has_next_page` (boolean) and `next_cursor` (string or null)) present under both the schematized and the minimal parameter set, and an `offset=500` request returned a byte-identical body. So the extension **does not assume a single response holds a whole conversation**: a body is archived only when the response *declares* there is no more (`has_next_page:false` and `next_cursor:null`); a response that declares more is **not archived**, it is recorded with its own reason code and the leg carries on with the next conversation (`apps/extension/lib/backfill/enumerate.ts:2267-2315`); a body with no `entries` is the unverified-empty case, never a confirmed receipt; and a body with no signal at all halts the leg `shape-changed` rather than being read as complete. 🔴 **Unverified, honestly: the observed thread had one entry.** Whether a genuinely long thread answers `has_next_page:true` / a non-null `next_cursor` when it truncates was not directly observed (the live probe budget), and the exact pinned five-key query was not itself transmitted. The completeness rule refuses when the signal says more and treats the response as whole when it says no more (the honest direction of the uncertainty) but no complete Perplexity backfill has been observed in a real browser yet, so read the backfill row as *implemented, not verified*.
    **Gemini's routes *were* measured too, and it is the first platform whose conversation body arrives in pages.** Its two RPCs (`MaZiqc` for the list, `hNvQHb` for one conversation), their response payload positions and the page's own token source were observed in a logged-in gemini.google.com session (2026-09-14), and the request-envelope parser was written and tested before any of it was wired up (`apps/extension/lib/backfill/enumerate.ts:3640-3768`). Two things follow from that measurement, both stated because they change what the backfill costs: the request body is a **URL-encoded form** whose arguments rest on a single source while the responses were measured, and a long conversation costs **as many requests as it has pages**, the leg follows the continuation token to the end, spaced 1-3 seconds apart, and a conversation needing more than 20 pages is **refused and listed as a failure** rather than archived in part (`apps/extension/lib/backfill/engine.ts:2388-2461`). The three tokens its requests carry come from the page's own `WIZ_global_data`, read at request time through the page-world hook, held in memory only, and attached to those two RPCs and nothing else (`apps/extension/lib/platform-auth.ts:679-779`). Unlike every platform above, **Gemini's passive capture sends requests of its own**: it fetches the conversation through the same allowlisted channel, starting at the first page and following the paging token to the end, so opening a conversation costs **one request for its first page plus one per remaining page** (`apps/extension/lib/gemini-capture.ts:150-234`). That first request is a deliberate repeat of the one the page just made: the observed response may be *any* page of the conversation, since the page asks for older turns as you scroll, and a bundle anchored anywhere but page 1 could hold only the oldest turns while satisfying every completeness check. Nothing is stored when any of that fails, a partial conversation is never filed as a whole one.
    **Claude's routes are source-backed, not measured, and it is the one platform whose requests need an identifier the page URL does not carry.** Nobody has opened claude.ai with this code, so every route and field name below comes from reading public open-source implementations rather than from a logged-in session (`apps/extension/lib/backfill/enumerate.ts:3771-3777`). Every request is addressed by **organization**, the value is in no page URL, and an account may belong to several, so the extension resolves it from evidence, in a fixed order, and stops rather than choosing: the page's own already-seen requests first, the `lastActiveOrg` cookie second, and one `GET /api/organizations` third; several organizations with neither of the first two naming one halts as "the organization could not be settled" **before a single list request goes out**, and the organizations are never iterated (`apps/extension/lib/backfill/claude-org.ts:211-265`). The resolution runs **in the claude.ai page** over the channel backfill already uses, and only when the organization is actually needed, when you press the popup's start button for that platform, and on a wake-up whose recorded scope is not an organization yet; a page that is merely open is asked nothing (`apps/extension/lib/backfill/claude-page.ts:70-148`; `apps/extension/lib/backfill/tab-port.ts:1133-1150`). 🔴 **The organization a backfill is started with is the one it keeps**: switching organizations on claude.ai does not move a run that is already going, and a backfill for a second organization starts by opening a conversation in it (`apps/extension/entrypoints/background.ts:1503-1584`). The conversation list is offset-paged with no termination field, so a short page is treated as an inference rather than an ending, and the page that repeats what was already listed halts instead of being read as "nothing left" (`apps/extension/lib/backfill/engine.ts:1749-1765`). 🔴 **Unverified: whether one response holds a whole long conversation.** The body comes back as a tree whose active branch is walked from its newest message back to a root; if that walk reaches a message the response does not carry, the conversation is **not archived**, it is recorded as a failure with its own reason code and the leg carries on, and both spellings of the parent link that the sources disagree about are accepted (`apps/extension/lib/backfill/enumerate.ts:3991-4066`). Nothing is stored when any of that fails, and the scope that addresses every request never appears in a URL the extension did not itself build (`apps/extension/lib/backfill/tab-port.ts:464-484`).
  The extension's popup states the same three tiers in the same terms (`apps/extension/lib/popup-view.ts:938-951`). This limit is about **backfill of past conversations**; passive capture of the conversation currently open in your browser is a separate leg with its own per-platform table (`apps/extension/lib/contract.ts:315-768`).

- **`seal` is not a universal file-renaming tool.** It is gated by the registry’s `seal_policy`, evidence, and platform confidence; fd-holder harnesses such as Codex are refused because renaming can strand later writes in the old inode. (`crates/chat-stasher/src/main.rs:730-762`; `crates/chat-stasher/data/harness-registry-v1.json:71-72`.)
- **The release gate is not a substitute for installation.** `scripts/release-gate.sh` builds `target/debug/chat-stasher` if it is missing and generates its own synthetic opaque fixtures, so a contributor can run it with no arguments and no setup (`--real-data` opts into local Claude sessions instead). It exercises push/read/verify/doctor. (`scripts/release-gate.sh:3-21`.)
- **License.** The project declares the Apache License 2.0 (`crates/chat-stasher/Cargo.toml:6`; [license text](LICENSE)).

## Security and privacy

Two documents, both written to be read before you trust this with an archive you
cannot recreate:

- **[`docs/threat-model.md`](docs/threat-model.md)**, organised as *who can see
  what*: us (nothing, there is no server in this design), your destination
  provider (encrypted objects, but your backup rhythm and volume leak as
  metadata), other programs on your machine (they can read the **plaintext**
  captures waiting in the extension's outbox, the staged shards, and your master
  key file), the chat platforms, and one row we honestly could not resolve: what
  other browser extensions can observe. It also lists the weaknesses and the
  threats we do **not** defend against.
- **[`docs/privacy.md`](docs/privacy.md)**, the store-listing privacy policy:
  what is collected (nothing, and how you can verify that yourself), where data
  is stored, who it is shared with, how long it is kept, how to delete it, and
  the known plaintext window. Written for outside readers and store reviewers.
- **[`SECURITY.md`](SECURITY.md)**, how to report a vulnerability, what is in
  scope, and what response you can and cannot expect. Reports go to
  `work@team.iopho.com` rather than a public issue.

Three things worth knowing before reading either:

- **Your master key file is the only key.** Lose it and the archive is
  unreadable forever, with no recovery path of any kind
  (`crates/chat-stasher/src/store.rs:1271-1278`, `:1229-1233`).
- **There is no restore command.** `read` returns one session at a time to
  stdout (`crates/chat-stasher/src/main.rs:353-355,6028-6167`) and `export`
  writes many to files under `--out` (`crates/chat-stasher/src/main.rs:562-642`);
  both are payload-output commands. Getting sessions *back into* a harness's own
  directories is not implemented, by either of them or by anything else.
- **Captured conversations are plaintext until they are delivered.** A live
  capture is written into the extension's own IndexedDB outbox *before* any
  delivery is attempted, and is removed only when the host answers with a
  matching `ack` (`apps/extension/lib/outbox.ts:309-377`, `:379-394`;
  `apps/extension/entrypoints/background.ts:290-307`). A delivery the host never
  confirms stays there, and the popup's export file contains the same bodies.

## Development status
 
The repository contains the command implementation, harness registry, inbox schema, and release-gate script. `scripts/release-gate.sh` prints `GATE: PASS` or `GATE: FAIL`; both directions were exercised on this checkout (`--selftest` injects one byte and must produce `GATE: FAIL`).

Before using this project in production, run `doctor` on a machine whose paths you are willing to inspect, and verify the remote-destination policy you actually intend to use.
