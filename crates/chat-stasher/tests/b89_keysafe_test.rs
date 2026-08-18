//! B89-KEYSAFE old-behaviour evidence and regression tests.
//!
//! Only synthetic paths in a tempfile are used; no repository, key, HOME, or
//! session content is involved.

use std::fs;
use std::process::Command;

#[test]
fn seal_does_not_create_an_unknown_session_partition() {
    let dir = tempfile::tempdir().unwrap();
    let stage = dir.path().join("stage");
    let holder = stage.join("target").join("holder");
    fs::create_dir_all(&holder).unwrap();

    // The raw path ends in .., so file_stem() is None, while canonicalizing
    // it resolves to the stage root and passes the stage ownership guard.
    let active = holder.join("..");
    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args([
            "seal",
            "--harness",
            "claude-code",
            "--active",
            active.to_str().unwrap(),
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "synthetic-machine",
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "a directory is not a sealable file"
    );
    let unknown = stage
        .join("sessions")
        .join("synthetic-machine")
        .join("unknown");
    assert!(
        !unknown.exists(),
        "failure before session derivation must not create an unknown partition"
    );
}
