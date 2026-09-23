# Installation guide

This document is written for **people who will use it**, not for developers.
You do not need to understand Rust or TypeScript, but you do need to be able to
open a terminal window and copy-paste a few commands.

Convention throughout the document: every claim about "what the software will
do" is followed by a `file:line` reference you can check in the repository
yourself. Anything we have **not verified by hand** is marked "unverified" —
"unverified" does not mean "does not exist", it means we have not tested it.

---

## 1. What you are installing: two things

It is not one app, it is **two pieces**, each doing its own job:

| Part | What it does | Where it lives |
| --- | --- | --- |
| **CLI (command-line program `chat-stasher`)** | Scans the session records left behind by various AI coding tools on your machine and collects them into an append-only encrypted archive | Your computer, run from the terminal |
| **Browser extension (Chat Stasher)** | Saves your conversations from **web-based** chats and hands them straight to the CLI over Native Messaging | Your browser |

**On the CLI side:** its self-description is "Append-only archive for every LLM
conversation, across harnesses." (`crates/chat-stasher/src/main.rs:81`). It
reads session files that already exist on your machine, and reads them
read-only (`crates/chat-stasher/src/main.rs:653`).

**On the extension side:** it currently recognizes **seven** web platforms —
DeepSeek (`chat.deepseek.com`), Perplexity (`www.perplexity.ai`), ChatGPT
(`chatgpt.com` / `chat.openai.com`), Gemini (`gemini.google.com`), Claude
(`claude.ai`), Kimi (`www.kimi.com`), Grok (`grok.com`)
(`apps/extension/lib/contract.ts:319,387,403,444,502,629,716`).

🔴 **Recognizing a platform is not the same as capturing on it, and for two of
the seven it measurably was not.** On 2026-09-19, in a real browser with the
extension loaded, a logged-in `gemini.google.com/app/<id>` tab still had the
browser's own `window.fetch` and `XMLHttpRequest.prototype.open` (nothing of
ours had run in that document), and a logged-in `www.kimi.com/chat/<id>` page
made a `POST /apiv2/kimi.gateway.chat.v1.ChatService/ListMessages` that was
answered **200** with a top-level `{messages}` body — the shape the capture row
declares — and produced **no capture at all**. One cause is fixed on this
branch: a same-origin **subframe** of a supported origin was never injected into
(`allFrames` was off), so a request made from one was invisible to the hook
(`apps/extension/entrypoints/dw-fetch-main.content.ts`;
`e2e/frame-capture.spec.ts` reproduces the reading above and passes only with
that fixed). The other — a document that already existed when the extension was
loaded, which Chrome does not re-inject into without host permissions this
extension deliberately does not request — is not fixable from inside the page:
**reloading the tab is what resolves it.** Which of the two a given tab is
cannot be told from here, so until one is ruled out, read Gemini and Kimi live
capture as **not working on a tab that predates the extension's load or
update**, and the cause as still under investigation.

It requests four permissions — `nativeMessaging`, `storage`, `alarms` and
`unlimitedStorage` — and **no host permissions at all**
(`apps/extension/wxt.config.ts:113`). There is no `downloads` permission and no
automatic download anywhere.

**How the two sides connect:** the extension sends each captured conversation to
a **Native Messaging host**, which is the `chat-stasher` binary you registered
by hand with `chat-stasher install-native-host --stage <your-stage>`
(`crates/chat-stasher/src/main.rs:789-831`). The protocol both sides implement
is written down in [`contracts/nativehost-protocol.md`](../contracts/nativehost-protocol.md).

🔴 **A conversation counts as delivered only when the host answers an `ack`
whose `request_id` and `sha256` equal the ones the extension sent**
(`apps/extension/lib/native-host.ts:775-784`). Everything else — a `nack`, a
timeout, a disconnect — is *not delivered*, and the capture stays in the
extension's own outbox until a matching `ack` deletes it
(`apps/extension/lib/outbox.ts:379-394`). There is no "probably delivered".

If you would rather not register the host at all, the extension can instead
export everything it has not delivered as one file, which you feed to the CLI by
hand: `chat-stasher ingest --inbox <directory> --stage <your-stage>`. Section 3.3
covers that.

🔴 **"Recognizing the platform" does not mean "it can recover your history on
that platform."** The extension has two legs; please read them separately:

- **Passive capture** (on by default): the conversation you are currently
  viewing is saved as a side effect when the page fetches its own data. Each
  platform registers in that table which route, method, and response shape
  count (`apps/extension/lib/contract.ts:272-718`).
  🔴 **Perplexity used to be the exception here; read where it stands now:**
  its row registers the **conversation-content** route — path hint
  `/rest/thread/`, method `GET`, response shape requiring `entries`
  (`apps/extension/lib/contract.ts:322-388`) — and it recognizes the session id
  from the page URL, the `/search/<slug>` the thread is open at
  (`apps/extension/lib/contract.ts:375-378`). The **conversation-list** route is
  deliberately outside the row: a list is a summary of conversations, not one of
  them, so it is skipped silently rather than captured.
  So, reading the code, passive capture on Perplexity **does name the
  conversation you are viewing and delivers it**
  (`apps/extension/lib/contract.ts:951-979`) — but this is still a conclusion
  drawn from reading the code, and the route itself was read out of public
  source rather than measured: **we have not tested it on a real perplexity.ai
  page.**
