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
//! Every traversal can list a snapshot whose tree packs the index its own
//! open loaded does not hold — a read racing a push on the same append-only
//! repository. The READ-RACE gate (see it below, above `impl BackupStore`)
//! reloads the index once and re-reads once, bounded, and otherwise keeps
//! the outcome an unknown; this file's readers inherit it at their entry
//! points, together with the two session-body readers in
//! [`crate::store`] that walk through the same rules.
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

/// The READ-RACE gate: one index reload and one re-read, bounded (A→C).
///
/// What was observed, reproducibly: a read-back can list a snapshot whose
/// tree packs the *loaded* index does not hold. The index is read once at
/// open time, a push commits index files before its snapshot file, and the
/// snapshot listing happens after both — so between an open and a snapshot
/// listing a push can land that the in-memory index predates. The read then
/// stops with `Tree ID … not found in index`: a "did not finish" answer,
/// never a count. On 2026-10-28 a 50-session read-back hit exactly this at
/// sample 31 of 50; a re-read from a fresh open resolved it and the session
/// was ruled matched, which is also the whole shape of the fix.
///
/// The approved handling, in the product:
///
/// * **A — reload once, then re-read.** When a read-back's attempt fails
///   because a snapshot tree or a shard blob is not in the index that
///   attempt loaded — [`index_race_in`], over the `I001`/`I002`/`I003`
///   codes the pinned reader attaches to its index-miss errors — wait
///   first, then run the attempt a second time. Every attempt re-opens the
///   repository itself, so the second attempt *is* the reload: it re-reads
///   the index files from the repository before it judges the same
///   snapshots again. The wait is the bound the read-back drivers already
///   used (one retry after a 300 s wait), so composing the product gate
///   with an ops-driver gate stays inside one budget shape.
/// * **C — otherwise keep the unknown.** If the second attempt hits the
///   same miss, the outcome is the established unknown semantics: `Err`
///   for the strict readers (the CLI's exit 3, "did not finish"), a
///   warning and an incomplete report for the cumulative ones. It is never
///   a match, and never a shard count: the proof-of-absence wording of
///   [`crate::store::BackupStore::read_session_concat`] stays reserved
///   for a walk that read every snapshot and found nothing.
/// * **B — neither fallback nor override.** A read that cannot resolve the
///   newest snapshot does not fall back to an older one to answer anyway —
///   an older copy could stand in for a superseded body and the drift
///   between them would never surface — and there is no switch to make it
///   do so, because masking a genuinely broken archive is the use such a
///   switch would find.
///
/// The gate is per *call*, not per snapshot: one classified failure spends
/// the budget, and a retry restarts that read from its own repository
/// open. That is the same granularity the driver-level gate had, so
/// behaviour the ops scripts already rely on does not change shape under
/// them.
///
/// The two test hooks are process environment, production unset: the wait
/// bound and a rendezvous directory, following [`crate::orphans`]' hold
/// hooks. Production waits the fixed 300 s cap once.
const INDEX_RACE_RELOAD_WAIT_SECONDS: u64 = 300;

/// Test hook: while set, the READ-RACE gate's wait is bounded to this many
/// milliseconds instead of the production cap. Unset in production; the
/// unit tests of this module set it so the retry arithmetic can be pinned
/// without sleeping for real.
const TEST_RACE_WAIT_MS_ENV: &str = "CHAT_STASHER_TEST_INDEX_RACE_WAIT_MS";

/// Test hook: a directory for the read-back wait rendezvous. While set, the
/// wait writes `reached` into it when the gate is about to park, and then
/// polls for `go` instead of sleeping blind, so an integration test can fix
/// the repository up *between* the two attempts — the one window an outside
/// process cannot reach any other way. Unset in production.
const TEST_RACE_WAIT_DIR_ENV: &str = "CHAT_STASHER_TEST_HOLD_INDEX_RACE_WAIT";

