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

// One implementation, shared with the crate's unit tests: an integration suite
// cannot see a `#[cfg(test)]` item of the crate, so this file is pulled in by
// path rather than copied.
#[path = "../src/test_support.rs"]
mod test_support;

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
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&self.home()),
            )
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("XDG_CACHE_HOME", self.root.path().join("rh-cache"))
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
    // WIZ-1 (ADR-039): the missing parameter is validated *before* the archive
    // pass, so a run that exits 2 must report the declaration as not made and
    // the pass as not started — never an archive the caller was not shown.
    //
    // The key is the one thing that pass does not own (W236 rule 2): the
    // declaration is a human step, and a human cannot attest to a copy of a file
    // that does not exist yet, so when the declaration is the *only* thing owed
    // this run creates the local repository and its key and stops — no snapshot,
    // no remote, no timer. `missing_masterkey_declaration_alone_bootstraps_the_
    // key_and_stops` is where that is pinned in full; here the object only has
    // to offer the key's real path, so the agent can name the file.
    assert_eq!(value["masterkey"]["declaration"], "not_declared");
    assert_eq!(value["masterkey"]["declaration_is_verified"], false);
    assert_eq!(value["steps"]["stage"], "provided");
    assert_eq!(
        value["steps"]["local_save"], "not_attempted",
        "a refused run must not claim it created an archive: {value}"
    );
    assert_eq!(value["steps"]["masterkey"], "not_declared");
    assert_eq!(
        value["masterkey"]["path"],
        serde_json::json!(undeclared.masterkey().to_str().unwrap()),
        "the key the user is asked to copy must be the one on this disk: {value}"
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

/// Every path in the throwaway HOME + XDG tree, root-relative and sorted, used
/// to prove a refusal wrote exactly what it says it wrote.
///
/// Names only — a file rewritten with different bytes is not visible here. That
/// is enough for what this compares: the tree is created empty and the only
/// file that exists before a run is `registry.json`, which no run rewrites.
/// Root-relative, not relative to each directory's parent, so a caller can tell
/// "the repository" from "the rustic cache" by prefix rather than by guessing
/// which component a fragment came from.
///
/// Components are joined with `/`, and that is the whole point of spelling a path
/// out here: the callers filter these strings by prefix (`data/chat-stasher/…`)
/// and by component name (`rustic`), and `Path::display` would hand them
/// `data\chat-stasher\…` on Windows, matching neither — so the snapshot has to
/// be the platform's own separators normalised away, not passed through.
fn tree_snapshot(root: &std::path::Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .expect("a dir that exists is enumerable")
            .collect::<Result<_, _>>()
            .expect("enumerate dir");
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(&path);
            out.push(
                rel.components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/"),
            );
            if path.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// WIZ-1's one deliberate exception to "exit 2 writes nothing": when the *only*
/// owed parameter is the masterkey declaration, a non-TTY `setup` still refuses
/// (exit 2), but it first creates the local repository and its masterkey — the
/// minimum a human needs in front of them to copy the key off this disk — and
/// stops before the archive pass, the remote step and the scheduler. The
/// masterkey object carries the key's `path`, so the agent can tell the user
/// which file to copy; re-running with the declaration then continues.
#[test]
fn missing_masterkey_declaration_alone_bootstraps_the_key_and_stops() {
    let sandbox = Sandbox::new(true);
    let before = tree_snapshot(sandbox.root.path());

    let output = sandbox.setup(&[]);
    let value = json_of(&output);

    assert_eq!(exit_code(&output), 2, "value={value}");
    assert_eq!(
        value["missing_parameters"],
        serde_json::json!(["masterkey_saved_elsewhere"])
    );
    // The archive pass never ran — no snapshot was archived — and the
    // declaration is still owed. These are observations, not a claim that the
    // repository does not exist: the JSON says so separately through the key's
    // `path`.
    assert_eq!(value["steps"]["local_save"], "not_attempted");
    assert_eq!(value["steps"]["masterkey"], "not_declared");
    assert_eq!(value["chain"]["kind"], "not_attempted");
    assert_eq!(value["runs"]["kind"], "not_attempted");
    assert_eq!(value["steps"]["schedule"], "not_attempted");
    // The masterkey object offers the path of the key that was just created, so
    // an agent knows exactly which file to tell the user to copy.
    assert_eq!(
        value["masterkey"]["path"],
        serde_json::json!(sandbox.masterkey().to_str().unwrap())
    );
    assert_eq!(value["masterkey"]["declaration"], "not_declared");
    assert_eq!(value["masterkey"]["declaration_is_verified"], false);

    // Only the repository and the key appeared. Their creation is the point of
    // the run; everything else owns all three "must NOT" guarantees from WIZ-1:
    // no archive snapshot (no run-once pass), no config remote, no scheduler.
    assert!(sandbox.repository().exists(), "the repository must exist");
    assert!(sandbox.masterkey().exists(), "the masterkey must exist");
    assert!(
        !sandbox.config_file().exists(),
        "a bootstrap keys the local archive only; it must not write a destination block"
    );
    let state_run_marker = sandbox
        .root
        .path()
        .join("data/chat-stasher/state/run-state.json");
    assert!(
        !state_run_marker.exists(),
        "no run-once pass ran, so no run-state (no archive snapshot) may exist: {}",
        state_run_marker.display()
    );
    assert!(
        !sandbox.stage().exists(),
        "no pass ran, so the stage must not be created either: {}",
        sandbox.stage().display()
    );

    let after = tree_snapshot(sandbox.root.path());
    let added: Vec<_> = after
        .iter()
        .filter(|path| !before.contains(path))
        .cloned()
        .collect();
    assert!(
        !added.is_empty(),
        "the bootstrap must write the repo and the key"
    );
    // The rustic library opens a cache of its own the first time it touches a
    // repository — `$HOME/Library/Caches/rustic` here, `$XDG_CACHE_HOME` (or
    // `$HOME/.cache`) elsewhere. That cache and the directories created to hold
    // it are incidental, not archive writes, and their shape is the platform's
    // business, so they are the one allowance this comparison makes. Everything
    // else added has to be the local repository and its key.
    let cache_paths: Vec<&String> = added
        .iter()
        .filter(|path| path.split('/').any(|component| component == "rustic"))
        .collect();
    let is_cache_path = |path: &str| {
        path.split('/').any(|component| component == "rustic")
            || cache_paths
                .iter()
                .any(|cache| cache.starts_with(&format!("{path}/")))
    };
    let unexpected: Vec<_> = added
        .iter()
        .filter(|path| *path != "data/chat-stasher" && !path.starts_with("data/chat-stasher/"))
        .filter(|path| !is_cache_path(path.as_str()))
        .cloned()
        .collect();
    assert!(
        unexpected.is_empty(),
        "the only new paths may be the local repository and its key (plus the rustic cache \
         the library opens on its own); added: {added:?}"
    );
    // The repository exists but holds no snapshot, which is the direct proof
    // that the bootstrap initialized and stopped rather than archiving: the
    // archive pass would have left one here.
    let snapshots = fs::read_dir(sandbox.repository().join("snapshots"))
        .expect("an initialized repository has a snapshots directory");
    assert_eq!(
        snapshots.count(),
        0,
        "the bootstrap must not archive a snapshot"
    );
}

/// A bootstrap that could not run refuses without inventing a key to copy.
///
/// The reason is deliberately silent about how far the attempt got: the key is
/// persisted *before* the repository is initialized, so a failure in the second
/// half leaves a key file on the disk. Which is why the refusal may say that
/// nothing was **archived** and may not say that nothing was **written**.
#[test]
fn a_bootstrap_that_fails_refuses_without_claiming_a_key() {
    let sandbox = Sandbox::new(true);
    // A file where the repository belongs: the path is there, so the existence
    // probe does not report an absence, and initializing it fails.
    fs::create_dir_all(sandbox.repository().parent().expect("a parent"))
        .expect("create the local data root");
    fs::write(sandbox.repository(), b"not a repository").expect("occupy the repository path");

    let output = sandbox.setup(&[]);
    let value = json_of(&output);

    assert_eq!(exit_code(&output), 2, "value={value}");
    assert_eq!(
        value["missing_parameters"],
        serde_json::json!(["masterkey_saved_elsewhere"])
    );
    assert_eq!(value["masterkey"]["kind"], "not_attempted");
    assert!(
        value["masterkey"]["path"].is_null(),
        "no key may be offered when the preparation failed: {value}"
    );
    assert_eq!(value["steps"]["local_save"], "not_attempted");
    assert_eq!(value["steps"]["masterkey"], "not_declared");
    let why = value["masterkey"]["why"].as_str().expect("a why string");
    assert!(
        why.contains("preparing the local repository and masterkey for you to copy failed"),
        "the refusal must name the preparation it could not perform: {why}"
    );
    assert!(
        !why.contains("nothing was written"),
        "the key is persisted before the repository is initialized, so this refusal may \
         not claim the disk is untouched: {why}"
    );
    assert_eq!(
        value["chain"]["kind"], "not_attempted",
        "a failed bootstrap must still not have archived anything: {value}"
    );
}

/// The other half of the WIZ-1 bootstrap contract: once the key exists and the
/// user re-runs with `--masterkey-saved-elsewhere`, the wizard continues — it
/// does **not** mint a fresh key or re-initialise a fresh repository, it adopts
/// the one it just created (local_save `existed`) and records the declaration.
#[test]
fn rerunning_with_the_declaration_after_a_bootstrap_continues() {
    let sandbox = Sandbox::new(true);

    // First run: only the declaration is owed, so the bootstrap creates the
    // local repository + masterkey and stops at exit 2.
    let first_output = sandbox.setup(&[]);
    let first = json_of(&first_output);
    assert_eq!(exit_code(&first_output), 2, "value={first}");
    assert!(sandbox.masterkey().exists());
    // The key the first run minted is the one the re-run must adopt: capture it
    // so a re-mint is visible as a different file, not merely as a `created`.
    let key_before_rerun = fs::read(sandbox.masterkey()).expect("read the bootstrapped key");

    // Re-run with the declaration: the run proceeds to the end, archives the
    // waiting session under the key it was shown, and reports the repository as
    // already existing rather than re-created.
    let output = sandbox.setup(&["--masterkey-saved-elsewhere"]);
    let value = json_of(&output);
    assert_eq!(exit_code(&output), 0, "value={value}");
    assert_eq!(
        fs::read(sandbox.masterkey()).expect("read the key after the re-run"),
        key_before_rerun,
        "the re-run must adopt the key the bootstrap created, not mint a new one"
    );
    assert_eq!(value["steps"]["local_save"], "existed");
    assert_eq!(value["steps"]["masterkey"], "declared");
    assert_eq!(value["healthy"], true);
    // The re-run did not stop at the declaration: it finished the chain the
    // wizard is for, read one session back out of the archive it wrote.
    assert_eq!(value["chain"]["readback"]["kind"], "known");
    assert_eq!(
        value["chain"]["readback"]["sessions"], 1,
        "the re-run must have archived the session that was waiting: {value}"
    );
}

/// The same promise for the *remote*: a non-TTY `setup` that names a destination
/// it has not set up exits 2 with the remote parameter named and writes no local
/// archive either. Before W236 this ran the local pass first and created a
/// repository the refused call was never shown.
#[test]
fn setup_exit_2_from_missing_remote_parameters_writes_nothing() {
    let sandbox = Sandbox::new(true);
    let before = tree_snapshot(sandbox.root.path());

    // A kind named without its parameters: the remote report refused before the
    // local pass, so the repository must not be created.
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
    assert_eq!(value["steps"]["local_save"], "not_attempted");
    assert_eq!(value["destination"]["config"]["kind"], "not_written");
    assert_eq!(value["destination"]["dest_init"]["kind"], "not_run");
    assert!(
        !sandbox.config_file().exists(),
        "an incomplete command line must not write a partial destination block"
    );
    assert!(
        !sandbox.repository().exists(),
        "the local pass must not run when a remote parameter is missing on a \
         non-TTY run: {}",
        sandbox.repository().display()
    );

    let after = tree_snapshot(sandbox.root.path());
    assert_eq!(
        after, before,
        "an exit-2 run for a missing remote parameter must leave the tree untouched: {value}"
    );
}

/// Rule 1 wins whenever anything else is missing alongside the masterkey
/// declaration: the write-nothing promise holds in full, and the WIZ-1
/// bootstrap (which is key-creation, not a write) must not run either. Here the
/// declaration is owed *and* a destination is named without a kind, so both
/// parameters are missing — the director's "if other parameters are ALSO
/// missing, refuse before any write" sentence, pinned to the tree.
#[test]
fn rule_1_wins_when_the_masterkey_declaration_and_a_remote_parameter_are_both_missing() {
    let sandbox = Sandbox::new(true);
    let before = tree_snapshot(sandbox.root.path());

    // No `--masterkey-saved-elsewhere` (declaration owed) and no remote kind
    // for the named destination: neither is supplied, so the bootstrap must not
    // create the very key the refused run never got a declaration for.
    let output = sandbox.setup(&["--destination", "nowhere"]);
    let value = json_of(&output);

    assert_eq!(exit_code(&output), 2, "value={value}");
    assert_eq!(
        value["missing_parameters"],
        serde_json::json!(["masterkey_saved_elsewhere", "remote"])
    );
    assert_eq!(value["steps"]["local_save"], "not_attempted");
    assert!(
        !sandbox.repository().exists(),
        "the bootstrap must not run when a second parameter is also missing"
    );
    assert!(
        !sandbox.masterkey().exists(),
        "no key may be created when the run refused before any write"
    );

    let after = tree_snapshot(sandbox.root.path());
    assert_eq!(
        after, before,
        "when rule 1 wins the whole tree must be untouched, bootstrap or not: {value}"
    );
}

#[cfg(unix)]
#[test]
fn setup_installs_scheduler_checks_run_once_and_reports_no_false_next_run() {
    let sandbox = Sandbox::new(true);
    let scheduler = sandbox.root.path().join("fake-scheduler");
    let calls = sandbox.root.path().join("scheduler-calls");
    let state = sandbox.root.path().join("scheduler-active");
    // What the fake scheduler answers when it is asked for a next run: the shape
    // systemd prints, three days from now, so no fixture here carries a date that
    // has already passed by the time the suite runs. Both sides of the assertion
    // below are this one string — the probe's job is to pass the scheduler's own
    // text through, and never to recompute it from the interval it was given.
    let armed = format!(
        "{} 03:17:00 UTC",
        (chrono::Utc::now() + chrono::Days::new(3)).format("%a %Y-%m-%d")
    );
    let script = if cfg!(target_os = "macos") {
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\necho scheduler-noise\necho scheduler-error >&2\ncase \"$1\" in\nprint) test -f '{}' ;;\nbootstrap) touch '{}' ;;\nbootout) rm -f '{}' ;;\nesac\n",
            calls.display(), state.display(), state.display(), state.display()
        )
    } else {
        // `$3` is the verb of `systemctl --user --no-pager status <timer>`,
        // the one call whose output the next-run probe reads. It answers with a
        // `Trigger:` line and the relative-time tail that must not reach the
        // value: the probe must pass the scheduler's own text through, never
        // recompute it from the interval it was given.
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\necho scheduler-noise\necho scheduler-error >&2\ncase \"$2\" in\nis-active) test -f '{}' ;;\nenable) touch '{}' ;;\ndisable) rm -f '{}' ;;\nesac\ncase \"$3\" in\nstatus) echo '    Trigger: {armed}; 3 days left' ;;\nesac\n",
            calls.display(), state.display(), state.display(), state.display()
        )
    };
    test_support::plant_executable(&scheduler, &script);

    let scheduled_binary = sandbox.home().join(".local/bin/chat-stasher");
    fs::create_dir_all(scheduled_binary.parent().expect("binary parent"))
        .expect("create installed binary directory");
    // The CLI under test execs this copy as its self-check, and the copy is
    // ours to make: plant it through a child so this process holds no write
    // descriptor on a file that is about to be exec'd.
    test_support::copy_executable(
        Path::new(env!("CARGO_BIN_EXE_chat-stasher")),
        &scheduled_binary,
    );

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
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&sandbox.home()),
            )
            .env("XDG_CONFIG_HOME", sandbox.root.path().join("config"))
            .env("XDG_DATA_HOME", sandbox.root.path().join("data"))
            .env("XDG_STATE_HOME", sandbox.root.path().join("state"))
            .env("XDG_CACHE_HOME", sandbox.root.path().join("rh-cache"))
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
                serde_json::json!(armed.as_str())
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

