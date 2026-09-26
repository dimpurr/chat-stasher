//! WIZ-2: `chat-stasher setup`, driven through the real binary against a
//! throwaway machine.
//!
//! Every fixture is synthetic and every machine is disposable: an isolated
//! HOME with its own XDG tree, a registry narrowed to one harness so the
//! assertions describe the wizard rather than whatever happens to be installed
//! where the suite runs, and a single opaque JSONL line the collector can count.
//! No assertion reads a session body, a key file's contents, or a real path —
//! counts, states, exit codes and the existence of directories only.
//!
//! The chain these tests are about is the one `_reconworker/readme-bakeoff/
//! NOTES.md` §6 recorded by hand before the wizard existed:
//! `INIT → NOOP → Healthy → search read-back`. WIZ-2's job is to make the
//! wizard reproduce and *report* it, so the tests check the report as well as
//! the artifacts: a chain link may only be called observed when the run
//! observed it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The bundled registry, narrowed to `claude-code`.
///
/// Narrowed on purpose. The full registry names paths on this machine that the
/// suite does not control — an absolute temp directory, a browser profile — and
/// a test whose session count depends on what is installed where it runs is a
/// test that will be green here and red somewhere else.
fn claude_code_only_registry() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("data")
        .join("harness-registry-v1.json");
    let text = fs::read_to_string(&path).expect("read the bundled harness registry");
    let mut registry: serde_json::Value =
        serde_json::from_str(&text).expect("the bundled registry is JSON");
    let harnesses = registry["harnesses"]
        .as_array_mut()
        .expect("the registry has a harnesses array");
    harnesses.retain(|harness| harness["id"] == "claude-code");
    assert_eq!(harnesses.len(), 1, "claude-code must be in the registry");
    serde_json::to_string(&registry).expect("re-serialise the narrowed registry")
}

/// A throwaway machine: isolated HOME, isolated XDG tree, its own registry, and
/// optionally one synthetic session for the collector to find.
struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new(with_session: bool) -> Self {
        let sandbox = Sandbox {
            root: tempfile::tempdir().expect("create a temp dir"),
        };
        for dir in ["home", "data", "config", "state"] {
            fs::create_dir_all(sandbox.root.path().join(dir)).expect("create the sandbox tree");
        }
        fs::write(
            sandbox.root.path().join("registry.json"),
            claude_code_only_registry(),
        )
        .expect("write the sandbox registry");
        if with_session {
            let dir = sandbox
                .home()
                .join(".claude")
                .join("projects")
                .join("fixture-project");
            fs::create_dir_all(&dir).expect("create the fixture harness root");
            fs::write(
                dir.join("019bf00d-97b6-7eb2-9bf8-eacbacc09765.jsonl"),
                b"{\"note\":\"WIZ-2 synthetic session line, not a conversation\"}\n",
            )
            .expect("write the synthetic session");
        }
        sandbox
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn stage(&self) -> PathBuf {
        self.home().join("stash").join("chat-stasher").join("stage")
    }

    fn data_root(&self) -> PathBuf {
        self.root.path().join("data").join("chat-stasher")
    }

    fn repository(&self) -> PathBuf {
        self.data_root().join("repo")
    }

    fn masterkey(&self) -> PathBuf {
        self.data_root().join("masterkey.json")
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("HOME", self.home())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env(
                "CHAT_STASHER_REGISTRY",
                self.root.path().join("registry.json"),
            )
            .output()
            .expect("run chat-stasher")
    }

    fn config_file(&self) -> PathBuf {
        self.root
            .path()
            .join("config")
            .join("chat-stasher")
            .join("config.toml")
    }

    /// Write a `config.toml` for this sandbox, verbatim.
    ///
    /// Used to declare a destination by hand, which is how a destination the
    /// wizard did not spell itself gets adopted: ADR-039 keeps the local-path
    /// and REST candidates explicitly selectable, and this is the test's stand-in
    /// for all of them — a real backend that is not a network.
    fn write_config(&self, text: &str) {
        let path = self.config_file();
        fs::create_dir_all(path.parent().expect("a config directory")).expect("create config dir");
        fs::write(&path, text).expect("write the sandbox config");
    }

    fn read_config(&self) -> String {
        fs::read_to_string(self.config_file()).expect("read the sandbox config")
    }

    /// A directory inside the sandbox that no destination has used yet.
    fn fake_remote(&self) -> PathBuf {
        self.root.path().join("fake-remote")
    }

    /// `setup --stage <this sandbox's stage>`, plus whatever else the case adds.
    ///
    /// The stage is passed explicitly rather than typed at a prompt: the
    /// interactive path needs a pty, which a test suite should not fake, and
    /// the state machine behind both paths is the same one.
    fn setup(&self, extra: &[&str]) -> Output {
        let stage = self.stage();
        let mut args = vec![
            "setup",
            "--stage",
            stage.to_str().expect("utf-8 stage path"),
        ];
        args.extend_from_slice(extra);
        self.command(&args)
    }
}

