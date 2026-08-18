//! ADR-013: initialising a *new* destination.
//!
//! # "Add a destination" means "keep one more copy", not "start a new archive"
//!
//! A fresh destination must end up holding the **union** of
//! `local sources ∪ every existing destination`. The order that union is
//! assembled in is fixed, and it is not arbitrary:
//!
//! 1. **Re-collect from the local sources first.** The local harness files are
//!    the truth, re-reading them puts no load on an existing destination, and
//!    a reread that produces the same bytes uploads nothing new.
//! 2. **Then copy only the difference** — what an existing destination holds
//!    and the local source no longer does.
//!
//! Step 2 exists because the local source is *not* a permanent superset. On
//! this machine `doctor` measured gemini's `sessionRetention` unset (30-day
//! default) with the oldest session already ~175 days past that threshold: for
//! those sessions the archive is the only remaining copy, so skipping step 2
//! would silently found the new destination on a truncated history.
//!
//! # How "the local source no longer has it" is decided
//!
//! Not by a second guess at the harness directories — by **what step 1
//! actually produced**. After the local re-collect, a session that has no
//! sealed shard on the stage is one the local sources did not hand over on
//! this pass, whatever the reason (deleted, unreadable, unrecognised format,
//! a harness we do not cover). Those are the sessions copied back.
//!
//! Which way the misjudgement leans is the point:
//!
//! * A session the local source *did* still have but that failed to collect
//!   has no shards on the stage, so it is copied from the existing
//!   destination — a redundant copy of bytes we already had. Cost: some
//!   traffic. rustic dedups identical content, so the archive does not grow.
//! * A session that is genuinely gone locally is never mistaken for present,
//!   because presence is proven by a shard that exists, not by an absence of
//!   evidence.
//!
//! So every uncertainty resolves to **copy more**, never to **copy less** —
//! which is the trade ADR-013 asks for.
//!
//! # An existing destination that cannot be consulted is not an empty one
//!
//! If a source destination cannot be opened, read or decrypted, its
//! contribution to the difference set is *unknown*. Unknown is never folded
//! into "it had nothing extra": the run reports the difference set as
//! **incomplete** and the caller exits non-zero. The local re-collect is still
//! pushed — refusing to save what we do know would trade one gap for another.
//!
//! # ADR-015: but "unknown" is not the only kind of absence, either
//!
//! Treating *every* absence as unknown has its own cost. Declare four
//! destinations and the first three runs each report INCOMPLETE, because the
//! other three do not exist yet — a warning on the completely normal path,
//! which is how users are taught to ignore warnings.
//!
//! Skipping the ones that are not there is worse, though, because **"no
//! repository at that location" has two opposite causes**: it was never built,
//! or it was built and is now gone. Gone may mean the only surviving copy of
//! some sessions just disappeared. The filesystem cannot tell these apart —
//! both look like an empty path.
//!
//! Our own records can. `state/debts-v2.json` keys its debt sets by
//! [`crate::collect::destination_id`], so "have we ever collected for this
//! destination" is a local fact needing no network. That splits the absence in
//! two, and gives three states in total:
//!
//! | state | evidence | effect |
//! |---|---|---|
//! | [`SourceStatus::KnownEmpty`] | no record, and definitively nothing there | not part of the union; the run may still COMPLETE |
//! | [`SourceStatus::SuspectedLoss`] | we have a record, and it cannot be read | reported loudly as possible data loss; non-zero exit |
//! | [`SourceStatus::Unknown`] | no record, and we cannot tell what is there | INCOMPLETE, as before; non-zero exit |
//!
//! The two failing states share an exit code and share nothing else: one says
//! *go find your archive*, the other says *try again when the network is up*.

