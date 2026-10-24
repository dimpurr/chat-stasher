//! W1007 — DSK-2: codex-archived source root, uuid dedup, move round-trips.
//!
//! Codex `thread/archive` MOVES a rollout from `~/.codex/sessions/` into
//! `~/.codex/archived_sessions/` and `thread/unarchive` moves it back; the
//! two directories are two roots of ONE store (measured 2026-10-16: 5
//! archived rollouts, 0 id overlap with sessions/ — moves, not copies). The
//! rollout's id is its filename, so the archive move never changes the
//! session identity — the ADR-059 draft's "movable source root".
//!
//! These tests pin the round-trip contract end to end, against synthetic
//! codex trees only: one session everywhere no matter which root currently
//! holds the file, the body sealed once however often the file moves back
//! and forth (a moved file is a new observation per ADR-055 — never a new
//! session, never a second copy of bytes the archive already holds), and
//! the archived discovery recorded as a bound provenance observation on
//! the one session. The registry fixture carries the same `movable` root
//! data the shipped registry does, including its `status: archived`
//! observation declaration.

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::provenance;
use chat_stasher::scanner;
use chat_stasher::store;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Env mutation is process-global and cargo runs tests in parallel threads;
/// every test in this file takes the lock while it holds a rewritten HOME
/// and registry pointer.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const MACHINE: &str = "fixture-machine";
/// The rollout filename is the session id (stem), unchanged by archive
/// moves — the whole point of the movable root.
const STEM: &str = "rollout-2026-09-01T10-00-00-019bf00d-0000-7000-8000-00000000000a";

fn session_id() -> String {
    format!("codex.{MACHINE}.{STEM}")
}

struct Env {
    _lock: std::sync::MutexGuard<'static, ()>,
    /// Undo actions in reverse order of pinning, run on drop.
    restore: Vec<Box<dyn FnOnce()>>,
}

/// Pin HOME (and its Windows spelling), the registry pointer, and keep
/// CODEX_HOME from redirecting the store for one test; every variable drops
/// back to its previous value, panic included. The test holds the return
/// value for as long as it needs the pin — dropping it early re-aims the
/// scan at the real machine, which is exactly the leak this fixture
/// exists to make impossible.
fn pin_env(home: &Path, registry: &Path) -> Env {
    let lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut restore = Vec::new();

    let previous_codex_home = std::env::var_os("CODEX_HOME");
    std::env::remove_var("CODEX_HOME");
    restore.push(Box::new(move || match previous_codex_home {
        Some(value) => std::env::set_var("CODEX_HOME", value),
        None => std::env::remove_var("CODEX_HOME"),
    }) as Box<dyn FnOnce()>);

    let previous_registry = std::env::var_os(scanner::REGISTRY_ENV);
    std::env::set_var(scanner::REGISTRY_ENV, registry);
    restore.push(Box::new(move || match previous_registry {
        Some(value) => std::env::set_var(scanner::REGISTRY_ENV, value),
        None => std::env::remove_var(scanner::REGISTRY_ENV),
    }));

    for name in ["HOME", "USERPROFILE"] {
        let previous = std::env::var_os(name);
        std::env::set_var(name, home);
        restore.push(Box::new(move || match previous {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }));
    }

    Env {
        _lock: lock,
        restore,
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Drop order: the env is restored while the lock is still held,
        // unpinned in the reverse of the pinning order.
        for undo in self.restore.drain(..).rev() {
            undo();
        }
    }
}

