//! W37 — Kimi Code's session layout, exercised over a fixture copy of the real
//! one.
//!
//! The real store on the measuring machine (Kimi Code 0.39.1, 2026-09-19) holds
//! `<home>/sessions/<workspaceId>/session_<uuid>/agents/main/wire.jsonl`, with
//! `state.json` and `logs/kimi-code.log` beside the transcript, an index at
//! `<home>/session_index.jsonl`, and a workspace map at
//! `<home>/workspaces.json`. None of that content is copied here: this file
//! builds the same **shape** with synthetic ids and one-line bodies, and
//! asserts counts, ids, byte sizes and mtimes only.
//!
//! Two things this file is specifically about:
//!
//! * the transcript is always named `wire.jsonl`, so the *filename* cannot
//!   identify a session. The id must come from the directory above it. If it
//!   came from the stem, every Kimi session on a machine would be
//!   `kimi-code.<machine>.wire` — one archive slot for every conversation.
//!   [`ids_come_from_the_session_directory_and_are_distinct`] is the test that
//!   fails if that regresses.
//! * everything else under the same root that is *not* a session stays out of
//!   the count — not silently counted, and not silently dropped either (the
//!   probe still reports the directory as scanned).
//!
//! The cell under test is read out of the shipped registry at run time, the
//! same instrument `registry_default_path_shape_test` uses, so this test
//! cannot drift from the data file it is about.
//!
//! Every test below states its location through process-global env vars and
//! `harness_roots`, so they all run one at a time under [`ENV_LOCK`] and put
//! the environment back through [`EnvRestore`] — a test that failed mid-way
//! must not decide what environment the next test starts from. The branch CI
//! failures this audit was opened for (w440) were a different cause —
//! branches cut from main before the recursive `session_dir` rules, whose
//! shipped data still declares the depth-one `session_*` pattern while this
//! store nests sessions at `sessions/<workspaceId>/<sessionId>`, so the
//! walk opened no session scope and every transcript came back `NotSession`
//! with the probe root resolved exactly as expected — but the audit did
//! find one real isolation gap: `unverified_platform_cells_are_not_scanned`
//! read `CHAT_STASHER_REGISTRY` without the lock, one scheduling accident
//! away from loading another test's planted one-harness registry instead of
//! the shipped one. The lock order and the restore guard close that here.

use chat_stasher::config::Config;
use chat_stasher::models::HarnessSource;
use chat_stasher::scanner;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Env mutation is process-global and cargo runs tests in parallel threads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The environment variables these tests state a location through: `HOME`
/// and `USERPROFILE` carry the isolated home, `KIMI_CODE_HOME` is the kimi
/// cell's declared override, and `CHAT_STASHER_REGISTRY` points the scanner
/// at the planted one-harness registry. These are exactly the names the
/// product consults on a scan (`config::home_from_env` for the first two,
/// the cell's `env_override` and [`scanner::REGISTRY_ENV`] for the rest), so
/// restoring them restores the whole scan-relevant environment.
const MUTATED_ENV: [&str; 4] = [
    "HOME",
    "USERPROFILE",
    "KIMI_CODE_HOME",
    scanner::REGISTRY_ENV,
];

