//! W1023 / STAGE-GROWTH §6 SG-2 (ADR-060): the health-gated reclaim, end to
//! end against the real binary.
//!
//! ADR-060's D8 fixture set, black-boxed the way
//! `w1012_destination_diagnosis_test` black-boxed SG-1: synthetic JSONL
//! bodies in a throwaway stage, local rustic repositories the test itself
//! pushed, state records written through the product's own modules —
//! nothing dials a network endpoint, no credential exists, no conversation
//! text beyond synthetic lines.
//!
//! The fixtures this file owns (the brief's three mandatory ones, plus the
//! §4.7 base shape through the real CLI):
//!   * A1 — a 35-day per-destination override keeps a 20-day-old push
//!     **healthy and consulted** (able to block when it genuinely misses a
//!     session; no degraded alarm in sight), exactly as the review's mandatory edit (A1) words it.
//!   * §4.7 — one never-pushed destination + one healthy destination that
//!     holds every candidate: the dry run proceeds, exit 0, the never-pushed
//!     one is listed degraded with the two roads, and the durable record's
//!     health snapshot says exactly what the report said.
//!   * unknown version ⇒ unreadable ⇒ **must-prove and alarmed**: a
//!     `push-state.json` written by a newer build refuses to classify, the
//!     destinations stay in the proof, and a genuine consult failure blocks
//!     with the W1012g wording co-present.
//!   * D5 — `[reclaim] stage_cap_bytes`: a stage over the cap alarms (in
//!     `reclaim-stage`'s own output and on the surfaces) and changes nothing
//!     about the run's outcome: alarm only, never a block. Plus the unknown-
//!     version arm of the reclaim record itself (D7's W1012g policy applied
//!     forward).

use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

/// One sandboxed machine: the config, home, state stage and repositories
/// all live under one temp root, and every binary run is pointed at them.
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

    fn stage(&self) -> PathBuf {
        self.root().join("stage")
    }

    fn state_dir(&self) -> PathBuf {
        self.root().join("data").join("chat-stasher").join("state")
    }

    /// Plant one weekly `reclaim-stage` unit file with the same renderer the
    /// product installs with, so the machine the fixtures build has the
    /// timer whose presence the job-level alerts (the unreadable-record
    /// alarm among them) are gated on.
    fn plant_reclaim_timer(&self) {
        let format = if cfg!(target_os = "macos") {
            chat_stasher::schedule::Format::Launchd
        } else {
            chat_stasher::schedule::Format::Systemd
        };
        let dir = match format {
            chat_stasher::schedule::Format::Launchd => self.home().join("Library/LaunchAgents"),
            chat_stasher::schedule::Format::Systemd => self.home().join(".config/systemd/user"),
        };
        fs::create_dir_all(&dir).expect("timer dir");
        let binary = self.home().join("bin/chat-stasher");
        let files = chat_stasher::schedule::render(
            chat_stasher::schedule::Unit::ReclaimStage,
            format,
            &binary,
            &self.stage(),
            0,
            &chat_stasher::schedule::RunOnceArgs::default(),
            &chat_stasher::schedule::ReclaimStageArgs::default(),
            &self.home(),
        );
        for file in &files {
            fs::write(dir.join(&file.name), &file.content).expect("timer file");
        }
    }

    fn write_config(&self, text: &str) {
        let dir = self.root().join("config").join("chat-stasher");
        fs::create_dir_all(&dir).expect("config dir");
        fs::write(dir.join("config.toml"), text).expect("config");
    }

    /// Every run of the real binary, with the ambient environment pinned.
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
            .env_remove("CODEX_HOME");
        command.output().expect("run chat-stasher")
    }
}

/// A destination's store config pointing at a sandbox-local repository.
/// `no_cache` on, cache pinned: a shard a test corrupts must never come back
/// from a cache as a clean read.
fn store_config(dir: &Path, repo_name: &str) -> StoreConfig {
    StoreConfig {
        repo_root: dir.join(repo_name).to_string_lossy().into_owned(),
        key_file: dir.join(format!("{repo_name}.key")),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: None,
        no_cache: true,
    }
}

