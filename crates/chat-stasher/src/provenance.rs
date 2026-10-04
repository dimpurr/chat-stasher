//! Shared, additive session provenance dimensions.
//!
//! The values are facts observed at capture time. Empty dimensions mean no
//! value was recorded; callers must not infer a value from the harness name.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

/// The four orthogonal, optional dimensions used to describe a session's
/// observed origin and lifecycle. Multiple values are retained because one
/// stable session may be resumed through several surfaces.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionProvenance {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub surface: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tenant: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub container: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<String>,
}

impl SessionProvenance {
    /// No dimension values were observed. This does not infer a source state.
    pub fn is_empty(&self) -> bool {
        is_empty(self)
    }

    /// The inbox schema requires non-empty, unique strings in every dimension.
    pub fn is_valid(&self) -> bool {
        [&self.surface, &self.tenant, &self.container, &self.status]
            .into_iter()
            .all(|values| {
                values.iter().all(|value| !value.is_empty())
                    && values.iter().collect::<BTreeSet<_>>().len() == values.len()
            })
    }
}

/// Fold typed dimensions embedded in archived JSONL records. Malformed or
/// unknown-shaped values are ignored so they cannot become trusted metadata.
pub fn dimensions_from_jsonl<'a>(lines: impl IntoIterator<Item = &'a str>) -> SessionProvenance {
    let mut merged = SessionProvenance::default();
    for line in lines {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("schema").and_then(serde_json::Value::as_str) != Some(crate::inbox::SCHEMA) {
            continue;
        }
        let Some(raw) = value.get("dimensions") else {
            continue;
        };
        let Ok(dimensions) = serde_json::from_value::<SessionProvenance>(raw.clone()) else {
            continue;
        };
        if dimensions.is_valid() {
            merge(&mut merged, &dimensions);
        }
    }
    merged
}

impl SessionProvenance {
    /// Add one fact without replacing observations made by another origin.
    pub fn insert_surface(&mut self, value: impl Into<String>) {
        let value = value.into();
        if !self.surface.contains(&value) {
            self.surface.push(value);
            self.surface.sort();
        }
    }
}

/// One append-only observation about a session's source dimensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceObservation {
    pub session_id: String,
    pub dimensions: SessionProvenance,
    /// Number of sealed shards hashed for this observation.
    pub body_shard_count: usize,
    pub body_sha256: String,
    /// Exact sealed shard sequence numbers whose concatenated bytes were
    /// hashed. Empty on legacy records, which retain prefix-count semantics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub body_shard_sequences: Vec<u64>,
}

fn observations_path(stage: &Path, machine: &str) -> PathBuf {
    stage.join("meta").join(machine).join("provenance-v1.jsonl")
}

