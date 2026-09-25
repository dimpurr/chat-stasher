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
| `homebrew/chat-stasher.rb` | both `url`s (the `vX.Y.Z` path segment of each is the formula's version), both `sha256`s | The source copy of the Homebrew tap formula. It deliberately has no separate `version` line: `brew audit --strict` flags an explicit version as redundant when the URL already carries one. Its `test` compares the installed binary against `version` itself rather than repeating the string, so a release edits the two URLs and the two digests and nothing else. |
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
   both `url`s in `homebrew/chat-stasher.rb` (the `vX.Y.Z` segment of each is
   the formula's version — there is no separate `version` line, on purpose, and
   the formula's `test` reads that scanned version rather than a copy of it),
   and the "Supported versions" table in `SECURITY.md`. The Homebrew `sha256`
   values can only be filled in after step 7.

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
   workflow checks".) The `Release` workflow runs from the tag push. It can also
   be started by hand, for a retry or a dry run, but **only against a tag**: its
   first gate reads the full ref and ends the run unless it is under
   `refs/tags/`, and unless the tag is exactly `vX.Y.Z` or `vX.Y.Z-rc.N`
   **and** agrees with `Cargo.toml`. So the manual form asks for the tag, never
   a branch — a branch named `v0.3.0` is refused there rather than treated as
   one (see "What the workflow checks").
   The workflow then publishes the checksum-verified npm platform packages,
   the `chat-stasher` npm launcher, and the crate to crates.io, in that order.
   Each package version is checked first and an existing version is skipped,
   so a failed run can be retried from the Actions `workflow_dispatch` control
   on the same tag. An rc publishes every npm package under the `next`
   dist-tag and a stable release under `latest`; the tag shape decides which
   (`scripts/release-tag-gate.sh`).
   `dry_run` defaults to true and runs `npm publish --dry-run` for the assembled
   npm packages and `cargo publish --dry-run --locked`; set it to false to
   publish. The dry-run flag covers registry publication; the normal release
   and Homebrew workflow steps still run.

   **Passing `--tag next` is not enough to keep an rc out of `latest`**, and
   this document claimed it was until v0.5.0-rc.2 disproved it. npm sets
   `latest` on a package's **first** publish in addition to the tag asked for,
   so on a package with no stable release yet the candidate becomes the version
   a plain `npm install` resolves. The publish step therefore repairs the tag
   after publishing — `scripts/npm-latest-tag.sh` re-points `latest` at the
   newest stable when one exists, and writes a note to the run's job summary
   when none does. See "Registry credentials" and step 7.
7. **Verify the published release** before telling anyone it exists:
   - the Release is marked **latest**, and the seven uploaded assets are exactly
     `chat-stasher-darwin-arm64`, `chat-stasher-darwin-x86_64`,
     `chat-stasher-linux-x86_64`, `chat-stasher-linux-arm64`,
     `chat-stasher-windows-x86_64.exe`, `chat-stasher-extension-X.Y.Z.zip` and
     `SHA256SUMS`. The extension zip is
     the stable channel and its manifest contains no experimental origins.
     (GitHub's own
     auto-generated "Source code" archives are not assets and are always
     present; the workflow asserts the uploaded set, `release.yml`.)
   - `sha256` of each downloaded asset, including the extension zip, matches its
     `SHA256SUMS` line;
   - the downloaded arm64 binary prints `chat-stasher X.Y.Z`.
     Each build job already ran its own binary with `--version` before
     uploading it, so this is the same check made by the owner, on the one
     platform the owner has to hand — not the first time any of these binaries
     was started.
   - **npm's `latest` names the newest stable version**, for all six packages —
     with the one exception set out under the block below: a package whose first
     stable release has not happened yet has no stable version to point `latest`
     at, and naming the candidate is expected there. This is the check the
     workflow now makes for itself, and it is still worth making by hand because
     it is the one registry state the workflow cannot undo once written:
     ```sh
     # Stable release: every one must print the version just released.
     # Release candidate: each package must print its newest *stable* version.
     # The one exception is a package with no stable release yet: npm points
     # `latest` at its first publish, so there it prints the rc until the
     # first stable release moves it.
     for p in chat-stasher @dimpurr/chat-stasher-darwin-arm64 \
              @dimpurr/chat-stasher-darwin-x64 @dimpurr/chat-stasher-linux-arm64 \
              @dimpurr/chat-stasher-linux-x64 @dimpurr/chat-stasher-win32-x64; do
       printf '%-42s %s\n' "$p" "$(npm view "$p" dist-tags.latest)"
     done
     ```
     A package whose first stable release has not happened yet has no stable
     version to point `latest` at; the job summary says so in as many words, and
     `latest` naming the candidate is expected there and temporary. Everywhere
     else `latest` must name the newest stable version — on an rc that means a
     package which already has a stable release prints that stable, not the
     candidate. What is *not* acceptable on any later release is `latest` still
     naming an older version, whether that older version is a candidate or a
     stable, while a newer stable exists.
8. **Fill the Homebrew `sha256` values** from that `SHA256SUMS` in
   `homebrew/chat-stasher.rb`. Step 4 set the URLs; this step makes them
   checksum-pinned.
9. **Update the Homebrew tap.** The tap is
   [`dimpurr/homebrew-tap`](https://github.com/dimpurr/homebrew-tap) (Homebrew
   short name `dimpurr/tap`); its formula is `Formula/chat-stasher.rb`, and
   users install it with `brew install dimpurr/tap/chat-stasher`. When the
   `TAP_TOKEN` repository secret is set, `release.yml`'s `homebrew-tap` job does
   this for you — it copies this release's `sha256` digests into the tap formula
   and opens a **draft PR** against `dimpurr/homebrew-tap`. Review that PR, run
   `brew audit --strict --online` and `brew style` against it, and merge it only
   after step 7 passed. When `TAP_TOKEN` is absent the job is skipped and this
   step is manual: copy the two `sha256` values (and the `vX.Y.Z` segment of
   both URLs) into the tap's `Formula/chat-stasher.rb`, run the same two checks,
   and open the PR.

   The job opens a *draft* PR and never merges — publishing to the tap is the
   step the owner reviews. `TAP_TOKEN` is a fine-grained personal access token
   with `contents: write` and `pull_requests: write` on
   `dimpurr/homebrew-tap`, because the workflow's own `GITHUB_TOKEN` cannot
   write to a different repository. The acceptance test is
   `brew install dimpurr/tap/chat-stasher` on a clean Mac, which must install
   the version just released.

   `brew test dimpurr/tap/chat-stasher` finishes that test. `brew test`
   installs nothing — it runs the `test do` block of a formula that is already
   installed — so run it after the `brew install` above. The block asserts that
   the binary in the Cellar reports the version the URLs pin. Neither `brew
   audit` nor `brew style` runs the formula's `test do` block, so the two lint
   checks above cannot see a formula that installs but reports the wrong
   version. This one can.
10. **Move `main` forward**: bump `crates/chat-stasher/Cargo.toml` to the next
    development version (`X.Y.(Z+1)-dev` or `X.(Y+1).0-dev`) in a new commit.
    `main` never sits on an unsuffixed version.

If a release is wrong, do not delete the tag and re-push it: an installer that
already ran will have the old binary, and `install.sh` pins by tag. Cut the
next version instead.

## When to cut a release candidate

An rc is not a second release. It is this release, published earlier under
a tag that says what it is, because some of what a release changes cannot
be taken back afterwards. Two kinds of change qualify:

- **A minor or major bump.** The number is a claim that something changed
  shape — `0.5.0` after `0.4.0`, or `1.0.0` after any `0.x` — and it is the
  only place the claim is recorded. A candidate is where it meets a real
  install before it becomes permanent.
- **A release that changes the release workflow, the packaging, or the
  distribution channels.** The version number says nothing about any of
  these, yet they are how every later release reaches its users: a
  patch-sized change here can be the only part of a release that nothing has
  ever tested. A candidate exercises it in a real publication while the
  version is still one that nothing installs by default.

A patch release that only fixes bugs may be tagged stable directly: it is the
case the `Channels` table has in mind when it calls the rc channel optional.
Neither of the two kinds above may.

The candidate decides nothing by itself. The stable tag is cut from the code
the candidate was cut from, and only after the candidate has passed "Verify the
published release" below in full — the seven assets and their `SHA256SUMS`
exist only once a Release has been published, so a candidate is the first
moment that list can be run against real artifacts at all. A candidate that
fails it is not repaired into a stable tag: `main` takes the fix, and the next
candidate is `rc.N+1`, because the tag that failed is installed wherever it was
tried. "How a release candidate works" below has the mechanics; this section is
the rule for when they apply.

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

**Stable releases ship the CLI binaries and the stable-channel extension zip.**
The extension zip is named `chat-stasher-extension-X.Y.Z.zip`, using the
extension's own `package.json` version independently of the CLI version. The
release workflow builds it with `CS_RELEASE_CHANNEL` unset, checks that its
manifest has no experimental origins, includes its checksum, and asserts the
exact seven-file asset set.

Chrome Web Store submission is a separate manual owner step; publishing a GitHub
Release does not submit or publish the extension to a browser store.

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

## Registry credentials

The registry job uses the `release` GitHub Actions environment, and each
registry accepts exactly one of two credentials. The job uses the token secret
when the environment has one and GitHub OIDC trusted publishing when it does
not, so **both are supported at once and neither is required**.

- **The bootstrap token.** `NPM_TOKEN` for npm, `CARGO_REGISTRY_TOKEN` for
  crates.io, configured as secrets on the `release` environment. A trusted
  publisher can only be configured for a package that already exists, so the
  first publish of each registry has to use one.
- **Trusted publishing (OIDC).** With no secret configured, the job asks GitHub
  for an OIDC token and trades it for a short-lived publish credential: npm does
  the exchange itself once the `_authToken` line `actions/setup-node` writes has
  been removed from its `.npmrc`, and crates.io goes through
  `rust-lang/crates-io-auth-action`. npm's client for this is npm 11.5.1 or
  later, so the job installs a pinned npm rather than using the one Node bundles.

So removing the token is part of the sequence, not an optional cleanup — and it
is why neither registry step demands the secret. After each registry's first
successful publish, configure trusted publishing for `dimpurr/chat-stasher`,
workflow `.github/workflows/release.yml`, environment `release` (both registries
match on all three, filename included), then delete that token from the
environment's secrets. The next release runs on OIDC alone.

npm packages are published in platform-first order and each publish carries
`--provenance`; the crate follows with `cargo publish --locked`. A retry checks
the exact package version and skips one that is already present, while
continuing with later packages.

### Two things a credential does not cover

Both of these are registry behaviour the workflow has to work around after it
has authenticated successfully. Neither is a credentials problem, and both cost
a release before they were written down.

**crates.io refuses a request that does not identify itself.** Its API enforces
a data-access policy (<https://crates.io/data-access>) and answers `403` to a
request whose `User-Agent` names only the HTTP client library — which is
exactly what `curl` sends by default. The version lookup that decides whether
the crate is already published was such a request, so it got a `403` instead of
a `404` and stopped the publish of v0.5.0-rc.2 (run 36137101318) with
`crates.io version lookup returned HTTP 403`. The lookup is now
`scripts/crates-version-state.sh`, which sends
`User-Agent: chat-stasher-release (https://github.com/dimpurr/chat-stasher)` —
the "identify your bot, and include contact information" shape the policy asks
for — and which treats only `200` and `404` as answers. Any other status,
including `403`, `429` and every `5xx`, means the question was **not** answered
and the run stops. That distinction is the point: a `403` read as "not
published yet" is a publish, and publishing over an existing version cannot be
undone.

**npm sets `latest` on a package's first publish.** `npm publish --tag next`
sets `next`, and also sets `latest` — the npm CLI docs say it sets `latest` only
"unless the `--tag` option is used", and on a first publish that is not what the
registry does. `latest` is what `npm install chat-stasher` resolves, so a
candidate published this way becomes the version a plain install gets. Measured
on v0.5.0-rc.2 (2026-09-25): all six packages came back with
`latest = 0.5.0-rc.2` and exactly one version each. The publish step therefore
runs `scripts/npm-latest-tag.sh` afterwards, whose rule is the invariant
**`latest` must name the newest published stable version**: it re-points
`latest` at that version when one exists (`npm dist-tag add <pkg>@<version>
latest`), and when none exists it writes a notice to the run's job summary
saying that `latest` temporarily names the candidate and that the first stable
release will correct it.

Re-pointing `latest` needs a credential that may write dist-tags, so the repair
runs inside the publish step, where the npm credential has already been
resolved — not as a separate step that would have to resolve it a second time.
A `dry_run` publishes nothing and therefore repairs nothing.

## What the workflow checks

A rule here is worth writing down only if something can tell when it is broken,
so this is the list of what `release.yml` refuses — and, at the end, the one
thing it does not.

- **That the ref is a tag at all.** Before anything else, because nothing below
  means anything about a branch. A `workflow_dispatch` run can be started
  against a branch, and a branch may be named exactly like a release tag, so the
  gate reads the full ref and ends the run unless it is under `refs/tags/`. The
  publishing job asserts the same condition in its own `if:`, so a later edit to
  the gate cannot hand the job that holds the registry secrets a branch.
  (`scripts/release-tag-gate.sh` — the gate is a script so that this refusal can
  be exercised without pushing a tag; `scripts/selftest-release-tag-gate.sh`.)
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
  `chat-stasher-darwin-x86_64`, `chat-stasher-linux-x86_64`,
  `chat-stasher-linux-arm64`, `chat-stasher-windows-x86_64.exe`,
  `chat-stasher-extension-X.Y.Z.zip` and `SHA256SUMS` reach the Release. The zip
  is built on the stable channel and its manifest is rejected if an experimental
  origin is present.
- **That each binary is the binary it says it is.** Each build job checks its
  own output before uploading: the macOS job asserts each Mach-O's architecture
  with `file`, each Linux job asserts the file names the architecture it should,
  has no `PT_INTERP` program header and no `DT_NEEDED` dynamic entry, and runs,
  and the Windows job runs the `.exe`. The Linux half is
  `scripts/check-static-binary.sh`, called with each matrix cell's own machine
  string so both architectures get identical logic, and driven locally by
  `scripts/selftest-check-static-binary.sh` against recorded tool output. It
  does not read `file`'s wording as the verdict: a static PIE is an `ET_DYN`
  image that keeps a dynamic section for self-relocation, and `file` calls it
  `static-pie linked` rather than `statically linked` — so a rule that accepted
  only the second spelling refused a static binary, and took `v0.5.0-rc.1` with
  it. The same applies to `ldd`, which exits 0 on such an image; the gate asks
  the headers instead.
- **The uploaded asset set on a re-run.** Re-running the job for a tag whose
  Release already exists replaces the assets rather than failing on them or
  adding to them: every asset the run does not stage is deleted first, the seven
  staged files are uploaded with `--clobber`, and the workflow fails unless the
  Release's asset set then equals the staged set exactly. First publication is
  unaffected — `gh release create` starts from nothing.
- **The Homebrew tap job does not gate the release.** It runs only after the
  Release is published, only for a stable `vX.Y.Z` tag, and only when
  `TAP_TOKEN` is set; otherwise it is skipped. It opens a draft PR from a
  `chat-stasher-X.Y.Z` branch and stops — it never merges, so a tap that is
  broken cannot make a release fail, and a release cannot merge to the tap
  without the owner's review (step 9). A pull request for that branch is never
  opened twice: an existing one, open or closed, is left alone and reported.
  A failure inside the job still marks the workflow *run* red — the Release
  itself is already published at that point, so that red is a report on the tap
  update, not a failed release.
- **The package registries publish in order.** After the Release and Homebrew
  job finish, the workflow assembles npm packages from the Release assets,
  verifies their checksums, publishes each platform package before the launcher
  with npm provenance, then publishes the crate. Existing exact versions are
  skipped; failed lookups other than a confirmed not-found stop the job. An rc
  publishes under npm's `next` dist-tag and a stable release under `latest` —
  the tag shape decides which, and the publish step passes it through rather
  than choosing. A `workflow_dispatch` run can dry-run both registries or retry
  publication.
- **The crate's version lookup identifies itself to crates.io.** The lookup is
  `scripts/crates-version-state.sh`, which sends a `User-Agent` naming this
  project and its URL, and which accepts only `200` ("already published") and
  `404` ("not published yet") as answers. Every other status — a `403` from
  crates.io's data-access policy, a `429`, a `5xx`, a transport failure — exits
  `3`, which means *the question was not answered*: the step stops rather than
  proceeding, because the branch that proceeds publishes and a version cannot
  be unpublished. The gate is a script so this can be exercised without a
  registry: `scripts/selftest-crates-version-state.sh` drives it with `curl`
  shimmed, including the recorded `403` body, and fails if the request stops
  carrying a descriptive `User-Agent` or starts using `curl -f`.
- **npm's `latest` dist-tag names the newest stable version.**
  `scripts/npm-latest-tag.sh` runs after the npm publishes and enforces one
  invariant: `latest` must name the newest *published stable* version. If such a
  version exists it re-points `latest` at it — from a candidate, from an older
  stable, or from no tag at all; if none does, it writes a notice to the job
  summary instead, a notice rather than a failure, because on a package's first
  release there is genuinely nowhere else to point it.
  `scripts/selftest-npm-latest-tag.sh` drives the same script against a shimmed
  `npm`, including the three ways this can be wrong in the *wrong direction*:
  re-pointing at a lexicographically-"larger" older version, re-pointing when
  `latest` was already correct, and leaving an older stable in place because the
  test asked only whether `latest` was a prerelease.
- **Development versions are refused.** Registry steps reject any version
  containing `-dev`; the tag gate also accepts only `vX.Y.Z` and `vX.Y.Z-rc.N`.

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

That verification is the workflow's first job, and every build job declares
`needs: gates`, so the dependency is an edge GitHub enforces rather than an
ordering by convention. The job that publishes the Release reaches it only
through those jobs; it cannot start beside them. Which of the two tag shapes
this run is was decided in the gates job and is carried to the publishing job as
a job output, so the shapes are still read from one place.

One consequence worth knowing when reading `release.yml`: nothing is built
until the gates have passed, and the tag-shape refusal happens before even the
gates — a tag that is not `vX.Y.Z` or `vX.Y.Z-rc.N` ends the run in seconds.
