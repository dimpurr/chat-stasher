//! B78 — `status` had to learn to say the word `doctor` already knew.
//!
//! B68 gave the Cursor probe a third answer to "where did my sessions go":
//! not "genuinely none" and not "filtered out" but **"it claims to exist, but cannot be read"**
//! (`HarnessProbe::unreadable_count`, per harness, `scanner.rs`).
//! `doctor` prints it:
//!
//! ```text
//! cursor  sessions 3 (before filter 414 / after filter 3, 411 unreadable)
//! ```
//!
//! `status` — the command a non-terminal-dweller actually runs — did not:
//!
//! ```text
//! [scan] 491 session(s) (0 compressed): claude-code 115 · codex 107 · cursor 3 · ...
//! ```
//!
//! So "411 sessions did not make it in" was true, counted, and invisible unless you knew to run
//! a second command.
//!
//! The other half of the constraint is that `status`'s default body was
//! deliberately shrunk from 463 lines to a handful, and must not creep back.
//! Hence the shape of these tests: the sentence is appended to the **existing**
//! `[scan]` line, and when nothing is unreadable the output is asserted
//! byte-identical by sha256 — not by line count, which would not notice a
//! reworded line.
//!
//! Everything below runs the real binary against a `tempfile` Cursor store with
//! `HOME`/`XDG_*`/`CHAT_STASHER_REGISTRY`/`CURSOR_USER_DIR` all redirected into
//! the sandbox. No real harness directory, archive or remote is touched, and no
//! session body is ever read by the test itself.

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The pre-B78 sha256 of the "nothing unreadable" status body, recorded from
/// the binary built at commit fbc2281 (before this change). The point of
/// pinning the digest rather than the line count: the guard has to fail on a
/// reworded line, an added space, or a stray marker — not only on an added row.
const CLEAN_STATUS_BODY_SHA256: &str =
    "16e84a41b7f9d35783e90c317dd98c7218c49dd9a2cf2cb6dbbcc32135bc873e";

/// The pre-B78 sha256 of the same body on a machine with nothing scanned at
/// all, recorded the same way. (Both digests were re-recorded once when the
/// user-visible output was translated to English; the byte-for-byte contract
/// they pin is unchanged.)
const EMPTY_STATUS_BODY_SHA256: &str =
    "699a80733a023719bf0d82bc275d44db697d8da28df4eea9a125d9304cbd6cb1";

// ---------------------------------------------------------------------------
// Fixtures — same shapes B68 pinned, planted through rusqlite
// ---------------------------------------------------------------------------

const QUALIFIED_ROW: &str = r#"{"composerId":"a","createdAt":1760000000000,"fullConversationHeadersOnly":[{"bubbleId":"b"}]}"#;
const GENUINELY_EMPTY_ROW: &str =
    r#"{"composerId":"d","createdAt":1760000000000,"fullConversationHeadersOnly":[]}"#;

/// A Cursor store with no rows at all: the scan finds nothing, `status` takes
/// its "No sessions were found on this machine." branch — the other line B78 touched.
fn plant_empty_cursor_store(user_dir: &Path) {
    plant_rows(user_dir, &[]);
}

/// A Cursor global store whose every dropped row was *read* and found empty:
/// `unreadable_count == 0`, so `status` must not add a single byte.
fn plant_clean_cursor_store(user_dir: &Path) {
    plant_rows(
        user_dir,
        &[
            ("composerData:aaa", Some(QUALIFIED_ROW)),
            ("composerData:ddd", Some(GENUINELY_EMPTY_ROW)),
        ],
    );
}

/// The B68 headline store: one row whose value will not decode at all, and one
/// qualified row whose native id is empty so enumeration skips it. Both are
/// counted by the probe and neither becomes a `SessionRecord` —
/// `unreadable_count == 2`.
fn plant_unreadable_cursor_store(user_dir: &Path) {
    plant_rows(
        user_dir,
        &[
            ("composerData:aaa", Some(QUALIFIED_ROW)),
            ("composerData:", Some(QUALIFIED_ROW)),
            ("composerData:ccc", None),
            ("composerData:ddd", Some(GENUINELY_EMPTY_ROW)),
        ],
    );
}

fn plant_rows(user_dir: &Path, rows: &[(&str, Option<&str>)]) {
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

/// A one-harness registry, the shipped macOS Cursor cell in all three platform
/// slots so the fixture takes the same code path on every OS.
fn write_cursor_registry(base: &Path) -> PathBuf {
    let cursor = r#"{ "template": "~/Library/Application Support/Cursor/User/globalStorage/state.vscdb",
                      "env_override": "CURSOR_USER_DIR", "format": "sqlite",
                      "confidence": "measured-locally", "source": "B78 test fixture",
                      "sql_table": "cursorDiskKV", "sql_id_column": "key",
                      "sql_required_columns": ["key", "value"],
                      "sql_key_column": "key", "sql_key_pattern": "composerData:%",
                      "sql_value_column": "value", "sql_time_json_path": "$.createdAt",
                      "sql_qualification": "cursor_composer" }"#;
    let path = base.join("registry.json");
    fs::write(
        &path,
        format!(
            r#"{{ "schema_version": 1, "generated": "B78",
                  "harnesses": [
                    {{ "id": "cursor", "display_name": "Cursor",
                       "paths": {{ "macos": {cursor}, "linux": {cursor}, "windows": {cursor} }} }}
                  ] }}"#
        ),
    )
    .expect("write fixture registry");
    path
}

