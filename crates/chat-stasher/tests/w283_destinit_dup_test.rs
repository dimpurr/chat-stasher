//! W283 — a destination must be *given* the shards the stage already holds,
//! never be handed a second, byte-identical seal of them.
//!
//! The WIZ-5 real-machine gate (W281) measured the defect on `main @ b95b3d7`
//! and reproduced it without any network; its BUG-1 carries the repro and the
//! gate's evidence index names the file it is written in:
//!
//! | step | shard-seq |
//! |---|---|
//! | `run-once` once | 1 ✅ |
//! | `run-once` again | 1 ✅ (idempotent) |
//! | `dest-init` on a **fresh** destination | 2 ❌ |
//! | `dest-init` on a **second** fresh destination | 3 ❌ |
//!
//! The debt state is keyed by destination (`collect::destination_id`), so a
//! destination that has no entry starts every source at offset zero. The
//! collector then re-reads the whole file and *seals what it read*, even when
//! the stage already holds exactly those bytes — one more full copy of every
//! session per destination, forever.
//!
//! What that costs, measured on the repro: `read` returns the body twice
//! (`884` bytes where the source is `442`), `export` writes it twice, the
//! archive-derived activity index doubles `line_count`, and `verify --level l3`
//! says `OK` because L3 derives its expectation from the same sealed tree.
//!
//! These tests drive the collector directly — the pass `dest-init` step 1 and
//! `run-once` both call — so the mechanism is pinned without standing up a
//! rustic repository. The counter-case at the bottom is the one that makes the
//! fix non-trivial: *real* content may repeat, and repeated content must still
//! be sealed.

use chat_stasher::collect;
use chat_stasher::collect::DestinationView;
use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry};
use chat_stasher::store;
use serde_json::json;
use std::fs;
use std::path::Path;

const MACHINE: &str = "fixture-machine";

