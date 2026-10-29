//! Read-only collection of scanner records into sealed local stage shards.
//!
//! A harness file is never renamed, opened for writing, or marked in the
//! harness directory. File sources keep a byte offset and a SHA-256 of the
//! committed prefix in a state file under chat-stasher's own data directory.
//! The opencode SQLite source keeps a logical high-water cursor instead:
//! session update time, row counts, and the greatest `(time_updated, id)` key
//! for message and part rows. Any mismatch resets the logical read to a
//! complete session export, deliberately preferring a measurable duplicate
//! over a silent omission.
//!
//! # An unterminated final line is in progress, not a change
//!
//! A JSONL source whose last line has no trailing newline is treated as
//! *being written*: only newline-terminated lines are ever sealed, because a
//! torn last record may be half of a write. The unterminated tail stays
//! behind the cursor and is re-read from it on every later pass, so the pass
//! that sees the next newline commits the tail in full — nothing is lost by
//! waiting, and the sealed prefix is never re-staged. A pass that re-read the
//! tail while no newline completed it staged nothing new, so it
//! counts the session as *unchanged* (the classification lives in
//! [`collect_scan_report`]): a source that stops changing must converge to a
//! no-op pass rather than re-flagging the same tail as a change forever.
//!
//! The same rule binds a `.jsonl.zst` source, whose decoded stream can end
//! the same way. Decoding is all-or-nothing, so its cursor is not a byte
//! offset into the compressed file but the observation "this whole source,
//! at this length and digest, was decoded and everything sealable in it was
//! sealed" — written by a pass that found nothing sealable just as by one
//! that sealed lines (`process_compressed`). A byte-identical source is
//! therefore answerable from the remembered digest alone: the next pass is a
//! no-op, not a reset. The first pass after a real change no longer matches
//! the digest, re-decodes, and seals the record the earlier passes held in
//! progress, so convergence never hides growth.
//!
//! The store *and* the per-session cursor answer two different questions, and
//! conflating them cost a full re-export of every session on every store write.
//! Which session changed is decided by that session's own cursor; whether
//! anything in the store moved at all is a separate, cheaper check that may
//! only ever *skip* work (`process_sqlite`).
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
//! full. There is no "trust it for now" branch: a measurable duplicate always
//! beats a silent omission.
//!
//! The one thing that moves under a key is the key. Grok Bot debts are
//! addressed by their session identity rather than their source path
//! ([`state_key`], W986): the same agent re-enters the scan whenever its
//! persistence tree moves, and a path-keyed delivered set made every new
//! path a first read that re-delivered the whole union. A state file from
//! before that change is re-keyed at load — each path entry seeding its
//! session's delivered set — which is a rewrite of *where an entry answers*,
//! never of what it claims: the merged entry still discharges itself against
//! the stage or the destination archive exactly as above, an entry whose
//! digests were never recorded still re-reads in full, and a session no entry
//! names has still never been read for.

use crate::config::Config;
use crate::models::{SessionRecord, SqliteSessionLayout};
use crate::scanner;
use crate::sqlite_probe::{
    cursor_global_schema, grok_schema, opencode_session_cursor, read_cursor_legacy_session,
    read_github_copilot_session, read_hermes_session, read_openclaw_session, read_sqlite_session,
    read_zed_session, sqlite_session_cursor, sqlite_store_fingerprint, zed_schema, OpenCodeCursor,
};
use crate::store;
use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const STATE_VERSION: u32 = 2;
pub(crate) const STATE_FILE: &str = "debts-v2.json";
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
    /// Whole-store fingerprint observed when this cursor was last confirmed.
    ///
    /// It is a **fast path over the store**, never part of one session's
    /// identity: equal fingerprint means nothing in the store moved, so no
    /// session moved and the whole pass can skip the store. It is checked in
    /// `process_sqlite` *before* any per-session query, and a difference falls
    /// through to the per-session comparison in `OpenCodeCursor`.
    ///
    /// `None` means "we have never recorded a fingerprint for this entry" —
    /// which is what a state file written before this field existed looks
    /// like. Unknown is not empty: `None` must never be compared as if it were
    /// a fingerprint value, so the shortcut simply does not apply and the
    /// per-session fields decide. That is what keeps a pre-change state file
    /// from turning every session into "changed".
    #[serde(default)]
    pub store_fingerprint: Option<String>,
    /// Grok Bot rows already delivered to this destination, keyed by the
    /// SHA-256 of their canonical raw JSON — one entry per archived row, so
    /// a sequence observed again with changed content still delivers the new
    /// variant once, and an identical replay delivers nothing.
    #[serde(default)]
    pub grok_bot_row_digests: Option<Vec<String>>,
    /// ADR-061: the per-record delivered set of one snapshot-source session's
    /// increment era, in the W986 shape — one entry per delivered record,
    /// the SHA-256 of the record's own payload serialization (which embeds
    /// the record's id, so an entry identifies both id and content). This
    /// is the entry's bounded cache of the increment map;
    /// [`OpenCodeRecordMapState`] holds the full `{record key → digest}` map
    /// beside the state directory, and the stage can rebuild the map from
    /// the sealed records alone. Kept only while the map holds at most
    /// [`OPENCODE_RECORD_CACHE_CAP`] records; a bigger map stores
    /// [`Self::opencode_map_digest`] instead. `None` on both fields means
    /// the increment era was never entered for this entry (or the state
    /// predates it) — unknown is never "no new messages": the session
    /// re-exports in full once, which re-derives the map (D2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode_record_digests: Option<Vec<String>>,
    /// The overall digest of the increment map once it passes
    /// [`OPENCODE_RECORD_CACHE_CAP`] records, replacing the per-record list
    /// so one long session's debt stays bounded in the state file. The map
    /// is rebuilt from the staged records (or a fresh complete observation)
    /// when it disagrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode_map_digest: Option<String>,
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
    /// Exact per-shard identities when every shard has a canonical sequence.
    /// Older persisted debt records omit this field and remain valid for exact
    /// aggregate comparisons only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shard_identities: Vec<ShardFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardFingerprint {
    pub sequence: u64,
    pub bytes: u64,
    pub sha256: String,
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
pub(crate) struct DestinationDebts {
    pub(crate) files: BTreeMap<String, DebtEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct DebtState {
    version: u32,
    /// Keyed by [`destination_id`]. A destination that is not in this map has
    /// never been read for, so every source is unread for it.
    pub(crate) destinations: BTreeMap<String, DestinationDebts>,
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

/// What a destination's archive can be shown to hold for machine logs, keyed
/// by `(machine, harness, log_id)` — the machine-log identity (ADR-053 D1/D4).
/// A separate map from [`ArchiveFacts`] so nothing that walks session facts can
/// see a machine log, and vice versa.
pub type MachineLogFacts = BTreeMap<(String, String, String), ShardFact>;

/// Fold a cross-machine read-back into the machine-log facts a stored cursor
/// can be checked against. Same reshaping rule as
/// [`archive_facts_from_readback`], including its exactness condition: per-shard
/// identities are kept only when every generation has a canonical sequence
/// number, so a legacy/ambiguous name degrades to an aggregate comparison
/// rather than to an invented sequence.
pub fn machine_log_facts_from_readback(report: &crate::readback::ReadAllReport) -> MachineLogFacts {
    report
        .machine_logs
        .iter()
        .map(|log| {
            (
                (log.machine.clone(), log.harness.clone(), log.log_id.clone()),
                ShardFact {
                    shard_count: log.shard_count,
                    concat_bytes: log.concat_bytes,
                    concat_sha256: log.sha256.clone(),
                    shard_identities: if log.shard_sequences.len() == log.shard_sha256.len()
                        && log.shard_sequences.len() == log.shard_bytes.len()
                        && log.shard_sequences.iter().all(Option::is_some)
                    {
                        log.shard_sequences
                            .iter()
                            .zip(&log.shard_bytes)
                            .zip(&log.shard_sha256)
                            .map(|((sequence, bytes), sha256)| ShardFingerprint {
                                sequence: sequence.expect("checked above"),
                                bytes: *bytes,
                                sha256: sha256.clone(),
                            })
                            .collect()
                    } else {
                        Vec::new()
                    },
                },
            )
        })
        .collect()
}

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
                    shard_identities: if session.shard_sequences.len() == session.shard_sha256.len()
                        && session.shard_sequences.len() == session.shard_bytes.len()
                        && session.shard_sequences.iter().all(Option::is_some)
                    {
                        session
                            .shard_sequences
                            .iter()
                            .zip(&session.shard_bytes)
                            .zip(&session.shard_sha256)
                            .map(|((sequence, bytes), sha256)| ShardFingerprint {
                                sequence: sequence.expect("checked above"),
                                bytes: *bytes,
                                sha256: sha256.clone(),
                            })
                            .collect()
                    } else {
                        Vec::new()
                    },
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
    probe: Box<dyn Fn(&BTreeSet<(String, String)>) -> anyhow::Result<ArchiveFacts> + 'a>,
    cache: OnceCell<Option<ArchiveFacts>>,
    /// A **second**, machine-log-shaped question for the same archive
    /// (ADR-053 D4). It is separate because the two facts are keyed
    /// differently — a session by `(machine, session id)`, a machine log by
    /// `(machine, harness, log id)` — and because the session probe's answer is
    /// scoped by the session `wanted` set while a machine-log answer never is.
    /// A view built without one (every fixture, and every destination that
    /// cannot answer) simply has no machine-log facts: its machine-log cursors
    /// then fall back to the local proof, and an unproved cursor is reread.
    machine_log_probe: Option<Box<dyn Fn() -> anyhow::Result<MachineLogFacts> + 'a>>,
    machine_log_cache: OnceCell<Option<MachineLogFacts>>,
}

impl<'a> DestinationView<'a> {
    pub fn new(
        id: impl Into<String>,
        probe: impl Fn(&BTreeSet<(String, String)>) -> anyhow::Result<ArchiveFacts> + 'a,
    ) -> Self {
        DestinationView {
            id: id.into(),
            probe: Box::new(probe),
            cache: OnceCell::new(),
            machine_log_probe: None,
            machine_log_cache: OnceCell::new(),
        }
    }

    /// A destination whose archive can also be asked for its machine-log
    /// generations. The machine-log probe takes no argument: the read reports
    /// every generation it sees, so there is no wanted-set to go stale.
    pub fn with_machine_logs(
        id: impl Into<String>,
        probe: impl Fn(&BTreeSet<(String, String)>) -> anyhow::Result<ArchiveFacts> + 'a,
        machine_log_probe: impl Fn() -> anyhow::Result<MachineLogFacts> + 'a,
    ) -> Self {
        DestinationView {
            id: id.into(),
            probe: Box::new(probe),
            cache: OnceCell::new(),
            machine_log_probe: Some(Box::new(machine_log_probe)),
            machine_log_cache: OnceCell::new(),
        }
    }

