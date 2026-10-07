//! Conversation-time extraction for the activity sidecar index.
//!
//! chat-stasher archives snapshots at **archival** time (rustic snapshot
//! time). To draw a heatmap by **when the conversation happened**, we need a
//! per-session earliest/latest *conversation* time, which only the session's
//! own lines carry. This module reads those lines (metadata-only: we extract a
//! timestamp and throw the line away) and produces one [`ActivityRow`] per
//! session, serialised as a JSONL line into
//! `<stage>/meta/<machine>/activity-v1.jsonl`.
//!
//! Besides the time, each row also carries a **label** for the session
//! (29-UI-DESIGN §2.2): the harness's own title line, else the head of the
//! first user line, capped at [`TITLE_CAP_CHARS`] characters. That label is
//! conversation-derived text by declared design — the one place the metadata
//! tier is allowed to carry any — and it is the only other thing this module
//! keeps: everything about a line beyond its timestamp and, for harnesses
//! with a title reader, that one candidate label is read and thrown away.
//!
//! The hard rule of this module: a time we cannot get is [`TimeSource::Unknown`]
//! with an explicit `why`. We never fabricate `0`, never use "now", and never
//! substitute the file's mtime. (This repo already paid for "0 as both sentinel
//! and valid value" once — see `inbox.rs` `modified_ns`.) And "this line could
//! not be parsed" is a different `why` from "this harness never records a time".
//! The same rule governs the label: content read with nothing label-able in it
//! records [`SessionTitle::NoLabelRecorded`] — an honest "no label recorded",
//! never an empty string and never a guess — and a harness whose lines this
//! module does not read a title from records the same `NoLabelRecorded`, by
//! design, rather than pretending to have looked.
//!
//! Timestamp shapes handled:
//!   * RFC 3339 string (`2025-01-15T12:34:56.789Z`) — unambiguous, [`TimeSource::Exact`],
//!   * numeric epoch, seconds or milliseconds — unit is *inferred* from the
//!     value's magnitude against a plausible window (2020–2100),
//!     [`TimeSource::Inferred`] so the guess is on record,
//!   * anything out of that window / unparseable — treated as suspicious, never
//!     silently clamped; a session whose only timestamps are suspicious is
//!     [`TimeSource::Unknown`].

use serde::{Deserialize, Serialize};

/// One activity-index row for a single archived session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityRow {
    pub session_id: String,
    pub machine: String,
    pub harness: String,
    pub first_unix: Option<i64>,
    pub last_unix: Option<i64>,
    pub line_count: u64,
    pub time_source: TimeSource,
    /// The time zone the source expressed this session's timestamps in, when
    /// the source actually said one. Nothing is ever read as UTC by assumption:
    /// a numeric epoch is an absolute instant whose own zone is `"UTC"`, a
    /// naive date-time string would carry its declared zone here, and an
    /// offset-bearing RFC 3339 string keeps its offset label. `None` means the
    /// harness reported no source (the local file/SQLite harnesses, which carry
    /// no zone).
    ///
    /// `#[serde(default)]` keeps the field additive: an `activity-v1.jsonl`
    /// written before this field existed has no such key, and must still
    /// deserialize (as `None`) rather than failing the whole index read.
    #[serde(default)]
    pub source_zone: Option<String>,
    /// The session's label: what a session is listed under in the dashboard
    /// (29-UI-DESIGN §2.2). `None` is **not** "no label" — it is a row written
    /// before this field existed, i.e. an index that predates labels, a
    /// machine-level state the consumer reports once per machine. The two
    /// recorded states are in [`SessionTitle`].
    ///
    /// Additive for the same reason as `source_zone` above: `#[serde(default)]`
    /// turns a pre-label index line into `None` instead of a parse failure, so
    /// an old archive never stops being readable.
    #[serde(default)]
    pub title: Option<SessionTitle>,
    /// ChatGPT source facts from archived inbox metadata. `None` means no
    /// provenance record was written; an explicit `project: "unknown"` stays
    /// inside `captured` and is never converted to no-project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ProjectProvenance>,
    /// Collection-time source location class and parent-session reference.
    /// Paths are reduced to a class before they leave the scanner; no absolute
    /// source path is persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_provenance: Option<SessionProvenance>,
    /// Generic 4D provenance observations. Multiple values are retained when a
    /// session is resumed through more than one surface.
    #[serde(default, skip_serializing_if = "crate::activity::provenance_empty")]
    pub dimensions: crate::provenance::SessionProvenance,
    /// W219 · The account keys this session's records actually carry, deduped
    /// and sorted. Each is a **comparable** pair: the fingerprint value and the
    /// `saltId` that makes it comparable to another value.
    ///
    /// Empty means *no comparable key was recorded* — an `account` envelope of
    /// kind `unknown`, a bundle that predates the field, or an index line
    /// written before this field existed. Those are different histories and the
    /// same answer to the only question this field exists for ("can two records
    /// be compared?"), so they fold to one state rather than to three
    /// indistinguishable ones. Nothing here ever holds a raw account id: the
    /// value is the irreversible HMAC the extension computed
    /// (`apps/extension/lib/account-fingerprint.ts`), and a row that could
    /// carry an id by accident cannot exist.
    ///
    /// Additive for the same reason as [`ActivityRow::source_zone`]: an index
    /// written before W219 has no such key and must still deserialize.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub account_keys: Vec<AccountKey>,
    /// The conversation body this row was **measured from**: the identity the
    /// stage's `manifest::SessionManifest` records for that body — its shard
    /// count and the sha256 of its concatenated shards.
    ///
    /// It exists so a row can be verified before it is carried forward. A
    /// rebuild that finds a session directory with no body in the stage keeps
    /// the row the last rebuild wrote only while that row is still bound to the
    /// body the stage's manifest records: the manifest is what `reclaim-stage`
    /// writes immediately before it deletes a body, so a row bound to a
    /// *different* identity describes content that no longer exists (the
    /// session was re-sealed and reclaimed again), and keeping it would report
    /// the previous conversation's times for it, forever.
    ///
    /// `None` is a row written before this field existed, or a row derived
    /// somewhere the stage manifest is not the authority (the archive rebuild,
    /// whose bytes are the archive's). Either way it is unverifiable, and an
    /// unverifiable row is not a row that may be carried.
    ///
    /// Additive for the same reason as [`ActivityRow::source_zone`]: an index
    /// written before this field existed has no such key, and must still
    /// deserialize rather than failing the whole index read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_body: Option<MeasuredBody>,
}

/// Privacy-safe provenance captured from a source path during collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProvenance {
    /// Normalized source layout class, such as `main` or `subagents`.
    pub source_path_class: String,
    /// Native id of the parent session for a subagent session, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredSessionProvenance {
    session_id: String,
    #[serde(flatten)]
    provenance: SessionProvenance,
}

/// Record privacy-safe source-path provenance at collection time. The source
/// path itself is reduced to a layout class and is never written.
pub fn record_session_provenance(
    stage: &std::path::Path,
    machine: &str,
    session_id: &str,
    source: crate::models::HarnessSource,
    source_path: &std::path::Path,
) -> anyhow::Result<()> {
    use std::path::Component;

    let components: Vec<String> = source_path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let subagents = (source == crate::models::HarnessSource::ClaudeCode)
        .then(|| components.iter().position(|part| part == "subagents"))
        .flatten();
    let provenance = SessionProvenance {
        source_path_class: if subagents.is_some() {
            "subagents"
        } else {
            "main"
        }
        .to_string(),
        parent_session_ref: subagents
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| components.get(index).cloned()),
    };

    let path = stage
        .join("meta")
        .join(machine)
        .join("session-provenance-v1.jsonl");
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("session provenance path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut rows = load_session_provenance(stage, machine)?;
    if rows.get(session_id) == Some(&provenance) {
        return Ok(());
    }
    rows.insert(session_id.to_string(), provenance);
    let mut body = String::new();
    for (id, provenance) in rows {
        let row = StoredSessionProvenance {
            session_id: id,
            provenance,
        };
        body.push_str(&serde_json::to_string(&row)?);
        body.push('\n');
    }
    let mut tmp = tempfile::Builder::new()
        .prefix(".session-provenance-")
        .tempfile_in(parent)?;
    use std::io::Write;
    tmp.write_all(body.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(&path)
        .map_err(|error| anyhow::anyhow!("publish session provenance: {}", error.error))?;
    Ok(())
}

/// Read the collection-time provenance sidecar. Missing is the legacy state;
/// a present but malformed file is an error, not an empty map.
pub fn load_session_provenance(
    stage: &std::path::Path,
    machine: &str,
) -> anyhow::Result<std::collections::BTreeMap<String, SessionProvenance>> {
    let path = stage
        .join("meta")
        .join(machine)
        .join("session-provenance-v1.jsonl");
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(std::collections::BTreeMap::new());
        }
        Err(error) => return Err(error.into()),
    };
    let mut rows = std::collections::BTreeMap::new();
    for (line, raw) in content.lines().enumerate() {
        let row: StoredSessionProvenance = serde_json::from_str(raw).map_err(|error| {
            anyhow::anyhow!("parse session provenance line {}: {error}", line + 1)
        })?;
        rows.insert(row.session_id, row.provenance);
    }
    Ok(rows)
}

#[cfg(test)]
mod session_provenance_tests {
    use super::*;

    #[test]
    fn collection_records_subagent_parent_without_changing_source_bytes() {
        let sandbox = crate::test_support::Sandbox::new();
        let source = sandbox
            .root()
            .join(".claude/projects/project-fixture/parent-fixture/subagents/agent-fixture.jsonl");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        let raw = br#"{"fixture":"synthetic"}"#;
        std::fs::write(&source, raw).unwrap();

        record_session_provenance(
            &sandbox.root().join("stage"),
            "machine-fixture",
            "claude-code.machine-fixture.agent-fixture",
            crate::models::HarnessSource::ClaudeCode,
            &source,
        )
        .unwrap();

        assert_eq!(std::fs::read(&source).unwrap(), raw);
        let rows =
            load_session_provenance(&sandbox.root().join("stage"), "machine-fixture").unwrap();
        assert_eq!(
            rows["claude-code.machine-fixture.agent-fixture"],
            SessionProvenance {
                source_path_class: "subagents".into(),
                parent_session_ref: Some("parent-fixture".into()),
            }
        );
        let sidecar_path = sandbox
            .root()
            .join("stage/meta/machine-fixture/session-provenance-v1.jsonl");
        let sidecar = std::fs::read_to_string(&sidecar_path).unwrap();
        assert!(!sidecar.contains(sandbox.root().to_string_lossy().as_ref()));
        let unchanged_mtime = std::fs::metadata(&sidecar_path)
            .unwrap()
            .modified()
            .unwrap();
        record_session_provenance(
            &sandbox.root().join("stage"),
            "machine-fixture",
            "claude-code.machine-fixture.agent-fixture",
            crate::models::HarnessSource::ClaudeCode,
            &source,
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&sidecar_path)
                .unwrap()
                .modified()
                .unwrap(),
            unchanged_mtime,
            "an unchanged collection must not touch the provenance sidecar"
        );

        let main_source = sandbox
            .root()
            .join(".claude/projects/project-fixture/main-fixture.jsonl");
        std::fs::create_dir_all(main_source.parent().unwrap()).unwrap();
        let main_raw = br#"{"fixture":"main"}"#;
        std::fs::write(&main_source, main_raw).unwrap();
        let main_id = "claude-code.machine-fixture.main-fixture";
        record_session_provenance(
            &sandbox.root().join("stage"),
            "machine-fixture",
            main_id,
            crate::models::HarnessSource::ClaudeCode,
            &main_source,
        )
        .unwrap();
        let rows =
            load_session_provenance(&sandbox.root().join("stage"), "machine-fixture").unwrap();
        assert_eq!(rows[main_id].source_path_class, "main");
        assert_eq!(rows[main_id].parent_session_ref, None);
        assert_eq!(std::fs::read(main_source).unwrap(), main_raw);
    }
}

fn provenance_empty(value: &crate::provenance::SessionProvenance) -> bool {
    value.is_empty()
}

/// The identity of one conversation body, as `manifest::SessionManifest` records
/// it: how many sealed shards were concatenated, in global sequence order, and
/// the sha256 of that concatenation.
///
/// Both halves are kept because both are what the manifest compares on, and a
/// rebuild that kept a row on a partial match would be keeping a claim the
/// stage no longer supports. Comparison is exact; there is no "close enough".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasuredBody {
    /// Number of sealed shards concatenated, in sequence order.
    pub shard_count: usize,
    /// Lowercase hex sha256 of the concatenated shard payload.
    pub concat_sha256: String,
}

/// One **comparable** account key: the fingerprint value plus the `saltId` it
/// may be compared under.
///
/// The extension's salt is per install (`storage.local`), and
/// `contracts/inbox.schema.json` states the rule this type exists to keep
/// visible: two fingerprints may be compared **only when their `saltId` values
/// are equal**, and "a per-install salt makes values from different installs or
/// profiles incomparable, and that must not be read as an account switch".
/// So the pair travels together — a bare `value` compared across salts would
/// invent an account switch out of a salt rotation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountKey {
    /// The public half of the salt the value was computed under. Opaque; not
    /// the key and not derivable from it.
    pub salt_id: String,
    /// Lowercase hex HMAC-SHA256. Irreversible: the input id is never stored.
    pub value: String,
}

/// Capture-time project evidence plus the latest append-only source supplement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectProvenance {
    pub captured: Option<serde_json::Value>,
    #[serde(rename = "effectiveProject")]
    pub effective_project: Option<serde_json::Value>,
    pub supplement: Option<serde_json::Value>,
}

/// Where the first/last time came from (or why it could not be obtained).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "snake_case"
)]
pub enum TimeSource {
    /// Line contained an explicit, unambiguous RFC 3339 timestamp.
    Exact,
    /// Derived from a field, with the inference written down.
    Inferred { how: String },
    /// Web chat harness: the span was derived from the **per-message**
    /// timestamps inside the stored payload (`messages`). `exact` is true when
    /// at least one came from an RFC 3339 string, false when all were numeric
    /// epochs whose unit/zone the parser had to interpret.
    Messages { exact: bool },
    /// Web chat harness: no messages are archived yet, so the span is only the
    /// conversation list's update time. Low confidence, and named as such so a
    /// consumer can tell it from a message-derived interval.
    #[serde(rename = "list-updated")]
    ListUpdated,
    /// The archived session holds no conversation content at all — zero lines,
    /// or only metadata lines (summary / file-history-snapshot / bridge-session
    /// / cost-state / journal / …) with no user or assistant message.
    ///
    /// This is deliberately **not** [`TimeSource::Unknown`] (ADR-035): there is
    /// nothing to place in time, so it is a different claim from "a real
    /// conversation whose time we could not find". It is counted separately and
    /// excluded from the unknown tallies (`machine_recall`, the recall WARN).
    NoConversationContent,
    /// The recorded `[first_unix, last_unix]` is only an **inner** bound of this
    /// session's conversation span: the session also holds records that could not
    /// be placed in time — a record of a type this module does not classify, or a
    /// conversation record whose own timestamp could not be read — and such a
    /// record may be conversation activity *outside* the recorded interval.
    ///
    /// The bounds are measured, so this is not [`TimeSource::Unknown`]; but they
    /// are not the span, so it is not `Exact`/`Inferred` either. `how` records
    /// how the bounds we do have were read (the same vocabulary `Inferred` uses),
    /// `why` records the reason the range is partial.
    ///
    /// A consumer asking "was this session active in this window?" may treat an
    /// **overlap** with the recorded bounds as proof (the recorded interval is a
    /// sub-interval of the span), but must treat a **non**-overlap as "may be
    /// outside" rather than as a proven absence. See
    /// [`crate::selector::TimeBounds::Partial`].
    PartialRange { how: String, why: String },
    /// Could not be obtained; why it could not.
    Unknown { why: String },
}

impl TimeSource {
    /// True when the session was archived with no conversation content at all
    /// (ADR-035). Distinct from [`TimeSource::Unknown`]: there is no
    /// conversation to place in time, not a conversation whose time is missing.
    pub fn is_no_conversation_content(&self) -> bool {
        matches!(self, TimeSource::NoConversationContent)
    }

    /// True when the carried bounds are only **part** of the conversation span —
    /// see [`TimeSource::PartialRange`]. Every other state's bounds are either
    /// the whole span or absent, so a consumer must not answer "outside the
    /// window" from this state's bounds without also asking this.
    pub fn bounds_are_partial(&self) -> bool {
        matches!(self, TimeSource::PartialRange { .. })
    }
}

/// How many characters a label text may hold (29-UI-DESIGN §2.2). Characters,
/// not bytes — a CJK prompt's label must never be cut inside one.
pub const TITLE_CAP_CHARS: usize = 100;

/// Where a session's label text came from. The label's provenance travels with
/// it (along ADR-031's "the source never lies"): a label that was invented by
/// the harness and one that was quoted from the conversation are different
/// claims, and the reader can tell them apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TitleSource {
    /// The harness's own title: a claude-code `ai-title` or `summary` line, a Grok
    /// CLI `session_docs.title`, or the label a web platform's archived body
    /// carries for the conversation.
    HarnessTitle,
    /// The head of the session's first user line, capped at
    /// [`TITLE_CAP_CHARS`] and flagged when the cap cut anything.
    FirstUserLine,
}

/// The label state an index row records for one session — the design's honest
/// words (29-UI-DESIGN §2.2):
///
/// * [`SessionTitle::Known`] — there is text to show, and the row says where
///   it came from and whether the cap cut it;
/// * [`SessionTitle::NoLabelRecorded`] — the content was read and holds
///   nothing label-able, **and also** the recorded state for a harness whose
///   lines this module does not read a title from (codex, cursor, and the web
///   platforms whose body carries no label of its own): no guess, no empty
///   string.
///
/// A row that predates labels writes no `title` key at all (see
/// [`ActivityRow::title`]) — that is the third, machine-level state, recorded
/// at query time rather than in any row. There is deliberately no "unknown"
/// variant here: a row records what a completed read found, and "we could not
/// tell" is composed where the read happens, exactly like the time side composes
/// its no-row cases in `search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "snake_case",
    rename_all_fields = "snake_case"
)]
pub enum SessionTitle {
    Known {
        text: String,
        source: TitleSource,
        truncated: bool,
    },
    NoLabelRecorded,
}

/// The label candidates one scan picked up, before the priority order decides
/// which of them becomes the row's title.
///
/// Explicit title readers supply candidates for claude-code, Grok CLI, and the
/// web platforms whose own archived body carries the conversation's label.
/// Claude Code title lines are `{"type":"ai-title","aiTitle":…}` and
/// `{"type":"summary","summary":…}` (measured shapes in the W156 report);
/// Grok CLI titles are read from its `session_docs` row; web labels use
/// [`web_title`]. Every other harness records [`SessionTitle::NoLabelRecorded`]
/// by design.
#[derive(Default)]
struct TitleCandidates {
    /// The harness's own title, from `ai-title` lines. The **last** one wins:
    /// a title can be revised mid-session, and the last line is the current
    /// one.
    ai_title: Option<String>,
    /// A `summary` line — claude-code's resume marker, written at the head of
    /// a continuation file. The **first** one wins: the head-of-file summary
    /// describes the whole continuation.
    summary: Option<String>,
    /// The first user line that carries prompt text (a plain string content or
    /// a `text` block — never a `tool_result` block, which is a tool answering,
    /// not a person asking). Only the first is kept, and a line marked
    /// `isMeta` never qualifies: that is the harness talking to itself.
    first_user: Option<String>,
    /// The label a web platform's own archived body carries for the
    /// conversation (see [`web_title`]). The **last** one wins, as for
    /// `ai_title`: a re-capture is a later reading of the same conversation,
    /// and a later body that carries no label leaves the recorded one alone
    /// rather than erasing it.
    web: Option<String>,
}

impl TitleCandidates {
    /// Fold one parsed claude-code line's candidates.
    fn fold(&mut self, value: &serde_json::Value) {
        let ty = value.get("type").and_then(serde_json::Value::as_str);
        let non_empty = |v: &serde_json::Value, key: &str| {
            v.get(key)
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        match ty {
            Some("ai-title") => {
                if let Some(t) = non_empty(value, "aiTitle") {
                    self.ai_title = Some(t);
                }
            }
            Some("summary") => {
                if self.summary.is_none() {
                    if let Some(t) = non_empty(value, "summary") {
                        self.summary = Some(t);
                    }
                }
            }
            Some("user") => {
                if self.first_user.is_none()
                    && value.get("isMeta").and_then(serde_json::Value::as_bool) != Some(true)
                {
                    if let Some(t) = user_prompt_text(value) {
                        self.first_user = Some(t);
                    }
                }
            }
            _ => {}
        }
    }

    /// Fold the label a web platform's archived body carried, if it carried
    /// one. A body with no label never clears a label an earlier body gave: the
    /// conversation keeps the label it was recorded under.
    fn fold_web(&mut self, title: Option<String>) {
        if title.is_some() {
            self.web = title;
        }
    }

    /// The label this session's scan produced, in the row's recorded shape.
    fn resolve(self) -> SessionTitle {
        let (raw, source) = if let Some(t) = self.ai_title {
            (t, TitleSource::HarnessTitle)
        } else if let Some(t) = self.summary {
            (t, TitleSource::HarnessTitle)
        } else if let Some(t) = self.web {
            (t, TitleSource::HarnessTitle)
        } else if let Some(t) = self.first_user {
            (t, TitleSource::FirstUserLine)
        } else {
            return SessionTitle::NoLabelRecorded;
        };
        let cut = raw.chars().count() > TITLE_CAP_CHARS;
        let text = raw.chars().take(TITLE_CAP_CHARS).collect();
        SessionTitle::Known {
            text,
            source,
            truncated: cut,
        }
    }
}

/// The prompt text of one claude-code user line, when it carries one: a
/// string `message.content`, or the first `{"type":"text","text":…}` block of
/// an array content. Tool-result blocks are skipped by construction — they are
/// what a tool answered, not what the person asked, and would label the
/// session with tool output.
fn user_prompt_text(value: &serde_json::Value) -> Option<String> {
    let content = value.get("message")?.get("content")?;
    match content {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Array(blocks) => blocks.iter().find_map(|block| {
            if block.get("type").and_then(serde_json::Value::as_str) != Some("text") {
                return None;
            }
            block
                .get("text")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        }),
        _ => None,
    }
}

/// Result of analysing one session's lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeAnalysis {
    pub first_unix: Option<i64>,
    pub last_unix: Option<i64>,
    pub line_count: u64,
    pub time_source: TimeSource,
    pub source_zone: Option<String>,
    pub title: SessionTitle,
}

/// Harnesses this module knows how to read times from. Everything else is
/// reported [`TimeSource::Unknown`] with an explicit "not implemented" `why`,
/// never guessed.
///
/// The first group are read from files/SQLite exports whose lines are the
/// conversation itself. The second group are the browser-extension web chat
/// harnesses, whose archived line is an inbox bundle carrying the raw HTTP body
/// under `raw.text` (see `inbox.rs`).
const SUPPORTED_HARNESSES: &[&str] = &[
    "claude-code",
    "codex",
    "opencode",
    "cursor",
    "gemini-cli",
    "google-antigravity",
    "grok",
    "kimi-code",
    "chatgpt",
    "deepseek",
    "claude",
    "gemini",
    "perplexity",
    "kimi",
];

/// Harnesses whose **line shape we can classify** into conversation vs
/// metadata, and therefore the only ones that can answer "this session holds no
/// conversation content at all" ([`TimeSource::NoConversationContent`]).
///
/// `kimi-code` is the CLI harness; the browser-extension `kimi` is a different
/// shape and is handled by [`web_time`].
const CLASSIFIED_HARNESSES: &[&str] = &["claude-code", "kimi-code"];

/// kimi-code wire records that carry the conversation: a message, a loop step
/// (assistant output and tool exchanges are assembled from those), or a user
/// turn's input.
///
/// Vocabulary measured on the installed Kimi Code 0.39.1 implementation
/// (`<KIMI_CODE_HOME>/bin/kimi`, read 2026-09-25) — its own v1→v2 resume-replay
/// mapping lists exactly these as the records a restored agent turns into
/// `{type:'message'}`. Corroborated on 41 real `wire.jsonl` files by an
/// independent importer: `context.append_message` is always `role: user`, and
/// assistant text lives in `context.append_loop_event → event.type ==
/// "content.part"` (<https://github.com/MemPalace/mempalace/issues/2180>).
const KIMI_CODE_CONVERSATION_OPS: &[&str] = &[
    "context.append_message",
    "context.append_loop_event",
    "turn.prompt",
    "turn.steer",
];

/// kimi-code wire records that positively rebuild state and carry no
/// conversation: configuration, tool discovery, usage/llm bookkeeping, goals,
/// plan mode, permissions, compaction and the turn lifecycle.
///
/// Every name here was read off the installed implementation, either as an
/// emitted `type` or in its record-vocabulary comment. The v2 families whose
/// member names were **not** measured (`interaction.*`, `token_counting.*`) are
/// deliberately absent — an op we cannot name is undetermined, not metadata.
///
/// This is a **whitelist** on purpose. An op that is neither here nor in
/// [`KIMI_CODE_CONVERSATION_OPS`] is *undetermined* rather than metadata, so a
/// conversation op added by a future kimi-code release is reported as an
/// unknown time — an honest gap a reader can triage — instead of being read as
/// "this session holds no conversation at all", which would record an unknown
/// as empty (this module's first rule, and ADR-035's).
const KIMI_CODE_BOOKKEEPING_OPS: &[&str] = &[
    // Envelope and configuration.
    "metadata",
    "config.update",
    "profile.bind",
    // Tools and MCP.
    "tools.set_active_tools",
    "tools.update_store",
    "mcp.tools_discovered",
    // Model traffic and accounting.
    "usage.record",
    "llm.request",
    "context.update_token_count",
    // Turn lifecycle; the prompt/steer ends of it are conversation ops.
    "turn.started",
    "turn.ended",
    "turn.step.started",
    "turn.step.completed",
    "turn.step.retrying",
    // Goals, plans, permissions.
    "goal.create",
    "goal.update",
    "goal.clear",
    "plan_mode.enter",
    "plan_mode.cancel",
    "plan_mode.exit",
    "plan.revision",
    "permission.set_mode",
    "permission.record_approval_result",
    // Context surgery.
    "context.undo",
    "context.clear",
    "context.apply_compaction",
    "full_compaction.begin",
    "full_compaction.cancel",
    "full_compaction.complete",
    // Background work and extensions.
    "task.started",
    "task.terminated",
    "skill.activate",
];

