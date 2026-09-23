# Privacy Policy — Chat Stasher

**Last updated: 2026-09-19.**

This policy covers the **Chat Stasher browser extension** and the **`chat-stasher`
command-line tool**. Together they copy your own AI-chat conversations into an
encrypted archive on storage you choose.

Every factual claim below about what the software does cites a file and line in
this repository, in the form `path:line`. Line numbers were checked against the
code in this checkout; they drift as the code changes. If a citation no longer
lands where this document says it does, **trust the code and treat the sentence
as unverified**.

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
- **The extension runs on seven chat platforms and nowhere else** — a fixed list
  compiled into the code, not a wildcard. See
  [section 5](#5-where-the-extension-runs) for the exact origins.
- **Running on a site is not the same as backing up your history there.** The
  optional backfill feature implements recovering past conversation text on
  **ChatGPT**, **DeepSeek**, **Gemini**, **Grok**, **Kimi** and **Claude** — on none of the
  six has a complete backfill been observed in a real browser, and for DeepSeek,
  Gemini, Grok, Kimi and Claude we have **not verified** whether a long conversation comes
  back complete (Gemini is the one of those that pages; the others do not, and say so).
  Grok and Claude are the least verified of the six: their routes come from reading
  public open-source implementations rather than from a logged-in session,
  and one Grok conversation costs two requests. Kimi's routes *were* measured in a
  logged-in session, and its requests carry your page's own login token, read
  from the page and held in memory only (step 1 of section 1). **Gemini's were
  measured too**, and its requests carry three values out of the page's own
  bootstrap blob — read at request time through the page-world hook, held in memory
  only, and attached to its two RPCs and nothing else (`apps/extension/lib/platform-auth.ts:669-769`).
  Claude is the one platform whose requests are addressed by an account-scoped
  identifier the page URL does not carry; the extension resolves it from the
  page's own requests, the browser's cookie, or one extra request, and stops
  rather than choosing when an account has several organizations (section 5).
  On **Perplexity** it lists your conversations and saves **none of their
  content**. See
  [section 5](#5-where-the-extension-runs).
- **Everything is stored on your machine or at a destination you configure**
  (a local disk, or a remote store whose credentials only you hold).
- **There is a known plaintext window.** A captured conversation sits
  *unencrypted* in the extension's own outbox storage until the `chat-stasher`
  host acknowledges it, and an export you trigger from the popup contains the
  same bodies. We do not encrypt it, restrict its permissions, or shorten that
  window. See [Known weaknesses](#known-weaknesses). Separately, `export --out`
  writes archived sessions back out decrypted into a directory you name; that
  copy is yours to delete and nothing of ours moves it on (section 9).
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
   (`apps/extension/lib/page-hook.ts:659`, `:698`, `:559-576`). Only responses
   matching a known platform route are kept
   (`apps/extension/lib/contract.ts:272-718`, `:842-870`).
   **One exception, on ChatGPT.** When you move between conversations inside
   the page, ChatGPT now loads only the most recent part of a conversation.
   Keeping that part would store an incomplete conversation, so it is never
   kept; the extension instead requests the full conversation itself, from your
   page, on the same origin (`apps/extension/lib/page-hook.ts:679-684`;
   `apps/extension/entrypoints/dw-bridge.content.ts:608-634`). That request —
   and every backfill request to ChatGPT's conversation list or a conversation
   body — carries your session's access token, which the extension reads from
   ChatGPT's own `/api/auth/session` on the same origin. The token is held only
   in the page's content-script memory: it is never written to storage, never
   logged, never sent to the `chat-stasher` host, and never attached to any
   other request (`apps/extension/lib/platform-auth.ts:61-71`, `:101-120`).
   **A second token, on Kimi, and it is read from the page rather than requested.**
   Backfill's two Kimi requests — the conversation list and one conversation's
   body — need the session's bearer token, and a request carrying only cookies is
   answered with **HTTP 401** (measured in a logged-in www.kimi.com session,
   2026-09-14). Kimi keeps that token in the page origin's own `localStorage`,
   under `access_token`; the extension reads it there **at the moment of each
   request** — no copy is kept, not in a variable of ours, not in storage, not in
   a log, and never in anything sent to the `chat-stasher` host. It is attached to
   those two endpoints and to **no** other request, including no other path on
   kimi.com; after a 401 it is re-read once and the request retried once, and if
   there is no token the request goes out **without** one so that the platform's
   own refusal is what the leg sees — a refusal is never recorded as “you have no
   conversations” (`apps/extension/lib/platform-auth.ts:258-295`,
   `apps/extension/entrypoints/dw-bridge.content.ts:449-455`).
2. **Queue on your machine.** The extension writes that text, as a JSON bundle,
   into its **own IndexedDB outbox** — extension-local storage on your disk,
   keyed by the SHA-256 of the bundle (`apps/extension/lib/outbox.ts:34-37`,
   `:309-377`). It does this *before* attempting any delivery, so a service
   worker killed between "the page produced bytes" and "the host answered" cannot
   lose a conversation without a trace
   (`apps/extension/entrypoints/background.ts:267-284`).
3. **Deliver to the local host.** The extension hands the bundle to a Native
   Messaging host — the `chat-stasher` binary **you** registered with
   `chat-stasher install-native-host --stage <your-stage>` — with
   `runtime.sendNativeMessage`
   (`apps/extension/lib/native-host.ts:30`, `:755-805`). The host seals it into
   the stage you configured, using the same code path and the same guarantees as
   `ingest` (`crates/chat-stasher/src/nativehost.rs:1139-1166`).
   🔴 **The bundle is deleted from the outbox only when the host answers an
   `ack` whose `request_id` and `sha256` equal the ones sent.** A `nack`, a
   timeout or a disconnect leaves it queued
   (`apps/extension/lib/native-host.ts:775-784`;
   `apps/extension/lib/outbox.ts:379-394`).
4. **Push.** `push` writes the staged shards into a `rustic` repository —
   encrypted — at a destination **you** configure, local or remote
   (`crates/chat-stasher/src/main.rs:235-272`;
   `crates/chat-stasher/src/store.rs:261-296`).

Steps 1–3 happen entirely on your machine, in plaintext. Step 4 is the only
step that can involve a network, and the only destination it can reach is the
one you put in your own config. The Native Messaging hop in step 3 is a local
process-to-process call: it is not a network connection, and the host it reaches
is one you registered yourself.

**There is no step in which anything is sent to the authors of this software.**
That is not a promise we are keeping — it is a property of there being no such
link in the code.

### What the host answers back

Two answers travel the other way — from the binary you installed to the
extension you installed. Both are read-only, and neither carries a conversation:

- **`summary`** — how many sessions are in the stage: in total, in the last 24
  hours, and split by harness name, plus when the last successful push was. It
  is computed from the stage's directory entries, each shard's own mtime and the
  local `run-state.json`; the host does not open a shard, does not decrypt the
  repository and does not touch the network, and the answer holds no session id,
  no title and no path beyond the stage path `hello` already returns
  (`crates/chat-stasher/src/nativehost.rs`;
  `contracts/nativehost-protocol.md` §6.4). A count the host could not read is
  reported as *unknown* with its reason — never as `0`, which would say "your
  archive is empty" when the truth is "that part of the stage could not be
  listed".
- **`open_dashboard`** — the URL of a dashboard the host starts for you
  (`chat-stasher ui`, on `127.0.0.1`, with a per-launch access token). For as
  long as the dashboard runs, that URL **is** a secret, and the host hands it to
  the extension and to nothing else: it is not logged, not written to disk and
  not printed (`contracts/nativehost-protocol.md` §6.5; `docs/threat-model.md`,
  "The local dashboard").

Neither answer leaves your machine, and neither reaches us: they travel one hop,
from the binary you installed to the extension you installed.

## 2. What we collect

**We collect nothing.** No personal information, no conversation content, no
identifiers, no analytics, no diagnostics.

The counts the popup shows ("12 sessions in the last 24 h") are computed on your
machine by the `chat-stasher` binary you installed and handed back to the
extension over the browser's local Native Messaging channel
(`crates/chat-stasher/src/nativehost.rs`). They are not sent anywhere else, and
no request in this repository transmits them.

Because "we do not collect" is the easiest sentence in any privacy policy to
write and the hardest to believe, here is how **you** can check it without
taking our word for it:

- **Check the permission list on the shipped extension.** Open
  `chrome://extensions` (or `about:addons`) and look at what Chat Stasher asks
  for. It requests exactly four permissions — `nativeMessaging`, `storage`,
  `alarms`, `unlimitedStorage` — and **no host permissions at all**
  (`apps/extension/wxt.config.ts:113`). An
  extension with no host permissions cannot make requests to a server of ours;
  the only network the code can touch is inside the pages it is already injected
  into. There is no origin belonging to this project anywhere in the extension.
- **Check the network tab.** Open your browser's developer tools on a chat page
  and watch the requests. On every platform except ChatGPT, capture is
  passive: the hook reads a clone of a response the page already fetched, and
  adds no request of its own. On ChatGPT it adds one same-origin request for
  the full conversation when you move between conversations in the page, plus
  one to `/api/auth/session` for the token (see step 1 of section 1)
  (`apps/extension/lib/page-hook.ts:698`, `:559-576`). The one feature that does
  add requests, backfill, is off unless you turn it on — see
  [section 4](#4-who-your-data-is-shared-with).
- **Check the code for a tracker.** Searching the extension and CLI sources for
  `analytics`, `telemetry`, `sentry`, `gtag`, `mixpanel`, `posthog`, and
  `amplitude` returns **zero matches** in `apps/extension/lib`,
  `apps/extension/entrypoints`, and `crates/chat-stasher/src`. There is no
  analytics SDK to configure, disable, or trust.
- **Check the Firefox data-collection declaration.** The add-on declares
  Mozilla's data-collection field as `none` (`apps/extension/wxt.config.ts:136`).
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

**a. The extension's outbox, an IndexedDB database inside your browser
profile.** Each captured session is written there as one record holding the
bundle — a JSON document whose `raw.text` field is the raw response body, that
is, the conversation itself (`apps/extension/entrypoints/background.ts:147-175`;
`apps/extension/lib/outbox.ts:64-80`, `:309-377`). The database is named
`chat-stasher-outbox` and lives under the extension's own origin; uninstalling
the extension removes it with the rest of the extension's storage. **Its
contents are not encrypted.** A record stays there until the host acknowledges
it, and a record the host refused outright is kept until you delete it by
uninstalling. See [Known weaknesses](#known-weaknesses).

A capture has a second plaintext copy if you press the popup's **export**
button: that writes one `chat-stasher-export-<UTC>.jsonl` file, one undelivered
bundle per line, into your browser's download directory
(`apps/extension/lib/outbox.ts:439-475`). That file is an ordinary download, so
your browser keeps a download-history entry for it — just its name and
timestamp, not its content. We do not delete it; `ingest` retires it to
`consumed/` when it has consumed every line
(`crates/chat-stasher/src/inbox.rs:57-60`).

The CLI makes one plaintext copy too, and it is not a capture but an archive
session: `chat-stasher export --out <dir>` writes the sessions it selected back
out **decrypted**, one file per session, into the directory you name
(`crates/chat-stasher/src/main.rs:521-601`). Nothing moves those files on and
we keep no record of where they went, so deleting the directory is yours to do.
The CLI writes no archive content anywhere you did not name.

**b. Your browser's local extension storage** (`storage.local`, never
`storage.sync`: no `storage.sync` call exists anywhere under `apps/extension`,
so nothing here is synced to a browser account by this extension).
What is kept there:

| Key | What it holds | Citation |
|---|---|---|
| `cs_backfill_enabled_v1` | Whether you turned the history-backfill feature on | `apps/extension/lib/backfill/schedule.ts:29` |
| `cs_backfill_targets_v1`, `cs_backfill_tabs_v1` | Which site/tab the backfill timer should wake up for | `apps/extension/lib/backfill/alarm.ts:211`; `apps/extension/lib/backfill/tab-port.ts:162` |
| `cs_backfill_v2:<platform>:<scope>` | The backfill progress header: list cursor, counters, daily count, halt record, the record that a platform's id list had to be read again, and — while a stored stop is being re-decided — which build has already spent that one re-decision (a version string and a timestamp, nothing else). The **conversation/session ids** themselves (archived and still pending) are kept one record per id in a second IndexedDB database, `chat-stasher-backfill` (object store `debts_by_platform`), so settling one conversation does not rewrite the whole list. 🔴 Each id record is keyed by **platform, account scope and id together**: the platform is part of the key because two platforms can share one scope string, and a key without it let one platform's ordinary ledger write delete another's ids. An older `cs_backfill_v1:<platform>:<scope>` record is migrated once and removed only after the new layout has been written and read back. Ids written before the platform became part of the key sit in the older `debts` store in the same database until the platform that owns them can be established from the rest of your storage; a row whose platform cannot be established is left there, uncounted and undeleted. | `apps/extension/lib/backfill/types.ts:1458-1494`; `apps/extension/lib/backfill/debt-store.ts:68-87` |
| `cs_native_host_status_v1`, `cs_native_host_pause_v1` | The last `hello` answer (stage, machine id, host version, or the named reason it failed) and the record that says the backfill leg is paused | `apps/extension/lib/host-status.ts:24-53`, `:89-113` |
| `cs_outbox_last_export_v1` | The time, size and file name of the last export you triggered | `apps/extension/lib/outbox.ts:59-60`, `:477-501` |
| `cs_backfill_lasttick_v1` | The trace of the most recent backfill alarm wake: when it was, whether it ran, the named outcome, and how many backfill targets were registered. 🔴 It also carries **how that tick ended** — the run's own stop reason (`stopped`), the halt it left behind (`halted`) and that halt's `detail`, or, for a tick that stopped before making any request, that tick's own named outcome. And whether that tick **swept open tabs** for a live page the registry had lost (`tabSweep`): `null` if it never swept, `{ looked: false }` if it could not list tabs, `{ looked: true, queried, pruned, pinged, registered, deferred, crowded }` if it did — counts only, so "we looked and found nothing" stays distinct from "we never looked", a sweep that hit its ping cap (`deferred > 0`) stays distinct from one that pinged everything it wanted to, and a sweep that refused an answering tab for want of a slot (`crowded > 0`) stays distinct from both. 🔴 The field also has a fourth value that is **not an outcome**: `{ sweeping: true }`, written by the provisional record the tick saves *before* its sweep starts, says this tick has no sweep result yet. It exists so that an interrupted tick is never recorded as one that never looked — that provisional record is the one that stays if the browser reclaims the worker mid-sweep, if the sweep throws, or if the tick's final save fails, and `null` there would have been a false "this tick never swept" about a tick that did. It is replaced in the same tick by one of the three values above whenever the tick finishes, and the popup says the tick had not finished rather than reading it as a skip. 🔴 W76 · It also carries **which target that wake served, and which ones it passed over and why** (`schedule`): the platform id the wake ran (`served`), and, for each target the walk examined and did not run, that platform's id beside the code it was passed over for — `no-http-port` (no open page for it), `halted` (a stop that still applies) or `waiting-retry` (a transient stop still inside its backoff). The wake visits the registry from the target after the one it served last, so this field is also how "every platform gets a turn" is auditable after the fact. Platform ids and reason codes only: no account scope, no origin, no free text. Metadata only: reason codes, counts and timestamps. The one free-text field is the halt's `detail`, and by construction it names storage keys, paths, HTTP statuses and counts — never a conversation id, title, or body. One record, overwritten by the next wake. | `apps/extension/lib/backfill/alarm.ts:821-924`; `apps/extension/entrypoints/background.ts:1894-1928`, `:1936-1973` |
| `cs_hook_v1:<origin>`, `cs_hook_declined_v1` | A top frame's own report about its capture hook: one record per origin, holding each observation with **when it was first made and when it was last made**. The two differ because a page that stays in one state re-sends that state every few seconds to say it still holds; the latest observation is the state that page is in (an older one is a state it has moved out of), and a capture that arrived after the latest one *began* is evidence about it. A child frame's observation is not stored — it is a statement about that frame, not about the origin. 🔴 And the one report that was **received and not recorded**, with the check that refused it, the origin and observation when they are known, how many times in a row the same refusal has repeated, and when. Metadata only: reason codes, a count, an origin string, timestamps; no URL path, no conversation id, no body, no token. Unlike the per-origin records, the declined one is a single record, overwritten by the next decline. | `apps/extension/lib/hook-status.ts:78-155`, `:289-418`; `apps/extension/entrypoints/background.ts:2104-2120`, `:2147-2163` |
| `cs_live_capture_v1:<platform>` | When a live capture from that platform was last **confirmed to be in your archive**, and how many are **on record as newly stored** — one record per platform. It exists because nothing else said when a live capture had last arrived: `cs_last_delivered_v1` (below) maps a delivery name to a hash and carries no time at all, so "did a capture arrive at time T" was a gap in the record. It is written at the one place the live leg decides a capture was **stored**, so a capture that was merely queued, rejected or refused leaves no record. 🔴 Its two fields change for different reasons, and the popup names both: the **time** moves for every arrival that was stored, including one whose whole content the archive already held (the page re-sent a conversation it had already sent — ChatGPT does this on every view), because that still measures the page-to-archive path; the **count** rises only for an arrival that was **newly** stored, so four views of one conversation are not four stored conversations. Nothing else is in it: a platform id, a timestamp and a count. 🔴 A platform with **no record** is not a platform with zero captures: no writer creates a row out of nothing — a row exists only where a capture reached the archive — so the popup reads an absent record as "nothing has been recorded here", a gap in the record, and never as "no capture arrived". A `count` of `0` *inside* a row is not that absence and is not rounded up either: it says nothing new is on record there, beside a time that says a capture did arrive. | `apps/extension/lib/live-capture.ts:149-151`, `:273-308`; `apps/extension/entrypoints/background.ts:241-243`, `:256,331` |


Three things in that table deserve to be called out rather than buried:

- The progress set stores **session ids** — not conversation text, but a list of
  which conversations exist and which you have archived.
- **A DeepSeek halt record says what the response looked like, and only that.**
  When a response does not match the shape the **DeepSeek list or conversation
  parser** recognises, that leg stops and the halt detail carries the *structure*
  of what arrived: the key names at the level that disagreed, the type of each,
  and array lengths. Every other platform's `shape-changed` halt still names only
  the field it could not find — the trace below is not yet built for them.
  **No value from the response is ever in it** — not an id, not a title, not a
  message body, and not a fragment or a length of one; strings are reported as
  `string` and nothing more.
  🔴 One residual, stated rather than implied: a key is echoed only when it reads
  like a snake_case field name (`data`, `biz_data`, `chat_sessions`), so a
  response that keys an object by a conversation title (`Kyoto`) withholds that
  key. An **all-lower-case single word** still passes that test, so a title or
  account name shaped like one would be echoed. Withholding every key instead
  would remove the diagnosis this trace exists for; the trade is named here so it
  is not discovered later.
  A key whose *name* could be content rather than a field name (a response keyed
  by titles or ids) is withheld and shown as a marker, so the shape stays legible
  without the text. The structure is capped in both depth and the number of keys
  named per object
  (`apps/extension/lib/backfill/enumerate.ts:1242-1310`).
- The `<scope>` part of that key is your **account identifier on that platform**
  when the extension could find one in a response body (a user id, an email
  address, or a handle), and the literal string `default` when it could not
  (`apps/extension/entrypoints/background.ts:1202-1245` — the identity itself is
  read by `apps/extension/lib/contract.ts:1053-1069`; the `default` fallback is on
  the `||` at `apps/extension/entrypoints/background.ts:1245`). It is used to

  keep two machines' archives of the same account from colliding. It stays in
  your local browser storage and is written into your own archive; it is not
  transmitted anywhere by this extension. Note that the backfill leg started by
  pressing the popup button records `default` for a platform whose account
  identifier the extension cannot read at all. For a platform whose requests are
  addressed by an account scope, the popup's own start now **asks the page** which
  one it is using (see the claude.ai bullet below) and records that; only when the
  answer cannot be obtained does the row keep `default`, together with the named
  reason it could not be obtained
  (`apps/extension/entrypoints/background.ts:965-986`, `:1115-1189`).
- **On claude.ai the scope is not read from a response body: it is the
  organization the page's own requests are addressed to**, and that value is
  required in every request path on that platform while appearing in no page URL
  (`apps/extension/lib/backfill/claude-org.ts:4-11`). It is resolved from
  evidence, in a fixed order — the organization an already-seen request carried
  first, the `lastActiveOrg` cookie second, and one `GET /api/organizations`
  third — and that third source is the only request this resolution ever adds: it
  is made **only when the first two answered nothing**, it fetches your account's
  list of organizations and nothing else, and it is sent through the same
  allowlisted same-origin channel as every other backfill request
  (`apps/extension/lib/backfill/claude-page.ts:84-92`). The resolution itself runs
  **in the claude.ai page**, over the channel the backfill already uses, and only
  when the extension actually needs the organization: when you press the popup's
  start button for that platform, and on a wake-up whose recorded scope is not an
  organization yet. A page that is simply open and idle is asked nothing
  (`apps/extension/lib/backfill/claude-page.ts:62-136`;
  `apps/extension/lib/backfill/tab-port.ts:1063-1080`). An account belonging to
  several organizations, with neither of the first two sources naming one, stops
  the leg instead of choosing: the value is never guessed and the organizations
  are never probed one by one, and once **this build** has recorded that answer
  the page is not asked again on every wake-up — the answer is already known
  (`apps/extension/lib/backfill/claude-org.ts:211-265`;
  `apps/extension/entrypoints/background.ts:924-968`). A record an **earlier**
  build left is re-asked once, and only once: a recorded judgement is that
  build's, not this one's, and a refusal that repeats is written back naming the
  build that saw it. 🔴 "Once" is enforced rather than intended: the attempt is
  recorded **before** the request goes out, so a write that does not land cannot
  turn it into a once-per-wake-up poll — the scope simply waits for the next
  build. The sentinel
  `default` — "the identifier could not be told" — is refused outright for this
  platform rather than written into a path segment where it would address an
  organization that does not exist (`apps/extension/lib/backfill/engine.ts:1373-1385`).
  🔴 **The organization a backfill is started with is the one it keeps.** The
  scope is written into the platform's progress record and the target registry
  when the backfill is registered, and no later request re-reads it from the page
  — so switching organizations on claude.ai, or having claude.ai open in two tabs
  at once, does not move a backfill that is already running: it keeps writing
  under the organization it started with
  (`apps/extension/entrypoints/background.ts:1392-1456`). A backfill for a *second*
  organization starts by opening a conversation in it and using the extension
  there, which registers that organization as its own target with its own
  progress record — the two runs then advance independently, each under its own
  scope.
  🔴 **A conversation title is not an organization, and it is not stored as one.**
  The identity heuristic can harvest a conversation body's `name` as a handle, and
  a pre-W31 target row used that string as the scope. That row names no account:
  it is dropped when a real organization is registered, it is not written from a
  capture whose path segment is not an organization id, and a wake-up whose
  recorded scope is a title asks the page rather than substituting the title into
  a request (`apps/extension/lib/backfill/claude-org.ts:110-136`;
  `apps/extension/entrypoints/background.ts:748-778`). Collapsing a title cannot
  put the unresolved sentinel in front of a live organization — the alarm would
  otherwise halt on `'default'` and never tick the organization
  (`apps/extension/lib/backfill/alarm.ts:426-490`;
  `apps/extension/entrypoints/background.ts:1392-1456`). The host archive is
  append-only and is not touched. The **local** ledger header at
  `cs_backfill_v2:<platform>:<scope>` is a different fact: a run opens it before
  any request, so a title tick has already written `halted` / `failures` /
  `enumCursor` there. Dropping the registry row also removes that header; the
  popup ignores a header whose scope is not a registered target, so an abandoned
  halt cannot outrank the organization's own ledger
  (`apps/extension/lib/backfill/alarm.ts:454-490`;
  `apps/extension/lib/popup-view.ts:1593-1655`).
- **The trace of a declined hook report may name a site this extension is not
  built for.** That record says which page's report was refused, and a refusal
  happens precisely when that origin is *not* one of the eight in the platform
  table. What can reach it is bounded by who can send the message at all: a
  content script of this extension, whose origin is computed in the extension's
  own isolated world and is never taken from anything the page posts on its own
  window. So the value is one this extension produced; a page cannot put an
  arbitrary string into it.

**c. Your archive destination.** Whatever you configured: a directory on your
own disk, or a remote store (S3, SFTP, and the like) whose credentials only you
hold (`crates/chat-stasher/src/config.rs:96`). Content is encrypted
by `rustic` before it is written there, with a master key that is generated and
kept on your machine (`crates/chat-stasher/src/store.rs:261-296,1064-1149`).
A directory written by `export --out` is **not** this: it is a separate,
unencrypted copy, and it is not created unless you run that command.

## 4. Who your data is shared with

**We share nothing with anyone, because we never receive anything.** There are
no third-party processors, no advertising partners, no analytics vendors, no
error-reporting service, and no data sales.

The parties who *do* see something, stated plainly:

| Party | What they see | Why |
|---|---|---|
| **The chat platform** (ChatGPT, DeepSeek, Perplexity, Gemini, Claude, Kimi, Grok) | Your conversations — they host them; they always could. Capture adds no traffic of its own, except on **ChatGPT**, where it requests the full conversation you just opened, and on **Gemini**, where it requests the conversation from its first page and follows the paging token to the end — one request for the first page plus one per remaining page, all on the same route the page itself calls (both same origin, your own session). | `apps/extension/lib/page-hook.ts:698`, `:559-576`; `apps/extension/lib/gemini-capture.ts:150-234` |
| **Your archive destination provider**, if you chose a remote one | Encrypted objects: their **sizes**, **timestamps**, and how many there are. Not the content. This is a real metadata leak: it reveals your archiving rhythm and volume. | `crates/chat-stasher/src/store.rs:261-296`; see `docs/threat-model.md` |
| **Your browser vendor**, possibly | The download-history entry for an export file, *if* you pressed the popup's export button *and* your browser syncs download history to your browser account. **We have not investigated** whether any particular browser does this by default. | `apps/extension/lib/outbox.ts:465-475` |
| **Anything else running on your computer as you** | The plaintext bundles in the extension's outbox, the staged shards, the config, and the master key file. We do not defend against this. | See [Known weaknesses](#known-weaknesses) |
| **Us, the authors** | Nothing. | Section 1 |

One further disclosure about the optional **backfill** feature, which walks your
conversation history to archive older chats. When you turn it on, it issues
additional requests to the chat platform, from your own logged-in session
(`apps/extension/lib/backfill/engine.ts:690`). That produces a request pattern
the platform can see and which does not look like a human reading their history.
**We have not investigated** whether any platform's terms of service prohibit
this, or whether it triggers rate-limiting. Backfill is off unless you enable it
(`apps/extension/lib/backfill/schedule.ts:29`), and with no HTTP port wired the
code refuses to fetch at all rather than defaulting to a live one
(`apps/extension/lib/backfill/engine.ts:103-106`).

Which platforms actually see those extra requests, stated exactly:
**ChatGPT** and **DeepSeek** (conversation list *and* each conversation's
content — on DeepSeek that is one body request per conversation), **Grok**
(conversation list *and* **two** body requests per conversation: a skeleton call
and a content call), **Kimi** (conversation list *and* one body request per
conversation, **both carrying your page's own login token** as described in
step 1 of section 1), **Gemini** (conversation list *and* **one body request per
page of a conversation** — a long conversation is several requests, 1-3 seconds
apart, and a conversation longer than 20 pages is refused rather than archived in
part; all of them carry three values out of the page's own bootstrap blob, read
at request time and held in memory only), and **Perplexity** (**conversation list
only** — backfill never requests the content of a Perplexity conversation, and
therefore never archives one; passive capture is a different leg and sends no
request of its own, it reads the response the page fetches for the conversation
you have open). On **Claude** it is a conversation list, one body request per
conversation, and — only when neither the page's own requests nor the cookie has
named the organization — **one** `GET /api/organizations`.
(`apps/extension/lib/backfill/enumerate.ts:4220-4247`;
`apps/extension/lib/backfill/engine.ts:1949-2001`.) The
practical reading for you: enabling backfill on Perplexity produces list traffic
the platform can see, and produces **no backup whatsoever** on your side. On
DeepSeek it produces list traffic *and* one body request per conversation, and on
Grok two (a skeleton call and a content call — a request pattern the platform is
more likely to notice, although both are the same two calls grok.com's own page
makes when you open a conversation, separated by a 2-5 second pause,
`apps/extension/lib/backfill/enumerate.ts:2806`), and on Kimi one body request —
the same call its own page makes when you open a conversation, carrying the page's
own token as described above (`apps/extension/lib/backfill/enumerate.ts:2954-3016`).
On Claude it is a conversation list and one body request per conversation — the
same call claude.ai's own page makes when you open a past conversation — plus, at
most once for as long as the organization stays unresolved, the organization-list
request described above
(`apps/extension/lib/backfill/enumerate.ts:3534-3546`).
On Gemini, one conversation costs as many requests as it has pages: the leg
follows the continuation token until the response says there is no more, waiting
1-3 seconds between pages, and it refuses (and lists as a failure) a conversation
that would need more than 20
(`apps/extension/lib/backfill/enumerate.ts:3393-3521`; `apps/extension/lib/backfill/engine.ts:1949-2001`).
Its **passive capture** sends requests too, on the same route: a conversation you
open is fetched from its first page and followed to the end, one request for the
first page plus one per remaining page (`apps/extension/lib/gemini-capture.ts:150-234`).
The first of those repeats the request the page had just made, on purpose: the
response the page produced may be any page of the conversation (it asks for older
turns as you scroll), and a copy that started anywhere else could hold only the
oldest turns while looking complete.

## 5. Where the extension runs

The extension's content scripts are injected on an **explicit, closed list of
origins** compiled into the code — never `<all_urls>`, never a wildcard:

- `https://chat.deepseek.com` (`apps/extension/lib/contract.ts:275`)
- `https://www.perplexity.ai` (`apps/extension/lib/contract.ts:323`)
- `https://chatgpt.com`, `https://chat.openai.com` (`apps/extension/lib/contract.ts:391`)
- `https://gemini.google.com` (`apps/extension/lib/contract.ts:407`)
- `https://claude.ai` (`apps/extension/lib/contract.ts:448`)
- `https://www.kimi.com` (`apps/extension/lib/contract.ts:514`)
- `https://grok.com` (`apps/extension/lib/contract.ts:636`)

The list the browser is given is derived mechanically from that table
(`apps/extension/lib/contract.ts:721-723`), so the sites the extension can run
on and the sites it can capture from are the same set by construction — they
cannot drift apart.

**You can verify this yourself without reading the code:** your browser shows
the extension's site access in `chrome://extensions` / `about:addons`, and it
will name these sites and no others. On every other website you visit, this
extension is not running.

Within those sites, not every request is captured. A response is only kept if it
matches the platform's expected route *and* method *and* status *and* body shape
(`apps/extension/lib/contract.ts:821-840`, `:933-941`). A body over 16 MiB is not
captured, and the page console says so rather than dropping it silently
(`apps/extension/lib/contract.ts:743`; `apps/extension/lib/page-hook.ts:374`). No shipped
platform row reads WebSocket frames; every row sets that switch to `false`
explicitly (`apps/extension/lib/contract.ts:319`, `:387`, `:403`, `:444`, `:502`,
`:629`, `:716`).

**What running on a site does *not* mean.** Being on this list means the
extension's content script is injected there. It does not mean your history on
that site gets archived, and — measured on 2026-09-19 in a real browser — it did
not even mean the conversation in front of you was, on two of the seven. On a
logged-in `gemini.google.com/app/<id>` tab, `window.fetch` and
`XMLHttpRequest.prototype.open` were still the browser's own functions, so
nothing of ours had run in that document; on a logged-in
`www.kimi.com/chat/<id>` page, a page-context POST to the messages endpoint was
answered 200 with a `{messages}` body and **no capture was produced at all**.
One cause is fixed on this branch (a same-origin **subframe** of a supported
origin was never injected into, `allFrames` being off, so a request made from
one was invisible); the other — a document that existed before the extension was
loaded or updated, which Chrome will not re-inject into without host permissions
this extension does not request — is not fixable from inside the page, and
**reloading the tab is what resolves it**. Until one of the two is ruled out for
a given tab, treat Gemini and Kimi live capture as **not working on a tab that
predates the extension's load**, and the cause as still under investigation. The
extension does not silently pretend otherwise: it carries no marker saying a
conversation was captured when none was.

The optional backfill feature — the only part that goes
looking for *past* conversations — is limited to a shorter list, and the middle
tier of that list is easy to misread:

| Platform | What backfill does when you enable it |
|---|---|
| **ChatGPT**, **DeepSeek**, **Gemini**, **Grok**, **Kimi**, **Claude** | Lists your conversations **and fetches their content**, one conversation at a time, handing it to the host (`apps/extension/lib/backfill/enumerate.ts:4220-4247`). All six are **implemented, not yet observed completing a backfill in a real browser**. For **DeepSeek** we have **not verified** whether a long conversation comes back complete either, and it does not page the endpoint (`apps/extension/lib/backfill/enumerate.ts:2560-2587`) — but the response is a tree, the extension walks it from its newest message back to a root, and a walk that reaches a message the response does not carry means that conversation is **not archived**; it is recorded as a failure with its own reason code and the leg carries on. For **Grok and Kimi** the same question is unverified with no such check. **Gemini does page**, so for it the completeness question is answered by following the token to the end; what bounds it instead is the 20-page cap, past which the conversation is refused rather than archived in part (`apps/extension/lib/backfill/engine.ts:1949-2001`). Kimi's routes, by contrast, **were** measured in a logged-in session (2026-09-14) and its one body request carries your page's own login token (`apps/extension/lib/platform-auth.ts:258-295`); whether a **long** Kimi conversation comes back complete is **not verified**, and a response that says it holds only part of a conversation is recorded as a failure rather than archived as a whole one (`apps/extension/lib/backfill/engine.ts:2053-2088`). Grok is the least verified: its routes were read from public open-source implementations rather than measured in a logged-in session, and **each conversation costs two requests** — a skeleton call, then a content call built only from the ids that skeleton named (`apps/extension/lib/backfill/enumerate.ts:2869-2930`). |
| **Perplexity** | Lists your conversations and **saves none of their content** — **nothing is delivered or queued, so this history is not backed up** (`apps/extension/lib/backfill/enumerate.ts:4220-4247`). |
| **Claude** | Lists your conversations **and fetches their content**, addressed by the organization resolved as above. Its routes were read from public open-source implementations, **not** measured in a logged-in claude.ai session — nobody has opened claude.ai with this code — so the route shapes are source-backed rather than observed (`apps/extension/lib/backfill/enumerate.ts:3524-3530`). **Each conversation's body is checked for completeness before it is stored**: the response is a tree, the extension walks the active branch from its newest message back to a root, and a walk that reaches a message the response does not carry means that conversation is **not archived** — it is recorded as a failure with its own reason code and the leg carries on (`apps/extension/lib/backfill/enumerate.ts:3675-3731`; `apps/extension/lib/backfill/engine.ts:2124-2137`). Whether a **long** conversation is capped server-side is not established by any source, which is exactly the case that check exists for. |

We state this in a privacy policy because the failure mode is a privacy
expectation, not just a feature gap: a user who believes their Perplexity
history is archived may delete it upstream. Backfill does not archive it: only a
conversation you open or continue in the tab, after the extension is installed,
is captured. A weaker version of the same caution applies to DeepSeek, Gemini, Grok and Kimi:
what backfill stores is only as complete as the body endpoint returns, and we
have not checked a genuinely long conversation against any of them. Kimi is the
one where that case is at least refused out loud: a body response that says it
holds only part of a conversation is not archived and is listed as a failure,
so a long Kimi conversation is missing from your archive rather than silently
half-there (`apps/extension/lib/backfill/engine.ts:2053-2088`). Grok
carries a second caveat of its own: where the sources for its list cursor
disagree, the extension does **not** pick one — a page that repeats what was
already listed stops the leg and says the response shape changed, rather than
being read as "you have no more conversations"
(`apps/extension/lib/backfill/engine.ts:1475-1522`). And because no source says
whether Grok returns a conversation's responses in a stable order, re-opening an
unchanged Grok conversation is more likely than on other platforms to deliver
another copy of it: an extra copy in your archive, never a lost one.

Responses are read from `fetch` and from `XMLHttpRequest`, and both go through
the same capture decision above (`apps/extension/lib/page-hook.ts:356-396`).
An XHR body is read only when the page itself reads it as text or JSON
(`apps/extension/lib/page-hook.ts:507-512`); a binary XHR body (arraybuffer,
blob, document) is never read and only prints a console warning (`:513-516`,
`:254-261`). `EventSource` streams are never read either — the hook only warns
that one was used (`apps/extension/lib/page-hook.ts:547-566`).

## 6. What each permission is for

The extension declares exactly four permissions and no host permissions
(`apps/extension/wxt.config.ts:113`):

| Permission | Why it is needed | What it does **not** allow |
|---|---|---|
| `nativeMessaging` | This is the delivery channel. A captured conversation is handed to the `chat-stasher` binary already on your machine, which you registered per-user with `chat-stasher install-native-host --stage <path>`; the host manifest names exactly one allowed extension id, and the host refuses to serve any other origin. (`crates/chat-stasher/src/nativehost.rs:75-88`, `:341-385`, `:1906-1940`) | It cannot reach any program other than the one host manifest you registered, and that host is the `chat-stasher` binary you installed yourself. There is no fallback channel: without a registered host, captures wait in the outbox instead. |
| `storage` | Persists the items listed in [section 3b](#3-where-your-data-is-stored) — the backfill switch and progress header (so an interrupted backfill can resume instead of restarting; the id list itself is in the `chat-stasher-backfill` IndexedDB database), the last host-status answer, the pause record, and the last-export stamp. (`apps/extension/lib/backfill/store.ts:18-48`) | This is `storage.local` only: `localArea()` reads `browser?.storage?.local` / `chrome?.storage?.local` and nothing else (`apps/extension/lib/backfill/store.ts:85-97`). Nothing is written to `storage.sync`, so nothing here is uploaded to your browser account by us. |
| `alarms` | Gives the backfill leg a periodic heartbeat, so history archiving can finish over days without you having to keep the chat tab open; since the Native Messaging rewrite the same alarm is also when the outbox is drained and retried. (`apps/extension/wxt.config.ts:91`; `apps/extension/lib/backfill/alarm.ts`; `apps/extension/lib/outbox-alarm.ts:20-46`) | It does not grant any network or data access. |
| `unlimitedStorage` | The outbox is an IndexedDB queue of undelivered bundles, capped at 256 MiB by us (`apps/extension/lib/outbox.ts:53`); the backfill id list (`chat-stasher-backfill`, ids only, no conversation text) is a second IndexedDB database. Without this permission Chrome may evict best-effort IndexedDB data under disk pressure, which would mean silently losing captures the user was told were queued. (`apps/extension/wxt.config.ts:103`) | It removes the browser's eviction path for data the extension already stores. It is not a claim on your disk beyond that, and the outbox refuses new captures rather than growing without bound. |

**No permission here shows an install-time warning.** `downloads` — which did
show "Manage your downloads" — is no longer requested at all
(`apps/extension/wxt.config.ts:77`), and none of these four raises one
(`apps/extension/lib/backfill/alarm.ts:16-18`,
`apps/extension/wxt.config.ts:113`). The one thing Chrome does tell you at
install time is that this extension can *"communicate with cooperating native
applications"*, which is what `nativeMessaging` means and is disclosed here
rather than left for you to discover.

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
would let it reach a collection endpoint (`apps/extension/wxt.config.ts:113`).
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
| Bundles in the extension's outbox | Until the host answers a matching `ack`, which deletes the record (`apps/extension/lib/outbox.ts:379-394`). A record the host **refused** outright is kept and never retried. **If the host is never reachable, they stay indefinitely, in plaintext.** | Uninstall the extension, or clear its site data in your browser; there is no per-record delete button. |
| An export file you triggered | Until `ingest` consumes it, which moves it to `<inbox>/consumed/` once every line was sealed or found to be a duplicate (`crates/chat-stasher/src/inbox.rs:57-60`). | Delete it from your download directory with your file manager. |
| Browser download-history entry for that export | Until you clear your browser history | Clear downloads in your browser's own history UI |
| Extension local storage (backfill progress, the alarm's last-wake trace, the last host status, the pause record, the capture-hook records and the last-export stamp) | Until you clear it or uninstall the extension | Uninstalling the extension removes it; browsers also expose per-extension site-data clearing |
| Staged shards | Until `push` moves them into the repository | Delete the stage directory you chose |
| A directory you exported to | **Until you delete it.** `export --out` writes the selected sessions there decrypted, and nothing — not `push`, not `ingest` — moves them on (`crates/chat-stasher/src/main.rs:521-601`). | Delete the directory you named. `--out` must be empty or absent unless `--force` is given, and the command deletes nothing, so nothing of yours is lost by pointing it at a directory you later remove. |
| Your archive repository | **Indefinitely, by design.** This is a backup tool: it exists so that history a platform deleted still survives. | Delete the repository directory or remote bucket yourself. **There is no `delete` subcommand and no command that restores sessions into a harness's own directories in this version** — the subcommand list is `init`, `run-once`, `schedule`, `push`, `status`, `read`, `doctor`, `verify`, `dest-init`, `search`, `export`, `ui` (`view` is a deprecated alias), `ingest`, `collect`, `seal`, `reclaim-stage`, `install-native-host`, `native-host`, `activity-index`, `machine-declare`, `machine-label`, `overview` (`crates/chat-stasher/src/main.rs:130-991`). Selective per-conversation deletion inside an archive is not implemented. |

**Uninstalling the extension stops all capture immediately** and removes its
local storage, which is where the outbox lives — so uninstalling also deletes
every capture that had not been acknowledged yet. It does not delete the staged
shards or your archive: those are yours, and deleting your backup without being
asked would be the worse failure.

## Known weaknesses

A privacy policy that lists no weaknesses is more dangerous than no policy at
all, so here are the ones that bear on your privacy. The full list is in
[`docs/threat-model.md`](threat-model.md).

**1. The plaintext window before delivery.** The extension writes each captured
session as an ordinary, unencrypted record into its outbox database, inside your
browser profile (`apps/extension/lib/outbox.ts:64-80`, `:309-377`). That record
contains the conversation itself. It sits there, readable by anything running as
your user, until the host acknowledges it — and a record the host refused stays
until you uninstall. **We do not encrypt it, we do not restrict its permissions,
and we do not shorten that window.** How long it is depends entirely on how
often the host is reachable; if it never is, the plaintext stays indefinitely.
A browser profile on an encrypted volume is what protects it at rest.

*What you can do today:* make sure the popup says "connected" so deliveries
succeed, uninstall the extension if you are done with it, and keep your browser
profile on an encrypted volume.

**2. We do not defend against a hostile program running as your user.** Anything
running as you can read the plaintext bundles in the extension's outbox, the
staged shards, your config, and — with your archive — decrypt everything. On a
single-user desktop this is the normal situation; on a shared machine it is the
dominant risk.

**3. The master key is the only key, and losing it is unrecoverable.** There is
no escrow, no recovery code, no maintainer-held copy, and no password reset — by
design, because any of those would mean someone other than you could open your
archive (`crates/chat-stasher/src/store.rs:1189-1196,1151-1155`). The key file
is written owner-only (`0600`) on Unix; on platforms without Unix modes it
inherits whatever the filesystem gives it
(`crates/chat-stasher/src/store.rs:1231-1316`).

**4. What other browser extensions can observe is unresolved.** We did not test
whether a second, hostile extension with broad host permissions on a chat origin
can observe our in-page hook or the `window.postMessage` traffic between our
page hook and our bridge, and we did not test whether an extension can reach
another extension's IndexedDB. Treat this as **potentially exposed, not safe**.
See the extension-ecosystem row of [`docs/threat-model.md`](threat-model.md).

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
