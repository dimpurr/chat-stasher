//! W1083 / ISSUE-008 — the opencode increment gate, its grace window, the
//! nightly full round, the batched ledger and the staging hash cache.
//!
//! Every fixture here is synthetic and stays metadata-shaped: counts,
//! byte totals and digests of well-known synthetic strings. Two shapes run:
//!
//! * a synthetic opencode SQLite store (the same three tables the real
//!   store has), for the gate, the grace window, the nightly round and the
//!   ledger;
//! * one synthetic claude-code-ish JSONL root, for the committed-prefix
//!   cache round trip and the per-pass ledger batching.
//!
//! No real harness store is read; no network; no conversation text.

use chat_stasher::collect;
use chat_stasher::collect::{CollectReport, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry};
use rusqlite::Connection;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const MACHINE: &str = "fixture-machine";
const DESTINATION: &str = "fixture-destination";
const SESSION: &str = "ses_w1083";

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        // reason: a clock set before the epoch has no meaningful "now" for
        // a fixture; zero keeps the fixture deterministic rather than failing
        .unwrap_or_default()
        .as_millis() as i64
}

// ------------------------------------------------------------ opencode fixture

fn sqlite_registry() -> HarnessRegistry {
    let cell = json!({
        "template": "$XDG_DATA_HOME/opencode/opencode.db",
        "format": "sqlite",
        "confidence": "source-confirmed",
        "source": "synthetic fixture"
    });
    let paths = match scanner::current_platform() {
        "macos" => json!({"macos": cell}),
        "linux" => json!({"linux": cell}),
        "windows" => json!({"windows": cell}),
        platform => panic!("unexpected platform: {platform}"),
    };
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic",
        "harnesses": [{
            "id": "opencode",
            "display_name": "synthetic",
            "paths": paths
        }]
    }))
    .unwrap()
}

fn dest<'a>() -> DestinationView<'a> {
    DestinationView::unreachable(DESTINATION)
}

fn create_store(db: &Path) {
    Connection::open(db)
        .unwrap()
        .execute_batch(
            "CREATE TABLE session (
                 id TEXT PRIMARY KEY,
                 time_created INTEGER NOT NULL,
                 time_updated INTEGER NOT NULL,
                 title TEXT
             );
             CREATE TABLE message (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL,
                 time_updated INTEGER NOT NULL,
                 data TEXT NOT NULL
             );
             CREATE TABLE part (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL,
                 time_updated INTEGER NOT NULL,
                 data TEXT NOT NULL
             );
             -- A table no session export reads: a fixture writes here to move
             -- the store's whole-file fingerprint (size, mtime) while every
             -- collected session's own fields stand still — the production
             -- shape of \"another worker wrote to the store\".
             CREATE TABLE event (
                 id INTEGER PRIMARY KEY,
                 payload TEXT NOT NULL
             );",
        )
        .unwrap();
}

fn connect(db: &Path) -> Connection {
    Connection::open(db).unwrap()
}

fn add_message(conn: &Connection, time_created: i64, role: &str, mid: &str) {
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?3, ?4)",
        rusqlite::params![
            format!("{SESSION}-{mid}"),
            SESSION,
            time_created,
            json!({"role": role}).to_string()
        ],
    )
    .unwrap();
}

fn add_part(conn: &Connection, mid: &str, pid: &str, time: i64, data: &str) {
    conn.execute(
        "INSERT INTO part (id, session_id, message_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        rusqlite::params![
            format!("{SESSION}-{pid}"),
            SESSION,
            format!("{SESSION}-{mid}"),
            time,
            data
        ],
    )
    .unwrap();
}

/// One opencode fixture: a quiet session (every time comfortably outside
/// the grace window) with two messages and three parts, over temp state and
/// stage directories.
struct OpenCodeFixture {
    #[allow(dead_code)] // keeps the tempdir alive for the fixture's lifetime
    dir: tempfile::TempDir,
    db: PathBuf,
    stage: PathBuf,
    state: PathBuf,
}

