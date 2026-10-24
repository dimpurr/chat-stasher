//! stagereclaim — reclaim the sealed shard body from the staging tree.
//!
//! The stage is an intentionally unbounded local full copy; nothing ever
//! shrinks it. ADR-020 Phase 4 retires the shard body once the archive
//! provably holds every byte, keeping only the per-session digest summaries
//! ([`crate::manifest`]) that L3 verify uses as its baseline.
//!
//! The hard rule (ADR-060 D1, amending ADR-020's role B): **no shard is
//! deleted before the archive is proven to hold it — enforced for every
//! healthy declared destination, with at least one proof overall.** Proof
//! means asking the archive itself — never a local cursor, never "I
//! pushed, so it should be there" — for each session's digest triple
//! `(shard_count, concat_bytes, concat_sha256)` and matching it against
//! the stage.
//!
//! Who must prove is decided by destination *health*, derived from the
//! durable push record through the one classifier in [`crate::destdiag`]
//! (ADR-060 D2/D3): `healthy` (and fail-safe `unknown`) destinations form
//! the must-prove set; `lagging` and `never-pushed` destinations are
//! degraded — **not consulted**, listed with their health and their stats
//! on every run, and never allowed to hold the whole machine's reclaim
//! hostage. A degraded destination's protection is the alarm layer and the
//! union backfill (ADR-013 via ADR-021's cumulative read), never stage
//! retention. The ≥1 guard is what keeps that safe: with no destination
//! left to prove against (the empty healthy set), nothing is deleted and
//! the loud refusal of D7's own shape speaks. A must-prove destination
//! that cannot be consulted is still *unproven* and blocks: unreachable is
//! never "does not have it" nor "has it".
//!
//! The mirror precondition (ADR-060 D6(ii), 47 §5.3) is wired: on a machine
//! whose configured-and-on mirror mirrors a destination this run consults,
//! a session is proven but still **held back** — `not-mirrored` — until the
//! copy's completed-round record ([`crate::mirrorstate`]) holds the proof's
//! snapshot closure-complete. The caller hands the mirror in as
//! [`MirrorHoldback`]; `None` (no section, switch off) runs the reclaim
//! exactly as it ran before mirrors existed. A mirror whose `mirror_of` is
//! degraded and therefore not consulted binds nothing: a degraded
//! destination may not hold the whole machine's reclaim hostage (D3's own
//! rule), its protection being the alarm layer and the union backfill.
//!
//! Default is a dry run: report what would be reclaimed and delete nothing.
//! `--apply` writes each machine's retained summaries first — so the L3
//! baseline lands before the body goes — then removes every proven
//! session's shard body while keeping the session's persistent `shard-seq`
//! counter, which is what stops the next sequence from falling back onto
//! archived shard names. Deletion is per session, resumable and idempotent.

use anyhow::Context;
use rustic_core::repofile::MasterKey;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::destdiag::DestinationHealth;
use crate::manifest;
use crate::store::{self, BackupStore, StoreConfig};

/// One declared destination handed to the reclaim: its config plus its
/// name for reporting, plus the health the one classifier derived for it
/// (ADR-060 D2). The classification is the caller's job — `reclaim-stage`
/// reads the durable push record through `destdiag` the same way the
/// three surfaces do, so the reclaim and `status` can never disagree about
/// health — never a clock read or a push-state read inside this module.
///
/// The masterkey is loaded by the reclaim itself, so a destination whose
/// key cannot be read is reported `Unreachable` rather than aborting.
#[derive(Debug, Clone)]
pub struct NamedStore {
    pub name: String,
    pub cfg: StoreConfig,
    /// The derived health, consulted only through
    /// [`DestinationHealth::is_degraded`] and its slug — this module holds
    /// no second classification.
    pub health: DestinationHealth,
}

/// One row of D7's per-destination report rows: the health the run judged
/// and the proof role that fell out of it. Every declared destination gets
/// one, on every run — degradation is listed, never swallowed.
#[derive(Debug, Clone)]
pub struct DestinationProofState {
    pub name: String,
    pub health: DestinationHealth,
    /// `must-prove` / `degraded-not-consulted`, D7's words.
    pub role: &'static str,
}

/// The run's health rows in the durable record's shape (D7's snapshot):
/// one row per declared destination, recorded verbatim from the same
/// classification the report carries — the CLI calls this for every
/// outcome, so even a refused run's record says what the run saw.
pub fn destination_states_as_state(
    destinations: &[NamedStore],
) -> Vec<crate::reclaimstate::ReclaimDestinationHealth> {
    destinations
        .iter()
        .map(|dest| crate::reclaimstate::ReclaimDestinationHealth {
            destination: dest.name.clone(),
            health: dest.health.slug().to_string(),
            role: dest.health.proof_role_word().to_string(),
        })
        .collect()
}

