//! readback — cross-machine read-back merge (`read --all-machines`).
//!
//! Answers "what do I have archived, across every machine, right now". It
//! re-opens the repository, lists *all* snapshots, groups them by `hostname`,
//! and for the newest snapshot of each machine buckets its sealed shards by
//! the `sessions/<machine>/…` marker. Every session's shards are concatenated
//! in global sequence order and hashed, across bucket directories.
//!
//! Why "newest per hostname" and not "the newest snapshot":
//!
//! * Each backup moves one machine's *own* source tree — the globally newest
//!   snapshot therefore contains only the last machine that pushed. Naively
//!   reading the newest snapshot alone (conceptually `latest`) would return
//!   exactly half of the archive: the user's other machines would vanish.
//!   (Spike B6b measured this: the newest snapshot covers exactly one machine.)
//! * `sessions/<machine>/…` is the path partition fixed at push time, so no
//!   cross-checks about "who owns this path" are needed — the read side just
//!   buckets what is there. The only cross-machine information that has to be
//!   derived here is *which snapshot is newest per machine*.
//!
//! The "newest per hostname" step is not provided by rustic: the library
//! groups snapshots ([`SnapshotGroupCriterion`] / [`Grouped`]) but never
//! reduces a group to its latest member — the CLI does that itself in
//! `SnapshotFilter::post_process`. That reduction is the ~ten lines this
//! module implements ([`newest_snapshot_per_host`]).
//!
//! "Newest per hostname" is *not* the whole archive, though, and ADR-021 made
//! the readers that ask "what do I have archived" cumulative: `reclaim-stage`
//! deletes a session's bodies from the stage once every destination has proved
//! it holds them, so a busy machine's newest snapshot holds only the last
//! push's batch. Those readers walk every snapshot of a hostname newest-first
//! and let a session's **first** appearance win
//! ([`snapshots_by_host_newest_first`]) — one function, so `read
//! --all-machines`, `verify` L3, `search`, `read --session` and `export` cannot
//! disagree about where a session lives. `newest_snapshot_per_host` remains for
//! the questions that really are about the latest backup run, such as the
//! reclaim proof, whose candidates are all still on the stage.
//!
//! Tree layout reality (measured): a rustic dir-backup's tree mirrors the full
//! source path (`snapshot.paths` minus the leading `/`), so `sessions/` sits
//! at `stage-relative` depth and *not* at the tree root, and `repo.ls` on the
//! root returns a flattened recursive listing. `read_all_machines` therefore
//! buckets file paths by the *last* `sessions` path component
//! ([`bucket_shard_path`]) instead of walking level by level.
//!
//! Privacy line: this module only ever returns ids, shard counts, byte
//! lengths and sha256 digests. Session payload bytes are read, concatenated
//! and hashed in place — they are never printed, logged or returned.

use anyhow::Context;
use rustic_core::repofile::{MasterKey, Node, NodeType, SnapshotFile};
use rustic_core::{Grouped, LsOptions, SnapshotGroupCriterion};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::store::{BackupStore, SESSIONS_DIR, SHARD_SUFFIX};

/// One session merged back from the archive.
#[derive(Debug, Clone)]
pub struct SessionBackedUp {
    /// Machine partition the session was found under (`sessions/<machine>/`).
    pub machine: String,
    /// Session directory name (the native session id).
    pub session_id: String,
    /// Number of sealed shards concatenated, in sequence order.
    pub shard_count: usize,
    /// Byte length of the concatenated payload.
    pub concat_bytes: u64,
    /// sha256 of the concatenated payload.
    pub sha256: String,
    /// sha256 of each shard, in sequence order. Empty when the caller did not
    /// ask for the shard-level evidence (the shard bodies are only in hand
    /// while they are being dumped anyway, so this costs nothing there).
    ///
    /// W283: the three fields above cannot see a shard that was sealed twice —
    /// a doubled body is a *consistent* triple. These can, because a re-seal
    /// leaves a shard byte-identical to one already in the sequence.
    pub shard_sha256: Vec<String>,
    /// Byte length of each shard, same order as [`Self::shard_sha256`].
    pub shard_bytes: Vec<u64>,
    /// `(start, end)` shard-index ranges whose concatenated bytes equal a
    /// later single shard. `end` is exclusive and is the index of that later
    /// shard; only indices and digests leave readback, never payload bytes.
    pub shard_run_duplicates: Vec<(usize, usize)>,
}