use crate::readback::ReadAllReport;
use crate::store::{self, BackupStore, StoreConfig};
use anyhow::Context;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A destination the difference set is computed *against* (an already existing
/// copy), named as the user names it plus the resolved store config.
pub struct SourceDestination {
    pub name: String,
    pub cfg: StoreConfig,
    /// Have we ever committed a read *for this destination* on this machine?
    ///
    /// ADR-015. "There is no repository at that location" has two opposite
    /// causes and the filesystem cannot tell them apart: never built, or built
    /// and since lost. Our own `state/debts-v2.json` can — it is keyed by
    /// [`crate::collect::destination_id`], so a key that is present is proof we
    /// once staged reads for this destination. Supplied by the caller precisely
    /// so this module never guesses from the location itself.
    ///
    /// B82: three values, not two. See [`CollectRecord::Unreadable`].
    pub record: CollectRecord,
}

/// What this machine's own collector state says about a destination.
///
/// B82. This used to be a `bool`, and an unreadable state file arrived here as
/// `false` — which this module then used as *evidence* that the destination
/// had never been built. That is ignorance presented as proof, and it is the
/// one place in this file where a wrong answer changes a decision rather than
/// a sentence: `KnownEmpty` is the single absence that exits 0 and lets the
/// run be declared COMPLETE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CollectRecord {
    /// No record of ever collecting for this destination, and we could read
    /// the state to establish that.
    #[default]
    Absent,
    /// We have collected for this destination before.
    Present,
    /// The record could not be read. We know nothing about this destination,
    /// which is not the same as knowing we never built it.
    Unreadable,
}

/// How a run was able (or unable) to account for one existing destination.
///
/// Three states, not two: an absence we can *explain* and an absence we cannot
/// must never print the same word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceStatus {
    /// The archive was opened and read. Its contribution is exact.
    #[default]
    Consulted,
    /// Nothing at the location, and we have no record of ever having dealt
    /// with it ⇒ it was never built. Contributes nothing, and that nothing is
    /// a *fact*, so it does not hold the union back.
    KnownEmpty,
    /// We have a record of dealing with it, and now it cannot be read. This is
    /// not "one fewer empty destination" — it is a copy that may be gone.
    SuspectedLoss,
    /// No record, and we cannot even establish whether a repository is there
    /// (backend down, location unreadable). Unknown is not empty.
    Unknown,
}

/// Why an archive could not be read.
enum Absence {
    /// The location answered, and there is definitively no repository in it.
    NoRepository,
    /// We could not establish what is there at all.
    Indeterminate,
}

/// What one existing destination contributed — or failed to contribute.
#[derive(Debug, Clone, Default)]
pub struct SourceOutcome {
    pub name: String,
    /// sha256 of the repository location. The location itself is never printed.
    pub destination_id: String,
    /// Which of the three states this destination resolved to.
    pub status: SourceStatus,
    /// Could this destination's archive be consulted at all?
    pub reachable: bool,
    /// Why not, when it could not. Metadata only.
    pub unreachable_reason: Option<String>,
    /// B82: this destination's `Unknown` comes from *our own* unreadable
    /// collector record, not from the location being unreachable. Different
    /// thing to go and do about it, so it is reported separately.
    pub record_unreadable: bool,
    /// Sessions found in this destination's newest snapshot for our machine.
    pub sessions_for_this_machine: usize,
    /// Sessions found under *another* machine partition. These are real,
    /// known content that this command deliberately does not copy: the stage
    /// is validated against a single machine partition before push
    /// (`store::validate_stage_machines`), so re-pushing another machine's
    /// sessions under our snapshot host would misattribute them. Reported, and
    /// never silently dropped.
    pub sessions_other_machines: usize,
    /// Sessions this destination has that the local re-collect did not produce.
    pub missing_locally: usize,
    pub restored_sessions: usize,
    pub restored_shards: usize,
    /// Sessions that were selected for copying but could not be reproduced
    /// (not returned by the archive, or restored bytes that did not re-hash to
    /// what the archive reported). Session id prefixes only.
    pub failed_sessions: Vec<String>,
}

/// Result of the difference-set step.
#[derive(Debug, Clone, Default)]
pub struct DiffReport {
    pub sources: Vec<SourceOutcome>,
    /// False when *any* source could not be fully accounted for. The caller
    /// must surface this and exit non-zero: an incomplete difference set means
    /// the new destination is not yet proven to hold the union.
    pub diff_complete: bool,
    pub restored_sessions: usize,
    pub restored_shards: usize,
}

impl DiffReport {
    /// Names of destinations we have a record for and can no longer read.
    pub fn suspected_loss(&self) -> Vec<&str> {
        self.sources
            .iter()
            .filter(|s| s.status == SourceStatus::SuspectedLoss)
            .map(|s| s.name.as_str())
            .collect()
    }

    /// Names of destinations whose existence we could not even establish.
    pub fn unknown(&self) -> Vec<&str> {
        self.sources
            .iter()
            .filter(|s| s.status == SourceStatus::Unknown)
            .map(|s| s.name.as_str())
            .collect()
    }

