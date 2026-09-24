//! search — **remote repository, local query** (prototype, metadata tier only).
//!
//! Two capabilities must never be confused:
//!
//! 1. *server-side search* — the remote executes the query, which requires the
//!    remote to hold decryption capability. rustic/restic has no mechanism to
//!    push a decrypted content query down to S3 / SFTP / WebDAV / rest, and it
//!    would break "the key never leaves this machine". **Not implemented, and
//!    not implementable on this protocol.**
//! 2. *remote repository, local query* — the key stays local, the repository is
//!    read on demand over the configured backend. **This is what this module
//!    does.**
//!
//! Cost tiers, which the product must keep apart:
//!
//! * **metadata tier** (this module): snapshot files + index + tree blobs, plus
//!   the per-machine activity sidecar (`meta/<machine>/activity-v1.jsonl`).
//!   It answers "which session, on which machine, when it was active, how big,
//!   in which snapshot". **No session shard is ever fetched or decrypted** —
//!   the entry points that return *session payload* are `dump` on a
//!   `sessions/…jsonl` node, `get_blob_cached`, `cat_blob` and `read_file_at`,
//!   and none of those appears in this file.
//!
//!   One honest qualification, because the naive version of that claim is no
//!   longer true: reading the activity index *does* go through `dump`, because
//!   in a rustic repository every file's bytes are a data blob — there is no
//!   side channel for small files. The index is metadata **by declaration**
//!   (it lives under `meta/`, is written by `activity-index`, and carries one
//!   timestamp per session), not conversation content. So the promise this
//!   module keeps is narrower and exact: *a shard of conversation never gets
//!   read*, which [`SearchReport::data_blobs_read`] counts and the tests prove
//!   by removing every data pack. Index reads are counted separately, in
//!   [`SearchReport::index_files_read`], so neither number hides behind the
//!   other.
//! * **payload tier** (NOT implemented): full-text matching inside the archived
//!   conversations. Every candidate session's data blobs would have to be
//!   fetched and decrypted locally. [`FulltextCost`] measures what that would
//!   cost, from tree metadata only, so the price is on the table before anyone
//!   commits to the feature.
//!
//! Product rule this module hard-codes: **a search is always scoped to exactly
//! one destination.** There is no implicit merge across destinations.
//!
//! Time rule this module hard-codes (ADR-027): the window filters each
//! session's **conversation interval** `[first_unix, last_unix]`, read from the
//! activity sidecar index, and matches when that interval *intersects* the
//! window. It does not filter on the rustic snapshot time — that instant is
//! shared by every session in a machine's snapshot, so it selects whole
//! machines and answers a question about the backup run rather than about the
//! conversations. The comparison itself lives in [`crate::selector`], which
//! `export` will reuse unchanged.
//!
//! Honesty rule this module hard-codes (same rule `push` applies to an empty
//! stage): **"this destination does not contain it" and "I could not finish
//! reading this destination" are different answers**, and so is "this session
//! cannot be placed in time". Callers must branch on [`SearchReport::complete`]
//! and [`SearchReport::answer_complete`] before saying "not found";
//! [`SearchReport::no_hit_line`] renders the cases as different terminal lines.
//!
//! Privacy line (same as `readback`): only ids, counts, byte lengths and
//! timestamps leave this module. No conversation byte is read, so none can leak.

use anyhow::Context;
use chrono::{Duration as ChronoDuration, NaiveDate};
use rustic_core::repofile::{MasterKey, NodeType};
use rustic_core::{Credentials, LsOptions, Repository};
use std::collections::{BTreeMap, BTreeSet};

use crate::activity::{ActivityRow, TimeSource as ActivityTimeSource};
use crate::readback::{bucket_shard_path, newest_snapshot_per_host};
use crate::selector::{Selector, SessionMeta, TimeWindow, UnplacedBy, Verdict};
use crate::sidecar::{activity_index_machine, infer_harness};
use crate::store::BackupStore;

/// One matched session. Every field comes from snapshot/tree metadata or the
/// activity sidecar — never from a shard.
#[derive(Debug, Clone)]
pub struct SessionHit {
    /// Machine partition (`sessions/<machine>/…`).
    pub machine: String,
    /// Session directory name (the native session id).
    pub session_id: String,
    /// Harness the session belongs to, from `sidecar::infer_harness` on the
    /// archived id. `None` when the id carries no harness prefix.
    ///
    /// The activity index records the same value — `activity-index` derives its
    /// `harness` column with the very same function — but the id is used here
    /// because it is available for sessions the index does not cover, so one
    /// rule decides every row.
    pub harness: Option<String>,
    /// Number of sealed shards belonging to this session in that snapshot.
    pub shard_count: usize,
    /// Sum of the shards' file sizes, from the tree nodes (NOT read back).
    pub bytes: u64,
    /// Full hex id of the snapshot the session was found in.
    pub snapshot_id: String,
    /// When that snapshot was taken, unix seconds. This is the **archive's own
    /// clock** — the backup run — not the conversation's, and it is not what
    /// the time filter compares. Kept because "which backup run carried this"
    /// is a real question; `first_unix` / `last_unix` answer the other one.
    pub archive_time_unix: i64,
    /// Earliest conversation time from the activity index, unix seconds.
    /// `None` is *unknown*, never zero.
    pub first_unix: Option<i64>,
    /// Latest conversation time from the activity index, unix seconds.
    pub last_unix: Option<i64>,
    /// Why the conversation time is unknown. `Some` whenever either bound is
    /// `None`, so the reason reaches the terminal instead of being reinvented
    /// there. `None` only when both bounds are known.
    pub time_why: Option<String>,
    /// Number of data blobs the shards are made of (from `node.content`).
    /// This is what a full-text pass would have to fetch — it is *counted*
    /// here, never fetched.
    pub data_blobs: usize,
    /// Non-blank lines the activity index counted for this session. Zero when
    /// the index has no row for it, which is why it is never rendered alone —
    /// see `time_source`.
    pub line_count: u64,
    /// How the activity index attested this session's conversation time.
    ///
    /// Carried rather than re-derived from the two `Option` bounds so that a
    /// consumer which needs the tri-state (the `ui` dashboard feeds the very
    /// same [`crate::overview::OverviewRow`]s `overview` renders) reads the
    /// index's own answer instead of inventing a second rule for "known".
    pub time_source: ActivityTimeSource,
}

impl SessionHit {
    /// Privacy-safe short form of the session id for terminal output.
    /// See [`crate::id::short_session_id`] — a bare 8-char prefix does not
    /// distinguish sessions, because both id shapes have a constant head.
    pub fn short_id(&self) -> String {
        crate::id::short_session_id(&self.session_id)
    }
    /// Privacy-safe short form of the snapshot id for terminal output.
    pub fn short_snapshot(&self) -> String {
        self.snapshot_id.chars().take(8).collect()
    }
}