/// Create a repository with its key, then push `stage` into it — the exact
/// artefacts `cmd_push` would have left behind.
fn init_and_push(cfg: &StoreConfig, machine: &str, stage: &Path) {
    let mk = MasterKey::new();
    store::persist_key_file(cfg, &mk).unwrap();
    BackupStore::new(cfg.clone(), machine.to_string())
        .push(stage, &mk)
        .unwrap();
}

/// Create a repository that exists, is reachable, and holds nothing —
/// the "declared, dest-init'd, never pushed" shape.
fn init_empty_repo(cfg: &StoreConfig) {
    let mk = MasterKey::new();
    store::persist_key_file(cfg, &mk).unwrap();
    BackupStore::new(cfg.clone(), "unused".to_string())
        .open_or_init(&mk)
        .unwrap();
}

/// Write `nshards` sealed shards for one session into `stage`.
fn write_session(stage: &Path, machine: &str, session: &str, nshards: u64) {
    for seq in 1..=nshards {
        let lines: Vec<String> = (0..10)
            .map(|i| format!("{{\"seq\":{seq},\"i\":{i},\"s\":\"synthetic-{session}-{seq}-{i}\"}}"))
            .collect();
        store::write_sealed_shard(store::StageWriter::Collect, stage, machine, session, &lines)
            .unwrap();
    }
}

