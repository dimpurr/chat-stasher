//! readback — cross-machine read-back merge (`read --all-machines`).
//!
//! Answers "what do I have archived, across every machine, right now". It
//! re-opens the repository, lists *all* snapshots, groups them by `hostname`,
//! and walks each machine's snapshots newest-first. For each session, the
//! newest copy of each shard sequence wins while older sequences absent from
//! later snapshots are retained. The resulting shards are concatenated in
//! global sequence order and hashed, across bucket directories.
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
//!   buckets what is there. The cross-machine information derived here is
//!   *which snapshots belong to each machine and which copy of a shard is
//!   newest*.
//!
//! Snapshot grouping and newest-first traversal are provided by
//! [`snapshots_by_host_newest_first`]. [`newest_snapshot_per_host`] remains
//! available for commands that specifically need only the latest run.
//!
//! "Newest per hostname" is *not* the whole archive, though, and ADR-021 made
//! the readers that ask "what do I have archived" cumulative: `reclaim-stage`
//! deletes a session's bodies from the stage once every destination has proved
//! it holds them, so a busy machine's newest snapshot holds only the last
//! push's batch. Those readers walk every snapshot of a hostname newest-first
//! and let each shard sequence's **newest** copy win while retaining older
//! sequences absent from later snapshots ([`snapshots_by_host_newest_first`])
//! — one function, so `read
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
use rustic_core::{Grouped, IndexedFullStatus, LsOptions, Repository, SnapshotGroupCriterion};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::store::{
    BackupStore, DuplicateShardPolicy, MACHINE_LOGS_DIR, SESSIONS_DIR, SHARD_SUFFIX,
};

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
    /// Parsed global sequence numbers aligned with `shard_sha256` and
    /// `shard_bytes`; `None` marks a retained noncanonical legacy filename.
    pub shard_sequences: Vec<Option<u64>>,
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

