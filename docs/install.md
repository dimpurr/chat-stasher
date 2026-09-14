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
conversation, across harnesses." (`crates/chat-stasher/src/main.rs:34`). It
reads session files that already exist on your machine, and reads them
read-only (`crates/chat-stasher/src/main.rs:502`).

**On the extension side:** it currently recognizes **six** web platforms —
DeepSeek (`chat.deepseek.com`), Perplexity (`www.perplexity.ai`), ChatGPT
(`chatgpt.com` / `chat.openai.com`), Gemini (`gemini.google.com`), Claude
(`claude.ai`), Kimi (`www.kimi.com`)
(`apps/extension/lib/contract.ts:70,121,139,155,171,241`).
It requests four permissions — `nativeMessaging`, `storage`, `alarms` and
`unlimitedStorage` — and **no host permissions at all**
(`apps/extension/wxt.config.ts:63`). There is no `downloads` permission and no
automatic download anywhere.

**How the two sides connect:** the extension sends each captured conversation to
a **Native Messaging host**, which is the `chat-stasher` binary you registered
by hand with `chat-stasher install-native-host --stage <your-stage>`
(`crates/chat-stasher/src/main.rs:638-680`). The protocol both sides implement
is written down in [`contracts/nativehost-protocol.md`](../contracts/nativehost-protocol.md).

🔴 **A conversation counts as delivered only when the host answers an `ack`
whose `request_id` and `sha256` equal the ones the extension sent**
(`apps/extension/lib/native-host.ts:451-460`). Everything else — a `nack`, a
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
  count (`apps/extension/lib/contract.ts:67-313`).
  🔴 **Perplexity is an exception; read it as it is:** its row registers only
  the **conversation-list** route, and registers no rule for recognizing a
  session id from a URL (`apps/extension/lib/contract.ts:120-135`; the empty
  list is `apps/extension/lib/contract.ts:132`). Reading the
  code, passive capture on Perplexity **cannot recognize a session id and
  therefore delivers nothing** (`apps/extension/lib/contract.ts:547-575`) — this
  is a conclusion drawn from reading the code; **we have not tested it on a
  real perplexity.ai page**.
- **History backfill** (off by default; see section 6): digs up your **past**
  conversations and saves them. This leg's **capability differs per platform**,
  spelled out in section 1.1 below.

### 1.1 🔴 History backfill: three tiers, not a "supported / unsupported" binary

The list below comes directly from the two tables in the code, not from
marketing (`apps/extension/lib/backfill/enumerate.ts:880-886`, `:894-900`, `:752`):

| Tier | Platforms | What you actually get when you enable backfill |
| --- | --- | --- |
| **Can recover the actual history text** | **ChatGPT** | Conversations are listed one by one, and their content is fetched one by one and delivered to the host. This tier is the one that means "your history is backed up." |
| **🔴 Can only list conversations, saves none of their content** | **DeepSeek**, **Perplexity** | The extension can list which historical conversations you have, but **will not fetch each conversation's content**, so **not one of them is delivered or queued**. Your DeepSeek / Perplexity history is **not backed up**. |
| **Not implemented** | **Gemini**, **Claude**, **Kimi** | The backfill leg stops before issuing any request. Nothing happens. |

🔴 **The middle tier is the easiest to misunderstand, so say it again**:
DeepSeek and Perplexity, with backfill enabled, the extension **does act** (it
lists conversations), and the popup shows "already listed N, waiting".
**But not one of those N is saved.** If you now close your browser, format your
disk, or the platform deletes your history, those N conversations are gone —
the extension holds only their ids, not their content.

🔴 **Perplexity gets one more sentence:** per the passive-capture note above,
its row cannot recognize a session id even for passive capture. Which means —
going by the code — **Perplexity currently archives nothing from either leg**:
backfill only lists, and passive capture delivers nothing either. It appears
in the list because the extension runs on that site; it does **not** mean what
is there is backed up.

The reason is written in the code, not because we are lazy: the
**conversation-list endpoints** for these two platforms have multiple
independent open-source implementations that cross-check one another, but the
**endpoint for fetching a single conversation's content has none**
(`apps/extension/lib/backfill/enumerate.ts:545-556`, `:630-640`). We will not
guess a content-endpoint address — a wrong guess would not error; it would save
only the first few turns of every conversation while you believed you had it
all.

The popup shows these three tiers in the same terms as the table above
(`apps/extension/lib/popup-view.ts:610-623`).

(**Passive capture is not affected by this table:** the passive-capture criteria
for the six platforms above are each registered in the table at
`apps/extension/lib/contract.ts:67-313`, a separate matter from backfill.)

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
already exist; it is non-destructive (`crates/chat-stasher/src/main.rs:46-47`).
The config file lives at `~/.config/chat-stasher/config.toml`, or under
`XDG_CONFIG_HOME` if you have set it (`crates/chat-stasher/src/config.rs:15,491-502`).

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
(`crates/chat-stasher/src/nativehost.rs:923-934`). The stage is the same staging
directory you use for `collect` / `seal` / `ingest`.