    /// A destination whose archive cannot be consulted at all.
    pub fn unreachable(id: impl Into<String>) -> Self {
        DestinationView::new(id, |_| Err(anyhow!("destination archive is not reachable")))
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Probe the archive for `wanted`, once.
    ///
    /// The result is cached for the life of this view, and `wanted` is honoured
    /// only on the **first** call: any later call returns that first result
    /// whatever set it is given. Callers must therefore collect the complete set
    /// of sessions that need remote proof before calling this, and call it once.
    /// A second call site with a different set would read a stale answer, find
    /// its session absent, and turn that absence into `Unverifiable` — which is
    /// how already-archived content ends up re-staged.
    pub fn facts(&self, wanted: &BTreeSet<(String, String)>) -> Option<&ArchiveFacts> {
        self.cache
            .get_or_init(|| (self.probe)(wanted).ok())
            .as_ref()
    }

    /// Machine-log shard sets the destination's archive holds, or `None` when
    /// this destination cannot answer that question.
    ///
    /// Asked only for the logs whose stored cursor could not be proved locally,
    /// and cached for the life of the view: a pass whose machine logs are all
    /// still owed on the stage never opens the archive a second time.
    pub fn machine_log_facts(&self) -> Option<&MachineLogFacts> {
        let probe = self.machine_log_probe.as_ref()?;
        self.machine_log_cache.get_or_init(|| probe().ok()).as_ref()
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
    /// The debt was settled and reclaimed: the stage body was deleted after
    /// being proven, and the local manifest records the matching fact triple.
    SettledReclaimed,
    Unverifiable(&'static str),
}

/// What the stage already holds for one session, in the exact framing the
/// sealers write: each line followed by one newline. Empty when nothing is
/// sealed — which is not the same as a session whose content is empty, so
/// callers must keep the two apart by checking the shard count, not this
/// length.
fn stage_sealed_shards(
    stage: &Path,
    machine: &str,
    session_id: &str,
) -> anyhow::Result<Vec<Vec<u8>>> {
    let dir = store::session_shard_dir(stage, machine, session_id);
    let mut entries = store::sealed_shard_entries(&dir)?;
    entries.sort_by_key(|(seq, _)| *seq);
    entries
        .into_iter()
        .map(|(_, path)| fs::read(path).map_err(Into::into))
        .collect()
}

/// Frame `lines` the way [`store::write_sealed_shard_bytes_with_cap`] does.
fn seal_framed(lines: &[Vec<u8>]) -> Vec<u8> {
    let mut raw = Vec::new();
    for line in lines {
        raw.extend_from_slice(line);
        raw.push(b'\n');
    }
    raw
}

/// Does the stage already hold exactly this export, for a destination that has
/// no cursor of its own?
///
/// A SQLite session cannot be handed a stage-derived cursor the way a file
/// source can: its cursor is a logical `(time_updated, id)` key, not a byte
/// offset, so the stage says nothing about it until the session has been
/// re-exported. Once it has, the comparison is exact — the sealer writes that
/// export as one line, so the stage's concatenation either equals the framed
/// line or it does not. A match means this destination is already owed
/// nothing, and sealing again would store the conversation a second time.
fn stage_holds_this_export(
    stage: &Path,
    machine: &str,
    session_id: &str,
    json_line: &[u8],
) -> anyhow::Result<bool> {
    let sealed = stage_sealed_shards(stage, machine, session_id)?;
    let mut framed = json_line.to_vec();
    framed.push(b'\n');
    Ok(stage_tail_is(&sealed, &framed))
}

/// ADR-020 role C / SG-2b / SG-2c: when the stage body has been reclaimed, does the
/// retained manifest summary prove that the archive already holds this exact
/// sealed framed payload?
fn manifest_holds_this_framed(
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
    session_id: &str,
    framed: &[u8],
) -> bool {
    let Some(facts) = manifest_facts else {
        return false;
    };
    let Some(fact) = facts.get(session_id) else {
        return false;
    };
    fact.concat_bytes == framed.len() as u64 && fact.concat_sha256 == sha256_hex(framed)
}

/// Check if the retained manifest summary holds this exact JSON line export.
fn manifest_holds_this_export(
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
    session_id: &str,
    json_line: &[u8],
) -> bool {
    let mut framed = json_line.to_vec();
    framed.push(b'\n');
    manifest_holds_this_framed(manifest_facts, session_id, &framed)
}

/// Can a byte-identical match against a baseline (the stage's held bodies,
/// a retained manifest fact) be trusted to say "this is the observation the
/// baseline already holds", so the pass may read in full and then seal
/// nothing — instead of capturing the bytes again with their multiplicity?
///
/// It is the same rule the cursor-recovery arm in `collect_scan_report`
/// (`None if !record.provenance.is_empty() && !record.movable_root`) and
/// the writer choice in `process_jsonl` apply, stated once for the
/// baseline-suppression sites that consult a fact instead of a prefix:
///
/// - Through a **movable root** byte equality is identity: a store that
///   MOVES its files between the roots of one registry entry makes a
///   same-id same-content discovery at a new path the same file by its
///   own rules (W1007, ADR-059 draft D4), so a baseline holding the bytes
///   proves the observation is already archived.
/// - A record with **no provenance rows** carries no measured observation
///   that a second root could out-run: byte equality then has no
///   cross-root identity question to answer, and the baseline match is
///   conclusive the way the stage-derived cursor recovery already treats
///   it for these records.
/// - A fresh **provenance-bearing** discovery at a **non-movable** root is
///   ambiguous even when every byte matches the baseline: one native UUID
///   may appear in multiple source roots, and the match can be a replay
///   of the same bytes or a genuinely repeated identical event. Suppressing
///   the seal would drop that turn — a silent omission — so such a record
///   must fall through to the multiplicity-preserving writer.
///
/// A stored cursor is not a baseline in this sense and needs no gate here:
/// it is identity-bearing through the debt stream it was verified against,
/// which is why the `old.is_some_and(...)` digest comparisons above each
/// manifest gate stay ungated.
fn byte_equality_establishes_event_identity(record: &SessionRecord) -> bool {
    record.movable_root || record.provenance.is_empty()
}

/// Is `frame` one sealed shard, or an exact repeated-shard suffix, of the
/// body sealed so far?
///
/// The snapshot shapes — a whole file, a compressed export, a SQLite session —
/// have no incremental cursor. Each pass seals the whole current body as one
/// shard, so the stage accumulates the versions it has seen and the newest one
/// is its tail. The question for a destination with no cursor of its own is
/// therefore whether the body this pass is about to seal is already there. A
/// normal snapshot is one shard, even when it contains several lines, so a
/// match against one complete tail shard is conclusive. The old defect could
/// also split one export across shards and append the same shard sequence a
/// second time; that exact repeated sequence is conclusive too. A run of
/// distinct earlier snapshots is not: if shards `A`, `B` are followed by an
/// export `A\nB\n`, their concatenation must not masquerade as one sealed
/// export. After a compressed export `A\nB\n` shrinks to `B\n`, the new export
/// is a byte suffix of the old one, but it is a new snapshot and must be sealed.
fn stage_tail_is(shards: &[Vec<u8>], frame: &[u8]) -> bool {
    if frame.is_empty() {
        return false;
    }
    let mut remaining = frame.len();
    for start in (0..shards.len()).rev() {
        let shard = &shards[start];
        if shard.len() > remaining || frame[remaining - shard.len()..remaining] != shard[..] {
            return false;
        }
        remaining -= shard.len();
        if remaining == 0 {
            let shard_count = shards.len() - start;
            return shard_count == 1
                || (start >= shard_count && shards[start - shard_count..start] == shards[start..]);
        }
    }
    false
}

/// The longest source line prefix present contiguously in the stage.
///
/// Old destination passes could seal the same source lines again in a new
/// shard. Shard concatenation therefore is not necessarily a source prefix:
/// `A,A,B` and `A,B,A,B` both hold a contiguous copy of the complete `A,B`
/// source. Matching ordered lines preserves their meaning and multiplicity;
/// source `A,A,B` still needs two adjacent `A` records before `B` is covered.
fn covered_source_prefix(stage: &[u8], source: &[u8]) -> u64 {
    let (stage_lines, _) = complete_lines(stage);
    let (source_lines, source_complete_len) = complete_lines(source);
    if source_lines.is_empty() {
        return 0;
    }

    // KMP over complete lines finds the longest source prefix occurring as a
    // contiguous run anywhere in the stage, in linear time even for large
    // transcripts. Comparing line bytes also prevents a match from starting
    // midway through a record.
    let mut prefix = vec![0usize; source_lines.len()];
    for index in 1..source_lines.len() {
        let mut matched = prefix[index - 1];
        while matched > 0 && source_lines[index] != source_lines[matched] {
            matched = prefix[matched - 1];
        }
        if source_lines[index] == source_lines[matched] {
            matched += 1;
        }
        prefix[index] = matched;
    }

    let mut matched = 0usize;
    let mut best = 0usize;
    for line in stage_lines {
        while matched > 0 && line != source_lines[matched] {
            matched = prefix[matched - 1];
        }
        if line == source_lines[matched] {
            matched += 1;
            best = best.max(matched);
            if matched == source_lines.len() {
                return source_complete_len as u64;
            }
        }
    }

    let covered_len: usize = source_lines[..best].iter().map(|line| line.len() + 1).sum();
    covered_len.min(source_complete_len) as u64
}

/// The stage's own sealed shards, expressed as a cursor, when they provably
/// hold the beginning of what this pass is about to read.
///
/// # The defect this exists to stop
///
/// The debt state is keyed by destination. A destination with no entry has
/// never been read for, so its pass legitimately starts every source at offset
/// zero — but the collector used to *seal what that reread produced*, even when
/// the stage already held exactly those bytes. Every additional destination
/// therefore added one more full copy of every session: `read` returned the
/// conversation twice, `export` wrote it twice, the archive-derived activity
/// index doubled `line_count`, and L3 said `OK` because it derived its
/// expectation from the same doubled tree. `setup` step 4 and `dest-init` are
/// exactly this pass, so every user who finished the wizard had it.
///
/// A destination must be given the shards the stage holds, never handed a
/// second seal of them.
///
/// # Why this is not a write-time "have I seen these bytes" guard
///
/// Repeated content is real: a harness may append bytes identical to bytes
/// already sealed (the source `a\n` growing to `a\na\n`). A guard that refused
/// to write because the new bytes matched something already present would drop
/// that turn — a silent omission, the one outcome this repository never trades
/// away. So the decision is made *before* the read, as a cursor the ordinary
/// read path then has to prove: the candidate prefix is handed to
/// [`read_jsonl_delta`] as if it were a stored one, and the same SHA-256
/// comparison guards it. A stage that does not match the source yields no
/// cursor, and the pass falls back to the full read it did before.
///
/// `None` means "no evidence", never "empty", and every case that returns it
/// falls back to the pre-existing full read rather than assuming the stage is
/// up to date: nothing sealed at all, a logical (SQLite) cursor that cannot be
/// spelled as a file offset, and a compressed or whole-file source the stage
/// does not already cover in full. (An append-only jsonl source is the one case
/// where partial coverage is exactly the information wanted.) A SQLite
/// session's reuse is judged at its own call site instead, against the export
/// it has just produced.
///
/// # A stage that holds the body more than once
///
/// The defect above ran on real machines, so this function has to read a stage
/// it has already damaged: an affected session's body is there twice (or behind
/// older versions of itself), which is not "no evidence" — it is *more* of the
/// same evidence. Reading the stage's length as the position would put the pass
/// past the end of an unchanged source (no cursor, so the whole file is read
/// and sealed again: a third copy, and a fourth the next time), so the position
/// is instead the longest source prefix whose complete lines the stage already
/// holds contiguously and in order. The whole-file and compressed shapes have
/// no such incremental cursor: what
/// they can ask is what the stage sealed *last*, and that is
/// [`stage_tail_is`]. A SQLite session asks the same question at its own call
/// site, against the export the pass has just produced.
fn stage_prefix_entry(
    record: &SessionRecord,
    stage: &Path,
    machine: &str,
    counters: &mut CollectCounters,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
) -> anyhow::Result<Option<OffsetEntry>> {
    // A SQLite session's cursor is logical, not a file offset, so the stage
    // cannot be turned into one without re-exporting the session. Those paths
    // judge the reuse themselves, against the export they just produced.
    if record.sqlite_layout.is_some() {
        return Ok(None);
    }
    let sealed_shards = stage_sealed_shards(stage, machine, &record.id)?;
    if sealed_shards.is_empty() {
        // A reclaimed session's shards are gone and a just-scanned one has
        // none yet — either way there is no stage prefix to reuse, so with
        // no manifest fact the caller reads its source in full. A retained
        // manifest fact (ADR-020 role C) can still prove the archive holds
        // the whole body, and is consulted per source shape below.
        if let Some(fact) = manifest_facts.and_then(|m| m.get(&record.id)) {
            if record.compressed || is_zstd_path(&record.absolute_path) {
                let compressed = fs::read(&record.absolute_path).with_context(|| {
                    format!(
                        "read compressed source ({})",
                        path_digest(&record.absolute_path)
                    )
                })?;
                let decoded =
                    zstd::stream::decode_all(&compressed[..]).context("decompress jsonl.zst")?;
                let (lines, _) = complete_lines(&decoded);
                let framed = seal_framed(&lines);
                if !lines.is_empty()
                    && fact.concat_bytes == framed.len() as u64
                    && fact.concat_sha256 == sha256_hex(&framed)
                {
                    let len = compressed.len() as u64;
                    return Ok(Some(OffsetEntry {
                        offset: len,
                        prefix_len: len,
                        prefix_sha256: sha256_hex(&compressed),
                        compressed: true,
                        opencode: None,
                        store_fingerprint: None,
                        grok_bot_row_digests: None,
                        opencode_record_digests: None,
                        opencode_map_digest: None,
                    }));
                }
            } else if is_jsonl_path(&record.absolute_path) {
                let source_len = fs::metadata(&record.absolute_path)
                    .with_context(|| {
                        format!("stat source ({})", path_digest(&record.absolute_path))
                    })?
                    .len();
                // W880: one source file stat'ed.
                counters.files_statted += 1;
                if fact.concat_bytes > 0 && source_len >= fact.concat_bytes {
                    let prefix = read_range(&record.absolute_path, 0, fact.concat_bytes)
                        .with_context(|| {
                            format!(
                                "read source prefix ({})",
                                path_digest(&record.absolute_path)
                            )
                        })?;
                    if sha256_hex(&prefix) == fact.concat_sha256 {
                        let offset = fact.concat_bytes;
                        return Ok(Some(OffsetEntry {
                            offset,
                            prefix_len: offset,
                            prefix_sha256: fact.concat_sha256.clone(),
                            compressed: false,
                            opencode: None,
                            store_fingerprint: None,
                            grok_bot_row_digests: None,
                            opencode_record_digests: None,
                            opencode_map_digest: None,
                        }));
                    }
                }
            } else {
                let bytes = fs::read(&record.absolute_path).with_context(|| {
                    format!("read source bytes ({})", path_digest(&record.absolute_path))
                })?;
                let framed = seal_framed(std::slice::from_ref(&bytes));
                if !bytes.is_empty()
                    && fact.concat_bytes == framed.len() as u64
                    && fact.concat_sha256 == sha256_hex(&framed)
                {
                    let len = bytes.len() as u64;
                    return Ok(Some(OffsetEntry {
                        offset: len,
                        prefix_len: len,
                        prefix_sha256: sha256_hex(&bytes),
                        compressed: false,
                        opencode: None,
                        store_fingerprint: None,
                        grok_bot_row_digests: None,
                        opencode_record_digests: None,
                        opencode_map_digest: None,
                    }));
                }
            }
        }
        return Ok(None);
    }

    if record.compressed || is_zstd_path(&record.absolute_path) {
        // The entry's offsets are over the *compressed* bytes while the stage
        // holds the decoded lines, so no prefix of the file can be spelled as
        // this cursor. Reuse is claimed only for the whole content: the entry
        // then says "the stage already holds exactly this export", which is
        // what `process_compressed` re-checks by hash.
        let compressed = fs::read(&record.absolute_path).with_context(|| {
            format!(
                "read compressed source ({})",
                path_digest(&record.absolute_path)
            )
        })?;
        let decoded = zstd::stream::decode_all(&compressed[..]).context("decompress jsonl.zst")?;
        let (lines, _) = complete_lines(&decoded);
        if lines.is_empty() || !stage_tail_is(&sealed_shards, &seal_framed(&lines)) {
            return Ok(None);
        }
        let len = compressed.len() as u64;
        return Ok(Some(OffsetEntry {
            offset: len,
            prefix_len: len,
            prefix_sha256: sha256_hex(&compressed),
            compressed: true,
            opencode: None,
            store_fingerprint: None,
            grok_bot_row_digests: None,
            opencode_record_digests: None,
            opencode_map_digest: None,
        }));
    }

    if is_jsonl_path(&record.absolute_path) {
        let sealed = sealed_shards.iter().flatten().copied().collect::<Vec<_>>();
        return jsonl_prefix_entry_from_shards(&record.absolute_path, &sealed, counters);
    }

    // Whole-file sources have no incremental model: `process_whole_file` seals
    // the file in one shard or not at all, so the reuse means the stage's tail
    // is already this file — which it is when the file has not changed since it
    // was sealed last, and also when the defect sealed it twice.
    let bytes = fs::read(&record.absolute_path)
        .with_context(|| format!("read source bytes ({})", path_digest(&record.absolute_path)))?;
    if bytes.is_empty()
        || !stage_tail_is(&sealed_shards, &seal_framed(std::slice::from_ref(&bytes)))
    {
        return Ok(None);
    }
    // The sealer appends a newline after the single "line" it is given, so the
    // sealed length is one byte longer than the file. The cursor is over the
    // *file*, which is what `process_whole_file` re-checks by hash.
    let len = bytes.len() as u64;
    Ok(Some(OffsetEntry {
        offset: len,
        prefix_len: len,
        prefix_sha256: sha256_hex(&bytes),
        compressed: false,
        opencode: None,
        store_fingerprint: None,
        grok_bot_row_digests: None,
        opencode_record_digests: None,
        opencode_map_digest: None,
    }))
}

/// Derive a destination's starting cursor for a plain JSONL source from the
/// shard bodies a stage already holds.
///
/// The stage may hold duplicated lines in a different arrangement than the
/// source. The cursor is therefore derived from a contiguous run of complete
/// source lines, so duplicate arrangements are recognized without treating
/// reordered or missing repeated lines as covered. `read_jsonl_delta` re-reads
/// and hashes the claimed source prefix before it accepts this cursor.
///
/// Shared by a session transcript and a machine log: both are plain JSONL with
/// a byte-offset cursor, and both ask this question when a destination has no
/// stored cursor of its own.
fn jsonl_prefix_entry_from_shards(
    path: &Path,
    sealed: &[u8],
    counters: &mut CollectCounters,
) -> anyhow::Result<Option<OffsetEntry>> {
    let source_len = fs::metadata(path)
        .with_context(|| format!("stat source ({})", path_digest(path)))?
        .len();
    // W880: one source file stat'ed.
    counters.files_statted += 1;
    // No covered prefix can be longer than the stage body, so cap this probe
    // there instead of reading an arbitrarily larger source.
    let source = read_range(path, 0, source_len.min(sealed.len() as u64))
        .with_context(|| format!("read source prefix ({})", path_digest(path)))?;
    let offset = covered_source_prefix(sealed, &source);
    if offset == 0 {
        return Ok(None);
    }
    Ok(Some(OffsetEntry {
        offset,
        prefix_len: offset,
        prefix_sha256: sha256_hex(&source[..offset as usize]),
        compressed: false,
        opencode: None,
        store_fingerprint: None,
        grok_bot_row_digests: None,
        opencode_record_digests: None,
        opencode_map_digest: None,
    }))
}

/// [`jsonl_prefix_entry_from_shards`] against a shard directory: what a
/// destination with no stored cursor starts from when the stage already holds
/// this source's generations. Used for machine logs, whose directory is not a
/// session's.
fn jsonl_stage_prefix_entry(
    path: &Path,
    dir: &Path,
    counters: &mut CollectCounters,
) -> anyhow::Result<Option<OffsetEntry>> {
    let mut entries = store::sealed_shard_entries(dir)?;
    entries.sort_by_key(|(sequence, _)| *sequence);
    if entries.is_empty() {
        return Ok(None);
    }
    let mut sealed = Vec::new();
    for (_, path) in entries {
        sealed.extend_from_slice(&fs::read(path)?);
    }
    jsonl_prefix_entry_from_shards(path, &sealed, counters)
}

/// The sealed byte length the stage holds for one session, from directory
/// metadata only — the sum of its sealed shard file sizes — so an
/// unchanged pass costs a directory walk, never a body read.
fn session_stage_len(stage: &Path, machine: &str, session_id: &str) -> anyhow::Result<u64> {
    let dir = store::session_shard_dir(stage, machine, session_id);
    let entries = store::sealed_shard_entries(&dir)?;
    let mut total = 0u64;
    for (_, path) in entries {
        total = total
            .checked_add(fs::metadata(&path)?.len())
            .ok_or_else(|| anyhow!("sealed shard sizes overflowed"))?;
    }
    Ok(total)
}

/// The cursor a record with a stored debt should read from (W1007).
///
/// Normally that is the stored cursor itself, and nothing here changes that.
/// The one exception is a plain JSONL source whose session's stage body is
/// LONGER than the cursor's position: the stage then holds bytes this
/// cursor never accounted for — delivered through a SIBLING source root of a
/// movable store (codex `thread/archive` moves the rollout between roots, so
/// the other root's pass seals the appends first) or by a pass whose cursor
/// save was lost after sealing. Reading from the stored position would seal
/// those bytes a second time — the stage-in-twice shape [`stage_prefix_entry`]
/// exists to repair — so the pass instead asks the stage what it truest
/// holds: the covered-prefix position [`stage_prefix_entry`] derives, which
/// never exceeds the bytes the archive verifiably holds for this session,
/// and which the ordinary read path re-validates against the source exactly
/// as if it were a stored cursor.
///
/// The trigger is metadata-only, so an ordinary append (stage body length
/// exactly the cursor's position) never pays for the stage's body. The
/// advance never loses a legitimate repeated turn either: the covered
/// prefix is a prefix of the source, and anything after it inside the file
/// is still read as a delta and sealed with its multiplicity intact.
fn cursor_or_stage_covered_position(
    record: &SessionRecord,
    entry: &DebtEntry,
    verified: bool,
    stage: &Path,
    machine: &str,
    counters: &mut CollectCounters,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
) -> anyhow::Result<OffsetEntry> {
    // An unverifiable stored cursor is handed down unchanged: `force_reset`
    // is what stops its reuse, and spending the stage's body read under a
    // flag that discards the result would be pure cost.
    if !verified {
        return Ok(entry.cursor.clone());
    }
    // Only the plain JSONL read has a byte-offset cursor whose position a
    // sibling root can out-run. A SQLite session's cursor is logical (its
    // own call site judges reuse), and the compressed and whole-file
    // shapes have no partial cursor at all — their reuse question is the
    // whole-content one their own read paths already answer.
    if record.sqlite_layout.is_some()
        || record.compressed
        || is_zstd_path(&record.absolute_path)
        || !is_jsonl_path(&record.absolute_path)
    {
        return Ok(entry.cursor.clone());
    }
    let stage_len = session_stage_len(stage, machine, &record.id)?;
    if stage_len <= entry.cursor.offset {
        if let Some(fact) = manifest_facts.and_then(|m| m.get(&record.id)) {
            if fact.concat_bytes > entry.cursor.offset {
                if let Some(covered) =
                    stage_prefix_entry(record, stage, machine, counters, manifest_facts)?
                {
                    if covered.offset > entry.cursor.offset {
                        return Ok(covered);
                    }
                }
            }
        }
        return Ok(entry.cursor.clone());
    }
    Ok(
        match stage_prefix_entry(record, stage, machine, counters, manifest_facts)? {
            Some(covered) if covered.offset > entry.cursor.offset => covered,
            _ => entry.cursor.clone(),
        },
    )
}

/// Observe what the stage currently holds for one session.
pub(crate) fn stage_shard_fact(
    stage: &Path,
    machine: &str,
    session_id: &str,
) -> anyhow::Result<ShardFact> {
    shard_fact_in_dir(&store::session_shard_dir(stage, machine, session_id))
}

/// Observe what the stage currently holds in one shard directory.
///
/// A machine log's generations are observed by exactly this function
/// (ADR-053 D4): its directory is the only difference from a session, so its
/// cursor is accountable for the same `(shard_count, concat_bytes,
/// concat_sha256)` triple, with the same per-shard fingerprints.
fn shard_fact_in_dir(dir: &Path) -> anyhow::Result<ShardFact> {
    let mut entries = store::sealed_shard_entries(dir)?;
    entries.sort_by_key(|(sequence, _)| *sequence);
    let shard_count = entries.len();
    let concat = store::concat_shards_in_dir(dir)?;
    Ok(ShardFact {
        shard_count,
        concat_bytes: concat.len() as u64,
        concat_sha256: sha256_hex(&concat),
        shard_identities: entries
            .into_iter()
            .map(|(sequence, path)| {
                let bytes = fs::read(path)?;
                Ok(ShardFingerprint {
                    sequence,
                    bytes: bytes.len() as u64,
                    sha256: sha256_hex(&bytes),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?,
    })
}

/// Read the local manifest baseline for one machine into a session lookup map,
/// if the manifest exists and is valid. Missing or corrupt files yield `None`.
pub(crate) fn load_local_manifest_facts(
    stage: &Path,
    machine: &str,
) -> Option<BTreeMap<String, ShardFact>> {
    match crate::manifest::read_manifest(stage, machine) {
        Ok(crate::manifest::ManifestFileState::Loaded(rows)) => {
            let mut map = BTreeMap::new();
            for row in rows {
                if row.machine == machine {
                    map.insert(
                        row.session_id,
                        ShardFact {
                            shard_count: row.shard_count,
                            concat_bytes: row.concat_bytes,
                            concat_sha256: row.concat_sha256,
                            shard_identities: Vec::new(),
                        },
                    );
                }
            }
            Some(map)
        }
        Ok(
            crate::manifest::ManifestFileState::Missing | crate::manifest::ManifestFileState::Empty,
        ) => None,
        Err(_) => None,
    }
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
    dir_covers(&store::session_shard_dir(stage, machine, session_id), fact)
}

/// [`stage_covers`] for an already-resolved shard directory, so a machine log's
/// cursor is proved by the same prefix test a session's is.
fn dir_covers(dir: &Path, fact: &ShardFact) -> anyhow::Result<bool> {
    if store::sealed_shard_entries(dir)?.len() < fact.shard_count {
        return Ok(false);
    }
    let concat = store::concat_shards_in_dir(dir)?;
    let Ok(covered) = usize::try_from(fact.concat_bytes) else {
        return Ok(false);
    };
    if concat.len() < covered {
        return Ok(false);
    }
    Ok(sha256_hex(&concat[..covered]) == fact.concat_sha256)
}

/// Check if a stored debt can be discharged locally without consulting the destination archive.
///
/// A debt is proved locally if:
/// - the machine partition does not match (fails locally as Unverifiable);
/// - the stage still covers the claimed shard prefix (OwedOnStage);
/// - the stage body was reclaimed, its shard directory is empty, and the local manifest
///   matches the claimed shard triple (SettledReclaimed).
///
/// Returns `Some(verdict)` if the debt was settled locally, or `None` if remote archive
/// verification is required.
pub(crate) fn debt_settled_locally(
    entry: &DebtEntry,
    machine: &str,
    stage: &Path,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
) -> anyhow::Result<Option<DebtVerdict>> {
    if entry.machine != machine {
        return Ok(Some(DebtVerdict::Unverifiable(
            "cursor was written for a different machine partition",
        )));
    }
    if stage_covers(stage, machine, &entry.session_id, &entry.shards)? {
        return Ok(Some(DebtVerdict::OwedOnStage));
    }
    let dir = store::session_shard_dir(stage, machine, &entry.session_id);
    if store::sealed_shard_entries(&dir)?.is_empty() {
        if let Some(facts) = manifest_facts {
            if facts
                .get(&entry.session_id)
                .is_some_and(|fact| same_shard_payload(fact, &entry.shards))
            {
                return Ok(Some(DebtVerdict::SettledReclaimed));
            }
        }
    }
    Ok(None)
}

/// Discharge a stored cursor against the authorities that can speak for
/// this destination — never against the cursor itself.
pub(crate) fn verify_debt(
    entry: &DebtEntry,
    machine: &str,
    stage: &Path,
    remote_facts: Option<&ArchiveFacts>,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
) -> anyhow::Result<DebtVerdict> {
    if let Some(verdict) = debt_settled_locally(entry, machine, stage, manifest_facts)? {
        return Ok(verdict);
    }
    let Some(facts) = remote_facts else {
        return Ok(DebtVerdict::Unverifiable(
            "shards left the stage and the destination archive cannot be consulted",
        ));
    };
    let key = (machine.to_string(), entry.session_id.clone());
    match facts.get(&key) {
        Some(observed)
            if entry.shards.shard_identities.is_empty()
                && same_shard_payload(observed, &entry.shards) =>
        {
            Ok(DebtVerdict::SettledInArchive)
        }
        Some(observed) if archive_contains_shards(observed, &entry.shards) => {
            Ok(DebtVerdict::SettledInArchive)
        }
        Some(_) => Ok(DebtVerdict::Unverifiable(
            "destination archive holds a different shard set than the cursor claims",
        )),
        None => Ok(DebtVerdict::Unverifiable(
            "destination archive does not hold the shard set the cursor claims",
        )),
    }
}

fn same_shard_payload(left: &ShardFact, right: &ShardFact) -> bool {
    left.shard_count == right.shard_count
        && left.concat_bytes == right.concat_bytes
        && left.concat_sha256 == right.concat_sha256
}

/// Whether the archive contains every exact sealed shard named by this debt.
/// This allows an older source-root debt to remain verifiable after a later
/// push appended additional global shard sequences. Legacy debt without
/// sequence fingerprints remains exact-aggregate-only.
fn archive_contains_shards(archive: &ShardFact, debt: &ShardFact) -> bool {
    if debt.shard_identities.is_empty()
        || archive.shard_identities.is_empty()
        || !valid_shard_fact(archive)
        || !valid_shard_fact(debt)
    {
        return false;
    }
    for expected in &debt.shard_identities {
        if !archive
            .shard_identities
            .iter()
            .any(|observed| observed == expected)
        {
            return false;
        }
    }
    true
}

fn valid_shard_fact(fact: &ShardFact) -> bool {
    if fact.shard_count != fact.shard_identities.len()
        || !valid_sha256(&fact.concat_sha256)
        || fact.shard_identities.is_empty()
    {
        return false;
    }
    let mut previous = 0;
    let mut bytes = 0u64;
    for shard in &fact.shard_identities {
        if shard.sequence == 0 || shard.sequence <= previous || !valid_sha256(&shard.sha256) {
            return false;
        }
        previous = shard.sequence;
        let Some(total) = bytes.checked_add(shard.bytes) else {
            return false;
        };
        bytes = total;
    }
    bytes == fact.concat_bytes
        && (fact.shard_count != 1 || fact.concat_sha256 == fact.shard_identities[0].sha256)
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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

/// Metadata-only result of collecting one **machine log** (ADR-053 D1/D4).
///
/// Never a session id, never a source path and never a line of prompt text:
/// what leaves this struct is the harness, the log's own id, counts and
/// booleans. Deliberately not a [`CollectOutcome`] — that type is a *session*
/// outcome, and reusing it would put machine-log activity into every session
/// counter that reads one (ADR-053 D4: counts split, never summed).
#[derive(Debug, Clone)]
pub struct MachineLogOutcome {
    /// Registry harness id that declares the log (`claude-code`).
    pub harness: String,
    /// The log's own declared id (`history`).
    pub log_id: String,
    /// File size of the source at read time.
    pub source_bytes: u64,
    /// Bytes read this pass (delta, or the whole snapshot on a base read).
    pub bytes_read: u64,
    /// Committed-prefix bytes re-read and re-hashed as the integrity check.
    pub prefix_bytes_validated: u64,
    /// Complete lines sealed by this pass.
    pub lines_written: usize,
    /// Sealed generations of this log on the stage after the pass.
    pub generations: usize,
    /// Lines sealed across every generation of this log on the stage.
    pub sealed_lines: u64,
    /// The generation file name this pass sealed, when it sealed one.
    pub shard: Option<String>,
    /// This pass started a new base: the committed prefix no longer matched the
    /// source, so the whole current snapshot was re-read. Every previously
    /// sealed generation stays (ADR-053 D3).
    pub reset: bool,
    /// Why a stored cursor could not be proved and was therefore reread. Fixed
    /// metadata; `None` when the cursor was proved, locally or against the
    /// archive. A reset caused by a real source rewrite reports `None` here —
    /// the two are different facts and are never merged into one word.
    pub unproven: Option<&'static str>,
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
    /// type, template unresolvable, confidence `unascertained`). They contribute no
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
    /// Machine-log outcomes, one per declared log that exists on this machine
    /// (ADR-053 D4). Separate from `outcomes` on purpose: these are not
    /// sessions, and no session counter reads this vector.
    pub machine_logs: Vec<MachineLogOutcome>,
    /// Declared machine logs this pass never looked at (no registry cell for
    /// this platform, an `unascertained` cell, an unresolvable template). A
    /// declared source that was not looked at is an unknown, never an empty one.
    pub machine_logs_unlooked: usize,
    /// Declared machine logs that were looked at but whose shape could not be
    /// established (I/O error, or a directory where a file is declared).
    pub machine_logs_indeterminate: usize,
    /// Machine logs whose read **failed** this pass (the source could not be
    /// read at all). Distinct from `machine_logs_indeterminate`, which is a
    /// scan-time question about the source's shape; neither is a log that was
    /// found empty.
    pub machine_logs_errors: usize,
    /// Machine-log generations sealed into the stage by this pass. Counted
    /// separately from `shards_written`, which stays a session number.
    pub machine_log_generations_written: usize,
    /// Complete machine-log lines sealed by this pass. Separate from
    /// `lines_written` for the same reason.
    pub machine_log_lines_written: usize,
    pub errors: Vec<CollectError>,
    pub reconciliations: Vec<ReconcileNotice>,
    /// W880: registry scan wall time, in milliseconds. Measured by
    /// [`collect`]; zero when a test supplies its own scan report.
    pub scan_ms: u64,
    /// W880: incremental staging wall time, in milliseconds.
    pub collect_ms: u64,
    /// W880: staging wall time per harness, keyed by harness id.
    pub collect_harness_ms: std::collections::BTreeMap<String, u64>,
    /// W880: files whose metadata this pass took. A file read whole
    /// without a stat is counted in `source_bytes_read`, not here.
    pub files_statted: u64,
    /// W880: committed-prefix bytes re-read and re-hashed during
    /// collection (the JSONL integrity check).
    pub shard_bytes_read_hashed: u64,
    /// W880: SQLite sessions whose per-session rows were queried.
    pub sqlite_sessions_queried: u64,
    /// W880: SQLite sessions exported (re-sealed) into the stage.
    pub sqlite_sessions_exported: u64,
    /// ADR-061: whole-file observation shards sealed for snapshot-source
    /// sessions (the pre-increment shape, and the one-pass degrade of an
    /// unknown increment map).
    pub sqlite_full_exports: u64,
    /// ADR-061: changed snapshot-source sessions whose pass sealed
    /// record-level increment shards.
    pub sqlite_increment_passes: u64,
    /// ADR-061: record lines sealed by those passes (new and changed
    /// records, explicit tombstones, and session-observation records).
    pub sqlite_increment_records: u64,
    /// ADR-061: bytes of those record lines.
    pub sqlite_increment_bytes: u64,
    /// ADR-061: the measured true growth of the same passes — payload
    /// bytes of new and changed records (Δ in D6's hourly rule,
    /// `written ≤ max(1.2 × Δ, 64 KiB)`).
    pub sqlite_growth_bytes: u64,
    /// W880: collector state saves this pass.
    pub state_saves: u64,
    /// Codebuff sessions this pass read but did **not** archive, because the
    /// transcript snapshot did not validate as a JSON array (ADR-054 D6):
    /// the session skips the pass — nothing was sealed for it, not partial
    /// bytes — and a skip is a third state, deliberately counted by no other
    /// field here ("not archived this pass" is neither changed nor
    /// unchanged). One row per skip, bounded fields only; the durable
    /// observation lives in the stage's control stream
    /// (`codebuff-skip-v1.jsonl`), and a non-empty list is what makes the
    /// run itself exit `3`, "did not finish reading".
    pub codebuff_torn_skips: Vec<CodebuffTornSkip>,
    /// Registry-scan evidence for the push-stage guard, derived from the scan
    /// this collection already performed. `run-once` passes it to `push` so
    /// the empty-stage guard does not rescan the registry.
    pub scan: ScanEvidence,
}

/// One codebuff torn-read skip, as the run report holds it (ADR-054 D6).
/// Counts, closed-enum reasons and identity only — never content, never a
/// path; the printed report renders the length of this vector and the
/// reason vocabulary, nothing finer.
#[derive(Debug, Clone)]
pub struct CodebuffTornSkip {
    /// Registry `source_roots[].id` (`codebuff` / `codebuff-dev` /
    /// `codebuff-staging`), from the scan; empty only for a record no root
    /// walk labelled.
    pub root_id: String,
    /// The native id (`<project-basename>.<chatId>`), in memory for tests
    /// and the exit surface; never printed.
    pub session_native_id: String,
    /// The closed-enum reason the snapshot was refused for.
    pub reason: &'static str,
    /// Byte size of the unread snapshot.
    pub byte_size: u64,
}

/// Identity and timestamp of one collecting pass, minted once per
/// `collect_scan_report` run and shared by every skip observation the pass
/// writes (ADR-054 D6: `run_id` names the run, `pass_ts` the pass).
#[derive(Debug, Clone)]
pub struct PassContext {
    /// Bounded hex, minted per run: the pass's start time and destination
    /// digest hashed — no hostname, no machine path, no content.
    pub run_id: String,
    /// Unix seconds the pass started.
    pub pass_ts: i64,
}

/// W880: counters only the per-record read paths can measure. They
/// happen inside the read functions, below the report's reach, so
/// they are threaded through the record loop and folded into the
/// report once the pass ends. Counts only — never a path or id.
#[derive(Debug, Default)]
struct CollectCounters {
    files_statted: u64,
    sqlite_sessions_queried: u64,
    /// ADR-061: whole-file observation shards sealed for snapshot-source
    /// sessions (the pre-increment shape, and the one-pass degrade of an
    /// unknown increment map).
    sqlite_full_exports: u64,
    /// ADR-061: changed snapshot-source sessions whose pass sealed
    /// record-level increment shards.
    sqlite_increment_passes: u64,
    /// ADR-061: record lines sealed by those passes (new and changed
    /// records, explicit tombstones, and session-observation records).
    sqlite_increment_records: u64,
    /// ADR-061: bytes of those record lines.
    sqlite_increment_bytes: u64,
    /// ADR-061: the measured true growth of the same passes — payload
    /// bytes of new and changed records (Δ in D6's hourly rule,
    /// `written ≤ max(1.2 × Δ, 64 KiB)`).
    sqlite_growth_bytes: u64,
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

/// The registry-scan half of the empty-stage guard, split out so a scan that
/// has already been validated can be reused without running it again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanEvidence {
    pub scanner_records: usize,
    pub scanner_sqlite_sessions: u64,
    pub scanner_sqlite_unknown: usize,
    pub scanner_unknown: usize,
}

impl ScanEvidence {
    /// Derive the guard's scan evidence from a completed registry scan.
    pub fn from_scan_report(scan: &scanner::ScanReport) -> Self {
        Self {
            scanner_records: scan.records.len(),
            scanner_sqlite_sessions: scan
                .probes
                .iter()
                .filter(|probe| matches!(probe.state, scanner::ProbeState::FileTarget))
                .filter_map(|probe| probe.record_count)
                .sum(),
            scanner_sqlite_unknown: scan
                .probes
                .iter()
                .filter(|probe| matches!(probe.state, scanner::ProbeState::FileTarget))
                .filter(|probe| probe.record_count.is_none())
                .count(),
            scanner_unknown: scan
                .probes
                .iter()
                .filter(|probe| probe.record_count.is_none())
                .count(),
        }
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

/// What `collect` validated about the stage and the registry, for the push
/// path to reuse instead of rescanning and recounting. `stage_shards` is bound
/// to the stage tree by its freshness token: a push that finds the tree
/// changed must recount rather than trust a count taken before files were
/// written.
#[derive(Debug, Clone)]
pub struct CollectedStageEvidence {
    /// Registry-scan evidence from collection's already-validated scan.
    pub scan: ScanEvidence,
    /// The sealed-shard count collection's caller took right after collecting.
    pub stage_shards: store::CountedStageShards,
}

/// Collect the metadata needed to distinguish a genuinely new/empty machine
/// from a stage that disappeared after collection. This reads registry
/// metadata and the collector cursor only; it never reads session bodies.
///
/// This is the standalone `push` path: it performs the full guard, scanning
/// the registry and counting the stage itself. `run-once` instead reuses the
/// evidence collection already produced
/// ([`inspect_stage_for_push_with_evidence`]).
pub fn inspect_stage_for_push(
    config: &Config,
    stage: &Path,
    state_dir: &Path,
    machine: &str,
) -> anyhow::Result<PushStageCheck> {
    let scan = scanner::scan_with_machine(config, machine)
        .context("scan harness sessions for empty-stage guard")?;
    inspect_stage_for_push_with_evidence(
        &ScanEvidence::from_scan_report(&scan),
        stage,
        state_dir,
        &store::CountedStageShards::count(stage)?,
    )
}

/// Push-stage guard from evidence collection already validated. This is the
/// `run-once` reuse path: the registry scan is collection's own, passed in
/// rather than repeated, and the stage count is reused only while its
/// freshness token still matches the stage — otherwise the stage is recounted,
/// so a shard written between collection and push is never omitted.
pub fn inspect_stage_for_push_with_evidence(
    scan: &ScanEvidence,
    stage: &Path,
    state_dir: &Path,
    counted: &store::CountedStageShards,
) -> anyhow::Result<PushStageCheck> {
    let stage_shards = match counted.fresh_count(stage) {
        Some(count) => count,
        None => store::sealed_shard_count(stage)?,
    };
    let state = load_state(&state_dir.join(STATE_FILE))?;
    Ok(PushStageCheck {
        stage_shards,
        scanner_records: scan.scanner_records,
        scanner_sqlite_sessions: scan.scanner_sqlite_sessions,
        scanner_sqlite_unknown: scan.scanner_sqlite_unknown,
        scanner_unknown: scan.scanner_unknown,
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
    let scan_started = std::time::Instant::now();
    let scan = scanner::scan_with_machine(config, machine).context("scan harness sessions")?;
    let scan_ms = scan_started.elapsed().as_millis() as u64;
    let collect_started = std::time::Instant::now();
    let mut report =
        collect_scan_report(&scan, stage, machine, state_dir, bucket_cap, destination)?;
    report.scan_ms = scan_ms;
    report.collect_ms = collect_started.elapsed().as_millis() as u64;
    Ok(report)
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
        // reason: on first archive to a new destination, runstate has no record for this destination, so the debt set is naturally empty
        .unwrap_or_default()
        .files;
    // Re-key grok-bot debts before the first lookup, so a state file written
    // before the session key (W986) answers under the key this build reads.
    // In memory only: the file is rewritten by the same save paths as ever,
    // and an entry still has to prove itself before any cursor is reused.
    migrate_grok_bot_debts(&mut debts);
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
        scan: ScanEvidence::from_scan_report(scan),
        ..CollectReport::default()
    };

    if legacy_state_ignored {
        report.reconciliations.push(ReconcileNotice {
            session_prefix: "*".to_string(),
            reason: "pre-destination state file ignored: it belongs to no destination",
        });
    }

    let manifest_facts = load_local_manifest_facts(stage, machine);
    let mut records = scan.records.clone();
    records.sort_by(|a, b| a.absolute_path.cmp(&b.absolute_path));
    let mut wanted = BTreeSet::new();
    for record in &records {
        let key = state_key(record);
        if let Some(entry) = debts.get(&key) {
            if debt_settled_locally(entry, machine, stage, manifest_facts.as_ref())?.is_none() {
                wanted.insert((machine.to_string(), entry.session_id.clone()));
            }
        }
    }
    let remote_facts = if wanted.is_empty() {
        None
    } else {
        destination.facts(&wanted)
    };
    // W880: deep counters the read functions can only reach through
    // this handle; folded into the report once the pass ends.
    let mut counters = CollectCounters::default();
    // ADR-054 D6: the run identity the pass's skip observations carry.
    // Minted once from the pass's own start and destination digest —
    // bounded, contentless, and distinct per pass without naming the
    // machine or any path on it.
    let pass_started_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        // reason: duration_since UNIX_EPOCH fails only on a system clock set before the epoch; 1970 is not a plausible pass time, and the observations it stamps may not either
        .unwrap_or_default();
    let pass = PassContext {
        run_id: sha256_hex(format!("{pass_started_nanos}").as_bytes())
            .chars()
            .take(16)
            .collect(),
        // reason: now_unix_seconds fails only on the same clock-before-epoch case the nanos line above names; a 1970 pass_ts is the epoch itself, so the observation still says "stamped by a broken clock" rather than showing a plausible time
        pass_ts: crate::manifest::now_unix_seconds().unwrap_or_default(),
    };
    for mut record in records {
        // W880: the whole iteration is this record's collect work, so
        // the per-harness timer covers the cursor resolution and the
        // provenance bookkeeping too, not only the read.
        let record_started = std::time::Instant::now();
        let key = state_key(&record);
        let stored = debts.get(&key).cloned();
        // A stored cursor is a claim, not a fact. It is only reused once it has
        // discharged itself against the stage, the local reclaim manifest, or
        // this destination's own archive.
        let mut unverifiable = None;
        if let Some(entry) = stored.as_ref() {
            match verify_debt(entry, machine, stage, remote_facts, manifest_facts.as_ref())? {
                DebtVerdict::OwedOnStage
                | DebtVerdict::SettledInArchive
                | DebtVerdict::SettledReclaimed => {}
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
        // A destination with no stored cursor has never been read for, so this
        // pass would otherwise start every source at offset zero and seal the
        // whole file again on top of shards the stage already holds. Ask the
        // stage first: what it holds — verified against the source by the
        // ordinary read path, not trusted because it is ours — is the position
        // this destination actually starts from. A stage that disagrees with
        // the source yields nothing here and the full read happens as before.
        let old = match stored.as_ref() {
            Some(entry) => Some(cursor_or_stage_covered_position(
                &record,
                entry,
                unverifiable.is_none(),
                stage,
                machine,
                &mut counters,
                manifest_facts.as_ref(),
            )?),
            // One native UUID may appear in multiple source roots. Without a
            // persisted cursor for this exact scanned source, a matching
            // stage prefix cannot establish whether it is a replay or a new
            // repeated turn; conservatively capture measured provenance rows.
            // A movable root is the exception that is not ambiguous: it
            // belongs to a store that MOVES its files between the roots of
            // one entry (codex `thread/archive`), so a same-id discovery at a
            // new path is the same file by the store's own rules (W1007, ADR-
            // 059 draft D4) — it recovers its position from the stage by
            // content like any cursor-less source, instead of re-sealing the
            // bytes the archive already holds.
            None if !record.provenance.is_empty() && !record.movable_root => None,
            None => stage_prefix_entry(
                &record,
                stage,
                machine,
                &mut counters,
                manifest_facts.as_ref(),
            )?,
        };
        // The cursor is still handed down so the outcome can report this as a
        // reread rather than a first read; `force_reset` is what actually stops
        // it from being reused.
        match collect_one(
            &record,
            old.as_ref(),
            unverifiable.is_some(),
            stage,
            state_dir,
            machine,
            bucket_cap,
            &mut counters,
            &pass,
            manifest_facts.as_ref(),
        ) {
            Ok(ReadOutcome::Processed(processed)) => {
                crate::activity::record_session_provenance(
                    stage,
                    machine,
                    &record.id,
                    record.source,
                    &record.absolute_path,
                )
                .with_context(|| {
                    format!(
                        "record source-path provenance for {}",
                        crate::id::short_session_id(&record.id)
                    )
                })?;
                let outcome = processed.outcome;
                // W286: a session is "changed" exactly when this pass handed
                // new content to the archive — a sealed shard — or had to
                // reset its cursor and reread. Bytes that were read but
                // committed nothing do not qualify: an unterminated final
                // line is deliberately treated as *in progress* (it may be
                // half of a write), so a pass re-reads it, seals nothing,
                // and must leave the session unchanged. Counting it as
                // changed re-flagged the same stable tail on every pass
                // forever, and `run-once` answered that flag with a fresh
                // snapshot of an unchanged stage on every run. The read that
                // observed the tail is still reported honestly through
                // `bytes_read`; it is the archive-additional work that is
                // absent, and absent work must not be spelled "changed".
                let changed = outcome.shard.is_some() || outcome.reset;
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
                // W880: the committed prefix was read and SHA-256
                // hashed to prove it still matches the stage — shard
                // body bytes read and hashed.
                report.shard_bytes_read_hashed += outcome.prefix_bytes_validated;
                // W880: a SQLite session is exported when this pass
                // re-sealed its rows into the stage.
                if record.sqlite_layout.is_some() && outcome.shard.is_some() {
                    report.sqlite_sessions_exported += 1;
                }
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
                    _ if outcome.shard.is_none() => {
                        if let Some(fact) = manifest_facts.as_ref().and_then(|m| m.get(&record.id))
                        {
                            fact.clone()
                        } else {
                            stage_shard_fact(stage, machine, &record.id)?
                        }
                    }
                    _ => stage_shard_fact(stage, machine, &record.id)?,
                };
                let entry = DebtEntry {
                    machine: machine.to_string(),
                    session_id: record.id.clone(),
                    cursor: processed.state,
                    shards: shards.clone(),
                };
                if stored.as_ref() != Some(&entry) {
                    debts.insert(key, entry);
                    save_state(&state_path, &state, &destination_id, &debts)?;
                    // W880: one durable collector state save.
                    report.state_saves += 1;
                }
                report.outcomes.push(outcome);
                // TICKET-4D-10: stamp what this read observed firsthand into
                // the record's provenance so the observation below carries it.
                // Merged, never assigned — a root's surface observation is
                // another origin's fact, not a replacement for this one, and
                // the merge is a set union, so a dimension both origins record
                // once is still recorded once.
                if !processed.dimensions.is_empty() {
                    crate::provenance::merge(&mut record.provenance, &processed.dimensions);
                }
                if !record.provenance.is_empty() {
                    let body = stage_shard_fact(stage, machine, &record.id)?;
                    crate::provenance::append_scan_observation(
                        stage,
                        machine,
                        &record.id,
                        &record.provenance,
                        body.shard_count,
                        &body.concat_sha256,
                    )?;
                }
            }
            Ok(ReadOutcome::Skipped(skip)) => {
                // ADR-054 D6: a torn or non-array transcript skipped this
                // pass. Nothing was sealed and no cursor was written — the
                // stored debt, if any, still describes the generation that
                // *was* archived — so this session is deliberately not
                // counted changed, unchanged or reset. The observation
                // stream already holds the bounded record, and the run
                // surfaces the length of this vector as its one reason to
                // report incompleteness.
                report.codebuff_torn_skips.push(skip);
            }
            Err(_) => report.errors.push(CollectError {
                session_prefix: id_prefix(&record.id),
                source_path_sha256: path_digest(&record.absolute_path),
            }),
        }
        // W880: fold this record's wall time into its harness's
        // bucket, keyed by the registry's public harness id.
        *report
            .collect_harness_ms
            .entry(record.source.short().to_string())
            .or_default() += record_started.elapsed().as_millis() as u64;
    }
    report.files_statted = counters.files_statted;
    report.sqlite_sessions_queried = counters.sqlite_sessions_queried;
    // Machine logs are collected after every session, and into their own
    // report fields: a machine log is never a session outcome, never a session
    // shard, and never a session line (ADR-053 D4).
    collect_machine_logs(
        scan,
        stage,
        machine,
        state_dir,
        bucket_cap,
        destination,
        &mut report,
    )?;
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
    /// The 4D dimensions this read observed firsthand (TICKET-4D-10; Grok
    /// Bot's tenant and container join under TICKET-4D-08). Only the OpenClaw
    /// layout and the Grok Bot replica pass read any today; every other
    /// source answers with the empty set, which is the honest answer for a
    /// dimension nothing was read from — and never a value inferred from
    /// the harness name.
    dimensions: crate::provenance::SessionProvenance,
}

/// What one record's read path decided (ADR-054 D6 splits the last arm):
/// the read either produced a cursor outcome, or — for the one harness whose
/// contract makes an unvalidatable snapshot a *skip* rather than an error —
/// it sealed nothing and says so.
#[derive(Debug)]
enum ReadOutcome {
    Processed(Processed),
    /// The session skipped this pass: nothing sealed, no cursor written
    /// (the stored debt, if any, keeps describing the generation that *was*
    /// archived), one skip observation in the control stream, and the run
    /// is incomplete because of it.
    Skipped(CodebuffTornSkip),
}

fn collect_one(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    state_dir: &Path,
    machine: &str,
    bucket_cap: usize,
    counters: &mut CollectCounters,
    pass: &PassContext,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
) -> anyhow::Result<ReadOutcome> {
    if let Some(layout) = record.sqlite_layout {
        if layout == SqliteSessionLayout::GrokBot {
            process_grok_bot(
                record,
                old,
                force_reset,
                stage,
                state_dir,
                machine,
                bucket_cap,
                counters,
            )
            .map(ReadOutcome::Processed)
        } else {
            process_sqlite(
                record,
                layout,
                old,
                force_reset,
                stage,
                state_dir,
                machine,
                bucket_cap,
                counters,
                manifest_facts,
            )
            .map(ReadOutcome::Processed)
        }
    } else if record.compressed || is_zstd_path(&record.absolute_path) {
        Ok(ReadOutcome::Processed(process_compressed(
            record,
            old,
            force_reset,
            stage,
            machine,
            bucket_cap,
            manifest_facts,
        )?))
    } else if is_jsonl_path(&record.absolute_path) {
        Ok(ReadOutcome::Processed(process_jsonl(
            record,
            old,
            force_reset,
            stage,
            machine,
            bucket_cap,
            counters,
            manifest_facts,
        )?))
    } else if record.source == crate::models::HarnessSource::Codebuff {
        process_codebuff_whole_file(
            record,
            old,
            force_reset,
            stage,
            machine,
            bucket_cap,
            pass,
            manifest_facts,
        )
    } else {
        Ok(ReadOutcome::Processed(process_whole_file(
            record,
            old,
            force_reset,
            stage,
            machine,
            bucket_cap,
            manifest_facts,
        )?))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GrokBotUnionState {
    version: u32,
    records: Vec<crate::grok_bot::ReplicaRecord>,
}

// Version 2 of the union state: records may carry more than one variant of
// one sequence (a position re-observed with changed content is kept as an
// additional row). The wire shape — `{version, records:[{sequence, raw}]}` —
// is unchanged; unknown fields such as version 1's per-record
// `sequence_gaps` are ignored on read.
const GROK_BOT_UNION_STATE_VERSION: u32 = 2;

/// Delivery identity of one Grok Bot union row: the SHA-256 of its
/// canonical raw JSON. Rows are content-addressed because a sequence number
/// alone cannot say whether a position carries one archived observation or
/// several variants.
pub(crate) fn grok_bot_row_digest(
    record: &crate::grok_bot::ReplicaRecord,
) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(&record.raw).context("serialize Grok Bot raw record")?;
    Ok(sha256_hex(&bytes))
}

/// The union state file one grok-bot agent's observations answer through.
pub(crate) fn grok_bot_union_path(state_dir: &Path, agent_id: &str) -> PathBuf {
    state_dir
        .join("grok-bot-unions")
        .join(format!("{}.json", sha256_hex(agent_id.as_bytes())))
}

/// Load one agent's union state from disk. A missing file is an empty union;
/// anything else — unreadable, or a version this build does not write — is a
/// named failure, never a silent empty.
pub(crate) fn load_grok_bot_union(
    union_path: &Path,
) -> anyhow::Result<Vec<crate::grok_bot::ReplicaRecord>> {
    match fs::read(union_path) {
        Ok(bytes) => {
            let state: GrokBotUnionState = serde_json::from_slice(&bytes).with_context(|| {
                format!("parse Grok Bot union state ({})", path_digest(union_path))
            })?;
            if state.version != 1 && state.version != GROK_BOT_UNION_STATE_VERSION {
                bail!("unsupported Grok Bot union state version {}", state.version);
            }
            // Version 1 held at most one record per sequence — a valid
            // variant list, so it merges as-is. No version 1 state file can
            // exist outside a test run: the version 1 reader never matched a
            // real replica blob on disk (W329b).
            Ok(state.records)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).context("read Grok Bot union state"),
    }
}

/// Commit one agent's union state atomically, tmp file then rename, exactly
/// as a delivered pass has always written it.
pub(crate) fn write_grok_bot_union(
    union_path: &Path,
    records: Vec<crate::grok_bot::ReplicaRecord>,
) -> anyhow::Result<()> {
    let state = GrokBotUnionState {
        version: GROK_BOT_UNION_STATE_VERSION,
        records,
    };
    let encoded = serde_json::to_vec(&state).context("serialize Grok Bot union state")?;
    let tmp = union_path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, &encoded).context("write Grok Bot union state")?;
    fs::rename(&tmp, union_path).context("commit Grok Bot union state")
}

/// Which union rows a delivery must hand over, and the delivered set it
/// leaves behind. The delivered set is the session's memory (W986): a row
/// already in it is never handed over a second time, and `deliver_all` — the
/// reread a forced reset or an unmeasured cursor requires — hands the whole
/// union over once so the set is measured from then on.
pub(crate) fn plan_grok_bot_rows<'a>(
    union: &'a [crate::grok_bot::ReplicaRecord],
    delivered: &std::collections::BTreeSet<String>,
    deliver_all: bool,
) -> anyhow::Result<(
    Vec<ReplicaRecordRef<'a>>,
    std::collections::BTreeSet<String>,
)> {
    let mut delivered_now = delivered.clone();
    let mut to_deliver = Vec::new();
    for item in union {
        let digest = grok_bot_row_digest(item)?;
        if deliver_all || !delivered.contains(&digest) {
            to_deliver.push(ReplicaRecordRef { record: item });
            delivered_now.insert(digest);
        }
    }
    Ok((to_deliver, delivered_now))
}

/// A borrowed union row a delivery has selected.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReplicaRecordRef<'a> {
    pub(crate) record: &'a crate::grok_bot::ReplicaRecord,
}

/// The `_chat_stasher` measurement line a delivered Grok Bot shard carries:
/// the union's own gaps and bounds, the agent name the persistence handled
/// us, and — only for shards a pass sealed (backup import W988, app-driven
/// backfill BF-GROKBOT-1) — the provenance of that pass. The reader skips
/// this line's unknown fields, so its forms stay one vocabulary.
pub(crate) fn grok_bot_delivery_metadata(
    agent_name: Option<&str>,
    union: &[crate::grok_bot::ReplicaRecord],
    provenance: Option<&GrokBotPassProvenance>,
) -> serde_json::Value {
    let union_sequences: Vec<u64> = union.iter().map(|item| item.sequence).collect();
    let gaps = crate::grok_bot::sequence_gaps(union_sequences.iter().copied());
    let mut metadata = serde_json::json!({
        "_chat_stasher": {
            "source": "grok-bot",
            "agent_name": agent_name,
            "agent_name_state": if agent_name.is_some() { "known" } else { "unknown" },
            "partial_replica": true,
            "sequence_gaps": gaps,
            "observed_sequence_min": union_sequences.first(),
            "observed_sequence_max": union_sequences.last(),
        }
    });
    match provenance {
        Some(GrokBotPassProvenance::BackupImport(import)) => {
            metadata["_chat_stasher"]["imported_from_backup"] =
                serde_json::Value::String(import.backup_name.clone());
            metadata["_chat_stasher"]["imported_at"] =
                serde_json::Value::String(import.imported_at.clone());
        }
        Some(GrokBotPassProvenance::Backfill(backfill)) => {
            metadata["_chat_stasher"]["backfill"] = serde_json::Value::Bool(true);
            metadata["_chat_stasher"]["backfill_run_id"] =
                serde_json::Value::String(backfill.run_id.clone());
            metadata["_chat_stasher"]["backfill_app_version"] = match &backfill.app_version {
                Some(version) => serde_json::Value::String(version.clone()),
                None => serde_json::Value::Null,
            };
            metadata["_chat_stasher"]["backfill_at"] =
                serde_json::Value::String(backfill.ran_at.clone());
        }
        None => {}
    }
    metadata
}

/// The cursor one delivered Grok Bot pass records: the delivered digest set
/// it is now measured by, plus the observation scale of the delivery itself.
pub(crate) fn grok_bot_delivery_cursor(
    source_bytes: u64,
    union: &[crate::grok_bot::ReplicaRecord],
    delivered_now: std::collections::BTreeSet<String>,
) -> anyhow::Result<OffsetEntry> {
    let digest = sha256_hex(&serde_json::to_vec(&union)?);
    Ok(OffsetEntry {
        offset: delivered_now.len() as u64,
        prefix_len: source_bytes,
        prefix_sha256: digest,
        compressed: false,
        opencode: None,
        store_fingerprint: None,
        grok_bot_row_digests: Some(delivered_now.into_iter().collect()),
        opencode_record_digests: None,
        opencode_map_digest: None,
    })
}

fn process_grok_bot(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    state_dir: &Path,
    machine: &str,
    bucket_cap: usize,
    counters: &mut CollectCounters,
) -> anyhow::Result<Processed> {
    let agent_id = record
        .id
        .splitn(3, '.')
        .nth(2)
        .ok_or_else(|| anyhow!("invalid Grok Bot agent session id"))?;
    let root = record
        .absolute_path
        .parent()
        .ok_or_else(|| anyhow!("Grok Bot replica has no persistence directory"))?;
    let observed = crate::grok_bot::read_persistence(root)?
        .into_iter()
        .find(|agent| agent.agent_id == agent_id)
        .ok_or_else(|| anyhow!("Grok Bot replica disappeared during collection"))?;

    // TICKET-4D-08: the persistence read answered the tenant this agent's
    // state key is scoped to and the identities of the agent itself, so this
    // pass observed them firsthand — on every pass, including one that has
    // no new rows to deliver, because the keys and roster are read each time.
    let mut dimensions = crate::provenance::SessionProvenance::default();
    if let Some(tenant) = &observed.tenant {
        dimensions.insert_tenant(tenant.clone());
    }
    dimensions.insert_container(agent_id.to_string());
    if let Some(name) = &observed.name {
        dimensions.insert_container(name.clone());
    }

    fs::create_dir_all(state_dir.join("grok-bot-unions"))
        .context("create Grok Bot union state directory")?;
    let union_path = grok_bot_union_path(state_dir, agent_id);
    let previous = load_grok_bot_union(&union_path)?;
    let union = crate::grok_bot::merge_replica_records(previous, observed.records.clone());
    let delivered: std::collections::BTreeSet<String> = old
        .and_then(|entry| entry.grok_bot_row_digests.as_ref())
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    let deliver_all = force_reset
        || old.is_none()
        || old.is_some_and(|entry| entry.grok_bot_row_digests.is_none());
    let (to_deliver, delivered_now) = plan_grok_bot_rows(&union, &delivered, deliver_all)?;
    let rows_written = to_deliver.len();
    if rows_written == 0 && old.is_some() && !force_reset {
        let source_bytes = fs::metadata(&record.absolute_path)
            .with_context(|| {
                format!(
                    "stat Grok Bot replica ({})",
                    path_digest(&record.absolute_path)
                )
            })?
            .len();
        // W880: one source file stat'ed.
        counters.files_statted += 1;
        return Ok(Processed {
            state: old.cloned().expect("checked above"),
            outcome: CollectOutcome {
                session_prefix: id_prefix(&record.id),
                source_path_sha256: path_digest(&record.absolute_path),
                source_bytes,
                bytes_read: source_bytes,
                prefix_bytes_validated: source_bytes,
                lines_written: 0,
                shard: None,
                reset: false,
                compressed: false,
            },
            dimensions,
        });
    }
    let mut payload = Vec::new();
    for item in &to_deliver {
        serde_json::to_writer(&mut payload, &item.record.raw)
            .context("serialize Grok Bot raw record")?;
        payload.push(b'\n');
    }
    let metadata = grok_bot_delivery_metadata(observed.name.as_deref(), &union, None);
    serde_json::to_writer(&mut payload, &metadata)
        .context("serialize Grok Bot partial metadata")?;
    payload.push(b'\n');

    let shard = Some(store::write_sealed_shard_bytes_with_cap(
        store::StageWriter::Collect,
        stage,
        machine,
        &record.id,
        &[payload.clone()],
        bucket_cap,
    )?);

    write_grok_bot_union(&union_path, union.clone())?;

    let source_bytes = fs::metadata(&record.absolute_path)
        .with_context(|| {
            format!(
                "stat Grok Bot replica ({})",
                path_digest(&record.absolute_path)
            )
        })?
        .len();
    // W880: one source file stat'ed.
    counters.files_statted += 1;
    Ok(Processed {
        state: grok_bot_delivery_cursor(source_bytes, &union, delivered_now)?,
        outcome: CollectOutcome {
            session_prefix: id_prefix(&record.id),
            source_path_sha256: path_digest(&record.absolute_path),
            source_bytes,
            bytes_read: source_bytes,
            prefix_bytes_validated: 0,
            lines_written: rows_written + 1,
            shard,
            reset: old.is_some_and(|entry| entry.grok_bot_row_digests.is_none()),
            compressed: false,
        },
        dimensions,
    })
}

fn process_sqlite(
    record: &SessionRecord,
    layout: SqliteSessionLayout,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    state_dir: &Path,
    machine: &str,
    bucket_cap: usize,
    counters: &mut CollectCounters,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
) -> anyhow::Result<Processed> {
    // The whole-store fingerprint is a shortcut over the *store*, so it is
    // checked once here, before any per-session query. Equal fingerprint means
    // the store file has the same size and mtime as when this cursor was last
    // confirmed, so nothing in it moved — including this session — and the
    // whole store can be skipped.
    //
    // A *different* fingerprint proves nothing about any single session: one
    // write anywhere in the store moves it. So a difference must never decide
    // "this session changed"; it only means the shortcut is unavailable and the
    // per-session fields below have to answer. That is the whole defect this
    // ordering fixes — the fingerprint used to sit inside `OpenCodeCursor`, so
    // it *was* the answer, and every session re-exported a shard whose bytes
    // were already staged.
    let store_fingerprint = sqlite_store_fingerprint(&record.absolute_path)
        .map_err(|error| anyhow!("failed to fingerprint SQLite store: {error}"))?;
    // W880: the fingerprint stats the store file and its two sidecars.
    counters.files_statted += 3;
    if let Some(entry) = old {
        if !force_reset
            && entry.opencode.is_some()
            && entry.store_fingerprint.as_deref() == Some(store_fingerprint.as_str())
        {
            // The store is byte-identical to the pass that already collected
            // this session, so the dimensions that pass observed (TICKET-4D-10)
            // are already written against these exact shards; an unchanged
            // session has nothing new to record.
            return Ok(unchanged_sqlite(
                record,
                entry,
                &store_fingerprint,
                Default::default(),
            ));
        }
    }
    // W880: the shortcut did not fire, so this session's own rows
    // are about to be queried.
    counters.sqlite_sessions_queried += 1;
    match layout {
        SqliteSessionLayout::OpenCode => process_opencode(
            record,
            old,
            force_reset,
            stage,
            state_dir,
            machine,
            bucket_cap,
            &store_fingerprint,
            counters,
            manifest_facts,
        ),
        SqliteSessionLayout::OpenClaw => process_openclaw(
            record,
            old,
            force_reset,
            stage,
            machine,
            bucket_cap,
            &store_fingerprint,
            manifest_facts,
        ),
        SqliteSessionLayout::HermesAgent => {
            let session_id = native_session_id(record, "Hermes Agent")?;
            let snapshot = read_hermes_session(&record.absolute_path, &session_id)
                .map_err(|error| anyhow!("failed to read Hermes session snapshot: {error}"))?;
            process_sqlite_snapshot(
                record,
                old,
                force_reset,
                stage,
                machine,
                bucket_cap,
                snapshot.cursor,
                snapshot.json_line,
                &store_fingerprint,
                Default::default(),
                manifest_facts,
            )
        }
        SqliteSessionLayout::CursorLegacy => {
            let session_id = native_session_id(record, "Cursor legacy")?;
            let snapshot = read_cursor_legacy_session(&record.absolute_path, &session_id).map_err(
                |error| anyhow!("failed to read Cursor legacy session snapshot: {error}"),
            )?;
            process_sqlite_snapshot(
                record,
                old,
                force_reset,
                stage,
                machine,
                bucket_cap,
                snapshot.cursor,
                snapshot.json_line,
                &store_fingerprint,
                Default::default(),
                manifest_facts,
            )
        }
        SqliteSessionLayout::CursorGlobal => {
            let session_id = native_session_id(record, "Cursor global")?;
            let spec = cursor_global_schema();
            let cursor = sqlite_session_cursor(&record.absolute_path, &spec, &session_id)
                .map_err(|error| anyhow!("failed to read Cursor session cursor: {error}"))?;
            if !force_reset && old.is_some_and(|entry| entry.opencode.as_ref() == Some(&cursor)) {
                return Ok(unchanged_sqlite(
                    record,
                    old.expect("checked above"),
                    &store_fingerprint,
                    Default::default(),
                ));
            }
            let snapshot = read_sqlite_session(&record.absolute_path, &spec, &session_id)
                .map_err(|error| anyhow!("failed to read Cursor session snapshot: {error}"))?;
            process_sqlite_snapshot(
                record,
                old,
                force_reset,
                stage,
                machine,
                bucket_cap,
                snapshot.cursor,
                snapshot.json_line,
                &store_fingerprint,
                Default::default(),
                manifest_facts,
            )
        }
        SqliteSessionLayout::Grok => {
            let session_id = native_session_id(record, "Grok")?;
            let spec = grok_schema();
            let cursor = sqlite_session_cursor(&record.absolute_path, &spec, &session_id)
                .map_err(|error| anyhow!("failed to read Grok session cursor: {error}"))?;
            if !force_reset && old.is_some_and(|entry| entry.opencode.as_ref() == Some(&cursor)) {
                return Ok(unchanged_sqlite(
                    record,
                    old.expect("checked above"),
                    &store_fingerprint,
                    Default::default(),
                ));
            }
            let snapshot = read_sqlite_session(&record.absolute_path, &spec, &session_id)
                .map_err(|error| anyhow!("failed to read Grok session snapshot: {error}"))?;
            process_sqlite_snapshot(
                record,
                old,
                force_reset,
                stage,
                machine,
                bucket_cap,
                snapshot.cursor,
                snapshot.json_line,
                &store_fingerprint,
                Default::default(),
                manifest_facts,
            )
        }
        SqliteSessionLayout::Zed => {
            let session_id = native_session_id(record, "Zed")?;
            let spec = zed_schema();
            let cursor = sqlite_session_cursor(&record.absolute_path, &spec, &session_id)
                .map_err(|error| anyhow!("failed to read Zed session cursor: {error}"))?;
            if !force_reset && old.is_some_and(|entry| entry.opencode.as_ref() == Some(&cursor)) {
                return Ok(unchanged_sqlite(
                    record,
                    old.expect("checked above"),
                    &store_fingerprint,
                    Default::default(),
                ));
            }
            let snapshot = read_zed_session(&record.absolute_path, &spec, &session_id)
                .map_err(|error| anyhow!("failed to read Zed session snapshot: {error}"))?;
            process_sqlite_snapshot(
                record,
                old,
                force_reset,
                stage,
                machine,
                bucket_cap,
                snapshot.cursor,
                snapshot.json_line,
                &store_fingerprint,
                Default::default(),
                manifest_facts,
            )
        }
        SqliteSessionLayout::GitHubCopilotCli => {
            let session_id = native_session_id(record, "GitHub Copilot CLI")?;
            let snapshot = read_github_copilot_session(&record.absolute_path, &session_id)
                .map_err(|error| anyhow!("failed to read Copilot session snapshot: {error}"))?;
            process_sqlite_snapshot(
                record,
                old,
                force_reset,
                stage,
                machine,
                bucket_cap,
                snapshot.cursor,
                snapshot.json_line,
                &store_fingerprint,
                snapshot.dimensions,
                manifest_facts,
            )
        }
        SqliteSessionLayout::GrokBot => {
            bail!("Grok Bot persistence must use its replica collector")
        }
    }
}

fn process_openclaw(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
    store_fingerprint: &str,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
) -> anyhow::Result<Processed> {
    let native = record
        .id
        .splitn(3, '.')
        .nth(2)
        .ok_or_else(|| anyhow!("invalid OpenClaw session id"))?;
    let (native, generation) = match native.rsplit_once("~g") {
        Some((native, generation)) => (
            native,
            Some(
                generation
                    .parse::<i64>()
                    .map_err(|_| anyhow!("invalid OpenClaw archive generation"))?,
            ),
        ),
        None => (native, None),
    };
    let encoded = native
        .strip_prefix("oc-")
        .ok_or_else(|| anyhow!("invalid OpenClaw agent/session id"))?;
    let (agent_id, session_id) = encoded
        .split_once('-')
        .ok_or_else(|| anyhow!("invalid OpenClaw agent/session id"))?;
    if agent_id.is_empty() {
        return Err(anyhow!("invalid OpenClaw agent identity"));
    }
    let session_id = decode_openclaw_id_component(session_id)?;
    let snapshot = read_openclaw_session(&record.absolute_path, &session_id, generation)
        .map_err(|error| anyhow!("failed to read OpenClaw session snapshot: {error}"))?;
    // TICKET-4D-10: the read observed the store's own `schema_meta.agent_id`
    // (the container every session in this store ran inside) and, for a cold
    // snapshot read from `session_transcript_archives`, the archived status.
    // They travel in `Processed` so the collect pass can stamp them into the
    // record's provenance observation.
    process_sqlite_snapshot(
        record,
        old,
        force_reset,
        stage,
        machine,
        bucket_cap,
        snapshot.cursor,
        snapshot.json_line,
        store_fingerprint,
        snapshot.dimensions,
        manifest_facts,
    )
}

fn decode_openclaw_id_component(encoded: &str) -> anyhow::Result<String> {
    if !encoded.len().is_multiple_of(2) {
        return Err(anyhow!("invalid OpenClaw session identity encoding"));
    }
    let bytes = encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).map_err(anyhow::Error::from)?;
            u8::from_str_radix(pair, 16).map_err(anyhow::Error::from)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    String::from_utf8(bytes).map_err(|_| anyhow!("OpenClaw session identity is not UTF-8"))
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
    store_fingerprint: &str,
    dimensions: crate::provenance::SessionProvenance,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
) -> anyhow::Result<Processed> {
    // This session's own fields, and nothing else. The whole-store fingerprint
    // was removed from `OpenCodeCursor` precisely so that this comparison
    // cannot be polluted by a write to some other session.
    if !force_reset && old.is_some_and(|entry| entry.opencode.as_ref() == Some(&cursor)) {
        return Ok(unchanged_sqlite(
            record,
            old.expect("checked above"),
            store_fingerprint,
            dimensions,
        ));
    }
    let source_bytes = json_line.len() as u64;
    let digest = sha256_hex(&json_line);
    // No cursor of this destination's own, and the stage already holds exactly
    // this export: the destination is owed nothing, and sealing it again would
    // store the conversation twice.
    let matches_manifest =
        !force_reset && manifest_holds_this_export(manifest_facts, &record.id, &json_line);
    if matches_manifest
        || (old.is_none()
            && !force_reset
            && stage_holds_this_export(stage, machine, &record.id, &json_line)?)
    {
        return Ok(unchanged_content_sqlite(
            record,
            cursor,
            store_fingerprint,
            source_bytes,
            &digest,
            dimensions,
        ));
    }
    if let Some(entry) = old {
        if !force_reset
            && (export_content_matches(entry, source_bytes, &digest) || matches_manifest)
        {
            return Ok(unchanged_content_sqlite(
                record,
                cursor,
                store_fingerprint,
                source_bytes,
                &digest,
                dimensions,
            ));
        }
    }
    // A changed SQLite cursor proves a fresh source observation even when its
    // exported bytes happen to match an earlier shard. Preserve that append;
    // every other caller gets the writer's exact-repeat guard.
    let shard_writer = if old.is_some_and(|entry| {
        entry
            .opencode
            .as_ref()
            .is_some_and(|previous| previous != &cursor)
    }) {
        store::write_sealed_shard_bytes_allow_exact_repeat_with_cap
    } else {
        store::write_sealed_shard_bytes_with_cap
    };
    let shard = Some(shard_writer(
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
            store_fingerprint: Some(store_fingerprint.to_string()),
            grok_bot_row_digests: None,
            opencode_record_digests: None,
            opencode_map_digest: None,
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
        dimensions,
    })
}

/// The safety net: are the bytes we are about to seal byte-for-byte the bytes
/// this cursor already exported?
///
/// `false` is always the safe answer, so every uncertain case answers `false`:
/// a missing or empty `prefix_sha256` (we never recorded a hash, or cannot read
/// one), a length that disagrees with the cursor's own two length fields, or an
/// entry that is not a SQLite session cursor at all. "We cannot tell" is never
/// read as "identical" — that would turn an unknown into a silent omission,
/// which is the one outcome this repository never trades away.
fn export_content_matches(entry: &OffsetEntry, source_bytes: u64, digest: &str) -> bool {
    entry.opencode.is_some()
        && !entry.compressed
        && entry.offset == source_bytes
        && entry.prefix_len == source_bytes
        && !entry.prefix_sha256.is_empty()
        && entry.prefix_sha256 == digest
}

/// The session's cursor moved, but the export it produced is byte-identical to
/// the one already staged. Advance the cursor and skip the write.
///
/// This is *not* an incremental model — that decision (one full snapshot per
/// change) is untouched. It removes a provably duplicate write of the exact
/// same bytes, and it is only reachable when the entry already carries the
/// digest of that export. `force_reset` is excluded by the caller: an
/// unverifiable cursor may mean the staged shard is gone, and "the bytes are
/// the same so we need not write" would then keep the data out of the archive
/// forever.
///
/// `bytes_read` reports the read that really happened — the session *was*
/// read, that is how the digest was computed. `lines_written` is 0 and `shard`
/// is `None` because nothing new was staged.
fn unchanged_content_sqlite(
    record: &SessionRecord,
    cursor: OpenCodeCursor,
    store_fingerprint: &str,
    source_bytes: u64,
    digest: &str,
    dimensions: crate::provenance::SessionProvenance,
) -> Processed {
    Processed {
        state: OffsetEntry {
            offset: source_bytes,
            prefix_len: source_bytes,
            prefix_sha256: digest.to_string(),
            compressed: false,
            opencode: Some(cursor),
            store_fingerprint: Some(store_fingerprint.to_string()),
            grok_bot_row_digests: None,
            opencode_record_digests: None,
            opencode_map_digest: None,
        },
        outcome: CollectOutcome {
            session_prefix: id_prefix(&record.id),
            source_path_sha256: path_digest(&record.absolute_path),
            source_bytes,
            bytes_read: source_bytes,
            prefix_bytes_validated: 0,
            lines_written: 0,
            shard: None,
            reset: false,
            compressed: false,
        },
        dimensions,
    }
}

/// The session did not change. The cursor is carried over untouched; only the
/// whole-store fingerprint is refreshed, because the caller has just observed
/// the store and compared this session's fields against it. Recording that
/// observation lets the next pass take the shortcut, and it claims nothing new:
/// an equal fingerprint means the store file is the same size and mtime, so the
/// session is still unchanged.
///
/// `old` is reused rather than rebuilt so a hand-edited or older entry keeps
/// whatever else it carries.
fn unchanged_sqlite(
    record: &SessionRecord,
    old: &OffsetEntry,
    store_fingerprint: &str,
    dimensions: crate::provenance::SessionProvenance,
) -> Processed {
    let mut state = old.clone();
    state.store_fingerprint = Some(store_fingerprint.to_string());
    Processed {
        state,
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
        dimensions,
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
    counters: &mut CollectCounters,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
) -> anyhow::Result<Processed> {
    let data = read_jsonl_delta(&record.absolute_path, old, force_reset, counters)?;
    let (lines, committed_delta) = complete_lines(&data.bytes);
    let new_offset = data.base_offset + committed_delta as u64;
    // A full read whose framed lines equal the retained manifest fact is a
    // zero-byte no-op only for the sources whose byte equality can say so —
    // see [`byte_equality_establishes_event_identity`]. A fresh
    // provenance-bearing root that re-observes byte-identical bytes stays
    // ambiguous: it may be a legitimately repeated identical turn, and
    // dropping it would trade a captured turn for a red invariant
    // ("an unknown must never be recorded as empty"), so it falls through
    // to the multiplicity-preserving writer below.
    let matches_manifest = !force_reset
        && data.base_offset == 0
        && !lines.is_empty()
        && byte_equality_establishes_event_identity(record)
        && manifest_holds_this_framed(manifest_facts, &record.id, &seal_framed(&lines));
    if matches_manifest {
        return Ok(Processed {
            state: plain_state(&record.absolute_path, new_offset)?,
            outcome: CollectOutcome {
                session_prefix: id_prefix(&record.id),
                source_path_sha256: path_digest(&record.absolute_path),
                source_bytes: data.source_len,
                bytes_read: data.bytes_read,
                prefix_bytes_validated: data.source_len,
                lines_written: 0,
                shard: None,
                reset: false,
                compressed: false,
            },
            dimensions: Default::default(),
        });
    }
    let new_state = if lines.is_empty() && !data.reset {
        old.cloned().unwrap_or(OffsetEntry {
            offset: 0,
            prefix_len: 0,
            prefix_sha256: sha256_hex(&[]),
            compressed: false,
            opencode: None,
            store_fingerprint: None,
            grok_bot_row_digests: None,
            opencode_record_digests: None,
            opencode_map_digest: None,
        })
    } else {
        plain_state(&record.absolute_path, new_offset)?
    };
    let shard = if lines.is_empty() {
        None
    } else if (!data.reset && data.base_offset > 0)
        || (old.is_none() && !record.provenance.is_empty() && !record.movable_root)
    {
        // A validated non-zero cursor proves these are newly appended source
        // bytes. They may be byte-identical to an earlier turn (`a\n` followed
        // by another real `a\n`) and still belong in the archive.
        // A fresh provenance-bearing source is ambiguous even at offset zero:
        // byte equality with a different root cannot establish event identity.
        // Its complete capture must therefore retain the same multiplicity —
        // with one exception. Through a movable root (a store that moves its
        // files between the roots of one registry entry, W1007) byte equality
        // with the sibling IS the same file by the store's own rules, so a
        // fresh movable capture takes the deduplicating writer instead: an
        // exact whole-shard repeat of bytes the stage already holds is
        // skipped, never sealed twice.
        Some(store::write_sealed_shard_bytes_allow_exact_repeat_with_cap(
            store::StageWriter::Collect,
            stage,
            machine,
            &record.id,
            &lines,
            bucket_cap,
        )?)
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
        dimensions: Default::default(),
    })
}

// -------------------------------------------------------------- machine logs
//
// ADR-053 C1. A *machine log* is a declared per-machine input log: what the
// user typed into a harness on this machine, in the harness's own append-only
// file. It is **not** a session and no session id is ever minted for it (D1):
// it lives in its own stage namespace
// (`machine-logs/<machine>/<harness>/<log-id>/`), contributes to no session
// list, count, coverage number or `machine_recall` bucket (D4/D6), and is
// sealed by the same JSONL prefix-hash read path a session transcript uses
// (D2) — `read_jsonl_delta`, `complete_lines` and the shared shard writer.
//
// What it *does* share with a session is the debt discipline (ADR-012/013): the
// cursor is a cache that must prove itself against the stage or against the
// destination's archive, and anything unprovable is reread. The two products
// keep separate state files and separate counters because they have separate
// identities; a single file would let machine-log bookkeeping rewrite session
// debt, and vice versa.

/// Durable machine-log cursors, per destination. Its own file: see the module
/// note above.
const MACHINE_LOG_STATE_FILE: &str = "machine-logs-v1.json";
const MACHINE_LOG_STATE_VERSION: u32 = 1;
/// Capture stamps and reset observations of one machine log, one JSON object
/// per line, next to the sealed generations they describe (D2/D3/D8). Holds
/// digests, counts and fixed enum values — never prompt text.
const MACHINE_LOG_CAPTURE_FILE: &str = "capture-v1.jsonl";
pub const MACHINE_LOG_CAPTURE_SCHEMA: &str = "chat-stasher/machine-log@1";

/// Cursor and accountability for one machine log, for one destination — the
/// same shape a session [`DebtEntry`] has, in a namespace of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MachineLogEntry {
    /// Machine partition the generations were sealed under. A cursor written
    /// for a different partition proves nothing here.
    machine: String,
    /// Registry harness id that declares the log.
    harness: String,
    /// The log's own declared id.
    log_id: String,
    /// Read position that produced the generation set below.
    cursor: OffsetEntry,
    /// What the cursor claims it handed over. Verified before reuse.
    shards: ShardFact,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MachineLogState {
    version: u32,
    /// Keyed by [`destination_id`], then by `"<harness>/<log_id>"`.
    destinations: BTreeMap<String, BTreeMap<String, MachineLogEntry>>,
}

/// One capture stamp / reset observation row. Written at capture, never
/// inferred later: `fidelity` is ADR-051's capture-time stamp (`source:
/// captured`, `value: raw` — a byte-faithful copy of the harness's own
/// persisted log), and `reason` is what this pass actually did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MachineLogCapture {
    schema: String,
    kind: &'static str,
    machine: String,
    harness: String,
    log_id: String,
    /// Global sequence of the generation this pass sealed.
    generation: u64,
    /// Bytes of that generation.
    bytes: u64,
    /// sha256 of that generation.
    sha256: String,
    /// The destination-shaped cursor after this pass: the committed prefix of
    /// the source, which is what the next pass validates (ADR-012/013).
    cursor_offset: u64,
    /// Source file size at capture.
    source_bytes: u64,
    /// What this pass did: `base` (no cursor existed), `append` (validated
    /// delta) or `reset` (a cursor existed and the whole current snapshot was
    /// re-read).
    reason: &'static str,
    /// For a `reset`, why: `cursor_unproven` (the stored cursor could not be
    /// proved against the stage or the archive) or `committed_prefix_changed`
    /// (the source's committed prefix no longer matches the recorded hash).
    /// `None` otherwise. Observation, not inference: the first is a fact about
    /// our own bookkeeping, the second a fact about the source bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reset_cause: Option<&'static str>,
    /// Every generation sealed before this one, still in the archive
    /// (ADR-053 D3: a rewrite never deletes what was already observed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    prior_generations: Vec<ShardFingerprint>,
    /// ADR-051 capture-time fidelity. Flattened, so the row reads
    /// `"fidelity":{"source":"captured","value":"raw"}`.
    fidelity: crate::message_audit::FidelityMetadata,
    captured_at_unix: u64,
}

/// One machine log's pass result, below the report's reach: the same
/// [`Processed`] shape a session read produces, minus the session identity.
#[derive(Debug)]
struct ProcessedMachineLog {
    cursor: OffsetEntry,
    reset: bool,
    bytes_read: u64,
    prefix_bytes_validated: u64,
    source_bytes: u64,
    lines_written: usize,
    shard: Option<String>,
    /// Sequence number of the generation sealed by this pass, when it sealed
    /// one.
    generation: Option<u64>,
}

fn machine_log_state_key(harness: &str, log_id: &str) -> String {
    format!("{harness}/{log_id}")
}

/// Load the machine-log state. Like the session debt set, a state file that
/// cannot be parsed or that carries a version this build does not write is
/// discarded rather than migrated: it can prove nothing, so every machine log
/// becomes unread and is reread in full — the conservative direction.
fn load_machine_log_state(path: &Path) -> anyhow::Result<MachineLogState> {
    let empty = MachineLogState {
        version: MACHINE_LOG_STATE_VERSION,
        destinations: BTreeMap::new(),
    };
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<MachineLogState>(&bytes) {
            Ok(state) if state.version == MACHINE_LOG_STATE_VERSION => Ok(state),
            _ => Ok(empty),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(empty),
        Err(e) => Err(e).with_context(|| format!("read machine-log state {}", path.display())),
    }
}

/// Persist one destination's machine-log cursors, leaving every other
/// destination's slice byte-for-byte as it was loaded.
fn save_machine_log_state(
    path: &Path,
    state: &MachineLogState,
    destination_id: &str,
    logs: &BTreeMap<String, MachineLogEntry>,
) -> anyhow::Result<()> {
    let mut out = state.clone();
    out.version = MACHINE_LOG_STATE_VERSION;
    out.destinations
        .insert(destination_id.to_string(), logs.clone());
    let tmp = path.with_file_name(format!(".{MACHINE_LOG_STATE_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(&out).context("serialise machine-log state")?;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Discharge a stored machine-log cursor against the authorities that can speak
/// for this destination — never against the cursor itself. The verdicts are the
/// session ones, because the question is the same question.
fn verify_machine_log_debt(
    entry: &MachineLogEntry,
    machine: &str,
    stage: &Path,
    remote: Option<&MachineLogFacts>,
) -> anyhow::Result<DebtVerdict> {
    if entry.machine != machine {
        return Ok(DebtVerdict::Unverifiable(
            "machine-log cursor was written for a different machine partition",
        ));
    }
    let dir = store::machine_log_shard_dir(stage, machine, &entry.harness, &entry.log_id);
    if dir_covers(&dir, &entry.shards)? {
        return Ok(DebtVerdict::OwedOnStage);
    }
    let Some(facts) = remote else {
        return Ok(DebtVerdict::Unverifiable(
            "machine-log generations left the stage and the destination archive cannot be consulted",
        ));
    };
    let key = (
        machine.to_string(),
        entry.harness.clone(),
        entry.log_id.clone(),
    );
    match facts.get(&key) {
        Some(observed)
            if entry.shards.shard_identities.is_empty()
                && same_shard_payload(observed, &entry.shards) =>
        {
            Ok(DebtVerdict::SettledInArchive)
        }
        Some(observed) if archive_contains_shards(observed, &entry.shards) => {
            Ok(DebtVerdict::SettledInArchive)
        }
        Some(_) => Ok(DebtVerdict::Unverifiable(
            "destination archive holds a different generation set than the machine-log cursor claims",
        )),
        None => Ok(DebtVerdict::Unverifiable(
            "destination archive does not hold the generation set the machine-log cursor claims",
        )),
    }
}

/// (generations, sealed lines) of one machine log as the stage holds it now.
/// Every sealed shard is newline-framed, so the line count is exact.
fn machine_log_stage_state(
    stage: &Path,
    machine: &str,
    harness: &str,
    log_id: &str,
) -> anyhow::Result<(usize, u64)> {
    let dir = store::machine_log_shard_dir(stage, machine, harness, log_id);
    let generations = store::sealed_shard_entries(&dir)?.len();
    let concat = store::concat_shards_in_dir(&dir)?;
    Ok((
        generations,
        concat.iter().filter(|byte| **byte == b'\n').count() as u64,
    ))
}

/// Append one capture stamp. Existing rows are never rewritten, so the file is
/// the append-only record of what each pass observed — and a generation is
/// stamped exactly once: a pass that found the generation already sealed (a
/// second destination's first read of a stage that already holds it) writes no
/// second row, because nothing was captured by it that was not captured
/// before.
fn append_machine_log_capture(
    stage: &Path,
    machine: &str,
    capture: &MachineLogCapture,
) -> anyhow::Result<()> {
    let dir = store::machine_log_shard_dir(stage, machine, &capture.harness, &capture.log_id);
    fs::create_dir_all(&dir)?;
    let path = dir.join(MACHINE_LOG_CAPTURE_FILE);
    if let Ok(existing) = fs::read_to_string(&path) {
        for line in existing.lines() {
            let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if row.get("generation").and_then(serde_json::Value::as_u64) == Some(capture.generation)
                && row.get("sha256").and_then(serde_json::Value::as_str)
                    == Some(capture.sha256.as_str())
            {
                return Ok(());
            }
        }
    }
    let mut line = serde_json::to_vec(capture).context("serialise machine-log capture stamp")?;
    line.push(b'\n');
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {} for append", path.display()))?;
    file.write_all(&line)?;
    file.sync_all()?;
    Ok(())
}

/// One machine-log pass: same JSONL increment rules as a session transcript,
/// pointed at the machine-log namespace (ADR-053 D2).
fn process_machine_log(
    record: &scanner::MachineLogRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
    counters: &mut CollectCounters,
) -> anyhow::Result<ProcessedMachineLog> {
    let data = read_jsonl_delta(&record.absolute_path, old, force_reset, counters)?;
    let (lines, committed_delta) = complete_lines(&data.bytes);
    let new_offset = data.base_offset + committed_delta as u64;
    let cursor = if lines.is_empty() && !data.reset {
        old.cloned().unwrap_or_else(empty_cursor)
    } else {
        plain_state(&record.absolute_path, new_offset)?
    };
    let shard = if lines.is_empty() {
        None
    } else {
        // A validated non-zero cursor proves these are newly appended source
        // bytes, and they may be byte-identical to an earlier generation
        // (`/usage\n` twice) without being a replay of it. A base read has no
        // such proof, so it keeps the idempotent default: a byte-identical
        // rewrite is an honest no-op of content rather than a second copy of
        // bytes the archive already holds.
        let allow_exact_repeat = !data.reset && data.base_offset > 0;
        Some(store::write_sealed_machine_log_shard(
            stage,
            machine,
            &record.harness,
            &record.log_id,
            &lines,
            bucket_cap,
            allow_exact_repeat,
        )?)
    };
    let generation = match shard.as_deref() {
        Some(name) => store::parse_shard_seq(name),
        None => None,
    };
    Ok(ProcessedMachineLog {
        cursor,
        reset: data.reset,
        bytes_read: data.bytes_read,
        prefix_bytes_validated: data.prefix_bytes_validated,
        source_bytes: data.source_len,
        lines_written: lines.len(),
        shard,
        generation,
    })
}

fn empty_cursor() -> OffsetEntry {
    OffsetEntry {
        offset: 0,
        prefix_len: 0,
        prefix_sha256: sha256_hex(&[]),
        compressed: false,
        opencode: None,
        store_fingerprint: None,
        grok_bot_row_digests: None,
        opencode_record_digests: None,
        opencode_map_digest: None,
    }
}

/// Collect every declared machine log that exists on this machine.
///
/// Writes into `report.machine_logs` and the two machine-log tallies, and
/// touches nothing else in the report: the session counters a caller reads are
/// exactly what they would be without a machine log (ADR-053 test 6).
fn collect_machine_logs(
    scan: &scanner::ScanReport,
    stage: &Path,
    machine: &str,
    state_dir: &Path,
    bucket_cap: usize,
    destination: &DestinationView<'_>,
    report: &mut CollectReport,
) -> anyhow::Result<()> {
    report.machine_logs_unlooked = scan.machine_logs_unlooked;
    report.machine_logs_indeterminate = scan.machine_logs_indeterminate;
    if scan.machine_logs.is_empty() {
        return Ok(());
    }
    let state_path = state_dir.join(MACHINE_LOG_STATE_FILE);
    let state = load_machine_log_state(&state_path)?;
    let destination_id = report.destination_id.clone();
    let mut logs = state
        .destinations
        .get(&destination_id)
        .cloned()
        // reason: a destination with no machine-log record has never been read
        // for, so no machine-log cursor exists to reuse
        .unwrap_or_default();

    let mut records = scan.machine_logs.clone();
    records.sort_by(|a, b| a.absolute_path.cmp(&b.absolute_path));

    // Ask the archive only when a stored cursor cannot be proved locally. A
    // pass whose generations are all still owed on the stage never opens it a
    // second time — the session pass owns the ordinary read.
    let mut remote_needed = false;
    for record in &records {
        let Some(entry) = logs.get(&machine_log_state_key(&record.harness, &record.log_id)) else {
            continue;
        };
        if entry.machine != machine {
            remote_needed = true;
            continue;
        }
        let dir = store::machine_log_shard_dir(stage, machine, &record.harness, &record.log_id);
        if !dir_covers(&dir, &entry.shards)? {
            remote_needed = true;
        }
    }
    let remote = if remote_needed {
        destination.machine_log_facts()
    } else {
        None
    };

    let mut counters = CollectCounters::default();
    for record in &records {
        let record_started = std::time::Instant::now();
        let key = machine_log_state_key(&record.harness, &record.log_id);
        let stored = logs.get(&key).cloned();
        // A stored cursor is a claim, not a fact. It is reused only once it has
        // discharged itself against the stage, the local generations, or this
        // destination's own archive.
        let verdict = match stored.as_ref() {
            Some(entry) => verify_machine_log_debt(entry, machine, stage, remote)?,
            None => DebtVerdict::OwedOnStage,
        };
        let unproven = match verdict {
            DebtVerdict::Unverifiable(reason) => Some(reason),
            _ => None,
        };
        let dir = store::machine_log_shard_dir(stage, machine, &record.harness, &record.log_id);
        // A destination with no stored cursor has never been read for. The
        // stage may still already hold this log's generations from another
        // destination's pass; what it holds — verified against the source by
        // the ordinary read path — is then the position this destination starts
        // from.
        let old = match stored.as_ref() {
            Some(entry) => Some(entry.cursor.clone()),
            None => jsonl_stage_prefix_entry(&record.absolute_path, &dir, &mut counters)?,
        };
        let processed = match process_machine_log(
            record,
            old.as_ref(),
            unproven.is_some(),
            stage,
            machine,
            bucket_cap,
            &mut counters,
        ) {
            Ok(processed) => processed,
            Err(_) => {
                report.machine_logs_errors += 1;
                continue;
            }
        };
        let (generations, sealed_lines) =
            machine_log_stage_state(stage, machine, &record.harness, &record.log_id)?;
        if processed.shard.is_some() {
            report.machine_log_generations_written += 1;
        }
        report.machine_log_lines_written += processed.lines_written;

        let fact = shard_fact_in_dir(&dir)?;
        if let (Some(shard_name), Some(generation)) =
            (processed.shard.as_deref(), processed.generation)
        {
            let path =
                store::machine_log_shard_dir(stage, machine, &record.harness, &record.log_id)
                    .join(store::shard_bucket_name(generation, bucket_cap))
                    .join(shard_name);
            let bytes = fs::read(&path)?;
            // What this pass did, not what we guess the harness did: `base` is
            // a whole-snapshot read with no position to start from, `append` a
            // read that started from a validated position, `reset` a
            // whole-snapshot read while a position existed.
            let reason = if processed.reset {
                "reset"
            } else if old.is_some() {
                "append"
            } else {
                "base"
            };
            let reset_cause = if processed.reset {
                Some(if unproven.is_some() {
                    "cursor_unproven"
                } else {
                    "committed_prefix_changed"
                })
            } else {
                None
            };
            // The generations this destination's cursor already accounted for
            // before this one. A reset implies a cursor existed: a cursor
            // derived from the stage is validated against the source before it
            // is reused, so it can never fail its own prefix check.
            let prior_generations = stored
                .as_ref()
                .map(|entry| entry.shards.shard_identities.clone())
                // reason: with no stored cursor there are no generations this
                //         destination had accounted for, so the honest prior
                //         set is empty — and a reset, the only row that uses
                //         this field, cannot reach here without a cursor
                .unwrap_or_default();
            let capture = MachineLogCapture {
                schema: MACHINE_LOG_CAPTURE_SCHEMA.to_string(),
                kind: "machine-log-generation",
                machine: machine.to_string(),
                harness: record.harness.clone(),
                log_id: record.log_id.clone(),
                generation,
                bytes: bytes.len() as u64,
                sha256: sha256_hex(&bytes),
                cursor_offset: processed.cursor.offset,
                source_bytes: processed.source_bytes,
                reason,
                reset_cause,
                prior_generations: if processed.reset {
                    prior_generations
                } else {
                    Vec::new()
                },
                fidelity: crate::message_audit::FidelityMetadata {
                    source: crate::message_audit::MetadataSource::Captured,
                    fidelity: crate::message_audit::Fidelity::Raw,
                },
                captured_at_unix: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_secs())
                    // reason: a clock before the epoch cannot date this capture;
                    // the stamp is metadata and 0 is the honest "unknown time"
                    // rather than a fabricated one
                    .unwrap_or(0),
            };
            append_machine_log_capture(stage, machine, &capture)?;
        }

        let entry = MachineLogEntry {
            machine: machine.to_string(),
            harness: record.harness.clone(),
            log_id: record.log_id.clone(),
            cursor: processed.cursor,
            shards: fact,
        };
        if stored.as_ref() != Some(&entry) {
            logs.insert(key, entry);
            save_machine_log_state(&state_path, &state, &destination_id, &logs)?;
            report.state_saves += 1;
        }
        report.machine_logs.push(MachineLogOutcome {
            harness: record.harness.clone(),
            log_id: record.log_id.clone(),
            source_bytes: processed.source_bytes,
            bytes_read: processed.bytes_read,
            prefix_bytes_validated: processed.prefix_bytes_validated,
            lines_written: processed.lines_written,
            generations,
            sealed_lines,
            shard: processed.shard,
            reset: processed.reset,
            unproven,
        });
        // W880: this machine log's wall time joins its harness's bucket, the
        // same way a session's does. Counts only.
        *report
            .collect_harness_ms
            .entry(record.harness.clone())
            .or_default() += record_started.elapsed().as_millis() as u64;
    }
    report.files_statted += counters.files_statted;
    // ADR-061 (SG-3): the outcome counters of the increment era, folded the
    // same way the W880 counters are.
    report.sqlite_full_exports += counters.sqlite_full_exports;
    report.sqlite_increment_passes += counters.sqlite_increment_passes;
    report.sqlite_increment_records += counters.sqlite_increment_records;
    report.sqlite_increment_bytes += counters.sqlite_increment_bytes;
    report.sqlite_growth_bytes += counters.sqlite_growth_bytes;
    Ok(())
}

/// ADR-061 (SG-3): the increment map of one snapshot-source session, kept
/// beside the state directory — `{record key → payload digest}` of every
/// record the collector has sealed to the stage whose last version is live.
/// It is the W986 grok-unions precedent applied to the increment era: the
/// state-entry cache (`OffsetEntry::opencode_record_digests` /
/// `OffsetEntry::opencode_map_digest`) stays bounded, this side file holds
/// the full map, and the map is a cache over what was *written*, so the
/// stage can rebuild it from the sealed records alone (ADR-012).
pub(crate) const OPENCODE_RECORD_MAP_DIR: &str = "opencode-session-maps";
const OPENCODE_RECORD_MAP_VERSION: u32 = 1;
/// The ruling's cap: a session's per-record digest list is stored in the
/// state entry only while it holds at most this many records; bigger maps
/// store one overall digest instead and rebuild from the stage on mismatch.
const OPENCODE_RECORD_CACHE_CAP: usize = 5000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OpenCodeRecordMapState {
    pub(crate) version: u32,
    /// The full session id (`opencode.<machine>.<sid>`).
    pub(crate) session_id: String,
    /// `{record key → payload digest}`, sorted by record key.
    pub(crate) records: BTreeMap<String, String>,
}

/// The map file of one session's increment map. Keyed by the SHA-256 of the
/// full session id, exactly like the grok-bot union file, so a session id
/// can never become a path component.
pub(crate) fn opencode_record_map_path(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir
        .join(OPENCODE_RECORD_MAP_DIR)
        .join(format!("{}.json", sha256_hex(session_id.as_bytes())))
}

pub(crate) fn safe_machine_id(machine: &str) -> bool {
    !machine.is_empty()
        && machine != "."
        && machine != ".."
        && !machine.contains('/')
        && !machine.contains('\\')
        && !machine.contains('\0')
}

/// SG-2b: Map file under `<stage>/meta/<machine>/opencode-session-maps/`,
/// ensuring the record-digest list survives reclaim when stage shard bodies are deleted.
pub fn opencode_record_map_meta_path(
    stage: &Path,
    machine: &str,
    session_id: &str,
) -> anyhow::Result<PathBuf> {
    if !safe_machine_id(machine) {
        anyhow::bail!(
            "machine `{}` cannot name an archive partition",
            crate::id::bounded_path_component(machine, 64)
        );
    }
    Ok(stage
        .join(crate::manifest::META_DIR)
        .join(machine)
        .join(OPENCODE_RECORD_MAP_DIR)
        .join(format!("{}.json", sha256_hex(session_id.as_bytes()))))
}

/// Rename a freshly written temp file onto its final name, waiting out the
/// transient denial Windows reports while another handle is open on either
/// name — a real-time scanner reading the temp file this caller just closed,
/// or the target path an uncooperative reader still holds. `std::fs::rename`
/// is `MoveFileEx` there, and a hold of that shape answers with
/// `ERROR_ACCESS_DENIED` (os error 5) or `ERROR_SHARING_VIOLATION`
/// (os error 32) for the microseconds the scan takes instead of waiting it
/// out; the denial is exactly what the one windows-latest failure in CI run
/// 39911056846 met — the run's only red test lost its first
/// `save_opencode_record_map` to "Access is denied" seconds after the same
/// temp-then-rename write pattern had passed in more than a thousand other
/// tests. A bounded wait turns that hold into the rename it was always going
/// to be; a rename still denied after a quarter of a second is a real
/// failure and is returned, because the wait is for a scan, not for a
/// permission this process lacks.
///
/// The retry arms only where the two codes are Windows' (`cfg!(windows)`),
/// so a Unix rename keeps its single attempt, but the loop itself is not
/// behind a `cfg`: it compiles and type-checks on every platform, so an
/// error in it breaks the build here rather than one OS's CI — the
/// `body_cache::touch_entry` convention for a Windows-only remedy. It also
/// deliberately has no test of its own: the hold it waits out needs a
/// scanner timing this suite cannot schedule, so the windows-latest job is
/// where the code runs (the same `touch_entry` precedent); on every other
/// platform the function is an unambiguous single rename.
fn commit_by_rename(temp: &Path, path: &Path) -> std::io::Result<()> {
    let mut attempt = 0;
    loop {
        match fs::rename(temp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let a_scanner_holds_the_name =
                    cfg!(windows) && matches!(e.raw_os_error(), Some(5) | Some(32));
                if !a_scanner_holds_the_name || attempt == 3 {
                    return Err(e);
                }
                let waits = [25, 100, 250];
                std::thread::sleep(std::time::Duration::from_millis(waits[attempt]));
                attempt += 1;
            }
        }
    }
}

/// Persist one session's increment map (tmp + rename, parent fsync like the
/// state file itself).
pub fn save_opencode_record_map(
    path: &Path,
    session_id: &str,
    records: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    if let Some(existing) = load_opencode_record_map(path, session_id) {
        if &existing == records {
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let state = OpenCodeRecordMapState {
        version: OPENCODE_RECORD_MAP_VERSION,
        session_id: session_id.to_string(),
        records: records.clone(),
    };
    let bytes = serde_json::to_vec(&state)?;
    let temp = path.with_extension("json.tmp");
    {
        let file = fs::File::create(&temp)?;
        let mut writer = std::io::BufWriter::new(file);
        writer.write_all(&bytes)?;
        writer.flush()?;
    }
    commit_by_rename(&temp, path)?;
    // Windows has no portable directory fsync; retain its state-file
    // durability convention. Unix proves the rename durable before the
    // map is trusted by a later pass.
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let dir = fs::File::open(parent)?;
        dir.sync_all()?;
    }
    Ok(())
}

/// Save the map to both `state_dir` and the stage's `meta/<machine>` tree (SG-2b).
fn save_opencode_record_map_both(
    stage: &Path,
    machine: &str,
    state_dir: &Path,
    session_id: &str,
    records: &BTreeMap<String, String>,
) -> anyhow::Result<()> {
    let meta_path = opencode_record_map_meta_path(stage, machine, session_id)?;
    save_opencode_record_map(&meta_path, session_id, records)?;
    let state_path = opencode_record_map_path(state_dir, session_id);
    save_opencode_record_map(&state_path, session_id, records)?;
    Ok(())
}

/// Load a session's increment map. A missing, unreadable or wrong-version
/// file answers `None`: unknown is never empty, and the caller degrades to
/// one full export that re-derives the map (D2) rather than trusting a file
/// it could not establish.
pub fn load_opencode_record_map(
    path: &Path,
    expected_session_id: &str,
) -> Option<BTreeMap<String, String>> {
    let bytes = fs::read(path).ok()?;
    let state: OpenCodeRecordMapState = serde_json::from_slice(&bytes).ok()?;
    // The path is derived from the session id, but a corrupt or swapped
    // file must fail on its own recorded identity, so the value inside is
    // checked too.
    if state.version != OPENCODE_RECORD_MAP_VERSION || state.session_id != expected_session_id {
        return None;
    }
    Some(state.records)
}

/// Load from `<stage>/meta/<machine>` first, then `state_dir` (SG-2b).
///
/// Deliberately asymmetric with the writers. A meta path this stage cannot
/// derive answers `None` here: the map is unknown, and the caller
/// (the increment-map reconcile) degrades to rebuilding it from the staged
/// records — the same recovery every unreadable map takes. The write path
/// (`save_opencode_record_map_both`) and the reclaim pre-pass propagate
/// the same derivation error instead: there, a location that cannot even
/// be named must never silently pass as saved. Reading is the fast path,
/// so its failures degrade to a rebuild; writing is the durable claim, so
/// its failures are loud.
pub(crate) fn load_opencode_record_map_any(
    stage: &Path,
    machine: &str,
    state_dir: &Path,
    session_id: &str,
) -> Option<BTreeMap<String, String>> {
    let meta_path = opencode_record_map_meta_path(stage, machine, session_id).ok()?;
    if let Some(records) = load_opencode_record_map(&meta_path, session_id) {
        return Some(records);
    }
    let state_path = opencode_record_map_path(state_dir, session_id);
    load_opencode_record_map(&state_path, session_id)
}

/// The overall digest of one increment map — the bounded shape a session
/// past the cache cap stores in its state entry.
fn opencode_map_overall_digest(records: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (key, digest) in records {
        hasher.update(key.as_bytes());
        hasher.update([0]);
        hasher.update(digest.as_bytes());
        hasher.update([b'\n']);
    }
    sha256_hex(&hasher.finalize())
}

/// The entry-shaped cache of one map: the per-record digest list while the
/// map is small, one overall digest once it passes the cap (ADR-061 ruling 2).
fn opencode_cache_of_map(
    records: &BTreeMap<String, String>,
) -> (Option<Vec<String>>, Option<String>) {
    if records.len() <= OPENCODE_RECORD_CACHE_CAP {
        (Some(records.values().cloned().collect()), None)
    } else {
        (None, Some(opencode_map_overall_digest(records)))
    }
}

/// Does the map agree with the cache shape the state entry endorses? An
/// entry with neither field, or with both, endorses nothing.
fn opencode_map_matches_entry(records: &BTreeMap<String, String>, entry: &OffsetEntry) -> bool {
    match (
        entry.opencode_record_digests.as_ref(),
        entry.opencode_map_digest.as_ref(),
    ) {
        (Some(list), None) => {
            let mut sorted_values: Vec<String> = records.values().cloned().collect();
            let mut sorted_list = list.clone();
            sorted_values.sort();
            sorted_list.sort();
            sorted_values == sorted_list
        }
        (None, Some(overall)) => *overall == opencode_map_overall_digest(records),
        _ => false,
    }
}

/// Process one opencode session record (ADR-061, SG-3).
///
/// The cursor fast path is unchanged: a session whose logical cursor still
/// equals the stored one skips for free. A session the store did move is
/// read in full (the serialize-and-hash sweep — the saving is on write
/// bytes, not read bytes), and then one of two write shapes applies:
///
/// * a **full observation**: the whole session as one v1 line, exactly the
///   pre-increment shape. It is the first sight of a session, the
///   forced-reset reread, and D2's one-pass degrade of an unknown increment
///   map — a complete observation, so D3's implicit tombstones apply to it.
///   Every full pass (re)derives the increment map from the sweep it just
///   read, which is what makes the degrade path data-safe.
/// * a **record increment**: message-row records, part records, a
///   session-observation record when the session row itself changed, and
///   explicit tombstones for records the source dropped — one shard holding
///   one line per record. Records whose `(id → digest)` already equal the
///   map are never re-serialized (the unchanged-content guard stays as a
///   second dedup layer beneath the map); a pass whose cursor moved without
///   a single record changing writes nothing at all and only advances the
///   cursor.
fn process_opencode(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    state_dir: &Path,
    machine: &str,
    bucket_cap: usize,
    store_fingerprint: &str,
    counters: &mut CollectCounters,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
) -> anyhow::Result<Processed> {
    let session_native = record
        .id
        .splitn(3, '.')
        .nth(2)
        .ok_or_else(|| anyhow!("invalid opencode session id"))?;
    let cursor = opencode_session_cursor(&record.absolute_path, session_native)
        .map_err(|error| anyhow!("failed to read opencode session cursor: {error}"))?;
    // This session's own fields, and nothing else — see
    // `process_sqlite` for why the store-wide fingerprint is not here.
    if !force_reset && old.is_some_and(|entry| entry.opencode.as_ref() == Some(&cursor)) {
        return Ok(unchanged_sqlite(
            record,
            old.expect("checked above"),
            store_fingerprint,
            Default::default(),
        ));
    }

    // The serialize-and-hash sweep: one SQLite transaction, every row, and
    // the same serializer the assembled views use for the export line.
    let sweep = crate::sqlite_probe::read_opencode_sweep(&record.absolute_path, session_native)
        .map_err(|error| anyhow!("failed to read opencode session sweep: {error}"))?;
    let snapshot = crate::sqlite_probe::OpenCodeSessionSnapshot {
        cursor: sweep.cursor.clone(),
        json_line: crate::opencode_records::compose_v1_line(
            &sweep.session_row,
            &sweep.message_rows,
            &sweep.part_rows,
        )
        .map_err(|error| anyhow!("failed to compose opencode session: {error}"))?,
    };
    let source_bytes = snapshot.json_line.len() as u64;
    let digest = sha256_hex(&snapshot.json_line);

    // The fresh increment map from the sweep the pass just read: what a
    // *complete observation* of the source's current state would deliver.
    let mut fresh_map = BTreeMap::new();
    {
        use crate::opencode_records::{payload_digest, record_key, RecordKind};
        let session_id_column = sweep
            .session_row
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow!("opencode session row has no id"))?;
        fresh_map.insert(
            record_key(RecordKind::Session, session_id_column),
            payload_digest(&sweep.session_row),
        );
        for row in &sweep.message_rows {
            let Some(id) = row.get("id").and_then(serde_json::Value::as_str) else {
                return Err(anyhow!("opencode message row has no id"));
            };
            fresh_map.insert(record_key(RecordKind::Message, id), payload_digest(row));
        }
        for row in &sweep.part_rows {
            let Some(id) = row.get("id").and_then(serde_json::Value::as_str) else {
                return Err(anyhow!("opencode part row has no id"));
            };
            fresh_map.insert(record_key(RecordKind::Part, id), payload_digest(row));
        }
    }

    let increment_era = !force_reset
        && old.is_some_and(|entry| {
            entry.opencode_record_digests.is_some() || entry.opencode_map_digest.is_some()
        });

    // What does this pass already hold? Three eras, one rule (ADR-012: the
    // archive/stage is the truth, the state a cache over it):
    //
    // * an increment-era entry reconciles its endorsement — the map file
    //   first, then a rebuild from the staged records — and anything that
    //   cannot be endorsed degrades to a full observation below;
    // * a fresh destination (no entry of its own) folds the stage this
    //   machine already holds: if the fold equals the fresh map there is
    //   nothing to seal (W283, one session shared by every destination), and
    //   a difference is delivered as the records that are missing — never
    //   as a whole-file re-export;
    // * a pre-increment entry or a forced reset has no map: unknown is
    //   never "no new messages", so the pass below writes one complete
    //   observation, which re-derives the map (D2).
    let stored_map = if force_reset {
        None
    } else if let Some(entry) = old {
        if increment_era {
            reconcile_opencode_increment_map(entry, state_dir, &record.id, stage, machine)
        } else {
            None
        }
    } else {
        fold_opencode_staged_map(stage, machine, &record.id)
            .ok()
            .flatten()
    };

    let outcome = |lines_written: usize, shard: Option<String>, reset: bool| CollectOutcome {
        session_prefix: id_prefix(&record.id),
        source_path_sha256: path_digest(&record.absolute_path),
        source_bytes,
        bytes_read: source_bytes,
        prefix_bytes_validated: 0,
        lines_written,
        shard,
        reset,
        compressed: false,
    };
    let entry_state = |cursor: crate::sqlite_probe::OpenCodeCursor,
                       records: &BTreeMap<String, String>| {
        let (record_digests, map_digest) = opencode_cache_of_map(records);
        OffsetEntry {
            offset: source_bytes,
            prefix_len: source_bytes,
            prefix_sha256: digest.clone(),
            compressed: false,
            opencode: Some(cursor),
            store_fingerprint: Some(store_fingerprint.to_string()),
            grok_bot_row_digests: None,
            opencode_record_digests: record_digests,
            opencode_map_digest: map_digest,
        }
    };

    if let Some(stored_map) = stored_map {
        // ---- the increment pass ----
        use crate::opencode_records::{record_line, tombstone_line, RecordKind};
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut growth_bytes: u64 = 0;
        // Session observation record: only when the session row's own bytes
        // changed (in practice: exactly the passes that observed change).
        let session_key = fresh_map
            .keys()
            .find(|key| key.starts_with("session:"))
            .cloned();
        if let Some(session_key) = session_key {
            if stored_map.get(&session_key) != fresh_map.get(&session_key) {
                let id = session_key.split_once(':').expect("record key").1;
                let row = &sweep.session_row;
                growth_bytes += serde_json::to_vec(row)
                    .map(|bytes| bytes.len())
                    // reason: serde only fails on non-string object keys,
                    // which a sqlite row never produces; a zero shrink of Δ
                    // only tightens the hourly ceiling the gate checks.
                    .unwrap_or(0) as u64;
                lines.push(record_line(session_native, RecordKind::Session, id, row));
            }
        }
        for row in &sweep.message_rows {
            let Some(id) = row.get("id").and_then(serde_json::Value::as_str) else {
                return Err(anyhow!("opencode message row has no id"));
            };
            let key = crate::opencode_records::record_key(RecordKind::Message, id);
            if stored_map.get(&key) == fresh_map.get(&key) {
                continue; // delivered already, identical bytes
            }
            growth_bytes += serde_json::to_vec(row)
                .map(|bytes| bytes.len())
                // reason: same as the session row above — a zero shrink of
                // Δ only tightens the hourly ceiling.
                .unwrap_or(0) as u64;
            lines.push(record_line(session_native, RecordKind::Message, id, row));
        }
        for row in &sweep.part_rows {
            let Some(id) = row.get("id").and_then(serde_json::Value::as_str) else {
                return Err(anyhow!("opencode part row has no id"));
            };
            let key = crate::opencode_records::record_key(RecordKind::Part, id);
            if stored_map.get(&key) == fresh_map.get(&key) {
                continue;
            }
            growth_bytes += serde_json::to_vec(row)
                .map(|bytes| bytes.len())
                // reason: same as the session row above — a zero shrink of
                // Δ only tightens the hourly ceiling.
                .unwrap_or(0) as u64;
            lines.push(record_line(session_native, RecordKind::Part, id, row));
        }
        // Explicit tombstones for records the source dropped: the increment
        // era must mark its deletions, or the assembled default view could
        // never equal the source's final state. Sorted by record key for a
        // deterministic shard.
        let mut deaths: Vec<String> = stored_map
            .keys()
            .filter(|key| !fresh_map.contains_key(*key))
            .cloned()
            .collect();
        deaths.sort();
        for key in &deaths {
            let Some((kind, id)) = crate::opencode_records::parse_record_key(key) else {
                continue;
            };
            lines.push(tombstone_line(session_native, kind, &id));
        }

        if lines.is_empty() {
            // The cursor moved, and not one record changed: a validated new
            // observation that writes nothing (ADR-024's rule, kept) — the
            // map subsumes the whole-export guard in this era.
            save_opencode_record_map_both(stage, machine, state_dir, &record.id, &fresh_map)?;
            counters.sqlite_increment_passes += 1;
            return Ok(Processed {
                state: entry_state(snapshot.cursor, &fresh_map),
                outcome: outcome(0, None, false),
                dimensions: Default::default(),
            });
        }

        let increment_bytes: u64 = lines.iter().map(|line| line.len() as u64).sum();
        let mut owned_lines = Vec::with_capacity(lines.len());
        for mut line in lines {
            line.push(b'\n');
            owned_lines.push(line);
        }
        let shard = Some(store::write_sealed_shard_bytes_allow_exact_repeat_with_cap(
            store::StageWriter::Collect,
            stage,
            machine,
            &record.id,
            &owned_lines,
            bucket_cap,
        )?);
        save_opencode_record_map_both(stage, machine, state_dir, &record.id, &fresh_map)?;
        counters.sqlite_increment_passes += 1;
        counters.sqlite_increment_records += owned_lines.len() as u64;
        counters.sqlite_increment_bytes += increment_bytes;
        counters.sqlite_growth_bytes += growth_bytes;
        return Ok(Processed {
            state: entry_state(snapshot.cursor, &fresh_map),
            outcome: outcome(owned_lines.len(), shard, old.is_some() || force_reset),
            dimensions: Default::default(),
        });
    }

    // ---- the full-observation path ----
    // First sight (a stage the fold could not answer from), forced reread,
    // or the map the entry endorses was not recoverable (D2's degrade). A
    // whole-line observation is a complete observation, so this pass
    // re-derives the map it just serialized — which is what lets the next
    // changed pass go incremental again.
    //
    // Guards: the ADR-024 unchanged-content layer, applied only to entries
    // that never entered the increment era. In the increment era the map
    // subsumes it, and a degrade must write — the entry's offset fields
    // describe the last observed export, not bytes this entry may skip.
    // (First sight with a stage already folded above never lands here.)
    let matches_manifest = !force_reset
        && old.is_some()
        && manifest_holds_this_export(manifest_facts, &record.id, &snapshot.json_line);
    let guards_apply = matches_manifest
        || (!force_reset
            && old.is_some_and(|entry| {
                entry.opencode_record_digests.is_none() && entry.opencode_map_digest.is_none()
            }));
    if guards_apply {
        if matches_manifest
            || old.is_some_and(|entry| export_content_matches(entry, source_bytes, &digest))
        {
            save_opencode_record_map_both(stage, machine, state_dir, &record.id, &fresh_map)?;
            return Ok(Processed {
                state: entry_state(snapshot.cursor, &fresh_map),
                outcome: outcome(0, None, false),
                dimensions: Default::default(),
            });
        }
    }
    let lines = vec![snapshot.json_line.clone()];
    // An advanced per-session cursor is a validated source delta; identical
    // export bytes can still represent a new observation.
    let shard_writer = if incremental_write_allowed(old, &snapshot.cursor) {
        store::write_sealed_shard_bytes_allow_exact_repeat_with_cap
    } else {
        store::write_sealed_shard_bytes_with_cap
    };
    let shard = Some(shard_writer(
        store::StageWriter::Collect,
        stage,
        machine,
        &record.id,
        &lines,
        bucket_cap,
    )?);
    save_opencode_record_map_both(stage, machine, state_dir, &record.id, &fresh_map)?;
    counters.sqlite_full_exports += 1;
    counters.sqlite_growth_bytes += source_bytes;
    Ok(Processed {
        state: entry_state(snapshot.cursor, &fresh_map),
        outcome: outcome(1, shard, old.is_some() || force_reset),
        dimensions: Default::default(),
    })
}

/// The pre-change rule, verbatim: whose writer gets the exact-repeat
/// allowance on the full-observation path.
fn incremental_write_allowed(
    old: Option<&OffsetEntry>,
    cursor: &crate::sqlite_probe::OpenCodeCursor,
) -> bool {
    old.is_some_and(|entry| {
        entry
            .opencode
            .as_ref()
            .is_some_and(|previous| previous != cursor)
    })
}

/// Fold the session's staged shard set into its live record map — the map
/// the stage proves by itself (ADR-012: the stage is the truth a cursor
/// claims to copy). An empty set answers `Ok(None)`: unknown, never an empty map.
/// An unreadable file or an unfoldable body returns an error so it is never
/// silently deleted without a map.
pub(crate) fn fold_opencode_staged_map(
    stage: &Path,
    machine: &str,
    session_id: &str,
) -> anyhow::Result<Option<BTreeMap<String, String>>> {
    let dir = store::session_shard_dir(stage, machine, session_id);
    let mut shards = match store::sealed_shard_entries(&dir) {
        Ok(s) => s,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(e) => {
            return Err(e).with_context(|| format!("read shard entries in {}", dir.display()));
        }
    };
    shards.sort_by_key(|(seq, _)| *seq);
    if shards.is_empty() {
        return Ok(None);
    }
    let native = crate::opencode_records::native_session_id(session_id)
        .ok_or_else(|| anyhow::anyhow!("invalid opencode session id: {session_id}"))?;
    let mut assembly = crate::opencode_records::Assembly::for_session(native);
    for (_, shard_path) in &shards {
        let body = fs::read(shard_path)
            .with_context(|| format!("read shard body {}", shard_path.display()))?;
        assembly
            .feed_shard_body(&body)
            .map_err(|e| anyhow::anyhow!("fold shard body {}: {e}", shard_path.display()))?;
    }
    Ok(Some(assembly.live_map()))
}

/// Recover the increment map the state entry endorses, in ADR-012's order:
/// the map file first (the durable copy from meta or state_dir), then a rebuild
/// from the staged records (the stage is the truth the map claims to copy),
/// and only a matching result is trusted. Anything else answers `None`, and the
/// caller degrades to one full export that re-derives the map — never a silent
/// empty map, never a mismatched map.
fn reconcile_opencode_increment_map(
    entry: &OffsetEntry,
    state_dir: &Path,
    session_id: &str,
    stage: &Path,
    machine: &str,
) -> Option<BTreeMap<String, String>> {
    if let Some(records) = load_opencode_record_map_any(stage, machine, state_dir, session_id) {
        if opencode_map_matches_entry(&records, entry) {
            return Some(records); // intact fast path: no stage read at all
        }
    }
    // Rebuild from the staged records (D2: "the map is rebuilt from the
    // staged records on mismatch").
    let records = fold_opencode_staged_map(stage, machine, session_id)
        .ok()
        .flatten()?;
    if opencode_map_matches_entry(&records, entry) {
        // Restored; persist so the next pass takes the fast path. A failed
        // persist only costs the same rebuild on the next pass — the
        // verified map is in hand either way — so the write is best effort.
        save_opencode_record_map_both(stage, machine, state_dir, session_id, &records).ok();
        return Some(records);
    }
    None
}

/// The codebuff whole-file arm (ADR-054 D2/D5/D6/D9). Distinct from
/// [`process_whole_file`] because this harness's contract makes validation
/// *before sealing* part of the read: the whole `chat-messages.json` must
/// read into one buffer and parse as a JSON array — the harness's own
/// reader refuses anything else, and ours takes a torn or non-array
/// snapshot as a **skip**, never as a half-seal and never as an error.
///
/// One snapshot serves everything (D6 rev 2): the buffer that validated is
/// the buffer that seals, and the path is never re-opened between the two.
/// The harness writes atomically (temp + rename), so a replaced path leaves
/// this read holding exactly the generation it opened — the sealed bytes
/// are always the ones that validated, never a mix of two generations.
///
/// On a skip the session archives nothing **and keeps its cursor**: the
/// stored debt, if one exists, still describes the generation that *was*
/// archived, and a completed file on a later pass fails the digest
/// comparison and seals normally. The skip itself is recorded once in the
/// stage's control stream with bounded fields only, and the run reports it
/// so nothing can exit clean while a session was left unread.
fn process_codebuff_whole_file(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
    pass: &PassContext,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
) -> anyhow::Result<ReadOutcome> {
    use crate::codebuff::TranscriptRead;
    let bytes = match crate::codebuff::read_transcript(&record.absolute_path) {
        TranscriptRead::Array(bytes) => bytes,
        TranscriptRead::Rejected { bytes, reason } => {
            // The skip observation's byte size is the read's own when the
            // read got that far, else the scanner's stat — a measurement of
            // the snapshot either way, never content.
            let byte_size = bytes
                .as_ref()
                .map_or(record.byte_size, |bytes| bytes.len() as u64);
            let facts = record.session_dir.as_ref();
            let root_id = facts
                .and_then(|facts| facts.root_id.clone())
                // reason: only a record no registry root walk labelled has no root to name; the empty word says so rather than inventing one
                .unwrap_or_default();
            let session_native_id = native_session_id(record, "codebuff")?;
            crate::codebuff::append_skip_observation(
                stage,
                machine,
                crate::codebuff::SkipObservation {
                    run_id: pass.run_id.clone(),
                    pass_ts: pass.pass_ts,
                    root_id: root_id.clone(),
                    session_native_id: session_native_id.clone(),
                    byte_size,
                    reason: reason.as_str().to_string(),
                    attempt: 0,
                },
            )?;
            return Ok(ReadOutcome::Skipped(CodebuffTornSkip {
                root_id,
                session_native_id,
                reason: reason.as_str(),
                byte_size,
            }));
        }
    };
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
        // The stage already holds this exact generation, so the snapshot
        // needs no seal — but the sidecars and the recorded start time are
        // stamped from this scan too, and a stage whose metadata did not
        // keep up (a first pass after the fields were introduced) repairs
        // itself here without touching the body.
        seal_codebuff_sidecars(record, stage, machine, pass)?;
        return Ok(ReadOutcome::Processed(Processed {
            state: old.expect("checked above").clone(),
            outcome: unchanged_outcome(record, source_len, false),
            dimensions: Default::default(),
        }));
    }
    let framed = seal_framed(std::slice::from_ref(&bytes));
    if !force_reset
        && byte_equality_establishes_event_identity(record)
        && manifest_holds_this_framed(manifest_facts, &record.id, &framed)
    {
        seal_codebuff_sidecars(record, stage, machine, pass)?;
        return Ok(ReadOutcome::Processed(Processed {
            state: OffsetEntry {
                offset: source_len,
                prefix_len: source_len,
                prefix_sha256: digest,
                compressed: false,
                opencode: None,
                store_fingerprint: None,
                grok_bot_row_digests: None,
                opencode_record_digests: None,
                opencode_map_digest: None,
            },
            outcome: unchanged_outcome(record, source_len, false),
            dimensions: Default::default(),
        }));
    }
    let lines = {
        // A validated array is never zero bytes — `[]` is two — so the
        // whole-file "empty source, no shard" branch cannot occur here; a
        // zero-byte `chat-messages.json` fails validation above and skips
        // instead of sealing a no-byte body.
        vec![bytes]
    };
    let shard = Some(store::write_sealed_shard_bytes_with_cap(
        store::StageWriter::Collect,
        stage,
        machine,
        &record.id,
        &lines,
        bucket_cap,
    )?);
    let reset = old.is_some();
    seal_codebuff_sidecars(record, stage, machine, pass)?;
    Ok(ReadOutcome::Processed(Processed {
        state: OffsetEntry {
            offset: source_len,
            prefix_len: source_len,
            prefix_sha256: digest,
            compressed: false,
            opencode: None,
            store_fingerprint: None,
            grok_bot_row_digests: None,
            opencode_record_digests: None,
            opencode_map_digest: None,
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
        dimensions: Default::default(),
    }))
}

/// Stamp the codebuff capture-time sidecars beside the session's sealed
/// shards (ADR-054 D4/D5/D9): the recorded start time (its stage file is
/// what a manifest regeneration reads back, so the fact survives the
/// reclaimed body), its directory-mtime cross-check, and the whitelist
/// sidecar files — sealed as raw bytes under the same session identity,
/// content-addressed so an unchanged sidecar costs nothing and a changed
/// one appends a new generation the way a shard does.
///
/// Called on both the unchanged and the sealed paths, never on a skip
/// (D6: a session that skips seals *nothing*, sidecars included). An
/// unreadable sidecar is left unsealed: the sidecar is optional derived
/// data whose absence is a normal state (D5), and an unread that archived
/// nothing reads honestly as its still being absent.
fn seal_codebuff_sidecars(
    record: &SessionRecord,
    stage: &Path,
    machine: &str,
    pass: &PassContext,
) -> anyhow::Result<()> {
    let Some(facts) = record.session_dir.as_ref() else {
        return Ok(());
    };
    // D4: the recorded start time is written beside the shards the manifest
    // will later regenerate from, so reuse — never re-derivation from the
    // id — is what carries the value.
    if let Some(captured) = facts.captured_start.as_ref() {
        store::write_captured_start(stage, machine, &record.id, captured)?;
        // D4 cross-check: the chat directory's own mtime must not precede
        // the recorded start. A mismatch is an observation and the recorded
        // time stands — it is never overwritten by a file time, and no
        // system clock is "fixed" here either.
        if let (Some(dir_mtime), Some(start_time)) = (
            facts.dir_mtime,
            crate::codebuff::captured_start_as_system_time(captured),
        ) {
            if dir_mtime < start_time {
                crate::codebuff::append_crosscheck_observation(
                    stage,
                    machine,
                    crate::codebuff::CrossCheckObservation {
                        pass_ts: pass.pass_ts,
                        root_id: facts
                            .root_id
                            .clone()
                            // reason: no root walk labelled this record; the empty word, never an invented root
                            .unwrap_or_default(),
                        session_native_id: native_session_id(record, "codebuff")?,
                        recorded_start: captured.time.clone(),
                        recorded_start_source: captured.source.clone(),
                        dir_mtime_unix: system_time_unix_seconds(dir_mtime),
                        result: crate::codebuff::CROSSCHECK_RESULT_PRECEDES.to_string(),
                    },
                )?;
            }
        }
    }
    for extra in &facts.extra_files {
        match fs::read(&extra.path) {
            Ok(bytes) => {
                store::write_session_extra_file(stage, machine, &record.id, &extra.name, &bytes)?;
            }
            Err(_) => {
                // Absent-tolerant by contract (D5): an unreadable sidecar
                // seals nothing and blocks nothing.
            }
        }
    }
    Ok(())
}

/// Unix seconds of a `SystemTime`, negative before the epoch — the
/// cross-check record needs one bounded number, not a lossy wall-clock
/// string.
fn system_time_unix_seconds(time: std::time::SystemTime) -> i64 {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(before) => -(before.duration().as_secs() as i64),
    }
}

fn process_whole_file(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
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
            dimensions: Default::default(),
        });
    }
    let framed = seal_framed(if bytes.is_empty() {
        &[]
    } else {
        std::slice::from_ref(&bytes)
    });
    let state = OffsetEntry {
        offset: source_len,
        prefix_len: source_len,
        prefix_sha256: digest,
        compressed: false,
        opencode: None,
        store_fingerprint: None,
        grok_bot_row_digests: None,
        opencode_record_digests: None,
        opencode_map_digest: None,
    };
    if !force_reset
        && !bytes.is_empty()
        && byte_equality_establishes_event_identity(record)
        && manifest_holds_this_framed(manifest_facts, &record.id, &framed)
    {
        return Ok(Processed {
            state,
            outcome: unchanged_outcome(record, source_len, false),
            dimensions: Default::default(),
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
        state,
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
        dimensions: Default::default(),
    })
}

fn process_compressed(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
    manifest_facts: Option<&BTreeMap<String, ShardFact>>,
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
            dimensions: Default::default(),
        });
    }
    // Decoding is all-or-nothing, so a compressed cursor records the whole
    // source it observed — length and digest — whether that pass sealed
    // lines or found none to seal. That is what lets a stable source
    // converge: a pass over a byte-identical source answers from the
    // comparison above as a no-op, so a rollout whose decoded stream ends
    // mid-record is re-observed only while it is actually changing, and the
    // first pass after a real change re-decodes and seals the completed
    // record. A zero cursor here (the pre-W286 spelling) could never match
    // a nonempty source, and with `reset: old.is_some()` below it counted
    // the session as changed on every pass forever.
    let state = OffsetEntry {
        offset: source_len,
        prefix_len: source_len,
        prefix_sha256: digest,
        compressed: true,
        opencode: None,
        store_fingerprint: None,
        grok_bot_row_digests: None,
        opencode_record_digests: None,
        opencode_map_digest: None,
    };
    let decoded = zstd::stream::decode_all(&compressed[..]).context("decompress jsonl.zst")?;
    let (lines, _) = complete_lines(&decoded);
    // A snapshot whose cursor no longer matches (the file moved between the
    // roots of a movable store, or its bytes were rewritten) must not seal
    // a second copy of content the stage already holds as its tail. That is
    // the same whole-content question [`stage_prefix_entry`]'s compressed
    // arm asks a destination that has no cursor at all, asked here for a
    // stored cursor that just failed its digest comparison: the compressed
    // cursor names the whole source it observed (length and digest), so a
    // digest mismatch alone cannot say whether the DECODED content is new
    // (a genuinely changed snapshot, sealed as its own generation) or the
    // same content re-read at a new path (a re-observation, sealed as
    // nothing). A stage that disagrees decodes-and-seals exactly as before.
    let framed = seal_framed(&lines);
    // The manifest term of this disjunction is gated the way the other
    // baseline suppressions are (see [`byte_equality_establishes_event_identity`]);
    // the stage-tail term keeps main's meaning: the stage is the live
    // record of what each destination's pass has sealed here, not a
    // re-derived claim over a reclaimed body.
    let already_held = !lines.is_empty()
        && !force_reset
        && ((byte_equality_establishes_event_identity(record)
            && manifest_holds_this_framed(manifest_facts, &record.id, &framed))
            || {
                let sealed = stage_sealed_shards(stage, machine, &record.id)?;
                !sealed.is_empty() && stage_tail_is(&sealed, &framed)
            });
    if already_held {
        // The state names the content just read (length and digest — a
        // compressed cursor is always whole-source), so the next pass over
        // a stable source answers from the digest comparison; the
        // observation stays unchanged: no shard, no reset.
        return Ok(Processed {
            state,
            outcome: unchanged_outcome(record, source_len, true),
            dimensions: Default::default(),
        });
    }
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
    // Decoding is all-or-nothing, so a compressed cursor records the whole
    // source it observed. The comment above the hoisted `state` explains the
    // convergence this buys; the mismatch paths re-derive it from the bytes
    // they just read, which is the same statement made again.
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
        dimensions: Default::default(),
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
    counters: &mut CollectCounters,
) -> anyhow::Result<ReadData> {
    for _ in 0..READ_RETRIES {
        let before = fs::metadata(path)?.len();
        // W880: one source file stat'ed.
        counters.files_statted += 1;
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
                // W880: one source file stat'ed.
                counters.files_statted += 1;
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
        // W880: one source file stat'ed.
        counters.files_statted += 1;
        if after != before {
            continue;
        }
        return Ok(ReadData {
            source_len: before,
            base_offset: 0,
            bytes: full,
            bytes_read: before,
            // B95: the old expression was `reusable.map(|e| e.offset).unwrap_or(0)`
            // with the reason "reusable is None, so nothing was validated". That
            // reason described only half the paths that land here. The other half
            // is `reusable == Some(entry)` whose recorded prefix hash *failed* the
            // check above — and there the expression handed back `entry.offset` as
            // "validated bytes" for a prefix just proven wrong, which `collect`
            // prints as `prefix_validated=N`. Whichever way we arrive, the code
            // below re-reads the file whole from offset 0 and validates no prefix.
            // reason: reaching here means no prefix passed validation — either there was no reusable entry to begin with,
            //         or the reusable entry's prefix hash just failed validation; in both cases validated prefix bytes are 0
            prefix_bytes_validated: 0,
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
        store_fingerprint: None,
        grok_bot_row_digests: None,
        opencode_record_digests: None,
        opencode_map_digest: None,
    })
}

/// Load the debt state.
///
/// A state file that cannot be parsed, or that carries a version this build
/// does not write, is *not* an error and is *not* migrated: it cannot prove
/// anything to any destination, so it is discarded and every source becomes
/// unread. Blocking the run instead would be the one outcome ADR-012 rules
/// out — the archive is the truth, and it is still reachable.
pub(crate) fn load_state(path: &Path) -> anyhow::Result<DebtState> {
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

/// Re-key this destination's Grok Bot debts from their source paths onto
/// their sessions (W986), in memory, before any lookup.
///
/// A state file written before the session key holds grok-bot entries under
/// each path the session was ever read from — `<path>\0<session id>` — so
/// one session collected from two paths holds two entries, and the new key
/// must not leave the second one unread-behind-a-path-name. Every entry
/// still names its session (`session_id`), so the migration needs nothing
/// but the state file itself:
///
/// * one path entry seeds its session's delivered set;
/// * several paths that read one session take the **union** of their
///   delivered sets — each entry is a delivery that happened, and no row
///   any path delivered is owed twice;
/// * an entry whose delivered digests are *unknown* (`grok_bot_row_digests`
///   `None`, a cursor a build wrote without a digest set) keeps the session
///   unknown: the union of an unknown with anything stays unknown, so the
///   session re-delivers once and is measured from then on — a re-keying
///   must never claim more than some path actually proved;
/// * every other field — the shard set the debt is accountable for, the
///   cursor's byte bookkeeping — is carried by the most advanced entry,
///   and the merged entry still has to discharge itself against the stage
///   or the destination archive exactly as before. Re-keying changes which
///   name an entry answers under, never what it is believed for
///   (ADR-012/013).
///
/// Idempotent: an entry already keyed by its session is kept where it is
/// and a session holding both new and old entries (a downgrade, then)
/// merges them all into one. Entries of other harnesses are untouched:
/// the moved entries are chosen by their session id, never by the shape of
/// their key, so no path-addressed key of another harness can be mistaken
/// for one of ours.
pub(crate) fn migrate_grok_bot_debts(files: &mut BTreeMap<String, DebtEntry>) {
    let original = std::mem::take(files);
    let mut by_session: BTreeMap<String, Vec<DebtEntry>> = BTreeMap::new();
    for (key, entry) in original {
        if key != grok_bot_session_key(&entry.session_id) && is_grok_bot_session(&entry.session_id)
        {
            by_session
                .entry(entry.session_id.clone())
                .or_default()
                .push(entry);
        } else {
            files.insert(key, entry);
        }
    }
    for (session_id, entries) in by_session {
        if let Some(merged) = merge_grok_bot_session_debts(entries) {
            files.insert(grok_bot_session_key(&session_id), merged);
        }
    }
}

/// Merge one session's path-keyed debt entries into the single entry its
/// session key will hold (see [`migrate_grok_bot_debts`]). `None` cannot
/// happen for a nonempty group, but the merge stays total — a degenerate
/// state file must re-deliver, never panic.
fn merge_grok_bot_session_debts(entries: Vec<DebtEntry>) -> Option<DebtEntry> {
    // Union of what the paths proved they delivered; `None` holds only
    // while the vector is empty, which the caller never passes.
    let Some(mut primary) = entries.first().cloned() else {
        return None;
    };
    let mut delivered: BTreeSet<String> = BTreeSet::new();
    let mut delivery_unknown = false;
    for entry in &entries {
        match entry.cursor.grok_bot_row_digests.as_ref() {
            Some(digests) => delivered.extend(digests.iter().cloned()),
            None => delivery_unknown = true,
        }
        if debt_advance(entry) > debt_advance(&primary) {
            primary = entry.clone();
        }
    }
    if delivery_unknown {
        // The unknown outranks every measured set: another path's digests
        // cannot narrow what the digest-less entry delivered, so the
        // session re-delivers once under its own key and carries a measured
        // set from then on.
        primary.cursor.grok_bot_row_digests = None;
    } else {
        primary.cursor.grok_bot_row_digests = Some(delivered.into_iter().collect());
    }
    Some(primary)
}

/// How far along the archive's own record one debt entry is: the sealed
/// bytes its shard set witnessed, then the shards themselves. Only the
/// entry carrying bookkeeping for a merged group is picked with this
/// (`merge_grok_bot_session_debts`); deliverability is decided by the
/// delivered-set union, never by this rank.
fn debt_advance(entry: &DebtEntry) -> (u64, usize) {
    (entry.shards.concat_bytes, entry.shards.shard_count)
}

/// Persist `debts` as this destination's slice, leaving every other
/// destination's slice byte-for-byte as it was loaded.
pub(crate) fn save_state(
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

/// The debt key of one Grok Bot session: the session's own identity, never
/// its source path (W986).
///
/// The delivered set decides what a grok-bot pass writes, and the same agent
/// is read again through a new path whenever its persistence tree moves —
/// the app relocating `sand-client-persistence`, a second persistence
/// directory under the app root, or a `[harness_roots]` rescue feed. A
/// path-keyed entry made every new path a first read that re-delivered and
/// re-sealed every union row, so the delivery identity is the one the
/// archive already uses everywhere else: the session id
/// `grok-bot.<machine>.<agent-uuid>` (machine + agent uuid, GB-5 §5), the
/// same identity the union state file and the stage shards answer for.
///
/// The sentinel stands where a source path would stand in the other keys of
/// this map; no path a filesystem can name equals it, so a session-keyed
/// entry can never collide with another harness's path-addressed key.
const GROK_BOT_SESSION_DEBT_KEY: &str = "grok-bot-session";

pub(crate) fn grok_bot_session_key(session_id: &str) -> String {
    format!("{GROK_BOT_SESSION_DEBT_KEY}\0{session_id}")
}

/// Whether a session id belongs to the Grok Bot harness. Its first dotted
/// component is the harness short name the registry ids share, read through
/// the same vocabulary rather than a string convention this file invents.
fn is_grok_bot_session(session_id: &str) -> bool {
    session_id
        .split('.')
        .next()
        .and_then(crate::models::HarnessSource::from_id)
        .is_some_and(|source| source == crate::models::HarnessSource::GrokBot)
}

fn state_key(record: &SessionRecord) -> String {
    match record.sqlite_layout {
        // Grok Bot delivery is session-keyed (see [`grok_bot_session_key`]):
        // the source path contributes nothing, on purpose.
        Some(SqliteSessionLayout::GrokBot) => grok_bot_session_key(&record.id),
        Some(_) => format!("{}\0{}", source_key(&record.absolute_path), record.id),
        None => source_key(&record.absolute_path),
    }
}

fn path_digest(path: &Path) -> String {
    sha256_hex(source_key(path).as_bytes())
}

pub(crate) fn id_prefix(id: &str) -> String {
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

// ------------------------------------------------------------------- W988: backup import

/// Provenance stamped on every shard a backup import seals (W988).
///
/// It names the backup the rows were read from by its **basename only** —
/// the same convention the takeout importer's raw record follows ("only the
/// file *name* is recorded, never its directory — an absolute path in the
/// stage would publish the shape of the machine that wrote it") — plus the
/// import run's own moment. Nothing else: no absolute paths, no home
/// directories, no account identifiers.
#[derive(Debug, Clone)]
pub struct GrokBotImportProvenance {
    /// Basename of the backup directory this pass read.
    pub backup_name: String,
    /// When the import pass ran, RFC 3339 UTC.
    pub imported_at: String,
}

/// Provenance carried by a grok-bot shard a **pass** sealed — a read of the
/// session that is not the ordinary live collect:
///
/// * [`Self::BackupImport`] (W988) — rows read from a copied local cache;
/// * [`Self::Backfill`] (BF-GROKBOT-1, ADR-058 D1) — rows the app itself
///   loaded into view and the backfill harvested.
///
/// One vocabulary on the `_chat_stasher` metadata line: each variant brings
/// its own identifying fields and the reader skips the fields of whichever
/// one it does not know, so no variant breaks an older body.
#[derive(Debug, Clone)]
pub enum GrokBotPassProvenance {
    BackupImport(GrokBotImportProvenance),
    Backfill(GrokBotBackfillProvenance),
}

/// Provenance stamped on every shard an app-driven backfill run sealed. The
/// same convention the backup import follows: no absolute paths, no home
/// directories, no account identifiers — a short run id, the app version the
/// run drove (unknown reported as unknown), and when the run ran.
#[derive(Debug, Clone)]
pub struct GrokBotBackfillProvenance {
    /// Short id of this backfill run.
    pub run_id: String,
    /// Version of the app the run drove, best effort from its bundle.
    pub app_version: Option<String>,
    /// When the run started, RFC 3339 UTC.
    pub ran_at: String,
}

/// Per-session outcome of one backup import pass. Counts and short ids only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokBotImportSession {
    /// Short session-id prefix, privacy-safe.
    pub session_prefix: String,
    /// Rows this pass delivered — or would deliver, through `--dry-run`.
    pub rows_new: u64,
    /// Rows the backups hold that the session had already delivered before
    /// this pass, deduplicated across overlapping backups.
    pub rows_already_delivered: u64,
    /// Shards this pass sealed — or would seal, through `--dry-run`.
    pub shards_written: usize,
    /// The session's stored cursor could not prove itself (or had never
    /// recorded a digest set), so the pass re-delivered its whole union once
    /// and is measured from then on.
    pub reread: bool,
}

/// What one backup import pass decided, and nothing it read.
///
/// Counts, short ids and backup basenames only. No conversation text, no
/// session bodies, no absolute paths cross this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrokBotImportReport {
    /// Whether this pass planned only, without writing anywhere.
    pub dry_run: bool,
    /// The destination whose delivered state this pass extended.
    pub destination_id: String,
    /// Distinct sessions seen across all the backups.
    pub sessions_seen: usize,
    pub rows_new: u64,
    pub rows_already_delivered: u64,
    pub shards_written: usize,
    /// Backups that were read whole but hold no Grok Bot replicas. An honest
    /// zero, not an error: an empty cache copy imports nothing.
    pub empty_backups: Vec<String>,
    /// Sessions that could not be turned into records (e.g. an unreadable
    /// replica blob), each named by why. A partial pass.
    pub errors: Vec<String>,
    /// One row per session seen, sorted by session id.
    pub sessions: Vec<GrokBotImportSession>,
}

/// One (backup, agent) pair the import pass will process.
#[derive(Debug)]
struct BackupSessionItem {
    /// The session this backup row addresses, `grok-bot.<machine>.<agent>`.
    session_id: String,
    /// Replica blob path inside the backup, exactly what a live scan's
    /// `SessionRecord.absolute_path` holds.
    source_path: PathBuf,
    /// The backup's own observation of this agent, already read to the end.
    observed: crate::grok_bot::AgentReplica,
    provenance: GrokBotImportProvenance,
}

/// Read one backup directory, read-only, into its per-agent observations.
///
/// A backup can be either shape a copy of the app's data takes: a whole
/// app-support tree (which may bury `sand-client-persistence` anywhere below
/// it) or the persistence directory itself. The app-support walk accepts
/// both without guessing when it finds replicas the direct read cannot find,
/// and refuses — rather than reporting an honest-looking zero — when a
/// directory this pass was pointed at cannot be read at all.
fn read_grok_bot_backup(dir: &Path) -> anyhow::Result<Vec<crate::grok_bot::AgentReplica>> {
    let agents = crate::grok_bot::read_from_app_support(dir)?;
    if !agents.is_empty() {
        return Ok(agents);
    }
    // The direct read is the fallback so that a copy of the persistence leaf
    // itself — not of a tree above it — is also a first-class backup. The
    // read is read-only; nothing in the backup is ever created or changed.
    crate::grok_bot::read_persistence(dir)
}

/// Import one or more Grok Bot cache backups into the collector's own
/// delivery state (GB-3b, W988).
///
/// This is the first-class shape of what the GB-3 rescue did through a
/// temporary `[harness_roots]` feed: a user can feed copied persistence trees
/// in once, as a command, without touching the config or the live cache. The
/// pass is read-only on every backup (and never reads the live cache at
/// all), and it reuses the session's own delivery identity, so the
/// guarantees are structural:
///
/// * **Only rows the session has not delivered** are sealed — the delivered
///   set in the session's debt entry decides (W986 identity: machine +
///   agent uuid). The union of several overlapping backups — or of a backup
///   and the live cache — is delivered once, with no duplicates and no
///   loss of already-delivered rows.
/// * **Idempotent**: running the same import again delivers 0 rows, and a
///   pass that delivers nothing writes nothing at all — no shard, no debt
///   save, no union write, no provenance row — so state files it did not
///   need stay byte-identical.
/// * **The live path survives untouched**: the import only extends the
///   session's delivered set and union monotonically — the same operations
///   a live pass performs — and moves no cursor: no reset, no re-keying, no
///   other harness's or destination's state. An ordinary collect after an
///   import therefore delivers only genuinely new rows and does not
///   re-deliver what the import delivered.
/// * **Verification before reuse**: a stored debt still has to discharge
///   itself against the stage or the destination's archive before its
///   delivered set is believed (ADR-012/013); one that cannot be verified is
///   re-delivered once, exactly as `collect` would.
/// * **`dry_run` writes nothing** — the identical planning computation happens
///   in memory, no stage path, no state path, no directory is created.
///
/// Errors: a backup directory that cannot be read to the end fails the whole
/// pass before anything is delivered (nothing is written, so a re-run re-does
/// all of it); per-session failures after that are collected in
/// [`GrokBotImportReport::errors`] and are a partial pass.
pub fn import_grok_bot_backups(
    backups: &[PathBuf],
    stage: &Path,
    machine: &str,
    state_dir: &Path,
    bucket_cap: usize,
    destination: &DestinationView<'_>,
    dry_run: bool,
) -> anyhow::Result<GrokBotImportReport> {
    // Read every backup to the end, before writing anything anywhere. A
    // backup that cannot be read is a failure of the whole pass — the same
    // refusal the takeout importer makes on a truncated export — so an
    // import either sees everything it was pointed at or writes nothing.
    let imported_at = chrono::Utc::now().to_rfc3339();
    let mut items: Vec<BackupSessionItem> = Vec::new();
    let mut empty_backups: Vec<String> = Vec::new();
    for dir in backups {
        let backup_name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| {
                anyhow!(
                    "backup directory {} has no name to record as provenance",
                    dir.display()
                )
            })?;
        let agents = read_grok_bot_backup(dir)?;
        if agents.is_empty() {
            empty_backups.push(backup_name);
            continue;
        }
        for agent in agents {
            items.push(BackupSessionItem {
                session_id: crate::id::SessionIdentity {
                    source_short: crate::models::HarnessSource::GrokBot.short(),
                    // The session id is this collector's own identity for the
                    // agent (machine + agent uuid, GB-5 §5): the same id a
                    // live scan of this machine builds, so the delivered
                    // set the import extends is the live one.
                    machine: machine.to_string(),
                    native_id: agent.agent_id.clone(),
                }
                .id(),
                source_path: agent.source_path.clone(),
                observed: agent,
                provenance: GrokBotImportProvenance {
                    backup_name: backup_name.clone(),
                    imported_at: imported_at.clone(),
                },
            });
        }
    }
    items.sort_by(|left, right| left.source_path.cmp(&right.source_path));

    if !dry_run {
        store::assert_stage_writer_audited(store::StageWriter::Collect)?;
    }
    let state_path = state_dir.join(STATE_FILE);
    let state = load_state(&state_path)?;
    let destination_id = destination.id().to_string();
    let mut debts = state
        .destinations
        .get(&destination_id)
        .cloned()
        // reason: an import against a destination never collected for starts
        // with no delivered rows, which is that destination's true state
        .unwrap_or_default()
        .files;
    // The delivered set and the union answer per session (W986 identity),
    // so a state file written before the session key is re-keyed in memory
    // exactly as a collect pass would.
    migrate_grok_bot_debts(&mut debts);
    // Deliver-mode state writing needs the directories; a dry run leaves the
    // filesystem entirely alone, down to creating the state directory.
    if !dry_run {
        fs::create_dir_all(state_dir)
            .with_context(|| format!("create collector state {}", state_dir.display()))?;
        fs::create_dir_all(state_dir.join("grok-bot-unions"))
            .context("create Grok Bot union state directory")?;
    }
    let manifest_facts = if dry_run {
        None
    } else {
        load_local_manifest_facts(stage, machine)
    };

    // Which sessions need the destination's own archive to prove a stored
    // debt, resolved once for the whole pass (see `DestinationView::facts`).
    let mut wanted = BTreeSet::new();
    for item in &items {
        if let Some(entry) = debts.get(&grok_bot_session_key(&item.session_id)) {
            if debt_settled_locally(entry, machine, stage, manifest_facts.as_ref())?.is_none() {
                wanted.insert((machine.to_string(), entry.session_id.clone()));
            }
        }
    }
    let remote_facts = if wanted.is_empty() {
        None
    } else {
        destination.facts(&wanted)
    };

    /// In-pass per-session accounting, shared by both modes.
    struct SessionAccount {
        prefix: String,
        rows_new: u64,
        shards_written: usize,
        reread: bool,
        /// Delivered set exactly as this pass found it, None when the
        /// session was unknown (never read, or never measured).
        delivered_before: Option<std::collections::BTreeSet<String>>,
        /// Distinct rows the backups hold for this session, by digest.
        observed_distinct: std::collections::BTreeSet<String>,
    }
    let mut accounts: BTreeMap<String, SessionAccount> = BTreeMap::new();
    // The union memory this pass reads through. Seeded from disk on the
    // first backup of an agent, extended by each observation — the same merge
    // a live pass applies — and written to disk by a delivering import, only.
    let mut union_pass: BTreeMap<String, Vec<crate::grok_bot::ReplicaRecord>> = BTreeMap::new();
    // The in-pass delivered set, one entry per session: a second backup of
    // the same session plans against the rows the first backup already
    // delivered in this same pass, exactly as the debts map carries them in
    // a delivering pass. Seeded from the session's stored debt, extended by
    // every plan — so `--dry-run` simulates the same in-pass dedup the
    // delivery would apply.
    let mut delivered_pass: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    let mut errors: Vec<String> = Vec::new();

    for item in &items {
        let key = grok_bot_session_key(&item.session_id);
        let stored = debts.get(&key).cloned();
        // A stored cursor is a claim, not a fact: it is believed only after
        // discharging itself against the stage, the local reclaim manifest,
        // or the destination's archive — the same order `collect` checks.
        let mut unverifiable = None;
        if let Some(entry) = stored.as_ref() {
            match verify_debt(entry, machine, stage, remote_facts, manifest_facts.as_ref())? {
                DebtVerdict::OwedOnStage
                | DebtVerdict::SettledInArchive
                | DebtVerdict::SettledReclaimed => {}
                DebtVerdict::Unverifiable(reason) => unverifiable = Some(reason),
            }
        }
        let old = stored.as_ref().map(|entry| entry.cursor.clone());
        let account = accounts
            .entry(item.session_id.clone())
            .or_insert_with(|| SessionAccount {
                prefix: id_prefix(&item.session_id),
                rows_new: 0,
                shards_written: 0,
                reread: false,
                delivered_before: old.as_ref().and_then(|entry| {
                    entry
                        .grok_bot_row_digests
                        .as_ref()
                        .map(|digests| digests.iter().cloned().collect())
                }),
                observed_distinct: std::collections::BTreeSet::new(),
            });
        // Every distinct row the backups hold for this session, by digest,
        // whatever happens to it below.
        let agent_id = item
            .session_id
            .splitn(3, '.')
            .nth(2)
            .ok_or_else(|| anyhow!("invalid Grok Bot agent session id"))?;
        for record in &item.observed.records {
            account
                .observed_distinct
                .insert(grok_bot_row_digest(record)?);
        }

        let union_path = grok_bot_union_path(state_dir, agent_id);
        let previous = match union_pass.get(agent_id) {
            Some(records) => records.clone(),
            None => load_grok_bot_union(&union_path)?,
        };
        let union = crate::grok_bot::merge_replica_records(previous, item.observed.records.clone());
        union_pass.insert(agent_id.to_string(), union.clone());

        let delivered: std::collections::BTreeSet<String> =
            match delivered_pass.get(&item.session_id) {
                Some(rows) => rows.clone(),
                None => old
                    .as_ref()
                    .and_then(|entry| entry.grok_bot_row_digests.as_ref())
                    .into_iter()
                    .flatten()
                    .cloned()
                    .collect(),
            };
        let deliver_all = unverifiable.is_some()
            || old.is_none()
            || old
                .as_ref()
                .is_some_and(|entry| entry.grok_bot_row_digests.is_none());
        let (to_deliver, delivered_now) = plan_grok_bot_rows(&union, &delivered, deliver_all)?;
        delivered_pass.insert(item.session_id.clone(), delivered_now.clone());
        let rows_new = to_deliver.len() as u64;
        // The live unchanged rule, verbatim: a debt that proved itself and a
        // delivery with nothing new means the pass writes nothing at all —
        // no shard, no union write, no cursor change, byte-identical state.
        // Every other first read, reread or degenerate shape falls through to
        // the delivery half, exactly as `process_grok_bot` does.
        let nothing_new = rows_new == 0 && old.is_some() && unverifiable.is_none();

        if dry_run {
            account.rows_new += rows_new;
            account.shards_written += usize::from(!nothing_new);
        } else {
            let source_bytes = match fs::metadata(&item.source_path) {
                Ok(metadata) => metadata.len(),
                Err(error) => {
                    errors.push(format!(
                        "backup replica for {} could not be inspected: {}",
                        id_prefix(&item.session_id),
                        error
                    ));
                    continue;
                }
            };
            if nothing_new {
                // Still ensure the session's provenance row exists — the
                // idempotent no-op a live pass over an unchanged session
                // performs; it writes only when the row is absent.
                crate::activity::record_session_provenance(
                    stage,
                    machine,
                    &item.session_id,
                    crate::models::HarnessSource::GrokBot,
                    &item.source_path,
                )?;
                continue;
            }
            let mut payload = Vec::new();
            for to_deliver in &to_deliver {
                serde_json::to_writer(&mut payload, &to_deliver.record.raw)
                    .context("serialize Grok Bot raw record")?;
                payload.push(b'\n');
            }
            let metadata = grok_bot_delivery_metadata(
                item.observed.name.as_deref(),
                &union,
                Some(&GrokBotPassProvenance::BackupImport(
                    item.provenance.clone(),
                )),
            );
            serde_json::to_writer(&mut payload, &metadata)
                .context("serialize Grok Bot partial metadata")?;
            payload.push(b'\n');
            store::write_sealed_shard_bytes_with_cap(
                store::StageWriter::Collect,
                stage,
                machine,
                &item.session_id,
                &[payload],
                bucket_cap,
            )?;
            write_grok_bot_union(&union_path, union.clone())?;
            account.rows_new += rows_new;
            account.shards_written += 1;
            if unverifiable.is_some()
                || old
                    .as_ref()
                    .is_some_and(|entry| entry.grok_bot_row_digests.is_none())
            {
                account.reread = true;
            }
            // A sealed shard is the pass's own evidence, so the shard fact is
            // re-read from the stage here — never swapped for stale memory
            // when nothing was sealed (the `nothing_new` branch above keeps
            // the previous fact instead, like `collect_scan_report`).
            let shards = stage_shard_fact(stage, machine, &item.session_id)?;
            let entry = DebtEntry {
                machine: machine.to_string(),
                session_id: item.session_id.clone(),
                cursor: grok_bot_delivery_cursor(source_bytes, &union, delivered_now)?,
                shards,
            };
            if stored.as_ref() != Some(&entry) {
                debts.insert(key, entry);
                save_state(&state_path, &state, &destination_id, &debts)?;
            }
            crate::activity::record_session_provenance(
                stage,
                machine,
                &item.session_id,
                crate::models::HarnessSource::GrokBot,
                &item.source_path,
            )?;
        }
    }

    let sessions: Vec<GrokBotImportSession> = accounts
        .into_iter()
        .map(|(_session_id, account)| {
            let already = account
                .delivered_before
                .as_ref()
                .map(|delivered| account.observed_distinct.intersection(delivered).count() as u64)
                // An unknown delivered set cannot prove anything was already
                // delivered, so none is counted; the reread flag on the
                // session says what happened instead.
                // reason: unknown is not the empty set — zero here is the
                // honest count of rows *proven* already delivered
                .unwrap_or_default();
            GrokBotImportSession {
                session_prefix: account.prefix,
                rows_new: account.rows_new,
                rows_already_delivered: already,
                shards_written: account.shards_written,
                reread: account.reread,
            }
        })
        .collect();
    let rows_new: u64 = sessions.iter().map(|session| session.rows_new).sum();
    let rows_already: u64 = sessions
        .iter()
        .map(|session| session.rows_already_delivered)
        .sum();
    let shards_written: usize = sessions.iter().map(|session| session.shards_written).sum();
    Ok(GrokBotImportReport {
        dry_run,
        destination_id,
        sessions_seen: sessions.len(),
        rows_new,
        rows_already_delivered: rows_already,
        shards_written,
        empty_backups,
        errors,
        sessions,
    })
}

#[cfg(test)]
mod shard_fact_tests {
    use super::*;

    fn fact(sequence: u64, body: &[u8]) -> ShardFact {
        ShardFact {
            shard_count: 1,
            concat_bytes: body.len() as u64,
            concat_sha256: sha256_hex(body),
            shard_identities: vec![ShardFingerprint {
                sequence,
                bytes: body.len() as u64,
                sha256: sha256_hex(body),
            }],
        }
    }

    #[test]
    fn subset_proof_rejects_inconsistent_aggregate_fields() {
        let archive = fact(1, b"record\n");
        let mut debt = fact(1, b"record\n");
        debt.concat_bytes += 10;
        assert!(!archive_contains_shards(&archive, &debt));

        let mut debt = fact(1, b"record\n");
        debt.shard_identities[0].sha256 = "bad".into();
        assert!(!archive_contains_shards(&archive, &debt));

        let mut archive = fact(1, b"record\n");
        archive
            .shard_identities
            .push(archive.shard_identities[0].clone());
        archive.shard_count += 1;
        assert!(!archive_contains_shards(&archive, &fact(1, b"record\n")));

        assert!(!archive_contains_shards(
            &fact(2, b"record\n"),
            &fact(1, b"record\n")
        ));
        let mut archive = fact(1, b"record\n");
        archive.shard_identities[0].sha256 = sha256_hex(b"other\n");
        assert!(!archive_contains_shards(&archive, &fact(1, b"record\n")));
    }
}

/// W986 — the re-keying of grok-bot debts from their source paths onto their
/// sessions. Every fixture digests and shard set here is synthetic; the merge
/// rules are the ones [`migrate_grok_bot_debts`] states.
#[cfg(test)]
mod grok_bot_debt_key_tests {
    use super::*;

    const SESSION: &str = "grok-bot.synthetic-machine.11111111-2222-4333-8444-555555555555";

    fn grok_entry(path: &str, digests: Option<Vec<&str>>) -> (String, DebtEntry) {
        let digests = digests.map(|digests| digests.into_iter().map(str::to_string).collect());
        let key = format!("{path}\0{SESSION}");
        (
            key,
            DebtEntry {
                machine: "synthetic-machine".to_string(),
                session_id: SESSION.to_string(),
                cursor: OffsetEntry {
                    offset: 0,
                    prefix_len: 0,
                    prefix_sha256: "synthetic-prefix".to_string(),
                    compressed: false,
                    opencode: None,
                    store_fingerprint: None,
                    grok_bot_row_digests: digests,
                    opencode_record_digests: None,
                    opencode_map_digest: None,
                },
                shards: ShardFact {
                    shard_count: 1,
                    concat_bytes: 10,
                    concat_sha256: "synthetic-concat".to_string(),
                    shard_identities: Vec::new(),
                },
            },
        )
    }

    fn other_entry(path: &str, session_id: &str) -> (String, DebtEntry) {
        // A debt of another harness: path-keyed, its own session id.
        let (_, mut entry) = grok_entry(path, None);
        entry.session_id = session_id.to_string();
        (path.to_string(), entry)
    }

    #[test]
    fn state_keyed_grok_bot_records_answer_by_session_never_by_path() {
        let from_root = |path: &str| SessionRecord {
            // The scanner fields state_key itself never reads beyond the
            // layout and the id; the rest is inert fixture here.
            id: SESSION.to_string(),
            absolute_path: std::path::PathBuf::from(path),
            byte_size: 0,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            source: crate::models::HarnessSource::GrokBot,
            compressed: false,
            sqlite_layout: Some(SqliteSessionLayout::GrokBot),
            provenance: Default::default(),
            session_dir: None,
            movable_root: false,
        };
        let path_keyed = state_key(&from_root("/synthetic/first/persistence/replica.blob"));
        // Two paths, two records: one session, one key.
        assert_eq!(
            path_keyed,
            state_key(&from_root("/synthetic/second/persistence/replica.blob"))
        );
        assert_eq!(path_keyed, grok_bot_session_key(SESSION));
        assert!(
            !path_keyed.contains("/synthetic"),
            "the delivery key must not carry the source path"
        );
    }

    #[test]
    fn path_entries_seed_their_session_and_union_their_delivered_sets() {
        let mut files = BTreeMap::new();
        let (key_a, entry_a) = grok_entry("/synthetic/one", Some(vec!["digest-one"]));
        let (key_b, entry_b) =
            grok_entry("/synthetic/two", Some(vec!["digest-two", "digest-three"]));
        let (claude_key, entry_plain) = other_entry(
            "/synthetic/claude/session.jsonl",
            "claude-code.synthetic-machine.synthetic-1",
        );
        files.insert(key_a, entry_a);
        files.insert(key_b, entry_b);
        files.insert(claude_key, entry_plain);
        let claude_before = files
            .get("/synthetic/claude/session.jsonl")
            .cloned()
            .unwrap();

        migrate_grok_bot_debts(&mut files);

        let merged = files
            .get(&grok_bot_session_key(SESSION))
            .expect("the session's entry answers under its session key");
        assert_eq!(
            merged.cursor.grok_bot_row_digests,
            Some(vec![
                "digest-one".to_string(),
                "digest-three".to_string(),
                "digest-two".to_string()
            ]),
            "the union of what the two paths delivered, sorted like every delivered set this build writes"
        );
        assert_eq!(
            files.len(),
            2,
            "two path entries became one session entry; the other harness kept exactly its own"
        );
        assert_eq!(
            files.get("/synthetic/claude/session.jsonl"),
            Some(&claude_before),
            "another harness's debt is untouched"
        );
    }

    #[test]
    fn an_unknown_delivered_set_migrates_to_unknown_not_to_a_usurped_union() {
        let mut files = BTreeMap::new();
        let (key_unknown, entry_unknown) = grok_entry("/synthetic/legacy", None);
        let (key_known, entry_known) = grok_entry("/synthetic/measured", Some(vec!["digest-one"]));
        files.insert(key_unknown, entry_unknown);
        files.insert(key_known, entry_known);

        migrate_grok_bot_debts(&mut files);

        let merged = files.get(&grok_bot_session_key(SESSION)).unwrap();
        assert_eq!(
            merged.cursor.grok_bot_row_digests, None,
            "unknown cannot be narrowed by union: the session re-delivers once and is measured from then on"
        );
    }

    #[test]
    fn the_most_advanced_entry_carries_the_shard_bookkeeping() {
        let mut files = BTreeMap::new();
        let (key_small, mut small) = grok_entry("/synthetic/a", Some(vec!["digest-one"]));
        small.shards = ShardFact {
            shard_count: 1,
            concat_bytes: 10,
            concat_sha256: "synthetic-small".to_string(),
            shard_identities: Vec::new(),
        };
        let (key_large, mut large) = grok_entry("/synthetic/b", Some(vec!["digest-two"]));
        large.shards = ShardFact {
            shard_count: 2,
            concat_bytes: 40,
            concat_sha256: "synthetic-large".to_string(),
            shard_identities: Vec::new(),
        };
        files.insert(key_small, small);
        files.insert(key_large, large);

        migrate_grok_bot_debts(&mut files);

        let merged = files.get(&grok_bot_session_key(SESSION)).unwrap();
        assert_eq!(merged.shards.shard_count, 2);
        assert_eq!(merged.shards.concat_bytes, 40);
        assert_eq!(merged.shards.concat_sha256, "synthetic-large");
    }

    #[test]
    fn re_keying_is_idempotent_and_ignores_foreign_sessions() {
        let mut files = BTreeMap::new();
        let (_, entry) = grok_entry("/synthetic/path", Some(vec!["digest-one", "digest-two"]));
        files.insert(grok_bot_session_key(SESSION), entry);

        let before = files.clone();
        migrate_grok_bot_debts(&mut files);
        assert_eq!(
            files, before,
            "an already session-keyed entry merges to itself, unchanged"
        );
    }
}

/// ADR-061 (SG-3) writer-side pins: the state-entry cache's shape (the W986
/// digest list below the cap, one overall digest above it), the endorsement
/// rule (a mismatched or half-broken cache endorses nothing), and the map
/// side file's round trip.
#[cfg(test)]
mod opencode_increment_tests {
    use super::*;

    fn entry_with(record_digests: Option<Vec<String>>, map_digest: Option<String>) -> OffsetEntry {
        OffsetEntry {
            offset: 0,
            prefix_len: 0,
            prefix_sha256: String::new(),
            compressed: false,
            opencode: None,
            store_fingerprint: None,
            grok_bot_row_digests: None,
            opencode_record_digests: record_digests,
            opencode_map_digest: map_digest,
        }
    }

    fn map_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, digest)| ((*key).to_string(), (*digest).to_string()))
            .collect()
    }

    #[test]
    fn the_state_cache_keeps_a_digest_list_up_to_the_cap_and_an_overall_digest_above_it() {
        let small = map_of(&[("message:m1", "aaaa"), ("part:p1", "bbbb")]);
        let (list, overall) = opencode_cache_of_map(&small);
        assert_eq!(
            list.as_deref(),
            Some(["aaaa".to_string(), "bbbb".to_string()].as_slice())
        );
        assert!(overall.is_none());

        let mut big = BTreeMap::new();
        for i in 0..=(OPENCODE_RECORD_CACHE_CAP as u64) {
            big.insert(format!("message:m{i}"), format!("{i:064x}"));
        }
        let (list, overall) = opencode_cache_of_map(&big);
        assert!(
            list.is_none(),
            "past the cap the bounded entry stores no list"
        );
        let overall = overall.expect("the overall digest replaces the list");
        // The bound the entry keeps is the ruling's own: 5000.
        assert_eq!(OPENCODE_RECORD_CACHE_CAP, 5000);
        // The overall digest is stable and order-anchored, so a hand-shuffled
        // cache file cannot silently re-endorse a different map.
        assert_eq!(overall, opencode_map_overall_digest(&big));
        assert_ne!(
            overall,
            opencode_map_overall_digest(&map_of(&[("message:m1", "aaaa")]))
        );
    }

    #[test]
    fn an_entry_endorses_a_map_only_through_the_exact_shape_it_stores() {
        let map = map_of(&[("message:m1", "aaaa"), ("part:p1", "bbbb")]);
        let (list, _) = opencode_cache_of_map(&map);
        assert!(opencode_map_matches_entry(
            &map,
            &entry_with(list.clone(), None)
        ));
        // A different map is not endorsed by the same list.
        let other = map_of(&[("message:m1", "aaaa"), ("part:p1", "cccc")]);
        assert!(!opencode_map_matches_entry(&other, &entry_with(list, None)));
        // Neither field alone in the wrong shape, both fields, or nothing.
        assert!(!opencode_map_matches_entry(&map, &entry_with(None, None)));
        let (list_none, _overall_from_small) = opencode_cache_of_map(&map);
        let _ = list_none;
        assert!(
            !opencode_map_matches_entry(
                &map,
                &entry_with(
                    Some(map.values().cloned().collect()),
                    Some(opencode_map_overall_digest(&map)),
                ),
            ),
            "an entry carrying both shapes endorses nothing"
        );
    }

    #[test]
    fn the_map_side_file_round_trips_and_a_wrong_version_answers_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = opencode_record_map_path(dir.path(), "opencode.m.ses");
        let map = map_of(&[("session:ses", "deadbeefcafe")]);
        save_opencode_record_map(&path, "opencode.m.ses", &map).unwrap();
        assert_eq!(
            load_opencode_record_map(&path, "opencode.m.ses"),
            Some(map.clone())
        );
        // Session ID mismatch: answers None.
        assert_eq!(
            load_opencode_record_map(&path, "opencode.m.other_ses"),
            None
        );
        // Corrupt file: unknown is not empty — None, and the caller degrades.
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(load_opencode_record_map(&path, "opencode.m.ses"), None);
        let wrong = OpenCodeRecordMapState {
            version: OPENCODE_RECORD_MAP_VERSION + 1,
            session_id: "opencode.m.ses".to_string(),
            records: map_of(&[]),
        };
        fs::write(&path, serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert_eq!(load_opencode_record_map(&path, "opencode.m.ses"), None);
        // Missing entirely.
        let other = opencode_record_map_path(dir.path(), "opencode.m.other");
        assert_eq!(load_opencode_record_map(&other, "opencode.m.other"), None);
    }

    /// The entry-level digest list is derived from a *state file* round trip:
    /// `#[serde(default)]` means a pre-change state (no fields named) loads
    /// as never-increment-era, and a state that carries them keeps them.
    #[test]
    fn a_state_entry_round_trips_the_increment_caches() {
        let entry = entry_with(Some(vec!["aaaa".to_string()]), Some("deadbeef".to_string()));
        let bytes = serde_json::to_vec(&entry).unwrap();
        let parsed: OffsetEntry = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, entry);
        let legacy = serde_json::json!({
            "offset": 0,
            "prefix_len": 0,
            "prefix_sha256": "abc",
            "compressed": false,
            "opencode": null,
            "grok_bot_row_digests": null
        });
        let parsed: OffsetEntry = serde_json::from_value(legacy).unwrap();
        assert!(parsed.opencode_record_digests.is_none());
        assert!(parsed.opencode_map_digest.is_none());
    }
}