fn json_of(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!(
            "stdout must be exactly one JSON object ({error}); exit={:?}\nstdout={stdout}\n\
             stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// The idempotence link must be decided on the repository's own snapshot count.
///
/// This is the one link whose obvious evidence — the second pass's run-state
/// record — is not sound: on a re-run both passes are NOOP with the same
/// outcome inside the same second, so a record identical to the previous pass's
/// is a *normal* outcome. An earlier version read that as "not attributable"
/// and reported this link as `unknown` on an ordinary second run, which is how
/// the test below found it. Pinning the wording keeps the evidence source
/// visible, so a future change back to the record cannot pass quietly.
fn assert_idempotence_rests_on_the_repository(value: &serde_json::Value) {
    let why = value["chain"]["noop"]["why"]
        .as_str()
        .expect("a why string");
    assert!(
        why.contains("still holds"),
        "the idempotence link must rest on the repository's snapshot count, not on the \
         second pass's run-state record: {why}"
    );
}

fn exit_code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("the wizard must exit with a code, never a signal")
}

/// The happy path: a machine with one session, all flags given.
///
/// This is `readme-bakeoff/NOTES.md` §6 reproduced — but through the wizard, and
/// with the wizard reporting each link instead of the reader comparing four
/// commands by hand.
#[test]
fn local_first_save_creates_the_repository_and_observes_the_whole_chain() {
    let sandbox = Sandbox::new(true);
    let output = sandbox.setup(&["--masterkey-saved-elsewhere"]);
    let value = json_of(&output);

    assert_eq!(exit_code(&output), 0, "value={value}");
    assert_eq!(value["healthy"], true);
    assert_eq!(value["exit_code"], 0);
    assert_eq!(value["steps"]["local_save"], "created");
    assert_eq!(value["steps"]["masterkey"], "declared");

    assert_eq!(value["chain"]["init"]["kind"], "observed");
    assert_eq!(value["chain"]["noop"]["kind"], "observed");
    assert_eq!(value["chain"]["readback"]["kind"], "known");
    assert_eq!(value["chain"]["readback"]["sessions"], 1);
    assert_eq!(value["chain"]["readback"]["snapshots_in_repo"], 1);
    assert_idempotence_rests_on_the_repository(&value);

    // The passes themselves, not only the links derived from them: INIT is a
    // pass that created a snapshot, NOOP is the next one creating none.
    assert_eq!(value["runs"]["first"]["outcome"], "COMPLETED");
    assert_eq!(value["runs"]["first"]["snapshot_created"], true);
    assert_eq!(value["runs"]["second"]["outcome"], "NOOP");
    assert_eq!(value["runs"]["second"]["snapshot_created"], false);

    // The report has to describe files that are actually there.
    assert!(
        sandbox.repository().exists(),
        "the chain says a repository was created; {} must exist",
        sandbox.repository().display()
    );
    assert!(
        sandbox.masterkey().exists(),
        "the wizard showed a masterkey path; {} must exist",
        sandbox.masterkey().display()
    );
    assert_eq!(
        Path::new(value["masterkey"]["path"].as_str().expect("a path string")),
        sandbox.masterkey(),
        "the path the wizard tells the user to copy must be the file it wrote"
    );
}

/// Running `setup` twice must not archive twice, and the second run has to say
/// so rather than re-reporting the first run's INIT.
#[test]
fn a_second_setup_run_reports_the_repository_as_pre_existing() {
    let sandbox = Sandbox::new(true);
    let first = json_of(&sandbox.setup(&["--masterkey-saved-elsewhere"]));
    assert_eq!(first["steps"]["local_save"], "created");

    let second_output = sandbox.setup(&["--masterkey-saved-elsewhere"]);
    let second = json_of(&second_output);
    assert_eq!(exit_code(&second_output), 0, "value={second}");
    assert_eq!(second["steps"]["local_save"], "existed");
    assert_eq!(second["chain"]["init"]["kind"], "not_observed");
    assert!(
        second["chain"]["init"]["why"]
            .as_str()
            .is_some_and(|why| !why.is_empty()),
        "a link that was not observed must say why: {second}"
    );
    assert_eq!(second["chain"]["noop"]["kind"], "observed");
    assert_idempotence_rests_on_the_repository(&second);
    assert_eq!(second["chain"]["readback"]["kind"], "known");
    assert_eq!(
        second["chain"]["readback"]["sessions"], first["chain"]["readback"]["sessions"],
        "a re-run must not change what is in the archive"
    );
    assert_eq!(
        second["chain"]["readback"]["snapshots_in_repo"],
        first["chain"]["readback"]["snapshots_in_repo"],
        "a re-run must add no snapshot"
    );
}

/// A machine the collector finds nothing on gets no repository and no masterkey.
/// That is a real state, and the wizard must report it as itself — never as an
/// archive with zero sessions, and never as a failure.
#[test]
fn a_machine_with_nothing_to_archive_reports_no_repository_not_an_empty_archive() {
    let sandbox = Sandbox::new(false);
    // Deliberately *without* the declaration flag: on this machine no key is
    // created, so nothing is owed. Passing the flag would make the empty
    // `missing_parameters` below a tautology instead of a claim.
    let output = sandbox.setup(&[]);
    let value = json_of(&output);

    assert_eq!(exit_code(&output), 0, "value={value}");
    assert_eq!(value["steps"]["local_save"], "nothing_to_archive");
    // No key exists, so there is nothing to declare — a third answer, not a
    // "declined", and not a missing parameter the caller could have supplied.
    assert_eq!(value["steps"]["masterkey"], "absent");
    assert_eq!(value["masterkey"]["kind"], "absent");
    assert_eq!(value["missing_parameters"], serde_json::json!([]));

    assert_eq!(value["chain"]["init"]["kind"], "not_observed");
    assert_eq!(value["chain"]["noop"]["kind"], "not_observed");
    assert_eq!(value["chain"]["readback"]["kind"], "not_applicable");
    // The whole point of the three-state rule: no measurement happened, so the
    // object must not carry a count of zero for someone to read as one.
    assert!(
        value["chain"]["readback"].get("sessions").is_none(),
        "nothing was read back, so no session count may be present: {value}"
    );

    assert!(
        !sandbox.repository().exists(),
        "no repository may be created when there was nothing to archive"
    );
    assert!(
        !sandbox.masterkey().exists(),
        "no masterkey may be created when there was nothing to archive"
    );
}

/// The confirmation is a declaration: the wizard records it as made, records
/// that nothing verified it, and refuses to call the step finished without it.
#[test]
fn the_masterkey_declaration_is_required_and_recorded_as_unverified() {
    let declared = Sandbox::new(true);
    let output = declared.setup(&["--masterkey-saved-elsewhere"]);
    let value = json_of(&output);
    assert_eq!(exit_code(&output), 0, "value={value}");
    assert_eq!(value["masterkey"]["declaration"], "declared");
    assert_eq!(
        value["masterkey"]["declaration_is_verified"], false,
        "nothing checks the copy exists, and the object must say so"
    );

    let undeclared = Sandbox::new(true);
    let output = undeclared.setup(&[]);
    let value = json_of(&output);
    assert_eq!(exit_code(&output), 2, "value={value}");
    assert_eq!(
        value["missing_parameters"],
        serde_json::json!(["masterkey_saved_elsewhere"])
    );
    assert_eq!(value["masterkey"]["declaration"], "not_declared");
    assert_eq!(value["masterkey"]["declaration_is_verified"], false);
    // The declaration is missing, not the work: a caller that sees exit 2 here
    // must still be told that the local archive was written.
    assert_eq!(value["steps"]["local_save"], "created");
    assert!(
        undeclared.repository().exists() && undeclared.masterkey().exists(),
        "the local first save runs before the declaration is asked for, so it must have happened"
    );
}

/// WIZ-1's promise, kept and tightened: a named missing parameter is reported,
/// and now provably *nothing* is written while it is missing.
#[test]
fn setup_without_a_stage_names_the_parameter_and_writes_nothing() {
    let sandbox = Sandbox::new(true);
    let output = sandbox.command(&["setup", "--json"]);
    let value = json_of(&output);

    assert_eq!(exit_code(&output), 2, "value={value}");
    assert_eq!(value["missing_parameters"], serde_json::json!(["stage"]));
    assert_eq!(value["steps"]["stage"], "missing");
    assert!(
        !sandbox.data_root().exists() && !sandbox.stage().exists(),
        "a wizard that has not been told where the archive goes must not create one"
    );
}

#[cfg(unix)]
#[test]
fn setup_installs_scheduler_checks_run_once_and_reports_no_false_next_run() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new(true);
    let scheduler = sandbox.root.path().join("fake-scheduler");
    let calls = sandbox.root.path().join("scheduler-calls");
    let state = sandbox.root.path().join("scheduler-active");
    let script = if cfg!(target_os = "macos") {
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\necho scheduler-noise\necho scheduler-error >&2\ncase \"$1\" in\nprint) test -f '{}' ;;\nbootstrap) touch '{}' ;;\nbootout) rm -f '{}' ;;\nesac\n",
            calls.display(), state.display(), state.display(), state.display()
        )
    } else {
        // `$3` is the verb of `systemctl --user --no-pager status <timer>`,
        // the one call whose output the next-run probe reads. It answers with a
        // fixed `Trigger:` line: the probe must pass the scheduler's own text
        // through, never recompute it from the interval it was given.
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\necho scheduler-noise\necho scheduler-error >&2\ncase \"$2\" in\nis-active) test -f '{}' ;;\nenable) touch '{}' ;;\ndisable) rm -f '{}' ;;\nesac\ncase \"$3\" in\nstatus) echo '    Trigger: Sun 2026-09-27 03:17:00 UTC; 3 days left' ;;\nesac\n",
            calls.display(), state.display(), state.display(), state.display()
        )
    };
    fs::write(&scheduler, script).expect("write fake scheduler");
    let mut permissions = fs::metadata(&scheduler)
        .expect("scheduler metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&scheduler, permissions).expect("make scheduler executable");

    let scheduled_binary = sandbox.home().join(".local/bin/chat-stasher");
    fs::create_dir_all(scheduled_binary.parent().expect("binary parent"))
        .expect("create installed binary directory");
    fs::copy(env!("CARGO_BIN_EXE_chat-stasher"), &scheduled_binary)
        .expect("copy CLI to installed path for scheduler self-check");

    let stage = sandbox.stage();
    let args = [
        "setup",
        "--stage",
        stage.to_str().expect("stage path"),
        "--masterkey-saved-elsewhere",
        "--install-schedule",
        "--json",
    ];
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("HOME", sandbox.home())
            .env("XDG_CONFIG_HOME", sandbox.root.path().join("config"))
            .env("XDG_DATA_HOME", sandbox.root.path().join("data"))
            .env("XDG_STATE_HOME", sandbox.root.path().join("state"))
            .env(
                "CHAT_STASHER_REGISTRY",
                sandbox.root.path().join("registry.json"),
            )
            .env("CHAT_STASHER_LAUNCHCTL", &scheduler)
            .env("CHAT_STASHER_SYSTEMCTL", &scheduler)
            .output()
            .expect("run setup with fake scheduler")
    };

    for _ in 0..2 {
        let output = run(&args);
        let value = json_of(&output);
        assert_eq!(exit_code(&output), 0, "value={value}");
        assert_eq!(value["steps"]["schedule"], "installed_and_checked");
        assert_eq!(value["schedule"]["status"], "installed_and_checked");
        if cfg!(target_os = "linux") {
            // The exact string the scheduler reported, not the hourly cadence
            // this run installed.
            assert_eq!(
                value["schedule"]["next_run"],
                serde_json::json!("Sun 2026-09-27 03:17:00 UTC")
            );
            assert!(
                value["schedule"].get("next_run_note").is_none(),
                "a reported next run carries no excuse: {value}"
            );
        } else {
            assert_eq!(value["schedule"]["next_run"], serde_json::Value::Null);
            assert_eq!(
                value["schedule"]["next_run_note"],
                serde_json::json!(
                    "launchd interval jobs expose no next fire time; the job runs every 60 \
                     minutes after load"
                ),
                "an empty next run has to arrive with the sentence saying why: {value}"
            );
        }
        assert!(!String::from_utf8_lossy(&output.stdout).contains("scheduler-noise"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("scheduler-error"));
    }
    let uninstall_args = [
        "setup",
        "--stage",
        stage.to_str().expect("stage path"),
        "--masterkey-saved-elsewhere",
        "--uninstall-schedule",
        "--json",
    ];
    let output = run(&uninstall_args);
    let value = json_of(&output);
    assert_eq!(exit_code(&output), 0, "value={value}");
    assert_eq!(value["steps"]["schedule"], "uninstalled");
    assert_eq!(value["schedule"]["status"], "uninstalled");
    assert_eq!(value["schedule"]["next_run"], serde_json::Value::Null);
    assert_eq!(
        value["schedule"]["next_run_note"],
        serde_json::json!(
            "no scheduler timer was installed by this run, so there is no next run to report"
        ),
        "a removed timer is a different state from a timer nobody could ask: {value}"
    );
    let calls = fs::read_to_string(calls).expect("read fake scheduler calls");
    assert_eq!(
        calls.matches("enable --now").count(),
        usize::from(cfg!(target_os = "linux"))
    );
    assert_eq!(
        calls.matches("bootstrap").count(),
        usize::from(cfg!(target_os = "macos"))
    );
    assert_eq!(
        calls.matches("disable --now").count(),
        usize::from(cfg!(target_os = "linux"))
    );
    assert_eq!(
        calls.matches("bootout").count(),
        usize::from(cfg!(target_os = "macos"))
    );
}

