//! W1011 · DSK-5 (ADR-059) — `search --surface` over a synthetic mixed
//! archive, and the loud refusal an unknown surface value gets.
//!
//! The fixture is one machine holding three sessions that differ only in
//! what their records **observed**:
//!
//! * a claude-code CLI-library session — its index row records **no**
//!   surface at all, exactly like the real CLI library does (ADR-059 D1:
//!   an absent observation is never an implicit `cli`);
//! * a claude-code Cowork session — its row records `desktop-cowork`;
//! * an antigravity session — its row records `cli`.
//!
//! What these tests are for, in order of importance:
//!
//! 1. the filter matches only observed surfaces: `--surface
//!    desktop-cowork` finds the Cowork session alone, and the session
//!    with no observation is reported as unplaceable — never silently
//!    dropped, never read as a non-match;
//! 2. it composes with the harness filter and the conversation-time
//!    window, in both directions (a window that excludes the one observed
//!    session is a measured empty answer for it, while the unobserved
//!    session stays unplaceable rather than becoming "not matched");
//! 3. with the flag absent the report is exactly the pre-dimension report:
//!    every session is in scope, nothing moves;
//! 4. a surface value the registry does not declare is a **usage error**
//!    (`2`), never an empty result — and the refusal happens before any
//!    destination is named or read, on `search` and on `export` alike.
//!
//! Privacy line: the archive is built here from synthetic lines, the
//! registry the value check reads is the shipped data file, and the only
//! strings asserted are surface words, exit codes, counts and session-id
//! prefixes. No real archive is opened.

use chat_stasher::activity::{ActivityRow, TimeSource};
use chat_stasher::search::search_sessions;
use chat_stasher::selector::{Selector, SelectorArgs, UnplacedBy};
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

/// 2024-01-01T00:00:00Z — far from any plausible "now", so a window around a
/// conversation and a window around the push cannot overlap by accident.
const JAN_2024: i64 = 1_704_067_200;

const CC_CLI: &str = "claude-code.m-alpha.aaaaaaaa-0000-0000-0000-000000000001";
const CC_COWORK: &str = "claude-code.m-alpha.bbbbbbbb-0000-0000-0000-000000000002";
const AG_CLI: &str = "google-antigravity.m-alpha.cccccccc-0000-0000-0000-000000000003";

/// `2024-01-<day>T<hh>:00Z` and its unix seconds, from one arithmetic so the
/// rows' clock and the windows' clock cannot disagree.
fn jan(day: i64, hh: i64) -> i64 {
    JAN_2024 + (day - 1) * 86_400 + hh * 3_600
}

// ------------------------------------------------------------------- fixture

/// The metadata cache inside this test's own directory (W289).
fn cfg(repo: &Path, key: &Path, root: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(root.join("rustic-cache")),
        no_cache: false,
    }
}

/// One synthetic session, two sealed shards of deterministic filler.
fn write_session_shards(stage: &Path, session: &str, salt: u64) {
    for shard in 0..2 {
        let mut state = salt
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(shard + 1);
        let mut body = Vec::with_capacity(4096);
        while body.len() < 4096 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            body.extend_from_slice(format!("{state:016x}").as_bytes());
        }
        let lines: Vec<Vec<u8>> = vec![body];
        store::write_sealed_shard_bytes_with_cap(
            StageWriter::Collect,
            stage,
            "m-alpha",
            session,
            &lines,
            store::DEFAULT_SHARD_BUCKET_CAP,
        )
        .unwrap();
    }
}

/// One index row with an exact conversation span and — the whole point of
/// this file — whatever surface values the source observed.
fn index_row(first: i64, last: i64, surfaces: &[&str]) -> ActivityRow {
    let mut dimensions = chat_stasher::provenance::SessionProvenance::default();
    for surface in surfaces {
        dimensions.insert_surface(*surface);
    }
    ActivityRow {
        session_id: String::new(),
        machine: "m-alpha".to_string(),
        harness: String::new(),
        first_unix: Some(first),
        last_unix: Some(last),
        line_count: 4,
        time_source: TimeSource::Exact,
        source_zone: None,
        title: None,
        provenance: None,
        session_provenance: None,
        dimensions,
        account_keys: Vec::new(),
        measured_body: None,
    }
}

