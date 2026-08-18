//! Read-only collection of scanner records into sealed local stage shards.
//!
//! A harness file is never renamed, opened for writing, or marked in the
//! harness directory. File sources keep a byte offset and a SHA-256 of the
//! committed prefix in a state file under chat-stasher's own data directory.
//! The opencode SQLite source keeps a logical high-water cursor instead: the
//! store fingerprint, session update time, row counts, and greatest
//! `(time_updated, id)` key for message and part rows. Any mismatch resets the
//! logical read to a complete session export, deliberately preferring a
//! measurable duplicate over a silent omission.
//!
//! # The state is a per-destination debt set, not a per-machine cursor
//!
//! ADR-012 / ADR-013. The archive is the truth; a cursor is only a cache that
//! must be able to prove itself. The durable question is therefore not "how far
//! did this machine read" but **"what does *this destination* still not
//! have"** — a write-intent set, per destination.
//!
//! Each entry carries the read cursor *plus* the sealed shard set that cursor
//! is accountable for, as the `(shard_count, concat_bytes, concat_sha256)`
//! triple that both the stage ([`crate::verify::expected_manifest`]) and the
//! archive ([`crate::readback::ReadAllReport`]) can compute independently.
//! Before a cursor is reused it must be discharged one of exactly two ways:
//!
//! 1. the shards are still sealed on the stage — the debt is still owed and is
//!    fully accounted for locally, so an unreachable destination costs nothing;
//! 2. the shards are gone from the stage, so the destination's own archive is
//!    asked whether it holds that exact triple — the debt was settled.
//!
//! Anything else — archive unreachable, archive disagrees, no entry under this
//! destination at all, state written by an older per-machine format — is
//! **unverifiable, and unverifiable means unread**. The source is reread in
//! full. There is no migration path and no "trust it for now" branch: a
//! measurable duplicate always beats a silent omission.

use crate::config::Config;
use crate::models::{SessionRecord, SqliteSessionLayout};
use crate::scanner;
use crate::sqlite_probe::{
    cursor_global_schema, grok_schema, opencode_session_cursor, read_cursor_legacy_session,
    read_opencode_session, read_sqlite_session, sqlite_session_cursor, OpenCodeCursor,
};
use crate::store;
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const STATE_VERSION: u32 = 2;
const STATE_FILE: &str = "debts-v2.json";
/// The pre-ADR-012 per-machine cursor file. It is never read and never
/// migrated: it belongs to no destination, so it can prove nothing to any
/// destination. Its only remaining job is to be *reported* as ignored, so the
/// resulting full reread is visible instead of silent.
const LEGACY_STATE_FILE: &str = "offsets-v1.json";
const READ_RETRIES: usize = 3;

/// Durable cursor for one source file. `prefix_len` is intentionally repeated
/// alongside `offset`: the state is self-describing and a partially written
/// or hand-edited state cannot silently widen the reusable prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OffsetEntry {
    pub offset: u64,
    pub prefix_len: u64,
    pub prefix_sha256: String,
    pub compressed: bool,
    /// Present only for a virtual SQLite record. This is the logical cursor
    /// replacing a file byte offset; the field name remains `opencode` for
    /// compatibility with the state file written by the previous worker.
    #[serde(default)]
    pub opencode: Option<OpenCodeCursor>,
}

/// The evidence a cursor has to produce on demand: the sealed shard set it is
/// accountable for. Stage and archive compute this triple independently
/// (`verify::expected_manifest` / `readback::SessionBackedUp`), which is
/// precisely why it can serve as the proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardFact {
    pub shard_count: usize,
    pub concat_bytes: u64,
    pub concat_sha256: String,
}

/// One source's outstanding debt to one destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebtEntry {
    /// Machine partition the shards were sealed under. A cursor written for a
    /// different partition addresses a different subtree and proves nothing
    /// here.
    pub machine: String,
    /// Session id — the stage *and* archive lookup key for the shard set.
    pub session_id: String,
    /// Read position that produced the shard set below.
    pub cursor: OffsetEntry,
    /// What the cursor claims it handed over. Verified before reuse.
    pub shards: ShardFact,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DestinationDebts {
    files: BTreeMap<String, DebtEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DebtState {
    version: u32,
    /// Keyed by [`destination_id`]. A destination that is not in this map has
    /// never been read for, so every source is unread for it.
    destinations: BTreeMap<String, DestinationDebts>,
}

/// Do we hold a local record of ever having collected *for* this destination?
///
/// Three answers, not two — the same shape as
/// [`crate::inbox::RememberedInboxes`], and for the same reason.
///
/// ADR-015. The filesystem cannot distinguish "never built" from "built and
/// since lost", but our own state can: `destinations` is keyed by
/// [`destination_id`], and a key only appears once a collect pass ran against
/// that destination. So this answers "have we dealt with it before" without a
/// single byte of network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationRecord {
    /// No state file has ever been written here: this machine has genuinely
    /// never collected for *any* destination. A real answer.
    Unrecorded,
    /// The state was read. `Known(false)` is a trusted "we have never
    /// collected for this one", deliberately distinct from [`Self::Unrecorded`]
    /// and from an unreadable state.
    Known(bool),
}