    /// Names of destinations that were never built and are therefore empty.
    pub fn known_empty(&self) -> Vec<&str> {
        self.sources
            .iter()
            .filter(|s| s.status == SourceStatus::KnownEmpty)
            .map(|s| s.name.as_str())
            .collect()
    }
}

/// Ask one destination's archive what it holds, for our machine partition.
///
/// Every failure path — no repository yet, no key, unreachable backend — is an
/// `Err`, never an empty report.
pub fn probe_archive(cfg: &StoreConfig, machine: &str) -> anyhow::Result<ReadAllReport> {
    probe_classified(cfg, machine).map_err(|(_, reason)| anyhow::anyhow!("{reason}"))
}

/// The plain filesystem path a repository location denotes — when it denotes
/// one at all.
///
/// This mirrors `rustic_backend`'s own location parsing, and it has to: rustic
/// accepts the *same* local directory under two spellings, a bare path and the
/// explicit `local:<path>` form. B46 measured the cost of only recognising the
/// first one — the identical unreadable directory resolved to `Unknown` spelled
/// bare and to `KnownEmpty` spelled with the prefix, and `KnownEmpty` is the one
/// absence that exits 0 and lets the run carry on. How a user spelled a location
/// must not decide whether an unreadable copy is reported.
///
/// `None` means "a real backend": its errors are its own to report, and the
/// local second-guess below would be meaningless.
fn local_repo_path(repo_root: &str) -> Option<&str> {
    match repo_root.split_once(':') {
        // No scheme at all — a plain path.
        None => Some(repo_root),
        // The explicit spelling of a plain path.
        Some(("local", path)) => Some(path),
        // Windows: a drive letter, or any location with a backslash in it, is
        // local for rustic too. Harmless elsewhere — such a path simply does
        // not exist, which the `NotFound` arm already reads as "nothing there".
        Some((scheme, _)) if scheme.len() == 1 => Some(repo_root),
        Some((scheme, path)) if scheme.contains('\\') || path.contains('\\') => Some(repo_root),
        Some(_) => None,
    }
}

/// "The backend listed no config file" is not by itself proof that no
/// repository is there.
///
/// Measured on this machine: with a *local* repository directory chmod'd to
/// `000`, `backends().repository().list(FileType::Config)` returns
/// `Ok(<empty>)` rather than an error — so a directory we simply cannot read
/// looks exactly like a directory that was never created. For a local path we
/// can settle it ourselves: a path that is not there is genuinely nothing, and
/// a path that is there but will not open is *unknown*.
///
/// A remote backend has to answer this for itself, and B46 measured that the
/// ones reachable from this crate do: `opendal:sftp` (connection refused, wrong
/// key, unreadable remote directory), `opendal:s3` (refused, 403) and
/// `opendal:http` (401) all return `Err`, never `Ok(<empty>)`. Only a genuine
/// "not found" comes back as `Ok(<empty>)`, which is exactly the case this
/// function is allowed to call nothing. That measurement is pinned by
/// `an_unreachable_remote_destination_is_unknown_not_empty`, because a backend
/// that changed its mind about it would land in `KnownEmpty` — the one branch
/// that exits 0 — with no other symptom.
fn no_repository_or_unreadable(cfg: &StoreConfig) -> (Absence, String) {
    let definitely_nothing = (
        Absence::NoRepository,
        "destination repository is not initialised".to_string(),
    );
    // Backend strings are the backend's business; only a plain path can be
    // second-guessed here.
    let Some(local) = local_repo_path(&cfg.repo_root) else {
        return definitely_nothing;
    };
    let root = Path::new(local);
    match std::fs::read_dir(root) {
        Ok(_) => definitely_nothing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => definitely_nothing,
        Err(e) => (
            Absence::Indeterminate,
            format!(
                "the destination location exists but cannot be read, so whether a repository \
                 is in it is unknown: {e}"
            ),
        ),
    }
}

/// Same probe, but keeping *why* it failed: "the location answered and there is
/// no repository" and "we could not find out" are the two halves of the
/// tri-state and collapsing them into one `Err` is what ADR-015 forbids.
fn probe_classified(cfg: &StoreConfig, machine: &str) -> Result<ReadAllReport, (Absence, String)> {
    let store = BackupStore::new(cfg.clone(), machine.to_string());
    match store.repository_exists() {
        Ok(true) => {}
        Ok(false) => return Err(no_repository_or_unreadable(cfg)),
        Err(e) => {
            return Err((
                Absence::Indeterminate,
                format!("cannot establish whether a repository is there: {e:#}"),
            ));
        }
    }
    let mk = store::load_key_file(cfg)
        .map_err(|e| (Absence::Indeterminate, format!("key unavailable: {e:#}")))?;
    store
        .read_all_machines(&mk)
        .map_err(|e| (Absence::Indeterminate, format!("archive unreadable: {e:#}")))
}

