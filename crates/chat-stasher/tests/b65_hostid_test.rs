//! B65 regression: an unavailable hostname must not become the real-looking
//! `localhost` archive partition.
//!
//! Both cases run with an empty synthetic registry and isolated XDG/HOME
//! directories. The child PATH contains no `hostname` executable, and
//! `HOSTNAME` is removed; no harness directory or session body is touched.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run_collect(sandbox: &Path, machine: Option<&str>) -> Output {
    let home = sandbox.join("home");
    let no_commands = sandbox.join("no-commands");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&no_commands).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"B65 synthetic","harnesses":[]}"#,
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .args(["collect", "--stage"])
        .arg(sandbox.join("stage"))
        .env("PATH", &no_commands)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .env_remove("HOSTNAME");
    if let Some(machine) = machine {
        command.args(["--machine", machine]);
    }
    command.output().unwrap()
}

#[test]
fn missing_machine_identity_is_reported_without_localhost_fallback() {
    let sandbox = tempfile::tempdir().unwrap();
    let output = run_collect(sandbox.path(), None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    assert_eq!(
        output.status.code(),
        Some(3),
        "identity-dependent collection did not start; stdout={stdout} stderr={stderr}"
    );
    assert!(
        combined.contains("machine identity unavailable"),
        "the unresolved state must be visible; stdout={stdout} stderr={stderr}"
    );
    assert!(
        combined.contains("--machine <name>"),
        "the user must be told how to choose the partition; stdout={stdout} stderr={stderr}"
    );
    assert!(
        !combined.contains("localhost"),
        "unavailable identity must never be rendered as localhost; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn explicit_machine_still_collects_when_hostname_is_unavailable() {
    let sandbox = tempfile::tempdir().unwrap();
    let output = run_collect(sandbox.path(), Some("b65-explicit"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(0),
        "explicit machine selection must keep the normal path; stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("[collect] machine         : b65-explicit"),
        "the selected partition must be used; stdout={stdout}"
    );
    assert!(!stdout.contains("localhost"), "stdout={stdout}");
    assert!(
        !stderr.contains("machine identity unavailable"),
        "stderr={stderr}"
    );
}
