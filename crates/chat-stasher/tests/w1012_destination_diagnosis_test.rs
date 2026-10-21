//! W1012 / STAGE-GROWTH §5.3 SG-1: the destination reconciliation, end to
//! end against the real binary.
//!
//! The machine this change was written for (46-STAGE-GROWTH §2) declared two
//! destinations, installed an hourly timer that named only one of them, put
//! the other's credentials in a shell variable the timer could never see,
//! and left the weekly stage clean-up blocking on it for weeks — with the
//! only trace in a log nobody read. Everything below replays that machine
//! inside a sandbox: the timer files the renderer itself writes, the state
//! records the commands themselves produce, and `status` / `reclaim-stage`
//! reporting what the weekly log used to be the only address of.
//!
//! Same rules as `b84_runstate_test`: the real binary, every ambient path
//! redirected into a sandbox, no network (nothing here dials a destination —
//! `status` is offline by construction and the reclaim's consult fails on a
//! missing masterkey before the backend is ever opened), no session
//! content beyond synthetic JSONL lines.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

const SHELL_ONLY_R2_SENTENCE: &str =
    "[dest] r2: credentials are shell-only (CHAT_STASHER_R2_ACCESS_KEY_ID)";

/// The status exit code for "the timer has never run" — the reconciliation
/// lines must not move it (docs-dev/install.md's contract for the integer).
const NEVER_RAN: i32 = 1;

/// One sandboxed machine, with the ambient environment `run` points at.
struct Machine {
    dir: tempfile::TempDir,
}

