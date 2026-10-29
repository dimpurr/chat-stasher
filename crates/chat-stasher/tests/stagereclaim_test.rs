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
use std::collections::{BTreeMap, BTreeSet};
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
    let report = stagereclaim::reclaim_stage(&stage, &dests, false, None).unwrap();
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
    let report = stagereclaim::reclaim_stage(&stage, &dests, true, None).unwrap();
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
    let report = stagereclaim::reclaim_stage(&stage, &dests, true, None).unwrap();
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
    let report = stagereclaim::reclaim_stage(&stage, &dests, true, None).unwrap();
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
    let report = stagereclaim::reclaim_stage(&stage, &dests, false, None).unwrap();
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
    let report = stagereclaim::reclaim_stage(&stage, &dests, true, None).unwrap();
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
    let report = stagereclaim::reclaim_stage(&stage, &dests, true, None).unwrap();
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
    let report = stagereclaim::reclaim_stage(&stage, &dests, true, None).unwrap();
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
    let report = stagereclaim::reclaim_stage(&stage, &dests, true, None).unwrap();
    assert!(!report.blocked());
    assert!(report.candidates.is_empty());
    assert!(report.reclaimed.is_empty());
    drop(dir);
}

// ------------------------------------------- ADR-060 D6(ii) / 47 §5.3, rev0 ①
// The mirror leg of the proof: a configured-and-on mirror of a consulted
// destination holds a session back — `not-mirrored`, D6.3's one spelling —
// until the copy's completed-round record holds that session's proof
// snapshot closure-complete. `None` (the machine before mirrors, every
// test above) must keep running exactly as it ran.

/// The newest snapshot id the consult would prove `machine`'s session
/// bytes from — the id the leg has to find covered.
fn newest_snapshot_of(cfg: &StoreConfig, mk: &MasterKey, machine: &str) -> String {
    let report = BackupStore::new(cfg.clone(), machine.to_string())
        .read_latest_per_machine(mk)
        .unwrap();
    match report
        .machines
        .iter()
        .find(|merge| merge.hostname == machine)
    {
        Some(merge) => merge.snapshot_id.clone(),
        None => panic!("machine {machine} holds a snapshot"),
    }
}

/// A wired-on holdback: the copy's directory exists and a completed-round
/// record exists, with `closure_complete` naming precisely what the test
/// wants the copy to have reached.
fn mirror_holdback(
    dir: &Path,
    mirror_of: &str,
    closure_complete: BTreeSet<String>,
) -> chat_stasher::stagereclaim::MirrorHoldback {
    let mirror_root = dir.join("mirror-copy");
    fs::create_dir_all(&mirror_root).unwrap();
    chat_stasher::stagereclaim::MirrorHoldback {
        mirror_of: mirror_of.to_string(),
        mirror_root,
        state: chat_stasher::mirrorstate::MirrorStateRead::Present(
            chat_stasher::mirrorstate::MirrorState {
                version: chat_stasher::mirrorstate::MIRROR_STATE_VERSION,
                finished_at_unix: 1_800_000_000,
                mirror_of: mirror_of.to_string(),
                closure_complete,
                snapshots_on_remote: 1,
                remote_total_bytes: 100,
            },
        ),
    }
}

/// A completed-round record that predates the proof's snapshot holds every
/// candidate back: the reclaim is blocked, per session, with D6.3's word,
/// the durable record carries it, and the body is untouched.
#[test]
fn a_mirror_lagging_behind_the_proof_holds_sessions_back() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-mirror";
    let session = "s-mir";
    let written = write_session(&stage, machine, session, 2);
    let cfg = store_config(dir.path(), "repo-m");
    let mk = init_and_push(&cfg, machine, &stage);
    let _snapshot = newest_snapshot_of(&cfg, &mk, machine);
    let dests = vec![named(dir.path(), "repo-m", DestinationHealth::Healthy)];

    let holdback = mirror_holdback(dir.path(), "repo-m", BTreeSet::new());
    let report = stagereclaim::reclaim_stage(&stage, &dests, true, Some(&holdback)).unwrap();
    assert!(
        report.blocked(),
        "the mirror leg refuses the deletion: {:?}",
        report.blocked
    );
    assert!(
        report.reclaimed.is_empty(),
        "nothing may be deleted while held"
    );
    assert_eq!(
        shard_count(&stage, machine, session),
        2,
        "the proven body stays until the copy reaches it"
    );
    assert_eq!(report.blocked.len(), 1);
    let block = &report.blocked[0];
    assert_eq!(block.destination, "repo-m");
    assert_eq!(
        block.sessions, 1,
        "the hold-back is per session, the whole-machine shape it is not"
    );
    assert_eq!(block.bytes, written.len() as u64);
    assert!(
        matches!(block.kind, BlockedKind::NotMirrored),
        "{:?}",
        block.kind
    );
    // The word and the durable record belong to one vocabulary (D6.3):
    // the weekly streak the menubar counts reads the same kind.
    assert_eq!(
        stagereclaim::block_kind_word(&BlockedKind::NotMirrored),
        "not-mirrored"
    );
    assert_eq!(
        chat_stasher::mirror::HELD_BACK_KIND,
        "not-mirrored",
        "the report word is the settled spelling"
    );
    let as_state = stagereclaim::blocks_as_state(&report.blocked);
    assert_eq!(as_state.len(), 1);
    assert_eq!(as_state[0].kind, "not-mirrored");
    assert_eq!(as_state[0].sessions, 1);
}

