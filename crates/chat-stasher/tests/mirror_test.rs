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
//!   * the exit-code trichotomy of the rev0 fixes: hard failures
//!     (`a_twice_corrupt_source_fails_the_round_not_pauses_it`,
//!     `a_local_write_failure_fails_the_round_and_the_note_carries_no_path`,
//!     CLI `mirror_sync_exits_1_when_the_source_proved_corrupt`), the
//!     usage class (`an_uninitialised_destination_is_a_usage_refusal_that_creates_nothing`,
//!     CLI `mirror_sync_reports_an_uninitialised_destination_as_usage`), and
//!     the pass-side ssh-master reap
//!     (`the_pass_reaps_the_ssh_masters_its_mirror_step_opened`).
//!
//! Everything is local and synthetic, under temp dirs; the spawned-binary
//! tests redirect the full XDG envelope into a sandbox the way the other
//! suites do (W306).

use chat_stasher::config::Config;
use chat_stasher::json_out::CountState;
use chat_stasher::mirror::{
    self, closure_snapshot_ids, mirror_dir, mirror_status, mirror_status_lines, status_json,
    sync_once, MirrorProgressRead, MirrorSettings, RoundEnd, RoundEnds, SyncReport, SyncRequest,
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

/// The steady row's date spelling, for assertions about "last sync … UTC"
/// against a pinned now. One place, so a wording change updates the
/// assertion beside the code that words it.
fn chrono_date(unix: u64) -> String {
    chrono::DateTime::from_timestamp(
        i64::try_from(unix).expect("the pinned test epoch is representable"),
        0,
    )
    .expect("the pinned test epoch is representable")
    .format("%Y-%m-%d %H:%M")
    .to_string()
}

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
    // The floor is the caller's to compute (rev2's single-floor fix):
    // the rig pins a near-zero `min_free_gib`, so the settings-derived
    // value is `gib.max(0.0) * GIB` — computed once, shared by the
    // request and the stop-rule, the same shape the CLI callers keep.
    sync_once(&SyncRequest {
        settings: &rig.settings,
        remote: &rig.remote,
        data_root: &rig.data_root,
        state_dir: &rig.state_dir,
        master_key: &rig.mk,
        now_unix: 1_800_000_000,
        min_free_bytes: rig.settings.min_free_bytes(&rig.mirror_root()),
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

/// The `status` config a machine declaring this mirror would hold. A second
/// declared destination, `storagebox-v2`, rides along so a repointed
/// `mirror_of` can name a destination that *is* declared — the fixture for
/// the stale-record tests.
fn mirror_config(toml_section: &str) -> Config {
    let raw = format!(
        "[destinations.storagebox]\nrepo = \"/tmp/formal-repo\"\nkey_file = \
         \"/tmp/formal-key.json\"\n\n[destinations.storagebox-v2]\nrepo = \
         \"/tmp/formal-repo-2\"\nkey_file = \"/tmp/formal-key-2.json\"\n\n[mirror]\n{toml_section}"
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
    // (`locks/` is nothing to compare: this build never writes locks.)
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

    // LM-4 / ADR-062 D3: a completed, healthy copy is no longer silent —
    // the steady row carries the settled field family (size, last sync,
    // snapshots behind, orphans), with every fact measured: a byte-exact
    // copy's size is the listing's own bytes, behindness is zero, and the
    // first fill measured its own orphans to zero.
    let config = mirror_config("enabled = true\nmirror_of = \"storagebox\"");
    let now: u64 = 1_800_000_000;
    let status = mirror_status(&config, &rig.data_root, &rig.state_dir, now);
    let rows = &status.rows;
    assert_eq!(
        rows.len(),
        1,
        "one steady row on a healthy mirror: {rows:?}"
    );
    let row = &rows[0].sentence;
    assert!(row.contains("[mirror] local copy of `storagebox`"), "{row}");
    assert!(
        row.contains(&format!("({} B) across", report.remote_total_bytes)),
        "{row}"
    );
    assert!(row.contains("file(s)"), "{row}");
    assert!(
        row.contains(&format!(
            "last sync {} UTC (less than a minute ago)",
            chrono_date(1_800_000_000)
        )),
        "the row words the completed sync's time and age: {row}"
    );
    assert!(row.contains("0 snapshot(s) behind"), "{row}");
    assert!(row.contains("0 orphan file(s)"), "{row}");
    assert_eq!(rows[0].severity, mirror::MirrorRowSeverity::Info);
    assert!(matches!(
        &status.size,
        mirror::MirrorUsageState::Known { bytes, .. } if *bytes == report.remote_total_bytes
    ));
    assert!(matches!(
        &status.orphans,
        mirror::MirrorUsageState::Known { files: 0, bytes: 0 }
    ));
    assert_eq!(
        status.snapshots_behind,
        CountState::Known { count: 0 },
        "a completed closure covers every listed snapshot"
    );
    assert!(matches!(
        status.last_sync,
        mirror::MirrorLastSyncState::Known {
            unix: 1_800_000_000
        }
    ));
    // `status --json` carries the same family beside the steady row, so
    // the third surface (the menu bar) decodes what this row says.
    let json = status_json(&config, &rig.data_root, &rig.state_dir, now);
    assert_eq!(json["first_sync"]["kind"], "known");
    assert_eq!(json["first_sync"]["complete"], true);
    assert_eq!(json["closure_record"]["covered_snapshots"], 2);
    assert_eq!(json["size"]["kind"], "known");
    assert_eq!(json["size"]["bytes"], report.remote_total_bytes);
    assert_eq!(
        json["snapshots_behind"],
        serde_json::json!({"kind": "known", "count": 0})
    );
    assert_eq!(
        json["orphans"],
        serde_json::json!({"kind": "known", "files": 0, "bytes": 0})
    );
    assert_eq!(
        json["last_sync"],
        serde_json::json!({"kind": "known", "unix": 1_800_000_000})
    );
    let rows_json = json["rows"].as_array().expect("rows is an array");
    assert_eq!(rows_json.len(), 1);
    assert_eq!(rows_json[0]["severity"], "info");
    assert_eq!(rows_json[0]["sentence"], serde_json::json!(row));
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
    // the C2 line families ("first sync …percent…", "reclaim holds … back"),
    // now with the row's severity declared for the menu bar's ladder
    // (info — progress, never a red over a first fill).
    let config = mirror_config("enabled = true\nmirror_of = \"storagebox\"");
    let status = mirror_status(&config, &rig.data_root, &rig.state_dir, 1_800_000_000);
    assert_eq!(status.rows.len(), 1, "one mirror row while filling");
    let row = &status.rows[0];
    assert_eq!(row.severity, mirror::MirrorRowSeverity::Info);
    let line = &row.sentence;
    assert!(line.contains("first sync"), "{line}");
    assert!(line.contains('%'), "{line}");
    assert!(line.contains("reclaim holds"), "{line}");
    assert!(
        !line.to_lowercase().contains("error"),
        "a first fill must not report as an error: {line}"
    );
    // The family fields stay honest mid-fill: the size is the partial
    // copy's real bytes, last sync and behind are unknown (no completed
    // round exists to read), and orphans are the round's own measurement.
    assert!(matches!(
        status.last_sync,
        mirror::MirrorLastSyncState::Unknown { .. }
    ));
    assert!(matches!(
        status.snapshots_behind,
        CountState::Unknown { .. }
    ));
    assert!(matches!(
        &status.orphans,
        mirror::MirrorUsageState::Known { files: 0, bytes: 0 }
    ));
    let json = status_json(&config, &rig.data_root, &rig.state_dir, 1_800_000_000);
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
        "storagebox",
        &rig.machine_of("machine-a")
    ));
    assert!(!mirrorstate::snapshot_covered(
        &broken_state,
        "storagebox",
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
    let lines = mirror_status_lines(&config, &rig.data_root, &rig.state_dir);
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

// --------------------------------------------------- exit-code trichotomy
// A hard failure must not fold into "did not finish" (invariant 2,
// `CLAUDE.md`): verified source corruption and a refused local write are
// checked-and-failed facts — `Err` out of the engine, exit 1 at the CLI —
// while the pause family (budget, disk floor, remote readability) stays
// the resumable exit 3.

/// Corrupt one remote file's bytes under its own name, so both download
/// attempts fail verification against the name its listing still claims.
fn corrupt_first_remote_pack(rig: &Rig) -> String {
    let remote_root = Path::new(&rig.remote.repo_root);
    let mut pack_rel = String::new();
    for rel in inventory(remote_root) {
        if rel.starts_with("data/") {
            pack_rel = rel;
            break;
        }
    }
    assert!(!pack_rel.is_empty(), "the fixture repository has packs");
    let pack_path = remote_root.join(&pack_rel);
    let mut pack = fs::read(&pack_path).unwrap();
    let mid = pack.len() / 2;
    pack[mid] ^= 0xff;
    fs::write(&pack_path, &pack).unwrap();
    pack_rel
}

#[test]
fn a_twice_corrupt_source_fails_the_round_not_pauses_it() {
    let rig = build_rig(None);
    corrupt_first_remote_pack(&rig);

    let request = SyncRequest {
        settings: &rig.settings,
        remote: &rig.remote,
        data_root: &rig.data_root,
        state_dir: &rig.state_dir,
        master_key: &rig.mk,
        now_unix: 1_800_000_000,
        min_free_bytes: rig.settings.min_free_bytes(&rig.mirror_root()),
        round_ends: &NeverStops,
    };
    let err = sync_once(&request)
        .expect_err("a twice-mismatching source is checked and failed, not a pause");
    assert!(
        err.is::<chat_stasher::mirror::HardFailure>(),
        "the hard class is typed for the CLI's exit-1 arm, got: {err:#}"
    );
    match err.downcast_ref::<chat_stasher::mirror::HardFailure>() {
        Some(hard) => {
            assert_eq!(hard.class, chat_stasher::mirror::HardClass::SourceCorrupt);
            assert!(
                hard.why.contains("hashed against its own name twice"),
                "{}",
                hard.why
            );
        }
        None => panic!("downcast cannot fail right after `is` matched"),
    }

    // The progress record words the failure as a failure, with the
    // state-file-safe sentence (ids only, no path).
    match mirror::load_progress(&rig.state_dir) {
        MirrorProgressRead::Present(progress) => {
            assert_eq!(progress.outcome, "failed", "not the pause family");
            let note = progress.outcome_note.unwrap_or_default();
            assert!(note.contains("hashed against its own name twice"), "{note}");
            assert!(
                !note.contains(&rig.remote.repo_root),
                "state files carry no paths: {note}"
            );
        }
        other => panic!("a failed round still writes progress, got {other:?}"),
    }
    assert!(matches!(
        mirrorstate::load(&rig.state_dir),
        MirrorStateRead::Missing
    ));

    // And `status` reads it as the failure it is, with the class sentence.
    let config = mirror_config("enabled = true\nmirror_of = \"storagebox\"");
    let lines = mirror_status_lines(&config, &rig.data_root, &rig.state_dir);
    let joined = lines.join("\n");
    assert!(
        joined.contains("the last mirror round failed"),
        "a failed round is not first-fill progress: {joined}"
    );
    assert!(
        joined.contains("source corruption"),
        "the status line carries the class sentence: {joined}"
    );
}

#[test]
fn a_local_write_failure_fails_the_round_and_the_note_carries_no_path() {
    let rig = build_rig(None);
    // Plant a directory where `config` must be written: the temp file is
    // created and written fine, the rename onto the existing directory
    // fails on every platform — a local (ENOTDIR/EISDIR-style) write error.
    let mirror_root = rig.mirror_root();
    fs::create_dir_all(&mirror_root).unwrap();
    fs::create_dir(mirror_root.join("config")).unwrap();

    let request = SyncRequest {
        settings: &rig.settings,
        remote: &rig.remote,
        data_root: &rig.data_root,
        state_dir: &rig.state_dir,
        master_key: &rig.mk,
        now_unix: 1_800_000_000,
        min_free_bytes: rig.settings.min_free_bytes(&rig.mirror_root()),
        round_ends: &NeverStops,
    };
    let err = sync_once(&request).expect_err("a refused local write is a hard failure");
    assert!(err.is::<chat_stasher::mirror::HardFailure>(), "{err:#}");
    match err.downcast_ref::<chat_stasher::mirror::HardFailure>() {
        Some(hard) => {
            assert_eq!(hard.class, chat_stasher::mirror::HardClass::LocalWrite);
            assert!(hard.why.contains("the local write failed"), "{}", hard.why);
            assert!(
                !hard.why.contains("mirror"),
                "the state-file-safe sentence carries the why, not the path: {}",
                hard.why
            );
            // The terminal-only half exists and carries the raw io detail
            // (its wording is platform-specific; the boundary under test is
            // that it is a *separate* half from the state-file `why`).
            let detail = hard.detail.clone();
            assert!(
                detail
                    .as_deref()
                    .map_or(false, |d| !d.is_empty() && d != hard.why),
                "the terminal-only detail keeps what humans need at the keyboard: {detail:?}"
            );
        }
        None => panic!("downcast cannot fail right after `is` matched"),
    }

    // The recorded note keeps the io class, never the raw chain.
    match mirror::load_progress(&rig.state_dir) {
        MirrorProgressRead::Present(progress) => {
            assert_eq!(progress.outcome, "failed");
            let note = progress.outcome_note.unwrap_or_default();
            assert!(note.contains("the local write failed"), "{note}");
            assert!(
                !note.contains(mirror_root.to_string_lossy().as_ref()),
                "state files carry no paths: {note}"
            );
        }
        other => panic!("a failed round still writes progress, got {other:?}"),
    }
    // A local write failure is not a remote-read pause: nothing anywhere
    // says the destination could not be read.
    let joined_progress = format!("{:?}", mirror::load_progress(&rig.state_dir));
    assert!(
        !joined_progress.contains("could not be read mid-fill"),
        "a local error is not worded as a remote one: {joined_progress}"
    );
}

// The paused (exit 3) half keeps its redaction: a mid-round remote read
// failure records the *class* of the error in the progress file, never the
// raw backend chain, which carries the repository path.
struct DeleteOneRemoteOnFirstAsk {
    path: PathBuf,
    fired: std::sync::atomic::AtomicBool,
}

impl RoundEnds for DeleteOneRemoteOnFirstAsk {
    fn round_end(&self, _next: &mirror::PlannedFile) -> Option<RoundEnd> {
        if !self.fired.swap(true, Ordering::SeqCst) {
            // The remove is the fixture's whole point, so its outcome is
            // asserted rather than dropped: half of the test's premise is
            // that the pack really is gone before the first read.
            assert!(
                fs::remove_file(&self.path).is_ok(),
                "the fixture must be able to delete the pack it names"
            );
        }
        None
    }
}

#[test]
fn a_mid_round_remote_read_pause_records_a_class_not_the_raw_error() {
    let rig = build_rig(None);
    let pack_rel = {
        let remote_root = Path::new(&rig.remote.repo_root);
        let mut found = String::new();
        for rel in inventory(remote_root) {
            if rel.starts_with("data/") {
                found = rel;
                break;
            }
        }
        found
    };
    assert!(!pack_rel.is_empty());

    let stopper = DeleteOneRemoteOnFirstAsk {
        path: Path::new(&rig.remote.repo_root).join(&pack_rel),
        fired: std::sync::atomic::AtomicBool::new(false),
    };
    let (report, outcome) = run_round(&rig, &stopper);
    assert!(
        matches!(
            &outcome,
            mirror::SyncOutcome::Stopped(RoundEnd::RemoteRead { .. })
        ),
        "the deleted pack's failed read pauses the round, got {outcome:?} (report: {report:?})"
    );

    match mirror::load_progress(&rig.state_dir) {
        MirrorProgressRead::Present(progress) => {
            let note = progress.outcome_note.unwrap_or_default();
            assert!(
                note.contains("the destination could not be read mid-fill"),
                "{note}"
            );
            assert!(
                note.contains("the raw error is not recorded in state files"),
                "{note}"
            );
            assert!(
                !note.contains(&rig.remote.repo_root),
                "a remote-read pause still writes no paths: {note}"
            );
        }
        other => panic!("a paused round writes progress, got {other:?}"),
    }
}

// ------------------------------------------------------------- usage class
// A destination with no repository on it is a usage problem (exit 2, per
// the command's own list), and a refused usage round creates nothing: no
// empty mirror directory left behind.

#[test]
fn an_uninitialised_destination_is_a_usage_refusal_that_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("empty-dest");
    fs::create_dir_all(&repo).unwrap();
    let remote = store_config(&repo, &dir.path().join("some-key.json"));
    let settings = MirrorSettings {
        enabled: true,
        mirror_of: "empty-dest".to_string(),
        min_free_gib: None,
    };
    let data_root = dir.path().join("data");
    let state_dir = dir.path().join("state");
    let request = SyncRequest {
        settings: &settings,
        remote: &remote,
        data_root: &data_root,
        state_dir: &state_dir,
        master_key: &MasterKey::new(),
        now_unix: 1_800_000_000,
        // The engine never reads the floor on this path (the round is
        // refused before the round starts), but the field is part of the
        // one-floor contract, so the fixture hands the settings-derived
        // value in like every caller does.
        min_free_bytes: settings.min_free_bytes(&mirror_dir(&data_root, &settings.mirror_of)),
        round_ends: &NeverStops,
    };
    let err = sync_once(&request).expect_err("no config file listed is a usage refusal");
    assert!(
        err.is::<chat_stasher::mirror::MirrorUsageError>(),
        "usage is typed for the CLI's exit-2 arm, got: {err:#}"
    );
    let text = format!("{err:#}");
    assert!(text.contains("has not been initialised"), "{text}");
    assert!(text.contains("run a push first"), "{text}");
    assert!(
        !mirror_dir(&data_root, "empty-dest").exists(),
        "a usage refusal creates no empty mirror directory"
    );
    assert!(matches!(
        mirrorstate::load(&state_dir),
        MirrorStateRead::Missing
    ));
    assert!(matches!(
        mirror::load_progress(&state_dir),
        MirrorProgressRead::Missing
    ));
}

// ------------------------------------------------------------ stale record
// Repoint `[mirror] mirror_of` and the completed record the old destination
// left behind must not stand as coverage for the new one (issue 4): the
// status surfaces day the mismatch, `--json` answers unknown, and the
// reclaim-side read interface answers false even for a snapshot id the
// record genuinely contains.

// -------------------------------------------------------------- LM-4 rows
// The steady row's settled field family and spellings (ADR-062 D3/D6.3,
// ruling 8): one row, one wording, three surfaces. The record-fixture
// tests pin the measure without a copier in the loop — the surfaces read the
// records, so a hand-written record is the honest instrument.

/// Write a completed-round progress record the way `write_progress` would
/// (the record is the interface; LM-3's tests build it the same way).
fn write_progress_record(state_dir: &Path, progress: &mirror::MirrorProgress) {
    fs::create_dir_all(state_dir).unwrap();
    fs::write(
        mirror::mirror_progress_path(state_dir),
        serde_json::to_vec_pretty(progress).unwrap(),
    )
    .unwrap();
}

/// Write a completed-round closure record the way `mirrorstate::save`
/// would (the record is the interface the surfaces read, so a hand-built
/// record is the honest fixture for this state).
fn write_state_record(state_dir: &Path, record: &mirrorstate::MirrorState) {
    fs::create_dir_all(state_dir).unwrap();
    fs::write(
        mirrorstate::mirror_state_path(state_dir),
        serde_json::to_vec_pretty(record).unwrap(),
    )
    .unwrap();
}

/// D6.3's behind count, verbatim the settled measure: the snapshots the record's listing
/// held minus the closures it verified — same record, so the row's age and
/// its behind count cannot come from two different rounds — with the
/// orphan pair and the row spelling asserted beside them.
#[test]
fn the_steady_row_words_behindness_orphans_and_age_from_the_records() {
    let sb = tempfile::TempDir::new().unwrap();
    let state_dir = sb.path().join("state");
    let data_root = sb.path().join("data");
    fs::create_dir_all(&state_dir).unwrap();

    let mut closure = BTreeSet::new();
    closure.insert("a".repeat(64));
    closure.insert("b".repeat(64));
    write_state_record(
        &state_dir,
        &mirrorstate::MirrorState {
            version: mirrorstate::MIRROR_STATE_VERSION,
            finished_at_unix: 1_800_000_000,
            mirror_of: "storagebox".to_string(),
            closure_complete: closure,
            snapshots_on_remote: 5,
            remote_total_bytes: 4096,
        },
    );
    write_progress_record(
        &state_dir,
        &mirror::MirrorProgress {
            version: mirror::MIRROR_PROGRESS_VERSION,
            mirror_of: "storagebox".to_string(),
            round_finished_at_unix: 1_800_000_000,
            complete: true,
            outcome: "complete".to_string(),
            outcome_note: None,
            remote_total_bytes: 4096,
            mirror_bytes: 4096,
            remote_files: 9,
            mirror_files: 9,
            orphan_files: Some(1),
            orphan_bytes: Some(1_234),
        },
    );

    let config = mirror_config("enabled = true\nmirror_of = \"storagebox\"");
    let status = mirror_status(&config, &data_root, &state_dir, 1_800_003_600);
    assert_eq!(status.rows.len(), 1, "completed records mean one row");
    assert_eq!(status.rows[0].severity, mirror::MirrorRowSeverity::Info);
    let row = &status.rows[0].sentence;
    assert_eq!(
        status.snapshots_behind,
        CountState::Known { count: 3 },
        "5 listed, 2 covered: the rest are behind, from the same record"
    );
    assert!(matches!(
        status.last_sync,
        mirror::MirrorLastSyncState::Known {
            unix: 1_800_000_000
        }
    ));
    // The directory does not exist: a measured empty, not an unknown.
    assert!(matches!(
        status.size,
        mirror::MirrorUsageState::Known { files: 0, bytes: 0 }
    ));
    assert!(row.contains("[mirror] local copy of `storagebox`"), "{row}");
    assert!(row.contains("3 snapshot(s) behind"), "{row}");
    assert!(
        row.contains(&format!(
            "last sync {} UTC (1 hour ago)",
            chrono_date(1_800_000_000)
        )),
        "{row}"
    );
    assert!(
        row.contains("1 orphan file(s) (1.2 KiB reclaimable)"),
        "{row}"
    );
    assert!(row.contains("0 B) across 0 file(s)"), "{row}");

    // The same family on the wire, tagged the way the rest of --json keeps
    // its tri-states, so the menu bar decodes what the row says.
    let json = status_json(&config, &data_root, &state_dir, 1_800_003_600);
    assert_eq!(
        json["size"],
        serde_json::json!({"kind": "known", "files": 0, "bytes": 0})
    );
    assert_eq!(
        json["last_sync"],
        serde_json::json!({"kind": "known", "unix": 1_800_000_000})
    );
    assert_eq!(
        json["snapshots_behind"],
        serde_json::json!({"kind": "known", "count": 3})
    );
    assert_eq!(
        json["orphans"],
        serde_json::json!({"kind": "known", "files": 1, "bytes": 1_234})
    );
    assert_eq!(json["rows"][0]["severity"], "info");
    assert_eq!(json["rows"][0]["sentence"], serde_json::json!(row));
}

/// A progress record written before orphan accounting existed parses (the
/// field is additive) and reads as what it is — an unmeasured count,
/// unknown, never a zero — while every other field stays measured.
#[test]
fn a_progress_record_from_before_orphan_accounting_reads_orphans_as_unknown() {
    let sb = tempfile::TempDir::new().unwrap();
    let state_dir = sb.path().join("state");
    let data_root = sb.path().join("data");
    fs::create_dir_all(&state_dir).unwrap();

    write_state_record(
        &state_dir,
        &mirrorstate::MirrorState {
            version: mirrorstate::MIRROR_STATE_VERSION,
            finished_at_unix: 1_800_000_000,
            mirror_of: "storagebox".to_string(),
            closure_complete: BTreeSet::new(),
            snapshots_on_remote: 0,
            remote_total_bytes: 0,
        },
    );
    // The pre-LM-4 shape: no orphan fields at all.
    fs::write(
        mirror::mirror_progress_path(&state_dir),
        r#"{
  "version": 1,
  "mirror_of": "storagebox",
  "round_finished_at_unix": 1800000000,
  "complete": true,
  "outcome": "complete",
  "remote_total_bytes": 4096,
  "mirror_bytes": 4096,
  "remote_files": 9,
  "mirror_files": 9
}"#,
    )
    .unwrap();

    let config = mirror_config("enabled = true\nmirror_of = \"storagebox\"");
    let status = mirror_status(&config, &data_root, &state_dir, 1_800_000_000);
    assert!(matches!(
        &status.orphans,
        mirror::MirrorUsageState::Unknown { why } if why.contains("did not measure")
    ));
    let row = &status.rows[0].sentence;
    assert!(row.contains("orphans unknown"), "{row}");
    assert!(row.contains("0 snapshot(s) behind"), "{row}");
}

/// Orphan files a remote cleanup leaves behind appear in the next round's
/// own records and in the steady row's count, and the directory's size
/// includes them: they are bytes on this machine whether or not anything
/// references them (ADR-062 D2 / 47 §4 C).
#[test]
fn orphan_files_surface_in_the_records_the_row_and_the_size() {
    let rig = build_rig(None);
    let (report1, outcome) = run_round(&rig, &NeverStops);
    assert!(matches!(outcome, mirror::SyncOutcome::Completed));
    assert_eq!(
        report1.orphan_files,
        Some(0),
        "the first fill copies only listed files"
    );

    // A remote cleanup happened between rounds: this file is in the mirror
    // but no listing will name it.
    let orphan_dir = rig.mirror_root().join("data").join("zz");
    fs::create_dir_all(&orphan_dir).unwrap();
    let orphan_bytes = 4096_u64;
    fs::write(
        orphan_dir.join("deadbeef.no-longer-remote"),
        vec![0_u8; orphan_bytes as usize],
    )
    .unwrap();

    let (report2, outcome) = run_round(&rig, &NeverStops);
    assert!(matches!(outcome, mirror::SyncOutcome::Completed));
    assert_eq!(
        report2.orphan_files,
        Some(1),
        "the round's listing sees the orphan"
    );
    assert_eq!(report2.orphan_bytes, Some(orphan_bytes));
    assert_eq!(
        report2.downloaded_files, 0,
        "an orphan is never re-downloaded or deleted"
    );

    match mirror::load_progress(&rig.state_dir) {
        MirrorProgressRead::Present(progress) => {
            assert_eq!(progress.orphan_files, Some(1));
            assert_eq!(progress.orphan_bytes, Some(orphan_bytes));
        }
        other => panic!("the up-to-date round rewrote progress, got {other:?}"),
    }

    let config = mirror_config("enabled = true\nmirror_of = \"storagebox\"");
    let status = mirror_status(&config, &rig.data_root, &rig.state_dir, 1_800_000_000);
    assert!(matches!(
        &status.orphans,
        mirror::MirrorUsageState::Known { files: 1, bytes } if *bytes == orphan_bytes
    ));
    // The size counts what deleting the directory would free: the copy
    // plus the orphan.
    assert!(matches!(
        &status.size,
        mirror::MirrorUsageState::Known { bytes, .. } if *bytes == report2.remote_total_bytes + orphan_bytes
    ));
    let row = &status.rows[0].sentence;
    assert!(
        row.contains("1 orphan file(s) (4.0 KiB reclaimable)"),
        "{row}"
    );
}

/// The completed-fill-with-no-state-record state the engine itself never
/// writes: the row keeps its measured halves (size, orphans) and words the
/// missing coverage honestly, in yellow — never info fading a state that
/// holds every reclaim back (D6.2's direction).
#[test]
fn a_missing_closure_record_after_a_completed_fill_makes_a_yellow_row() {
    let rig = build_rig(None);
    run_round(&rig, &NeverStops);
    fs::remove_file(mirrorstate::mirror_state_path(&rig.state_dir)).unwrap();

    let config = mirror_config("enabled = true\nmirror_of = \"storagebox\"");
    let status = mirror_status(&config, &rig.data_root, &rig.state_dir, 1_800_000_000);
    assert_eq!(status.rows.len(), 1);
    let row = &status.rows[0];
    assert_eq!(row.severity, mirror::MirrorRowSeverity::Yellow);
    assert!(
        row.sentence
            .contains("last sync unknown (no sync round has completed yet)"),
        "{}",
        row.sentence
    );
    assert!(
        row.sentence
            .contains("snapshots behind unknown (no sync round has completed yet)"),
        "{}",
        row.sentence
    );
    assert!(
        row.sentence.contains("file(s)"),
        "the measured halves stay: {}",
        row.sentence
    );
    // The field family on the wire says the same thing.
    let json = status_json(&config, &rig.data_root, &rig.state_dir, 1_800_000_000);
    assert_eq!(json["last_sync"]["kind"], "unknown");
    assert_eq!(json["snapshots_behind"]["kind"], "unknown");
    assert_eq!(json["rows"][0]["severity"], "yellow");
}

/// The reclaim-leg spellings LM-4 settles (ADR-062 D6.3, ruling 8): the
/// `--json` value the ruling fixed and the human phrase the ADR quotes,
/// pinned so the reclaim-side wiring cannot grow a second spelling.
#[test]
fn the_held_back_spellings_are_settled() {
    assert_eq!(mirror::HELD_BACK_KIND, "not-mirrored");
    assert_eq!(
        mirror::HELD_BACK_SENTENCE,
        "the mirror has not synced to this snapshot yet"
    );
}

#[test]
fn a_repointed_mirror_of_expires_the_old_record_not_the_new_machine() {
    let rig = build_rig(None);
    run_round(&rig, &NeverStops);

    let config = mirror_config("enabled = true\nmirror_of = \"storagebox-v2\"");
    let lines = mirror_status_lines(&config, &rig.data_root, &rig.state_dir);
    let joined = lines.join("\n");
    assert!(
        joined.contains(
            "mirror-state.json names `storagebox`, but this machine \
                         mirrors `storagebox-v2`"
        ),
        "the stale record is stated as stale: {joined}"
    );
    assert!(
        joined.contains("reclaim must hold sessions back"),
        "{joined}"
    );
    let json = status_json(&config, &rig.data_root, &rig.state_dir, 1_800_000_000);
    assert_eq!(json["closure_record"]["kind"], "unknown");
    let why = json["closure_record"]["why"].as_str().unwrap_or_default();
    assert!(
        why.contains("this machine mirrors `storagebox-v2`"),
        "{why}"
    );

    // The read interface agrees: membership alone grants nothing when the
    // record names another destination.
    let recorded = match mirrorstate::load(&rig.state_dir) {
        MirrorStateRead::Present(record) => record,
        other => panic!("the first fill completed, got {other:?}"),
    };
    let covered_id = rig.machine_of("machine-a");
    assert!(mirrorstate::snapshot_covered(
        &MirrorStateRead::Present(recorded.clone()),
        "storagebox",
        &covered_id
    ));
    assert!(
        !mirrorstate::snapshot_covered(
            &MirrorStateRead::Present(recorded),
            "storagebox-v2",
            &covered_id
        ),
        "a record from the old mirror_of is not coverage for the new one"
    );
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
        ])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the second pass succeeds too: {all}");
    assert!(
        !all.contains("[reap] skipped"),
        "the fixture never passed --keep-ssh-masters, so nothing may print a \
         skip line: {all}"
    );
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

// ------------------------------------------------------- CLI exit codes

#[test]
fn mirror_sync_exits_1_when_the_source_proved_corrupt() {
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();
    cli_fixture_remote(sandbox);
    write_config(
        sandbox,
        "enabled = true\nmirror_of = \"storagebox\"\nmin_free_gib = 1.0",
    );

    // Corrupt one pack under its own name: the listing keeps claiming the
    // bytes, both downloads fail verification, and the round is a
    // checked-and-failed fact — exit 1, with the class's own sentence.
    let remote_root = sandbox.join("remote-repo");
    let mut pack_rel = String::new();
    for rel in inventory(&remote_root) {
        if rel.starts_with("data/") {
            pack_rel = rel;
            break;
        }
    }
    assert!(!pack_rel.is_empty());
    let pack_path = remote_root.join(&pack_rel);
    let mut pack = fs::read(&pack_path).unwrap();
    let mid = pack.len() / 2;
    pack[mid] ^= 0xff;
    fs::write(&pack_path, &pack).unwrap();

    let out = binary_command(sandbox)
        .args(["mirror", "sync", "--budget-secs", "120"])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "verified source corruption is the finished-and-failed class: {all}"
    );
    assert!(
        all.contains("exit_code=1 — the source proved corrupt"),
        "{all}"
    );
    // And the command never words it as a pause.
    assert!(
        !all.contains("not a failure"),
        "the corrupt-source exit 1 must not print pause wording: {all}"
    );

    // The durable note from the same failed round carries no repository path.
    let state_record = mirror_progress_record(sandbox);
    assert!(
        state_record.contains("\"outcome\": \"failed\""),
        "the record words the class: {state_record}"
    );
    assert!(
        !state_record.contains(sandbox.to_string_lossy().as_ref()),
        "state files carry no paths: {state_record}"
    );
}

/// The CLI-visible text of `mirror-progress.json`: the state dir is
/// `<XDG_DATA_HOME>/chat-stasher/state` (`collect::default_state_dir`), the
/// same root `mirror_root_of` has.
fn mirror_progress_record(sandbox: &Path) -> String {
    let candidate = sandbox
        .join("data")
        .join("chat-stasher")
        .join("state")
        .join("mirror-progress.json");
    if candidate.is_file() {
        fs::read_to_string(&candidate).unwrap()
    } else {
        String::new()
    }
}

#[test]
fn mirror_sync_reports_an_uninitialised_destination_as_usage() {
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();
    // A declared destination whose directory exists but has never been
    // pushed to: no repository there to mirror. The key file is planted so
    // the round reaches the listing and discovers the empty repository —
    // the usage refusa must be about the destination, not the key.
    fs::create_dir_all(sandbox.join("remote-repo")).unwrap();
    store::persist_key_file(
        &store_config(&sandbox.join("remote-repo"), &sandbox.join("remote.key")),
        &MasterKey::new(),
    )
    .unwrap();
    write_config(
        sandbox,
        "enabled = true\nmirror_of = \"storagebox\"\nmin_free_gib = 0.0",
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
        Some(2),
        "an uninitialised destination is a usage problem, nothing was done: {all}"
    );
    assert!(all.contains("has not been initialised"), "{all}");
    assert!(all.contains("exit_code=2"), "{all}");
    assert!(
        !mirror_root_of(sandbox).exists(),
        "the refused usage round creates no empty mirror directory: {all}"
    );
}

/// The two masterkey "nothing was done" refusals are usage (exit 2), not
/// a checked-and-failed read (exit 1): with the key missing or unreadable
/// the round never ran, and `docs/cli.md`'s exit-code contract puts
/// exactly that under 2 ("usage error … Nothing was read or written").
/// Invariant 2 makes exit 1 a report of a check that actually ran, so
/// answering 1 here would claim a verification that never happened. No
/// test pinned either branch before (rev2's finding, the key half of
/// rev0 issue 10's fix).
#[test]
fn mirror_sync_answers_a_missing_or_unreadable_masterkey_with_usage() {
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();
    cli_fixture_remote(sandbox);
    write_config(
        sandbox,
        "enabled = true\nmirror_of = \"storagebox\"\nmin_free_gib = 1.0",
    );

    // Missing: refused before the round reads or writes anything.
    fs::remove_file(sandbox.join("remote.key")).unwrap();
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
        Some(2),
        "a missing masterkey is usage (nothing was read or written): {all}"
    );
    assert!(all.contains("is not on this machine"), "{all}");
    assert!(all.contains("exit_code=2"), "{all}");
    assert!(
        !mirror_root_of(sandbox).exists(),
        "the refused round moves nothing: {all}"
    );

    // Unusable: a key file that exists but cannot be used — same class.
    fs::write(sandbox.join("remote.key"), "not a masterkey file").unwrap();
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
        Some(2),
        "an unusable masterkey is usage too (nothing was read or written): {all}"
    );
    assert!(all.contains("cannot be read"), "{all}");
    assert!(all.contains("exit_code=2"), "{all}");
    assert!(
        !mirror_root_of(sandbox).exists(),
        "the refused round moves nothing: {all}"
    );
}

