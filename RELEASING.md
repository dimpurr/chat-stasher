# Releasing chat-stasher

The operational model, and the exact steps to cut a release. It is short on
purpose: every rule here is one a release has to obey, and the ones a machine
can check are checked by the workflow that publishes — see "What the workflow
checks" for that list and for the one rule it deliberately leaves to the owner.

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

The release title and the asset names are derived from the **tag**, not from
these files. `Cargo.toml` is read by the release workflow all the same, for
exactly one thing: its first gate asserts that the tag's version and the
`[package]` version are the same string, and ends the run if they are not. The
binary embeds that string (`CARGO_PKG_VERSION`, from the `cargo build` in the
same workflow), so a tag that disagrees with it ships a Release named after the
tag whose `chat-stasher --version` reports the other one. For an rc that is
worse than cosmetic; see "How a release candidate works".

## Cutting a stable release

The owner approves the release before step 5. Nothing else pushes a tag, and no
automation creates one.

1. **Confirm `main` is releasable.** CI green on the commit you are about to
   tag, and the local gate list in `CONTRIBUTING.md` exits 0 against it.
2. **Write the changelog entry** in `CHANGELOG.md`: rename `Unreleased — X.Y.Z`
   or add the dated heading, so the entry that ships is the entry that was
   reviewed. Check it against
   `git log --no-merges vPREV..HEAD -- crates/ scripts/ install.sh`.
3. **Set the release version** in `crates/chat-stasher/Cargo.toml` — `0.3.0-dev`
   becomes `0.3.0` — and commit the same change in `Cargo.lock`, which records
   the workspace crate's own version (`cargo build` rewrites that line for you).
   The tag pushed in step 6 must equal this string exactly, and the binary must
   report the released version rather than a development one.
4. **Update the version pins** in `scripts/install.sh` (the `VERSION` default),
   the `homebrew/chat-stasher.rb` `version` and both `url`s, and the
   "Supported versions" table in `SECURITY.md`. The Homebrew `sha256` values can
   only be filled in after step 7.

   The `VERSION` default is the release that `curl | sh` installs for everyone
   who does not name one, so it is the newest **stable** version — never a
   `-dev` and never a `-rc.N`. Left behind, it quietly hands a new user an old
   build from a command the docs told them to run. Naming a prerelease is what
   `CHAT_STASHER_VERSION` is for.
