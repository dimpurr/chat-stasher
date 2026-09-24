//! W19 · `export` — write **exactly** what `search` selects, byte for byte.
//!
//! The fixture is one synthetic local repository holding two machines and two
//! harnesses:
//!
//! * `m-alpha` — two `claude-code` sessions with an activity index, so their
//!   conversation times are known. Their lines are Claude Code shaped
//!   (`type` / `message.content` / `timestamp`) so the turns filter and the
//!   line trim have something real to decide on, and one line in each carries
//!   no timestamp at all;
//! * `m-beta` — one `codex` session. Its index was **removed and the machine
//!   re-pushed**, so the newest snapshot holds the sessions and no index: its
//!   conversation time is unknown, which is the state the export must never
//!   turn into "nothing".
//!
//! What these tests are for, in order of importance:
//!
//! 1. the written set equals `search`'s matched set for the same flags, over
//!    several combinations, including one that selects nothing;
//! 2. the bytes on disk are byte-identical to what `read` returns for that
//!    session, and the manifest's sha256 is the digest of the file on disk;
//! 3. `--turns user` filters where the format is certain and says
//!    `not-supported` where it is not — dropping nothing silently;
//! 4. `--trim-to-window` drops out-of-window lines, keeps untimed ones, and
//!    counts them;
//! 5. a non-empty `--out` is refused, nothing is written outside `--out`, and
//!    nothing is deleted;
//! 6. the exit codes 0 / 1 / 3 / 2 are each reachable and each mean what the
//!    help text says.
//!
//! Privacy line: assertions read the fixture's synthetic lines and count them.
//! Nothing here prints a session id beyond its first 8 characters, and no real
//! archive, key or destination is ever touched.

use chat_stasher::activity::{ActivityRow, TimeSource as ActivityTimeSource};
use chat_stasher::export::{self, ExportOptions, Turns};
use chat_stasher::search::search_sessions;
use chat_stasher::selector::{Selector, SelectorArgs};
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 2024-01-01T00:00:00Z. Far enough from any plausible "now" in a test run that
/// a window around the push and a window around the conversation cannot
/// accidentally overlap.
const JAN_2024: i64 = 1_704_067_200;
const DAY: i64 = 86_400;

const CC_ONE: &str = "claude-code.m-alpha.aaaaaaaa-0000-0000-0000-000000000001";
const CC_TWO: &str = "claude-code.m-alpha.aaaaaaaa-0000-0000-0000-000000000002";
const CODEX: &str = "codex.m-beta.bbbbbbbb-0000-0000-0000-000000000001";

/// `2024-01-<day>T<hh>:00:00Z` and its unix seconds, from one piece of
/// arithmetic so the fixture's clock and its text cannot disagree.
fn jan(day: u32, hh: u32) -> (String, i64) {
    let unix = JAN_2024 + (day as i64 - 1) * DAY + hh as i64 * 3_600;
    (format!("2024-01-{day:02}T{hh:02}:00:00Z"), unix)
}

// ---------------------------------------------------------------- line shapes

fn user_line(text: &str, ts: Option<&str>) -> String {
    match ts {
        Some(ts) => format!(
            r#"{{"type":"user","message":{{"role":"user","content":"{text}"}},"timestamp":"{ts}"}}"#
        ),
        None => format!(r#"{{"type":"user","message":{{"role":"user","content":"{text}"}}}}"#),
    }
}

fn assistant_line(text: &str, ts: Option<&str>) -> String {
    match ts {
        Some(ts) => format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}},"timestamp":"{ts}"}}"#
        ),
        None => format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        ),
    }
}

/// A tool's output. Claude Code routes it through `type: "user"`, which is
/// exactly why `type == "user"` alone is not the user-turns rule.
fn tool_result_line(ts: &str) -> String {
    format!(
        r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","content":"ok"}}]}},"timestamp":"{ts}"}}"#
    )
}

// ------------------------------------------------------------------ fixture

fn cfg(repo: &Path, key: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: None,
        no_cache: false,
    }
}

fn stage_path(root: &Path, machine: &str) -> PathBuf {
    root.join(format!("stage-{machine}"))
}

/// Write `session`'s lines as two sealed shards (so `shard_count` is a real
/// number rather than 1 or 0).
fn write_two_shards(stage: &Path, machine: &str, session: &str, lines: &[String]) {
    let half = lines.len().div_ceil(2).max(1);
    let first: Vec<Vec<u8>> = lines[..half.min(lines.len())]
        .iter()
        .map(|l| l.as_bytes().to_vec())
        .collect();
    let second: Vec<Vec<u8>> = lines[half.min(lines.len())..]
        .iter()
        .map(|l| l.as_bytes().to_vec())
        .collect();
    for body in [&first, &second] {
        if body.is_empty() {
            continue;
        }
        store::write_sealed_shard_bytes_with_cap(
            StageWriter::Collect,
            stage,
            machine,
            session,
            body,
            store::DEFAULT_SHARD_BUCKET_CAP,
        )
        .unwrap();
    }
}

fn write_activity_index(stage: &Path, machine: &str, rows: &[ActivityRow]) {
    let meta_dir = stage.join("meta").join(machine);
    fs::create_dir_all(&meta_dir).unwrap();
    let mut body = String::new();
    for row in rows {
        body.push_str(&serde_json::to_string(row).unwrap());
        body.push('\n');
    }
    fs::write(meta_dir.join("activity-v1.jsonl"), body).unwrap();
}

