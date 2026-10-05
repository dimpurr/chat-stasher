# Privacy Policy — Chat Stasher

**Last updated: 2026-10-03.**

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
[`docs-dev/threat-model.md`](threat-model.md). This policy is the short answer;
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
  **ChatGPT**, **DeepSeek**, **Gemini**, **Grok**, **Kimi**, **Perplexity** and **Claude** — on
  none of the seven has a complete backfill been observed in a real browser, and
  for DeepSeek, Gemini, Grok, Kimi, Perplexity and Claude we have **not verified**
  whether a long conversation comes back complete (Gemini is the one of those
  that pages; the others do not, and say so).
  Grok and Claude are the least verified of the seven: their routes come from reading
  public open-source implementations rather than from a logged-in session,
  and one Grok conversation costs two requests. Kimi's routes *were* measured in a
  logged-in session, and its requests carry your page's own login token, read
  from the page and held in memory only (step 1 of section 1). **Gemini's were
  measured too**, and its requests carry three values out of the page's own
  bootstrap blob — read at request time through the page-world hook, held in memory
  only, and attached to its two RPCs and nothing else (`apps/extension/lib/platform-auth.ts:590-628, 635-645, 769-772`; `apps/extension/entrypoints/dw-bridge.content.ts:557-575, 593-603`; `apps/extension/lib/page-hook.ts:1103-1146`).
  Claude is the one platform whose requests are addressed by an account-scoped
  identifier the page URL does not carry; the extension resolves it from the
  page's own requests, the browser's cookie, or one extra request, and stops
  rather than choosing when an account has several organizations (section 5).
  On **Perplexity** it lists your conversations **and fetches their content**,
  since W84 (2026-09-23) — with each conversation checked for completeness before
  it is stored (section 5). See
  [section 5](#5-where-the-extension-runs).
- **Everything is stored on your machine or at a destination you configure**
  (a local disk, or a remote store whose credentials only you hold).
- **The optional local full-text index is plaintext.** `index build` reads
  changed archived sessions into a destination-scoped SQLite cache under the
  operating-system cache directory; `index clear` removes that cache
  (`crates/chat-stasher/src/fts.rs:1-6,1462-1682,1684-1699`).
- **The snapshot session cache is plaintext too, but holds identifiers rather
  than text.** A repeated `search` keeps each snapshot's session list — session
  ids, the machine partition, shard counts and byte sizes — in a
  destination-scoped cache under the operating-system cache directory, so a
  repeat search does not walk every snapshot again. No conversation text and no
  decrypted byte is stored, the root and its files are owner-only (0700 and
  0600, the same as the index), and deleting the directory costs nothing but the
  next search's speed: no part of the archive depends on it
  (`crates/chat-stasher/src/snapshot_cache.rs:1-66,587-621`).
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
   (`apps/extension/lib/page-hook.ts:829`, `:869`, `:671-718`). Only responses
   matching a known platform route are kept
   (`apps/extension/lib/contract.ts:356-852`, `:1085-1129`).
   **One exception, on ChatGPT.** When you move between conversations inside
   the page, ChatGPT now loads only the most recent part of a conversation.
   Keeping that part would store an incomplete conversation, so it is never
   kept; the extension instead requests the full conversation itself, from your
   page, on the same origin (`apps/extension/lib/page-hook.ts:850-855`;
   `apps/extension/entrypoints/dw-bridge.content.ts:694-722`). That request —
   and every backfill request to ChatGPT's conversation list or a conversation
   body — carries your session's access token, which the extension reads from
   ChatGPT's own `/api/auth/session` on the same origin
   (`apps/extension/lib/platform-auth.ts:47`, `:92-107`). The token is held only
   in the page's content-script memory: it is never written to storage, never
   logged, never sent to the `chat-stasher` host, and never attached to any
   other request (`apps/extension/lib/platform-auth.ts:61-71`, `:113-165`).
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
   conversations” (`apps/extension/lib/platform-auth.ts:303-357`,
  `apps/extension/entrypoints/dw-bridge.content.ts:507-513`).
2. **Queue on your machine.** The extension writes that text, as a JSON bundle,
   into its **own IndexedDB outbox** — extension-local storage on your disk,
   keyed by the SHA-256 of the bundle (`apps/extension/lib/outbox.ts:35-38`,
   `:365-437`). It does this *before* attempting any delivery, so a service
   worker killed between "the page produced bytes" and "the host answered" cannot
   lose a conversation without a trace
   (`apps/extension/entrypoints/background.ts:378-395`).
3. **Deliver to the local host.** The extension hands the bundle to a Native
   Messaging host — the `chat-stasher` binary **you** registered with
   `chat-stasher install-native-host --stage <your-stage>` — with
   `runtime.sendNativeMessage`
   (`apps/extension/lib/native-host.ts:32`, `:769-819`). The host seals it into
   the stage you configured, using the same code path and the same guarantees as
   `ingest` (`crates/chat-stasher/src/nativehost.rs:2992-3003`).
   🔴 **The bundle is deleted from the outbox only when the host answers an
   `ack` whose `request_id` and `sha256` equal the ones sent.** A `nack`, a
   timeout or a disconnect leaves it queued
   (`apps/extension/lib/native-host.ts:1118-1127`;
   `apps/extension/lib/outbox.ts:440-455`).
4. **Push.** `push` writes the staged shards into a `rustic` repository —
   encrypted — at a destination **you** configure, local or remote
   (`crates/chat-stasher/src/main.rs:329-366`;
   `crates/chat-stasher/src/store.rs:318-418`).

Steps 1–3 happen entirely on your machine, in plaintext. Step 4 is the only
step that can involve a network, and the only destination it can reach is the
one you put in your own config. The Native Messaging hop in step 3 is a local
process-to-process call: it is not a network connection, and the host it reaches
is one you registered yourself.

**There is no step in which anything is sent to the authors of this software.**
That is not a promise we are keeping — it is a property of there being no such
link in the code.

### What the host answers back

Three answers travel the other way — from the binary you installed to the
extension you installed. All are read-only, and none carries a conversation:

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
  not printed (`contracts/nativehost-protocol.md` §6.5; `docs-dev/threat-model.md`,
  "The local dashboard").
- **`has`** — one question, *is this exact content already stored?*, asked before
  the extension spends a delivery on a conversation its own record says it may
  already hold. The request names that conversation's platform and session id and
  the SHA-256 `fingerprint` the extension computed from the capture body — values
  the asking side already holds; the answer is `held`, true or false, plus the
  matching shard's file name when something holds it. The host writes nothing,
  takes no lock, and looks only in the directory a `deliver` of that same
  conversation would write to; a directory it could not read is a `nack`, never
  a `held: false`, so "asked and could not look" stays distinct from "asked and
  answered: nothing there holds it" (`contracts/nativehost-protocol.md` §6.6;
  `crates/chat-stasher/src/nativehost.rs`). Nothing the answer carries is news
  to the asker: it sent the only id, and the rest is one bit and a file name.

No answer leaves your machine, and none reaches us: they travel one hop,
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
  (`apps/extension/wxt.config.ts:125`). An
  extension with no host permissions cannot make requests to a server of ours;
  the only network the code can touch is inside the pages it is already injected
  into. There is no origin belonging to this project anywhere in the extension.
