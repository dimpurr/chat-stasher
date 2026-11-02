//! W1064: a registry cell whose declared `format` / `session_pattern` cannot
//! match the file name its tool really writes is a source that reads
//! "supported" and counts zero. aider (`.aider.chat.history.md`), Continue
//! (`<uuid>.json` beside an index `sessions.json`) and crush (`crush.db`, a
//! WAL-mode SQLite database) were all in that state.
//!
//! These tests drive the SHIPPED registry (the data file itself), with real
//! on-disk file names, through the scan and through `collect`. Negatives pin
//! that the new names do not pull in unrelated `.md`, `.db` or `.json` files.
//! All fixtures are synthetic; no real conversation text.

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry, ProbeState};
use chat_stasher::store;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[path = "../src/test_support.rs"]
mod test_support;
use test_support::Sandbox;

const MACHINE: &str = "synthetic-machine";
const UUID_A: &str = "11111111-2222-4333-8444-555555555555";
const UUID_B: &str = "66666666-7777-4888-9999-000000000000";

fn shipped_registry_json() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("data/harness-registry-v1.json");
    serde_json::from_str(&fs::read_to_string(path).expect("read shipped registry"))
        .expect("shipped registry is JSON")
}

/// The shipped entry for `id` alone, so a scan never walks the real HOME.
fn shipped_registry_only(id: &str) -> HarnessRegistry {
    let all = shipped_registry_json();
    let entry = all["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == id)
        .unwrap_or_else(|| panic!("shipped registry has no {id}"))
        .clone();
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic",
        "harnesses": [entry],
    }))
    .expect("shipped entry parses")
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn scan_root(id: &str, root: &Path) -> scanner::ScanReport {
    let config = Config {
        harness_roots: BTreeMap::from([(id.to_string(), root.to_string_lossy().into_owned())]),
        ..Config::default()
    };
    scanner::scan_with_registry_and_machine(&config, &shipped_registry_only(id), MACHINE).unwrap()
}

fn records_of<'a>(
    report: &'a scanner::ScanReport,
    short: &str,
) -> Vec<&'a chat_stasher::models::SessionRecord> {
    let prefix = format!("{short}.{MACHINE}.");
    report
        .records
        .iter()
        .filter(|r| r.id.starts_with(&prefix))
        .collect()
}

// ---------------------------------------------------------------------------
// aider
// ---------------------------------------------------------------------------

#[test]
fn aider_chat_history_is_collected_one_session_per_project_directory() {
    let sandbox = Sandbox::new();
    let root = sandbox.root().join("projects");
    // aider writes `.aider.chat.history.md` at the git root of each project.
    write(
        &root.join("proj-a/.aider.chat.history.md"),
        "# synthetic A\n",
    );
    write(
        &root.join("proj-b/.aider.chat.history.md"),
        "# synthetic B\n",
    );
    // Not chat history: aider's own sidecars, and unrelated markdown.
    write(&root.join("proj-a/.aider.input.history"), "synthetic\n");
    write(&root.join("proj-a/.aider.llm.history"), "synthetic\n");
    write(&root.join("proj-a/README.md"), "# readme\n");
    write(&root.join("proj-a/notes.md"), "# notes\n");
    write(&root.join("proj-a/CHANGELOG.md"), "# log\n");
    write(&root.join("proj-b/chat.history.md"), "# not aider\n");

    let report = scan_root("aider", &root);
    let records = records_of(&report, "aider");
    assert_eq!(
        records.len(),
        2,
        "exactly the two history files: {records:?}"
    );
    assert_ne!(
        records[0].id, records[1].id,
        "the constant file name must not merge two projects into one archive slot"
    );
    for record in &records {
        assert_eq!(
            record.absolute_path.file_name().unwrap(),
            ".aider.chat.history.md"
        );
    }

    // And collection (not only the scan count) archives both bodies.
    let stage = sandbox.root().join("stage");
    let state = sandbox.root().join("state/collector.json");
    collect::collect_scan_report(
        &report,
        &stage,
        MACHINE,
        &state,
        20,
        &DestinationView::unreachable("synthetic-destination"),
    )
    .unwrap();
    let mut bodies: Vec<String> = records
        .iter()
        .map(|r| String::from_utf8(store::concat_shards(&stage, MACHINE, &r.id).unwrap()).unwrap())
        .collect();
    bodies.sort();
    // The stage terminates the stored body with a newline.
    let bodies: Vec<&str> = bodies.iter().map(|b| b.trim_end()).collect();
    assert_eq!(bodies, vec!["# synthetic A", "# synthetic B"]);
}

