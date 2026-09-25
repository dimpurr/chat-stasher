//! sessions — `/sessions` (the drilled list), `/session` (one row's metadata)
//! and `/content` (the raw shards, the one route that reaches the payload tier).
//!
//! The list's three "nothing matched" sentences reuse `search`'s vocabulary,
//! and `search`'s own selector produces the set the list is drawn from.

use std::collections::BTreeSet;

use crate::activity::TitleSource;
use crate::search::SessionLabel;
use crate::selector::{Resolved, UnplacedBy};

use super::html::{completeness_banner, describe_selector, esc, fmt_bytes, fmt_unix, footer, head};
use super::{
    index_param, percent_encode, select, selector_from_query, Content, ContentSource, Query,
    Response, Selection, UiData, UiSession, EXPLICIT_REPO_LABEL, PROVENANCE_FIRST_USER_LINE,
    PROVENANCE_HARNESS_TITLE,
};

pub(super) fn list_page(params: &Query, token: &str, data: &UiData) -> Response {
    let resolved = match selector_from_query(params) {
        Ok(r) => r,
        Err(e) => return Response::text(400, "Bad Request", format!("ui: {e}\n")),
    };
    let sel = select(&data.sessions, &resolved.selector);
    Response::html(200, "OK", page_sessions(&sel, &resolved, token, data))
}

pub(super) fn one_session_page(params: &Query, token: &str, data: &UiData) -> Response {
    let Some(row) = index_param(params, data) else {
        return Response::text(
            400,
            "Bad Request",
            "ui: `i` must name a row of this dashboard's session list. An index that \
             resolves to nothing is a usage error, not an empty session.\n",
        );
    };
    Response::html(200, "OK", page_session(row, token, data))
}

pub(super) fn content_page(params: &Query, data: &UiData, content: &dyn ContentSource) -> Response {
    let Some(row) = index_param(params, data) else {
        return Response::text(
            400,
            "Bad Request",
            "ui: `i` must name a row of this dashboard's session list. An index that \
             resolves to nothing is a usage error, not an empty session.\n",
        );
    };
    match content.fetch(&row.machine, &row.session_id) {
        Ok(c) => Response::html(200, "OK", page_content(row, &c, data)),
        Err(e) => Response::text(
            502,
            "Bad Gateway",
            format!(
                "ui: could not read session {} on `{}`: {e}\n\
                 This is a failure to read, NOT an empty session.\n",
                row.short_id, row.machine
            ),
        ),
    }
}

// -------------------------------------------------------------- drilled list

