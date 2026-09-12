//! ADR-022 regression tests:
//! 1. Modifying `machine.json` with no new shards triggers run-once push (previously NOOP).
//! 2. Running twice with no changes results in NOOP on the second run (activity index rewrite does not trigger push).
//! 3. Stage with 0 shards but meta change can push and snapshot includes meta; 0 shards and 0 meta is refused.
//! 4. Push failure does not update the recorded hash; the next run still pushes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    if !registry.exists() {
        fs::write(
            &registry,
            r#"{"schema_version":1,"generated":"ADR-022 synthetic","harnesses":[]}"#,
        )
        .unwrap();
    }
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

fn write_dummy_shard(stage: &Path, machine: &str, session: &str) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("000001.jsonl"),
        r#"{"sessionId":"s","type":"user","message":{"role":"user","content":"hi"},"uuid":"u1","timestamp":"2026-01-01T00:00:00Z"}"#.to_string() + "\n",
    )
    .unwrap();
}

fn write_machine_json(stage: &Path, machine: &str, display_name: &str) -> PathBuf {
    let meta_dir = stage.join("meta").join(machine);
    fs::create_dir_all(&meta_dir).unwrap();
    let path = meta_dir.join("machine.json");
    let content = format!(
        r#"{{"machine_id":"{machine}","display_name":"{display_name}","os":"macos","first_seen_unix":1700000000,"declared_harnesses":["claude-code"]}}"#
    );
    fs::write(&path, content).unwrap();
    path
}

