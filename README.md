# chat-stasher

**An append-only, encrypted archive of your AI conversations, from 5+ platforms, on storage you control.**

**Local-first: there is no server of ours in the path.** Conversations are encrypted on your machine before they are stored. The archive is only ever added to. Anything the tool cannot see is reported as *unknown*, never quietly counted as zero.

<!-- screenshot: `chat-stasher ui` dashboard, totals, machine × source matrix, weekly heatmap (no session titles, paths or ids visible) -->

[![Release](https://img.shields.io/github/v/release/dimpurr/chat-stasher)](https://github.com/dimpurr/chat-stasher/releases/latest)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

[Quick start](#quick-start) · [Install](#install) · [Support at a glance](#support-at-a-glance) · [Where the archive lives](#where-the-archive-lives) · [Security](#security-and-privacy) · [Docs](#documentation)

## Works with the tools you already use

The coding agents and web AI chats you already use, in one archive ([status per platform](#support-at-a-glance)):

- **Coding agents:** Claude Code, Codex CLI, opencode, Gemini CLI, Cursor, Grok CLI, Kimi Code, Copilot CLI, aider, crush, Continue, Zed
- **Web chats:** ChatGPT, Claude, Gemini, Grok, DeepSeek, Perplexity, Kimi

## Quick start

```sh
curl -fsSL https://chatstasher.com/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"                   # the line the installer prints if this folder is not on PATH
chat-stasher doctor                                    # read-only: is anything deleting your history?
chat-stasher init && chat-stasher run-once --stage ~/stash/chat-stasher/stage
```

The first command installs the CLI into `~/.local/bin` (checksum-verified, no `sudo`); the `export` is the line the installer prints when that folder is not already on your `PATH`, and running it twice is harmless. `doctor` changes nothing; `run-once` makes your first local, encrypted archive.

The install script places a prebuilt macOS binary today. Linux and Windows builds arrive with **0.5.0**; until then those platforms build from source. All of it is in [Install](#install), and backups to a remote and scheduling are in [Your first archive](#your-first-archive).

## Why this exists

Your AI tools are not archives. Several delete history on a schedule you never chose:

- **Gemini CLI** keeps sessions under a directory literally named `tmp`. Its documented default retention is 30 days.
- **Claude Code** deletes transcripts older than `cleanupPeriodDays` at startup, with a default of 30 days.
- **Web chats** live in accounts you do not control, behind export features that can change without notice.

Once a conversation is gone from its tool, the archive is the only copy left. chat-stasher makes that copy continuously. It keeps every version it has made, and a deletion in a tool never reaches the archive.

It starts by answering one question, read-only: **is anything on this machine deleting my history right now?**

```sh
chat-stasher doctor
```

## How it works

```
 coding-agent sessions ──(chat-stasher CLI, hourly)──┐
                                                      ├─► stage ─► encrypt ─► your destination(s)
 web chats ──(browser extension → local host)────────┘       (local disk · SFTP · R2)
```

There are two ways in, and both end in the same archive:

- **The CLI** reads each tool's own session files where the tool keeps them, and never modifies them. New data is sealed into a local folder called the *stage*, then encrypted into a repository at your destination.
- **The browser extension** sees web conversations as the platform's own API returns them. It hands each one to the same `chat-stasher` binary on your machine, which your browser starts as a small local host. It never sends them to a server.

A **destination** is a place the archive lives, such as a folder on this disk, an SFTP server or an R2 bucket. You can keep more than one. Each is a full, independently encrypted copy with its own key file.

## Install

### The CLI

macOS (Apple Silicon and Intel):

```sh
curl -fsSL https://chatstasher.com/install.sh | sh
```

The script downloads a pinned release and checks its SHA-256 before installing. It puts `chat-stasher` in `~/.local/bin`, never uses `sudo`, and prints the one line to add if that folder is not on your `PATH`.

**Linux and Windows:** there is no prebuilt binary in a stable release yet. Both arrive with 0.5.0, a statically linked Linux binary for x86-64 and arm64 and an `.exe` for Windows. Until then, build from source with `cargo build --release`. [docs/install.md](docs/install.md) covers each system, updating and uninstalling.

### The browser extension

For web chats, the extension works alongside the CLI. It is not in any extension store yet: each release attaches `chat-stasher-extension-X.Y.Z.zip`.

1. **Register the local host first.** The extension delivers to it, and nothing else:

   ```sh
   mkdir -p ~/stash/chat-stasher/stage
   chat-stasher install-native-host --stage ~/stash/chat-stasher/stage
   ```

   The command prints every file it writes, and `--uninstall` removes exactly those. This half is per **machine**: one registration serves every browser and every profile on it, and `--uninstall` therefore takes the delivery channel away from all of them at once.
2. Download the zip from the [latest release](https://github.com/dimpurr/chat-stasher/releases/latest) and unzip it into a folder you will keep.
3. Open `chrome://extensions`, turn on **Developer mode**, click **Load unpacked**, and choose that folder.
4. **Reload any AI chat tab that was already open.** Then open the extension's popup: it should say it can reach the host.

**Do steps 3 and 4 in every browser profile you chat in.** An extension belongs to one profile, so a copy in Chrome's Personal profile captures nothing in its Work profile. Each copy has its own queue and its own backfill settings; all of them deliver into the one stage above, so the archive is still one archive.

Past conversations are a separate, opt-in step: switch on **backfill** in the popup. Backfill works through an open, logged-in tab of that platform, and goes gently by default. Its daily cap is **per install**: three profiles with backfill on are three schedules, so the account can see about three times one install's rate. The full walkthrough is in [docs/install.md](docs/install.md#the-browser-extension).

## Your first archive

With the CLI installed, this is the whole loop. [docs/start.md](docs/start.md) walks through the same steps with explanations, and `chat-stasher setup` runs the first pass of it for you.

**1. Write a config.** `init` writes a fully commented `~/.config/chat-stasher/config.toml`, and never overwrites one that exists.

```sh
chat-stasher init
```

**2. Archive once.** With no remote configured yet, this archives to an encrypted repository on this disk. That already protects you from a tool deleting its own history.

```sh
mkdir -p ~/stash/chat-stasher/stage
chat-stasher run-once --stage ~/stash/chat-stasher/stage
```

The first run ends like this:

```
[push] INIT (new repository created) · masterkey created+persisted
[run-once] result: COMPLETED snapshot=created exit_code=0
```

Running it again when nothing changed prints `result: NOOP` and creates no new snapshot. That is the normal, healthy case.

> [!WARNING]
> **Back up `~/.local/share/chat-stasher/masterkey.json` now.** It is the only key to your archive. If you lose it, the archive can never be read again. There is no recovery code and no reset, and nobody can help. Keep a copy somewhere other than this disk, such as a password manager.

**3. Keep it running.** `schedule` writes an hourly timer file:

```sh
chat-stasher schedule --stage ~/stash/chat-stasher/stage \
  --output ~/Library/LaunchAgents/com.chat-stasher.run-once.plist
```

`schedule` renders the timer, but it never installs it. It prints the exact command that does, which on macOS is a `launchctl bootstrap …` line. Run that command. On macOS, `chat-stasher schedule install` runs that last step for you; on Linux, add `--format systemd`.

`chat-stasher setup --install-schedule` does that same install as the last step of the wizard, and says when the timer will next run: the scheduler's own answer where it has one — the next elapse systemd reports for the timer, or the local time a launchd calendar slot resolves to — and otherwise why there is no time to report, which is the case for a launchd interval job and for a timer systemd has not armed. A cadence is never printed as a timestamp.

**4. Check on it any time.**

```sh
chat-stasher status
```

The first line is the verdict, for example `[run-once] Healthy: last run 12 minutes ago, …`, or the reason it is not healthy. `status` exits non-zero when the timer looks dead, and that includes "has never run".

**5. Add an off-site copy.** A local archive does not survive losing this disk. [docs/destinations.md](docs/destinations.md) sets up R2, SFTP or a second local disk.

## Support at a glance

This table is generated from the registry that ships inside the CLI and from the extension's own platform list, so it cannot drift from what the tool actually scans. **Supported** means the scanner or the extension acts on that tool. **Experimental** means the platform is enabled in the development build only. A date in **Last verified** appears only where a real session was archived end to end on a maintainer's machine, with the month of that check recorded; every other row shows a dash, because these tools change their storage without notice and a permanent checkmark would be a claim nobody has re-made.

<!-- support-matrix:short:start -->
**5+ platforms.** Local AI coding tools and web chats, archived the same way.

| Surface | Platform | Status | Last verified |
|---|---|---|---|
| Local | Claude Code | supported | - |
| Local | OpenAI Codex CLI | supported | - |
| Local | Gemini CLI | supported | - |
| Local | opencode | supported | - |
| Local | Cursor | supported | - |
| Local | Grok (xAI CLI) | supported | - |
| Local | GitHub Copilot CLI | supported | - |
| Local | aider | supported | - |
| Local | crush | supported | - |
| Local | Zed | supported | - |
| Local | Continue | supported | - |
| Local | Kimi Code | supported | - |
| Web | deepseek | supported | - |
| Web | perplexity | experimental | - |
| Web | chatgpt | supported | - |
| Web | gemini | supported | - |
| Web | claude | supported | - |
| Web | kimi | experimental | - |
| Web | grok | supported | - |

Verified means a maintainer archived a real session end to end on their own machine. Formats change, so a date is recorded instead of a permanent check.
<!-- support-matrix:short:end -->

Two things the table cannot show:

- A tool's sessions may live somewhere else on your machine. Point `[harness_roots]` in the config at the right path and the scanner looks there instead. A path that does not exist is reported as *unknown*, never as "0 sessions".
- When backfill gets back an incomplete conversation, it records a failure. It never stores the partial copy as if it were whole. A tab that was open before the extension was installed or updated is not captured until you reload it; the popup tells you when this applies.

## Where the archive lives

| Destination | Status | Best for |
|---|---|---|
| **Cloudflare R2** | ✅ Verified end to end (2026-09) | An off-site copy with a free tier (10 GB-month when the pricing page was checked, 2026-09-25) |
| **SFTP** (for example, a Hetzner Storage Box) | ✅ In daily use on three machines | An off-site copy on a server you rent |
| **A local folder or disk** | ✅ Supported | A single machine, or a second copy on an external disk |

Other backends: a **rustic REST server** (`rest:`) is accepted by the config but has not been verified, and other **S3-compatible services** take the same options as R2 without having been tested.

Declare a destination in `~/.config/chat-stasher/config.toml`, then initialise it once:

```sh
chat-stasher dest-init --destination <name> --stage ~/stash/chat-stasher/stage
```

`dest-init` seeds the new destination with this machine's history. That includes sessions your other destinations hold for this machine that it has since lost. After that, `run-once --destination <name>` keeps it current. Once any destination is declared, archive-reading commands need `--destination <name>`. There is deliberately no silent default; the one exception is `ui`, which opens your only destination when there is just one.

**One step here needs a human.** The first connection to a new SFTP server stops until you have compared its fingerprint with the one your provider publishes. chat-stasher never trusts a new host on its own.

[docs/destinations.md](docs/destinations.md) has a section for each destination: config, credentials, the fingerprint step, and known pitfalls.

## More than one computer

Install chat-stasher on each computer and point them all at the same destination. Each computer gets its own random identity on its first run, and writes only to its own part of the archive. Two laptops never overwrite each other, even if they share a hostname.

The dashboard shows every machine side by side, and `status --destination <name>` flags any machine running an older chat-stasher than the rest.

Several browser profiles on one computer are the same idea one level down, with one difference: they share that machine's identity, so their captures land in the same part of the archive. Install the extension in each profile you chat in, as above. A conversation that two profiles both captured is stored once only when the two deliveries are byte-identical; otherwise both are kept, because the archive records what was captured rather than deciding which profile was right.

## Getting your conversations back

- **Browse.** `chat-stasher ui` opens a dashboard on `127.0.0.1`, and needs no flag when the config leaves the choice unambiguous (one declared destination, or a default recorded). It shows totals, a machine × source matrix and a weekly heatmap, and you can drill into any cell. The list is sorted and paged on the server, and a conversation opens in a reader that fetches and decrypts that one session, printing the byte cost first.
- **Search inside conversations.** The dashboard's `/search` runs a query against a **local full-text index**, which `chat-stasher index build` creates from the archive into the operating-system cache directory. It matches literal substrings (including scripts without spaces) from three characters up, and every answer states how much of the destination that index covers — a session the index has not read cannot be searched, and the page says so rather than reporting no match. `chat-stasher index clear` deletes the index.
- **Find.** `chat-stasher search` finds sessions by machine, tool and **conversation date**: `--day 2026-01-15`, or `--since` / `--until`.
- **Take out.** `chat-stasher export --out <dir>` writes exactly the sessions `search` found as files in their native format, plus a checksummed `manifest.json`. `--dry-run` shows the cost and writes nothing. `chat-stasher read` prints a single session.

There is no command yet that puts sessions back into a tool's own folder.

## Scripts and agents

chat-stasher is built to be driven by scripts and agents that act on its answers:

| Exit code | `search`, `export`, `ui` | `status` |
|---|---|---|
| `0` | Answered completely | Timer healthy |
| `1` | Read everything, found nothing (`ui` never returns this) | Timer unhealthy, or it has never run |
| `3` | Could not finish, so a "0" would be unproven | The scan itself did not complete |
| `2` | Usage error | Usage error |

- `doctor --json` and `status --json` print exactly one JSON object. An unknown value is tagged `{"kind":"unknown","why":…}`, never written as `0` or `null`. `status --json` also carries a `local` layer — whether the scheduler units are installed, the last run, and the staged sessions still waiting to upload — and `overview --json --summary` returns aggregate totals, one record per machine and per source, and the last 30 local days, with no per-session array.
- `status` writes its human report to **stderr**, so `status | head` hides its exit code.
- `run-once` is safe to run again at any time.
- Never put credentials on the command line.

## Security and privacy

- **Nothing is sent to us.** There is no account, no telemetry and no server of ours. The CLI talks only to the destinations you configure. The extension talks only to the chat sites you already use and to the local host.
- **Encrypted before it leaves your machine.** Your storage provider sees encrypted objects. It can still see how much you back up, and when.
- **You hold the only key.** Nobody can recover it, and nobody can read the archive without it.
- **Append-only.** Each run adds a snapshot. The only thing chat-stasher deletes is its own staged copy, and only after every destination proves it holds those bytes.
- **Plaintext on your own disk.** Three things sit on your disk unencrypted: captures waiting in the extension, staged shards, and the key file. Other programs running as you can read them.
- **The extension** has four permissions and no site host permissions. Backfill requests are made from inside your own open tabs.

Read [docs/privacy-security.md](docs/privacy-security.md) before trusting chat-stasher with an archive you cannot recreate. To report a vulnerability, see [SECURITY.md](SECURITY.md).

## Common questions

**Does it change my tools' files?**
No. It reads session files where they are and copies what is new into its own stage. It never renames, truncates or deletes a file a tool owns.

**Why not just back up the folder?**
A folder sync follows deletions, so when the tool deletes a session, your copy goes too. A general-purpose backup keeps old versions, but it cannot tell you which conversations exist, when they happened, or which machine they came from. It also does not cover web chats. chat-stasher keeps every version, lets you search conversations by date across tools and machines, and says plainly what it could not see.

**Will backfill get my account flagged?**
Backfill is deliberately gentle: small batches spread through the day, under a daily cap, sent from your own logged-in tab. It is still automated traffic to a service whose terms you accepted, so leave it off if that worries you. Capturing the conversations you open costs little or nothing extra.

## Not done yet

- **No `restore` command.** Sessions come out with `read` and `export`, but nothing writes them back into a tool.
- **The extension is not in a store**, and backfill is not yet verified end to end on any platform.
- **No prebuilt Linux or Windows binary in a stable release yet**; they arrive with 0.5.0.
- **Search covers only what the local index holds, and only the dashboard can use it.** The index is a separate cache built by an explicit command; anything it has not read is unsearchable until you rebuild it, and the search page says which sessions that is. There is no command-line twin yet (`search --text`), and no linear scan mode for the one- and two-character queries the index cannot answer.

## Roadmap

- Cloudflare R2 as the documented default remote.
- A small macOS menubar app showing how fresh each backup is.
- Platform and date facets on the search page, and a command-line `search --text`.

Sessions are in the dashboard's search results because the index holds the text SQLite can match on; the archive itself is never the search target, so a stale index shows stale matches until it is rebuilt.
- A per-platform coverage page in the extension (next extension release), and store listings.

## Documentation

| I want to… | Start here |
|---|---|
| Get my first archive working | [docs/start.md](docs/start.md) |
| Install on my system, update or uninstall | [docs/install.md](docs/install.md) |
| Set up R2, SFTP or another disk | [docs/destinations.md](docs/destinations.md) |
| Decide whether to trust it | [docs/privacy-security.md](docs/privacy-security.md) |
| Look up a command's flags and exit codes | `chat-stasher <command> --help` |
| Work on the code | [CONTRIBUTING.md](CONTRIBUTING.md) · [docs-dev/](docs-dev/) |

Release notes are in [CHANGELOG.md](CHANGELOG.md). The CLI and the extension are versioned separately.

## Contributing

Issues and pull requests are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers the local checks and commit style. Please keep reports free of your own conversations, account names, hostnames and paths. A redacted `doctor` or `status` output is usually enough.

## License

Licensed under the [Apache License 2.0](LICENSE).