/// The three `rustic_core` error codes that mark an index-miss: the reader
/// could not resolve a tree or a blob id in the in-memory index. Structural,
/// the same [`rustic_core::RusticError::is_code`] pin `repofile/keyfile.rs`
/// uses for its own codes — the rendered text is never matched, so a reworded
/// message cannot silently widen or narrow what this gate retries.
const INDEX_RACE_ERROR_CODES: [&str; 3] = ["I001", "I002", "I003"];

/// Whether a `rustic_core` error is an index-miss (`I001`/`I002`/`I003`).
fn index_race_rustic(error: &rustic_core::RusticError) -> bool {
    INDEX_RACE_ERROR_CODES
        .iter()
        .any(|code| error.is_code(code))
}

/// Whether an `anyhow` chain carries an index-miss.
///
/// The pinned reader's errors reach the read-back wrapped as `Box<
/// RusticError>` — which is itself the chain root, per how `anyhow` keeps
/// the boxed type — so the downcast targets the box first and the bare
/// error second. Context layers above it never hide the root.
fn index_race_in(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let rustic = match cause.downcast_ref::<Box<rustic_core::RusticError>>() {
            Some(boxed) => boxed.as_ref(),
            None => match cause.downcast_ref::<rustic_core::RusticError>() {
                Some(bare) => bare,
                None => return false,
            },
        };
        index_race_rustic(rustic)
    })
}

/// The wait before the one re-read.
///
/// Production sleeps the fixed cap from [`INDEX_RACE_RELOAD_WAIT_SECONDS`]
/// exactly once, mirroring the gate the read-back drivers already ran: the
/// miss is one a concurrent push typically resolves, and a backend whose
/// listing lags is given time to converge before the single retry spends
/// itself. The test-env forms shrink the bound and add the
/// `reached`/`go` rendezvous described on [`TEST_RACE_WAIT_DIR_ENV`].
fn index_race_wait() {
    let bound_ms = std::env::var(TEST_RACE_WAIT_MS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    let Some(dir) = std::env::var(TEST_RACE_WAIT_DIR_ENV)
        .ok()
        .filter(|value| !value.is_empty())
    else {
        std::thread::sleep(std::time::Duration::from_millis(
            bound_ms.unwrap_or(INDEX_RACE_RELOAD_WAIT_SECONDS * 1000),
        ));
        return;
    };
    let dir = Path::new(&dir);
    #[allow(
        clippy::let_underscore_must_use,
        reason = "A rendezvous that cannot be staged leaves its test to report the miss, which is the failure shape the test exists to surface."
    )]
    let _ = std::fs::create_dir_all(dir);
    #[allow(
        clippy::let_underscore_must_use,
        reason = "As above: the test that did not reach the gate is the report itself."
    )]
    let _ = std::fs::write(dir.join("reached"), b"");
    let go = dir.join("go");
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(
            bound_ms.unwrap_or(INDEX_RACE_RELOAD_WAIT_SECONDS * 1000),
        );
    while !go.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Whether the one re-read may be announced as a success: only an attempt
/// whose own raced flag is `false`. The report-shaped readers answer `Ok`
/// with a warning-plus-incomplete-report when they stay raced, and those
/// must keep speaking their own unknown vocabulary — a success note over a
/// still-warning report would tell the operator "succeeded" on a read
/// that stayed unknown.
///
/// Unit-pinned next to the retry arithmetic: the flag-true → flag-true
/// case returns the second report, and this predicate is what keeps its
/// note silent.
fn reread_resolved_without_miss(second_raced: bool) -> bool {
    !second_raced
}

