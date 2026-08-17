//! ADR-013: a new destination is initialised as a *full extra copy*.
//!
//! Three properties are pinned here, because each of them fails in a
//! different, silent way:
//!
//! 1. the union really happens — a session that is gone from the local source
//!    but still in an existing destination reaches the new one;
//! 2. it does not over-fire — with the local source intact nothing is copied
//!    from the existing destination, i.e. this is not "re-upload everything,
//!    every time" wearing a difference set's clothes;
//! 3. an existing destination that cannot be consulted is reported as an
//!    *incomplete* difference set, never folded into "it had nothing extra".
//!
//! Only synthetic temp trees are touched; assertions are counts and digests.

use chat_stasher::destinit::{self, SourceDestination};
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::path::Path;

const MACHINE: &str = "fixture-machine";
const SESSION_KEPT: &str = "019bf00d-0000-7eb2-9bf8-000000000001";
const SESSION_LOST: &str = "019bf00d-0000-7eb2-9bf8-000000000002";

fn cfg_for(repo: &Path, key: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
    }
}

/// Seal one synthetic session into a stage.
fn stage_session(stage: &Path, session: &str, lines: &[&str]) {
    store::write_sealed_shard(
        store::StageWriter::Collect,
        stage,
        MACHINE,
        session,
        &lines.iter().map(|l| (*l).to_string()).collect::<Vec<_>>(),
    )
    .unwrap();
}

/// Push a stage into a repository, creating it and persisting its key.
fn push(cfg: &StoreConfig, stage: &Path) {
    let mk = MasterKey::new();
    store::persist_key_file(cfg, &mk).unwrap();
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(stage, &mk)
        .unwrap();
}

/// Session ids + concatenated digests a destination actually holds.
fn archived(cfg: &StoreConfig) -> BTreeMap<String, String> {
    let mk = store::load_key_file(cfg).unwrap();
    let store = BackupStore::new(cfg.clone(), MACHINE.to_string());
    store
        .read_all_machines(&mk)
        .unwrap()
        .machines
        .into_iter()
        .flat_map(|m| m.sessions)
        .map(|s| (s.session_id, s.sha256))
        .collect()
}

/// Criterion 1 — the union is real. Destination A holds two sessions; the
/// local source then loses one of them; destination B is initialised and must
/// still end up with both: one from the local re-collect, one from A.
#[test]
fn new_destination_gets_the_union_of_local_and_existing_destination() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage_a = dir.path().join("stage-a");
    let repo_a = dir.path().join("repo-a");
    let key_a = dir.path().join("key-a.json");
    let cfg_a = cfg_for(&repo_a, &key_a);

    // Destination A: initialised while both sessions were still local.
    stage_session(&stage_a, SESSION_KEPT, &["{\"k\":1}"]);
    stage_session(&stage_a, SESSION_LOST, &["{\"l\":1}", "{\"l\":2}"]);
    push(&cfg_a, &stage_a);
    let in_a = archived(&cfg_a);
    assert_eq!(in_a.len(), 2);

    // Destination B: the local source no longer has SESSION_LOST, so the
    // local re-collect can only stage SESSION_KEPT.
    let stage_b = dir.path().join("stage-b");
    stage_session(&stage_b, SESSION_KEPT, &["{\"k\":1}"]);

    let diff = destinit::fill_difference(
        &stage_b,
        MACHINE,
        store::DEFAULT_SHARD_BUCKET_CAP,
        &[SourceDestination {
            name: "a".to_string(),
            cfg: cfg_a.clone(),
        }],
    );
    assert!(diff.diff_complete, "source A was reachable");
    assert_eq!(diff.restored_sessions, 1, "exactly the lost session");
    assert_eq!(diff.sources[0].missing_locally, 1);
    assert_eq!(diff.sources[0].sessions_other_machines, 0);
    assert!(diff.sources[0].failed_sessions.is_empty());

    // The restored session is byte-identical to what A holds.
    assert_eq!(
        store::expected_concat_sha(&stage_b, MACHINE, SESSION_LOST).unwrap(),
        in_a[SESSION_LOST],
    );

    let repo_b = dir.path().join("repo-b");
    let key_b = dir.path().join("key-b.json");
    let cfg_b = cfg_for(&repo_b, &key_b);
    push(&cfg_b, &stage_b);
    let in_b = archived(&cfg_b);
    assert_eq!(in_b.len(), 2, "B holds the union, not just the local half");
    assert_eq!(in_b, in_a, "same sessions, same digests");
}