fn index_row(machine: &str, session: &str, harness: &str, first: i64, last: i64) -> ActivityRow {
    ActivityRow {
        session_id: session.to_string(),
        machine: machine.to_string(),
        harness: harness.to_string(),
        first_unix: Some(first),
        last_unix: Some(last),
        line_count: 4,
        time_source: ActivityTimeSource::Exact,
    source_zone: None,
    }
}

/// Build the fixture repository below `root`, and return `(repo, key, mk)`.
///
/// `m-alpha` (claude-code, index present): `CC_ONE` runs across Jan 2–3,
/// `CC_TWO` on Jan 10. `m-beta` (codex, index removed): `CODEX` has no recorded
/// conversation time at all.
fn build_fixture_at(root: &Path) -> (PathBuf, PathBuf, MasterKey) {
    let root = root.to_path_buf();
    let repo = root.join("repo");
    let mk = MasterKey::new();
    let key = root.join("key.json");

    // ---- m-alpha --------------------------------------------------------
    let stage_a = stage_path(&root, "m-alpha");
    let (d2_10, t_one_a) = jan(2, 10);
    let (d2_11, _t_one_b) = jan(2, 11);
    let (d3_09, t_one_c) = jan(3, 9);
    let (d10_14, t_two_a) = jan(10, 14);
    let (d10_15, t_two_b) = jan(10, 15);
    let one = vec![
        user_line("first question", Some(&d2_10)),
        assistant_line("first answer", Some(&d2_11)),
        tool_result_line(&d2_11),
        user_line("second question", Some(&d3_09)),
        // No timestamp anywhere in this line: a trim must keep it and count it.
        user_line("undated question", None),
    ];
    let two = vec![
        user_line("another question", Some(&d10_14)),
        assistant_line("another answer", Some(&d10_15)),
        tool_result_line(&d10_15),
    ];
    write_two_shards(&stage_a, "m-alpha", CC_ONE, &one);
    write_two_shards(&stage_a, "m-alpha", CC_TWO, &two);
    write_activity_index(
        &stage_a,
        "m-alpha",
        &[
            index_row("m-alpha", CC_ONE, "claude-code", t_one_a, t_one_c),
            index_row("m-alpha", CC_TWO, "claude-code", t_two_a, t_two_b),
        ],
    );

    // ---- m-beta: index written, pushed, then removed and pushed again ----
    let stage_b = stage_path(&root, "m-beta");
    let (d20_10, t_codex_a) = jan(20, 10);
    let (d20_11, t_codex_b) = jan(20, 11);
    let codex_lines = vec![
        format!(r#"{{"type":"message","role":"user","text":"codex question","ts":"{d20_10}"}}"#),
        format!(r#"{{"type":"message","role":"assistant","text":"codex answer","ts":"{d20_11}"}}"#),
    ];
    write_two_shards(&stage_b, "m-beta", CODEX, &codex_lines);
    write_activity_index(
        &stage_b,
        "m-beta",
        &[index_row("m-beta", CODEX, "codex", t_codex_a, t_codex_b)],
    );

    store::persist_key_file(&cfg(&repo, &key), &mk).unwrap();
    for (machine, stage) in [("m-alpha", &stage_a), ("m-beta", &stage_b)] {
        let store = BackupStore::new(cfg(&repo, &key), machine.to_string());
        assert!(store.push(stage, &mk).unwrap().files_new > 0);
    }

    // Now remove m-beta's index and push again: the newest snapshot of that
    // machine has its sessions and no index, which is the state under test.
    fs::remove_dir_all(stage_b.join("meta")).unwrap();
    let store = BackupStore::new(cfg(&repo, &key), "m-beta".to_string());
    store.push(&stage_b, &mk).unwrap();
    assert!(
        !stage_b.join("meta").exists(),
        "the fixture's second machine must have no index in its newest snapshot"
    );

    (repo, key, mk)
}

/// [`build_fixture_at`] in a directory this test owns.
fn build_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, MasterKey) {
    let dir = tempfile::TempDir::new().unwrap();
    let (repo, key, mk) = build_fixture_at(dir.path());
    (dir, repo, key, mk)
}

fn store_of(repo: &Path, root: &Path, machine: &str) -> BackupStore {
    BackupStore::new(cfg(repo, &root.join("key.json")), machine.to_string())
}

fn selector(args: SelectorArgs) -> Selector {
    args.resolve().unwrap().selector
}

/// A raw unix-seconds window, built the way the deprecated `--since-unix` /
/// `--until-unix` flags build it.
fn window_args(since: i64, until: i64) -> SelectorArgs {
    SelectorArgs {
        since_unix: Some(since),
        until_unix: Some(until),
        ..Default::default()
    }
}

fn export_opts(out: &Path, turns: Turns, trim: bool) -> ExportOptions {
    ExportOptions {
        out: out.to_path_buf(),
        turns,
        trim_to_window: trim,
        force: false,
        dry_run: false,
    }
}

/// Relative paths of the files a run wrote, sorted, spelled the way the
/// archive spells them: `/`, on every OS.
fn written_paths(out: &Path) -> Vec<String> {
    let mut found = Vec::new();
    collect_files(out, out, &mut found);
    found.sort();
    found
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out);
        } else {
            out.push(portable_relative(&path, root).unwrap());
        }
    }
}

