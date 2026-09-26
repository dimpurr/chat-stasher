//! sessions — `/sessions` (the drilled list), `/session` (one row's metadata)
//! and `/content` (the raw shards, the one route that reaches the payload tier).
//!
//! The list's three "nothing matched" sentences reuse `search`'s vocabulary,
//! and `search`'s own selector produces the set the list is drawn from. The
//! list beyond that set is one window of it (29-UI-DESIGN §5.2): sorted by an
//! order the header names, `limit` rows from `offset`, and an offset past the
//! end is its own sentence — never a "nothing matched" one.

use std::collections::BTreeSet;

use crate::activity::{TimeSource, TitleSource};
use crate::search::SessionLabel;
use crate::selector::{Resolved, UnplacedBy};

use super::facets;
use super::html::{
    completeness_banner, conjoined_flags, describe_selector, destinations_block, esc, fmt_bytes,
    fmt_unix, footer, head, merged_counts, read_from_note,
};
use super::{
    bad_index_response, index_param, page_from_query, page_href, page_window, paging_nav,
    percent_encode, select, selector_from_query, sort_rows, Content, ContentSource, ListSort, Page,
    Query, Response, Selection, UiData, UiSession, DESTINATION_JOIN, EXPLICIT_REPO_LABEL,
    LIST_CARRY, PROVENANCE_FIRST_USER_LINE, PROVENANCE_HARNESS_TITLE,
};

pub(super) fn list_page(params: &Query, token: &str, data: &UiData) -> Response {
    let resolved = match selector_from_query(params) {
        Ok(r) => r,
        Err(e) => return Response::text(400, "Bad Request", format!("ui: {e}\n")),
    };
    // Same rule as the filter above: a paging parameter that cannot resolve is
    // a usage error, never a list of zero rows a browser would render for one.
    let page = match page_from_query(params) {
        Ok(p) => p,
        Err(e) => return Response::text(400, "Bad Request", format!("ui: {e}\n")),
    };
    let sel = select(&data.sessions, &resolved.selector);
    Response::html(
        200,
        "OK",
        page_sessions(&sel, &resolved, &page, params, token, data),
    )
}

pub(super) fn one_session_page(params: &Query, token: &str, data: &UiData) -> Response {
    let Some(row) = index_param(params, data) else {
        return bad_index_response();
    };
    Response::html(200, "OK", page_session(row, token, data))
}

