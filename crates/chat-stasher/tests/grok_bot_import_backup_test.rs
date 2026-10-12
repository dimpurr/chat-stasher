//! W988 (GB-3b): `import --harness grok-bot --from <dir>` — feeding copied
//! Grok Bot cache backups in as a first-class pass.
//!
//! Every fixture is synthetic: no real conversation text, no real ids, no
//! real stage/archive/state, and the "backups" are sandbox directories the
//! test itself names — never a copy of a real cache. The properties pinned
//! here are the ones the GB-3 rescue made necessary and the ones W986's
//! session-keyed delivered set made possible:
//! a backup that overlaps the live cache, another backup, or a previous
//! import delivers exactly its new rows, once; the live collector's own
//! delivery stays incremental afterwards; and a pass with nothing new writes
//! nothing at all.

#[path = "../src/test_support.rs"]
mod test_support;

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::models::{HarnessSource, SessionRecord, SqliteSessionLayout};
use chat_stasher::scanner::ScanReport;
use chat_stasher::store;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const AGENT: &str = "11111111-2222-4333-8444-555555555555";
/// A second synthetic agent, so one import sees more than one session.
const AGENT_B: &str = "12121212-3434-4545-5656-676767676767";
const ACCOUNT: &str = "grok%7Cuser_synthetic-0000";

fn replica_blob_name(agent: &str) -> String {
    format!(
        "{}.blob",
        test_support::grok_bot_blob_name(&format!(
            "sand.client.slice.account.{ACCOUNT}.transcript.replicas.{agent}"
        ))
    )
}

/// One app-shaped entry (key names from W321/W319b) for a given sequence.
fn entry(sequence: u64, content: &str) -> Value {
    json!({
        "id": format!("synthetic-{sequence}"),
        "kind": "message",
        "message": {"content": content, "role": "user"},
        "seq": sequence,
        "timestampMs": 1780000000000u64 + sequence * 100,
    })
}

fn replica_payload(entries: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schemaVersion": 1,
        "value": {
            "acceptedSequenceHint": 0,
            "entries": entries,
            "epochHint": "synthetic-epoch",
            "persistedAt": 1780000000000u64,
        }
    }))
    .unwrap()
}

/// Seed one Grok Bot persistence directory — the shape a copied cache
/// backup takes — and return the directory itself.
fn backup_dir(sandbox_root: &Path, name: &str, entries: &[Value]) -> PathBuf {
    let dir = sandbox_root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(replica_blob_name(AGENT)),
        replica_payload(entries.into()),
    )
    .unwrap();
    dir
}

/// One persistence directory for `AGENT_B`, so a backup can hold two sessions.
fn backup_dir_with_agent_b(
    sandbox_root: &Path,
    name: &str,
    entries_a: &[Value],
    entries_b: &[Value],
) -> PathBuf {
    let dir = backup_dir(sandbox_root, name, entries_a);
    fs::write(
        dir.join(replica_blob_name(AGENT_B)),
        replica_payload(entries_b.into()),
    )
    .unwrap();
    dir
}

/// One live persistence directory the collector itself reads, and the
/// `SessionRecord` a live scan of it would build.
fn live_source(sandbox_root: &Path, name: &str, entries: &[Value]) -> (SessionRecord, PathBuf) {
    let source = backup_dir(sandbox_root, name, entries);
    let session_id = format!("grok-bot.synthetic-machine.{AGENT}");
    let record = SessionRecord {
        id: session_id,
        absolute_path: source.join(replica_blob_name(AGENT)),
        byte_size: fs::metadata(source.join(replica_blob_name(AGENT)))
            .unwrap()
            .len(),
        mtime: SystemTime::now(),
        source: HarnessSource::GrokBot,
        compressed: false,
        sqlite_layout: Some(SqliteSessionLayout::GrokBot),
        provenance: Default::default(),
    };
    (record, source)
}

fn destination() -> DestinationView<'static> {
    DestinationView::unreachable("synthetic-destination")
}

fn state_path(state: &Path) -> PathBuf {
    state.join("debts-v2.json")
}

fn session_id() -> String {
    format!("grok-bot.synthetic-machine.{AGENT}")
}

/// Run one ordinary collect pass over the given live records, exactly as the
/// scheduled collector does.
fn collect_live(records: &[SessionRecord], stage: &Path, state: &Path) -> collect::CollectReport {
    let mut scan = ScanReport::default();
    scan.records = records.to_vec();
    collect::collect_scan_report(&scan, stage, "synthetic-machine", state, 20, &destination())
        .unwrap()
}