#[test]
fn the_pass_reaps_the_ssh_masters_its_mirror_step_opened() {
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    // The pass pushes to the healthy local `storagebox`; the mirror names a
    // *second* declared destination with an ssh endpoint no server answers —
    // so the only ControlPersist-masters bookkeeping in this pass is the
    // mirror step's own, and the reap line it must print (issue 2) cannot
    // be confused with push's or verify's.
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "push_only_if_changed = false\n\n[destinations.storagebox]\nrepo = \
             \"{}\"\nkey_file = \"{}\"\nno_cache = true\n\n[destinations.backup2]\nrepo = \
             \"opendal:sftp\"\nkey_file = \"{}\"\n\n[destinations.backup2.options]\nendpoint = \
             \"ssh://127.0.0.1:1/none\"\n\n[mirror]\nenabled = true\nmirror_of = \
             \"backup2\"\nmin_free_gib = 1.0\n",
            sandbox.join("remote-repo").display(),
            sandbox.join("remote.key").display(),
            sandbox.join("backup2.key").display(),
        ),
    )
    .unwrap();
    // Both key files must exist and parse: `storagebox`'s for the pass's
    // own push, `backup2`'s for the mirror step to reach its remote read —
    // the goal is the mirror *round* running against the unreachable
    // endpoint, not the step refusing earlier at the key check.
    store::persist_key_file(
        &store_config(&sandbox.join("remote-repo"), &sandbox.join("remote.key")),
        &MasterKey::new(),
    )
    .unwrap();
    store::persist_key_file(
        &store_config(&sandbox.join("none"), &sandbox.join("backup2.key")),
        &MasterKey::new(),
    )
    .unwrap();

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
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "the pass itself succeeds: the mirror's unreachable destination is a \
         warning, not a failed pass\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("[run-once] result:") && stderr.contains("mirror-sync warning"),
        "the pass completed and the mirror step warned:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("[reap] host 127.0.0.1 · ssh masters shut down:"),
        "the mirror step must reap the ssh masters its remote reads opened, in \
         the same pass that opened them:\nstderr:\n{stderr}"
    );
}