The command is idempotent — run it twice and there is exactly one manifest per
browser, byte-identical, exit 0 both times — and it prints every path it wrote,
left alone, skipped or removed, absolutely (`crates/chat-stasher/src/main.rs:615-637`).
It is per-user; nothing needs elevation. `--uninstall` removes exactly the files
it wrote and nothing else.

**What the browser asks you at install time.** Registering the host does not
remove any browser prompt, but it changes which one you see. The extension
declares `nativeMessaging`, so Chrome shows *"communicate with cooperating
native applications"* on its details page. It no longer declares `downloads`, so
the *"Manage your downloads"* warning is gone
(`apps/extension/wxt.config.ts:63`).

### 3.2 Confirm the popup says "connected"

Click the extension's toolbar icon. The popup asks the host one `hello` question
and renders the answer — **the stage it writes to, the machine id, and the host
version** — or the reason it could not, with the command that fixes it
(`apps/extension/lib/ui-strings.ts:80-100`;
`apps/extension/entrypoints/background.ts:370-377`).

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
is a stage nothing pushes (`crates/chat-stasher/src/nativehost.rs:923-934`).

Two properties of that directory, both from
[`contracts/nativehost-protocol.md`](../contracts/nativehost-protocol.md):

- **The host and `ingest` take an exclusive lock on `<stage>/.ingest.lock`
  before they allocate a shard sequence number**, so two browsers, two profiles,
  or a host racing a manual `ingest` cannot pick the same number. The wait is
  bounded at 10 seconds, and a timeout comes back as a `stage-unavailable` the
  extension retries (`crates/chat-stasher/src/inbox.rs:66-68`, `:853-881`).
- **A stage the host cannot use is reported, not replaced.** A missing or
  relative `[native_host] stage` is a `config` refusal, and a path that is not a
  directory is `stage-unavailable` (`crates/chat-stasher/src/nativehost.rs:875-935`);
  if the seal itself fails, a lock-wait timeout is `stage-unavailable` and any
  other write error is `io`, and neither acknowledges anything
  (`crates/chat-stasher/src/nativehost.rs:1113-1117`). In every case the reason
  names the fix.

Put it somewhere you will not delete: these shards are the archive's input, and
`push` is what moves them into the encrypted repository.

**The host never invents a machine identity either.** It resolves the machine id
exactly as `ingest` does, and if there is none it refuses with a `config` `nack`
that names the fix, rather than minting a second identity — which would silently
put every delivered shard in a different machine's archive partition
(`crates/chat-stasher/src/nativehost.rs:940-969`). Run any archiving command
once from your shell before registering the host.

### 4.2 Run `chat-stasher init` once

See section 2. If you already did it, you do not need to do it again.

### 4.3 Decide where the archive lives, and **back up your master key file**

The archive's destination is decided by your config and command-line arguments
— a local path, or a backend you configure yourself. `push` / `read` / `verify`
read the repository and key file you select in config or arguments
(`crates/chat-stasher/src/main.rs:163-168,251-256,302-307`).

🔴 **The master key file is the only key. Lose it and the archive can never be
read again; there is no way to recover it.** The source's own words are "The
masterkey is the repository's only key — losing it means the repo is unreadable
forever" (`crates/chat-stasher/src/store.rs:1106-1108`). The key file is written
with owner-only-readable permissions, on platforms that can express them
(`crates/chat-stasher/src/store.rs:1206-1214`).

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
(`crates/chat-stasher/src/main.rs:2737-2750`); without it, an unattended
scheduled run that meets a new host stops instead of quietly trusting it.

**What you see when it happens.** `dest-init` connects once, read-only, before
it does anything else (`crates/chat-stasher/src/main.rs:2773-2797`). An
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
to `~/.ssh/known_hosts` (`crates/chat-stasher/src/main.rs:2752-2761`;
`crates/chat-stasher/src/remote_err.rs:503-536`). The flag is for remote
destinations only: on a local path it is refused with exit code `2` rather than
silently doing nothing (`crates/chat-stasher/src/main.rs:2740-2748`).

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
(`crates/chat-stasher/src/main.rs:86`). The generated template wraps a
`run-once` command (`crates/chat-stasher/src/main.rs:86-145`).

`run-once` is one complete collect-and-push pass; it exits when done, and
repeated invocation is safe (`crates/chat-stasher/src/main.rs:48-85`).

---

## 5. How to confirm it is working

Run this:

```sh
chat-stasher status
```

`status` is read-only. The source states its output boundary as: only ids,
paths, sizes, mtimes, and flags go to standard output; conversation content
does not (`crates/chat-stasher/src/main.rs:5813-5814`). This is the
source's self-description; we have not exhaustively verified every output path.

Its output has two parts. **The first line** is the timer health conclusion,
from the record left by the last `run-once`
(`crates/chat-stasher/src/main.rs:5594-5595`). These are the conclusions defined
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
(`crates/chat-stasher/src/main.rs:5804-5814`):

- When there are conversations: `[scan] N conversations (N compressed): <source> N · <source> N`
- When none are found: `[scan] No conversations found on this machine.`
- When a source root directory does not exist, an extra line: `[scan] Skipped N source root directories that do not exist.`
- When there are identified conversations that will not be archived: `⚠ N harnesses have identified conversations that collect will not archive.`
- Finally, a fixed last line: `Details (one line per session): chat-stasher status --sessions`