- **History backfill** (off by default; see section 6): digs up your **past**
  conversations and saves them. This leg's **capability differs per platform**,
  spelled out in section 1.1 below.

### 1.1 🔴 History backfill: three tiers, not a "supported / unsupported" binary

The list below comes directly from the two tables in the code, not from
marketing (`apps/extension/lib/backfill/enumerate.ts:4220-4247`):

| Tier | Platforms | What you actually get when you enable backfill |
| --- | --- | --- |
| **Implements fetching the actual history text** | **ChatGPT**, **DeepSeek**, **Gemini**, **Grok**, **Kimi**, **Claude** | Conversations are listed one by one, and their content is fetched one by one and delivered to the host. This tier is the one that means "your history is backed up" — but read it as *implemented*, not *verified*: a complete backfill has not yet been observed in a real browser on any of the six. DeepSeek's body request is `GET /api/v0/chat/history_messages?chat_session_id=<id>` (`apps/extension/lib/backfill/enumerate.ts:2617-2620`). 🔴 Grok and Claude are the least verified of the six: their routes came from reading public open-source implementations, not from a logged-in session, and Grok fetches one conversation with **two** same-origin requests — a skeleton call, then a content call whose body is built only from the ids the skeleton named (`apps/extension/lib/backfill/enumerate.ts:2787-2789`, `:2869-2930`). Kimi's routes, unlike Grok's, **were** measured in a logged-in www.kimi.com session — and its requests carry your page's own login token, read from the page's local storage at request time and held in memory only. If a conversation's body response ever says it holds only part of that conversation, Kimi refuses to archive it and lists it as a failure instead (`apps/extension/lib/backfill/enumerate.ts:2954-3016`; `apps/extension/lib/platform-auth.ts:216-248`; `apps/extension/lib/backfill/engine.ts:2010-2045`). Gemini's routes **were** measured as well (2026-09-14), and it is the one platform here whose conversation body arrives **in pages**: the leg follows the continuation token to the end, one request per page with a 1-3 second gap, and a conversation needing more than 20 pages is refused and listed as a failure rather than archived in part (`apps/extension/lib/backfill/enumerate.ts:3393-3521`; `apps/extension/lib/backfill/engine.ts:1888-1940`). Its requests carry three values from the page's own `WIZ_global_data` — read through the page-world hook at request time, memory only, attached to its two RPCs and nothing else (`apps/extension/lib/platform-auth.ts:577-639`). |
| **🔴 Can only list conversations, saves none of their content** | **Perplexity** | The extension can list which historical conversations you have, but **will not fetch each conversation's content**, so **not one of them is delivered or queued**. Your Perplexity history is **not backed up**. |


🔴 **The middle tier is the easiest to misunderstand, so say it again**:
Perplexity, with backfill enabled, the extension **does act** (it lists
conversations), and the popup shows "already listed N, waiting". **But not one of
those N is saved.** If you now close your browser, format your disk, or the
platform deletes your history, those N conversations are gone — the extension
holds only their ids, not their content.

🔴 **The DeepSeek caveat, which exists for the same reason.** DeepSeek fetches
bodies, and whether its body endpoint pages or truncates a long conversation is
**still not settled by any source** — but the extension no longer assumes an
answer. The response carries a tree:
`chat_session.current_message_id` names the newest message of the branch you were
looking at, and every message names its `parent_id`. The extension walks that
chain and archives the body **only when the walk closes at a root**; a response
that came back short is **not archived** — it is recorded as a failure with its
own reason code and the leg carries on, rather than being stored as a complete
conversation (`apps/extension/lib/backfill/enumerate.ts:2560-2587`). The evidence
for the endpoint itself is solid — it is the route DeepSeek's own page calls over
XHR when a user opens a past conversation in a real logged-in session, and several
mutually independent open-source exporters request the same route
(`apps/extension/lib/backfill/enumerate.ts:2544-2558`). What the check cannot
prove is written down too: it shows the response is closed under the branch you
were looking at, not under every discarded sibling branch, and it cannot catch a
server that truncates a body *and* rewrites the boundary message's `parent_id` to
`null` so the chain looks rooted. That is the residual, and it is why the
completeness question is still called open rather than answered.

🔴 **Perplexity gets one more sentence, and its two legs now point in opposite
directions:** passive capture names and delivers the conversation you have open
(the note above), while **backfill still archives nothing** — it lists your past
conversations and saves none of their content. So "Perplexity is supported" is
true of the conversation in front of you and false of your history. Read the
table row above as being about **backfill**, which is what it is about.

The reason is written in the code, not because we are lazy: Perplexity's
**conversation-list endpoint** has multiple independent open-source
implementations that cross-check one another
(`apps/extension/lib/backfill/enumerate.ts:2763-2777`), but this plan's
**single-conversation segment is left unfilled** — and it is left unfilled on
purpose, not for want of a route. The content route itself is known
(`apps/extension/lib/contract.ts:340-355`), but the parameters the sources give
for it disagree with one another and nothing establishes whether one response
holds a whole long conversation, so `detailPath` stays `null`
(`apps/extension/lib/backfill/enumerate.ts:2751-2760`, `:2763-2777`). We will not guess a
content-endpoint profile — a wrong guess would not error; it would save only the
first few turns of every conversation while you believed you had it all.

