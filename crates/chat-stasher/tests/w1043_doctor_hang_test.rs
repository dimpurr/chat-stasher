//! W1043 regression tests — `doctor` must always reach a report, even when a
//! destination never answers.
//!
//! Rollback proof: before this change, one wedged check froze the whole
//! command: every section printed only at the end after every check, and a
//! destination whose connection sat unanswered held the report hostage with
//! zero output (the observed incident — 90 s on an idle machine, no line
//! printed). The fix runs every check behind its own deadline and traces it
//! before it starts. Each fixture below asserts the deadline machinery's
//! observable answer end to end, so reverting the harness makes the fixture
//! red: the `CHAT_STASHER_DOCTOR_CHECK_BUDGET_SECS` override would be read by
//! nothing and the blackhole probe would hang the spawned `doctor` until the
//! test runner's own deadline (or assert a clean exit it cannot get).
//!
//! The wedge is deterministic and machine-local: a TCP listener that accepts
//! the connection and then never answers it — the exact shape the observed
//! hang had (connect succeeds, the read never ends) — pointed at by an
//! S3-shaped destination in a sandboxed HOME. No real remote, credential or
//! session body is involved; names, counts and bytes below are synthetic.

#[path = "../src/test_support.rs"]
mod test_support;

use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The process environment is process-global and `cargo test` runs this
/// file's tests as threads of one process; every spawn snapshots it. The
/// budget override is a write to it (B97's rule: readers serialize too).
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Bind a listener on loopback that accepts connections and then never
/// answers one — a connect-succeeds-then-reads-forever wedge, the shape the
/// observed hang had. The accepted sockets are held open (dropping one would
/// send an RST and turn the wedge into a quick error, which is the failure
/// mode this fixture must not be satisfied by).
fn blackhole() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the blackhole on loopback");
    let port = listener
        .local_addr()
        .expect("read the blackhole's port")
        .port();
    std::thread::spawn(move || {
        let mut held: Vec<std::net::TcpStream> = Vec::new();
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    // Set no read/write timeout: the wedge is the point.
                    held.push(stream);
                }
                Err(_) => break,
            }
        }
        // `held` lives as long as this thread: the process outlives the
        // fixture, so the sockets stay open until the test binary exits.
        drop(held);
    });
    port
}

/// A `doctor` command in a fully isolated HOME whose only declared
/// destination is the blackhole, with every check's budget set to
/// `budget_secs` (the test override W1043 ships; the fixture would
/// otherwise be a 45-second wait).
fn doctor_against_blackhole(
    sandbox: &Path,
    registry: &Path,
    port: u16,
    json: bool,
    budget_secs: u64,
) -> Command {
    let config_dir = sandbox.join("xdg-config").join("chat-stasher");
    fs::create_dir_all(&config_dir).expect("create the isolated config dir");
    let mut config =
        fs::File::create(config_dir.join("config.toml")).expect("write the isolated config");
    writeln!(
        config,
        r#"machine = "w1043-fixture"

[destinations.blackhole]
repo = "opendal:s3"
connections = 2

[destinations.blackhole.options]
bucket = "chat-stasher"
endpoint = "http://127.0.0.1:{port}"
region = "auto"
root = "/synthetic/v1"
access_key_id = "synthetic-access-key-id"
secret_access_key = "synthetic-secret-access-key"
disable_config_load = "true"
"#
    )
    .expect("write the destination");

    let home = sandbox.join("home");
    fs::create_dir_all(&home).expect("create isolated HOME");
    let no_tools = sandbox.join("no-host-tools");
    fs::create_dir_all(&no_tools).expect("create empty PATH directory");
    let mut args = vec!["doctor".to_string()];
    if json {
        args.push("--json".to_string());
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .args(&args)
        .env("PATH", &no_tools)
        .env("HOSTNAME", "w1043-fixture")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("XDG_CACHE_HOME", sandbox.join("xdg-cache"))
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("CHAT_STASHER_REGISTRY", registry)
        .env(
            "CHAT_STASHER_DOCTOR_CHECK_BUDGET_SECS",
            budget_secs.to_string(),
        )
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .env_remove("CURSOR_USER_DIR");
    command
}

/// The empty registry `doctor`'s scan needs (and one no-op cell is worth
/// more than a scan failure that would exit 3 on its own, muddying the
/// fixture's one-timed-out-check story: every other way to exit 3 must be
/// out of the picture, so the only INCOMPLETE this run carries is the
/// deadline's).
fn empty_registry(sandbox: &Path) -> std::path::PathBuf {
    let path = sandbox.join("registry.json");
    fs::write(
        &path,
        br#"{"schema_version":1,"generated":"W1043","harnesses":[]}"#,
    )
    .expect("write synthetic empty registry");
    path
}

/// One run's whole evidence package, in the shape the assertions read.
///
/// The child is polled and killed at a hard deadline rather than waited on:
/// this fixture exists to catch the very hang it simulates, and a fixture
/// that could hang with it would turn the regression it guards into a
/// stuck CI job instead of a red test. `code: None` therefore means "the
/// child was still alive at the deadline" — every exit-code assertion
/// fails loudly on that, which is exactly what a regression of W1043 must
/// look like.
struct Run {
    code: Option<i32>,
    text: String,
    stdout: String,
    elapsed: Duration,
}

#[allow(
    clippy::let_underscore_must_use,
    reason = "the reads and the kill below are best-effort by design: a child that was killed \
              at the hard deadline leaves a truncated stream whose bytes are the evidence, and \
              reaping a process that may already have exited is exactly the failure whose \
              verdict `None` — not an `unwrap` — encodes"
)]
fn run_doctor(mut command: Command) -> Run {
    const HARD_DEADLINE: Duration = Duration::from_secs(60);
    use std::io::Read;
    use std::process::Stdio;
    let started = Instant::now();
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn doctor");
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    // Drain on threads of their own: a child that fills an unread pipe
    // buffer would block on writing, not on wedging, and the fixture is
    // about the wedge.
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() > HARD_DEADLINE {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("waiting for doctor: {error}"),
        }
    };
    let stdout = stdout_reader.join().expect("read stdout");
    let stderr = stderr_reader.join().expect("read stderr");
    Run {
        code: status.map(|status| status.code().unwrap_or(-1)),
        text: format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        ),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        elapsed: started.elapsed(),
    }
}

