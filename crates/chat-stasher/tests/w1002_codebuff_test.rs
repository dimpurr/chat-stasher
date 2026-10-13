//! W1002 — the codebuff / Freebuff harness arm (ADR-054 FB-P2), exercised over
//! synthetic fixtures that mirror the shipped registry's declared layout:
//! `~/.config/manicode[-dev|-staging]/projects/<basename>/chats/<chatId>/`.
//!
//! Nothing real is read: every root is a temp directory behind an isolated
//! HOME (or an explicit `FREEBUFF_CONFIG_DIR`), every transcript is a
//! synthetic JSON array, and the assertions inspect counts, ids, byte sizes,
//! digests and closed-enum words — never message content. The canary test
//! is the one place an anything-shaped token is planted, and it exists to
//! prove the token cannot reach a control tier.
//!
//! The cell under test is the shipped registry entry with one test-only
//! change: every `unascertained` cell's confidence is raised to
//! `source-confirmed`, so the fixture root resolves on every OS this suite
//! runs on (the shipped Linux and Windows cells stay `unascertained` because
//! no install was measured there; the rules under test here are the root
//! resolution, the id composition and the skip path, not the confidence
//! gate). [`raised_entry`] strips and deep-compares the shipped shape, so
//! any other drift between the copy and the shipped file is a failure.
//!
//! Env-mutating tests run one at a time under [`ENV_LOCK`] and restore the
//! environment through [`EnvRestore`] (the w37 discipline): `HOME` and
//! `USERPROFILE` carry the isolated home, and `FREEBUFF_CONFIG_DIR` is the
//! codebuff entry's declared entry-wide override. The spawned-CLI tests
//! pass their whole environment to the child, so they need no lock.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};

use chat_stasher::codebuff;
use chat_stasher::collect;
use chat_stasher::collect::DestinationView;
use chat_stasher::config::Config;
use chat_stasher::manifest;
use chat_stasher::models::SessionRecord;
use chat_stasher::scanner::{self, HarnessRegistry};
use chat_stasher::store;

#[path = "../src/test_support.rs"]
mod test_support;

/// Env mutation is process-global and cargo runs a target's tests in
/// parallel threads.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const MUTATED_ENV: [&str; 3] = ["HOME", "USERPROFILE", "FREEBUFF_CONFIG_DIR"];

struct EnvRestore(Vec<(&'static str, Option<OsString>)>);

impl EnvRestore {
    fn now() -> Self {
        Self(
            MUTATED_ENV
                .iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        )
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (name, previous) in self.0.drain(..) {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// The lock serialises the env-mutating tests; the snapshot undoes each
/// one's bookkeeping at scope exit, panic or not. Field order is the design
/// (the w37 audit): the environment restores *while the lock is held*.
struct ScanEnv {
    _env: EnvRestore,
    _lock: MutexGuard<'static, ()>,
}

impl ScanEnv {
    fn acquire() -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let env = EnvRestore::now();
        Self {
            _lock: lock,
            _env: env,
        }
    }

    fn isolate_home_at(&self, home: &Path) {
        std::env::set_var("HOME", home);
        std::env::set_var("USERPROFILE", home);
        std::env::remove_var("FREEBUFF_CONFIG_DIR");
    }
}

const MACHINE: &str = "w1002-probe-machine";
const CHAT_ID: &str = "2026-10-05T18-07-03.513Z";

/// Strip the `confidence` words from every cell, so the shipped entry and
/// the test's raised copy can be deep-compared over everything else.
fn without_confidence(entry: &serde_json::Value) -> serde_json::Value {
    let mut stripped = entry.clone();
    for root in stripped["source_roots"]
        .as_array_mut()
        .expect("source_roots is an array")
    {
        let paths = root["paths"].as_object_mut().expect("paths is an object");
        for cell in paths.values_mut() {
            cell.as_object_mut()
                .expect("each cell is an object")
                .remove("confidence");
        }
    }
    stripped
}

/// The shipped `codebuff` registry entry with every cell's confidence raised
/// to `source-confirmed` (see the file doc). Everything else must be
/// byte-for-byte the shipped shape, which the stripped deep-equality below
/// enforces: a test that regenerated the layout from memory would drift and
/// fail, exactly like the w37 "straight copy, not a re-spelling" rule.
fn raised_entry() -> serde_json::Value {
    let shipped: serde_json::Value =
        serde_json::from_str(include_str!("../data/harness-registry-v1.json"))
            .expect("shipped registry must be valid JSON");
    let entry = shipped["harnesses"]
        .as_array()
        .expect("harnesses is an array")
        .iter()
        .find(|h| h["id"] == "codebuff")
        .expect("the shipped registry carries codebuff")
        .clone();
    let mut raised = entry.clone();
    for root in raised["source_roots"]
        .as_array_mut()
        .expect("source_roots is an array")
    {
        for os in ["macos", "linux", "windows"] {
            if let Some(cell) = root["paths"][os].as_object_mut() {
                assert!(
                    matches!(
                        cell.get("confidence").and_then(|c| c.as_str()),
                        Some("unascertained") | Some("source-confirmed")
                    ),
                    "the fixture only lifts `unascertained` cells; the shipped shape changed"
                );
                cell.insert(
                    "confidence".to_string(),
                    serde_json::json!("source-confirmed"),
                );
            }
        }
    }
    assert_eq!(
        without_confidence(&entry),
        without_confidence(&raised),
        "only the confidence words may differ from the shipped entry"
    );
    raised
}

fn raised_registry() -> HarnessRegistry {
    serde_json::from_value(serde_json::json!({
        "schema_version": 1,
        "generated": "W1002 synthetic fixture",
        "harnesses": [raised_entry()],
    }))
    .expect("the raised entry must deserialize into the registry schema")
}

fn scan_raised(config: &Config) -> scanner::ScanReport {
    scanner::scan_with_registry_and_machine(config, &raised_registry(), MACHINE).unwrap()
}

/// A synthetic transcript: one valid JSON array with the given messages.
fn transcript(messages: &[serde_json::Value]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::Value::Array(messages.to_vec())).unwrap()
}

fn user_message(id: &str, content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "variant": "user",
        "content": content,
        "timestamp": "04:16 AM",
        "metadata": {"runState": {"sessionState": {"synthetic": true}}},
    })
}

fn config_home() -> PathBuf {
    PathBuf::from(
        std::env::var_os("HOME").expect("the isolated home must be set before planting fixtures"),
    )
}

fn default_config_root() -> PathBuf {
    config_home().join(".config").join("manicode")
}

/// Plant one complete chat directory under a config root and return the
/// transcript path. `meta` is the whitelist's second file, sealed when
/// present.
fn plant_chat(
    root: &Path,
    basename: &str,
    chat_name: &str,
    transcript_bytes: &[u8],
    meta: Option<&[u8]>,
) -> PathBuf {
    let dir = root
        .join("projects")
        .join(basename)
        .join("chats")
        .join(chat_name);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("chat-messages.json");
    fs::write(&path, transcript_bytes).unwrap();
    if let Some(meta) = meta {
        fs::write(dir.join("chat-meta.json"), meta).unwrap();
    }
    path
}

fn plant_default_home_chat(basename: &str, chat_name: &str, transcript_bytes: &[u8]) -> PathBuf {
    plant_chat(
        &default_config_root(),
        basename,
        chat_name,
        transcript_bytes,
        None,
    )
}

fn chat_meta(first_prompt: &str, size: u64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "messageCount": 1,
        "firstPrompt": first_prompt,
        "messagesSize": size,
        "messagesMtimeMs": 1,
    }))
    .unwrap()
}

