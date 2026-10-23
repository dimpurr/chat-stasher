//! Durable record of what the weekly `reclaim-stage` did (W1012 / SG-1 C1④+ C2).
//!
//! Before this file, a blocked reclaim left exactly one trace: lines in
//! `reclaim-stage.log`, a file nobody reads until the disk is full (46 §2's
//! finding). This module writes one small JSON record after every
//! completed reclaim attempt — success, blocked, or refused — so `status`,
//! `doctor` and the menu bar can say what the weekly job actually did
//! without opening a log, and C2's "two consecutive blocked runs produce a
//! system notification" has a number to count from.
//!
//! The record is a single file, overwritten per attempt, never appended:
//! `reclaim-stage.log` gets truncated at the 5 MiB cap by its own timer
//! preamble, so anything that must survive belongs in a state file, not in
//! the log (SG-0's E6).
//!
//! Privacy: counts, byte totals, unix times, destination names, a
//! classifier slug and the health words (ADR-060 D7's snapshot). No
//! paths, no session ids, no error text bodies.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::Path;

/// File name inside the state dir (`collect::default_state_dir()`).
pub const RECLAIM_STATE_FILE: &str = "reclaim-state.json";

/// Bumped whenever the shape below changes incompatibly. An unknown version
/// is treated as unreadable, never as a healthy record.
///
/// **v2 (ADR-060 D7, SG-2)** adds the run's per-destination health snapshot
/// ([`ReclaimAttempt::destination_health`]). A v1 record reads as
/// unreadable — unknown version, so the W1012g policy applies forward: the
/// streak alert says the record is unreadable rather than trusting a shape
/// this build cannot interpret for deletion-adjacent facts, and the next
/// completed reclaim rewrites it.
pub const RECLAIM_STATE_VERSION: u32 = 2;

/// When a weekly reclaim job is considered overdue: the fixed weekly slot
/// plus one full week of grace, so a single holiday-week holiday (an asleep
/// machine, a paused scheduler) is not an alarm — but the silent-failure
/// shape SG-0's E2 measured (the job failing to start, exit 128, no log
/// lines at all) cannot pass unnoticed for two consecutive weeks.
///
/// `reclaim-stage` itself is weekly by construction (`schedule.rs`'s
/// Sunday 03:17 slot), so this threshold is a constant rather than a
/// config knob; a machine that schedules it differently is a machine that
/// installed a different timer file, and the staleness wording names the
/// record's age rather than guessing the cadence.
pub const RECLAIM_OVERDUE_SECS: u64 = 8 * 86_400;

/// How this attempt ended. The three states a caller must keep apart:
/// `Ok` proves the destinations (a dry "ok" proved them too — nothing was
/// deleted, but nothing was blocked either), `Blocked` is the ADR-020
/// refusal (nothing deleted, held back recorded), `Refused` means the run
/// ended before candidates were even consulted (an unreadable stage
/// manifest, a crashed phase) — its hold-back is unknown, not zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReclaimOutcome {
    Ok,
    Blocked,
    Refused,
}

impl ReclaimOutcome {
    pub fn slug(self) -> &'static str {
        match self {
            ReclaimOutcome::Ok => "ok",
            ReclaimOutcome::Blocked => "blocked",
            ReclaimOutcome::Refused => "refused",
        }
    }
}

/// Why one destination blocked the reclaim, in the machine-readable shape
/// `cmd_reclaim_stage` already prints — `kind` is the block's own word, and
/// `error_kind` carries the ADR-023 classifier's verdict for the
/// unreachable/partial-read cases when the failure produced one, so the
/// reason a destination could not be consulted stays distinguishable from
/// "this is a credentials problem" without shipping error text bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimBlock {
    pub destination: String,
    /// The block kind's word: `unreachable` / `partial-read` / `not-held` /
    /// `triple-mismatch`.
    pub kind: String,
    /// The ADR-023 classifier slug when the block carries an error it
    /// classified, otherwise absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    pub sessions: usize,
    pub bytes: u64,
}

