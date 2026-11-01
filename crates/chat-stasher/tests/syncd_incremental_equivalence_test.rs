//! W1017 · SYNC-D — the acceptance gate: **an incremental activity-index
//! update must be byte-for-byte identical to a full rebuild of the same
//! stage** (sha256 equality over the complete
//! `<stage>/meta/<machine>/activity-v1.jsonl`).
//!
//! W935 §4 defined this test's shape; W961's T0
//! (`tests/syncd_index_determinism_test.rs`) laid the canonical-order ground
//! it stands on — without ascending `session_id` rows the comparison below
//! would flake on readdir instead of failing on the change it exists to
//! catch. This file is the D-side of that pair.
//!
//! The scheme under test, so the failures are readable: every sealed shard
//! is written once (never rewritten, never appended to), and the writer
//! records each shard's `(seq, bytes, sha256)` beside it in a `shard-facts`
//! record. An incremental build keeps a session's previous row only when
//! *every* input is proven identical — the writer-recorded digest of every
//! shard, the current byte sizes, the duplicate policy, and the session's
//! provenance observations and session-provenance sidecar (recomputed). Every
//! other state — no recorded state, a record that disagrees with the bytes,
//! a record a legacy writer never wrote, a moved observation file, a policy
//! flip — is derived by the same code a full rebuild uses. A shard with no
//! writer record is re-read on every build, by design (the ADR-034
//! disposable stance at shard grain): the alternative is trusting a size
//! check, which cannot see a same-size rewrite.
//!
//! What the matrix covers (W935 §4 numbering; one machine partition per
//! test):
//!
//! | # | case | test |
//! |---|---|---|
//! | 1 | multi-shard, bucketed layout, sequence order across buckets | `unchanged_stage` |
//! | 2 | RFC 3339 timestamps → `Exact` | `unchanged_stage` |
//! | 3 | numeric epoch seconds/millis → `Inferred` | `unchanged_stage` |
//! | 4 | unparseable timestamp → `Unknown { why }` | `unchanged_stage` |
//! | 5 | no inferable harness → counted | `unchanged_stage` |
//! | 6 | no label-able content → `NoLabelRecorded`, never empty | `unchanged_stage` |
//! | 7 | one changed shard, rest unchanged | `changed_session` |
//! | 8 | no change at all | `unchanged_stage` |
//! | 9 | reclaimed body, row still bound → carried verbatim | `reclaim_paths` |
//! | 10 | re-sealed-then-reclaimed, row bound to a different body | `reclaim_paths` |
//! | 11 | unbindable previous row | `reclaim_paths` |
//! | 12 | duplicate shard replay + policy flip | `duplicate_policy` |
//! | 13 | torn / non-UTF-8 bytes → lossy handling identical | `torn_bytes_and_corrupt_record` |
//! | 14 | record missing/corrupt for an unchanged shard → re-read, never a stale row | `same_size_rewrite`, `torn_bytes_and_corrupt_record` |
//! | 15 | same size, different bytes, mtime untouched → still detected | `same_size_rewrite` |
//! | 16 | provenance + observation merges | `unchanged_stage`, `randomized_differential_rounds` |
//! | 17 | read interrupted → exit 3, previous index bytes untouched | `interrupted_build` |
//! | 18 | randomized differential: N sessions, random subset changed per round | `randomized_differential_rounds` |
//!
//! Two invariants of the scheme are named here so the fixture shapes make
//! sense. A content change a *writer* makes is always detected — the writer
//! records the digest of what it wrote, so the record moves with the bytes
//! (that is what the same-size rewrite in case 15 models, with both files'
//! mtimes restored, so a scheme keying on size or mtime would go red). And a
//! raw in-place body edit that keeps size, mtime and re-records nothing is
//! out of model for any scheme that skips reading — the case-15 recordless
//! arm stays honest by re-reading such shards every build rather than
//! pretending to have looked.
//!
//! Secondary equality, per W935 §4: `sessions`, `carried`, `no-harness`
//! counts and each row's `time_source` kind must match between the paths.
//! `shard_bytes_read_hashed` and `elapsed` are *required to differ* — the
//! payoff assertion `incremental.hashed < full.hashed` is the point of D,
//! and a "carry" that re-read everything is counted for what it is.
//!
//! Black-box through the real binary, on the T0 pattern (`run()`,
//! `write_shard`-style fixtures, the index path). Synthetic JSONL only: no
//! archive, no real machine's config, data, state or cache; counts, byte
//! sizes and sha prefixes only in logs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

#[path = "../src/test_support.rs"]
mod test_support;

const MACHINE: &str = "mbp-w1017";

/// Run the real binary with every ambient path redirected into `sandbox`;
/// `env_extra` layers per-call values (the duplicate-policy flip) on top.
///
/// `LOCALAPPDATA` is set beside `HOME` for the reason `w267` and `w269`
/// spell out: on Windows the cache root is not a child of `$HOME` at all.
fn run(sandbox: &Path, args: &[&str], env_extra: &[(&str, &str)]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W1017 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .args(args)
        .env("HOME", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("USERPROFILE", &home)
        .env("LOCALAPPDATA", home.join("AppData").join("Local"))
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("cache"))
        .env("CHAT_STASHER_REGISTRY", &registry);
    for (key, value) in env_extra {
        command.env(key, value);
    }
    command.output().unwrap()
}

/// One synthetic claude-code line with an RFC 3339 timestamp.
fn cc_line(ts: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"hi w1017"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
    )
}

/// A claude-code user line whose `content` is a number: non-meta, so it is
/// conversation, but it carries no prompt text, so the session records
/// `NoLabelRecorded` — case 6's "read the content, nothing label-able" state.
fn cc_unlabelled_line(ts: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"isMeta":false,"sessionId":"s","type":"user","message":{{"role":"user","content":12345}},"uuid":"u2","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
    )
}

/// One synthetic opencode export line: numeric epoch-**millis** timestamps,
/// unit inferred at read time — case 3's `Inferred` shape (never `Exact`).
fn opencode_line(first_millis: u64, last_millis: u64) -> String {
    format!(
        r#"{{"schema":"session.v0","session":{{"time_created":{first_millis},"time_updated":{last_millis}}},"messages":[{{"time_created":{first_millis}}},{{"time_created":{last_millis}}}]}}"#
    )
}

