//! B65 regression, re-expressed for ADR-018: the archive partition is never
//! derived from the hostname, so a machine with no hostname tool and no
//! `HOSTNAME` neither falls back to a real-looking `localhost` partition nor
//! stalls — a **missing identity file** gets a fresh random 32-hex identity
//! generated and persisted, and an **unusable** identity file hard-fails with
//! a "do not delete this file" instruction instead of being replaced.
//!
//! All cases run with an empty synthetic registry and isolated XDG/HOME
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
fn missing_identity_generates_a_partition_never_localhost() {
    let sandbox = tempfile::tempdir().unwrap();
    let output = run_collect(sandbox.path(), None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    assert_eq!(
        output.status.code(),
        Some(0),
        "a missing identity must be generated so collection proceeds; stdout={stdout} stderr={stderr}"
    );
    assert!(
        combined.contains("generated a new machine identity"),
        "the generation must be visible; stdout={stdout} stderr={stderr}"
    );
    let machine_line = stdout
        .lines()
        .find(|l| l.starts_with("[collect] machine"))
        .expect("collect must print the machine line")
        .to_string();
    let machine = machine_line.split(':').nth(1).unwrap().trim();
    assert_eq!(
        machine.len(),
        32,
        "the partition must be a 32-hex identity: {machine_line}"
    );
    assert!(
        machine.chars().all(|c| c.is_ascii_hexdigit()),
        "the partition must be hex: {machine_line}"
    );
    assert!(
        !combined.contains("localhost"),
        "an identity is never a localhost partition; stdout={stdout} stderr={stderr}"
    );
    let id_path = sandbox.path().join("data/chat-stasher/machine-identity");
    assert_eq!(
        std::fs::read_to_string(&id_path).unwrap().trim(),
        machine,
        "the generated identity must be persisted where the next run reads it"
    );
}

#[test]
fn unusable_identity_hard_fails_without_replacing_file() {
    let sandbox = tempfile::tempdir().unwrap();
    let id_path = sandbox.path().join("data/chat-stasher/machine-identity");
    fs::create_dir_all(id_path.parent().unwrap()).unwrap();
    let garbage = "not-32-hex-garbage";
    fs::write(&id_path, garbage).unwrap();

    let output = run_collect(sandbox.path(), None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(3),
        "an unusable identity must hard-fail, never generate a replacement; stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.contains("do not delete"),
        "the user must be told not to delete the identity file; stderr={stderr}"
    );
    assert!(
        !stderr.contains("generated a new machine identity"),
        "an unusable identity must not be silently replaced; stderr={stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(&id_path).unwrap(),
        garbage,
        "an unusable identity must remain untouched on disk"
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
