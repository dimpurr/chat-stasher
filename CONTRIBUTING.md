# Contributing to chat-stasher

Thank you for helping build an append-only conversation archive. This project
handles user conversation data, so a contribution must be safe to review and
safe to reproduce without access to anyone's private conversations.

## Before opening a change

- Do not commit local notes, session files, real conversation data, credentials,
  account identifiers, or machine-specific absolute paths.
- Use synthetic JSONL when a fixture is needed. Keep fixture output opaque and
  bounded; test logs may contain counts, byte sizes, timestamps, session ID
  prefixes, and SHA-256 prefixes, but not conversation text.
- Keep the append-only invariant: deleting or rotating a source must not delete
  already archived data. Changes that weaken integrity verification or silently
  turn the archive into mirror-sync are not acceptable.
- Keep changes focused, explain observable behavior, and add or update tests
  for behavior changes. Documentation-only changes should still state what a
  new user can verify locally.

## Commit messages

The language invariant the rest of this section enforces is stated once, in
`CLAUDE.md` (the "Language" section): the public surface is English, with one
exception. That file is the single source for it; this section is how it is
enforced.

Commit messages are English, like the rest of the public surface: an imperative
subject line, and a body that explains why the change is worth making. The diff
already says what it does; the message is the only place the reason survives.

This is checked by one script, `scripts/check-commit-messages.py`, in two places:
CI checks every commit a push adds, and a `commit-msg` hook can check yours before
the commit exists. To enable the hook, once per clone:

```sh
git config core.hooksPath scripts/hooks
```

Git does not read hooks out of a checkout by itself, and nothing in this
repository runs that command for you, so a fresh clone is unfiltered until you
do. The Chinese in `apps/extension/locales/zh_CN.yml` is the single exception,
and it is the same one `check-terminology.py` encodes as T5.

## Local checks

From the repository root, run:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D clippy::let_underscore_must_use
cargo clippy --lib --bins -- -D clippy::unwrap_used
bash scripts/dev/check-test-cache-isolation.sh -- \
  bash scripts/dev/check-test-isolation.sh -- cargo test
