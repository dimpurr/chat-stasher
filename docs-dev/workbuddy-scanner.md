# WorkBuddy scanner — design draft (CS-WB-1, not implemented)

Status: draft for Dim review. 0 lines of production code written for this draft.
Evidence: `~/scratch/handoff/2026-10-30-cs-workbuddy-feasibility.REPORT.md` (to be written this round).

## 0. Recommendation: route A (dual store), B only if measurement had failed

- **A · sqlite metadata + jsonl transcript** (recommended): `sessions` gives id/title/model/mode/transport/tombstone, jsonl gives the transcript. Both confirmed on real samples this round.
- **B · jsonl only** (rejected unless): loses title/model/tombstone and silently drops `transport != local` sessions — violates the project's first iron rule. Would only qualify if measurement had shown every session on disk with full metadata recoverable; it did not (ai-title covers 3/3 local sessions but is a best-effort event, not a record).
- **C · import arm** (rejected): no user-facing session export exists on the WorkBuddy line. The zip-export entry in `cloud.tencent.cn/document/api/1267/123662` belongs to **CodeBuddy IDE 4.12.0**, not WorkBuddy (verified by opening both changelogs 2026-10-30). WorkBuddy's official migration (5.9.1/5.9.3) is same-account device transfer with an import-side contract (100 GB cap), never a parseable export format.
- **D · not doing** (rejected): the gap grows silently and compounds; community tooling proves demand and feasibility.

## 1. Registry entry sketch (`harness-registry-v1.json`)

- `id: workbuddy`, `product_group: agent-platforms`, `dev_priority: normal`.
- `paths.macos.template: ~/.workbuddy/projects/`, `format: jsonl`, `confidence: measured-locally`,
  `source: measured locally (CS-WB-1): <slug>/<uuid>.jsonl + workbuddy.db sessions`.
- `sql_table: sessions`, `sql_id_column: id`. No other OS rows (unascertained; scanner refuses unmeasured paths).
- `known_issue`: no official schema (third-party reverse-engineering only); event-type census per run, drift → re-verify (see §5).
- 90-day reverify: same mechanism as every row (generator fails past 90 days).

## 2. Identity, tombstone, unknown

- **id**: `sessions.id`, verified == jsonl filename on 3/3 local sessions and community samples. Filename (uuid stem) is the fallback; db row wins on conflict.
- **tombstone**: `deleted_at IS NOT NULL` (any non-null value, including the `-1` the cloud-sort index implies) ⇒ tombstone record, same shape as the opencode path. No deleted rows observed locally — the tombstone arm MUST ship with a simulated fixture, not just code.
- **unknown (exit 3), never 0**: `transport != 'local'` (schema admits `cloud`; index `idx_sessions_cloud_*` proves the vendor expects it) with no local transcript ⇒ unknown. Session row with no matching jsonl (e.g. the 14/49 cwd-less segments community reporters saw) ⇒ unknown. Missing sequence values stay metadata.
- **append semantics**: file order is arrival order; `timestamp` is provider wall-clock with ms-level inversions (measured: session-meta and call_result stamped before their predecessors). Cursor MUST be line/byte offset, never timestamp sort. Empirically append-only: 40-min-apart prefix hashes identical on all 3 live files while sizes grew.

## 3. Normalizer mapping (6–7 event types, CodeBuddy vocabulary)

`message` (role user/assistant; user text inside `<user_query>`, skip `<cb_summary>`/`<task-notification>`-prefixed system injections) / `reasoning` / `function_call`+`function_call_result` (paired by `callId`; unclosed calls stay unclosed, never synthesized) / `file-history-snapshot` (trackedFileBackups metadata, not messages) / `ai-title` (title hint only) / `session-meta` (hostKind/sessionTitle metadata). `id` is NOT row-unique (one model response shares `id` across message + calls). `arguments` is a JSON string needing a second parse. `cwd` rides every event; `sessionId` == filename. Rendered `tool-results/*.txt` (`{content, renderer}`) are derivative — do not double-ingest.

## 4. 4D projection coverage

surface ← project slug (cwd-derived, lowercase-drive + `\`→`-` rules per community); model ← `sessions.model`; title ← `sessions.title`/`custom_title` else `aiTitle`; time ← `created_at`/`last_activity_at` (ms). Full 4D fillable from A route.

## 5. Drift detection (format changes with app upgrades)

Per collect run: census event `type` values + top-level key sets against the §3 inventory; any new type/key or removed type ⇒ collect continues (unknown-leaning) + `known_issue` gains a dated line + 90-day clock restarts. WAL: open `mode=ro` (never `immutable=1` — forensics + hermes precedent: WAL holds live sessions); read the db trio as one unit.

## 6. Effort vs hermes baseline (f984aa11 = 22 files / +632 −34)

Same skeleton (registry ~50 + scanner ~160–200 with dual-source dedupe + sqlite_probe ~200 + normalizer ~150–250 for 6–7 event types vs hermes's 1 + tests ~100 + docs ~20 + matrix ~5): **≈700–950 lines, 1 W task**, within ±50% of hermes. Extra vs hermes: dual-path pairing logic, tombstone fixture, drift census. Less vs hermes: no cost/token aggregates (session_usage exists but out of scope for v1).