/// Read the local collector record for one destination.
///
/// B82: the old `destination_has_record` returned a bare `bool` and reached it
/// through `unwrap_or(false)`, so "the state file exists and we could not read
/// it" produced exactly the same answer as "we have never collected for this
/// destination". `dest-init` then used that answer as *evidence* that the
/// destination was never built — ignorance offered as proof. `Err` now means
/// what it says: the record exists and could not be read, so the caller knows
/// nothing about this destination and must not call it empty.
///
/// Note this deliberately does not go through [`load_state`], which maps a
/// corrupt or version-mismatched file to an empty state. That mapping is right
/// for the collector (discard the cursors, re-read everything — the
/// conservative direction there), and wrong here, where "no cursors" would be
/// read as "never dealt with".
pub fn destination_record(
    state_dir: &Path,
    destination_id: &str,
) -> anyhow::Result<DestinationRecord> {
    let path = state_dir.join(STATE_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DestinationRecord::Unrecorded)
        }
        Err(e) => {
            return Err(e).with_context(|| format!("read collector state {}", path.display()))
        }
    };
    let state: DebtState = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse collector state {}", path.display()))?;
    if state.version != STATE_VERSION {
        anyhow::bail!(
            "collector state {} is version {}, this build writes {STATE_VERSION}; its destination list cannot be trusted",
            path.display(),
            state.version
        );
    }
    Ok(DestinationRecord::Known(
        state.destinations.contains_key(destination_id),
    ))
}

/// [`destination_record`] flattened to the historical boolean. Kept for the
/// callers that only ask "is it in there" and have nothing to decide on the
/// difference — an unreadable record answers `false` here, so **never** use it
/// to prove a destination was never built.
pub fn destination_has_record(state_dir: &Path, destination_id: &str) -> bool {
    matches!(
        destination_record(state_dir, destination_id),
        Ok(DestinationRecord::Known(true))
    )
}

/// What a destination's archive can be shown to hold, keyed by
/// `(machine, session_id)`.
pub type ArchiveFacts = BTreeMap<(String, String), ShardFact>;

/// Privacy-safe destination identity. The repository location can be a real
/// path or a real host, so only its digest is ever stored or printed.
pub fn destination_id(repo_root: &str) -> String {
    sha256_hex(repo_root.as_bytes())
}

/// Fold a cross-machine read-back into the facts a cursor can be checked
/// against. This is the only place the archive observation is reshaped; the
/// observation itself comes from [`crate::readback`], unchanged.
pub fn archive_facts_from_readback(report: &crate::readback::ReadAllReport) -> ArchiveFacts {
    report
        .machines
        .iter()
        .flat_map(|machine| machine.sessions.iter())
        .map(|session| {
            (
                (session.machine.clone(), session.session_id.clone()),
                ShardFact {
                    shard_count: session.shard_count,
                    concat_bytes: session.concat_bytes,
                    concat_sha256: session.sha256.clone(),
                },
            )
        })
        .collect()
}

/// The destination `collect` is reading *for*, plus a lazy way to ask that
/// destination's archive what it really holds.
///
/// The probe is lazy and cached on purpose: opening a repository and reading
/// every snapshot back is expensive, and a run where every debt is still owed
/// locally never needs to ask. A probe that fails — unreachable backend,
/// missing key, no repository yet — yields *no* facts, which is not the same
/// as "the archive holds nothing": it makes affected cursors unverifiable, and
/// unverifiable means reread.
pub struct DestinationView<'a> {
    id: String,
    probe: Box<dyn Fn() -> anyhow::Result<ArchiveFacts> + 'a>,
    cache: OnceCell<Option<ArchiveFacts>>,
}

impl<'a> DestinationView<'a> {
    pub fn new(
        id: impl Into<String>,
        probe: impl Fn() -> anyhow::Result<ArchiveFacts> + 'a,
    ) -> Self {
        DestinationView {
            id: id.into(),
            probe: Box::new(probe),
            cache: OnceCell::new(),
        }
    }