impl SessionBackedUp {
    /// Re-derive the digest after flipping one byte — for the negative
    /// self-check: the same comparison the e2e driver runs must mismatch.
    pub fn sha_matches(&self, concat: &[u8]) -> bool {
        self.sha256 == hex_digest(&Sha256::digest(concat))
    }
}

/// The merged report for one machine: its newest snapshot + all sessions
/// walked from that snapshot.
#[derive(Debug, Clone)]
pub struct MachineMerge {
    /// Snapshot `hostname` (== path-partition machine name for our pushes).
    pub hostname: String,
    /// Full hex id of the newest snapshot for this hostname.
    pub snapshot_id: String,
    /// Human-readable snapshot time.
    pub snapshot_time: String,
    /// Unix seconds of the snapshot time (stable sort key / cost note).
    pub snapshot_time_unix: i64,
    /// One row per session found under `sessions/<machine>/`.
    pub sessions: Vec<SessionBackedUp>,
}

/// Full `read --all-machines` output.
#[derive(Debug, Default)]
pub struct ReadAllReport {
    /// How many snapshot files were read from `snapshots/`. `get_all_snapshots`
    /// loads every one of them — this equals the length of the snapshot list.
    pub snapshots_in_repo: usize,
    /// One [`MachineMerge`] per hostname seen in the repository.
    pub machines: Vec<MachineMerge>,
    /// Notes about parts of the archive this read could not reach (e.g. a
    /// snapshot whose tree root would not open). Non-fatal for the *rest* of
    /// the report, but each one is a machine whose sessions are missing from
    /// it — see [`ReadAllReport::complete`].
    pub warnings: Vec<String>,
}

impl ReadAllReport {
    /// Whether every snapshot this read picked was actually walked.
    ///
    /// Deliberately the same shape and the same meaning as
    /// [`crate::search::SearchReport::complete`]: `false` == the listing below
    /// is real but is not the whole archive, so its absences prove nothing.
    /// A `warning` here is never cosmetic — it is one hostname whose newest
    /// snapshot contributed an empty session list because it could not be
    /// opened, which is indistinguishable from "that machine backed nothing
    /// up" unless the caller is told.
    pub fn complete(&self) -> bool {
        self.warnings.is_empty()
    }
}

/// What one cumulative single-machine session walk read.
///
/// `snapshots_in_repo` counts every snapshot in the repository, not just this
/// machine's, so a caller can print "N of M scanned" the way `search` does:
/// a short scan and a complete one must not render the same.
#[derive(Debug, Default)]
pub struct CumulativeSessionRead {
    /// Sessions resolved and handed to the visitor, each exactly once.
    pub sessions: usize,
    /// This machine's snapshots walked, newest first. A walk that stops early
    /// on an unreadable snapshot returns `Err` instead of a smaller number.
    pub snapshots_scanned: usize,
    /// Snapshots the repository holds, across every machine.
    pub snapshots_in_repo: usize,
}

/// Newest snapshot per hostname group.
///
/// This is the make-or-break step: if we simply took the globally newest
/// snapshot we would only ever see the last machine that pushed. Groups come
/// from rustic's [`Grouped`]; picking the max `time` within each group is the
/// reduction rustic does not provide. [`SnapshotFile`]'s `Ord` compares `time`
/// only, so `max()` == newest by construction.
pub fn newest_snapshot_per_host(snaps: Vec<SnapshotFile>) -> Vec<SnapshotFile> {
    let grouped = Grouped::from_items(snaps, SnapshotGroupCriterion::new().hostname(true));
    let mut out = Vec::with_capacity(grouped.groups.len());
    for group in grouped.groups {
        if let Some(newest) = group.items.into_iter().max() {
            out.push(newest);
        }
    }
    out
}

/// Snapshots grouped by `hostname`, each group ordered **newest first**, and
/// the groups themselves ordered by hostname.
///
/// This is the traversal order ADR-021's cumulative readers use, and the order
/// is the whole rule: a cumulative reader walks one hostname's snapshots from
/// newest to oldest and keeps the **first** appearance of each session, so the
/// snapshot a session is reported against is the newest one that holds it.
///
/// One function rather than one per reader, because "which snapshot holds this
/// session" must have exactly one answer. `read --all-machines` / `verify` L3
/// ([`BackupStore::read_cumulative_sessions`]), `search` and `read --session`
/// all resolve it through here.
pub fn snapshots_by_host_newest_first(
    snaps: Vec<SnapshotFile>,
) -> Vec<(String, Vec<SnapshotFile>)> {
    let grouped = Grouped::from_items(snaps, SnapshotGroupCriterion::new().hostname(true));
    let mut out = Vec::with_capacity(grouped.groups.len());
    for mut group in grouped.groups {
        // `SnapshotFile`'s `Ord` compares `time` only, so this is "newest
        // first" by construction and cannot drift from `max()`.
        group.items.sort_by(|a, b| b.time.cmp(&a.time));
        let Some(newest) = group.items.first() else {
            continue;
        };
        out.push((newest.hostname.clone(), group.items));
    }
    out.sort_by(|(a, _), (b, _)| a.cmp(b));
    out
}

