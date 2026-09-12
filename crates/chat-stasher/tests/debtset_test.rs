//! ADR-012 / ADR-013: the collector's durable state is a **per-destination
//! debt set**, and a stored cursor is a claim that has to prove itself against
//! either the stage or that destination's archive before it is reused.
//!
//! These tests pin the three ways a cursor loses that argument (a foreign
//! state format, an archive that cannot be consulted, an archive that
//! disagrees) *and* the one way it wins, because a state model that always
//! rereads is not incremental — it is just slow.
//!
//! Only synthetic trees in temp dirs are touched; assertions are byte counts
//! and digests, never session content.

use chat_stasher::collect::{self, ArchiveFacts, DestinationView, ShardFact};
use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry};
use chat_stasher::store;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

const MACHINE: &str = "fixture-machine";

fn registry() -> HarnessRegistry {
    let cell = json!({
        "template": "~/.claude/projects",
        "format": "jsonl",
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
            "id": "claude-code",
            "display_name": "synthetic",
            "paths": paths
        }]
    }))
    .unwrap()
}

fn scan(root: &Path) -> scanner::ScanReport {
    let config = Config {
        claude_projects_dir: Some(root.to_string_lossy().into_owned()),
        ..Config::default()
    };
    scanner::scan_with_registry(&config, &registry()).unwrap()
}

fn unreachable<'a>(id: &str) -> DestinationView<'a> {
    DestinationView::unreachable(id)
}

/// A destination whose archive answers with exactly `facts`.
fn holding<'a>(id: &str, facts: ArchiveFacts) -> DestinationView<'a> {
    DestinationView::new(id.to_string(), move |_| Ok(facts.clone()))
}

fn fact_of(stage: &Path, session_id: &str) -> ShardFact {
    let concat = store::concat_shards(stage, MACHINE, session_id).unwrap();
    ShardFact {
        shard_count: store::sealed_shard_entries(&store::session_shard_dir(
            stage, MACHINE, session_id,
        ))
        .unwrap()
        .len(),
        concat_bytes: concat.len() as u64,
        concat_sha256: Sha256::digest(&concat)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    }
}

/// Fixture: one synthetic source file, its own stage and state dir.
struct Fixture {
    _dir: tempfile::TempDir,
    source_root: std::path::PathBuf,
    source: std::path::PathBuf,
    stage: std::path::PathBuf,
    state: std::path::PathBuf,
}

fn fixture(contents: &[u8]) -> Fixture {
    let dir = tempfile::TempDir::new().unwrap();
    let source_root = dir.path().join("source");
    fs::create_dir_all(&source_root).unwrap();
    let source = source_root.join("session.jsonl");
    fs::write(&source, contents).unwrap();
    Fixture {
        source_root,
        source,
        stage: dir.path().join("stage"),
        state: dir.path().join("state"),
        _dir: dir,
    }
}

impl Fixture {
    fn collect(&self, destination: &DestinationView<'_>) -> collect::CollectReport {
        collect::collect_scan_report(
            &scan(&self.source_root),
            &self.stage,
            MACHINE,
            &self.state,
            20,
            destination,
        )
        .unwrap()
    }

    fn session_id(&self) -> String {
        scan(&self.source_root).records[0].id.clone()
    }
}