    /// A destination whose archive cannot be consulted at all.
    pub fn unreachable(id: impl Into<String>) -> Self {
        DestinationView::new(id, || Err(anyhow!("destination archive is not reachable")))
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    fn facts(&self) -> Option<&ArchiveFacts> {
        self.cache.get_or_init(|| (self.probe)().ok()).as_ref()
    }
}

/// Why a stored cursor could not prove itself. Fixed metadata only — never a
/// source path, a session body or a repository location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebtVerdict {
    /// The debt is still owed and fully accounted for on the stage.
    OwedOnStage,
    /// The debt was settled: the destination's archive holds exactly it.
    SettledInArchive,
    Unverifiable(&'static str),
}

/// Observe what the stage currently holds for one session.
fn stage_shard_fact(stage: &Path, machine: &str, session_id: &str) -> anyhow::Result<ShardFact> {
    let dir = store::session_shard_dir(stage, machine, session_id);
    let shard_count = store::sealed_shard_entries(&dir)?.len();
    let concat = store::concat_shards(stage, machine, session_id)?;
    Ok(ShardFact {
        shard_count,
        concat_bytes: concat.len() as u64,
        concat_sha256: sha256_hex(&concat),
    })
}

/// Does the stage still hold, unmodified, everything `fact` accounts for?
///
/// Coverage is a *prefix* test, not an equality test. Sealed shards are
/// append-only and concatenated in sequence order, so later shards — a later
/// pass, or a pass for a different destination sharing this stage — extend the
/// concatenation without touching the part this fact speaks for. Requiring
/// equality would make one destination's progress look like another's
/// corruption. Anything that shortens or rewrites the covered prefix still
/// fails, which is the property being defended.
fn stage_covers(
    stage: &Path,
    machine: &str,
    session_id: &str,
    fact: &ShardFact,
) -> anyhow::Result<bool> {
    let dir = store::session_shard_dir(stage, machine, session_id);
    if store::sealed_shard_entries(&dir)?.len() < fact.shard_count {
        return Ok(false);
    }
    let concat = store::concat_shards(stage, machine, session_id)?;
    let Ok(covered) = usize::try_from(fact.concat_bytes) else {
        return Ok(false);
    };
    if concat.len() < covered {
        return Ok(false);
    }
    Ok(sha256_hex(&concat[..covered]) == fact.concat_sha256)
}

/// Discharge a stored cursor against the two authorities that can speak for
/// this destination — never against the cursor itself.
fn verify_debt(
    entry: &DebtEntry,
    machine: &str,
    stage: &Path,
    destination: &DestinationView<'_>,
) -> anyhow::Result<DebtVerdict> {
    if entry.machine != machine {
        return Ok(DebtVerdict::Unverifiable(
            "cursor was written for a different machine partition",
        ));
    }
    if stage_covers(stage, machine, &entry.session_id, &entry.shards)? {
        return Ok(DebtVerdict::OwedOnStage);
    }
    let Some(facts) = destination.facts() else {
        return Ok(DebtVerdict::Unverifiable(
            "shards left the stage and the destination archive cannot be consulted",
        ));
    };
    let key = (machine.to_string(), entry.session_id.clone());
    match facts.get(&key) {
        Some(observed) if *observed == entry.shards => Ok(DebtVerdict::SettledInArchive),
        Some(_) => Ok(DebtVerdict::Unverifiable(
            "destination archive holds a different shard set than the cursor claims",
        )),
        None => Ok(DebtVerdict::Unverifiable(
            "destination archive does not hold the shard set the cursor claims",
        )),
    }
}

/// Metadata-only result of one collected source. The source path is represented
/// by a digest so CLI output cannot disclose a harness directory name.
#[derive(Debug, Clone)]
pub struct CollectOutcome {
    pub session_prefix: String,
    pub source_path_sha256: String,
    pub source_bytes: u64,
    pub bytes_read: u64,
    pub prefix_bytes_validated: u64,
    pub lines_written: usize,
    pub shard: Option<String>,
    pub reset: bool,
    pub compressed: bool,
}

/// A source that could not be collected. Its path is represented only by a
/// digest; the command prints counts and this digest, never the source path.
#[derive(Debug, Clone)]
pub struct CollectError {
    pub session_prefix: String,
    pub source_path_sha256: String,
}

/// A state/stage mismatch repaired by forcing this session through the normal
/// reset read path. The reason is intentionally fixed metadata, never source
/// content or a real harness path.
#[derive(Debug, Clone)]
pub struct ReconcileNotice {
    pub session_prefix: String,
    pub reason: &'static str,
}

/// Complete metadata-only summary of one `collect` pass.
#[derive(Debug, Clone, Default)]
pub struct CollectReport {
    /// Digest of the destination this pass read *for*.
    pub destination_id: String,
    /// A pre-ADR-012 per-machine state file was present and deliberately
    /// ignored. Surfaced so the resulting full reread is never silent.
    pub legacy_state_ignored: bool,
    /// Stored cursors that could not prove themselves and were therefore
    /// treated as unread.
    pub unverified_cursors: usize,
    pub scanned_records: usize,
    pub scanned_opencode_records: usize,
    pub scanned_cursor_records: usize,
    pub scanned_grok_records: usize,
    /// Known session candidates and directory entries that the scanner could
    /// not hand over. The latter is not a session count: an inaccessible
    /// subtree may contain any number of sessions.
    pub scanner_unreadable_count: u64,
    /// B90: harnesses that *were* enumerated but whose unreadable tally could
    /// not be taken. `scanner_unreadable_count` sums only the tallies that
    /// exist, so without this the sum reads as complete when it is a floor.
    pub scanner_unreadable_unknown: u64,
    pub scanner_unreadable_entry_count: u64,
    /// B82: harnesses this pass never got to look at (root un-stattable, wrong
    /// type, template unresolvable, confidence `未查明`). They contribute no
    /// records, and without this count a pass that skipped them reads as a
    /// pass that found them empty.
    pub scanner_unlooked_harnesses: usize,
    /// Harnesses with recognised sessions that produced fewer
    /// `SessionRecord`s; these sessions were not consumed by this pass.
    pub archive_gaps: Vec<scanner::ArchiveGap>,
    pub changed_records: usize,
    pub unchanged_records: usize,
    pub reset_records: usize,
    pub shards_written: usize,
    pub lines_written: usize,
    pub source_bytes_read: u64,
    pub delta_bytes_read: u64,
    pub prefix_bytes_validated: u64,
    pub outcomes: Vec<CollectOutcome>,
    pub errors: Vec<CollectError>,
    pub reconciliations: Vec<ReconcileNotice>,
}

/// Default private state directory. It is owned by chat-stasher, never a
/// harness directory and never inside the stage tree that `push` archives.
pub fn default_state_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(xdg).join("chat-stasher").join("state")
    } else {
        crate::config::home_dir()
            .join(".local")
            .join("share")
            .join("chat-stasher")
            .join("state")
    }
}

