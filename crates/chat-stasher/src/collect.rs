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
//! full. There is no migration path and no "trust it for now" branch: a
//! measurable duplicate always beats a silent omission.

use crate::config::Config;
use crate::models::{SessionRecord, SqliteSessionLayout};
use crate::scanner;
use crate::sqlite_probe::{
    cursor_global_schema, grok_schema, opencode_session_cursor, read_cursor_legacy_session,
    read_hermes_session, read_openclaw_session, read_opencode_session, read_sqlite_session,
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
) -> anyhow::Result<Option<OffsetEntry>> {
    // A SQLite session's cursor is logical, not a file offset, so the stage
    // cannot be turned into one without re-exporting the session. Those paths
    // judge the reuse themselves, against the export they just produced.
    if record.sqlite_layout.is_some() {
        return Ok(None);
    }
    let sealed_shards = stage_sealed_shards(stage, machine, &record.id)?;
    if sealed_shards.is_empty() {
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
        || !stage_tail_is(
            &stage_sealed_shards(stage, machine, &record.id)?,
            &seal_framed(std::slice::from_ref(&bytes)),
        )
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

/// Observe what the stage currently holds for one session.
fn stage_shard_fact(stage: &Path, machine: &str, session_id: &str) -> anyhow::Result<ShardFact> {
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
fn load_local_manifest_facts(stage: &Path, machine: &str) -> Option<BTreeMap<String, ShardFact>> {
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
fn debt_settled_locally(
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
fn verify_debt(
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
    /// W880: collector state saves this pass.
    pub state_saves: u64,
}

/// W880: counters only the per-record read paths can measure. They
/// happen inside the read functions, below the report's reach, so
/// they are threaded through the record loop and folded into the
/// report once the pass ends. Counts only — never a path or id.
#[derive(Debug, Default)]
struct CollectCounters {
    files_statted: u64,
    sqlite_sessions_queried: u64,
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
    for record in records {
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
            Some(entry) => Some(entry.cursor.clone()),
            // One native UUID may appear in multiple source roots. Without a
            // persisted cursor for this exact scanned source, a matching
            // stage prefix cannot establish whether it is a replay or a new
            // repeated turn; conservatively capture measured provenance rows.
            None if !record.provenance.is_empty() => None,
            None => stage_prefix_entry(&record, stage, machine, &mut counters)?,
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
        ) {
            Ok(processed) => {
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
                    // W880: one durable collector state save.
                    report.state_saves += 1;
                }
                report.outcomes.push(outcome);
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
) -> anyhow::Result<Processed> {
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
        } else {
            process_sqlite(
                record,
                layout,
                old,
                force_reset,
                stage,
                machine,
                bucket_cap,
                counters,
            )
        }
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
            counters,
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
fn grok_bot_row_digest(record: &crate::grok_bot::ReplicaRecord) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(&record.raw).context("serialize Grok Bot raw record")?;
    Ok(sha256_hex(&bytes))
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

    let union_dir = state_dir.join("grok-bot-unions");
    fs::create_dir_all(&union_dir).context("create Grok Bot union state directory")?;
    let union_path = union_dir.join(format!("{}.json", sha256_hex(agent_id.as_bytes())));
    let previous = match fs::read(&union_path) {
        Ok(bytes) => {
            let state: GrokBotUnionState = serde_json::from_slice(&bytes).with_context(|| {
                format!("parse Grok Bot union state ({})", path_digest(&union_path))
            })?;
            if state.version != 1 && state.version != GROK_BOT_UNION_STATE_VERSION {
                bail!("unsupported Grok Bot union state version {}", state.version);
            }
            // Version 1 held at most one record per sequence — a valid
            // variant list, so it merges as-is. No version 1 state file can
            // exist outside a test run: the version 1 reader never matched a
            // real replica blob on disk (W329b).
            state.records
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error).context("read Grok Bot union state"),
    };
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
    let mut payload = Vec::new();
    let mut rows_written = 0usize;
    let mut delivered_now = delivered.clone();
    for item in &union {
        let digest = grok_bot_row_digest(item)?;
        if deliver_all || !delivered.contains(&digest) {
            serde_json::to_writer(&mut payload, &item.raw)
                .context("serialize Grok Bot raw record")?;
            payload.push(b'\n');
            delivered_now.insert(digest);
            rows_written += 1;
        }
    }
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
        });
    }
    let union_sequences: Vec<u64> = union.iter().map(|item| item.sequence).collect();
    let gaps = crate::grok_bot::sequence_gaps(union_sequences.iter().copied());
    let metadata = serde_json::json!({
        "_chat_stasher": {
            "source": "grok-bot",
            "agent_name": observed.name,
            "agent_name_state": if observed.name.is_some() { "known" } else { "unknown" },
            "partial_replica": true,
            "sequence_gaps": gaps,
            "observed_sequence_min": union_sequences.first(),
            "observed_sequence_max": union_sequences.last(),
        }
    });
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

    let state = GrokBotUnionState {
        version: GROK_BOT_UNION_STATE_VERSION,
        records: union,
    };
    let encoded = serde_json::to_vec(&state).context("serialize Grok Bot union state")?;
    let tmp = union_path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&tmp, &encoded).context("write Grok Bot union state")?;
    fs::rename(&tmp, &union_path).context("commit Grok Bot union state")?;

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
    let digest = sha256_hex(&serde_json::to_vec(&state.records)?);
    Ok(Processed {
        state: OffsetEntry {
            offset: delivered_now.len() as u64,
            prefix_len: source_bytes,
            prefix_sha256: digest,
            compressed: false,
            opencode: None,
            store_fingerprint: None,
            grok_bot_row_digests: Some(delivered_now.into_iter().collect()),
        },
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
    })
}

fn process_sqlite(
    record: &SessionRecord,
    layout: SqliteSessionLayout,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
    counters: &mut CollectCounters,
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
            return Ok(unchanged_sqlite(record, entry, &store_fingerprint));
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
            machine,
            bucket_cap,
            &store_fingerprint,
        ),
        SqliteSessionLayout::OpenClaw => process_openclaw(
            record,
            old,
            force_reset,
            stage,
            machine,
            bucket_cap,
            &store_fingerprint,
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
) -> anyhow::Result<Processed> {
    // This session's own fields, and nothing else. The whole-store fingerprint
    // was removed from `OpenCodeCursor` precisely so that this comparison
    // cannot be polluted by a write to some other session.
    if !force_reset && old.is_some_and(|entry| entry.opencode.as_ref() == Some(&cursor)) {
        return Ok(unchanged_sqlite(
            record,
            old.expect("checked above"),
            store_fingerprint,
        ));
    }
    let source_bytes = json_line.len() as u64;
    let digest = sha256_hex(&json_line);
    // No cursor of this destination's own, and the stage already holds exactly
    // this export: the destination is owed nothing, and sealing it again would
    // store the conversation twice.
    if old.is_none()
        && !force_reset
        && stage_holds_this_export(stage, machine, &record.id, &json_line)?
    {
        return Ok(unchanged_content_sqlite(
            record,
            cursor,
            store_fingerprint,
            source_bytes,
            &digest,
        ));
    }
    if let Some(entry) = old {
        if !force_reset && export_content_matches(entry, source_bytes, &digest) {
            return Ok(unchanged_content_sqlite(
                record,
                cursor,
                store_fingerprint,
                source_bytes,
                &digest,
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
) -> anyhow::Result<Processed> {
    let data = read_jsonl_delta(&record.absolute_path, old, force_reset, counters)?;
    let (lines, committed_delta) = complete_lines(&data.bytes);
    let new_offset = data.base_offset + committed_delta as u64;
    let new_state = if lines.is_empty() && !data.reset {
        old.cloned().unwrap_or(OffsetEntry {
            offset: 0,
            prefix_len: 0,
            prefix_sha256: sha256_hex(&[]),
            compressed: false,
            opencode: None,
            store_fingerprint: None,
            grok_bot_row_digests: None,
        })
    } else {
        plain_state(&record.absolute_path, new_offset)?
    };
    let shard = if lines.is_empty() {
        None
    } else if (!data.reset && data.base_offset > 0)
        || (old.is_none() && !record.provenance.is_empty())
    {
        // A validated non-zero cursor proves these are newly appended source
        // bytes. They may be byte-identical to an earlier turn (`a\n` followed
        // by another real `a\n`) and still belong in the archive.
        // A fresh provenance-bearing source is ambiguous even at offset zero:
        // byte equality with a different root cannot establish event identity.
        // Its complete capture must therefore retain the same multiplicity.
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
    Ok(())
}

fn process_opencode(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
    store_fingerprint: &str,
) -> anyhow::Result<Processed> {
    let session_id = record
        .id
        .splitn(3, '.')
        .nth(2)
        .ok_or_else(|| anyhow!("invalid opencode session id"))?;
    let cursor = opencode_session_cursor(&record.absolute_path, session_id)
        .map_err(|error| anyhow!("failed to read opencode session cursor: {error}"))?;
    // This session's own fields, and nothing else — see
    // `process_sqlite_snapshot` for why the store-wide fingerprint is not here.
    if !force_reset && old.is_some_and(|entry| entry.opencode.as_ref() == Some(&cursor)) {
        return Ok(unchanged_sqlite(
            record,
            old.expect("checked above"),
            store_fingerprint,
        ));
    }

    let snapshot = read_opencode_session(&record.absolute_path, session_id)
        .map_err(|error| anyhow!("failed to read opencode session snapshot: {error}"))?;
    let source_bytes = snapshot.json_line.len() as u64;
    let digest = sha256_hex(&snapshot.json_line);
    if old.is_none()
        && !force_reset
        && stage_holds_this_export(stage, machine, &record.id, &snapshot.json_line)?
    {
        return Ok(unchanged_content_sqlite(
            record,
            snapshot.cursor,
            store_fingerprint,
            source_bytes,
            &digest,
        ));
    }
    if let Some(entry) = old {
        if !force_reset && export_content_matches(entry, source_bytes, &digest) {
            return Ok(unchanged_content_sqlite(
                record,
                snapshot.cursor,
                store_fingerprint,
                source_bytes,
                &digest,
            ));
        }
    }
    let lines = vec![snapshot.json_line];
    // As above, an advanced per-session cursor is a validated source delta;
    // identical export bytes can still represent a new observation.
    let shard_writer = if old.is_some_and(|entry| {
        entry
            .opencode
            .as_ref()
            .is_some_and(|previous| previous != &snapshot.cursor)
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
            store_fingerprint: Some(store_fingerprint.to_string()),
            grok_bot_row_digests: None,
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
            store_fingerprint: None,
            grok_bot_row_digests: None,
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