/// W287 §3, on the platform it is about.
///
/// Windows has no scheduler this build integrates with, so the wizard's
/// scheduler step is decided *before* it is attempted: the run must report the
/// step `not_attempted` rather than `failed`, name `schedule` as an unfinished
/// step and exit non-zero. The archive is left without a timer, which is a fact
/// and not a failure of the archive itself — and the distinction is the whole
/// point, because "the platform refused" and "the install ran and broke" are
/// different things to tell a reader.
///
/// `#[cfg(windows)]` is deliberate and two-sided: the property exists only on
/// Windows (the arm exists because `schedule` refuses there), the other arm —
/// the step runs and reports `installed_and_checked` — is asserted by
/// `setup_installs_scheduler_checks_run_once_and_reports_no_false_next_run`
/// above, and the decision reads `std::env::consts::OS`, which no test can set.
/// That leaves this test as the only place the arm can execute at all; the
/// `windows-latest` cell in `.github/workflows/ci.yml` runs `cargo test`, so it
/// does. The interactive half of the step — the suppressed question and the
/// summary line that replaces it — needs a pty and is not reachable here.
#[cfg(windows)]
#[test]
fn a_windows_wizard_scheduler_step_is_not_attempted_and_the_run_is_incomplete() {
    let sandbox = Sandbox::new(true);
    let output = sandbox.setup(&[
        "--masterkey-saved-elsewhere",
        "--install-schedule",
        "--json",
    ]);
    let value = json_of(&output);
    assert_eq!(
        value["steps"]["schedule"], "not_attempted",
        "a platform that refused before anything ran did not fail: {value}"
    );
    assert_eq!(value["schedule"]["status"], "not_attempted");
    assert_eq!(value["schedule"]["requested"], serde_json::json!(true));
    assert_eq!(
        value["schedule"]["next_run"],
        serde_json::Value::Null,
        "nothing was installed, so there is no next run: {value}"
    );
    assert!(
        value["schedule"]["next_run_note"]
            .as_str()
            .is_some_and(|note| note.contains("no next run")),
        "an empty next run arrives with the sentence saying why: {value}"
    );
    assert!(
        value["incomplete"]
            .as_array()
            .is_some_and(|steps| steps.iter().any(|step| step == "schedule")),
        "the run must name the step it did not finish: {value}"
    );
    assert_eq!(
        exit_code(&output),
        1,
        "an unfinished step is 1, never a success: {value}"
    );

    // A refusal writes nothing: no unit files for a manager this platform does
    // not have, and no plists for the one it does not have either.
    assert!(
        !sandbox.home().join(".config").join("systemd").exists(),
        "the Windows refusal must not leave systemd unit files behind (the defect it \
         replaced wrote them, could not load them, and let status call them installed)"
    );
    assert!(
        !sandbox.home().join("Library").exists(),
        "nor is anything launchd's to install on Windows"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("docs/schedule.md#windows-a-task-in-task-scheduler"),
        "the refusal has to point at the manual steps that replace it: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn setup_self_check_uses_the_installed_binary_selected_from_a_build_artifact() {
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
    test_support::plant_executable(&scheduler, &script);

    let installed_binary = sandbox.home().join(".local/bin/chat-stasher");
    fs::create_dir_all(installed_binary.parent().expect("binary parent"))
        .expect("create installed binary directory");
    // The CLI execs this to prove the self-check refuses a failing binary, so it
    // must be reachable to `exec` and not merely present.
    test_support::plant_executable(&installed_binary, "#!/bin/sh\nexit 1\n");

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
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&sandbox.home()),
        )
        .env("XDG_CONFIG_HOME", sandbox.root.path().join("config"))
        .env("XDG_DATA_HOME", sandbox.root.path().join("data"))
        .env("XDG_STATE_HOME", sandbox.root.path().join("state"))
        .env("XDG_CACHE_HOME", sandbox.root.path().join("rh-cache"))
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
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(home.path()),
        )
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
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(home.path()),
        )
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

