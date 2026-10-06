//! Timer-visibility state: what happened on the last `run-once` pass.
//!
//! The scheduler (launchd/systemd, see [`crate::schedule`]) runs `run-once`
//! with nobody watching stdout. Without a durable record the user cannot tell
//! a healthy timer from one that died months ago — the common failure of a
//! broken timer is not an error message, it is silence.
//!
//! So every `run-once` pass, success *and* failure, writes one small file:
//! `<state_dir>/run-state.json`. `chat-stasher status` reads it back and says
//! one sentence in plain language. The write itself is silent; the only
//! stdout this feature adds is the one W880 phase summary line.
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

/// Per-phase wall time and privacy-safe counters for one `run-once`
/// pass, recorded so a slow pass can be attributed to the phase that
/// was slow — the scheduled command runs with nobody watching stdout,
/// and the only durable record of where its seconds went is this file.
///
/// Durations are milliseconds, integers like [`RunState::duration_ms`].
/// Counters are counts and byte totals only: no session id, no source
/// or stage path, no harness display name, no conversation content —
/// the same boundary [`RunState`] already holds. The per-harness
/// collect timings are keyed by harness *id* (the registry's public
/// short label, the same value `status` prints), never by a path.
///
/// Additive: a `run-state.json` written before this struct existed
/// deserializes with every field at its [`Default`] (the field is
/// `#[serde(default)]` on [`RunState`]), and an older reader ignores
/// the unknown field, so no version bump is needed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassMetrics {
    // Wall time per phase, in milliseconds. A phase the pass never
    // reached (push preflight and backup on a no-op pass) stays 0:
    // zero is a measurement of "did not run", never a fallback.
    /// Registry scan (`scanner::scan_with_machine`).
    pub scan_ms: u64,
    /// Incremental staging of every scanned record
    /// (`collect::collect_scan_report`).
    pub collect_ms: u64,
    /// Collect wall time per harness, keyed by harness id.
    pub collect_harness_ms: std::collections::BTreeMap<String, u64>,
    /// Stage audit: counting the sealed shards in the stage.
    pub stage_audit_ms: u64,
    /// Metadata hash: hashing the machine's metadata to decide a push.
    pub metadata_hash_ms: u64,
    /// Activity index rebuild before a push.
    pub activity_index_ms: u64,
    /// Push preflight: the empty-stage safety scan before a backup.
    pub push_preflight_ms: u64,
    /// Backup: the rustic traversal and snapshot write.
    pub backup_ms: u64,
    /// Writing this run-state file itself.
    pub run_state_write_ms: u64,
    // Counters for the same pass.
    /// Session records the scanner handed to the collector.
    pub records_scanned: u64,
    /// Files whose metadata the pass took (stat calls on source and
    /// store files; a file read whole without a stat is counted in
    /// `source_bytes_read`, not here).
    pub files_statted: u64,
    /// Source bytes read during collection (committed prefixes, deltas
    /// and whole-file reads alike).
    pub source_bytes_read: u64,
    /// Shard-body bytes read and hashed: the committed JSONL prefixes
    /// re-hashed during collection plus the shard bodies the activity
    /// index read and hashed.
    pub shard_bytes_read_hashed: u64,
    /// SQLite sessions whose per-session rows were queried.
    pub sqlite_sessions_queried: u64,
    /// SQLite sessions exported (re-sealed) into the stage.
    pub sqlite_sessions_exported: u64,
    /// Collector state saves plus this run-state write.
    pub state_saves: u64,
}