/// The config text a machine with one or two declared destinations writes,
/// plus optional `[reclaim]` / `[native_host]` blocks. Each destination
/// declares its key file at `<root>/<name>.key` so the CLI's store
/// resolution and the test's `store_config` agree on exactly one masterkey
/// path. Paths are escaped so a Windows sandbox root parses too.
fn destinations_config(
    root: &Path,
    first: &str,
    first_extra: &str,
    second: Option<&str>,
    reclaim: &str,
    native_host_stage: Option<&Path>,
) -> String {
    let escape = |p: &Path| p.display().to_string().replace('\\', "\\\\");
    let destination = |name: &str, extra: &str| {
        format!(
            "[destinations.{name}]\nrepo = \"{}\"\nkey_file = \"{}\"\n{extra}\n",
            escape(&root.join(name)),
            escape(&root.join(format!("{name}.key"))),
        )
    };
    let mut text = destination(first, first_extra);
    if let Some(second) = second {
        text.push('\n');
        text.push_str(&destination(second, ""));
    }
    if !reclaim.is_empty() {
        text.push_str(&format!("\n{reclaim}\n"));
    }
    if let Some(stage) = native_host_stage {
        text.push_str(&format!("\n[native_host]\nstage = \"{}\"\n", escape(stage)));
    }
    text
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn combined(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        stdout_of(output),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// **A1 (the review's mandatory edit)** — a destination that declares `degrade_after_days = 35`
/// and whose last successful push is 20 days old is **healthy**: consulted
/// by the proof, with no degraded alarm anywhere. The same 20 days on the
/// global window would lag a destination into a permanent alarm — the
/// exact case the override exists for (a hand-pushed external disk on its
/// own monthly rhythm).
#[test]
fn a_thirty_five_day_override_keeps_a_twenty_day_push_healthy_and_consulted() {
    let machine = Machine::new();
    let machine_name = "m-a1";
    let stage = machine.stage();
    write_session(&stage, machine_name, "s-a1", 2);

    // `monthly-disk` holds every candidate and was pushed 20 days ago
    // (healthy through its 35-day override); `hourly-box` exists, was
    // pushed an hour ago, and also holds every candidate.
    let monthly = store_config(machine.root(), "monthly-disk");
    init_and_push(&monthly, machine_name, &stage);
    let hourly = store_config(machine.root(), "hourly-box");
    init_and_push(&hourly, machine_name, &stage);

    machine.write_config(&destinations_config(
        machine.root(),
        "monthly-disk",
        "degrade_after_days = 35",
        Some("hourly-box"),
        "[reclaim]\ndegrade_after_days = 7",
        None,
    ));
    let state_dir = machine.state_dir();
    fs::create_dir_all(&state_dir).unwrap();
    let now = now_unix();
    chat_stasher::pushstate::record_success(
        &state_dir,
        "monthly-disk",
        machine_name,
        3,
        now - 20 * 86_400,
    )
    .unwrap();
    chat_stasher::pushstate::record_success(&state_dir, "hourly-box", machine_name, 3, now - 3_600)
        .unwrap();

    // Dry run: both destinations are healthy and consulted — the override
    // one on its declared rhythm — and the run proceeds.
    let output = machine.run(&["reclaim-stage", "--stage", stage.to_str().unwrap()]);
    let stdout = stdout_of(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "the healthy set proves; the dry run proceeds: {stdout}"
    );
    assert!(
        stdout.contains("health: monthly-disk — healthy"),
        "the override destination is classified healthy, its 20-day push named: {stdout}"
    );
    assert!(
        stdout.contains("role: must-prove"),
        "a healthy destination on its own rhythm is a prover: {stdout}"
    );
    assert!(
        !stdout.contains("monthly-disk — degraded"),
        "no degraded word for a destination inside its declared rhythm: {stdout}"
    );
    assert!(
        stdout.contains("reclaimable"),
        "the run reports what it would reclaim: {stdout}"
    );

    // The durable record's snapshot says the same thing the report said.
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(state_dir.join("reclaim-state.json")).unwrap())
            .unwrap();
    let snapshot = record["destination_health"]
        .as_array()
        .expect("the health snapshot is recorded");
    let monthly_row = snapshot
        .iter()
        .find(|row| row["destination"] == "monthly-disk")
        .unwrap();
    assert_eq!(monthly_row["health"], "healthy");
    assert_eq!(monthly_row["role"], "must-prove");
}

/// **A1's "able to block" half** — the same 35-day-override destination,
/// 20-day-old record, consulted: when it genuinely misses a session the
/// refusal is its consult's own answer (not-held), never a degradation —
/// the override never removes a destination's protection, it only moves
/// "how behind is behind".
#[test]
fn an_override_destination_that_misses_a_candidate_blocks() {
    let machine = Machine::new();
    let machine_name = "m-a1b";
    let stage = machine.stage();
    write_session(&stage, machine_name, "s-missing", 1);
    write_session(&stage, machine_name, "s-present", 1);

    // The era `monthly-disk` received: a stage holding only `s-present` —
    // the out-of-sync shape a real destination produces when a push window
    // was missed on the way to the monthly hand-push.
    let present_only = machine.root().join("stage-present");
    write_session(&present_only, machine_name, "s-present", 1);
    let monthly = store_config(machine.root(), "monthly-disk");
    init_and_push(&monthly, machine_name, &present_only);
    let hourly = store_config(machine.root(), "hourly-box");
    init_and_push(&hourly, machine_name, &stage);

    machine.write_config(&destinations_config(
        machine.root(),
        "monthly-disk",
        "degrade_after_days = 35",
        Some("hourly-box"),
        "[reclaim]\ndegrade_after_days = 7",
        None,
    ));
    let state_dir = machine.state_dir();
    fs::create_dir_all(&state_dir).unwrap();
    let now = now_unix();
    chat_stasher::pushstate::record_success(
        &state_dir,
        "monthly-disk",
        machine_name,
        3,
        now - 20 * 86_400,
    )
    .unwrap();
    chat_stasher::pushstate::record_success(&state_dir, "hourly-box", machine_name, 3, now - 3_600)
        .unwrap();

    let output = machine.run(&["reclaim-stage", "--stage", stage.to_str().unwrap()]);
    let stdout = stdout_of(&output);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a healthy, consulted destination that misses a session refuses: {stdout}"
    );
    assert!(
        stdout.contains("monthly-disk — healthy"),
        "still healthy, still consulted: {stdout}"
    );
    assert!(
        stdout.contains("does not hold"),
        "the refusal is its own consult answer: {stdout}"
    );
    assert!(
        stdout.contains("nothing was deleted"),
        "the blocked shape keeps its nothing-was-deleted wording: {stdout}"
    );
    assert!(
        !stdout.contains("monthly-disk — degraded"),
        "a destination inside its rhythm is never degraded by a missing session: {stdout}"
    );
}