impl OpenCodeFixture {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("opencode.db");
        create_store(&db);
        let opened = connect(&db);
        opened
            .execute(
                "INSERT INTO session (id, time_created, time_updated, title)
                 VALUES (?1, ?2, ?2, 'fixture')",
                rusqlite::params![SESSION, now_ms() - 3_600_000],
            )
            .unwrap();
        add_message(&opened, now_ms() - 3_500_000, "user", "m1");
        add_part(
            &opened,
            "m1",
            "p1",
            now_ms() - 3_400_000,
            r#"{"type":"text","text":"synthetic one"}"#,
        );
        add_message(&opened, now_ms() - 3_300_000, "assistant", "m2");
        add_part(
            &opened,
            "m2",
            "p2",
            now_ms() - 3_200_000,
            r#"{"type":"text","text":"synthetic two"}"#,
        );
        add_part(
            &opened,
            "m2",
            "p3",
            now_ms() - 3_100_000,
            r#"{"type":"text","text":"synthetic three"}"#,
        );
        drop(opened);
        let stage = dir.path().join("stage");
        fs::create_dir_all(&stage).unwrap();
        let state = dir.path().join("state");
        fs::create_dir_all(&state).unwrap();
        OpenCodeFixture {
            dir,
            db,
            stage,
            state,
        }
    }

    fn scan(&self) -> scanner::ScanReport {
        let mut roots = BTreeMap::new();
        roots.insert("opencode".to_string(), self.db.to_string_lossy().into_owned());
        let config = Config {
            harness_roots: roots,
            ..Config::default()
        };
        scanner::scan_with_registry(&config, &sqlite_registry()).unwrap()
    }

    /// An ordinary incremental pass.
    fn collect(&self) -> CollectReport {
        collect::collect_scan_report(&self.scan(), &self.stage, MACHINE, &self.state, 20, &dest())
            .unwrap()
    }

    /// A pass whose `opencode_full_sweep` flag is decided by the caller —
    /// the nightly full round a scheduled pass runs (ISSUE-008 B).
    fn collect_mode(&self, full_sweep: bool) -> CollectReport {
        collect::collect_scan_report_with_relocation(
            &self.scan(),
            &self.stage,
            MACHINE,
            &self.state,
            20,
            &dest(),
            None,
            full_sweep,
        )
        .unwrap()
    }

    /// Move the store's whole-file fingerprint (the store's size and mtime
    /// change) without touching any session the scan collects.
    fn move_store_fingerprint(&self) {
        self.connect_store()
            .execute(
                "INSERT INTO event (id, payload) VALUES (1, 'not collected')",
                [],
            )
            .unwrap();
    }

    fn connect_store(&self) -> Connection {
        connect(&self.db)
    }
}

// ------------------------------------------------------------------- the gate

/// The session row's time did not move — but opencode wrote rows after it
/// (ISSUE-008 ①'s measured lag). The cursor's message/part high-water fields
/// carry the change, so the gate opens and the next pass delivers — this is
/// ISSUE-008 §4's S2 fixture: a `time_updated`-only gate would have missed
/// both halves of this test.
#[test]
fn rows_written_past_the_session_time_still_deliver_next_pass() {
    let fx = OpenCodeFixture::new();
    let first = fx.collect();
    assert_eq!(first.sqlite_sessions_exported, 1);
    assert_eq!(first.shards_written, 1);

    // A late part: the message existed, the session row never moved.
    let conn = fx.connect_store();
    add_part(
        &conn,
        "m2",
        "p4",
        now_ms() - 3_000_000,
        r#"{"type":"text","text":"synthetic four"}"#,
    );
    drop(conn);
    let second = fx.collect();
    assert_eq!(
        second.sqlite_sessions_skipped, 0,
        "the part high-water moved: the gate must open"
    );
    assert_eq!(second.sqlite_sessions_exported, 1);
    assert_eq!(
        second.lines_written, 1,
        "the increment is exactly the new part's record"
    );

    // An in-place rewrite that bumps its own time, the session row still
    // not moved.
    let conn = fx.connect_store();
    conn.execute(
        "UPDATE part SET data = ?3, time_updated = ?2 WHERE id = ?1",
        rusqlite::params![
            format!("{SESSION}-p1"),
            now_ms() - 2_500_000,
            r#"{"type":"text","text":"synthetic one rewritten"}"#
        ],
    )
    .unwrap();
    drop(conn);
    let third = fx.collect();
    assert_eq!(third.sqlite_sessions_skipped, 0);
    assert_eq!(third.sqlite_sessions_exported, 1);
    assert_eq!(third.lines_written, 1);
}

