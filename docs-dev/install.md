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
read-only (`crates/chat-stasher/src/main.rs:774`).

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
(`crates/chat-stasher/src/main.rs:910-952`). The protocol both sides implement
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
  (`apps/extension/lib/contract.ts:1155-1183`) — but this is still a conclusion
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
`apps/extension/entrypoints/background.ts:850-852`). That page does not have to
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
already exist; it is non-destructive (`crates/chat-stasher/src/main.rs:162-163`).
The config file lives at `~/.config/chat-stasher/config.toml`, or under
`XDG_CONFIG_HOME` if you have set it (`crates/chat-stasher/src/config.rs:23,823-834`).

🔴 **A config file that exists has to be valid, and the tool will not pretend
otherwise.** If it does not parse, if a value has the wrong type, or if a path in
it cannot be resolved, every command that reads it stops with **exit code `3`**
and prints the file, the position and the reason
(`crates/chat-stasher/src/config.rs:367-379,914-921`). It does **not** warn
and continue on the built-in defaults: those defaults declare no destination, so a
scheduled `push` would then run exactly as if you had never declared one, and the
archive would quietly stop being copied anywhere
(`crates/chat-stasher/src/main.rs:9839-9849`).

Two exceptions, and only two. `doctor` is the one command that keeps going — it
reports the error and lists the checks it therefore could not perform, so "no
destination declared" is never printed as a finding about a config nobody read
(`crates/chat-stasher/src/doctor.rs:1165-1205`). And an **absent** config file is a
different state altogether, not an error: that is the normal first run, and it
does use the defaults (`crates/chat-stasher/src/config.rs:367-374`). If you want
the defaults back, move the file aside rather than leaving a broken one in place.

---

## 3. Install the browser extension

🔴 **Install it once in every browser profile you chat in.** An extension is
installed into a single browser profile, not into the browser as a whole: the
extension management page (`chrome://extensions`, `edge://extensions`, …) lists
that profile's extensions only, so loading it in Chrome's *Personal* profile
leaves its *Work* profile completely uncovered: no capture, no backfill, nothing
in the popup. Section 3.1 is the other half of this and is **not** repeated per
profile: the host is registered once per machine, and every browser on it shares
that one registration.

What one install per profile means, once done:

- Each install captures the tabs of its own profile and nothing else.
- Each install keeps its **own** outbox, its own backfill progress and debt
  ledger, and its own speed preset, all in that profile's `storage.local`
  (`apps/extension/lib/backfill/store.ts:85-97`). One install cannot read
  another's, and the popup's counts are that install's own.
- Every install in every browser delivers into the **same stage**, so the
  archive stays one archive: the stage is a property of your config, not of an
  install (`crates/chat-stasher/src/nativehost.rs:922-997`).
- The popup's one host line is therefore **not** this install's number: the
  host's `summary` counts the sessions in the stage directory it resolves from
  your config, wherever they came from
  (`crates/chat-stasher/src/nativehost.rs:1646-1656`, `:1383`).

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
rule at `.gitignore:12` and the extension ignore rule at
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
(`crates/chat-stasher/src/nativehost.rs:1338-1349`). The stage is the same staging
directory you use for `collect` / `seal` / `ingest`.

The command is idempotent — run it twice and there is exactly one manifest per
browser, byte-identical, exit 0 both times — and it prints every path it wrote,
left alone, skipped or removed, absolutely (`crates/chat-stasher/src/main.rs:887-909`).
It is per-user; nothing needs elevation. `--uninstall` removes exactly the files
it wrote and nothing else.

🔴 **The host is one program per machine and per user account, shared by every
browser and every profile on it.** Run this command once, not once per profile.
Four properties of the registration say so, and each is the reason for one
sentence the surrounding documents have to get right:

- **The manifest is per browser, not per profile.** On macOS and Linux it goes
  into the browser's own discovery directory, which is inside the browser's
  folder and beside its profile directories, not inside any one of them
  (`Google/Chrome/NativeMessagingHosts` on macOS), and every profile of that
  browser reads the same file (`crates/chat-stasher/src/nativehost.rs:253-320`).
  On Windows there is one JSON per browser plus a registry value that points at
  it (`crates/chat-stasher/src/nativehost.rs:292-296`).