fn page_sessions(sel: &Selection<'_>, resolved: &Resolved, token: &str, data: &UiData) -> String {
    let mut out = head(&format!(
        "chat-stasher · sessions · {}",
        data.destination_label
    ));
    out.push_str("<h1>Sessions</h1>\n<p class=sub><a href=\"/?token=");
    out.push_str(&percent_encode(token));
    out.push_str("\">← overview</a> · destination <b>");
    out.push_str(&esc(&data.destination_label));
    out.push_str("</b></p>\n");

    if let Some(text) = describe_selector(&resolved.selector) {
        out.push_str(&format!(
            "<div class=note><b>Filter:</b> {}<br><span class=sub>Applied by the same selector \
             <code>search</code> uses, to one unfiltered read of the archive — so this list is \
             the set <code>chat-stasher search</code> would return with the same flags.</span>\
             </div>\n",
            esc(&text)
        ));
    }
    for w in &resolved.warnings {
        out.push_str(&format!("<div class=warn>{}</div>\n", esc(w)));
    }
    out.push_str(&completeness_banner(data));
    out.push_str(&label_coverage_note(sel, data));

    if sel.matched.is_empty() {
        out.push_str(&no_hit_html(sel, data));
    } else {
        out.push_str(&format!(
            "<p><b>{}</b> session(s) matched{}.</p>\n",
            sel.matched.len(),
            if sel.not_matched > 0 {
                format!("; {} were evaluated and rejected", sel.not_matched)
            } else {
                String::new()
            }
        ));
        out.push_str(
            "<div class=scroll><table>\n<thead><tr><th>machine</th><th>source</th>\
             <th>session (short)</th><th>label</th><th class=n>shards</th>\
             <th class=n>bytes</th>\
             <th>first message</th><th>last message</th><th>snapshot time</th>\
             </tr></thead>\n<tbody>\n",
        );
        for s in &sel.matched {
            out.push_str(&list_row(s, token));
        }
        out.push_str("</tbody></table></div>\n");
    }
    if !sel.unplaced.is_empty() {
        out.push_str(&format!(
            "<section><h2>Could not be placed ({} session(s))</h2>\n\
             <p>An active filter has no answer for these. They are <b>neither matched nor \
             rejected</b>: reporting them as \"not found\" would turn \"we could not tell\" \
             into \"it is not there\".</p>\n<div class=scroll><table>\n\
             <thead><tr><th>machine</th><th>source</th><th>session</th><th>which filter</th>\
             <th>why</th></tr></thead>\n<tbody>\n",
            sel.unplaced.len()
        ));
        for (s, dim, why) in &sel.unplaced {
            out.push_str(&format!(
                "<tr><td class=mono>{m}</td><td>{h}</td>\
                 <td><a class=mono href=\"/session?i={i}&token={t}\">{sid}</a></td>\
                 <td>{dim}</td><td>{why}</td></tr>\n",
                m = esc(&s.machine),
                h = esc(&s.source_label()),
                i = s.index,
                t = percent_encode(token),
                sid = esc(&s.short_id),
                dim = match dim {
                    UnplacedBy::Time => "conversation time",
                    UnplacedBy::Harness => "harness",
                    UnplacedBy::NoContent => "no conversation content",
                },
                why = esc(why),
            ));
        }
        out.push_str("</tbody></table></div></section>\n");
    }
    out.push_str(&footer(data));
    out.push_str("</body></html>\n");
    out
}

/// The three "nothing here" sentences `search` already distinguishes. Reusing
/// its vocabulary — rather than one "no results" line — is the point.
fn no_hit_html(sel: &Selection<'_>, data: &UiData) -> String {
    if !data.complete() {
        return format!(
            "<div class=warn><b>UNKNOWN — not \"not there\".</b> The destination could not be \
             read in full ({} part(s) unreadable), so 0 matched in the part that could be read \
             proves nothing.</div>\n",
            data.unreadable.len()
        );
    }
    let unplaced_blocking = sel
        .unplaced
        .iter()
        .filter(|(_, dimension, _)| *dimension != UnplacedBy::NoContent)
        .count();
    if unplaced_blocking > 0 {
        return format!(
            "<div class=warn><b>UNKNOWN — not \"not there\".</b> 0 of {} session(s) matched, \
             but {} could not be placed (below), so this is not a proven absence.</div>\n",
            data.sessions.len(),
            unplaced_blocking
        );
    }
    format!(
        "<p><b>Not in this destination</b> — 0 of the {} session(s) in view matched, and the \
         destination was read in full. This is a real absence, not a failure to look.</p>\n",
        data.sessions.len()
    )
}

/// How one session's label renders in the list (29-UI-DESIGN §2.2's three
/// states): the text with its provenance in the cell's title attribute and a
/// visible stop when the cap cut it, or one of the two honest non-labels —
/// `no label recorded` (the index read this session and found nothing
/// label-able, or the harness's lines are labelless by design) and
/// `label unknown` (the row predates labels, or there is no row at all: a
/// read that predates the label keys, never a session that was examined).
fn label_cell_html(s: &UiSession) -> String {
    match &s.title {
        SessionLabel::Known {
            text,
            source,
            truncated,
        } => {
            let provenance = match source {
                TitleSource::HarnessTitle => PROVENANCE_HARNESS_TITLE,
                TitleSource::FirstUserLine => PROVENANCE_FIRST_USER_LINE,
            };
            let dots = if *truncated { "\u{2026}" } else { "" };
            format!("<td title=\"{provenance}\">{}{}</td>", esc(text), dots)
        }
        SessionLabel::NoLabelRecorded => "<td>no label recorded</td>".to_string(),
        SessionLabel::LegacyIndex => format!(
            "<td title=\"{}\">label unknown</td>",
            esc(
                "this machine's activity index predates labels — see the note above; \
                re-running chat-stasher activity-index on that machine fills it in"
            )
        ),
        SessionLabel::Unknown { why } => {
            format!("<td title=\"{}\">label unknown</td>", esc(why))
        }
    }
}

