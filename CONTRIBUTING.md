# Contributing to chat-stasher

Thank you for helping build an append-only conversation archive. This project
handles user conversation data, so a contribution must be safe to review and
safe to reproduce without access to anyone's private conversations.

## Before opening a change

- Do not commit `.private/`, files copied from `~/.claude/projects/`, real
  conversation payloads, credentials, account identifiers, or machine-specific
  absolute paths.
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
cargo test
python3 scripts/check-semantic-defaults.py
python3 scripts/check-terminology.py
python3 scripts/check-citation-drift.py
python3 scripts/output-inventory.py --check
python3 scripts/check-commit-messages.py --selftest
bash scripts/dev/test-reload-extension.sh
bash scripts/selftest-relocate-citations.sh
bash scripts/release-gate.sh
```

`test-reload-extension.sh` drives `reload-extension.sh` against a throwaway temp
repository and a stub build command, so it needs no extension toolchain, no
network and no browser. It is the guard for the reload script's mechanics, which
is why it sits here rather than only in the section below that describes them.

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

Every one of these must exit 0. They are the same checks CI runs, listed here
so that a green local run means a green pull request; if this list and CI ever
disagree, that is a bug in this document.

The release gate builds `target/debug/chat-stasher` if it is missing and
generates its own synthetic fixtures, so it needs no arguments and no setup. It
prints only privacy-preserving summary fields and should end with `GATE: PASS`
and exit 0 on the happy path.

Four of these checks guard properties that are easy to break without noticing:

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

The negative check is also useful when changing verification logic:

```sh
bash scripts/release-gate.sh --selftest
```

This command intentionally corrupts a temporary staging shard. Its expected
result is `GATE: FAIL` with a non-zero exit status; that is a successful
self-test, not a successful release gate.

### Resolving a merge

A branch that touches code moves line numbers, so a merge conflicts on README.md,
the documents under `docs/`, and `docs/citations.lock`. Only the prose is a
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
the onto side for conflicts in `README.md`, `docs/*.md`, and
`docs/citations.lock`. Before rebasing, it compares the branch's changed
Markdown with the onto version after normalizing citation line ranges; any
remaining difference is reported as prose to re-apply by hand. Code conflicts
abort and restore the original SHA. A relocation that needs a human leaves the
branch rebased and uncommitted so the reported citations can be reviewed. A
successful run commits `Relocate citations after rebasing onto main`.

The workflow's throwaway-repository self-test covers citation conflicts,
prose refusal, and code-conflict rollback:

```sh
bash scripts/dev/test-rebase-onto-main.sh
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
  session-id prefixes are the substitute, and `docs/privacy.md` lists exactly
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
- **A document the reader cannot open.** Anything under `.private/` — an ADR, a
  plan, a worker report — is not published, so citing one as the authority for a
  claim leaves the reader nothing to check. State the fact, and where it is
  verifiable give the public source. (A bare `ADR-nn` label in a published
  document — `contracts/nativehost-protocol.md` names one — is a naming
  convention rather than a citation: the difference is whether the reader is
  asked to accept something on the word of a document they cannot read.)

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
