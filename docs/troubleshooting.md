# Troubleshooting

<!-- RELEASE GATE: the index/search sections and the file:/env-file:/keychain: advice are merged after 0.5.0-rc.2. -->

Start here when something looks wrong. Each section begins with what you see, then says what it means and what to do.

## First, two read-only checks

Both commands change nothing. Run them before anything else:

```sh
chat-stasher doctor
chat-stasher status
```

- **`doctor`** looks at this machine: which AI tools are installed and whether any of them is deleting old sessions, each declared destination (reached, not reached with the reason, or not configured), the browser host registration and its stage, and how much the local caches hold.
- **`status`** answers "is the hourly archive working?" on its first line, then counts what the scanner finds.

Most answers below start from one of these two outputs.

> [!TIP]
> **Unknown is not zero.** When chat-stasher cannot see something, it says *unknown* and gives the reason. It never reports an unreadable folder as "0 sessions", or an unreachable destination as "empty". When you see *unknown*, fix the reason it names. Don't read it as "nothing there".

## The timer

### `status` says `No run has ever been recorded`

No archive pass has run on this machine since the timer was set up, or the timer was never installed.

1. If you just installed it, wait one interval (an hour by default) and check again.
2. Otherwise install it: [schedule.md](schedule.md#install-the-timer).
3. To rule out the pass itself, run one by hand: `chat-stasher run-once --stage <stage>` (add `--destination <name>` if your config declares destinations). If that works, the problem is the timer, not the archive.

### `status` says `No run for … (threshold …)`

Passes used to run and have stopped. Common causes:

| Cause | Fix |
|---|---|
| You moved, reinstalled or deleted the binary the timer points at. | Run `chat-stasher schedule install --stage <stage>` again. It rewrites the timer with the current path. |
| You declared your first destination, and the old timer passes no `--destination`. | Run `schedule install` again (one timer per destination), then remove the old one: [schedule.md → Remove the timer](schedule.md#remove-the-timer). |
| The machine was asleep or off for a long stretch. | Nothing to fix. The next pass catches up with everything that changed. |

### `status` says `Last run failed: the <step> step errored …`

The timer fires, but the pass fails. The step name says where. Read the log for the full error:

- macOS: `~/Library/Logs/chat-stasher/run-once.err.log`
- Linux: `journalctl --user -u 'chat-stasher-run-once*'`

Then run the same pass by hand, so you see the error directly: `chat-stasher run-once --stage <stage> --destination <name>`.

### It works from the terminal but fails under the timer

The difference is almost always **environment variables**. A timer does not see what you exported in your shell profile. If your R2 or S3 credentials are written as `env:NAME`, the timer's pass has no credentials and cannot reach the bucket.

Write them as `file:`, `env-file:` or (on macOS) `keychain:` references instead. Those are read without a shell: [schedule.md → Credentials a timer can reach](schedule.md#credentials-a-timer-can-reach).

### `schedule` asks you to pass `--destination`

You ran `schedule` (the render form) with destinations declared and named none. Either name one with `--destination`, or use `schedule install`, which sets up one timer per declared destination.

### `schedule` refuses a binary under `target/`

A timer records the binary's path, and a build folder is emptied by `cargo clean`. Copy the binary somewhere permanent (`install -m 755 target/release/chat-stasher ~/.local/bin/`) and run `schedule install` from there, or pass `--binary <installed path>`.

## Destinations

### A command exits `3`

`3` always means **"did not finish reading"**. It is never "found nothing" (that is `1`). Whatever the command printed about what it could read is real. Everything it could not read is unproven. Look for the reason in the last lines of the output, often under `Caused by:`.

### `pass --destination <name>` / `there is no default`

Once your config declares any `[destinations.<name>]`, commands that read or write an archive must say which one. This is deliberate: you always know which copy you are touching. Add `--destination <name>`, or `--repo <path>` for an undeclared archive.

The dashboard (`ui`) is the one exception. It opens your only declared destination without being told. With several, it opens the one named by `destination = "<name>"` under `[native_host]` in your config, or lists them and exits `2`.

### R2 or S3: `failed to load signing credential`, `region is missing`, and similar

See [destinations.md → Pitfalls](destinations.md#pitfalls). The two most common:

- `region = "auto"` is missing from the options;
- the credentials did not reach the command: with `env:NAME`, the variable is not set in this process.

### SFTP: the first connection stops and prints fingerprints

This is on purpose. chat-stasher never trusts a server it has not met before. Compare the fingerprints with the ones your provider publishes, then re-run with `--trust-host` if they match: [destinations.md → Trust the server](destinations.md#2-trust-the-server-once-yourself).

### SFTP: the server's key has changed

chat-stasher refuses, and no flag gets past it. A changed key can mean someone is impersonating your server. Find out why it changed (for example, from your provider) before you edit `~/.ssh/known_hosts`.

## Web chats and the browser extension

### A chat is not being captured

1. **Reload the tab.** A tab that was already open when the extension was installed or updated is not captured until you reload it. The popup lists such tabs.
2. **Check the profile.** The extension belongs to one browser profile. A copy in your Personal profile captures nothing in your Work profile. Load it in every profile you chat in.
3. **Check the platform.** The popup and [support.md](support.md) list which chat sites each release covers.

### The popup says it cannot reach the host

The host is the `chat-stasher` binary, which your browser starts when the extension delivers. Register it (once per machine, for every browser and profile):

```sh
chat-stasher install-native-host --stage ~/stash/chat-stasher/stage
```

Use **the same stage** your CLI archives from. Then reload the extension on `chrome://extensions`.

If the binary moved since you registered it, run the same command again: the registration records the binary's path.

### The popup reports a stage or config problem

The host never creates a stage and never guesses one. `chat-stasher doctor` has a section for the browser host that says which of these applies:

| `doctor` says | Fix |
|---|---|
| no `[native_host] stage` in the config | Run `install-native-host --stage <stage>`. It records the stage in your config. |
| the stage is configured but not on disk | Recreate the folder, or run `install-native-host --stage <the right folder>`. |
| the stage is configured but is not a directory | Point it at a folder: `install-native-host --stage <folder>`. |

Captures that could not be delivered stay in the extension's queue in that profile. They are not lost while the extension stays installed, but uninstalling the extension deletes them: see [install.md → Before you remove the host](install.md#before-you-remove-the-host).

### **Open dashboard** in the popup does not open anything

The host opens the dashboard for **one** destination, and it only uses the one your config names:

```toml
[native_host]
destination = "<name>"
```

The name must be one of your declared `[destinations.<name>]`. There is no fallback, even when only one is declared. Until you add this line, open the dashboard from a terminal with `chat-stasher ui` instead.

## The dashboard and search

### `ui` lists destinations and exits `2`

Your config declares several destinations and does not say which one to open. Name one (`--destination r2`), several (`--destination r2,laptop`), or `all`. To make one the default, set `destination = "<name>"` under `[native_host]`.

### The dashboard closed by itself

It stops after 5 minutes with no requests. Run `chat-stasher ui` again (each launch has a new URL). `--idle-timeout 0` keeps it open until you press Ctrl+C.

### The search page says there is no index, or that it covers only part of the archive

Search inside conversations uses a **local full-text index**, which you build yourself:

```sh
chat-stasher index build --destination <name>
```

It only covers what the archive held when you last built it. Run it again to include newer sessions. `chat-stasher index check --destination <name>` reports what the index holds without contacting the archive.

Using a text search to decide which conversations to export is [a task of its own](use-cases/skill-from-history.md), with the reading of the coverage counts written out.

A session the index has not read cannot be searched, and the page says so instead of reporting "no match".

### A one- or two-character search finds nothing

The index matches runs of three or more characters. For a shorter query, use `chat-stasher search --destination <name> --scan --text <query>` from the terminal. It reads the conversations themselves, so it is slower and downloads what it reads.

### `search` or `export` exits `3` with "0 matched"

Some sessions could not be read, or could not be placed in time (their conversation dates are unknown). While any remain, "0 matched" is not proof there is nothing, so the exit code is `3` instead of `1`. The output lists those sessions separately, with the reason.

## Keys

### I lost the master key

The archive it opens cannot be read again. There is no recovery, reset or backdoor, and nobody can help. If you have another destination with its own key, that copy is still readable. Keep a copy of every key file somewhere other than the disk it protects.

### I want to read my archive on another machine

Copy the destination's key file to the same path on the other machine (or set `key_file` in that machine's config), declare the same destination, and use `ui`, `search` or `export` as usual.

If the machine the conversations came from is gone for good, that is a whole task rather than a one-liner, and it has its own page: [Recover a lost machine's conversations](use-cases/lost-machine.md). It covers finding the machine's partition, what an exit `3` from a search means, and how to read one conversation back with a checksum.

## Still stuck?

Open an issue on GitHub. Include the output of `chat-stasher --version`, and a redacted `doctor` or `status`. Remove conversation text, account names, hostnames and paths before you paste anything. To report a security problem, follow [SECURITY.md](../SECURITY.md) instead of opening a public issue.
