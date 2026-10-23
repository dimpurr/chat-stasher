//! Black-box mirror tests (ADR-062 / LM-2): build a synthetic two-machine
//! repository with a real push, then exercise the copy round, the closure
//! record, the cross-round resume, the tamper heal, and the CLI contract.
//!
//! The three C1 closure fixtures (ADR-062 D6.4) map to:
//!   * (a) missing pack ⇒ not covered: `missing_pack_un_covers_and_the_next_sync_heals`
//!   * (b) one machine's snapshot synced, the other's not ⇒ the other's is
//!         not covered: `one_machines_snapshot_missing_is_not_covered`
//!   * (c) both complete ⇒ covered: `first_fill_copies_every_file_and_records_closure`
//!
//! The other named fixtures:
//!   * tampered file (47 §3.2): `tampered_mirror_files_are_re_downloaded`
//!   * a round cut mid-fill — the budget-cut shape C2 says must resume and
//!     not count as failure: `budget_cut_resumes_across_rounds_and_states_only_on_completion`
//!   * disk-floor pause: `disk_floor_pauses_the_round_without_failing_it`
//!   * the C3 pre-estimate refusal: `space_pre_estimate_refuses_before_any_download`
//!     and `mirror_sync_refuses_to_start_when_the_floor_cannot_fit` (CLI).
//!
//! Everything is local and synthetic, under temp dirs; the spawned-binary
//! tests redirect the full XDG envelope into a sandbox the way the other
//! suites do (W306).

use chat_stasher::config::Config;
use chat_stasher::mirror::{
    self, closure_snapshot_ids, mirror_dir, mirror_status_lines, status_json, sync_once,
    MirrorProgressRead, MirrorSettings, RoundEnd, RoundEnds, SyncReport, SyncRequest,
};
use chat_stasher::mirrorstate::{self, MirrorStateRead};
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

#[path = "../src/test_support.rs"]
mod test_support;

// ------------------------------------------------------------- fixtures

fn store_config(repo_path: &Path, key_path: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo_path.to_string_lossy().into_owned(),
        key_file: key_path.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        // Tests must not share the user's rustic cache (W289): a cached pack
        // would let a tampered or missing mirror file still read clean.
        cache_dir: None,
        no_cache: true,
    }
}

/// Write `nshards` sealed shards for one session, the stagereclaim helper's
/// shape.
fn write_session(stage: &Path, machine: &str, session: &str, nshards: u64) {
    for seq in 1..=nshards {
        let lines: Vec<String> = (0..10)
            .map(|i| format!("{{\"seq\":{seq},\"i\":{i},\"s\":\"payload-{session}-{seq}-{i}\"}}"))
            .collect();
        store::write_sealed_shard(StageWriter::Collect, stage, machine, session, &lines).unwrap();
    }
}

/// A two-machine repository pushed from two synthetic stages, plus the
/// mirrors-side scaffolding one sync round needs. Returns everything the
/// tests read: the remote StoreConfig, the masterkey, explicit data/state
/// roots (no ambient env, so in-process rounds never touch the real data
/// root), and the snapshot-id → machine mapping for the closure fixtures.
struct Rig {
    dir: tempfile::TempDir,
    remote: StoreConfig,
    mk: MasterKey,
    data_root: PathBuf,
    state_dir: PathBuf,
    settings: MirrorSettings,
    /// snapshot hex id → machine hostname, read back from the repository.
    snapshots: BTreeMap<String, String>,
}

impl Rig {
    fn machine_of(&self, machine: &str) -> String {
        self.snapshots
            .iter()
            .find(|(_id, host)| host.as_str() == machine)
            .map(|(id, _)| id.clone())
            .expect("each machine has exactly one snapshot")
    }

    fn mirror_root(&self) -> PathBuf {
        mirror_dir(&self.data_root, &self.settings.mirror_of)
    }
}

/// The machines' snapshots, read back from the pushed repository.
fn snapshots_by_id(remote: &StoreConfig, mk: &MasterKey) -> BTreeMap<String, String> {
    let store = BackupStore::for_metadata_query(remote.clone());
    let (repo, _init) = store.open_or_init(mk).unwrap();
    repo.get_all_snapshots()
        .unwrap()
        .into_iter()
        .map(|snap| (snap.id.to_hex().to_string(), snap.hostname.clone()))
        .collect()
}

