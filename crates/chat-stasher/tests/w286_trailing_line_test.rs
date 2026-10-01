//! W286 — a session file whose final line has no trailing newline must
//! converge, and its tail must be committed once it completes.
//!
//! The Windows/WSL real-machine validation measured a JSONL source whose
//! last line was not newline-terminated. The first pass sealed only the
//! complete lines, and every later pass re-read the unterminated tail,
//! sealed nothing — and still counted the session as `changed`, so every
//! `run-once` pushed a fresh snapshot of an unchanged stage, forever: the
//! report's table read +1 snapshot per run with the same bytes re-added
//! each time, and `setup` reported `noop: not_observed` while still calling
//! itself healthy.
//!
//! The deterministic rule this file pins, stated once here because the
//! report asked for a decided rule rather than an emergent one:
//!
//! - an unterminated final line is **in progress**: only newline-terminated
//!   lines are ever sealed, because a torn last line may be half of a write;
//! - the tail is re-read on every later pass from the committed cursor, so
//!   the pass that ends the line with a newline commits it in full — a tail
//!   is never lost by being left behind, and the sealed prefix is never
//!   re-staged;
//! - a pass that read the tail but sealed no line staged nothing new, so
//!   the session counts as *unchanged*: a source that stops changing
//!   converges to a NOOP pass and zero new snapshots instead of re-flagging
//!   the same tail as a change forever.
//!
//! Everything here is synthetic: counts, byte sizes and id prefixes only.

use chat_stasher::collect;
use chat_stasher::collect::DestinationView;
use chat_stasher::config::Config;
use chat_stasher::id::short_session_id;
use chat_stasher::scanner::{self, HarnessRegistry};
use chat_stasher::store;
use serde_json::json;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};

#[path = "../src/test_support.rs"]
mod test_support;

fn registry() -> HarnessRegistry {
    let cell = json!({
        "template": "~/.claude/projects",
        "format": "jsonl",
        "confidence": "source-confirmed",
        "source": "synthetic fixture"
    });
    let paths = match scanner::current_platform() {
        "macos" => json!({"macos": cell}),
        "linux" => json!({"linux": cell}),
        "windows" => json!({"windows": cell}),
        platform => panic!("unexpected platform: {platform}"),
    };
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic",
        "harnesses": [{
            "id": "claude-code",
            "display_name": "synthetic",
            "paths": paths
        }]
    }))
    .unwrap()
}

/// These tests only exercise the local read path, so the destination archive
/// is deliberately not reachable: a debt still owed on the stage must be
/// provable without ever asking it.
fn dest<'a>() -> DestinationView<'a> {
    DestinationView::unreachable("fixture-destination")
}

fn scan(root: &Path) -> scanner::ScanReport {
    let config = Config {
        claude_projects_dir: Some(root.to_string_lossy().into_owned()),
        ..Config::default()
    };
    scanner::scan_with_registry(&config, &registry()).unwrap()
}

fn pass(root: &Path, stage: &Path, state: &Path) -> collect::CollectReport {
    collect::collect_scan_report(&scan(root), stage, "fixture-machine", state, 20, &dest()).unwrap()
}