- **All of them point at the same binary and the same stage.** The manifest
  records this executable's absolute path, and the stage lives in your one config
  as `[native_host] stage`, which the host resolves on every launch
  (`crates/chat-stasher/src/main.rs:1970-1985`;
  `crates/chat-stasher/src/nativehost.rs:922-997`). So several installs deliver
  into one stage, which is what keeps the archive one archive.
- **The default browser set is "whatever is installed here", sampled now.** With
  no `--browser`, the command walks every browser it knows a path for and skips
  the ones whose data directory is absent, saying so per browser
  (`crates/chat-stasher/src/main.rs:1912-1922`;
  `crates/chat-stasher/src/nativehost.rs:464-466`). A browser you install later
  is therefore not registered until the command is run again.
- **`--uninstall` is the whole registration, not one profile's share of it.** It
  removes the manifest for every browser it knows in one pass (`--browser
  chrome` limits it to the ones named, and `--stage` cannot be combined with it
  at all, exit 2), and it leaves the config, the stage, the sealed captures and
  every other vendor's manifest untouched
  (`crates/chat-stasher/src/main.rs:1856-1862`, `:2012-2050`, `:2119-2125`).
  "It is per-user" does **not** mean "it is per profile": removing the extension
  from one profile is done on that profile's own extension page, and doing it
  with `--uninstall` takes the channel away from the profiles you kept, whose
  captures then wait in their own outboxes instead of being delivered.

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
`apps/extension/entrypoints/background.ts:636-643`).

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
is a stage nothing pushes (`crates/chat-stasher/src/nativehost.rs:1338-1349`).

Two properties of that directory, both from
[`contracts/nativehost-protocol.md`](../contracts/nativehost-protocol.md):

- **The host and `ingest` take an exclusive lock on `<stage>/.ingest.lock`
  before they allocate a shard sequence number**, so two browsers, two profiles,
  or a host racing a manual `ingest` cannot pick the same number. The wait is
  bounded at 10 seconds, and a timeout comes back as a `stage-unavailable` the
  extension retries (`crates/chat-stasher/src/inbox.rs:66-68`, `:1002-1030`).
- **A stage the host cannot use is reported, not replaced.** A missing or
  relative `[native_host] stage` is a `config` refusal, and a path that is not a
  directory is `stage-unavailable` (`crates/chat-stasher/src/nativehost.rs:1283-1350`);
  if the seal itself fails, a lock-wait timeout is `stage-unavailable` and any
  other write error is `io`, and neither acknowledges anything
  (`crates/chat-stasher/src/nativehost.rs:1545-1549`). In every case the reason
  names the fix.

Put it somewhere you will not delete: these shards are the archive's input, and
`push` is what moves them into the encrypted repository.

**The host never invents a machine identity either.** It resolves the machine id
exactly as `ingest` does, and if there is none it refuses with a `config` `nack`
that names the fix, rather than minting a second identity — which would silently
put every delivered shard in a different machine's archive partition
(`crates/chat-stasher/src/nativehost.rs:1355-1384`). Run any archiving command
once from your shell before registering the host.

### 4.2 Run `chat-stasher init` once

See section 2. If you already did it, you do not need to do it again.

### 4.3 Decide where the archive lives, and **back up your master key file**

The archive's destination is decided by your config and command-line arguments
— a local path, or a backend you configure yourself. `push` / `read` / `verify`
read the repository and key file you select in config or arguments
(`crates/chat-stasher/src/main.rs:324-356,418-459,472-521`).

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
(`crates/chat-stasher/src/store.rs:153-156`, `:306-310`, `:1549-1554`; the config
field itself is `crates/chat-stasher/src/config.rs:268-269`).

**Why this step exists.** A remote destination is reached by running the system
`ssh` client. The first time it meets a host it has no record of, it refuses:
that host's key is not in `~/.ssh/known_hosts`, so the key the server just
presented has nothing to be compared against. **That refusal is the feature.**
It is the one moment at which "is this really my storage box?" can be answered
by you rather than by whoever is on the network path.

