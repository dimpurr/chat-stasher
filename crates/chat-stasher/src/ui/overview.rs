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
    let mut out = head(&format!("chat-stasher · {}", data.destination_label), token);
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
    // W219 · the headline total is the **conversation** count: distinct archive
    // ids in view. One conversation archived on a laptop and a desktop is one
    // conversation and two rows, and a page that added the two rows would
    // inflate the archive by exactly the redundancy the backup provides. The row
    // reading is printed under this number by `machine_axis_counts`, never
    // dropped — the same distinct/raw pair `merged_counts` keeps for the
    // destination axis.
    //
    // It is `UiData`'s own count rather than a second one taken here: the
    // sentences under it read the same field, and two counts of one thing are
    // how a page ends up disagreeing with itself.
    let conversations_in_view = data.conversations;
    let sessions_word = if filtered {
        "conversations in view"
    } else {
        "conversations"
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
        n = conversations_in_view,
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
    // W219 · the machine axis, stated beside the destination one. Two different
    // questions ("how many backup copies" vs "how many machines hold this
    // conversation") with two different answers, so neither is derived from the
    // other and both reach the page.
    out.push_str(&super::html::machine_axis_counts(data));
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

// ---------------------------------------------------------- extensions page

/// What a row calls an install whose record does not carry a field. Spelled out
/// rather than left blank: an empty cell is how "we could not read it" gets
/// read as "there was nothing there", and the two are not the same claim.
const BROWSER_UNKNOWN: &str = "Unknown browser";
const PROFILE_UNNAMED: &str = "Unnamed profile";
const MACHINE_UNKNOWN: &str = "Machine unknown";

/// What the two numbers in a platform column are. The column header is the one
/// place a reader finds out which count is which, and a pair of bare integers
/// says neither: `4 · 2` is as easily one ratio as two counts.
const CAPTURED_PENDING: &str = "captured · pending";

/// A platform the install's report carries no row for. Absent — which is neither
/// a zero nor an unknown, and is told in words in the cell's `title` because the
/// visible mark for it is an em dash.
const CELL_NO_ROW: &str = "no row in the report from this install";

/// `1`/`n` agreement. The page pluralizes two words and both follow the regular
/// rule, so this stays a suffix rather than a table of irregulars.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// What a row asks of its reader, worst first. The declaration order **is** the
/// sort order — sorting by this enum is what puts the install that stopped
/// above the ones that are reporting.
///
/// A stale install outranks a paused one, because a pause is the extension
/// saying what it is doing and going quiet is it saying nothing; an unreadable
/// status is placed with the two that need a look rather than among the healthy
/// ones, because "we could not read it" is not "it is fine".
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Attention {
    Stale,
    Paused,
    Unknown,
    Reporting,
}

impl Attention {
    /// The word beside the dot. `<b>Stale</b>` used to be a badge of its own;
    /// it is now the row's status, which is the one place a reader looks.
    fn word(self) -> &'static str {
        match self {
            Attention::Stale => "Stale",
            Attention::Paused => "Paused",
            Attention::Unknown => "Unknown",
            Attention::Reporting => "Reporting",
        }
    }

    /// The dot's modifier. A word always accompanies it — the dot is the second
    /// reading of the status, never the only one.
    fn dot(self) -> &'static str {
        match self {
            Attention::Stale => "stale",
            Attention::Paused => "paused",
            Attention::Unknown => "unknown",
            Attention::Reporting => "ok",
        }
    }
}

/// One platform's reading inside one install's report.
struct PlatformCell {
    captured: Option<u64>,
    pending: Option<u64>,
    /// `Some(reason)` when the row says this platform is paused.
    paused: Option<String>,
    /// True when the row carried no `paused_reason` key at all, so the pause
    /// state is unknown. A `null` value is the report saying "not paused",
    /// which is a different statement and is not folded into this one.
    pause_unknown: bool,
}

/// One archived install, read once and rendered twice: as a row of its
/// machine's table and as a line in that table's `<details>`.
struct InstallView<'a> {
    install_id: Option<&'a str>,
    /// `None` when the record names no machine. It is kept as `None` rather
    /// than defaulted to a label so the page can say which of the two it has.
    machine: Option<&'a str>,
    browser: Option<&'a str>,
    profile: Option<&'a str>,
    extension_version: Option<&'a str>,
    reported_unix: Option<i64>,
    platforms: BTreeMap<String, PlatformCell>,
    attention: Attention,
}

