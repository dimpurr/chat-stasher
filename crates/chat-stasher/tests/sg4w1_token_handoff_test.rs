//! SG-4-W1 — the run-once token handoff.
//!
//! The stage audit counts the sealed shards and takes a freshness token of the
//! sessions tree. The push preflight used to re-walk and re-stat every machine
//! and session directory to confirm the token (about 12.8k stats on the
//! production stage). Inside one run-once pass it now rechecks a single point,
//! the sessions root's mtime. These tests pin that: the walk is not repeated,
//! a moved root still forces a recount, an unreadable root still fails closed,
//! and the standalone path keeps its full per-directory check.

use chat_stasher::collect::{self, ScanEvidence};
use chat_stasher::store::{self, CountedStageShards};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

/// The walk counter is process-wide; serialize the tests that read it.
static LOCK: Mutex<()> = Mutex::new(());

fn seal(stage: &Path, machine: &str, session: &str, seq: &str) {
    let dir = stage.join("sessions").join(machine).join(session);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{seq}.jsonl")), b"{\"k\":1}\n").unwrap();
}

#[cfg(unix)]
fn set_mtime(path: &Path, when: std::time::SystemTime) {
    use std::os::unix::ffi::OsStrExt;
    use std::time::UNIX_EPOCH;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let secs = when.duration_since(UNIX_EPOCH).unwrap().as_secs() as libc::time_t;
    let t = libc::timeval {
        tv_sec: secs,
        tv_usec: 0,
    };
    let rc = unsafe { libc::utimes(c_path.as_ptr(), [t, t].as_ptr()) };
    assert_eq!(rc, 0);
}

struct Fx {
    _d: tempfile::TempDir,
    stage: std::path::PathBuf,
    state: std::path::PathBuf,
}

fn fx() -> Fx {
    let d = tempfile::TempDir::new().unwrap();
    let stage = d.path().join("stage");
    let state = d.path().join("state");
    fs::create_dir_all(&state).unwrap();
    seal(
        &stage,
        "m-a",
        "019bf00d-0000-7eb2-9bf8-000000000001",
        "000000",
    );
    seal(
        &stage,
        "m-a",
        "019bf00d-0000-7eb2-9bf8-000000000002",
        "000000",
    );
    Fx {
        _d: d,
        stage,
        state,
    }
}

/// The handoff trusts the audit's count after one stat: no tree walk.
#[test]
fn handoff_does_not_rewalk_the_stage_tree() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let f = fx();
    let counted = CountedStageShards::count(&f.stage).unwrap();
    assert_eq!(counted.count, 2);

    store::reset_stage_token_walk_calls();
    store::reset_sealed_shard_count_calls();
    let check = collect::inspect_stage_for_push_with_handoff(
        &ScanEvidence::default(),
        &f.stage,
        &f.state,
        &counted,
    )
    .unwrap();
    assert_eq!(check.stage_shards, 2);
    assert_eq!(
        store::stage_token_walk_calls(),
        0,
        "preflight re-walked the tree"
    );
    assert_eq!(store::sealed_shard_count_calls(), 0);
}

/// The standalone-shaped check is unchanged: it still walks the tree once.
#[test]
fn full_check_still_walks_the_tree() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let f = fx();
    let counted = CountedStageShards::count(&f.stage).unwrap();

    store::reset_stage_token_walk_calls();
    let check = collect::inspect_stage_for_push_with_evidence(
        &ScanEvidence::default(),
        &f.stage,
        &f.state,
        &counted,
    )
    .unwrap();
    assert_eq!(check.stage_shards, 2);
    assert_eq!(store::stage_token_walk_calls(), 1);
}

/// A new machine directory moves the sessions root: the handoff recounts.
///
/// Unix-only: the property needs the sessions root's mtime to move when a
/// child directory is created, and NTFS updates a parent directory's
/// timestamp lazily (not on child creation), with no std way to set it. The
/// root-changed => recount rule is still asserted on every platform by
/// `vanished_sessions_root_is_not_trusted` and
/// `absent_root_then_appearing_root`, which change the root's presence.
#[cfg(unix)]
#[test]
fn moved_sessions_root_forces_a_recount() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let f = fx();
    let counted = CountedStageShards::count(&f.stage).unwrap();

    seal(
        &f.stage,
        "m-b",
        "019bf00d-0000-7eb2-9bf8-000000000003",
        "000000",
    );
    set_mtime(
        &f.stage.join("sessions"),
        std::time::SystemTime::now() + std::time::Duration::from_secs(3600),
    );
    assert_eq!(counted.handed_off_count(&f.stage), None);

    store::reset_sealed_shard_count_calls();
    let check = collect::inspect_stage_for_push_with_handoff(
        &ScanEvidence::default(),
        &f.stage,
        &f.state,
        &counted,
    )
    .unwrap();
    assert_eq!(check.stage_shards, 3);
    assert_eq!(store::sealed_shard_count_calls(), 1);
}

/// A stage that vanished after the audit counted it is not "still fresh".
#[test]
fn vanished_sessions_root_is_not_trusted() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let f = fx();
    let counted = CountedStageShards::count(&f.stage).unwrap();
    fs::remove_dir_all(f.stage.join("sessions")).unwrap();
    assert_eq!(counted.handed_off_count(&f.stage), None);
    let check = collect::inspect_stage_for_push_with_handoff(
        &ScanEvidence::default(),
        &f.stage,
        &f.state,
        &counted,
    )
    .unwrap();
    assert_eq!(check.stage_shards, 0);
}

/// An empty stage counted as empty stays trusted as empty (root absent both
/// times), and a root that appears afterwards forces a recount.
#[test]
fn absent_root_then_appearing_root() {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let d = tempfile::TempDir::new().unwrap();
    let stage = d.path().join("stage");
    let state = d.path().join("state");
    fs::create_dir_all(&state).unwrap();
    let counted = CountedStageShards::count(&stage).unwrap();
    assert_eq!(counted.count, 0);
    assert_eq!(counted.handed_off_count(&stage), Some(0));
    seal(
        &stage,
        "m-a",
        "019bf00d-0000-7eb2-9bf8-000000000001",
        "000000",
    );
    assert_eq!(counted.handed_off_count(&stage), None);
}
