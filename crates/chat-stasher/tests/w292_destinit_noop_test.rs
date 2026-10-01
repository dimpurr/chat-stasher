//! W292 (OBS-5 from the WIZ-5 real-machine gate): a `dest-init` that has
//! nothing new to publish must not publish a snapshot anyway.
//!
//! W291 measured the defect on real machines: re-running `setup` (which runs
//! `dest-init`) appended one snapshot to the destination per invocation — 2
//! after two runs, while the local repository held 1 — although the stage had
//! not changed between them, and the second push added `files_new=0
//! data_added=0`. `run-once` is no-op aware; `dest-init` was not.
//!
//! The reason a destination-side comparison is required, and not a reuse of
//! `run-once`'s stage-side one, is measured by the second test here: a
//! *fresh* destination is also one where step 1 wrote no shard and step 2
//! restored none (the stage already held them, and the new destination held
//! nothing), so "this pass changed nothing" cannot tell "the destination
//! already has it" from "the destination has nothing at all". Only what the
//! destination itself holds can.
//!
//! Every fixture is synthetic: one opaque JSONL line per session, a stage of
//! sealed shards, and a local-path destination. Assertions are snapshot
//! counts, session ids and exit codes — never session content.

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "../src/test_support.rs"]
mod test_support;

const MACHINE: &str = "w292-fixture";
const SESSION_ONE: &str = "w292.synthetic-session-one";
const SESSION_TWO: &str = "w292.synthetic-session-two";

/// A destination opened from this test is cached under its own directory
/// (W289): the cache stays on — a real destination read is a cached one — only
/// its location moves. The spawned children get the same isolation through
/// `CHAT_STASHER_RUSTIC_CACHE_DIR` in [`command`].
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

/// One sealed synthetic session, in a stage of its own.
fn stage_with(root: &Path, session: &str) -> PathBuf {
    let stage = root.join("stage");
    store::write_sealed_shard(
        StageWriter::Collect,
        &stage,
        MACHINE,
        session,
        &[format!("{session} synthetic shard")],
    )
    .unwrap();
    stage
}

