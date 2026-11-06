//! `read`'s steps: traced before they run, and bounded by a deadline (W1052).
//!
//! The observed incident: `read` printed its five pre-flight lines and then
//! answered nothing — neither the report nor an exit — for a 1373-byte
//! session, because the archive walk sat on a backend call that opens a
//! connection with no timeout of its own (`store::repository_is_local`
//! documents that gap from the *other* side: shared read paths refuse
//! backend-shaped repositories precisely because `list()` can hang). The
//! whole command's answer was hostage to one step that never answered, and a
//! user staring at five lines had no culprit named and no exit code.
//!
//! `doctor` fixed the same shape for its own report in W1043, and this module
//! is that fix's read-path sibling, stated by contract rather than by copy:
//!
//! 1. **A trace before every step.** The step prints what it is about to do
//!    *before* it runs, so a step that wedges leaves the culprit named on a
//!    line that was already flushed — the last thing on screen is the thing
//!    that never answered.
//! 2. **A deadline per step.** A step that outlives its budget is abandoned:
//!    its worker thread is detached where it is (joining it would be the hang
//!    again, the recorded W1043 lesson), the caller is handed
//!    [`StepTimedOut`], and it words the step as *unknown* and exits in the
//!    "did not finish" family — `3`, never `1`, because a deadline is a
//!    decision about the clock, and collapsing it into a failure would claim
//!    the archive was read.
//! 3. **A panic propagates unchanged.** The worker resumes the payload back on
//!    the caller's thread ([`std::panic::resume_unwind`]); a panicking step is
//!    a bug to fix, not an unknown to word around — the same line
//!    `doctor::CheckRunner` holds, preserved here so the two runners cannot
//!    drift apart on the point a `catch_unwind` would silently change.
//!
//! This is not `verify`'s rejected wall-clock limit. [`crate::reader_guard`]
//! records why a whole-archive limit was refused there: it cannot tell a
//! wedged reader from a slow one, and `verify` walks the whole archive, where
//! slow is normal. `read --session` walks one session through stages of
//! bounded work; the budgets below are sized so a healthy read fits in a
//! fraction of them, and every one of them is replaceable from the
//! environment for the fixtures that prove the abandonment happens. A budget
//! that starts cutting legitimately slow runs is a bug to file against the
//! budget, not a reason to take the deadline back out.

use std::time::Duration;

/// Replaces **every** read-step budget for one run (the W1043 override's
/// sibling, named for `read` because the budgets it replaces are the read
/// path's own, not `doctor`'s).
pub const READ_STEP_BUDGET_ENV: &str = "CHAT_STASHER_READ_STEP_BUDGET_SECS";

/// The configuration file read carries `doctor`'s precedent budget (W1043f):
/// a config that never answers gets one bounded attempt, never a wedged run.
pub const CONFIG_FILE_BUDGET_SECS: u64 = 30;

/// The machine-identity resolution: one identity file read plus one write
/// when the identity is being created — local, but a wedged filesystem
/// deserves the same named exit an unresponsive remote gets.
pub const IDENTITY_BUDGET_SECS: u64 = 30;

/// The body-cache and mirror decision walk this machine's own directories
/// (the tiers a read consults before it touches the destination);
/// `doctor`'s local-walk precedent (D6/D9) is 90 s.
pub const LOCAL_TIERS_BUDGET_SECS: u64 = 90;

/// The masterkey file read — one local file, same shape as the config one.
pub const KEY_FILE_BUDGET_SECS: u64 = 30;

/// One session's archive read. This is not a small number guessed small: the
/// incident's own archive answered the observed 1373-byte session in 1058 s
/// wall clock — ~70-130 s of sftp session stall the backend retries through
/// silently, then a cumulative walk that streams and zstd-decodes every tree
/// of every snapshot the session's machine partition has, newest first,
/// until the session is found (an old session is not in the newest
/// snapshot; that is the reclaim design, ADR-021). 1800 s leaves that
/// measured healthy reality ~70% headroom, and a read that outlives it is
/// worded as unknown — never as an empty or absent session. The override
/// below lifts it; a budget is the wrong place to be stingy, and the wrong
/// place to be infinite.
pub const ARCHIVE_READ_BUDGET_SECS: u64 = 1800;

/// `read --all-machines` reads and hashes **every** shard of every machine, a
/// bulk operation by design, so its budget is a whole hour rather than a
/// slow-link guess. A pass that outlives it is worded exactly like the
/// single-session timeout, not as a failure of the archive.
pub const ALL_MACHINES_READ_BUDGET_SECS: u64 = 3600;

/// The ssh-master teardown that ends a remote run: local `ssh -O exit` calls
/// bounded so the exit after a finished read cannot become the hang the
/// report already survived.
pub const SSH_REAP_BUDGET_SECS: u64 = 30;