/// One machine log's sealed generations as the archive holds them
/// (ADR-053 D4).
///
/// Deliberately **not** a [`SessionBackedUp`] and deliberately not inside
/// [`MachineMerge::sessions`]: a machine log has no session id (D1) and must
/// never be visible to anything that counts, lists or reports sessions. What
/// it is for is cursor verification: "does the archive hold the generations
/// this cursor claims", asked exactly like it is asked for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineLogBackedUp {
    /// Machine partition the log belongs to (`machine-logs/<machine>/`).
    pub machine: String,
    /// Registry harness id that declares the log (`claude-code`).
    pub harness: String,
    /// The log's own declared id (`history`).
    pub log_id: String,
    /// Number of sealed generations found, in sequence order.
    pub shard_count: usize,
    /// Byte length of the concatenated generations.
    pub concat_bytes: u64,
    /// sha256 of the concatenated generations.
    pub sha256: String,
    /// sha256 of each generation, in sequence order.
    pub shard_sha256: Vec<String>,
    /// Parsed global sequence numbers aligned with `shard_sha256`.
    pub shard_sequences: Vec<Option<u64>>,
    /// Byte length of each generation, same order.
    pub shard_bytes: Vec<u64>,
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
    /// Hosts with at least one snapshot that could not be read. Their
    /// accumulated sessions may come from an older snapshot and must not be
    /// presented as a complete current inventory.
    pub incomplete_machines: BTreeSet<String>,
    /// Notes about parts of the archive this read could not reach (e.g. a
    /// snapshot whose tree root would not open). Non-fatal for the *rest* of
    /// the report, but each one is a machine whose sessions are missing from
    /// it — see [`ReadAllReport::complete`].
    pub warnings: Vec<String>,
    /// Machine logs the archive holds (ADR-053 D4), one row per
    /// `(machine, harness, log-id)`.
    ///
    /// A separate list from `machines[].sessions` by construction, so every
    /// consumer of this report that counts sessions keeps counting exactly
    /// what it counted before a machine log existed.
    pub machine_logs: Vec<MachineLogBackedUp>,
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
/// newest to oldest and keeps the newest copy of each shard path/sequence,
/// while retaining older sequences missing from later snapshots. A session is
/// reported against its newest snapshot appearance, but its body can span pushes.
///
/// One function rather than one per reader, so every reader uses the same
/// snapshot order. `read --all-machines` / `verify` L3
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
    /// Read the newest archived provenance observation sidecar for one
    /// machine. The match is by the archived trailing `meta/<machine>/`
    /// components; the original stage root is intentionally irrelevant.
    pub fn read_archived_provenance_observations(
        &self,
        mk: &MasterKey,
        machine: &str,
    ) -> anyhow::Result<Vec<crate::provenance::ProvenanceObservation>> {
        let backends = self.backends()?;
        let (repo, _adoption) = crate::orphans::open_adopting(&self.cfg, &backends, mk)
            .context("open repository for provenance observations")?;
        self.require_sound_packs(&repo)?;
        let snapshots = snapshots_by_host_newest_first(repo.get_all_snapshots()?);
        let Some((_, host_snaps)) = snapshots.into_iter().find(|(host, _)| host == machine) else {
            anyhow::bail!("no snapshot for machine `{machine}` in this repository");
        };
        for snapshot in host_snaps {
            let root = repo.node_from_snapshot_and_path(&snapshot, "")?;
            let entries = repo
                .ls(&root, &LsOptions::default())?
                .collect::<rustic_core::RusticResult<Vec<_>>>()?;
            for (path, node) in entries {
                if node.node_type != NodeType::File || !is_provenance_path(&path, machine) {
                    continue;
                }
                let mut bytes = Vec::new();
                repo.dump(&node, &mut bytes)
                    .context("read archived provenance observations")?;
                return crate::provenance::parse_observations(&bytes);
            }
        }
        Ok(Vec::new())
    }

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

        // Machine-log generations, accumulated across the snapshots this read
        // walks. Kept out of `buckets`/`sessions` entirely (ADR-053 D4).
        let mut machine_log_shards: MachineLogShards = BTreeMap::new();

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
                if let Some(key) = machine_log_shard_path(path) {
                    collect_machine_log_shard(
                        &mut machine_log_shards,
                        path,
                        key,
                        node,
                        &snapshot_id,
                    );
                }
            }

            let mut sessions = Vec::new();
            for ((machine, session_id), mut shards) in buckets {
                // Bucket names are not the ordering key: the six-digit shard
                // sequence is global across all buckets.
                shards.sort_by_key(|(n, _)| store_seq_of(n));
                let mut concat = Vec::new();
                let mut shard_sha256 = Vec::with_capacity(shards.len());
                let mut shard_sequences = Vec::with_capacity(shards.len());
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
                    shard_sequences.push(crate::store::parse_shard_seq(shard));
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
                    shard_sequences,
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
        // Machine logs are read whether or not a `wanted` session set was
        // given: a wanted-set answer that omitted a log would be read as "the
        // archive does not hold it" and force a full re-read of that log, so
        // the small generation set is always included. Session filtering is
        // unchanged.
        report.machine_logs = materialize_machine_logs(&repo, machine_log_shards)?;
        report.machines = merges;
        Ok(report)
    }

    /// Cumulative cross-machine read-back merge (`read --all-machines` and `verify` L3).
    ///
    /// Answers "what do all snapshots hold across machines, cumulatively" (ADR-021).
    /// Groups snapshots by hostname and traverses newest first, keeping the newest
    /// copy of each shard path/sequence while retaining older missing sequences.
    /// Only the resolved sessions (filtered by `wanted` if specified) have their
    /// data blobs downloaded and concatenated into verification triples.
    pub fn read_cumulative_sessions(
        &self,
        mk: &MasterKey,
        wanted: Option<&BTreeSet<(String, String)>>,
    ) -> anyhow::Result<ReadAllReport> {
        self.read_cumulative_sessions_inner(mk, wanted, DuplicateShardPolicy::KeepAll)
    }

    /// Raw cumulative bytes for L3 reconciliation and duplicate inventory.
    /// User-facing bulk reads use [`Self::read_all_machines`] so a
    /// whole-prefix reseal cannot duplicate conversation content.
    pub fn read_cumulative_sessions_raw(
        &self,
        mk: &MasterKey,
        wanted: Option<&BTreeSet<(String, String)>>,
    ) -> anyhow::Result<ReadAllReport> {
        self.read_cumulative_sessions_inner(mk, wanted, DuplicateShardPolicy::KeepAll)
    }

    fn read_cumulative_sessions_inner(
        &self,
        mk: &MasterKey,
        wanted: Option<&BTreeSet<(String, String)>>,
        shard_policy: DuplicateShardPolicy,
    ) -> anyhow::Result<ReadAllReport> {
        let backends = self.backends()?;
        let (repo, _adoption) = crate::orphans::open_adopting(&self.cfg, &backends, mk)
            .context("open repository for read-all")?;
        self.require_sound_packs(&repo)?;

        let snaps = repo.get_all_snapshots().context("list snapshots")?;
        let snapshots_in_repo = snaps.len();

        let mut report = ReadAllReport::default();
        report.snapshots_in_repo = snapshots_in_repo;

        // Machine-log generations, accumulated across every snapshot of every
        // host this read walks — one row per (machine, harness, log-id) at the
        // end. Kept entirely out of `sessions_map` (ADR-053 D4).
        let mut machine_log_shards: MachineLogShards = BTreeMap::new();

        let mut merges = Vec::new();
        for (_host, snaps) in snapshots_by_host_newest_first(snaps) {
            let newest_snap = &snaps[0];
            let hostname = newest_snap.hostname.clone();
            let snapshot_id = newest_snap.id.to_hex().as_str().to_string();
            let snapshot_time = newest_snap.time.to_string();
            let snapshot_time_unix = newest_snap.time.timestamp().as_second();

            // Keep the newest copy of every sequence, while retaining older
            // sequences absent from later snapshots. A reclaimed stage may
            // push only the next shard, so the newest snapshot alone is not a
            // complete session body.
            let mut sessions_map: BTreeMap<
                String,
                BTreeMap<(u64, String), (Option<u64>, String, String, rustic_core::repofile::Node)>,
            > = BTreeMap::new();
            let mut observations: BTreeMap<String, Vec<crate::provenance::ProvenanceObservation>> =
                BTreeMap::new();
            let mut metadata_loaded = false;
            let mut sequence_paths: BTreeMap<(String, u64), String> = BTreeMap::new();
            let mut ambiguous_sequences: BTreeSet<(String, u64)> = BTreeSet::new();

            for snap in &snaps {
                let snap_id = snap.id.to_hex().as_str().to_string();
                let short = snap_id[..8.min(snap_id.len())].to_string();

                let root = match repo.node_from_snapshot_and_path(snap, "") {
                    Ok(n) => n,
                    Err(e) => {
                        report.incomplete_machines.insert(hostname.clone());
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
                            report.incomplete_machines.insert(hostname.clone());
                            report.warnings.push(format!(
                                "host `{hostname}` snapshot {}: cannot collect entries: {e}",
                                snap.id.to_hex().as_str()
                            ));
                            continue;
                        }
                    },
                    Err(e) => {
                        report.incomplete_machines.insert(hostname.clone());
                        report.warnings.push(format!(
                            "host `{hostname}` snapshot {}: cannot ls tree: {e}",
                            snap.id.to_hex().as_str()
                        ));
                        continue;
                    }
                };

                for (path, node) in entries {
                    if node.node_type != NodeType::File {
                        continue;
                    }
                    if shard_policy.collapses() && is_provenance_path(&path, &hostname) {
                        if !metadata_loaded {
                            let mut bytes = Vec::new();
                            repo.dump(&node, &mut bytes).with_context(|| {
                                format!("snapshot {short}: cannot read provenance metadata")
                            })?;
                            for observation in crate::provenance::parse_observations(&bytes)
                                .with_context(|| {
                                    format!("snapshot {short}: parse provenance metadata")
                                })?
                            {
                                if observation.dimensions.is_valid() {
                                    observations
                                        .entry(observation.session_id.clone())
                                        .or_default()
                                        .push(observation);
                                }
                            }
                            metadata_loaded = true;
                        }
                        continue;
                    }
                    if let Some(key) = machine_log_shard_path(&path) {
                        collect_machine_log_shard(
                            &mut machine_log_shards,
                            &path,
                            key,
                            &node,
                            &short,
                        );
                    }
                    if let Some((m, session, shard)) = bucket_shard_path(&path) {
                        if m == hostname {
                            if let Some(wanted_set) = wanted {
                                if !wanted_set.contains(&(hostname.clone(), session.clone())) {
                                    continue;
                                }
                            }
                            let (identity, sequence) = archive_shard_identity(&path, &shard);
                            if let Some(sequence) = sequence {
                                let key = (session.clone(), sequence);
                                let path_identity = path.to_string_lossy().into_owned();
                                if sequence_paths
                                    .insert(key.clone(), path_identity.clone())
                                    .is_some_and(|prior| prior != path_identity)
                                {
                                    ambiguous_sequences.insert(key);
                                }
                            }
                            sessions_map
                                .entry(session)
                                .or_default()
                                .entry(identity)
                                .or_insert((sequence, short.clone(), shard, node));
                        }
                    }
                }
            }

            let mut sessions = Vec::new();
            for (session_id, shards) in sessions_map {
                let shards: Vec<_> = shards
                    .into_values()
                    .map(|(sequence, snapshot, shard, node)| (sequence, snapshot, shard, node))
                    .collect();
                let mut concat = Vec::new();
                let mut shard_sha256 = Vec::with_capacity(shards.len());
                let mut shard_bytes = Vec::with_capacity(shards.len());
                let mut shard_sequences = Vec::with_capacity(shards.len());
                let mut shard_run_duplicates = Vec::new();
                let mut shard_offsets: Vec<usize> = Vec::with_capacity(shards.len());
                let mut shard_bodies = Vec::with_capacity(shards.len());
                for (sequence, snapshot, shard, node) in &shards {
                    let mut buf = Vec::new();
                    repo.dump(node, &mut buf)
                        .with_context(|| format!("snapshot {snapshot}: dump shard {shard}"))?;
                    shard_bodies.push((
                        sequence.filter(|sequence| {
                            !ambiguous_sequences.contains(&(session_id.clone(), *sequence))
                        }),
                        buf,
                    ));
                }
                let duplicate_indices: BTreeSet<_> = if shard_policy.collapses() {
                    let bodies: Vec<_> =
                        shard_bodies.iter().map(|(_, body)| body.clone()).collect();
                    crate::store::duplicate_shard_indices(&bodies)
                        .into_iter()
                        .collect()
                } else {
                    BTreeSet::new()
                };
                let observed_sequences = crate::provenance::verified_observation_sequences(
                    &session_id,
                    observations.get(&session_id).map_or(&[], Vec::as_slice),
                    &shard_bodies,
                );
                for ((_sequence, _snapshot, _shard, _node), (index, (sequence, buf))) in
                    shards.iter().zip(shard_bodies.iter().enumerate())
                {
                    let digest: [u8; 32] = Sha256::digest(&buf).into();
                    let provenance_bound =
                        sequence.is_some_and(|sequence| observed_sequences.contains(&sequence));
                    if duplicate_indices.contains(&index) && !provenance_bound {
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
                    shard_sequences.push(*sequence);
                    concat.extend_from_slice(&buf);
                }
                sessions.push(SessionBackedUp {
                    machine: hostname.clone(),
                    session_id,
                    shard_count: shards.len(),
                    concat_bytes: concat.len() as u64,
                    sha256: hex_digest(&Sha256::digest(&concat)),
                    shard_sha256,
                    shard_sequences,
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
        // Machine logs are read whether or not a `wanted` session set was
        // given: a wanted-set answer that omitted a log would be read as "the
        // archive does not hold it" and force a full re-read of that log, so
        // the small generation set is always included. Session filtering is
        // unchanged.
        report.machine_logs = materialize_machine_logs(&repo, machine_log_shards)?;
        report.machines = merges;
        Ok(report)
    }

    /// Cross-machine read-back merge (`read --all-machines`).
    ///
    /// Answers "what do I have archived, across every machine, right now".
    /// Traverses all snapshots cumulatively, grouping sessions by their newest
    /// snapshot appearance (ADR-021).
    pub fn read_all_machines(&self, mk: &MasterKey) -> anyhow::Result<ReadAllReport> {
        self.read_cumulative_sessions_inner(mk, None, self.shard_policy())
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
                let Some((found_machine, session, shard)) = bucket_shard_path(&path) else {
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
        visit: F,
    ) -> anyhow::Result<CumulativeSessionRead>
    where
        F: FnMut(&str, &[u8]) -> anyhow::Result<()>,
    {
        // The index paths have no flag of their own, so their opt-out is the
        // environment: [`crate::store::KEEP_ALL_SHARDS_ENV`] set to a truthy
        // value keeps every stored shard in the indexed text instead of
        // collapsing a whole-content replay.
        self.for_each_archived_session_with_policy(
            mk,
            machine,
            DuplicateShardPolicy::from_env(),
            visit,
        )
    }

    /// [`Self::for_each_archived_session`] with the shard-joining policy named
    /// directly, so a caller (or a test) can decide it without a process-wide
    /// environment change.
    pub fn for_each_archived_session_with_policy<F>(
        &self,
        mk: &MasterKey,
        machine: &str,
        shard_policy: DuplicateShardPolicy,
        mut visit: F,
    ) -> anyhow::Result<CumulativeSessionRead>
    where
        F: FnMut(&str, &[u8]) -> anyhow::Result<()>,
    {
        self.for_each_archived_session_shards_with_policy(
            mk,
            machine,
            shard_policy,
            |id, shards| {
                let concat: Vec<u8> = shards
                    .iter()
                    .flat_map(|(_, body)| body.iter().copied())
                    .collect();
                visit(id, &concat)
            },
        )
    }

    /// Shard-preserving archive walk for consumers that need provenance body
    /// bindings. The callback receives shards with optional parsed sequence
    /// numbers, after applying the same duplicate policy as the concatenated reader.
    pub fn for_each_archived_session_shards_with_policy<F>(
        &self,
        mk: &MasterKey,
        machine: &str,
        shard_policy: DuplicateShardPolicy,
        mut visit: F,
    ) -> anyhow::Result<CumulativeSessionRead>
    where
        F: FnMut(&str, &[(Option<u64>, Vec<u8>)]) -> anyhow::Result<()>,
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
        // Take the newest copy of each sequence number, while retaining older
        // sequence numbers that later snapshots no longer contain. Reclaimed
        // stages can append a new shard after an earlier push, so resolving a
        // whole session from its newest snapshot would lose the archived prefix.
        let mut held: BTreeMap<String, BTreeMap<(u64, String), (Option<u64>, String, Node)>> =
            BTreeMap::new();
        let mut observations: BTreeMap<String, Vec<crate::provenance::ProvenanceObservation>> =
            BTreeMap::new();
        let mut metadata_loaded = false;
        let mut sequence_paths: BTreeMap<(String, u64), String> = BTreeMap::new();
        let mut ambiguous_sequences: BTreeSet<(String, u64)> = BTreeSet::new();
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

            for (path, node) in entries {
                if node.node_type != NodeType::File {
                    continue;
                }
                if shard_policy.collapses() && is_provenance_path(&path, machine) {
                    if !metadata_loaded {
                        let mut bytes = Vec::new();
                        repo.dump(&node, &mut bytes).with_context(|| {
                            format!("snapshot {short}: cannot read provenance metadata")
                        })?;
                        for observation in crate::provenance::parse_observations(&bytes)
                            .with_context(|| {
                                format!("snapshot {short}: parse provenance metadata")
                            })?
                        {
                            if observation.dimensions.is_valid() {
                                observations
                                    .entry(observation.session_id.clone())
                                    .or_default()
                                    .push(observation);
                            }
                        }
                        metadata_loaded = true;
                    }
                    continue;
                }
                let Some((found_machine, session, shard)) = bucket_shard_path(&path) else {
                    continue;
                };
                if found_machine != machine {
                    continue;
                }
                let (identity, sequence) = archive_shard_identity(&path, &shard);
                if let Some(sequence) = sequence {
                    let key = (session.clone(), sequence);
                    let path_identity = path.to_string_lossy().into_owned();
                    if sequence_paths
                        .insert(key.clone(), path_identity.clone())
                        .is_some_and(|prior| prior != path_identity)
                    {
                        ambiguous_sequences.insert(key);
                    }
                }
                held.entry(session).or_default().entry(identity).or_insert((
                    sequence,
                    short.to_string(),
                    node,
                ));
            }
        }
        for (session, shards) in held {
            let mut shard_bodies = Vec::with_capacity(shards.len());
            for ((_sort_sequence, _identity), (sequence, snapshot, node)) in shards {
                let mut buf = Vec::new();
                repo.dump(&node, &mut buf).with_context(|| {
                    format!(
                        "snapshot {snapshot}: cannot read a shard of session `{}`",
                        crate::id::short_session_id(&session)
                    )
                })?;
                let sequence = sequence.filter(|sequence| {
                    !ambiguous_sequences.contains(&(session.clone(), *sequence))
                });
                shard_bodies.push((sequence, buf));
            }
            let selected = if shard_policy.collapses() {
                let bodies: Vec<_> = shard_bodies.iter().map(|(_, body)| body.clone()).collect();
                let duplicates: BTreeSet<_> = crate::store::duplicate_shard_indices(&bodies)
                    .into_iter()
                    .collect();
                let observed_sequences = crate::provenance::verified_observation_sequences(
                    &session,
                    observations.get(&session).map_or(&[], Vec::as_slice),
                    &shard_bodies,
                );
                shard_bodies
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, shard)| {
                        let provenance_bound = shard
                            .0
                            .is_some_and(|sequence| observed_sequences.contains(&sequence));
                        (!duplicates.contains(&index) || provenance_bound).then_some(shard)
                    })
                    .collect()
            } else {
                shard_bodies
            };
            visit(&session, &selected)?;
            out.sessions += 1;
        }
        Ok(out)
    }
}

