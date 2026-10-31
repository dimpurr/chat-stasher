DONE 0713471b9e05ac64d28f01f0e032681a666cd48a (Condition the read-race re-read claims on what the re-read found)
Branch `w1050-read-race` (one fix commit, replayed at 0713471b by the 2026-10-31 deputy rebase onto 89af630d — was eaa5bdbe on tip fcf2afd7 over base 7cbeab1c; no new branch). Revision verdict FIX-FIRST, all four findings (2 Medium + 2 Low) fixed plus the windows-only CI red; full text below. Deputy round state: rebased, recordings regenerated, force-pushed, CI dispatched; windows green to be read from the dispatched run.

# W1050f OUT — rev0 findings fixed, fixture's Windows spelling fixed

## The findings, as fixed

### Medium 1 — success note conditioned on the re-read's own flag (`readback.rs`)
`Ok((value, _))` discarded the second attempt's raced flag, so the gate printed
"[read-back] the re-read after the index reload succeeded" over a report-shaped
reader that still warned about not-in-index trees. Fix: `reread_once_on_index_race`
now binds `second_raced` and prints the note only through
`reread_resolved_without_miss(second_raced)` (a named `!second_raced` predicate, so
the decision is unit-pinnable while the literal stays at its `eprintln!` site for the
output inventory). A still-flagged second attempt prints nothing — the report's own
unknown vocabulary keeps the last word; the strict readers (always false-flag on the
re-read) still earn the note. Matching test: the existing flag-true→flag-true case
(pinned `(2, true)`) is extended with two asserts on the predicate, and the CLI
fixture's recovered case A now also requires `the re-read after the index reload
succeeded` on stderr, so the note is pinned end-to-end on the exact `read --session`
path the drivers use.
**Red-first**: mutation run flipping the predicate (`!second_raced` → `second_raced`)
reddens the unit test at exactly the new pin — `W1050f-mutation-M1-134915-22164.log`
rc=101 ("a re-read that answered without another miss is the shape the success note
exists for"); mutation reverted, green re-verified (`W1050f-lib-reback-134929-22588.log`).

### Medium 2 — visit re-runs disclosed + the two accumulators made retry-proof
The retry re-invokes the caller's `visit` callback for sessions already handed on the
raced attempt (the reloaded open cannot resume a half-finished walk; every attempt
re-opens the repository by construction).
- **Doc, done on every wrapper**: the gate's own doc now carries the sentence, and all
  three public wrappers disclose it — `for_each_archived_session`,
  `for_each_archived_session_with_policy`, and a full paragraph on the wrapper that owns
  the gate, `for_each_archived_session_shards_with_policy`, including the caller-side
  contract: the returned `CumulativeSessionRead` counts one attempt (the last), and
  caller-side counting must tolerate or guard the re-visit.
- **The two in-tree accumulators, both guarded** (reviewer's preferred shape, not
  doc-only):
  - `rebuild_activity_index_from_archive` (main.rs, `sessions_indexed`):
    a `sessions_answered: BTreeSet<String>` guard skips a re-handed session. This also
    fixes a consequence the review named only in passing: without the guard a retry
    would append the same session's row a second time into the derived-index temp file,
    so the guard protects the published file's one-row-per-session shape, not just the
    counter (`read_previous_activity_rows`'s dedup-at-read survives, but published
    duplicates were still a latent shape break — now they cannot exist).
  - `backfill_archive` (audit_store.rs, `BackfillReport` counters):
    a `backfilled_sessions` BTreeSet skips re-handed sessions. Verified by reading
    `message_audit::append_jsonl`: the sidecar commits reconcile by body-digest/position
    identity, so attempt-2 re-appends were already no-ops — only the counters were
    wrong; skipping preserves the (idempotent) writes and makes the counts true:
    one session reports as one scanned, not two.
  Both guards are no-ops on a clean walk — the only behavior change is under the race,
  and both rely on the verified fact that within one attempt a session id is visited
  at most once (the `held` BTreeMap walk). No dedicated CLI fixture exercises a mid-walk
  race on the *rebuild* paths (that would need a second sideline+rendezvous fixture
  inside an index-rebuild command); the guards are reasoning-verified and the wrapper
  doc now states the contract for future callers.

### Low 1 — env-mutating retry test serialized and leak-proofed
The retry-arithmetic test mutates process env (`CHAT_STASHER_TEST_INDEX_RACE_WAIT_MS`)
while the binary's other lib tests run concurrently. It now:
- takes a named static `READ_RACE_TEST_ENV_GUARD: std::sync::Mutex` (whose doc says
  any future lib test that drives a gated reader or touches either hook takes it first);
- clears the wait env through a Drop guard (`UnsetWaitEnvOnDrop`), so a failed assert
  can no longer leak the narrowed bound into the rest of the test process (the old
  trailing `remove_var` never ran on panic).
The old "nothing else can walk far enough to classify an index-miss" rationale is
replaced by the lock's — today's safety stays, tomorrow's collision is guarded.

### Low 2 — wait note no longer overclaims the cause
"this happens when a read races a push writing to the same archive" asserted the race
as THE cause for every I001–I003 miss; a genuinely corrupt archive got the same causal
wording before its 300 s stall and eventual rc=3. Reworded to name the race as the
usual cause and stop: "…loaded — the usual cause is a read racing a push writing to the
same archive, and an archive that genuinely lacks that tree in its index reads the same
from here; waiting {} s, …". Message structure, code path, and the reload vocabulary
unchanged; the one changed string is the inventory's only content delta (verified
below).

## The windows-only CI red (run 41192502437, `rust (windows-latest)` only)

**Diagnosis — fixture path handling, not push semantics.** The log pins it: the child
test panicked at 10:49:09Z at `tests\w1050_read_race_test.rs:238:9` with "A: the
fixture must have a parent snapshot before the second push". That assert fires BEFORE
the second push, and the *first* push had already succeeded (`Sandbox::new()` asserts
it and the panic came later), so the parent snapshot file existed. `relative_files`
renders repository paths via `strip_prefix` + `to_string_lossy`, so on Windows the
snapshot is spelled `snapshots\<hex>` — and the precondition looked for the literal
byte prefix `"snapshots/"`, which no Windows-rendered path can start with. The
macOS/ubuntu cells were green on the same product code, and the push/read semantics
need no Windows fix: the same class as the W1049 forward-slash-join, this time on the
fixture's diff side.

**Fix.** All five top-directory prefix checks (the precondition plus the
one-snapshot/one-index/≥1-pack counts and the sideline exclude) now go through
`in_top_dir(relative, top)`, which compares by top path component split on **both**
separator spellings and stays a whole-component match (`snapshots-x/<id>` is still no
snapshot; a bare `snapshots` segment alone is not a snapshot). Nothing the test proves
was weakened: the same states are asserted — recovery rc=0 with the gate note, still-
missing rc=3 `UNKNOWN` (no proof-of-absence wording), healthy-absent in the reserved
wording — with the *same* counts and digests, now matched platform-natively.
**Honest note**: my first helper draft used `Path::starts_with`, and the new pin test
caught its own non-portability locally — on a `/` platform a `"snapshots\x"` string is
one component, so the predicate was not fixture-agnostic; the split-both form is the
one that is, pinned by a dedicated test
`relative_top_dir_checks_match_both_separator_spellings` that runs on every platform
(that it reddened the draft first is why it can be believed now:
`W1050f-race-134742-15382.log` rc=101 → `W1050f-race2-134852-19609.log` rc=0, 2/2).
Remaining fixture steps after the old red point (sideline/restore renames, the
rendezvous poll, exit codes, product strings) were re-checked for separator
assumptions by grep — the five repaired sites were the only matches — but they have not
executed on real Windows before the old red point, so **windows green must be read
from CI after push**; the pin test plus the unchanged fixture logic is the local
evidence.

A fix-round observation, not fixed on purpose to keep scope: the branch-new fixture
carries a benign `dead_code` warning (struct field `root: tempfile::TempDir` never
read — it is held for its Drop; Windows log shows it at 10:36:04Z), pre-existing from
the impl commit, denied by no gate. Left for the maintainer's judgement
(`#[expect(dead_code, reason = …)]` would silence it).

