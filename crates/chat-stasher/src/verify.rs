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

/// Where an L3 expected-manifest row came from.
///
/// The two bases have different evidential strength and must never be
/// conflated. Hashing the shard bytes that are still on the staging disk is a
/// direct check: "the archive still equals the bytes on disk". Reading a
/// summary written earlier is weaker: "the archive still equals the numbers we
/// computed some time ago", which says nothing about anything that changed
/// before the summary was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpectationBasis {
    /// Expected values freshly derived by hashing the sealed shard bytes still
    /// on the staging disk.
    DerivedFromStageBody,
    /// Expected values read back from the persisted per-session summary
    /// (`manifest-v1.jsonl`), written while the body still existed.
    StoredManifest,
}

impl ExpectationBasis {
    /// Short machine-readable label for this basis.
    pub fn label(&self) -> &'static str {
        match self {
            Self::DerivedFromStageBody => "derived-from-stage",
            Self::StoredManifest => "stored-manifest",
        }
    }

    /// One-sentence description of what this basis actually proves, for
    /// user-facing output.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::DerivedFromStageBody => {
                "expected values derived by hashing sealed shard bytes on disk"
            }
            Self::StoredManifest => {
                "expected values read from the persisted summary written earlier; this compares archived data against our own past numbers, not against current disk bytes"
            }
        }
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
    /// Which authority this row's expected values came from.
    pub basis: ExpectationBasis,
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
    /// Which basis this row's expected values were compared against. A
    /// consumer printing rows must surface this: `StoredManifest` is weaker
    /// evidence than `DerivedFromStageBody` and must not be presented as the
    /// same check.
    pub basis: ExpectationBasis,
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

    /// How many rows were reconciled against the persisted summary rather than
    /// fresh stage bytes. Zero in the normal body-present case. A caller that
    /// prints L3 rows must surface this count (or the per-row basis): when it
    /// is non-zero the run is comparing against our own past numbers, not
    /// against the bytes on disk.
    pub fn stored_basis_rows(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.basis == ExpectationBasis::StoredManifest)
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
        let expected = load_expected_manifest(stage)?;
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
                // reason: 远端归档中未找到对应会话时，观测到的分片数与字节数即为 0，sha 为空字符串
                observed_shards: obs_map.get(&key).map(|o| o.shard_count).unwrap_or(0),
                // reason: 远端归档中未找到对应会话时，观测到的分片数与字节数即为 0，sha 为空字符串
                observed_bytes: obs_map.get(&key).map(|o| o.concat_bytes).unwrap_or(0),
                observed_sha: obs_map
                    .get(&key)
                    .map(|o| o.sha256.clone())
                    // reason: 远端归档中未找到对应会话时，观测到的分片数与字节数即为 0，sha 为空字符串
                    .unwrap_or_default(),
                basis: exp.basis.clone(),
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
                basis: ExpectationBasis::DerivedFromStageBody,
            });
        }
    }
    out.sort_by(|a, b| (&a.machine, &a.session_id).cmp(&(&b.machine, &b.session_id)));
    Ok(out)
}

/// Resolve the L3 expected manifest, preferring the strongest available
/// basis: when the sealed shard body is still on the staging disk, derive the
/// expectations fresh by hashing those bytes; when the body is gone, fall back
/// to the persisted per-session summaries. No baseline at all is an error —
/// L3 must never pass vacuously against an empty expectation list.
pub fn load_expected_manifest(stage: &Path) -> anyhow::Result<Vec<SessionExpectation>> {
    if stage_body_present(stage)? {
        expected_manifest(stage)
    } else {
        stored_manifest_expectations(stage)
    }
}

/// Whether the staging tree still holds any sealed shard (the "body"). Once
/// the archived body is reaped this is false, and L3 must fall back to the
/// persisted summary.
fn stage_body_present(stage: &Path) -> anyhow::Result<bool> {
    Ok(crate::store::sealed_shard_count(stage)? > 0)
}

