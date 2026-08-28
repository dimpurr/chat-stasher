//! Black-box stagereap tests (ADR-020 Phase 4): build a synthetic repository
//! via a real push, then exercise the proof and the reap.
//!
//! Everything is local + synthetic, in temp dirs. The five acceptance
//! criteria map to:
//!   * a) `apply_reaps_body_but_keeps_summary`
//!   * b) `verify_l3_still_runs_after_reap_on_retained_summary`
//!   * c) `reap_then_new_shards_do_not_collide_and_read_back_identical`
//!   * d) `two_destinations_one_pushed_is_blocked_and_names_it`
//!   * e) `unreachable_destination_blocks_and_exits_nonzero`

use chat_stasher::manifest;
use chat_stasher::stagereap::{self, BlockedKind, NamedStore};
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn store_config(dir: &Path, repo_name: &str) -> StoreConfig {
    StoreConfig {
        repo_root: dir.join(repo_name).to_string_lossy().into_owned(),
        key_file: dir.join(format!("{repo_name}.key")),
        connections: 1,
        options: BTreeMap::new(),
        // Tests must not share the user's rustic cache: a cached pack would let
        // a corrupted or reaped body still read back clean.
        cache_dir: None,
        no_cache: true,
    }
}

fn named(dir: &Path, name: &str) -> NamedStore {
    NamedStore {
        name: name.to_string(),
        cfg: store_config(dir, name),
    }
}

/// Persist a key for `cfg` and push `stage` into it. Returns the key.
fn init_and_push(cfg: &StoreConfig, machine: &str, stage: &Path) -> MasterKey {
    let mk = MasterKey::new();
    store::persist_key_file(cfg, &mk).unwrap();
    BackupStore::new(cfg.clone(), machine.to_string())
        .push(stage, &mk)
        .unwrap();
    mk
}

/// Initialise a repository that exists and is reachable but has never been
/// pushed to — the "declared destination that owes" case.
fn init_empty_repo(cfg: &StoreConfig) {
    let mk = MasterKey::new();
    store::persist_key_file(cfg, &mk).unwrap();
    let store = BackupStore::new(cfg.clone(), "unused".to_string());
    store.open_or_init(&mk).unwrap();
}

/// Write `nshards` sealed shards for one session; returns the expected
/// concatenated bytes (each line followed by exactly one newline).
fn write_session(stage: &Path, machine: &str, session: &str, nshards: u64) -> Vec<u8> {
    let mut concat = Vec::new();
    for seq in 1..=nshards {
        let lines: Vec<String> = (0..10)
            .map(|i| format!("{{\"seq\":{seq},\"i\":{i},\"s\":\"payload-{session}-{seq}-{i}\"}}"))
            .collect();
        store::write_sealed_shard(StageWriter::Collect, stage, machine, session, &lines).unwrap();
        for line in &lines {
            concat.extend_from_slice(line.as_bytes());
            concat.push(b'\n');
        }
    }
    concat
}

fn shard_count(stage: &Path, machine: &str, session: &str) -> usize {
    store::sealed_shard_entries(&store::session_shard_dir(stage, machine, session))
        .unwrap()
        .len()
}

fn stored_summary(stage: &Path, machine: &str, session: &str) -> Option<manifest::SessionManifest> {
    match manifest::read_manifest(stage, machine).unwrap() {
        manifest::ManifestFileState::Loaded(rows) => {
            rows.into_iter().find(|r| r.session_id == session)
        }
        _ => None,
    }
}

