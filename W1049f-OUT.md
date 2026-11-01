DONE 96c8ca8e (W1049f · Join the fixture rollout path one segment at a time so its digests are platform-native)
Branch `w1049-run-source-vanish`, base 7cbeab1c unchanged, one fix commit, not pushed, no tag, no CI touched.

# W1049f OUT — windows-only red in the two vanish-digest tests

## The real platform lock (the brief's premise, corrected)

There are **no hardcoded digests in the file at any HEAD** this branch ever had — the two CI-red assertions already computed `right` at runtime through `expect_path_digest`, which mirrors the product's `source_key(path)` letter for letter (canonicalize when possible, else the path as given, hashed as a string). The brief's "expected hex computed from a macOS temp path" does not describe the file; what was platform-locked was **which bytes** that runtime computation hashed.

Root cause (fits every CI fact, including which assertions survived):

- The fixture built the rollout as `sessions_dir.join("2026/10/27").join(…jsonl)`. `PathBuf::push` appends that literal **verbatim**, so on Windows the test's own path string keeps forward slashes (`…\codex-sessions\2026/10/27\rollout…`) while the scanner's own walk (one `read_dir` entry at a time) names the same file with backslashes (`…\codex-sessions\2026\10\27\rollout…`).
- While the file exists, `fs::canonicalize` erases the difference on both sides — every existing-file digest assertion (test 2's outcome digest; test 1 would-be `relocated_path_sha256`) passed on Windows.
- The two CI-red assertions are exactly the only ones that hash a path **whose file is gone**: there both the test helper and the product take the verbatim fallback, and hash different bytes for the same vanished file. macOS and ubuntu spell both sides identically (`/` is native), so only Windows could disagree.
- `assert_eq!(…, fx.archived, "the post-move walk finds the rollout…")` still passed on Windows because `Path` equality compares components (both separators split), not bytes — which is why only the digest sub-assertions went red.

## What changed (crates/chat-stasher/tests/w1049_run_source_vanish_test.rs, 13+/2−)

The fixture now joins `2026`, `10`, `27` as separate components, so the test's path bytes are identical to the walk's on every platform by construction, and `expect_path_digest(&fx.active)` — unchanged, still the exact per-brief "sha256 of the actual vanished path string as the test itself constructed it" — now agrees with the product on any platform. A comment in the fixture states why one-segment joins are required. **No product-code change was needed** (the collector hashes the path exactly as it scanned it; that behavior is untouched). No assertion was weakened or re-pointed: both still compare the notice's / the error's `source_path_sha256` against the test's own vanished path. On macOS the byte string is unchanged, so the local run exercises byte-identical fixtures to rev0 (`5/5`, two of them platform-relevant). No other digest in the file was hardcoded (the brief's conditional clause ("if any other hardcoded path-digest appears…") found nothing to fix).

## Gates (gate tool, rc recorded — logs under ~/scratch/gate-logs/2026-10-31/)

- `W1049f-focused -- cargo test --test w1049_run_source_vanish_test` — **rc=0 PASS**, 5 passed / 0 failed, log `W1049f-focused-123522-63609.log`
- `W1049f-fmt -- cargo fmt --check` — **rc=0 PASS**, log `W1049f-fmt-123701-83691.log`
- `W1049f-clippy-targets -- cargo clippy --all-targets -- -D clippy::let_underscore_must_use` — **rc=0 PASS**, zero mentions of the touched file; the printed warnings are pre-existing, log `W1049f-clippy-targets-123715-84312.log`
- `W1049f-clippy-unwrap -- cargo clippy --lib --bins -- -D clippy::unwrap_used` — **rc=0 PASS**, log `W1049f-clippy-unwrap-123743-85570.log`
- `W1049f-citation-drift -- python3 scripts/check-citation-drift.py` — **rc=0 PASS**, OK (543 锚点 / 769 处引用), **no citation moves** (test-only diff), so no relocate dry-run was needed, log `W1049f-citation-drift-123703-83783.log`
- `W1049f-output-inventory -- python3 scripts/output-inventory.py --check` — **rc=0 PASS**, up to date, log `W1049f-output-inventory-123704-83803.log`
- `W1049f-commit-msgs -- python3 scripts/check-commit-messages.py --range HEAD~1..HEAD` — **rc=0 PASS** (1 commit, English); the `commit-msg` hook (`core.hooksPath=scripts/hooks`, already set in this clone) also passed both the commit and the message-typo amend, log `W1049f-commit-msgs-124111-90496.log`

Full unfiltered suite deliberately not run (worker rule 4); the isolation-guarded full-suite line stays the deputy/reviewer's call if wanted.

## Windows verification state

Could not be shown directly from this machine: no reachable Windows endpoint exists right now (not in the mssh host table; no local Windows VM running), so the sanctioned cross-build-then-run flow was not attempted — **0 bytes cross-built or transferred (rule 6)**. What is left is the construction argument recorded in the commit body: after the change the fixture's spelling and the walk's spelling of the same path are the same bytes on every platform, and the product/test digest algorithms are mirror images, so the failure mode (platform ≠ native-separator-macOS) has no remaining seam to diverge on. The deputy's CI run is the Windows verdict; if it is still red, the failing digests will now be genuinely interesting (they would falsify the byte-form argument and point at the product instead).

## Red lines / worker rules

No keys, certificates, signing identities or API keys touched; no real conversation data read or written (synthetic codex-store fixtures only, `Sandbox` temp roots, no env pinned); no real HOME state writes; no `reclaim --apply`; no push, no tag, no CI (deputy's). Shared `CARGO_TARGET_DIR` never set or overridden — it came from the environment (`/Users/dimpurr/scratch/cargo-target-shared`, empty at start after the recent cleanup); the only knobs I exported were `CARGO_PROFILE_DEV_DEBUG=0` for the build/clippy gates (rule 5's disk knob: shared dir holds 2.9 GB after). Rule 7 check done — `pgrep -fl "cargo|rustc"` before every gate showed no concurrent build (the only match was the deputy's own dashboard-watching zsh loop, no cargo/rustc). No `rustic_core` was patched, so the sibling-worktree stale-rlib hazard does not apply. No background processes left, build slot released (BUILD-SERIAL). No temp files created (rule 3 — the gate tool's own logs under ~/scratch/gate-logs/2026-10-31/ are the only non-repo artifacts). No cross-compile artifacts exist to delete.

## For Dim（一句话）

红的原因不是硬编码摘要（文件里从来就没有——两头都是运行时算的），而是 fixture 用 `.join("2026/10/27")` 造路径、Windows 下这串字面量原样进了路径字节，跟扫描自己逐级拼出来的名字差了分隔符——文件还在时 canonicalize 两边都盖掉了，文件消失（恰好只有两条红断言）才咬人；按 brief 的意图改成逐段 join，测试自己算的期望在所有平台都和产品对上，产品代码零改动，本机 5/5 全绿，Windows 由 CI 收尾。