impl<'a> InstallView<'a> {
    fn of(install: &'a serde_json::Value) -> Self {
        let mut platforms: BTreeMap<String, PlatformCell> = BTreeMap::new();
        if let Some(rows) = install
            .get("platforms")
            .and_then(serde_json::Value::as_array)
        {
            for row in rows {
                let Some(platform) = row.get("platform").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let cell = platforms
                    .entry(platform.to_owned())
                    .or_insert(PlatformCell {
                        captured: Some(0),
                        pending: Some(0),
                        paused: None,
                        pause_unknown: false,
                    });
                // Two rows for one platform in one install are that install's
                // own account rows, so folding them is a count of one install —
                // not the cross-install sum the topology forbids. The fold
                // starts at zero *for a row that exists*; an absent count
                // stays absent, because `Some(0).zip(None)` is `None`.
                cell.captured = cell
                    .captured
                    .zip(
                        row.get("captured_by_this_browser")
                            .and_then(serde_json::Value::as_u64),
                    )
                    .and_then(|(total, count)| total.checked_add(count));
                cell.pending = cell
                    .pending
                    .zip(row.get("pending").and_then(serde_json::Value::as_u64))
                    .and_then(|(total, count)| total.checked_add(count));
                match row.get("paused_reason") {
                    Some(serde_json::Value::Null) => {}
                    Some(serde_json::Value::String(reason)) => {
                        cell.paused = Some(reason.clone());
                    }
                    _ => cell.pause_unknown = true,
                }
            }
        }
        let attention = if platforms.values().any(|cell| cell.paused.is_some()) {
            Attention::Paused
        } else if platforms.values().any(|cell| cell.pause_unknown) {
            Attention::Unknown
        } else {
            match install.get("stale").and_then(serde_json::Value::as_bool) {
                Some(true) => Attention::Stale,
                Some(false) => Attention::Reporting,
                None => Attention::Unknown,
            }
        };
        InstallView {
            install_id: install
                .get("install_id")
                .and_then(serde_json::Value::as_str),
            machine: install.get("machine").and_then(serde_json::Value::as_str),
            browser: install.get("browser").and_then(serde_json::Value::as_str),
            profile: install
                .get("profile_label")
                .and_then(serde_json::Value::as_str),
            extension_version: install
                .get("extension_version")
                .and_then(serde_json::Value::as_str),
            reported_unix: install
                .get("reported_at")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.timestamp()),
            platforms,
            attention,
        }
    }

    fn label(&self) -> String {
        format!(
            "{} · {}",
            esc(self.browser.unwrap_or(BROWSER_UNKNOWN)),
            esc(self.profile.unwrap_or(PROFILE_UNNAMED))
        )
    }

    /// One platform cell: this install's own `captured · pending`, plus the
    /// pause word when that platform is paused. A platform this install's report
    /// carries no row for is an em dash — absent, which is neither a zero nor an
    /// unknown.
    ///
    /// The visible pair is two integers and a separator, and the column header
    /// names them for a reader who can see it — so the cell carries the same
    /// reading in its `title`, for the reader who cannot. It is the same three
    /// parts either way (the platform, both counts, this install's pause state),
    /// because a title that dropped one would describe a different cell from the
    /// one it is attached to.
    fn cell(&self, platform: &str) -> String {
        let Some(cell) = self.platforms.get(platform) else {
            return format!(
                "<td class=n title=\"{}\">—</td>",
                esc(&format!("{platform}: {CELL_NO_ROW}"))
            );
        };
        let count = |value: Option<u64>| match value {
            Some(n) => n.to_string(),
            None => "Unknown".to_string(),
        };
        let mut out = format!(
            "<td class=n title=\"{}\">{} · {}",
            esc(&cell_title(platform, cell)),
            count(cell.captured),
            count(cell.pending)
        );
        if cell.paused.is_some() {
            out.push_str(" <span class=paused>paused</span>");
        } else if cell.pause_unknown {
            out.push_str(" <span class=unknown>pause unknown</span>");
        }
        out.push_str("</td>");
        out
    }
}

