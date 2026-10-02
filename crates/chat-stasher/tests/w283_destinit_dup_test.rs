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
//!
//! The defect also ran on real machines, so the last section starts from the
//! stage it left behind — a body sealed two or three times — and pins that a
//! pass over *that* seals nothing either. The machine's own `run-once` is one
//! of those passes, and a fix that only held on a clean stage would have left
//! the damage growing every hour.

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

// ------------------------------- the stage the defect already left behind
//
// The defect ran on real machines before this branch existed, so the fix meets
// a stage that is *already* wrong: the real archive holds 1212 sessions whose
// body is sealed twice. A pass over that stage must seal nothing — and it must
// keep sealing nothing however many times it runs, because the pass the machine
// runs every hour is exactly this one.
//
// So these tests plant the second copy by hand (a fixed pass can no longer
// produce one) and drive the ordinary pass over it. The shapes are the ones the
// defect leaves, plus the one the *cap* leaves: a body large enough to be
// sealed as several shards re-sealed as the same several shards (`A, B, A, B`),
// which is a re-seal the single-shard test cannot see.

impl Fixture {
    /// Seal `lines` as the next shard, on top of whatever the stage holds: the
    /// shard a pass that re-read a source it had already sealed used to leave.
    fn plant(&self, lines: &[&str]) {
        // Plant pre-fix archive bytes explicitly: ordinary writers now make
        // exact retries a no-op, while the regressions below need to model
        // historical shards that were already doubled on disk.
        store::write_sealed_shard_bytes_allow_exact_repeat_with_cap(
            store::StageWriter::Collect,
            &self.stage,
            MACHINE,
            &self.id,
            &lines
                .iter()
                .map(|line| line.as_bytes().to_vec())
                .collect::<Vec<_>>(),
            store::DEFAULT_SHARD_BUCKET_CAP,
        )
        .unwrap();
    }
}

/// **The answer to "is the hourly timer making the real archive worse?"**
///
/// The machine's timer runs `run-once` every hour for a destination whose
/// cursor is stored. That destination never asks the stage for a position — it
/// has one of its own — so the pass seals only real deltas and the doubling
/// stays where the defect left it. Pinned here so a fix that started re-reading
/// the source hourly would be caught.
#[test]
fn a_stored_cursor_pass_over_a_doubled_stage_seals_nothing() {
    let fx = Fixture::new(b"one\ntwo\nthree\n");
    fx.collect("first");
    fx.plant(&["one", "two", "three"]);
    assert_eq!(fx.shard_count(), 2, "the fixture must start doubled");

    // Two more passes for the same destination: the hourly timer's shape.
    fx.collect("first");
    fx.collect("first");
    assert_eq!(
        fx.shard_count(),
        2,
        "an ordinary pass has a cursor of its own and must seal nothing"
    );
    assert_eq!(fx.concat(), b"one\ntwo\nthree\none\ntwo\nthree\n");
}

/// A destination with no cursor of its own, over the stage the defect left: the
/// body it is owed is already there, twice. It must seal nothing. This is the
/// path `dest-init` step 1 and `setup` step 4 take, and the path a lost state
/// file puts the hourly pass on as well.
#[test]
fn a_fresh_destination_over_a_doubled_stage_seals_nothing() {
    let fx = Fixture::new(b"one\ntwo\nthree\n");
    fx.collect("first");
    fx.plant(&["one", "two", "three"]);
    assert_eq!(fx.shard_count(), 2);

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
        2,
        "a doubled stage must not become a tripled one"
    );
    assert_eq!(
        fx.concat(),
        b"one\ntwo\nthree\none\ntwo\nthree\n",
        "the stage's body must be left exactly as it was found"
    );
    assert_eq!(
        second.lines_written, 0,
        "nothing new was read, so nothing may be written"
    );
}

/// Twice is not a special case. A stage that has already been tripled must not
/// be quadrupled, or the fix would only slow the growth down.
#[test]
fn a_fresh_destination_over_a_tripled_stage_seals_nothing() {
    let fx = Fixture::new(b"one\ntwo\nthree\n");
    fx.collect("first");
    fx.plant(&["one", "two", "three"]);
    fx.plant(&["one", "two", "three"]);
    assert_eq!(fx.shard_count(), 3);

    fx.collect("second");
    assert_eq!(fx.shard_count(), 3);
    assert_eq!(
        fx.concat(),
        b"one\ntwo\nthree\none\ntwo\nthree\none\ntwo\nthree\n"
    );
}