#[test]
fn aider_root_may_be_the_project_directory_itself() {
    let sandbox = Sandbox::new();
    let root = sandbox.root().join("only-project");
    write(&root.join(".aider.chat.history.md"), "# synthetic\n");
    write(&root.join("notes.md"), "# notes\n");
    let report = scan_root("aider", &root);
    assert_eq!(records_of(&report, "aider").len(), 1);
}

#[test]
fn aider_same_basename_in_two_places_stays_two_sessions() {
    let sandbox = Sandbox::new();
    let root = sandbox.root().join("projects");
    write(&root.join("one/app/.aider.chat.history.md"), "# one\n");
    write(&root.join("two/app/.aider.chat.history.md"), "# two\n");
    let report = scan_root("aider", &root);
    let records = records_of(&report, "aider");
    assert_eq!(records.len(), 2);
    assert_ne!(records[0].id, records[1].id);
}

// ---------------------------------------------------------------------------
// Continue
// ---------------------------------------------------------------------------

#[test]
fn continue_session_files_are_collected_and_the_index_and_config_are_not() {
    let sandbox = Sandbox::new();
    let root = sandbox.root().join(".continue/sessions");
    let body_a =
        r#"{"sessionId":"a","title":"synthetic","workspaceDirectory":"/synthetic","history":[]}"#;
    write(&root.join(format!("{UUID_A}.json")), body_a);
    write(&root.join(format!("{UUID_B}.json")), body_a);
    // The per-folder index and unrelated json beside it.
    write(&root.join("sessions.json"), "[]");
    write(&root.join("config.json"), "{}");
    write(&root.join("settings.json"), "{}");
    write(&root.join("notes.json"), "{}");

    let report = scan_root("continue", &root);
    let records = records_of(&report, "continue");
    let mut ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![
            format!("continue.{MACHINE}.{UUID_A}"),
            format!("continue.{MACHINE}.{UUID_B}")
        ],
        "exactly the two uuid-named session files"
    );

    let stage = sandbox.root().join("stage");
    let state = sandbox.root().join("state/collector.json");
    collect::collect_scan_report(
        &report,
        &stage,
        MACHINE,
        &state,
        20,
        &DestinationView::unreachable("synthetic-destination"),
    )
    .unwrap();
    let body = store::concat_shards(&stage, MACHINE, &ids[0].to_string()).unwrap();
    assert_eq!(String::from_utf8(body).unwrap().trim_end(), body_a);
}

// ---------------------------------------------------------------------------
// crush
// ---------------------------------------------------------------------------