/// The one re-read of the READ-RACE gate over an attempt that re-opens the
/// repository itself.
///
/// `attempt` returns its value together with a `bool`: `true` when this
/// attempt recorded a not-in-index condition into its own unknown
/// vocabulary — a warning plus an incomplete report — instead of an `Err`.
/// The first branch of the gate is symmetric over both shapes; the strict
/// readers return plain values and pass `false` through
/// [`reread_plain_once_on_index_race`].
///
/// Exactly one retry, ever:
///
/// * first attempt clean (`(value, false)` or an unclassified `Err`) —
///   returned as-is; the gate did not fire;
/// * first attempt raced — wait ([`index_race_wait`]), re-run once, return the
///   second attempt's outcome whatever it is;
/// * the second attempt is never retried again, and its still-raced result
///   is the unknown the caller already records (the report's warnings, or
///   the `Err` carrying the walk's own did-not-finish wording).
///
/// A `visit`-shaped attempt re-runs *whole* on that one retry: the
/// callback runs again for sessions the raced attempt already delivered,
/// because the re-loaded open cannot resume a half-finished walk. The one
/// wrapper with such an attempt
/// ([`BackupStore::for_each_archived_session_shards_with_policy`]) carries
/// that contract for its callers in its own doc.
///
/// The only mutation on the outcome is one context line on a *classified*
/// second failure, stating that the reload already happened — so an
/// operator reading a still-racing read knows the product gate spent its
/// one retry and the two-attempt budget is not still available.
pub(crate) fn reread_once_on_index_race<T>(
    mut attempt: impl FnMut() -> anyhow::Result<(T, bool)>,
) -> anyhow::Result<T> {
    let first = attempt();
    let raced = match &first {
        Ok((_, raced)) => *raced,
        Err(error) => index_race_in(error),
    };
    if !raced {
        return first.map(|(value, _)| value);
    }
    eprintln!(
        "[read-back] a snapshot tree this read resolves was not in the index this open \
         loaded — the usual cause is a read racing a push writing to the same archive, \
         and an archive that genuinely lacks that tree in its index reads the same \
         from here; waiting {} s, then reloading the index and reading again once",
        bound_ms().unwrap_or(INDEX_RACE_RELOAD_WAIT_SECONDS * 1000) / 1000
    );
    index_race_wait();
    match attempt() {
        Ok((value, second_raced)) => {
            // The note is earned by this second attempt alone: a
            // report-shaped reader that stays flagged answers `Ok` with a
            // warning-laden report, and a success line over it would
            // contradict the warning it still prints. The gate stays
            // silent there, and the report's own unknown vocabulary keeps
            // the last word.
            if reread_resolved_without_miss(second_raced) {
                eprintln!("[read-back] the re-read after the index reload succeeded");
            }
            Ok(value)
        }
        Err(second) => {
            if index_race_in(&second) {
                Err(second.context(
                    "the index was reloaded once and the read retried; a snapshot tree \
                     was still not in the index the reloaded open loaded",
                ))
            } else {
                Err(second)
            }
        }
    }
}

/// [`reread_once_on_index_race`] for the strict readers, whose attempts
/// return plain values: a raced attempt is one whose `Err` classifies.
pub(crate) fn reread_plain_once_on_index_race<T>(
    mut attempt: impl FnMut() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    reread_once_on_index_race(move || attempt().map(|value| (value, false)))
}

