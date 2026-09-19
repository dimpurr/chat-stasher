# Threat model

This document is organised around one question: **who can see your conversation
content, and who cannot?** It is not a list of things we are good at.

Every mechanical claim below cites a file and line in this repository. Where we
could not establish something, the row says so in one of three distinct ways,
and the difference matters:

- **Not found** — we looked in this repository and the thing is not there.
- **Not investigated** — we did not do the work; the answer is unknown to us.
- **Does not exist** — we are asserting it is absent, with a citation.

We have not run an adversarial security assessment against this project. "We
have not attacked this" is never written below as "this attack does not work".

Line numbers were checked against the code in this checkout. They drift as the
code changes; if a citation no longer lands where this document says it does,
trust the code and treat the sentence as unverified.

## How the data moves

Understanding the roles below requires knowing the path the content takes.

1. A browser extension hooks `fetch` on a fixed list of chat origins and keeps
   the raw response text (`apps/extension/lib/contract.ts:244-269`, `:719-721`;
   the `fetch` wrap at `apps/extension/lib/page-hook.ts:705-730`, the
   `response.clone().text()` read at `:698`, and the capture decision at
   `:356-396`).
2. The extension writes that text, as a JSON bundle, into its **own IndexedDB
   outbox** inside your browser profile — before attempting any delivery, so a
   service worker killed mid-flight cannot lose it without a trace
   (`apps/extension/lib/outbox.ts:309-377`;
   `apps/extension/entrypoints/background.ts:205-222`).
3. The extension delivers the bundle to a **Native Messaging host** — the
   `chat-stasher` binary you registered with
   `chat-stasher install-native-host --stage <path>` — over
   `runtime.sendNativeMessage`. The host seals it into that stage as a *sealed
   shard*, through the same code path `ingest` uses
   (`apps/extension/lib/native-host.ts:755-805`;
   `crates/chat-stasher/src/nativehost.rs:1139-1166`). The bundle leaves the
   outbox **only** on a matching `ack`
   (`apps/extension/lib/native-host.ts:775-784`). Separately, the CLI reads
   local coding-harness session stores (`collect`, `status`) and can take bundles
   from a directory by hand (`ingest --inbox`)
   (`crates/chat-stasher/src/main.rs:635-685`).
4. `push` writes the stage into a rustic repository — encrypted — at a
   destination you configure, local or remote
   (`crates/chat-stasher/src/main.rs:235-272`).

Steps 1–3 are plaintext on your own machine. Step 4 is the only encrypted
boundary, and it is also the only step that can involve a network.

## Who can see what

### Us — the project authors

| | |
|---|---|
| **Can see** | Nothing. |
| **Cannot see** | Your conversation content, your session ids, your account identity, your destination address, whether you run this at all. |
| **Evidence** | The repository contains no project-operated endpoint. The CLI's only network capability is the rustic/opendal backend you configure yourself (`crates/chat-stasher/Cargo.toml:20-21`; `crates/chat-stasher/src/config.rs:95-100,146-162`). The extension's only outbound HTTP port defaults to a function that refuses to send (`apps/extension/lib/backfill/engine.ts:86-89`), and when it is wired every request goes through `checkBackfillRequest`, which refuses anything that is not same-origin, not in the platform table, not on a platform with a backfill plan, not one of that plan's exact paths (a plan may name three: the list, the body, and a body's optional second step), or not carrying a permitted method (`apps/extension/lib/backfill/tab-port.ts:405-444`). Its one other process boundary is `runtime.sendNativeMessage` to the pinned host name (`apps/extension/lib/native-host.ts:30`), which is a local pipe to a binary on your machine, not a network call. The extension declares no host permissions and no telemetry endpoint (`apps/extension/wxt.config.ts:87`). |

**Why this is worth stating precisely:** this is not a promise we are keeping.
It is a property of there being no such link in the code. We could not read your
conversations if we wanted to, because there is no component of this project
that we operate. If you compile from source, you can check this yourself with
the citations above — you do not have to trust the sentence.