/// The machine-level label-coverage note (29-UI-DESIGN §2.2 state 2): one
/// note per machine whose pre-label rows are on this page, naming the real
/// destination-side repair command. The rows themselves only say
/// `label unknown`; this note is the one place the reason and the fix are
/// spelled out, and it follows the machine filter so a fresh machine never
/// reads as partial.
fn label_coverage_note(sel: &Selection<'_>, data: &UiData) -> String {
    let on_page: BTreeSet<&str> = sel.matched.iter().map(|s| s.machine.as_str()).collect();
    let legacy: Vec<&str> = data
        .machines_with_legacy_index
        .iter()
        .map(String::as_str)
        .filter(|m| on_page.contains(m))
        .collect();
    if legacy.is_empty() {
        return String::new();
    }
    // A dashboard opened with `--repo` cannot name a destination; the repair
    // command then uses the same `--repo` flag the user opened this dashboard
    // with, and the repository path itself stays off the page (see the module
    // privacy line).
    let dest_flag = if data.destination_label == EXPLICIT_REPO_LABEL {
        "--repo <repository>"
    } else {
        &format!("--destination {}", &data.destination_label)
    };
    legacy
        .iter()
        .map(|m| {
            format!(
                "<div class=note><b>Label coverage is partial.</b> Machine \
                 <span class=mono>{m}</span>'s activity index predates labels, so its rows \
                 show <i>label unknown</i> — that is the index's age, not a session with no \
                 label. Backfill with <span class=mono>chat-stasher activity-index \
                 --rebuild {dest_flag} --machine {m} --stage <an existing empty work \
                 directory></span>: the rebuild restores that machine's archived shards \
                 itself and appends a fresh snapshot.</div>\n",
                m = esc(m),
                dest_flag = esc(dest_flag),
            )
        })
        .collect()
}

fn list_row(s: &UiSession, token: &str) -> String {
    let time = |v: Option<i64>| match v {
        // A bound that is only *part* of the span says so where it is shown: a
        // bare date here would read as the session's whole extent.
        Some(unix) if s.time_source.bounds_are_partial() => {
            format!("{} (partial)", esc(&fmt_unix(unix)))
        }
        Some(unix) => esc(&fmt_unix(unix)),
        None if s.time_source.is_no_conversation_content() => "no conversation content".to_string(),
        None => "<span class=bad title=\"unknown\">unknown</span>".to_string(),
    };
    let label = label_cell_html(s);
    format!(
        "<tr><td class=mono>{m}</td><td>{h}</td>\
         <td><a class=mono href=\"/session?i={i}&token={t}\">{sid}</a></td>{label}\
         <td class=n>{sh}</td><td class=n>{b}</td><td>{f}</td><td>{l}</td><td>{snap}</td></tr>\n",
        m = esc(&s.machine),
        h = esc(&s.source_label()),
        i = s.index,
        t = percent_encode(token),
        sid = esc(&s.short_id),
        sh = s.shard_count,
        b = esc(&fmt_bytes(s.bytes)),
        f = time(s.first_unix),
        l = time(s.last_unix),
        snap = esc(&fmt_unix(s.archive_time_unix)),
    )
}

// -------------------------------------------------------------- session page