/// One step that outlived its deadline — an unknown, by construction: the
/// step did not answer, it did not measure anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepTimedOut {
    /// The step's label — the words the trace printed and the timeout
    /// sentences reuse, one wording per step per run.
    pub step: String,
    /// The per-step budget that expired, in seconds.
    pub budget_secs: u64,
}

impl StepTimedOut {
    /// The budget as printed. One place formats it so the trace, the note and
    /// the exit line cannot disagree about the same number.
    pub fn budget_word(&self) -> String {
        format!("{}s", self.budget_secs)
    }
}

/// The budget one step will actually run under: the per-step default, unless
/// [`READ_STEP_BUDGET_ENV`] replaces every step's budget for this run.
pub fn budget_secs(default_secs: u64) -> u64 {
    std::env::var(READ_STEP_BUDGET_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(default_secs)
}

/// Trace `trace`, then run `step` behind its deadline.
///
/// `Ok(value)` is the step's answer, whatever it measured — a finished step
/// is never hidden behind the budget. `Err([`StepTimedOut`])` is exclusively
/// the deadline: the step outlived it and was abandoned. A panic inside the
/// step propagates to the caller unchanged. The trace goes to stdout where the
/// `read` family lives, so the last line on screen names the step that is
/// wedging; errors and timeout verdicts stay the CLI's stderr concern.
///
/// The worker is spawned and then never joined on the timeout path — a
/// detached thread that keeps whatever it was blocked on until the process
/// exits, the same abandonment `doctor::CheckRunner` records: here the
/// blocked thing is a read, so the only thing the leak holds is the wait the
/// step was already stuck in. A spawn failure is the one branch that must not
/// lose the question it was handed: the closure travels in a carrier the
/// caller-side recovery re-takes, and the step runs unbounded — the bound was
/// already dead without a thread to hold it.
pub fn run_behind_deadline<T, F>(
    trace: &str,
    step: &str,
    default_budget_secs: u64,
    f: F,
) -> Result<T, StepTimedOut>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    println!("{trace}");
    run_deadline_body(step, default_budget_secs, f)
}

/// The same deadline with the trace on **stderr** — ISSUE-007 S3's shape for
/// the commands whose stdout is a machine-parsed surface (`search --json`
/// promises one JSON object and nothing else, `export`'s stdout carries the
/// plan and report a user pipes onward), so "every step announces itself
/// before it runs" keeps holding there without a step-trace line landing in
/// the wrong channel.
///
/// Identical contract to [`run_behind_deadline`] in every other way — same
/// budget override, same abandonment on timeout, same panic propagation.
pub fn run_behind_deadline_quiet<T, F>(
    trace: &str,
    step: &str,
    default_budget_secs: u64,
    f: F,
) -> Result<T, StepTimedOut>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    eprintln!("{trace}");
    run_deadline_body(step, default_budget_secs, f)
}