/// Once the copy's completed-round record holds the proof's snapshot
/// closure-complete, the same run reclaims the body it proved.
#[test]
fn a_covered_mirror_frees_what_it_proved() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-mirror";
    let session = "s-mir";
    write_session(&stage, machine, session, 2);
    let cfg = store_config(dir.path(), "repo-m");
    let mk = init_and_push(&cfg, machine, &stage);
    let snapshot = newest_snapshot_of(&cfg, &mk, machine);
    let dests = vec![named(dir.path(), "repo-m", DestinationHealth::Healthy)];

    let mut closure = BTreeSet::new();
    closure.insert(snapshot);
    let holdback = mirror_holdback(dir.path(), "repo-m", closure);
    let report = stagereclaim::reclaim_stage(&stage, &dests, true, Some(&holdback)).unwrap();
    assert!(
        !report.blocked(),
        "a covered proof snapshot is reclaimable: {:?}",
        report.blocked
    );
    assert_eq!(report.reclaimed.len(), 1);
    assert_eq!(shard_count(&stage, machine, session), 0, "the body goes");
    let summary = stored_summary(&stage, machine, session).expect("summary retained");
    assert_eq!(summary.shard_count, 2);
    drop(dir);
}

/// A record no directory backs vouches for nothing: coverage is a claim
/// about bytes on this machine's disk, so even a record that *names* the
/// proof's snapshot holds everything back when the copy is gone — the
/// safe direction, and the same `is_dir` honesty the read seam keeps.
#[test]
fn a_record_no_directory_backs_holds_everything_back() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-mirror";
    let session = "s-mir";
    write_session(&stage, machine, session, 1);
    let cfg = store_config(dir.path(), "repo-m");
    let mk = init_and_push(&cfg, machine, &stage);
    let snapshot = newest_snapshot_of(&cfg, &mk, machine);
    let dests = vec![named(dir.path(), "repo-m", DestinationHealth::Healthy)];

    let mut closure = BTreeSet::new();
    closure.insert(snapshot);
    let holdback = chat_stasher::stagereclaim::MirrorHoldback {
        mirror_of: "repo-m".to_string(),
        mirror_root: dir.path().join("mirror-copy"), // deliberately absent
        state: chat_stasher::mirrorstate::MirrorStateRead::Present(
            chat_stasher::mirrorstate::MirrorState {
                version: chat_stasher::mirrorstate::MIRROR_STATE_VERSION,
                finished_at_unix: 1_800_000_000,
                mirror_of: "repo-m".to_string(),
                closure_complete: closure,
                snapshots_on_remote: 1,
                remote_total_bytes: 100,
            },
        ),
    };
    let report = stagereclaim::reclaim_stage(&stage, &dests, true, Some(&holdback)).unwrap();
    assert!(
        report.blocked(),
        "a deleted copy is not coverage: {:?}",
        report.blocked
    );
    assert!(matches!(report.blocked[0].kind, BlockedKind::NotMirrored));
    assert_eq!(shard_count(&stage, machine, session), 1, "the body stays");
}

