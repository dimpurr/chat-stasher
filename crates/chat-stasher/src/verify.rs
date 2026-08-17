//! verify — "can you prove the backup is intact" (three independently
//! runnable levels, cheapest first):
//!
//! * **L1 structure** — rustic's `check` *without* `read_data`: repository
//!   metadata, index↔pack presence/size agreement, snapshot tree references.
//!   Only the small tree blobs are decrypted; the shard payloads are never
//!   read, so a bit flip inside a data blob goes unnoticed (measured).
//! * **L2 content** — rustic's `check(read_data = true)`: every pack is
//!   downloaded, decrypted, decompressed and re-hashed. Any bit flip in a
//!   pack shows up here.
//! * **L3 reconcile** — compares what the newest snapshot per machine really
//!   holds (shard count / concatenated bytes / concatenated sha256) against an
//!   *expected manifest* computed from the sealed staging tree.
//!
//! # Where the L3 expectation comes from
//!
//! Deriving the expectation from the archive itself is a tautology — any
//! corruption would be re-derived into the "expected" value and the check
//! would always pass, so it is rejected. The independent authority is the
//! sealed shard tree on the staging side ([`expected_manifest`]); the cost is
//! that staging must still exist to run L3. Push-time durable receipts (an
//! immutable, content-addressed manifest — like rustic's own index, never
//! rewritten in place — written with every push and picked newest on read)
//! would lift that requirement; [`expected_manifest`] is already the exact
//! shape such a receipt would carry.
//!
//! Privacy line: L3 only ever compares ids, counts, byte lengths and sha256
//! digests. Shard payloads are read and hashed in place, never returned.

use anyhow::Context;
use rustic_core::repofile::MasterKey;
use rustic_core::{CheckOptions, Credentials, Repository, RepositoryOptions};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::readback::SessionBackedUp;
use crate::store::{BackupStore, SESSIONS_DIR};

/// Outcome of rustic's `check` at one data level.
#[derive(Debug, Clone)]
pub struct CheckSummary {
    /// `true` if every pack was downloaded and re-verified (L2), `false` if
    /// only the tree blobs were decrypted (L1).
    pub read_data: bool,
    /// Total findings of all severities.
    pub findings: usize,
    /// `CheckErrorLevel::Error` findings (repository is not as it should be).
    pub errors: usize,
    /// `CheckErrorLevel::Warn` findings (strange, but integrity not affected).
    pub warns: usize,
    pub duration: Duration,
    /// One line per finding, ready to print.
    pub details: Vec<String>,
}

impl CheckSummary {
    /// Zero finding = the level passed. Warnings alone still pass.
    pub fn ok(&self) -> bool {
        self.errors == 0
    }
}

/// One row of the L3 expected manifest: what the sealed stage says a session
/// must be, once archived.
#[derive(Debug, Clone)]
pub struct SessionExpectation {
    pub machine: String,
    pub session_id: String,
    pub shard_count: usize,
    pub concat_bytes: u64,
    pub sha256: String,
}

/// Verdict for one session after reconciling the archive against the expected
/// manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutcome {
    /// Archive observation equals the expected manifest row.
    Match,
    /// Expected by the manifest but absent from (the newest snapshot of) the
    /// archive.
    MissingInArchive,
    /// Present, but the number of shards differs.
    ShardCountMismatch { expected: usize, observed: usize },
    /// Present with the same shard count, but the concatenated length differs.
    ByteLengthMismatch { expected: u64, observed: u64 },
    /// Present with the same shape, but the concatenated sha256 differs.
    ShaMismatch { expected: String, observed: String },
}

/// One reconciled row — always a manifest row; archive-only sessions are
/// reported separately and never fail the run.
#[derive(Debug, Clone)]
pub struct ReconcileRow {
    pub machine: String,
    pub session_id: String,
    pub outcome: SessionOutcome,
    /// Shard count actually observed in the archive.
    pub observed_shards: usize,
    /// Concatenated byte length actually read back from the archive.
    pub observed_bytes: u64,
    /// sha256 of the concatenation actually read back from the archive.
    pub observed_sha: String,
}

