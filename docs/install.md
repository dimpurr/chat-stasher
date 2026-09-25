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
conversation, across harnesses." (`crates/chat-stasher/src/main.rs:82`). It
reads session files that already exist on your machine, and reads them
read-only (`crates/chat-stasher/src/main.rs:715`).

**On the extension side:** it currently recognizes **seven** web platforms —
DeepSeek (`chat.deepseek.com`), Perplexity (`www.perplexity.ai`), ChatGPT
(`chatgpt.com` / `chat.openai.com`), Gemini (`gemini.google.com`), Claude
(`claude.ai`), Kimi (`www.kimi.com`), Grok (`grok.com`)
(`apps/extension/lib/contract.ts:363,432,449,491,550,678,766`).

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
(`apps/extension/wxt.config.ts:125`). There is no `downloads` permission and no
automatic download anywhere.

**How the two sides connect:** the extension sends each captured conversation to
a **Native Messaging host**, which is the `chat-stasher` binary you registered
by hand with `chat-stasher install-native-host --stage <your-stage>`
(`crates/chat-stasher/src/main.rs:851-893`). The protocol both sides implement
is written down in [`contracts/nativehost-protocol.md`](../contracts/nativehost-protocol.md).

🔴 **A conversation counts as delivered only when the host answers an `ack`
whose `request_id` and `sha256` equal the ones the extension sent**
(`apps/extension/lib/native-host.ts:933-942`). Everything else — a `nack`, a
timeout, a disconnect — is *not delivered*, and the capture stays in the
extension's own outbox until a matching `ack` deletes it
(`apps/extension/lib/outbox.ts:380-395`). There is no "probably delivered".

If you would rather not register the host at all, the extension can instead
export everything it has not delivered as one file, which you feed to the CLI by
hand: `chat-stasher ingest --inbox <directory> --stage <your-stage>`. Section 3.3
covers that.

🔴 **"Recognizing the platform" does not mean "it can recover your history on
that platform."** The extension has two legs; please read them separately:

- **Passive capture** (on by default): the conversation you are currently
  viewing is saved as a side effect when the page fetches its own data. Each
  platform registers in that table which route, method, and response shape
  count (`apps/extension/lib/contract.ts:315-768`).
  🔴 **Perplexity used to be the exception here; read where it stands now:**
  its row registers the **conversation-content** route — path hint
  `/rest/thread/`, method `GET`, response shape requiring `entries`
  (`apps/extension/lib/contract.ts:367-435`) — and it recognizes the session id
  from the page URL, the `/search/<slug>` the thread is open at
  (`apps/extension/lib/contract.ts:419-424`). The **conversation-list** route is
  deliberately outside the row: a list is a summary of conversations, not one of
  them, so it is skipped silently rather than captured.
  So, reading the code, passive capture on Perplexity **does name the
  conversation you are viewing and delivers it**
  (`apps/extension/lib/contract.ts:1132-1160`) — but this is still a conclusion
  drawn from reading the code, and the route itself was read out of public
  source rather than measured: **we have not tested it on a real perplexity.ai
  page.**
- **History backfill** (off by default; see section 6): digs up your **past**
  conversations and saves them. This leg's **capability differs per platform**,
  spelled out in section 1.1 below.

### 1.1 🔴 History backfill: three tiers, not a "supported / unsupported" binary

The list below comes directly from the two tables in the code, not from
marketing (`apps/extension/lib/backfill/enumerate.ts:4590-4621`):