fn build_rig(min_free_gib: Option<f64>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("storagebox");
    let key = dir.path().join("masterkey-storagebox.json");
    let remote = store_config(&repo, &key);

    let stage_a = dir.path().join("stage-a");
    let stage_b = dir.path().join("stage-b");
    write_session(&stage_a, "machine-a", "session-a1", 2);
    write_session(&stage_b, "machine-b", "session-b1", 3);

    let mk = MasterKey::new();
    store::persist_key_file(&remote, &mk).unwrap();
    BackupStore::new(remote.clone(), "machine-a".to_string())
        .push(&stage_a, &mk)
        .unwrap();
    BackupStore::new(remote.clone(), "machine-b".to_string())
        .push(&stage_b, &mk)
        .unwrap();

    let snapshots = snapshots_by_id(&remote, &mk);
    assert_eq!(snapshots.len(), 2, "two machines pushed, two snapshots");
    Rig {
        data_root: dir.path().join("data"),
        state_dir: dir.path().join("state"),
        dir,
        remote,
        mk,
        settings: MirrorSettings {
            enabled: true,
            mirror_of: "storagebox".to_string(),
            // The engine fixtures pin a near-zero floor so the rounds answer
            // the question under test (copy, cut, heal) rather than this dev
            // machine's own occupancy: the ADR's default floor is a real
            // max(10 GiB, 5% of capacity), which on a 95%-full build machine
            // correctly refuses a first fill — the refusal has its own test
            // below, and the default floor has its own unit test.
            min_free_gib: Some(min_free_gib.unwrap_or(0.0)),
        },
        snapshots,
    }
}

/// One in-process sync round against explicit roots (the CLI resolves the
/// real roots; the engine itself takes paths, which is what keeps these
/// tests off the machine's live data home).
fn run_round(rig: &Rig, round_ends: &dyn RoundEnds) -> (SyncReport, mirror::SyncOutcome) {
    sync_once(&SyncRequest {
        settings: &rig.settings,
        remote: &rig.remote,
        data_root: &rig.data_root,
        state_dir: &rig.state_dir,
        master_key: &rig.mk,
        now_unix: 1_800_000_000,
        round_ends,
    })
    .unwrap()
}

// ------------------------------------------------------------- stoppers

/// A round with no budget at all: the first thing it is asked about a file,
/// it stops — the cross-round resume still starts from real progress.
struct NeverStops;

impl RoundEnds for NeverStops {
    fn round_end(&self, _next: &mirror::PlannedFile) -> Option<RoundEnd> {
        None
    }
}

/// Ends the round once `limit` files have been started: a deterministic
/// budget cut, the shape C2 says must resume and never count as failure.
struct EndAfter {
    asked: AtomicUsize,
    limit: usize,
}

impl RoundEnds for EndAfter {
    fn round_end(&self, _next: &mirror::PlannedFile) -> Option<RoundEnd> {
        if self.asked.fetch_add(1, Ordering::SeqCst) >= self.limit {
            return Some(RoundEnd::BudgetSpent);
        }
        None
    }
}

/// The disk-floor stopper with no real disk pressure: proves the pause path
/// without a fixture needing to fill a volume.
struct FloorBroke;

impl RoundEnds for FloorBroke {
    fn round_end(&self, _next: &mirror::PlannedFile) -> Option<RoundEnd> {
        Some(RoundEnd::DiskBelowThreshold {
            free_bytes: 0,
            min_free_bytes: 0,
        })
    }
}

// ------------------------------------------------------------- helpers

/// Every regular file under `root` as repo-relative `/`-joined paths — one
/// spelling on every platform, the way the mirror layout is defined.
fn inventory(root: &Path) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if !name.starts_with('.') {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/");
                    files.insert(rel);
                }
            }
        }
    }
    files
}

fn assert_files_byte_equal(left: &Path, right: &Path, rel: &str) {
    let a = fs::read(left.join(rel)).unwrap();
    let b = fs::read(right.join(rel)).unwrap();
    assert_eq!(
        format!("{rel}: {} bytes", a.len()),
        format!("{rel}: {} bytes", b.len()),
        "{rel} must copy byte-exact"
    );
    let fingerprint = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .concat()
    };
    assert_eq!(
        fingerprint(&a),
        fingerprint(&b),
        "{rel} must copy byte-exact"
    );
}

/// The `status` config a machine declaring this mirror would hold.
fn mirror_config(toml_section: &str) -> Config {
    let raw = format!(
        "[destinations.storagebox]\nrepo = \"/tmp/formal-repo\"\nkey_file = \
         \"/tmp/formal-key.json\"\n\n[mirror]\n{toml_section}"
    );
    toml::from_str(&raw).expect("the fixture config must parse")
}

