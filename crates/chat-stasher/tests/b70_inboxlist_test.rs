//! B70 regression: "we could not read the inbox record" must never be served
//! as "you have no inbox".
//!
//! `push` audits an empty stage against the list of inboxes this machine has
//! ever ingested from. That list lives in one small JSON file. When the file
//! could not be read, the old call site substituted an empty list, so the line
//!
//!     [push] consumed audit: inboxes=0 files=0 ...
//!
//! reported a *number* where the truthful answer was *unknown* — and the empty
//! stage was then audited against nothing at all. A user with a browser
//! extension quietly dropping sessions into an inbox could be told the audit
//! agreed there was nothing to do. An audit that passes because it read
//! nothing is worse than no audit.
//!
//! Three states have to stay apart, and all three are exercised here:
//!   * no record at all   -> this machine truly has no inbox (normal path)
//!   * record, empty list -> a real, trusted zero        (normal path)
//!   * record unreadable  -> unknown; the run must stop, non-zero
//!
//! Everything runs in a `tempfile` sandbox with isolated HOME/XDG/registry.
//! No real inbox, archive, repository or session body is touched, and only
//! exit codes and the presence/absence of words are asserted.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The state directory `push` derives from `XDG_DATA_HOME`.
fn state_dir(sandbox: &Path) -> PathBuf {
    sandbox.join("data").join("chat-stasher").join("state")
}

/// The chat-stasher-owned record `push` reads to learn which inboxes exist.
/// Written by hand on purpose: this test is about how `push` reacts to the
/// file, and the ingest path itself is explicitly out of scope for B70.
fn write_inbox_record(sandbox: &Path, known_inboxes: &str) -> PathBuf {
    let dir = state_dir(sandbox);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("consumed-audit-v1.json");
    fs::write(
        &path,
        format!("{{\"version\":1,\"known_inboxes\":[{known_inboxes}],\"files\":{{}}}}"),
    )
    .unwrap();
    path
}

/// An empty stage is the whole point: this is the branch where `push` decides
/// what "nothing to archive" means, and where the inbox list is consulted.
fn empty_stage(sandbox: &Path) -> PathBuf {
    let stage = sandbox.join("stage");
    fs::create_dir_all(&stage).unwrap();
    stage
}

fn run_push(sandbox: &Path, stage: &Path, machine: &str) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"B70 synthetic","harnesses":[]}"#,
    )
    .unwrap();

    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(["push", "--stage"])
        .arg(stage)
        .arg("--repo")
        .arg(sandbox.join("repo"))
        .arg("--key-file")
        .arg(sandbox.join("keys").join("masterkey.json"))
        .args(["--machine", machine, "--keep-ssh-masters"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

/// Make the record unreadable the honest way — real file permissions, no code
/// change and no corrupted bytes, so the failure is exactly "the file is there
/// and we cannot read it". Only the record itself is locked; the directory
/// stays traversable so the *collector* state next to it still loads and the
/// run still reaches the inbox-list step. Returns false when the sandbox
/// cannot express unreadability (e.g. running as root), so the case is skipped
/// rather than silently passing.
#[cfg(unix)]
fn lock_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    fs::read(path).is_err()
}

#[cfg(unix)]
fn unlock_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    #[allow(
        clippy::let_underscore_must_use,
        reason = "Only restores permissions before teardown; failure is not part of the assertion."
    )]
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

/// The regression itself.
///
/// Unix-only: "the record cannot be read" is injected by chmodding the file to
/// 0o000, and Windows has no standard-API equivalent of an unreadable file.
/// The two normal-path arms around it — `machine_that_never_used_an_inbox_is_
/// unchanged` and `readable_but_empty_record_prints_exactly_what_no_record_
/// prints` — run on every platform, so on Windows the "unknown must not become
/// inboxes=0" property is pinned from the sides that Windows can express while
/// the chmod arm is documented rather than faked.
#[test]
#[cfg(unix)]
fn unreadable_inbox_record_is_not_reported_as_a_passing_audit() {
    let sandbox = tempfile::tempdir().unwrap();
    let stage = empty_stage(sandbox.path());
    let record = write_inbox_record(sandbox.path(), "\"/nonexistent/b70-inbox\"");
    if !lock_file(&record) {
        eprintln!("b70: sandbox cannot make a file unreadable (root?), case skipped");
        return;
    }

    let output = run_push(sandbox.path(), &stage, "b70-unreadable");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    unlock_file(&record);

    assert_eq!(
        output.status.code(),
        Some(3),
        "a record we never read is \"did not finish\" (3), not \"finished and failed\" (1) and \
         certainly not success; stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("no archivable content this run"),
        "the reassuring all-clear must not be printed when the inbox list was never read; \
         stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("consumed audit:"),
        "no `inboxes=N` may be printed for a list we could not read: the honest answer is \
         unknown, not a number; stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.contains("cannot read the remembered inbox list"),
        "the failure must name the thing that could not be read, not blame a downstream step; \
         stdout={stdout} stderr={stderr}"
    );
}

/// False-positive guard, and just as important: a machine that genuinely never
/// ingested from an inbox must be entirely unaffected by the fix.
#[test]
fn machine_that_never_used_an_inbox_is_unchanged() {
    let sandbox = tempfile::tempdir().unwrap();
    let stage = empty_stage(sandbox.path());
    // Deliberately no record file at all.
    assert!(!state_dir(sandbox.path())
        .join("consumed-audit-v1.json")
        .exists());

    let output = run_push(sandbox.path(), &stage, "b70-norecord");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        stdout.contains("[push] consumed audit: inboxes=0 files=0"),
        "with no inbox ever used, `inboxes=0` is a true statement and must still be printed \
         verbatim; stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stderr.contains("cannot read the remembered inbox list"),
        "absence of the record is not a read failure; stdout={stdout} stderr={stderr}"
    );
}

/// The third state: the record exists and honestly lists nothing. This must be
/// indistinguishable from the case above — byte for byte on stdout and on the
/// exit code — which is what proves the fix costs the normal path nothing.
#[test]
fn readable_but_empty_record_prints_exactly_what_no_record_prints() {
    let no_record = tempfile::tempdir().unwrap();
    let stage_a = empty_stage(no_record.path());
    let a = run_push(no_record.path(), &stage_a, "b70-same");

    let empty_record = tempfile::tempdir().unwrap();
    let stage_b = empty_stage(empty_record.path());
    write_inbox_record(empty_record.path(), "");
    let b = run_push(empty_record.path(), &stage_b, "b70-same");

    let a_out = String::from_utf8_lossy(&a.stdout).into_owned();
    let b_out = String::from_utf8_lossy(&b.stdout).into_owned();
    assert_eq!(
        a.status.code(),
        b.status.code(),
        "a trusted empty list and no list at all must reach the same verdict"
    );
    assert_eq!(
        a_out, b_out,
        "the normal path must not have gained a single word from this fix"
    );
}