/// How many snapshots a destination repository holds. `get_all_snapshots`
/// loads every snapshot file, so this counts what is really there.
fn destination_snapshots_with(sandbox: &Sandbox, repo: &Path, key: &Path) -> usize {
    let cfg = chat_stasher::store::StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: std::collections::BTreeMap::new(),
        // W289: the cache stays on — a configured destination read is a cached
        // read — but it lives under this sandbox, never in the user's cache.
        cache_dir: Some(sandbox.root.path().join("destination-cache")),
        no_cache: false,
    };
    let mk = chat_stasher::store::load_key_file(&cfg).expect("read the destination key");
    let backends = chat_stasher::store::BackupStore::for_metadata_query(cfg.clone())
        .backends()
        .expect("destination backends");
    let (repo, _adoption) = chat_stasher::orphans::open_adopting(&cfg, &backends, &mk)
        .expect("open the destination repository");
    repo.get_all_snapshots()
        .expect("list destination snapshots")
        .len()
}

/// W292 (OBS-5): re-running the wizard must not grow the destination.
///
/// The wizard runs `dest-init` on every invocation that names a destination,
/// and its own declaration step makes a re-run likely — so before the fix each
/// run appended a snapshot to a destination that already held everything.
/// W291 measured it on two machines (three runs, three snapshots) and this is
/// its local replay. The first run is the one that seeds the destination, so
/// the count is anchored to it rather than to a hard-coded 1.
#[test]
fn re_running_the_wizard_does_not_grow_the_destination() {
    let sandbox = Sandbox::new(true);
    let remote = sandbox.fake_remote();
    sandbox.write_config(&format!(
        "[destinations.fake]\nrepo = '{}'\nkey_file = '{}'\n",
        remote.display(),
        sandbox.masterkey().display()
    ));

    // The first run seeds the destination. The destination reuses this
    // machine's key file, so no fresh key is created and no declaration is
    // owed: both runs complete.
    let first = sandbox.setup(&["--destination", "fake", "--masterkey-saved-elsewhere"]);
    let value = json_of(&first);
    assert_eq!(exit_code(&first), 0, "value={value}");
    assert_eq!(value["steps"]["destination"], "reachable");
    assert_eq!(value["destination"]["dest_init"]["kind"], "ran");
    let after_first = destination_snapshots_with(&sandbox, &remote, &sandbox.masterkey());
    assert_eq!(
        after_first, 1,
        "the first setup must seed the destination exactly once"
    );

    // The re-run has nothing new to archive: the collector found the same one
    // session, the stage already holds its shard, and the destination already
    // holds what would be published.
    let second = sandbox.setup(&["--destination", "fake", "--masterkey-saved-elsewhere"]);
    let value = json_of(&second);
    assert_eq!(exit_code(&second), 0, "value={value}");
    assert_eq!(value["destination"]["dest_init"]["kind"], "ran");
    assert_eq!(
        destination_snapshots_with(&sandbox, &remote, &sandbox.masterkey()),
        after_first,
        "a second setup over an unchanged stage grew the destination; it already holds \
         what would be published, so nothing may be published: {value}"
    );
}

