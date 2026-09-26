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

Run app status sentence tests with `swift test`. The CLI must be available on `PATH`; set `CHAT_STASHER_DESTINATION` in the app launch environment if multiple destinations exist. The app refreshes when its popover opens or when **Refresh** is selected.

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

The app consumes `chat-stasher overview --json --summary` and `chat-stasher status --json`, both schema version 1. It requires matching command names and matching process/payload exit codes, and CLI ≥ 0.5.0-rc.2. Missing fields or invalid JSON are treated as an unreadable response or an older CLI; an unknown count is never converted to zero. A failed refresh keeps the last successful archive result visible with an Offline label.

The summary variant supplies per-machine newest snapshot time and health, source totals and last-save timestamps, and 30 local days without sending every session row to the app. The local status supplies the scheduler, last run, staged sessions waiting to upload, configured destination count, and CLI version. Web chats always say `Extension status: see each browser's extension` until instance reports are present; install counts are never inferred from a browser profile or summed across machines. Source activity older than seven days is shown as stopped; source groups with fewer than three archived conversations remain gray as occasional use.

Sparkle 2 checks the GitHub Releases appcast at `https://github.com/dimpurr/chat-stasher/releases/latest/download/appcast.xml`. The app bundle uses `SUFeedURL` and `SUPublicEDKey`; release signing and appcast generation belong to the release workflow.
