---
name: chat-stasher
description: Install, check and operate chat-stasher, the encrypted append-only archive of AI conversations. Use when asked to back up, check or find AI chat history.
license: Apache-2.0
compatibility: Requires the chat-stasher CLI, version 0.5.0 or newer. Run `chat-stasher --version` first and use the manual path when the binary predates that.
metadata:
  homepage: https://github.com/dimpurr/chat-stasher
---

# chat-stasher

chat-stasher archives a person's AI conversations into an encrypted, append-only archive on storage they control. It covers coding-agent sessions, collected by the CLI, and web chats, captured by a browser extension. You are helping the user install it, check it, or use it.

This skill is deliberately thin. **The CLI is the authority. This file only tells you how to drive it safely.**

## Rules that always apply

1. **Start with the read-only checks, and read the version first.** Run `chat-stasher --version`, then `chat-stasher doctor --json` (see [Step 1](#step-1-check-the-machine)). The version decides which setup path you may offer, so read it before you offer either: the floor this file declares is in its `compatibility` line at the top, and rule 2 says what to do below it. If the binary is missing, go to [Step 0](#step-0-is-it-installed).
2. **Only use commands and flags that exist in the installed version.** Confirm them with `chat-stasher --help` and `chat-stasher <command> --help`. Do not invent subcommands. In particular:
   - `setup` needs **0.5.0 or newer**: that is the floor in this file's `compatibility` line. `0.4.0` and older have no such command at all, so on those tell the user and use the manual path in [Step 2](#step-2-first-archive-on-this-disk); do not offer a command that is not there. Ask `chat-stasher setup --help` whether it is there before you plan around it, and if it is, use it: even an early build does more than the manual path. What such a build may lack are the flags that came later. The release candidate `0.5.0-rc.2` has `setup`, but its destination and scheduler steps only print what they would do, so its `setup --help` does not list `--remote`, `--trust-host` or `--uninstall-schedule`. Confirm every flag before you pass it, and never spend the user's answer on a flag their build does not have;
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

The script installs to `~/.local/bin` without `sudo`, and prints a line to add if that folder is not on `PATH`. It installs the newest **stable** release, which may predate the Linux binaries; if it refuses for that reason, name a version that carries one. The variable has to reach `sh`, which means setting it on the **right-hand side of the pipe**: `sh` is what reads the script, so the assignment belongs to it, and `curl` still fetches the same script:

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

`setup` is the path. One run scans, archives once, proves that a second pass adds nothing, reads one session back out, and shows the user the one file they cannot lose. In a terminal it prompts. With no terminal it takes named flags instead, prints exactly one JSON object, and never prompts:

```sh
chat-stasher setup --stage <stage> --json
```

Use that form even when it would be chosen for you, because you are not a terminal: `--json` states the contract you are reading from.

- `--stage <stage>` is the one required parameter. The stage is a folder where sealed sessions wait before they are archived, so it must not be deleted. Suggest `~/stash/chat-stasher/stage`, and let the user confirm the path: every future run deposits their data there.
- A missing named parameter is **reported, never guessed**: `missing_parameters` names the flags the run needed and did not get, and the run exits `2`. Supply them and run it again. Nothing was written — with one exception, and it is the masterkey declaration described below: when that is the *only* parameter owed, the run creates the local repository and the key first, so the user has a file to copy, and stops there.
- The exit codes are the tool's own: `0` finished · `1` finished and a step failed · `3` did not finish reading, so nothing it failed to look at may be reported as absent · `2` a usage error. The `exit_code` field inside the object is the same decision as the process status, not a second opinion.
- Warnings go to stderr. Read stdout only, and parse it as one object.

Read the object before you report anything:

| Field | What it tells you |
|---|---|
| `steps.stage` | `provided` or `missing` |
| `steps.local_save` | `created` (this run made the repository and the masterkey), `existed`, `nothing_to_archive`, `failed`, `unknown`, or `not_attempted` when the run refused before the pass |
| `steps.masterkey` | `declared`, `not_declared` (the declaration is owed and not made), `not_attempted` (a run that refused before it looked), or `absent` when there is no repository and therefore no key |
| `steps.destination`, `steps.schedule`, `steps.native_host` | how far those steps got |
| `chain` | `init`, `noop` and `readback`, each `observed` where the run watched it happen, and an explicit unknown where it did not |
| `incomplete` | the steps that did not finish, named one by one |
| `unread` | the parts that could not be read. When this is not empty the exit code is `3`, and every empty collection elsewhere in the object means "did not look" rather than "nothing there" |

`chain.readback` being `known` is the evidence that the archive can be opened again; the manual path below cannot give you that. Report an unknown as unknown, with its `why`: never as `0`, never as `no`, never as `nothing`.

**Human step: the master key.** The key is `~/.local/share/chat-stasher/masterkey.json`, and two runs create it: the first pass that archives something, and — for the reason two paragraphs down — a headless run whose only owed parameter is the declaration. A machine with nothing to archive and no repository gets neither, and then `steps.masterkey` is `absent`, which is a third answer and not a "no": there is no key to back up yet, and saying otherwise would send the user looking for a file that is not there.

When a key does exist, the run records whether the user has said they keep a copy. Until they have, `steps.masterkey` is `not_declared` and the run exits `2` with `masterkey_saved_elsewhere` in `missing_parameters`. The file to copy is the one at `masterkey.path` — read it from there rather than from the default above, because a config can put the key somewhere else. Tell the user to copy that file somewhere off this disk, such as a password manager or an external drive, and say plainly that it is the only key to the archive and that a lost key cannot be recovered. Do not read or print the file. Wait for their answer, then re-run the same command with `--masterkey-saved-elsewhere`, which records it.

**When the declaration is the only thing owed, the run creates the key first.** A user cannot confirm they saved a file that does not exist yet, so a headless run whose `missing_parameters` is exactly `["masterkey_saved_elsewhere"]` creates the local repository and the key and *then* exits `2`, and `masterkey.path` is the new key's path. Nothing else ran: `steps.local_save`, `chain`, `runs` and `steps.schedule` are all `not_attempted`, and no snapshot was archived. That is the single exception to "an exit-2 `setup` wrote nothing", and it is a file the user is about to copy rather than an archive — never report it as an archive, and never as a completed run. Re-running the same command with `--masterkey-saved-elsewhere` continues from the key that is already there. If anything else is missing as well, this does not happen: the run refuses before writing anything at all.

Do not pass that flag before they answer: it is a declaration about their machine, and the tool's own output says so, with `masterkey.declaration_is_verified` set to `false`. Nothing here, and nothing anywhere else, can check that a copy exists.

The off-site copy and the hourly timer are the steps that follow, and `setup` can take both: [Step 4](#step-4-an-off-site-copy-optional-recommended) for `--destination` and `--remote`, [Step 3](#step-3-hourly-archiving) for `--install-schedule`.

### If the installed version has no `setup`

Below 0.5.0 there is no wizard, and the manual path is **less** than what `setup` does, not the same work: `setup` runs the pass twice and reads a session back out, while this runs **one** pass and reads nothing back, so it gives you no evidence that the archive can be opened again. Say that to the user rather than offering the two as equivalent.

```sh
chat-stasher init
mkdir -p <stage>
chat-stasher run-once --stage <stage>
```

`init` writes a commented `~/.config/chat-stasher/config.toml` only if none exists, and never overwrites one. Ask the user where the stage should live before running the second command. With no destination declared, `run-once` archives to `~/.local/share/chat-stasher/repo`. Success ends with `result: COMPLETED` on the first run, and `result: NOOP` when nothing changed. Both exit `0`. The master key step above applies here too, with one difference you have to read for yourself: a key exists only if the run actually archived something, so treat `COMPLETED` as "a key now exists" and `NOOP` as "there is nothing to back up yet".

## Step 3: hourly archiving

If you are already running `setup`, pass it `--install-schedule` and let the wizard do this as its last step: it installs the timer, exercises the scheduled command once, and reports when the timer will next run. Otherwise the work is two commands, one that renders the timer and one that installs it.

```sh
chat-stasher schedule --stage <stage> --output ~/Library/LaunchAgents/com.chat-stasher.run-once.plist
```

On Linux, use `--format systemd --output ~/.config/systemd/user/`. Without `--output`, `schedule` prints the rendered unit instead of writing it.

`schedule` **writes the timer file but does not install it**. `chat-stasher schedule install --stage <stage>` installs it with the platform's scheduler (add `--format systemd` on Linux), and `chat-stasher schedule uninstall` stops and removes it. Installing registers a background job, so show the user what it will do and run it only if they agree. Then confirm the timer with `chat-stasher status`.

The next run is reported where the scheduler itself can answer it, and otherwise the report says why there is no time to give. A cadence is never printed as a timestamp, so do not turn one into a timestamp yourself.

If destinations are declared (Step 4), `run-once`, `schedule` and `schedule install` also need `--destination <name>`.

## Step 4: an off-site copy (optional, recommended)

Read [docs/destinations.md](docs/destinations.md) for the exact config of each kind.

`setup` does all of it in one run when you hand it the recipe, and reports it in the same JSON object as the steps before it:

```sh
chat-stasher setup --stage <stage> --destination <name> --remote <kind> --json
```

`sftp` also needs `--remote-endpoint ssh://<host>:<port>` and `--remote-user <user>`. `s3` also needs `--remote-endpoint <url>`, `--remote-bucket <bucket>`, and the **names** of two environment variables, `--remote-access-key-id-env <VAR>` and `--remote-secret-key-env <VAR>`. Names are what those two flags take, never the credentials themselves: a pasted key is refused before anything is read or written. The user exports those variables in their own shell; the config records the names, so the values never pass through you, a command line, or a log.

A variable the user has not exported yet is not a reason to stop the run: the config block is still written, `credentials` reports that variable as unset, and the connection fails later with a credential error. Tell the user to export it, and run `setup` again (or `dest-init` on its own): the destination block does not have to change. A destination already declared in the config is adopted and verified as it stands, never rewritten, so a second run needs no parameters.

Then read `destination` in the object, field by field:

| Field | What it tells you |
|---|---|
| `config` | `written`, `already_declared`, `not_written`, `failed` |
| `reach` | `reached`, `untrusted_host`, `host_key_changed`, `unreachable`, `unreadable` |
| `trust` | `not_required`, `required`, `declared`, `declined`, with `known_hosts_write_authorized` and, for `declared`, `declaration_is_verified` |
| `dest_init` | `ran` with its `exit_code`, or `not_run` with the reason it never started |
| `credentials` | `checked` per variable, or `none_named`, or `not_checked` with why |
| `recommended_remote_kind` | the kind the wizard suggests |

Doing it by hand is the same work in more steps:

1. Ask which destination the user wants. The choices are Cloudflare R2, an SFTP server, or an external disk.
2. Show the config block from that page **with placeholders**. The user fills in the real values themselves, in `~/.config/chat-stasher/config.toml`. If the block holds credentials, suggest `chmod 600` on the file.
3. Initialise it:

   ```sh
   chat-stasher dest-init --destination <name> --stage <stage>
   ```
4. **SFTP, first connection: stop for the human step.** The command stops with exit code `3` and prints the fingerprints the server presented, and writes nothing. The `setup` form stops the same way: `trust.kind` is `required`, `known_hosts_write_authorized` is `false`, and `dest-init` does not run at all.
   - **Human step:** ask the user to compare the fingerprints with the ones their provider publishes.
   - Only if they say the fingerprints match, re-run with `--trust-host`, which `setup` takes under the same condition. It is the only thing in chat-stasher that writes `~/.ssh/known_hosts`, and the object records it as a declaration, not a verification: `declaration_is_verified` is `false`.
   - If a host's key has *changed* since it was trusted, stop. Do not try to get past it, and do not offer a way to.
5. Exit `3` means "did not finish reading". It never means "the destination is empty", and it never means the archive is not there.

## Step 5: web chats (optional)

The browser extension needs the local host, registered with the **same stage**. The stage must already exist: the host never creates one.

```sh
chat-stasher install-native-host --stage <stage>
```

`setup` does not do this step for you. Its `steps.native_host` field is a read-only check of whether a registration already exists on this machine, and it installs nothing, so a run that reports `none_registered` has told you what to do next rather than having done it.

`install-native-host` prints every file it wrote, and `--uninstall` removes exactly those. This half is per **machine**, not per browser profile: one run registers every installed browser, all of them pointing at the same stage, so never tell the user to run it again for another profile.

**Human step:** the user downloads `chat-stasher-extension-X.Y.Z.zip` from the [latest release](https://github.com/dimpurr/chat-stasher/releases/latest), unzips it into a folder they keep, and loads it in `chrome://extensions` → Developer mode → **Load unpacked**. Then they reload any chat tabs that were already open, because a tab that existed before the extension was installed is not captured until it is reloaded. You cannot click these for them.

**Ask which profiles, then repeat it in each one.** An extension is installed into a single browser profile, and `chrome://extensions` shows only that profile's extensions, so a user who chats in three profiles needs three `Load unpacked` passes, one in each profile. Ask before presenting the step: "which browser profiles do you chat in?" A profile they skip captures nothing, and the popup must be checked in each profile separately, because each copy has to reach the host on its own. Also tell them the host half is not repeated, and that `--uninstall` is machine-wide: it removes the delivery channel for every browser and profile at once, so it is not the way to drop one profile.

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