#[cfg(unix)]
#[test]
fn setup_self_check_uses_the_installed_binary_selected_from_a_build_artifact() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new(true);
    let scheduler = sandbox.root.path().join("fake-scheduler");
    let state = sandbox.root.path().join("scheduler-active");
    let script = if cfg!(target_os = "macos") {
        format!(
            "#!/bin/sh\ncase \"$1\" in\nprint) test -f '{}' ;;\nbootstrap) touch '{}' ;;\nbootout) rm -f '{}' ;;\nesac\n",
            state.display(), state.display(), state.display()
        )
    } else {
        format!(
            "#!/bin/sh\ncase \"$2\" in\nis-active) test -f '{}' ;;\nenable) touch '{}' ;;\ndisable) rm -f '{}' ;;\nesac\n",
            state.display(), state.display(), state.display()
        )
    };
    fs::write(&scheduler, script).expect("write fake scheduler");
    let mut permissions = fs::metadata(&scheduler)
        .expect("scheduler metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&scheduler, permissions).expect("make scheduler executable");

    let installed_binary = sandbox.home().join(".local/bin/chat-stasher");
    fs::create_dir_all(installed_binary.parent().expect("binary parent"))
        .expect("create installed binary directory");
    fs::write(&installed_binary, "#!/bin/sh\nexit 1\n").expect("write failing installed binary");
    let mut permissions = fs::metadata(&installed_binary)
        .expect("installed binary metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&installed_binary, permissions).expect("make installed binary executable");

    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args([
            "setup",
            "--stage",
            sandbox.stage().to_str().expect("stage path"),
            "--masterkey-saved-elsewhere",
            "--install-schedule",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", sandbox.home())
        .env("XDG_CONFIG_HOME", sandbox.root.path().join("config"))
        .env("XDG_DATA_HOME", sandbox.root.path().join("data"))
        .env("XDG_STATE_HOME", sandbox.root.path().join("state"))
        .env(
            "CHAT_STASHER_REGISTRY",
            sandbox.root.path().join("registry.json"),
        )
        .env("CHAT_STASHER_LAUNCHCTL", &scheduler)
        .env("CHAT_STASHER_SYSTEMCTL", &scheduler)
        .output()
        .expect("run setup with installed fallback binary");
    let value = json_of(&output);

    assert_eq!(exit_code(&output), 1, "value={value}");
    assert_eq!(value["schedule"]["status"], "self_check_failed");
    assert_eq!(value["steps"]["schedule"], "self_check_failed");
    assert_eq!(
        value["schedule"]["next_run"],
        serde_json::Value::Null,
        "a next run the scheduler did not report must not be invented"
    );
    let note = value["schedule"]["next_run_note"]
        .as_str()
        .expect("an empty next run arrives with the sentence saying why");
    if cfg!(target_os = "macos") {
        assert!(
            note.contains("launchd interval jobs expose no next fire time"),
            "note={note}"
        );
    } else {
        // This fake answers `is-active` and `enable` only, so the probe gets no
        // `Trigger:` line at all — which is not the same as a timer systemd
        // says has no next elapse, and both must be reported as an absence.
        assert!(note.contains("did not report a next run"), "note={note}");
    }
}

#[test]
fn non_tty_setup_emits_json_missing_parameters_and_does_not_echo_stdin() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .arg("setup")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"synthetic-secret-input")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["command"], "setup");
    assert_eq!(value["missing_parameters"], serde_json::json!(["stage"]));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-secret-input"));
}

