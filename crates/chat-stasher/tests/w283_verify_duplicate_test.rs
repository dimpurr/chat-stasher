//! W283 — L3 must *see* a doubled shard sequence, and must not call it
//! corruption.
//!
//! W281's finding, measured: `dest-init` sealed every session a second time, so
//! a fresh destination stored the conversation twice — and `verify --level l3`
//! said `OK`, because L3 reconciles on `(shard count, concatenated bytes,
//! concatenated sha256)` and a doubled body is *self-consistent* on all three.
//! The doubled archive was invisible to the integrity path.
//!
//! What L3 can see is the shard sequence itself: a re-seal leaves a shard
//! byte-identical to one already in it. These tests push real repositories
//! through the real binary — one with such a sequence, one without — and pin
//! both halves of the answer:
//!
//! * the repeat is named, per session, and counted into the `L3 verdict` line
//!   so a green run cannot hide it;
//! * it is reported as a *possibility* and does not fail the run. A harness may
//!   legitimately append bytes identical to bytes already sealed, and the
//!   archive records no provenance that could tell the two apart — a false
//!   "your archive is corrupt" would be as much a lie as the false "OK".
//!
//! No real archive is touched: every path is a temp dir, and the assertions are
//! on the tool's own output, never on conversation content.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const MACHINE: &str = "w283-machine";
const SESSION: &str = "claude-code.w283-machine.019bf00d-97b6-7eb2-9bf8-eacbacc09765";

fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W283 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

/// One sealed shard file at the next global sequence.
fn write_shard(stage: &Path, seq: u32, body: &str) {
    let dir = stage
        .join("sessions")
        .join(MACHINE)
        .join(SESSION)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{seq:06}.jsonl")), body).unwrap();
}

/// Push `stage` into a fresh local repository, then run L3 over it, and return
/// `(exit_ok, stdout)`.
fn push_then_verify_l3(sandbox: &Path, stage: &Path) -> (bool, String) {
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let push = run(
        sandbox,
        &[
            "push",
            "--stage",
            stage.to_str().unwrap(),
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            MACHINE,
            "--keep-ssh-masters",
        ],
    );
    assert!(
        push.status.success(),
        "push should exit 0, got {:?}\n{}{}",
        push.status,
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );

    let verify = run(
        sandbox,
        &[
            "verify",
            "--level",
            "l3",
            "--stage",
            stage.to_str().unwrap(),
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            MACHINE,
            "--keep-ssh-masters",
        ],
    );
    (
        verify.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&verify.stdout),
            String::from_utf8_lossy(&verify.stderr)
        ),
    )
}

/// The doubled sequence `dest-init` left behind: two shards, one conversation.
#[test]
fn l3_names_a_repeated_shard_and_does_not_fail_on_it() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let line =
        r#"{"parentUuid":null,"type":"user","message":{"role":"user","content":"hi"},"uuid":"u1"}"#;
    write_shard(&stage, 1, &format!("{line}\n"));
    write_shard(&stage, 2, &format!("{line}\n"));

    let (ok, out) = push_then_verify_l3(sandbox, &stage);
    assert!(
        out.contains("POSSIBLE DUPLICATE SEAL"),
        "L3 must name the repeated shard:\n{out}"
    );
    assert!(
        out.contains("shard 2 repeats shard 1"),
        "L3 must name which shards repeat:\n{out}"
    );
    assert!(
        out.contains("exactly the whole preceding body"),
        "a whole-body re-seal must be distinguished from a repeated block:\n{out}"
    );
    assert!(
        out.contains("possible duplicate seal") && out.contains("L3 verdict"),
        "the verdict line must carry the count, so a green run cannot hide it:\n{out}"
    );
    assert!(
        ok,
        "a repeated shard is a possibility, not corruption — L3 must not fail on it:\n{out}"
    );
}

/// A normal incremental sequence: two different shards, no repeat, no noise.
#[test]
fn l3_does_not_cry_duplicate_over_a_normal_sequence() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    write_shard(&stage, 1, "{\"uuid\":\"u1\"}\n");
    write_shard(&stage, 2, "{\"uuid\":\"u2\"}\n");

    let (ok, out) = push_then_verify_l3(sandbox, &stage);
    assert!(
        !out.contains("POSSIBLE DUPLICATE SEAL"),
        "two different shards must not be reported as a repeat:\n{out}"
    );
    assert!(
        out.contains("[verify] L3 verdict       : OK"),
        "the verdict must be exactly OK when there is nothing to report:\n{out}"
    );
    assert!(ok, "L3 should exit 0:\n{out}");
}

/// A body that took two shards to seal, then sealed again: `A, B, A, B`. The
/// repeat spans shards, so naming a single shard against the bytes before it
/// would call this a repeated block — and the whole body is what repeated.
#[test]
fn l3_names_a_multi_shard_reseal() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    write_shard(&stage, 1, "{\"uuid\":\"u1\"}\n");
    write_shard(&stage, 2, "{\"uuid\":\"u2\"}\n");
    write_shard(&stage, 3, "{\"uuid\":\"u1\"}\n");
    write_shard(&stage, 4, "{\"uuid\":\"u2\"}\n");

    let (ok, out) = push_then_verify_l3(sandbox, &stage);
    assert!(
        out.contains("POSSIBLE DUPLICATE SEAL"),
        "L3 must name the repeated body:\n{out}"
    );
    assert!(
        out.contains("shard 3 repeats shard 1"),
        "the report must name where the repeat begins:\n{out}"
    );
    assert!(
        out.contains("2 shards that repeat the body sealed before them"),
        "a repeat that spans shards must be named as the body, not as one shard:\n{out}"
    );
    assert!(
        ok,
        "a repeated body is a possibility, not corruption — L3 must not fail on it:\n{out}"
    );
}

/// The other side of that line: shard 3 repeating shard 2 while shard 1 differs
/// is a repeated *block*, which is the shape real repetition leaves as much as a
/// re-seal — so it is named as a block and not as a repeated body.
#[test]
fn l3_calls_a_repeated_block_a_repeated_block() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    write_shard(&stage, 1, "{\"uuid\":\"u1\"}\n");
    write_shard(&stage, 2, "{\"uuid\":\"u2\"}\n");
    write_shard(&stage, 3, "{\"uuid\":\"u2\"}\n");

    let (ok, out) = push_then_verify_l3(sandbox, &stage);
    assert!(
        out.contains("shard 3 repeats shard 2"),
        "the repeated pair must be named:\n{out}"
    );
    assert!(
        out.contains("a repeated block, not the whole preceding body"),
        "a repeated block must not be presented as a repeated body:\n{out}"
    );
    assert!(ok, "L3 must not fail on a possibility:\n{out}");
}