/// **§4.7's fixture through the real CLI** — one never-pushed destination
/// (declared, reachable, holds nothing yet) + one healthy destination
/// holding every candidate: the dry run proceeds (exit 0), the never-pushed
/// one is listed degraded with both roads, in the absence wording — never
/// "never pushed" in prose — and the durable record keeps the row visible.
#[test]
fn a_never_pushed_destination_does_not_hold_the_reclaim_hostage() {
    let machine = Machine::new();
    let machine_name = "m-47";
    let stage = machine.stage();
    write_session(&stage, machine_name, "s-47", 2);

    let good = store_config(machine.root(), "good-box");
    init_and_push(&good, machine_name, &stage);
    // `r2`: exists, reachable, never pushed — the §2 machine's bad shape,
    // with credentials that need no shell so the only thing wrong with it
    // is the push record's absence.
    let r2 = store_config(machine.root(), "r2");
    init_empty_repo(&r2);

    machine.write_config(&destinations_config(
        machine.root(),
        "good-box",
        "",
        Some("r2"),
        "[reclaim]\ndegrade_after_days = 7",
        None,
    ));
    let state_dir = machine.state_dir();
    fs::create_dir_all(&state_dir).unwrap();
    chat_stasher::pushstate::record_success(
        &state_dir,
        "good-box",
        machine_name,
        3,
        now_unix() - 3_600,
    )
    .unwrap();

    let output = machine.run(&["reclaim-stage", "--stage", stage.to_str().unwrap()]);
    let stdout = stdout_of(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "the never-pushed destination is degraded, not consulted — the reclaim proceeds: {stdout}"
    );
    assert!(
        stdout.contains("health: r2 — degraded; no push on record; not consulted this run"),
        "the degraded row is listed with the absence wording, never 'never pushed': {stdout}"
    );
    assert!(
        stdout.contains("two roads"),
        "A2: the two roads print under the degraded destination: {stdout}"
    );
    assert!(
        stdout.contains("or remove `[destinations.r2]` from the config"),
        "road two names the removal: {stdout}"
    );
    assert!(
        stdout.contains("health: good-box — healthy"),
        "the healthy destination's row says its role: {stdout}"
    );
    assert!(
        stdout.contains("RESULT       : OK (dry run)"),
        "the run's own verdict is untouched by the degradation: {stdout}"
    );

    // The durable record: outcome ok, and the health snapshot keeps the
    // never-pushed row visible for every surface that reads the file.
    let record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(state_dir.join("reclaim-state.json")).unwrap())
            .unwrap();
    assert_eq!(record["outcome"], "ok");
    let snapshot = record["destination_health"]
        .as_array()
        .expect("health snapshot recorded");
    let r2_row = snapshot
        .iter()
        .find(|row| row["destination"] == "r2")
        .unwrap();
    assert_eq!(r2_row["health"], "never-pushed", "the slug is D2's own");
    assert_eq!(r2_row["role"], "degraded-not-consulted");

    // And `status --json` reads the same facts back from the record: the
    // destination layer's reclaim_share for r2 is D7's third arm, and the
    // health the run recorded is the health the surface reports.
    let output = machine.run(&["status", "--json"]);
    let value: serde_json::Value = serde_json::from_str(stdout_of(&output).lines().next().unwrap())
        .expect("status --json writes one JSON object");
    let rows = value["local"]["destinations"].as_array().unwrap();
    let r2_row = rows.iter().find(|row| row["name"] == "r2").unwrap().clone();
    assert_eq!(r2_row["reclaim_share"]["kind"], "degraded-not-consulted");
    assert_eq!(
        r2_row["reclaim_share"]["health"], "never-pushed",
        "the share carries the health the run recorded"
    );
    assert_eq!(r2_row["health"]["kind"], "never-pushed");
    assert!(r2_row["health"]["degrade_after_days"].as_u64().is_some());
    // The healthy destination holds everything, so its share is quiet.
    let good_row = rows
        .iter()
        .find(|row| row["name"] == "good-box")
        .unwrap()
        .clone();
    assert_eq!(good_row["health"]["kind"], "healthy");
    assert_eq!(good_row["reclaim_share"]["kind"], "not_blocking");

    let status_human = machine.run(&["status"]);
    let status_text = combined(&status_human);
    assert!(
        status_text.contains("[dest] r2: degraded — no push to `r2` is on record"),
        "truly never-pushed destination keeps original no push on record wording in status: {status_text}"
    );
    assert!(
        !status_text.contains("升级后还没有推送记录，下一次定时推送后恢复"), // t5-allow:
        "truly never-pushed destination does not use upgrade window wording: {status_text}"
    );
}