/// The deadline body the two public spellings share: everything after the
/// trace line, split out so the trace channel is the only difference
/// between them and neither can drift from the other's mechanics.
fn run_deadline_body<T, F>(step: &str, default_budget_secs: u64, f: F) -> Result<T, StepTimedOut>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let budget = Duration::from_secs(budget_secs(default_budget_secs));
    // The carrier keeps the closure reachable from this thread even if
    // `Builder::spawn` refuses to create a worker, so a machine out of
    // threads still answers the question instead of dropping it.
    let carrier = std::sync::Arc::new(std::sync::Mutex::new(Some(f)));
    let (answer, answers) = std::sync::mpsc::channel();
    let carried = std::sync::Arc::clone(&carrier);
    let worker = std::thread::Builder::new()
        .name("read-step".to_string())
        .spawn(move || {
            let taken = match carried.lock() {
                Ok(mut slot) => slot.take(),
                Err(poisoned) => poisoned.into_inner().take(),
            };
            if let Some(f) = taken {
                #[allow(
                    clippy::let_underscore_must_use,
                    reason = "a send that fails means the caller's budget already expired and the \
                              receiver is gone — the answer is rightfully discarded, and the error \
                              says nothing either side should act on"
                )]
                let _ = answer.send(f());
            }
        });
    let handle = match worker {
        Ok(handle) => handle,
        Err(_) => {
            // reason: a spawn failure is the bound itself being unenforceable
            // (no thread to run the watcher), so the honest answer is the
            // step's own — the alternative, refusing a read because the
            // machine was out of threads, invents a failure nothing measured.
            let recovered = match carrier.lock() {
                Ok(mut slot) => slot.take(),
                Err(poisoned) => poisoned.into_inner().take(),
            };
            match recovered {
                Some(f) => return Ok(f()),
                None => {
                    std::unreachable!(
                        "the read-step carrier was stripped by a worker that was never created"
                    );
                }
            }
        }
    };
    match answers.recv_timeout(budget) {
        Ok(value) => Ok(value),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(StepTimedOut {
            step: step.to_string(),
            budget_secs: budget.as_secs(),
        }),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            // The worker ended without answering: a panic. Hand its payload
            // back to this thread and let it unwind exactly as it would have
            // without the deadline.
            match handle.join() {
                Ok(()) => std::unreachable!(
                    "a read-step worker only ends without answering by panicking, and the carrier \
                     is only stripped by a thread that never existed"
                ),
                Err(payload) => std::panic::resume_unwind(payload),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The paved path: a step that answers within its budget passes its
    /// value through untouched — the deadline guards the wedged case, not
    /// the answered one.
    #[test]
    fn a_fast_step_passes_its_answer_through() {
        let outcome: Result<u64, StepTimedOut> = run_behind_deadline("trace", "step", 30, || 42);
        assert_eq!(outcome.expect("a fast step answers"), 42);
    }

    /// The wedged case this module exists for: a step that outlives its
    /// budget is abandoned, not joined, and the verdict is the timeout with
    /// the budget that expired. The worker stays blocked; this test ends and
    /// the process keeps running, which is exactly the property read needs at
    /// the moment its archive walk stops answering.
    #[test]
    fn a_wedged_step_is_abandoned_after_its_budget() {
        let started = std::time::Instant::now();
        let outcome: Result<(), StepTimedOut> =
            run_behind_deadline("trace", "wedged-step", 1, || {
                std::thread::sleep(Duration::from_secs(300));
            });
        let timed_out = outcome.expect_err("a wedged step must not answer");
        assert_eq!(timed_out.step, "wedged-step");
        assert_eq!(timed_out.budget_secs, 1, "the note carries the budget");
        assert_eq!(timed_out.budget_word(), "1s");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the abandonment happens at the budget, not at the worker's own end; waited {:?}",
            started.elapsed()
        );
    }

    /// A panicking step is a bug to fix, not an unknown to word around: the
    /// payload resumes on the caller's thread exactly as it would have
    /// without the deadline.
    #[test]
    fn a_panicking_step_propagates_its_panic() {
        let outcome = std::panic::catch_unwind(|| {
            let _unused: Result<(), StepTimedOut> =
                run_behind_deadline("trace", "step", 30, || {
                    panic!("boom: the payload must survive the deadline")
                });
        });
        let payload = outcome.expect_err("the panic must propagate, not become a timeout");
        // A `panic!` with a literal message and no arguments carries
        // `&'static str`; formatting one produces `String`. Accept both, the
        // same two shapes `reader_guard::payload_note` accepts, so the
        // assertion pins the propagation, not the payload's type.
        let text = payload
            .downcast_ref::<&'static str>()
            .map(|text| (*text).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .expect("the panic payload carries its message");
        assert!(
            text.contains("the payload must survive the deadline"),
            "the payload survives: {text}"
        );
    }

    /// Env mutation is process-global, so the budget override test serializes
    /// through this lock — the same discipline the W1043 fixtures established
    /// for the one in-process env swap a suite needs.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The stderr-trace spelling (ISSUE-007 S3): same pass-through, same
    /// timeout shape, same budget — the only difference this test can see
    /// from the outside is the step verdict, which must be identical.
    #[test]
    fn the_quiet_spelling_passes_answers_and_times_out_the_same() {
        let answer: Result<u64, StepTimedOut> =
            run_behind_deadline_quiet("trace line", "fast-quiet-step", 30, || 7);
        assert_eq!(answer.expect("a fast quiet step answers"), 7);
        let outcome: Result<(), StepTimedOut> =
            run_behind_deadline_quiet("trace line", "wedged-quiet-step", 1, || {
                std::thread::sleep(Duration::from_secs(300));
            });
        let timed_out = outcome.expect_err("a wedged quiet step must not answer");
        assert_eq!(timed_out.step, "wedged-quiet-step");
        assert_eq!(timed_out.budget_secs, 1);
    }

    /// The override the fixtures rely on: `CHAT_STASHER_READ_STEP_BUDGET_SECS`
    /// replaces the default only while it parses — every malformed shape keeps
    /// the step's own default rather than inventing a budget the user did not
    /// write.
    #[test]
    fn the_env_override_replaces_the_default_budget() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::remove_var(READ_STEP_BUDGET_ENV);
        assert_eq!(budget_secs(600), 600);
        assert_eq!(budget_secs(30), 30, "each step keeps its own default");
        std::env::set_var(READ_STEP_BUDGET_ENV, "2");
        assert_eq!(budget_secs(600), 2);
        std::env::set_var(READ_STEP_BUDGET_ENV, "yes");
        assert_eq!(
            budget_secs(600),
            600,
            "a value that does not parse keeps the step's default"
        );
        std::env::set_var(READ_STEP_BUDGET_ENV, "0");
        assert_eq!(
            budget_secs(600),
            600,
            "a zero budget would time the step out before it starts; the step's default stands"
        );
        std::env::remove_var(READ_STEP_BUDGET_ENV);
    }
}
