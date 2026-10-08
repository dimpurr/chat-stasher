//! B75 regression: `dest-init` must not collapse unread, read-failed, and
//! push-not-started states into one exit code.
//!
//! Every fixture is synthetic and isolated. The command is allowed to inspect
//! only registry metadata and synthetic stage/archive bytes; assertions inspect
//! exit codes and metadata-only diagnostic lines, never session content.

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "../src/test_support.rs"]
mod test_support;

const MACHINE: &str = "b75-fixture";
const SESSION: &str = "b75.synthetic-session";

/// The source repository's metadata cache lives under this test's own
/// directory (W289), part of the same isolation the spawned `dest-init`
/// children below get through their config's `rustic_cache_dir` and their
/// sandbox HOME. The cache itself stays enabled — dest-init's difference
/// counts are read paths, and a real destination read is a cached one.
fn cfg(repo: &Path, key: &Path, cache: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(cache.join("rustic-cache")),
        no_cache: false,
    }
}

fn synthetic_stage(root: &Path) -> PathBuf {
    let stage = root.join("stage");
    store::write_sealed_shard(
        StageWriter::Collect,
        &stage,
        MACHINE,
        SESSION,
        &["B75 synthetic shard".to_string()],
    )
    .unwrap();
    stage
}

fn empty_registry(root: &Path) -> PathBuf {
    let path = root.join("registry.json");
    fs::write(
        &path,
        r#"{"schema_version":1,"generated":"B75 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    path
}

fn write_config(
    root: &Path,
    target_repo: &Path,
    target_key: &Path,
    source_repo: &Path,
    source_key: &Path,
) {
    let config = root.join("config/chat-stasher/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        config,
        format!(
            // `rustic_cache_dir` is what keeps the spawned dest-init from
            // writing its metadata cache into the real user cache dir
            // (W289). It is a relocation, not a feature flag: the cache stays
            // on, exactly as a configured destination read has it.
            "rustic_cache_dir = '{}'\n\n[destinations.target]\nrepo = '{}'\nkey_file = '{}'\n\n[destinations.source]\nrepo = '{}'\nkey_file = '{}'\n",
            root.join("rustic-cache").display(),
            target_repo.display(),
            target_key.display(),
            source_repo.display(),
            source_key.display(),
        ),
    )
    .unwrap();
}

fn command(root: &Path, registry: &Path) -> Command {
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .env("HOME", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("CHAT_STASHER_REGISTRY", registry)
        .env_remove("CODEX_HOME")
        .env_remove("OPENCODE_DB")
        .env_remove("B75_SOURCE");
    command
}

fn run_empty_dest_init(
    root: &Path,
    registry: &Path,
    stage: &Path,
    repo: &Path,
    key: &Path,
) -> Output {
    let mut command = command(root, registry);
    command
        .args(["dest-init", "--stage"])
        .arg(stage)
        .args(["--machine", MACHINE, "--repo"])
        .arg(repo)
        .args(["--key-file"])
        .arg(key)
        .args(["--keep-ssh-masters"]);
    command.output().unwrap()
}

fn run_named_dest_init(root: &Path, registry: &Path, stage: &Path) -> Output {
    let mut command = command(root, registry);
    command
        .args([
            "dest-init",
            "--destination",
            "target",
            "--from",
            "source",
            "--stage",
        ])
        .arg(stage)
        .args(["--machine", MACHINE, "--keep-ssh-masters"]);
    command.output().unwrap()
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The old code used `report.errors` only. A recognised but unarchivable
/// harness therefore reached the success line and exit 0.
#[test]
fn archive_gap_is_not_a_completed_union() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("registry.json");
    let cell = r#"{"template":"$OPENCODE_DB","env_override":"OPENCODE_DB","format":"sqlite","confidence":"source-confirmed","source":"B75 synthetic"}"#;
    fs::write(
        &registry,
        format!(
            r#"{{"schema_version":1,"generated":"B75 synthetic","harnesses":[{{"id":"opencode","display_name":"synthetic opencode","paths":{{"macos":{cell},"linux":{cell},"windows":{cell}}}}}]}}"#
        ),
    )
    .unwrap();
    let db = root.path().join("not-a-sqlite.db");
    fs::write(&db, b"B75 synthetic database marker").unwrap();
    let stage = root.path().join("stage");
    let output = {
        let mut command = command(root.path(), &registry);
        command
            .args(["dest-init", "--stage"])
            .arg(&stage)
            .args(["--machine", MACHINE, "--repo"])
            .arg(root.path().join("target-repo"))
            .args(["--key-file"])
            .arg(root.path().join("target-key.json"))
            .env("OPENCODE_DB", &db)
            .args(["--keep-ssh-masters"]);
        command.output().unwrap()
    };
    let (stdout, stderr) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains("exit_code=3"),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("[collect] not archivable  : 1 harness(es)"),
        "stdout={stdout}"
    );
    assert!(!stdout.contains("COMPLETED exit_code=0"), "stdout={stdout}");
}

/// An existing source with no readable key is an unread difference source, not
/// a completed read whose result merely failed.
#[test]
fn unread_difference_source_exits_three() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    let source_repo = root.path().join("source-repo");
    let source_key = root.path().join("source-key.json");
    let source_cfg = cfg(&source_repo, &source_key, root.path());
    let source_stage = synthetic_stage(root.path());
    let key = MasterKey::new();
    store::persist_key_file(&source_cfg, &key).unwrap();
    BackupStore::new(source_cfg, MACHINE.to_string())
        .push(&source_stage, &key)
        .unwrap();
    fs::remove_file(&source_key).unwrap();

    let target_repo = root.path().join("target-repo");
    let target_key = root.path().join("target-key.json");
    write_config(
        root.path(),
        &target_repo,
        &target_key,
        &source_repo,
        &source_key,
    );
    let output = run_named_dest_init(root.path(), &registry, &root.path().join("empty-stage"));
    let (stdout, stderr) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains("INCOMPLETE exit_code=3"),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("[dest-init] diff complete : false"),
        "stdout={stdout}"
    );
}