/// Does the stage already hold at least one sealed shard for this session?
fn stage_has(stage: &Path, machine: &str, session_id: &str) -> bool {
    let dir = store::session_shard_dir(stage, machine, session_id);
    store::sealed_shard_entries(&dir)
        .map(|entries| !entries.is_empty())
        .unwrap_or(false)
}

/// A session id coming out of an archive path is untrusted input: it becomes a
/// directory name on the stage.
fn safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains('\0')
}

fn id_prefix(id: &str) -> String {
    crate::id::short_session_id(id)
}

/// Copy back what the existing destinations have and the local re-collect did
/// not produce. Runs *after* the local pass has staged everything it can.
pub fn fill_difference(
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
    sources: &[SourceDestination],
) -> DiffReport {
    let mut report = DiffReport {
        diff_complete: true,
        ..DiffReport::default()
    };
    for source in sources {
        let mut outcome = SourceOutcome {
            name: source.name.clone(),
            destination_id: crate::collect::destination_id(&source.cfg.repo_root),
            ..SourceOutcome::default()
        };
        match probe_classified(&source.cfg, machine) {
            Ok(observation) => {
                outcome.reachable = true;
                outcome.status = SourceStatus::Consulted;
                let mut wanted: BTreeSet<String> = BTreeSet::new();
                let mut expected: BTreeMap<String, String> = BTreeMap::new();
                for merge in &observation.machines {
                    for session in &merge.sessions {
                        if session.machine != machine {
                            outcome.sessions_other_machines += 1;
                            continue;
                        }
                        outcome.sessions_for_this_machine += 1;
                        if !safe_session_id(&session.session_id) {
                            outcome.failed_sessions.push("<unsafe-id>".to_string());
                            continue;
                        }
                        if stage_has(stage, machine, &session.session_id) {
                            continue;
                        }
                        outcome.missing_locally += 1;
                        wanted.insert(session.session_id.clone());
                        expected.insert(session.session_id.clone(), session.sha256.clone());
                    }
                }
                match restore(&source.cfg, machine, stage, bucket_cap, &wanted, &expected) {
                    Ok(restored) => {
                        for (session, shards) in restored.done {
                            outcome.restored_sessions += 1;
                            outcome.restored_shards += shards;
                            let _ = session;
                        }
                        outcome.failed_sessions.extend(restored.failed);
                    }
                    Err(e) => {
                        outcome
                            .failed_sessions
                            .extend(wanted.iter().map(|id| id_prefix(id)));
                        outcome.unreachable_reason = Some(format!("{e:#}"));
                    }
                }
            }
            Err((absence, reason)) => {
                outcome.reachable = false;
                outcome.unreachable_reason = Some(reason);
                // The whole tri-state turns on this line. An absence at the
                // location is only "it was never built" when *our own record*
                // agrees we never built it. If we have dealt with this
                // destination before, the same absence means the opposite
                // thing, and the opposite thing is a lost copy.
                outcome.status = match (absence, source.record) {
                    (_, CollectRecord::Present) => SourceStatus::SuspectedLoss,
                    (Absence::NoRepository, CollectRecord::Absent) => SourceStatus::KnownEmpty,
                    // B82: an absent repository plus a record we could not
                    // read is `Unknown`, not `KnownEmpty`. Both halves of the
                    // "never built" claim have to be established; here the
                    // second one was not, and reading `false` out of a failed
                    // read is how "I could not confirm" became "confirmed
                    // nothing". Unknown costs exit 3 and a re-run; the old
                    // answer cost a destination that may have been lost being
                    // written off as one that never existed.
                    (Absence::NoRepository, CollectRecord::Unreadable) => SourceStatus::Unknown,
                    (Absence::Indeterminate, _) => SourceStatus::Unknown,
                };
                if source.record == CollectRecord::Unreadable {
                    outcome.record_unreadable = true;
                }
            }
        }
        // Only a *known* empty destination leaves the union provable. Both
        // other absences leave it unproven — for different reasons, reported
        // with different words by the caller.
        if outcome.status == SourceStatus::SuspectedLoss
            || outcome.status == SourceStatus::Unknown
            || !outcome.failed_sessions.is_empty()
        {
            report.diff_complete = false;
        }
        report.restored_sessions += outcome.restored_sessions;
        report.restored_shards += outcome.restored_shards;
        report.sources.push(outcome);
    }
    report
}

