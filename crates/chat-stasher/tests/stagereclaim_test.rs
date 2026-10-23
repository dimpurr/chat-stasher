//! Black-box stagereclaim tests (ADR-020 Phase 4, as amended by ADR-060):
//! build a synthetic repository via a real push, then exercise the proof
//! and the reclaim.
//!
//! ADR-060 (SG-2) moves the goalposts mid-file: the proof consults the
//! **healthy** set with a ≥1 guard, degraded destinations are listed but
//! not consulted, and the all-gone case is its own blocked shape. Every
//! `NamedStore` here therefore states its health explicitly — the
//! classification itself has its own fixtures in `destdiag.rs` and
//! `w1023_sg2_degraded_reclaim_test.rs`, so these tests pin what the proof
//! does with the classification it was handed.
//!
//! Everything is local + synthetic, in temp dirs. The acceptance mapping:
//!   * a) `apply_reclaims_body_but_keeps_summary`
//!   * b) `verify_l3_still_runs_after_reclaim_on_retained_summary`
//!   * c) `reclaim_then_new_shards_do_not_collide_and_read_back_identical`
//!   * d) ADR-060 D8's §4.7 fixture:
//!         `never_pushed_destination_is_listed_degraded_and_not_consulted`
//!     plus the must-prove refusal arms:
//!         `a_must_prove_destination_olderthan_a_candidate_blocks`
//!   * e) `unreachable_destination_blocks_and_exits_nonzero` (as the
//!     empty-proof-set refusal a record-less single destination now is)

use chat_stasher::destdiag::DestinationHealth;
use chat_stasher::manifest;
use chat_stasher::stagereclaim::{self, BlockedKind, NamedStore};
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[path = "../src/test_support.rs"]
mod test_support;

fn store_config(dir: &Path, repo_name: &str) -> StoreConfig {
    StoreConfig {
        repo_root: dir.join(repo_name).to_string_lossy().into_owned(),
        key_file: dir.join(format!("{repo_name}.key")),
        connections: 1,
        options: BTreeMap::new(),
        // Tests must not share the user's rustic cache: a cached pack would let
        // a corrupted or reclaimed body still read back clean.
        cache_dir: None,
        no_cache: true,
    }
}

fn named(dir: &Path, name: &str, health: DestinationHealth) -> NamedStore {
    NamedStore {
        name: name.to_string(),
        cfg: store_config(dir, name),
        health,
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
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
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

/// (a) push -> reclaim-stage --apply -> body gone, summary still there.
#[test]
fn apply_reclaims_body_but_keeps_summary() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-reclaim";
    let session = "s-aaa";
    write_session(&stage, machine, session, 2);
    let cfg = store_config(dir.path(), "repo-a");
    init_and_push(&cfg, machine, &stage);
    let dests = vec![named(dir.path(), "repo-a", DestinationHealth::Healthy)];

    // Dry run first: proves but deletes nothing.
    let report = stagereclaim::reclaim_stage(&stage, &dests, false).unwrap();
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
    let report = stagereclaim::reclaim_stage(&stage, &dests, true).unwrap();
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
        "shard-seq counter must survive the reclaim"
    );
    drop(dir);
}

// ------------------------------------------------------------- acceptance b

/// (b) after the reclaim, L3 verify still runs against the retained summary.
#[test]
fn verify_l3_still_runs_after_reclaim_on_retained_summary() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-verify";
    let session = "s-bbb";
    write_session(&stage, machine, session, 1);
    let cfg = store_config(dir.path(), "repo-b");
    let mk = init_and_push(&cfg, machine, &stage);
    let dests = vec![named(dir.path(), "repo-b", DestinationHealth::Healthy)];
    let report = stagereclaim::reclaim_stage(&stage, &dests, true).unwrap();
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