/// A session an **active** filter could not evaluate.
///
/// This list is the reason [`SearchReport::answer_complete`] exists. Such a
/// session is neither a match nor a proven non-match: its conversation time is
/// unknown, or its harness cannot be derived, so a question the user actually
/// asked has no answer for it. Reporting it as "not matched" would be the
/// "unknown recorded as empty" failure this repo forbids; dropping it silently
/// would be worse.
#[derive(Debug, Clone)]
pub struct UnplacedSession {
    pub machine: String,
    pub session_id: String,
    pub harness: Option<String>,
    pub shard_count: usize,
    pub bytes: u64,
    /// Snapshot this session was found in, and when it was taken.
    pub snapshot_id: String,
    pub archive_time_unix: i64,
    /// Which active filter had no answer for this session.
    pub dimension: UnplacedBy,
    /// The filter that could not be evaluated, and why. User-facing.
    pub why: String,
    /// Same as [`SessionHit::line_count`].
    pub line_count: u64,
    /// Same as [`SessionHit::time_source`]. Kept here too, because a session can
    /// be unplaced for its *harness* while its conversation time is perfectly
    /// known — reading this off `why` would collapse the two.
    pub time_source: ActivityTimeSource,
}

impl UnplacedSession {
    /// Privacy-safe short form of the session id, same rule as [`SessionHit`].
    pub fn short_id(&self) -> String {
        crate::id::short_session_id(&self.session_id)
    }
}

/// One hostname whose newest snapshot was walked.
///
/// This is the archive's machine list, and it is deliberately *not* derived
/// from the sessions that were found: a machine that has pushed a snapshot but
/// holds no conversations yet must appear here with zero sessions, rather than
/// be absent (which would read as "that machine does not exist") or be shown as
/// "no sessions" without saying how we know.
#[derive(Debug, Clone)]
pub struct HostSnapshot {
    /// Snapshot `hostname` — the archive's machine key, and the partition name
    /// under `sessions/`.
    pub hostname: String,
    /// Full hex id of that snapshot.
    pub snapshot_id: String,
    /// When the snapshot was taken, unix seconds — the **archive's** clock
    /// (the backup run), not any conversation's.
    pub archive_time_unix: i64,
    /// Whether `meta/<hostname>/activity-v1.jsonl` is in that snapshot's tree.
    pub has_activity_index: bool,
    /// Whether that index was read successfully. `has == true` with
    /// `index_read_ok == false` is a third state — the file is there and could
    /// not be read — and must not be reported as "no index" or as "fine".
    pub index_read_ok: bool,
    /// Whether the readable index is syntactically valid and covers every
    /// archived session in this snapshot.
    pub index_trusted: bool,
}

/// What a payload-tier (full-text) pass over the hits would cost, derived from
/// tree metadata only.
#[derive(Debug, Clone, Default)]
pub struct FulltextCost {
    pub sessions: usize,
    pub shards: usize,
    /// Data blobs that would have to be fetched and decrypted.
    pub data_blobs: usize,
    /// Plaintext bytes that would have to be decrypted (file sizes from the
    /// tree). The bytes actually transferred are the *packed* blob lengths,
    /// which this tier deliberately does not guess at.
    pub plaintext_bytes: u64,
}

/// Result of one metadata-tier search against exactly one destination.
#[derive(Debug, Clone)]
pub struct SearchReport {
    /// Label of the destination that was searched (never auto-merged).
    pub destination: String,
    /// Snapshot files present in the repository.
    pub snapshots_in_repo: usize,
    /// Snapshots whose tree was actually walked (newest per hostname).
    pub snapshots_scanned: usize,
    /// Sessions seen at all, before the filter was applied.
    pub sessions_seen: usize,
    /// The conversation-time window the filter compared against, if one was
    /// asked for. Carried so the output can name it instead of leaving the
    /// reader to guess what "matched" was measured against.
    pub window: Option<TimeWindow>,
    /// Matched sessions.
    pub hits: Vec<SessionHit>,
    /// Sessions an active filter could not evaluate. Non-empty means the
    /// answer is partial *even if every object was readable*.
    pub unplaced: Vec<UnplacedSession>,
    /// Session-count deltas: sessions every active filter evaluated and
    /// rejected. A measurement, not a fallback.
    pub not_matched: usize,
    /// Machines that have sessions in a scanned snapshot but no activity index
    /// beside them. Named explicitly: a machine missing from the index must
    /// never look like a machine with no sessions.
    pub machines_without_index: Vec<String>,
    /// Every hostname whose newest snapshot was walked, with that snapshot's id
    /// and time and whether its activity index was present and readable. The
    /// archive's machine list — see [`HostSnapshot`].
    pub hosts: Vec<HostSnapshot>,
    /// Non-empty == the scan could not read part of the destination. When this
    /// is non-empty, "no hits" means UNKNOWN, never "not there".
    pub unreadable: Vec<String>,
    /// Session shard data blobs this search fetched. Structurally always 0 for
    /// the metadata tier; asserted by the tests so a future edit that adds a
    /// `dump` call on a shard cannot pass silently.
    pub data_blobs_read: usize,
    /// Activity-index files this search fetched. NOT zero by nature — the
    /// index is a real file in the repository — and deliberately counted apart
    /// from [`Self::data_blobs_read`] so the "no shard was read" claim stays
    /// checkable.
    pub index_files_read: usize,
}

/// Recall accounting for one machine in the active query's candidate set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MachineWindowSummary {
    pub machine: String,
    pub located: usize,
    pub time_unknown: usize,
    /// True when an activity index was readable and covered every candidate
    /// session. It describes index structure/completeness, not timestamp quality.
    pub index_trusted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MachineDaySummary {
    pub day: String,
    pub machine: String,
    pub located: usize,
    pub unknown_anywhere: usize,
    pub index_trusted: bool,
}

/// An unknown share of at least half means the query has at least as many
/// unplaced candidate sessions as located ones; that is a material recall gap.
pub const UNKNOWN_SHARE_WARN_PERCENT: usize = 50;

impl MachineWindowSummary {
    pub fn should_warn(&self) -> bool {
        let candidates = self.located + self.time_unknown;
        candidates > 0
            && self.time_unknown > 0
            && self.time_unknown.saturating_mul(100) / candidates >= UNKNOWN_SHARE_WARN_PERCENT
    }
}

