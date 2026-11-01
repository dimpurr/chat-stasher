//! W860 / ADR-053 Phase C1 — the machine-log collector.
//!
//! Every fixture here is **synthetic**: hand-written JSON lines in a temp
//! sandbox, with `HOME`, the XDG roots and the stage all pointing inside it.
//! Nothing in this file reads a real `~/.claude/history.jsonl`, and no line of
//! a real person's prompt history is copied into a fixture. The canaries below
//! are invented strings whose only job is to be findable.
//!
//! What the four tests pin, in the ADR's own numbering:
//!
//! * **1 — append-only increment**: a later pass reads only the delta at the
//!   validated offset, earlier sealed bytes are untouched, and a torn last line
//!   is not sealed.
//! * **2 — head rewrite keeps prior generations**: the prior generations stay
//!   byte-identical and still extractable through push and reclaim, and the
//!   rewrite is recorded as an observation rather than inferred.
//! * **5 — privacy**: typed and pasted text reach body-tier shards only; the
//!   metadata tier (activity index, ext-status, snapshot cache, stage meta,
//!   state dir) holds zero canaries.
//! * **6 — coverage isolation**: with a machine log and without one, every
//!   session number and the whole `sessions/` tree are byte-identical.

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry};
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[path = "../src/test_support.rs"]
mod test_support;

/// The in-process environment is process-global, so every test that moves
/// `HOME` (or the machine-log env override) takes this lock. Same arrangement
/// `collect_test.rs` uses for the same reason.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const FALLBACK_MACHINE: &str = "w860-synthetic-machine";
const LOG_TEMPLATE: &str = "~/.claude/history.jsonl";

/// The machine partition every test seals under.
///
/// The resolved hostname where there is one, so that a `doctor` report built by
/// the same process measures the same namespace this suite collected into —
/// `doctor` resolves its own machine and takes no argument. The fallback keeps
/// the suite runnable on a host whose identity is unavailable, because the
/// collector itself is given the name explicitly.
fn machine() -> String {
    chat_stasher::id::machine_id().unwrap_or_else(|| FALLBACK_MACHINE.to_string())
}

/// Restore one environment variable on drop, including "was not set".
struct EnvReset(&'static str, Option<std::ffi::OsString>);

impl EnvReset {
    fn capture(name: &'static str) -> Self {
        EnvReset(name, std::env::var_os(name))
    }
}

impl Drop for EnvReset {
    fn drop(&mut self) {
        match self.1.take() {
            Some(value) => std::env::set_var(self.0, value),
            None => std::env::remove_var(self.0),
        }
    }
}

/// The registry this suite scans: a `claude-code` entry whose *session* cell is
/// a synthetic projects directory and whose `machine_logs` role declares the
/// history log (ADR-053 D7).
///
/// The machine-log cell is declared for whichever platform the test runs on,
/// unlike the shipped registry where the linux/windows cells are deliberately
/// `unascertained`. The difference is the point: this fixture is a measurement
/// of nothing, so it must exercise the collector arm everywhere, while the
/// shipped cells must stay honest about what was never measured.
fn registry(project_root: &Path, with_machine_log: bool) -> HarnessRegistry {
    registry_with_log_cell(
        project_root,
        with_machine_log,
        json!({
            "template": LOG_TEMPLATE,
            "format": "jsonl",
            "confidence": "measured-locally",
            "source": "synthetic fixture"
        }),
    )
}

fn registry_with_log_cell(
    project_root: &Path,
    with_machine_log: bool,
    log_cell: serde_json::Value,
) -> HarnessRegistry {
    let platform = scanner::current_platform();
    let session_cell = json!({
        "template": project_root.to_string_lossy(),
        "format": "jsonl",
        "confidence": "source-confirmed",
        "source": "synthetic fixture"
    });
    let mut session_paths = serde_json::Map::new();
    session_paths.insert(platform.to_string(), session_cell);
    let machine_logs = if with_machine_log {
        let mut log_paths = serde_json::Map::new();
        log_paths.insert(platform.to_string(), log_cell);
        json!([{
            "id": "history",
            "role": "machine-log",
            "paths": log_paths,
            "env_override": "CLAUDE_CONFIG_DIR",
            "notes": "synthetic fixture"
        }])
    } else {
        json!([])
    };
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic",
        "harnesses": [{
            "id": "claude-code",
            "display_name": "synthetic",
            "paths": session_paths,
            "machine_logs": machine_logs
        }]
    }))
    .expect("synthetic registry")
}