/// No harness is installed in the sandbox: the stage holds the shards and the
/// collect pass has nothing to re-read, so every assertion below is about
/// `dest-init`'s push and nothing about what the host running the suite has
/// installed.
fn empty_registry(root: &Path) -> PathBuf {
    let path = root.join("registry.json");
    fs::write(
        &path,
        r#"{"schema_version":1,"generated":"W292 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    path
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
        .env("CHAT_STASHER_REGISTRY", registry);
    command
}

/// A config file, written verbatim. `extra` carries top-level settings such as
/// `push_only_if_changed`.
fn write_config(root: &Path, extra: &str) {
    let path = root.join("config/chat-stasher/config.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, extra).unwrap();
}

fn run_dest_init(root: &Path, registry: &Path, stage: &Path, repo: &Path, key: &Path) -> Output {
    let mut command = command(root, registry);
    command
        .args(["dest-init", "--stage"])
        .arg(stage)
        .args(["--machine", MACHINE, "--repo"])
        .arg(repo)
        .args(["--key-file"])
        .arg(key);
    command.output().unwrap()
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// How many snapshots the destination holds. `get_all_snapshots` loads every
/// snapshot file, so this is a count of what is really there, not of a cached
/// listing.
fn destination_snapshots(cfg: &StoreConfig) -> usize {
    let mk = store::load_key_file(cfg).expect("read the destination key");
    let backends = BackupStore::for_metadata_query(cfg.clone())
        .backends()
        .expect("destination backends");
    let (repo, _adoption) =
        chat_stasher::orphans::open_adopting(cfg, &backends, &mk).expect("open destination");
    repo.get_all_snapshots()
        .expect("list destination snapshots")
        .len()
}

/// The defect: two `dest-init` runs over one unchanged stage, one snapshot.
#[test]
fn re_running_dest_init_over_an_unchanged_stage_publishes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    let stage = stage_with(root.path(), SESSION_ONE);
    let repo = root.path().join("destination");
    let key = root.path().join("destination-key.json");

    let first = run_dest_init(root.path(), &registry, &stage, &repo, &key);
    let (stdout, stderr) = text(&first);
    assert_eq!(
        first.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    let after_first = destination_snapshots(&cfg(&repo, &key, root.path()));
    assert_eq!(
        after_first, 1,
        "the first dest-init must seed the destination: stdout={stdout}"
    );

    let second = run_dest_init(root.path(), &registry, &stage, &repo, &key);
    let (stdout, stderr) = text(&second);
    assert_eq!(
        second.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    let after_second = destination_snapshots(&cfg(&repo, &key, root.path()));
    assert_eq!(
        after_second, after_first,
        "a second dest-init over an unchanged stage published another snapshot \
         ({} -> {after_second}); the destination already held what would be published, \
         so nothing may be published: stdout={stdout}",
        after_first,
    );
    assert!(
        stdout.contains("[dest-init] push skipped"),
        "the run must say it published nothing: stdout={stdout}"
    );
}

/// The trap the destination-side comparison exists for: a *fresh* destination
/// whose stage already holds everything. Step 1 wrote no shard and step 2
/// restored none — exactly the counters an unchanged re-run produces — and yet
/// the destination holds nothing and must be seeded.
#[test]
fn a_fresh_destination_is_still_seeded_when_the_stage_already_holds_the_shards() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    let stage = stage_with(root.path(), SESSION_ONE);
    let repo = root.path().join("destination");
    let key = root.path().join("destination-key.json");

    let output = run_dest_init(root.path(), &registry, &stage, &repo, &key);
    let (stdout, stderr) = text(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        !stdout.contains("push skipped"),
        "this destination holds nothing; it must be given the stage: stdout={stdout}"
    );
    assert_eq!(
        destination_snapshots(&cfg(&repo, &key, root.path())),
        1,
        "stdout={stdout}"
    );
}

/// The other direction: new content on the stage must still be published.
#[test]
fn new_stage_content_is_still_published() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    let stage = stage_with(root.path(), SESSION_ONE);
    let repo = root.path().join("destination");
    let key = root.path().join("destination-key.json");

    let first = run_dest_init(root.path(), &registry, &stage, &repo, &key);
    assert_eq!(first.status.code(), Some(0));
    assert_eq!(
        destination_snapshots(&cfg(&repo, &key, root.path())),
        1,
        "seed"
    );

    store::write_sealed_shard(
        StageWriter::Collect,
        &stage,
        MACHINE,
        SESSION_TWO,
        &[format!("{SESSION_TWO} synthetic shard")],
    )
    .unwrap();

    let second = run_dest_init(root.path(), &registry, &stage, &repo, &key);
    let (stdout, stderr) = text(&second);
    assert_eq!(
        second.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        !stdout.contains("push skipped"),
        "a stage that gained a session is a change and must be published: stdout={stdout}"
    );
    assert_eq!(
        destination_snapshots(&cfg(&repo, &key, root.path())),
        2,
        "the new session must reach the destination: stdout={stdout}"
    );
}

/// `push_only_if_changed = false` keeps the old behaviour on purpose: one
/// snapshot per invocation, which is what a caller who disabled the check
/// asked for.
#[test]
fn push_only_if_changed_false_still_publishes_each_run() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    let stage = stage_with(root.path(), SESSION_ONE);
    let repo = root.path().join("destination");
    let key = root.path().join("destination-key.json");
    write_config(root.path(), "push_only_if_changed = false\n");

    for expected in 1..=2 {
        let output = run_dest_init(root.path(), &registry, &stage, &repo, &key);
        let (stdout, stderr) = text(&output);
        assert_eq!(
            output.status.code(),
            Some(0),
            "stdout={stdout}\nstderr={stderr}"
        );
        assert_eq!(
            destination_snapshots(&cfg(&repo, &key, root.path())),
            expected,
            "push_only_if_changed=false must keep a snapshot per run: stdout={stdout}"
        );
    }
}
