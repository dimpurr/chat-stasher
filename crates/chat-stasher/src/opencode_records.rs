//! ADR-061 (SG-3): snapshot-style sources go increment per record, opencode
//! first.
//!
//! A snapshot source re-stores the whole session on every change; a real
//! opencode session grows 1–5 % per pass while the stage pays a complete copy.
//! This module holds the **record** shapes the increment era writes and the
//! one **assembly** every reader flows through; the collector lives in
//! [`crate::collect`] and the byte composition is shared with the source
//! reader (see [`compose_v1_line`]) so the assembled default view and a
//! fresh export are the *same serializer's* bytes, not two formats that
//! happen to agree.
//!
//! # The record shape (D1)
//!
//! One record is one line. The schema string is
//! [`SESSION_V2_SCHEMA`]; every line is self-describing: the session it
//! belongs to, its own record kind and id, and the SHA-256 of its payload
//! serialization. Message records carry the message row **without** its
//! parts inlined — parts are records of their own under their own
//! `part.id`, and assembly hangs them back onto their owning message by the
//! source's own columns (`message_id, time_created, id`), never by arrival
//! order. Deletions are append-only tombstone records carrying the word
//! `deleted` with a `kind` field distinguishing messages from parts; a
//! tombstone masks its record out of the view **without erasing the stored
//! bytes**.
//!
//! # Tombstone semantics and the byte contract (D3)
//!
//! A **whole-file v1 observation is a complete observation**: a record known
//! from earlier observances that does not appear in it is treated as deleted
//! as of that observation — an implicit tombstone. The increment era sends
//! explicit tombstones for its deletions. At quiescence the assembled
//! default view therefore equals a fresh export of the source's final
//! state, byte for byte ([`BodyView::Current`]). [`BodyView::IncludeDeleted`]
//! emits the deleted messages and parts at their original positions with
//! `"deleted": true` markers — the deleted history is never lost and stays
//! reachable.
//!
//! Absence is evidence of deletion only inside a complete observation; an id
//! absent from an increment shard is *unknown*, never assumed deleted.
//! Cross-snapshot assembly is the rule (the whole archived set folds by
//! last-version-per-record in global sequence order), which is what keeps a
//! reclaimed-then-continued session readable.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

/// The pre-increment export envelope: one whole session as one line (a
/// complete observation).
pub const SESSION_V1_SCHEMA: &str = "chat-stasher.opencode.session.v1";
/// The increment-era record envelope: one record (or tombstone) per line.
pub const SESSION_V2_SCHEMA: &str = "chat-stasher.opencode.session.v2";

/// The three record kinds a snapshot-source session folds by. Session-row
/// records mark the observation itself (they fire only on passes that
/// observed the session row change); message rows key under `message.id`
/// and parts under `part.id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecordKind {
    Session,
    Message,
    Part,
}

impl RecordKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RecordKind::Session => "session",
            RecordKind::Message => "message",
            RecordKind::Part => "part",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "session" => Some(RecordKind::Session),
            "message" => Some(RecordKind::Message),
            "part" => Some(RecordKind::Part),
            _ => None,
        }
    }
}

/// The map key of one record: `"<kind>:<id>"` — collision-proof across kinds
/// by construction.
pub fn record_key(kind: RecordKind, id: &str) -> String {
    format!("{}:{}", kind.as_str(), id)
}

/// Split a [`record_key`] back into its kind and id.
pub fn parse_record_key(key: &str) -> Option<(RecordKind, String)> {
    let (kind, id) = key.split_once(':')?;
    Some((RecordKind::from_str(kind)?, id.to_string()))
}

/// Which body a reader asked for.
///
/// [`BodyView::Current`] is the default: the source's final state, the view
/// that must equal a fresh export byte for byte at quiescence.
/// [`BodyView::IncludeDeleted`] additionally emits the deleted records at
/// their original positions, each carrying `"deleted": true`, so the
/// deletions the source performed stay reachable (they were never erased
/// from storage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyView {
    Current,
    IncludeDeleted,
}

/// A named, loud failure to assemble a session body: the reader cannot know
/// what the session holds, so it must never answer a short count (the
/// unknown is surfaced as an exit-3 read failure, never as empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssemblyError {
    /// A line in the shard set is not JSON at all.
    NotJson { why: String },
    /// A line whose schema is neither opencode envelope.
    ForeignShape { found: String },
    /// A v2 record line missing (or disagreeing about) its own identity.
    CorruptRecord { why: String },
    /// The fold ended without any session row, so there is no envelope to
    /// compose.
    MissingSession,
    /// A row is missing a column the source's own ordering rule needs.
    MissingOrder { what: &'static str },
    /// A line belongs to a different session than the one being assembled.
    SessionIdMismatch { found: String },
}