fn import(
    backups: &[PathBuf],
    stage: &Path,
    state: &Path,
    dry_run: bool,
) -> collect::GrokBotImportReport {
    collect::import_grok_bot_backups(
        backups,
        stage,
        "synthetic-machine",
        state,
        20,
        &destination(),
        dry_run,
    )
    .unwrap()
}

/// Count how often one raw row content occurs in one session's stage body.
fn row_content_count(body: &[u8], needle: &str) -> usize {
    String::from_utf8_lossy(body)
        .lines()
        .filter(|line| line.contains(needle))
        .count()
}

/// A recursive digest of every file — and every directory's existence —
/// under a root: the shape a writes-nothing assertion can hash
/// byte-for-byte, so even a freshly created empty directory is visible.
fn tree_fingerprint(root: &Path) -> BTreeMap<String, (u64, String)> {
    fn walk(root: &Path, prefix: &Path, out: &mut BTreeMap<String, (u64, String)>) {
        let entries = fs::read_dir(root).unwrap();
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                out.insert(
                    prefix
                        .join(entry.file_name())
                        .to_string_lossy()
                        .into_owned(),
                    (0, "directory".to_string()),
                );
                walk(&path, &prefix.join(entry.file_name()), out);
            } else {
                let bytes = fs::read(&path).unwrap();
                use sha2::Digest;
                let digest: String = sha2::Sha256::digest(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                out.insert(
                    prefix
                        .join(entry.file_name())
                        .to_string_lossy()
                        .into_owned(),
                    (bytes.len() as u64, digest),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, Path::new(""), &mut out);
    out
}

struct Fixture {
    sandbox: test_support::Sandbox,
    stage: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let sandbox = test_support::Sandbox::new();
        let stage = sandbox.data_home().join("stage");
        let state = sandbox.state_home().join("chat-stasher");
        Fixture {
            sandbox,
            stage,
            state,
        }
    }
}

fn body(fixture: &Fixture, session: &str) -> Vec<u8> {
    store::concat_shards(&fixture.stage, "synthetic-machine", session).unwrap()
}

/// W988 task test 1, first half: the live cache and a backup copy of the
/// same session hold the same rows, so the import delivers *nothing* — and
/// because it delivers nothing it writes nothing at all.
#[test]
fn a_backup_copy_of_a_live_session_imports_no_rows_and_writes_nothing() {
    let fixture = Fixture::new();
    let rows = vec![entry(4, "synthetic-live-4"), entry(5, "synthetic-live-5")];
    let (record, _live) = live_source(fixture.sandbox.root(), "live-cache", &rows);
    collect_live(&[record], &fixture.stage, &fixture.state);
    let backup = backup_dir(fixture.sandbox.root(), "grokbot-cache-copy", &rows);
    let before = tree_fingerprint(fixture.sandbox.root());

    let report = import(&[backup], &fixture.stage, &fixture.state, false);

    assert_eq!(report.sessions_seen, 1, "the one session the fixture holds");
    assert_eq!(
        report.rows_new, 0,
        "a copy of already-delivered rows is not new"
    );
    assert_eq!(
        report.rows_already_delivered, 2,
        "both rows were already delivered"
    );
    assert_eq!(report.shards_written, 0, "nothing was sealed");
    let mut sessions = report.sessions.iter();
    let session = sessions.next().unwrap();
    assert_eq!(session.rows_new, 0);
    assert_eq!(session.rows_already_delivered, 2);
    assert!(!session.reread, "a debt the stage proves is left alone");
    let after = tree_fingerprint(fixture.sandbox.root());
    assert_eq!(
        before, after,
        "a pass that delivers nothing must leave every namespace byte-identical — \
         state, stage and the backup itself"
    );
}

/// W988 task test 1, second half: a backup holding exactly three rows the
/// live cache no longer has imports exactly those three, and re-running the
/// import delivers 0.
#[test]
fn a_backup_with_three_extra_older_rows_imports_exactly_those_three() {
    let fixture = Fixture::new();
    let live_rows = vec![entry(4, "synthetic-live-4"), entry(5, "synthetic-live-5")];
    let (record, _live) = live_source(fixture.sandbox.root(), "live-cache", &live_rows);
    collect_live(&[record.clone()], &fixture.stage, &fixture.state);

    let backup_rows = vec![
        entry(1, "synthetic-backup-1"),
        entry(2, "synthetic-backup-2"),
        entry(3, "synthetic-backup-3"),
        entry(4, "synthetic-live-4"),
        entry(5, "synthetic-live-5"),
    ];
    let backup = backup_dir(fixture.sandbox.root(), "grokbot-cache-copy", &backup_rows);
    let report = import(&[backup.clone()], &fixture.stage, &fixture.state, false);
    assert_eq!(report.rows_new, 3, "exactly the three older rows");
    assert_eq!(
        report.rows_already_delivered, 2,
        "the live rows the backup repeats"
    );
    assert_eq!(
        report.shards_written, 1,
        "one sealed shard for the new rows"
    );
    let session = session_id();
    let stage_body = body(&fixture, &session);
    for needle in [
        "synthetic-backup-1",
        "synthetic-backup-2",
        "synthetic-backup-3",
        "synthetic-live-4",
        "synthetic-live-5",
    ] {
        assert_eq!(
            row_content_count(&stage_body, needle),
            1,
            "row {needle} must be stored exactly once after the import"
        );
    }

    // Idempotence: running the same import a second time delivers 0 rows
    // and writes nothing at all.
    let before = tree_fingerprint(fixture.sandbox.root());
    let repeat = import(&[backup], &fixture.stage, &fixture.state, false);
    assert_eq!(
        repeat.rows_new, 0,
        "a re-run of the same import delivers nothing"
    );
    assert_eq!(
        repeat.rows_already_delivered, 5,
        "everything is already delivered"
    );
    assert_eq!(repeat.shards_written, 0);
    let after = tree_fingerprint(fixture.sandbox.root());
    assert_eq!(
        before, after,
        "an idempotent re-run must leave every namespace byte-identical"
    );
}

/// W988 task test 2: three overlapping backups — A ⊂ B, B∩C partial, C
/// holding rows the live cache lost — deliver the union of every row once:
/// no duplicates anywhere, no loss of the rows the live cache no longer
/// holds, and re-running all three imports delivers nothing.
#[test]
fn three_overlapping_backups_deliver_the_union_once() {
    let fixture = Fixture::new();
    // The live cache holds only the newest rows; the older ones fell out of
    // its window — which is exactly what the copies were kept for.
    let live_rows = vec![entry(8, "synthetic-live-8")];
    let (record, _live) = live_source(fixture.sandbox.root(), "live-cache", &live_rows);
    collect_live(&[record], &fixture.stage, &fixture.state);

    let backup_a = backup_dir(
        fixture.sandbox.root(),
        "grokbot-cache-a",
        &[entry(1, "synthetic-union-1"), entry(2, "synthetic-union-2")],
    );
    let backup_b = backup_dir(
        fixture.sandbox.root(),
        "grokbot-cache-b",
        &[
            entry(1, "synthetic-union-1"),
            entry(2, "synthetic-union-2"),
            entry(3, "synthetic-union-3"),
            entry(4, "synthetic-union-4"),
        ],
    );
    // C: partial overlap with B, plus rows B never held.
    let backup_c = backup_dir(
        fixture.sandbox.root(),
        "grokbot-cache-c",
        &[
            entry(4, "synthetic-union-4"),
            entry(5, "synthetic-union-5"),
            entry(6, "synthetic-union-6"),
        ],
    );
    let report = import(
        &[backup_a.clone(), backup_b.clone(), backup_c.clone()],
        &fixture.stage,
        &fixture.state,
        false,
    );
    let session = session_id();
    let stage_body = body(&fixture, &session);
    // No loss: every row any backup held is in the archive.
    for needle in [
        "synthetic-union-1",
        "synthetic-union-2",
        "synthetic-union-3",
        "synthetic-union-4",
        "synthetic-union-5",
        "synthetic-union-6",
        "synthetic-live-8",
    ] {
        assert_eq!(
            row_content_count(&stage_body, needle),
            1,
            "row {needle} must be delivered exactly once across all frames"
        );
    }
    assert_eq!(
        report.rows_new, 6,
        "the union minus what live had already delivered"
    );
    // Each backup that held something new seals its own shard — the same
    // per-record boundary a live pass over two roots of one session leaves
    // (W986's two-paths-one-pass shape). What the row assertions below pin
    // is the property that matters: no row twice, no row lost.
    assert_eq!(
        report.shards_written, 3,
        "one shard per backup that held new rows"
    );
    assert_eq!(
        report.rows_already_delivered, 0,
        "no backup row was already delivered: live row 8 is in no backup, and the \
         backups' overlaps with each other are deduplicated in the counting — no \
         row is counted once per backup that holds it"
    );

    // Re-running all three imports delivers 0 rows in total.
    let repeat = import(
        &[backup_a, backup_b, backup_c],
        &fixture.stage,
        &fixture.state,
        false,
    );
    assert_eq!(
        repeat.rows_new, 0,
        "the union is delivered; re-running adds nothing"
    );
    assert_eq!(repeat.shards_written, 0);
    assert_eq!(
        repeat.rows_already_delivered, 6,
        "every row the backups hold is now delivered"
    );
}

/// W988 task test 3: an import must not disturb the live collector's own
/// delivery. A live collect after the import delivers no already-imported
/// rows, resets nothing, and leaves other sessions' state untouched — for a
/// session the import had nothing new for, every state file stays
/// byte-identical (checked in the first test); here we check the sessions it
/// did extend: nothing is re-delivered and nothing is reset.
#[test]
fn a_live_collect_after_an_import_delivers_nothing_the_import_delivered() {
    let fixture = Fixture::new();
    let live_rows = vec![entry(6, "synthetic-live-6"), entry(7, "synthetic-live-7")];
    let (record, _live) = live_source(fixture.sandbox.root(), "live-cache", &live_rows);
    collect_live(&[record.clone()], &fixture.stage, &fixture.state);

    let backup_rows = vec![
        entry(1, "synthetic-backup-1"),
        entry(2, "synthetic-backup-2"),
        entry(3, "synthetic-backup-3"),
        entry(6, "synthetic-live-6"),
        entry(7, "synthetic-live-7"),
    ];
    let backup = backup_dir(fixture.sandbox.root(), "grokbot-cache-copy", &backup_rows);
    let report = import(&[backup], &fixture.stage, &fixture.state, false);
    assert_eq!(report.rows_new, 3);

    // The ordinary collect reads the live cache again: everything it holds
    // is already in the session's delivered set, so it delivers nothing and
    // resets nothing. Its own cursor stays valid — that is the meaning of
    // the import not moving the live path's state.
    let after = collect_live(&[record], &fixture.stage, &fixture.state);
    assert_eq!(
        after.shards_written, 0,
        "no already-imported row was re-delivered"
    );
    assert_eq!(after.changed_records, 0);
    assert_eq!(
        after.reset_records, 0,
        "the live cursor was not reset by the import"
    );
    assert_eq!(after.lines_written, 0);

    // And the union of live + backup rows is what the session claims to have
    // delivered: five distinct digests, no more, no less.
    let state_file: Value =
        serde_json::from_str(&fs::read_to_string(state_path(&fixture.state)).unwrap()).unwrap();
    let digests = state_file["destinations"]["synthetic-destination"]["files"]
        [format!("grok-bot-session\0{}", session_id())]["cursor"]["grok_bot_row_digests"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(
        digests, 5,
        "the delivered set holds the five distinct rows exactly once each"
    );
}

/// W988 task test 4: `--dry-run` reports the same per-session counts the
/// delivery would produce, and writes nothing anywhere — the whole sandbox
/// stays byte-identical, and a dry run of an import the world has never seen
/// creates not even a state directory.
#[test]
fn dry_run_reports_the_counts_and_writes_nothing() {
    let fixture = Fixture::new();
    let live_rows = vec![entry(4, "synthetic-live-4")];
    let (record, _live) = live_source(fixture.sandbox.root(), "live-cache", &live_rows);
    collect_live(&[record], &fixture.stage, &fixture.state);

    let backup_a = backup_dir_with_agent_b(
        fixture.sandbox.root(),
        "grokbot-cache-a",
        &[
            entry(2, "synthetic-backup-2"),
            entry(3, "synthetic-backup-3"),
            entry(4, "synthetic-live-4"),
        ],
        &[entry(1, "synthetic-agent-b-1")],
    );
    // The second backup overlaps the first; the dry run must plan the
    // in-pass dedup the delivery applies, not count shared rows twice.
    let backup_b = backup_dir(
        fixture.sandbox.root(),
        "grokbot-cache-b",
        &[entry(3, "synthetic-backup-3"), entry(4, "synthetic-live-4")],
    );

    let before = tree_fingerprint(fixture.sandbox.root());
    let report = import(&[backup_a, backup_b], &fixture.stage, &fixture.state, true);
    assert_eq!(
        tree_fingerprint(fixture.sandbox.root()),
        before,
        "a dry run writes nothing anywhere — no stage byte, no state byte, no new directory"
    );

    assert!(report.dry_run);
    assert_eq!(
        report.sessions_seen, 2,
        "the backups hold agent A and agent B"
    );
    assert_eq!(
        report.rows_new, 3,
        "rows 2 and 3 for agent A, row 1 for agent B"
    );
    assert_eq!(
        report.rows_already_delivered, 1,
        "live row 4, counted once across both backups"
    );
    assert_eq!(
        report.shards_written, 2,
        "one shard per session that would be sealed"
    );
    // Sessions are reported one row each, ordered by session id — agent A
    // (uuid 1111…) before agent B (uuid 1212…).
    assert_eq!(
        report
            .sessions
            .iter()
            .map(|session| (session.rows_new, session.rows_already_delivered))
            .collect::<Vec<_>>(),
        vec![(2, 1), (1, 0)],
        "per-session counts: agent A would deliver 2 (row 4 already live), agent B 1"
    );
}

/// W988 task test 5: after the imports above, the rendered and the indexed
/// views show each turn once — the reader picks one variant per sequence and
/// the FTS extractor reads through the same rule, so an imported row is one
/// turn, not two, even when the same position was observed by both the live
/// cache and a backup.
#[test]
fn rendered_and_indexed_views_show_each_turn_once_after_imports() {
    let fixture = Fixture::new();
    // A row shaped the way the reader renders it (the `message` kind carries
    // its own role and content at record level). The live cache saw seq 7
    // once while streaming (an unfinished variant), then the final variant;
    // the backup holds the final variant again.
    let readable_row = |sequence: u64, content: &str, streaming: bool| {
        json!({
            "id": format!("synthetic-{sequence}"),
            "kind": "message",
            "seq": sequence,
            "isStreaming": streaming,
            "role": if streaming { "assistant" } else { "user" },
            "content": content,
            "timestampMs": 1780000000000u64 + sequence * 100,
        })
    };
    let live_rows = vec![
        readable_row(6, "synthetic-live-6", false),
        readable_row(7, "synthetic-live-7", true),
        readable_row(7, "synthetic-live-7", false),
    ];
    let (record, _live) = live_source(fixture.sandbox.root(), "live-cache", &live_rows);
    collect_live(&[record], &fixture.stage, &fixture.state);

    let backup_rows = vec![
        readable_row(5, "synthetic-backup-5", false),
        // The same row twice inside one backup: the union merges the
        // observation, it does not hold two of it.
        readable_row(5, "synthetic-backup-5", false),
        readable_row(7, "synthetic-live-7", false),
    ];
    let backup = backup_dir(fixture.sandbox.root(), "grokbot-cache-copy", &backup_rows);
    let report = import(&[backup], &fixture.stage, &fixture.state, false);
    assert_eq!(
        report.rows_new, 1,
        "only row 5 is new; the variant and its replay are delivered"
    );

    let stage_body = body(&fixture, &session_id());
    let extracted = chat_stasher::fts::extract_index_document_for("grok-bot", &stage_body)
        .expect("the grok-bot reader indexes the imported body");
    assert!(extracted.is_indexable());
    // Three positions rendered: 5, 6, 7 — each exactly once, even though the
    // same finished variant of 7 sits in both the live shard and the
    // imported one.
    assert_eq!(extracted.message_offsets.len(), 3);
    for needle in ["synthetic-backup-5", "synthetic-live-6", "synthetic-live-7"] {
        assert_eq!(
            extracted.body.matches(needle).count(),
            1,
            "turn {needle:?} must be rendered and indexed exactly once"
        );
    }
    assert_eq!(extracted.unread_lines, 0);
}

/// Provenance (task item 3): every shard an import seals says so in its
/// `_chat_stasher` line — the backup directory's *basename* and the import
/// time — and no archived byte carries an absolute path of the machine that
/// imported, following the same name-only convention the takeout importer's
/// raw record uses.
#[test]
fn sealed_shards_record_backup_provenance_and_no_absolute_paths() {
    let fixture = Fixture::new();
    let (record, _live) = live_source(
        fixture.sandbox.root(),
        "live-cache",
        &[entry(1, "synthetic-live-1")],
    );
    collect_live(&[record], &fixture.stage, &fixture.state);

    let backup = backup_dir(
        fixture.sandbox.root(),
        "grokbot-cache-2026",
        &[entry(1, "synthetic-live-1"), entry(2, "synthetic-backup-2")],
    );
    let report = import(&[backup], &fixture.stage, &fixture.state, false);
    assert_eq!(report.rows_new, 1);
    let stage_body = body(&fixture, &session_id());
    let text = String::from_utf8_lossy(&stage_body).into_owned();

    let metadata_lines: Vec<Value> = text
        .lines()
        .filter(|line| line.contains("\"_chat_stasher\""))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    assert_eq!(
        metadata_lines.len(),
        2,
        "the live shard carries no import provenance; the imported one must"
    );
    assert!(metadata_lines[0]
        .get("_chat_stasher")
        .unwrap()
        .get("imported_from_backup")
        .is_none());
    let imported = metadata_lines[1]["_chat_stasher"].clone();
    assert_eq!(imported["imported_from_backup"], "grokbot-cache-2026");
    let imported_at = imported["imported_at"].as_str().unwrap();
    assert!(
        imported_at.parse::<chrono::DateTime<chrono::Utc>>().is_ok(),
        "the import time must be a stamped RFC 3339 moment"
    );

    // The name-only convention: no absolute path of this run exists anywhere
    // in the sealed bytes. The stage's own directory is a fixture root, so
    // its path as a string presence is the failure this asserts against.
    assert!(
        !text.contains(fixture.sandbox.root().to_string_lossy().as_ref()),
        "no home- or sandbox-rooted path may appear in archived data"
    );
    assert!(
        !text.contains("/Users/") && !text.contains("/tmp/"),
        "no machine-shaped path may appear in archived data"
    );
}

/// The backup can be either copy shape: a persistence directory in its own
/// right, or a whole app-support tree above `sand-client-persistence`
/// directories — a user copying the app's data folder gets the same import.
#[test]
fn an_app_support_tree_backup_imports_like_a_persistence_leaf() {
    let fixture = Fixture::new();
    let tree = fixture
        .sandbox
        .root()
        .join("support-tree")
        .join("Grok Bot")
        .join("anywhere")
        .join("sand-client-persistence");
    fs::create_dir_all(&tree).unwrap();
    fs::write(
        tree.join(replica_blob_name(AGENT)),
        replica_payload(vec![
            entry(1, "synthetic-tree-1"),
            entry(2, "synthetic-tree-2"),
        ]),
    )
    .unwrap();
    let report = import(
        &[fixture.sandbox.root().join("support-tree")],
        &fixture.stage,
        &fixture.state,
        false,
    );
    assert_eq!(
        report.rows_new, 2,
        "the tree's buried persistence was found"
    );
    let stage_body = body(&fixture, &session_id());
    assert_eq!(row_content_count(&stage_body, "synthetic-tree-1"), 1);
    assert_eq!(row_content_count(&stage_body, "synthetic-tree-2"), 1);
}

/// A backup that holds no Grok Bot replicas is an honest zero, not a failure
/// and not a guess: the run reports the empty by name and imports nothing.
#[test]
fn a_backup_without_replicas_is_reported_empty_and_imports_nothing() {
    let fixture = Fixture::new();
    let empty = fixture.sandbox.root().join("empty-backup");
    fs::create_dir_all(&empty).unwrap();
    let good = backup_dir(
        fixture.sandbox.root(),
        "grokbot-cache",
        &[entry(1, "synthetic-good-1")],
    );
    let report = import(&[empty, good], &fixture.stage, &fixture.state, false);
    assert_eq!(report.sessions_seen, 1);
    assert_eq!(report.rows_new, 1);
    assert_eq!(
        report.empty_backups,
        vec!["empty-backup".to_string()],
        "the empty backup is named, so a wrong path is visible as itself"
    );
    let stage_body = body(&fixture, &session_id());
    assert_eq!(row_content_count(&stage_body, "synthetic-good-1"), 1);
}

/// A backup that cannot be read to the end refuses the whole pass before
/// anything is written: nothing about its contents is proven, so nothing is
/// imported — the same refusal the takeout importer makes on a truncated
/// export.
#[test]
fn an_unreadable_backup_refuses_the_whole_pass_before_writing() {
    let fixture = Fixture::new();
    let result = collect::import_grok_bot_backups(
        &[fixture.sandbox.root().join("missing-grokbot-cache")],
        &fixture.stage,
        "synthetic-machine",
        &fixture.state,
        20,
        &destination(),
        false,
    );
    assert!(
        result.is_err(),
        "a directory that cannot be read is an error, not a zero"
    );
    assert!(
        !fixture.state.is_dir(),
        "the refusal happens before any state directory is created"
    );
    assert!(
        !fixture.stage.is_dir(),
        "the refusal happens before anything is staged"
    );
}