#[test]
fn non_tty_setup_refuses_invalid_config_instead_of_scanning_defaults() {
    let home = tempfile::tempdir().unwrap();
    let config_home = home.path().join("config");
    let config_dir = config_home.join("chat-stasher");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "this is not valid TOML = [").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(["setup", "--stage", "/fixture/stage"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", &config_home)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(3));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["command"], "setup");
    assert_eq!(value["healthy"], false);
    assert_eq!(value["exit_code"], 3);
    assert_eq!(value["scanner"]["kind"], "failed");
    assert!(value["scanner"]["why"]
        .as_str()
        .unwrap()
        .contains("not valid TOML"));
}

/// The scan the wizard shows is `status`'s scan, byte for byte. Now that the
/// wizard also writes, the two are compared on a machine where the wizard has
/// something to do, so the equality is not an artifact of both being idle.
#[test]
fn setup_and_status_commands_emit_the_same_scan_json() {
    let sandbox = Sandbox::new(true);
    let setup_output = sandbox.setup(&["--masterkey-saved-elsewhere"]);
    let setup = json_of(&setup_output);
    assert_eq!(exit_code(&setup_output), 0, "value={setup}");

    let status_output = sandbox.command(&["status", "--json"]);
    let status = json_of(&status_output);

    let mut status_scan = status["scanner"].clone();
    status_scan
        .as_object_mut()
        .unwrap()
        .remove("writer_versions");
    assert_eq!(setup["scanner"], status_scan);
}

// ---------------------------------------------------------------------------
// WIZ-3: the remote step.
//
// Every backend here is a **fake**: a directory inside the sandbox, or a closed
// port on loopback. Nothing in this file opens a socket to a host that is not
// this machine, and no test ever writes to a real `known_hosts` — the sandbox's
// HOME is a temp directory, so even a trust path that ran by mistake could only
// touch the sandbox.
// ---------------------------------------------------------------------------

/// The three states a destination can be in, on one machine, in one run each.
///
/// The point of the trio is the third: a destination that cannot be reached is
/// reported as *unread* with exit 3 — the code that means "what was not read
/// proves nothing" — and not as an empty destination or a completed run.
#[test]
fn a_reachable_destination_is_verified_and_an_unreachable_one_is_unread() {
    // (1) A destination declared by hand, pointing at a directory inside the
    // sandbox. This is the "adopt what the file says" path: the wizard writes
    // nothing, connects, and lets `dest-init` seed it. The key file is the one
    // the local first save already created, so the adopt path is exercised
    // rather than short-circuited by a missing key.
    let sandbox = Sandbox::new(true);
    let remote = sandbox.fake_remote();
    sandbox.write_config(&format!(
        "[destinations.fake]\nrepo = '{}'\nkey_file = '{}'\n",
        remote.display(),
        sandbox.masterkey().display()
    ));
    let output = sandbox.setup(&["--destination", "fake", "--masterkey-saved-elsewhere"]);
    let value = json_of(&output);

    assert_eq!(
        exit_code(&output),
        0,
        "a reachable destination must not make the run fail: {value}"
    );
    assert_eq!(value["steps"]["destination"], "reachable");
    assert_eq!(value["destination"]["config"]["kind"], "already_declared");
    assert_eq!(value["destination"]["reach"]["kind"], "reached");
    assert_eq!(value["destination"]["dest_init"]["kind"], "ran");
    assert_eq!(value["destination"]["dest_init"]["exit_code"], 0);
    assert_eq!(
        value["destination"]["trust"]["known_hosts_write_authorized"],
        false
    );
    assert_eq!(value["unread"], serde_json::json!([]));
    assert_eq!(value["incomplete"], serde_json::json!([]));
    // The report has to describe something that actually happened.
    assert!(
        remote.join("config").exists() || remote.join("data").exists(),
        "dest-init reported exit 0, so it must have created a repository at {}",
        remote.display()
    );

    // (2) A destination declared by hand whose endpoint is a closed port on
    // loopback. Connection refused, no network, and — the property under test —
    // *not* an empty destination.
    let unreachable = Sandbox::new(true);
    unreachable.write_config(
        "[destinations.gone]\n\
         repo = 'opendal:sftp'\n\
         key_file = '/nonexistent/key.json'\n\
         [destinations.gone.options]\n\
         endpoint = 'ssh://127.0.0.1:1'\n\
         user = 'nobody'\n",
    );
    let output = unreachable.setup(&["--destination", "gone", "--masterkey-saved-elsewhere"]);
    let value = json_of(&output);

    assert_eq!(
        exit_code(&output),
        3,
        "an unreachable destination is 'did not finish reading', not a failed read: {value}"
    );
    assert_eq!(value["exit_code"], 3);
    assert_eq!(value["steps"]["destination"], "unread");
    assert_eq!(value["destination"]["reach"]["kind"], "unreachable");
    assert_eq!(
        value["destination"]["trust"]["known_hosts_write_authorized"],
        false
    );
    // Nothing was connected to, so nothing may be reported as run.
    assert_eq!(value["destination"]["dest_init"]["kind"], "not_run");
    assert_eq!(value["unread"], serde_json::json!(["destination"]));
    assert_eq!(value["incomplete"], serde_json::json!([]));
    // The reach object must not carry a repository verdict: the probe never got
    // one, and `false` here would read as "we looked and there was nothing".
    assert!(
        value["destination"]["reach"]
            .get("repository_exists")
            .is_none(),
        "nothing was measured at the destination, so no verdict may be present: {value}"
    );

    // (3) The same two runs, but the whole remote step skipped. Exit 0 — the
    // archive really does exist — with the cost stated rather than implied.
    let skipped = Sandbox::new(true);
    let output = skipped.setup(&["--masterkey-saved-elsewhere"]);
    let value = json_of(&output);
    assert_eq!(exit_code(&output), 0, "value={value}");
    assert_eq!(value["steps"]["destination"], "skipped");
    assert!(
        value["destination"]["consequence"]
            .as_str()
            .is_some_and(|text| text.contains("loses the archive")),
        "the skip branch must say what it costs: {value}"
    );
}

/// A destination declared by hand with no `repo` is a config-content problem
/// this run *read* — so it is reported, not treated as a usage error.
///
/// The block below is the shape ADR-039 keeps open for the local-path and REST
/// candidates, and the shape a half-filled-in block has. Every other command
/// resolves a destination through `resolve_store_config`, which ends the process
/// with 2; reached from inside the wizard, that made a `--json` run print
/// **nothing at all** on stdout — not even the object that says what is wrong —
/// and called a config the run had already read a command-line mistake. Two of
/// the three exit codes exist precisely to keep those apart.
#[test]
fn a_declared_destination_without_a_repo_is_reported_rather_than_exiting() {
    let sandbox = Sandbox::new(true);
    sandbox.write_config("[destinations.bare]\nkey_file = '/nonexistent/key.json'\n");
    let before = sandbox.read_config();
    let output = sandbox.setup(&["--destination", "bare", "--masterkey-saved-elsewhere"]);
    let value = json_of(&output);

    assert_eq!(
        exit_code(&output),
        3,
        "the destination was never consulted, so what was not read proves nothing: {value}"
    );
    assert_eq!(value["steps"]["destination"], "unread");
    assert_eq!(value["destination"]["reach"]["kind"], "unreadable");
    assert_eq!(value["unread"], serde_json::json!(["destination"]));
    assert_eq!(value["incomplete"], serde_json::json!([]));
    // A half-written block is the user's: the wizard adopts it as it stands and
    // never rewrites it.
    assert_eq!(value["destination"]["config"]["kind"], "already_declared");
    assert_eq!(
        sandbox.read_config(),
        before,
        "the adopted block must be left exactly as it was found"
    );
    // The reason has to name what is missing, or a wrapper knows only that
    // something is wrong.
    let why = value["destination"]["reach"]["why"]
        .as_str()
        .expect("a why string");
    assert!(
        why.contains("repo"),
        "the reason must name the missing key: {why}"
    );
    // Nothing may be reported as run against a destination that was never
    // resolved.
    assert_eq!(value["destination"]["dest_init"]["kind"], "not_run");
}

/// The credential observation reaches the JSON, not only the terminal.
///
/// A non-TTY run prints no wizard lines at all — stdout is one JSON object, and
/// that object is the whole of what a wrapper sees — so an observation that
/// existed only as a stderr sentence was missing for exactly the half of the
/// acceptance surface the flags exist for. The two variables are reported
/// separately because they have two different fixes, and neither is reported as
/// a missing *parameter*: the parameter was given, the variable is absent.
#[test]
fn an_unusable_credential_variable_is_reported_to_a_non_tty_caller() {
    let sandbox = Sandbox::new(true);
    let unset = "CHAT_STASHER_W177_NOT_SET_ANYWHERE";
    let empty = "CHAT_STASHER_W177_SET_BUT_EMPTY";
    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args([
            "setup",
            "--stage",
            sandbox.stage().to_str().expect("utf-8 stage"),
            "--destination",
            "r2box",
            "--masterkey-saved-elsewhere",
            "--remote",
            "s3",
            "--remote-endpoint",
            "https://127.0.0.1:1",
            "--remote-bucket",
            "fixture-bucket",
            "--remote-access-key-id-env",
            unset,
            "--remote-secret-key-env",
            empty,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", sandbox.home())
        .env("XDG_CONFIG_HOME", sandbox.root.path().join("config"))
        .env("XDG_DATA_HOME", sandbox.root.path().join("data"))
        .env("XDG_STATE_HOME", sandbox.root.path().join("state"))
        .env(
            "CHAT_STASHER_REGISTRY",
            sandbox.root.path().join("registry.json"),
        )
        .env_remove(unset)
        .env(empty, "")
        .output()
        .expect("run chat-stasher");

    let value = json_of(&output);
    // The endpoint is a closed loopback port, so the destination is unread. What
    // is under test is the observation, which is taken before any connection.
    assert_eq!(exit_code(&output), 3, "value={value}");
    assert_eq!(value["destination"]["credentials"]["kind"], "checked");
    assert_eq!(
        value["destination"]["credentials"]["variables"][0]["name"],
        unset
    );
    assert_eq!(
        value["destination"]["credentials"]["variables"][0]["state"], "not_set",
        "an unset variable is a state in the object, not an absent field: {value}"
    );
    assert_eq!(
        value["destination"]["credentials"]["variables"][1]["state"], "empty",
        "set-but-empty has a different fix, so it is a different state: {value}"
    );
    assert_eq!(
        value["missing_parameters"],
        serde_json::json!([]),
        "the parameter was supplied; the *variable* it names is what is absent: {value}"
    );
}

/// The property the credential indirection exists for, checked on the bytes.
///
/// The run is aimed at a **closed port on loopback** with credentials that are
/// set to recognisable fake values. Two things are asserted about the config the
/// wizard wrote: it carries `env:VAR`, and it does not carry the fake secret
/// anywhere. The second is the one that matters — a config that loads fine and
/// leaks a credential is the failure this design exists to prevent.
#[test]
fn the_written_destination_carries_credential_references_and_never_the_secret() {
    let sandbox = Sandbox::new(true);
    let access = "not-a-real-access-key-id";
    let secret = "not-a-real-secret-value";
    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args([
            "setup",
            "--stage",
            sandbox.stage().to_str().expect("utf-8 stage"),
            "--destination",
            "r2box",
            "--masterkey-saved-elsewhere",
            "--remote",
            "s3",
            "--remote-endpoint",
            "https://127.0.0.1:1",
            "--remote-bucket",
            "fixture-bucket",
            "--remote-access-key-id-env",
            "W177_FAKE_ACCESS_KEY_ID",
            "--remote-secret-key-env",
            "W177_FAKE_SECRET",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", sandbox.home())
        .env("XDG_CONFIG_HOME", sandbox.root.path().join("config"))
        .env("XDG_DATA_HOME", sandbox.root.path().join("data"))
        .env("XDG_STATE_HOME", sandbox.root.path().join("state"))
        .env(
            "CHAT_STASHER_REGISTRY",
            sandbox.root.path().join("registry.json"),
        )
        .env("W177_FAKE_ACCESS_KEY_ID", access)
        .env("W177_FAKE_SECRET", secret)
        .output()
        .expect("run chat-stasher");

    let value = json_of(&output);
    // The endpoint is a closed loopback port, so the destination is unread —
    // and that is fine: what is under test is the file that was written before
    // the connection was attempted.
    assert_eq!(exit_code(&output), 3, "value={value}");
    assert_eq!(value["destination"]["config"]["kind"], "written");
    assert_eq!(value["destination"]["reach"]["kind"], "unreachable");
    assert_eq!(value["destination"]["remote_kind"], "s3");
    assert_eq!(value["destination"]["recommended_remote_kind"], "s3");

    let written = sandbox.read_config();
    assert!(
        written.contains("env:W177_FAKE_ACCESS_KEY_ID"),
        "the credential must be written as a reference: {written}"
    );
    assert!(written.contains("env:W177_FAKE_SECRET"), "{written}");
    assert!(
        !written.contains(access) && !written.contains(secret),
        "the credentials must not reach the config file: {written}"
    );
    // §4.5 requires both switches, and they are the reason a missing credential
    // cannot fall through to the ambient AWS chain or to the instance metadata
    // service.
    assert!(written.contains("region = \"auto\""), "{written}");
    assert!(
        written.contains("disable_config_load = \"true\""),
        "{written}"
    );
    assert!(
        written.contains("disable_ec2_metadata = \"true\""),
        "{written}"
    );
    // The secret must not reach stdout or stderr either — those are the streams
    // that end up in a log or a CI transcript.
    let streams = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!streams.contains(access), "a credential reached the output");
    assert!(!streams.contains(secret), "a credential reached the output");
}

/// A pasted credential where a variable name belongs is refused before anything
/// is written, and the value is never echoed — on either stream, in either mode.
#[test]
fn a_pasted_credential_is_refused_and_never_echoed() {
    for pasted in [
        // The shape a secret access key has, and the shape most tokens have.
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        // The shape the variable-name check cannot catch on its own: an access
        // key id is all uppercase letters and digits.
        "AKIAIOSFODNN7EXAMPLE",
    ] {
        let sandbox = Sandbox::new(true);
        let output = sandbox.setup(&[
            "--destination",
            "r2box",
            "--remote",
            "s3",
            "--remote-endpoint",
            "https://127.0.0.1:1",
            "--remote-bucket",
            "fixture-bucket",
            "--remote-access-key-id-env",
            pasted,
            "--remote-secret-key-env",
            "W177_PLACEHOLDER",
        ]);
        let value = json_of(&output);

        assert_eq!(exit_code(&output), 2, "value={value}");
        assert_eq!(
            value["invalid_parameters"],
            serde_json::json!(["--remote-access-key-id-env"])
        );
        // Separate from `missing_parameters`: the parameter is present and
        // unusable, so a wrapper that supplied it still has to change it.
        assert_eq!(value["missing_parameters"], serde_json::json!([]));
        assert!(
            !sandbox.config_file().exists(),
            "a refused command line must write nothing, including no config file"
        );
        assert!(
            !sandbox.stage().exists() && !sandbox.data_root().exists(),
            "the refusal is checked before any work, so the local archive must not exist either"
        );
        let streams = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !streams.contains(pasted),
            "the refusal must not echo the value it refused"
        );
    }
}

/// A kind that was named without its parameters reports them by name, writes
/// nothing, and says which flag supplies each one.
#[test]
fn an_incomplete_remote_names_every_parameter_it_needs() {
    let sandbox = Sandbox::new(true);
    let output = sandbox.setup(&[
        "--destination",
        "r2box",
        "--masterkey-saved-elsewhere",
        "--remote",
        "s3",
        "--remote-endpoint",
        "https://127.0.0.1:1",
    ]);
    let value = json_of(&output);

    assert_eq!(exit_code(&output), 2, "value={value}");
    assert_eq!(
        value["missing_parameters"],
        serde_json::json!([
            "remote_bucket",
            "remote_access_key_id_env",
            "remote_secret_key_env"
        ])
    );
    assert_eq!(value["destination"]["config"]["kind"], "not_written");
    assert_eq!(value["destination"]["dest_init"]["kind"], "not_run");
    // The names are the flag ids, so the report and the flag are one word.
    for name in [
        "remote_bucket",
        "remote_access_key_id_env",
        "remote_secret_key_env",
    ] {
        assert!(
            value["destination"]["config"]["why"]
                .as_str()
                .is_some_and(|why| !why.is_empty()),
            "an unwritten config must say why: {name} in {value}"
        );
    }
    assert!(
        !sandbox.config_file().exists(),
        "an incomplete command line must not write a partial destination block"
    );
}

/// A destination the wizard cannot spell is not a destination it may guess at:
/// naming one without a kind is a named missing parameter, and nothing is
/// written.
#[test]
fn naming_an_undeclared_destination_without_a_kind_asks_for_the_kind() {
    let sandbox = Sandbox::new(true);
    let output = sandbox.setup(&["--destination", "nowhere", "--masterkey-saved-elsewhere"]);
    let value = json_of(&output);

    assert_eq!(exit_code(&output), 2, "value={value}");
    assert_eq!(value["missing_parameters"], serde_json::json!(["remote"]));
    assert_eq!(value["destination"]["config"]["kind"], "not_written");
    assert!(
        !sandbox.config_file().exists(),
        "nothing may be written when the kind is unknown"
    );
}

/// `--trust-host` is a declaration, and an SFTP destination that never gets as
/// far as a host key must not record one.
///
/// This is the property ADR-039's rejected option E is about, and it is checked
/// on the sandbox's own `known_hosts`: an unreachable host writes nothing to it,
/// whether or not the declaration was made. The end-to-end "unknown host stops
/// and waits" path needs a real ssh handshake and is **not** exercised here —
/// see the report; what is exercised is that the flag cannot cause a write on a
/// destination that was never reached.
#[test]
fn a_run_that_never_reached_a_host_writes_nothing_to_known_hosts() {
    for extra in [vec![], vec!["--trust-host"]] {
        let sandbox = Sandbox::new(true);
        sandbox.write_config(
            "[destinations.gone]\n\
             repo = 'opendal:sftp'\n\
             key_file = '/nonexistent/key.json'\n\
             [destinations.gone.options]\n\
             endpoint = 'ssh://127.0.0.1:1'\n\
             user = 'nobody'\n",
        );
        let mut args = vec!["--destination", "gone", "--masterkey-saved-elsewhere"];
        args.extend_from_slice(&extra);
        let output = sandbox.setup(&args);
        let value = json_of(&output);

        assert_eq!(exit_code(&output), 3, "value={value}");
        assert_eq!(
            value["destination"]["trust"]["known_hosts_write_authorized"],
            false
        );
        assert_eq!(value["destination"]["dest_init"]["kind"], "not_run");
        let known_hosts = sandbox.home().join(".ssh").join("known_hosts");
        assert!(
            !known_hosts.exists(),
            "nothing may be recorded for a host that was never reached ({}): {}",
            if extra.is_empty() {
                "no declaration"
            } else {
                "--trust-host given"
            },
            known_hosts.display()
        );
    }
}

// ---------------------------------------------------------------------------
// EXT-1: the browser host step.
//
// The wizard does not register a native messaging host — ADR-039 decision 7
// puts `install-native-host` in the user's hands — so this step is a *report*,
// and the properties worth pinning are that it reports the same inventory
// `doctor` does and that nothing in it can be read as "the extension is
// installed". A data directory outlives an uninstall and is shared by every
// profile, so the two are different facts and the JSON keeps them apart.
// ---------------------------------------------------------------------------

/// The three ways a host step can read, so a wrapper can branch on one field.
const HOST_STEPS: [&str; 3] = ["registered", "none_registered", "nothing_to_look_at"];

#[test]
fn setup_reports_the_browser_host_inventory_doctor_reports() {
    let sandbox = Sandbox::new(true);
    let setup_output = sandbox.setup(&["--masterkey-saved-elsewhere"]);
    let setup = json_of(&setup_output);
    assert_eq!(exit_code(&setup_output), 0, "value={setup}");

    let step = setup["steps"]["native_host"]
        .as_str()
        .expect("the host step is named");
    assert!(
        HOST_STEPS.contains(&step),
        "the host step must be one of {HOST_STEPS:?}, not {step:?}"
    );

    // The same machine, asked twice. `doctor`'s D8 and the wizard resolve the
    // same root and read the same config, so a difference here means one of the
    // two surfaces is describing this machine in its own words — which is how
    // two screens end up disagreeing about one browser.
    let doctor_output = sandbox.command(&["doctor", "--json"]);
    let doctor = json_of(&doctor_output);
    assert_eq!(
        setup["native_host"], doctor["native_host"],
        "the wizard and `doctor` must report one inventory, not two"
    );
    assert_eq!(step, setup["native_host"]["step"].as_str().unwrap());
    assert_ne!(
        step, "not_checked",
        "this run read the config, so it looked: {setup}"
    );

    let rows = setup["native_host"]["manifests"]
        .as_array()
        .expect("the inventory is a list of browsers");
    assert!(
        !rows.is_empty(),
        "a machine always has browsers to look for"
    );

    // Per browser: three facts, none of them "installed".
    for row in rows {
        assert!(
            row.get("installed").is_none(),
            "a browser's data directory must never be serialised as `installed`: {row}"
        );
        for field in [
            "browser",
            "support",
            "detected",
            "manifest",
            "registered",
            "kind",
        ] {
            assert!(row.get(field).is_some(), "{field} missing from {row}");
        }
        // The two states this build can be in for a pair are "we looked and
        // there is nothing here" and "we do not look there" — and the second
        // has no path to report. A row with neither is a row that lost its
        // subject.
        let no_path = row["kind"] == "no_discovery_path";
        assert_eq!(no_path, row["manifest"].is_null(), "{row}");
        assert_eq!(no_path, row["support"].is_null(), "{row}");
        if no_path {
            continue;
        }
        assert_eq!(row["kind"], "not_registered", "{row}");
        assert_eq!(row["registered"], false, "{row}");
    }

    // `registered` counts rows, and `detected_not_registered` is the actionable
    // intersection of two independent facts — never a sum of them.
    let counted = rows.iter().filter(|row| row["registered"] == true).count();
    assert_eq!(setup["native_host"]["registered"], counted, "{setup}");

    let actionable: Vec<&str> = setup["native_host"]["detected_not_registered"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    for id in &actionable {
        let row = rows
            .iter()
            .find(|row| row["browser"] == *id)
            .expect("an actionable browser is a row");
        assert_eq!(row["detected"], true, "{row}");
        assert_eq!(row["registered"], false, "{row}");
    }

    // Nothing was registered in this sandbox, whatever the runner's own
    // machine happens to have: the sandbox HOME has no browser directories.
    assert_eq!(setup["native_host"]["registered"], 0, "{setup}");
    for id in ["chrome", "edge", "firefox"] {
        let supported: Vec<&str> = setup["native_host"]["supported"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert!(supported.contains(&id), "{id} is not in {supported:?}");
    }
}

/// The host step reaches the JSON even on a run that stops early, and says it
/// did not look rather than that it found nothing.
///
/// `setup_without_a_stage_names_the_parameter_and_writes_nothing` covers the
/// exit code and the missing parameter; this pins the third answer, because a
/// wrapper that reads `steps.native_host` must not see `none_registered` on a
/// run that never opened a browser directory.
#[test]
fn a_host_step_that_was_never_asked_says_not_checked() {
    let sandbox = Sandbox::new(false);
    let output = sandbox.command(&["setup"]);
    let value = json_of(&output);
    assert_eq!(exit_code(&output), 2, "value={value}");
    assert_eq!(value["missing_parameters"], serde_json::json!(["stage"]));
    assert_eq!(value["steps"]["native_host"], "not_checked");
    assert_eq!(value["native_host"]["kind"], "not_checked");
    assert!(
        value["native_host"].get("registered").is_none(),
        "a run that never looked must not carry a registration count: {value}"
    );
    assert!(
        value["native_host"]["why"]
            .as_str()
            .is_some_and(|why| why.contains("did not look")),
        "the reason must say it did not look, not that it found nothing: {value}"
    );
}