fn session_id(session: &str) -> String {
    format!("claude-code.{MACHINE}.{session}")
}

fn opencode_session_id(session: &str) -> String {
    format!("opencode.{MACHINE}.{session}")
}

fn stage_index(stage: &Path) -> PathBuf {
    stage.join("meta").join(MACHINE).join("activity-v1.jsonl")
}

fn sessions_dir(stage: &Path) -> PathBuf {
    stage.join("sessions").join(MACHINE)
}

/// Where a sealed shard of `seq` lands: `<session>/NNN/NNNNNN.jsonl` under
/// the default bucket cap of 20, or directly in the session dir for a
/// legacy-layout shard.
fn shard_path(session_dir: &Path, seq: u64, legacy: bool) -> PathBuf {
    if legacy {
        session_dir.join(format!("{seq:06}.jsonl"))
    } else {
        session_dir
            .join(format!("{:03}", (seq - 1) / 20))
            .join(format!("{seq:06}.jsonl"))
    }
}

/// The bytes one sealed shard holds: the lines, each followed by one `\n`.
fn framed(lines: &[String]) -> Vec<u8> {
    let mut raw = Vec::new();
    for line in lines {
        raw.extend_from_slice(line.as_bytes());
        raw.push(b'\n');
    }
    raw
}

/// Hand-write one shard of `seq` under an explicit session-directory id and
/// record its identity with the same record API the sealing writer uses —
/// the fixture shape a body sealed after `shard-facts` records existed
/// always has.
fn write_recorded_shard_under(
    stage: &Path,
    directory_id: &str,
    seq: u64,
    lines: &[String],
    legacy: bool,
) -> Vec<u8> {
    let session_dir = sessions_dir(stage).join(directory_id);
    let raw = framed(lines);
    fs::create_dir_all(shard_path(&session_dir, seq, legacy).parent().unwrap()).unwrap();
    fs::write(shard_path(&session_dir, seq, legacy), &raw).unwrap();
    chat_stasher::shard_facts::record_in_dir(
        &session_dir,
        &chat_stasher::shard_facts::fact_for(seq, &raw),
    )
    .expect("recording a shard fact in a test fixture cannot fail");
    raw
}

fn write_recorded_shard(
    stage: &Path,
    session: &str,
    seq: u64,
    lines: &[String],
    legacy: bool,
) -> Vec<u8> {
    write_recorded_shard_under(stage, &session_id(session), seq, lines, legacy)
}

/// Seal one shard through the **real** writer — the path collect and seal
/// take, including the `shard-facts` record that path now writes.
fn seal_shard(stage: &Path, session: &str, lines: &[String]) {
    chat_stasher::store::write_sealed_shard(
        chat_stasher::store::StageWriter::Collect,
        stage,
        MACHINE,
        &session_id(session),
        lines,
    )
    .expect("sealing a synthetic shard cannot fail");
}

/// The same writer, with the exact-repeat policy collect uses for validated
/// appended bytes — the only way production ever lays down a replay.
fn seal_shard_allow_repeat(stage: &Path, session: &str, lines: &[Vec<u8>]) {
    chat_stasher::store::write_sealed_shard_bytes_allow_exact_repeat_with_cap(
        chat_stasher::store::StageWriter::Collect,
        stage,
        MACHINE,
        &session_id(session),
        lines,
        20,
    )
    .expect("sealing an exact-repeat shard cannot fail");
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The `N` of a `[activity-index] <label> : N [B]` stdout line — the counts
/// both paths must agree on (`sessions`, `carried`, `no-harness`) and the
/// byte totals they are required to differ on (`hashed`).
fn reported(stdout: &str, label: &str) -> u64 {
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("[activity-index] ") {
            if let Some((key, value)) = rest.split_once(':') {
                if key.trim() == label {
                    let number = value
                        .split(|c: char| !c.is_ascii_digit())
                        .find(|token| !token.is_empty())
                        .expect("a short word, not row text");
                    return number.parse().expect("activity-index counts are numbers");
                }
            }
        }
    }
    panic!("no [activity-index] `{label}` line in output:\n{stdout}");
}

/// The gate assertion every test in this file ends with: sha256 equality
/// first — the quantity the SYNC-D acceptance gate compares — then the bytes
/// themselves, so a mismatch fails naming the first differing row rather
/// than dumping every byte.
fn assert_same_index(incremental: &[u8], full: &[u8]) {
    assert_eq!(
        sha256_hex(incremental),
        sha256_hex(full),
        "incremental must equal full rebuild (W935 §4 acceptance gate)"
    );
    assert_eq!(
        incremental,
        full,
        "incremental must be byte-identical to full rebuild ({})",
        first_difference(incremental, full)
    );
}

/// Where two index byte-strings first disagree — named by the rows'
/// session-id prefixes so a log carries no conversation-derived text.
fn first_difference(first: &[u8], second: &[u8]) -> String {
    let a = String::from_utf8_lossy(first);
    let b = String::from_utf8_lossy(second);
    for (number, (left, right)) in a.lines().zip(b.lines()).enumerate() {
        if left != right {
            let id_of = |line: &str| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|row| {
                        row.get("session_id")
                            .and_then(serde_json::Value::as_str)
                            .map(|id| id.chars().take(40).collect::<String>())
                    })
                    .unwrap_or_else(|| "<unparseable line>".to_string())
            };
            return format!(
                "first difference at line {}: `{}` vs `{}`",
                number + 1,
                id_of(left),
                id_of(right)
            );
        }
    }
    if a.lines().count() != b.lines().count() {
        return format!(
            "line counts differ: {} vs {}",
            a.lines().count(),
            b.lines().count()
        );
    }
    "no line differs, but the bytes do (a trailing newline?)".to_string()
}

