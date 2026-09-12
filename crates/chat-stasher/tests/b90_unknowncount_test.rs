//! B90 — "Uncountable" rendered as a concrete number.
//!
//! The strictest invariant in this repository is "never treat unknown as empty".
//! B90 addresses its **counting** variant:
//! an uncountable quantity becomes `0` / `0 days` on screen, so readers cannot
//! distinguish it from "genuinely zero". This file covers the two cases reproducible
//! end-to-end:
//!
//!   * **B** — `doctor`'s Claude Code risk line. When `earliest` is `None`, date
//!     honestly prints `n/a`, but days silently become `0.0`, resulting in half
//!     honest and half fabricated statements in the same sentence, and the fabricated
//!     half is a **false alarm prompting immediate action**: "only about 0 days left".
//!   * **C** — `status --sessions` mtime column. An unavailable / pre-epoch mtime
//!     rendered as `0`, indistinguishable from "genuinely equal to 1970-01-01". This
//!     is the exact bug just fixed in `inbox.rs`, but not yet swept in this table.
//!
//! (A — `sqlite_probe::unreadable_candidate_count` reporting `0` when itself unreadable —
//! requires making "opening the same db a second time" fail, which cannot be deterministically
//! reproduced end-to-end in a single process; disproof tests reside in unit tests of
//! `src/sqlite_probe.rs` and `src/doctor.rs`.)
//!
//! Everything runs strictly in `tempfile` scratch directories: HOME / XDG_* /
//! CHAT_STASHER_REGISTRY / CURSOR_USER_DIR are all diverted into the sandbox, never
//! touching real harness directories and never reading session contents.

use rusqlite::Connection;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// `doctor::run()` relies on process-level environment variables, whereas cargo runs tests in parallel in the same process.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn isolate_home(home: &Path) {
    std::env::set_var("HOME", home);
    std::env::set_var("USERPROFILE", home);
    std::env::set_var("XDG_DATA_HOME", home.join("xdg-data"));
    std::env::set_var("XDG_CONFIG_HOME", home.join("xdg-config"));
    std::env::set_var("XDG_STATE_HOME", home.join("xdg-state"));
    for var in [
        "CODEX_HOME",
        "GEMINI_CLI_HOME",
        "CURSOR_USER_DIR",
        "OPENCODE_DB",
        "CHAT_STASHER_REGISTRY",
    ] {
        std::env::remove_var(var);
    }
}

// ---------------------------------------------------------------------------
// B — Half honest, half fabricated in the same sentence
// ---------------------------------------------------------------------------

/// Extract the Claude Code risk line.
fn claude_risk(report: &chat_stasher::doctor::DoctorReport) -> String {
    report
        .risks
        .iter()
        .find(|line| line.contains("Claude Code"))
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "doctor did not output any Claude Code risk line: {:?}",
                report.risks
            )
        })
}

/// **Disproof of B.** On a machine without any Claude sessions: `earliest` is `None`,
/// so "earliest session is n/a" is honest, while "about 0 days ago" / "only about 0 days left" are fabricated.
/// Before the fix, this assertion must fail — it asserts that the fabricated number must not appear.
#[test]
fn claude_risk_never_invents_a_day_count_it_does_not_have() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let sandbox = tempfile::tempdir().expect("sandbox");
    isolate_home(sandbox.path());

    let report = chat_stasher::doctor::run();
    let line = claude_risk(&report);

    assert!(
        !line.contains("0 days ago"),
        "fabricated 'about 0 days ago' must not be printed when earliest session is unknown; actual output:\n{line}"
    );
    assert!(
        !line.contains("only about 0 days left"),
        "'only about 0 days left' is a false alarm prompting immediate action; actual output:\n{line}"
    );
}

/// After removing the fabricated number, this line must not degrade into silence: the risk itself (cleanupPeriodDays
/// unset -> default 30 days) still exists, only the day count is stated as "unknown".
#[test]
fn claude_risk_still_says_the_retention_risk_and_names_the_unknown() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let sandbox = tempfile::tempdir().expect("sandbox");
    isolate_home(sandbox.path());

    let report = chat_stasher::doctor::run();
    let line = claude_risk(&report);

    assert!(
        line.contains("cleanupPeriodDays is unset"),
        "the risk itself did not disappear and must still be stated; actual output:\n{line}"
    );
    assert!(
        line.contains("unknown"),
        "when day count cannot be determined it must explicitly state 'unknown' rather than staying silent; actual output:\n{line}"
    );
}

/// Dignified failure path: on a machine **with** sessions, this line must still give the real date and day count.
/// This also serves as evidence that 'it stays quiet on healthy machines' — the unknown branch does not pollute the known branch.
#[test]
fn a_machine_with_sessions_still_gets_a_real_date_and_a_real_day_count() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let sandbox = tempfile::tempdir().expect("sandbox");
    isolate_home(sandbox.path());
    // A realistically-shaped claude-code session file, mtime is now.
    let projects = sandbox.path().join(".claude").join("projects").join("p");
    fs::create_dir_all(&projects).expect("create claude projects dir");
    fs::write(projects.join("s.jsonl"), "{}\n").expect("write session file");

    let report = chat_stasher::doctor::run();
    let line = claude_risk(&report);

    assert!(
        !line.contains("n/a"),
        "date must be real when sessions exist; actual output:\n{line}"
    );
    assert!(
        line.contains("about 0 days ago"),
        "day count must still be printed when sessions exist (freshly written file is 0 days ago); actual output:\n{line}"
    );
}

