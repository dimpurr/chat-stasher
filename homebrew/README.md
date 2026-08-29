# Homebrew tap: chat-stasher

This is the official [Homebrew](https://brew.sh) tap for
[`chat-stasher`](https://github.com/dimpurr/chat-stasher) — an append-only
archive for every LLM conversation, across harnesses.

Unlike the source-install path in the main repository, this tap installs a
**precompiled binary** from the project's GitHub Release. No Rust toolchain, no
`cargo build`, no Apple notarization ceremony — just a checksummed binary on
your `PATH`.

## Requirements

- macOS on **Apple Silicon (arm64)** — this is the only platform that ships a
  prebuilt binary today. Intel macOS and Linux are not available yet (see
  [What this tap does not ship](#what-this-tap-does-not-ship)).
- [Homebrew](https://brew.sh) (works on both Intel and Apple Silicon Homebrew;
  only the *formula* is arm64-only).

## Install

```sh
brew install dimpurr/chat-stasher/chat-stasher
```

Homebrew will fetch the tap and install the formula automatically. If you
prefer to tap explicitly first:

```sh
brew tap dimpurr/chat-stasher
brew install chat-stasher
```

Verify the install:

```sh
chat-stasher --version
# chat-stasher 0.2.0

chat-stasher doctor
```

## What you get

- `/opt/homebrew/bin/chat-stasher` (Apple Silicon Homebrew prefix) — the CLI.
- The binary is verified against a pinned `sha256` in the formula, so a
  corrupted or tampered download fails the install instead of landing on your
  machine.

## Upgrade

```sh
brew upgrade chat-stasher
```

(Or `brew upgrade dimpurr/chat-stasher/chat-stasher` when you have multiple
taps.)

## Uninstall

```sh
brew uninstall chat-stasher
```

The tap itself stays registered. To remove it too:

```sh
brew untap dimpurr/chat-stasher
```

## How this tap works (and why it is a formula, not a cask)

- It is a **formula**, not a cask. The downloaded artifact is a plain CLI
  binary — no `.app`, no `.dmg`, no installer — so there is nothing for Apple
  notarization to sign.
- Homebrew downloads the binary with `curl`, which does **not** set
  `com.apple.quarantine`, so the binary runs without tripping Gatekeeper.
- The formula does **not** build from source. The URL and artifact names are
  kept in lockstep with `scripts/install.sh` and
  `scripts/release-artifacts.sh` in the main repository, so the release
  pipeline, the shell installer, and this tap all fetch the same bytes.

## What this tap does not ship

| Target | Status |
| --- | --- |
| macOS arm64 (Apple Silicon) | ✅ prebuilt, v0.2.0 |
| macOS x86_64 (Intel) | ⏳ not shipped yet — the formula already carries the branch |
| Linux | ❌ not shipped — no prebuilt binary, no source build path in this tap |

For a platform without a prebuilt binary, build from source:

```sh
git clone https://github.com/dimpurr/chat-stasher
cd chat-stasher && cargo build --release
```

See [`docs/install.md`](https://github.com/dimpurr/chat-stasher/blob/main/docs/install.md)
in the main repository for the full manual-install guide.

## Security & trust

- The formula pins a `sha256` per architecture; Homebrew refuses to install on
  mismatch.
- Homebrew's default `curl` fetch means no quarantine flag — the unsigned
  binary runs without the `rc=137` kill that a browser-downloaded,
  quarantine-flagged binary would hit.
- The archive's master key is the only key to your data. Back it up. See the
  main repository's [`SECURITY.md`](https://github.com/dimpurr/chat-stasher/blob/main/SECURITY.md).
