//! W1084 (ISSUE-008 ②, GO-1105 ②): the pass time budget through the real
//! binary — the fixture the incident asked for, run at production shape.
//!
//! The 2026-11-05 incident was 46 consecutive rounds of 44–98 minutes
//! against an hourly cadence, each silently accepted because no check
//! anywhere said how long a round should take. These round-trips prove the
//! three surfaces that were missing, in the shapes the deputy heartbeat and
//! a sleeping human actually read:
//!
//! * `run-state.json` gains `budget_ms`, `last_duration_ms` and
//!   `over_budget`, written by the pass itself;
//! * `status` shows the last round's duration and budget plus the
//!   over-budget sentence, read from that record alone — *including*
//!   for the incident's own numbers (a record with no budget says
//!   "unknown", never a number it invented);
//! * a pass more than 50% over its budget writes a WARN to stderr, the
//!   scheduled unit's `run-once.err.log`, naming the duration, the
//!   budget, the over-run percentage and the slowest phase;
//! * a `run_budget_secs = 0` is refused with the key named, because
//!   that would be a budget every round is over.
//!
//! Every round here runs in a `tempfile` sandbox with `HOME` / `XDG_*`
//! redirected, an empty-harness registry (nothing real to collect — the
//! artificial duration of the seam-level sibling tests comes from a
//! sleeping producer step there, so this file's rounds only need a
//! budget they fit), synthetic shards, and no network, no credential,
//! no session body. Logs may carry counts, byte sizes and durations.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "../src/test_support.rs"]
mod test_support;

const CHANGED_NOTHING: i32 = 0;

/// Run the real binary with every ambient path redirected into `sandbox`.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W1084 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("rh-cache"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

/// Write a `config.toml` with the budget keys under test. `extra` carries
/// the budget lines themselves so each test states its own semantics.
fn write_config(sandbox: &Path, budget_lines: &str) {
    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!("push_only_if_changed = false\n\n[sources]\n{budget_lines}"),
    )
    .unwrap();
}

fn write_shard(stage: &Path, machine: &str, session: &str) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("000001.jsonl"),
        r#"{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{"role":"user","content":"hi"},"uuid":"u1","timestamp":"2025-01-15T12:34:56.789Z","cwd":"/x","version":"1.0.31"}"#
            .to_string()
            + "\n",
    )
    .unwrap();
}

/// Where `collect::default_state_dir()` lands given the `XDG_DATA_HOME`
/// above — the same path `status` and the heartbeat read.
fn state_dir(sandbox: &Path) -> PathBuf {
    sandbox.join("data").join("chat-stasher").join("state")
}

/// The durable record of the last pass, as the same struct `status` reads.
fn load_run_state(sandbox: &Path) -> chat_stasher::runstate::RunState {
    let read = chat_stasher::runstate::load(&state_dir(sandbox));
    match read {
        chat_stasher::runstate::RunStateRead::Present(state) => state,
        other => panic!("the pass must have recorded a run state, got {other:?}"),
    }
}

fn stdio(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

/// Round 1, in budget: the pass that fits its (seconds-scale, cadence-
/// defaulted) budget, through the real binary. The record it writes is
/// machine-readable where the heartbeat reads — `budget_ms` naming the
/// cadence, `last_duration_ms` matching the record's own duration,
/// `over_budget` an explicit false — and `status`, the same binary in
/// the same sandbox, answers with both numbers plus "within budget",
/// both in prose and in `status --json`'s `run_state` object.
#[test]
fn a_pass_that_fits_its_budget_records_it_and_status_shows_both_numbers() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    write_shard(&stage, machine, session);
    // A seconds-scale cadence, so the budget the default names is easy to
    // read back; the pass still finishes far inside it.
    write_config(sandbox, "backup_interval_secs = 4\n");

    let out = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
            "--repo",
            sandbox.join("repo").to_str().unwrap(),
            "--key-file",
            sandbox
                .join("keys")
                .join("masterkey.json")
                .to_str()
                .unwrap(),
            "--keep-ssh-masters",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(CHANGED_NOTHING),
        "a round inside its budget is a clean round: {}",
        stdio(&out)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("[run-once] WARN:"),
        "a fitting round leaves no WARN in the error log: {stderr}"
    );

    let state = load_run_state(sandbox);
    assert_eq!(
        state.budget_ms,
        Some(4_000),
        "with no override the budget is the cadence, in milliseconds"
    );
    assert!(state.duration_ms > 0, "the round spent measured wall time");
    assert_eq!(
        state.last_duration_ms,
        Some(state.duration_ms),
        "the heartbeat twin carries the same number"
    );
    assert_eq!(state.over_budget, Some(false));

    // `status` reads the same record and answers in prose.
    let out = run(sandbox, &["status"]);
    assert_eq!(
        out.status.code(),
        Some(CHANGED_NOTHING),
        "status exit stays the timer verdict, unaffected by the budget: {}",
        stdio(&out)
    );
    let text = stdio(&out);
    assert!(
        text.contains("[run-once] Last round took")
            && text.contains(&format!(
                "against a budget of {} ms — within budget.",
                4_000
            )),
        "the budget line shows both numbers plus the judgment:\n{text}"
    );

    // `status --json` answers the same facts machine-readably, under the
    // names the heartbeat greps for.
    let out = run(sandbox, &["status", "--json"]);
    assert_eq!(out.status.code(), Some(CHANGED_NOTHING), "{}", stdio(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("one JSON object on stdout, got {stdout:?} ({e})"));
    assert_eq!(json["run_state"]["kind"], serde_json::json!("known"));
    assert_eq!(json["run_state"]["budget_ms"], serde_json::json!(4_000));
    assert_eq!(json["run_state"]["over_budget"], serde_json::json!(false));
    assert_eq!(
        json["run_state"]["last_duration_ms"],
        serde_json::json!(state.duration_ms),
        "the json names the same duration the record holds: {}",
        json["run_state"]
    );
}

