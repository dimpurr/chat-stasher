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
//!    *incomplete* difference set, never folded into "it had nothing extra";
//! 4. (ADR-015) an absent destination resolves to one of three distinct
//!    states, and which one it is turns on our own record of having dealt with
//!    it — never on the filesystem alone, which cannot tell "never built" from
//!    "built and since lost".
//!
//! Only synthetic temp trees are touched; assertions are counts and digests.

use chat_stasher::destinit::{self, SourceDestination};
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
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
            previously_recorded: true,
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
            previously_recorded: true,
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
            previously_recorded: true,
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

/// Criterion 4 — ADR-015. "No repository at that location" is the *same*
/// observation in two opposite situations, and the only thing that can tell
/// them apart is our own record of having dealt with the destination before.
/// This test runs both halves against an identical missing path, so the record
/// is provably the deciding input and not some property of the location.
#[test]
fn a_missing_repository_is_never_built_or_suspected_loss_depending_on_our_record() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage_b = dir.path().join("stage-b");
    stage_session(&stage_b, SESSION_KEPT, &["{\"k\":1}"]);
    let missing = cfg_for(
        &dir.path().join("no-such-repo"),
        &dir.path().join("no-such-key.json"),
    );

    // Never recorded: it was never built. Known to be empty, so the union is
    // still provable and the run must not cry wolf.
    let never = destinit::fill_difference(
        &stage_b,
        MACHINE,
        store::DEFAULT_SHARD_BUCKET_CAP,
        &[SourceDestination {
            name: "never-made".to_string(),
            previously_recorded: false,
            cfg: missing.clone(),
        }],
    );
    assert_eq!(never.sources[0].status, destinit::SourceStatus::KnownEmpty);
    assert!(
        never.diff_complete,
        "declaring a destination you have not created yet must not report the earlier ones as \
         incomplete — that is a warning on the normal path, and it trains the user to ignore \
         warnings"
    );
    assert_eq!(never.known_empty(), vec!["never-made"]);
    assert!(never.suspected_loss().is_empty());

    // Recorded: we collected for it once, and now it is not there. Same
    // filesystem observation, opposite meaning.
    let lost = destinit::fill_difference(
        &stage_b,
        MACHINE,
        store::DEFAULT_SHARD_BUCKET_CAP,
        &[SourceDestination {
            name: "was-here".to_string(),
            previously_recorded: true,
            cfg: missing,
        }],
    );
    assert_eq!(
        lost.sources[0].status,
        destinit::SourceStatus::SuspectedLoss,
        "a destination we have dealt with before, now gone, may have been holding the only copy"
    );
    assert!(!lost.diff_complete);
    assert_eq!(lost.suspected_loss(), vec!["was-here"]);
    assert!(lost.known_empty().is_empty());
}

/// The unreadable middle state: the location is there, we cannot see into it,
/// and we have no record either way. That is *unknown* — not empty, and not
/// loss, because claiming loss here would cry wolf on a permissions blip.
#[test]
fn a_location_we_cannot_read_is_unknown_not_empty_and_not_loss() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage_b = dir.path().join("stage-b");
    stage_session(&stage_b, SESSION_KEPT, &["{\"k\":1}"]);

    let repo = dir.path().join("unreadable-repo");
    std::fs::create_dir_all(&repo).unwrap();
    let locked = std::fs::Permissions::from_mode(0o000);
    std::fs::set_permissions(&repo, locked).unwrap();

    let diff = destinit::fill_difference(
        &stage_b,
        MACHINE,
        store::DEFAULT_SHARD_BUCKET_CAP,
        &[SourceDestination {
            name: "opaque".to_string(),
            previously_recorded: false,
            cfg: cfg_for(&repo, &dir.path().join("no-such-key.json")),
        }],
    );
    // Restore before asserting, so a failure still leaves a removable tempdir.
    std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(diff.sources[0].status, destinit::SourceStatus::Unknown);
    assert!(
        !diff.diff_complete,
        "we could not establish whether a repository is there, and unknown is not empty"
    );
    assert_eq!(diff.unknown(), vec!["opaque"]);
    assert!(diff.known_empty().is_empty());
    assert!(diff.suspected_loss().is_empty());
}

