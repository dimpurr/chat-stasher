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

Line numbers were checked against commit `143cb79`. They drift as the code
changes; if a citation no longer lands where this document says it does, trust
the code and treat the sentence as unverified.

## How the data moves

Understanding the roles below requires knowing the path the content takes.

1. A browser extension hooks `fetch` on a fixed list of chat origins and keeps
   the raw response text (`apps/extension/lib/contract.ts:53-120`,
   `apps/extension/lib/page-hook.ts:217`, `:241`, `:282`).
2. The extension writes that text, as a JSON bundle, to a file **in your
   browser's download directory**, under `chat-stasher/inbox/`
   (`apps/extension/lib/contract.ts:139`; `apps/extension/lib/download.ts:91-93`).
   The delivery channel is `chrome.downloads`
   (`apps/extension/lib/download.ts:28-32`).
3. The Rust CLI reads those files (`ingest`) and/or reads local coding-harness
   session stores (`collect`, `status`), and produces *sealed shards* in a stage
   directory (`crates/chat-stasher/src/main.rs:471-529`).
4. `push` writes the stage into a rustic repository — encrypted — at a
   destination you configure, local or remote
   (`crates/chat-stasher/src/main.rs:146-183`).

Steps 1–3 are plaintext on your own machine. Step 4 is the only encrypted
boundary, and it is also the only step that can involve a network.

## Who can see what

### Us — the project authors

| | |
|---|---|
| **Can see** | Nothing. |
| **Cannot see** | Your conversation content, your session ids, your account identity, your destination address, whether you run this at all. |
| **Evidence** | The repository contains no project-operated endpoint. The CLI's only network capability is the rustic/opendal backend you configure yourself (`crates/chat-stasher/Cargo.toml:20-21`; `crates/chat-stasher/src/config.rs:95-100,146-162`). The extension's only outbound HTTP port defaults to a function that refuses to send (`apps/extension/lib/backfill/engine.ts:39-41`, `:119`), and when wired it is restricted to an origin that must already be in the platform table (`apps/extension/lib/backfill/engine.ts:186-188`). The extension declares no host permissions and no telemetry endpoint (`apps/extension/wxt.config.ts:22`). |

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
| **Evidence** | Content is written through `rustic_core` into a repository whose master key never leaves your machine (`crates/chat-stasher/src/store.rs:261-296,977-1062`). The backend is `rustic_backend` with the opendal feature and the options you supply (`crates/chat-stasher/Cargo.toml:20-21`; `crates/chat-stasher/src/config.rs:146-162`). SSH connection handling: `crates/chat-stasher/src/reap.rs:1-12`. |

**This is a real metadata leak and we are stating it plainly.** A destination
provider learns your **backup rhythm and volume**: how often you archive, how
much you produced each time, and therefore roughly when you were and were not
having conversations. If you archive on a schedule
(`crates/chat-stasher/src/main.rs:86-145`), the schedule itself is visible to
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
| **Can see** | The captured conversations **in plaintext**, in your browser's download directory; the master key that opens your entire archive; the staged shards before they are pushed; your config, including your destination address. |
| **Cannot see** | Nothing meaningful is withheld from a process running as your user. |

Concretely, four separate plaintext exposures:

1. **The download-directory window.** The extension writes each captured session
   as an ordinary, unencrypted JSON file into your download directory at
   `chat-stasher/inbox/<platform>-<sessionId>.json`
   (`apps/extension/lib/contract.ts:139`;
   `apps/extension/lib/download.ts:91-93`;
   `apps/extension/entrypoints/background.ts:84-86`). The file contains the raw
   response body — the conversation itself
   (`apps/extension/lib/contract.ts:282-291`). It sits there, world-readable to
   anything running as you, until the CLI consumes it. **We do not encrypt it,
   we do not restrict its permissions, and we do not shorten that window.** How
   long the window is depends entirely on how often you run `ingest`; if you
   never run it, the plaintext stays indefinitely.

2. **The master key file.** It is written as plaintext JSON. On Unix it is
   created `0600` — the mode is set when the file is created, not afterwards —
   inside a parent directory tightened to `0700`
   (`crates/chat-stasher/src/store.rs:1144-1229`); on platforms without Unix
   modes it inherits whatever the filesystem gives it. That keeps it away from
   *other* users, not from you: any process running as you can read it and,
   combined with access to your destination, decrypt the entire archive.