/// A pre-ADR-012 `offsets-v1.json` claims the whole file was read, but it
/// belongs to no destination and so can prove nothing to one. It must be
/// ignored — not migrated, and above all not treated as "already read" — and
/// the run must not be blocked by it either.
#[test]
fn legacy_per_machine_state_is_ignored_and_the_source_is_reread() {
    let fx = fixture(b"one\ntwo\nthree\n");
    fs::create_dir_all(&fx.state).unwrap();
    let source_key = fs::canonicalize(&fx.source)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let full = fs::read(&fx.source).unwrap();
    let legacy = json!({
        "version": 1,
        "files": {
            source_key: {
                "offset": full.len(),
                "prefix_len": full.len(),
                "prefix_sha256": Sha256::digest(&full).iter()
                    .map(|b| format!("{b:02x}")).collect::<String>(),
                "compressed": false,
                "opencode": null
            }
        }
    });
    let legacy_path = fx.state.join("offsets-v1.json");
    fs::write(&legacy_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

    let report = fx.collect(&unreachable("dest-a"));

    assert!(
        report.errors.is_empty(),
        "a legacy state must not block a run"
    );
    assert!(
        report.legacy_state_ignored,
        "the ignored legacy state must be reported, not silently dropped"
    );
    // The whole file is read, exactly as if nothing had ever been collected.
    assert_eq!(report.lines_written, 3);
    assert_eq!(report.delta_bytes_read, full.len() as u64);
    assert_eq!(
        store::concat_shards(&fx.stage, MACHINE, &fx.session_id()).unwrap(),
        b"one\ntwo\nthree\n"
    );
    // And it is left untouched on disk: no migration, no rewrite, no deletion.
    assert!(legacy_path.exists());
    println!(
        "legacy_state ignored={} lines_written={} delta_bytes={} legacy_file_still_present={}",
        report.legacy_state_ignored,
        report.lines_written,
        report.delta_bytes_read,
        legacy_path.exists()
    );
}

/// The cursor says it read the file; the stage no longer holds the shards that
/// back that claim; the destination archive cannot be consulted. Nothing can
/// discharge the debt, so the source counts as unread.
#[test]
fn cursor_that_nobody_can_vouch_for_is_reread() {
    let fx = fixture(b"one\ntwo\n");
    let first = fx.collect(&unreachable("dest-a"));
    assert_eq!(first.lines_written, 2);
    assert_eq!(first.unverified_cursors, 0);

    // The shards the cursor is accountable for disappear.
    fs::remove_dir_all(&fx.stage).unwrap();

    let second = fx.collect(&unreachable("dest-a"));
    assert_eq!(second.unverified_cursors, 1);
    assert_eq!(second.reset_records, 1);
    assert_eq!(second.unchanged_records, 0);
    assert_eq!(second.lines_written, 2);
    assert_eq!(second.delta_bytes_read, b"one\ntwo\n".len() as u64);
    assert_eq!(
        store::concat_shards(&fx.stage, MACHINE, &fx.session_id()).unwrap(),
        b"one\ntwo\n"
    );
    println!(
        "unreachable_archive unverified={} reset={} lines_written={}",
        second.unverified_cursors, second.reset_records, second.lines_written
    );
}

/// Same situation, but the archive *is* reachable and holds a different shard
/// set than the cursor claims. A reachable archive that disagrees is not a
/// weaker signal than an unreachable one — it is a stronger one.
#[test]
fn cursor_the_archive_contradicts_is_reread() {
    let fx = fixture(b"one\ntwo\n");
    let first = fx.collect(&unreachable("dest-a"));
    assert_eq!(first.lines_written, 2);
    let session = fx.session_id();

    fs::remove_dir_all(&fx.stage).unwrap();
    let mut wrong = ArchiveFacts::new();
    wrong.insert(
        (MACHINE.to_string(), session.clone()),
        ShardFact {
            shard_count: 1,
            concat_bytes: 999,
            concat_sha256: "not-the-digest-the-cursor-claims".into(),
        },
    );

    let second = fx.collect(&holding("dest-a", wrong));
    assert_eq!(second.unverified_cursors, 1);
    assert_eq!(second.reset_records, 1);
    assert_eq!(second.lines_written, 2);
    println!(
        "archive_disagrees unverified={} reset={} lines_written={}",
        second.unverified_cursors, second.reset_records, second.lines_written
    );
}

/// The load-bearing negative: the model must not degrade into "reread
/// everything, every time". A cursor whose debt is still sealed on the stage
/// proves itself locally, so the second pass reads nothing — and it does so
/// *without* the archive, because an unpushed debt is nobody else's business
/// yet.
#[test]
fn a_debt_still_owed_on_the_stage_proves_itself_offline() {
    let fx = fixture(b"one\ntwo\nthree\n");
    let first = fx.collect(&unreachable("dest-a"));
    let second = fx.collect(&unreachable("dest-a"));

    assert_eq!(first.lines_written, 3);
    assert_eq!(second.lines_written, 0);
    assert_eq!(second.delta_bytes_read, 0);
    assert_eq!(second.unverified_cursors, 0);
    assert_eq!(second.reset_records, 0);
    assert_eq!(second.unchanged_records, 1);
    assert_eq!(second.shards_written, 0);
    // No duplicate content on the stage either.
    assert_eq!(
        store::concat_shards(&fx.stage, MACHINE, &fx.session_id()).unwrap(),
        b"one\ntwo\nthree\n"
    );
    println!(
        "no_false_reread first_lines={} second_lines={} second_delta_bytes={} second_reset={}",
        first.lines_written, second.lines_written, second.delta_bytes_read, second.reset_records
    );
}

/// Regression, found by running the real CLI rather than by reading the code:
/// a pass that stages nothing must not overwrite the debt record with what it
/// happens to see on an emptied stage. Doing so replaces the evidence with the
/// empty shard set, which every later cursor then satisfies trivially — the
/// cursor would go on claiming 8 bytes were read with nothing anywhere backing
/// it, which is the exact "trust it for now" branch ADR-012 forbids.
#[test]
fn an_unchanged_pass_must_not_erase_the_evidence() {
    let fx = fixture(b"one\ntwo\n");
    fx.collect(&unreachable("dest-a"));
    let session = fx.session_id();
    let archived = fact_of(&fx.stage, &session);

    // Pushed, then the stage went away: the archive settles the debt and the
    // pass stages nothing.
    fs::remove_dir_all(&fx.stage).unwrap();
    let mut facts = ArchiveFacts::new();
    facts.insert((MACHINE.to_string(), session.clone()), archived);
    let settled = fx.collect(&holding("dest-a", facts));
    assert_eq!(settled.lines_written, 0);
    assert_eq!(settled.unverified_cursors, 0);

    // Now the archive is gone too. The cursor still claims the whole file, and
    // nothing can back that claim any more, so it must be reread.
    let orphaned = fx.collect(&unreachable("dest-a"));
    assert_eq!(orphaned.unverified_cursors, 1);
    assert_eq!(orphaned.reset_records, 1);
    assert_eq!(orphaned.lines_written, 2);
    assert_eq!(orphaned.delta_bytes_read, b"one\ntwo\n".len() as u64);
    println!(
        "evidence_survives_unchanged_pass settled_lines={} orphaned_unverified={} orphaned_lines={}",
        settled.lines_written, orphaned.unverified_cursors, orphaned.lines_written
    );
}

/// The other half of "does not degrade": once the shards have left the stage,
/// an archive that holds exactly what the cursor claims settles the debt, and
/// the source is still not reread.
#[test]
fn a_debt_the_archive_settles_is_not_reread() {
    let fx = fixture(b"one\ntwo\n");
    fx.collect(&unreachable("dest-a"));
    let session = fx.session_id();
    let archived = fact_of(&fx.stage, &session);

    // Push happened, then the stage was reclaimed: the archive is now the only
    // authority left.
    fs::remove_dir_all(&fx.stage).unwrap();
    let mut facts = ArchiveFacts::new();
    facts.insert((MACHINE.to_string(), session.clone()), archived);

    let second = fx.collect(&holding("dest-a", facts));
    assert_eq!(second.unverified_cursors, 0);
    assert_eq!(second.reset_records, 0);
    assert_eq!(second.lines_written, 0);
    assert_eq!(second.delta_bytes_read, 0);
    println!(
        "archive_settles unverified={} reset={} lines_written={}",
        second.unverified_cursors, second.reset_records, second.lines_written
    );
}

/// The debt set is per destination: what one destination has been given says
/// nothing about what another one owes.
#[test]
fn a_second_destination_starts_owing_everything() {
    let fx = fixture(b"one\ntwo\n");
    let a_first = fx.collect(&unreachable("dest-a"));
    let b_first = fx.collect(&unreachable("dest-b"));
    let a_second = fx.collect(&unreachable("dest-a"));

    assert_eq!(a_first.lines_written, 2);
    // dest-b has never been read for, so the source is unread *for it*.
    assert_eq!(b_first.lines_written, 2);
    assert_eq!(b_first.delta_bytes_read, b"one\ntwo\n".len() as u64);
    // …and dest-a's own debt record survived dest-b's pass untouched.
    assert_eq!(a_second.lines_written, 0);
    assert_eq!(a_second.unverified_cursors, 0);
    println!(
        "per_destination a_first={} b_first={} a_second={}",
        a_first.lines_written, b_first.lines_written, a_second.lines_written
    );
}

/// ADR-015: the debt set is also the answer to "have we ever dealt with this
/// destination at all". That question has to be answerable *locally*, because
/// it is exactly the question you cannot ask a destination that is not there.
#[test]
fn the_debt_set_records_which_destinations_we_have_dealt_with() {
    let fx = fixture(b"one\ntwo\n");

    // Before any pass, no destination is on record — including the one we are
    // about to collect for.
    assert!(!collect::destination_has_record(&fx.state, "dest-a"));

    fx.collect(&unreachable("dest-a"));

    assert!(
        collect::destination_has_record(&fx.state, "dest-a"),
        "a destination we have collected for must be on record, or its later absence \
         cannot be told apart from never having been built"
    );
    assert!(
        !collect::destination_has_record(&fx.state, "dest-b"),
        "and one we never touched must not be — otherwise every declared-but-unbuilt \
         destination would look like a lost copy"
    );
    println!(
        "record a={} b={}",
        collect::destination_has_record(&fx.state, "dest-a"),
        collect::destination_has_record(&fx.state, "dest-b"),
    );
}

/// (1) Main path: after reclaim (shards deleted from stage, manifest written),
/// a subsequent collect against an unreachable destination MUST NOT recreate
/// shards for the already-reclaimed session.
#[test]
fn reclaimed_session_with_manifest_is_not_restaged() {
    let fx = fixture(b"one\ntwo\n");
    let first = fx.collect(&unreachable("dest-a"));
    assert_eq!(first.lines_written, 2);
    assert_eq!(first.shards_written, 1);
    let session = fx.session_id();

    // Reclaim: manifest is written, sealed shards are removed from stage.
    let fresh = chat_stasher::manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    chat_stasher::manifest::write_manifest(&fx.stage, MACHINE, &fresh).unwrap();
    let removed =
        chat_stasher::stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session).unwrap();
    assert_eq!(removed, 1);

    // Destination archive is unreachable (offline). Manifest must discharge the debt locally.
    let second = fx.collect(&unreachable("dest-a"));
    assert_eq!(
        second.unverified_cursors, 0,
        "manifest must prove the reclaimed session"
    );
    assert_eq!(
        second.reset_records, 0,
        "reclaimed session must not be forced into reset"
    );
    assert_eq!(
        second.lines_written, 0,
        "reclaimed session must not produce any new lines"
    );
    assert_eq!(
        second.shards_written, 0,
        "reclaimed session must not write new shards to stage"
    );
    assert_eq!(
        store::sealed_shard_entries(&store::session_shard_dir(&fx.stage, MACHINE, &session))
            .unwrap()
            .len(),
        0,
        "no new shards may be restaged"
    );
}