fn scan(registry: &HarnessRegistry, project_root: &Path) -> scanner::ScanReport {
    let config = Config {
        claude_projects_dir: Some(project_root.to_string_lossy().into_owned()),
        ..Config::default()
    };
    scanner::scan_with_registry_and_machine(&config, registry, &machine()).expect("synthetic scan")
}

/// The destination every test collects *for*.
///
/// It is the same derived id the archive-backed view in test 2 uses, because
/// the collector's state is keyed per destination: two spellings of the same
/// archive are two destinations, and a fixture that mixed them would measure
/// its own bookkeeping rather than the feature.
fn destination_id() -> String {
    collect::destination_id("w860-synthetic-destination")
}

/// A destination whose archive cannot be consulted, so every cursor must prove
/// itself on the stage.
fn local_only<'a>() -> DestinationView<'a> {
    DestinationView::unreachable(destination_id())
}

fn log_dir(stage: &Path) -> PathBuf {
    store::machine_log_shard_dir(stage, &machine(), "claude-code", "history")
}

fn sealed(stage: &Path) -> Vec<u8> {
    store::concat_shards_in_dir(&log_dir(stage)).unwrap()
}

fn generation_files(stage: &Path) -> Vec<PathBuf> {
    store::machine_log_shard_entries(stage, &machine(), "claude-code", "history")
        .unwrap()
        .into_iter()
        .map(|(_, path)| path)
        .collect()
}

fn append(path: &Path, bytes: &[u8]) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

/// Every file under `root`, as `path relative to root → bytes`.
///
/// Relative on purpose: two worlds live in different directories, so comparing
/// absolute paths would compare the sandbox layout rather than the tree.
fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let key = path
                    .strip_prefix(root)
                    .expect("every path is under root")
                    .to_path_buf();
                out.insert(key, fs::read(&path).unwrap());
            }
        }
    }
    out
}

/// Copy a fixture tree, so a test can modify its own copy.
fn copy_tree(from: &Path, to: &Path) {
    for (relative, bytes) in snapshot_tree(from) {
        let target = to.join(&relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, bytes).unwrap();
    }
}

/// The capture stamps of one machine log, parsed.
fn capture_rows(stage: &Path) -> Vec<serde_json::Value> {
    let path = log_dir(stage).join("capture-v1.jsonl");
    let raw = fs::read_to_string(&path).unwrap_or_default();
    raw.lines()
        .map(|line| serde_json::from_str(line).expect("capture row is JSON"))
        .collect()
}

/// One synthetic history line: the shape ADR-053 F1 measured (display,
/// pastedContents, timestamp ms, project, sessionId).
fn history_line(display: &str, session: &str, at_ms: u64) -> String {
    format!(
        "{}\n",
        json!({
            "display": display,
            "pastedContents": {},
            "timestamp": at_ms,
            "project": "/synthetic/project",
            "sessionId": session,
        })
    )
}

// --------------------------------------------------------------------- test 1

