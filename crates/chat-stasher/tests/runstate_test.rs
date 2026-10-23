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
        verdict.line.contains("No run has ever been recorded"),
        "must say there is no record at all, got: {}",
        verdict.line
    );
    assert!(
        !verdict.line.contains("Healthy:"),
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
        verdict.line.contains("Healthy:") && verdict.line.contains('3'),
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
        verdict.line.contains("Last run failed") && verdict.line.contains("push"),
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
        verdict.line.contains("No run for") || verdict.line.contains("timer may have stopped"),
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
    assert!(!verdict.line.contains("No run has ever been recorded"));
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

/// A pass that archived cleanly but left the declared Letta producer unread
/// is two lines, not one unhealthy verdict: the archive cycle's health stays
/// worded as healthy, the producer's own failure prints as the second line
/// with its fix, and the overall verdict is still not healthy — the exit
/// code must not let the run read as success.
#[test]
fn producer_only_failure_is_two_lines_not_one_unhealthy_verdict() {
    let mut state = ok_state(NOW - 600);
    state.outcome = RunOutcome::Error;
    state.failed_step = Some("pull-letta".to_string());
    state.snapshot_created = true;
    state.pull_letta_unread = true;
    let verdict = runstate::summarize(&RunStateRead::Present(state), NOW, 4 * HOUR);
    assert!(!verdict.healthy, "the exit code stays non-zero");
    assert!(
        verdict.line.starts_with("Archive cycle healthy:"),
        "the first line reports the archive cycle, got: {}",
        verdict.line
    );
    let letta = verdict
        .letta
        .as_ref()
        .expect("the producer's failure has its own line");
    assert!(
        letta.sentence.contains("did not finish reading"),
        "the producer line keeps the exit-3 vocabulary: {}",
        letta.sentence
    );
    let fix = letta
        .fix
        .as_ref()
        .expect("a pull failure without progress carries a fix");
    assert!(
        fix.contains("pull.letta.api_key") && fix.contains("file:") && fix.contains("keychain:"),
        "the fix names the timer-reachable credential forms: {fix}"
    );
}

/// While the producer is still catching a large account up across rounds,
/// the second line is progress, not a failure sentence — and the pass still
/// records "did not finish reading" honestly, because it did not.
#[test]
fn backfill_progress_has_its_own_line_and_never_claims_completion() {
    let mut state = ok_state(NOW - 600);
    state.outcome = RunOutcome::Error;
    state.failed_step = Some("pull-letta".to_string());
    state.pull_letta_unread = true;
    state.pull_letta_backfill = Some(chat_stasher::runstate::PullLettaBackfill {
        pulled: 8,
        estimated_total: 25,
    });
    let verdict = runstate::summarize(&RunStateRead::Present(state), NOW, 4 * HOUR);
    assert!(!verdict.healthy);
    let letta = verdict.letta.as_ref().expect("the progress line exists");
    assert!(
        letta.sentence.contains("backfilling") && letta.sentence.contains("8 of ~25"),
        "the line reports pulled-of-estimated progress: {}",
        letta.sentence
    );
    assert!(
        letta.sentence.contains("did not finish reading"),
        "progress does not turn an unfinished pass into a finished one"
    );
    assert!(
        letta.fix.is_none(),
        "progress needs no fix text; the estimate is the answer"
    );
}

/// The sandwich state of a large backfill: every conversation is archived,
/// and what the pass still owes is the agent-level material of a round
/// that was cut short. `pulled` is a conversation count, so the line must
/// not word that state as "N of ~N conversations" — an unarchived
/// conversation every archive record contradicts — and the agent-level
/// remainder is named for what it is (rev3's caught-up-from-empty-position
/// hole, surfaced as its own wording).
#[test]
fn a_position_with_every_conversation_published_names_its_agent_level_remainder() {
    let mut state = ok_state(NOW - 600);
    state.outcome = RunOutcome::Error;
    state.failed_step = Some("pull-letta".to_string());
    state.pull_letta_unread = true;
    state.pull_letta_backfill = Some(chat_stasher::runstate::PullLettaBackfill {
        pulled: 25,
        estimated_total: 25,
    });
    let verdict = runstate::summarize(&RunStateRead::Present(state), NOW, 4 * HOUR);
    assert!(!verdict.healthy);
    let letta = verdict.letta.as_ref().expect("the progress line exists");
    assert!(
        letta.sentence.contains("agent-level material"),
        "the remainder is named for what it is: {}",
        letta.sentence
    );
    assert!(
        !letta.sentence.contains("25 of ~25"),
        "a fully published conversation position is not worded as an \
         unarchived conversation: {}",
        letta.sentence
    );
    assert!(
        letta.sentence.contains("did not finish reading"),
        "the pass is honestly unfinished while the material is owed"
    );
}

/// A pass that deferred to the sibling timer holding the producer's lock is
/// the recurring shape of a multi-destination install, so its line is the
/// hand-off itself: the first line stays the green verdict, the producer's
/// line names the deferral and never borrows the "did not finish reading"
/// vocabulary or a fix, because the winning pass reported the pull.
#[test]
fn deferred_producer_renders_the_hand_off_beside_a_green_verdict() {
    let mut state = ok_state(NOW - 600);
    state.pull_letta_deferred = true;
    let verdict = runstate::summarize(&RunStateRead::Present(state), NOW, 4 * HOUR);
    assert!(verdict.healthy, "a deferred producer is a healthy pass");
    assert!(
        verdict.line.starts_with("Healthy:"),
        "the first line is the archive cycle's own green sentence: {}",
        verdict.line
    );
    let letta = verdict
        .letta
        .as_ref()
        .expect("the deferral is rendered, never silent");
    assert!(
        letta.sentence.contains("deferred"),
        "the producer's line names the hand-off: {}",
        letta.sentence
    );
    assert!(
        !letta.sentence.contains("did not finish reading"),
        "the deferral is not worded as a read failure: {}",
        letta.sentence
    );
    assert!(
        letta.fix.is_none(),
        "a hand-off carries no fix — nothing is broken: {letta:?}"
    );
}

/// The two facts stay two facts even when the local cycle failed too: a
/// collect failure keeps its step-named line, and the producer's line stays
/// beside it rather than being erased by the earlier failure (the durable
/// record keeps both, so the answer does too).
#[test]
fn a_local_failure_does_not_erase_the_producer_fact() {
    let mut state = ok_state(NOW - 600);
    state.outcome = RunOutcome::Error;
    state.failed_step = Some("collect".to_string());
    state.pull_letta_unread = true;
    let verdict = runstate::summarize(&RunStateRead::Present(state), NOW, 4 * HOUR);
    assert!(!verdict.healthy);
    assert!(
        verdict.line.contains("Last run failed") && verdict.line.contains("collect"),
        "the local failure keeps its step-named line: {}",
        verdict.line
    );
    assert!(
        verdict.letta.is_some(),
        "the producer's own line survives the local failure"
    );
}

/// Older records (written before the producer fields existed) still parse,
/// with the new fields at their defaults rather than refusing the read.
#[test]
fn a_record_without_the_producer_fields_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(
        runstate::run_state_path(&state_dir),
        r#"{ "version": 1, "finished_at_unix": 100, "duration_ms": 5,
             "outcome": "completed", "failed_step": null, "shards_written": 0,
             "stage_shards": 0, "snapshot_created": false,
             "collect_errors": 0, "archive_gaps": 0,
             "machine_digest": "aaaaaaaaaaaa" }"#,
    )
    .unwrap();
    match runstate::load(&state_dir) {
        RunStateRead::Present(state) => {
            assert!(!state.pull_letta_unread);
            assert!(state.pull_letta_backfill.is_none());
        }
        other => panic!("a pre-producer record must load, got {other:?}"),
    }
}
