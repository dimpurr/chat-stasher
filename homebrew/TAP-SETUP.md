# TAP-SETUP.md — Creating the `dimpurr/homebrew-chat-stasher` tap

This document is the one-time, do-it-by-hand guide for **Dim** to create the
GitHub repository that backs the `dimpurr/chat-stasher` Homebrew tap. Only you
can do the "create the repo" part — no script, CI, or agent can do it for you.

Everything here was verified against the Homebrew official documentation on
2026-08-26:

- <https://docs.brew.sh/Taps>
- <https://docs.brew.sh/How-to-Create-and-Maintain-a-Tap>

If a later Homebrew version changes any of these rules, re-check those pages
before trusting this file.

---

## 0. TL;DR checklist

1. Create GitHub repo **`dimpurr/homebrew-chat-stasher`** (must start with `homebrew-`).
2. Put `chat-stasher.rb` in the repo (root **or** `Formula/` — see §2).
3. Copy the `README.md` as the repo's README.
4. Replace the placeholder `sha256` in the formula with the real digest (§5).
5. Create the `v0.1.0` GitHub Release in `dimpurr/chat-stasher` with
   `chat-stasher-darwin-arm64` + `SHA256SUMS` (§5).
6. Test: `brew install dimpurr/chat-stasher/chat-stasher` (§6).

---

## 1. What the repo must be called (hard rule)

**The repository must be named `homebrew-chat-stasher`.**

From the official docs — *Taps*:

> "On GitHub, a repository must be named `homebrew-<repository>` to use the
> one-argument form of `brew tap`."

> "`brew tap <user>/<repository>` clones
> `https://github.com/<user>/homebrew-<repository>` into Homebrew's tap
> directory."

So:

| Thing | Value |
| --- | --- |
| GitHub repository | `https://github.com/dimpurr/homebrew-chat-stasher` |
| Tap short name (what `brew` calls it) | `dimpurr/chat-stasher` |
| Fully-qualified install command | `brew install dimpurr/chat-stasher/chat-stasher` |
| Untap command | `brew untap dimpurr/chat-stasher` |

The `homebrew-` prefix is what lets the short one-argument form work. If the
repo were named something else (e.g. `chat-stasher-tap`), users would need the
two-argument form `brew tap dimpurr/chat-stasher https://...` — do not do that.

**Do not use a personal `homebrew-core`-style name or fork `homebrew-core`.**
This is an independent tap.

---

## 2. Directory structure and where the formula goes

From the official docs — *How-to-Create-and-Maintain-a-Tap*:

> "Formula files can be added under either the `Formula` subdirectory, the
> `HomebrewFormula` subdirectory or the repository's root. The first available
> directory is used, other locations will be ignored."

That last sentence is load-bearing: **only one location may hold formulae.** Do
not put `chat-stasher.rb` in the root *and* in `Formula/`.

Recommended layout (subdirectory is what the docs recommend):

```
homebrew-chat-stasher/
├── Formula/
│   └── chat-stasher.rb      ← the formula (copy from packaging/homebrew/)
├── README.md                ← copy from packaging/homebrew/README.md
└── LICENSE                  ← Apache-2.0 (same license as the main repo)
```

A root-level `chat-stasher.rb` also works — pick one layout and stay with it.

Notes:

- **No `Casks/` directory.** This tap ships a formula (CLI binary), not a
  cask. Casks go in `Casks/`; formulae go in `Formula/` or root.
- **No GitHub Actions workflow is required.** The formula points at binaries
  released by the *main* `dimpurr/chat-stasher` repository. The tap repo itself
  is a static container of formula files — nothing builds in it.
- The formula filename must be `chat-stasher.rb` and must define
  `class ChatStasher`. Homebrew derives the formula name from the filename.

---

## 3. What the formula file contains (why it looks this way)

The formula at `packaging/homebrew/chat-stasher.rb` is a **binary formula**:

- `url` points at the GitHub Release asset, **not** a source tarball.
- There is intentionally **no top-level `url`**: only macOS arm64 ships a
  binary, so the URL is selected inside `on_macos` via `Hardware::CPU.arm?`.
  On Linux the formula has no URL and install fails early — by design.
- `def install` just copies the staged artifact to `bin/chat-stasher`.
- `test do` runs `chat-stasher --version` and asserts the output contains
  `chat-stasher 0.1.0` — a real check, not an empty block.

**Keep the artifact names in sync with the main repo.** The three places that
must agree:

1. `scripts/install.sh` — `BASE_URL` and `ARTIFACT` (`chat-stasher-darwin-arm64`)
2. `scripts/release-artifacts.sh` — writes `$OUT/chat-stasher-$HOST`
3. This formula — `url` + `sha256`

If the release pipeline renames the artifact and this formula is not updated in
the same commit, `brew install` will fetch a 404.

---

## 4. One-time repo creation (Dim)

1. Create the repository on GitHub:
   - Owner: `dimpurr`
   - Name: **`homebrew-chat-stasher`**
   - Visibility: **public** (Homebrew taps must be publicly readable)
   - Do **not** initialize with README/license/gitignore (you are copying files
     in; choosing "Add a README" would conflict on push).

2. Clone it and copy the prepared files in:

   ```sh
   git clone https://github.com/dimpurr/homebrew-chat-stasher.git
   cd homebrew-chat-stasher

   # from a checkout of the main repo:
   cp <main-repo>/packaging/homebrew/chat-stasher.rb Formula/
   cp <main-repo>/packaging/homebrew/README.md README.md
   cp <main-repo>/LICENSE LICENSE
   ```

3. Commit and push:

   ```sh
   git add -A
   git commit -m "Add chat-stasher formula v0.1.0"
   git push -u origin main
   ```

   No tag is required on the tap repo; the formula pins the version and sha256
   itself.

---

## 5. Publish the v0.1.0 release (must happen before users can install)

`brew install` downloads the binary from the **main** repository's GitHub
Release, so the release must exist first:

1. In `dimpurr/chat-stasher`, build and stage the artifacts:

   ```sh
   bash scripts/release-artifacts.sh   # writes dist/ with binary + SHA256SUMS
   ```

2. Create a GitHub Release tagged **`v0.1.0`** (the tag must be exactly
   `v0.1.0` — `install.sh` and the formula both use `v${VERSION}`), and upload
   from `dist/`:
   - `chat-stasher-darwin-arm64`
   - `SHA256SUMS`

3. **Replace the placeholder sha256 in the formula.** Get the real digest:

   ```sh
   shasum -a 256 dist/chat-stasher-darwin-arm64
   ```

   Open `Formula/chat-stasher.rb`, replace the all-zeros
   `sha256 "0000…0000"` in the arm64 branch with the real digest, then commit
   and push the tap. (The formula currently carries the placeholder on purpose;
   installing before this step fails the checksum — that is the expected
   guard, not a bug.)

4. If/when an Intel (x86_64) build is ever released, the same release assets
   should include `chat-stasher-darwin-x86_64` and the formula's `else` branch
   sha256 gets filled in too.

---

## 6. Verify the tap end-to-end

On a clean-ish machine (or after removing any prior install):

```sh
brew uninstall chat-stasher 2>/dev/null; true
brew untap dimpurr/chat-stasher 2>/dev/null; true

# Fully-qualified form — Homebrew auto-taps, no explicit `brew tap` needed:
brew install dimpurr/chat-stasher/chat-stasher

chat-stasher --version   # → chat-stasher 0.1.0
chat-stasher doctor      # → runs and reports (may exit non-zero if a timer is unhealthy; that's expected)
```

Sanity checks that are cheap to run before pushing the tap:

```sh
brew style --formula Formula/chat-stasher.rb
brew audit --formula Formula/chat-stasher.rb
```

`brew audit` will flag that the formula has no top-level `url` (it is
macOS-only) and may warn that the release URL is unreachable until the release
is published — both are expected for this design. It should **not** complain
about sha256 shape (all-zeros is valid hex), and any complaint about the
formula DSL itself must be fixed before publishing.

---

## 7. Update flow for a future release (e.g. v0.2.0)

1. Bump `version` in `scripts/install.sh` (default `VERSION`) and in the
   formula (`version "0.2.0"`, and `v0.2.0` in the URLs).
2. Release `v0.2.0` in `dimpurr/chat-stasher` with the new artifacts.
3. Replace both sha256s in the formula.
4. Push the tap. Users get it via `brew upgrade`.

---

## 8. Undo / teardown (if it ever goes wrong)

- Remove the tap: `brew untap dimpurr/chat-stasher`
- Remove the install: `brew uninstall chat-stasher`
- Delete the GitHub repo under `dimpurr/homebrew-chat-stasher`.

Nothing in this flow writes to system directories, modifies shell config, or
requires sudo.