/// The key files the wizard names for the user to back up: every entry of the
/// `masterkey.keys` array, falling back to `masterkey.path` (the local key)
/// when the array is absent — the pre-fix shape that named only the local key
/// and so could not recover a destination on a second machine (W281 BUG-2).
fn wizard_key_paths(value: &serde_json::Value) -> Vec<PathBuf> {
    match value["masterkey"]["keys"].as_array() {
        Some(keys) => keys
            .iter()
            .filter_map(|key| key["path"].as_str())
            .map(PathBuf::from)
            .collect(),
        None => value["masterkey"]["path"]
            .as_str()
            .map(|path| vec![PathBuf::from(path)])
            .unwrap_or_default(),
    }
}

/// W281 BUG-2's acceptance, as an assertion: a user who follows the wizard
/// literally — copies every key file it names, then runs the same command again
/// — can restore those files on a second machine and read the destination back.
/// The destination is an ordinary local-path repo seeded with this machine's
/// history, so the round trip needs no network; the only thing that separates
/// "I can recover" from BUG-2's hard `exit 3` is that the wizard names the
/// destination's own key.
///
/// The two runs are the literal flow, not a convenience: the destination's key
/// does not exist until the run that initialises the destination creates it, so
/// that run cannot declare it — it stops with `2` and names the file it just
/// created. A later run, on a machine where the file is already there, may.
#[test]
fn a_second_machine_recovers_a_destination_using_every_key_the_wizard_named() {
    // Machine A: one session, one adopted destination ("backup") whose repo is
    // a shared directory. The destination has its own key, the default
    // `masterkey-backup.json`, separate from the local one.
    let a = Sandbox::new(true);
    let shared = a.root.path().join("shared-archive");
    a.write_config(&format!(
        "[destinations.backup]\nrepo = '{}'\n",
        shared.display()
    ));

    // The first run cannot declare a key that does not exist yet, and the
    // destination's key is one of those: `dest-init` creates it during this
    // run, so at the moment the flag was given there was no file to copy. The
    // run therefore stops with `2` — the declaration is still owed — and names
    // the file it just created, which is the whole point of stopping.
    let aout = a.setup(&["--destination", "backup", "--masterkey-saved-elsewhere"]);
    let value = json_of(&aout);
    assert_eq!(
        exit_code(&aout),
        2,
        "the destination's key did not exist when the flag was given, so the declaration \
         cannot have been made: {value}"
    );
    assert_eq!(
        value["missing_parameters"],
        serde_json::json!(["masterkey_saved_elsewhere"]),
        "the declaration is the one thing still owed: {value}"
    );
    assert_eq!(
        value["destination"]["dest_init"]["exit_code"], 0,
        "the key exists because this run created it: {value}"
    );

    // Follow the wizard's instructions literally: back up every key it names.
    let named = wizard_key_paths(&value);
    assert!(
        named
            .iter()
            .any(|path| path.file_name() == Some(std::ffi::OsStr::new("masterkey-backup.json"))),
        "the wizard must name the destination's own key so the user knows to copy it; \
         named: {named:?}"
    );
    assert!(
        named
            .iter()
            .any(|path| path.file_name() == Some(std::ffi::OsStr::new("masterkey.json"))),
        "the wizard must still name the local key; named: {named:?}"
    );
    let created = value["masterkey"]["keys"]
        .as_array()
        .expect("the wizard names its keys as an array")
        .iter()
        .find(|key| key["scope"] == "destination")
        .expect("the destination's key is one of the named keys");
    assert_eq!(
        created["declared"], false,
        "a key this run created cannot be declared: nobody has had it to copy: {value}"
    );
    let tape = a.root.path().join("backup-tape");
    fs::create_dir_all(&tape).expect("the tape dir");
    for path in &named {
        fs::copy(path, tape.join(path.file_name().unwrap())).expect("copy the named key");
    }

    // The user copies the files, then runs the same command again. Now every
    // key already exists on the disk, so the declaration covers them and the
    // run finishes.
    let aout = a.setup(&["--destination", "backup", "--masterkey-saved-elsewhere"]);
    let value = json_of(&aout);
    assert_eq!(
        exit_code(&aout),
        0,
        "every named key is on the disk now, so the declaration can be recorded: {value}"
    );
    assert_eq!(value["destination"]["dest_init"]["exit_code"], 0);

    // Machine B: a fresh machine that only adopts the same destination, and
    // whose only connection to A's archive is the keys put back at the same
    // paths. B has no local repo and no stage.
    let b = Sandbox::new(false);
    b.write_config(&format!(
        "[destinations.backup]\nrepo = '{}'\n",
        shared.display()
    ));
    fs::create_dir_all(b.data_root()).expect("the data root where keys are restored");
    for key in &named {
        let name = key.file_name().expect("a named key file");
        fs::copy(tape.join(name), b.data_root().join(name)).expect("restore the key");
    }

    // Read the destination back on B: this is the recovery the promise is about.
    let bout = b.command(&["search", "--destination", "backup", "--json"]);
    let bvalue = json_of(&bout);
    assert_eq!(
        exit_code(&bout),
        0,
        "a second machine with the wizard-named keys must read the destination, not exit 3: \
         search failed with {bvalue}"
    );

    // Each named key says which copy it opens and whether the user declared it.
    // The declaration is a statement, never a verification, and the object has
    // to keep those two apart.
    let keys = value["masterkey"]["keys"]
        .as_array()
        .expect("the wizard names its keys as an array");
    let destination_key = keys
        .iter()
        .find(|key| key["scope"] == "destination")
        .expect("the destination's key is one of the named keys");
    assert_eq!(destination_key["name"], "backup");
    assert_eq!(destination_key["declared"], true);
    assert_eq!(
        destination_key["declaration_is_verified"], false,
        "a declaration is a statement by the user, never something this tool checked"
    );

    // `doctor` repeats it from the other side: which keys this machine holds,
    // and which of them the user has declared saved. The destination key is a
    // separate row from the local one because it opens a separate copy.
    let doctor = a.command(&["doctor", "--json"]);
    let dvalue = json_of(&doctor);
    let copies = dvalue["keys"]["copies"]
        .as_array()
        .expect("doctor reports the key inventory");
    let reported = copies
        .iter()
        .find(|row| row["name"] == "backup")
        .unwrap_or_else(|| panic!("doctor must report the destination's key: {dvalue}"));
    assert_eq!(reported["scope"], "destination");
    assert_eq!(reported["exists"], true, "the wizard just created it here");
    assert_eq!(reported["declared_saved"], true, "the run declared it");
    assert!(
        copies.iter().any(|row| row["scope"] == "local"),
        "the local key is a row of its own: {dvalue}"
    );
}