/// Criterion 2 — no over-copying. With the local source intact, initialising a
/// second destination must not pull a single session across.
#[test]
fn an_intact_local_source_copies_nothing_from_the_existing_destination() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage_a = dir.path().join("stage-a");
    let cfg_a = cfg_for(&dir.path().join("repo-a"), &dir.path().join("key-a.json"));
    stage_session(&stage_a, SESSION_KEPT, &["{\"k\":1}"]);
    stage_session(&stage_a, SESSION_LOST, &["{\"l\":1}", "{\"l\":2}"]);
    push(&cfg_a, &stage_a);

    // The local re-collect for B produced *both* sessions this time.
    let stage_b = dir.path().join("stage-b");
    stage_session(&stage_b, SESSION_KEPT, &["{\"k\":1}"]);
    stage_session(&stage_b, SESSION_LOST, &["{\"l\":1}", "{\"l\":2}"]);
    let before = store::sealed_shard_count(&stage_b).unwrap();

    let diff = destinit::fill_difference(
        &stage_b,
        MACHINE,
        store::DEFAULT_SHARD_BUCKET_CAP,
        &[SourceDestination {
            name: "a".to_string(),
            cfg: cfg_a,
        }],
    );
    assert!(diff.diff_complete);
    assert_eq!(diff.sources[0].sessions_for_this_machine, 2);
    assert_eq!(
        diff.sources[0].missing_locally, 0,
        "nothing is missing locally, so the difference set is empty"
    );
    assert_eq!(diff.restored_sessions, 0);
    assert_eq!(
        store::sealed_shard_count(&stage_b).unwrap(),
        before,
        "the stage must be untouched — a difference set that copies every time \
         is just an unconditional re-upload"
    );
}

/// Criterion 3 — an existing destination that cannot be consulted must make
/// the difference set *incomplete*, not empty.
#[test]
fn an_unconsultable_destination_is_reported_not_treated_as_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage_a = dir.path().join("stage-a");
    let repo_a = dir.path().join("repo-a");
    let key_a = dir.path().join("key-a.json");
    let cfg_a = cfg_for(&repo_a, &key_a);
    stage_session(&stage_a, SESSION_KEPT, &["{\"k\":1}"]);
    stage_session(&stage_a, SESSION_LOST, &["{\"l\":1}"]);
    push(&cfg_a, &stage_a);

    // Same repository, key gone: the archive exists and demonstrably holds
    // something, and we still cannot see what.
    std::fs::remove_file(&key_a).unwrap();

    let stage_b = dir.path().join("stage-b");
    stage_session(&stage_b, SESSION_KEPT, &["{\"k\":1}"]);
    let diff = destinit::fill_difference(
        &stage_b,
        MACHINE,
        store::DEFAULT_SHARD_BUCKET_CAP,
        &[SourceDestination {
            name: "a".to_string(),
            cfg: cfg_a,
        }],
    );
    assert!(
        !diff.diff_complete,
        "unknown must never be reported as an empty difference set"
    );
    assert!(!diff.sources[0].reachable);
    assert!(diff.sources[0].unreachable_reason.is_some());
    assert_eq!(diff.restored_sessions, 0);
    assert_eq!(
        diff.sources[0].missing_locally, 0,
        "an unreachable source contributes no knowledge at all — not even zero"
    );
}

/// A repository path that does not exist at all is the same class of failure
/// as an unreadable one: unknown, not empty.
#[test]
fn a_missing_repository_is_also_incomplete_not_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage_b = dir.path().join("stage-b");
    stage_session(&stage_b, SESSION_KEPT, &["{\"k\":1}"]);
    let diff = destinit::fill_difference(
        &stage_b,
        MACHINE,
        store::DEFAULT_SHARD_BUCKET_CAP,
        &[SourceDestination {
            name: "gone".to_string(),
            cfg: cfg_for(
                &dir.path().join("no-such-repo"),
                &dir.path().join("no-such-key.json"),
            ),
        }],
    );
    assert!(!diff.diff_complete);
    assert!(!diff.sources[0].reachable);
}