/// The gate's known blind spot, pinned: a rewrite that bumps no timestamp,
/// no count and no high-water moves no cursor field, so an incremental
/// pass skips it — and the nightly full round (B) catches it by per-record
/// digest comparison and delivers exactly the rewritten record. Worst case
/// one day, by design.
#[test]
fn the_nightly_full_round_catches_a_rewrite_that_bumps_nothing() {
    let fx = OpenCodeFixture::new();
    let first = fx.collect();
    assert_eq!(first.sqlite_sessions_exported, 1);

    // The 72% shape: content changed in place, every time column still.
    let conn = fx.connect_store();
    conn.execute(
        "UPDATE part SET data = ?2 WHERE id = ?1",
        rusqlite::params![
            format!("{SESSION}-p2"),
            r#"{"type":"text","text":"synthetic two rewritten silently"}"#
        ],
    )
    .unwrap();
    drop(conn);

    // An ordinary incremental pass is allowed to be blind here — and says
    // so honestly in the S4 counters.
    let second = fx.collect();
    assert_eq!(second.sqlite_sessions_skipped, 1);
    assert_eq!(second.sqlite_sessions_exported, 0);
    assert_eq!(second.lines_written, 0);

    // The nightly round re-reads everything the gates cannot see.
    let third = fx.collect_mode(true);
    assert_eq!(
        third.sqlite_sessions_full_swept, 1,
        "the full round swept the session despite the equal cursor"
    );
    assert_eq!(third.sqlite_sessions_exported, 1);
    assert_eq!(
        third.lines_written, 1,
        "the digest comparison delivered exactly the rewritten record"
    );

    // And a second full round over a now-clean session converges to nothing.
    let fourth = fx.collect_mode(true);
    assert_eq!(fourth.sqlite_sessions_full_swept, 1);
    assert_eq!(fourth.sqlite_sessions_exported, 0);
    assert_eq!(fourth.lines_written, 0);
}

/// The full round also overrides the whole-store fingerprint shortcut: a
/// store whose file did not move since the last pass is exactly the store
/// the nightly sweep re-proves, so the session must be probed and swept
/// even when the shortcut would have answered "nothing moved".
#[test]
fn the_full_round_overrides_the_whole_store_shortcut() {
    let fx = OpenCodeFixture::new();
    let first = fx.collect();
    assert_eq!(first.sqlite_sessions_exported, 1);

    // The quiet store: the shortcut answers "nothing moved", and the
    // ordinary pass keeps that answer in its counters.
    let second = fx.collect();
    assert_eq!(second.sqlite_sessions_queried, 0);
    assert_eq!(second.sqlite_sessions_skipped, 1);

    // The nightly round must not be stoppable by the same shortcut.
    let third = fx.collect_mode(true);
    assert_eq!(third.sqlite_sessions_queried, 1);
    assert_eq!(third.sqlite_sessions_full_swept, 1);
    assert_eq!(third.sqlite_sessions_exported, 0);
}

