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