/// Test 1: Only `machine.json` changed, no new shards -> run-once pushes (previously NOOP).
#[test]
fn only_machine_json_changed_triggers_push() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let machine = "mbp-test1";
    let session = "claude-code.mbp-test1.019bf00d-97b6-7eb2-9bf8-eacbacc09765";

    write_dummy_shard(&stage, machine, session);
    write_machine_json(&stage, machine, "Initial Name");

    // Run 1: Initial push of shard and initial machine.json
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
    assert!(out1.status.success(), "run 1 failed: {stdout1}");
    assert!(
        stdout1.contains("snapshot=created"),
        "run 1 must create snapshot: {stdout1}"
    );
    assert_eq!(count_snapshots(&repo), 1);

    // Run 2: Second run without any changes -> must be NOOP
    let out2 = run(
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
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(out2.status.success(), "run 2 failed: {stdout2}");
    assert!(
        stdout2.contains("snapshot=not-created"),
        "run 2 must be NOOP: {stdout2}"
    );
    assert_eq!(count_snapshots(&repo), 1);

    // Now change ONLY machine.json (no new shards)
    write_machine_json(&stage, machine, "Renamed Machine");

    // Run 3: Must trigger push because meta changed!
    let out3 = run(
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
    let stdout3 = String::from_utf8_lossy(&out3.stdout);
    let stderr3 = String::from_utf8_lossy(&out3.stderr);
    let all3 = format!("{stdout3}\n{stderr3}");
    assert!(out3.status.success(), "run 3 failed: {all3}");
    assert!(
        stdout3.contains("snapshot=created"),
        "run 3 must create snapshot on meta change, but got: {all3}"
    );
    assert_eq!(
        count_snapshots(&repo),
        2,
        "must have created second snapshot in repo"
    );
}

/// Test 2: Running twice with no changes -> second run is NOOP (activity index rewrite must not trigger push).
#[test]
fn no_changes_twice_second_run_is_noop() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let machine = "mbp-test2";
    let session = "claude-code.mbp-test2.019bf00d-97b6-7eb2-9bf8-eacbacc09765";

    write_dummy_shard(&stage, machine, session);
    write_machine_json(&stage, machine, "Test Machine 2");

    // Force push first to establish repository snapshot 1
    let out_init = run(
        sandbox,
        &[
            "push",
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
    assert!(out_init.status.success(), "initial push failed");
    assert_eq!(count_snapshots(&repo), 1);

    // Run 1 of run-once: syncs/records meta hash, nothing new to push -> NOOP
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
    assert!(out1.status.success(), "run 1 failed: {stdout1}");
    assert!(
        stdout1.contains("snapshot=not-created"),
        "run 1 should be NOOP: {stdout1}"
    );
    assert_eq!(count_snapshots(&repo), 1);

    // Run 2 of run-once: immediately run again without changes -> MUST STILL BE NOOP
    let out2 = run(
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
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(out2.status.success(), "run 2 failed: {stdout2}");
    assert!(
        stdout2.contains("snapshot=not-created"),
        "second run must be NOOP even if activity index was touched:\n{stdout2}"
    );
    assert_eq!(count_snapshots(&repo), 1);
}

/// Test 3: Stage with 0 shards but meta change can push and snapshot carries meta;
/// 0 shards and 0 meta is still refused.
#[test]
fn stage_zero_shards_with_meta_change_can_push_and_zero_meta_refused() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let machine = "mbp-test3";

    // 3b: 0 shards AND 0 meta -> run-once must not push and no snapshot created
    fs::create_dir_all(&stage).unwrap();
    let out_empty = run(
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
    let stdout_empty = String::from_utf8_lossy(&out_empty.stdout);
    assert!(
        stdout_empty.contains("snapshot=not-created"),
        "0 shards and 0 meta must not push: {stdout_empty}"
    );
    assert_eq!(count_snapshots(&repo), 0);

    // Also test direct push command: must not create snapshot when 0 shards and 0 meta
    let _out_cmd_push_empty = run(
        sandbox,
        &[
            "push",
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
    assert_eq!(
        count_snapshots(&repo),
        0,
        "direct push on 0 shards and 0 meta must not create snapshot"
    );

    // 3a: Stage has 0 shards, BUT has meta/machine.json -> can push and snapshot is created!
    write_machine_json(&stage, machine, "Declared Alone");

    let out_meta = run(
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
    let stdout_meta = String::from_utf8_lossy(&out_meta.stdout);
    let stderr_meta = String::from_utf8_lossy(&out_meta.stderr);
    let all_meta = format!("{stdout_meta}\n{stderr_meta}");
    assert!(
        out_meta.status.success(),
        "run-once with 0 shards but meta present must succeed: {all_meta}"
    );
    assert!(
        stdout_meta.contains("snapshot=created"),
        "run-once with 0 shards but meta present must create snapshot: {all_meta}"
    );
    assert_eq!(count_snapshots(&repo), 1);
}

/// Test 4: Push failure does not update recorded hash; subsequent run pushes.
#[test]
fn push_failure_does_not_update_hash_and_subsequent_run_pushes() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let valid_repo = sandbox.join("repo");
    let invalid_repo = sandbox.join("file_blocking_repo");
    fs::write(&invalid_repo, "not a directory").unwrap();
    let key = sandbox.join("keys").join("masterkey.json");
    let machine = "mbp-test4";
    let session = "claude-code.mbp-test4.019bf00d-97b6-7eb2-9bf8-eacbacc09765";

    write_dummy_shard(&stage, machine, session);
    write_machine_json(&stage, machine, "Initial 4");

    // Run 1: Initial successful push to valid repo
    let out1 = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
            "--repo",
            valid_repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    assert!(out1.status.success());
    assert_eq!(count_snapshots(&valid_repo), 1);

    // Modify machine.json
    write_machine_json(&stage, machine, "Updated 4");

    // Run 2: Attempt push to an invalid repo path that fails
    let out2 = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
            "--repo",
            invalid_repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    assert!(!out2.status.success(), "push to invalid repo must fail");

    // Run 3: Now run with valid repo again -> because hash was NOT updated on failure,
    // this run MUST still detect the change and push!
    let out3 = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
            "--repo",
            valid_repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    let stdout3 = String::from_utf8_lossy(&out3.stdout);
    assert!(
        out3.status.success(),
        "run 3 with valid repo must succeed: {stdout3}"
    );
    assert!(
        stdout3.contains("snapshot=created"),
        "run 3 must create snapshot because failed run did not advance hash: {stdout3}"
    );
    assert_eq!(count_snapshots(&valid_repo), 2);
}