To see the per-session detail, add `--sessions`; that will be hundreds of lines
(`crates/chat-stasher/src/main.rs:206-208`).

**🔴 A common pitfall:** `status` exits with a **non-zero code** when it judges
the timer "unhealthy", **it exits with a non-zero code**
(`crates/chat-stasher/src/main.rs:5662-5669`). So "the command errored"
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
(`crates/chat-stasher/src/main.rs:268-279`).

`doctor` also **connects once to each destination you declared**, read-only, and
reports what came back in three separate states rather than two: reached (and
whether a repository is there), not reached (with the classifier's verdict
attached), and not configured at all — a destination with no `repo` was never
dialled, and calling it "unreachable" would put a config mistake and a dead
network in one bucket (`crates/chat-stasher/src/doctor.rs:806-835`, `:873-911`).
It creates nothing, so a destination it reports as "not there yet" is still not
created by running `doctor`. This is the one thing `doctor` does that touches
the network; see section 4.4 if it reports a host it cannot trust.

---

## 6. 🔴 Things that do not exist yet

This section is an **honest list**. Everything below is the current state we
confirmed in the code, not a temporary disclaimer.

- **There is no `restore` (bulk recovery) command. Not in phase one.** The
  subcommand table has no `restore` entry
  (`crates/chat-stasher/src/main.rs:44-827`). What you can do is `read`, which
  dumps **one** conversation to standard output at a time
  (`crates/chat-stasher/src/main.rs:223-267`). Bulk restore = for now you have
  to write your own script loop.

- **🔴 Lose the master key and there is no way to recover it.** There is no
  recovery process, no recovery code, no customer service. The source's own
  words are in section 4.3 (`crates/chat-stasher/src/store.rs:1106-1113`).

- **History backfill takes days, not minutes.** The backfill leg's rate limit
  for fetching content is **at most 200 per day**, with at least 20 seconds
  between two requests (`apps/extension/lib/backfill/pace.ts:49`; the arithmetic
  behind both numbers is the comment at `:16-22`). At that cap, a thousand
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
  be recovered.** Only ChatGPT actually saves the history content; DeepSeek and
  Perplexity **only list conversations, saving none of their content**;
  Gemini / Claude / Kimi are entirely unsupported. See section 1.1 for the list
  and the detailed explanation (list from
  `apps/extension/lib/backfill/enumerate.ts:880-886`, `:894-900`, `:752`). This
  tier is the one most likely to make you think "I've backed it up", so it gets
  its own bullet here.

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
  files (`crates/chat-stasher/src/main.rs:86`). The actual installation steps
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
point — collect, push, exit (`crates/chat-stasher/src/main.rs:48-85`). It does
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
| Whether Chrome shows the "communicate with cooperating native applications" note for this permission set | **Unverified** (the permission list is `apps/extension/wxt.config.ts:63`; we read the manifest, we did not install the build and look at the warnings Chrome renders) |
| Whether every browser's discovery directory is where `install-native-host` looks for it | **Partly verified** (the per-OS layout is in `crates/chat-stasher/src/nativehost.rs:200-289`; the command prints every path it wrote, left alone, skipped or removed, so you can check the one your browser reads) |
| Whether the popup's language follows your browser correctly on every browser | **Unverified** (the default locale is `en` with a `zh_CN` catalog, `apps/extension/wxt.config.ts:16`; we did not test every browser's locale resolution) |
| Each browser's menu path for "Load unpacked extension" | **Unverified** |
| The minimum Rust version to compile the CLI | **Unverified** (the repository does not declare `rust-version`) |
| The minimum Node / pnpm version to build the extension | **Unverified** (the repository does not declare it) |
| The concrete installation steps for a launchd / systemd timer | **Unverified** (`schedule` only renders templates, does not install) |
| How `known_hosts_strategy` behaves against a real server | **Partly verified** (the three values and their `StrictHostKeyChecking` equivalents were read from the pinned dependency's source — opendal-service-sftp 0.57.0 `src/backend.rs` lines 148-165 and the `openssh` crate it maps onto — but we have not exercised `add` or `accept` against a live host. Section 4.4 describes what each one gives up.) |
| Whether passive capture actually delivers anything on Perplexity | **Unverified** (reading the code, the conclusion is "cannot recognize a session id, therefore delivers nothing"; see section 1. We have not tried it on a real page.) |
| Whether the DeepSeek / Perplexity conversation-list endpoints still look like this today | **Unverified** (from cross-checking multiple open-source implementations, not official documentation, and not tested with a logged-in session; `apps/extension/lib/backfill/enumerate.ts:545-556`, `:630-640`. If the shape changes, it stops on the spot and leaves a trace, rather than producing fake progress.) |

"Unverified" = we have not tested it; it does not mean it does not exist, and
it does not mean it does not work. The things in section 6 above that are
listed as **absent** are things we checked in the code and confirmed **really
do not exist** — please keep the two categories separate.