// ----------------------------------------------------------------- (c)
// C1 fixture (c): both machines' snapshots and everything they reference
// complete in the mirror ⇒ both covered — and the copy is byte-exact.

#[test]
fn first_fill_copies_every_file_and_records_closure() {
    let rig = build_rig(None);

    let (report, outcome) = run_round(&rig, &NeverStops);

    assert!(
        matches!(outcome, mirror::SyncOutcome::Completed),
        "first fill must complete: got {outcome:?} (report: {report:?})"
    );
    assert_eq!(report.snapshots_remote, 2);
    assert_eq!(report.covered_snapshots, 2, "fixture (c): both covered");
    assert!(!matches!(
        mirrorstate::load(&rig.state_dir),
        MirrorStateRead::Missing
    ));
    match mirrorstate::load(&rig.state_dir) {
        MirrorStateRead::Present(state) => {
            assert_eq!(state.closure_complete.len(), 2);
            assert!(state
                .closure_complete
                .contains(&rig.machine_of("machine-a")));
            assert!(state
                .closure_complete
                .contains(&rig.machine_of("machine-b")));
            assert_eq!(state.mirror_of, "storagebox");
            assert_eq!(state.remote_total_bytes, report.remote_total_bytes);
        }
        other => panic!("expected a recorded mirror state, got {other:?}"),
    }

    // Byte-exact, file by file: the whole remote inventory, equal in the
    // mirror, with `config` and every content-addressed file byte equal.
    // （`locks/` is nothing to compare: this build never writes locks.)
    let remote_files = inventory(Path::new(&rig.remote.repo_root));
    let mirror_files = inventory(&rig.mirror_root());
    assert_eq!(
        remote_files, mirror_files,
        "the mirror holds exactly the remote's files"
    );
    assert!(!remote_files.contains("locks"), "no locks directory exists");
    for rel in &remote_files {
        assert_files_byte_equal(Path::new(&rig.remote.repo_root), &rig.mirror_root(), rel);
    }

    // A complete round also reports completion through the progress record:
    // first-sync percent 100, complete true.
    match mirror::load_progress(&rig.state_dir) {
        MirrorProgressRead::Present(progress) => {
            assert!(progress.complete);
            assert_eq!(progress.percent(), 100);
            assert_eq!(progress.outcome, "complete");
        }
        other => panic!("expected a present progress record, got {other:?}"),
    }

    // The status surfaces stay silent on this healthy machine: complete
    // means no lines and no findings.
    let config = mirror_config("enabled = true\nmirror_of = \"storagebox\"");
    assert!(mirror_status_lines(&config, &rig.state_dir).is_empty());
    let json = status_json(&config, &rig.state_dir);
    assert_eq!(json["first_sync"]["kind"], "known");
    assert_eq!(json["first_sync"]["complete"], true);
    assert_eq!(json["closure_record"]["covered_snapshots"], 2);
}

// ------------------------------------------------------- cross-round resume
// C2's budget cut: a round stopped mid-fill writes progress, never state,
// never a failure; the next round resumes and the file set it already
// wrote is not written again.