/// The grace window (ISSUE-008 ①): a session whose freshest signal is
/// younger than the window is swept even when its cursor compares equal —
/// writes are in flight that no cursor field reflects yet. A quiet session
/// with an equal cursor is skipped as before (`a_moved_store_does_not…`
/// pins that half).
#[test]
fn a_fresh_session_is_swept_despite_an_equal_cursor() {
    let fx = OpenCodeFixture::new();

    // Bring the session right up to the last ten seconds, and export it.
    let conn = fx.connect_store();
    conn.execute(
        "UPDATE session SET time_updated = ?2 WHERE id = ?1",
        rusqlite::params![SESSION, now_ms() - 10_000],
    )
    .unwrap();
    conn.execute(
        "UPDATE part SET time_updated = ?2 WHERE id = ?1",
        rusqlite::params![format!("{SESSION}-p3"), now_ms() - 10_000],
    )
    .unwrap();
    drop(conn);
    let first = fx.collect();
    assert_eq!(first.sqlite_sessions_exported, 1);

    // The store moves underneath (another worker), so the shortcut is out
    // and the per-session gate decides — with a cursor that stands still
    // and a freshest signal ten seconds old.
    fx.move_store_fingerprint();
    let second = fx.collect();
    assert_eq!(
        second.sqlite_sessions_grace_swept, 1,
        "equal cursor, fresh session: swept, not skipped"
    );
    assert_eq!(second.sqlite_sessions_skipped, 0);
    assert_eq!(second.sqlite_sessions_exported, 0);
}

// ------------------------------------------------------------------- the ledger

/// ISSUE-008 ②-a's first cut: an unchanged opencode session under a store
/// another worker wrote to must not re-write the debt ledger. The
/// whole-store fingerprint moved, the session's own fields did not, and the
/// unchanged path no longer stamps the new fingerprint as if it were a
/// change: the pass saves nothing.
#[test]
fn a_moved_store_does_not_restamp_an_unchanged_session() {
    let fx = OpenCodeFixture::new();
    let first = fx.collect();
    assert_eq!(first.sqlite_sessions_exported, 1);
    assert_eq!(first.state_saves, 1, "the first observation is saved once");

    fx.move_store_fingerprint();
    let second = fx.collect();
    assert_eq!(second.sqlite_sessions_skipped, 1);
    assert_eq!(second.sqlite_sessions_queried, 1);
    assert_eq!(
        second.state_saves, 0,
        "an unchanged session claims nothing new, so it records nothing new"
    );
}

// ----------------------------------------------------------- nightly policy