| Tier | Platforms | What you actually get when you enable backfill |
| --- | --- | --- |
| **Implements fetching the actual history text** | **ChatGPT**, **DeepSeek**, **Perplexity**, **Gemini**, **Grok**, **Kimi**, **Claude** | Conversations are listed one by one, and their content is fetched one by one and delivered to the host. This tier is the one that means "your history is backed up" — but read it as *implemented*, not *verified*: a complete backfill has not yet been observed in a real browser on any of the seven. DeepSeek's body request is `GET /api/v0/chat/history_messages?chat_session_id=<id>` (`apps/extension/lib/backfill/enumerate.ts:2815-2818`). 🔴 Grok and Claude are the least verified of the seven: their routes came from reading public open-source implementations, not from a logged-in session, and Grok fetches one conversation with **two** same-origin requests — a skeleton call, then a content call whose body is built only from the ids the skeleton named (`apps/extension/lib/backfill/enumerate.ts:3034-3036`, `:3116-3177`). 🔴 W84 (2026-09-23) filled in Perplexity's body segment last: its route is `GET /rest/thread/<slug>` with a five-parameter set pinned by the plan (`apps/extension/lib/backfill/enumerate.ts:2919-2930`), and a logged-in probe observed a stated completeness signal (`has_next_page` + `next_cursor`) at the top level, so the extension archives a body only when the response declares there is no more, and **refuses** a body that declares more rather than archiving a truncated conversation (`apps/extension/lib/backfill/enumerate.ts:2267-2311`). Kimi's routes, unlike Grok's, **were** measured in a logged-in www.kimi.com session — and its requests carry your page's own login token, read from the page's local storage at request time and held in memory only. If a conversation's body response ever says it holds only part of that conversation, Kimi refuses to archive it and lists it as a failure instead (`apps/extension/lib/backfill/enumerate.ts:3201-3263`; `apps/extension/lib/platform-auth.ts:268-305`; `apps/extension/lib/backfill/engine.ts:2595-2630`). Gemini's routes **were** measured as well (2026-09-14), and it is the one platform here whose conversation body arrives **in pages**: the leg follows the continuation token to the end, one request per page with a 1-3 second gap, and a conversation needing more than 20 pages is refused and listed as a failure rather than archived in part (`apps/extension/lib/backfill/enumerate.ts:3640-3768`; `apps/extension/lib/backfill/engine.ts:2418-2477`). Its requests carry three values from the page's own `WIZ_global_data` — read through the page-world hook at request time, memory only, attached to its two RPCs and nothing else (`apps/extension/lib/platform-auth.ts:679-779`). |


🔴 **The one precondition, before any tier applies: this leg fetches through a
page, so it needs one open.** The extension requests **no host permissions at
all** (`apps/extension/wxt.config.ts:122-125`), so a backfill request is made
from inside an open, logged-in page of the same platform — a same-origin request
using the login you already have — rather than by the extension itself. With no
page of that platform open there is no channel at all and the leg fetches
nothing: the popup says archiving is not running for want of a fetch channel,
and the alarm's last-tick trace names the same thing as `no-http-port`
(`apps/extension/lib/backfill/schedule.ts:212`;
`apps/extension/entrypoints/background.ts:848-850`). That page does not have to
be the conversation being archived — any open page of that platform answers —
and the leg carries on by itself as soon as one is open. One open page per
platform you want archived is the whole operational requirement; it is the price
of the permission model, not a fault to wait out.

🔴 There is no longer a middle tier: the "can list conversations but saves none of
their content" row was Perplexity's, and W84 (2026-09-23) filled that plan's body
segment in from a live probe, so every platform in the table above now fetches
content. The reason that middle tier existed at all is worth keeping in view,
because it is the same reason every completeness watchword in this section exists:
**a platform that can list your conversations is not automatically one that can
save them.** A wrong body profile would not error — it would silently archive only
the first few turns of every conversation while you believed you had them all, so
the body leg was only written once a completeness signal was *observed*, not
guessed.

🔴 **The DeepSeek caveat, which exists for the same reason.** DeepSeek fetches
bodies, and whether its body endpoint pages or truncates a long conversation is
**still not settled by any source** — but the extension no longer assumes an
answer. The response carries a tree:
`chat_session.current_message_id` names the newest message of the branch you were
looking at, and every message names its `parent_id`. The extension walks that
chain and archives the body **only when the walk closes at a root**; a response
that came back short is **not archived** — it is recorded as a failure with its
own reason code and the leg carries on, rather than being stored as a complete
conversation (`apps/extension/lib/backfill/enumerate.ts:2758-2785`). The evidence
for the endpoint itself is solid — it is the route DeepSeek's own page calls over
XHR when a user opens a past conversation in a real logged-in session, and several
mutually independent open-source exporters request the same route
(`apps/extension/lib/backfill/enumerate.ts:2742-2756`). What the check cannot
prove is written down too: it shows the response is closed under the branch you
were looking at, not under every discarded sibling branch, and it cannot catch a
server that truncates a body *and* rewrites the boundary message's `parent_id` to
`null` so the chain looks rooted. That is the residual, and it is why the
completeness question is still called open rather than answered.