#[test]
fn append_only_increment_reads_the_delta_and_leaves_sealed_bytes_alone() {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = test_support::Sandbox::new();
    sandbox.ensure_dirs();
    let _home = EnvReset::capture("HOME");
    let _override = EnvReset::capture("CLAUDE_CONFIG_DIR");
    std::env::set_var("HOME", sandbox.home());
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    let log = sandbox.home().join(".claude/history.jsonl");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(
        &log,
        [
            history_line("/first", "s-1", 1),
            history_line("/second", "s-1", 2),
        ]
        .concat(),
    )
    .unwrap();

    let project_root = sandbox.root().join("projects");
    fs::create_dir_all(&project_root).unwrap();
    let registry = registry(&project_root, true);
    let stage = sandbox.root().join("stage");
    let state = sandbox.root().join("state");

    let first = collect::collect_scan_report(
        &scan(&registry, &project_root),
        &stage,
        &machine(),
        &state,
        20,
        &local_only(),
    )
    .unwrap();

    // The machine log is collected into its own outcome, and the *session*
    // counters of this pass are exactly what they would be with no machine log
    // at all (ADR-053 D4).
    assert_eq!(
        first.machine_logs.len(),
        1,
        "one declared machine log exists"
    );
    let first_log = &first.machine_logs[0];
    assert_eq!(first_log.harness, "claude-code");
    assert_eq!(first_log.log_id, "history");
    assert_eq!(first_log.lines_written, 2);
    assert_eq!(first_log.generations, 1);
    assert_eq!(first_log.sealed_lines, 2);
    assert!(!first_log.reset);
    assert_eq!(first_log.unproven, None);
    assert_eq!(first.shards_written, 0, "no session shard was written");
    assert_eq!(first.lines_written, 0, "no session line was written");
    assert_eq!(first.changed_records, 0);
    assert_eq!(first.machine_log_generations_written, 1);
    assert_eq!(first.machine_log_lines_written, 2);

    let after_first = sealed(&stage);
    assert_eq!(
        after_first,
        [
            history_line("/first", "s-1", 1),
            history_line("/second", "s-1", 2)
        ]
        .concat()
        .as_bytes()
    );
    // No session tree was created by a machine log.
    assert!(!stage.join(store::SESSIONS_DIR).join(machine()).exists());

    // A torn last line is not sealed and does not advance the cursor past it.
    let torn = b"{\"display\":\"/third\",\"timestamp\":3,\"sessionId\":\"s-1\"}";
    append(&log, torn);
    let second = collect::collect_scan_report(
        &scan(&registry, &project_root),
        &stage,
        &machine(),
        &state,
        20,
        &local_only(),
    )
    .unwrap();
    let second_log = &second.machine_logs[0];
    assert_eq!(second_log.lines_written, 0, "a torn line is in progress");
    assert_eq!(second_log.bytes_read, torn.len() as u64);
    assert!(!second_log.reset);
    assert_eq!(second_log.generations, 1, "nothing new was sealed");
    assert_eq!(sealed(&stage), after_first);

    // The newline completes it: only the tail is read, and the earlier sealed
    // bytes are still exactly the bytes they were.
    append(&log, b"\n");
    let third = collect::collect_scan_report(
        &scan(&registry, &project_root),
        &stage,
        &machine(),
        &state,
        20,
        &local_only(),
    )
    .unwrap();
    let third_log = &third.machine_logs[0];
    assert_eq!(third_log.lines_written, 1);
    assert_eq!(third_log.bytes_read, torn.len() as u64 + 1);
    assert_eq!(third_log.prefix_bytes_validated, after_first.len() as u64);
    assert_eq!(third_log.generations, 2);
    assert_eq!(third_log.sealed_lines, 3);
    let after_third = sealed(&stage);
    assert_eq!(
        after_third,
        [after_first.clone(), torn.to_vec(), b"\n".to_vec()].concat()
    );
    assert_eq!(
        &after_third[..after_first.len()],
        &after_first[..],
        "the earlier sealed bytes are untouched"
    );

    println!(
        "append_only first_lines={} torn_bytes={} completing_bytes={} generations={} sealed_bytes={}",
        first_log.lines_written,
        second_log.bytes_read,
        third_log.bytes_read,
        third_log.generations,
        after_third.len()
    );
}

// --------------------------------------------------------------------- test 2