impl SearchReport {
    /// Per-machine counts in the query candidate set, with index coverage kept
    /// separate from the quality of timestamps recorded in a complete index.
    pub fn machine_window_summary(&self) -> Vec<MachineWindowSummary> {
        let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for host in &self.hosts {
            counts.entry(host.hostname.clone()).or_default();
        }
        for hit in &self.hits {
            let count = counts.entry(hit.machine.clone()).or_default();
            if hit.first_unix.is_some() && hit.last_unix.is_some() {
                count.0 += 1;
            } else {
                count.1 += 1;
            }
        }
        for unknown in self
            .unplaced
            .iter()
            .filter(|u| u.dimension == UnplacedBy::Time)
        {
            counts.entry(unknown.machine.clone()).or_default().1 += 1;
        }

        counts
            .into_iter()
            .map(|(machine, (located, time_unknown))| {
                let index_trusted = self
                    .hosts
                    .iter()
                    .any(|host| host.hostname == machine && host.index_trusted)
                    && !self.machines_without_index.contains(&machine)
                    && !self.unreadable.iter().any(|part| {
                        part.contains(&format!("machine `{machine}`"))
                            || part.contains(&format!("host `{machine}`"))
                    });
                MachineWindowSummary {
                    machine: machine.clone(),
                    located,
                    time_unknown,
                    index_trusted,
                }
            })
            .collect()
    }

    pub fn machine_recall_warnings(&self) -> Vec<String> {
        self.machine_window_summary()
            .into_iter()
            .filter(MachineWindowSummary::should_warn)
            .map(|summary| format!(
                "WARN: machine `{}` has high unknown-time share ({}/{} candidates); index_trusted={}. Repair with `chat-stasher activity-index --rebuild --destination <destination> --machine <machine> --stage <workspace>`.",
                summary.machine,
                summary.time_unknown,
                summary.located + summary.time_unknown,
                summary.index_trusted,
            ))
            .collect()
    }

    /// Per-machine, per-local-day recall counts for a bounded calendar range.
    /// Unknown-time candidates are included for each requested day because
    /// their activity cannot be ruled out for any day in that range.
    pub fn machine_day_summary(&self) -> Vec<MachineDaySummary> {
        let Some(window) = self.window.as_ref().filter(|w| {
            w.how == crate::selector::WindowHow::LocalDays
                && w.since_text.is_some()
                && w.until_text.is_some()
        }) else {
            return Vec::new();
        };
        let (Some(start_text), Some(end_text)) =
            (window.since_text.as_deref(), window.until_text.as_deref())
        else {
            return Vec::new();
        };
        let (Ok(mut day), Ok(end_day)) = (
            NaiveDate::parse_from_str(start_text, "%Y-%m-%d"),
            NaiveDate::parse_from_str(end_text, "%Y-%m-%d"),
        ) else {
            return Vec::new();
        };
        let summaries: BTreeMap<String, MachineWindowSummary> = self
            .machine_window_summary()
            .into_iter()
            .map(|summary| (summary.machine.clone(), summary))
            .collect();
        let mut result = Vec::new();
        while day <= end_day {
            let is_last_day = day == end_day;
            let text = day.format("%Y-%m-%d").to_string();
            let Ok((since, until)) = crate::selector::local_day_bounds(&text) else {
                if is_last_day {
                    break;
                }
                day += ChronoDuration::days(1);
                continue;
            };
            let mut counts: BTreeMap<String, usize> = summaries
                .keys()
                .map(|machine| (machine.clone(), 0))
                .collect();
            for hit in &self.hits {
                if hit.first_unix.is_some_and(|first| first <= until)
                    && hit.last_unix.is_some_and(|last| last >= since)
                {
                    *counts.entry(hit.machine.clone()).or_default() += 1;
                }
            }
            result.extend(counts.into_iter().map(|(machine, located)| {
                MachineDaySummary {
                    day: text.clone(),
                    index_trusted: summaries
                        .get(&machine)
                        .is_some_and(|summary| summary.index_trusted),
                    machine: machine.clone(),
                    located,
                    unknown_anywhere: summaries
                        .get(&machine)
                        .map_or(0, |summary| summary.time_unknown),
                }
            }));
            if is_last_day {
                break;
            }
            day += ChronoDuration::days(1);
        }
        result
    }

    /// Whether the whole destination was read. `false` == partial knowledge.
    pub fn complete(&self) -> bool {
        self.unreadable.is_empty()
    }

    /// Whether the query was answered for **every** session seen.
    ///
    /// This is stricter than [`Self::complete`]: a destination can be read in
    /// full and still not answer the question, because some session's
    /// conversation time is unknown. When that happens, `hits.is_empty()`
    /// proves nothing — hence the separate name, so a caller cannot pick the
    /// weaker one by accident.
    pub fn answer_complete(&self) -> bool {
        self.complete() && self.unplaced.is_empty()
    }

    /// The one terminal line for "your query matched nothing". The two cases
    /// are deliberately different sentences.
    pub fn no_hit_line(&self) -> String {
        if !self.complete() {
            format!(
                "search: UNKNOWN — could not finish reading `{}` ({} of {} snapshots unreadable); 0 matched in the part I could read. This is NOT \"not there\".",
                self.destination,
                self.unreadable.len(),
                self.snapshots_scanned
            )
        } else if !self.unplaced.is_empty() {
            format!(
                "search: UNKNOWN — 0 of {} sessions matched in `{}`, but {} session(s) could not be placed (see the list below), so this is NOT \"not there\".",
                self.sessions_seen,
                self.destination,
                self.unplaced.len()
            )
        } else {
            format!(
                "search: not in this destination — 0 of {} sessions matched in `{}` ({} snapshots read, all readable)",
                self.sessions_seen, self.destination, self.snapshots_scanned
            )
        }
    }

    /// How many sessions could not be placed in time specifically. Counted
    /// from the tagged dimension, never by reading the reason text.
    pub fn session_time_unknown(&self) -> usize {
        self.unplaced
            .iter()
            .filter(|u| u.dimension == UnplacedBy::Time)
            .count()
    }

    /// Cost of running a full-text pass over the current hits.
    pub fn fulltext_cost(&self) -> FulltextCost {
        FulltextCost {
            sessions: self.hits.len(),
            shards: self.hits.iter().map(|h| h.shard_count).sum(),
            data_blobs: self.hits.iter().map(|h| h.data_blobs).sum(),
            plaintext_bytes: self.hits.iter().map(|h| h.bytes).sum(),
        }
    }
}