/// The collect-level repro: a stable unterminated tail must not keep the
/// session permanently `changed`, and the tail must commit once complete.
#[test]
fn unterminated_tail_converges_and_commits_once_complete() {
    let dir = tempfile::TempDir::new().unwrap();
    let source_root = dir.path().join("source");
    fs::create_dir_all(&source_root).unwrap();
    let source = source_root.join("session.jsonl");
    fs::write(&source, b"one\ntwo\nthree-in-progress").unwrap();
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");
    let id = scan(&source_root).records[0].id.clone();

    // Pass 1: the two complete lines are sealed; the unterminated tail is
    // left for a later pass to complete.
    let first = pass(&source_root, &stage, &state);
    assert_eq!(first.lines_written, 2);
    assert_eq!(first.changed_records, 1);
    assert_eq!(first.shards_written, 1);
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &id).unwrap(),
        b"one\ntwo\n"
    );

    // Pass 2, source unchanged: the tail is still in progress, so it is
    // re-read from the committed cursor — but no line completed, so nothing
    // new was staged.
    let second = pass(&source_root, &stage, &state);
    assert_eq!(second.lines_written, 0);
    assert_eq!(second.shards_written, 0);
    assert_eq!(second.reset_records, 0);
    assert_eq!(
        second.delta_bytes_read,
        b"three-in-progress".len() as u64,
        "the unterminated tail is deliberately re-read (in-progress probe), \
         without re-reading or re-staging the committed prefix"
    );

    // A pass that staged nothing new must not count the session as changed:
    // this is the assertion that failed before the fix, and its failure is
    // what kept naming this session on every run.
    assert_eq!(
        second.changed_records, 0,
        "a pass that read the tail but committed no complete line staged \
         nothing new, so it must count the session unchanged"
    );
    assert_eq!(
        second.unchanged_records, 1,
        "an unterminated tail that did not complete is in-progress, not a change"
    );

    // Pass 3, still unchanged: convergence holds on every later pass, not
    // only on the second one.
    let third = pass(&source_root, &stage, &state);
    assert_eq!(third.changed_records, 0);
    assert_eq!(third.shards_written, 0);
    assert_eq!(third.unchanged_records, 1);

    // Complete the tail: the newline ends the line, so the pass commits the
    // previously unterminated tail in full — the archive stops short of the
    // source no longer.
    let mut file = fs::OpenOptions::new().append(true).open(&source).unwrap();
    file.write_all(b"\n").unwrap();
    drop(file);
    let fourth = pass(&source_root, &stage, &state);
    assert_eq!(fourth.lines_written, 1);
    assert_eq!(fourth.changed_records, 1);
    assert_eq!(fourth.shards_written, 1);
    assert_eq!(fourth.reset_records, 0);
    assert_eq!(
        fourth.delta_bytes_read,
        b"three-in-progress\n".len() as u64,
        "the tail is re-read from the committed cursor, together with the \
         newline that completed it; the sealed prefix is not re-staged"
    );
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &id).unwrap(),
        b"one\ntwo\nthree-in-progress\n",
        "once the line completes, the tail is committed in full"
    );

    // Pass 5, unchanged again after the completion: back to zero, so the
    // converged state is the steady state and not a one-step transition.
    let fifth = pass(&source_root, &stage, &state);
    assert_eq!(fifth.changed_records, 0);
    assert_eq!(fifth.shards_written, 0);
    assert_eq!(fifth.delta_bytes_read, 0);

    println!(
        "w286 tail first_lines={} tail_bytes={} converged_changed={} converged_unchanged={} \
         completed_lines={} completed_delta_bytes={} final_stage_bytes={}",
        first.lines_written,
        second.delta_bytes_read,
        second.changed_records,
        second.unchanged_records,
        fourth.lines_written,
        fourth.delta_bytes_read,
        store::concat_shards(&stage, "fixture-machine", &id)
            .unwrap()
            .len()
    );
}

/// A codex-style registry cell, the shape of the shipped one that lists
/// `.jsonl` and `.jsonl.zst` side by side: a rollout compressed to
/// `.jsonl.zst` is enumerated with `compressed` set, so the collector takes
/// the decode-everything path these tests pin.
fn compressed_registry() -> HarnessRegistry {
    let cell = json!({
        "template": "~/.codex/sessions",
        "format": "jsonl / jsonl.zst",
        "confidence": "source-confirmed",
        "source": "synthetic fixture"
    });
    let paths = match scanner::current_platform() {
        "macos" => json!({"macos": cell}),
        "linux" => json!({"linux": cell}),
        "windows" => json!({"windows": cell}),
        platform => panic!("unexpected platform: {platform}"),
    };
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic",
        "harnesses": [{
            "id": "codex",
            "display_name": "synthetic",
            "paths": paths
        }]
    }))
    .unwrap()
}

fn scan_compressed(root: &Path) -> scanner::ScanReport {
    let config = Config {
        codex_sessions_dir: Some(root.to_string_lossy().into_owned()),
        ..Config::default()
    };
    scanner::scan_with_registry(&config, &compressed_registry()).unwrap()
}