🔴 **Perplexity's body leg is filled in, and it carries the same honesty Perplexity
used to be the exception for.** Passive capture names and delivers the
conversation you have open; backfill now lists your past conversations **and**
fetches their content, one `GET /rest/thread/<slug>` per conversation
(`apps/extension/lib/backfill/enumerate.ts:2919-2930`). What W84 observed, and
what the earlier refusals were waiting for, is a **completeness signal at the top
level of the body**: a 2026-09-23 logged-in probe of one thread returned
`has_next_page` (boolean) and `next_cursor` (string or null), present under both
the schematized and the minimal parameter set, and an `offset=500` request came
back byte-identical. So the extension does not guess whether a single response
holds a whole conversation: a body is archived **only when the response declares
there is no more**; a body that declares more is **refused** — recorded as a
failure with its own reason code, with nothing archived, and the leg carries on
with the next conversation (`apps/extension/lib/backfill/enumerate.ts:2267-2311`);
a body with no `entries` is the unverified-empty case, never a confirmed receipt;
and a body with no signal at all halts the leg `shape-changed` rather than being
read as complete. 🔴 **Unverified, written down rather than hidden:** the observed
thread had one entry, so whether a genuinely long thread answers
`has_next_page:true` (or a non-null `next_cursor`) when it truncates was not
directly observed — the completeness rule refuses when the signal says more and
treats the response as whole when it says no more, and no complete Perplexity
backfill has been observed in a real browser, so read the row as *implemented,
not verified*.

The popup shows these three tiers in the same terms as the table above
(`apps/extension/lib/popup-view.ts:938-951`).

(**Passive capture is not affected by this table:** the passive-capture criteria
for the seven platforms above are each registered in the table at
`apps/extension/lib/contract.ts:315-768`, a separate matter from backfill.)

---

## 2. Install the CLI

There are two paths, and "you have to compile first" is not one of them.

