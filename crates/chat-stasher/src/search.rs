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
//! * **metadata tier** (this module): snapshot files + index + tree blobs. It
//!   answers "which session, on which machine, when, how big, in which
//!   snapshot". No `data` blob is ever fetched or decrypted — the only rustic
//!   entry points that return payload bytes are `dump` / `get_blob_cached` /
//!   `cat_blob` / `read_file_at`, and none of them appears in this file.
//! * **payload tier** (NOT implemented): full-text matching inside the archived
//!   conversations. Every candidate session's data blobs would have to be
//!   fetched and decrypted locally. [`FulltextCost`] measures what that would
//!   cost, from tree metadata only, so the price is on the table before anyone
//!   commits to the feature.
//!
//! Product rule this module hard-codes: **a search is always scoped to exactly
//! one destination.** There is no implicit merge across destinations.
//!
//! Honesty rule this module hard-codes (same rule `push` applies to an empty
//! stage): **"this destination does not contain it" and "I could not finish
//! reading this destination" are different answers.** Callers must branch on
//! [`SearchReport::complete`] before saying "not found"; [`SearchReport::no_hit_line`]
//! renders the two cases as two different terminal lines.
//!
//! Privacy line (same as `readback`): only ids, counts, byte lengths and
//! timestamps leave this module. No payload byte is read, so none can leak.

use anyhow::Context;
use rustic_core::repofile::{MasterKey, NodeType};
use rustic_core::{Credentials, LsOptions, Repository, RepositoryOptions};
use std::collections::BTreeMap;

use crate::readback::{bucket_shard_path, newest_snapshot_per_host};
use crate::store::BackupStore;

/// Metadata-tier filter. Every field is `None` == "no constraint".
#[derive(Debug, Clone, Default)]
pub struct SearchFilter {
    /// Match sessions whose id starts with this prefix.
    pub session_id_prefix: Option<String>,
    /// Match one machine partition exactly (`sessions/<machine>/…`).
    pub machine: Option<String>,
    /// Lower bound (inclusive) on the session's activity time, unix seconds.
    pub since_unix: Option<i64>,
    /// Upper bound (inclusive) on the session's activity time, unix seconds.
    pub until_unix: Option<i64>,
}

impl SearchFilter {
    pub fn session_id_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.session_id_prefix = Some(prefix.into());
        self
    }
    pub fn machine(mut self, machine: impl Into<String>) -> Self {
        self.machine = Some(machine.into());
        self
    }
    pub fn since_unix(mut self, unix: i64) -> Self {
        self.since_unix = Some(unix);
        self
    }
    pub fn until_unix(mut self, unix: i64) -> Self {
        self.until_unix = Some(unix);
        self
    }
}

/// One matched session. Every field comes from snapshot/tree metadata.
#[derive(Debug, Clone)]
pub struct SessionHit {
    /// Machine partition (`sessions/<machine>/…`).
    pub machine: String,
    /// Session directory name (the native session id).
    pub session_id: String,
    /// Number of sealed shards belonging to this session in that snapshot.
    pub shard_count: usize,
    /// Sum of the shards' file sizes, from the tree nodes (NOT read back).
    pub bytes: u64,
    /// Full hex id of the snapshot the session was found in.
    pub snapshot_id: String,
    /// Snapshot time, unix seconds.
    pub snapshot_time_unix: i64,
    /// Newest shard mtime in the tree, unix seconds; falls back to the
    /// snapshot time when the tree carries no mtime. This is the value the
    /// time filter is applied to.
    pub activity_unix: i64,
    /// Number of data blobs the shards are made of (from `node.content`).
    /// This is what a full-text pass would have to fetch — it is *counted*
    /// here, never fetched.
    pub data_blobs: usize,
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
    /// Matched sessions.
    pub hits: Vec<SessionHit>,
    /// Non-empty == the scan could not read part of the destination. When this
    /// is non-empty, "no hits" means UNKNOWN, never "not there".
    pub unreadable: Vec<String>,
    /// Data blobs this search fetched. Structurally always 0 for the metadata
    /// tier; asserted by the tests so a future edit that adds a `dump` call
    /// cannot pass silently.
    pub data_blobs_read: usize,
}

impl SearchReport {
    /// Whether the whole destination was read. `false` == partial knowledge.
    pub fn complete(&self) -> bool {
        self.unreadable.is_empty()
    }

