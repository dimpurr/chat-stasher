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

| Platform | Key | Binary package | In a Release? |
| --- | --- | --- | --- |
| macOS arm64 (Apple Silicon) | `darwin-arm64` | `@dimpurr/chat-stasher-darwin-arm64` | yes, every Release |
| macOS x86_64 (Intel) | `darwin-x64` | `@dimpurr/chat-stasher-darwin-x64` | yes, every Release |
| Linux x86_64 | `linux-x64` | `@dimpurr/chat-stasher-linux-x64` | not yet |
| Linux arm64 | `linux-arm64` | `@dimpurr/chat-stasher-linux-arm64` | not yet |
| Windows x86_64 | `win32-x64` | `@dimpurr/chat-stasher-win32-x64` | not yet |

The last column is about the Release, not about npm, because the Release is what
decides whether the package can exist at all: a platform package is assembled
from a Release's own assets, and `scripts/npm/assemble.mjs` refuses to build one
whose binary is not among them (`release asset is not in <dir>`). The Linux and
Windows binaries are produced by the release workflow
(`.github/workflows/release.yml`), but no version tag has been pushed since they
were added, so no Release has carried one — as of 2026-09-25 the newest Release,
v0.4.0, holds two macOS binaries, an extension zip and `SHA256SUMS`. Those three
platform packages cannot be assembled until a Release carries their binaries;
the rows above are the set npm can match on.

The Linux binaries are built against musl and statically linked, so one package
per architecture covers every distribution — the same binary runs on Alpine and
on Ubuntu, and no libc version has to match. On Windows the binary inside the
package is `chat-stasher.exe`: Windows starts a file by its extension, so it
cannot be named `chat-stasher` the way the others are.

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

Anything for a platform the release does not build a binary for — a 32-bit
system, or a CPU architecture other than x86_64 and arm64. Adding one means a
release artifact, one more platform package under `npm/platforms/`, and one more
entry in the launcher's supported list; the three are compared by
`npm/test/platform-packages.test.mjs`, so a half-added platform fails the tests
rather than shipping a package nobody publishes.

For a platform with no npm package, install another way:

```sh
curl -fsSL https://chatstasher.com/install.sh | sh
# or, from source
cargo install chat-stasher --locked
```

The installer keeps the same contract as the table above: it reads the Release's
manifest and refuses, naming the release and what it does carry, when that
release holds no binary for the platform. So on a platform the newest Release
does not carry, the source line is the one that works.

## Source

The package is assembled from `npm/` in the
[main repository](https://github.com/dimpurr/chat-stasher): the launcher is
`npm/bin/chat-stasher.js`, the platform package templates are under
`npm/platforms/`, and `scripts/npm/assemble.mjs` turns a release's assets into
the publishable directories.

## Licence

Apache-2.0. The archive's master key is the only key to your data. Back it up.
See [`SECURITY.md`](https://github.com/dimpurr/chat-stasher/blob/main/SECURITY.md).