fn dest<'a>() -> DestinationView<'a> {
    // These tests exercise the local read path, so the destination archive
    // is deliberately not reachable: a debt still owed on the stage must be
    // provable without ever asking it.
    DestinationView::unreachable("fixture-destination")
}

/// Collect a scan into a fresh stage/state pair and return the report.
fn collect_into(scan: &scanner::ScanReport, dir: &Path) -> collect::CollectReport {
    let stage = dir.join("stage");
    let state = dir.join("state");
    collect::collect_scan_report(scan, &stage, MACHINE, &state, 20, &dest()).unwrap()
}

fn stage_path(report_dir: &Path) -> PathBuf {
    report_dir.join("stage")
}

fn record_for<'a>(scan: &'a scanner::ScanReport, needle: &str) -> &'a SessionRecord {
    scan.records
        .iter()
        .find(|record| record.id.contains(needle))
        .unwrap_or_else(|| panic!("no record carries {needle:?}"))
}

// ---------------------------------------------------------------------------
// Tests 1 + 7 (identity): the id is `<basename>.<chatId>`, and the start time
// is a captured field, never parsed back out of the id.
// ---------------------------------------------------------------------------

#[test]
fn ids_come_from_basename_and_chat_id_and_do_not_collapse_across_projects() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    let root = default_config_root();
    plant_chat(
        &root,
        "project-one",
        CHAT_ID,
        &transcript(&[user_message("m1", "hello")]),
        None,
    );
    // The same timestamp chat id under a *different* project basename: the
    // ADR's named collision. The last-component rule would collapse these
    // two directories into one archive slot.
    plant_chat(
        &root,
        "project-two",
        CHAT_ID,
        &transcript(&[user_message("m2", "hello")]),
        None,
    );
    // A second chat under the *same* basename: a third, distinct id.
    plant_chat(
        &root,
        "project-one",
        "2026-10-05T19-00-00.001Z",
        &transcript(&[user_message("m3", "hello")]),
        None,
    );
    // A basename that itself contains dots (and a timestamp-shaped tail):
    // the composite id keeps its dots, and the captured start time is the
    // chat directory's instant, never the basename's tail.
    plant_chat(
        &root,
        "2026-10-09T01-02-03.000Z-backup",
        "2026-10-05T20-00-00.000Z",
        &transcript(&[user_message("m4", "hello")]),
        None,
    );

    let scan = scan_raised(&Config::default());
    assert_eq!(scan.records.len(), 4, "one record per chat directory");
    let native_ids: std::collections::BTreeSet<String> = scan
        .records
        .iter()
        .map(|record| {
            record
                .id
                .splitn(3, '.')
                .nth(2)
                .expect("the ladder keeps two separators only")
                .to_string()
        })
        .collect();
    // The ladder keeps dots inside the native id (D3): `splitn(3, '.')`
    // stops after two separators, so everything after `codebuff.<machine>`
    // is the id — including its own dots.
    let expected: std::collections::BTreeSet<String> = [
        format!("project-one.{CHAT_ID}"),
        format!("project-two.{CHAT_ID}"),
        "project-one.2026-10-05T19-00-00.001Z".to_string(),
        "2026-10-09T01-02-03.000Z-backup.2026-10-05T20-00-00.000Z".to_string(),
    ]
    .into_iter()
    .collect();
    assert_eq!(native_ids, expected, "no two chats collapsed into one slot");

    // The captured start time (test 7): recorded at capture with its source
    // named, from the chat directory's *own* name.
    for (needle, chat_instant) in [
        ("project-one.2026-10-05T18-07", "2026-10-05T18:07:03.513Z"),
        ("project-two.2026-10-05T18-07", "2026-10-05T18:07:03.513Z"),
        ("project-one.2026-10-05T19-00", "2026-10-05T19:00:00.001Z"),
        ("backup.2026-10-05T20-00", "2026-10-05T20:00:00.000Z"),
    ] {
        let record = record_for(&scan, needle);
        let facts = record
            .session_dir
            .as_ref()
            .expect("codebuff records carry facts");
        let captured = facts
            .captured_start
            .as_ref()
            .expect("shape-valid name captured");
        assert_eq!(captured.source, "chat-directory-name");
        assert_eq!(
            captured.time, chat_instant,
            "the captured time is the chat directory name's own instant, never the id's"
        );
    }
    // The dotted, timestamp-shaped basename is identity, not a time source.
    let dotted = record_for(&scan, "backup");
    assert_eq!(
        dotted
            .session_dir
            .as_ref()
            .unwrap()
            .captured_start
            .as_ref()
            .unwrap()
            .time,
        "2026-10-05T20:00:00.000Z",
        "the basename's timestamp-shaped tail is identity, never a session start"
    );
}

#[test]
fn a_chat_directory_name_outside_the_accepted_shape_records_no_start_time() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    let root = default_config_root();
    plant_chat(
        &root,
        "p1",
        "renamed-by-hand",
        &transcript(&[user_message("m", "hello")]),
        None,
    );
    plant_chat(
        &root,
        "p1",
        "2026-10-05T18-07-03.5Z",
        &transcript(&[user_message("m", "hi")]),
        None,
    );

    let scan = scan_raised(&Config::default());
    assert_eq!(scan.records.len(), 2);
    for record in &scan.records {
        let facts = record.session_dir.as_ref().unwrap();
        assert!(
            facts.captured_start.is_none(),
            "an unaccepted name records unknown, never a guess or a file time"
        );
        assert!(
            facts.dir_mtime.is_some(),
            "the cross-check input is captured even when no start time is"
        );
    }
}