python3 scripts/check-semantic-defaults.py
```

The fourth line **is** the test suite, run once through two isolation guards:
the rustic-cache guard (W289) on the outside and the user-data guard (W306)
inside it. The cache guard snapshots this machine's real user cache root
(`~/Library/Caches/rustic` on macOS, `$XDG_CACHE_HOME` or `~/.cache/rustic` on
Linux, `%LOCALAPPDATA%\rustic` on Windows) before and after the suite and exits
non-zero if the run created, removed or renamed anything there. That invariant
exists because the suite once opened its repositories with rustic's default
cache settings, and every such open planted a per-repository directory in the
user's cache — 36,174 had accumulated on one machine before W289, which the
cache-walking tests then had to sweep and which only
`scripts/dev/prune-test-rustic-cache.sh` can safely reclaim. So: every test
that opens a repository points its cache at a directory under its own temp
fixture — `StoreConfig::cache_dir` for an in-process open, and
`CHAT_STASHER_RUSTIC_CACHE_DIR` (the product's runtime override for
`rustic_cache_dir`, set through `tests`' shared fixture) for a spawned child —
and the cache itself stays enabled, only its location moves. That guard turns a
regression back into a red run instead of a slow machine. On the first run after
a large accumulation the two snapshots cost a directory walk each; the prune
script is the way to make that cheap again. On Windows the guard watches
`%LOCALAPPDATA%\rustic` (the Known Folder, which no environment variable
redirects, which is why the relocation travels through the product's own
`rustic_cache_dir` knob there rather than through `XDG_CACHE_HOME`), and refuses
rather than running unguarded when the root cannot be named.

The inner guard (`scripts/dev/check-test-isolation.sh`, W306) extends the same
idea to the rest of the machine's real state, because the cache pin turned out
to be one directory too narrow: on 2026-10-02 a test run resolved the real
`$XDG_DATA_HOME/chat-stasher` and planted
`stage/sessions/<machine>/chatgpt.synthetic-session` and a `synthetic-install`
row in the real `state/extension-coordination.sqlite3`.

It asks *what a change carries*, not *where it landed*. The machine that runs
this check also runs the live product, which writes the real
`stage/ext-status/…` every few minutes, so a guard that reds on any change to
the data root does not survive contact with its own purpose. Instead, after the
run, every entry the run created or modified under the real data, config,
cache and state-home roots is inspected, and is a leak when its name carries a
reserved fixture token (`synthetic`, `fixture`, `probe`, `dummy`,
case-insensitive, as a whole token) or a per-run marker, when a small file's
bytes carry the marker, or when a small file under `<data root>/state` carries
a fixture token — the coordination store is where the incident's row landed,
and it holds no conversation text for the scan to mistake for one. That last
rule matches the token as a case-insensitive **substring**, not a whole token,
because a store is not delimited text: SQLite lays a record's columns down with
no separator, so the incident's row is the bytes `chatgptsynthetic-install` and
a boundary rule cannot see the `synthetic` glued to the `chatgpt` before it.
Both rules subtract a **per-run baseline** — the fixture tokens already in the
state store and the fixture-named paths already under the watched roots, read
before the command starts. The machine that runs the check may carry debris
from an earlier leak (the incident's `chatgpt.synthetic-session` shard is still
in the real stage), and the live product rewrites the files that hold it, so a
write that only re-touches pre-existing debris is green while an entry or token
the run *introduces* is still red. The marker is a random token the guard
exports for one run; the shared `Sandbox` fixture in `src/test_support.rs`
names its temp root after it, so a value a test derives from its sandbox
carries the marker even when the write lands in a real root. A fixture-named
entry the run *removed* is a leak too. The inbox and every
`NativeMessagingHosts` directory the machine has (read from the filesystem, so
a browser directory that appears during the run is itself a diff) keep the old
whole-snapshot rule, because nothing writes them during a run: any change
there, including a create-then-delete caught by the run boundary, is red.

Every test still gets its environment from that `Sandbox` fixture, which points
`HOME`, `USERPROFILE`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`,
`XDG_CACHE_HOME` and the rustic-cache pin inside one temp root; the
native-messaging half is covered by tests passing an explicit `--target-root`,
because Windows resolves those directories through the Known Folder API and no
environment variable moves them. The guard is the check; a second, code-level
fail-safe (`src/test_identity_guard.rs`) refuses to write a reserved fixture
identity (`synthetic-…`, `fixture-…`, `probe-…`, `dummy-…`) to a destination
outside the process temp directory at all, so a test that loses its sandbox is
stopped at the write even if the guard is not the thing running it.