3. **The stage directory.** Sealed shards are ordinary files on disk before
   `push` encrypts them into the repository
   (`crates/chat-stasher/src/main.rs:146-150`).

4. **Browser download history.** The two-phase write erases only the `.part`
   entry from the download shelf; the final file's entry is not erased
   (`apps/extension/lib/download.ts:129-141`). Your browser therefore retains a
   history record whose filename embeds the platform name and the session id
   (`apps/extension/entrypoints/background.ts:116-117`). That is metadata, not
   content, but it is metadata about which conversations you archived, and it
   may be synced by your browser to your browser vendor. **We have not
   investigated** whether any particular browser syncs download history by
   default.

**We do not defend against a hostile process running as your user.** On a
single-user desktop this is the normal situation and the exposure is
proportionate; on a shared machine, or a machine running untrusted software as
your user, it is the dominant risk in this document.

### Someone with physical access to your machine, or your stolen disk

| | |
|---|---|
| **Can see** | Everything the previous row lists, if the disk is not encrypted or is unlocked: the plaintext inbox files, the key file, the stage, the config. With the key file *and* the repository, they can read the entire archive. |
| **Cannot see** | The repository contents alone, *without* the key file — a stolen remote-destination copy is encrypted (`crates/chat-stasher/src/store.rs:261-296`). |
| **Evidence** | No at-rest protection is implemented by this project beyond the rustic repository itself; see the key-file citations above. |

The practical consequence: **full-disk encryption is doing the work here, not
this tool.** If your laptop disk is unencrypted, the archive's encryption buys
you protection against the *storage provider*, not against the person holding
your laptop, because the key sits next to the config.

We have not implemented, and do not currently plan for phase one: OS keychain
storage for the key, or passphrase-wrapping of the key file.

### The chat platforms (ChatGPT, DeepSeek, Perplexity, Gemini, Claude, Kimi)

| | |
|---|---|
| **Can see** | Your conversations — they always could; they host them. Additionally, the extension's capture is indistinguishable from your own browsing, because it reads responses to requests **made in your already-logged-in session**. |
| **Cannot see** | That the capture happened, as far as we know — but see the caveat below. |
| **Evidence** | The hook wraps `fetch` in the page's own world and reads a clone of responses the page already requested (`apps/extension/lib/page-hook.ts:217`, `:241`, `:265-282`; `apps/extension/entrypoints/dw-fetch-main.content.ts:13-15`). Backfill, when enabled, issues additional requests to the same origin (`apps/extension/lib/backfill/engine.ts:199-232`, `:260`). |

**Caveat, stated honestly:** the passive hook adds no traffic, so there is
nothing distinctive for the platform to observe from it. **Backfill is
different** — it walks conversation lists and detail endpoints
(`apps/extension/lib/backfill/engine.ts:199-232`, `:260`), which produces a
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
| **ChatGPT** | Conversation-list requests **and** one request per conversation | The conversation text (`apps/extension/lib/backfill/enumerate.ts:762`) |
| **DeepSeek**, **Perplexity** | Conversation-list requests **only** | **Nothing.** Not one conversation body is requested or written (`apps/extension/lib/backfill/enumerate.ts:774`, `:538-548`, `:603-611`) |
| **Gemini**, **Claude**, **Kimi** | None; the leg halts before the first request | Nothing (`apps/extension/lib/backfill/enumerate.ts:643`) |

🔴 The middle row is the dangerous one to misread. On DeepSeek and Perplexity
the extension *does* work — it enumerates your conversations and reports a
pending count — while archiving **zero** of them. If you rely on this tool as
the reason it is safe to delete history upstream, that reasoning does not hold
for those two platforms. The code declines to guess a conversation-content
endpoint precisely because a wrong guess fails silently: it would archive a
truncated version of every chat and still look like success.

Note also that the extension attempts to extract an account identity (user id,
email, or handle) from response bodies in order to deduplicate across machines
(`apps/extension/lib/contract.ts:365-369`, `:418-433`). That value is written
into the inbox bundle and therefore into your archive. It never leaves your
machine, but it means your archive contains your account identifier.