/// Metadata-only evidence used by `push` before it decides what an empty
/// stage means. A committed read is an offset entry whose durable offset is
/// greater than zero; zero-byte sources do not count as read content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PushStageCheck {
    pub stage_shards: usize,
    pub scanner_records: usize,
    pub scanner_sqlite_sessions: u64,
    pub scanner_sqlite_unknown: usize,
    pub scanner_unknown: usize,
    pub committed_reads: usize,
}

impl PushStageCheck {
    /// An empty stage is a normal no-op only when every metadata source agrees
    /// that there is nothing to archive. Any positive signal is conservative:
    /// the caller must keep the existing failure path instead of creating an
    /// empty snapshot.
    pub fn empty_stage_is_safe(&self) -> bool {
        self.stage_shards == 0
            && self.scanner_records == 0
            && self.scanner_sqlite_sessions == 0
            && self.scanner_sqlite_unknown == 0
            && self.scanner_unknown == 0
            && self.committed_reads == 0
    }
}

/// Collect the metadata needed to distinguish a genuinely new/empty machine
/// from a stage that disappeared after collection. This reads registry
/// metadata and the collector cursor only; it never reads session bodies.
pub fn inspect_stage_for_push(
    config: &Config,
    stage: &Path,
    state_dir: &Path,
    machine: &str,
) -> anyhow::Result<PushStageCheck> {
    let scan = scanner::scan_with_machine(config, machine)
        .context("scan harness sessions for empty-stage guard")?;
    let state = load_state(&state_dir.join(STATE_FILE))?;
    let scanner_sqlite_sessions = scan
        .probes
        .iter()
        .filter(|probe| matches!(probe.state, scanner::ProbeState::FileTarget))
        .filter_map(|probe| probe.record_count)
        .sum();
    let scanner_sqlite_unknown = scan
        .probes
        .iter()
        .filter(|probe| matches!(probe.state, scanner::ProbeState::FileTarget))
        .filter(|probe| probe.record_count.is_none())
        .count();
    let scanner_unknown = scan
        .probes
        .iter()
        .filter(|probe| probe.record_count.is_none())
        .count();
    Ok(PushStageCheck {
        stage_shards: store::sealed_shard_count(stage)?,
        scanner_records: scan.records.len(),
        scanner_sqlite_sessions,
        scanner_sqlite_unknown,
        scanner_unknown,
        // Counted across every destination on purpose: the question this guard
        // answers is "did this machine ever commit a read at all", and any
        // positive signal must keep the conservative failure path.
        committed_reads: state
            .destinations
            .values()
            .flat_map(|dest| dest.files.values())
            .filter(|entry| entry.cursor.offset > 0)
            .count(),
    })
}

/// Scan every registry record and incrementally stage it for `destination`.
pub fn collect(
    config: &Config,
    stage: &Path,
    machine: &str,
    state_dir: &Path,
    bucket_cap: usize,
    destination: &DestinationView<'_>,
) -> anyhow::Result<CollectReport> {
    let scan = scanner::scan_with_machine(config, machine).context("scan harness sessions")?;
    collect_scan_report(&scan, stage, machine, state_dir, bucket_cap, destination)
}

