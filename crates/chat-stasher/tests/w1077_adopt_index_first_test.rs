//! W1077: the index naming adopted packs is published before the snapshot that
//! depends on them.
//!
//! A kill test proved that a snapshot can reference a tree that lives only in
//! a stranded pack: the push adopted the pack in memory, wrote the snapshot,
//! and only then named the pack in a new index file (W1066). A SIGKILL between
//! the two left a snapshot whose trees no index reaches, and a plain index read
//! failed with `Tree ID ... not found in index`.
//!
//! What this file pins, through the real `push` command on a local backend:
//!
//! * **kill at the persist window** — a push killed (SIGKILL) while parked at
//!   the persist step leaves a repository whose every snapshot resolves under
//!   an independent plain-index read; before the fix the parked push had
//!   already published the snapshot, and the read failed;
//! * **a failed naming write** — when the index file cannot be written the push
//!   fails and publishes no snapshot, instead of reporting success with a
//!   snapshot that depends on an unpersisted adoption;
//! * **the harmless leftover** — an index file without the snapshot that was to
//!   follow it (the state a kill between the two publishes leaves) breaks
//!   nothing, and the next push completes over it.
//!
//! The independent read is rustic's own `check` over a plain index, not
//! chat-stasher's adopting open, so it cannot be satisfied by the very adoption
//! under test. Every fixture is synthetic and local.

use chat_stasher::store::{BackupStore, StoreConfig};
use rustic_core::repofile::{IndexFile, MasterKey};
use rustic_core::{CheckOptions, Credentials, Repository, RepositoryOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

#[path = "../src/test_support.rs"]
mod test_support;

const MACHINE: &str = "mbp-adopt-first";

fn filler(bytes: usize, seed: u64) -> String {
    let mut state = seed | 1;
    let hex = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes);
    while out.len() < bytes {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push(hex[(state & 0xf) as usize] as char);
    }
    out
}

/// One repository, one stage, and the plumbing to run the real `push` binary.
struct Sandbox {
    dir: tempfile::TempDir,
    registry: PathBuf,
    repo: PathBuf,
    key: PathBuf,
    stage: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for sub in ["home", "config", "data", "state"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        let registry = root.join("registry.json");
        fs::write(
            &registry,
            r#"{"schema_version":1,"generated":"W1077 adopt index first","harnesses":[]}"#,
        )
        .unwrap();
        let stage = root.join("stage");
        for i in 0..2usize {
            let session = format!("claude-code.{MACHINE}.019bf00d-97b6-7eb2-9bf8-7{i:011}");
            let body = format!(
                r#"{{"parentUuid":null,"sessionId":"{session}","type":"user","message":{{"role":"user","content":"{}"}},"uuid":"u1","timestamp":"2025-06-01T10:00:00Z"}}"#,
                filler(200_000, 0x9E37_79B9_7F4A_7C15 ^ (i as u64) * 0x517C_C1B7)
            ) + "\n";
            let shard = stage
                .join("sessions")
                .join(MACHINE)
                .join(&session)
                .join("000");
            fs::create_dir_all(&shard).unwrap();
            fs::write(shard.join("000001.jsonl"), &body).unwrap();
        }
        Self {
            registry,
            repo: root.join("repo"),
            key: root.join("key.json"),
            stage,
            dir,
        }
    }

