//! overview — the `/` page: machines, the machine × source matrix, the weekly
//! heatmap and every list of sessions whose time is unknown.
//!
//! Every number here is measured off the rows in view; a machine with no
//! activity index is named as such, and "time unknown" is a list with reasons
//! rather than a zero.

use std::collections::{BTreeMap, BTreeSet};

use crate::activity::TimeSource;
use crate::overview::{Granularity, HeatmapAxis, OverviewRow};

use super::facets::{self, PlatformGroup};
use super::html::{
    completeness_banner, describe_selector, destinations_block, esc, fmt_age, fmt_bytes, fmt_unix,
    footer, head, launch_banner, machines_without_index_banner, merged_counts,
};
use super::{
    health_of, percent_encode, select, Health, UiData, UiSession, NO_HARNESS, STALE_AFTER_DAYS,
};

// ------------------------------------------------------------- overview page

pub(super) fn page_overview(data: &UiData, token: &str) -> String {
    let sel = select(&data.sessions, &data.launch);
    let in_view = sel.in_view();
    let mut out = head(&format!("chat-stasher · {}", data.destination_label));
    out.push_str(&format!(
        "<h1>chat-stasher</h1>\n<p class=sub>destination <b>{}</b> · {} snapshot(s) scanned of {} \
         in repository · this page rendered {} of the archive metadata only</p>\n",
        esc(&data.destination_label),
        data.snapshots_scanned,
        data.snapshots_in_repo,
        fmt_unix(data.now_unix)
    ));
    out.push_str(&launch_banner(data));
    out.push_str(&completeness_banner(data));
    out.push_str(&destinations_block(data));
    out.push_str(&machines_without_index_banner(data));

    let total_bytes: u64 = in_view.iter().map(|s| s.bytes).sum();
    let sources: BTreeSet<String> = in_view.iter().map(|s| s.source_label()).collect();
    let machines = data.machine_keys();
    let filtered = describe_selector(&data.launch).is_some();
    let sessions_word = if filtered {
        "sessions in view"
    } else {
        "sessions"
    };
    let bytes_word = if filtered {
        "bytes in view (shard sizes)"
    } else {
        "bytes (shard sizes)"
    };
    out.push_str(&format!(
        "<section class=stats>\
         <div class=stat><span class=v>{n}</span><span class=l>{sw}</span></div>\
         <div class=stat><span class=v>{b}</span><span class=l>{bw}</span></div>\
         <div class=stat><span class=v>{m}</span><span class=l>machines</span></div>\
         <div class=stat><span class=v>{h}</span><span class=l>sources</span></div>\
         </section>\n",
        n = in_view.len(),
        b = esc(&fmt_bytes(total_bytes)),
        m = machines.len(),
        h = sources.len(),
        sw = sessions_word,
        bw = bytes_word,
    ));
    // §4.8 / R10: with more than one destination the session stat above is the
    // **distinct** reading, and the raw one cannot be recovered from it — a
    // reader told "3 sessions" has no way to learn that two of the rows behind
    // it are copies. So the pair goes directly under the number it qualifies;
    // with a single destination it is empty, because there the two readings
    // are equal by construction.
    out.push_str(&merged_counts(data));
    if sel.not_matched > 0 || !sel.unplaced.is_empty() {
        out.push_str(&format!(
            "<p>{} session(s) were evaluated and <b>rejected</b> by the filter in force; \
             {} session(s) could not be evaluated at all and are listed below.</p>\n",
            sel.not_matched,
            sel.unplaced.len()
        ));
    }
    // OQ-2 (29-UI-DESIGN §4.1/§12, landed by W172): the empty page is a served
    // screen, never an exit. Only the archive that truly holds nothing gets
    // the absence sentence, and only a complete read may claim it — a read
    // with unreadable parts keeps the zero a floor, because "held nothing
    // that we could see" is not "held nothing". A launch filter that matched
    // everything away is neither: its zero is scoped to the view, which the
    // headline above already says.
    if in_view.is_empty() && data.archive_sessions == 0 {
        if data.complete() {
            out.push_str("<p>(this destination holds no sessions)</p>\n");
        } else {
            out.push_str(&format!(
                "<p>(this destination holds no sessions in the parts this read could reach — \
                 with {} part(s) it could not, 0 is a floor, not a proof that none \
                 exist)</p>\n",
                data.unreadable.len()
            ));
        }
    }

    out.push_str(&render_machines(&machines, &in_view, data, token));
    out.push_str(&render_matrix(&machines, &in_view, token));
    out.push_str(&render_heatmap(&in_view, data, token));
    out.push_str(&render_time_unknown(&in_view, token));
    out.push_str(&footer(data));
    out.push_str("</body></html>\n");
    out
}

