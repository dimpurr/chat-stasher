//! B69 — an installed Codex store whose sessions cannot be counted is not empty.
//!
//! The fixtures are metadata-only: the unknown case is a temporary regular file
//! and the known-empty case is a temporary directory. No real harness path or
//! session body is read.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn write_registry(sandbox: &Path, template: &str) -> PathBuf {
    let cell = format!(
        r#"{{
            "template": "{template}",
            "env_override": "CODEX_HOME",
            "format": "jsonl / jsonl.zst",
            "confidence": "源码确认",
            "source": "B69 synthetic fixture"
        }}"#
    );
    let registry = sandbox.join("registry.json");
    fs::write(
        &registry,
        format!(
            r#"{{
                "schema_version": 1,
                "generated": "B69 synthetic fixture",
                "harnesses": [{{
                    "id": "codex",
                    "display_name": "synthetic codex",
                    "paths": {{
                        "macos": {cell},
                        "linux": {cell},
                        "windows": {cell}
                    }}
                }}]
            }}"#
        ),
    )
    .unwrap();
    registry
}

fn run_doctor(home: &Path, registry: &Path, codex_home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .arg("doctor")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("CHAT_STASHER_REGISTRY", registry)
        .env("CODEX_HOME", codex_home)
        .env_remove("OPENCODE_DB")
        .output()
        .unwrap()
}

#[test]
fn installed_but_unenumerable_codex_is_not_printed_as_zero() {
    let sandbox = tempfile::tempdir().unwrap();
    let home = sandbox.path().join("home");
    let codex_home = sandbox.path().join("codex-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&codex_home).unwrap();

    // A regular file makes the probe's target exist, but gives the directory
    // footprint no enumerable sessions. This is the installed + None shape.
    fs::write(codex_home.join("target.db"), []).unwrap();
    let registry = write_registry(sandbox.path(), "~/.codex/target.db");
    let output = run_doctor(&home, &registry, &codex_home);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "doctor failed: exit={:?}\nstderr={stderr}",
        output.status.code()
    );
    assert!(
        stderr.contains("(unknown rollouts on this machine all uncompressed)"),
        "installed + unknown must say it is unknown; stderr={stderr}"
    );
    assert!(
        !stderr.contains("(0 rollouts on this machine all uncompressed)"),
        "unknown must not be presented as the reassuring zero; stderr={stderr}"
    );
}

#[test]
fn installed_and_known_empty_codex_still_prints_zero() {
    let sandbox = tempfile::tempdir().unwrap();
    let home = sandbox.path().join("home");
    let codex_home = sandbox.path().join("codex-home");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(codex_home.join("sessions")).unwrap();

    let registry = write_registry(sandbox.path(), "~/.codex/sessions/");
    let output = run_doctor(&home, &registry, &codex_home);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "doctor failed: exit={:?}\nstderr={stderr}",
        output.status.code()
    );
    assert!(
        stderr.contains("(0 rollouts on this machine all uncompressed)"),
        "a genuinely enumerated empty directory must remain zero; stderr={stderr}"
    );
}