/// One destination's health as the run classified it — ADR-060 D7's
/// snapshot of "what the run actually saw", so the surfaces can report a
/// destination the run never consulted (`degraded-not-consulted`) as that
/// state and never as an ordinary quiet row. `health` is D2's slug
/// vocabulary verbatim (`healthy` / `lagging` / `never-pushed` / `unknown`)
/// and `role` is D7's proof-role word (`must-prove` /
/// `degraded-not-consulted`); both are recorded as strings the way
/// [`ReclaimBlock::kind`] is, so the record is a witness, not a reader, of
/// the vocabulary.
///
/// Privacy: a destination name, two machine words. No paths, no session
/// ids, no error text bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimDestinationHealth {
    pub destination: String,
    /// `healthy` / `lagging` / `never-pushed` / `unknown` (ADR-060 D2).
    pub health: String,
    /// `must-prove` / `degraded-not-consulted` (ADR-060 D7).
    pub role: String,
}

/// One completed reclaim attempt, as durable metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReclaimAttempt {
    pub version: u32,
    /// Wall-clock seconds since the epoch at the moment the attempt ended.
    pub finished_at_unix: u64,
    /// Whether this was `--apply` or a dry run.
    pub apply: bool,
    pub outcome: ReclaimOutcome,
    /// Blocked destinations in the order the report listed them. Empty for
    /// an `Ok` attempt, and for a `Refused` attempt whose reasons were not
    /// per-destination — and, since ADR-060, empty for the **empty-proof-
    /// set** refusal too (a `Blocked` attempt where every declared
    /// destination was degraded: the ≥1 guard deleted nothing, and the
    /// snapshot below is what names the destinations).
    pub blocked: Vec<ReclaimBlock>,
    /// The run's per-destination health snapshot (ADR-060 D7): one row per
    /// declared destination, in the order the report listed them. Absent
    /// when the run recorded no classification — an unknown, never
    /// "there were no destinations".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_health: Option<Vec<ReclaimDestinationHealth>>,
    /// Consecutive non-`Ok` attempts ending with this one. Written by
    /// `record` from the previous record; the counter survives crashes
    /// because it lives in the same overwrite the outcome does.
    pub consecutive_blocked: u32,
    /// Sessions / body bytes the report counted as reclaim candidates.
    ///
    /// Absent (not zero) for a `Refused` attempt: the run ended before the
    /// stage was surveyed, so zero would be a measurement it never made.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_bytes: Option<u64>,
}

/// What a read of the state file can tell us.
#[derive(Debug, Clone)]
pub enum ReclaimStateRead {
    Missing,
    Unreadable(String),
    Present(ReclaimAttempt),
}

pub fn reclaim_state_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join(RECLAIM_STATE_FILE)
}

pub fn load(state_dir: &Path) -> ReclaimStateRead {
    let path = reclaim_state_path(state_dir);
    match fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ReclaimStateRead::Missing,
        Err(e) => ReclaimStateRead::Unreadable(e.kind().to_string()),
        Ok(bytes) => match serde_json::from_slice::<ReclaimAttempt>(&bytes) {
            Ok(state) if state.version == RECLAIM_STATE_VERSION => ReclaimStateRead::Present(state),
            Ok(state) => ReclaimStateRead::Unreadable(format!("unknown version {}", state.version)),
            Err(e) => ReclaimStateRead::Unreadable(format!("malformed json: {e}")),
        },
    }
}

/// Persist one attempt, computing `consecutive_blocked` from the record it
/// replaces. The file is one attempt deep on purpose — deeper history
/// belongs to a log, and the only streak this file exists to count is "how
/// many in a row did the weekly job fail to reclaim", which the overwrite
/// already answers.
pub fn record(state_dir: &Path, attempt: &ReclaimAttempt) -> anyhow::Result<()> {
    let mut attempt = attempt.clone();
    let previous = match load(state_dir) {
        ReclaimStateRead::Present(prev) if prev.version == RECLAIM_STATE_VERSION => {
            Some(prev.consecutive_blocked)
        }
        // An unreadable or absent predecessor both reset the streak to
        // this attempt alone — an unknown prefix is not a smaller number
        // (which would understate the streak) and not a larger one.
        _ => None,
    };
    attempt.consecutive_blocked = match attempt.outcome {
        ReclaimOutcome::Ok => 0,
        _ => previous.map_or(1, |count| count.saturating_add(1)),
    };
    save(state_dir, &attempt)
}

