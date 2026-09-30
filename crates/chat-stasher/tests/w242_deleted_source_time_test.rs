//! W242 item 4 — a session whose source file is gone but whose shard is still
//! in the archive: where does its conversation time come from?
//!
//! `M2-PLAN.md:58-66` leaves this open: a session deleted at the source but
//! still in the archive — is its timestamp recomputed by reading the archive
//! back, or marked unknown? ADR-017 made conversation time a *derived* index,
//! which is what makes the question answerable at all: ADR-012's shape is
//! "the cursor is a verifiable cache, the archive is the truth", so an index can
//! always be recomputed from the archive — expensively, but possibly.
//!
//! The answer, measured here: **read back**. `activity-index --rebuild` restores
//! a session's shards from the repository and re-derives the index from their
//! contents, so a session whose source no longer exists keeps its real
//! conversation time. It is never the shard's mtime, never the snapshot time,
//! and never `unknown`.
//!
//! The fixture makes all three distinguishable at once:
//!
//! * the only in-content timestamp is 2025-01-15;
//! * the shard file's mtime is forced to 2001-01-01;
//! * the snapshot is written at the moment the test runs.
//!
//! The archive is built *without* an activity index on purpose. If the index
//! were already in the snapshot, a later `search` could be reading the index it
//! was pushed with, and the test would not show that anything was re-derived.
//! With no index anywhere until the rebuild, a placement on 2025-01-15 can only
//! have come from the archived shard bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};

const MACHINE: &str = "mbp-deleted-source";
const SESSION: &str = "claude-code.mbp-deleted-source.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
/// 2025-01-15T12:34:56Z and 2025-01-15T13:45:07Z.
const FIRST_UNIX: i64 = 1_736_944_496;
const LAST_UNIX: i64 = 1_736_948_707;

fn cc_line(ts: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"{SESSION}","type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
    )
}

/// Run the real binary with every ambient path redirected into `sandbox`.
///
/// The sandbox declares `machine = MACHINE`, and that is part of the fixture
/// rather than decoration: `activity-index --rebuild` repairs a partition in
/// the archive only for the machine that owns it, and rebuilds any other
/// machine read-only into a local derived index (ADR-017 — a partition has one
/// writer). This fixture has exactly one machine, whose stage is deleted, so
/// the partition being rebuilt is this machine's own. Without the declaration
/// the sandbox has no identity at all, and the rebuild would rightly refuse to
/// treat a partition it cannot claim as its own.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W242 deleted source","harnesses":[]}"#,
    )
    .unwrap();
    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!("machine = \"{MACHINE}\"\n"),
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

/// The `matched` count and exit code of one `search --machine <M> --since..--until`.
fn search_window(sandbox: &Path, repo: &Path, key: &Path, since: &str, until: &str) -> (i32, u32) {
    let out = run(
        sandbox,
        &[
            "search",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            MACHINE,
            "--since",
            since,
            "--until",
            until,
            "--keep-ssh-masters",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let matched = stdout
        .lines()
        .find(|line| line.contains("] matched"))
        .and_then(|line| line.split(':').nth(1))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| panic!("no `matched` line in search output:\n{stdout}"));
    (out.status.code().unwrap_or(-1), matched)
}

/// The index row for this session, as written by `activity-index`, read from
/// the stage's sidecar file.
fn index_row(stage: &Path) -> serde_json::Value {
    let path = stage.join("meta").join(MACHINE).join("activity-v1.jsonl");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("no activity index at {}: {err}", path.display()));
    let row: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(row["session_id"], SESSION);
    row
}

fn write_shard(stage: &Path) -> PathBuf {
    let dir = stage
        .join("sessions")
        .join(MACHINE)
        .join(SESSION)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    let shard = dir.join("000001.jsonl");
    fs::write(
        &shard,
        cc_line("2025-01-15T12:34:56Z") + "\n" + &cc_line("2025-01-15T13:45:07Z") + "\n",
    )
    .unwrap();
    // A decoy: any implementation that reached for the file's mtime would put
    // this session in 2001 instead.
    let file = fs::OpenOptions::new().write(true).open(&shard).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(978_307_200))
        .unwrap();
    shard
}

fn main_stage(sandbox: &Path) -> PathBuf {
    sandbox.join("stage")
}