std::thread_local! {
    static DIFFERENTIAL_COPY: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The differential runner every equality below begins with: copy the whole
/// stage, run a **full rebuild on the copy** and the **incremental update on
/// the original** under the same environment, so the two outputs describe
/// the same bytes. The full rebuild never touches the original's index or
/// state, so the original stays exactly where the last increment left it.
fn differential(
    sandbox: &Path,
    stage: &Path,
    env_extra: &[(&str, &str)],
) -> ((Vec<u8>, String), (Vec<u8>, String)) {
    DIFFERENTIAL_COPY.with(|cell| cell.set(cell.get() + 1));
    let copy_root = stage.with_file_name(format!(
        "{}-fullcopy-{}",
        stage.file_name().unwrap().to_string_lossy(),
        DIFFERENTIAL_COPY.with(std::cell::Cell::get),
    ));
    copy_tree(stage, &copy_root);
    let args = [
        "activity-index",
        "--stage",
        copy_root.to_str().unwrap(),
        "--machine",
        MACHINE,
    ];
    let out = run(sandbox, &args, env_extra);
    assert!(
        out.status.success(),
        "full rebuild failed ({}):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let full = fs::read(stage_index(&copy_root)).unwrap();
    fs::remove_dir_all(&copy_root).unwrap();
    let incremental = run_increment(sandbox, stage, env_extra);
    (
        incremental,
        (full, String::from_utf8_lossy(&out.stdout).into_owned()),
    )
}

fn run_increment(sandbox: &Path, stage: &Path, env_extra: &[(&str, &str)]) -> (Vec<u8>, String) {
    let out = run(
        sandbox,
        &[
            "activity-index",
            "--incremental",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
        ],
        env_extra,
    );
    assert!(
        out.status.success(),
        "incremental update failed ({}):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    (
        fs::read(stage_index(stage)).unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// A full rebuild of a stage through the real binary, asserted successful.
fn full_rebuild(sandbox: &Path, stage: &Path, env_extra: &[(&str, &str)]) -> (Vec<u8>, String) {
    let out = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
        ],
        env_extra,
    );
    assert!(
        out.status.success(),
        "full rebuild failed ({}):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    (
        fs::read(stage_index(stage)).unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Restore a file's modification time, via a write handle — the `body_cache`
/// precedent (Windows needs the write-opened handle for `File::set_times`
/// to carry the right access).
fn restore_mtime(path: &Path, before: SystemTime) {
    let handle = fs::OpenOptions::new().write(true).open(path).unwrap();
    handle
        .set_times(std::fs::FileTimes::new().set_modified(before))
        .expect("restoring an mtime in a test fixture cannot fail");
}

/// Recursive copy of a whole stage tree, for the full-rebuild differential:
/// small fixtures, so a plain std walk is enough.
fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&from, &to);
        } else {
            fs::copy(&from, &to).unwrap();
        }
    }
}

/// The rows of one index, keyed by session id — a reader's view, which is
/// order-independent, so the byte-level comparisons above stay the thing
/// that pins order.
fn rows_of(index_bytes: &[u8]) -> BTreeMap<String, serde_json::Value> {
    let mut rows = BTreeMap::new();
    for line in String::from_utf8_lossy(index_bytes).lines() {
        let row: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("index line is not a row ({error})"));
        let id = row
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .expect("a row always names its session")
            .to_string();
        rows.insert(id, row);
    }
    rows
}

/// A reclaimed session's manifest row: what `reclaim-stage` records (shard
/// count and the digest of the shards concatenated in sequence order) before
/// it deletes the body it describes.
fn manifest_row(session_id: &str, shard_count: usize, concat_sha256: &str) -> String {
    format!(
        r#"{{"session_id":"{session_id}","machine":"{MACHINE}","shard_count":{shard_count},"concat_bytes":0,"concat_sha256":"{concat_sha256}","sealed_at_unix":1791000000}}"#
    )
}

/// Write the stage's manifest via the real writer.
fn write_manifest_rows(stage: &Path, lines: &[String]) {
    let mut rows = Vec::new();
    for line in lines {
        rows.push(
            serde_json::from_str::<chat_stasher::manifest::SessionManifest>(line)
                .expect("fixture manifest rows are well-formed"),
        );
    }
    chat_stasher::manifest::write_manifest(stage, MACHINE, &rows)
        .expect("writing a fixture manifest cannot fail");
}