/// The `--json` report. Exactly one object, and the three groups are separate
/// fields: matched (`sessions`), evaluated-and-rejected (`not_matched`), and
/// could-not-be-evaluated (`could_not_be_placed` + `sessions_not_placed`).
/// A consumer that reads only `sessions` still cannot mistake the third for an
/// absence, because `answer_complete` is right there beside it.
///
/// The shape follows `overview --json` and `view`'s `/api/sessions`
/// ([`crate::json_out::TimeState`]) so a consumer reads one vocabulary across
/// commands rather than one per command. An unknown time is a tagged `unknown`
/// with its reason — never `null`, never `0`.
///
/// Lives here rather than in the CLI so the shape is testable without a
/// repository, exactly like [`crate::view::render_json`].
pub fn report_json(report: &SearchReport, cost: bool) -> String {
    let time_state = |unix: Option<i64>, why: Option<&str>| match unix {
        Some(unix) => crate::json_out::TimeState::known(unix),
        None => crate::json_out::TimeState::unknown(
            why.unwrap_or("no conversation time was recorded for this session")
                .to_string(),
        ),
    };
    let hits: Vec<serde_json::Value> = report
        .hits
        .iter()
        .map(|h| {
            serde_json::json!({
                "machine": h.machine,
                "session_short_id": h.short_id(),
                "harness": h.harness,
                "shards": h.shard_count,
                "bytes": h.bytes,
                "snapshot_short_id": h.short_snapshot(),
                "archive_time_unix": h.archive_time_unix,
                "first_unix": time_state(h.first_unix, h.time_why.as_deref()),
                "last_unix": time_state(h.last_unix, h.time_why.as_deref()),
            })
        })
        .collect();
    let unplaced: Vec<serde_json::Value> = report
        .unplaced
        .iter()
        .map(|u| {
            serde_json::json!({
                "machine": u.machine,
                "session_short_id": u.short_id(),
                "harness": u.harness,
                "shards": u.shard_count,
                "bytes": u.bytes,
                "snapshot_short_id": u.snapshot_id.chars().take(8).collect::<String>(),
                "archive_time_unix": u.archive_time_unix,
                "dimension": match u.dimension {
                    UnplacedBy::Time => "time",
                    UnplacedBy::Harness => "harness",
                },
                "why": u.why,
            })
        })
        .collect();
    let c = report.fulltext_cost();
    let v = serde_json::json!({
        "destination": report.destination,
        "tier": "metadata",
        "payload_loaded": false,
        "data_blobs_read": report.data_blobs_read,
        "index_files_read": report.index_files_read,
        "complete": report.complete(),
        "answer_complete": report.answer_complete(),
        "snapshots_scanned": report.snapshots_scanned,
        "snapshots_in_repo": report.snapshots_in_repo,
        "sessions_seen": report.sessions_seen,
        "time_window": report.window.as_ref().map(|w| serde_json::json!({
            "how": match w.how {
                crate::selector::WindowHow::LocalDays => "local_days",
                crate::selector::WindowHow::UnixSeconds => "unix_seconds",
            },
            "since_unix": w.since_unix,
            "until_unix": w.until_unix,
            "description": w.describe(),
        })),
        "unreadable_parts": report.unreadable,
        "machines_without_activity_index": report.machines_without_index,
            "machine_recall": report.machine_window_summary(),
        "machine_recall_by_day": report.machine_day_summary(),
        "matched": report.hits.len(),
        "not_matched": report.not_matched,
        "could_not_be_placed": report.unplaced.len(),
        "fulltext_cost_if_loaded": {
            "sessions": c.sessions,
            "shards": c.shards,
            "data_blobs": c.data_blobs,
            "plaintext_bytes": c.plaintext_bytes,
            "note": if cost { "not implemented, not performed" } else { "not requested" },
        },
        "sessions": hits,
        "sessions_not_placed": unplaced,
    });
    let mut s = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into());
    s.push('\n');
    s
}

/// What the activity index said about one session's conversation time.
#[derive(Debug, Clone)]
struct IndexedTime {
    first_unix: Option<i64>,
    last_unix: Option<i64>,
    /// `Some` == the time is unknown and this is why.
    why: Option<String>,
    line_count: u64,
    /// The index's own tri-state, passed through unchanged.
    source: ActivityTimeSource,
}

/// Turn one index row into the tri-state this module actually needs.
///
/// A row whose `time_source` says `Exact`/`Inferred` but which carries no
/// bounds is self-contradictory. It is kept as *unknown*, with the
/// contradiction written down, rather than being read as "starts at 0" — which
/// is why the carried `source` is corrected to `Unknown` there rather than
/// passed through.
fn indexed_time(row: &ActivityRow) -> IndexedTime {
    const NO_BOUND: &str = "the activity index row carries no conversation-time bound and records \
                            no reason for that, so the time is unknown rather than zero";
    let why = match &row.time_source {
        ActivityTimeSource::Unknown { why } => Some(why.clone()),
        _ => None,
    };
    let line_count = row.line_count;
    match (row.first_unix, row.last_unix, why) {
        (Some(first), Some(last), _) => IndexedTime {
            first_unix: Some(first),
            last_unix: Some(last),
            why: None,
            line_count,
            source: row.time_source.clone(),
        },
        (first, last, Some(why)) => IndexedTime {
            first_unix: first,
            last_unix: last,
            why: Some(why.clone()),
            line_count,
            source: ActivityTimeSource::Unknown { why },
        },
        (first, last, None) => IndexedTime {
            first_unix: first,
            last_unix: last,
            why: Some(NO_BOUND.to_string()),
            line_count,
            source: ActivityTimeSource::Unknown {
                why: NO_BOUND.to_string(),
            },
        },
    }
}