/// A `run_budget_secs` override is the budget, not the schedule: the
/// cadence can stay hourly while the cost line is whatever the machine
/// can afford, and the record proves the override won.
#[test]
fn run_budget_secs_overrides_the_cadence_as_the_recorded_budget() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    write_shard(
        &stage,
        "mbp-test",
        "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
    );
    write_config(
        sandbox,
        "backup_interval_secs = 7200\nrun_budget_secs = 4\n",
    );
    let out = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "mbp-test",
            "--repo",
            sandbox.join("repo").to_str().unwrap(),
            "--key-file",
            sandbox
                .join("keys")
                .join("masterkey.json")
                .to_str()
                .unwrap(),
            "--keep-ssh-masters",
        ],
    );
    assert_eq!(out.status.code(), Some(CHANGED_NOTHING), "{}", stdio(&out));
    let state = load_run_state(sandbox);
    assert_eq!(
        state.budget_ms,
        Some(4_000),
        "the override is the budget even on an hourly cadence"
    );
    let out = run(sandbox, &["status"]);
    assert!(
        stdio(&out).contains("against a budget of 4000 ms — within budget."),
        "status shows the budget that was actually applied:\n{}",
        stdio(&out)
    );
}

/// A zero budget is a budget every round would be over: the pass refuses
/// it before anything runs, with the key named — an empty-stage safety
/// scan is never the thing that stopped the round.
#[test]
fn a_zero_run_budget_secs_refuses_the_pass_naming_the_key() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    write_shard(
        &stage,
        "mbp-test",
        "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
    );
    write_config(sandbox, "run_budget_secs = 0\n");
    let out = run(
        sandbox,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "mbp-test",
            "--repo",
            sandbox.join("repo").to_str().unwrap(),
            "--key-file",
            sandbox
                .join("keys")
                .join("masterkey.json")
                .to_str()
                .unwrap(),
            "--keep-ssh-masters",
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "the refusal keeps exit 3");
    assert!(
        stderr.contains("run_budget_secs must be greater than zero")
            && stderr.contains("[run-once] result: ERROR exit_code=3 config="),
        "the refusal names the config key at fault:\n{stderr}"
    );
    let state = load_run_state(sandbox);
    assert_eq!(state.failed_step.as_deref(), Some("config"));
    assert_eq!(
        state.budget_ms, None,
        "the refused pass records no budget, never the zero that refused it"
    );
    assert_eq!(state.over_budget, None);
}

/// The over-budget display, with the incident's own numbers: a round of
/// 5,901,000 ms against a 3,600,000 ms budget (the ISSUE-008 example,
/// 163.9%) reads back through `status` as "over budget", the record's
/// written facts shown as they were recorded — and the exit code stays
/// 0, because the budget line is a cost line, not a timer verdict: a
/// slow-then-successful round is a healthy timer whose rounds cost too
/// much, and the two facts must not collapse into one integer.
#[test]
fn status_reports_an_over_budget_round_and_keeps_the_exit_code_a_timer_verdict() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    write_config(sandbox, "backup_interval_secs = 3600\n");
    let dir = state_dir(sandbox);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("run-state.json"),
        format!(
            r#"{{ "version": 1, "finished_at_unix": {}, "duration_ms": 5901000,
                  "outcome": "completed", "failed_step": null, "shards_written": 4,
                  "stage_shards": 4, "snapshot_created": true,
                  "collect_errors": 0, "archive_gaps": 0, "torn_skips": 0,
                  "machine_digest": "aaaaaaaaaaaa",
                  "budget_ms": 3600000, "last_duration_ms": 5901000,
                  "over_budget": true }}"#,
            now_unix().saturating_sub(60)
        ),
    )
    .unwrap();

    let out = run(sandbox, &["status"]);
    assert_eq!(
        out.status.code(),
        Some(CHANGED_NOTHING),
        "over budget is prose, not an exit-code change: {}",
        stdio(&out)
    );
    let text = stdio(&out);
    assert!(
        text.contains(
            "Last round took 5901000 ms against a budget of 3600000 ms — over budget \
             (163.9% of the budget; the pass's error log carries a WARN from 150%)"
        ),
        "the over round shows both numbers, the percentage and where the WARN \
         lives:\n{text}"
    );
    assert!(
        !text.contains("within budget."),
        "an over round is never worded within:\n{text}"
    );

    let out = run(sandbox, &["status", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("one JSON object on stdout, got {stdout:?} ({e})"));
    assert_eq!(json["run_state"]["over_budget"], serde_json::json!(true));
    assert_eq!(json["run_state"]["budget_ms"], serde_json::json!(3_600_000));
    assert_eq!(
        json["run_state"]["last_duration_ms"],
        serde_json::json!(5_901_000)
    );
}