- **Check the network tab.** Open your browser's developer tools on a chat page
  and watch the requests. On every platform except ChatGPT, capture is
  passive: the hook reads a clone of a response the page already fetched, and
  adds no request of its own. On ChatGPT it adds one same-origin request for
  the full conversation when you move between conversations in the page, plus
  one to `/api/auth/session` for the token (see step 1 of section 1)
  (`apps/extension/lib/page-hook.ts:869`, `:671-718`; the extra request and the
  token it carries: `apps/extension/entrypoints/dw-bridge.content.ts:694-722`,
  `apps/extension/lib/platform-auth.ts:47`, `:92-107`). The one feature that does
  add requests, backfill, is off unless you turn it on — see
  [section 4](#4-who-your-data-is-shared-with).
- **Check the code for a tracker.** Searching the extension and CLI sources for
  `analytics`, `telemetry`, `sentry`, `gtag`, `mixpanel`, `posthog`, and
  `amplitude` returns **zero matches** in `apps/extension/lib`,
  `apps/extension/entrypoints`, and `crates/chat-stasher/src`. There is no
  analytics SDK to configure, disable, or trust.
- **Check the Firefox data-collection declaration.** The add-on declares
  Mozilla's data-collection field as `none` (`apps/extension/wxt.config.ts:148`).
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
is, the conversation itself (`apps/extension/entrypoints/background.ts:268-271`;
`apps/extension/lib/outbox.ts:102-119`, `:365-437`). The bundle also carries an
**account fingerprint** beside the identity described below: not the account id
and not an email, but a keyed digest of the platform's account id, computed
in the extension with a random per-install salt
(`apps/extension/lib/account-fingerprint.ts:416-435`;
`apps/extension/lib/contract.ts:1334-1343`). Its purpose is to make two accounts
distinguishable in your archive; unlike the identity, it cannot be turned back
into the account id, and when no account id is available the bundle says so
explicitly rather than carrying a value
(`apps/extension/lib/contract.ts:1297-1306`).

🔴 W239 · **Where a platform files conversations under an _organization_ rather than an
account, no fingerprint is recorded at all.** claude.ai addresses every conversation by
organization, and two accounts can be members of one organization — so a value derived
from it would be *equal* for both, and recording it would tell your archive that a
conversation from one account came from the other. Those bundles carry
`organization-is-not-an-account` instead, and no secret is created to key a value that is
not produced. The organization is not lost by this: it is already in the bundle's own
`url`, and it is the scope this browser's backfill progress is filed under. It is simply
not called an account, because it is not one.

🔴 W299 · **On ChatGPT the fingerprint's input is a header on the captured request, not
the response body.** ChatGPT names no account in the conversation JSON, so before W299
those bundles carried `unknown`. The page's own request does name one — the
`ChatGPT-Account-Id` header — and the extension reads it on the exact request whose
response it captured, hashes it with the per-install HMAC key and then discards it
without writing the raw value anywhere: not into the bundle, an outbox record, an
export, a log line or a native-host message. Malformed or oversized header metadata is
`unknown`; it never rejects an otherwise valid capture, and it cannot fall through to
a body id. Current captures also record when the header is absent and remain `unknown`;
only a legacy capture predating that presence marker may retain its former body-axis
behavior. What the archive then holds is that a value
was carried and whether two captures carried the same one — two workspaces that share a
value produce one fingerprint — so it distinguishes exactly what the header
distinguishes, and it is not proof of a person. The header is not the outbox's `accountId`
field described below: that field stays the namespace a request addresses. A separate
workspace-observation route fingerprints the same header before storing a backfill
target, ledger scope, or sending the scope to the native host. Existing raw
`chatgpt:<workspace>` targets and ledger headers are migrated with pending debt IDs
preserved under the fingerprinted scope; the old storage keys are removed. No backfill
lease is derived from the capture fingerprint in this build. The
bundle's `account.source` names the mechanism (`request-header-chatgpt-account-id`), so a
reader can tell which value was hashed.

W218 adds a second, cross-install key for account-scoped host arbitration. The
extension keeps the raw platform account id as a separate local outbox field
(`accountId`), outside the JSON payload and export file. 🔴 W239 · For a platform
whose requests are addressed by an _organization_, this field carries that
organization — it is the namespace the request itself names, which is what the
host's per-namespace budget is keyed by, and it is not presented anywhere as an
account identity (see the paragraph above). Native messaging sends
that value only to this machine's host. The host derives a domain- and
platform-separated HMAC key from the configured archive masterkey; it never
returns the masterkey or derived key to the extension. The derived key is stored
in the host's local coordination database and as sealed shard metadata, which
is covered by archive encryption. 🔴 It is also the only account-scoped value in
this design that is deliberately comparable across your own installs and your own
machines — the install-local fingerprint above is not, by construction —
so it is what lets one machine recognise the same account as another with no
account id in the comparison. The records that do the recognising — rows of
the machine-local `extension-coordination.sqlite3` that section 3b below names —
hold a platform id, an install id, times and this key, and no raw account id in
any form. What they compare is one stored key against another, a comparison the
masterkey is not needed to make: the host itself makes it when counting how
many installs recently showed one key, and any other process running as you on
this machine can make it too. Crossing from a stored key to an account id is
the step that needs the masterkey: without it, a stored key can be neither
derived from an account id nor matched against one, so two matching stored
keys say "the same account twice" and never which. None of this keeps the
account id out of your archive: the sealed shard record of a capture still
carries the account id verbatim, in its separate `identity` field, whenever the
extension could find one — it reaches that record from the bundle itself, not
from this key — and that field is never what these comparisons match on.
Without an unambiguous readable masterkey,
the extension still delivers normally and coordination falls back to the
existing platform scope (`apps/extension/lib/outbox.ts:102-112`,
`apps/extension/lib/native-host.ts:1083-1127`,
`crates/chat-stasher/src/nativehost.rs:1272-1346`, `:1507-1522`, `:2839-3054`,
`crates/chat-stasher/src/inbox.rs:510-520`, `:879-881`).

The bundle also names the **install** that captured it: three fields — a random
UUID minted once per extension install, the browser family read from this
browser's own navigator, and the label you gave this browser profile in the
popup (until you name it, the literal `Unnamed profile`, which the bundled
schema documents as claiming nothing about which profile produced the capture)
(`apps/extension/entrypoints/background.ts:244-254`;
`apps/extension/lib/install-identity.ts:1-23`). Their purpose is the topology
this extension lives in — one user, several machines, several browsers, several
profiles per browser: the sealed shard record keeps them beside a `machine`
name the host itself assigns, so your archive can say which install produced a
conversation (`crates/chat-stasher/src/inbox.rs:447-452`,
`:509-517`, `:880-883`). 🔴 A copied browser profile brings its copied
`install_id` along, and the stage can tell: a delivery whose identity names a
different browser, or a different label the user actually named, while the same
`install_id` was already sealed under another is refused — the capture stays in
your outbox, listed there as rejected with the refusal's own instruction, and
is never merged with the first install's record
(`crates/chat-stasher/src/inbox.rs:839-849`, `:918-979`;
`crates/chat-stasher/src/nativehost.rs:3069-3073`;
`apps/extension/lib/outbox.ts:458-503`). The label is a name you typed, and it
is plaintext wherever the bundle is — the outbox record, the export file, the
staged shards — exactly like the account fingerprint; this extension transmits
it nowhere but to your own host.

🔴 **The case that check cannot see is caught from the other side, and that is
EXT-13.** Copying a profile copies this storage, so the copy starts with the same
`install_id` *and* at the same point in a counter — but not with the same future.
Each install keeps a counter that rises with every status report and every
delivery, and mints a fresh random value beside each number, written down before
the message that carries it is sent
(`apps/extension/lib/report-seq.ts:1-36`, `:45`). Two live copies therefore
eventually reserve the same number with two different random values, and that
pair is the only positive evidence your host can have that two writers share one
identity; a repeated number on its own is not, because reports legitimately
arrive out of order, a send is retried and a worker restarts
(`crates/chat-stasher/src/nativehost.rs:1712-1729`, `:1833`). The limit is stated
in the code as plainly as it is here: **until two copies have each reserved the
same number they are indistinguishable**, so a copied profile whose copies have
never both reported looks exactly like one install
(`crates/chat-stasher/src/nativehost.rs:1938-1940`). When the host does see it,
your captures are kept **queued** rather than filed: the bytes are not wrong,
they are unattributable, so nothing is archived under an identity the host cannot
name and nothing is thrown away. The popup in *each* conflicting copy then offers
the one repair — giving that profile a new identity — and nothing rotates an
identity on its own. The repair rewrites no history: captures already archived
keep the identity they were sealed with
(`apps/extension/lib/install-identity.ts:71-110`;
`apps/extension/lib/popup-view.ts:529-545`;
`apps/extension/entrypoints/background.ts:3299-3312`). This is also why a shared
`install_id` on two *different* machines cannot be noticed by either machine: it
becomes visible only where both records are read together, in your archive
(`crates/chat-stasher/src/overview.rs:688-745`).

**One more thing leaves this browser besides captures: a status report.** It is
sent at the end of a backfill tick: the install id, the browser, the
profile label, the extension version, a report time, and one row per platform
saying how many captures this browser confirmed, how many are still pending,
why a leg is paused, and — for a row that has one — that account's
install-local fingerprint. Your
host writes it into the stage as `ext-status/<machine>/<install_id>.json`, and
`push` puts it into your archive with everything else
(`apps/extension/entrypoints/background.ts:3067-3096`;
`crates/chat-stasher/src/nativehost.rs:2572`, `:2597-2613`;
`crates/chat-stasher/src/metahash.rs:1-12`). It is metadata only — counts, codes,
a version string and timestamps — and it carries no conversation text, no session
id and no account scope label.

For platforms with a known volatile field (ChatGPT's `safe_urls` today), the
bundle since W213 also carries a **content fingerprint**: sha256 of the raw
body with that volatile field removed, so two copies of one conversation that
differ only in a rotating URL count as the same content
(`apps/extension/lib/recapture.ts:95-116`, `:223-237`). It is derived from the
body the bundle already carries, adds nothing a reader of the bundle did not
already have, and exists so the export-import escape hatch keeps the archive
able to answer "is this exact content already stored?" for a bundle that never
reached the host live (`contracts/nativehost-protocol.md` §8, W213): the host
records it beside the bytes on the sealed shard and compares it as a string
(`crates/chat-stasher/src/inbox.rs:528-550`).

A ChatGPT bundle also carries **project provenance**: the workspace the
conversation was fetched under and the project it belongs to, as the capture leg
recorded them (`contracts/inbox.schema.json:180-198`). 🔴 A capture taken before
the project was known records the literal `unknown` rather than leaving the field
out — "not learned yet" and "this conversation belongs to no project" are
different facts and stay different. A later observation may add a **supplement**
beside that record: the project a source reported, the source's name, and the
time it was observed; it never replaces what the capture recorded, so the archive
shows both what was known then and what was learned afterwards
(`contracts/inbox.schema.json:210-222`; `crates/chat-stasher/src/activity.rs:2435-2485`). A project name is a label from
the platform rather than conversation text, but it is still **yours** and still
plaintext: it sits in the bundle, in the staged shards and in the activity index
beside everything else this section describes. A page cannot author either field
— a capture payload carrying provenance is rejected outright
(`apps/extension/lib/contract.ts:1116-1117`) — and, like the account fingerprint,
these values are written into your own archive and are not transmitted anywhere
by this extension.

The database is named
`chat-stasher-outbox` and lives under the extension's own origin; uninstalling
the extension removes it with the rest of the extension's storage. **Its
contents are not encrypted.** A record stays there until the host acknowledges
it, and a record the host refused outright is kept until you delete it by
uninstalling. See [Known weaknesses](#known-weaknesses).

A capture has a second plaintext copy if you press the popup's **export**
button: that writes one
`chat-stasher-export-<UTC>-<install-short-id>-<nonce>.jsonl` file (since W213
the name carries the producing install's short id and a per-export nonce, so
two browser profiles exporting in the same second cannot overwrite each
other's file), one undelivered bundle per line, into your browser's download
directory (`apps/extension/lib/outbox.ts:540-621`). That file is an ordinary
download, so your browser keeps a download-history entry for it — just its name
and timestamp, not its content. We do not delete it; `ingest` retires it to
`consumed/` when it has consumed every line
(`crates/chat-stasher/src/inbox.rs:57-60`).

The CLI makes one plaintext copy too, and it is not a capture but an archive
session: `chat-stasher export --out <dir>` writes the sessions it selected back
out **decrypted**, one file per session, into the directory you name
(`crates/chat-stasher/src/main.rs:686-772`). Nothing moves those files on and
we keep no record of where they went, so deleting the directory is yours to do.
The CLI writes no archive content anywhere you did not name.

When `collect` reads a session, it also records a normalized **source-path
class** in the stage metadata: `main` or, for a Claude Code subagent, `subagents`.
The subagent row carries the parent session's native id as a reference. It does
not store the source path, and collection leaves the session's raw bytes alone.
The activity index carries these fields into the archive, and `search --json`
returns them with each matched or unplaced session that has a provenance row;
older archives without one omit the field
(`crates/chat-stasher/src/activity.rs:132-213`,
`crates/chat-stasher/src/collect.rs:1207-1219`,
`crates/chat-stasher/src/main.rs:2709-2717,2781-2789`,
`crates/chat-stasher/src/search.rs:749-795`). The parent id is still session
metadata in plaintext in the stage and inside the encrypted archive; it can link
a subagent to its parent conversation.

**b. Your browser's local extension storage** (`storage.local`, never
`storage.sync`: no `storage.sync` call exists anywhere under `apps/extension`,
so nothing here is synced to a browser account by this extension).
What is kept there:

| Key | What it holds | Citation |
|---|---|---|
| `cs_backfill_enabled_v1` | Whether you turned the history-backfill feature on | `apps/extension/lib/backfill/schedule.ts:40` |
| `cs_backfill_targets_v1`, `cs_backfill_tabs_v1` | Which site/tab the backfill timer should wake up for. 🔴 The target registry is a bounded cache (eight rows): registering a ninth target evicts the seat that has waited longest — an organization row can be the one that leaves — and an eviction follows the same rule as any dropped row: the row leaves *together with* its `cs_backfill_v2:<platform>:<scope>` header, so an evicted scope leaves nothing behind that a lost registry row would otherwise leave hidden; the host archive is untouched and that account's next capture registers it again. 🔴 W54b · Registration also attempts to record each cap eviction in `cs_backfill_evicted_v1` (next row), the receipt the popup reads beside the registry state. This receipt is best effort: if its read or write fails, the code warns and registration still completes, so that eviction may have no durable receipt. | `apps/extension/lib/backfill/alarm.ts:244-245, 832-842, 844-883, 1003-1058, 1062-1100`; `apps/extension/lib/backfill/tab-port.ts:181-184` |
| `cs_backfill_evicted_v1` | The registry's **eviction record**: one entry per seat the eight-row cap has pushed off — the evicted row's platform, the scope that row held, the reason code (`registry-cap` in this build), and when the evicting registration happened — newest first, bounded at 16 entries, with the number of records that bound has itself pushed off kept as a count on the record. 🔴 Only cap evictions enter it: the deliberate collapse of a non-organization row (the Claude bullet below) is written nowhere near it, because such a row's scope can be a leftover conversation *title* — user content a deliberate replacement must not leave a record of. 🔴 The cap's slice has no such exclusion: it takes the seat that has waited longest without inspecting the row, and the receipt copies the evicted row's scope exactly as the registry held it. A pre-W31 claude.ai row whose scope is such a leftover title can therefore still be sitting in the registry, and the write that pushes it off the cap is a registration for **another** platform: claude.ai is the one platform whose registrations go through the collapse, which removes such a row before the cap ever sees it, while every other platform's capture or start-button registration goes through the plain writer that records whatever its cap pushed off. That is the one case in which this receipt holds user content — a conversation title as one entry's copied scope, in storage and on screen, because the popup's eviction note prints the scope between platform and reason. This build never writes a title as a claude.ai scope — a registration that cannot name the organization stores the `default` sentinel instead — and whether a stored row is a pre-W31 leftover is not something this build can tell: a registry row records platform, origin, scope and a time, nothing that names the build that wrote it. Nothing else of a conversation enters the receipt: the other fields are platform ids, reason codes and timestamps, and no entry ever carries a conversation *body*. The popup shows entries for platforms this build serves; an absent or unparseable outer record produces no note. Malformed entries are skipped, and a missing or invalid dropped-count is read as zero, so malformed stored data can understate the receipt history. | `apps/extension/lib/backfill/alarm.ts:729-744`; `apps/extension/lib/backfill/alarm.ts:876-883`; `apps/extension/lib/backfill/alarm.ts:894-1001`; `apps/extension/lib/backfill/alarm.ts:1003-1058`; `apps/extension/lib/backfill/alarm.ts:1062-1100`; `apps/extension/lib/backfill/alarm.ts:1194-1239`; `apps/extension/entrypoints/background.ts:1348-1378`; `apps/extension/lib/backfill/claude-org.ts:112-138`; `apps/extension/lib/popup-view.ts:1086-1105` |
| `cs_backfill_v2:<platform>:<scope>` | The backfill progress header: list cursor, counters, daily count, halt record, the record that a platform's id list had to be read again, and — while a stored stop is being re-decided — which build has already spent that one re-decision (a version string and a timestamp, nothing else). Since W128 step 2 it also carries this scope's **account lease** — the same irreversible fingerprint described above, plus the opaque id of the salt it was computed with and which mechanism read the id — and, when the scope was stopped because the responses were coming from a different account, the suspended record that holds it until its own account is seen again; both carry fingerprints and never an account id (`apps/extension/lib/backfill/types.ts:1747-1757`, `:1895-1915`). The **conversation/session ids** themselves (archived and still pending) are kept one record per id in a second IndexedDB database, `chat-stasher-backfill` (object store `debts_by_platform`), so settling one conversation does not rewrite the whole list. 🔴 Each id record is keyed by **platform, account scope and id together**: the platform is part of the key because two platforms can share one scope string, and a key without it let one platform's ordinary ledger write delete another's ids. An older `cs_backfill_v1:<platform>:<scope>` record is migrated once and removed only after the new layout has been written and read back. Ids written before the platform became part of the key sit in the older `debts` store in the same database until the platform that owns them can be established from the rest of your storage; a row whose platform cannot be established is left there, uncounted and undeleted. | `apps/extension/lib/backfill/types.ts:1917-1957, 1968-1994`; `apps/extension/lib/backfill/ledger.ts:738-749, 755-838`; `apps/extension/lib/backfill/debt-store.ts:27-36, 103-130, 846-922` |
| `cs_backfill_day_slow_v1` | The one record this profile keeps when a platform answers a request with **429 Too Many Requests**: for each platform that did so, when the refusal was seen and the instant the slowdown ends — the next **local** midnight. While it is in force every request that platform gets is made more slowly: the gaps of whichever speed preset is chosen are doubled, and one tick fetches at most one body. It holds a platform id and two timestamps — no account, no conversation — is overwritten by the next refusal, and expires on the clock, so nothing has to clear it. | `apps/extension/lib/backfill/day-slow.ts:91, 112-124, 135-150, 181-197, 249-310` |
| `cs_native_host_status_v1`, `cs_native_host_pause_v1` | The last `hello` answer (stage, machine id, host version, or the named reason it failed) and the record that says the backfill leg is paused | `apps/extension/lib/host-status.ts:24-53`, `:114-138` |
| `cs_ext_coordination_unavailable_v1` | A Boolean popup note recording whether this profile's most recent backfill tick found coordination unavailable. It is true when the host cannot answer EXT-3 and false after a successful coordination claim; a tick without a request channel preserves the prior result. It stores no platform, account, or conversation data. | `apps/extension/entrypoints/background.ts:915-924` |
| `extension-coordination.sqlite3` in the native host's local state directory | Machine-local backfill coordination metadata: platform ids, install ids, masterkey-derived HMAC account keys when available, last-seen and lease/cooldown/request timestamps, and a per-day detail-request count. It contains no raw account id, conversation id, title, or message content. Install sightings older than 30 days are pruned when coordination runs. | `crates/chat-stasher/src/nativehost.rs:1236-1311`, `:1417-1423`, `:1507-1522` |
| `cs_outbox_last_export_v1` | The time, size and file name of the last export you triggered | `apps/extension/lib/outbox.ts:70-71`, `:623-647` |
| `cs_account_salt_v1` | The random 32-byte secret every account fingerprint is keyed with, plus an opaque per-install id and the time it was created — nothing else, and no account id in any form. It is what makes a fingerprint irreversible: the value in your archive cannot be turned back into an account id without this secret, which never leaves this browser profile and is never synced. 🔴 Two installs, two profiles, or one install whose record you delete produce **incomparable** fingerprints for the same account; a fingerprint is only ever meant to be compared with one carrying the same id, and a mismatch there is not evidence of an account switch. A stored record this build cannot read is reported as `salt-unreadable` and deliberately **left alone** rather than replaced — re-keying would change every later fingerprint and make one unchanged account look like a switch. | `apps/extension/lib/account-fingerprint.ts:53`, `:156-191`; `apps/extension/lib/contract.ts:1326-1330` |
| `cs_backfill_lasttick_v1` | The trace of the most recent backfill alarm wake: when it was, whether it ran, the named outcome, and how many backfill targets were registered. 🔴 It also carries **how that tick ended** — the run's own stop reason (`stopped`), the halt it left behind (`halted`) and that halt's `detail`, or, for a tick that stopped before making any request, that tick's own named outcome. And whether that tick **swept open tabs** for a live page the registry had lost (`tabSweep`): `null` if it never swept, `{ looked: false }` if it could not list tabs, `{ looked: true, queried, pruned, pinged, registered, deferred, crowded }` if it did — counts only, so "we looked and found nothing" stays distinct from "we never looked", a sweep that hit its ping cap (`deferred > 0`) stays distinct from one that pinged everything it wanted to, and a sweep that refused an answering tab for want of a slot (`crowded > 0`) stays distinct from both. 🔴 The field also has a fourth value that is **not an outcome**: `{ sweeping: true }`, written by the provisional record the tick saves *before* its sweep starts, says this tick has no sweep result yet. It exists so that an interrupted tick is never recorded as one that never looked — that provisional record is the one that stays if the browser reclaims the worker mid-sweep, if the sweep throws, or if the tick's final save fails, and `null` there would have been a false "this tick never swept" about a tick that did. It is replaced in the same tick by one of the three values above whenever the tick finishes, and the popup says the tick had not finished rather than reading it as a skip. 🔴 W76 · It also carries **which target that wake served, and which ones it passed over and why** (`schedule`): the platform id the wake ran (`served`), and, for each target the walk examined and did not run, that platform's id beside the code it was passed over for — `no-http-port` (no open page for it), `halted` (a stop that still applies) or `waiting-retry` (a transient stop still inside its backoff). The walk orders targets by least-recently-served rank; never-seen targets join at the back and registry order breaks ties. Platform ids and reason codes only: no account scope, no origin, no free text. Metadata only: reason codes, counts and timestamps. The one free-text field is the halt's `detail`, and by construction it names storage keys, paths, HTTP statuses and counts — never a conversation id, title, or body. One record, overwritten by the next wake. | `apps/extension/lib/backfill/alarm.ts:1577-1679`, `:1682-1761`; `apps/extension/entrypoints/background.ts:2994-3059`, `:2964-3034` |
| `cs_hook_v1:<origin>`, `cs_hook_declined_v1` | A top frame's own report about its capture hook: one record per origin, holding each observation with **when it was first made and when it was last made**. The two differ because a page that stays in one state re-sends that state every few seconds to say it still holds; the latest observation is the state that page is in (an older one is a state it has moved out of), and a capture that arrived after the latest one *began* is evidence about it. A child frame's observation is not stored — it is a statement about that frame, not about the origin. 🔴 And the one report that was **received and not recorded**, with the check that refused it, the origin and observation when they are known, how many times in a row the same refusal has repeated, and when. Metadata only: reason codes, a count, an origin string, timestamps; no URL path, no conversation id, no body, no token. Unlike the per-origin records, the declined one is a single record, overwritten by the next decline. | `apps/extension/lib/hook-status.ts:78-155`, `:289-418`; `apps/extension/entrypoints/background.ts:3400-3469`, `:3382-3456` |
| `cs_last_delivered_v1` | One entry per conversation this profile has delivered: the delivery name it went out under, and the **content fingerprint** of the response — a SHA-256 over the response body with that platform's known volatile fields removed, so two views of one unchanged conversation share it. Nothing else: no URL, no title, no account, no conversation text, and — since W50c — no stage or machine id either, because where a copy went is answered by the host from its own archive rather than remembered here. 🔴 Its only use is to decide whether asking the host is worth a round trip (`has`, "What the host answers back" above). It cannot by itself mark a conversation archived: the extension asks the `chat-stasher` binary still, and only a positive answer from the stage the host is writing to now is acted on. Up to 2000 entries, oldest forgotten first — an entry that has been forgotten costs one delivery of a conversation the archive may already hold, never a skipped one. | `apps/extension/lib/recapture.ts:46`, `:103`, `:274-304`, `:342-354` |
| `cs_live_capture_v1:<platform>` | When a live capture from that platform was last **confirmed to be in your archive**, and how many are **on record as newly stored** — one record per platform. It exists because nothing else said when a live capture had last arrived: `cs_last_delivered_v1` (above) maps a delivery name to the fingerprint of the response it stored and carries no time at all, so "did a capture arrive at time T" was a gap in the record. It is written at the one place the live leg decides a capture was **stored**, so a capture that was merely queued, rejected or refused leaves no record. 🔴 Its two fields change for different reasons, and the popup names both: the **time** moves for every arrival that was stored, including one whose whole content the archive already held (the page re-sent a conversation it had already sent — ChatGPT does this on every view), because that still measures the page-to-archive path; the **count** rises only for an arrival that was **newly** stored, so four views of one conversation are not four stored conversations. Nothing else is in it: a platform id, a timestamp and a count. 🔴 A platform with **no record** is not a platform with zero captures: no writer creates a row out of nothing — a row exists only where a capture reached the archive — so the popup reads an absent record as "nothing has been recorded here", a gap in the record, and never as "no capture arrived". A `count` of `0` *inside* a row is not that absence and is not rounded up either: it says nothing new is on record there, beside a time that says a capture did arrive. | `apps/extension/lib/live-capture.ts:149-151`, `:273-308`; `apps/extension/entrypoints/background.ts:346-348`, `:367,443` |
| `cs_install_identity_v1` | This browser profile's own identity for the extension: a random UUID minted the first time this profile captures (from the browser's own crypto random, written down only after a write-and-read-back confirms it), the detected browser family — `Edge`, `Brave`, `Opera`, `Firefox`, `Vivaldi`, `Arc`, `Chrome`, or `Chromium`, a name read from the navigator and nothing else — and the profile label you typed in the popup, `null` until you name it and then trimmed and cut at 80 characters. No history is kept: writing the UUID is a one-time act, and a rename overwrites the label in place. 🔴 A profile whose storage or random source cannot be used **refuses to capture** rather than inventing a per-session identity, and a stored record this build cannot read is refused rather than replaced — both keep "which install produced this" answered by exactly one stable id, never by a guess. Its values leave this browser profile only inside a capture bundle, whose destinations are section 3a and 3c above and below. | `apps/extension/lib/install-identity.ts:1-5`, `:13-23`, `:39-69`, `:114-124`; `apps/extension/entrypoints/background.ts:244-254` |
| `cs_report_seq_v1` | This install's own **report counter**: the next number it will send, with the random value minted beside that number, written down before the message carrying it goes out. It exists so that two live copies of one install can be told apart — they start at the same counter position but not with the same future, so both eventually reserve one number and your host sees that number under two different random values, which a single writer cannot produce. 🔴 A number whose record cannot be read is **unknown, never `0`**: a fabricated zero is exactly what a copy's displaced counter would look like, and guessing one would accuse an honest install of being a copy, so the message goes out with no number at all and the host records "no evidence" rather than a reading. Nothing else is in it, and it is never compared against a count of anything you did. | `apps/extension/lib/report-seq.ts:1-36`, `:45`; `apps/extension/lib/native-host.ts:1075-1114` |


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
  (`apps/extension/lib/backfill/enumerate.ts:1488-1556`).
- For platforms that use response-body identity extraction, the `<scope>` part
  of that key is your **account identifier on that platform** when the extension
  could find one (a user id, an email address, or a handle), and the literal
  string `default` when it could not (`apps/extension/entrypoints/background.ts:2072-2120`;
  the identity itself is read by `apps/extension/lib/contract.ts:1524-1540`). It is used to

  keep two machines' archives of the same account from colliding. It stays in
  your local browser storage and is written into your own archive; it is not
  transmitted anywhere by this extension. For platforms that use this identity
  path, the backfill leg started by pressing the popup button records `default`
  when it cannot read an account identifier. For a platform whose requests are
  addressed by an account scope, the popup's own start now **asks the page** which
  one it is using (see the claude.ai bullet below) and records that; only when the
  answer cannot be obtained does the row keep `default`, together with the named
  reason it could not be obtained
  (`apps/extension/entrypoints/background.ts:1733-1793`).
- **ChatGPT is workspace-scoped.** The extension fingerprints the
  `ChatGPT-Account-Id` header observed on that page's outgoing requests with the
  per-install HMAC key before storing a target, ledger scope, or sending the scope
  to the native host. Fingerprinted scopes carry the explicit `chatgpt:fp1:` marker;
  unmarked ChatGPT scopes are treated as raw migration inputs regardless of their
  shape, and the host refuses an unmarked scope on both coordination and delivery. A
  one-time migration re-keys older raw workspace scopes and their pending debt rows
  before removing old storage keys. Coverage waits for that migration before reading
  the stored scopes. It does not infer the
  workspace from conversation content. Main, archived, project-discovery, and
  per-project conversation enumeration have separate resumable cursors in the
  workspace ledger. If the workspace is unresolved or ambiguous, the extension
  records that named refusal and issues no list request; it does not mark the
  unknown workspace empty (`apps/extension/lib/backfill/chatgpt-workspace.ts:1-23`, `:48-72`;
  `apps/extension/lib/backfill/chatgpt-scope-migration.ts:27-110`;
  `apps/extension/entrypoints/background.ts:2142-2154`;
  `apps/extension/lib/backfill/engine.ts:1938-1953`;
  `apps/extension/lib/backfill/types.ts:1762-1779`;
  `apps/extension/lib/backfill/enumerate.ts:314-384`;
  `apps/extension/lib/backfill/engine.ts:2597-2624`).
  🔴 W303 · The content bridge keeps only the current raw account header in one
  in-memory slot. A new observation replaces it, and `pagehide` or `unload` clears
  it. Each observation is forwarded immediately to the worker, where it is
  HMAC-fingerprinted with this install's key; only the branded identity returns to
  the content bridge for workspace ambiguity detection. At send time, every ChatGPT
  backfill request explicitly sets `ChatGPT-Account-Id` from that slot. The
  content-to-worker reply carries the exact header sent, which the worker fingerprints at the
  boundary before the engine compares the request-local identity with its lease. If
  the session-token read delays a request, the slot is sampled after that read; a
  retry samples it again. If the header changes while a 401/403 retry is waiting for
  fresh authorization, the retry is not sent and the first refusal, with its own
  header identity, is returned so the scope suspends. If the slot is empty, no
  request is sent and the scope is suspended. The raw value is
  transmitted only in the explicit ChatGPT request header and to the worker boundary;
  it is never written to storage, logs, exports, or host messages. Before the first
  list request, the run reads the page's current observation and establishes or refreshes the request-header
  lease from its fingerprint; an absent or ambiguous starting identity stops before
  enumeration. It checks every
  main, archived, project, and detail response against that identity. A proven mismatch suspends that
  workspace scope and preserves every pending id; IDs from the other response are not
  enqueued, and its bodies do not settle debts. Missing or ambiguous identity stops
  attribution with `refused-unknown`; HTTP 401/403 also suspends the scope. Both
  preserve pending debt. The workspace scope remains a
  separate field from the request-header account lease
  (`apps/extension/entrypoints/dw-bridge.content.ts:248-275`, `:616-660`;
  `apps/extension/entrypoints/background.ts:3234-3248`;
  `apps/extension/lib/backfill/tab-port.ts:226-245`;
  `apps/extension/lib/backfill/tab-port.ts:816-819`;
  `apps/extension/lib/backfill/tab-port.ts:876-904`;
  `apps/extension/lib/backfill/tab-port.ts:1077-1149`, `:1178-1236`;
  `apps/extension/lib/backfill/engine.ts:1590-1605`;
  `apps/extension/lib/backfill/engine.ts:1683-1712`;
  `apps/extension/lib/backfill/engine.ts:2197-2209`;
  `apps/extension/lib/backfill/engine.ts:2634-2645`;
  `apps/extension/lib/backfill/engine.ts:1714-1735`;
  `apps/extension/lib/backfill/engine.ts:2943-2946`;
  `apps/extension/lib/backfill/engine.ts:3001-3005`;
  `apps/extension/lib/backfill/engine.ts:3081-3085`;
  `apps/extension/lib/backfill/engine.ts:3204-3211`;
  `apps/extension/lib/backfill/chatgpt-workspace.ts:1-35`;
  `apps/extension/lib/backfill/chatgpt-workspace.ts:94-96`).
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
  (`apps/extension/lib/backfill/claude-org.ts:48-49`, `:219-236`;
  `apps/extension/lib/backfill/enumerate.ts:4059`;
  `apps/extension/lib/backfill/claude-page.ts:97-116`). The resolution itself runs
  **in the claude.ai page**, over the channel the backfill already uses, and only
  when the extension actually needs the organization: when you press the popup's
  start button for that platform, and on a wake-up whose recorded scope is not an
  organization yet. A page that is simply open and idle is asked nothing
  (`apps/extension/lib/backfill/claude-page.ts:71-158`;
  `apps/extension/lib/backfill/tab-port.ts:1333-1350`). A 403 or 429 from that
  request is reported to the machine-local host, which stores a platform cooldown
  shared by installs on that machine; if the report fails, this profile pauses
  backfill locally
  (`apps/extension/entrypoints/background.ts:756-768, :1711-1715, :2079-2088`;
  `crates/chat-stasher/src/nativehost.rs:1350-1357`). An account belonging
  to several organizations, with neither of the first two sources naming one, stops
  the leg instead of choosing: the value is never guessed and the organizations
  are never probed one by one, and once **this build** has recorded that answer
  the page is not asked again on every wake-up — the answer is already known
  (`apps/extension/lib/backfill/claude-org.ts:219-273`;
  `apps/extension/entrypoints/background.ts:1733-1793`). A record an **earlier**
  build left is re-asked once, and only once: a recorded judgement is that
  build's, not this one's, and a refusal that repeats is written back naming the
  build that saw it. 🔴 "Once" is enforced rather than intended: the attempt is
  recorded **before** the request goes out, so a write that does not land cannot
  turn it into a once-per-wake-up poll — the scope simply waits for the next
  build. The sentinel
  `default` — "the identifier could not be told" — is refused outright for this
  platform rather than written into a path segment where it would address an
  organization that does not exist (`apps/extension/lib/backfill/engine.ts:2015-2027`).
  🔴 **The organization a backfill is started with is the one it keeps.** The
  scope is written into the platform's progress record and the target registry
  when the backfill is registered, and no later request re-reads it from the page
  — so switching organizations on claude.ai, or having claude.ai open in two tabs
  at once, does not move a backfill that is already running: it keeps writing
  under the organization it started with
  (`apps/extension/entrypoints/background.ts:2379-2480`). A backfill for a *second*
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
  a request (`apps/extension/lib/backfill/claude-org.ts:112-138`;
  `apps/extension/entrypoints/background.ts:1348-1378`). Collapsing a title cannot
  put the unresolved sentinel in front of a live organization — the alarm would
  otherwise halt on `'default'` and never tick the organization
  (`apps/extension/lib/backfill/alarm.ts:1137-1239`;
  `apps/extension/entrypoints/background.ts:2379-2480`). The host archive is
  append-only and is not touched. The **local** ledger header at
  `cs_backfill_v2:<platform>:<scope>` is a different fact: a run opens it before
  any request, so a title tick has already written `halted` / `failures` /
  `enumCursor` there. Dropping the registry row also removes that header; the
  popup ignores a header whose scope is not a registered target, so an abandoned
  halt cannot outrank the organization's own ledger
  (`apps/extension/lib/backfill/alarm.ts:1125-1135, 1160-1205`;
  `apps/extension/lib/popup-view.ts:1963-2050`).
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
hold (`crates/chat-stasher/src/config.rs:116`). Content is encrypted
by `rustic` before it is written there, with a master key that is generated and
kept on your machine (`crates/chat-stasher/src/store.rs:318-418,1951-1953,1993-2003`; `crates/chat-stasher/src/main.rs:7930-7931`).
A directory written by `export --out` is **not** this: it is a separate,
unencrypted copy, and it is not created unless you run that command.

## 4. Who your data is shared with

**We share nothing with anyone, because we never receive anything.** There are
no third-party processors, no advertising partners, no analytics vendors, no
error-reporting service, and no data sales.

The parties who *do* see something, stated plainly:

| Party | What they see | Why |
|---|---|---|
| **The chat platform** (ChatGPT, DeepSeek, Perplexity, Gemini, Claude, Kimi, Grok) | Your conversations — they host them; they always could. Capture adds no traffic of its own, except on **ChatGPT**, where it requests the full conversation you just opened, and on **Gemini**, where it requests the conversation from its first page and follows the paging token to the end — one request for the first page plus one per remaining page, all on the same route the page itself calls (both same origin, your own session). | `apps/extension/lib/page-hook.ts:869`, `:671-718`; `apps/extension/lib/gemini-capture.ts:150-238` |
| **Your archive destination provider**, if you chose a remote one | Encrypted objects: their **sizes**, **timestamps**, and how many there are. Not the content. This is a real metadata leak: it reveals your archiving rhythm and volume. | `crates/chat-stasher/src/store.rs:318-418`; see `docs-dev/threat-model.md` |
| **Your browser vendor**, possibly | The download-history entry for an export file, *if* you pressed the popup's export button *and* your browser syncs download history to your browser account. **We have not investigated** whether any particular browser does this by default. | `apps/extension/lib/outbox.ts:588-621` |
| **Anything else running on your computer as you** | The plaintext bundles in the extension's outbox, the staged shards, the config, and the master key files. We do not defend against this. | See [Known weaknesses](#known-weaknesses) |
| **Us, the authors** | Nothing. | Section 1 |

One further disclosure about the optional **backfill** feature, which walks your
conversation history to archive older chats. When you turn it on, it issues
additional requests to the chat platform, from your own logged-in session
(`apps/extension/lib/backfill/engine.ts:2903-2946`). That produces a request pattern
the platform can see and which does not look like a human reading their history.
**We have not investigated** whether any platform's terms of service prohibit
this, or whether it triggers rate-limiting. Backfill is off unless you enable it
(`apps/extension/lib/backfill/schedule.ts:40`), and with no HTTP port wired the
code refuses to fetch at all rather than defaulting to a live one
(`apps/extension/lib/backfill/engine.ts:161-164`).

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
and one body request per conversation** — since W84, 2026-09-23, the body segment
is filled in from a logged-in probe that observed a completeness signal, and a
response that declares more of a conversation is refused rather than archived in
part; passive capture is a different leg and sends no request of its own, it
reads the response the page fetches for the conversation you have open). On **Claude** it is a conversation list, one body request per
conversation, and — only when neither the page's own requests nor the cookie has
named the organization — **one** `GET /api/organizations`.
(`apps/extension/lib/backfill/enumerate.ts:4907-4938`;
`apps/extension/lib/backfill/engine.ts:3042-3115`.) The
practical reading for you: enabling backfill on Perplexity now produces list
traffic *and* one body request per conversation the platform can see, and the
conversations are backed up only as completely as the body endpoint returns — a
response that declares it holds only part of a conversation is refused and lists
a failure, never stored in part. On
DeepSeek it produces list traffic *and* one body request per conversation, and on
Grok two (a skeleton call and a content call — a request pattern the platform is
more likely to notice, although both are the same two calls grok.com's own page
makes when you open a conversation, separated by a 2-5 second pause,
`apps/extension/lib/backfill/enumerate.ts:3360-3371`), and on Kimi one body request —
the same call its own page makes when you open a conversation, carrying the page's
own token as described above (`apps/extension/lib/backfill/enumerate.ts:3375-3437`).
On Claude it is a conversation list and one body request per conversation — the
same call claude.ai's own page makes when you open a past conversation — plus, at
most once for as long as the organization stays unresolved, the organization-list
request described above
(`apps/extension/lib/backfill/enumerate.ts:4492-4501`).
On Gemini, one conversation costs as many requests as it has pages: the leg
follows the continuation token until the response says there is no more, waiting
1-3 seconds between pages, and it refuses (and lists as a failure) a conversation
that would need more than 20
(`apps/extension/lib/backfill/enumerate.ts:3870-4028`; `apps/extension/lib/backfill/engine.ts:3042-3115`).
Its **passive capture** sends requests too, on the same route: a conversation you
open is fetched from its first page and followed to the end, one request for the
first page plus one per remaining page (`apps/extension/lib/gemini-capture.ts:150-238`).
The first of those repeats the request the page had just made, on purpose: the
response the page produced may be any page of the conversation (it asks for older
turns as you scroll), and a copy that started anywhere else could hold only the
oldest turns while looking complete.

🔴 **Nothing coordinates two machines, and that is a property of the design
rather than a gap still being filled.** The pacing that keeps this leg short of a
scraper is enforced by your own host and is machine-local: it arbitrates one
per-platform budget keyed by machine, platform and the masterkey-derived account
key, so it only ever sees the installs on the machine it runs on
(`crates/chat-stasher/src/nativehost.rs:1591`, `:1609`). There is no server, so
if you enable backfill for the same account on two machines, each machine's waits
and cooldowns apply only to its own requests, and the platform sees the two
patterns added together. **We do not warn you about that today.** The one
coordination sentence the extension does show is about *this* machine: when the
local coordination channel is unavailable — an older host, or a host it cannot
reach — the leg refuses to run at all, live capture carries on, and the popup
says exactly that, "Update chat-stasher to enable backfill. Live capture remains
active in this browser." (`apps/extension/entrypoints/background.ts:2054`;
`apps/extension/locales/en.yml:488-489`).

