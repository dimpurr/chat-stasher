//! Read-only reader for the Grok Bot desktop app's local transcript replicas.
//!
//! On disk the app stores each client state in one blob file whose name is
//! the unpadded lowercase RFC 4648 base32 of its state key, plus a `.blob`
//! suffix (W321, measured locally): `sand.client.slice.account.<account
//! ref>.transcript.replicas.<agent-uuid>` holds one bot's observed transcript
//! window and the roster keys hold agent names. The keys, not the encoded
//! byte strings, are the identity the reader matches on.
//!
//! Replica records are observations, not a complete transcript: sequence
//! positions are numbered from one, so the missing numbers reported for an
//! agent run from 1 through the highest observed sequence, and every result
//! carries `partial_replica`.

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
        Ok(Self { sequence, raw })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentReplica {
    pub agent_id: String,
    pub name: Option<String>,
    /// The tenant this agent's state key is scoped to, extracted from the
    /// `account.<account_ref>` segment of the state key. `None` when the key
    /// carries no account scope.
    pub tenant: Option<String>,
    pub source_path: PathBuf,
    pub records: Vec<ReplicaRecord>,
    /// Missing sequence positions from 1 through `max_sequence`.
    pub sequence_gaps: Vec<u64>,
    /// The highest sequence observed so far — the inclusive upper bound of
    /// `sequence_gaps`; `None` when nothing has been observed.
    pub max_sequence: Option<u64>,
    pub partial_replica: bool,
}

/// Merge observations from successive local reads into one ordered union.
///
/// An identical repeat is idempotent. A position observed later with changed
/// content becomes an additional variant of that sequence: the archive is
/// append-only, so an earlier raw record is never overwritten — and the
/// union never holds a conflict, which a later run could neither resolve nor
/// get past (the observed key union includes mutable fields such as
/// `isStreaming`, so a rewrite is a normal observation, not corruption).
/// Records stay sorted by sequence, then by first observation.
pub fn merge_replica_records(
    previous: impl IntoIterator<Item = ReplicaRecord>,
    observed: impl IntoIterator<Item = ReplicaRecord>,
) -> Vec<ReplicaRecord> {
    let mut by_sequence: BTreeMap<u64, Vec<Value>> = BTreeMap::new();
    for record in previous.into_iter().chain(observed) {
        let variants = by_sequence.entry(record.sequence).or_default();
        if !variants.contains(&record.raw) {
            variants.push(record.raw);
        }
    }
    by_sequence
        .into_iter()
        .flat_map(|(sequence, variants)| {
            variants
                .into_iter()
                .map(move |raw| ReplicaRecord { sequence, raw })
        })
        .collect()
}

/// Missing sequence numbers from 1 through the highest observed value.
/// Positions before the first stored record are gaps like any other:
/// replica sequences are numbered from one (W321), so an absent head is
/// reported, not left to be inferred from where the records happen to start.
pub fn sequence_gaps(sequences: impl IntoIterator<Item = u64>) -> Vec<u64> {
    let sequences: BTreeSet<u64> = sequences.into_iter().collect();
    let Some(&max) = sequences.last() else {
        return Vec::new();
    };
    (1..=max)
        .filter(|sequence| !sequences.contains(sequence))
        .collect()
}

/// Read every replica blob under the persistence directory, plus the agent
/// names carried by roster-like blobs. The app directory is opened
/// read-only; parse and I/O failures of the blobs we claim to model are
/// surfaced, while entries we do not model — the migration sentinel, keys
/// this reader never heard of — are skipped, not guessed at.
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
        let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = filename.strip_suffix(".blob") else {
            continue;
        };
        let is_replica = match decode_state_key(stem) {
            Some(key) => replica_agent_id(&key).is_some(),
            None => false,
        };
        if is_replica {
            continue;
        }
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            collect_agent_names(&value, &mut names);
        }
    }

    let mut by_agent = BTreeMap::<String, AgentReplica>::new();
    for path in files {
        let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(stem) = filename.strip_suffix(".blob") else {
            continue;
        };
        let Some(key) = decode_state_key(stem) else {
            continue;
        };
        let Some(agent_id) = replica_agent_id(&key) else {
            continue;
        };
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse Grok Bot replica {}", path.display()))?;
        let mut observed = Vec::new();
        collect_records(&value, &mut observed)?;
        // More than one state key can end in the same agent uuid — two
        // account scopes, say. They are observations of one agent, so they
        // merge like successive reads; they must not become two sessions.
        let records = match by_agent.remove(agent_id) {
            Some(previous) => merge_replica_records(previous.records, observed),
            None => merge_replica_records([], observed),
        };
        let sequence_gaps = sequence_gaps(records.iter().map(|record| record.sequence));
        let max_sequence = records.last().map(|record| record.sequence);
        let name = names.get(agent_id).cloned();
        let tenant = account_ref(&key).map(str::to_string);
        let agent = AgentReplica {
            agent_id: agent_id.to_string(),
            name,
            tenant,
            source_path: path,
            records,
            sequence_gaps,
            max_sequence,
            partial_replica: true,
        };
        by_agent.insert(agent.agent_id.clone(), agent);
    }
    Ok(by_agent.into_values().collect())
}

/// Decode the stem of a persistence blob filename to its state key.
///
/// The stem is unpadded, lowercase RFC 4648 base32 (case-insensitive here)
/// of a UTF-8 state key. A stem that is not well-formed base32, or does not
/// decode to UTF-8, is not an error: the directory also carries files this
/// reader does not model — including an unencoded
/// `.migrated-from-local-storage` sentinel — and such a name only means
/// "not a key we can read", never "unreadable data".
fn decode_state_key(stem: &str) -> Option<String> {
    let mut buffer = 0u32;
    let mut bits = 0u32;
    let mut decoded = Vec::with_capacity(stem.len() * 5 / 8);
    for character in stem.chars() {
        let value = match character {
            'a'..='z' => character as u32 - 'a' as u32,
            'A'..='Z' => character as u32 - 'A' as u32,
            '2'..='7' => character as u32 - '2' as u32 + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | value;
        bits += 5;
        while bits >= 8 {
            bits -= 8;
            decoded.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// The state keys of replica blobs end in `.transcript.replicas.<agent-uuid>`
/// — one replica per bot, one observed window per replica (W321). Only that
/// suffix is the identity: the account-scoped `sand.client.slice.…` prefix
/// carries no meaning this reader depends on, so a re-scoped key still
/// resolves.
fn replica_agent_id(state_key: &str) -> Option<&str> {
    let (_, agent_id) = state_key.rsplit_once(".transcript.replicas.")?;
    is_uuid(agent_id).then_some(agent_id)
}

/// Extract the account reference from a replica state key.
///
/// The key format is `sand.client.slice.account.<account_ref>.transcript.replicas.<agent-uuid>`.
/// Returns `None` when the key does not carry an account scope.
fn account_ref(state_key: &str) -> Option<&str> {
    let rest = state_key.strip_prefix("sand.client.slice.account.")?;
    let (account, _) = rest.split_once(".transcript.replicas.")?;
    Some(account)
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
                agent.records = merge_replica_records(previous.records, agent.records);
                agent.sequence_gaps =
                    sequence_gaps(agent.records.iter().map(|record| record.sequence));
                agent.max_sequence = agent.records.last().map(|record| record.sequence);
                if agent.name.is_none() {
                    agent.name = previous.name;
                }
                if agent.tenant.is_none() {
                    agent.tenant = previous.tenant;
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
