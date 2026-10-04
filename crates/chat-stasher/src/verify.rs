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
use rustic_core::{CheckOptions, Credentials, Repository};
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
    /// If the expectation baseline could not be verified (e.g. summary missing or corrupt).
    pub unverifiable_reason: Option<String>,
}

/// Verdict for one session after reconciling the archive against the expected
/// manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutcome {
    /// Archive observation equals the expected manifest row.
    Match,
    /// Expected by the manifest but absent from the archive.
    MissingInArchive,
    /// Present, but the number of shards differs.
    ShardCountMismatch { expected: usize, observed: usize },
    /// Present with the same shard count, but the concatenated length differs.
    ByteLengthMismatch { expected: u64, observed: u64 },
    /// Present with the same shape, but the concatenated sha256 differs.
    ShaMismatch { expected: String, observed: String },
    /// Baseline could not be verified (e.g. summary is missing or corrupt).
    /// Does not count as passed (ok), does not count as lost (failed).
    Unverifiable { reason: String },
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

/// A session whose archived shard sequence repeats shards byte-for-byte.
///
/// W283. The three-field triple L3 reconciles on is *self-consistent* on a
/// doubled body — a re-seal of the whole source produces a longer
/// concatenation with a matching digest, so nothing above this type can see it.
/// A repeated shard can: the same bytes appear twice in one sequence.
///
/// It is reported as a **possible duplicate seal**, never as a verdict, and the
/// difference is not timidity. Real content repeats — a harness may append
/// bytes identical to bytes it already wrote, and a source staged as `a\n` that
/// grows to `a\na\n` produces exactly the shard sequence a re-seal produces.
/// Nothing in the archive records *why* a shard was written: the sealer stores
/// payload bytes and nothing else, and the per-session summary
/// (`manifest-v1.jsonl`) is derived from the same sealed tree, so it inherits
/// the same ambiguity. Since a false "your archive is corrupt" is as much a
/// lie as a false "OK", this is surfaced loudly and does not fail the run.
///
/// What the type carries is therefore the *shape* and not a cause:
/// [`DuplicateSeal::repeated_body_shards`] says how much of the sequence is
/// repeated from its start, which is what separates the shape a re-seal leaves
/// from a block that merely recurs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateSeal {
    pub machine: String,
    pub session_id: String,
    /// 0-based positions, in global sequence order, of the two shards whose
    /// bytes are identical.
    pub first: usize,
    pub second: usize,
    /// How many shards, counted from the start of the sequence, the shards from
    /// `second` onward repeat byte for byte.
    ///
    /// `Some(1)` is one shard repeated (`A, A`) — what a re-seal of a body
    /// small enough to be sealed in one shard leaves, and 1211 of the 1212 real
    /// cases. `Some(n)` is the same event on a body that took n shards to seal
    /// (`A, B, A, B`), which a test that compares one shard against the bytes
    /// before it cannot see. `None` is a repeat that does *not* begin the
    /// sequence — the weakest evidence of the three, and the one least
    /// distinguishable from content that genuinely repeats.
    pub repeated_body_shards: Option<usize>,
    /// Earlier contiguous shard range whose concatenation equals `second`.
    /// The end is exclusive. This covers collector snapshot re-seals such as
    /// A, B, AB, where no individual shard repeats.
    pub prior_run: Option<(usize, usize)>,
}