/// The whole question in one test: the source is deleted, the archive is not,
/// and the real conversation time comes back.
#[test]
fn a_session_whose_source_is_deleted_keeps_its_conversation_time() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = main_stage(sandbox);
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");

    write_shard(&stage);

    // 1. Archive the shard WITHOUT any activity index — so nothing anywhere
    //    holds a conversation time for it.
    let push = run(
        sandbox,
        &[
            "push",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    assert!(
        push.status.success(),
        "push failed: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    // `push` records the writer version under `meta/`, but no activity index:
    // that is the whole point of this fixture.
    assert!(
        !stage
            .join("meta")
            .join(MACHINE)
            .join("activity-v1.jsonl")
            .exists(),
        "this fixture must archive no activity index, so the rebuild has to derive one"
    );

    // 2. Delete the source. The archive is now the only copy.
    fs::remove_dir_all(&stage).unwrap();
    let workspace = sandbox.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    // 3. Rebuild the index from the archive. The session's shards have to come
    //    back out of the repository for this to produce anything.
    let rebuild = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            workspace.to_str().unwrap(),
            "--machine",
            MACHINE,
            "--rebuild",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    let rebuild_out = format!(
        "{}{}",
        String::from_utf8_lossy(&rebuild.stdout),
        String::from_utf8_lossy(&rebuild.stderr)
    );
    assert!(
        rebuild.status.success(),
        "the readback rebuild failed: {rebuild_out}"
    );
    assert!(
        rebuild_out.contains("sessions : 1"),
        "the rebuild must have restored and indexed the deleted session: {rebuild_out}"
    );

    // 4. The time is the conversation's, and it is neither the shard's mtime
    //    (2001) nor the snapshot's time (now).
    let (code, matched) = search_window(sandbox, &repo, &key, "2025-01-14", "2025-01-17");
    assert_eq!(
        matched, 1,
        "the session must be placed on its conversation day after the source was deleted \
         (exit {code})"
    );
    let (_, in_2001) = search_window(sandbox, &repo, &key, "2000-12-31", "2001-01-02");
    assert_eq!(
        in_2001, 0,
        "the session must not be placed on the deleted file's mtime"
    );

    // 5. The rebuild worked from shards it restored into the workspace and then
    //    cleaned up, so nothing is left there to be the source of a later
    //    reading.
    assert!(
        !workspace.join("sessions").exists(),
        "the rebuild left restored shards behind in its workspace"
    );
}

/// The same measurement taken directly on the index the local source produces,
/// so the two derivations (local stage, and archived shards read back) can be
/// compared rather than assumed equal.
#[test]
fn the_local_index_and_the_readback_index_agree_on_the_conversation_time() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = main_stage(sandbox);
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");

    write_shard(&stage);

    // Local derivation, from the stage.
    let local = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
        ],
    );
    assert!(
        local.status.success(),
        "activity-index failed: {}",
        String::from_utf8_lossy(&local.stderr)
    );
    let row = index_row(&stage);
    assert_eq!(row["first_unix"], FIRST_UNIX);
    assert_eq!(row["last_unix"], LAST_UNIX);
    assert_eq!(
        row["time_source"]["kind"], "exact",
        "a parseable in-content RFC 3339 timestamp is an exact reading"
    );

    // Now archive it and take the same measurement from the archived shards.
    assert!(run(
        sandbox,
        &[
            "push",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    )
    .status
    .success());

    // The sidecar is in the snapshot, so the readback rebuild has something to
    // carry forward; the point here is that it also re-derives the row from the
    // shards it restores, and lands on the same numbers.
    let workspace = sandbox.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let rebuild = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            workspace.to_str().unwrap(),
            "--machine",
            MACHINE,
            "--rebuild",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    assert!(
        rebuild.status.success(),
        "the readback rebuild failed: {}",
        String::from_utf8_lossy(&rebuild.stderr)
    );

    // `search` reads the archived index, so a window on the conversation day
    // that still matches is that index agreeing with the local one.
    // A three-day window: the session's local calendar day depends on the
    // machine's zone, but it always falls inside this range.
    let (_, matched) = search_window(sandbox, &repo, &key, "2025-01-14", "2025-01-17");
    assert_eq!(matched, 1, "the archived index must carry the same day");
}