pub(super) fn count_by_machine<'a>(rows: &[&'a UiSession], machine: &str) -> (usize, u64) {
    let mut n = 0usize;
    let mut bytes = 0u64;
    for s in rows {
        if s.machine == machine {
            n += 1;
            bytes += s.bytes;
        }
    }
    (n, bytes)
}

fn render_machines(
    machines: &[String],
    in_view: &[&UiSession],
    data: &UiData,
    token: &str,
) -> String {
    let mut out = String::from(
        "<section><h2>Machines</h2>\n<div class=scroll><table>\n\
         <thead><tr><th>machine</th><th>newest snapshot</th><th>health</th>\
         <th class=n>sessions</th><th class=n>bytes</th><th>activity index</th></tr></thead>\n<tbody>\n",
    );
    for m in machines {
        let host = data.host(m);
        let health = health_of(host, data.now_unix);
        let (n, bytes) = count_by_machine(in_view, m);
        let snapshot = match host {
            Some(h) => format!(
                "{}<br><span class=mono>{}…</span>",
                esc(&fmt_unix(h.archive_time_unix)),
                esc(&h.snapshot_id.chars().take(8).collect::<String>())
            ),
            None => "<i>no snapshot</i>".to_string(),
        };
        let index = match (
            host.map(|h| h.has_activity_index),
            host.map(|h| h.index_read_ok),
        ) {
            (Some(true), Some(true)) => "<span class=ok>present, read</span>".to_string(),
            (Some(true), Some(false)) => "<span class=bad>present, UNREADABLE</span>".to_string(),
            (Some(false), _) => "<span class=bad>MISSING</span>".to_string(),
            _ => "<i>unknown</i>".to_string(),
        };
        let word = match health {
            Health::Archived { age_secs } => {
                format!(
                    "<span class=ok>{}</span> · {}",
                    health.word(),
                    fmt_age(age_secs)
                )
            }
            Health::Unknown => format!("<span class=bad>{}</span>", health.word()),
            _ => format!("<span class=bad>{}</span>", health.word()),
        };
        out.push_str(&format!(
            "<tr><td class=mono><a href=\"/sessions?machine={q}&token={t}\">{m}</a></td>\
             <td>{snapshot}</td><td>{word}</td><td class=n>{n}</td><td class=n>{b}</td>\
             <td>{index}</td></tr>\n",
            q = percent_encode(m),
            t = percent_encode(token),
            m = esc(m),
            b = esc(&fmt_bytes(bytes)),
        ));
    }
    if machines.is_empty() {
        out.push_str("<tr><td colspan=6>(this destination holds no snapshot at all)</td></tr>\n");
    }
    out.push_str("</tbody></table></div>\n");
    out.push_str(
        "<p class=sub>Health is one of four measured states — a snapshot age against \
         ",
    );
    out.push_str(&format!(
        "a {STALE_AFTER_DAYS}-day threshold, or an index that is missing or unreadable. \
         There is no health percentage: nothing here measures a denominator that would make \
         one mean anything.</p>\n</section>\n"
    ));
    out
}

