# Install

chat-stasher has two parts:

- **The CLI** (`chat-stasher`) archives your coding-agent sessions, and it owns the archive. You always need it.
- **The browser extension** archives web AI chats. It hands every conversation to the CLI on your machine, so it needs the CLI too.

This page installs each part, then covers updating and uninstalling. To make your first archive afterwards, go to [start.md](start.md).

## The CLI

### macOS

Prebuilt binaries are published for Apple Silicon and Intel Macs.

**Install script (recommended):**

```sh
curl -fsSL https://raw.githubusercontent.com/dimpurr/chat-stasher/main/scripts/install.sh | sh
```

What the script does:

| | |
|---|---|
| Version | Installs a **pinned** release, never "whatever is latest". To choose another version, set `CHAT_STASHER_VERSION=0.4.0` (for example) before `sh`. |
| Integrity | Downloads the release's `SHA256SUMS` and refuses to install if the binary does not match. |
| First run | Starts the downloaded binary once before moving it into place. A binary that matches its checksum but cannot run on this machine is refused, and an install that was already there is left alone. |
| Location | `~/.local/bin/chat-stasher`. To use another folder, set `CHAT_STASHER_INSTALL_DIR`. |
| Privileges | Never uses `sudo`, and never writes to a system folder. |
| Your shell | Tells you if the folder is not on your `PATH`, and prints the line to add. It does not edit your shell files. |

The binary is downloaded by `curl`, not by a browser, so macOS does not quarantine it and it runs without an extra approval step.

**Homebrew:**

<!-- RELEASE GATE: the formula must be live in dimpurr/homebrew-tap (and `brew install` tested on a clean machine) before this section ships. -->
```sh
brew install dimpurr/tap/chat-stasher
```

Homebrew installs the same checksummed release binary, and `brew upgrade` updates it. Use one install method only. If you have both, the `chat-stasher` your shell finds first depends on `PATH` order, and can be an older copy.

### Linux

Prebuilt Linux binaries are published from **0.5.0**: `chat-stasher-linux-x86_64` and `chat-stasher-linux-arm64`. They are built against musl and statically linked, so one binary covers every distribution and no libc version has to match. The install script installs them the way it installs a Mac build.

<!-- RELEASE GATE: the version named in the command below must be released. Until it is, the same command works with a 0.5.0 release candidate named explicitly. -->
Name the version, because the script otherwise installs the newest **stable** release, and a release from before the Linux binaries existed has none for this platform. It refuses rather than installing something that would not run:

```sh
curl -fsSL https://raw.githubusercontent.com/dimpurr/chat-stasher/main/scripts/install.sh | CHAT_STASHER_VERSION=0.5.0 sh
```

If you would rather build it, you need a Rust toolchain:

```sh
git clone https://github.com/dimpurr/chat-stasher
cd chat-stasher
cargo build --release
install -m 755 target/release/chat-stasher ~/.local/bin/
```

Copy the binary out of `target/` as the last line does. Timers and the browser host record the binary's path, and a path inside `target/` stops working after `cargo clean`.

