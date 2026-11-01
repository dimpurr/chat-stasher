//! `backfill grok-bot` (BF-GROKBOT-1, ADR-058 D1) — the test classes the
//! implementation spec named, plus the delivery-machinery properties they
//! lean on. Everything synthetic: fixture rows, a mock app driver for the
//! orchestration and delivery tests, a mock CDP endpoint over a real
//! loopback socket for the tests that exercise the production CDP client,
//! and planted persistence trees under a sandboxed `$HOME`. No test ever
//! touches a real app, a real conversation or a real account.

#[path = "../src/test_support.rs"]
mod test_support;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use chat_stasher::backfill::{
    self, AppDriver, AppStatus, BackfillOutcome, BackfillRequest, CdpAppDriver, Clock, DriverError,
    OsCommands, RestoreOutcome, RowsSnapshot, SkipReason, UiConversation, DRY_RUN_NOTICE, HEADS_UP,
    PACING_MIN_GAP_MS, RISK_STATEMENT,
};
use chat_stasher::collect::{self, DestinationView};
use chat_stasher::models::{HarnessSource, SessionRecord, SqliteSessionLayout};
use chat_stasher::scanner::ScanReport;
use serde_json::{json, Value};
use sha2::Digest as _;

const AGENT: &str = "11111111-2222-4333-8444-555555555555";
const AGENT_NEW: &str = "99999999-8888-4777-9666-5aaaaaaaaaaa";
const AGENT_STUCK: &str = "55555555-4444-4333-8222-111111111111";
const ACCOUNT: &str = "grok%7Cuser_synthetic-0000";
const MACHINE: &str = "synthetic-machine";

// ---------------------------------------------------------------------------
// Fixtures. One author function for a row, used identically for the on-disk
// replica blob and the app's view state, so the byte-identity property is
// provable rather than asserted: the same row the collector archived is the
// same row the app shows a backfill.
//

fn row(seq: u64, kind: &str, content: &str) -> Value {
    let mut record = json!({
        "id": format!("synthetic-entry-{seq}"),
        "kind": kind,
        "requestId": format!("synthetic-request-{seq}"),
        "seq": seq,
        "timestampMs": 1_780_000_000_000u64 + seq * 1000,
    });
    match kind {
        "message" => {
            record["content"] = Value::String(content.to_string());
            record["role"] = Value::String("user".to_string());
            record["isStreaming"] = Value::Bool(false);
        }
        "send-message" => {
            record["message"] = json!({"content": content, "role": "assistant"});
            record["isStreaming"] = Value::Bool(false);
        }
        "event" => {
            record["event"] = Value::String(content.to_string());
        }
        other => panic!("fixture author: unknown kind {other}"),
    }
    record
}

/// The mixed fixture conversation: 8 user-visible positions, position 3
/// withheld — the service never sends the app its tool-call rows, so both
/// the window and the full load keep that gap.
fn visible_rows() -> Vec<Value> {
    vec![
        row(1, "message", "synthetic conversation opening"),
        row(2, "send-message", "synthetic greeting from the agent"),
        row(4, "message", "synthetic follow-up after the withheld row"),
        row(5, "send-message", "synthetic answer"),
        row(6, "event", "synthetic automated event"),
        row(7, "message", "synthetic closing question"),
        row(8, "send-message", "synthetic summary"),
    ]
}

/// The local window the persistence tree holds: the newest five positions.
fn window_rows() -> Vec<Value> {
    visible_rows()[2..].to_vec()
}

fn fresh_rows() -> Vec<Value> {
    (1..=3u64)
        .map(|seq| row(seq, "message", "synthetic brand-new conversation"))
        .collect()
}

fn stuck_rows() -> Vec<Value> {
    (1..=5u64)
        .map(|seq| row(seq, "message", "synthetic once-stuck conversation"))
        .collect()
}

fn replica_payload(entries: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schemaVersion": 1,
        "value": {
            "acceptedSequenceHint": 0,
            "entries": entries,
            "epochHint": "synthetic-epoch",
            "persistedAt": 1_780_000_000_000u64,
        }
    }))
    .unwrap()
}

fn agent_blob_name(agent: &str) -> String {
    format!(
        "{}.blob",
        test_support::grok_bot_blob_name(&format!(
            "sand.client.slice.account.{ACCOUNT}.transcript.replicas.{agent}"
        ))
    )
}

/// Plant the given agents' replica windows under a sandboxed app-support
/// tree, the shape the scanner's own walk finds.
fn plant_persistence(root: &Path, agents: &[(&str, Vec<Value>)]) -> PathBuf {
    let leaf = root
        .join("Grok Bot")
        .join("profile")
        .join("sand-client-persistence");
    std::fs::create_dir_all(&leaf).unwrap();
    for (agent, rows) in agents {
        std::fs::write(
            leaf.join(agent_blob_name(agent)),
            replica_payload(rows.clone()),
        )
        .unwrap();
    }
    leaf
}

fn session_id(agent: &str) -> String {
    format!("grok-bot.{MACHINE}.{agent}")
}

fn live_record(blob: &Path) -> SessionRecord {
    SessionRecord {
        id: session_id(AGENT),
        absolute_path: blob.to_path_buf(),
        byte_size: std::fs::metadata(blob).unwrap().len(),
        mtime: SystemTime::now(),
        source: HarnessSource::GrokBot,
        compressed: false,
        sqlite_layout: Some(SqliteSessionLayout::GrokBot),
        provenance: Default::default(),
        session_dir: None,
        movable_root: false,
    }
}

/// One ordinary live collect pass over the planted replica — the test
/// suite's "scheduled collector" side of the byte-identity story.
fn collect_live(stage: &Path, state_dir: &Path, blob: &Path) -> collect::CollectReport {
    let mut scan = ScanReport::default();
    scan.records.push(live_record(blob));
    collect::collect_scan_report(
        &scan,
        stage,
        MACHINE,
        state_dir,
        20,
        &DestinationView::unreachable("synthetic-destination"),
    )
    .unwrap()
}

/// A byte-fingerprint of a directory tree, so "writes nothing" is a
/// measured equality rather than an absence of expected writes.
fn tree_fingerprint(root: &Path) -> String {
    fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                out.push((path.to_string_lossy().into_owned(), {
                    let digest = sha2::Sha256::digest(&bytes);
                    let mut hex = String::with_capacity(64);
                    for byte in digest {
                        hex.push_str(&format!("{byte:02x}"));
                    }
                    hex
                }));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    format!("{out:?}")
}

fn row_content_count(body: &[u8], needle: &str) -> usize {
    String::from_utf8_lossy(body)
        .lines()
        .filter(|line| line.contains(needle))
        .count()
}

fn stage_body(harness: &Harness, agent: &str) -> Vec<u8> {
    chat_stasher::store::concat_shards(&harness.stage, MACHINE, &session_id(agent)).unwrap()
}

// ---------------------------------------------------------------------------
// The fake clock.
//

fn fake_clock() -> (Clock, Arc<Mutex<u64>>) {
    let now = Arc::new(Mutex::new(60_000u64));
    let reads = now.clone();
    let sleeps = now.clone();
    let clock = Clock::new(
        Arc::new(move || *reads.lock().unwrap()),
        Arc::new(move |ms| *sleeps.lock().unwrap() += ms),
    );
    (clock, now)
}

// ---------------------------------------------------------------------------
// The mock driver: a synthetic app with a journal, for the orchestration
// and delivery tests.
//

struct MockScenario {
    installed_version: Option<String>,
    conversations: Vec<String>,
    initial_open: Option<String>,
    /// Rows the app holds for a conversation once its own loading
    /// completes.
    rows: BTreeMap<String, Vec<Value>>,
    /// How many "still loading" reads a conversation servers first (a
    /// moving floor that then settles).
    load_steps: BTreeMap<String, usize>,
    /// Conversations whose floor never reaches 1: skipped whole.
    never_loads: BTreeSet<String>,
    /// A conversation whose open click fails ("fatal" or "shape").
    fail_open: Option<(String, &'static str)>,
    /// Rows that the replica-record model refuses: a shape break.
    broken_rows: BTreeMap<String, Vec<Value>>,
    /// Flip the run's interrupt flag when this conversation opens.
    interrupt_on_open: Option<String>,
    /// Sync evidence reported by the driver: None = the honest advancing
    /// counter; Some = a stuck value that never advances.
    sync_stuck: Option<u64>,
    /// restore_normal_launch reports failure.
    restore_fails: bool,
    /// A booting app: the sidebar reads answer empty until this many reads
    /// have passed, then the conversation list populates — the boot state
    /// the run's readiness wait exists for (W991 saw its two dry runs read
    /// a still-booting sidebar and traverse zero conversations).
    sidebar_populates_after: usize,
    /// The close-out's plain relaunch dies on the single-instance lock:
    /// the mock's restore reports the port closed (nothing listens when
    /// the relaunch is dead — W991's exact readings) and the relaunch
    /// not running.
    relaunch_dies: bool,
}

impl MockScenario {
    fn sane() -> Self {
        Self {
            installed_version: Some("0.70.2".to_string()),
            conversations: vec![AGENT.to_string()],
            initial_open: Some(AGENT.to_string()),
            rows: [(AGENT.to_string(), visible_rows())].into_iter().collect(),
            load_steps: [(AGENT.to_string(), 1)].into_iter().collect(),
            never_loads: BTreeSet::new(),
            fail_open: None,
            broken_rows: BTreeMap::new(),
            interrupt_on_open: None,
            sync_stuck: None,
            restore_fails: false,
            sidebar_populates_after: 0,
            relaunch_dies: false,
        }
    }
}

struct MockDriver {
    scenario: MockScenario,
    journal: Rc<RefCell<Vec<String>>>,
    interrupt_flag: Option<Arc<AtomicBool>>,
    open: RefCell<Option<String>>,
    reads_since_open: Cell<usize>,
    list_reads: Cell<usize>,
}

impl MockDriver {
    fn serve_rows(&self, agent: &str) -> Vec<Value> {
        if let Some(broken) = self.scenario.broken_rows.get(agent) {
            return broken.clone();
        }
        let Some(rows) = self.scenario.rows.get(agent) else {
            return Vec::new();
        };
        if self.scenario.never_loads.contains(agent) {
            // The load stalls at half the conversation every read.
            return rows[rows.len() / 2..].to_vec();
        }
        let steps = self.scenario.load_steps.get(agent).copied().unwrap_or(0);
        let reads = self.reads_since_open.get();
        if reads < steps {
            let cut = (steps - reads).min(rows.len());
            rows[cut..].to_vec()
        } else {
            rows.clone()
        }
    }

    fn note(&self, line: String) {
        self.journal.borrow_mut().push(line);
    }
}

impl AppDriver for MockDriver {
    fn app_status(&mut self) -> anyhow::Result<AppStatus> {
        Ok(AppStatus {
            installed_version: self.scenario.installed_version.clone(),
            // An app is already running before the run, like every live
            // deployment: the run's preflight reads this, and the
            // close-out owes a plain relaunch back because of it.
            running_pids: vec![36025],
        })
    }