/// (2) Growing session: after reclaim, if the source file grows, only the new
/// content is staged. The previously reclaimed lines are NOT restaged.
#[test]
fn reclaimed_session_growth_stages_only_delta_not_old_content() {
    let fx = fixture(b"one\ntwo\n");
    let first = fx.collect(&unreachable("dest-a"));
    assert_eq!(first.lines_written, 2);
    assert_eq!(first.shards_written, 1);
    let session = fx.session_id();

    // Reclaim: manifest written, shard body removed.
    let fresh = chat_stasher::manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    chat_stasher::manifest::write_manifest(&fx.stage, MACHINE, &fresh).unwrap();
    let removed =
        chat_stasher::stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session).unwrap();
    assert_eq!(removed, 1);

    // Source file grows with 2 new lines.
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(&fx.source)
        .unwrap();
    f.write_all(b"three\nfour\n").unwrap();
    f.sync_all().unwrap();
    drop(f);

    let second = fx.collect(&unreachable("dest-a"));
    assert_eq!(
        second.unverified_cursors, 0,
        "cursor must be accepted without reset"
    );
    assert_eq!(
        second.reset_records, 0,
        "growth must not reset the read position"
    );
    assert_eq!(
        second.lines_written, 2,
        "only the 2 newly appended lines should be written"
    );
    assert_eq!(
        second.shards_written, 1,
        "exactly 1 new shard should be created for delta"
    );
    // Crucial check: the new shard on stage MUST contain ONLY "three\nfour\n",
    // NOT "one\ntwo\nthree\nfour\n".
    let staged_bytes = store::concat_shards(&fx.stage, MACHINE, &session).unwrap();
    assert_eq!(
        staged_bytes, b"three\nfour\n",
        "stage must hold only the new delta, old content must not be restaged"
    );
}