/// Sort a session dir's entries into sequence order.
///
/// Well-formed sealed shards are `NNNNNN.jsonl`, zero-padded to six digits, so
/// lexicographic order of the file name equals chronological order — that is
/// why names can be sorted without parsing sequence numbers (measured in spike
/// B4). Non-conforming names are kept (sorted after valid ones by name) rather
/// than silently dropped; the caller decides what to do with them.
pub fn sort_shards<T>(entries: &mut Vec<(PathBuf, T)>) {
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
}

/// Machine partitions' writer-version records, as read out of a destination.
///
/// `machines` holds every host that has a snapshot; `records` holds the ones
/// whose `meta/<machine>/writer.json` was read *and* names its own machine;
/// `unreadable` holds those whose record could not be read at all. The third
/// set exists so "the record is missing" (written by ≤0.3.0, which recorded
/// none) and "the record is there but unreadable" stay different answers —
/// [`crate::sidecar::writer_statuses`] renders them as `behind` and `unknown`.
#[derive(Debug, Default)]
pub struct ArchivedWriters {
    pub machines: BTreeSet<String>,
    pub records: BTreeMap<String, crate::sidecar::WriterVersionRecord>,
    pub unreadable: BTreeSet<String>,
}

impl BackupStore {
    /// Read every machine's `meta/<machine>/writer.json` from its newest
    /// snapshot, for the writer-version comparison `overview`, `status` and the
    /// stale-index check all make.
    ///
    /// The archived paths carry each machine's own absolute stage prefix, so
    /// the file is matched by its trailing `meta` / `<machine>` / `writer.json`
    /// marker ([`crate::sidecar::writer_machine`]) — never by a local path.
    pub fn read_archived_writers(&self, mk: &MasterKey) -> anyhow::Result<ArchivedWriters> {
        let backends = self.backends()?;
        let (repo, _adoption) = crate::orphans::open_adopting(&self.cfg, &backends, mk)
            .context("open repository for writer versions")?;
        self.require_sound_packs(&repo)?;
        let mut out = ArchivedWriters::default();
        for snapshot in newest_snapshot_per_host(repo.get_all_snapshots()?) {
            out.machines.insert(snapshot.hostname.clone());
            let root = repo.node_from_snapshot_and_path(&snapshot, "")?;
            let entries = repo
                .ls(&root, &LsOptions::default())?
                .collect::<rustic_core::RusticResult<Vec<_>>>()?;
            for (path, node) in entries {
                let Some(machine) = crate::sidecar::writer_machine(&path) else {
                    continue;
                };
                let mut bytes = Vec::new();
                repo.dump(&node, &mut bytes)?;
                match serde_json::from_slice::<crate::sidecar::WriterVersionRecord>(&bytes) {
                    // A record whose own `machine_id` disagrees with its path is
                    // not a version for either machine: it is unreadable.
                    Ok(record) if record.machine_id == machine => {
                        out.records.insert(machine, record);
                    }
                    _ => {
                        out.unreadable.insert(machine);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Read only the newest snapshot per machine.
    ///
    /// Used by `stagereclaim` to prove candidates that are still currently on stage.
    pub fn read_latest_per_machine(&self, mk: &MasterKey) -> anyhow::Result<ReadAllReport> {
        let backends = self.backends()?;
        let (repo, _adoption) = crate::orphans::open_adopting(&self.cfg, &backends, mk)
            .context("open repository for read-all")?;
        self.require_sound_packs(&repo)?;

        let snaps = repo.get_all_snapshots().context("list snapshots")?;
        // `get_all_snapshots` reads every snapshot file under `snapshots/` —
        // that is the (one and only) snapshot read cost of this command.
        let snapshots_in_repo = snaps.len();
        let newest = newest_snapshot_per_host(snaps);

        let mut report = ReadAllReport::default();
        report.snapshots_in_repo = snapshots_in_repo;

        let mut merges = Vec::new();
        for snap in newest {
            let hostname = snap.hostname.clone();
            let snapshot_id = snap.id.to_hex().as_str().to_string();
            let snapshot_time = snap.time.to_string();
            let snapshot_time_unix = snap.time.timestamp().as_second();

            let root = match repo.node_from_snapshot_and_path(&snap, "") {
                Ok(n) => n,
                Err(e) => {
                    report.warnings.push(format!(
                        "host `{hostname}` snapshot {snapshot_id}: cannot read tree root: {e}"
                    ));
                    merges.push(MachineMerge {
                        hostname,
                        snapshot_id,
                        snapshot_time,
                        snapshot_time_unix,
                        sessions: Vec::new(),
                    });
                    continue;
                }
            };
            let entries = repo
                .ls(&root, &LsOptions::default())
                .context("ls snapshot root")?
                .collect::<rustic_core::RusticResult<Vec<_>>>()
                .context("collect snapshot entries")?;

            let mut buckets: BTreeMap<(String, String), Vec<(String, usize)>> = BTreeMap::new();
            for (i, (path, node)) in entries.iter().enumerate() {
                if node.node_type != NodeType::File {
                    continue;
                }
                if let Some((machine, session, shard)) = bucket_shard_path(path) {
                    buckets
                        .entry((machine, session))
                        .or_default()
                        .push((shard, i));
                }
            }

            let mut sessions = Vec::new();
            for ((machine, session_id), mut shards) in buckets {
                // Bucket names are not the ordering key: the six-digit shard
                // sequence is global across all buckets.
                shards.sort_by_key(|(n, _)| store_seq_of(n));
                let mut concat = Vec::new();
                let mut shard_sha256 = Vec::with_capacity(shards.len());
                let mut shard_bytes = Vec::with_capacity(shards.len());
                let mut shard_run_duplicates = Vec::new();
                let mut shard_offsets: Vec<usize> = Vec::with_capacity(shards.len());
                for (shard, idx) in &shards {
                    let mut buf = Vec::new();
                    repo.dump(&entries[*idx].1, &mut buf)
                        .with_context(|| format!("dump shard {shard}"))?;
                    let current_index = shard_sha256.len();
                    for (start, offset) in shard_offsets.iter().copied().enumerate() {
                        let Some(end_offset) = offset.checked_add(buf.len()) else {
                            continue;
                        };
                        let ends_at_shard_boundary = end_offset == concat.len()
                            || shard_offsets.binary_search(&end_offset).is_ok();
                        if ends_at_shard_boundary
                            && concat.get(offset..end_offset) == Some(buf.as_slice())
                        {
                            shard_run_duplicates.push((start, current_index));
                        }
                    }
                    shard_offsets.push(concat.len());
                    shard_sha256.push(hex_digest(&Sha256::digest(&buf)));
                    shard_bytes.push(buf.len() as u64);
                    concat.extend_from_slice(&buf);
                }
                sessions.push(SessionBackedUp {
                    machine,
                    session_id,
                    shard_count: shards.len(),
                    concat_bytes: concat.len() as u64,
                    sha256: hex_digest(&Sha256::digest(&concat)),
                    shard_sha256,
                    shard_bytes,
                    shard_run_duplicates,
                });
            }

            merges.push(MachineMerge {
                hostname,
                snapshot_id,
                snapshot_time,
                snapshot_time_unix,
                sessions,
            });
        }

        merges.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        report.machines = merges;
        Ok(report)
    }

    /// Cumulative cross-machine read-back merge (`read --all-machines` and `verify` L3).
    ///
    /// Answers "what do all snapshots hold across machines, cumulatively" (ADR-021).
    /// Groups snapshots by hostname, traverses snapshots from newest to oldest reading
    /// tree metadata, and maps each session to the newest snapshot that contains it.
    /// Only the resolved sessions (filtered by `wanted` if specified) have their data
    /// blobs downloaded and concatenated into verification triples.
    pub fn read_cumulative_sessions(
        &self,
        mk: &MasterKey,
        wanted: Option<&BTreeSet<(String, String)>>,
    ) -> anyhow::Result<ReadAllReport> {
        self.read_cumulative_sessions_inner(mk, wanted, false)
    }

    /// Raw cumulative bytes for L3 reconciliation and duplicate inventory.
    /// User-facing bulk reads use [`Self::read_all_machines`] so a
    /// whole-prefix reseal cannot duplicate conversation content.
    pub fn read_cumulative_sessions_raw(
        &self,
        mk: &MasterKey,
        wanted: Option<&BTreeSet<(String, String)>>,
    ) -> anyhow::Result<ReadAllReport> {
        self.read_cumulative_sessions_inner(mk, wanted, false)
    }

    fn read_cumulative_sessions_inner(
        &self,
        mk: &MasterKey,
        wanted: Option<&BTreeSet<(String, String)>>,
        collapse_duplicate_shards: bool,
    ) -> anyhow::Result<ReadAllReport> {
        let backends = self.backends()?;
        let (repo, _adoption) = crate::orphans::open_adopting(&self.cfg, &backends, mk)
            .context("open repository for read-all")?;
        self.require_sound_packs(&repo)?;

        let snaps = repo.get_all_snapshots().context("list snapshots")?;
        let snapshots_in_repo = snaps.len();

        let mut report = ReadAllReport::default();
        report.snapshots_in_repo = snapshots_in_repo;

        let mut merges = Vec::new();
        for (_host, snaps) in snapshots_by_host_newest_first(snaps) {
            let newest_snap = &snaps[0];
            let hostname = newest_snap.hostname.clone();
            let snapshot_id = newest_snap.id.to_hex().as_str().to_string();
            let snapshot_time = newest_snap.time.to_string();
            let snapshot_time_unix = newest_snap.time.timestamp().as_second();

            let mut sessions_map: BTreeMap<String, Vec<(String, rustic_core::repofile::Node)>> =
                BTreeMap::new();

            for snap in &snaps {
                if let Some(wanted_set) = wanted {
                    let wanted_for_this_machine: BTreeSet<&str> = wanted_set
                        .iter()
                        .filter(|(m, _)| m == &hostname)
                        .map(|(_, s)| s.as_str())
                        .collect();
                    if !wanted_for_this_machine.is_empty()
                        && wanted_for_this_machine
                            .iter()
                            .all(|s| sessions_map.contains_key(*s))
                    {
                        break;
                    }
                }

                let root = match repo.node_from_snapshot_and_path(snap, "") {
                    Ok(n) => n,
                    Err(e) => {
                        report.warnings.push(format!(
                            "host `{hostname}` snapshot {}: cannot read tree root: {e}",
                            snap.id.to_hex().as_str()
                        ));
                        continue;
                    }
                };

                let entries = match repo.ls(&root, &LsOptions::default()) {
                    Ok(it) => match it.collect::<rustic_core::RusticResult<Vec<_>>>() {
                        Ok(e) => e,
                        Err(e) => {
                            report.warnings.push(format!(
                                "host `{hostname}` snapshot {}: cannot collect entries: {e}",
                                snap.id.to_hex().as_str()
                            ));
                            continue;
                        }
                    },
                    Err(e) => {
                        report.warnings.push(format!(
                            "host `{hostname}` snapshot {}: cannot ls tree: {e}",
                            snap.id.to_hex().as_str()
                        ));
                        continue;
                    }
                };

                let mut snap_sessions: BTreeMap<
                    String,
                    Vec<(String, rustic_core::repofile::Node)>,
                > = BTreeMap::new();
                for (path, node) in entries {
                    if node.node_type != NodeType::File {
                        continue;
                    }
                    if let Some((m, session, shard)) = bucket_shard_path(&path) {
                        if m == hostname {
                            if let Some(wanted_set) = wanted {
                                if !wanted_set.contains(&(hostname.clone(), session.clone())) {
                                    continue;
                                }
                            }
                            if !sessions_map.contains_key(&session) {
                                snap_sessions
                                    .entry(session)
                                    .or_default()
                                    .push((shard, node));
                            }
                        }
                    }
                }

                for (session, shards) in snap_sessions {
                    sessions_map.entry(session).or_insert(shards);
                }
            }

            let mut sessions = Vec::new();
            for (session_id, mut shards) in sessions_map {
                shards.sort_by_key(|(n, _)| store_seq_of(n));
                let mut concat = Vec::new();
                let mut shard_sha256 = Vec::with_capacity(shards.len());
                let mut shard_bytes = Vec::with_capacity(shards.len());
                let mut shard_run_duplicates = Vec::new();
                let mut shard_offsets: Vec<usize> = Vec::with_capacity(shards.len());
                let mut shard_bodies = Vec::with_capacity(shards.len());
                for (shard, node) in &shards {
                    let mut buf = Vec::new();
                    repo.dump(node, &mut buf)
                        .with_context(|| format!("dump shard {shard}"))?;
                    shard_bodies.push(buf);
                }
                let duplicate_indices: BTreeSet<_> = if collapse_duplicate_shards {
                    crate::store::duplicate_shard_indices(&shard_bodies)
                        .into_iter()
                        .collect()
                } else {
                    BTreeSet::new()
                };
                for ((_shard, _node), (index, buf)) in
                    shards.iter().zip(shard_bodies.iter().enumerate())
                {
                    let digest: [u8; 32] = Sha256::digest(&buf).into();
                    if duplicate_indices.contains(&index) {
                        continue;
                    }
                    let current_index = shard_sha256.len();
                    for (start, offset) in shard_offsets.iter().copied().enumerate() {
                        let Some(end_offset) = offset.checked_add(buf.len()) else {
                            continue;
                        };
                        let ends_at_shard_boundary = end_offset == concat.len()
                            || shard_offsets.binary_search(&end_offset).is_ok();
                        if ends_at_shard_boundary
                            && concat.get(offset..end_offset) == Some(buf.as_slice())
                        {
                            shard_run_duplicates.push((start, current_index));
                        }
                    }
                    shard_offsets.push(concat.len());
                    shard_sha256.push(hex_digest(&digest));
                    shard_bytes.push(buf.len() as u64);
                    concat.extend_from_slice(&buf);
                }
                sessions.push(SessionBackedUp {
                    machine: hostname.clone(),
                    session_id,
                    shard_count: shards.len(),
                    concat_bytes: concat.len() as u64,
                    sha256: hex_digest(&Sha256::digest(&concat)),
                    shard_sha256,
                    shard_bytes,
                    shard_run_duplicates,
                });
            }

            merges.push(MachineMerge {
                hostname,
                snapshot_id,
                snapshot_time,
                snapshot_time_unix,
                sessions,
            });
        }

        merges.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        report.machines = merges;
        Ok(report)
    }

    /// Cross-machine read-back merge (`read --all-machines`).
    ///
    /// Answers "what do I have archived, across every machine, right now".
    /// Traverses all snapshots cumulatively, grouping sessions by their newest
    /// snapshot appearance (ADR-021).
    pub fn read_all_machines(&self, mk: &MasterKey) -> anyhow::Result<ReadAllReport> {
        self.read_cumulative_sessions_inner(mk, None, true)
    }

    /// Dump the individual sealed shards of selected sessions of one machine
    /// partition, in global sequence order, from that machine's newest
    /// snapshot.
    ///
    /// [`Self::read_all_machines`] answers "what is archived" and deliberately
    /// only returns digests. This answers "hand me those bytes back", which is
    /// what the ADR-013 difference-set restore needs; shards are kept separate
    /// (not concatenated) so the restored stage reproduces the archived shard
    /// *set*, not just its concatenation.
    ///
    /// A session in `wanted` that the newest snapshot does not hold is simply
    /// absent from the result — the caller must treat "asked for, not
    /// returned" as a failure to copy, never as "there was nothing to copy".
    pub fn dump_machine_sessions(
        &self,
        mk: &MasterKey,
        machine: &str,
        wanted: &std::collections::BTreeSet<String>,
    ) -> anyhow::Result<BTreeMap<String, Vec<Vec<u8>>>> {
        let mut out: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();
        if wanted.is_empty() {
            return Ok(out);
        }
        let backends = self.backends()?;
        let (repo, _adoption) = crate::orphans::open_adopting(&self.cfg, &backends, mk)
            .context("open repository for shard restore")?;
        self.require_sound_packs(&repo)?;
        let snaps = repo.get_all_snapshots().context("list snapshots")?;
        for snap in newest_snapshot_per_host(snaps) {
            if snap.hostname != machine {
                continue;
            }
            let root = repo
                .node_from_snapshot_and_path(&snap, "")
                .context("read snapshot root for shard restore")?;
            let entries = repo
                .ls(&root, &LsOptions::default())
                .context("ls snapshot root for shard restore")?
                .collect::<rustic_core::RusticResult<Vec<_>>>()
                .context("collect snapshot entries for shard restore")?;
            let mut buckets: BTreeMap<String, Vec<(u64, usize)>> = BTreeMap::new();
            for (i, (path, node)) in entries.iter().enumerate() {
                if node.node_type != NodeType::File {
                    continue;
                }
                let Some((found_machine, session, shard)) = bucket_shard_path(path) else {
                    continue;
                };
                if found_machine != machine || !wanted.contains(&session) {
                    continue;
                }
                buckets
                    .entry(session)
                    .or_default()
                    .push((store_seq_of(&shard), i));
            }
            for (session, mut shards) in buckets {
                shards.sort_by_key(|(seq, _)| *seq);
                let mut bytes = Vec::with_capacity(shards.len());
                for (_, idx) in &shards {
                    let mut buf = Vec::new();
                    repo.dump(&entries[*idx].1, &mut buf)
                        .context("dump shard for restore")?;
                    bytes.push(buf);
                }
                out.insert(session, bytes);
            }
        }
        Ok(out)
    }

    /// Walk **one** machine's snapshots newest-first and hand every session's
    /// concatenated shard body to `visit`, one session at a time.
    ///
    /// The resolution rule is ADR-021's, through the same
    /// [`snapshots_by_host_newest_first`] resolver every other cumulative
    /// reader uses: a session's **first** appearance wins, so the body handed
    /// back is the newest one the archive holds, and it is assembled from that
    /// snapshot's shards in global sequence order — byte for byte the rule
    /// [`BackupStore::read_cumulative_sessions`] hashes by, so an index built
    /// from this walk and a digest computed from that one describe the same
    /// bytes.
    ///
    /// This is the walk a reader needs when the boundary is *not* "which
    /// sessions are here" but "hand me each of them": the payload is delivered
    /// through the callback rather than returned, because a machine's archived
    /// bodies are gigabytes and nothing here may accumulate them. `visit` owns
    /// whatever it makes of the bytes; they are not retained by this function.
    ///
    /// One machine rather than every machine, because the question this
    /// answers — reconstruct a partition from the archive alone, without that
    /// machine's stage — is per partition, and a caller that wants a second one
    /// asks for it by name.
    ///
    /// A snapshot that cannot be walked is an `Err` naming it, never a skip:
    /// continuing past it would answer a smaller question while rendering as a
    /// complete answer (invariant 2 — exit 3, not a short count). The same
    /// holds when the machine has no snapshots at all: "this repository does
    /// not hold that machine" is an error, not an empty result.
    pub fn for_each_archived_session<F>(
        &self,
        mk: &MasterKey,
        machine: &str,
        mut visit: F,
    ) -> anyhow::Result<CumulativeSessionRead>
    where
        F: FnMut(&str, &[u8]) -> anyhow::Result<()>,
    {
        let backends = self.backends()?;
        let (repo, _adoption) = crate::orphans::open_adopting(&self.cfg, &backends, mk)
            .context("open repository for archived sessions")?;
        self.require_sound_packs(&repo)?;
        let snaps = repo.get_all_snapshots().context("list snapshots")?;
        let snapshots_in_repo = snaps.len();
        let Some(host_snaps) = snapshots_by_host_newest_first(snaps)
            .into_iter()
            .find(|(host, _)| host == machine)
            .map(|(_, snaps)| snaps)
        else {
            anyhow::bail!("no snapshot for machine `{machine}` in this repository");
        };

        let mut out = CumulativeSessionRead {
            sessions: 0,
            snapshots_scanned: 0,
            snapshots_in_repo,
        };
        // Sessions already claimed by a newer snapshot. Kept across snapshots so
        // an older copy can never overwrite the newer one (ADR-021).
        let mut resolved: BTreeSet<String> = BTreeSet::new();
        for snap in &host_snaps {
            let snap_id = snap.id.to_hex().as_str().to_string();
            let short = &snap_id[..8.min(snap_id.len())];
            let root = repo
                .node_from_snapshot_and_path(snap, "")
                .with_context(|| format!("snapshot {short}: cannot read tree root"))?;
            let entries = repo
                .ls(&root, &LsOptions::default())
                .and_then(|iter| iter.collect::<rustic_core::RusticResult<Vec<_>>>())
                .with_context(|| format!("snapshot {short}: cannot list tree"))?;
            out.snapshots_scanned += 1;

            let mut held: BTreeMap<String, Vec<(u64, &Node)>> = BTreeMap::new();
            for (path, node) in &entries {
                if node.node_type != NodeType::File {
                    continue;
                }
                let Some((found_machine, session, shard)) = bucket_shard_path(path) else {
                    continue;
                };
                if found_machine != machine || resolved.contains(&session) {
                    continue;
                }
                held.entry(session)
                    .or_default()
                    .push((store_seq_of(&shard), node));
            }

            for (session, mut shards) in held {
                if !resolved.insert(session.clone()) {
                    continue;
                }
                shards.sort_by_key(|(seq, _)| *seq);
                let mut shard_bodies = Vec::with_capacity(shards.len());
                for (_, node) in shards {
                    let mut buf = Vec::new();
                    repo.dump(node, &mut buf).with_context(|| {
                        format!(
                            "snapshot {short}: cannot read a shard of session `{}`",
                            crate::id::short_session_id(&session)
                        )
                    })?;
                    shard_bodies.push(buf);
                }
                let concat = crate::store::unique_shard_bodies(shard_bodies)
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                visit(&session, &concat)?;
                out.sessions += 1;
            }
        }
        Ok(out)
    }
}

fn store_seq_of(name: &str) -> u64 {
    crate::store::parse_shard_seq(name).unwrap_or(u64::MAX)
}

/// Bucket one file path into `(machine, session, shard-name)` if its trailing
/// components are either the legacy
/// `…/sessions/<machine>/<session>/<shard>.jsonl` or the bucketed
/// `…/sessions/<machine>/<session>/<bucket>/<shard>.jsonl` layout.
///
/// The `sessions` marker is *anywhere* in the path (the stage prefix differs
/// per machine and even per host), so we take the last component equal to
/// `sessions` and require exactly three or four more components after it. The
/// bucket component is intentionally ignored because sequence is global.
pub fn bucket_shard_path(path: &Path) -> Option<(String, String, String)> {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let mut marker = None;
    for (i, c) in comps.iter().enumerate() {
        if *c == SESSIONS_DIR {
            marker = Some(i);
        }
    }
    let i = marker?;
    let rest = &comps[i + 1..];
    if rest.len() != 3 && rest.len() != 4 {
        return None;
    }
    let shard = rest.last()?;
    if !shard.ends_with(SHARD_SUFFIX) {
        return None;
    }
    Some((
        rest[0].to_string(),
        rest[1].to_string(),
        (*shard).to_string(),
    ))
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sort_shards` orders a session dir's names in sequence order even when
    /// the underlying Vec arrives unsorted (ls order is not guaranteed).
    #[test]
    fn shard_names_sort_into_sequence_order() {
        let mut entries: Vec<(PathBuf, u8)> = vec![
            (PathBuf::from("000003.jsonl"), 0),
            (PathBuf::from("000001.jsonl"), 0),
            (PathBuf::from("000002.jsonl"), 0),
            (PathBuf::from("000010.jsonl"), 0),
        ];
        sort_shards(&mut entries);
        let names: Vec<String> = entries
            .into_iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "000001.jsonl",
                "000002.jsonl",
                "000003.jsonl",
                "000010.jsonl"
            ]
        );
    }

    /// Non-conforming names survive the sort (they follow valid ones) instead
    /// of being dropped — the caller decides their fate.
    #[test]
    fn non_shard_names_stay_sorted_not_dropped() {
        let mut entries: Vec<(PathBuf, u8)> = vec![
            (PathBuf::from("zzz-unknown"), 0),
            (PathBuf::from("000002.jsonl"), 0),
        ];
        sort_shards(&mut entries);
        let names: Vec<String> = entries
            .into_iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["000002.jsonl", "zzz-unknown"]);
    }

    #[test]
    fn bucket_path_parser_accepts_legacy_and_bucketed_layouts() {
        assert_eq!(
            bucket_shard_path(Path::new("/stage/sessions/m/s/000001.jsonl")),
            Some(("m".into(), "s".into(), "000001.jsonl".into()))
        );
        assert_eq!(
            bucket_shard_path(Path::new("/stage/sessions/m/s/007/000141.jsonl")),
            Some(("m".into(), "s".into(), "000141.jsonl".into()))
        );
        assert_eq!(
            bucket_shard_path(Path::new("/stage/sessions/m/s/007/deeper/000141.jsonl")),
            None
        );
    }

    /// Negative self-check unit: a one-byte flip in a payload must change the
    /// digest, so the e2e comparison really is able to report a mismatch.
    #[test]
    fn one_byte_flip_changes_digest() {
        let mut payload = b"hello world, this is a sealed shard payload".to_vec();
        let original = hex_digest(&Sha256::digest(&payload));
        payload[3] ^= 0x01;
        let flipped = hex_digest(&Sha256::digest(&payload));
        assert_ne!(original, flipped);
    }

    /// Direct shape check of [`SessionBackedUp::sha_matches`] — the exact
    /// comparison used by the e2e driver's negative self-check.
    #[test]
    fn sha_matches_rejects_flipped_bytes() {
        let payload = b"aaaaaaaaaaaaaaaaaaaa".to_vec();
        let good = SessionBackedUp {
            machine: "m".into(),
            session_id: "s".into(),
            shard_count: 1,
            concat_bytes: payload.len() as u64,
            sha256: hex_digest(&Sha256::digest(&payload)),
            shard_sha256: vec![hex_digest(&Sha256::digest(&payload))],
            shard_bytes: vec![payload.len() as u64],
            shard_run_duplicates: Vec::new(),
        };
        assert!(good.sha_matches(&payload));
        let mut bad = payload.clone();
        bad[7] ^= 0x02;
        assert!(!good.sha_matches(&bad));
    }
}