## 5. Where the extension runs

The extension's content scripts are injected on an **explicit, closed list of
origins** compiled into the code — never `<all_urls>`, never a wildcard:

- `https://chat.deepseek.com` (`apps/extension/lib/contract.ts:359`)
- `https://www.perplexity.ai` (`apps/extension/lib/contract.ts:419`)
- `https://chatgpt.com`, `https://chat.openai.com` (`apps/extension/lib/contract.ts:490`)
- `https://gemini.google.com` (`apps/extension/lib/contract.ts:514`)
- `https://claude.ai` (`apps/extension/lib/contract.ts:563`)
- `https://www.kimi.com` (`apps/extension/lib/contract.ts:637`)
- `https://grok.com` (`apps/extension/lib/contract.ts:762`)

The list the browser is given is derived mechanically from that table
(`apps/extension/lib/contract.ts:859-894`), so the sites the extension can run
on and the sites it can capture from are the same set by construction — they
cannot drift apart.

**You can verify this yourself without reading the code:** your browser shows
the extension's site access in `chrome://extensions` / `about:addons`, and it
will name these sites and no others. On every other website you visit, this
extension is not running.

**And it runs only in the profiles where you installed it.** This is a list of
where the extension *may* run, not a list of where it is running: an extension
belongs to one browser profile, so a browser profile you never loaded it in has
no capture at all, no backfill and no outbox. That is also why one install's
storage is not another's; see
[install.md → Install the browser extension](install.md#3-install-the-browser-extension)
for what one install per profile means in full.

**The extension itself cannot read the browser's profile list, and the dashboard
does.** An extension knows which browser it is in but not which profile, let
alone the name you gave that profile — so the "Open in …" action on the local
dashboard, which has to turn a profile *name* into the directory the browser was
launched with, reads that mapping from the two files the browser maintains
itself: the browser's **Local State**, for the human-readable name of each
profile, and each profile's **Preferences**, only to test whether this extension
is installed in that profile
(`crates/chat-stasher/src/ui/extension_profile.rs:146-177`). Three things out of
those files are used — the profile's name, its directory name, and whether this
extension is installed in it — and only to build one launch command: nothing
read from either file is written back, sent to the host, or archived, and a name
matching more than one profile, or a profile the extension is not installed in,
resolves to nothing, so the row says no exact match was found instead of
guessing.

Within those sites, not every request is captured. A response is only kept if it
matches the platform's expected route *and* method *and* status *and* body shape
(`apps/extension/lib/contract.ts:1073-1094`, `:1085-1129`). A body over 16 MiB is not
captured, and the page console says so rather than dropping it silently
(`apps/extension/lib/contract.ts:915`; `apps/extension/lib/page-hook.ts:481`). No shipped
platform row reads `WebSocket` frames or `EventSource` messages: every row
states `webSocketCapture: false` explicitly, and none declares
`eventSourceCapture` (`apps/extension/lib/contract.ts:356-852`).

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
| **ChatGPT** | Main conversations, archived conversations, project discovery, and each project's conversations use separate cursors inside a workspace-scoped ledger; each page is requested through the current tab (`apps/extension/lib/backfill/types.ts:1762-1779`; `apps/extension/lib/backfill/enumerate.ts:314-384`; `apps/extension/lib/backfill/engine.ts:2596-2623`). | The conversation text, fetched one conversation at a time. Implemented, **not yet observed completing a backfill in a real browser**. Workspace attribution comes from the page's outgoing request header; if the workspace is unknown or ambiguous, enumeration stops with a named refusal and no list request (`apps/extension/entrypoints/background.ts:2142-2154`; `apps/extension/lib/backfill/engine.ts:1937-1952`). |
| **DeepSeek**, **Gemini**, **Grok**, **Kimi**, **Claude** | Lists your conversations **and fetches their content**, one conversation at a time, handing it to the host (`apps/extension/lib/backfill/enumerate.ts:4959-4983`). All five are **implemented, not yet observed completing a backfill in a real browser**. For **DeepSeek** we have **not verified** whether a long conversation comes back complete either, and it does not page the endpoint (`apps/extension/lib/backfill/enumerate.ts:2980-3007`) — but the response is a tree, the extension walks it from its newest message back to a root, and a walk that reaches a message the response does not carry means that conversation is **not archived**; it is recorded as a failure with its own reason code and the leg carries on. For **Grok and Kimi** the same question is unverified with no such check. **Gemini does page**, so for it the completeness question is answered by following the token to the end; what bounds it instead is the 20-page cap, past which the conversation is refused rather than archived in part (`apps/extension/lib/backfill/engine.ts:3042-3115`). Kimi's routes, by contrast, **were** measured in a logged-in session (2026-09-14) and its one body request carries your page's own login token (`apps/extension/lib/platform-auth.ts:313-350`); whether a **long** Kimi conversation comes back complete is **not verified**, and a response that says it holds only part of a conversation is recorded as a failure rather than archived as a whole one (`apps/extension/lib/backfill/engine.ts:3250-3284`). Grok is the least verified: its routes were read from public open-source implementations rather than measured in a logged-in session, and **each conversation costs two requests** — a skeleton call, then a content call built only from the ids that skeleton named (`apps/extension/lib/backfill/enumerate.ts:3338-3399`). |
| **Perplexity** | Lists your conversations **and fetches their content**, one `GET /rest/thread/<slug>` per conversation (`apps/extension/lib/backfill/enumerate.ts:3111-3112`). Each conversation is **checked for completeness before it is stored**: the 2026-09-23 probe found the body carries a stated `has_next_page` / `next_cursor` signal, so a response that declares there is more of the conversation is **not archived** — it is recorded as a failure with its own reason code and the leg carries on, never storing a truncated conversation as a whole one (`apps/extension/lib/backfill/enumerate.ts:2432-2476`). Implemented, **not yet observed completing a backfill in a real browser**. |
| **Claude** | Lists your conversations **and fetches their content**, addressed by the organization resolved as above. Its routes were read from public open-source implementations, **not** measured in a logged-in claude.ai session — nobody has opened claude.ai with this code — so the route shapes are source-backed rather than observed (`apps/extension/lib/backfill/enumerate.ts:4031-4037`). **Each conversation's body is checked for completeness before it is stored**: the response is a tree, and the extension walks the active branch from its newest message back to the branch root. A parent the response does not carry (the shared tree-root id every real body omits, measured 2026-09-24) is accepted as that root only when the body's own shape corroborates it — one shared absent parent, and a root at the foot of the message `index` counter; a body with a missing middle or a dropped prefix, a body whose newest message is absent, or one whose parent links form a cycle is **not archived** — it is recorded as a failure with its own reason code and the leg carries on (`apps/extension/lib/backfill/enumerate.ts:4278-4356`; `apps/extension/lib/backfill/engine.ts:3327-3347`). Whether a **long** conversation is capped server-side is not established by any source; a body so capped is refused unless it also rewrote the survivor to index 0 and the shared id, which no measurement shows. |

We state this in a privacy policy because the failure mode is a privacy
expectation, not just a feature gap: a user who believes their Perplexity
history is archived may delete it upstream. Backfill now does archive it — but
only as completely as the body endpoint returns, and a body that declares it
holds only part of a conversation is refused rather than stored in part, so a
long conversation you delete upstream is absent from your archive rather than
half-there. The same caution applies to every platform in the table above:
what backfill stores is only as complete as the body endpoint returns, and we
have not checked a genuinely long conversation against any of them. Kimi is the
one where that case is at least refused out loud: a body response that says it
holds only part of a conversation is not archived and is listed as a failure,
so a long Kimi conversation is missing from your archive rather than silently
half-there (`apps/extension/lib/backfill/engine.ts:3251-3285`). Grok
carries a second caveat of its own: where the sources for its list cursor
disagree, the extension does **not** pick one — a page that repeats what was
already listed stops the leg and says the response shape changed, rather than
being read as "you have no more conversations"
(`apps/extension/lib/backfill/engine.ts:2267-2385`). And because no source says
whether Grok returns a conversation's responses in a stable order, re-opening an
unchanged Grok conversation is more likely than on other platforms to deliver
another copy of it: an extra copy in your archive, never a lost one.

Responses are read from `fetch` and from `XMLHttpRequest`, and both go through
the same capture decision above (`apps/extension/lib/page-hook.ts:448-520`).
An XHR body is read only when the page itself reads it as text or JSON
(`apps/extension/lib/page-hook.ts:629-634`); a binary XHR body (arraybuffer,
blob, document) is never read and only prints a console warning (`:635-638`,
`:258-265`). Stream transports are opt-in too: `EventSource` messages and
`WebSocket` text frames are read only where the platform row opts in
(`eventSourceCapture`, `webSocketCapture`), and no shipped row opts into
either, so a stream that only looks like a candidate route just gets the
console warning (`apps/extension/lib/page-hook.ts:680-684`, `:768-771`).

## 6. What each permission is for

The extension declares exactly four permissions and no host permissions
(`apps/extension/wxt.config.ts:125`):

| Permission | Why it is needed | What it does **not** allow |
|---|---|---|
| `nativeMessaging` | This is the delivery channel. A captured conversation is handed to the `chat-stasher` binary already on your machine, which you registered per-user with `chat-stasher install-native-host --stage <path>`; the host manifest names exactly one allowed extension id, and the host refuses to serve any other origin. The registration is per user account, not per install: one host manifest per browser, shared by every profile of it, all pointing at the same binary and the same stage (`crates/chat-stasher/src/nativehost.rs:155-168`, `:647-691`, `:3927-3961`, `:499-612`; `crates/chat-stasher/src/main.rs:2163-2178`) | It cannot reach any program other than the one host manifest you registered, and that host is the `chat-stasher` binary you installed yourself. There is no fallback channel: without a registered host, captures wait in the outbox instead. |
| `storage` | Persists the items listed in [section 3b](#3-where-your-data-is-stored) — the backfill switch and progress header (so an interrupted backfill can resume instead of restarting; the id list itself is in the `chat-stasher-backfill` IndexedDB database), the last host-status answer, the pause record, and the last-export stamp. (`apps/extension/lib/backfill/store.ts:18-27`) | This is `storage.local` only: `localArea()` reads `browser?.storage?.local` / `chrome?.storage?.local` and nothing else (`apps/extension/lib/backfill/store.ts:85-97`). Nothing is written to `storage.sync`, so nothing here is uploaded to your browser account by us. |
| `alarms` | Gives the backfill leg a periodic heartbeat, so history archiving can finish over days without you having to keep the specific chat tab open — the leg does need *some* open, logged-in page of that platform to fetch through, and the install guide states that precondition in full; since the Native Messaging rewrite the same alarm is also when the outbox is drained and retried. (`apps/extension/wxt.config.ts:103-107`; `apps/extension/lib/backfill/alarm.ts`; `apps/extension/lib/outbox-alarm.ts:20-46`) | It does not grant any network or data access. |
| `unlimitedStorage` | The outbox is an IndexedDB queue of undelivered bundles, capped at 256 MiB by us (`apps/extension/lib/outbox.ts:54`); the backfill id list (`chat-stasher-backfill`, ids only, no conversation text) is a second IndexedDB database. Without this permission Chrome may evict best-effort IndexedDB data under disk pressure, which would mean silently losing captures the user was told were queued. (`apps/extension/wxt.config.ts:115`) | It removes the browser's eviction path for data the extension already stores. It is not a claim on your disk beyond that, and the outbox refuses new captures rather than growing without bound. |

**No permission here shows an install-time warning.** `downloads` — which did
show "Manage your downloads" — is no longer requested at all
(`apps/extension/wxt.config.ts:89`), and none of these four raises one
(`apps/extension/lib/backfill/alarm.ts:16-18`,
`apps/extension/wxt.config.ts:125`). The one thing Chrome does tell you at
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
would let it reach a collection endpoint (`apps/extension/wxt.config.ts:125`).
A network capture on the extension's background page is the check that does not
require trusting us at all.

We do not respond to Do-Not-Track signals, for the simple reason that we operate
no service that could receive one.

## 8. We are not an AI service

Chat Stasher does not call any AI model, does not send your conversations to a
model provider, and does not use your conversations for training anything. The
word "chat" in this product refers to conversations you already had, on someone
else's service, that this tool copies into your own archive. The archive format
is `rustic` encrypted backup objects (`crates/chat-stasher/src/store.rs:318-418`);
nothing reads them except you.

## 9. How long data is kept, and how to delete it

**We keep your data for zero seconds, because we never hold it.** There is no
account to close and no deletion request to file with us — there is nothing on
our side to delete.

Retention on **your** machine is under your control:

| Where | How long it stays | How to delete it |
|---|---|---|
| Bundles in the extension's outbox | Until the host answers a matching `ack`, which deletes the record (`apps/extension/lib/outbox.ts:440-455`). A record the host **refused** outright is kept and never retried. **If the host is never reachable, they stay indefinitely, in plaintext.** One outbox per install: it holds only what that profile's copy captured, and no other install can read or drain it. | Uninstall the extension **in that profile**, or clear its site data in your browser; there is no per-record delete button. Either one deletes that install's queue and leaves every other profile's alone. |
| An export file you triggered | Until `ingest` consumes it, which moves it to `<inbox>/consumed/` once every line was sealed or found to be a duplicate (`crates/chat-stasher/src/inbox.rs:57-60`). | Delete it from your download directory with your file manager. |
| Browser download-history entry for that export | Until you clear your browser history | Clear downloads in your browser's own history UI |
| Extension local storage — the install identity, the report counter, the account-fingerprint salt, backfill progress, the alarm's last-wake trace, the last host status, the pause record, the capture-hook records and the last-export stamp (the full list is the table in [section 3b](#3-where-your-data-is-stored)) | Until you clear it or uninstall the extension | Uninstalling the extension removes it; browsers also expose per-extension site-data clearing |
| Staged shards | Until `push` moves them into the repository | Delete the stage directory you chose |
| A directory you exported to | **Until you delete it.** `export --out` writes the selected sessions there decrypted, and nothing — not `push`, not `ingest` — moves them on (`crates/chat-stasher/src/main.rs:686-772`). | Delete the directory you named. `--out` must be empty or absent unless `--force` is given, and the command deletes nothing, so nothing of yours is lost by pointing it at a directory you later remove. |
| The optional full-text index | Until you run `chat-stasher index clear` or remove the OS cache directory. It stores indexed titles and user/assistant text in a local SQLite database. | Run `chat-stasher index clear --destination <name>` or use the explicit `--repo` used to select the index. |
| Your archive repository | **Indefinitely, by design.** This is a backup tool: it exists so that history a platform deleted still survives. Grok CLI usage sidecars are retained as a separate shard linked by session id; the original `usage.json` bytes are kept intact, including each model's `modelUsage` object and all counters such as `inputTokens`, `cachedReadTokens`, `outputTokens`, `totalTokens`, and any additional fields the source contains (`crates/chat-stasher/src/scanner.rs:1742-1858`; `crates/chat-stasher/src/collect.rs:2050-2090`). | Delete the repository directory or remote bucket yourself. **There is no `delete` subcommand and no command that restores sessions into a harness's own directories in this version** — the subcommand list now includes `index` and has no restore command (`crates/chat-stasher/src/main.rs:168-1319`). Selective per-conversation deletion inside an archive is not implemented. |
**Uninstalling the extension in one profile stops capture in that profile
immediately** and removes that profile's local storage, which is where its outbox
lives, so uninstalling also deletes the captures *that install* had not been
acknowledged yet. Other profiles and other browsers keep capturing and
delivering; their outboxes are their own. Nor does it delete the staged shards or
your archive: those are yours, and deleting your backup without being asked would
be the worse failure.

**The host registration is the larger removal of the two, and it is not how one
profile is retired.** `chat-stasher install-native-host --uninstall` removes the
host manifest for every browser on the machine in one pass, so an install you
left in place can no longer deliver and its captures wait in its outbox instead
(`crates/chat-stasher/src/main.rs:2214-2252`, `:2321-2327`). Draining each
profile's outbox first is the subject of
[install.md → Before you remove the host](../docs/install.md#before-you-remove-the-host),
and it is worth doing because an outbox goes away with its profile.

## Known weaknesses

A privacy policy that lists no weaknesses is more dangerous than no policy at
all, so here are the ones that bear on your privacy. The full list is in
[`docs-dev/threat-model.md`](threat-model.md).

**1. The plaintext window before delivery.** The extension writes each captured
session as an ordinary, unencrypted record into its outbox database, inside your
browser profile (`apps/extension/lib/outbox.ts:102-119`, `:365-437`). That record
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

**3. A master key file is the only key to the repository it opens, and losing it
is unrecoverable.** There is
no escrow, no recovery code, no maintainer-held copy, and no password reset — by
design, because any of those would mean someone other than you could open your
archive (`crates/chat-stasher/src/store.rs:1951-1953`). The key file
is written owner-only (`0600`) on Unix; on platforms without Unix modes it
inherits whatever the filesystem gives it
(`crates/chat-stasher/src/store.rs:1993-2063`).

There is one key file per repository — `rustic_key_file` for the local archive,
`key_file` per destination, defaulting to
`~/.local/share/chat-stasher/masterkey-<destination>.json` — so losing one loses
that copy alone, and a copy of one does not restore another. A second machine
reads a destination with that destination's key and does not use the local one,
which is why every key file has to be backed up
(`crates/chat-stasher/src/main.rs:7822-7827`).

**4. What other browser extensions can observe is unresolved.** We did not test
whether a second, hostile extension with broad host permissions on a chat origin
can observe our in-page hook or the `window.postMessage` traffic between our
page hook and our bridge, and we did not test whether an extension can reach
another extension's IndexedDB. Treat this as **potentially exposed, not safe**.
See the extension-ecosystem row of [`docs-dev/threat-model.md`](threat-model.md).

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

The package metadata identifies the Apache License 2.0
(`crates/chat-stasher/Cargo.toml:6`; [license text](../LICENSE)), which includes
its warranty terms.

## 13. Changes to this policy

If this policy changes, the "Last updated" date at the top changes with it, and
the change is visible in this repository's commit history. If a future version
of the software ever collects anything, this document will say so **before** that
version ships, and we would expect you to hold us to that.

## 14. Contact

**Email: `work@team.iopho.com`** — the same address as security reports
([security policy](../SECURITY.md)).

For a suspected vulnerability, please read [`SECURITY.md`](../SECURITY.md)
first: mail the address above rather than opening a public issue, and please do
**not** include your own conversation content, repository paths, hostnames, or
key material in the report.

This is a personal project with a single maintainer. There is no response-time
commitment ([security policy](../SECURITY.md)).

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
