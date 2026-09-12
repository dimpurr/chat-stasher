//! B100 — a SQLite store's whole-file fingerprint must not be part of one
//! session's identity.
//!
//! `OpenCodeCursor` used to carry `store_fingerprint` as its first field, and
//! `collect` decided "this session did not change" with `old.opencode ==
//! Some(&cursor)`. So one write anywhere in `opencode.db` — even to a table no
//! session reads — made **every** session compare unequal, and every session
//! re-exported a fresh shard whose bytes were identical to the one already
//! staged. Measured before the fix: 346 sessions re-exported in a single pass.
//!
//! The fingerprint is still useful, but only as a *fast path*: identical
//! fingerprint means nothing in the store moved, so no session can have moved
//! either. When it differs, the decision must fall back to the per-session
//! fields (`session_time_updated`, row counts, `(time_updated, id)` high-water
//! marks) — never to the store-wide string.
//!
//! This file pins both halves of the fix plus the migration:
//!
//!   * `only_the_session_whose_rows_changed_gets_a_new_shard` — A and B, only A
//!     touched, store fingerprint moved. Fails before the fix (both re-export).
//!   * `a_store_write_that_changes_no_session_writes_no_shard` — only an
//!     unrelated table is written. Fails before the fix (both re-export).
//!   * `a_legacy_state_file_does_not_force_a_reexport` — a state file in the
//!     pre-change shape must not turn every session into "changed".
//!   * `a_cursor_that_advanced_over_identical_bytes_writes_no_shard` — the
//!     safety net: identical export bytes must not become a second shard.
//!     Fails before the fix.
//!   * `an_unavailable_previous_export_hash_still_writes_a_shard` — the same
//!     net, safe direction: an unreadable previous hash is *not* "identical".
//!
//! Everything runs in `tempfile` scratch directories against a synthetic store
//! built by this file. No real harness store is read, no network is touched,
//! and assertions look only at shard counts, concatenated bytes and cursors.

use chat_stasher::collect;
use chat_stasher::collect::{CollectReport, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry, ScanReport};
use chat_stasher::sqlite_probe;
use chat_stasher::store;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const SESSION_A: &str = "session-a";
const SESSION_B: &str = "session-b";
const MACHINE: &str = "fixture-machine";
const DESTINATION: &str = "fixture-destination";
const STATE_FILE: &str = "debts-v2.json";

/// The fixture harness is declared exactly like the real registry cell for
/// `opencode`, minus `env_override`: a test must not be steered by an ambient
/// `OPENCODE_DB`, so the configured root is the only thing that can resolve.
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

/// These tests only exercise the local read path, so the destination archive is
/// deliberately unreachable: a debt still owed on the stage must be provable
/// without ever asking it.
fn dest<'a>() -> DestinationView<'a> {
    DestinationView::unreachable(DESTINATION)
}

struct Fixture {
    _dir: tempfile::TempDir,
    db: PathBuf,
    stage: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("opencode.db");
        create_store(&db);
        Fixture {
            db,
            stage: dir.path().join("stage"),
            state: dir.path().join("state"),
            _dir: dir,
        }
    }

    fn scan(&self) -> ScanReport {
        let mut roots = BTreeMap::new();
        roots.insert(
            "opencode".to_string(),
            self.db.to_string_lossy().into_owned(),
        );
        let config = Config {
            harness_roots: roots,
            ..Config::default()
        };
        scanner::scan_with_registry(&config, &sqlite_registry()).unwrap()
    }

    fn collect(&self) -> CollectReport {
        collect::collect_scan_report(&self.scan(), &self.stage, MACHINE, &self.state, 20, &dest())
            .unwrap()
    }

    /// The whole-store fingerprint the fix must stop treating as identity. It
    /// is asserted on directly in every test that claims the store moved, so a
    /// premise that silently stopped holding shows up as a failure rather than
    /// as a test that passes for the wrong reason.
    fn fingerprint(&self) -> String {
        sqlite_probe::sqlite_store_fingerprint(&self.db).unwrap()
    }

    fn session_id(&self, native: &str) -> String {
        let suffix = format!(".{native}");
        self.scan()
            .records
            .iter()
            .map(|record| record.id.clone())
            .find(|id| id.ends_with(&suffix))
            .unwrap_or_else(|| panic!("scan produced no record for {native}"))
    }

    fn concat(&self, session_id: &str) -> Vec<u8> {
        store::concat_shards(&self.stage, MACHINE, session_id).unwrap()
    }

    fn shard_count(&self, session_id: &str) -> usize {
        let dir = store::session_shard_dir(&self.stage, MACHINE, session_id);
        store::sealed_shard_entries(&dir).unwrap().len()
    }

    fn state_path(&self) -> PathBuf {
        self.state.join(STATE_FILE)
    }

    fn read_state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.state_path()).unwrap()).unwrap()
    }
}