/// Find a repeated shard sequence in one archived session, if any.
///
/// The shape a re-seal leaves is looked for first: a run of shards at the start
/// of the sequence that recurs immediately after itself. That is one shard in
/// the common case and several when the body was sealed in several shards, and
/// it is checked with sizes before digests so the common case is a handful of
/// integer comparisons; the digest decides.
///
/// Failing that, the first byte-identical shard pair is reported, wherever in
/// the sequence it falls — a block that recurs without beginning the sequence,
/// which is all that can be said for it.
pub fn duplicate_seal(session: &SessionBackedUp) -> Option<DuplicateSeal> {
    let digests = &session.shard_sha256;
    let sizes = &session.shard_bytes;
    if digests.len() != sizes.len() || digests.len() < 2 {
        return None;
    }
    let seal = |first: usize,
                second: usize,
                repeated_body_shards: Option<usize>,
                prior_run: Option<(usize, usize)>| {
        Some(DuplicateSeal {
            machine: session.machine.clone(),
            session_id: session.session_id.clone(),
            first,
            second,
            repeated_body_shards,
            prior_run,
        })
    };
    for run in 1..=digests.len() / 2 {
        if (0..run).all(|i| sizes[i] == sizes[run + i] && digests[i] == digests[run + i]) {
            return seal(0, run, Some(run), None);
        }
    }
    for j in 1..digests.len() {
        for i in 0..j {
            if sizes[i] == sizes[j] && digests[i] == digests[j] {
                return seal(i, j, None, None);
            }
        }
    }
    if let Some(&(start, shard)) = session.shard_run_duplicates.first() {
        return seal(start, shard, None, Some((start, shard)));
    }
    None
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
    /// Sessions whose shard sequence repeats a shard byte-for-byte. See
    /// [`DuplicateSeal`] for why this is a possibility and not a failure.
    pub possible_duplicate_seals: Vec<DuplicateSeal>,
    pub duration: Duration,
}

impl ReconcileReport {
    /// True if every expected row is a Match.
    pub fn ok(&self) -> bool {
        self.rows.iter().all(|r| r.outcome == SessionOutcome::Match)
    }

    /// Hard failures: missing in archive or content mismatches.
    /// Does NOT include Unverifiable rows (which are neither passed nor lost).
    pub fn failed(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| {
                matches!(
                    r.outcome,
                    SessionOutcome::MissingInArchive
                        | SessionOutcome::ShardCountMismatch { .. }
                        | SessionOutcome::ByteLengthMismatch { .. }
                        | SessionOutcome::ShaMismatch { .. }
                )
            })
            .count()
    }

    /// Number of unverifiable rows.
    pub fn unverifiable(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r.outcome, SessionOutcome::Unverifiable { .. }))
            .count()
    }

    /// Number of matching rows.
    pub fn matched(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.outcome == SessionOutcome::Match)
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
        let repo = Repository::new(&self.cfg.repository_options(), &backends)
            .context("build repository")?
            .open(&Credentials::Masterkey(mk.clone()))
            .context("open repository for verify")?;
        self.require_sound_packs(&repo)?;
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

        let wanted: BTreeSet<(String, String)> = expected
            .iter()
            .filter(|e| e.unverifiable_reason.is_none())
            .map(|e| (e.machine.clone(), e.session_id.clone()))
            .collect();

        let observation = self
            .read_cumulative_sessions_raw(mk, Some(&wanted))
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
            let outcome = if let Some(reason) = &exp.unverifiable_reason {
                SessionOutcome::Unverifiable {
                    reason: reason.clone(),
                }
            } else {
                match obs_map.get(&key) {
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
                }
            };
            rows.push(ReconcileRow {
                machine: exp.machine.clone(),
                session_id: exp.session_id.clone(),
                outcome,
                // reason: when a session is not found in remote archive, observed shards and bytes are 0, and sha is empty string
                observed_shards: obs_map.get(&key).map(|o| o.shard_count).unwrap_or(0),
                // reason: when a session is not found in remote archive, observed shards and bytes are 0, and sha is empty string
                observed_bytes: obs_map.get(&key).map(|o| o.concat_bytes).unwrap_or(0),
                observed_sha: obs_map
                    .get(&key)
                    .map(|o| o.sha256.clone())
                    // reason: when a session is not found in remote archive, observed shards and bytes are 0, and sha is empty string
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

        // Over the sessions this read actually brought back — the expected
        // manifest's, since that is the set L3 asks the archive for. The
        // shard-level evidence is only in hand for those, and they are the ones
        // the staging side claims to know about.
        let possible_duplicate_seals: Vec<DuplicateSeal> =
            obs_map.values().filter_map(duplicate_seal).collect();

        Ok(ReconcileReport {
            machines,
            rows,
            extra_in_archive,
            possible_duplicate_seals,
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
                unverifiable_reason: None,
            });
        }
    }
    out.sort_by(|a, b| (&a.machine, &a.session_id).cmp(&(&b.machine, &b.session_id)));
    Ok(out)
}

