//! W1033 · HOME-ISO: guard against test/gate push-state and state writes
//! touching the real user home directory.
//!
//! On 2026-10-23 22:09:33 BST, `~/.local/share/chat-stasher/state/push-state.json`
//! in the user's real HOME was written with:
//! `pushes.default: { finished_at_unix: 1792789773, machine: "gate-mbp", snapshots_in_repo: 2 }`
//! because `scripts/release-gate.sh` ran `chat-stasher push` without HOME / XDG isolation.
//!
//! W1033 isolates `HOME`, `USERPROFILE`, and `XDG_*` in `release-gate.sh`, and
//! reserves `gate-mbp` in `test_identity_guard`, so that any push-state or state
//! write for a gate/fixture machine outside the temporary directory fails closed.
//!
//! The guard test here exercises that refusal against a scratch directory outside
//! `std::env::temp_dir()` (`CARGO_TARGET_TMPDIR`), asserting the write fails and
//! leaves the non-temp directory untouched. The guard test itself NEVER writes to
//! real HOME.

use chat_stasher::{metahash, pushstate, test_identity_guard};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

/// A scratch root that is deliberately **outside** `std::env::temp_dir()`.
///
/// `CARGO_TARGET_TMPDIR` is `<target>/tmp`, created by cargo for integration
/// tests. It is not under the process temp directory, and it is disposable —
/// never a user data or home directory.
struct NonTempDir {
    path: PathBuf,
}

impl Drop for NonTempDir {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.path));
    }
}

impl std::ops::Deref for NonTempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.path
    }
}

impl AsRef<std::path::Path> for NonTempDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

fn non_temp_dir(name: &str) -> NonTempDir {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    drop(fs::remove_dir_all(&root));
    fs::create_dir_all(&root).expect("create non-temp test dir");
    NonTempDir { path: root }
}

#[test]
fn gate_fixture_identity_is_reserved() {
    assert!(test_identity_guard::is_fixture_identity("gate-mbp"));
    assert!(test_identity_guard::is_fixture_identity("gate_mbp"));
    assert!(test_identity_guard::is_fixture_identity("gate--mbp"));
    assert!(test_identity_guard::is_fixture_identity("gate__mbp"));
    assert!(test_identity_guard::is_fixture_identity("GATE-MBP"));
    // Real hostnames containing "gate" as a whole token must NOT be reserved:
    assert!(!test_identity_guard::is_fixture_identity("gate"));
    assert!(!test_identity_guard::is_fixture_identity("mbp-gate"));
    assert!(!test_identity_guard::is_fixture_identity("data-gate"));
    // Ordinary words containing "gate" as a substring without whole token match must NOT be reserved:
    assert!(!test_identity_guard::is_fixture_identity(
        "aggregate-machine"
    ));
    assert!(!test_identity_guard::is_fixture_identity("surrogate"));
}

#[test]
fn push_state_refuses_gate_fixture_outside_temp_dir() {
    let non_temp = non_temp_dir("w1033-pushstate-refused");
    let state_dir = non_temp.join("state");
    assert!(
        !test_identity_guard::is_under_temp_dir(&state_dir),
        "precondition: state_dir must be outside temp_dir"
    );

    let err = pushstate::record_success(&state_dir, "default", "gate-mbp", 2, 1_800_000_000)
        .expect_err("gate-mbp outside temp dir must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("refusing to write fixture identity `gate-mbp`"),
        "error must name refusal of fixture identity: {msg}"
    );
    assert!(
        !state_dir.join("push-state.json").exists(),
        "push-state.json must not be created on refusal"
    );
}

#[test]
fn metahash_refuses_gate_fixture_outside_temp_dir() {
    let non_temp = non_temp_dir("w1033-metahash-refused");
    let state_dir = non_temp.join("state");
    assert!(
        !test_identity_guard::is_under_temp_dir(&state_dir),
        "precondition: state_dir must be outside temp_dir"
    );

    let err = metahash::record_pushed_meta_hash(
        &state_dir,
        "gate-mbp",
        Some("abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"),
    )
    .expect_err("gate-mbp metahash outside temp dir must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("refusing to write fixture identity `gate-mbp`"),
        "error must name refusal of fixture identity: {msg}"
    );
    let record_path = metahash::pushed_meta_record_path(&state_dir, "gate-mbp");
    assert!(
        !record_path.exists(),
        "pushed-meta file must not be created on refusal"
    );
}

#[test]
fn push_command_refuses_gate_machine_outside_temp_dir() {
    let non_temp = non_temp_dir("w1033-cli-push-refused");
    let fake_home = non_temp.join("fake-real-home");
    let stage = non_temp.join("stage");
    let repo = non_temp.join("repo");
    let key = non_temp.join("masterkey.json");
    fs::create_dir_all(&fake_home).unwrap();
    fs::create_dir_all(&stage).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args([
            "push",
            "--stage",
            stage.to_str().unwrap(),
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            "gate-mbp",
            "--keep-ssh-masters",
        ])
        .env("HOME", &fake_home)
        .env("USERPROFILE", &fake_home)
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&non_temp),
        )
        .output()
        .expect("spawn push command");

    assert!(
        !output.status.success(),
        "push with fixture machine outside temp dir must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing to write fixture identity `gate-mbp`"),
        "stderr must report refusal of fixture identity: {stderr}"
    );
    assert!(
        !fake_home
            .join(".local/share/chat-stasher/state/push-state.json")
            .exists(),
        "push-state.json must not be written to non-temp HOME"
    );
}

#[test]
fn push_state_allows_gate_machine_inside_temp_dir() {
    let temp = tempfile::tempdir().expect("temp root");
    let state_dir = temp.path().join("state");
    pushstate::record_success(&state_dir, "default", "gate-mbp", 2, 1_800_000_000)
        .expect("gate-mbp inside temp dir must be allowed");
    assert!(state_dir.join("push-state.json").exists());
}

#[test]
fn push_state_allows_ordinary_machine_outside_temp_dir() {
    let non_temp = non_temp_dir("w1033-ordinary-allowed");
    let state_dir = non_temp.join("state");
    // Real hostnames containing the token "gate" must not be refused:
    for name in ["production-host-1", "gate", "mbp-gate", "data-gate"] {
        pushstate::record_success(&state_dir, "default", name, 1, 1_800_000_000).unwrap_or_else(
            |e| panic!("ordinary host {name} outside temp dir must be allowed: {e}"),
        );
    }
    assert!(state_dir.join("push-state.json").exists());
}
