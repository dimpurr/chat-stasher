//! B74 regression: a sealed shard whose *rename* was never proven durable must
//! not be reported as consumed, and must not cause the inbox file to be retired.
//!
//! Why this one step is different from every other best-effort fsync in this
//! crate: ingest is "seal first, retire second", and retirement moves the inbox
//! file into `<inbox>/consumed/`, which `ingest` deliberately never rescans. So
//! the *only* thing that makes ingest re-runnable is the inbox file still being
//! in the inbox. If the shard's directory entry is not durable but the
//! retirement is, a power cut leaves a stage with no shard and an inbox with
//! nothing to re-consume — the content-addressed `fileSha256` dedup cannot help,
//! because it only ever compares against files the scan still sees. That is a
//! silent, tool-unrecoverable loss, so the durability proof has to be reported.
//!
//! The contrasting case is `retire`'s own directory fsync, which stays
//! best-effort on purpose: losing *that* rename just puts the file back in the
//! inbox, where the next run reads it, matches its `fileSha256` against the
//! sealed shard and reports a duplicate. Idempotent and self-healing — turning
//! it fatal would block a user over something a re-run already fixes.
//!
//! Everything runs against a `tempfile` sandbox with a synthetic bundle. No real
//! inbox, stage, repository or session body is touched, and the assertions only
//! look at counts, path existence and the presence/absence of words.

#![cfg(unix)]

use chat_stasher::inbox;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const MACHINE: &str = "b74-machine";
const SESSION: &str = "b74-synthetic-session";
const BUNDLE_FILE: &str = "deepseek-b74-synthetic-session.json";
/// `parse_bundle` builds the id as `<platform>.<sessionId>`.
const SESSION_ID: &str = "deepseek.b74-synthetic-session";

/// A synthetic `inbox@2`-shaped bundle. `raw.text` is a fixed marker string,
/// never a real conversation.
fn bundle_bytes() -> Vec<u8> {
    format!(
        r#"{{"platform":"deepseek","sessionId":"{SESSION}","capturedAt":"2026-01-01T00:00:00Z",
            "parsed":{{"hasJson":false,"keys":[]}},
            "raw":{{"text":"B74 synthetic bundle, not a session","bytes":35}}}}"#
    )
    .into_bytes()
}

fn write_bundle(inbox_dir: &Path) -> PathBuf {
    fs::create_dir_all(inbox_dir).unwrap();
    let path = inbox_dir.join(BUNDLE_FILE);
    fs::write(&path, bundle_bytes()).unwrap();
    path
}

fn mode(path: &Path, bits: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
}

/// Make the shard's own bucket directory un-`fsync`-able the honest way — a real
/// directory with real permissions, no production code change and no fault
/// injection hook.
///
/// The trick is the `0o300` mode: write+execute lets the seal *create* its temp
/// file and *rename* it into place, while the missing read bit makes
/// `File::open(dir)` — the only way to fsync a directory — fail with `EACCES`.
/// That isolates precisely the durability proof from everything before it.
///
/// The bucket directory is reached through a **symlink** (`<session>/000` ->
/// locked dir) for a specific reason: `clean_stale_tmp` and
/// `sealed_shard_entries` both walk the session directory and `read_dir` every
/// entry whose `file_type()` is a directory. `std`'s `DirEntry::file_type` does
/// not follow symlinks, so a symlinked bucket is skipped by both walkers (which
/// would otherwise fail on the unreadable directory first, for the wrong
/// reason), while `create_dir_all`, `File::create`, `rename` and `File::open`
/// all follow it normally.
///
/// Returns `None` when the sandbox cannot express "unreadable" (e.g. running as
/// root), so the case is skipped instead of silently passing.
fn rig_unfsyncable_bucket(stage: &Path, locked: &Path) -> Option<PathBuf> {
    let session_dir = stage.join("sessions").join(MACHINE).join(SESSION_ID);
    fs::create_dir_all(&session_dir).unwrap();
    fs::create_dir_all(locked).unwrap();
    let bucket = session_dir.join("000");
    std::os::unix::fs::symlink(locked, &bucket).unwrap();
    mode(locked, 0o300);

    // The rig is only meaningful if the directory really cannot be opened for
    // fsync while still accepting a rename.
    if fs::File::open(&bucket).is_ok() {
        mode(locked, 0o700);
        return None;
    }
    Some(bucket)
}

