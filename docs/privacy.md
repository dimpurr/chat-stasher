# Privacy Policy — Chat Stasher

**Last updated: 2026-08-17.**

This policy covers the **Chat Stasher browser extension** and the **`chat-stasher`
command-line tool**. Together they copy your own AI-chat conversations into an
encrypted archive on storage you choose.

Every factual claim below about what the software does cites a file and line in
this repository, in the form `path:line`. Line numbers were checked against
commit `7253d1f`; they drift as the code changes. If a citation no longer lands
where this document says it does, **trust the code and treat the sentence as
unverified**.

If you want the longer, harsher version of this — organised as *who can see
what*, including the parts we do not defend — read
[`docs/threat-model.md`](threat-model.md). This policy is the short answer;
that document is the honest one.

---

## Summary of key points

- **There is no Chat Stasher server.** No account, no sign-up, no sync service.
  Your conversations never pass through any system we operate, because no such
  system exists in this design.
- **We receive nothing.** Not your conversations, not your email, not your IP
  address, not usage statistics, not crash reports, not even the fact that you
  installed this.
- **The extension runs on six chat platforms and nowhere else** — a fixed list
  compiled into the code, not a wildcard. See
  [section 5](#5-where-the-extension-runs) for the exact origins.
- **Running on a site is not the same as backing up your history there.** The
  optional backfill feature can actually recover past conversation text on
  **ChatGPT only**. On **DeepSeek** and **Perplexity** it lists your
  conversations and saves **none of their content**; on Gemini, Claude, and Kimi
  it does nothing at all. See
  [section 5](#5-where-the-extension-runs).
- **Everything is stored on your machine or at a destination you configure**
  (a local disk, or a remote store whose credentials only you hold).
- **There is a known plaintext window.** Captured conversations sit
  *unencrypted* in your browser's download directory until the CLI consumes
  them. We do not encrypt that file, restrict its permissions, or shorten that
  window. See [Known weaknesses](#known-weaknesses).
- **Contact: `work@team.iopho.com`.**

## Contents

1. [How the data actually moves](#1-how-the-data-actually-moves)
2. [What we collect](#2-what-we-collect)
3. [Where your data is stored](#3-where-your-data-is-stored)
4. [Who your data is shared with](#4-who-your-data-is-shared-with)
5. [Where the extension runs](#5-where-the-extension-runs)
6. [What each permission is for](#6-what-each-permission-is-for)
7. [Cookies, analytics, and tracking](#7-cookies-analytics-and-tracking)
8. [We are not an AI service](#8-we-are-not-an-ai-service)
9. [How long data is kept, and how to delete it](#9-how-long-data-is-kept-and-how-to-delete-it)
10. [Known weaknesses](#known-weaknesses)
11. [Children](#11-children)
12. [Legal status of this policy](#12-legal-status-of-this-policy)
13. [Changes to this policy](#13-changes-to-this-policy)
14. [Contact](#14-contact)
15. [What this policy does not establish](#15-what-this-policy-does-not-establish)

---

## 1. How the data actually moves

Read this first. Every "we do not …" later in this document is a consequence of
this path, and you should be able to check the path yourself rather than believe
the sentence.

1. **Capture.** A content script, injected only on a fixed list of chat origins,
   wraps `fetch` in the page and keeps a **clone** of the response text of
   requests **the page itself already made** in your already-logged-in session
   (`apps/extension/lib/page-hook.ts:308`, `:332`, `:356-373`). Only responses
   matching a known platform route are kept
   (`apps/extension/lib/contract.ts:67-289`, `:463-479`).
2. **Write to disk.** The extension writes that text, as a JSON bundle, to a
   file **in your own browser's download directory**, under `chat-stasher/inbox/`
   (`apps/extension/lib/contract.ts:308`;
   `apps/extension/lib/download.ts:91-93`). The delivery channel is the
   browser's own download API (`apps/extension/lib/download.ts:117-135`).
3. **Ingest.** The `chat-stasher` CLI, which you run yourself on your own
   machine, reads those files and turns them into sealed shards in a staging
   directory (`crates/chat-stasher/src/main.rs:457-479`).
4. **Push.** `push` writes the staged shards into a `rustic` repository —
   encrypted — at a destination **you** configure, local or remote
   (`crates/chat-stasher/src/main.rs:132-169`;
   `crates/chat-stasher/src/store.rs:261-296`).

Steps 1–3 happen entirely on your machine, in plaintext. Step 4 is the only
step that can involve a network, and the only destination it can reach is the
one you put in your own config.

**There is no step in which anything is sent to the authors of this software.**
That is not a promise we are keeping — it is a property of there being no such
link in the code.

## 2. What we collect

**We collect nothing.** No personal information, no conversation content, no
identifiers, no analytics, no diagnostics.

Because "we do not collect" is the easiest sentence in any privacy policy to
write and the hardest to believe, here is how **you** can check it without
taking our word for it:

- **Check the permission list on the shipped extension.** Open
  `chrome://extensions` (or `about:addons`) and look at what Chat Stasher asks
  for. It requests exactly four permissions — `downloads`, `storage`, `alarms`,
  `nativeMessaging` — and **no host permissions at all**
  (`apps/extension/wxt.config.ts:36`). An
  extension with no host permissions cannot make requests to a server of ours;
  the only network the code can touch is inside the pages it is already injected
  into. There is no origin belonging to this project anywhere in the extension.
- **Check the network tab.** Open your browser's developer tools on a chat page
  and watch the requests. Capture is passive: the hook reads a clone of a
  response the page already fetched, and adds no request of its own
  (`apps/extension/lib/page-hook.ts:332`, `:356-373`). The one feature that does
  add requests, backfill, is off unless you turn it on — see
  [section 4](#4-who-your-data-is-shared-with).
- **Check the code for a tracker.** Searching the extension and CLI sources for
  `analytics`, `telemetry`, `sentry`, `gtag`, `mixpanel`, `posthog`, and
  `amplitude` returns **zero matches** in `apps/extension/lib`,
  `apps/extension/entrypoints`, and `crates/chat-stasher/src`. There is no
  analytics SDK to configure, disable, or trust.
- **Check the Firefox data-collection declaration.** The add-on declares
  Mozilla's data-collection field as `none` (`apps/extension/wxt.config.ts:59`).
  The extension is not listed on addons.mozilla.org yet, so today you read that
  declaration in the source or in the manifest of a build you made yourself.
  Once it is listed, AMO publishes the declaration alongside the add-on and it
  is binding on us; if it were false, that would be a policy violation you
  could report.

The honest limit on all four checks: they tell you about **this** version, built
from **this** source. They say nothing about a future version, and nothing about
a build you did not compile yourself. See
[section 15](#15-what-this-policy-does-not-establish).

## 3. Where your data is stored

Three places, all of them yours.

**a. Your browser's download directory.** Each captured session is written as an
ordinary JSON file at `chat-stasher/inbox/<platform>-<sessionId>.json`
(`apps/extension/lib/contract.ts:308`; `apps/extension/lib/download.ts:91-93`).
It contains the raw response body — that is, the conversation itself. **It is
not encrypted.** See [Known weaknesses](#known-weaknesses).

Your browser also keeps a **download-history entry** for that file. The filename
embeds the platform name and the session id, so the history is a record of
*which* conversations you archived (metadata, not content). We erase only the
temporary `.part` entry from the download shelf, not the final one
(`apps/extension/lib/download.ts:138-139`).

**b. Your browser's local extension storage** (`storage.local`, never
`storage.sync`: no `storage.sync` call exists anywhere under `apps/extension`,
so nothing here is synced to a browser account by this extension).
What is kept there:

| Key | What it holds | Citation |
|---|---|---|
| `cs_count`, `cs_last_capture_at` | A capture counter and a timestamp, for the toolbar badge | `apps/extension/lib/badge.ts:22-23` |
| `cs_download_guard_v…` | Recent download outcomes, so a stalled download pauses the tool | `apps/extension/lib/download-guard.ts:37` |
| `cs_backfill_enabled_v1` | Whether you turned the history-backfill feature on | `apps/extension/lib/backfill/schedule.ts:24` |
| `cs_backfill_targets_v1`, `cs_backfill_tabs_v1` | Which site/tab the backfill timer should wake up for | `apps/extension/lib/backfill/alarm.ts:16-17`; `apps/extension/lib/backfill/tab-port.ts:51` |
| `cs_backfill_v1:<platform>:<scope>` | The backfill progress set: **conversation/session ids** already archived and still pending, plus counters | `apps/extension/lib/backfill/types.ts:67-96`, `:116-118` |

Two things in that table deserve to be called out rather than buried:

- The progress set stores **session ids** — not conversation text, but a list of
  which conversations exist and which you have archived.
- The `<scope>` part of that key is your **account identifier on that platform**
  when the extension could find one in a response body (a user id, an email
  address, or a handle), and the literal string `default` when it could not
  (`apps/extension/entrypoints/background.ts:299`;
  `apps/extension/lib/contract.ts:534-538`, `:587-602`). It is used to keep two
  machines' archives of the same account from colliding. It stays in your local
  browser storage and is written into your own archive; it is not transmitted
  anywhere by this extension.

**c. Your archive destination.** Whatever you configured: a directory on your
own disk, or a remote store (S3, SFTP, and the like) whose credentials only you
hold (`crates/chat-stasher/src/config.rs:96`). Content is encrypted
by `rustic` before it is written there, with a master key that is generated and
kept on your machine (`crates/chat-stasher/src/store.rs:261-296,919-1004`).

## 4. Who your data is shared with

**We share nothing with anyone, because we never receive anything.** There are
no third-party processors, no advertising partners, no analytics vendors, no
error-reporting service, and no data sales.

The parties who *do* see something, stated plainly:

| Party | What they see | Why |
|---|---|---|
| **The chat platform** (ChatGPT, DeepSeek, Perplexity, Gemini, Claude, Kimi) | Your conversations — they host them; they always could. The passive capture adds no traffic of its own. | `apps/extension/lib/page-hook.ts:332`, `:356-373` |
| **Your archive destination provider**, if you chose a remote one | Encrypted objects: their **sizes**, **timestamps**, and how many there are. Not the content. This is a real metadata leak: it reveals your archiving rhythm and volume. | `crates/chat-stasher/src/store.rs:261-296`; see `docs/threat-model.md` |
| **Your browser vendor**, possibly | Download-history entries containing platform names and session ids, *if* your browser syncs download history to your browser account. **We have not investigated** whether any particular browser does this by default. | `apps/extension/lib/download.ts:138-139` |
| **Anything else running on your computer as you** | The plaintext inbox files, the staged shards, the config, and the master key file. We do not defend against this. | See [Known weaknesses](#known-weaknesses) |
| **Us, the authors** | Nothing. | Section 1 |

One further disclosure about the optional **backfill** feature, which walks your
conversation history to archive older chats. When you turn it on, it issues
additional requests to the chat platform, from your own logged-in session
(`apps/extension/lib/backfill/engine.ts:199`). That produces a request pattern
the platform can see and which does not look like a human reading their history.
**We have not investigated** whether any platform's terms of service prohibit
this, or whether it triggers rate-limiting. Backfill is off unless you enable it
(`apps/extension/lib/backfill/schedule.ts:24`), and with no HTTP port wired the
code refuses to fetch at all rather than defaulting to a live one
(`apps/extension/lib/backfill/engine.ts:39-41`, `:119`).

Which platforms actually see those extra requests, stated exactly:
**ChatGPT** (conversation list *and* each conversation's content), and
**DeepSeek** and **Perplexity** (**conversation list only** — the extension
never requests the content of a DeepSeek or Perplexity conversation, and
therefore never saves one). On **Gemini**, **Claude**, and **Kimi** backfill
issues no requests at all. (`apps/extension/lib/backfill/enumerate.ts:762`,
`:774`, `:643`.) The practical reading for you: enabling backfill on DeepSeek or
Perplexity produces list traffic the platform can see, and produces **no backup
whatsoever** on your side.

## 5. Where the extension runs

The extension's content scripts are injected on an **explicit, closed list of
origins** compiled into the code — never `<all_urls>`, never a wildcard:

- `https://chat.deepseek.com` (`apps/extension/lib/contract.ts:70`)
- `https://www.perplexity.ai` (`apps/extension/lib/contract.ts:113`)
- `https://chatgpt.com`, `https://chat.openai.com` (`apps/extension/lib/contract.ts:129`)
- `https://gemini.google.com` (`apps/extension/lib/contract.ts:145`)
- `https://claude.ai` (`apps/extension/lib/contract.ts:161`)
- `https://www.kimi.com` (`apps/extension/lib/contract.ts:231`)

The list the browser is given is derived mechanically from that table
(`apps/extension/lib/contract.ts:307-310`), so the sites the extension can run
on and the sites it can capture from are the same set by construction — they
cannot drift apart.

**You can verify this yourself without reading the code:** your browser shows
the extension's site access in `chrome://extensions` / `about:addons`, and it
will name these sites and no others. On every other website you visit, this
extension is not running.

Within those sites, not every request is captured. A response is only kept if it
matches the platform's expected route *and* method *and* status *and* body shape
(`apps/extension/lib/contract.ts:380-400`, `:502-516`), and bodies over 4 MB are
skipped as streamed media (`apps/extension/lib/contract.ts:322`). No shipped
platform row reads WebSocket frames; every row sets that switch to `false`
explicitly (`apps/extension/lib/contract.ts:109`, `:125`, `:141`, `:157`, `:219`,
`:303`).

**What running on a site does *not* mean.** Being on this list means the
extension's content script is injected there. It does not mean your history on
that site gets archived. The optional backfill feature — the only part that goes
looking for *past* conversations — is limited to a shorter list, and the middle
tier of that list is easy to misread:

| Platform | What backfill does when you enable it |
|---|---|
| **ChatGPT** | Lists your conversations **and saves their content** (`apps/extension/lib/backfill/enumerate.ts:762`). |
| **DeepSeek**, **Perplexity** | Lists your conversations and **saves none of their content** — **no file is written, so this history is not backed up** (`apps/extension/lib/backfill/enumerate.ts:774`, `:538-548`, `:603-611`). |
| **Gemini**, **Claude**, **Kimi** | Nothing; the leg stops before issuing any request (`apps/extension/lib/backfill/enumerate.ts:643`). |

We state this in a privacy policy because the failure mode is a privacy
expectation, not just a feature gap: a user who believes their DeepSeek or
Perplexity history is archived may delete it upstream. It is not archived.

Only `fetch` responses are read. The hook also wraps `XMLHttpRequest` and
`EventSource` on these sites, but **solely to print a console warning** that a
transport it cannot capture was used — it never reads those response bodies
(`apps/extension/lib/page-hook.ts:118-125`, `:174-200`, `:202-214`).

## 6. What each permission is for

The extension declares exactly four permissions and no host permissions
(`apps/extension/wxt.config.ts:36`):

| Permission | Why it is needed | What it does **not** allow |
|---|---|---|
| `downloads` | The only zero-configuration way to write a file to your disk from a background service worker. This is how a captured conversation reaches the inbox directory. (`apps/extension/lib/download.ts:27-32`) | It does not let us read your other downloads. The code only deletes files it created itself (`apps/extension/lib/download.ts:62-68`). |
| `nativeMessaging` | Hands a captured conversation straight to the `chat-stasher` binary already on your machine, instead of going through a download. Added because Chrome's browser-level "ask where to save each file before downloading" preference overrides the extension's `saveAs: false`, which turns a 200-conversation backfill into 200 modal dialogs. The host is registered by you, per-user, with `chat-stasher install-native-host`; the manifest names exactly one allowed extension id. | It cannot reach any program other than the one host manifest you registered, and that host is a binary you installed yourself. If you never run `install-native-host`, this permission does nothing at all and the extension falls back to `downloads`. |
| `storage` | Persists the items listed in [section 3b](#3-where-your-data-is-stored) — the badge counter, the download guard, and the backfill progress set, so an interrupted backfill can resume instead of restarting. (`apps/extension/lib/backfill/store.ts:1-13`) | This is `storage.local` only. Nothing is written to `storage.sync`, so nothing here is uploaded to your browser account by us. |
| `alarms` | Gives the backfill leg a periodic heartbeat, so history archiving can finish over days without you having to keep the chat tab open. (`apps/extension/wxt.config.ts:23-32`; `apps/extension/lib/backfill/alarm.ts`) | It does not grant any network or data access. |

None of the three shows an install-time warning except `downloads` ("Manage your
downloads"), which is disclosed here rather than left for you to discover.

## 7. Cookies, analytics, and tracking

**The extension sets no cookies, contains no analytics SDK, and sends no
telemetry, crash reports, or usage pings.** The CLI likewise reports nothing
home.

The verifiable basis for that sentence, again so you do not have to take it on
trust: there is no analytics dependency to find, no endpoint to block, and no
opt-out setting — because there is nothing to opt out of. A search of
`apps/extension/lib`, `apps/extension/entrypoints`, and `crates/chat-stasher/src`
for `analytics`, `telemetry`, `sentry`, `gtag`, `mixpanel`, `posthog`, and
`amplitude` returns no matches, and the extension holds no host permission that
would let it reach a collection endpoint (`apps/extension/wxt.config.ts:36`).
A network capture on the extension's background page is the check that does not
require trusting us at all.

We do not respond to Do-Not-Track signals, for the simple reason that we operate
no service that could receive one.

## 8. We are not an AI service

Chat Stasher does not call any AI model, does not send your conversations to a
model provider, and does not use your conversations for training anything. The
word "chat" in this product refers to conversations you already had, on someone
else's service, that this tool copies into your own archive. The archive format
is `rustic` encrypted backup objects (`crates/chat-stasher/src/store.rs:261-296`);
nothing reads them except you.

## 9. How long data is kept, and how to delete it

**We keep your data for zero seconds, because we never hold it.** There is no
account to close and no deletion request to file with us — there is nothing on
our side to delete.

Retention on **your** machine is under your control:

| Where | How long it stays | How to delete it |
|---|---|---|
| Inbox files in your download directory | Until the CLI's `ingest` consumes them, which moves each file to `<inbox>/consumed/` (`crates/chat-stasher/src/main.rs:457-479`). **If you never run `ingest`, they stay indefinitely, in plaintext.** | Delete the files in `chat-stasher/inbox/` (and `consumed/`) with your file manager. Nothing else depends on them once ingested. |
| Browser download history entries | Until you clear your browser history | Clear downloads in your browser's own history UI |
| Extension local storage (badge, guard, backfill progress) | Until you clear it or uninstall the extension | Uninstalling the extension removes it; browsers also expose per-extension site-data clearing |
| Staged shards | Until `push` moves them into the repository | Delete the stage directory you chose |
| Your archive repository | **Indefinitely, by design.** This is a backup tool: it exists so that history a platform deleted still survives. | Delete the repository directory or remote bucket yourself. **There is no `delete` subcommand and no `restore` subcommand in this version** — the subcommand list is `init`, `run-once`, `schedule`, `push`, `status`, `read`, `doctor`, `verify`, `dest-init`, `search`, `view`, `ingest`, `collect`, `seal`, `install-native-host`, `native-host` (`crates/chat-stasher/src/main.rs:44-760`). Selective per-conversation deletion inside an archive is not implemented. |

**Uninstalling the extension stops all capture immediately** and removes its
local storage. It does not delete files already written to your download
directory or your archive — those are yours, and deleting your backup without
being asked would be the worse failure.

## Known weaknesses

A privacy policy that lists no weaknesses is more dangerous than no policy at
all, so here are the ones that bear on your privacy. The full list is in
[`docs/threat-model.md`](threat-model.md).

**1. The plaintext window in your download directory.** The extension writes
each captured session as an ordinary, unencrypted JSON file into your download
directory (`apps/extension/lib/download.ts:91-93`). The file contains the
conversation itself. It sits there, readable by anything running as your user,
until the CLI consumes it. **We do not encrypt it, we do not restrict its
permissions, and we do not shorten that window.** How long the window is depends
entirely on how often you run `ingest`; if you never run it, the plaintext stays
indefinitely.

*What you can do today:* run `ingest` promptly, and keep your download directory
on an encrypted volume.

**2. We do not defend against a hostile program running as your user.** Anything
running as you can read the plaintext inbox files, the staged shards, your
config, and — with your archive — decrypt everything. On a single-user desktop
this is the normal situation; on a shared machine it is the dominant risk.

**3. The master key is the only key, and losing it is unrecoverable.** There is
no escrow, no recovery code, no maintainer-held copy, and no password reset — by
design, because any of those would mean someone other than you could open your
archive (`crates/chat-stasher/src/store.rs:1044-1051,1006-1010`). The key file
is written owner-only (`0600`) on Unix; on platforms without Unix modes it
inherits whatever the filesystem gives it
(`crates/chat-stasher/src/store.rs:1086-1171`).

**4. What other browser extensions can observe is unresolved.** We did not test
whether a second, hostile extension can read our downloaded files, observe the
`window.postMessage` traffic between our page hook and our bridge, or read
download history. Treat this as **potentially exposed, not safe**. See the
extension-ecosystem row of [`docs/threat-model.md`](threat-model.md).

**5. No security audit has been performed.** We have not commissioned or run a
formal security assessment of this project. "We have not attacked this" is never
written here as "this attack does not work."

We make no claim that this software is secure, that your data cannot be lost, or
that any of the above will be fixed on a schedule.

## 11. Children

This software is not directed at children and we do not knowingly collect
information from anyone, of any age — there is no collection mechanism to
receive it. It is a developer tool that requires a command line to be useful.

## 12. Legal status of this policy

We do not act as a data controller or data processor for your conversations,
because we never receive them: the software runs on your computer and writes to
storage you own. For that reason this policy does not set out GDPR lawful bases,
international-transfer mechanisms, or per-jurisdiction consumer-rights tables —
those frameworks describe an operator holding your data, and stating them here
would imply a relationship that does not exist.

Rights such as access, portability, correction, and erasure are, in practice,
already yours by construction: the data is in files on your own disk, in
documented formats, and you can read, copy, or delete them without asking us.

The software is distributed under the Apache License 2.0 (`LICENSE:2-3`) and
comes with no warranty of any kind, as that license states.

## 13. Changes to this policy

If this policy changes, the "Last updated" date at the top changes with it, and
the change is visible in this repository's commit history. If a future version
of the software ever collects anything, this document will say so **before** that
version ships, and we would expect you to hold us to that.

## 14. Contact

**Email: `work@team.iopho.com`** — the same address as security reports
(`SECURITY.md:9`).

For a suspected vulnerability, please read [`SECURITY.md`](../SECURITY.md)
first: mail the address above rather than opening a public issue, and please do
**not** include your own conversation content, repository paths, hostnames, or
key material in the report.

This is a personal project with a single maintainer. There is no response-time
commitment (`SECURITY.md:29-35`).

## 15. What this policy does not establish

Stated separately, because the value of everything above depends on being clear
about what it does *not* cover:

- **This describes this version, built from this source.** It says nothing about
  a future release, and nothing about a build you obtained from somewhere other
  than a source you checked.
- **We do not defend against a compromised dependency.** The CLI and the
  extension both pull third-party packages, and there is no signed release, no
  reproducible-build claim, and no published artifact checksum to verify against.
- **We have not investigated** whether browsers sync download history to a
  vendor account by default; what other extensions can observe; or whether any
  chat platform's terms of service permit the capture or the backfill request
  pattern. Using this tool is your decision against your provider's terms.
- **We have not verified** the TLS or host-key behaviour of every storage
  backend the configuration accepts. Your archive's own encryption still
  protects the content, but transport security is whatever your chosen backend
  provides.