#[test]
fn the_captured_start_time_survives_manifest_regeneration() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    plant_default_home_chat("p1", CHAT_ID, &transcript(&[user_message("m1", "hello")]));

    let scan = scan_raised(&Config::default());
    let dir = tempfile::tempdir().unwrap();
    let report = collect_into(&scan, dir.path());
    assert_eq!(report.codebuff_torn_skips.len(), 0);
    let id = scan.records[0].id.clone();
    let stage = stage_path(dir.path());

    // The stage-side file is what regeneration reads, so the value reaches
    // the manifest row without anyone re-deriving it from the id (D4).
    let target = store::session_shard_dir(&stage, MACHINE, &id);
    let on_disk = store::read_captured_start_in_dir(&target)
        .unwrap()
        .expect("written at capture");
    assert_eq!(on_disk.time, "2026-10-05T18:07:03.513Z");
    assert_eq!(on_disk.source, "chat-directory-name");

    let rows = manifest::generate_manifest_at(&stage, MACHINE, 1_760_000_000).unwrap();
    assert_eq!(rows.len(), 1);
    let captured = rows[0]
        .captured_start
        .as_ref()
        .expect("regenerated row keeps the value");
    assert_eq!(captured.time, "2026-10-05T18:07:03.513Z");
    assert_eq!(captured.source, "chat-directory-name");
    // The recorded time is not the file's own time: the row's bookkeeping
    // stamp is the regeneration instant, and the captured fact is what it
    // read off the stage side file.
    assert_eq!(rows[0].sealed_at_unix, 1_760_000_000);

    // A deleted stage-side record is an honest unknown and is never filled
    // with a file time; absent means unknown, never zero.
    fs::remove_file(target.join(store::CAPTURED_START_FILE)).unwrap();
    let rows = manifest::generate_manifest_at(&stage, MACHINE, 1_760_000_001).unwrap();
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].captured_start.is_none(),
        "a fact whose record is gone is unknown, not approximated"
    );
}

#[test]
fn a_directory_mtime_preceding_the_recorded_time_is_observed_and_the_time_stands() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    // A chat name in the (shape-valid, calendar-valid) future: a directory
    // on disk cannot have been created before its own recorded session
    // start, so this fixture makes the cross-check observe a mismatch
    // without needing to forge file times.
    plant_default_home_chat(
        "p1",
        "2099-01-01T00-00-00.000Z",
        &transcript(&[user_message("m1", "hello")]),
    );

    let scan = scan_raised(&Config::default());
    assert_eq!(scan.records.len(), 1);
    let facts = scan.records[0].session_dir.as_ref().unwrap();
    assert_eq!(
        facts.captured_start.as_ref().unwrap().time,
        "2099-01-01T00:00:00.000Z",
        "the recorded time stands while the mismatch is observed"
    );

    let dir = tempfile::tempdir().unwrap();
    let report = collect_into(&scan, dir.path());
    assert_eq!(report.errors.len(), 0);
    assert_eq!(report.codebuff_torn_skips.len(), 0);
    let stage = stage_path(dir.path());
    // The cross-check landed in the stage's control stream, bounded fields
    // only, and the recorded start time is still what it was.
    let text = fs::read_to_string(
        stage
            .join(manifest::META_DIR)
            .join(MACHINE)
            .join(codebuff::CROSSCHECK_FILE),
    )
    .expect("one mismatch, one observation");
    let row: codebuff::CrossCheckObservation =
        serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(row.result, codebuff::CROSSCHECK_RESULT_PRECEDES);
    assert_eq!(row.recorded_start, "2099-01-01T00:00:00.000Z");
    assert_eq!(row.recorded_start_source, "chat-directory-name");
    assert!(
        row.dir_mtime_unix > 1_577_836_800 && row.dir_mtime_unix < 4_102_444_800,
        "the observed mtime is a real recent measurement, before the recorded 2099 start"
    );
    assert_eq!(row.root_id, "codebuff");
    assert!(row.session_native_id.ends_with("2099-01-01T00-00-00.000Z"));

    // A clean cross-check observes nothing: 2020 chats on a 2026 disk are
    // preceded by no mtime, so they leave no row.
    plant_default_home_chat(
        "p2",
        "2020-01-01T00-00-00.000Z",
        &transcript(&[user_message("m2", "hi")]),
    );
    let scan = scan_raised(&Config::default());
    let second = tempfile::tempdir().unwrap();
    collect_into(&scan, second.path());
    let exists = fs::read_to_string(
        stage_path(second.path())
            .join(manifest::META_DIR)
            .join(MACHINE)
            .join(codebuff::CROSSCHECK_FILE),
    )
    .unwrap_or_default();
    assert!(
        exists.lines().all(|line| line.contains("2099")),
        "only the future-named chat cross-checks as preceded: {exists}"
    );
}

// ---------------------------------------------------------------------------
// Tests 2 + 8: the whitelist, and the sidecar that rides the same identity.
// ---------------------------------------------------------------------------