/// The nightly decision itself, over a synthetic calendar: the window hours,
/// the once-per-local-day cap, the wrap-around window, the missed-window
/// catch-up, and the never-written marker. The days are passed explicitly
/// (the caller derives them), so no timezone of the machine running this
/// test can move an assertion.
#[test]
fn the_full_sweep_decision_honours_the_window_and_the_catch_up() {
    use chrono::NaiveDate;
    let today = NaiveDate::from_ymd_opt(2026, 11, 5).unwrap();
    let yesterday = today.pred_opt().unwrap();

    let due = |last_full: Option<i64>,
               now_unix: i64,
               local_hour: u32,
               today_is: chrono::NaiveDate,
               last_day: Option<chrono::NaiveDate>|
     -> bool {
        collect::opencode_full_sweep_due(
            last_full,
            now_unix,
            local_hour,
            today_is,
            last_day,
            (0, 5),
        )
    };
    let window = |start: u32, end: u32| (start, end);

    // A local day in seconds, only to keep the arithmetic readable below.
    const HOUR: i64 = 3600;

    // Default window [0, 5): the first pass of the night, never two.
    assert!(
        due(None, 0, 2, today, None),
        "in window, never swept: the first night full-sweeps"
    );
    assert!(
        !due(Some(0), HOUR, 3, today, Some(today)),
        "already swept this local day: incremental"
    );
    assert!(
        !due(
            Some(0),
            2 * HOUR,
            12,
            yesterday,
            Some(yesterday),
        ),
        "a full last night (different day), but midday is outside the window",
    );

    // The window narrows to a configured one and may wrap midnight.
    assert!(collect::opencode_full_sweep_due(
        None,
        0,
        23,
        today,
        None,
        window(22, 5),
    ));
    assert!(collect::opencode_full_sweep_due(
        None,
        0,
        3,
        today,
        None,
        window(22, 5),
    ));
    assert!(!collect::opencode_full_sweep_due(
        None,
        0,
        12,
        today,
        None,
        window(22, 5),
    ));

    // An empty window turns the nightly off, but never the catch-up: a
    // marker ~28 h old full-sweeps immediately, whatever the hour, so the
    // residue bound holds on a machine that sleeps through its window;
    // 27 h old does not.
    let empty = window(3, 3);
    assert!(!collect::opencode_full_sweep_due(
        Some(0),
        28 * HOUR + 60,
        12,
        today,
        None,
        empty,
    ));
    // The catch-up fires regardless of the day fields the caller passes.
    assert!(collect::opencode_full_sweep_due(
        Some(0),
        28 * HOUR + 60,
        12,
        today,
        Some(today),
        empty,
    ));
    assert!(!collect::opencode_full_sweep_due(
        Some(0),
        27 * HOUR,
        12,
        today,
        Some(today),
        empty,
    ));
    // A marker that was never written does not catch up on its own: the
    // next window hour seeds it, so a fresh install's first full sweep is
    // its first night, not its first afternoon.
    assert!(!collect::opencode_full_sweep_due(
        None,
        12 * HOUR,
        12,
        today,
        None,
        empty,
    ));

    // Configured hours arrive clamped, never as an error.
    let config = Config {
        opencode_full_sweep_start_hour: Some(99),
        opencode_full_sweep_end_hour: Some(7),
        ..Config::default()
    };
    assert_eq!(collect::opencode_full_sweep_hours(&config), (23, 7));
    assert_eq!(
        collect::opencode_full_sweep_hours(&Config::default()),
        (
            collect::DEFAULT_OPENCODE_FULL_SWEEP_START_HOUR,
            collect::DEFAULT_OPENCODE_FULL_SWEEP_END_HOUR
        )
    );
}

/// The marker's own read/write round trip: written once a full pass
/// succeeded, read back by the next decision, and unknown (never "zero")
/// for a state directory that has none or cannot parse one.
#[test]
fn the_full_sweep_marker_round_trips_and_stays_unknown_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    assert_eq!(collect::load_opencode_full_sweep_marker(state), None);

    collect::write_opencode_full_sweep_marker(state, 1_799_428_000).unwrap();
    assert_eq!(
        collect::load_opencode_full_sweep_marker(state),
        Some(1_799_428_000)
    );

    // A corrupt marker is an unknown, never a zero.
    fs::write(
        state.join("opencode-full-sweep-v1.json"),
        b"{not json",
    )
    .unwrap();
    assert_eq!(collect::load_opencode_full_sweep_marker(state), None);
}

// ----------------------------------------------------------- JSONL side

struct JsonlFixture {
    #[allow(dead_code)] // keeps the tempdir alive for the fixture's lifetime
    dir: tempfile::TempDir,
    root: PathBuf,
    stage: PathBuf,
    state: PathBuf,
}

fn jsonl_registry() -> HarnessRegistry {
    let cell = json!({
        "template": "~/.claude/projects",
        "format": "jsonl",
        "confidence": "source-confirmed",
        "source": "synthetic fixture"
    });
    let paths = match scanner::current_platform() {
        "macos" => json!({"macos": cell}),
        "linux" => json!({"linux": cell}),
        "windows" => json!({"windows": cell}),
        platform => panic!("unexpected platform: {platform}"),
    };
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic",
        "harnesses": [{
            "id": "claude-code",
            "display_name": "synthetic",
            "paths": paths
        }]
    }))
    .unwrap()
}