#[test]
fn head_rewrite_keeps_prior_generations_in_the_archive() {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = test_support::Sandbox::new();
    sandbox.ensure_dirs();
    let _home = EnvReset::capture("HOME");
    let _override = EnvReset::capture("CLAUDE_CONFIG_DIR");
    std::env::set_var("HOME", sandbox.home());
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    let log = sandbox.home().join(".claude/history.jsonl");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    let first_base = history_line("/kept-forever", "s-1", 1);
    fs::write(&log, first_base.as_bytes()).unwrap();

    let project_root = sandbox.root().join("projects");
    fs::create_dir_all(&project_root).unwrap();
    let registry = registry(&project_root, true);
    let stage = sandbox.root().join("stage");
    let state = sandbox.root().join("state");

    collect::collect_scan_report(
        &scan(&registry, &project_root),
        &stage,
        &machine(),
        &state,
        20,
        &local_only(),
    )
    .unwrap();
    let after_base = sealed(&stage);
    let base_generations = generation_files(&stage);
    assert_eq!(base_generations.len(), 1);

    // A head rewrite of the same byte length, so only the prefix hash can
    // detect it — a shorter rewrite would also be caught by the length check.
    let rewritten = history_line("/kept-second!", "s-1", 1);
    assert_eq!(rewritten.len(), first_base.len());
    fs::write(&log, rewritten.as_bytes()).unwrap();

    let second = collect::collect_scan_report(
        &scan(&registry, &project_root),
        &stage,
        &machine(),
        &state,
        20,
        &local_only(),
    )
    .unwrap();
    let second_log = &second.machine_logs[0];
    assert!(second_log.reset, "a rewritten prefix starts a new base");
    assert_eq!(
        second_log.unproven, None,
        "this is a source fact, not a cursor one"
    );
    assert_eq!(second_log.lines_written, 1);
    assert_eq!(second_log.generations, 2);

    // The prior generation is still on the stage, byte-identical, and the new
    // base was appended beside it — nothing was overwritten or deleted.
    let after_reset = sealed(&stage);
    assert_eq!(&after_reset[..after_base.len()], &after_base[..]);
    assert_eq!(
        &after_reset[after_base.len()..],
        rewritten.as_bytes(),
        "the new base is the whole current snapshot"
    );
    let after_reset_generations = generation_files(&stage);
    assert_eq!(after_reset_generations.len(), 2);
    assert_eq!(
        fs::read(&after_reset_generations[0]).unwrap(),
        fs::read(&base_generations[0]).unwrap(),
        "generation 1 is byte-identical after the rewrite"
    );

    // The rewrite is recorded as an observation, with the prior generation's
    // SHA-256 and the capture-time fidelity stamp (ADR-053 D3/D8).
    let rows = capture_rows(&stage);
    assert_eq!(rows.len(), 2, "one row per sealed generation");
    assert_eq!(rows[0]["reason"], "base");
    assert_eq!(rows[0]["fidelity"]["source"], "captured");
    assert_eq!(rows[0]["fidelity"]["value"], "raw");
    assert_eq!(rows[1]["reason"], "reset");
    assert_eq!(rows[1]["reset_cause"], "committed_prefix_changed");
    assert_eq!(
        rows[1]["prior_generations"][0]["sha256"], rows[0]["sha256"],
        "the observation names the generation it superseded, and it is still sealed"
    );

    // Push, then read the same generations back out of the archive: the prior
    // one is still extractable, byte-identical.
    let archive_root = sandbox.root().join("archive");
    let cfg = StoreConfig {
        repo_root: archive_root.to_string_lossy().into_owned(),
        key_file: sandbox.root().join("archive-key.json"),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(test_support::rustic_cache_root(sandbox.root())),
        no_cache: false,
    };
    let key = MasterKey::new();
    store::persist_key_file(&cfg, &key).unwrap();
    let archive = BackupStore::new(cfg, machine());
    archive.push(&stage, &key).unwrap();

    let readback = archive.read_cumulative_sessions(&key, None).unwrap();
    let facts = collect::machine_log_facts_from_readback(&readback);
    let fact = facts
        .get(&(machine(), "claude-code".to_string(), "history".to_string()))
        .expect("the archive holds the machine log");
    assert_eq!(fact.shard_count, 2);
    assert_eq!(fact.concat_bytes, after_reset.len() as u64);
    assert_eq!(
        fact.shard_identities[0].sha256,
        rows[0]["sha256"].as_str().unwrap()
    );

    // Reclaim the stage the way `stagereclaim` does, then collect again with a
    // destination that can answer: the cursor proves itself against the archive
    // instead of re-reading a file that did not change, and nothing new is
    // sealed.
    fs::remove_dir_all(&log_dir(&stage)).unwrap();
    let destination = DestinationView::with_machine_logs(
        destination_id(),
        |_| panic!("no session needs remote proof in this fixture"),
        || Ok(collect::machine_log_facts_from_readback(&readback)),
    );
    let third = collect::collect_scan_report(
        &scan(&registry, &project_root),
        &stage,
        &machine(),
        &state,
        20,
        &destination,
    )
    .unwrap();
    let third_log = &third.machine_logs[0];
    assert!(!third_log.reset, "an unchanged file is not a rewrite");
    assert_eq!(third_log.unproven, None, "the archive proved the cursor");
    assert_eq!(third_log.lines_written, 0);
    assert_eq!(third_log.generations, 0, "nothing was sealed again");
    assert_eq!(third.machine_log_generations_written, 0);
    assert_eq!(
        capture_rows(&stage).len(),
        0,
        "and no observation was invented"
    );

    println!(
        "head_rewrite generations={} prior_bytes={} archive_shards={} reread_after_reclaim={}",
        after_reset_generations.len(),
        after_base.len(),
        fact.shard_count,
        third_log.lines_written
    );
}

// --------------------------------------------------------------------- test 5