/// `CollectReport::errors` means a recognised source was read and that read
/// failed. It stays in the completed-read failure family, exit 1.
#[test]
fn local_collect_error_exits_one() {
    let root = tempfile::tempdir().unwrap();
    let registry = root.path().join("registry.json");
    let cell = r#"{"template":"$B75_SOURCE/.codex/sessions/<date>","env_override":"B75_SOURCE","env_override_layer":".codex/","format":"jsonl.zst","confidence":"source-confirmed","source":"B75 synthetic"}"#;
    fs::write(
        &registry,
        format!(
            r#"{{"schema_version":1,"generated":"B75 synthetic","harnesses":[{{"id":"codex","display_name":"synthetic codex","paths":{{"macos":{cell},"linux":{cell},"windows":{cell}}}}}]}}"#
        ),
    )
    .unwrap();
    let source_root = root.path().join("codex-root");
    let source_dir = source_root.join("sessions");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(source_dir.join("broken.jsonl.zst"), b"B75 not zstd").unwrap();
    let stage = root.path().join("stage");
    let mut command = command(root.path(), &registry);
    command
        .args(["dest-init", "--stage"])
        .arg(&stage)
        .args(["--machine", MACHINE, "--repo"])
        .arg(root.path().join("target-repo"))
        .args(["--key-file"])
        .arg(root.path().join("target-key.json"))
        .env("B75_SOURCE", source_root)
        .args(["--keep-ssh-masters"]);
    let output = command.output().unwrap();
    let (stdout, stderr) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains("local_errors=true"),
        "stdout={stdout}\nstderr={stderr}"
    );
}

/// A push that reaches rustic and fails remains a completed-operation failure.
#[test]
fn attempted_push_failure_exits_one() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    let stage = synthetic_stage(root.path());
    let repo_file = root.path().join("repo-is-a-file");
    fs::write(&repo_file, b"B75 not a repository directory").unwrap();
    let output = run_empty_dest_init(
        root.path(),
        &registry,
        &stage,
        &repo_file,
        &root.path().join("target-key.json"),
    );
    let (stdout, stderr) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains("push_failed=true"),
        "stdout={stdout}\nstderr={stderr}"
    );
}

/// If the key cannot be persisted, push never starts. That is not the same as
/// a push which started and failed, so it belongs to exit 3.
#[test]
fn push_not_started_exits_three() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    let stage = synthetic_stage(root.path());
    let blocker = root.path().join("key-parent-is-a-file");
    fs::write(&blocker, b"B75 not a directory").unwrap();
    let output = run_empty_dest_init(
        root.path(),
        &registry,
        &stage,
        &root.path().join("target-repo"),
        &blocker.join("target-key.json"),
    );
    let (stdout, stderr) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stderr.contains("nothing was pushed"),
        "stdout={stdout}\nstderr={stderr}"
    );
}

/// `dest-init` is the non-wizard path to a destination, and it creates that
/// destination's **own** key. It therefore has to name the file where the user
/// can see it: a second machine reads this copy with `masterkey-<name>.json`
/// (or the `key_file` this destination declares), never with the local
/// `masterkey.json`, so a user who backs up the local key alone cannot recover
/// this copy (W281 BUG-2). The wizard says this at step 4; the command a user
/// runs instead of the wizard has to say it too.
#[test]
fn dest_init_names_the_destination_key_it_created() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    let key = root.path().join("target-key.json");
    let output = run_empty_dest_init(
        root.path(),
        &registry,
        &synthetic_stage(root.path()),
        &root.path().join("target-repo"),
        &key,
    );
    let (stdout, stderr) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains(&format!("[dest-init] key           : {}", key.display())),
        "dest-init created the destination's key and must name it; stdout={stdout}"
    );
}

#[test]
fn normal_dest_init_stays_zero_and_keeps_completion_line() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    let output = run_empty_dest_init(
        root.path(),
        &registry,
        &root.path().join("empty-stage"),
        &root.path().join("target-repo"),
        &root.path().join("target-key.json"),
    );
    let (stdout, stderr) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains(
            "[dest-init] result: COMPLETED exit_code=0 union=local+existing-destinations"
        ),
        "stdout={stdout}"
    );
}