impl PassMetrics {
    /// One line of `key=value` pairs, for the run-once summary line.
    /// Counts and durations only — no names, paths or content.
    pub fn summary_line(&self) -> String {
        let harness_ms = self
            .collect_harness_ms
            .iter()
            .map(|(harness, ms)| format!("{harness}={ms}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "[run-once] phases ms: scan={} collect={} collect_harness=[{}] \
             stage_audit={} metadata_hash={} activity_index={} push_preflight={} \
             backup={} run_state_write={}; counts: records_scanned={} \
             files_statted={} source_bytes_read={} shard_bytes_read_hashed={} \
             sqlite_sessions_queried={} sqlite_sessions_exported={} state_saves={}",
            self.scan_ms,
            self.collect_ms,
            harness_ms,
            self.stage_audit_ms,
            self.metadata_hash_ms,
            self.activity_index_ms,
            self.push_preflight_ms,
            self.backup_ms,
            self.run_state_write_ms,
            self.records_scanned,
            self.files_statted,
            self.source_bytes_read,
            self.shard_bytes_read_hashed,
            self.sqlite_sessions_queried,
            self.sqlite_sessions_exported,
            self.state_saves,
        )
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
    /// Per-phase timers and counters. Additive: absent in files written
    /// before it existed, so it defaults rather than failing the read.
    #[serde(default)]
    pub phases: PassMetrics,
}

impl RunState {
    /// Build a record for a pass, stamped with the current second.
    ///
    /// `run_once_pass` builds this at the *start* of the pass, as the
    /// pessimistic record it then corrects step by step, so the stamp taken
    /// here is the moment the pass began. Whoever writes the file has to call
    /// [`RunState::mark_finished`] first: a record that skips it claims a pass
    /// which took N seconds finished N seconds before it started, and every
    /// reader of `finished_at_unix` is then wrong by the whole pass.
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
            phases: PassMetrics::default(),
        }
    }