/// The holdback binds only a destination this run actually consulted: a
/// mirror pointed at a degraded destination is not a proof input (D3's
/// own rule — a degraded destination may not hold the whole machine's
/// reclaim hostage), and neither is a name no run consults.
#[test]
fn a_mirror_of_a_destination_this_run_does_not_consult_binds_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-mirror";
    let session = "s-mir";
    write_session(&stage, machine, session, 1);
    let primary = store_config(dir.path(), "repo-primary");
    init_and_push(&primary, machine, &stage);
    let lagging = store_config(dir.path(), "repo-lagging");
    init_empty_repo(&lagging);
    let dests = vec![
        named(dir.path(), "repo-primary", DestinationHealth::Healthy),
        named(dir.path(), "repo-lagging", DestinationHealth::Lagging),
    ];

    // A name no run consults (never declared) binds nothing.
    let holdback = mirror_holdback(dir.path(), "nope-never-declared", BTreeSet::new());
    let report = stagereclaim::reclaim_stage(&stage, &dests, false, Some(&holdback)).unwrap();
    assert!(
        !report.blocked(),
        "an unconsulted mirror_of is not a proof input: {:?}",
        report.blocked
    );

    // The degraded destination's own mirror binds nothing either: its
    // protection is the alarm layer and the union backfill, never stage
    // retention — the reclaim reclaims what the healthy destination
    // proved.
    let holdback = mirror_holdback(dir.path(), "repo-lagging", BTreeSet::new());
    let report = stagereclaim::reclaim_stage(&stage, &dests, true, Some(&holdback)).unwrap();
    assert!(
        !report.blocked(),
        "D3's rule: a degraded destination's mirror may not hold the reclaim hostage: {:?}",
        report.blocked
    );
    assert_eq!(report.reclaimed.len(), 1, "the proven body goes");
    assert_eq!(shard_count(&stage, machine, session), 0);
}

/// The CLI wires the leg the docs promise: on a machine with
/// `[mirror] enabled = true`, the weekly command refuses as
/// `not-mirrored` (here: a copy whose first fill has not completed, so no
/// completed-round record exists), deletes nothing, and names the round
/// that frees the body. The regression direction is the wiring itself:
/// a CLI that left the holdback `None` would print a dry "reclaimable".
#[test]
fn the_cli_wires_the_mirror_leg_and_refuses_as_not_mirrored() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let stage = sandbox.path().join("stage");
    let machine = "m-cli";
    let session = "s-cli";
    write_session(&stage, machine, session, 1);
    let cfg = store_config(sandbox.path(), "repo-cli");
    init_and_push(&cfg, machine, &stage);
    // The push record the machine's own `cmd_push` would have left, so
    // the single destination classifies healthy and the proof consults
    // it and holds the session.
    let state_dir = sandbox.path().join("data/chat-stasher/state");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    chat_stasher::pushstate::record_success(&state_dir, "default", machine, 3, now - 60).unwrap();
    // A configured-and-on mirror of the one destination; the copy's
    // directory exists but no completed round ever wrote a record — the
    // machine the D6(ii) gate exists for.
    let config_dir = sandbox.path().join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        "[mirror]\nenabled = true\nmirror_of = \"default\"\n",
    )
    .unwrap();
    fs::create_dir_all(sandbox.path().join("data/chat-stasher/mirror/default")).unwrap();

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
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(1),
        "the mirror leg refuses the deletion: {stdout}"
    );
    assert!(
        stdout.contains("the mirror has not synced to this snapshot yet"),
        "D6.3's sentence words the refusal: {stdout}"
    );
    // rev2 ⑤: the destination's noun phrase ends with the colon, so the
    // sentence reads as an assertion about it — never the bare
    // juxtaposition `destination "x" the mirror has not …` that parses
    // like nothing.
    assert!(
        stdout.contains(
            "  blocked: destination \"default\": the mirror has not synced to this snapshot \
             yet — 1 session(s) / "
        ),
        "the blocked line is the full sentence about the destination, with the counts: {stdout}"
    );
    assert!(
        stdout.contains("nothing was deleted"),
        "the refusal keeps the blocked shape: {stdout}"
    );
    assert!(
        stdout.contains("mirror sync"),
        "the fix names the round that frees the body: {stdout}"
    );
    assert_eq!(
        shard_count(&stage, machine, session),
        1,
        "the body must be untouched while the copy lags"
    );
}

