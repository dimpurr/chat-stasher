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
| **Browser extension (Chat Stasher)** | Saves your conversations from **web-based** chats as files into your download directory, waiting for the CLI to collect them | Your browser |

**On the CLI side:** its self-description is "Append-only archive for every LLM
conversation, across harnesses." (`crates/chat-stasher/src/main.rs:34`). It
reads session files that already exist on your machine, and reads them
read-only (`crates/chat-stasher/src/main.rs:496`).

**On the extension side:** it currently recognizes **six** web platforms —
DeepSeek (`chat.deepseek.com`), Perplexity (`www.perplexity.ai`), ChatGPT
(`chatgpt.com` / `chat.openai.com`), Gemini (`gemini.google.com`), Claude
(`claude.ai`), Kimi (`www.kimi.com`)
(`apps/extension/lib/contract.ts:69-70,112-113,128-129,144-145,160-161,222-231`).
It requests only three permissions: `downloads`, `storage` and `alarms`
(`apps/extension/wxt.config.ts:35`). It writes captured conversations as JSON
files at `chat-stasher/inbox/<name>.json` under the download directory
(`apps/extension/lib/contract.ts:324`, `apps/extension/lib/download.ts:91`).

**How the two sides connect:** the extension only writes files to disk; the CLI
takes them away with `ingest --inbox <your-inbox> --stage <your-stage>`
(`crates/chat-stasher/src/main.rs:471-493`).

🔴 **"Recognizing the platform" does not mean "it can recover your history on
that platform."** The extension has two legs; please read them separately:

- **Passive capture** (on by default): the conversation you are currently
  viewing is saved as a side effect when the page fetches its own data. Each
  platform registers in that table which route, method, and response shape
  count (`apps/extension/lib/contract.ts:69-303`).
  🔴 **Perplexity is an exception; read it as it is:** its row registers only
  the **conversation-list** route, and registers no rule for recognizing a
  session id from a URL (`apps/extension/lib/contract.ts:113-122`). Reading the
  code, passive capture on Perplexity **cannot recognize a session id and
  therefore saves no files** (`apps/extension/lib/contract.ts:517-519`) — this
  is a conclusion drawn from reading the code; **we have not tested it on a
  real perplexity.ai page**.
- **History backfill** (off by default; see section 6): digs up your **past**
  conversations and saves them. This leg's **capability differs per platform**,
  spelled out in section 1.1 below.

### 1.1 🔴 History backfill: three tiers, not a "supported / unsupported" binary

The list below comes directly from the two tables in the code, not from
marketing (`apps/extension/lib/backfill/enumerate.ts:762`, `:774`, `:643`):

| Tier | Platforms | What you actually get when you enable backfill |
| --- | --- | --- |
| **Can recover the actual history text** | **ChatGPT** | Conversations are listed one by one, and their content is fetched one by one and saved as files. This tier is the one that means "your history is backed up." |
| **🔴 Can only list conversations, saves none of their content** | **DeepSeek**, **Perplexity** | The extension can list which historical conversations you have, but **will not fetch each conversation's content**, so **not a single file lands in your download directory**. Your DeepSeek / Perplexity history is **not backed up**. |
| **Not implemented** | **Gemini**, **Claude**, **Kimi** | The backfill leg stops before issuing any request. Nothing happens. |

🔴 **The middle tier is the easiest to misunderstand, so say it again**:
DeepSeek and Perplexity, with backfill enabled, the extension **does act** (it
lists conversations), and the popup shows "already listed N, waiting".
**But not one of those N is saved.** If you now close your browser, format your
disk, or the platform deletes your history, those N conversations are gone —
the extension holds only their ids, not their content.

🔴 **Perplexity gets one more sentence:** per the passive-capture note above,
its row cannot recognize a session id even for passive capture. Which means —
going by the code — **Perplexity currently leaves you no files from either
leg**: backfill only lists, and passive capture saves nothing either. It appears
in the list because the extension runs on that site; it does **not** mean what
is there is backed up.

The reason is written in the code, not because we are lazy: the
**conversation-list endpoints** for these two platforms have multiple
independent open-source implementations that cross-check one another, but the
**endpoint for fetching a single conversation's content has none**
(`apps/extension/lib/backfill/enumerate.ts:538-548`, `:603-611`). We will not
guess a content-endpoint address — a wrong guess would not error; it would save
only the first few turns of every conversation while you believed you had it
all.