#[test]
fn the_whitelist_takes_two_files_and_ignores_everything_else() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    let root = default_config_root();
    let body = transcript(&[user_message("m1", "hello")]);
    let meta = chat_meta("first prompt", body.len() as u64);
    let path = plant_chat(&root, "p1", CHAT_ID, &body, Some(&meta));
    let dir = path.parent().unwrap();
    // Deliberately excluded by the whitelist (D5): the multi-megabyte-ish
    // run state, the debug log, and an atomic-write temp file left behind
    // mid-write.
    fs::write(dir.join("run-state.json"), vec![b'x'; 65_536]).unwrap();
    fs::write(dir.join("log.jsonl"), b"debug\ndebug\n").unwrap();
    fs::write(
        dir.join("chat-messages.json.1234.9f0e1d2c-3b4a-4c5d-8e9f-0a1b2c3d4e5f.tmp"),
        b"[]",
    )
    .unwrap();

    let scan = scan_raised(&Config::default());
    assert_eq!(
        scan.records.len(),
        1,
        "exactly one record: the transcript, never its neighbours"
    );
    let record = &scan.records[0];
    assert_eq!(
        record.byte_size,
        body.len() as u64,
        "the byte size is the transcript's, not a neighbour's"
    );
    let facts = record.session_dir.as_ref().unwrap();
    assert_eq!(facts.extra_files.len(), 1, "chat-meta.json is whitelisted");
    assert_eq!(facts.extra_files[0].name, "chat-meta.json");
    assert_eq!(facts.extra_files[0].byte_size, meta.len() as u64);
    assert_eq!(facts.root_id.as_deref(), Some("codebuff"));

    // Collect, then audit the stage directory shape: the body is only the
    // transcript's generations, and every excluded file sealed nothing.
    let out = tempfile::tempdir().unwrap();
    let report = collect_into(&scan, out.path());
    assert_eq!(report.errors.len(), 0);
    assert_eq!(report.changed_records, 1);
    let stage_dir = store::session_shard_dir(&stage_path(out.path()), MACHINE, &record.id);
    let shards = store::sealed_shard_entries(&stage_dir).unwrap();
    assert_eq!(shards.len(), 1, "one generation sealed");
    let concat = store::concat_shards(&stage_path(out.path()), MACHINE, &record.id).unwrap();
    // Raw fidelity modulo the sealed-shard line framing: the shard seam
    // terminates each sealed element with one newline (the same seam grok's
    // usage.json rides, D9's precedent), so a byte-identical seal is the
    // file's exact bytes with the framing's single trailing newline.
    assert_eq!(
        concat.strip_suffix(b"\n"),
        Some(body.as_slice()),
        "raw fidelity: byte-identical to the file modulo the framing newline (D2)"
    );
    let names: Vec<String> = fs::read_dir(&stage_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.contains(&store::CAPTURED_START_FILE.to_string()),
        "the capture-time record sits beside the shards: {names:?}"
    );
    assert_eq!(
        names
            .iter()
            .filter(|name| store::is_extra_file(name))
            .count(),
        1,
        "the sidecar sealed once: {names:?}"
    );
    for name in &names {
        assert!(
            !name.starts_with("run-state") && !name.starts_with("log.") && !name.contains(".tmp"),
            "excluded files sealed nothing: {names:?}"
        );
    }
}

#[test]
fn a_sidecar_seals_raw_under_the_same_identity_and_never_counts_as_a_session() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    let body = transcript(&[user_message("m1", "hello")]);
    let path = plant_default_home_chat("p1", CHAT_ID, &body);

    let scan_a = scan_raised(&Config::default());
    assert_eq!(scan_a.records.len(), 1);
    let out = tempfile::tempdir().unwrap();
    collect_into(&scan_a, out.path());
    let id = scan_a.records[0].id.clone();
    let stage = stage_path(out.path());
    assert_eq!(
        manifest::generate_manifest_at(&stage, MACHINE, 1)
            .unwrap()
            .len(),
        1,
        "one session before the sidecar exists"
    );

    // The sidecar appears later — and is *stale* against the transcript
    // (D5/F9): present files are sealed when present, and staleness changes
    // no count and no identity.
    let meta = chat_meta("a stale summary", 999_999);
    fs::write(path.parent().unwrap().join("chat-meta.json"), &meta).unwrap();

    let scan_b = scan_raised(&Config::default());
    assert_eq!(
        scan_b.records.len(),
        1,
        "the sidecar adds no session record"
    );
    assert_eq!(scan_b.records[0].id, id, "the sidecar changes no identity");
    let report = collect_into(&scan_b, out.path());
    assert_eq!(report.errors.len(), 0);
    assert_eq!(
        report.changed_records, 0,
        "the transcript is unchanged; a sidecar is not a session turn"
    );

    let stage_dir = store::session_shard_dir(&stage, MACHINE, &id);
    let extras: Vec<PathBuf> = fs::read_dir(&stage_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| store::is_extra_file(&path.file_name().unwrap().to_string_lossy()))
        .collect();
    assert_eq!(extras.len(), 1, "one sealed sidecar generation");
    let bytes = fs::read_to_string(&extras[0]).unwrap();
    assert_eq!(
        bytes,
        String::from_utf8_lossy(&meta),
        "the sidecar's raw bytes are kept intact (D5)"
    );
    // The sidecar is not a shard and not a session: the manifest still
    // counts one session whose concat is only the transcript.
    let rows = manifest::generate_manifest_at(&stage, MACHINE, 2).unwrap();
    assert_eq!(rows.len(), 1, "still one session, one manifest row");
    assert_eq!(rows[0].shard_count, 1, "the sidecar is not a shard");
    assert_eq!(
        store::concat_shards(&stage, MACHINE, &id)
            .unwrap()
            .strip_suffix(b"\n"),
        Some(body.as_slice()),
        "the body is only the transcript's generations"
    );
    // A changed sidecar appends a generation rather than rewriting history,
    // the same one-generation-per-observation shape the shards keep.
    let meta_two = chat_meta("a second summary", 2);
    fs::write(path.parent().unwrap().join("chat-meta.json"), &meta_two).unwrap();
    let scan_c = scan_raised(&Config::default());
    collect_into(&scan_c, out.path());
    let extras: Vec<PathBuf> = fs::read_dir(&stage_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| store::is_extra_file(&path.file_name().unwrap().to_string_lossy()))
        .collect();
    assert_eq!(
        extras.len(),
        2,
        "a changed sidecar appends, it does not rewrite"
    );
}

#[test]
fn an_absent_sidecar_is_a_normal_state() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    plant_default_home_chat("p1", CHAT_ID, &transcript(&[user_message("m1", "hello")]));
    let scan = scan_raised(&Config::default());
    assert_eq!(scan.records.len(), 1);
    assert!(
        scan.records[0]
            .session_dir
            .as_ref()
            .unwrap()
            .extra_files
            .is_empty(),
        "an absent sidecar is a normal state, not an error"
    );
    let out = tempfile::tempdir().unwrap();
    let report = collect_into(&scan, out.path());
    assert_eq!(report.errors.len(), 0);
    assert_eq!(report.changed_records, 1);
}

// ---------------------------------------------------------------------------
// Tests 3 + 4: raw fidelity, and the torn-read skip with exit code 3.
// ---------------------------------------------------------------------------