/// A session with a body in the stage, carrying the expected digest triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub machine: String,
    pub session_id: String,
    pub shard_count: usize,
    pub concat_bytes: u64,
    pub sha256: String,
}

/// Why the reclaim is refused. `sessions` / `bytes` count what is held back.
#[derive(Debug, Clone)]
pub struct Blocked {
    /// Destination that failed the proof.
    pub destination: String,
    pub kind: BlockedKind,
    /// Sessions held back by this block.
    pub sessions: usize,
    /// Body bytes held back by this block.
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub enum BlockedKind {
    /// The destination could not be opened, or its masterkey could not be
    /// read. Unreachable is "unproven", never "does not have it".
    ///
    /// `kind` is the ADR-023 classifier's verdict for the failure when it
    /// produced one (W1012 / SG-1 C3): an R2 destination whose credentials a
    /// timer cannot resolve used to reach the user as a third-party
    /// "We believe this is a bug" chain with no verdict at all, and the
    /// same error text is what decides the advice the report now leads with.
    Unreachable {
        kind: Option<crate::remote_err::RemoteErrorKind>,
        error: String,
    },
    /// The destination was read but not to completion; its absences are
    /// unproven and therefore block, exactly like unreachable.
    PartialRead { detail: String },
    /// The destination is readable but holds no such session.
    SessionNotHeld,
    /// The destination holds the session but with a different digest triple.
    TripleMismatch,
    /// The mirror leg (ADR-060 D6(ii) / 47 §5.3): the session is proven on
    /// every must-prove destination, but the machine's configured-and-on
    /// mirror of the mirroring destination has not reached the proof's
    /// snapshot — the copy is this machine's offline fallback, so the stage
    /// body stays until the mirror has durably taken it over. The word and
    /// sentence this kind prints are [`crate::mirror::HELD_BACK_KIND`] and
    /// [`crate::mirror::HELD_BACK_SENTENCE`], the one spelling D6.3 fixed.
    NotMirrored,
}

/// What the caller states about this machine's mirror, for the mirror leg
/// of the proof (ADR-060 D6(ii) / 47 §5.3). Built by the CLI from the
/// config and the state dir — never loaded inside this module, so a test
/// can state any mirror record the fixtures write.
///
/// `None` is the no-mirror machine: no section, or `[mirror] enabled =
/// false` — the reclaim then has no mirror precondition, exactly as it
/// ran before LM-2. `Some` binds only when the consult set actually holds
/// `mirror_of` as a must-prove, successfully consulted destination;
/// a mirror pointed at a degraded destination is not a proof input.
#[derive(Debug, Clone)]
pub struct MirrorHoldback {
    /// The declared destination this machine mirrors (`[mirror] mirror_of`).
    pub mirror_of: String,
    /// The copy's directory under the data root. Coverage is a claim about
    /// bytes on disk, so a record no directory backs ([`Path::is_dir`] is
    /// false — deleted, or a stale record from a copy that is gone) vouches
    /// for nothing and the leg holds everything back.
    pub mirror_root: std::path::PathBuf,
    /// The mirror's durable completed-round record, read by the caller.
    pub state: crate::mirrorstate::MirrorStateRead,
}

/// One session whose body was deleted by `--apply`.
#[derive(Debug, Clone)]
pub struct Reclaimed {
    pub machine: String,
    pub session_id: String,
    pub shard_count: usize,
    pub bytes: u64,
}

/// Full result of a proof (and optional reclaim).
#[derive(Debug, Default)]
pub struct ReclaimReport {
    /// Destination names consulted, in the order given.
    pub destinations: Vec<String>,
    /// D7's per-destination rows — every declared destination's derived
    /// health and proof role, in the order given. Degraded destinations
    /// appear with their role even though they produced no `Blocked` row:
    /// a state the user must see may not fold into silence.
    pub destination_states: Vec<DestinationProofState>,
    /// Every session with a body in the stage, with its expected triple.
    pub candidates: Vec<Candidate>,
    /// Sessions that have a stored summary but no body (already reclaimed).
    pub already_reclaimed: usize,
    /// The empty-proof-set refusal (ADR-060 D7): every declared destination
    /// was degraded, so no destination could be consulted and the ≥1
    /// guard deleted nothing. Its own blocked shape — `blocked()` reports
    /// it, the caller prints D7's prose, the refusal exit code stays.
    pub no_healthy_prover: bool,
    /// Non-empty means the reclaim is refused and nothing was deleted.
    pub blocked: Vec<Blocked>,
    /// Only populated when `apply` ran with an empty `blocked`.
    pub reclaimed: Vec<Reclaimed>,
    /// Total body bytes across candidates.
    pub candidate_bytes: u64,
}

impl ReclaimReport {
    /// Whether the reclaim is refused (a proof failed on some destination,
    /// or no destination could prove at all — the empty healthy set).
    pub fn blocked(&self) -> bool {
        !self.blocked.is_empty() || self.no_healthy_prover
    }
}

/// The machine-readable word for one block kind, as `cmd_reclaim_stage`
/// prints it and [`crate::reclaimstate`] persists it (W1012 / SG-1): the
/// printed line and the stored record name the same failure the same way,
/// so a weekly streak computed from the file never disagrees with the log
/// the user can read.
pub fn block_kind_word(kind: &BlockedKind) -> &'static str {
    match kind {
        BlockedKind::Unreachable { .. } => "unreachable",
        BlockedKind::PartialRead { .. } => "partial-read",
        BlockedKind::SessionNotHeld => "not-held",
        BlockedKind::TripleMismatch => "triple-mismatch",
        // D6.3's fixed `--json` spelling, shared with every surface that
        // repeats it, so the report word never becomes a second vocabulary.
        BlockedKind::NotMirrored => crate::mirror::HELD_BACK_KIND,
    }
}

