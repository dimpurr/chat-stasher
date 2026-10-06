---
name: chat-stasher
description: Install, check and operate chat-stasher, the encrypted append-only archive of AI conversations. Use when asked to back up, check or find AI chat history.
license: Apache-2.0
compatibility: Requires the chat-stasher CLI, version 0.5.0 or newer. Run `chat-stasher --version` first, and check for a second copy on `PATH` before falling back to the manual path.
metadata:
  homepage: https://github.com/dimpurr/chat-stasher
---

# chat-stasher

chat-stasher archives a person's AI conversations into an encrypted, append-only archive on storage they control. It covers coding-agent sessions, collected by the CLI, and web chats, captured by a browser extension. You are helping the user install it, check it, or use it.

This skill is deliberately thin. **The CLI is the authority. This file only tells you how to drive it safely.**

## Rules that always apply

1. **Start with the read-only checks, and read the version first.** Run `chat-stasher --version`, then `chat-stasher doctor --json` (see [Step 1](#step-1-check-the-machine)). The version decides which setup path you may offer, so read it before you offer either: the floor this file declares is in its `compatibility` line at the top, and rule 2 says what to do below it. If the binary is missing, go to [Step 0](#step-0-is-it-installed).
2. **Only use commands and flags that exist in the installed version.** Confirm them with `chat-stasher --help` and `chat-stasher <command> --help`. Do not invent subcommands. In particular:
   - `setup` needs **0.5.0 or newer**: that is the floor in this file's `compatibility` line. Ask `chat-stasher setup --help` whether it is there before you plan around it, and if it is, use it: even an early build does more than the manual path, and what such a build may lack are the flags that came later, so confirm every flag before you pass it and never spend the user's answer on a flag their build does not have. A build with no `setup` is **not** proof that this machine has none: the copy that answered may be an old one earlier on `PATH` while a newer one sits where the installer puts it (`~/.local/bin`), so ask the shell for every copy (`type -a chat-stasher`) and look before you conclude. Only when the newest copy is still below 0.5.0 does the manual path in [Step 2](#step-2-first-archive-on-this-disk) apply — do not offer a command that is not there;
   - there is **no `restore`** command: nothing writes a session back into a tool's own folder;
   - there is **no `delete`** command.
3. **Secrets never pass through you.** Storage credentials (R2 / S3 keys, SFTP passwords) and the master key file are typed or copied by the user, **on their own machine, into their own files**. Do not ask for them in chat. Do not put them on a command line; command lines end up in logs and shell history. The two credential flags of `setup` name the **environment variables** that hold the credentials, never the values. In the config that value may also be a **reference** rather than a variable name: `env:NAME`, `file:PATH`, `env-file:PATH:NAME`, or, on macOS, `keychain:ACCOUNT` ([docs/config.md](docs/config.md#credentials-in-options)). Do not print key files, or config sections that hold credentials.
4. **Stop at the three human steps.** You cannot do these for the user, and nothing in the tool can verify them:
   - backing up the master key — **every key file the run names, one per archive copy**, not one for the machine (see [Step 2](#step-2-first-archive-on-this-disk));
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

The script installs to `~/.local/bin` without `sudo`, and prints a line to add if that folder is not on `PATH`. That folder is also the reason a second copy is easy to miss: an older binary in a directory that comes first on `PATH` keeps answering after a newer one is installed, and its version is what rule 2 makes you check before you plan anything. It installs the newest **stable** release, which may predate the Linux binaries; if it refuses for that reason, name a version that carries one. The variable has to reach `sh`, which means setting it on the **right-hand side of the pipe**: `sh` is what reads the script, so the assignment belongs to it, and `curl` still fetches the same script:

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

When a configured browser stage exists, `stage_duplicate_shards` reports any same-session shards with identical hashes. If it finds duplicates, tell the user fixed readers collapse those bodies automatically, then offer `chat-stasher repair-duplicates --destination <name> --json` to inventory the archived destination. The command is read-only; physical removal is a separate decision from Dim.

Tri-state fields are tagged `{"kind":"known",…}`, `{"kind":"unknown","why":…}` or `{"kind":"not_applicable",…}`; an unknown is never serialised as `0`, `null` or a missing field. Two fields answer the question the tool exists for: `claude.verdict.kind` (one of `unset_default` / `safe` / `small_value` / `parse_failed`) and `gemini` (which reports `max_age`). A `config_error` that is not `null` means every empty collection in the object means "did not look" rather than "nothing there", and `not_checked` names which.

Report what it found in plain words: which AI tools are present, and whether any of them may be deleting old sessions (`risks`). The same object is also where a broken install shows up, so read these two blocks when something is wrong: `destinations`, one entry per declared destination, whose `kind` says whether it was reached and whose `detail` carries the backend's own message when it was not; and `native_host`, whose `step` is `registered` / `none_registered` / `nothing_to_look_at`, with the browsers it `detected` and the `stage` it is registered against (`{"kind":"not_configured"}` when the config records none). When a value is unknown, say so, and pass on its `why`.

Run `doctor` **without** `--json` only for the user to read: its human report goes to stderr, so a pipe on stdout shows nothing. [docs/troubleshooting.md](docs/troubleshooting.md) starts from these same two commands and is the page to follow whenever something looks wrong — a destination that stopped being reachable, a host that is not registered, or a run that stopped working.

Then find out whether chat-stasher is already set up:

```sh
chat-stasher status
```

The report is on **stderr**, and the first line is the verdict (for example `[run-once] Healthy: …`, `No run has ever been recorded: …`, or `Last run failed: …`). Exit codes: `0` healthy · `1` unhealthy or never run · `3` the scan did not complete · `2` usage error. Read the exit code from `status` itself, not through a pipe. `status --json` puts the same verdict in one object on stdout, with `exit_semantics` spelling out what its own code means, `local.schedule` for the timer's `next_run` and why there is none, and `local.stage` for the stage's backlog — the stage counts are `unknown`, with a `why`, when no `[native_host] stage` is configured, and that is not a zero.

If it is already healthy, skip to [Everyday use](#everyday-use).

## Step 2: first archive, on this disk

`setup` is the path. One run scans, archives once, proves that a second pass adds nothing, reads one session back out, and shows the user the one file they cannot lose. In a terminal it prompts. With no terminal it takes named flags instead, prints exactly one JSON object, and never prompts:

```sh
chat-stasher setup --stage <stage> --json
```

Use that form even when it would be chosen for you, because you are not a terminal: `--json` states the contract you are reading from.

- `--stage <stage>` is the one required parameter. The stage is a folder where sealed sessions wait before they are archived, so it must not be deleted. Suggest `~/stash/chat-stasher/stage`, and let the user confirm the path: every future run deposits their data there.
- A missing named parameter is **reported, never guessed**: `missing_parameters` names the flags the run needed and did not get, and the run exits `2`. Supply them and run it again. Nothing was written — with one exception, and it is the masterkey declaration described below: when that is the *only* parameter owed, the run creates the local repository and the key first, so the user has a file to copy, and stops there.
- The exit codes are the tool's own: `0` finished · `1` finished and a step failed · `3` did not finish reading, so nothing it failed to look at may be reported as absent · `2` a usage error. The `exit_code` field inside the object is the same decision as the process status, not a second opinion. When more than one applies, `3` is reported before `1` before `2`: a run whose destination could not be read exits `3` **even though** the local save before it succeeded, and that object can carry a non-empty `missing_parameters` too — read it, and do not report such a run as a usage error.
- Warnings go to stderr. Read stdout only, and parse it as one object.

Read the object before you report anything:

| Field | What it tells you |
|---|---|
| `steps.stage` | `provided` or `missing` |
| `steps.local_save` | `created` (this run made the repository and the masterkey), `existed`, `nothing_to_archive`, `failed`, `unknown`, or `not_attempted` when the run refused before the pass |
| `steps.masterkey` | `declared`, `not_declared` (the declaration is owed and not made), `not_attempted` (a run that refused before it looked), or `absent` when there is no repository and therefore no key |
| `steps.destination`, `steps.schedule`, `steps.native_host` | how far those steps got: `not_attempted` before the run reached them, `skipped` for a step the run did not owe, and the step's own outcome once it ran (`steps.native_host` is `registered` / `none_registered` / `nothing_to_look_at`, and it installs nothing — see [Step 5](#step-5-web-chats-optional)) |
| `masterkey` | the keys this run names, and whether each has a declared backup: `keys[]` is one entry per archive copy — the local one and each destination's. See [the human step](#step-2-first-archive-on-this-disk) |
| `destination` | the off-site step in full, including `kind` and, when no destination was named, `consequence` — see [Step 4](#step-4-an-off-site-copy-optional-recommended) |
| `chain` | `init`, `noop` and `readback`. Each link is `observed` only where the run watched it happen; `not_observed` is its own state when it did not (a repository that already existed is `not_observed`, not unknown), and `not_applicable` / `unknown` are two further states |
| `incomplete` | the steps that did not finish, named one by one |
| `unread` | the parts that could not be read. When this is not empty the exit code is `3`, and every empty collection elsewhere in the object means "did not look" rather than "nothing there" |

`chain.readback` being `known` is the evidence that the archive can be opened again; the manual path below cannot give you that. Report an unknown as unknown, with its `why`: never as `0`, never as `no`, never as `nothing`.

**Human step: the master keys — one per archive copy, not one in total.** The local key is `~/.local/share/chat-stasher/masterkey.json`, and two runs create it: the first pass that archives something, and — for the reason two paragraphs down — a headless run whose only owed parameter is the declaration. A machine with nothing to archive and no repository gets neither, and then `steps.masterkey` is `absent`, which is a third answer and not a "no": there is no key to back up yet, and saying otherwise would send the user looking for a file that is not there.

**Every destination has its own key as well**, `~/.local/share/chat-stasher/masterkey-<destination>.json`, and it is not the same file as the local one. A second machine reads a destination with **that destination's** key file and with nothing else: a user who backs up only `masterkey.json` and then loses this machine cannot read the off-site copy, and the tool says so with exit `3` (`cannot read masterkey file … (lost key?)`) rather than pretending the archive is empty. Never tell a user that one key covers the archive — it covers one copy of it, and each declared destination is another copy with another key.

The object lists every key this run is asking about, in `masterkey.keys`:

| Field | What it tells you |
|---|---|
| `masterkey.keys[].scope` | `local` for this machine's archive, `destination` for a destination's copy |
| `masterkey.keys[].name` | the destination's name, for a `destination` entry; absent for `local` |
| `masterkey.keys[].path` | the file to copy. **Read it from here**, not from the defaults above — a config can put a key somewhere else |
| `masterkey.keys[].declared` | whether the user has declared they keep a copy. Always paired with `declaration_is_verified: false` |

That array is the list to hand the user: every file in it, copied somewhere off this disk — a password manager, an external drive, a backup they already keep. `masterkey.path` is the same value as the `local` entry's `path`, kept for callers that predate the array. Do not read or print any of these files.

When a key exists, the run records whether the user has said they keep a copy of it. Until they have, `steps.masterkey` is `not_declared` and the run exits `2` with `masterkey_saved_elsewhere` in `missing_parameters`. Tell the user plainly that each of these files is the only key to its own copy of the archive, and that a lost key cannot be recovered. Wait for their answer, then re-run the same command with `--masterkey-saved-elsewhere`: **one declaration covers every key the run named that was already on this machine**, and the run records it per copy, so a later `status` or `doctor` can say which of them the user has declared.

That flag cannot cover a key the run itself creates, because the user cannot have copied a file that did not exist when they answered — and that holds for **every** key, not only a destination's. On a machine where the local repository's key does not exist yet, a first run with the flag creates it, so the flag cannot cover it either: the `local` entry of `masterkey.keys` has `declared: false` and the path of the file just created, the run stops with `2`, and a second run is what records the declaration. A destination's key is the same case one step later: it is created by `dest-init` *during* the run, so a first run with an off-site copy exits `2` even with the flag on the command line, with `missing_parameters` `["masterkey_saved_elsewhere"]` again and the `destination` entry of `masterkey.keys` carrying `declared: false` and the path of the file just created. That run did everything it could — `steps.local_save` and `steps.destination` read as they would on a completed run — and the declaration is the only thing left owed. Do not re-try the same command or look for a missing flag: nothing is wrong with it. Hand the user that path, wait for them to copy it, and run the same command again; the file is on the disk by then and the declaration is recorded. A `declared: false` entry is always the one still owed, whether the run asked and was not answered or created the file itself.

A `masterkey.path` — or a `local` entry in `masterkey.keys` — means that key **is on the disk**, whatever the run's exit code was: a run that created the key and then could not read its destination still exits `3`, and that is not a reason to skip the human step. Only when `masterkey` is `absent` is there no local key to copy.

Step 4's destination key is reported in the same array, but only once `dest-init` has run and created it — so a run that never reached the destination, or whose destination was already declared, has no `destination` entry, and that is a fact about this run and not about the key. A destination whose key exists but has no declared backup is reported by `status` and `doctor` on later runs, with its path, so it can still be caught outside the wizard.

**When the declaration is the only thing owed and the archive pass has not run, the run creates every key it will ask about and stops there.** A user cannot confirm they saved a file that does not exist yet, so a headless run that owes only the declaration and refused *before* the local archive pass — `missing_parameters` is exactly `["masterkey_saved_elsewhere"]` **and** `steps.local_save` is `not_attempted` — creates the keys this run names and *then* exits `2`; `masterkey.keys` is the list to hand over, and `masterkey.path` is the `local` entry's path. That `steps.local_save` half is what tells the two exit-`2` shapes apart: a run that *was* given the flag on a machine where the local key did not exist has the same `missing_parameters`, but it ran the archive pass, so its `steps.local_save` is `created` and its `chain` and `runs` are real — its shape is the paragraph above, not this one. The local repository and its key come first. When a destination was named, the destination step runs here too — `dest-init` is what creates the destination's own key, so without it the key would not exist to be copied — and the destination key joins `masterkey.keys`. That is what makes **a first setup with an off-site copy two runs, not three**: one run creates every key, the next declares them all (see the paragraph above for why the creating run cannot declare any of them). A `destination` entry appears here only once `dest-init` has created the key; a step that could not reach its destination leaves it out, and the `destination` object says why. What did not run: `steps.local_save`, `chain`, `runs` and `steps.schedule` are all `not_attempted`, and the local archive pass did not run, so the local repository holds no snapshot. That is the shape that distinguishes it from the paragraph above: there, the run got all the way through and owes only the declaration; here it stopped before the local archive pass. It is the one case in which an exit-2 `setup` writes before doing anything else, and what it wrote is a file the user is about to copy rather than a local archive — never report it as a completed run. Re-running the same command with `--masterkey-saved-elsewhere` continues from the keys that are already there. If anything else is missing as well, this does not happen: the run refuses before writing anything at all.

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

If you are already running `setup`, pass it `--install-schedule` and let the wizard do this as its last step: it installs the timer, exercises the scheduled command once, and reports when the timer will next run. That is `steps.schedule` in the object, and `schedule.status` beside it: `installed` when the timer was written, `installed_and_checked` once the exercise returned `0`, `self_check_failed` when it did not — which also puts `schedule` into `incomplete` — and `failed` when the install itself did. Otherwise the work is two commands, one that renders the timer and one that installs it.

```sh
chat-stasher schedule --stage <stage> --output ~/Library/LaunchAgents/com.chat-stasher.run-once.plist
```

On Linux, use `--format systemd --output ~/.config/systemd/user/`. Without `--output`, `schedule` prints the rendered unit instead of writing it, and it prints the exact `launchctl bootstrap` (or `systemctl --user`) line that would install it.

`schedule` **writes the timer file but does not install it**. `chat-stasher schedule install --stage <stage>` writes it into the platform's own folder *and* loads it with the platform's scheduler (add `--format systemd` on Linux), and `chat-stasher schedule uninstall` stops and removes it. Rendering first is optional: `install` puts the file in the same place, so the two paths end at one timer, not two. Installing registers a background job, so show the user what it will do and run it only if they agree. Then confirm the timer with `chat-stasher status`.

The timer template names the binary by **absolute path**, taken from the running executable. A path inside `target/` is rejected, and `--binary <path>` states one instead — which is why the copy the installer puts outside `target/` is the one a timer should point at.

There is a second unit: `--unit reclaim-stage` renders the weekly stage clean-up rather than the hourly archive, and it is a different cadence with a different job. Do not offer it as a substitute for the hourly one.

The next run is reported where the scheduler itself can answer it, and otherwise the report says why there is no time to give — a launchd interval job, for instance, has no next fire time to give, and the report says so. The field is `schedule.next_run` in the `setup` object and `local.schedule.next_run` in `status --json`; empty it comes with a `next_run_note` (`setup`) or `next_run_why` (`status`). A cadence is never printed as a timestamp, so do not turn one into a timestamp yourself.

If destinations are declared (Step 4), `run-once`, `schedule` and `schedule install` also need `--destination <name>`. And whatever a scheduled command needs in order to reach a destination has to be readable **without a shell**: a timer does not inherit the environment of the shell that installed it, so a credential the user only exported in their terminal is one the hourly run will not have. Step 4 says which forms work.

## Step 4: an off-site copy (optional, recommended)

Read [docs/destinations.md](docs/destinations.md) for the exact config of each kind.

`setup` does all of it in one run when you hand it the recipe, and reports it in the same JSON object as the steps before it:

```sh
chat-stasher setup --stage <stage> --destination <name> --remote <kind> --json
```

`sftp` also needs `--remote-endpoint ssh://<host>:<port>` and `--remote-user <user>`. `s3` also needs `--remote-endpoint <url>`, `--remote-bucket <bucket>`, and the **names** of two environment variables, `--remote-access-key-id-env <VAR>` and `--remote-secret-key-env <VAR>`. Names are what those two flags take, never the credentials themselves: a pasted key is refused before anything is read or written. The user exports those variables in their own shell, and the config records the reference `env:<VAR>` — so the values never pass through you, a command line, or a log.

A variable the user has not exported yet is not a reason to stop the run: the config block is still written, `credentials` reports that variable as `not_set` under the name you gave, and the option is dropped with a warning naming it, so the connection fails afterwards and the run ends `3` with `unread` naming the destination. The backend's own message is in that destination's entry in `doctor`, which is where to look next. Tell the user to export it, and run `setup` again (or `dest-init` on its own): the destination block does not have to change. A destination already declared in the config is adopted and verified as it stands, never rewritten, so a second run needs no parameters.

**A plain variable is the wrong form for a destination a timer pushes to.** A timer does not run in the user's shell, so an `env:NAME` that works in their terminal is dropped in the hourly run, and the only sign of it is in that job's log. Before scheduling a push, the credential in the config should be one of the forms that needs no shell — `file:PATH`, `env-file:PATH:NAME`, or, on macOS, `keychain:ACCOUNT` — written by the user in `~/.config/chat-stasher/config.toml` in place of `env:NAME`. `doctor`'s `destinations` entry is where you confirm it reads.

Then read `destination` in the object, field by field:

| Field | What it tells you |
|---|---|
| `kind` | the step's own verdict: `skipped` (no destination was named), `not_configured`, `not_attempted`, `reachable`, `unread`, `failed` |
| `consequence` | what it means that no destination was named — this is the sentence to pass on, not a reassuring paraphrase |
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

`install-native-host` prints every file it wrote, and `--uninstall` removes exactly those. It also records the stage in the config as `[native_host] stage`, which is what the wizard and `doctor` read afterwards. This half is per **machine**, not per browser profile: one run registers every installed browser, all of them pointing at the same stage, so never tell the user to run it again for another profile. `--browser <name>` narrows it to one browser, in both directions, and is how a browser that was skipped (not installed, said in the output) is registered anyway.

Its exit codes are its own: `0` at least one manifest is in place, `3` nothing was written because no known browser directory was found, `2` a usage error, `1` an action failed. The `3` is a real "nothing was registered" and not a failure — read the line that names the missing directory rather than retrying.

**Human step:** the user downloads `chat-stasher-extension-X.Y.Z.zip` from the [latest release](https://github.com/dimpurr/chat-stasher/releases/latest), unzips it into a folder they keep, and loads it in `chrome://extensions` → Developer mode → **Load unpacked**. Then they reload any chat tabs that were already open, because a tab that existed before the extension was installed is not captured until it is reloaded. You cannot click these for them.

**Ask which profiles, then repeat it in each one.** An extension is installed into a single browser profile, and `chrome://extensions` shows only that profile's extensions, so a user who chats in three profiles needs three `Load unpacked` passes, one in each profile. Ask before presenting the step: "which browser profiles do you chat in?" A profile they skip captures nothing, and the popup must be checked in each profile separately, because each copy has to reach the host on its own. Also tell them the host half is not repeated, and that `--uninstall` covers every browser it registered at once, so it is not the way to drop one profile — that is done in `chrome://extensions`, in that profile.

**Each profile is a separate install with its own identity.** A copy mints a random id into that profile's own extension storage and carries a label the user gives it (the popup offers **Name this profile** until they do). `chat-stasher ui --view extensions` lists them as *N installs on M machines*; report that count as the page states it and **never** add installs together, because two profiles signed in to the same account may capture the same conversation. Never present an install that has gone quiet as healthy or as zero — it is "last reported at T". If a profile reports that it **shares its identity with another copy**, that is a conflict the user repairs in that profile's own popup (**Give this profile a new identity**); do not repair it for them, and do not treat it as a capture problem.

## Everyday use

| The user wants to… | Command |
|---|---|
| Know if backups are working | `chat-stasher status` (add `--destination <name>` to spot a machine on an older chat-stasher) |
| See what is in the archive, at a glance | `chat-stasher overview --destination <name>` — a machine × tool matrix (or `--repo <path>`); add `--json` for the per-session array and `--json --summary` for totals per machine and per source |
| Find sessions from a day or range | `chat-stasher search --destination <name> --day YYYY-MM-DD` (or `--since` / `--until`; with no destination declared, `--repo <path>`) |
| Find sessions by their text | `chat-stasher index build …` first, then `chat-stasher search --destination <name> --text <query>` |
| Get sessions out as files | `chat-stasher export --destination <name> … --out <empty dir>`; add `--dry-run` first to see the cost |
| Prove one session is intact | `chat-stasher read --session <id> …` (prints shard names and SHA-256 values, not the conversation) |
| Browse, and read one conversation | `chat-stasher ui` (with no flag when the config leaves the choice unambiguous); prints a local URL with a one-time token |
| Check archive integrity | `chat-stasher verify --destination <name> --level l1` |
| Check for identical duplicate shards | `chat-stasher repair-duplicates --destination <name> --json` — read-only dry run; it never removes archive data |
| Report the packs no index file names | `chat-stasher prune-orphans --destination <name>` — a read-only survey; `--apply` is refused, so never offer it as a way to delete |

`search`, `export` and `ui` exit `0` when answered completely, `3` when they could not finish, and `2` on a usage error. `search` and `export` also exit `1` when everything was read and nothing matched; **`ui` never exits `1`**. With `3`, a result of "0 found" is **not** proof of absence, so say so: `search` says this itself, and it exits `3` rather than `1` whenever a session exists that it could not place in time.

A `--text` search has states a metadata search does not, and every one of them is a `3` rather than a zero: a query shorter than 3 characters cannot be evaluated at all (`query_state` is `too_short` and `matched` is `null`, so read the tool's own line about the minimum), and so are a missing index, an index behind the archive, an index holding sessions it cannot read, and a truncated index. `--scan` reads the conversations instead of the index and is the fallback the tool names. `prune-orphans` exits `0` for a survey that read every pack, `1` when an index names a pack the backend does not list, and `3` when the survey did not finish — or whenever `--apply` was asked for, which is a refusal rather than a partial delete. `overview` exits `1` when the whole repository was read and holds no activity index at all, and `3` when the repository could not be read in full.

`export` and `ui` are the two commands that reach conversation content: `export` writes it to files under `--out`, and `ui` serves a one-line label per session and fetches a session's full text only after the user opens it. `search` reads metadata only — except with `--text`, which reads the local index the archive was copied into by `chat-stasher index build`, and `--scan`, which reads the conversations themselves. Use them when the user asks to see or extract conversations, and do not paste large amounts of content back into the chat unless asked.

For duplicate-shard reports, run `repair-duplicates` on each affected destination. It reports per-machine duplicate session and byte counts and exits `3` if the archive could not be read completely. Fixed readers already collapse same-session shards with identical hashes, so the user can upgrade before any storage decision. Do not delete or rewrite shards or snapshots: physical removal needs a separate decision from Dim.