#[test]
fn budget_cut_resumes_across_rounds_and_states_only_on_completion() {
    let rig = build_rig(None);

    // Round 1: stop after four files have been started — three copied, one
    // handed back to the queue. Whatever the count, it must be a strict
    // subset of the total: a cut, not a drain.
    let (report1, outcome1) = run_round(
        &rig,
        &EndAfter {
            asked: AtomicUsize::new(0),
            limit: 4,
        },
    );
    assert!(
        matches!(
            outcome1,
            mirror::SyncOutcome::Stopped(RoundEnd::BudgetSpent)
        ),
        "the cut must round-end as a budget pause, got {outcome1:?}"
    );
    let copied_round1 = report1.downloaded_files;
    assert!(copied_round1 > 0, "the first round copies before it cuts");
    assert!(
        copied_round1 < report1.remote_files,
        "the first round must leave the fill incomplete"
    );
    assert!(report1.missing_bytes_remaining > 0);

    // The state file is *absent*: only a completed round writes it (ruling
    // 2 — a cut must not move "what the mirror holds" forward).
    assert!(matches!(
        mirrorstate::load(&rig.state_dir),
        MirrorStateRead::Missing
    ));

    // The progress record is present and honest: partial, stopped, and
    // worded as a resume, not a failure.
    match mirror::load_progress(&rig.state_dir) {
        MirrorProgressRead::Present(progress) => {
            assert!(!progress.complete);
            assert_eq!(progress.outcome, "stopped");
            let note = progress.outcome_note.clone().unwrap_or_default();
            assert!(
                note.contains("resumes"),
                "note should say the next round resumes: {note}"
            );
            assert!(progress.percent() < 100);
            assert!(progress.percent() > 0, "progress, not a reset");
        }
        other => panic!("expected a present progress record, got {other:?}"),
    }

    // While the fill is cut, the status surface reports progress, not red:
    // the C2 line families ("first sync …percent…", "reclaim holds … back").
    let config = mirror_config("enabled = true\nmirror_of = \"storagebox\"");
    let lines = mirror_status_lines(&config, &rig.state_dir);
    assert_eq!(lines.len(), 1, "one mirror line while filling: {lines:?}");
    let line = &lines[0];
    assert!(line.contains("first sync"), "{line}");
    assert!(line.contains('%'), "{line}");
    assert!(line.contains("reclaim holds"), "{line}");
    assert!(
        !line.to_lowercase().contains("error"),
        "a first fill must not report as an error: {line}"
    );
    let json = status_json(&config, &rig.state_dir);
    assert_eq!(json["first_sync"]["kind"], "known");
    assert_eq!(json["closure_record"]["kind"], "unknown");

    // Round 2: no cut — the fill completes, and only the files the cut left
    // are downloaded: everything round 1 wrote is already-and-verified.
    let (report2, outcome2) = run_round(&rig, &NeverStops);
    assert!(
        matches!(outcome2, mirror::SyncOutcome::Completed),
        "the resumed round completes, got {outcome2:?}"
    );
    assert_eq!(
        report2.downloaded_files,
        report2.remote_files - report1.present_files - copied_round1,
        "resume downloads exactly what the cut left behind — nothing it already          wrote is fetched again"
    );
    match mirrorstate::load(&rig.state_dir) {
        MirrorStateRead::Present(state) => assert_eq!(state.closure_complete.len(), 2),
        other => panic!("the completed round records state, got {other:?}"),
    }
    match mirror::load_progress(&rig.state_dir) {
        MirrorProgressRead::Present(progress) => {
            assert!(progress.complete);
            assert_eq!(progress.percent(), 100);
        }
        other => panic!("expected a present progress record, got {other:?}"),
    }
}

// A zero-budget round is the degenerate cut: nothing copied, still a pause.

#[test]
fn a_spent_budget_round_copies_nothing_and_still_records_progress() {
    let rig = build_rig(None);

    let (report, outcome) = run_round(
        &rig,
        &EndAfter {
            asked: AtomicUsize::new(0),
            limit: 0,
        },
    );
    assert!(
        matches!(outcome, mirror::SyncOutcome::Stopped(RoundEnd::BudgetSpent)),
        "a spent budget stops before copying, got {outcome:?}"
    );
    assert_eq!(report.downloaded_files, 0);
    assert!(matches!(
        mirrorstate::load(&rig.state_dir),
        MirrorStateRead::Missing
    ));
    match mirror::load_progress(&rig.state_dir) {
        MirrorProgressRead::Present(progress) => {
            assert_eq!(progress.percent(), 0);
            assert_eq!(progress.outcome, "stopped");
        }
        other => panic!("expected a present progress record, got {other:?}"),
    }
}

// ------------------------------------------------------------- tamper
// 47 §3.2's tamper fixture: a mirror file that no longer matches its own
// name is re-downloaded and heals; `config`, the non-addressed file, heals
// by byte comparison.