The support table in the [README](../README.md#support-at-a-glance) shows where each tool keeps its sessions on this platform and how the project knows. On Linux those paths come from a tool's own source code or documentation, or from a measurement taken on macOS; nothing records a Linux machine that archived end to end, so read a Linux path as registered rather than proven. Timers use systemd user units (`chat-stasher schedule --format systemd`).

### Windows

A prebuilt `chat-stasher-windows-x86_64.exe` is published from **0.5.0**. The install script does not install it: it is a POSIX `sh` script, so on Windows it prints that asset's URL and stops rather than pretending. Download the file, put it anywhere on your `PATH`, and run `chat-stasher doctor`.

Two things are known about Windows, and neither is a promise:

- One Claude Code path-handling detail is unverified. Windows session paths are readable slugs while they are short and a hashed directory once they are long, and whether the drive-letter colon and the backslash are sanitised the same way in both cases has not been measured on real Windows hardware.
- `schedule` renders launchd and systemd timers only. Use Task Scheduler to run `chat-stasher run-once --stage <stage>` yourself.

### npm and cargo

<!-- RELEASE GATE: 0.5.0 must be published to both registries before this section is accurate. Until then the npm package resolves only for the 0.5.0 prerelease, and `cargo install chat-stasher` finds no crate. -->
From **0.5.0**, a release is also published to two package registries, for platforms the install script does not cover and for people who install everything that way:

```sh
npm install -g chat-stasher
cargo install chat-stasher
```

The npm package is a launcher and carries no binary: it pulls in the package built for your platform, so `npx chat-stasher doctor` needs Node 18 or newer and nothing to compile. `cargo install` builds from source with a Rust toolchain; `cargo binstall chat-stasher` fetches a prebuilt binary instead where the release has one.

### Check the install

```sh
chat-stasher --version
chat-stasher doctor
```

`doctor` is read-only. It reports which AI tools on this machine may be deleting their own history, and prints paths, counts, sizes and dates only.

## The browser extension

The extension is released alongside the CLI as `chat-stasher-extension-X.Y.Z.zip`, on each release page from 0.4.0 onwards. It is **not in any extension store yet**, so you load it yourself. It runs in Chromium-based browsers: Chrome, Edge, Brave, Vivaldi and Chromium.

**Which platforms it covers.** The released build covers ChatGPT, Claude, DeepSeek, Gemini and Grok. Perplexity and Kimi are in the development build only. The [README](../README.md#support-at-a-glance) shows what is verified on each.

**Install it in every browser profile you chat in.** A browser extension belongs to one profile, not to the browser as a whole, so a copy in Chrome's *Personal* profile captures nothing in its *Work* profile. Download and unzip the release once, then load it in each profile you chat in (steps 3 and 4 below). Steps 1 and 2 are different: you register the host **once for the machine**, and the unzip is one folder several profiles can share.

What one install per profile means, once it is done:

- each copy captures the tabs of its own profile and nothing else;
- each copy keeps its own queue of captures waiting to be delivered, and its own backfill progress, so a profile you never open neither captures nor backfills;
- every copy in every browser delivers into the **same stage** on this machine, so they all feed your one archive.

Two counts describe different things, and they are never added together:

| Count | What it is |
|---|---|
| **captured by this browser** | What one profile's copy of the extension has collected. Some of it may still be waiting in that profile's queue. |
| **saved** | What a `push` has sealed into your encrypted repository. This is a fact about the archive, and the extension cannot see it. |

The popup's one host line is neither: it counts the **shared stage**, which every install on this machine writes into. That is why two profiles can show the same number there.

### 1. Register the local host

The extension delivers every conversation to a small host program: the `chat-stasher` binary itself, started by your browser. Register it first, and point it at the **same stage** your CLI archives from. The stage must already exist:

```sh
mkdir -p ~/stash/chat-stasher/stage
chat-stasher install-native-host --stage ~/stash/chat-stasher/stage
```

- It writes one small manifest into each installed browser's configuration folder, and prints every path it writes.
- It needs no administrator rights.
- Running it twice is harmless.
- It records the stage in your config as `[native_host] stage`.
- `--browser chrome` (repeatable) limits it to the browsers you name.
- `--uninstall` removes exactly the files it wrote.

**The host is one program on this machine, shared by every browser and every profile on it.** The command writes one manifest per *browser*, in that browser's own configuration folder, and every profile of that browser reads the same one. All of those manifests point at the same `chat-stasher` binary and the same stage. Three consequences:

- Run this once per machine and per user account, not once per profile. A browser you install later is not covered until you run the command again, because its default is "every browser whose data directory exists here".
- Removing a browser profile does not remove the host, and does not affect the other profiles or the other browsers. The delivery channel is unaffected by what happens to a profile.
- `--uninstall` is the reverse of the whole registration, not of one profile. Running it takes the delivery channel away from every browser and every profile on this machine at once, so it is the wrong tool for "I stopped using this one Chrome profile": do that on that profile's own `chrome://extensions` page instead. `--browser chrome` limits it to one browser. It deletes no capture, and leaves your config and the stage alone.

The host never creates a stage and never invents a machine identity. If the stage is missing, or this machine has never run chat-stasher, the host refuses and says what to fix. Run `chat-stasher run-once --stage <stage>` once from your terminal before loading the extension.

### 2. Load the extension

Do this in each browser profile you chat in. `chrome://extensions` is per profile: it lists that profile's extensions, so loading the extension in one profile does nothing for the next one. Download and unzip the release once, then repeat the last two steps in every profile you want covered.

1. Download `chat-stasher-extension-X.Y.Z.zip` from the [latest release](https://github.com/dimpurr/chat-stasher/releases/latest).
2. Unzip it into a folder you will **keep**. The browser loads the extension from that folder, so deleting the folder removes the extension.
3. Open `chrome://extensions`. Other browsers have the same page: `edge://extensions`, `brave://extensions`, and so on.
4. Turn on **Developer mode**, click **Load unpacked**, and choose the folder.

The same unpacked folder can be loaded by several profiles and several browsers at once: each gets its own copy, with its own queue and its own settings.

### 3. Reload your chat tabs

A chat tab that was open before the extension was installed or updated is **not captured** until you reload it. Browsers do not inject extensions into pages that already exist, and none of the permissions this extension asks for would change that. A page the extension never reached reports itself: the popup lists it, one record per site, with the action that clears it.

### 4. Check the popup

Click the extension's icon **in each profile you installed it in**. Each copy has to reach the host on its own, and the popup is where it says whether it can. The popup shows:

- one summary line from the host: sessions staged in the last 24 hours and in total, how those 24 hours split by tool, and when the last push happened. A value the host cannot measure says *unknown*, with the reason. This line describes the **stage**, which every install on this machine shares, so it reads the same in every profile;
- an **Open dashboard** button, which starts `chat-stasher ui` and opens it.

If the host is missing, unreachable or older than the extension, the popup says exactly that. The dashboard button is then disabled, and the reason is shown.

### 5. Optional: archive past conversations (backfill)

Conversations you open are captured as you use the site. Your **past** conversations are a separate, opt-in step: switch on **backfill** for a platform in the popup.

- Backfill makes its requests from inside an open, logged-in tab of that platform. The extension itself has no permission to reach any site. With no such tab open, backfill waits, and it carries on by itself once you open one. Any page of that platform works; it does not have to be a particular conversation.
- It is slow on purpose. Small batches are spread over the day, under a daily cap. The popup's coverage card opens a full coverage page, where one of three speeds is chosen: *gentle* (the default) fetches one conversation per round and draws a cap of 150 to 200 for the day, *standard* fetches two and draws 300 to 400, and *faster* fetches four and draws 600 to 800. **The speed changes how much is done, never how fast:** the gap before each request is the same at all three.
- **Every one of those numbers is per install.** They live in the profile's copy of the extension and no other copy can see them. If you install the extension in three profiles and switch backfill on in all three, you have three independent schedules, each drawing its own daily cap, so the account's daily total can be about three times the number above. Nothing coordinates them in this version: the copies do not talk to each other and there is no server between them. If that matters for an account you care about, switch backfill on in **one** profile. Two copies logged into the same account list the same conversations, and both deliver into the same stage, so the others would mostly fetch what it already fetched.
- A conversation that comes back incomplete is recorded as a failure, with a reason. It is never stored as if it were whole.

### Without the host: the export file

If you prefer not to register a host, the popup can export captures that have not been delivered as a file, named `chat-stasher-export-<UTC timestamp>.jsonl`. Point the CLI at the folder holding it:

```sh
chat-stasher ingest --inbox <folder with the export> --stage ~/stash/chat-stasher/stage
```

Exporting does not remove the captures from the extension, so the same bytes delivered later are recognised and stored once.

## Updating

**CLI.** Run the install script again; it installs the release it currently points to. With Homebrew, run `brew upgrade chat-stasher`. Your config, archive and key are not touched. If the binary's path changed, run `install-native-host` again, and `schedule` too, so the browser host and the timer point at the new copy. Both commands cover the whole machine, so one run is enough however many profiles you installed the extension in.

**Extension.** Unzip the new release **over the same folder**. In each profile where you loaded that folder, press the reload button on the extension's card in that profile's `chrome://extensions`, then reload your open chat tabs there. Every profile has its own copy and its own card, so updating is one pass but not one click.

The CLI and the extension have separate version numbers; [CHANGELOG.md](../CHANGELOG.md) records which extension version shipped with each CLI release.

## Uninstalling

Uninstalling the tool never deletes your archive. Remove the archive yourself only if you mean to.

**The host and the extension are removed separately, and they are not the same size of action.** The extension is per profile; the host is per machine. Removing one profile's extension leaves every other profile and browser working. Removing the host takes the delivery channel away from all of them at once.

| To remove | Do this |
|---|---|
| The hourly timer (macOS) | `launchctl bootout "gui/$(id -u)" ~/Library/LaunchAgents/com.chat-stasher.run-once.plist`, then delete the file. Do the same for `com.chat-stasher.reclaim-stage.plist` if you installed it. |
| The hourly timer (Linux) | `systemctl --user disable --now` the chat-stasher timer, then delete its unit files. |
| The extension, in one profile | Remove it on that profile's own `chrome://extensions` page. Every other profile and every other browser keeps its copy and keeps delivering. ⚠️ This also deletes any captures that profile had not yet delivered, so read the next section first if you are not sure. |
| The extension, everywhere | Repeat the row above in every profile where you installed it. |
| The browser host, everywhere on this machine | `chat-stasher install-native-host --uninstall`. This is machine-wide: it removes the manifest for every browser it registered, and therefore the delivery channel for every profile of each, in one command. Add `--browser chrome` (repeatable) to remove it from one browser only. It deletes no captures, leaves your config and the stage alone, and does not touch the entry for any other program. |
| The CLI | Delete `~/.local/bin/chat-stasher`, or run `brew uninstall chat-stasher`. |
| Config | `~/.config/chat-stasher/` |
| Local state, local archive and key | `~/.local/share/chat-stasher/` (`repo/`, `masterkey.json`, `state/`, `machine-identity`) |
| The stage | the folder you chose, for example `~/stash/chat-stasher/stage` |
| Timer logs (macOS) | `~/Library/Logs/chat-stasher/` |

### Before you remove the host

**A host is shared; captures are not.** They wait inside the profile that produced them, and only that profile's copy of the extension can read them. So drain them profile by profile **before** you uninstall the extension anywhere:

1. Open each browser profile where you installed the extension.
2. Click the extension's icon. If it reports captures that have not been delivered, press **export undelivered captures**. The file lands in that profile's download folder.
3. Feed each export file to the CLI, naming the same stage you archive from:

   ```sh
   chat-stasher ingest --inbox <folder holding the export> --stage ~/stash/chat-stasher/stage
   ```

4. Only now remove the extension from those profiles, and remove the host last.

Exporting does not remove anything from the extension, so a conversation that was in fact delivered is recognised as already stored rather than archived twice. A capture still queued when its profile goes away is **gone**: uninstalling the extension deletes its queue with it.

> [!WARNING]
> `install-native-host --uninstall` is not how you remove the extension from one profile. It unregisters the host for **every** browser on this machine, so any copy you leave in place can no longer deliver and its captures pile up in its own queue instead. Remove a profile's extension from that profile's `chrome://extensions` page; keep `--uninstall` for the day you are taking chat-stasher off this machine.

> [!WARNING]
> Deleting `repo/` deletes a local archive, and deleting a key file makes the archive it opens unreadable. Remote archives (R2, SFTP) are not touched by any of the steps above. Delete them in your provider's console when you are sure.

## Where things live

| What | Default path | Changed with |
|---|---|---|
| Config | `~/.config/chat-stasher/config.toml` | `XDG_CONFIG_HOME` |
| Local archive | `~/.local/share/chat-stasher/repo` | `rustic_repo` in the config, or per destination |
| Master key | `~/.local/share/chat-stasher/masterkey.json` | `rustic_key_file`, or `key_file` per destination |
| Machine identity | `~/.local/share/chat-stasher/machine-identity` | `machine = "…"` in the config |
| Stage | the folder you pass to `--stage` | - |
| Browser host registration | one manifest per installed browser, inside that browser's own configuration folder | `--browser` at `install-native-host` |

On macOS and Linux these defaults follow `XDG_CONFIG_HOME` / `XDG_DATA_HOME` when they are set.
