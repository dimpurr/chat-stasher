//! Read-only reader for the Grok Bot desktop app's local transcript replicas.
//!
//! Replica records are observations, not a complete transcript: missing
//! sequence numbers are reported and every result carries `partial_replica`.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicaRecord {
    pub sequence: u64,
    pub raw: Value,
    #[serde(default)]
    pub sequence_gaps: Vec<u64>,
}

impl ReplicaRecord {
    pub fn from_value(raw: Value) -> Result<Self> {
        let sequence = raw
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("replica record has no unsigned seq"))?;
        if raw.get("kind").and_then(Value::as_str).is_none() {
            bail!("replica record has no string kind");
        }
        Ok(Self {
            sequence,
            raw,
            sequence_gaps: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentReplica {
    pub agent_id: String,
    pub name: Option<String>,
    pub source_path: PathBuf,
    pub records: Vec<ReplicaRecord>,
    pub sequence_gaps: Vec<u64>,
    pub partial_replica: bool,
}

/// Merge observations from successive local reads. Identical repeats are
/// idempotent; a conflicting payload at one sequence is an error, never an
/// overwrite of the earlier raw record.
pub fn merge_replica_records(
    previous: impl IntoIterator<Item = ReplicaRecord>,
    observed: impl IntoIterator<Item = ReplicaRecord>,
) -> Result<Vec<ReplicaRecord>> {
    let mut by_sequence = BTreeMap::<u64, Value>::new();
    for record in previous.into_iter().chain(observed) {
        if let Some(old) = by_sequence.get(&record.sequence) {
            if old != &record.raw {
                bail!(
                    "conflicting raw records at replica sequence {}",
                    record.sequence
                );
            }
        } else {
            by_sequence.insert(record.sequence, record.raw);
        }
    }
    let gaps = sequence_gaps(by_sequence.keys().copied());
    Ok(by_sequence
        .into_iter()
        .map(|(sequence, raw)| ReplicaRecord {
            sequence,
            raw,
            sequence_gaps: gaps.clone(),
        })
        .collect())
}

pub fn sequence_gaps(sequences: impl IntoIterator<Item = u64>) -> Vec<u64> {
    let sequences: BTreeSet<u64> = sequences.into_iter().collect();
    let (Some(first), Some(last)) = (sequences.first(), sequences.last()) else {
        return Vec::new();
    };
    (*first..=*last)
        .filter(|sequence| !sequences.contains(sequence))
        .collect()
}

/// Read every `transcript.replicas.<agent-uuid>` blob under the persistence
/// directory, plus roster-like name mappings found in local `.blob` files.
/// The app directory is opened read-only; parse and I/O failures are surfaced.
pub fn read_persistence(root: &Path) -> Result<Vec<AgentReplica>> {
    let entries = fs::read_dir(root)
        .with_context(|| format!("read Grok Bot persistence directory {}", root.display()))?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("enumerate {}", root.display()))?;
        let file_type = entry.file_type().with_context(|| {
            format!(
                "inspect Grok Bot persistence entry {}",
                entry.path().display()
            )
        })?;
        if file_type.is_file() {
            files.push(entry.path());
        }
    }
    files.sort();

    let mut names = BTreeMap::new();
    for path in &files {
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if filename.ends_with(".blob") && !filename.starts_with("transcript.replicas.") {
            let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                collect_agent_names(&value, &mut names);
            }
        }
    }

    let mut agents = Vec::new();
    for path in files {
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(agent_id) = filename.strip_prefix("transcript.replicas.") else {
            continue;
        };
        if !is_uuid(agent_id) {
            continue;
        }
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse Grok Bot replica {}", path.display()))?;
        let mut observed = Vec::new();
        collect_records(&value, &mut observed)?;
        let records = merge_replica_records([], observed)?;
        let sequence_gaps = records
            .first()
            .map(|record| record.sequence_gaps.clone())
            .unwrap_or_default(); // reason: an empty replica has no observed range in which to measure gaps.
        agents.push(AgentReplica {
            agent_id: agent_id.to_string(),
            name: names.get(agent_id).cloned(),
            source_path: path,
            records,
            sequence_gaps,
            partial_replica: true,
        });
    }
    Ok(agents)
}

/// Find the app's persistence leaf below its platform app-support directory.
/// Symlinks are ignored so a malformed local tree cannot redirect the reader
/// outside Grok Bot's own data directory.
pub fn read_from_app_support(app_support: &Path) -> Result<Vec<AgentReplica>> {
    let mut stack = vec![app_support.to_path_buf()];
    let mut roots = Vec::new();
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .with_context(|| format!("enumerate Grok Bot app data {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("enumerate {}", dir.display()))?;
            let file_type = entry.file_type().with_context(|| {
                format!("inspect Grok Bot app data entry {}", entry.path().display())
            })?;
            if !file_type.is_dir() {
                continue;
            }
            if entry.file_name() == "sand-client-persistence" {
                roots.push(entry.path());
            } else {
                stack.push(entry.path());
            }
        }
    }
    roots.sort();
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_agent = BTreeMap::<String, AgentReplica>::new();
    for root in roots {
        for mut agent in read_persistence(&root)? {
            if let Some(previous) = by_agent.remove(&agent.agent_id) {
                agent.records = merge_replica_records(previous.records, agent.records)?;
                agent.sequence_gaps = agent
                    .records
                    .first()
                    .map(|record| record.sequence_gaps.clone())
                    .unwrap_or_default(); // reason: an empty merged replica has no observed range in which to measure gaps.
                if agent.name.is_none() {
                    agent.name = previous.name;
                }
            }
            by_agent.insert(agent.agent_id.clone(), agent);
        }
    }
    Ok(by_agent.into_values().collect())
}

fn collect_records(value: &Value, records: &mut Vec<ReplicaRecord>) -> Result<()> {
    match value {
        Value::Object(map) => {
            if map.contains_key("seq") && map.contains_key("kind") {
                records.push(ReplicaRecord::from_value(value.clone())?);
                return Ok(());
            }
            for child in map.values() {
                collect_records(child, records)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_records(child, records)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_agent_names(value: &Value, names: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            let id = map
                .get("agentId")
                .or_else(|| map.get("id"))
                .and_then(Value::as_str);
            let name = map
                .get("displayName")
                .or_else(|| map.get("name"))
                .and_then(Value::as_str);
            if let (Some(id), Some(name)) = (id, name) {
                if is_uuid(id) {
                    names.insert(id.to_string(), name.to_string());
                }
            }
            for child in map.values() {
                collect_agent_names(child, names);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_agent_names(child, names);
            }
        }
        _ => {}
    }
}

fn is_uuid(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == 5
        && [8, 4, 4, 4, 12].iter().zip(parts).all(|(length, part)| {
            part.len() == *length && part.bytes().all(|b| b.is_ascii_hexdigit())
        })
}