/// The path of `path` below `root`, spelled the way the archive spells a
/// relative path: `/`, on every OS.
///
/// `Path`'s own spelling — `Display`, `to_string_lossy` — is the separator of
/// the machine the test happens to run on. Asked for the relative path of
/// `<out>/m-alpha/claude-code/<id>.jsonl`, it answers
/// `m-alpha/claude-code/<id>.jsonl` here and `m-alpha\claude-code\<id>.jsonl`
/// under `windows-latest`, so an assertion written against the first spelling
/// is comparing the runner's OS to the archive. The product does not have that
/// problem: `export.rs` builds `relative_path` with a `format!` literal and
/// `/`, so `manifest.json` reads the same everywhere.
///
/// Joining `Path::components()` is the fix, not `str::replace('\\', "/")`:
/// `components` is the OS's own parse of the path, so a backslash that is part
/// of a real file *name* is left where it is instead of being promoted to a
/// separator.
fn portable_relative(path: &Path, root: &Path) -> Option<String> {
    Some(
        path.strip_prefix(root)
            .ok()?
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn manifest(out: &Path) -> serde_json::Value {
    let text = fs::read_to_string(out.join("manifest.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The set of `(machine, session id)` a report chose, as a comparable set.
fn search_set(report: &chat_stasher::search::SearchReport) -> BTreeSet<(String, String)> {
    report
        .hits
        .iter()
        .map(|h| (h.machine.clone(), h.session_id.clone()))
        .collect()
}

/// The set the manifest says was written, in the same shape.
fn written_set(out: &Path) -> BTreeSet<(String, String)> {
    manifest(out)["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["machine"].as_str().unwrap().to_string(),
                s["session_id"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

// ------------------------------------------------------ the path spelling rule

/// A relative path is spelled with `/`, whatever separator the `Path` it came
/// from is spelled with.
///
/// This is the rule the Windows job was failing on: the export writes
/// `<out>/<machine>/<harness>/<session>.jsonl`, and the file the walk finds
/// there has to carry that name on `windows-latest` too, not
/// `m-alpha\claude-code\<session>.jsonl`.
#[test]
fn a_relative_path_is_spelled_with_forward_slashes_on_every_os() {
    let root = Path::new("out");
    // Built the way the export builds its paths: with `join`.
    let path = root
        .join("m-alpha")
        .join("claude-code")
        .join("session.jsonl");
    assert_eq!(
        portable_relative(&path, root).as_deref(),
        Some("m-alpha/claude-code/session.jsonl")
    );
}

/// The same rule, for a path spelled the way Windows spells one.
///
/// Windows-only, and it has to be: on Unix `\` is an ordinary character in a
/// file name rather than a separator, so `out\m-alpha\session.jsonl` is a
/// *single* component there and the property this asserts does not exist on
/// that side. The test above runs everywhere and covers the composition, which
/// is all a Unix runner can see of this rule; `windows-latest` runs this one.
#[cfg(windows)]
#[test]
fn a_backslash_spelled_relative_path_is_still_written_with_forward_slashes() {
    let root = Path::new("out");
    let path = PathBuf::from(r"out\m-alpha\claude-code\session.jsonl");
    assert_eq!(
        portable_relative(&path, root).as_deref(),
        Some("m-alpha/claude-code/session.jsonl")
    );
}

// ------------------------------------------------------------------ test 1

/// The one property the whole feature rests on: `export` writes exactly the set
/// `search` selects, for the same flags — including a selection that is empty,
/// and one where a session could not be placed in time.
#[test]
fn export_writes_exactly_what_search_selects() {
    let (dir, repo, _key, mk) = build_fixture();
    let root = dir.path();
    let store = store_of(&repo, root, "m-alpha");

    let cases: Vec<(&str, SelectorArgs)> = vec![
        ("no filter", SelectorArgs::default()),
        (
            "machine=m-alpha",
            SelectorArgs {
                machine: Some("m-alpha".into()),
                ..Default::default()
            },
        ),
        (
            "harness=codex",
            SelectorArgs {
                harness: Some(vec!["codex".into()]),
                ..Default::default()
            },
        ),
        (
            "session=claude-code.m-alpha.aaaaaaaa-0000-0000-0000-000000000001",
            SelectorArgs {
                session: Some("claude-code.m-alpha.aaaaaaaa-0000-0000-0000-000000000001".into()),
                ..Default::default()
            },
        ),
        (
            "window over Jan 2 only",
            window_args(JAN_2024 + DAY, JAN_2024 + 2 * DAY),
        ),
        (
            "window over Jan 2-11",
            window_args(JAN_2024 + DAY, JAN_2024 + 11 * DAY),
        ),
        (
            "window over an empty stretch of 2025",
            window_args(JAN_2024 + 400 * DAY, JAN_2024 + 401 * DAY),
        ),
    ];

    let mut selected_counts = BTreeSet::new();
    for (name, args) in &cases {
        let selector = selector(args.clone());
        let report = search_sessions(&store, &mk, &selector).unwrap();
        let out = root.join(format!("export-{}", name.replace([' ', '=', '/'], "-")));

        let exported = export::export_sessions(
            &store,
            &mk,
            &selector,
            &export_opts(&out, Turns::All, false),
            &|_| {},
        )
        .unwrap();

        println!(
            "[W19] case `{name}`: matched={} unplaced={} written={} exit={}",
            report.hits.len(),
            report.unplaced.len(),
            exported.sessions.len(),
            exported.exit_status()
        );

        assert_eq!(
            written_set(&out),
            search_set(&report),
            "case `{name}`: the written set must be exactly the matched set"
        );
        assert_eq!(
            exported.selected,
            report.hits.len(),
            "case `{name}`: the same number was selected"
        );
        assert_eq!(
            exported.not_placed.len(),
            report.unplaced.len(),
            "case `{name}`: the sessions that could not be placed travel through"
        );
        assert_eq!(
            exported.exit_status(),
            if !report.answer_complete() {
                3
            } else if report.hits.is_empty() {
                1
            } else {
                0
            },
            "case `{name}`: export's exit code must mean what search's means"
        );
        selected_counts.insert(report.hits.len());
    }

    // The case list has to actually span the answer space, or "the sets agree"
    // is a statement about one situation.
    assert!(
        selected_counts.contains(&0),
        "one case must select nothing: {selected_counts:?}"
    );
    assert!(
        selected_counts.contains(&3),
        "one case must select everything: {selected_counts:?}"
    );
    assert!(
        selected_counts.contains(&2),
        "one case must select a subset"
    );

    // A dry run writes nothing at all — not the directory, not a manifest.
    let dry = root.join("export-dry");
    let report = search_sessions(&store, &mk, &Selector::default()).unwrap();
    // The plan callback is `Fn`, so the capture is a `Cell` rather than a `mut`
    // binding: the point is to observe what was announced, not to accumulate.
    let planned = std::cell::Cell::new(0usize);
    let exported = export::export_sessions(
        &store,
        &mk,
        &Selector::default(),
        &ExportOptions {
            out: dry.clone(),
            turns: Turns::All,
            trim_to_window: false,
            force: false,
            dry_run: true,
        },
        &|plan| planned.set(plan.sessions),
    )
    .unwrap();
    let planned = planned.get();
    assert_eq!(planned, report.hits.len(), "the plan counts the selection");
    assert_eq!(
        exported.selected, planned,
        "a dry run selects exactly what it planned"
    );
    assert!(
        !dry.exists(),
        "--dry-run must not create the output directory"
    );
    assert!(exported.sessions.is_empty());
    assert_eq!(exported.exit_status(), 0, "the real run would have written");
}

// ------------------------------------------------------------------ test 2

/// The bytes are the archive's own bytes: each written file equals what `read`
/// returns for that session, and the manifest's sha256 is the digest of the file
/// on disk (not of the source, which is the mistake this pins).
#[test]
fn written_files_are_byte_identical_to_read_and_the_manifest_sha_matches() {
    let (dir, repo, _key, mk) = build_fixture();
    let root = dir.path();
    let store = store_of(&repo, root, "m-alpha");
    let out = root.join("export-bytes");

    let exported = export::export_sessions(
        &store,
        &mk,
        &Selector::default(),
        &export_opts(&out, Turns::All, false),
        &|_| {},
    )
    .unwrap();
    assert_eq!(
        exported.sessions.len(),
        3,
        "the fixture holds three sessions"
    );
    assert_eq!(exported.exit_status(), 0);

    let m = manifest(&out);
    let sessions = m["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 3);
    let mut checked = 0usize;
    for entry in sessions {
        let machine = entry["machine"].as_str().unwrap();
        let session = entry["session_id"].as_str().unwrap();
        let relative = entry["relative_path"].as_str().unwrap();
        // A portable archive: the manifest is read on machines other than the
        // one that wrote it, so this path is spelled for the archive and not
        // for the writer. `is_safe_component` already refuses `\` inside any
        // component, so this can only fail by the path being built from a
        // `Path` instead of the `format!` in `export.rs`.
        assert!(
            !relative.contains('\\'),
            "the manifest's relative_path must be spelled for a portable archive, not for the \
             machine that wrote it: `{relative}`"
        );
        let file = out.join(relative);
        let on_disk = fs::read(&file).unwrap();

        // `read`'s own code path, from that machine's stage.
        let stage = stage_path(root, machine);
        let (from_read, shards) = store_of(&repo, root, machine)
            .read_session_readback(&stage, session, &mk)
            .unwrap();
        assert_eq!(
            on_disk,
            from_read,
            "session {} (machine {machine}) is not byte-identical to `read`",
            &session[..8]
        );

        let digest = Sha256::digest(&on_disk)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert_eq!(
            entry["sha256"].as_str().unwrap(),
            digest,
            "the manifest's sha256 must be the digest of the file on disk"
        );
        assert_eq!(
            entry["bytes_written"].as_u64().unwrap(),
            on_disk.len() as u64
        );
        assert_eq!(
            entry["shard_count"].as_u64().unwrap() as usize,
            shards.len(),
            "the recorded shard count is the number of shards read"
        );
        assert_eq!(
            on_disk.len() as u64,
            exported
                .sessions
                .iter()
                .find(|s| s.session_id == session)
                .unwrap()
                .bytes_written
        );
        checked += 1;
    }
    assert_eq!(checked, 3);

    // Two machines, two harnesses, and the path carries both.
    let paths = written_paths(&out);
    assert!(paths.contains(&format!("m-alpha/claude-code/{CC_ONE}.jsonl")));
    assert!(paths.contains(&format!("m-beta/codex/{CODEX}.jsonl")));
    assert!(paths.contains(&"manifest.json".to_string()));

    // The unknown conversation time of the index-less machine is a tagged
    // unknown in the manifest, never a null and never a zero.
    let beta = sessions
        .iter()
        .find(|s| s["machine"] == "m-beta")
        .expect("the index-less machine's session is in the manifest");
    assert_eq!(beta["first_message"]["kind"], "unknown");
    assert!(beta["first_message"]["why"]
        .as_str()
        .unwrap()
        .contains("no activity index"));
    assert!(m["machines_without_activity_index"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "m-beta"));
}

// ------------------------------------------------------------------ test 3

/// `--turns user`: applied where the format is certain, explicitly
/// `not-supported` — with every line written — where it is not.
#[test]
fn turns_user_filters_claude_code_and_is_not_supported_for_codex() {
    let (dir, repo, _key, mk) = build_fixture();
    let root = dir.path();
    let store = store_of(&repo, root, "m-alpha");
    let out = root.join("export-turns");

    export::export_sessions(
        &store,
        &mk,
        &Selector::default(),
        &export_opts(&out, Turns::User, false),
        &|_| {},
    )
    .unwrap();

    let m = manifest(&out);
    let entry = |session: &str| {
        m["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["session_id"] == session)
            .unwrap()
            .clone()
    };

    // claude-code: only the user's own messages, tool results and assistant
    // turns gone.
    let one = entry(CC_ONE);
    assert_eq!(one["turns_filter"], "applied");
    let body = fs::read_to_string(
        out.join("m-alpha/claude-code/")
            .join(format!("{CC_ONE}.jsonl")),
    )
    .unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 3, "three user lines in the fixture session");
    assert!(lines.iter().all(|l| l.contains(r#""type":"user""#)));
    assert!(
        !body.contains("tool_result"),
        "a tool result routed through the user role is not the user's message"
    );
    assert!(!body.contains(r#""type":"assistant""#));
    assert_eq!(one["lines_total"], 5);
    assert_eq!(one["lines_written"], 3);

    // codex: the format does not make the question answerable, so nothing is
    // dropped and the session says so.
    let codex = entry(CODEX);
    assert_eq!(codex["turns_filter"], "not-supported");
    assert_eq!(
        codex["lines_written"], codex["lines_total"],
        "an unsupported harness must write every line"
    );
    let codex_body =
        fs::read_to_string(out.join("m-beta/codex/").join(format!("{CODEX}.jsonl"))).unwrap();
    assert_eq!(
        codex_body.lines().count() as u64,
        codex["lines_total"].as_u64().unwrap()
    );

    // A run with `--turns all` records that no filter was asked for.
    let plain = root.join("export-turns-off");
    export::export_sessions(
        &store,
        &mk,
        &Selector::default(),
        &export_opts(&plain, Turns::All, false),
        &|_| {},
    )
    .unwrap();
    assert!(manifest(&plain)["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["turns_filter"] == "not-requested"));
}

// ------------------------------------------------------------------ test 4

/// `--trim-to-window` drops the lines outside the window, keeps the one whose
/// time cannot be read, and counts it.
#[test]
fn trim_to_window_keeps_untimed_lines_and_counts_them() {
    let (dir, repo, _key, mk) = build_fixture();
    let root = dir.path();
    let store = store_of(&repo, root, "m-alpha");
    let out = root.join("export-trim");

    // Jan 2 only: CC_ONE's first two lines are inside, its Jan 3 line and its
    // undated line are what the trim has to decide about. The machine filter
    // keeps the index-less machine out of the query entirely — otherwise its
    // unplaceable session would (correctly) make this answer a 3, and this test
    // is about the trim, not about that.
    let args = SelectorArgs {
        machine: Some("m-alpha".into()),
        ..window_args(JAN_2024 + DAY, JAN_2024 + DAY + (DAY - 1))
    };
    let exported = export::export_sessions(
        &store,
        &mk,
        &selector(args),
        &export_opts(&out, Turns::All, true),
        &|_| {},
    )
    .unwrap();
    assert_eq!(exported.exit_status(), 0);
    assert_eq!(exported.sessions.len(), 1, "only CC_ONE spans Jan 2");

    let m = manifest(&out);
    let entry = &m["sessions"][0];
    assert_eq!(entry["session_id"], CC_ONE);
    assert_eq!(entry["trimmed_to_window"], true);
    assert_eq!(entry["lines_total"], 5);
    assert_eq!(
        entry["lines_written"], 4,
        "one line is outside the window; the undated line stays"
    );
    assert_eq!(
        entry["untimed_lines"], 1,
        "the line with no readable timestamp is kept AND counted"
    );

    let body = fs::read_to_string(
        out.join("m-alpha/claude-code/")
            .join(format!("{CC_ONE}.jsonl")),
    )
    .unwrap();
    assert!(body.contains("first question"));
    assert!(body.contains("undated question"), "an untimed line is kept");
    assert!(
        !body.contains("second question"),
        "the Jan 3 line is outside a Jan 2 window"
    );

    // A trim with no window to trim to is a usage error, not a silent no-op.
    let err = export::export_sessions(
        &store,
        &mk,
        &Selector::default(),
        &export_opts(&root.join("export-trim-nowhere"), Turns::All, true),
        &|_| {},
    )
    .expect_err("--trim-to-window without a window must be refused");
    assert_eq!(export::exit_status_for_error(&err), 2);
    assert!(format!("{err}").contains("--trim-to-window"), "{err}");
}

// ------------------------------------------------------------------ test 4b

/// The same equality, one level up: the files `export` leaves on disk are the
/// sessions `search --json` reports as matched, for the same flags. This is the
/// promise the issue makes in the user's own terms — `search` is the dry run of
/// `export` — and it is checked through the CLI, not through the library, so a
/// caller who only reads the JSON is covered too.
#[test]
fn cli_export_writes_the_same_set_search_json_reports() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let root = sandbox.path();
    let (repo, key, _mk) = build_fixture_at(root);

    let cases: Vec<(&str, Vec<String>)> = vec![
        ("everything", vec![]),
        ("one machine", vec!["--machine".into(), "m-alpha".into()]),
        ("one harness", vec!["--harness".into(), "codex".into()]),
        (
            "a window that selects nothing",
            vec![
                "--machine".into(),
                "m-alpha".into(),
                "--since-unix".into(),
                (JAN_2024 + 400 * DAY).to_string(),
                "--until-unix".into(),
                (JAN_2024 + 401 * DAY).to_string(),
            ],
        ),
    ];

    for (index, (name, flags)) in cases.iter().enumerate() {
        let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
        let search = run_cli(
            root,
            &repo,
            &key,
            "search",
            &[&["--json"], flags.as_slice()].concat(),
        );
        assert!(
            search.status.success() || search.status.code() == Some(1),
            "case `{name}`: search --json exited {:?}\n{}",
            search.status.code(),
            String::from_utf8_lossy(&search.stderr)
        );
        let json: serde_json::Value = serde_json::from_slice(&search.stdout).unwrap_or_else(|e| {
            panic!(
                "case `{name}`: search --json did not print one JSON object: {e}\n{}",
                String::from_utf8_lossy(&search.stdout)
            )
        });
        let matched: BTreeSet<(String, String)> = json["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["machine"].as_str().unwrap().to_string(),
                    s["session_short_id"].as_str().unwrap().to_string(),
                )
            })
            .collect();

        let out = root.join(format!("cli-export-{index}"));
        let export = run_cli(
            root,
            &repo,
            &key,
            "export",
            &[&["--out", out.to_str().unwrap()], flags.as_slice()].concat(),
        );
        assert_ne!(
            export.status.code(),
            Some(2),
            "case `{name}`: export rejected flags search accepted\n{}",
            String::from_utf8_lossy(&export.stderr)
        );

        // The manifest carries full ids; `search --json` prints the privacy-safe
        // short form, which is the same function applied to the same id.
        let written: BTreeSet<(String, String)> = manifest(&out)["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["machine"].as_str().unwrap().to_string(),
                    chat_stasher::id::short_session_id(s["session_id"].as_str().unwrap()),
                )
            })
            .collect();
        println!(
            "[W19] cli case `{name}`: search matched={} export wrote={} exit={:?}",
            matched.len(),
            written.len(),
            export.status.code()
        );
        assert_eq!(
            written, matched,
            "case `{name}`: the CLI's written set must equal `search --json`'s matched set"
        );
    }
}

// ------------------------------------------------------------------ test 5

/// A non-empty `--out` is refused unless `--force`, nothing is ever deleted,
/// and no byte lands outside `--out`.
#[test]
fn non_empty_out_is_refused_and_nothing_lands_outside_it() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let root = sandbox.path();
    let (repo, key, _mk) = build_fixture_at(root);
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();

    // ---- refused ---------------------------------------------------------
    let occupied = work.join("occupied");
    fs::create_dir_all(&occupied).unwrap();
    fs::write(occupied.join("existing.txt"), b"keep me").unwrap();
    let before = tree(root);
    let output = run_export(root, &repo, &key, &occupied, &[]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "a non-empty --out is a usage error\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(occupied.join("existing.txt").exists(), "nothing is deleted");
    assert!(!occupied.join("manifest.json").exists());
    let after = tree(root);
    assert_eq!(before, after, "a refused run writes nothing anywhere");

    // ---- accepted with --force, and still nothing deleted -----------------
    let output = run_export(root, &repo, &key, &occupied, &["--force"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        occupied.join("existing.txt").exists(),
        "--force deletes nothing"
    );
    assert!(occupied.join("manifest.json").exists());

    // ---- everything new is below --out ------------------------------------
    let out = work.join("nested/deeper/exported");
    let before = tree(root);
    let output = run_export(root, &repo, &key, &out, &[]);
    assert_eq!(output.status.code(), Some(0));
    let after = tree(root);
    let new: Vec<&String> = after.difference(&before).collect();
    assert!(!new.is_empty(), "the run must have written something");
    for path in new {
        assert!(
            Path::new(path).starts_with(&out),
            "`{path}` is outside --out ({})",
            out.display()
        );
    }
    assert_eq!(
        written_paths(&out),
        vec![
            format!("m-alpha/claude-code/{CC_ONE}.jsonl"),
            format!("m-alpha/claude-code/{CC_TWO}.jsonl"),
            format!("m-beta/codex/{CODEX}.jsonl"),
            "manifest.json".to_string(),
        ]
    );
}

/// Every file below `root`, as an absolute path string.
fn tree(root: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    collect_tree(root, &mut out);
    out
}

fn collect_tree(dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            collect_tree(&path, out);
        } else {
            out.insert(path.to_string_lossy().into_owned());
        }
    }
}

// ------------------------------------------------------------------ test 6

/// The four exit codes, each reached through the real CLI on the same fixture:
/// `0` wrote something · `1` read everything and selected nothing · `3` could
/// not finish (no key, or a session that cannot be placed) · `2` usage.
#[test]
fn exit_codes_are_zero_one_three_and_two() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let root = sandbox.path();
    let (repo, key, _mk) = build_fixture_at(root);

    // 0 — the whole archive, nothing filtered.
    let ok = root.join("out-zero");
    assert_eq!(
        run_export(root, &repo, &key, &ok, &[]).status.code(),
        Some(0)
    );

    // 1 — a window that matches nothing, with every session placeable except
    // the index-less machine's. That exception is what makes it a 3, so this
    // case narrows the selection with a machine filter the archive can answer.
    let empty = root.join("out-one");
    let args = [
        "--machine",
        "m-alpha",
        "--since-unix",
        &(JAN_2024 + 400 * DAY).to_string(),
        "--until-unix",
        &(JAN_2024 + 401 * DAY).to_string(),
    ];
    let output = run_export(root, &repo, &key, &empty, &args);
    assert_eq!(
        output.status.code(),
        Some(1),
        "read it all, selected nothing\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        empty.join("manifest.json").exists(),
        "the manifest is still written"
    );
    assert_eq!(manifest(&empty)["exit_status"], 1);

    // 3a — nothing selected, and a session that could not be placed: "nothing"
    // is unproven, so the same window over the whole archive is a 3.
    let unproven = root.join("out-three-unplaced");
    let output = run_export(root, &repo, &key, &unproven, &args[2..]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // 3b — no key at all: the archive was never read.
    let missing_key = root.join("no-such-key.json");
    let no_key_out = root.join("out-no-key");
    let output = run_export(root, &repo, &missing_key, &no_key_out, &[]);
    assert_eq!(output.status.code(), Some(3));
    assert!(
        !no_key_out.exists(),
        "a run that could not read its archive must not leave an empty directory that looks like an output"
    );

    // 2 — a usage error: a directory that is not a directory.
    let file_out = root.join("out-is-a-file");
    fs::write(&file_out, b"x").unwrap();
    let output = run_export(root, &repo, &key, &file_out, &[]);
    assert_eq!(output.status.code(), Some(2));

    // 2 — and a malformed date, before anything is read.
    let output = run_export(
        root,
        &repo,
        &key,
        &root.join("out-bad-date"),
        &["--day", "yesterday"],
    );
    assert_eq!(output.status.code(), Some(2));
}

// ------------------------------------------------------------------ test 7

/// A symlink below `--out` is not a destination. With `<out>/<machine>` pointing
/// at a directory outside `--out`, `create_dir_all` and `fs::write` would follow
/// it and put archived conversation content outside the directory the user
/// named — so the session is refused and recorded, and the run is a `3`.
///
/// Unix-only, and the platform is the subject rather than an accommodation: the
/// symlink is what this test is about, and unix is where the fixture can create
/// one.
#[cfg(unix)]
#[test]
fn a_symlinked_machine_directory_cannot_be_written_through() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let root = sandbox.path();
    let (repo, key, _mk) = build_fixture_at(root);

    let outside = root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let out = root.join("out-symlink-dir");
    fs::create_dir_all(&out).unwrap();
    std::os::unix::fs::symlink(&outside, out.join("m-alpha")).unwrap();

    let output = run_export(root, &repo, &key, &out, &["--force"]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "a session that could not be written leaves the run unfinished\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Nothing was created through the link — the refusal is the point.
    assert_eq!(
        fs::read_dir(&outside).unwrap().count(),
        0,
        "no byte of the export may land outside --out"
    );
    assert!(
        out.join("m-alpha").is_symlink(),
        "the refusal deletes nothing: the symlink is still there"
    );

    // The refusal is recorded per session, with a reason, in the manifest — the
    // absent files are not left to be misread as "there was nothing".
    let m = manifest(&out);
    let failed = m["sessions_failed"].as_array().unwrap();
    assert_eq!(failed.len(), 2, "both claude-code sessions are refused");
    for entry in failed {
        let why = entry["why"].as_str().unwrap();
        assert!(
            why.contains("symlink"),
            "the manifest must name the reason: {why}"
        );
        assert_eq!(entry["machine"], "m-alpha");
    }
    assert_eq!(m["exit_status"], 3);

    // The machine that was not symlinked is still written: the rest of the
    // export is still worth having.
    assert!(
        out.join("m-beta/codex")
            .join(format!("{CODEX}.jsonl"))
            .exists(),
        "the sessions below a sound path are still exported"
    );
    assert_eq!(m["written"], 1);
}

/// The manifest is a write below `--out` like any other, so a symlink planted at
/// its own path is refused as well. There is no manifest left to record that in,
/// so the run ends the way an unwritable manifest already ends it: exit `1`,
/// "read everything, then failed to write" — never a `0` with the manifest
/// somewhere the user did not name.
#[cfg(unix)]
#[test]
fn a_symlinked_manifest_is_not_written_through() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let root = sandbox.path();
    let (repo, key, _mk) = build_fixture_at(root);

    let victim = root.join("victim.json");
    let before_bytes = b"{\"not\":\"a manifest\"}".to_vec();
    fs::write(&victim, &before_bytes).unwrap();

    let out = root.join("out-symlink-manifest");
    fs::create_dir_all(&out).unwrap();
    std::os::unix::fs::symlink(&victim, out.join("manifest.json")).unwrap();

    let output = run_export(root, &repo, &key, &out, &["--force"]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "the archive was read and the write then failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(&victim).unwrap(),
        before_bytes,
        "the file the manifest link points at must be untouched"
    );
    assert!(
        out.join("manifest.json").is_symlink(),
        "the refusal deletes nothing: the link is still there"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("symlink"),
        "the refusal must say why: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The same rule one level down: the target `.jsonl` itself is a symlink to a
/// file outside `--out`. The file it points at must be exactly as it was —
/// `fs::write` would have followed the link and overwritten it.
#[cfg(unix)]
#[test]
fn a_symlinked_target_file_is_not_overwritten() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let root = sandbox.path();
    let (repo, key, _mk) = build_fixture_at(root);

    let victim = root.join("victim.jsonl");
    let before_bytes = b"this is not an export of anything".to_vec();
    fs::write(&victim, &before_bytes).unwrap();

    let out = root.join("out-symlink-file");
    let target = out
        .join("m-alpha/claude-code")
        .join(format!("{CC_ONE}.jsonl"));
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&victim, &target).unwrap();

    let output = run_export(root, &repo, &key, &out, &["--force"]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "the session whose target is a link could not be written\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        fs::read(&victim).unwrap(),
        before_bytes,
        "the file the link points at must be untouched"
    );
    assert!(
        target.is_symlink(),
        "the refusal deletes nothing: the link is still there"
    );

    let m = manifest(&out);
    let failed = m["sessions_failed"].as_array().unwrap();
    assert_eq!(failed.len(), 1, "only the linked session is refused");
    assert_eq!(failed[0]["session_id"], CC_ONE);
    assert!(
        failed[0]["why"].as_str().unwrap().contains("symlink"),
        "the manifest must name the reason: {}",
        failed[0]["why"]
    );
    // The two sessions under sound paths are still written.
    assert_eq!(m["written"], 2);
    assert!(out
        .join("m-beta/codex")
        .join(format!("{CODEX}.jsonl"))
        .exists());
}

/// `--force` over a previous export of the same selection: the files are the
/// ones this run wrote, the bytes are the same, and nothing about the second
/// run's own output (an existing `manifest.json`, existing directories) makes it
/// fail. A guard against a symlink check that refuses ordinary re-runs.
#[test]
fn a_force_rerun_over_a_previous_export_succeeds() {
    let sandbox = tempfile::TempDir::new().unwrap();
    let root = sandbox.path();
    let (repo, key, _mk) = build_fixture_at(root);
    let out = root.join("out-rerun");

    let first = run_export(root, &repo, &key, &out, &[]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let paths = written_paths(&out);
    let before: Vec<(String, Vec<u8>)> = paths
        .iter()
        .map(|p| (p.clone(), fs::read(out.join(p)).unwrap()))
        .collect();

    let second = run_export(root, &repo, &key, &out, &["--force"]);
    assert_eq!(
        second.status.code(),
        Some(0),
        "a --force re-run over a previous export must succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        written_paths(&out),
        paths,
        "the re-run writes the same set at the same paths"
    );
    for (path, bytes) in &before {
        assert_eq!(
            &fs::read(out.join(path)).unwrap(),
            bytes,
            "`{path}` is the same export as before"
        );
    }
    let m = manifest(&out);
    assert_eq!(m["exit_status"], 0);
    assert_eq!(m["written"], 3);
    assert!(m["sessions_failed"].as_array().unwrap().is_empty());
}

/// Run the CLI with an isolated HOME and XDG tree — the same isolation
/// `b73_readexit_test.rs` uses, so no real config, cache or stage is read.
fn run_export(sandbox: &Path, repo: &Path, key: &Path, out: &Path, extra: &[&str]) -> Output {
    run_cli(
        sandbox,
        repo,
        key,
        "export",
        &[&["--out", out.to_str().unwrap()], extra].concat(),
    )
}

/// `--keep-ssh-masters` is deliberately **not** passed: `reap_remote` prints
/// `[reap] skipped (--keep-ssh-masters)` on stdout when it is, and this fixture
/// is a local repository with no `endpoint` option, so the reap path returns
/// silently either way. Passing the flag would only add a line after the JSON
/// `search --json` promises is alone on stdout.
fn run_cli(sandbox: &Path, repo: &Path, key: &Path, subcommand: &str, extra: &[&str]) -> Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .arg(subcommand)
        .arg("--repo")
        .arg(repo)
        .arg("--key-file")
        .arg(key)
        .args(extra)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("cache"))
        .env_remove("CODEX_HOME")
        .env_remove("RUSTIC_REPO")
        .env_remove("RUSTIC_KEY_FILE");
    command.output().unwrap()
}
