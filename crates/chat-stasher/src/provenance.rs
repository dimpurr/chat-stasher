//! Shared, additive session provenance dimensions.
//!
//! The values are facts observed at capture time. Empty dimensions mean no
//! value was recorded; callers must not infer a value from the harness name.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
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
    /// The cumulative sealed body this observation was captured against.
    pub body_shard_count: usize,
    pub body_sha256: String,
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
    let path = observations_path(stage, machine);
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
    )) {
        return Ok(());
    }
    let row = ProvenanceObservation {
        session_id: session_id.to_string(),
        dimensions: dimensions.clone(),
        body_shard_count,
        body_sha256: body_sha256.to_string(),
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

/// Merge only observations bound to the exact raw shard prefix of this
/// session. This lets a prior observation survive append-only resume while a
/// reordered, replaced, or unrelated body's stale observation remains unknown.
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

/// Archive rebuilds currently deliver a session as its complete concatenated
/// payload. Accept provenance only when the archived observation describes a
/// one-shard body and its full digest matches; multi-shard prefix observations
/// need shard boundaries and therefore remain unknown on this path.
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
        let mut cli = SessionProvenance::default();
        cli.insert_surface("cli");
        let mut app = SessionProvenance::default();
        app.insert_surface("app");

        append_scan_observation(
            dir.path(),
            "machine-fixture",
            "harness-a.session-fixture",
            &cli,
            1,
            "sha-fixture-1",
        )
        .unwrap();
        append_scan_observation(
            dir.path(),
            "machine-fixture",
            "harness-a.session-fixture",
            &cli,
            1,
            "sha-fixture-1",
        )
        .unwrap();
        append_scan_observation(
            dir.path(),
            "machine-fixture",
            "harness-a.session-fixture",
            &app,
            2,
            "sha-fixture-2",
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
        assert_eq!(rows[0].body_sha256, "sha-fixture-1");
        assert_eq!(rows[1].body_shard_count, 2);
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
            },
            ProvenanceObservation {
                session_id: "harness-a.machine.same-uuid".into(),
                dimensions: app,
                body_shard_count: 2,
                body_sha256: digest(resumed.finalize()),
            },
            ProvenanceObservation {
                session_id: "harness-a.machine.other-uuid".into(),
                dimensions: SessionProvenance {
                    surface: vec!["unknown".into()],
                    ..Default::default()
                },
                body_shard_count: 1,
                body_sha256: "never-match".into(),
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
}