## Gates (gate tool; rc recorded; all green on the committed tree)

- `W1050f-lib-readback` — `cargo test --lib readback` — **rc=0 PASS**, 9/9 incl. the
  extended retry-arithmetic test with the env guard (13:46, final code; the later
  mutation-revert pair re-ran the touched target green at 13:49)
- `W1050f-race` — `cargo test --test w1050_read_race_test` — rc=101 FAIL at 13:47: the
  new pin test caught the `Path::starts_with` draft (described above); helper fixed
- `W1050f-race2` — same gate — **rc=0 PASS**, 2/2 in 3.68 s (`read_race_reload_…` +
  the spelling pin)
- `W1050f-mutation-M1` — mutation run — rc=101 FAIL **by design**; reverted,
  `W1050f-lib-reback` rc=0 PASS
- `cargo fmt --check` — first run rc=1 (one chain rustfmt wanted compacted), `cargo
  fmt` applied, **`W1050f-fmt2` rc=0 PASS** at 13:46, re-run on the committed tree
  **`W1050f-fmt3` rc=0 PASS**
- `W1050f-clippy1` — `cargo clippy --all-targets -- -D clippy::let_underscore_must_use`
  — **rc=0 PASS** (no new warning in touched files; the two pre-existing
  `field_reassign_with_default` notes still travel with their code)
