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
bash scripts/release-gate.sh
```

`test-reload-extension.sh` drives `reload-extension.sh` against a throwaway temp
repository and a stub build command, so it needs no extension toolchain, no
network and no browser. It is the guard for the reload script's mechanics, which
is why it sits here rather than only in the section below that describes them.

The browser extension is a second project with its own toolchain. Its checks are
the same three CI runs for it, and they must exit 0 too:

```sh
cd apps/extension && pnpm -s compile && pnpm -s test && pnpm -s build
```

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
`--recover`, which moves `.prev` back. The one step it cannot take for you — the
browser offers no supported API for it — it prints: toggle the extension off and
on in chrome://extensions, then reload the platform tabs.

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
visibility and licensing remain owner decisions.

## Pull requests

Describe:

1. the user-visible or data-integrity behavior that changed;
2. the checks you ran and their expected result;
3. any fixture, schema, compatibility, or migration impact; and
4. whether the change touches a privacy boundary.

If a report needs to refer to a sensitive failure, provide counts, byte sizes,
timestamps, hashes, or redacted identifiers only. Never paste conversation
text, a real account, or a key.
