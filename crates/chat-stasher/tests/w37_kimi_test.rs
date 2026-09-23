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

use chat_stasher::config::Config;
use chat_stasher::models::HarnessSource;
use chat_stasher::scanner;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Env mutation is process-global and cargo runs tests in parallel threads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

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
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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

/// `KIMI_CODE_HOME` moves the whole store, and the cell declares it. A
/// declared override that resolves to nothing would be worse than no override
/// at all, so this test pins that it is honoured.
#[test]
fn kimi_code_home_env_override_moves_the_root() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
