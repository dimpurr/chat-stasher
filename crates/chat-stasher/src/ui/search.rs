//! search — `/search`, the local full-text index's one screen.
//!
//! The index is a **separate thing** from the archive, and this page is built
//! so nothing on it can be read as the other. It is not a subset of the
//! archive: it is a cache, built at some earlier moment, by an explicit
//! command, over the sessions that read could see. So every answer carries
//! three facts beside it (29-UI-DESIGN §4.6) — how much of this view the index
//! covers, when its file was last written, and which query mode ran — and a
//! zero is not reported as a result until those three say a zero *means*
//! something here:
//!
//! * no index at all — nothing was searched, and the page names the command
//!   that builds one;
//! * an unreadable index — nothing was searched, and the reason names the
//!   repair;
//! * coverage short of the sessions in this view, or a metadata read that did
//!   not finish — `UNKNOWN`: a session that is not in the index cannot be
//!   looked up, which is not the same as looking and not finding it;
//! * hits the active filter rejected, or that this dashboard's list does not
//!   hold at all — each in its own words, because both are about the question
//!   or about the index rather than about the archive;
//! * and only when a complete index was searched over a complete read, with
//!   nothing excluded, "not in the indexed archive".
//!
//! A query shorter than the trigram tokenizer's three characters is its own
//! state for the same reason: the index has no token to look for, so its
//! answer is "I cannot evaluate this", never "nothing matched".
//!
//! The query runs as one literal phrase, so what a reader types is what is
//! searched for — see [`crate::fts::Index::matches`].

use std::collections::{BTreeMap, BTreeSet};

use crate::fts::{MatchPlace, QueryTooShort, RankedMatch};
use crate::search::SessionLabel;
use crate::selector::Resolved;

use super::html::{
    completeness_banner, describe_selector, esc, fmt_unix, head, marked_html, search_footer,
};
use super::{
    archive_document_ids, count_by_machine_id, page_from_query, page_href, paging_nav, param,
    percent_encode, select, selector_from_query, window_of, IndexState, Page, Query, QueryResult,
    Response, TextIndex, UiData, UiSession, EXPLICIT_REPO_LABEL, SEARCH_CARRY,
};

/// The selector keys a `/search` URL may carry, in the order the form writes
/// them. The same list the shared selector reads, minus `sort` (which
/// `/search` refuses) and plus `q`.
const FILTER_KEYS: [&str; 6] = ["session", "machine", "harness", "day", "since", "until"];

pub(super) fn search_page(
    params: &Query,
    token: &str,
    data: &UiData,
    index: &dyn TextIndex,
) -> Response {
    let request = match Request::parse(params) {
        Ok(request) => request,
        // A filter or a paging parameter that cannot resolve is an error, never
        // a page of zero results: a typo is not allowed to masquerade as
        // "nothing matched" (§5.2).
        Err(message) => return Response::text(400, "Bad Request", format!("ui: {message}\n")),
    };
    let answer = solve(&request, data, index);
    Response::html(200, "OK", page_search(&request, &answer, token, data))
}

/// One `/search` request, parsed. Both routes parse through this, so the page
/// and the JSON body cannot answer different questions.
#[derive(Debug, Clone)]
pub(super) struct Request {
    /// The query as matched, shown and carried in links — trimmed once, so a
    /// link cannot search for a different text than the page says it searched.
    pub query: String,
    pub resolved: Resolved,
    pub page: Page,
    /// The query string this request arrived with. Links are rebuilt from it
    /// through the route's own key list, so every filter in force survives
    /// paging without this module re-spelling it.
    pub params: Query,
}

impl Request {
    pub(super) fn parse(params: &Query) -> Result<Self, String> {
        // `/search` has no `sort`: its rows are in the index's relevance order,
        // which is not one of the list's sortable keys. Accepting the parameter
        // and ignoring it would let a URL carry an order the answer never used.
        if let Some(raw) = param(params, "sort") {
            return Err(format!(
                "`sort` is a `/sessions` parameter and `/search` does not sort — its rows are \
                 in the index's relevance order. Got `sort={raw}`"
            ));
        }
        let resolved = selector_from_query(params).map_err(|e| e.to_string())?;
        let page = page_from_query(params).map_err(|e| e.to_string())?;
        Ok(Self {
            query: param(params, "q").unwrap_or("").trim().to_string(),
            resolved,
            page,
            params: params.clone(),
        })
    }
}

/// What one query produced.
#[derive(Debug, Clone)]
pub(super) enum QueryOutcome {
    /// No `q` in the URL: nothing was asked, and nothing here is a result.
    NoQuery,
    /// There is no usable index, so the query was never run. The coverage line
    /// above names the state and the command that changes it.
    NoIndex,
    /// The index cannot evaluate this query at all. Not a zero-result.
    TooShort(QueryTooShort),
    /// A readable index could not answer. Not a zero-result.
    Failed(String),
    /// The index answered.
    Hits(Hits),
}

/// The matches, grouped the way `search` groups them.
#[derive(Debug, Clone, Default)]
pub(super) struct Hits {
    /// The matches the active filter kept, in the index's relevance order.
    pub rows: Vec<Hit>,
    /// Matches the active filter evaluated and rejected.
    pub not_matched: usize,
    /// Matches no active filter could place, with the reason. Neither kept nor
    /// rejected: reported as their own group.
    pub unplaced: Vec<(String, String)>,
    /// Indexed documents this dashboard's list does not hold at all.
    pub not_in_view: usize,
    /// Matches before the filter narrowed them. `unfiltered > rows.len()` is
    /// the difference between "the archive has none" and "the filter excluded
    /// every one the index has".
    pub unfiltered: usize,
    /// The index held more matches than one answer returns. Every count here is
    /// then a floor, and the page says so rather than letting a full last
    /// window read as the end of the index.
    pub truncated: bool,
}

impl Hits {
    /// The window this page shows, under the §5.2 paging contract.
    pub fn window(&self, page: Page) -> &[Hit] {
        window_of(&self.rows, page)
    }

    /// The matched total paging and the zero-hit rules are stated against: the
    /// matches this view kept. The unfiltered count is never substituted for
    /// it, or paging would offer pages of rows the filter excluded.
    pub fn total(&self) -> usize {
        self.rows.len()
    }
}

/// One hit, with everything a rendering needs.
#[derive(Debug, Clone)]
pub(super) struct Hit {
    pub matched: RankedMatch,
    /// The dashboard row this hit is. Every kept hit has one: a hit with no row
    /// was not kept.
    pub row_index: usize,
    /// Where in the conversation the index placed the match, which decides
    /// whether a message anchor can be offered at all.
    pub place: MatchPlace,
}

/// How much of this dashboard's view the index can answer for.
///
/// The question is asked of **session ids**, never of counts. A coverage made
/// of counts cannot tell "these sessions are indexed" from "a different set of
/// the same size is indexed": replace one session by another on the same
/// machine and every per-machine count stays equal, while the new session was
/// never indexed. The page would then call the index complete and print a zero
/// as a proven absence over text nobody looked at.
///
/// `indexed` and `total` are counted over the same set, so
/// `indexed + not_searchable() == total` always — the three numbers on the
/// coverage line and the JSON body add up because they are one measurement,
/// not three.
#[derive(Debug, Clone, Default)]
pub(super) struct Coverage {
    /// Sessions in this view the index holds a document for.
    pub indexed: usize,
    /// Sessions this view holds.
    pub total: usize,
    /// Machines with at least one session in this view the index does not
    /// hold, as `(machine, indexed, in view)`. The machine, not a ratio, is
    /// what makes the gap actionable — "1 machine behind" cannot be acted on.
    pub behind: Vec<(String, usize, usize)>,
}

impl Coverage {
    /// True when every session in this view is in the index. This is what lets
    /// a zero be read as an answer instead of as a hole in the index.
    pub fn complete(&self) -> bool {
        self.behind.is_empty()
    }

    /// Sessions in this view the index cannot answer for.
    pub fn not_searchable(&self) -> usize {
        self.behind
            .iter()
            .map(|(_, indexed, in_view)| in_view.saturating_sub(*indexed))
            .sum()
    }

