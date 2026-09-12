//! ADR-023 — the first connection to a remote destination must be a decision,
//! not a side effect.
//!
//! Two properties are pinned here, both at the CLI boundary because that is
//! where the decision is actually made:
//!
//!   1. Nothing writes `~/.ssh/known_hosts` unless `--trust-host` was passed.
//!      A run that merely *tries* to connect must leave the file exactly as it
//!      found it — including not creating it.
//!   2. `doctor` reports a destination it could not reach as UNKNOWN, never as
//!      empty, and keeps the three states (reached / unreachable /
//!      not configured) distinct.
//!
//! Every endpoint below is a closed port on loopback. Nothing outside this
//! machine is contacted, and no fixture contains session content.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MACHINE: &str = "adr023-fixture";

fn empty_registry(root: &Path) -> PathBuf {
    let path = root.join("registry.json");
    fs::write(
        &path,
        r#"{"schema_version":1,"generated":"ADR-023 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    path
}

/// A destination that will refuse every connection: port 1 on loopback has no
/// listener. `retry = off` keeps the backend from spending its retry budget on
/// a port that is known shut.
fn remote_destination(root: &Path) -> String {
    format!(
        "[destinations.box]\n\
         repo = 'opendal:sftp'\n\
         key_file = '{}'\n\
         \n\
         [destinations.box.options]\n\
         endpoint = 'ssh://127.0.0.1:1'\n\
         user = 'adr023-nobody'\n\
         known_hosts_strategy = 'strict'\n\
         root = '/adr023'\n\
         retry = 'off'\n",
        root.join("box-key.json").display()
    )
}

fn local_destination(root: &Path) -> String {
    format!(
        "[destinations.localbox]\nrepo = '{}'\nkey_file = '{}'\n",
        root.join("local-repo").display(),
        root.join("local-key.json").display()
    )
}

fn write_config(root: &Path, body: &str) {
    let config = root.join("config/chat-stasher/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(config, body).unwrap();
}

fn command(root: &Path, registry: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("CHAT_STASHER_REGISTRY", registry)
        .env_remove("CODEX_HOME")
        .env_remove("OPENCODE_DB");
    command
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn known_hosts_path(root: &Path) -> PathBuf {
    root.join("home/.ssh/known_hosts")
}

/// The whole point of `--trust-host` being opt-in: an unattended run that
/// simply tries to collect must not make a host trusted by trying. Before
/// ADR-023 this run failed later, out of the push, with exit 1 and no advice.
#[test]
fn a_first_connection_never_writes_known_hosts_without_the_flag() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    write_config(root.path(), &remote_destination(root.path()));
    let stage = root.path().join("stage");
    fs::create_dir_all(&stage).unwrap();

    let output = command(root.path(), &registry)
        .args([
            "dest-init",
            "--destination",
            "box",
            "--machine",
            MACHINE,
            "--stage",
        ])
        .arg(&stage)
        .output()
        .unwrap();
    let (stdout, stderr) = text(&output);

    assert_eq!(
        output.status.code(),
        Some(3),
        "an unreachable destination is 'did not finish reading' (3), not a completed failure (1).\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !known_hosts_path(root.path()).exists(),
        "dest-init without --trust-host created known_hosts — the first connection must never \
         trust a host by itself.\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("UNKNOWN, not empty"),
        "the run has to say the destination was not read, not that it is empty.\nstderr:\n{stderr}"
    );
}

/// `--trust-host` is a remote-only action. On a local destination there is no
/// host key, and the command must say so rather than silently doing nothing.
///
/// The assertion deliberately rejects clap's own "unexpected argument" text:
/// on a build without the flag, argument parsing fails with the same exit code
/// 2, so the exit code alone cannot tell "the flag does not exist" apart from
/// "the flag exists and is refused here".
#[test]
fn trust_host_on_a_local_destination_is_a_usage_error() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    write_config(root.path(), &local_destination(root.path()));
    let stage = root.path().join("stage");
    fs::create_dir_all(&stage).unwrap();

    let output = command(root.path(), &registry)
        .args([
            "dest-init",
            "--destination",
            "localbox",
            "--machine",
            MACHINE,
            "--trust-host",
            "--stage",
        ])
        .arg(&stage)
        .output()
        .unwrap();
    let (_stdout, stderr) = text(&output);

    assert_eq!(output.status.code(), Some(2), "stderr:\n{stderr}");
    assert!(
        stderr.contains("--trust-host applies to a remote destination"),
        "the flag must be refused with its own reason.\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("unexpected argument"),
        "`--trust-host` must be a known flag, not rejected by the argument parser.\nstderr:\n{stderr}"
    );
    assert!(
        !known_hosts_path(root.path()).exists(),
        "a refused --trust-host must not write known_hosts.\nstderr:\n{stderr}"
    );
}

/// A destination that did not answer is UNKNOWN. `doctor` says so, names the
/// destination, and still exits 0 — the diagnosis succeeded, the finding is bad.
#[test]
fn doctor_reports_an_unreachable_destination_as_unknown() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    write_config(root.path(), &remote_destination(root.path()));

    let output = command(root.path(), &registry)
        .arg("doctor")
        .output()
        .unwrap();
    let (stdout, stderr) = text(&output);
    let combined = format!("{stdout}{stderr}");

    assert_eq!(
        output.status.code(),
        Some(0),
        "an unreachable destination is a diagnosis with a bad finding, not a failed diagnosis.\nstderr:\n{stderr}"
    );
    assert!(
        combined.contains("D7 · Declared destinations"),
        "doctor did not report on the declared destination.\n{combined}"
    );
    assert!(
        combined.contains("box") && combined.contains("NOT REACHED"),
        "the unreachable destination was not reported.\n{combined}"
    );
    assert!(
        combined.contains("UNKNOWN, not empty"),
        "doctor must not let an unreachable destination read as an empty one.\n{combined}"
    );
}