/// The blocked list in the durable record's shape (SG-1 C1④): per
/// destination, the block's word plus the classifier's slug when the
/// failure produced one — counts only, never error text bodies.
pub fn blocks_as_state(blocks: &[Blocked]) -> Vec<crate::reclaimstate::ReclaimBlock> {
    blocks
        .iter()
        .map(|block| crate::reclaimstate::ReclaimBlock {
            destination: block.destination.clone(),
            kind: block_kind_word(&block.kind).to_string(),
            error_kind: match &block.kind {
                BlockedKind::Unreachable { kind, .. } => kind.map(|k| k.slug().to_string()),
                _ => None,
            },
            sessions: block.sessions,
            bytes: block.bytes,
        })
        .collect()
}

/// Reclaim the stage body, proving every session against every **healthy**
/// destination first (ADR-060 D1/D3), then past the mirror leg
/// (ADR-060 D6(ii) / 47 §5.3): with a configured-and-on mirror of a
/// consulted destination, only sessions whose proof's snapshot the
/// mirror's completed-round record holds closure-complete are reclaimable
/// — the rest are held back per session as `not-mirrored`. Degraded
/// destinations are listed on every run but not consulted; the ≥1 guard
/// refuses the whole run when no destination is left to prove.
///
/// Returns `Ok(report)` even when the reclaim is refused: the refusal lives in
/// [`ReclaimReport::blocked`], and the caller decides the exit code from it.
pub fn reclaim_stage(
    stage: &Path,
    destinations: &[NamedStore],
    apply: bool,
    mirror: Option<&MirrorHoldback>,
) -> anyhow::Result<ReclaimReport> {
    let mut report = ReclaimReport {
        destinations: destinations.iter().map(|d| d.name.clone()).collect(),
        ..Default::default()
    };

    // Candidates: every session with a body in the stage. A session directory
    // that holds only `shard-seq` (already reclaimed) has no body and is not a
    // candidate.
    for exp in crate::verify::expected_manifest(stage)? {
        if exp.shard_count == 0 {
            continue;
        }
        report.candidate_bytes = report.candidate_bytes.saturating_add(exp.concat_bytes);
        report.candidates.push(Candidate {
            machine: exp.machine,
            session_id: exp.session_id,
            shard_count: exp.shard_count,
            concat_bytes: exp.concat_bytes,
            sha256: exp.sha256,
        });
    }

    // Sessions with a stored summary but no body were reclaimed by an earlier
    // run; informational, and it must never make a missing baseline look like
    // zero sessions.
    if let Ok(stored) = manifest::stored_manifest_rows(stage) {
        let body_keys: BTreeSet<(String, String)> = report
            .candidates
            .iter()
            .map(|c| (c.machine.clone(), c.session_id.clone()))
            .collect();
        report.already_reclaimed = stored
            .rows
            .iter()
            .filter(|r| !body_keys.contains(&(r.machine.clone(), r.session_id.clone())))
            .count();
    }

    if report.candidates.is_empty() {
        // The no-candidate run is a genuine no-op (nothing at stake, nothing
        // held back), and D7's per-destination rows still record what the
        // run's classification saw — the snapshot below is filled here too,
        // because the phases that follow are the ones with something to do.
        report.destination_states = destinations
            .iter()
            .map(|dest| DestinationProofState {
                name: dest.name.clone(),
                health: dest.health.clone(),
                role: dest.health.proof_role_word(),
            })
            .collect();
        return Ok(report);
    }

    // Defensive: the CLI always hands at least one destination, but an empty
    // set would make the proof vacuous (everything "proven" against nothing).
    if destinations.is_empty() {
        report.blocked.push(Blocked {
            destination: "<none>".to_string(),
            kind: BlockedKind::Unreachable {
                kind: None,
                error: "no destination declared; nothing can be proven against an empty set"
                    .to_string(),
            },
            sessions: report.candidates.len(),
            bytes: report.candidate_bytes,
        });
        return Ok(report);
    }

    // Phase 0 (ADR-060 D1/D2/D3/D7): partition the declared set by the
    // health the caller classified. Degraded destinations (`lagging` /
    // `never-pushed`) are not consulted — D3's own words — but are listed
    // on every run through `destination_states`, so their state is a
    // report row rather than a swallowed row. `unknown` stays in the
    // must-prove set: an unreadable push record is fail-safe, never
    // permission to skip a destination.
    report.destination_states = destinations
        .iter()
        .map(|dest| DestinationProofState {
            name: dest.name.clone(),
            health: dest.health.clone(),
            role: dest.health.proof_role_word(),
        })
        .collect();
    let must_prove: Vec<&NamedStore> = destinations
        .iter()
        .filter(|dest| !dest.health.is_degraded())
        .collect();

    // D7's empty-proof-set refusal, the ≥1 guard's own blocked shape: with
    // no destination able to prove, nothing can be proven at all — never
    // 0-with-success, never silence, and the exit code stays the refusal
    // code for it. The destination rows above are what name the degraded
    // set; `blocked` stays empty so the per-destination arms keep meaning
    // "this destination answered and failed".
    if must_prove.is_empty() {
        report.no_healthy_prover = true;
        return Ok(report);
    }

    // Phase 1 — consult every must-prove destination, keeping each answer's
    // per-session digest map and the machine → newest-snapshot map the
    // mirror leg needs beside it. One unconsultable destination in the
    // must-prove set blocks the whole reclaim: nothing at all can be proven
    // against it (an unconsultable destination is *unproven*, never "does
    // not have it" nor "has it").
    type Consulted<'a> = (
        &'a NamedStore,
        BTreeMap<(String, String), Candidate>,
        BTreeMap<String, String>,
    );
    let mut ready: Vec<Consulted> = Vec::new();
    for dest in must_prove {
        let mk = match store::load_key_file(&dest.cfg) {
            Ok(mk) => mk,
            Err(e) => {
                let error = format!("cannot read masterkey: {e:#}");
                report.blocked.push(Blocked {
                    destination: dest.name.clone(),
                    kind: BlockedKind::Unreachable {
                        kind: crate::remote_err::classify_error_str(&error),
                        error,
                    },
                    sessions: report.candidates.len(),
                    bytes: report.candidate_bytes,
                });
                continue;
            }
        };
        match consult_destination(dest, &mk) {
            DestinationRead::Ready(map, snapshots) => ready.push((dest, map, snapshots)),
            DestinationRead::Unreachable { kind, error } => report.blocked.push(Blocked {
                destination: dest.name.clone(),
                kind: BlockedKind::Unreachable { kind, error },
                sessions: report.candidates.len(),
                bytes: report.candidate_bytes,
            }),
            DestinationRead::PartialRead(detail) => report.blocked.push(Blocked {
                destination: dest.name.clone(),
                kind: BlockedKind::PartialRead { detail },
                sessions: report.candidates.len(),
                bytes: report.candidate_bytes,
            }),
        }
    }
    if report.blocked() {
        return Ok(report);
    }

    // Phase 2 — per-session proof across every must-prove destination. A
    // session that any of them fails to hold is held back (exactly that
    // session and its bytes, in the report), and one held-back session
    // blocks the whole reclaim: nothing is deleted while any healthy
    // destination's answer stands against it — the blocked ⇒ delete-nothing
    // / exit-1 contract, narrowed to the must-prove set, intact for it
    // (ADR-060's Invalidates).
    let mut held: BTreeMap<&str, (usize, u64, BlockedKind)> = BTreeMap::new();
    for cand in &report.candidates {
        let key = (cand.machine.clone(), cand.session_id.clone());
        for (dest, map, _) in &ready {
            match map.get(&key) {
                None => {
                    let entry =
                        held.entry(&dest.name)
                            .or_insert((0, 0, BlockedKind::SessionNotHeld));
                    entry.0 += 1;
                    entry.1 += cand.concat_bytes;
                }
                Some(obs)
                    if obs.shard_count != cand.shard_count
                        || obs.concat_bytes != cand.concat_bytes
                        || obs.sha256 != cand.sha256 =>
                {
                    let entry =
                        held.entry(&dest.name)
                            .or_insert((0, 0, BlockedKind::TripleMismatch));
                    entry.0 += 1;
                    entry.1 += cand.concat_bytes;
                }
                Some(_) => {}
            }
        }
    }
    if !held.is_empty() {
        for (name, (sessions, bytes, kind)) in held {
            report.blocked.push(Blocked {
                destination: name.to_string(),
                kind,
                sessions,
                bytes,
            });
        }
        return Ok(report);
    }

    // Phase 2½ — the mirror leg (ADR-060 D6(ii) / 47 §5.3). Every candidate
    // is proven on every must-prove destination; a machine whose
    // configured-and-on mirror copies a consulted destination may still
    // not delete a body until the copy's durable completed-round record
    // holds that proof's snapshot closure-complete. Binding follows the
    // consult set, not the config alone: the holdback applies only when
    // `mirror_of` was a must-prove destination this run actually consulted
    // — a mirror of a degraded destination binds nothing (D3's rule: a
    // degraded destination may not hold the whole machine's reclaim
    // hostage; its protection is the alarm layer and the union backfill).
    // A held-back session is blocked per session as `not-mirrored`
    // ([`crate::mirror::HELD_BACK_KIND`]), the same word the row and
    // `--json` spell.
    if let Some((holdback, mirroring, snapshots)) = mirror.and_then(|holdback| {
        ready
            .iter()
            .find(|(dest, _, _)| dest.name == holdback.mirror_of)
            .map(|(dest, _, snaps)| (holdback, dest, snaps))
    }) {
        // A record no directory backs vouches for nothing: coverage is a
        // claim about bytes on this machine's disk, so a deleted (or never
        // filled) copy holds everything back, whatever the record says.
        let copy_on_disk = holdback.mirror_root.is_dir();
        let mut held_sessions = 0_usize;
        let mut held_bytes = 0_u64;
        for cand in &report.candidates {
            let covered = copy_on_disk
                && snapshots
                    .get(cand.machine.as_str())
                    .is_some_and(|snapshot_id| {
                        crate::mirrorstate::snapshot_covered(
                            &holdback.state,
                            &holdback.mirror_of,
                            snapshot_id,
                        )
                    });
            if !covered {
                held_sessions += 1;
                held_bytes = held_bytes.saturating_add(cand.concat_bytes);
            }
        }
        if held_sessions > 0 {
            report.blocked.push(Blocked {
                destination: mirroring.name.clone(),
                kind: BlockedKind::NotMirrored,
                sessions: held_sessions,
                bytes: held_bytes,
            });
        }
    }
    if report.blocked() {
        return Ok(report);
    }

    // Phase 3 — every candidate proven on every destination. Apply deletes;
    // dry-run only reports.
    if !apply {
        return Ok(report);
    }

    // The summary must land before the body goes: refresh each machine's
    // manifest from the current body (preserving rows of already-reclaimed
    // sessions), then delete session by session.
    let mut machines: BTreeMap<&str, Vec<&Candidate>> = BTreeMap::new();
    for cand in &report.candidates {
        machines
            .entry(cand.machine.as_str())
            .or_default()
            .push(cand);
    }
    for (machine, cands) in &machines {
        let fresh = manifest::generate_manifest(stage, machine)
            .with_context(|| format!("generate manifest for machine {machine}"))?;
        refresh_manifest(stage, machine, &fresh)?;
        for cand in cands {
            let removed = reclaim_session_body(stage, &cand.machine, &cand.session_id)
                .with_context(|| {
                    format!(
                        "reclaim session `{}` on machine `{}`",
                        cand.session_id, cand.machine
                    )
                })?;
            report.reclaimed.push(Reclaimed {
                machine: cand.machine.clone(),
                session_id: cand.session_id.clone(),
                shard_count: removed,
                bytes: cand.concat_bytes,
            });
        }
    }

    Ok(report)
}