#[test]
fn sealed_bytes_are_byte_identical_including_unknown_and_truncated_fields() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    // Unknown fields, a block whose type this build does not name, a
    // payload the harness itself truncated (F17), and a heavy unrendered
    // runState — none of them repaired, reordered or pretty-printed (D2).
    let body = serde_json::json!([
        {
            "id": "w1002-m1",
            "variant": "user",
            "content": "synthetic prompt",
            "unknownFutureField": {"nested": [1, 2, 3]},
            "metadata": {"runState": {"sessionState": {"mainAgentState": {"messageHistory": [null, null]}}}},
        },
        {
            "id": "w1002-m2",
            "variant": "ai",
            "content": "[TRUNCATED_BY_SRC]",
            "blocks": [{"type": "not-in-this-build", "prob": 1.0}],
        }
    ]);
    let bytes = serde_json::to_vec_pretty(&body).unwrap();
    plant_default_home_chat("p1", CHAT_ID, &bytes);

    let scan = scan_raised(&Config::default());
    let out = tempfile::tempdir().unwrap();
    let report = collect_into(&scan, out.path());
    assert_eq!(report.codebuff_torn_skips.len(), 0);
    let id = scan.records[0].id.clone();
    let concat = store::concat_shards(&stage_path(out.path()), MACHINE, &id).unwrap();
    assert_eq!(
        concat.strip_suffix(b"\n"),
        Some(bytes.as_slice()),
        "raw fidelity: nothing is repaired, reordered or re-serialized (D2/F17)"
    );
}

#[test]
fn a_torn_transcript_skips_the_pass_and_a_completed_file_archives_on_the_next_pass() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    let torn: &[u8] = br#"[{"variant":"user","content":"synthetic turn"},{"variant":"ai","con"#;
    let path = plant_default_home_chat("p1", CHAT_ID, torn);

    let scan_a = scan_raised(&Config::default());
    let out = tempfile::tempdir().unwrap();
    let report = collect_into(&scan_a, out.path());
    assert_eq!(report.errors.len(), 0, "a skip is not an error (D6)");
    assert_eq!(report.changed_records, 0, "nothing was sealed");
    assert_eq!(
        report.unchanged_records, 0,
        "a skipped session is a third state: neither changed nor unchanged"
    );
    assert_eq!(report.codebuff_torn_skips.len(), 1, "one skipped session");
    let skip = &report.codebuff_torn_skips[0];
    assert_eq!(
        skip.reason,
        codebuff::TranscriptSkipReason::TornJson.as_str()
    );
    assert_eq!(skip.byte_size, torn.len() as u64);
    assert_eq!(skip.root_id, "codebuff");
    assert_eq!(skip.session_native_id, format!("p1.{CHAT_ID}"));
    let stage = stage_path(out.path());
    let id = scan_a.records[0].id.clone();
    assert!(
        store::sealed_shard_entries(&store::session_shard_dir(&stage, MACHINE, &id))
            .unwrap()
            .is_empty(),
        "nothing was sealed for the skipped session, not even partial bytes"
    );
    let observations = codebuff::read_skip_observations(&stage, MACHINE).unwrap();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].reason, "torn-json");
    assert_eq!(observations[0].attempt, 1);
    assert_eq!(observations[0].root_id, "codebuff");
    assert_eq!(observations[0].byte_size, torn.len() as u64);
    assert_eq!(
        observations[0].session_native_id,
        format!("p1.{CHAT_ID}"),
        "the skip observation names directory-name components, never a path"
    );

    // Skip twice more: the attempt count says how many passes went unread.
    collect_into(&scan_a, out.path());
    collect_into(&scan_a, out.path());
    let observations = codebuff::read_skip_observations(&stage, MACHINE).unwrap();
    assert_eq!(observations.len(), 3);
    assert_eq!(observations[2].attempt, 3);

    // The file completes: the next pass archives it normally (D6), and the
    // pass after that recognizes the generation as unchanged.
    let whole = transcript(&[
        user_message("m1", "synthetic turn"),
        user_message("m2", "synthetic reply"),
    ]);
    fs::write(&path, &whole).unwrap();
    let scan_b = scan_raised(&Config::default());
    let report = collect_into(&scan_b, out.path());
    assert_eq!(report.codebuff_torn_skips.len(), 0);
    let concat = store::concat_shards(&stage, MACHINE, &id).unwrap();
    assert!(
        concat
            .strip_suffix(b"\n")
            .is_some_and(|tail| tail.ends_with(&whole)),
        "the completed generation archives byte-exactly"
    );
    let scan_c = scan_raised(&Config::default());
    let third = collect_into(&scan_c, out.path());
    assert_eq!(third.codebuff_torn_skips.len(), 0);
    assert_eq!(third.changed_records, 0);
    assert_eq!(third.unchanged_records, 1);
    // And the attempt counter does not grow once the session reads clean.
    let observations = codebuff::read_skip_observations(&stage, MACHINE).unwrap();
    assert_eq!(observations.len(), 3, "a clean pass adds no observation");
}

#[test]
fn a_valid_non_array_transcript_is_its_own_closed_enum_reason() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    plant_default_home_chat("p1", CHAT_ID, br#"{"messageCount": 3}"#);

    let scan = scan_raised(&Config::default());
    let out = tempfile::tempdir().unwrap();
    let report = collect_into(&scan, out.path());
    assert_eq!(report.codebuff_torn_skips.len(), 1);
    assert_eq!(
        report.codebuff_torn_skips[0].reason,
        codebuff::TranscriptSkipReason::NotAnArray.as_str()
    );
    let observations = codebuff::read_skip_observations(&stage_path(out.path()), MACHINE).unwrap();
    assert_eq!(observations[0].reason, "not-an-array");
}

#[test]
fn a_valid_empty_array_archives_as_an_empty_chat_and_is_distinguishable_from_a_skip() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    plant_default_home_chat("p-empty", CHAT_ID, b"[]");
    // Torn: a valid array head with unparsable bytes behind it.
    plant_default_home_chat("p-torn", "2026-10-05T18-07-03.514Z", b"[]truncated-tail");

    let scan = scan_raised(&Config::default());
    assert_eq!(scan.records.len(), 2);
    let out = tempfile::tempdir().unwrap();
    let report = collect_into(&scan, out.path());
    // D6's three states: `[]` is a successful read of an empty chat
    // (changed, sealed), the torn bytes skipped — distinguishable in the
    // report and in the stage.
    assert_eq!(
        report.changed_records, 1,
        "the valid [] sealed a real generation"
    );
    assert_eq!(report.unchanged_records, 0);
    assert_eq!(
        report.codebuff_torn_skips.len(),
        1,
        "the torn bytes skipped"
    );
    let empty = record_for(&scan, "p-empty");
    let concat = store::concat_shards(&stage_path(out.path()), MACHINE, &empty.id).unwrap();
    assert_eq!(
        concat.strip_suffix(b"\n"),
        Some(&b"[]"[..]),
        "an empty chat is archived as an empty chat (D6)"
    );
    assert_eq!(
        store::sealed_shard_entries(&store::session_shard_dir(
            &stage_path(out.path()),
            MACHINE,
            &record_for(&scan, "p-torn").id
        ))
        .unwrap()
        .len(),
        0,
        "the skipped session left no body at all"
    );
}