impl fmt::Display for AssemblyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AssemblyError::NotJson { why } => {
                write!(f, "a shard line is not JSON: {why}")
            }
            AssemblyError::ForeignShape { found } => {
                write!(f, "a line carries a foreign schema: {found}")
            }
            AssemblyError::CorruptRecord { why } => {
                write!(f, "a corrupted record line: {why}")
            }
            AssemblyError::MissingSession => {
                write!(
                    f,
                    "the fold holds no session row, so no envelope can be composed"
                )
            }
            AssemblyError::MissingOrder { what } => {
                write!(f, "a row is missing the ordering column `{what}`")
            }
            AssemblyError::SessionIdMismatch { found } => {
                write!(
                    f,
                    "a line names session `{found}`, not the session being assembled"
                )
            }
        }
    }
}

impl std::error::Error for AssemblyError {}

/// SHA-256 of a record's own payload serialization. The payload embeds the
/// record's id, so this digest identifies *both* the record's identity and
/// its content — the delivery identity the collector's map is keyed by.
pub fn payload_digest(value: &Value) -> String {
    // reason: serde_json can only fail on non-string object keys, which
    // sqlite-derived values never produce; the empty digest such a can-only-
    // be-a-bug path yields matches no real payload hash, so the safe
    // direction everywhere a digest is compared is re-delivery.
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Compose the v1 whole-session envelope — the one serializer the fresh
/// export, the assembled default view and the deleted-included view all
/// share, which is what makes "assembled equals fresh export" a byte
/// contract rather than a structural one.
///
/// `messages` are message rows **without** parts (the record form); parts
/// attach per the v1 export's own rule: grouped by `message_id`, each
/// message receives its parts in `(time_created, id)` order and `[]` when
/// it has none; the remainder become `orphan_parts` in `message_id` order.
/// The order of the `messages` array and the `part_rows` sequence is
/// preserved exactly as given — callers pass the source's own
/// (`time_created, id`) / (`message_id, time_created, id`) orders.
pub fn compose_v1_line(
    session: &Value,
    messages: &[Value],
    part_rows: &[Value],
) -> Result<Vec<u8>, AssemblyError> {
    let mut parts_by_message: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for part in part_rows {
        let message_id =
            part.get("message_id")
                .and_then(Value::as_str)
                .ok_or(AssemblyError::MissingOrder {
                    what: "part.message_id",
                })?;
        parts_by_message
            .entry(message_id.to_string())
            .or_default()
            .push(part.clone());
    }
    let mut composed_messages = Vec::with_capacity(messages.len());
    for message in messages {
        let mut object = match message {
            Value::Object(object) => object.clone(),
            other => {
                return Err(AssemblyError::CorruptRecord {
                    why: format!("a message row is not a JSON object: {other}"),
                })
            }
        };
        let id = message
            .get("id")
            .and_then(Value::as_str)
            .ok_or(AssemblyError::MissingOrder { what: "message.id" })?
            .to_string();
        // reason: when a message has no corresponding entries in the part
        // table, the parts array itself is empty list [] — the v1 export's
        // own rule, reproduced by the composer the export shares.
        let parts = parts_by_message.remove(&id).unwrap_or_default();
        object.insert("parts".to_string(), Value::Array(parts));
        composed_messages.push(Value::Object(object));
    }
    let orphan_parts: Vec<Value> = parts_by_message.into_values().flatten().collect();
    let envelope = json!({
        "schema": SESSION_V1_SCHEMA,
        "session": session,
        "messages": composed_messages,
        "orphan_parts": orphan_parts,
    });
    // reason: the envelope is a serde_json Value whose inputs (sqlite rows)
    // carry only string keys, so serde cannot fail here; an Err on this line
    // would be a can-only-be-a-bug path, and the byte contract's twin
    // (a fresh export composed of the very same values) would mismatch it
    // loudly rather than let silence pass.
    Ok(serde_json::to_vec(&envelope).unwrap_or_default())
}

/// One v2 record line (no trailing newline): the record's self-describing
/// envelope carrying its payload and the payload's SHA-256.
pub fn record_line(
    native_session: &str,
    kind: RecordKind,
    record_id: &str,
    payload: &Value,
) -> Vec<u8> {
    let digest = payload_digest(payload);
    let line = json!({
        "schema": SESSION_V2_SCHEMA,
        "session": native_session,
        "kind": kind.as_str(),
        "record_id": record_id,
        "deleted": false,
        "record_sha256": digest,
        "payload": payload,
    });
    // reason: a Value built from sqlite-derived rows cannot fail to
    // serialize (string keys only), and the empty line that would result
    // from the can-only-be-a-bug path is refused upstream — the fold
    // rejects a non-JSON line loudly.
    serde_json::to_vec(&line).unwrap_or_default()
}

/// One v2 tombstone line (no trailing newline). Tombstones carry no payload:
/// they mask, and the payload's bytes stay in the records that delivered
/// them.
pub fn tombstone_line(native_session: &str, kind: RecordKind, record_id: &str) -> Vec<u8> {
    let line = json!({
        "schema": SESSION_V2_SCHEMA,
        "session": native_session,
        "kind": kind.as_str(),
        "record_id": record_id,
        "deleted": true,
    });
    // reason: a fixed object with string keys cannot fail to serialize; an
    // empty line from this can-only-be-a-bug path is refused loudly by the
    // fold's JSON parse.
    serde_json::to_vec(&line).unwrap_or_default()
}

/// The fold's one record state: last-version-wins *with* the mask-never-erase
/// property — a tombstone keeps the payload it masks so the deleted view can
/// still emit the record at its original position.
#[derive(Debug, Clone)]
struct FoldedRecord {
    payload: Option<Value>,
    deleted: bool,
}

/// The last-version-per-record fold of one session's whole shard set.
///
/// Feed it every shard body in global sequence order (whatever order the
/// reader holds), then compose. The fold works across v1 whole-file lines
/// and v2 increment records mixed together, which is what makes the
/// existing stock readable without conversion (mixed-era sessions are
/// permanent furniture).
#[derive(Debug, Default)]
pub struct Assembly {
    native_session: Option<String>,
    records: BTreeMap<(RecordKind, String), FoldedRecord>,
}

impl Assembly {
    /// Fold for a known native session id: every line's self-declared session
    /// must agree with it, so a session directory that holds a foreign
    /// session's line is a loud failure, never a mixup.
    pub fn for_session(native_session: &str) -> Self {
        Self {
            native_session: Some(native_session.to_string()),
            records: BTreeMap::new(),
        }
    }

    /// Fold without a session assertion (used by the collect-side map
    /// rebuild, where the directory identity already pins the session).
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Fold one shard's body: every non-blank line contributes its records
    /// in order.
    pub fn feed_shard_body(&mut self, body: &[u8]) -> Result<(), AssemblyError> {
        for line in String::from_utf8_lossy(body).lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            self.feed_line(trimmed)?;
        }
        Ok(())
    }

    pub fn feed_line(&mut self, line: &str) -> Result<(), AssemblyError> {
        let value: Value = serde_json::from_str(line).map_err(|error| AssemblyError::NotJson {
            why: error.to_string(),
        })?;
        match value.get("schema").and_then(Value::as_str) {
            Some(SESSION_V1_SCHEMA) => self.feed_v1_observation(&value),
            Some(SESSION_V2_SCHEMA) => self.feed_v2_record(&value),
            other => Err(AssemblyError::ForeignShape {
                found: other.unwrap_or("<missing>").to_string(),
            }),
        }
    }

    fn check_session(&self, found: &str) -> Result<(), AssemblyError> {
        match &self.native_session {
            Some(expected) if expected != found => Err(AssemblyError::SessionIdMismatch {
                found: found.to_string(),
            }),
            _ => Ok(()),
        }
    }

    /// One whole-file v1 line is **one complete observation** (ADR-061 B2):
    /// every record id the fold knows that this observation does not carry
    /// is deleted as of this observation — an implicit tombstone. The
    /// observation then overwrites its own records (resurrections win).
    fn feed_v1_observation(&mut self, value: &Value) -> Result<(), AssemblyError> {
        let session_row = value
            .get("session")
            .ok_or_else(|| AssemblyError::CorruptRecord {
                why: "a v1 observation carries no session row".to_string(),
            })?
            .clone();
        let session_id = session_row
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| AssemblyError::CorruptRecord {
                why: "the session row has no id".to_string(),
            })?
            .to_string();
        self.check_session(&session_id)?;

        let mut observation: BTreeMap<(RecordKind, String), Value> = BTreeMap::new();
        observation.insert((RecordKind::Session, session_id.clone()), session_row);
        let empty = Vec::new();
        let messages = value
            .get("messages")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        for message in messages {
            let id = message
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| AssemblyError::CorruptRecord {
                    why: "a message row has no id".to_string(),
                })?
                .to_string();
            self.check_message_native(&id, &session_id)?;
            let payload = strip_parts(message);
            observation.insert((RecordKind::Message, id.clone()), payload);
            let parts = message.get("parts").and_then(Value::as_array);
            for part in parts.into_iter().flatten() {
                let part_id = part
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AssemblyError::CorruptRecord {
                        why: "a part row has no id".to_string(),
                    })?
                    .to_string();
                observation.insert((RecordKind::Part, part_id), part.clone());
            }
        }
        let orphans = value
            .get("orphan_parts")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        for part in orphans {
            let part_id = part
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| AssemblyError::CorruptRecord {
                    why: "an orphan part row has no id".to_string(),
                })?
                .to_string();
            observation.insert((RecordKind::Part, part_id), part.clone());
        }

        // Implicit tombstones: everything the fold holds that this complete
        // observation no longer carries is deleted as of now. The payload is
        // kept — a tombstone masks, it never erases.
        for (key, record) in self.records.iter_mut() {
            if !observation.contains_key(key) {
                record.deleted = true;
            }
        }
        for (key, payload) in observation {
            self.records.insert(
                key,
                FoldedRecord {
                    payload: Some(payload),
                    deleted: false,
                },
            );
        }
        Ok(())
    }

    fn check_message_native(
        &self,
        message_id: &str,
        session_id: &str,
    ) -> Result<(), AssemblyError> {
        // opencode message ids are `<session>-msg…` shaped in our fixtures,
        // but the source makes no such promise; the only invariant is that a
        // message row belongs to the session the envelope names. Assertion
        // hook kept trivial to stay honest about that.
        let _ = (message_id, session_id);
        Ok(())
    }

    fn feed_v2_record(&mut self, value: &Value) -> Result<(), AssemblyError> {
        let native = value
            .get("session")
            .and_then(Value::as_str)
            .ok_or_else(|| AssemblyError::CorruptRecord {
                why: "a record line carries no session id".to_string(),
            })?
            .to_string();
        self.check_session(&native)?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .and_then(RecordKind::from_str)
            .ok_or_else(|| AssemblyError::CorruptRecord {
                why: "a record line carries no known kind".to_string(),
            })?;
        let record_id = value
            .get("record_id")
            .and_then(Value::as_str)
            .ok_or_else(|| AssemblyError::CorruptRecord {
                why: "a record line carries no record id".to_string(),
            })?
            .to_string();
        let deleted = value
            .get("deleted")
            .and_then(Value::as_bool)
            // reason: a record line that does not spell `deleted` carries
            // the pre-existing record shape from before tombstones existed,
            // and the only honest reading of such a line is a present
            // record — missing has never meant deleted in this archive.
            .unwrap_or(false);
        let key = (kind, record_id);
        if deleted {
            // Mask, never erase: keep whatever payload earlier records
            // delivered so the deleted view can still emit it.
            let entry = self.records.entry(key).or_insert(FoldedRecord {
                payload: None,
                deleted: true,
            });
            entry.deleted = true;
            return Ok(());
        }
        let payload = value
            .get("payload")
            .filter(|payload| !payload.is_null())
            .ok_or_else(|| AssemblyError::CorruptRecord {
                why: "a non-deleted record line carries no payload".to_string(),
            })?;
        // The record is independently verifiable: its declared hash must be
        // the hash of the payload it carries (ADR-012's family rule — each
        // record's contribution is computable from the record itself).
        if let Some(declared) = value.get("record_sha256").and_then(Value::as_str) {
            if declared != payload_digest(payload) {
                return Err(AssemblyError::CorruptRecord {
                    why: "the record's declared hash does not match its payload".to_string(),
                });
            }
        }
        self.records.insert(
            key,
            FoldedRecord {
                payload: Some(payload.clone()),
                deleted: false,
            },
        );
        Ok(())
    }

    /// The live record map: `{record key → payload digest}` of every record
    /// whose last version is present (tombstoned records are not live — a
    /// re-observed previously-tombstoned record is a resurrection and
    /// delivers as data, D4). This is the map the collector keeps and the
    /// stage rebuilds.
    pub fn live_map(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for ((kind, id), record) in &self.records {
            if record.deleted {
                continue;
            }
            if let Some(payload) = &record.payload {
                out.insert(record_key(*kind, id), payload_digest(payload));
            }
        }
        out
    }

    /// Compose the requested view as one framed v1 line (line + `\n`, the
    /// exact shape a sealed single-line shard holds).
    pub fn compose(&self, view: BodyView) -> Result<Vec<u8>, AssemblyError> {
        let session_value = self
            .records
            .iter()
            .find(|((kind, _), _)| *kind == RecordKind::Session)
            .and_then(|(_, record)| record.payload.clone())
            .ok_or(AssemblyError::MissingSession)?;

        // Message selection: the default view is the source's final state
        // (live records only); the deleted-included view also emits
        // tombstoned messages at their original positions. Ordering is the
        // source's own — (time_created, id), recomputed at assembly, never
        // arrival order.
        let mut messages = self.select_rows(RecordKind::Message, view)?;
        messages.sort_by_key(|row| {
            (
                row.payload
                    .get("time_created")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MIN),
                row.id.clone(),
            )
        });

        // Parts sort by the source's own attachment columns (message_id,
        // time_created, id) — the same order the fresh export attaches in,
        // which keeps orphan ordering for free: a filtered subsequence of a
        // (message_id, time_created, id)-sorted list is still sorted by it.
        let mut parts = self.select_rows(RecordKind::Part, view)?;
        parts.sort_by_key(|row| {
            (
                row.payload
                    .get("message_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                row.payload
                    .get("time_created")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MIN),
                row.id.clone(),
            )
        });

        // Attachment follows the v1 export's own rule. In the default view
        // a live part whose owning message is gone joins `orphan_parts` —
        // exactly where a fresh export would put it, which is what keeps the
        // byte contract honest. In the deleted view tombstoned messages are
        // part of the emitted set, so their parts attach to them.
        let message_ids: std::collections::BTreeSet<&str> =
            messages.iter().map(|row| row.id.as_str()).collect();
        let mut attachments: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut orphan_values: Vec<Value> = Vec::new();
        for row in parts {
            let message_id = row
                .payload
                .get("message_id")
                .and_then(Value::as_str)
                .ok_or(AssemblyError::MissingOrder {
                    what: "part.message_id",
                })?
                .to_string();
            let emitted = row.emitted(view);
            if message_ids.contains(message_id.as_str()) {
                attachments.entry(message_id).or_default().push(emitted);
            } else {
                orphan_values.push(emitted);
            }
        }

        let mut composed_messages = Vec::with_capacity(messages.len());
        for row in messages {
            let mut emitted = row.emitted(view);
            // reason: a message whose parts were never selected (every part
            // tombstoned) emits `[]`, the same place the v1 export puts a
            // message with no part rows.
            let attached = attachments.remove(&row.id).unwrap_or_default();
            if let Some(object) = emitted.as_object_mut() {
                object.insert("parts".to_string(), Value::Array(attached));
            }
            composed_messages.push(emitted);
        }

        let envelope = json!({
            "schema": SESSION_V1_SCHEMA,
            "session": session_value,
            "messages": composed_messages,
            "orphan_parts": orphan_values,
        });
        // reason: the envelope is a serde_json Value built by this function
        // from values that already serialized once on the write side, and
        // serde only fails on non-string object keys, which none of its
        // inputs can carry; an empty composed line would mismatch every
        // reader's byte comparison loudly rather than pass as silence.
        let mut line = serde_json::to_vec(&envelope).unwrap_or_default();
        line.push(b'\n');
        Ok(line)
    }

    /// The rows of one kind for one view: (id, deleted, payload). A live row
    /// without a payload is a corruption of the fold's own invariants, so it
    /// is loud, never skipped.
    fn select_rows(
        &self,
        kind: RecordKind,
        view: BodyView,
    ) -> Result<Vec<SelectedRow>, AssemblyError> {
        let mut rows = Vec::new();
        for ((found_kind, id), record) in &self.records {
            if *found_kind != kind {
                continue;
            }
            if view != BodyView::IncludeDeleted && record.deleted {
                continue;
            }
            let Some(payload) = &record.payload else {
                return Err(AssemblyError::CorruptRecord {
                    why: format!("record `{id}` is emitted without a payload"),
                });
            };
            rows.push(SelectedRow {
                id: id.clone(),
                deleted: record.deleted,
                payload: payload.clone(),
            });
        }
        Ok(rows)
    }
}