/// The returning story: a destination that holds the connection and never
/// answers gets its probe abandoned at the budget, the trace names the
/// check before it wedges, the report still prints every other section,
/// the timed-out sections are worded as unknown — and the exit code is 3,
/// never 0 for a machine only half of which was inspected. The whole run
/// stays bounded: N is what the fixture will actually wait for
/// (a handful of local checks plus the budget), not "as long as the hang".
#[test]
fn doctor_returns_within_n_seconds_when_a_destination_never_answers() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let registry = empty_registry(sandbox.path());
    let port = blackhole();
    let run = run_doctor(doctor_against_blackhole(
        sandbox.path(),
        &registry,
        port,
        false,
        2,
    ));

    // N = a 2 s budget plus every local check on an empty sandbox (all
    // sub-second), with an order of slack for a busy CI machine — the point
    // is the bound, not the exact number.
    assert!(
        run.elapsed < Duration::from_secs(30),
        "doctor must bound its run when a check wedges; waited {:?}",
        run.elapsed
    );
    assert_eq!(run.code, Some(3), "the deadline's exit is 3, not a hang");
    assert!(
        run.text
            .contains("doctor: checking D7 destination probe (blackhole) — connection"),
        "the trace must name the check before it runs, so a re-hang leaves the \
         culprit on a line: {run:?}",
        run = run.text
    );
    assert!(
        run.text
            .contains("🔴 CHECK TIMED OUT — these checks outlived their own deadline"),
        "the report's block opens the timed-out story: {}",
        run.text
    );
    assert!(
        run.text
            .contains("D7 destination probe (blackhole) — connection (no answer within 2s)"),
        "the note names the check and its budget: {}",
        run.text
    );
    assert!(
        run.text.contains(
            "doctor: INCOMPLETE exit_code=3 — the D7 destination probe (blackhole) — \
             connection check did not answer within 2s"
        ),
        "one INCOMPLETE line per timed-out check: {}",
        run.text
    );
    assert!(
        run.text
            .contains("NO ANSWER WITHIN 2s (the probe outlived its deadline)"),
        "the D7 line words the deadline, not a refusal: {}",
        run.text
    );
    assert!(
        run.text.contains("Reachability is UNKNOWN, not absent"),
        "the unknown stays distinct from a measured absence: {}",
        run.text
    );
    // The sections that did answer stand as printed, and the timed-out
    // ones say so instead of showing their placeholder values.
    assert!(
        run.text.contains("D1 · Claude Code rotation settings")
            && run.text.contains("cleanupPeriodDays: unset (default 30d)"),
        "D1 answered and still stands: {}",
        run.text
    );
    assert!(
        run.text.contains("D2 · Gemini CLI retention policy")
            && run.text.contains("sessionRetention: enabled, maxAge=30d"),
        "D2 answered and still stands: {}",
        run.text
    );
    // The destination is not "reachable", "not configured" — and crucially
    // not "NOT REACHED", the word a measured refusal owns.
    assert!(
        !run.text.contains("NOT REACHED ("),
        "a deadline is not a measured refusal; the D7 line must not use the \
         unreachable wording: {}",
        run.text
    );
}