#[test]
fn typed_and_pasted_text_reach_body_tier_shards_only() {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = test_support::Sandbox::new();
    sandbox.ensure_dirs();
    let _home = EnvReset::capture("HOME");
    let _override = EnvReset::capture("CLAUDE_CONFIG_DIR");
    std::env::set_var("HOME", sandbox.home());
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    const TYPED: &str = "W860-CANARY-TYPED-PROMPT-9f3a";
    const PASTED: &str = "W860-CANARY-PASTED-BODY-1c2b";

    let log = sandbox.home().join(".claude/history.jsonl");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    let typed_line = format!(
        "{}\n",
        json!({
            "display": TYPED,
            "pastedContents": {"0": {"content": PASTED}},
            "timestamp": 1,
            "project": "/synthetic/project",
            "sessionId": "s-1",
        })
    );
    let slash_line = history_line("/usage", "s-2", 2);
    fs::write(&log, [typed_line.as_str(), slash_line.as_str()].concat()).unwrap();

    let project_root = sandbox.root().join("projects");
    fs::create_dir_all(&project_root).unwrap();
    let registry = registry(&project_root, true);
    let stage = sandbox.root().join("stage");
    let state = sandbox.root().join("state");

    let report = collect::collect_scan_report(
        &scan(&registry, &project_root),
        &stage,
        &machine(),
        &state,
        20,
        &local_only(),
    )
    .unwrap();
    assert_eq!(report.machine_logs[0].sealed_lines, 2);

    // Nothing is filtered at capture: the slash line is in the sealed bytes.
    let body = sealed(&stage);
    let body_text = String::from_utf8(body.clone()).unwrap();
    assert!(body_text.contains(TYPED));
    assert!(body_text.contains(PASTED));
    assert!(body_text.contains("/usage"));

    // The canaries exist in exactly the sealed generations, and nowhere else in
    // the stage, the state directory or the capture stamps beside them.
    let mut carriers: Vec<PathBuf> = Vec::new();
    for (path, bytes) in snapshot_tree(&stage)
        .into_iter()
        .chain(snapshot_tree(&state))
    {
        let haystack = String::from_utf8_lossy(&bytes).into_owned();
        if haystack.contains(TYPED) || haystack.contains(PASTED) {
            carriers.push(path);
        }
    }
    carriers.sort();
    let mut expected: Vec<PathBuf> = generation_files(&stage)
        .iter()
        .map(|path| path.strip_prefix(&stage).unwrap().to_path_buf())
        .collect();
    expected.sort();
    assert_eq!(
        carriers, expected,
        "typed and pasted text must appear only in the sealed generations"
    );

    // The metadata tier holds counts and codes, never prose: the capture stamp
    // names the log, the generation, the digests and the fidelity stamp.
    let rows = capture_rows(&stage);
    assert_eq!(rows.len(), 1);
    let stamp = serde_json::to_string(&rows[0]).unwrap();
    assert!(!stamp.contains(TYPED) && !stamp.contains(PASTED));
    assert_eq!(rows[0]["fidelity"]["value"], "raw");
    assert_eq!(rows[0]["fidelity"]["source"], "captured");

    // The collector's own counters carry counts only.
    let counters = format!("{:?}", report.machine_logs[0]);
    assert!(!counters.contains(TYPED) && !counters.contains(PASTED));

    // A `doctor` report of the same state exposes the machine log as counts and
    // flags, with the canaries still absent from the JSON.
    let doctor_json = serde_json::to_string(&doctor_machine_logs(&sandbox, &stage)).unwrap();
    assert!(!doctor_json.contains(TYPED) && !doctor_json.contains(PASTED));

    println!(
        "privacy carriers={} capture_rows={} sealed_lines={}",
        carriers.len(),
        rows.len(),
        report.machine_logs[0].sealed_lines
    );
}

