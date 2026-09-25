# Chat Stasher menu bar prototype

A macOS 13+ SwiftUI `MenuBarExtra` prototype with a window-style popover. It runs `chat-stasher overview --json` when the popover appears and presents the measured summary fields without exposing session, machine, or harness labels. The **Open dashboard** action starts `chat-stasher ui`; the CLI keeps ownership of its loopback server, token, browser launch, and shutdown behavior.

Build and package a local app bundle from this directory:

```sh
swift build
./package-app.sh
open ".build/Chat Stasher.app"
```

For a synthetic screenshot/demo that does not inspect an archive, package and open the demo bundle:

```sh
./package-app.sh --demo
open ".build/Chat Stasher Demo.app"
```

The CLI must be available on `PATH`. If multiple destinations are configured, set `CHAT_STASHER_DESTINATION` in the app's launch environment to the intended destination; the same value is passed to both commands. No background refresh runs while the popover is closed.

## Read-only data contract

The app consumes only `chat-stasher overview --json` schema version 1. It requires `command == "overview"`, matching process and payload exit codes, and the numeric `summary` fields `machines`, `harnesses`, `sessions`, `lines`, `unknown_time_sessions`, and `no_conversation_content_sessions`. It does not read the archive, config, key file, or repository itself and does not write to them.

- Exit 0 means an index was read; the summary is measured.
- Exit 1 means the archive was read completely and no activity index exists anywhere; the CLI's measured summary is shown with an explicit status.
- Exit 2 (usage/configuration) and exit 3 (incomplete/unreadable archive) are shown as an unknown/error state. They are never converted to zero.
- Missing fields, invalid JSON, and unsupported schema versions also show an unknown/error state.
- The visible refresh time is the local time the successful command response was received; it is not an archive timestamp.

The prototype currently relies on the documented `overview --json` shape rather than defining a second endpoint or aggregation. Any incompatible schema change must update this decoder and the contract together.