// ---------------------------------------------------------------------------
// C — status --sessions mtime column
// ---------------------------------------------------------------------------

/// A Cursor composer with negative `createdAt`: its mtime predates the epoch,
/// `duration_since(UNIX_EPOCH)` fails, and old code rendered it as `0`.
const PRE_EPOCH_ROW: &str =
    r#"{"composerId":"a","createdAt":-1000,"fullConversationHeadersOnly":[{"bubbleId":"b"}]}"#;
/// An equally valid entry but with normal timestamp, used to prove normal row display is not broken.
const NORMAL_ROW: &str = r#"{"composerId":"b","createdAt":1760000000000,"fullConversationHeadersOnly":[{"bubbleId":"b"}]}"#;

fn plant_rows(user_dir: &Path, rows: &[(&str, &str)]) {
    let db = user_dir.join("globalStorage").join("state.vscdb");
    fs::create_dir_all(db.parent().expect("db path has a parent")).expect("create globalStorage");
    let conn = Connection::open(&db).expect("open fixture store");
    conn.execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB)")
        .expect("create fixture table");
    for (key, value) in rows {
        conn.execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
        )
        .expect("insert fixture row");
    }
    drop(conn);
}

/// A single-harness registry with the same Cursor cell in all three platform slots,
/// allowing the fixture to execute the same code path on any OS.
fn write_cursor_registry(base: &Path) -> PathBuf {
    let cursor = r#"{ "template": "~/Library/Application Support/Cursor/User/globalStorage/state.vscdb",
                      "env_override": "CURSOR_USER_DIR", "format": "sqlite",
                      "confidence": "measured-locally", "source": "B90 test fixture",
                      "sql_table": "cursorDiskKV", "sql_id_column": "key",
                      "sql_required_columns": ["key", "value"],
                      "sql_key_column": "key", "sql_key_pattern": "composerData:%",
                      "sql_value_column": "value", "sql_time_json_path": "$.createdAt",
                      "sql_qualification": "cursor_composer" }"#;
    let path = base.join("registry.json");
    fs::write(
        &path,
        format!(
            r#"{{ "schema_version": 1, "generated": "B90",
                  "harnesses": [
                    {{ "id": "cursor", "display_name": "Cursor",
                       "paths": {{ "macos": {cursor}, "linux": {cursor}, "windows": {cursor} }} }}
                  ] }}"#
        ),
    )
    .expect("write fixture registry");
    path
}

/// Body of `status --sessions` (stripping wall-clock content from the `[run-once]` line).
fn run_status_sessions(sandbox: &Path, rows: &[(&str, &str)]) -> String {
    let home = sandbox.join("home");
    let user_dir = home.join("Cursor").join("User");
    fs::create_dir_all(&home).expect("create sandbox home");
    plant_rows(&user_dir, rows);
    let registry = write_cursor_registry(sandbox);

    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .arg("status")
        .arg("--sessions")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .env("CURSOR_USER_DIR", &user_dir)
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .output()
        .expect("run status --sessions");

    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.starts_with("[run-once]"))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// All values in the mtime column of the table.
fn mtime_column(body: &str) -> Vec<String> {
    body.lines()
        .filter(|line| line.trim_start().starts_with("cursor "))
        .filter(|line| !line.contains("sessions :"))
        .filter_map(|line| line.split_whitespace().nth(2).map(str::to_string))
        .collect()
}

/// **Disproof of C.** A pre-epoch mtime rendered as `0` in the table is indistinguishable
/// from "genuinely equal to epoch". Before the fix this assertion must fail.
#[test]
fn status_sessions_never_prints_an_unknown_mtime_as_zero() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let body = run_status_sessions(sandbox.path(), &[("composerData:aaa", PRE_EPOCH_ROW)]);
    let column = mtime_column(&body);

    assert_eq!(
        column.len(),
        1,
        "fixture should produce exactly one row; actual output:\n{body}"
    );
    assert_ne!(
        column[0], "0",
        "unavailable / pre-epoch mtime must not be rendered as 0 — indistinguishable from 'genuinely equal to 1970-01-01'; actual output:\n{body}"
    );
    assert!(
        column[0].contains("unknown"),
        "it should be displayed as 'unknown'; actual output:\n{body}"
    );
}

/// "It does not ring" on healthy machines: a row with a normal timestamp prints seconds as usual, without extra words.
#[test]
fn a_readable_mtime_still_prints_its_seconds() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let body = run_status_sessions(sandbox.path(), &[("composerData:bbb", NORMAL_ROW)]);
    let column = mtime_column(&body);

    assert_eq!(
        column,
        vec!["1760000000".to_string()],
        "actual output:\n{body}"
    );
    assert!(
        !body.contains("unknown"),
        "when nothing is unknown, 'unknown' must not appear in output; actual output:\n{body}"
    );
}
