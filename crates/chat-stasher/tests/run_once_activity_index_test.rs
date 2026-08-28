//! T1 — `run-once` must rebuild the activity index before pushing, so the
//! snapshot that leaves the stage carries an index that reflects what was just
//! collected, instead of one frozen at the last manual `activity-index` build.
//!
//! Black-box through the real binary and a real rustic repository: a stale
//! index (old content, mtime two days ago) must come back rewritten and with a
//! fresh mtime after one `run-once`, and the pass must report the index step.
//! The stage is pushed into a fresh local repo exactly like `push` does, so the
//! whole collect -> index -> push sequence is exercised.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

/// Run the real binary with every ambient path redirected into `sandbox`.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"T1 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
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

/// A stage with one pre-sealed session plus a deliberately stale index whose
/// mtime is two days old. After a correct `run-once` both content and mtime
/// must change.
#[test]
fn run_once_rebuilds_the_activity_index_before_push() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";

    // The scheduled pass must reach the push path even when nothing new was
    // collected (the stage already holds sealed shards): disable the
    // no-change skip so `run-once` pushes and rebuilds the index.
    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        "push_only_if_changed = false\n",
    )
    .unwrap();

    write_shard(
        &stage,
        machine,
        session,
        &[
            cc_line("2025-01-15T12:34:56.789Z"),
            cc_line("2025-01-15T13:45:07Z"),
        ],
    );

    let index = stage.join("meta").join(machine).join("activity-v1.jsonl");
    fs::create_dir_all(index.parent().unwrap()).unwrap();
    fs::write(&index, "stale\n").unwrap();
    let stale_mtime = SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
    fs::OpenOptions::new()
        .write(true)
        .open(&index)
        .unwrap()
        .set_modified(stale_mtime)
        .unwrap();

    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
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
            "--no-reap",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let all = format!("{stdout}{stderr}");
    assert!(
        out.status.success(),
        "run-once should exit 0, got {:?}\n{all}",
        out.status
    );

    // The pass must report the index step with the session count.
    assert!(
        stdout.contains("[run-once] activity-index: sessions=1"),
        "run-once must report the index step on stdout:\n{all}"
    );

    // Content was rebuilt: one row naming the machine and harness, no stale bytes.
    let content = fs::read_to_string(&index).unwrap();
    assert!(
        !content.contains("stale"),
        "a stale index must be rebuilt, not left behind:\n{content}"
    );
    let rows: Vec<&str> = content.lines().collect();
    assert_eq!(rows.len(), 1, "one session -> one row:\n{content}");
    assert!(
        content.contains(&format!("\"machine\":\"{machine}\"")),
        "row must name the machine:\n{content}"
    );
    assert!(
        content.contains("claude-code"),
        "row must name the harness:\n{content}"
    );

    // mtime advanced past the stale marker.
    let new_mtime = fs::metadata(&index).unwrap().modified().unwrap();
    assert!(
        new_mtime > stale_mtime,
        "index mtime must advance on rebuild"
    );
}