impl JsonlFixture {
    fn new(sessions: usize) -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("source");
        fs::create_dir_all(&root).unwrap();
        for index in 0..sessions {
            fs::write(
                root.join(format!("session-{index}.jsonl")),
                format!("line-{index}-one\nline-{index}-two\ntail-{index}\n"),
            )
            .unwrap();
        }
        let stage = dir.path().join("stage");
        fs::create_dir_all(&stage).unwrap();
        let state = dir.path().join("state");
        fs::create_dir_all(&state).unwrap();
        JsonlFixture {
            dir,
            root,
            stage,
            state,
        }
    }

    fn scan(&self) -> scanner::ScanReport {
        let config = Config {
            claude_projects_dir: Some(self.root.to_string_lossy().into_owned()),
            ..Config::default()
        };
        scanner::scan_with_registry(&config, &jsonl_registry()).unwrap()
    }

    fn collect(&self) -> CollectReport {
        self.collect_mode(false)
    }

    fn collect_mode(&self, full_sweep: bool) -> CollectReport {
        collect::collect_scan_report_with_relocation(
            &self.scan(),
            &self.stage,
            MACHINE,
            &self.state,
            20,
            &dest(),
            None,
            full_sweep,
        )
        .unwrap()
    }

    fn append(&self, index: usize, tail: &str) {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(self.root.join(format!("session-{index}.jsonl")))
            .unwrap();
        file.write_all(tail.as_bytes()).unwrap();
    }

    fn source_len(&self, index: usize) -> u64 {
        fs::metadata(self.root.join(format!("session-{index}.jsonl")))
            .unwrap()
            .len()
    }
}

/// The ledger batch (ISSUE-008 ②-a): a pass writes the debt file once, not
/// once per changed cursor — and a pass that changed nothing writes it not
/// at all. Against the pre-change code this very test is red: three
/// sessions produced three per-record saves on the first pass.
#[test]
fn the_ledger_is_batched_to_once_per_pass_and_none_when_unchanged() {
    let fx = JsonlFixture::new(3);
    let first = fx.collect();
    assert_eq!(first.scanned_records, 3);
    assert_eq!(
        first.state_saves, 1,
        "three first observations, one ledger write"
    );

    // Grow two of the three sessions; the ledger still lands once.
    fx.append(0, "grown-one\n");
    fx.append(2, "grown-two\n");
    let second = fx.collect();
    assert_eq!(second.changed_records, 2);
    assert_eq!(second.state_saves, 1);

    // A pass where nothing moved — not even the seal path ran — writes no
    // state at all.
    let third = fx.collect();
    assert_eq!(third.changed_records, 0);
    assert_eq!(third.state_saves, 0);
}

/// The committed-prefix re-hash is the cost a quiet hour should not pay
/// (ISSUE-008 S3): after a session grows, the *next* quiet pass declares
/// its committed prefix valid without re-reading the bytes, and the
/// stage-coverage proofs read nothing for shards that did not move. Against
/// the pre-change code this is red: every quiet pass re-read and re-hashed
/// the whole committed prefix.
#[test]
fn a_quiet_pass_revalidates_the_prefix_without_re_reading_it() {
    let fx = JsonlFixture::new(1);
    let first = fx.collect();
    assert_eq!(first.shards_written, 1);
    let sealed_after_first = first.stage_covers_bytes_read;

    // Growth: the delta read validates and the cursor moves.
    fx.append(0, "grown-line\n");
    let second = fx.collect();
    assert_eq!(second.changed_records, 1);
    assert_eq!(
        second.shard_bytes_read_hashed,
        fx.source_len(0) - "grown-line\n".len() as u64,
        "the first post-grow pass validates its prefix by reading it"
    );

    // The quiet pass after that: same bytes, same stat — the prefix proves
    // itself from the hash cache, and the stage coverage of the unmoved
    // shards reads nothing.
    let third = fx.collect();
    assert_eq!(third.changed_records, 0);
    assert_eq!(
        third.shard_bytes_read_hashed, 0,
        "an unchanged source's prefix is validated, not re-read"
    );
    assert_eq!(
        third.stage_covers_bytes_read, 0,
        "unmoved stage shards are covered by remembered digests"
    );
    assert!(third.hash_cache_hits >= 2);
    assert!(sealed_after_first <= 2 * fx.source_len(0));
}
