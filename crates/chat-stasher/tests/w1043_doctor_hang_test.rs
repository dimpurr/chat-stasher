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
//! W1043f adds the config-reuse regressions (rev0's High): a config file
//! whose reader never answers is the wedge the fixture pair below pins.
//! When the bounded config check times out, the CLI must reuse that
//! "no usable config" answer — the unfixed command re-issued the same
//! deadline-less `fs::read_to_string` and hung forever on the wedged file.
//!
//! The wedge is deterministic and machine-local: a TCP listener that accepts
//! the connection and then never answers it — the exact shape the observed
//! hang had (connect succeeds, the read never ends) — pointed at by an
//! S3-shaped destination in a sandboxed HOME; the config-level wedge is a
//! FIFO nobody ever opens for writing, so the reader's `open()` never
//! returns. No real remote, credential or session body is involved; names,
//! counts and bytes below are synthetic.

#[path = "../src/test_support.rs"]
mod test_support;

use std::fs;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Every run below measures a wall-clock budget, so two fixtures running at
/// once on the same cores could make a working fix look like a miss: they
/// serialize through this lock, which is what the timing assertions below
/// measure (B97's rule: the readers of one slow resource serialize too).
/// No spawned fixture writes to this process's environment — each child
/// gets its variables through `Command::env`, which snapshots them at
/// spawn; the one in-process fixture sets and restores the real ones under
/// this lock.
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
    let budget: u64 = 2;
    let run = run_doctor(doctor_against_blackhole(
        sandbox.path(),
        &registry,
        port,
        false,
        budget,
    ));

    // N = the pathological upper bound of a *working* fix, derived rather
    // than guessed (W1043 rev0's L3): the slowest a correct run can be is
    // every budgeted check consuming its full budget — ~18 of them on this
    // fixture (the settings pair, the config, the scan, the other-harness
    // listing, the desktop probe, the nine config-derived sections, the
    // machine-log rows, the CLI's inbox and the one destination probe that
    // actually wedges) — plus slack for a busy CI machine. Deliberately
    // under `run_doctor`'s 60 s hard kill: the kill plus the exit-code
    // assertions are the bound's teeth, and a fixture whose number sits
    // below what whole-machine slowness can produce is a flake, not a
    // bound.
    let bound = Duration::from_secs(budget * 18 + 10);
    assert!(
        run.elapsed < bound,
        "doctor must bound its run when a check wedges; waited {:?} (bound {bound:?})",
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

// ---------------------------------------------------------------------------
// W1043f — the config a wedged run must not re-read (rev0's High)
// ---------------------------------------------------------------------------

/// W1043f (rev0's High): a config file whose reader never answers is the
/// same class of wedge the blackhole is, one step earlier in the report —
/// `Config::load` is a plain `fs::read_to_string`, and the library already
/// bounds it. The regression this pair of fixtures pins is what the CLI did
/// **after** that bounded read reported a timeout: it re-issued the same
/// deadline-less read and hung forever on the same wedged file. The fixed
/// command reuses the answer the report already carries.
///
/// The wedge: a FIFO nobody ever opens for writing, so the reader's
/// `open()` never returns. Unix-only mechanism, stated per CLAUDE.md's cfg
/// rule because it is the property that is platform-bound, not the fix: a
/// writer-less FIFO is the only file shape `std` can create whose *open for
/// read* blocks, and Windows offers no equivalent without win32 named-pipe
/// API surface this dependency set does not carry. The replaced read and
/// the reuse are platform-neutral Rust control flow, and the unix CI cells
/// run this regression on every push.
#[cfg(unix)]
fn wedged_config(sandbox: &Path) -> std::path::PathBuf {
    let config_dir = sandbox.join("xdg-config").join("chat-stasher");
    fs::create_dir_all(&config_dir).expect("create the isolated config dir");
    let path = config_dir.join("config.toml");
    let mkfifo = Command::new("mkfifo")
        .arg(&path)
        .status()
        .expect("run mkfifo for the wedged config fixture");
    assert!(
        mkfifo.success(),
        "mkfifo must create the wedged config at {}",
        path.display()
    );
    path
}

/// A `doctor` command against the wedged config, in the same isolated HOME
/// shape `doctor_against_blackhole` uses, with every check's budget at
/// `budget_secs`.
#[cfg(unix)]
fn doctor_against_wedged_config(
    sandbox: &Path,
    registry: &Path,
    json: bool,
    budget_secs: u64,
) -> Command {
    wedged_config(sandbox);
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

/// The bounded config check times out, words the unknown honestly, and the
/// command still reaches its report and exits 3 — the deadline did its job.
/// What this fixture is for is the step after: the run must not re-read the
/// same wedged file unbounded. Under the unfixed command this exact spawn
/// hung on the second `Config::load` with zero report printed, and the
/// fixture's 60 s kill is the line it dies on.
#[test]
#[cfg(unix)]
fn doctor_does_not_reread_a_config_that_never_answers() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let registry = empty_registry(sandbox.path());
    let budget: u64 = 2;
    let run = run_doctor(doctor_against_wedged_config(
        sandbox.path(),
        &registry,
        false,
        budget,
    ));
    // Exactly one check can consume its budget on this fixture (the config
    // read; D1/D2 read absent files and the CLI has nothing to probe), so
    // the bound is that budget plus the empty-sandbox locals and slack —
    // and far under `run_doctor`'s 60 s kill, which is how a reverted fix
    // fails this test instead of hanging CI.
    assert!(
        run.elapsed < Duration::from_secs(budget + 20),
        "doctor must reach its report without re-reading the wedged config; \
         waited {:?}",
        run.elapsed
    );
    assert_eq!(
        run.code,
        Some(3),
        "the config deadline's exit is 3, not a hang: {}",
        run.text
    );
    assert!(
        run.text.contains("doctor: checking configuration file"),
        "the trace must name the config check before it wedges: {}",
        run.text
    );
    assert!(
        run.text.contains(&format!(
            "reading the configuration did not answer within {budget}s"
        )),
        "the honest note words the unknown: {}",
        run.text
    );
    assert!(
        run.text.contains(&format!(
            "the configuration file check did not answer within {budget}s"
        )),
        "the per-check INCOMPLETE line prints on the config-error exit too, not \
         only on the timeout-only one (rev0's L2): {}",
        run.text
    );
    assert!(
        run.text.contains("the config file could not be used"),
        "the config-error branch still names the half of the setup it skipped: {}",
        run.text
    );
    assert!(
        !run.text.contains("doctor: checking D7 destination probe"),
        "no destination was dialed: with no usable config there is nothing to \
         probe, and re-reading the file is the hang this fixture pins: {}",
        run.text
    );
}

/// The machine-readable half of the same run: `timed_out` names the config
/// check, `config_error` is a string, and `destinations` stays empty — a
/// measured destination beside a "did not look" config was the second half
/// of the review's finding, and reuse closes it by construction: there is
/// no second config to disagree with the report.
#[test]
#[cfg(unix)]
fn doctor_json_keeps_one_story_when_the_config_never_answers() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let registry = empty_registry(sandbox.path());
    let budget: u64 = 2;
    let run = run_doctor(doctor_against_wedged_config(
        sandbox.path(),
        &registry,
        true,
        budget,
    ));
    assert_eq!(
        run.code,
        Some(3),
        "the config deadline's exit is 3 in JSON mode too: {}",
        run.text
    );
    let value: serde_json::Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|error| panic!("doctor --json must still print one object: {error}"));
    assert!(
        !value["config_error"].is_null(),
        "the config error is a string a script reads: {value}"
    );
    let timed = value["timed_out"]
        .as_array()
        .expect("timed_out is an array");
    assert!(
        timed
            .iter()
            .any(|note| note["check"] == "configuration file" && note["budget_secs"] == 2),
        "the array names the config check with its budget: {value}"
    );
    assert_eq!(
        value["destinations"],
        serde_json::json!([]),
        "nothing was probed, and an empty array under a non-null config_error \
         reads as 'did not look' — the one story: {value}"
    );
    assert_eq!(
        value["claude"]["verdict"]["kind"],
        serde_json::json!("unset_default"),
        "the checks that did run keep their ordinary shapes: {value}"
    );
}

