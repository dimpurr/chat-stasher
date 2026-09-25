# chat-stasher (npm)

> **Status: not published yet.** This directory is the npm package source. It is
> assembled and published by the release workflow, and until a release runs,
> `npx chat-stasher` will not resolve. Everything below describes the package
> once that release exists.

The npm install path for [chat-stasher](https://github.com/dimpurr/chat-stasher),
an append-only archive for every LLM conversation, across harnesses.

This package is a **launcher**. It carries no binary. The binary is in a
per-platform package, and this one finds the package for the running platform,
runs it, and relays its exit status and signals. So `npx chat-stasher doctor`
works with no toolchain to install and nothing to compile.

## Install

```sh
npx chat-stasher doctor
```

Or install it:

```sh
npm install -g chat-stasher
chat-stasher --version
```

Requires Node 18 or newer and one of the platforms below.

## What you get

Nothing but the launcher and `node_modules/.bin/chat-stasher`. The binary itself
lives in `@dimpurr/chat-stasher-<platform>-<arch>`, which npm installs alongside
this package on a matching platform.

| Platform | Key | Binary package | Status |
| --- | --- | --- | --- |
| macOS arm64 (Apple Silicon) | `darwin-arm64` | `@dimpurr/chat-stasher-darwin-arm64` | shipped |
| macOS x86_64 (Intel) | `darwin-x64` | `@dimpurr/chat-stasher-darwin-x64` | shipped |
| Linux, Windows | any | none | not shipped |

On a platform with no binary, the launcher prints one line naming it and exits
non-zero. It does not install anything, and it does not fall back to a build.

## How it works, and why

- **The binary is verified against the release's `SHA256SUMS`** before it is
  copied into a package, at assembly time, so a package on npm holds the same
  bytes as the artifact on the GitHub Release. npm itself also pins an integrity
  hash per tarball.
- **There is no `postinstall` script.** Many people install with
  `--ignore-scripts`, and an install hook is supply-chain surface this package
  does not need: the platform packages are chosen by npm's own `os` and `cpu`
  matching, not by code running at install time.
- **The version is pinned in lockstep.** The launcher's `optionalDependencies`
  name each platform package at the exact same version as the launcher, so npm
  can never pair a launcher with a binary from a different release.
- **The three failure modes are three messages.** "Your platform is not shipped",
  "your platform is shipped but the package did not install" (npm treats a failed
  optional dependency as a success, so this is silent at install time), and "the
  package installed but its binary cannot be run" are different situations needing
  different next steps, so the launcher says which one happened.

## What this package does not ship

Linux and Windows binaries. Both need release artifacts first; when they exist,
each needs one more platform package and one more entry in the launcher's
supported list.

For a platform with no npm package, install another way:

```sh
curl -fsSL https://chatstasher.com/install.sh | sh
# or, from source
cargo install chat-stasher --locked
```

## Source

The package is assembled from `npm/` in the
[main repository](https://github.com/dimpurr/chat-stasher): the launcher is
`npm/bin/chat-stasher.js`, the platform package templates are under
`npm/platforms/`, and `scripts/npm/assemble.mjs` turns a release's assets into
the publishable directories.

## Licence

Apache-2.0. The archive's master key is the only key to your data. Back it up.
See [`SECURITY.md`](https://github.com/dimpurr/chat-stasher/blob/main/SECURITY.md).