/// B46: the *same* unreadable directory, spelled the other way rustic accepts.
///
/// `rustic_backend` resolves a bare path and the explicit `local:<path>` form to
/// the same `SupportedBackend::Local` location. `destinit` only second-guessed
/// the bare form — it read the colon as "this is a backend string, not my
/// business" — so the prefixed spelling of an unreadable directory came out
/// `KnownEmpty`, the one absence that exits 0 and lets the run continue.
///
/// Measured in B46: bare → `Unknown` / `diff_complete=false`;
/// `local:` → `KnownEmpty` / `diff_complete=true`. Same directory, opposite
/// verdict, no other input changed.
#[test]
fn a_local_prefixed_unreadable_destination_is_unknown_not_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage_b = dir.path().join("stage-b");
    stage_session(&stage_b, SESSION_KEPT, &["{\"k\":1}"]);

    let repo = dir.path().join("unreadable-repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o000)).unwrap();

    let mut cfg = cfg_for(&repo, &dir.path().join("no-such-key.json"));
    cfg.repo_root = format!("local:{}", cfg.repo_root);

    let diff = destinit::fill_difference(
        &stage_b,
        MACHINE,
        store::DEFAULT_SHARD_BUCKET_CAP,
        &[SourceDestination {
            name: "prefixed".to_string(),
            previously_recorded: false,
            cfg,
        }],
    );
    std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(
        diff.sources[0].status,
        destinit::SourceStatus::Unknown,
        "`local:<path>` is the same local path as `<path>`; how the user spelled \
         the location must not decide whether an unreadable copy is reported"
    );
    assert!(!diff.diff_complete);
    assert_eq!(diff.unknown(), vec!["prefixed"]);
    assert!(diff.known_empty().is_empty());
}

/// B46: a genuinely absent `local:`-spelled destination must still be
/// `KnownEmpty`.
///
/// The guard above must not be paid for by making every not-yet-created
/// destination report INCOMPLETE — that is the warning-on-the-normal-path
/// failure ADR-015 exists to avoid. So the prefix has to be resolved, not
/// treated as "suspicious".
#[test]
fn a_local_prefixed_destination_that_was_never_built_is_still_known_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage_b = dir.path().join("stage-b");
    stage_session(&stage_b, SESSION_KEPT, &["{\"k\":1}"]);

    let mut cfg = cfg_for(
        &dir.path().join("never-built"),
        &dir.path().join("no-such-key.json"),
    );
    cfg.repo_root = format!("local:{}", cfg.repo_root);

    let diff = destinit::fill_difference(
        &stage_b,
        MACHINE,
        store::DEFAULT_SHARD_BUCKET_CAP,
        &[SourceDestination {
            name: "not-yet".to_string(),
            previously_recorded: false,
            cfg,
        }],
    );
    assert_eq!(diff.sources[0].status, destinit::SourceStatus::KnownEmpty);
    assert!(diff.diff_complete);
}

/// B46: pin the measured backend fact that the tri-state relies on.
///
/// A remote backend that cannot be reached must answer `Err`, never `Ok(empty)`
/// — because `destinit` cannot second-guess a remote location the way it can a
/// local path, so an `Ok(empty)` from an unreachable endpoint would land in
/// `KnownEmpty` and exit 0. Measured for `opendal:sftp` (connection refused and
/// wrong credentials), `opendal:s3` (refused, 403) and `opendal:http` (401);
/// all three answered `Err`. This test is the alarm for a backend version that
/// changes its mind.
///
/// The endpoint is a closed port on loopback: no service, real or otherwise, is
/// contacted.
#[test]
fn an_unreachable_remote_destination_is_unknown_not_empty() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage_b = dir.path().join("stage-b");
    stage_session(&stage_b, SESSION_KEPT, &["{\"k\":1}"]);

    let mut cfg = cfg_for(dir.path(), &dir.path().join("no-such-key.json"));
    cfg.repo_root = "opendal:sftp".to_string();
    cfg.options.insert(
        "endpoint".to_string(),
        // Port 1 on loopback: nothing listens there.
        "ssh://127.0.0.1:1".to_string(),
    );
    cfg.options
        .insert("user".to_string(), "b46-nobody".to_string());
    cfg.options
        .insert("known_hosts_strategy".to_string(), "accept".to_string());
    cfg.options
        .insert("root".to_string(), "/chat-stasher-test/b46".to_string());
    // Do not spend the backend's default retry budget on a port we know is shut.
    cfg.options.insert("retry".to_string(), "off".to_string());

    let diff = destinit::fill_difference(
        &stage_b,
        MACHINE,
        store::DEFAULT_SHARD_BUCKET_CAP,
        &[SourceDestination {
            name: "offline".to_string(),
            previously_recorded: false,
            cfg,
        }],
    );
    assert_eq!(
        diff.sources[0].status,
        destinit::SourceStatus::Unknown,
        "an unreachable remote must never resolve to `never built` — that is the \
         only branch that exits 0"
    );
    assert!(!diff.diff_complete);
}
