//! Conversation-time extraction for the activity sidecar index.
//!
//! chat-stasher archives snapshots at **archival** time (rustic snapshot
//! time). To draw a heatmap by **when the conversation happened**, we need a
//! per-session earliest/latest *conversation* time, which only the session's
//! own lines carry. This module reads those lines (metadata-only: we extract a
//! timestamp and throw the line away — nothing else is ever kept or printed)
//! and produces one [`ActivityRow`] per session, serialised as a JSONL line
//! into `<stage>/meta/<machine>/activity-v1.jsonl`.
//!
//! The hard rule of this module: a time we cannot get is [`TimeSource::Unknown`]
//! with an explicit `why`. We never fabricate `0`, never use "now", and never
//! substitute the file's mtime. (This repo already paid for "0 as both sentinel
//! and valid value" once — see `inbox.rs` `modified_ns`.) And "this line could
//! not be parsed" is a different `why` from "this harness never records a time".
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
    /// the source actually said (or the owner measured) one. `None` means the
    /// values were absolute epoch numbers, where a zone would be meaningless.
    /// Nothing is ever read as UTC by assumption: a naive field carries its
    /// declared zone here, and an offset-bearing RFC 3339 string keeps its
    /// offset label.
    pub source_zone: Option<String>,
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
    /// Could not be obtained; why it could not.
    Unknown { why: String },
}