The guard's self-test is `bash scripts/dev/test-test-isolation-guard.sh`; the
code-level fail-safe's is `cargo test -p chat-stasher --test
w306_test_isolation_test` (the ordinary suite runs it too).

The remaining checks — source-text gates, script self-tests, the release gate
and the smoke — follow:

```sh
python3 scripts/check-terminology.py
python3 scripts/check-citation-drift.py
python3 -m unittest scripts/tests/test_check_citation_drift.py
python3 -m unittest scripts/tests/test_user_strings.py
python3 scripts/output-inventory.py --check
python3 scripts/check-support-matrix.py
python3 scripts/check-support-matrix.py --selftest
python3 scripts/takeout-format-inventory.py --selftest
python3 scripts/tests/test_takeout_format_inventory.py
python3 scripts/check-commit-messages.py --selftest
python3 scripts/check-doc-links.py
python3 scripts/check-doc-links.py --selftest
python3 scripts/check-private-paths.py
python3 scripts/check-private-paths.py --selftest
bash scripts/check-workflows.sh
bash scripts/check-workflows.sh --selftest
bash scripts/selftest-release-tag-gate.sh
bash scripts/selftest-check-static-binary.sh
bash scripts/selftest-crates-version-state.sh
bash scripts/selftest-npm-latest-tag.sh
bash scripts/dev/test-reload-extension.sh
bash scripts/dev/test-cdp-keep-platform-tabs.sh
bash scripts/dev/test-rebase-onto-main.sh
bash scripts/dev/test-merge-drivers.sh
bash scripts/dev/test-cache-isolation-guard.sh
bash scripts/dev/test-test-isolation-guard.sh
bash scripts/dev/test-prune-rustic-cache.sh
python3 scripts/dev/scoreboard.py --selftest
bash scripts/selftest-relocate-citations.sh
bash scripts/self-test-install.sh
node --test npm/test/*.test.mjs
bash scripts/release-gate.sh
bash scripts/smoke/linux-smoke.sh
```

`test-reload-extension.sh` drives `reload-extension.sh` against a throwaway temp
repository and a stub build command, so it needs no extension toolchain, no
network and no browser. It is the guard for the reload script's mechanics, which
is why it sits here rather than only in the section below that describes them.
The cases from 16 on are the guard for which `node_modules` the build runs from:
a checkout whose install cannot serve the ref being built must trigger an
install *inside the throwaway worktree* and must not be written to, because the
checkout it was invoked in may be the tree the developer's own browser loads
from.

`test-cdp-keep-platform-tabs.sh` is the same shape for
`scripts/dev/cdp-keep-platform-tabs.mjs`, the opt-in dev helper that keeps the
platform tabs the backfill leg needs open in a dedicated CDP test browser. It
runs against a mock DevTools endpoint built into the test file — HTTP plus a
minimal WebSocket handshake, no browser and no network — and that mock records
every `Page.reload` with its own timestamp, which is what makes the two
properties worth asserting assertable without driving a real Chrome: only an
exact-host allowlist is reloaded, and the reloads are spaced. Both are the
properties that keep the helper from being a way to hammer someone's browser.

`test-rebase-onto-main.sh` and `test-merge-drivers.sh` are the pair for the
derived-file merge rule described under "Resolving a merge" below. Both build
throwaway repositories, so they need no history and cannot leave this tree dirty.
The first drives `rebase-onto-main.sh` through a citation-only conflict, a prose
refusal, a code-conflict rollback, a rebase with the driver registered, and the
textual fallback a clone without the driver gets; the second pins what git itself
does with the drivers — under `merge` *and* under `rebase` — and asserts both
states of a fresh clone, so a driver that stopped being consulted cannot pass as a
fixture that happened not to conflict. Cases 4 and 5 of the rebase test are the
only checks anywhere that run the tool's regeneration step against the real
`scripts/output-inventory.py` rather than a stub, and they judge the committed
file with the generator's own `--check`, so a regeneration that silently stopped
happening goes red here instead of committing a stale recording.

`scripts/dev/scoreboard.py --selftest` is the scoreboard generator's own suite.
It builds every input it judges — synthetic ext-status reports, overview
snapshots, oracle results, editorial fields — under a temp directory, so it
needs no archive, no private data and no network. It is the only check that
holds the board's rule set: an unavailable source is never rendered as a zero,
pending is never summed across installs, unknown stays distinct from "not
there", and a freshness rule whose input is missing says so instead of passing
silently. Nothing else in this list exercises those rules, which is exactly why
the selftest must run.

`self-test-install.sh` is the same shape for the installer: it serves a mock
Release over `file://` and shadows `uname` on `PATH`, so the platform branches
the host cannot reach on its own — both Linux architectures, the Windows
refusal, and the Intel macOS artifact — are exercised with no network call. That
shadowing is why it is here rather than left to one machine's own architecture:
without it, the branch that decides a platform's artifact would only ever run
for the platform the suite happens to start on.

It runs every case twice, under `bash` and under `dash`, and lints the installer
with `shellcheck --shell=sh`. That is not thoroughness for its own sake.
`install.sh` is documented as `curl -fsSL … | sh`, and on Debian and Ubuntu `sh`
is dash, where `set -o pipefail` is an illegal option — so the installer was
broken for exactly the users it was written for, and this suite was green the
whole time because every case invoked `bash`. `/bin/sh` is not a second opinion
on macOS, where it is bash under another name and accepts the same bashism.
`shellcheck` needs `--shell=sh` for the same reason: without it the linter
assumes bash and passes the construct. Both are skipped in as many words when
the tool is missing locally, and CI asserts both are present so the skip cannot
become the normal case in the one place meant to catch it.

`check-workflows.sh` is the only check here that reads the workflows
themselves. GitHub is the first thing that parses a workflow, and for
`release.yml` that parse happens while a release is being published with the
`release` environment's secrets in scope — so the four files that decide what
this project publishes had no check at all before it. It runs a pinned
actionlint: the one on `PATH` if there is one, otherwise a download whose
sha256 is pinned in the script, and it skips in as many words when it can get
neither. CI passes `--require`, so that skip cannot become the normal case in
the one place meant to catch it. Integration with shellcheck is deliberately
off — actionlint would run whatever shellcheck the machine happens to have
(0.9.0 on ubuntu-24.04, 0.11.0 upstream), which would make the verdict depend on
the runner rather than on the pinned version.

`selftest-release-tag-gate.sh` drives the gate `release.yml` calls before it
builds anything. That gate is a script rather than a step's here-doc for exactly
this reason: `scripts/release-tag-gate.sh` is both what runs and what is tested,
so the refusals that matter — a ref that is not a tag, a tag that is not one of
the two release shapes, a tag that disagrees with `Cargo.toml` — can be
exercised without pushing a tag. `scripts/commit-message-range.sh` is the same
arrangement for the same reason: a second copy of a gate drifts from the copy
under test.

`selftest-check-static-binary.sh` is that arrangement for the other gate
`release.yml` calls: the one that decides whether a Linux asset is really static,
which `scripts/check-static-binary.sh` owns. Its probes are recorded `file`
and `readelf` output with those two tools shimmed on `PATH`, because the host
this suite runs on is a Mac that has neither and cannot read a Linux ELF at all —
and because the failure that motivated the check was a *string*: a binary that
was static, reported by `file` as `static-pie linked`, refused by a test that
knew only `statically linked`. Two of its probes therefore hand the gate a
`file` sentence and ELF headers that disagree, and require it to believe the
headers. A check that can only be exercised on Linux is a check nobody can
reproduce before pushing; the Linux artifacts are built in CI and cannot be built
here.

`selftest-crates-version-state.sh` and `selftest-npm-latest-tag.sh` are the same
arrangement for the two registry steps `release.yml` runs last, and they are
here for the reason the whole list is: both steps are where a release stops
being reversible. The first asks crates.io whether an exact crate version is
already published, and its probes shim `curl` so the answer can be any status —
the recorded `403` from crates.io's data-access policy above all, because the
branch that reads a `403` as "not published yet" is the branch that publishes
over a version that cannot be unpublished. The second re-points npm's `latest`
dist-tag, and its probes shim `npm` so no real dist-tag is touched; two of them
exist to catch a *wrong* answer that looks like a successful repair, which is
what a lexicographic version comparison produces (0.9.0 over 0.10.0). Neither
needs a network, a registry account, or a published package.

The npm launcher's tests need no toolchain either: they run against a fake
platform package built in a temp directory, and they force the platform they
examine rather than reading the machine's, so the macOS paths are covered on any
host. Node 18 is the floor `npm/package.json` declares, so that is what CI uses.

The browser extension is a second project with its own toolchain. Its checks are
the same CI runs for it, and they must exit 0 too:

```sh
cd apps/extension && pnpm -s compile && pnpm -s test && pnpm -s build
```

Its end-to-end suite is the fourth of those runs. It needs a browser download
that the three above do not, so it is a separate command rather than part of
that line:

```sh
cd apps/extension && npx playwright install chromium   # once per machine
cd apps/extension && pnpm e2e                          # builds first, then runs e2e/
```

Two properties of that suite are worth knowing before changing it, because both
are easy to break while making it pass:

- **Nothing leaves the machine.** The specs intercept the platform's own origins
  and serve a fake page and a hand-written response there; every request that is
  not served by a fixture is aborted and counted, and each spec asserts that
  count is 0. There is no test account, no credential, and no live site — so a
  contribution that needs a logged-in session to reproduce is not a contribution
  to this suite.
- **It runs headless.** Playwright's default headless build
  (`chromium-headless-shell`, what `headless: true` without a channel selects)
  loads no extensions at all. The suite passes `channel: 'chromium'`, which
  selects the full browser with the new headless mode, and that is why CI needs
  no `xvfb`. Removing that channel makes the whole suite fail at launch rather
  than silently test nothing.

One spec goes further and needs a second toolchain, so it has a second command:

```sh
cd apps/extension && pnpm e2e:multi   # builds the native host, then runs one spec
```

`e2e/multi-install.spec.ts` drives two persistent profiles against **one real
`chat-stasher` native host** — the binary, one process per request, over the
frame format in `crates/chat-stasher/src/nativehost.rs` — so it needs
`cargo build` as well as the browser. It is kept out of `pnpm e2e` rather than
added to it, because a compiler is a much larger thing to require of the command
someone runs for an extension-only change. `e2e/playwright.multi.config.ts` and
`apps/extension/package.json` say the same thing from the other side, and CI runs
it in its own job on the tiers that can afford a build. The harness starts the
host itself and `e2e/harness.ts` (`NativeHost`) documents why the browser cannot
— measured: Chromium resolves its native-messaging directory from the OS home,
not from `$HOME` — and exactly which parts of that arrangement are still real.

Every one of these must exit 0. They are the same checks CI runs, listed here
so that a green local run means a green pull request; if this list and CI ever
disagree, that is a bug in this document.

The release gate builds `target/debug/chat-stasher` if it is missing and
generates its own synthetic fixtures, so it needs no arguments and no setup. It
prints only privacy-preserving summary fields and should end with `GATE: PASS`
and exit 0 on the happy path.

The smoke test does the same for the *installed* CLI: it builds the binary if it
is missing, plants synthetic histories for every harness the shipped registry can
be seeded for, and drives `doctor` → `init` → `run-once` → `read` → `overview` →
`schedule` against a throwaway HOME. It needs no arguments, touches no network
and should end with `SMOKE: PASS` and exit 0. It runs wherever bash and python3
do; the CI job that carries it alongside `release-gate.sh` is the ubuntu cell, so
a Windows contributor is not expected to run this one (which is a stated gap, not
a silent skip). `--platform <name>` is a development aid that replays a foreign
platform's registry cells locally; CI never passes it.

Seven of these checks guard properties that are easy to break without noticing:

- `check-semantic-defaults.py` requires a `// reason:` note wherever production
  code turns an unknown into a concrete value (`unwrap_or(0)` and friends). The
  rule is not "avoid defaults" but "say why this default is honest".
- `check-terminology.py` keeps one word to one meaning in user-visible strings,
  and keeps absence, read failure, and unknown from being worded as each other.
- `check-citation-drift.py` verifies that every `file:line` citation in the
  documentation still points at the content it was written about. Re-locate the
  citation and confirm the sentence still holds before running `--update`;
  updating the lockfile without reading the code defeats the check.
- `output-inventory.py --check` pins the inventory of user-visible strings, so
  a change to what the tool says is visible in review rather than incidental.
- `check-support-matrix.py` re-derives the support tables from the harness
  registry and the extension's platform table, and fails when either source
  moved without the committed tables being re-derived. It never decides whether
  a row's claim is true — only whether the table still matches its inputs.
- `check-doc-links.py` resolves every link between our own Markdown files,
  including the `#fragment` on each one. A link to a file that moved, or to a
  heading that was renamed under it, renders perfectly and fails silently, and
  no other check in this list reads a document for its links. External links are
  counted, never fetched: a gate that needs the network fails when the network
  does, and someone else's 404 is not a fact about this repository.
- `check-private-paths.py` fails when a tracked file names a private working
  directory, or an absolute path into one. A path is not a citation a reader can
  follow: it points at material that was never published, and an absolute one
  also publishes the shape of the machine that wrote it. Name the document, not
  the path to it.

The negative check is also useful when changing verification logic:

```sh
bash scripts/release-gate.sh --selftest
```

This command intentionally corrupts a temporary staging shard. Its expected
result is `GATE: FAIL` with a non-zero exit status; that is a successful
self-test, not a successful release gate.

### Resolving a merge

A branch that touches code moves line numbers, so a merge conflicts on README.md,
the documents under `docs-dev/`, and `docs-dev/citations.lock`. Only the prose is a
judgement call; the citation numbers are mechanical and `scripts/relocate-citations.py`
does them. Take both sides' prose, then:

```sh
python3 scripts/relocate-citations.py --old <parentA> --old <parentB> --dry-run
python3 scripts/relocate-citations.py --old <parentA> --old <parentB>
python3 scripts/check-citation-drift.py
python3 scripts/check-citation-drift.py --update
```

Pass **every** parent of the merge, in one invocation. A resolved document keeps
citations from both sides, and a citation is read in the numbers of the side whose
own document writes that same range — so `--old <parentB>` cannot move a citation
that only A ever wrote down. A range no declared side writes is reported, not
relocated.

Run it **once**, on the freshly resolved document. Afterwards the documents carry
the working tree's numbers, so "these numbers are side A's" is no longer true of
them and the next run refuses rather than relocating a second time. That refusal is
the tool working: re-reading a relocated citation in a parent's line numbers is how
a correct anchor gets moved onto an unrelated range.

A range is moved only when its old text is found in exactly one place, so a run that
reports `REFUSE` needs a human — the cited text was rewritten, or it now occurs more
than once, and either way the sentence has to be re-read against the code. A range
reported `GROWN` was relocated with lines inserted inside it: read those lines
before `--update` and confirm they belong to the claim.

`--update` locks in whatever the documents now say. Running it without reading the
failures defeats the check; the whole point of the sequence above is that the drift
check gets a chance to be red first.

For a branch whose documentation changes are citation coordinates only, use
`bash scripts/dev/rebase-onto-main.sh [--onto <ref>]`. It refuses a dirty
worktree, preserves the original SHA if rebasing or checking fails, and takes
the onto side for conflicts in `README.md`, `docs-dev/*.md`, and
`docs-dev/citations.lock`. Before rebasing, it compares the branch's changed
Markdown with the onto version after normalizing citation line ranges; any
remaining difference is reported as prose to re-apply by hand. Code conflicts
abort and restore the original SHA. A relocation that needs a human leaves the
branch rebased and uncommitted so the reported citations can be reviewed. A
successful run commits `Relocate citations after rebasing onto main`.

Both derived files also carry a `merge=regenerate-*` driver (`.gitattributes`).
Git does not read drivers out of a checkout, so register them once per clone:

```sh
bash scripts/dev/setup-merge-drivers.sh
```

With the drivers registered, a merge or rebase never *stops* on the two
recordings: the driver keeps the current side, which is disposable by
construction — it is exactly what regeneration replaces. That makes re-deriving
them mandatory rather than a tidy-up, and it is why the step above is not
optional: the value the driver leaves committed is only correct once
`relocate-citations.py` (for `citations.lock`) and `output-inventory.py` (for
`output-inventory.txt`) have re-derived it from the merged tree. Landing a merge
by hand rather than with `rebase-onto-main.sh` is the same work — the flow above
— and the two gates are the alarm either way: `check-citation-drift.py` and
`output-inventory.py --check` compare the committed recordings against the tree
they claim to describe, so a skipped regeneration is red, never quietly stale. A
clone that never ran the setup gets git's ordinary textual conflict on these two
files instead, which is the conflict this section resolves by hand.

The workflow's throwaway-repository self-tests cover citation conflicts, prose
refusal, code-conflict rollback, and the derived files both with and without the
drivers:

```sh
bash scripts/dev/test-rebase-onto-main.sh
bash scripts/dev/test-merge-drivers.sh
```

## Reloading the extension during development

Chrome only re-reads a manifest when the version changes — its "Update" button
and the reload arrow do not re-inject content scripts. To exercise a build in a
real browser you therefore need to bump the version, and the whole cycle is
easy to botch by hand. `scripts/dev/reload-extension.sh` automates it:

```sh
bash scripts/dev/reload-extension.sh --load-dir /path/to/unpacked-load-dir
```

It builds the extension from a throwaway worktree of `HEAD` (so uncommitted
edits never leak into the build), appends the next build number as the 4th
version component, and swaps the result into `--load-dir`, keeping the previous
build as `<load-dir>.prev`. The swap is two renames, not one atomic step: the
build is staged in a sibling temp directory, the previous build is renamed aside
to `<load-dir>.prev`, and then the staged directory is renamed into place. Each
rename is atomic, so a half-copied build is never visible under `--load-dir`;
the pair is not, so for the instant between the two renames the load dir does
not exist. That window is not left for you to find — if the second rename fails
the script renames `.prev` back and exits non-zero, and if a run is interrupted
inside the window the next run refuses to touch the load dir and asks for
`--recover`, which moves `.prev` back. By default the reload of the running
extension is left manual, because the browser offers no supported API for it; the
script prints it: toggle the extension off and on in chrome://extensions, then
reload the platform tabs.

The worktree borrows the checkout's `node_modules` by symlink so the build needs
no install and no network — but only when that tree can serve the ref being
built. The check is against the *ref's* `apps/extension/package.json`, not the
checkout's, because the ref is what gets built: a ref whose dependencies moved
past the checkout's install is exactly the stale case. When a declared
dependency is not linked, the script says so by name and runs
`pnpm install --frozen-lockfile --prefer-offline` **inside the throwaway
worktree** — never in your checkout, which is very often the directory your own
browser loads its unpacked extension from. `--frozen-lockfile` is what keeps
that honest: a ref whose lockfile disagrees fails there rather than quietly
building a different dependency tree than the one committed.

That toggle is removable when Chrome was started with
`--remote-debugging-port`. Passing `--cdp-port <port>` makes the script find the
extension's service worker over the DevTools Protocol, call
`chrome.runtime.reload()`, and fail unless the worker comes back on the version
it just built. It needs `node` on PATH; without the flag nothing changes.

The build number comes from `--build-number`, else from the previous load
dir's 4th version component plus one, or 1. A load dir that does not look like
a previous build is refused; `--init` allows one that is absent or empty, so a
first build can seed an empty directory you created for it, but a non-empty
directory is never renamed aside — `--init` cannot be pointed at a projects or
home directory. `--dry-run` prints the plan and changes nothing. `--ref <ref>`
builds a ref other than `HEAD`. A plain manifest build can be given a build
number too:
`CS_BUILD_NUMBER=<n> pnpm -s build` in `apps/extension` appends it as the 4th
component and sets `version_name` to `<semver>+build.<n>`; without it the
manifest is byte-identical to a release build.

The mechanics are covered by a bash test you can run anywhere:

```sh
bash scripts/dev/test-reload-extension.sh
```

## What counts as acceptable

A change is acceptable when it preserves the documented data-safety contract,
has a reproducible check, does not require credentials, and does not put real
user data into the repository, CI logs, issues, or pull requests. New external
integration tests must be optional and must be skipped when their credentials
or services are unavailable; they must not make the local, credential-free
checks depend on a remote account.

Do not add a new release or publication action to a pull request. Repository
visibility and licensing remain owner decisions. The release model — the dev and
stable channels, who approves, and the exact steps to cut a release — is in
[`RELEASING.md`](RELEASING.md).

## Issues

An issue is the other public surface, and it is read by people who never agreed
to see anything about your setup. The same constraint as a pull request body
applies to it, and it fails in a way a pull request does not: an issue is
published the moment you press the button, and its edits stay in the API's
history, so a redaction that arrives an hour later is still a correction. Write
it redacted the first time.

Four things a public report must not carry, each because it says more than the
report needs:

- **Conversation content.** No message text, no conversation titles, no
  attachment or file names. Counts, byte sizes, timestamps, hashes and
  session-id prefixes are the substitute, and `docs-dev/privacy.md` lists exactly
  which fields exist to be quoted.
- **Another project by name.** Route shapes, field names and parameter names may
  be quoted as evidence — that is what the documents in this repository do — but
  not the project, author or store listing they were read out of. Naming a
  competitor in a public issue points the reader at that project instead of at
  the claim being made.
- **Your own machine.** No machine names, no absolute paths, no home-directory
  names, no account identifiers, no e-mail addresses, and no origin carrying a
  tenant or workspace identifier. A path that names your work directory names
  your employer.
- **A document the reader cannot open.** Do not cite a document unavailable to
  readers as the authority for a claim. State the fact, and where it is
  verifiable give the source readers can access. (A bare `ADR-nn` label in a
  published document — `contracts/nativehost-protocol.md` names one — is a
  naming convention rather than a citation: the difference is whether readers
  are asked to accept something on the word of a document they cannot read.)

A redacted report still has to be worth reading. Reduce the case to a synthetic
reproduction where you can, and say which facts you verified, which you assumed,
and which you could not check — in this repository "not found" and "does not
exist" are different answers, and a report that blurs them costs more than it
gives.

## Pull requests

Describe:

1. the user-visible or data-integrity behavior that changed;
2. the checks you ran and their expected result;
3. any fixture, schema, compatibility, or migration impact; and
4. whether the change touches a privacy boundary.

If a report needs to refer to a sensitive failure, provide counts, byte sizes,
timestamps, hashes, or redacted identifiers only. Never paste conversation
text, a real account, or a key.