/// A one-harness registry whose only cell declares `format`. The scanner
/// matches a file to a cell by the format's suffix allowlist
/// (`strip_format_suffix`), so this is also what decides which of the three
/// seal paths — incremental jsonl, whole file, compressed — the fixture takes.
///
/// `pattern` is only needed for a generic JSON cell: `build_record` rejects a
/// cell whose format declares `.json` and which names no `session_pattern`, so
/// that settings/state JSON cannot enter the session count. That is the
/// scanner's rule, not this test's — it is stated here so the fixture does not
/// look like it is asking for something unusual.
fn registry(format: &str, pattern: Option<&str>) -> HarnessRegistry {
    let mut cell = json!({
        "template": "~/.claude/projects",
        "format": format,
        "confidence": "source-confirmed",
        "source": "synthetic fixture"
    });
    if let Some(pattern) = pattern {
        cell["session_pattern"] = json!(pattern);
    }
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

fn scan_as(root: &Path, format: &str, pattern: Option<&str>) -> scanner::ScanReport {
    let config = Config {
        claude_projects_dir: Some(root.to_string_lossy().into_owned()),
        ..Config::default()
    };
    scanner::scan_with_registry(&config, &registry(format, pattern)).unwrap()
}

/// A destination that cannot be consulted. Every pass here is the first pass
/// for its destination, which is exactly the state `dest-init` puts a brand-new
/// destination in: no debt entry, nothing to ask the archive.
fn dest(name: &'static str) -> DestinationView<'static> {
    DestinationView::unreachable(name)
}

/// One source file, a stage and a state dir, with the session id the scanner
/// gives that file.
struct Fixture {
    _dir: tempfile::TempDir,
    source_root: std::path::PathBuf,
    source: std::path::PathBuf,
    stage: std::path::PathBuf,
    state: std::path::PathBuf,
    id: String,
    format: &'static str,
    pattern: Option<&'static str>,
}

impl Fixture {
    fn new(body: &[u8]) -> Fixture {
        Fixture::named("session.jsonl", "jsonl", None, body)
    }

    /// The same fixture for a source whose *shape* differs — the collector
    /// seals a plain `.jsonl` incrementally, a whole file (`.json`) in one
    /// shard, and a `.jsonl.zst` from its decoded lines. All three add a shard
    /// on a destination's first pass, so all three double without the fix.
    fn named(
        name: &str,
        format: &'static str,
        pattern: Option<&'static str>,
        body: &[u8],
    ) -> Fixture {
        let dir = tempfile::TempDir::new().unwrap();
        let source_root = dir.path().join("source");
        fs::create_dir_all(&source_root).unwrap();
        let source = source_root.join(name);
        fs::write(&source, body).unwrap();
        let id = scan_as(&source_root, format, pattern).records[0].id.clone();
        Fixture {
            source_root,
            source,
            stage: dir.path().join("stage"),
            state: dir.path().join("state"),
            id,
            format,
            pattern,
            _dir: dir,
        }
    }

    fn scan(&self) -> scanner::ScanReport {
        scan_as(&self.source_root, self.format, self.pattern)
    }

    fn collect(&self, destination: &'static str) {
        collect::collect_scan_report(
            &self.scan(),
            &self.stage,
            MACHINE,
            &self.state,
            20,
            &dest(destination),
        )
        .unwrap();
    }

    fn concat(&self) -> Vec<u8> {
        store::concat_shards(&self.stage, MACHINE, &self.id).unwrap()
    }

    fn shard_count(&self) -> usize {
        store::sealed_shard_count(&self.stage).unwrap()
    }

    fn append(&self, bytes: &[u8]) {
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&self.source)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }
}

/// The reproduction, at the level the defect lives: one destination seals the
/// source; every further destination must find it already sealed.
#[test]
fn a_fresh_destination_does_not_reseal_shards_the_stage_already_holds() {
    let fx = Fixture::new(b"one\ntwo\nthree\n");

    // Pass 1 — the first destination reads the source in full. One shard,
    // byte-for-byte the source.
    fx.collect("first");
    assert_eq!(fx.shard_count(), 1);
    assert_eq!(fx.concat(), b"one\ntwo\nthree\n");

    // Pass 2 — a second destination, with no record of its own. The stage
    // already holds this session's bytes, so this pass has nothing to seal.
    let second = collect::collect_scan_report(
        &fx.scan(),
        &fx.stage,
        MACHINE,
        &fx.state,
        20,
        &dest("second"),
    )
    .unwrap();
    assert_eq!(
        fx.shard_count(),
        1,
        "a second destination must not append a shard the stage already holds"
    );
    assert_eq!(
        fx.concat(),
        b"one\ntwo\nthree\n",
        "the archived body must stay the conversation once, not twice"
    );
    assert_eq!(
        second.lines_written, 0,
        "nothing new was read, so nothing may be written"
    );
    assert_eq!(
        second.reset_records, 0,
        "reusing the stage's own prefix is not a reset reread of the source"
    );

    // Pass 3 — and it is not a one-shot: a third fresh destination adds
    // nothing either.
    fx.collect("third");
    assert_eq!(fx.shard_count(), 1);
    assert_eq!(fx.concat(), b"one\ntwo\nthree\n");

    // The second destination is still a *debt holder*: its pass must have
    // recorded what this destination is now owed, so the next pass for it is
    // a no-op rather than another reread. The debt state is keyed by
    // `DestinationView::id`, which the CLI sets to `destination_id(repo_root)`.
    let debts = fs::read_to_string(fx.state.join("debts-v2.json")).unwrap();
    assert!(
        debts.contains("\"second\""),
        "the pass must record the second destination's debt:\n{debts}"
    );
}

/// A source that grew while a destination was being added: the pass must seal
/// the *tail* the stage does not hold, not the whole file over again.
#[test]
fn a_fresh_destination_seals_only_what_the_stage_lacks() {
    let fx = Fixture::new(b"one\ntwo\n");
    fx.collect("first");
    fx.append(b"three\n");
    fx.collect("second");

    assert_eq!(
        fx.concat(),
        b"one\ntwo\nthree\n",
        "the stage must end up exactly the source, not the source twice"
    );
    assert_eq!(fx.shard_count(), 2);
}

/// A whole-file source (`format: json`, e.g. a single-JSON harness export).
///
/// It has no incremental model — `process_whole_file` seals the file in one
/// shard or not at all — so the reuse is claimed only when the stage already
/// holds that file entire. The seal appends a newline after the single "line"
/// it is given, which is why the expected body ends in one.
#[test]
fn a_fresh_destination_does_not_reseal_a_whole_file_source() {
    let fx = Fixture::named("session.json", "json", Some("session.json"), br#"{"a":1}"#);
    fx.collect("first");
    assert_eq!(fx.shard_count(), 1);
    assert_eq!(fx.concat(), b"{\"a\":1}\n");

    fx.collect("second");
    assert_eq!(
        fx.shard_count(),
        1,
        "a whole-file source must not be sealed a second time"
    );
    assert_eq!(fx.concat(), b"{\"a\":1}\n");
}

/// A compressed source (`jsonl.zst`). Its cursor is over the *compressed*
/// bytes while the stage holds the decoded lines, so a prefix of the file
/// cannot be spelled as one — the reuse is claimed only for the whole export.
#[test]
fn a_fresh_destination_does_not_reseal_a_compressed_source() {
    let lines = b"{\"uuid\":\"u1\"}\n{\"uuid\":\"u2\"}\n";
    let compressed = zstd::stream::encode_all(&lines[..], 3).unwrap();
    let fx = Fixture::named("session.jsonl.zst", "jsonl / jsonl.zst", None, &compressed);

    fx.collect("first");
    assert_eq!(fx.shard_count(), 1);
    assert_eq!(fx.concat(), lines);

    fx.collect("second");
    assert_eq!(
        fx.shard_count(),
        1,
        "a compressed source must not be sealed a second time"
    );
    assert_eq!(fx.concat(), lines);
}

/// The counter-case that makes a write-time identity guard wrong.
///
/// A harness may genuinely append bytes identical to bytes already sealed —
/// here, the file `a\n` grows to `a\na\n`. The appended `a\n` is byte-identical
/// to the shard the stage already holds, and it is **real content**: dropping
/// it would lose a turn. A destination with no record of its own must therefore
/// resume from the length the stage covers, not refuse to write because the
/// bytes look familiar.
#[test]
fn repeated_content_is_still_sealed() {
    let fx = Fixture::new(b"a\n");
    fx.collect("first");
    assert_eq!(fx.concat(), b"a\n");

    fx.append(b"a\n");
    fx.collect("second");

    assert_eq!(
        fx.concat(),
        b"a\na\n",
        "the repeated line is real content and must be sealed"
    );
}

// ---------------------------------------------------------------- SQLite

/// The `opencode` cell, declared exactly as the real registry declares it: a
/// single SQLite file, reached through the configured root only.
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

/// One session in an opencode-shaped store. The schema is the one the product's
/// own unit tests pin (`b100_cursor_identity_test.rs`).
fn create_store(db: &Path) {
    let conn = rusqlite::Connection::open(db).unwrap();
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
         );",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session (id, time_created, time_updated, title) VALUES (?1, ?2, ?2, ?3)",
        rusqlite::params!["session-a", 1_000, "fixture"],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?3, ?4)",
        rusqlite::params![
            "session-a-msg-1",
            "session-a",
            1_010,
            r#"{"role":"user","text":"first"}"#
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, session_id, message_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        rusqlite::params![
            "session-a-part-1",
            "session-a",
            "session-a-msg-1",
            1_020,
            r#"{"text":"first"}"#
        ],
    )
    .unwrap();
}

