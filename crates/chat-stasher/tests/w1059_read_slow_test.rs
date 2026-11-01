//! W1059 · `read --session` must cost the session, not the repository history.
//!
//! Field report: a 1373-byte session took 760-981 s at m1max because the
//! cumulative walk listed every file of every one of 982 snapshots (a recursive
//! `ls` of each snapshot root, i.e. every session of the machine, 982 times)
//! just to keep the few paths of one session.
//!
//! Two tests:
//!
//! * `identity_digest_...` builds a deterministic multi-snapshot fixture
//!   (reclaimed bodies, continued sessions, a late session, an early-only
//!   session, a miss) and prints one sha256 over everything the cumulative
//!   readers return (`read --session`, the batch reader behind `export`, the
//!   raw reader behind `verify` L3, `read --all-machines`). The digest must be
//!   the same before and after the change; `W1059_OUT` also writes the
//!   per-line material so a diff names the line.
//! * `benchmark_...` (ignored) times one small-session read on a larger
//!   repository. `W1059_REPO=<dir>` reuses a repository built by an earlier
//!   run so before/after read exactly the same bytes.
//!
//! Everything is synthetic: generated ids and payload only.

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const MACHINE: &str = "m-w1059";

fn cfg_in(dir: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: dir.join("repo").to_string_lossy().into_owned(),
        key_file: dir.join("repo.key"),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(dir.join("cache")),
        no_cache: false,
    }
}