/// Upgrade window: destination has snapshots in repository (pushed by old binary),
/// but no push-state record yet.
/// Both status and reclaim-stage must say the upgrade window wording,
/// while --json keeps "never-pushed", and truly never-pushed continues to say "no push on record".
#[test]
fn upgrade_window_destination_wording_in_status_and_reclaim_stage() {
    let machine = Machine::new();
    let machine_name = "m-upgrade";
    let stage = machine.stage();
    write_session(&stage, machine_name, "s-upg", 2);

    // `storagebox`: has snapshots in repo (pushed by old binary that did not write push-state)
    let storagebox = store_config(machine.root(), "storagebox");
    init_and_push(&storagebox, machine_name, &stage);

    // `r2`: truly never pushed (empty repo, no snapshots)
    let r2 = store_config(machine.root(), "r2");
    init_empty_repo(&r2);

    machine.write_config(&destinations_config(
        machine.root(),
        "storagebox",
        "",
        Some("r2"),
        "[reclaim]\ndegrade_after_days = 7",
        None,
    ));

    // state_dir has NO push-state record for storagebox or r2
    let state_dir = machine.state_dir();
    fs::create_dir_all(&state_dir).unwrap();

    // 1. reclaim-stage test
    let output = machine.run(&["reclaim-stage", "--stage", stage.to_str().unwrap()]);
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("health: storagebox — degraded; 升级后还没有推送记录，下一次定时推送后恢复; not consulted this run"), // t5-allow:
        "upgrade window destination says upgrade wording in reclaim-stage: {stdout}"
    );
    assert!(
        stdout.contains("health: r2 — degraded; no push on record; not consulted this run"),
        "truly never pushed destination keeps no push on record in reclaim-stage: {stdout}"
    );

    // 2. status human-readable test
    let status_human = machine.run(&["status"]);
    let status_text = combined(&status_human);
    assert!(
        status_text
            .contains("[dest] storagebox: degraded — 升级后还没有推送记录，下一次定时推送后恢复"), // t5-allow:
        "upgrade window destination says upgrade wording in status: {status_text}"
    );
    assert!(
        status_text.contains("[dest] r2: degraded — no push to `r2` is on record"),
        "truly never pushed destination keeps original wording in status: {status_text}"
    );
    assert!(
        !status_text.contains("[dest] r2: degraded — 升级后还没有推送记录，下一次定时推送后恢复"), // t5-allow:
        "r2 does not get upgrade window wording: {status_text}"
    );

    // 3. status --json keeps never-pushed
    let output_json = machine.run(&["status", "--json"]);
    let value: serde_json::Value =
        serde_json::from_str(stdout_of(&output_json).lines().next().unwrap()).unwrap();
    let rows = value["local"]["destinations"].as_array().unwrap();
    let storagebox_row = rows.iter().find(|row| row["name"] == "storagebox").unwrap();
    assert_eq!(
        storagebox_row["health"]["kind"], "never-pushed",
        "--json slug is preserved"
    );
    let r2_row = rows.iter().find(|row| row["name"] == "r2").unwrap();
    assert_eq!(
        r2_row["health"]["kind"], "never-pushed",
        "--json slug is preserved"
    );
}

