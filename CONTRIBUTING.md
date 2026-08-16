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

## Local checks

From the repository root, run:

```sh
cargo fmt --check
cargo build
cargo test
bash scripts/release-gate.sh
```

The release gate expects the debug binary at `target/debug/chat-stasher`, so
the build must pass first. The gate prints only privacy-preserving summary
fields and should end with `GATE: PASS` and exit 0 on the happy path.

The negative check is also useful when changing verification logic:

```sh
bash scripts/release-gate.sh --selftest
```

This command intentionally corrupts a temporary staging shard. Its expected
result is `GATE: FAIL` with a non-zero exit status; that is a successful
self-test, not a successful release gate.

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
正文, a real account, or a key.