/// M2 test: `reclaim_stage` synthesizes and persists the opencode increment map under
/// `<stage>/meta/<machine>/opencode-session-maps/` before deleting shard bodies,
/// respects an existing meta map (staleness branch), and reports persist failure.
#[test]
fn reclaim_stage_synthesizes_opencode_increment_map_before_deleting_body() {
    let dir = tempfile::tempdir().unwrap();
    let machine = "m";
    let session = "opencode.m.ses_m2_test";
    let stage = dir.path().join("stage");
    let cfg = store_config(dir.path(), "repo-a");

    // Stage an opencode session with 1 shard
    let body_bytes = chat_stasher::opencode_records::record_line(
        "ses_m2_test",
        chat_stasher::opencode_records::RecordKind::Message,
        "ses_m2_test-m1",
        &serde_json::json!({"role": "user", "text": "hello"}),
    );
    let body_line = String::from_utf8(body_bytes).unwrap();
    store::write_sealed_shard(
        StageWriter::Collect,
        &stage,
        machine,
        session,
        &[body_line.clone()],
    )
    .unwrap();

    let mk = init_and_push(&cfg, machine, &stage);
    let dests = vec![named(dir.path(), "repo-a", DestinationHealth::Healthy)];

    let meta_path =
        chat_stasher::collect::opencode_record_map_meta_path(&stage, machine, session).unwrap();
    assert!(
        !meta_path.exists(),
        "meta map must not exist before reclaim"
    );

    // 1. Synthesize-then-delete: reclaim synthesizes map and deletes shard body
    let report = stagereclaim::reclaim_stage(&stage, &dests, true, None).unwrap();
    assert!(
        !report.blocked(),
        "reclaim must not be blocked: {:?}",
        report.blocked
    );
    assert_eq!(report.reclaimed.len(), 1);
    assert_eq!(
        shard_count(&stage, machine, session),
        0,
        "body must be deleted"
    );
    assert!(
        meta_path.exists(),
        "meta map must have been synthesized and persisted"
    );

    let loaded = chat_stasher::collect::load_opencode_record_map(&meta_path, session);
    assert!(loaded.is_some(), "persisted map must be readable");
    let records = loaded.unwrap();
    assert_eq!(
        records.get("message:ses_m2_test-m1"),
        Some(&"09a32d6caf7737200a34ee38c65c82446831be896aa5d5aa6b858729ee13beb1".to_string())
    );

    // 2. Staleness branch: if meta_path already exists, it is untouched / not re-synthesized
    let custom_map = std::collections::BTreeMap::from([(
        "message:existing_pre_reclaim".to_string(),
        "custom_hash".to_string(),
    )]);
    chat_stasher::collect::save_opencode_record_map(&meta_path, session, &custom_map).unwrap();

    // Stage another shard and push again using existing mk
    store::write_sealed_shard(
        StageWriter::Collect,
        &stage,
        machine,
        session,
        &[body_line.clone()],
    )
    .unwrap();
    BackupStore::new(cfg.clone(), machine.to_string())
        .push(&stage, &mk)
        .unwrap();
    let report2 = stagereclaim::reclaim_stage(&stage, &dests, true, None).unwrap();
    assert!(!report2.blocked());
    assert_eq!(shard_count(&stage, machine, session), 0);

    let re_loaded = chat_stasher::collect::load_opencode_record_map(&meta_path, session).unwrap();
    assert_eq!(
        re_loaded.get("message:existing_pre_reclaim"),
        Some(&"custom_hash".to_string()),
        "existing meta map was not overwritten"
    );

    // 3. Persist failure is reported: make meta_path parent a read-only directory or file
    let session_fail = "opencode.m.ses_m2_fail";
    // Stage an opencode session with 1 shard
    let body_bytes_fail = chat_stasher::opencode_records::record_line(
        "ses_m2_fail",
        chat_stasher::opencode_records::RecordKind::Message,
        "ses_m2_fail-m1",
        &serde_json::json!({"role": "user", "text": "fail"}),
    );
    let body_line_fail = String::from_utf8(body_bytes_fail).unwrap();
    store::write_sealed_shard(
        StageWriter::Collect,
        &stage,
        machine,
        session_fail,
        std::slice::from_ref(&body_line_fail),
    )
    .unwrap();
    BackupStore::new(cfg.clone(), machine.to_string())
        .push(&stage, &mk)
        .unwrap();

    let fail_meta_path =
        chat_stasher::collect::opencode_record_map_meta_path(&stage, machine, session_fail)
            .unwrap();
    // Block write by placing a directory where the temp file would be
    fs::create_dir_all(fail_meta_path.parent().unwrap()).unwrap();
    let tmp = fail_meta_path.with_extension("json.tmp");
    fs::create_dir(&tmp).unwrap(); // fs::File::create(&temp) fails because it is a directory

    let err = stagereclaim::reclaim_stage(&stage, &dests, true, None);
    assert!(
        err.is_err(),
        "persist failure must not be silently swallowed"
    );
    let err_msg = format!("{:#}", err.unwrap_err());
    assert!(
        err_msg.contains("persist synthesized increment map"),
        "error message must report persist failure: {err_msg}"
    );
}