    fn command(&self) -> Command {
        let root = self.dir.path();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        cmd.env("HOME", root.join("home"))
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&root.join("home")),
            )
            .env("USERPROFILE", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("rh-cache"))
            .env("CHAT_STASHER_REGISTRY", &self.registry);
        cmd
    }

    fn push_args(&self) -> Vec<String> {
        vec![
            "push".into(),
            "--stage".into(),
            self.stage.to_string_lossy().into_owned(),
            "--machine".into(),
            MACHINE.into(),
            "--repo".into(),
            self.repo.to_string_lossy().into_owned(),
            "--key-file".into(),
            self.key.to_string_lossy().into_owned(),
            "--keep-ssh-masters".into(),
        ]
    }

    fn push(&self) -> Output {
        self.command().args(self.push_args()).output().unwrap()
    }

    /// A `push` that parks at the persist window: the pre-write survey has run,
    /// the naming index file has not been written.
    fn spawn_parked(&self, rendezvous: &Path) -> Child {
        self.command()
            .env("CHAT_STASHER_TEST_HOLD_PERSIST_BEFORE_WRITE", rendezvous)
            .args(self.push_args())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn master_key(&self) -> MasterKey {
        serde_json::from_str(&fs::read_to_string(&self.key).unwrap()).unwrap()
    }

    fn store_config(&self) -> StoreConfig {
        StoreConfig {
            repo_root: self.repo.to_string_lossy().into_owned(),
            key_file: self.key.clone(),
            connections: 1,
            options: BTreeMap::new(),
            cache_dir: None,
            no_cache: true,
        }
    }

    /// Seed the repository with a healthy push, then leave only its packs:
    /// the state a push killed inside its pack window leaves behind.
    fn seed_and_strand(&self) {
        let seeded = self.push();
        assert!(
            seeded.status.success(),
            "the seeding push failed: {}",
            String::from_utf8_lossy(&seeded.stderr)
        );
        assert!(
            !final_packs(&self.repo).is_empty(),
            "the seeding push wrote no packs to strand"
        );
        for entry in ["index", "snapshots"] {
            let dir = self.repo.join(entry);
            if dir.exists() {
                fs::remove_dir_all(&dir).unwrap();
            }
            fs::create_dir_all(&dir).unwrap();
        }
    }
}

fn final_packs(repo: &Path) -> Vec<String> {
    let mut packs = Vec::new();
    let mut stack = vec![repo.join("data")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                stack.push(path);
            } else if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                packs.push(name);
            }
        }
    }
    packs.sort();
    packs
}

/// How many files the repository's `dir` ("snapshots" or "index") holds.
fn files_in(repo: &Path, dir: &str) -> usize {
    let mut count = 0usize;
    let mut stack = vec![repo.join(dir)];
    while let Some(path) = stack.pop() {
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            if entry.path().is_dir() {
                stack.push(entry.path());
            } else {
                count += 1;
            }
        }
    }
    count
}

/// The independent plain-index read: rustic's own `check` over the repository
/// as the index files alone describe it. No chat-stasher adoption is involved,
/// so a snapshot whose trees live only in packs no index names fails here with
/// `Tree ID ... not found in index`. Returns the number of snapshots read.
fn plain_index_read(repo_path: &Path, mk: &MasterKey) -> Result<usize, String> {
    let backends = rustic_backend::BackendOptions::default()
        .repository(repo_path.to_str().unwrap())
        .to_backends()
        .unwrap();
    let repo = Repository::new(&RepositoryOptions::default().no_cache(true), &backends)
        .unwrap()
        .open(&Credentials::Masterkey(mk.clone()))
        .unwrap();
    let snapshots = repo.get_all_snapshots().map_err(|e| e.to_string())?.len();
    match repo.check(CheckOptions::default()) {
        Ok(results) => results.is_ok().map_err(|e| e.to_string())?,
        Err(e) => return Err(e.to_string()),
    }
    Ok(snapshots)
}

/// Ids named by no index file, from a fresh survey.
fn unindexed_packs(cfg: &StoreConfig, mk: &MasterKey) -> BTreeSet<String> {
    let backends = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .backends()
        .unwrap();
    let repo = chat_stasher::orphans::open_for_survey(cfg, &backends, mk).unwrap();
    let inventory = chat_stasher::orphans::survey_packs(&repo, &backends).unwrap();
    // Touch the index listing so a broken index directory fails here.
    for file in repo.stream_files::<IndexFile>().unwrap() {
        let _ = file.unwrap();
    }
    inventory
        .report
        .unindexed
        .iter()
        .map(|(id, _)| id.clone())
        .collect()
}

fn wait_parked(rendezvous: &Path, child: &mut Child) {
    let reached = rendezvous.join("reached");
    let deadline = Instant::now() + Duration::from_secs(120);
    while !reached.exists() {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("the push exited ({status}) without reaching its persist window");
        }
        assert!(
            Instant::now() < deadline,
            "the push never reached its persist window"
        );
        sleep(Duration::from_millis(2));
    }
}