/// Append new non-empty source observations, deduplicating only identical
/// `(session, dimensions)` facts. Existing rows are never rewritten.
pub fn append_scan_observation(
    stage: &Path,
    machine: &str,
    session_id: &str,
    dimensions: &SessionProvenance,
    body_shard_count: usize,
    body_sha256: &str,
) -> anyhow::Result<()> {
    if is_empty(dimensions) {
        return Ok(());
    }
    if !dimensions.is_valid() {
        anyhow::bail!("provenance dimensions are malformed");
    }
    if body_shard_count == 0 {
        return Ok(());
    }
    let path = observations_path(stage, machine);
    let session_dir = crate::store::session_shard_dir(stage, machine, session_id);
    let mut shard_entries = crate::store::sealed_shard_entries(&session_dir)?;
    shard_entries.sort_by_key(|(sequence, _)| *sequence);
    if shard_entries.len() != body_shard_count {
        anyhow::bail!(
            "provenance shard count does not match stage for `{}`",
            crate::id::short_session_id(session_id)
        );
    }
    if shard_entries.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        anyhow::bail!("provenance shard sequences are not unique and ordered");
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    for (_, shard_path) in &shard_entries {
        let mut shard = fs::File::open(shard_path)?;
        loop {
            let count = shard.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
    }
    let observed_sha256 = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if observed_sha256 != body_sha256 {
        anyhow::bail!(
            "provenance body SHA-256 does not match stage for `{}`",
            crate::id::short_session_id(session_id)
        );
    }
    let body_shard_sequences: Vec<u64> = shard_entries
        .iter()
        .map(|(sequence, _)| *sequence)
        .collect();
    let parent = path.parent().expect("provenance path has parent");
    fs::create_dir_all(parent)?;
    let mut seen = BTreeSet::new();
    match fs::File::open(&path) {
        Ok(file) => {
            for line in BufReader::new(file).lines() {
                let line = line?;
                let row: ProvenanceObservation = serde_json::from_str(&line)?;
                seen.insert((
                    row.session_id,
                    serde_json::to_string(&row.dimensions)?,
                    row.body_shard_count,
                    row.body_sha256,
                    row.body_shard_sequences,
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let dimensions_json = serde_json::to_string(dimensions)?;
    if seen.contains(&(
        session_id.to_string(),
        dimensions_json.clone(),
        body_shard_count,
        body_sha256.to_string(),
        body_shard_sequences.clone(),
    )) {
        return Ok(());
    }
    let row = ProvenanceObservation {
        session_id: session_id.to_string(),
        dimensions: dimensions.clone(),
        body_shard_count,
        body_sha256: body_sha256.to_string(),
        body_shard_sequences,
    };
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    serde_json::to_writer(&mut file, &row)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

/// Read all complete observations for one machine. A corrupt row is an error,
/// never absence, so callers cannot silently lose provenance on rebuild.
pub fn read_observations(
    stage: &Path,
    machine: &str,
) -> anyhow::Result<Vec<ProvenanceObservation>> {
    let path = observations_path(stage, machine);
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut BufReader::new(file), &mut bytes)?;
    parse_observations(&bytes)
}

pub fn parse_observations(bytes: &[u8]) -> anyhow::Result<Vec<ProvenanceObservation>> {
    let text = std::str::from_utf8(bytes)?;
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn is_empty(value: &SessionProvenance) -> bool {
    value.surface.is_empty()
        && value.tenant.is_empty()
        && value.container.is_empty()
        && value.status.is_empty()
}

/// Merge another observation while preserving every distinct value.
pub fn merge(into: &mut SessionProvenance, from: &SessionProvenance) {
    fn add(to: &mut Vec<String>, values: &[String]) {
        for value in values {
            if !to.contains(value) {
                to.push(value.clone());
            }
        }
        to.sort();
    }
    add(&mut into.surface, &from.surface);
    add(&mut into.tenant, &from.tenant);
    add(&mut into.container, &from.container);
    add(&mut into.status, &from.status);
}

/// Merge legacy observations bound to the exact raw shard prefix of this
/// session. New observations use explicit shard sequences below; a reordered,
/// replaced, or unrelated body's stale observation remains unknown.
pub fn merge_verified_observations(
    into: &mut SessionProvenance,
    session_id: &str,
    observations: &[ProvenanceObservation],
    shard_bodies: &[Vec<u8>],
) {
    for observation in observations {
        if observation.session_id != session_id
            || observation.body_shard_count == 0
            || observation.body_shard_count > shard_bodies.len()
            || !observation.body_shard_sequences.is_empty()
            || !observation.dimensions.is_valid()
        {
            continue;
        }
        let mut digest = Sha256::new();
        for body in &shard_bodies[..observation.body_shard_count] {
            digest.update(body);
        }
        let observed_sha = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if observed_sha == observation.body_sha256 {
            merge(into, &observation.dimensions);
        }
    }
}

/// Merge sequence-bound observations against archive shards collected across
/// snapshots. New observations name their actual sealed sequence files; legacy
/// rows without that metadata retain the historical prefix behavior.
pub fn merge_verified_shard_sequence_observations(
    into: &mut SessionProvenance,
    session_id: &str,
    observations: &[ProvenanceObservation],
    shard_bodies: &[(Option<u64>, Vec<u8>)],
) {
    let bodies: Vec<Vec<u8>> = shard_bodies.iter().map(|(_, body)| body.clone()).collect();
    let legacy: Vec<_> = observations
        .iter()
        .filter(|row| row.body_shard_sequences.is_empty())
        .cloned()
        .collect();
    merge_verified_observations(into, session_id, &legacy, &bodies);

    let mut by_sequence: BTreeMap<u64, &[u8]> = BTreeMap::new();
    let mut ambiguous_sequences = BTreeSet::new();
    for (sequence, body) in shard_bodies {
        let Some(sequence) = sequence else {
            continue;
        };
        if by_sequence.insert(*sequence, body.as_slice()).is_some() {
            by_sequence.remove(sequence);
            ambiguous_sequences.insert(*sequence);
        }
    }
    for observation in observations {
        let sequences = &observation.body_shard_sequences;
        if observation.session_id != session_id
            || sequences.is_empty()
            || sequences.len() != observation.body_shard_count
            || sequences.iter().any(|sequence| *sequence == 0)
            || sequences.windows(2).any(|pair| pair[0] >= pair[1])
            || !observation.dimensions.is_valid()
        {
            continue;
        }
        let mut digest = Sha256::new();
        let mut complete = true;
        for sequence in sequences {
            if ambiguous_sequences.contains(sequence) {
                complete = false;
                break;
            } else if let Some(body) = by_sequence.get(sequence) {
                digest.update(body);
            } else {
                complete = false;
                break;
            }
        }
        if !complete {
            continue;
        }
        let observed_sha = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if observed_sha == observation.body_sha256 {
            merge(into, &observation.dimensions);
        }
    }
}

/// Return sequence numbers whose exact bodies are bound by valid append-only
/// observations. Archive readers use this evidence to distinguish a genuine
/// repeated turn sealed at a new sequence from an unbound replay duplicate.
pub fn verified_observation_sequences(
    session_id: &str,
    observations: &[ProvenanceObservation],
    shard_bodies: &[(Option<u64>, Vec<u8>)],
) -> BTreeSet<u64> {
    let by_sequence: BTreeMap<u64, &[u8]> = shard_bodies
        .iter()
        .filter_map(|(sequence, body)| Some(((*sequence)?, body.as_slice())))
        .collect();
    let mut verified = BTreeSet::new();
    for observation in observations {
        let sequences = &observation.body_shard_sequences;
        if observation.session_id != session_id
            || sequences.is_empty()
            || sequences.len() != observation.body_shard_count
            || sequences.iter().any(|sequence| *sequence == 0)
            || sequences.windows(2).any(|pair| pair[0] >= pair[1])
            || !observation.dimensions.is_valid()
        {
            continue;
        }
        let mut digest = Sha256::new();
        let complete = sequences.iter().all(|sequence| {
            if let Some(body) = by_sequence.get(sequence) {
                digest.update(body);
                true
            } else {
                false
            }
        });
        if complete {
            let observed_sha = digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            if observed_sha == observation.body_sha256 {
                verified.extend(sequences.iter().copied());
            }
        }
    }
    verified
}

/// Compatibility helper for callers that only have a complete raw body.
/// Sequence-aware archive rebuilds use [`merge_verified_shard_sequence_observations`].
pub fn merge_verified_single_shard_observations(
    into: &mut SessionProvenance,
    session_id: &str,
    observations: &[ProvenanceObservation],
    raw_body: &[u8],
) {
    let digest = Sha256::digest(raw_body)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    for observation in observations {
        if observation.session_id == session_id
            && observation.body_shard_count == 1
            && observation.body_sha256 == digest
            && observation.dimensions.is_valid()
        {
            merge(into, &observation.dimensions);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_only_observations_keep_resumed_surfaces_and_body_binding() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = crate::store::session_shard_dir(
            dir.path(),
            "machine-fixture",
            "harness-a.session-fixture",
        );
        fs::create_dir_all(&session_dir).unwrap();
        let first_body = b"fixture-shard-1\n";
        let second_body = b"fixture-shard-2\n";
        fs::write(session_dir.join("000001.jsonl"), first_body).unwrap();
        let mut cli = SessionProvenance::default();
        cli.insert_surface("cli");
        let mut app = SessionProvenance::default();
        app.insert_surface("app");
        let digest = |bytes: &[u8]| {
            Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };

        append_scan_observation(
            dir.path(),
            "machine-fixture",
            "harness-a.session-fixture",
            &cli,
            1,
            &digest(first_body),
        )
        .unwrap();
        append_scan_observation(
            dir.path(),
            "machine-fixture",
            "harness-a.session-fixture",
            &cli,
            1,
            &digest(first_body),
        )
        .unwrap();
        fs::write(session_dir.join("000002.jsonl"), second_body).unwrap();
        let complete_body = [first_body.as_slice(), second_body.as_slice()].concat();
        append_scan_observation(
            dir.path(),
            "machine-fixture",
            "harness-a.session-fixture",
            &app,
            2,
            &digest(&complete_body),
        )
        .unwrap();

        let rows = read_observations(dir.path(), "machine-fixture").unwrap();
        assert_eq!(
            rows.len(),
            2,
            "identical facts dedupe; changed body plus new surface is retained"
        );
        assert_eq!(rows[0].dimensions.surface, ["cli"]);
        assert_eq!(rows[1].dimensions.surface, ["app"]);
        assert_eq!(rows[0].body_sha256, digest(first_body));
        assert_eq!(rows[0].body_shard_sequences, [1]);
        assert_eq!(rows[1].body_shard_count, 2);
        assert_eq!(rows[1].body_shard_sequences, [1, 2]);
        let mut merged = SessionProvenance::default();
        for row in rows {
            merge(&mut merged, &row.dimensions);
        }
        assert_eq!(merged.surface, ["app", "cli"]);
    }

    #[test]
    fn empty_dimensions_do_not_mean_unknown_or_write_an_observation() {
        let dir = tempfile::tempdir().unwrap();
        append_scan_observation(
            dir.path(),
            "machine-fixture",
            "session-fixture",
            &SessionProvenance::default(),
            1,
            "sha-fixture",
        )
        .unwrap();
        assert!(read_observations(dir.path(), "machine-fixture")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn legacy_observation_without_sequence_metadata_remains_readable() {
        let row: ProvenanceObservation = serde_json::from_str(
            r#"{"session_id":"fixture-session","dimensions":{"surface":["cli"],"tenant":[],"container":[],"status":[]},"body_shard_count":1,"body_sha256":"fixture-sha"}"#,
        )
        .unwrap();
        assert_eq!(row.session_id, "fixture-session");
        assert_eq!(row.dimensions.surface, ["cli"]);
        assert!(row.body_shard_sequences.is_empty());
    }

    #[test]
    fn valid_captured_dimensions_are_folded_but_malformed_values_stay_unknown() {
        let lines = [
            r#"{"schema":"chat-stasher/inbox@1","dimensions":{"surface":["cli","app"],"tenant":[],"container":[],"status":[]}}"#,
            r#"{"schema":"chat-stasher/inbox@1","dimensions":{"surface":[""],"tenant":[],"container":[],"status":[]}}"#,
            r#"{"schema":"chat-stasher/inbox@1","dimensions":{"surface":["untrusted"],"other":[]}}"#,
            r#"{"schema":"foreign/event@1","dimensions":{"surface":["wrong"],"tenant":[],"container":[],"status":[]}}"#,
        ];
        let dimensions = dimensions_from_jsonl(lines);
        assert_eq!(dimensions.surface, ["app", "cli"]);
        assert!(dimensions.tenant.is_empty());
        assert!(dimensions.container.is_empty());
        assert!(dimensions.status.is_empty());
    }

    #[test]
    fn resumed_overlap_keeps_each_surface_and_rejects_wrong_body_identity() {
        let cli_body = b"event-0\n".to_vec();
        let resumed_body = b"event-1\n".to_vec();
        let mut first = Sha256::new();
        first.update(&cli_body);
        let mut resumed = Sha256::new();
        resumed.update(&cli_body);
        resumed.update(&resumed_body);
        let digest = |value: sha2::digest::Output<Sha256>| {
            value
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let mut cli = SessionProvenance::default();
        cli.insert_surface("cli");
        let mut app = SessionProvenance::default();
        app.insert_surface("app");
        let rows = vec![
            ProvenanceObservation {
                session_id: "harness-a.machine.same-uuid".into(),
                dimensions: cli,
                body_shard_count: 1,
                body_sha256: digest(first.finalize()),
                body_shard_sequences: Vec::new(),
            },
            ProvenanceObservation {
                session_id: "harness-a.machine.same-uuid".into(),
                dimensions: app,
                body_shard_count: 2,
                body_sha256: digest(resumed.finalize()),
                body_shard_sequences: Vec::new(),
            },
            ProvenanceObservation {
                session_id: "harness-a.machine.other-uuid".into(),
                dimensions: SessionProvenance {
                    surface: vec!["unknown".into()],
                    ..Default::default()
                },
                body_shard_count: 1,
                body_sha256: "never-match".into(),
                body_shard_sequences: Vec::new(),
            },
        ];
        let shards = vec![cli_body, resumed_body];
        let mut observed = SessionProvenance::default();
        merge_verified_observations(&mut observed, "harness-a.machine.same-uuid", &rows, &shards);
        assert_eq!(observed.surface, ["app", "cli"]);

        let mut reordered = SessionProvenance::default();
        merge_verified_observations(
            &mut reordered,
            "harness-a.machine.same-uuid",
            &rows,
            &[shards[1].clone(), shards[0].clone()],
        );
        assert!(
            reordered.surface.is_empty(),
            "full SHA-256 must reject reordered content"
        );
    }

    #[test]
    fn sequence_bound_observations_survive_reclaim_and_require_exact_full_hash() {
        let shard_one = b"old archived prefix\n".to_vec();
        let shard_two = b"new resumed tail\n".to_vec();
        let hash = |body: &[u8]| {
            Sha256::digest(body)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let mut cli = SessionProvenance::default();
        cli.insert_surface("cli");
        let mut app = SessionProvenance::default();
        app.insert_surface("app");
        let rows = vec![
            ProvenanceObservation {
                session_id: "harness-a.machine.same".into(),
                dimensions: cli,
                body_shard_count: 1,
                body_sha256: hash(&shard_one),
                body_shard_sequences: vec![1],
            },
            ProvenanceObservation {
                session_id: "harness-a.machine.same".into(),
                dimensions: app,
                body_shard_count: 1,
                body_sha256: hash(&shard_two),
                body_shard_sequences: vec![2],
            },
            ProvenanceObservation {
                session_id: "harness-a.machine.same".into(),
                dimensions: SessionProvenance {
                    surface: vec!["ide".into()],
                    ..Default::default()
                },
                body_shard_count: 1,
                body_sha256: "0".repeat(64),
                body_shard_sequences: vec![2],
            },
            ProvenanceObservation {
                session_id: "harness-a.machine.same".into(),
                dimensions: SessionProvenance {
                    surface: vec![String::new()],
                    ..Default::default()
                },
                body_shard_count: 1,
                body_sha256: hash(b"new resumed tail\n"),
                body_shard_sequences: vec![2],
            },
        ];
        let archived = vec![(Some(1), shard_one), (Some(2), shard_two)];
        let mut observed = SessionProvenance::default();
        merge_verified_shard_sequence_observations(
            &mut observed,
            "harness-a.machine.same",
            &rows,
            &archived,
        );
        assert_eq!(observed.surface, ["app", "cli"]);
    }
}