/// (c) reclaim, then write new shards: the sequence must not reset onto archived
/// shard names, and read-back must be byte-identical to the new source.
#[test]
fn reclaim_then_new_shards_do_not_collide_and_read_back_identical() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-seq";
    let session = "s-ccc";
    write_session(&stage, machine, session, 2); // archived shards 1,2
    let cfg = store_config(dir.path(), "repo-c");
    let mk = init_and_push(&cfg, machine, &stage);
    let dests = vec![named(dir.path(), "repo-c", DestinationHealth::Healthy)];
    let report = stagereclaim::reclaim_stage(&stage, &dests, true).unwrap();
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

/// (d, ADR-060 D8's §4.7 fixture) one never-pushed destination + one
/// healthy destination that holds every candidate: the reclaim proceeds
/// (dry-run and apply), the never-pushed one is degraded, **not
/// consulted** for proof, and listed with its health and role in the
/// report so no surface can read it as an ordinary quiet row.
#[test]
fn never_pushed_destination_is_listed_degraded_and_not_consulted() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-multi";
    let session = "s-ddd";
    write_session(&stage, machine, session, 1);

    // "repo-primary" holds the session and was pushed just now (healthy);
    // "repo-secondary" exists and is reachable but has never been pushed
    // to — no record, so the classification a real run would derive from
    // the same facts is `never-pushed`: degraded, not consulted.
    let primary = store_config(dir.path(), "repo-primary");
    init_and_push(&primary, machine, &stage);
    let secondary = store_config(dir.path(), "repo-secondary");
    init_empty_repo(&secondary);
    assert!(
        Path::new(&secondary.repo_root).is_dir(),
        "the degraded destination exists and is reachable — its absence from the proof is \
         the health decision, never a reachability accident"
    );

    let dests = vec![
        named(dir.path(), "repo-primary", DestinationHealth::Healthy),
        named(dir.path(), "repo-secondary", DestinationHealth::NeverPushed),
    ];

    // Dry run first: the degraded destination does not block it.
    let report = stagereclaim::reclaim_stage(&stage, &dests, false).unwrap();
    assert!(
        !report.blocked(),
        "the never-pushed destination is degraded — it must not hold the reclaim hostage: {:?}",
        report.blocked
    );
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.reclaimed.len(), 0, "dry run deletes nothing");
    // Every declared destination keeps a report row (D7): the degraded one
    // with its health and its `degraded-not-consulted` role, never a
    // swallowed row, never folded into `blocked`.
    let secondary_row = report
        .destination_states
        .iter()
        .find(|state| state.name == "repo-secondary")
        .expect("the degraded destination is listed on the run");
    assert_eq!(secondary_row.health, DestinationHealth::NeverPushed);
    assert_eq!(secondary_row.role, "degraded-not-consulted");
    let primary_row = report
        .destination_states
        .iter()
        .find(|state| state.name == "repo-primary")
        .expect("the healthy destination is listed too");
    assert_eq!(primary_row.role, "must-prove");

    // Apply: same proof, the body goes, the summary stays.
    let report = stagereclaim::reclaim_stage(&stage, &dests, true).unwrap();
    assert!(
        !report.blocked(),
        "apply must not be blocked by the degraded destination: {:?}",
        report.blocked
    );
    assert_eq!(report.reclaimed.len(), 1);
    assert_eq!(
        shard_count(&stage, machine, session),
        0,
        "the proven body is reclaimed"
    );
    let summary = stored_summary(&stage, machine, session).expect("summary must be retained");
    assert_eq!(summary.shard_count, 1);
    drop(dir);
}