/// The other half of the same requirement, on the machine where it matters:
/// `status` and `doctor` must say when a destination's key has **no declared
/// backup**, because that is the state in which a lost machine loses that copy.
///
/// The fixture plants a destination key file directly rather than running the
/// wizard: the point is the reporting of a key that exists here with nothing
/// declared about it, which the wizard's own run cannot produce (it declares as
/// it creates). The local key is deliberately absent — a machine that has not
/// archived anything yet has none, and that is not a destination's problem.
#[test]
fn an_undeclared_destination_key_is_named_by_status_and_doctor() {
    let sandbox = Sandbox::new(false);
    let shared = sandbox.root.path().join("shared-archive");
    sandbox.write_config(&format!(
        "[destinations.backup]\nrepo = '{}'\n",
        shared.display()
    ));
    fs::create_dir_all(sandbox.data_root()).expect("the data root the key lives in");
    let destination_key = sandbox.data_root().join("masterkey-backup.json");
    fs::write(
        &destination_key,
        b"opaque fixture bytes, never read as a key\n",
    )
    .expect("plant the destination key file");

    // `status` says it, and says which file and what to do — this is the line a
    // user who never finished the wizard actually reads.
    let status = sandbox.command(&["status"]);
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        stderr.contains("[keys] destination=backup"),
        "status must name the destination whose key has no declared backup; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&destination_key.display().to_string()),
        "the line must carry the path the user has to copy; stderr:\n{stderr}"
    );

    // `doctor` reports the same fact in full, including the local copy that
    // `status` stays quiet about.
    let doctor = sandbox.command(&["doctor", "--json"]);
    let dvalue = json_of(&doctor);
    let copies = dvalue["keys"]["copies"]
        .as_array()
        .expect("doctor reports the key inventory");
    let reported = copies
        .iter()
        .find(|row| row["name"] == "backup")
        .unwrap_or_else(|| panic!("doctor must report the destination's key: {dvalue}"));
    assert_eq!(reported["exists"], true);
    assert_eq!(
        reported["declared_saved"], false,
        "nothing declared this key saved, so it must not read as backed up"
    );
    let local = copies
        .iter()
        .find(|row| row["scope"] == "local")
        .expect("doctor reports the local copy even when it has no key yet");
    assert_eq!(
        local["exists"], false,
        "no repository and no key exist on this fixture; that is an honest absence, not a row \
         omitted"
    );
}