    /// Restamp `finished_at_unix` with the current second, immediately before
    /// the record is written.
    ///
    /// The field documents itself as the moment the pass *ended*, and only
    /// the caller holding the finished pass knows that moment — a record built
    /// at the start of the work cannot. So the stamp is moved here, at the one
    /// point where both the duration and the end are known: after
    /// `cmd_run_once` has measured `duration_ms`, before `save`.
    pub fn mark_finished(&mut self) {
        self.finished_at_unix = now_unix();
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
                "phases": serde_json::to_value(&state.phases)
                    // reason: PassMetrics is plain integers and string keys,
                    // so its serialization cannot fail; the fallback keeps
                    // the rest of the record readable if that ever changes.
                    .unwrap_or(serde_json::Value::Null),
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

/// One word for how a pass ended. Shared with the native host's `summary`
/// answer (`nativehost.rs`), so the same record is never described two ways.
pub fn outcome_word(outcome: RunOutcome) -> &'static str {
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

    /// W880: a `run-state.json` written before the per-phase timers
    /// existed has no `phases` field. It is still a valid record —
    /// the phases default, they are not "unreadable".
    #[test]
    fn run_state_without_phases_still_loads_with_default_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            run_state_path(&state_dir),
            r#"{ "version": 1, "finished_at_unix": 100, "duration_ms": 5,
                 "outcome": "noop", "failed_step": null, "shards_written": 0,
                 "stage_shards": 0, "snapshot_created": false,
                 "collect_errors": 0, "archive_gaps": 0,
                 "machine_digest": "aaaaaaaaaaaa" }"#,
        )
        .unwrap();
        match load(&state_dir) {
            RunStateRead::Present(state) => {
                assert_eq!(state.phases, PassMetrics::default());
                assert_eq!(state.phases.scan_ms, 0);
            }
            other => panic!("pre-W880 record must load, got {other:?}"),
        }
    }

    /// W880: the per-phase record survives the atomic write/read
    /// round trip, and the harness keys stay harness ids.
    #[test]
    fn run_state_with_phases_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let mut state = RunState::new(RunOutcome::Completed, None, "machine", 7);
        state.phases.scan_ms = 11;
        state.phases.collect_ms = 22;
        state
            .phases
            .collect_harness_ms
            .insert("chatgpt".to_string(), 3);
        state
            .phases
            .collect_harness_ms
            .insert("claude".to_string(), 4);
        state.phases.stage_audit_ms = 5;
        state.phases.metadata_hash_ms = 6;
        state.phases.activity_index_ms = 7;
        state.phases.push_preflight_ms = 8;
        state.phases.backup_ms = 9;
        state.phases.run_state_write_ms = 1;
        state.phases.records_scanned = 12;
        state.phases.files_statted = 13;
        state.phases.source_bytes_read = 14;
        state.phases.shard_bytes_read_hashed = 15;
        state.phases.sqlite_sessions_queried = 16;
        state.phases.sqlite_sessions_exported = 17;
        state.phases.state_saves = 18;
        save(&state_dir, &state).unwrap();
        match load(&state_dir) {
            RunStateRead::Present(read) => {
                assert_eq!(read.phases, state.phases);
            }
            other => panic!("expected a present state, got {other:?}"),
        }
    }

    /// W880: the summary line carries every phase and counter, and
    /// nothing that could name a session, a path or a title.
    #[test]
    fn summary_line_carries_every_phase_and_counter() {
        let mut metrics = PassMetrics::default();
        metrics.scan_ms = 1;
        metrics.collect_ms = 2;
        metrics.collect_harness_ms.insert("chatgpt".to_string(), 3);
        metrics.stage_audit_ms = 4;
        metrics.metadata_hash_ms = 5;
        metrics.activity_index_ms = 6;
        metrics.push_preflight_ms = 7;
        metrics.backup_ms = 8;
        metrics.run_state_write_ms = 9;
        metrics.records_scanned = 10;
        metrics.files_statted = 11;
        metrics.source_bytes_read = 12;
        metrics.shard_bytes_read_hashed = 13;
        metrics.sqlite_sessions_queried = 14;
        metrics.sqlite_sessions_exported = 15;
        metrics.state_saves = 16;
        let line = metrics.summary_line();
        for key in [
            "scan=1",
            "collect=2",
            "chatgpt=3",
            "stage_audit=4",
            "metadata_hash=5",
            "activity_index=6",
            "push_preflight=7",
            "backup=8",
            "run_state_write=9",
            "records_scanned=10",
            "files_statted=11",
            "source_bytes_read=12",
            "shard_bytes_read_hashed=13",
            "sqlite_sessions_queried=14",
            "sqlite_sessions_exported=15",
            "state_saves=16",
        ] {
            assert!(line.contains(key), "summary line must carry {key}: {line}");
        }
    }

    /// W880: `status --json` exposes the per-phase record under
    /// `phases`, alongside the fields it already exposed.
    #[test]
    fn status_json_exposes_the_phases() {
        let mut state = RunState::new(RunOutcome::Noop, None, "machine", 5);
        state.phases.scan_ms = 42;
        state.phases.records_scanned = 7;
        let json = run_state_json(&RunStateRead::Present(state), 1_000, 4 * 3600);
        assert_eq!(json["kind"], "known");
        assert_eq!(json["phases"]["scan_ms"], 42);
        assert_eq!(json["phases"]["records_scanned"], 7);
        assert!(json["phases"]["collect_harness_ms"].as_object().is_some());
    }

    /// The record is built at the start of a pass and written at its end, so
    /// the field that promises "the moment the pass ended" can only become
    /// true when the caller restamps it. 1000 stands in for a stamp taken
    /// long before the write.
    #[test]
    fn mark_finished_replaces_the_stamp_taken_when_the_pass_started() {
        let mut state = RunState::new(RunOutcome::Noop, None, "some-laptop", 42);
        state.finished_at_unix = 1_000;
        state.mark_finished();
        assert!(
            state.finished_at_unix > 1_000,
            "mark_finished must re-read the clock, got {}",
            state.finished_at_unix
        );
        assert!(state.finished_at_unix <= now_unix(), "never in the future");
    }
}