/// The machine-readable half of the same story: `--json` carries the
/// `timed_out` array one level above the sections (a placeholder must not
/// be presentable as a measurement), the destination's kind is
/// `probe_timed_out`, and the sections beside it are the ordinary checked
/// shapes.
#[test]
fn doctor_json_names_the_timed_out_check_and_keeps_the_other_sections() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let registry = empty_registry(sandbox.path());
    let port = blackhole();
    let run = run_doctor(doctor_against_blackhole(
        sandbox.path(),
        &registry,
        port,
        true,
        2,
    ));
    assert_eq!(
        run.code,
        Some(3),
        "the deadline's exit is 3 in JSON mode too"
    );
    let value: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|error| panic!("doctor --json must still print one object: {error}"));
    let timed = value["timed_out"]
        .as_array()
        .expect("timed_out is an array");
    assert!(
        timed
            .iter()
            .any(|note| note["check"] == "D7 destination probe (blackhole) — connection"),
        "the array names the connection check: {value}"
    );
    assert_eq!(
        value["destinations"][0]["kind"],
        serde_json::json!("probe_timed_out"),
        "the destination's own kind words the deadline: {value}"
    );
    assert_eq!(
        value["destinations"][0]["budget_secs"],
        serde_json::json!(2),
        "the kind carries the budget it exceeded: {value}"
    );
    assert_eq!(
        value["claude"]["verdict"]["kind"],
        serde_json::json!("unset_default"),
        "the answered checks keep their ordinary checked shapes: {value}"
    );
}

/// The paved path: with every check able to answer quickly, doctor exits 0,
/// no timed-out block prints, and the trace's `checking` lines still show
/// the steps a hung run would have left behind.
#[test]
fn doctor_stays_green_and_traceable_when_every_check_answers() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let registry = empty_registry(sandbox.path());
    let home = sandbox.path().join("home");
    fs::create_dir_all(&home).expect("create isolated HOME");
    let no_tools = sandbox.path().join("no-host-tools");
    fs::create_dir_all(&no_tools).expect("create empty PATH directory");
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .args(["doctor"])
        .env("PATH", &no_tools)
        .env("HOSTNAME", "w1043-fixture")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.path().join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.path().join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.path().join("xdg-state"))
        .env("XDG_CACHE_HOME", sandbox.path().join("xdg-cache"))
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("CHAT_STASHER_REGISTRY", &registry)
        .env("CHAT_STASHER_DOCTOR_CHECK_BUDGET_SECS", "10")
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .env_remove("CURSOR_USER_DIR");
    let run = run_doctor(command);
    assert_eq!(
        run.code,
        Some(0),
        "a fully answered run stays exit 0: {}",
        run.text
    );
    assert!(
        !run.text.contains("CHECK TIMED OUT"),
        "nothing timed out, so no block: {}",
        run.text
    );
    assert!(
        run.text
            .contains("doctor: checking D1 Claude Code rotation settings")
            && run
                .text
                .contains("doctor: checking D3 harness registry and scan"),
        "the trace shows the steps before they run: {}",
        run.text
    );
}

/// The production-budget variant of the returning story, on demand: run with
/// `cargo test --test w1043_doctor_hang_test -- --ignored` to see the
/// default 45 s connection budget expire against the same blackhole. The
/// always-on fixtures above use the override because a gate that waits 45
/// s per case on every machine is a gate people stop running.
#[test]
#[ignore = "waits out the production 45s connection budget on purpose"]
fn doctor_bounds_the_production_budget_too() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let registry = empty_registry(sandbox.path());
    let port = blackhole();
    let started = Instant::now();
    let run = run_doctor(doctor_against_blackhole(
        sandbox.path(),
        &registry,
        port,
        false,
        45,
    ));
    assert!(
        started.elapsed() < Duration::from_secs(90),
        "the production budget bounds the run: waited {:?}",
        started.elapsed()
    );
    assert_eq!(run.code, Some(3), "exit 3, not a hang: {}", run.text);
    assert!(
        run.text
            .contains("doctor: checking D7 destination probe (blackhole) — connection"),
        "the trace names the wedge: {}",
        run.text
    );
    assert!(
        run.text.contains("NO ANSWER WITHIN 45s"),
        "the D7 line carries the production budget: {}",
        run.text
    );
}