fn page_session(s: &UiSession, token: &str, data: &UiData) -> String {
    let time = |v: Option<i64>| match v {
        // Same rule as the list row: a partial bound is labelled where it is
        // shown, so it is never read as the session's whole extent.
        Some(unix) if s.time_source.bounds_are_partial() => {
            format!("{} (partial)", esc(&fmt_unix(unix)))
        }
        Some(unix) => esc(&fmt_unix(unix)),
        None if s.time_source.is_no_conversation_content() => "no conversation content".to_string(),
        None => "<b class=bad>unknown</b>".to_string(),
    };
    let payload_bytes = s.bytes;
    let mut out = head(&format!("chat-stasher · session {}", s.short_id));
    out.push_str(&format!(
        "<h1>Session <span class=mono>{sid}</span></h1>\n\
         <p class=sub><a href=\"/?token={t}\">← overview</a> · destination <b>{d}</b></p>\n",
        sid = esc(&s.short_id),
        t = percent_encode(token),
        d = esc(&data.destination_label),
    ));
    // The label rows (29-UI-DESIGN §4.3): the label itself, and its source —
    // the provenance row only exists for a known label, because there is
    // nothing to attribute for the honest non-labels.
    let legacy_hint = "this machine's activity index predates labels — re-running \
                       chat-stasher activity-index on that machine fills it in";
    let (label_row, label_source_row) = match &s.title {
        SessionLabel::Known {
            text,
            source,
            truncated,
        } => {
            let dots = if *truncated { "\u{2026}" } else { "" };
            let word = match source {
                TitleSource::HarnessTitle => "the harness's own title",
                TitleSource::FirstUserLine => "the first user line",
            };
            (
                format!(
                    "<tr><th>label</th><td title=\"label source: {word}\">{}{}</td></tr>\n",
                    esc(text),
                    dots
                ),
                format!("<tr><th>label source</th><td>{word}</td></tr>\n"),
            )
        }
        SessionLabel::NoLabelRecorded => (
            "<tr><th>label</th><td>no label recorded</td></tr>\n".to_string(),
            String::new(),
        ),
        SessionLabel::LegacyIndex => (
            format!(
                "<tr><th>label</th><td title=\"{}\">label unknown</td></tr>\n",
                esc(legacy_hint)
            ),
            String::new(),
        ),
        SessionLabel::Unknown { why } => (
            format!(
                "<tr><th>label</th><td title=\"{}\">label unknown</td></tr>\n",
                esc(why)
            ),
            String::new(),
        ),
    };
    let project_row = provenance_row_html(s);
    out.push_str(&format!(
        "<div class=scroll><table>\n<tbody>\n\
         <tr><th>machine</th><td class=mono>{m}</td></tr>\n\
         <tr><th>source</th><td>{h}</td></tr>\n\
         {label_row}\
         {label_source_row}\
         {project_row}\
         <tr><th>first message</th><td>{f}</td></tr>\n\
         <tr><th>last message</th><td>{l}</td></tr>\n\
         <tr><th>shards</th><td class=n>{sh}</td></tr>\n\
         <tr><th>bytes in archive</th><td class=n>{b}</td></tr>\n\
         <tr><th>data blobs</th><td class=n>{db}</td></tr>\n\
         <tr><th>archive snapshot time</th><td>{snap}</td></tr>\n\
         </tbody></table></div>\n",
        m = esc(&s.machine),
        h = esc(&s.source_label()),
        label_row = label_row,
        label_source_row = label_source_row,
        project_row = project_row,
        f = time(s.first_unix),
        l = time(s.last_unix),
        sh = s.shard_count,
        b = esc(&fmt_bytes(s.bytes)),
        db = s.data_blobs,
        snap = esc(&fmt_unix(s.archive_time_unix)),
    ));
    if let Some(why) = &s.time_why {
        out.push_str(&format!(
            "<div class=note><b>Conversation time is unknown.</b> {}</div>\n",
            esc(why)
        ));
    }
    out.push_str(&format!(
        "<div class=warn><b>Body not loaded.</b> Everything above came from archive metadata \
         (snapshot + index + tree); no conversation byte has been fetched or decrypted.<br><br>\
         Loading this session costs <b>{b} of shard data</b> across <b>{sh} shard(s)</b> made of \
         <b>{db} data blob(s)</b> — fetched from the destination and decrypted locally. \
         Nothing is fetched until you ask.<br><br>\
         <a href=\"/reader?i={i}&token={t}\"><b>Open reader</b></a> · \
         <a href=\"/content?i={i}&token={t}\"><b>show raw shards</b></a> \
         <span class=sub>({b})</span></div>\n",
        b = esc(&fmt_bytes(payload_bytes)),
        sh = s.shard_count,
        db = s.data_blobs,
        i = s.index,
        t = percent_encode(token),
    ));
    out.push_str(&footer(data));
    out.push_str("</body></html>\n");
    out
}