/// Full L3 result.
#[derive(Debug, Clone)]
pub struct ReconcileReport {
    pub machines: usize,
    /// Sessions the manifest expected, with a verdict each.
    pub rows: Vec<ReconcileRow>,
    /// Sessions found in the archive but not in the manifest (informational —
    /// e.g. staging was cleaned up for that machine, or a push missed staging).
    pub extra_in_archive: Vec<(String, String)>,
    pub duration: Duration,
}

impl ReconcileReport {
    /// A failure is a manifest row that did not come back `Match`.
    pub fn ok(&self) -> bool {
        self.rows.iter().all(|r| r.outcome == SessionOutcome::Match)
    }

    pub fn failed(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.outcome != SessionOutcome::Match)
            .count()
    }
}

impl BackupStore {
    /// Run rustic's repository check at one data level and summarise the
    /// findings. Only `read_data` differs between L1 and L2 — the subset is
    /// left at its default (`All`), so L2 really downloads every pack.
    ///
    /// Note on severity counting: `rustic_core` does not re-export
    /// `CheckErrorLevel` (only `CheckResults`), so severity is recovered from
    /// the level's `Debug` spelling rather than by matching the type.
    pub fn check_repo(&self, mk: &MasterKey, read_data: bool) -> anyhow::Result<CheckSummary> {
        let start = Instant::now();
        let backends = self.backends()?;
        let repo = Repository::new(&RepositoryOptions::default(), &backends)
            .context("build repository")?
            .open(&Credentials::Masterkey(mk.clone()))
            .context("open repository for verify")?;
        let opts = CheckOptions::default().read_data(read_data).clone();
        let results = repo.check(opts).context("run rustic check")?;

        let findings = results.0.len();
        let errors = results
            .0
            .iter()
            .filter(|(lvl, _)| format!("{lvl:?}") == "Error")
            .count();
        let warns = findings - errors;
        let mut details = Vec::with_capacity(findings);
        for (lvl, err) in &results.0 {
            details.push(format!("[{lvl:?}] {err}"));
        }
        Ok(CheckSummary {
            read_data,
            findings,
            errors,
            warns,
            duration: start.elapsed(),
            details,
        })
    }

    /// L3 reconcile: read the archive back (newest snapshot per machine) and
    /// compare it session by session against the expected manifest derived
    /// from the staging tree.
    pub fn reconcile_manifest(
        &self,
        mk: &MasterKey,
        stage: &Path,
    ) -> anyhow::Result<ReconcileReport> {
        let start = Instant::now();
        let expected = expected_manifest(stage)?;
        let observation = self
            .read_all_machines(mk)
            .context("read archive back for reconcile")?;

        let obs_map: BTreeMap<(String, String), SessionBackedUp> = observation
            .machines
            .iter()
            .flat_map(|m| m.sessions.iter().cloned())
            .map(|s| ((s.machine.clone(), s.session_id.clone()), s))
            .collect();

        let mut rows = Vec::with_capacity(expected.len());
        for exp in &expected {
            let key = (exp.machine.clone(), exp.session_id.clone());
            let outcome = match obs_map.get(&key) {
                None => SessionOutcome::MissingInArchive,
                Some(obs) if obs.shard_count != exp.shard_count => {
                    SessionOutcome::ShardCountMismatch {
                        expected: exp.shard_count,
                        observed: obs.shard_count,
                    }
                }
                Some(obs) if obs.concat_bytes != exp.concat_bytes => {
                    SessionOutcome::ByteLengthMismatch {
                        expected: exp.concat_bytes,
                        observed: obs.concat_bytes,
                    }
                }
                Some(obs) if obs.sha256 != exp.sha256 => SessionOutcome::ShaMismatch {
                    expected: exp.sha256.clone(),
                    observed: obs.sha256.clone(),
                },
                Some(_) => SessionOutcome::Match,
            };
            rows.push(ReconcileRow {
                machine: exp.machine.clone(),
                session_id: exp.session_id.clone(),
                outcome,
                observed_shards: obs_map.get(&key).map(|o| o.shard_count).unwrap_or(0),
                observed_bytes: obs_map.get(&key).map(|o| o.concat_bytes).unwrap_or(0),
                observed_sha: obs_map
                    .get(&key)
                    .map(|o| o.sha256.clone())
                    .unwrap_or_default(),
            });
        }

        let expected_keys: BTreeSet<(String, String)> = expected
            .iter()
            .map(|e| (e.machine.clone(), e.session_id.clone()))
            .collect();
        let extra_in_archive: Vec<(String, String)> = obs_map
            .keys()
            .filter(|k| !expected_keys.contains(k))
            .cloned()
            .collect();

        let machines = {
            let mut all: BTreeSet<String> = expected.iter().map(|e| e.machine.clone()).collect();
            all.extend(obs_map.keys().map(|(m, _)| m.clone()));
            all.len()
        };

        Ok(ReconcileReport {
            machines,
            rows,
            extra_in_archive,
            duration: start.elapsed(),
        })
    }
}