/// Build the mixed fixture below `root` and return `(repo, key, mk)` — one
/// machine, three sessions, one activity index, one push.
fn build_fixture(root: &Path) -> (PathBuf, PathBuf, MasterKey) {
    let repo = root.join("repo");
    let key = root.join("key.json");
    let mk = MasterKey::new();
    let stage = root.join("stage-m-alpha");

    store::persist_key_file(&cfg(&repo, &key, root), &mk).unwrap();
    let store = BackupStore::new(cfg(&repo, &key, root), "m-alpha".to_string());

    // (session, span, observed surfaces) — the CLI-library row carries none on
    // purpose: that absence is what the filter must report as unplaceable
    // rather than interpreting it as "cli".
    let rows: [(&str, i64, i64, &[&str]); 3] = [
        (CC_CLI, jan(2, 10), jan(2, 11), &[]),
        (CC_COWORK, jan(10, 14), jan(10, 15), &["desktop-cowork"]),
        (AG_CLI, jan(20, 9), jan(20, 10), &["cli"]),
    ];
    for (i, (session, first, last, surfaces)) in rows.iter().enumerate() {
        write_session_shards(&stage, session, 100 + i as u64);
        let mut row = index_row(*first, *last, surfaces);
        row.session_id = (*session).to_string();
        row.harness = session.split('.').next().unwrap().to_string();
        write_activity_row(&stage, &row);
    }

    assert!(store.push(&stage, &mk).unwrap().files_new > 0);
    (repo, key, mk)
}

fn write_activity_row(stage: &Path, row: &ActivityRow) {
    let meta_dir = stage.join("meta").join("m-alpha");
    fs::create_dir_all(&meta_dir).unwrap();
    let path = meta_dir.join("activity-v1.jsonl");
    let mut body = fs::read_to_string(&path).unwrap_or_default();
    body.push_str(&serde_json::to_string(row).unwrap());
    body.push('\n');
    fs::write(&path, body).unwrap();
}

/// A raw unix-seconds window, built the way the deprecated `--since-unix` /
/// `--until-unix` flags build it — the same shape the CLI produces.
fn window_args(since: i64, until: i64) -> SelectorArgs {
    SelectorArgs {
        since_unix: Some(since),
        until_unix: Some(until),
        ..Default::default()
    }
}

fn selector_of(args: SelectorArgs) -> Selector {
    args.resolve().unwrap().selector
}

// --------------------------------------------------------------------- tests