/// SIGKILL a push parked at the persist step, then read the repository it left
/// with nothing but its index files. Red before W1077: the parked push had
/// already published a snapshot whose trees only the stranded packs hold, and
/// no index file named them.
#[test]
fn a_kill_at_the_persist_window_leaves_no_snapshot_the_index_cannot_resolve() {
    let sb = Sandbox::new();
    sb.seed_and_strand();
    let mk = sb.master_key();

    let rendezvous = sb.dir.path().join("persist-park");
    let mut child = sb.spawn_parked(&rendezvous);
    wait_parked(&rendezvous, &mut child);
    // `Child::kill` is SIGKILL on unix and TerminateProcess on Windows: the
    // process gets no chance to clean up or finish what it was about to write.
    child.kill().unwrap();
    child.wait().unwrap();

    let snapshots_published = files_in(&sb.repo, "snapshots");
    let read = plain_index_read(&sb.repo, &mk);
    assert!(
        read.is_ok(),
        "after a kill at the persist window, a plain index read fails ({} snapshot file(s) \
         published, {} index file(s)): {}",
        snapshots_published,
        files_in(&sb.repo, "index"),
        read.err().unwrap_or_default()
    );
    assert_eq!(
        snapshots_published, 0,
        "a snapshot was published before the index file naming the packs it reuses"
    );

    // The kill cost nothing: the next push adopts again, names, publishes.
    let retry = sb.push();
    assert!(
        retry.status.success(),
        "the push after the kill failed: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(plain_index_read(&sb.repo, &mk), Ok(1));
    assert!(
        unindexed_packs(&sb.store_config(), &mk).is_empty(),
        "the push after the kill left packs unindexed"
    );
}

/// The naming write fails (the index location is not writable as a directory)
/// after the pre-write survey passed. The push must fail and publish no
/// snapshot: before W1077 it reported success with a snapshot that depends on
/// an adoption no index file records.
#[test]
fn a_failed_naming_write_publishes_no_dependent_snapshot() {
    let sb = Sandbox::new();
    sb.seed_and_strand();
    let mk = sb.master_key();

    let rendezvous = sb.dir.path().join("persist-park-write");
    let mut child = sb.spawn_parked(&rendezvous);
    wait_parked(&rendezvous, &mut child);
    // Sabotage the write the parked push is about to make: `index` becomes a
    // regular file, so no index file can be created beneath it.
    fs::remove_dir_all(sb.repo.join("index")).unwrap();
    fs::write(sb.repo.join("index"), b"not a directory").unwrap();
    fs::write(rendezvous.join("go"), b"").unwrap();
    let out = child.wait_with_output().unwrap();
    let published = files_in(&sb.repo, "snapshots");

    // Put the repository back before asserting, so a failure leaves it sane.
    fs::remove_file(sb.repo.join("index")).unwrap();
    fs::create_dir_all(sb.repo.join("index")).unwrap();

    assert_eq!(
        published, 0,
        "a snapshot was published although the index file naming the packs it reuses could not be written"
    );
    assert!(
        !out.status.success(),
        "the push reported success although its adoption could not be persisted"
    );

    let retry = sb.push();
    assert!(
        retry.status.success(),
        "the retry after the failed write failed: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(plain_index_read(&sb.repo, &mk), Ok(1));
}

/// The state a kill between the two publishes leaves is an index file naming
/// verified packs and no snapshot after it. It is harmless: a plain read
/// resolves, and the next push completes over it without re-uploading.
#[test]
fn an_index_file_without_its_snapshot_is_a_harmless_leftover() {
    let sb = Sandbox::new();
    sb.seed_and_strand();
    let mk = sb.master_key();

    let adopting = sb.push();
    assert!(
        adopting.status.success(),
        "the adopting push failed: {}",
        String::from_utf8_lossy(&adopting.stderr)
    );
    assert!(
        files_in(&sb.repo, "index") >= 1,
        "the adopting push wrote no index file naming the stranded packs"
    );
    // Remove the snapshot the push published: index published, snapshot not.
    fs::remove_dir_all(sb.repo.join("snapshots")).unwrap();
    fs::create_dir_all(sb.repo.join("snapshots")).unwrap();

    assert_eq!(
        plain_index_read(&sb.repo, &mk),
        Ok(0),
        "an index file with no snapshot after it broke a plain read"
    );
    assert!(unindexed_packs(&sb.store_config(), &mk).is_empty());

    let retry = sb.push();
    assert!(
        retry.status.success(),
        "the push over the leftover index failed: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    assert_eq!(plain_index_read(&sb.repo, &mk), Ok(1));
}