#[test]
fn reclaim_stage_unreadable_body_aborts_without_deleting_and_collect_first_protects_prior_candidates(
) {
    let dir = tempfile::tempdir().unwrap();
    let machine = "m";
    let session_valid = "opencode.m.ses_valid";
    let session_corrupt = "opencode.m.ses_corrupt";
    let stage = dir.path().join("stage");
    let cfg = store_config(dir.path(), "repo-a");

    // Stage valid opencode session
    let body_bytes_valid = chat_stasher::opencode_records::record_line(
        "ses_valid",
        chat_stasher::opencode_records::RecordKind::Message,
        "ses_valid-m1",
        &serde_json::json!({"role": "user", "text": "valid"}),
    );
    let body_line_valid = String::from_utf8(body_bytes_valid).unwrap();
    store::write_sealed_shard(
        StageWriter::Collect,
        &stage,
        machine,
        session_valid,
        &[body_line_valid],
    )
    .unwrap();

    // Stage corrupt opencode session (invalid JSON shard body)
    store::write_sealed_shard(
        StageWriter::Collect,
        &stage,
        machine,
        session_corrupt,
        &["this is not valid json\n".to_string()],
    )
    .unwrap();

    let _mk = init_and_push(&cfg, machine, &stage);
    let dests = vec![named(dir.path(), "repo-a", DestinationHealth::Healthy)];

    let err = stagereclaim::reclaim_stage(&stage, &dests, true, None);
    assert!(
        err.is_err(),
        "unreadable/unfoldable body must cause reclaim to fail"
    );
    let err_msg = format!("{:#}", err.unwrap_err());
    assert!(
        err_msg.contains("fold staged increment map"),
        "error message must report fold failure: {err_msg}"
    );

    // Both bodies must be kept (not deleted) because collect-first aborted before deletions started
    assert_eq!(
        shard_count(&stage, machine, session_valid),
        1,
        "valid candidate body must be kept when a sibling candidate fails in collect-first"
    );
    assert_eq!(
        shard_count(&stage, machine, session_corrupt),
        1,
        "unfoldable candidate body must be kept"
    );
}

#[test]
fn opencode_record_map_meta_path_validation_and_unchanged_save() {
    let dir = tempfile::tempdir().unwrap();
    let stage = dir.path().join("stage");

    // 1. Validation returns error on unsafe machine
    assert!(chat_stasher::collect::opencode_record_map_meta_path(
        &stage,
        "../escape",
        "opencode.m.s"
    )
    .is_err());
    assert!(
        chat_stasher::collect::opencode_record_map_meta_path(&stage, "", "opencode.m.s").is_err()
    );
    assert!(
        chat_stasher::collect::opencode_record_map_meta_path(&stage, "m/sub", "opencode.m.s")
            .is_err()
    );
    assert!(chat_stasher::collect::opencode_record_map_meta_path(
        &stage,
        "valid-machine",
        "opencode.m.s"
    )
    .is_ok());

    // 2. Unchanged map is not rewritten
    let meta_path =
        chat_stasher::collect::opencode_record_map_meta_path(&stage, "m", "opencode.m.s").unwrap();
    let records = std::collections::BTreeMap::from([("rec1".to_string(), "hash1".to_string())]);
    chat_stasher::collect::save_opencode_record_map(&meta_path, "opencode.m.s", &records).unwrap();
    assert!(meta_path.exists());

    let mtime1 = fs::metadata(&meta_path).unwrap().modified().unwrap();
    // Short sleep to ensure if rewritten, mtime would differ
    std::thread::sleep(std::time::Duration::from_millis(50));
    chat_stasher::collect::save_opencode_record_map(&meta_path, "opencode.m.s", &records).unwrap();
    let mtime2 = fs::metadata(&meta_path).unwrap().modified().unwrap();
    assert_eq!(mtime1, mtime2, "unchanged map must not be rewritten");
}