    fn of(summary: &crate::fts::IndexSummary, data: &UiData) -> Self {
        let in_view = archive_document_ids(data);
        // This view's sessions that the index holds a document for — the exact
        // set, which is what `summary.ids` reports and what `complete()` is a
        // statement about.
        let indexed_ids = count_by_machine_id(
            in_view
                .iter()
                .filter(|id| summary.ids.contains(*id))
                .map(String::as_str),
        );
        let mut behind: Vec<(String, usize, usize)> = Vec::new();
        for (machine, count) in count_by_machine_id(in_view.iter().map(String::as_str)) {
            // reason: a machine the index holds no document for has indexed
            // **zero** of this view's sessions. That is the measurement the
            // coverage line is made of, not a default standing in for an
            // unknown: `indexed_ids` counts this view's own sessions against
            // the index's complete set of ids, so a missing machine key is an
            // absence and not a failure to read.
            let indexed = indexed_ids.get(&machine).copied().unwrap_or(0);
            if indexed < count {
                behind.push((machine, indexed, count));
            }
        }
        Self {
            indexed: indexed_ids.values().sum(),
            total: in_view.len(),
            behind,
        }
    }
}

/// One request's whole answer: what the index is, and what the query produced.
/// Decided once, rendered by both routes.
#[derive(Debug, Clone)]
pub(super) struct Answer {
    /// The index as this request found it.
    pub index: IndexState,
    /// How much of this view it covers — `None` when there is no readable
    /// index, because a coverage computed without one would be a measurement
    /// of nothing.
    pub coverage: Option<Coverage>,
    /// Each destination's own index state, when the server answers for more
    /// than one. `None` on an ordinary single-destination dashboard, where
    /// [`Self::index`] is already that one answer — so the extra line renders
    /// only where it carries information.
    pub index_parts: Option<Vec<(String, IndexState)>>,
    pub outcome: QueryOutcome,
}

impl Answer {
    /// The hits, when a query ran.
    pub fn hits(&self) -> Option<&Hits> {
        match &self.outcome {
            QueryOutcome::Hits(hits) => Some(hits),
            _ => None,
        }
    }
}

/// Answer one request against the index. The only place in this module that
/// reads it, which is what lets the router test prove no other route does.
pub(super) fn solve(request: &Request, data: &UiData, index: &dyn TextIndex) -> Answer {
    // The index's state is read once, and it is the one the whole answer
    // describes: reading it again later could let the coverage line above a
    // result disagree with the result.
    let state = index.state();
    let coverage = match &state {
        IndexState::Ready(summary) => Some(Coverage::of(summary, data)),
        _ => None,
    };
    let outcome = if request.query.is_empty() {
        QueryOutcome::NoQuery
    } else if coverage.is_none() {
        QueryOutcome::NoIndex
    } else {
        match index.query(&request.query) {
            Ok(QueryResult::TooShort(too_short)) => QueryOutcome::TooShort(too_short),
            Ok(QueryResult::Matches(set)) => match group(set, request, data, index) {
                Ok(hits) => QueryOutcome::Hits(hits),
                Err(reason) => QueryOutcome::Failed(reason),
            },
            Err(reason) => QueryOutcome::Failed(reason),
        }
    };
    Answer {
        index: state,
        coverage,
        index_parts: index.parts(),
        outcome,
    }
}

/// Sort the index's matches into the three groups this page reports: kept by
/// the active filter, rejected by it, or placeable by nobody.
///
/// `Err` is a failure to read the index, and it is returned rather than
/// recovered from: a placement that could not be **read** is not the same
/// answer as a placement the index could not **make**, and reporting the first
/// as the second would put "no message number is claimed" on a page whose real
/// problem was that the index went away mid-request.
fn group(
    set: crate::fts::MatchSet,
    request: &Request,
    data: &UiData,
    index: &dyn TextIndex,
) -> Result<Hits, String> {
    // The dashboard's list, keyed the way the index keys a document, so the two
    // id spellings are joined in exactly one place.
    let by_id: BTreeMap<String, &UiSession> = data
        .sessions
        .iter()
        .map(|s| (format!("{}/{}", s.machine, s.session_id), s))
        .collect();
    let selection = select(&data.sessions, &request.resolved.selector);
    let kept_ids: BTreeSet<String> = selection
        .matched
        .iter()
        .map(|s| format!("{}/{}", s.machine, s.session_id))
        .collect();
    let unplaced_ids: BTreeMap<String, String> = selection
        .unplaced
        .iter()
        .map(|(s, _, why)| (format!("{}/{}", s.machine, s.session_id), why.clone()))
        .collect();

    let mut hits = Hits {
        unfiltered: set.matches.len(),
        truncated: set.truncated,
        ..Hits::default()
    };
    let mut kept: Vec<(RankedMatch, usize)> = Vec::new();
    for matched in set.matches {
        let id = matched.id.clone();
        match by_id.get(&id) {
            None => hits.not_in_view += 1,
            Some(row) if kept_ids.contains(&id) => kept.push((matched, row.index)),
            Some(_) if unplaced_ids.contains_key(&id) => {
                hits.unplaced.push((id.clone(), unplaced_ids[&id].clone()))
            }
            Some(_) => hits.not_matched += 1,
        }
    }
    // Only the kept hits are placed: an ordinal is needed only for a row that
    // is rendered, and `placements` answers per document.
    let ids: Vec<String> = kept.iter().map(|(m, _)| m.id.clone()).collect();
    let places = index.placements(&request.query, &ids)?;
    if places.len() != ids.len() {
        // The index's own contract: one answer per document asked about. A
        // short answer is a broken read, and guessing the missing ones into
        // "no message number" would hide it behind a plausible sentence.
        return Err(format!(
            "the index returned {} placement(s) for the {} document(s) it was asked about",
            places.len(),
            ids.len()
        ));
    }
    for (position, (matched, row_index)) in kept.into_iter().enumerate() {
        hits.rows.push(Hit {
            matched,
            row_index,
            place: places[position].clone(),
        });
    }
    Ok(hits)
}

// ------------------------------------------------------------------ rendering

fn page_search(request: &Request, answer: &Answer, token: &str, data: &UiData) -> String {
    let mut out = head(
        &format!("chat-stasher · search · {}", data.destination_label),
        token,
    );
    out.push_str("<h1>Search</h1>\n<p class=sub><a href=\"/?token=");
    out.push_str(&percent_encode(token));
    out.push_str("\">← overview</a> · <a href=\"/sessions?token=");
    out.push_str(&percent_encode(token));
    out.push_str("\">sessions</a> · destination <b>");
    out.push_str(&esc(&data.destination_label));
    out.push_str("</b></p>\n");

    out.push_str(&search_form(request, token, data));
    out.push_str(&completeness_banner(data));
    if let Some(text) = describe_selector(&request.resolved.selector) {
        out.push_str(&format!(
            "<div class=note><b>Filter:</b> {}<br><span class=sub>Applied by the same selector \
             <code>sessions</code> uses to the matches the index returned, so a hit the filter \
             rejects is reported as rejected rather than disappearing.</span></div>\n",
            esc(&text)
        ));
    }
    for warning in &request.resolved.warnings {
        out.push_str(&format!("<div class=warn>{}</div>\n", esc(warning)));
    }
    out.push_str(&result_body(request, answer, token, data));
    // Not `footer`: that one says the page holds no conversation text beyond a
    // one-line label, which is false here by design — this page's excerpts are
    // conversation text out of the index.
    out.push_str(&search_footer(data));
    out.push_str("</body></html>\n");
    out
}

