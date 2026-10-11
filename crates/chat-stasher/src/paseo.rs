//! The Paseo agent observation contract (ADR-056 D5).
//!
//! One row says: *this daemon, this agent, recorded these facts about itself at
//! `observed_at`, and here is what the archive can prove about the native body
//! it points at*. It is the only new stored contract this work adds, and it is
//! deliberately not a transcript, not a second body identity, and not a live
//! status: [`AgentObservationRow`] carries the association and its proof, while
//! the native readers keep ownership of the bytes
//! (`crate::normalize`, `crate::readback`).
//!
//! What this module holds today is the contract and its validator. The scanner
//! integration, the read-only resolvers and the append to
//! `meta/<machine>/paseo-agents-v1.jsonl` are separate steps of the same ADR and
//! are not here; nothing in this file touches the filesystem, so the shape and
//! its rules can be settled and tested before anything writes a row.
//!
//! Three rules the types are shaped around:
//!
//! * **An absent field is a recorded absence, never a claim.** Every optional
//!   projection field is `Option`, and an empty string is rejected rather than
//!   read as "nothing recorded" — otherwise a source that wrote `""` and a
//!   source that wrote nothing would look identical while meaning different
//!   things (CLAUDE.md invariant 1).
//! * **The identity is over content, not over an observation event.**
//!   [`AgentObservation::semantic_sha256`] excludes `observed_at`, so re-seeing
//!   an unchanged agent hashes to the id already in the file and appends
//!   nothing; only a change to what was recorded produces a new row.
//! * **Nothing here locates a file.** No source path, configuration value or
//!   credential has a field to live in, and the validators reject values that
//!   are shaped like locators. A recorded working directory is existing
//!   provenance (`crate::provenance`) and travels separately by design; a
//!   `cwd` field is not part of this contract.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The schema id of the row this module defines. It is part of the semantic
/// payload, so a future @2 can never collide with an @1 row.
pub const SCHEMA: &str = "chat-stasher/paseo-agent-observation@1";

/// The registry adapter this contract belongs to
/// (`harnesses[].session_indexes[].adapter`, ADR-056 D1). The registry refuses
/// any other value outright, because a declaration naming an adapter this build
/// cannot read would otherwise be reported as "nothing was found".
pub const AGENT_INDEX_ADAPTER: &str = "paseo-agent-index";

/// Longest title label the contract carries. The cap is a *bound on archived
/// label text*, and validation is the enforcement point: a longer label is a
/// rejection here, and any capping belongs to the capture step that chooses
/// what to record. Silently truncating an existing row would rewrite a fact.
pub const TITLE_MAX_CHARS: usize = 100;

/// Longest privacy-safe reason string in a non-archived body state.
pub const REASON_MAX_CHARS: usize = 200;

/// Bound on a daemon namespace key and a recorded lifecycle status.
const TOKEN_MAX_CHARS: usize = 128;

/// Bound on a provider name. Deliberately not a closed enum: ADR-056 D9 does
/// not promise a fixed provider set, and a new provider is a new label, not a
/// schema change. It is a bounded lowercase token, though, so one provider
/// cannot be archived under two spellings.
const PROVIDER_MAX_CHARS: usize = 64;

/// Bound on an identifier-shaped value. 255 is the budget
/// [`crate::id::bounded_path_component`] gives one archive path component, so an
/// id this contract accepts always fits the identity it must refer to.
const ID_MAX_CHARS: usize = 255;

/// Width of a lowercase SHA-256 hex digest.
const SHA256_HEX_CHARS: usize = 64;

/// What the source itself recorded about one agent, restricted to the fields
/// ADR-056 D5 whitelists. A field is absent when the source recorded nothing;
/// `deny_unknown_fields` is what makes the whitelist a whitelist, and it is why
/// a config value, an environment value or a credential cannot ride along just
/// because a newer source version started writing it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProjection {
    /// The workspace the source recorded the agent in, spelled as the source
    /// spelled it. A recorded identifier, not a directory: a value shaped like
    /// a path is rejected by [`AgentProjection::validate`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The agent the source recorded as this one's parent, as a complete UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    /// A label. Capped, and the only field here whose content came from a
    /// conversation's own naming, so [`AgentObservationRow::validate`] names the
    /// rule it broke without repeating the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The lifecycle status the source recorded, verbatim. Not mapped and not a
    /// closed set (ADR-056 D6): absence is not `active`, `closed` is not
    /// deleted, and a status this build has not seen is still a fact worth
    /// keeping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Lifecycle timestamps the source recorded, as RFC 3339 text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<String>,
    /// When the source recorded the agent as archived. Kept distinct from
    /// [`BodyState::Missing`]: the agent's own record saying "archived" and the
    /// archive having no body are different statements.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
}

