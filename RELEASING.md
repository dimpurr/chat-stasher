# Releasing chat-stasher

The operational model, and the exact steps to cut a release. It is short on
purpose: every rule here is one a release has to obey, and every one of them is
checkable.

## Channels

| Channel | What it is | Published? |
|---|---|---|
| `main` | The development channel. Work lands here; CI must stay green at every commit. Carries a `-dev` version (`0.3.0-dev`). | **Never.** |
| `vX.Y.Z` tag | A stable release. | Yes — GitHub Release, and it becomes the repository's "latest" |
| `vX.Y.Z-rc.N` tag | A release candidate, for checking a build before it is stable. Optional. | Yes — GitHub **prerelease** |
| nightly | — | There is no nightly channel. |

"Stable" means exactly one thing: an annotated tag named `vX.Y.Z` that the owner
approved. A green `main` is not a release, and a `-dev` build is never
mistakable for one, because the version string itself says so.

## Where the version lives

A version string is a claim about a shipped artifact, so each copy of it has a
different owner and a different release-time edit:

| File | Field | Meaning |
|---|---|---|
| `crates/chat-stasher/Cargo.toml` | `version` | **The CLI's version.** It is what `chat-stasher --version` prints and what the native host reports as `host_version`. On `main` it always carries a `-dev` suffix. |
| `scripts/install.sh` | `VERSION` default | Which release `curl \| sh` installs. Pinned on purpose — the installer never resolves "latest". |
| `homebrew/chat-stasher.rb` | `version`, both `url`s, both `sha256`s | The Homebrew tap. |
| `SECURITY.md` | "Supported versions" table | Which line receives fixes. |
| `apps/extension/package.json` | `version` | **The extension's own version**, independent of the CLI. See "The extension" below. |

Anything else that displays a version (the release title, the asset names) is
derived from the **tag**, not from these files — the release workflow never
reads `Cargo.toml`.

## Cutting a stable release

The owner approves the release before step 5. Nothing else pushes a tag, and no
automation creates one.

1. **Confirm `main` is releasable.** CI green on the commit you are about to
   tag, and the local gate list in `CONTRIBUTING.md` exits 0 against it.
2. **Write the changelog entry** in `CHANGELOG.md`: rename `Unreleased — X.Y.Z`
   or add the dated heading, so the entry that ships is the entry that was
   reviewed. Check it against
   `git log --no-merges vPREV..HEAD -- crates/ scripts/ install.sh`.
3. **Drop the `-dev` suffix** in `crates/chat-stasher/Cargo.toml`, so the
   tagged binary reports the released version and not a development one.
4. **Update the version pins** in `scripts/install.sh` (the `VERSION` default),
   the `homebrew/chat-stasher.rb` `version` and both `url`s, and the
   "Supported versions" table in `SECURITY.md`. The Homebrew `sha256` values can
   only be filled in after step 7.
5. **Commit** those edits on `main` (English, per `CONTRIBUTING.md`).
6. **Tag and push the tag.** This is the act that publishes:
   ```sh
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
   The `Release` workflow runs from the tag push — it does not run on a branch.
7. **Verify the published release** before telling anyone it exists:
   - the Release is marked **latest**, and the three uploaded assets are exactly
     `chat-stasher-darwin-arm64`, `chat-stasher-darwin-x86_64` and
     `SHA256SUMS` — nothing extension-shaped is among them. (GitHub's own
     auto-generated "Source code" archives are not assets and are always
     present; the workflow asserts the uploaded set, `release.yml`.)
   - `sha256` of each downloaded binary matches its `SHA256SUMS` line;
   - the downloaded arm64 binary prints `chat-stasher X.Y.Z`.
8. **Fill the Homebrew `sha256` values** from that `SHA256SUMS` and commit them
   to the tap.
9. **Move `main` forward**: bump `crates/chat-stasher/Cargo.toml` to the next
   development version (`X.Y.(Z+1)-dev` or `X.(Y+1).0-dev`) in a new commit.
   `main` never sits on an unsuffixed version.

If a release is wrong, do not delete the tag and re-push it: an installer that
already ran will have the old binary, and `install.sh` pins by tag. Cut the
next version instead.

## How a release candidate works

An rc exists to answer "does this exact artifact install and run" before the
version becomes stable. It uses the same workflow and the same gates as a stable
tag:

```sh
git tag vX.Y.Z-rc.1
git push origin vX.Y.Z-rc.1
```

The tag must end in `-rc.N`; the workflow marks exactly those releases as
GitHub **prereleases**, so they never become the repository's "latest". That
matters because "latest" is what a reader of the Releases page means by stable.

**Nothing installs an rc by default.** `install.sh` pins its version and never
resolves "latest", so an rc is reachable only by asking for it by name:

```sh
CHAT_STASHER_VERSION=0.3.0-rc.1 curl -fsSL .../install.sh | sh
```

If the release candidate is good, cut `vX.Y.Z` from the same code (not from the
rc tag) by the steps above. If it is bad, fix `main` and cut `rc.2`.

## The extension

**Stable releases ship the CLI only.** The extension is not built by the release
workflow, is not attached as a release asset, and is not versioned by the CLI's
version — its `package.json` moves on its own schedule.

The extension joins stable releases when at least four platforms deliver data to
the CLI end to end; until then it is distributed only as an unpacked
development build. That is a condition to be checked by the owner, not a rule
the workflow can enforce, so nothing in `.github/workflows/` will stop a mistake
here — look at the asset list in step 7.

## Why the release workflow runs the gates itself

`release.yml` re-runs the release gate chain before it builds. It does not defer
to CI, because GitHub Actions cannot express "workflow B depends on workflow A":
the v0.1.0 tag once triggered CI and the release workflow in parallel and the
release shipped while CI's gate was failing. The workflow that publishes must be
the workflow that verifies.