#[test]
fn tampered_mirror_files_are_re_downloaded() {
    let rig = build_rig(None);
    run_round(&rig, &NeverStops);
    let mirror_root = rig.mirror_root();

    // Tamper one pack's bytes (its name still claims its original hash) and
    // the config's bytes.
    let mut pack_rel = String::new();
    for rel in inventory(&mirror_root) {
        if rel.starts_with("data/") {
            pack_rel = rel;
            break;
        }
    }
    assert!(!pack_rel.is_empty(), "the fixture repo has packs");
    let pack_path = mirror_root.join(&pack_rel);
    let mut pack = fs::read(&pack_path).unwrap();
    let mid = pack.len() / 2;
    pack[mid] ^= 0xff;
    fs::write(&pack_path, &pack).unwrap();
    let config_path = mirror_root.join("config");
    fs::write(&config_path, b"tampered config").unwrap();

    let (report, outcome) = run_round(&rig, &NeverStops);
    assert!(matches!(outcome, mirror::SyncOutcome::Completed));
    assert_eq!(
        report.downloaded_files, 2,
        "exactly the tampered pack and config re-download"
    );
    assert!(report.downloaded_bytes > 0);

    // Healed: both files byte-equal the remote again, and the covered set is
    // unchanged.
    assert_files_byte_equal(Path::new(&rig.remote.repo_root), &mirror_root, &pack_rel);
    assert_files_byte_equal(Path::new(&rig.remote.repo_root), &mirror_root, "config");
    match mirrorstate::load(&rig.state_dir) {
        MirrorStateRead::Present(state) => assert_eq!(state.closure_complete.len(), 2),
        other => panic!("expected the healed state, got {other:?}"),
    }
}

// ------------------------------------------------------- (a) missing pack
// C1 fixture (a): the mirror losing a pack an index names makes every
// snapshot's closure unclaimable — the judgement says not covered — and the
// next round heals it by re-downloading the pack.

#[test]
fn missing_pack_un_covers_and_the_next_sync_heals() {
    let rig = build_rig(None);
    run_round(&rig, &NeverStops);
    let mirror_root = rig.mirror_root();

    // Delete a pack the mirror's own index names.
    let mut pack_rel = String::new();
    for rel in inventory(&mirror_root) {
        if rel.starts_with("data/") {
            pack_rel = rel;
            break;
        }
    }
    assert!(!pack_rel.is_empty());
    fs::remove_file(mirror_root.join(&pack_rel)).unwrap();

    // The judgement: deleted pack ⇒ no snapshot's closure can be claimed.
    let (covered, anomaly) = closure_snapshot_ids(&mirror_root, &rig.mk);
    assert!(
        covered.is_empty(),
        "fixture (a): a missing referenced pack denies coverage"
    );
    let why = anomaly.unwrap_or_default();
    assert!(
        why.contains("pack(s) the mirror's index files name"),
        "{why}"
    );

    // The judge this stack actually exposes to a future reclaim agrees: the
    // coverage interface answers false on every snapshot while the mirror
    // is broken this way.
    let broken_state = mirrorstate::MirrorStateRead::Present(mirrorstate::MirrorState {
        version: mirrorstate::MIRROR_STATE_VERSION,
        finished_at_unix: 1,
        mirror_of: "storagebox".to_string(),
        closure_complete: covered,
        snapshots_on_remote: 2,
        remote_total_bytes: 1,
    });
    assert!(!mirrorstate::snapshot_covered(
        &broken_state,
        &rig.machine_of("machine-a")
    ));
    assert!(!mirrorstate::snapshot_covered(
        &broken_state,
        &rig.machine_of("machine-b")
    ));

    // And the next round re-downloads the pack, healing the closure.
    let (report, outcome) = run_round(&rig, &NeverStops);
    assert!(matches!(outcome, mirror::SyncOutcome::Completed));
    assert!(
        report.downloaded_files >= 1,
        "the heal re-downloads the deleted pack"
    );
    assert!(mirror_root.join(&pack_rel).is_file());
    match mirrorstate::load(&rig.state_dir) {
        MirrorStateRead::Present(state) => assert_eq!(state.closure_complete.len(), 2),
        other => panic!("the healed round records coverage, got {other:?}"),
    }
}

// ------------------------------------------------- (b) one machine synced
// C1 fixture (b): machine-b's snapshot and the whole shared pack set are in
// the mirror, machine-a's snapshot file is not ⇒ machine-a's session is
// judged not covered, machine-b's is covered.