struct Restored {
    /// `(session id, shard count)` per successfully reproduced session.
    done: Vec<(String, usize)>,
    /// Session id prefixes that were asked for and not reproduced.
    failed: Vec<String>,
}

/// Pull the wanted sessions' shards back and install them on the stage, then
/// re-hash what landed against what the archive said it holds. A session whose
/// restored concatenation does not match is reported as failed rather than
/// counted as copied.
fn restore(
    cfg: &StoreConfig,
    machine: &str,
    stage: &Path,
    bucket_cap: usize,
    wanted: &BTreeSet<String>,
    expected: &BTreeMap<String, String>,
) -> anyhow::Result<Restored> {
    let mut out = Restored {
        done: Vec::new(),
        failed: Vec::new(),
    };
    if wanted.is_empty() {
        return Ok(out);
    }
    let store = BackupStore::new(cfg.clone(), machine.to_string());
    let mk = store::load_key_file(cfg)?;
    let dumped = store
        .dump_machine_sessions(&mk, machine, wanted)
        .context("dump shards from the existing destination")?;
    for session in wanted {
        let Some(shards) = dumped.get(session) else {
            // Asked for, not returned. That is a failure to copy, not proof
            // that there was nothing to copy.
            out.failed.push(id_prefix(session));
            continue;
        };
        let mut written = 0usize;
        let mut error = None;
        for shard in shards {
            match store::write_sealed_shard_raw_with_cap(
                store::StageWriter::Restore,
                stage,
                machine,
                session,
                shard,
                bucket_cap,
            ) {
                Ok(_) => written += 1,
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        if error.is_some() {
            out.failed.push(id_prefix(session));
            continue;
        }
        let matches = match store::expected_concat_sha(stage, machine, session) {
            Ok(sha) => expected.get(session).is_some_and(|want| *want == sha),
            Err(_) => false,
        };
        if matches {
            out.done.push((session.clone(), written));
        } else {
            out.failed.push(id_prefix(session));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_supplied_session_ids_cannot_escape_the_stage() {
        assert!(safe_session_id("019bf00d-97b6-7eb2-9bf8-eacbacc09765"));
        assert!(!safe_session_id(".."));
        assert!(!safe_session_id("a/../../b"));
        assert!(!safe_session_id(""));
    }

    #[test]
    fn an_unreachable_source_is_not_an_empty_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let sources = vec![SourceDestination {
            name: "gone".to_string(),
            // We have collected for it before — so its absence is loss, not
            // emptiness, and it still holds the union back.
            record: CollectRecord::Present,
            cfg: StoreConfig {
                repo_root: dir
                    .path()
                    .join("no-such-repo")
                    .to_string_lossy()
                    .into_owned(),
                key_file: dir.path().join("no-such-key.json"),
                connections: 1,
                options: BTreeMap::new(),
            },
        }];
        let report = fill_difference(&dir.path().join("stage"), "fixture-machine", 20, &sources);
        assert!(
            !report.diff_complete,
            "an unconsultable destination must make the difference set incomplete"
        );
        assert!(!report.sources[0].reachable);
        assert_eq!(report.sources[0].status, SourceStatus::SuspectedLoss);
        assert_eq!(report.restored_sessions, 0);
    }

    /// The same location, the same absence, and the opposite verdict — the
    /// only thing that changed is whether we have a record of it.
    #[test]
    fn the_same_absence_means_never_built_when_we_have_no_record_of_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let sources = vec![SourceDestination {
            name: "never-made".to_string(),
            record: CollectRecord::Absent,
            cfg: StoreConfig {
                repo_root: dir
                    .path()
                    .join("no-such-repo")
                    .to_string_lossy()
                    .into_owned(),
                key_file: dir.path().join("no-such-key.json"),
                connections: 1,
                options: BTreeMap::new(),
            },
        }];
        let report = fill_difference(&dir.path().join("stage"), "fixture-machine", 20, &sources);
        assert_eq!(report.sources[0].status, SourceStatus::KnownEmpty);
        assert!(
            report.diff_complete,
            "a destination that was never built holds nothing, and that nothing is knowledge — \
             it must not make every earlier destination's run look incomplete"
        );
    }
}