fn provenance_row_html(s: &UiSession) -> String {
    let Some(provenance) = &s.provenance else {
        return String::new();
    };
    let captured_project = provenance.captured.as_ref().and_then(|v| v.get("project"));
    let captured_was_unknown = captured_project.is_some_and(|v| v == "unknown");
    let name = |project: &serde_json::Value| -> String {
        match project {
            serde_json::Value::Null => "no project".to_string(),
            serde_json::Value::String(s) if s == "unknown" => "unknown".to_string(),
            serde_json::Value::Object(_) => project
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(esc)
                .unwrap_or_else(|| "project name unreadable".to_string()),
            _ => "project attribution unreadable".to_string(),
        }
    };
    let effective = provenance
        .effective_project
        .as_ref()
        .map(&name)
        .unwrap_or_else(|| "unknown".to_string());
    if let Some(supplement) = &provenance.supplement {
        let source = supplement
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("source unknown");
        let observed_at = supplement
            .get("observedAt")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("time unknown");
        // 🔴 A capture that recorded no provenance at all is **not** a capture
        //    that recorded `unknown`. "We never wrote it down" and "it was
        //    written down as unknown" are two different states (CLAUDE.md
        //    invariant 1), and this sentence is the one ADR-043 mandates, so
        //    the distinction has to survive in it. The no-supplement branch
        //    below says "not recorded" for the same reason.
        let captured = if captured_was_unknown {
            "unknown".to_string()
        } else {
            captured_project
                .map(&name)
                .unwrap_or_else(|| "not recorded".to_string())
        };
        format!(
            "<tr><th>project</th><td>{effective} (learned later from {source} at {observed_at}); capture recorded project: {captured}</td></tr>\n",
            effective = effective,
            source = esc(source),
            observed_at = esc(observed_at),
            captured = captured,
        )
    } else {
        let original = captured_project
            .map(&name)
            .unwrap_or_else(|| "not recorded".to_string());
        format!("<tr><th>project</th><td>{}</td></tr>\n", esc(&original))
    }
}

/// The payload tier. Reached only by the explicit link on the session page.
fn page_content(s: &UiSession, c: &Content, data: &UiData) -> String {
    let mut shards = String::new();
    for (name, hash) in &c.shards {
        shards.push_str(&format!(
            "<li class=mono>{} sha256={}</li>\n",
            esc(name),
            esc(&hash.chars().take(12).collect::<String>())
        ));
    }
    format!(
        "{head}\n<h1>Session <span class=mono>{sid}</span> · content</h1>\n\
         <p class=sub>machine <span class=mono>{m}</span> · source {h}</p>\n\
         <p>Loaded <b>{b}</b> of shard data across {n} shard(s). Concatenation sha256 \
         <span class=mono>{sha}</span>.</p>\n\
         <ul>{shards}</ul>\n\
         <p class=sub>The same digest <code>chat-stasher read</code> prints for the \
         concatenation of these shards.</p>\n\
         <h2>Concatenated shards</h2>\n<pre>{body}</pre>\n\
         {footer}\n</body></html>\n",
        head = head(&format!("chat-stasher · content · {}", s.short_id)),
        sid = esc(&s.short_id),
        m = esc(&s.machine),
        h = esc(&s.source_label()),
        b = esc(&fmt_bytes(c.bytes as u64)),
        n = c.shards.len(),
        sha = esc(&c.concat_sha256),
        shards = shards,
        body = esc(&c.body),
        footer = footer(data),
    )
}
