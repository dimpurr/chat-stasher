# Chat Stasher menu bar app

A macOS 13+ SwiftUI `MenuBarExtra` app that reads `chat-stasher overview --json --summary` and `chat-stasher status --json`. It combines local upload/scheduler health with archive health, source freshness, one conversation count, a 30-day chart, and per-machine freshness. The app does not open the archive itself. **Open dashboard** starts `chat-stasher ui`.

Build and package a local app bundle:

```sh
swift build
./package-app.sh
open ".build/Chat Stasher.app"
```

Build a demo bundle, then run any of the synthetic screenshot states with `--demo=all-green`, `--demo=source-stopped`, `--demo=silent`, `--demo=offline`, `--demo=cli-missing`, `--demo=cli-old`, `--demo=not-configured`, or `--demo=unreadable`:

```sh
./package-app.sh --demo
open ".build/Chat Stasher Demo.app" --args --demo=all-green
```

Run app status sentence tests with `swift test`. The CLI must be available on `PATH`. With multiple destinations, the app checks each configured destination and shows the one with the worst state; **Settings…** lets you select one destination instead. `CHAT_STASHER_DESTINATION` remains an optional launch-time override. The app refreshes when its popover opens or when **Refresh** is selected.

`chat-stasher-menubar --resolve-cli` prints the version handshake headlessly — one
line naming which CLI the app's PATH search found, the version that binary
reported, and the app's verdict — and exits before any GUI exists. It exists so
the install matrix can drive the real binary:

```sh
bash apps/menubar/scripts/install-matrix.sh
```

That script verifies the app against MEN-3's states — no CLI, the CLI installed
by the real `scripts/install.sh` into a throwaway home, a stale older CLI
earlier on PATH, and the older→newer upgrade — building throwaway CLIs of its
own and never touching the real `~/.local/bin`.

## Release build

`package-app.sh` above is the development path: it builds in debug, writes a
prototype bundle identifier, and signs nothing. A build that can be downloaded
needs a different chain, and that is `scripts/sign-and-notarize.sh`.

```sh
apps/menubar/scripts/sign-and-notarize.sh --version 0.5.0          # notarize
apps/menubar/scripts/sign-and-notarize.sh --version 0.5.0 --signed # sign only
apps/menubar/scripts/sign-and-notarize.sh --ad-hoc                 # no certificate
```

It builds in release, assembles the bundle, signs every executable it finds
inside-out with the Hardened Runtime and a secure timestamp, verifies the
result, and (unless `--signed` or `--ad-hoc`) submits the disk image to Apple's
notary service, staples the ticket and validates it. `--ad-hoc` needs no
certificate and exists to exercise the chain; what it produces cannot be
notarized and is not distributable.

Two prerequisites are not in this repository, and the script refuses rather than
working around them:

- a **Developer ID Application** certificate in the login keychain, and
- **notary credentials** — either the `ASC_KEY_ID`/`ASC_ISSUER_ID`/
  `ASC_PRIVATE_KEY_PATH` API key triple, or a `notarytool` keychain profile.

Check both before a release, without building anything:

```sh
apps/menubar/scripts/sign-and-notarize.sh --self-check
```

It exits 0 when ready, 1 when a prerequisite is absent, and 3 when one could not
be determined — an unanswered question is not the same as an absent one, and
neither is a pass. `apps/menubar/scripts/build-dmg.sh` builds only the disk
image, from a bundle that already exists; `--help` on either script lists the
flags. The release steps an owner follows, and the one thing this chain does not
do, are in [`RELEASING.md`](../../RELEASING.md) under "The menu bar app".

## Read-only data contract

The app consumes `chat-stasher overview --json --summary` and `chat-stasher status --json`, both schema version 1. It requires matching command names and matching process/payload exit codes, and CLI ≥ 0.5.0-rc.2. Missing fields or invalid JSON are treated as an unreadable response or an older CLI; a status document that decodes but carries no `local` section (the 0.4.x shape) is classified as an older CLI, never as a setup problem, because what it lacks is inside that CLI, not in the machine's config; an unknown count is never converted to zero. A failed refresh keeps the last successful archive result visible with an Offline label.

The summary variant supplies per-machine newest snapshot time and health, per-source totals, active calendar days, cadence-derived silence thresholds and last-save timestamps, and 30 local days without sending every session row to the app. The local status supplies the scheduler, last run, staged sessions waiting to upload, configured destination names, CLI version, and the absolute path the app found that CLI at; the About sheet shows the version and that path, which is how "which CLI did the app find on this machine" has an answer when several are installed. With several destinations, the app reads each destination separately and never adds their conversation totals; Settings can show the worst status across all destinations or one named destination. Web chats always say `Extension status: see each browser's extension` until instance reports are present; install counts are never inferred from a browser profile or summed across machines. A source is considered regularly used after activity on three distinct calendar days; its own observed cadence determines when it is marked stopped. The popover shows compact per-source icons and status bars; source names and counts are in the collapsed **Show sources** list. Launch-at-login and silence-threshold controls live in **Settings…** (⌘,).

Sparkle 2 checks the GitHub Releases appcast at `https://github.com/dimpurr/chat-stasher/releases/latest/download/appcast.xml`. The app bundle uses `SUFeedURL` and `SUPublicEDKey`; release signing and appcast generation belong to the release workflow.