/// The state directory the sandbox's `chat-stasher` writes its own records to
/// (`$XDG_DATA_HOME/chat-stasher/state`), which is where a key declaration is
/// recorded and read back from.
fn state_dir(sandbox: &Sandbox) -> PathBuf {
    sandbox
        .root
        .path()
        .join("data")
        .join("chat-stasher")
        .join("state")
}

/// A declaration is about a **file**, not about a name.
///
/// The record the user made is a statement about the key file they were shown:
/// that path, holding those bytes. A destination's `key_file` can change — the
/// block is a hand-edited file — and a declaration made for the old file says
/// nothing about the new one. Reporting it as `declared_saved` would be the
/// false backup this whole surface exists to prevent: the user is told a copy
/// of *this* key exists when the statement was about a different file that this
/// machine no longer holds.
#[test]
fn a_declaration_made_about_another_key_file_does_not_cover_this_one() {
    let sandbox = Sandbox::new(false);
    let shared = sandbox.root.path().join("shared-archive");
    sandbox.write_config(&format!(
        "[destinations.backup]\nrepo = '{}'\n",
        shared.display()
    ));
    fs::create_dir_all(sandbox.data_root()).expect("the data root the key lives in");
    // The key this machine holds now, and the older file the declaration was
    // actually made about.
    let destination_key = sandbox.data_root().join("masterkey-backup.json");
    fs::write(&destination_key, b"the current key file, never declared\n")
        .expect("plant the current destination key");
    let older_key = sandbox.data_root().join("masterkey-backup-old.json");
    fs::write(&older_key, b"the key file the declaration was made about\n")
        .expect("plant the older destination key");
    chat_stasher::keydecl::mark_declared(
        &state_dir(&sandbox),
        &chat_stasher::keydecl::destination_scope("backup"),
        &older_key,
    )
    .expect("record the declaration the user made, about the older file");

    let doctor = sandbox.command(&["doctor", "--json"]);
    let dvalue = json_of(&doctor);
    let copies = dvalue["keys"]["copies"]
        .as_array()
        .expect("doctor reports the key inventory");
    let reported = copies
        .iter()
        .find(|row| row["name"] == "backup")
        .unwrap_or_else(|| panic!("doctor must report the destination's key: {dvalue}"));
    assert_eq!(
        reported["path"],
        destination_key.display().to_string(),
        "the row is about the key this machine holds now: {dvalue}"
    );
    assert_eq!(
        reported["declared_saved"], false,
        "the declaration was made about a different file, so this key is not declared \
         saved: {dvalue}"
    );

    // `status` says the same, because the user who reads only `status` is the
    // one who has to be told to copy the file they actually hold.
    let status = sandbox.command(&["status"]);
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        stderr.contains("[keys] destination=backup"),
        "status must name the destination whose current key has no declared backup; \
         stderr:\n{stderr}"
    );
}