#[test]
fn crush_is_not_claimed_while_there_is_no_sqlite_reader() {
    // crush.db is a WAL-mode SQLite database and this build has no reader for
    // its schema. Copying the file whole is not a consistent snapshot of a live
    // database, so the registry must not promise a scan.
    let registry = shipped_registry_json();
    let crush = registry["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == "crush")
        .expect("crush entry");
    for platform in ["macos", "linux", "windows"] {
        assert_eq!(
            crush["paths"][platform]["confidence"], "unascertained",
            "crush/{platform} must not read as scannable without a reader"
        );
    }

    let report = scanner::scan_with_registry_and_machine(
        &Config::default(),
        &shipped_registry_only("crush"),
        MACHINE,
    )
    .unwrap();
    let probe = report.probes.iter().find(|p| p.id == "crush").unwrap();
    assert_eq!(probe.state, ProbeState::SkipUnascertained);
}

#[test]
fn crush_db_in_a_walked_directory_is_never_collected_as_a_file() {
    let sandbox = Sandbox::new();
    let root = sandbox.root().join("crush-root");
    write(&root.join(".crush/crush.db"), "SQLite format 3\0synthetic");
    write(&root.join(".crush/crush.db-wal"), "synthetic");
    let report = scan_root("crush", &root);
    assert!(
        records_of(&report, "crush").is_empty(),
        "a raw copy of a live WAL database is not collected"
    );
}

// ---------------------------------------------------------------------------
// Every registry entry: the declared format matches a realistic file name
// ---------------------------------------------------------------------------

/// Realistic on-disk file (relative to the walked root) per generic file-walk
/// source. Key is `<harness>` for the primary store, `<harness>:<root id>` for
/// an additional source root.
fn realistic_files() -> BTreeMap<&'static str, Vec<String>> {
    let rollout = format!("2026/11/01/rollout-2026-11-01T00-00-00-{UUID_A}");
    BTreeMap::from([
        (
            "claude-code",
            vec![format!("-synthetic-proj/{UUID_A}.jsonl")],
        ),
        (
            "claude-code:claude-desktop-cowork",
            vec![format!(
                "org/acct/local_{UUID_A}/.claude/projects/p/{UUID_B}.jsonl"
            )],
        ),
        (
            "codex",
            vec![format!("{rollout}.jsonl"), format!("{rollout}.jsonl.zst")],
        ),
        (
            "codex:codex-archived",
            vec![format!("{rollout}.jsonl"), format!("{rollout}.jsonl.zst")],
        ),
        (
            "gemini-cli",
            vec![
                "session-2026-11-01T00-00-abcd1234.json".to_string(),
                "session-2026-11-01T00-00-abcd1234.jsonl".to_string(),
            ],
        ),
        ("aider", vec!["proj/.aider.chat.history.md".to_string()]),
        ("continue", vec![format!("{UUID_A}.json")]),
        (
            "kimi-code",
            vec![format!("ws1/session_{UUID_A}/agents/main/wire.jsonl")],
        ),
        (
            "deepseek-harness",
            vec!["ab/cd/session.v4.jsonl.zstd".to_string()],
        ),
        (
            "google-antigravity:antigravity",
            vec!["conv1/.system_generated/logs/transcript.jsonl".to_string()],
        ),
        (
            "google-antigravity:antigravity-cli",
            vec!["conv1/.system_generated/logs/transcript.jsonl".to_string()],
        ),
        (
            "google-antigravity:antigravity-ide",
            vec!["conv1/.system_generated/logs/transcript.jsonl".to_string()],
        ),
        (
            "codebuff:codebuff",
            vec!["projects/proj/chats/2026-11-01T00-00-00.000Z/chat-messages.json".to_string()],
        ),
        (
            "codebuff:codebuff-dev",
            vec!["projects/proj/chats/2026-11-01T00-00-00.000Z/chat-messages.json".to_string()],
        ),
        (
            "codebuff:codebuff-staging",
            vec!["projects/proj/chats/2026-11-01T00-00-00.000Z/chat-messages.json".to_string()],
        ),
    ])
}

/// Entries whose store is read by a dedicated prober (not the generic file
/// walk). Their file-name acceptance is pinned by their own suites.
const DEDICATED_PROBER: &[(&str, &str)] = &[
    (
        "grok-bot",
        "grok_bot_*_test (reads the app-support replica store)",
    ),
    ("workbuddy", "workbuddy_scanner_test"),
    ("codebuddy", "codebuddy_scanner_test"),
    (
        "github-copilot-cli",
        "dedicated probe_github_copilot_cli_harness",
    ),
    ("openclaw", "dedicated probe_openclaw_agents"),
];

/// SQLite-format harnesses that have a per-session reader in this build. A
/// `sqlite` cell for any other harness would be collected, if at all, as a
/// raw whole-file copy of a live database.
const SQLITE_WITH_READER: &[&str] = &[
    "opencode",
    "hermes-agent",
    "cursor",
    "grok",
    "zed",
    "openclaw",
    "github-copilot-cli",
];