/// Web chat harnesses whose archived payload is an inbox bundle, not a message
/// line. Their times live inside `raw.text` and are read by [`web_time`]. The
/// reader uses this same list to decide which harnesses have the generic reader
/// as their reader — see `normalize::is_web_bundle` — so the two can never
/// disagree about which archived line is a bundle.
pub(crate) const WEB_HARNESSES: &[&str] = &[
    "chatgpt",
    "deepseek",
    "claude",
    "grok",
    "gemini",
    "perplexity",
    "kimi",
];

/// The `how` recorded for bounds read from **numeric epoch** timestamps, whose
/// unit the reader had to infer. One constant, shared by [`TimeSource::Inferred`]
/// and [`TimeSource::PartialRange`], so the two can never describe the same
/// inference in two different ways.
const EPOCH_HOW: &str = "in-line timestamps are numeric epochs: unit inferred from magnitude (values in the 2020–2100 seconds range treated as seconds; millis-range values divided by 1000, micros-range values divided by 1_000_000, to get seconds)";

/// The `how` recorded for bounds read from explicit RFC 3339 strings, used when
/// the range is partial. The complete case is [`TimeSource::Exact`], which
/// carries no `how` because there is nothing to explain.
const RFC3339_HOW: &str =
    "the records that carry a time do so as explicit RFC 3339 strings, which are unambiguous";

/// Why the recorded span is only *part* of the session's conversation, or
/// `None` when nothing in the session failed to be placed in time.
///
/// Only a harness whose line shapes we classify can answer this at all: for the
/// others, "a line we could not classify" is not a state we can distinguish from
/// "a line that carries no time", so no partiality is claimed there.
///
/// The text carries **no numbers of its own** (no timestamps): the human views
/// group sessions by this string, so a per-session value would put every session
/// in its own group. The measured bounds are the row's `first_unix`/`last_unix`.
fn partial_range_why(
    harness: &str,
    undetermined_lines: u64,
    conversation_without_time: u64,
    conversation_invalid_time: u64,
) -> Option<String> {
    if !CLASSIFIED_HARNESSES.contains(&harness) {
        return None;
    }
    let mut reasons: Vec<String> = Vec::new();
    if undetermined_lines > 0 {
        reasons.push(format!(
            "{undetermined_lines} record(s) of a type this module does not recognise"
        ));
    }
    if conversation_without_time > 0 {
        reasons.push(format!(
            "{conversation_without_time} conversation record(s) that carry no timestamp field"
        ));
    }
    if conversation_invalid_time > 0 {
        reasons.push(format!(
            "{conversation_invalid_time} conversation record(s) whose timestamp could not be read"
        ));
    }
    if reasons.is_empty() {
        return None;
    }
    Some(format!(
        "the recorded span is only part of this session's conversation — {} may carry activity outside it",
        reasons.join("; ")
    ))
}

/// Analyse a session's lines and pull out the earliest/latest conversation time.
///
/// Every non-blank line is counted in [`TimeAnalysis::line_count`] regardless
/// of whether it carries a timestamp — that number is "how big is this session",
/// which stays meaningful even when the time does not. Timestamps are gathered
/// only from the known-parseable harnesses ([`SUPPORTED_HARNESSES`]); for any
/// other harness the session is [`TimeSource::Unknown`] with an explicit
/// "not implemented" `why`, so a caller can tell "we did not look" from
/// "there was nothing to find".
pub fn analyze_session(harness: &str, lines: &[&str]) -> TimeAnalysis {
    let mut line_count = 0u64;
    let mut fold = Fold::default();

    // Diagnosis counters for the Unknown branch: they keep "parse failure"
    // distinct from "this harness never records a time".
    let mut unparseable_json = 0u64;
    let mut invalid_timestamp = 0u64;
    let mut saw_timestamp_field = false;

    // Conversation-content counters (ADR-035). Only the harnesses in
    // [`CLASSIFIED_HARNESSES`] have a line shape we can classify without
    // guessing. A line that cannot be classified — unparseable, or a record
    // type we do not know — is *undetermined* rather than metadata, so a
    // session is only ever called "no conversation content" when every line was
    // positively classified as metadata.
    let mut conversation_lines = 0u64;
    let mut undetermined_lines = 0u64;

    // Records that are positively conversation but could not be placed in time,
    // counted apart from the outcome counters above because they do not change
    // the "no conversation content" verdict — they *are* conversation. They do
    // change what the recorded span means: a conversation record whose own time
    // we could not read may have happened outside it (see
    // [`TimeSource::PartialRange`]).
    let mut conversation_without_time = 0u64;
    let mut conversation_invalid_time = 0u64;

    // The label candidates for the harnesses with an explicit title reader.
    let mut titles = TitleCandidates::default();

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        line_count += 1;

        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            unparseable_json += 1;
            if CLASSIFIED_HARNESSES.contains(&harness) {
                undetermined_lines += 1;
            }
            continue;
        };
        if harness == "claude-code" {
            titles.fold(&value);
        } else if harness == "grok" {
            // Grok CLI SQLite rows and Grok web inbox records share a registry
            // ID. Prefer the CLI's exact envelope, then retain the existing
            // web-title reader for inbox records.
            titles.fold_web(grok_cli_title(&value).or_else(|| web_title(harness, &value)));
        } else if WEB_HARNESSES.contains(&harness) {
            titles.fold_web(web_title(harness, &value));
        }
        let line_class = classify_line(harness, &value);
        // Whether this line is positively a conversation record, needed by the
        // time-match below to tell "a conversation record we could not place in
        // time" (which makes the span partial) from "a line of a harness we do
        // not classify" (which is read for a time and nothing else).
        let is_conversation_line = matches!(line_class, LineClass::Conversation);
        match line_class {
            LineClass::Conversation => conversation_lines += 1,
            // A metadata record can carry a timestamp too (Claude Code summary
            // shards; kimi-code's config and tool-discovery records). Those
            // describe the record, not conversation activity.
            LineClass::Metadata => continue,
            LineClass::Undetermined => {
                undetermined_lines += 1;
                continue;
            }
            // A harness whose lines we do not classify: look for a time anyway,
            // exactly as before.
            LineClass::Unclassified => {}
        }
        match line_time(harness, &value) {
            // A single line can now span a whole session (opencode/cursor/
            // gemini export one session as one JSON object), so a line carries
            // its own first/last and the aggregation folds those in.
            lt @ LineTime::Time { .. } => fold.add(lt),
            LineTime::Absent => {
                if is_conversation_line {
                    conversation_without_time += 1;
                }
            }
            LineTime::NoTimestampField => {
                saw_timestamp_field = false;
            }
            LineTime::Invalid => {
                saw_timestamp_field = true;
                invalid_timestamp += 1;
                if is_conversation_line {
                    conversation_invalid_time += 1;
                }
            }
        }
    }

    // gemini-cli stores a whole session as one **pretty-printed JSON
    // document**, not JSONL, so its lines do not parse individually. When no
    // line yielded a time, re-read the file as the single document it is.
    if fold.first.is_none() && harness == "gemini-cli" {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(lines.join("\n").trim()) {
            fold.add(gemini_time(&value));
        }
    }

    let time_source = if fold.first.is_some() {
        // A record that could not be placed in time makes the recorded interval
        // an inner bound of the span, so it is reported as such **instead of**
        // the confidence the read records alone would support (see
        // [`TimeSource::PartialRange`]). Checked first: it is a statement about
        // what the bounds are, and it is true whatever the bounds were read from.
        if let Some(why) = partial_range_why(
            harness,
            undetermined_lines,
            conversation_without_time,
            conversation_invalid_time,
        ) {
            TimeSource::PartialRange {
                how: if fold.any_rfc3339 {
                    RFC3339_HOW.to_string()
                } else {
                    EPOCH_HOW.to_string()
                },
                why,
            }
        } else if fold.any_messages {
            TimeSource::Messages {
                exact: fold.any_rfc3339,
            }
        } else if fold.any_list {
            // A list-only span is low confidence by construction; it is never
            // reported as an exact/inferred message interval.
            TimeSource::ListUpdated
        } else if fold.any_rfc3339 {
            TimeSource::Exact
        } else {
            TimeSource::Inferred {
                how: EPOCH_HOW.to_string(),
            }
        }
    } else if no_conversation_content(harness, line_count, conversation_lines, undetermined_lines) {
        TimeSource::NoConversationContent
    } else {
        TimeSource::Unknown {
            why: unknown_why(
                harness,
                unparseable_json,
                invalid_timestamp,
                saw_timestamp_field,
            ),
        }
    };

    TimeAnalysis {
        first_unix: fold.first,
        last_unix: fold.last,
        line_count,
        time_source,
        source_zone: fold.source_zone,
        // A harness whose candidates were never folded resolves to the design's
        // "no label recorded" — see the module docs.
        title: titles.resolve(),
    }
}

/// One archived line's own conversation time, read by the same per-harness
/// extractor [`analyze_session`] uses, without the session-level aggregation.
///
/// Unlike [`analyze_session`] this reads the time off a line whatever the line
/// is classified as — a Claude Code `attachment` record is metadata to that
/// function (it is skipped before its time is read) but still carries the
/// top-level `timestamp` a caller may need. `None` means the line carries no
/// readable time; an unknown time is never a zero.
pub fn line_unix(harness: &str, line: &str) -> Option<i64> {
    let value = serde_json::from_str::<serde_json::Value>(line.trim()).ok()?;
    match line_time(harness, &value) {
        LineTime::Time { first, .. } => Some(first),
        _ => None,
    }
}

/// Whether an archived session holds no conversation content at all (ADR-035).
///
/// * zero non-blank lines — the shard is empty;
/// * a [`CLASSIFIED_HARNESSES`] session whose every line is metadata: no line
///   is a `user` / `assistant` message (claude-code: summary,
///   file-history-snapshot, bridge-session, cost-state, agent-name, ai-title,
///   last-prompt, journal, …) and no line carries a wire message (kimi-code:
///   config, MCP tool discovery, usage, turn bookkeeping, …).
///
/// Only harnesses whose line shapes we know can answer this. An unsupported
/// harness keeps its explicit "not implemented" [`TimeSource::Unknown`], and a
/// harness whose conversation shape we do not classify is never called empty.
fn no_conversation_content(
    harness: &str,
    line_count: u64,
    conversation_lines: u64,
    undetermined_lines: u64,
) -> bool {
    if !SUPPORTED_HARNESSES.contains(&harness) {
        return false;
    }
    line_count == 0
        || (CLASSIFIED_HARNESSES.contains(&harness)
            && conversation_lines == 0
            && undetermined_lines == 0)
}

/// How one archived line relates to the conversation, for the harnesses whose
/// line shapes are known.
enum LineClass {
    /// A message, or a wire record that carries one.
    Conversation,
    /// A positively recognised metadata / state-rebuilding record.
    Metadata,
    /// Not classifiable: an unparseable line, or a record type we do not know.
    /// Never counted as metadata — "we do not know this record" must not become
    /// "there was nothing here".
    Undetermined,
    /// This harness's lines are not classified (they are read for a time, and
    /// can never yield the "no conversation content" verdict).
    Unclassified,
}

/// Classify one parsed line of a harness whose shape we know.
fn classify_line(harness: &str, value: &serde_json::Value) -> LineClass {
    match harness {
        // Claude Code: `user` / `assistant` are the conversation; a line whose
        // `type` we know but that is neither is metadata; a line with no `type`
        // is undetermined.
        "claude-code" => match value.get("type").and_then(serde_json::Value::as_str) {
            Some("user") | Some("assistant") => LineClass::Conversation,
            Some(_) => LineClass::Metadata,
            None => LineClass::Undetermined,
        },
        // kimi-code: the wire record vocabulary decides. A record type that is
        // neither a conversation op nor a known bookkeeping op is undetermined.
        "kimi-code" => match value.get("type").and_then(serde_json::Value::as_str) {
            Some(op) if KIMI_CODE_CONVERSATION_OPS.contains(&op) => LineClass::Conversation,
            Some(op) if KIMI_CODE_BOOKKEEPING_OPS.contains(&op) => LineClass::Metadata,
            _ => LineClass::Undetermined,
        },
        _ => LineClass::Unclassified,
    }
}

/// The accumulators a stream of [`LineTime`]s folds into.
#[derive(Default)]
struct Fold {
    first: Option<i64>,
    last: Option<i64>,
    any_rfc3339: bool,
    any_messages: bool,
    any_list: bool,
    source_zone: Option<String>,
}

impl Fold {
    /// Fold one line's verdict into the session span. A non-`Time` verdict for
    /// the multi-line gemini-cli document is simply ignored.
    fn add(&mut self, line: LineTime) {
        let LineTime::Time {
            first: f,
            last: l,
            rfc3339,
            basis,
            zone,
        } = line
        else {
            return;
        };
        self.first = Some(self.first.map_or(f, |old| old.min(f)));
        self.last = Some(self.last.map_or(l, |old| old.max(l)));
        // Once any timestamp is unambiguous RFC 3339, the whole session is
        // Exact; otherwise (numeric epochs only) it is Inferred.
        if rfc3339 {
            self.any_rfc3339 = true;
        }
        match basis {
            Basis::Messages => self.any_messages = true,
            Basis::List => self.any_list = true,
            Basis::Local => {}
        }
        if self.source_zone.is_none() {
            self.source_zone = zone;
        }
    }
}

/// Compose the `why` for the "no usable time" case, keeping the distinct
/// reasons separate so a caller can tell them apart.
fn unknown_why(
    harness: &str,
    unparseable_json: u64,
    invalid_timestamp: u64,
    saw_timestamp_field: bool,
) -> String {
    if !SUPPORTED_HARNESSES.contains(&harness) {
        return format!(
            "conversation-time parsing is not implemented for this harness ({harness}) (supported: {})",
            SUPPORTED_HARNESSES.join(", ")
        );
    }
    let mut reasons: Vec<String> = Vec::new();
    if unparseable_json > 0 {
        reasons.push(format!(
            "{unparseable_json} line(s) could not be parsed as JSON"
        ));
    }
    if invalid_timestamp > 0 {
        reasons.push(
            "a timestamp field exists but the value is unparseable or outside the plausible 2020–2100 range"
                .to_string(),
        );
    }
    if !saw_timestamp_field {
        reasons.push("no timestamp field found within the line".to_string());
    }
    if reasons.is_empty() {
        reasons.push("no usable timestamp at all".to_string());
    }
    format!("cannot determine conversation time: {}", reasons.join("; "))
}

/// What one line yielded.
enum LineTime {
    /// A usable unix-seconds timestamp (or span) was recovered from this line.
    Time {
        /// Earliest unix-second timestamp in the line.
        first: i64,
        /// Latest unix-second timestamp in the line.
        last: i64,
        /// True when at least one came from an explicit RFC 3339 string
        /// (unambiguous); false when all came from numeric epochs whose unit
        /// was inferred.
        rfc3339: bool,
        /// What the span was derived from: the local harnesses' own fields
        /// (neither), web **messages**, or a web list update time.
        basis: Basis,
        /// The source zone label, when the source named one.
        zone: Option<String>,
    },
    /// The harness is supported but this line carries no timestamp field.
    Absent,
    /// This harness is not supported at all — nothing to look for.
    NoTimestampField,
    /// A timestamp field exists but is unparseable / out of the plausible window.
    Invalid,
}

/// A local-harness span: not message/list classified, no named source zone.
fn local_time(first: i64, last: i64, rfc3339: bool) -> LineTime {
    LineTime::Time {
        first,
        last,
        rfc3339,
        basis: Basis::Local,
        zone: None,
    }
}

/// Where a recovered span came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Basis {
    /// A local CLI harness's own fields (the pre-existing harnesses).
    Local,
    /// Per-message timestamps inside a web chat payload.
    Messages,
    /// A web conversation list's update time, with no messages archived yet.
    List,
}

/// Extract the timestamp-bearing value from a line, per harness.
///
/// `claude-code` / `codex` export one JSON object per message line, each with a
/// top-level `timestamp`. The SQLite-backed harnesses (opencode, cursor) and
/// gemini-cli export one *whole session* per JSON line, so their timestamps live
/// in the nested structure and a single line yields a full first/last span.
///
/// Web chat harnesses (`WEB_HARNESSES`) archive an inbox bundle per line; their
/// payload is the raw HTTP body under `raw.text` and is read by [`web_time`].
fn line_time(harness: &str, value: &serde_json::Value) -> LineTime {
    // `grok` is two harnesses sharing one name: the browser-extension bundle
    // (a web payload) and the grok **CLI**, whose archived line is a SQLite
    // `session_docs` row carrying the session's own `updated_at`.
    if harness == "grok" {
        return grok_time(value);
    }
    if WEB_HARNESSES.contains(&harness) {
        return web_time(harness, value);
    }
    match harness {
        "claude-code" | "codex" => top_level_timestamp(value),
        "opencode" => opencode_time(value),
        "cursor" => cursor_time(value),
        "gemini-cli" => gemini_time(value),
        "google-antigravity" => antigravity_time(value),
        "kimi-code" => kimi_code_time(value),
        "zed" => zed_time(value),
        _ => LineTime::NoTimestampField,
    }
}

/// zed: the CLI archives one `threads` SQLite row per session as
/// `{schema, table:"threads", session:{…, updated_at, created_at}}`, where `updated_at`
/// and `created_at` are ISO 8601 strings.
fn zed_time(value: &serde_json::Value) -> LineTime {
    if let Some(updated) = value.get("session").and_then(|session| {
        session
            .get("updated_at")
            .or_else(|| session.get("created_at"))
    }) {
        return match one_ts_value(updated) {
            Some((t, rfc3339)) => local_time(t, t, rfc3339),
            None => LineTime::Invalid,
        };
    }
    LineTime::Absent
}

/// kimi-code: the agent journal `<sessionDir>/agents/main/wire.jsonl`, one
/// record per line as `{type, time, …}`.
///
/// `time` is the writer's own `Date.now()` — the journal appends
/// `time: Date.now()` to every record that does not already carry one — so it
/// is an epoch in **milliseconds**. Its unit is inferred by magnitude, which
/// makes it [`TimeSource::Inferred`] and never `Exact` (the same rule as
/// opencode's and cursor's epoch-millis fields). There is no source-zone label
/// to record: a numeric epoch is an absolute instant.
///
/// Only the conversation ops reach here ([`classify_line`] skips the rest), so
/// the record's own `time` is a real conversation time. The opening `metadata`
/// record carries `created_at` instead of `time` and is metadata anyway — it is
/// never read as the session's time.
fn kimi_code_time(value: &serde_json::Value) -> LineTime {
    let Some(ts) = value.get("time") else {
        return LineTime::Absent;
    };
    match one_ts_value(ts) {
        Some((t, rfc3339)) => local_time(t, t, rfc3339),
        None => LineTime::Invalid,
    }
}

/// grok: the CLI archives one `session_docs` SQLite row per session as
/// `{schema, table:"session_docs", session:{…, updated_at}}`, where `updated_at`
/// is the session's own **epoch-seconds** last-update time (measured in the
/// registry's grok schema, `time_is_seconds: true`). It is a session-level
/// instant, not a per-message timestamp, so it is `Inferred` (numeric epoch),
/// never `Exact`. When the line is instead the browser-extension bundle (no
/// `session.updated_at`), the web reader handles it.
fn grok_time(value: &serde_json::Value) -> LineTime {
    if let Some(updated) = value
        .get("session")
        .and_then(|session| session.get("updated_at"))
    {
        return match one_ts_value(updated) {
            Some((t, rfc3339)) => local_time(t, t, rfc3339),
            None => LineTime::Invalid,
        };
    }
    web_time("grok", value)
}

/// Grok CLI's SQLite export carries its harness title in the same
/// `session_docs` row as `updated_at`. Require that exact envelope so a title
/// from an unrelated object is never attributed to this session.
fn grok_cli_title(value: &serde_json::Value) -> Option<String> {
    if value.get("schema").and_then(serde_json::Value::as_str)
        != Some("chat-stasher.sqlite.session.v1")
        || value.get("table").and_then(serde_json::Value::as_str) != Some("session_docs")
    {
        return None;
    }
    value
        .get("session")?
        .get("title")?
        .as_str()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
}

/// Parse one timestamp value (RFC 3339 string, numeric epoch string, or numeric
/// epoch) into unix seconds plus whether it was an unambiguous RFC 3339.
fn one_ts_value(raw: &serde_json::Value) -> Option<(i64, bool)> {
    match raw {
        serde_json::Value::String(s) => match parse_rfc3339(s) {
            Some(t) => Some((t, true)),
            // A numeric epoch can also arrive as a JSON string.
            None => parse_numeric_epoch(s).map(|t| (t, false)),
        },
        serde_json::Value::Number(n) => numeric_value_seconds(n).map(|t| (t, false)),
        _ => None,
    }
}

/// Fold `field` across an iterator of JSON objects into a first/last span.
/// Returns `(first, last, any_rfc3339, saw_field, had_invalid)`.
fn collect_span<'a, I>(objects: I, field: &str) -> (Option<i64>, Option<i64>, bool, bool, bool)
where
    I: Iterator<Item = &'a serde_json::Value>,
{
    let mut first: Option<i64> = None;
    let mut last: Option<i64> = None;
    let mut any_rfc3339 = false;
    let mut saw = false;
    let mut invalid = false;
    for object in objects {
        let Some(raw) = object.get(field) else {
            continue;
        };
        saw = true;
        match one_ts_value(raw) {
            Some((t, rfc)) => {
                any_rfc3339 |= rfc;
                first = Some(first.map_or(t, |old| old.min(t)));
                last = Some(last.map_or(t, |old| old.max(t)));
            }
            None => invalid = true,
        }
    }
    (first, last, any_rfc3339, saw, invalid)
}

/// claude-code / codex: one timestamp per line at the top level.
fn top_level_timestamp(value: &serde_json::Value) -> LineTime {
    let Some(ts) = value.get("timestamp") else {
        return LineTime::Absent;
    };
    match one_ts_value(ts) {
        Some((t, rfc3339)) => local_time(t, t, rfc3339),
        None => LineTime::Invalid,
    }
}

/// opencode: one exported session per line, envelope
/// `{schema, session:{time_created,time_updated}, messages:[{time_created,...}]}`.
///
/// `time_created` is an epoch-**millis** number (unit inferred → `Inferred`, not
/// `Exact`). Message-level times are preferred because they are the real
/// conversation span; only when a session has no messages do we fall back to the
/// session-level `time_created`/`time_updated` (session creation/last-update).
fn opencode_time(value: &serde_json::Value) -> LineTime {
    let messages = value
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten();
    let (mf, ml, mrfc, msaw, minvalid) = collect_span(messages, "time_created");
    if let (Some(f), Some(l)) = (mf, ml) {
        return local_time(f, l, mrfc);
    }
    // No usable message time — fall back to the session-level fields.
    let (sf, sl, srfc, ssaw, sinvalid) = match value.get("session") {
        Some(session) => {
            let has_created = session.get("time_created").is_some();
            let has_updated = session.get("time_updated").is_some();
            let first = session.get("time_created").and_then(one_ts_value);
            let last = session.get("time_updated").and_then(one_ts_value);
            let invalid = (has_created && first.is_none()) || (has_updated && last.is_none());
            let rfc =
                // reason: a missing session timestamp is simply not RFC3339; the
                // distinct "field present but unparseable" case is tracked by the
                // `invalid` tally below, so false is an honest "no timestamp".
                first.map(|(_, r)| r).unwrap_or(false) || last.map(|(_, r)| r).unwrap_or(false);
            (
                first.map(|(t, _)| t),
                last.map(|(t, _)| t),
                rfc,
                has_created || has_updated,
                invalid,
            )
        }
        None => (None, None, false, false, false),
    };
    match (sf, sl) {
        (Some(f), Some(l)) => local_time(f, l.max(f), srfc),
        (Some(f), None) => local_time(f, f, srfc),
        (None, Some(l)) => local_time(l, l, srfc),
        (None, None) => {
            if msaw || ssaw || minvalid || sinvalid {
                LineTime::Invalid
            } else {
                LineTime::Absent
            }
        }
    }
}

/// cursor: one composer per line, in one of two exported shapes —
///   global  `{schema:"chat-stasher.sqlite.session.v1", session:{value:{createdAt}}}`,
///   legacy  `{schema:"chat-stasher.cursor.legacy.session.v1", session:{createdAt}}`.
/// `createdAt` is the composer's **creation time** (epoch millis), i.e.
/// session-level, not per-message — so this is `Inferred`, never `Exact`.
fn cursor_time(value: &serde_json::Value) -> LineTime {
    let created = value
        .get("session")
        .and_then(|session| session.get("value"))
        .and_then(|v| v.get("createdAt"))
        // Legacy shape carries createdAt directly on the session object.
        .or_else(|| value.get("session").and_then(|s| s.get("createdAt")));
    match created {
        Some(raw) => match one_ts_value(raw) {
            Some((t, rfc3339)) => local_time(t, t, rfc3339),
            None => LineTime::Invalid,
        },
        None => LineTime::Absent,
    }
}

/// gemini-cli: one whole session per line, a JSON object
/// `{startTime, lastUpdated, messages:[{timestamp,...}]}`. `startTime`,
/// `lastUpdated` and every `messages[].timestamp` are RFC 3339 strings
/// (unambiguous → `Exact`). Message times are the real conversation span; the
/// top-level start/last-update fields are the session-level fallback.
fn gemini_time(value: &serde_json::Value) -> LineTime {
    let messages = value
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten();
    let (mf, ml, mrfc, msaw, minvalid) = collect_span(messages, "timestamp");
    if let (Some(f), Some(l)) = (mf, ml) {
        return local_time(f, l, mrfc);
    }
    let start = value.get("startTime").and_then(one_ts_value);
    let last_updated = value.get("lastUpdated").and_then(one_ts_value);
    let has_top = value.get("startTime").is_some() || value.get("lastUpdated").is_some();
    let invalid = has_top && (start.is_none() || last_updated.is_none());
    let rfc =
        // reason: absent startTime/lastUpdated means "no timestamp", hence not
        // RFC3339; a field that exists but fails to parse is flagged separately
        // by the `invalid` tally, so false is an honest absence, not a lie.
        start.map(|(_, r)| r).unwrap_or(false) || last_updated.map(|(_, r)| r).unwrap_or(false);
    let sf = start.map(|(t, _)| t);
    let sl = last_updated.map(|(t, _)| t);
    match (sf, sl) {
        (Some(f), Some(l)) => local_time(f, l.max(f), rfc),
        (Some(f), None) => local_time(f, f, rfc),
        (None, Some(l)) => local_time(l, l, rfc),
        (None, None) => {
            if msaw || has_top || minvalid || invalid {
                LineTime::Invalid
            } else {
                LineTime::Absent
            }
        }
    }
}