/// Duplicated lines may precede the remaining source lines. The cursor must
/// find the longest complete source prefix anywhere in the stage, independent
/// of how the old bug arranged its repeated copies.
#[test]
fn a_fresh_destination_recognizes_duplicated_line_arrangements() {
    for (name, source, staged) in [
        (
            "single duplicated line",
            b"A\n".as_slice(),
            b"A\nA\n".as_slice(),
        ),
        (
            "duplicate before remaining line",
            b"A\nB\n".as_slice(),
            b"A\nA\nB\n".as_slice(),
        ),
        (
            "two duplicated copies",
            b"A\nB\n".as_slice(),
            b"A\nB\nA\nB\n".as_slice(),
        ),
    ] {
        let fx = Fixture::new(source);
        fx.plant(
            &std::str::from_utf8(staged)
                .unwrap()
                .trim_end()
                .split('\n')
                .collect::<Vec<_>>(),
        );
        assert_eq!(fx.concat(), staged, "fixture arrangement: {name}");

        fx.collect("fresh");
        assert_eq!(
            fx.concat(),
            staged,
            "already sealed source lines must not be appended again: {name}"
        );
    }
}

/// A genuine append after a doubled stage is still source data. The cursor
/// finds the covered prefix and seals only the unseen B here.
#[test]
fn a_genuine_append_after_doubling_is_sealed_once() {
    let fx = Fixture::new(b"A\n");
    fx.collect("first");
    fx.plant(&["A"]);
    fx.append(b"B\n");

    fx.collect("second");

    assert_eq!(fx.concat(), b"A\nA\nB\n");
    assert_eq!(fx.shard_count(), 3);
}

/// A body sealed over two passes (`A`, then `B`) and then re-sealed by the
/// defect as *one* shard holding both — the shape a multi-shard body leaves
/// when a destination with no cursor re-reads the whole source. The stage's
/// body is the source twice, but the copy's shard boundaries do not line up
/// with the first copy's, so neither "the stage's length" nor "the last
/// shard's length" is the position: where the two bodies agree is.
#[test]
fn a_fresh_destination_over_a_multi_shard_reseal_seals_nothing() {
    let fx = Fixture::new(b"one\ntwo\n");
    fx.collect("first");
    fx.append(b"three\nfour\n");
    fx.collect("first");
    assert_eq!(fx.shard_count(), 2, "A, then B: the source in two shards");
    fx.plant(&["one", "two", "three", "four"]);
    assert_eq!(fx.shard_count(), 3, "the defect's third shard holds both");

    fx.collect("second");
    assert_eq!(
        fx.shard_count(),
        3,
        "a re-seal of a multi-shard body must not be sealed a third time"
    );
    assert_eq!(
        fx.concat(),
        b"one\ntwo\nthree\nfour\none\ntwo\nthree\nfour\n"
    );
}

/// The source grew after the doubling — the state the real archive reaches
/// within the hour. The pass must seal the new turn and *only* the new turn:
/// the bytes already held twice are not sealed a third time, and the doubled
/// copy is not mistaken for a reason to re-read the whole file.
#[test]
fn a_fresh_destination_over_a_doubled_stage_seals_only_the_growth() {
    let fx = Fixture::new(b"one\n");
    fx.collect("first");
    fx.plant(&["one"]);
    fx.append(b"two\n");
    assert_eq!(fx.concat(), b"one\none\n");

    fx.collect("second");
    assert_eq!(
        fx.concat(),
        b"one\none\ntwo\n",
        "the growth is what is owed; the rest is already there"
    );
    assert_eq!(fx.shard_count(), 3);
}

/// A stage whose *total* length is no longer than the source, because the
/// source has grown past the doubled body: the stage is not a prefix of the
/// file, so neither "the stage's length" nor "the source's length" is the
/// position wanted. The position is where the two agree.
#[test]
fn a_grown_source_out_past_a_doubled_stage_seals_only_the_growth() {
    let fx = Fixture::new(b"one\n");
    fx.collect("first");
    fx.plant(&["one"]);
    fx.append(b"two\nthree\nfour\n");
    assert_eq!(fx.concat(), b"one\none\n");

    fx.collect("second");
    assert_eq!(
        fx.concat(),
        b"one\none\ntwo\nthree\nfour\n",
        "the doubled body is already there; only the new lines are owed"
    );
}