pub(super) fn content_page(params: &Query, data: &UiData, content: &dyn ContentSource) -> Response {
    let Some(row) = index_param(params, data) else {
        return bad_index_response();
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

fn page_sessions(
    sel: &Selection<'_>,
    resolved: &Resolved,
    page: &Page,
    params: &Query,
    token: &str,
    data: &UiData,
) -> String {
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
    out.push_str(&destinations_block(data));
    out.push_str(&label_coverage_note(sel, data));
    // The facet bar (29-UI-DESIGN §2.3) renders for every state the page can
    // be in — a zero-match page most of all, because a facet typed wrong is
    // exactly what the bar is for switching, and a page with no way to
    // correct the query would trap the typo (§4.1's OQ-2 rationale).
    out.push_str(&facets::facet_bar(resolved, params, token, data));

    // §4.8: the distinct/raw pair describes the **view**, so it goes under
    // every answer this page can give — the zero-match one included. It used
    // to hang off the matched branch alone, which made "nothing matched" the
    // one page a reader could reach a merged view from without ever being
    // told that two of its rows are copies of one conversation.
    let counts = merged_counts(data);
    if sel.matched.is_empty() {
        // The three "nothing matched" sentences are triggered by the matched
        // **total** alone (§5.2 invariant 1). Which window was asked for plays
        // no part in them, so paging can neither create nor hide one.
        out.push_str(&no_hit_html(sel, data));
        out.push_str(&counts);
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
        out.push_str(&counts);
        // The order is fixed once, here, and the window cut from it, so the
        // rows on a page and the range sentence below describe the same
        // sequence a concatenated walk of all pages reproduces.
        let ordered = sort_rows(&sel.matched, page.sort);
        let total = ordered.len();
        let window = page_window(&ordered, *page);
        out.push_str(&range_sentence(page, &ordered, window.len()));
        out.push_str(&list_nav(params, token, *page, total));
        if window.is_empty() {
            // A window that starts past the last matching row is the fourth
            // sentence (§4.2): a position, not a count. It must never read as
            // "not in this destination" — the filter matched, the reader
            // simply paged past its rows.
            out.push_str(&format!(
                "<div class=note><b>No rows on this page.</b> The filter matched \
                 <b>{total}</b> session(s), but this window starts past the last of them. \
                 That is a paging position, not a match count: it is not the same as matching \
                 nothing. <a href=\"{back}\">Back to page 1</a></div>\n",
                back = page_href("/sessions", &LIST_CARRY, params, token, 0),
            ));
        } else {
            out.push_str(
                "<div class=scroll><table>\n<thead><tr><th>machine</th><th>source</th>\
                 <th>session (short)</th>",
            );
            // The destination column exists only where it carries information:
            // with one destination every cell would repeat the page header, and
            // a column that never varies is a column readers learn to skip —
            // including the one place it would have mattered (§4.8/R10).
            if data.destinations.len() > 1 {
                out.push_str("<th>destination</th>");
            }
            out.push_str(
                "<th>label</th>\
                 <th class=n title=\"count of non-blank lines in the archived session record, \
                 measured by the activity index\">msgs</th><th class=n>shards</th>\
                 <th class=n>bytes</th>\
                 <th>first message</th><th>last message</th><th>snapshot time</th>\
                 </tr></thead>\n<tbody>\n",
            );
            for s in window {
                out.push_str(&list_row(s, token, data));
            }
            out.push_str("</tbody></table></div>\n");
            // The legend goes with the table it explains: it names the marks
            // the rows above it carry, so it renders beside them and not on a
            // page whose table a zero-hit or an empty window replaced with a
            // sentence.
            out.push_str(&format!(
                "<p class=sub>time-state legend: {GLYPH_EXACT} exact · \
                 {GLYPH_INTERPRETED} inferred, interpreted, a conversation-list update, or a \
                 partial range · {GLYPH_UNKNOWN} unknown · {GLYPH_NO_CONTENT} no conversation \
                 content · \"(partial)\" = the bounds cover only part of the conversation span — \
                 a time cell's tooltip names where its time came from</p>\n"
            ));
        }
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
    out.push_str(&export_cli_block(resolved, data));
    out.push_str(&footer(data));
    out.push_str("</body></html>\n");
    out
}

/// The CLI-parity block (29-UI-DESIGN §3.5/§4.7, R7): the `chat-stasher
/// export` command that selects exactly this page's view, plus the pointer to
/// the single-session download. The command's flags come from
/// [`conjoined_flags`] — the same selector constraint walk
/// [`describe_selector`] renders — so the page's prose sentence about the
/// filter and the command it prints cannot drift apart.
///
/// `Err` from the walk is a filter pair that cannot both hold (or cannot be
/// spelled on one command line). The block then says so and prints no
/// command: a command that almost matches would export something other than
/// what the page is showing, quietly. What the block never does is count the
/// set for the command — the match sentence above the table already names
/// exactly what the filter selected.
fn export_cli_block(resolved: &Resolved, data: &UiData) -> String {
    let mut out = String::from(
        "<section id=export-cli>\n<h2>Export this view (CLI)</h2>\n<p>The same \
                      filters this page applied, spelled as <code>chat-stasher export</code> \
                      flags, ready to copy.</p>\n",
    );
    match conjoined_flags(&data.launch, &resolved.selector) {
        Ok(flags) => {
            // A dashboard opened with `--repo` has no destination to name; its
            // label is a placeholder word, and the repository path itself stays
            // off the page. The command carries the flag the reader fills in,
            // never an argument nobody can type.
            let target = if data.destination_label == EXPLICIT_REPO_LABEL {
                "--repo <this dashboard's repository>".to_string()
            } else {
                format!("--destination {}", data.destination_label)
            };
            let mut command = format!("chat-stasher export {target}");
            if !flags.is_empty() {
                command.push_str(&format!(" {flags}"));
            }
            command.push_str(" --out ~/out");
            out.push_str(&format!(
                "<pre>{}</pre>\n\
                 <p class=sub>Running it writes one file per selected session under \
                 <code>--out</code> — <code>&lt;out&gt;/&lt;machine&gt;/&lt;harness&gt;/\
                 &lt;session-id&gt;.jsonl</code>, and <code>--out</code> must name a new or \
                 empty directory — byte-identical to what <code>chat-stasher read</code> \
                 returns for that session, and <code>--dry-run</code> prints what it will do \
                 before it does any of it. One session needs no command: its session page \
                 offers a <b>download .jsonl</b> link whose bytes are exactly \
                 <code>read</code>'s, with the same digest the command prints riding the \
                 response as <code>X-Checksum-Sha256</code>.</p>\n</section>\n",
                esc(&command)
            ));
        }
        Err(why) => {
            out.push_str(&format!(
                "<p>This view's filters cannot be spelled as one \
                 <code>chat-stasher export</code> command: {}. Nothing is exported by \
                 accident — narrow the page's filter, or reopen <code>chat-stasher ui</code> \
                 with different filter flags, and the command this block prints will select \
                 exactly what that page shows.</p>\n</section>\n",
                esc(&why)
            ));
        }
    }
    out
}

/// The three "nothing here" sentences `search` already distinguishes. Reusing
/// its vocabulary — rather than one "no results" line — is the point.
fn no_hit_html(sel: &Selection<'_>, data: &UiData) -> String {
    if !data.complete() {
        let failed = data.incomplete_destinations();
        if data.destinations.len() == 1 {
            return format!(
                "<div class=warn><b>UNKNOWN — not \"not there\".</b> The destination could not be \
                 read in full ({} part(s) unreadable), so 0 matched in the part that could be read \
                 proves nothing.</div>\n",
                data.unreadable.len()
            );
        }
        // The failing destinations are named and the healthy ones are not: the
        // reader has to act on the broken copy, and listing the others under a
        // "could not be read" sentence would be a false statement about them.
        return format!(
            "<div class=warn><b>UNKNOWN — not \"not there\".</b> {} of the {} destinations could \
             not be read in full ({}), so 0 matched in what could be read proves nothing. The \
             other destination(s) read in full — their 0 is real, and this answer is still not \
             a proven absence of the whole view.</div>\n",
            failed.len(),
            data.destinations.len(),
            esc(&failed.join(DESTINATION_JOIN)),
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
    if data.destinations.len() > 1 {
        return format!(
            "<p><b>Not in these destinations</b> — 0 of the {} session(s) in view matched, and \
             all {} destinations were read in full. This is a real absence, not a failure to \
             look.</p>\n",
            data.sessions.len(),
            data.destinations.len(),
        );
    }
    format!(
        "<p><b>Not in this destination</b> — 0 of the {} session(s) in view matched, and the \
         destination was read in full. This is a real absence, not a failure to look.</p>\n",
        data.sessions.len()
    )
}

/// The line under the match count that names the window and the order in
/// force (§5.2: the header prints the sort — one phrase, one place). The
/// range is 1-based and inclusive, the same corners a reader would count
/// from the rows actually on the page.
fn range_sentence(page: &Page, ordered: &[&UiSession], shown: usize) -> String {
    // An empty window has no range to name; the sentence it gets instead is
    // the empty-window one, and that one states the matched total itself.
    if shown == 0 {
        return String::new();
    }
    let total = ordered.len();
    // `archive order` names the sequence's source rather than a key it was
    // sorted by, so the sentence says it plainly rather than "sorted by
    // archive order", which would read as one more ranking.
    let order_words = match page.sort {
        ListSort::ArchiveOrder => "archive order".to_string(),
        sort => format!("sorted by {}", sort.describe()),
    };
    let mut out = format!(
        "<p class=sub>Sessions {}–{} of {} · {}",
        page.offset + 1,
        page.offset + shown,
        total,
        order_words
    );
    // §5.2's note: unknown times are never ranked into a timeline. The line
    // says so exactly when the order in force ranks time and some row has
    // none — otherwise the bottom of the list would read as "oldest", a rank
    // the archive does not record.
    let unranked = match page.sort {
        ListSort::LastDesc | ListSort::LastAsc => ordered.iter().any(|s| s.last_unix.is_none()),
        ListSort::FirstDesc | ListSort::FirstAsc => ordered.iter().any(|s| s.first_unix.is_none()),
        ListSort::ArchiveOrder | ListSort::SizeDesc => false,
    };
    if unranked {
        out.push_str(
            " · sessions with an unknown conversation time are not ranked and \
                      stay at the bottom",
        );
    }
    out.push_str("</p>\n");
    out
}

/// The paging links for this route: the shared builder (29-UI-DESIGN §5.2),
/// pointed at `/sessions` with the keys this route reads.
fn list_nav(params: &Query, token: &str, page: Page, total: usize) -> String {
    paging_nav(
        "/sessions",
        &LIST_CARRY,
        "session pages",
        params,
        token,
        page,
        total,
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
    if data.destinations.len() > 1 {
        return merged_label_coverage_note(sel, data);
    }
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

/// The same note for a merged view, where the machine's rows can come from
/// several copies and only some of those indexes are old.
///
/// The repair is per destination — `activity-index --rebuild` writes one
/// destination's snapshot — so the note names the destinations that actually
/// reported the machine as legacy rather than the whole merged view. Attaching
/// the repair to a destination that has a good index for that machine would
/// send the reader to rebuild something that is not broken, and naming no
/// destination would leave the command unrunnable.
fn merged_label_coverage_note(sel: &Selection<'_>, data: &UiData) -> String {
    let on_page: BTreeSet<&str> = sel.matched.iter().map(|s| s.machine.as_str()).collect();
    // (machine, the destinations whose index is old for it), in destination
    // order — the same order the rows and the badges use.
    let mut by_machine: Vec<(&str, Vec<&str>)> = Vec::new();
    for d in &data.destinations {
        for machine in &d.machines_with_legacy_index {
            let machine = machine.as_str();
            if !on_page.contains(machine) {
                continue;
            }
            match by_machine.iter_mut().find(|(m, _)| *m == machine) {
                Some((_, dests)) => {
                    if !dests.contains(&d.label.as_str()) {
                        dests.push(d.label.as_str());
                    }
                }
                None => by_machine.push((machine, vec![d.label.as_str()])),
            }
        }
    }
    by_machine
        .iter()
        .map(|(machine, dests)| {
            let commands: String = dests
                .iter()
                .map(|d| {
                    format!(
                        "<span class=mono>chat-stasher activity-index --rebuild --destination {} \
                         --machine {m} --stage <an existing empty work directory></span>",
                        esc(d),
                        m = esc(machine),
                    )
                })
                .collect::<Vec<_>>()
                .join("<br>");
            let where_ = if dests.len() == 1 {
                format!("in <span class=mono>{}</span>", esc(dests[0]))
            } else {
                format!(
                    "in {} of the destinations read ({})",
                    dests.len(),
                    esc(&dests.join(DESTINATION_JOIN))
                )
            };
            format!(
                "<div class=note><b>Label coverage is partial.</b> Machine \
                 <span class=mono>{m}</span>'s activity index {where_} predates labels, so the \
                 rows that came from there show <i>label unknown</i> — that is the index's age, \
                 not a session with no label. The same machine's rows from another destination \
                 can carry a label; the destination column says which copy a row came from. \
                 Backfill each one with:<br>{commands}</div>\n",
                m = esc(machine),
                where_ = where_,
                commands = commands,
            )
        })
        .collect()
}

fn list_row(s: &UiSession, token: &str, data: &UiData) -> String {
    let msgs = msgs_cell_html(s);
    let f = time_cell_html(s, s.first_unix);
    let l = time_cell_html(s, s.last_unix);
    format!(
        "<tr><td class=mono>{m}</td><td>{h}</td>\
         <td><a class=mono href=\"/session?i={i}&token={t}\">{sid}</a></td>{dest}{label}{msgs}\
         <td class=n>{sh}</td><td class=n>{b}</td>{f}{l}<td>{snap}</td></tr>\n",
        m = esc(&s.machine),
        h = esc(&s.source_label()),
        i = s.index,
        t = percent_encode(token),
        sid = esc(&s.short_id),
        dest = destination_cell_html(s, data),
        label = label_cell_html(s),
        msgs = msgs,
        sh = s.shard_count,
        b = esc(&fmt_bytes(s.bytes)),
        f = f,
        l = l,
        snap = esc(&fmt_unix(s.archive_time_unix)),
    )
}

/// The destination cell — rendered only where the destination column exists, so
/// it must stay in step with the header's own `if`.
///
/// One destination: the name, no badge. More than one: every name the session
/// is held by, and the `×N backup` badge. The badge is the row's own copy of
/// the fact, because the row is where a reader asks it — the block above the
/// table explains the convention once, but a reader who scrolls straight to a
/// row must still be able to see that this conversation exists twice.
///
/// The cell's tooltip names the copy the row's own facts came from. That is the
/// half of the badge a reader cannot otherwise see: the two copies were pushed
/// at different times, so their times and byte counts can disagree, and this
/// row shows one of them.
fn destination_cell_html(s: &UiSession, data: &UiData) -> String {
    if data.destinations.len() <= 1 {
        return String::new();
    }
    let names: Vec<&str> = s
        .destinations
        .iter()
        .filter_map(|position| data.destination(*position))
        .map(|d| d.label.as_str())
        .collect();
    let listed = if names.is_empty() {
        // A row with no destination cannot happen — every row came from one —
        // so this is a rendering hole, and the cell says so rather than
        // defaulting to a name it does not have.
        "no destination recorded".to_string()
    } else {
        esc(&names.join(DESTINATION_JOIN))
    };
    let mut out = String::from("<td>");
    if names.len() > 1 {
        out.push_str(&format!(
            "<span class=badge title=\"held by {} destinations: {}. This row's label, times and \
             byte count are read from `{}`, the first destination named on the command line — the \
             copies were pushed at different times, so another copy's numbers can differ, and \
             `{}` is the copy the reader and the raw view open.\">×{n} backup</span> ",
            names.len(),
            esc(&names.join(", ")),
            esc(names[0]),
            esc(names[0]),
            n = names.len(),
        ));
    }
    out.push_str(&format!("<span class=mono>{listed}</span></td>"));
    out
}

// -------------------------------------------------------------- time states

/// The marks the list writes beside a conversation time (29-UI-DESIGN §3.2's
/// legend). One mark per honesty class, not per `TimeSource` variant: `✔`
/// for a time recorded as-is, `~` for one this pipeline had to interpret or
/// read only in part, `?` for unknown, `Ø` for a session with no conversation
/// content. The variant-exact word travels on each cell's tooltip, so a mark
/// is a pointer into the legend, never the only claim a row makes.
const GLYPH_EXACT: &str = "\u{2714}";
const GLYPH_INTERPRETED: &str = "~";
const GLYPH_UNKNOWN: &str = "?";
const GLYPH_NO_CONTENT: &str = "\u{2205}";

/// The tooltip word for one row's time: the same vocabulary the reader names
/// a message's timestamp with (`ui::reader::time_source_label`), so the list
/// and the reader cannot drift into naming one state two ways. Like that
/// function, the interpolated `how` is escaped here and the result is used
/// raw — it must not be escaped a second time by the caller.
fn time_source_words(source: &TimeSource) -> String {
    match source {
        TimeSource::Exact => "time source: exact".to_string(),
        TimeSource::Messages { exact: true } => "time source: from messages, exact".to_string(),
        TimeSource::Messages { exact: false } => {
            "time source: from messages, interpreted".to_string()
        }
        TimeSource::Inferred { how } => format!("time source: inferred ({})", esc(how)),
        TimeSource::ListUpdated => "time source: the conversation list update time".to_string(),
        TimeSource::PartialRange { how, .. } => {
            format!("time source: partial range ({})", esc(how))
        }
        TimeSource::Unknown { .. } => "unknown".to_string(),
        TimeSource::NoConversationContent => "no conversation content".to_string(),
    }
}

/// The legend mark one `TimeSource` renders beside its time.
fn mark_of(source: &TimeSource) -> &'static str {
    match source {
        TimeSource::Exact | TimeSource::Messages { exact: true } => GLYPH_EXACT,
        TimeSource::Inferred { .. }
        | TimeSource::Messages { exact: false }
        | TimeSource::ListUpdated
        | TimeSource::PartialRange { .. } => GLYPH_INTERPRETED,
        TimeSource::Unknown { .. } => GLYPH_UNKNOWN,
        TimeSource::NoConversationContent => GLYPH_NO_CONTENT,
    }
}

/// One conversation-time cell (first or last message). The mark and the
/// tooltip are decided by the session's own [`TimeSource`], never re-derived
/// from whether a bound happens to be present: `unknown` and
/// `no conversation content` are states the conversation itself is in, and a
/// cell that showed only its bound would hide them — the label column's rule
/// (see [`SessionLabel`]) applied to time.
fn time_cell_html(s: &UiSession, unix: Option<i64>) -> String {
    if s.time_source.is_no_conversation_content() {
        return format!("<td>{} no conversation content</td>", GLYPH_NO_CONTENT);
    }
    let words = time_source_words(&s.time_source);
    match unix {
        // A bound that is only *part* of the span says so where it is
        // shown: a bare date here would read as the session's whole extent.
        Some(unix) if s.time_source.bounds_are_partial() => format!(
            "<td title=\"{words}\">{} (partial) {GLYPH_INTERPRETED}</td>",
            esc(&fmt_unix(unix))
        ),
        Some(unix) => format!(
            "<td title=\"{words}\">{} {}</td>",
            esc(&fmt_unix(unix)),
            mark_of(&s.time_source)
        ),
        None => {
            // The reader sees the reason on hover; the legend's `?` is the
            // same state at a glance.
            let why = esc(s.time_why.as_deref().unwrap_or("unknown"));
            format!("<td><span class=bad title=\"{why}\">{GLYPH_UNKNOWN} unknown</span></td>")
        }
    }
}

/// The `msgs` cell (29-UI-DESIGN §3.2): the count the activity index measured
/// for this session. The discriminator is the label column's own state, not
/// the time's: a row the index holds was counted even when its conversation
/// time could not be read, while a session with **no row** (any
/// [`SessionLabel::Unknown`] label) was never counted — there, `0` would be a
/// claim no one measured (invariant 1: a count of zero is a measurement, not
/// a fallback), so the cell carries the unknown state and the recorded
/// reason instead of a number.
fn msgs_cell_html(s: &UiSession) -> String {
    if matches!(s.title, SessionLabel::Unknown { .. }) {
        return format!(
            "<td class=n><span class=bad title=\"{}\">unknown</span></td>",
            esc(&why_of_never_counted(s))
        );
    }
    format!("<td class=n>{}</td>", s.line_count)
}

/// What the never-counted `msgs` cell says on hover. The label column's `why`
/// names the real case (`search` writes it: a machine whose index has no row
/// for this session, or a machine with no index at all), so the reasons
/// cannot drift between the label cell and the count cell.
fn why_of_never_counted(s: &UiSession) -> String {
    let why = match &s.title {
        SessionLabel::Unknown { why } => why.clone(),
        _ => "the activity index holds no row for this session, so its lines were never \
              counted"
            .to_string(),
    };
    format!("no line count was measured — {why}")
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
    out.push_str(&read_from_note(s, data));
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
         <a href=\"/content?i={i}&token={t}\"><b>show raw shards</b></a> · \
         <a href=\"/export?i={i}&fmt=jsonl&token={t}\"><b>download .jsonl</b></a> \
         <span class=sub>({b} once, for whichever of the three you click)</span><br> \
         The download is an attachment on this page's row: its bytes are exactly what <code>chat-stasher \
         read</code> returns for this session, its name is the session's short id, and the digest \
         <code>read</code> prints rides the response as <code>X-Checksum-Sha256</code>.</div>\n",
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
         <p class=sub>machine <span class=mono>{m}</span> · source {h}</p>\n{note}\
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
        note = read_from_note(s, data),
        b = esc(&fmt_bytes(c.bytes as u64)),
        n = c.shards.len(),
        sha = esc(&c.concat_sha256),
        shards = shards,
        body = esc(&c.body),
        footer = footer(data),
    )
}