/// Metadata-tier search over one destination.
///
/// Reuses the read path `readback` already established: open fresh, group
/// snapshots by hostname, take the newest per hostname, `ls` its tree and
/// bucket shard paths with [`bucket_shard_path`]. The one difference — and it
/// is the whole point — is that this walk never dumps a **session shard**, so
/// no conversation blob is fetched or decrypted. Each machine's activity
/// sidecar is read in the same pass, because that is where the conversation
/// times live.
///
/// Errors that make the answer partial are collected into
/// [`SearchReport::unreadable`] instead of being swallowed — including an
/// activity index that exists but cannot be read, which is emphatically not
/// "this machine has no sessions". A failure to open the repository at all is
/// returned as `Err`: "I cannot read this destination" must never be rendered
/// as "empty".
pub fn search_sessions(
    store: &BackupStore,
    mk: &MasterKey,
    selector: &Selector,
) -> anyhow::Result<SearchReport> {
    let backends = store.backends()?;
    let repo = Repository::new(&store.cfg.repository_options(), &backends)?
        .open(&Credentials::Masterkey(mk.clone()))
        .context("open repository for search")?
        .to_indexed()
        .context("index repository for search")?;

    let snaps = repo
        .get_all_snapshots()
        .context("list snapshots for search")?;
    let snapshots_in_repo = snaps.len();
    let newest = newest_snapshot_per_host(snaps);

    let mut report = SearchReport {
        destination: store.cfg.repo_root.clone(),
        snapshots_in_repo,
        snapshots_scanned: newest.len(),
        sessions_seen: 0,
        window: selector.window.clone(),
        hits: Vec::new(),
        unplaced: Vec::new(),
        not_matched: 0,
        machines_without_index: Vec::new(),
        hosts: Vec::new(),
        unreadable: Vec::new(),
        data_blobs_read: 0,
        index_files_read: 0,
    };
    let mut index_machines: BTreeSet<String> = BTreeSet::new();

    for snap in newest {
        let snapshot_id = snap.id.to_hex().as_str().to_string();
        let archive_time_unix = snap.time.timestamp().as_second();
        let host = snap.hostname.clone();

        // The host is listed before anything can fail, so a machine whose tree
        // could not be walked still appears — with `index_read_ok == false` and
        // the failure recorded in `unreadable`. Dropping it here would turn
        // "we could not look" into "this machine is not in the archive".
        report.hosts.push(HostSnapshot {
            hostname: host.clone(),
            snapshot_id: snapshot_id.clone(),
            archive_time_unix,
            has_activity_index: false,
            index_read_ok: false,
            index_trusted: false,
        });
        let host_entry = report.hosts.len() - 1;

        let root = match repo.node_from_snapshot_and_path(&snap, "") {
            Ok(node) => node,
            Err(e) => {
                report.unreadable.push(format!(
                    "host `{host}`: snapshot {} tree root unreadable: {e}",
                    &snapshot_id[..8.min(snapshot_id.len())]
                ));
                continue;
            }
        };
        let entries = match repo
            .ls(&root, &LsOptions::default())
            .and_then(|iter| iter.collect::<rustic_core::RusticResult<Vec<_>>>())
        {
            Ok(entries) => entries,
            Err(e) => {
                report.unreadable.push(format!(
                    "host `{host}`: snapshot {} tree walk failed: {e}",
                    &snapshot_id[..8.min(snapshot_id.len())]
                ));
                continue;
            }
        };

        // One pass over the tree collects both halves: the shard paths that
        // say which sessions exist, and the index files that say when they
        // were active.
        let mut sessions: BTreeMap<(String, String), (usize, u64, usize)> = BTreeMap::new();
        let mut index_nodes: BTreeMap<String, _> = BTreeMap::new();
        let mut partial_indexes: BTreeSet<String> = BTreeSet::new();
        let mut machines_with_missing_rows: BTreeSet<String> = BTreeSet::new();
        for (path, node) in &entries {
            if node.node_type != NodeType::File {
                continue;
            }
            if let Some(machine) = activity_index_machine(path) {
                index_nodes.insert(machine, node);
                continue;
            }
            let Some((machine, session, _shard)) = bucket_shard_path(path) else {
                continue;
            };
            let entry = sessions.entry((machine, session)).or_insert((0, 0, 0));
            entry.0 += 1;
            entry.1 += node.meta.size;
            entry.2 += node.content.as_ref().map_or(0, Vec::len);
        }

        // ---- the activity sidecars, read before any verdict is reached -----
        let mut times: BTreeMap<(String, String), IndexedTime> = BTreeMap::new();
        let mut machines_with_index: BTreeSet<String> = BTreeSet::new();
        for (machine, node) in &index_nodes {
            let mut buf = Vec::new();
            match repo.dump(node, &mut buf) {
                Ok(()) => {
                    report.index_files_read += 1;
                    machines_with_index.insert(machine.clone());
                    index_machines.insert(machine.clone());
                }
                Err(e) => {
                    partial_indexes.insert(machine.clone());
                    report.unreadable.push(format!(
                        "host `{host}` machine `{machine}`: activity index exists but could not be read: {e} — that machine's sessions cannot be placed in time, which is NOT the same as having none"
                    ));
                    continue;
                }
            }
            let mut malformed = 0usize;
            // Empty until a line fails; the only reader is guarded by
            // `malformed > 0`, which is exactly when it has been filled.
            let mut first_error = String::new();
            for line in String::from_utf8_lossy(&buf).lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<ActivityRow>(line) {
                    Ok(row) => {
                        times.insert(
                            (row.machine.clone(), row.session_id.clone()),
                            indexed_time(&row),
                        );
                    }
                    Err(e) => {
                        malformed += 1;
                        if malformed == 1 {
                            first_error = e.to_string();
                        }
                    }
                }
            }
            if malformed > 0 {
                partial_indexes.insert(machine.clone());
                report.unreadable.push(format!(
                    "host `{host}` machine `{machine}`: {malformed} malformed activity index line(s) (first: {first_error}) — the index is partial, so some sessions may be listed as time-unknown that are not"
                ));
            }
        }

        // Machines that have sessions here but no index beside them: named, not
        // assumed empty.
        for (machine, _) in sessions.keys() {
            if !machines_with_index.contains(machine)
                && !report.machines_without_index.contains(machine)
            {
                report.machines_without_index.push(machine.clone());
            }
        }

        // The host's index state, now that both halves are known: the file's
        // presence comes from the tree walk, readability from the dump. The two
        // are separate fields because "no index", "index unreadable" and "index
        // read" are three different answers.
        report.hosts[host_entry].has_activity_index = index_nodes.contains_key(&host);
        report.hosts[host_entry].index_read_ok = machines_with_index.contains(&host);
        report.hosts[host_entry].index_trusted = index_nodes.contains_key(&host)
            && machines_with_index.contains(&host)
            && !partial_indexes.contains(&host);

        for ((machine, session_id), (shard_count, bytes, data_blobs)) in sessions {
            report.sessions_seen += 1;
            let harness = infer_harness(&session_id);
            let indexed = times.get(&(machine.clone(), session_id.clone()));
            let (first_unix, last_unix, time_why, line_count, time_source) = match indexed {
                Some(t) => (
                    t.first_unix,
                    t.last_unix,
                    t.why.clone(),
                    t.line_count,
                    t.source.clone(),
                ),
                None => {
                    if machines_with_index.contains(&machine) {
                        machines_with_missing_rows.insert(machine.clone());
                    }
                    let why = if machines_with_index.contains(&machine) {
                        format!(
                            "machine `{machine}`'s activity index in this snapshot has no row for this session"
                        )
                    } else {
                        format!(
                            "machine `{machine}` has no activity index (`meta/{machine}/activity-v1.jsonl`) in this snapshot, so its conversation time was never recorded"
                        )
                    };
                    // The line count is NOT read from the shards here (this tier
                    // does not decrypt payload), so 0 would be a claim we did not
                    // measure. `Unknown` carries that, and the why distinguishes
                    // "no index at all" from "an index with no row".
                    (
                        None,
                        None,
                        Some(why.clone()),
                        0,
                        ActivityTimeSource::Unknown { why },
                    )
                }
            };
            let meta = SessionMeta {
                machine: &machine,
                session_id: &session_id,
                harness: harness.as_deref(),
                first_unix,
                last_unix,
                time_why: time_why.as_deref(),
            };
            match selector.select(&meta) {
                Verdict::Selected => report.hits.push(SessionHit {
                    machine,
                    session_id,
                    harness,
                    shard_count,
                    bytes,
                    snapshot_id: snapshot_id.clone(),
                    archive_time_unix,
                    first_unix,
                    last_unix,
                    time_why,
                    data_blobs,
                    line_count,
                    time_source,
                }),
                Verdict::NotSelected => report.not_matched += 1,
                Verdict::Unevaluated { dimension, why } => report.unplaced.push(UnplacedSession {
                    machine,
                    session_id,
                    harness,
                    shard_count,
                    bytes,
                    snapshot_id: snapshot_id.clone(),
                    archive_time_unix,
                    dimension,
                    why,
                    line_count,
                    time_source,
                }),
            }
        }
        if machines_with_missing_rows.contains(&host) {
            report.hosts[host_entry].index_trusted = false;
        }
    }

    report.machines_without_index.sort();
    report
        .hits
        .sort_by(|a, b| (&a.machine, &a.session_id).cmp(&(&b.machine, &b.session_id)));
    report
        .unplaced
        .sort_by(|a, b| (&a.machine, &a.session_id).cmp(&(&b.machine, &b.session_id)));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(machine: &str, session: &str, archive_time: i64) -> SessionHit {
        SessionHit {
            machine: machine.into(),
            session_id: session.into(),
            harness: infer_harness(session),
            shard_count: 2,
            bytes: 100,
            snapshot_id: "abcdef0123456789".into(),
            archive_time_unix: archive_time,
            first_unix: None,
            last_unix: None,
            time_why: Some("test fixture records no conversation time".into()),
            data_blobs: 2,
            line_count: 0,
            time_source: ActivityTimeSource::Unknown {
                why: "test fixture records no conversation time".into(),
            },
        }
    }

    #[test]
    fn machine_recall_reports_located_unknown_and_index_trust() {
        let mut located = hit("machine-a", "claude-code.machine-a.located", 10);
        located.first_unix = Some(10);
        located.last_unix = Some(20);
        let mut report = SearchReport {
            destination: "fixture".into(),
            snapshots_in_repo: 1,
            snapshots_scanned: 1,
            sessions_seen: 2,
            window: None,
            hits: vec![located],
            unplaced: vec![UnplacedSession {
                machine: "machine-a".into(),
                session_id: "claude-code.machine-a.unknown".into(),
                harness: Some("claude-code".into()),
                shard_count: 1,
                bytes: 10,
                snapshot_id: "abcdef0123456789".into(),
                archive_time_unix: 10,
                dimension: UnplacedBy::Time,
                why: "no timestamp".into(),
                line_count: 1,
                time_source: ActivityTimeSource::Unknown {
                    why: "no timestamp".into(),
                },
            }],
            not_matched: 0,
            machines_without_index: Vec::new(),
            hosts: vec![HostSnapshot {
                hostname: "machine-a".into(),
                snapshot_id: "abcdef0123456789".into(),
                archive_time_unix: 10,
                has_activity_index: true,
                index_read_ok: true,
                index_trusted: true,
            }],
            unreadable: Vec::new(),
            data_blobs_read: 0,
            index_files_read: 1,
        };
        let summary = report.machine_window_summary();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].located, 1);
        assert_eq!(summary[0].time_unknown, 1);
        assert!(summary[0].index_trusted);
        assert!(summary[0].should_warn());
        report.machines_without_index.push("machine-a".into());
        assert!(!report.machine_window_summary()[0].index_trusted);
        assert!(report.machine_recall_warnings()[0].contains("--rebuild"));
    }

    #[test]
    fn machine_day_summary_separates_unknown_anywhere_from_daily_activity() {
        let (start, _) = crate::selector::local_day_bounds("2026-09-24").unwrap();
        let (_, end) = crate::selector::local_day_bounds("2026-09-25").unwrap();
        let mut located = hit("machine-a", "claude-code.machine-a.located", 10);
        located.first_unix = Some(start + 60);
        located.last_unix = Some(start + 120);
        let report = SearchReport {
            destination: "fixture".into(),
            snapshots_in_repo: 1,
            snapshots_scanned: 1,
            sessions_seen: 2,
            window: Some(TimeWindow {
                since_unix: Some(start),
                until_unix: Some(end),
                how: crate::selector::WindowHow::LocalDays,
                since_text: Some("2026-09-24".into()),
                until_text: Some("2026-09-25".into()),
            }),
            hits: vec![located],
            unplaced: vec![UnplacedSession {
                machine: "machine-a".into(),
                session_id: "claude-code.machine-a.unknown".into(),
                harness: Some("claude-code".into()),
                shard_count: 1,
                bytes: 10,
                snapshot_id: "abcdef0123456789".into(),
                archive_time_unix: 10,
                dimension: UnplacedBy::Time,
                why: "no timestamp".into(),
                line_count: 1,
                time_source: ActivityTimeSource::Unknown {
                    why: "no timestamp".into(),
                },
            }],
            not_matched: 0,
            machines_without_index: Vec::new(),
            hosts: vec![HostSnapshot {
                hostname: "machine-a".into(),
                snapshot_id: "abcdef0123456789".into(),
                archive_time_unix: 10,
                has_activity_index: true,
                index_read_ok: true,
                index_trusted: true,
            }],
            unreadable: Vec::new(),
            data_blobs_read: 0,
            index_files_read: 1,
        };
        let days = report.machine_day_summary();
        assert_eq!(days.len(), 2);
        assert_eq!((days[0].located, days[0].unknown_anywhere), (1, 1));
        assert_eq!((days[1].located, days[1].unknown_anywhere), (0, 1));
        let json: serde_json::Value = serde_json::from_str(&report_json(&report, false)).unwrap();
        assert_eq!(json["machine_recall_by_day"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn empty_report_structure_is_constructible() {
        let report = SearchReport {
            destination: "d".into(),
            snapshots_in_repo: 0,
            snapshots_scanned: 0,
            sessions_seen: 0,
            window: None,
            hits: Vec::new(),
            unplaced: Vec::new(),
            not_matched: 0,
            machines_without_index: Vec::new(),
            hosts: Vec::new(),
            unreadable: Vec::new(),
            data_blobs_read: 0,
            index_files_read: 0,
        };
        assert!(report.complete());
        assert!(report.answer_complete());
    }

    /// The honesty rule, at unit level: an incomplete scan with zero hits must
    /// never render the same sentence as a complete scan with zero hits.
    #[test]
    fn no_hit_line_separates_not_there_from_unknown() {
        let mut report = SearchReport {
            destination: "dest-under-test".into(),
            snapshots_in_repo: 3,
            snapshots_scanned: 3,
            sessions_seen: 7,
            window: None,
            hits: Vec::new(),
            unplaced: Vec::new(),
            not_matched: 7,
            machines_without_index: Vec::new(),
            hosts: Vec::new(),
            unreadable: Vec::new(),
            data_blobs_read: 0,
            index_files_read: 0,
        };
        let complete = report.no_hit_line();
        assert!(report.complete());
        assert!(complete.contains("not in this destination"));
        assert!(!complete.contains("UNKNOWN"));

        report
            .unreadable
            .push("snapshot deadbeef: unreadable".into());
        let partial = report.no_hit_line();
        assert!(!report.complete());
        assert!(partial.contains("UNKNOWN"));
        assert!(partial.contains("could not finish reading"));
        assert_ne!(complete, partial);
    }

    /// A destination that was read in full can still fail to answer the
    /// question. That case gets its own sentence — reusing the "not in this
    /// destination" line would turn "we could not tell" into "it is not here".
    #[test]
    fn unplaced_sessions_make_a_zero_hit_answer_unproven() {
        let mut report = SearchReport {
            destination: "dest-under-test".into(),
            snapshots_in_repo: 2,
            snapshots_scanned: 2,
            sessions_seen: 4,
            window: None,
            hits: Vec::new(),
            unplaced: Vec::new(),
            not_matched: 4,
            machines_without_index: vec!["m-1".into()],
            hosts: Vec::new(),
            unreadable: Vec::new(),
            data_blobs_read: 0,
            index_files_read: 0,
        };
        assert!(report.complete(), "the destination itself was read");
        assert!(report.no_hit_line().contains("not in this destination"));

        report.unplaced.push(UnplacedSession {
            machine: "m-1".into(),
            session_id: "claude-code.m-1.abc".into(),
            harness: Some("claude-code".into()),
            shard_count: 1,
            bytes: 10,
            snapshot_id: "deadbeefdeadbeef".into(),
            archive_time_unix: 1,
            dimension: UnplacedBy::Time,
            why: "no activity index".into(),
            line_count: 0,
            time_source: ActivityTimeSource::Unknown {
                why: "no activity index".into(),
            },
        });
        assert!(report.complete(), "still read in full…");
        assert!(!report.answer_complete(), "…but the question is unanswered");
        assert!(report.no_hit_line().contains("UNKNOWN"));
        assert!(!report.no_hit_line().contains("not in this destination"));
    }

    #[test]
    fn fulltext_cost_sums_the_metadata_tier_counters() {
        let report = SearchReport {
            destination: "d".into(),
            snapshots_in_repo: 1,
            snapshots_scanned: 1,
            sessions_seen: 2,
            window: None,
            hits: vec![hit("m-1", "a", 10), hit("m-1", "b", 20)],
            unplaced: Vec::new(),
            not_matched: 0,
            machines_without_index: Vec::new(),
            hosts: Vec::new(),
            unreadable: Vec::new(),
            data_blobs_read: 0,
            index_files_read: 0,
        };
        let cost = report.fulltext_cost();
        assert_eq!(cost.sessions, 2);
        assert_eq!(cost.shards, 4);
        assert_eq!(cost.data_blobs, 4);
        assert_eq!(cost.plaintext_bytes, 200);
    }

    #[test]
    fn ids_are_shortened_for_terminal_output() {
        let h = hit("m-1", "0123456789abcdef", 1);
        assert!(h.short_id().starts_with("01234567"));
        assert!(!h.short_id().contains("0123456789"), "never the full id");
        assert_eq!(h.short_snapshot(), "abcdef01");
    }

    /// B54. Extension-path session ids are `platform.sessionId`
    /// (`inbox.rs:658`), so two different DeepSeek sessions share their first
    /// nine characters. A short form that cannot tell them apart has no reason
    /// to exist: its whole job is to distinguish sessions in a report without
    /// printing the full id.
    #[test]
    fn ext_ids_differing_only_in_the_tail_get_distinct_short_ids() {
        let a = hit("m-1", "deepseek.d41f6a2b9c0e4711", 1);
        let b = hit("m-1", "deepseek.d41f6a2b9c0e4722", 1);
        assert_ne!(
            a.short_id(),
            b.short_id(),
            "two distinct ext sessions must not render as the same short id"
        );
        // …and still without leaking either full id.
        assert!(!a.short_id().contains("d41f6a2b9c0e4711"));
        assert!(!b.short_id().contains("d41f6a2b9c0e4722"));
    }

    /// The native shape must stay readable and stable: same input, same output,
    /// every time, and the leading part is still the recognisable id head.
    #[test]
    fn native_uuid_short_id_stays_readable_and_stable() {
        let h = hit("m-1", "019bf00d-97b6-7eb2-9bf8-eacbacc09765", 1);
        assert!(
            h.short_id().starts_with("019bf00d"),
            "native head must survive: {}",
            h.short_id()
        );
        assert_eq!(h.short_id(), h.short_id(), "no randomness, no clock");
        assert_eq!(
            h.short_id(),
            hit("m-2", "019bf00d-97b6-7eb2-9bf8-eacbacc09765", 999).short_id(),
            "depends on the id alone"
        );
        assert!(h.short_id().len() <= 16, "short enough for a table column");
        assert!(!h.short_id().contains("eacbacc09765"), "never the full id");
    }

    /// The unplaced list must shorten ids exactly like the hit list does — it
    /// is the same privacy boundary, and a second implementation would be a
    /// second chance to leak a full id.
    #[test]
    fn unplaced_ids_are_shortened_the_same_way_as_hits() {
        let u = UnplacedSession {
            machine: "m-1".into(),
            session_id: "019bf00d-97b6-7eb2-9bf8-eacbacc09765".into(),
            harness: Some("claude-code".into()),
            shard_count: 1,
            bytes: 1,
            snapshot_id: "abcdef0123456789".into(),
            archive_time_unix: 1,
            dimension: UnplacedBy::Time,
            why: "no index".into(),
            line_count: 0,
            time_source: ActivityTimeSource::Unknown {
                why: "no index".into(),
            },
        };
        assert_eq!(u.short_id(), hit("m-1", &u.session_id, 1).short_id());
        assert!(!u.short_id().contains("eacbacc09765"));
    }

    /// The three groups must be three fields. A consumer that reads `sessions`
    /// and stops must not be able to conclude "not there" from a report whose
    /// `sessions_not_placed` is not empty, so the count and the list have to be
    /// present and separate even when `sessions` is empty.
    #[test]
    fn report_json_keeps_the_three_groups_apart() {
        let report = SearchReport {
            destination: "dest".into(),
            snapshots_in_repo: 2,
            snapshots_scanned: 2,
            sessions_seen: 3,
            window: Some(crate::selector::TimeWindow {
                since_unix: Some(100),
                until_unix: Some(200),
                how: crate::selector::WindowHow::UnixSeconds,
                since_text: Some("100".into()),
                until_text: Some("200".into()),
            }),
            hits: vec![SessionHit {
                machine: "m-1".into(),
                session_id: "claude-code.m-1.aaaaaaaa-0000-0000-0000-000000000001".into(),
                harness: Some("claude-code".into()),
                shard_count: 2,
                bytes: 10,
                snapshot_id: "abcdef0123456789".into(),
                archive_time_unix: 5,
                first_unix: Some(150),
                last_unix: Some(150),
                time_why: None,
                data_blobs: 2,
                line_count: 3,
                time_source: ActivityTimeSource::Exact,
            }],
            unplaced: vec![UnplacedSession {
                machine: "m-2".into(),
                session_id: "codex.m-2.bbbbbbbb-0000-0000-0000-000000000002".into(),
                harness: Some("codex".into()),
                shard_count: 1,
                bytes: 5,
                snapshot_id: "deadbeefdeadbeef".into(),
                archive_time_unix: 6,
                dimension: UnplacedBy::Time,
                why: "no activity index".into(),
                line_count: 0,
                time_source: ActivityTimeSource::Unknown {
                    why: "no activity index".into(),
                },
            }],
            not_matched: 1,
            machines_without_index: vec!["m-2".into()],
            hosts: Vec::new(),
            unreadable: Vec::new(),
            data_blobs_read: 0,
            index_files_read: 1,
        };

        let v: serde_json::Value = serde_json::from_str(&report_json(&report, false)).unwrap();
        assert_eq!(v["matched"], 1);
        assert_eq!(v["not_matched"], 1);
        assert_eq!(v["could_not_be_placed"], 1);
        assert_eq!(v["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(v["sessions_not_placed"].as_array().unwrap().len(), 1);
        assert_eq!(v["machines_without_activity_index"][0], "m-2");
        assert_eq!(v["complete"], true);
        assert_eq!(v["answer_complete"], false);
        assert_eq!(v["tier"], "metadata");
        assert_eq!(v["payload_loaded"], false);
        assert_eq!(v["time_window"]["how"], "unix_seconds");
        // The unknown is a tagged unknown, never a null and never 0.
        assert_eq!(v["sessions_not_placed"][0]["dimension"], "time");
        assert_eq!(v["sessions_not_placed"][0]["why"], "no activity index");
        assert_eq!(v["sessions"][0]["first_unix"]["kind"], "known");
        assert_eq!(v["sessions"][0]["first_unix"]["unix"], 150);
        assert!(v["sessions"][0]["session_short_id"]
            .as_str()
            .unwrap()
            .contains('~'));
    }

    /// An unknown conversation time on a *matched* session is `unknown` with
    /// the reason — not a null a JSON reader would take for zero, and not a
    /// missing key.
    #[test]
    fn report_json_encodes_an_unknown_time_as_a_reasoned_unknown() {
        let report = SearchReport {
            destination: "dest".into(),
            snapshots_in_repo: 1,
            snapshots_scanned: 1,
            sessions_seen: 1,
            window: None,
            hits: vec![SessionHit {
                machine: "m".into(),
                session_id: "codex.m.bbbbbbbb-0000-0000-0000-000000000002".into(),
                harness: Some("codex".into()),
                shard_count: 1,
                bytes: 1,
                snapshot_id: "abcdef0123456789".into(),
                archive_time_unix: 1,
                first_unix: None,
                last_unix: None,
                time_why: Some("no timestamp field found".into()),
                data_blobs: 1,
                line_count: 1,
                time_source: ActivityTimeSource::Unknown {
                    why: "no timestamp field found".into(),
                },
            }],
            unplaced: Vec::new(),
            not_matched: 0,
            machines_without_index: Vec::new(),
            hosts: Vec::new(),
            unreadable: Vec::new(),
            data_blobs_read: 0,
            index_files_read: 0,
        };
        let v: serde_json::Value = serde_json::from_str(&report_json(&report, false)).unwrap();
        assert_eq!(v["sessions"][0]["first_unix"]["kind"], "unknown");
        assert_eq!(
            v["sessions"][0]["first_unix"]["why"],
            "no timestamp field found"
        );
        assert!(v["sessions"][0]["first_unix"]["unix"].is_null());
        assert_eq!(v["time_window"], serde_json::Value::Null);
        assert!(v["answer_complete"].as_bool().unwrap());
        assert_eq!(v["fulltext_cost_if_loaded"]["note"], "not requested");
    }

    /// `--cost` says what the payload pass would cost; without it the same
    /// block is present but marked "not requested", so a consumer cannot read
    /// a missing field as a cost of zero.
    #[test]
    fn report_json_marks_an_unrequested_cost_as_unrequested() {
        let report = SearchReport {
            destination: "d".into(),
            snapshots_in_repo: 0,
            snapshots_scanned: 0,
            sessions_seen: 0,
            window: None,
            hits: Vec::new(),
            unplaced: Vec::new(),
            not_matched: 0,
            machines_without_index: Vec::new(),
            hosts: Vec::new(),
            unreadable: Vec::new(),
            data_blobs_read: 0,
            index_files_read: 0,
        };
        let asked: serde_json::Value = serde_json::from_str(&report_json(&report, true)).unwrap();
        assert_eq!(
            asked["fulltext_cost_if_loaded"]["note"],
            "not implemented, not performed"
        );
    }

    /// An index row that contradicts itself (`Exact` but no bounds) is unknown,
    /// not "starts at zero". `inbox.rs`'s `modified_ns` is the precedent.
    #[test]
    fn a_self_contradicting_index_row_becomes_unknown_not_zero() {
        let row = ActivityRow {
            session_id: "claude-code.m.abc".into(),
            machine: "m".into(),
            harness: "claude-code".into(),
            first_unix: None,
            last_unix: None,
            line_count: 3,
            time_source: ActivityTimeSource::Exact,
            source_zone: None,
        };
        let t = indexed_time(&row);
        assert_eq!(t.first_unix, None);
        assert_eq!(t.last_unix, None);
        let why = t.why.expect("a contradiction must carry a reason");
        assert!(why.contains("unknown rather than zero"), "{why}");
    }

    /// A row that says `Unknown` keeps the parser's own wording — that text is
    /// the whole reason the field exists.
    #[test]
    fn an_unknown_index_row_keeps_its_recorded_reason() {
        let row = ActivityRow {
            session_id: "s".into(),
            machine: "m".into(),
            harness: "codex".into(),
            first_unix: None,
            last_unix: None,
            line_count: 0,
            time_source: ActivityTimeSource::Unknown {
                why: "no timestamp field found".into(),
            },
            source_zone: None,
        };
        assert_eq!(
            indexed_time(&row).why.as_deref(),
            Some("no timestamp field found")
        );
    }

    /// A fully-known row carries no `why` at all: a reason must mean something.
    #[test]
    fn a_known_index_row_carries_no_reason() {
        let row = ActivityRow {
            session_id: "s".into(),
            machine: "m".into(),
            harness: "codex".into(),
            first_unix: Some(10),
            last_unix: Some(20),
            line_count: 1,
            time_source: ActivityTimeSource::Inferred {
                how: "numeric epoch".into(),
            },
            source_zone: None,
        };
        let t = indexed_time(&row);
        assert_eq!(
            (t.first_unix, t.last_unix, t.why),
            (Some(10), Some(20), None)
        );
    }
}
