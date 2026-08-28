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
use rustic_core::repofile::{MasterKey, NodeType, SnapshotFile};
use rustic_core::{Credentials, Grouped, LsOptions, Repository, SnapshotGroupCriterion};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
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

impl BackupStore {
    /// Cross-machine read-back merge.
    ///
    /// Opens the repository fresh, groups all snapshots by hostname, takes the
    /// newest snapshot per hostname, and buckets that snapshot's sealed shards
    /// under its `sessions/<machine>/…` subtree. Session shards are
    /// concatenated in sequence order and hashed. Only ids / counts /
    /// lengths / digests are ever produced.
    pub fn read_all_machines(&self, mk: &MasterKey) -> anyhow::Result<ReadAllReport> {
        let backends = self.backends()?;
        let repo = Repository::new(&self.cfg.repository_options(), &backends)?
            .open(&Credentials::Masterkey(mk.clone()))
            .context("open repository for read-all")?
            .to_indexed()
            .context("index repository for read-all")?;

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

            // A rustic dir-backup's tree mirrors the full source path
            // (`snapshot.paths` minus the leading `/`), so `sessions/` sits at
            // `stage-relative` depth and *not* at the tree root. We therefore
            // list the whole tree and bucket shard files by the `sessions/`
            // marker instead of `node_from_snapshot_and_path(snap, "sessions")`.
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
                for (shard, idx) in &shards {
                    let mut buf = Vec::new();
                    repo.dump(&entries[*idx].1, &mut buf)
                        .with_context(|| format!("dump shard {shard}"))?;
                    concat.extend_from_slice(&buf);
                }
                sessions.push(SessionBackedUp {
                    machine,
                    session_id,
                    shard_count: shards.len(),
                    concat_bytes: concat.len() as u64,
                    sha256: hex_digest(&Sha256::digest(&concat)),
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
        let repo = Repository::new(&self.cfg.repository_options(), &backends)?
            .open(&Credentials::Masterkey(mk.clone()))
            .context("open repository for shard restore")?
            .to_indexed()
            .context("index repository for shard restore")?;
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
        };
        assert!(good.sha_matches(&payload));
        let mut bad = payload.clone();
        bad[7] ^= 0x02;
        assert!(!good.sha_matches(&bad));
    }
}
