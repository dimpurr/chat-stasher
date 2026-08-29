//! B56 — pin the split between diagnostic output and data output.
//!
//! Two classes of line leave this CLI on very different contracts:
//!
//! * **A · diagnostics** — the human-facing report (`doctor`'s narrative,
//!   `status`'s verdict). Nothing parses these, so they belong on `stderr`.
//! * **B · data** — lines another program reads. `scripts/release-gate.sh:180`
//!   pulls `data_added=` out of `push` with `sed`, so that line is a contract
//!   and must stay on `stdout`.
//!
//! The gate itself cannot enforce this: every one of its invocations captures
//! `2>&1` (`scripts/release-gate.sh:163-164, 175-176, 189, 217-218, 227-228`),
//! which merges the two streams before it parses them. A green gate therefore
//! says nothing about *which* stream a line came out of. These tests read the
//! two streams separately, which is the only way the distinction is observable.
//!
//! Everything here is synthetic: the fixture bytes are generated in this file,
//! and each child gets an isolated `HOME` plus an isolated XDG tree so the
//! developer's real config and real sessions are never touched or printed.

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
        .env_remove("CARGO_MANIFEST_DIR")
        .env_remove("CHAT_STASHER_REGISTRY")
        .output()
        .expect("run chat-stasher fixture command")
}

fn streams(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A class: `doctor` is pure narrative. Nothing reads it — the gate runs it and
/// checks only the exit code (`scripts/release-gate.sh:240-249`) — so not one
/// line of it may land on `stdout`.
#[test]
fn doctor_report_is_diagnostic_and_stays_off_stdout() {
    let binary = Path::new(env!("CARGO_BIN_EXE_chat-stasher"));
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();

    let out = run(binary, outside.path(), home.path(), &["doctor"]);
    let (stdout, stderr) = streams(&out);

    assert!(
        stderr.contains("doctor —"),
        "doctor's report must be on stderr, but stderr was:\n{stderr}"
    );
    assert!(
        !stdout.contains("doctor —"),
        "doctor's report is diagnostic and must not reach stdout, but stdout was:\n{stdout}"
    );
    assert!(
        stdout.trim().is_empty(),
        "doctor emits no data, so stdout must be empty, but stdout was:\n{stdout}"
    );
}

/// A class: `status`'s run-once verdict. Only `docs/install.md:166,272` refers
/// to it, and only as something a person reads.
#[test]
fn status_verdict_is_diagnostic_and_stays_off_stdout() {
    let binary = Path::new(env!("CARGO_BIN_EXE_chat-stasher"));
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();

    let out = run(binary, outside.path(), home.path(), &["status"]);
    let (stdout, stderr) = streams(&out);

    assert!(
        stderr.contains("[run-once]"),
        "status's verdict must be on stderr, but stderr was:\n{stderr}"
    );
    assert!(
        !stdout.contains("[run-once]"),
        "status's verdict is diagnostic and must not reach stdout, but stdout was:\n{stdout}"
    );
}

/// B class: `push`'s summary line. `scripts/release-gate.sh:180` runs
/// `sed -n 's/.*data_added=\([0-9]*\).*/\1/p'` over it, and `:168` greps the
/// same output for `INIT`. Both are consumed by a program, so both stay on
/// `stdout` — this test fails if a later cleanup sweeps them to `stderr`.
#[test]
fn push_summary_is_data_and_stays_on_stdout() {
    let binary = Path::new(env!("CARGO_BIN_EXE_chat-stasher"));
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();

    // Synthetic sealed stage: one machine, one session, one shard.
    let shard = outside
        .path()
        .join("stage/sessions/fixture-machine/fixture-session/000001.jsonl");
    std::fs::create_dir_all(shard.parent().unwrap()).unwrap();
    let mut body = String::new();
    for i in 1..=64 {
        body.push_str(&format!(
            "{{\"seq\":{i},\"text\":\"B56 synthetic payload\"}}\n"
        ));
    }
    std::fs::write(&shard, body).unwrap();

    let stage = outside.path().join("stage");
    let repo = outside.path().join("repo");
    let key_file = outside.path().join("masterkey.json");

    let out = run(
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
    );
    let (stdout, stderr) = streams(&out);
    assert!(
        out.status.success(),
        "push failed: exit={:?}, stderr={stderr}",
        out.status.code()
    );

    assert!(
        stdout.contains("data_added="),
        "release-gate.sh:180 parses `data_added=` out of push's stdout, but stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("INIT"),
        "release-gate.sh:168 greps push's stdout for `INIT`, but stdout was:\n{stdout}"
    );
}