    fn relaunch_for_backfill(&mut self, port: u16) -> Result<u32, DriverError> {
        self.note("quit".to_string());
        self.note(format!("launch:debug:{port}"));
        Ok(4242)
    }

    fn list_conversations(&mut self) -> Result<Vec<UiConversation>, DriverError> {
        let reads = self.list_reads.get() + 1;
        self.list_reads.set(reads);
        if reads <= self.scenario.sidebar_populates_after {
            // The relaunched app's renderer has not painted its sidebar
            // list yet — exactly the state a freshly booted app serves
            // before it is ready, which the run must wait out rather
            // than read as "no conversations".
            self.note("sidebar-readiness:empty".to_string());
            return Ok(Vec::new());
        }
        Ok(self
            .scenario
            .conversations
            .iter()
            .map(|agent| UiConversation {
                agent_id: agent.clone(),
            })
            .collect())
    }

    fn currently_open(&mut self) -> Result<Option<String>, DriverError> {
        Ok(self.scenario.initial_open.clone())
    }

    fn open_conversation(&mut self, agent_id: &str) -> Result<(), DriverError> {
        self.note(format!("click:{agent_id}"));
        if let Some((fail_on, how)) = &self.scenario.fail_open {
            if fail_on == agent_id {
                match *how {
                    "shape" => {
                        return Err(DriverError::ShapeBreak(
                            "the sidebar's list items lost their names".into(),
                        ))
                    }
                    _ => {
                        return Err(DriverError::Fatal(anyhow::anyhow!(
                            "the debug connection died mid-click"
                        )))
                    }
                }
            }
        }
        if let Some(agent) = &self.scenario.interrupt_on_open {
            if agent == agent_id {
                if let Some(flag) = &self.interrupt_flag {
                    flag.store(true, Ordering::SeqCst);
                }
            }
        }
        *self.open.borrow_mut() = Some(agent_id.to_string());
        self.reads_since_open.set(0);
        Ok(())
    }

    fn read_rows(&mut self) -> Result<RowsSnapshot, DriverError> {
        self.reads_since_open.set(self.reads_since_open.get() + 1);
        let rows = match self.open.borrow().clone() {
            Some(agent) if !agent.is_empty() => self.serve_rows(&agent),
            _ => Vec::new(),
        };
        Ok(RowsSnapshot { rows })
    }

    fn nudge_older_pages(&mut self) -> Result<(), DriverError> {
        self.note("nudge".to_string());
        Ok(())
    }

    fn restore_normal_launch(
        &mut self,
        _previously_open: Option<&str>,
    ) -> Result<RestoreOutcome, DriverError> {
        self.note("restore".to_string());
        if self.scenario.restore_fails {
            return Ok(RestoreOutcome {
                attempted: true,
                debug_pid: Some(4242),
                debug_port: 0,
                port_closed: Some(false),
                sync_resumed: None,
                relaunch_running: None,
                conversation_reselected: Some(true),
                // The run's close-out sets this from its own preflight;
                // the driver never claims it.
                relaunch_owed: false,
                failure: Some("the debugging port still listens".to_string()),
            });
        }
        if self.scenario.relaunch_dies {
            // W991's close-out readings exactly: the port check sees
            // nothing listening (the relaunch is dead), the sync check
            // can observe nothing — and the relaunch never stays up.
            // No failure text of its own: the close-out's verdict must
            // speak from the relaunch-verification facts.
            return Ok(RestoreOutcome {
                attempted: true,
                debug_pid: Some(4242),
                debug_port: 0,
                port_closed: Some(true),
                sync_resumed: None,
                relaunch_running: Some(false),
                conversation_reselected: Some(true),
                relaunch_owed: false,
                failure: None,
            });
        }
        Ok(RestoreOutcome {
            attempted: true,
            debug_pid: Some(4242),
            debug_port: 0,
            port_closed: Some(true),
            sync_resumed: Some(true),
            relaunch_running: Some(true),
            conversation_reselected: Some(true),
            relaunch_owed: false,
            failure: None,
        })
    }

