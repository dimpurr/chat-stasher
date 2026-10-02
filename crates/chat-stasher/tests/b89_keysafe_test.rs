//! B89-KEYSAFE old-behaviour evidence and regression tests.
//!
//! Only synthetic paths in a tempfile are used; no repository, key, HOME, or
//! session content is involved.

use std::fs;
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

#[test]
fn seal_does_not_create_an_unknown_session_partition() {
    // W306: the child gets the full sandbox, not just the cache pin. Without a
    // HOME, `config_path()` / `default_data_root()` / `default_state_dir()`
    // would fall back to the machine's real home, so a later change that made
    // `seal` read any of them would reach real user data.
    let sandbox = test_support::Sandbox::new();
    let stage = sandbox.root().join("stage");
    let holder = stage.join("target").join("holder");
    fs::create_dir_all(&holder).unwrap();

    // The raw path ends in .., so file_stem() is None, while canonicalizing
    // it resolves to the stage root and passes the stage ownership guard.
    let active = holder.join("..");
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command.args([
        "seal",
        "--harness",
        "claude-code",
        "--active",
        active.to_str().unwrap(),
        "--stage",
        stage.to_str().unwrap(),
        "--machine",
        "synthetic-machine",
    ]);
    let output = sandbox.apply(&mut command).output().unwrap();

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
