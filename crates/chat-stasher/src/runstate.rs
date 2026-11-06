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
//! W1084 (ISSUE-008 ②, GO-1105 ②) adds the cost line that was missing when
//! the 2026-11-05 incident happened: every pass also records the time
//! budget it ran under, its finished duration under the heartbeat's name
//! for it, and whether it ran over — so `status` can show the two numbers
//! beside its verdict and the scheduled unit's error log can WARN on a
//! round that overran the budget by half. All of that travels in this same
//! file, read back only from here; the run-once log itself is never
//! re-opened to answer it.
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

/// W1084 (ISSUE-008 ②): how far past its budget a pass may run before the
/// scheduled unit's error log gets a WARN line, as a percentage. 50 means
/// strictly past 150% of the budget — GO-1105 ② decided the warn threshold
/// as "a pass over its budget by 50%" (the full statement, with the
/// reasoning, lives on [`over_budget_warn_line`]). A single round past
/// the budget can be a one-off (a nightly full sweep, a loaded hour); a
/// round that also burns half the interval again on top is the one
/// nobody is watching.
pub const OVER_BUDGET_WARN_PCT: u64 = 50;

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
    /// The declared `[pull.letta]` API-pull producer, pulled before the
    /// collect step. `0` also covers a pass with no producer declared:
    /// a pull that did not run and a declaration that does not exist are
    /// both "no letta pull happened".
    pub letta_pull_ms: u64,
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
    /// index read and hashed. A session the incremental index carried
    /// forward is here by having contributed zero to this counter,
    /// not by being counted — the carried count is
    /// [`Self::activity_index_sessions_carried`].
    pub shard_bytes_read_hashed: u64,
    /// Sessions the incremental activity-index build carried forward
    /// without re-reading their bodies (SYNC-D). Zero is the honest
    /// value for a build that derived everything — a pass that carried
    /// nothing is a measurement, not a failure.
    pub activity_index_sessions_carried: u64,
    /// SQLite sessions whose per-session rows were queried.
    pub sqlite_sessions_queried: u64,
    /// SQLite sessions exported (re-sealed) into the stage.
    pub sqlite_sessions_exported: u64,
    /// ADR-061: snapshot-source sessions sealed as one whole-file
    /// observation line (the pre-increment shape, and the one-pass degrade
    /// of an unknown increment map).
    #[serde(default)]
    pub sqlite_full_exports: u64,
    /// ADR-061: changed snapshot-source sessions sealed as record-level
    /// increment shards.
    #[serde(default)]
    pub sqlite_increment_passes: u64,
    /// ADR-061: record lines sealed by those passes (records, explicit
    /// tombstones, session observations).
    #[serde(default)]
    pub sqlite_increment_records: u64,
    /// ADR-061: bytes of those record lines (Σw in D6's hourly rule,
    /// `written ≤ max(1.2 × growth, 64 KiB)`).
    #[serde(default)]
    pub sqlite_increment_bytes: u64,
    /// ADR-061: the measured true growth of the same passes — payload
    /// bytes of new and changed records (Δ in D6's hourly rule).
    #[serde(default)]
    pub sqlite_growth_bytes: u64,
    /// ADR-062 (LM-2): the post-push mirror copy round, when a mirror is
    /// configured and enabled. Zero is the honest value for a pass that did
    /// not reach the step or runs with the mirror off; a round that stopped
    /// early (budget, disk floor, remote readability) still measures its
    /// wall time, because a stopped mirror round is a fact, not a failure.
    #[serde(default)]
    pub mirror_sync_ms: u64,
    /// Collector state saves plus this run-state write.
    pub state_saves: u64,
    /// W1083 (S4): SQLite sessions the increment gate skipped without a
    /// sweep. Queried minus skipped minus exported is the sessions the pass
    /// swept and sealed nothing new for.
    #[serde(default)]
    pub sqlite_sessions_skipped: u64,
    /// W1083 (S4): opencode sessions swept inside the freshness grace
    /// window despite an equal cursor (ISSUE-008 ①).
    #[serde(default)]
    pub sqlite_sessions_grace_swept: u64,
    /// W1083 (S4): opencode sessions swept because this pass was the
    /// nightly full round (ISSUE-008 B).
    #[serde(default)]
    pub sqlite_sessions_full_swept: u64,
    /// W1083 (S4): whether this pass ran as the nightly full sweep.
    #[serde(default)]
    pub opencode_full_sweep: bool,
    /// W1083 (S3): shard-body bytes re-read and hashed while proving
    /// stage coverage and stage shard facts at hash-cache miss.
    #[serde(default)]
    pub stage_covers_bytes_read: u64,
    /// W1083 (C): hash-cache lookups answered without reading.
    #[serde(default)]
    pub hash_cache_hits: u64,
    /// W1083 (C): hash-cache lookups that had to read the bytes.
    #[serde(default)]
    pub hash_cache_misses: u64,
}