impl AgentProjection {
    /// Nothing was recorded about this agent. This is not a claim that the
    /// source had nothing to record.
    pub fn is_empty(&self) -> bool {
        let Self {
            workspace_id,
            parent_agent_id,
            title,
            status,
            created_at,
            updated_at,
            last_activity_at,
            archived_at,
        } = self;
        [
            workspace_id,
            parent_agent_id,
            title,
            status,
            created_at,
            updated_at,
            last_activity_at,
            archived_at,
        ]
        .into_iter()
        .all(|field| field.is_none())
    }

    /// Each recorded value against its own rule. Field names only ever appear
    /// in a message; a recorded value never does.
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(value) = &self.workspace_id {
            check("workspace_id", value, is_recorded_id)?;
        }
        if let Some(value) = &self.parent_agent_id {
            check("parent_agent_id", value, is_agent_uuid)?;
        }
        if let Some(value) = &self.title {
            check("title", value, is_title_label)?;
        }
        if let Some(value) = &self.status {
            check("status", value, is_status_token)?;
        }
        for (name, value) in [
            ("created_at", &self.created_at),
            ("updated_at", &self.updated_at),
            ("last_activity_at", &self.last_activity_at),
            ("archived_at", &self.archived_at),
        ] {
            if let Some(value) = value {
                check(name, value, is_rfc3339)?;
            }
        }
        Ok(())
    }
}

/// What the archive can say about the native body the agent's recorded handle
/// refers to. Five states, kept separate because they mean different things to
/// a reader: proved present, provably not there, and three ways of *not knowing*
/// — which one could not be read, which is not a format this build reads, and
/// which candidate it would be.
///
/// Internally tagged, so the state is the first thing a reader (and a later
/// index) sees without parsing the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase", deny_unknown_fields)]
pub enum BodyState {
    /// A verified body: the canonical archive identity, the exact shard
    /// sequences that make up the body, and the digest those bytes hash to
    /// (ADR-056 D5). Only a producer that has actually verified the sealed
    /// shards may construct this.
    Archived {
        /// `<harness>.<machine>.<native-id>` as [`crate::id`] spells it — the
        /// native body identity is unchanged by Paseo existing (ADR-056 D2).
        canonical_session_id: String,
        /// Exact and strictly increasing: the body is the concatenation of
        /// exactly these shard sequences in this order.
        shard_sequences: Vec<u64>,
        /// Lowercase SHA-256 hex over those bytes.
        body_sha256: String,
    },
    /// The association was read, and no body for it exists to archive.
    Missing,
    /// A body exists and was not read. Not `Missing`.
    Unreadable { reason: String },
    /// A body exists in a format this build does not read. Not `Unreadable`.
    Unsupported { reason: String },
    /// More than one candidate, and the evidence does not choose between them.
    /// Never resolved by picking the first one.
    Ambiguous { reason: String },
}

impl BodyState {
    /// The state tag as it is written on disk.
    pub fn tag(&self) -> &'static str {
        match self {
            BodyState::Archived { .. } => "archived",
            BodyState::Missing => "missing",
            BodyState::Unreadable { .. } => "unreadable",
            BodyState::Unsupported { .. } => "unsupported",
            BodyState::Ambiguous { .. } => "ambiguous",
        }
    }

    /// `true` only for a verified body. The three unknown states are not
    /// evidence of absence, and a reader must not count them as sessions.
    pub fn is_archived(&self) -> bool {
        matches!(self, BodyState::Archived { .. })
    }

    /// Validate the payload a state carries. A state never pays for a payload,
    /// and `archived` never accepts a partial one.
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            BodyState::Archived {
                canonical_session_id,
                shard_sequences,
                body_sha256,
            } => {
                check(
                    "body.canonical_session_id",
                    canonical_session_id,
                    is_archive_identity,
                )?;
                anyhow::ensure!(
                    canonical_session_id.matches('.').count() >= 2,
                    "body.canonical_session_id is not `<harness>.<machine>.<native-id>`: it carries fewer \
                     than three dot-separated components"
                );
                anyhow::ensure!(
                    !shard_sequences.is_empty(),
                    "body.shard_sequences is empty: which shards make up a body is a measurement, so an \
                     empty list cannot stand in for \"unknown\""
                );
                anyhow::ensure!(
                    shard_sequences.iter().all(|sequence| *sequence >= 1),
                    "body.shard_sequences carries a sequence below 1: the stage numbers its first shard 1 \
                     (see `store::next_shard_seq`), so a 0 or negative entry names no shard"
                );
                anyhow::ensure!(
                    shard_sequences.windows(2).all(|pair| pair[0] < pair[1]),
                    "body.shard_sequences is not strictly increasing: the list is the exact, ordered set \
                     that makes the body, so a repeated or reordered list is either a fabricated body or \
                     two identities for one body"
                );
                check("body.body_sha256", body_sha256, is_sha256_hex)?;
                Ok(())
            }
            BodyState::Missing => Ok(()),
            state => reason_check(state.tag(), reason_of(state)),
        }
    }
}

