//! The Windows shape of `doctor_consistency_test`, runnable on every platform.
//!
//! `doctor_tables_never_contradict_any_harness` is green on macOS and Linux and
//! red on `windows-latest`. The two things that are different about Windows are
//! reproducible without a Windows machine, so they are reproduced here:
//!
//!   1. **the registry cell that applies** — on Windows the Cursor cell is
//!      `%APPDATA%\Cursor\User\globalStorage\state.vscdb`, which
//!      `scanner::static_prefix_root` cannot anchor (`%APPDATA%` is not a
//!      variable it resolves), so the template contributes nothing at all;
//!   2. **the shape of the path the user writes down** — a Windows home
//!      directory is spelled with backslashes, so the path that lands in
//!      `config.toml` is `C:\Users\...\state.vscdb`.
//!
//! (2) is the load-bearing one: a backslash is TOML's escape character inside a
//! basic string, so a Windows path pasted verbatim makes the *whole config file*
//! unparseable — and the config is the only thing that tells the tool where this
//! fixture planted its store. That is the same invariant the explicit-root work
//! (`config_explicit_root_test`) nailed down, failing one layer earlier: the
//! user did write the path down, and the tool acted as if they had not.
//!
//! Both halves are simulated, so this test executes the *same* code path on
//! macOS, Linux and Windows: the platform cells of the scratch registry are all
//! the shipped Windows cell, and the home directory is one whose name contains
//! backslashes (on Windows the temporary directory already is one — backslashes
//! are not legal in a Windows filename, so it is the platform, not the test,
//! that supplies the shape there).

use chat_stasher::doctor;
use chat_stasher::scanner::ProbeState;
use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Env mutation is process-global and cargo runs tests in parallel threads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Same planted ground truth as `doctor_consistency_test`.
const CURSOR_SESSIONS: u64 = 2;
const GROK_SESSIONS: u64 = 1;

/// A home directory whose path is spelled the way a Windows home is: with
/// backslashes in it.
///
/// On Windows the temporary directory already is such a path (and a literal
/// backslash cannot appear *inside* a Windows filename), so there the platform
/// supplies the shape. Everywhere else a directory whose name contains
/// backslashes is created — a perfectly ordinary POSIX filename that produces
/// exactly the property under test: `home.display()` contains `\`.
fn windows_shaped_home(base: &Path) -> PathBuf {
    let home = if cfg!(windows) {
        base.to_path_buf()
    } else {
        base.join(r"C:\Users\winshape")
    };
    fs::create_dir_all(&home).unwrap();
    assert!(
        home.display().to_string().contains('\\'),
        "premise of instrument failed: simulated HOME must contain backslash, actual is {}",
        home.display()
    );
    home
}

/// Point every base-directory variable at `home`, so no probe can reach the
/// real machine.
fn isolate_home(home: &Path) {
    std::env::set_var("HOME", home);
    std::env::set_var("USERPROFILE", home);
    std::env::set_var("XDG_DATA_HOME", home);
    std::env::remove_var("XDG_CONFIG_HOME");
    std::env::remove_var("XDG_STATE_HOME");
    for var in [
        "CODEX_HOME",
        "GEMINI_CLI_HOME",
        "CURSOR_USER_DIR",
        "OPENCODE_DB",
    ] {
        std::env::remove_var(var);
    }
}