fn sqlite_scan(db: &Path) -> scanner::ScanReport {
    let mut roots = std::collections::BTreeMap::new();
    roots.insert("opencode".to_string(), db.to_string_lossy().into_owned());
    let config = Config {
        harness_roots: roots,
        ..Config::default()
    };
    scanner::scan_with_registry(&config, &sqlite_registry()).unwrap()
}

fn shard_count(stage: &Path, session_id: &str) -> usize {
    store::sealed_shard_entries(&store::session_shard_dir(stage, MACHINE, session_id))
        .unwrap()
        .len()
}

/// A SQLite session cannot be handed a stage-derived *offset*: its cursor is a
/// logical `(time_updated, id)` key, so the stage says nothing about it until
/// the session has been re-exported. The reuse is therefore judged afterwards,
/// against the export the pass just produced — and it has to be, or a fresh
/// destination re-exports and re-seals every opencode / cursor / grok session
/// as well.
#[test]
fn a_fresh_destination_does_not_reseal_a_sqlite_session() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("opencode.db");
    create_store(&db);
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");
    let scan = sqlite_scan(&db);
    assert_eq!(scan.records.len(), 1, "the fixture has one session");
    let id = scan.records[0].id.clone();

    collect::collect_scan_report(&scan, &stage, MACHINE, &state, 20, &dest("first")).unwrap();
    assert_eq!(shard_count(&stage, &id), 1);
    let body = store::concat_shards(&stage, MACHINE, &id).unwrap();

    collect::collect_scan_report(&scan, &stage, MACHINE, &state, 20, &dest("second")).unwrap();
    assert_eq!(
        shard_count(&stage, &id),
        1,
        "a SQLite session must not be re-exported and re-sealed for a second destination"
    );
    assert_eq!(
        store::concat_shards(&stage, MACHINE, &id).unwrap(),
        body,
        "the exported body must not be stored twice"
    );
}