/// A `chat-stasher` binary run with fully isolated XDG/home env.
fn isolated_command(sandbox: &Path) -> std::process::Command {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
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

// ------------------------------------------------------------- acceptance a

/// (a) push -> reap-stage --apply -> body gone, summary still there.
#[test]
fn apply_reaps_body_but_keeps_summary() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-reap";
    let session = "s-aaa";
    write_session(&stage, machine, session, 2);
    let cfg = store_config(dir.path(), "repo-a");
    init_and_push(&cfg, machine, &stage);
    let dests = vec![named(dir.path(), "repo-a")];

    // Dry run first: proves but deletes nothing.
    let report = stagereap::reap_stage(&stage, &dests, false).unwrap();
    assert!(
        !report.blocked(),
        "dry run must not be blocked: {:?}",
        report.blocked
    );
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.reclaimed.len(), 0);
    assert_eq!(
        shard_count(&stage, machine, session),
        2,
        "dry run must not delete"
    );

    // Apply: body goes, summary stays, counter survives.
    let report = stagereap::reap_stage(&stage, &dests, true).unwrap();
    assert!(
        !report.blocked(),
        "apply must not be blocked: {:?}",
        report.blocked
    );
    assert_eq!(report.reclaimed.len(), 1);
    assert_eq!(
        shard_count(&stage, machine, session),
        0,
        "body must be gone"
    );
    let summary = stored_summary(&stage, machine, session).expect("summary must be retained");
    assert_eq!(summary.session_id, session);
    assert_eq!(summary.shard_count, 2);
    assert!(
        store::shard_seq_file(&stage, machine, session).is_file(),
        "shard-seq counter must survive the reap"
    );
    drop(dir);
}

// ------------------------------------------------------------- acceptance b

/// (b) after the reap, L3 verify still runs against the retained summary.
#[test]
fn verify_l3_still_runs_after_reap_on_retained_summary() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-verify";
    let session = "s-bbb";
    write_session(&stage, machine, session, 1);
    let cfg = store_config(dir.path(), "repo-b");
    let mk = init_and_push(&cfg, machine, &stage);
    let dests = vec![named(dir.path(), "repo-b")];
    let report = stagereap::reap_stage(&stage, &dests, true).unwrap();
    assert!(!report.blocked());
    assert_eq!(report.reclaimed.len(), 1);

    let store = BackupStore::new(cfg, machine.to_string());
    let l3 = store.reconcile_manifest(&mk, &stage).unwrap();
    assert!(
        l3.ok(),
        "L3 must pass on the retained summary: {:?}",
        l3.rows
    );
    assert_eq!(
        l3.stored_basis_rows(),
        1,
        "L3 must use the stored-manifest basis once the body is gone"
    );
    drop(dir);
}

// ------------------------------------------------------------- acceptance c

/// (c) reap, then write new shards: the sequence must not reset onto archived
/// shard names, and read-back must be byte-identical to the new source.
#[test]
fn reap_then_new_shards_do_not_collide_and_read_back_identical() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-seq";
    let session = "s-ccc";
    write_session(&stage, machine, session, 2); // archived shards 1,2
    let cfg = store_config(dir.path(), "repo-c");
    let mk = init_and_push(&cfg, machine, &stage);
    let dests = vec![named(dir.path(), "repo-c")];
    let report = stagereap::reap_stage(&stage, &dests, true).unwrap();
    assert!(!report.blocked());
    assert_eq!(report.reclaimed.len(), 1);

    // New shards must continue the sequence (3,4), never fall back to 1,2.
    let content_b = write_session(&stage, machine, session, 2);
    // `sealed_shard_entries` does not promise an order — `read_session_readback`
    // sorts explicitly before concatenating, so callers that care must sort too.
    // What this assertion is about is the *values* (3,4 not 1,2), not the order.
    let mut seqs: Vec<u64> =
        store::sealed_shard_entries(&store::session_shard_dir(&stage, machine, session))
            .unwrap()
            .into_iter()
            .map(|(seq, _)| seq)
            .collect();
    seqs.sort_unstable();
    assert_eq!(
        seqs,
        vec![3, 4],
        "the sequence must not reset onto archived shard names"
    );

    // Push the new shards (reusing the same key — the repo is encrypted under
    // it) and read back: byte-identical to the new source.
    BackupStore::new(cfg.clone(), machine.to_string())
        .push(&stage, &mk)
        .unwrap();
    let store = BackupStore::new(cfg, machine.to_string());
    let (concat, hashes) = store.read_session_readback(&stage, session, &mk).unwrap();
    assert_eq!(
        concat, content_b,
        "read-back must equal the new source bytes"
    );
    assert_eq!(hashes.len(), 2);
    drop(dir);
}

// ------------------------------------------------------------- acceptance d