/// (d's refusal arm, ADR-060 D1/D3/D8's variant) a **must-prove**
/// destination that answers but does not hold a candidate still blocks the
/// whole run: the blocked ⇒ delete-nothing / exit-1 contract, narrowed to
/// the healthy set, intact for it. The A1 fixture in
/// `w1023_sg2_degraded_reclaim_test.rs` proves the same destination is
/// *consulted* on its own declared rhythm; this pin is what "consulted"
/// costs when the session is missing.
#[test]
fn a_must_prove_destination_missing_a_candidate_blocks() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-multi";
    let session = "s-missing";
    write_session(&stage, machine, session, 1);

    // "repo-a" holds the session (healthy); "repo-b" was pushed just now
    // (healthy) but to an *empty* stage — it answers fine and simply does
    // not hold this session.
    let a = store_config(dir.path(), "repo-a");
    init_and_push(&a, machine, &stage);
    let b = store_config(dir.path(), "repo-b");
    // A healthy destination with a real, fresh push under its belt that
    // this session never reached: b holds a *different* session, so the
    // consult completes and the absence is a measured answer, exactly the
    // shape a real out-of-sync destination produces.
    let other_stage = dir.path().join("stage-other");
    write_session(&other_stage, machine, "s-other", 1);
    init_and_push(&b, machine, &other_stage);

    let dests = vec![
        named(dir.path(), "repo-a", DestinationHealth::Healthy),
        named(dir.path(), "repo-b", DestinationHealth::Healthy),
    ];
    let report = stagereclaim::reclaim_stage(&stage, &dests, true).unwrap();
    assert!(
        report.blocked(),
        "a healthy destination missing the session must block the reclaim"
    );
    assert!(
        report.reclaimed.is_empty(),
        "nothing may be deleted while blocked"
    );
    let names: Vec<&str> = report
        .blocked
        .iter()
        .map(|block| block.destination.as_str())
        .collect();
    assert_eq!(names, vec!["repo-b"], "the must-prove destination is named");
    assert!(report
        .blocked
        .iter()
        .any(|block| matches!(block.kind, BlockedKind::SessionNotHeld)));
    assert_eq!(
        shard_count(&stage, machine, session),
        1,
        "the body must be untouched while blocked"
    );
    drop(dir);
}

/// (ADR-060 D7, the ≥1 guard) every declared destination degraded ⇒ the
/// empty-proof-set refusal: nothing is consulted, nothing is deleted, and
/// the report's own shape refuses — never 0-with-success, never silence.
#[test]
fn an_empty_healthy_set_is_its_own_blocked_shape() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    write_session(&stage, "m-solo", "s-solo", 2);

    let dests = vec![
        named(dir.path(), "repo-lagging", DestinationHealth::Lagging),
        named(dir.path(), "repo-never", DestinationHealth::NeverPushed),
    ];
    let report = stagereclaim::reclaim_stage(&stage, &dests, true).unwrap();
    assert!(report.blocked(), "the ≥1 guard holds the bottom");
    assert!(
        report.blocked.is_empty(),
        "no destination was consulted, so no per-destination block exists — the refusal is the \
         set-level shape"
    );
    assert!(
        report.no_healthy_prover,
        "the refusal is named for what it is"
    );
    assert!(
        report.reclaimed.is_empty(),
        "nothing may be deleted with no prover"
    );
    assert_eq!(
        shard_count(&stage, "m-solo", "s-solo"),
        2,
        "the body must be untouched: no proof exists at all"
    );
    drop(dir);
}

// ------------------------------------------------------------- acceptance e