/// Order the matrix's source columns by group (UIA-3, 29-UI-DESIGN §3.1):
/// web platforms, then coding agents, then the ungrouped bucket — each sorted
/// alphabetically within itself so the order is stable across launches — and
/// the no-harness column last, a *rowspan* header of its own rather than a
/// group member, because it is the absence of a classifiable source, not one
/// more group. `BTreeSet` iteration is already sorted, so grouping is a
/// three-way stable partition of it.
fn grouped_sources(
    sources: &BTreeSet<String>,
    no_harness: &str,
) -> Vec<(String, Option<PlatformGroup>)> {
    let mut out: Vec<(String, Option<PlatformGroup>)> = Vec::new();
    for group in [
        PlatformGroup::WebPlatforms,
        PlatformGroup::CodingAgents,
        PlatformGroup::Ungrouped,
    ] {
        out.extend(
            sources
                .iter()
                .filter(|s| s.as_str() != no_harness)
                .filter(|s| facets::group_of(s) == group)
                .map(|s| (s.clone(), Some(group))),
        );
    }
    if sources.contains(no_harness) {
        out.push((no_harness.to_string(), None));
    }
    out
}

/// The two header rows of the machine × source matrix: the group cells — one
/// `<th colspan=n>` per contiguous group run, coloured the group's colour —
/// above the per-source `<th>`s that carry the runs' sources. The machine and
/// total header cells span both rows, which is what keeps their meaning where
/// it already was; the no-harness column is a single `rowspan=2` cell, because
/// no group claims it and saying so twice would invent a label for it.
fn matrix_header(sources: &[(String, Option<PlatformGroup>)]) -> String {
    let mut runs: Vec<(PlatformGroup, usize)> = Vec::new();
    for (_, group) in sources.iter().filter_map(|(s, g)| {
        // The no-harness column is not part of any run; it renders as its own
        // spanning cell in the first row.
        if g.is_none() {
            None
        } else {
            Some((s, *g))
        }
    }) {
        let group = group.expect("the no-harness column was filtered out above");
        match runs.last_mut() {
            Some((last, n)) if *last == group => *n += 1,
            _ => runs.push((group, 1)),
        }
    }
    let mut top = String::from("<tr><th rowspan=2>machine</th>");
    for (group, n) in &runs {
        top.push_str(&format!(
            "<th colspan={n} class=\"ghead {}\">{}</th>",
            group.css(),
            group.label()
        ));
    }
    let mut bottom = String::from("<tr>");
    for (source, group) in sources {
        match group {
            Some(group) => bottom.push_str(&format!(
                "<th class=\"n {}\">{}</th>",
                group.css(),
                esc(source)
            )),
            None => {
                // The rowspan cell belongs in the *first* row, beside the
                // group cells, not the second: a cell spanning both header
                // rows is one header, not a group with one member.
                top.push_str(&format!("<th rowspan=2 class=n>{}</th>", esc(source)));
            }
        }
    }
    top.push_str("<th rowspan=2 class=n>total</th></tr>\n");
    bottom.push_str("</tr>\n");
    format!("<thead>{top}{bottom}</thead>\n<tbody>\n")
}