/// Resolve the L3 expected manifest, preferring the strongest available
/// basis: when sealed shard bodies are on the staging disk, derive the
/// expectations fresh by hashing those bytes; when bodies are gone (reclaimed),
/// resolve from the persisted per-session summaries. No baseline at all is an error —
/// L3 must never pass vacuously against an empty expectation list.
pub fn load_expected_manifest(stage: &Path) -> anyhow::Result<Vec<SessionExpectation>> {
    let sessions_root = stage.join(SESSIONS_DIR);
    let meta_root = stage.join(crate::manifest::META_DIR);

    if !sessions_root.exists() && !meta_root.exists() {
        anyhow::bail!(
            "L3 baseline: stage body is gone and no stored summary exists under {}; there is nothing to reconcile against",
            meta_root.display()
        );
    }

    let mut map: BTreeMap<(String, String), SessionExpectation> = BTreeMap::new();
    let mut manifest_machines = BTreeSet::new();

    // 1. Process meta_root for stored manifests
    if meta_root.is_dir() {
        if let Ok(entries) = fs::read_dir(&meta_root) {
            for entry in entries.flatten() {
                if let Ok(ft) = entry.file_type() {
                    if ft.is_dir() {
                        let machine = entry.file_name().to_string_lossy().into_owned();
                        manifest_machines.insert(machine.clone());
                        match crate::manifest::read_manifest(stage, &machine) {
                            Ok(crate::manifest::ManifestFileState::Loaded(rows)) => {
                                for row in rows {
                                    let unverifiable_reason = if row.concat_sha256.len() != 64
                                        || row.concat_sha256.chars().any(|c| !c.is_ascii_hexdigit())
                                    {
                                        Some("stored manifest has invalid digest".to_string())
                                    } else {
                                        None
                                    };
                                    map.insert(
                                        (machine.clone(), row.session_id.clone()),
                                        SessionExpectation {
                                            machine: machine.clone(),
                                            session_id: row.session_id,
                                            shard_count: row.shard_count,
                                            concat_bytes: row.concat_bytes,
                                            sha256: row.concat_sha256,
                                            basis: ExpectationBasis::StoredManifest,
                                            unverifiable_reason,
                                        },
                                    );
                                }
                            }
                            Ok(crate::manifest::ManifestFileState::Empty) => {}
                            Ok(crate::manifest::ManifestFileState::Missing) => {}
                            Err(e) => {
                                map.insert(
                                    (machine.clone(), "<corrupt-manifest>".to_string()),
                                    SessionExpectation {
                                        machine: machine.clone(),
                                        session_id: "<corrupt-manifest>".to_string(),
                                        shard_count: 0,
                                        concat_bytes: 0,
                                        sha256: String::new(),
                                        basis: ExpectationBasis::StoredManifest,
                                        unverifiable_reason: Some(format!(
                                            "stored manifest corrupt: {e:#}"
                                        )),
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Process sessions_root: disk bodies take precedence over stored manifests
    if sessions_root.is_dir() {
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
                if shard_count > 0 {
                    let concat = crate::store::concat_shards(stage, &machine, &session_id)?;
                    map.insert(
                        (machine.clone(), session_id.clone()),
                        SessionExpectation {
                            machine: machine.clone(),
                            session_id,
                            shard_count,
                            concat_bytes: concat.len() as u64,
                            sha256: hex_digest(&Sha256::digest(&concat)),
                            basis: ExpectationBasis::DerivedFromStageBody,
                            unverifiable_reason: None,
                        },
                    );
                } else {
                    // Body reclaimed: check stored manifest
                    if let Some(corrupt) =
                        map.remove(&(machine.clone(), "<corrupt-manifest>".to_string()))
                    {
                        map.insert(
                            (machine.clone(), session_id.clone()),
                            SessionExpectation {
                                machine: machine.clone(),
                                session_id,
                                shard_count: 0,
                                concat_bytes: 0,
                                sha256: String::new(),
                                basis: ExpectationBasis::StoredManifest,
                                unverifiable_reason: corrupt.unverifiable_reason,
                            },
                        );
                    } else if !map.contains_key(&(machine.clone(), session_id.clone())) {
                        let reason = if manifest_machines.contains(&machine) {
                            "reclaimed session missing from stored manifest".to_string()
                        } else {
                            "stored manifest is missing".to_string()
                        };
                        map.insert(
                            (machine.clone(), session_id.clone()),
                            SessionExpectation {
                                machine: machine.clone(),
                                session_id,
                                shard_count: 0,
                                concat_bytes: 0,
                                sha256: String::new(),
                                basis: ExpectationBasis::StoredManifest,
                                unverifiable_reason: Some(reason),
                            },
                        );
                    }
                }
            }
        }
    }

    if map.is_empty() {
        anyhow::bail!(
            "L3 baseline: stage body is gone and no stored summary exists under {}; there is nothing to reconcile against",
            meta_root.display()
        );
    }

    let mut out: Vec<SessionExpectation> = map.into_values().collect();
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
            possible_duplicate_seals: Vec::new(),
            duration: Duration::ZERO,
        };
        assert_eq!(report.stored_basis_rows(), 2);
    }

    /// The detector must fire on the sequence a re-seal leaves and stay silent
    /// on a sequence that merely contains a long shard.
    #[test]
    fn duplicate_seal_fires_only_on_repeated_shard_bytes() {
        let session = |digests: &[&str], sizes: &[u64]| SessionBackedUp {
            machine: "m".into(),
            session_id: "s".into(),
            shard_count: digests.len(),
            concat_bytes: sizes.iter().sum(),
            sha256: String::new(),
            shard_sha256: digests.iter().map(|d| d.to_string()).collect(),
            shard_sequences: (1..=digests.len() as u64).map(Some).collect(),
            shard_bytes: sizes.to_vec(),
            shard_run_duplicates: Vec::new(),
        };

        // The W281 repro: one source shard, then the same bytes sealed again.
        let resealed = session(&["aa", "aa"], &[442, 442]);
        let seal = duplicate_seal(&resealed).expect("a re-sealed shard must be reported");
        assert_eq!((seal.first, seal.second), (0, 1));
        assert_eq!(seal.repeated_body_shards, Some(1));

        // Three destinations: still one report, still the whole-body shape.
        let thrice = session(&["aa", "aa", "aa"], &[442, 442, 442]);
        assert_eq!(
            duplicate_seal(&thrice).unwrap().repeated_body_shards,
            Some(1)
        );

        // A body sealed as two shards and then sealed again: `A, B, A, B`. The
        // repeat is the whole body, so it must be named as one — a test that
        // only compares a single shard against the bytes before it calls this
        // a repeated block, which is the shape real repetition leaves.
        let two_shard_body = session(&["aa", "bb", "aa", "bb"], &[442, 442, 442, 442]);
        let seal = duplicate_seal(&two_shard_body).expect("a re-sealed body is a repeat");
        assert_eq!(
            (seal.first, seal.second, seal.repeated_body_shards),
            (0, 2, Some(2)),
            "A, B, A, B is a whole-body re-seal, not a repeated block"
        );

        // A body sealed twice and then grown: only the opening run repeats, and
        // that run is the whole body the re-seal duplicated.
        let grown = session(&["aa", "bb", "aa", "bb", "cc"], &[442, 442, 442, 442, 99]);
        assert_eq!(
            duplicate_seal(&grown).unwrap().repeated_body_shards,
            Some(2)
        );

        // A genuinely appended delta that happens to repeat an earlier block is
        // reported too, and *not* as a whole-body repeat: nothing in the
        // archive can tell the two apart, which is why this is a possibility.
        let real_repeat = session(&["aa", "bb", "bb"], &[10, 10, 10]);
        let seal = duplicate_seal(&real_repeat).expect("a repeated block is still a repeat");
        assert_eq!((seal.first, seal.second), (1, 2));
        assert_eq!(seal.repeated_body_shards, None);

        // Same length, different bytes: silence.
        assert!(duplicate_seal(&session(&["aa", "bb"], &[442, 442])).is_none());
        // One shard, or none: nothing to compare.
        assert!(duplicate_seal(&session(&["aa"], &[442])).is_none());
        assert!(duplicate_seal(&session(&[], &[])).is_none());
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
        // Reclaim the body: the session shard tree disappears entirely.
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