/// One selected record's emission inputs (see [`Assembly::compose`]).
struct SelectedRow {
    id: String,
    deleted: bool,
    payload: Value,
}

impl SelectedRow {
    /// The JSON value to emit for one record in one view: the payload, with
    /// `"deleted": true` added when the view includes deleted records and
    /// this one is deleted.
    fn emitted(&self, view: BodyView) -> Value {
        if view == BodyView::IncludeDeleted && self.deleted {
            // reason: a deleted row only reaches emission through
            // `select_rows`, which refuses a missing payload outright, so a
            // non-object payload here cannot occur; the empty object keeps
            // the marker emission total instead of inventing content.
            let mut object = self.payload.as_object().cloned().unwrap_or_default();
            object.insert("deleted".to_string(), Value::Bool(true));
            Value::Object(object)
        } else {
            self.payload.clone()
        }
    }
}

fn strip_parts(message: &Value) -> Value {
    match message {
        Value::Object(object) => {
            let mut stripped = object.clone();
            stripped.remove("parts");
            Value::Object(stripped)
        }
        other => other.clone(),
    }
}

/// Does this session id belong to the opencode snapshot source? Only opencode
/// sessions assemble in this module (D5: opencode first); every other
/// harness keeps its raw-concat growth-log body.
pub fn harness_is_opencode(session_id: &str) -> bool {
    crate::sidecar::infer_harness(session_id).as_deref() == Some("opencode")
}