/// 1 — per-surface filtering over the mixed archive. The default run is
/// asserted first and doubles as the parity anchor: nothing is unplaceable
/// and every session is a hit, so any behaviour change the flag-absent path
/// picked up would break this test, not just the filtered ones.
#[test]
fn surface_filtering_matches_only_observed_surfaces() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let (repo, key, mk) = build_fixture(root);
    let store = BackupStore::new(cfg(&repo, &key, root), "m-alpha".to_string());
    // ---- the flag-absent baseline: all three sessions, nothing unplaced ----
    let all = search_sessions(&store, &mk, &Selector::default()).unwrap();
    assert_eq!(all.hits.len(), 3, "the unfiltered archive holds all three");
    assert!(all.unplaced.is_empty());
    assert!(all.answer_complete());
    assert_eq!(all.data_blobs_read, 0, "the metadata tier reads no shard");
    let found: std::collections::BTreeSet<&str> =
        all.hits.iter().map(|hit| hit.session_id.as_str()).collect();
    assert_eq!(
        found,
        [CC_CLI, CC_COWORK, AG_CLI].into_iter().collect(),
        "every session is in scope by default"
    );

    // ---- `--surface desktop-cowork`: the Cowork session alone -------------
    let cowork = search_sessions(
        &store,
        &mk,
        &Selector {
            surface: Some("desktop-cowork".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cowork.hits.len(), 1, "only the session that observed it");
    assert_eq!(cowork.hits[0].session_id, CC_COWORK);
    assert_eq!(
        cowork.not_matched, 1,
        "the session that observed `cli` is a proven non-match"
    );
    assert_eq!(cowork.unplaced.len(), 1);
    assert_eq!(cowork.unplaced[0].session_id, CC_CLI);
    assert_eq!(cowork.unplaced[0].dimension, UnplacedBy::Surface);
    assert!(
        cowork.unplaced[0]
            .why
            .contains("no source-surface provenance was recorded"),
        "the reason must name the absent observation: {}",
        cowork.unplaced[0].why
    );
    assert!(
        !cowork.answer_complete(),
        "a hit alongside an unplaceable session is a partial answer"
    );

    // ---- `--surface cli`: the antigravity session alone --------------------
    let cli = search_sessions(
        &store,
        &mk,
        &Selector {
            surface: Some("cli".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cli.hits.len(), 1);
    assert_eq!(cli.hits[0].session_id, AG_CLI);
    assert_eq!(cli.hits[0].harness.as_deref(), Some("google-antigravity"));
    assert_eq!(cli.not_matched, 1, "the Cowork observation is a non-match");
    assert_eq!(cli.unplaced.len(), 1, "still the same absent observation");
    assert_eq!(cli.unplaced[0].session_id, CC_CLI);
    assert_eq!(cli.unplaced[0].dimension, UnplacedBy::Surface);

    // The observed surface travels with the hit, in the same shape `--json`
    // prints, which is the UI's and the export manifest's answer too.
    let json: serde_json::Value =
        serde_json::from_str(&chat_stasher::search::report_json(&cowork, false)).unwrap();
    assert_eq!(
        json["sessions"][0]["dimensions"]["surface"],
        serde_json::json!(["desktop-cowork"])
    );
    assert_eq!(
        json["sessions_not_placed"][0]["dimension"],
        serde_json::json!("surface")
    );
}

/// 2 — composition. Same fixture; the surface filter is an AND with the
/// harness filter and the time window, and each constraint keeps its own
/// verdict: a session the harness already rejected stays a non-match, the
/// window can exclude the observed session as a *measured* non-match, and
/// the session with no observation stays unplaceable through all of it —
/// unknown is not allowed to become "no" because another filter is active.
#[test]
fn the_surface_filter_composes_with_harness_and_time_window() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let (repo, key, mk) = build_fixture(root);
    let store = BackupStore::new(cfg(&repo, &key, root), "m-alpha".to_string());
    // All three constraints picking out the Cowork session.
    let all_three = Selector {
        surface: Some("desktop-cowork".to_string()),
        harnesses: Some(["claude-code".to_string()].into_iter().collect()),
        ..selector_of(window_args(jan(10, 0), jan(11, 0)))
    };
    let picked = search_sessions(&store, &mk, &all_three).unwrap();
    assert_eq!(picked.hits.len(), 1);
    assert_eq!(picked.hits[0].session_id, CC_COWORK);
    assert_eq!(picked.unplaced.len(), 1, "the CLI-library session");
    assert_eq!(picked.unplaced[0].dimension, UnplacedBy::Surface);
    assert_eq!(
        picked.not_matched, 1,
        "the harness rejected the antigravity session"
    );

    // The window moves off the one observed session: the Cowork session is
    // now a measured non-match (its time is known), while the unobserved one
    // stays unplaceable — the surface question is still unanswered for it.
    let window_elsewhere = Selector {
        surface: Some("desktop-cowork".to_string()),
        ..selector_of(window_args(jan(4, 0), jan(5, 0)))
    };
    let elsewhere = search_sessions(&store, &mk, &window_elsewhere).unwrap();
    assert_eq!(elsewhere.hits.len(), 0);
    assert_eq!(elsewhere.not_matched, 2);
    assert_eq!(elsewhere.unplaced.len(), 1);
    assert_eq!(elsewhere.unplaced[0].session_id, CC_CLI);
    assert_eq!(elsewhere.unplaced[0].dimension, UnplacedBy::Surface);
    assert!(!elsewhere.answer_complete());
}

/// 3 — the flag-absent default. The selector the CLI resolves from no flags
/// must answer exactly what the pre-dimension selector answered: same hits,
/// same counts, nothing unplaceable, and no registry read on the path
/// (`resolve_selector_flags` only consults the registry when a surface was
/// named). The other filters keep working unchanged beside it.
#[test]
fn an_absent_surface_flag_answers_exactly_as_before() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let (repo, key, mk) = build_fixture(root);
    let store = BackupStore::new(cfg(&repo, &key, root), "m-alpha".to_string());
    // What `search` resolves from an empty command line today, through the
    // very entry the CLI now calls.
    let resolved = chat_stasher::scanner::resolve_selector_flags(&SelectorArgs::default()).unwrap();
    assert_eq!(resolved.selector, Selector::default());
    assert!(resolved.warnings.is_empty());

    let no_flags = search_sessions(&store, &mk, &resolved.selector).unwrap();
    let direct = search_sessions(&store, &mk, &Selector::default()).unwrap();
    let same_ids = |hits: &Vec<chat_stasher::search::SessionHit>| {
        hits.iter()
            .map(|h| h.session_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(same_ids(&no_flags.hits), same_ids(&direct.hits));
    assert_eq!(no_flags.sessions_seen, direct.sessions_seen);
    assert_eq!(no_flags.not_matched, direct.not_matched);
    assert_eq!(no_flags.unplaced.len(), direct.unplaced.len());
    assert_eq!(no_flags.answer_complete(), direct.answer_complete());
    assert_eq!(
        no_flags.index_files_read, direct.index_files_read,
        "the activity index is read the same way, once per machine"
    );

    // And a real filter beside it still selects by machine alone.
    let machine_only = selector_of(SelectorArgs {
        machine: Some("m-alpha".to_string()),
        ..Default::default()
    });
    let by_machine = search_sessions(&store, &mk, &machine_only).unwrap();
    assert_eq!(by_machine.hits.len(), 3);
    assert!(by_machine.answer_complete());
}

/// 4 — the loud refusal. A surface value the registry does not declare is a
/// usage error: exit `2`, the message names the requested value and the
/// declared vocabulary, nothing is printed on stdout, and no destination is
/// named or opened — the same discipline a malformed date gets. A value the
/// registry does declare passes validation and moves on to the next unmet
/// requirement (naming a destination), proving both the order and that the
/// valid path is unaffected.
#[test]
fn an_unknown_surface_value_is_a_usage_error_not_an_empty_result() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    let bad = run_search_cli(root, &["--surface", "desktop-cowork-x"]);
    assert_eq!(
        bad.status.code(),
        Some(2),
        "a typo must be a usage error, never an empty answer; stdout={} stderr={}",
        String::from_utf8_lossy(&bad.stdout),
        String::from_utf8_lossy(&bad.stderr)
    );
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(
        stderr.contains("desktop-cowork-x"),
        "the refusal must name the requested value: {stderr}"
    );
    assert!(
        stderr.contains("desktop-cowork") && stderr.contains("ide"),
        "the refusal must list the declared vocabulary: {stderr}"
    );
    assert!(
        stderr.contains("dimensions.surfaces"),
        "the refusal must say where the vocabulary lives: {stderr}"
    );
    assert!(
        bad.stdout.is_empty(),
        "a refused query prints no result object"
    );

    // A declared value passes the check and runs on to the next missing
    // piece — the destination — which is a different usage error.
    let good_value = run_search_cli(root, &["--surface", "desktop-cowork"]);
    assert_eq!(good_value.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&good_value.stderr);
    assert!(
        !stderr.contains("is not a surface"),
        "a declared surface must pass validation: {stderr}"
    );
    assert!(
        stderr.contains("name the destination"),
        "a validated filter moves on to the next requirement: {stderr}"
    );
}

/// 5 — the same refusal on `export`, because `export` writes the sessions
/// `search` selects "for the same flags": an unknown surface there must not
/// behave differently than it does on the dry run.
#[test]
fn export_refuses_an_unknown_surface_value_like_search() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let out = root.join("never-written");
    let export = run_cli_without_destination(
        root,
        "export",
        &["--out", out.to_str().unwrap(), "--surface", "desktp-cowork"],
    );
    assert_eq!(
        export.status.code(),
        Some(2),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&export.stdout),
        String::from_utf8_lossy(&export.stderr)
    );
    let stderr = String::from_utf8_lossy(&export.stderr);
    assert!(
        stderr.contains("desktp-cowork"),
        "the refusal must name the requested value: {stderr}"
    );
    assert!(!out.exists(), "a refused export writes nothing");
}

// ------------------------------------------------------------------ children

/// Spawn the built CLI with the W289/W306 sandbox discipline this suite
/// uses everywhere, against a destination-less `search` line: the filter
/// resolve happens before any `--repo` or `--destination` is consulted, so
/// a refused query costs nothing and opens nothing. `CHAT_STASHER_REGISTRY`
/// is removed so the child reads the same registry data file this test's
/// expectations describe, not a developer override.
fn run_cli_without_destination(
    root: &Path,
    subcommand: &str,
    args: &[&str],
) -> std::process::Output {
    let home = root.join("child-home");
    fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .arg(subcommand)
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("XDG_CONFIG_HOME", root.join("child-config"))
        .env("XDG_DATA_HOME", root.join("child-data"))
        .env("XDG_STATE_HOME", root.join("child-state"))
        .env("XDG_CACHE_HOME", root.join("child-cache"))
        .env_remove("RUSTIC_REPO")
        .env_remove("RUSTIC_KEY_FILE")
        .env_remove("CHAT_STASHER_REGISTRY");
    command.output().unwrap()
}

fn run_search_cli(root: &Path, args: &[&str]) -> std::process::Output {
    run_cli_without_destination(root, "search", args)
}