/// The machine-log rows `doctor` would print for this sandbox: the real
/// `doctor::run` path, with `[native_host] stage` pointed at the fixture stage.
fn doctor_machine_logs(sandbox: &test_support::Sandbox, stage: &Path) -> serde_json::Value {
    // `Config::load` reads `$XDG_CONFIG_HOME/chat-stasher/config.toml` when that
    // variable is set, so the sandbox pins it out of the way first — a user's
    // own config must not be readable by this test either.
    let _config_home = EnvReset::capture("XDG_CONFIG_HOME");
    std::env::remove_var("XDG_CONFIG_HOME");
    let config_dir = sandbox.home().join(".config/chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!("[native_host]\nstage = \"{}\"\n", stage.display()),
    )
    .unwrap();
    let report = chat_stasher::doctor::run();
    let json = chat_stasher::doctor::report_to_json(&report)["machine_logs"].clone();
    // The row is measured from the stage this suite collected into, so the
    // counters are real numbers rather than the `unknown` tri-state.
    if scanner::current_platform() == "macos" {
        assert_eq!(
            json["present"].as_array().map(Vec::len),
            Some(1),
            "the shipped macos cell declares the history log"
        );
        assert_eq!(json["present"][0]["lines"]["count"], 2);
        assert_eq!(json["present"][0]["generations"]["count"], 1);
    }
    json
}

// --------------------------------------------------------------------- test 6

#[test]
fn a_machine_log_changes_no_session_number_and_no_session_byte() {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = test_support::Sandbox::new();
    sandbox.ensure_dirs();
    let _home = EnvReset::capture("HOME");
    let _override = EnvReset::capture("CLAUDE_CONFIG_DIR");
    std::env::set_var("HOME", sandbox.home());
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    // Two synthetic session transcripts, in the same source tree both worlds
    // read.
    let project_root = sandbox.root().join("projects");
    let sessions = project_root.join("synthetic-project");
    fs::create_dir_all(&sessions).unwrap();
    for i in 0..2 {
        fs::write(
            sessions.join(format!("019bf00d-{i:04}-4000-8000-000000000000.jsonl")),
            format!("{{\"type\":\"user\",\"n\":{i}}}\n").repeat(i + 2),
        )
        .unwrap();
    }

    // The machine log exists in `HOME` for both worlds; only one registry
    // declares it, which is exactly the "with and without" pair the ADR asks
    // for.
    let log = sandbox.home().join(".claude/history.jsonl");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(&log, history_line("/synthetic", "s-1", 1)).unwrap();

    let with_log = sandbox.root().join("world-with");
    let without_log = sandbox.root().join("world-without");

    // Each world gets its own destination state: the state file is keyed by
    // source *path* and lives per destination, so sharing one between two
    // stages would make the second world read the first world's cursors and
    // answer a question this test is not asking.
    let run = |stage: &Path, state: &Path, with_machine_log: bool| {
        let registry = registry(&project_root, with_machine_log);
        let scan = scan(&registry, &project_root);
        let first =
            collect::collect_scan_report(&scan, stage, &machine(), state, 20, &local_only())
                .unwrap();
        // A second pass over an unchanged source must also be identical across
        // the two worlds — that is where a stray machine-log count would show
        // up as a "changed" session.
        let second =
            collect::collect_scan_report(&scan, stage, &machine(), state, 20, &local_only())
                .unwrap();
        (first, second)
    };
    let (report_with, second_with) = run(&with_log, &sandbox.root().join("state-with"), true);
    let (report_without, second_without) =
        run(&without_log, &sandbox.root().join("state-without"), false);
    let second_with = &second_with;
    let second_without = &second_without;
    assert_eq!(report_with.unchanged_records, 0, "the first pass seals");
    assert_eq!(second_with.unchanged_records, report_with.scanned_records);
    assert_eq!(
        second_without.unchanged_records,
        report_without.scanned_records
    );

    // The machine log is visible, in its own fields.
    assert_eq!(report_with.machine_logs.len(), 1);
    assert_eq!(report_with.machine_log_lines_written, 1);
    assert!(report_without.machine_logs.is_empty());
    assert_eq!(report_without.machine_log_generations_written, 0);

    // Every session number is identical — in the pass that seals and in the
    // pass that finds nothing to do.
    let outcome_key = |report: &collect::CollectReport| {
        report
            .outcomes
            .iter()
            .map(|outcome| {
                (
                    outcome.session_prefix.clone(),
                    outcome.lines_written,
                    outcome.source_bytes,
                    outcome.bytes_read,
                    outcome.reset,
                )
            })
            .collect::<Vec<_>>()
    };
    for (label, with, without) in [
        ("first", &report_with, &report_without),
        ("second", second_with, second_without),
    ] {
        assert_eq!(
            with.scanned_records, without.scanned_records,
            "{label}: scanned"
        );
        assert_eq!(
            with.changed_records, without.changed_records,
            "{label}: changed"
        );
        assert_eq!(
            with.unchanged_records, without.unchanged_records,
            "{label}: unchanged"
        );
        assert_eq!(with.reset_records, without.reset_records, "{label}: reset");
        assert_eq!(
            with.shards_written, without.shards_written,
            "{label}: shards"
        );
        assert_eq!(with.lines_written, without.lines_written, "{label}: lines");
        assert_eq!(
            with.delta_bytes_read, without.delta_bytes_read,
            "{label}: bytes"
        );
        assert_eq!(
            with.prefix_bytes_validated, without.prefix_bytes_validated,
            "{label}: prefix"
        );
        assert_eq!(with.errors.len(), without.errors.len(), "{label}: errors");
        assert_eq!(with.archive_gaps, without.archive_gaps, "{label}: gaps");
        assert_eq!(
            with.scanner_unreadable_count, without.scanner_unreadable_count,
            "{label}: unreadable"
        );
        assert_eq!(
            with.machine_logs_unlooked, without.machine_logs_unlooked,
            "{label}: unlooked"
        );
        assert_eq!(outcome_key(with), outcome_key(without), "{label}: outcomes");
    }
    assert!(
        !report_without.outcomes.is_empty(),
        "both worlds must hold sessions"
    );

    // The session subtree is byte-identical, and no machine log leaked into it.
    let sessions_with = snapshot_tree(&with_log.join(store::SESSIONS_DIR));
    let sessions_without = snapshot_tree(&without_log.join(store::SESSIONS_DIR));
    assert_eq!(sessions_with, sessions_without);
    assert_eq!(
        store::sealed_shard_count(&with_log).unwrap(),
        store::sealed_shard_count(&without_log).unwrap()
    );
    assert!(with_log.join(store::MACHINE_LOGS_DIR).is_dir());
    assert!(!without_log.join(store::MACHINE_LOGS_DIR).exists());

    // The metadata-only counts that a status line reads do not move either: a
    // machine log is not a metadata file. What *does* move is the change-hash,
    // on purpose — that is what makes a machine-log pass push its content.
    use chat_stasher::metahash;
    let machine = machine();
    assert_eq!(
        metahash::count_meta_files(&with_log, &machine).unwrap(),
        metahash::count_meta_files(&without_log, &machine).unwrap()
    );
    assert_eq!(
        metahash::has_meta_files(&with_log, &machine).unwrap(),
        metahash::has_meta_files(&without_log, &machine).unwrap()
    );
    assert!(metahash::has_machine_log_files(&with_log, &machine).unwrap());
    assert!(!metahash::has_machine_log_files(&without_log, &machine).unwrap());
    // The change-hash *does* move, and the machine-log files are the whole
    // difference: strip the namespace from a copy of the first world's stage
    // and it hashes exactly like the world that never declared a machine log.
    // That is what makes a machine-log-only pass push (ADR-053 D4), while the
    // metadata-file count above stays untouched.
    let stripped = sandbox.root().join("world-with-stripped");
    copy_tree(&with_log, &stripped);
    fs::remove_dir_all(stripped.join(store::MACHINE_LOGS_DIR)).unwrap();
    assert_eq!(
        metahash::compute_meta_hash(&stripped, &machine).unwrap(),
        metahash::compute_meta_hash(&without_log, &machine).unwrap()
    );
    assert_ne!(
        metahash::compute_meta_hash(&with_log, &machine).unwrap(),
        metahash::compute_meta_hash(&without_log, &machine).unwrap()
    );

    println!(
        "coverage_isolation sessions={} shards={} lines={} machine_logs_with={} without={}",
        report_with.scanned_records,
        report_with.shards_written,
        report_with.lines_written,
        report_with.machine_logs.len(),
        report_without.machine_logs.len()
    );
}