/// The zero-JS form: a plain `GET` to this route, carrying the token and every
/// filter in force as hidden fields.
///
/// A `GET` form submits only its own controls, so without the hidden fields a
/// search started from a filtered page would silently drop the filter — and
/// without the token field it would be answered `403`, because the token is a
/// query parameter here and a browser is not required to keep the query string
/// an action URL already has.
fn search_form(request: &Request, token: &str, data: &UiData) -> String {
    let mut hidden = format!("<input type=hidden name=token value=\"{}\">", esc(token));
    for key in FILTER_KEYS {
        if let Some((_, value)) = request
            .params
            .iter()
            .find(|(candidate, _)| candidate == key)
        {
            hidden.push_str(&format!(
                "<input type=hidden name=\"{}\" value=\"{}\">",
                esc(key),
                esc(value)
            ));
        }
    }
    format!(
        "<form method=get action=\"/search\" role=search>\n\
         <p><label for=q>Search sessions</label> (title + message text) \
         <input type=search id=q name=q value=\"{}\" autofocus size=40> \
         <button type=submit>Search</button></p>\n\
         {hidden}\n</form>\n\
         <p class=sub>{scope}</p>\n",
        esc(&request.query),
        scope = esc(&index_scope_sentence(data)),
    )
}

/// What the index a query runs against actually is, in one sentence, so its
/// results are never read as a search of the whole archive.
fn index_scope_sentence(data: &UiData) -> String {
    format!(
        "The index is not the archive: it holds the sessions that were readable when it was \
         built. The coverage line on every result says how much of this view's {} session(s) \
         that is.",
        data.sessions.len()
    )
}

/// The coverage, the query and the results — or the reason there are none.
fn result_body(request: &Request, answer: &Answer, token: &str, data: &UiData) -> String {
    let mut out = coverage_paragraph(answer);
    match &answer.outcome {
        QueryOutcome::NoQuery => out.push_str(
            "<div class=note><b>No query.</b> Type text above and press Search. Nothing was \
             searched, so no count here is a result count.</div>\n",
        ),
        QueryOutcome::NoIndex => out.push_str(&no_index_html(answer, data)),
        QueryOutcome::TooShort(too_short) => out.push_str(&format!(
            "<div class=warn><b>This query cannot be evaluated.</b> The index matches with \
             SQLite's trigram tokenizer, which needs at least {} characters; `{}` has {}. \
             <b>Nothing was searched</b> — this is not a zero-result, and it is not \
             \"not there\".</div>\n",
            too_short.minimum,
            esc(&request.query),
            too_short.chars,
        )),
        QueryOutcome::Failed(reason) => out.push_str(&format!(
            "<div class=warn><b>The index could not answer.</b> {}. Nothing was searched: this \
             is not a zero-result, and it is not \"not there\".</div>\n",
            esc(reason)
        )),
        QueryOutcome::Hits(hits) => {
            out.push_str(&answered_body(request, hits, answer, token, data))
        }
    }
    out
}

/// The CLI command that builds or clears this page's index, naming the target
/// this dashboard is actually reading.
///
/// The target matters: when the dashboard is opened with `--repo`, the label is
/// [`EXPLICIT_REPO_LABEL`] — the words "(explicit --repo)" — and printing it
/// after `--destination` would put an argument on the page that nobody can
/// type. A suggestion that cannot be run is worse than no suggestion.
fn index_command(data: &UiData, action: &str) -> String {
    // A merged dashboard has one index per destination, and `index build` takes
    // exactly one destination — one index belongs to one repository. So the
    // suggestion is one command per copy, joined rather than folded into a
    // `--destination a,b` that the command itself would refuse (R10/§4.8).
    if data.destinations.len() > 1 {
        return data
            .destinations
            .iter()
            .map(|d| format!("chat-stasher index {action} --destination {}", d.label))
            .collect::<Vec<_>>()
            .join(" · ");
    }
    if data.destination_label == EXPLICIT_REPO_LABEL {
        format!("chat-stasher index {action} --repo <this dashboard's repository>")
    } else {
        format!(
            "chat-stasher index {action} --destination {}",
            data.destination_label
        )
    }
}

/// The two states with no index to query, each naming the command that changes
/// it: a missing index has one to build, an unreadable one has a repair. They
/// are different sentences because they are different actions.
fn no_index_html(answer: &Answer, data: &UiData) -> String {
    match &answer.index {
        IndexState::Missing => format!(
            "<div class=warn><b>No index.</b> No local index has been built for this destination, \
             so this query was never run — any count here would be a measurement of nothing. \
             Build one with <code>{}</code>, then reload this page.</div>\n",
            esc(&index_command(data, "build"))
        ),
        IndexState::Unreadable(reason) => format!(
            "<div class=warn><b>The index could not be read.</b> {}. Nothing was searched: this \
             is not a zero-result, and it is not \"not there\". <code>{clear}</code> followed by \
             <code>{build}</code> rebuilds it from the archive.</div>\n",
            esc(reason),
            clear = esc(&index_command(data, "clear")),
            build = esc(&index_command(data, "build")),
        ),
        // Unreachable by construction (`NoIndex` is only set when there is no
        // readable index) — but stated rather than asserted, so a future state
        // cannot silently render as "fine".
        IndexState::Ready(_) => "<div class=warn><b>UNKNOWN.</b> The index reported no usable \
             state for this request, so nothing was searched.</div>\n"
            .to_string(),
    }
}