/// One cell's reading, in words, for a reader who cannot see the column it sits
/// under (the `title` attribute: a tooltip on hover and the description a screen
/// reader reads with the cell).
///
/// The counts are named one by one rather than printed as a pair, because they
/// are the two different claims the column header exists to tell apart, and an
/// omitted count stays the word `Unknown` rather than a blank — the same rule the
/// visible cell follows.
fn cell_title(platform: &str, cell: &PlatformCell) -> String {
    let count = |value: Option<u64>, word: &str| match value {
        Some(n) => format!("{word} {n}"),
        None => format!("{word} unknown"),
    };
    let mut out = format!(
        "{platform}: {}, {}",
        count(cell.captured, "captured"),
        count(cell.pending, "pending")
    );
    if let Some(reason) = &cell.paused {
        out.push_str(&format!(", paused ({reason})"));
    } else if cell.pause_unknown {
        out.push_str(", pause unknown");
    }
    out
}

/// Render archived extension reports as one summary sentence and one compact
/// table per machine.
///
/// The shape is forced by the topology rather than by taste: one user is several
/// machines × several browsers × several profiles × several installs
/// (`36-EXTENSION-TOPOLOGY.md` §1), so a page that describes each install at
/// length grows without bound and buries the one install that needs a look.
/// Three rules the layout is built around:
///
/// · A count belongs to one install and is never added across installs. The
///   summary counts *installs*, a platform cell is that install's own pair, and
///   the collapsed lines carry the detail the table has no room for.
/// · Stale, paused and unreadable are three states and read as three words.
///   None of them is drawn as a zero, and the ones that need a look sort above
///   the ones that do not, at both levels.
/// · Only an install on *this* machine can be opened, and only through a browser
///   profile this machine was verified to have (§4, principle 5). Every other
///   install says which machine to open it on instead.
pub(super) fn page_extensions(data: &UiData, token: &str) -> String {
    let mut out = head("chat-stasher · Extensions", token);
    out.push_str(
        "<h1>Extensions</h1>\n<p class=sub>Archived reports, one row per install. One user \
         is several machines × several browsers × several profiles, so each install is \
         listed on its own and no count here is added across them.</p>\n",
    );
    let views = data
        .extension_installs
        .iter()
        .map(InstallView::of)
        .collect::<Vec<_>>();
    if !data.extension_status_read {
        out.push_str(
            "<p class=message>Extension reports could not be read completely. The list may \
             be incomplete.</p>\n",
        );
    } else if views.is_empty() {
        out.push_str(
            "<p>No extension status reports are present in the readable archive snapshots.</p>\n",
        );
    }
    if views.is_empty() {
        out.push_str(&footer(data));
        out.push_str("</body></html>\n");
        return out;
    }
    out.push_str(&extension_summary(&views, data.extension_status_read));

    // Grouped by machine. Rows sort by attention, and so do the machines: a
    // reader who opens this page is looking for the install that stopped, and a
    // stale install on machine 3 must not sit below the fold because machines
    // are listed alphabetically.
    let mut groups: BTreeMap<Option<&str>, Vec<&InstallView>> = BTreeMap::new();
    for view in &views {
        groups.entry(view.machine).or_default().push(view);
    }
    let mut groups = groups.into_iter().collect::<Vec<_>>();
    for (_, rows) in groups.iter_mut() {
        rows.sort_by(|a, b| {
            a.attention
                .cmp(&b.attention)
                .then_with(|| a.browser.cmp(&b.browser))
                .then_with(|| a.profile.cmp(&b.profile))
        });
    }
    groups.sort_by(|(a, a_rows), (b, b_rows)| {
        worst_attention(a_rows)
            .cmp(&worst_attention(b_rows))
            .then_with(|| {
                a.unwrap_or(MACHINE_UNKNOWN)
                    .cmp(b.unwrap_or(MACHINE_UNKNOWN))
            })
    });

    let local = data.local_machine_id.as_deref();
    for (machine, rows) in &groups {
        let here = local.is_some() && local == *machine;
        out.push_str(&format!(
            "<section><h2>{}{}</h2>\n<p class=sub>{}</p>\n",
            esc(machine.unwrap_or(MACHINE_UNKNOWN)),
            if here {
                " <span class=h2note>· this machine</span>"
            } else {
                ""
            },
            extension_counts(rows),
        ));
        out.push_str(&open_guidance(local, *machine));
        out.push_str(&extension_table(rows, here, data, token));
        out.push_str(&extension_details(rows, data.now_unix));
        out.push_str("</section>\n");
    }
    out.push_str(&footer(data));
    out.push_str("</body></html>\n");
    out
}

/// The worst state any of the machine's installs is in — the machine's own
/// reason to be looked at.
///
/// A minimum over the rows rather than the attention of whichever row happens
/// to be first: reading it off the first row would make the machine's rank a
/// consequence of the row sort, so a change to one would silently redefine the
/// other. (It did: a mutation that stopped sorting rows by attention left the
/// machine order looking right for the wrong reason.)
fn worst_attention(rows: &[&InstallView]) -> Option<Attention> {
    rows.iter().map(|view| view.attention).min()
}

