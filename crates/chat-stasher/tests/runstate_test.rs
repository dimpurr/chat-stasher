//! Timer visibility: the three answers `status` must be able to give.
//!
//! Time is injected (`summarize(read, now_unix, stale_after)`), never read
//! from the system clock, so the "nothing has run for days" case is tested
//! without waiting for days.

use chat_stasher::runstate::{self, RunOutcome, RunState, RunStateRead};

const HOUR: u64 = 3600;
/// A fixed, arbitrary "now" — the tests only use differences from it.
const NOW: u64 = 1_760_000_000;

fn ok_state(finished_at_unix: u64) -> RunState {
    let mut state = RunState::new(RunOutcome::Completed, None, "test-machine", 1234);
    state.finished_at_unix = finished_at_unix;
    state.shards_written = 3;
    state.stage_shards = 3;
    state
}

/// Case 1 — no record at all. An absent file is not evidence of health, so it
/// must be said out loud and must fail.
#[test]
fn never_ran_is_reported_as_unknown_not_as_healthy() {
    let verdict = runstate::summarize(&RunStateRead::Missing, NOW, 4 * HOUR);
    assert!(
        !verdict.healthy,
        "missing run-state must not be healthy: {verdict:?}"
    );
    assert!(
        verdict.line.contains("还没有任何运行记录"),
        "must say there is no record at all, got: {}",
        verdict.line
    );
    assert!(
        !verdict.line.contains("正常"),
        "must never claim things are fine, got: {}",
        verdict.line
    );
}

/// Case 2 — recent success.
#[test]
fn recent_success_is_healthy_and_reports_what_landed() {
    let read = RunStateRead::Present(ok_state(NOW - 600));
    let verdict = runstate::summarize(&read, NOW, 4 * HOUR);
    assert!(
        verdict.healthy,
        "recent success must be healthy: {verdict:?}"
    );
    assert!(
        verdict.line.contains("正常") && verdict.line.contains('3'),
        "must report the shard count that landed, got: {}",
        verdict.line
    );
}

/// Case 3a — the last run failed.
#[test]
fn last_run_failure_is_surfaced_with_the_failing_step() {
    let mut state = ok_state(NOW - 600);
    state.outcome = RunOutcome::Error;
    state.failed_step = Some("push".to_string());
    state.snapshot_created = false;
    let verdict = runstate::summarize(&RunStateRead::Present(state), NOW, 4 * HOUR);
    assert!(
        !verdict.healthy,
        "failed run must not be healthy: {verdict:?}"
    );
    assert!(
        verdict.line.contains("失败") && verdict.line.contains("push"),
        "must name the failing step, got: {}",
        verdict.line
    );
}

/// Case 3b — THE important one. A broken timer does not report an error, it
/// just stops firing, leaving a perfectly *successful* last run behind.
/// Checking only `outcome` would call this healthy forever.
#[test]
fn a_stopped_timer_is_caught_even_though_the_last_run_succeeded() {
    let state = ok_state(NOW - 9 * 86_400);
    assert_eq!(
        state.outcome,
        RunOutcome::Completed,
        "premise: the last recorded run succeeded"
    );
    let verdict = runstate::summarize(&RunStateRead::Present(state), NOW, 4 * HOUR);
    assert!(
        !verdict.healthy,
        "9 days with no run must not be healthy even after a success: {verdict:?}"
    );
    assert!(
        verdict.line.contains("没有运行") || verdict.line.contains("定时器"),
        "must say it stopped running, got: {}",
        verdict.line
    );
}

/// A file that exists but cannot be parsed is its own answer: not "never ran",
/// and certainly not "healthy".
#[test]
fn unreadable_record_is_neither_missing_nor_healthy() {
    let read = RunStateRead::Unreadable("malformed json".to_string());
    let verdict = runstate::summarize(&read, NOW, 4 * HOUR);
    assert!(!verdict.healthy);
    assert!(!verdict.line.contains("还没有任何运行记录"));
}

/// Round-trip through the real atomic write, in an isolated temp state dir.
#[test]
fn save_then_load_round_trips_and_leaves_no_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    assert!(matches!(runstate::load(&state_dir), RunStateRead::Missing));

    let mut written = ok_state(NOW - 60);
    written.outcome = RunOutcome::Noop;
    written.snapshot_created = false;
    runstate::save(&state_dir, &written).unwrap();

    match runstate::load(&state_dir) {
        RunStateRead::Present(read) => {
            assert_eq!(read.outcome, RunOutcome::Noop);
            assert_eq!(read.shards_written, 3);
            assert_eq!(read.finished_at_unix, NOW - 60);
            assert_eq!(read.machine_digest.len(), 12);
        }
        other => panic!("expected a present state, got {other:?}"),
    }

    let leftovers: Vec<_> = std::fs::read_dir(&state_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
}

/// The record must not carry the machine name or any session text.
#[test]
fn record_never_contains_the_hostname() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    runstate::save(
        &state_dir,
        &RunState::new(RunOutcome::Noop, None, "very-distinctive-hostname", 5),
    )
    .unwrap();
    let raw = std::fs::read_to_string(runstate::run_state_path(&state_dir)).unwrap();
    assert!(
        !raw.contains("very-distinctive-hostname"),
        "run-state must store a digest, not the hostname"
    );
}