The corresponding honest limit: this says nothing about a *future* version, a
build you did not compile yourself, or a dependency (see
[Supply chain](#supply-chain-not-defended)).

### The provider of your destination (Storage Box, S3, SFTP host, …)

| | |
|---|---|
| **Can see** | That encrypted objects exist; their **sizes**; their **timestamps**; how many there are and how that changes over time. From the SFTP/SSH case specifically, also your source IP and connection times, as with any SSH server. Your account with them, obviously. |
| **Cannot see** | Conversation text, session ids, platform names, which harness a session came from — all of it is inside the encrypted rustic repository. |
| **Evidence** | Content is written through `rustic_core` into a repository whose master key never leaves your machine (`crates/chat-stasher/src/store.rs:261-296,1064-1149`). The backend is `rustic_backend` with the opendal feature and the options you supply (`crates/chat-stasher/Cargo.toml:20-21`; `crates/chat-stasher/src/config.rs:146-162`). SSH connection handling: `crates/chat-stasher/src/reap.rs:1-12`. |

**This is a real metadata leak and we are stating it plainly.** A destination
provider learns your **backup rhythm and volume**: how often you archive, how
much you produced each time, and therefore roughly when you were and were not
having conversations. If you archive on a schedule
(`crates/chat-stasher/src/main.rs:175-234`), the schedule itself is visible to
them as a pattern of writes. If you archive manually, the write times are a
usage log.

We do not pad object sizes, batch on a fixed cadence, or add cover traffic. If
your threat model includes "the storage provider must not learn when I use an
LLM", **this tool does not solve that**, and a local-only destination is the
answer.

### Other programs running on your machine, as you

**This is the row most easily overlooked, and it is the widest one.**

| | |
|---|---|
| **Can see** | The captured conversations **in plaintext**, in the extension's outbox inside your browser profile; the master key that opens your entire archive; the staged shards before they are pushed; any directory you exported to; your config, including your destination address. |
| **Cannot see** | Nothing meaningful is withheld from a process running as your user. |

Concretely, five separate plaintext exposures:

1. **The outbox window, before delivery.** The extension writes each captured
   session as an ordinary, unencrypted record into its outbox IndexedDB
   database, inside your browser profile
   (`apps/extension/lib/outbox.ts:64-80`, `:309-377`). The record's `raw.text`
   field is the raw response body — the conversation itself
   (`apps/extension/entrypoints/background.ts:132-160`). It sits there,
   readable by anything running as you, until the host answers a matching `ack`
   and the record is deleted (`apps/extension/lib/outbox.ts:379-394`). **We do
   not encrypt it, we do not restrict its permissions, and we do not shorten
   that window.** How long it is depends on how often the host is reachable; if
   it never is, the plaintext stays indefinitely. A second plaintext copy of a
   capture exists if you press the popup's export button, which writes the same
   bodies into an ordinary download file
   (`apps/extension/lib/outbox.ts:465-475`). (An **archived** session can also be
   written out decrypted, by `export --out` — that is exposure 5, below.)

2. **The master key file.** It is written as plaintext JSON. On Unix it is
   created `0600` — the mode is set when the file is created, not afterwards —
   inside a parent directory tightened to `0700`
   (`crates/chat-stasher/src/store.rs:1231-1316`); on platforms without Unix
   modes it inherits whatever the filesystem gives it. That keeps it away from
   *other* users, not from you: any process running as you can read it and,
   combined with access to your destination, decrypt the entire archive.

3. **The stage directory.** Sealed shards are ordinary files on disk before
   `push` encrypts them into the repository
   (`crates/chat-stasher/src/main.rs:235-239`).

4. **The download-history entry for an export file.** If you press the popup's
   export button, the browser records an ordinary download whose file name is
   `chat-stasher-export-<UTC>.jsonl` (`apps/extension/lib/outbox.ts:442-446`).
   That is metadata, not content — it says an export happened and when, not
   which conversations were in it — and it may be synced by your browser to your
   browser vendor. **We have not investigated** whether any particular browser
   syncs download history by default.

5. **A directory you exported to.** `chat-stasher export --out <dir>` writes the
   archived sessions it selected back out **decrypted**, one file per session,
   into a directory you name (`crates/chat-stasher/src/main.rs:521-601`). Unlike
   the stage, nothing here is sealed and nothing moves it on: the files stay
   exactly as written until you delete them, and the command keeps no record of
   where they went. Name a directory you would be willing to lose, and delete it
   yourself when you are done. `--turns user` and `--trim-to-window` write less,
   but what they write is still plaintext.

**We do not defend against a hostile process running as your user.** On a
single-user desktop this is the normal situation and the exposure is
proportionate; on a shared machine, or a machine running untrusted software as
your user, it is the dominant risk in this document.

### The local dashboard (`chat-stasher ui`)

| | |
|---|---|
| **Can see** | Any program running as you can connect to the dashboard's port, because it listens on `127.0.0.1` (`crates/chat-stasher/src/view.rs:173`). Loopback is not a security boundary. |
| **Cannot see** | Anything, without the random token printed in the URL at launch. Every route checks it with a constant-time comparison before doing anything else, and any method other than GET is refused (`crates/chat-stasher/src/view.rs:256`, `:180`). |

**Who can start it.** There are two ways, and both end in the same
loopback-only, token-gated server:

- You run `chat-stasher ui` yourself, by hand.
- **The pinned extension can ask the native host to start it**
  (`open_dashboard`, `contracts/nativehost-protocol.md` §6.5). The host is a
  Native Messaging host, so the browser starts it only for an extension whose id
  is in the host manifest that `chat-stasher install-native-host` wrote;
  `crates/chat-stasher/src/nativehost.rs` refuses every other origin
  (`crates/chat-stasher/src/nativehost.rs:1906-1940`). The extension therefore cannot be *any* extension you happen to
  have installed — it has to be this one, with the pinned id, on a manifest you
  registered yourself.

What that new path does and does not change:

- **It does not widen who can reach the dashboard.** The socket is still bound to
  `127.0.0.1` with an OS-assigned port, and the token is still generated from the
  OS CSPRNG at launch (`crates/chat-stasher/src/view.rs:138-150`, `:158-172`).
  The host starts the same binary with the same config
  (`dashboard_argv`), so a dashboard opened from the popup is the same object as
  one you typed.
- **It hands the token to one more local party: this extension.** The URL is
  returned in the `open_dashboard` response and is not logged, not written to
  disk and not printed by the host
  (`crates/chat-stasher/src/nativehost.rs`). The extension opens it in a tab
  (`apps/extension/entrypoints/popup/main.ts`) only after checking it is
  `http://127.0.0.1:<port>/?token=<64 hex>`
  (`apps/extension/lib/native-host.ts`). Anything else — a `nack`, a timeout, a
  missing host, a URL that is not loopback — opens nothing.
- **The destination is never chosen by the extension.** There is no default
  destination (ADR-013), and the message is parameterless, so the dashboard opens
  the destination named by `[native_host] destination` in your config or not at
  all. An extension cannot point the dashboard at a repository, a key file or a
  flag of its choosing.
- 🔴 **The token's lifetime is now less predictable to you.** A dashboard you
  started by hand is one you can see in a terminal. One started from the popup
  exists because you clicked a button, runs until its idle timeout
  (`chat-stasher ui --help`), and each click starts a **new** dashboard — the
  host cannot tell whether one is already running, because it is a
  one-process-per-request host with no state and the token is per-launch and
  never persisted. Closing the tab does not stop the process; its idle timeout
  does.

Two things worth stating plainly:

- **Opening a conversation is a GET request that fetches and decrypts it** (`crates/chat-stasher/src/ui.rs:550`). That is acceptable only because the per-launch token is the one gate: there is no separate CSRF token and no Origin check. Treat the printed URL as a secret for as long as the process runs. A dashboard started from the popup prints nothing: its URL exists in the extension, in the tab, and nowhere else.
- **Whether the macOS application firewall prompts for a server bound only to `127.0.0.1` is documented, not verified.** Apple's firewall documentation describes protection against connections from other computers and does not mention loopback either way; third-party documentation states that the application firewall does not filter loopback. We have not observed the behaviour on a machine with the firewall turned on.

### Someone with physical access to your machine, or your stolen disk

| | |
|---|---|
| **Can see** | Everything the previous row lists, if the disk is not encrypted or is unlocked: the plaintext outbox records in your browser profile, the key file, the stage, the config. With the key file *and* the repository, they can read the entire archive. |
| **Cannot see** | The repository contents alone, *without* the key file — a stolen remote-destination copy is encrypted (`crates/chat-stasher/src/store.rs:261-296`). |
| **Evidence** | No at-rest protection is implemented by this project beyond the rustic repository itself; see the key-file citations above. |

The practical consequence: **full-disk encryption is doing the work here, not
this tool.** If your laptop disk is unencrypted, the archive's encryption buys
you protection against the *storage provider*, not against the person holding
your laptop, because the key sits next to the config.

We have not implemented, and do not currently plan for phase one: OS keychain
storage for the key, or passphrase-wrapping of the key file.

### The chat platforms (ChatGPT, DeepSeek, Perplexity, Gemini, Claude, Kimi, Grok)

| | |
|---|---|
| **Can see** | Your conversations — they always could; they host them. Additionally, the extension's capture is indistinguishable from your own browsing, because it reads responses to requests **made in your already-logged-in session**. |
| **Cannot see** | That the capture happened, as far as we know — but see the caveat below. |
| **Evidence** | The hook wraps `fetch` in the page's own world — `window.fetch` is replaced by the wrapper defined at `apps/extension/lib/page-hook.ts:705-730` — and reads a clone of responses the page already requested (`apps/extension/lib/page-hook.ts:698`; `apps/extension/entrypoints/dw-fetch-main.content.ts:13-15`). Backfill, when enabled, issues additional requests to the same origin (`apps/extension/lib/backfill/engine.ts:807-862`, `:1172-1194`). |

**Caveat, stated honestly, and one measurement this document owes the reader:**
the "cannot see that the capture happened" line above is about what the platform
observes, and it presumes a capture happened. On 2026-09-19, in a real browser
with the extension loaded, live capture on **Gemini** and **Kimi** did **not**
happen: the `gemini.google.com/app/<id>` tab still had the browser's own
`window.fetch` and `XMLHttpRequest.prototype.open`, and the `www.kimi.com/chat/<id>`
page's own messages request was answered 200 with a `{messages}` body and
produced no capture. One cause is fixed on this branch (a same-origin **subframe**
of a supported origin was never injected into — `allFrames` was off — so a
request made from one never reached the hook); the other is that a document
existing **before** the extension was loaded or updated is not re-injected into
by Chrome without host permissions this extension does not request, and is fixed
only by reloading the tab. Which one a given tab is cannot be told from outside
it, and the cause is still under investigation. This is stated here because the
opposite reading — "capture works on all seven, so the platforms see nothing
distinctive" — would be a claim about a channel that, on those two, was not
running. `e2e/frame-capture.spec.ts` is the reproduction of the first cause.

**Caveat, stated honestly:** on every platform except ChatGPT and Gemini the
passive hook adds no traffic, so there is nothing distinctive for the platform to
observe from it. **On Gemini it does add traffic:** when a conversation is
opened, the extension fetches it from its first page and follows the paging token
to the end, through the same allowlisted channel the backfill leg uses
(`apps/extension/lib/gemini-capture.ts:150-234`). That is one request for the
first page — a repeat of the one the page just made — plus one per remaining
page, spaced 1-3 seconds apart. The repeat is deliberate: the response the page
produced may be any page of the conversation, because the page asks for older
turns as you scroll, and a copy anchored anywhere but page 1 could look complete
while holding only the oldest turns. **On ChatGPT it does add traffic:** when you move between conversations
in the page, ChatGPT loads only a recent slice, and the extension requests the
full conversation itself, with the access token it reads from the same origin's
`/api/auth/session` (`apps/extension/lib/page-hook.ts:679-684`;
`apps/extension/entrypoints/dw-bridge.content.ts:548-574`;
`apps/extension/lib/platform-auth.ts:99-118`). That is one extra request per
conversation you open, at most once per 15 seconds per conversation. The token
stays in the content script's memory; a script on the page itself could already
read the same token, so this adds no new party who can see it. **Two other
platforms are places where the extension sends a credential rather than only
cookies** (backfill requests, and on Gemini the live leg too): Kimi reads the page
origin's own `localStorage.access_token` at request time, sends it to Kimi's two
backfill paths and nothing else, and holds no copy (`apps/extension/lib/platform-auth.ts:214-246`);
Gemini reads three values out of the page's own `WIZ_global_data` — the XSRF token
that goes in the request body, and two identifiers that go in the query — through
a page-world pull, per request, holding no copy, and attaches them to its two RPCs
and nothing else (`apps/extension/lib/platform-auth.ts:355-417`;
`apps/extension/lib/contract.ts:39-100`). By the same argument that applies on
ChatGPT, neither adds a party who could not already see it — any script on those
origins, and the pages' own requests, carry those same values. Gemini's borrow
one more property worth naming: the extension *asks the page* for them, and the
page's answer travels over `window.postMessage`, which any script on the page can
also read. That is the same set of values such a script can read directly out of
`window.WIZ_global_data`, which is why the channel is acceptable here — it
discloses nothing new — rather than an accident nobody looked at. **Backfill is
different** — it walks conversation lists and detail endpoints
(`apps/extension/lib/backfill/engine.ts:807-862`, `:1172-1194`), which produces a
request pattern the platform can see and which does not look like a human
reading their history. **We have not investigated** whether any platform's terms
of service prohibit this, nor whether any platform rate-limits or flags such a
pattern. Using this tool is your decision against your provider's terms; we make
no claim that it is permitted.

**Which platforms backfill actually touches, and what you get back.** This
matters to the threat model twice over — it bounds the observable traffic, and
it bounds what you may safely assume is archived:

| Platform | Requests the platform sees | What lands in your archive |
|---|---|---|
| **ChatGPT**, **DeepSeek**, **Gemini**, **Grok**, **Kimi**, **Claude** | Conversation-list requests **and** requests per conversation — one on ChatGPT, DeepSeek, Kimi and Claude, **one per page** on Gemini (a long conversation is several requests), **two** on Grok (a skeleton call, then a content call), plus **one** resolution request on Claude when neither the page's own requests nor its cookie names the organization | The conversation text (`apps/extension/lib/backfill/enumerate.ts:3890-3917`). On all six this is **implemented but not yet observed completing in a real browser**; on Grok and Kimi, whether a long conversation comes back complete is **unverified**, because the extension does not page those endpoints. On DeepSeek it is unverified too and the endpoint is not paged either — but the body is **checked before it is stored**: the response is a tree, the extension walks it from its newest message back to a root, and a walk that leaves the messages the response carries means that conversation is not archived (`apps/extension/lib/backfill/enumerate.ts:2239-2266`). Gemini **is** paged, to the end of the continuation token, and a conversation needing more than 20 pages is refused and listed as a failure rather than archived in part (`apps/extension/lib/backfill/engine.ts:1302-1354`). Grok's routes were read out of public open-source implementations rather than measured in a logged-in session, and where its sources disagree about the list cursor the leg stops instead of choosing (`apps/extension/lib/backfill/enumerate.ts:2539-2600`; `apps/extension/lib/backfill/engine.ts:886-931`). Kimi's routes **were** measured in a logged-in session, and both of its requests carry the token that session uses — read from the page origin's own local storage at request time, held in memory only, and sent to those two paths and no others (`apps/extension/lib/platform-auth.ts:214-246`); a Kimi body response that says it holds only part of a conversation is refused and listed as a failure rather than archived as a whole one (`apps/extension/lib/backfill/engine.ts:1406-1441`) |
| **Perplexity** | Conversation-list requests **only** | **Nothing.** Not one conversation body is requested or delivered (`apps/extension/lib/backfill/enumerate.ts:3890-3917`) |
| **Claude** | Conversation-list requests **and** requests per conversation, each addressed by an account-scoped organization; plus **one** organization-list request when the page's own requests and the cookie both answered nothing | The conversation text, on the condition its own parent links hold the whole branch — a body whose walk back from its newest message reaches a message the response does not carry is refused and listed as a failure rather than archived (`apps/extension/lib/backfill/enumerate.ts:3345-3401`). Every request path carries the organization, which the page URL does not; it is resolved from evidence in a fixed order and the leg **stops** rather than choosing when an account has several (`apps/extension/lib/backfill/claude-org.ts:170-224`), so the request that is sent is always one the extension itself built for one resolved organization (`apps/extension/lib/backfill/tab-port.ts:447-463`) |

🔴 The middle row is the dangerous one to misread. On Perplexity the extension
*does* work — it enumerates your conversations and reports a pending count —
while archiving **zero** of them. If you rely on this tool as the reason it is
safe to delete history upstream, that reasoning does not hold there. The backfill
leg leaves that conversation-content segment unfilled precisely because a wrong
guess fails silently: it would archive a truncated version of every chat and
still look like success. The route itself is no longer the unknown — the
extension's live-capture row reads the response the page fetches when you open a
conversation (`apps/extension/lib/contract.ts:338-353`) — but the sources
disagree about that route's parameters, and nothing establishes whether one
response holds a whole long conversation. Reading a response the page already
fetched is also not the same as issuing that request yourself, and backfill
declines to issue it. That second failure mode is also why DeepSeek's row is marked
unverified rather than verified — the endpoint it uses is not paged by the
extension, so a long conversation could be stored as a truncated version while
looking like success.

Note also that the extension attempts to extract an account identity (user id,
email, or handle) from response bodies in order to deduplicate across machines
(`apps/extension/lib/contract.ts:892-905`, `:1051-1067`). That value is written
into the bundle and therefore into your archive
(`apps/extension/entrypoints/background.ts:147-149`). It never leaves your
machine, but it means your archive contains your account identifier.

### The browser extension ecosystem — other extensions installed alongside ours

| | |
|---|---|
| **Can see** | **Not investigated.** |
| **Cannot see** | **Not investigated.** |

We did not test what a second, hostile extension can observe. The specific
questions we did **not** answer, and which a reader should not assume are safe:

- Whether an extension with broad host permissions on a chat origin can observe
  our MAIN-world hook, the `window.postMessage` traffic between the page hook
  and the bridge (`apps/extension/lib/contract.ts:6-16`), or the page-world
  markers we set (`apps/extension/lib/contract.ts:102-104`).
- Whether a second extension can reach another extension's IndexedDB — which is
  where the outbox, and therefore the undelivered conversations, live
  (`apps/extension/lib/outbox.ts:34-37`).
- Whether the download-history entry for an export file is readable by other
  extensions.

The message contract does carry a token check on the hook's ready message
(`apps/extension/lib/contract.ts:875-885`), and payloads are shape-validated
before reaching extension APIs (`apps/extension/lib/contract.ts:840-873`). Those
are input-validation measures against a malicious *page*; **we have not
established** that they constitute a defence against a malicious *extension*,
and we do not claim they do.

Since the page-world hook communicates over `window.postMessage`, the
conservative assumption is that content in transit is observable to anything
else with script access to that page. Treat this row as **unresolved and
potentially exposed**, not as safe.

### The Native Messaging host — the boundary the browser enforces for us

This is the newest boundary in the design, and one of the few that is enforced
by something other than our own code.

| | |
|---|---|
| **Can see** | Every bundle the extension delivers: the conversation text, the platform name, the session id, the account identity in it. It is the local process that writes your archive's input. It also answers the extension's two read-only questions — a count-only summary of the stage, and a request to start the dashboard (see the two bullets at the end of this section). |
| **Cannot see** | Nothing is withheld from it: it sees every bundle it is asked to archive. But it is *not* a network service — it opens no socket, the browser starts one process per request, and it writes only into the stage you configured. |

The properties that bound this boundary:

- **The host manifest names exactly one allowed extension id.** Chrome's
  `allowed_origins` and Firefox's `allowed_extensions` are rendered from two
  pinned constants — `gihmdkkmmmkeiagjjiimacmgkdilofhi` and
  `chat-stasher@team.iopho.com` — and the extension's own Chrome id is pinned by
  a public key in its manifest, so it cannot vary per machine
  (`crates/chat-stasher/src/nativehost.rs:75-88`, `:341-385`;
  `apps/extension/wxt.config.ts:88-94`).
- **The host refuses a launch from anyone else.** A `chrome-extension://` origin
  carrying any other id, or a Firefox-shaped launch for any other add-on, gets
  nothing on stdout, a line on stderr, and a non-zero exit
  (`crates/chat-stasher/src/nativehost.rs:1906-1940`).
- **The host never creates the stage, and never mints a machine identity.** A
  missing `[native_host] stage`, a relative one, a path that is not a directory,
  or no persisted identity are each a named refusal that says how to fix it —
  never a silently created one
  (`crates/chat-stasher/src/nativehost.rs:915-975`, `:980-1009`).
- **Concurrent writers are serialised.** The host and `ingest` both hold an
  exclusive lock on `<stage>/.ingest.lock` while they allocate a shard sequence
  number and seal the shard, with a bounded 10-second wait
  (`crates/chat-stasher/src/inbox.rs:66-68`, `:853-881`). Two browsers, two
  profiles, or a host racing a manual `ingest` therefore cannot pick the same
  sequence number.
- **A delivery is confirmed twice over.** The host recomputes SHA-256 over the
  payload bytes and refuses on a mismatch, and the extension counts a
  conversation as delivered only when the `ack` carries back both the
  `request_id` and the `sha256` it sent
  (`crates/chat-stasher/src/nativehost.rs:1115-1124`;
  `apps/extension/lib/native-host.ts:775-784`).
- **The payload is checked before it is sealed**, and a bundle this channel
  cannot archive is refused with a named `nack` rather than stored as raw bytes
  (`crates/chat-stasher/src/nativehost.rs:1131-1137`).
- **The host also answers two read-only questions, and writes nothing for
  either.** `summary` counts the sessions in the stage from its directory
  entries and each shard's own mtime plus the local `run-state.json` — it does
  not open a shard, does not decrypt the repository and does not touch the
  network — and answers with counts, harness names, a window length and a push
  timestamp, never a session id or a title
  (`crates/chat-stasher/src/nativehost.rs`;
  `contracts/nativehost-protocol.md` §6.4). `open_dashboard` starts this same
  binary as `ui --no-open` for the destination named in your config and returns
  the per-launch URL to the extension only; "The local dashboard" section above
  covers what that hands over and to whom.
- 🔴 **Both are parameterless, and anything else is refused.** An
  `open_dashboard` carrying a `destination`, a `repo`, a `key_file` or any other
  field the document does not define is answered `nack` `bad-request`, so a
  compromised or hostile extension — or a call from one of your *other*
  extensions, if it could reach this host at all, which it cannot — cannot
  direct the dashboard at a repository or a flag of its choosing
  (`contracts/nativehost-protocol.md` §6, §6.5).

What this boundary does **not** buy you: the host is an ordinary binary running
as you, so anything that can replace it can do anything it can — see "A replaced
binary" below. And the registration is per-user, not per-machine: another user
account on the same computer registers its own host, with its own stage.

### Anyone else on the network between you and your destination

| | |
|---|---|
| **Can see** | Encrypted object traffic: sizes and timing, as with the destination provider. |
| **Cannot see** | Content. |
| **Evidence** | Same encryption boundary as the destination row (`crates/chat-stasher/src/store.rs:261-296`). Transport confidentiality is whatever your configured backend provides — SSH for the SFTP case (`crates/chat-stasher/src/reap.rs:1-12`). |

**We have not verified** the TLS or host-key verification behaviour of every
opendal backend the config permits. If you configure a backend over a plaintext
protocol, the repository's own encryption still protects content, but you are
relying on that alone.

## What the CLI deliberately does *not* touch

Two properties worth stating because they bound the blast radius on your own
machine:

- **Harness session stores are opened read-only.** Every SQLite connection uses
  `SQLITE_OPEN_READ_ONLY` with a `mode=ro` URI, falling back to
  `mode=ro&immutable=1` when a WAL store has no `-shm`
  (`crates/chat-stasher/src/sqlite_probe.rs:1373-1376`). The module states the
  intent that a read-only probe never creates or touches `-wal`/`-shm` sidecars
  (`crates/chat-stasher/src/sqlite_probe.rs:23-29`), and there is a test
  asserting no sidecars are created (`crates/chat-stasher/src/sqlite_probe.rs:2010-2056`).
  `status` and `doctor` are likewise declared read-only
  (`crates/chat-stasher/src/main.rs:298,357-358`).
- **`seal` refuses to rename files it cannot justify renaming.** It is gated by
  the registry's `seal_policy`, an evidence line, and a platform-confidence
  cell; a harness that holds an open file descriptor (Codex) is refused with
  the active file untouched, because renaming it would strand later writes in
  the old inode (`crates/chat-stasher/src/main.rs:687-719`).

## Integrity: unknown is never treated as empty

This project's archive may become the only surviving copy of a conversation,
because platforms delete history and suspend accounts. That makes one class of
bug more dangerous than an information leak: **silently recording "nothing was
there" when the truth is "we could not tell".**

Two enforcement points exist in the code:

- **`push` refuses an unprovable empty snapshot.** If the stage holds neither
  sealed shards nor machine metadata (a stage whose shards were all reclaimed
  still carries its declaration and retained summaries, and pushing those is
  not an empty snapshot), `push` audits consumed inbox files against the stage and the
  repository; it succeeds only when stage, scanner, collector and audit all
  agree, and otherwise exits non-zero with an explicit refusal rather than
  writing an empty snapshot
  (`crates/chat-stasher/src/main.rs:4654-4747`). It also fails closed when it
  cannot even establish stage safety
  (`crates/chat-stasher/src/main.rs:4626-4633`).
- **A destination that cannot be consulted is not an empty destination.**
  `dest-init` classifies each source destination into three states, not two:
  `Consulted`, `KnownEmpty` (nothing there *and* no local record of ever having
  collected for it — a fact), `SuspectedLoss` (we have a record and it cannot be
  read — possible data loss, reported loudly), and `Unknown` (no record and we
  cannot determine what is there)
  (`crates/chat-stasher/src/destinit.rs:104-118`, `:391-399`). The rationale is
  that "no repository at that location" has two opposite causes and the
  filesystem cannot distinguish them
  (`crates/chat-stasher/src/destinit.rs:57-72`). The user-facing text says so in
  as many words (`crates/chat-stasher/src/main.rs:3363-3419`).

This is an integrity property, not a confidentiality one. It does not protect
your data from anyone; it protects you from believing you have a backup you do
not have.

## Known weaknesses and things we have not done

This section is the reason to trust the rest of the document. Everything here is
a real limitation of the current code.

### Confirmed weaknesses

1. **Plaintext window before delivery.** Described in full above. Captured
   conversations sit unencrypted in the extension's outbox, inside your browser
   profile, until the host answers a matching `ack`
   (`apps/extension/lib/outbox.ts:309-377`, `:379-394`). **We do not currently
   defend this.** Mitigation available to you today: keep the popup's channel
   line healthy so deliveries go through, uninstall the extension when you are
   done with it, and put your browser profile on an encrypted volume.

2. **The master key file is plaintext on disk.** It is not passphrase-wrapped
   and not kept in an OS keychain. On Unix it is created `0600` in a `0700`
   parent (`crates/chat-stasher/src/store.rs:1231-1316`), which keeps it from
   other users but not from anything running as you; on platforms without Unix
   modes it inherits the filesystem's defaults.

3. **Lose the key file and the data is gone. We have no recovery mechanism of
   any kind.** The master key is the repository's only key
   (`crates/chat-stasher/src/store.rs:1189-1191`); losing it makes the repository
   unreadable, and `load_key_file` can only report the loss
   (`crates/chat-stasher/src/store.rs:1318-1322`). There is no escrow, no
   recovery code, no maintainer-held copy, and no password-reset path — by
   design, because any of those would mean someone other than you could open
   your archive. **Back up the key file separately from the repository, or your
   archive is a very reliable way to lose your conversations.**

4. **There is no restore command.** The subcommands in this version are `init`,
   `run-once`, `schedule`, `push`, `status`, `read`, `doctor`, `verify`,
   `dest-init`, `search`, `export`, `ui` (`view` is a deprecated alias), `ingest`,
   `collect`, `seal`, `reclaim-stage`, `install-native-host`, `native-host`,
   `activity-index`, `machine-declare`, `machine-label`, `overview`
   (`crates/chat-stasher/src/main.rs:130-991`); **a command that puts sessions
   back into a harness's own directories does not exist**. There are two
   retrieval paths, and both are payload-output commands — each puts
   conversation content where you can read it. `read` dumps **one session at a
   time** to stdout and prints its SHA-256
   (`crates/chat-stasher/src/main.rs:312-314,4979-5097`). `export --out <dir>`
   writes **many** sessions to files in one command, laid out as
   `<out>/<machine>/<harness>/<session-id>.jsonl`, and its directory is
   **plaintext** (`crates/chat-stasher/src/main.rs:521-601`) — see exposure 5
   above. Bulk retrieval of the sessions a time window selects is therefore
   possible; what remains missing is restoring them into a harness's own
   directories.

5. **Search is metadata-only.** `search` walks snapshot/index/tree objects and
   never fetches or decrypts a **session shard** — the conversation payload;
   full-text matching is not implemented
   (`crates/chat-stasher/src/main.rs:459-489`). One qualification, because the
   looser version of that sentence is no longer true: `search` also reads each
   machine's activity sidecar `meta/<machine>/activity-v1.jsonl`, and in a
   rustic repository every file's bytes are a data blob, so that read does go
   through the blob layer. The sidecar is metadata by declaration — it lives
   under `meta/`, is written by `activity-index`, and holds timestamps, not
   conversation text. `search` counts the two apart (`data_blobs_read` stays 0;
   the sidecar reads are reported separately), and
   `crates/chat-stasher/src/search.rs:16-34` states exactly which claim holds.
   It also distinguishes "nothing matched" from "could not finish reading"
   **and** from "read it all but could not place every session in time", which
   is the same unknown-is-not-empty discipline as above
   (`crates/chat-stasher/src/main.rs:485-489`).

6. **Session enumeration is incomplete for some harnesses**, which means the
   archive can be incomplete in ways this document does not enumerate. See the
   limits section of `README.md`.

7. **Browser-side history backfill runs on seven platforms, recovers text on
   six of them, and one tier of it looks like coverage
   without being coverage.** Backfill recovers past conversation *text* on
   **ChatGPT**, **DeepSeek**, **Gemini**, **Grok**, **Kimi** and **Claude**
   (`apps/extension/lib/backfill/enumerate.ts:3890-3917`), but on none of them
   has a complete backfill been observed in a real browser, and on Gemini, Grok
   and Kimi we have **not verified** whether a long conversation comes back whole
   rather than truncated (`apps/extension/lib/backfill/enumerate.ts:3553-3573`).
   DeepSeek and Claude belong to a different sentence, not this one: on both, the
   body is **checked before it is stored** — the response is a tree, and a walk
   from its newest message that leaves the messages the response carries means the
   conversation is not archived
   (`apps/extension/lib/backfill/enumerate.ts:2239-2266`, `:3345-3401`). Grok and Claude are the least
   verified: their routes come from reading public open-source implementations,
   not from a logged-in session, and one Grok conversation costs two
   requests. Claude adds a second kind of unverified: every one of its requests
   is addressed by an organization the page URL does not carry, so a resolution
   that cannot name exactly one stops the leg instead of reading some other
   organization's history (`apps/extension/lib/backfill/claude-org.ts:170-224`).
   That resolution is asked for **in the claude.ai page** over the channel
   backfill already fetches through, and only when it is needed — at the popup's
   start button for that platform, and on a wake-up whose recorded scope is not
   an organization yet; it costs at most one `GET /api/organizations` request,
   sent only when the page's own requests and the cookie both named none, and a
   recorded "several organizations, no signal" is not asked again
   (`apps/extension/lib/backfill/claude-page.ts:62-136`;
   `apps/extension/entrypoints/background.ts:704-720`). Kimi's routes, by contrast, were measured in a logged-in session,
   and its requests carry the page's own login token, read at request time and
   held in memory only (`apps/extension/lib/platform-auth.ts:214-246`); a body
   response that admits it is incomplete is refused and listed as a failure
   rather than archived (`apps/extension/lib/backfill/engine.ts:1406-1441`).
   Gemini's routes were measured in a logged-in session as well, its requests
   carry three values read out of the page's own bootstrap blob at request time
   and held in memory only, and its body is paged: more than 20 pages and the
   conversation is refused and listed as a failure rather than archived in part
   (`apps/extension/lib/platform-auth.ts:355-417`;
   `apps/extension/lib/backfill/engine.ts:1302-1354`). On
   **Perplexity** it enumerates your conversations and archives
   **none of them** (`apps/extension/lib/backfill/enumerate.ts:3890-3917`). The user-visible symptom of
   the middle tier is *activity* — a growing pending count — with an empty
   result, so "the extension is clearly doing something" is not evidence your
   history is safe. See the platform table above.

### Threats we do not model and do not defend against

Stating these as "we do not defend this" rather than implying coverage:

- <a id="supply-chain-not-defended"></a>**Supply chain.** We do not defend
  against a compromised dependency. The CLI pulls `rustic_core`,
  `rustic_backend`, `rusqlite` and others
  (`crates/chat-stasher/Cargo.toml:14-27`); the extension has its own npm
  dependency tree (`apps/extension/package.json`). Some crate versions are
  pinned (`crates/chat-stasher/Cargo.toml:20-21,27`), which aids
  reproducibility but is not a defence against a malicious pinned version.
  There is no signed release, no reproducible build claim, and no published
  artifact checksum to verify.
- **A modified or hostile browser.** Everything the extension sees, the browser
  sees first. A patched browser, or a browser with a hostile policy/profile,
  can read or alter captures. We do not attempt to detect this.
- **A replaced binary.** If someone can replace your `chat-stasher` binary, they
  can exfiltrate everything, and nothing in this design would notice. We publish
  no signature to check against.
- **A hostile process running as your user.** See the third role above. This is
  not defended.
- **The platform itself.** The platform hosts your conversations and can read,
  alter, or delete them regardless of what this tool does.
- **Traffic analysis of your archiving pattern.** Object size and timing are
  visible to your destination provider and we do not obscure them.
- **Physical/coercive access.** Out of scope.

### Explicitly not investigated

Listed separately from "not defended", because for these we simply do not know
the answer:

- Cross-extension exposure, in all three forms listed in the extension-ecosystem
  row above.
- Whether browsers sync download history to a vendor account by default.
- TLS and host-key verification behaviour across every opendal backend the
  config accepts.
- Whether any chat platform's terms of service prohibit the capture or the
  backfill request pattern, and whether backfill triggers rate limiting or
  account flagging.
- The behaviour of the extension when the `storage` permission is absent at
  runtime; the code is written to fail closed, but this was not verified against
  a real browser (`apps/extension/wxt.config.ts:57-63`).
- Windows-specific path handling for at least one harness; see `README.md`.

We have not commissioned or performed a formal security audit of this project.

## If you want the strongest configuration available today

Not a promise, just the honest best case with the current code:

1. Use a **local destination on an encrypted volume**, which removes the
   destination-provider row entirely.
2. Keep your **browser profile on the same encrypted volume** (that is where the
   outbox lives), and keep the host registered so deliveries actually leave it —
   an undelivered capture sits in the plaintext window indefinitely.
3. Store the **key file somewhere other than the repository**, and back it up —
   losing it is unrecoverable (weakness 3).
4. On a platform without Unix file modes, check the key file's permissions
   yourself after first run — the tool can only set them where the platform can
   express them (weakness 2).
5. Run `verify` (`crates/chat-stasher/src/main.rs:369-407`) rather than assuming
   the archive is intact.