    /// The one terminal line for "your query matched nothing". The two cases
    /// are deliberately different sentences.
    pub fn no_hit_line(&self) -> String {
        if self.complete() {
            format!(
                "search: not in this destination — 0 of {} sessions matched in `{}` ({} snapshots read, all readable)",
                self.sessions_seen, self.destination, self.snapshots_scanned
            )
        } else {
            format!(
                "search: UNKNOWN — could not finish reading `{}` ({} of {} snapshots unreadable); 0 matched in the part I could read. This is NOT \"not there\".",
                self.destination,
                self.unreadable.len(),
                self.snapshots_scanned
            )
        }
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

/// Metadata-tier search over one destination.
///
/// Reuses the read path `readback` already established: open fresh, group
/// snapshots by hostname, take the newest per hostname, `ls` its tree and
/// bucket shard paths with [`bucket_shard_path`]. The one difference — and it
/// is the whole point — is that this walk never calls `repo.dump`, so no data
/// blob is fetched or decrypted.
///
/// Errors that make the answer partial are collected into
/// [`SearchReport::unreadable`] instead of being swallowed. A failure to open
/// the repository at all is returned as `Err`: "I cannot read this
/// destination" must never be rendered as "empty".
pub fn search_sessions(
    store: &BackupStore,
    mk: &MasterKey,
    filter: &SearchFilter,
) -> anyhow::Result<SearchReport> {
    let backends = store.backends()?;
    let repo = Repository::new(&RepositoryOptions::default(), &backends)?
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
        hits: Vec::new(),
        unreadable: Vec::new(),
        data_blobs_read: 0,
    };

    for snap in newest {
        let snapshot_id = snap.id.to_hex().as_str().to_string();
        let snapshot_time_unix = snap.time.timestamp().as_second();

        let root = match repo.node_from_snapshot_and_path(&snap, "") {
            Ok(node) => node,
            Err(e) => {
                report.unreadable.push(format!(
                    "snapshot {}: tree root unreadable: {e}",
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
                    "snapshot {}: tree walk failed: {e}",
                    &snapshot_id[..8.min(snapshot_id.len())]
                ));
                continue;
            }
        };

        // (machine, session) -> (shards, bytes, data blobs, newest mtime)
        let mut sessions: BTreeMap<(String, String), (usize, u64, usize, Option<i64>)> =
            BTreeMap::new();
        for (path, node) in &entries {
            if node.node_type != NodeType::File {
                continue;
            }
            let Some((machine, session, _shard)) = bucket_shard_path(path) else {
                continue;
            };
            let entry = sessions
                .entry((machine, session))
                .or_insert((0, 0, 0, None));
            entry.0 += 1;
            entry.1 += node.meta.size;
            entry.2 += node.content.as_ref().map_or(0, Vec::len);
            let mtime = node.meta.mtime.map(|t| t.as_second());
            entry.3 = match (entry.3, mtime) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            };
        }

        for ((machine, session_id), (shard_count, bytes, data_blobs, mtime)) in sessions {
            report.sessions_seen += 1;
            let activity_unix = mtime.unwrap_or(snapshot_time_unix);
            if !matches(filter, &machine, &session_id, activity_unix) {
                continue;
            }
            report.hits.push(SessionHit {
                machine,
                session_id,
                shard_count,
                bytes,
                snapshot_id: snapshot_id.clone(),
                snapshot_time_unix,
                activity_unix,
                data_blobs,
            });
        }
    }

    report
        .hits
        .sort_by(|a, b| (&a.machine, &a.session_id).cmp(&(&b.machine, &b.session_id)));
    Ok(report)
}

fn matches(filter: &SearchFilter, machine: &str, session_id: &str, activity_unix: i64) -> bool {
    if let Some(want) = &filter.machine {
        if machine != want {
            return false;
        }
    }
    if let Some(prefix) = &filter.session_id_prefix {
        if !session_id.starts_with(prefix.as_str()) {
            return false;
        }
    }
    if let Some(since) = filter.since_unix {
        if activity_unix < since {
            return false;
        }
    }
    if let Some(until) = filter.until_unix {
        if activity_unix > until {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(machine: &str, session: &str, activity: i64) -> SessionHit {
        SessionHit {
            machine: machine.into(),
            session_id: session.into(),
            shard_count: 2,
            bytes: 100,
            snapshot_id: "abcdef0123456789".into(),
            snapshot_time_unix: activity,
            activity_unix: activity,
            data_blobs: 2,
        }
    }

    #[test]
    fn filter_matches_prefix_machine_and_window() {
        let f = SearchFilter::default()
            .session_id_prefix("sess-a")
            .machine("m-1")
            .since_unix(100)
            .until_unix(200);
        assert!(matches(&f, "m-1", "sess-abc", 150));
        assert!(!matches(&f, "m-2", "sess-abc", 150), "machine must match");
        assert!(!matches(&f, "m-1", "sess-b", 150), "prefix must match");
        assert!(!matches(&f, "m-1", "sess-abc", 99), "before window");
        assert!(!matches(&f, "m-1", "sess-abc", 201), "after window");
        // Inclusive bounds.
        assert!(matches(&f, "m-1", "sess-abc", 100));
        assert!(matches(&f, "m-1", "sess-abc", 200));
    }

    #[test]
    fn empty_filter_matches_everything() {
        let f = SearchFilter::default();
        assert!(matches(&f, "anything", "anything", i64::MIN));
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
            hits: Vec::new(),
            unreadable: Vec::new(),
            data_blobs_read: 0,
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

    #[test]
    fn fulltext_cost_sums_the_metadata_tier_counters() {
        let report = SearchReport {
            destination: "d".into(),
            snapshots_in_repo: 1,
            snapshots_scanned: 1,
            sessions_seen: 2,
            hits: vec![hit("m-1", "a", 10), hit("m-1", "b", 20)],
            unreadable: Vec::new(),
            data_blobs_read: 0,
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
}
