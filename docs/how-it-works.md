# How it works

This page explains the moving parts: where a conversation goes, what is encrypted when, how several machines and browser profiles share one archive, and why chat-stasher answers *unknown* instead of zero. Nothing here is needed to use it. For that, start with [start.md](start.md).

<!-- RELEASE GATE: sections marked "(next release)" describe behaviour merged after 0.5.0-rc.2. Remove the marks when the release that carries them ships, or drop those sections if it does not. -->

## The pipeline

```
 AI tools' own session files ──(collect)──┐
                                          ├─► stage ──(push)──► destination(s)
 browser extension ──(local host)─────────┘   plaintext,        encrypted,
                                              this disk         append-only
```

| Stage of the trip | What happens | Encrypted? |
|---|---|---|
| **Source** | Each AI tool keeps its sessions in its own files or database. chat-stasher opens them read-only and never renames, truncates or deletes them. | As the tool left them |
| **Extension queue** | A web chat captured by the extension waits in that browser profile's storage until the host confirms delivery. | No |
| **Stage** | New data is copied into a folder you chose, as **sealed shards**: numbered, append-only files per session. | No |
| **Destination** | `push` encrypts and snapshots the stage tree. A successful push adds a **snapshot**; `run-once` skips the push when collection finds no change. | Yes, before anything leaves the machine |

`run-once` is `collect` followed by `push`, with the push skipped when nothing changed.

## Collecting from the tools

