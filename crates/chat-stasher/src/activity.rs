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
}

/// Harnesses this module knows how to read times from. Everything else is
/// reported [`TimeSource::Unknown`] with an explicit "not implemented" `why`,
/// never guessed.
const SUPPORTED_HARNESSES: &[&str] = &["claude-code", "codex"];

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
            LineTime::Time { unix, rfc3339 } => {
                first = Some(first.map_or(unix, |old| old.min(unix)));
                last = Some(last.map_or(unix, |old| old.max(unix)));
                // Once any timestamp is unambiguous RFC 3339, the whole session
                // is Exact; otherwise (numeric epochs only) it is Inferred.
                if rfc3339 {
                    any_rfc3339 = true;
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
        if any_rfc3339 {
            TimeSource::Exact
        } else {
            TimeSource::Inferred {
                how: "行内时间戳为数值 epoch：按数值大小判断单位（落在 2020–2100 的秒区间按秒、落在毫秒区间除以 1000 得秒）"
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
            "该 harness（{harness}）的对话时间解析尚未实现（目前支持 {}）",
            SUPPORTED_HARNESSES.join("、")
        );
    }
    let mut reasons: Vec<String> = Vec::new();
    if unparseable_json > 0 {
        reasons.push(format!("{unparseable_json} 行无法解析为 JSON"));
    }
    if invalid_timestamp > 0 {
        reasons.push("存在时间戳字段但值无法解析或超出 2020–2100 合理范围".to_string());
    }
    if !saw_timestamp_field {
        reasons.push("行内未找到 timestamp 时间戳字段".to_string());
    }
    if reasons.is_empty() {
        reasons.push("没有任何可用时间戳".to_string());
    }
    format!("无法确定对话时间：{}", reasons.join("；"))
}

/// What one line yielded.
enum LineTime {
    /// A usable unix-seconds timestamp was recovered from this line.
    Time {
        unix: i64,
        /// True when it came from an explicit RFC 3339 string (unambiguous);
        /// false when it came from a numeric epoch whose unit was inferred.
        rfc3339: bool,
    },
    /// The harness is supported but this line carries no timestamp field.
    Absent,
    /// This harness is not supported at all — nothing to look for.
    NoTimestampField,
    /// A timestamp field exists but is unparseable / out of the plausible window.
    Invalid,
}

/// Extract the timestamp-bearing value from a line, per harness.
fn line_time(harness: &str, value: &serde_json::Value) -> LineTime {
    let ts = match harness {
        "claude-code" | "codex" => value.get("timestamp"),
        _ => return LineTime::NoTimestampField,
    };
    let Some(ts) = ts else {
        return LineTime::Absent;
    };
    match ts {
        serde_json::Value::String(s) => match parse_rfc3339(s) {
            Some(t) => LineTime::Time {
                unix: t,
                rfc3339: true,
            },
            // A numeric epoch can also arrive as a JSON string.
            None => match parse_numeric_epoch(s) {
                Some(t) => LineTime::Time {
                    unix: t,
                    rfc3339: false,
                },
                None => LineTime::Invalid,
            },
        },
        serde_json::Value::Number(n) => match numeric_value_seconds(n) {
            Some(t) => LineTime::Time {
                unix: t,
                rfc3339: false,
            },
            None => LineTime::Invalid,
        },
        _ => LineTime::Invalid,
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
// RFC 3339 parsing (no external time crate — this crate adds no dependencies)
// ---------------------------------------------------------------------------

/// Parse an RFC 3339 timestamp into unix seconds (UTC). Accepts `Z` and
/// `±HH:MM` offsets, and optional fractional seconds (precision beyond the
/// second is discarded — we report whole seconds). `None` on any malformed /
/// out-of-range input.
fn parse_rfc3339(s: &str) -> Option<i64> {
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

    let (second, offset_seconds) = match b.get(idx) {
        Some(&b'Z') => (second, 0),
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
            (second, sign * (oh * 3600 + om * 60))
        }
        _ => return None,
    };

    let days = days_from_civil(year as i64, month as u32, day as u32)?;
    Some(days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second - offset_seconds)
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
        assert!(how.contains("毫秒"), "how should say millis: {how}");
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
        assert!(why.contains("合理"), "why should mention the range: {why}");
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
            why.contains("未实现") || why.contains("harness"),
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
