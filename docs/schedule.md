# Hourly archiving

<!-- RELEASE GATE: "Credentials a timer can reach" (file:/env-file:/keychain:) is merged after 0.5.0-rc.2. `schedule install --format systemd` must be confirmed in the release that ships this page (rc.2's notes name launchd only). -->

`chat-stasher run-once` does one pass and exits. To keep your archive current without thinking about it, let your system's own scheduler run it every hour: **launchd** on macOS, a **systemd user timer** on Linux. chat-stasher never runs a background process of its own.

This page installs the timer, checks it, and covers more than one destination, credentials, logs and removal.

> [!NOTE]
> If you used [`chat-stasher setup`](setup.md) and answered yes to the scheduler question, the timer is already installed. Skip to [Check that it runs](#check-that-it-runs).

## Before you start

You need:

- a stage folder that `run-once` has already archived from once ([start.md](start.md));
- the CLI installed somewhere permanent, such as `~/.local/bin/chat-stasher`. A timer records the binary's path, so a copy inside a build folder (`target/`) is refused: it would stop working after the next `cargo clean`.

## Install the timer

**macOS:**

```sh
chat-stasher schedule install --stage ~/stash/chat-stasher/stage
```

**Linux:**

```sh
chat-stasher schedule install --format systemd --stage ~/stash/chat-stasher/stage
```

That is the whole step. `schedule install`:

1. renders the timer for this machine, with the path of the installed binary and your stage;
2. writes it where the scheduler looks: `~/Library/LaunchAgents/` on macOS, `~/.config/systemd/user/` on Linux;
3. loads it: `launchctl bootstrap` on macOS, `systemctl --user daemon-reload` and `systemctl --user enable --now` on Linux.

It ends with one line, for example `[schedule] installed agents: 1 unchanged: 0`.

Running it again is safe. A timer that is already installed with the same content is left alone and counted as `unchanged`. One whose content changed, for example because you moved the binary, is rewritten and reloaded.

> [!NOTE]
> **A failed install puts the machine back the way it was.** The scheduler is talked to *after* the timer files are written — that is the only order a manager can accept them in — so a manager that refuses fails the install on a machine whose disk already holds part of a schedule nothing will run. Both formats then roll themselves back: the files this run created are removed, the ones it replaced get their previous content back, the timers it had got armed are stopped, and on macOS the agents that were loaded before it are loaded again. On a machine that had no timer, `status` reports `not_installed` — never an installed timer that is armed nowhere.

> [!NOTE]
> On Linux, pass `--format systemd` every time, including for `schedule uninstall`. Without it the command renders a launchd timer.

### Which binary the timer runs

Without `--binary`, the timer uses the `chat-stasher` you are running, as long as it is not inside a `target/` folder. If you are running a build from `target/`, it falls back to `~/.local/bin/chat-stasher`, then `/opt/homebrew/bin/chat-stasher`, then `/usr/local/bin/chat-stasher`. To be explicit:

```sh
chat-stasher schedule install --stage ~/stash/chat-stasher/stage --binary ~/.local/bin/chat-stasher
```

If you later move or reinstall the binary somewhere else, run `schedule install` again so the timer points at the new copy.

## With destinations declared

Once your config declares a destination ([destinations.md](destinations.md)), every archive pass has to name one. The timer follows the same rule, and handles it for you:

| You run | You get |
|---|---|
| `schedule install --stage …` | **One timer per declared destination.** Each runs `run-once --destination <name>`. |
| `schedule install --stage … --destination r2` | A timer for `r2` only. Repeat `--destination` to pick several. |
| `schedule install --stage …` with no destination declared | One timer, pushing to the local archive, as in [start.md](start.md). |

Each destination's timer has its own name, so they never replace each other:

| | Timer name |
|---|---|
| macOS | `com.chat-stasher.run-once.<destination>` |
| Linux | `chat-stasher-run-once-<destination>.service` and `.timer` |

A destination name with characters that are not letters, digits, `-`, `_` or `.` gets a short suffix, so two names cannot collide.

> [!IMPORTANT]
> **Coming from [start.md](start.md)?** The timer you installed there is named without a destination (`com.chat-stasher.run-once`) and passes no `--destination`. Once you declare a destination, that timer's runs fail, because a pass has to name one. Run `schedule install` again so each destination gets its own timer, then remove the old one as described in [Remove the timer](#remove-the-timer).

## Credentials a timer can reach

A timer is started by launchd or systemd, not by your shell. It does **not** see variables you exported in your shell profile. That matters for R2 and S3, whose credentials you may have written as `env:NAME` ([destinations.md](destinations.md#keeping-the-secret-out-of-the-config-file-env)): under a timer, the variable is not there, the option is dropped, and the run cannot reach the bucket.

A credential written in one of these forms is resolved without a shell, so it works the same from a terminal and from a timer:

| Written as | The secret is read from |
|---|---|
| `file:/path/to/secret` | The whole file, with one trailing newline removed. |
| `env-file:/path/to/file.env:NAME` | The `NAME=` line of a dotenv-style file. `#` comments, an `export ` prefix and quotes around the value are accepted. |
| `keychain:ACCOUNT` or `keychain:SERVICE:ACCOUNT` | macOS only: a generic password in your login keychain. The service defaults to `chat-stasher`. |

For example:

```toml
[destinations.r2.options]
access_key_id = "file:~/.config/chat-stasher/r2-access-key-id"
secret_access_key = "keychain:r2"
```

Unlike `env:`, these forms **fail closed**: if the file, the line or the keychain item cannot be read, the config is refused with an error naming the option and the reference, never the secret. So a broken reference shows up the first time you run any command, not as a silent failure at 3 a.m.

Make a secret file readable only by you (`chmod 600`). To add a keychain item:

```sh
security add-generic-password -s chat-stasher -a r2 -w
```

`-w` with no value prompts for the secret, so it never appears in your shell history.

## Check that it runs

```sh
chat-stasher status
```

The first line is the verdict. It is read from the record the last pass left behind, so it tells you whether the timer is actually firing, not only whether it is installed:

| First line starts with | Meaning |
|---|---|
| `[run-once] Healthy: last run … ago` | The timer fires and the last pass succeeded. |
| `[run-once] No run has ever been recorded` | Nothing has run yet. Right after installing, wait for the first interval. |
| `[run-once] No run for … (threshold …)` | Passes used to run and have stopped. The timer is not firing. |
| `[run-once] Last run failed: the <step> step errored …` | The timer fires, but the pass fails at that step. |

"Stopped" means no pass for more than four intervals (at least one hour). `status` exits `0` only when the verdict is healthy, and `1` otherwise, so it works as a check in a script. Its report is on stderr: run it bare, not through a pipe, if you want the exit code.

`status --json` adds a `local` section: whether the timer units are installed, which ones, when the next run is due and why, and the sessions still staged and waiting to upload. `installed` needs two things on both platforms: every timer file present, *and* the scheduler confirming the job is loaded — systemd confirming each timer active on Linux, launchd finding each agent on macOS. Files the scheduler did not confirm are reported as `unconfirmed` — the reason sits beside them in `next_run_why` ("launchd has not loaded …", "systemd did not report a next run", or that the manager could not be asked at all). Those two are different answers and are worded as different answers: a plist written by hand with `--output` and never bootstrapped is the first, and a machine with no user session to reach is the second. The last pass is reported separately, in `run_state`: a file that is in place is not proof that the scheduler loaded it, so installed files and a healthy verdict are never one field.

### When it runs

| | Hourly archive (`run-once`) |
|---|---|
| Interval | `backup_interval_secs` in your config, default `3600` |
| Start | macOS: counted from when the timer was loaded, not on the hour. Linux: one interval after boot, then every interval after the last run. |
| Jitter | Up to 5 minutes of random delay per run, so several machines do not hit one destination at the same second. |

Change the interval in the config, then run `schedule install` again so the timer picks it up.

### Logs

| | Where |
|---|---|
| macOS | `~/Library/Logs/chat-stasher/run-once.log` and `run-once.err.log`. All destinations' timers write to these two files. Each is emptied at the start of a run once it passes 5 MB. |
| Linux | The journal: `journalctl --user -u chat-stasher-run-once-<destination>.service` |

A successful pass ends with `result: COMPLETED` (a new snapshot) or `result: NOOP` (nothing changed). Both are healthy.

## The weekly stage clean-up (optional)

The stage keeps a full local copy of everything it has sealed, so it only grows. A second, optional timer removes staged copies once **every** declared destination proves it holds those exact bytes:

```sh
chat-stasher reclaim-stage --stage ~/stash/chat-stasher/stage           # dry run: shows what it would remove
chat-stasher schedule install --unit reclaim-stage --stage ~/stash/chat-stasher/stage
```

It runs weekly, on Sunday at 03:17 local time on macOS, and around then on Linux (with up to 15 minutes of random delay). It is one timer for all destinations, so it takes no `--destination`. If any destination cannot be reached or does not hold a session, nothing is removed that week, and the reason is in its log (`reclaim-stage.log`).

Run the dry run by hand first, so you see what it would do.

## Remove the timer

```sh
chat-stasher schedule uninstall                 # macOS
chat-stasher schedule uninstall --format systemd  # Linux
```

This stops and removes the timer for every declared destination, or only for the ones you name with `--destination`. It prints `[schedule] uninstalled agents: <n>`. Add `--unit reclaim-stage` to remove the weekly clean-up. Your archive, stage and config are not touched.

A timer installed **before** you declared any destination is named without one, and `schedule uninstall` does not look for it once destinations exist. Remove it by hand:

```sh
launchctl bootout "gui/$(id -u)/com.chat-stasher.run-once"
rm ~/Library/LaunchAgents/com.chat-stasher.run-once.plist
```

On Linux: `systemctl --user disable --now chat-stasher-run-once.timer`, then delete `chat-stasher-run-once.service` and `.timer` from `~/.config/systemd/user/`.

## Doing it by hand

`schedule` without `install` renders the timer and changes nothing on your system. Use it to read the timer before you trust it, or to install it your own way:

```sh
chat-stasher schedule --stage ~/stash/chat-stasher/stage                  # print it
chat-stasher schedule --stage ~/stash/chat-stasher/stage \
  --output ~/Library/LaunchAgents/com.chat-stasher.run-once.plist         # write it
```

With `--output`, it writes the file and prints the exact command that loads it. Nothing is loaded until you run that command. With more than one destination, `--output` must be a folder.

### Windows: a task in Task Scheduler

Windows has no scheduler this build integrates with: every `chat-stasher schedule` action there refuses with exit 2 and writes nothing. Create the pass yourself instead, as a per-user task in Task Scheduler.

`schtasks.exe` does the whole job. Run this in `cmd.exe` or PowerShell, with the two paths replaced by yours — the binary's **full path** is required, because Task Scheduler does not search `PATH`:

```bat
schtasks /Create /TN "chat-stasher run-once" /SC HOURLY /TR "\"%USERPROFILE%\bin\chat-stasher.exe\" run-once --stage \"%USERPROFILE%\stash\chat-stasher\stage\" --destination r2"
```

The parts that are easy to get wrong:

- `/TN` is the task's name. `/Query`, `/Run` and `/Delete` address the task by it.
- `/TR` is the whole command, as one string. The inner quotes are backslash-escaped: `cmd.exe` passes `\"` through untouched and `schtasks` reads it as a real quote, which is what keeps a program path containing spaces from being split in two.
- `run-once` needs its own `--stage`, and `--destination <name>` as soon as your config declares a destination ([destinations.md](destinations.md)). A pass that names none is refused, and the task then looks scheduled while failing every hour. Drop `--destination` only while no destination is declared.
- The binary has to be a permanent copy, the same rule as [above](#which-binary-the-timer-runs): one inside a build folder stops working after the next `cargo clean`.
- Without `/RU` and `/RP`, the task runs **only while you are logged on**. This is the reason this page gives no wrapper around it: a task that looks scheduled and silently stops at every log-off is worse than one you know you set up by hand. If the pass must run while you are logged off, add `/RU <account> /RP` and expect Windows to ask for that account's password.

Check what Windows did with it:

```bat
schtasks /Query /TN "chat-stasher run-once" /V /FO LIST
```

Read `Next Run Time`, `Last Run Time` and `Last Result`. `Last Result` is the pass's own exit code, so `0` is a successful pass and anything else is the exit code [cli.md](cli.md#exit-codes) explains. To run it once now, rather than waiting for the hour:

```bat
schtasks /Run /TN "chat-stasher run-once"
```

Neither of those says the archive is current — a task Windows ran and a pass that succeeded are two different facts. That one is `status`:

```bat
chat-stasher status
```

It reads the record the pass writes, so it says whether the last pass ran and what it did, whether or not a task was involved.

Remove it:

```bat
schtasks /Delete /TN "chat-stasher run-once" /F
```

> [!NOTE]
> A build before this refusal could write `chat-stasher-run-once.service`, `chat-stasher-run-once.timer` and any `chat-stasher-run-once-<destination>.service` / `.timer` into your home folder's `.config\systemd\user` directory. They schedule nothing on Windows: delete those files, and leave whatever else you find in that directory alone — another tool may keep its own units there.

## See also

- [cli.md](cli.md#schedule) lists every `schedule` flag.
- [troubleshooting.md](troubleshooting.md#the-timer) covers a timer that stopped.