/// Persist crash-safely: temp file + rename, same shape as `runstate::save`.
fn save(state_dir: &Path, attempt: &ReclaimAttempt) -> anyhow::Result<()> {
    fs::create_dir_all(state_dir)
        .with_context(|| format!("create state dir {}", state_dir.display()))?;
    let path = reclaim_state_path(state_dir);
    let tmp = path.with_file_name(format!(".{RECLAIM_STATE_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(attempt).context("serialise reclaim state")?;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(destination: &str, sessions: usize, bytes: u64) -> ReclaimAttempt {
        ReclaimAttempt {
            version: RECLAIM_STATE_VERSION,
            finished_at_unix: 100,
            apply: true,
            outcome: ReclaimOutcome::Blocked,
            blocked: vec![ReclaimBlock {
                destination: destination.to_string(),
                kind: "unreachable".to_string(),
                error_kind: Some("credentials_missing_or_invalid".to_string()),
                sessions,
                bytes,
            }],
            destination_health: None,
            consecutive_blocked: 0,
            candidates: Some(sessions),
            candidate_bytes: Some(bytes),
        }
    }

    fn ok(candidates: usize, bytes: u64) -> ReclaimAttempt {
        ReclaimAttempt {
            version: RECLAIM_STATE_VERSION,
            finished_at_unix: 200,
            apply: true,
            outcome: ReclaimOutcome::Ok,
            blocked: Vec::new(),
            destination_health: None,
            consecutive_blocked: 0,
            candidates: Some(candidates),
            candidate_bytes: Some(bytes),
        }
    }

    /// C2's number: the streak counts consecutive non-ok attempts and resets
    /// on the first ok one. Written by `record`, read by the menubar.
    #[test]
    fn consecutive_blocked_counts_and_resets() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        record(&state_dir, &blocked("r2", 10, 1000)).unwrap();
        record(&state_dir, &blocked("r2", 20, 2000)).unwrap();
        match load(&state_dir) {
            ReclaimStateRead::Present(a) => {
                assert_eq!(a.consecutive_blocked, 2, "two blocked in a row");
                assert_eq!(a.blocked[0].destination, "r2");
            }
            other => panic!("expected a present record, got {other:?}"),
        }
        record(&state_dir, &blocked("r2", 30, 3000)).unwrap();
        match load(&state_dir) {
            ReclaimStateRead::Present(a) => assert_eq!(a.consecutive_blocked, 3),
            other => panic!("expected a present record, got {other:?}"),
        }
        record(&state_dir, &ok(40, 4000)).unwrap();
        match load(&state_dir) {
            ReclaimStateRead::Present(a) => assert_eq!(a.consecutive_blocked, 0, "ok resets"),
            other => panic!("expected a present record, got {other:?}"),
        }
    }

    /// A refused attempt (the run never consulted the destinations) counts
    /// toward the streak too: the stage was not reclaimed either way, and
    /// the person whose disk is filling has no reason to care which of the
    /// two failure shapes it was.
    #[test]
    fn refused_counts_toward_the_streak() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        record(
            &state_dir,
            &ReclaimAttempt {
                version: RECLAIM_STATE_VERSION,
                finished_at_unix: 50,
                apply: true,
                outcome: ReclaimOutcome::Refused,
                blocked: Vec::new(),
                destination_health: None,
                consecutive_blocked: 0,
                // A refused run never surveyed the stage: the counts stay
                // absent rather than pretending zero was measured.
                candidates: None,
                candidate_bytes: None,
            },
        )
        .unwrap();
        record(&state_dir, &blocked("r2", 5, 500)).unwrap();
        match load(&state_dir) {
            ReclaimStateRead::Present(a) => assert_eq!(a.consecutive_blocked, 2),
            other => panic!("expected a present record, got {other:?}"),
        }
    }

    /// An unreadable predecessor is an unknown prefix, and the streak is
    /// this attempt alone — not zero measured, not a guess at the future.
    #[test]
    fn an_unreadable_predecessor_resets_the_streak_to_one() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(reclaim_state_path(&state_dir), "corrupted").unwrap();
        record(&state_dir, &blocked("r2", 1, 10)).unwrap();
        match load(&state_dir) {
            ReclaimStateRead::Present(a) => assert_eq!(a.consecutive_blocked, 1),
            other => panic!("expected a present record, got {other:?}"),
        }
    }

    /// Missing / unreadable / present stay three states at the reader too,
    /// and the unknown-version case lands in unreadable rather than being
    /// treated as a fresh install — including the v1 shape this v2 build
    /// can no longer interpret, which must not quietly read as a record
    /// (ADR-060 D7's forward application of the W1012g policy).
    #[test]
    fn missing_unreadable_and_present_stay_three_states() {
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = dir.path().join("state");
        assert!(matches!(load(&state_dir), ReclaimStateRead::Missing));
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            reclaim_state_path(&state_dir),
            r#"{ "version": 99, "finished_at_unix": 1, "apply": true,
                "outcome": "ok", "blocked": [], "consecutive_blocked": 0,
                "candidates": 0, "candidate_bytes": 0 }"#,
        )
        .unwrap();
        match load(&state_dir) {
            ReclaimStateRead::Unreadable(why) => assert!(why.contains("unknown version"), "{why}"),
            other => panic!("expected Unreadable, got {other:?}"),
        }
        // The v1 shape (pre-SG-2, no health snapshot) is an unknown
        // version to this build exactly the same way: unreadable, never a
        // present record the surfaces could read a healthy set out of.
        fs::write(
            reclaim_state_path(&state_dir),
            r#"{ "version": 1, "finished_at_unix": 1, "apply": true,
                "outcome": "blocked", "blocked": [], "consecutive_blocked": 2,
                "candidates": 5, "candidate_bytes": 500 }"#,
        )
        .unwrap();
        match load(&state_dir) {
            ReclaimStateRead::Unreadable(why) => {
                assert!(why.contains("unknown version 1"), "{why}");
            }
            other => panic!("a v1 record is unreadable to a v2 build: {other:?}"),
        }
    }

    /// ADR-060 D7's health snapshot round-trips: the run's own words
    /// (`healthy` / `lagging` / `never-pushed` / `unknown`, and the roles
    /// `must-prove` / `degraded-not-consulted`) are recorded verbatim and
    /// read back without reinterpretation — the record is a witness of the
    /// vocabulary it was handed, so a future rename shows up as a diff in
    /// the surfaces instead of being silently translated here.
    #[test]
    fn the_destination_health_snapshot_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let state_dir = dir.path().join("state");
        let mut attempt = blocked("r2", 2, 200);
        attempt.blocked.clear();
        attempt.outcome = ReclaimOutcome::Blocked;
        attempt.destination_health = Some(vec![
            ReclaimDestinationHealth {
                destination: "storagebox".to_string(),
                health: "healthy".to_string(),
                role: "must-prove".to_string(),
            },
            ReclaimDestinationHealth {
                destination: "r2".to_string(),
                health: "never-pushed".to_string(),
                role: "degraded-not-consulted".to_string(),
            },
        ]);
        record(&state_dir, &attempt).unwrap();
        match load(&state_dir) {
            ReclaimStateRead::Present(a) => {
                let snapshot = a.destination_health.expect("snapshot recorded");
                assert_eq!(snapshot.len(), 2);
                assert_eq!(snapshot[0].destination, "storagebox");
                assert_eq!(snapshot[0].role, "must-prove");
                assert_eq!(snapshot[1].health, "never-pushed");
                assert_eq!(snapshot[1].role, "degraded-not-consulted");
                assert!(
                    a.blocked.is_empty(),
                    "the empty-proof-set refusal carries its destinations in the snapshot"
                );
                assert_eq!(
                    a.consecutive_blocked, 1,
                    "it still counts toward the streak"
                );
            }
            other => panic!("expected a present record, got {other:?}"),
        }
        // And the absent case stays absent — a run that recorded no
        // classification reads back as no classification, not as an
        // empty-and-therefore-meaningless zero-row list.
        record(&state_dir, &ok(1, 10)).unwrap();
        match load(&state_dir) {
            ReclaimStateRead::Present(a) => assert!(a.destination_health.is_none()),
            other => panic!("expected a present record, got {other:?}"),
        }
    }
}