/// The whole-file shape (`format: json`), which has no incremental model: the
/// reuse is claimed only when the stage holds that file entire — and a stage
/// holding it twice still holds it entire.
#[test]
fn a_fresh_destination_over_a_doubled_whole_file_source_seals_nothing() {
    let fx = Fixture::named("session.json", "json", Some("session.json"), br#"{"a":1}"#);
    fx.collect("first");
    fx.plant(&[r#"{"a":1}"#]);
    assert_eq!(fx.shard_count(), 2);

    fx.collect("second");
    assert_eq!(
        fx.shard_count(),
        2,
        "a whole-file source sealed twice must not be sealed a third time"
    );
    assert_eq!(fx.concat(), b"{\"a\":1}\n{\"a\":1}\n");
}

/// The compressed shape (`jsonl.zst`): the cursor is over the *compressed*
/// bytes while the stage holds the decoded lines, so the reuse is claimed only
/// for the whole decoded export — twice over as well as once.
#[test]
fn a_fresh_destination_over_a_doubled_compressed_source_seals_nothing() {
    let lines = b"{\"uuid\":\"u1\"}\n{\"uuid\":\"u2\"}\n";
    let compressed = zstd::stream::encode_all(&lines[..], 3).unwrap();
    let fx = Fixture::named("session.jsonl.zst", "jsonl / jsonl.zst", None, &compressed);
    fx.collect("first");
    fx.plant(&[r#"{"uuid":"u1"}"#, r#"{"uuid":"u2"}"#]);
    assert_eq!(fx.shard_count(), 2);
    assert_eq!(
        fx.concat(),
        b"{\"uuid\":\"u1\"}\n{\"uuid\":\"u2\"}\n{\"uuid\":\"u1\"}\n{\"uuid\":\"u2\"}\n"
    );

    fx.collect("second");
    assert_eq!(
        fx.shard_count(),
        2,
        "a compressed source sealed twice must not be sealed a third time"
    );
}

/// The same, for a SQLite session. Its reuse is judged after the export rather
/// than from a stage-derived offset, but the question is identical: does the
/// stage already hold this export? A stage holding it twice does.
#[test]
fn a_fresh_destination_does_not_reseal_a_doubled_sqlite_session() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("opencode.db");
    create_store(&db);
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");
    let scan = sqlite_scan(&db);
    let id = scan.records[0].id.clone();

    collect::collect_scan_report(&scan, &stage, MACHINE, &state, 20, &dest("first")).unwrap();
    let body = store::concat_shards(&stage, MACHINE, &id).unwrap();
    store::write_sealed_shard_raw_with_cap(
        store::StageWriter::Restore,
        &stage,
        MACHINE,
        &id,
        &body,
        20,
    )
    .unwrap();
    assert_eq!(shard_count(&stage, &id), 2);

    collect::collect_scan_report(&scan, &stage, MACHINE, &state, 20, &dest("second")).unwrap();
    assert_eq!(
        shard_count(&stage, &id),
        2,
        "an export the stage already holds twice must not be sealed a third time"
    );
    assert_eq!(
        store::concat_shards(&stage, MACHINE, &id).unwrap(),
        [body.clone(), body].concat()
    );
}

/// A word-whole source that has been exported more than once: the stage holds an
/// older version *and* this one. This version is the stage's tail, so nothing is
/// owed — a whole-body comparison sees the older version too and seals this one
/// again, which is the whole-file half of the same defect.
#[test]
fn a_fresh_destination_over_a_whole_file_source_with_an_older_version_seals_nothing() {
    let fx = Fixture::named("session.json", "json", Some("session.json"), br#"{"a":1}"#);
    fx.collect("first");
    fs::write(&fx.source, br#"{"a":1,"b":2}"#).unwrap();
    fx.collect("first");
    assert_eq!(fx.shard_count(), 2, "one shard per exported version");
    assert_eq!(fx.concat(), b"{\"a\":1}\n{\"a\":1,\"b\":2}\n");

    fx.collect("second");
    assert_eq!(
        fx.shard_count(),
        2,
        "the current version is already the stage's tail"
    );
    assert_eq!(fx.concat(), b"{\"a\":1}\n{\"a\":1,\"b\":2}\n");
}

/// The same shape for a compressed source: the stage holds the older export and
/// the current one.
#[test]
fn a_fresh_destination_over_a_compressed_source_with_an_older_version_seals_nothing() {
    let first = b"{\"uuid\":\"u1\"}\n";
    let fx = Fixture::named(
        "session.jsonl.zst",
        "jsonl / jsonl.zst",
        None,
        &zstd::stream::encode_all(&first[..], 3).unwrap(),
    );
    fx.collect("first");
    let second = b"{\"uuid\":\"u1\"}\n{\"uuid\":\"u2\"}\n";
    fs::write(
        &fx.source,
        zstd::stream::encode_all(&second[..], 3).unwrap(),
    )
    .unwrap();
    fx.collect("first");
    assert_eq!(fx.shard_count(), 2, "one shard per exported version");
    assert_eq!(
        fx.concat(),
        b"{\"uuid\":\"u1\"}\n{\"uuid\":\"u1\"}\n{\"uuid\":\"u2\"}\n"
    );

    fx.collect("second");
    assert_eq!(
        fx.shard_count(),
        2,
        "the current export is already the stage's tail"
    );
}

/// A shrink or rewrite is a new snapshot even when its bytes happen to be a
/// suffix of an older compressed export. It must be recorded on a fresh
/// destination rather than mistaken for an export already sealed.
#[test]
fn a_fresh_destination_records_a_shrunken_compressed_export() {
    let older = b"{\"uuid\":\"u1\"}\n{\"uuid\":\"u2\"}\n";
    let fx = Fixture::named(
        "session.jsonl.zst",
        "jsonl / jsonl.zst",
        None,
        &zstd::stream::encode_all(&older[..], 3).unwrap(),
    );
    fx.collect("first");
    let current = b"{\"uuid\":\"u2\"}\n";
    fs::write(
        &fx.source,
        zstd::stream::encode_all(&current[..], 3).unwrap(),
    )
    .unwrap();
    fx.collect("second");
    assert_eq!(
        fx.shard_count(),
        2,
        "a shrunken export that is only a suffix of the older snapshot is a new version"
    );
    assert_eq!(fx.concat(), [older.as_slice(), current.as_slice()].concat());
}

/// Distinct earlier snapshots may each occupy one shard. Their concatenation
/// is not evidence that the current, whole compressed export was ever sealed
/// as one snapshot. In particular, seeing A then B must not make a fresh
/// destination skip the new export AB.
#[test]
fn a_fresh_destination_seals_a_compressed_export_after_distinct_snapshot_shards() {
    let current = b"A\nB\n";
    let fx = Fixture::named(
        "session.jsonl.zst",
        "jsonl / jsonl.zst",
        None,
        &zstd::stream::encode_all(&current[..], 3).unwrap(),
    );
    fx.plant(&["A"]);
    fx.plant(&["B"]);

    fx.collect("fresh");

    assert_eq!(fx.shard_count(), 3, "AB is a new compressed snapshot");
    assert_eq!(fx.concat(), b"A\nB\nA\nB\n");
}

/// A multi-shard export repeated by the old bug is distinguishable from two
/// distinct snapshots because the exact shard sequence appears twice.
#[test]
fn a_fresh_destination_recognizes_an_exact_repeated_compressed_shard_sequence() {
    let current = b"A\nB\n";
    let fx = Fixture::named(
        "session.jsonl.zst",
        "jsonl / jsonl.zst",
        None,
        &zstd::stream::encode_all(&current[..], 3).unwrap(),
    );
    for line in ["A", "B", "A", "B"] {
        fx.plant(&[line]);
    }

    fx.collect("fresh");

    assert_eq!(fx.shard_count(), 4, "the repeated export is already sealed");
    assert_eq!(fx.concat(), b"A\nB\nA\nB\n");
}

/// Append one message to the fixture session, so its next export differs.
fn add_message(db: &Path, id: &str, at: i64) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?3, ?4)",
        rusqlite::params![id, "session-a", at, r#"{"role":"user","text":"second"}"#],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, session_id, message_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        rusqlite::params![
            format!("{id}-part"),
            "session-a",
            id,
            at,
            r#"{"text":"second"}"#
        ],
    )
    .unwrap();
    conn.execute(
        "UPDATE session SET time_updated = ?1 WHERE id = ?2",
        rusqlite::params![at, "session-a"],
    )
    .unwrap();
}

/// A SQLite session that has been exported more than once: each export is a
/// whole-session snapshot, so the stage holds every version it has seen and the
/// current one is the tail. A fresh destination is owed nothing.
#[test]
fn a_fresh_destination_does_not_reseal_a_sqlite_session_with_an_older_version() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("opencode.db");
    create_store(&db);
    let stage = dir.path().join("stage");
    let state = dir.path().join("state");
    let scan = sqlite_scan(&db);
    let id = scan.records[0].id.clone();

    collect::collect_scan_report(&scan, &stage, MACHINE, &state, 20, &dest("first")).unwrap();
    assert_eq!(shard_count(&stage, &id), 1);
    add_message(&db, "session-a-msg-2", 2_000);
    collect::collect_scan_report(&scan, &stage, MACHINE, &state, 20, &dest("first")).unwrap();
    assert_eq!(
        shard_count(&stage, &id),
        2,
        "the session changed, so it is exported and sealed again"
    );

    collect::collect_scan_report(&scan, &stage, MACHINE, &state, 20, &dest("second")).unwrap();
    assert_eq!(
        shard_count(&stage, &id),
        2,
        "the current export is already the stage's tail"
    );
}