impl PassMetrics {
    /// The phase that spent the most wall time, as `(summary-line key,
    /// milliseconds)` plus — when that phase is collect — the harness
    /// inside it that spent the most, because "collect" alone is the sum
    /// across harnesses and cannot say which source was the slow one.
    ///
    /// `None` when no phase recorded any time: zero means "did not run",
    /// and a phase that did not run is not the largest anything. The
    /// harness names are the registry's public ids, the same labels the
    /// summary line and `status` print, never a path.
    pub fn largest_phase(&self) -> Option<(String, u64, Option<(String, u64)>)> {
        // `mirror_sync` takes part only when it ran (non-zero), the same
        // rule its summary-line rendering follows; every other phase is
        // plain "0 = did not run" too, which the `ms > 0` guard holds for
        // all of them.
        let mut largest: Option<(&'static str, u64)> = None;
        for (name, ms) in [
            ("letta_pull", self.letta_pull_ms),
            ("scan", self.scan_ms),
            ("collect", self.collect_ms),
            ("stage_audit", self.stage_audit_ms),
            ("metadata_hash", self.metadata_hash_ms),
            ("activity_index", self.activity_index_ms),
            ("push_preflight", self.push_preflight_ms),
            ("backup", self.backup_ms),
            ("run_state_write", self.run_state_write_ms),
            ("mirror_sync", self.mirror_sync_ms),
        ] {
            if ms > 0 && largest.map_or(true, |(_, largest_ms)| ms > largest_ms) {
                largest = Some((name, ms));
            }
        }
        let (phase, phase_ms) = largest?;
        let harness = (phase == "collect")
            .then(|| {
                self.collect_harness_ms
                    .iter()
                    .max_by_key(|(_, ms)| **ms)
                    .map(|(harness, ms)| (harness.clone(), *ms))
            })
            .flatten();
        Some((phase.to_string(), phase_ms, harness))
    }

    /// One line of `key=value` pairs, for the run-once summary line.
    /// Counts and durations only — no names, paths or content.
    ///
    /// `mirror_sync` rides on the same line — but only when the pass reached
    /// the mirror step (ADR-062 / LM-2: the field is 0 for a pass that did
    /// not run the step, and a zero standing in for "did not run" is the
    /// shape this module never prints). A pass with the mirror unconfigured
    /// or off therefore changes no output at all.
    pub fn summary_line(&self) -> String {
        let harness_ms = self
            .collect_harness_ms
            .iter()
            .map(|(harness, ms)| format!("{harness}={ms}"))
            .collect::<Vec<_>>()
            .join(",");
        let mirror_sync = if self.mirror_sync_ms > 0 {
            format!(" mirror_sync={}", self.mirror_sync_ms)
        } else {
            String::new()
        };
        format!(
            "[run-once] phases ms: letta_pull={} scan={} collect={} collect_harness=[{}] \
             stage_audit={} metadata_hash={} activity_index={} push_preflight={} \
             backup={} run_state_write={}{}; counts: records_scanned={} \
             files_statted={} source_bytes_read={} shard_bytes_read_hashed={} \
             sqlite_sessions_queried={} sqlite_sessions_exported={} \
             state_saves={} activity_index_sessions_carried={}; \
             gate: skipped={} grace_swept={} full_swept={} full_sweep={} \
             stage_covers_bytes_read={} hash_cache_hits={} hash_cache_misses={}; \
             sg3: full_exports={} incr_passes={} incr_records={} incr_bytes={} growth_bytes={}",
            self.letta_pull_ms,
            self.scan_ms,
            self.collect_ms,
            harness_ms,
            self.stage_audit_ms,
            self.metadata_hash_ms,
            self.activity_index_ms,
            self.push_preflight_ms,
            self.backup_ms,
            self.run_state_write_ms,
            mirror_sync,
            self.records_scanned,
            self.files_statted,
            self.source_bytes_read,
            self.shard_bytes_read_hashed,
            self.sqlite_sessions_queried,
            self.sqlite_sessions_exported,
            self.state_saves,
            self.activity_index_sessions_carried,
            self.sqlite_sessions_skipped,
            self.sqlite_sessions_grace_swept,
            self.sqlite_sessions_full_swept,
            self.opencode_full_sweep,
            self.stage_covers_bytes_read,
            self.hash_cache_hits,
            self.hash_cache_misses,
            self.sqlite_full_exports,
            self.sqlite_increment_passes,
            self.sqlite_increment_records,
            self.sqlite_increment_bytes,
            self.sqlite_growth_bytes,
        )
    }
}

/// Backfill progress the declared `[pull.letta]` producer reported for the
/// last pass: conversations published and journaled so far, and the
/// estimated total this round's listing named. Present while a large
/// account is still completing across rounds; absent when the producer is
/// caught up, undeclared, or the pass never reached it. Counts only, the
/// same boundary every other field of this file holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullLettaBackfill {
    /// Conversations whose first publication the producer completed.
    pub pulled: usize,
    /// Conversations the listing said exist, journaled or still owed.
    pub estimated_total: usize,
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
    /// The declared `[pull.letta]` producer did not finish reading this
    /// pass — rate-limited, refusing, partly proven, contending, or still
    /// backfilling. Recorded **beside** `failed_step`, not inside it: a
    /// pass whose collect or push failed while the producer also went
    /// unread keeps both facts, so the later local failure cannot erase the
    /// producer's (the two lines `status` renders say both).
    #[serde(default)]
    pub pull_letta_unread: bool,
    /// Another pass of this machine held the producer's state lock, so this
    /// pass deferred instead of reading the producer itself. Not a failure
    /// — the pass that won the lock reports the producer's own result the
    /// same round, and every pass's push moves the shared stage — but never
    /// silent either: `status` renders it as the producer's own line beside
    /// the green archive sentence. Mutually exclusive with
    /// [`Self::pull_letta_unread`] in practice: a pass that deferred never
    /// learned the producer's own state, and an unread pass can only be the
    /// one that held the lock.
    #[serde(default)]
    pub pull_letta_deferred: bool,
    /// The producer's backfill progress, when it reported one. Absent when
    /// caught up, undeclared, or never reached — never a zero standing in
    /// for "did not run".
    #[serde(default)]
    pub pull_letta_backfill: Option<PullLettaBackfill>,
    /// Shards this pass sealed into the stage.
    pub shards_written: usize,
    /// Sealed shards present in the stage when the pass decided to push.
    pub stage_shards: usize,
    pub snapshot_created: bool,
    pub collect_errors: usize,
    pub archive_gaps: usize,
    /// Codebuff sessions the collect half read but could not archive, because
    /// the transcript snapshot did not validate as a JSON array (ADR-054
    /// D6). A pass with skips here is *incomplete* — it did not finish
    /// reading — which `run-once` reports instead of a success. Counts
    /// only, like the fields beside it, and additively default so older
    /// state files still parse.
    #[serde(default)]
    pub torn_skips: usize,
    /// sha256 prefix of the machine partition name — enough to notice that
    /// two machines share a state dir, without recording the hostname.
    pub machine_digest: String,
    /// Per-phase timers and counters. Additive: absent in files written
    /// before it existed, so it defaults rather than failing the read.
    #[serde(default)]
    pub phases: PassMetrics,
    /// W1084 (ISSUE-008 ②): the time budget this pass ran under, in
    /// milliseconds — the default is the scheduling cadence, unless
    /// `run_budget_secs` states a different line ([`run_budget_ms`] says
    /// which and why). `None` when no honest budget number existed at pass
    /// time: a record written before W1084, a pass whose config could not
    /// be read, or a machine whose cadence itself is unusable. `status`
    /// renders that as unknown, never as a number it invented.
    #[serde(default)]
    pub budget_ms: Option<u64>,
    /// W1084 (ISSUE-008 ②): the finished pass's wall time under the name
    /// the deputy heartbeat reads. `duration_ms` carries the same value
    /// for every older reader; the two are written in one place only,
    /// [`RunState::record_duration`], so they cannot drift apart.
    /// `None` on records written before W1084.
    #[serde(default)]
    pub last_duration_ms: Option<u64>,
    /// W1084 (ISSUE-008 ②): whether the finished pass ran longer than the
    /// budget in [`Self::budget_ms`]. `None` whenever the budget is `None`
    /// — a heartbeat must not read "unknown" as "not over": the honest
    /// report for a budgetless round is that over-budget cannot be told,
    /// and it reports that instead of counting the round as within.
    #[serde(default)]
    pub over_budget: Option<bool>,
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
            pull_letta_unread: false,
            pull_letta_deferred: false,
            pull_letta_backfill: None,
            shards_written: 0,
            stage_shards: 0,
            snapshot_created: matches!(outcome, RunOutcome::Completed),
            collect_errors: 0,
            archive_gaps: 0,
            torn_skips: 0,
            machine_digest: machine_digest(machine),
            phases: PassMetrics::default(),
            budget_ms: None,
            last_duration_ms: None,
            over_budget: None,
        }
    }

    /// Stamp the finished pass's wall time on the record: the legacy
    /// `duration_ms`, the heartbeat's `last_duration_ms` twin, and the
    /// over-budget judgment, all in this one method so no pass can write
    /// them apart (W1084, ISSUE-008 ②). The judgment is `None` whenever
    /// the record holds no usable budget — never a false "not over".
    pub fn record_duration(&mut self, duration_ms: u64) {
        self.duration_ms = duration_ms;
        self.last_duration_ms = Some(duration_ms);
        self.over_budget = self
            .budget_ms
            // A zero budget would divide the percentage below and judge
            // every round over; nothing produces it (an explicit zero
            // refuses the pass), but a hand-crafted record must not
            // upgrade it into a judgment either.
            .filter(|budget_ms| *budget_ms > 0)
            .map(|budget_ms| duration_ms > budget_ms);
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

/// The time budget one `run-once` pass runs against, in milliseconds
/// (W1084, ISSUE-008 ②).
///
/// The default is the scheduling cadence itself: a pass is expected to
/// fit inside the interval it is scheduled at, because the 2026-11-05
/// incident was exactly rounds that ran 44–98 minutes against an hourly
/// cadence — the next round started as the last one finished, and no
/// check anywhere said how long a round "should" take. `run_budget_secs`
/// overrides the cadence as the budget when a machine wants a line other
/// than its cadence (a 600 s budget on a 3600 s cadence keeps the noise
/// line tighter than the schedule).
///
/// The two ways this returns `[Ok(None)]` are both honest "cannot tell":
///
/// * an explicit `run_budget_secs = 0` is refused with an error before
///   that, naming the key — the alternative would be a budget every
///   pass is over, WARN spam every interval;
/// * with no override, a cadence the scheduler already refuses
///   (`backup_interval_secs = 0`) leaves the budget unknown: the pass
///   still archives, `status` still reports the invalid cadence as its
///   own verdict, and neither is turned into a fabricated budget.
pub fn run_budget_ms(config: &crate::config::Config) -> anyhow::Result<Option<u64>> {
    if let Some(explicit) = config.run_budget_secs {
        if explicit == 0 {
            anyhow::bail!("run_budget_secs must be greater than zero");
        }
        return Ok(Some(explicit.saturating_mul(1000)));
    }
    match crate::schedule::interval_secs(config) {
        Ok(interval_secs) => Ok(Some(interval_secs.saturating_mul(1000))),
        Err(_) => Ok(None),
    }
}

/// The WARN line a pass over its budget leaves in the scheduled unit's
/// error log (W1084, ISSUE-008 ②, GO-1105 ②).
///
/// The threshold is *strictly past* 150% of the budget — a pass landing
/// exactly on the line is at the line, not over it — chosen by GO-1105 ②
/// because a single over-budget round can be a one-off while a round that
/// burns the whole interval and half of another is the shape that was
/// silently accepted every hour during the incident.
///
/// Every fact the line names is stated so the log entry stands on its own:
/// the round's own wall time, the budget it ran against, the over-run as a
/// percentage of that budget, and the phase that spent the most wall time
/// (naming the harness inside collect when collect is the slow phase, and
/// saying so in words when no timer recorded anything). Durations and
/// percentages only — the line cannot name a session, a path or a title,
/// the same boundary the rest of this record holds.
pub fn over_budget_warn_line(
    duration_ms: u64,
    budget_ms: u64,
    phases: &PassMetrics,
) -> Option<String> {
    if budget_ms == 0 {
        return None;
    }
    if duration_ms.saturating_mul(100) <= budget_ms.saturating_mul(100 + OVER_BUDGET_WARN_PCT) {
        return None;
    }
    let attribution = match phases.largest_phase() {
        Some((phase, phase_ms, Some((harness, harness_ms)))) => format!(
            "; largest phase {phase} at {phase_ms} ms, of which the {harness} \
             harness alone spent {harness_ms} ms"
        ),
        Some((phase, phase_ms, None)) => {
            format!("; largest phase {phase} at {phase_ms} ms")
        }
        // A round long enough to trip the warn with every phase timer at
        // zero is a record that did not measure where its time went —
        // named in words rather than papered over with a plain zero.
        None => "; the phase timers record no time in any phase".to_string(),
    };
    Some(format!(
        "[run-once] WARN: this pass overran its time budget: took {duration_ms} ms \
         against a budget of {budget_ms} ms ({}% of the budget, past the warn \
         line at {}%){attribution}",
        percent_of_budget_text(duration_ms, budget_ms),
        100 + OVER_BUDGET_WARN_PCT,
    ))
}

/// `duration_ms` as a percentage of `budget_ms`, with one decimal, because
/// the round bytes vary: the incident's own numbers were 163.9%, and a
/// whole-number rounding would print the boundary round as "150% of the
/// budget, past the warn line at 150%" — a sentence that contradicts
/// itself. `budget_ms` must be non-zero; every caller checks first.
fn percent_of_budget_text(duration_ms: u64, budget_ms: u64) -> String {
    let per_mille = duration_ms.saturating_mul(1000) / budget_ms;
    format!("{}.{}", per_mille / 10, per_mille % 10)
}

/// The declared Letta producer's own line in `status`'s answer, when the
/// last pass leaves the producer worth a separate sentence. This is the
/// second line of the two-line split: the archive cycle's health is
/// [`Verdict::line`], the producer's state is here, and neither pretends
/// to be the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LettaLine {
    /// The producer's sentence, already worded.
    pub sentence: String,
    /// The command or check that addresses it, when one exists.
    pub fix: Option<String>,
}