#[test]
fn one_machines_snapshot_missing_is_not_covered() {
    let rig = build_rig(None);
    let mirror_root = rig.dir.path().join("partial-mirror");
    let snapshot_a = format!("snapshots/{}", rig.machine_of("machine-a"));
    let snapshot_b = format!("snapshots/{}", rig.machine_of("machine-b"));

    // Copy the whole remote repository except machine-a's snapshot — the
    // shape of a first fill whose round carried machine-b's line but not
    // machine-a's.
    let remote_root = Path::new(&rig.remote.repo_root);
    for rel in inventory(remote_root) {
        if rel == snapshot_a {
            continue;
        }
        let target = mirror_root.join(&rel);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(remote_root.join(&rel), &target).unwrap();
    }
    assert!(!mirror_root.join(&snapshot_a).is_file());
    assert!(mirror_root.join(&snapshot_b).is_file());

    let (covered, anomaly) = closure_snapshot_ids(&mirror_root, &rig.mk);
    assert!(anomaly.is_none(), "no packs are missing: {anomaly:?}");
    assert_eq!(
        covered.len(),
        1,
        "only the synced machine's snapshot can be covered"
    );
    assert!(
        covered.contains(&rig.machine_of("machine-b")),
        "machine-b's snapshot is closure-complete"
    );
    assert!(
        !covered.contains(&rig.machine_of("machine-a")),
        "fixture (b): machine-a's session is judged not covered"
    );
}

// ------------------------------------------------------------- disk floor
// C3's threshold: the disk-floor stop ends the round as a pause — no state
// write, progress recorded, no failure anywhere in the outcome.

#[test]
fn disk_floor_pauses_the_round_without_failing_it() {
    let rig = build_rig(None);

    let (report, outcome) = run_round(&rig, &FloorBroke);
    assert!(
        matches!(
            outcome,
            mirror::SyncOutcome::Stopped(RoundEnd::DiskBelowThreshold { .. })
        ),
        "the floor pauses, got {outcome:?}"
    );
    assert_eq!(report.downloaded_files, 0, "the pause writes nothing");
    assert!(matches!(
        mirrorstate::load(&rig.state_dir),
        MirrorStateRead::Missing
    ));
    match mirror::load_progress(&rig.state_dir) {
        MirrorProgressRead::Present(progress) => {
            assert_eq!(progress.outcome, "stopped");
            let note = progress.outcome_note.unwrap_or_default();
            assert!(note.contains("free disk fell below"), "{note}");
            assert!(note.contains("paused"), "{note}");
        }
        other => panic!("expected a present progress record, got {other:?}"),
    }
}

// ------------------------------------------------------------- pre-estimate
// C3's refusal: a floor the free space cannot fit refuses before a byte
// moves, with the numbers a human needs (how much, how short).

#[test]
fn space_pre_estimate_refuses_before_any_download() {
    // A floor of a million GiB cannot fit on any volume.
    let rig = build_rig(Some(1_000_000.0));

    let (report, outcome) = run_round(&rig, &NeverStops);
    match outcome {
        mirror::SyncOutcome::RefusedSpace {
            needed_bytes,
            missing_bytes,
            headroom_bytes,
            free_bytes,
        } => {
            assert!(missing_bytes > 0);
            assert_eq!(headroom_bytes, 1_000_000_u64 * 1_073_741_824);
            assert_eq!(needed_bytes, missing_bytes.saturating_add(headroom_bytes));
            assert!(free_bytes < needed_bytes);
            assert!(
                needed_bytes.saturating_sub(free_bytes) > 0,
                "the refusal says how much short"
            );
        }
        other => panic!("the pre-estimate must refuse, got {other:?}"),
    }
    assert_eq!(report.downloaded_files, 0, "the refusal copies nothing");
    assert!(
        inventory(&rig.mirror_root()).is_empty(),
        "the refusal moved nothing: no file exists in the mirror"
    );
    assert!(matches!(
        mirrorstate::load(&rig.state_dir),
        MirrorStateRead::Missing
    ));
    match mirror::load_progress(&rig.state_dir) {
        MirrorProgressRead::Present(progress) => {
            assert_eq!(progress.outcome, "refused");
            let note = progress.outcome_note.unwrap_or_default();
            assert!(note.contains("not started"), "{note}");
        }
        other => panic!("the refusal leaves a record saying why, got {other:?}"),
    }

    // And status speaks it in human words, still without red.
    let config =
        mirror_config("enabled = true\nmirror_of = \"storagebox\"\nmin_free_gib = 1000000.0");
    let lines = mirror_status_lines(&config, &rig.state_dir);
    assert!(!lines.is_empty(), "a refused mirror is worth a line");
    let line = lines.join("\n");
    assert!(line.contains("first sync"), "{line}");
    assert!(!line.to_lowercase().contains("error"), "{line}");
}

// ------------------------------------------------------------ swept files
// A subsequent round with an empty queue re-verifies the existing mirror
// and rewrites the same closure set — the steady-state contract the hourly
// pass exercises every hour.