/// **Unknown version ⇒ unreadable ⇒ must-prove + alarm** (the brief's
/// unknown-state fixture; D2's fail-safe, the W1012g policy applied
/// forward): a `push-state.json` written by a newer build classifies
/// nothing — the destinations stay in the proof, and a genuine consult
/// failure blocks with the unreadable-record wording co-present. Never
/// permission to skip.
#[test]
fn an_unknown_version_push_record_keeps_the_destinations_in_the_proof() {
    let machine = Machine::new();
    let machine_name = "m-unknown";
    let stage = machine.stage();
    write_session(&stage, machine_name, "s-unknown", 1);

    let box_one = store_config(machine.root(), "box-one");
    init_and_push(&box_one, machine_name, &stage);
    // box-two was pushed a different era (`s-other` only) — a real,
    // measured not-held once its consult runs.
    let box_two_stage = machine.root().join("stage-two");
    write_session(&box_two_stage, machine_name, "s-other", 1);
    let box_two = store_config(machine.root(), "box-two");
    init_and_push(&box_two, machine_name, &box_two_stage);

    machine.write_config(&destinations_config(
        machine.root(),
        "box-one",
        "",
        Some("box-two"),
        "",
        None,
    ));
    // The record a future build wrote: unknown version, unreadable to this
    // one — never a healthy classification, never permission to skip.
    let state_dir = machine.state_dir();
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
        chat_stasher::pushstate::push_state_path(&state_dir),
        r#"{ "version": 99, "pushes": {} }"#,
    )
    .unwrap();

    let output = machine.run(&["reclaim-stage", "--stage", stage.to_str().unwrap()]);
    let stdout = stdout_of(&output);
    assert_eq!(
        output.status.code(),
        Some(1),
        "the consult refusal stands, record or no record: {stdout}"
    );
    assert!(
        stdout.contains(
            "health: box-one — unknown; the push-state record exists but is \
                         unreadable"
        ),
        "the destinations classify unknown, never healthy: {stdout}"
    );
    assert!(
        stdout.contains("health: box-two — unknown"),
        "an unreadable record never skips a destination: {stdout}"
    );
    assert!(
        stdout.contains("must-prove — an unreadable record may never become deletion permission"),
        "the fail-safe wording says what it is: {stdout}"
    );
    assert!(
        !stdout.contains("not consulted this run"),
        "an unknown record does not degrade anyone: {stdout}"
    );
    assert!(
        stdout.contains("does not hold"),
        "a destination in the proof still has to answer for its sessions: {stdout}"
    );
    assert!(
        stdout.contains("nothing was deleted"),
        "blocked means nothing was deleted: {stdout}"
    );

    // The co-appearing W1012g alarm on the surfaces: `status` carries the
    // unreadable-record alert with its fix.
    let output = machine.run(&["status"]);
    let text = combined(&output);
    assert!(
        text.contains("the push-state record exists but is unreadable"),
        "the W1012g alert co-appears with the reclaim's own refusal: {text}"
    );
    assert!(
        text.contains("not ok, and not never pushed"),
        "the unknown is worded as unknown: {text}"
    );
}