/// The shipped `cursor` / `grok` **Windows** cells, verbatim, in all three
/// platform slots — the registry-swap trick that makes a foreign platform's
/// shape reachable from any machine. Cursor's Windows template is `%APPDATA%`
/// (unanchorable) and Grok's is `unascertained`; both therefore have to be supplied by
/// the config, which is precisely what this test is about.
fn write_windows_shaped_registry(home: &Path) -> PathBuf {
    let cursor = r#"{ "template": "%APPDATA%\\Cursor\\User\\globalStorage\\state.vscdb",
                      "env_override": "CURSOR_USER_DIR", "format": "sqlite",
                      "confidence": "community-claim-unverified", "source": "B58 test: cursor.windows cell of shipped registry",
                      "sql_table": "cursorDiskKV", "sql_id_column": "key",
                      "sql_required_columns": ["key", "value"],
                      "sql_key_column": "key", "sql_key_pattern": "composerData:%",
                      "sql_value_column": "value", "sql_time_json_path": "$.createdAt",
                      "sql_qualification": "cursor-composer-headers" }"#;
    let grok = r#"{ "template": "%USERPROFILE%\\.grok\\sessions\\session_search.sqlite",
                    "format": "sqlite", "confidence": "unascertained",
                    "source": "B58 test: grok.windows cell of shipped registry",
                    "sql_table": "session_docs", "sql_id_column": "session_id",
                    "sql_required_columns": ["session_id", "updated_at"],
                    "sql_time_column": "updated_at", "sql_time_value_is_seconds": true }"#;
    let path = home.join("registry.json");
    fs::write(
        &path,
        format!(
            r#"{{ "schema_version": 1, "generated": "B58",
                  "harnesses": [
                    {{ "id": "cursor", "display_name": "Cursor",
                       "paths": {{ "macos": {cursor}, "linux": {cursor}, "windows": {cursor} }} }},
                    {{ "id": "grok", "display_name": "Grok CLI",
                       "paths": {{ "macos": {grok}, "linux": {grok}, "windows": {grok} }} }}
                  ] }}"#
        ),
    )
    .unwrap();
    std::env::set_var("CHAT_STASHER_REGISTRY", &path);
    path
}

/// Same Cursor store the consistency test plants: `cursorDiskKV`, sessions
/// selected by `composerData:%`, `createdAt` inside the JSON value.
fn plant_cursor_db(home: &Path) -> PathBuf {
    let db = home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
    )
    .unwrap();
    for i in 0..CURSOR_SESSIONS {
        let key = format!("composerData:00000000-0000-4000-8000-{i:012}");
        let value = format!(
            r#"{{"composerId":"c{i}","createdAt":{},"fullConversationHeadersOnly":[{{}}]}}"#,
            1760000000000i64 + i as i64
        );
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
        .unwrap();
    }
    drop(conn);
    db
}

/// Same Grok store the consistency test plants: `session_docs`, `updated_at`
/// in seconds.
fn plant_grok_db(home: &Path) -> PathBuf {
    let db = home.join(".grok/sessions/session_search.sqlite");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL, updated_at INTEGER NOT NULL, title TEXT NOT NULL, content TEXT NOT NULL, content_hash TEXT NOT NULL, last_indexed_offset INTEGER NOT NULL DEFAULT 0)",
    )
    .unwrap();
    for i in 0..GROK_SESSIONS {
        conn.execute(
            "INSERT INTO session_docs (session_id, cwd, updated_at, title, content, content_hash) VALUES (?1,?2,?3,?4,?5,?6)",
            rusqlite::params![
                format!("019f95ce-{i:08}-0000-0000-0000-000000000000"),
                "/tmp/x",
                1784924765i64 + i as i64,
                "t",
                "c",
                "h"
            ],
        )
        .unwrap();
    }
    drop(conn);
    db
}

/// Write the config the way a user on this platform naturally would: the path
/// exactly as the OS spells it, pasted between quotes. On Windows that string
/// contains backslashes — which is the whole point.
fn declare_planted_roots(home: &Path, cursor: &Path, grok: &Path) -> PathBuf {
    let config = home.join(".config/chat-stasher/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        format!(
            "[harness_roots]\ncursor = \"{}\"\ngrok = \"{}\"\n",
            cursor.display(),
            grok.display()
        ),
    )
    .unwrap();
    config
}

/// The same invariant `doctor_consistency_test` asserts on line 300, in the
/// Windows shape: the store is planted, the user wrote its path down, the tool
/// can open it — so the footprint table must report the planted count.
#[test]
fn windows_shaped_home_still_honours_the_configured_root() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let base = tempfile::tempdir().unwrap();
    let home = windows_shaped_home(base.path());
    isolate_home(&home);
    write_windows_shaped_registry(&home);

    let cursor_db = plant_cursor_db(&home);
    let grok_db = plant_grok_db(&home);
    let config = declare_planted_roots(&home, &cursor_db, &grok_db);

    // Premises of the instrument itself, so a failure below cannot be blamed on
    // the fixture: the stores are really there and the config really names them.
    assert!(
        cursor_db.is_file(),
        "instrument premise: planted cursor store must exist"
    );
    assert!(
        grok_db.is_file(),
        "instrument premise: planted grok store must exist"
    );
    assert!(
        config.is_file(),
        "instrument premise: config file must be written out"
    );

    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");

    let cursor = report
        .footprints
        .iter()
        .find(|f| f.name == "cursor")
        .unwrap();
    assert_eq!(
        cursor.session_count,
        Some(CURSOR_SESSIONS),
        "footprint table self-check against known ground truth: should recognize our {CURSOR_SESSIONS} planted cursorDiskKV sessions"
    );
    let grok = report.footprints.iter().find(|f| f.name == "grok").unwrap();
    assert_eq!(
        grok.session_count,
        Some(GROK_SESSIONS),
        "footprint table self-check against known ground truth: should recognize our {GROK_SESSIONS} planted session_docs sessions"
    );

    std::env::remove_var("CHAT_STASHER_REGISTRY");
}