/// Result of analysing one session's lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeAnalysis {
    pub first_unix: Option<i64>,
    pub last_unix: Option<i64>,
    pub line_count: u64,
    pub time_source: TimeSource,
    pub source_zone: Option<String>,
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
    "chatgpt",
    "deepseek",
    "claude",
    "grok",
    "gemini",
    "perplexity",
    "kimi",
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
    let mut first: Option<i64> = None;
    let mut last: Option<i64> = None;
    let mut any_rfc3339 = false;
    let mut any_messages = false;
    let mut any_list = false;
    let mut source_zone: Option<String> = None;

    // Diagnosis counters for the Unknown branch: they keep "parse failure"
    // distinct from "this harness never records a time".
    let mut unparseable_json = 0u64;
    let mut invalid_timestamp = 0u64;
    let mut saw_timestamp_field = false;

    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        line_count += 1;

        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            unparseable_json += 1;
            continue;
        };
        match line_time(harness, &value) {
            // A single line can now span a whole session (opencode/cursor/
            // gemini export one session as one JSON object), so a line carries
            // its own first/last and the aggregation folds those in.
            LineTime::Time {
                first: f,
                last: l,
                rfc3339,
                basis,
                zone,
            } => {
                first = Some(first.map_or(f, |old| old.min(f)));
                last = Some(last.map_or(l, |old| old.max(l)));
                // Once any timestamp is unambiguous RFC 3339, the whole session
                // is Exact; otherwise (numeric epochs only) it is Inferred.
                if rfc3339 {
                    any_rfc3339 = true;
                }
                match basis {
                    Basis::Messages => any_messages = true,
                    Basis::List => any_list = true,
                    Basis::Local => {}
                }
                if source_zone.is_none() {
                    source_zone = zone;
                }
            }
            LineTime::Absent => {}
            LineTime::NoTimestampField => {
                saw_timestamp_field = false;
            }
            LineTime::Invalid => {
                saw_timestamp_field = true;
                invalid_timestamp += 1;
            }
        }
    }

    let time_source = if first.is_some() {
        if any_messages {
            TimeSource::Messages { exact: any_rfc3339 }
        } else if any_list {
            // A list-only span is low confidence by construction; it is never
            // reported as an exact/inferred message interval.
            TimeSource::ListUpdated
        } else if any_rfc3339 {
            TimeSource::Exact
        } else {
            TimeSource::Inferred {
                how: "in-line timestamps are numeric epochs: unit inferred from magnitude (values in the 2020–2100 seconds range treated as seconds; millis-range values divided by 1000 to get seconds)"
                    .to_string(),
            }
        }
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
        first_unix: first,
        last_unix: last,
        line_count,
        time_source,
        source_zone,
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
    if WEB_HARNESSES.contains(&harness) {
        return web_time(harness, value);
    }
    match harness {
        "claude-code" | "codex" => top_level_timestamp(value),
        "opencode" => opencode_time(value),
        "cursor" => cursor_time(value),
        "gemini-cli" => gemini_time(value),
        _ => LineTime::NoTimestampField,
    }
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
// keeps the offset it was written with (`source_zone` records it), and a
// platform whose numeric field is local wall clock declares its measured zone
// (DeepSeek, +08:00) — the value is shifted to true UTC and the zone is
// recorded, never silently dropped.

/// How one raw JSON value becomes unix seconds for a web harness.
#[derive(Clone, Copy)]
enum EpochMode {
    /// Absolute epoch seconds (or millis) — a zone would be meaningless.
    Absolute,
    /// RFC 3339 string; the string's own offset is recorded.
    Rfc3339,
    /// Local-wall-clock epoch seconds at a fixed measured offset; unix = value −
    /// offset. The declared zone label is attached to every stamp.
    NaiveLocal {
        offset_seconds: i64,
        label: &'static str,
    },
}

/// DeepSeek's numeric timestamps were measured at +08:00 by the owner (the
/// account's wall clock). Competitor implementations treat the same `inserted_at`
/// as an absolute epoch; this parser follows the owner's measurement and records
/// the zone so a consumer can see the choice. See the W97 report's Prior art.
const DEEPSEEK_ZONE: EpochMode = EpochMode::NaiveLocal {
    offset_seconds: 8 * 3600,
    label: "+08:00",
};

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
        EpochMode::Absolute => web_numeric_seconds(raw).map(|t| (t, false, None)),
        EpochMode::NaiveLocal {
            offset_seconds,
            label,
        } => web_numeric_seconds(raw).map(|t| (t - offset_seconds, false, Some(label.to_string()))),
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
/// `{data:{biz_data:{chat_sessions:[{updated_at}]}}}`. Local +08:00 (owner-measured).
fn deepseek_span(payload: &serde_json::Value) -> WebSpan {
    let biz = payload.pointer("/data/biz_data");
    let mut msg = Stamps::default();
    if let Some(messages) = biz
        .and_then(|b| b.get("chat_messages"))
        .and_then(serde_json::Value::as_array)
    {
        msg.add_field(messages.iter(), "inserted_at", DEEPSEEK_ZONE);
    }
    if msg.has() {
        return WebSpan::Messages(msg);
    }
    let mut list = Stamps::default();
    if let Some(session) = biz.and_then(|b| b.get("chat_session")) {
        for field in ["inserted_at", "updated_at"] {
            if let Some(raw) = session.get(field) {
                list.add(raw, DEEPSEEK_ZONE);
            }
        }
    }
    // A list body stored under one conversation: only a single-item page is
    // unambiguous enough to attribute to this session.
    if let Some(sessions) = biz
        .and_then(|b| b.get("chat_sessions"))
        .and_then(serde_json::Value::as_array)
    {
        if sessions.len() == 1 {
            list.add_field(sessions.iter(), "updated_at", DEEPSEEK_ZONE);
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
    let mut list = Stamps::default();
    for field in ["created_at", "updated_at"] {
        if let Some(raw) = payload.get(field) {
            list.add(raw, EpochMode::Rfc3339);
        }
    }
    if let Some(items) = payload.as_array() {
        if items.len() == 1 {
            for field in ["created_at", "updated_at"] {
                list.add_field(items.iter(), field, EpochMode::Rfc3339);
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
    let mut list = Stamps::default();
    if let Some(conversation) = payload.pointer("/conversation_v2/conversation") {
        for field in ["createTime", "modifyTime"] {
            if let Some(raw) = conversation.get(field) {
                list.add(raw, EpochMode::Rfc3339);
            }
        }
    }
    if let Some(items) = payload
        .get("conversations")
        .and_then(serde_json::Value::as_array)
    {
        if items.len() == 1 {
            for field in ["createTime", "modifyTime"] {
                list.add_field(items.iter(), field, EpochMode::Rfc3339);
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
    let mut list = Stamps::default();
    if let Some(chat) = payload.get("chat") {
        for field in ["createTime", "updateTime"] {
            if let Some(raw) = chat.get(field) {
                list.add(raw, EpochMode::Rfc3339);
            }
        }
    }
    if let Some(items) = payload.get("items").and_then(serde_json::Value::as_array) {
        if items.len() == 1 {
            if let Some(chat) = items[0].get("chat") {
                for field in ["createTime", "updateTime"] {
                    if let Some(raw) = chat.get(field) {
                        list.add(raw, EpochMode::Rfc3339);
                    }
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
/// extension's own `walkFrames`: a line that is all digits is the byte length of
/// the frame that follows; anything else is taken as a one-line frame.
fn parse_batchexecute_frames(body: &str) -> Vec<serde_json::Value> {
    let mut frames = Vec::new();
    let bytes = body.as_bytes();
    let mut pos = 0usize;
    while pos < body.len() {
        while pos < body.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= body.len() {
            break;
        }
        let line_end = body[pos..].find('\n').map_or(body.len(), |i| pos + i);
        let line = &body[pos..line_end];
        if !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()) {
            let Ok(len) = line.parse::<usize>() else {
                // A length that does not fit in a usize cannot be a frame we
                // could read; move past the line rather than inventing a zero.
                pos = if line_end < body.len() {
                    line_end + 1
                } else {
                    body.len()
                };
                continue;
            };
            let start = if body[line_end..].starts_with('\n') {
                line_end + 1
            } else {
                line_end
            };
            if len == 0 {
                pos = start;
                continue;
            }
            let end = (start + len).min(body.len());
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body[start..end]) {
                frames.push(value);
            }
            pos = end;
        } else {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                frames.push(value);
            }
            pos = if line_end < body.len() {
                line_end + 1
            } else {
                body.len()
            };
        }
    }
    frames
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
    }
}

/// Serialise one row as a single JSONL line (trailing newline included).
pub fn to_jsonl(row: &ActivityRow) -> String {
    // Cannot fail for this shape: plain strings, an Option<i64>, an int, and an
    // internally-tagged enum of strings. No NaN / recursion involved.
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
    fn all_blank_lines_is_unknown() {
        let lines = ["", "   ", ""];
        let a = analyze_session("claude-code", &lines);
        assert_eq!(a.first_unix, None);
        assert_eq!(a.line_count, 0);
        assert!(matches!(a.time_source, TimeSource::Unknown { .. }));
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
    }

    /// DeepSeek's numeric fields are local +08:00 (owner-measured): the stored
    /// value is 8 h ahead of true UTC, and the parser records the source zone.
    #[test]
    fn deepseek_message_times_apply_the_plus_08_zone() {
        let z = 8 * 3600;
        let body = serde_json::json!({
            "data": { "biz_data": {
                "chat_messages": [
                    { "inserted_at": T1 + z },
                    { "inserted_at": T2 + z },
                ],
                "chat_session": { "inserted_at": T1 + z, "updated_at": T2 + z },
            } },
        })
        .to_string();
        let line = web_line("deepseek", &body);
        let a = analyze_session("deepseek", &[line.as_str()]);
        assert_eq!(
            a.first_unix,
            Some(T1),
            "the +08:00 value must be shifted to UTC"
        );
        assert_eq!(a.last_unix, Some(T2));
        assert_eq!(a.source_zone.as_deref(), Some("+08:00"));
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

    /// No messages archived yet: the span is only the list/update time, and its
    /// low confidence is named.
    #[test]
    fn claude_web_list_only_is_list_updated() {
        let body = serde_json::json!({
            "uuid": "u",
            "created_at": RFC_T1,
            "updated_at": RFC_T2,
        })
        .to_string();
        let line = web_line("claude", &body);
        let a = analyze_session("claude", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T1), Some(T2)));
        assert_eq!(a.time_source, TimeSource::ListUpdated);
    }

    #[test]
    fn deepseek_list_only_is_list_updated() {
        let z = 8 * 3600;
        let body = serde_json::json!({
            "data": { "biz_data": {
                "chat_session": { "inserted_at": T1 + z, "updated_at": T2 + z },
            } },
        })
        .to_string();
        let line = web_line("deepseek", &body);
        let a = analyze_session("deepseek", &[line.as_str()]);
        assert_eq!((a.first_unix, a.last_unix), (Some(T1), Some(T2)));
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
        assert_eq!(a.first_unix, Some(T1));
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