fn render_matrix(machines: &[String], in_view: &[&UiSession], token: &str) -> String {
    let mut sources: BTreeSet<String> = BTreeSet::new();
    for s in in_view {
        sources.insert(s.source_label());
    }
    let sources = grouped_sources(&sources, NO_HARNESS);
    let mut out = String::from(
        "<section><h2>Machine × source</h2>\n<p class=sub>Session counts. A cell links to that \
         machine and source; the row label links to the whole machine. Columns are grouped by \
         the facet bar's platform groups (web platforms, coding agents, then any source this \
         build does not classify).</p>\n\
         <div class=scroll><table>\n",
    );
    out.push_str(&matrix_header(&sources));

    for m in machines {
        out.push_str(&format!(
            "<tr><td class=mono><a href=\"/sessions?machine={q}&token={t}\">{m}</a></td>",
            q = percent_encode(m),
            t = percent_encode(token),
            m = esc(m),
        ));
        let mut row_total = 0usize;
        for (h, group) in &sources {
            let n = in_view
                .iter()
                .filter(|s| s.machine == *m && s.source_label() == *h)
                .count();
            row_total += n;
            if n == 0 {
                out.push_str("<td class=n>·</td>");
            } else if group.is_none() {
                // Not expressible as a `--harness` filter: the shared selector's
                // harness constraint needs a harness to compare against, and
                // these ids have none. Linking to something that would return a
                // *different* set is worse than not linking at all, so the cell
                // says why and the sessions are listed below.
                out.push_str(&format!(
                    "<td class=n title=\"not expressible as a harness filter — see the list \
                     below\">{n}</td>"
                ));
            } else if !facets::is_expressible_as_filter_value(h) {
                // A harness containing the value-list separator cannot be
                // named by any `--harness` filter — the grammar would split
                // it into ids that do not exist, so the link would return a
                // different set than the count promises. Same rule as the
                // no-prefix cells: a count that says why, never a lying link.
                out.push_str(&format!(
                    "<td class=n title=\"this source's id contains the harness list separator, \
                     so no harness filter can select it\">{n}</td>"
                ));
            } else {
                out.push_str(&format!(
                    "<td class=n><a href=\"/sessions?machine={qm}&harness={qh}&token={t}\">{n}</a></td>",
                    qm = percent_encode(m),
                    qh = percent_encode(h),
                    t = percent_encode(token),
                ));
            }
        }
        out.push_str(&format!("<td class=n><b>{row_total}</b></td></tr>\n"));
    }
    out.push_str("</tbody></table></div>\n");
    // Two independent reasons a cell is a count rather than a link, each
    // said only when its column is on the page — a note about a column this
    // table does not hold would be a claim about nothing.
    if sources.iter().any(|(_, group)| group.is_none()) {
        out.push_str(&format!(
            "<p class=sub>Sessions counted under <code>{}</code> cannot be selected with \
             <code>--harness</code>: the archived id carries no harness prefix, so the filter \
             has nothing to compare. Those cells are counts, not links, and the sessions are \
             listed below.</p>\n",
            esc(NO_HARNESS)
        ));
    }
    let has_inexpressible = sources
        .iter()
        .any(|(source, group)| group.is_some() && !facets::is_expressible_as_filter_value(source));
    if has_inexpressible {
        out.push_str(
            "<p class=sub>A source whose id contains the harness list separator cannot be \
             named by any <code>--harness</code> filter — the filter would split it into ids \
             that do not exist. Those cells are counts, not links.</p>\n",
        );
    }
    out.push_str("</section>\n");
    out
}

/// The machines R9 exists for: in [`UiData::machines_without_index`] — the
/// same list `machines_without_activity_index` carries in `/api/overview` —
/// and holding at least one in-view session whose conversation time is
/// unknown, which is the shape the heatmap draws as a whole row of `?`.
///
/// A machine in the no-index list with no in-view session (filtered away, or
/// this page's launch filter matched another machine) is not drawn here at
/// all; the banner above still names it, exactly as the JSON field does.
fn no_index_machines_in_view(in_view: &[&UiSession], data: &UiData) -> Vec<String> {
    data.machines_without_index
        .iter()
        .filter(|m| {
            in_view.iter().any(|s| {
                &s.machine == *m
                    && !s.has_known_time()
                    && !s.time_source.is_no_conversation_content()
            })
        })
        .cloned()
        .collect()
}

/// R9 (29-UI-DESIGN §3.1): the heatmap's reason line — one per no-index
/// machine the table actually shows a row for. Same vocabulary as the `search`
/// hint line ("no activity index for machine `X` — its sessions cannot be
/// placed in time"), and it names the repair command the way the list page's
/// legacy-label note does.
fn no_index_reason_lines(machines: &[String]) -> String {
    machines
        .iter()
        .map(|m| {
            format!(
                "<p class=sub>Machine <span class=mono>{m}</span> has no activity index, so \
                 its sessions cannot be placed in time: every week cell in its row is \
                 <i>UNKNOWN</i>, not empty. Run <code>chat-stasher activity-index</code> on \
                 that machine to record one.</p>\n",
                m = esc(m)
            )
        })
        .collect()
}