/// How a destination answered the consult. The unreachable arm carries the
/// classifier's verdict ([`crate::remote_err::RemoteErrorKind`]) beside the
/// raw chain, so the report can say which failure it was while the original
/// text stays whole (SG-1 C3).
enum DestinationRead {
    /// The per-session digest map, plus each machine's newest snapshot id —
    /// the snapshot the proof came from, which is what the mirror leg must
    /// find coverage for (ADR-060 D6(ii): the proof's snapshot, named by the
    /// destination's own answer, never by a local cursor).
    Ready(
        BTreeMap<(String, String), Candidate>,
        BTreeMap<String, String>,
    ),
    Unreachable {
        kind: Option<crate::remote_err::RemoteErrorKind>,
        error: String,
    },
    PartialRead(String),
}

/// Ask one destination for the digest triple of every session it holds. The
/// answer comes from the archive itself (`read_latest_per_machine`, newest snapshot
/// per machine), never from a local cursor. A read that did not finish is a
/// `PartialRead` — its absences prove nothing.
///
/// Note on latest snapshot sufficiency (ADR-021 Decision 1 / Task 6):
/// Reclaim candidates are all present in the stage right now (only sessions with
/// un-reclaimed bodies in the stage are candidates). Therefore, if a candidate
/// has been pushed to a destination, it is guaranteed to be present in that
/// destination's newest snapshot. Consulting older snapshots is unnecessary for
/// un-reclaimed stage candidates and would add redundant tree walks.
fn consult_destination(dest: &NamedStore, mk: &MasterKey) -> DestinationRead {
    let store = BackupStore::for_metadata_query(dest.cfg.clone());
    let report = match store.read_latest_per_machine(mk) {
        Ok(r) => r,
        Err(e) => {
            let error = format!("{e:#}");
            return DestinationRead::Unreachable {
                kind: crate::remote_err::classify_error_str(&error),
                error,
            };
        }
    };
    if !report.complete() {
        return DestinationRead::PartialRead(report.warnings.join("; "));
    }
    let mut map = BTreeMap::new();
    let mut snapshots: BTreeMap<String, String> = BTreeMap::new();
    for machine in &report.machines {
        for session in &machine.sessions {
            map.insert(
                (session.machine.clone(), session.session_id.clone()),
                Candidate {
                    machine: session.machine.clone(),
                    session_id: session.session_id.clone(),
                    shard_count: session.shard_count,
                    concat_bytes: session.concat_bytes,
                    sha256: session.sha256.clone(),
                },
            );
            // The proof's snapshot for this machine: the newest snapshot
            // the consult read the session's bytes out of. Keyed by the
            // session's own machine name (the snapshot's hostname equals it
            // for our pushes, but the path is the fact both sides share).
            snapshots.insert(session.machine.clone(), machine.snapshot_id.clone());
        }
    }
    DestinationRead::Ready(map, snapshots)
}