The popup shows these three tiers in the same terms as the table above
(`apps/extension/lib/popup-view.ts:498-514`).

(**Passive capture is not affected by this table:** the passive-capture criteria
for the six platforms above are each registered in the table at
`apps/extension/lib/contract.ts:69-303`, a separate matter from backfill.)

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
by `.gitignore`, `.gitignore:15`). Then **how to load that directory into your
browser** — each browser's "Load unpacked extension" menu path — see the next
section; we have not tested each one, and marked them "unverified".

---

## 4. One-time setup checklist

The following things, you do **once at install time and then never again**.

### 4.1 🔴 Turn off the browser's "Ask where to save each file before downloading"

**This one matters most; please do not skip it.**

**Why turn it off:** the extension saves conversations through the browser's
download channel (`apps/extension/lib/download.ts:117-121,130-134`). And if you
have "ask where to save each file before downloading" on, the browser may pop a
system "Save As" dialog for every file it saves. Our target scenario is
archiving **thousands of conversations** over a few days — the number of dialogs
in that situation is not one you want to experience.

**🔴 Please read the strength of the evidence for this item honestly:**

- **Verified:** Chrome does have this setting, and its key in the config file is
  `download.prompt_for_download`. We read the key directly in our machine's
  Chrome `Preferences` file, and its value at the time was `true` (on). This is
  **first-hand evidence**, but it only proves "this setting exists", not how it
  affects the extension.
- **Code fact:** when the extension calls the download API it passes
  `saveAs: false`, meaning the code **asks** for no Save-As dialog
  (`apps/extension/lib/download.ts:121`, `:134`).
- **🔴 What we do not know:** **whether `saveAs: false` is forcibly overridden
  by this browser setting — we have not tested it ourselves.** External reports
  and a Chromium issue point to "it is overridden", but that is **second-hand
  evidence**, and we have not reproduced it.

**So this is an operational recommendation, not a behavior guarantee:** please
turn this setting off, to **try to avoid** dialogs interrupting the archiving
process. We do not promise that no dialog will ever appear with it off, nor that
dialogs will definitely appear with it on — we are not yet in a position to
claim either.

**Where to click:**

- **Chrome:** in Settings, the "Downloads" section has an "Ask where to save
  each file before downloading" toggle; turn it off. **The exact menu hierarchy
  and wording — unverified** (we only verified the existence of the config key
  `download.prompt_for_download`; we did not actually click through the UI, and
  the wording may change across browser versions).
- **Edge:** **unverified**. Edge is also Chromium-based, so the setting most
  likely exists with a similar name, but we have not verified any menu path on
  Edge, so we give no path here.
- **Firefox:** **unverified**. We have not verified the setting's location on
  Firefox, nor its effect on the extension's downloads.

(We would rather have you search your own settings for the word "download" than
invent a menu path here that might be wrong.)

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
forever" (`crates/chat-stasher/src/store.rs:1102-1104`). The key file is written
with owner-only-readable permissions, on platforms that can express them
(`crates/chat-stasher/src/store.rs:1202-1210`).

**Make a copy of it somewhere else right now.** No one can do this for you.

### 4.4 Install a timer (optional, but this is the key to "install once and forget it")

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
does not (`crates/chat-stasher/src/main.rs:5658-5659`). This is the
source's self-description; we have not exhaustively verified every output path.

Its output has two parts. **The first line** is the timer health conclusion,
from the record left by the last `run-once`
(`crates/chat-stasher/src/main.rs:5439-5440`). These are the conclusions defined
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
(`crates/chat-stasher/src/main.rs:5649-5659`):

- When there are conversations: `[scan] N conversations (N compressed): <source> N · <source> N`
- When none are found: `[scan] No conversations found on this machine.`
- When a source root directory does not exist, an extra line: `[scan] Skipped N source root directories that do not exist.`
- When there are identified conversations that will not be archived: `⚠ N harnesses have identified conversations that collect will not archive.`
- Finally, a fixed last line: `Details (one line per session): chat-stasher status --sessions`

To see the per-session detail, add `--sessions`; that will be hundreds of lines
(`crates/chat-stasher/src/main.rs:206-208`).

