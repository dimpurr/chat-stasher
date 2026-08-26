//! Timer-visibility state: what happened on the last `run-once` pass.
//!
//! The scheduler (launchd/systemd, see [`crate::schedule`]) runs `run-once`
//! with nobody watching stdout. Without a durable record the user cannot tell
//! a healthy timer from one that died months ago — the common failure of a
//! broken timer is not an error message, it is silence.
//!
//! So every `run-once` pass, success *and* failure, writes one small file:
//! `<state_dir>/run-state.json`. `chat-stasher status` reads it back and says
//! one sentence in plain language. Writing is silent: `run-once` prints
//! nothing extra for this feature.
//!
//! Privacy: this file holds counts, byte-free timestamps and a machine
//! *digest* only. No session content, no session id, no hostname.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// File name inside the state dir (`collect::default_state_dir()`).
pub const RUN_STATE_FILE: &str = "run-state.json";

/// Bumped whenever the shape below changes incompatibly. An unknown version
/// is treated as "unreadable", never as "healthy".
pub const RUN_STATE_VERSION: u32 = 1;

/// A run is considered overdue once this many configured intervals have
/// elapsed with no new pass. Four gives a timer three chances to miss (sleep,
/// reboot, one transient failure) before `status` complains.
pub const STALE_INTERVAL_MULTIPLIER: u64 = 4;

/// Floor for the overdue threshold, so a tiny `backup_interval_secs` cannot
/// make `status` cry wolf every few minutes.
pub const STALE_FLOOR_SECS: u64 = 3600;

/// How this pass ended. `Noop` is a success: nothing changed, so no snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunOutcome {
    /// A snapshot was created.
    Completed,
    /// Healthy pass, nothing to archive.
    Noop,
    /// The pass failed. This is the single most valuable record in the file.
    Error,
}

impl RunOutcome {
    pub fn is_failure(self) -> bool {
        matches!(self, RunOutcome::Error)
    }
}

/// One `run-once` pass, as durable metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    pub version: u32,
    /// Wall-clock seconds since the epoch at the moment the pass ended.
    pub finished_at_unix: u64,
    pub duration_ms: u64,
    pub outcome: RunOutcome,
    /// Which step failed (`collect`, `stage-audit`, `push`, `verify`, ...).
    /// `None` on success. Never carries an error message body, so no path or
    /// session text can leak into it.
    pub failed_step: Option<String>,
    /// Shards this pass sealed into the stage.
    pub shards_written: usize,
    /// Sealed shards present in the stage when the pass decided to push.
    pub stage_shards: usize,
    pub snapshot_created: bool,
    pub collect_errors: usize,
    pub archive_gaps: usize,
    /// sha256 prefix of the machine partition name — enough to notice that
    /// two machines share a state dir, without recording the hostname.
    pub machine_digest: String,
}

impl RunState {
    /// Build a record for a pass that ended now.
    pub fn new(
        outcome: RunOutcome,
        failed_step: Option<&str>,
        machine: &str,
        duration_ms: u64,
    ) -> Self {
        Self {
            version: RUN_STATE_VERSION,
            finished_at_unix: now_unix(),
            duration_ms,
            outcome,
            failed_step: failed_step.map(str::to_string),
            shards_written: 0,
            stage_shards: 0,
            snapshot_created: matches!(outcome, RunOutcome::Completed),
            collect_errors: 0,
            archive_gaps: 0,
            machine_digest: machine_digest(machine),
        }
    }
}

pub fn run_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join(RUN_STATE_FILE)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        // reason: NOT an honest default — recorded as (a)-fragile, unreachable today.
        // A clock before 1970 makes this 0, which lands in `finished_at_unix`; then
        // `summarize` computes `now - 0` and states "55 years since the last run" —
        // a confidently wrong sentence, which is the failure mode this repo cares
        // about most. It is left as-is only because the branch needs a pre-1970
        // system clock to reach. Do not copy this shape; if it ever becomes
        // reachable, make it Option and say "unknown".
        .unwrap_or(0)
}

/// 12 hex chars of sha256 — an identifier, not a hostname.
pub fn machine_digest(machine: &str) -> String {
    let out = Sha256::digest(machine.as_bytes());
    out.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(12)
        .collect()
}