/// Google Antigravity transcript events carry an RFC 3339 `created_at` field.
/// Each line is one event, so the session fold derives its span from those
/// observed per-event timestamps.
fn antigravity_time(value: &serde_json::Value) -> LineTime {
    match value.get("created_at").and_then(one_ts_value) {
        Some((time, rfc3339)) => local_time(time, time, rfc3339),
        None if value.get("created_at").is_some() => LineTime::Invalid,
        None => LineTime::Absent,
    }
}

/// Plausible window for a *conversation* timestamp, in unix seconds. Anything
/// outside it is suspicious — treating a value in this range as seconds is
/// never silently clamped; it is reported as Unknown/Inferred instead.
const MIN_PLAUSIBLE_SECONDS: i64 = 1_577_836_800; // 2020-01-01
const MAX_PLAUSIBLE_SECONDS: i64 = 4_102_444_800; // 2100-01-01

/// Interpret a numeric epoch as seconds (or, when its magnitude says so,
/// milliseconds → seconds). Returns `None` when out of the plausible window.
fn numeric_value_seconds(n: &serde_json::Number) -> Option<i64> {
    if let Some(v) = n.as_i64() {
        return plausible_seconds(v);
    }
    if let Some(v) = n.as_f64() {
        if v.is_finite() && v.fract() == 0.0 && v >= i64::MIN as f64 && v <= i64::MAX as f64 {
            return plausible_seconds(v as i64);
        }
    }
    None
}

/// Convert a raw numeric epoch to unix seconds, inferring the unit from its
/// magnitude. `None` when the value falls in neither plausible range.
fn parse_numeric_epoch(raw: &str) -> Option<i64> {
    let n: i64 = raw.trim().parse().ok()?;
    plausible_seconds(n)
}

fn plausible_seconds(v: i64) -> Option<i64> {
    if (MIN_PLAUSIBLE_SECONDS..=MAX_PLAUSIBLE_SECONDS).contains(&v) {
        Some(v)
    } else if (MIN_PLAUSIBLE_SECONDS * 1000..=MAX_PLAUSIBLE_SECONDS * 1000).contains(&v) {
        Some(v / 1000)
    } else if (MIN_PLAUSIBLE_SECONDS * 1_000_000..=MAX_PLAUSIBLE_SECONDS * 1_000_000).contains(&v) {
        // Microsecond epochs (perplexity's `*_us` fields): 1 s = 1_000_000 µs.
        // The value is an absolute instant, so this stays an honest inference
        // from magnitude, labelled `Inferred` by the caller.
        Some(v / 1_000_000)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Web chat harnesses (browser-extension captures)
// ---------------------------------------------------------------------------
//
// A web chat session is archived as an inbox bundle line: the platform's own
// identity envelope under top-level keys, with the authoritative raw HTTP body
// as a **string** in `raw.text` (see `inbox.rs`). So every field name below is
// read out of the platform's real wire shape — the shapes the extension's own
// parsers already read (`lib/backfill/enumerate.ts`, `lib/gemini-rpc.ts`,
// `lib/contract.ts`) and the real-sanitized competitor fixtures those
// projects publish themselves.
//
// Zone discipline: nothing is read as UTC by assumption. An RFC 3339 string
// keeps the offset it was written with (`source_zone` records it); a numeric
// field is a Unix epoch — an absolute instant whose own zone is UTC — so it is
// recorded as `"UTC"` and never shifted. Only a naive date-time **string** with
// no offset could carry a source-zone label other than its own, and such a
// field would have to say so here; no such field is read today.

/// How one raw JSON value becomes unix seconds for a web harness.
#[derive(Clone, Copy)]
enum EpochMode {
    /// Absolute epoch seconds (or millis). A numeric epoch is an instant in
    /// UTC; its own zone is recorded as `"UTC"`, and the value is never shifted.
    Absolute,
    /// RFC 3339 string; the string's own offset is recorded.
    Rfc3339,
}

/// A folded set of stamps for one candidate source (messages or list).
#[derive(Default, Clone)]
struct Stamps {
    first: Option<i64>,
    last: Option<i64>,
    exact: bool,
    saw: bool,
    invalid: bool,
    zone: Option<String>,
}

impl Stamps {
    fn add(&mut self, raw: &serde_json::Value, mode: EpochMode) {
        self.saw = true;
        match stamp_from_value(raw, mode) {
            Some((t, exact, zone)) => {
                self.exact |= exact;
                self.first = Some(self.first.map_or(t, |old| old.min(t)));
                self.last = Some(self.last.map_or(t, |old| old.max(t)));
                if self.zone.is_none() {
                    self.zone = zone;
                }
            }
            None => self.invalid = true,
        }
    }

    /// Fold the field `field` of every object in `objects`.
    fn add_field<'a, I>(&mut self, objects: I, field: &str, mode: EpochMode)
    where
        I: Iterator<Item = &'a serde_json::Value>,
    {
        for object in objects {
            if let Some(raw) = object.get(field) {
                self.add(raw, mode);
            }
        }
    }

    fn has(&self) -> bool {
        self.first.is_some()
    }
}

/// What one web payload yielded, before it is turned into a [`LineTime`].
enum WebSpan {
    /// The span came from **messages** in the stored payload.
    Messages(Stamps),
    /// No messages; the span is the conversation list's update time only.
    List(Stamps),
    /// Payload present, but carries no time field of the expected name.
    Absent,
    /// A time field is present but unreadable / out of the plausible window.
    Invalid,
}

impl WebSpan {
    fn into_line_time(self) -> LineTime {
        let (stamps, basis) = match self {
            WebSpan::Messages(s) => (s, Basis::Messages),
            WebSpan::List(s) => (s, Basis::List),
            WebSpan::Absent => return LineTime::Absent,
            WebSpan::Invalid => return LineTime::Invalid,
        };
        match (stamps.first, stamps.last) {
            (Some(f), Some(l)) => LineTime::Time {
                first: f,
                last: l.max(f),
                rfc3339: stamps.exact,
                basis,
                zone: stamps.zone,
            },
            _ => {
                if stamps.saw || stamps.invalid {
                    LineTime::Invalid
                } else {
                    LineTime::Absent
                }
            }
        }
    }
}

/// Prefer messages; fall back to the list's update time; otherwise say which
/// failure it was. Never merges the two: a list-only span stays `ListUpdated`.
fn classify(msg: Stamps, list: Stamps) -> WebSpan {
    if msg.has() {
        return WebSpan::Messages(msg);
    }
    if list.has() {
        return WebSpan::List(list);
    }
    if msg.saw || msg.invalid || list.saw || list.invalid {
        WebSpan::Invalid
    } else {
        WebSpan::Absent
    }
}

/// Parse one raw value into `(unix seconds, exact, zone)` per its mode.
fn stamp_from_value(
    raw: &serde_json::Value,
    mode: EpochMode,
) -> Option<(i64, bool, Option<String>)> {
    match mode {
        EpochMode::Rfc3339 => {
            let text = raw.as_str()?;
            parse_rfc3339_zoned(text).map(|(t, zone)| (t, true, zone))
        }
        EpochMode::Absolute => {
            web_numeric_seconds(raw).map(|t| (t, false, Some("UTC".to_string())))
        }
    }
}

/// Numeric epoch for a web field: number (may be a float epoch second) or a
/// numeric string. Unit inferred by magnitude, same window as the rest.
fn web_numeric_seconds(raw: &serde_json::Value) -> Option<i64> {
    match raw {
        serde_json::Value::Number(n) => {
            if let Some(v) = n.as_i64() {
                plausible_seconds(v)
            } else {
                let v = n.as_f64()?;
                if v.is_finite() {
                    plausible_seconds(v.trunc() as i64)
                } else {
                    None
                }
            }
        }
        serde_json::Value::String(s) => parse_numeric_epoch(s),
        _ => None,
    }
}

/// The label a web chat harness's own archived body carries for the
/// conversation, if it carries one.
///
/// The label is the conversation-level field — chatgpt `title`, claude `name` —
/// of the body that carries *this conversation's* own record, and it is read
/// from the same body the time pass reads the conversation's own metadata from
/// (the design's "the list title is the `ListUpdated` row's metadata from the
/// same source"). Two shapes are that body, and [`web_span`] decides both:
///
/// * **the conversation's detail body** — the capture contract requires
///   `mapping` + `current_node` of a chatgpt body (`contract.ts:488-511`) and
///   `chat_messages` of a claude body (`contract.ts:561-584`), so a body
///   carrying the same marker is this conversation's record;
/// * **this conversation's own metadata record** — a body the time pass reads
///   as [`WebSpan::List`], i.e. one written with *one* conversation's
///   conversation-level update time and no messages. Every harness's list
///   branch reads that time from a single record's own fields
///   (`claude_span`'s one-item page above all, whose reason is exactly this:
///   "only a single-item page is unambiguous enough to attribute to this
///   session"), so the label and the `ListUpdated` time it produces are one
///   reading of one body.
///
/// What neither shape admits is a **page of several conversations**: every
/// list branch requires the single-record form, so a sidebar page filed under a
/// session still labels nothing rather than lending one entry's title to
/// whatever session it was filed under.
///
/// A body with no such field, or one whose field is empty or not a string,
/// yields `None`: the row then records
/// [`SessionTitle::NoLabelRecorded`], never an empty string and never the head
/// of the conversation.
fn web_title(harness: &str, line: &serde_json::Value) -> Option<String> {
    let raw_text = line
        .get("raw")
        .and_then(|raw| raw.get("text"))
        .and_then(serde_json::Value::as_str);
    let owned;
    let body: &serde_json::Value = match raw_text {
        Some(text) => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(value) => {
                owned = value;
                &owned
            }
            // An unparsable body labels nothing. Its bytes are still counted by
            // the time pass, which is the pass that has to say so.
            Err(_) => return None,
        },
        None => line,
    };
    let (marker, field) = match harness {
        "chatgpt" => ("mapping", "title"),
        "claude" => ("chat_messages", "name"),
        // Every other harness's label field is unknown to us. Not guessed, and
        // not read from whichever key looks plausible.
        _ => return None,
    };
    // The record the label is read from: the body itself, or — when the body is
    // a one-row page — that row, which is the same record the time pass reads
    // the conversation's own list-level metadata from.
    let record = match body {
        serde_json::Value::Array(items) if items.len() == 1 => &items[0],
        _ => body,
    };
    // Two shapes are this conversation's own record: the detail body, whose
    // marker the capture contract requires, and the single metadata record whose
    // span the time pass reads as `List`.
    let detail = record.get(marker).is_some();
    if !detail && !matches!(web_span(harness, body), WebSpan::List(_)) {
        return None;
    }
    record
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
}

/// Read a web chat harness's line into a [`LineTime`], unwrapping the inbox
/// bundle's `raw.text` first.
fn web_time(harness: &str, line: &serde_json::Value) -> LineTime {
    let raw_text = line
        .get("raw")
        .and_then(|raw| raw.get("text"))
        .and_then(serde_json::Value::as_str);

    // Gemini's raw body is a `)]}'`-guarded batchexecute stream, not JSON, so it
    // is handed the text rather than a parsed document.
    if harness == "gemini" {
        return gemini_web_time(line, raw_text);
    }

    let owned;
    let payload: &serde_json::Value = match raw_text {
        Some(text) => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(value) => {
                owned = value;
                &owned
            }
            Err(_) => return LineTime::Invalid,
        },
        None => line,
    };

    web_span(harness, payload).into_line_time()
}

/// The span one web harness's JSON body yields. One dispatcher, because two
/// readers ask the same question of the same body: [`web_time`] reads the time
/// from it, and [`web_title`] reads the label only from a body this returns
/// [`WebSpan::List`] for — the label and the `ListUpdated` time are one
/// conversation-level metadata record, so they are read from one body.
fn web_span(harness: &str, payload: &serde_json::Value) -> WebSpan {
    match harness {
        "chatgpt" => chatgpt_span(payload),
        "deepseek" => deepseek_span(payload),
        "claude" => claude_span(payload),
        "grok" => grok_span(payload),
        "perplexity" => perplexity_span(payload),
        "kimi" => kimi_span(payload),
        _ => WebSpan::Absent,
    }
}

/// Iterate `payload[path]` as an array of objects, following `.` separators.
fn array_at<'a>(payload: &'a serde_json::Value, path: &str) -> Option<&'a Vec<serde_json::Value>> {
    let mut current = payload;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    current.as_array()
}

/// ChatGPT: detail body `{create_time, update_time, mapping:{<id>:{message:
/// {create_time}}}}`. `create_time` is a (possibly fractional) epoch second.
fn chatgpt_span(payload: &serde_json::Value) -> WebSpan {
    let mut msg = Stamps::default();
    if let Some(mapping) = payload
        .get("mapping")
        .and_then(serde_json::Value::as_object)
    {
        for node in mapping.values() {
            if let Some(ct) = node.get("message").and_then(|m| m.get("create_time")) {
                msg.add(ct, EpochMode::Absolute);
            }
        }
    }
    let mut list = Stamps::default();
    for field in ["create_time", "update_time"] {
        if let Some(raw) = payload.get(field) {
            list.add(raw, EpochMode::Absolute);
        }
    }
    classify(msg, list)
}

/// DeepSeek: detail `{data:{biz_data:{chat_messages:[{inserted_at}],
/// chat_session:{inserted_at,updated_at}}}}`; list
/// `{data:{biz_data:{chat_sessions:[{updated_at}]}}}`. `inserted_at`/`updated_at`
/// are Unix epochs (absolute instants), never shifted.
fn deepseek_span(payload: &serde_json::Value) -> WebSpan {
    let biz = payload.pointer("/data/biz_data");
    let mut msg = Stamps::default();
    if let Some(messages) = biz
        .and_then(|b| b.get("chat_messages"))
        .and_then(serde_json::Value::as_array)
    {
        msg.add_field(messages.iter(), "inserted_at", EpochMode::Absolute);
    }
    if msg.has() {
        return WebSpan::Messages(msg);
    }
    // List-only: the interval is the session's update time as a point.
    let mut list = Stamps::default();
    if let Some(session) = biz.and_then(|b| b.get("chat_session")) {
        if let Some(raw) = session.get("updated_at") {
            list.add(raw, EpochMode::Absolute);
        }
    }
    // A list body stored under one conversation: only a single-item page is
    // unambiguous enough to attribute to this session.
    if let Some(sessions) = biz
        .and_then(|b| b.get("chat_sessions"))
        .and_then(serde_json::Value::as_array)
    {
        if sessions.len() == 1 {
            list.add_field(sessions.iter(), "updated_at", EpochMode::Absolute);
        }
    }
    classify(msg, list)
}

/// claude.ai: detail `{created_at, updated_at, chat_messages:[{created_at,
/// updated_at}]}` (RFC 3339, `Z`); list `[{uuid, created_at, updated_at}]`.
fn claude_span(payload: &serde_json::Value) -> WebSpan {
    let mut msg = Stamps::default();
    if let Some(messages) = payload
        .get("chat_messages")
        .and_then(serde_json::Value::as_array)
    {
        for field in ["created_at", "updated_at"] {
            msg.add_field(messages.iter(), field, EpochMode::Rfc3339);
        }
    }
    if msg.has() {
        return WebSpan::Messages(msg);
    }
    // List-only: the interval is the list's **update** time as a point, never a
    // created..updated span. `created_at` is deliberately not folded in.
    let mut list = Stamps::default();
    if let Some(raw) = payload.get("updated_at") {
        list.add(raw, EpochMode::Rfc3339);
    }
    if let Some(items) = payload.as_array() {
        if items.len() == 1 {
            if let Some(raw) = items[0].get("updated_at") {
                list.add(raw, EpochMode::Rfc3339);
            }
        }
    }
    classify(msg, list)
}

/// Grok: detail `{responses:[{createTime}]}` (extension's measured shape) or the
/// competitor fixture's `{conversation_v2:{conversation:{createTime,modifyTime}},
/// load_responses:{responses:[{createTime}]}}`; list `{conversations:[{
/// createTime,modifyTime}]}`. RFC 3339 strings.
fn grok_span(payload: &serde_json::Value) -> WebSpan {
    let mut msg = Stamps::default();
    for path in ["responses", "load_responses.responses"] {
        if let Some(items) = array_at(payload, path) {
            msg.add_field(items.iter(), "createTime", EpochMode::Rfc3339);
        }
    }
    if msg.has() {
        return WebSpan::Messages(msg);
    }
    // List-only: the interval is the conversation's modify time as a point.
    let mut list = Stamps::default();
    if let Some(conversation) = payload.pointer("/conversation_v2/conversation") {
        if let Some(raw) = conversation.get("modifyTime") {
            list.add(raw, EpochMode::Rfc3339);
        }
    }
    if let Some(items) = payload
        .get("conversations")
        .and_then(serde_json::Value::as_array)
    {
        if items.len() == 1 {
            if let Some(raw) = items[0].get("modifyTime") {
                list.add(raw, EpochMode::Rfc3339);
            }
        }
    }
    classify(msg, list)
}

/// Perplexity: content response `{entries:[{updated_datetime}]}`; list is a
/// top-level array whose items carry `last_query_datetime` (probe-observed).
/// RFC 3339 strings.
fn perplexity_span(payload: &serde_json::Value) -> WebSpan {
    let mut msg = Stamps::default();
    if let Some(entries) = payload.get("entries").and_then(serde_json::Value::as_array) {
        msg.add_field(entries.iter(), "updated_datetime", EpochMode::Rfc3339);
        // `updated_datetime` is written without an offset (naive), which zone
        // discipline refuses to read as UTC. The same entries also carry the
        // absolute microsecond epochs `created_us` / `updated_us` — read those
        // so the entry still gets a real instant instead of being lost.
        for field in ["updated_us", "created_us"] {
            msg.add_field(entries.iter(), field, EpochMode::Absolute);
        }
    }
    if msg.has() {
        return WebSpan::Messages(msg);
    }
    let mut list = Stamps::default();
    if let Some(items) = payload.as_array() {
        if items.len() == 1 {
            for field in ["last_query_datetime", "updated_datetime"] {
                list.add_field(items.iter(), field, EpochMode::Rfc3339);
            }
        }
    }
    classify(msg, list)
}

/// Kimi: detail `{messages:[{createTime,updateTime}]}`; feed/list
/// `{items:[{chat:{id,createTime,updateTime}}]}` or `{chat:{...}}`. RFC 3339.
fn kimi_span(payload: &serde_json::Value) -> WebSpan {
    let mut msg = Stamps::default();
    if let Some(messages) = payload
        .get("messages")
        .and_then(serde_json::Value::as_array)
    {
        for field in ["createTime", "updateTime"] {
            msg.add_field(messages.iter(), field, EpochMode::Rfc3339);
        }
    }
    if msg.has() {
        return WebSpan::Messages(msg);
    }
    // List-only: the interval is the chat's update time as a point.
    let mut list = Stamps::default();
    if let Some(chat) = payload.get("chat") {
        if let Some(raw) = chat.get("updateTime") {
            list.add(raw, EpochMode::Rfc3339);
        }
    }
    if let Some(items) = payload.get("items").and_then(serde_json::Value::as_array) {
        if items.len() == 1 {
            if let Some(chat) = items[0].get("chat") {
                if let Some(raw) = chat.get("updateTime") {
                    list.add(raw, EpochMode::Rfc3339);
                }
            }
        }
    }
    classify(msg, list)
}

/// Gemini web: the archived artifact is a `{rpcid, conversationId, pages}`
/// bundle (lib/gemini-rpc.ts:817) or a bare page string. Each page is a
/// `)]}'`-guarded batchexecute stream; a detail `hNvQHb` document holds turns at
/// `payload[0]` with the exchange timestamp at `turn[4] = [seconds, nanos]`, and
/// a list `MaZiqc` document holds items at `payload[2]` with `item[5] =
/// [seconds, nanos]`. Epoch seconds (absolute).
fn gemini_web_time(line: &serde_json::Value, raw_text: Option<&str>) -> LineTime {
    let mut msg = Stamps::default();
    let mut list = Stamps::default();
    match raw_text {
        Some(text) => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(value) => gemini_collect(&value, &mut msg, &mut list),
            Err(_) => gemini_scan_text(text, &mut msg, &mut list),
        },
        None => gemini_collect(line, &mut msg, &mut list),
    }
    classify(msg, list).into_line_time()
}

/// Collect from a gemini bundle object, a bare page string, or a decoded array.
fn gemini_collect(value: &serde_json::Value, msg: &mut Stamps, list: &mut Stamps) {
    if let Some(pages) = value.get("pages").and_then(serde_json::Value::as_array) {
        for page in pages {
            if let Some(text) = page.as_str() {
                gemini_scan_text(text, msg, list);
            }
        }
    } else if let Some(text) = value.as_str() {
        gemini_scan_text(text, msg, list);
    }
}

/// Decode one batchexecute page and fold its turn / item timestamps.
fn gemini_scan_text(text: &str, msg: &mut Stamps, list: &mut Stamps) {
    let body = text.strip_prefix(")]}'").unwrap_or(text);
    for frame in parse_batchexecute_frames(body) {
        let serde_json::Value::Array(entries) = frame else {
            continue;
        };
        for entry in entries {
            let serde_json::Value::Array(entry) = entry else {
                continue;
            };
            if entry.first().and_then(serde_json::Value::as_str) != Some("wrb.fr") {
                continue;
            }
            let Some(inner) = entry.get(2).and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Ok(payload) = serde_json::from_str::<serde_json::Value>(inner) else {
                continue;
            };
            // Detail: payload[0] = turns, turn[4] = [seconds, nanos].
            if let Some(turns) = payload.get(0).and_then(serde_json::Value::as_array) {
                for turn in turns {
                    if let Some(raw) = turn.get(4).and_then(tuple_seconds) {
                        msg.add(&serde_json::Value::Number(raw.into()), EpochMode::Absolute);
                    }
                }
            }
            // List: payload[2] = items, item[5] = [seconds, nanos].
            if let Some(items) = payload.get(2).and_then(serde_json::Value::as_array) {
                for item in items {
                    if let Some(raw) = item.get(5).and_then(tuple_seconds) {
                        list.add(&serde_json::Value::Number(raw.into()), EpochMode::Absolute);
                    }
                }
            }
        }
    }
}

/// `[seconds, nanos]` (or a bare number) → seconds.
fn tuple_seconds(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Array(pair) => pair.first().and_then(serde_json::Value::as_i64),
        serde_json::Value::Number(_) => value.as_i64(),
        _ => None,
    }
}

/// Split a batchexecute body into its length-prefixed JSON frames. Mirrors the
/// extension's own `walkFrames` (`apps/extension/lib/gemini-rpc.ts`): a line
/// that is all digits declares the **UTF-16 code-unit** length of the frame
/// that follows (measured on the one real captured body: declared = JSON line
/// + 2 units); anything else is taken as a one-line frame.
///
/// The declared length is checked, not trusted. Its slice is used only when it
/// lands on a frame boundary and parses as JSON; otherwise the newline-
/// delimited line is used. Because a Rust `&str` is indexed in bytes while the
/// prefix counts UTF-16 code units, the count is first translated to a byte
/// offset that is guaranteed to be a character boundary — a declared length
/// that would land inside a character (or past the end) simply fails the check
/// and falls back, so no input can panic.
fn parse_batchexecute_frames(body: &str) -> Vec<serde_json::Value> {
    let mut frames = Vec::new();
    let mut pos = 0usize;
    while pos < body.len() {
        // Leading separators between frames are not part of any chunk.
        while pos < body.len() && is_separator_at(body, pos) {
            pos += char_len_at(body, pos);
        }
        if pos >= body.len() {
            break;
        }
        let line_end = body[pos..].find('\n').map_or(body.len(), |i| pos + i);
        let header = body[pos..line_end].trim();
        let declared = if !header.is_empty() && header.bytes().all(|b| b.is_ascii_digit()) {
            header.parse::<usize>().ok()
        } else {
            None
        };

        // No length line: the line itself has to be the chunk.
        let Some(declared) = declared else {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body[pos..line_end]) {
                frames.push(value);
            }
            pos = next_line_start(body, line_end);
            continue;
        };

        let start = next_line_start(body, line_end);
        if start >= body.len() {
            // A length line with nothing after it declares a chunk that is not
            // there; move past it rather than inventing a frame.
            break;
        }
        let line_stop = body[start..].find('\n').map_or(body.len(), |i| start + i);

        // A zero-length prefix can never be a chunk; step past the line so the
        // walk terminates instead of re-reading the same position.
        if declared == 0 {
            pos = next_line_start(body, line_stop);
            continue;
        }

        // Try the declared slice, interpreted in UTF-16 code units. The
        // translation returns `None` when the count runs past the end or would
        // land inside a character, in which case the declared slice is not
        // usable and the newline line below is used instead.
        let declared_end = byte_index_after_utf16(&body[start..], declared).map(|off| start + off);
        if let Some(end) = declared_end {
            if end >= body.len() || is_separator_at(body, end) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body[start..end]) {
                    frames.push(value);
                    pos = end;
                    continue;
                }
            }
        }

        // Fallback: the newline-delimited line, exactly as the extension does
        // when the declared slice is not a usable frame.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body[start..line_stop]) {
            frames.push(value);
        }
        pos = next_line_start(body, line_stop);
    }
    frames
}

/// The byte index of the newline ending a line that ends at `line_end`, or the
/// end of `body` when that line is the last one.
fn next_line_start(body: &str, line_end: usize) -> usize {
    if line_end < body.len() {
        line_end + 1
    } else {
        body.len()
    }
}

/// The UTF-8 length of the character starting at `byte`, or `1` if `byte` is not
/// a character boundary (so a caller only ever advances past a whole character).
fn char_len_at(s: &str, byte: usize) -> usize {
    s.get(byte..)
        .and_then(|rest| rest.chars().next())
        .map_or(1, |c| c.len_utf8())
}

/// Whether the character at `byte` is a frame separator. `false` when `byte` is
/// at or past the end, or not a character boundary.
fn is_separator_at(s: &str, byte: usize) -> bool {
    matches!(
        s.get(byte..).and_then(|rest| rest.chars().next()),
        Some('\n' | '\r' | '\t' | ' ')
    )
}