// ------------------------------------------------------- registry / override

#[test]
fn the_shipped_registry_declares_the_history_log_with_the_documented_role() {
    let registry: HarnessRegistry =
        serde_json::from_str(include_str!("../data/harness-registry-v1.json"))
            .expect("the shipped registry parses with a machine_logs role");
    let claude = registry
        .harnesses
        .iter()
        .find(|harness| harness.id == "claude-code")
        .expect("claude-code is a declared harness");
    let logs = claude.machine_logs.as_slice();
    assert_eq!(logs.len(), 1, "exactly one machine log is declared");
    let log = &logs[0];
    assert_eq!(log.id, "history");
    assert_eq!(log.role, "machine-log");
    assert_eq!(
        log.env_override.as_deref(),
        Some("CLAUDE_CONFIG_DIR"),
        "the override is declared because the official directory docs state it relocates every ~/.claude path"
    );
    let macos = log.paths.macos.as_ref().expect("macos cell");
    assert_eq!(macos.template, "~/.claude/history.jsonl");
    assert_eq!(macos.format, "jsonl");
    assert_eq!(macos.confidence, "measured-locally");
    // The other cells stay honest about never having been measured, and the
    // session cells of the same entry are untouched by the new role.
    assert_eq!(
        log.paths.linux.as_ref().unwrap().confidence,
        "unascertained"
    );
    assert_eq!(
        log.paths.windows.as_ref().unwrap().confidence,
        "unascertained"
    );
    assert_eq!(
        claude.paths.macos.as_ref().unwrap().template,
        "~/.claude/projects/<sanitized-cwd>/<uuid>.jsonl"
    );
}