/// (e, reshaped by ADR-060) a single-destination machine with **no push
/// record** — the CLI's `--repo` mode against a repository that was never
/// pushed — is the empty-proof-set refusal: the ≥1 guard deletes nothing,
/// the exit is the refusal code, and the prose names what it is. Never
/// 0-with-success, never "the destination is unreachable" (a degraded
/// destination is not consulted at all, so it cannot be the failure).
#[test]
fn a_recordless_single_destination_is_the_empty_proof_set_refusal() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let stage = sandbox.path().join("stage");
    write_session(&stage, "m-cli", "s-cli", 1);
    // Single-destination mode via --repo/--key-file pointing at nothing.
    let repo = sandbox.path().join("repo-not-created");
    let key = sandbox.path().join("key-not-created.json");

    let output = isolated_command(sandbox.path())
        .args(["reclaim-stage", "--stage"])
        .arg(&stage)
        .args(["--repo"])
        .arg(&repo)
        .args(["--key-file"])
        .arg(&key)
        .args(["--keep-ssh-masters"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "the empty-proof-set refusal is the refusal exit: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no healthy destination to prove against"),
        "the refusal names itself (D7's prose): {stdout}"
    );
    assert!(
        stdout.contains("not consulted this run"),
        "the degradation is listed, not swallowed: {stdout}"
    );
    assert!(
        stdout.contains("never-pushed") || stdout.contains("no push on record"),
        "the absence is worded as absence in prose: {stdout}"
    );
    assert!(
        stdout.contains("nothing was deleted"),
        "output must state that nothing was deleted: {stdout}"
    );
    assert_eq!(
        shard_count(&stage, "m-cli", "s-cli"),
        1,
        "the body must be untouched: no proof exists at all"
    );
    drop(sandbox);
}

/// (ADR-060 D8: "healthy-but-unreachable blocks as today") — a
/// single-destination machine whose push record is fresh (so the one
/// destination is healthy and must prove) but whose repository cannot be
/// read: the consult fails, the whole run deletes nothing and the failure
/// is named unreachable. Degradation is time-based, never
/// reachability-based — a young record cannot be outrun by a broken repo.
#[test]
fn a_healthy_unreachable_destination_blocks_as_today() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let stage = sandbox.path().join("stage");
    write_session(&stage, "m-cli", "s-cli", 1);
    let repo = sandbox.path().join("repo-not-created");
    let key = sandbox.path().join("key-not-created.json");
    // A fresh successful push on record: the same row `cmd_push` would
    // have written for the default destination, aging the classification
    // to healthy.
    let state_dir = sandbox.path().join("data/chat-stasher/state");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    chat_stasher::pushstate::record_success(&state_dir, "default", "m-cli", 3, now - 60).unwrap();

    let output = isolated_command(sandbox.path())
        .args(["reclaim-stage", "--stage"])
        .arg(&stage)
        .args(["--repo"])
        .arg(&repo)
        .args(["--key-file"])
        .arg(&key)
        .args(["--keep-ssh-masters"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "a healthy-but-unreachable destination blocks: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("degraded (lagging)") && !stdout.contains("not consulted this run"),
        "the destination is healthy, it is consulted: {stdout}"
    );
    assert!(
        stdout.contains("unreachable"),
        "the failure kind is the consult's: {stdout}"
    );
    assert!(
        stdout.contains("held back") && stdout.contains("nothing was deleted"),
        "the blocked shape keeps its wording: {stdout}"
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
    // The push record the machine's own `cmd_push` would have left: without
    // it the single destination classifies as never-pushed and the dry run
    // is the empty-proof-set refusal instead of the reclaimable report.
    let state_dir = sandbox.path().join("data/chat-stasher/state");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    chat_stasher::pushstate::record_success(&state_dir, "default", machine, 3, now - 60).unwrap();

    let output = isolated_command(sandbox.path())
        .args(["reclaim-stage", "--stage"])
        .arg(&stage)
        .args(["--repo"])
        .arg(&cfg.repo_root)
        .args(["--key-file"])
        .arg(&cfg.key_file)
        .args(["--keep-ssh-masters"])
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

/// `reclaim_stage` must not delete anything when the stage holds no shards.
#[test]
fn empty_stage_is_a_noop() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let dests = vec![named(dir.path(), "repo-empty", DestinationHealth::Healthy)];
    let report = stagereclaim::reclaim_stage(&stage, &dests, true).unwrap();
    assert!(!report.blocked());
    assert!(report.candidates.is_empty());
    assert!(report.reclaimed.is_empty());
    drop(dir);
}