/// What a producer observed before a row is stamped with an identity and a
/// timestamp. Serialize-only on purpose: the stored shape is
/// [`AgentObservationRow`] and this is not a second accepted on-disk format, so
/// there is exactly one spelling of one observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentObservation {
    /// Stable per-daemon namespace key. Identifies one daemon across scans
    /// without naming its home directory or its `server-id` bytes.
    pub daemon_key: String,
    /// Complete agent UUID, as the source spelled it — never a prefix.
    pub agent_id: String,
    pub provider: String,
    /// The native session id the agent's own record carried. `None` means the
    /// source recorded no handle, which is not a claim that no body exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    pub projection: AgentProjection,
    pub body: BodyState,
}

impl AgentObservation {
    /// SHA-256 over this observation's semantic content, framed with the schema
    /// id and excluding `observed_at`. Idempotent by construction: the same
    /// facts hash to the same digest however many times they are seen again, so
    /// an unchanged agent appends nothing (ADR-056 D5).
    pub fn semantic_sha256(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(canonical_json(&semantic_payload(self)).as_bytes());
        lower_hex(hasher.finalize())
    }

    /// Every rule that does not depend on when the row was stamped.
    pub fn validate(&self) -> anyhow::Result<()> {
        check("daemon_key", &self.daemon_key, is_daemon_key)?;
        check("agent_id", &self.agent_id, is_agent_uuid)?;
        check("provider", &self.provider, is_provider_token)?;
        if let Some(value) = &self.native_session_id {
            check("native_session_id", value, is_native_identity)?;
        }
        self.projection.validate()?;
        self.body.validate()
    }
}

/// One archived row: the semantic content, its identity, and when it was
/// observed. Stored append-only as one line per row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentObservationRow {
    pub schema: String,
    /// [`AgentObservation::semantic_sha256`] of this row's content.
    pub observation_id: String,
    pub daemon_key: String,
    pub agent_id: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    pub projection: AgentProjection,
    pub body: BodyState,
    /// RFC 3339. Excluded from the identity, so it is the one field that may
    /// change without producing a new row — and a reader must describe an
    /// agent as "last observed" at this instant, never as currently online
    /// (ADR-056 D6).
    pub observed_at: String,
}

impl AgentObservationRow {
    /// Stamp an observation. The identity is derived here rather than accepted
    /// from a caller, so a row cannot be filed under an id its content does not
    /// hash to.
    pub fn new(observation: AgentObservation, observed_at: impl Into<String>) -> Self {
        let observation_id = observation.semantic_sha256();
        let AgentObservation {
            daemon_key,
            agent_id,
            provider,
            native_session_id,
            projection,
            body,
        } = observation;
        Self {
            schema: SCHEMA.to_string(),
            observation_id,
            daemon_key,
            agent_id,
            provider,
            native_session_id,
            projection,
            body,
            observed_at: observed_at.into(),
        }
    }

    /// The content half of this row. Written as an exhaustive destructuring, so
    /// a future field that is not mapped into the semantic content is a compile
    /// error rather than a field that silently stops being hashed.
    fn content(&self) -> AgentObservation {
        AgentObservation {
            daemon_key: self.daemon_key.clone(),
            agent_id: self.agent_id.clone(),
            provider: self.provider.clone(),
            native_session_id: self.native_session_id.clone(),
            projection: self.projection.clone(),
            body: self.body.clone(),
        }
    }