/// Merge freshly derived summaries into one machine's manifest file. Rows for
/// sessions whose body is still present replace (or add) the stored row; rows
/// for already-reclaimed sessions (no body, so `shard_count == 0` in the fresh
/// derivation) are skipped so their stored baseline survives. A fresh row
/// that carries no captured start time never erases the stored row's
/// (ADR-054 D4): a session whose stage-side fact file went missing regenerates
/// from the *stored* fact, and an absent fact stays absent — unknown, never
/// zero, never a file time.
fn refresh_manifest(
    stage: &Path,
    machine: &str,
    fresh: &[manifest::SessionManifest],
) -> anyhow::Result<()> {
    let mut merged: BTreeMap<String, manifest::SessionManifest> = BTreeMap::new();
    if let manifest::ManifestFileState::Loaded(rows) = manifest::read_manifest(stage, machine)? {
        for row in rows {
            merged.insert(row.session_id.clone(), row);
        }
    }
    for row in fresh {
        if row.shard_count > 0 {
            let mut row = row.clone();
            if row.captured_start.is_none() {
                if let Some(stored) = merged.get(&row.session_id) {
                    row.captured_start = stored.captured_start.clone();
                }
            }
            merged.insert(row.session_id.clone(), row);
        }
    }
    let rows: Vec<manifest::SessionManifest> = merged.into_values().collect();
    manifest::write_manifest(stage, machine, &rows)
}

