//! json — the `/api/overview` and `/api/sessions` wire shapes.
//!
//! A filter that does not resolve is reported as an error object under the same
//! `400` the HTML route gives, never as an empty session list, so a consumer
//! cannot read "we could not search" as "nothing matched".

use std::collections::BTreeSet;

use crate::activity::{TimeSource, TitleSource};
use crate::search::SessionLabel;
use crate::selector::{Resolved, UnplacedBy, UsageError};

use super::html::describe_selector;
use super::overview::count_by_machine;
use super::{health_of, percent_encode, select, Selection, UiData, UiSession};

// ---------------------------------------------------------------- json routes

pub(super) fn json_overview(data: &UiData) -> String {
    let sel = select(&data.sessions, &data.launch);
    let in_view = sel.in_view();
    let machines: Vec<serde_json::Value> = data
        .machine_keys()
        .iter()
        .map(|m| {
            let host = data.host(m);
            let (n, bytes) = count_by_machine(&in_view, m);
            serde_json::json!({
                "machine": m,
                "sessions": n,
                "bytes": bytes,
                "newest_snapshot_unix": host.map(|h| h.archive_time_unix),
                "newest_snapshot_short_id": host.map(|h| h.snapshot_id.chars().take(8).collect::<String>()),
                "has_activity_index": host.map(|h| h.has_activity_index),
                "index_read_ok": host.map(|h| h.index_read_ok),
                "health": health_of(host, data.now_unix).word(),
            })
        })
        .collect();
    let total_bytes: u64 = in_view.iter().map(|s| s.bytes).sum();
    let sources: BTreeSet<String> = in_view.iter().map(|s| s.source_label()).collect();
    let v = serde_json::json!({
        "schema_version": 1,
        "command": "ui",
        "destination": data.destination_label,
        "tier": "metadata",
        "payload_loaded": false,
        "data_blobs_read": data.data_blobs_read,
        "index_files_read": data.index_files_read,
        "complete": data.complete(),
        "unreadable_parts": data.unreadable,
        "snapshots_scanned": data.snapshots_scanned,
        "snapshots_in_repo": data.snapshots_in_repo,
        "sessions_seen": data.sessions_seen,
        "filter": describe_selector(&data.launch),
        "summary": {
            "sessions_in_view": in_view.len(),
            "bytes_in_view": total_bytes,
            "machines": data.machine_keys().len(),
            "sources": sources.len(),
            "matched": sel.matched.len(),
            "not_matched": sel.not_matched,
            "could_not_be_placed": sel.unplaced.len(),
            "time_unknown": in_view.iter().filter(|s| !s.has_known_time() && !s.time_source.is_no_conversation_content()).count(),
        },
        "machines": machines,
        "machines_without_activity_index": data.machines_without_index,
        "generated_unix": data.now_unix,
    });
    json_string(&v)
}

/// The wire shape of one row's label (29-UI-DESIGN §5.3): an object, never a
/// bare string, so nothing can confuse an empty label with a recorded
/// absence. `state` is one of the design's three words — plus `unknown` for
/// the corner §2.2 names at machine level: a session the index holds no row
/// for at all, whose `why` says which case it is.
fn title_json(s: &UiSession) -> serde_json::Value {
    match &s.title {
        SessionLabel::Known {
            text,
            source,
            truncated,
        } => {
            let source = match source {
                TitleSource::HarnessTitle => "harness_title",
                TitleSource::FirstUserLine => "first_user_line",
            };
            serde_json::json!({
                "state": "known",
                "text": text,
                "source": source,
                "truncated": truncated,
            })
        }
        SessionLabel::NoLabelRecorded => serde_json::json!({"state": "no_label"}),
        SessionLabel::LegacyIndex => serde_json::json!({"state": "legacy_index"}),
        SessionLabel::Unknown { why } => serde_json::json!({
            "state": "unknown",
            "why": why,
        }),
    }
}

pub(super) fn json_sessions(
    sel: &Selection<'_>,
    resolved: &Resolved,
    token: &str,
    data: &UiData,
) -> String {
    let row = |s: &UiSession| {
        let mut row = serde_json::json!({
            "index": s.index,
            "machine": s.machine,
            "source": s.source_label(),
            "harness": s.harness,
            "session_short_id": s.short_id,
            "shards": s.shard_count,
            "bytes": s.bytes,
            "first_unix": time_state(s.first_unix, s.time_why.as_deref(), &s.time_source),
            "last_unix": time_state(s.last_unix, s.time_why.as_deref(), &s.time_source),
            "line_count": s.line_count,
            "archive_time_unix": s.archive_time_unix,
            "title": title_json(s),
            "href": format!("/session?i={}&token={}", s.index, percent_encode(token)),
        });
        if let Some(provenance) = &s.provenance {
            row["provenance"] = serde_json::json!(provenance);
        }
        row
    };
    let v = serde_json::json!({
        "schema_version": 2,
        "command": "ui",
        "destination": data.destination_label,
        "tier": "metadata",
        "payload_loaded": false,
        "data_blobs_read": data.data_blobs_read,
        "complete": data.complete(),
        "unreadable_parts": data.unreadable,
        "filter": describe_selector(&resolved.selector),
        "matched": sel.matched.len(),
        "not_matched": sel.not_matched,
        "could_not_be_placed": sel.unplaced.len(),
        "machines_with_legacy_index": data.machines_with_legacy_index,
        "sessions": sel.matched.iter().map(|s| row(s)).collect::<Vec<_>>(),
        "sessions_not_placed": sel.unplaced.iter().map(|(s, dim, why)| serde_json::json!({
            "index": s.index,
            "machine": s.machine,
            "source": s.source_label(),
            "session_short_id": s.short_id,
            "dimension": match dim { UnplacedBy::Time => "time", UnplacedBy::Harness => "harness", UnplacedBy::NoContent => "no_content" },
            "why": why,
        })).collect::<Vec<_>>(),
    });
    json_string(&v)
}

/// A filter that does not resolve is reported as a JSON object with the same
/// `400` status the HTML route would give — never as an empty session list,
/// which a consumer would read as "nothing matched".
pub(super) fn json_filter_error(data: &UiData, e: &UsageError) -> String {
    json_string(&serde_json::json!({
        "schema_version": 1,
        "command": "ui",
        "destination": data.destination_label,
        "tier": "metadata",
        "payload_loaded": false,
        "status": 400,
        "error": e.to_string(),
        "complete": false,
        "matched": null,
        "note": "the filter could not be resolved, so nothing was searched — this is not an empty result",
    }))
}

fn time_state(
    unix: Option<i64>,
    why: Option<&str>,
    source: &TimeSource,
) -> crate::json_out::TimeState {
    match unix {
        Some(unix) => crate::json_out::TimeState::known(unix),
        None if source.is_no_conversation_content() => {
            crate::json_out::TimeState::no_conversation_content()
        }
        None => crate::json_out::TimeState::unknown(
            why.unwrap_or("no conversation time was recorded for this session")
                .to_string(),
        ),
    }
}

fn json_string(v: &serde_json::Value) -> String {
    let mut s = serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".into());
    s.push('\n');
    s
}