#[test]
fn an_up_to_date_round_rewrites_state_without_downloading() {
    let rig = build_rig(None);
    run_round(&rig, &NeverStops);

    let (report, outcome) = run_round(&rig, &NeverStops);
    assert!(matches!(outcome, mirror::SyncOutcome::Completed));
    assert_eq!(report.downloaded_files, 0, "nothing new to copy");
    assert_eq!(report.covered_snapshots, 2);
}

// ------------------------------------------------------- CLI + serverless

/// A `chat-stasher` binary run with the full isolation envelope.
fn binary_command(sandbox: &Path) -> std::process::Command {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .env("HOME", &home)
        .env("USERPROFILE", &home)
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

fn write_config(sandbox: &Path, extra_mirror: &str) {
    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    let repo = sandbox.join("remote-repo");
    let key = sandbox.join("remote.key");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[destinations.storagebox]\nrepo = \"{}\"\nkey_file = \"{}\"\nno_cache = \
             true\n\n[mirror]\n{extra_mirror}",
            repo.display(),
            key.display()
        ),
    )
    .unwrap();
}

/// Push a two-machine synthetic repository for CLI-level tests.
fn cli_fixture_remote(sandbox: &Path) {
    let stage_a = sandbox.join("stage-a");
    let stage_b = sandbox.join("stage-b");
    write_session(&stage_a, "machine-a", "session-a1", 2);
    write_session(&stage_b, "machine-b", "session-b1", 2);
    let cfg = store_config(&sandbox.join("remote-repo"), &sandbox.join("remote.key"));
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    for (stage, machine) in [(&stage_a, "machine-a"), (&stage_b, "machine-b")] {
        BackupStore::new(cfg.clone(), machine.to_string())
            .push(stage, &mk)
            .unwrap();
    }
}

fn mirror_root_of(sandbox: &Path) -> PathBuf {
    sandbox
        .join("data")
        .join("chat-stasher")
        .join("mirror")
        .join("storagebox")
}