// ---------------------------------------------------------------------------
// Test 5 (rev 2): one snapshot for validation and sealing.
// ---------------------------------------------------------------------------

#[test]
fn an_atomic_replacement_mid_read_seals_exactly_one_generation_never_a_mix() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    // A big valid generation, so a read takes a measurable moment.
    let one: Vec<serde_json::Value> = (0..20_000)
        .map(|i| user_message(&format!("w1002-g1-{i}"), "generation one"))
        .collect();
    let g1 = transcript(&one);
    let path = plant_default_home_chat("p1", CHAT_ID, &g1);

    let scan_a = scan_raised(&Config::default());
    let out = tempfile::tempdir().unwrap();
    let report_a = collect_into(&scan_a, out.path());
    assert_eq!(report_a.codebuff_torn_skips.len(), 0);

    let two: Vec<serde_json::Value> = (0..20_000)
        .map(|i| user_message(&format!("w1002-g2-{i}"), "generation two"))
        .collect();
    let g2 = transcript(&two);

    // Atomically replace the transcript while a pass is reading it. The
    // collector reads one buffer and validates that buffer, so the pass
    // either finished before the rename (the file is unchanged) or read
    // one full generation through its open file descriptor — g1 or g2
    // exactly, never bytes from both (D6 rev 2).
    let swap = {
        let path = path.clone();
        let g2 = g2.clone();
        move || {
            let tmp = path.with_extension("tmp-swap");
            fs::write(&tmp, &g2).unwrap();
            fs::rename(&tmp, &path).unwrap();
        }
    };
    let scan = scan_raised(&Config::default());
    let collector_dir = out.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(2));
        swap();
    });
    let report = collect_into(&scan, &collector_dir);
    handle.join().unwrap();
    assert_eq!(report.errors.len(), 0);
    assert_eq!(report.codebuff_torn_skips.len(), 0);

    let id = scan.records[0].id.clone();
    let entries = store::sealed_shard_entries(&store::session_shard_dir(
        &stage_path(&collector_dir),
        MACHINE,
        &id,
    ))
    .unwrap();
    assert!(
        entries.len() == 1 || entries.len() == 2,
        "either the rename lost the race or it did not: both are whole outcomes"
    );
    let mut previous: Option<Vec<u8>> = None;
    for (_, shard) in &entries {
        let bytes = fs::read(shard).unwrap();
        // The shard seam terminates each sealed element with one newline
        // (the same framing every whole-file seam uses); modulo that single
        // trailing byte, a sealed generation is exactly the snapshot that
        // was validated — never a mix of the two generations (D6 rev 2).
        let generation: &[u8] = match bytes.strip_suffix(b"\n") {
            Some(generation) => generation,
            None => &bytes,
        };
        assert!(
            generation == g1 || generation == g2,
            "every sealed generation is one whole snapshot, never a mix (D6 rev 2)"
        );
        if let Some(previous) = &previous {
            assert_ne!(generation, previous.as_slice(), "the generations differ");
        }
        previous = Some(generation.to_vec());
    }
    // A later deterministic pass over the replaced file converges on it.
    let scan_b = scan_raised(&Config::default());
    let report_b = collect_into(&scan_b, &collector_dir);
    assert_eq!(report_b.codebuff_torn_skips.len(), 0);
    let mut entries = store::sealed_shard_entries(&store::session_shard_dir(
        &stage_path(&collector_dir),
        MACHINE,
        &id,
    ))
    .unwrap();
    entries.sort_by_key(|(seq, _)| *seq);
    let (_, newest) = entries.last().expect("generations exist");
    assert_eq!(
        fs::read(newest).unwrap().strip_suffix(b"\n"),
        Some(g2.as_slice()),
        "the newest generation is the file's current state"
    );
}

// ---------------------------------------------------------------------------
// Test 6 (rev 3): the privacy canary, two fixtures.
// ---------------------------------------------------------------------------

const CANARY: &str = "w1002-canary-unique-token-x9q";

#[test]
fn the_canary_reaches_the_body_tier_only_and_never_a_control_record() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    // (a) Valid-capture fixture: the canary sits in the transcript bytes and
    // in chat-meta.json's firstPrompt — both body tier, sealed as such.
    let good_body = transcript(&[user_message("m1", CANARY)]);
    let good_meta = chat_meta(CANARY, good_body.len() as u64);
    plant_default_home_chat("p-good", CHAT_ID, &good_body);
    let dir = default_config_root()
        .join("projects")
        .join("p-good")
        .join("chats")
        .join(CHAT_ID);
    fs::write(dir.join("chat-meta.json"), &good_meta).unwrap();
    // (b) Torn-read fixture: the canary sits inside bytes that fail
    // validation mid-token.
    let torn = format!(r#"[{{"variant":"user","content":"{}"#, CANARY).into_bytes();
    plant_default_home_chat("p-torn", "2026-10-05T18-07-03.514Z", &torn);

    let scan = scan_raised(&Config::default());
    let out = tempfile::tempdir().unwrap();
    let report = collect_into(&scan, out.path());
    assert_eq!(
        report.codebuff_torn_skips.len(),
        1,
        "the torn fixture skipped"
    );
    assert_eq!(report.changed_records, 1, "the valid capture sealed");

    let stage = stage_path(out.path());
    // (a) The canary is inside the sealed body — where content belongs —
    // and inside the sealed raw sidecar, which carries a `firstPrompt`: a
    // truncated copy of user text, body tier too (D9).
    let good = record_for(&scan, "p-good");
    let body = store::concat_shards(&stage, MACHINE, &good.id).unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains(CANARY), "the body tier may carry content");
    let good_stage_dir = store::session_shard_dir(&stage, MACHINE, &good.id);
    let extra = fs::read_dir(&good_stage_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| store::is_extra_file(&path.file_name().unwrap().to_string_lossy()))
        .expect("the sidecar sealed beside the body");
    assert!(
        fs::read_to_string(&extra).unwrap().contains(CANARY),
        "the sealed raw sidecar is body tier too"
    );

    // (a) + (b): every control surface says zero canaries — the skip
    // stream, the cross-check stream, the in-memory report, and the
    // manifest rows.
    let skip_text = fs::read_to_string(
        stage
            .join(manifest::META_DIR)
            .join(MACHINE)
            .join(codebuff::SKIP_FILE),
    )
    .unwrap();
    assert!(
        !skip_text.contains(CANARY),
        "the canary sat inside bytes that failed validation; a control row cannot carry it"
    );
    let crosscheck_text = fs::read_to_string(
        stage
            .join(manifest::META_DIR)
            .join(MACHINE)
            .join(codebuff::CROSSCHECK_FILE),
    )
    .unwrap_or_default();
    assert!(!crosscheck_text.contains(CANARY));
    assert!(
        !format!("{report:?}").contains(CANARY),
        "the run report cannot carry content"
    );
    for row in manifest::generate_manifest_at(&stage, MACHINE, 1).unwrap() {
        assert!(!format!("{row:?}").contains(CANARY));
    }

    // (b) The torn fixture sealed nothing at all: no shard, no sidecar, no
    // captured-start record — a skip is a whole-session skip.
    let torn_id = record_for(&scan, "p-torn").id.clone();
    let torn_dir = store::session_shard_dir(&stage, MACHINE, &torn_id);
    assert!(
        store::sealed_shard_entries(&torn_dir).unwrap().is_empty(),
        "no shard behind a skipped session"
    );
    assert!(
        !torn_dir.join(store::CAPTURED_START_FILE).exists(),
        "a skipped session records facts for nothing"
    );
    let remaining = fs::read_dir(&torn_dir);
    match remaining {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("unexpected read failure: {e}"),
        Ok(entries) => {
            let leftovers: Vec<String> = entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert!(
                leftovers.is_empty(),
                "the skipped session's stage directory holds nothing at all: {leftovers:?}"
            );
        }
    }
    // And the observation about it names identity and a size — the size is
    // a measurement, and no byte of the measured content rides along.
    let observations = codebuff::read_skip_observations(&stage, MACHINE).unwrap();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].byte_size, torn.len() as u64);
    assert_eq!(observations[0].reason, "torn-json");
}

