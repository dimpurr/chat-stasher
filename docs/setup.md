# The setup wizard

<!-- RELEASE GATE: written against main. In 0.5.0-rc.2 the destination (step 4) and scheduler (step 5) steps are descriptive stubs; they become real in the next release. Ship this page with that release. -->

`chat-stasher setup` walks you through a first run in one command. It does the same things as the steps in [start.md](start.md), in the same order, and checks each one before it moves on:

1. it scans this machine for AI tools, read-only;
2. it makes your first encrypted archive on this disk, and proves it can read a session back out of it;
3. it shows you the master key and asks you to confirm you have a copy somewhere else;
4. optionally, it sets up an off-site copy (R2 or another S3-compatible service, or SFTP);
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

### 4. An off-site copy (optional)

The wizard asks for a destination name. Leave it blank to skip. The wizard then tells you plainly that the archive and its key are on one disk, and that losing this machine loses both. You can come back to this step any time.

To add one, type a name, for example `r2`. If that name is already declared in your config, the wizard uses it as it stands and does not rewrite it. Otherwise it asks which kind to set up (it suggests S3, which covers R2) and then asks for that kind's values:

| Kind | It asks for |
|---|---|
| `s3` (R2 and other S3-compatible services) | endpoint, bucket, region (`auto` for R2), an optional folder prefix, and the **names** of the two environment variables that hold your access key id and secret |
| `sftp` | `ssh://<host>:<port>`, the ssh user, an optional private key (leave it out to use your ssh agent), and an optional folder |

It writes one `[destinations.<name>]` block into your config, connects to the destination read-only, and then runs `dest-init` to seed it with this machine's history. The credentials are written as `env:NAME` references, so the secret itself never lands in the config file. [destinations.md](destinations.md#keeping-the-secret-out-of-the-config-file-env) explains what that means for timers.

**SFTP, first connection.** If the server is one you have never connected to, the wizard stops, prints the fingerprints the server presented, and writes nothing. Compare them with the ones your provider publishes, in the provider's own documentation, not over this connection. If they match, type the sentence the wizard asks for (**I compared the fingerprint with my provider's published one**), or re-run with `--trust-host`. If they do not match, stop. [destinations.md → SFTP](destinations.md#2-trust-the-server-once-yourself) has the details.

### 5. The hourly timer (optional)

```
Install the scheduler now? [y/N]:
```

Answer `y` to install the timer, one per declared destination. The wizard then tells you when the timer will next run, if the system scheduler can say. On macOS an interval timer has no fixed next time, and the wizard says that instead of guessing. [schedule.md](schedule.md) covers the timer in full.

### The summary

The wizard ends with one summary: the local archive, the destination, the timer and the browser host. Anything that did not finish is named on its own line:

| You see | It means |
|---|---|
| `INCOMPLETE` | A step ran and did not finish, for example the second pass or the read-back. The local archive is not proven readable yet. |
| `UNREAD` | Something could not be read, usually the destination. Nothing about it is proven, so do **not** read it as "the destination is empty". |
| `the masterkey declaration was not made` | You skipped step 3. |

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
| `missing_parameters` | Flags that were needed and not given, for example `["stage"]` or `["masterkey_saved_elsewhere"]`. Supply them and run again. |
| `incomplete` | Steps that ran and did not finish. |
| `unread` | Parts that could not be read. Their absence proves nothing. |
| `steps` | One entry per step: `stage`, `local_save`, `masterkey`, `destination`, `schedule`, `native_host`. |
| `chain` | The three proofs from step 2: `init`, `noop`, `readback`. |
| `destination` | `config`, `reach`, `trust`, `dest_init`, `credentials`, and `recommended_remote_kind`. `credentials` names each variable and whether it is set, never its value. |
| `exit_code` | The same value the process exits with. |

Exit codes:

| Code | Meaning |
|---|---|
| `0` | Everything it was asked to do finished. |
| `1` | A step did not finish (`incomplete` is not empty). |
| `2` | A required parameter is missing, or a flag is malformed. Nothing was written. |
| `3` | Something could not be read (`unread` is not empty), or the scan could not run. |

When several apply, `3` wins over `1`, and `1` wins over `2`.

Three rules the wizard keeps, and your script should too:

- **No flag takes a secret.** `--remote-access-key-id-env` and `--remote-secret-key-env` take the **name** of a variable. A value that looks like a secret instead of a name is refused, and not echoed back.
- **`--masterkey-saved-elsewhere` is a statement by the person, not by you.** Pass it only after the user has said they copied the key.
- **`--trust-host` is the same.** Pass it only after the user compared the fingerprints and said they match.

`chat-stasher setup --help` lists every flag.