fn create_store(db: &Path) {
    let conn = Connection::open(db).unwrap();
    conn.execute_batch(
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
         -- A table no session export reads. Writing here moves the whole-store
         -- fingerprint without touching any session, which is exactly the case
         -- the fast path and the per-session comparison have to separate.
         CREATE TABLE unrelated (id INTEGER PRIMARY KEY, note TEXT);",
    )
    .unwrap();
    insert_session(&conn, SESSION_A, 1_000);
    insert_session(&conn, SESSION_B, 2_000);
}

fn insert_session(conn: &Connection, session: &str, base: i64) {
    conn.execute(
        "INSERT INTO session (id, time_created, time_updated, title) VALUES (?1, ?2, ?2, ?3)",
        rusqlite::params![session, base, format!("fixture {session}")],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?3, ?4)",
        rusqlite::params![
            format!("{session}-msg-1"),
            session,
            base + 10,
            r#"{"role":"user","text":"first"}"#
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, session_id, message_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        rusqlite::params![
            format!("{session}-part-1"),
            session,
            format!("{session}-msg-1"),
            base + 20,
            r#"{"text":"first"}"#
        ],
    )
    .unwrap();
}

/// Change session A only: its exported JSON line really is different bytes.
fn change_session_a(db: &Path) {
    let conn = Connection::open(db).unwrap();
    conn.execute(
        "UPDATE session SET time_updated = time_updated + 5 WHERE id = ?1",
        [SESSION_A],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, session_id, message_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, 1200, 1200, ?4)",
        rusqlite::params![
            format!("{SESSION_A}-part-2"),
            SESSION_A,
            format!("{SESSION_A}-msg-1"),
            r#"{"text":"second"}"#
        ],
    )
    .unwrap();
}

/// Write to the store without touching any session's rows.
fn write_unrelated(db: &Path) {
    let conn = Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO unrelated (id, note) VALUES (1, 'a table no session export reads')",
        [],
    )
    .unwrap();
}

/// Hand-edit one session's stored entry. The fixture writes the *old* shape, so
/// this leaves `shards` untouched and the entry stays verifiable against the
/// stage — the edit only changes what the next pass believes it already read.
fn edit_state(fx: &Fixture, native: &str, edit: impl FnOnce(&mut Value)) {
    let mut state = fx.read_state();
    let entry = state_entry_mut(&mut state, native);
    edit(entry);
    fs::write(fx.state_path(), serde_json::to_vec(&state).unwrap()).unwrap();
}

fn state_entry_mut<'a>(state: &'a mut Value, native: &str) -> &'a mut Value {
    let suffix = format!(".{native}");
    state["destinations"][DESTINATION]["files"]
        .as_object_mut()
        .expect("state holds a files map")
        .values_mut()
        .find(|entry| {
            entry["session_id"]
                .as_str()
                .is_some_and(|id| id.ends_with(&suffix))
        })
        .unwrap_or_else(|| panic!("state holds no entry for {native}"))
}

fn state_entry<'a>(state: &'a Value, native: &str) -> &'a Value {
    let suffix = format!(".{native}");
    state["destinations"][DESTINATION]["files"]
        .as_object()
        .expect("state holds a files map")
        .values()
        .find(|entry| {
            entry["session_id"]
                .as_str()
                .is_some_and(|id| id.ends_with(&suffix))
        })
        .unwrap_or_else(|| panic!("state holds no entry for {native}"))
}

/// A and B are both staged on the first pass. Then only A's rows change, which
/// moves the whole-store fingerprint. Only A may produce a new shard; B's
/// staged bytes must not move a single byte.
#[test]
fn only_the_session_whose_rows_changed_gets_a_new_shard() {
    let fx = Fixture::new();
    let first = fx.collect();
    assert_eq!(
        first.shards_written, 2,
        "both sessions are new to the stage, so each is staged exactly once"
    );
    let id_a = fx.session_id(SESSION_A);
    let id_b = fx.session_id(SESSION_B);
    let a_before = fx.concat(&id_a);
    let b_before = fx.concat(&id_b);
    let fingerprint_before = fx.fingerprint();

    change_session_a(&fx.db);
    assert_ne!(
        fx.fingerprint(),
        fingerprint_before,
        "premise: a write to the store must move the whole-store fingerprint"
    );

    let second = fx.collect();
    assert_eq!(
        second.shards_written, 1,
        "only session A changed; B must not be re-exported just because the store was written"
    );
    assert_eq!(fx.shard_count(&id_a), 2, "A's change seals one more shard");
    assert_eq!(fx.shard_count(&id_b), 1, "B gains no shard");
    assert_eq!(fx.concat(&id_b), b_before, "B's staged bytes are untouched");
    let a_after = fx.concat(&id_a);
    assert!(
        a_after.starts_with(&a_before) && a_after.len() > a_before.len(),
        "the archive is append-only: A's previous shard is a prefix of the new concatenation"
    );
    println!(
        "b100 session_identity first_shards={} second_shards={} a_shards={} b_shards={} a_bytes_before={} a_bytes_after={} b_bytes={}",
        first.shards_written,
        second.shards_written,
        fx.shard_count(&id_a),
        fx.shard_count(&id_b),
        a_before.len(),
        a_after.len(),
        b_before.len()
    );
}