/// What `status` printed, minus the `[run-once]` verdict line.
///
/// That line carries "how long since the last run", which is wall-clock and
/// therefore not hashable; everything below it is the scan body this ticket is
/// about. Returned with the exit code, because B78 is not allowed to move it.
struct StatusRun {
    body: String,
    code: Option<i32>,
}

fn run_status(sandbox: &Path, plant: fn(&Path)) -> StatusRun {
    let home = sandbox.join("home");
    let user_dir = home.join("Cursor").join("User");
    fs::create_dir_all(&home).expect("create sandbox home");
    plant(&user_dir);
    let registry = write_cursor_registry(sandbox);

    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .arg("status")
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
        .expect("run status");

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let body = stderr
        .lines()
        .filter(|line| !line.starts_with("[run-once]"))
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    StatusRun {
        body,
        code: output.status.code(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

// ---------------------------------------------------------------------------
// The gap itself
// ---------------------------------------------------------------------------

/// **The B78 regression.** Two sessions the store knows about and this build
/// cannot hand over, and `status` used to print a line that mentioned neither
/// them nor the fact that a second command knows about them.
///
/// Reverting `unreadable_notice` in `main.rs` makes this assertion fail: the
/// `[scan]` line goes back to `cursor 2` with nothing after it.
#[test]
fn status_says_out_loud_that_some_sessions_could_not_be_read() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let run = run_status(sandbox.path(), plant_unreadable_cursor_store);

    assert!(
        run.body.contains("indexed but not archived"),
        "status must say out loud that the sessions are indexed but not archived, not leave it to doctor alone; actual output:\n{}",
        run.body
    );
    assert!(
        run.body.contains("2 session(s)"),
        "the count must land in that sentence; actual output:\n{}",
        run.body
    );
}

/// The wording half of the ticket: a reader must not be able to come away
/// thinking those sessions are safely in the archive. "another" says they are
/// not part of the count beside them; "not archived" says where they are not.
#[test]
fn the_sentence_cannot_be_read_as_already_archived() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let run = run_status(sandbox.path(), plant_unreadable_cursor_store);

    assert!(
        run.body.contains("another") && run.body.contains("not archived"),
        "the wording must make it clear at a glance that these sessions have not been archived; actual output:\n{}",
        run.body
    );
    assert!(
        run.body.contains("chat-stasher doctor"),
        "once it reports a problem it must say where to look for detail; actual output:\n{}",
        run.body
    );
}

/// The other hard constraint: `status`'s default body was shrunk from 463
/// lines to a handful and must not creep back. The notice rides on the
/// existing `[scan]` line, so this fixture prints exactly the number of lines
/// it printed before B78 — three, recorded from the pre-change binary:
///
/// ```text
/// [scan] 1 session(s) (0 compressed): cursor 1
/// ⚠ 1 harness(es) have recognised sessions that collect will not archive.
/// details (one line per session): chat-stasher status --sessions
/// ```
///
/// (The middle line is the pre-existing archive-gap notice, not B78's — which
/// is why this pins the fixture's own before/after count rather than comparing
/// against the clean fixture, whose gap line does not fire.)
#[test]
fn saying_it_costs_zero_extra_lines() {
    /// Line count of this fixture's status body at commit fbc2281.
    const PRE_B78_LINES: usize = 3;

    let clean_box = tempfile::tempdir().expect("sandbox");
    let dirty_box = tempfile::tempdir().expect("sandbox");
    let clean = run_status(clean_box.path(), plant_clean_cursor_store);
    let dirty = run_status(dirty_box.path(), plant_unreadable_cursor_store);

    assert_eq!(
        dirty.body.lines().count(),
        PRE_B78_LINES,
        "not allowed to add an extra line — status was just slimmed down from 463 to 4 lines, this sentence must ride on the [scan] line:\n{}",
        dirty.body
    );
    assert!(
        dirty
            .body
            .lines()
            .any(|line| line.starts_with("[scan]") && line.contains("indexed but not archived")),
        "the notice must ride inside the [scan] line, not stand on its own line:\n{}",
        dirty.body
    );
    assert_eq!(
        dirty.code, clean.code,
        "the exit code must not change because of this sentence; clean={:?} unreadable={:?}",
        clean.code, dirty.code
    );
}

/// **The byte-for-byte guard.** With nothing unreadable, `status` must print
/// what it printed before B78 — verified by digest, so a reworded line or a
/// single added space fails here.
#[test]
fn a_clean_machine_sees_a_byte_identical_status() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let run = run_status(sandbox.path(), plant_clean_cursor_store);

    assert!(
        !run.body.contains("indexed but not archived"),
        "with zero unreadable, not a single word may be added; actual output:\n{}",
        run.body
    );
    assert_eq!(
        sha256_hex(run.body.as_bytes()),
        CLEAN_STATUS_BODY_SHA256,
        "status must remain byte-identical when there are no unreadable sessions; actual output:\n{}",
        run.body
    );
}

/// The same byte-for-byte guard on the *other* branch B78 edited: a machine
/// with nothing to report at all. The empty-scan line was rewritten from a
/// literal into a `format!`, and this digest is what says the rewrite was a
/// no-op.
#[test]
fn an_empty_machine_sees_a_byte_identical_status() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let run = run_status(sandbox.path(), plant_empty_cursor_store);

    assert!(
        run.body.contains("No sessions were found on this machine"),
        "the fixture must really reach the empty-scan branch; actual output:\n{}",
        run.body
    );
    assert_eq!(
        sha256_hex(run.body.as_bytes()),
        EMPTY_STATUS_BODY_SHA256,
        "status on an empty machine must remain byte-identical; actual output:\n{}",
        run.body
    );
}