impl Machine {
    fn new() -> Machine {
        Machine {
            dir: tempfile::tempdir().expect("sandbox"),
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn home(&self) -> PathBuf {
        self.root().join("home")
    }

    /// The sandboxed config file `config_path()` resolves to.
    fn write_config(&self, body: &str) {
        let dir = self.root().join("config").join("chat-stasher");
        fs::create_dir_all(&dir).expect("config dir");
        fs::write(dir.join("config.toml"), body).expect("config");
    }

    /// Write the two timer files this machine's bad shape has, using the
    /// same renderer the product installs with: one hourly `run-once` for
    /// `storagebox`, one weekly `reclaim-stage`.
    fn plant_timer_files(&self) {
        let dir = install_agents_dir(&self.home());
        fs::create_dir_all(&dir).expect("LaunchAgents dir");
        let stage = self.root().join("stage");
        fs::create_dir_all(&stage).expect("stage dir");
        let binary = self.home().join("bin/chat-stasher");
        let files = chat_stasher::schedule::render(
            chat_stasher::schedule::Unit::RunOnce,
            format_of_this_machine(),
            &binary,
            &stage,
            3600,
            &chat_stasher::schedule::RunOnceArgs {
                destination: Some("storagebox".to_string()),
                ..Default::default()
            },
            &chat_stasher::schedule::ReclaimStageArgs::default(),
            &self.home(),
        );
        for file in &files {
            fs::write(dir.join(&file.name), &file.content).expect("plist");
        }
        let reclaim = chat_stasher::schedule::render(
            chat_stasher::schedule::Unit::ReclaimStage,
            format_of_this_machine(),
            &binary,
            &stage,
            0,
            &chat_stasher::schedule::RunOnceArgs::default(),
            &chat_stasher::schedule::ReclaimStageArgs::default(),
            &self.home(),
        );
        for file in &reclaim {
            fs::write(dir.join(&file.name), &file.content).expect("plist");
        }
    }

    /// Where the state records land under this sandbox.
    fn state_dir(&self) -> PathBuf {
        self.root().join("data").join("chat-stasher").join("state")
    }

    /// Run the real binary, with every path the product resolves pointed
    /// into this sandbox. `arguments` follows the subcommand.
    fn run(&self, arguments: &[&str]) -> std::process::Output {
        let home = self.home();
        fs::create_dir_all(&home).expect("home dir");
        let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        command
            .args(arguments)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("XDG_CONFIG_HOME", self.root().join("config"))
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("XDG_STATE_HOME", self.root().join("state"))
            .env("XDG_CACHE_HOME", self.root().join("cache"))
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(self.root()),
            )
            .env_remove("CHAT_STASHER_REGISTRY")
            .env_remove("CHAT_STASHER_R2_ACCESS_KEY_ID")
            .env_remove("CODEX_HOME")
            .env_remove("GEMINI_CLI_HOME")
            .env_remove("OPENCODE_DB")
            .env_remove("CURSOR_USER_DIR");
        command.output().expect("run chat-stasher")
    }
}

fn format_of_this_machine() -> chat_stasher::schedule::Format {
    if cfg!(target_os = "macos") {
        chat_stasher::schedule::Format::Launchd
    } else {
        chat_stasher::schedule::Format::Systemd
    }
}

/// The scheduler's install directory *inside the sandbox* — the launchd
/// directory on macOS, the systemd directory otherwise — always under the
/// sandbox home, because the observation code resolves both formats'
/// directories from the same home the product installs into
/// (`Library/LaunchAgents` under launchd, `.config/systemd/user` under
/// systemd). The renderer's format and this directory must agree for the
/// planted files to be found, which is the same coupling the real
/// install has.
fn install_agents_dir(home: &Path) -> PathBuf {
    match format_of_this_machine() {
        chat_stasher::schedule::Format::Launchd => home.join("Library/LaunchAgents"),
        chat_stasher::schedule::Format::Systemd => home.join(".config/systemd/user"),
    }
}

/// The config text of the machine's bad shape: two destinations, one whose
/// credentials are written as the shell-only `env:` form. Both `repo`
/// paths are sandbox-local so that not one test dials a network endpoint,
/// and their backslashes are escaped the way `config_explicit_root_test`
/// escapes its root: the reconciliation's raw `env:` scan strict-parses
/// this same file, so on Windows an unescaped `C:\Users\...` fails that
/// parse and quietly turns the shell-only destination into an ordinary
/// one.
fn bad_shape_config(repo_one: &Path, repo_two: &Path) -> String {
    format!(
        "[destinations.storagebox]\n\
         repo = \"{repo_one}\"\n\
         \n\
         [destinations.r2]\n\
         repo = \"{repo_two}\"\n\
         options = {{ access_key_id = \"env:CHAT_STASHER_R2_ACCESS_KEY_ID\" }}\n",
        repo_one = repo_one.display().to_string().replace('\\', "\\\\"),
        repo_two = repo_two.display().to_string().replace('\\', "\\\\")
    )
}

fn combined(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// `status` reports the machine's four facts in prose: the destination no
/// timer pushes, the credentials a timer cannot resolve, and the recorded
/// reclaim blocked twice in a row — the sentences `doctor` shows and
/// `status --json` hands the menu bar app. The covered destination stays
/// silent, and the exit code keeps its documented meaning ("is the
/// scheduled run healthy"), which this machine's never-run timer answers
/// with its usual non-zero.
#[test]
fn status_names_the_destination_the_timers_do_not_push() {
    let machine = Machine::new();
    machine.plant_timer_files();
    machine.write_config(&bad_shape_config(
        &machine.root().join("repo-one"),
        &machine.root().join("repo-two"),
    ));

    // The reclaim record the weekly job would have left behind by the
    // second blocked week, written through the module itself so the file's
    // shape is the product's, not the test's.
    let state_dir = machine.state_dir();
    fs::create_dir_all(&state_dir).expect("state dir");
    let blocked_attempt = |finished_at: u64| chat_stasher::reclaimstate::ReclaimAttempt {
        version: chat_stasher::reclaimstate::RECLAIM_STATE_VERSION,
        finished_at_unix: finished_at,
        apply: true,
        outcome: chat_stasher::reclaimstate::ReclaimOutcome::Blocked,
        blocked: vec![chat_stasher::reclaimstate::ReclaimBlock {
            destination: "r2".to_string(),
            kind: "unreachable".to_string(),
            error_kind: Some("credentials_missing_or_invalid".to_string()),
            sessions: 9_869,
            bytes: 12_900_000_000,
        }],
        consecutive_blocked: 0,
        candidates: Some(9_869),
        candidate_bytes: Some(12_900_000_000),
    };
    chat_stasher::reclaimstate::record(&state_dir, &blocked_attempt(1_750_000_000))
        .expect("record the first blocked week");
    chat_stasher::reclaimstate::record(&state_dir, &blocked_attempt(1_750_100_000))
        .expect("record the second blocked week");

    let output = machine.run(&["status"]);
    let text = combined(&output);
    assert!(
        text.contains("[dest] r2: declared, but no installed timer pushes it"),
        "the uncovered destination must be named: {text}"
    );
    assert!(
        !text.contains("[dest] storagebox: declared, but no installed timer pushes it"),
        "the covered destination stays silent: {text}"
    );
    assert!(
        text.contains(SHELL_ONLY_R2_SENTENCE),
        "the shell-only credentials sentence, in the one wording all three surfaces share: {text}"
    );
    assert!(
        text.contains("[reclaim] the weekly stage clean-up is blocked 2 times in a row"),
        "the recorded streak is reported, not just the log's: {text}"
    );
    assert!(
        text.contains("[reclaim] blocked by `r2`"),
        "the per-destination share of the block is named: {text}"
    );
    assert_eq!(
        output.status.code(),
        Some(NEVER_RAN),
        "the reconciliation reports without moving the timer-verdict exit code"
    );
}

/// `status --json` carries the same facts at the `local` layer the menu bar
/// app reads: per-destination fact kinds, the alert sentences with their
/// severities, and the reclaim record the C2 threshold counts from.
#[test]
fn status_json_carries_the_reconciliation_and_the_reclaim_record() {
    let machine = Machine::new();
    machine.plant_timer_files();
    machine.write_config(&bad_shape_config(
        &machine.root().join("repo-one"),
        &machine.root().join("repo-two"),
    ));
    let state_dir = machine.state_dir();
    // A push record for the covered destination only. The uncovered one
    // gets a no-record fact, worded as absence rather than as never.
    fs::create_dir_all(&state_dir).expect("state dir");
    chat_stasher::pushstate::record_success(&state_dir, "storagebox", "m-unit", 42, 1_750_000_500)
        .expect("record storagebox's push");

    let output = machine.run(&["status", "--json"]);
    let line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let value: serde_json::Value = serde_json::from_str(&line)
        .unwrap_or_else(|e| panic!("status --json writes one JSON object: {e}\n{line}"));
    let local = &value["local"];
    let destinations = local["destinations"]
        .as_array()
        .expect("local.destinations is the reconciliation's fact rows");
    let r2 = destinations
        .iter()
        .find(|row| row["name"] == "r2")
        .expect("one row per declared destination");
    assert_eq!(r2["push_coverage"]["kind"], "no_scheduled_push");
    assert_eq!(
        r2["credentials"]["kind"], "not_resolvable_in_timer",
        "r2's fact 2 says the timer cannot resolve it: {r2}"
    );
    assert_eq!(r2["last_push"]["kind"], "no_record");
    assert_eq!(
        r2["reclaim_share"]["kind"], "unknown_run",
        "with no reclaim record the share is unknown in shape too, not zero: {r2}"
    );
    let storagebox = destinations
        .iter()
        .find(|row| row["name"] == "storagebox")
        .expect("the covered destination has a row too");
    assert_eq!(storagebox["push_coverage"]["kind"], "covered");
    assert_eq!(storagebox["credentials"]["kind"], "shell_independent");
    assert_eq!(storagebox["last_push"]["kind"], "known");
    assert_eq!(storagebox["last_push"]["snapshots_in_repo"], 42);

    let alerts = local["dest_alerts"]
        .as_array()
        .expect("local.dest_alerts carries the pre-worded sentences");
    assert!(
        alerts.iter().any(|alert| alert["severity"] == "yellow"
            && alert["sentence"]
                .as_str()
                .is_some_and(|s| s.contains("[dest] r2: declared, but no installed timer"))),
        "the push-coverage alert with its severity: {alerts:?}"
    );
    assert_eq!(
        local["reclaim"]["kind"], "missing",
        "no reclaim record exists on this machine, and the record says so in shape"
    );
    assert_eq!(
        local["reclaim"]["unit_installed"], true,
        "the planted timer file is reported present"
    );
}

/// A quiet machine stays byte-quiet: no destinations declared, no timers,
/// no state — `status`'s body carries no `[dest]` / `[reclaim]` lines and
/// the exit code keeps answering the timer question it always answered.
/// This is the regression half of "silence is what a healthy machine
/// keeps": the reconciliation is a new sound, not a new hum.
#[test]
fn a_machine_with_no_reconciliation_to_report_prints_no_new_lines() {
    let machine = Machine::new();
    machine.write_config("");

    let output = machine.run(&["status"]);
    let text = combined(&output);
    assert!(!text.contains("[dest]"), "no destination lines: {text}");
    assert!(!text.contains("[reclaim]"), "no reclaim lines: {text}");
}

/// `reclaim-stage` writes the durable record itself, whatever the outcome:
/// a blocked consult blocks the streak counter, and the second blocked run
/// is the consecutive count the menubar's notification threshold reads.
/// The destinations are sandbox-local paths with no masterkeys, so the
/// consult fails on the key file — the same "unreachable" class the weekly
/// job records, without any network in the test.
#[test]
fn reclaim_stage_records_the_block_and_counts_the_streak() {
    let machine = Machine::new();
    let stage = machine.root().join("stage");
    fs::create_dir_all(
        stage
            .join("sessions/m-unit/s-one")
            .parent()
            .expect("session dir"),
    )
    .expect("session dir");
    for seq in 1..=3 {
        let lines: Vec<String> = (0..4)
            .map(|i| format!("{{\"seq\":{seq},\"i\":{i}}}"))
            .collect();
        chat_stasher::store::write_sealed_shard(
            chat_stasher::store::StageWriter::Collect,
            &stage,
            "m-unit",
            "s-one",
            &lines,
        )
        .expect("write synthetic shard");
    }
    machine.write_config(&bad_shape_config(
        &machine.root().join("repo-one"),
        &machine.root().join("repo-two"),
    ));
    machine.plant_timer_files();

    for _ in 0..2 {
        let output = machine.run(&[
            "reclaim-stage",
            "--stage",
            stage.to_str().expect("utf-8 stage path"),
        ]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "a blocked reclaim exits 1: {}",
            combined(&output)
        );
    }

    let state = machine.state_dir().join("reclaim-state.json");
    let body = fs::read_to_string(&state).expect("the reclaim wrote its record");
    let value: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("the record is the product's own shape: {e}\n{body}"));
    assert_eq!(value["outcome"], "blocked");
    assert_eq!(
        value["consecutive_blocked"], 2,
        "two blocked runs in a row is the number the notification threshold counts"
    );
    let blocked = value["blocked"]
        .as_array()
        .expect("the blocked destinations are recorded");
    assert_eq!(blocked.len(), 2, "both declared destinations blocked");
    assert!(blocked.iter().all(|row| row["kind"] == "unreachable"));
    // The candidates were surveyed (the dry run had real work to prove)
    // and both destinations held every byte back.
    assert_eq!(value["candidates"], 1);

    // The dry-run output names the class of failure in its "blocked" line
    // for both destinations, which is what the machine's weekly log would
    // carry from here on — and the streak line that used to live nowhere
    // but the log is now also the durable record just asserted.
    // One more blocked run from the CLI would make it three; the record
    // is the counter, the run just increments it.
    let third = machine.run(&[
        "reclaim-stage",
        "--stage",
        stage.to_str().expect("utf-8 stage path"),
    ]);
    assert_eq!(third.status.code(), Some(1));
    let body = fs::read_to_string(&state).expect("the reclaim rewrote its record");
    let value: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("rewrite: {e}\n{body}"));
    assert_eq!(value["consecutive_blocked"], 3);
}