/// (3) Degradation: if manifest is missing or corrupt, behavior falls back to
/// standard remote verification (never silent pass, never crashing).
#[test]
fn manifest_missing_or_corrupt_falls_back_to_unverifiable() {
    let fx = fixture(b"one\ntwo\n");
    fx.collect(&unreachable("dest-a"));
    let session = fx.session_id();

    // Reclaim: write valid manifest and remove body.
    let fresh = chat_stasher::manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    chat_stasher::manifest::write_manifest(&fx.stage, MACHINE, &fresh).unwrap();
    chat_stasher::stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session).unwrap();

    // Baseline: with intact manifest, unreachable destination does not cause reset.
    let intact = fx.collect(&unreachable("dest-a"));
    assert_eq!(
        intact.unverified_cursors, 0,
        "intact manifest must verify the cursor locally"
    );

    // Case A: manifest corrupted (e.g. invalid JSON garbage)
    let manifest_file = chat_stasher::manifest::manifest_path(&fx.stage, MACHINE);
    fs::write(&manifest_file, b"NOT_VALID_JSON_GARBAGE\n").unwrap();

    let report_corrupt = fx.collect(&unreachable("dest-a"));
    assert_eq!(
        report_corrupt.unverified_cursors, 1,
        "corrupted manifest must not be trusted; must fall back to unverifiable"
    );
    assert_eq!(
        report_corrupt.reset_records, 1,
        "corrupted manifest causes reset reread"
    );
    assert!(
        report_corrupt.errors.is_empty(),
        "corrupted manifest must not crash the run"
    );

    // Case B: manifest missing (e.g. deleted)
    fs::remove_file(&manifest_file).unwrap();
    // Clear out shards written by the reset above
    chat_stasher::stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session).unwrap();

    let report_missing = fx.collect(&unreachable("dest-a"));
    assert_eq!(
        report_missing.unverified_cursors, 1,
        "missing manifest must fall back to remote probe (unverifiable when offline)"
    );
    assert_eq!(report_missing.reset_records, 1);
    assert!(report_missing.errors.is_empty());
}