/// Collection entry point split out for tests: the scanner report is supplied
/// by a synthetic registry, while the real CLI always calls [`collect`].
pub fn collect_scan_report(
    scan: &scanner::ScanReport,
    stage: &Path,
    machine: &str,
    state_dir: &Path,
    bucket_cap: usize,
    destination: &DestinationView<'_>,
) -> anyhow::Result<CollectReport> {
    store::assert_stage_writer_audited(store::StageWriter::Collect)?;
    fs::create_dir_all(stage).with_context(|| format!("create stage {}", stage.display()))?;
    fs::create_dir_all(state_dir)
        .with_context(|| format!("create collector state {}", state_dir.display()))?;
    let state_path = state_dir.join(STATE_FILE);
    let state = load_state(&state_path)?;
    let destination_id = destination.id().to_string();
    let mut debts = state
        .destinations
        .get(&destination_id)
        .cloned()
        // reason: 新目标仓库首次归档时 runstate 中无该 destination 记录，欠债集合天然为空集合
        .unwrap_or_default()
        .files;
    let legacy_state_ignored = state_dir.join(LEGACY_STATE_FILE).exists();
    let mut report = CollectReport {
        destination_id: destination_id.clone(),
        legacy_state_ignored,
        scanned_records: scan.records.len(),
        scanned_opencode_records: scan
            .records
            .iter()
            .filter(|record| record.source == crate::models::HarnessSource::OpenCode)
            .count(),
        scanned_cursor_records: scan
            .records
            .iter()
            .filter(|record| record.source == crate::models::HarnessSource::Cursor)
            .count(),
        scanned_grok_records: scan
            .records
            .iter()
            .filter(|record| record.source == crate::models::HarnessSource::Grok)
            .count(),
        scanner_unreadable_count: scan
            .probes
            .iter()
            .filter_map(|probe| probe.unreadable_count)
            .sum(),
        scanner_unreadable_unknown: scan
            .probes
            .iter()
            .filter(|probe| probe.record_count.is_some() && probe.unreadable_count.is_none())
            .count() as u64,
        scanner_unreadable_entry_count: scan
            .probes
            .iter()
            .filter_map(|probe| probe.unreadable_entry_count)
            .sum(),
        scanner_unlooked_harnesses: scan
            .probes
            .iter()
            .filter(|probe| {
                matches!(
                    probe.state,
                    scanner::ProbeState::Indeterminate
                        | scanner::ProbeState::SkipUnascertained
                        | scanner::ProbeState::SkipUnresolvable
                )
            })
            .count(),
        archive_gaps: scan.archive_gaps(),
        ..CollectReport::default()
    };

    if legacy_state_ignored {
        report.reconciliations.push(ReconcileNotice {
            session_prefix: "*".to_string(),
            reason: "pre-destination state file ignored: it belongs to no destination",
        });
    }

    let mut records = scan.records.clone();
    records.sort_by(|a, b| a.absolute_path.cmp(&b.absolute_path));
    for record in records {
        let key = state_key(&record);
        let stored = debts.get(&key).cloned();
        // A stored cursor is a claim, not a fact. It is only reused once it has
        // discharged itself against the stage or against this destination's
        // own archive.
        let mut unverifiable = None;
        if let Some(entry) = stored.as_ref() {
            match verify_debt(entry, machine, stage, destination)? {
                DebtVerdict::OwedOnStage | DebtVerdict::SettledInArchive => {}
                DebtVerdict::Unverifiable(reason) => unverifiable = Some(reason),
            }
        }
        if let Some(reason) = unverifiable {
            report.unverified_cursors += 1;
            report.reconciliations.push(ReconcileNotice {
                session_prefix: id_prefix(&record.id),
                reason,
            });
        }
        let old = stored.as_ref().map(|entry| entry.cursor.clone());
        // The cursor is still handed down so the outcome can report this as a
        // reread rather than a first read; `force_reset` is what actually stops
        // it from being reused.
        match collect_one(
            &record,
            old.as_ref(),
            unverifiable.is_some(),
            stage,
            machine,
            bucket_cap,
        ) {
            Ok(processed) => {
                let outcome = processed.outcome;
                let changed = outcome.bytes_read > 0 || outcome.reset;
                if changed {
                    report.changed_records += 1;
                } else {
                    report.unchanged_records += 1;
                }
                if outcome.reset {
                    report.reset_records += 1;
                }
                report.source_bytes_read += outcome.bytes_read;
                report.delta_bytes_read += outcome.bytes_read;
                report.prefix_bytes_validated += outcome.prefix_bytes_validated;
                if outcome.shard.is_some() {
                    report.shards_written += 1;
                }
                report.lines_written += outcome.lines_written;

                // What the cursor is accountable for only ever grows with a
                // shard. A pass that staged nothing keeps the fact it already
                // proved this run — re-observing here would quietly swap the
                // evidence for whatever the stage looks like now, and on an
                // emptied stage that is the *empty* shard set, which any cursor
                // satisfies for free. That is how a cursor gets believed with
                // nothing behind it.
                let shards = match stored.as_ref() {
                    Some(previous) if outcome.shard.is_none() && unverifiable.is_none() => {
                        previous.shards.clone()
                    }
                    _ => stage_shard_fact(stage, machine, &record.id)?,
                };
                let entry = DebtEntry {
                    machine: machine.to_string(),
                    session_id: record.id.clone(),
                    cursor: processed.state,
                    shards,
                };
                if stored.as_ref() != Some(&entry) {
                    debts.insert(key, entry);
                    save_state(&state_path, &state, &destination_id, &debts)?;
                }
                report.outcomes.push(outcome);
            }
            Err(_) => report.errors.push(CollectError {
                session_prefix: id_prefix(&record.id),
                source_path_sha256: path_digest(&record.absolute_path),
            }),
        }
    }
    Ok(report)
}

#[derive(Debug)]
struct ReadData {
    source_len: u64,
    base_offset: u64,
    bytes: Vec<u8>,
    bytes_read: u64,
    prefix_bytes_validated: u64,
    reset: bool,
}

#[derive(Debug)]
struct Processed {
    outcome: CollectOutcome,
    state: OffsetEntry,
}

fn collect_one(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
) -> anyhow::Result<Processed> {
    if let Some(layout) = record.sqlite_layout {
        process_sqlite(record, layout, old, force_reset, stage, machine, bucket_cap)
    } else if record.compressed || is_zstd_path(&record.absolute_path) {
        Ok(process_compressed(
            record,
            old,
            force_reset,
            stage,
            machine,
            bucket_cap,
        )?)
    } else if is_jsonl_path(&record.absolute_path) {
        Ok(process_jsonl(
            record,
            old,
            force_reset,
            stage,
            machine,
            bucket_cap,
        )?)
    } else {
        Ok(process_whole_file(
            record,
            old,
            force_reset,
            stage,
            machine,
            bucket_cap,
        )?)
    }
}

