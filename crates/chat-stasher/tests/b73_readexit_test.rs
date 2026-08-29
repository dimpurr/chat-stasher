//! B73 regression: every `read` failure must use the repository's exit-code
//! contract, while a complete read remains success.
//!
//! Exit-code contract exercised here:
//!   * 3 — the archive was not read or the read did not finish;
//!   * 2 — the command was used without a required argument;
//!   * 0 — the read completed successfully.
//!
//! All repositories, key files, stages, and environment directories are
//! synthetic `tempfile` fixtures. Assertions inspect exit codes only; no
//! session body is printed or read by the test itself.

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn config(repo: &Path, key: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: None,
        no_cache: false,
    }
}

fn isolated_read_command(sandbox: &Path, repo: &Path, key: &Path) -> Command {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .args(["read", "--repo"])
        .arg(repo)
        .args(["--key-file"])
        .arg(key)
        .args(["--keep-ssh-masters"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("cache"))
        .env_remove("CODEX_HOME")
        .env_remove("RUSTIC_REPO")
        .env_remove("RUSTIC_KEY_FILE");
    command
}

fn run_read(sandbox: &Path, repo: &Path, key: &Path, extra: &[&str]) -> Output {
    let mut command = isolated_read_command(sandbox, repo, key);
    command.args(extra);
    command.output().unwrap()
}

fn write_key(repo: &Path, key: &Path) -> MasterKey {
    let mk = MasterKey::new();
    store::persist_key_file(&config(repo, key), &mk).unwrap();
    mk
}

/// The pre-fix behavior is intentionally covered by the assertions below:
/// before B73 changes, each affected case returned 1 and this test failed.
#[test]
fn missing_key_is_not_finished_read_failure() {
    let sandbox = tempfile::tempdir().unwrap();
    let output = run_read(
        sandbox.path(),
        &sandbox.path().join("repo-not-created"),
        &sandbox.path().join("key-not-created.json"),
        &["--all-machines"],
    );
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn missing_stage_is_usage_error() {
    let sandbox = tempfile::tempdir().unwrap();
    let repo = sandbox.path().join("repo");
    let key = sandbox.path().join("key.json");
    write_key(&repo, &key);
    let output = run_read(sandbox.path(), &repo, &key, &["--machine", "b73-machine"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn missing_session_is_usage_error() {
    let sandbox = tempfile::tempdir().unwrap();
    let repo = sandbox.path().join("repo");
    let key = sandbox.path().join("key.json");
    write_key(&repo, &key);
    let stage = sandbox.path().join("stage");
    let output = run_read(
        sandbox.path(),
        &repo,
        &key,
        &[
            "--machine",
            "b73-machine",
            "--stage",
            stage.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn session_readback_failure_is_not_finished_read_failure() {
    let sandbox = tempfile::tempdir().unwrap();
    let repo = sandbox.path().join("repo-not-created");
    let key = sandbox.path().join("key.json");
    write_key(&repo, &key);
    let output = run_read(
        sandbox.path(),
        &repo,
        &key,
        &[
            "--machine",
            "b73-machine",
            "--stage",
            sandbox.path().join("stage").to_str().unwrap(),
            "--session",
            "b73-session",
        ],
    );
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn all_machines_repository_failure_is_not_finished_read_failure() {
    let sandbox = tempfile::tempdir().unwrap();
    let repo = sandbox.path().join("repo-not-created");
    let key = sandbox.path().join("key.json");
    write_key(&repo, &key);
    let output = run_read(sandbox.path(), &repo, &key, &["--all-machines"]);
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn successful_all_machines_read_still_exits_zero() {
    let sandbox = tempfile::tempdir().unwrap();
    let repo = sandbox.path().join("repo");
    let key = sandbox.path().join("key.json");
    let mk = write_key(&repo, &key);
    let stage = sandbox.path().join("stage");
    store::write_sealed_shard(
        StageWriter::Collect,
        &stage,
        "b73-machine",
        "b73-session",
        &["{\"synthetic\":true}".to_string()],
    )
    .unwrap();
    BackupStore::new(config(&repo, &key), "b73-machine".to_string())
        .push(&stage, &mk)
        .unwrap();

    let output = run_read(sandbox.path(), &repo, &key, &["--all-machines"]);
    assert_eq!(output.status.code(), Some(0));
}