struct MultiFixture {
    _dir: tempfile::TempDir,
    source_root: std::path::PathBuf,
    stage: std::path::PathBuf,
    state: std::path::PathBuf,
}

fn multi_fixture(sessions: &[(&str, &[u8])]) -> MultiFixture {
    let dir = tempfile::TempDir::new().unwrap();
    let source_root = dir.path().join("source");
    fs::create_dir_all(&source_root).unwrap();
    for (name, content) in sessions {
        fs::write(source_root.join(format!("{name}.jsonl")), content).unwrap();
    }
    MultiFixture {
        source_root,
        stage: dir.path().join("stage"),
        state: dir.path().join("state"),
        _dir: dir,
    }
}

impl MultiFixture {
    fn collect(&self, destination: &DestinationView<'_>) -> collect::CollectReport {
        collect::collect_scan_report(
            &scan(&self.source_root),
            &self.stage,
            MACHINE,
            &self.state,
            20,
            destination,
        )
        .unwrap()
    }

    fn session_id_for(&self, name: &str) -> String {
        let scan_report = scan(&self.source_root);
        scan_report
            .records
            .iter()
            .find(|r| r.absolute_path.ends_with(format!("{name}.jsonl")))
            .unwrap()
            .id
            .clone()
    }
}

/// Verify that the remote probe is only queried for sessions that genuinely need it:
/// - Session A: reclaimed (manifest only), proved locally -> not queried.
/// - Session B: reclaimed, then grew (incremental shard on stage) -> queried.
/// - Session C: never reclaimed, proved on stage -> not queried.
/// - When all sessions are proven locally, remote probe is called 0 times.
/// - When remote probe fails, only Session B is marked Unverifiable.
#[test]
fn remote_query_scopes_to_only_needed_sessions() {
    let fx = multi_fixture(&[
        ("session_a", b"a-line-1\n"),
        ("session_b", b"b-line-1\n"),
        ("session_c", b"c-line-1\n"),
    ]);
    let initial = fx.collect(&unreachable("dest-a"));
    assert_eq!(initial.lines_written, 3);
    assert_eq!(initial.unverified_cursors, 0);

    let id_a = fx.session_id_for("session_a");
    let id_b = fx.session_id_for("session_b");
    let _id_c = fx.session_id_for("session_c");

    let b_fact = fact_of(&fx.stage, &id_b);

    // Reclaim session A: write manifest, delete shard body.
    let manifest = chat_stasher::manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    chat_stasher::manifest::write_manifest(&fx.stage, MACHINE, &manifest).unwrap();
    chat_stasher::stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &id_a).unwrap();

    // Reclaim session B: delete shard body, manifest retains B's initial fact.
    chat_stasher::stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &id_b).unwrap();

    // Session B grows: stage has incremental shard only (e.g. shard 2), so stage does not cover
    // the whole prefix, and shard dir is not empty (skipping manifest check).
    let b_dir = store::session_shard_dir(&fx.stage, MACHINE, &id_b);
    fs::create_dir_all(&b_dir).unwrap();
    fs::write(b_dir.join("000002.jsonl"), b"b-line-2-increment\n").unwrap();

    // Session C remains intact on stage.

    // 1. Assert remote query scope contains ONLY session B.
    let queried_sessions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let queried_clone = queried_sessions.clone();
    let probe_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe_count_clone = probe_count.clone();

    let b_id_for_closure = id_b.clone();
    let dest = DestinationView::new("dest-a", move |wanted| {
        probe_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut list: Vec<(String, String)> = wanted.iter().cloned().collect();
        list.sort();
        *queried_clone.lock().unwrap() = list;
        let mut facts = ArchiveFacts::new();
        facts.insert(
            (MACHINE.to_string(), b_id_for_closure.clone()),
            b_fact.clone(),
        );
        Ok(facts)
    });

    let second = fx.collect(&dest);
    assert_eq!(probe_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    let requested = queried_sessions.lock().unwrap().clone();
    assert_eq!(
        requested,
        vec![(MACHINE.to_string(), id_b.clone())],
        "remote query must only ask for session B, not A or C"
    );
    assert_eq!(second.unverified_cursors, 0, "session B settled in archive");

    // 2. Assert when everything is locally proven, remote probe is never called (0 calls).
    fs::remove_dir_all(&b_dir).unwrap();
    let probe_count_local = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe_count_local_clone = probe_count_local.clone();
    let dest_local = DestinationView::new("dest-a", move |_wanted| {
        probe_count_local_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ArchiveFacts::new())
    });
    let third = fx.collect(&dest_local);
    assert_eq!(
        probe_count_local.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "when all sessions are proven locally, remote probe must never be called"
    );
    assert_eq!(third.unverified_cursors, 0);

    // 3. Assert probe error makes only sessions needing remote verification Unverifiable,
    // while locally-proven sessions remain unaffected.
    fs::create_dir_all(&b_dir).unwrap();
    fs::write(b_dir.join("000002.jsonl"), b"b-line-2-increment\n").unwrap();

    let dest_err = DestinationView::new("dest-a", move |_wanted| {
        anyhow::bail!("simulated remote failure");
    });
    let report_err = fx.collect(&dest_err);
    assert_eq!(
        report_err.unverified_cursors, 1,
        "only session B should be unverifiable when probe fails"
    );
    assert_eq!(report_err.reconciliations.len(), 1);
    assert_eq!(
        report_err.reconciliations[0].session_prefix,
        chat_stasher::id::short_session_id(&id_b),
    );
}