fn process_sqlite(
    record: &SessionRecord,
    layout: SqliteSessionLayout,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
) -> anyhow::Result<Processed> {
    match layout {
        SqliteSessionLayout::OpenCode => {
            process_opencode(record, old, force_reset, stage, machine, bucket_cap)
        }
        SqliteSessionLayout::CursorLegacy => {
            let session_id = native_session_id(record, "Cursor legacy")?;
            let snapshot = read_cursor_legacy_session(&record.absolute_path, &session_id)
                .map_err(|error| anyhow!("读取 Cursor legacy 会话快照失败: {error}"))?;
            process_sqlite_snapshot(
                record,
                old,
                force_reset,
                stage,
                machine,
                bucket_cap,
                snapshot.cursor,
                snapshot.json_line,
            )
        }
        SqliteSessionLayout::CursorGlobal => {
            let session_id = native_session_id(record, "Cursor global")?;
            let spec = cursor_global_schema();
            let cursor = sqlite_session_cursor(&record.absolute_path, &spec, &session_id)
                .map_err(|error| anyhow!("读取 Cursor 会话游标失败: {error}"))?;
            if !force_reset && old.is_some_and(|entry| entry.opencode.as_ref() == Some(&cursor)) {
                return Ok(unchanged_sqlite(record, old.expect("checked above")));
            }
            let snapshot = read_sqlite_session(&record.absolute_path, &spec, &session_id)
                .map_err(|error| anyhow!("读取 Cursor 会话快照失败: {error}"))?;
            process_sqlite_snapshot(
                record,
                old,
                force_reset,
                stage,
                machine,
                bucket_cap,
                snapshot.cursor,
                snapshot.json_line,
            )
        }
        SqliteSessionLayout::Grok => {
            let session_id = native_session_id(record, "Grok")?;
            let spec = grok_schema();
            let cursor = sqlite_session_cursor(&record.absolute_path, &spec, &session_id)
                .map_err(|error| anyhow!("读取 Grok 会话游标失败: {error}"))?;
            if !force_reset && old.is_some_and(|entry| entry.opencode.as_ref() == Some(&cursor)) {
                return Ok(unchanged_sqlite(record, old.expect("checked above")));
            }
            let snapshot = read_sqlite_session(&record.absolute_path, &spec, &session_id)
                .map_err(|error| anyhow!("读取 Grok 会话快照失败: {error}"))?;
            process_sqlite_snapshot(
                record,
                old,
                force_reset,
                stage,
                machine,
                bucket_cap,
                snapshot.cursor,
                snapshot.json_line,
            )
        }
    }
}

fn process_sqlite_snapshot(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
    cursor: OpenCodeCursor,
    json_line: Vec<u8>,
) -> anyhow::Result<Processed> {
    if !force_reset && old.is_some_and(|entry| entry.opencode.as_ref() == Some(&cursor)) {
        return Ok(unchanged_sqlite(record, old.expect("checked above")));
    }
    let source_bytes = json_line.len() as u64;
    let digest = sha256_hex(&json_line);
    let shard = Some(store::write_sealed_shard_bytes_with_cap(
        store::StageWriter::Collect,
        stage,
        machine,
        &record.id,
        &[json_line],
        bucket_cap,
    )?);
    Ok(Processed {
        state: OffsetEntry {
            offset: source_bytes,
            prefix_len: source_bytes,
            prefix_sha256: digest,
            compressed: false,
            opencode: Some(cursor),
        },
        outcome: CollectOutcome {
            session_prefix: id_prefix(&record.id),
            source_path_sha256: path_digest(&record.absolute_path),
            source_bytes,
            bytes_read: source_bytes,
            prefix_bytes_validated: 0,
            lines_written: 1,
            shard,
            reset: old.is_some() || force_reset,
            compressed: false,
        },
    })
}

fn unchanged_sqlite(record: &SessionRecord, old: &OffsetEntry) -> Processed {
    Processed {
        state: old.clone(),
        outcome: CollectOutcome {
            session_prefix: id_prefix(&record.id),
            source_path_sha256: path_digest(&record.absolute_path),
            source_bytes: old.offset,
            bytes_read: 0,
            prefix_bytes_validated: 0,
            lines_written: 0,
            shard: None,
            reset: false,
            compressed: false,
        },
    }
}

fn native_session_id(record: &SessionRecord, label: &str) -> anyhow::Result<String> {
    record
        .id
        .splitn(3, '.')
        .nth(2)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("invalid {label} session id"))
}

fn process_jsonl(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
) -> anyhow::Result<Processed> {
    let data = read_jsonl_delta(&record.absolute_path, old, force_reset)?;
    let (lines, committed_delta) = complete_lines(&data.bytes);
    let new_offset = data.base_offset + committed_delta as u64;
    let new_state = if lines.is_empty() && !data.reset {
        old.cloned().unwrap_or(OffsetEntry {
            offset: 0,
            prefix_len: 0,
            prefix_sha256: sha256_hex(&[]),
            compressed: false,
            opencode: None,
        })
    } else {
        plain_state(&record.absolute_path, new_offset)?
    };
    let shard = if lines.is_empty() {
        None
    } else {
        Some(store::write_sealed_shard_bytes_with_cap(
            store::StageWriter::Collect,
            stage,
            machine,
            &record.id,
            &lines,
            bucket_cap,
        )?)
    };
    Ok(Processed {
        state: new_state,
        outcome: CollectOutcome {
            session_prefix: id_prefix(&record.id),
            source_path_sha256: path_digest(&record.absolute_path),
            source_bytes: data.source_len,
            bytes_read: data.bytes_read,
            prefix_bytes_validated: data.prefix_bytes_validated,
            lines_written: lines.len(),
            shard,
            reset: data.reset,
            compressed: false,
        },
    })
}