The popup shows these three tiers in the same terms as the table above
(`apps/extension/lib/popup-view.ts:770-783`).

(**Passive capture is not affected by this table:** the passive-capture criteria
for the seven platforms above are each registered in the table at
`apps/extension/lib/contract.ts:272-718`, a separate matter from backfill.)

---

## 2. Install the CLI

There is no precompiled package in the repository, and no one-command channel
like `brew install` — you need to compile from source once.

```sh
git clone <repository-url> <your-directory>
cd <your-directory>
cargo build --release
```

- You need the Rust toolchain (`cargo`). **The repository does not declare a
  minimum Rust version:** neither `Cargo.toml` nor
  `crates/chat-stasher/Cargo.toml` has a `rust-version` field
  (`Cargo.toml:1-11`, `crates/chat-stasher/Cargo.toml:1-6`). Which exact
  version compiles — **unverified**.
- The build output is at `target/release/chat-stasher`.

Then write a config:

```sh
chat-stasher init
```

`init` writes a commented default config only when the config does **not**
already exist; it is non-destructive (`crates/chat-stasher/src/main.rs:135-136`).
The config file lives at `~/.config/chat-stasher/config.toml`, or under
`XDG_CONFIG_HOME` if you have set it (`crates/chat-stasher/src/config.rs:15,500-511`).

---

## 3. Install the browser extension

**It is not yet on any app store** (see section 6 for details). For now you can
only install it manually:

```sh
cd apps/extension
pnpm install
pnpm build            # Chrome/Edge and other Chromium-based browsers
pnpm build:firefox    # Firefox
```

(The script names come from `apps/extension/package.json:10-11`. You need Node
and pnpm; **the exact minimum versions are not declared in the repository —
unverified**.)

The build output lands in `apps/extension/.output/` (that directory is excluded
by `.gitignore`, `.gitignore:15`). Load that directory into your browser with
its "Load unpacked extension" menu — we have not tested each browser's menu
path, and section 8 marks them "unverified".

### 3.1 Register the Native Messaging host

The extension on its own can capture conversations but cannot archive them: it
has to hand each one to the `chat-stasher` binary, and the browser only allows
that for a host the browser has been told about. One command does both halves of
that registration — it records the stage in your config and writes the host
manifest into each installed browser's discovery directory:

```sh
chat-stasher install-native-host --stage <your-stage>
```

`--stage` must be an **absolute path to a directory that already exists**: the
host never creates a stage, because a stage that appears because a host was
pointed at it is a stage nothing pushes
(`crates/chat-stasher/src/nativehost.rs:963-974`). The stage is the same staging
directory you use for `collect` / `seal` / `ingest`.

The command is idempotent — run it twice and there is exactly one manifest per
browser, byte-identical, exit 0 both times — and it prints every path it wrote,
left alone, skipped or removed, absolutely (`crates/chat-stasher/src/main.rs:766-788`).
It is per-user; nothing needs elevation. `--uninstall` removes exactly the files
it wrote and nothing else.

**What the browser asks you at install time.** Registering the host does not
remove any browser prompt, but it changes which one you see. The extension
declares `nativeMessaging`, so Chrome shows *"communicate with cooperating
native applications"* on its details page. It no longer declares `downloads`, so
the *"Manage your downloads"* warning is gone
(`apps/extension/wxt.config.ts:113`).

### 3.2 Confirm the popup says "connected"

Click the extension's toolbar icon. The popup asks the host one `hello` question
and renders the answer — **the stage it writes to, the machine id, and the host
version** — or the reason it could not, with the command that fixes it
(`apps/extension/lib/ui-strings.ts:80-100`;
`apps/extension/entrypoints/background.ts:437-444`).

If it does **not** say connected, the popup prints the named reason (the host's
own `nack` kind, e.g. `config` or `stage-unavailable`), the stage it last knew
about, and the fix command with that path already filled in
(`apps/extension/lib/ui-strings.ts:34-36`, `:89-100`). Nothing is delivered
while this is the case: captures wait in the extension's outbox instead, and the
toolbar badge shows how many (`apps/extension/lib/badge.ts:46-73`).

### 3.3 If you never register the host: the export file

The popup has an **"export undelivered captures"** button. It appears only when
something has not been delivered, and it writes one file named
`chat-stasher-export-<UTC yyyymmddThhmmssZ>.jsonl` into your download directory —
one line per undelivered capture, each line being exactly the payload that would
have been sent to the host (`apps/extension/lib/outbox.ts:439-475`).

Feed that directory to the CLI:

```sh
chat-stasher ingest --inbox <directory-holding-the-export> --stage <your-stage>
```

`ingest` accepts `*.jsonl` export files next to `*.json` bundles, and treats
each line as one bundle content-addressed by the SHA-256 of the line without its
trailing newline — the same key the host would have used, so a line that was in
fact delivered is recognised as a duplicate rather than archived twice
(`crates/chat-stasher/src/inbox.rs:59-60`;
`contracts/nativehost-protocol.md` §8). Exporting does not remove anything from
the outbox (`apps/extension/lib/outbox.ts:457-464`).

