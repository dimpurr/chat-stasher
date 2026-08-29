//! Exercise the built CLI from outside the checkout.
//!
//! The fixture is synthetic and the child processes receive an isolated HOME
//! plus an isolated XDG tree. Assertions inspect only command success and
//! metadata summaries; no session body is printed.

use std::path::Path;
use std::process::{Command, Output};

fn run(binary: &Path, cwd: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(binary)
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_STATE_HOME", home.join("state"))
        // Simulate an installed binary: the runtime manifest hint must not
        // be needed when the cwd is outside the source checkout.
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CHAT_STASHER_REGISTRY")
        .output()
        .expect("run chat-stasher fixture command")
}

fn assert_success(output: Output, command: &str) {
    assert!(
        output.status.success(),
        "{command} failed: exit={:?}, stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn installed_binary_uses_embedded_registry_outside_checkout() {
    let binary = Path::new(env!("CARGO_BIN_EXE_chat-stasher"));
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    assert!(!outside.path().starts_with(env!("CARGO_MANIFEST_DIR")));

    let source = home
        .path()
        .join(".claude/projects/fixture-project/019bf00d-97b6-7eb2-9bf8-eacbacc09765.jsonl");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, b"{}\n").unwrap();

    let stage = outside.path().join("stage");
    let repo = outside.path().join("repo");
    let key_file = outside.path().join("masterkey.json");

    assert_success(run(binary, outside.path(), home.path(), &["init"]), "init");
    assert_success(
        run(
            binary,
            outside.path(),
            home.path(),
            &[
                "collect",
                "--stage",
                stage.to_str().unwrap(),
                "--machine",
                "fixture-machine",
            ],
        ),
        "collect",
    );
    assert_success(
        run(
            binary,
            outside.path(),
            home.path(),
            &[
                "push",
                "--stage",
                stage.to_str().unwrap(),
                "--repo",
                repo.to_str().unwrap(),
                "--key-file",
                key_file.to_str().unwrap(),
                "--machine",
                "fixture-machine",
                "--connections",
                "1",
                "--keep-ssh-masters",
            ],
        ),
        "push",
    );
    assert_success(
        run(
            binary,
            outside.path(),
            home.path(),
            &[
                "verify",
                "--level",
                "all",
                "--stage",
                stage.to_str().unwrap(),
                "--repo",
                repo.to_str().unwrap(),
                "--key-file",
                key_file.to_str().unwrap(),
                "--machine",
                "fixture-machine",
                "--connections",
                "1",
                "--keep-ssh-masters",
            ],
        ),
        "verify",
    );
}