/// Put the [`MUTATED_ENV`] variables back to the values they carried at
/// construction, even when the test panics while they are still mutated.
/// The manual `set_var`/`remove_var` calls inside the tests narrow the window
/// in which the environment is mutated for anything reading it — they cannot
/// put anything back if an assertion fails between them; this guard can, and
/// that is the invariant [`ENV_LOCK`] alone cannot give: the lock orders the
/// mutations, this makes each one reversible.
struct EnvRestore(Vec<(&'static str, Option<OsString>)>);

impl EnvRestore {
    fn now() -> Self {
        Self(
            MUTATED_ENV
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        )
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (name, previous) in self.0.drain(..) {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// The analysis-test twin of [`ENV_LOCK`]: the lock serialises the
/// env-mutating tests, and the snapshot undoes each one's bookkeeping at
/// scope exit, panic or not.
///
/// Field order is the whole design, so both fields are held for their `Drop`
/// alone — the leading underscores say exactly that. Rust drops struct
/// fields in declaration order, so `_env` restores the environment *while
/// the lock is still held*, and only then does `_lock` release it. A
/// `(guard, restore)` tuple — or this struct with the fields swapped — would
/// drop the lock first and hand it to the next waiting test before the
/// environment was restored, letting the leak become *its* "incoming
/// environment". The declaration order is what turns two half-guards into
/// one correct guard.
struct ScanEnv {
    _env: EnvRestore,
    _lock: MutexGuard<'static, ()>,
}

impl ScanEnv {
    fn acquire() -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let env = EnvRestore::now();
        Self {
            _lock: lock,
            _env: env,
        }
    }
}

/// The machine component every id in this file must carry.
const MACHINE: &str = "w37-fixture";

const WS_A: &str = "wd_fixture_00000000000a";
const WS_B: &str = "wd_fixture_00000000000b";

const SESSION_A: &str = "session_00000000-0000-4000-8000-00000000000a";
const SESSION_B: &str = "session_00000000-0000-4000-8000-00000000000b";
const SESSION_C: &str = "session_00000000-0000-4000-8000-00000000000c";

/// Body written into a fixture transcript. Length is asserted, so the three
/// sessions use three different sizes — a record that read the wrong file
/// would show the wrong size rather than pass by symmetry.
fn body(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        out.extend_from_slice(b"{\"role\":\"user\",\"text\":\"synthetic\"}\n");
    }
    out.truncate(len);
    out
}

fn write_file(path: &Path, bytes: &[u8], mtime: SystemTime) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
}

/// The shipped `kimi-code` entry, taken out of the registry this build ships
/// and written into a scratch registry file for `CHAT_STASHER_REGISTRY`.
///
/// It is a straight copy of the shipped JSON, not a re-spelling: a test that
/// restated the cell could pass while the shipped one was wrong.
fn plant_shipped_kimi_cell(dir: &Path) -> PathBuf {
    let shipped = scanner::load_registry_from_repo().expect("shipped registry must load");
    let value: serde_json::Value =
        serde_json::from_str(include_str!("../data/harness-registry-v1.json"))
            .expect("shipped registry must be valid JSON");
    assert!(
        shipped.harnesses.iter().any(|h| h.id == "kimi-code"),
        "the shipped registry must carry kimi-code"
    );
    let entry = value["harnesses"]
        .as_array()
        .expect("harnesses must be an array")
        .iter()
        .find(|h| h["id"] == "kimi-code")
        .expect("shipped registry must carry a kimi-code entry")
        .clone();
    let registry = serde_json::json!({
        "schema_version": 1,
        "generated": "W37 fixture",
        "harnesses": [entry],
    });
    let path = dir.join("registry.json");
    fs::write(&path, serde_json::to_string_pretty(&registry).unwrap()).unwrap();
    path
}

/// An isolated HOME carrying one unconfigured Kimi Code home directory.
fn isolated_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    std::env::set_var("USERPROFILE", home.path());
    // A configured machine is a different test: the template must anchor this
    // one on its own.
    std::env::remove_var("KIMI_CODE_HOME");
    home
}

/// Build the fixture tree and return `(home, sessions_root)`.
fn fixture_store(home: &Path) -> PathBuf {
    let root = home.join(".kimi-code").join("sessions");
    let base = UNIX_EPOCH + Duration::from_secs(1_800_000_000);

    let a = root.join(WS_A).join(SESSION_A);
    let b = root.join(WS_A).join(SESSION_B);
    let c = root.join(WS_B).join(SESSION_C);

    write_file(
        &a.join("agents/main/wire.jsonl"),
        &body(120),
        base + Duration::from_secs(11),
    );
    write_file(
        &b.join("agents/main/wire.jsonl"),
        &body(240),
        base + Duration::from_secs(22),
    );
    write_file(
        &c.join("agents/main/wire.jsonl"),
        &body(360),
        base + Duration::from_secs(33),
    );

    // Everything below is the *shape* of the non-session files the real store
    // keeps beside the transcript. Each one is a plausible session file by
    // name; none of them is a session.
    write_file(&a.join("state.json"), b"{\"synthetic\":true}\n", base);
    write_file(&b.join("state.json"), b"{\"synthetic\":true}\n", base);
    write_file(&a.join("logs/kimi-code.log"), b"synthetic log line\n", base);
    // The home-level index and workspace map live *above* the sessions root.
    write_file(
        &home.join(".kimi-code").join("session_index.jsonl"),
        b"{\"synthetic\":\"index\"}\n",
        base,
    );
    write_file(
        &home.join(".kimi-code").join("workspaces.json"),
        b"{\"synthetic\":\"workspaces\"}\n",
        base,
    );

    root
}

/// Replace the home-level `session_index.jsonl` with one that carries
/// real index lines: a JSON object per session, with the `sessionId`,
/// `sessionDir` and `workDir` columns the implementation writes. Only
/// `sessionId` and `workDir` are read by the scanner; `sessionDir` is
/// written so the fixture matches the real line's shape.
///
/// An entry whose session is not in the fixture store is included on
/// purpose: an index line for an unknown session must observe nothing.
fn write_session_index(home: &Path, entries: &[(&str, &str)]) {
    let mut bytes = Vec::new();
    for (session, work_dir) in entries {
        let line = serde_json::json!({
            "sessionId": session,
            "sessionDir": format!("sessions/fixture-workspace/{session}"),
            "workDir": work_dir,
        });
        let encoded = serde_json::to_string(&line).expect("index line is JSON");
        bytes.extend_from_slice(encoded.as_bytes());
        bytes.push(b'\n');
    }
    write_file(
        &home.join(".kimi-code").join("session_index.jsonl"),
        &bytes,
        UNIX_EPOCH,
    );
}

/// Run the registry-driven scan with the isolated HOME in place. The fixture
/// store lives under the isolated HOME's `.kimi-code`, so the scanner is given
/// that exact location as an explicit kimi-code root.
///
/// Why the root is explicit rather than let the template resolve it: the
/// shipped kimi-code cell is `unascertained` on every platform except macOS
/// (there is no measured Linux/Windows install), and an `unascertained` cell
/// is walked only when the *user* states the location — the contract the
/// scanner itself pins in
/// `configured_root_is_scanned_even_when_the_cell_is_unascertained`. These
/// tests are about the session-dir layout, not about trusting a guessed path,
/// so they hand the fixture location to the scanner as a stated root. Without
/// this the file only ever ran on macOS, where the cell happens to be
/// `source-confirmed`.
fn scan_fixture(home: &Path) -> scanner::ScanReport {
    let registry_path = plant_shipped_kimi_cell(home);
    std::env::set_var(scanner::REGISTRY_ENV, &registry_path);
    let config = Config {
        harness_roots: [(
            "kimi-code".to_string(),
            home.join(".kimi-code")
                .join("sessions")
                .to_string_lossy()
                .into(),
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let report = scanner::scan_with_machine(&config, MACHINE)
        .expect("scan must run with an explicit machine name");
    std::env::remove_var(scanner::REGISTRY_ENV);
    report
}

fn expected_id(session: &str) -> String {
    format!("kimi-code.{MACHINE}.{session}")
}

/// The invariant the whole `session_dir` rule exists for: one record per
/// session, each identified by its own directory, all three distinct.
#[test]
fn ids_come_from_the_session_directory_and_are_distinct() {
    let _scan = ScanEnv::acquire();
    let home = isolated_home();
    let root = fixture_store(home.path());

    let report = scan_fixture(home.path());

    let kimi: Vec<_> = report
        .records
        .iter()
        .filter(|r| r.source == HarnessSource::KimiCode)
        .collect();
    assert_eq!(
        kimi.len(),
        3,
        "one record per session directory; got {:?}",
        kimi.iter().map(|r| r.id.clone()).collect::<Vec<_>>()
    );

    let mut ids: Vec<&str> = kimi.iter().map(|r| r.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![
            expected_id(SESSION_A),
            expected_id(SESSION_B),
            expected_id(SESSION_C),
        ],
        "the native id must be the session directory's own name, not the transcript's stem"
    );

    // The failure this guards against, stated as an assertion: with the stem
    // as the id, all three records would be the same string.
    assert!(
        !ids.contains(&format!("kimi-code.{MACHINE}.wire").as_str()),
        "the constant transcript filename must never become a session id"
    );

    // Each record points at its own transcript, with that file's own size.
    let expected_path = |session: &str| {
        let workspace = if session == SESSION_C { WS_B } else { WS_A };
        root.join(workspace)
            .join(session)
            .join("agents/main/wire.jsonl")
    };
    let mut sizes: Vec<(String, u64)> = kimi
        .iter()
        .map(|r| {
            let session = r.id.rsplit('.').next().unwrap();
            assert_eq!(
                r.absolute_path,
                expected_path(session),
                "a record must point at the transcript inside its own session directory"
            );
            let on_disk = fs::metadata(&r.absolute_path).unwrap();
            assert_eq!(
                r.byte_size,
                on_disk.len(),
                "byte size must come from the file the record names"
            );
            (r.id.clone(), r.byte_size)
        })
        .collect();
    sizes.sort();
    let expected_sizes = [
        (expected_id(SESSION_A), 120),
        (expected_id(SESSION_B), 240),
        (expected_id(SESSION_C), 360),
    ];
    assert_eq!(
        sizes, expected_sizes,
        "each record must carry its own file's size — equal sizes would hide a mix-up"
    );

    // mtime comes from the file too, and the three are distinct on purpose.
    let mut times: Vec<u64> = kimi
        .iter()
        .map(|r| {
            r.mtime
                .duration_since(UNIX_EPOCH)
                .expect("fixture mtimes are after the epoch")
                .as_secs()
        })
        .collect();
    times.sort_unstable();
    assert_eq!(times, vec![1_800_000_011, 1_800_000_022, 1_800_000_033]);
}

/// The probe reports the directory as scanned with a real count, and nothing
/// under it is claimed to be unreadable.
#[test]
fn the_probe_counts_the_sessions_and_claims_nothing_more() {
    let _scan = ScanEnv::acquire();
    let home = isolated_home();
    let root = fixture_store(home.path());

    let report = scan_fixture(home.path());

    let probe = report
        .probes
        .iter()
        .find(|p| p.id == "kimi-code")
        .expect("kimi-code probe row must exist");
    assert_eq!(probe.state, scanner::ProbeState::Scanned);
    assert_eq!(probe.root.as_deref(), Some(root.as_path()));
    assert_eq!(probe.record_count, Some(3));
    assert_eq!(probe.unreadable_count, Some(0));
    assert_eq!(probe.unreadable_entry_count, Some(0));
    // Recognised == handed out, so `collect` can archive every one of them.
    assert_eq!(report.archive_gaps(), Vec::new());
}

/// `state.json`, `logs/kimi-code.log` and the home-level index live in the
/// same tree. None of them is a session, and — the other half — excluding them
/// must not turn into "the store is empty".
#[test]
fn non_session_files_in_the_tree_are_not_counted() {
    let _scan = ScanEnv::acquire();
    let home = isolated_home();
    fixture_store(home.path());

    let report = scan_fixture(home.path());

    for record in &report.records {
        let name = record.absolute_path.file_name().unwrap().to_string_lossy();
        assert_eq!(
            name, "wire.jsonl",
            "only the transcript is a session, got {name}"
        );
        assert!(
            !record.absolute_path.to_string_lossy().contains("logs"),
            "a log file must never be a session record: {}",
            record.absolute_path.display()
        );
    }
    assert_eq!(report.records.len(), 3);
}

/// Only `agents/main/wire.jsonl` is the session. The implementation that was
/// read supports more than one agent directory per session (`agentScopeOf`),
/// so a second agent's transcript must not become a second session — that
/// would hand two records the same session directory, i.e. the same id.
///
/// (The real store on the measuring machine holds `main` only; this case is
/// the shape the implementation allows, not one that was observed.)
#[test]
fn a_sub_agent_transcript_is_not_a_second_session() {
    let _scan = ScanEnv::acquire();
    let home = isolated_home();
    let root = fixture_store(home.path());
    write_file(
        &root
            .join(WS_A)
            .join(SESSION_A)
            .join("agents/explore/wire.jsonl"),
        &body(90),
        UNIX_EPOCH + Duration::from_secs(1_800_000_044),
    );

    let report = scan_fixture(home.path());

    let kimi: Vec<_> = report
        .records
        .iter()
        .filter(|r| r.source == HarnessSource::KimiCode)
        .collect();
    assert_eq!(
        kimi.len(),
        3,
        "a second agent's transcript is not a second session; got {:?}",
        kimi.iter().map(|r| r.id.clone()).collect::<Vec<_>>()
    );
    let mut ids: Vec<&str> = kimi.iter().map(|r| r.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3, "ids must stay unique");
}

/// A transcript with no session directory above it is not a session. Real
/// stores grow stray files; the rule must not adopt them.
#[test]
fn a_transcript_without_a_session_directory_is_not_a_session() {
    let _scan = ScanEnv::acquire();
    let home = isolated_home();
    let root = fixture_store(home.path());
    write_file(
        &root.join("loose/agents/main/wire.jsonl"),
        &body(50),
        UNIX_EPOCH + Duration::from_secs(1_800_000_055),
    );

    let report = scan_fixture(home.path());
    assert_eq!(report.records.len(), 3);
}

/// Two nested directories both matching the pattern: the session a file
/// belongs to is the *nearest* matching directory above it, so the inner one
/// owns the transcript — once, and under its own id.
///
/// This is the case that separates "the directory the file actually sits in"
/// from "some directory further up that also matched". Attributing the file to
/// the outer directory would give one session two ids' worth of claims on it;
/// the assertion below is what fails if that changes.
#[test]
fn a_nested_session_directory_owns_its_transcript_under_its_own_id() {
    let _scan = ScanEnv::acquire();
    let home = isolated_home();
    let root = fixture_store(home.path());
    const OUTER: &str = "session_00000000-0000-4000-8000-00000000000d";
    const INNER: &str = "session_00000000-0000-4000-8000-00000000000e";
    write_file(
        &root
            .join(WS_B)
            .join(OUTER)
            .join(INNER)
            .join("agents/main/wire.jsonl"),
        &body(70),
        UNIX_EPOCH + Duration::from_secs(1_800_000_066),
    );

    let report = scan_fixture(home.path());

    let mut ids: Vec<String> = report.records.iter().map(|r| r.id.clone()).collect();
    ids.sort_unstable();
    let mut expected = vec![
        expected_id(SESSION_A),
        expected_id(SESSION_B),
        expected_id(SESSION_C),
        expected_id(INNER),
    ];
    expected.sort_unstable();
    assert_eq!(
        ids, expected,
        "the inner directory owns its transcript; the outer one must not claim it too"
    );
    let nested = report
        .records
        .iter()
        .find(|r| r.id == expected_id(INNER))
        .expect("the nested session must be present");
    assert_eq!(
        nested.absolute_path,
        root.join(WS_B)
            .join(OUTER)
            .join(INNER)
            .join("agents/main/wire.jsonl"),
        "the record must point at the transcript inside the session directory it named"
    );
    assert_eq!(nested.byte_size, 70);
}

/// The 4D container of a Kimi Code session is the `<workspaceId>`
/// segment the session directory sits under: the measured layout is
/// `sessions/<workspaceId>/<sessionId>`, so the parent segment of
/// the session directory is its workspace. Two sessions in one
/// workspace share that container; a session in another workspace
/// carries the other one.
#[test]
fn the_workspace_segment_above_the_session_directory_is_the_container() {
    let _scan = ScanEnv::acquire();
    let home = isolated_home();
    fixture_store(home.path());

    let report = scan_fixture(home.path());

    let container_of = |session: &str| -> Vec<String> {
        report
            .records
            .iter()
            .find(|r| r.id == expected_id(session))
            .map(|r| r.provenance.container.clone())
            .unwrap_or_default()
    };
    assert_eq!(
        container_of(SESSION_A),
        [WS_A],
        "a session's container is the workspace directory above it"
    );
    assert_eq!(container_of(SESSION_B), [WS_A]);
    assert_eq!(container_of(SESSION_C), [WS_B]);

    // The fixture's index line carries no sessionId or workDir, so
    // no cwd is observed from it: an index that observes nothing
    // observes nothing, and the container stays the only dimension.
    let cwd_of = |session: &str| -> Vec<String> {
        report
            .records
            .iter()
            .find(|r| r.id == expected_id(session))
            .map(|r| r.provenance.cwd.clone())
            .unwrap_or_default()
    };
    assert!(
        cwd_of(SESSION_A).is_empty(),
        "a malformed index line must not become a cwd: {:?}",
        cwd_of(SESSION_A)
    );
}

/// The `workDir` column of `session_index.jsonl` is the working
/// directory the source itself recorded, so it is the session's
/// observed cwd — alongside, not instead of, the container the
/// directory hierarchy carries. A session the index holds no line
/// for observes no cwd.
#[test]
fn the_index_work_dir_is_the_observed_cwd() {
    let _scan = ScanEnv::acquire();
    let home = isolated_home();
    fixture_store(home.path());
    write_session_index(
        home.path(),
        &[
            (SESSION_A, "/w/fixture-a"),
            (SESSION_B, "/w/fixture-b"),
            (
                "session_00000000-0000-4000-8000-0000000000ff",
                "/w/elsewhere",
            ),
        ],
    );

    let report = scan_fixture(home.path());

    let dimension_of = |session: &str| -> (Vec<String>, Vec<String>) {
        report
            .records
            .iter()
            .find(|r| r.id == expected_id(session))
            .map(|r| (r.provenance.container.clone(), r.provenance.cwd.clone()))
            .unwrap_or_default()
    };
    assert_eq!(
        dimension_of(SESSION_A),
        (vec![WS_A.to_string()], vec!["/w/fixture-a".to_string()]),
        "the index workDir is the cwd, and the workspace segment stays the container"
    );
    assert_eq!(
        dimension_of(SESSION_B),
        (vec![WS_A.to_string()], vec!["/w/fixture-b".to_string()])
    );
    // SESSION_C has no index line: the cwd dimension stays
    // unobserved — an empty vector is "nothing observed", never
    // "this session ran nowhere" — while the container its own
    // directory hierarchy carried stays observed.
    assert_eq!(
        dimension_of(SESSION_C),
        (vec![WS_B.to_string()], Vec::<String>::new()),
        "a session without an index line observes no cwd"
    );
}

/// The recursive rule also matches a session directory that sits
/// *directly* under the sessions root. That hierarchy has no
/// workspace segment above the session directory, so the container
/// stays unobserved — not empty-string, not the root itself.
#[test]
fn a_session_directly_under_the_sessions_root_leaves_the_container_unobserved() {
    let _scan = ScanEnv::acquire();
    let home = isolated_home();
    let root = fixture_store(home.path());
    write_file(
        &root.join("session_loose").join("agents/main/wire.jsonl"),
        &body(40),
        UNIX_EPOCH + Duration::from_secs(1_800_000_088),
    );

    let report = scan_fixture(home.path());

    let loose = report
        .records
        .iter()
        .find(|r| r.id == expected_id("session_loose"))
        .expect("a depth-one session directory still matches the recursive rule");
    assert_eq!(
        loose.provenance.container,
        Vec::<String>::new(),
        "a session directly under the sessions root has no workspace segment"
    );
}

/// A session directory nested below another matching directory owns
/// its transcript under its own id (see the test above), but the
/// segment immediately above it is an implementation directory, not
/// a workspace — so the container stays unobserved.
#[test]
fn a_nested_session_directory_leaves_the_container_unobserved() {
    let _scan = ScanEnv::acquire();
    let home = isolated_home();
    let root = fixture_store(home.path());
    const OUTER: &str = "session_00000000-0000-4000-8000-00000000000d";
    const INNER: &str = "session_00000000-0000-4000-8000-00000000000e";
    write_file(
        &root
            .join(WS_B)
            .join(OUTER)
            .join(INNER)
            .join("agents/main/wire.jsonl"),
        &body(70),
        UNIX_EPOCH + Duration::from_secs(1_800_000_066),
    );

    let report = scan_fixture(home.path());

    let nested = report
        .records
        .iter()
        .find(|r| r.id == expected_id(INNER))
        .expect("the nested session directory owns its transcript");
    assert_eq!(
        nested.provenance.container,
        Vec::<String>::new(),
        "the segment above a nested session directory is not a workspace"
    );
}

/// `KIMI_CODE_HOME` moves the whole store, and the cell declares it. A
/// declared override that resolves to nothing would be worse than no override
/// at all, so this test pins that it is honoured.
#[test]
fn kimi_code_home_env_override_moves_the_root() {
    let _scan = ScanEnv::acquire();
    let home = isolated_home();
    // The default location stays empty; the override holds the store.
    let elsewhere = tempfile::tempdir().unwrap();
    let override_home = elsewhere.path().join("kimi-home");
    std::env::set_var("KIMI_CODE_HOME", &override_home);

    let root = override_home.join("sessions");
    write_file(
        &root
            .join(WS_A)
            .join(SESSION_A)
            .join("agents/main/wire.jsonl"),
        &body(80),
        UNIX_EPOCH + Duration::from_secs(1_800_000_077),
    );

    let report = scan_fixture(home.path());

    let probe = report
        .probes
        .iter()
        .find(|p| p.id == "kimi-code")
        .expect("kimi-code probe row must exist");
    assert_eq!(
        probe.root.as_deref(),
        Some(root.as_path()),
        "the declared env override must be the resolved root (note: {})",
        probe.note
    );
    assert_eq!(probe.record_count, Some(1));
    assert_eq!(
        report.records.first().map(|r| r.id.clone()),
        Some(expected_id(SESSION_A))
    );

    std::env::remove_var("KIMI_CODE_HOME");
}

/// The cells the registry ships for the platforms this build is not running
/// on are declared `unascertained`, so this build refuses to walk them rather
/// than guessing a path. That is the documented behaviour of a low-confidence
/// cell, asserted here so a future edit that quietly promotes them is caught.
#[test]
fn unverified_platform_cells_are_not_scanned() {
    // Readers serialize with writers too (same as B97's `ENV_LOCK`):
    // `load_registry_from_repo` consults the process-global
    // `CHAT_STASHER_REGISTRY`, which every scanner test above briefly points
    // at its planted one-harness copy. Without the lock this test passed only
    // by scheduling luck — the planted copy happens to carry the same kimi
    // entry — and a future test that plants a modified cell would turn this
    // into an order-dependent failure.
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let registry = scanner::load_registry_from_repo().expect("shipped registry must load");
    let kimi = registry
        .harnesses
        .iter()
        .find(|h| h.id == "kimi-code")
        .expect("shipped registry must carry kimi-code");
    for platform in ["linux", "windows"] {
        let cell = kimi
            .paths
            .cell_for(platform)
            .unwrap_or_else(|| panic!("kimi-code must declare a {platform} cell"));
        assert_eq!(
            cell.confidence, "unascertained",
            "kimi-code.{platform} is unmeasured: its confidence must stay unascertained"
        );
        assert!(
            !scanner::Confidence::classify(&cell.confidence).scan_allowed(),
            "an unascertained cell must not be walked on that platform"
        );
    }
    let macos = kimi
        .paths
        .cell_for("macos")
        .expect("kimi-code must declare a macos cell");
    assert_eq!(
        macos.confidence, "source-confirmed",
        "the measured macOS cell must carry its measured confidence"
    );
}