fn sid(i: usize) -> String {
    format!("claude-code.{MACHINE}.00000000-0000-0000-0000-{i:012}")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn add_shard(stage: &Path, session: &str, tag: &str) {
    let lines: Vec<String> = (0..4)
        .map(|i| format!("{{\"t\":\"{tag}\",\"i\":{i}}}"))
        .collect();
    store::write_sealed_shard(StageWriter::Collect, stage, MACHINE, session, &lines).unwrap();
}

fn drop_bodies(stage: &Path, session: &str) {
    let dir = stage.join("sessions").join(MACHINE).join(session);
    if dir.is_dir() {
        for entry in walk(&dir) {
            fs::remove_file(entry).unwrap();
        }
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().filter_map(Result::ok) {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn push(store: &BackupStore, stage: &Path, mk: &MasterKey) {
    store.push(stage, mk).unwrap();
}

/// Build `snapshots` pushes over `sessions` sessions. Deterministic.
fn build(dir: &Path, snapshots: usize, sessions: usize) -> MasterKey {
    let cfg = cfg_in(dir);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let st = BackupStore::new(cfg, MACHINE.to_string());
    let stage = dir.join("stage");
    for i in 0..sessions {
        add_shard(&stage, &sid(i), &format!("init-{i}"));
    }
    for run in 0..snapshots {
        // a few sessions move on every run, so every snapshot's tree differs
        for i in 0..sessions {
            if (run + i) % 29 == 0 {
                add_shard(&stage, &sid(i), &format!("run-{run}-{i}"));
            }
        }
        // a session that exists only late
        if run + 2 == snapshots {
            add_shard(&stage, &sid(sessions), "late");
        }
        push(&st, &stage, &mk);
        // reclaim: every 5th run the bodies leave the stage, later snapshots
        // hold directories only (the W253 state)
        if run % 5 == 4 {
            for i in 0..sessions {
                if i % 3 != 0 {
                    drop_bodies(&stage, &sid(i));
                }
            }
        }
        // a session written once at the start and gone from the stage after
        // the first reclaim is covered by the i % 3 rule above.
    }
    mk
}

fn digest_everything(dir: &Path, mk: &MasterKey, sessions: usize) -> (String, Vec<String>) {
    let st = BackupStore::new(cfg_in(dir), MACHINE.to_string());
    let mut lines: Vec<String> = Vec::new();
    // `read --session`, one at a time, including the late session and a miss.
    let mut wanted = BTreeSet::new();
    let mut ids: Vec<String> = (0..=sessions).map(sid).collect();
    ids.push("claude-code.nonexistent-session".to_string());
    for id in &ids {
        match st.read_session_concat(MACHINE, id, mk) {
            Ok((bytes, hashes)) => {
                lines.push(format!(
                    "read {id} OK bytes={} sha={} shards={:?}",
                    bytes.len(),
                    hex(&Sha256::digest(&bytes)),
                    hashes
                ));
                wanted.insert((MACHINE.to_string(), id.clone()));
            }
            Err(e) => lines.push(format!("read {id} ERR {e:#}")),
        }
    }
    // the unknown machine
    match st.read_session_concat("other-machine", &sid(0), mk) {
        Ok(_) => lines.push("read other-machine OK".into()),
        Err(e) => lines.push(format!("read other-machine ERR {e:#}")),
    }
    // `export`'s batch reader
    let batch = st.read_selected_sessions(mk, &wanted).unwrap();
    for ((m, s), (bytes, hashes)) in &batch {
        lines.push(format!(
            "export {m}/{s} bytes={} sha={} shards={:?}",
            bytes.len(),
            hex(&Sha256::digest(bytes)),
            hashes
        ));
    }
    // `verify` L3 (raw) and `read --all-machines`
    for (label, report) in [
        (
            "verify",
            st.read_cumulative_sessions_raw(mk, Some(&wanted)).unwrap(),
        ),
        ("all", st.read_all_machines(mk).unwrap()),
    ] {
        lines.push(format!(
            "{label} complete={} warnings={:?}",
            report.complete(),
            report.warnings
        ));
        for m in &report.machines {
            for s in &m.sessions {
                lines.push(format!(
                    "{label} {}/{} n={} bytes={} sha={} shard_sha={:?} seq={:?}",
                    s.machine,
                    s.session_id,
                    s.shard_count,
                    s.concat_bytes,
                    s.sha256,
                    s.shard_sha256,
                    s.shard_sequences
                ));
            }
        }
    }
    let mut h = Sha256::new();
    for l in &lines {
        h.update(l.as_bytes());
        h.update(b"\n");
    }
    (hex(&h.finalize()), lines)
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn open_or_build(
    snapshots: usize,
    sessions: usize,
) -> (PathBuf, MasterKey, Option<tempfile::TempDir>) {
    if let Ok(keep) = std::env::var("W1059_REPO") {
        let dir = PathBuf::from(keep);
        if dir.join("repo").exists() {
            let mk = store::load_key_file(&cfg_in(&dir)).unwrap();
            return (dir, mk, None);
        }
        fs::create_dir_all(&dir).unwrap();
        let mk = build(&dir, snapshots, sessions);
        return (dir, mk, None);
    }
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    let mk = build(&dir, snapshots, sessions);
    (dir, mk, Some(tmp))
}

#[test]
fn identity_digest_of_every_cumulative_reader() {
    let snapshots = env_usize("W1059_IDENT_SNAPS", 24);
    let sessions = env_usize("W1059_IDENT_SESSIONS", 20);
    let (dir, mk, _guard) = open_or_build(snapshots, sessions);
    let (digest, lines) = digest_everything(&dir, &mk, sessions);
    if let Ok(out) = std::env::var("W1059_OUT") {
        fs::write(&out, lines.join("\n") + "\n").unwrap();
    }
    println!("[W1059] IDENTITY lines={} sha256={digest}", lines.len());
    // Measured on the code before W1059 (full recursive listing of every
    // snapshot) with the default fixture; the fast path must reproduce it.
    if std::env::var("W1059_REPO").is_err() {
        assert_eq!(
            digest, "6c15f5973498e890aab612327e01f55beb1a83bea3686ce30823b57930150012",
            "cumulative readers' output changed"
        );
    }
    assert!(lines
        .iter()
        .any(|l| l.starts_with("read ") && l.contains(" OK ")));
    // a miss is an error naming how many snapshots were read, never empty
    let miss = lines
        .iter()
        .find(|l| l.contains("nonexistent-session"))
        .unwrap();
    assert!(
        miss.contains("ERR") && miss.contains("all of them were read"),
        "{miss}"
    );
}

#[test]
#[ignore = "manual wall-clock measurement"]
fn benchmark_single_session_read_against_repository_history() {
    let snapshots = env_usize("W1059_SNAPS", 300);
    let sessions = env_usize("W1059_SESSIONS", 300);
    let (dir, mk, _guard) = open_or_build(snapshots, sessions);
    let st = BackupStore::new(cfg_in(&dir), MACHINE.to_string());
    // warm the metadata cache once, as an operator's second command would
    st.read_session_concat(MACHINE, &sid(1), &mk).unwrap();
    let mut times = Vec::new();
    for i in [1usize, 2, 4, 5] {
        let t = std::time::Instant::now();
        let (bytes, hashes) = st.read_session_concat(MACHINE, &sid(i), &mk).unwrap();
        times.push(t.elapsed().as_millis());
        println!(
            "[W1059] read session {i}: {} B in {} shard(s), sha={}",
            bytes.len(),
            hashes.len(),
            hex(&Sha256::digest(&bytes))
        );
    }
    let t = std::time::Instant::now();
    let miss = st.read_session_concat(MACHINE, "claude-code.nonexistent-session", &mk);
    let miss_ms = t.elapsed().as_millis();
    println!(
        "[W1059] BENCH snapshots={snapshots} sessions={sessions} read_ms={times:?} miss_ms={miss_ms} miss_is_err={}",
        miss.is_err()
    );
}