For each supported tool, a built-in **registry** records where its sessions live on each operating system, in what format, and how that path was established (from the tool's source code, its documentation, a local measurement, or an unconfirmed claim). [support.md](support.md) prints that registry.

- **Only what is new is read when a source grows normally.** For a file that grows, chat-stasher remembers how far it has read and a hash of what it has already taken. If the committed prefix changes or the file shrinks, it rereads the current content and seals any complete records as another shard; JSONL with no complete lines and an empty whole file produce no shard. Earlier staged shards are retained. For a database, it remembers a high-water mark. That state lives in chat-stasher's own data folder, never inside a tool's folder.
- **A file is never renamed out from under a tool** unless the registry has confirmed that tool tolerates it. Tools that keep a file open, or store sessions in a database, are only ever read.
- **An unverified path is not guessed.** Where the registry has no confirmed path for your system, the scanner reports *unknown* rather than scanning a guess. `[harness_roots]` in the config tells it where to look ([config.md](config.md#harness_roots)).
- **Sessions a build cannot archive are named.** If a tool has sessions in a shape this version cannot read, `collect` says so and exits `3` (partial), rather than reporting success.

## The archive

The archive is an encrypted, content-addressed repository built on the open-source **rustic** backup engine.

| Property | What it means for you |
|---|---|
| **Encrypted on your machine** | The destination receives encrypted objects only. It can see their number, size and timing. |
| **One key per destination** | The key file is created when the archive is created. Without it, nobody can read that archive, and there is no recovery. |
| **Append-only** | Each successful push adds a snapshot; a no-change `run-once` normally creates none, but it can append a repair snapshot when this machine's archived activity index was written by an older version. A session disappearing from its source stays in its staged shards and every snapshot that already holds it; source-side deletion never propagates to the archive. Archive removal is reserved for an explicit user-initiated purge of a named session, but that feature is not shipped and there is currently no command to delete one conversation. See [Privacy and security → Keeping and deleting](privacy-security.md#keeping-and-deleting). |
| **Deduplicated** | Identical data is stored once, so hourly snapshots of mostly unchanged sessions stay small. Shards are grouped into buckets (20 by default) so each push rewrites little. |
| **Verifiable** | `verify --level l1` checks structure cheaply. `l2` downloads and re-hashes everything. `l3` checks every staged session against the archive. |

### Machines and partitions

Each machine gets a random 128-bit **identity** on its first run, and writes only to its own **partition** of the archive: `sessions/<machine>/…`. Two machines never write over each other, even with the same host name.

That is why several machines can push to one destination, and why the dashboard can show them side by side. `machine-declare` gives a machine a readable name, and `machine-label` names one that can no longer name itself, such as a sold laptop. That last case is a whole task rather than a command: [Recover a lost machine's conversations](use-cases/lost-machine.md) is it end to end.

### Destinations are full copies

A destination is **a place a copy lives**, never a share of one archive. Each has its own repository and its own key.

- **`dest-init`** seeds a new one with this machine's partition: first everything this machine's tools still have, then anything another destination holds for this machine that the machine has since lost. Other machines' partitions arrive when those machines push to it.
- **There is no default destination.** Once any is declared, a command that touches an archive must name one, so you never act on the wrong copy by accident.
- **A copy you cannot read is not an empty copy.** If a destination cannot be reached, `dest-init` reports its difference as incomplete. It never assumes that destination had nothing extra.

## Two tiers: metadata and content

Reading an archive has two very different costs, and chat-stasher keeps them apart.

| Tier | What it reads | Used by |
|---|---|---|
| **Metadata** | Snapshot, index and tree objects, plus each machine's small **activity index**: per session, its tool, its first and last message time, and a one-line label (at most 100 characters). | `status --destination`, `overview`, `search` without `--text`, every list in the dashboard |
| **Content** | The conversation itself, fetched and decrypted | `read`, `export`, the dashboard's reader (after you ask, with the cost shown first), `search --scan`, `index build` |

On a small test archive, the metadata walk read about 12 KB against 1.2 MB of conversation data: two orders of magnitude. That is why you can search years of history by date without downloading it.

Dates always mean the **conversation's own** time, taken from the activity index, not when the backup ran. A conversation from January found in a push made in June still matches `--day` in January.

A session whose time cannot be established is listed separately, with the reason. It is never dropped from a result, and never counted as "not matching".

## Local helpers: caches and the index

Three things chat-stasher keeps on this machine are **not** part of the archive, and are safe to delete:

| Helper | Holds | Plaintext? |
|---|---|---|
| **Metadata cache** | Archive metadata, to make re-opening fast | No conversation data |
| **Body cache** | Conversations you opened, as the destination's own **encrypted** bytes, re-checked before every use | No |
| **Full-text index** (next release) | User and assistant text, extracted by `index build`, one index per destination, in a SQLite database in the system cache folder | **Yes** |

The full-text index is the one place conversation text is written outside the stage and the archive. It is never uploaded. It is also never treated as the archive: every search answer says how much of the destination the index covers, and a session the index has not read is reported as unsearched, not as "no match". [privacy-security.md](privacy-security.md) covers what that means for your threat model.

## Web chats: the extension and the host

The browser extension sees a conversation as the chat site's own page receives it. It has no permission to reach any site by itself, and it cannot write files. So it hands every capture to a small **host**: the same `chat-stasher` binary, started by the browser on demand, which writes it into the stage. From there it is archived like any other session.

### One host per machine, one extension per profile

A real setup is several machines, each with several browsers, each with several profiles. chat-stasher is built for that shape:

| Level | How many | What is shared |
|---|---|---|
| **Machine** | One host registration, one stage, one identity | Every browser and profile on the machine delivers into the same stage, under the same machine partition. |
| **Browser profile** | One extension install each, with its own id and a name you give it | Its own queue of undelivered captures, its own backfill progress. |

Consequences:

- **Install the extension in every profile you chat in.** A copy in one profile captures nothing in another.
- **Counts are never added across installs.** Two profiles signed in to the same account may both capture the same conversation. The archive stores byte-identical deliveries once, and keeps both when they differ. It records what was captured, and does not decide which profile was right. The dashboard counts distinct conversations and raw copies separately.
- **Every install reports for itself** (next release). At the end of each backfill round, an install writes a short status report into the stage: browser, profile name, extension version, and per platform what it captured and what it still owes. The report carries counts and labels, never conversation text. It is archived with everything else, so any machine's dashboard (`ui --view extensions`) can list every install's last report. A report that is old is shown as stale, never as zero.
- **Only this machine's installs can be opened** from the dashboard. For an install on another machine, the page says which machine and profile to open it on.

### Backfill

Capturing the conversations you open costs little or nothing extra. **Backfill**, archiving your past conversations, is opt-in per platform. It runs from inside an open, logged-in tab of that platform, in small batches spread through the day.

(next release) Backfill is coordinated per machine through the host: when several installs on one machine backfill the same account on the same platform, one runs at a time, and a rate limit one install receives pauses that platform for all of them. Nothing is coordinated across machines, because there is no server between them.

A conversation that comes back incomplete is recorded as a failure, with a reason. It is never stored as if it were whole.

### The extension without the CLI

The extension also works before the CLI is installed, or without it (next release). Captures then wait in the browser, and the popup says plainly that they exist only in this browser and are **not a backup yet**. It offers the one-line installer, and an export of undelivered captures as a file, which `chat-stasher ingest` archives later. The queue holds up to 256 MiB: backfill pauses at 80 percent, and new captures are refused at 100 percent. Nothing already queued is ever dropped. When a host turns up, the backlog is delivered by itself, and the popup says how many.

## What the stage keeps, and when it shrinks

Collection retains previously sealed shards, including when a source is rewritten, truncated or removed. The stage shrinks only when `reclaim-stage --apply` removes a session's shard body after **every** declared destination proves, by the archive's own digest (shard count, bytes and SHA-256), that it holds exactly those bytes. The digest summary and shard sequence counter remain in the stage. One unreachable destination blocks the whole run; nothing is deleted on a guess. See [schedule.md → The weekly stage clean-up](schedule.md#the-weekly-stage-clean-up-optional).

## Why "unknown" is never "0"

The purpose of an archive is to be the copy you rely on when the original is gone. A tool that reports "0 sessions" for a folder it could not read, or "empty" for a destination it could not reach, tells you the one thing you most need to be true is true, without having looked.

So chat-stasher keeps three answers apart, everywhere:

| Answer | Exit code | Example |
|---|---|---|
| Found something | `0` | `search` matched sessions. |
| Looked everywhere, found nothing | `1` | Every session was read and placed in time, and none matched. |
| Could not finish looking | `3` | A destination timed out, or some sessions have unknown dates. |

In JSON, an unmeasured value is `{"kind":"unknown","why":…}`, never `0` or `null`. In the terminal and the dashboard, it is the word *unknown*, with the reason.

## What chat-stasher does not do

- **It runs no service.** The timer is your system's scheduler, the dashboard exists only while `ui` runs, and the host starts only when the browser calls it.
- **It has no server.** Nothing is sent to the project. The CLI talks only to your destinations. The extension talks only to the chat sites you use and to the host on your machine.
- **It does not restore into tools.** Sessions come out through `read`, `export` and the dashboard. No command writes them back into a tool's own folder.
