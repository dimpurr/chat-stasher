//! Rebuildable per-message audit projection; no archive, ingestion or transport IO.
//!
//! The v1 sidecar belongs at `meta/<machine>/message-audit-v1.jsonl`. Each JSONL
//! entry is tagged and versioned: either a message row or a body extraction
//! outcome. Positions are JSON pointers within a JSON body, or zero-based byte
//! offsets of JSONL records followed by a pointer within that record. Body
//! SHA-256 always covers the exact supplied bytes, never reserialized JSON.
//!
//! Join policy: a caller supplies one random 256-bit secret per archive, shared
//! by its partitions and retained for rebuilds. HMAC-SHA256 separates event-id
//! classes, harnesses, session references and cwd values. `key_scope` is a keyed
//! public identifier, not the secret. Rotation starts a separate sidecar lineage
//! (retain the old secret and file); mixed scopes are refused. Keys are never
//! derived from public machine/session IDs and are never persisted here.
//!
//! Append is a pure codec operation: existing bytes are kept verbatim, retries
//! add nothing, conflicting rows fail. The caller must lock and durably append
//! the returned suffix; missing/unreadable files must be handled by that caller,
//! not translated into an empty slice. RI-3d owns that wiring.

use anyhow::{ensure, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const SCHEMA: &str = "chat-stasher/message-audit@1";
pub const FILE_NAME: &str = "message-audit-v1.jsonl";

/// Does not implement Debug or Serialize: the archive secret must stay private.
pub struct JoinPolicy {
    secret: [u8; 32],
}
impl JoinPolicy {
    pub fn new(secret: [u8; 32]) -> Self {
        Self { secret }
    }
    pub fn key_scope(&self) -> String {
        self.join("", "scope", "message-audit-v1")
    }
    fn join(&self, harness: &str, class: &str, value: &str) -> String {
        let mut input = Vec::new();
        for field in ["chat-stasher/message-audit@1", harness, class, value] {
            input.extend_from_slice(&(field.len() as u64).to_be_bytes());
            input.extend_from_slice(field.as_bytes());
        }
        hmac(&self.secret, &input)
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
// RFC 2104, SHA-256's 64-byte block. The policy always supplies a 32-byte key.
fn hmac(key: &[u8; 32], bytes: &[u8]) -> String {
    let mut inner = [0x36; 64];
    let mut outer = [0x5c; 64];
    for (i, b) in key.iter().enumerate() {
        inner[i] ^= b;
        outer[i] ^= b;
    }
    let mut hash = Sha256::new();
    hash.update(inner);
    hash.update(bytes);
    let result = hash.finalize();
    let mut hash = Sha256::new();
    hash.update(outer);
    hash.update(result);
    hex(&hash.finalize())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetadataSource {
    Captured,
    Declared,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "value", rename_all = "lowercase", deny_unknown_fields)]
pub enum Fidelity {
    Raw,
    Full { representation: Representation },
    Partial { note: String },
    Unknown { reason: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Representation {
    Api,
    Decoded,
    Export,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FidelityMetadata {
    pub source: MetadataSource,
    #[serde(flatten)]
    pub fidelity: Fidelity,
}
impl<'de> Deserialize<'de> for FidelityMetadata {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        use serde::de::Error;
        let mut fields = BTreeMap::<String, Value>::deserialize(deserializer)?;
        let source = fields
            .remove("source")
            .ok_or_else(|| D::Error::custom("missing fidelity source"))?;
        let source = serde_json::from_value(source)
            .map_err(|_| D::Error::custom("invalid fidelity source"))?;
        let fidelity = serde_json::from_value(Value::Object(fields.into_iter().collect()))
            .map_err(|_| D::Error::custom("invalid fidelity fields"))?;
        Ok(Self { source, fidelity })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProducerKind {
    Send,
    Extension,
    Collect,
    Ingest,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Producer {
    pub kind: ProducerKind,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(rename = "sendKeyId", skip_serializing_if = "Option::is_none")]
    pub send_key_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerMetadata {
    pub source: MetadataSource,
    pub producer: Producer,
}

/// No capture timestamp fallback; an invalid source value is never copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "classification", rename_all = "lowercase", deny_unknown_fields)]
pub enum EventTime {
    Missing,
    Null { source: String },
    Invalid { source: String },
    Parsed { source: String, unix_millis: i64 },
}
/// Only numeric/null leaves are admitted. Unknown future usage counters retain
/// their original names; arbitrary strings (including error text) cannot enter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Usage {
    Number(serde_json::Number),
    Null,
    Object(BTreeMap<String, Usage>),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldState {
    Missing,
    Null,
    Present,
    Invalid,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRow {
    pub session: String,
    pub harness: String,
    pub body_sha256: String,
    pub record_position: String,
    pub event_id_class: String,
    pub event_key: String,
    /// Additional keyed identifiers let parent UUIDs join even when the primary
    /// event key uses a provider message ID. No raw identifier is retained.
    pub join_keys: BTreeMap<String, String>,
    /// Preserve missing/null/invalid observations without copying unsafe values.
    pub field_states: BTreeMap<String, FieldState>,
    pub timestamp: EventTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub usage: BTreeMap<String, Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidechain: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_hash: Option<String>,
    pub fidelity: FidelityMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer: Option<ProducerMetadata>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtractionStatus {
    Complete,
    Partial,
    Failed,
    Unsupported,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionOutcome {
    pub session: String,
    pub harness: String,
    pub body_sha256: String,
    pub status: ExtractionStatus,
    pub recognized: u64,
    pub unsupported: u64,
    pub malformed: u64,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub key_scope: String,
    pub rows: Vec<AuditRow>,
    pub outcomes: Vec<ExtractionOutcome>,
}
/// Explicit input for future collection/migration callers. Legacy callers must
/// use declared unknown, never guess from a registry or from the harness name.
#[derive(Debug, Clone)]
pub struct BodyMetadata {
    pub session: String,
    pub harness: String,
    pub fidelity: FidelityMetadata,
    pub producer: Option<ProducerMetadata>,
    pub subagent: Option<bool>,
    pub parent_native_session: Option<String>,
    pub native_session: Option<String>,
}
impl BodyMetadata {
    pub fn legacy(session: String, harness: String) -> Self {
        Self {
            session,
            harness,
            fidelity: FidelityMetadata {
                source: MetadataSource::Declared,
                fidelity: Fidelity::Unknown {
                    reason: "capture metadata was not recorded".into(),
                },
            },
            producer: None,
            subagent: None,
            parent_native_session: None,
            native_session: None,
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.session.trim().is_empty() && safe_label(&self.harness),
            "invalid audit source identity"
        );
        match &self.fidelity.fidelity {
            Fidelity::Partial { note } => {
                ensure!(!note.trim().is_empty(), "partial fidelity requires a note")
            }
            Fidelity::Unknown { reason } => ensure!(
                !reason.trim().is_empty(),
                "unknown fidelity requires a reason"
            ),
            _ => (),
        }
        if let Some(p) = &self.producer {
            ensure!(
                !p.producer.version.trim().is_empty(),
                "invalid audit producer version"
            );
            for label in [&p.producer.platform, &p.producer.send_key_id]
                .into_iter()
                .flatten()
            {
                ensure!(safe_label(label), "invalid audit producer label");
            }
        }
        Ok(())
    }
}

/// Validate the incoming bundle contract, decode its authoritative raw body and
/// project it. `session` is the archive identity supplied by the caller, never
/// reconstructed from a receiving machine, a path or a send key.
pub fn project_bundle(bytes: &[u8], session: &str, key: &JoinPolicy) -> Result<Projection> {
    crate::inbox::check_bundle(bytes).map_err(|_| anyhow::anyhow!("invalid audit bundle"))?;
    let v: Value =
        serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("invalid audit bundle JSON"))?;
    let v3 = v["schema"] == "chat-stasher/inbox@3";
    let harness_file = v3 && v["kind"] == "harness-file";
    let harness = v[if harness_file { "harness" } else { "platform" }]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing audit harness"))?;
    let mut meta = BodyMetadata::legacy(session.into(), harness.into());
    if v3 {
        meta.fidelity = FidelityMetadata {
            source: MetadataSource::Captured,
            fidelity: serde_json::from_value(v["fidelity"].clone())
                .map_err(|_| anyhow::anyhow!("invalid audit fidelity"))?,
        };
        if let Some(p) = v.get("producer") {
            meta.producer = Some(ProducerMetadata {
                source: MetadataSource::Captured,
                producer: serde_json::from_value(p.clone())
                    .map_err(|_| anyhow::anyhow!("invalid audit producer"))?,
            });
        }
        if harness_file {
            meta.native_session = v["nativeSessionId"].as_str().map(String::from);
            meta.subagent = Some(v["file"]["role"] == "subagent");
            meta.parent_native_session = v["file"]["parentNativeSessionId"]
                .as_str()
                .map(String::from);
        }
    }
    let body = if harness_file {
        let data = v["raw"]["data"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing audit bytes"))?;
        if v["raw"]["encoding"] == "base64" {
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| anyhow::anyhow!("invalid audit encoding"))?
        } else {
            data.as_bytes().to_vec()
        }
    } else if let Some(text) = v["raw"]["text"].as_str() {
        text.as_bytes().to_vec()
    } else if v.get("messages").is_some() {
        bytes.to_vec()
    } else {
        anyhow::bail!("missing authoritative audit body")
    };
    project_body(&body, &meta, key)
}

/// Extract Claude Code JSON/JSONL, Codex JSON/JSONL and OpenCode session exports.
/// Web APIs and other harnesses are deliberately unsupported until a dated
/// extractor is added; no generic recursive search through conversation text.
pub fn project_body(body: &[u8], meta: &BodyMetadata, key: &JoinPolicy) -> Result<Projection> {
    meta.validate()?;
    let sha = digest(body);
    let mut p = Projection {
        key_scope: key.key_scope(),
        rows: Vec::new(),
        outcomes: Vec::new(),
    };
    let mut outcome = ExtractionOutcome {
        session: meta.session.clone(),
        harness: meta.harness.clone(),
        body_sha256: sha.clone(),
        status: ExtractionStatus::Complete,
        recognized: 0,
        unsupported: 0,
        malformed: 0,
    };
    if !matches!(meta.harness.as_str(), "claude-code" | "codex" | "opencode") {
        outcome.status = ExtractionStatus::Unsupported;
    } else if body.iter().any(|b| !b.is_ascii_whitespace()) {
        if let Ok(v) = serde_json::from_slice::<Value>(body) {
            extract_records(&v, "json", meta, key, &sha, &mut p.rows, &mut outcome);
        } else {
            let mut offset = 0;
            for line in body.split_inclusive(|b| *b == b'\n') {
                if line.iter().any(|b| !b.is_ascii_whitespace()) {
                    match serde_json::from_slice::<Value>(line) {
                        Ok(v) => extract_records(
                            &v,
                            &format!("byte:{offset}"),
                            meta,
                            key,
                            &sha,
                            &mut p.rows,
                            &mut outcome,
                        ),
                        Err(_) => outcome.malformed += 1,
                    }
                }
                offset += line.len();
            }
        }
        outcome.status = if outcome.malformed > 0 && outcome.recognized == 0 {
            ExtractionStatus::Failed
        } else if outcome.recognized == 0 && outcome.unsupported > 0 {
            ExtractionStatus::Unsupported
        } else if outcome.malformed > 0 || outcome.unsupported > 0 {
            ExtractionStatus::Partial
        } else {
            ExtractionStatus::Complete
        };
    }
    outcome.recognized = p.rows.len() as u64;
    p.outcomes.push(outcome);
    Ok(p)
}
fn extract_records(
    v: &Value,
    position: &str,
    meta: &BodyMetadata,
    key: &JoinPolicy,
    sha: &str,
    rows: &mut Vec<AuditRow>,
    outcome: &mut ExtractionOutcome,
) {
    let array = v
        .as_array()
        .map(|a| ("", a))
        .or_else(|| {
            v.get("records")
                .and_then(Value::as_array)
                .map(|a| ("/records", a))
        })
        .or_else(|| {
            if meta.harness == "opencode" {
                v.get("messages")
                    .and_then(Value::as_array)
                    .map(|a| ("/messages", a))
            } else {
                None
            }
        });
    if let Some((prefix, array)) = array {
        for (i, record) in array.iter().enumerate() {
            extract_records(
                record,
                &format!("{position}{prefix}/{i}"),
                meta,
                key,
                sha,
                rows,
                outcome,
            );
        }
        return;
    }
    if let Some(row) = extract_event(v, position, meta, key, sha) {
        rows.push(row);
        outcome.recognized += 1;
    } else {
        outcome.unsupported += 1;
    }
}
fn safe_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-:/".contains(&b))
        && !s.starts_with('/')
        && !s.contains("..")
}
fn label(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| safe_label(s))
        .map(String::from)
}
fn numeric(v: &Value) -> Option<Usage> {
    match v {
        Value::Number(n) => Some(Usage::Number(n.clone())),
        Value::Null => Some(Usage::Null),
        Value::Object(o) => {
            let m: BTreeMap<_, _> = o
                .iter()
                .filter(|(name, _)| safe_label(name))
                .filter_map(|(name, v)| numeric(v).map(|v| (name.clone(), v)))
                .collect();
            if m.is_empty() && !o.is_empty() {
                None
            } else {
                Some(Usage::Object(m))
            }
        }
        _ => None,
    }
}
fn event_time(v: Option<&Value>, source: &str, numeric_millis: bool) -> EventTime {
    let Some(v) = v else {
        return EventTime::Missing;
    };
    if v.is_null() {
        return EventTime::Null {
            source: source.into(),
        };
    }
    let millis = if numeric_millis {
        v.as_i64()
            .filter(|ms| chrono::DateTime::from_timestamp_millis(*ms).is_some())
    } else {
        v.as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp_millis())
    };
    match millis {
        Some(unix_millis) => EventTime::Parsed {
            source: source.into(),
            unix_millis,
        },
        None => EventTime::Invalid {
            source: source.into(),
        },
    }
}
fn extract_event(
    v: &Value,
    position: &str,
    meta: &BodyMetadata,
    key: &JoinPolicy,
    sha: &str,
) -> Option<AuditRow> {
    let (event, id, id_class, model, provider, cwd, timestamp) = match meta.harness.as_str() {
        "claude-code" if matches!(v["type"].as_str(), Some("assistant" | "user" | "system")) => {
            let (id, class) = if v["message"]["id"].is_string() {
                (v["message"]["id"].as_str(), "message-id")
            } else {
                (v["uuid"].as_str(), "uuid")
            };
            (
                v,
                id,
                class,
                label(v["message"].get("model")),
                label(v["message"].get("provider")),
                v["cwd"].as_str(),
                event_time(v.get("timestamp"), "timestamp", false),
            )
        }
        "codex"
            if matches!(
                v["type"].as_str(),
                Some("event_msg" | "turn_context" | "response_item" | "session_meta")
            ) =>
        {
            let e = v.get("payload").or_else(|| v.get("event_msg")).unwrap_or(v);
            (
                e,
                v["id"].as_str(),
                "event-id",
                label(e.get("model").or_else(|| e["turn_context"].get("model"))),
                label(e.get("model_provider")),
                e["cwd"].as_str(),
                event_time(v.get("timestamp"), "timestamp", false),
            )
        }
        "opencode" => {
            let e = v.get("data").filter(|x| x.is_object()).unwrap_or(v);
            if !matches!(e["role"].as_str(), Some("assistant" | "user" | "system")) {
                return None;
            }
            (
                e,
                v["id"].as_str().or_else(|| e["id"].as_str()),
                "message-id",
                label(e.get("modelID")),
                label(e.get("providerID")),
                e["path"]["cwd"].as_str(),
                event_time(e["time"].get("created"), "time.created", true),
            )
        }
        _ => return None,
    };
    let mut usage = BTreeMap::new();
    let usage_fields = match meta.harness.as_str() {
        "claude-code" => vec![("usage", v["message"].get("usage"))],
        "codex" => vec![
            ("last_token_usage", event["info"].get("last_token_usage")),
            ("total_token_usage", event["info"].get("total_token_usage")),
            ("rate_limits", event.get("rate_limits")),
        ],
        _ => vec![("tokens", event.get("tokens")), ("cost", event.get("cost"))],
    };
    for (name, value) in usage_fields {
        if let Some(n) = value.and_then(numeric) {
            usage.insert(name.into(), n);
        }
    }
    let (class, value) = match id {
        Some(id) => (id_class, id.to_string()),
        None => ("position", format!("{}:{sha}:{position}", meta.session)),
    };
    let parent = if let Some(parent) = v["parentUuid"].as_str() {
        Some(("uuid", parent))
    } else {
        meta.parent_native_session
            .as_deref()
            .map(|p| ("native-session", p))
    };
    let mut join_keys = BTreeMap::new();
    for (class, id) in [(id_class, id), ("uuid", v["uuid"].as_str())] {
        if let Some(id) = id {
            join_keys.insert(class.into(), key.join(&meta.harness, class, id));
        }
    }
    if let Some(native) = &meta.native_session {
        join_keys.insert(
            "native-session".into(),
            key.join(&meta.harness, "native-session", native),
        );
    }
    let mut field_states = BTreeMap::new();
    for (name, observed) in [
        (
            "provider",
            if meta.harness == "opencode" {
                event.get("providerID")
            } else if meta.harness == "claude-code" {
                v["message"].get("provider")
            } else {
                event.get("model_provider")
            },
        ),
        ("error", event.get("error")),
        ("event_id", v.get("id")),
        ("message_id", v["message"].get("id")),
        ("uuid", v.get("uuid")),
        ("api_error_flag", v.get("isApiErrorMessage")),
    ] {
        field_states.insert(
            name.into(),
            match observed {
                None => FieldState::Missing,
                Some(Value::Null) => FieldState::Null,
                Some(_) => FieldState::Present,
            },
        );
    }
    for (name, value) in [
        ("sidechain", v.get("isSidechain")),
        ("parent", v.get("parentUuid")),
        (
            "cwd",
            if meta.harness == "opencode" {
                event["path"].get("cwd")
            } else {
                event.get("cwd")
            },
        ),
        (
            "model",
            if meta.harness == "claude-code" {
                v["message"].get("model")
            } else if meta.harness == "opencode" {
                event.get("modelID")
            } else {
                event
                    .get("model")
                    .or_else(|| event["turn_context"].get("model"))
            },
        ),
    ] {
        let state = match value {
            None => FieldState::Missing,
            Some(Value::Null) => FieldState::Null,
            Some(value)
                if (name == "sidechain" && !value.is_boolean())
                    || (name != "sidechain" && !value.is_string()) =>
            {
                FieldState::Invalid
            }
            Some(_) => FieldState::Present,
        };
        field_states.insert(name.into(), state);
    }
    let error_class = label(
        event["error"]
            .get("class")
            .or_else(|| event["error"].get("name")),
    )
    .or_else(|| (v["isApiErrorMessage"].as_bool() == Some(true)).then(|| "api-error".into()));
    Some(AuditRow {
        session: meta.session.clone(),
        harness: meta.harness.clone(),
        body_sha256: sha.into(),
        record_position: position.into(),
        event_id_class: class.into(),
        event_key: key.join(&meta.harness, class, &value),
        join_keys,
        field_states,
        timestamp,
        model,
        provider,
        usage,
        error_class,
        sidechain: v["isSidechain"].as_bool(),
        subagent: meta.subagent,
        parent_key: parent.map(|(class, value)| key.join(&meta.harness, class, value)),
        parent_class: parent.map(|(class, _)| class.into()),
        cwd_hash: cwd.map(|cwd| key.join("", "cwd", cwd)),
        fidelity: meta.fidelity.clone(),
        producer: meta.producer.clone(),
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "lowercase", deny_unknown_fields)]
enum Entry {
    Row {
        schema: String,
        key_scope: String,
        row: AuditRow,
    },
    Outcome {
        schema: String,
        key_scope: String,
        outcome: ExtractionOutcome,
    },
}
fn entries(p: &Projection) -> Vec<Entry> {
    p.rows
        .iter()
        .cloned()
        .map(|row| Entry::Row {
            schema: SCHEMA.into(),
            key_scope: p.key_scope.clone(),
            row,
        })
        .chain(p.outcomes.iter().cloned().map(|outcome| Entry::Outcome {
            schema: SCHEMA.into(),
            key_scope: p.key_scope.clone(),
            outcome,
        }))
        .collect()
}
fn identity(e: &Entry) -> String {
    match e {
        Entry::Row { row, .. } => serde_json::to_string(&(
            "row",
            &row.session,
            &row.body_sha256,
            &row.record_position,
            &row.event_key,
        ))
        .expect("string tuple serializes"),
        Entry::Outcome { outcome, .. } => serde_json::to_string(&(
            "outcome",
            &outcome.session,
            &outcome.harness,
            &outcome.body_sha256,
        ))
        .expect("string tuple serializes"),
    }
}
fn validate_entry(e: &Entry) -> Result<()> {
    let (schema, scope) = match e {
        Entry::Row {
            schema, key_scope, ..
        }
        | Entry::Outcome {
            schema, key_scope, ..
        } => (schema, key_scope),
    };
    ensure!(
        schema == SCHEMA && is_digest(scope),
        "unsupported audit version or key scope"
    );
    match e {
        Entry::Row { row, .. } => {
            BodyMetadata {
                session: row.session.clone(),
                harness: row.harness.clone(),
                fidelity: row.fidelity.clone(),
                producer: row.producer.clone(),
                subagent: row.subagent,
                parent_native_session: None,
                native_session: None,
            }
            .validate()?;
            ensure!(
                is_digest(&row.body_sha256) && is_digest(&row.event_key),
                "invalid audit digest"
            );
            ensure!(
                matches!(
                    row.event_id_class.as_str(),
                    "message-id" | "event-id" | "uuid" | "position"
                ),
                "invalid audit id class"
            );
            ensure!(
                valid_position(&row.record_position),
                "invalid audit record position"
            );
            for (class, hash) in &row.join_keys {
                ensure!(
                    matches!(
                        class.as_str(),
                        "message-id" | "event-id" | "uuid" | "native-session"
                    ) && is_digest(hash),
                    "invalid audit join key"
                );
            }
            ensure!(
                row.field_states.keys().all(|k| matches!(
                    k.as_str(),
                    "sidechain"
                        | "parent"
                        | "cwd"
                        | "model"
                        | "provider"
                        | "error"
                        | "event_id"
                        | "message_id"
                        | "uuid"
                        | "api_error_flag"
                )),
                "invalid audit field state"
            );
            for usage in row.usage.values() {
                validate_usage(usage)?;
            }
            ensure!(
                row.usage.keys().all(|k| matches!(
                    k.as_str(),
                    "usage"
                        | "last_token_usage"
                        | "total_token_usage"
                        | "rate_limits"
                        | "tokens"
                        | "cost"
                )),
                "invalid audit usage field"
            );
            match &row.timestamp {
                EventTime::Null { source }
                | EventTime::Invalid { source }
                | EventTime::Parsed { source, .. } => ensure!(
                    matches!(source.as_str(), "timestamp" | "time.created"),
                    "invalid audit time source"
                ),
                EventTime::Missing => (),
            }
            if let Some(class) = &row.parent_class {
                ensure!(
                    matches!(class.as_str(), "uuid" | "native-session"),
                    "invalid audit parent class"
                );
            }
            for hash in [&row.cwd_hash, &row.parent_key].into_iter().flatten() {
                ensure!(is_digest(hash), "invalid audit keyed hash");
            }
            ensure!(
                row.parent_key.is_some() == row.parent_class.is_some(),
                "invalid audit parent"
            );
            for label in [&row.model, &row.provider, &row.error_class]
                .into_iter()
                .flatten()
            {
                ensure!(safe_label(label), "invalid audit label");
            }
        }
        Entry::Outcome { outcome, .. } => ensure!(
            is_digest(&outcome.body_sha256),
            "invalid audit outcome digest"
        ),
    }
    Ok(())
}
fn valid_position(s: &str) -> bool {
    let mut parts = s.split('/');
    let Some(head) = parts.next() else {
        return false;
    };
    if head != "json"
        && !head
            .strip_prefix("byte:")
            .is_some_and(|offset| !offset.is_empty() && offset.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    parts.all(|part| {
        matches!(part, "records" | "messages")
            || (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    })
}
fn validate_usage(usage: &Usage) -> Result<()> {
    if let Usage::Object(map) = usage {
        for (name, value) in map {
            ensure!(safe_label(name), "invalid audit usage name");
            validate_usage(value)?;
        }
    }
    Ok(())
}
fn is_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
/// Strict reading: a truncated JSONL tail, unknown fields, unknown version or
/// conflicting duplicate is an error, never a successful empty audit.
pub fn decode_jsonl(bytes: &[u8]) -> Result<Projection> {
    ensure!(
        bytes.is_empty() || bytes.ends_with(b"\n"),
        "incomplete audit JSONL tail"
    );
    let mut p = Projection {
        key_scope: String::new(),
        rows: Vec::new(),
        outcomes: Vec::new(),
    };
    let mut seen = BTreeMap::new();
    for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let e: Entry = serde_json::from_slice(line)
            .map_err(|_| anyhow::anyhow!("invalid audit JSONL entry"))?;
        validate_entry(&e)?;
        let scope = match &e {
            Entry::Row { key_scope, .. } | Entry::Outcome { key_scope, .. } => key_scope,
        };
        if p.key_scope.is_empty() {
            p.key_scope = scope.clone();
        }
        ensure!(p.key_scope == *scope, "mixed audit key scopes");
        let value = serde_json::to_vec(&e)?;
        let id = identity(&e);
        if let Some(previous) = seen.get(&id) {
            ensure!(*previous == value, "conflicting audit entry");
            continue;
        }
        seen.insert(id, value);
        match e {
            Entry::Row { row, .. } => p.rows.push(row),
            Entry::Outcome { outcome, .. } => p.outcomes.push(outcome),
        }
    }
    Ok(p)
}
/// Return existing bytes plus only new entries. This does not write a file or
/// erase duplicate messages from authoritative bodies. Identity includes body
/// digest and position: different versions/positions remain separate facts.
pub fn append_jsonl(existing: &[u8], projection: &Projection) -> Result<Vec<u8>> {
    let old = decode_jsonl(existing)?;
    ensure!(
        old.key_scope.is_empty() || old.key_scope == projection.key_scope,
        "mixed audit key scopes"
    );
    let mut seen = BTreeMap::new();
    for e in entries(&old) {
        seen.insert(identity(&e), serde_json::to_vec(&e)?);
    }
    let mut output = existing.to_vec();
    for e in entries(projection) {
        validate_entry(&e)?;
        let bytes = serde_json::to_vec(&e)?;
        let id = identity(&e);
        if let Some(previous) = seen.get(&id) {
            ensure!(*previous == bytes, "conflicting audit entry");
        } else {
            output.extend_from_slice(&bytes);
            output.push(b'\n');
            seen.insert(id, bytes);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hmac_sha256_rfc4231_case_1() {
        // RFC case 1 uses a 20-byte key; zero-padding to 32 is equivalent.
        let mut key = [0; 32];
        key[..20].fill(0x0b);
        assert_eq!(
            hmac(&key, b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }
}
