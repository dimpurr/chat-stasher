//! B91 end-to-end: `activity-index` writes the sidecar, `push` archives it,
//! and `overview` reads it back out of the repository and renders it.
//!
//! This is the ADR-017 round-trip pinned as a black-box test through the real
//! binary and a real rustic repository. The three exit-code rules of `overview`
//! are asserted directly: 0 = read and rendered an index, 1 = read the whole
//! repo and there is no index anywhere (a real empty), and the machine-with-
//! snapshot-but-no-index case is *named*, never silently dropped.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"B91 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

/// One synthetic claude-code line with an RFC 3339 timestamp.
fn cc_line(ts: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
    )
}

/// Write one sealed shard for a session named `session` under `machine`.
fn write_shard(stage: &Path, machine: &str, session: &str, lines: &[String]) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("000001.jsonl"), lines.join("\n") + "\n").unwrap();
}

fn sandbox() -> tempfile::TempDir {
    tempfile::TempDir::new().unwrap()
}

/// Happy path: index a stage, push it, and read the overview back. The machine
/// and harness must appear and the command must exit 0.
#[test]
fn activity_index_then_push_then_overview_roundtrips() {
    let sb = sandbox();
    let stage = sb.path().join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    write_shard(
        &stage,
        machine,
        session,
        &[
            cc_line("2025-01-15T12:34:56.789Z"),
            cc_line("2025-01-15T13:45:07Z"),
        ],
    );

    // 1. Build the sidecar.
    let out = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(
        out.status.success(),
        "activity-index failed: {:?}",
        out.status
    );
    let index = stage.join("meta").join(machine).join("activity-v1.jsonl");
    assert!(index.exists(), "activity index was not written");
    assert_eq!(fs::read_to_string(&index).unwrap().lines().count(), 1);

    // 2. Push the stage (sessions/ and meta/ together) into a fresh repo.
    let repo = sb.path().join("repo");
    let key = sb.path().join("keys").join("masterkey.json");
    let push = run(
        sb.path(),
        &[
            "push",
            "--stage",
            stage.to_str().unwrap(),
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            machine,
            "--keep-ssh-masters",
        ],
    );
    assert!(
        push.status.success(),
        "push failed: {:?}\n{}",
        push.status,
        String::from_utf8_lossy(&push.stderr)
    );

    // 3. Read the overview back.
    let ov = run(
        sb.path(),
        &[
            "overview",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--width",
            "80",
            "--keep-ssh-masters",
        ],
    );
    let stdout = String::from_utf8_lossy(&ov.stdout);
    assert_eq!(
        ov.status.code(),
        Some(0),
        "overview should exit 0, got {:?}\n{}",
        ov.status,
        stdout
    );
    assert!(
        stdout.contains("mbp-test"),
        "overview must name the machine:\n{stdout}"
    );
    assert!(
        stdout.contains("claude-code"),
        "overview must name the harness:\n{stdout}"
    );
    assert!(
        !stdout.contains("index missing"),
        "an indexed machine must not be flagged missing:\n{stdout}"
    );
}

/// A machine that has a snapshot but no activity index must be *named* — it
/// must never vanish silently (that would fold "no index" into "no sessions").
#[test]
fn machine_with_snapshot_but_no_index_is_listed_missing() {
    let sb = sandbox();
    let stage = sb.path().join("stage");
    let machine = "mbp-test";
    write_shard(
        &stage,
        machine,
        "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
        &[cc_line("2025-01-15T12:34:56.789Z")],
    );
    // Deliberately NO meta/... — the machine archives sessions but never ran
    // activity-index, so it has a snapshot with no index.
    let repo = sb.path().join("repo");
    let key = sb.path().join("keys").join("masterkey.json");
    let push = run(
        sb.path(),
        &[
            "push",
            "--stage",
            stage.to_str().unwrap(),
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            machine,
            "--keep-ssh-masters",
        ],
    );
    assert!(push.status.success(), "push failed: {:?}", push.status);

    let ov = run(
        sb.path(),
        &[
            "overview",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--width",
            "80",
            "--keep-ssh-masters",
        ],
    );
    let stdout = String::from_utf8_lossy(&ov.stdout);
    // No index anywhere -> a real "there is none", exit 1 (never 3, and never 0).
    assert_eq!(
        ov.status.code(),
        Some(1),
        "overview with no index must exit 1, got {:?}\n{}",
        ov.status,
        stdout
    );
    assert!(
        stdout.contains("mbp-test") && stdout.contains("index missing"),
        "the snapshot-without-index machine must be named as missing:\n{stdout}"
    );
}

/// An unreadable / un-openable repository is "could not finish reading": exit 3,
/// never 1. "No index" (1) must stay distinct from "could not look" (3).
#[test]
fn unopenable_repo_is_exit_3_not_exit_1() {
    let sb = sandbox();
    let repo = sb.path().join("repo-does-not-exist");
    let key = sb.path().join("keys").join("masterkey.json");
    let ov = run(
        sb.path(),
        &[
            "overview",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--width",
            "80",
            "--keep-ssh-masters",
        ],
    );
    assert_eq!(
        ov.status.code(),
        Some(3),
        "an unreadable repository must exit 3 (did not finish reading), not 1\n{}",
        String::from_utf8_lossy(&ov.stdout)
    );
}