/// The pass's hard-failure warning says what the rounds do — every round
/// re-checks the source — instead of claiming a hold no code keeps (the
/// rev2 finding: "stays stopped" described a stopped state nothing
/// records, while the step actually re-ran each hour), and once the
/// corrupt bytes are repaired under their own name, the next pass's
/// round sees the fix and completes with no manual un-hold: the retry is
/// the observation that *sees* the fix, never what heals it.
#[test]
fn the_pass_warns_saying_each_round_re_checks_and_heals_once_the_source_is_fixed() {
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    // The pass pushes healthy data to `storagebox`; the mirror names a
    // second *local* destination, `box2`, whose pushed repository gets
    // one pack corrupted under its own name — the corrupt-source shape
    // the mirror step hits on its first fill of `box2`, with a repo the
    // step can actually read (unlike the unreachable-endpoint reap
    // fixture).
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "push_only_if_changed = false\n\n[destinations.storagebox]\nrepo = \
             \"{}\"\nkey_file = \"{}\"\nno_cache = true\n\n[destinations.box2]\nrepo = \
             \"{}\"\nkey_file = \"{}\"\nno_cache = true\n\n[mirror]\nenabled = true\nmirror_of = \
             \"box2\"\nmin_free_gib = 1.0\n",
            sandbox.join("remote-repo").display(),
            sandbox.join("remote.key").display(),
            sandbox.join("box2-repo").display(),
            sandbox.join("box2.key").display(),
        ),
    )
    .unwrap();
    store::persist_key_file(
        &store_config(&sandbox.join("remote-repo"), &sandbox.join("remote.key")),
        &MasterKey::new(),
    )
    .unwrap();
    let box2 = store_config(&sandbox.join("box2-repo"), &sandbox.join("box2.key"));
    let mk2 = MasterKey::new();
    store::persist_key_file(&box2, &mk2).unwrap();
    let stage2 = sandbox.join("stage2");
    write_session(&stage2, "machine-b2", "session-b2", 2);
    BackupStore::new(box2.clone(), "machine-b2".to_string())
        .push(&stage2, &mk2)
        .unwrap();

    // Corrupt one box2 pack under its own name, keeping the true bytes,
    // so the repair can put exactly them back later.
    let box2_root = sandbox.join("box2-repo");
    let mut pack_rel = String::new();
    for rel in inventory(&box2_root) {
        if rel.starts_with("data/") {
            pack_rel = rel;
            break;
        }
    }
    assert!(!pack_rel.is_empty(), "the fixture repository has packs");
    let pack_path = box2_root.join(&pack_rel);
    let true_pack = fs::read(&pack_path).unwrap();
    let mut corrupt = true_pack.clone();
    let mid = corrupt.len() / 2;
    corrupt[mid] ^= 0xff;
    fs::write(&pack_path, &corrupt).unwrap();

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
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        out.status.success(),
        "the mirror's corrupt source is a warning, not a failed pass\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("[run-once] result:"),
        "the pass itself completed:\nstdout:\n{stdout}"
    );
    assert!(
        stderr.contains("the source proved corrupt"),
        "the warning names the hard class:\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("re-checks the source every round"),
        "the warning says what the rounds actually do — re-check, no hold:\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("stays stopped"),
        "no code holds the mirror stopped; the warning must not claim one:\nstderr:\n{stderr}"
    );

    // Repair the source under the same name and run the pass again: the
    // step re-ran on its own, re-checked, and this fill completes — a
    // held mirror could not have healed without a manual un-hold, and
    // there is none to call.
    fs::write(&pack_path, &true_pack).unwrap();
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
        ])
        .output()
        .unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "the second pass succeeds: {all}");
    assert!(
        all.contains("[run-once] mirror-sync: complete"),
        "a fixed source heals on the next round with no manual step: {all}"
    );
    assert!(
        !all.contains("the source proved corrupt"),
        "the healed round reports completion, not the old failure: {all}"
    );
}

