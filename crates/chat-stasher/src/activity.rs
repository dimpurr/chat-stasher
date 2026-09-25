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
//! keeps: everything about a line beyond its timestamp and (for claude-code)
//! that one candidate label is read and thrown away, and never printed.
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
    /// The harness's own title: a claude-code `ai-title` line, or a `summary`
    /// line when the session has no title.
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
///   lines this module does not read a title from (codex, cursor, the web
///   platforms until their list metadata is read): no guess, no empty string.
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

/// The label candidates one claude-code scan picked up, before the priority
/// order decides which of them becomes the row's title.
///
/// Only claude-code has candidates at all: it is the one harness whose title
/// lines and prompt shape this module knows (`{"type":"ai-title","aiTitle":…}`
/// and `{"type":"summary","summary":…}`, measured shapes in the W156 report).
/// Every other harness records [`SessionTitle::NoLabelRecorded`] by design.
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

    /// The label this session's scan produced, in the row's recorded shape.
    fn resolve(self) -> SessionTitle {
        let (raw, source) = if let Some(t) = self.ai_title {
            (t, TitleSource::HarnessTitle)
        } else if let Some(t) = self.summary {
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
/// line. Their times live inside `raw.text` and are read by [`web_time`].
const WEB_HARNESSES: &[&str] = &[
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

    // The label candidates (claude-code only — see [`TitleCandidates`]).
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
        // For any harness except claude-code no candidate was ever folded, so
        // this is the design's "no label recorded" — see the module docs.
        title: titles.resolve(),
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
        "kimi-code" => kimi_code_time(value),
        _ => LineTime::NoTimestampField,
    }
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
// `lib/contract.ts`) and the real-sanitized competitor fixtures under
// `nm/w5-competitors/`.
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

    let span = match harness {
        "chatgpt" => chatgpt_span(payload),
        "deepseek" => deepseek_span(payload),
        "claude" => claude_span(payload),
        "grok" => grok_span(payload),
        "perplexity" => perplexity_span(payload),
        "kimi" => kimi_span(payload),
        _ => WebSpan::Absent,
    };
    span.into_line_time()
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
    }
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
    serde_json::to_string(row).expect("ActivityRow serializes to JSON") + "\n"
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
            r#"{{"schema":"chat-stasher.sqlite.session.v1","table":"session_docs","session":{{"session_id":"s1","updated_at":{T2},"title":"t","content":"x"}}}}"#
        );
        let lines = [envelope.as_str()];
        let a = analyze_session("grok", &lines);
        assert_eq!(a.first_unix, Some(T2));
        assert_eq!(a.last_unix, Some(T2));
        assert!(matches!(a.time_source, TimeSource::Inferred { .. }));
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
