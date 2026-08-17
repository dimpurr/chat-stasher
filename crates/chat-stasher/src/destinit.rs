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
}

/// What one existing destination contributed — or failed to contribute.
#[derive(Debug, Clone, Default)]
pub struct SourceOutcome {
    pub name: String,
    /// sha256 of the repository location. The location itself is never printed.
    pub destination_id: String,
    /// Could this destination's archive be consulted at all?
    pub reachable: bool,
    /// Why not, when it could not. Metadata only.
    pub unreachable_reason: Option<String>,
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

/// Ask one destination's archive what it holds, for our machine partition.
///
/// Every failure path — no repository yet, no key, unreachable backend — is an
/// `Err`, never an empty report.
pub fn probe_archive(cfg: &StoreConfig, machine: &str) -> anyhow::Result<ReadAllReport> {
    let store = BackupStore::new(cfg.clone(), machine.to_string());
    if !store.repository_exists()? {
        anyhow::bail!("destination repository is not initialised");
    }
    let mk = store::load_key_file(cfg)?;
    store.read_all_machines(&mk)
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
    id.chars().take(8).collect()
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
        match probe_archive(&source.cfg, machine) {
            Ok(observation) => {
                outcome.reachable = true;
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
            Err(e) => {
                outcome.reachable = false;
                outcome.unreachable_reason = Some(format!("{e:#}"));
            }
        }
        if !outcome.reachable || !outcome.failed_sessions.is_empty() {
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
        assert_eq!(report.restored_sessions, 0);
    }
}