/// The native opencode session id (the part after the second dot of
/// `opencode.<machine>.<sid>`).
pub fn native_session_id(session_id: &str) -> Option<&str> {
    session_id.splitn(3, '.').nth(2)
}

/// Assemble one session's body from its shard bodies (in global sequence
/// order) into the requested view. Shards may mix v1 whole-file lines and
/// v2 increment records freely.
pub fn assemble_session_body(
    session_id: &str,
    bodies: &[Vec<u8>],
    view: BodyView,
) -> Result<Vec<u8>, AssemblyError> {
    let native = native_session_id(session_id).unwrap_or(session_id);
    let mut assembly = Assembly::for_session(native);
    for body in bodies {
        assembly.feed_shard_body(body)?;
    }
    assembly.compose(view)
}

/// Which stored-line family a session body belongs to. The snapshot-source
/// projection only applies to bodies whose lines are opencode session
/// records; the inbox seal path stores `chat-stasher/inbox@*` bundles under
/// `opencode.*` session ids verbatim (W213: the bundle is the identity, and
/// `read` must return exactly the sealed bytes), and those stay raw until
/// the extension-ingest follow-on converts them at ingest (D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFamily {
    /// Every first line names an opencode session envelope: a snapshot
    /// source's stored observations, assembled strictly below.
    SnapshotRecords,
    /// An inbox capture bundle, or any body this build does not know: the
    /// bytes pass through untouched.
    Other,
}

