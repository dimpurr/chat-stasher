---
name: chat-stasher
description: Install, check and operate chat-stasher, the encrypted append-only archive of AI conversations. Use when asked to back up, check or find AI chat history.
license: Apache-2.0
metadata:
  homepage: https://github.com/dimpurr/chat-stasher
---

# chat-stasher

chat-stasher archives a person's AI conversations into an encrypted, append-only archive on storage they control. It covers coding-agent sessions, collected by the CLI, and web chats, captured by a browser extension. You are helping the user install it, check it, or use it.

This skill is deliberately thin. **The CLI is the authority. This file only tells you how to drive it safely.**

## Rules that always apply

1. **Start with the read-only checks.** Run `chat-stasher --version`, then `chat-stasher doctor --json` (see [Step 1](#step-1-check-the-machine)). If the binary is missing, go to [Step 0](#step-0-is-it-installed).
2. **Only use commands and flags that exist in the installed version.** Confirm them with `chat-stasher --help` and `chat-stasher <command> --help`. Do not invent subcommands. In particular:
   - `setup` exists from the 0.5.0 release candidates onward. 0.4.0 and older do not have it, so check before you offer it and use the manual path in [Step 2](#step-2-first-archive-on-this-disk) if it is absent;
   - there is **no `restore`** command: nothing writes a session back into a tool's own folder;
   - there is **no `delete`** command.
3. **Secrets never pass through you.** Storage credentials (R2 / S3 keys, SFTP passwords) and the master key file are typed or copied by the user, **on their own machine, into their own files**. Do not ask for them in chat. Do not put them on a command line; command lines end up in logs and shell history. The two credential flags of `setup` name the **environment variables** that hold the credentials, never the values. Do not print key files, or config sections that hold credentials.
4. **Stop at the three human steps.** You cannot do these for the user, and nothing in the tool can verify them:
   - backing up the master key;
   - comparing a new SFTP server's fingerprint with the one the provider publishes;
   - loading the browser extension (**Load unpacked**).

   Say what the user must do, wait for them, and record their answer as *the user says done*, not as verified. The two sentences `setup` asks for (`I saved it elsewhere`, and `I compared the fingerprint with my provider's published one`) are declarations, and the tool says so in its own output: it cannot check either one.
5. **Never pass `--trust-host` on your own judgement.** Pass it only after the user has compared the fingerprints and told you they match.
6. **Do not delete** the stage, any repository, any key file or config, and do not uninstall anything, unless the user asks for that specific deletion.
7. **Unknown is not zero.** chat-stasher reports what it could not see as *unknown*, with a reason. Pass that on as unknown. Never summarise it as `0` or `none`.

## Step 0: is it installed?

```sh
chat-stasher --version
```

If the command is missing, ask the user before installing anything. On macOS and Linux:

```sh
curl -fsSL https://chatstasher.com/install.sh | sh
```

The script installs to `~/.local/bin` without `sudo`, and prints a line to add if that folder is not on `PATH`. It installs the newest **stable** release, which may predate the Linux binaries; if it refuses for that reason, name a version that carries one. The variable has to reach `sh`, which means setting it on the **right-hand side of the pipe** — `sh` is what reads the script, so the assignment belongs to it, and `curl` still fetches the same script:

```sh
curl -fsSL https://chatstasher.com/install.sh | CHAT_STASHER_VERSION=<version> sh
```

Windows is not installed by this script, because it is a POSIX `sh` script: it prints the URL of the `.exe` release asset to download instead.

Alternative installs, published from 0.5.0: `npm install -g chat-stasher` (a launcher, so it needs Node 18 or newer and nothing to compile) and `cargo install chat-stasher`. To build from source, `cargo build --release`, then **copy the binary out of `target/`**: timers and the browser host record the binary's path, and a path inside `target/` stops working after `cargo clean`. [docs/install.md](docs/install.md) covers each system, updating and uninstalling.

## Step 1: check the machine

```sh
chat-stasher doctor --json
```

It prints exactly one JSON object on stdout, and nothing else on stdout. It is read-only, and it never includes conversation text. Read **stdout only**: warnings can also be written to stderr, so do not merge the two streams before parsing.

Tri-state fields are tagged `{"kind":"known",…}`, `{"kind":"unknown","why":…}` or `{"kind":"not_applicable",…}`; an unknown is never serialised as `0`, `null` or a missing field. Two fields answer the question the tool exists for: `claude.verdict.kind` (one of `unset_default` / `safe` / `small_value` / `parse_failed`) and `gemini` (which reports `max_age`). A `config_error` that is not `null` means every empty collection in the object means "did not look" rather than "nothing there", and `not_checked` names which.

Report what it found in plain words: which AI tools are present, and whether any of them may be deleting old sessions. When a value is unknown, say so, and pass on its `why`.

Then find out whether chat-stasher is already set up:

```sh
chat-stasher status
```

The report is on **stderr**, and the first line is the verdict (for example `[run-once] Healthy: …`, `No run records yet …`, or `Last run failed: …`). Exit codes: `0` healthy · `1` unhealthy or never run · `3` the scan did not complete · `2` usage error. Read the exit code from `status` itself, not through a pipe.

If it is already healthy, skip to [Everyday use](#everyday-use).

## Step 2: first archive, on this disk

If `setup` exists in the installed version, prefer it: it runs the first pass, proves a second pass adds nothing, reads one session back out, and shows the user the one file they cannot lose.

```sh
chat-stasher setup
```

A non-TTY run does the same work from named flags and prints one JSON object, including any missing named parameters. The destination step writes a `[destinations.<name>]` block and then runs `dest-init`. The scheduler step is still a stub: it plans and prints, and installs nothing, so do [Step 3](#step-3-hourly-archiving) yourself.

Otherwise, do it by hand — but know that this is **less** than `setup` does, not the same work. `setup` runs the pass twice and reads a session back out; the manual path runs **one** pass and reads nothing back, so it gives you no evidence that the archive can be opened again. If the user needs that proof, use `setup`.

```sh
chat-stasher init
```

This writes a commented `~/.config/chat-stasher/config.toml` only if none exists, and never overwrites one.

Ask the user where the **stage** should live. The stage is a folder where sessions wait before being archived, so it must not be deleted. Suggest `~/stash/chat-stasher/stage`, then:

```sh
mkdir -p <stage>
chat-stasher run-once --stage <stage>
```

With no destination declared, this archives to `~/.local/share/chat-stasher/repo`. Success ends with `result: COMPLETED` on the first run, and `result: NOOP` when nothing changed. Both exit `0`.

**Human step: the master key — only if the run actually created one.** The key is made by the step that writes a snapshot, so a `NOOP` run creates neither the repository nor `~/.local/share/chat-stasher/masterkey.json`: there was nothing to archive, and `push` never ran. In that case there is no key to back up yet, and saying otherwise would send the user looking for a file that is not there. The step applies to the first run that archives something — read the `result:` line, and treat `COMPLETED` as "a key now exists".

When a key does exist, tell the user to copy `~/.local/share/chat-stasher/masterkey.json` somewhere off this disk, such as a password manager or an external drive, and say plainly that it is the only key to the archive and that a lost key cannot be recovered. Do not read or print the file.

## Step 3: hourly archiving

```sh
chat-stasher schedule --stage <stage> --output ~/Library/LaunchAgents/com.chat-stasher.run-once.plist
```

On Linux, use `--format systemd --output ~/.config/systemd/user/`.

`schedule` **writes the timer file but does not install it**, and the install action it offers is macOS-only. It prints the exact command to run instead. Show that command to the user, and run it only if they agree: it registers a background job. Then confirm the timer with `chat-stasher status`.

If destinations are declared (Step 4), `run-once` and `schedule` also need `--destination <name>`.

## Step 4: an off-site copy (optional, recommended)

Read [docs/destinations.md](docs/destinations.md) for the exact config of each kind. Your part:

1. Ask which destination the user wants. The choices are Cloudflare R2, an SFTP server, or an external disk.
2. Show the config block from that page **with placeholders**. The user fills in the real values themselves, in `~/.config/chat-stasher/config.toml`. If the block holds credentials, suggest `chmod 600` on the file.
3. Initialise it:

   ```sh
   chat-stasher dest-init --destination <name> --stage <stage>
   ```
4. **SFTP, first connection:** the command stops with exit code `3` and prints the fingerprints the server presented, and writes nothing.
   - **Human step:** ask the user to compare them with the fingerprints their provider publishes.
   - Only if they say the fingerprints match, re-run with `--trust-host`. It is the only thing in chat-stasher that writes `~/.ssh/known_hosts`.
   - If a host's key has *changed* since it was trusted, stop. Do not try to get past it.
5. Exit `3` means "did not finish reading". It never means "the destination is empty".

## Step 5: web chats (optional)

The browser extension needs the local host, registered with the **same stage**. The stage must already exist: the host never creates one.

```sh
chat-stasher install-native-host --stage <stage>
```

It prints every file it wrote, and `--uninstall` removes exactly those.

**Human step:** the user downloads `chat-stasher-extension-X.Y.Z.zip` from the [latest release](https://github.com/dimpurr/chat-stasher/releases/latest), unzips it into a folder they keep, and loads it in `chrome://extensions` → Developer mode → **Load unpacked**. Then they reload any chat tabs that were already open, because a tab that existed before the extension was installed is not captured until it is reloaded. You cannot click these for them.

## Everyday use

| The user wants to… | Command |
|---|---|
| Know if backups are working | `chat-stasher status` (add `--destination <name>` to spot a machine on an older chat-stasher) |
| Find sessions from a day or range | `chat-stasher search --destination <name> --day YYYY-MM-DD` (or `--since` / `--until`; with no destination declared, `--repo <path>`) |
| Get sessions out as files | `chat-stasher export --destination <name> … --out <empty dir>`; add `--dry-run` first to see the cost |
| Prove one session is intact | `chat-stasher read --session <id> …` (prints shard names and SHA-256 values, not the conversation) |
| Browse, and read one conversation | `chat-stasher ui` (with no flag when the config leaves the choice unambiguous); prints a local URL with a one-time token |
| Check archive integrity | `chat-stasher verify --destination <name> --level l1` |

`search`, `export` and `ui` exit `0` when answered completely, `3` when they could not finish, and `2` on a usage error. `search` and `export` also exit `1` when everything was read and nothing matched; **`ui` never exits `1`**. With `3`, a result of "0 found" is **not** proof of absence, so say so: `search` says this itself, and it exits `3` rather than `1` whenever a session exists that it could not place in time.

`export` and `ui` are the two commands that reach conversation content: `export` writes it to files under `--out`, and `ui` serves a one-line label per session and fetches a session's full text only after the user opens it. `search` reads metadata only. Use them when the user asks to see or extract conversations, and do not paste large amounts of content back into the chat unless asked.