/// Build expectations from the persisted summaries. Zero manifest files means
/// no baseline exists — an error, never an empty expectation list.
fn stored_manifest_expectations(stage: &Path) -> anyhow::Result<Vec<SessionExpectation>> {
    let stored = crate::manifest::stored_manifest_rows(stage)?;
    if stored.files == 0 {
        anyhow::bail!(
            "L3 baseline: stage body is gone and no stored summary exists under {}; there is nothing to reconcile against",
            stage.join(crate::manifest::META_DIR).display()
        );
    }
    let mut out: Vec<SessionExpectation> = stored
        .rows
        .iter()
        .map(|row| SessionExpectation {
            machine: row.machine.clone(),
            session_id: row.session_id.clone(),
            shard_count: row.shard_count,
            concat_bytes: row.concat_bytes,
            sha256: row.concat_sha256.clone(),
            basis: ExpectationBasis::StoredManifest,
        })
        .collect();
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
        assert_eq!(row.basis, ExpectationBasis::DerivedFromStageBody);
        drop(dir);
    }

    #[test]
    fn expected_manifest_is_empty_without_stage() {
        let dir = TempDir::new().unwrap();
        assert!(expected_manifest(dir.path()).unwrap().is_empty());
        drop(dir);
    }

    #[test]
    fn expectation_basis_labels_are_distinct_and_descriptions_distinguish() {
        let derived = ExpectationBasis::DerivedFromStageBody;
        let stored = ExpectationBasis::StoredManifest;
        assert_ne!(derived.label(), stored.label());
        assert!(derived.describe().contains("bytes on disk"));
        assert!(stored.describe().contains("past numbers"));
        // The stored-manifest description must not sound like the same direct
        // byte check — that conflation is exactly what the basis exists to stop.
        assert!(!stored.describe().contains("bytes on disk"));
    }

    #[test]
    fn reconcile_report_stored_basis_rows_counts_only_stored_rows() {
        let row = |basis: ExpectationBasis| ReconcileRow {
            machine: "m".into(),
            session_id: "s".into(),
            outcome: SessionOutcome::Match,
            observed_shards: 0,
            observed_bytes: 0,
            observed_sha: String::new(),
            basis,
        };
        let report = ReconcileReport {
            machines: 1,
            rows: vec![
                row(ExpectationBasis::DerivedFromStageBody),
                row(ExpectationBasis::StoredManifest),
                row(ExpectationBasis::StoredManifest),
            ],
            extra_in_archive: Vec::new(),
            duration: Duration::ZERO,
        };
        assert_eq!(report.stored_basis_rows(), 2);
    }

    #[test]
    fn load_expected_manifest_derives_from_body_when_present() {
        let dir = TempDir::new().unwrap();
        let stage = dir.path();
        let machine = "m-l3";
        let session = "s-1";
        let lines = vec![r#"{"x":1}"#.to_string()];
        crate::store::write_sealed_shard(
            crate::store::StageWriter::Collect,
            stage,
            machine,
            session,
            &lines,
        )
        .unwrap();
        let rows = load_expected_manifest(stage).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].basis, ExpectationBasis::DerivedFromStageBody);
        drop(dir);
    }

    #[test]
    fn load_expected_manifest_falls_back_to_stored_when_body_gone() {
        let dir = TempDir::new().unwrap();
        let stage = dir.path();
        let machine = "m-l3";
        let session = "s-1";
        let lines = vec![r#"{"x":1}"#.to_string()];
        crate::store::write_sealed_shard(
            crate::store::StageWriter::Collect,
            stage,
            machine,
            session,
            &lines,
        )
        .unwrap();
        let rows = crate::manifest::generate_manifest_at(stage, machine, 1_736_944_496).unwrap();
        crate::manifest::write_manifest(stage, machine, &rows).unwrap();
        // Reap the body: the session shard tree disappears entirely.
        fs::remove_dir_all(stage.join(crate::store::SESSIONS_DIR)).unwrap();

        let expectations = load_expected_manifest(stage).unwrap();
        assert_eq!(expectations.len(), 1);
        assert_eq!(expectations[0].basis, ExpectationBasis::StoredManifest);
        assert_eq!(expectations[0].session_id, session);
        assert_eq!(expectations[0].shard_count, 1);
        drop(dir);
    }

    #[test]
    fn load_expected_manifest_errors_when_body_gone_and_no_baseline() {
        let dir = TempDir::new().unwrap();
        let stage = dir.path();
        // Empty stage: no body, no stored summary. This must be an error, not
        // a vacuous pass against zero expected rows.
        let err = load_expected_manifest(stage).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("baseline") || msg.contains("nothing to reconcile"),
            "error should explain the missing baseline: {msg}"
        );
        drop(dir);
    }
}