- `W1050f-clippy2` — `cargo clippy --lib --bins -- -D clippy::unwrap_used` — **rc=0
  PASS**
- `W1050f-terminology` — `python3 scripts/check-terminology.py` — **rc=0 PASS**, no
  violations
- `W1050f-inventory` — `python3 scripts/output-inventory.py --check` — **rc=0 PASS**
  after regeneration; content-delta analysis: 3164 strings before and after, exactly
  one entry changed (the reworded wait note), zero added/removed
- `W1050f-reloc-dryrun` — `python3 scripts/relocate-citations.py --old HEAD --dry-run`
  — **rc=0**: 22 relocated, all pure +10 shifts of byte-identical blocks, 747 already
  right, **0 need a human** (only main.rs anchors ≥7371 shift; nothing cites readback.rs
  or audit_store.rs); claims stand because the cited content did not change
- `W1050f-reloc-apply` — 4 documents rewritten (SECURITY.md, install/privacy/
  threat-model) — **rc=0**; `W1050f-citation` — drift check correctly RED (rc=1: the
  lock holds pre-shift coordinates); claims re-read → `--update` →
  **`W1050f-citation2` rc=0 PASS** — 543 锚点 / 769 处引用, digests consistent
- `python3 scripts/check-semantic-defaults.py` — rc=0 OK (no new default sites)
- `W1050f-commitmsg` — `python3 scripts/check-commit-messages.py --range HEAD~1..HEAD`
  — **rc=0 PASS**, 1 commit, English; the `commit-msg` hook (core.hooksPath already
  `scripts/hooks` in this clone) also passed at commit time

Full unfiltered suite deliberately **not** run (worker rule 4); every gate ran from
the shared `CARGO_TARGET_DIR` as found. The cargo gates ran on the exact tree that
was committed (working tree clean after commit; only derived/markdown files moved
after the last test run).

## Worker-rule and red-line compliance

- **BUILD-SERIAL honored**: `pgrep -fl cargo|rustc` before launch showed only a
  read-only W1049 review process (its brief text contains the word "cargo", matched by
  `-f`); exact-name `pgrep -x cargo` / `pgrep -x rustc` empty. Disk 36G free before
  builds. Rung gates promptly, single-threaded, no background processes left.
- No keys, certs, signing identities, or API keys touched; credentials pass by env
  var by name only, never printed.
- No real conversation data: every fixture byte stays synthetic filler; assertions
  are names/counts/sizes/digests.
- No real HOME state: all children sandbox HOME/USERPROFILE/XDG_* into per-run temp
  roots; no reclaim `--apply`; no push, no tag, no CI touched; no installed binary
  replaced.
- `CARGO_TARGET_DIR` never set or overridden (shared dir as found, never deleted or
  cleared); no sibling interleave during the run, so no `cargo clean -p` needed
  (rule 7 recorded: if a sibling tree had interleaved with a patched vendored
  package, the clean+rebuild would have been due).
- No cross-builds, no exes transferred; mid-run CI log copy kept outside the repo was
  deleted (bytes: one 386 KB text file under the operator tmp area; two /tmp inventory
  scratch files).
- Commit: one fix commit, now 0713471b after the 2026-10-31 rebase (was eaa5bdbe
  on the pre-rebase branch; English, hook-checked). This OUT now travels
  committed with the deputy round's regeneration commit, its SHA references
  remapped to the replayed commits; the deputy records/pushes.

## For Dim (一句话)

四个软伤全修 + Windows 红是 fixture 自己的分隔符拼写（非 push 语义），修复把五个前缀检查改成跨拼写分组件比较并有跨平台 pin 测试（本地先红后绿）；windows 侧绿与 CI 由 deputy push 后盯；遗留一个分支新 fixture 的 dead_code `root` 字段警告（无害，未清理，留给维护裁量）。