/// The other half of the same rule: the file at that path has to be the file
/// the declaration was made about.
///
/// A key deleted and re-created at the same path — the wizard re-run after the
/// user lost or removed it — is a *different* key. A record that matched on the
/// path alone would report the new key as backed up because the old one was,
/// which is the one direction this must never get wrong.
#[test]
fn a_declaration_does_not_cover_a_key_re_created_at_the_same_path() {
    let sandbox = Sandbox::new(false);
    let shared = sandbox.root.path().join("shared-archive");
    sandbox.write_config(&format!(
        "[destinations.backup]\nrepo = '{}'\n",
        shared.display()
    ));
    fs::create_dir_all(sandbox.data_root()).expect("the data root the key lives in");
    let destination_key = sandbox.data_root().join("masterkey-backup.json");
    fs::write(&destination_key, b"the key the user copied elsewhere\n")
        .expect("plant the destination key");
    chat_stasher::keydecl::mark_declared(
        &state_dir(&sandbox),
        &chat_stasher::keydecl::destination_scope("backup"),
        &destination_key,
    )
    .expect("record the declaration about the file that is there");

    // The user loses that key and the wizard creates a new one at the same
    // path. The bytes change; the path does not.
    fs::write(
        &destination_key,
        b"a different key, created after the copy was made\n",
    )
    .expect("replace the destination key");

    let doctor = sandbox.command(&["doctor", "--json"]);
    let dvalue = json_of(&doctor);
    let copies = dvalue["keys"]["copies"]
        .as_array()
        .expect("doctor reports the key inventory");
    let reported = copies
        .iter()
        .find(|row| row["name"] == "backup")
        .unwrap_or_else(|| panic!("doctor must report the destination's key: {dvalue}"));
    assert_eq!(
        reported["declared_saved"], false,
        "a declaration about the old key is not a declaration about the new one at the same \
         path: {dvalue}"
    );

    let status = sandbox.command(&["status"]);
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        stderr.contains("[keys] destination=backup"),
        "status must name the key whose backup nobody has confirmed; stderr:\n{stderr}"
    );
}