    /// Re-compute the identity from this row's own content.
    pub fn semantic_sha256(&self) -> String {
        self.content().semantic_sha256()
    }

    /// Full validation of a row about to be written or trusted after a read.
    ///
    /// A failure is a corruption report, never a repair: the caller must stop
    /// rather than rewrite or drop the row (ADR-056 D5 "validate existing rows
    /// and fail visibly on corruption rather than replacing them").
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema == SCHEMA,
            "row schema is not {SCHEMA}: it names a contract this build does not read"
        );
        check("observation_id", &self.observation_id, is_sha256_hex)?;
        check("observed_at", &self.observed_at, is_rfc3339)?;
        self.content().validate()?;
        // The identity check runs last on purpose. Every row this rejects stays
        // rejected either way, but a row whose *content* breaks a rule has to
        // report the rule it broke: with the digest compared first, a bad shard
        // list and a bad title cap both arrive as "observation_id does not
        // match", which is true of the row and useless for finding the line that
        // went wrong in a file nobody can re-derive.
        anyhow::ensure!(
            self.observation_id == self.semantic_sha256(),
            "observation_id does not match this row's content: the row is corrupt, or it was written \
             against a different framing of {SCHEMA}. It is not rewritten, and it is not ignored."
        );
        Ok(())
    }

    /// The exact text a later append writes for this row: one JSON line, no
    /// trailing newline.
    pub fn to_line(&self) -> anyhow::Result<String> {
        let line = serde_json::to_string(self)?;
        anyhow::ensure!(
            !line.contains('\n'),
            "a row serialised to multiple lines, which would make the file unparseable per line"
        );
        Ok(line)
    }
}

/// Parse archived rows. A malformed, unknown-shaped or internally inconsistent
/// row is an error naming its line, never a skipped line: silently dropping a
/// row is how an archive starts reporting fewer agents than it holds.
///
/// The error names the line and the rule and carries a bounded, sanitised
/// fragment of the parser's message — archived values are not repeated into a
/// log (the same reason `crate::inbox` refuses to print validation errors).
pub fn parse_rows(text: &str) -> anyhow::Result<Vec<AgentObservationRow>> {
    let mut rows = Vec::new();
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: AgentObservationRow = serde_json::from_str(line).map_err(|error| {
            anyhow::anyhow!(
                "row {} is not a valid {SCHEMA}: {}",
                number + 1,
                bounded("parser message", &error.to_string())
            )
        })?;
        row.validate()
            .map_err(|error| anyhow::anyhow!("row {}: {error}", number + 1))?;
        rows.push(row);
    }
    Ok(rows)
}

/// The semantic payload: the content, plus the schema id it belongs to, and
/// nothing else.
fn semantic_payload(observation: &AgentObservation) -> serde_json::Value {
    let mut value = serde_json::to_value(observation)
        .expect("an observation is a plain data tree, so serialising it cannot fail");
    if let Some(map) = value.as_object_mut() {
        map.insert(
            "schema".to_string(),
            serde_json::Value::String(SCHEMA.to_string()),
        );
    }
    value
}

/// JSON text with every object's keys in byte order and every array left in
/// order.
///
/// Written here instead of relying on `serde_json`'s map ordering because this
/// text is a *stored identity*: `serde_json` sorts keys only while its map is a
/// `BTreeMap`, and one dependency turning on the crate's `preserve_order`
/// feature would turn it into an insertion-ordered map — the same fact would
/// then hash to a second id and every agent would append a duplicate row. Sorting
/// explicitly keeps one fact one id regardless of who else is in the build.
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: BTreeMap<&String, &serde_json::Value> = map.iter().collect();
            let fields: Vec<String> = sorted
                .into_iter()
                .map(|(key, value)| {
                    format!(
                        "{}:{}",
                        serde_json::Value::String(key.clone()),
                        canonical_json(value)
                    )
                })
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        serde_json::Value::Array(values) => {
            format!(
                "[{}]",
                values
                    .iter()
                    .map(canonical_json)
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        other => other.to_string(),
    }
}

fn lower_hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn reason_of(state: &BodyState) -> &str {
    match state {
        BodyState::Unreadable { reason }
        | BodyState::Unsupported { reason }
        | BodyState::Ambiguous { reason } => reason,
        _ => "",
    }
}