fn render_heatmap(in_view: &[&UiSession], data: &UiData, token: &str) -> String {
    let rows: Vec<OverviewRow> = in_view.iter().map(|s| s.overview_row()).collect();
    let axis = HeatmapAxis::Machine;
    // Weekly, always: the dashboard is read at a weekly cadence, so a day column
    // would show mostly-empty columns and a hundred of them.
    let hd = crate::overview::heatmap_data(&rows, 80, axis, Some(Granularity::Week));
    let no_index = no_index_machines_in_view(in_view, data);
    let mut out = String::from("<section><h2>Activity, by week</h2>\n");
    if hd.rows.is_empty() {
        out.push_str("<p>(no sessions in view)</p></section>\n");
        return out;
    }
    if !hd.has_time_axis {
        out.push_str(
            "<p>No session in view has a known conversation time, so there is no time axis \
             to draw. The weekly columns are UNKNOWN, not empty.</p>\n",
        );
        out.push_str(&no_index_reason_lines(&no_index));
        out.push_str("</section>\n");
        return out;
    }
    out.push_str(
        "<p class=sub>One column per week (UTC, Monday). Density uses the same ramp as \
         <code>overview</code>. A cell links to the sessions whose conversation interval \
         <i>intersects</i> that week — the archive's time filter compares intervals, so a \
         conversation spanning several weeks appears in each of them.</p>\n\
         <div class=scroll><table class=heat>\n<thead><tr><th></th>",
    );
    for b in &hd.buckets {
        out.push_str(&format!(
            "<th class=week title=\"{}\">{}</th>",
            esc(&format!("{}..{}", b.label, b.last_day)),
            esc(&b.label)
        ));
    }
    out.push_str("<th class=week>?</th></tr></thead>\n<tbody>\n");
    let max = hd.max_count;
    for r in &hd.rows {
        out.push_str(&format!(
            "<tr><td class=mono><a href=\"/sessions?machine={q}&token={t}\">{m}</a></td>",
            q = percent_encode(&r.label),
            t = percent_encode(token),
            m = esc(&r.label),
        ));
        // R9: a machine listed in `machines_without_index` had its row's weeks
        // never measured, and an unmeasured week must not read — or link — as
        // an empty one. Its cells carry the unknown marker with the reason in
        // the title, and no time-filter link: a link promises "the sessions in
        // this week", and no week of this row was ever measured.
        let no_index_row = no_index.iter().any(|m| m == &r.label);
        for b in &hd.buckets {
            if no_index_row {
                out.push_str(&format!(
                    "<td class=cell><span title=\"{}\">{}</span></td>",
                    esc(&format!(
                        "{}..{}: UNKNOWN — no activity index for this machine",
                        b.label, b.last_day
                    )),
                    esc(&crate::overview::heatmap_unknown_char().to_string())
                ));
                continue;
            }
            // reason: the horizontal axis spans the full activity range, so a
            // bucket with no sessions for this machine is a true empty bucket;
            // 0 is that measurement, not a fallback.
            let n = r.counts.get(&b.label).copied().unwrap_or(0);
            let ch = crate::overview::heatmap_cell_char(n, max);
            if n == 0 {
                out.push_str(&format!(
                    "<td class=cell><span title=\"{}\">{}</span></td>",
                    esc(&format!("{}..{}: no sessions", b.label, b.last_day)),
                    esc(&ch.to_string())
                ));
            } else {
                out.push_str(&format!(
                    "<td class=cell><a title=\"{}\" href=\"/sessions?machine={qm}&since={s}&until={u}&token={t}\">{ch}</a></td>",
                    esc(&format!("{}..{}: {n} session(s)", b.label, b.last_day)),
                    qm = percent_encode(&r.label),
                    s = percent_encode(&b.label),
                    u = percent_encode(&b.last_day),
                    t = percent_encode(token),
                    ch = esc(&ch.to_string()),
                ));
            }
        }
        let uk = if r.unknown > 0 {
            format!("<span class=bad>{}</span>", r.unknown)
        } else {
            "<span class=sub>·</span>".to_string()
        };
        let uk_title = if no_index_row {
            "time unknown — no activity index for this machine"
        } else {
            "time unknown"
        };
        out.push_str(&format!(
            "<td class=uk title=\"{uk_title}\">{uk}</td></tr>\n"
        ));
    }
    out.push_str("</tbody></table></div>\n");
    out.push_str(&no_index_reason_lines(&no_index));
    out.push_str(&format!(
        "<p class=sub>Last column: sessions with no known time, never merged into a week. \
         Ramp {} (fewest) to {} (most). Week boundaries are UTC days; the link's filter is a \
         local calendar day, so a session within hours of a week edge can fall in a different \
         column than the one you clicked.</p>\n",
        esc(&crate::overview::heatmap_ramp().0.to_string()),
        esc(&crate::overview::heatmap_ramp().1.to_string()),
    ));
    out.push_str("</section>\n");
    out
}