/// Build the L3 expected manifest from the sealed staging tree:
/// `sessions/<machine>/<session-id>/<bucket>/{NNNNNN}.jsonl` (plus the legacy
/// unbucketed layout). Each session contributes its shard count, concatenated
/// byte length and concatenated sha256 — the three properties `read
/// --all-machines` later reports for the archive.
pub fn expected_manifest(stage: &Path) -> anyhow::Result<Vec<SessionExpectation>> {
    let sessions_root = stage.join(SESSIONS_DIR);
    let mut out = Vec::new();
    if !sessions_root.is_dir() {
        return Ok(out);
    }
    for m_entry in fs::read_dir(&sessions_root).context("read sessions root")? {
        let m_entry = m_entry?;
        if !m_entry.file_type()?.is_dir() {
            continue;
        }
        let machine = m_entry.file_name().to_string_lossy().into_owned();
        for s_entry in fs::read_dir(m_entry.path()).context("read machine dir")? {
            let s_entry = s_entry?;
            if !s_entry.file_type()?.is_dir() {
                continue;
            }
            let session_id = s_entry.file_name().to_string_lossy().into_owned();
            let shard_count = count_shards(&s_entry.path())?;
            let concat = crate::store::concat_shards(stage, &machine, &session_id)?;
            out.push(SessionExpectation {
                machine: machine.clone(),
                session_id,
                shard_count,
                concat_bytes: concat.len() as u64,
                sha256: hex_digest(&Sha256::digest(&concat)),
            });
        }
    }
    out.sort_by(|a, b| (&a.machine, &a.session_id).cmp(&(&b.machine, &b.session_id)));
    Ok(out)
}

/// Count sealed shards in both the legacy direct layout and one-level buckets.
fn count_shards(session_dir: &Path) -> anyhow::Result<usize> {
    Ok(crate::store::sealed_shard_entries(session_dir)?.len())
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn expected_manifest_reads_sealed_tree() {
        let dir = TempDir::new().unwrap();
        let stage = dir.path();
        let machine = "m-test";
        let session = "s-1";
        let mut expected_bytes = Vec::new();
        for seq in 1..=2u64 {
            let lines: Vec<String> = (0..10)
                .map(|i| format!("{{\"seq\":{seq},\"i\":{i}}}"))
                .collect();
            crate::store::write_sealed_shard(
                crate::store::StageWriter::Collect,
                stage,
                machine,
                session,
                &lines,
            )
            .unwrap();
            for l in &lines {
                expected_bytes.extend_from_slice(l.as_bytes());
                expected_bytes.push(b'\n');
            }
        }
        let manifest = expected_manifest(stage).unwrap();
        assert_eq!(manifest.len(), 1);
        let row = &manifest[0];
        assert_eq!(row.machine, machine);
        assert_eq!(row.session_id, session);
        assert_eq!(row.shard_count, 2);
        assert_eq!(row.concat_bytes, expected_bytes.len() as u64);
        assert_eq!(row.sha256, hex_digest(&Sha256::digest(&expected_bytes)));
        drop(dir);
    }

    #[test]
    fn expected_manifest_is_empty_without_stage() {
        let dir = TempDir::new().unwrap();
        assert!(expected_manifest(dir.path()).unwrap().is_empty());
        drop(dir);
    }
}