// ---------------------------------------------------------------------------
// Tests 9 + 10: the entry-wide override, both states.
// ---------------------------------------------------------------------------

#[test]
fn the_env_override_moves_the_whole_entry_and_suppresses_the_default_roots() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    // Default roots under the isolated home: one session per channel that
    // the defaults must find when the override is off, and none of which
    // may be read while the override is on.
    let primary = home.path().join(".config").join("manicode");
    let dev = home.path().join(".config").join("manicode-dev");
    let staging = home.path().join(".config").join("manicode-staging");
    plant_chat(
        &primary,
        "only-in-default",
        "2026-10-05T18-00-00.000Z",
        &transcript(&[user_message("m", "primary")]),
        None,
    );
    plant_chat(
        &dev,
        "only-in-dev",
        "2026-10-05T18-00-01.000Z",
        &transcript(&[user_message("m", "dev")]),
        None,
    );
    plant_chat(
        &staging,
        "only-in-staging",
        "2026-10-05T18-00-02.000Z",
        &transcript(&[user_message("m", "staging")]),
        None,
    );
    // The moved override root: `FREEBUFF_CONFIG_DIR` replaces the whole
    // base, and one chat here also exists in the default primary root with
    // the same basename and chat id, to pin the exactly-once rule.
    let moved = tempfile::tempdir().unwrap();
    let moved_root = moved.path().join("anywhere-config");
    plant_chat(
        &moved_root,
        "moved-only",
        "2026-10-05T18-00-03.000Z",
        &transcript(&[user_message("m", "moved")]),
        None,
    );
    plant_chat(
        &moved_root,
        "twice-present",
        "2026-10-05T18-00-04.000Z",
        &transcript(&[user_message("m", "moved copy")]),
        None,
    );
    plant_chat(
        &primary,
        "twice-present",
        "2026-10-05T18-00-04.000Z",
        &transcript(&[user_message("m", "default copy")]),
        None,
    );

    // Override set (D7): exactly one root — the override value. The three
    // default roots are not scanned; a session present twice counts once.
    std::env::set_var("FREEBUFF_CONFIG_DIR", moved_root.as_os_str());
    let scan = scan_raised(&Config::default());
    assert_eq!(
        scan.records.len(),
        2,
        "the override root is the only root while the variable is set"
    );
    for record in &scan.records {
        let facts = record.session_dir.as_ref().unwrap();
        assert_eq!(
            facts.root_id.as_deref(),
            Some("codebuff"),
            "the primary root's id names the whole entry while the override is in force"
        );
        assert!(
            record.absolute_path.starts_with(&moved_root),
            "the collected session must be the override copy, not the default one"
        );
        assert!(
            record.provenance.is_empty(),
            "the override root is not a Surface: Dimension remains unobserved"
        );
    }
    let ids: std::collections::BTreeSet<&str> =
        scan.records.iter().map(|r| r.id.as_str()).collect();
    assert!(
        !ids.iter().any(|id| id.contains("only-in-default")
            || id.contains("only-in-dev")
            || id.contains("only-in-staging")),
        "no default-only root contributes a record while the override is set: {ids:?}"
    );

    // The twice-present chat archives exactly once, under one slot.
    let out = tempfile::tempdir().unwrap();
    let report = collect_into(&scan, out.path());
    assert_eq!(report.errors.len(), 0);
    assert_eq!(report.changed_records, 2);
    let manifest_rows =
        manifest::generate_manifest_at(&stage_path(out.path()), MACHINE, 1).unwrap();
    assert_eq!(
        manifest_rows.len(),
        2,
        "one archive slot for the twice-present chat"
    );
}

#[test]
fn without_the_override_all_three_channel_roots_scan_and_write_no_dimension() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    let primary = home.path().join(".config").join("manicode");
    let dev = home.path().join(".config").join("manicode-dev");
    let staging = home.path().join(".config").join("manicode-staging");
    plant_chat(
        &primary,
        "in-prod",
        "2026-10-05T18-00-00.000Z",
        &transcript(&[user_message("m", "prod")]),
        None,
    );
    plant_chat(
        &dev,
        "in-dev",
        "2026-10-05T18-00-01.000Z",
        &transcript(&[user_message("m", "dev")]),
        None,
    );
    plant_chat(
        &staging,
        "in-staging",
        "2026-10-05T18-00-02.000Z",
        &transcript(&[user_message("m", "staging")]),
        None,
    );

    let scan = scan_raised(&Config::default());
    assert_eq!(
        scan.records.len(),
        3,
        "all three roots contribute a session"
    );
    let mut roots: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for record in &scan.records {
        let facts = record.session_dir.as_ref().unwrap();
        roots.insert(facts.root_id.clone().unwrap());
        assert!(
            record.provenance.is_empty(),
            "the channel is not a surface: dimensions stay empty (D1/D10)"
        );
    }
    assert_eq!(
        roots,
        std::collections::BTreeSet::from([
            "codebuff".to_string(),
            "codebuff-dev".to_string(),
            "codebuff-staging".to_string(),
        ]),
        "each root walks under its own registry id"
    );
}