**This tool never answers it for you.** `--trust-host` is the only thing in the
program that writes to `known_hosts`
(`crates/chat-stasher/src/main.rs:4823-4836`); without it, an unattended
scheduled run that meets a new host stops instead of quietly trusting it.

**What you see when it happens.** `dest-init` connects once, read-only, before
it does anything else (`crates/chat-stasher/src/main.rs:4859-4883`). An
untrusted host stops the command there with exit code `3` — "did not finish
reading", which is *not* the same as "the destination is empty" — and prints
which host is untrusted, the fingerprints it received, and the next step
(`crates/chat-stasher/src/remote_err.rs:180-187`, `:465-494`).

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
to `~/.ssh/known_hosts` (`crates/chat-stasher/src/main.rs:4838-4847`;
`crates/chat-stasher/src/remote_err.rs:514-547`). The flag is for remote
destinations only: on a local path it is refused with exit code `2` rather than
silently doing nothing (`crates/chat-stasher/src/main.rs:4826-4834`).

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

### 4.5 An S3-compatible destination (tested with Cloudflare R2)

Skip this if your destination is a local path or an SSH host (§4.4). It applies
when `repo` names an S3 backend, spelled `opendal:s3`. The options you write
under `[destinations.<name>.options]` are forwarded verbatim to the backend
(`crates/chat-stasher/src/store.rs:153-156`, `:1549-1554`; the field itself is
`crates/chat-stasher/src/config.rs:268-269`), so the option names below belong
to the backend, not to this tool.