---

## 4. One-time setup checklist

The following things, you do **once at install time and then never again**.

### 4.1 🔴 Decide the stage directory, and keep it

The `--stage` you gave `install-native-host` (section 3.1) is the same directory
`collect`, `seal` and `ingest` write sealed shards into. It is a real directory
on your disk, and it must exist *before* you point the host at it: the host
never creates a stage, and a stage that appears because a host was pointed at it
is a stage nothing pushes (`crates/chat-stasher/src/nativehost.rs:963-974`).

Two properties of that directory, both from
[`contracts/nativehost-protocol.md`](../contracts/nativehost-protocol.md):

- **The host and `ingest` take an exclusive lock on `<stage>/.ingest.lock`
  before they allocate a shard sequence number**, so two browsers, two profiles,
  or a host racing a manual `ingest` cannot pick the same number. The wait is
  bounded at 10 seconds, and a timeout comes back as a `stage-unavailable` the
  extension retries (`crates/chat-stasher/src/inbox.rs:66-68`, `:853-881`).
- **A stage the host cannot use is reported, not replaced.** A missing or
  relative `[native_host] stage` is a `config` refusal, and a path that is not a
  directory is `stage-unavailable` (`crates/chat-stasher/src/nativehost.rs:915-975`);
  if the seal itself fails, a lock-wait timeout is `stage-unavailable` and any
  other write error is `io`, and neither acknowledges anything
  (`crates/chat-stasher/src/nativehost.rs:1162-1166`). In every case the reason
  names the fix.

Put it somewhere you will not delete: these shards are the archive's input, and
`push` is what moves them into the encrypted repository.

**The host never invents a machine identity either.** It resolves the machine id
exactly as `ingest` does, and if there is none it refuses with a `config` `nack`
that names the fix, rather than minting a second identity — which would silently
put every delivered shard in a different machine's archive partition
(`crates/chat-stasher/src/nativehost.rs:980-1009`). Run any archiving command
once from your shell before registering the host.

### 4.2 Run `chat-stasher init` once

See section 2. If you already did it, you do not need to do it again.

### 4.3 Decide where the archive lives, and **back up your master key file**

The archive's destination is decided by your config and command-line arguments
— a local path, or a backend you configure yourself. `push` / `read` / `verify`
read the repository and key file you select in config or arguments
(`crates/chat-stasher/src/main.rs:251-256,340-345,391-396`).

🔴 **The master key file is the only key. Lose it and the archive can never be
read again; there is no way to recover it.** The source's own words are "The
masterkey is the repository's only key — losing it means the repo is unreadable
forever" (`crates/chat-stasher/src/store.rs:1189-1191`). The key file is written
with owner-only-readable permissions, on platforms that can express them
(`crates/chat-stasher/src/store.rs:1289-1297`).

**Make a copy of it somewhere else right now.** No one can do this for you.

### 4.4 🔴 A remote destination: the first connection needs a human

Skip this if your archive lives on a local path. It applies when `repo` names a
remote backend such as `opendal:sftp` — the options you write under
`[destinations.<name>.options]` are forwarded verbatim to the backend
(`crates/chat-stasher/src/store.rs:153-156`, `:271-275`; the config field itself
is `crates/chat-stasher/src/config.rs:175-176`).

**Why this step exists.** A remote destination is reached by running the system
`ssh` client. The first time it meets a host it has no record of, it refuses:
that host's key is not in `~/.ssh/known_hosts`, so the key the server just
presented has nothing to be compared against. **That refusal is the feature.**
It is the one moment at which "is this really my storage box?" can be answered
by you rather than by whoever is on the network path.

**This tool never answers it for you.** `--trust-host` is the only thing in the
program that writes to `known_hosts`
(`crates/chat-stasher/src/main.rs:3167-3180`); without it, an unattended
scheduled run that meets a new host stops instead of quietly trusting it.

**What you see when it happens.** `dest-init` connects once, read-only, before
it does anything else (`crates/chat-stasher/src/main.rs:3203-3227`). An
untrusted host stops the command there with exit code `3` — "did not finish
reading", which is *not* the same as "the destination is empty" — and prints
which host is untrusted, the fingerprints it received, and the next step
(`crates/chat-stasher/src/remote_err.rs:180-187`, `:454-483`).

**Step 1: check the fingerprint out of band.** See the key the network hands
out, without logging in:

```sh
ssh-keyscan -p <your-port> <your-host>
```