/// The shipped Windows shape of the claude-code family (W1006d, ADR-059
/// draft D6): the CLI primary store is scannable while the desktop Cowork
/// root's Windows cell is `unascertained` — a path the scan refuses to
/// guess at — so the family's combined total is a *qualified lower
/// bound*. `doctor` must report the measured CLI count, not erase it into
/// a blanket unknown (the side effect the draft's external review rejected),
/// and must mark the unverified root in both the registry probe row and
/// the footprint row, so the two tables keep agreeing with each other
/// (`doctor_consistency_test` joins them) while neither claims the
/// family total.
///
/// The registry-swap trick of the file (`doctor_windows_shape_test`)
/// again makes a foreign platform's shape runnable from any machine: the
/// claude-code primary's Windows cell is replayed for this platform.
#[test]
fn windows_unverified_desktop_cell_keeps_the_measured_cli_count_marked() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let base = tempfile::tempdir().unwrap();
    let home = windows_shaped_home(base.path());
    isolate_home(&home);
    write_windows_desktop_store_registry(&home);

    // Two CLI sessions under the registry-declared primary root, resolved
    // through the isolated `$HOME` (the shape-mate above replays the
    // Windows path itself; this test replays the Windows *aggregate*).
    let claude_projects = home.join(".claude").join("projects").join("repo-abc");
    fs::create_dir_all(&claude_projects).unwrap();
    for i in 0..2u8 {
        fs::write(
            claude_projects.join(format!("019d5c06-{i:04}.jsonl")),
            "{}\n",
        )
        .unwrap();
    }
    // The desktop Cowork root is deliberately NOT planted: its cell is
    // `unascertained`, so planting anything there changes nothing — the
    // probe never looks, which is the point of the fixture.

    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");

    let probe = report
        .probes
        .iter()
        .find(|p| p.id == "claude-code")
        .expect("registry probe row for claude-code");
    assert_eq!(
        probe.record_count,
        Some(2),
        "the measured CLI count is reported, never erased by the unverified desktop root; note={}",
        probe.note
    );
    assert_eq!(
        probe
            .unverified_roots
            .iter()
            .map(|root| root.id.as_str())
            .collect::<Vec<_>>(),
        ["claude-desktop-cowork"],
        "the probe row marks the unverified root so the count reads as a lower bound"
    );
    assert!(
        probe.note.contains("lower bound"),
        "the note qualifies the total; note={}",
        probe.note
    );

    let footprint = report
        .footprints
        .iter()
        .find(|f| f.name == "claude-code")
        .expect("footprint row for claude-code");
    assert_eq!(
        footprint.session_count, probe.record_count,
        "the two doctor tables must keep agreeing on the count"
    );
    assert_eq!(
        footprint.unverified_roots,
        vec!["claude-desktop-cowork".to_string()],
        "the footprint row carries the same marking as the probe row"
    );
    assert!(
        footprint.note.contains("lower bound"),
        "the footprint note qualifies the count too; note={}",
        footprint.note
    );

    std::env::remove_var("CHAT_STASHER_REGISTRY");
}

