# The setup wizard

<!-- RELEASE GATE: written against main. In 0.5.0-rc.2 the destination (step 4) and scheduler (step 5) steps are descriptive stubs; they become real in the next release. Ship this page with that release. -->

`chat-stasher setup` walks you through a first run in one command. It does the same things as the steps in [start.md](start.md), in the same order, and checks each one before it moves on:

1. it scans this machine for AI tools, read-only;
2. it makes your first encrypted archive on this disk, and proves it can read a session back out of it;
3. it shows you the master key and asks you to confirm you have a copy somewhere else;
4. optionally, it sets up an off-site copy (R2 or another S3-compatible service, or SFTP), **which has a key file of its own** — the wizard shows you that one too and asks for the same confirmation;
5. optionally, it installs the hourly timer.

At the end it also reports whether the browser host is registered on this machine. It only looks: registering the host is still `install-native-host`, as in [install.md](install.md#the-browser-extension).

You can stop after step 3. You will have a working, encrypted archive on this disk, and you can add the rest later by running `setup` again.

## Before you start

Install the CLI ([install.md](install.md)) and make a folder for the **stage**, where new sessions wait before they are archived. Put it somewhere you will not delete:

```sh
mkdir -p ~/stash/chat-stasher/stage
```

If you plan to add an off-site copy, have its details ready. The wizard asks for the same values [destinations.md](destinations.md) describes. For R2 or S3, put the access key id and secret access key in **environment variables** in the shell you run `setup` from. The wizard asks for the variable **names**, never the values.

## Run it

```sh
chat-stasher setup
```

The wizard asks one question at a time. Press Enter to accept the value in brackets.

### 1. The stage

```
Stage directory [~/stash/chat-stasher/stage]:
```

Accept the default, or type the folder you made. You can also give it up front with `--stage <folder>`.

### 2. The first archive

The wizard runs the archive pass twice and reads the result back:

- the **first** pass collects your sessions and creates the encrypted archive at `~/.local/share/chat-stasher/repo`, with its master key;
- the **second** pass should find nothing new. That proves a repeat run adds nothing when nothing changed, which is what the hourly timer will do most of the time;
- then it **reads one session back** out of the archive, to prove the archive can actually be opened.

It prints a line for each of these. If there is nothing on this machine to archive yet, it says so, and creates no archive and no key. The first run that finds something will create them.

### 3. The master key

The wizard prints the path of the master key and explains why it matters.

> [!WARNING]
> The master key is the **only** way to read your archive. If you lose it, nobody can read the archive again, including you. Copy it somewhere that is not this disk now: a password manager, another disk, or a backup you already keep.

Then it asks you to type **I saved it elsewhere**. This is a promise you make, not a check: chat-stasher cannot see your password manager, and it says so. If you skip it, the wizard still finishes, but it reports this step as not done.

To use the archive on another machine later, put your copy of the key back at the same path there.

> [!IMPORTANT]
> **There is one key per archive copy, not one key for the machine.** The file this step names is the key to the archive **on this disk**. If you set up an off-site copy in step 4, that copy gets a key file of its own — `~/.local/share/chat-stasher/masterkey-<destination>.json`, for example `masterkey-r2.json` — and a second machine reads it with **that** file. A backup that copies only the key named here cannot read the off-site copy: it fails with `cannot read masterkey file … (lost key?)` and exit `3`, which is the tool saying it could not read anything, not that the archive is empty. Step 4 names the second file when it creates it; back up both.

### 4. An off-site copy (optional)

The wizard asks for a destination name. Leave it blank to skip. The wizard then tells you plainly that the archive and its key are on one disk, and that losing this machine loses both. You can come back to this step any time.

To add one, type a name, for example `r2`. If that name is already declared in your config, the wizard uses it as it stands and does not rewrite it. Otherwise it asks which kind to set up (it suggests S3, which covers R2) and then asks for that kind's values:

| Kind | It asks for |
|---|---|
| `s3` (R2 and other S3-compatible services) | endpoint, bucket, region (`auto` for R2), an optional folder prefix, and the **names** of the two environment variables that hold your access key id and secret |
| `sftp` | `ssh://<host>:<port>`, the ssh user, an optional private key (leave it out to use your ssh agent), and an optional folder |

It writes one `[destinations.<name>]` block into your config, connects to the destination read-only, and then runs `dest-init` to seed it with this machine's history. The credentials are written as `env:NAME` references, so the secret itself never lands in the config file. [destinations.md](destinations.md#keeping-the-secret-out-of-the-config-file-env) explains what that means for timers.

**The destination gets its own key.** `dest-init` creates `~/.local/share/chat-stasher/masterkey-<name>.json` — for the example above, `masterkey-r2.json` — and the wizard prints that path and asks you to confirm a copy of it, exactly as step 3 does for the local key. This is not a spare copy of the local key: it is a different file, and it is the only thing that opens the off-site copy. A second machine restored from your backups needs **this** file to read the destination; the local key from step 3 is not used at all by a machine that only reads a destination. Back up every path the wizard prints.

**SFTP, first connection.** If the server is one you have never connected to, the wizard stops, prints the fingerprints the server presented, and writes nothing. Compare them with the ones your provider publishes, in the provider's own documentation, not over this connection. If they match, type the sentence the wizard asks for (**I compared the fingerprint with my provider's published one**), or re-run with `--trust-host`. If they do not match, stop. [destinations.md → SFTP](destinations.md#2-trust-the-server-once-yourself) has the details.

### 5. The hourly timer (optional)

```
Install the scheduler now? [y/N]:
```

Answer `y` to install the timer, one per declared destination. The wizard then tells you when the timer will next run, if the system scheduler can say. On macOS an interval timer has no fixed next time, and the wizard says that instead of guessing. [schedule.md](schedule.md) covers the timer in full.

On Windows there is no question: this build has no scheduler integration there, so the wizard skips the step and points at the manual Task Scheduler steps in [schedule.md](schedule.md#windows-a-task-in-task-scheduler). A `setup --install-schedule` run on Windows reports the scheduler step as `not_attempted`, with the same pointer, and the run is `INCOMPLETE` — the archive is left without a timer, which is a fact and not a failure of the archive itself.

### The summary

The wizard ends with one summary: the local archive, the destination, the timer and the browser host. Anything that did not finish is named on its own line:

| You see | It means |
|---|---|
| `INCOMPLETE` | A step ran and did not finish, for example the second pass or the read-back. The local archive is not proven readable yet. |
| `UNREAD` | Something could not be read, usually the destination. Nothing about it is proven, so do **not** read it as "the destination is empty". |
| `the masterkey declaration was not made` | You skipped step 3, or step 4's destination key. Each archive copy has its own key, and the declaration covers all of them. |

## Run it again at any time

`setup` is safe to repeat. A second run finds the archive already there, creates nothing new locally, and re-checks everything. Use it to:

- add the off-site copy you skipped: `chat-stasher setup --destination <name>`;
- install the timer later: `chat-stasher setup --install-schedule`;
- remove the timer: `chat-stasher setup --uninstall-schedule`.

## For scripts and agents

When `setup` is not attached to a terminal, it never prompts. It does the same work and prints **one JSON object** on stdout. Pass every answer as a flag:

```sh
chat-stasher setup --stage ~/stash/chat-stasher/stage \
  --masterkey-saved-elsewhere \
  --destination r2 --remote s3 \
  --remote-endpoint https://<account-id>.r2.cloudflarestorage.com \
  --remote-bucket <bucket> \
  --remote-access-key-id-env CHAT_STASHER_R2_ACCESS_KEY_ID \
  --remote-secret-key-env CHAT_STASHER_R2_SECRET_ACCESS_KEY \
  --install-schedule
```

`--json` asks for the same object from an interactive terminal.

What to read in the object:

| Field | What it tells you |
|---|---|
| `missing_parameters` | Flags that were needed and not given, for example `["stage"]` or `["masterkey_saved_elsewhere"]`. Supply them and run again. On its own, the masterkey declaration is the one case that wrote anything first — see below. |
| `incomplete` | Steps that ran and did not finish. |
| `unread` | Parts that could not be read. Their absence proves nothing. |
| `steps` | One entry per step: `stage`, `local_save`, `masterkey`, `destination`, `schedule`, `native_host`. |
| `chain` | The three proofs from step 2: `init`, `noop`, `readback`. |
| `masterkey.keys` | Every key file this run asks you to back up, one entry per archive copy. See below. |
| `destination` | `config`, `reach`, `trust`, `dest_init`, `credentials`, and `recommended_remote_kind`. `credentials` names each variable and whether it is set, never its value. `key_file` and `key_declared` name the destination's own key and whether you have declared a copy of it, once `dest-init` has created it. |
| `exit_code` | The same value the process exits with. |

Exit codes:

| Code | Meaning |
|---|---|
| `0` | Everything it was asked to do finished. |
| `1` | A step did not finish (`incomplete` is not empty). |
| `2` | A required parameter is missing, or a flag is malformed — and the masterkey declaration is a required parameter the person, not the command line, can supply. Nothing was written, with the two exceptions below. |
| `3` | Something could not be read (`unread` is not empty), or the scan could not run. |

When several apply, `3` wins over `1`, and `1` wins over `2`.

**The one refusal that writes something.** A person cannot confirm they have copied a file that does not exist yet. So when a run with no terminal owes exactly one thing — `missing_parameters` is `["masterkey_saved_elsewhere"]`, and nothing else is missing — it creates every key it will ask about, reports them in `masterkey.keys` (`masterkey.path` is the `local` entry's path), and *then* exits `2`. The local repository and its master key come first. When the run names a destination, step 4 runs here too: `dest-init` is what creates a destination's own key, so without it the key would not exist to be copied, and the destination key joins `masterkey.keys`. Read the locations to copy from `masterkey.keys` rather than assuming the usual ones, because your config can put a key somewhere else. Tell the user to copy every file in that array somewhere off this disk, wait for their answer, and run the same command again with `--masterkey-saved-elsewhere`; it continues from the keys that are already there and records them all. Because the first run already created every key, **a first setup with an off-site copy takes two runs, not three** — which is the practical reason this run reaches step 4 rather than stopping after the local key. What did not happen: no local archive pass and no timer, so the local repository holds no snapshot on that run. If anything else is missing as well, this does not happen, and that run writes nothing at all.

**A destination key the run creates cannot be declared by a flag given before it existed.** The same rule, one step later in the flow. `--masterkey-saved-elsewhere` is a statement about key files the user has already seen, and a destination's key is created by `dest-init`, *during* the run — so on that run the declaration cannot be made: a first run with an off-site copy stops with `exit_code` `2` *even though the flag was on the command line*, with `missing_parameters` `["masterkey_saved_elsewhere"]` again and the `destination` entry of `masterkey.keys` carrying `declared: false` and the path of the file that was just created. This is also why a first setup with an off-site copy takes two runs — one to create the keys, one to declare them all — whether or not the flag is given on the first of them. Nothing is wrong with the command line and there is nothing to re-supply: the user copies that file, and the same command run again records the declaration, because the file is on the disk by then. Hand over every path in `masterkey.keys` whose `declared` is `false` — that is the entry still owed.

**Every key, not the first one.** A completed run with a destination names **two** key files, and they open different copies:

```json
"masterkey": {
  "path": "/home/you/.local/share/chat-stasher/masterkey.json",
  "declaration": "declared",
  "declaration_is_verified": false,
  "keys": [
    {"scope": "local", "path": "…/masterkey.json", "declared": true, "declaration_is_verified": false},
    {"scope": "destination", "name": "r2", "path": "…/masterkey-r2.json", "declared": true, "declaration_is_verified": false}
  ]
}
```

`masterkey.path` is the `local` entry's path, kept for callers written before `keys` existed. Hand the user **every** path in `keys`, and treat `declared` as a report of what they said, never as a check: `declaration_is_verified` is `false` for the same reason on every entry. A second machine needs the `destination` entry's file to read that destination — the `local` one will not do it. An entry whose `declared` is `false` is one nobody has confirmed: the run either asked and was not answered, or created that file itself and stopped — copy it and run again, as above.

Three rules the wizard keeps, and your script should too:

- **No flag takes a secret.** `--remote-access-key-id-env` and `--remote-secret-key-env` take the **name** of a variable. A value that looks like a secret instead of a name is refused, and not echoed back.
- **`--masterkey-saved-elsewhere` is a statement by the person, not by you.** Pass it only after the user has said they copied the key.
- **`--trust-host` is the same.** Pass it only after the user compared the fingerprints and said they match.

`chat-stasher setup --help` lists every flag.