/// (d) two declared destinations, only one pushed: the reap is refused and
/// the owing destination is named.
#[test]
fn two_destinations_one_pushed_is_blocked_and_names_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-multi";
    let session = "s-ddd";
    write_session(&stage, machine, session, 1);

    // "repo-primary" holds the session; "repo-secondary" exists and is
    // reachable but has never been pushed to.
    let primary = store_config(dir.path(), "repo-primary");
    init_and_push(&primary, machine, &stage);
    let secondary = store_config(dir.path(), "repo-secondary");
    init_empty_repo(&secondary);

    let dests = vec![
        named(dir.path(), "repo-primary"),
        named(dir.path(), "repo-secondary"),
    ];
    let report = stagereap::reap_stage(&stage, &dests, true).unwrap();
    assert!(
        report.blocked(),
        "a destination not holding the session must block the reap"
    );
    assert!(
        report.reclaimed.is_empty(),
        "nothing may be deleted while blocked"
    );
    let names: Vec<&str> = report
        .blocked
        .iter()
        .map(|b| b.destination.as_str())
        .collect();
    assert!(
        names.contains(&"repo-secondary"),
        "the owing destination must be named: {names:?}"
    );
    assert!(
        !names.contains(&"repo-primary"),
        "the destination that holds the session must not be blamed: {names:?}"
    );
    assert!(report
        .blocked
        .iter()
        .any(|b| matches!(b.kind, BlockedKind::SessionNotHeld)));
    assert_eq!(
        shard_count(&stage, machine, session),
        1,
        "the body must be untouched while blocked"
    );
    drop(dir);
}

// ------------------------------------------------------------- acceptance e

/// (e) an unreachable destination refuses the reap and exits non-zero — never
/// silently treated as "can delete".
#[test]
fn unreachable_destination_blocks_and_exits_nonzero() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let stage = sandbox.path().join("stage");
    write_session(&stage, "m-cli", "s-cli", 1);
    // Single-destination mode via --repo/--key-file pointing at nothing.
    let repo = sandbox.path().join("repo-not-created");
    let key = sandbox.path().join("key-not-created.json");

    let output = isolated_command(sandbox.path())
        .args(["reap-stage", "--stage"])
        .arg(&stage)
        .args(["--repo"])
        .arg(&repo)
        .args(["--key-file"])
        .arg(&key)
        .args(["--no-reap"])
        .output()
        .unwrap();
    assert_ne!(
        output.status.code(),
        Some(0),
        "a blocked reap must exit non-zero"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("blocked"),
        "output must say blocked: {stdout}"
    );
    assert!(
        stdout.contains("unreachable"),
        "output must name the failure kind: {stdout}"
    );
    assert!(
        stdout.contains("held back"),
        "output must say what is held back: {stdout}"
    );
    assert!(
        stdout.contains("nothing was deleted"),
        "output must state that nothing was deleted: {stdout}"
    );
    drop(sandbox);
}

/// The CLI dry run reports reclaimable and exits 0 without deleting anything.
#[test]
fn cli_dry_run_reports_reclaimable_and_exits_zero() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let stage = sandbox.path().join("stage");
    let machine = "m-cli";
    write_session(&stage, machine, "s-cli", 1);
    let cfg = store_config(sandbox.path(), "repo-cli");
    init_and_push(&cfg, machine, &stage);

    let output = isolated_command(sandbox.path())
        .args(["reap-stage", "--stage"])
        .arg(&stage)
        .args(["--repo"])
        .arg(&cfg.repo_root)
        .args(["--key-file"])
        .arg(&cfg.key_file)
        .args(["--no-reap"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "an unblocked dry run must exit 0"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("reclaimable"),
        "dry run must report reclaimable: {stdout}"
    );
    assert!(
        stdout.contains("dry run"),
        "dry run must say it is a dry run: {stdout}"
    );
    assert_eq!(
        shard_count(&stage, machine, "s-cli"),
        1,
        "dry run must not delete the body"
    );
    drop(sandbox);
}

/// `reap_stage` must not delete anything when the stage holds no shards.
#[test]
fn empty_stage_is_a_noop() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let dests = vec![named(dir.path(), "repo-empty")];
    let report = stagereap::reap_stage(&stage, &dests, true).unwrap();
    assert!(!report.blocked());
    assert!(report.candidates.is_empty());
    assert!(report.reclaimed.is_empty());
    drop(dir);
}