/// One human sentence plus the exit decision behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The timer health conclusion — the archive cycle's own state. When
    /// only the declared Letta producer went unread, this line stays the
    /// *healthy archive* sentence: the whole pass is not worded unhealthy
    /// (the producer's failure is the second line, `letta`), but it does
    /// not get to call the run healthy-overall either (`healthy` says no).
    pub line: String,
    /// false → `status` must exit non-zero.
    pub healthy: bool,
    /// The declared Letta producer's separate line, when the last pass
    /// left one to say: "did not finish reading" with its fix, or backfill
    /// progress with the next-round promise. `None` when no producer was
    /// declared or it completed.
    pub letta: Option<LettaLine>,
    /// W1084 (ISSUE-008 ②): the budget line, the last round's two numbers
    /// — its duration and the budget it ran against — plus the
    /// over-budget judgment, answered from the record alone (the run-once
    /// log is never opened for it). `None` when there is no record at
    /// all: a missing or unreadable record has no duration to compare,
    /// and its verdict line already says what it is.
    pub budget: Option<String>,
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
            letta: None,
            budget: None,
        },
        RunStateRead::Unreadable(why) => Verdict {
            line: format!(
                "A run record exists but is unreadable ({why}): there is no way to tell whether the last run succeeded."
            ),
            healthy: false,
            letta: None,
            budget: None,
        },
        RunStateRead::Present(state) => {
            let age = now_unix.saturating_sub(state.finished_at_unix);
            let ago = human_age(age);
            // W1084 (ISSUE-008 ②): the budget line is rendered beside
            // every verdict a record can support — the timer's health and
            // the round's cost are different questions, and an overdue or
            // failed round is exactly the one whose numbers a reader most
            // wants named. It never moves `healthy`: a slow round is a
            // cost line, not a timer verdict.
            let budget = Some(budget_line_of(state));
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
                    letta: letta_line_of(state),
                    budget,
                };
            }
            if state.outcome.is_failure() {
                // A pass that failed only at its local steps keeps naming the
                // local step here, with the producer's own line rendered
                // beside it when the pass recorded one — both facts stay
                // in the record, so both stay in the answer.
                let producer_only = state.pull_letta_unread
                    && matches!(state.failed_step.as_deref(), None | Some("pull-letta"));
                let step = state.failed_step.as_deref().unwrap_or("no step recorded");
                return Verdict {
                    line: if producer_only {
                        // The local cycle completed — the record proved its
                        // snapshot and shards — so the first line reports the
                        // archive cycle as healthy while the producer's line
                        // carries the failure. The pass still was not a
                        // success, and `healthy: false` keeps the exit code
                        // from saying otherwise.
                        format!(
                            "Archive cycle healthy: last run {ago} ago, took {} ms, archived {} shard(s), {}.",
                            state.duration_ms,
                            state.shards_written,
                            if state.snapshot_created {
                                "snapshot created"
                            } else {
                                "no change, so no snapshot created"
                            }
                        )
                    } else {
                        format!(
                            "Last run failed: the {step} step errored {ago} ago, with no successful run since."
                        )
                    },
                    healthy: false,
                    letta: letta_line_of(state),
                    budget,
                };
            }
            // The healthy first line can still carry a producer line: a
            // deferred producer is the one fact that turns up beside a
            // green archive cycle — the hand-off the deferral sentence
            // names, never a failure wording.
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
                letta: letta_line_of(state),
                budget,
            }
        }
    }
}