fn render_time_unknown(in_view: &[&UiSession], token: &str) -> String {
    // ADR-035: sessions with no conversation content are a different claim from
    // "a conversation whose time we could not find", so they are counted in
    // their own list below rather than folded in here.
    let no_content: Vec<&&UiSession> = in_view
        .iter()
        .filter(|s| s.time_source.is_no_conversation_content())
        .collect();
    let unknown: Vec<&&UiSession> = in_view
        .iter()
        .filter(|s| !s.has_known_time() && !s.time_source.is_no_conversation_content())
        .collect();
    let mut pending_append = String::new();
    if unknown.is_empty() {
        if no_content.is_empty() {
            pending_append.push_str(
                "<section><h2>Time unknown</h2>\n<p class=ok>0 sessions — every session in view \
                 has a recorded conversation time.</p></section>\n",
            );
        } else {
            pending_append.push_str(
                "<section><h2>Time unknown</h2>\n<p class=ok>0 sessions with conversation content \
                 have an unknown conversation time.</p></section>\n",
            );
            pending_append.push_str(&format!(
                "<section><h2>No conversation content</h2>\n<p>{} session(s) in view were archived \
                 with no user or assistant message — an empty shard or metadata-only lines. They are \
                 not in any time bucket, and are not counted as time-unknown.</p></section>\n",
                no_content.len()
            ));
        }
        return pending_append;
    }
    let mut by_reason: BTreeMap<String, Vec<&&UiSession>> = BTreeMap::new();
    for s in &unknown {
        let why = match &s.time_source {
            TimeSource::Unknown { why } => why.clone(),
            // A range that is only part of the span: it has measured bounds, so
            // "no time" is the wrong reason — the variant's own reason stands.
            TimeSource::PartialRange { why, .. } => why.clone(),
            // A session whose source attests a time but carries no bound. Named
            // as its own case rather than folded into a reason it does not have.
            _ => "the activity index attests a time but recorded no bound for it".to_string(),
        };
        by_reason.entry(why).or_default().push(s);
    }
    pending_append.push_str(&format!(
        "<section><h2>Time unknown</h2>\n<p>{} session(s) in view have no conversation time \
         that could be <b>fully</b> placed — either none was recorded, or only part of the span \
         could be read. They are <b>not</b> in any bucket above, and they are not \"no sessions\" \
         — each carries its own reason.</p>\n",
        unknown.len()
    ));
    for (why, rows) in &by_reason {
        pending_append.push_str(&format!(
            "<h3 class=sub>{} session(s): {}</h3>\n<div class=scroll><table>\n\
             <thead><tr><th>machine</th><th>source</th><th>session</th><th class=n>shards</th>\
             <th class=n>bytes</th><th>snapshot time</th></tr></thead>\n<tbody>\n",
            rows.len(),
            esc(why)
        ));
        for s in rows {
            pending_append.push_str(&session_row_html(s, token));
        }
        pending_append.push_str("</tbody></table></div>\n");
    }
    pending_append.push_str("</section>\n");
    if !no_content.is_empty() {
        pending_append.push_str(&format!(
            "<section><h2>No conversation content</h2>\n<p>{} session(s) in view were archived \
             with no user or assistant message — an empty shard or metadata-only lines. They are \
             not in any time bucket, and are not counted as time-unknown.</p></section>\n",
            no_content.len()
        ));
    }
    pending_append
}

fn session_row_html(s: &UiSession, token: &str) -> String {
    format!(
        "<tr><td class=mono>{m}</td><td>{h}</td>\
         <td><a class=mono href=\"/session?i={i}&token={t}\">{sid}</a></td>\
         <td class=n>{sh}</td><td class=n>{b}</td><td>{snap}</td></tr>\n",
        m = esc(&s.machine),
        h = esc(&s.source_label()),
        i = s.index,
        t = percent_encode(token),
        sid = esc(&s.short_id),
        sh = s.shard_count,
        b = esc(&fmt_bytes(s.bytes)),
        snap = esc(&fmt_unix(s.archive_time_unix)),
    )
}