fn pass_compressed(root: &Path, stage: &Path, state: &Path) -> collect::CollectReport {
    collect::collect_scan_report(
        &scan_compressed(root),
        stage,
        "fixture-machine",
        state,
        20,
        &dest(),
    )
    .unwrap()
}

/// The compressed double of the tail repro: a `.jsonl.zst` rollout whose
/// decoded stream holds a single record and no newline must converge
/// exactly like an uncompressed tail — no reset, no snapshot — and must
/// still commit the record once its newline arrives.
///
/// This path had its own hole: a pass that decoded and found nothing
/// sealable wrote a zero cursor — offset 0, prefix hash of nothing — which
/// can never match a nonempty source, and set its reset flag
/// unconditionally, so every later pass over a byte-identical source
/// re-decoded it and still counted the session as `changed`: the
/// convergence rule this file states was written for plain `.jsonl` and was
/// false for every compressed rollout. Such a pass now records the source it
/// observed — compressed length and digest, the same all-or-nothing cursor a
/// sealing pass writes — so an unchanged source with nothing sealable is a
/// no-op pass.
#[test]
fn compressed_unterminated_record_converges_and_commits_once_complete() {
    let dir = tempfile::TempDir::new().unwrap();
    let source_root = dir.path().join("source");
    fs::create_dir_all(&source_root).unwrap();
    let source = source_root.join("019bf00d-97b6-7eb2-9bf8-0000000003c3.jsonl.zst");
    let record = b"w286 synthetic rollout record, one line, no newline yet";
    let bytes = zstd::stream::encode_all(record.as_slice(), 3).unwrap();
    fs::write(&source, &bytes).unwrap();
    let compressed_len = bytes.len() as u64;
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");
    let id = scan_compressed(&source_root).records[0].id.clone();

    // Pass 1: the decoded stream holds one record and no newline, so no line
    // is sealable and nothing is staged. The pass really did read the whole
    // compressed source — decoding is all-or-nothing — and that read is
    // reported, the same way a tail-probing pass reports its read.
    let first = pass_compressed(&source_root, &stage, &state);
    assert_eq!(first.lines_written, 0);
    assert_eq!(first.shards_written, 0);
    assert_eq!(first.reset_records, 0);
    assert_eq!(first.changed_records, 0);
    assert_eq!(first.unchanged_records, 1);
    assert_eq!(
        first.delta_bytes_read, compressed_len,
        "the first pass reads the compressed source whole to decode it"
    );

    // Passes 2-4, source unchanged: the source is byte-identical to what
    // pass 1 observed, so nothing sealable can have appeared and the pass
    // must be a no-op — no decode, no reset, nothing staged, the session
    // unchanged. Before the fix each of these passes rewrote the zero
    // cursor, re-read and re-decoded the same bytes, flagged `reset`, and
    // counted the session `changed`, so run-once pushed a fresh snapshot of
    // an unchanged stage on every run, forever. This loop is the RED pair.
    for pass_no in 2..=4 {
        let report = pass_compressed(&source_root, &stage, &state);
        assert_eq!(
            report.changed_records, 0,
            "pass {pass_no}: an unchanged source with nothing sealable changed nothing"
        );
        assert_eq!(report.unchanged_records, 1);
        assert_eq!(
            report.reset_records, 0,
            "pass {pass_no}: an unchanged source must not be flagged as a reset"
        );
        assert_eq!(report.shards_written, 0);
        assert_eq!(report.lines_written, 0);
        assert_eq!(
            report.delta_bytes_read, 0,
            "pass {pass_no}: the remembered source digest answers, so nothing is re-read"
        );
    }

    // Complete the record the way the writing harness would: the decoded
    // stream gains its newline, and the compressed bytes change with it, so
    // the remembered observation no longer matches. The record that sat
    // unterminated is sealed in full — convergence must never hide growth.
    let completed = [record.as_slice(), b"\n"].concat();
    fs::write(
        &source,
        zstd::stream::encode_all(completed.as_slice(), 3).unwrap(),
    )
    .unwrap();
    let fifth = pass_compressed(&source_root, &stage, &state);
    assert_eq!(fifth.lines_written, 1);
    assert_eq!(fifth.shards_written, 1);
    assert_eq!(fifth.changed_records, 1);
    assert_eq!(
        store::concat_shards(&stage, "fixture-machine", &id).unwrap(),
        completed,
        "once the record completes, it is committed in full"
    );

    // Pass 6, unchanged again after the completion: back to the no-op, so
    // the converged state is the steady state and not a one-step transition.
    let sixth = pass_compressed(&source_root, &stage, &state);
    assert_eq!(sixth.changed_records, 0);
    assert_eq!(sixth.shards_written, 0);
    assert_eq!(sixth.delta_bytes_read, 0);

    println!(
        "w286 compressed_source_bytes={} converged_changed={} converged_reset={} \
         converged_delta_bytes={} completed_lines={} completed_stage_bytes={}",
        compressed_len,
        sixth.changed_records,
        sixth.reset_records,
        sixth.delta_bytes_read,
        fifth.lines_written,
        store::concat_shards(&stage, "fixture-machine", &id)
            .unwrap()
            .len()
    );
}