/// Persist the record crash-safely: temp file + fsync + atomic rename.
/// Same shape as `collect::save_state` (src/collect.rs:1189-1195).
pub fn save(state_dir: &Path, state: &RunState) -> anyhow::Result<()> {
    fs::create_dir_all(state_dir)
        .with_context(|| format!("create state dir {}", state_dir.display()))?;
    let path = run_state_path(state_dir);
    let tmp = path.with_file_name(format!(".{RUN_STATE_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(state).context("serialise run state")?;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// What a read of the state file can tell us. A file that exists but cannot
/// be parsed is its own case: it is *not* "never ran" and *not* "healthy".
#[derive(Debug, Clone)]
pub enum RunStateRead {
    Missing,
    Unreadable(String),
    Present(RunState),
}

pub fn load(state_dir: &Path) -> RunStateRead {
    let path = run_state_path(state_dir);
    match fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RunStateRead::Missing,
        Err(e) => RunStateRead::Unreadable(e.kind().to_string()),
        Ok(bytes) => match serde_json::from_slice::<RunState>(&bytes) {
            Ok(state) if state.version == RUN_STATE_VERSION => RunStateRead::Present(state),
            Ok(state) => RunStateRead::Unreadable(format!("unknown version {}", state.version)),
            Err(_) => RunStateRead::Unreadable("malformed json".to_string()),
        },
    }
}

/// The overdue threshold derived from the configured cadence.
pub fn stale_after_secs(interval_secs: u64) -> u64 {
    interval_secs
        .saturating_mul(STALE_INTERVAL_MULTIPLIER)
        .max(STALE_FLOOR_SECS)
}

/// One human sentence plus the exit decision behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub line: String,
    /// false → `status` must exit non-zero.
    pub healthy: bool,
}

/// Turn a read into one sentence. `now_unix` is a parameter, never read from
/// the clock here, so tests can place a run arbitrarily far in the past
/// without waiting for real time to pass.
pub fn summarize(read: &RunStateRead, now_unix: u64, stale_after_secs: u64) -> Verdict {
    match read {
        // "Never ran" must never be reported as fine: an absent record is the
        // absence of evidence, not evidence of health.
        RunStateRead::Missing => Verdict {
            line: "No run has ever been recorded: run-once has never completed successfully on this machine (or the state directory was cleared). It is impossible to tell whether the timer is working."
                .to_string(),
            healthy: false,
        },
        RunStateRead::Unreadable(why) => Verdict {
            line: format!(
                "A run record exists but is unreadable ({why}): there is no way to tell whether the last run succeeded."
            ),
            healthy: false,
        },
        RunStateRead::Present(state) => {
            let age = now_unix.saturating_sub(state.finished_at_unix);
            let ago = human_age(age);
            // Overdue is checked before outcome on purpose: a timer that
            // stopped firing leaves a *successful* last run behind, so
            // looking only at the outcome would call it healthy forever.
            let overdue = age > stale_after_secs;
            if overdue {
                return Verdict {
                    line: format!(
                        "No run for {ago} (threshold {}): the timer may have stopped; the last result was {}.",
                        human_age(stale_after_secs),
                        outcome_word(state.outcome)
                    ),
                    healthy: false,
                };
            }
            if state.outcome.is_failure() {
                let step = state.failed_step.as_deref().unwrap_or("no step recorded");
                return Verdict {
                    line: format!(
                        "Last run failed: the {step} step errored {ago} ago, with no successful run since."
                    ),
                    healthy: false,
                };
            }
            Verdict {
                line: format!(
                    "Healthy: last run {ago} ago, took {} ms, archived {} shard(s), {}.",
                    state.duration_ms,
                    state.shards_written,
                    if state.snapshot_created {
                        "snapshot created"
                    } else {
                        "no change, so no snapshot created"
                    }
                ),
                healthy: true,
            }
        }
    }
}

/// Machine-readable shape of a [`RunStateRead`] for `status --json`.
///
/// The same three cases `summarize` renders as sentences are tagged here with
/// a `kind`, so a script can tell "never ran" from "record unreadable" from
/// "ran, here is when" without parsing prose. `unknown` is never serialised as
/// `finished_at_unix: null`: a missing or unreadable record is its own tagged
/// case with an explicit `why` (the same rule as `activity::TimeSource`).
pub fn run_state_json(
    read: &RunStateRead,
    now_unix: u64,
    stale_after_secs: u64,
) -> serde_json::Value {
    match read {
        RunStateRead::Missing => serde_json::json!({
            "kind": "missing",
            "why": "no run record: run-once has never completed successfully on this machine, or the state directory was cleared",
        }),
        RunStateRead::Unreadable(why) => serde_json::json!({
            "kind": "unreadable",
            "why": format!("a run record exists but is unreadable: {why}"),
        }),
        RunStateRead::Present(state) => {
            let age = now_unix.saturating_sub(state.finished_at_unix);
            let outcome = match state.outcome {
                RunOutcome::Completed => "completed",
                RunOutcome::Noop => "noop",
                RunOutcome::Error => "error",
            };
            serde_json::json!({
                "kind": "known",
                "finished_at_unix": state.finished_at_unix,
                "age_secs": age,
                "overdue": age > stale_after_secs,
                "stale_after_secs": stale_after_secs,
                "outcome": outcome,
                "duration_ms": state.duration_ms,
                "failed_step": state.failed_step,
                "shards_written": state.shards_written,
                "snapshot_created": state.snapshot_created,
                "collect_errors": state.collect_errors,
                "archive_gaps": state.archive_gaps,
            })
        }
    }
}

/// Coarse, honest duration wording — no invented precision.
fn human_age(secs: u64) -> String {
    if secs < 90 {
        format!("{secs} seconds")
    } else if secs < 5400 {
        format!("{} minutes", secs / 60)
    } else if secs < 172_800 {
        format!("{} hours", secs / 3600)
    } else {
        format!("{} days", secs / 86_400)
    }
}

fn outcome_word(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Completed => "success (snapshot created)",
        RunOutcome::Noop => "success (no change)",
        RunOutcome::Error => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_digest_is_not_the_hostname() {
        let d = machine_digest("some-laptop");
        assert_eq!(d.len(), 12);
        assert!(!d.contains("laptop"));
    }

    #[test]
    fn stale_threshold_respects_floor_and_multiplier() {
        assert_eq!(stale_after_secs(3600), 4 * 3600);
        // A 5-minute cadence still gets the 1-hour floor.
        assert_eq!(stale_after_secs(300), STALE_FLOOR_SECS);
    }
}