fn process_opencode(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
) -> anyhow::Result<Processed> {
    let session_id = record
        .id
        .splitn(3, '.')
        .nth(2)
        .ok_or_else(|| anyhow!("invalid opencode session id"))?;
    let cursor = opencode_session_cursor(&record.absolute_path, session_id)
        .map_err(|error| anyhow!("读取 opencode 会话游标失败: {error}"))?;
    if !force_reset && old.is_some_and(|entry| entry.opencode.as_ref() == Some(&cursor)) {
        // reason: 前提是 old.is_some() 为 true，此处 unwrap_or(0) 仅为类型解包保底，实际必有 offset
        let source_bytes = old.map(|entry| entry.offset).unwrap_or(0);
        return Ok(Processed {
            state: old.expect("checked above").clone(),
            outcome: CollectOutcome {
                session_prefix: id_prefix(&record.id),
                source_path_sha256: path_digest(&record.absolute_path),
                source_bytes,
                bytes_read: 0,
                prefix_bytes_validated: 0,
                lines_written: 0,
                shard: None,
                reset: false,
                compressed: false,
            },
        });
    }

    let snapshot = read_opencode_session(&record.absolute_path, session_id)
        .map_err(|error| anyhow!("读取 opencode 会话快照失败: {error}"))?;
    let source_bytes = snapshot.json_line.len() as u64;
    let digest = sha256_hex(&snapshot.json_line);
    let lines = vec![snapshot.json_line];
    let shard = Some(store::write_sealed_shard_bytes_with_cap(
        store::StageWriter::Collect,
        stage,
        machine,
        &record.id,
        &lines,
        bucket_cap,
    )?);
    Ok(Processed {
        state: OffsetEntry {
            offset: source_bytes,
            prefix_len: source_bytes,
            prefix_sha256: digest,
            compressed: false,
            opencode: Some(snapshot.cursor),
        },
        outcome: CollectOutcome {
            session_prefix: id_prefix(&record.id),
            source_path_sha256: path_digest(&record.absolute_path),
            source_bytes,
            bytes_read: source_bytes,
            prefix_bytes_validated: 0,
            lines_written: 1,
            shard,
            reset: old.is_some() || force_reset,
            compressed: false,
        },
    })
}

fn process_whole_file(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
) -> anyhow::Result<Processed> {
    let bytes = fs::read(&record.absolute_path)
        .with_context(|| format!("read source bytes ({})", path_digest(&record.absolute_path)))?;
    let source_len = bytes.len() as u64;
    let digest = sha256_hex(&bytes);
    if !force_reset
        && old.is_some_and(|entry| {
            !entry.compressed
                && entry.offset == source_len
                && entry.prefix_len == source_len
                && entry.prefix_sha256 == digest
        })
    {
        return Ok(Processed {
            state: old.expect("checked above").clone(),
            outcome: unchanged_outcome(record, source_len, false),
        });
    }
    let lines = if bytes.is_empty() {
        Vec::new()
    } else {
        vec![bytes]
    };
    let shard = if lines.is_empty() {
        None
    } else {
        Some(store::write_sealed_shard_bytes_with_cap(
            store::StageWriter::Collect,
            stage,
            machine,
            &record.id,
            &lines,
            bucket_cap,
        )?)
    };
    let reset = old.is_some();
    Ok(Processed {
        state: OffsetEntry {
            offset: source_len,
            prefix_len: source_len,
            prefix_sha256: digest,
            compressed: false,
            opencode: None,
        },
        outcome: CollectOutcome {
            session_prefix: id_prefix(&record.id),
            source_path_sha256: path_digest(&record.absolute_path),
            source_bytes: source_len,
            bytes_read: source_len,
            prefix_bytes_validated: 0,
            lines_written: lines.len(),
            shard,
            reset,
            compressed: false,
        },
    })
}

fn process_compressed(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
) -> anyhow::Result<Processed> {
    let compressed = fs::read(&record.absolute_path).with_context(|| {
        format!(
            "read compressed source ({})",
            path_digest(&record.absolute_path)
        )
    })?;
    let source_len = compressed.len() as u64;
    let digest = sha256_hex(&compressed);
    if !force_reset
        && old.is_some_and(|entry| {
            entry.compressed
                && entry.offset == source_len
                && entry.prefix_len == source_len
                && entry.prefix_sha256 == digest
        })
    {
        return Ok(Processed {
            state: old.expect("checked above").clone(),
            outcome: unchanged_outcome(record, source_len, true),
        });
    }
    let decoded = zstd::stream::decode_all(&compressed[..]).context("decompress jsonl.zst")?;
    let (lines, _) = complete_lines(&decoded);
    let shard = if lines.is_empty() {
        None
    } else {
        Some(store::write_sealed_shard_bytes_with_cap(
            store::StageWriter::Collect,
            stage,
            machine,
            &record.id,
            &lines,
            bucket_cap,
        )?)
    };
    let state = if lines.is_empty() {
        OffsetEntry {
            offset: 0,
            prefix_len: 0,
            prefix_sha256: sha256_hex(&[]),
            compressed: true,
            opencode: None,
        }
    } else {
        OffsetEntry {
            offset: source_len,
            prefix_len: source_len,
            prefix_sha256: digest,
            compressed: true,
            opencode: None,
        }
    };
    Ok(Processed {
        state,
        outcome: CollectOutcome {
            session_prefix: id_prefix(&record.id),
            source_path_sha256: path_digest(&record.absolute_path),
            source_bytes: source_len,
            bytes_read: source_len,
            prefix_bytes_validated: 0,
            lines_written: lines.len(),
            shard,
            reset: old.is_some(),
            compressed: true,
        },
    })
}