// ------------------------------------------- LM-4 / ruling 8, end to end
// `doctor` (D13) shows the same rows `status --json` carries, from the same
// machine state, in one isolated envelope — the three-surface wording
// invariant ADR-062 ruling 8 states, pinned at the CLI boundary so no
// surface can reword the other two into drift.

#[test]
fn doctor_shows_the_rows_status_words_for_the_same_mirror() {
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();
    cli_fixture_remote(sandbox);
    write_config(
        sandbox,
        "enabled = true\nmirror_of = \"storagebox\"\nmin_free_gib = 1.0",
    );
    fs::write(
        sandbox.join("registry.json"),
        r#"{"schema_version":1,"generated":"T1 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    let registry = sandbox.join("registry.json");
    let with_registry = |command: &mut std::process::Command| {
        command.env("CHAT_STASHER_REGISTRY", &registry);
    };

    // The first fill through the real CLI, so the records on disk are the
    // product's own.
    let mut sync = binary_command(sandbox);
    with_registry(&mut sync);
    let out = sync
        .args(["mirror", "sync", "--budget-secs", "300"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the first fill completes\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let json_of = |args: &[&str]| -> serde_json::Value {
        let mut command = binary_command(sandbox);
        with_registry(&mut command);
        let out = command.args(args).output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        // `--json` prints one object on stdout; other lines (if any) are
        // prose the command's stderr-free path never adds here.
        let line = stdout
            .lines()
            .find(|line| line.starts_with('{'))
            .unwrap_or(&stdout);
        serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("parse {args:?} output as JSON: {error}\n{stdout}"))
    };

    let status = json_of(&["status", "--json"]);
    let mirror = &status["local"]["mirror"];
    assert_eq!(mirror["configured"], serde_json::json!(true));
    assert_eq!(mirror["first_sync"]["complete"], serde_json::json!(true));
    assert_eq!(mirror["size"]["kind"], serde_json::json!("known"));
    assert_eq!(mirror["orphans"]["kind"], serde_json::json!("known"));
    assert_eq!(mirror["rows"][0]["severity"], serde_json::json!("info"));
    let sentence = mirror["rows"][0]["sentence"]
        .as_str()
        .expect("the row text travels as one sentence")
        .to_string();
    assert!(
        sentence.contains("local copy of `storagebox`"),
        "the steady row words the copy: {sentence}"
    );
    assert!(
        sentence.contains("0 snapshot(s) behind"),
        "a completed closure is behind by nothing: {sentence}"
    );

    // Doctor's page shows the same section, and its `--json` carries the
    // same sentence — the wording cannot drift between the surfaces.
    let mut doctor = binary_command(sandbox);
    with_registry(&mut doctor);
    let out = doctor.args(["doctor"]).output().unwrap();
    let page = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        page.contains("D13 · Local mirror — size, last sync, snapshots behind, orphans (ADR-062)"),
        "the doctor page names the mirror section: {page}"
    );
    assert!(
        page.contains(&sentence),
        "doctor prints the same row status words:\nstatus: {sentence}\ndoctor:\n{page}"
    );

    let doctor_json = json_of(&["doctor", "--json"]);
    let check = &doctor_json["mirror"];
    assert_eq!(check["checked"], serde_json::json!(true));
    assert_eq!(
        check["rows"][0]["sentence"].as_str(),
        Some(sentence.as_str()),
        "ruling 8: one sentence on every surface"
    );
    assert_eq!(check["orphans"], mirror["orphans"]);
    assert_eq!(check["size"], mirror["size"]);
    assert_eq!(check["snapshots_behind"], mirror["snapshots_behind"]);
    assert_eq!(check["last_sync"], mirror["last_sync"]);
}