/// The wait bound this process will actually use, in milliseconds — the
/// production cap unless the test hook narrows it. The printed note says
/// the real number, not the constant, so the test form never lies about
/// waiting five minutes.
fn bound_ms() -> Option<u64> {
    std::env::var(TEST_RACE_WAIT_MS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
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
    /// Used by `stagereclaim` to prove candidates that are still currently on
    /// stage. A snapshot tree that is not in the index this read's open
    /// loaded first goes through the READ-RACE gate above — one index
    /// reload and one re-read — and only then lands in the warning below,
    /// which keeps the answer unknown rather than complete.
    pub fn read_latest_per_machine(&self, mk: &MasterKey) -> anyhow::Result<ReadAllReport> {
        reread_once_on_index_race(|| self.read_latest_per_machine_attempt(mk))
    }

    /// [`Self::read_latest_per_machine`]'s single attempt: one repository
    /// open, one walk. The `bool` reports whether a snapshot tree the walk
    /// could not resolve is a READ-RACE index-miss (see [`reread_once_on_index_race`]).
    fn read_latest_per_machine_attempt(
        &self,
        mk: &MasterKey,
    ) -> anyhow::Result<(ReadAllReport, bool)> {
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
        let mut raced_index = false;

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
                    raced_index = raced_index || index_race_rustic(&e);
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
        Ok((report, raced_index))
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
        self.read_cumulative_sessions_inner(mk, wanted, DuplicateShardPolicy::KeepAll, false)
    }

    /// Raw cumulative bytes for L3 reconciliation and duplicate inventory.
    /// User-facing bulk reads use [`Self::read_all_machines`] so a
    /// whole-prefix reseal cannot duplicate conversation content.
    pub fn read_cumulative_sessions_raw(
        &self,
        mk: &MasterKey,
        wanted: Option<&BTreeSet<(String, String)>>,
    ) -> anyhow::Result<ReadAllReport> {
        self.read_cumulative_sessions_inner(mk, wanted, DuplicateShardPolicy::KeepAll, false)
    }

    fn read_cumulative_sessions_inner(
        &self,
        mk: &MasterKey,
        wanted: Option<&BTreeSet<(String, String)>>,
        shard_policy: DuplicateShardPolicy,
        apply_control_policy: bool,
    ) -> anyhow::Result<ReadAllReport> {
        reread_once_on_index_race(move || {
            self.read_cumulative_sessions_attempt(mk, wanted, shard_policy, apply_control_policy)
        })
    }

    /// [`read_cumulative_sessions_inner`]'s single attempt: one repository
    /// open, one cumulative walk. The `bool` reports whether any snapshot
    /// tree the walk could not resolve is a READ-RACE index-miss (see
    /// [`reread_once_on_index_race`]); the walk's own `Err`s classify
    /// directly at the gate.
    fn read_cumulative_sessions_attempt(
        &self,
        mk: &MasterKey,
        wanted: Option<&BTreeSet<(String, String)>>,
        shard_policy: DuplicateShardPolicy,
        apply_control_policy: bool,
    ) -> anyhow::Result<(ReadAllReport, bool)> {
        let backends = self.backends()?;
        let (repo, _adoption) = crate::orphans::open_adopting(&self.cfg, &backends, mk)
            .context("open repository for read-all")?;
        self.require_sound_packs(&repo)?;

        let snaps = repo.get_all_snapshots().context("list snapshots")?;
        let snapshots_in_repo = snaps.len();

        let mut report = ReadAllReport::default();
        report.snapshots_in_repo = snapshots_in_repo;
        let mut raced_index = false;

        // ADR-046 (draft) Phase 1: the *user-asking* walks fold the effective
        // control stream — each host's newest snapshot's copy, unioned across
        // partitions — before any session resolves, so a hide recorded by one
        // machine is honored for identities this walk touches in any
        // partition. The administrative walks (`verify` L3, duplicate
        // inventory, collector debt) deliberately pass `false`: they ask what
        // is *physically held*, and a hidden session is still held — L3 must
        // keep proving the stage against it, or "hidden" would quietly stop
        // meaning "presented differently" and start meaning "unchecked". An
        // unreadable or conflicting control state is an `Err` either way:
        // the walk fails closed rather than resolve sessions whose policy it
        // cannot establish.
        let grouped_hosts = snapshots_by_host_newest_first(snaps);
        let control_policy = if apply_control_policy {
            // Damage tolerance is the readers' own contract (W257/W271): a
            // snapshot the walk cannot list is recorded by the walk, in the
            // vocabulary every reader of this report expects, and makes the
            // whole report partial — which is how an unestablished control
            // state is surfaced here too, because sessions a damaged
            // snapshot could have governed cannot be listed at all. The
            // strict single-identity paths
            // (`BackupStore::read_session_concat`, the FTS build) close on
            // that state instead of summarizing beside it.
            let (policy, _damage_notes) =
                crate::deletion_control::load_reader_policy_with_notes(&repo, &grouped_hosts)?;
            Some(policy)
        } else {
            None
        };

        // Machine-log generations, accumulated across every snapshot of every
        // host this read walks — one row per (machine, harness, log-id) at the
        // end. Kept entirely out of `sessions_map` (ADR-053 D4).
        let mut machine_log_shards: MachineLogShards = BTreeMap::new();

        let mut merges = Vec::new();
        for (_host, snaps) in grouped_hosts {
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
                        raced_index = raced_index || index_race_rustic(&e);
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
                            raced_index = raced_index || index_race_rustic(&e);
                            report.incomplete_machines.insert(hostname.clone());
                            report.warnings.push(format!(
                                "host `{hostname}` snapshot {}: cannot collect entries: {e}",
                                snap.id.to_hex().as_str()
                            ));
                            continue;
                        }
                    },
                    Err(e) => {
                        raced_index = raced_index || index_race_rustic(&e);
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
                            if control_policy
                                .as_ref()
                                .is_some_and(|policy| policy.omitted_from_readers(&m, &session))
                            {
                                continue;
                            }
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
        Ok((report, raced_index))
    }

    /// Cross-machine read-back merge (`read --all-machines`).
    ///
    /// Answers "what do I have archived, across every machine, right now".
    /// Traverses all snapshots cumulatively, grouping sessions by their newest
    /// snapshot appearance (ADR-021).
    pub fn read_all_machines(&self, mk: &MasterKey) -> anyhow::Result<ReadAllReport> {
        self.read_cumulative_sessions_inner(mk, None, self.shard_policy(), true)
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
        reread_plain_once_on_index_race(move || {
            self.dump_machine_sessions_attempt(mk, machine, wanted)
        })
    }

    /// [`Self::dump_machine_sessions`]'s single attempt: one repository open,
    /// one walk of the newest snapshot per machine. A READ-RACE index-miss
    /// is classified and retried at the wrapper.
    fn dump_machine_sessions_attempt(
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
    ///
    /// `visit` may run more than once for a session the raced attempt
    /// already delivered — the READ-RACE gate re-runs the whole walk once;
    /// see [`Self::for_each_archived_session_shards_with_policy`] for that
    /// contract.
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
    /// environment change. `visit` may run more than once for a session the
    /// raced attempt already delivered — the READ-RACE gate re-runs the whole
    /// walk once; see [`Self::for_each_archived_session_shards_with_policy`]
    /// for that contract.
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
    ///
    /// The walk runs behind the READ-RACE gate
    /// ([`reread_once_on_index_race`]): on a classified index-miss it re-runs
    /// the whole walk once against the reloaded open, because the reloaded
    /// open has no way to resume the raced attempt's half-finished walk —
    /// so `visit` **may run more than once for a session the raced attempt
    /// already delivered**. The returned [`CumulativeSessionRead`] counts
    /// one attempt (the last, which is the one that stands), but any
    /// caller-side counting of its own callback must tolerate the
    /// re-visit or keep a one-per-session guard, as the two in-tree
    /// callers do: a session the gate re-hands is one session, not two.
    pub fn for_each_archived_session_shards_with_policy<F>(
        &self,
        mk: &MasterKey,
        machine: &str,
        shard_policy: DuplicateShardPolicy,
        visit: F,
    ) -> anyhow::Result<CumulativeSessionRead>
    where
        F: FnMut(&str, &[(Option<u64>, Vec<u8>)]) -> anyhow::Result<()>,
    {
        let mut visit = visit;
        reread_plain_once_on_index_race(move || {
            self.for_each_archived_session_shards_attempt(mk, machine, shard_policy, &mut visit)
        })
    }

    /// [`Self::for_each_archived_session_shards_with_policy`]'s single
    /// attempt: one repository open, one machine walk. A snapshot tree this
    /// attempt cannot resolve — including a READ-RACE index-miss — is the
    /// `Err` naming the snapshot the walk's contract already requires,
    /// classified by the READ-RACE gate at the wrapper.
    fn for_each_archived_session_shards_attempt<F>(
        &self,
        mk: &MasterKey,
        machine: &str,
        shard_policy: DuplicateShardPolicy,
        visit: &mut F,
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
        // ADR-046 (draft) Phase 1: a single-machine walk still loads the
        // control union from every host's newest snapshot — control records
        // sit in each writer's own partition, so machine A's hide of a
        // machine B identity is honorably loadable here or not at all, and an
        // unreadable stream fails the walk closed.
        let grouped_hosts = snapshots_by_host_newest_first(snaps);
        let control_policy =
            crate::deletion_control::load_strict_reader_policy(&repo, &grouped_hosts)?;
        let Some(host_snaps) = grouped_hosts
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
                if control_policy.omitted_from_readers(&found_machine, &session) {
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

    // -- the READ-RACE gate (W1050). These unit tests pin the retry's own
    // arithmetic and the classifier over the exact error shapes the pinned
    // reader raises; the park-and-recover behaviour over a real repository is
    // pinned by `tests/w1050_read_race_test.rs`, which drives the CLI. The
    // wait bound is process env for the tests only, so the retry test sets and
    // clears it itself — under the serial guard below, because `std::env` is
    // one store per test process and the other threads of this binary's test
    // run read it too.

    /// Serializes every unit test that touches the READ-RACE gate's test
    /// env (the wait bound and the rendezvous dir): they live in
    /// `std::env`, which is shared by every test thread of this binary, so
    /// two tests mutating/reading them concurrently would see half of one
    /// another's mutations. Today that is the retry-arithmetic test below;
    /// a future lib test that drives a gated reader or mutates either
    /// hook takes this lock first.
    static READ_RACE_TEST_ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Removes the wait-bound test env when dropped, so a failed assert
    /// inside the guarded test cannot leave the narrowed bound set for
    /// whatever runs afterwards in this process.
    struct UnsetWaitEnvOnDrop(&'static str);
    impl Drop for UnsetWaitEnvOnDrop {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }

    /// An index-miss exactly as the pinned reader's `tree.rs` raises it —
    /// the same construction, the same code attachment.
    fn index_miss() -> Box<rustic_core::RusticError> {
        rustic_core::RusticError::new(
            rustic_core::ErrorKind::Internal,
            "Tree ID `{tree_id}` not found in index",
        )
        .attach_error_code("I002")
        .attach_context("tree_id", "33c5514b")
    }

    #[test]
    fn readback_index_miss_codes_classify_through_an_anyhow_context() {
        // The three index-miss sites, built the way each site builds itself,
        // must classify — through the `.with_context` wrapping the product
        // puts around them, because that is how they reach the gate.
        for (code, message) in [
            ("I001", "Blob `{id}` with type `{type}` not found in index"),
            ("I002", "Tree ID `{tree_id}` not found in index"),
            ("I003", "Data Blob `{id}`  not found in index"),
        ] {
            let bare = rustic_core::RusticError::new(rustic_core::ErrorKind::Internal, message)
                .attach_error_code(code);
            let wrapped = Err::<(), Box<rustic_core::RusticError>>(bare).context(
                "UNKNOWN: session `…` cannot be resolved for machine `m`: snapshot 33c5514b \
                 tree root",
            );
            let err = wrapped.unwrap_err();
            assert!(
                index_race_in(&err),
                "the {code} site must classify through the product's context wrapping"
            );
        }
        // Nothing else does: a backend error, a bare bail, and the pinned
        // reader's own password error all stay outside the gate, because
        // retrying them cannot help and the budget must not spend itself.
        let backend = rustic_core::RusticError::new(
            rustic_core::ErrorKind::Backend,
            "The destination object is unreachable.",
        );
        let backend = Err::<(), Box<rustic_core::RusticError>>(backend)
            .context("the walk cannot reach the backend")
            .unwrap_err();
        assert!(!index_race_in(&backend), "a backend error is not a race");
        let bail = anyhow::Error::msg("session directory not in newest snapshot");
        assert!(!index_race_in(&bail), "a plain bail is not a race");
        let password = rustic_core::RusticError::new(
            rustic_core::ErrorKind::Credentials,
            "Password is invalid for given key.",
        )
        .attach_error_code("C002");
        let password = Err::<(), Box<rustic_core::RusticError>>(password)
            .context("the key did not open the repository")
            .unwrap_err();
        assert!(
            !index_race_in(&password),
            "the C002 password error is not a race — a retry cannot refresh credentials"
        );
    }

    #[test]
    fn readback_index_race_gate_retries_exactly_once() {
        let _guard = READ_RACE_TEST_ENV_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var(TEST_RACE_WAIT_MS_ENV, "1");
        let _unset_wait_env = UnsetWaitEnvOnDrop(TEST_RACE_WAIT_MS_ENV);

        // Classified first attempt, recovered second: the value of the second
        // attempt is returned, and exactly two attempts ran.
        let mut attempts = 0;
        let out = reread_once_on_index_race(|| {
            attempts += 1;
            match attempts {
                1 => Err::<(usize, bool), anyhow::Error>(index_miss().into())
                    .context("UNKNOWN: snapshot tree root"),
                _ => Ok((7, false)),
            }
        });
        assert_eq!(
            out.expect("the re-read after the reload must answer"),
            7,
            "the second attempt's value is the answer"
        );
        assert_eq!(attempts, 2, "one reload, one re-read, never more");

        // A classified second failure keeps the walk's own error and gains the
        // gate's reload note — and is not retried again.
        let mut attempts = 0;
        let out = reread_once_on_index_race(|| {
            attempts += 1;
            Err::<(usize, bool), anyhow::Error>(index_miss().into())
                .context("UNKNOWN: snapshot tree root")
        });
        let err = out.expect_err("a still-raced read must stay an Err");
        assert_eq!(attempts, 2, "the second attempt is never retried");
        let text = format!("{err:#}");
        assert!(
            text.contains("the index was reloaded once"),
            "the operator must see that the product gate already spent its retry: {text}"
        );
        assert!(
            text.contains("UNKNOWN: snapshot tree root"),
            "the walk's own unknown wording must survive: {text}"
        );

        // An unclassified failure is returned untouched, with no retry: the
        // gate exists for one race, not as a general retry loop.
        let mut attempts = 0;
        let out = reread_once_on_index_race(|| {
            attempts += 1;
            Err::<(usize, bool), anyhow::Error>(anyhow::anyhow!("backend refused"))
        });
        let err = out.expect_err("a non-race error must propagate");
        assert_eq!(attempts, 1, "no wait and no reload for a non-race error");
        assert_eq!(
            format!("{err:#}"),
            "backend refused",
            "nothing was attached to it"
        );

        // A flagged first attempt — the cumulative readers' warning shape —
        // retries once too, and the second attempt's report stands as-is.
        let mut attempts = 0;
        let out = reread_once_on_index_race(|| {
            attempts += 1;
            match attempts {
                1 => Ok((1, true)),
                _ => Ok((2, true)),
            }
        });
        assert_eq!(
            out.expect("the second flags-report is the answer"),
            2,
            "a still-raced report is returned, not looped on"
        );
        assert_eq!(attempts, 2);
        // The success note is earned by the re-read's own outcome, not by
        // the gate having run: the second report above still carries its
        // not-in-index warnings, so announcing a success over it would tell
        // the operator "succeeded" on a read that stayed unknown. Only a
        // re-read that answered without another miss — the strict readers,
        // or a recovered cumulative report — may carry the note.
        assert!(
            reread_resolved_without_miss(false),
            "a re-read that answered without another miss is the shape the \
             success note exists for"
        );
        assert!(
            !reread_resolved_without_miss(true),
            "a re-read that still recorded a not-in-index condition must not be \
             announced as a success — the report it answers with still warns"
        );

        // A clean first attempt costs nothing.
        let mut attempts = 0;
        let out = reread_once_on_index_race(|| {
            attempts += 1;
            Ok((9, false))
        });
        assert_eq!(out.expect("a clean read is passed through"), 9);
        assert_eq!(attempts, 1);

        // The strict readers' plain wrapper does the same bookkeeping.
        let mut attempts = 0;
        let out = reread_plain_once_on_index_race(|| {
            attempts += 1;
            match attempts {
                1 => Err::<usize, anyhow::Error>(index_miss().into()),
                _ => Ok(3),
            }
        });
        assert_eq!(
            out.expect("the plain wrapper must reload and re-read too"),
            3
        );
        assert_eq!(attempts, 2);

        // The wait-bound env is cleared by `_unset_wait_env`'s Drop, which
        // also runs when an assert above fails — the guarded mutation cannot
        // outlive this test.
    }
}