fn is_scannable(cell: &Value) -> bool {
    cell["confidence"] != "unascertained"
}

#[test]
fn every_scannable_cell_declares_a_format_that_matches_a_realistic_file_name() {
    let registry = shipped_registry_json();
    let realistic = realistic_files();
    let mut covered: Vec<String> = Vec::new();
    let mut unclassified: Vec<String> = Vec::new();

    for harness in registry["harnesses"].as_array().unwrap() {
        let id = harness["id"].as_str().unwrap();
        // Primary store plus every additional source root.
        let mut cells: Vec<(String, Value)> = Vec::new();
        if let Some(paths) = harness.get("paths") {
            cells.push((id.to_string(), paths.clone()));
        }
        for root in harness
            .get("source_roots")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            cells.push((
                format!("{id}:{}", root["id"].as_str().unwrap()),
                root["paths"].clone(),
            ));
        }
        for (key, paths) in cells {
            for platform in ["macos", "linux", "windows"] {
                let Some(cell) = paths.get(platform) else {
                    continue;
                };
                if !is_scannable(cell) {
                    continue;
                }
                let format = cell["format"].as_str().unwrap_or_default();
                if format.contains("sqlite") {
                    assert!(
                        SQLITE_WITH_READER.contains(&id),
                        "{key}/{platform}: declares sqlite but this build has no session reader \
                         for {id}; a scannable sqlite cell without one is a raw copy of a live database"
                    );
                    continue;
                }
                if DEDICATED_PROBER.iter().any(|(d, _)| *d == id) {
                    continue;
                }
                let Some(files) = realistic.get(key.as_str()) else {
                    unclassified.push(format!("{key}/{platform}"));
                    continue;
                };
                let sandbox = Sandbox::new();
                let root = sandbox.root().join("root");
                fs::create_dir_all(&root).unwrap();
                for file in files {
                    write(&root.join(file), "synthetic\n");
                }
                // One-cell registry: this exact cell (format, pattern,
                // session_dir) as the entry's primary store.
                let mut one = cell.clone();
                one["template"] = json!(root.to_string_lossy());
                // The env override would send the scan to the real install.
                one.as_object_mut().unwrap().remove("env_override");
                let platform_now = scanner::current_platform();
                let entry = json!({
                    "id": id,
                    "display_name": id,
                    "paths": { platform_now: one },
                });
                let reg: HarnessRegistry = serde_json::from_value(json!({
                    "schema_version": 1,
                    "generated": "synthetic",
                    "harnesses": [entry],
                }))
                .unwrap();
                let report =
                    scanner::scan_with_registry_and_machine(&Config::default(), &reg, MACHINE)
                        .unwrap();
                let probe = report.probes.iter().find(|p| p.id == id).unwrap();
                assert_eq!(
                    probe.state,
                    ProbeState::Scanned,
                    "{key}/{platform}: {}",
                    probe.note
                );
                let mut paths_seen: Vec<PathBuf> = report
                    .records
                    .iter()
                    .map(|r| r.absolute_path.clone())
                    .collect();
                paths_seen.sort();
                let mut want: Vec<PathBuf> = files.iter().map(|f| root.join(f)).collect();
                want.sort();
                // Compare file names: the sandbox may be reached through a symlink.
                let names = |v: &[PathBuf]| -> Vec<std::ffi::OsString> {
                    v.iter()
                        .map(|p| p.file_name().unwrap().to_owned())
                        .collect()
                };
                assert_eq!(
                    names(&paths_seen),
                    names(&want),
                    "{key}/{platform}: the declared format/pattern must accept every realistic \
                     file name ({format:?})"
                );
                covered.push(format!("{key}/{platform}"));
            }
        }
    }
    assert!(
        unclassified.is_empty(),
        "scannable registry cells with neither a realistic file name here, a dedicated prober \
         nor a sqlite reader: {unclassified:?}"
    );
    assert!(
        covered.len() >= 20,
        "the table must actually cover cells: {covered:?}"
    );
}
