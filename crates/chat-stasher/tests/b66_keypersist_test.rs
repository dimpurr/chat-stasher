//! B66 regression: a masterkey that never reached the disk must never be
//! announced as `masterkey created+persisted`.
//!
//! The masterkey is the only thing that can ever re-open this repository. A run
//! that writes an archive under a key that is not on disk produces a repository
//! nobody can read again — strictly worse than not backing up, because it also
//! reports success. So the failure has to stop the run *before* a single byte
//! of archive is written.
//!
//! Both cases run against an empty synthetic registry with isolated HOME/XDG
//! directories, a hand-built stage, and a temporary repository. No real key
//! file, no real repository, no real session body is touched, and no key
//! material is ever printed — the assertions only look at modes, exit codes and
//! the presence/absence of words.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The stage only has to be non-empty: `cmd_push` reaches the masterkey step
/// once `sealed_shard_count(stage) > 0`, and with an empty registry no harness
/// is scanned. The shard body is synthetic text, never a real session.
fn stage_with_one_shard(sandbox: &Path, machine: &str) -> PathBuf {
    let stage = sandbox.join("stage");
    let shard_dir = stage
        .join("sessions")
        .join(machine)
        .join("b66.synthetic-session")
        .join("000");
    fs::create_dir_all(&shard_dir).unwrap();
    fs::write(
        shard_dir.join("000001.jsonl"),
        "{\"schema_version\":1,\"note\":\"B66 synthetic shard, not a session\"}\n",
    )
    .unwrap();
    stage
}

fn run_push(sandbox: &Path, stage: &Path, repo: &Path, key_file: &Path, machine: &str) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"B66 synthetic","harnesses":[]}"#,
    )
    .unwrap();

    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(["push", "--stage"])
        .arg(stage)
        .arg("--repo")
        .arg(repo)
        .arg("--key-file")
        .arg(key_file)
        .args(["--machine", machine, "--keep-ssh-masters"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

/// Make the key file undrivable the honest way: a real, unwritable directory,
/// no code change. The key path goes one level *below* the locked directory on
/// purpose — `persist_key_file` chmods its own parent to 0700 before writing,
/// so locking the immediate parent would only be undone; locking its parent
/// makes the `create_dir_all` itself fail, which is a genuine "the key cannot
/// be written" case. Returns false when the sandbox cannot express that (e.g.
/// running as root), in which case the case is skipped rather than silently
/// passing.
#[cfg(unix)]
fn lock_directory(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dir).unwrap();
    fs::set_permissions(dir, fs::Permissions::from_mode(0o500)).unwrap();
    fs::write(dir.join(".b66-probe"), b"probe").is_err()
}

#[cfg(unix)]
fn unlock_directory(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    #[allow(
        clippy::let_underscore_must_use,
        reason = "The test only needs to restore permissions before teardown; failure is not part of the assertion."
    )]
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
#[cfg(unix)]
fn key_that_cannot_be_written_is_never_called_persisted_and_writes_no_archive() {
    let sandbox = tempfile::tempdir().unwrap();
    let machine = "b66-locked";
    let stage = stage_with_one_shard(sandbox.path(), machine);
    let locked = sandbox.path().join("locked-keydir");
    if !lock_directory(&locked) {
        eprintln!("b66: sandbox cannot make a directory unwritable (root?), case skipped");
        return;
    }
    let repo = sandbox.path().join("repo");
    let key_file = locked.join("keys").join("masterkey.json");

    let output = run_push(sandbox.path(), &stage, &repo, &key_file, machine);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let combined = format!("{stdout}{stderr}");
    unlock_directory(&locked);

    assert_eq!(
        output.status.code(),
        Some(3),
        "a key that never reached the disk must not exit 0 or be confused with a completed \
         failure; stdout={stdout} stderr={stderr}"
    );
    assert!(
        !combined.contains("persist"),
        "nothing may claim the key was persisted; stdout={stdout} stderr={stderr}"
    );
    assert!(
        !key_file.exists(),
        "no key file may be left behind on the failure path"
    );
    assert!(
        !repo.exists(),
        "the run must stop BEFORE writing an archive: a repository written under a key that is \
         not on disk can never be opened again; stdout={stdout} stderr={stderr}"
    );
}

#[test]
#[cfg(unix)]
fn normal_path_still_creates_a_0600_key_that_the_next_run_loads() {
    let sandbox = tempfile::tempdir().unwrap();
    let machine = "b66-normal";
    let stage = stage_with_one_shard(sandbox.path(), machine);
    let repo = sandbox.path().join("repo");
    let key_file = sandbox.path().join("keys").join("masterkey.json");

    let first = run_push(sandbox.path(), &stage, &repo, &key_file, machine);
    let stdout = String::from_utf8_lossy(&first.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&first.stderr).into_owned();
    assert_eq!(
        first.status.code(),
        Some(0),
        "the ordinary first push must still succeed; stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("masterkey created+persisted"),
        "a key that IS on disk must still say so; stdout={stdout} stderr={stderr}"
    );
    assert_eq!(
        mode_of(&key_file),
        0o600,
        "the created key file must stay owner-only"
    );

    // Second run: the "existing key" path must be untouched by this fix.
    let second = run_push(sandbox.path(), &stage, &repo, &key_file, machine);
    let stdout2 = String::from_utf8_lossy(&second.stdout).into_owned();
    let stderr2 = String::from_utf8_lossy(&second.stderr).into_owned();
    assert_eq!(
        second.status.code(),
        Some(0),
        "the existing-key path must be unchanged; stdout={stdout2} stderr={stderr2}"
    );
    assert!(
        stdout2.contains("masterkey loaded"),
        "the second run must load the persisted key; stdout={stdout2} stderr={stderr2}"
    );
}