/// The store is written, but nothing any session export reads has changed. No
/// session may produce a shard, and every session must still be reported as
/// unchanged rather than re-read and re-written.
#[test]
fn a_store_write_that_changes_no_session_writes_no_shard() {
    let fx = Fixture::new();
    let first = fx.collect();
    assert_eq!(first.shards_written, 2);
    let id_a = fx.session_id(SESSION_A);
    let id_b = fx.session_id(SESSION_B);
    let a_before = fx.concat(&id_a);
    let b_before = fx.concat(&id_b);
    let fingerprint_before = fx.fingerprint();

    write_unrelated(&fx.db);
    assert_ne!(
        fx.fingerprint(),
        fingerprint_before,
        "premise: the unrelated write must move the whole-store fingerprint"
    );

    let second = fx.collect();
    assert_eq!(
        second.shards_written, 0,
        "a store write that changes no session must not re-export any session"
    );
    assert_eq!(second.changed_records, 0);
    assert_eq!(second.unchanged_records, 2);
    assert_eq!(fx.shard_count(&id_a), 1);
    assert_eq!(fx.shard_count(&id_b), 1);
    assert_eq!(fx.concat(&id_a), a_before);
    assert_eq!(fx.concat(&id_b), b_before);
    println!(
        "b100 store_only_write shards={} changed={} unchanged={} reset={} stage_bytes={}",
        second.shards_written,
        second.changed_records,
        second.unchanged_records,
        second.reset_records,
        a_before.len() + b_before.len()
    );
}

/// A state file written before the change keeps the whole-store fingerprint
/// *inside* the session cursor and has no entry-level field. That file must be
/// read without discarding the cursors: every stored session cursor field
/// survives, so the per-session comparison still says "unchanged".
///
/// The pass after that writes the current shape, so the fast path is available
/// again from the second run onwards — one pass of lost shortcut, never a
/// re-export.
#[test]
fn a_legacy_state_file_does_not_force_a_reexport() {
    let fx = Fixture::new();
    let first = fx.collect();
    assert_eq!(first.shards_written, 2);
    let id_a = fx.session_id(SESSION_A);
    let id_b = fx.session_id(SESSION_B);
    let a_before = fx.concat(&id_a);
    let b_before = fx.concat(&id_b);

    // Rewrite the state into the pre-change shape: the store fingerprint nested
    // inside the session cursor (`cursor.opencode.store_fingerprint`) instead of
    // beside it (`cursor.store_fingerprint`). Before the change this is a no-op
    // (the two shapes are the same file); after it, this is the migration
    // fixture. Either way the *premise* asserted below has to hold, so the test
    // cannot pass by having quietly failed to build the shape it claims.
    let mut state = fx.read_state();
    let mut rewritten = 0;
    for entry in state["destinations"][DESTINATION]["files"]
        .as_object_mut()
        .expect("state holds a files map")
        .values_mut()
    {
        let nested = entry["cursor"]["store_fingerprint"].take();
        if nested.is_string() {
            entry["cursor"]["opencode"]["store_fingerprint"] = nested;
            entry["cursor"]
                .as_object_mut()
                .unwrap()
                .remove("store_fingerprint");
        }
        rewritten += 1;
    }
    assert_eq!(rewritten, 2, "both sessions carry a stored cursor");
    let legacy = serde_json::to_vec(&state).unwrap();
    fs::write(fx.state_path(), &legacy).unwrap();

    let reread = fx.read_state();
    for native in [SESSION_A, SESSION_B] {
        let entry = state_entry(&reread, native);
        assert!(
            entry["cursor"]["store_fingerprint"].is_null(),
            "{native}: the legacy fixture must have no entry-level store fingerprint"
        );
        assert!(
            entry["cursor"]["opencode"]["store_fingerprint"].is_string(),
            "{native}: the legacy fixture must carry the fingerprint inside the cursor"
        );
    }

    let second = fx.collect();
    assert_eq!(
        second.shards_written, 0,
        "a legacy-shaped state file must not turn unchanged sessions into re-exports"
    );
    assert_eq!(second.reset_records, 0);
    assert_eq!(second.unchanged_records, 2);
    assert_eq!(fx.concat(&id_a), a_before);
    assert_eq!(fx.concat(&id_b), b_before);

    // The same pass repaired the shape: the next run has the fast path again.
    let repaired = fx.read_state();
    for native in [SESSION_A, SESSION_B] {
        assert!(
            state_entry(&repaired, native)["cursor"]["store_fingerprint"].is_string(),
            "{native}: the pass must record the whole-store fingerprint at the entry level"
        );
    }
    let third = fx.collect();
    assert_eq!(third.shards_written, 0);
    println!(
        "b100 legacy_state legacy_bytes={} second_shards={} third_shards={} unchanged={}",
        legacy.len(),
        second.shards_written,
        third.shards_written,
        second.unchanged_records
    );
}