pub(crate) fn is_provenance_path(path: &Path, machine: &str) -> bool {
    let parts: Vec<_> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect();
    parts.len() >= 3
        && parts[parts.len() - 3] == "meta"
        && parts[parts.len() - 2] == machine
        && parts[parts.len() - 1] == "provenance-v1.jsonl"
}

fn store_seq_of(name: &str) -> u64 {
    crate::store::parse_shard_seq(name).unwrap_or(u64::MAX)
}

/// Bucket one file path into `(machine, harness, log-id, shard-name)` when its
/// trailing components are the machine-log layout
/// `…/machine-logs/<machine>/<harness>/<log-id>/[<bucket>/]<shard>.jsonl`
/// (ADR-053 D4).
///
/// The same rule as [`bucket_shard_path`], for the same reason: the stage
/// prefix differs per machine and even per host, so the `machine-logs` marker
/// is taken as the *last* component equal to that name. The bucket component
/// is ignored because the six-digit sequence is global. A path with a
/// `sessions` component never matches here, and one with `machine-logs` never
/// matches there — the two products cannot be confused for one another.
pub fn machine_log_shard_path(path: &Path) -> Option<(String, String, String, String)> {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let mut marker = None;
    for (i, c) in comps.iter().enumerate() {
        if *c == MACHINE_LOGS_DIR {
            marker = Some(i);
        }
    }
    let i = marker?;
    let rest = &comps[i + 1..];
    if rest.len() != 4 && rest.len() != 5 {
        return None;
    }
    let shard = rest.last()?;
    if !shard.ends_with(SHARD_SUFFIX) {
        return None;
    }
    // A generation is a *canonical* shard name (`000001.jsonl`), not merely a
    // file that ends in `.jsonl`: the machine-log directory also holds its
    // capture stamps and its sequence counter, and a capture stamp must never
    // be counted as a generation (nor folded into a concatenation).
    crate::store::parse_shard_seq(shard)?;
    Some((
        rest[0].to_string(),
        rest[1].to_string(),
        rest[2].to_string(),
        (*shard).to_string(),
    ))
}