/// 29-UI-DESIGN §4.6: every response carries the index's coverage, its file's
/// last write time and the query mode. They are the three facts that decide
/// whether a zero means anything, so they are printed above everything else
/// rather than only when something is wrong.
fn coverage_paragraph(answer: &Answer) -> String {
    let head = match (&answer.index, &answer.coverage) {
        (IndexState::Missing, _) => "<p class=sub>index coverage: <b>none</b> — no index has been \
             built for this destination · index written: none · mode: fts (trigram)</p>\n"
            .to_string(),
        (IndexState::Unreadable(reason), _) => format!(
            "<p class=sub>index coverage: <b>unknown</b> — the index could not be read ({}) · \
             index written: unknown · mode: fts (trigram)</p>\n",
            esc(reason)
        ),
        (IndexState::Ready(summary), Some(coverage)) => {
            let behind = if coverage.behind.is_empty() {
                String::new()
            } else {
                format!(
                    " · <b>{}</b> machine(s) behind: {}",
                    coverage.behind.len(),
                    coverage
                        .behind
                        .iter()
                        .map(|(machine, indexed, in_view)| format!(
                            "{} {indexed}/{in_view}",
                            esc(machine)
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let written = match summary.written_unix {
                Some(unix) => fmt_unix(unix),
                None => "unknown".to_string(),
            };
            format!(
                "<p class=sub>index coverage: <b>{indexed}</b> of <b>{total}</b> session(s) in \
                 this view indexed{behind} · index file written: {written} (the file's mtime, \
                 not a recorded build time) · mode: fts (trigram)</p>\n",
                indexed = coverage.indexed,
                total = coverage.total,
            )
        }
        // Unreachable by construction: the coverage is computed from the same
        // state read. Said as `unknown` rather than asserted, so a future state
        // cannot render as "fine" by falling through.
        (IndexState::Ready(_), None) => "<p class=sub>index coverage: <b>unknown</b> — the index \
             state could not be summarised · mode: fts (trigram)</p>\n"
            .to_string(),
    };
    format!("{head}{}", index_parts_line(answer))
}

/// Which destination each index belongs to, when there is more than one.
///
/// Only rendered for a merged dashboard: with one destination the coverage line
/// above *is* that destination's own state, and repeating it per copy would make
/// the same fact look like two. With several it is the line that answers the
/// question the merged coverage line cannot: the merged index is one object, but
/// the thing the reader has to repair is one destination's file.
fn index_parts_line(answer: &Answer) -> String {
    let Some(parts) = &answer.index_parts else {
        return String::new();
    };
    let items: Vec<String> = parts
        .iter()
        .map(|(label, state)| {
            let words = match state {
                IndexState::Ready(summary) => format!(
                    "ready — {} document(s){}",
                    summary.ids.len(),
                    match summary.written_unix {
                        Some(unix) => format!(", written {}", esc(&fmt_unix(unix))),
                        None => String::new(),
                    }
                ),
                IndexState::Missing => "no index built".to_string(),
                IndexState::Unreadable(reason) => format!("unreadable ({})", esc(reason)),
            };
            format!("<span class=mono>{}</span> {words}", esc(label))
        })
        .collect();
    format!(
        "<p class=sub>index per destination: {} — a query here runs against every readable \
         index, and a document held by two destinations is one result.</p>\n",
        items.join(" · ")
    )
}

fn answered_body(
    request: &Request,
    hits: &Hits,
    answer: &Answer,
    token: &str,
    data: &UiData,
) -> String {
    let mut out = String::new();
    let total = hits.total();
    let window = hits.window(request.page);
    out.push_str(&format!(
        "<p class=sub>query: <b>{query}</b> · mode: fts (trigram) · <b>{total}</b> result(s) \
         over {unfiltered} indexed document(s) the text matched{truncated}{filtered}</p>\n",
        query = esc(&request.query),
        unfiltered = hits.unfiltered,
        truncated = if hits.truncated {
            " (the index held more matches than one answer returns, so every count here is a \
             floor)"
        } else {
            ""
        },
        filtered = if hits.unfiltered == total {
            String::new()
        } else {
            format!(
                " ({not_matched} evaluated and rejected by the filter, {unplaced} could not be \
                 placed by it, {not_in_view} not in this dashboard's list)",
                not_matched = hits.not_matched,
                unplaced = hits.unplaced.len(),
                not_in_view = hits.not_in_view,
            )
        }
    ));
    if total == 0 {
        if let Some(reason) = no_hit(answer, data) {
            out.push_str(&no_hit_html(hits, reason, request, answer, data));
        }
        return out;
    }
    out.push_str(&range_sentence(request.page, total, window.len()));
    out.push_str(&paging_nav(
        "/search",
        &SEARCH_CARRY,
        "search result pages",
        &request.params,
        token,
        request.page,
        total,
    ));
    if window.is_empty() {
        // A window past the last match is a position, not a count — the same
        // sentence `/sessions` uses, for the same reason.
        out.push_str(&format!(
            "<div class=note><b>No rows on this page.</b> The query matched <b>{total}</b> \
             session(s), but this window starts past the last of them. That is a paging \
             position, not a match count: it is not the same as matching nothing. \
             <a href=\"{back}\">Back to page 1</a></div>\n",
            back = page_href("/search", &SEARCH_CARRY, &request.params, token, 0),
        ));
        return out;
    }
    out.push_str(&format!(
        "<ol class=results start={}>\n",
        request.page.offset + 1
    ));
    for hit in window {
        out.push_str(&hit_html(hit, token, data));
    }
    out.push_str("</ol>\n");
    if !hits.unplaced.is_empty() {
        out.push_str(&format!(
            "<section><h2>Could not be placed ({})</h2>\n<p>These documents matched the text, \
             but a filter in force has no answer for them, so they are <b>neither results nor \
             rejections</b>.</p>\n<ul>\n",
            hits.unplaced.len()
        ));
        for (id, why) in &hits.unplaced {
            out.push_str(&format!(
                "<li class=mono>{}</li>\n",
                esc(&document_summary(id))
            ));
            out.push_str(&format!("<li class=sub>{}</li>\n", esc(why)));
        }
        out.push_str("</ul></section>\n");
    }
    out
}

/// The line under the hit count naming the window and the order in force. The
/// order is the index's relevance rank, which is not one of the list's sortable
/// keys — so it is named as what it is rather than by a `sort` value.
fn range_sentence(page: Page, total: usize, shown: usize) -> String {
    if shown == 0 {
        return String::new();
    }
    format!(
        "<p class=sub>Results {}–{} of {} · ranked by the index (bm25, best match first)</p>\n",
        page.offset + 1,
        page.offset + shown,
        total
    )
}

/// One hit: where it is, what matched, and the way in.
fn hit_html(hit: &Hit, token: &str, data: &UiData) -> String {
    let Some(row) = data.session_at(hit.row_index) else {
        // A kept hit always has a row. If the list changed under us, saying so
        // is the only honest option — never a guessed row.
        return format!(
            "<li class=warn>the index matched {}, but this dashboard's list no longer holds \
             that session</li>\n",
            esc(&document_summary(&hit.matched.id))
        );
    };
    let mut out = format!(
        "<li><p class=sub><span class=mono>{machine}</span> · {source} · \
         <span class=mono>{short}</span> · last message {time}</p>\n",
        machine = esc(&row.machine),
        source = esc(&row.source_label()),
        short = esc(&row.short_id),
        time = match row.last_unix {
            Some(unix) => esc(&fmt_unix(unix)),
            None => "unknown".to_string(),
        },
    );
    match &hit.matched.snippet {
        Some(snippet) => out.push_str(&format!(
            "<blockquote>{}</blockquote>\n",
            marked_html(snippet)
        )),
        // No body excerpt: the tokenizer matched this document outside its
        // body, so the text that matched is the session's label.
        None => out.push_str(&format!(
            "<blockquote class=sub>matched in the session label: {}</blockquote>\n",
            esc(&label_text(row))
        )),
    }
    out.push_str(&reader_link(hit, row, token));
    out.push_str("</li>\n");
    out
}

/// A session's label, in the three honest states the list also renders — never
/// an empty string standing in for "there was nothing to show".
fn label_text(row: &UiSession) -> String {
    match &row.title {
        SessionLabel::Known { text, .. } => text.clone(),
        SessionLabel::NoLabelRecorded => "no label was recorded for this session".to_string(),
        SessionLabel::LegacyIndex => {
            "this session's label predates the label keys (legacy index)".to_string()
        }
        SessionLabel::Unknown { why } => why.clone(),
    }
}

/// The way from a hit into the conversation: anchored to the message the index
/// placed the match in when it could place it, and saying so plainly when it
/// could not.
fn reader_link(hit: &Hit, row: &UiSession, token: &str) -> String {
    let plain = format!("/reader?i={}&token={}", row.index, percent_encode(token));
    match hit.place {
        MatchPlace::Message { ordinal } => {
            // The reader's default window, moved to the boundary at or below
            // the message, so the anchor is on the page that is actually
            // served: `#m<n>` on a page that does not hold message n scrolls
            // nowhere.
            let width = crate::normalize::DEFAULT_WINDOW;
            let start = ordinal - (ordinal % width);
            format!(
                "<p><a href=\"/reader?i={i}&m={start}&n={width}&token={t}#m{ordinal}\">\
                 open the conversation at this message ▸</a> <span class=sub>(message \
                 {number})</span></p>\n",
                i = row.index,
                t = percent_encode(token),
                number = ordinal + 1,
            )
        }
        MatchPlace::Label => format!(
            "<p><a href=\"{plain}\">open the conversation from the top ▸</a> <span class=sub>(the \
             match is in this session's label, which is not a message, so there is no message to \
             anchor to)</span></p>\n"
        ),
        MatchPlace::NotRelocated => format!(
            "<p><a href=\"{plain}\">open the conversation from the top ▸</a> <span class=sub>(the \
             index matched this session, but the matched text could not be located in the stored \
             messages, so no message number is claimed)</span></p>\n"
        ),
    }
}

/// A document id is `<machine>/<session_id>`, and a full session id never
/// reaches a page (the privacy line every screen keeps). The machine is what a
/// reader can act on, so it is what is printed.
fn document_summary(id: &str) -> String {
    match id.split_once('/') {
        Some((machine, _)) => format!("a session on {machine}"),
        None => "a document the index holds".to_string(),
    }
}

/// Which zero-hit sentence applies. One decision, taken here, so the page's
/// sentence and the JSON body's word can never be two different answers — and
/// a count alone never decides it: zero is a measurement only once the index
/// and the read are both known to be complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NoHit {
    /// No readable index: nothing was looked up.
    NoIndex,
    /// The text matched documents, but none is a result in this view.
    FilterExcludedAll,
    /// The index matched documents this dashboard's list does not hold, and
    /// nothing else.
    IndexStale,
    /// The index does not cover every session in this view.
    CoverageIncomplete,
    /// The destination read did not finish, so the index could not have
    /// covered it either.
    ReadIncomplete,
    /// A complete index over a complete read, with nothing excluded.
    TrulyAbsent,
}

impl NoHit {
    /// The word `/api/search` reports. The page prints a sentence; a consumer
    /// gets the same distinction as one of these words rather than a phrase it
    /// would have to parse back out of prose.
    pub fn wire(self) -> &'static str {
        match self {
            NoHit::NoIndex => "no_index",
            NoHit::FilterExcludedAll => "filter_excluded_all",
            NoHit::IndexStale => "index_stale",
            NoHit::CoverageIncomplete => "coverage_incomplete",
            NoHit::ReadIncomplete => "read_incomplete",
            NoHit::TrulyAbsent => "not_in_indexed_archive",
        }
    }
}

/// Why a query that ran produced no result — `None` when there is no zero to
/// explain.
///
/// The order is by *what explains the absence*, not by which count is larger:
/// the filter is blamed only when it actually rejected or failed to place a
/// document. A match that is not in this dashboard's list was never offered to
/// the filter at all, so when every match is one of those, the index is the
/// thing that is stale and the filter is not the reason.
pub(super) fn no_hit(answer: &Answer, data: &UiData) -> Option<NoHit> {
    let hits = answer.hits()?;
    if hits.total() > 0 {
        return None;
    }
    if hits.unfiltered > 0 {
        let filter_had_a_hand = hits.not_matched > 0 || !hits.unplaced.is_empty();
        return Some(if filter_had_a_hand {
            NoHit::FilterExcludedAll
        } else {
            NoHit::IndexStale
        });
    }
    Some(match answer.coverage.as_ref() {
        None => NoHit::NoIndex,
        Some(coverage) if !coverage.complete() => NoHit::CoverageIncomplete,
        Some(_) if !data.complete() => NoHit::ReadIncomplete,
        Some(_) => NoHit::TrulyAbsent,
    })
}

/// The sentence for a state that cannot be measured at all. Used by the two
/// arms of [`no_hit_html`] that would otherwise have to default a missing
/// coverage to zero, which is the one thing a zero-hit page must never do.
fn unmeasurable(why: &str) -> String {
    format!(
        "<div class=warn><b>UNKNOWN — not \"not there\".</b> {}. Nothing here is a measurement \
         of the archive.</div>\n",
        esc(why)
    )
}

/// The zero-hit sentence for the reason [`no_hit`] decided.
fn no_hit_html(
    hits: &Hits,
    reason: NoHit,
    request: &Request,
    answer: &Answer,
    data: &UiData,
) -> String {
    match reason {
        // The index found text; the reasons none of it is on this page are
        // about the question and the index, not about the archive.
        NoHit::FilterExcludedAll => format!(
            "<div class=warn><b>No results in this view.</b> The index matched <b>{unfiltered}</b> \
             document(s) for `{query}`, but none is a result here: {not_matched} were evaluated \
             and rejected by the filter in force, {unplaced} could not be placed by it, and \
             {not_in_view} are not in this dashboard's session list at all. This is <b>not</b> \
             \"not there\" — it is \"not in what this page could look at\".</div>\n",
            unfiltered = hits.unfiltered,
            query = esc(&request.query),
            not_matched = hits.not_matched,
            unplaced = hits.unplaced.len(),
            not_in_view = hits.not_in_view,
        ),
        NoHit::IndexStale => format!(
            "<div class=warn><b>UNKNOWN — not \"not there\".</b> The index matched {not_in_view} \
             document(s) that this dashboard's session list does not hold, and nothing else. \
             They may belong to a different or older read of the archive, so nothing here proves \
             the text is absent. Rebuild with <code>{build}</code> if this destination's sessions \
             changed machine or were re-collected.</div>\n",
            not_in_view = hits.not_in_view,
            build = esc(&index_command(data, "build")),
        ),
        // No readable index at all: the coverage line above already named the
        // command that changes it, and this sentence states only the verdict.
        NoHit::NoIndex => "<p><b>No index was searched</b>, so there is no zero to read here — \
             see the coverage line above for the state and the command that changes it.</p>\n"
            .to_string(),
        // The two reasons below are reported only for a coverage that was read,
        // so their `None` arms are unreachable. Each says so in words rather
        // than defaulting to zero: a fallback there would print a coverage of
        // nothing as if it were a measurement of the archive.
        NoHit::CoverageIncomplete => match answer.coverage.as_ref() {
            Some(coverage) => format!(
                "<div class=warn><b>UNKNOWN — not \"not there\".</b> The index covers \
                 <b>{indexed}</b> of the <b>{total}</b> session(s) in this view, so \
                 <b>{missing}</b> session(s) are <i>not searchable</i>: a session that is not in \
                 the index cannot be looked up, which is not the same as looking and not finding \
                 it.<ul>{machines}</ul></div>\n",
                indexed = coverage.indexed,
                total = coverage.total,
                missing = coverage.not_searchable(),
                machines = coverage
                    .behind
                    .iter()
                    .map(|(machine, indexed, in_view)| format!(
                        "<li class=mono>{}: {} of {} session(s) in this view indexed</li>",
                        esc(machine),
                        indexed,
                        in_view
                    ))
                    .collect::<String>(),
            ),
            None => unmeasurable("the index reported a coverage it did not return"),
        },
        NoHit::ReadIncomplete => format!(
            "<div class=warn><b>UNKNOWN — not \"not there\".</b> The index covers this view, but \
             the destination itself could not be read in full ({} part(s) unreadable), so \
             sessions missing from this list were never indexed either.</div>\n",
            data.unreadable.len()
        ),
        NoHit::TrulyAbsent => match answer.coverage.as_ref() {
            Some(coverage) => format!(
                "<p><b>Not in the indexed archive</b> — 0 of the {} indexed session(s) in this \
                 view matched, the index covers every one of them, and the destination was read \
                 in full. This is a real absence, not a failure to look.</p>\n",
                coverage.indexed
            ),
            None => unmeasurable("the index reported a coverage it did not return"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::fixture::{self, StubHit, StubIndex};

    /// The fixture archive's rows, by the ids the index uses:
    /// `m-1/claude-code.m-1.…9765`, `m-1/deepseek.d41f6a2b9c0e47aaaa1111`,
    /// `m-2/claude-code.m-2.…9766`, `m-2/.hidden-session`.
    const ON_M1: &str = "m-1/claude-code.m-1.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    const ON_M2: &str = "m-2/claude-code.m-2.019bf00d-97b6-7eb2-9bf8-eacbacc09766";
    const DEEPSEEK_M1: &str = "m-1/deepseek.d41f6a2b9c0e47aaaa1111";
    const HIDDEN_M2: &str = "m-2/.hidden-session";

    fn page(target: &str, index: &StubIndex) -> Response {
        page_with(target, &fixture::data(), index)
    }

    fn page_with(target: &str, data: &UiData, index: &StubIndex) -> Response {
        let (path, params) = crate::ui::split_target(target);
        crate::ui::handle(path, &params, "t", data, &crate::ui::NoContent, index)
            .unwrap_or_else(|| panic!("`{target}` must be a known route"))
    }

    /// A complete index over the fixture archive: both machines, every row.
    fn complete_index() -> StubIndex {
        StubIndex::covering(&fixture::data(), &["m-1", "m-2"])
    }

    /// The same archive with `m-2` left out of the index — a short coverage
    /// the page must report rather than answer through.
    fn index_short_on_m2() -> StubIndex {
        StubIndex::covering(&fixture::data(), &["m-1"])
    }

    // ------------------------------------------------- the states of a zero

    /// No index at all: the page says no index, names the command that builds
    /// one, and never prints a zero that could be read as an answer.
    #[test]
    fn no_index_is_its_own_state_and_names_the_build_command() {
        let html = page("/search?q=synthetic", &StubIndex::missing()).body;
        assert!(html.contains("No index."), "{html}");
        assert!(
            html.contains("index build --destination dest-under-test"),
            "the sentence must name the command that changes the state: {html}"
        );
        assert!(
            !html.contains("Not in the indexed archive"),
            "an unbuilt index must never render as a proven absence: {html}"
        );
        assert!(html.contains("index coverage: <b>none</b>"), "{html}");
    }

    /// A suggestion the reader cannot run is worse than none: the label of a
    /// dashboard opened with `--repo` is a placeholder, and pasting it after
    /// `--destination` names no destination at all.
    #[test]
    fn the_printed_index_commands_name_a_target_that_can_be_typed() {
        // The explicit-repo label is what `ui --repo` sets, so the page reached
        // that way must not print a command containing it.
        let mut data = fixture::data();
        data.destination_label = crate::ui::EXPLICIT_REPO_LABEL.to_string();
        for (index, phrase) in [
            (StubIndex::missing(), "index build"),
            (StubIndex::unreadable("corrupt"), "index clear"),
        ] {
            let html = page_with("/search?q=synthetic", &data, &index).body;
            assert!(html.contains(phrase), "{html}");
            assert!(
                !html.contains("--destination (explicit --repo)"),
                "the page printed an argument nobody can type: {html}"
            );
            // The placeholder's apostrophe is escaped in the HTML (`&#39;`), so
            // the assertion is on the part of the command that is not.
            assert!(
                html.contains(&format!("{phrase} --repo")),
                "the command must name the repository instead: {html}"
            );
        }
        // A named destination keeps the flag the design specifies (§6.4).
        let html = page("/search?q=synthetic", &StubIndex::missing()).body;
        assert!(
            html.contains("index build --destination dest-under-test") && !html.contains("--repo"),
            "{html}"
        );
    }

    /// An index that exists and cannot be read is a different state from one
    /// that was never built, and it names the repair rather than a build.
    #[test]
    fn an_unreadable_index_names_the_repair_and_not_a_build() {
        let html = page(
            "/search?q=synthetic",
            &StubIndex::unreadable(
                "corrupt FTS index; use `chat-stasher index clear` then rebuild",
            ),
        )
        .body;
        assert!(html.contains("The index could not be read."), "{html}");
        assert!(html.contains("corrupt FTS index"), "{html}");
        assert!(
            html.contains("index clear"),
            "the repair must reach the reader: {html}"
        );
        assert!(!html.contains("No local index has been built"), "{html}");
    }

    /// A one- or two-character query cannot be evaluated by the tokenizer, so
    /// it is not an empty result — the page must say which of the two it is.
    #[test]
    fn a_query_too_short_is_its_own_state_not_a_zero_result() {
        let html = page("/search?q=ab", &complete_index()).body;
        assert!(html.contains("This query cannot be evaluated"), "{html}");
        assert!(html.contains("needs at least 3 characters"), "{html}");
        assert!(
            !html.contains("Not in the indexed archive"),
            "a query the index cannot evaluate must never read as absence: {html}"
        );
        // …and the state is the index's answer, not the page's guess: the
        // index was asked, and it is the index that reported the refusal.
        let index = complete_index();
        page("/search?q=ab", &index);
        assert!(
            index.calls.borrow().iter().any(|call| call == "query:ab"),
            "{:?}",
            index.calls.borrow()
        );
    }

    /// An index that does not cover every session in view reports UNKNOWN, and
    /// names the machine that is short rather than only a total.
    #[test]
    fn an_incomplete_index_reports_unknown_and_names_the_machine() {
        let html = page("/search?q=synthetic", &index_short_on_m2()).body;
        assert!(html.contains("UNKNOWN — not \"not there\"."), "{html}");
        assert!(html.contains("not searchable"), "{html}");
        assert!(
            html.contains("m-2: 0 of 2 session(s) in this view indexed"),
            "the short machine must be named with its counts: {html}"
        );
        assert!(
            !html.contains("Not in the indexed archive"),
            "an incompletely indexed view must never render as absence: {html}"
        );
    }

    /// An index answers for a **session**, not for a slot: coverage is read
    /// from the ids the index holds, never from how many documents sit on each
    /// machine.
    ///
    /// The two sets below have the same size and the same machine split, and
    /// differ in one session. A coverage made of counts cannot tell them
    /// apart, so it would call the second index complete — and the page would
    /// print "not in the indexed archive" for a session that the index has
    /// never seen, which is the one sentence on this page that claims to have
    /// looked.
    #[test]
    fn coverage_is_read_from_the_session_ids_and_not_from_the_counts() {
        // m-2's `…9766` replaced by a session this view does not hold: four
        // documents, two machines, one of them a different session.
        let replaced = StubIndex::ready(&[
            ON_M1,
            DEEPSEEK_M1,
            HIDDEN_M2,
            "m-2/claude-code.m-2.019bf00d-97b6-7eb2-9bf8-eacbacc09799",
        ]);
        let body = page_with(
            "/api/search?q=definitely-not-in-this-archive",
            &fixture::data(),
            &replaced,
        )
        .body;
        let value: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("`/api/search` is not JSON: {e}\n{body}"));
        assert_eq!(
            value["no_hit"].as_str(),
            Some("coverage_incomplete"),
            "a session the index does not hold must not be answered for: {body}"
        );
        assert_eq!(
            value["coverage"]["complete"].as_bool(),
            Some(false),
            "{body}"
        );
        assert_eq!(
            value["coverage"]["not_searchable"].as_u64(),
            Some(1),
            "{body}"
        );
        assert_eq!(value["coverage"]["indexed"].as_u64(), Some(3), "{body}");
        // The three numbers are one measurement, and this is what makes the
        // coverage line readable: a consumer must never have to guess whether
        // `indexed` counts this view or the whole index.
        assert_eq!(
            value["coverage"]["indexed"].as_u64().unwrap()
                + value["coverage"]["not_searchable"].as_u64().unwrap(),
            value["coverage"]["in_view"].as_u64().unwrap(),
            "{body}"
        );
        let html = page("/search?q=definitely-not-in-this-archive", &replaced).body;
        assert!(html.contains("not searchable"), "{html}");
        assert!(
            !html.contains("Not in the indexed archive"),
            "the replacement session was never searched, so nothing here proves an absence: {html}"
        );
        // The other direction, so that this pins the id and not merely a size:
        // the view's own four ids are complete, and a zero then earns the
        // absence sentence.
        let html = page(
            "/search?q=definitely-not-in-this-archive",
            &complete_index(),
        )
        .body;
        assert!(html.contains("Not in the indexed archive"), "{html}");
    }

    /// A destination read that did not finish leaves the same hole for a
    /// different reason: the sessions missing from the list were never indexed
    /// either, so a zero is still not an answer.
    #[test]
    fn an_incomplete_destination_read_reports_unknown() {
        let html = page_with(
            "/search?q=synthetic",
            &fixture::partial_data(),
            &complete_index(),
        )
        .body;
        assert!(html.contains("UNKNOWN — not \"not there\"."), "{html}");
        assert!(html.contains("could not be read in full"), "{html}");
        assert!(
            !html.contains("Not in the indexed archive"),
            "an unfinished read must never render as absence: {html}"
        );
    }

    /// Only a complete index over a complete, fully covered read earns the
    /// sentence that says the text is really not there.
    #[test]
    fn a_complete_index_over_a_complete_read_reports_a_real_absence() {
        let html = page(
            "/search?q=definitely-not-in-this-archive",
            &complete_index(),
        )
        .body;
        assert!(html.contains("Not in the indexed archive"), "{html}");
        assert!(html.contains("0 of the 4 indexed session(s)"), "{html}");
        assert!(!html.contains("UNKNOWN"), "{html}");
    }

    /// Text that matched documents the filter rejected is neither a result nor
    /// an absence, and the page counts all three groups.
    #[test]
    fn hits_the_filter_rejected_are_reported_as_rejected() {
        let html = page(
            "/search?q=synthetic&machine=m-1",
            &complete_index().with_hits(vec![
                StubHit::placed(ON_M2, "a \u{1}synthetic\u{2} one", 0),
                StubHit::placed(HIDDEN_M2, "a \u{1}synthetic\u{2} two", 0),
            ]),
        )
        .body;
        assert!(html.contains("No results in this view."), "{html}");
        assert!(
            html.contains("evaluated and rejected by the filter"),
            "{html}"
        );
        assert!(
            !html.contains("Not in the indexed archive"),
            "a filtered-out hit must never read as absence: {html}"
        );
    }

    /// A document the index holds that this dashboard's list does not is a
    /// stale index, not a proven absence.
    #[test]
    fn a_hit_the_list_does_not_hold_reports_a_stale_index() {
        let html = page(
            "/search?q=synthetic",
            &complete_index().with_hits(vec![StubHit::placed(
                "m-9/vanished-session",
                "a \u{1}synthetic\u{2} one",
                0,
            )]),
        )
        .body;
        assert!(html.contains("UNKNOWN — not \"not there\"."), "{html}");
        assert!(html.contains("session list does not hold"), "{html}");
        assert!(html.contains("index build"), "{html}");
    }

    // ------------------------------------------------------------ the hits

    /// The excerpt is conversation text: every character of it is escaped, and
    /// the only markup in it is the `<mark>` the index's own markers became.
    #[test]
    fn a_snippet_is_escaped_and_only_the_marked_span_becomes_markup() {
        let html = page(
            "/search?q=script",
            &complete_index().with_hits(vec![StubHit::placed(
                ON_M1,
                "before <script>alert(1)</script> \u{1}script\u{2} after & \"quoted\"",
                3,
            )]),
        )
        .body;
        assert!(
            !html.contains("<script>"),
            "conversation text must never reach the page as markup: {html}"
        );
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{html}"
        );
        assert!(html.contains("&amp; &quot;quoted&quot;"), "{html}");
        assert!(
            html.contains("<mark>script</mark>"),
            "the matched span must be marked: {html}"
        );
    }

    /// A hit the index could place deep-links to the message, in the window
    /// the reader opens by default — an anchor the served page actually holds.
    #[test]
    fn a_placed_hit_deep_links_into_the_message_window() {
        let html = page(
            "/search?q=synthetic",
            &complete_index().with_hits(vec![
                StubHit::placed(ON_M1, "a \u{1}synthetic\u{2} one", 7),
                StubHit::placed(DEEPSEEK_M1, "another \u{1}synthetic\u{2}", 60),
            ]),
        )
        .body;
        // Message 8 (0-based 7) sits in the reader's first window.
        assert!(html.contains("m=0&n=50&token=t#m7"), "{html}");
        // Message 61 sits in the second: the window moves to the boundary at
        // or below it, so the anchor is on the page that is served.
        assert!(html.contains("m=50&n=50&token=t#m60"), "{html}");
    }

    /// A match the index could not place still links, but claims no message.
    #[test]
    fn a_hit_that_could_not_be_placed_carries_no_anchor() {
        for hit in [
            StubHit {
                id: ON_M1.to_string(),
                excerpt: Some("a \u{1}synthetic\u{2} one".to_string()),
                ordinal: None,
            },
            StubHit::in_label(ON_M1),
        ] {
            let html = page(
                "/search?q=synthetic",
                &complete_index().with_hits(vec![hit]),
            )
            .body;
            assert!(
                !html.contains("#m"),
                "no anchor may be claimed without a placement: {html}"
            );
            assert!(
                html.contains("open the conversation from the top"),
                "{html}"
            );
        }
    }

    /// A document whose body held no match has no excerpt: `snippet()` would
    /// otherwise hand back the body's opening tokens unmarked, which reads as
    /// an excerpt of a match that is not there.
    #[test]
    fn a_label_only_match_shows_the_label_instead_of_a_body_excerpt() {
        let html = page(
            "/search?q=synthetic+label",
            &complete_index().with_hits(vec![StubHit::in_label(ON_M1)]),
        )
        .body;
        assert!(html.contains("matched in the session label"), "{html}");
        assert!(!html.contains("<blockquote>"), "{html}");
    }

    // ----------------------------------------------------------- the paging

    /// §5.2 for the search route: the page names its window, its order is the
    /// index's rank, and walking the offsets reproduces the whole result set
    /// with the query and the filter carried.
    #[test]
    fn paging_walks_the_hits_and_carries_the_query_and_the_filter() {
        let hits = vec![
            StubHit::placed(ON_M1, "a \u{1}synthetic\u{2} one", 0),
            StubHit::placed(DEEPSEEK_M1, "a \u{1}synthetic\u{2} two", 0),
            StubHit::placed(ON_M1, "a \u{1}synthetic\u{2} three", 0),
        ];
        let index = complete_index().with_hits(hits);
        let first = page("/search?q=synthetic&limit=2", &index).body;
        assert!(first.contains("Results 1–2 of 3"), "{first}");
        assert!(first.contains("ranked by the index"), "{first}");
        assert!(
            first.contains("offset=2") && first.contains("q=synthetic"),
            "the next link must carry the window and the query: {first}"
        );
        let second = page("/search?q=synthetic&limit=2&offset=2", &index).body;
        assert!(second.contains("Results 3–3 of 3"), "{second}");
        // The two pages hold different rows: the query block's third hit is on
        // the second page, and the first page's two are not.
        assert!(second.contains("m=0&n=50&token=t#m0"), "{second}");
        // A window past the end is a position, never a count.
        let past = page("/search?q=synthetic&limit=2&offset=99", &index).body;
        assert!(past.contains("No rows on this page."), "{past}");
        assert!(past.contains("Back to page 1"), "{past}");
    }

    /// A parameter `/search` does not read is refused rather than ignored: a
    /// URL must not carry an order the answer never used.
    #[test]
    fn sort_is_refused_on_the_search_route() {
        let response = page("/search?q=synthetic&sort=size-desc", &complete_index());
        assert_eq!(response.status, 400, "{}", response.body);
        assert!(response.body.contains("/sessions"), "{}", response.body);
        // …and an unresolvable filter is refused the same way, never rendered
        // as a page of zero results.
        let response = page("/search?q=synthetic&day=nonsense", &complete_index());
        assert_eq!(response.status, 400, "{}", response.body);
        let response = page("/search?q=synthetic&limit=0", &complete_index());
        assert_eq!(response.status, 400, "{}", response.body);
    }

    // ----------------------------------------------------------- the shape

    /// The JSON body and the page are two renderings of one decision.
    ///
    /// Each state is checked twice and against **different observable text**:
    /// the JSON field a consumer reads, and the phrase the page prints. Both
    /// are read out of real responses, so this fails if either rendering stops
    /// making the distinction the other one makes — checking that two calls to
    /// one function agree would prove nothing.
    #[test]
    fn the_json_and_the_page_make_the_same_distinction() {
        // (target, data, index, json field, json value, a phrase only that
        // state's sentence contains)
        let cases: Vec<(String, UiData, StubIndex, &str, &str, &str)> = vec![
            (
                "/search?q=synthetic".to_string(),
                fixture::data(),
                StubIndex::missing(),
                "query_state",
                "no_index",
                "No index.",
            ),
            (
                "/search?q=ab".to_string(),
                fixture::data(),
                complete_index(),
                "query_state",
                "too_short",
                "This query cannot be evaluated",
            ),
            (
                "/search?q=synthetic".to_string(),
                fixture::data(),
                index_short_on_m2(),
                "no_hit",
                "coverage_incomplete",
                "not searchable",
            ),
            (
                "/search?q=synthetic".to_string(),
                fixture::partial_data(),
                complete_index(),
                "no_hit",
                "read_incomplete",
                "could not be read in full",
            ),
            (
                "/search?q=nothing-matches-this".to_string(),
                fixture::data(),
                complete_index(),
                "no_hit",
                "not_in_indexed_archive",
                "Not in the indexed archive",
            ),
            (
                "/search?q=synthetic".to_string(),
                fixture::data(),
                complete_index().with_hits(vec![StubHit::placed("m-9/gone", "a \u{1}s\u{2}", 0)]),
                "no_hit",
                "index_stale",
                "session list does not hold",
            ),
            (
                "/search?q=synthetic&machine=m-1".to_string(),
                fixture::data(),
                complete_index().with_hits(vec![StubHit::placed(ON_M2, "a \u{1}s\u{2}", 0)]),
                "no_hit",
                "filter_excluded_all",
                "No results in this view.",
            ),
        ];
        for (target, data, index, field, expected, phrase) in cases {
            let body = page_with(&target.replace("/search?", "/api/search?"), &data, &index).body;
            let value: serde_json::Value = serde_json::from_str(&body)
                .unwrap_or_else(|e| panic!("{target} is not JSON: {e}\n{body}"));
            assert_eq!(
                value[field].as_str(),
                Some(expected),
                "for {target} ({field}): {body}"
            );
            let html = page_with(&target, &data, &index).body;
            assert!(
                html.contains(phrase),
                "for {target}: the page must print the sentence this state owns ({phrase:?})\n{html}"
            );
        }
    }

    /// The footer's first statement is about the tier the page read, and
    /// `/search` reads a tier that holds conversation text. The metadata-tier
    /// sentence would be false here — it promises that the page's only
    /// conversation text is a one-line label — so this page must not carry it.
    #[test]
    fn the_search_footer_does_not_claim_the_metadata_tier() {
        let html = page("/search?q=synthetic", &complete_index()).body;
        assert!(html.contains("<b>Index tier.</b>"), "{html}");
        assert!(
            !html.contains("The only conversation text it holds is each session's one-line"),
            "the metadata-tier sentence is false on a page that shows excerpts: {html}"
        );
        // …and the metadata pages keep it, so this is a property of the route
        // rather than a rewrite of the shared wording.
        let (path, params) = crate::ui::split_target("/sessions");
        let list = crate::ui::handle(
            path,
            &params,
            "t",
            &fixture::data(),
            &crate::ui::NoContent,
            &complete_index(),
        )
        .unwrap()
        .body;
        assert!(list.contains("<b>Metadata tier.</b>"), "{list}");
    }

    /// The coverage line is on every response (§4.6), even the ones with no
    /// query at all, and it says the mode and the file's write time.
    #[test]
    fn every_response_carries_the_coverage_the_time_and_the_mode() {
        for target in ["/search", "/search?q=synthetic", "/search?q=ab"] {
            let html = page(target, &complete_index()).body;
            assert!(html.contains("index coverage:"), "{target}: {html}");
            assert!(html.contains("mode: fts (trigram)"), "{target}: {html}");
            assert!(
                html.contains("index file written:"),
                "{target} must report when the index was written: {html}"
            );
        }
        // The write time is reported as a file mtime, never as a build time:
        // the index records none, and printing one would invent it.
        let html = page("/search?q=synthetic", &complete_index()).body;
        assert!(html.contains("not a recorded build time"), "{html}");
    }

    /// The form is zero-JS and carries the token and every filter in force, so
    /// a search started from a filtered page does not silently drop them.
    #[test]
    fn the_form_carries_the_token_and_the_filters() {
        let html = page("/search?machine=m-2&q=synthetic", &complete_index()).body;
        assert!(html.contains("method=get"), "{html}");
        assert!(html.contains("action=\"/search\""), "{html}");
        assert!(
            html.contains("type=hidden name=token value=\"t\""),
            "{html}"
        );
        assert!(
            html.contains("type=hidden name=\"machine\" value=\"m-2\""),
            "{html}"
        );
        assert!(html.contains("<label for=q>"), "{html}");
        assert!(html.contains("type=search"), "{html}");
        assert!(
            html.contains("autofocus"),
            "the search box must take focus without JavaScript: {html}"
        );
    }

    /// What a reader types into the search box is what gets searched for — and
    /// with no JavaScript on this page, the only way it can arrive is as a
    /// browser-encoded form submission, where a space is `+`.
    ///
    /// The target is built out of the page's own form rather than written by
    /// hand, so a form that lost its token or dropped the filter in force fails
    /// here as well: this is the submission, not a URL that happens to work.
    #[test]
    fn a_search_typed_into_the_form_is_searched_for_as_typed() {
        let index = complete_index();
        let html = page("/search?machine=m-2&q=synthetic", &index).body;
        let target = browser_submission(&html, "q", "hello world");
        assert!(
            target.contains("q=hello+world"),
            "the fixture must encode the space the way a browser does: {target}"
        );
        let (path, params) = crate::ui::split_target(&target);
        crate::ui::handle(
            path,
            &params,
            "t",
            &fixture::data(),
            &crate::ui::NoContent,
            &index,
        )
        .unwrap_or_else(|| panic!("`{target}` must be a known route"));
        let calls = index.calls.borrow();
        assert!(
            calls.iter().any(|call| call == "query:hello world"),
            "a space typed into the box must be searched for as a space: {calls:?}"
        );
        assert!(
            !calls.iter().any(|call| call == "query:hello+world"),
            "the browser's `+` must not reach the index as itself: {calls:?}"
        );
    }

    /// The GET target a browser would send for the form `html` prints, with
    /// `field` set to `value`.
    ///
    /// Fields are read out of the form element this test selects by its action,
    /// in the order it prints them, and encoded as
    /// `application/x-www-form-urlencoded` — a space as `+`, everything outside
    /// the unreserved set percent-encoded. Attribute values are read in both
    /// spellings the page uses (`name=token` and `name="machine"`).
    fn browser_submission(html: &str, field: &str, value: &str) -> String {
        let form = html
            .split("<form ")
            .filter_map(|rest| rest.split_once("</form>").map(|(form, _)| form))
            .find(|form| form.contains("action=\"/search\""))
            .unwrap_or_else(|| panic!("the page must print the search form: {html}"));
        let action = attribute(form, "action")
            .unwrap_or_else(|| panic!("the search form must name an action: {form}"));
        let mut fields: Vec<(String, String)> = Vec::new();
        for tag in form.split('<').filter(|tag| tag.starts_with("input ")) {
            let tag = tag.split('>').next().unwrap_or(tag);
            let name = attribute(tag, "name")
                .unwrap_or_else(|| panic!("every input in the form is named: {tag}"));
            fields.push((name, attribute(tag, "value").unwrap_or_default()));
        }
        match fields.iter_mut().find(|(name, _)| name == field) {
            Some(slot) => slot.1 = value.to_string(),
            None => fields.push((field.to_string(), value.to_string())),
        }
        let query = fields
            .iter()
            .map(|(name, value)| format!("{}={}", form_encode(name), form_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        format!("{action}?{query}")
    }

    /// The value of `name=…` in one tag, in the quoted or the bare spelling.
    fn attribute(tag: &str, name: &str) -> Option<String> {
        let (_, rest) = tag.split_once(&format!("{name}="))?;
        match rest.strip_prefix('"') {
            Some(quoted) => Some(quoted.split_once('"')?.0.to_string()),
            None => Some(rest.split([' ', '>', '\n']).next()?.to_string()),
        }
    }

    /// `application/x-www-form-urlencoded`, which is what a browser sends and
    /// what this page's query string therefore is.
    fn form_encode(s: &str) -> String {
        s.chars()
            .map(|c| match c {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
                ' ' => "+".to_string(),
                other => other
                    .to_string()
                    .bytes()
                    .map(|byte| format!("%{byte:02X}"))
                    .collect(),
            })
            .collect()
    }

    /// A placement that could not be **read** is not a placement that could not be
    /// **made**: the first says the index went away mid-request, the second says
    /// the match has no message to jump to. Reporting the first as the second would
    /// put a plausible sentence on a page whose real answer is "this did not
    /// finish".
    #[test]
    fn a_failed_placement_read_is_not_reported_as_an_unplaceable_match() {
        let mut index = complete_index().with_hits(vec![StubHit::placed(
            ON_M1,
            "a \u{1}synthetic\u{2} one",
            3,
        )]);
        index.fail_placements = true;
        let html = page("/search?q=synthetic", &index).body;
        assert!(html.contains("The index could not answer."), "{html}");
        assert!(
            !html.contains("could not be located in the stored messages"),
            "a read failure must not be attributed to the document: {html}"
        );
        assert!(!html.contains("open the conversation"), "{html}");
    }
}