/// Delete one session's body the way `reclaim-stage --apply` does: every
/// sealed shard and the body-tier records that describe it. `shard-seq`
/// persists, so the next sequence number never repeats an archived name.
fn delete_body(stage: &Path, directory_id: &str) {
    let dir = sessions_dir(stage).join(directory_id);
    for entry in fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if path.is_dir() {
            for sub in fs::read_dir(&path).unwrap() {
                let sub = sub.unwrap();
                if sub.file_name().to_string_lossy().ends_with(".jsonl") {
                    fs::remove_file(sub.path()).unwrap();
                }
            }
        } else if name.ends_with(".jsonl") || name == chat_stasher::shard_facts::FILE_NAME {
            fs::remove_file(&path).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// W935 §4 cases 1–6, 8 and 16 in one partition: every recorded shape an
/// unchanged stage can hold, then an incremental update that must come out
/// byte-identical — the 99.85%-unchanged case the whole scheme exists for.
#[test]
fn unchanged_stage_matches_full_rebuild_for_every_shape() {
    let holder = tempfile::tempdir().unwrap();
    let sandbox = holder.path().join("sandbox");
    fs::create_dir_all(&sandbox).unwrap();
    let stage = holder.path().join("stage");

    // Cases 1 + 2: multi-shard bucketed claude-code, sequence numbers
    // spanning buckets (cap 20: seq 1 in bucket 000, 21 and 22 in bucket
    // 001), plus one legacy-layout shard directly in the session dir —
    // RFC 3339 timestamps → `Exact`.
    write_recorded_shard(
        &stage,
        "multi",
        1,
        &[cc_line("2025-01-15T12:00:56.789Z")],
        false,
    );
    write_recorded_shard(
        &stage,
        "multi",
        21,
        &[cc_line("2025-01-15T13:00:56.789Z")],
        false,
    );
    write_recorded_shard(
        &stage,
        "multi",
        22,
        &[cc_line("2025-01-15T14:00:56.789Z")],
        false,
    );
    write_recorded_shard(
        &stage,
        "multi",
        7,
        &[cc_line("2025-01-15T15:00:56.789Z")],
        true,
    );

    // Case 3: opencode numeric epoch millis → `Inferred`, never `Exact`.
    write_recorded_shard_under(
        &stage,
        &opencode_session_id("019b-numbers"),
        1,
        &[opencode_line(1736948096123, 1736948296456)],
        false,
    );

    // Case 4: an unparseable timestamp → the session holds a conversation
    // whose time is `Unknown`, named — never zero, never absent.
    write_recorded_shard(
        &stage,
        "weird-time",
        1,
        &[cc_line("--not-a-timestamp--")],
        false,
    );

    // Case 5: no inferable harness — a delimiter-led id names none — counted
    // as such, never skipped; hand-written and deliberately recordless, so
    // it is also case 14's every-build re-read happening quietly beside the
    // recorded ones.
    let noharness_dir = sessions_dir(&stage).join("~w1017-no-harness");
    fs::create_dir_all(noharness_dir.join("000")).unwrap();
    fs::write(
        shard_path(&noharness_dir, 1, false),
        framed(&[cc_line("2025-01-15T16:00:56.789Z")]),
    )
    .unwrap();

    // Case 6: content read, nothing label-able → `NoLabelRecorded`, never an
    // empty string, never no-content: the line is a genuine user message.
    write_recorded_shard(
        &stage,
        "unlabelled",
        1,
        &[cc_unlabelled_line("2025-01-15T17:00:56.789Z")],
        false,
    );

    // Case 16: provenance both ways — a verified observation whose digest
    // names the body's only shard, and a collection-time session-provenance
    // sidecar row. The observation's fingerprint participates in the carry
    // decision; the sidecar stamp reaches carried rows like derived ones.
    let dim_body = write_recorded_shard(
        &stage,
        "with-provenance",
        1,
        &[cc_line("2025-01-15T18:00:56.789Z")],
        false,
    );
    let meta = stage.join("meta").join(MACHINE);
    fs::create_dir_all(&meta).unwrap();
    fs::write(
        meta.join("provenance-v1.jsonl"),
        format!(
            "{{\"session_id\":\"{}\",\"dimensions\":{{\"surface\":[\"api\"]}},\"body_shard_count\":1,\"body_sha256\":\"{}\",\"body_shard_sequences\":[1]}}\n",
            session_id("with-provenance"),
            sha256_hex(&dim_body),
        ),
    )
    .unwrap();
    fs::write(
        meta.join("session-provenance-v1.jsonl"),
        format!(
            "{{\"session_id\":\"{}\",\"source_path_class\":\"subagents\",\"parent_session_ref\":\"w1017-parent\"}}\n",
            session_id("with-provenance"),
        ),
    )
    .unwrap();

    let (full_bytes, full_out) = full_rebuild(&sandbox, &stage, &[]);
    assert_eq!(reported(&full_out, "sessions"), 6);
    assert_eq!(
        reported(&full_out, "carried"),
        0,
        "a full rebuild derives all"
    );
    assert!(reported(&full_out, "hashed") > 0);

    // What each shape's row must say — asserted once here, so the
    // differential below only needs the equality; kinds are the W935 §4
    // "secondary, must match" quantities.
    let rows = rows_of(&full_bytes);
    let kind = |rows: &BTreeMap<String, serde_json::Value>, id: &str| {
        rows[id]["time_source"]["kind"]
            .as_str()
            .expect("time_source is a tagged kind")
            .to_string()
    };
    assert_eq!(
        kind(&rows, &session_id("multi")),
        "exact",
        "case 2: RFC 3339"
    );
    assert_eq!(
        rows[&session_id("multi")]["line_count"],
        serde_json::json!(4),
        "case 1: every shard's lines, across buckets and the legacy layout"
    );
    assert_eq!(
        kind(&rows, &opencode_session_id("019b-numbers")),
        "inferred",
        "case 3: numeric epochs"
    );
    assert_eq!(
        kind(&rows, &session_id("weird-time")),
        "unknown",
        "case 4: unparseable time"
    );
    assert_eq!(
        rows[&"~w1017-no-harness".to_string()]["harness"],
        serde_json::json!("unknown"),
        "case 5: recorded honestly"
    );
    assert_eq!(
        reported(&full_out, "no-harness"),
        1,
        "case 5: and counted visibly"
    );
    assert_eq!(
        rows[&session_id("unlabelled")]["title"]["state"],
        serde_json::json!("no_label_recorded"),
        "case 6: read-but-unlabel-able, never an empty string, never nothing"
    );
    assert_eq!(
        kind(&rows, &session_id("unlabelled")),
        "exact",
        "case 6's line still carries an RFC 3339 timestamp"
    );
    assert_eq!(
        rows[&session_id("with-provenance")]["dimensions"]["surface"],
        serde_json::json!(["api"]),
        "case 16: a verified observation merges its dimensions"
    );
    assert_eq!(
        rows[&session_id("with-provenance")]["session_provenance"]["source_path_class"],
        serde_json::json!("subagents"),
        "case 16: the collection-time stamp"
    );

    // Case 8: nothing is touched between the two builds — the shape almost
    // every hourly pass takes. The first build was a full one, so every
    // recorded body's digest is validated in the state; the increment then
    // carries every recorded session and re-reads nothing. Carrying five of
    // six — the recordless no-harness session re-derives by design — plus
    // zero bytes hashed by those five is the payoff in its purest form,
    // with the equality beside it as the safety assertion.
    let (incremental_bytes, incremental_out) = run_increment(&sandbox, &stage, &[]);
    assert_eq!(reported(&incremental_out, "sessions"), 6);
    assert_eq!(reported(&incremental_out, "carried"), 5);
    let hashed = reported(&incremental_out, "hashed");
    let recordless = framed(&[cc_line("2025-01-15T16:00:56.789Z")]).len() as u64;
    assert_eq!(
        hashed, recordless,
        "only the recordless re-read: everything recorded re-read zero bytes"
    );
    assert!(
        hashed < reported(&full_out, "hashed"),
        "W935 §4 payoff: the increment reads strictly fewer bytes"
    );
    assert_eq!(
        reported(&incremental_out, "no-harness"),
        1,
        "the secondary counts match even for carried sessions"
    );
    assert_same_index(&incremental_bytes, &full_bytes);
}

/// W935 §4 case 7: one changed session, everything else untouched. The
/// change goes through the real sealing writer, exactly as collect would —
/// which also exercises the writer's `shard-facts` record on a growing body.
#[test]
fn one_changed_session_still_matches_full_rebuild() {
    let holder = tempfile::tempdir().unwrap();
    let sandbox = holder.path().join("sandbox");
    fs::create_dir_all(&sandbox).unwrap();
    let stage = holder.path().join("stage");
    seal_shard(&stage, "alpha", &[cc_line("2025-02-01T09:00:00.000Z")]);
    seal_shard(&stage, "beta", &[cc_line("2025-02-01T10:00:00.000Z")]);
    seal_shard(&stage, "gamma", &[cc_line("2025-02-01T11:00:00.000Z")]);

    let (full_before, _) = full_rebuild(&sandbox, &stage, &[]);

    // One session grows: a new shard, new bytes — the incremental's whole
    // job, since the append-only stage only ever grows a session.
    seal_shard(&stage, "beta", &[cc_line("2025-02-02T10:00:00.000Z")]);

    let ((incremental_bytes, incremental_out), (full_bytes, full_out)) =
        differential(&sandbox, &stage, &[]);

    assert_eq!(reported(&incremental_out, "sessions"), 3);
    assert_eq!(reported(&incremental_out, "carried"), 2, "alpha and gamma");
    assert_eq!(reported(&full_out, "carried"), 0);
    assert!(
        reported(&incremental_out, "hashed") < reported(&full_out, "hashed"),
        "W935 §4: the increment must read strictly fewer bytes than a full rebuild"
    );
    assert_same_index(&incremental_bytes, &full_bytes);
    assert_ne!(
        incremental_bytes, full_before,
        "the change must be visible: this is not the index the last full rebuild wrote"
    );
    assert_eq!(
        rows_of(&incremental_bytes)[&session_id("beta")]["line_count"],
        serde_json::json!(2),
        "the grown session's new shard reached its row"
    );
}

/// W935 §4 case 15 — the one that decides whether the scheme is safe at
/// all: a body rewritten to different bytes **of the same length**, with the
/// files' modification times restored, is still detected, because detection
/// compares the digest a writer records, never size or mtime. Case 14 rides
/// along: a body no writer ever recorded is re-read on every build — the
/// fallback that can never carry a stale row.
#[test]
fn same_size_rewrite_with_untouched_mtime_is_detected() {
    let holder = tempfile::tempdir().unwrap();
    let sandbox = holder.path().join("sandbox");
    fs::create_dir_all(&sandbox).unwrap();
    let stage = holder.path().join("stage");

    // A recorded body: hand-written plus its record, the shape any body
    // sealed after the records existed has.
    let recorded_before = framed(&[cc_line("2025-03-01T08:01:02.000Z")]);
    let recorded_dir = sessions_dir(&stage).join(&session_id("recorded"));
    fs::create_dir_all(recorded_dir.join("000")).unwrap();
    fs::write(shard_path(&recorded_dir, 1, false), &recorded_before).unwrap();
    chat_stasher::shard_facts::record_in_dir(
        &recorded_dir,
        &chat_stasher::shard_facts::fact_for(1, &recorded_before),
    )
    .unwrap();

    // A recordless legacy body: the shape every shard sealed before the
    // records existed has — and keeps, until it is re-sealed or restored.
    let legacy_dir = sessions_dir(&stage).join(&session_id("legacy"));
    fs::create_dir_all(legacy_dir.join("000")).unwrap();
    fs::write(
        shard_path(&legacy_dir, 1, false),
        framed(&[cc_line("2025-03-01T09:01:02.000Z")]),
    )
    .unwrap();

    let (full_first, _) = full_rebuild(&sandbox, &stage, &[]);

    // Same length, different bytes — a rewrite made through a writer, which
    // is exactly why the record moves with it.
    let recorded_after = framed(&[cc_line("2025-03-01T08:09:09.000Z")]);
    assert_eq!(recorded_after.len(), recorded_before.len());
    let legacy_after = framed(&[cc_line("2025-03-01T09:09:09.000Z")]);

    // Watch both files' mtimes, break them, then put every mtime back —
    // and prove they went back: a scheme that could have leaned on mtime
    // has nothing to lean on here.
    let body_path = shard_path(&recorded_dir, 1, false);
    let record_path = recorded_dir.join(chat_stasher::shard_facts::FILE_NAME);
    let legacy_body = shard_path(&legacy_dir, 1, false);
    let watched = [body_path.clone(), record_path.clone(), legacy_body.clone()].map(|path| {
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        (path, before)
    });
    fs::write(&body_path, &recorded_after).unwrap();
    chat_stasher::shard_facts::record_in_dir(
        &recorded_dir,
        &chat_stasher::shard_facts::fact_for(1, &recorded_after),
    )
    .unwrap();
    fs::write(&legacy_body, &legacy_after).unwrap();
    for (path, before) in watched {
        restore_mtime(&path, before);
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "the fixture must qualify as mtime-untouched",
        );
    }

    let ((incremental_bytes, incremental_out), (full_bytes, _)) =
        differential(&sandbox, &stage, &[]);

    assert_eq!(
        reported(&incremental_out, "carried"),
        0,
        "the recorded digest changed, so nothing may be carried"
    );
    assert_eq!(reported(&incremental_out, "sessions"), 2);
    assert!(
        reported(&incremental_out, "hashed") > 0,
        "both bodies must have been read"
    );
    assert_same_index(&incremental_bytes, &full_bytes);
    assert_ne!(
        incremental_bytes, full_first,
        "the rewrite must be visible: fresh reads, not carried rows"
    );
    // 2025-03-01T08:09:09Z = 1740816549. The old body said 08:01:02
    // (1740816062); a carried row would still answer the old instant and a
    // wrong answer would have failed the equality above before this.
    assert_eq!(
        rows_of(&incremental_bytes)[&session_id("recorded")]["first_unix"],
        serde_json::json!(1740816549),
        "the recorded body's rewrite is the row the increment now carries"
    );
}

/// W935 §4 cases 9–11: the reclaim side. A body-less session's row depends
/// on the previous index and the manifest — not on shard bytes — so both
/// paths run the same branch and must agree on all three arms: carried
/// verbatim while still bound; rewritten `Unknown` after a re-seal;
/// rewritten `Unknown` when nothing can bind the previous row. The
/// empty-directory arm (nothing was ever sealed) is the fourth state,
/// distinct from all three.
#[test]
fn reclaim_carry_paths_match_full_rebuild() {
    let holder = tempfile::tempdir().unwrap();
    let sandbox = holder.path().join("sandbox");
    fs::create_dir_all(&sandbox).unwrap();
    let stage = holder.path().join("stage");
    for name in ["bound", "resealed", "unbindable"] {
        seal_shard(&stage, name, &[cc_line("2025-04-01T07:00:00.000Z")]);
    }
    fs::create_dir_all(sessions_dir(&stage).join(&session_id("never-sealed"))).unwrap();

    let (full_first, _) = full_rebuild(&sandbox, &stage, &[]);
    assert_eq!(
        rows_of(&full_first).len(),
        4,
        "three recorded + the never-sealed dir"
    );
    let rows_before = rows_of(&full_first);
    let measured_of = |name: &str| {
        let measured = &rows_before[&session_id(name)]["measured_body"];
        (
            measured["shard_count"].as_u64().expect("count") as usize,
            measured["concat_sha256"]
                .as_str()
                .expect("digest")
                .to_string(),
        )
    };

    // Reclaim the recorded bodies, and record in the manifest what
    // `reclaim-stage` would record: exactly the summaries below.
    for name in ["bound", "resealed", "unbindable"] {
        delete_body(&stage, &session_id(name));
    }
    let (bound_count, bound_concat) = measured_of("bound");
    let (resealed_count, _) = measured_of("resealed");
    write_manifest_rows(
        &stage,
        &[
            // Case 9: bound — the manifest still names the exact body the
            // row measured, so the row carries verbatim.
            manifest_row(&session_id("bound"), bound_count, &bound_concat),
            // Case 10: re-sealed — the manifest names a *different* body,
            // so the previous row described another conversation.
            manifest_row(&session_id("resealed"), resealed_count, &"f".repeat(64)),
        ],
    );
    // `unbindable` deliberately has no manifest row: nothing ties its row to
    // any body → case 11.

    // A late-arriving empty session directory: no body, no previous row —
    // the fourth state, which is "nothing was ever sealed here" and not any
    // unknown.
    fs::create_dir_all(sessions_dir(&stage).join(&session_id("late-empty"))).unwrap();

    let ((incremental_bytes, incremental_out), (full_bytes, _)) =
        differential(&sandbox, &stage, &[]);
    assert_eq!(reported(&incremental_out, "sessions"), 5);
    assert_same_index(&incremental_bytes, &full_bytes);

    let after = rows_of(&incremental_bytes);
    assert_eq!(
        serde_json::to_string(&after[&session_id("bound")]).unwrap(),
        serde_json::to_string(&rows_before[&session_id("bound")]).unwrap(),
        "case 9: a still-bound row is carried verbatim"
    );
    let why_of = |rows: &BTreeMap<String, serde_json::Value>, id: &str| {
        rows[id]["time_source"]["why"]
            .as_str()
            .expect("an Unknown always names its why")
            .to_string()
    };
    assert_eq!(
        after[&session_id("resealed")]["time_source"]["kind"],
        serde_json::json!("unknown"),
        "case 10: the row does not survive a different body"
    );
    assert!(
        why_of(&after, &session_id("resealed")).contains("re-sealed"),
        "case 10: the why names the re-seal"
    );
    assert!(
        why_of(&after, &session_id("unbindable")).contains("cannot be bound"),
        "case 11: the why names the unbindable row"
    );
    // The empty directory that *already had a row* — from the first build,
    // before it was relevant — is an established pre-increment behavior and
    // is asserted here as it is, both paths agreeing through the equality
    // above: once a row exists, its own state is the unbindable unknown.
    assert_eq!(
        after[&session_id("never-sealed")]["time_source"]["kind"],
        serde_json::json!("unknown"),
    );
    assert!(why_of(&after, &session_id("never-sealed")).contains("cannot be bound"),);
    // The late-arriving empty directory records the fourth state, distinct
    // from every unknown: nothing was ever sealed, so there is no
    // conversation to place in time.
    assert_eq!(
        after[&session_id("late-empty")]["time_source"]["kind"],
        serde_json::json!("no_conversation_content"),
        "an empty directory records its own state, distinct from every unknown"
    );
    assert_eq!(
        rows_before[&session_id("never-sealed")]["time_source"]["kind"],
        serde_json::json!("no_conversation_content"),
        "on the first build — no previous row — the empty directory said the same"
    );
}

/// W935 §4 case 12: a duplicated playback of whole shards collapses the same
/// way in both paths under the default policy — and a policy *flip* between
/// builds re-lines every session, so the increment still equals a full
/// rebuild under the new policy rather than carrying collapse-era rows.
#[test]
fn duplicate_replay_and_policy_flip_match_full_rebuild() {
    let holder = tempfile::tempdir().unwrap();
    let sandbox = holder.path().join("sandbox");
    fs::create_dir_all(&sandbox).unwrap();
    let stage = holder.path().join("stage");

    let first = framed(&[cc_line("2025-05-01T06:00:00.000Z")]);
    let second = framed(&[cc_line("2025-05-01T07:00:00.000Z")]);
    seal_shard_allow_repeat(&stage, "replayed", &[first.clone()]);
    seal_shard_allow_repeat(&stage, "replayed", &[second.clone()]);
    // The replay pairs exist only through the exact-repeat policy — a
    // whole-content run replayed at higher sequence numbers.
    seal_shard_allow_repeat(&stage, "replayed", &[first]);
    seal_shard_allow_repeat(&stage, "replayed", &[second]);
    seal_shard(&stage, "single", &[cc_line("2025-05-01T08:00:00.000Z")]);

    let (collapsed_full, _) = full_rebuild(&sandbox, &stage, &[]);
    assert_eq!(
        rows_of(&collapsed_full)[&session_id("replayed")]["line_count"],
        serde_json::json!(2),
        "the default policy collapses the whole-content replay"
    );

    let (collapsed_incremental, _) = run_increment(&sandbox, &stage, &[]);
    assert_same_index(&collapsed_incremental, &collapsed_full);

    // Flip: keep every shard. Recorded digests all line up — a scheme
    // keying on nothing but digests would wrongly carry — but the policy
    // does not, so a correct increment re-derives everything.
    let flip_env = [(chat_stasher::store::KEEP_ALL_SHARDS_ENV, "1")];
    let ((kept_incremental, kept_out), (kept_full, _)) = differential(&sandbox, &stage, &flip_env);
    assert_eq!(
        reported(&kept_out, "carried"),
        0,
        "a policy flip re-lines every session — nothing may be carried"
    );
    assert_same_index(&kept_incremental, &kept_full);

    // And the flip back: equality again, both ways, with no collapsed state
    // left over from having run under the other policy.
    let (back_full, _) = full_rebuild(&sandbox, &stage, &[]);
    let (back_incremental, _) = run_increment(&sandbox, &stage, &[]);
    assert_same_index(&back_incremental, &back_full);
    assert_eq!(
        rows_of(&back_incremental)[&session_id("replayed")]["line_count"],
        serde_json::json!(2),
        "collapse is back"
    );
}

/// W935 §4 cases 13 and 14: torn non-UTF-8 bytes are read lossily — the same
/// bytes reaching the same row through both paths — and a **damaged or
/// missing record for an unchanged body** must degrade to reading the body,
/// never to carrying a stale row (ADR-034's disposable stance, at the
/// record).
#[test]
fn torn_bytes_and_a_corrupt_record_match_full_rebuild() {
    let holder = tempfile::tempdir().unwrap();
    let sandbox = holder.path().join("sandbox");
    fs::create_dir_all(&sandbox).unwrap();
    let stage = holder.path().join("stage");

    // Torn bytes through the real writer: arbitrary byte strings are legal
    // line bytes and read lossily (`from_utf8_lossy`).
    let torn =
        b"{\"timestamp\":\"2025-06-01T05:00:00.000Z\",\"torn\":\"\xff\xfe\x00 ok w1017\"}".to_vec();
    chat_stasher::store::write_sealed_shard_bytes_with_cap(
        chat_stasher::store::StageWriter::Collect,
        &stage,
        MACHINE,
        &session_id("torn"),
        &[torn],
        20,
    )
    .expect("sealing torn-but-legal bytes cannot fail");
    seal_shard(&stage, "intact", &[cc_line("2025-06-01T05:00:00.000Z")]);

    let (first_full, _) = full_rebuild(&sandbox, &stage, &[]);
    let (first_incremental, _) = run_increment(&sandbox, &stage, &[]);
    assert_same_index(&first_incremental, &first_full);

    // Corrupt the intact session's record with bytes no fact parses from —
    // a damaged record for an unchanged body. The body did not move; the
    // record no longer vouches for anything.
    let record = sessions_dir(&stage)
        .join(&session_id("intact"))
        .join(chat_stasher::shard_facts::FILE_NAME);
    fs::write(&record, "{\"format\":\"that is not a fact at all\"}\n").unwrap();

    let ((incremental_bytes, incremental_out), (full_bytes, _)) =
        differential(&sandbox, &stage, &[]);
    assert_eq!(
        reported(&incremental_out, "carried"),
        1,
        "the torn session still carries; the one whose record lies cannot"
    );
    assert!(reported(&incremental_out, "hashed") > 0);
    assert_same_index(&incremental_bytes, &full_bytes);

    // Removing the record entirely degrades the same way — case 14's other
    // arm: re-read, never a stale row.
    fs::remove_file(&record).unwrap();
    let ((again_incremental, again_out), (again_full, _)) = differential(&sandbox, &stage, &[]);
    assert_eq!(reported(&again_out, "carried"), 1);
    assert_same_index(&again_incremental, &again_full);
}

/// W935 §4 case 17: an interrupted read must not publish a partial index —
/// exit 3, and the previous index bytes untouched. The partition root is
/// replaced by a regular file (a `read_dir` failure on every platform,
/// deterministic and permission-independent), so enumeration fails before
/// any row is written; publication happens only after a complete pass,
/// which is exactly the property this pins, for both build modes.
#[test]
fn interrupted_build_exits_3_and_leaves_the_previous_index_untouched() {
    let holder = tempfile::tempdir().unwrap();
    let sandbox = holder.path().join("sandbox");
    fs::create_dir_all(&sandbox).unwrap();
    let stage = holder.path().join("stage");
    seal_shard(&stage, "steady", &[cc_line("2025-07-01T04:00:00.000Z")]);

    let _ = full_rebuild(&sandbox, &stage, &[]);
    let published = fs::read(stage_index(&stage)).unwrap();
    let published_sha = sha256_hex(&published);

    // Break the read: the partition root is now a regular file, so
    // `read_dir` fails — "did not finish reading", never an empty index.
    let moved_aside = holder.path().join("moved-aside");
    fs::rename(sessions_dir(&stage), &moved_aside).unwrap();
    fs::write(sessions_dir(&stage), "not a directory").unwrap();

    let incremental_args = vec![
        "activity-index",
        "--incremental",
        "--stage",
        stage.to_str().unwrap(),
        "--machine",
        MACHINE,
    ];
    let full_args = vec![
        "activity-index",
        "--stage",
        stage.to_str().unwrap(),
        "--machine",
        MACHINE,
    ];
    for args in [incremental_args, full_args] {
        let out = run(&sandbox, &args, &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "a read that did not finish is exit 3 in both modes, not a lenient \
             shorter answer:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            sha256_hex(&fs::read(stage_index(&stage)).unwrap()),
            published_sha,
            "a failed read must leave the previous index byte-intact"
        );
    }
}

/// A deterministic small-step random generator: no dependency, no system
/// randomness — a red today is a red tomorrow, on any seed's terms.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// W935 §4 case 18: the randomized differential. One partition, sessions of
/// several shapes, a mutation schedule across rounds — appends through the
/// real writer, a reclaim with its manifest refresh, a deleted record, a
/// policy flip, appended observations, and a do-nothing round. Every round
/// ends the same way: the incremental update and a full rebuild of the
/// same bytes share one sha256.
#[test]
fn randomized_differential_rounds_match_full_rebuild() {
    let holder = tempfile::tempdir().unwrap();
    let sandbox = holder.path().join("sandbox");
    fs::create_dir_all(&sandbox).unwrap();
    let stage = holder.path().join("stage");

    // Ten recorded claude-code sessions …
    let matrix: Vec<String> = (0..10).map(|i| format!("w1017-matrix-{i:02}")).collect();
    for (i, name) in matrix.iter().enumerate() {
        let minute = 60 * i + 1;
        seal_shard(
            &stage,
            name,
            &[cc_line(&format!("2025-08-01T00:{minute:02}:00.000Z"))],
        );
    }
    // … one opencode session with numeric epochs (a second reader) …
    chat_stasher::store::write_sealed_shard_with_cap(
        chat_stasher::store::StageWriter::Collect,
        &stage,
        MACHINE,
        &opencode_session_id("w1017-numbers"),
        &[opencode_line(1736948096123, 1736948296456)],
        20,
    )
    .expect("sealing an opencode session cannot fail");
    // … and one legacy recordless session — the pre-record body shape.
    let legacy_dir = sessions_dir(&stage).join(&session_id("w1017-legacy"));
    fs::create_dir_all(legacy_dir.join("000")).unwrap();
    fs::write(
        shard_path(&legacy_dir, 1, false),
        framed(&[cc_line("2025-08-01T09:59:00.000Z")]),
    )
    .unwrap();

    let (r0_full, _) = full_rebuild(&sandbox, &stage, &[]);
    assert_eq!(rows_of(&r0_full).len(), 12, "ten + opencode + legacy");

    // Round 1: append to two recorded sessions through the real writer.
    let mut lcg = Lcg(0x9E37_79B9_7F4A_7C15);
    let picks = [(lcg.next() as usize) % 10, (lcg.next() as usize) % 10];
    for i in picks {
        seal_shard(
            &stage,
            matrix[i].as_str(),
            &[cc_line("2025-08-02T12:00:00.000Z")],
        );
    }
    let ((r1_incremental, r1_out), (r1_full, _)) = differential(&sandbox, &stage, &[]);
    assert_eq!(reported(&r1_out, "sessions"), 12);
    assert_eq!(
        reported(&r1_out, "carried"),
        9,
        "eleven recorded sessions minus the two just grown; legacy re-derives by design"
    );
    assert_same_index(&r1_incremental, &r1_full);

    // Round 2: nothing changes — every recorded session carries, and the
    // index is rewritten to exactly the same bytes.
    let (r2_incremental, r2_out) = run_increment(&sandbox, &stage, &[]);
    assert_eq!(reported(&r2_out, "carried"), 11);
    assert_eq!(
        r2_incremental, r1_full,
        "a do-nothing round rewrites exactly the same index bytes"
    );

    // Round 3: reclaim one session and refresh its manifest from the row the
    // last build measured, plus one append elsewhere. Both sides must carry
    // the reclaimed row forward identically (case 9's rule) while the
    // appended session re-derives.
    {
        let rows = rows_of(&r1_full);
        let measured = &rows[&session_id(&matrix[3])]["measured_body"];
        let count = measured["shard_count"].as_u64().expect("count") as usize;
        let digest = measured["concat_sha256"].as_str().expect("digest");
        delete_body(&stage, &session_id(&matrix[3]));
        write_manifest_rows(
            &stage,
            &[manifest_row(&session_id(&matrix[3]), count, digest)],
        );
        seal_shard(
            &stage,
            matrix[0].as_str(),
            &[cc_line("2025-08-03T03:00:00.000Z")],
        );
    }
    let ((r3_incremental, _), (r3_full, _)) = differential(&sandbox, &stage, &[]);
    assert_same_index(&r3_incremental, &r3_full);

    // Round 4: delete an untouched session's `shard-facts` — the record goes
    // missing while the body did not move.
    fs::remove_file(
        sessions_dir(&stage)
            .join(&session_id(&matrix[8]))
            .join(chat_stasher::shard_facts::FILE_NAME),
    )
    .unwrap();
    let ((r4_incremental, r4_out), (r4_full, _)) = differential(&sandbox, &stage, &[]);
    assert_eq!(reported(&r4_out, "carried"), 9);
    assert_same_index(&r4_incremental, &r4_full);

    // Round 5: a fresh provenance observation and a session-provenance
    // sidecar row, both for a session whose body did not move — the cheap
    // inputs a carry still has to notice.
    {
        let facts = chat_stasher::shard_facts::read_in_dir(
            &sessions_dir(&stage).join(&session_id(&matrix[9])),
        )
        .expect("the session is recorded");
        let (_, fact) = facts
            .iter()
            .max_by_key(|(seq, _)| *seq)
            .expect("at least one recorded shard");
        let meta = stage.join("meta").join(MACHINE);
        let mut provenance =
            fs::read_to_string(meta.join("provenance-v1.jsonl")).unwrap_or_default();
        provenance.push_str(&format!(
            "{{\"session_id\":\"{}\",\"dimensions\":{{\"container\":[\"w1017\"]}},\"body_shard_count\":1,\"body_sha256\":\"{}\",\"body_shard_sequences\":[{}]}}\n",
            session_id(&matrix[9]),
            fact.sha256,
            fact.seq,
        ));
        fs::write(meta.join("provenance-v1.jsonl"), provenance).unwrap();
        fs::write(
            meta.join("session-provenance-v1.jsonl"),
            format!(
                "{{\"session_id\":\"{}\",\"source_path_class\":\"main\"}}\n",
                session_id(&matrix[9]),
            ),
        )
        .unwrap();
    }
    let ((r5_incremental, r5_out), (r5_full, _)) = differential(&sandbox, &stage, &[]);
    assert_eq!(
        reported(&r5_out, "carried"),
        8,
        "the observed session re-derives so its verified merge can run again"
    );
    assert_same_index(&r5_incremental, &r5_full);
    assert_eq!(
        rows_of(&r5_incremental)[&session_id(&matrix[9])]["dimensions"]["container"],
        serde_json::json!(["w1017"]),
        "the new observation verified against the unchanged body and merged"
    );

    // Round 6: policy flip, last so nothing runs on top of the flipped
    // state. This partition holds no whole-content replay, so the flipped
    // policy re-lines rows that come out the same — the assertion that
    // matters here is that the re-lined index is exactly what a full
    // rebuild under the same policy writes (the *material* effect of a
    // flip is covered separately, in
    // `duplicate_replay_and_policy_flip_match_full_rebuild`).
    let flip_env = [(chat_stasher::store::KEEP_ALL_SHARDS_ENV, "1")];
    let ((r6_incremental, r6_out), (r6_full, _)) = differential(&sandbox, &stage, &flip_env);
    assert_eq!(reported(&r6_out, "carried"), 0);
    assert_same_index(&r6_incremental, &r6_full);
}
