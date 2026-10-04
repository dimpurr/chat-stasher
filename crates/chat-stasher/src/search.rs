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
//!   timestamp and one capped one-line label per session), not conversation
//!   content — with the label the one piece of conversation-derived text the
//!   declaration admits (a harness title, or the head of the session's first
//!   user line, at most 100 characters, recorded by the R3 label column). So
//!   the promise this module keeps is narrower and exact: *a shard of
//!   conversation never gets read*, which is what
//!   [`SearchReport::data_blobs_read`] counts and the tests prove by removing
//!   every data pack. Index reads are counted separately, in
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
use rustic_core::{
    FileType, IndexedFullStatus, LsOptions, ReadBackend, Repository, RepositoryBackends, TreeId,
};
use std::collections::{BTreeMap, BTreeSet};

use crate::activity::{
    ActivityRow, ProjectProvenance, SessionTitle, TimeSource as ActivityTimeSource, TitleSource,
};
use crate::readback::{bucket_shard_path, snapshots_by_host_newest_first};
use crate::selector::{Selector, SessionMeta, TimeBounds, TimeWindow, UnplacedBy, Verdict};
use crate::sidecar::{activity_index_machine, infer_harness};
use crate::snapshot_cache::{CachedIndexFile, CachedSession, SnapshotEntry};
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
    /// Why the conversation time is unknown, **or** why the known bounds are
    /// only part of the session's span (`TimeSource::PartialRange`). `Some`
    /// whenever either bound is `None` or the bounds are partial, so the reason
    /// reaches the terminal instead of being reinvented there. `None` only when
    /// both bounds are known to be the whole span.
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
    /// The label this session is listed under, resolved from the index row —
    /// the design's three label states plus the query-time corner the design
    /// names at machine level (see [`SessionLabel`]).
    pub title: SessionLabel,
    /// Capture-time provenance and the latest supplemental attribution.
    pub provenance: Option<ProjectProvenance>,
    /// Collection-time source path class and parent-session reference.
    pub session_provenance: Option<crate::activity::SessionProvenance>,
    /// Generic 4D provenance retained even when a filter could not place the
    /// session. An empty value means no dimensions were recorded.
    pub dimensions: crate::provenance::SessionProvenance,
    /// W219 · The comparable account keys this session's own records carry, from
    /// its activity-index row. Empty means no comparable key was recorded — an
    /// unknown account, or an index written before the field existed — which is
    /// the only distinction the identity question needs.
    ///
    /// Carried so the dashboard can judge an archive id's account the same way
    /// `overview` does, instead of inventing a second rule: one function,
    /// [`crate::overview::conversation_identities`], decides both.
    pub account_keys: Vec<crate::activity::AccountKey>,
}

/// The label state one session resolves to once its index row has been read
/// (29-UI-DESIGN §2.2's three states, plus the two query-time corners that
/// arise when there is no row to read at all):
///
/// * [`SessionLabel::Known`] and [`SessionLabel::NoLabelRecorded`] are what
///   the row recorded — a label with provenance, or an honest absence;
/// * [`SessionLabel::LegacyIndex`] means the row exists but predates labels:
///   this machine's index was written before the `title` key existed, a
///   machine-level state (`SearchReport::machines_with_legacy_index`) the
///   consumer explains once per machine rather than shouting per row;
/// * [`SessionLabel::Unknown`] means there was no row to read — no index for
///   the machine at all, or none for this session — with the `why` saying
///   which, mirroring how the time side reports the very same corner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLabel {
    Known {
        text: String,
        source: TitleSource,
        truncated: bool,
    },
    NoLabelRecorded,
    LegacyIndex,
    Unknown {
        why: String,
    },
}