**Download the prebuilt binary.** A Release carries one binary per platform its
build produced, plus a `SHA256SUMS` over them
(<https://github.com/dimpurr/chat-stasher/releases>). This URL serves
`scripts/install.sh` from `main`, and is the installer:

```sh
curl -fsSL https://chatstasher.com/install.sh | sh
```

It detects your platform, reads the Release's `SHA256SUMS` **before** it
downloads anything, downloads that platform's artifact, checks it against that
manifest, and starts it once before moving it into place — so a binary that
matched its checksum but cannot start on your machine is refused rather than
installed, and whatever was installed there before is left alone. macOS
(`darwin-arm64`, `darwin-x86_64`) and Linux (`linux-x86_64`, `linux-arm64`,
static musl builds) are the platforms it has an install path for. Windows is not
one of them: this installer is a POSIX shell script, so that branch prints the
`.exe` release asset to download instead.

🔴 **On Linux that command refuses today, and the refusal is deliberate.** The
installer pins the newest *stable* release, which is v0.4.0, and v0.4.0's assets
are two macOS binaries, the extension zip and `SHA256SUMS` — it carries no Linux
binary at all (`gh release view v0.4.0`, 2026-09-25). The Linux builds exist in
the release workflow (`.github/workflows/release.yml`), but no version tag has
been pushed since they were added, so no Release holds one and there is nothing
for this path to install. Rather than downloading a wrong file or writing a
binary that cannot run, the installer reads the Release's own manifest first and
reports what is missing: the release it asked for, the artifact it needed
(`chat-stasher-linux-x86_64`), the assets that release *does* carry, and the two
ways forward — name a version that has one once a Release carries it
(`CHAT_STASHER_VERSION=<version>`, then run the same command), or use the source
path below, which needs no release artifact. **Building from source is the only
one of the two that works for Linux today**: the npm packages are assembled from
a Release's assets as well, and `scripts/npm/assemble.mjs` refuses to build a
platform package whose binary the release does not carry.

**Or compile from source**, which is the path below and needs no release
artifact:

```sh
git clone <repository-url> <your-directory>
cd <your-directory>
cargo build --release
```

⚠️ Which of those binaries has been observed to build and run is in section 8 —
"a Release carries it" and "it runs on your machine" are two different claims.
The Homebrew formula in this repository (`homebrew/chat-stasher.rb`) has no tap
to be published to yet (`dimpurr/homebrew-chat-stasher` did not exist when this
was checked on 2026-09-25), so `brew install` is not a channel you can use
today.

- You need the Rust toolchain (`cargo`). **The package manifest does not declare a
  minimum Rust version** (`crates/chat-stasher/Cargo.toml:1-6`). Which exact
  version compiles — **unverified**.
- The build output is at `target/release/chat-stasher`.

Then write a config:

```sh
chat-stasher init
```

`init` writes a commented default config only when the config does **not**
already exist; it is non-destructive (`crates/chat-stasher/src/main.rs:149-150`).
The config file lives at `~/.config/chat-stasher/config.toml`, or under
`XDG_CONFIG_HOME` if you have set it (`crates/chat-stasher/src/config.rs:23,638-649`).

🔴 **A config file that exists has to be valid, and the tool will not pretend
otherwise.** If it does not parse, if a value has the wrong type, or if a path in
it cannot be resolved, every command that reads it stops with **exit code `3`**
and prints the file, the position and the reason
(`crates/chat-stasher/src/config.rs:352-364,715-722`). It does **not** warn
and continue on the built-in defaults: those defaults declare no destination, so a
scheduled `push` would then run exactly as if you had never declared one, and the
archive would quietly stop being copied anywhere
(`crates/chat-stasher/src/main.rs:7736-7748`).

Two exceptions, and only two. `doctor` is the one command that keeps going — it
reports the error and lists the checks it therefore could not perform, so "no
destination declared" is never printed as a finding about a config nobody read
(`crates/chat-stasher/src/doctor.rs:1153-1181,1186-1192`). And an **absent** config file is a
different state altogether, not an error: that is the normal first run, and it
does use the defaults (`crates/chat-stasher/src/config.rs:352-359`). If you want
the defaults back, move the file aside rather than leaving a broken one in place.

---

## 3. Install the browser extension

**It is not yet on any app store** (see section 6 for details). Stable releases
include a stable-channel extension zip named `chat-stasher-extension-X.Y.Z.zip`.
Download that asset from the GitHub Release, unzip it, then open your browser's
extension management page, enable developer mode, choose **Load unpacked**, and
select the extracted extension directory. The zip is already built for the
stable channel.

You can also build from source:

```sh
cd apps/extension
pnpm install
pnpm build            # Chrome/Edge and other Chromium-based browsers
pnpm build:firefox    # Firefox
```

(The script names come from `apps/extension/package.json:8-19`. You need Node
and pnpm; **the exact minimum versions are not declared in the repository —
unverified**.)

A source build lands in `apps/extension/.output/` (excluded by the root ignore
rule at `.gitignore:15` and the extension ignore rule at
`apps/extension/.gitignore:11`). Load that directory with your browser's **Load
unpacked** menu. The exact menu path varies by browser and is unverified.

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
(`crates/chat-stasher/src/nativehost.rs:985-996`). The stage is the same staging
directory you use for `collect` / `seal` / `ingest`.

The command is idempotent — run it twice and there is exactly one manifest per
browser, byte-identical, exit 0 both times — and it prints every path it wrote,
left alone, skipped or removed, absolutely (`crates/chat-stasher/src/main.rs:828-850`).
It is per-user; nothing needs elevation. `--uninstall` removes exactly the files
it wrote and nothing else.

**What the browser asks you at install time.** Registering the host does not
remove any browser prompt, but it changes which one you see. The extension
declares `nativeMessaging`, so Chrome shows *"communicate with cooperating
native applications"* on its details page. It no longer declares `downloads`, so
the *"Manage your downloads"* warning is gone
(`apps/extension/wxt.config.ts:125`).

### 3.2 Confirm the popup says "connected"

Click the extension's toolbar icon. The popup asks the host one `hello` question
and renders the answer — **the stage it writes to, the machine id, and the host
version** — or the reason it could not, with the command that fixes it
(`apps/extension/lib/ui-strings.ts:80-100`;
`apps/extension/entrypoints/background.ts:634-641`).

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
have been sent to the host (`apps/extension/lib/outbox.ts:440-476`).

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
the outbox (`apps/extension/lib/outbox.ts:458-465`).

---

## 4. One-time setup checklist

The following things, you do **once at install time and then never again**.

### 4.1 🔴 Decide the stage directory, and keep it

The `--stage` you gave `install-native-host` (section 3.1) is the same directory
`collect`, `seal` and `ingest` write sealed shards into. It is a real directory
on your disk, and it must exist *before* you point the host at it: the host
never creates a stage, and a stage that appears because a host was pointed at it
is a stage nothing pushes (`crates/chat-stasher/src/nativehost.rs:985-996`).

Two properties of that directory, both from
[`contracts/nativehost-protocol.md`](../contracts/nativehost-protocol.md):

- **The host and `ingest` take an exclusive lock on `<stage>/.ingest.lock`
  before they allocate a shard sequence number**, so two browsers, two profiles,
  or a host racing a manual `ingest` cannot pick the same number. The wait is
  bounded at 10 seconds, and a timeout comes back as a `stage-unavailable` the
  extension retries (`crates/chat-stasher/src/inbox.rs:66-68`, `:992-1020`).
- **A stage the host cannot use is reported, not replaced.** A missing or
  relative `[native_host] stage` is a `config` refusal, and a path that is not a
  directory is `stage-unavailable` (`crates/chat-stasher/src/nativehost.rs:930-997`);
  if the seal itself fails, a lock-wait timeout is `stage-unavailable` and any
  other write error is `io`, and neither acknowledges anything
  (`crates/chat-stasher/src/nativehost.rs:1192-1196`). In every case the reason
  names the fix.

Put it somewhere you will not delete: these shards are the archive's input, and
`push` is what moves them into the encrypted repository.

**The host never invents a machine identity either.** It resolves the machine id
exactly as `ingest` does, and if there is none it refuses with a `config` `nack`
that names the fix, rather than minting a second identity — which would silently
put every delivered shard in a different machine's archive partition
(`crates/chat-stasher/src/nativehost.rs:1002-1031`). Run any archiving command
once from your shell before registering the host.

### 4.2 Run `chat-stasher init` once

See section 2. If you already did it, you do not need to do it again.

### 4.3 Decide where the archive lives, and **back up your master key file**

The archive's destination is decided by your config and command-line arguments
— a local path, or a backend you configure yourself. `push` / `read` / `verify`
read the repository and key file you select in config or arguments
(`crates/chat-stasher/src/main.rs:279-311,373-414,427-476`).

🔴 **The master key file is the only key. Lose it and the archive can never be
read again; there is no way to recover it.** The source's own words are "The
masterkey is the repository's only key — losing it means the repo is unreadable
forever" (`crates/chat-stasher/src/store.rs:1271-1273`). The key file is written
with owner-only-readable permissions, on platforms that can express them
(`crates/chat-stasher/src/store.rs:1371-1379`).

**Make a copy of it somewhere else right now.** No one can do this for you.

### 4.4 🔴 A remote destination: the first connection needs a human

Skip this if your archive lives on a local path. It applies when `repo` names a
remote backend such as `opendal:sftp` — the options you write under
`[destinations.<name>.options]` are forwarded verbatim to the backend
(`crates/chat-stasher/src/store.rs:153-156`, `:306-310`; the config field itself
is `crates/chat-stasher/src/config.rs:227-228`).

**Why this step exists.** A remote destination is reached by running the system
`ssh` client. The first time it meets a host it has no record of, it refuses:
that host's key is not in `~/.ssh/known_hosts`, so the key the server just
presented has nothing to be compared against. **That refusal is the feature.**
It is the one moment at which "is this really my storage box?" can be answered
by you rather than by whoever is on the network path.

**This tool never answers it for you.** `--trust-host` is the only thing in the
program that writes to `known_hosts`
(`crates/chat-stasher/src/main.rs:3959-3972`); without it, an unattended
scheduled run that meets a new host stops instead of quietly trusting it.

**What you see when it happens.** `dest-init` connects once, read-only, before
it does anything else (`crates/chat-stasher/src/main.rs:3995-4019`). An
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
to `~/.ssh/known_hosts` (`crates/chat-stasher/src/main.rs:3974-3983`;
`crates/chat-stasher/src/remote_err.rs:503-536`). The flag is for remote
destinations only: on a local path it is refused with exit code `2` rather than
silently doing nothing (`crates/chat-stasher/src/main.rs:3962-3970`).

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

`chat-stasher schedule` renders a launchd plist or systemd user service/timer.
Render-only remains the default. On macOS, `chat-stasher schedule install`
writes and loads the launchd agent, while `chat-stasher schedule uninstall`
unloads and removes it; both operations are idempotent. A configured
destination must be named explicitly, and repeating `--destination` creates
one independently named unit per destination. The embedded binary must be an
installed path outside `target/` (`crates/chat-stasher/src/main.rs:206-273`).
The generated template wraps a `run-once` command
(`crates/chat-stasher/src/main.rs:206-273`).

`run-once` is one complete collect-and-push pass; it exits when done, and
repeated invocation is safe (`crates/chat-stasher/src/main.rs:168-205`).

---

## 5. How to confirm it is working

Run this:

```sh
chat-stasher status
```

`status` is read-only. The source states its output boundary as: only ids,
paths, sizes, mtimes, and flags go to standard output; conversation content
does not (`crates/chat-stasher/src/main.rs:8085-8086`). This is the
source's self-description; we have not exhaustively verified every output path.

Its output has two parts. **The first line** is the timer health conclusion,
from the record left by the last `run-once`
(`crates/chat-stasher/src/main.rs:7593-7594`). These are the conclusions defined
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
(`crates/chat-stasher/src/main.rs:8266-8556`):

- When there are conversations: `[scan] N conversations (N compressed): <source> N · <source> N`
- When none are found: `[scan] No conversations found on this machine.`
- When a source root directory does not exist, an extra line: `[scan] Skipped N source root directories that do not exist.`
- When there are identified conversations that will not be archived: `⚠ N harnesses have identified conversations that collect will not archive.`
- Finally, a fixed last line: `Details (one line per session): chat-stasher status --sessions`

To see the per-session detail, add `--sessions`; that will be hundreds of lines
(`crates/chat-stasher/src/main.rs:335-337`).

**🔴 A common pitfall:** `status` exits with a **non-zero code** when it judges
the timer "unhealthy", **it exits with a non-zero code**
(`crates/chat-stasher/src/main.rs:7686-7693`). So "the command errored"
does not necessarily mean the command is broken; it may well be telling you the
timer has stopped. Please read that first line.

Its four exit codes are: `0` = the timer is judged healthy · `1` = the scan
finished, but the timer is judged unhealthy (including **never having run**) ·
`3` = the scan did not complete at all (the registry could not be read, for
example; in that case it has no conclusion about your machine) · `2` = usage
error. A config file it could not read is the same case, not a fifth one: nothing
was scanned, so nothing is claimed
(`crates/chat-stasher/src/main.rs:7935-7949`). **Note:** the entire report goes to
**stderr**, so a pipeline like
`chat-stasher status 2>&1 | head` gives you `head`'s exit code of 0, not its.
To see the exit code, do not pipe, or use `${PIPESTATUS[0]}`.

There is also a related command: `doctor`. It answers a different question —
**whether any tool is silently deleting your history**. Its report contains
only paths, counts, bytes, and timestamps
(`crates/chat-stasher/src/main.rs:415-426`).

`doctor` also **connects to each destination you declared**, read-only, and
reports what came back in three separate states rather than two: reached (and
whether a repository is there), not reached (with the classifier's verdict
attached), and not configured at all — a destination with no `repo` was never
dialled, and calling it "unreachable" would put a config mistake and a dead
network in one bucket (`crates/chat-stasher/src/doctor.rs:829-971`, `:1009-1069`).
When the repository is there it also reads each machine's `writer.json` and
reports the machines whose archived activity index was written by an older
`chat-stasher`, with the exact command that rebuilds each one
(`crates/chat-stasher/src/doctor.rs:851-971`).
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
(`crates/chat-stasher/src/main.rs:579-659`).

---

## 6. 🔴 Things that do not exist yet

This section is an **honest list**. Everything below is the current state we
confirmed in the code, not a temporary disclaimer.

- **There is no `restore` command — nothing puts a session back into a
  harness's own directory, and that is not in phase one.** The subcommand table
  has no `restore` entry (`crates/chat-stasher/src/main.rs:148-1106`). Getting
  content *out* does have a bulk path: `export --out <dir>` writes every session
  a time window selects to files in one command
  (`crates/chat-stasher/src/main.rs:579-659`), and `read` dumps **one**
  conversation to standard output at a time
  (`crates/chat-stasher/src/main.rs:370-414`). Restoring = for now you have to
  write your own script loop.

- **🔴 Lose the master key and there is no way to recover it.** There is no
  recovery process, no recovery code, no customer service. The source's own
  words are in section 4.3 (`crates/chat-stasher/src/store.rs:1271-1278`).

- **History backfill takes days, not minutes, and never runs on a fixed beat.**
  Content is fetched **at most 300–400 per day** (the day's cap is drawn once per
  local day and can never exceed 400; ADR-033 doubled the old 150–200 on
  2026-09-24), up to two conversations per round, with at least 20 seconds plus a
  random 0–25 seconds between two requests
  (`apps/extension/lib/backfill/pace.ts:121-132`, `:154-155`;
  `apps/extension/lib/backfill/schedule.ts:72`), and each round
  starts a random 5–10 minutes after the previous one
  (`apps/extension/lib/backfill/alarm.ts:99-100`). At that cap, a thousand
  conversations take 2.5–3.3 days. This is deliberately slow, not a bug.

- **Backfill is off by default.** The default is off
  (`apps/extension/lib/backfill/schedule.ts:50`), and the source states the
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
  several), and each conversation's body is walked from its newest message back to
  the branch root — a parent the response does not carry is that root (the shared
  tree-root id every real body omits, measured 2026-09-24), so the body archives;
  only a missing newest message or a cycle in the parent links is refused and
  listed as a failure rather than archived. That organization is asked for
  **in the claude.ai page**, over the same channel the backfill fetches through,
  and only when it is actually needed: when you press the start button for that
  platform, and on a wake-up whose recorded scope is not an organization yet.
  Resolving costs at most one extra request — `GET /api/organizations`, sent only
  when the page's own requests and the cookie both named none — and an account
  belonging to several organizations stops with a sentence telling you to open a
  conversation in the one you want archived, rather than picking one
  (`apps/extension/lib/backfill/claude-page.ts:70-148`). 🔴 **A Claude backfill
  checks every request against the organization established by the page. On a
  fresh `/new` page, it resolves that scope from an observed page request, the
  active-org cookie, or one unambiguous organizations response before allowing
  the list fetch. If the active organization differs from the stored target, that
  request is refused as `scope-mismatch`; the next tick asks the page again and
  adopts its answer. Separate organization targets keep separate progress records
  (`apps/extension/entrypoints/background.ts:1611-1692`). Perplexity now lists
  conversations **and** fetches their content — with the completeness gate
  described in section 1.1, where every platform's body leg (list from
  `apps/extension/lib/backfill/enumerate.ts:4590-4621`) is covered.