/// The one sentence at the top: how many installs, on how many machines, and
/// how many of them need a look.
///
/// An incomplete read keeps its counts as floors — "at least N", and the clause
/// that says so — because a count taken off a partial scan is not a total, and a
/// page that shows one anyway is the collapse of "we do not know" into "we
/// checked" that the whole tool exists to avoid.
fn extension_summary(views: &[InstallView], complete: bool) -> String {
    let machines = views
        .iter()
        .map(|view| view.machine)
        .collect::<BTreeSet<_>>()
        .len();
    let n = views.len();
    format!(
        "<p class=sub>{}{} install{} on {} machine{}{}{}</p>\n",
        if complete { "" } else { "At least " },
        n,
        plural(n),
        machines,
        plural(machines),
        attention_counts(views.iter().map(|view| view.attention)),
        if complete {
            ""
        } else {
            " · each count is a floor, not a total"
        },
    )
}

/// One machine's own line: how many installs it holds and how many need a look.
/// Counts are of installs, never of captures — the same rule the page summary
/// follows, applied one level down.
fn extension_counts(rows: &[&InstallView]) -> String {
    format!(
        "{} install{}{}",
        rows.len(),
        plural(rows.len()),
        attention_counts(rows.iter().map(|view| view.attention))
    )
}

/// `· 1 stale · 2 paused` and so on, or the sentence that says there are none.
/// A count of installs, and only of installs.
fn attention_counts(attentions: impl Iterator<Item = Attention>) -> String {
    let mut counts: BTreeMap<Attention, usize> = BTreeMap::new();
    for attention in attentions {
        *counts.entry(attention).or_insert(0) += 1;
    }
    let mut out = String::new();
    for (attention, word) in [
        (Attention::Stale, "stale"),
        (Attention::Paused, "paused"),
        (Attention::Unknown, "with an unreadable status"),
    ] {
        if let Some(n) = counts.get(&attention).copied().filter(|n| *n > 0) {
            out.push_str(&format!(" · {n} {word}"));
        }
    }
    if out.is_empty() {
        out.push_str(" · none stale, paused or unreadable");
    }
    out
}

/// Who can be opened from here. Only this machine's installs get an action — the
/// dashboard on machine A cannot open a profile in machine B's browser, and a
/// control that looks like it can is worse than the sentence that says where to
/// go instead (topology principle 5).
///
/// A paragraph of its own rather than a clause appended to the count line: it is
/// about a different subject ("these installs are elsewhere") from the counts,
/// and run together the two read as one claim about the same rows. The local
/// machine gets nothing here, because its table carries the action instead.
fn open_guidance(local: Option<&str>, machine: Option<&str>) -> String {
    match (local, machine) {
        (_, None) => {
            "<p class=sub>The record names no machine, so these profiles cannot be opened \
             from here.</p>\n"
                .to_string()
        }
        (Some(local), Some(machine)) if local == machine => String::new(),
        (Some(_), Some(machine)) => format!(
            "<p class=sub>These installs are on {}; open them there.</p>\n",
            esc(machine)
        ),
        (None, Some(_)) => {
            "<p class=sub>This machine's own identity is unavailable, so no profile is \
             offered to open.</p>\n"
                .to_string()
        }
    }
}