#[test]
fn the_config_dir_override_relocates_the_machine_log() {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = test_support::Sandbox::new();
    sandbox.ensure_dirs();
    let _home = EnvReset::capture("HOME");
    let _override = EnvReset::capture("CLAUDE_CONFIG_DIR");
    let project_root = sandbox.root().join("projects");
    fs::create_dir_all(&project_root).unwrap();

    let real = sandbox.home().join(".claude/history.jsonl");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::write(&real, history_line("/from-home", "s-1", 1)).unwrap();

    let relocated = sandbox.root().join("relocated");
    fs::create_dir_all(&relocated).unwrap();
    fs::write(
        relocated.join("history.jsonl"),
        history_line("/from-override", "s-2", 2),
    )
    .unwrap();

    std::env::set_var("HOME", sandbox.home());
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    let registry = registry(&project_root, true);
    let home_scan = scan(&registry, &project_root);
    assert_eq!(home_scan.machine_logs.len(), 1);
    assert!(home_scan.machine_logs[0]
        .absolute_path
        .ends_with(".claude/history.jsonl"));
    assert_eq!(home_scan.machine_logs[0].absolute_path, real);

    std::env::set_var("CLAUDE_CONFIG_DIR", &relocated);
    let overridden = scan(&registry, &project_root);
    assert_eq!(overridden.machine_logs.len(), 1);
    assert_eq!(
        overridden.machine_logs[0].absolute_path,
        relocated.join("history.jsonl")
    );

    // And the declared override is what moved it: the same scan with the
    // variable removed reads the home path again.
    std::env::remove_var("CLAUDE_CONFIG_DIR");
    assert_eq!(
        scan(&registry, &project_root).machine_logs[0].absolute_path,
        real
    );
}

/// A machine log the registry declares for a platform it has no cell for is an
/// unknown, not an absent log: it is counted as unlooked, with its reason.
#[test]
fn a_log_whose_cell_is_unascertained_is_unlooked_not_absent() {
    let _lock = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = test_support::Sandbox::new();
    sandbox.ensure_dirs();
    let _home = EnvReset::capture("HOME");
    let _override = EnvReset::capture("CLAUDE_CONFIG_DIR");
    std::env::set_var("HOME", sandbox.home());
    std::env::remove_var("CLAUDE_CONFIG_DIR");

    // The file is right there; the registry simply refuses to claim it has been
    // measured on this platform.
    let log = sandbox.home().join(".claude/history.jsonl");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(&log, history_line("/present-but-unascertained", "s-1", 1)).unwrap();

    let project_root = sandbox.root().join("projects");
    fs::create_dir_all(&project_root).unwrap();
    let registry = registry_with_log_cell(
        &project_root,
        true,
        json!({
            "template": LOG_TEMPLATE,
            "format": "jsonl",
            "confidence": "unascertained",
            "source": "synthetic fixture"
        }),
    );
    let report = scan(&registry, &project_root);
    assert!(report.machine_logs.is_empty());
    assert_eq!(report.machine_logs_unlooked, 1);
    assert_eq!(
        report.machine_logs_unlooked_reasons,
        vec!["registry cell confidence is unascertained"]
    );
    let stage = sandbox.root().join("stage");
    let collected = collect::collect_scan_report(
        &report,
        &stage,
        &machine(),
        &sandbox.root().join("state"),
        20,
        &local_only(),
    )
    .unwrap();
    assert!(collected.machine_logs.is_empty());
    assert_eq!(collected.machine_logs_unlooked, 1);
    assert!(!stage.join(store::MACHINE_LOGS_DIR).exists());
}