/// A key that exists but cannot be read — the W284 review's replaced key: a
/// directory sitting at the key's path — must not be reported as declared
/// saved.
///
/// Before this fix the declaration comparison folded every read error into the
/// "file was deleted" branch, which keeps the declaration standing: a
/// destination whose key file was replaced by something unreadable read as
/// backed up on the strength of a record made about bytes nobody can read
/// back. That is unknown, and the point of the three-state rule is that it is
/// not worded as "no declaration" either — a user with the declaration on file
/// must not be sent looking for a step they already did.
#[test]
fn a_key_replaced_by_an_unreadable_file_is_unknown_not_declared_saved() {
    let sandbox = Sandbox::new(false);
    let shared = sandbox.root.path().join("shared-archive");
    sandbox.write_config(&format!(
        "[destinations.backup]\nrepo = '{}'\n",
        shared.display()
    ));
    fs::create_dir_all(sandbox.data_root()).expect("the data root the key lives in");
    let destination_key = sandbox.data_root().join("masterkey-backup.json");
    fs::write(&destination_key, b"the key the user copied elsewhere\n")
        .expect("plant the destination key");
    chat_stasher::keydecl::mark_declared(
        &state_dir(&sandbox),
        &chat_stasher::keydecl::destination_scope("backup"),
        &destination_key,
    )
    .expect("record the declaration about the file that is there");

    // Something unreadable replaces the key at the exact path the declaration
    // names. The path still leads somewhere, so absence is not the finding.
    fs::remove_file(&destination_key).expect("remove the key file");
    fs::create_dir(&destination_key).expect("something unreadable now sits at that path");

    let doctor = sandbox.command(&["doctor", "--json"]);
    let dvalue = json_of(&doctor);
    let copies = dvalue["keys"]["copies"]
        .as_array()
        .expect("doctor reports the key inventory");
    let reported = copies
        .iter()
        .find(|row| row["name"] == "backup")
        .unwrap_or_else(|| panic!("doctor must report the destination's key: {dvalue}"));
    assert_eq!(
        reported["exists"], true,
        "a path exists — the finding is that it cannot be read, not that it is gone: {dvalue}"
    );
    assert_eq!(
        reported["declared_saved"], false,
        "an unreadable file cannot be confirmed as the declared key, so it must not read as \
         declared saved: {dvalue}"
    );
    assert_eq!(
        reported["declared_state"], "unreadable",
        "the unknown is its own state, not a plain 'not declared': {dvalue}"
    );

    // `status` words it as an unknown of its own, for the same reason: the
    // "here but no saved copy was ever declared" line would be false — a copy
    // may well have been declared, and this machine cannot say.
    let status = sandbox.command(&["status"]);
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        stderr.contains("[keys] destination=backup"),
        "status must name the destination whose key cannot be read; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("unreadable") && stderr.contains("unknown whether"),
        "status must word the unreadable key as unknown rather than as an undeclared one; \
         stderr:\n{stderr}"
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
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&sandbox.home()),
        )
        .env("XDG_CONFIG_HOME", sandbox.root.path().join("config"))
        .env("XDG_DATA_HOME", sandbox.root.path().join("data"))
        .env("XDG_STATE_HOME", sandbox.root.path().join("state"))
        .env("XDG_CACHE_HOME", sandbox.root.path().join("rh-cache"))
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
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&sandbox.home()),
        )
        .env("XDG_CONFIG_HOME", sandbox.root.path().join("config"))
        .env("XDG_DATA_HOME", sandbox.root.path().join("data"))
        .env("XDG_STATE_HOME", sandbox.root.path().join("state"))
        .env("XDG_CACHE_HOME", sandbox.root.path().join("rh-cache"))
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
    let before = tree_snapshot(sandbox.root.path());
    let output = sandbox.setup(&["--destination", "nowhere", "--masterkey-saved-elsewhere"]);
    let value = json_of(&output);

    assert_eq!(exit_code(&output), 2, "value={value}");
    assert_eq!(value["missing_parameters"], serde_json::json!(["remote"]));
    assert_eq!(value["steps"]["local_save"], "not_attempted");
    assert_eq!(value["destination"]["config"]["kind"], "not_written");
    assert!(
        !sandbox.config_file().exists(),
        "nothing may be written when the kind is unknown"
    );
    let after = tree_snapshot(sandbox.root.path());
    assert_eq!(
        after, before,
        "an undeclared destination with no kind must leave the whole tree untouched, and must \
         not bootstrap the local repository either: {value}"
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
