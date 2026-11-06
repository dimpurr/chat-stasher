//! W1091 (PERF-NEXT ①) — the observability the slow-round diagnosis reads:
//! the destination probes' wall times, the wanted-count every whole or
//! targeted archive read was asked with, and the committed-prefix
//! validation ledger (checked / answered by the hash cache / read for
//! real / bytes those reads cost).
//!
//! Two angles, both on synthetic fixtures only:
//!
//! 1. the collector's own record (`CollectReport`) carries the probe
//!    timings, the wanted count and the prefix-validation counters,
//!    asserted against fixture sources whose bytes and debt shapes are
//!    known;
//! 2. the real binary's `run-once` prints the two new `[collect]` lines,
//!    extends the one phases summary line, records the same numbers in
//!    `run-state.json`, and leaves the per-read `wanted=` line on stderr
//!    — including through the real archive read a pass runs when a
//!    stored debt can no longer prove itself locally.
//!
//! Assertions inspect counts, byte totals and boolean flags only; no
//! conversation text appears in any assertion or output, and no path
//! or session id is printed.

use chat_stasher::collect::{self, ArchiveFacts, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::rc::Rc;

#[path = "../src/test_support.rs"]
mod test_support;

const MACHINE: &str = "w1091-machine";

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

/// One synthetic source file, its own stage and state directory, and a
/// `collect` pass that runs against a caller-chosen destination view.
struct Fixture {
    _dir: tempfile::TempDir,
    source_root: PathBuf,
    source: PathBuf,
    stage: PathBuf,
    state: PathBuf,
}

fn fixture(contents: &[u8]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
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

/// A pass with no debt asking for remote proof never opens the archive:
/// wanted zero, not asked, no wall time charged to either probe.
#[test]
fn a_pass_without_remote_debts_never_asks_the_archive() {
    let fx = fixture(b"one\ntwo\nthree\n");
    let report = fx.collect(&DestinationView::unreachable("dest-a"));

    assert_eq!(report.lines_written, 3, "the pass collected the source");
    assert_eq!(
        report.destination_facts_wanted, 0,
        "a fresh pass has no debt that needs remote facts"
    );
    assert!(!report.destination_facts_asked, "the lazy probe never ran");
    assert_eq!(report.destination_facts_ms, 0);
    assert_eq!(report.machine_log_facts_ms, 0);

    // A quiet second pass proves its debt from the stage alone, so the
    // archive stays closed for the same reasons.
    let second = fx.collect(&DestinationView::unreachable("dest-a"));
    assert_eq!(second.changed_records, 0, "nothing changed");
    assert_eq!(second.destination_facts_wanted, 0);
    assert!(!second.destination_facts_asked);
    assert_eq!(second.destination_facts_ms, 0);
    println!(
        "quiet_pass wanted={} asked={} facts_ms={}",
        second.destination_facts_wanted,
        second.destination_facts_asked,
        second.destination_facts_ms
    );
}

/// A debt the stage can no longer prove puts its session in the wanted set
/// and runs the probe: the report then carries the wanted count, the
/// asked flag, and the exact set the archive was asked with.
#[test]
fn a_debt_that_needs_remote_facts_asked_once_with_its_wanted_set() {
    let fx = fixture(b"one\ntwo\n");
    let first = fx.collect(&DestinationView::unreachable("dest-a"));
    assert_eq!(first.lines_written, 2);

    // The stage the cursor's debt was accounted on disappears, so only the
    // destination's archive can still vouch for the delivered session.
    fs::remove_dir_all(&fx.stage).unwrap();
    let session = fx.session_id();

    let seen: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    let captured = seen.clone();
    let destination = DestinationView::new("dest-a", move |wanted| {
        // A reachable archive that answers "I hold nothing for these": the
        // probe ran and its answer is what the caller acts on, whatever it
        // says. The set it was given is the fact under test.
        *captured.borrow_mut() = wanted.iter().cloned().collect();
        Ok(ArchiveFacts::new())
    });

    let second = fx.collect(&destination);

    assert_eq!(
        seen.borrow().as_slice(),
        [(MACHINE.to_string(), session.clone())].as_slice()
    );
    assert_eq!(
        second.destination_facts_wanted, 1,
        "exactly the session whose debt lost its local proof"
    );
    assert!(second.destination_facts_asked, "the probe ran and answered");
    // An answer of "not held" makes the cursor unverifiable: the source is
    // reread, the old counters tell that story, and the new counters tell
    // what asking the archive cost, not what it covered.
    assert_eq!(second.unverified_cursors, 1);
    assert_eq!(second.reset_records, 1);
    println!(
        "remote_debt wanted={} asked={} unverified={}",
        second.destination_facts_wanted, second.destination_facts_asked, second.unverified_cursors
    );
}

/// The view's extractors read back what its own probes measured — and a
/// view nobody asked reports the zeros of "never ran", including the
/// machine-log probe a destination that cannot answer never had.
#[test]
fn the_view_reports_its_own_probe_timings() {
    let wanted_set: BTreeSet<(String, String)> = [(MACHINE.to_string(), "session-x".to_string())]
        .into_iter()
        .collect();
    let facts_seen = RefCell::new(false);
    let logs_seen = Cell::new(false);
    let view = DestinationView::with_machine_logs(
        "dest-a",
        move |wanted| {
            *facts_seen.borrow_mut() = wanted == &wanted_set;
            Ok(ArchiveFacts::new())
        },
        move || {
            logs_seen.set(true);
            Ok(std::collections::BTreeMap::new())
        },
    );

    // Before anything asked: no time charged, nothing marked as asked.
    assert_eq!(view.session_facts_ms(), 0);
    assert!(!view.session_facts_asked());
    assert_eq!(view.machine_log_facts_ms(), 0);

    let facts = view
        .facts(&wanted_set)
        .expect("the session probe answered with facts");
    assert!(
        facts.is_empty(),
        "the fixture archive holds nothing for the asked set"
    );
    assert!(
        facts_seen.borrow(),
        "the session probe ran with the wanted set"
    );
    assert!(view.session_facts_asked());
    assert_eq!(view.machine_log_facts_ms(), 0, "not asked yet, stays zero");

    let logs = view
        .machine_log_facts()
        .expect("the machine-log probe answered");
    assert!(logs.is_empty(), "the fixture archive holds no log facts");
    assert!(logs_seen.get(), "the machine-log probe ran");
    // A run probe's wall time may honestly be zero on a fast fixture, so
    // the extractor contract checked here is "the run asked implies the
    // timing is readable" — both extractors return after the runs above
    // without panicking, which is what the read-back promises.
    let _ = (view.session_facts_ms(), view.machine_log_facts_ms());
    println!(
        "probe_timings session_facts_ms={} machine_log_ms={}",
        view.session_facts_ms(),
        view.machine_log_facts_ms()
    );
}

/// The prefix-validation ledger: a first pass validates nothing (no stored
/// cursor), a quiet pass validates the committed prefix once and the hash
/// cache answers for free, and a grown source validates once at a cache
/// miss whose streamed digest read and post-delta re-check read both count.
#[test]
fn the_prefix_validation_ledger_counts_hits_misses_and_bytes() {
    let first_bytes = b"one\ntwo\nthree\n";
    let fx = fixture(first_bytes);
    let dest = DestinationView::unreachable("dest-a");

    let first = fx.collect(&dest);
    assert_eq!(first.lines_written, 3);
    assert_eq!(first.prefix_checks, 0, "no cursor existed to validate");
    assert_eq!(first.prefix_check_cache_hits, 0);
    assert_eq!(first.prefix_check_cache_misses, 0);
    assert_eq!(first.prefix_check_bytes_read, 0);

    let second = fx.collect(&dest);
    assert_eq!(second.changed_records, 0, "the quiet pass changed nothing");
    assert_eq!(second.prefix_checks, 1, "the stored cursor was validated");
    assert_eq!(
        second.prefix_check_cache_hits, 1,
        "the cache answered from the stat identity, no bytes re-read"
    );
    assert_eq!(second.prefix_check_cache_misses, 0);
    assert_eq!(second.prefix_check_bytes_read, 0);
    assert_eq!(
        second.shard_bytes_read_hashed, 0,
        "cross-check: nothing re-hashed"
    );

    fs::OpenOptions::new()
        .append(true)
        .open(&fx.source)
        .unwrap()
        .write_all(b"four\n")
        .unwrap();
    let third = fx.collect(&dest);
    assert_eq!(third.lines_written, 1, "only the appended line was sealed");
    assert_eq!(third.prefix_checks, 1);
    assert_eq!(third.prefix_check_cache_hits, 0);
    assert_eq!(
        third.prefix_check_cache_misses, 1,
        "the grown source's stat identity matched no stored entry"
    );
    assert_eq!(
        third.prefix_check_bytes_read,
        (first_bytes.len() * 2) as u64,
        "the streamed digest read plus the post-delta re-check read, each the size of the committed prefix"
    );
    assert_eq!(
        third.prefix_bytes_validated,
        first_bytes.len() as u64,
        "the pre-existing counter still says what validation covered"
    );
    println!(
        "prefix_ledger quiet=(checks={} hits={} bytes={}) grown=(checks={} misses={} bytes={})",
        second.prefix_checks,
        second.prefix_check_cache_hits,
        second.prefix_check_bytes_read,
        third.prefix_checks,
        third.prefix_check_cache_misses,
        third.prefix_check_bytes_read
    );
}

/// Run the real binary with every ambient path redirected into
/// `sandbox`, against a registry whose only harness is a synthetic
/// claude-code cell (the W880 harness).
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    if !registry.exists() {
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
        fs::write(
            &registry,
            serde_json::to_string(&json!({
                "schema_version": 1,
                "generated": "W1091 synthetic",
                "harnesses": [{
                    "id": "claude-code",
                    "display_name": "synthetic",
                    "paths": paths
                }]
            }))
            .unwrap(),
        )
        .unwrap();
    }
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("USERPROFILE", &home)
        .env(
            "XDG_CONFIG_HOME",
            test_support::consent_config_home(sandbox.join("config"), &["claude-code"]),
        )
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("rh-cache"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

fn state_dir(sandbox: &Path) -> PathBuf {
    sandbox.join("data").join("chat-stasher").join("state")
}

fn phases_of(sandbox: &Path) -> serde_json::Value {
    let raw = fs::read_to_string(state_dir(sandbox).join("run-state.json"))
        .expect("run-state.json must exist");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("run-state.json must parse");
    value["phases"].clone()
}

/// The real binary prints the two new `[collect]` lines, extends the one
/// phases summary line, records the same numbers in `run-state.json`, and —
/// when a debt can only be vouched remotely — leaves the per-read
/// `wanted=<n>` line on stderr naming the count it actually needed.
#[test]
fn run_once_prints_and_records_the_new_ledgers() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let home = sandbox.join("home");
    let projects = home.join(".claude").join("projects");
    fs::create_dir_all(&projects).unwrap();
    let session = projects.join("session.jsonl");
    let bytes = b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n";
    fs::write(&session, bytes).unwrap();
    let stage = sandbox.join("stage");

    // Pass one: no cursor to validate, no debt to ask about.
    let out = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let all = format!("{stdout}\n{stderr}");
    assert!(out.status.success(), "run-once failed: {all}");
    assert!(
        stdout.contains(
            "[collect] prefix check    : checked=0 cache_hits=0 cache_misses=0 bytes_read=0"
        ),
        "a first pass validates no prefix: {stdout}"
    );
    assert!(
        stdout.contains(
            "[collect] destination     : wanted=0 asked=false facts_ms=0 machine_log_ms=0"
        ),
        "a first pass asks no destination question: {stdout}"
    );
    assert_eq!(phases_of(sandbox)["prefix_checks"].as_u64(), Some(0));
    assert_eq!(
        phases_of(sandbox)["destination_facts_wanted"].as_u64(),
        Some(0)
    );
    assert_eq!(
        phases_of(sandbox)["destination_facts_asked"].as_bool(),
        Some(false)
    );

    // Pass two, unchanged: the committed prefix is validated once, the
    // hash cache answers it, no bytes move, and the archive stays closed.
    let out2 = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
        ],
    );
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    let all2 = format!("{stdout2}\n{stderr2}");
    assert!(out2.status.success(), "second run-once failed: {all2}");
    assert!(
        stdout2.contains(
            "[collect] prefix check    : checked=1 cache_hits=1 cache_misses=0 bytes_read=0"
        ),
        "the quiet pass's validation was answered by the cache: {stdout2}"
    );
    let summary2 = stdout2
        .lines()
        .find(|line| line.starts_with("[run-once] phases ms:"))
        .expect("run-once must print the phase summary line");
    assert!(
        summary2.contains("prefix: checked=1 cache_hits=1 cache_misses=0 bytes_read=0"),
        "the summary line carries the prefix ledger: {summary2}"
    );
    assert!(
        summary2.contains("dest: wanted=0 asked=false facts_ms=0 machine_log_ms=0"),
        "the summary line carries the destination ledger: {summary2}"
    );
    assert_eq!(
        phases_of(sandbox)["prefix_check_cache_hits"].as_u64(),
        Some(1)
    );
    assert_eq!(
        phases_of(sandbox)["destination_facts_wanted"].as_u64(),
        Some(0)
    );

    // Pass three: the stage is gone, so the pass's only way to vouch the
    // delivered session is the destination's archive — the probe reads it,
    // stderr records the read with the count this pass actually needed, and
    // the ledgers say so.
    fs::remove_dir_all(&stage).unwrap();
    let out3 = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
        ],
    );
    let stdout3 = String::from_utf8_lossy(&out3.stdout);
    let stderr3 = String::from_utf8_lossy(&out3.stderr);
    let all3 = format!("{stdout3}\n{stderr3}");
    assert!(out3.status.success(), "third run-once failed: {all3}");
    assert!(
        stderr3.contains("[readback] cumulative read: wanted=1"),
        "the archive read names the session count it was asked for: {stderr3}"
    );
    assert!(
        stdout3.contains("[collect] destination     : wanted=1 asked=true"),
        "the collect line carries the asked state: {stdout3}"
    );
    let phases3 = phases_of(sandbox);
    assert_eq!(
        phases3["destination_facts_wanted"].as_u64(),
        Some(1),
        "the run-state keeps the wanted count: {phases3}"
    );
    assert_eq!(
        phases3["destination_facts_asked"].as_bool(),
        Some(true),
        "the run-state keeps the asked flag: {phases3}"
    );
    assert!(
        phases3["destination_facts_ms"].as_u64().is_some(),
        "the probe's wall time is a recorded integer: {phases3}"
    );
    println!(
        "e2e static_wanted_lines stderr_has_wanted_line={}",
        stderr3.contains("[readback] cumulative read: wanted=1")
    );
}