#[test]
fn mirror_sync_refuses_usage_when_unconfigured_or_off() {
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();

    // No config at all: usage, named plainly.
    let out = binary_command(sandbox)
        .args(["mirror", "sync"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no [mirror] section"),
        "stderr should name the missing section:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Section present, switch off: usage, again saying what to change.
    write_config(sandbox, "enabled = false\nmirror_of = \"storagebox\"");
    let out = binary_command(sandbox)
        .args(["mirror", "sync"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("enabled is false"));

    // Switch on, name wrong: usage, with the declared list to copy from.
    write_config(sandbox, "enabled = true\nmirror_of = \"nope\"");
    let out = binary_command(sandbox)
        .args(["mirror", "sync"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("names no declared destination"), "{stderr}");
    assert!(
        stderr.contains("storagebox"),
        "declared list names the fix: {stderr}"
    );
}

#[test]
fn mirror_sync_clones_a_repository_round_trip_over_the_cli() {
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();
    cli_fixture_remote(sandbox);
    // A pinned floor: the ADR's default (max(10 GiB, 5% of the volume)) is a
    // real refusal on this 95%-full build machine, and the refusal has its
    // own test — this one is about the copy.
    write_config(
        sandbox,
        "enabled = true\nmirror_of = \"storagebox\"\nmin_free_gib = 1.0",
    );

    let out = binary_command(sandbox)
        .args(["mirror", "sync", "--budget-secs", "300"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "the first sync round completes, got {:?}\n{all}",
        out.status
    );
    assert!(
        all.contains("[mirror] syncing      : `storagebox`"),
        "{all}"
    );
    assert!(
        all.contains("complete — 2 of 2 snapshots' closures covered"),
        "{all}"
    );

    // The mirror lives under the sandboxed data root, byte-exact.
    let mirror_root = mirror_root_of(sandbox);
    let remote_root = sandbox.join("remote-repo");
    assert_eq!(
        inventory(&remote_root),
        inventory(&mirror_root),
        "the CLI round copies the whole repository"
    );
    assert!(
        sandbox
            .join("data")
            .join("chat-stasher")
            .join("state")
            .join("mirror-state.json")
            .is_file(),
        "state lands in the state dir with the other records"
    );

    // A second round is a cheap no-op re-verification.
    let out = binary_command(sandbox)
        .args(["mirror", "sync"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "the steady-state round completes: {all}"
    );

    // `status` on the now-complete mirror is silent about the mirror and
    // does not go red over it; --json carries the honest kinds.
    let out = binary_command(sandbox).args(["status"]).output().unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !all.contains("first sync"),
        "a complete mirror is silent in status: {all}"
    );
    let out = binary_command(sandbox)
        .args(["status", "--json"])
        .output()
        .unwrap();
    let json = String::from_utf8_lossy(&out.stdout);
    assert!(json.contains("\"mirror\""), "{json}");
    assert!(json.contains("\"first_sync\""), "{json}");
    assert!(
        json.contains("\"kind\":\"known\""),
        "complete-first-sync is a known state: {json}"
    );
    assert!(
        json.contains("\"closure_record\""),
        "coverage is on the wire for the future reclaim leg: {json}"
    );
}

#[test]
fn mirror_sync_refuses_to_start_when_the_floor_cannot_fit() {
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();
    cli_fixture_remote(sandbox);
    write_config(
        sandbox,
        "enabled = true\nmirror_of = \"storagebox\"\nmin_free_gib = 1000000.0",
    );

    let out = binary_command(sandbox)
        .args(["mirror", "sync"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(3),
        "the C3 pre-estimate refusal is a did-not-finish exit, got {all}"
    );
    assert!(all.contains("not enough free space"), "{all}");
    assert!(all.contains("needs"), "the refusal says how much: {all}");
    assert!(
        all.contains("short by"),
        "the refusal says how short: {all}"
    );
    assert!(
        !inventory(&mirror_root_of(sandbox))
            .iter()
            .any(|rel| rel == "config"),
        "the refusal moved nothing"
    );
}

#[test]
fn run_once_mirrors_after_its_push_and_again_on_a_noop_pass() {
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "push_only_if_changed = false\n\n[destinations.storagebox]\nrepo = \
             \"{}\"\nkey_file = \"{}\"\nno_cache = true\n\n[mirror]\nenabled = \
             true\nmirror_of = \"storagebox\"\nmin_free_gib = 1.0\n",
            sandbox.join("remote-repo").display(),
            sandbox.join("remote.key").display()
        ),
    )
    .unwrap();

    // One sealed claude-code shard, the run-once harness's shape.
    let cc_line = |ts: &str| {
        format!(
            r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
        )
    };
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("000001.jsonl"),
        cc_line("2025-01-15T12:34:56.789Z") + "\n",
    )
    .unwrap();

    // The pass owns its key init like any first run: `run-once` pushes, and
    // the post-push copy runs with whatever the pass established.
    let mut command = binary_command(sandbox);
    command.env("CHAT_STASHER_REGISTRY", sandbox.join("registry.json"));
    fs::write(
        sandbox.join("registry.json"),
        r#"{"schema_version":1,"generated":"T1 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    let out = command
        .args([
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
            "--destination",
            "storagebox",
            "--keep-ssh-masters",
        ])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "the pass succeeds and mirrors with it, got {:?}\n{all}",
        out.status
    );
    assert!(
        all.contains("[run-once] mirror-sync: complete"),
        "the copy step reports inside the pass:\n{all}"
    );
    let state_path = sandbox
        .join("data")
        .join("chat-stasher")
        .join("state")
        .join("mirror-state.json");
    assert!(
        state_path.is_file(),
        "the pass's mirror round records coverage"
    );
    let recorded = fs::read_to_string(&state_path).unwrap();
    assert!(recorded.contains("\"closure_complete\""), "{recorded}");
    assert!(
        recorded.contains("\"mirror_of\": \"storagebox\""),
        "{recorded}"
    );

    // A second pass with nothing new: push skipped, mirror still syncs —
    // "after the push" is the round's order, not a demand the push write a
    // new snapshot each time. Machine-b via a fresh session would change
    // nothing here; the assertion is the NOOP-path copy step running.
    let mut command = binary_command(sandbox);
    command.env("CHAT_STASHER_REGISTRY", sandbox.join("registry.json"));
    let out = command
        .args([
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
            "--destination",
            "storagebox",
            "--keep-ssh-masters",
        ])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the second pass succeeds too: {all}");
    // The steady follow-up pass copies again at its tail whether its push
    // made a snapshot or skipped-as-unchanged: the pull half of "push, then
    // mirror" exists so a machine with no changes of its own still picks up
    // the *remote's* growth (ADR-062 D2), so the assertion is about the
    // step running, not about which push verdict the pass took.
    assert!(
        all.contains("[run-once] mirror-sync:"),
        "every pass tail runs the mirror step:\n{all}"
    );
}
