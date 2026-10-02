//! W306 · the code-level fail-safe against a fixture identity reaching a real
//! archive.
//!
//! On 2026-10-02 a test run resolved the author's real data root and planted
//! `stage/sessions/<machine>/chatgpt.synthetic-session` and a `synthetic-install`
//! row in the real `extension-coordination.sqlite3`. W306 isolates the
//! environment, and [`chat_stasher::test_identity_guard`] is the backstop that
//! does not depend on a test getting its environment right.
//!
//! The proof here is deliberately **not** a fixture under a temp root — that is
//! the arrangement that already works. It aims a fixture identity at a
//! directory *outside* the process temp directory (`CARGO_TARGET_TMPDIR`, a
//! scratch dir under `target/`, so nothing real is at risk) and asserts the
//! write is refused before the stage is touched. The control does the same
//! write with an ordinary identity and asserts it succeeds, which is what makes
//! the first assertion a statement about the identity and not about the path.
//!
//! RED-first: before the guard existed, the first test failed by *succeeding* —
//! `seal_payload` wrote the shard (and `.ingest.lock`) into the non-temp stage.
//! Removing the guard from `inbox::seal_payload` and re-running reproduces that
//! exactly; the mutation used is stated in the W306 report.

use chat_stasher::inbox;
use std::path::{Path, PathBuf};

const MACHINE: &str = "w306-probe-machine";

fn bundle(session_id: &str) -> Vec<u8> {
    format!(
        r#"{{"schema":"chat-stasher/inbox@1","platform":"chatgpt","sessionId":"{session_id}","parsed":{{"hasJson":true,"keys":["id"]}},"raw":{{"text":"{{}}","bytes":2}}}}"#
    )
    .into_bytes()
}

/// A scratch root that is deliberately **outside** `std::env::temp_dir()`.
///
/// The directory is created so the control can take the stage lock: a stage
/// that does not exist yet is refused for that reason, and the probe would then
/// be measuring the missing directory instead of the identity.
fn non_temp_stage(name: &str) -> (tempfile::TempDir, PathBuf) {
    // `CARGO_TARGET_TMPDIR` is `<target>/tmp`, created by cargo for integration
    // tests. It is not under the process temp directory, and it is disposable —
    // never a user data directory.
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    drop(std::fs::remove_dir_all(&root));
    std::fs::create_dir_all(&root).expect("create probe stage");
    let holder = tempfile::tempdir().expect("temp holder");
    (holder, root)
}

#[test]
fn a_fixture_identity_is_refused_outside_the_temp_directory() {
    let (_holder, stage) = non_temp_stage("w306-refused");
    assert!(
        !stage.starts_with(std::env::temp_dir()),
        "precondition: the probe stage must not be a temp path"
    );

    let err = inbox::seal_payload(
        "chatgpt-synthetic-session.json",
        &bundle("synthetic-session"),
        &stage,
        MACHINE,
        chat_stasher::store::DEFAULT_SHARD_BUCKET_CAP,
        None,
        None,
    )
    .expect_err("a fixture identity outside the temp directory must be refused");

    let text = err.to_string();
    assert!(
        text.contains("refusing to write fixture identity"),
        "the refusal must name the guard, not fail for an unrelated reason: {text}"
    );
    // The point of refusing *before* the lock: the stage was not touched — no
    // lock file, no session partition, nothing.
    assert!(
        !stage.join(".ingest.lock").exists(),
        "the refused write must not even take the stage lock ({})",
        stage.display()
    );
    assert!(
        !stage.join("sessions").exists(),
        "the refused write must not create a session partition ({})",
        stage.display()
    );
}

#[test]
fn the_same_write_with_an_ordinary_identity_succeeds() {
    let (_holder, stage) = non_temp_stage("w306-control");

    let outcome = inbox::seal_payload(
        "chatgpt-ordinary-session.json",
        &bundle("6a4bb1c6-458c-83eb-a146-676418a2f960"),
        &stage,
        MACHINE,
        chat_stasher::store::DEFAULT_SHARD_BUCKET_CAP,
        None,
        None,
    )
    .expect("an ordinary identity must not be refused by the fixture guard");

    assert!(
        matches!(outcome, inbox::SealOutcome::Stored(_)),
        "the control write must land, or the first test proves nothing about identity"
    );
    assert!(
        stage.join("sessions").join(MACHINE).exists(),
        "the control must actually write into its stage"
    );

    drop(std::fs::remove_dir_all(Path::new(&stage)));
}