/// A destination with no `repo` was never dialled, and saying "unreachable"
/// about it would put a config mistake and a dead network in one bucket.
#[test]
fn doctor_says_nothing_was_attempted_for_a_destination_with_no_repo() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    write_config(
        root.path(),
        &format!(
            "[destinations.halfwritten]\nkey_file = '{}'\n",
            root.path().join("k.json").display()
        ),
    );

    let output = command(root.path(), &registry)
        .arg("doctor")
        .output()
        .unwrap();
    let (stdout, stderr) = text(&output);
    let combined = format!("{stdout}{stderr}");

    assert_eq!(output.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(
        combined.contains("NOTHING WAS ATTEMPTED"),
        "a destination with no `repo` must be reported as not configured.\n{combined}"
    );
    assert!(
        !combined.contains("NOT REACHED"),
        "not configured was reported as unreachable.\n{combined}"
    );
}

/// The `--json` shape carries the same three states as distinct `kind` values,
/// so a script can tell them apart without parsing prose.
#[test]
fn doctor_json_keeps_the_three_destination_states_distinct() {
    let root = tempfile::tempdir().unwrap();
    let registry = empty_registry(root.path());
    let mut body = remote_destination(root.path());
    body.push_str(&format!(
        "\n[destinations.halfwritten]\nkey_file = '{}'\n",
        root.path().join("k.json").display()
    ));
    body.push_str(&local_destination(root.path()));
    write_config(root.path(), &body);

    let output = command(root.path(), &registry)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&output);
    assert_eq!(output.status.code(), Some(0), "stderr:\n{stderr}");

    let value: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("doctor --json: {e}\n{stdout}"));
    let destinations = value["destinations"]
        .as_array()
        .unwrap_or_else(|| panic!("no destinations array in {stdout}"));

    let kind_of = |name: &str| -> String {
        destinations
            .iter()
            .find(|d| d["name"] == serde_json::json!(name))
            .unwrap_or_else(|| panic!("destination `{name}` missing from {stdout}"))["kind"]
            .as_str()
            .unwrap()
            .to_string()
    };

    assert_eq!(kind_of("box"), "unreachable");
    assert_eq!(kind_of("halfwritten"), "not_configured");
    assert_eq!(kind_of("localbox"), "repository_absent");
}