/// Classify by the first non-blank JSON line's `schema` — a byte-level
/// question, never an id-shape guess (an inbox `sessionId` may legally
/// contain dots, so the id's segment count cannot separate the families).
pub fn classify_session_body(bodies: &[Vec<u8>]) -> BodyFamily {
    for body in bodies {
        for line in String::from_utf8_lossy(body).lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let family = serde_json::from_str::<Value>(trimmed)
                .ok()
                .and_then(|value| {
                    value
                        .get("schema")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .filter(|schema| schema == SESSION_V1_SCHEMA || schema == SESSION_V2_SCHEMA)
                .map(|_| BodyFamily::SnapshotRecords)
                .unwrap_or(BodyFamily::Other);
            return family;
        }
    }
    BodyFamily::Other
}

/// Project one stored body according to D3: a snapshot-source record set
/// (any era mix) assembles into the requested view — strictly, so a fold
/// that cannot complete is a loud error, never a short body — while every
/// other body returns byte for byte as stored (see [`BodyFamily`]).
pub fn project_session_body(
    session_id: &str,
    bodies: &[Vec<u8>],
    view: BodyView,
) -> Result<Vec<u8>, AssemblyError> {
    if classify_session_body(bodies) == BodyFamily::SnapshotRecords {
        assemble_session_body(session_id, bodies, view)
    } else {
        Ok(bodies.iter().flatten().copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_row(id: &str, updated: i64) -> Value {
        json!({"id": id, "time_created": 1000, "time_updated": updated, "title": "fixture"})
    }

    fn message_row(id: &str, role: &str, time_created: i64, parts: Vec<Value>) -> Value {
        json!({
            "id": id,
            "session_id": "ses",
            "time_created": time_created,
            "time_updated": time_created,
            "data": {"role": role},
            "parts": parts,
        })
    }

    fn part_row(id: &str, message_id: &str, time_created: i64, text: &str) -> Value {
        json!({
            "id": id,
            "session_id": "ses",
            "message_id": message_id,
            "time_created": time_created,
            "time_updated": time_created,
            "data": {"type": "text", "text": text},
        })
    }

    fn v1_line(session: &Value, messages: &[Value], orphans: &[Value]) -> Vec<u8> {
        let mut line = json!({
            "schema": SESSION_V1_SCHEMA,
            "session": session,
            "messages": messages,
            "orphan_parts": orphans,
        });
        let line = serde_json::to_vec(&line).unwrap();
        line
    }

    fn frame(mut line: Vec<u8>) -> Vec<Vec<u8>> {
        line.push(b'\n');
        vec![line]
    }

    /// A delete-free single-observation session composes back to itself: for
    /// the newest copy, the assembled default view *is* the fresh export.
    #[test]
    fn a_single_v1_observation_assembles_to_itself() {
        let session = session_row("ses", 1_000);
        let m1 = message_row(
            "m1",
            "user",
            2_000,
            vec![part_row("p1", "m1", 2_100, "one")],
        );
        let fresh = compose_v1_line(
            &session,
            &[m1.clone()],
            &[part_row("p1", "m1", 2_100, "one")],
        )
        .unwrap();
        let mut assembly = Assembly::for_session("ses");
        assembly
            .feed_shard_body(&frame(v1_line(&session, &[m1], &[]))[0])
            .unwrap();
        let composed = assembly.compose(BodyView::Current).unwrap();
        // compose frames with one trailing newline; the fresh export does
        // not, so compare the unframed bytes.
        assert_eq!(&composed[..composed.len() - 1], &fresh[..]);
    }

    /// Two successive v1 observations and the newer one omits a message: the
    /// older message is implicitly tombstoned, so the default view equals
    /// the newer observation alone (B2's core rule, whole-file era).
    #[test]
    fn an_omitted_message_in_a_newer_whole_file_observation_is_implicitly_deleted() {
        let session = session_row("ses", 1_000);
        let m1 = message_row(
            "m1",
            "user",
            2_000,
            vec![part_row("p1", "m1", 2_100, "one")],
        );
        let m2 = message_row(
            "m2",
            "assistant",
            2_200,
            vec![part_row("p2", "m2", 2_300, "two")],
        );

        let mut updated_session = session_row("ses", 1_500);
        updated_session["title"] = json!("compacted");
        let fresh = compose_v1_line(
            &updated_session,
            &[message_row("m2", "assistant", 2_200, vec![])],
            &[part_row("p2", "m2", 2_300, "two")],
        )
        .unwrap();

        let mut assembly = Assembly::for_session("ses");
        assembly
            .feed_shard_body(&frame(v1_line(&session, &[m1.clone(), m2.clone()], &[]))[0])
            .unwrap();
        assembly
            .feed_shard_body(&frame(v1_line(&updated_session, &[m2], &[]))[0])
            .unwrap();
        let view = assembly.compose(BodyView::Current).unwrap();
        // The trailing byte of compose is the frame; the fresh export has
        // none, so compare accordingly.
        assert_eq!(
            &view[..view.len() - 1],
            &fresh[..],
            "the default view is the newest observation"
        );

        // The deleted view still holds m1 at its original position, marked.
        let deleted = assembly.compose(BodyView::IncludeDeleted).unwrap();
        let parsed: Value = serde_json::from_slice(&deleted[..deleted.len() - 1]).unwrap();
        let messages = parsed["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "m1 reappears in the deleted view");
        assert_eq!(messages[0]["id"], "m1");
        assert_eq!(
            messages[0]["deleted"], true,
            "m1 carries the deletion marker"
        );
        assert_eq!(
            messages[0]["parts"][0]["deleted"], true,
            "its part is marked too"
        );
        assert_eq!(messages[1]["id"], "m2");
        assert!(
            messages[1].get("deleted").is_none(),
            "m2 is live and unmarked"
        );
    }

    /// The increment era: a new part record, a message record, an explicit
    /// tombstone and a resurrection all fold correctly beside v1 lines.
    #[test]
    fn v2_records_merge_with_v1_stock_and_tombstones() {
        let session = session_row("ses", 1_000);
        let m1 = message_row(
            "m1",
            "user",
            2_000,
            vec![part_row("p1", "m1", 2_100, "one")],
        );
        let m2 = message_row("m2", "assistant", 2_200, vec![]);

        let mut assembly = Assembly::for_session("ses");
        assembly
            .feed_shard_body(&frame(v1_line(&session, &[m1.clone(), m2.clone()], &[]))[0])
            .unwrap();

        // Pass 2 (increments): a new message m3 with part p2, and m2's
        // tombstone because the source reverted it.
        let session_updated = session_row("ses", 1_700);
        let mut lines: Vec<u8> = Vec::new();
        for bytes in [
            record_line("ses", RecordKind::Session, "ses", &session_updated),
            record_line(
                "ses",
                RecordKind::Message,
                "m3",
                &message_row("m3", "user", 2_500, vec![]),
            ),
            record_line(
                "ses",
                RecordKind::Part,
                "p2",
                &part_row("p2", "m3", 2_600, "three"),
            ),
            tombstone_line("ses", RecordKind::Message, "m2"),
        ] {
            lines.extend_from_slice(&bytes);
            lines.push(b'\n');
        }
        assembly.feed_shard_body(&lines).unwrap();

        let fresh = compose_v1_line(
            &session_updated,
            &[strip_parts(&m1), message_row("m3", "user", 2_500, vec![])],
            &[
                part_row("p1", "m1", 2_100, "one"),
                part_row("p2", "m3", 2_600, "three"),
            ],
        )
        .unwrap();

        // Default view: m1 (with p1 attached), m3 (with p2 attached), no m2.
        let view = assembly.compose(BodyView::Current).unwrap();
        let parsed: Value = serde_json::from_slice(&view[..view.len() - 1]).unwrap();
        let ids: Vec<&str> = parsed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["m1", "m3"], "m2 is masked out of the default view");

        // Deleted view: m2 is back, between m1 and m3, marked deleted.
        let deleted = assembly.compose(BodyView::IncludeDeleted).unwrap();
        let parsed: Value = serde_json::from_slice(&deleted[..deleted.len() - 1]).unwrap();
        let ids: Vec<&str> = parsed["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["m1", "m2", "m3"], "m2 sits at its original position");
        assert_eq!(parsed["messages"][1]["deleted"], true);

        // The fresh export of the same final state equals the default view.
        let view_now = assembly.compose(BodyView::Current).unwrap();
        // compose includes the trailing frame byte; the fresh export does not.
        assert_eq!(&view_now[..view_now.len() - 1], &fresh[..]);
    }

    /// A resurrection after an explicit tombstone delivers as data: the
    /// record wins over its own tombstone by being newer.
    #[test]
    fn a_record_resent_after_its_tombstone_is_a_resurrection() {
        let session = session_row("ses", 1_000);
        let m1 = message_row(
            "m1",
            "user",
            2_000,
            vec![part_row("p1", "m1", 2_100, "one")],
        );
        let m1_row = strip_parts(&m1);

        let mut assembly = Assembly::for_session("ses");
        assembly
            .feed_shard_body(&frame(v1_line(&session, &[m1.clone()], &[]))[0])
            .unwrap();
        let mut burial = Vec::new();
        burial.extend_from_slice(&tombstone_line("ses", RecordKind::Message, "m1"));
        burial.push(b'\n');
        assembly.feed_shard_body(&burial).unwrap();
        assert!(
            assembly.live_map().get("message:m1").is_none(),
            "the tombstone removed m1 from the live map"
        );

        let resurrection = m1_row.clone();
        let mut lines = record_line("ses", RecordKind::Message, "m1", &resurrection);
        lines.push(b'\n');
        assembly.feed_shard_body(&lines).unwrap();
        assert!(
            assembly.live_map().contains_key("message:m1"),
            "the resurrection delivers as data"
        );
        let view = assembly.compose(BodyView::Current).unwrap();
        let parsed: Value = serde_json::from_slice(&view[..view.len() - 1]).unwrap();
        assert_eq!(parsed["messages"].as_array().unwrap().len(), 1);
        assert!(
            parsed["messages"][0].get("deleted").is_none(),
            "the resurrected record is live and carries no marker"
        );
    }

    /// Orphan parts keep the v1 export's own order (message_id, then the
    /// part's (time_created, id)); orphans marked deleted keep it too.
    #[test]
    fn orphan_parts_order_follows_the_export_rule() {
        let session = session_row("ses", 1_000);
        // p_b belongs to a missing message "b", p_a to missing message "a":
        // the orphan array is ordered by message_id first.
        let orphans = vec![
            part_row("p_b", "msg_b", 2_100, "b"),
            part_row("p_a", "msg_a", 2_300, "a"),
        ];
        let mut assembly = Assembly::for_session("ses");
        assembly
            .feed_shard_body(&frame(v1_line(&session, &[], &orphans))[0])
            .unwrap();
        let view = assembly.compose(BodyView::Current).unwrap();
        let parsed: Value = serde_json::from_slice(&view[..view.len() - 1]).unwrap();
        let ids: Vec<&str> = parsed["orphan_parts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|part| part["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["p_a", "p_b"]);
    }

    /// A corrupted shard line is a loud assembly failure, never a silent
    /// short read.
    #[test]
    fn a_foreign_line_fails_assembly_loudly() {
        let mut assembly = Assembly::for_session("ses");
        let garbage = b"{\"schema\":\"chat-stasher.foreign\",\"x\":1}\n";
        let error = assembly.feed_shard_body(garbage).unwrap_err();
        assert!(matches!(error, AssemblyError::ForeignShape { .. }));
        let garbage_text = b"not json at all\n";
        assert!(matches!(
            assembly.feed_shard_body(garbage_text).unwrap_err(),
            AssemblyError::NotJson { .. }
        ));
    }

    /// A record whose declared hash disagrees with its payload is a print
    /// failure: each record verifies independently.
    #[test]
    fn a_record_with_a_wrong_declared_hash_fails_the_fold() {
        let bad = json!({
            "schema": SESSION_V2_SCHEMA,
            "session": "ses",
            "kind": "message",
            "record_id": "m1",
            "deleted": false,
            "record_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
            "payload": message_row("m1", "user", 2_000, vec![]),
        });
        let mut assembly = Assembly::for_session("ses");
        let line = serde_json::to_vec(&bad).unwrap();
        assert!(matches!(
            assembly.feed_shard_body(&line).unwrap_err(),
            AssemblyError::CorruptRecord { .. }
        ));
    }

    /// An inbox capture bundle under an `opencode.*` id is not a snapshot
    /// record set: it classifies as byte-verbatim (W213) and projects to
    /// exactly the stored bytes, while the same id directory holding session
    /// records still assembles strictly.
    #[test]
    fn an_inbox_bundle_under_an_opencode_id_stays_verbatim() {
        let bundle = json!({
            "schema": "chat-stasher/inbox@3",
            "kind": "harness-file",
            "harness": "opencode",
            "nativeSessionId": "synthetic-native",
            "file": {"role": "transcript", "relPath": "synthetic.jsonl"},
            "raw": {"encoding": "utf-8", "data": "synthetic body"},
        });
        let mut line = serde_json::to_vec(&bundle).unwrap();
        line.push(b'\n');
        let bodies = vec![line.clone()];
        assert_eq!(classify_session_body(&bodies), BodyFamily::Other);
        let projected = project_session_body("opencode~xx", &bodies, BodyView::Current)
            .expect("verbatim is never an error");
        assert_eq!(projected, line, "the bundle passes through byte for byte");

        // The snapshot family is unaffected: a v1 line still assembles.
        let session = session_row("ses", 1_000);
        let v1 = frame(
            json!({
                "schema": SESSION_V1_SCHEMA,
                "session": session,
                "messages": [],
                "orphan_parts": [],
            })
            .to_string()
            .into_bytes(),
        );
        assert_eq!(classify_session_body(&v1), BodyFamily::SnapshotRecords);
        assert!(project_session_body("opencode.m.ses", &v1, BodyView::Current).is_ok());
    }

    /// A line that names another session never folds into this one's body.
    #[test]
    fn a_line_of_a_foreign_session_is_refused() {
        let mut assembly = Assembly::for_session("ses");
        let foreign = record_line(
            "other",
            RecordKind::Message,
            "m1",
            &message_row("m1", "user", 2_000, vec![]),
        );
        assert!(matches!(
            assembly.feed_shard_body(&foreign).unwrap_err(),
            AssemblyError::SessionIdMismatch { .. }
        ));
    }
}