fn reason_check(tag: &str, reason: &str) -> anyhow::Result<()> {
    check(&format!("body[{tag}].reason"), reason, |value| {
        !value.is_empty() && value.chars().count() <= REASON_MAX_CHARS && !is_locator_like(value)
    })?;
    Ok(())
}

/// Run one rule, reporting the field and the rule without the value.
fn check(field: &str, value: &str, rule: impl Fn(&str) -> bool) -> anyhow::Result<()> {
    anyhow::ensure!(rule(value), "{field} does not satisfy its contract rule");
    Ok(())
}

/// Bound and flatten a fragment before it reaches an error message, so a
/// rejection can be described without quoting an archived value verbatim.
fn bounded(kind: &str, text: &str) -> String {
    const CAP: usize = 200;
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(CAP)
        .collect();
    format!("{kind} was rejected ({flat})")
}

/// A lowercase SHA-256 hex digest, and nothing else that could pass as one.
fn is_sha256_hex(value: &str) -> bool {
    value.len() == SHA256_HEX_CHARS
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// A daemon namespace key: a bounded lowercase label. Lowercase-only is not
/// cosmetics — a key that can be spelled two ways splits one daemon's agents
/// across two namespaces.
fn is_daemon_key(value: &str) -> bool {
    is_lowercase_token(value, TOKEN_MAX_CHARS, "-_")
}

fn is_provider_token(value: &str) -> bool {
    is_lowercase_token(value, PROVIDER_MAX_CHARS, "._-")
}

fn is_status_token(value: &str) -> bool {
    is_lowercase_token(value, TOKEN_MAX_CHARS, "._-")
}

fn is_lowercase_token(value: &str, max_chars: usize, extra: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= max_chars
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || extra.contains(c))
}

/// A complete UUID in the canonical lowercase spelling. A prefix is refused:
/// ADR-056 D2 keys an agent on its complete UUID precisely so that a shortened
/// id can never be silently re-expanded to the wrong agent.
fn is_agent_uuid(value: &str) -> bool {
    let bytes: Vec<char> = value.chars().collect();
    if bytes.len() != 36 {
        return false;
    }
    let hex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
    bytes.iter().enumerate().all(|(index, c)| match index {
        8 | 13 | 18 | 23 => *c == '-',
        _ => hex(*c),
    })
}

/// An identifier as [`crate::id`] can spell one: the alphabet
/// `bounded_path_component` emits (letters, digits, `.`, `_`, `-`, and the `~`
/// overflow tag) inside one archive component's byte budget. Note that a
/// separator cannot be written at all, which is what keeps a locator out of an
/// id field.
fn is_archive_component(value: &str, allow_tilde: bool) -> bool {
    !value.is_empty()
        && value.len() <= ID_MAX_CHARS
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') || (allow_tilde && c == '~')
        })
}

fn is_archive_identity(value: &str) -> bool {
    is_archive_component(value, true)
}

/// A native session id as its own harness spelled it — a Codex rollout filename
/// stem, a Grok session directory name, and anything a later harness records.
/// It is *not* required to look like a UUID: re-shaping a native id into a
/// canonical one is exactly the second identity ADR-056 D2 forbids.
fn is_native_identity(value: &str) -> bool {
    is_archive_component(value, true)
}

/// A recorded workspace identifier: same alphabet without the `~` overflow tag,
/// which only [`crate::id`] ever writes.
fn is_recorded_id(value: &str) -> bool {
    is_archive_component(value, false)
}

fn is_title_label(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= TITLE_MAX_CHARS
        && !value.chars().any(char::is_control)
}

fn is_rfc3339(value: &str) -> bool {
    !value.is_empty() && chrono::DateTime::parse_from_rfc3339(value).is_ok()
}

/// A value shaped like somewhere to read a file from: a path separator, a home
/// shortcut, an environment reference or a Windows drive prefix. Rejected
/// wherever it appears — ADR-056 D5 archives no source-file locator, and a
/// locator smuggled into an id or a reason is still a locator.
fn is_locator_like(value: &str) -> bool {
    if value.contains(|c: char| c == '/' || c == '\\' || c == '~' || c == '$') {
        return true;
    }
    if value.chars().any(char::is_control) {
        return true;
    }
    let mut prefix = value.chars();
    matches!(
        (prefix.next(), prefix.next()),
        (Some(drive), Some(':')) if drive.is_ascii_alphabetic()
    )
}