/// The byte index in `s` that sits `units` UTF-16 code units in, or `None` when
/// that count runs past the end of `s` or lands between the two halves of a
/// surrogate pair (i.e. at no byte boundary). The returned index is always a
/// valid byte boundary, so a caller can slice `s` there without panicking.
fn byte_index_after_utf16(s: &str, units: usize) -> Option<usize> {
    if units == 0 {
        return Some(0);
    }
    let mut remaining = units;
    for (byte, ch) in s.char_indices() {
        let width = ch.len_utf16();
        if width > remaining {
            // A cut inside a supplementary character (width 2, one unit left):
            // there is no byte boundary here.
            return None;
        }
        remaining -= width;
        if remaining == 0 {
            return Some(byte + ch.len_utf8());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// RFC 3339 parsing (no external time crate — this crate adds no dependencies)
// ---------------------------------------------------------------------------

/// Parse an RFC 3339 timestamp into unix seconds (UTC). Accepts `Z` and
/// `±HH:MM` offsets, and optional fractional seconds (precision beyond the
/// second is discarded — we report whole seconds). `None` on any malformed /
/// out-of-range input.
fn parse_rfc3339(s: &str) -> Option<i64> {
    parse_rfc3339_zoned(s).map(|(t, _)| t)
}

/// As [`parse_rfc3339`], but also reporting the source zone the string was
/// written in: `"UTC"` for a `Z` suffix, or the literal `+HH:MM` / `-HH:MM`
/// offset. The offset is already applied to the returned unix seconds; the
/// label is what a consumer needs to know which zone the source used.
fn parse_rfc3339_zoned(s: &str) -> Option<(i64, Option<String>)> {
    let b = s.as_bytes();
    if b.len() < 20 {
        return None;
    }
    let year = parse_digits(b, 0, 4)?;
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    let month = parse_digits(b, 5, 2)?;
    let day = parse_digits(b, 8, 2)?;
    if b.get(10) != Some(&b'T') {
        return None;
    }
    let hour = parse_digits(b, 11, 2)?;
    if b.get(13) != Some(&b':') {
        return None;
    }
    let minute = parse_digits(b, 14, 2)?;
    if b.get(16) != Some(&b':') {
        return None;
    }
    let second = parse_digits(b, 17, 2)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut idx = 19;
    // Optional fractional seconds: consume the digits, ignore sub-second detail.
    if b.get(idx) == Some(&b'.') {
        idx += 1;
        while b.get(idx).is_some_and(|c| c.is_ascii_digit()) {
            idx += 1;
        }
    }

    let (second, offset_seconds, zone) = match b.get(idx) {
        Some(&b'Z') => (second, 0, Some("UTC".to_string())),
        Some(&b'+') | Some(&b'-') => {
            if b.len() < idx + 6 {
                return None;
            }
            let oh = parse_digits(b, idx + 1, 2)?;
            if b.get(idx + 3) != Some(&b':') {
                return None;
            }
            let om = parse_digits(b, idx + 4, 2)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let sign: i64 = if b[idx] == b'+' { 1 } else { -1 };
            let label = format!("{}{oh:02}:{om:02}", b[idx] as char);
            (second, sign * (oh * 3600 + om * 60), Some(label))
        }
        _ => return None,
    };

    let days = days_from_civil(year as i64, month as u32, day as u32)?;
    Some((
        days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second - offset_seconds,
        zone,
    ))
}

/// Read `len` ASCII digits starting at `start`. `None` if they are not digits.
fn parse_digits(b: &[u8], start: usize, len: usize) -> Option<i64> {
    if start + len > b.len() {
        return None;
    }
    let mut v: i64 = 0;
    for &c in &b[start..start + len] {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as i64;
    }
    Some(v)
}

/// Days since 1970-01-01 for a civil date, using Howard Hinnant's
/// `days_from_civil`. Validated ranges are enforced by the caller, so any day
/// in 1..=31 yields a date that always maps to a well-defined day count.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if month == 0 || day == 0 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((month as i64) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// The 4D dimensions this session's own records state, read per harness.
///
/// This is the harness's side of [`crate::provenance::SessionProvenance`]: what
/// the archived records themselves say about where the session came from, as
/// opposed to what the collection path or the registry already recorded. Seven
/// harnesses have a reader today — `claude-code` (its working directory and
/// owning tenancy), `gemini-cli` (its `projectHash`), `opencode` (its
/// directory, project, and archive fact), `openclaw` (its `agent_id`, plus
/// the archived status of a cold snapshot), `grok` (its CLI `session_docs`
/// row's `cwd`), `codex` (its `session_meta` record's working directory
/// and repository), and `continue` (the workspace directory its session
/// file names); every other harness answers with the empty set, which is
/// the honest answer for a dimension nothing was read from — and never a
/// value inferred from the harness name.
///
/// A record that does not parse is skipped, exactly as it is everywhere else in
/// this module: an unreadable line is not evidence about a dimension.
fn session_dimensions(harness: &str, lines: &[&str]) -> crate::provenance::SessionProvenance {
    let mut dimensions = crate::provenance::SessionProvenance::default();
    match harness {
        "claude-code" => {
            for line in lines {
                let Ok(record) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                fold_claude_code_dimensions(&mut dimensions, &record);
            }
        }
        "opencode" => {
            // The same JSONL-shaped body as Claude Code's, one sealed export
            // envelope per line, so the same per-line loop — only the fold the
            // lines go through differs.
            for line in lines {
                let Ok(record) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                fold_opencode_dimensions(&mut dimensions, &record);
            }
        }
        "gemini-cli" => {
            // gemini-cli writes a whole session as one **pretty-printed JSON
            // document**, so — exactly as for the session span in
            // `analyze_session` — its lines do not parse individually. Each
            // line is tried first (a JSONL-shaped body parses per line), and
            // when no line yielded a project hash the body is re-read as the
            // stream of documents it is, the same framing `normalize` applies.
            let mut saw_hash = false;
            for line in lines {
                let Ok(record) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                if fold_gemini_cli_dimensions(&mut dimensions, &record) {
                    saw_hash = true;
                }
            }
            if !saw_hash {
                let body = lines.join("\n");
                let documents = serde_json::Deserializer::from_str(&body)
                    .into_iter::<serde_json::Value>()
                    .collect::<Result<Vec<_>, _>>();
                if let Ok(documents) = documents {
                    for document in &documents {
                        fold_gemini_cli_dimensions(&mut dimensions, document);
                    }
                }
            }
        }
        "grok" => {
            for line in lines {
                let Ok(record) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                fold_grok_dimensions(&mut dimensions, &record);
            }
        }
        "codex" => {
            // A codex rollout states its session facts on the `session_meta`
            // record that opens it, and each turn restates the directory it
            // started in on its `turn_context` record. The session's own
            // record is the primary source for both dimensions; a turn
            // context is the fallback for `cwd` alone, read only when the
            // session's own record stated no directory — a session that
            // already said where it ran has answered the question a turn
            // context would.
            let mut session_stated_cwd = false;
            for line in lines {
                let Ok(record) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                session_stated_cwd |= fold_codex_session_meta(&mut dimensions, &record);
            }
            if !session_stated_cwd {
                for line in lines {
                    let Ok(record) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                        continue;
                    };
                    fold_codex_turn_context_cwd(&mut dimensions, &record);
                }
            }
        }
        "openclaw" => {
            for line in lines {
                let Ok(record) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                fold_openclaw_dimensions(&mut dimensions, &record);
            }
        }
        "continue" => {
            // Continue writes one session as one **pretty-printed JSON file**
            // (`sessions/<id>.json`), so its lines do not parse individually —
            // the same framing problem gemini-cli has, solved the same way: try
            // each line first, then re-read the body as the document it is.
            let mut saw_workspace = false;
            for line in lines {
                let Ok(record) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                if fold_continue_dimensions(&mut dimensions, &record) {
                    saw_workspace = true;
                }
            }
            if !saw_workspace {
                let body = lines.join("\n");
                let documents = serde_json::Deserializer::from_str(&body)
                    .into_iter::<serde_json::Value>()
                    .collect::<Result<Vec<_>, _>>();
                if let Ok(documents) = documents {
                    for document in &documents {
                        fold_continue_dimensions(&mut dimensions, document);
                    }
                }
            }
        }
        _ => {}
    }
    dimensions
}

/// Claude Code writes two provenance facts on the **top level** of a transcript
/// record, beside the conversation itself.
///
/// `cwd` is the working directory the session ran in, and it is a **path, not a
/// repository identity**: the project directory a transcript lives in is a
/// sanitized spelling of a path like this one rather than an entity the harness
/// recorded, so it never becomes a `container` (the `cwd ≠ repo identity` rule).
/// Every distinct directory is kept, because a session that `cd`s into a
/// subdirectory recorded each place it ran and the last one alone would report
/// where the conversation ended as where it ran.
///
/// `ownerOrganizationUuid` and `ownerAccountUuid` name the tenancy the
/// conversation was authored under, and the organization wins per record: two
/// accounts can be members of one organization, so recording the account beside
/// an organization the same record already names would file one tenancy under
/// two. The value is a namespace, never a person — two sessions by one person
/// under one organization share it, which is what makes it a tenancy rather than
/// an identity.
///
/// Neither field is required and neither is inferred: a record without them, a
/// line whose value is not a non-empty string, and a line that does not parse at
/// all each leave the dimension exactly as empty as it was — "not recorded" and
/// "not recorded yet" are the same honest answer here.
fn fold_claude_code_dimensions(
    dimensions: &mut crate::provenance::SessionProvenance,
    record: &serde_json::Value,
) {
    if let Some(cwd) = non_empty_str(record.get("cwd")) {
        dimensions.insert_cwd(cwd);
    }
    let tenant = non_empty_str(record.get("ownerOrganizationUuid"))
        .or_else(|| non_empty_str(record.get("ownerAccountUuid")));
    if let Some(tenant) = tenant {
        dimensions.insert_tenant(tenant);
    }
}

/// gemini-cli states one provenance fact on the **top level** of a session
/// document: `projectHash`, the hash of the project (working directory tree)
/// the session ran in. It is a repository/workspace identity, not a path — the
/// digest is not a directory and never becomes a `cwd` (the `cwd ≠ repo
/// identity` rule) — so it is recorded as the session's `container`.
///
/// The value must be a non-empty string, and neither field presence nor value
/// is inferred: a document without one, a value of another type, and a line
/// that does not parse at all each leave the dimension exactly as empty as it
/// was. Returns whether a hash was folded, so the caller can tell a body whose
/// lines were read from one whose lines never parsed at all.
fn fold_gemini_cli_dimensions(
    dimensions: &mut crate::provenance::SessionProvenance,
    record: &serde_json::Value,
) -> bool {
    if let Some(hash) = non_empty_str(record.get("projectHash")) {
        dimensions.insert_container(hash);
        return true;
    }
    false
}

/// opencode exports a session as one envelope that carries, beside the
/// conversation, the row its own `session` table recorded about it
/// (`sqlite_probe.rs` seals that row under the
/// `chat-stasher.opencode.session.v1` schema). Three of its fields are
/// dimension facts, and they are three different kinds of fact:
///
/// `directory` — else `path`, where the directory is null or empty — is where
/// the session ran. It is a path, so it lands in `cwd` under the same rule as
/// Claude Code's: a place the harness executed in, never a repository
/// identity. A session resumed and re-exported states each directory it ran in,
/// and every one is kept.
///
/// `project_id` is the inverse case, and the one that makes the pair a pair:
/// an opaque **foreign key** the harness itself recorded, not a path and not
/// derivable from one. It names the project the session belonged to, which is
/// what `container` exists for — and keeping it there rather than leaning on
/// the directory is the whole point of the `cwd ≠ repo identity` rule.
///
/// `time_archived` is a recorded fact, not a state to compute: the row holds
/// the millisecond moment the session was archived, or nothing. An integer
/// stamps `archived`; a null stays empty, because "not archived *yet*" and
/// "never archived" are both things the row does not say, and inferring
/// `active` would publish a lifecycle the source never recorded. Only the
/// integer shape the exporter writes is trusted — a value of another type is a
/// row this reader cannot read, not a fact it may take on faith.
///
/// Only an envelope identified by its schema is read at all: a line with a
/// `session` object that the exporter did not seal is a record nothing
/// vouches for, however much it looks like one.
fn fold_opencode_dimensions(
    dimensions: &mut crate::provenance::SessionProvenance,
    record: &serde_json::Value,
) {
    if record.get("schema").and_then(serde_json::Value::as_str)
        != Some("chat-stasher.opencode.session.v1")
    {
        return;
    }
    let Some(session) = record.get("session") else {
        return;
    };
    let cwd =
        non_empty_str(session.get("directory")).or_else(|| non_empty_str(session.get("path")));
    if let Some(cwd) = cwd {
        dimensions.insert_cwd(cwd);
    }
    if let Some(project) = non_empty_str(session.get("project_id")) {
        dimensions.insert_container(project);
    }
    if session
        .get("time_archived")
        .is_some_and(serde_json::Value::is_i64)
    {
        dimensions.insert_status("archived");
    }
}

/// `grok` is two harnesses sharing one name: the browser-extension bundle,
/// whose payload is a web conversation, and the CLI, whose archived line is one
/// `session_docs` SQLite row exported by `sqlite_probe.rs` under the
/// `chat-stasher.sqlite.session.v1` schema. Only the CLI row states a
/// dimension, and the one it states is `cwd`: the directory the session ran
/// in, a path rather than a repository identity under the same rule as Claude
/// Code's. Every distinct directory is kept, because a session resumed and
/// re-exported states each place it ran.
///
/// The row names no tenant, no project and no lifecycle state, and none is
/// inferred from the harness name — the directory alone is never promoted to a
/// `container`. The browser-extension bundle carries no `session_docs` row at
/// all and contributes nothing.
///
/// The guard is the exporter's own envelope, the schema string *and* the
/// `session_docs` table, the same pair [`grok_cli_title`] requires, so a value
/// from an unrelated object is never attributed to this session.
fn fold_grok_dimensions(
    dimensions: &mut crate::provenance::SessionProvenance,
    record: &serde_json::Value,
) {
    if record.get("schema").and_then(serde_json::Value::as_str)
        != Some("chat-stasher.sqlite.session.v1")
        || record.get("table").and_then(serde_json::Value::as_str) != Some("session_docs")
    {
        return;
    }
    let Some(session) = record.get("session") else {
        return;
    };
    if let Some(cwd) = non_empty_str(session.get("cwd")) {
        dimensions.insert_cwd(cwd);
    }
}

/// A codex rollout opens with one `session_meta` record that states the
/// session's own facts: the directory it ran in (`cwd`), the repository it
/// belonged to (`git.repository_url`), and the workspace roots the runtime
/// gave it (`runtime_workspace_roots`).
///
/// `cwd` is a path, so it lands in `cwd` under the same rule as Claude
/// Code's. `git.repository_url` is a repository identity, so it lands in
/// `container`; when the session ran outside any repository — a scratch
/// directory with no `git` — the first runtime workspace root, the
/// workspace boundary the session was given, stands in for it. The
/// directory itself never becomes a `container` (the `cwd ≠ repo identity`
/// rule): only the repository URL or a workspace root does.
///
/// Returns whether a `cwd` was observed, so the caller can tell whether the
/// `turn_context` fallback is needed.
fn fold_codex_session_meta(
    dimensions: &mut crate::provenance::SessionProvenance,
    record: &serde_json::Value,
) -> bool {
    if record.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        return false;
    }
    let Some(payload) = record.get("payload") else {
        return false;
    };
    let cwd = non_empty_str(payload.get("cwd"));
    let cwd_observed = cwd.is_some();
    if let Some(cwd) = cwd {
        dimensions.insert_cwd(cwd);
    }
    let repository_url = payload.get("git").and_then(|git| git.get("repository_url"));
    let workspace_root = payload
        .get("runtime_workspace_roots")
        .and_then(serde_json::Value::as_array)
        .and_then(|roots| roots.first());
    let container = non_empty_str(repository_url).or_else(|| non_empty_str(workspace_root));
    if let Some(container) = container {
        dimensions.insert_container(container);
    }
    cwd_observed
}

/// A codex `turn_context` record states the directory each turn started
/// in. It is the fallback for `cwd`: read only when the session's own
/// `session_meta` record stated no directory, because a session that
/// already said where it ran has answered the question a turn context
/// would. It states no container, and none is read from it.
fn fold_codex_turn_context_cwd(
    dimensions: &mut crate::provenance::SessionProvenance,
    record: &serde_json::Value,
) {
    if record.get("type").and_then(serde_json::Value::as_str) != Some("turn_context") {
        return;
    }
    let Some(payload) = record.get("payload") else {
        return;
    };
    if let Some(cwd) = non_empty_str(payload.get("cwd")) {
        dimensions.insert_cwd(cwd);
    }
}

/// OpenClaw writes two provenance facts on the **top level** of every sealed
/// export line, beside the session itself (TICKET-4D-10).
///
/// `agent_id` is the store's own `schema_meta` row: the agent every session in
/// this store ran inside, which is a container rather than a path — the
/// distinction `cwd ≠ repo identity` draws. It is stamped onto the line by the
/// reader, so the index observes it here exactly as the collect pass observed
/// it firsthand.
///
/// A line whose schema is the archive schema is the harness's own record that
/// the session was read from `session_transcript_archives` — a cold snapshot of
/// a session that no longer has a live window. That is the only place the
/// archived status is ever written, so it is the only line that records it; a
/// live window line leaves the status unobserved rather than assuming
/// "active" from the mere fact that the window exists.
///
/// Neither field is required and neither is inferred: a line without them, a
/// value that is not a non-empty string, and a line that does not parse at all
/// each leave the dimension exactly as empty as it was.
fn fold_openclaw_dimensions(
    dimensions: &mut crate::provenance::SessionProvenance,
    record: &serde_json::Value,
) {
    // Only a line sealed under one of the two OpenClaw schemas vouches for
    // what it states: everything the export writes is sealed, so a line
    // carrying neither schema is a foreign or hand-written shape whose
    // top-level `agent_id` is the same field with nothing behind it — the
    // same rule the opencode fold applies to a line the exporter did not
    // seal.
    let schema = record.get("schema").and_then(serde_json::Value::as_str);
    let archived = schema == Some(crate::sqlite_probe::OPENCLAW_ARCHIVE_SCHEMA);
    if !archived && schema != Some(crate::sqlite_probe::OPENCLAW_SESSION_SCHEMA) {
        return;
    }
    if let Some(agent_id) = non_empty_str(record.get("agent_id")) {
        dimensions.insert_container(agent_id);
    }
    if archived {
        dimensions.insert_status("archived");
    }
}

/// Continue states one provenance fact on the **top level** of its session
/// document: `workspaceDirectory`, the directory the session's workspace was
/// opened in. TICKET-4D-11 sends that one recorded field to the two dimensions
/// it answers, and they are two different questions about the same bytes: the
/// value is a filesystem path, so it is the session's `cwd` like any other path
/// here; and it is also the **workspace** Continue itself named, which is what
/// `container` is for — for this harness the workspace and its directory are one
/// recorded value, so neither dimension is derived from the other and both come
/// from the field the source wrote. Nothing else is consulted to fill a dimension
/// this field leaves empty, which is why `tenant` and `status` stay unobserved
/// for a Continue session: the measurement found no field for them.
///
/// A document that records no non-empty string under the key states nothing, so
/// both dimensions stay exactly as empty as they were. Returns whether a
/// directory was folded, so the caller can tell a body whose lines were read from
/// one whose lines never parsed at all.
fn fold_continue_dimensions(
    dimensions: &mut crate::provenance::SessionProvenance,
    record: &serde_json::Value,
) -> bool {
    let Some(workspace) = non_empty_str(record.get("workspaceDirectory")) else {
        return false;
    };
    dimensions.insert_cwd(workspace.clone());
    dimensions.insert_container(workspace);
    true
}

/// A string a record actually recorded, as opposed to one that is absent, of
/// another type, or empty. An empty string is the case this exists for: it would
/// otherwise become a dimension value that every such session shares.
fn non_empty_str(value: Option<&serde_json::Value>) -> Option<String> {
    value
        .and_then(serde_json::Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Build an [`ActivityRow`] for one session from its lines.
pub fn build_row(session_id: &str, machine: &str, harness: &str, lines: &[&str]) -> ActivityRow {
    let a = analyze_session(harness, lines);
    ActivityRow {
        session_id: session_id.to_string(),
        machine: machine.to_string(),
        harness: harness.to_string(),
        first_unix: a.first_unix,
        last_unix: a.last_unix,
        line_count: a.line_count,
        time_source: a.time_source,
        source_zone: a.source_zone,
        title: Some(a.title),
        provenance: project_provenance(lines),
        session_provenance: None,
        dimensions: session_dimensions(harness, lines),
        account_keys: account_keys(lines),
        // This function only ever sees lines, never the shard files they came
        // from, so it cannot record what it measured. A caller that read the
        // body from a stage (which is the only place the manifest is the
        // authority on it) stamps the identity itself.
        measured_body: None,
    }
}

/// The one `AccountIdSource` whose value hashes a **person's** id, and so the
/// only stored source `account_keys` turns into an account key.
///
/// The contract's enum carries a second label, and the two are not
/// interchangeable: see `account_keys` for what that one names and why it may
/// not become a key.
const PERSON_ACCOUNT_SOURCE: &str = "response-body-platform-uid";

/// W219 · Every comparable account key this session's records carry.
///
/// Read from the `account` envelope the extension wrote on each bundle
/// (`contracts/inbox.schema.json`). Only `kind: "fingerprint"` yields a key: for
/// `kind: "unknown"` the account is *not known*, and an unknown is not a value —
/// folding it in would let an unknown silently agree (or disagree) with a real
/// fingerprint, which is the failure invariant 1 forbids. A malformed envelope
/// yields nothing for the same reason a malformed line is skipped everywhere
/// else in this module: it is not evidence about an account.
///
/// 🔴 W239 · **Which fingerprint** is a second question, and the envelope answers
/// it with `source`. Only a source that names a person's id yields a key: a
/// fingerprint whose source is `request-url-organization` hashes an
/// **organization**, which two accounts can share, so a key made from it states
/// that two people are one — a positive false claim rather than an unknown one.
/// The extension no longer writes that label (`ACCOUNT_ID_SOURCES` in
/// `apps/extension/lib/contract.ts` pins today's set, and the plan table beside
/// it is why), so the bundles carrying it are ones an **older build** wrote —
/// which is precisely why this reader cannot skip the question: the archive
/// holds them, and the extension's own fix cannot reach data already written.
/// This reader is where that claim stops. An envelope whose `source` is absent,
/// or is a label the closed enum does not name, is a bundle this reader cannot
/// read rather than one it may assume holds an account.
///
/// Deduped and sorted so the row is a set, not a log: the same account captured
/// twice is one key, and the row's bytes do not depend on line order — which is
/// what lets the index stay a pure function of the session's content.
fn account_keys(lines: &[&str]) -> Vec<AccountKey> {
    let mut out: Vec<AccountKey> = Vec::new();
    for line in lines {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(account) = record.get("account").and_then(|v| v.as_object()) else {
            continue;
        };
        if account.get("kind").and_then(|k| k.as_str()) != Some("fingerprint") {
            continue;
        }
        if account.get("source").and_then(|v| v.as_str()) != Some(PERSON_ACCOUNT_SOURCE) {
            continue;
        }
        let Some(value) = account.get("value").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(salt_id) = account.get("saltId").and_then(|v| v.as_str()) else {
            continue;
        };
        // An empty string is not a key: it would make every session with an
        // empty value comparable to every other one.
        if value.is_empty() || salt_id.is_empty() {
            continue;
        }
        let key = AccountKey {
            salt_id: salt_id.to_string(),
            value: value.to_string(),
        };
        if !out.contains(&key) {
            out.push(key);
        }
    }
    out.sort();
    out
}

fn project_provenance(lines: &[&str]) -> Option<ProjectProvenance> {
    let mut captured = None;
    let mut supplement: Option<serde_json::Value> = None;
    for line in lines {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if captured.is_none()
            && record
                .get("provenance")
                .is_some_and(serde_json::Value::is_object)
        {
            captured = record.get("provenance").cloned();
        }
        let next = record
            .get("provenanceSupplement")
            .or_else(|| record.get("provenance_supplement"));
        if let Some(next) = next.filter(|v| v.is_object()) {
            let next_time = next
                .get("observedAt")
                .and_then(serde_json::Value::as_str)
                .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok());
            let prior_time = supplement
                .as_ref()
                .and_then(|v| v.get("observedAt"))
                .and_then(serde_json::Value::as_str)
                .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok());
            if supplement.is_none()
                || matches!((next_time, prior_time), (Some(next), Some(prior)) if next > prior)
                || matches!((next_time, prior_time), (Some(_), None))
                || matches!((next_time, prior_time), (None, None))
            {
                supplement = Some(next.clone());
            }
        }
    }
    if captured.is_none() && supplement.is_none() {
        return None;
    }
    let effective_project = supplement
        .as_ref()
        .and_then(|v| v.get("project"))
        .cloned()
        .or_else(|| captured.as_ref().and_then(|v| v.get("project")).cloned());
    Some(ProjectProvenance {
        captured,
        effective_project,
        supplement,
    })
}

/// Serialise one row as a single JSONL line (trailing newline included).
pub fn to_jsonl(row: &ActivityRow) -> String {
    // Cannot fail for this shape: plain strings, an Option<i64>, an int, and
    // internally-tagged enums of strings and one capped-text struct. No NaN /
    // recursion involved.
    let mut json = serde_json::to_string(row).expect("ActivityRow serializes to JSON");
    // Keep the path-fragment field consumed by older activity-index readers
    // alongside the structured W323 object. It is a fixed class label plus a
    // slash, never a source path; the structured value remains authoritative.
    if let Some(provenance) = &row.session_provenance {
        let legacy_class = match provenance.source_path_class.as_str() {
            "main" => Some("main/"),
            "subagents" => Some("subagents/"),
            _ => None,
        };
        if let Some(legacy_class) = legacy_class {
            let object_end = json
                .rfind('}')
                .expect("ActivityRow JSON serializes as an object");
            json.insert_str(
                object_end,
                &format!(",\"source_path_class\":{legacy_class:?}"),
            );
        }
    }
    json + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- reference times (all verified against Unix epoch) -----------------
    // 2025-01-15T12:34:56.789Z = 1736944496 (floor to whole second)
    const RFC_T1: &str = "2025-01-15T12:34:56.789Z";
    const T1: i64 = 1_736_944_496;
    // 2025-01-15T13:45:07Z = T1 + 4211
    const RFC_T2: &str = "2025-01-15T13:45:07Z";
    const T2: i64 = T1 + 4211;

    fn cc_user(ts: &str) -> String {
        format!(
            r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
        )
    }
    fn cc_assistant(ts: &str) -> String {
        format!(
            r#"{{"parentUuid":"u1","isMeta":null,"sessionId":"s","type":"assistant","message":{{"role":"assistant","content":"hello"}},"uuid":"u2","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
        )
    }
    fn codex(ts: &str, ty: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"{ty}","payload":{{"message":{{"role":"user","content":"hi"}}}},"cwd":"/x"}}"#
        )
    }

    // --------------------------------------------- TICKET-4D-02 · claude-code
    //
    // The two facts a Claude Code transcript states about itself: the directory
    // it ran in, and the organization (else the account) that authored it.

    /// One transcript record carrying whatever provenance the caller spells in,
    /// so each case states exactly which fields it is about.
    fn cc_with(fields: &str) -> String {
        format!(
            r#"{{"parentUuid":null,"type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":"{RFC_T1}"{fields}}}"#
        )
    }

    #[test]
    fn claude_code_records_its_cwd_and_its_organization_as_tenant() {
        let line = cc_with(r#","cwd":"/w/one","ownerOrganizationUuid":"org-fixture""#);
        let row = build_row("s", "mbp", "claude-code", &[line.as_str()]);
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
        assert_eq!(row.dimensions.tenant, ["org-fixture"]);
        assert!(
            row.dimensions.container.is_empty(),
            "a path is not a repository identity, so nothing becomes a container: {:?}",
            row.dimensions.container
        );
    }

    #[test]
    fn claude_code_tenant_falls_back_to_the_account_uuid() {
        let line = cc_with(r#","cwd":"/w/one","ownerAccountUuid":"account-fixture""#);
        let row = build_row("s", "mbp", "claude-code", &[line.as_str()]);
        assert_eq!(
            row.dimensions.tenant,
            ["account-fixture"],
            "an unauthenticated-team session has no organization, and the account \
             that did author it is still a tenancy that was recorded"
        );
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
    }

    #[test]
    fn the_organization_wins_over_the_account_that_may_belong_to_it() {
        let line = cc_with(
            r#","ownerOrganizationUuid":"org-fixture","ownerAccountUuid":"account-fixture""#,
        );
        let row = build_row("s", "mbp", "claude-code", &[line.as_str()]);
        assert_eq!(
            row.dimensions.tenant,
            ["org-fixture"],
            "two accounts can be members of one organization, so filing both would \
             record one tenancy as two"
        );
    }

    #[test]
    fn a_record_without_a_cwd_leaves_cwd_unobserved() {
        let line = cc_with(r#","ownerOrganizationUuid":"org-fixture""#);
        let row = build_row("s", "mbp", "claude-code", &[line.as_str()]);
        assert!(row.dimensions.cwd.is_empty());
        assert_eq!(row.dimensions.tenant, ["org-fixture"]);
    }

    #[test]
    fn a_record_without_tenancy_leaves_tenant_unobserved() {
        let line = cc_with(r#","cwd":"/w/one""#);
        let row = build_row("s", "mbp", "claude-code", &[line.as_str()]);
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
        assert!(
            row.dimensions.tenant.is_empty(),
            "an unauthenticated local run recorded no tenancy, and none is invented"
        );
    }

    #[test]
    fn an_absent_empty_or_mistyped_value_records_no_dimension() {
        let row = build_row(
            "s",
            "mbp",
            "claude-code",
            &[
                r#"not json at all"#,
                cc_with(r#","cwd":"","ownerOrganizationUuid":null"#).as_str(),
                cc_with(r#","cwd":7,"ownerAccountUuid":""#).as_str(),
                cc_with(r#","cwd":["/w/one"],"ownerAccountUuid":{"id":"x"}"#).as_str(),
            ],
        );
        assert!(
            row.dimensions.is_empty(),
            "an empty or mistyped value is nothing observed, never a value: {:?}",
            row.dimensions
        );
        assert!(
            row.dimensions.is_valid(),
            "and it must never be a value a capture envelope could compare against"
        );
    }

    #[test]
    fn every_directory_a_session_ran_in_is_kept_as_one_sorted_set() {
        // One session that cd'd into a subdirectory: the last line is the place
        // the conversation *ended*, so keeping only it would misreport the run.
        let first = cc_with(r#","cwd":"/w/one/apps","ownerOrganizationUuid":"org-fixture""#);
        let second = cc_with(r#","cwd":"/w/one""#);
        let third = cc_with(r#","cwd":"/w/one""#);
        let row = build_row(
            "s",
            "mbp",
            "claude-code",
            &[first.as_str(), second.as_str(), third.as_str()],
        );
        assert_eq!(row.dimensions.cwd, ["/w/one", "/w/one/apps"]);
        assert_eq!(row.dimensions.tenant, ["org-fixture"]);
    }

    #[test]
    fn a_second_tenancy_observed_in_one_session_is_both_retained() {
        let first = cc_with(r#","ownerOrganizationUuid":"org-one""#);
        let second = cc_with(r#","ownerAccountUuid":"account-two""#);
        let row = build_row(
            "s",
            "mbp",
            "claude-code",
            &[first.as_str(), second.as_str()],
        );
        assert_eq!(row.dimensions.tenant, ["account-two", "org-one"]);
    }

    #[test]
    fn another_harness_reads_no_claude_code_dimensions() {
        // Codex records carry a top-level `cwd` of their own. Reading it here
        // would be this ticket's answer for a field another one owns, and would
        // report a directory as observed for a harness nobody asked about.
        let line = codex(RFC_T1, "user_message");
        let row = build_row("s", "mbp", "codex", &[line.as_str()]);
        assert!(
            row.dimensions.is_empty(),
            "dimensions are read per harness, not from whatever key a line has: {:?}",
            row.dimensions
        );
    }

    #[test]
    fn the_projected_dimensions_travel_in_the_index_line() {
        let line = cc_with(r#","cwd":"/w/one","ownerAccountUuid":"account-fixture""#);
        let row = build_row("s", "mbp", "claude-code", &[line.as_str()]);
        let json = to_jsonl(&row);
        assert!(json.contains(r#""cwd":["/w/one"]"#), "line: {json}");
        assert!(
            json.contains(r#""tenant":["account-fixture"]"#),
            "line: {json}"
        );

        // A row with nothing observed omits the whole object rather than
        // writing four empty arrays: an index written before this field existed
        // must still read as the same shape it always did.
        let bare = cc_user(RFC_T1);
        let json = to_jsonl(&build_row("s", "mbp", "aider", &[bare.as_str()]));
        assert!(!json.contains("\"dimensions\""), "line: {json}");
    }

    // ------------------------------------------------ TICKET-4D-07 · gemini-cli
    //
    // The one fact a gemini-cli session document states about itself: the
    // `projectHash` of the project the session ran in.

    /// One gemini-cli document carrying whatever the caller spells in.
    fn gemini_doc(fields: &str) -> String {
        format!(
            r#"{{"sessionId":"s1","startTime":"{RFC_T1}","lastUpdated":"{RFC_T2}","messages":[{{"id":"m1","timestamp":"{RFC_T1}","type":"user","content":[{{"text":"hi"}}]}}],"kind":"main"{fields}}}"#
        )
    }

    // ------------------------------------------------- TICKET-4D-01 · opencode
    //
    // The three facts an opencode export envelope states about its session:
    // where it ran, the project it belonged to, and whether it was archived.

    /// One opencode export envelope carrying whatever `session` fields the
    /// caller spells in, so each case states exactly which fields it is about.
    fn oc_with(fields: &str) -> String {
        format!(
            r#"{{"schema":"chat-stasher.opencode.session.v1","session":{{{fields}}},"messages":[],"orphan_parts":[]}}"#
        )
    }
    #[test]
    fn gemini_cli_records_its_project_hash_as_the_container() {
        let line = gemini_doc(r#","projectHash":"hash-fixture""#);
        let row = build_row("s", "mbp", "gemini-cli", &[line.as_str()]);
        assert_eq!(row.dimensions.container, ["hash-fixture"]);
        assert!(
            row.dimensions.cwd.is_empty(),
            "a digest is not a path, so it never becomes a cwd: {:?}",
            row.dimensions.cwd
        );
    }

    /// The 2026-02 shape: one **pretty-printed** document whose physical lines
    /// do not parse individually, so the container is read from the whole
    /// document, exactly as the session span is.
    #[test]
    fn gemini_cli_pretty_printed_document_yields_its_container() {
        let doc = format!(
            "{{\n  \"sessionId\": \"s1\",\n  \"projectHash\": \"hash-fixture\",\n  \
             \"startTime\": \"{RFC_T1}\",\n  \"lastUpdated\": \"{RFC_T2}\",\n  \
             \"messages\": [\n    {{\n      \"id\": \"m1\",\n      \"timestamp\": \"{RFC_T1}\",\n      \
             \"type\": \"user\"\n    }}\n  ]\n}}\n"
        );
        let lines: Vec<&str> = doc.lines().collect();
        assert!(
            lines
                .iter()
                .all(|l| serde_json::from_str::<serde_json::Value>(l).is_err()),
            "premise: no individual line parses"
        );
        let row = build_row("s", "mbp", "gemini-cli", &lines);
        assert_eq!(row.dimensions.container, ["hash-fixture"]);
        assert!(row.dimensions.cwd.is_empty());
    }

    /// Several documents run together (a session plus export mirrors): each
    /// document's own hash is read, and the dimension stays a set.
    #[test]
    fn every_gemini_cli_document_in_one_body_is_read() {
        let body = format!(
            "{}\n{}\n",
            gemini_doc(r#","projectHash":"hash-one""#),
            gemini_doc(r#","projectHash":"hash-two""#),
        );
        let lines: Vec<&str> = body.lines().collect();
        let row = build_row("s", "mbp", "gemini-cli", &lines);
        assert_eq!(row.dimensions.container, ["hash-one", "hash-two"]);
    }

    #[test]
    fn a_gemini_cli_document_without_a_project_hash_leaves_container_unobserved() {
        let line = gemini_doc("");
        let row = build_row("s", "mbp", "gemini-cli", &[line.as_str()]);
        assert!(row.dimensions.container.is_empty());
        assert!(
            row.dimensions.is_empty(),
            "nothing else was observed either, so the row carries no dimensions: {:?}",
            row.dimensions
        );
    }

    #[test]
    fn an_absent_empty_or_mistyped_project_hash_records_no_container() {
        let row = build_row(
            "s",
            "mbp",
            "gemini-cli",
            &[
                r#"not json at all"#,
                gemini_doc(r#","projectHash":"""#).as_str(),
                gemini_doc(r#","projectHash":null"#).as_str(),
                gemini_doc(r#","projectHash":7"#).as_str(),
                gemini_doc(r#","projectHash":["h"]"#).as_str(),
                gemini_doc(r#","projectHash":{{"id":"h"}}"#).as_str(),
            ],
        );
        assert!(
            row.dimensions.is_empty(),
            "an empty or mistyped value is nothing observed, never a value: {:?}",
            row.dimensions
        );
        assert!(
            row.dimensions.is_valid(),
            "and it must never be a value a capture envelope could compare against"
        );
    }

    /// Dimensions are read per harness: a claude-code record carrying a
    /// `projectHash` key of its own is not a gemini-cli document, and reading
    /// it here would report a container for a harness nobody asked about.
    #[test]
    fn another_harness_reads_no_gemini_cli_dimensions() {
        let line = cc_with(r#","cwd":"/w/one","projectHash":"hash-fixture""#);
        let row = build_row("s", "mbp", "claude-code", &[line.as_str()]);
        assert!(
            row.dimensions.container.is_empty(),
            "the gemini-cli reader is the only one that reads projectHash: {:?}",
            row.dimensions.container
        );
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
    }

    #[test]
    fn the_gemini_cli_container_travels_in_the_index_line() {
        let line = gemini_doc(r#","projectHash":"hash-fixture""#);
        let row = build_row("s", "mbp", "gemini-cli", &[line.as_str()]);
        let json = to_jsonl(&row);
        assert!(
            json.contains(r#""container":["hash-fixture"]"#),
            "line: {json}"
        );
        assert!(
            json.contains(r#""cwd":[]"#) || !json.contains("\"cwd\""),
            "a project hash is never a working directory: {json}"
        );
    }

    #[test]
    fn an_opencode_export_states_its_directory_project_and_archive_fact() {
        let line = oc_with(
            r#""id":"s1","directory":"/w/one","project_id":"project-fixture","time_archived":1770000000000"#,
        );
        let row = build_row("s", "mbp", "opencode", &[line.as_str()]);
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
        assert_eq!(
            row.dimensions.container,
            ["project-fixture"],
            "a foreign key the harness recorded is a container, which is what the \
             directory on its own can never be"
        );
        assert_eq!(row.dimensions.status, ["archived"]);
    }

    #[test]
    fn an_unarchived_opencode_session_stays_status_unobserved() {
        // The one lifecycle fact the row can state is the moment it was
        // archived; a null says neither "never" nor "not yet", so no state is
        // inferred from the absence.
        let line = oc_with(r#""id":"s1","directory":"/w/one","time_archived":null"#);
        let row = build_row("s", "mbp", "opencode", &[line.as_str()]);
        assert!(
            row.dimensions.status.is_empty(),
            "a null time_archived is an unobserved status, never `{}`: {:?}",
            "active",
            row.dimensions.status
        );
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
    }

    #[test]
    fn an_opencode_session_without_a_project_records_no_container() {
        let line = oc_with(r#""id":"s1","directory":"/w/one""#);
        let row = build_row("s", "mbp", "opencode", &[line.as_str()]);
        assert!(
            row.dimensions.container.is_empty(),
            "no project key was recorded, and none is invented from the directory"
        );
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
        // The empty-string spelling of the same absence is the same absence.
        let row = build_row(
            "s",
            "mbp",
            "opencode",
            &[oc_with(r#""id":"s1","directory":"/w/one","project_id"""#).as_str()],
        );
        assert!(row.dimensions.container.is_empty());
    }

    #[test]
    fn a_null_directory_falls_back_to_the_path_the_row_also_carries() {
        let line =
            oc_with(r#""id":"s1","directory":null,"path":"/w/one","project_id":"project-fixture""#);
        let row = build_row("s", "mbp", "opencode", &[line.as_str()]);
        assert_eq!(
            row.dimensions.cwd,
            ["/w/one"],
            "a null directory is not a directory, and the path beside it still is"
        );
        assert_eq!(row.dimensions.container, ["project-fixture"]);
    }

    #[test]
    fn every_directory_an_opencode_session_ran_in_is_kept_as_one_sorted_set() {
        // A resume re-exports the whole session, so one that moved carries both
        // directories — and the archive holds both exports.
        let first = oc_with(r#""id":"s1","directory":"/w/one""#);
        let second = oc_with(r#""id":"s1","directory":"/w/one/apps""#);
        let third = oc_with(r#""id":"s1","directory":"/w/one""#);
        let row = build_row(
            "s",
            "mbp",
            "opencode",
            &[first.as_str(), second.as_str(), third.as_str()],
        );
        assert_eq!(row.dimensions.cwd, ["/w/one", "/w/one/apps"]);
    }

    #[test]
    fn a_session_archived_after_a_resume_is_archived() {
        // The earlier export predates the archive, so only the later one
        // states the fact — and one observation is enough to record it.
        let before = oc_with(r#""id":"s1","directory":"/w/one","time_archived":null"#);
        let after = oc_with(
            r#""id":"s1","directory":"/w/one","project_id":"project-fixture","time_archived":1770000000000"#,
        );
        let row = build_row("s", "mbp", "opencode", &[before.as_str(), after.as_str()]);
        assert_eq!(row.dimensions.status, ["archived"]);
        assert_eq!(row.dimensions.container, ["project-fixture"]);
    }

    #[test]
    fn an_opencode_record_without_a_usable_value_records_no_dimension() {
        let row = build_row(
            "s",
            "mbp",
            "opencode",
            &[
                "not json at all",
                oc_with(r#""id":"s1","directory":"","path":"","project_id":"""#).as_str(),
                oc_with(r#""id":"s1","directory":7,"project_id":{"id":"x"}"#).as_str(),
                // A timestamp spelled as a string is not the integer shape
                // the exporter writes, so it is unread rather than trusted.
                oc_with(r#""id":"s1","time_archived":"1770000000000""#).as_str(),
                oc_with(r#""id":"s1","directory":{"path":"/w/one"}"#).as_str(),
                // A `session` object the exporter did not seal under the export
                // schema is a record nothing vouches for, whatever it says.
                r#"{"session":{"directory":"/w/one","project_id":"project-fixture","time_archived":1770000000000}}"#,
                // And neither is an envelope whose schema names something else.
                r#"{"schema":"chat-stasher.opencode.session.v2","session":{"directory":"/w/one"}}"#,
            ],
        );
        assert!(
            row.dimensions.is_empty(),
            "an empty, mistyped, or unsealed value is nothing observed, never a value: {:?}",
            row.dimensions
        );
        assert!(
            row.dimensions.is_valid(),
            "and it must never be a value a capture envelope could compare against"
        );
    }

    #[test]
    fn another_harness_reads_no_opencode_dimensions() {
        // A Claude Code harness handed an opencode envelope: the same bytes
        // exist, and dimensions are still read per harness, not per shape.
        let line = oc_with(r#""id":"s1","directory":"/w/one","project_id":"project-fixture""#);
        let row = build_row("s", "mbp", "claude-code", &[line.as_str()]);
        assert!(
            row.dimensions.is_empty(),
            "dimensions are answered per harness, not read from whatever an \
             archived line happens to carry: {:?}",
            row.dimensions
        );
        // And the envelope under its own harness is where the facts live.
        let row = build_row("s", "mbp", "opencode", &[line.as_str()]);
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
        assert_eq!(row.dimensions.container, ["project-fixture"]);
    }

    #[test]
    fn the_opencode_dimensions_travel_in_the_index_line() {
        let line = oc_with(
            r#""id":"s1","directory":"/w/one","project_id":"project-fixture","time_archived":1770000000000"#,
        );
        let json = to_jsonl(&build_row("s", "mbp", "opencode", &[line.as_str()]));
        assert!(json.contains(r#""cwd":["/w/one"]"#), "line: {json}");
        assert!(
            json.contains(r#""container":["project-fixture"]"#),
            "line: {json}"
        );
        assert!(json.contains(r#""status":["archived"]"#), "line: {json}");
        assert!(!json.contains("\"tenant\""), "line: {json}");
    }

    /// One Grok CLI export envelope carrying whatever `session` fields the
    /// caller spells in, so each case states exactly which fields it is about.
    fn grok_with(fields: &str) -> String {
        format!(
            r#"{{"schema":"chat-stasher.sqlite.session.v1","table":"session_docs","session":{{{fields}}}}}"#
        )
    }

    #[test]
    fn a_grok_cli_session_docs_row_states_its_cwd() {
        let line = grok_with(r#""session_id":"s1","cwd":"/w/one","updated_at":1789384914"#);
        let row = build_row("s", "mbp", "grok", &[line.as_str()]);
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
        assert!(
            row.dimensions.container.is_empty(),
            "a path is not a repository identity, so nothing becomes a container: {:?}",
            row.dimensions.container
        );
    }

    #[test]
    fn a_grok_cli_row_without_a_usable_cwd_records_none() {
        let row = build_row(
            "s",
            "mbp",
            "grok",
            &[
                "not json at all",
                grok_with(r#""session_id":"s1","cwd":"","updated_at":1789384914"#).as_str(),
                grok_with(r#""session_id":"s1","updated_at":1789384914"#).as_str(),
                grok_with(r#""session_id":"s1","cwd":null,"updated_at":1789384914"#).as_str(),
                grok_with(r#""session_id":"s1","cwd":7,"updated_at":1789384914"#).as_str(),
                grok_with(r#""session_id":"s1","cwd":{"path":"/w/one"},"updated_at":1789384914"#)
                    .as_str(),
                // A `session_docs`-looking object the exporter did not seal under
                // its schema is a record nothing vouches for.
                r#"{"table":"session_docs","session":{"cwd":"/w/one"}}"#,
                // And neither is the export schema over a different table.
                r#"{"schema":"chat-stasher.sqlite.session.v1","table":"threads","session":{"cwd":"/w/one"}}"#,
            ],
        );
        assert!(
            row.dimensions.is_empty(),
            "an absent, empty, or mistyped cwd is nothing observed, never a value: {:?}",
            row.dimensions
        );
        assert!(row.dimensions.is_valid());
    }

    #[test]
    fn every_directory_a_grok_session_ran_in_is_kept_as_one_sorted_set() {
        // A session resumed and re-exported states each place it ran; the last
        // line alone would report where the conversation *ended* as where it ran.
        let first = grok_with(r#""session_id":"s1","cwd":"/w/one/apps","updated_at":1789384914"#);
        let second = grok_with(r#""session_id":"s1","cwd":"/w/one","updated_at":1789384915"#);
        let third = grok_with(r#""session_id":"s1","cwd":"/w/one","updated_at":1789384916"#);
        let row = build_row(
            "s",
            "mbp",
            "grok",
            &[first.as_str(), second.as_str(), third.as_str()],
        );
        assert_eq!(row.dimensions.cwd, ["/w/one", "/w/one/apps"]);
    }

    #[test]
    fn the_grok_web_bundle_states_no_cwd() {
        // The browser-extension bundle shares the `grok` name but is a web
        // payload, not a `session_docs` row; it must not be read as one.
        let body = serde_json::json!({
            "responses": [ { "createTime": RFC_T1 } ],
        })
        .to_string();
        let line = web_line("grok", &body);
        let row = build_row("s", "mbp", "grok", &[line.as_str()]);
        assert!(
            row.dimensions.is_empty(),
            "a web bundle states no working directory, and none is invented: {:?}",
            row.dimensions
        );
    }

    #[test]
    fn another_harness_reads_no_grok_dimensions() {
        let line = grok_with(r#""session_id":"s1","cwd":"/w/one","updated_at":1789384914"#);
        let row = build_row("s", "mbp", "claude-code", &[line.as_str()]);
        assert!(
            row.dimensions.is_empty(),
            "dimensions are answered per harness, not read from whatever an \
             archived line happens to carry: {:?}",
            row.dimensions
        );
    }

    #[test]
    fn the_grok_cwd_travels_in_the_index_line() {
        let line = grok_with(r#""session_id":"s1","cwd":"/w/one","updated_at":1789384914"#);
        let json = to_jsonl(&build_row("s", "mbp", "grok", &[line.as_str()]));
        assert!(json.contains(r#""cwd":["/w/one"]"#), "line: {json}");
        assert!(!json.contains("\"tenant\""), "line: {json}");
        assert!(!json.contains("\"container\""), "line: {json}");
    }

    // --------------------------------------------- TICKET-4D-03 · codex
    //
    // The two facts a codex rollout states about its session: the
    // directory it ran in, and the repository (else the workspace root)
    // it belonged to.

    /// One codex rollout record of the given `type`, carrying whatever
    /// `payload` fields the caller spells in, so each case states
    /// exactly which fields it is about.
    fn codex_record(ty: &str, payload: &str) -> String {
        format!(r#"{{"timestamp":"{RFC_T1}","type":"{ty}","payload":{{{payload}}}}}"#)
    }

    #[test]
    fn codex_records_its_cwd_and_its_repository_as_container() {
        let line = codex_record(
            "session_meta",
            r#""id":"s1","cwd":"/w/one","git":{"repository_url":"https://github.com/org/repo-fixture"}"#,
        );
        let row = build_row("s", "mbp", "codex", &[line.as_str()]);
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
        assert_eq!(
            row.dimensions.container,
            ["https://github.com/org/repo-fixture"],
            "the repository URL the harness recorded is a container, which is \
             what the directory on its own can never be"
        );
    }

    #[test]
    fn a_codex_repository_url_wins_over_the_workspace_root_beside_it() {
        let line = codex_record(
            "session_meta",
            r#""id":"s1","cwd":"/w/one","git":{"repository_url":"https://github.com/org/repo-fixture"},"runtime_workspace_roots":["/w/one","/w/other"]"#,
        );
        let row = build_row("s", "mbp", "codex", &[line.as_str()]);
        assert_eq!(
            row.dimensions.container,
            ["https://github.com/org/repo-fixture"],
            "the repository URL is the primary container; the workspace root \
             beside it is only the fallback"
        );
    }

    #[test]
    fn codex_container_falls_back_to_the_first_runtime_workspace_root() {
        // A session run outside any repository: no `git`, so the first
        // workspace root the runtime gave it stands in for the repository
        // identity.
        let line = codex_record(
            "session_meta",
            r#""id":"s1","cwd":"/w/one","runtime_workspace_roots":["/w/one","/w/other"]"#,
        );
        let row = build_row("s", "mbp", "codex", &[line.as_str()]);
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
        assert_eq!(
            row.dimensions.container,
            ["/w/one"],
            "the first runtime workspace root is the container when no \
             repository URL was recorded"
        );
    }

    #[test]
    fn codex_cwd_without_a_repository_or_root_leaves_container_unobserved() {
        let line = codex_record("session_meta", r#""id":"s1","cwd":"/w/one""#);
        let row = build_row("s", "mbp", "codex", &[line.as_str()]);
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
        assert!(
            row.dimensions.container.is_empty(),
            "no repository URL and no workspace root was recorded, and none \
             is invented from the directory: {:?}",
            row.dimensions.container
        );
    }

    #[test]
    fn a_codex_session_stating_nothing_leaves_both_dimensions_unobserved() {
        let line = codex_record("session_meta", r#""id":"s1""#);
        let row = build_row("s", "mbp", "codex", &[line.as_str()]);
        assert!(
            row.dimensions.is_empty(),
            "a session_meta that states no directory and no repository \
             observed nothing: {:?}",
            row.dimensions
        );
    }

    #[test]
    fn a_codex_session_without_a_session_meta_cwd_falls_back_to_the_turn_context() {
        // The session's own record stated no directory, so the directory
        // the turn started in is the fallback observation.
        let meta = codex_record(
            "session_meta",
            r#""id":"s1","git":{"repository_url":"https://github.com/org/repo-fixture"}"#,
        );
        let turn = codex_record("turn_context", r#""cwd":"/w/one""#);
        let row = build_row("s", "mbp", "codex", &[meta.as_str(), turn.as_str()]);
        assert_eq!(
            row.dimensions.cwd,
            ["/w/one"],
            "a session_meta with no cwd leaves the turn_context cwd as the \
             fallback observation"
        );
        assert_eq!(
            row.dimensions.container,
            ["https://github.com/org/repo-fixture"]
        );
    }

    #[test]
    fn a_codex_session_meta_cwd_is_not_seconded_by_its_turn_contexts() {
        // The session's own record already said where it ran, so the turn
        // contexts — even one naming another directory — are not read: the
        // fallback is for a session that stated no directory, not a second
        // opinion beside one.
        let meta = codex_record("session_meta", r#""id":"s1","cwd":"/w/one""#);
        let turn = codex_record("turn_context", r#""cwd":"/w/two""#);
        let row = build_row("s", "mbp", "codex", &[meta.as_str(), turn.as_str()]);
        assert_eq!(
            row.dimensions.cwd,
            ["/w/one"],
            "the session's own record answered the cwd question, so the turn \
             context beside it is not a second observation"
        );
    }

    #[test]
    fn an_absent_empty_or_mistyped_codex_value_records_no_dimension() {
        let row = build_row(
            "s",
            "mbp",
            "codex",
            &[
                "not json at all",
                codex_record(
                    "session_meta",
                    r#""id":"s1","cwd":"","git":{"repository_url":""}"#,
                )
                .as_str(),
                codex_record("session_meta", r#""id":"s1","cwd":null,"git":null"#).as_str(),
                codex_record(
                    "session_meta",
                    r#""id":"s1","cwd":7,"git":{"repository_url":7}"#,
                )
                .as_str(),
                codex_record("session_meta", r#""id":"s1","cwd":{"path":"/w/one"}"#).as_str(),
                codex_record(
                    "session_meta",
                    r#""id":"s1","runtime_workspace_roots":[]"#,
                )
                .as_str(),
                codex_record(
                    "session_meta",
                    r#""id":"s1","runtime_workspace_roots":[""]"#,
                )
                .as_str(),
                codex_record(
                    "session_meta",
                    r#""id":"s1","runtime_workspace_roots":[7]"#,
                )
                .as_str(),
                // A record the exporter did not tag `session_meta` or
                // `turn_context` is a conversation or event record, not a
                // session-fact record, whatever it carries.
                codex_record(
                    "response_item",
                    r#""type":"message","role":"user","cwd":"/w/one","git":{"repository_url":"https://github.com/org/repo-fixture"}"#,
                )
                .as_str(),
            ],
        );
        assert!(
            row.dimensions.is_empty(),
            "an empty, mistyped, or wrongly-tagged value is nothing observed, \
             never a value: {:?}",
            row.dimensions
        );
        assert!(
            row.dimensions.is_valid(),
            "and it must never be a value a capture envelope could compare against"
        );
    }

    #[test]
    fn another_harness_reads_no_codex_dimensions() {
        // A Claude Code harness handed a codex session_meta record: the
        // same bytes exist, and dimensions are still read per harness, not
        // per shape.
        let line = codex_record(
            "session_meta",
            r#""id":"s1","cwd":"/w/one","git":{"repository_url":"https://github.com/org/repo-fixture"}"#,
        );
        let row = build_row("s", "mbp", "claude-code", &[line.as_str()]);
        assert!(
            row.dimensions.is_empty(),
            "dimensions are answered per harness, not read from whatever an \
             archived line happens to carry: {:?}",
            row.dimensions
        );
        // And the record under its own harness is where the facts live.
        let row = build_row("s", "mbp", "codex", &[line.as_str()]);
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
        assert_eq!(
            row.dimensions.container,
            ["https://github.com/org/repo-fixture"]
        );
    }

    #[test]
    fn the_codex_dimensions_travel_in_the_index_line() {
        let line = codex_record(
            "session_meta",
            r#""id":"s1","cwd":"/w/one","git":{"repository_url":"https://github.com/org/repo-fixture"}"#,
        );
        let json = to_jsonl(&build_row("s", "mbp", "codex", &[line.as_str()]));
        assert!(json.contains(r#""cwd":["/w/one"]"#), "line: {json}");
        assert!(
            json.contains(r#""container":["https://github.com/org/repo-fixture"]"#),
            "line: {json}"
        );
        assert!(!json.contains("\"tenant\""), "line: {json}");
        assert!(!json.contains("\"status\""), "line: {json}");
    }

    // --------------------------------------------- TICKET-4D-10 · openclaw
    //
    // The two facts an OpenClaw export line states about itself: the agent the
    // session ran inside, and — only on a cold archive line — that the session
    // is archived.

    /// One sealed OpenClaw export line, spelled exactly the way
    /// `sqlite_probe::read_openclaw_session` seals it.
    fn oc_line(schema: &str, agent_id: &str) -> String {
        format!(r#"{{"schema":"{schema}","agent_id":"{agent_id}","window":{{"session_id":"s1"}}}}"#)
    }

    #[test]
    fn openclaw_records_its_agent_as_the_container_it_ran_inside() {
        let line = oc_line(
            crate::sqlite_probe::OPENCLAW_SESSION_SCHEMA,
            "synthetic-agent",
        );
        let row = build_row("s", "mbp", "openclaw", &[line.as_str()]);
        assert_eq!(row.dimensions.container, ["synthetic-agent"]);
        assert!(
            row.dimensions.status.is_empty(),
            "a live window records no lifecycle status, and none is assumed: {:?}",
            row.dimensions.status
        );
    }

    #[test]
    fn openclaw_records_archived_status_on_a_cold_archive_line() {
        let line = oc_line(
            crate::sqlite_probe::OPENCLAW_ARCHIVE_SCHEMA,
            "synthetic-agent",
        );
        let row = build_row("s", "mbp", "openclaw", &[line.as_str()]);
        assert_eq!(row.dimensions.container, ["synthetic-agent"]);
        assert_eq!(
            row.dimensions.status,
            ["archived"],
            "a line sealed from session_transcript_archives is the harness's own \
             record that the session is archived"
        );
    }

    #[test]
    fn an_openclaw_line_without_an_agent_id_records_no_container() {
        let line = r#"{"schema":"chat-stasher.openclaw.session.v1","window":{}}"#;
        let row = build_row("s", "mbp", "openclaw", &[line]);
        assert!(
            row.dimensions.is_empty(),
            "an agent id that was not observed is not invented: {:?}",
            row.dimensions
        );
        assert!(row.dimensions.is_valid());
    }

    #[test]
    fn an_empty_or_mistyped_openclaw_agent_id_records_no_container() {
        // A live-window line carries no status of its own, so the only fact
        // these lines could record is the container — and an empty or mistyped
        // agent id records no container either.
        let row = build_row(
            "s",
            "mbp",
            "openclaw",
            &[
                r#"not json at all"#,
                r#"{"schema":"chat-stasher.openclaw.session.v1","agent_id":""}"#,
                r#"{"schema":"chat-stasher.openclaw.session.v1","agent_id":7}"#,
                r#"{"schema":"chat-stasher.openclaw.session.v1","agent_id":["a"]}"#,
            ],
        );
        assert!(
            row.dimensions.is_empty(),
            "an empty or mistyped value is nothing observed, never a value: {:?}",
            row.dimensions
        );
        assert!(row.dimensions.is_valid());

        // On an archive line the status is the schema's own fact, independent
        // of the agent id: a mistyped agent id records no container, while
        // the line still states the session is archived.
        let row = build_row(
            "s",
            "mbp",
            "openclaw",
            &[r#"{"schema":"chat-stasher.openclaw.archive.v1","agent_id":""}"#],
        );
        assert!(
            row.dimensions.container.is_empty(),
            "the container stays unobserved: {:?}",
            row.dimensions.container
        );
        assert_eq!(row.dimensions.status, ["archived"]);
    }

    #[test]
    fn an_unsealed_or_foreign_schema_line_records_no_openclaw_dimensions() {
        // Only a line sealed under one of the two OpenClaw schemas vouches
        // for what it states: everything the export writes is sealed, so a
        // line carrying neither schema — a foreign schema's line that
        // happens to share a top-level `agent_id`, or an unsealed object —
        // is the same shape with nobody behind it, exactly as the opencode
        // fold treats a line the exporter did not seal.
        let row = build_row(
            "s",
            "mbp",
            "openclaw",
            &[
                r#"{"schema":"chat-stasher.opencode.session.v1","agent_id":"synthetic-agent"}"#,
                r#"{"agent_id":"synthetic-agent"}"#,
                r#"{"schema":"chat-stasher.openclaw.session.v0","agent_id":"synthetic-agent"}"#,
            ],
        );
        assert!(
            row.dimensions.is_empty(),
            "a line no OpenClaw schema sealed is nothing observed, never a container: {:?}",
            row.dimensions
        );
        assert!(row.dimensions.is_valid());
    }

    #[test]
    fn one_agent_observed_twice_is_one_container() {
        let first = oc_line(
            crate::sqlite_probe::OPENCLAW_SESSION_SCHEMA,
            "synthetic-agent",
        );
        let second = oc_line(
            crate::sqlite_probe::OPENCLAW_ARCHIVE_SCHEMA,
            "synthetic-agent",
        );
        let row = build_row("s", "mbp", "openclaw", &[first.as_str(), second.as_str()]);
        assert_eq!(row.dimensions.container, ["synthetic-agent"]);
        assert_eq!(row.dimensions.status, ["archived"]);
    }

    #[test]
    fn another_harness_reads_no_openclaw_dimensions() {
        // The same line read under a harness nobody asked about is not
        // evidence: dimensions are read per harness, exactly as claude-code's
        // are.
        let line = oc_line(
            crate::sqlite_probe::OPENCLAW_ARCHIVE_SCHEMA,
            "synthetic-agent",
        );
        let row = build_row("s", "mbp", "codex", &[line.as_str()]);
        assert!(
            row.dimensions.is_empty(),
            "dimensions are read per harness, not from whatever key a line has: {:?}",
            row.dimensions
        );
    }

    #[test]
    fn the_openclaw_dimensions_travel_in_the_index_line() {
        let line = oc_line(
            crate::sqlite_probe::OPENCLAW_ARCHIVE_SCHEMA,
            "synthetic-agent",
        );
        let json = to_jsonl(&build_row("s", "mbp", "openclaw", &[line.as_str()]));
        assert!(
            json.contains(r#""container":["synthetic-agent"]"#),
            "line: {json}"
        );
        assert!(json.contains(r#""status":["archived"]"#), "line: {json}");

        // A live window line carries the container and omits the status.
        let live = oc_line(
            crate::sqlite_probe::OPENCLAW_SESSION_SCHEMA,
            "synthetic-agent",
        );
        let json = to_jsonl(&build_row("s", "mbp", "openclaw", &[live.as_str()]));
        assert!(
            json.contains(r#""container":["synthetic-agent"]"#),
            "line: {json}"
        );
        assert!(!json.contains("status"), "line: {json}");
    }

    // --------------------------------------------- TICKET-4D-11 · continue
    //
    // Continue states one provenance fact on the top level of its session
    // document: `workspaceDirectory`, the workspace the session was opened in.

    /// One Continue session file (`sessions/<id>.json`) carrying whatever
    /// top-level fields the caller spells in, so each case states exactly which
    /// fields it is about.
    fn continue_doc(fields: &str) -> String {
        format!(
            r#"{{"sessionId":"sess-1-continue-4d","title":"t","messages":[{{"role":"user","content":"synthetic question"}}]{fields}}}"#
        )
    }

    #[test]
    fn a_continue_session_records_its_workspace_directory_as_cwd_and_container() {
        let line = continue_doc(r#","workspaceDirectory":"/w/one""#);
        let row = build_row("s", "mbp", "continue", &[line.as_str()]);
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
        assert_eq!(
            row.dimensions.container,
            ["/w/one"],
            "the workspace directory is the workspace identity Continue itself \
             wrote down, and the ticket sends that one recorded value to both"
        );
        assert!(row.dimensions.tenant.is_empty());
        assert!(row.dimensions.status.is_empty());
    }

    /// Continue writes its session file **pretty-printed**, so — like the
    /// gemini-cli document — its physical lines do not parse on their own and
    /// the field is read from the whole document.
    #[test]
    fn a_pretty_printed_continue_document_yields_its_workspace_directory() {
        let doc = format!(
            "{{\n  \"sessionId\": \"sess-1-continue-4d\",\n  \"workspaceDirectory\": \
             \"/w/one\",\n  \"messages\": [\n    {{\n      \"role\": \"user\",\n      \
             \"content\": \"synthetic question\"\n    }}\n  ]\n}}\n"
        );
        let lines: Vec<&str> = doc.lines().collect();
        assert!(
            lines
                .iter()
                .all(|l| serde_json::from_str::<serde_json::Value>(l).is_err()),
            "premise: no individual line parses"
        );
        let row = build_row("s", "mbp", "continue", &lines);
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
        assert_eq!(row.dimensions.container, ["/w/one"]);
    }

    #[test]
    fn a_continue_session_without_a_workspace_directory_leaves_both_unobserved() {
        // Absent, empty, null, and mistyped are four spellings of the same
        // absence, and none of them becomes a value.
        let row = build_row(
            "s",
            "mbp",
            "continue",
            &[
                "not json at all",
                continue_doc("").as_str(),
                continue_doc(r#","workspaceDirectory":"""#).as_str(),
                continue_doc(r#","workspaceDirectory":null"#).as_str(),
                continue_doc(r#","workspaceDirectory":7"#).as_str(),
                continue_doc(r#","workspaceDirectory":["/w/one"]"#).as_str(),
                continue_doc(r#","workspaceDirectory":{{"path":"/w/one"}}"#).as_str(),
            ],
        );
        assert!(
            row.dimensions.is_empty(),
            "a session that named no workspace observed nothing, and never a \
             value every such session would share: {:?}",
            row.dimensions
        );
        assert!(
            row.dimensions.is_valid(),
            "and it must never be a value a capture envelope could compare against"
        );
    }

    /// A workspace directory is not a tenancy and not a lifecycle: nothing here
    /// is filled in for the two dimensions Continue records no field for.
    #[test]
    fn a_continue_workspace_directory_fills_no_other_dimension() {
        let row = build_row(
            "s",
            "mbp",
            "continue",
            &[continue_doc(r#","workspaceDirectory":"/w/one""#).as_str()],
        );
        assert!(
            row.dimensions.tenant.is_empty() && row.dimensions.status.is_empty(),
            "no tenant and no status is recorded by this field: {:?}",
            row.dimensions
        );
    }

    #[test]
    fn another_harness_reads_no_continue_dimensions() {
        // A Claude Code transcript whose line happens to carry the key is not a
        // Continue session file: dimensions are read per harness, not per shape.
        let line = cc_with(r#","cwd":"/w/one","workspaceDirectory":"/w/other""#);
        let row = build_row("s", "mbp", "claude-code", &[line.as_str()]);
        assert!(
            row.dimensions.container.is_empty(),
            "the continue reader is the only one that reads workspaceDirectory: {:?}",
            row.dimensions.container
        );
        assert_eq!(row.dimensions.cwd, ["/w/one"]);
    }

    #[test]
    fn the_continue_dimensions_travel_in_the_index_line() {
        let line = continue_doc(r#","workspaceDirectory":"/w/one""#);
        let json = to_jsonl(&build_row("s", "mbp", "continue", &[line.as_str()]));
        assert!(json.contains(r#""cwd":["/w/one"]"#), "line: {json}");
        assert!(json.contains(r#""container":["/w/one"]"#), "line: {json}");
        assert!(!json.contains("\"tenant\""), "line: {json}");
        assert!(!json.contains("\"status\""), "line: {json}");
    }

    // ------------------------------------------------- W219 · account keys

    /// A shard record line carrying one account envelope, spelled exactly the
    /// way `contracts/inbox.schema.json` and the extension write it.
    fn with_account(account: &str) -> String {
        format!(
            r#"{{"schema":"chat-stasher/inbox@2","platform":"deepseek","sessionId":"s","account":{account},"raw":{{"text":"x","bytes":1}}}}"#
        )
    }

    fn fingerprint(salt: &str, value: &str) -> String {
        format!(
            r#"{{"kind":"fingerprint","value":"{value}","source":"response-body-platform-uid","saltId":"{salt}"}}"#
        )
    }

    /// The keys are a **set**: the same account captured twice is one key, and
    /// the order the lines happen to be in does not change the row — which is
    /// what lets the index stay a pure function of the session's content.
    #[test]
    fn account_keys_are_deduped_and_sorted_into_a_set() {
        let ok = AccountKey {
            salt_id: "salt-1".into(),
            value: "aa".into(),
        };
        // Same two keys, written in the two orders, with one line repeated.
        let a = with_account(&fingerprint("salt-1", "aa"));
        let b = with_account(&fingerprint("salt-1", "bb"));
        let forward = build_row("deepseek.s", "mbp", "deepseek", &[a.as_str(), b.as_str()]);
        let backward = build_row(
            "deepseek.s",
            "mbp",
            "deepseek",
            &[b.as_str(), a.as_str(), a.as_str()],
        );
        assert_eq!(
            forward.account_keys.len(),
            2,
            "the repeated line is one key"
        );
        assert_eq!(
            forward.account_keys, backward.account_keys,
            "line order must not change the row"
        );
        assert_eq!(forward.account_keys[0], ok);
    }

    /// Two accounts under one salt are **both** kept: this is the fact the
    /// collision verdict is read from, so dropping either would erase it.
    #[test]
    fn two_accounts_under_one_salt_are_both_recorded() {
        let a = with_account(&fingerprint("salt-1", "aa"));
        let b = with_account(&fingerprint("salt-1", "bb"));
        let row = build_row("deepseek.s", "mbp", "deepseek", &[a.as_str(), b.as_str()]);
        assert_eq!(
            row.account_keys,
            vec![
                AccountKey {
                    salt_id: "salt-1".into(),
                    value: "aa".into()
                },
                AccountKey {
                    salt_id: "salt-1".into(),
                    value: "bb".into()
                },
            ]
        );
    }

    /// An **unknown** account is not a value. Recording it as a key would let it
    /// silently agree or disagree with a real fingerprint, which is exactly the
    /// "unknown recorded as a concrete value" failure invariant 1 forbids.
    #[test]
    fn an_unknown_account_records_no_key() {
        for reason in [
            "no-account-id-in-capture",
            "email-is-not-an-account-id",
            "salt-unreadable",
            // W239 · the organization-scoped refusal. It is a reason and not a value for
            // the same reason as the others: what it states is that no account can be
            // named here, so a key made from it would be a key made from nothing.
            "organization-is-not-an-account",
        ] {
            let line = with_account(&format!(r#"{{"kind":"unknown","reason":"{reason}"}}"#));
            let row = build_row("deepseek.s", "mbp", "deepseek", &[line.as_str()]);
            assert!(
                row.account_keys.is_empty(),
                "`{reason}` is not a key: {:?}",
                row.account_keys
            );
        }
    }

    /// A bundle an older build wrote for an organization-scoped plan: kind
    /// `fingerprint`, and a `source` that says the value hashes an
    /// **organization**. W239 keeps this label readable precisely so these
    /// bundles still parse.
    fn organization_fingerprint(salt: &str, value: &str) -> String {
        format!(
            r#"{{"kind":"fingerprint","value":"{value}","source":"request-url-organization","saltId":"{salt}"}}"#
        )
    }

    /// 🔴 W239 · A stored fingerprint whose `source` names an organization is not
    /// an account key, and this is the half of W239 that no live code path can
    /// reach: the extension stopped writing this label, so the only bundles that
    /// carry it are ones an older build wrote and an archive still holds. Two
    /// accounts in one organization hash to the same value, so accepting the key
    /// would keep a positive, false statement that two people are one — the
    /// mis-attribution W239 exists to remove, surviving in the data that was
    /// already written.
    #[test]
    fn an_organization_derived_fingerprint_records_no_key() {
        let legacy = with_account(&organization_fingerprint("salt-1", "aa"));
        let row = build_row("claude.s", "mbp", "claude", &[legacy.as_str()]);
        assert!(
            row.account_keys.is_empty(),
            "an organization is not an account: {:?}",
            row.account_keys
        );
    }

    /// The same session carrying an organization value **and** a real account
    /// value keeps the real one. The rule drops a claim, not the row: a reader
    /// that lost the account the build could actually name would be trading one
    /// wrong answer for another.
    #[test]
    fn an_organization_derived_fingerprint_does_not_hide_a_real_one() {
        let legacy = with_account(&organization_fingerprint("salt-1", "aa"));
        let real = with_account(&fingerprint("salt-1", "bb"));
        let row = build_row(
            "claude.s",
            "mbp",
            "claude",
            &[legacy.as_str(), real.as_str()],
        );
        assert_eq!(
            row.account_keys,
            vec![AccountKey {
                salt_id: "salt-1".into(),
                value: "bb".into()
            }]
        );
    }

    /// A `source` the contract does not name as a person's id is not evidence
    /// about an account, so it is not a key — the same rule the other malformed
    /// envelopes above follow. `contracts/inbox.schema.json` closes this enum,
    /// so a label outside it is a bundle this reader cannot read rather than one
    /// it may guess about; `ACCOUNT_ID_SOURCES` is where the extension pins the
    /// same set.
    #[test]
    fn a_fingerprint_with_an_unknown_source_records_no_key() {
        let absent = with_account(r#"{"kind":"fingerprint","value":"aa","saltId":"salt-1"}"#);
        let invented = with_account(
            r#"{"kind":"fingerprint","value":"aa","source":"response-body-uid","saltId":"salt-1"}"#,
        );
        for line in [absent, invented] {
            let row = build_row("deepseek.s", "mbp", "deepseek", &[line.as_str()]);
            assert!(row.account_keys.is_empty(), "{:?}", row.account_keys);
        }
    }

    /// A fingerprint envelope missing either half of the comparable pair is not
    /// comparable, so it is not a key. `saltId` is the whole reason the pair
    /// travels together: a value with no salt would be compared against every
    /// other salt's values.
    #[test]
    fn a_fingerprint_without_a_salt_or_a_value_records_no_key() {
        let no_salt = with_account(r#"{"kind":"fingerprint","value":"aa"}"#);
        let no_value = with_account(r#"{"kind":"fingerprint","saltId":"salt-1"}"#);
        let empty = with_account(&fingerprint("", ""));
        for line in [no_salt, no_value, empty] {
            let row = build_row("deepseek.s", "mbp", "deepseek", &[line.as_str()]);
            assert!(row.account_keys.is_empty(), "{:?}", row.account_keys);
        }
    }

    /// An index written before W219 has no `account_keys` key at all, and must
    /// still deserialize — the same additive rule `source_zone` and `title`
    /// follow.
    #[test]
    fn an_index_line_without_account_keys_still_deserializes() {
        let old = r#"{"session_id":"deepseek.s","machine":"m","harness":"deepseek","first_unix":1,"last_unix":2,"line_count":3,"time_source":{"kind":"exact"}}"#;
        let row: ActivityRow = serde_json::from_str(old).expect("a pre-W219 line still reads");
        assert!(row.account_keys.is_empty());
        // And a row with no keys writes no key, so `@1`-era output is unchanged.
        assert!(!to_jsonl(&row).contains("account_keys"));
    }

    // ------------------------------------------------------------------ Exact
    #[test]
    fn claude_code_rfc3339_exact_first_last() {
        let l1 = cc_user(RFC_T1);
        let l2 = cc_assistant(RFC_T2);
        let lines = [l1.as_str(), l2.as_str()];
        let a = analyze_session("claude-code", &lines);
        assert_eq!(a.first_unix, Some(T1));
        assert_eq!(a.last_unix, Some(T2));
        assert_eq!(a.line_count, 2);
        assert_eq!(a.time_source, TimeSource::Exact);
    }

    #[test]
    fn codex_rfc3339_exact_first_last() {
        let l1 = codex(RFC_T1, "user_message");
        let l2 = codex(RFC_T2, "turn_start");
        let lines = [l1.as_str(), l2.as_str()];
        let a = analyze_session("codex", &lines);
        assert_eq!(a.first_unix, Some(T1));
        assert_eq!(a.last_unix, Some(T2));
        assert_eq!(a.time_source, TimeSource::Exact);
    }

    // --------------------------------------------------------------- Inferred
    #[test]
    fn numeric_epoch_millis_is_inferred_not_seconds() {
        // 1736944496 as millis would be year 1970 if read as seconds; magnitude
        // disambiguation must divide by 1000, and label it Inferred.
        let l = codex_ms(1_736_944_496_789);
        let lines = [l.as_str()];
        let a = analyze_session("codex", &lines);
        assert_eq!(a.first_unix, Some(T1));
        let TimeSource::Inferred { how } = &a.time_source else {
            panic!("expected Inferred, got {:?}", a.time_source);
        };
        assert!(how.contains("millis"), "how should say millis: {how}");
    }

    #[test]
    fn numeric_epoch_seconds_is_inferred() {
        let l = cc_epoch(T1);
        let lines = [l.as_str()];
        let a = analyze_session("claude-code", &lines);
        assert_eq!(a.first_unix, Some(T1));
        assert!(matches!(a.time_source, TimeSource::Inferred { .. }));
    }

    // ----------------------------------------------------------------- Unknown
    #[test]
    fn claude_code_lines_without_timestamp_is_unknown_with_why() {
        let l1 = r#"{"type":"summary","sessionId":"s","uuid":"u9","summary":"..."}"#;
        let l2 = r#"{"parentUuid":null,"type":"user","message":{"role":"user","content":"x"},"uuid":"u1","cwd":"/x","version":"1.0.31"}"#;
        let lines = [l1, l2];
        let a = analyze_session("claude-code", &lines);
        assert_eq!(a.first_unix, None);
        let TimeSource::Unknown { why } = &a.time_source else {
            panic!("expected Unknown, got {:?}", a.time_source);
        };
        assert!(
            why.contains("timestamp"),
            "why should mention the field: {why}"
        );
    }

    #[test]
    fn out_of_window_numeric_is_unknown_not_clamped() {
        // A value that, as seconds, is the year 1970 — absurd. Must NOT be used.
        let l = cc_epoch(1_736_944);
        let lines = [l.as_str()];
        let a = analyze_session("claude-code", &lines);
        assert_eq!(a.first_unix, None);
        let TimeSource::Unknown { why } = &a.time_source else {
            panic!("expected Unknown, got {:?}", a.time_source);
        };
        assert!(
            why.contains("plausible"),
            "why should mention the range: {why}"
        );
    }

    #[test]
    fn unparseable_json_lines_is_unknown_with_why() {
        let lines = ["this is not json", "also not json"];
        let a = analyze_session("claude-code", &lines);
        assert_eq!(a.first_unix, None);
        assert!(matches!(a.time_source, TimeSource::Unknown { .. }));
    }

    #[test]
    fn unsupported_harness_is_unknown_distinct_why() {
        let a = analyze_session("aider", &[]);
        let TimeSource::Unknown { why } = &a.time_source else {
            panic!("expected Unknown, got {:?}", a.time_source);
        };
        assert!(
            why.contains("not implemented") || why.contains("harness"),
            "unsupported-harness why must be distinct: {why}"
        );
    }

    #[test]
    fn all_blank_lines_is_no_conversation_content() {
        // ADR-035: a session with no lines at all is "no conversation content",
        // a distinct claim from "a conversation whose time we could not find".
        let lines = ["", "   ", ""];
        let a = analyze_session("claude-code", &lines);
        assert_eq!(a.first_unix, None);
        assert_eq!(a.line_count, 0);
        assert_eq!(a.time_source, TimeSource::NoConversationContent);
    }

    // -------------------------------------------------------------- RFC parser
    #[test]
    fn rfc3339_with_offset_is_converted_to_utc() {
        // 2025-01-15T20:34:56+08:00 == 12:34:56Z
        assert_eq!(parse_rfc3339("2025-01-15T20:34:56+08:00"), Some(T1));
        assert_eq!(parse_rfc3339("2025-01-15T12:34:56Z"), Some(T1));
        assert_eq!(parse_rfc3339("2025-01-15T12:34:56.789Z"), Some(T1));
    }

    #[test]
    fn rfc3339_rejects_garbage() {
        assert_eq!(parse_rfc3339("not a time"), None);
        assert_eq!(parse_rfc3339("2025-01-15"), None);
        assert_eq!(parse_rfc3339("2025-13-15T12:34:56Z"), None);
        assert_eq!(parse_rfc3339("2025-01-15T25:00:00Z"), None);
    }

    // --------------------------------------------------------------- JSONL row
    #[test]
    fn to_jsonl_round_trips_through_serde() {
        let l = cc_user(RFC_T1);
        let row = build_row("s1", "mbp", "claude-code", &[l.as_str()]);
        let line = to_jsonl(&row);
        assert!(line.ends_with('\n'));
        let back: ActivityRow = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(back, row);
    }

    #[test]
    fn serialized_kind_is_exact() {
        let l = cc_user(RFC_T1);
        let row = build_row("s1", "mbp", "claude-code", &[l.as_str()]);
        let line = to_jsonl(&row);
        assert!(line.contains(r#""kind":"exact""#), "line: {line}");
    }

    // ---------------------------------------------------------------- opencode
    // One session = one JSON line (the collect envelope). Message times are
    // epoch millis → Inferred, and message-level (real conversation span).
    #[test]
    fn opencode_message_times_are_inferred_first_last() {
        // m1 = 2025-01-15T12:34:56.789Z (T1), m2 = T2 (T1+4211s), as ms.
        let m1 = 1_736_944_496_789i64;
        let m2 = 1_736_948_707_123i64; // T2 = 1736948707
        let envelope = format!(
            r#"{{"schema":"chat-stasher.opencode.session.v1","session":{{"id":"s1","time_created":{m1},"time_updated":{m2}}},"messages":[{{"id":"m1","session_id":"s1","time_created":{m1},"time_updated":{m1},"parts":[]}},{{"id":"m2","session_id":"s1","time_created":{m2},"time_updated":{m2},"parts":[]}}],"orphan_parts":[]}}"#
        );
        let lines = [envelope.as_str()];
        let a = analyze_session("opencode", &lines);
        assert_eq!(a.first_unix, Some(T1));
        assert_eq!(a.last_unix, Some(T2));
        let TimeSource::Inferred { how } = &a.time_source else {
            panic!("expected Inferred (epoch millis), got {:?}", a.time_source);
        };
        assert!(how.contains("millis"), "how should say millis: {how}");
    }

    #[test]
    fn opencode_empty_messages_falls_back_to_session_time() {
        let m1 = 1_736_944_496_789i64; // T1
        let m2 = 1_736_948_707_123i64; // T2
        let envelope = format!(
            r#"{{"schema":"chat-stasher.opencode.session.v1","session":{{"id":"s1","time_created":{m1},"time_updated":{m2}}},"messages":[],"orphan_parts":[]}}"#
        );
        let lines = [envelope.as_str()];
        let a = analyze_session("opencode", &lines);
        assert_eq!(a.first_unix, Some(T1));
        assert_eq!(a.last_unix, Some(T2));
        assert!(matches!(a.time_source, TimeSource::Inferred { .. }));
    }

    #[test]
    fn opencode_line_without_time_is_unknown_with_why() {
        // Envelope but the session row has no time columns at all.
        let envelope = r#"{"schema":"chat-stasher.opencode.session.v1","session":{"id":"s1"},"messages":[],"orphan_parts":[]}"#;
        let lines = [envelope];
        let a = analyze_session("opencode", &lines);
        assert_eq!(a.first_unix, None);
        let TimeSource::Unknown { why } = &a.time_source else {
            panic!("expected Unknown, got {:?}", a.time_source);
        };
        assert!(
            why.contains("timestamp"),
            "why should mention the missing field: {why}"
        );
    }

    #[test]
    fn opencode_out_of_window_message_time_is_unknown_not_clamped() {
        // A message time that as seconds is year 1970 — absurd, must not be used.
        let envelope = format!(
            r#"{{"schema":"chat-stasher.opencode.session.v1","session":{{"id":"s1","time_created":1736944}},"messages":[{{"id":"m1","session_id":"s1","time_created":1736944,"time_updated":1736944,"parts":[]}}],"orphan_parts":[]}}"#
        );
        let lines = [envelope.as_str()];
        let a = analyze_session("opencode", &lines);
        assert_eq!(a.first_unix, None);
        let TimeSource::Unknown { why } = &a.time_source else {
            panic!("expected Unknown, got {:?}", a.time_source);
        };
        assert!(
            why.contains("plausible"),
            "why should mention the range: {why}"
        );
    }

    // ------------------------------------------------------------------ cursor
    #[test]
    fn cursor_global_created_at_is_inferred() {
        // 1751779149032 ms = 1751779149 s
        let envelope = r#"{"schema":"chat-stasher.sqlite.session.v1","table":"cursorDiskKV","session":{"key":"composerData:aaaaaaaa-1111","value":{"composerId":"a","createdAt":1751779149032}}}"#;
        let lines = [envelope];
        let a = analyze_session("cursor", &lines);
        assert_eq!(a.first_unix, Some(1_751_779_149));
        assert_eq!(a.last_unix, Some(1_751_779_149));
        let TimeSource::Inferred { how } = &a.time_source else {
            panic!(
                "expected Inferred (createdAt is session-level millis), got {:?}",
                a.time_source
            );
        };
        assert!(how.contains("millis"), "how should say millis: {how}");
    }

    #[test]
    fn cursor_legacy_created_at_is_inferred() {
        let envelope = r#"{"schema":"chat-stasher.cursor.legacy.session.v1","session":{"composerId":"a","createdAt":1751779149032,"conversation":[{}]}}"#;
        let lines = [envelope];
        let a = analyze_session("cursor", &lines);
        assert_eq!(a.first_unix, Some(1_751_779_149));
        assert!(matches!(a.time_source, TimeSource::Inferred { .. }));
    }

    #[test]
    fn cursor_line_without_created_at_is_unknown_with_why() {
        let envelope = r#"{"schema":"chat-stasher.sqlite.session.v1","table":"cursorDiskKV","session":{"key":"composerData:a","value":{"composerId":"a"}}}"#;
        let lines = [envelope];
        let a = analyze_session("cursor", &lines);
        assert_eq!(a.first_unix, None);
        let TimeSource::Unknown { why } = &a.time_source else {
            panic!("expected Unknown, got {:?}", a.time_source);
        };
        assert!(
            why.contains("timestamp"),
            "why should mention the missing field: {why}"
        );
    }

    // ---------------------------------------------------------------- gemini-cli
    // One session = one whole-file JSON line. Message timestamps are RFC 3339
    // strings → Exact, message-level.
    #[test]
    fn gemini_message_times_are_exact_first_last() {
        let envelope = format!(
            r#"{{"sessionId":"s1","projectHash":"h","startTime":"{RFC_T1}","lastUpdated":"{RFC_T2}","messages":[{{"id":"m1","timestamp":"{RFC_T1}","type":"user","content":[{{"text":"hi"}}]}},{{"id":"m2","timestamp":"{RFC_T2}","type":"gemini","content":[{{"text":"hi"}}]}}],"kind":"main"}}"#
        );
        let lines = [envelope.as_str()];
        let a = analyze_session("gemini-cli", &lines);
        assert_eq!(a.first_unix, Some(T1));
        assert_eq!(a.last_unix, Some(T2));
        assert_eq!(a.time_source, TimeSource::Exact);
    }

    #[test]
    fn gemini_empty_messages_falls_back_to_start_last_updated() {
        let envelope = format!(
            r#"{{"sessionId":"s1","projectHash":"h","startTime":"{RFC_T1}","lastUpdated":"{RFC_T2}","messages":[],"kind":"main"}}"#
        );
        let lines = [envelope.as_str()];
        let a = analyze_session("gemini-cli", &lines);
        assert_eq!(a.first_unix, Some(T1));
        assert_eq!(a.last_unix, Some(T2));
        assert_eq!(a.time_source, TimeSource::Exact);
    }

    #[test]
    fn gemini_line_without_time_is_unknown_with_why() {
        let envelope = r#"{"sessionId":"s1","projectHash":"h","kind":"main"}"#;
        let lines = [envelope];
        let a = analyze_session("gemini-cli", &lines);
        assert_eq!(a.first_unix, None);
        let TimeSource::Unknown { why } = &a.time_source else {
            panic!("expected Unknown, got {:?}", a.time_source);
        };
        assert!(
            why.contains("timestamp"),
            "why should mention the missing field: {why}"
        );
    }

    /// The real gemini-cli source file is a **pretty-printed JSON document**
    /// split across many lines, not JSONL: no single line parses, and the whole
    /// document must be re-read. This is the W118 "format variant".
    #[test]
    fn gemini_pretty_printed_document_is_read_as_one() {
        let doc = format!(
            "{{\n  \"sessionId\": \"s1\",\n  \"projectHash\": \"h\",\n  \"startTime\": \"{RFC_T1}\",\n  \"lastUpdated\": \"{RFC_T2}\",\n  \"messages\": [\n    {{\n      \"id\": \"m1\",\n      \"timestamp\": \"{RFC_T1}\",\n      \"type\": \"user\"\n    }},\n    {{\n      \"id\": \"m2\",\n      \"timestamp\": \"{RFC_T2}\",\n      \"type\": \"gemini\"\n    }}\n  ],\n  \"kind\": \"main\"\n}}\n"
        );
        let lines: Vec<&str> = doc.lines().collect();
        assert!(
            lines
                .iter()
                .all(|l| serde_json::from_str::<serde_json::Value>(l).is_err()),
            "premise: no individual line parses"
        );
        let a = analyze_session("gemini-cli", &lines);
        assert_eq!(a.first_unix, Some(T1));
        assert_eq!(a.last_unix, Some(T2));
        assert_eq!(a.time_source, TimeSource::Exact);
    }

    // --------------------------------------------------------- kimi-code CLI
    //
    // Kimi Code's agent journal is `agents/main/wire.jsonl`: one wire record per
    // line, `{type, time, …}`, where the writer stamps `time: Date.now()` —
    // **epoch milliseconds** — on every record it appends. The first line is
    // `metadata`, which carries `created_at` instead of `time`. Record
    // vocabulary measured on the installed implementation (Kimi Code 0.39.1,
    // `~/.kimi-code/bin/kimi`, read 2026-09-25) and corroborated on 41 real
    // `wire.jsonl` files by an independent importer
    // (<https://github.com/MemPalace/mempalace/issues/2180>).
    //
    // The conversation ops are the ones that carry a message or a loop step;
    // everything else (`config.update`, `mcp.tools_discovered`,
    // `tools.set_active_tools`, `usage.record`, `llm.request`, `goal.*`,
    // `plan_mode.*`, `permission.*`, `full_compaction.*`, `turn.*` bookkeeping)
    // rebuilds state only. The fixtures below are real-shaped and synthetic.

    /// A user message record: `context.append_message` with its `message`.
    fn kimi_user(ms: i64) -> String {
        format!(
            r#"{{"type":"context.append_message","time":{ms},"message":{{"role":"user","origin":{{"kind":"user"}},"content":[{{"type":"text","text":"synthetic"}}]}}}}"#
        )
    }
    /// A loop-event record: assistant output and tool exchanges live here.
    fn kimi_loop(ms: i64, event: &str) -> String {
        format!(
            r#"{{"type":"context.append_loop_event","time":{ms},"event":{{"type":"{event}"}}}}"#
        )
    }
    /// The journal's first line: no `time`, only `created_at`.
    fn kimi_metadata(ms: i64) -> String {
        format!(r#"{{"type":"metadata","protocol_version":"1.4","created_at":{ms}}}"#)
    }
    /// A state-rebuilding record: not conversation content, but timestamped.
    fn kimi_bookkeeping(ty: &str, ms: i64) -> String {
        format!(r#"{{"type":"{ty}","time":{ms}}}"#)
    }

    #[test]
    fn kimi_code_message_times_are_inferred_first_last() {
        let l1 = kimi_user(T1 * 1000);
        let l2 = kimi_loop(T2 * 1000, "step.begin");
        let lines = [l1.as_str(), l2.as_str()];
        let a = analyze_session("kimi-code", &lines);
        assert_eq!(a.first_unix, Some(T1));
        assert_eq!(a.last_unix, Some(T2));
        let TimeSource::Inferred { how } = &a.time_source else {
            panic!("expected Inferred (epoch millis), got {:?}", a.time_source);
        };
        assert!(how.contains("millis"), "how should say millis: {how}");
        // A local harness names no source zone; the epoch is an absolute instant.
        assert_eq!(a.source_zone, None);
    }

    /// Assistant output never arrives as `context.append_message`; it is
    /// assembled from loop events, so those must carry the conversation span.
    #[test]
    fn kimi_code_loop_events_carry_the_assistant_span() {
        let begin = kimi_loop(T1 * 1000, "step.begin");
        let part = kimi_loop(T2 * 1000, "content.part");
        let lines = [begin.as_str(), part.as_str()];
        let a = analyze_session("kimi-code", &lines);
        assert_eq!((a.first_unix, a.last_unix), (Some(T1), Some(T2)));
        assert!(matches!(a.time_source, TimeSource::Inferred { .. }));
    }

    /// `turn.prompt` is the human's input and is timestamped like the rest.
    #[test]
    fn kimi_code_turn_prompt_counts_as_conversation() {
        let line = kimi_bookkeeping("turn.prompt", T1 * 1000);
        let a = analyze_session("kimi-code", &[line.as_str()]);
        assert_eq!(a.first_unix, Some(T1));
        assert!(matches!(a.time_source, TimeSource::Inferred { .. }));
    }

    /// The composition measured on this machine's three real sessions: an
    /// opening `metadata` line plus `config.update` / `mcp.tools_discovered` /
    /// `tools.set_active_tools` only — no conversation op anywhere. Such a
    /// session is "no conversation content" (ADR-035 class B), not an unknown
    /// time, and the metadata line's `created_at` is never used as one.
    #[test]
    fn kimi_code_metadata_only_session_is_no_conversation_content() {
        let header = kimi_metadata(T1 * 1000);
        let config = kimi_bookkeeping("config.update", T1 * 1000);
        let mcp = kimi_bookkeeping("mcp.tools_discovered", T1 * 1000 + 164);
        let tools = kimi_bookkeeping("tools.set_active_tools", T1 * 1000 + 257);
        let lines = [
            header.as_str(),
            config.as_str(),
            mcp.as_str(),
            tools.as_str(),
        ];
        let a = analyze_session("kimi-code", &lines);
        assert_eq!(a.line_count, 4);
        assert_eq!(a.first_unix, None);
        assert_eq!(a.last_unix, None);
        assert_eq!(a.time_source, TimeSource::NoConversationContent);
    }

    /// A record type we do not recognise is *undetermined*, not bookkeeping:
    /// "we do not know this op" must never be reported as "there was nothing
    /// here" (invariant 1). The session stays Unknown.
    #[test]
    fn kimi_code_unrecognised_record_type_is_not_empty() {
        let header = kimi_metadata(T1 * 1000);
        let future = kimi_bookkeeping("some.future_op", T1 * 1000);
        let lines = [header.as_str(), future.as_str()];
        let a = analyze_session("kimi-code", &lines);
        assert_eq!(a.first_unix, None);
        assert!(
            matches!(a.time_source, TimeSource::Unknown { .. }),
            "an unclassified op must stay Unknown, got {:?}",
            a.time_source
        );
    }

    /// The W116 review finding: one recognised, timestamped conversation record
    /// does **not** license a complete range while an unrecognised record is
    /// present, because that record may be conversation carrying a time outside
    /// the recorded one. The bounds stay (they are measured); what changes is
    /// what they claim to be.
    #[test]
    fn kimi_code_unrecognised_record_makes_the_range_partial() {
        let user = kimi_user(T1 * 1000);
        let step = kimi_loop(T2 * 1000, "step.begin");
        // An op added by a future release: undetermined, timestamped after the
        // recorded span ends.
        let future = kimi_bookkeeping("some.future_op", (T2 + 3600) * 1000);
        let lines = [user.as_str(), step.as_str(), future.as_str()];
        let a = analyze_session("kimi-code", &lines);

        assert_eq!(
            (a.first_unix, a.last_unix),
            (Some(T1), Some(T2)),
            "the measured bounds are kept — withdrawing them would delete a measurement"
        );
        let TimeSource::PartialRange { how, why } = &a.time_source else {
            panic!("expected PartialRange, got {:?}", a.time_source);
        };
        assert!(
            how.contains("millis"),
            "the bounds we do have must still say how they were read: {how}"
        );
        assert!(
            why.contains("does not recognise"),
            "the reason must name the unclassified record: {why}"
        );
        assert!(
            !why.contains(&T2.to_string()) && !why.contains(&T1.to_string()),
            "the reason is a grouping key in the human views, so it must not carry \
             per-session numbers: {why}"
        );
        assert!(
            !a.time_source.is_no_conversation_content(),
            "this session does hold conversation, so it is not the ADR-035 class B state"
        );
        assert!(
            a.time_source.bounds_are_partial(),
            "consumers must be able to ask this without reading the variant"
        );
    }

    /// The same hole, one field over: a record we positively classified as
    /// conversation whose own timestamp cannot be read is just as unplaceable as
    /// an op we do not know, so it must not be folded into a complete range.
    #[test]
    fn kimi_code_conversation_record_without_a_time_makes_the_range_partial() {
        let user = kimi_user(T1 * 1000);
        // `restore(record)` in the installed implementation reads
        // `record.time ?? Date.now()`, so a stored conversation record may have
        // no `time` at all — this is a real shape, not a hypothetical.
        let untimed = r#"{"type":"context.append_message","message":{"role":"user","origin":{"kind":"user"}}}"#;
        let a = analyze_session("kimi-code", &[user.as_str(), untimed]);

        assert_eq!(a.first_unix, Some(T1));
        let TimeSource::PartialRange { why, .. } = &a.time_source else {
            panic!("expected PartialRange, got {:?}", a.time_source);
        };
        assert!(why.contains("no timestamp field"), "{why}");
    }

    /// And the unreadable-timestamp end of it.
    #[test]
    fn kimi_code_conversation_record_with_an_unreadable_time_makes_the_range_partial() {
        let user = kimi_user(T1 * 1000);
        // Out of the plausible window: never clamped, never used, and never
        // allowed to look like a bound that is the whole span.
        let bogus = kimi_loop(1_736_944, "step.begin");
        let a = analyze_session("kimi-code", &[user.as_str(), bogus.as_str()]);

        assert_eq!(a.first_unix, Some(T1));
        let TimeSource::PartialRange { why, .. } = &a.time_source else {
            panic!("expected PartialRange, got {:?}", a.time_source);
        };
        assert!(why.contains("could not be read"), "{why}");
    }

    /// Claude Code has the same classification (`Some(_)` metadata, `None`
    /// undetermined), so the rule is the harness's shape, not Kimi's, and the
    /// `how` names the RFC 3339 read rather than the epoch-millis one.
    #[test]
    fn claude_code_untyped_record_makes_the_range_partial() {
        let user = cc_user(RFC_T1);
        let junk = r#"{"sessionId":"s","uuid":"u9"}"#;
        let a = analyze_session("claude-code", &[user.as_str(), junk]);

        assert_eq!(a.first_unix, Some(T1));
        let TimeSource::PartialRange { how, why } = &a.time_source else {
            panic!("expected PartialRange, got {:?}", a.time_source);
        };
        assert!(how.contains("RFC 3339"), "{how}");
        assert!(why.contains("does not recognise"), "{why}");
    }

    /// The control: with every record classified and timestamped, the range is
    /// still the plain inferred one. A partial range must not become the answer
    /// for the ordinary case.
    #[test]
    fn kimi_code_fully_recognised_records_stay_inferred() {
        let user = kimi_user(T1 * 1000);
        let step = kimi_loop(T2 * 1000, "step.begin");
        let a = analyze_session("kimi-code", &[user.as_str(), step.as_str()]);

        assert!(
            matches!(a.time_source, TimeSource::Inferred { .. }),
            "no unplaceable record here, so nothing licenses a partial range: {:?}",
            a.time_source
        );
        assert!(!a.time_source.bounds_are_partial());
    }

    /// The journal's own reader warns about corrupted lines, so a line that
    /// does not parse must not be read as "no content".
    #[test]
    fn kimi_code_corrupt_line_is_not_empty() {
        let header = kimi_metadata(T1 * 1000);
        let lines = [header.as_str(), r#"{"type":"context.append_me"#];
        let a = analyze_session("kimi-code", &lines);
        assert_eq!(a.first_unix, None);
        let TimeSource::Unknown { why } = &a.time_source else {
            panic!("expected Unknown, got {:?}", a.time_source);
        };
        assert!(why.contains("could not be parsed"), "{why}");
    }

    /// A real conversation whose records somehow carry no `time` is Unknown
    /// with the "no timestamp field" reason — not "no content", and not
    /// "not implemented".
    #[test]
    fn kimi_code_conversation_without_time_is_unknown() {
        let lines = [r#"{"type":"context.append_message","message":{"role":"user"}}"#];
        let a = analyze_session("kimi-code", &lines);
        assert_eq!(a.first_unix, None);
        let TimeSource::Unknown { why } = &a.time_source else {
            panic!("expected Unknown, got {:?}", a.time_source);
        };
        assert!(!why.contains("not implemented"), "{why}");
        assert!(why.contains("timestamp"), "{why}");
    }

    /// A `time` outside the plausible window is never silently clamped.
    #[test]
    fn kimi_code_out_of_window_time_is_unknown_not_clamped() {
        // As epoch millis this is 1970; as seconds it would be a plausible
        // millis value. Magnitude must not rescue an absurd time.
        let line = kimi_bookkeeping("turn.prompt", 1_736_944);
        let a = analyze_session("kimi-code", &[line.as_str()]);
        assert_eq!(a.first_unix, None);
        let TimeSource::Unknown { why } = &a.time_source else {
            panic!("expected Unknown, got {:?}", a.time_source);
        };
        assert!(
            why.contains("plausible"),
            "why should mention the range: {why}"
        );
    }

    /// The CLI harness must stop reporting itself as unimplemented, while the
    /// web harness of the same name (`kimi`) keeps its own reader.
    #[test]
    fn kimi_code_no_longer_reports_not_implemented() {
        let line = kimi_user(T1 * 1000);
        let a = analyze_session("kimi-code", &[line.as_str()]);
        assert_eq!(a.first_unix, Some(T1));
        let empty = analyze_session("kimi-code", &[""]);
        assert_eq!(empty.time_source, TimeSource::NoConversationContent);
    }

    // ------------------------------------------------------------------- grok
    /// The grok **CLI** archives one `session_docs` SQLite row per session with
    /// the session's own `updated_at` (epoch seconds). The web-extension grok
    /// bundle is a different shape and must still go through the web reader.
    #[test]
    fn grok_cli_updated_at_is_inferred() {
        let envelope = format!(
            r#"{{"schema":"chat-stasher.sqlite.session.v1","table":"session_docs","session":{{"session_id":"s1","updated_at":{T2},"title":"synthetic Grok title","content":"synthetic body"}}}}"#
        );
        let lines = [envelope.as_str()];
        let a = analyze_session("grok", &lines);
        assert_eq!(a.first_unix, Some(T2));
        assert_eq!(a.last_unix, Some(T2));
        assert!(matches!(a.time_source, TimeSource::Inferred { .. }));
        assert_eq!(
            a.title,
            SessionTitle::Known {
                text: "synthetic Grok title".to_string(),
                source: TitleSource::HarnessTitle,
                truncated: false,
            }
        );
    }

    #[test]
    fn grok_cli_title_requires_a_session_docs_record_and_trims_whitespace() {
        let envelope = r#"{"schema":"chat-stasher.sqlite.session.v1","table":"session_docs","session":{"session_id":"opaque","updated_at":1789384914,"title":"  synthetic title  ","content":"synthetic body"}}"#;
        let a = analyze_session("grok", &[envelope]);
        assert_eq!(
            a.title,
            SessionTitle::Known {
                text: "synthetic title".to_string(),
                source: TitleSource::HarnessTitle,
                truncated: false,
            }
        );

        // A web payload or an unrelated envelope must not lend its title to
        // the Grok CLI reader merely because it has a top-level `title` key.
        let unrelated = r#"{"title":"synthetic unrelated title","content":"synthetic body"}"#;
        assert_eq!(
            analyze_session("grok", &[unrelated]).title,
            SessionTitle::NoLabelRecorded
        );
    }

    #[test]
    fn grok_web_bundle_still_reads_responses() {
        let body = serde_json::json!({
            "responses": [ { "createTime": RFC_T1 }, { "createTime": RFC_T2 } ],
        })
        .to_string();
        let line = web_line("grok", &body);
        let a = analyze_session("grok", &[line.as_str()]);
        assert_eq!(a.first_unix, Some(T1));
        assert_eq!(a.last_unix, Some(T2));
        assert_eq!(a.time_source, TimeSource::Messages { exact: true });
    }

    /// perplexity writes an offset-less `updated_datetime` (refused by zone
    /// discipline) **and** an absolute microsecond epoch `updated_us`;
    /// the microsecond value is the honest time source.
    #[test]
    fn perplexity_microsecond_epoch_is_inferred() {
        let body = serde_json::json!({
            "entries": [ { "updated_datetime": "2026-07-18T15:54:22.049009", "updated_us": T2 * 1_000_000 } ],
        })
        .to_string();
        let line = web_line("perplexity", &body);
        let a = analyze_session("perplexity", &[line.as_str()]);
        assert_eq!(a.first_unix, Some(T2));
        assert_eq!(a.last_unix, Some(T2));
        assert_eq!(a.time_source, TimeSource::Messages { exact: false });
    }

    // --------------------------------------------------- no conversation content
    #[test]
    fn claude_code_summary_only_is_no_conversation_content() {
        // A file whose every line is metadata: no user/assistant message.
        let lines = [
            r#"{"type":"summary","sessionId":"s","uuid":"u9","summary":"..."}"#,
            r#"{"type":"file-history-snapshot","messageId":"m","snapshot":{},"isSnapshotUpdate":false}"#,
            r#"{"type":"bridge-session","sessionId":"s","lastSequenceNum":3}"#,
        ];
        let a = analyze_session("claude-code", &lines);
        assert_eq!(a.first_unix, None);
        assert_eq!(a.time_source, TimeSource::NoConversationContent);
    }

    #[test]
    fn claude_code_timestamped_summary_is_not_conversation_activity() {
        let line = format!(
            r#"{{"type":"summary","sessionId":"s","uuid":"u9","summary":"...","timestamp":"{RFC_T1}"}}"#
        );
        let a = analyze_session("claude-code", &[line.as_str()]);
        assert_eq!(a.first_unix, None);
        assert_eq!(a.last_unix, None);
        assert_eq!(a.line_count, 1);
        assert_eq!(a.time_source, TimeSource::NoConversationContent);
    }

    #[test]
    fn claude_code_message_without_timestamp_stays_unknown() {
        // The same file shape but one real user line: it is a conversation, so
        // a missing timestamp is Unknown, never "no content".
        let lines = [
            r#"{"type":"summary","sessionId":"s","uuid":"u9","summary":"..."}"#,
            r#"{"type":"user","message":{"role":"user","content":"x"},"uuid":"u1"}"#,
        ];
        let a = analyze_session("claude-code", &lines);
        assert_eq!(a.first_unix, None);
        assert!(matches!(a.time_source, TimeSource::Unknown { .. }));
    }

    #[test]
    fn unsupported_harness_with_no_lines_stays_unknown() {
        // ADR-035 "no content" is only asserted for harnesses whose shape we
        // know; an unsupported harness keeps its explicit "not implemented".
        let a = analyze_session("aider", &[]);
        assert!(matches!(a.time_source, TimeSource::Unknown { .. }));
    }

    // --------------------------------------------------------- web chat harnesses
    // Each case is a synthetic but real-shaped inbox bundle: the platform's own
    // body under `raw.text`, exactly as `inbox.rs` seals it. No real content.

    fn web_line(platform: &str, body: &str) -> String {
        serde_json::json!({
            "schema": "chat-stasher/inbox@2",
            "platform": platform,
            "sessionId": "s1",
            "raw": { "text": body, "bytes": body.len() },
        })
        .to_string()
    }

    #[test]
    fn chatgpt_message_times_are_message_derived() {
        let body = serde_json::json!({
            "title": "t",
            "create_time": T1,
            "update_time": T2,
            "mapping": {
                "n1": { "message": { "create_time": T1 } },
                "n2": { "message": { "create_time": T2 } },
            },
        })
        .to_string();
        let line = web_line("chatgpt", &body);
        let a = analyze_session("chatgpt", &[line.as_str()]);
        assert_eq!(a.first_unix, Some(T1));
        assert_eq!(a.last_unix, Some(T2));
        assert_eq!(a.time_source, TimeSource::Messages { exact: false });
        assert_eq!(a.source_zone.as_deref(), Some("UTC"));
    }

    /// A numeric `inserted_at` is a Unix epoch: an absolute instant. The parser
    /// must not shift it, and records the value's own zone as UTC.
    #[test]
    fn deepseek_message_times_are_absolute_epochs_not_shifted() {
        let body = serde_json::json!({
            "data": { "biz_data": {
                "chat_messages": [
                    { "inserted_at": T1 },
                    { "inserted_at": T2 },
                ],
                "chat_session": { "inserted_at": T1, "updated_at": T2 },
            } },
        })
        .to_string();
        let line = web_line("deepseek", &body);
        let a = analyze_session("deepseek", &[line.as_str()]);
        assert_eq!(
            a.first_unix,
            Some(T1),
            "the epoch is the instant as stored; it must not be shifted by any zone"
        );
        assert_eq!(a.last_unix, Some(T2));
        assert_eq!(a.source_zone.as_deref(), Some("UTC"));
        assert_eq!(a.time_source, TimeSource::Messages { exact: false });
    }

    #[test]
    fn claude_web_message_times_are_exact() {
        let body = serde_json::json!({
            "uuid": "u",
            "created_at": RFC_T1,
            "updated_at": RFC_T2,
            "chat_messages": [
                { "created_at": RFC_T1 },
                { "created_at": RFC_T2 },
            ],
        })
        .to_string();
        let line = web_line("claude", &body);
        let a = analyze_session("claude", &[line.as_str()]);
        assert_eq!(a.first_unix, Some(T1));
        assert_eq!(a.last_unix, Some(T2));
        assert_eq!(a.time_source, TimeSource::Messages { exact: true });
        assert_eq!(a.source_zone.as_deref(), Some("UTC"));
    }

    /// A `+08:00` RFC 3339 string is converted to UTC and its offset recorded.
    #[test]
    fn rfc3339_offset_is_converted_and_its_zone_recorded() {
        let body = r#"{"chat_messages":[{"created_at":"2025-01-15T20:34:56+08:00"}]}"#;
        let line = web_line("claude", body);
        let a = analyze_session("claude", &[line.as_str()]);
        assert_eq!(a.first_unix, Some(T1));
        assert_eq!(a.source_zone.as_deref(), Some("+08:00"));
    }

    #[test]
    fn grok_message_times_from_responses() {
        let body = serde_json::json!({
            "responses": [
                { "createTime": RFC_T1 },
                { "createTime": RFC_T2 },
            ],
        })
        .to_string();
        let line = web_line("grok", &body);
        let a = analyze_session("grok", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T1), Some(T2)));
        assert_eq!(a.time_source, TimeSource::Messages { exact: true });
    }

    #[test]
    fn kimi_message_times_are_message_derived() {
        let body = serde_json::json!({
            "messages": [
                { "createTime": RFC_T1 },
                { "createTime": RFC_T2 },
            ],
        })
        .to_string();
        let line = web_line("kimi", &body);
        let a = analyze_session("kimi", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T1), Some(T2)));
        assert_eq!(a.time_source, TimeSource::Messages { exact: true });
    }

    #[test]
    fn perplexity_entry_times_are_message_derived() {
        let body = serde_json::json!({
            "entries": [
                { "updated_datetime": RFC_T1 },
                { "updated_datetime": RFC_T2 },
            ],
        })
        .to_string();
        let line = web_line("perplexity", &body);
        let a = analyze_session("perplexity", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T1), Some(T2)));
        assert_eq!(a.time_source, TimeSource::Messages { exact: true });
    }

    #[test]
    fn gemini_batchexecute_turn_times_are_message_derived() {
        // turn[4] = [seconds, nanos]; payload[0] is the turns array.
        let turn = |secs: i64| serde_json::json!([["c_x", "r_y"], null, null, null, [secs, 0]]);
        let inner = serde_json::json!([[turn(T1), turn(T2)]]);
        let entry = serde_json::json!([
            "wrb.fr",
            "hNvQHb",
            inner.to_string(),
            null,
            null,
            null,
            "generic"
        ]);
        let frame = serde_json::json!([entry]).to_string();
        let page = format!(")]}}'\n\n{}\n{}", frame.len(), frame);
        let bundle = serde_json::json!({
            "rpcid": "hNvQHb",
            "conversationId": "c_x",
            "pages": [page],
        })
        .to_string();
        let line = web_line("gemini", &bundle);
        let a = analyze_session("gemini", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T1), Some(T2)));
        assert_eq!(a.time_source, TimeSource::Messages { exact: false });
    }

    /// A frame's declared length prefix is a **UTF-16 code-unit count**, not a
    /// byte count. When the JSON holds CJK/emoji the two differ, and a
    /// byte-indexed cut either truncates the frame or slices inside a char.
    fn utf16_len(text: &str) -> usize {
        text.encode_utf16().count()
    }

    #[test]
    fn gemini_batchexecute_frame_with_multibyte_json_uses_utf16_length() {
        let turn = |secs: i64| serde_json::json!([["c_x", "r_y"], null, null, null, [secs, 0]]);
        let inner = serde_json::json!([[turn(T1), turn(T2)], ["café résumé 😀"]]);
        let entry = serde_json::json!([
            "wrb.fr",
            "hNvQHb",
            inner.to_string(),
            null,
            null,
            null,
            "generic"
        ]);
        let frame = serde_json::json!([entry]).to_string();
        assert!(
            frame.len() > utf16_len(&frame),
            "the frame must contain multibyte text for this test to mean anything"
        );
        let page = format!(")]}}'\n\n{}\n{}", utf16_len(&frame), frame);
        let bundle = serde_json::json!({ "pages": [page] }).to_string();
        let line = web_line("gemini", &bundle);
        let a = analyze_session("gemini", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T1), Some(T2)));
        assert_eq!(a.time_source, TimeSource::Messages { exact: false });
    }

    /// The measured real framing declares `JSON line + 2` UTF-16 units. That
    /// slice must fall back to the newline-delimited line rather than drop the
    /// frame (or slice mid-char).
    #[test]
    fn gemini_batchexecute_declared_length_plus_two_falls_back_to_line() {
        let turn = |secs: i64| serde_json::json!([["c_x", "r_y"], null, null, null, [secs, 0]]);
        let inner = serde_json::json!([[turn(T1), turn(T2)], ["café 😀"]]);
        let entry = serde_json::json!([
            "wrb.fr",
            "hNvQHb",
            inner.to_string(),
            null,
            null,
            null,
            "generic"
        ]);
        let frame = serde_json::json!([entry]).to_string();
        let page = format!(")]}}'\n\n{}\n{}", utf16_len(&frame) + 2, frame);
        let bundle = serde_json::json!({ "pages": [page] }).to_string();
        let line = web_line("gemini", &bundle);
        let a = analyze_session("gemini", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T1), Some(T2)));
    }

    /// A digit prefix that points somewhere useless must not consume the frame:
    /// the newline-delimited line is read instead.
    #[test]
    fn gemini_batchexecute_garbage_prefix_falls_back_to_line() {
        let turn = |secs: i64| serde_json::json!([["c_x", "r_y"], null, null, null, [secs, 0]]);
        let inner = serde_json::json!([[turn(T1), turn(T2)], ["café 😀"]]);
        let entry = serde_json::json!([
            "wrb.fr",
            "hNvQHb",
            inner.to_string(),
            null,
            null,
            null,
            "generic"
        ]);
        let frame = serde_json::json!([entry]).to_string();
        let page = format!(")]}}'\n\n1\n{}", frame);
        let bundle = serde_json::json!({ "pages": [page] }).to_string();
        let line = web_line("gemini", &bundle);
        let a = analyze_session("gemini", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T1), Some(T2)));
    }

    /// No input may panic: the walker must never slice a `&str` at a byte index
    /// inside a character. Every declared length around a multibyte body is
    /// tried; returning frames (or none) is fine, panicking is not.
    #[test]
    fn gemini_frame_walker_never_panics_on_multibyte_cut_points() {
        let payload = "é😀xyz";
        for declared in 0..=(utf16_len(payload) + 4) {
            let body = format!("{declared}\n{payload}\n");
            let _ = parse_batchexecute_frames(&body);
        }
    }

    /// No messages archived yet: the span is only the list/update time, and its
    /// low confidence is named.
    #[test]
    fn claude_web_list_only_is_list_updated() {
        // created and updated differ by a year: the list-only interval is the
        // updated time as a point, never the created..updated span.
        let body = serde_json::json!({
            "uuid": "u",
            "created_at": RFC_T1,
            "updated_at": RFC_T2,
        })
        .to_string();
        let line = web_line("claude", &body);
        let a = analyze_session("claude", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T2), Some(T2)));
        assert_eq!(a.time_source, TimeSource::ListUpdated);
    }

    #[test]
    fn deepseek_list_only_is_list_updated() {
        let body = serde_json::json!({
            "data": { "biz_data": {
                "chat_session": { "inserted_at": T1, "updated_at": T2 },
            } },
        })
        .to_string();
        let line = web_line("deepseek", &body);
        let a = analyze_session("deepseek", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T2), Some(T2)));
        assert_eq!(a.source_zone.as_deref(), Some("UTC"));
        assert_eq!(a.time_source, TimeSource::ListUpdated);
    }

    #[test]
    fn grok_list_only_is_the_modify_time_point() {
        // The list folds `createTime` and `modifyTime`; the interval is only the
        // update end, as a point.
        let body = serde_json::json!({
            "conversation_v2": {
                "conversation": { "createTime": RFC_T1, "modifyTime": RFC_T2 },
            },
        })
        .to_string();
        let line = web_line("grok", &body);
        let a = analyze_session("grok", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T2), Some(T2)));
        assert_eq!(a.time_source, TimeSource::ListUpdated);
    }

    #[test]
    fn kimi_list_only_is_the_update_time_point() {
        let body = serde_json::json!({
            "chat": { "createTime": RFC_T1, "updateTime": RFC_T2 },
        })
        .to_string();
        let line = web_line("kimi", &body);
        let a = analyze_session("kimi", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T2), Some(T2)));
        assert_eq!(a.time_source, TimeSource::ListUpdated);
    }

    #[test]
    fn gemini_list_only_items_fall_back_to_list_updated() {
        // payload[2] = items, item[5] = [seconds, nanos].
        let item = |secs: i64| serde_json::json!([null, "t", null, null, null, [secs, 0]]);
        let inner = serde_json::json!([[], null, [item(T1)]]);
        let entry = serde_json::json!([
            "wrb.fr",
            "MaZiqc",
            inner.to_string(),
            null,
            null,
            null,
            "generic"
        ]);
        let frame = serde_json::json!([entry]).to_string();
        let page = format!(")]}}'\n\n{}\n{}", frame.len(), frame);
        let bundle = serde_json::json!({ "pages": [page] }).to_string();
        let line = web_line("gemini", &bundle);
        let a = analyze_session("gemini", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T1), Some(T1)));
        assert_eq!(a.time_source, TimeSource::ListUpdated);
    }

    /// A web payload that carries no time field at all is Unknown, and its
    /// reason is the "no timestamp field" one, not "not implemented".
    #[test]
    fn web_payload_without_time_is_unknown_and_not_implemented_is_gone() {
        let body = r#"{"messages":[{"role":"user"}]}"#;
        let line = web_line("kimi", body);
        let a = analyze_session("kimi", &[line.as_str()]);
        assert_eq!(a.first_unix, None);
        let TimeSource::Unknown { why } = &a.time_source else {
            panic!("expected Unknown, got {:?}", a.time_source);
        };
        assert!(!why.contains("not implemented"), "{why}");
        assert!(why.contains("timestamp"), "{why}");
    }

    #[test]
    fn web_harness_overview_why_no_longer_says_not_implemented() {
        let line = web_line("chatgpt", r#"{"title":"t"}"#);
        let a = analyze_session("chatgpt", &[line.as_str()]);
        let TimeSource::Unknown { why } = &a.time_source else {
            panic!("expected Unknown, got {:?}", a.time_source);
        };
        assert!(!why.contains("not implemented"), "{why}");
    }

    /// An `activity-v1.jsonl` line written before `source_zone` existed must
    /// still deserialize, so `overview`/`search` keep reading old destinations.
    #[test]
    fn activity_row_without_source_zone_still_deserializes() {
        let old = r#"{"session_id":"s","machine":"m","harness":"claude-code","first_unix":1736944496,"last_unix":1736948707,"line_count":2,"time_source":{"kind":"exact"}}"#;
        let row: ActivityRow = serde_json::from_str(old).expect("pre-W97 row must deserialize");
        assert_eq!(row.source_zone, None);
    }

    // ------------------------------------------------------------- W156 label

    fn cc_ai_title(title: &str) -> String {
        format!(r#"{{"type":"ai-title","aiTitle":"{title}","sessionId":"s"}}"#)
    }
    fn cc_summary(summary: &str) -> String {
        format!(r#"{{"type":"summary","summary":"{summary}","leafUuid":"u0"}}"#)
    }
    fn cc_prompt(content: &str) -> String {
        format!(
            r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"{content}"}},"uuid":"u1","timestamp":"{RFC_T1}","cwd":"/x","version":"1.0.31"}}"#
        )
    }

    /// The harness's own title wins over the first user line, whatever order
    /// the lines arrive in — and the *last* `ai-title` is the current one.
    #[test]
    fn claude_code_ai_title_wins_and_the_last_one_is_current() {
        let user = cc_prompt("please help me sort a failing retry case");
        let old_title = cc_ai_title("An earlier title");
        let new_title = cc_ai_title("Fix the parser retry loop");
        let a = analyze_session("claude-code", &[&user, &old_title, &new_title]);
        assert_eq!(
            a.title,
            SessionTitle::Known {
                text: "Fix the parser retry loop".into(),
                source: TitleSource::HarnessTitle,
                truncated: false,
            }
        );
    }

    /// With no `ai-title`, a `summary` line is the harness's own label — the
    /// first one, because it is written at the head of the continuation it
    /// describes.
    #[test]
    fn claude_code_summary_is_the_harness_title_when_no_ai_title() {
        let first = cc_summary("Continuing the parser work");
        let second = cc_summary("A second summary, later in the file");
        let user = cc_prompt("and where were we");
        let a = analyze_session("claude-code", &[&first, &second, &user]);
        assert_eq!(
            a.title,
            SessionTitle::Known {
                text: "Continuing the parser work".into(),
                source: TitleSource::HarnessTitle,
                truncated: false,
            }
        );
    }

    /// With no title lines at all, the first user line's head is the label,
    /// capped at 100 characters and flagged when the cap cut anything.
    #[test]
    fn claude_code_first_user_line_is_capped_and_flagged() {
        let over_cap: String = "word ".repeat(30); // 150 chars, single line
        assert!(over_cap.chars().count() > TITLE_CAP_CHARS);
        let user = cc_prompt(&over_cap);
        let a = analyze_session("claude-code", &[user.as_str()]);
        let SessionTitle::Known {
            text,
            source,
            truncated,
        } = &a.title
        else {
            panic!("expected a known title, got {:?}", a.title);
        };
        assert_eq!(source, &TitleSource::FirstUserLine);
        assert_eq!(text.chars().count(), TITLE_CAP_CHARS);
        assert!(*truncated, "the cut must be on record, not visual only");
    }

    /// A first user line whose content is the typed-blocks array shape is read
    /// from its `text` block — and a tool-result-only user line never becomes
    /// the label, because it is a tool answering, not a person asking.
    #[test]
    fn claude_code_first_user_line_comes_from_the_text_block() {
        let tool_result = r#"{"parentUuid":"u1","isMeta":null,"sessionId":"s","type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"12 files changed"}]},"uuid":"u2","timestamp":"RFC_T1","cwd":"/x","version":"1.0.31"}"#.replace("RFC_T1", RFC_T1);
        let blocks = r#"{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{"role":"user","content":[{"type":"text","text":"extract the retry policy from the config"}]},"uuid":"u1","timestamp":"RFC_T1","cwd":"/x","version":"1.0.31"}"#.replace("RFC_T1", RFC_T1);
        let a = analyze_session("claude-code", &[tool_result.as_str(), blocks.as_str()]);
        assert_eq!(
            a.title,
            SessionTitle::Known {
                text: "extract the retry policy from the config".into(),
                source: TitleSource::FirstUserLine,
                truncated: false,
            }
        );
    }

    /// A user line the harness marked `isMeta` is the harness talking to
    /// itself; the label waits for a line a person actually typed.
    #[test]
    fn claude_code_meta_user_lines_do_not_become_the_label() {
        let meta = r#"{"parentUuid":null,"isMeta":true,"sessionId":"s","type":"user","message":{"role":"user","content":"Caveat: the messages below were generated"},"uuid":"u0","timestamp":"RFC_T1","cwd":"/x","version":"1.0.31"}"#.replace("RFC_T1", RFC_T1);
        let prompt = cc_prompt("what is the ownership model");
        let a = analyze_session("claude-code", &[meta.as_str(), prompt.as_str()]);
        assert_eq!(
            a.title,
            SessionTitle::Known {
                text: "what is the ownership model".into(),
                source: TitleSource::FirstUserLine,
                truncated: false,
            }
        );
    }

    /// A session whose lines are all metadata records an honest "no label
    /// recorded" — never an empty string, never a guess.
    #[test]
    fn claude_code_metadata_only_session_records_no_label() {
        let snapshot =
            r#"{"type":"file-history-snapshot","sessionId":"s","uuid":"u7"}"#.to_string();
        let a = analyze_session("claude-code", &[snapshot.as_str()]);
        assert_eq!(a.title, SessionTitle::NoLabelRecorded);
    }

    /// A harness whose lines this module does not read a title from records
    /// the same "no label recorded", by design (29-UI-DESIGN §2.2: no label
    /// rather than a guess or an empty string) — its prompt text is NOT quoted
    /// into the label.
    #[test]
    fn a_harness_without_a_title_reader_records_no_label_by_design() {
        let codex = codex(RFC_T1, "user_message");
        let a = analyze_session("codex", &[codex.as_str()]);
        assert_eq!(a.title, SessionTitle::NoLabelRecorded);
    }

    /// The label is the `ListUpdated` row's metadata **from the same body**: a
    /// body the time pass reads as this session's own list-level record labels
    /// from that body. Claude has two such shapes — one metadata record, and a
    /// page holding exactly one — and both are shapes `claude_span` already
    /// attributes to this session.
    #[test]
    fn a_web_list_record_labels_from_the_body_that_gives_it_its_time() {
        let expected = SessionTitle::Known {
            text: "synthetic list label".into(),
            source: TitleSource::HarnessTitle,
            truncated: false,
        };
        for body in [
            serde_json::json!({"uuid":"u","name":"synthetic list label","created_at":RFC_T1,"updated_at":RFC_T2}),
            serde_json::json!([{"uuid":"u","name":"synthetic list label","created_at":RFC_T1,"updated_at":RFC_T2}]),
        ] {
            let line = web_line("claude", &body.to_string());
            let a = analyze_session("claude", &[line.as_str()]);
            assert_eq!(a.time_source, TimeSource::ListUpdated, "{body}");
            assert_eq!(a.title, expected, "{body}");
        }
    }

    /// …and a body the time pass reads no list-level time from labels nothing,
    /// even when it carries the very field a label is read from. ChatGPT's
    /// list rows carry ISO-string times, which is not a time this reader reads,
    /// so such a body is not this session's own record and its `title` is not
    /// lent to the session it was filed under.
    #[test]
    fn a_web_body_without_a_list_time_labels_nothing() {
        let row = serde_json::json!({
            "id": "u",
            "title": "synthetic chatgpt list row",
            "create_time": RFC_T1,
            "update_time": RFC_T2,
        })
        .to_string();
        let line = web_line("chatgpt", &row);
        let a = analyze_session("chatgpt", &[line.as_str()]);
        assert_eq!(a.title, SessionTitle::NoLabelRecorded);
    }

    /// A page of several conversations is nobody's own record: the label is read
    /// from the row only when the page holds one, so no entry's name is lent to
    /// whatever session the page was filed under.
    #[test]
    fn a_multi_row_web_page_labels_nothing() {
        let page = serde_json::json!([
            {"uuid":"u1","name":"synthetic first","created_at":RFC_T1,"updated_at":RFC_T2},
            {"uuid":"u2","name":"synthetic second","created_at":RFC_T1,"updated_at":RFC_T2},
        ])
        .to_string();
        let line = web_line("claude", &page);
        let a = analyze_session("claude", &[line.as_str()]);
        assert_eq!(a.title, SessionTitle::NoLabelRecorded);
    }

    /// An `activity-v1.jsonl` line written before `title` existed must still
    /// deserialize — and read back as `None`, which is the machine-level
    /// "index predates labels" state, not "no label".
    #[test]
    fn activity_row_without_title_still_deserializes_as_predates() {
        let old = r#"{"session_id":"s","machine":"m","harness":"claude-code","first_unix":1736944496,"last_unix":1736948707,"line_count":2,"time_source":{"kind":"exact"},"source_zone":null}"#;
        let row: ActivityRow = serde_json::from_str(old).expect("pre-W156 row must deserialize");
        assert_eq!(row.title, None);
    }

    /// The row's title round-trips through the wire shape the whole pipeline
    /// agrees on: `{"state":"known","text":…,"source":…,"truncated":…}` and
    /// `{"state":"no_label_recorded"}`.
    #[test]
    fn title_states_round_trip_through_the_index_line() {
        let known = build_row(
            "claude-code.mbp.019bf00d-0000-0000-0000-000000000001",
            "mbp",
            "claude-code",
            &[cc_ai_title("Fix the parser retry loop").as_str()],
        );
        let line = to_jsonl(&known);
        assert!(
            line.contains(r#""title":{"state":"known""#),
            "the wire shape is the contract the UI consumes: {line}"
        );
        assert!(line.contains(r#""source":"harness_title""#), "{line}");
        let back: ActivityRow = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(back.title, known.title);

        let none_label = build_row(
            "codex.mbp.99000001-0000-0000-0000-000000000002",
            "mbp",
            "codex",
            &[codex(RFC_T1, "user_message").as_str()],
        );
        let line = to_jsonl(&none_label);
        assert!(
            line.contains(r#""title":{"state":"no_label_recorded"}"#),
            "the absence must be a recorded state, never a missing string: {line}"
        );
    }

    #[test]
    fn activity_index_keeps_unknown_capture_and_exposes_later_supplement() {
        let capture = r#"{"provenance":{"workspace":"unknown","project":"unknown","archived":false},"raw":{"text":"{}"}}"#;
        let supplement = r#"{"provenanceSupplement":{"workspace":"workspace-fixture","project":{"id":"project-fixture","name":"Synthetic Project"},"source":"project-list","observedAt":"2026-09-25T12:00:00.000Z"},"raw":{"text":"{}"}}"#;
        let row = build_row(
            "chatgpt.session-fixture",
            "machine-fixture",
            "chatgpt",
            &[capture, supplement],
        );
        let provenance = row
            .provenance
            .expect("source facts must reach the activity index");
        assert_eq!(provenance.captured.as_ref().unwrap()["project"], "unknown");
        assert_eq!(
            provenance.effective_project.as_ref().unwrap()["name"],
            "Synthetic Project"
        );
        assert_eq!(
            provenance.supplement.as_ref().unwrap()["source"],
            "project-list"
        );
        assert_eq!(
            provenance.supplement.as_ref().unwrap()["observedAt"],
            "2026-09-25T12:00:00.000Z"
        );
    }

    // helpers -----------------------------------------------------------------
    fn codex_ms(ms: i64) -> String {
        format!(
            r#"{{"timestamp":{ms},"type":"user_message","payload":{{"message":{{"role":"user","content":"hi"}}}},"cwd":"/x"}}"#
        )
    }
    fn cc_epoch(secs: i64) -> String {
        format!(
            r#"{{"parentUuid":null,"type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":{secs},"cwd":"/x","version":"1.0.31"}}"#
        )
    }
}