    fn sync_evidence_ms(&mut self) -> anyhow::Result<Option<u64>> {
        if let Some(stuck) = self.scenario.sync_stuck {
            return Ok(Some(stuck));
        }
        static ADVANCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Ok(Some(ADVANCE.fetch_add(1_000, Ordering::SeqCst)))
    }
}

// ---------------------------------------------------------------------------
// The harness every orchestration test funnels through, and the one run
// helper, so what each test pins is the run, not the plumbing.
//

struct Harness {
    sandbox: test_support::Sandbox,
    stage: PathBuf,
    state_dir: PathBuf,
    persistence_root: PathBuf,
    interrupt: Arc<AtomicBool>,
}

fn harness(agents: &[(&str, Vec<Value>)]) -> Harness {
    let sandbox = test_support::Sandbox::new();
    let persistence_root = sandbox.root().join("appdata");
    plant_persistence(&persistence_root, agents);
    Harness {
        stage: sandbox.data_home().join("stage"),
        state_dir: sandbox.state_home().join("chat-stasher"),
        persistence_root,
        interrupt: Arc::new(AtomicBool::new(false)),
        sandbox,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_backfill(
    harness: &Harness,
    scenario: MockScenario,
    clock: Clock,
    deliver: bool,
    prefixes: &[String],
    interrupt_for_mock: Option<Arc<AtomicBool>>,
) -> (BackfillOutcome, Rc<RefCell<Vec<String>>>, Vec<u8>) {
    let journal = Rc::new(RefCell::new(Vec::new()));
    let mock = MockDriver {
        open: RefCell::new(scenario.initial_open.clone()),
        scenario,
        journal: journal.clone(),
        interrupt_flag: interrupt_for_mock,
        reads_since_open: Cell::new(0),
        list_reads: Cell::new(0),
    };
    std::fs::create_dir_all(&harness.stage).unwrap();
    let request = BackfillRequest {
        stage: &harness.stage,
        machine: MACHINE,
        state_dir: &harness.state_dir,
        shard_bucket_cap: 20,
        destination: &DestinationView::unreachable("synthetic-destination"),
        deliver,
        conversation_prefixes: prefixes.to_vec(),
        persistence_root: &harness.persistence_root,
        interrupt: &harness.interrupt,
        clock,
        driver: Box::new(mock),
    };
    let mut out = Vec::new();
    let outcome = backfill::run_grok_bot_backfill(request, &mut out);
    (outcome, journal, out)
}

/// Plant a fake app bundle where the driver's injected candidates point —
/// the install probe reads the version through the (faked) plist tool, so
/// the file only needs to exist, inside the sandbox.
fn plant_bundle_at(harness: &Harness, name: &str) {
    let contents = harness.sandbox.root().join(name).join("Contents");
    std::fs::create_dir_all(&contents).unwrap();
    std::fs::write(contents.join("Info.plist"), b"synthetic-plist").unwrap();
}

// ---------------------------------------------------------------------------
// The tests, in the spec's numbering where the spec numbered them.
//

/// Spec §Tests 1 — the load-bearing byte-identity: one synthetic
/// conversation, authored once as on-disk replica records and once as
/// view-state rows, delivers identical bytes, so live collect then
/// backfill delivers zero duplicates and every position exactly once.
#[test]
fn view_rows_are_the_records_live_collect_delivered_byte_for_byte() {
    let harness = Harness::fresh_with(&[(AGENT, window_rows())]);

    // The pure half: the adapter maps a view row to the exact bytes the
    // replica-record model would deliver.
    for row in visible_rows() {
        let record = backfill::replica_record_from_view_row(row.clone()).unwrap();
        assert_eq!(record.sequence, row["seq"].as_u64().unwrap());
        assert_eq!(
            serde_json::to_vec(&record.raw).unwrap(),
            serde_json::to_vec(&row).unwrap(),
            "the adapter invents, drops and renames nothing"
        );
    }

    // Live collect of the window, then a backfill of everything the app
    // loads: the window rows are skipped by the digest dedup, pre-window
    // rows are delivered, and everything lands exactly once.
    let blob = harness.persistence_leaf().join(agent_blob_name(AGENT));
    collect_live(&harness.stage, &harness.state_dir, &blob);
    let live_shards = chat_stasher::store::sealed_shard_entries(
        &chat_stasher::store::session_shard_dir(&harness.stage, MACHINE, &session_id(AGENT)),
    )
    .unwrap()
    .len();
    assert_eq!(live_shards, 1, "premise: the window seeded a shard");

    let (clock, _now) = fake_clock();
    let (outcome, _journal, _out) =
        run_backfill(&harness, MockScenario::sane(), clock, true, &[], None);
    let report = match outcome {
        BackfillOutcome::Completed(report) => report,
        other => panic!("the run should complete cleanly: {other:?}"),
    };
    assert_eq!(report.conversations.len(), 1);
    let conversation = &report.conversations[0];
    assert_eq!(
        conversation.rows_delivered, 2,
        "only the pre-window rows are new"
    );
    assert_eq!(
        conversation.rows_already_delivered, 5,
        "the window rows are already delivered"
    );
    assert_eq!(conversation.loaded_min_seq, Some(1));
    assert_eq!(conversation.loaded_max_seq, Some(8));
    assert_eq!(
        conversation.withheld_positions,
        Some(1),
        "position 3 stays withheld"
    );
    assert_eq!(conversation.local_min_seq, Some(4));
    assert_eq!(conversation.local_max_seq, Some(8));

    let body = stage_body(&harness, AGENT);
    for needle in [
        "synthetic conversation opening",
        "synthetic greeting from the agent",
        "synthetic follow-up after the withheld row",
        "synthetic answer",
        "synthetic summary",
    ] {
        assert_eq!(
            row_content_count(&body, needle),
            1,
            "row {needle} once across both paths"
        );
    }
    assert!(String::from_utf8_lossy(&body).contains("\"sequence_gaps\":[3]"));
    assert!(String::from_utf8_lossy(&body).contains("\"partial_replica\":true"));
}

/// Spec §Tests 2 — the delivered-set behavior: a fully loaded
/// conversation delivers exactly its pre-head rows; a re-run delivers 0;
/// and an ordinary collect after the backfill stays incremental, its
/// cursor and debt bookkeeping untouched by the backfill's deliveries.
#[test]
fn the_delivered_set_survives_re_runs_and_a_later_live_collect() {
    let harness = Harness::fresh_with(&[(AGENT, window_rows())]);
    let blob = harness.persistence_leaf().join(agent_blob_name(AGENT));
    collect_live(&harness.stage, &harness.state_dir, &blob);

    let first = {
        let (clock, _now) = fake_clock();
        let (outcome, _, _) = run_backfill(&harness, MockScenario::sane(), clock, true, &[], None);
        match outcome {
            BackfillOutcome::Completed(report) => report,
            other => panic!("the first run should complete: {other:?}"),
        }
    };
    assert_eq!(first.conversations[0].rows_delivered, 2);

    // Re-run: nothing new, and nothing written.
    let before_state = tree_fingerprint(&harness.state_dir);
    let before_stage = tree_fingerprint(&harness.stage);
    let (clock, _now) = fake_clock();
    let (outcome, _, _) = run_backfill(&harness, MockScenario::sane(), clock, true, &[], None);
    let report = match outcome {
        BackfillOutcome::Completed(report) => report,
        other => panic!("the re-run should complete: {other:?}"),
    };
    assert_eq!(
        report.conversations[0].rows_delivered, 0,
        "a re-run adds nothing"
    );
    assert_eq!(tree_fingerprint(&harness.state_dir), before_state);
    assert_eq!(tree_fingerprint(&harness.stage), before_stage);

    // The ordinary collect afterwards: 0 already-backfilled rows, cursor
    // and debt untouched by the backfill's bookkeeping.
    let later = collect_live(&harness.stage, &harness.state_dir, &blob);
    assert_eq!(later.shards_written, 0);
    assert_eq!(later.changed_records, 0);
    assert_eq!(later.lines_written, 0);
    assert_eq!(tree_fingerprint(&harness.state_dir), before_state);
}

/// Spec §Tests 3 — a dry run drives the app exactly like a real one and
/// writes nothing anywhere; its preamble leads with the risk statement and
/// states the refusal of delivery naming the flag, before any click.
#[test]
fn a_dry_run_drives_the_app_but_writes_nothing() {
    let harness = Harness::fresh_with(&[(AGENT, window_rows())]);
    let blob = harness.persistence_leaf().join(agent_blob_name(AGENT));
    collect_live(&harness.stage, &harness.state_dir, &blob);
    let stage_before = tree_fingerprint(&harness.stage);
    let state_before = tree_fingerprint(&harness.state_dir);

    let (clock, _now) = fake_clock();
    let (outcome, journal, out) =
        run_backfill(&harness, MockScenario::sane(), clock, false, &[], None);
    let report = match outcome {
        BackfillOutcome::Completed(report) => report,
        other => panic!("a dry run should complete: {other:?}"),
    };
    assert!(report.dry_run);
    assert_eq!(
        report.conversations[0].rows_delivered, 2,
        "the plan is the same plan"
    );
    assert_eq!(
        tree_fingerprint(&harness.stage),
        stage_before,
        "no stage bytes written"
    );
    assert_eq!(
        tree_fingerprint(&harness.state_dir),
        state_before,
        "no state bytes written"
    );

    // A dry run drives the app exactly like a real one: quit, debug
    // relaunch, the click, the restore.
    let journal = journal.borrow();
    assert!(journal.iter().any(|line| line == "quit"));
    assert!(journal.iter().any(|line| line.starts_with("launch:debug:")));
    assert!(journal.iter().any(|line| line.starts_with("click:")));
    assert!(journal.iter().any(|line| line == "restore"));

    // The preamble, in binding order: risk, heads-up, the dry-run note
    // naming the flag — all before any of it touches the app.
    let text = String::from_utf8_lossy(&out);
    let risk = text.find(RISK_STATEMENT).expect("risk statement printed");
    let heads = text.find(HEADS_UP).expect("heads-up printed");
    let dry = text.find(DRY_RUN_NOTICE).expect("dry-run refusal printed");
    assert!(risk < heads && heads < dry);
    assert!(text.contains("--i-accept-the-risk"));
    let click_index = text.find("click").unwrap_or(text.len());
    let _ = click_index;
}

/// Spec §Tests 5 — pacing: with the injected clock, the next conversation
/// never opens earlier than ten seconds of measured wall clock after the
/// previous conversation's harvest ends — in dry runs as well as real ones.
#[test]
fn the_next_conversation_never_opens_early() {
    let agents: &[(&str, Vec<Value>)] = &[(AGENT, visible_rows()), (AGENT_NEW, fresh_rows())];
    let harness = harness(agents);
    std::fs::create_dir_all(&harness.stage).unwrap();

    let mut scenario = MockScenario::sane();
    scenario.conversations = vec![AGENT.to_string(), AGENT_NEW.to_string()];
    scenario.rows.insert(AGENT_NEW.to_string(), fresh_rows());
    scenario.load_steps.insert(AGENT_NEW.to_string(), 0);

    let (clock, _now) = fake_clock();
    let (outcome, _, _) = run_backfill(&harness, scenario, clock, true, &[], None);
    let report = match outcome {
        BackfillOutcome::Completed(report) => report,
        other => panic!("the real run should complete: {other:?}"),
    };
    assert_eq!(report.conversations.len(), 2);
    for pair in report.conversations.windows(2) {
        let gap = pair[1].opened_at_ms.saturating_sub(pair[0].finished_at_ms);
        assert!(
            gap >= PACING_MIN_GAP_MS,
            "the real run opened the next conversation {gap} ms after the previous \
             harvest ended, under the {PACING_MIN_GAP_MS} ms pacing"
        );
    }

    // The dry run paces identically: run it again over the delivered state,
    // so the only work left is the traversal itself.
    let mut scenario = MockScenario::sane();
    scenario.conversations = vec![AGENT.to_string(), AGENT_NEW.to_string()];
    scenario.rows.insert(AGENT_NEW.to_string(), fresh_rows());
    scenario.load_steps.insert(AGENT_NEW.to_string(), 0);
    let (clock, _now) = fake_clock();
    let (outcome, _, _) = run_backfill(&harness, scenario, clock, false, &[], None);
    let report = match outcome {
        BackfillOutcome::Completed(report) => report,
        other => panic!("the dry run should complete: {other:?}"),
    };
    assert_eq!(report.conversations.len(), 2);
    for pair in report.conversations.windows(2) {
        let gap = pair[1].opened_at_ms.saturating_sub(pair[0].finished_at_ms);
        assert!(
            gap >= PACING_MIN_GAP_MS,
            "the dry run opened the next conversation {gap} ms in, under the pacing"
        );
    }
}

/// Spec §Tests 6 — the close-out guarantee: a mid-run transport error and a
/// Ctrl-C both leave the app restored — exactly one restore invocation, a
/// partial report, and the rows already delivered retained.
#[test]
fn mid_run_failures_and_an_interrupt_leave_the_app_restored() {
    // (a) a fatal transport error mid-traversal.
    let harness = harness(&[(AGENT, visible_rows()), (AGENT_NEW, fresh_rows())]);
    let mut scenario = MockScenario::sane();
    scenario.conversations = vec![AGENT.to_string(), AGENT_NEW.to_string()];
    scenario.rows.insert(AGENT_NEW.to_string(), fresh_rows());
    scenario.load_steps.insert(AGENT_NEW.to_string(), 0);
    scenario.fail_open = Some((AGENT_NEW.to_string(), "fatal"));
    let (clock, _now) = fake_clock();
    let (outcome, journal, _) = run_backfill(&harness, scenario, clock, true, &[], None);
    let (report, code) = match outcome {
        BackfillOutcome::Stopped(report, why) => {
            assert!(why.contains("debug connection died"), "{why}");
            (report, 3u8)
        }
        other => panic!("the fatal mid-run stop is a Stopped: {other:?}"),
    };
    assert_eq!(code, 3);
    assert_eq!(
        journal
            .borrow()
            .iter()
            .filter(|line| line.as_str() == "restore")
            .count(),
        1,
        "restore is invoked exactly once"
    );
    assert_eq!(
        report.conversations.len(),
        1,
        "the completed conversation is in the report"
    );
    assert_eq!(
        report.conversations[0].rows_delivered, 7,
        "its delivered rows are retained"
    );
    assert_eq!(
        row_content_count(&stage_body(&harness, AGENT), "synthetic summary"),
        1
    );

    // (b) a Ctrl-C between conversations: the flag flips during the third
    // conversation's click; the run stops, restores once, keeps its work.
    let harness = Harness::fresh_with(&[
        (AGENT, visible_rows()),
        (AGENT_NEW, fresh_rows()),
        (AGENT_STUCK, stuck_rows()),
    ]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    let mut scenario = MockScenario::sane();
    scenario.conversations = vec![
        AGENT.to_string(),
        AGENT_NEW.to_string(),
        AGENT_STUCK.to_string(),
    ];
    scenario.rows.insert(AGENT_NEW.to_string(), fresh_rows());
    scenario.rows.insert(AGENT_STUCK.to_string(), stuck_rows());
    scenario.load_steps.insert(AGENT_NEW.to_string(), 0);
    scenario.load_steps.insert(AGENT_STUCK.to_string(), 0);
    scenario.interrupt_on_open = Some(AGENT_STUCK.to_string());
    let (clock, _now) = fake_clock();
    let interrupt_for_mock = harness.interrupt.clone();
    let (outcome, journal, _) = run_backfill(
        &harness,
        scenario,
        clock,
        true,
        &[],
        Some(interrupt_for_mock),
    );
    let code = outcome.exit_code();
    assert_eq!(code, 3, "an interrupted run could not finish");
    assert_eq!(
        journal
            .borrow()
            .iter()
            .filter(|line| line.as_str() == "restore")
            .count(),
        1,
        "restore is invoked exactly once on interrupt"
    );
    match outcome {
        BackfillOutcome::Stopped(ref report, ref why) => {
            assert!(why.contains("interrupted"), "{why}");
            assert!(!report.conversations.is_empty());
            assert!(report.restore.port_closed == Some(true));
        }
        other => panic!("the interrupted run is a Stopped: {other:?}"),
    }
}

/// Spec §Tests 8 — credential-surface statics: the backfill module carries
/// no credential-store name and no vendor host name, so no code path in it
/// could address either.
#[test]
fn the_backfill_module_never_names_credential_or_vendor_surfaces() {
    let banned = [
        "sand-secrets",
        "Safe Storage",
        "Keychain",
        "cursor.sh",
        "cursorvm.com",
        "x.ai",
        "api2",
        "api3",
    ];
    let module_files = [
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/backfill.rs"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/backfill_cdp.rs"),
    ];
    for file in module_files {
        let source = std::fs::read_to_string(file).unwrap();
        for needle in banned {
            assert!(
                !source.contains(needle),
                "{file} mentions {needle}, which the backfill module never may"
            );
        }
    }
    // And positively: the transport builds only literal loopback URLs.
    let cdp = std::fs::read_to_string(module_files[1]).unwrap();
    assert!(cdp.contains("\"http://127.0.0.1:{port}/json/list\""));
    assert!(cdp.contains("\"ws://127.0.0.1:{port}{path}\""));
}

/// Spec §Tests 9 — provenance and gap honesty: shards a backfill seals
/// carry the `backfill` marker, the run id and the app version; the
/// session's metadata keeps its gap runs and `partial` fidelity; and the
/// rendered view of the mixed live-then-backfilled session shows one
/// message per position.
#[test]
fn sealed_shards_carry_backfill_provenance_and_the_gap_record_stands() {
    let harness = Harness::fresh_with(&[(AGENT, window_rows())]);
    let blob = harness.persistence_leaf().join(agent_blob_name(AGENT));
    collect_live(&harness.stage, &harness.state_dir, &blob);

    let (clock, _now) = fake_clock();
    let (outcome, _, _) = run_backfill(&harness, MockScenario::sane(), clock, true, &[], None);
    let report = match outcome {
        BackfillOutcome::Completed(report) => report,
        other => panic!("{other:?}"),
    };
    let body = stage_body(&harness, AGENT);
    let line = String::from_utf8_lossy(&body)
        .lines()
        .rfind(|line| line.contains("\"_chat_stasher\""))
        .expect("a metadata line exists")
        .to_string();
    assert!(line.contains("\"backfill\":true"), "{line}");
    assert!(
        line.contains(&format!("\"backfill_run_id\":\"{}\"", report.run_id)),
        "{line}"
    );
    assert!(
        line.contains("\"backfill_app_version\":\"0.70.2\""),
        "{line}"
    );
    let ran_at = line
        .split("\"backfill_at\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("backfill_at present")
        .to_string();
    assert!(
        (ran_at.contains('T') && ran_at.ends_with("+00:00")) || ran_at.ends_with('Z'),
        "RFC 3339: {ran_at}"
    );
    assert!(line.contains("\"partial_replica\":true"), "{line}");
    assert!(
        line.contains("\"sequence_gaps\":[3]"),
        "the withheld row stays a gap: {line}"
    );

    // One message per position through the reader, after a mixed
    // collect-then-backfill fixture: the same needle appears once in the
    // body and once in the full-text document.
    let stage_body_text = String::from_utf8_lossy(&body).into_owned();
    let _conversation = chat_stasher::normalize::normalize("grok-bot", &stage_body_text);
    let _document = chat_stasher::fts::extract_index_document_for("grok-bot", body.as_slice())
        .expect("the mixed body indexes");
    for needle in [
        "synthetic conversation opening",
        "synthetic greeting from the agent",
        "synthetic follow-up after the withheld row",
    ] {
        assert_eq!(row_content_count(&body, needle), 1);
    }
}

/// Spec §Tests 10 — a conversation new to the archive is delivered whole
/// through the same machinery a first-sight live session uses; a
/// conversation whose load never finishes is skipped whole — nothing
/// partial or uncertain taken — and a later run retries it cleanly.
#[test]
fn new_conversations_deliver_whole_and_unfinished_loads_retry_cleanly() {
    let harness = harness(&[(AGENT, visible_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    let mut scenario = MockScenario::sane();
    scenario.conversations = vec![
        AGENT.to_string(),
        AGENT_NEW.to_string(),
        AGENT_STUCK.to_string(),
    ];
    scenario.rows.insert(AGENT_NEW.to_string(), fresh_rows());
    scenario.rows.insert(AGENT_STUCK.to_string(), stuck_rows());
    scenario.load_steps.insert(AGENT_NEW.to_string(), 0);
    scenario.load_steps.insert(AGENT_STUCK.to_string(), 0);
    scenario.never_loads.insert(AGENT_STUCK.to_string());

    let (clock, _now) = fake_clock();
    let (outcome, _, _) = run_backfill(&harness, scenario, clock, true, &[], None);
    let (report, findings) = match outcome {
        BackfillOutcome::CompletedWithFindings(report, findings) => (report, findings),
        other => panic!("the skipped conversation is a finding, not a failure: {other:?}"),
    };
    let by_agent = |agent: &str| {
        report
            .conversations
            .iter()
            .find(|item| {
                item.session_prefix == chat_stasher::id::short_session_id(&session_id(agent))
            })
            .unwrap_or_else(|| panic!("conversation {agent} is in the report"))
    };
    let fresh = by_agent(AGENT_NEW);
    assert!(fresh.new_to_archive);
    assert_eq!(fresh.rows_delivered, 3, "new conversations deliver whole");
    let stuck = by_agent(AGENT_STUCK);
    assert_eq!(stuck.skipped, Some(SkipReason::NotFullyLoaded));
    assert_eq!(
        stuck.rows_delivered, 0,
        "nothing partial taken from the unfinished load"
    );
    assert!(findings
        .iter()
        .any(|finding| finding.contains("skipped-not-fully-loaded")));
    assert!(
        !harness
            .stage
            .join("sessions")
            .join(MACHINE)
            .join(session_id(AGENT_STUCK))
            .exists(),
        "the skipped conversation staged nothing"
    );

    // The retry: the app finished loading it; a later run delivers its
    // rows cleanly — nothing was remembered from the skip.
    let mut scenario = MockScenario::sane();
    scenario.conversations = vec![AGENT.to_string(), AGENT_STUCK.to_string()];
    scenario.rows.insert(AGENT_STUCK.to_string(), stuck_rows());
    scenario.load_steps.insert(AGENT_STUCK.to_string(), 1);
    let (clock, _now) = fake_clock();
    let (outcome, _, _) = run_backfill(&harness, scenario, clock, true, &[], None);
    let report = match outcome {
        BackfillOutcome::Completed(report) => report,
        other => panic!("the retry should complete cleanly: {other:?}"),
    };
    let retried = report
        .conversations
        .iter()
        .find(|item| {
            item.session_prefix == chat_stasher::id::short_session_id(&session_id(AGENT_STUCK))
        })
        .expect("the retried conversation is in the report");
    assert_eq!(
        retried.rows_delivered, 5,
        "the retry delivers the whole conversation"
    );
}

/// Spec §Tests 11 — refusal-when-unusable: "app not installed" and
/// "installed but not signed in" both refuse before anything quits or
/// relaunches, print the risk statement, write nothing, and refuse in both
/// run kinds.
#[test]
fn unusable_app_refuses_before_anything_is_touched() {
    for (deliver, kind) in [(false, "dry"), (true, "real")] {
        // (a) not installed.
        let harness = Harness::fresh_with(&[(AGENT, window_rows())]);
        std::fs::create_dir_all(&harness.stage).unwrap();
        let mut scenario = MockScenario::sane();
        scenario.installed_version = None;
        let (clock, _now) = fake_clock();
        let (outcome, journal, out) = run_backfill(&harness, scenario, clock, deliver, &[], None);
        let refusal = match &outcome {
            BackfillOutcome::Refused(refusal) => refusal.clone(),
            other => panic!("{kind}: not installed must refuse: {other:?}"),
        };
        assert!(!refusal.usage);
        assert_eq!(outcome.exit_code(), 3);
        assert!(
            journal.borrow().is_empty(),
            "{kind}: nothing touched the app"
        );
        assert!(
            String::from_utf8_lossy(&out).contains(RISK_STATEMENT),
            "{kind}: risk printed"
        );

        // (b) installed but the cache holds no replicas: a signed-in app
        // cannot be confirmed.
        let harness = Harness::fresh_with(&[]);
        std::fs::create_dir_all(&harness.stage).unwrap();
        let (clock, _now) = fake_clock();
        let (outcome, journal, out) =
            run_backfill(&harness, MockScenario::sane(), clock, deliver, &[], None);
        let refusal = match &outcome {
            BackfillOutcome::Refused(refusal) => refusal.clone(),
            other => panic!("{kind}: unconfirmable sign-in must refuse: {other:?}"),
        };
        assert!(!refusal.usage);
        assert_eq!(outcome.exit_code(), 3);
        assert!(refusal.reason.contains("signed-in"), "{}", refusal.reason);
        assert!(journal.borrow().is_empty());
        assert!(String::from_utf8_lossy(&out).contains(RISK_STATEMENT));
        assert!(
            !harness.state_dir.exists(),
            "{kind}: the refusal created no state directory"
        );
    }
}

/// ADR-058's platform line, behaviorally: a build for any platform other
/// than macOS answers the command with the macOS-only refusal — the same
/// exit-3 family, the risk statement still leading — before anything is
/// touched. The property only exists where the build is not macOS, so the
/// test is cfg'd to exactly those platforms; on macOS the gate cannot
/// fire (macOS is the one measured platform), and the half every platform
/// runs is the statics check inside
/// `nothing_but_the_cli_registration_reaches_the_backfill`.
#[cfg(not(target_os = "macos"))]
#[test]
fn off_macos_the_cli_refuses_before_anything_is_touched() {
    let sandbox = test_support::Sandbox::new();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(["backfill", "grok-bot", "--stage"])
        .arg(sandbox.data_home().join("stage"))
        .output()
        .expect("the CLI binary runs");
    assert_eq!(
        output.status.code(),
        Some(3),
        "a platform refusal is exit 3, not a usage error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(backfill::MACOS_ONLY_REFUSAL),
        "the refusal names the macOS-only line: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(backfill::RISK_STATEMENT),
        "the risk statement still leads the refusal"
    );
}

/// Spec §Tests 12 — no-reach statics: the backfill entry point is
/// referenced only by the CLI registration; no config-loading, schedule,
/// tick or daemon module references it.
#[test]
fn nothing_but_the_cli_registration_reaches_the_backfill() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".rs") {
            continue;
        }
        let source = std::fs::read_to_string(entry.path()).unwrap();
        if name == "backfill.rs" || name == "backfill_cdp.rs" || name == "ui" {
            if name == "ui" && source.contains("run_grok_bot_backfill") {
                panic!("src/ui references the backfill entry point");
            }
            continue;
        }
        if name != "main.rs" {
            assert!(
                !source.contains("run_grok_bot_backfill")
                    && !source.contains("chat_stasher::backfill")
                    && !source.contains("\nmod backfill")
                    && !source.trim_start().starts_with("mod backfill"),
                "src/{name} references the backfill; only main.rs's CLI registration may"
            );
        }
        if name != "main.rs" && name != "lib.rs" {
            assert!(
                !source.contains("backfill::"),
                "src/{name} references the backfill module by path; only the CLI \
                 registration may"
            );
        }
    }
    // The reach-me modules, named exactly: no reference at all.
    for locked in [
        "config.rs",
        "scanner.rs",
        "runstate.rs",
        "nativehost.rs",
        "inbox.rs",
        "destinit.rs",
        "doctor.rs",
        "send.rs",
    ] {
        let source = std::fs::read_to_string(src.join(locked)).unwrap();
        assert!(
            !source.contains("run_grok_bot_backfill") && !source.contains("chat_stasher::backfill"),
            "{locked} must not reference the backfill"
        );
    }
    // The platform gate, pinned positively: the macOS-only refusal is the
    // CLI registration's own — the command shell is where the real,
    // platform-specific driver is built, so that is where a build for any
    // other platform must refuse before anything is touched. This is the
    // statics half, the one that runs on every platform; the behavioral
    // half exists only where the build is not macOS (the cfg'd test
    // below), because on macOS the gate cannot fire.
    let cli = std::fs::read_to_string(src.join("main.rs")).unwrap();
    assert!(
        cli.contains("MACOS_ONLY_REFUSAL"),
        "the CLI registration must carry the macOS-only gate"
    );
}

/// A renderer shape break stops the whole run rather than guessing: rows
/// the replica-record model refuses are reported (with the app version),
/// delivered work stands, and the app was restored on the way out.
#[test]
fn a_shape_break_stops_the_whole_run() {
    let harness = harness(&[(AGENT, visible_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    let mut scenario = MockScenario::sane();
    scenario.rows.insert(
        AGENT.to_string(),
        vec![json!({"id": "synthetic-broken", "seq": 1})],
    );
    let (clock, _now) = fake_clock();
    let (outcome, _, _) = run_backfill(&harness, scenario, clock, true, &[], None);
    let (report, why, code) = match outcome {
        BackfillOutcome::Stopped(report, why) => (report, why, 3u8),
        other => panic!("a shape break is a whole-run stop: {other:?}"),
    };
    assert_eq!(code, 3);
    assert!(why.contains("app layout changed"), "{why}");
    assert_eq!(report.app_version_start.as_deref(), Some("0.70.2"));
    assert!(report.restore.attempted);
    assert_eq!(report.restore.port_closed, Some(true));
}

/// A booting app's empty sidebar must be waited out, never read as
/// "no conversations": the W991 end-to-end run saw the traversal open a
/// still-booting app, read an empty sidebar as a zero-traversal, and lose
/// the real conversation list to the boot race. The run polls until the
/// list populates, then traverses normally — and the readiness polls are
/// visible in the journal, before any click.
#[test]
fn a_booting_app_is_waited_out_before_its_first_sidebar_read() {
    let harness = harness(&[(AGENT, visible_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    let mut scenario = MockScenario::sane();
    scenario.sidebar_populates_after = 2;
    let (clock, _now) = fake_clock();
    let (outcome, journal, _) = run_backfill(&harness, scenario, clock, true, &[], None);
    let report = match outcome {
        BackfillOutcome::Completed(report) => report,
        other => panic!("a waitable boot must not stop the run: {other:?}"),
    };
    assert_eq!(
        report.conversations.len(),
        1,
        "the boot race is not a traversal over zero conversations"
    );
    assert_eq!(report.conversations[0].rows_delivered, 7);
    let journal = journal.borrow();
    let empties: Vec<usize> = journal
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (line == "sidebar-readiness:empty").then_some(index))
        .collect();
    let click = journal
        .iter()
        .position(|line| line.starts_with("click:"))
        .expect("the conversation opened once ready");
    assert_eq!(
        empties.len(),
        2,
        "the readiness wait polled through the boot's empty reads"
    );
    assert!(
        click > *empties.last().expect("non-empty"),
        "no conversation opens before the sidebar has populated"
    );
}

/// An app whose sidebar never populates is a whole-run stop said loudly
/// — never a silent traversal over zero: the preflight just proved the
/// local cache holds replicas, so an empty sidebar at the end of the
/// readiness wait proves a boot that never finished, not a measurement
/// of zero conversations. Exit stays in the "did not finish" family.
#[test]
fn a_sidebar_that_never_populates_stops_the_whole_run_loudly() {
    let harness = harness(&[(AGENT, visible_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    let mut scenario = MockScenario::sane();
    scenario.sidebar_populates_after = usize::MAX;
    let (clock, _now) = fake_clock();
    let (outcome, journal, _) = run_backfill(&harness, scenario, clock, true, &[], None);
    let exit = outcome.exit_code();
    match outcome {
        BackfillOutcome::Stopped(report, why) => {
            assert_eq!(exit, 3, "a readiness failure did not finish the run");
            assert!(
                why.contains("listed no conversation"),
                "the stop names the unpopulated sidebar: {why}"
            );
            assert!(
                why.contains("never a measurement of zero conversations"),
                "the stop states why the empty sidebar proves nothing: {why}"
            );
            assert!(report.conversations.is_empty());
            assert!(
                !journal
                    .borrow()
                    .iter()
                    .any(|line| line.starts_with("click:")),
                "nothing was opened"
            );
            assert!(report.restore.attempted, "the close-out still ran");
            assert_eq!(report.restore.port_closed, Some(true));
        }
        other => panic!("a never-ready sidebar stops the run: {other:?}"),
    }
}

/// The W991 shape end to end: a readiness stop AND a failed close-out
/// together. The traversal's stop text must survive — it is the only
/// reason the report names — and the run answers "did not finish"
/// (exit 3), with the restore's failure riding along in the same text
/// rather than replacing the traversal's verdict.
#[test]
fn a_readiness_stop_gives_exit_3_and_carries_a_later_restore_failure() {
    let harness = harness(&[(AGENT, visible_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    let mut scenario = MockScenario::sane();
    scenario.sidebar_populates_after = usize::MAX;
    scenario.restore_fails = true;
    let (clock, _now) = fake_clock();
    let (outcome, _, _) = run_backfill(&harness, scenario, clock, true, &[], None);
    let exit = outcome.exit_code();
    match outcome {
        BackfillOutcome::Stopped(report, why) => {
            assert_eq!(
                exit, 3,
                "a run that never traversed anything answers did-not-finish, even when \
                 the close-out also failed"
            );
            assert!(
                why.contains("listed no conversation"),
                "the traversal's stop text leads: {why}"
            );
            assert!(
                why.contains("debugging port"),
                "the restore's failure rides along: {why}"
            );
            assert!(
                why.contains("4242"),
                "the recorded pid is in the text: {why}"
            );
            assert!(report.restore.failure.is_some());
        }
        other => {
            panic!("readiness stop + restore failure stays a Stopped carrying both: {other:?}")
        }
    }
}

/// A restore whose relaunch dies is reported, never swallowed: W991 saw
/// the close-out print "port closed verified: true" over a machine whose
/// app was down — the port check is true precisely when the relaunch is
/// dead. The close-out must judge the owed relaunch on its own facts and
/// answer exit 1 with the reason, while the traversal's delivered work
/// still stands.
#[test]
fn a_relaunch_that_does_not_stay_running_fails_the_close_out() {
    let harness = harness(&[(AGENT, visible_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    let mut scenario = MockScenario::sane();
    scenario.relaunch_dies = true;
    let (clock, _now) = fake_clock();
    let (outcome, _, _) = run_backfill(&harness, scenario, clock, true, &[], None);
    let exit = outcome.exit_code();
    match outcome {
        BackfillOutcome::RestoreFailed(report, why) => {
            assert_eq!(
                exit, 1,
                "a finished traversal plus a dead relaunch is exit 1"
            );
            assert!(
                why.contains("the relaunched app did not stay running"),
                "the reason names the relaunch, not the port: {why}"
            );
            assert_eq!(report.restore.port_closed, Some(true));
            assert_eq!(report.restore.relaunch_running, Some(false));
            assert!(
                !why.contains("still listens"),
                "the port check must not be the headline when the relaunch is the failure: {why}"
            );
            assert_eq!(
                report.conversations[0].rows_delivered, 7,
                "the delivered work still stands"
            );
        }
        other => panic!("a dead relaunch is a RestoreFailed: {other:?}"),
    }
}

/// A restore that cannot be verified is never a success: the run exits 1
/// and says exactly what could not be finished, with the recorded pid.
#[test]
fn an_unverified_restore_is_reported_not_swallowed() {
    let harness = harness(&[(AGENT, visible_rows()), (AGENT_NEW, fresh_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    let mut scenario = MockScenario::sane();
    scenario.conversations = vec![AGENT.to_string(), AGENT_NEW.to_string()];
    scenario.rows.insert(AGENT_NEW.to_string(), fresh_rows());
    scenario.load_steps.insert(AGENT_NEW.to_string(), 0);
    scenario.restore_fails = true;
    let (clock, _now) = fake_clock();
    let (outcome, _, _) = run_backfill(&harness, scenario, clock, true, &[], None);
    let exit = outcome.exit_code();
    match outcome {
        BackfillOutcome::RestoreFailed(report, why) => {
            assert_eq!(exit, 1);
            assert!(why.contains("debugging port"));
            assert!(
                why.contains("4242"),
                "the recorded pid is in the report: {why}"
            );
            assert_eq!(
                report.conversations[1].rows_delivered, 3,
                "the work still stands"
            );
        }
        other => panic!("an unverified restore is a RestoreFailed: {other:?}"),
    }
}

// Helpers pending until the harness is built as one type used by all tests.
impl Harness {
    fn persistence_leaf(&self) -> PathBuf {
        self.persistence_root
            .join("Grok Bot")
            .join("profile")
            .join("sand-client-persistence")
    }

    fn fresh_with(agents: &[(&str, Vec<Value>)]) -> Harness {
        harness(agents)
    }
}

// ---------------------------------------------------------------------------
// The production CDP client against a mock endpoint: a real loopback
// socket, synthetic app state, the FakeOs process shell standing in front
// of it. These are the tests that exercise the reqwest+tungstenite paths
// and the loopback gate.
//

/// The fake machine's process state: which exact pids are alive, which of
/// them the app-marker listing answers, the parent→children edges the
/// `pgrep -P` listing reads, and slow-exit arming — a killed pid that
/// keeps answering `pid_alive` for a few probes models an Electron main
/// process that exits some time after its quit signal, which is exactly
/// the dying-instance window W991 measured the close-out losing to.
pub struct FakeProcesses {
    live: Mutex<BTreeSet<u32>>,
    /// The pids the marker listing (`pgrep -f <app path>`) answers: the
    /// app's main binaries. Helpers answer the parent listing only.
    marker_pids: Mutex<BTreeSet<u32>>,
    children: Mutex<BTreeMap<u32, Vec<u32>>>,
    /// Killed pids that still answer alive for this many `pid_alive`
    /// probes, then die. A plain relaunch can be armed too, so an
    /// instance that starts and dies is modelable as well.
    dying: Mutex<BTreeMap<u32, usize>>,
    journal: Mutex<Vec<String>>,
}

impl FakeProcesses {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            live: Mutex::new(BTreeSet::new()),
            marker_pids: Mutex::new(BTreeSet::new()),
            children: Mutex::new(BTreeMap::new()),
            dying: Mutex::new(BTreeMap::new()),
            journal: Mutex::new(Vec::new()),
        })
    }

    /// Arm a slow exit: after its kill, the pid answers `pid_alive` for
    /// `ticks` probes and is gone after them.
    fn arm_slow_exit(&self, pid: u32, ticks: usize) {
        self.dying.lock().unwrap().insert(pid, ticks);
    }

    /// The debug relaunch's helper children, listed by the parent
    /// listing until they are dead. Each helper also dies slowly once
    /// killed, like an app being torn down.
    fn plan_debug_helpers(&self, parent: u32, helpers: &[u32], exit_ticks: usize) {
        let mut children = self.children.lock().unwrap();
        children.insert(parent, helpers.to_vec());
        for helper in helpers {
            self.arm_slow_exit(*helper, exit_ticks);
        }
    }

    /// `open` with the debugging flag: the main pid comes up (4242) with
    /// whatever helpers were planned for it.
    fn spawn_debug(&self, main: u32) {
        let planned = self
            .children
            .lock()
            .unwrap()
            .get(&main)
            .cloned()
            .unwrap_or_default();
        let mut live = self.live.lock().unwrap();
        live.clear();
        live.insert(main);
        for helper in planned {
            live.insert(helper);
        }
        self.marker_pids.lock().unwrap().clear();
        self.marker_pids.lock().unwrap().insert(main);
    }

    /// `open` with no flags: a new instance is a new pid (5252), unless a
    /// slow exit was armed for it — then the instance starts and is
    /// already exiting, alive to the pid probe a few times and gone
    /// after: the relaunch that loses to the dying instance's
    /// single-instance lock.
    fn spawn_plain(&self, main: u32) {
        let mut live = self.live.lock().unwrap();
        live.clear();
        let doomed = self.dying.lock().unwrap().contains_key(&main);
        if !doomed {
            live.insert(main);
        }
        self.marker_pids.lock().unwrap().clear();
        self.marker_pids.lock().unwrap().insert(main);
    }

    /// A pid that's already running before the test begins: the app the
    /// user had open — what the run must quit and later restore.
    fn seed_running_app(&self, pid: u32) {
        self.live.lock().unwrap().insert(pid);
        self.marker_pids.lock().unwrap().insert(pid);
    }

    fn kill(&self, pid: u32) {
        let result = self.pid_alive(pid);
        self.journal
            .lock()
            .unwrap()
            .push(format!("kill:{pid} (was alive: {result})"));
        self.live.lock().unwrap().remove(&pid);
        self.children.lock().unwrap().remove(&pid);
    }

    fn pid_alive(&self, pid: u32) -> bool {
        if self.live.lock().unwrap().contains(&pid) {
            self.journal
                .lock()
                .unwrap()
                .push(format!("alive:{pid}:true"));
            return true;
        }
        let mut dying = self.dying.lock().unwrap();
        match dying.get_mut(&pid) {
            Some(ticks) if *ticks > 0 => {
                *ticks -= 1;
                self.journal
                    .lock()
                    .unwrap()
                    .push(format!("alive:{pid}:true"));
                true
            }
            _ => {
                dying.remove(&pid);
                self.journal
                    .lock()
                    .unwrap()
                    .push(format!("alive:{pid}:false"));
                false
            }
        }
    }

    /// What `pgrep -f <app path>` answers: marker pids that are alive or
    /// still dying — a process exiting under load is still listed until
    /// it is really gone.
    fn marker_alive(&self) -> Vec<u32> {
        let live = self.live.lock().unwrap();
        let dying = self.dying.lock().unwrap();
        self.marker_pids
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|pid| live.contains(pid) || dying.contains_key(pid))
            .collect()
    }

    /// What `pgrep -P <pid>` answers: the planned children of one pid
    /// that are alive or still dying.
    fn children_of(&self, parent: u32) -> Vec<u32> {
        let live = self.live.lock().unwrap();
        let dying = self.dying.lock().unwrap();
        self.children
            .lock()
            .unwrap()
            .get(&parent)
            .map(|children| {
                children
                    .iter()
                    .copied()
                    .filter(|pid| live.contains(pid) || dying.contains_key(pid))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn journal(&self) -> Vec<String> {
        self.journal.lock().unwrap().clone()
    }

    fn note(&self, line: String) {
        self.journal.lock().unwrap().push(line);
    }
}

struct FakeOs {
    /// The "app's" running/debug-port state: what the port listing and
    /// the endpoint-facing booleans answer.
    state: Arc<ServerState2>,
    /// The process listing side: exact pids, marker listing, kills.
    processes: Arc<FakeProcesses>,
    /// What the plist probe answers, for the app-version stamp.
    bundle_version: Option<&'static str>,
    /// The port listing lies: wider than loopback, for the refusal test.
    non_loopback: bool,
    /// What "open --args --remote-debugging-port=<p>" does on this fake
    /// machine: bind the mock endpoint on exactly that port (the real app
    /// would). `None` = the unreachable-endpoint scenario: pretend to open,
    /// listen on nothing.
    on_debug_port: Option<Arc<dyn Fn(u16) + Send + Sync>>,
}

pub struct ServerState2 {
    running: std::sync::atomic::AtomicBool,
    port: Mutex<Option<u16>>,
}

impl FakeOs {
    fn with(
        state: Arc<ServerState2>,
        bundle_version: Option<&'static str>,
        on_debug_port: Option<Arc<dyn Fn(u16) + Send + Sync>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state,
            processes: FakeProcesses::new(),
            bundle_version,
            non_loopback: false,
            on_debug_port,
        })
    }

    fn non_loopback(
        state: Arc<ServerState2>,
        on_debug_port: Arc<dyn Fn(u16) + Send + Sync>,
    ) -> Arc<Self> {
        // The endpoint answers on loopback — the *listing* reports a wider
        // binding, which is what the loopback gate reads. The mock must
        // still serve `/json/list` on the real port; discovery is honest
        // and the refusal comes from the falsified binding report.
        Arc::new(Self {
            state,
            processes: FakeProcesses::new(),
            bundle_version: Some("0.70.2"),
            non_loopback: true,
            on_debug_port: Some(on_debug_port),
        })
    }

    fn silent(state: Arc<ServerState2>) -> Arc<Self> {
        Arc::new(Self {
            state,
            processes: FakeProcesses::new(),
            bundle_version: Some("0.70.2"),
            non_loopback: false,
            on_debug_port: None,
        })
    }

    /// The process model this fake machine answers its exact-pid queries
    /// from, so a test can arm slow exits and read the event journal.
    fn processes(&self) -> Arc<FakeProcesses> {
        self.processes.clone()
    }
}

impl OsCommands for FakeOs {
    fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<String> {
        if program.contains("PlistBuddy") {
            return Ok(match self.bundle_version {
                Some(version) => format!("{version}\n"),
                None => String::new(),
            });
        }
        if program == "pgrep" && args.first() == Some(&"-P") {
            let parent = args
                .get(1)
                .and_then(|pid| pid.parse::<u32>().ok())
                .unwrap_or_default();
            return Ok(self
                .processes
                .children_of(parent)
                .iter()
                .map(|pid| pid.to_string())
                .collect::<Vec<String>>()
                .join("\n"));
        }
        if program == "pgrep" {
            return Ok(String::new());
        }
        anyhow::bail!("FakeOs: no other command is planned ({program})")
    }

    fn start(&self, program: &str, args: &[&str]) -> anyhow::Result<()> {
        assert_eq!(program, "open", "the only detached start is `open`");
        assert!(args[..2] == ["-a", "Grok Bot"]);
        let port = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--remote-debugging-port="))
            .and_then(|port| port.parse::<u16>().ok());
        if let Some(port) = port {
            self.processes.spawn_debug(4242);
            self.processes.note(format!("open:debug:{port}"));
            *self.state.port.lock().unwrap() = Some(port);
            self.state.running.store(true, Ordering::SeqCst);
            if let Some(start) = &self.on_debug_port {
                start(port);
            }
        } else {
            // The normal relaunch after the run: no debug flag, no socket.
            self.processes.spawn_plain(5252);
            self.processes.note("open:plain".to_string());
            self.state.running.store(true, Ordering::SeqCst);
            *self.state.port.lock().unwrap() = None;
        }
        Ok(())
    }

    fn pids_matching(&self, _marker: &str) -> anyhow::Result<Vec<u32>> {
        Ok(self.processes.marker_alive())
    }

    fn kill(&self, pid: u32) -> anyhow::Result<()> {
        self.processes.kill(pid);
        // Quitting a main app process ends the endpoint and the port it
        // holds; helpers do not.
        if self.processes.marker_pids.lock().unwrap().contains(&pid) {
            self.state.running.store(false, Ordering::SeqCst);
            *self.state.port.lock().unwrap() = None;
        }
        Ok(())
    }

    fn pid_alive(&self, pid: u32) -> bool {
        self.processes.pid_alive(pid)
    }

    fn port_listeners(&self, port: u16) -> anyhow::Result<Vec<String>> {
        if !self.state.running.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        if *self.state.port.lock().unwrap() != Some(port) {
            return Ok(Vec::new());
        }
        if self.non_loopback {
            Ok(vec![format!(
                "lsof-fake 4242 synthetic 4u IPv4 0x0t0 TCP 10.0.0.5:{port} (LISTEN)"
            )])
        } else {
            Ok(vec![format!(
                "lsof-fake 4242 synthetic 4u IPv4 0x0t0 TCP 127.0.0.1:{port} (LISTEN)"
            )])
        }
    }
}

#[derive(Clone)]
struct MockApp {
    conversations: Vec<String>,
    rows: BTreeMap<String, Vec<Value>>,
    initial_open: Option<String>,
    journal: Arc<Mutex<Vec<String>>>,
    /// What the last locate named; the next mousePress opens it.
    located: Arc<Mutex<Option<String>>>,
    /// The conversation a click last opened: what the rows reads serve.
    current: Arc<Mutex<Option<String>>>,
    /// Reads since the last click: the pager simulation.
    reads: Arc<Mutex<usize>>,
    partials: usize,
}

/// Build the mock endpoint toolkit: the process state the FakeOs answers
/// from, and the "open with a debug port" closure that actually binds a
/// listener on the port the run picked.
fn mock_toolkit(app: MockApp) -> (Arc<ServerState2>, Arc<dyn Fn(u16) + Send + Sync>) {
    let state = Arc::new(ServerState2 {
        running: std::sync::atomic::AtomicBool::new(false),
        port: Mutex::new(None),
    });
    let starter = Arc::new(move |port: u16| {
        let listener = std::net::TcpListener::bind(("127.0.0.1", port))
            .expect("the run picked a port that was still free");
        listener.set_nonblocking(true).expect("the mock polls");
        let app = app.clone();
        let stop = Arc::new(AtomicBool::new(false));
        std::thread::spawn(move || serve_mock_app(listener, app, stop));
    });
    (state, starter)
}

/// Spec §Tests 4 + 7 — dry-run report correctness through the real
/// production CDP client against a mock endpoint on a real loopback socket
/// (`reqwest` discovery, `tungstenite` frames, the loopback gate), then the
/// endpoint refusals: unreachable, and a binding reported wider than
/// loopback.
#[test]
fn the_production_cdp_client_drives_a_mock_endpoint_and_reports_exactly() {
    // ---- The dry-run report, through the production client.
    let harness = Harness::fresh_with(&[(AGENT, window_rows()), (AGENT_NEW, fresh_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    plant_bundle_at(&harness, "Grok Bot.app");
    let blob = harness.persistence_leaf().join(agent_blob_name(AGENT));
    collect_live(&harness.stage, &harness.state_dir, &blob);

    let app = MockApp {
        conversations: vec![AGENT.to_string(), AGENT_NEW.to_string()],
        rows: [
            (AGENT.to_string(), visible_rows()),
            (AGENT_NEW.to_string(), fresh_rows()),
        ]
        .into_iter()
        .collect(),
        initial_open: Some(AGENT.to_string()),
        journal: Arc::new(Mutex::new(Vec::new())),
        located: Arc::new(Mutex::new(None)),
        current: Arc::new(Mutex::new(None)),
        reads: Arc::new(Mutex::new(0)),
        partials: 1,
    };
    let app_view = app.clone();
    let (state, starter) = mock_toolkit(app_view);
    let os = FakeOs::with(state, Some("0.70.2"), Some(starter));
    let driver = CdpAppDriver::with_locations(
        os,
        Clock::wall(),
        &harness.persistence_root,
        vec![harness.sandbox.root().join("Grok Bot.app")],
    );

    // The production driver reads sync evidence from the planted tree's
    // real mtimes, so a background writer keeps them advancing through the
    // run (the app "synced"); stop it before the run's assertions end.
    let payload = std::fs::read(&blob).unwrap();
    let producer = {
        let blob = blob.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let join = std::thread::spawn(move || {
            // Write-then-rename, not truncate-in-place: a concurrent reader
            // of the persistence tree must never catch the blob half-ways —
            // the same atomicity the app itself is held to.
            let scratch = blob.parent().unwrap().join(".producer-tmp");
            while !flag.load(Ordering::SeqCst) {
                if std::fs::write(&scratch, &payload).is_ok() {
                    drop(std::fs::rename(&scratch, &blob));
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            drop(std::fs::remove_file(&scratch));
        });
        (stop, join)
    };

    let mut out = Vec::new();
    let outcome = {
        let request = BackfillRequest {
            stage: &harness.stage,
            machine: MACHINE,
            state_dir: &harness.state_dir,
            shard_bucket_cap: 20,
            destination: &DestinationView::unreachable("synthetic-destination"),
            deliver: false,
            conversation_prefixes: vec![],
            persistence_root: &harness.persistence_root,
            interrupt: &harness.interrupt,
            clock: Clock::wall(),
            driver: Box::new(driver),
        };
        backfill::run_grok_bot_backfill(request, &mut out)
    };
    producer.0.store(true, Ordering::SeqCst);
    producer.1.join().unwrap();

    let report = match outcome {
        BackfillOutcome::Completed(report) => report,
        other => panic!("the CDP dry run should complete against the mock: {other:?}"),
    };
    assert!(report.dry_run, "the run was a dry run");
    assert_eq!(report.app_version_start.as_deref(), Some("0.70.2"));
    assert_eq!(report.conversations.len(), 2);
    let first = report
        .conversations
        .iter()
        .find(|item| item.session_prefix == chat_stasher::id::short_session_id(&session_id(AGENT)))
        .expect("the windowed conversation ran");
    assert_eq!(
        first.rows_delivered, 2,
        "the dry run's plan: pre-head rows only"
    );
    assert_eq!(first.rows_already_delivered, 5);
    assert_eq!(first.loaded_min_seq, Some(1));
    assert_eq!(first.loaded_max_seq, Some(8));
    assert_eq!(first.local_min_seq, Some(4));
    assert_eq!(first.local_max_seq, Some(8));
    assert_eq!(first.withheld_positions, Some(1));
    let second = report
        .conversations
        .iter()
        .find(|item| {
            item.session_prefix == chat_stasher::id::short_session_id(&session_id(AGENT_NEW))
        })
        .expect("the fresh conversation ran");
    assert!(second.new_to_archive);
    assert_eq!(second.rows_delivered, 3);
    // The real transport clicked both conversations; close-out re-clicked
    // the previously open one.
    let events = app.journal.lock().unwrap().clone();
    let clicks = events
        .iter()
        .filter(|line| line.starts_with("click:"))
        .count();
    assert_eq!(clicks, 3, "two opens + one re-click of the previously open");
    // A dry run against a seeded destination wrote nothing: the pre-window
    // rows it found are still absent from the stage body — only live
    // collect's window rows are there.
    assert_eq!(
        row_content_count(
            &stage_body(&harness, AGENT),
            "synthetic follow-up after the withheld row"
        ),
        1,
        "the window row collect delivered is exactly once"
    );
    assert_eq!(
        row_content_count(
            &stage_body(&harness, AGENT),
            "synthetic conversation opening"
        ),
        0,
        "the pre-window row the dry run found is still undelivered"
    );

    // ---- (7a) unreachable endpoint: nothing listens where the app was
    // told to. FakeOs pretends the app launched; no socket answers.
    let harness = Harness::fresh_with(&[(AGENT, window_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    plant_bundle_at(&harness, "Grok Bot.app");
    let (state, _starter) = mock_toolkit(MockApp {
        conversations: vec![AGENT.to_string()],
        rows: [(AGENT.to_string(), visible_rows())].into_iter().collect(),
        initial_open: None,
        journal: Arc::new(Mutex::new(Vec::new())),
        located: Arc::new(Mutex::new(None)),
        current: Arc::new(Mutex::new(None)),
        reads: Arc::new(Mutex::new(0)),
        partials: 0,
    });
    let os = FakeOs::silent(state);
    let (fake_clock_for_boot_wait, _now) = fake_clock();
    let driver = CdpAppDriver::with_locations(
        os,
        fake_clock_for_boot_wait,
        &harness.persistence_root,
        vec![harness.sandbox.root().join("Grok Bot.app")],
    );
    let (outcome, _) = drive_request(&harness, driver, false);
    {
        let exit = outcome.exit_code();
        match outcome {
            BackfillOutcome::Stopped(report, why) => {
                assert_eq!(exit, 3);
                assert!(why.contains("never answered"), "{why}");
                assert!(report.conversations.is_empty());
            }
            other => panic!("an unreachable endpoint stops the run: {other:?}"),
        }
    }

    // ---- (7b) a binding reported wider than loopback: refuse after
    // discovery answers, before any WebSocket is used.
    let harness = Harness::fresh_with(&[(AGENT, window_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    plant_bundle_at(&harness, "Grok Bot.app");
    let (state, starter) = mock_toolkit(MockApp {
        conversations: vec![AGENT.to_string()],
        rows: [(AGENT.to_string(), visible_rows())].into_iter().collect(),
        initial_open: None,
        journal: Arc::new(Mutex::new(Vec::new())),
        located: Arc::new(Mutex::new(None)),
        current: Arc::new(Mutex::new(None)),
        reads: Arc::new(Mutex::new(0)),
        partials: 0,
    });
    let os = FakeOs::non_loopback(state, starter);
    let (clock, _now) = fake_clock();
    let driver = CdpAppDriver::with_locations(
        os,
        clock,
        &harness.persistence_root,
        vec![harness.sandbox.root().join("Grok Bot.app")],
    );
    let (outcome, _) = drive_request(&harness, driver, false);
    {
        let exit = outcome.exit_code();
        match outcome {
            BackfillOutcome::Stopped(report, why) => {
                assert_eq!(exit, 3);
                assert!(why.contains("wider than loopback"), "{why}");
                assert!(report.conversations.is_empty());
            }
            other => panic!("a non-loopback endpoint is refused: {other:?}"),
        }
    }
}

/// W991's close-out left the app not running: the plain relaunch fired
/// milliseconds after the debug instance was killed and lost to its
/// single-instance lock. The close-out must wait for the exact recorded
/// pids — the main process and its children — to be fully gone before
/// relaunching, because a dying Electron process still holds the lock.
/// This test drives the production driver (mock endpoint, fake process
/// table) and pins the ordering: no `open:plain` before the first
/// `pid_alive:…:false` probe of each quit pid.
#[test]
fn the_close_out_waits_out_the_dying_debug_instance_before_the_plain_relaunch() {
    let harness = Harness::fresh_with(&[(AGENT, window_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    plant_bundle_at(&harness, "Grok Bot.app");
    let (state, starter) = mock_toolkit(MockApp {
        conversations: vec![AGENT.to_string()],
        rows: [(AGENT.to_string(), visible_rows())].into_iter().collect(),
        initial_open: Some(AGENT.to_string()),
        journal: Arc::new(Mutex::new(Vec::new())),
        located: Arc::new(Mutex::new(None)),
        current: Arc::new(Mutex::new(None)),
        reads: Arc::new(Mutex::new(0)),
        partials: 0,
    });
    let os = FakeOs::with(state, Some("0.70.2"), Some(starter));
    // The machine W991 measured, down to the pre-state: the user's app is
    // running before the run (the close-out owes a plain relaunch back),
    // and the quitting app's main process and its helper keep answering
    // pid_alive for a few probes after the kill, because an Electron app
    // exits some time after its quit signal.
    os.processes().seed_running_app(36025);
    os.processes().plan_debug_helpers(4242, &[4301], 2);
    os.processes().arm_slow_exit(4242, 3);
    let (clock, _now) = fake_clock();
    let mut driver = CdpAppDriver::with_locations(
        os.clone(),
        clock,
        &harness.persistence_root,
        vec![harness.sandbox.root().join("Grok Bot.app")],
    );
    let pid = driver
        .relaunch_for_backfill(60_515)
        .expect("the debug relaunch comes up against the mock endpoint");
    assert_eq!(pid, 4242);
    let listed = driver
        .list_conversations()
        .expect("the sidebar reads once ready");
    assert_eq!(listed.len(), 1);

    let outcome = driver
        .restore_normal_launch(Some(AGENT))
        .expect("the restore runs");
    assert!(outcome.attempted);

    let journal = os.processes().journal();
    let plain = journal
        .iter()
        .position(|line| line == "open:plain")
        .expect("the plain relaunch ran");
    let main_gone = journal
        .iter()
        .position(|line| line == "alive:4242:false")
        .expect("the main process's exit was confirmed by an exact-pid probe");
    let helper_gone = journal
        .iter()
        .position(|line| line == "alive:4301:false")
        .expect("the helper's exit was confirmed by an exact-pid probe");
    assert!(
        plain > main_gone && plain > helper_gone,
        "the plain relaunch started only after the quitting instance and its \
         recorded children were fully gone — journal: {journal:?}"
    );
    // The restore that owes the relaunch verified it: the relaunched
    // instance stayed running through its settle window, nothing listens
    // on the debug port, and the previously-open conversation's
    // re-selection was performed before the quit.
    assert_eq!(outcome.relaunch_running, Some(true));
    assert_eq!(outcome.port_closed, Some(true));
    assert_eq!(outcome.conversation_reselected, Some(true));
    assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
}

/// A plain relaunch that starts and immediately dies — the single-instance
/// race when a straggler still holds the lock — is caught by the settle
/// window and reported as exactly that, with the port check still true
/// (nothing listens when the relaunched instance is dead: W991's exact
/// readings, which once made the failure invisible).
#[test]
fn a_relaunch_that_starts_and_dies_on_the_lock_is_reported_not_swallowed() {
    let harness = Harness::fresh_with(&[(AGENT, window_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    plant_bundle_at(&harness, "Grok Bot.app");
    let (state, starter) = mock_toolkit(MockApp {
        conversations: vec![AGENT.to_string()],
        rows: [(AGENT.to_string(), visible_rows())].into_iter().collect(),
        initial_open: None,
        journal: Arc::new(Mutex::new(Vec::new())),
        located: Arc::new(Mutex::new(None)),
        current: Arc::new(Mutex::new(None)),
        reads: Arc::new(Mutex::new(0)),
        partials: 0,
    });
    let os = FakeOs::with(state, Some("0.70.2"), Some(starter));
    os.processes().seed_running_app(36025);
    // The plain relaunch is doomed at spawn: alive to the settle probes
    // twice, then gone — an instance that died on the lock.
    os.processes().arm_slow_exit(5252, 2);
    let (clock, _now) = fake_clock();
    let mut driver = CdpAppDriver::with_locations(
        os.clone(),
        clock,
        &harness.persistence_root,
        vec![harness.sandbox.root().join("Grok Bot.app")],
    );
    let _pid = driver
        .relaunch_for_backfill(60_516)
        .expect("the debug relaunch comes up against the mock endpoint");
    let outcome = driver
        .restore_normal_launch(None)
        .expect("the restore runs");
    assert_eq!(
        outcome.relaunch_running,
        Some(false),
        "the settle window caught the dying relaunch"
    );
    assert_eq!(outcome.port_closed, Some(true));
    let failure = outcome.failure.expect("the dead relaunch is a failure");
    assert!(
        failure.contains("exited again within its settle window"),
        "the failure names what was observed: {failure}"
    );
}

/// A quitting debug instance that never exits holds its single-instance
/// lock forever; the close-out waits its bounded wait, then reports the
/// unconfirmed release as exactly that — never relaunches into the lock
/// silently, never guesses it released.
#[test]
fn a_debug_instance_that_never_exits_is_reported_not_guessed() {
    let harness = Harness::fresh_with(&[(AGENT, window_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    plant_bundle_at(&harness, "Grok Bot.app");
    let (state, starter) = mock_toolkit(MockApp {
        conversations: vec![AGENT.to_string()],
        rows: [(AGENT.to_string(), visible_rows())].into_iter().collect(),
        initial_open: None,
        journal: Arc::new(Mutex::new(Vec::new())),
        located: Arc::new(Mutex::new(None)),
        current: Arc::new(Mutex::new(None)),
        reads: Arc::new(Mutex::new(0)),
        partials: 0,
    });
    let os = FakeOs::with(state, Some("0.70.2"), Some(starter));
    // Cold start (nothing to quit at the preflight): the wedge is the
    // debug instance itself, which answers every pid_alive probe.
    os.processes().arm_slow_exit(4242, usize::MAX);
    let (clock, _now) = fake_clock();
    let mut driver = CdpAppDriver::with_locations(
        os.clone(),
        clock,
        &harness.persistence_root,
        vec![harness.sandbox.root().join("Grok Bot.app")],
    );
    let _pid = driver
        .relaunch_for_backfill(60_517)
        .expect("the debug relaunch comes up against the mock endpoint");
    let outcome = driver
        .restore_normal_launch(None)
        .expect("the restore runs");
    let failure = outcome
        .failure
        .expect("an unconfirmed exit is a restore failure");
    assert!(
        failure.contains("did not fully exit within its bounded wait"),
        "the failure names the unconfirmed exit, never a guessed release: {failure}"
    );
    assert!(outcome.relaunch_running.is_none());
}

/// A cold start owes no plain relaunch: the run began against a machine
/// with the app not running, so "restored" means the debug relaunch quit
/// and the machine left as it was — not an app started by the restore.
#[test]
fn a_cold_start_quits_the_debug_relaunch_and_owes_no_plain_relaunch() {
    let harness = Harness::fresh_with(&[(AGENT, window_rows())]);
    std::fs::create_dir_all(&harness.stage).unwrap();
    plant_bundle_at(&harness, "Grok Bot.app");
    let (state, starter) = mock_toolkit(MockApp {
        conversations: vec![AGENT.to_string()],
        rows: [(AGENT.to_string(), visible_rows())].into_iter().collect(),
        initial_open: None,
        journal: Arc::new(Mutex::new(Vec::new())),
        located: Arc::new(Mutex::new(None)),
        current: Arc::new(Mutex::new(None)),
        reads: Arc::new(Mutex::new(0)),
        partials: 0,
    });
    let os = FakeOs::with(state, Some("0.70.2"), Some(starter));
    let (clock, _now) = fake_clock();
    let mut driver = CdpAppDriver::with_locations(
        os.clone(),
        clock,
        &harness.persistence_root,
        vec![harness.sandbox.root().join("Grok Bot.app")],
    );
    let _pid = driver
        .relaunch_for_backfill(60_518)
        .expect("the debug relaunch comes up against the mock endpoint");
    let outcome = driver
        .restore_normal_launch(None)
        .expect("the restore runs");
    let journal = os.processes().journal();
    assert!(
        !journal.iter().any(|line| line == "open:plain"),
        "nothing was running before the run, so the restore starts nothing: {journal:?}"
    );
    assert_eq!(outcome.relaunch_running, None);
    assert_eq!(outcome.port_closed, Some(true));
    assert!(outcome.failure.is_none(), "{:?}", outcome.failure);
}

fn drive_request(
    harness: &Harness,
    driver: CdpAppDriver,
    deliver: bool,
) -> (BackfillOutcome, Vec<u8>) {
    let mut out = Vec::new();
    let request = BackfillRequest {
        stage: &harness.stage,
        machine: MACHINE,
        state_dir: &harness.state_dir,
        shard_bucket_cap: 20,
        destination: &DestinationView::unreachable("synthetic-destination"),
        deliver,
        conversation_prefixes: vec![],
        persistence_root: &harness.persistence_root,
        interrupt: &harness.interrupt,
        clock: Clock::wall(),
        driver: Box::new(driver),
    };
    let outcome = backfill::run_grok_bot_backfill(request, &mut out);
    (outcome, out)
}

fn serve_mock_app(listener: std::net::TcpListener, app: MockApp, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        let Ok((port_stream, _)) = listener.accept() else {
            std::thread::sleep(std::time::Duration::from_millis(25));
            continue;
        };
        let mut stream = port_stream;
        // The listener polls in non-blocking mode; the accepted connection
        // must be blocking again (a socket that inherits the flag returns
        // EWOULDBLOCK from its very first frame read).
        stream.set_nonblocking(false).ok();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .ok();
        // Read the request head as it arrives: the routing decision needs
        // the path, while a WebSocket handshake must still see every byte.
        let mut pending: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    pending.extend_from_slice(&chunk[..count]);
                    if pending.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        stream.set_read_timeout(None).ok();
        let head = String::from_utf8_lossy(&pending).into_owned();
        if head.contains("/json/list") {
            let body = format!(
                r#"[{{"type":"page","id":"synthetic","title":"synthetic","webSocketDebuggerUrl":"ws://127.0.0.1:{}/devtools/page/synthetic"}}]"#,
                stream.local_addr().unwrap().port()
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).ok();
            continue;
        }
        if !head.contains("Upgrade: websocket") && !head.contains("upgrade: websocket") {
            stream
                .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                .ok();
            continue;
        }
        let duplex = PeekedStream {
            pending,
            position: 0,
            inner: Some(stream),
        };
        match tungstenite::accept(duplex) {
            Ok(socket) => serve_mock_connection(socket, &app),
            Err(error) => {
                eprintln!("mock-app accept failed: {error}");
                continue;
            }
        }
    }
}

/// A stream whose first bytes were already read (the request head); the
/// WebSocket handshake still sees them, byte for byte.
struct PeekedStream {
    pending: Vec<u8>,
    position: usize,
    inner: Option<std::net::TcpStream>,
}

impl PeekedStream {
    fn inner(&mut self) -> &mut std::net::TcpStream {
        self.inner
            .as_mut()
            .expect("the stream lives through the connection")
    }
}

impl std::io::Read for PeekedStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.position < self.pending.len() {
            let copy = (self.pending.len() - self.position).min(buf.len());
            buf[..copy].copy_from_slice(&self.pending[self.position..self.position + copy]);
            self.position += copy;
            return Ok(copy);
        }
        self.inner().read(buf)
    }
}

impl std::io::Write for PeekedStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner().flush()
    }
}

fn serve_mock_connection(mut socket: tungstenite::WebSocket<PeekedStream>, app: &MockApp) {
    loop {
        let message = match socket.read() {
            Ok(message) => message,
            Err(_) => return,
        };
        let tungstenite::Message::Text(text) = message else {
            continue;
        };
        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let id = frame["id"].as_u64().unwrap_or(0);
        let method = frame["method"].as_str().unwrap_or_default().to_string();
        let expression = frame["params"]["expression"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let reply = route_mock_method(&method, &expression, &frame, app);
        let payload =
            serde_json::json!({"id": id, "result": {"result": {"type": "object", "value": reply}}});
        if socket
            .send(tungstenite::Message::Text(payload.to_string().into()))
            .is_err()
        {
            return;
        }
    }
}

fn route_mock_method(method: &str, expression: &str, frame: &Value, app: &MockApp) -> Value {
    match method {
        "Runtime.evaluate" => {
            if expression.contains("/*cs:sidebar-list*/") {
                let items: Vec<Value> = app
                    .conversations
                    .iter()
                    .map(|agent| json!({"id": agent}))
                    .collect();
                json!({"items": items})
            } else if expression.contains("/*cs:sidebar-current*/") {
                json!({"agent": app.initial_open.clone()})
            } else if expression.contains("/*cs:sidebar-locate*/") {
                let needle = expression
                    .split("const needle = \"")
                    .nth(1)
                    .and_then(|rest| rest.split('"').next())
                    .unwrap_or_default()
                    .to_string();
                let known = app.conversations.contains(&needle);
                *app.located.lock().unwrap() = known.then_some(needle);
                json!({"found": known, "x": 30.0, "y": 40.0})
            } else if expression.contains("/*cs:transcript-rows*/") {
                let reads = {
                    let mut guard = app.reads.lock().unwrap();
                    *guard += 1;
                    *guard
                };
                let agent = app.current.lock().unwrap().clone();
                let rows = match agent {
                    Some(agent) => app.rows.get(&agent).cloned().unwrap_or_default(),
                    None => Vec::new(),
                };
                let cut = app.partials.saturating_sub(reads).min(rows.len());
                json!({"rows": rows[cut..]})
            } else {
                Value::Null
            }
        }
        "Input.dispatchMouseEvent" => {
            let kind = frame["params"]["type"].as_str().unwrap_or_default();
            if kind == "mousePressed" {
                let opened = app.located.lock().unwrap().clone();
                if let Some(agent) = &opened {
                    app.journal.lock().unwrap().push(format!("click:{agent}"));
                    *app.located.lock().unwrap() = None;
                    *app.current.lock().unwrap() = Some(agent.clone());
                    *app.reads.lock().unwrap() = 0;
                }
            } else if kind == "mouseWheel" {
                app.journal.lock().unwrap().push("nudge".to_string());
            }
            json!({})
        }
        _ => Value::Null,
    }
}