#[test]
fn shard_whose_rename_is_not_proven_durable_is_not_consumed_and_is_not_retired() {
    let sandbox = tempfile::tempdir().unwrap();
    let inbox_dir = sandbox.path().join("inbox");
    let stage = sandbox.path().join("stage");
    let locked = sandbox.path().join("locked-bucket");
    let source = write_bundle(&inbox_dir);

    let Some(_bucket) = rig_unfsyncable_bucket(&stage, &locked) else {
        eprintln!("b74: sandbox cannot make a directory unreadable (root?), case skipped");
        return;
    };

    let report = inbox::ingest(&inbox_dir, &stage, MACHINE).unwrap();
    // Restore before any assertion can unwind past the tempdir teardown.
    mode(&locked, 0o700);

    let shard = locked.join("000001.jsonl");
    assert!(
        shard.exists(),
        "precondition: the shard bytes did land — the only thing that failed is the \
         durability proof for the rename, which is exactly the step under test"
    );

    assert!(
        report.consumed.is_empty(),
        "a shard whose rename was never proven durable must not be reported as consumed; \
         consumed={:?}",
        report
            .consumed
            .iter()
            .map(|c| c.source_file.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        report.errors.len(),
        1,
        "the failure must be visible to the user as exactly one per-file error; errors={:?}",
        report.errors
    );
    let message = report.errors[0].message.to_ascii_lowercase();
    assert!(
        message.contains("durable") || message.contains("fsync"),
        "the error must name the durability proof, not something generic; message={}",
        report.errors[0].message
    );

    assert!(
        source.exists(),
        "the inbox file must stay in the inbox: it is the ONLY thing that makes the next run \
         able to redo this ingest — `consumed/` is never rescanned"
    );
    assert!(
        !inbox_dir.join("consumed").join(BUNDLE_FILE).exists(),
        "the inbox file must not be retired behind an unproven seal"
    );
}

/// The re-run this fix buys: with the directory readable again, the very next
/// ingest over the untouched inbox file recognises the already-sealed bytes via
/// `fileSha256` and reports a duplicate instead of writing a second shard. This
/// is what makes "fatal for this one file" safe rather than obstructive.
#[test]
fn the_next_run_recovers_the_unproven_seal_through_content_dedup() {
    let sandbox = tempfile::tempdir().unwrap();
    let inbox_dir = sandbox.path().join("inbox");
    let stage = sandbox.path().join("stage");
    let locked = sandbox.path().join("locked-bucket");
    let source = write_bundle(&inbox_dir);

    let Some(bucket) = rig_unfsyncable_bucket(&stage, &locked) else {
        eprintln!("b74: sandbox cannot make a directory unreadable (root?), case skipped");
        return;
    };

    let first = inbox::ingest(&inbox_dir, &stage, MACHINE).unwrap();
    mode(&locked, 0o700);
    assert_eq!(first.errors.len(), 1, "first run must fail visibly");
    assert!(
        source.exists(),
        "first run must leave the file in the inbox"
    );

    // Drop the rig before the recovery run. The symlink exists only to inject
    // the fsync failure; leaving it in place would also change what the *dedup*
    // scan sees (`sealed_shard_entries` classifies entries with
    // `DirEntry::file_type`, so a symlinked bucket is skipped) and the second
    // run would be measuring the rig instead of the recovery. A real machine
    // coming back up sees an ordinary readable bucket directory, so that is what
    // the recovery run gets.
    fs::remove_file(&bucket).unwrap();
    fs::rename(&locked, &bucket).unwrap();

    let second = inbox::ingest(&inbox_dir, &stage, MACHINE).unwrap();
    assert!(
        second.errors.is_empty(),
        "the recovered run must be clean; errors={:?}",
        second.errors
    );
    assert_eq!(
        second.duplicates.len(),
        1,
        "the already-sealed bytes must be recognised by fileSha256, not sealed twice; \
         consumed={} duplicates={}",
        second.consumed.len(),
        second.duplicates.len()
    );
    assert!(
        !source.exists() && inbox_dir.join("consumed").join(BUNDLE_FILE).exists(),
        "the recovered run must retire the file"
    );
    assert!(
        !bucket.join("000002.jsonl").exists(),
        "no second shard may be written for the same bytes"
    );
}

/// Guard rail: the ordinary ingest path is untouched — no new failure, no new
/// error entry, same seal-then-retire outcome.
#[test]
fn ordinary_ingest_still_consumes_seals_and_retires() {
    let sandbox = tempfile::tempdir().unwrap();
    let inbox_dir = sandbox.path().join("inbox");
    let stage = sandbox.path().join("stage");
    let source = write_bundle(&inbox_dir);

    let report = inbox::ingest(&inbox_dir, &stage, MACHINE).unwrap();

    assert!(
        report.errors.is_empty(),
        "the normal path must not gain a failure; errors={:?}",
        report.errors
    );
    assert_eq!(report.consumed.len(), 1, "one bundle in, one shard out");
    assert_eq!(report.consumed[0].shard, "000001.jsonl");
    assert!(
        stage
            .join("sessions")
            .join(MACHINE)
            .join(SESSION_ID)
            .join("000")
            .join("000001.jsonl")
            .exists(),
        "the sealed shard must be where the report says it is"
    );
    assert!(!source.exists(), "the consumed file must leave the inbox");
    assert!(
        inbox_dir.join("consumed").join(BUNDLE_FILE).exists(),
        "the consumed file must be retired, not deleted"
    );
}
