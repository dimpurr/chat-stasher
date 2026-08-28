//! B63 regression: an unresolved scanner probe is not evidence of an empty
//! machine, so `push` must not turn it into a successful no-op.
//!
//! The fixture contains registry metadata only. It creates no session files
//! and asserts only exit status plus metadata-only diagnostic text.

use std::fs;
use std::process::Command;

#[test]
fn push_does_not_call_an_unresolved_scan_empty() {
    let sandbox = tempfile::tempdir().unwrap();
    let home = sandbox.path().join("home");
    let registry = sandbox.path().join("registry.json");
    let stage = sandbox.path().join("stage");
    let repo = sandbox.path().join("repo");
    let key_file = sandbox.path().join("masterkey.json");
    fs::create_dir_all(&home).unwrap();

    let cell = r#"{
        "template": "$CODEX_HOME/sessions/<date>",
        "env_override": "CODEX_HOME",
        "format": "jsonl",
        "confidence": "source-confirmed",
        "source": "B63 synthetic fixture"
    }"#;
    fs::write(
        &registry,
        format!(
            r#"{{
                "schema_version": 1,
                "generated": "B63 synthetic fixture",
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

    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args([
            "push",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "b63-fixture",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key_file.to_str().unwrap(),
            "--no-reap",
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.path().join("config"))
        .env("XDG_DATA_HOME", sandbox.path().join("data"))
        .env("XDG_STATE_HOME", sandbox.path().join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .env_remove("CODEX_HOME")
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "an unresolved scanner probe must not produce success; exit={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status.code()
    );
    assert!(
        stdout.contains("scanner_unknown=1"),
        // stderr is included because a failure here has twice been an *earlier*
        // failure in disguise: the command exited non-zero before printing any
        // stdout at all, so the missing substring says nothing about why. On
        // Windows CI (2026-08-28) this assertion fired with a completely empty
        // stdout, and without stderr there was no way to tell whether the
        // scanner diagnostic regressed or the process died before reaching it.
        "the diagnostic must preserve the unknown probe count; exit={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status.code()
    );
    assert!(
        !stdout.contains("no archivable content this run"),
        "unknown scan state must not be rendered as an empty successful run; stdout={stdout}"
    );
}