/// The healthy half of the reuse: a run whose config answered hands the
/// caller the exact config the report was measured from, so the CLI's
/// probes reuse it instead of re-reading the file. In-process, because the
/// spawned fixtures cannot see the pub field; env is swapped under the
/// file's lock and restored on the way out (including a panic), the same
/// contract `doctor_empty_machine_test` keeps.
#[test]
fn run_hands_the_cli_the_config_it_was_measured_from() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let registry = empty_registry(sandbox.path());
    let config_dir = sandbox.path().join("xdg-config").join("chat-stasher");
    fs::create_dir_all(&config_dir).expect("create the isolated config dir");
    let mut config =
        fs::File::create(config_dir.join("config.toml")).expect("write the isolated config");
    writeln!(
        config,
        r#"machine = "w1043f-fixture"

[destinations.synthetic-destination]
repo = "opendal:s3"

[destinations.synthetic-destination.options]
bucket = "chat-stasher"
endpoint = "http://127.0.0.1:1"
region = "auto"
root = "/synthetic/v1"
access_key_id = "synthetic-access-key-id"
secret_access_key = "synthetic-secret-access-key"
disable_config_load = "true"
"#
    )
    .expect("write the synthetic destination");

    let home = sandbox.path().join("home");
    fs::create_dir_all(&home).expect("create isolated HOME");
    let swap = swap_env(&[
        ("HOME", home.clone().into_os_string()),
        ("USERPROFILE", home.clone().into_os_string()),
        (
            "XDG_CONFIG_HOME",
            sandbox.path().join("xdg-config").into_os_string(),
        ),
        (
            "XDG_DATA_HOME",
            sandbox.path().join("xdg-data").into_os_string(),
        ),
        (
            "XDG_STATE_HOME",
            sandbox.path().join("xdg-state").into_os_string(),
        ),
        (
            "XDG_CACHE_HOME",
            sandbox.path().join("xdg-cache").into_os_string(),
        ),
        (
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home).into_os_string(),
        ),
        ("CHAT_STASHER_REGISTRY", registry.into_os_string()),
        ("CHAT_STASHER_DOCTOR_CHECK_BUDGET_SECS", "10".into()),
        ("CODEX_HOME", std::ffi::OsString::new()),
        ("GEMINI_CLI_HOME", std::ffi::OsString::new()),
        ("OPENCODE_DB", std::ffi::OsString::new()),
        ("CURSOR_USER_DIR", std::ffi::OsString::new()),
    ]);
    let report = chat_stasher::doctor::run();
    drop(swap);

    assert!(
        report.config_error.is_none(),
        "the planted config parses: {:?}",
        report.config_error
    );
    assert!(
        !report.scan_failed,
        "the empty registry loads, so the run reached every section"
    );
    let config = report
        .config
        .as_ref()
        .expect("a run with a usable config hands its caller that config");
    assert_eq!(
        config.source.label(),
        "file",
        "the carried config is the file's meaning, not defaults"
    );
    assert_eq!(config.machine.as_deref(), Some("w1043f-fixture"));
    assert!(
        config.destinations.contains_key("synthetic-destination"),
        "the carried config names the declared destination — the probes reuse \
         exactly this and never re-read the file"
    );
    // `run()` never dials the destination (ADR-023: probes are the CLI's
    // step), so this fixture stays machine-local: the endpoint is synthetic,
    // never contacted, and `destinations` stayed empty as the lib entry
    // point promises.
    assert!(report.destinations.is_empty());
}