fn unchanged_outcome(record: &SessionRecord, source_len: u64, compressed: bool) -> CollectOutcome {
    CollectOutcome {
        session_prefix: id_prefix(&record.id),
        source_path_sha256: path_digest(&record.absolute_path),
        source_bytes: source_len,
        bytes_read: 0,
        prefix_bytes_validated: source_len,
        lines_written: 0,
        shard: None,
        reset: false,
        compressed,
    }
}

fn read_jsonl_delta(
    path: &Path,
    old: Option<&OffsetEntry>,
    force_reset: bool,
) -> anyhow::Result<ReadData> {
    for _ in 0..READ_RETRIES {
        let before = fs::metadata(path)?.len();
        let reusable = old.filter(|entry| {
            !force_reset
                && !entry.compressed
                && entry.prefix_len == entry.offset
                && entry.offset <= before
        });
        if let Some(entry) = reusable {
            let prefix = read_range(path, 0, entry.offset)?;
            let expected = entry.prefix_sha256.clone();
            if sha256_hex(&prefix) == expected {
                let delta = read_range(path, entry.offset, before - entry.offset)?;
                let after = fs::metadata(path)?.len();
                if after != before {
                    continue;
                }
                // Re-check the committed prefix after reading the delta. If a
                // writer rewrote the prefix during this pass, retry instead
                // of combining bytes from two different file versions.
                let prefix_after = read_range(path, 0, entry.offset)?;
                if sha256_hex(&prefix_after) != expected {
                    continue;
                }
                return Ok(ReadData {
                    source_len: before,
                    base_offset: entry.offset,
                    bytes: delta,
                    bytes_read: before - entry.offset,
                    prefix_bytes_validated: entry.offset,
                    reset: false,
                });
            }
        }

        // The file is shorter or the committed prefix changed. Read the
        // complete current snapshot and commit only complete lines from it.
        let full = read_range(path, 0, before)?;
        let after = fs::metadata(path)?.len();
        if after != before {
            continue;
        }
        return Ok(ReadData {
            source_len: before,
            base_offset: 0,
            bytes: full,
            bytes_read: before,
            // reason: reusable 为 None 表示没有已校验的前缀，已校验前缀字节数自然为 0
            prefix_bytes_validated: reusable.map(|entry| entry.offset).unwrap_or(0),
            reset: old.is_some(),
        });
    }
    Err(anyhow!(
        "source changed during three consistent-read attempts"
    ))
}

fn complete_lines(bytes: &[u8]) -> (Vec<Vec<u8>>, usize) {
    let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return (Vec::new(), 0);
    };
    let complete_len = last_newline + 1;
    let mut lines: Vec<Vec<u8>> = bytes[..complete_len]
        .split(|byte| *byte == b'\n')
        .map(|line| line.to_vec())
        .collect();
    // `split` returns one empty item after the final delimiter; that delimiter
    // is the boundary, not another record. Empty records between two newlines
    // remain in the vector and are preserved.
    lines.pop();
    (lines, complete_len)
}

fn plain_state(path: &Path, offset: u64) -> anyhow::Result<OffsetEntry> {
    let prefix = read_range(path, 0, offset)?;
    Ok(OffsetEntry {
        offset,
        prefix_len: offset,
        prefix_sha256: sha256_hex(&prefix),
        compressed: false,
        opencode: None,
    })
}

/// Load the debt state.
///
/// A state file that cannot be parsed, or that carries a version this build
/// does not write, is *not* an error and is *not* migrated: it cannot prove
/// anything to any destination, so it is discarded and every source becomes
/// unread. Blocking the run instead would be the one outcome ADR-012 rules
/// out — the archive is the truth, and it is still reachable.
fn load_state(path: &Path) -> anyhow::Result<DebtState> {
    let empty = DebtState {
        version: STATE_VERSION,
        destinations: BTreeMap::new(),
    };
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<DebtState>(&bytes) {
            Ok(state) if state.version == STATE_VERSION => Ok(state),
            _ => Ok(empty),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(empty),
        Err(e) => Err(e).with_context(|| format!("read collector state {}", path.display())),
    }
}

/// Persist `debts` as this destination's slice, leaving every other
/// destination's slice byte-for-byte as it was loaded.
fn save_state(
    path: &Path,
    state: &DebtState,
    destination_id: &str,
    debts: &BTreeMap<String, DebtEntry>,
) -> anyhow::Result<()> {
    let mut out = state.clone();
    out.version = STATE_VERSION;
    out.destinations.insert(
        destination_id.to_string(),
        DestinationDebts {
            files: debts.clone(),
        },
    );
    let tmp = path.with_file_name(format!(".{STATE_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(&out).context("serialise collector state")?;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

fn source_key(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn state_key(record: &SessionRecord) -> String {
    let path = source_key(&record.absolute_path);
    if record.sqlite_layout.is_some() {
        format!("{path}\0{}", record.id)
    } else {
        path
    }
}

fn path_digest(path: &Path) -> String {
    sha256_hex(source_key(path).as_bytes())
}

fn id_prefix(id: &str) -> String {
    crate::id::short_session_id(id)
}

fn is_jsonl_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

fn is_zstd_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zst"))
}

fn read_range(path: &Path, start: u64, len: u64) -> anyhow::Result<Vec<u8>> {
    let len: usize = len
        .try_into()
        .map_err(|_| anyhow!("source range is too large for this process"))?;
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0u8; len];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