**🔴 A common pitfall:** `status` exits with a **non-zero code** when it judges
the timer "unhealthy", **it exits with a non-zero code**
(`crates/chat-stasher/src/main.rs:5507-5514`). So "the command errored"
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

---

## 6. 🔴 Things that do not exist yet

This section is an **honest list**. Everything below is the current state we
confirmed in the code, not a temporary disclaimer.

- **There is no `restore` (bulk recovery) command. Not in phase one.** The
  subcommand table has no `restore` entry
  (`crates/chat-stasher/src/main.rs:44-821`). What you can do is `read`, which
  dumps **one** conversation to standard output at a time
  (`crates/chat-stasher/src/main.rs:223-267`). Bulk restore = for now you have
  to write your own script loop.

- **🔴 Lose the master key and there is no way to recover it.** There is no
  recovery process, no recovery code, no customer service. The source's own
  words are in section 4.3 (`crates/chat-stasher/src/store.rs:1102-1109`).

- **History backfill takes days, not minutes.** The backfill leg's rate limit
  for fetching content is **at most 200 per day**, with at least 20 seconds
  between two requests (`apps/extension/lib/backfill/pace.ts:42`; comment at
  `:15`). At that cap, a thousand conversations take at least 5 days. This is
  deliberately slow, not a bug.

- **Backfill is off by default.** The default is off
  (`apps/extension/lib/backfill/schedule.ts:32`), and the source states the
  reason for enabling it clearly: backfill uses your logged-in session to walk
  your whole account and write hundreds or thousands of files into the download
  directory, so there must first be an explicit turn-on.
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
  `apps/extension/lib/backfill/enumerate.ts:762`, `:774`, `:643`). This tier is
  the one most likely to make you think "I've backed it up", so it gets its own
  bullet here.

- **The extension is not on a store yet; you install it manually.** The
  repository has no store listing material and no store extension ID;
  `package.json` is marked `"private": true` (`apps/extension/package.json:4`),
  and the build scripts produce a local directory and a zip
  (`apps/extension/package.json:10-13`). See section 3 for how to install.

- **Captured conversations lie in plaintext in your download directory until
  `ingest` takes them away.** (`apps/extension/lib/download.ts:91`; the
  "Security and privacy" section of `README.md` says the same.) Other programs
  on the same machine can read them.

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

- Turn off the browser's "ask where to save each file before downloading"
- `chat-stasher init`
- Decide where the archive lives
- 🔴 Back up the master key file
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

**This is not "zero config."** Those five things above genuinely require you,
and the one about backing up the key is something no one can do for you. But it
is indeed **one-time** — once done, you do not have to think about it again.

---

## 8. This document's "unverified" list

Collected in one place, so you know which spots to double-check yourself:

| Item | Status |
| --- | --- |
| Whether `saveAs: false` is overridden by the browser's "ask where to save each file before downloading" | **Unverified** (we did not test it; external reports and a Chromium issue point to "yes", which is **second-hand evidence**) |
| Chrome's **menu path and wording** for turning that setting off | **Unverified** (only verified that the config key `download.prompt_for_download` exists in this machine's Chrome `Preferences`, with value `true`) |
| Edge's location for that setting | **Unverified** |
| Firefox's location for that setting and its effect on extension downloads | **Unverified** |
| Each browser's menu path for "Load unpacked extension" | **Unverified** |
| The minimum Rust version to compile the CLI | **Unverified** (the repository does not declare `rust-version`) |
| The minimum Node / pnpm version to build the extension | **Unverified** (the repository does not declare it) |
| The concrete installation steps for a launchd / systemd timer | **Unverified** (`schedule` only renders templates, does not install) |
| Whether passive capture actually saves anything on Perplexity | **Unverified** (reading the code, the conclusion is "cannot recognize a session id, therefore saves nothing"; see section 1. We have not tried it on a real page.) |
| Whether the DeepSeek / Perplexity conversation-list endpoints still look like this today | **Unverified** (from cross-checking multiple open-source implementations, not official documentation, and not tested with a logged-in session; `apps/extension/lib/backfill/enumerate.ts:549-557`, `:612-621`. If the shape changes, it stops on the spot and leaves a trace, rather than producing fake progress.) |

"Unverified" = we have not tested it; it does not mean it does not exist, and
it does not mean it does not work. The things in section 6 above that are
listed as **absent** are things we checked in the code and confirmed **really
do not exist** — please keep the two categories separate.