🔴 **What that output is worth — read this before using it.** OpenSSH's own
manual says: "ssh-keyscan cannot verify the authenticity of the host keys it
obtains", and that a network attacker can substitute their own key, so its
output "should be verified out of band"
(<https://man.openbsd.org/ssh-keyscan>). So compare the fingerprint it printed
against the one your provider publishes — Hetzner, for example, lists them in
the Storage Box overview, and its SFTP/SCP guide says comparing your connection's
fingerprint with those "confirms the authenticity of the connection"
(<https://docs.hetzner.com/storage/storage-box/access/access-sftp-scp/>). **If
they do not match, stop here** and do not go on to step 2.

**Step 2: record it, once, with `--trust-host`.** Only after the fingerprints
match:

```sh
chat-stasher dest-init --destination <name> --stage <your-stage> --trust-host
```

It prints the fingerprints it found and each record it writes, then appends them
to `~/.ssh/known_hosts` (`crates/chat-stasher/src/main.rs:3182-3191`;
`crates/chat-stasher/src/remote_err.rs:503-536`). The flag is for remote
destinations only: on a local path it is refused with exit code `2` rather than
silently doing nothing (`crates/chat-stasher/src/main.rs:3170-3178`).

🔴 **Never do this for a host whose key has *changed*.** If a host you already
trusted now presents a different key, OpenSSH prints `REMOTE HOST IDENTIFICATION
HAS CHANGED`, and that can mean someone is impersonating your destination. The
program classifies that case separately from "a host I have never seen" and
refuses it; it is never accepted as a new host, and none of the options below
should be used to push past it
(`crates/chat-stasher/src/remote_err.rs:101-112`). Find out why the key changed
before editing `known_hosts`.

**Optional: `known_hosts_strategy`.** A destination's options table also accepts
`known_hosts_strategy`, alongside `endpoint`, `user`, `key` and `root`:

```toml
[destinations.storagebox.options]
endpoint = "ssh://<your-host>:<your-port>"
user = "<your-user>"
key = "~/.ssh/id_ed25519"
known_hosts_strategy = "strict"
```

What the pinned backend does with the three accepted values — read from its own
source (opendal-service-sftp 0.57.0, `src/backend.rs` lines 148-165, which maps
onto the `openssh` crate's `KnownHosts`): leaving the option out means `strict`;
`add` also accepts a host that is not known yet and records it
(`StrictHostKeyChecking=accept-new`); `accept` takes whatever key the server
presents (`StrictHostKeyChecking=no`), which includes a changed key. **This
project sets none of this for you and does not change the default** — omitting
the option is `strict`, which is the behaviour described above. `add` and
`accept` move the trust decision away from you; choose them deliberately if you
choose them at all, and note that `accept` weakens exactly the case step 2's
warning is about.

### 4.5 Install a timer (optional, but this is the key to "install once and forget it")

`chat-stasher schedule` **renders** a launchd plist or systemd user
service/timer — note its own words are "never installs it", i.e. it only
generates files, **it does not install them for you**
(`crates/chat-stasher/src/main.rs:175`). The generated template wraps a
`run-once` command (`crates/chat-stasher/src/main.rs:175-234`).

`run-once` is one complete collect-and-push pass; it exits when done, and
repeated invocation is safe (`crates/chat-stasher/src/main.rs:137-174`).

---

## 5. How to confirm it is working

Run this:

```sh
chat-stasher status
```

`status` is read-only. The source states its output boundary as: only ids,
paths, sizes, mtimes, and flags go to standard output; conversation content
does not (`crates/chat-stasher/src/main.rs:6243-6244`). This is the
source's self-description; we have not exhaustively verified every output path.

Its output has two parts. **The first line** is the timer health conclusion,
from the record left by the last `run-once`
(`crates/chat-stasher/src/main.rs:6024-6025`). These are the conclusions defined
verbatim in the source (`crates/chat-stasher/src/runstate.rs:184-232`):

- No timer installed / never run successfully:
  `[run-once] No run records yet: this machine has never completed a run-once successfully (or the state directory was cleared). Cannot determine whether the timer is working.`
- Everything is normal (`{}` is filled with the real numbers):
  `[run-once] OK: last run N minutes ago, took N ms, stored N shards, snapshot created.`
  (When there is nothing new, the ending is "no changes, so no snapshot
  created".)
- The timer may have stopped:
  `[run-once] Has not run for N days (threshold N hours): the timer may have stopped; the last result was success (no changes).`
- The last run failed:
  `[run-once] Last run failed: N minutes ago an error occurred at the <step> step, and no run has succeeded since.`

**The second part** is the scan result. By default it is a fixed summary of a
few lines and does not flood the screen
(`crates/chat-stasher/src/main.rs:6234-6244`):

- When there are conversations: `[scan] N conversations (N compressed): <source> N · <source> N`
- When none are found: `[scan] No conversations found on this machine.`
- When a source root directory does not exist, an extra line: `[scan] Skipped N source root directories that do not exist.`
- When there are identified conversations that will not be archived: `⚠ N harnesses have identified conversations that collect will not archive.`
- Finally, a fixed last line: `Details (one line per session): chat-stasher status --sessions`

To see the per-session detail, add `--sessions`; that will be hundreds of lines
(`crates/chat-stasher/src/main.rs:295-297`).

**🔴 A common pitfall:** `status` exits with a **non-zero code** when it judges
the timer "unhealthy", **it exits with a non-zero code**
(`crates/chat-stasher/src/main.rs:6092-6099`). So "the command errored"
does not necessarily mean the command is broken; it may well be telling you the
timer has stopped. Please read that first line.

Its four exit codes are: `0` = the timer is judged healthy · `1` = the scan
finished, but the timer is judged unhealthy (including **never having run**) ·
`3` = the scan did not complete at all (the registry could not be read, for
example; in that case it has no conclusion about your machine) · `2` = usage
error. **Note:** the entire report goes to **stderr**, so a pipeline like
`chat-stasher status 2>&1 | head` gives you `head`'s exit code of 0, not its.
To see the exit code, do not pipe, or use `${PIPESTATUS[0]}`.

There is also a related command: `doctor`. It answers a different question —
**whether any tool is silently deleting your history**. Its report contains
only paths, counts, bytes, and timestamps
(`crates/chat-stasher/src/main.rs:357-368`).

`doctor` also **connects once to each destination you declared**, read-only, and
reports what came back in three separate states rather than two: reached (and
whether a repository is there), not reached (with the classifier's verdict
attached), and not configured at all — a destination with no `repo` was never
dialled, and calling it "unreachable" would put a config mistake and a dead
network in one bucket (`crates/chat-stasher/src/doctor.rs:812-841`, `:879-917`).
It creates nothing, so a destination it reports as "not there yet" is still not
created by running `doctor`. This is the one thing `doctor` does that touches
the network; see section 4.4 if it reports a host it cannot trust.

### 5.1 Exporting a day

To get one day's conversations out of the archive as plain files:

```sh
chat-stasher export --destination <your-destination> --day 2026-01-15 --out ~/export-2026-01-15
```

That writes `<out>/<machine>/<harness>/<session-id>.jsonl` — each session's
archived lines in their native format, byte-identical to what `read` returns for
that session — plus `<out>/manifest.json`, which lists every session written
with its first and last message time, shard count, bytes, sha256 and the filters
that were applied.

Before it fetches anything it prints the price it is about to pay
(`sessions=` · `shards=` · `data_blobs=` · `plaintext_bytes=`), the same numbers
`chat-stasher search --cost` reports. `--dry-run` prints that price and stops:
no directory is created and no file is written.

- The day is matched against each session's **conversation** time, so a session
  that began the day before and was still active on this day is selected. Add
  `--trim-to-window` to also drop the individual lines whose own timestamp falls
  outside the day; a line whose timestamp cannot be read is kept and counted in
  the manifest's `untimed_lines`, never dropped.
- A session whose conversation time cannot be read is **not** silently left out.
  It is listed in the manifest under `sessions_not_placed` with the reason, and
  the command exits `3` rather than `0`, because "there was nothing that day"
  would then be unproven.
- `--out` must be empty or absent unless you pass `--force`. Nothing is ever
  deleted, and nothing is written outside `--out`.

Exit codes are the same family `search` uses: `0` wrote at least one session ·
`1` read everything and selected nothing · `3` did not finish (the files it did
write are real, and the manifest says what is missing) · `2` usage error
(`crates/chat-stasher/src/main.rs:521-601`).

---

## 6. 🔴 Things that do not exist yet

This section is an **honest list**. Everything below is the current state we
confirmed in the code, not a temporary disclaimer.

- **There is no `restore` command — nothing puts a session back into a
  harness's own directory, and that is not in phase one.** The subcommand table
  has no `restore` entry (`crates/chat-stasher/src/main.rs:130-991`). Getting
  content *out* does have a bulk path: `export --out <dir>` writes every session
  a time window selects to files in one command
  (`crates/chat-stasher/src/main.rs:521-601`), and `read` dumps **one**
  conversation to standard output at a time
  (`crates/chat-stasher/src/main.rs:312-356`). Restoring = for now you have to
  write your own script loop.

- **🔴 Lose the master key and there is no way to recover it.** There is no
  recovery process, no recovery code, no customer service. The source's own
  words are in section 4.3 (`crates/chat-stasher/src/store.rs:1189-1196`).

- **History backfill takes days, not minutes, and never runs on a fixed beat.**
  Content is fetched **at most 150–200 per day** (the day's cap is drawn once per
  local day and can never exceed 200), with at least 20 seconds plus a random
  0–25 seconds between two requests
  (`apps/extension/lib/backfill/pace.ts:92-103`, `:120-121`), and each round
  starts a random 5–10 minutes after the previous one
  (`apps/extension/lib/backfill/alarm.ts:90-91`). At that cap, a thousand
  conversations take at least 5 days. This is deliberately slow, not a bug.

- **Backfill is off by default.** The default is off
  (`apps/extension/lib/backfill/schedule.ts:40`), and the source states the
  reason for enabling it clearly: backfill uses your logged-in session to walk
  your whole account and fetch hundreds or thousands of conversations, so there
  must first be an explicit turn-on.
  ⚠️ **An earlier version of this document said "there is no on/off UI"; that
  sentence is now outdated:** clicking the extension icon in your browser
  toolbar now opens a small panel with a checkbox to turn it on
  (`apps/extension/entrypoints/popup/index.html`,
  `apps/extension/entrypoints/popup/main.ts`). That it defaults to off has not
  changed.

- **🔴 Turning on backfill still does not mean every platform's history will
  be recovered.** ChatGPT, DeepSeek, Gemini, Grok, Kimi and Claude list
  conversations **and** fetch their content — implemented on all six, not yet
  observed completing in a real browser, and on DeepSeek, Gemini, Grok, Kimi and
  Claude the completeness of a long conversation is unverified (Gemini is the one
  that pages; the others do not, and say so). Grok and Claude are the least
  verified of the six (their routes come from reading public open-source
  implementations rather than a logged-in session; Grok's one conversation costs
  two requests, and Claude's requests are addressed by an organization resolved
  from evidence rather than from the page URL), and where Grok's sources disagree
  about the list cursor it stops rather than picking one. Kimi's routes were measured in a logged-in
  session, and its requests carry your page's own login token (read from the page,
  memory only); a body response that admits it is incomplete is refused and listed
  as a failure rather than archived. Gemini's routes were measured too, its requests
  carry three values out of the page's own bootstrap blob (read at request time,
  memory only), and a conversation longer than 20 pages is refused rather than
  archived in part. Claude's routes come from reading public open-source
  implementations rather than from a logged-in claude.ai session, every request is
  addressed by an account-scoped organization the page URL does not carry (resolved
  from evidence, and the leg stops rather than choosing when an account has
  several), and a conversation whose body does not hold its whole branch is refused
  and listed as a failure rather than archived. That organization is asked for
  **in the claude.ai page**, over the same channel the backfill fetches through,
  and only when it is actually needed: when you press the start button for that
  platform, and on a wake-up whose recorded scope is not an organization yet.
  Resolving costs at most one extra request — `GET /api/organizations`, sent only
  when the page's own requests and the cookie both named none — and an account
  belonging to several organizations stops with a sentence telling you to open a
  conversation in the one you want archived, rather than picking one
  (`apps/extension/lib/backfill/claude-page.ts:62-136`). 🔴 **A Claude backfill
  keeps the organization it was started with:** switching organizations on
  claude.ai does not move it, and starting one for another organization means
  opening a conversation in that organization and pressing start there — the two
  then run as separate progress records
  (`apps/extension/entrypoints/background.ts:1106-1167`). Perplexity **only lists
  conversations, saving none of their content**. See section 1.1 for
  the list and the detailed explanation (list from
  `apps/extension/lib/backfill/enumerate.ts:4220-4247`). The
  middle tier is the one most likely to make you think "I've backed it up", so it
  gets its own bullet here.

- **The extension is not on a store yet; you install it manually.** The
  repository has no store listing material and no store extension ID;
  `package.json` is marked `"private": true` (`apps/extension/package.json:4`),
  and the build scripts produce a local directory and a zip
  (`apps/extension/package.json:10-13`). See section 3 for how to install.

- **A captured conversation is plaintext until the host acknowledges it.** A
  live capture is written into the extension's own IndexedDB outbox before any
  delivery is attempted and deleted only on a matching `ack`
  (`apps/extension/lib/outbox.ts:309-377`, `:379-394`); the popup's export file
  contains the same bodies. Other programs running as you can read all of it.
  (The "Security and privacy" section of `README.md` says the same.)

- **Zed and Cursor conversation enumeration is not implemented** (see the
  "What this does not do / current limits" section of `README.md` and the
  `crates/chat-stasher/data/harness-registry-v1.json` it cites).

- **`schedule` does not install the timer for you**; it only generates template
  files (`crates/chat-stasher/src/main.rs:175`). The actual installation steps
  are yours to do; **this document does not give the concrete install
  commands — unverified** (we have not completed a full launchd/systemd
  installation flow on this machine).

---

## 7. One-time, or something you keep doing?

This is the product's core promise, so say it clearly:

**At install time you do a few things by hand. After that, you never have to
touch it again.**

**Do once** (the ones in section 4):

- 🔴 Decide the stage directory and register the Native Messaging host
  (section 3.1), then confirm the popup says "connected" (section 3.2)
- `chat-stasher init`
- Decide where the archive lives
- 🔴 Back up the master key file
- 🔴 If the destination is remote, trust its host key once (section 4.4)
- Install the timer

**Then it runs automatically:** the timer runs `run-once` at each scheduled
point — collect, push, exit (`crates/chat-stasher/src/main.rs:137-174`). It does
not need you to confirm anything.

**What you should occasionally do** (not required, but recommended):

- Run `chat-stasher status` once in a while, and read that first line. The
  typical symptom of a broken timer is **not an error, it is silence** —
  `run-once` runs in the background and no one looks at its output, so it
  leaves a record every time, precisely so that `status` can say that sentence
  for you (`crates/chat-stasher/src/runstate.rs:1-11`). This is also why "never
  ran" is judged **unhealthy** rather than "fine": an absent record is the
  **absence of evidence**, not **evidence of health**
  (`crates/chat-stasher/src/runstate.rs:186-192`).
- Run `doctor` occasionally, to check whether any tool has started deleting
  your history.

**This is not "zero config."** Those seven things above genuinely require you
(the host-key one only if your destination is remote), and the ones about
backing up the key and checking a fingerprint are things no one can do for you.
But it is indeed **one-time** — once done, you do not have to think about it
again.

---

## 8. This document's "unverified" list

Collected in one place, so you know which spots to double-check yourself:

| Item | Status |
| --- | --- |
| Whether Chrome shows the "communicate with cooperating native applications" note for this permission set | **Unverified** (the permission list is `apps/extension/wxt.config.ts:113`; we read the manifest, we did not install the build and look at the warnings Chrome renders) |
| Whether every browser's discovery directory is where `install-native-host` looks for it | **Partly verified** (the per-OS layout is in `crates/chat-stasher/src/nativehost.rs:204-320`; the command prints every path it wrote, left alone, skipped or removed, so you can check the one your browser reads) |
| Whether the popup's language follows your browser correctly on every browser | **Unverified** (the default locale is `en` with a `zh_CN` catalog, `apps/extension/wxt.config.ts:66`; we did not test every browser's locale resolution) |
| Each browser's menu path for "Load unpacked extension" | **Unverified** |
| The minimum Rust version to compile the CLI | **Unverified** (the repository does not declare `rust-version`) |
| The minimum Node / pnpm version to build the extension | **Unverified** (the repository does not declare it) |
| The concrete installation steps for a launchd / systemd timer | **Unverified** (`schedule` only renders templates, does not install) |
| How `known_hosts_strategy` behaves against a real server | **Partly verified** (the three values and their `StrictHostKeyChecking` equivalents were read from the pinned dependency's source — opendal-service-sftp 0.57.0 `src/backend.rs` lines 148-165 and the `openssh` crate it maps onto — but we have not exercised `add` or `accept` against a live host. Section 4.4 describes what each one gives up.) |
| Whether passive capture on Perplexity delivers the conversation it names | **Unverified** (reading the code, the conclusion is now "it recognizes the id and delivers"; see section 1. The route itself was read out of public source, not measured, and we have not tried it on a real page.) |
| Whether the DeepSeek / Perplexity / Grok conversation-list endpoints still look like this today | **Unverified** (from cross-checking multiple open-source implementations, not official documentation, and not tested with a logged-in session; `apps/extension/lib/backfill/enumerate.ts:2634-2686`, `:2763-2777`, `:2869-2930`. If the shape changes, it stops on the spot and leaves a trace, rather than producing fake progress. That trace carries the shape of the response that did not match — the key names, types and array lengths at the level that disagreed — and carries no conversation text, no id and no title from it (`apps/extension/lib/backfill/enumerate.ts:1242-1310`), so a shape change can be diagnosed from the trace itself instead of from a second logged-in session.) |
| Which Grok list cursor the real backend honours — an opaque `pageToken` or an integer `page` | **Unverified** (the sources disagree; `apps/extension/lib/backfill/enumerate.ts:2869-2930`. The extension does not choose: it hands back exactly what it was given, and a page that repeats what was already listed stops the leg and says the response shape changed, rather than being read as "no more conversations"; `apps/extension/lib/backfill/engine.ts:1437-1482`.) |
| Whether a **long** Grok conversation comes back complete from the backfill content endpoint | **Unverified** (its two-step route — a skeleton call then a content call — was cross-checked across implementations, but none of them pages the content call and this extension adds no paging, so a long conversation may be stored as only its first part; `apps/extension/lib/backfill/enumerate.ts:2869-2930`.) |
| Whether a **long** DeepSeek conversation comes back complete from the backfill body endpoint | **Not settled by any source — and checked rather than assumed** (the endpoint itself is well evidenced: it is the route DeepSeek's own page calls in a real logged-in browser session, and several independent open-source exporters request the same route; `apps/extension/lib/backfill/enumerate.ts:2544-2558`. None of the reviewed implementations pages it and this extension adds no paging. Instead of assuming a single response holds the whole conversation, the extension walks the response's own tree — `chat_session.current_message_id` back along `parent_id` to a root — and archives the body only if that walk closes; a body that came back short is not archived at all, it is recorded as a failure with its own reason code and the leg carries on; `apps/extension/lib/backfill/enumerate.ts:2560-2587`.) |
| Whether a **long** Kimi conversation comes back complete from the backfill body endpoint | **Unverified, and handled rather than guessed** (a logged-in session measured the route and five **short** conversations, none of which carried a page-token field; nothing here pages that endpoint. If a response ever does say it holds more of the conversation, that conversation is not archived at all — it is recorded as a failure with its own reason code and the leg moves on, because a truncated conversation stored as a complete one would be silent loss; `apps/extension/lib/backfill/enumerate.ts:2954-3016`; `apps/extension/lib/backfill/engine.ts:2010-2045`.) |
| Whether the Kimi gateway requires the two extra request headers the page sends, or whether they are merely what the page happens to send | **Unverified** (the page's requests were observed carrying `x-msh-platform` and `x-language` alongside the bearer token, so the backfill requests send them too — that they are *required* has not been tested; `apps/extension/lib/platform-auth.ts:216-248`.) |
| Whether a Kimi backfill run has ever completed end to end in a real browser | **Unverified** (implemented and wired to the host, like the other three; no complete run observed. See section 1.1.) |
| Whether a ChatGPT or DeepSeek backfill run has ever completed end to end in a real browser | **Unverified** (both legs are implemented and wired to the host, but no complete run has been observed in a real browser. See section 1.1.) |

"Unverified" = we have not tested it; it does not mean it does not exist, and
it does not mean it does not work. The things in section 6 above that are
listed as **absent** are things we checked in the code and confirmed **really
do not exist** — please keep the two categories separate.