- **The extension is not on a store yet; you install it manually.** The
  repository has no store listing material and no store extension ID;
  `package.json` is marked `"private": true` (`apps/extension/package.json:4`),
  and the build scripts produce a local directory and a zip
  (`apps/extension/package.json:8-19`). See section 3 for how to install.

- **A captured conversation is plaintext until the host acknowledges it.** A
  live capture is written into the extension's own IndexedDB outbox before any
  delivery is attempted and deleted only on a matching `ack`
  (`apps/extension/lib/outbox.ts:310-378`, `:380-395`); the popup's export file
  contains the same bodies. Other programs running as you can read all of it.
  (The "Security and privacy" section of `README.md` says the same.)

- **Zed and Cursor conversation enumeration is not implemented** (see the
  "What this does not do / current limits" section of `README.md` and the
  `crates/chat-stasher/data/harness-registry-v1.json` it cites).

- **`schedule` render-only mode does not install the timer**; use the explicit
  macOS `schedule install` action when launchd should load the generated agent.
  `schedule uninstall` unloads and removes the matching agent. Linux systemd
  remains render-only in this command (`crates/chat-stasher/src/main.rs:206-273`).

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
point — collect, push, exit (`crates/chat-stasher/src/main.rs:168-205`). It does
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
| Whether Chrome shows the "communicate with cooperating native applications" note for this permission set | **Unverified** (the permission list is `apps/extension/wxt.config.ts:125`; we read the manifest, we did not install the build and look at the warnings Chrome renders) |
| Whether every browser's discovery directory is where `install-native-host` looks for it | **Partly verified** (the per-OS layout is in `crates/chat-stasher/src/nativehost.rs:204-320`; the command prints every path it wrote, left alone, skipped or removed, so you can check the one your browser reads) |
| Whether the popup's language follows your browser correctly on every browser | **Unverified** (the default locale is `en` with a `zh_CN` catalog, `apps/extension/wxt.config.ts:78`; we did not test every browser's locale resolution) |
| Each browser's menu path for "Load unpacked extension" | **Unverified** |
| The minimum Rust version to compile the CLI | **Unverified** (the repository does not declare `rust-version`) |
| The minimum Node / pnpm version to build the extension | **Unverified** (the repository does not declare it) |
| The concrete installation steps for a launchd timer | **Partly verified** (the command uses an injectable launchctl runner in tests; a real launchd session was not touched here) |
| How `known_hosts_strategy` behaves against a real server | **Partly verified** (the three values and their `StrictHostKeyChecking` equivalents were read from the pinned dependency's source — opendal-service-sftp 0.57.0 `src/backend.rs` lines 148-165 and the `openssh` crate it maps onto — but we have not exercised `add` or `accept` against a live host. Section 4.4 describes what each one gives up.) |
| Whether passive capture on Perplexity delivers the conversation it names | **Unverified** (reading the code, the conclusion is now "it recognizes the id and delivers"; see section 1. The route itself was read out of public source, not measured, and we have not tried it on a real page.) |
| Whether the DeepSeek / Perplexity / Grok conversation-list endpoints still look like this today | **Unverified** (from cross-checking multiple open-source implementations, not official documentation, and not tested with a logged-in session; `apps/extension/lib/backfill/enumerate.ts:2880-2932`, `:3052-3073`, `:3164-3225`. If the shape changes, it stops on the spot and leaves a trace, rather than producing fake progress. That trace carries the shape of the response that did not match — the key names, types and array lengths at the level that disagreed — and carries no conversation text, no id and no title from it (`apps/extension/lib/backfill/enumerate.ts:1351-1419`), so a shape change can be diagnosed from the trace itself instead of from a second logged-in session.) |
| Which Grok list cursor the real backend honours — an opaque `pageToken` or an integer `page` | **Unverified** (the sources disagree; `apps/extension/lib/backfill/enumerate.ts:3164-3225`. The extension does not choose: it hands back exactly what it was given, and a page that repeats what was already listed stops the leg and says the response shape changed, rather than being read as "no more conversations"; `apps/extension/lib/backfill/engine.ts:1836-1916`.) |
| Whether a **long** Grok conversation comes back complete from the backfill content endpoint | **Unverified** (its two-step route — a skeleton call then a content call — was cross-checked across implementations, but none of them pages the content call and this extension adds no paging, so a long conversation may be stored as only its first part; `apps/extension/lib/backfill/enumerate.ts:3164-3225`.) |
| Whether a **long** DeepSeek conversation comes back complete from the backfill body endpoint | **Not settled by any source — and checked rather than assumed** (the endpoint itself is well evidenced: it is the route DeepSeek's own page calls in a real logged-in browser session, and several independent open-source exporters request the same route; `apps/extension/lib/backfill/enumerate.ts:2790-2804`. None of the reviewed implementations pages it and this extension adds no paging. Instead of assuming a single response holds the whole conversation, the extension walks the response's own tree — `chat_session.current_message_id` back along `parent_id` to a root — and archives the body only if that walk closes; a body that came back short is not archived at all, it is recorded as a failure with its own reason code and the leg carries on; `apps/extension/lib/backfill/enumerate.ts:2806-2833`.) |
| Whether a **long** Kimi conversation comes back complete from the backfill body endpoint | **Unverified, and handled rather than guessed** (a logged-in session measured the route and five **short** conversations, none of which carried a page-token field; nothing here pages that endpoint. If a response ever does say it holds more of the conversation, that conversation is not archived at all — it is recorded as a failure with its own reason code and the leg moves on, because a truncated conversation stored as a complete one would be silent loss; `apps/extension/lib/backfill/enumerate.ts:3249-3311`; `apps/extension/lib/backfill/engine.ts:2617-2630`.) |
| Whether the Kimi gateway requires the two extra request headers the page sends, or whether they are merely what the page happens to send | **Unverified** (the page's requests were observed carrying `x-msh-platform` and `x-language` alongside the bearer token, so the backfill requests send them too — that they are *required* has not been tested; `apps/extension/lib/platform-auth.ts:268-305`.) |
| Whether a Kimi backfill run has ever completed end to end in a real browser | **Unverified** (implemented and wired to the host, like the other three; no complete run observed. See section 1.1.) |
| Whether a ChatGPT or DeepSeek backfill run has ever completed end to end in a real browser | **Unverified** (both legs are implemented and wired to the host, but no complete run has been observed in a real browser. See section 1.1.) |
| Whether the Linux and Windows binaries a Release will carry have ever been built, let alone run | **Unverified** (`.github/workflows/release.yml` builds one per platform on a version tag — static musl for `linux-x86_64` and `linux-arm64`, and `chat-stasher-windows-x86_64.exe`. The Linux jobs assert their output is statically linked and start it; the Windows job starts the `.exe`; the macOS job checks each Mach-O's architecture with `file`. No version tag has been pushed since those jobs were added, so no Release carries any of the three and none of them has been observed to build. The assets in v0.4.0 — two macOS binaries, the extension zip and `SHA256SUMS` — predate the change, and section 2's installer refuses on Linux for exactly that reason: it reads the release's own manifest first and reports that `chat-stasher-linux-x86_64` is not in it, rather than installing something that has not been built yet.) |

"Unverified" = we have not tested it; it does not mean it does not exist, and
it does not mean it does not work. The things in section 6 above that are
listed as **absent** are things we checked in the code and confirmed **really
do not exist** — please keep the two categories separate.
