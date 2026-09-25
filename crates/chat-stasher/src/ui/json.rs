//! json — the `/api/overview` and `/api/sessions` wire shapes.
//!
//! A filter that does not resolve is reported as an error object under the same
//! `400` the HTML route gives, never as an empty session list, so a consumer
//! cannot read "we could not search" as "nothing matched". The same holds for
//! an unresolvable paging parameter: both are one class of answer — a query
//! the server refused to guess at.
//!
//! `/api/sessions` pages the list under the §5.2 contract: `paging` reports
//! the window (`total` is the matched count, not the row count of this
//! response) and `sessions` carries only the window's rows, in the order
//! `paging.sort` names. Walking the pages at `offset` 0, `limit`, `2·limit`, …
//! and concatenating reproduces the full sorted list — that walking is the
//! point of `paging`, which is why the counts (`matched`, `not_matched`,
//! `could_not_be_placed`) always describe the whole selection, never the page.

use std::collections::BTreeSet;

use crate::activity::{TimeSource, TitleSource};
use crate::search::SessionLabel;
use crate::selector::{Resolved, UnplacedBy, UsageError};

use super::html::describe_selector;
use super::overview::count_by_machine;
use super::{
    health_of, page_window, percent_encode, select, sort_rows, IndexState, Page, Query, Response,
    Selection, TextIndex, UiData, UiSession,
};

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
    page: Page,
    token: &str,
    data: &UiData,
) -> String {
    let row = |s: &UiSession| {
        let mut row = serde_json::json!({
            "index": s.index,
            "machine": s.machine,
            "source": s.source_label(),
            "harness": s.harness,
            // The group the row's harness falls into (UIA-3). Mirrors
            // `harness` exactly: `null` when the archived id carries no
            // prefix — not a group, and not guessable into one — and a wire
            // word otherwise, `ungrouped` included, so a consumer sees an
            // unclassified source as what it is rather than as the closest
            // classified guess.
            "platform_group": s.harness.as_deref().map(super::facets::group_of).map(|g| g.wire()),
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
    let ordered = sort_rows(&sel.matched, page.sort);
    let window = page_window(&ordered, page);
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
        // The whole selection, page or no page (§5.2: the three-state counts
        // travel with every window, so a page is never mistaken for a
        // measurement of the archive).
        "matched": sel.matched.len(),
        "not_matched": sel.not_matched,
        "could_not_be_placed": sel.unplaced.len(),
        "machines_with_legacy_index": data.machines_with_legacy_index,
        // The window this page is: `total` is the matched count the pages
        // divide, `sort` the order to read `sessions` in. A consumer that
        // walks the offsets and concatenates gets exactly the sorted list.
        "paging": {
            "total": sel.matched.len(),
            "limit": page.limit,
            "offset": page.offset,
            "sort": page.sort.wire(),
        },
        "sessions": window.iter().map(|s| row(s)).collect::<Vec<_>>(),
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

/// `/api/search` — the same answer `/search` renders, as an object.
///
/// It carries the same three facts the page prints (the index's coverage, its
/// file's write time, the query mode) and the same distinction between the
/// states a zero can be in, as one of [`super::search::NoHit`]'s words. A
/// consumer therefore never has to parse a sentence to tell "the index cannot
/// look" from "it looked and the text is not there" — the words are the same
/// ones the page's own sentences are chosen from, decided once by
/// `search::no_hit`.
pub(super) fn api_search(
    params: &Query,
    token: &str,
    data: &UiData,
    index: &dyn TextIndex,
) -> Response {
    let request = match super::search::Request::parse(params) {
        Ok(request) => request,
        Err(message) => {
            return Response::json(
                400,
                "Bad Request",
                json_string(&serde_json::json!({
                    "schema_version": 1,
                    "command": "ui",
                    "destination": data.destination_label,
                    "tier": "index",
                    "payload_loaded": false,
                    "status": 400,
                    "error": message,
                    "query_state": "refused",
                    "matched": serde_json::Value::Null,
                    "no_hit": serde_json::Value::Null,
                    "note": "the query was refused, so nothing was searched — this is not an empty result",
                })),
            )
        }
    };
    let answer = super::search::solve(&request, data, index);
    Response::json(
        200,
        "OK",
        json_string(&search_json(&request, &answer, token, data)),
    )
}

fn search_json(
    request: &super::search::Request,
    answer: &super::search::Answer,
    token: &str,
    data: &UiData,
) -> serde_json::Value {
    use super::search::QueryOutcome;

    let (index_state, index_reason, written_unix, documents) = match &answer.index {
        IndexState::Missing => ("missing", serde_json::Value::Null, None, None),
        IndexState::Unreadable(reason) => (
            "unreadable",
            serde_json::Value::String(reason.clone()),
            None,
            None,
        ),
        IndexState::Ready(summary) => (
            "ready",
            serde_json::Value::Null,
            summary.written_unix,
            Some(summary.documents),
        ),
    };
    let coverage = answer.coverage.as_ref().map(|coverage| {
        serde_json::json!({
            "indexed": coverage.indexed,
            "in_view": coverage.total,
            "not_searchable": coverage.not_searchable(),
            "complete": coverage.complete(),
            "machines_behind": coverage.behind.iter().map(|(machine, indexed, in_view)| {
                serde_json::json!({"machine": machine, "indexed": indexed, "in_view": in_view})
            }).collect::<Vec<_>>(),
        })
    });
    let (query_state, hits, too_short) = match &answer.outcome {
        QueryOutcome::NoQuery => ("no_query", None, None),
        QueryOutcome::NoIndex => ("no_index", None, None),
        QueryOutcome::TooShort(too_short) => ("too_short", None, Some(*too_short)),
        QueryOutcome::Failed(_) => ("failed", None, None),
        QueryOutcome::Hits(hits) => ("answered", Some(hits), None),
    };
    let hits: Option<&super::search::Hits> = hits;
    // A query that never ran has no window: `results` is then an empty array
    // because there is nothing to report, and `query_state` above is what says
    // why — not because a missing answer was defaulted into an empty one.
    let window = match &hits {
        Some(hits) => hits.window(request.page),
        None => &[],
    };
    let results: Vec<serde_json::Value> = window
        .iter()
        .map(|hit| {
            let row = data.session_at(hit.row_index);
            serde_json::json!({
                "index": hit.row_index,
                "machine": row.map(|r| r.machine.clone()),
                "source": row.map(|r| r.source_label()),
                "session_short_id": row.map(|r| r.short_id.clone()),
                "title": row.map(title_json),
                // The excerpt as the tokenizer marked it: `matched` runs and
                // plain runs, so a consumer renders a highlight without having
                // to know the marker characters.
                "snippet": hit.matched.snippet.as_deref().map(|snippet| {
                    crate::fts::marked_segments(snippet)
                        .into_iter()
                        .map(|segment| serde_json::json!({
                            "matched": segment.matched,
                            "text": segment.text,
                        }))
                        .collect::<Vec<_>>()
                }),
                "matched_in": match &hit.place {
                    crate::fts::MatchPlace::Message { .. } => "message",
                    crate::fts::MatchPlace::Label => "label",
                    crate::fts::MatchPlace::NotRelocated => "not_relocated",
                },
                "message_ordinal": match &hit.place {
                    crate::fts::MatchPlace::Message { ordinal } => Some(ordinal + 1),
                    _ => None,
                },
                "rank": hit.matched.rank,
                "href": reader_href(hit, token, row.map(|r| r.index)),
            })
        })
        .collect();
    let body = match &answer.outcome {
        QueryOutcome::Failed(reason) => serde_json::json!(reason),
        _ => serde_json::Value::Null,
    };
    serde_json::json!({
        "schema_version": 1,
        "command": "ui",
        "destination": data.destination_label,
        "tier": "index",
        "payload_loaded": false,
        "query": request.query,
        // The query mode is always present, as it is on the page: `scan` does
        // not exist yet, so every answer this server can give is `fts`, and
        // saying so is what keeps a future mode from being assumed.
        "mode": "fts",
        "tokenizer": "trigram",
        "query_state": query_state,
        "query_error": body,
        // Present only when the query was refused for its length: the two
        // numbers a caller needs to say which length would have been answered.
        "too_short": too_short.map(|too_short| serde_json::json!({
            "chars": too_short.chars,
            "minimum": too_short.minimum,
        })),
        "index": {
            "state": index_state,
            "reason": index_reason,
            "documents": documents,
            "written_unix": written_unix,
            "written_note": "the index file's mtime; the index records no build time of its own",
        },
        "coverage": coverage,
        // The destination read, which is not the same thing as the index's
        // coverage: a complete read can still be covered by a stale index.
        "complete": data.complete(),
        "unreadable_parts": data.unreadable,
        "matched": hits.map(|h| h.total()),
        "unfiltered_matched": hits.map(|h| h.unfiltered),
        "not_matched": hits.map(|h| h.not_matched),
        "could_not_be_placed": hits.map(|h| h.unplaced.len()),
        "not_in_view": hits.map(|h| h.not_in_view),
        "truncated": hits.map(|h| h.truncated),
        // The zero, named. `null` when the query matched something or never ran.
        "no_hit": super::search::no_hit(answer, data).map(|reason| reason.wire()),
        "paging": match hits {
            Some(hits) => serde_json::json!({
                "total": hits.total(),
                "limit": request.page.limit,
                "offset": request.page.offset,
                // `/search` has no `sort`: the order is the index's relevance
                // rank. Reported as its own field rather than as a `sort` value
                // a consumer could mistake for one of the list's keys.
                "order": "relevance",
                "ranked_by": "bm25",
            }),
            None => serde_json::Value::Null,
        },
        "results": results,
        "sessions_not_placed": hits.map(|h| h.unplaced.iter().map(|(id, why)| {
            serde_json::json!({
                "machine": super::machine_of_document_id(id),
                "why": why,
            })
        }).collect::<Vec<_>>()),
    })
}

/// The reader URL a hit points at, anchored when the index could place the
/// match. The same URL the page's link carries, built by the same rule.
fn reader_href(hit: &super::search::Hit, token: &str, row_index: Option<usize>) -> String {
    let Some(index) = row_index else {
        return String::new();
    };
    match &hit.place {
        crate::fts::MatchPlace::Message { ordinal } => {
            let width = crate::normalize::DEFAULT_WINDOW;
            format!(
                "/reader?i={index}&m={}&n={width}&token={}#m{ordinal}",
                ordinal - (ordinal % width),
                percent_encode(token)
            )
        }
        _ => format!("/reader?i={index}&token={}", percent_encode(token)),
    }
}

/// A query that does not resolve — a filter value or a paging parameter — is
/// reported as a JSON object with the same `400` status the HTML route gives,
/// never as an empty session list, which a consumer would read as "nothing
/// matched".
pub(super) fn json_usage_error(data: &UiData, e: &UsageError) -> String {
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
        "note": "the query could not be resolved, so nothing was searched — this is not an empty result",
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