/// Delete one session's sealed shard body, leaving the session directory and
/// its persistent `shard-seq` counter in place so the next sequence number
/// never falls back onto archived shard names. Returns how many shard files
/// were removed. Idempotent: a session whose body is already gone removes
/// nothing.
pub fn reclaim_session_body(
    stage: &Path,
    machine: &str,
    session_id: &str,
) -> anyhow::Result<usize> {
    let dir = store::session_shard_dir(stage, machine, session_id);
    let entries = store::sealed_shard_entries(&dir)?;
    for (_, path) in &entries {
        fs::remove_file(path)
            .with_context(|| format!("remove reclaimed shard {}", path.display()))?;
    }
    // Remove the sealed raw sidecar files too (ADR-054 D5/D9): they are
    // body-tier content — `chat-meta.json`'s `firstPrompt` is a truncated
    // copy of user text — so a reclaimed body must not quietly leave them
    // behind in the stage. The archive already holds every generation, and
    // the metadata this directory must keep (`shard-seq`,
    // `captured-start`) is not prefixed `extra-`.
    let rd = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(entries.len()),
        Err(e) => return Err(e).with_context(|| format!("read session dir {}", dir.display())),
    };
    for entry in rd {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            match fs::remove_dir(entry.path()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    // Not empty (or not ours); harmless — the bucket keeps
                    // whatever non-shard files it has.
                }
            }
        } else if store::is_extra_file(&entry.file_name().to_string_lossy()) {
            fs::remove_file(entry.path())
                .with_context(|| format!("remove reclaimed sidecar {}", entry.path().display()))?;
        } else if entry.file_name().to_string_lossy() == crate::shard_facts::FILE_NAME {
            // The per-shard digest record describes the body being deleted;
            // with the body gone it is neither metadata nor retained summary
            // (that is the manifest's job), so it leaves with the shards.
            fs::remove_file(entry.path())
                .with_context(|| format!("remove reclaimed sidecar {}", entry.path().display()))?;
        }
    }
    Ok(entries.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn session_dir(stage: &Path, machine: &str, session: &str) -> PathBuf {
        store::session_shard_dir(stage, machine, session)
    }

    fn shard_count(stage: &Path, machine: &str, session: &str) -> usize {
        store::sealed_shard_entries(&session_dir(stage, machine, session))
            .unwrap()
            .len()
    }

    fn write_session(stage: &Path, machine: &str, session: &str, nshards: u64) {
        for seq in 1..=nshards {
            let lines: Vec<String> = (0..4)
                .map(|i| format!("{{\"seq\":{seq},\"i\":{i}}}"))
                .collect();
            store::write_sealed_shard(store::StageWriter::Collect, stage, machine, session, &lines)
                .unwrap();
        }
    }

    /// Reclaim keeps the session directory and its `shard-seq` counter.
    #[test]
    fn reclaim_session_body_removes_shards_but_keeps_counter() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path();
        let machine = "m-unit";
        let session = "s-unit";
        write_session(stage, machine, session, 3);
        assert_eq!(shard_count(stage, machine, session), 3);
        let seq_file = store::shard_seq_file(stage, machine, session);
        assert!(seq_file.is_file(), "counter must exist before the reclaim");

        let removed = reclaim_session_body(stage, machine, session).unwrap();
        assert_eq!(removed, 3);
        assert_eq!(shard_count(stage, machine, session), 0, "body gone");
        assert!(seq_file.is_file(), "counter must survive the reclaim");
        assert!(
            session_dir(stage, machine, session).is_dir(),
            "session dir must stay (it holds the counter)"
        );
        assert!(
            !session_dir(stage, machine, session)
                .join(crate::shard_facts::FILE_NAME)
                .is_file(),
            "the shard-facts digest records must leave with the body they describe"
        );
        drop(dir);
    }

    /// Reclaiming an already-reclaimed session is a no-op (idempotent re-run).
    #[test]
    fn reclaim_session_body_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path();
        let machine = "m-unit";
        let session = "s-unit";
        write_session(stage, machine, session, 2);
        assert_eq!(reclaim_session_body(stage, machine, session).unwrap(), 2);
        assert_eq!(reclaim_session_body(stage, machine, session).unwrap(), 0);
        drop(dir);
    }

    /// A session with only a `shard-seq` counter (already reclaimed) is not a
    /// candidate: it has no body to reclaim.
    #[test]
    fn reclaim_stage_skips_sessions_without_a_body() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path();
        let machine = "m-unit";
        let session = "s-unit";
        write_session(stage, machine, session, 1);
        // Drop the body directly; the counter remains.
        reclaim_session_body(stage, machine, session).unwrap();
        let report = reclaim_stage(stage, &[], false, None).unwrap();
        assert!(report.candidates.is_empty());
        drop(dir);
    }

    /// `refresh_manifest` must not clobber an already-reclaimed session's stored
    /// summary with the zero triple its absent body would derive.
    #[test]
    fn refresh_manifest_preserves_reclaimed_rows() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path();
        let machine = "m-unit";

        // Session A: still has a body (1 shard).
        write_session(stage, machine, "s-a", 1);
        // Session B: was already reclaimed; store its original summary.
        write_session(stage, machine, "s-b", 2);
        reclaim_session_body(stage, machine, "s-b").unwrap();
        let stored = manifest::generate_manifest(stage, machine).unwrap();
        assert_eq!(stored.len(), 1, "only s-a still has a body");
        let original_b = manifest::SessionManifest {
            session_id: "s-b".into(),
            machine: machine.into(),
            shard_count: 2,
            concat_bytes: 0,
            concat_sha256: "b".repeat(64),
            sealed_at_unix: 1,
            captured_start: None,
        };
        manifest::write_manifest(stage, machine, &[original_b.clone()]).unwrap();

        // Refreshing from the current body must keep s-b's stored row.
        let fresh = manifest::generate_manifest(stage, machine).unwrap();
        refresh_manifest(stage, machine, &fresh).unwrap();
        let rows = match manifest::read_manifest(stage, machine).unwrap() {
            manifest::ManifestFileState::Loaded(rows) => rows,
            other => panic!("expected Loaded, got {other:?}"),
        };
        let mut by_id: BTreeMap<String, manifest::SessionManifest> = rows
            .into_iter()
            .map(|r| (r.session_id.clone(), r))
            .collect();
        let b = by_id.remove("s-b").expect("s-b row must survive refresh");
        assert_eq!(
            b.shard_count, 2,
            "reclaimed session keeps its stored shard count"
        );
        assert_eq!(b.concat_sha256, "b".repeat(64));
        let a = by_id.remove("s-a").expect("s-a row must be derived fresh");
        assert_eq!(a.shard_count, 1);
        drop(dir);
    }

    #[test]
    fn refresh_manifest_carries_a_stored_captured_start_into_a_fresh_row_without_one() {
        // ADR-054 D4: the capture-time start is written beside the sealed
        // shards, so a regeneration carries it into the fresh row. When the
        // stage-side record is missing (deleted by its owner), the *stored*
        // row's value survives the refresh — a fresh row with no field
        // never erases it, and absent still means unknown.
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path();
        let machine = "m-unit";
        write_session(stage, machine, "s-a", 1);
        let stored_start = manifest::CapturedStart {
            time: "2026-10-05T18:07:03.513Z".into(),
            source: crate::codebuff::START_SOURCE_CHAT_DIRECTORY_NAME.into(),
        };
        let original = manifest::SessionManifest {
            session_id: "s-a".into(),
            machine: machine.into(),
            shard_count: 1,
            concat_bytes: 10,
            concat_sha256: "a".repeat(64),
            sealed_at_unix: 1,
            captured_start: Some(stored_start.clone()),
        };
        manifest::write_manifest(stage, machine, &[original]).unwrap();

        // A fresh derivation whose session directory lost the stage-side
        // record (so the fresh row carries no field).
        let fresh = vec![manifest::SessionManifest {
            session_id: "s-a".into(),
            machine: machine.into(),
            shard_count: 1,
            concat_bytes: 10,
            concat_sha256: "a".repeat(64),
            sealed_at_unix: 2,
            captured_start: None,
        }];
        refresh_manifest(stage, machine, &fresh).unwrap();
        let rows = match manifest::read_manifest(stage, machine).unwrap() {
            manifest::ManifestFileState::Loaded(rows) => rows,
            other => panic!("expected Loaded, got {other:?}"),
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].captured_start,
            Some(stored_start),
            "the stored fact survives a regeneration that could not re-read it"
        );
        assert_eq!(rows[0].sealed_at_unix, 2, "bookkeeping still refreshes");

        // And a fresh row that *does* carry a field is the one that wins:
        // the stage-side record is the fact's source, not the stored row.
        let fresh_start = manifest::CapturedStart {
            time: "2026-10-05T19:00:00.001Z".into(),
            source: crate::codebuff::START_SOURCE_CHAT_DIRECTORY_NAME.into(),
        };
        let fresh = vec![manifest::SessionManifest {
            session_id: "s-a".into(),
            machine: machine.into(),
            shard_count: 1,
            concat_bytes: 10,
            concat_sha256: "a".repeat(64),
            sealed_at_unix: 3,
            captured_start: Some(fresh_start.clone()),
        }];
        refresh_manifest(stage, machine, &fresh).unwrap();
        let rows = match manifest::read_manifest(stage, machine).unwrap() {
            manifest::ManifestFileState::Loaded(rows) => rows,
            other => panic!("expected Loaded, got {other:?}"),
        };
        assert_eq!(rows[0].captured_start, Some(fresh_start));
        drop(dir);
    }
}