// ---------------------------------------------------------------------------
// Test 11 (rev 5): a long basename bounds the id and keeps the captured time.
// ---------------------------------------------------------------------------

#[test]
fn a_long_basename_bounds_the_id_and_the_recorded_start_time_survives() {
    let env = ScanEnv::acquire();
    let home = tempfile::tempdir().unwrap();
    env.isolate_home_at(home.path());
    let root = default_config_root();
    // One basename that still fits (the whole ladder string stays under
    // 255 bytes) and two that do not, differing only in their last
    // character — the bounded form must keep them distinct (D3/rev 5).
    let fitting = "x".repeat(200);
    let mut base_a = "b".repeat(248);
    base_a.push('a');
    let mut base_b = "b".repeat(248);
    base_b.push('c');
    plant_chat(
        &root,
        &fitting,
        CHAT_ID,
        &transcript(&[user_message("m1", "hello")]),
        None,
    );
    plant_chat(
        &root,
        &base_a,
        CHAT_ID,
        &transcript(&[user_message("m2", "hello")]),
        None,
    );
    plant_chat(
        &root,
        &base_b,
        CHAT_ID,
        &transcript(&[user_message("m3", "hello")]),
        None,
    );

    let scan = scan_raised(&Config::default());
    assert_eq!(scan.records.len(), 3);
    let fitting_record = record_for(&scan, &format!(".{fitting}."));
    assert!(
        fitting_record.id.ends_with(CHAT_ID),
        "a fitting basename keeps the id's time-shaped tail"
    );
    let bounded: Vec<&SessionRecord> = scan
        .records
        .iter()
        .filter(|record| record.id.starts_with("codebuff.w1002-probe-machine.b"))
        .collect();
    assert_eq!(bounded.len(), 2, "the two over-long basenames bounded");
    assert_ne!(
        bounded[0].id, bounded[1].id,
        "the tag carries the uniqueness"
    );
    for record in bounded.iter().chain(std::iter::once(&fitting_record)) {
        assert!(
            record.id.len() <= 255,
            "the whole ladder string is bounded as one path component (D3)"
        );
        assert!(
            record.id.bytes().all(|b| b != b'/'),
            "the bounded id is a single path component"
        );
        let facts = record.session_dir.as_ref().unwrap();
        let captured = facts.captured_start.as_ref().unwrap();
        assert_eq!(
            captured.time, "2026-10-05T18:07:03.513Z",
            "the captured field is untouched by the bounding (D4/rev 5)"
        );
    }
    for record in &bounded {
        assert!(
            !record.id.ends_with(CHAT_ID),
            "a bounded id carries no time-shaped tail to be misread"
        );
    }

    // A long basename whose chat directory does not match the accepted
    // shape records no start time, and none is invented out of the id.
    let before: std::collections::BTreeSet<String> =
        scan.records.iter().map(|r| r.id.clone()).collect();
    plant_chat(
        &root,
        &base_a,
        "renamed",
        &transcript(&[user_message("m4", "hello")]),
        None,
    );
    let scan_two = scan_raised(&Config::default());
    let new_record = scan_two
        .records
        .iter()
        .find(|record| !before.contains(&record.id))
        .expect("the renamed chat is the one new session");
    assert!(
        new_record
            .session_dir
            .as_ref()
            .unwrap()
            .captured_start
            .is_none(),
        "a name that does not match the pattern records no start time, and a bounded id's tail invents none"
    );
}

// ---------------------------------------------------------------------------
// Test 4's run-level interface: exit code 3 through the real CLI.
// ---------------------------------------------------------------------------

fn run_cli_collect(sandbox: &Path, home: &Path) -> Output {
    let registry = sandbox.join("registry.json");
    fs::write(
        &registry,
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 1,
            "generated": "W1002 synthetic fixture",
            "harnesses": [raised_entry()],
        }))
        .unwrap(),
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args([
            "collect",
            "--stage",
            sandbox.join("stage").to_str().unwrap(),
            "--machine",
            MACHINE,
        ])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(home),
        )
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .env_remove("FREEBUFF_CONFIG_DIR")
        .output()
        .unwrap()
}

#[test]
fn a_torn_session_makes_the_collect_run_exit_three_without_a_canary() {
    let sandbox = tempfile::tempdir().unwrap();
    let home = sandbox.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let torn = format!(r#"[{{"variant":"user","content":"{}"#, CANARY).into_bytes();
    let root = home.join(".config").join("manicode");
    plant_chat(&root, "p-torn", CHAT_ID, &torn, None);
    plant_chat(
        &root,
        "p-healthy",
        "2026-10-05T18-07-03.514Z",
        &transcript(&[user_message("m", "clean")]),
        None,
    );

    let output = run_cli_collect(sandbox.path(), &home);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(3),
        "a run that skipped a torn session did not finish reading (ADR-054 D6/F23); stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout
            .contains("[collect] codebuff skips : 1 session(s) not archived this pass (torn-json)"),
        "the report names the count and the closed-enum reason; stdout={stdout}"
    );
    assert!(
        stderr.contains("PARTIAL exit_code=3"),
        "the exit code is explained in words; stderr={stderr}"
    );
    assert!(
        !stdout.contains(CANARY) && !stderr.contains(CANARY),
        "no run output may carry the torn bytes' content"
    );
    // The healthy session still archived: the shards written above are real,
    // and the run is incomplete, not failed.
    assert!(
        stdout.contains("changed=1 unchanged=0 reset=0 shards=1 lines=1"),
        "a skip poisons neither the pass nor its real results; stdout={stdout}"
    );
}

#[test]
fn a_clean_codebuff_collect_run_exits_zero() {
    let sandbox = tempfile::tempdir().unwrap();
    let home = sandbox.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = home.join(".config").join("manicode");
    plant_chat(
        &root,
        "p-ok",
        CHAT_ID,
        &transcript(&[user_message("m", "clean")]),
        None,
    );

    let output = run_cli_collect(sandbox.path(), &home);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a pass with nothing skipped and no gap is success; stdout={stdout}\nstderr={stderr}"
    );
}