5. **Commit** those edits on `main` (English, per `CONTRIBUTING.md`).
6. **Tag and push the tag.** This is the act that publishes:
   ```sh
   git tag -a vX.Y.Z -m "chat-stasher vX.Y.Z"
   git push origin vX.Y.Z
   ```
   `-a`, because an annotated tag is the one artifact that records who cut the
   release and when. (The workflow does not check that, though — see "What the
   workflow checks".) The `Release` workflow runs from the tag push — it does
   not run on a branch — and its first gate ends the run unless the tag is
   exactly `vX.Y.Z` or `vX.Y.Z-rc.N` **and** agrees with `Cargo.toml`.
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
git tag -a vX.Y.Z-rc.1 -m "chat-stasher vX.Y.Z-rc.1"
git push origin vX.Y.Z-rc.1
```

An rc is cut by the steps above with two differences. The version committed in
step 3 is `X.Y.Z-rc.N`, not `X.Y.Z` — the binary embeds it, so an rc that
claimed `X.Y.Z` would print the same `chat-stasher --version` as the stable it
is a candidate for, and step 7 could not tell you which of the two you had
downloaded. The version pins in step 4 stay on the newest **stable** release: an
rc is never what an unqualified install gets.

The tag must end in `-rc.N`; the workflow refuses every other `v*` shape, and
marks exactly this one as a GitHub **prerelease**, so an rc never becomes the
repository's "latest". That matters because "latest" is what a reader of the
Releases page means by stable.

**Nothing installs an rc by default.** `install.sh` pins its version and never
resolves "latest", so an rc is reachable only by asking for it by name:

```sh
CHAT_STASHER_VERSION=0.3.0-rc.1 curl -fsSL .../install.sh | sh
```

If the release candidate is good, cut `vX.Y.Z` from the same code (not from the
rc tag) by the steps above — which includes a commit that sets `Cargo.toml` to
`X.Y.Z`, since the tag has to agree with it. If it is bad, fix `main` and cut
`rc.2`.

## The extension

**Stable releases ship the CLI only.** The extension is not built by the release
workflow, is not attached as a release asset, and is not versioned by the CLI's
version — its `package.json` moves on its own schedule.

The extension joins stable releases when at least four platforms deliver data to
the CLI end to end; until then it is distributed only as an unpacked
development build. That is a condition to be checked by the owner, not a rule
the workflow can enforce, so nothing in `.github/workflows/` will stop a mistake
here — look at the asset list in step 7.

### Release channel: `stable` vs `dev`

The extension is built for one of two release channels, and the channel decides
which platforms a build activates:

- **`stable`** — the default of `pnpm build`, and therefore of the zip a release
  produces. An experimental platform is **fully inert** here: its origins are not
  in the generated manifest's content-script matches (no `host_permissions` are
  declared at all), its backfill registry rows and ledgers are **ignored, not
  deleted**, no enumeration or tick serves it, and the popup does not list it.
- **`dev`** — `pnpm dev`, `pnpm build:dev`, and the `pnpm e2e` suite. Every
  platform is active exactly as it is in development today.

The channel is chosen at build time. `build:dev` runs
`CS_RELEASE_CHANNEL=dev pnpm build`; with the variable unset the build is
`stable`. `pnpm dev` and the e2e suite are `dev` because they are development
commands, not because anything sets the variable for them.

**Already-captured, still-undelivered bundles are delivered by either channel.**
The outbox is user data, not platform state: a dev build that captured an
experimental platform while the native host was down leaves the bundle queued
(`<platform>-<session>.json`), and a stable build that later drains the outbox
sends it rather than dropping it. The channel decides which platforms a build
*serves*, never which already-captured conversations it is allowed to hand to the
CLI; dropping a queued bundle would lose data the user already has
(`apps/extension/lib/outbox.ts`, `runDrain`).

**One list is the source of truth.** `ALL_PLATFORMS` in
`apps/extension/lib/contract.ts` carries every platform row with a per-row
`channel: 'stable' | 'experimental'`. A stable build derives its manifest matches
and its runtime platform set from the `stable` rows of that list; a dev build
derives both from every row. Nothing else decides what a stable build touches.

**Promoting a platform is a one-line change.** After an acceptance run on the dev
channel proves the platform's live capture, backfill, and popup path, change that
one row's `channel` from `'experimental'` to `'stable'` in `ALL_PLATFORMS`, then
run the gates (`pnpm -s compile`, `pnpm -s test`, the stable and dev builds,
`pnpm e2e`). `tests/w91-build-channels.test.ts` builds both channels and reads
the generated manifests back, and `tests/w91-channels.test.ts` asserts the list,
the derived sets, and the runtime refusal; together they fail if the two channels
and the list ever drift apart. Demoting a platform — shelving it again — is the
same edit in reverse, and stored rows and ledgers for it are ignored, never
deleted.

The native host and the CLI side need no change for any of this: the channel only
decides what the extension itself builds and serves.

## What the workflow checks

A rule here is worth writing down only if something can tell when it is broken,
so this is the list of what `release.yml` refuses — and, at the end, the one
thing it does not.

- **The tag's shape.** Before the gates and before the build, the tag must be
  exactly `vX.Y.Z` or `vX.Y.Z-rc.N`. The workflow is triggered by `v*`, which is
  wider than that, and each of the other shapes is a plausible typo:
  under the earlier rule, which only asked whether the version contained a
  hyphen, `v0.3.0rc1` would have published as a normal release and taken the
  "latest" slot, while `v0.3.0-rc` and `v0.3.0-beta.1` would have published as
  prereleases. None of them is built now; the run ends first.
- **The version's agreement with `Cargo.toml`.** The same gate reads the
  `[package]` version and requires it to equal the tag minus `v`, character for
  character, for both shapes.
- **The uploaded asset set.** Exactly `chat-stasher-darwin-arm64`,
  `chat-stasher-darwin-x86_64` and `SHA256SUMS` reach the Release, so nothing
  extension-shaped can arrive by accident.

It does not check the tag object. A *lightweight* tag named `vX.Y.Z` passes
every check above, so `git tag -a` in step 6 is a step the owner follows and not
a property this pipeline verifies. Nothing downstream depends on the tag object:
the Release title, notes and asset names are all derived from the tag's name.
The reason to keep using `-a` anyway is that `v0.1.0` and `v0.2.0` are annotated,
and an annotated tag is the only artifact that records who cut a release and
when.

## Why the release workflow runs the gates itself

`release.yml` re-runs the release gate chain before it builds. It does not defer
to CI, because GitHub Actions cannot express "workflow B depends on workflow A":
the v0.1.0 tag once triggered CI and the release workflow in parallel and the
release shipped while CI's gate was failing. The workflow that publishes must be
the workflow that verifies.