/// W1084 (ISSUE-008 ②): the budget sentence the record can answer with —
/// the two numbers `status` shows beside its verdict and the over-budget
/// judgment on the same line. Rendered from the record alone, so a record
/// without a usable budget says "unknown", never a number it invented; and
/// the over wording names the warn line so a reader can tell "over but
/// under 150%" from "over, and the error log has the WARN".
fn budget_line_of(state: &RunState) -> String {
    let Some(budget_ms) = state.budget_ms.filter(|budget_ms| *budget_ms > 0) else {
        return format!(
            "Last round took {} ms; that pass recorded no time budget, so whether it \
             ran over one is unknown (not \"within budget\").",
            state.duration_ms
        );
    };
    if state.duration_ms > budget_ms {
        format!(
            "Last round took {} ms against a budget of {budget_ms} ms — over budget \
             ({}% of the budget; the pass's error log carries a WARN from {}).",
            state.duration_ms,
            percent_of_budget_text(state.duration_ms, budget_ms),
            100 + OVER_BUDGET_WARN_PCT,
        )
    } else {
        format!(
            "Last round took {} ms against a budget of {budget_ms} ms — within budget.",
            state.duration_ms
        )
    }
}

/// The declared producer's second line, from the record's facts. `None`
/// when the pass declared no producer or the producer completed. A
/// deferral is rendered as the hand-off it is — never a failure
/// wording — and an unread producer as the failure it is; the healthy
/// first line carries either beside it unchanged.
fn letta_line_of(state: &RunState) -> Option<LettaLine> {
    if !state.pull_letta_unread {
        // A deferral is the producer's own line too — visible beside the
        // green archive sentence, but worded as the hand-off it is, never
        // as a failure: the pass that won the lock reported the pull that
        // hour.
        if state.pull_letta_deferred {
            return Some(LettaLine {
                sentence: "Letta pull deferred: another pass of this machine held \
                           the producer's state lock, so this pass did not read the \
                           producer itself. Not a failure — the pass that held the \
                           lock reports the producer's result, and every pass's push \
                           moves the shared stage — but the record names it rather \
                           than staying silent."
                    .to_string(),
                fix: None,
            });
        }
        return None;
    }
    if let Some(backfill) = &state.pull_letta_backfill {
        // The counted unit of `pulled` is the conversation, so a record at
        // `pulled == estimated_total` is the sandwich state a round that
        // published every conversation but could not reach the agent-level
        // material leaves: wording it "N of ~N conversations still
        // backfilling" would imply an unarchived conversation where every
        // one is archived — the remainder is named for what it is.
        let progress = if backfill.pulled >= backfill.estimated_total {
            format!(
                "every conversation is archived and the agent-level material \
                  (window, listing, metadata and capture order) is what remains"
            )
        } else {
            format!(
                "{} of ~{} conversations archived so far",
                backfill.pulled, backfill.estimated_total
            )
        };
        return Some(LettaLine {
            sentence: format!(
                "Letta pull did not finish reading: the account is still backfilling — \
                  {progress}; the next pass continues from where this one stopped, and \
                  the pass stays at exit 3 (\"did not finish reading\") until the \
                  backfill completes."
            ),
            fix: None,
        });
    }
    Some(LettaLine {
        sentence:
            "Letta pull did not finish reading this pass; its unread remainder stays pending \
             in the producer state, never \"no messages\", and the pass stays at exit 3 \
             (\"did not finish reading\")."
                .to_string(),
        fix: Some(
            "check that the timer can resolve pull.letta.api_key (a file:, env-file: or \
             keychain: reference) and that the inbox and state paths are reachable; the \
             step's own error is in the run-once error log"
                .to_string(),
        ),
    })
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
                "last_duration_ms": state.last_duration_ms,
                "budget_ms": state.budget_ms,
                "over_budget": state.over_budget,
                "failed_step": state.failed_step,
                "pull_letta_unread": state.pull_letta_unread,
                "pull_letta_deferred": state.pull_letta_deferred,
                "pull_letta_backfill": state.pull_letta_backfill.as_ref().map(|backfill| {
                    serde_json::json!({
                        "pulled": backfill.pulled,
                        "estimated_total": backfill.estimated_total,
                    })
                }),
                "shards_written": state.shards_written,
                "snapshot_created": state.snapshot_created,
                "collect_errors": state.collect_errors,
                "archive_gaps": state.archive_gaps,
                "torn_skips": state.torn_skips,
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

    /// W1084 (ISSUE-008 ②): a record written before the budget fields
    /// existed still loads, with the budget unknown rather than
    /// fabricated — the heartbeat reads `over_budget: null`, which is
    /// "cannot tell", and `status` words it that way, never "within".
    #[test]
    fn a_pre_budget_record_loads_with_the_budget_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            run_state_path(&state_dir),
            r#"{ "version": 1, "finished_at_unix": 100, "duration_ms": 5901000,
                 "outcome": "completed", "failed_step": null, "shards_written": 0,
                 "stage_shards": 0, "snapshot_created": false,
                 "collect_errors": 0, "archive_gaps": 0,
                 "machine_digest": "aaaaaaaaaaaa" }"#,
        )
        .unwrap();
        match load(&state_dir) {
            RunStateRead::Present(state) => {
                assert_eq!(state.budget_ms, None, "no budget in an old record");
                assert_eq!(state.last_duration_ms, None);
                assert_eq!(
                    state.over_budget, None,
                    "unknown is never a false \"not over\""
                );
                let verdict = summarize(
                    &RunStateRead::Present(state),
                    100 + 60,
                    stale_after_secs(3600),
                );
                let budget = verdict
                    .budget
                    .as_deref()
                    .expect("the record renders its (unknown) budget line");
                assert!(
                    budget.contains("recorded no time budget") && budget.contains("unknown"),
                    "the status line says unknown, in words: {budget}"
                );
            }
            other => panic!("pre-W1084 record must load, got {other:?}"),
        }
    }

    /// W1084 (ISSUE-008 ②, GO-1105 ②): the WARN is strictly past 150% of
    /// the budget — a round that lands exactly on the line is at the line,
    /// not over it — and the line names every fact the incident kept
    /// silent: the round's own time, the budget, the percentage and the
    /// phase that spent the seconds.
    #[test]
    fn the_warn_needs_strictly_more_than_half_over_and_names_every_fact() {
        let mut phases = PassMetrics::default();
        phases.collect_ms = 3_100;
        phases.backup_ms = 600;
        // At exactly +50% there is no warn: the threshold is strictly past.
        assert!(over_budget_warn_line(1_500, 1_000, &phases).is_none());
        // A hair past it warns (150.1%, the one decimal that keeps the
        // from-150% sentence honest at the boundary).
        let warn = over_budget_warn_line(1_501, 1_000, &phases)
            .expect("a round 50.1% over its budget warns");
        assert!(
            warn.contains("[run-once] WARN: this pass overran its time budget:"),
            "stable grep prefix first: {warn}"
        );
        assert!(
            warn.contains("took 1501 ms") && warn.contains("against a budget of 1000 ms"),
            "the round's own time and the budget, both named: {warn}"
        );
        assert!(
            warn.contains("(150.1% of the budget, past the warn line at 150%)"),
            "the over-run as a percentage of the budget, and the line it crossed: {warn}"
        );
        assert!(
            warn.contains("largest phase collect at 3100 ms"),
            "the largest phase by its summary-line name: {warn}"
        );
        // Within the budget: no line, no matter the shape of the phase record.
        assert!(over_budget_warn_line(999, 1_000, &phases).is_none());
        // A zero budget never warns: nothing produces one (an explicit zero
        // refuses the pass), and a hand-crafted record must not upgrade it.
        assert!(over_budget_warn_line(5_901_000, 0, &phases).is_none());
    }

    /// W1084 (ISSUE-008 ②): when collect is the slow phase, the WARN
    /// attributes the time to the harness inside it — "collect" is a sum,
    /// and the incident's own 31 minutes were one harness's scan — and a
    /// record with no phase timers at all names that in words instead of
    /// attributing the time to a zero.
    #[test]
    fn the_warn_names_the_harness_inside_a_slow_collect() {
        let mut phases = PassMetrics::default();
        phases.collect_ms = 3_100;
        phases.collect_harness_ms.insert("chatgpt".to_string(), 300);
        phases
            .collect_harness_ms
            .insert("opencode".to_string(), 2_700);
        phases.backup_ms = 600;
        let warn = over_budget_warn_line(4_200, 1_000, &phases).unwrap();
        assert!(
            warn.contains(
                "largest phase collect at 3100 ms, of which the \
                           opencode harness alone spent 2700 ms"
            ),
            "attribution continues into the harness that was slow: {warn}"
        );

        // No timer recorded anything: the warn says so rather than
        // attributing the over-run to a zero phase.
        let bare = over_budget_warn_line(4_200, 1_000, &PassMetrics::default()).unwrap();
        assert!(
            bare.contains("the phase timers record no time in any phase"),
            "an unmeasured over-run is named as unmeasured: {bare}"
        );
    }

    /// W1084 (ISSUE-008 ②): `record_duration` is the one place the
    /// duration twins and the over-budget judgment are written, so they
    /// stay in lockstep — and a record with no usable budget gets `None`
    /// for the judgment, never a false "not over".
    #[test]
    fn record_duration_writes_the_twins_and_the_judgment_in_one_place() {
        let mut state = RunState::new(RunOutcome::Completed, None, "machine", 0);
        state.budget_ms = Some(2_000);
        state.record_duration(1_999);
        assert_eq!(state.duration_ms, 1_999, "the legacy field keeps its value");
        assert_eq!(
            state.last_duration_ms,
            Some(1_999),
            "the twin is the same number"
        );
        assert_eq!(
            state.over_budget,
            Some(false),
            "one millisecond under is not over"
        );
        state.record_duration(2_000);
        assert_eq!(
            state.over_budget,
            Some(false),
            "over is strictly past the budget, the same strictness the warn \
             threshold uses: at the line is not past it"
        );
        state.record_duration(2_001);
        assert_eq!(
            state.over_budget,
            Some(true),
            "one millisecond past is an explicit over — it ate into the next \
             round's interval"
        );
        // No budget: the judgment is unknown, not "not over".
        state.budget_ms = None;
        state.record_duration(5_901);
        assert_eq!(state.over_budget, None, "unknown is never a verdict");
        // A zero budget — nothing produces one, but a hand-crafted record
        // must not turn it into "over a zero budget".
        state.budget_ms = Some(0);
        state.record_duration(1);
        assert_eq!(state.over_budget, None);

        // The trio survives the real atomic write/read round trip.
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        state.budget_ms = Some(2_000);
        state.record_duration(5_901_000);
        save(&state_dir, &state).unwrap();
        match load(&state_dir) {
            RunStateRead::Present(read) => {
                assert_eq!(read.budget_ms, Some(2_000));
                assert_eq!(read.last_duration_ms, Some(5_901_000));
                assert_eq!(read.over_budget, Some(true));
            }
            other => panic!("the W1084 trio must round-trip, got {other:?}"),
        }
    }

    /// W1084 (ISSUE-008 ②): the budget line beside the `status` verdict —
    /// both numbers plus the judgment, from the record alone — and the
    /// wording never lets an over-round read as healthy budgeting or an
    /// unknown read as "within".
    #[test]
    fn summarize_renders_the_budget_line_with_both_numbers_and_the_judgment() {
        let mut over = RunState::new(RunOutcome::Completed, None, "machine", 5_901_000);
        over.finished_at_unix = 1_760_000_000;
        over.budget_ms = Some(3_600_000);
        over.record_duration(5_901_000);
        let verdict = summarize(
            &RunStateRead::Present(over),
            1_760_000_060,
            stale_after_secs(3600),
        );
        let budget = verdict.budget.as_deref().unwrap();
        assert!(
            budget.contains("Last round took 5901000 ms against a budget of 3600000 ms"),
            "both numbers, plain: {budget}"
        );
        assert!(
            budget.contains(
                "over budget (163.9% of the budget; the pass's error log \
                             carries a WARN from 150%)"
            ),
            "the over judgment, the percentage, and where the WARN lives: {budget}"
        );

        let mut within = RunState::new(RunOutcome::Noop, None, "machine", 412_000);
        within.finished_at_unix = 1_760_000_000;
        within.budget_ms = Some(3_600_000);
        within.record_duration(412_000);
        let verdict = summarize(
            &RunStateRead::Present(within),
            1_760_000_060,
            stale_after_secs(3600),
        );
        assert!(
            verdict
                .budget
                .as_deref()
                .unwrap()
                .ends_with("within budget."),
            "a fitting round says so: {:?}",
            verdict.budget
        );
        assert!(verdict.healthy, "the budget line never moves the verdict");
    }

    /// W1084 (ISSUE-008 ②): the three heartbeat fields reach
    /// `status --json`'s `run_state` object, and an unknown stays
    /// `null` — which for this trio is its own honest shape: the record
    /// is present, the budget is not, and neither is invented.
    #[test]
    fn status_json_carries_the_budget_trio() {
        let mut state = RunState::new(RunOutcome::Noop, None, "machine", 5);
        state.budget_ms = Some(2_000);
        state.record_duration(100);
        let json = run_state_json(&RunStateRead::Present(state), 1_000, 4 * 3600);
        assert_eq!(json["last_duration_ms"], serde_json::json!(100));
        assert_eq!(json["budget_ms"], serde_json::json!(2_000));
        assert_eq!(json["over_budget"], serde_json::json!(false));

        let unknown = RunState::new(RunOutcome::Noop, None, "machine", 5);
        let json = run_state_json(&RunStateRead::Present(unknown), 1_000, 4 * 3600);
        assert_eq!(json["last_duration_ms"], serde_json::Value::Null);
        assert_eq!(json["budget_ms"], serde_json::Value::Null);
        assert_eq!(json["over_budget"], serde_json::Value::Null);
    }

    /// W1084 (ISSUE-008 ②): the default budget is the cadence and an
    /// explicit `run_budget_secs` overrides only the budget — a tighter
    /// line does not re-write the schedule — while a zero override is an
    /// error naming its key and an unusable cadence leaves no budget
    /// rather than inventing one.
    #[test]
    fn run_budget_ms_defaults_to_the_cadence_and_refuses_a_zero_override() {
        let mut config = crate::config::Config::default();
        config.backup_interval_secs = Some(600);
        assert_eq!(run_budget_ms(&config).unwrap(), Some(600_000));
        config.run_budget_secs = Some(90);
        assert_eq!(
            run_budget_ms(&config).unwrap(),
            Some(90_000),
            "an explicit budget overrides the cadence as the line, not as the schedule"
        );
        config.run_budget_secs = Some(0);
        let refusal = run_budget_ms(&config).unwrap_err().to_string();
        assert!(
            refusal.contains("run_budget_secs must be greater than zero"),
            "the refusal names the key at fault: {refusal}"
        );
        // A cadence the scheduler already refuses leaves the budget
        // unknown: the pass still archives, and this stays "cannot tell".
        config.run_budget_secs = None;
        config.backup_interval_secs = Some(0);
        assert_eq!(run_budget_ms(&config).unwrap(), None);
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
        state.phases.letta_pull_ms = 19;
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
            "letta_pull=0",
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
        assert!(
            !line.contains("mirror_sync="),
            "a pass that did not reach the mirror step prints no mirror phase, \
             not a zero standing in for \"did not run\": {line}"
        );

        // ADR-062 (LM-2): a pass that did reach the mirror step carries it
        // on the same measured line as every other phase.
        metrics.mirror_sync_ms = 17;
        let line = metrics.summary_line();
        assert!(line.contains("mirror_sync=17"), "{line}");
        assert!(
            line.contains("run_state_write=9 mirror_sync=17;"),
            "mirror_sync belongs inside the phases list, same half as the \
             other durations: {line}"
        );
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
