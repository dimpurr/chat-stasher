# Get started

This page takes you from nothing to a working, automatic archive of your AI coding sessions in about ten minutes. You will:

1. install the CLI,
2. check what on your machine is at risk,
3. make your first encrypted archive on this disk,
4. put the key somewhere safe,
5. switch on hourly archiving,
6. confirm it works.

Adding an off-site copy and web chats comes at the end. Each is one more page.

> **In a hurry, or not sure?** `chat-stasher setup` walks through the same first run: it scans, makes the first archive, reads a session back out of it to prove the archive works, and shows you the key path. It does not install the timer, and the off-site copy is still [destinations.md](destinations.md).

Everything below was run on macOS. The commands are the same on Linux, with the prebuilt binaries and systemd timers described in [install.md](install.md); on Windows, download the `.exe` first, as that page says.

## 1. Install the CLI

```sh
curl -fsSL https://raw.githubusercontent.com/dimpurr/chat-stasher/main/scripts/install.sh | sh
```

The script installs one file, `~/.local/bin/chat-stasher`. If it says that folder is not on your `PATH`, add the line it prints to your shell profile and open a new terminal. Then check:

```sh
chat-stasher --version
```

## 2. Check what is at risk

```sh
chat-stasher doctor
```

`doctor` is read-only. It looks at the AI tools on this machine and their settings, and answers one question: **is anything here deleting its own history?** It prints paths, counts, sizes and dates, never the text of a conversation.

Nothing to fix yet. Whatever it reports, the next step starts protecting you.

## 3. Make your first archive

First, write a config file. `init` creates `~/.config/chat-stasher/config.toml` with every setting explained in comments. It never overwrites a config you already have.

```sh
chat-stasher init
```

Next, make a **stage**. This is a folder where new sessions wait, sealed, until they are pushed into the encrypted archive. Put it somewhere you will not delete:

```sh
mkdir -p ~/stash/chat-stasher/stage
```

Now archive once:

```sh
chat-stasher run-once --stage ~/stash/chat-stasher/stage
```

This reads each tool's session files without changing them. It seals what is new into the stage, then encrypts it into a repository. You have not configured a remote, so the repository is on this disk, at `~/.local/share/chat-stasher/repo`.

The first run prints a few lines per session it found, and ends like this:

```
[push] INIT (new repository created) · masterkey created+persisted
[run-once] result: COMPLETED snapshot=created exit_code=0
```

Two things just happened for the first time:

- **This machine got an identity.** It is a random id that names this machine's part of the archive. Another computer gets its own, so two machines never write over each other.
- **A master key was created** at `~/.local/share/chat-stasher/masterkey.json`.

Run the same command again:

```sh
chat-stasher run-once --stage ~/stash/chat-stasher/stage
```

This time it ends with `result: NOOP snapshot=not-created`. Nothing changed, so nothing new was stored. This is what a healthy run looks like most of the time.

## 4. Put the key somewhere safe

> [!WARNING]
> The master key is the **only** way to read your archive. If you lose it, the archive is unreadable forever. There is no recovery code, no reset and no one to ask. That is the price of nobody else being able to open it.

Copy `~/.local/share/chat-stasher/masterkey.json` somewhere that is not this disk. A password manager is a good place, and so is a USB drive kept elsewhere. Keep the copy private: anyone with the key file and access to your archive can read every conversation in it.

## 5. Switch on hourly archiving

`run-once` does one pass and exits. To run it every hour, let chat-stasher write a timer file:

```sh
chat-stasher schedule --stage ~/stash/chat-stasher/stage \
  --output ~/Library/LaunchAgents/com.chat-stasher.run-once.plist
```

It writes the file, then prints `[schedule] install is NOT automatic.` followed by the exact command that loads the timer:

```
mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs/chat-stasher" && cp … && launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.chat-stasher.run-once.plist"
```

Copy that command from **your** terminal and run it. It contains your own paths.

On Linux, ask for a systemd unit instead, then run the command it prints:

```sh
chat-stasher schedule --format systemd --stage ~/stash/chat-stasher/stage --output ~/.config/systemd/user/
```

The timer calls the installed binary, never one inside `target/`: a build path is refused, and without `--binary` the command falls back to `~/.local/bin/chat-stasher`, `/opt/homebrew/bin/chat-stasher` or `/usr/local/bin/chat-stasher`. If you installed somewhere else, pass `--binary` with that path.

## 6. Confirm it works

```sh
chat-stasher status
```

The first line is the verdict:

- `[run-once] Healthy: last run … ago, took … ms, archived … shard(s), …`: everything is working.
- `[run-once] No run has ever been recorded: …` or `[run-once] No run for … (threshold …): the timer may have stopped; …`: the timer is not firing. Check step 5.
- `[run-once] Last run failed: the <step> step errored … ago, with no successful run since.`: the timer runs, but something in the pass fails. The step name tells you where.

The next line counts what the scan finds, for example `[scan] 214 session(s) (0 compressed): claude-code 120 · codex 61 · opencode 33`.

`status` exits with `0` only when the timer looks healthy. It exits `1` when the timer looks dead, which includes "has never run". Its report is on stderr, so run it bare rather than through a pipe if you want to see that exit code.

To look around the archive in your browser:

```sh
chat-stasher ui --repo ~/.local/share/chat-stasher/repo
```

The dashboard runs on your machine only (`127.0.0.1`) and closes itself after five idle minutes.

## Next

- **Add an off-site copy.** Right now the archive is on the same disk as the sessions it protects. That guards against a tool deleting its history, but not against losing the disk. [destinations.md](destinations.md) sets up Cloudflare R2, SFTP or an external disk.
- **Archive your web chats.** Install the browser extension: [install.md → The browser extension](install.md#the-browser-extension). Register the extension's host with **the same stage** you used above.
- **Use more than one computer.** Install chat-stasher on each machine and point them at the same destination. Each machine archives into its own part, and the dashboard shows them side by side.