/// Run the real binary with every ambient path redirected into `sandbox`.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("rh-cache"))
        .env("CHAT_STASHER_REGISTRY", sandbox.join("registry.json"))
        .output()
        .unwrap()
}

fn write_registry(sandbox: &Path) {
    fs::write(sandbox.join("registry.json"), {
        let cell = json!({
            "template": "~/.claude/projects",
            "format": "jsonl",
            "confidence": "source-confirmed",
            "source": "synthetic fixture"
        });
        let paths = match scanner::current_platform() {
            "macos" => json!({"macos": cell}),
            "linux" => json!({"linux": cell}),
            "windows" => json!({"windows": cell}),
            platform => panic!("unexpected platform: {platform}"),
        };
        serde_json::to_string(&json!({
            "schema_version": 1,
            "generated": "W286 synthetic",
            "harnesses": [{
                "id": "claude-code",
                "display_name": "synthetic",
                "paths": paths
            }]
        }))
        .unwrap()
    })
    .unwrap();
}

/// One synthetic claude-code line with an RFC 3339 timestamp, so the activity
/// index can place the session in time and `search` can report it.
fn cc_line(uuid: &str, ts: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"{uuid}","type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
    )
}

fn count_snapshots(repo: &Path) -> usize {
    let snap_dir = repo.join("snapshots");
    if !snap_dir.exists() {
        return 0;
    }
    fs::read_dir(snap_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .count()
}

/// The whole-loop repro through the real binary: a stable session file with
/// an unterminated last line must not create a new snapshot on every
/// `run-once` — the report measured one new snapshot per run, forever.
#[test]
fn stable_unterminated_tail_run_once_adds_no_new_snapshots() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    write_registry(sandbox);
    let machine = "mbp-w286";
    let stem = "019bf00d-97b6-7eb2-9bf8-0000000001a1";
    let projects = sandbox.join("home").join(".claude").join("projects");
    let tail_session = projects.join("w286-src").join(format!("{stem}.jsonl"));
    let plain_session = projects
        .join("w286-src")
        .join("019bf00d-97b6-7eb2-9bf8-0000000002b2.jsonl");
    fs::create_dir_all(tail_session.parent().unwrap()).unwrap();
    // Two sessions, exactly like the report's fixture: one whose last line is
    // newline-terminated, and one whose last line is a complete-looking JSON
    // record with no final newline.
    let plain_body = format!(
        "{}\n{}\n",
        cc_line(stem, "2026-09-01T12:00:00.000Z"),
        cc_line(stem, "2026-09-01T12:01:00Z")
    );
    let tail_body = format!(
        "{}\n{}{}",
        cc_line(stem, "2026-09-01T13:00:00.000Z"),
        cc_line(stem, "2026-09-01T13:01:00Z"),
        cc_line(stem, "2026-09-01T13:02:00Z")
    );
    fs::write(&plain_session, &plain_body).unwrap();
    fs::write(&tail_session, &tail_body).unwrap();

    let stage = sandbox.join("stage");
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");

    // Run 1: both sessions changed (the tail session seals its complete
    // prefix), so a snapshot is created.
    let out1 = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    let stderr1 = String::from_utf8_lossy(&out1.stderr);
    let all1 = format!("{stdout1}{stderr1}");
    assert!(
        out1.status.success()
            && stdout1.contains("result: COMPLETED")
            && stdout1.contains("snapshot=created"),
        "run 1 must archive the sessions and create the first snapshot:\n{all1}"
    );
    assert!(
        stdout1.contains("[collect] changed=2"),
        "run 1 must see both sessions changed:\n{stdout1}"
    );
    assert_eq!(count_snapshots(&repo), 1);

    // Runs 2 and 3, with nothing changed anywhere: the report measured a new
    // snapshot on every such run (5 -> 6 -> 7 -> 8), so this is the RED pair.
    // Each pass still re-reads the unterminated tail in progress, but it
    // seals nothing, counts nothing changed, and therefore pushes nothing.
    for run_no in [2, 3] {
        let out = run(
            sandbox,
            &[
                "run-once",
                "--stage",
                stage.to_str().unwrap(),
                "--machine",
                machine,
                "--repo",
                repo.to_str().unwrap(),
                "--key-file",
                key.to_str().unwrap(),
                "--keep-ssh-masters",
            ],
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let all = format!("{stdout}{stderr}");
        assert!(
            out.status.success()
                && stdout.contains("result: NOOP")
                && stdout.contains("snapshot=not-created"),
            "run {run_no} on an unchanged machine must take the no-change path:\n{all}"
        );
        assert!(
            stdout.contains("[collect] changed=0"),
            "run {run_no} must count no session changed; the unterminated tail is \
             in progress, not a change:\n{stdout}"
        );
        assert_eq!(
            count_snapshots(&repo),
            1,
            "run {run_no} must add no snapshot; the report measured one per run forever"
        );
    }

    // Complete the tail the way the writing harness would: append the
    // newline that ends the last record. The next pass must commit it —
    // one new snapshot saying what it archived — and the archive must stop
    // reporting the session at its old prefix size.
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&tail_session)
        .unwrap();
    file.write_all(b"\n").unwrap();
    drop(file);
    let out4 = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    let stdout4 = String::from_utf8_lossy(&out4.stdout);
    let stderr4 = String::from_utf8_lossy(&out4.stderr);
    let all4 = format!("{stdout4}{stderr4}");
    assert!(
        out4.status.success()
            && stdout4.contains("result: COMPLETED")
            && stdout4.contains("snapshot=created"),
        "the pass after the tail completed must archive it:\n{all4}"
    );
    assert_eq!(count_snapshots(&repo), 2);

    // The read-back must hold the completed session in full: before the
    // fix the archive stayed at the sealed prefix forever while the source
    // was larger, which is the 'never fully archived' half of the report.
    let search = run(
        sandbox,
        &[
            "search",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    let search_out = String::from_utf8_lossy(&search.stdout);
    let search_err = String::from_utf8_lossy(&search.stderr);
    let search_all = format!("{search_out}{search_err}");
    assert!(
        search.status.success(),
        "search must read the archive back completely:\n{search_all}"
    );
    let tail_short = short_session_id(&format!("claude-code.{machine}.{stem}"));
    let tail_line = search_out
        .lines()
        .find(|line| line.contains(&tail_short))
        .unwrap_or_else(|| panic!("search must list the tail session:\n{search_out}"));
    assert!(
        tail_line.contains(&format!("bytes={}", tail_body.len() + 1)),
        "the archived session must hold the completed tail in full \
         ({} bytes), line: {tail_line}",
        tail_body.len() + 1
    );
    assert_ne!(
        tail_body.len() + 1,
        plain_body.len(),
        "sanity: the two sessions' expected sizes differ, so the byte \
         assertions above distinguish them"
    );

    println!(
        "w286 run_once snapshots_after_runs_1_3={} snapshots_after_completion={} \
         tail_prefix_bytes={} tail_complete_bytes={}",
        count_snapshots(&repo) - 1,
        count_snapshots(&repo),
        tail_body.len(),
        tail_body.len() + 1
    );
}