/// The DSK-2 codex registry shape: a primary `sessions/` store plus the
/// `codex-archived` movable root the desktop's `thread/archive` moves
/// rollouts into, declaring the same `status: archived` observation the
/// shipped registry declares on that root.
fn write_registry(dir: &Path) -> PathBuf {
    let cell = serde_json::json!({
        "template": "~/.codex/sessions/",
        "format": "jsonl / jsonl.zst",
        "confidence": "source-confirmed",
        "source": "synthetic W1007 fixture"
    });
    let archived_cell = serde_json::json!({
        "template": "~/.codex/archived_sessions/",
        "format": "jsonl / jsonl.zst",
        "confidence": "source-confirmed",
        "source": "synthetic W1007 fixture"
    });
    let platform_key = scanner::current_platform();
    let registry = dir.join("registry.json");
    fs::write(
        &registry,
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "generated": "W1007 synthetic",
            "harnesses": [{
                "id": "codex",
                "display_name": "synthetic codex",
                "paths": { platform_key: cell },
                "source_roots": [{
                    "id": "codex-archived",
                    "paths": { platform_key: archived_cell },
                    "movable": true,
                    "provenance": { "status": ["archived"] }
                }]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    registry
}

struct Fixture {
    _env: Env,
    _dir: tempfile::TempDir,
    home: PathBuf,
    stage: PathBuf,
    state: PathBuf,
    active: PathBuf,
    archived: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let registry = write_registry(dir.path());
        let active = home
            .join(".codex/sessions/2026/09/01")
            .join(format!("{STEM}.jsonl"));
        let archived = home
            .join(".codex/archived_sessions/2026/09/01")
            .join(format!("{STEM}.jsonl"));
        fs::create_dir_all(active.parent().unwrap()).unwrap();
        fs::create_dir_all(archived.parent().unwrap()).unwrap();
        let stage = dir.path().join("stage");
        let state = dir.path().join("state");
        // The env pin is held by the fixture, so every collect and scan the
        // test runs resolves HOME and the registry at this fixture — never
        // at the real machine.
        let env = pin_env(&home, &registry);
        Self {
            _env: env,
            _dir: dir,
            home,
            stage,
            state,
            active,
            archived,
        }
    }

    fn collect(&self) -> collect::CollectReport {
        collect::collect(
            &Config::default(),
            &self.stage,
            MACHINE,
            &self.state,
            20,
            &DestinationView::unreachable("fixture-destination"),
        )
        .unwrap()
    }

    /// The registry scan collect would see, run directly so the probe
    /// aggregate is assertable in its own right.
    fn scan(&self) -> scanner::ScanReport {
        scanner::scan_with_machine(&Config::default(), MACHINE).unwrap()
    }

    /// The session's sealed body on the stage, by identity — the one
    /// number every assertion in this file reduces to.
    fn body(&self) -> Vec<u8> {
        store::concat_shards_in_dir(&store::session_shard_dir(
            &self.stage,
            MACHINE,
            &session_id(),
        ))
        .unwrap()
    }

    /// How many session directories the stage holds for this machine at
    /// all. A forked session is a second directory; this is the anti-fork
    /// assertion.
    fn staged_session_dirs(&self) -> Vec<String> {
        let dir = self.stage.join("sessions").join(MACHINE);
        let mut names: Vec<String> = fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The archived-status observation rows the collected passes appended
    /// for this session.
    fn archived_observations(&self) -> Vec<provenance::ProvenanceObservation> {
        provenance::read_observations(&self.stage, MACHINE)
            .unwrap()
            .into_iter()
            .filter(|row| row.session_id == session_id())
            .filter(|row| row.dimensions.status == ["archived".to_string()])
            .collect()
    }
}

fn lines(turns: &[u32]) -> Vec<u8> {
    let mut body = Vec::new();
    for turn in turns {
        body.extend_from_slice(format!(r#"{{"synthetic":"w1007-turn-{turn}"}}"#).as_bytes());
        body.push(b'\n');
    }
    body
}

/// The archive → unarchive → regrow → re-archive round trip keeps one
/// session and one body: the move is a new observation of the same rollout
/// (ADR-055), never a fork and never a second copy of bytes the stage
/// already holds. A pass over the archived root appends a bound
/// `status: archived` observation; a pass over the active root records no
/// status at all (an absence is never turned into "active").
#[test]
fn archive_unarchive_regrow_round_trip_is_one_session_never_forks() {
    let fx = Fixture::new();
    fs::write(&fx.active, lines(&[1, 2])).unwrap();

    // Active discovery: the rollout is sealed from the sessions root once.
    fx.collect();
    assert_eq!(fx.body(), lines(&[1, 2]), "first capture seals the rollout");
    assert_eq!(fx.staged_session_dirs(), [session_id()]);

    // thread/archive: the rollout moves into the archived root. The same
    // session, at a new path, is a new observation — the body must not
    // grow (the pre-DSK-2 collector re-sealed the whole file here).
    fs::rename(&fx.active, &fx.archived).unwrap();
    fx.collect();
    assert_eq!(
        fx.body(),
        lines(&[1, 2]),
        "an archived move is re-observed, not re-sealed"
    );
    assert_eq!(
        fx.staged_session_dirs(),
        [session_id()],
        "the moved rollout must not fork a second session directory"
    );
    let first_move_rows = fx.archived_observations();
    assert_eq!(
        first_move_rows.len(),
        1,
        "the archived discovery appends exactly one status observation"
    );
    assert_eq!(first_move_rows[0].body_shard_count, 1);

    // thread/unarchive, then the conversation continues: appended turns
    // arrive at the active path and must land in the same body, in order.
    fs::rename(&fx.archived, &fx.active).unwrap();
    let mut grown = lines(&[1, 2]);
    grown.extend_from_slice(&lines(&[3, 4]));
    fs::write(&fx.active, grown).unwrap();
    fx.collect();
    assert_eq!(
        fx.body(),
        lines(&[1, 2, 3, 4]),
        "appended turns are not lost across the move"
    );
    assert_eq!(fx.staged_session_dirs(), [session_id()]);

    // thread/archive again with the larger body: the delta the active
    // path sealed must not be delivered a second time through the
    // archived path (the pre-DSK-2 cursor duplicated it here).
    fs::rename(&fx.active, &fx.archived).unwrap();
    fx.collect();
    assert_eq!(
        fx.body(),
        lines(&[1, 2, 3, 4]),
        "re-archiving re-observes what the archive already holds, adds nothing"
    );
    assert_eq!(fx.staged_session_dirs(), [session_id()]);
    let second_move_rows = fx.archived_observations();
    assert_eq!(
        second_move_rows.len(),
        2,
        "a discovery of a changed body is a new bound observation; the \
         identical earlier one is not duplicated"
    );
    assert_eq!(second_move_rows[0].body_shard_count, 1);
    assert_eq!(second_move_rows[1].body_shard_count, 2);

    // Idempotent rescan: nothing on the machine changed, so nothing in
    // the archive may change.
    let before = fx.body();
    fx.collect();
    let scan = fx.scan();
    assert_eq!(fx.body(), before, "an idempotent rescan adds nothing");
    assert_eq!(fx.staged_session_dirs(), [session_id()]);
    assert_eq!(
        scan.probes
            .iter()
            .find(|probe| probe.id == "codex")
            .and_then(|probe| probe.record_count),
        Some(1),
        "the aggregate counts the one thread once, wherever it sits"
    );
    assert_eq!(fx.archived_observations().len(), 2);
}

/// A rollout that was archived before the first scan is captured from the
/// archived root directly — the archived root is a source, not only a
/// footnote — and repeated passes over it are idempotent.
#[test]
fn a_rollout_archived_before_first_scan_is_captured_once() {
    let fx = Fixture::new();
    fs::write(&fx.archived, lines(&[5])).unwrap();

    fx.collect();
    assert_eq!(fx.body(), lines(&[5]));
    assert_eq!(fx.staged_session_dirs(), [session_id()]);
    let rows = fx.archived_observations();
    assert_eq!(rows.len(), 1, "the archived root's own capture is observed");
    assert_eq!(rows[0].dimensions.status, ["archived".to_string()]);

    fx.collect();
    assert_eq!(
        fx.body(),
        lines(&[5]),
        "a stable archived rollout re-observes, never re-seals"
    );
    assert_eq!(
        fx.archived_observations().len(),
        1,
        "an identical re-observation dedups against its own row"
    );
    assert_eq!(fx.staged_session_dirs(), [session_id()]);
}

/// The uuid dedup rule: a rollout filename that sits in BOTH roots at once
/// (a copy instead of a move — the measured store moves files, but the
/// archive must not double on the copies it does not expect) is ONE
/// session: two scan records with one id, one aggregate count, one sealed
/// body, one session directory.
#[test]
fn the_same_uuid_in_both_roots_is_one_session_not_two() {
    let fx = Fixture::new();
    fs::write(&fx.active, lines(&[6])).unwrap();
    fs::copy(&fx.active, &fx.archived).unwrap();

    let report = fx.collect();
    assert_eq!(
        report.scan.scanner_records, 2,
        "both roots' discoveries are handed to the collector"
    );
    let scan = fx.scan();
    let probe = scan
        .probes
        .iter()
        .find(|probe| probe.id == "codex")
        .expect("codex probe");
    assert_eq!(probe.record_count, Some(1), "uuid dedup across the roots");
    let codex_records: Vec<&chat_stasher::models::SessionRecord> = scan
        .records
        .iter()
        .filter(|record| record.source == chat_stasher::models::HarnessSource::Codex)
        .collect();
    assert_eq!(
        codex_records.len(),
        2,
        "each root's discovery is kept as its own observation"
    );
    assert_eq!(
        codex_records
            .iter()
            .map(|record| record.id.as_str())
            .collect::<Vec<_>>(),
        [session_id().as_str(), session_id().as_str()],
        "both copies carry the one session identity"
    );

    assert_eq!(
        fx.body(),
        lines(&[6]),
        "a byte-identical copy in the sibling root is covered content, \
         sealed exactly once"
    );
    assert_eq!(fx.staged_session_dirs(), [session_id()]);

    let _rerun = fx.collect();
    let rerun_scan = fx.scan();
    assert_eq!(fx.body(), lines(&[6]));
    assert_eq!(
        rerun_scan
            .probes
            .iter()
            .find(|probe| probe.id == "codex")
            .and_then(|probe| probe.record_count),
        Some(1)
    );
    assert_eq!(fx.staged_session_dirs(), [session_id()]);
}

/// The codex home's metadata — `session_index.jsonl` at the home root and
/// the `*.sqlite` thread stores the desktop's app-server keeps its index in
/// — is NOT in scanning scope (the plan's non-goal list): the declared
/// roots are the two rollout directories and nothing else, so none of it
/// may become a session, a record or a count.
#[test]
fn codex_home_metadata_outside_the_roots_is_never_a_session() {
    let fx = Fixture::new();
    fs::write(&fx.active, lines(&[7])).unwrap();
    // Thread metadata at the codex home root, one level above both declared
    // roots, exactly where the store keeps it.
    fs::write(
        fx.home.join(".codex/session_index.jsonl"),
        "{\"id\":\"metadata-fixture\",\"thread_name\":\"never a session\"}\n",
    )
    .unwrap();
    fs::write(
        fx.home.join(".codex/state_5.sqlite"),
        b"sqlite-metadata-fixture",
    )
    .unwrap();
    // And a stray sqlite-looking file inside a declared root still does not
    // become a session: the root's declared format is jsonl / jsonl.zst.
    fs::write(
        fx.archived
            .parent()
            .unwrap()
            .join("thread_history_1.sqlite"),
        b"sqlite-metadata-fixture",
    )
    .unwrap();

    let scan = fx.scan();
    let probe = scan
        .probes
        .iter()
        .find(|probe| probe.id == "codex")
        .expect("codex probe");
    assert_eq!(
        probe.record_count,
        Some(1),
        "the one rollout is the one session; none of the metadata counts"
    );
    assert!(
        scan.records.iter().all(|record| record
            .absolute_path
            .extension()
            .map(|extension| extension != "sqlite" && extension != "json")
            .unwrap_or(true)),
        "no metadata file became a record: {:?}",
        scan.records
            .iter()
            .map(|record| record.absolute_path.clone())
            .collect::<Vec<_>>()
    );
    // The recognised-file set — what the probe says the scan saw — holds
    // only the rollout.
    assert_eq!(
        probe.recognized_files.as_slice(),
        std::slice::from_ref(&fx.active)
    );

    fx.collect();
    assert_eq!(fx.staged_session_dirs(), [session_id()]);
    assert_eq!(fx.body(), lines(&[7]));
}