/// The empty-machine half of the W1006d shape above: the same Windows family
/// — scannable CLI primary beside an `unascertained` desktop Cowork root —
/// with **nothing installed at all**. The primary root was still MEASURED
/// (looked for, found absent), so the scanner's aggregate keeps the measured
/// zero as the lower bound's floor instead of collapsing the family into an
/// unknown: `record_count == Some(0)` with the unverified sibling named in
/// `unverified_roots`, the state that tells the truth about presence staying
/// `missing`, and doctor's two tables carrying the same marked `>=0` — the
/// "an absent harness is not an empty one" sentence is spoken by the
/// marking plus the unknown presence, not by faking the zero as a family
/// total.
///
/// This is the exact shape the `windows-latest` CI run caught through the
/// shipped registry (claude-code on a bare runner: `footprint session count
/// must be 'unknown', not Some(0)`); the shipped desktop cell is
/// `unascertained` on Windows only, so the shipped-registry form of it is
/// unreachable on macOS and Linux. The replay makes the *invariant* — the
/// floor is always carried with its qualification — run on every platform,
/// so the suite cannot again be green everywhere except one OS it never
/// exercised.
#[test]
fn windows_unverified_desktop_cell_empty_machine_keeps_the_floor_marked() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let base = tempfile::tempdir().unwrap();
    let home = windows_shaped_home(base.path());
    isolate_home(&home);
    write_windows_desktop_store_registry(&home);

    let report = doctor::run();
    assert!(!report.scan_failed, "scratch registry must load");

    let probe = report
        .probes
        .iter()
        .find(|p| p.id == "claude-code")
        .expect("registry probe row for claude-code");
    assert_eq!(
        probe.state,
        ProbeState::Missing,
        "the measured primary was found absent; note={}",
        probe.note
    );
    assert_eq!(
        probe.record_count,
        Some(0),
        "the measured zero is kept as the lower bound's floor, never a blanket unknown; note={}",
        probe.note
    );
    assert_eq!(
        probe
            .unverified_roots
            .iter()
            .map(|root| root.id.as_str())
            .collect::<Vec<_>>(),
        ["claude-desktop-cowork"],
        "the unverified sibling still qualifies the floor; note={}",
        probe.note
    );

    let footprint = report
        .footprints
        .iter()
        .find(|f| f.name == "claude-code")
        .expect("footprint row for claude-code");
    assert!(
        !footprint.installed,
        "nothing is installed; the row must not claim otherwise"
    );
    assert!(
        footprint.presence_unknown,
        "a family with an unverified root is partially unknown, not a measured 'not installed'"
    );
    assert_eq!(
        footprint.session_count, probe.record_count,
        "the two doctor tables must keep agreeing on the count (session_count={:?})",
        footprint.session_count
    );
    assert_eq!(
        footprint.unverified_roots,
        vec!["claude-desktop-cowork".to_string()],
        "the footprint row carries the same marking as the probe row"
    );
    assert!(
        footprint.note.contains("lower bound"),
        "the footprint note qualifies the floor too; note={}",
        footprint.note
    );

    std::env::remove_var("CHAT_STASHER_REGISTRY");
}

/// The registry the qualified-lower-bound test runs against: the shipped
/// claude-code Windows family's shape — a scannable primary beside an
/// `unascertained` desktop Cowork root — replayed for every platform so
/// the aggregate rule for it is exercised wherever the suite runs. The
/// primary cell uses the portable `~` template on purpose (the sibling
/// test above already replays the Windows *path* shapes); what is pinned
/// here is the aggregate: a measured root beside a root the scan refuses
/// to guess at. The desktop cell keeps the shipped `%APPDATA%` template —
/// its `unascertained` confidence is what decides the skip, before any
/// path is ever resolved.
fn write_windows_desktop_store_registry(home: &Path) -> PathBuf {
    let primary = r#"{ "template": "~/.claude/projects",
                      "format": "jsonl", "confidence": "official-docs",
                      "source": "W1006d test: claude-code primary cell, portable replay" }"#;
    let desktop = r#"{ "template": "%APPDATA%\\Claude\\local-agent-mode-sessions\\",
                      "format": "jsonl", "confidence": "unascertained",
                      "source": "W1006d test: claude-desktop-cowork.windows cell of shipped registry" }"#;
    let path = home.join("registry.json");
    fs::write(
        &path,
        format!(
            r#"{{ "schema_version": 1, "generated": "W1006d",
                  "harnesses": [
                    {{ "id": "claude-code", "display_name": "Claude Code",
                       "paths": {{ "macos": {primary}, "linux": {primary}, "windows": {primary} }},
                       "source_roots": [
                         {{ "id": "claude-desktop-cowork",
                            "paths": {{ "macos": {desktop}, "linux": {desktop}, "windows": {desktop} }} }}
                       ] }}
                  ] }}"#
        ),
    )
    .unwrap();
    std::env::set_var("CHAT_STASHER_REGISTRY", &path);
    path
}