/// The machine's install table: one row per install, one column per platform the
/// machine has any install for.
///
/// The cell is that install's own pair and never a total, and the columns are
/// what a reader compares *down* — which install stopped, and on which platform
/// it is behind. `local` decides whether the open column exists at all, so a
/// remote machine's table cannot show an action that would be a lie.
///
/// Every header is `scope=col` (§8.1), and a platform column carries the word
/// for each of its two counts on a second line rather than in a `title` alone: a
/// reader who never hovers any cell still has to be told what the pair is, and the
/// label is emitted from *this* table's own platform set, so it is the same set of
/// columns the reader is counting.
fn extension_table(rows: &[&InstallView], local: bool, data: &UiData, token: &str) -> String {
    let platforms = rows
        .iter()
        .flat_map(|view| view.platforms.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut out = String::from(
        "<div class=scroll><table>\n<thead><tr><th scope=col>browser · profile</th>\
         <th scope=col>last report</th><th scope=col>status</th>",
    );
    if local {
        out.push_str("<th scope=col>open</th>");
    }
    for platform in &platforms {
        // The space before the span is the header's own separator, not layout:
        // the span is a block, so the space collapses at the end of the first
        // line, and what a screen reader reads is `chatgpt captured · pending`
        // rather than the run-together name the two elements would otherwise
        // compute to. It is one text either way — there is no second source to
        // drift from the visible label.
        out.push_str(&format!(
            "<th class=n scope=col>{} <span class=unit>{CAPTURED_PENDING}</span></th>",
            esc(platform)
        ));
    }
    out.push_str("</tr></thead>\n<tbody>\n");
    let mut unmatched = 0usize;
    for view in rows {
        out.push_str(&format!(
            "<tr class=\"install {}\"><td>{}</td><td>{}</td><td><span class=\"dot {}\"></span>{}</td>",
            view.attention.dot(),
            view.label(),
            esc(&fmt_report_age(data.now_unix, view.reported_unix)),
            view.attention.dot(),
            view.attention.word(),
        ));
        if local {
            match view
                .install_id
                .filter(|id| data.extension_open_targets.contains_key(*id))
            {
                Some(id) => out.push_str(&format!(
                    "<td><a href=\"/open-extension?install={}&amp;token={}\">Open in {} · {}</a></td>",
                    super::percent_encode(id),
                    super::percent_encode(token),
                    esc(view.browser.unwrap_or(BROWSER_UNKNOWN)),
                    esc(view.profile.unwrap_or(PROFILE_UNNAMED)),
                )),
                None => {
                    unmatched += 1;
                    out.push_str("<td><span class=muted>no match</span></td>");
                }
            }
        }
        for platform in &platforms {
            out.push_str(&view.cell(platform));
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</tbody></table></div>\n");
    if local && unmatched > 0 {
        out.push_str(&format!(
            "<p class=sub>An exact browser profile match is unavailable for {unmatched} of \
             these {} installs, so {} no open action; open {} from the named browser \
             profile.</p>\n",
            rows.len(),
            if unmatched == 1 {
                "that row has"
            } else {
                "those rows have"
            },
            if unmatched == 1 { "it" } else { "them" },
        ));
    }
    out
}

/// The rest of it, collapsed: the absolute instant behind the relative one, the
/// extension version the install reported, and why a paused platform is paused.
///
/// A list rather than a second table, because a details block that repeats the
/// table above it is the length this page exists to lose. The pause reasons live
/// here and not in the cell: the cell has room for the word, the reason is what
/// a reader opens the block for.
fn extension_details(rows: &[&InstallView], now_unix: i64) -> String {
    let mut out = format!(
        "<details><summary>Details for these {} install{}</summary>\n<ul>\n",
        rows.len(),
        plural(rows.len())
    );
    for view in rows {
        out.push_str(&format!("<li><b>{}</b> — last report ", view.label()));
        match view.reported_unix {
            Some(unix) => out.push_str(&format!(
                "{} ({})",
                esc(&fmt_unix(unix)),
                esc(&fmt_report_age(now_unix, view.reported_unix))
            )),
            None => out.push_str("Unknown"),
        }
        match view.extension_version {
            Some(version) => out.push_str(&format!(" · extension {}", esc(version))),
            None => out.push_str(" · extension version not recorded"),
        }
        for (platform, cell) in &view.platforms {
            if let Some(reason) = &cell.paused {
                out.push_str(&format!(" · {} paused ({})", esc(platform), esc(reason)));
            } else if cell.pause_unknown {
                out.push_str(&format!(" · {} pause state unknown", esc(platform)));
            }
        }
        out.push_str("</li>\n");
    }
    out.push_str("</ul></details>\n");
    out
}

/// A report's age, finer than [`fmt_age`]: that one rounds anything under an
/// hour to `0h ago`, and on a status table a literal zero in the "last report"
/// column reads as "no reports", which is the one reading this page may not
/// produce. Minutes are a measurement; the zero was not.
fn fmt_report_age(now_unix: i64, reported_unix: Option<i64>) -> String {
    let Some(reported_unix) = reported_unix else {
        return "Unknown".to_string();
    };
    if reported_unix > now_unix {
        // Machines' clocks disagree, and a report stamped ahead of this
        // machine's clock is not an age at all. Clamping it to zero would be
        // the same fabricated measurement as above.
        return "in the future".to_string();
    }
    let secs = now_unix - reported_unix;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        fmt_age(secs)
    }
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