/// Snapshot + swap the given environment variables, restoring the snapshot
/// when returned (restore is a drop, so a panicking test gives the machine
/// back its environment on the way out). An empty `value` removes the
/// variable. Only for the one in-process fixture above, under the file's
/// ENV_LOCK: env mutation is process-global.
struct SwappedEnv {
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

fn swap_env(pairs: &[(&'static str, std::ffi::OsString)]) -> SwappedEnv {
    let mut previous = Vec::with_capacity(pairs.len());
    for (name, value) in pairs {
        previous.push((*name, std::env::var_os(name)));
        if value.is_empty() {
            std::env::remove_var(name);
        } else {
            std::env::set_var(name, value);
        }
    }
    SwappedEnv { previous }
}

impl Drop for SwappedEnv {
    fn drop(&mut self) {
        for (name, value) in self.previous.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// W1043f2 — the sections a wedged scan used to vanish (rev1's Medium) and
// their text half, which reverting the printer wirings kept green
// ---------------------------------------------------------------------------

/// The FIFO shape `wedged_config` uses, lifted so the registry fixture can
/// wedge the same way: a FIFO nobody ever opens for writing, so the
/// reader's `open()` never returns. Unix-only for the same reason the
/// config fixture is — `std` cannot create a writer-less FIFO on Windows —
/// and the property it pins (text and `--json` telling one story on a
/// wedged check) is platform-neutral control flow exercised by the unix
/// CI cells on every push.
#[cfg(unix)]
fn wedged_fifo(at: &Path) {
    let mkfifo = Command::new("mkfifo")
        .arg(at)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "run mkfifo for the wedged FIFO at {}: {error}",
                at.display()
            )
        });
    assert!(
        mkfifo.success(),
        "mkfifo must create the wedged FIFO at {}",
        at.display()
    );
}

/// A `doctor` command in the same isolated HOME shape the fixtures above
/// use, with the config planted as a real readable file (this round's
/// wedge is elsewhere: the registry or one check's own input) and every
/// check's budget at `budget_secs`. `path_dir` becomes the child's whole
/// `PATH`: an empty directory for the plain fixtures, or one carrying a
/// `hostname` that never answers for the machine-identity wedge.
fn doctor_with_planted_config(
    sandbox: &Path,
    registry: &Path,
    config_body: &str,
    path_dir: &Path,
    json: bool,
    budget_secs: u64,
) -> Command {
    let config_dir = sandbox.join("xdg-config").join("chat-stasher");
    fs::create_dir_all(&config_dir).expect("create the isolated config dir");
    fs::write(config_dir.join("config.toml"), config_body).expect("write the planted config");
    let home = sandbox.join("home");
    fs::create_dir_all(&home).expect("create isolated HOME");
    fs::create_dir_all(path_dir).expect("create the isolated PATH directory");
    let mut args = vec!["doctor".to_string()];
    if json {
        args.push("--json".to_string());
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .args(&args)
        .env("PATH", path_dir)
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

/// W1043f2 (rev1's Medium): when the scan itself outlives its budget, the
/// three sections `run()` measures without it — the destination
/// reconciliation, the mirror and the key inventory — must still print,
/// measured or wedged, on the same footing the nine W1043f sections got.
/// The fixture wedges the scan (registry FIFO) *and* the key inventory (a
/// FIFO at the declarations store only `key_inventory` opens), so one run
/// pins both halves: a measured D12/D13 keeps its heading, and a wedged
/// Keys keeps its heading plus the one UNKNOWN line naming the check and
/// its budget — the `print_timed_out_section` text M1 shipped, asserted
/// here in stderr rather than only in `--json` (rev1's L3: reverting the
/// printer wirings kept every JSON-only gate green).
#[test]
#[cfg(unix)]
fn doctor_keeps_the_sections_a_wedged_scan_does_not_gate() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    // The wedged scan: `load_registry` is the first thing the D3 closure
    // reads, and a writer-less FIFO is the one file shape whose open()
    // never returns — the scan's own budget expires and the report lands
    // on the scan-failed path with a D3 note, the exact shape the review
    // found vanishing sections on.
    let registry = sandbox.path().join("registry.json");
    wedged_fifo(&registry);
    // The wedged key inventory: `key_inventory` opens the declarations
    // store before it can list anything, and nothing else in a doctor run
    // opens this file, so the FIFO wedges the keys check alone.
    let state_dir = sandbox
        .path()
        .join("xdg-data")
        .join("chat-stasher")
        .join("state");
    fs::create_dir_all(&state_dir).expect("create the isolated state dir");
    wedged_fifo(&state_dir.join("key-declarations.json"));
    // A real, readable config — the registry and the declarations store
    // are the only wedged readers — declaring a mirror with the switch
    // off, so the D13 section prints its configured-off line
    // deterministically instead of staying silent as a mirror-less
    // config legitimately would.
    let config = "machine = \"w1043f2-fixture\"\n\n[mirror]\nenabled = false\n";
    let budget: u64 = 2;
    let path_dir = sandbox.path().join("no-host-tools");
    let run = run_doctor(doctor_with_planted_config(
        sandbox.path(),
        &registry,
        config,
        &path_dir,
        false,
        budget,
    ));
    // The bound is derived, not guessed: exactly two checks on this
    // fixture can consume their full budget (the scan and the key
    // inventory), everything else answers in milliseconds, and the slack
    // covers a busy machine. Deliberately under `run_doctor`'s 60 s kill,
    // so a reverted *deadline machinery* fails this fixture as a hang and
    // kill, not a quiet pass.
    assert!(
        run.elapsed < Duration::from_secs(budget * 2 + 20),
        "two wedged checks bound the run; waited {:?}",
        run.elapsed
    );
    assert_eq!(
        run.code,
        Some(3),
        "the wedged scan and keys walk exit 3 together: {}",
        run.text
    );
    assert!(
        run.text
            .contains("doctor: checking D3 harness registry and scan"),
        "the trace names the scan before it wedges: {}",
        run.text
    );
    assert!(
        run.text.contains(&format!(
            "D3 · Coverage — 🔴 the registry-driven scan did not answer within {budget}s"
        )),
        "the scan-failed branch words its own timeout: {}",
        run.text
    );
    assert!(
        run.text.contains(
            "D12 · Declared destinations vs installed timers — push coverage, credentials, last push, reclaim"
        ),
        "a measured destination reconciliation keeps its heading on the scan-failed \
         path — the vanishing section rev1 found: {run}",
        run = run.text,
    );
    assert!(
        run.text
            .contains("D13 · Local mirror — size, last sync, snapshots behind, orphans (ADR-062)")
            && run.text.contains(
                "[mirror] configured but `enabled = false` — this machine mirrors nothing"
            ),
        "a measured mirror keeps its heading and its configured-off line on the \
         scan-failed path: {run}",
        run = run.text,
    );
    assert!(
        run.text.contains(&format!(
            "Keys · one file per archive copy — each opens only its own copy — 🔴 UNKNOWN: \
             the key inventory check did not answer within {budget}s, so which key files \
             this machine holds is unknown, not empty."
        )),
        "a wedged key inventory keeps its heading and one UNKNOWN line naming the check \
         and the budget — the M1 text half, pinned in text rather than JSON only: {run}",
        run = run.text,
    );
}

/// The PATH directory that hangs every `hostname -s` lookup: doctor resolves
/// its machine identity through `Command::new("hostname").arg("-s")`, which
/// has no timeout of its own, so a `hostname` that never answers wedges
/// exactly the checks that resolve identity — the scan (the first thing
/// `scan_with_registry` does) and the machine-log rows. `exec` replaces the
/// shell with the sleeper, and the sleep is bounded at 30 s so a worker
/// abandoned by its budget cannot leave a stray process beyond the test
/// run. Unix-only like the FIFO fixtures: the /bin/sh script is the wedge
/// mechanism, while the property it drives — a wedged machine-logs check
/// keeps its sub-block named — is platform-neutral printer control flow.
#[cfg(unix)]
fn hanging_hostname_dir(sandbox: &Path) -> std::path::PathBuf {
    let dir = sandbox.join("hanging-tools");
    fs::create_dir_all(&dir).expect("create the hanging PATH dir");
    let script = dir.join("hostname");
    // `/bin/sleep` is absolute because the child's whole PATH is this
    // directory: a bare `sleep` would not be found and the "hang" would
    // exit as a command-not-found error, which machine_id words as a
    // missing source and falls back from — not the wedge this fixture is.
    fs::write(&script, "#!/bin/sh\nexec /bin/sleep 30\n").expect("write the hanging hostname");
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
        .expect("make the hanging hostname executable");
    dir
}

/// The same scan-failed path, one layer deeper: the machine-log rows check
/// wedges without any file being unreadable — its identity lookup never
/// answers — and the text report must print the machine-log sub-block's
/// UNKNOWN line under the D3 block (the smaller instance rev1 found:
/// `print_machine_logs` returned silently on the wedged walk while
/// `--json` answered `{"checked": false, "timed_out_after_secs": N}`).
/// Against the unfixed printer the line does not print at all.
#[test]
#[cfg(unix)]
fn doctor_names_a_wedged_machine_log_rows_check_in_text() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let sandbox = tempfile::tempdir().expect("create sandbox");
    // A healthy registry the scan can parse — so the wedge is the identity
    // lookup inside it, not the registry read.
    let registry = empty_registry(sandbox.path());
    let path_dir = hanging_hostname_dir(sandbox.path());
    let config = "machine = \"w1043f2-fixture\"\n";
    let budget: u64 = 2;
    let run = run_doctor(doctor_with_planted_config(
        sandbox.path(),
        &registry,
        config,
        &path_dir,
        false,
        budget,
    ));
    // The scan resolves identity first and the machine-log rows resolve it
    // again — the two checks that can consume their full budget here.
    assert!(
        run.elapsed < Duration::from_secs(budget * 2 + 20),
        "the wedged identity lookups bound the run; waited {:?}",
        run.elapsed
    );
    assert_eq!(
        run.code,
        Some(3),
        "the wedged scan and rows exit 3: {}",
        run.text
    );
    assert!(
        run.text.contains("doctor: checking machine log rows")
            && run
                .text
                .contains("doctor: checking D3 harness registry and scan"),
        "the traces name both identity-resolving checks before they wedge: {}",
        run.text
    );
    assert!(
        run.text.contains(&format!(
            "D3 · Coverage — 🔴 the registry-driven scan did not answer within {budget}s"
        )),
        "the scan wedged on its identity lookup, not on the registry: {}",
        run.text
    );
    assert!(
        run.text.contains(&format!(
            "  machine logs (per-machine input logs, ADR-053 — not sessions, never counted \
             in the table above) — 🔴 UNKNOWN: the machine log rows check did not answer \
             within {budget}s, so the machine-log rows are unknown, not empty."
        )),
        "a wedged machine-log rows check keeps its sub-block named in text, the same \
         UNKNOWN story `--json`'s machine_logs object tells: {run}",
        run = run.text,
    );
}