### The browser extension ecosystem — other extensions installed alongside ours

| | |
|---|---|
| **Can see** | **Not investigated.** |
| **Cannot see** | **Not investigated.** |

We did not test what a second, hostile extension can observe. The specific
questions we did **not** answer, and which a reader should not assume are safe:

- Whether an extension holding the `downloads` permission can enumerate or read
  files that *our* extension downloaded. (Our own code notes only that an
  extension may `removeFile` on downloads *it* initiated —
  `apps/extension/lib/download.ts:44-54`, `:57-61` — which is a statement about
  our deletions, not about another extension's reads.)
- Whether an extension with broad host permissions on a chat origin can observe
  our MAIN-world hook, the `window.postMessage` traffic between the page hook
  and the bridge (`apps/extension/lib/contract.ts:6-15`), or the page-world
  marker we set (`apps/extension/lib/contract.ts:12-15`).
- Whether download history is readable by other extensions.

The message contract does carry a token check on ready/verify messages
(`apps/extension/lib/contract.ts:229-252`), and payloads are shape-validated
before reaching extension APIs (`apps/extension/lib/contract.ts:201-227`). Those
are input-validation measures against a malicious *page*; **we have not
established** that they constitute a defence against a malicious *extension*,
and we do not claim they do.

Since the page-world hook communicates over `window.postMessage`, the
conservative assumption is that content in transit is observable to anything
else with script access to that page. Treat this row as **unresolved and
potentially exposed**, not as safe.

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
  (`crates/chat-stasher/src/sqlite_probe.rs:1364-1367`). The module states the
  intent that a read-only probe never creates or touches `-wal`/`-shm` sidecars
  (`crates/chat-stasher/src/sqlite_probe.rs:23-29`), and there is a test
  asserting no sidecars are created (`crates/chat-stasher/src/sqlite_probe.rs:2001-2047`).
  `status` and `doctor` are likewise declared read-only
  (`crates/chat-stasher/src/main.rs:184-185,267-268`).
- **`seal` refuses to rename files it cannot justify renaming.** It is gated by
  the registry's `seal_policy`, an evidence line, and a platform-confidence
  cell; a harness that holds an open file descriptor (Codex) is refused with
  the active file untouched, because renaming it would strand later writes in
  the old inode (`crates/chat-stasher/src/main.rs:530-562`).

## Integrity: unknown is never treated as empty

This project's archive may become the only surviving copy of a conversation,
because platforms delete history and suspend accounts. That makes one class of
bug more dangerous than an information leak: **silently recording "nothing was
there" when the truth is "we could not tell".**

Two enforcement points exist in the code:

- **`push` refuses an unprovable empty snapshot.** If the stage holds no sealed
  shards, `push` audits consumed inbox files against the stage and the
  repository; it succeeds only when stage, scanner, collector and audit all
  agree, and otherwise exits non-zero with an explicit refusal rather than
  writing an empty snapshot
  (`crates/chat-stasher/src/main.rs:4118-4210`). It also fails closed when it
  cannot even establish stage safety
  (`crates/chat-stasher/src/main.rs:4101-4108`).
- **A destination that cannot be consulted is not an empty destination.**
  `dest-init` classifies each source destination into three states, not two:
  `Consulted`, `KnownEmpty` (nothing there *and* no local record of ever having
  collected for it — a fact), `SuspectedLoss` (we have a record and it cannot be
  read — possible data loss, reported loudly), and `Unknown` (no record and we
  cannot determine what is there)
  (`crates/chat-stasher/src/destinit.rs:104-118`, `:387-395`). The rationale is
  that "no repository at that location" has two opposite causes and the
  filesystem cannot distinguish them
  (`crates/chat-stasher/src/destinit.rs:57-72`). The user-facing text says so in
  as many words (`crates/chat-stasher/src/main.rs:2855-2911`).

This is an integrity property, not a confidentiality one. It does not protect
your data from anyone; it protects you from believing you have a backup you do
not have.

## Known weaknesses and things we have not done

This section is the reason to trust the rest of the document. Everything here is
a real limitation of the current code.

### Confirmed weaknesses

1. **Plaintext window in the download directory.** Described in full above.
   Captured conversations sit unencrypted, with default permissions, in your
   browser's download directory until you run `ingest`
   (`apps/extension/lib/download.ts:91-93`). **We do not currently defend this.**
   Mitigation available to you today: run `ingest` promptly, and put your
   download directory on an encrypted volume.

2. **The master key file is plaintext on disk.** It is not passphrase-wrapped
   and not kept in an OS keychain. On Unix it is created `0600` in a `0700`
   parent (`crates/chat-stasher/src/store.rs:1144-1229`), which keeps it from
   other users but not from anything running as you; on platforms without Unix
   modes it inherits the filesystem's defaults.

3. **Lose the key file and the data is gone. We have no recovery mechanism of
   any kind.** The master key is the repository's only key
   (`crates/chat-stasher/src/store.rs:1102-1104`); losing it makes the repository
   unreadable, and `load_key_file` can only report the loss
   (`crates/chat-stasher/src/store.rs:1231-1235`). There is no escrow, no
   recovery code, no maintainer-held copy, and no password-reset path — by
   design, because any of those would mean someone other than you could open
   your archive. **Back up the key file separately from the repository, or your
   archive is a very reliable way to lose your conversations.**

4. **There is no restore command.** The subcommands in this version are `init`,
   `run-once`, `schedule`, `push`, `status`, `read`, `doctor`, `verify`,
   `dest-init`, `search`, `view`, `ingest`, `collect`, `seal`
   (`crates/chat-stasher/src/main.rs:44-821`); **a bulk restore-to-disk command
   does not exist**. The only retrieval path is `read`, which dumps **one
   session at a time** to stdout and prints its SHA-256
   (`crates/chat-stasher/src/main.rs:223-225,4188-4219`), and note that `read` therefore
   *is* a payload-output command — it prints conversation content. Restoring a
   whole archive is not something you can currently do with one command. If
   getting everything back in bulk matters to you, this is not ready for you
   yet.

5. **Search is metadata-only.** `search` walks snapshot/index/tree objects and
   never fetches or decrypts a data blob; full-text matching is not implemented
   (`crates/chat-stasher/src/main.rs:364-379`). It also distinguishes "nothing
   matched" from "could not finish reading", which is the same
   unknown-is-not-empty discipline as above
   (`crates/chat-stasher/src/main.rs:376-379`).

6. **Session enumeration is incomplete for some harnesses**, which means the
   archive can be incomplete in ways this document does not enumerate. See the
   limits section of `README.md`.

7. **Browser-side history backfill covers one platform, and one tier of it
   looks like coverage without being coverage.** Backfill recovers past
   conversation *text* on **ChatGPT** only
   (`apps/extension/lib/backfill/enumerate.ts:762`). On **DeepSeek** and
   **Perplexity** it enumerates your conversations and archives **none of
   them** (`apps/extension/lib/backfill/enumerate.ts:774`); on **Gemini**,
   **Claude**, and **Kimi** it does nothing
   (`apps/extension/lib/backfill/enumerate.ts:643`). The user-visible symptom of
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
- Whether browsers sync download history containing our filenames to a vendor
  account by default.
- TLS and host-key verification behaviour across every opendal backend the
  config accepts.
- Whether any chat platform's terms of service prohibit the capture or the
  backfill request pattern, and whether backfill triggers rate limiting or
  account flagging.
- The behaviour of the extension when the `storage` permission is absent at
  runtime; the code is written to fail closed, but this was not verified against
  a real browser (`apps/extension/wxt.config.ts:14-21`).
- Windows-specific path handling for at least one harness; see `README.md`.

We have not commissioned or performed a formal security audit of this project.

## If you want the strongest configuration available today

Not a promise, just the honest best case with the current code:

1. Use a **local destination on an encrypted volume**, which removes the
   destination-provider row entirely.
2. Keep your **download directory on the same encrypted volume**, and run
   `ingest` often to shorten the plaintext window.
3. Store the **key file somewhere other than the repository**, and back it up —
   losing it is unrecoverable (weakness 3).
4. On a platform without Unix file modes, check the key file's permissions
   yourself after first run — the tool can only set them where the platform can
   express them (weakness 2).
5. Run `verify` (`crates/chat-stasher/src/main.rs:280-318`) rather than assuming
   the archive is intact.