**What was tested, and where that stops.** The configuration below was exercised
end to end against Cloudflare R2: seeding one machine's partition, an ordinary
push, a read of three sessions back out of the destination, and an integrity
verification. `disable_ec2_metadata` was added to it after that run, when its
absence turned out to be a gap in this section rather than a choice; with both
credential switches in the file, verification layers 1 and 2 were then re-run
against the same destination and both reported `ok=true`, `findings=0`,
`errors=0`, `warns=0` — layer 2 with `read_data=true`, so the credentials
resolved and the requests were signed and served. No other S3-compatible service
was tested. Cloudflare's own compatibility page lists the headers and operations
R2 does not implement (<https://developers.cloudflare.com/r2/api/s3/api/>); the
pitfall list at the end of this section says which of them this configuration can
actually reach.

**The configuration.**

```toml
[destinations.<name>]
repo = "opendal:s3"
key_file = "~/stash/chat-stasher/masterkey-<name>.json"
connections = 4

[destinations.<name>.options]
bucket = "<your-bucket>"
endpoint = "https://<your-account-id>.r2.cloudflarestorage.com"
region = "auto"
root = "/chat-stasher/v1"
access_key_id = "env:CHAT_STASHER_<NAME>_ACCESS_KEY_ID"
secret_access_key = "env:CHAT_STASHER_<NAME>_SECRET_ACCESS_KEY"
disable_config_load = "true"
disable_ec2_metadata = "true"
```

`root` is the prefix inside the bucket that the repository lives under, so one
bucket can hold more than one destination. `key_file` is the destination's
master key and is **not** the S3 credential: back it up as §4.3 says, and give
each destination its own.

**`region` is required, and this backend will not guess it.** R2 is a
single-region service — Cloudflare's page gives the value and notes that an
empty value and `us-east-1` both alias to it: "the region for an R2 bucket is
`auto`". The backend reads `region` from the option, else from `AWS_REGION` /
`AWS_DEFAULT_REGION`, and otherwise fails to build with `region is missing.
Please find it by S3::detect_region() or set them in env.` (opendal-service-s3
0.57.0, the version this project pins, `src/backend.rs` lines 812-826). It does
ship an R2-aware `detect_region` (`src/backend.rs` lines 654-655) — but that is
a separate call this tool does not make, so write the value out.

**Two of the options here are about what you are asking the backend *not* to
do, and they are separate switches.** Left out, the backend looks for
credentials beyond the two you wrote, and it does so in two independent places.
`disable_config_load = "true"` removes the ambient AWS environment and the
`~/.aws/` profiles (opendal-service-s3 0.57.0 `src/backend.rs` lines 856-857;
the option's own description names `AWS_ACCESS_KEY_ID` and `~/.aws/config` at
`src/config.rs` lines 114-121). It does **not** remove the EC2 metadata service:
that is a separate switch, and `disable_ec2_metadata = "true"` is the one that
takes IMDSv2 out of the chain (same file, `src/backend.rs` lines 860-861). Set
both. On a machine that runs under an instance role, setting only the first
leaves the metadata service in the chain, so a missing or mistyped credential can
authenticate as that role — against whatever address the config names.

**`disable_config_load` also gates the endpoint, which is the same hazard
arriving through the address instead of the credential.** With it left out, an
absent `endpoint` is filled in from `AWS_ENDPOINT_URL`, `AWS_ENDPOINT` or
`AWS_S3_ENDPOINT` (`src/backend.rs` lines 830-839). With it set that fallback is
off — but an absent `endpoint` still does not fail: it becomes
`https://s3.amazonaws.com` (same file, line 517), AWS's own endpoint, carrying
your bucket name and your credentials. So write `endpoint` out, and do not count
on `dest-init` catching a missing one: AWS is reachable, and the request would go
there rather than being refused locally.

**Credentials: two shapes, one of them conditional.**

A value written literally is resolved immediately: put the two credentials in
`[destinations.<name>.options]` as they are, and keep the config file
owner-only-readable (`chmod 600`). That is the shape most S3 clients document,
and nothing about it is wrong — it is a secret on a disk.

A value spelled `env:NAME` is instead resolved at config load, out of the
process environment (`crates/chat-stasher/src/config.rs:1023`). The four ways
that can fail — the name is not a legal variable name, the variable is set but
empty, it is set to a value that is not valid Unicode, it is not set at all —
are four different messages, and none of them quotes the value
(`crates/chat-stasher/src/config.rs:942-959`; the warning is printed at `:901`).
A reference that cannot be resolved **removes that option** rather than
substituting an empty string, so the failure is a credential error, not a
silently-empty one.

🔴 **`env:NAME` means the variable must be in the environment of *every*
invocation, including anything a scheduler runs.** A scheduled run that does not
have it finds the destination unreachable — and `reclaim-stage`, which proves
every session against every destination before it deletes anything, then refuses
and deletes nothing. That is the safe direction, but it is silent apart from
that scheduler's own log. If a timer runs the CLI for you, give that job the
variables; a shell that needs them interactively does it like this, and note
that a `.env`-style file is not exported unless the file says `export`:

```sh
set -a; . ~/.config/chat-stasher/<name>.env; set +a
chat-stasher push --destination <name>
```

**Pitfalls, in the order you meet them.**

- **A token can be scoped to one bucket, and then account-level calls fail.**
  Measured against R2 with a token scoped to a single bucket: asking the account
  for its bucket list exits non-zero and reports a 403-family response, while
  listing and reading *inside* the configured bucket works normally. The
  destination is healthy; the failing call is one this tool never makes.
- **An unreachable endpoint is reported as a missing object.** Point the
  destination at a host that does not resolve and the backend's first sentence
  is `Path \`config\` does not exist.` — which reads as "the bucket has no
  repository in it". The real cause is the last line of the same error, a `dns
  error: failed to lookup address information ...`. Read the `Caused by:` tail.
  The CLI still classifies the destination as unreachable, which is what makes
  `reclaim-stage` refuse.
- **`dest-init` seeds this machine's partition, not a copy of the other
  destination.** When both destinations are reachable it reports how many
  sessions belong to another machine's partition: those are deliberately *not*
  copied, because copying them would re-attribute another machine's history to
  this one. They stay only where they already were. A second destination
  therefore does not start out equal to the first, and `dest-init` is not the
  tool that makes it so.
- **Two destinations are not expected to be equal, and equality is the wrong
  thing to check.** Each is the archive as of its own last push, so comparing
  them finds two kinds of difference and neither is a fault. Sessions that only
  one of them holds are the machinery working: the other machine's partitions
  live where they were made. Sessions *both* hold can differ too — a session
  still being written when one destination is pushed has fewer shards there and
  gets the rest at the next push. Measured between two destinations whose newest
  snapshots were 4.5 hours apart: 3,198 sessions in common, 3,186 identical in
  shard count and byte count, and 12 where the earlier-pushed copy was smaller —
  all 12 in that direction, none the other way, and ten of the twelve were the
  sessions whose last activity sits closest to the earlier snapshot's moment.
  What a healthy pair looks like is therefore *direction*, not equality: the
  later push holds at least as much, never less. `verify`'s layer 3 is where a
  real loss would show, and it compares against the stage rather than against
  the other destination.
- **Do not set `checksum_algorithm`.** R2 implements `CRC-64/NVME` for full
  objects and lists `CRC-32`, `CRC-32C`, `SHA-1` and `SHA-256` as composite-only
  — that is, not usable as whole-object checksums
  (<https://developers.cloudflare.com/r2/api/s3/api/>). Some S3 clients began
  sending checksum headers by default in 2025 and broke against R2 and other
  non-AWS stores; the JavaScript SDK's announcement issue is still open
  (<https://github.com/aws/aws-sdk-js-v3/issues/6810>, opened 2025-01-16, read
  2026-09-25). This backend sends none unless you ask (the option has no default
  in opendal-service-s3 0.57.0 `src/config.rs` lines 217-218), so leave it out.
- **Do not set `default_acl` or `enable_request_payer`.** They map onto headers
  R2 marks unimplemented — `x-amz-acl` and the `x-amz-grant-*` family, and
  `x-amz-request-payer` (<https://developers.cloudflare.com/r2/api/s3/api/>).
- **Verification reads a stage that is still moving.** If your sources are still
  being collected while `verify` runs, its layer-3 count can report a handful of
  sessions as `MISSING IN ARCHIVE` that a later run finds. Comparison layers 1
  and 2 are about the repository's own consistency; layer 3 is about the stage
  as it stood at that moment.
- **The address shape.** With `enable_virtual_host_style` unset — the default
  every run here used — requests go to `<endpoint>/<bucket>/…`, with the bucket
  as a path segment. Virtual-host style (`<bucket>.<endpoint>`) was not tested
  against R2.

**What you can check locally.** `chat-stasher dest-init --destination <name>
--stage <your-stage>` connects and reads before it writes: a wrong region, a
wrong endpoint or a credential that did not resolve stops it there, and it says
which. After that, `chat-stasher verify --destination <name>` re-reads what was
written and `chat-stasher read --destination <name>` reads a session back out.
Neither command needs a real conversation to be interesting — a destination that
was never seeded reports that it holds nothing, which is a different answer from
a destination that could not be reached.

### 4.6 Install a timer (optional, but this is the key to "install once and forget it")

`chat-stasher schedule` renders a launchd plist or systemd user service/timer.
Render-only remains the default. `chat-stasher schedule install` writes and
loads a launchd agent on macOS or writes and enables a systemd user timer on
Linux; `schedule uninstall` stops and removes the matching job. Both operations
are idempotent. Installation targets every configured destination by default;
repeating `--destination` selects a subset. The embedded binary must be an
installed path outside `target/` (`crates/chat-stasher/src/schedule.rs:508-649`).
The generated template wraps a `run-once` command
(`crates/chat-stasher/src/schedule.rs:197-301`).

`run-once` is one complete collect-and-push pass; it exits when done, and
repeated invocation is safe (`crates/chat-stasher/src/main.rs:213-250`).

---

## 5. How to confirm it is working

Run this:

```sh
chat-stasher status
```

`status` is read-only. The source states its output boundary as: only ids,
paths, sizes, mtimes, and flags go to standard output; conversation content
does not (`crates/chat-stasher/src/main.rs:13194-13207`). This is the
source's self-description; we have not exhaustively verified every output path.

Its output has two parts. **The first line** is the timer health conclusion,
from the record left by the last `run-once`
(`crates/chat-stasher/src/main.rs:12892-12894`). These are the conclusions defined
verbatim in the source (`crates/chat-stasher/src/runstate.rs:184-232`):

- No timer installed / never run successfully:
  `[run-once] No run has ever been recorded: run-once has never completed successfully on this machine (or the state directory was cleared). It is impossible to tell whether the timer is working.`
- Everything is normal (the numbers in `{}` are the real ones):
  `[run-once] Healthy: last run N minutes ago, took N ms, archived N shard(s), snapshot created.`
  (When there is nothing new, the ending is "no change, so no snapshot
  created".)
- The timer may have stopped:
  `[run-once] No run for N days (threshold N hours): the timer may have stopped; the last result was success (no change).`
- The last run failed:
  `[run-once] Last run failed: the <step> step errored N minutes ago, with no successful run since.`

**The second part** is the scan result. By default it is a fixed summary of a
few lines and does not flood the screen
(`crates/chat-stasher/src/main.rs:13202-13492`):

- When there are sessions: `[scan] N session(s) (N compressed): <source> N · <source> N`
- When none are found: `[scan] No sessions were found on this machine.`
- When a source root directory does not exist, an extra line: `[scan] skipped N non-existent source root(s).`
- When there are recognised sessions that will not be archived: `⚠ N harness(es) have recognised sessions that collect will not archive.`
- Finally, a fixed last line: `details (one line per session): chat-stasher status --sessions`

To see the per-session detail, add `--sessions`; that will be hundreds of lines
(`crates/chat-stasher/src/main.rs:380-382`).

**🔴 A common pitfall:** `status` exits with a **non-zero code** when it judges
the timer "unhealthy", **it exits with a non-zero code**
(`crates/chat-stasher/src/main.rs:12925-12966`). So "the command errored"
does not necessarily mean the command is broken; it may well be telling you the
timer has stopped. Please read that first line.

Its four exit codes are: `0` = the timer is judged healthy · `1` = the scan
finished, but the timer is judged unhealthy (including **never having run**) ·
`3` = the scan did not complete at all (the registry could not be read, for
example; in that case it has no conclusion about your machine) · `2` = usage
error. A config file it could not read is the same case, not a fifth one: nothing
was scanned, so nothing is claimed
(`crates/chat-stasher/src/main.rs:12869-12889`). **Note:** the human-readable report goes to
**stderr**, so a pipeline like
`chat-stasher status 2>&1 | head` gives you `head`'s exit code of 0, not its.
To see the exit code, do not pipe, or use `${PIPESTATUS[0]}`. With `--json`,
the JSON report is on stdout.

There is also a related command: `doctor`. It answers a different question —
**whether any tool is silently deleting your history**. Its report contains
only paths, counts, bytes, and timestamps
(`crates/chat-stasher/src/main.rs:460-471`).

`doctor` also **connects to each destination you declared**, read-only, and
reports what came back in three separate states rather than two: reached (and
whether a repository is there), not reached (with the classifier's verdict
attached), and not configured at all — a destination with no `repo` was never
dialled, and calling it "unreachable" would put a config mistake and a dead
network in one bucket (`crates/chat-stasher/src/doctor.rs:845-982,1020-1079`).
When the repository is there it also reads each machine's `writer.json` and
reports the machines whose archived activity index was written by an older
`chat-stasher`, with the exact command that rebuilds each one
(`crates/chat-stasher/src/doctor.rs:862-982`).
It creates nothing, so a destination it reports as "not there yet" is still not
created by running `doctor`. D10 also checks each destination's local FTS index
without connecting to it (`crates/chat-stasher/src/doctor.rs:1316,1836-1885`).
The destination probes are the one check that touches the network; see section
4.4 if it reports a host it cannot trust (`crates/chat-stasher/src/doctor.rs:845-982,1020-1079`).

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
(`crates/chat-stasher/src/main.rs:638-718`).

---

## 6. 🔴 Things that do not exist yet

This section is an **honest list**. Everything below is the current state we
confirmed in the code, not a temporary disclaimer.

- **There is no `restore` command — nothing puts a session back into a
  harness's own directory, and that is not in phase one.** The subcommand table
  has no `restore` entry (`crates/chat-stasher/src/main.rs:161-1170`). Getting
  content *out* does have a bulk path: `export --out <dir>` writes every session
  a time window selects to files in one command
  (`crates/chat-stasher/src/main.rs:638-718`), and `read` dumps **one**
  conversation to standard output at a time
  (`crates/chat-stasher/src/main.rs:415-459`). Restoring = for now you have to
  write your own script loop.

- **🔴 Lose the master key and there is no way to recover it.** There is no
  recovery process, no recovery code, no customer service. The source's own
  words are in section 4.3 (`crates/chat-stasher/src/store.rs:1271-1278`).

- **History backfill takes days, not minutes, and never runs on a fixed beat.**
  Content is fetched under a **daily cap drawn once per local day**, and the cap
  belongs to the speed preset in force. The shipped default is *gentle*, which
  draws **150–200** bodies a day and fetches **one** conversation per round
  (`apps/extension/lib/backfill/speed.ts:65`, `:92-136`;
  `apps/extension/lib/backfill/pace.ts:158-159`); *standard* draws 300–400 and
  fetches two (`apps/extension/lib/backfill/pace.ts:154-155`;
  `apps/extension/lib/backfill/schedule.ts:72`); *faster* draws 600–800 and
  fetches four. The gap between two requests is the same at all three: at least
  20 seconds plus a random 0–25 seconds between two bodies
  (`apps/extension/lib/backfill/pace.ts:121-132`), 2 plus 0–4 seconds between two
  list pages (`apps/extension/lib/backfill/pace.ts:109-119`), and each round
  starts a random 5–10 minutes after the previous one
  (`apps/extension/lib/backfill/alarm.ts:99-100`). At the *gentle* cap a thousand
  conversations take 5–6.7 days; at *standard*, 2.5–3.3. This is deliberately
  slow, not a bug.

- 🔴 **And every number above is per install, not per account.** The preset, the
  day's draw, the day's counter and the request anchors all live in that
  profile's own `storage.local`
  (`apps/extension/lib/backfill/speed.ts:59`;
  `apps/extension/lib/backfill/store.ts:85-97`), which no other install can read.
  N profiles with backfill on are therefore N independent schedules, and the
  account sees about N times the band above: three profiles at *faster* is up to
  1,800–2,400 bodies a day against one account. **Nothing coordinates them in
  this version**: the installs do not talk to each other, and there is no server
  between them. Nor does the extra traffic buy extra coverage: two installs signed
  into the same account list the same conversations and both deliver into the
  same stage, so the second one fetches what the first will fetch anyway. Read
  the section above as *per install* every time it states a rate.

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
  (`apps/extension/entrypoints/background.ts:1613-1694`). Perplexity now lists
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
  `schedule install` action to load a launchd agent on macOS or enable the
  systemd user timer on Linux. `schedule uninstall` stops and removes the
  matching job (`crates/chat-stasher/src/schedule.rs:508-649`).

---

## 7. One-time, or something you keep doing?

This is the product's core promise, so say it clearly:

**At install time you do a few things by hand. After that, you never have to
touch it again.**

**Do once** (the ones in section 4):

- 🔴 Decide the stage directory and register the Native Messaging host
  (section 3.1), then confirm the popup says "connected" (section 3.2)
- 🔴 Load the extension in **every browser profile you chat in** (section 3), and
  check the popup once per profile. This is the one item on this list whose count
  is not one: the host half is registered once for the machine, but the extension
  half is one install per profile, and a profile you skip captures nothing.
- `chat-stasher init`
- Decide where the archive lives
- 🔴 Back up the master key file
- 🔴 If the destination is remote, trust its host key once (section 4.4)
- Install the timer

**Then it runs automatically:** the timer runs `run-once` at each scheduled
point — collect, push, exit (`crates/chat-stasher/src/main.rs:213-250`). It does
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
| Whether every browser's discovery directory is where `install-native-host` looks for it | **Partly verified** (the per-OS layout is in `crates/chat-stasher/src/nativehost.rs:411-622`; the command prints every path it wrote, left alone, skipped or removed, so you can check the one your browser reads) |
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
| Whether an S3-compatible service other than the one tested behaves the same way — including its multipart and virtual-host behaviour | **Unverified** (section 4.5's configuration was exercised end to end against one service; the option names are the pinned backend's, but no second service was tried, and virtual-host addressing was left at its default) |

"Unverified" = we have not tested it; it does not mean it does not exist, and
it does not mean it does not work. The things in section 6 above that are
listed as **absent** are things we checked in the code and confirmed **really
do not exist** — please keep the two categories separate.