/// The safety net, positive direction: the session cursor moved, so the pass
/// does read the session — and the bytes it would write are byte-for-byte the
/// bytes already staged. No second shard may appear; the cursor still advances.
#[test]
fn a_cursor_that_advanced_over_identical_bytes_writes_no_shard() {
    let fx = Fixture::new();
    let first = fx.collect();
    assert_eq!(first.shards_written, 2);
    let id_a = fx.session_id(SESSION_A);
    let id_b = fx.session_id(SESSION_B);
    let a_before = fx.concat(&id_a);
    let b_before = fx.concat(&id_b);
    let live = sqlite_probe::opencode_session_cursor(&fx.db, SESSION_A).unwrap();

    // Move the *stored* cursor ahead of the live one so the per-session
    // comparison has to say "changed" and the session is read for real.
    edit_state(&fx, SESSION_A, |entry| {
        entry["cursor"]["opencode"]["session_time_updated"] = json!(live.session_time_updated + 1);
    });
    write_unrelated(&fx.db);

    let second = fx.collect();
    assert_eq!(
        second.shards_written, 0,
        "identical export bytes must not become a second shard"
    );
    assert_eq!(fx.shard_count(&id_a), 1);
    assert_eq!(fx.shard_count(&id_b), 1);
    assert_eq!(fx.concat(&id_a), a_before);
    assert_eq!(fx.concat(&id_b), b_before);

    // Bookkeeping still happens: skipping the write must not skip the cursor.
    let stored = fx.read_state();
    assert_eq!(
        state_entry(&stored, SESSION_A)["cursor"]["opencode"]["session_time_updated"].as_i64(),
        Some(live.session_time_updated),
        "the stored cursor must advance to the value the pass just read"
    );
    println!(
        "b100 content_net_same shards={} a_shards={} a_bytes={} advanced_to_live={}",
        second.shards_written,
        fx.shard_count(&id_a),
        a_before.len(),
        live.session_time_updated
    );
}

/// The safety net, safe direction: when the previous export hash is not
/// available, "we cannot tell" must not be read as "identical". The shard is
/// written exactly as before the net existed.
#[test]
fn an_unavailable_previous_export_hash_still_writes_a_shard() {
    let fx = Fixture::new();
    let first = fx.collect();
    assert_eq!(first.shards_written, 2);
    let id_a = fx.session_id(SESSION_A);
    let a_before = fx.concat(&id_a);
    let live = sqlite_probe::opencode_session_cursor(&fx.db, SESSION_A).unwrap();

    edit_state(&fx, SESSION_A, |entry| {
        entry["cursor"]["opencode"]["session_time_updated"] = json!(live.session_time_updated + 1);
        // An absent / unreadable previous hash, not a hash that happens to
        // match: the net has nothing to compare against.
        entry["cursor"]["prefix_sha256"] = json!("");
    });
    write_unrelated(&fx.db);

    let second = fx.collect();
    assert_eq!(
        second.shards_written, 1,
        "an unavailable previous hash must still produce a shard"
    );
    assert_eq!(fx.shard_count(&id_a), 2);
    let a_after = fx.concat(&id_a);
    assert!(
        a_after.starts_with(&a_before) && a_after.len() > a_before.len(),
        "the write is a normal append"
    );
    println!(
        "b100 content_net_unavailable shards={} a_shards={} a_bytes_before={} a_bytes_after={}",
        second.shards_written,
        fx.shard_count(&id_a),
        a_before.len(),
        a_after.len()
    );
}
