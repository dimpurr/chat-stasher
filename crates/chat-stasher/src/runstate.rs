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
            line: "还没有任何运行记录：本机从未成功跑完一次 run-once（也可能状态目录被清空）。无法判断定时器是否在工作。"
                .to_string(),
            healthy: false,
        },
        RunStateRead::Unreadable(why) => Verdict {
            line: format!("运行记录存在但读不出来（{why}）：无法判断上次运行是否正常。"),
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
                        "已经{ago}没有运行了（阈值 {}）：定时器可能已经停了，上次结果是{}。",
                        human_age(stale_after_secs),
                        outcome_word(state.outcome)
                    ),
                    healthy: false,
                };
            }
            if state.outcome.is_failure() {
                let step = state.failed_step.as_deref().unwrap_or("未记录步骤");
                return Verdict {
                    line: format!("上次运行失败：{ago}前在 {step} 步骤出错，此后没有成功的运行。"),
                    healthy: false,
                };
            }
            Verdict {
                line: format!(
                    "正常：上次运行在 {ago}前，耗时 {} ms，入库 {} 个分片，{}。",
                    state.duration_ms,
                    state.shards_written,
                    if state.snapshot_created {
                        "已创建快照"
                    } else {
                        "无变化故未创建快照"
                    }
                ),
                healthy: true,
            }
        }
    }
}

/// Coarse, honest duration wording — no invented precision.
fn human_age(secs: u64) -> String {
    if secs < 90 {
        format!("{secs} 秒")
    } else if secs < 5400 {
        format!("{} 分钟", secs / 60)
    } else if secs < 172_800 {
        format!("{} 小时", secs / 3600)
    } else {
        format!("{} 天", secs / 86_400)
    }
}

fn outcome_word(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Completed => "成功（已创建快照）",
        RunOutcome::Noop => "成功（无变化）",
        RunOutcome::Error => "失败",
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