/// Accumulator for machine-log generations across the snapshots one read
/// walks: `(machine, harness, log-id)` → shard identity → shard info.
///
/// The identity is [`archive_shard_identity`]'s, so a generation present in
/// two snapshots is one row, and a generation only an older snapshot holds is
/// retained. Every generation ever pushed is kept — never collapsed — because
/// a cursor claims the whole set a log's directory holds, and after a head
/// rewrite the archive legitimately holds generations from two bases
/// (ADR-053 D3).
type MachineLogShards = BTreeMap<
    (String, String, String),
    BTreeMap<(u64, String), (Option<u64>, String, String, Node)>,
>;

fn collect_machine_log_shard(
    into: &mut MachineLogShards,
    path: &Path,
    (machine, harness, log_id, shard): (String, String, String, String),
    node: &Node,
    snapshot_short: &str,
) {
    let (identity, sequence) = archive_shard_identity(path, &shard);
    into.entry((machine, harness, log_id))
        .or_default()
        .entry(identity)
        .or_insert((sequence, snapshot_short.to_string(), shard, node.clone()));
}

/// Fold accumulated machine-log generations into one row per log, in
/// sequence order. Digests and lengths only: each body is dumped, hashed and
/// dropped, never returned.
fn materialize_machine_logs(
    repo: &Repository<IndexedFullStatus>,
    shards: MachineLogShards,
) -> anyhow::Result<Vec<MachineLogBackedUp>> {
    let mut out = Vec::new();
    for ((machine, harness, log_id), entries) in shards {
        let mut ordered: Vec<_> = entries.into_values().collect();
        // The six-digit sequence is the ordering key; a noncanonical legacy
        // name sorts last, exactly as it does for sessions.
        ordered.sort_by_key(|(_, _, name, _)| store_seq_of(name));
        let mut concat = Vec::new();
        let mut shard_sha256 = Vec::with_capacity(ordered.len());
        let mut shard_sequences = Vec::with_capacity(ordered.len());
        let mut shard_bytes = Vec::with_capacity(ordered.len());
        for (sequence, _snapshot, name, node) in &ordered {
            let mut buf = Vec::new();
            repo.dump(node, &mut buf)
                .with_context(|| format!("dump machine-log shard {name}"))?;
            shard_sha256.push(hex_digest(&Sha256::digest(&buf)));
            shard_sequences.push(*sequence);
            shard_bytes.push(buf.len() as u64);
            concat.extend_from_slice(&buf);
        }
        out.push(MachineLogBackedUp {
            machine,
            harness,
            log_id,
            shard_count: ordered.len(),
            concat_bytes: concat.len() as u64,
            sha256: hex_digest(&Sha256::digest(&concat)),
            shard_sha256,
            shard_sequences,
            shard_bytes,
        });
    }
    out.sort_by(|a, b| {
        (&a.machine, &a.harness, &a.log_id).cmp(&(&b.machine, &b.harness, &b.log_id))
    });
    Ok(out)
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

/// Stable identity while accumulating session shards across snapshots. Valid
/// sequence filenames dedupe within the session; invalid filenames are kept
/// by full archive-relative path so equal basenames cannot collide.
fn archive_shard_identity(path: &Path, shard: &str) -> ((u64, String), Option<u64>) {
    let sequence = crate::store::parse_shard_seq(shard);
    let sort_sequence = sequence.unwrap_or(u64::MAX);
    // Distinct paths with the same sequence are malformed/ambiguous, but both
    // payloads must survive readback. Repeated copies at the same path across
    // snapshots still collapse to the newest node.
    let path_identity = path.to_string_lossy().into_owned();
    ((sort_sequence, path_identity), sequence)
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

    #[test]
    fn malformed_shards_with_same_basename_keep_distinct_archive_paths() {
        let first = archive_shard_identity(
            Path::new("root/sessions/m/s/001/legacy.jsonl"),
            "legacy.jsonl",
        );
        let second = archive_shard_identity(
            Path::new("root/sessions/m/s/002/legacy.jsonl"),
            "legacy.jsonl",
        );
        assert_eq!(first.1, None);
        assert_eq!(second.1, None);
        assert_ne!(first, second);
    }

    #[test]
    fn same_sequence_with_distinct_archive_paths_keeps_both_identities() {
        let first = archive_shard_identity(
            Path::new("root/sessions/m/s/001/000007.jsonl"),
            "000007.jsonl",
        );
        let second = archive_shard_identity(
            Path::new("root/sessions/m/s/002/000007.jsonl"),
            "000007.jsonl",
        );
        assert_eq!(first.1, Some(7));
        assert_eq!(second.1, Some(7));
        assert_ne!(first.0, second.0);
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
            shard_sequences: vec![Some(1)],
            shard_bytes: vec![payload.len() as u64],
            shard_run_duplicates: Vec::new(),
        };
        assert!(good.sha_matches(&payload));
        let mut bad = payload.clone();
        bad[7] ^= 0x02;
        assert!(!good.sha_matches(&bad));
    }
}