impl SessionLabel {
    /// Resolve a session's label from its index row. `indexed` is the same
    /// lookup the times come from; `None` means this session has no row (or
    /// the machine no index), and `machine_with_index` says which of the two.
    fn from_indexed(indexed: Option<&IndexedTime>, machine: &str, machine_has_index: bool) -> Self {
        match indexed {
            Some(t) => match &t.title {
                Some(SessionTitle::Known {
                    text,
                    source,
                    truncated,
                }) => SessionLabel::Known {
                    text: text.clone(),
                    source: source.clone(),
                    truncated: *truncated,
                },
                Some(SessionTitle::NoLabelRecorded) => SessionLabel::NoLabelRecorded,
                None => SessionLabel::LegacyIndex,
            },
            None => SessionLabel::Unknown {
                why: if machine_has_index {
                    format!(
                        "machine `{machine}`'s activity index in this snapshot has no row \
                         for this session, so its label was never recorded"
                    )
                } else {
                    format!(
                        "machine `{machine}` has no activity index \
                         (`meta/{machine}/activity-v1.jsonl`) in this snapshot, so its \
                         label was never recorded"
                    )
                },
            },
        }
    }
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
/// unknown, or its recorded bounds are only part of its span (so a window they
/// do not reach may still hold the rest of it), or its harness cannot be
/// derived, so a question the user actually asked has no answer for it.
/// Reporting it as "not matched" would be the "unknown recorded as empty"
/// failure this repo forbids; dropping it silently would be worse.
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
    /// Collection-time source path class and parent-session reference.
    pub session_provenance: Option<crate::activity::SessionProvenance>,
    /// Generic 4D provenance retained even when a filter could not place the
    /// session. An empty value means no dimensions were recorded.
    pub dimensions: crate::provenance::SessionProvenance,
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
    /// Snapshots whose tree was actually walked. Cumulative since ADR-021's
    /// rule reached this reader: every snapshot of every hostname is walked
    /// unless it failed, so `snapshots_scanned < snapshots_in_repo` is exactly
    /// the "a snapshot could not be scanned" state — see
    /// [`SearchReport::scanned_all_snapshots`].
    pub snapshots_scanned: usize,
    /// Of those, the ones answered from the snapshot session cache rather than
    /// by walking their tree (SRCH-1B).
    ///
    /// It counts *snapshots*, so it is a subset of [`Self::snapshots_scanned`],
    /// never a second denominator: a cached snapshot was accounted for, and the
    /// count of what was accounted for must not move with how it was accounted
    /// for. It is a property of the **run**, not of the archive — the same
    /// destination reports a different number on a cold cache, a warm one and
    /// with no cache at all, and reports the same answer every time.
    pub snapshots_from_cache: usize,
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
    /// Per-machine located and time-unknown counts before the query window is
    /// applied. Used for recall warnings so a narrow day cannot inflate the
    /// unknown share by excluding known-time sessions from its denominator.
    pub all_recall: BTreeMap<String, (usize, usize)>,
    /// Session-count deltas: sessions every active filter evaluated and
    /// rejected. A measurement, not a fallback.
    pub not_matched: usize,
    /// Machines that have sessions in a scanned snapshot but no activity index
    /// beside them. Named explicitly: a machine missing from the index must
    /// never look like a machine with no sessions.
    pub machines_without_index: Vec<String>,
    /// Machines whose index predates labels: at least one of their rows
    /// carried no `title` key. Named at machine level on purpose — the
    /// affected sessions' labels read as unknown, and the consumer explains
    /// the machine once, not once per row.
    pub machines_with_legacy_index: Vec<String>,
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
            if hit.time_source.is_no_conversation_content() {
                // ADR-035: no conversation content is a third state, not a
                // time-unknown, so it must not inflate this machine's recall gap.
                continue;
            }
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
        let overall: BTreeMap<String, MachineWindowSummary> = self
            .machine_recall_summary()
            .into_iter()
            .map(|summary| (summary.machine.clone(), summary))
            .collect();
        self.machine_window_summary()
            .into_iter()
            .filter_map(|window| {
                overall
                    .get(&window.machine)
                    .cloned()
                    .map(|summary| (window.machine, summary))
            })
            .filter(|(_, summary)| summary.should_warn())
            .map(|(machine, summary)| format!(
                "WARN: machine `{}` has high unknown-time share ({}/{} candidates); index_trusted={}. Repair with `chat-stasher activity-index --rebuild --destination <destination> --machine <machine> --stage <workspace>`.",
                machine,
                summary.time_unknown,
                summary.located + summary.time_unknown,
                summary.index_trusted,
            ))
            .collect()
    }

    /// Overall per-machine recall counts, independent of a query window.
    pub fn machine_recall_summary(&self) -> Vec<MachineWindowSummary> {
        let mut counts = self.all_recall.clone();
        if counts.is_empty() {
            // reason: manually constructed reports and old serialized fixtures do not
            // carry the pre-window counters; their available candidate counts are the
            // only measured values and remain explicit rather than inferred as zero.
            for host in &self.hosts {
                counts.entry(host.hostname.clone()).or_default();
            }
            for hit in &self.hits {
                if hit.time_source.is_no_conversation_content() {
                    // ADR-035: no conversation content is not a time-unknown.
                    continue;
                }
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
                    machine,
                    located,
                    time_unknown,
                    index_trusted,
                }
            })
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

    /// Whether every snapshot in the repository was walked.
    ///
    /// A search enumerates a destination's sessions by walking snapshot trees,
    /// so a snapshot that was never walked is a set of sessions that was never
    /// looked for. Since the walk is cumulative ([`Self::complete`] covers the
    /// snapshots it *tried*), this is the second half of "did we look
    /// everywhere": `complete()` says no walk failed, this says there was no
    /// walk left undone. Both are needed — a search that stopped early, or a
    /// repository the snapshot listing under-counts, is not a destination that
    /// was read in full.
    pub fn scanned_all_snapshots(&self) -> bool {
        self.snapshots_scanned >= self.snapshots_in_repo
    }

    /// Whether the query was answered for **every** session seen.
    ///
    /// This is stricter than [`Self::complete`]: a destination can be read in
    /// full and still not answer the question, because some session's
    /// conversation time is unknown, or because a snapshot was never walked
    /// (see [`Self::scanned_all_snapshots`]). When either happens,
    /// `hits.is_empty()` proves nothing — hence the separate name, so a caller
    /// cannot pick the weaker one by accident.
    ///
    /// A session with **no conversation content** (ADR-035) does not block the
    /// answer: there is no conversation to be in the window, so "not matched"
    /// is a proven result for it, not an unproven absence.
    pub fn answer_complete(&self) -> bool {
        self.complete()
            && self.scanned_all_snapshots()
            && self
                .unplaced
                .iter()
                .all(|u| u.dimension == UnplacedBy::NoContent)
    }

    /// Unplaced sessions that actually leave the answer unproven (everything
    /// except the no-conversation-content state, which is a proven non-match).
    fn unplaced_leaving_answer_open(&self) -> usize {
        self.unplaced
            .iter()
            .filter(|u| u.dimension != UnplacedBy::NoContent)
            .count()
    }

    /// The one terminal line for "your query matched nothing". The cases are
    /// deliberately different sentences, and only the last one may say the
    /// session is not here.
    pub fn no_hit_line(&self) -> String {
        if !self.complete() {
            format!(
                "search: UNKNOWN — could not finish reading `{}` ({} problem(s) recorded, see the list above); 0 matched in the part I could read. This is NOT \"not there\".",
                self.destination,
                self.unreadable.len(),
            )
        } else if !self.scanned_all_snapshots() {
            format!(
                "search: UNKNOWN — 0 of {} sessions matched in `{}`, but only {} of {} snapshots were scanned, and a snapshot that was not scanned is a set of sessions that was not looked for. This is NOT \"not there\".",
                self.sessions_seen, self.destination, self.snapshots_scanned, self.snapshots_in_repo,
            )
        } else if self.unplaced_leaving_answer_open() > 0 {
            format!(
                "search: UNKNOWN — 0 of {} sessions matched in `{}`, but {} session(s) could not be placed (see the list below), so this is NOT \"not there\".",
                self.sessions_seen,
                self.destination,
                self.unplaced_leaving_answer_open()
            )
        } else {
            format!(
                "search: not in this destination — 0 of {} sessions matched in `{}` (all {} of {} snapshots scanned, all readable)",
                self.sessions_seen, self.destination, self.snapshots_scanned, self.snapshots_in_repo,
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
    let time_state = |unix: Option<i64>, why: Option<&str>, source: &ActivityTimeSource| match unix
    {
        Some(unix) => crate::json_out::TimeState::known(unix),
        None if source.is_no_conversation_content() => {
            crate::json_out::TimeState::no_conversation_content()
        }
        None => crate::json_out::TimeState::unknown(
            why.unwrap_or("no conversation time was recorded for this session")
                .to_string(),
        ),
    };
    let hits: Vec<serde_json::Value> = report
        .hits
        .iter()
        .map(|h| {
            let mut hit = serde_json::json!({
                "machine": h.machine,
                "session_short_id": h.short_id(),
                "harness": h.harness,
                "shards": h.shard_count,
                "bytes": h.bytes,
                "snapshot_short_id": h.short_snapshot(),
                "archive_time_unix": h.archive_time_unix,
                "first_unix": time_state(h.first_unix, h.time_why.as_deref(), &h.time_source),
                "last_unix": time_state(h.last_unix, h.time_why.as_deref(), &h.time_source),
                "dimensions": h.dimensions,
            });
            if let Some(provenance) = &h.provenance {
                hit["provenance"] = serde_json::json!(provenance);
            }
            if let Some(provenance) = &h.session_provenance {
                hit["session_provenance"] = serde_json::json!(provenance);
            }
            hit
        })
        .collect();
    let unplaced: Vec<serde_json::Value> = report
        .unplaced
        .iter()
        .map(|u| {
            let mut item = serde_json::json!({
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
                    UnplacedBy::Surface => "surface",
                    UnplacedBy::NoContent => "no_content",
                },
                "why": u.why,
            });
            if let Some(provenance) = &u.session_provenance {
                item["session_provenance"] = serde_json::json!(provenance);
            }
            item["dimensions"] = serde_json::json!(u.dimensions);
            item
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
        // Of the snapshots accounted for, how many were answered from the local
        // session cache instead of by walking their tree. A property of this
        // run rather than of the archive: it counts what the cache *served*,
        // and the answer above is the same whichever way each snapshot was
        // accounted for.
        "snapshots_from_cache": report.snapshots_from_cache,
        "snapshots_in_repo": report.snapshots_in_repo,
        // Read `answer_complete` before concluding anything from `sessions`
        // being empty: a destination whose snapshots were not all scanned is
        // not a destination where the session is absent.
        "snapshots_all_scanned": report.scanned_all_snapshots(),
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

/// What the activity index said about one session's conversation time, and
/// (since R3) what its row recorded as the session's label.
#[derive(Debug, Clone)]
struct IndexedTime {
    first_unix: Option<i64>,
    last_unix: Option<i64>,
    /// `Some` == the time is unknown and this is why.
    why: Option<String>,
    line_count: u64,
    /// The index's own tri-state, passed through unchanged.
    source: ActivityTimeSource,
    /// The row's label, or `None` when the row predates labels (the
    /// machine-level [`SearchReport::machines_with_legacy_index`] state —
    /// see [`SessionLabel::LegacyIndex`]).
    title: Option<SessionTitle>,
    provenance: Option<ProjectProvenance>,
    session_provenance: Option<crate::activity::SessionProvenance>,
    dimensions: crate::provenance::SessionProvenance,
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
        // A partial range keeps its measured bounds **and** the reason they are
        // not the whole span: the selector needs that reason to report "may be
        // outside" instead of excluding the session.
        ActivityTimeSource::PartialRange { why, .. } => Some(why.clone()),
        _ => None,
    };
    let line_count = row.line_count;
    let title = row.title.clone();
    let provenance = row.provenance.clone();
    let session_provenance = row.session_provenance.clone();
    let dimensions = row.dimensions.clone();
    match (row.first_unix, row.last_unix, why) {
        (None, None, None) if row.time_source.is_no_conversation_content() => IndexedTime {
            first_unix: None,
            last_unix: None,
            why: None,
            line_count,
            source: ActivityTimeSource::NoConversationContent,
            title,
            provenance,
            session_provenance,
            dimensions: dimensions.clone(),
        },
        // Bounds that are only part of the span: carried through as measured,
        // with the partiality kept on the source so no consumer answers
        // "outside the window" from them.
        (Some(first), Some(last), Some(why)) if row.time_source.bounds_are_partial() => {
            IndexedTime {
                first_unix: Some(first),
                last_unix: Some(last),
                why: Some(why),
                line_count,
                source: row.time_source.clone(),
                title,
                provenance,
                session_provenance,
                dimensions: dimensions.clone(),
            }
        }
        (Some(first), Some(last), _) => IndexedTime {
            first_unix: Some(first),
            last_unix: Some(last),
            why: None,
            line_count,
            source: row.time_source.clone(),
            title,
            provenance,
            session_provenance,
            dimensions: dimensions.clone(),
        },
        (first, last, Some(why)) => IndexedTime {
            first_unix: first,
            last_unix: last,
            why: Some(why.clone()),
            line_count,
            source: ActivityTimeSource::Unknown { why },
            title,
            provenance,
            session_provenance,
            dimensions: dimensions.clone(),
        },
        (first, last, None) => IndexedTime {
            first_unix: first,
            last_unix: last,
            why: Some(NO_BOUND.to_string()),
            line_count,
            source: ActivityTimeSource::Unknown {
                why: NO_BOUND.to_string(),
            },
            title,
            provenance,
            session_provenance,
            dimensions,
        },
    }
}

/// One session as a **single** snapshot holds it: how many shards, how many
/// bytes, and which snapshot that was.
///
/// The snapshot is carried because a cumulative search reports a session
/// against the newest snapshot that holds it, which is not the same snapshot
/// for every session of a machine whose stage has been reclaimed.
#[derive(Debug, Clone)]
struct SessionSlot {
    shard_count: usize,
    bytes: u64,
    data_blobs: usize,
    snapshot_id: String,
    archive_time_unix: i64,
}

/// Whether a cached snapshot's trees are still there to be read (SRCH-1B).
///
/// A cache hit stands in for a tree walk, and a walk reads tree blobs. A
/// snapshot's id proves its *contents* cannot have changed; it says nothing
/// about whether the destination still holds the packs those contents live in.
/// A pack can be lost after an entry was written, and the uncached path reports
/// exactly that — the walk fails on the first tree it cannot fetch, the
/// snapshot lands in `unreadable`, and the answer comes back partial. A hit
/// accepted on the strength of its own bytes would report the same archive as
/// complete, which is the one thing this cache may never do.
///
/// So a hit is admissible only when this says the snapshot's trees are still
/// readable: every tree id the walk read must still be in the repository's
/// index, and the destination must still hold the pack that index places it in.
/// Neither question fetches a tree or touches a shard. What it deliberately
/// does **not** ask about is data blobs: a walk never reads a shard body, so a
/// lost data pack is not a snapshot that cannot be walked — checking it would
/// make the cached path stricter than the uncached one, which is its own way of
/// changing an answer.
///
/// # Why the packs are read rather than listed
///
/// The obvious way to ask the second question is to list the destination's
/// packs and look for the id. It is what this first did, and it is not cheap on
/// the destination this cache exists for: over `opendal:sftp` a pack listing
/// enumerates every one of the 256 hex prefix directories under `data/`, which
/// **measured 31 seconds and ~1100 round trips** for a 40-snapshot repository
/// (`w246_remote_cache_test::snapshot_cache_over_a_latency_injected_sftp_link`,
/// 20 ms round trip) — more than the whole search it was meant to speed up.
///
/// One ranged read of the pack itself is one request, and it is the *stronger*
/// fact of the two: a name in a listing is a name, while bytes that come back
/// are proof the object is still there. The read is one byte, at offset 0, of
/// the encrypted pack, and it is discarded — no tree and no shard is fetched,
/// decrypted or parsed, and `data_blobs_read` is untouched by it.
///
/// What it proves is *reachability*, not integrity. A pack that is present and
/// readable can still be truncated, and neither this reader nor the one it
/// replaces has ever detected that: the uncached path fetches a tree through
/// such a pack and panics on the slice (`crate::reader_guard` documents the
/// hazard, and `verify` is where a short pack is reported). A hit does not make
/// that state worse — it answers what a healthy walk of that snapshot would
/// have answered — and a *missing* pack, which is the damage this exists for,
/// fails the read and sends the snapshot back to its walk.
struct TreeAvailability {
    /// Packs already asked about, and whether the destination could read them.
    ///
    /// Memoised for the whole search, not per snapshot: the same pack holds the
    /// trees of many snapshots (a push writes one), so without this a
    /// 417-snapshot search would ask about the same pack hundreds of times —
    /// which is the cost this exists to avoid, one request at a time.
    packs: BTreeMap<String, bool>,
}

/// Whether the destination still holds the pack `pack` — asked by reading one
/// byte of it.
///
/// `cacheable = false` on purpose, and asked of the destination's own backend
/// handles rather than of the reader: this is a question about what the
/// *destination* holds, and an answer served from rustic's local copy of the
/// pack is precisely the answer that must not be given.
fn pack_is_readable(backends: &RepositoryBackends, pack: &str) -> bool {
    let Ok(id) = pack.parse::<rustic_core::Id>() else {
        return false;
    };
    backends
        .repository()
        .read_partial(FileType::Pack, &id, false, 0, 1)
        .is_ok()
}

impl TreeAvailability {
    fn new() -> Self {
        TreeAvailability {
            packs: BTreeMap::new(),
        }
    }

    /// Whether every tree in `trees` is still in the repository's index with
    /// its pack still readable at the destination.
    ///
    /// An empty list is not a proof: every entry this cache writes names at
    /// least the snapshot's own root, so a body with no trees is not one of
    /// ours, and nothing about it may be trusted.
    fn holds(
        &mut self,
        repo: &Repository<IndexedFullStatus>,
        backends: &RepositoryBackends,
        trees: &[String],
    ) -> bool {
        for hex in trees {
            let Ok(tree) = hex.parse::<TreeId>() else {
                return false;
            };
            // `get_index_entry` is "is this blob in the index"; the pack it
            // names is then asked about, because an index entry outlives the
            // pack file it describes.
            let Ok(entry) = repo.get_index_entry(&tree) else {
                return false;
            };
            let pack = entry.pack.to_hex().as_str().to_string();
            let readable = match self.packs.get(&pack) {
                Some(known) => *known,
                None => {
                    let readable = pack_is_readable(backends, &pack);
                    self.packs.insert(pack, readable);
                    readable
                }
            };
            if !readable {
                return false;
            }
        }
        !trees.is_empty()
    }
}

/// Metadata-tier search over one destination.
///
/// Reuses the read path `readback` already established: open fresh, group
/// snapshots by hostname, `ls` their trees and bucket shard paths with
/// [`bucket_shard_path`]. The one difference — and it is the whole point — is
/// that this walk never dumps a **session shard**, so no conversation blob is
/// fetched or decrypted. Each machine's activity sidecar is read in the same
/// pass, because that is where the conversation times live.
///
/// The walk is **cumulative over every snapshot of a hostname**, newest first,
/// with the first appearance of a session winning (ADR-021) — the same rule,
/// through the same function, that `read --all-machines` and `verify` L3 use.
/// Looking only at the newest snapshot is what made a reclaimed machine's older
/// conversations invisible; see the walk's own comment for the measurement that
/// motivated it.
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
    let (repo, _adoption) = crate::orphans::open_adopting(&store.cfg, &backends, mk)
        .context("open repository for search")?;

    let snaps = repo
        .get_all_snapshots()
        .context("list snapshots for search")?;
    let snapshots_in_repo = snaps.len();

    // SRCH-1b · the per-destination snapshot session cache, when the caller
    // installed one. `None` is the uncached search this module has always done,
    // and every path below reads identically with it.
    //
    // The listing above is the cache's authority: an entry is only ever read
    // for a snapshot in it, and every entry for a snapshot *not* in it is
    // dropped here. Snapshots are immutable, so an entry cannot go stale while
    // its snapshot is listed; what this closes is the other direction — an
    // entry outliving the listing that justified it, which would let a pruned
    // snapshot answer for a repository that no longer holds it.
    let cache = store.snapshot_cache();
    if let Some(cache) = cache {
        let listed: BTreeSet<String> = snaps
            .iter()
            .map(|snap| snap.id.to_hex().as_str().to_string())
            .collect();
        let _pruned = cache.retain(&listed);
    }
    // What a hit has to be judged against, fetched at most once and only if
    // there is a hit to judge — see [`TreeAvailability`].
    let mut trees_available = TreeAvailability::new();

    let mut report = SearchReport {
        destination: store.cfg.repo_root.clone(),
        snapshots_in_repo,
        // Counted as the trees are really walked, so it can only ever be short
        // of `snapshots_in_repo` for the same reason a snapshot is in
        // `unreadable` — never a second count that could drift from that list.
        snapshots_scanned: 0,
        snapshots_from_cache: 0,
        sessions_seen: 0,
        window: selector.window.clone(),
        hits: Vec::new(),
        unplaced: Vec::new(),
        all_recall: BTreeMap::new(),
        not_matched: 0,
        machines_without_index: Vec::new(),
        machines_with_legacy_index: Vec::new(),
        hosts: Vec::new(),
        unreadable: Vec::new(),
        data_blobs_read: 0,
        index_files_read: 0,
    };
    let mut index_machines: BTreeSet<String> = BTreeSet::new();
    // Rows whose `title` key was absent — an index written before labels
    // existed. Machine-level state; see the report field of the same name.
    let mut legacy_index_machines: BTreeSet<String> = BTreeSet::new();

    for (host, host_snaps) in snapshots_by_host_newest_first(snaps) {
        // The machine-level identity comes from the host's **newest** snapshot
        // (which run this host's row describes, and when it ran). The sessions
        // come from all of them.
        let newest = &host_snaps[0];
        let snapshot_id = newest.id.to_hex().as_str().to_string();
        let archive_time_unix = newest.time.timestamp().as_second();

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

        // ---- cumulative session enumeration (ADR-021) -----------------------
        //
        // `reclaim-stage` deletes a session's shard bodies from the stage once
        // every declared destination has proved it holds them, so the newest
        // snapshot of a busy machine holds only the last push's batch: in the
        // W253 field test `mac`'s newest snapshot held 16 sessions against the
        // 3178 its activity index records, and the other 3162 bodies were in
        // older snapshots. Looking only at the newest snapshot therefore
        // answered "not in this destination" about a destination that holds the
        // session. Every snapshot is walked here instead.
        //
        // Newest first, and the first appearance of a session wins: a session
        // is reported against the newest snapshot that holds it. That is the
        // same rule `read --all-machines` / `verify` L3 apply, resolved by the
        // same function ([`snapshots_by_host_newest_first`]), so the readers
        // cannot disagree about where a session lives.
        //
        // Tree metadata only: no shard is ever dumped here, which is why
        // `data_blobs_read` stays 0. A snapshot that cannot be walked is
        // recorded in `unreadable` and skipped — an older snapshot we could not
        // read might hold a session we did not find, so that makes the answer
        // partial, never empty.
        let mut sessions: BTreeMap<(String, String), SessionSlot> = BTreeMap::new();
        // Owned, not borrowed: each snapshot's entry list is scoped to its own
        // turn of the loop, while the index node has to outlive it until the
        // sidecar is read below.
        let mut index_nodes: BTreeMap<String, rustic_core::repofile::Node> = BTreeMap::new();
        let mut partial_indexes: BTreeSet<String> = BTreeSet::new();
        let mut machines_with_missing_rows: BTreeSet<String> = BTreeSet::new();
        for (depth, snap) in host_snaps.iter().enumerate() {
            let snap_id = snap.id.to_hex().as_str().to_string();
            let snap_time_unix = snap.time.timestamp().as_second();

            // ---- the cache hit (SRCH-1b) ----------------------------------
            //
            // A snapshot's id is the hash of its own contents, so what a walk
            // of it found can never change: an entry is valid for exactly as
            // long as the repository lists the snapshot, and `retain` drops it
            // the moment the listing stops naming it. A hit therefore stands in
            // for the walk below and reproduces its buckets — including the
            // counting, because a cached snapshot is one that *was* accounted
            // for, and `snapshots_scanned` must mean the same thing either way.
            //
            // Listing the snapshot is not the same as still being able to read
            // it, so a hit has a second condition before it is used: the trees
            // the walk would read must still be in the repository's index with
            // their packs still listed ([`TreeAvailability`]). Without that, a
            // pack lost after the entry was written would leave this reader
            // calling an archive complete that the uncached path reports as
            // unreadable — the cache changing an answer, which it may never do.
            //
            // A hit that fails either condition is not an error and not a
            // finding of its own: it is simply not used, and the walk below runs
            // and says what is really true of this snapshot — in its own words,
            // in the same `unreadable` list the uncached path uses, or as a
            // fresh walk if the snapshot was readable after all.
            //
            // The one thing a hit does not stand in for is the activity index.
            // The entry carries the index's archived path, and the file itself
            // is fetched, dumped and parsed below exactly as it is after a
            // walk, so "no index", "index unreadable" and "index malformed"
            // keep being answered by the snapshot rather than by a note about
            // it. A node that cannot be fetched now leaves the hit unused and
            // the walk runs, on the same terms.
            if let Some(cache) = cache {
                if let Some(cached) = cache.load(&snap_id) {
                    if trees_available.holds(&repo, &backends, &cached.trees) {
                        let mut index_nodes_from_cache: BTreeMap<
                            String,
                            rustic_core::repofile::Node,
                        > = BTreeMap::new();
                        let mut usable = true;
                        if depth == 0 {
                            for file in &cached.index_files {
                                match repo.node_from_snapshot_and_path(snap, &file.path) {
                                    Ok(node) => {
                                        index_nodes_from_cache.insert(file.machine.clone(), node);
                                    }
                                    Err(_) => {
                                        usable = false;
                                        break;
                                    }
                                }
                            }
                        }
                        if usable {
                            report.snapshots_scanned += 1;
                            report.snapshots_from_cache += 1;
                            index_nodes.extend(index_nodes_from_cache);
                            for row in cached.sessions {
                                sessions
                                    .entry((row.machine, row.session))
                                    .or_insert(SessionSlot {
                                        shard_count: row.shard_count,
                                        bytes: row.bytes,
                                        data_blobs: row.data_blobs,
                                        snapshot_id: snap_id.clone(),
                                        archive_time_unix: snap_time_unix,
                                    });
                            }
                            continue;
                        }
                    }
                }
            }

            let root = match repo.node_from_snapshot_and_path(snap, "") {
                Ok(node) => node,
                Err(e) => {
                    report.unreadable.push(format!(
                        "host `{host}`: snapshot {} tree root unreadable: {e}",
                        &snap_id[..8.min(snap_id.len())]
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
                        &snap_id[..8.min(snap_id.len())]
                    ));
                    continue;
                }
            };
            report.snapshots_scanned += 1;

            // One pass over this snapshot's tree collects both halves: the
            // shard paths that say which sessions it holds, and — in the newest
            // snapshot only — the index files that say when they were active.
            let mut snap_sessions: BTreeMap<(String, String), (usize, u64, usize)> =
                BTreeMap::new();
            // Every activity index this snapshot holds, at every depth, because
            // a cache entry is keyed by snapshot id alone: a snapshot that is
            // oldest today can be newest once the ones after it are pruned, and
            // an entry written here must answer the same at either depth.
            let mut index_files: Vec<CachedIndexFile> = Vec::new();
            // Every tree blob this walk reads, which is what a later hit is
            // judged against ([`TreeAvailability`]). The snapshot's own root is
            // read to start the walk, and the streamer reads a subtree before it
            // can yield any directory that names it — so the root plus every
            // entry's `subtree` is the complete set, collected from the walk
            // that just did the reading rather than guessed at.
            let mut trees: BTreeSet<String> = BTreeSet::new();
            trees.insert(snap.tree.to_hex().as_str().to_string());
            for (path, node) in &entries {
                if let Some(subtree) = node.subtree {
                    trees.insert(subtree.to_hex().as_str().to_string());
                }
                if node.node_type != NodeType::File {
                    continue;
                }
                // An activity index is never a session, and that is decided the
                // same way at every depth — it was previously decided only for
                // the newest snapshot, so an older snapshot's index could fall
                // through to `bucket_shard_path` and be bucketed as a session if
                // the machine's stage path happened to contain a `sessions`
                // component. Nothing but an index file is affected, and making
                // the two depths agree is what lets one entry describe a
                // snapshot regardless of where in the order it was walked.
                //
                // The index is still *read* from the newest snapshot alone.
                // `activity-index` rebuilds it cumulatively on every run — a
                // reclaimed session keeps its row, with `line_count: 0` — so
                // the newest index already names every session the machine ever
                // recorded. Reading all of them would re-fetch the same rows
                // once per snapshot and add no session the first one missed.
                // The older snapshots are walked for *bodies*, which is exactly
                // what the index cannot tell us about.
                if let Some(machine) = activity_index_machine(path) {
                    index_files.push(CachedIndexFile {
                        machine: machine.clone(),
                        path: path.to_string_lossy().into_owned(),
                    });
                    if depth == 0 {
                        index_nodes.insert(machine, node.clone());
                    }
                    continue;
                }
                let Some((machine, session, _shard)) = bucket_shard_path(path) else {
                    continue;
                };
                let entry = snap_sessions.entry((machine, session)).or_insert((0, 0, 0));
                entry.0 += 1;
                entry.1 += node.meta.size;
                entry.2 += node.content.as_ref().map_or(0, Vec::len);
            }

            // Record what this snapshot held, so the next run does not walk it
            // again. Written only after the walk has succeeded — a snapshot
            // whose tree could not be read must be retried on every run, never
            // remembered as unreadable — and the result is deliberately dropped:
            // this is a cache, a machine whose cache directory is unwritable is
            // entitled to a slower search, and no answer depends on the write.
            if let Some(cache) = cache {
                let stored = SnapshotEntry::new(
                    &snap_id,
                    snap_sessions
                        .iter()
                        .map(|((machine, session), (shard_count, bytes, data_blobs))| {
                            CachedSession {
                                machine: machine.clone(),
                                session: session.clone(),
                                shard_count: *shard_count,
                                bytes: *bytes,
                                data_blobs: *data_blobs,
                            }
                        })
                        .collect(),
                    index_files,
                    trees.into_iter().collect(),
                );
                let _stored = cache.store(&stored);
            }

            for (key, (shard_count, bytes, data_blobs)) in snap_sessions {
                // `or_insert`, not `+=`: a snapshot holds a full (deduplicated)
                // copy of the stage at its own moment, so a session's body is
                // not spread across snapshots and the older appearances must
                // not be added to the newest one's counts.
                sessions.entry(key).or_insert(SessionSlot {
                    shard_count,
                    bytes,
                    data_blobs,
                    snapshot_id: snap_id.clone(),
                    archive_time_unix: snap_time_unix,
                });
            }
        }

        // ---- the activity sidecars, read before any verdict is reached -----
        let mut times: BTreeMap<(String, String), IndexedTime> = BTreeMap::new();
        // W219 · the index row's comparable account keys, keyed the same way, so
        // the verdict below can hand them to the row it builds.
        let mut accounts: BTreeMap<(String, String), Vec<crate::activity::AccountKey>> =
            BTreeMap::new();
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
                        if row.title.is_none() {
                            legacy_index_machines.insert(machine.clone());
                        }
                        times.insert(
                            (row.machine.clone(), row.session_id.clone()),
                            indexed_time(&row),
                        );
                        if !row.account_keys.is_empty() {
                            accounts.insert(
                                (row.machine.clone(), row.session_id.clone()),
                                row.account_keys.clone(),
                            );
                        }
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

        for ((machine, session_id), slot) in sessions {
            report.sessions_seen += 1;
            let harness = infer_harness(&session_id);
            let indexed = times.get(&(machine.clone(), session_id.clone()));
            let provenance = indexed.and_then(|row| row.provenance.clone());
            // W219 · the row's comparable account keys. Read here, before
            // `machine` / `session_id` are moved into the row below. An empty
            // list is the honest value for "no comparable account key was
            // recorded": no index row at all, an index predating the field, or
            // an `account` envelope of `kind: unknown`. All three answer the
            // one question this list serves — "can two records be compared?" —
            // so folding them into one empty set loses nothing, and inventing
            // a key would be the failure.
            let account_keys = accounts
                .get(&(machine.clone(), session_id.clone()))
                .cloned()
                // reason: see the block above — empty means "nothing to
                // compare", which is the same answer for all three histories
                // and never a claim that the accounts agree.
                .unwrap_or_default();
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
            let no_content = time_source.is_no_conversation_content();
            let title = SessionLabel::from_indexed(
                indexed,
                &machine,
                machines_with_index.contains(&machine),
            );
            let recall = report.all_recall.entry(machine.clone()).or_default();
            if first_unix.is_some() && last_unix.is_some() {
                recall.0 += 1;
            } else if !no_content {
                // ADR-035: a session with no conversation content is not a
                // recall gap — there is nothing whose time we failed to find.
                recall.1 += 1;
            }
            let meta = SessionMeta {
                machine: &machine,
                session_id: &session_id,
                harness: harness.as_deref(),
                first_unix,
                last_unix,
                // The index's own answer, never re-derived from the bounds: a
                // partial range is exactly the case where the bounds alone
                // would read as complete.
                time_bounds: if time_source.bounds_are_partial() {
                    TimeBounds::Partial
                } else {
                    TimeBounds::Complete
                },
                time_why: time_why.as_deref(),
                surfaces: indexed.map(|row| row.dimensions.surface.as_slice()),
            };
            match selector.select(&meta) {
                Verdict::Selected => report.hits.push(SessionHit {
                    machine,
                    session_id,
                    harness,
                    shard_count: slot.shard_count,
                    bytes: slot.bytes,
                    // The snapshot named here is the newest one that holds this
                    // session, not necessarily the host's newest — see the
                    // cumulative walk above.
                    snapshot_id: slot.snapshot_id,
                    archive_time_unix: slot.archive_time_unix,
                    first_unix,
                    last_unix,
                    time_why,
                    data_blobs: slot.data_blobs,
                    line_count,
                    time_source,
                    title,
                    provenance,
                    session_provenance: indexed.and_then(|row| row.session_provenance.clone()),
                    // Empty means no recorded dimensions; selector evaluation
                    // above preserves that as Unevaluated under a surface filter.
                    dimensions: indexed
                        .map_or_else(crate::provenance::SessionProvenance::default, |row| {
                            row.dimensions.clone()
                        }),
                    account_keys,
                }),
                Verdict::NotSelected => report.not_matched += 1,
                Verdict::Unevaluated { dimension, why } => {
                    // A time window cannot place a session with no conversation
                    // content either, but that is a distinct reason from a real
                    // conversation with an unreadable time (ADR-035), and it is
                    // excluded from the unknown tallies.
                    let (dimension, why) = if dimension == UnplacedBy::Time && no_content {
                        (
                            UnplacedBy::NoContent,
                            "this session holds no conversation content (no user or assistant \
                             message); there is nothing to place in a time bucket"
                                .to_string(),
                        )
                    } else {
                        (dimension, why)
                    };
                    report.unplaced.push(UnplacedSession {
                        machine,
                        session_id,
                        harness,
                        shard_count: slot.shard_count,
                        bytes: slot.bytes,
                        snapshot_id: slot.snapshot_id,
                        archive_time_unix: slot.archive_time_unix,
                        dimension,
                        why,
                        line_count,
                        time_source,
                        session_provenance: indexed.and_then(|row| row.session_provenance.clone()),
                        dimensions: indexed
                            .map_or_else(crate::provenance::SessionProvenance::default, |row| {
                                row.dimensions.clone()
                            }),
                    })
                }
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
    report.machines_with_legacy_index = legacy_index_machines.into_iter().collect();
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
            title: SessionLabel::NoLabelRecorded,
            provenance: None,
            session_provenance: None,
            dimensions: Default::default(),
            account_keys: Vec::new(),
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
            snapshots_from_cache: 0,
            sessions_seen: 2,
            window: None,
            all_recall: BTreeMap::new(),
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
                session_provenance: None,
                dimensions: Default::default(),
            }],
            not_matched: 0,
            machines_without_index: Vec::new(),
            machines_with_legacy_index: Vec::new(),
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
    fn no_conversation_content_is_excluded_from_recall_and_unknown() {
        let mut no_content = hit("machine-a", "claude-code.machine-a.empty", 10);
        no_content.line_count = 0;
        no_content.time_why = None;
        no_content.time_source = ActivityTimeSource::NoConversationContent;
        let report = SearchReport {
            destination: "fixture".into(),
            snapshots_in_repo: 1,
            snapshots_scanned: 1,
            snapshots_from_cache: 0,
            sessions_seen: 1,
            window: Some(TimeWindow {
                since_unix: Some(1),
                until_unix: Some(2),
                how: crate::selector::WindowHow::LocalDays,
                since_text: Some("2026-09-24".into()),
                until_text: Some("2026-09-24".into()),
            }),
            all_recall: BTreeMap::new(),
            hits: vec![no_content],
            unplaced: vec![UnplacedSession {
                machine: "machine-a".into(),
                session_id: "claude-code.machine-a.empty".into(),
                harness: Some("claude-code".into()),
                shard_count: 1,
                bytes: 0,
                snapshot_id: "abcdef0123456789".into(),
                archive_time_unix: 10,
                dimension: UnplacedBy::NoContent,
                why: "no conversation content".into(),
                line_count: 0,
                time_source: ActivityTimeSource::NoConversationContent,
                session_provenance: None,
                dimensions: Default::default(),
            }],
            not_matched: 0,
            machines_without_index: Vec::new(),
            machines_with_legacy_index: Vec::new(),
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
        assert_eq!(
            report.machine_window_summary()[0].time_unknown,
            0,
            "no-conversation-content must not count as a time-unknown"
        );
        assert_eq!(
            report.machine_recall_summary()[0].time_unknown,
            0,
            "the overall recall must exclude it too"
        );
        assert!(
            report.machine_recall_warnings().is_empty(),
            "no WARN for a destination whose only unplaceable session has no content"
        );
        assert_eq!(report.session_time_unknown(), 0, "dimension is no_content");
        assert!(
            report.answer_complete(),
            "a no-content session does not leave the query unanswered"
        );
    }

    #[test]
    fn recall_warning_uses_overall_unknown_share_for_a_day_query() {
        let mut report = SearchReport {
            destination: "fixture".into(),
            snapshots_in_repo: 1,
            snapshots_scanned: 1,
            snapshots_from_cache: 0,
            sessions_seen: 101,
            window: Some(TimeWindow {
                since_unix: Some(1),
                until_unix: Some(2),
                how: crate::selector::WindowHow::LocalDays,
                since_text: Some("2026-09-24".into()),
                until_text: Some("2026-09-24".into()),
            }),
            all_recall: [("machine-a".to_string(), (100, 1))].into_iter().collect(),
            hits: Vec::new(),
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
                session_provenance: None,
                dimensions: Default::default(),
            }],
            not_matched: 100,
            machines_without_index: Vec::new(),
            machines_with_legacy_index: Vec::new(),
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
        assert!(report.machine_window_summary()[0].should_warn());
        assert!(report.machine_day_summary()[0].unknown_anywhere > 0);
        assert!(
            report.machine_recall_warnings().is_empty(),
            "one unknown among 101 overall sessions is below the warning threshold"
        );
        report.all_recall.insert("machine-a".into(), (0, 1));
        assert_eq!(report.machine_recall_warnings().len(), 1);
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
            snapshots_from_cache: 0,
            sessions_seen: 2,
            window: Some(TimeWindow {
                since_unix: Some(start),
                until_unix: Some(end),
                how: crate::selector::WindowHow::LocalDays,
                since_text: Some("2026-09-24".into()),
                until_text: Some("2026-09-25".into()),
            }),
            all_recall: BTreeMap::new(),
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
                session_provenance: None,
                dimensions: Default::default(),
            }],
            not_matched: 0,
            machines_without_index: Vec::new(),
            machines_with_legacy_index: Vec::new(),
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
            snapshots_from_cache: 0,
            sessions_seen: 0,
            window: None,
            all_recall: BTreeMap::new(),
            hits: Vec::new(),
            unplaced: Vec::new(),
            not_matched: 0,
            machines_without_index: Vec::new(),
            machines_with_legacy_index: Vec::new(),
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
            snapshots_from_cache: 0,
            sessions_seen: 7,
            window: None,
            all_recall: BTreeMap::new(),
            hits: Vec::new(),
            unplaced: Vec::new(),
            not_matched: 7,
            machines_without_index: Vec::new(),
            machines_with_legacy_index: Vec::new(),
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
            snapshots_from_cache: 0,
            sessions_seen: 4,
            window: None,
            all_recall: BTreeMap::new(),
            hits: Vec::new(),
            unplaced: Vec::new(),
            not_matched: 4,
            machines_without_index: vec!["m-1".into()],
            machines_with_legacy_index: Vec::new(),
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
            session_provenance: None,
            dimensions: Default::default(),
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
            snapshots_from_cache: 0,
            sessions_seen: 2,
            window: None,
            all_recall: BTreeMap::new(),
            hits: vec![hit("m-1", "a", 10), hit("m-1", "b", 20)],
            unplaced: Vec::new(),
            not_matched: 0,
            machines_without_index: Vec::new(),
            machines_with_legacy_index: Vec::new(),
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
            session_provenance: None,
            dimensions: Default::default(),
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
            snapshots_from_cache: 0,
            sessions_seen: 3,
            window: Some(crate::selector::TimeWindow {
                since_unix: Some(100),
                until_unix: Some(200),
                how: crate::selector::WindowHow::UnixSeconds,
                since_text: Some("100".into()),
                until_text: Some("200".into()),
            }),
            all_recall: BTreeMap::new(),
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
                title: SessionLabel::NoLabelRecorded,
                provenance: None,
                session_provenance: None,
                dimensions: Default::default(),
                account_keys: Vec::new(),
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
                session_provenance: None,
                dimensions: Default::default(),
            }],
            not_matched: 1,
            machines_without_index: vec!["m-2".into()],
            machines_with_legacy_index: Vec::new(),
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
            snapshots_from_cache: 0,
            sessions_seen: 1,
            window: None,
            all_recall: BTreeMap::new(),
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
                title: SessionLabel::Unknown {
                    why: "machine `m`'s activity index in this snapshot has no row \
                          for this session, so its label was never recorded"
                        .into(),
                },
                provenance: None,
                session_provenance: None,
                dimensions: Default::default(),
                account_keys: Vec::new(),
            }],
            unplaced: Vec::new(),
            not_matched: 0,
            machines_without_index: Vec::new(),
            machines_with_legacy_index: Vec::new(),
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

    #[test]
    fn report_json_carries_session_source_class_and_parent_reference() {
        let mut hit = hit("m-fixture", "claude-code.m-fixture.agent-fixture", 1);
        let provenance = crate::activity::SessionProvenance {
            source_path_class: "subagents".into(),
            parent_session_ref: Some("parent-fixture".into()),
        };
        hit.session_provenance = Some(provenance.clone());
        let report = SearchReport {
            destination: "dest-fixture".into(),
            snapshots_in_repo: 1,
            snapshots_scanned: 1,
            snapshots_from_cache: 0,
            sessions_seen: 1,
            window: None,
            all_recall: BTreeMap::new(),
            hits: vec![hit],
            unplaced: vec![UnplacedSession {
                machine: "m-fixture".into(),
                session_id: "claude-code.m-fixture.unplaced-fixture".into(),
                harness: Some("claude-code".into()),
                shard_count: 1,
                bytes: 1,
                snapshot_id: "abcdef0123456789".into(),
                archive_time_unix: 1,
                dimension: UnplacedBy::Time,
                why: "no timestamp in fixture".into(),
                line_count: 1,
                time_source: ActivityTimeSource::Unknown {
                    why: "no timestamp in fixture".into(),
                },
                session_provenance: Some(provenance),
                dimensions: Default::default(),
            }],
            not_matched: 0,
            machines_without_index: Vec::new(),
            machines_with_legacy_index: Vec::new(),
            hosts: Vec::new(),
            unreadable: Vec::new(),
            data_blobs_read: 0,
            index_files_read: 1,
        };

        let json: serde_json::Value = serde_json::from_str(&report_json(&report, false)).unwrap();
        assert_eq!(
            json["sessions"][0]["session_provenance"]["source_path_class"],
            "subagents"
        );
        assert_eq!(
            json["sessions"][0]["session_provenance"]["parent_session_ref"],
            "parent-fixture"
        );
        assert_eq!(
            json["sessions_not_placed"][0]["session_provenance"]["source_path_class"],
            "subagents"
        );
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
            snapshots_from_cache: 0,
            sessions_seen: 0,
            window: None,
            all_recall: BTreeMap::new(),
            hits: Vec::new(),
            unplaced: Vec::new(),
            not_matched: 0,
            machines_without_index: Vec::new(),
            machines_with_legacy_index: Vec::new(),
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

    /// A hit's project provenance must reach `search --json`, the surface the
    /// dashboard reads (ADR-043's "shown by the CLI / UI"). The capture-time fact and the
    /// later supplement travel as two fields, and a hit whose archive recorded
    /// no provenance carries **no key at all** — never a `null` one, which
    /// would collapse "we never wrote it down" into "no project".
    #[test]
    fn report_json_carries_project_provenance() {
        let mut known = hit("m-1", "chatgpt.m-1.aaaaaaaa-0000-0000-0000-000000000001", 5);
        known.provenance = Some(crate::activity::ProjectProvenance {
            captured: Some(serde_json::json!({
                "workspace": "unknown",
                "project": "unknown",
                "archived": false,
            })),
            effective_project: Some(serde_json::json!({
                "id": "project-fixture",
                "name": "Synthetic Project",
            })),
            supplement: Some(serde_json::json!({
                "workspace": "workspace-fixture",
                "project": {"id": "project-fixture", "name": "Synthetic Project"},
                "source": "project-list",
                "observedAt": "2026-09-25T12:00:00.000Z",
            })),
        });
        let report = SearchReport {
            destination: "d".into(),
            snapshots_in_repo: 1,
            snapshots_scanned: 1,
            snapshots_from_cache: 0,
            sessions_seen: 2,
            window: None,
            all_recall: BTreeMap::new(),
            hits: vec![
                known,
                hit("m-1", "chatgpt.m-1.aaaaaaaa-0000-0000-0000-000000000002", 6),
            ],
            unplaced: Vec::new(),
            not_matched: 0,
            machines_without_index: Vec::new(),
            machines_with_legacy_index: Vec::new(),
            hosts: Vec::new(),
            unreadable: Vec::new(),
            data_blobs_read: 0,
            index_files_read: 0,
        };
        let v: serde_json::Value = serde_json::from_str(&report_json(&report, false)).unwrap();
        let sessions = v["sessions"].as_array().unwrap();
        assert_eq!(
            sessions[0]["provenance"]["captured"]["project"], "unknown",
            "the capture-time fact must travel unchanged, marker included"
        );
        assert_eq!(
            sessions[0]["provenance"]["effectiveProject"]["name"], "Synthetic Project",
            "the later attribution is what a consumer shows as the project"
        );
        assert_eq!(
            sessions[0]["provenance"]["supplement"]["source"],
            "project-list"
        );
        assert_eq!(
            sessions[0]["provenance"]["supplement"]["observedAt"],
            "2026-09-25T12:00:00.000Z"
        );
        assert!(
            sessions[1].get("provenance").is_none(),
            "a hit with no provenance record carries no key at all, never null: {}",
            sessions[1]
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
            title: None,
            provenance: None,
            session_provenance: None,
            dimensions: Default::default(),
            account_keys: Vec::new(),
            measured_body: None,
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
            title: None,
            provenance: None,
            session_provenance: None,
            dimensions: Default::default(),
            account_keys: Vec::new(),
            measured_body: None,
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
            title: None,
            provenance: None,
            session_provenance: None,
            dimensions: Default::default(),
            account_keys: Vec::new(),
            measured_body: None,
        };
        let t = indexed_time(&row);
        assert_eq!(
            (t.first_unix, t.last_unix, t.why),
            (Some(10), Some(20), None)
        );
    }
}