/// **D5 — the cap only alarms** (the brief's cap fixture): a stage over
/// `[reclaim] stage_cap_bytes` gets the warning line in `reclaim-stage`'s
/// own output and the red alert on the surfaces, and the run's outcome is
/// untouched — a healthy proof still proceeds, exit 0, alarm only, never a
/// block. `status --json` carries the measured bytes, the cap and
/// `exceeded` for the menu bar, because the config names the stage
/// (`[native_host] stage`).
#[test]
fn an_over_cap_stage_alarms_and_does_not_touch_the_outcome() {
    let machine = Machine::new();
    let machine_name = "m-cap";
    let stage = machine.stage();
    write_session(&stage, machine_name, "s-cap", 4);

    let cap_repo = store_config(machine.root(), "cap-box");
    init_and_push(&cap_repo, machine_name, &stage);

    machine.write_config(&destinations_config(
        machine.root(),
        "cap-box",
        "",
        None,
        "[reclaim]\nstage_cap_bytes = \"500B\"",
        Some(&stage),
    ));
    let state_dir = machine.state_dir();
    fs::create_dir_all(&state_dir).unwrap();
    chat_stasher::pushstate::record_success(
        &state_dir,
        "cap-box",
        machine_name,
        3,
        now_unix() - 3_600,
    )
    .unwrap();

    let output = machine.run(&["reclaim-stage", "--stage", stage.to_str().unwrap()]);
    let stdout = stdout_of(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "the cap is an alarm only — the healthy proof still proceeds: {stdout}"
    );
    assert!(
        stdout.contains("warning      : the stage holds"),
        "the over-cap warning prints on the run itself: {stdout}"
    );
    assert!(
        stdout.contains("over the configured cap")
            && stdout.contains("this is an alarm, not a block"),
        "the alarm-only promise is in the wording: {stdout}"
    );
    assert!(
        stdout.contains("RESULT       : OK (dry run)"),
        "the outcome is the proof's, not the cap's: {stdout}"
    );

    // `status --json` — the menubar's half: the fact is measured, over the
    // cap, with the red alert carrying the same wording.
    let output = machine.run(&["status", "--json"]);
    let value: serde_json::Value = serde_json::from_str(stdout_of(&output).lines().next().unwrap())
        .expect("status --json writes one JSON object");
    let stage_cap = &value["local"]["stage_cap"];
    assert_eq!(stage_cap["kind"], "measured");
    assert_eq!(
        stage_cap["cap"], 500,
        "the configured cap is in effect, not the 10 GiB default"
    );
    assert_eq!(stage_cap["exceeded"], true);
    assert!(
        stage_cap["bytes"].as_u64().unwrap() > 500,
        "the measured bytes are the sessions tree's, not meta's"
    );
    let alerts = value["local"]["dest_alerts"]
        .as_array()
        .expect("dest_alerts is an array");
    assert!(
        alerts.iter().any(|alert| alert["severity"] == "red"
            && alert["sentence"]
                .as_str()
                .is_some_and(|sentence| sentence.contains("over the configured cap"))),
        "the red cap alert is in the shared-wording surface: {alerts:?}"
    );
}

/// The unknown-version arm of the *reclaim* record (D7's W1012g policy
/// applied forward): a `reclaim-state.json` this build cannot interpret —
/// the v1 shape SG-1 wrote — alarms as unreadable, never reads as a
/// healthy streak to reset or a per-destination fact to fold.
#[test]
fn an_unknown_version_reclaim_record_alarms_and_never_reads_healthy() {
    let machine = Machine::new();
    machine.write_config(&destinations_config(
        machine.root(),
        "a-box",
        "",
        None,
        "",
        None,
    ));
    machine.plant_reclaim_timer();
    let state_dir = machine.state_dir();
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
        chat_stasher::reclaimstate::reclaim_state_path(&state_dir),
        r#"{ "version": 1, "finished_at_unix": 1, "apply": true,
            "outcome": "blocked", "blocked": [], "consecutive_blocked": 2,
            "candidates": 5, "candidate_bytes": 500 }"#,
    )
    .unwrap();

    let output = machine.run(&["status"]);
    let text = combined(&output);
    assert!(
        text.contains("the reclaim record exists but is unreadable"),
        "the v1 record alarms as unreadable: {text}"
    );
    assert!(
        text.contains("not ok, and not never-run"),
        "the unknown is worded as unknown, never folded into a verdict: {text}"
    );

    // And the `--json` half keeps its own unreadable shape — never the
    // "known"/"ok" object a healthy record would produce.
    let output = machine.run(&["status", "--json"]);
    let value: serde_json::Value =
        serde_json::from_str(stdout_of(&output).lines().next().unwrap()).unwrap();
    assert_eq!(value["local"]["reclaim"]["kind"], "unreadable");
    assert!(
        value["local"]["reclaim"]["why"]
            .as_str()
            .is_some_and(|why| why.contains("unknown version 1")),
        "the reason names the version it cannot read: {}",
        value["local"]["reclaim"]
    );
}
