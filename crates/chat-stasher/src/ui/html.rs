//! html — escaping, measurements, the stylesheet and the page chrome.
//!
//! Every page is built from these: one escaping function for element content
//! and quoted attributes, unit-carrying byte/instant/age formats (never a bare
//! ratio), and the three statements [`footer`] puts on every page.

use crate::schedule::sh_single_quote;
use crate::selector::Selector;

use super::{DestinationState, UiData, UiSession, DAY};

// ------------------------------------------------------------- html rendering

pub(super) fn esc(s: &str) -> String {
    crate::view::esc(s)
}

/// An index excerpt as HTML: every character of it escaped, and the runs the
/// index marked as the match wrapped in `<mark>`.
///
/// The escaping happens **per run**, on the conversation text, and the only
/// markup added is the two tags this function writes — so conversation text
/// cannot become markup by containing a tag, and the marker characters cannot
/// reach the page (the split drops them; see [`crate::fts::marked_segments`]).
pub(super) fn marked_html(excerpt: &str) -> String {
    let mut out = String::new();
    for segment in crate::fts::marked_segments(excerpt) {
        if segment.matched {
            out.push_str("<mark>");
            out.push_str(&esc(&segment.text));
            out.push_str("</mark>");
        } else {
            out.push_str(&esc(&segment.text));
        }
    }
    out
}

/// `1 234 567 B (1.2 MiB)` — a measurement with its unit, never a bare ratio.
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TiB", 1 << 40),
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
    ];
    for (unit, scale) in UNITS {
        if n >= scale {
            let whole = n / scale;
            let frac = (n % scale) * 10 / scale;
            return format!("{whole}.{frac} {unit} ({n} B)");
        }
    }
    format!("{n} B")
}

/// `2026-01-15 12:34 UTC`. UTC rather than local so the page is a pure function
/// of the data — a local rendering would change with the reader's clock.
pub fn fmt_unix(unix: i64) -> String {
    match chrono::DateTime::from_timestamp(unix, 0) {
        Some(t) => t.format("%Y-%m-%d %H:%M UTC").to_string(),
        None => format!("(unrepresentable instant: {unix})"),
    }
}

/// A duration as whole days/hours. Used for ages only.
pub fn fmt_age(secs: i64) -> String {
    let secs = secs.max(0);
    let days = secs / DAY;
    if days >= 1 {
        format!("{days}d ago")
    } else {
        format!("{}h ago", secs / 3600)
    }
}

const STYLE: &str = r#"
:root{color-scheme:light dark;--fg:#1a1a1a;--bg:#fff;--muted:#5c5c5c;--line:#d8d8d8;
--head:#f4f4f4;--note-bg:#fffbe6;--note-line:#e6d98a;--warn-bg:#ffecec;--warn-line:#e0a0a0;
--ok:#1c6b3c;--bad:#a33;--link:#0b57d0;--up:#1c6b3c;--down:#8a1c1c}
@media (prefers-color-scheme:dark){:root{--fg:#e6e6e6;--bg:#141414;--muted:#a8a8a8;--line:#3a3a3a;
--head:#1f1f1f;--note-bg:#2e2a17;--note-line:#6b5f22;--warn-bg:#2e1a1a;--warn-line:#6b2a2a;
--ok:#7ddc9f;--bad:#ff9b9b;--link:#8ab4f8;--up:#7ddc9f;--down:#ff9b9b}}
*{box-sizing:border-box}
body{font:15px/1.55 -apple-system,system-ui,"Segoe UI",sans-serif;margin:0;padding:1.5rem 1.25rem 4rem;
color:var(--fg);background:var(--bg);max-width:76rem}
h1{font-size:1.4rem;margin:0 0 .2rem}
h2{font-size:1.05rem;margin:2rem 0 .6rem;border-bottom:1px solid var(--line);padding-bottom:.25rem}
a{color:var(--link)}
.sub{color:var(--muted);margin:0 0 1rem}
.mono{font-family:ui-monospace,Menlo,Consolas,monospace;font-size:.88em}
table{border-collapse:collapse;width:100%;margin:.4rem 0}
th,td{border-bottom:1px solid var(--line);padding:.35rem .55rem;text-align:left;vertical-align:top}
th{background:var(--head);font-weight:600}
td.n,th.n{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}
.note{background:var(--note-bg);border:1px solid var(--note-line);padding:.7rem 1rem;margin:.9rem 0;border-radius:4px}
.warn{background:var(--warn-bg);border:1px solid var(--warn-line);padding:.7rem 1rem;margin:.9rem 0;border-radius:4px}
.stats{display:flex;flex-wrap:wrap;gap:1.5rem;margin:1rem 0}
.stat{min-width:8rem}
.stat .v{display:block;font-size:1.6rem;font-weight:600;font-variant-numeric:tabular-nums}
.stat .l{color:var(--muted);font-size:.85rem}
.scroll{overflow-x:auto}
.heat td.cell{padding:0;text-align:center}
.heat td.cell a,.heat td.cell span{display:block;padding:.2rem .35rem;text-decoration:none;color:inherit}
.heat td.cell a:hover{outline:1px solid var(--link)}
.heat th.week{font-weight:400;font-size:.75rem;writing-mode:vertical-rl;transform:rotate(180deg);
white-space:nowrap;padding:.3rem .1rem;color:var(--muted)}
.heat td.uk{text-align:center;color:var(--muted)}
.ok{color:var(--ok)}.bad{color:var(--bad)}
/* R10 (29-UI-DESIGN §1.2): the ×N-backup badge. Colour comes from the
   variables above — the badge is a word first and a colour second. */
.badge{background:var(--head);border:1px solid var(--line);border-radius:3px;
font-size:.74rem;padding:.05rem .3rem;white-space:nowrap;color:var(--fg)}
/* UIA-3 (29-UI-DESIGN §3.1): the matrix's source columns grouped and coloured
   by platform group — labels first, colour second (the group name sits in the
   header row), and drawn only from the variables above (§8.1: no new colours). */
th.ghead{font-weight:400;font-size:.78rem;padding-bottom:.05rem}
.g-web{color:var(--link)}
.g-agents{color:var(--ok)}
.g-ungrouped{color:var(--muted)}
footer{color:var(--muted);font-size:.82rem;margin-top:2.5rem;border-top:1px solid var(--line);padding-top:.8rem}
pre{white-space:pre-wrap;word-break:break-word;background:var(--head);padding:.6rem .8rem;
border-radius:4px;overflow-x:auto;font-size:.82rem;line-height:1.35}
.message{border:1px solid var(--line);border-radius:5px;margin:1rem 0;padding:.75rem 1rem}
.message>header{color:var(--muted);font-size:.85rem;margin-bottom:.5rem}
.message>header b{color:var(--fg)}
.message p:first-child{margin-top:0}.message p:last-child{margin-bottom:0}
.message details{margin:.6rem 0}
.message summary{cursor:pointer}
.thinking{border-left:3px solid var(--note-line);padding-left:.7rem}
.tool{border-left:3px solid var(--link);padding-left:.7rem}
.attachment{color:var(--muted);border-left:3px solid var(--line);padding-left:.7rem}
nav.sub{display:flex;gap:1rem}
"#;

pub(super) fn head(title: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=en><head><meta charset=utf-8>\n\
         <meta name=viewport content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n<style>{STYLE}</style></head><body>\n",
        esc(title)
    )
}

/// Every page carries the same three statements. They are the reason the page
/// can be trusted at a glance, so they are emitted by one function rather than
/// re-typed per route.
///
/// The first statement is the one a route may have to replace: it says the page
/// holds no conversation text beyond a one-line label, and `/search` shows a
/// bounded excerpt of indexed conversation around each match. That page uses
/// [`search_footer`] instead of quietly carrying a sentence that would be false
/// on it — a footer that is true of most pages and wrong on one is worse than
/// no footer, because it is read as a property of the page in front of you.
pub(super) fn footer(data: &UiData) -> String {
    format!(
        "<footer>\n\
         <p><b>Metadata tier.</b> This page was rendered from one archive-metadata read \
         taken before the server started (snapshots + index + tree + the activity sidecar). \
         The only conversation text it holds is each session's one-line label — at most 100 \
         characters, recorded in the activity index, which rode that same metadata read. \
         {} session shard blob(s) were fetched by that read — a session's full conversation \
         is fetched only when you click it and then click <i>load</i>, and the cost is shown \
         before you do.</p>\n{tail}",
        data.data_blobs_read,
        tail = footer_tail()
    )
}

/// The `/search` page's first statement, which says what that page actually
/// holds: the index is a plaintext cache of conversation text, and the excerpts
/// on the page come from it rather than from the payload tier.
pub(super) fn search_footer(data: &UiData) -> String {
    format!(
        "<footer>\n\
         <p><b>Index tier.</b> This page was rendered from one archive-metadata read taken \
         before the server started plus the local full-text index. Each hit shows at most about \
         240 characters of conversation text around the match, taken from that index — not from \
         the archive: the index is a plaintext cache built by <code>chat-stasher index build</code>, \
         and an excerpt's absence proves nothing about the archive, only about the index. \
         This page fetched {} session shard blob(s).</p>\n{tail}",
        data.data_blobs_read,
        tail = footer_tail()
    )
}

/// The two statements every page shares, whatever tier it reads.
fn footer_tail() -> &'static str {
    "<p>Machines are named by their archive partition id, which is the key the repository \
     actually partitions on. Times are UTC. No JavaScript, no external asset, no \
     off-host link is loaded by this page.</p>\n\
     <p><b>This server is on 127.0.0.1, which is not a security boundary</b> — any other \
     program on this machine can connect to it. Access is gated only by the random token \
     in this page's URL. Do not share the URL. The server exits by itself when idle.</p>\n\
     </footer>\n"
}

pub(super) fn completeness_banner(data: &UiData) -> String {
    if data.complete() {
        return if data.destinations.len() == 1 {
            "<p class=ok>Read in full — every snapshot scanned was readable.</p>\n".to_string()
        } else {
            format!(
                "<p class=ok>Read in full — every snapshot scanned was readable, in each of the \
                 {} destinations ({}).</p>\n",
                data.destinations.len(),
                esc(&data.destination_label),
            )
        };
    }
    if data.destinations.len() == 1 {
        return format!(
            "<div class=warn><b>INCOMPLETE READ.</b> {} part(s) of this destination could not be \
             read, so every count below is a <i>floor</i>: sessions missing from these tables are \
             UNKNOWN, not absent.<ul>{}</ul></div>\n",
            data.unreadable.len(),
            data.unreadable
                .iter()
                .map(|u| format!("<li class=mono>{}</li>", esc(u)))
                .collect::<String>()
        );
    }
    // Several destinations: the floor is contagious — one unreadable copy makes
    // every count on this page a floor, because a session the merge could not
    // see in that copy may be one the other copy does not hold either. The
    // failing destinations are named and the healthy ones are not: a page that
    // listed every destination under a failure banner would make a complete
    // read look damaged by association, which is its own kind of lie.
    let failed: Vec<&DestinationState> =
        data.destinations.iter().filter(|d| !d.complete()).collect();
    format!(
        "<div class=warn><b>INCOMPLETE READ.</b> {} of the {} destinations could not be read in \
         full, so every count below is a <i>floor</i>: sessions missing from these tables are \
         UNKNOWN, not absent. The destination(s) that could not be read:<ul>{}</ul>\
         The other destination(s) in this view read in full; their rows are real, and the \
         floor is what the merge could not see in the one(s) above.</div>\n",
        failed.len(),
        data.destinations.len(),
        failed
            .iter()
            .map(|d| format!(
                "<li><b>{}</b> — {} part(s) unreadable<ul>{}</ul></li>",
                esc(&d.label),
                d.unreadable.len(),
                d.unreadable
                    .iter()
                    .map(|u| format!("<li class=mono>{}</li>", esc(u)))
                    .collect::<String>()
            ))
            .collect::<String>()
    )
}

/// Which copy of this session the payload routes open (`/session`, `/content`,
/// `/reader`), when there is more than one.
///
/// A merged row exists in several places and the bytes on the page came from
/// exactly one of them. The list's destination cell says which, but these are
/// different pages: someone following a link, a bookmark or a reload sees the
/// payload and not the list. "Which copy" is the fact that decides whether what
/// is on screen is the newest of them, so it is stated where the bytes are.
///
/// Nothing renders for a single-destination dashboard: there is one answer, it
/// is already in the page header, and a sentence repeating it would be noise on
/// every page that does have one.
pub(super) fn read_from_note(s: &UiSession, data: &UiData) -> String {
    if data.destinations.len() <= 1 {
        return String::new();
    }
    let copies: Vec<&str> = s
        .destinations
        .iter()
        .filter_map(|position| data.destination(*position))
        .map(|d| d.label.as_str())
        .collect();
    let Some(owner) = copies.first() else {
        return String::new();
    };
    format!(
        "<p class=sub>read from destination <b>{owner}</b> — this session is held by {n} \
         destination(s) ({all}), and the copies were pushed at different times, so another \
         copy's bytes can differ. Open another one by starting a dashboard that names only it \
         (<span class=mono>ui --destination &lt;name&gt;</span>).</p>\n",
        owner = esc(owner),
        n = copies.len(),
        all = esc(&copies.join(", ")),
    )
}

/// The per-destination block: what each copy held, and whether it read in full.
///
/// Rendered only when more than one destination was read. With one, every line
/// of it would restate the page header — and a table that says nothing is how a
/// reader learns to skip the tables that do.
pub(super) fn destinations_block(data: &UiData) -> String {
    if data.destinations.len() <= 1 {
        return String::new();
    }
    let rows: String = data
        .destinations
        .iter()
        .map(|d| {
            let state = if d.complete() {
                "read in full".to_string()
            } else {
                format!("{} part(s) unreadable", d.unreadable.len())
            };
            format!(
                "<tr><td><b>{}</b>{}</td><td class=n>{}</td><td class=n>{}</td>\
                 <td class=n>{}</td><td>{}</td></tr>\n",
                esc(&d.label),
                if d.complete() {
                    ""
                } else {
                    " <span class=bad title=\"this destination could not be read in full\">!</span>"
                },
                d.in_view,
                d.sessions,
                d.snapshots_scanned,
                esc(&state),
            )
        })
        .collect();
    format!(
        "<div class=note><b>Destinations read.</b> {} copies, merged row by row on \
         <span class=mono>(machine, session id)</span>: one session held by two destinations is \
         <b>one row</b> below with a <span class=badge>×2 backup</span> badge, and its facts are \
         read from the first destination named on the command line ({}). \
         <span class=mono>in view</span> counts the rows each copy put into the current filter; \
         <span class=mono>held</span> is that copy's whole total.<div class=scroll><table>\n\
         <thead><tr><th>destination</th><th class=n>in view</th><th class=n>held</th>\
         <th class=n>snapshots scanned</th><th>state</th></tr></thead>\n<tbody>\n{rows}\
         </tbody></table></div></div>\n",
        data.destinations.len(),
        esc(&data.first_destination_label()),
    )
}

/// The two count lines a merged view prints instead of one (§4.8): the
/// **distinct** sessions the view holds, and the **raw** copies the
/// destinations hold between them.
///
/// Both are printed whenever more than one destination was read, and neither
/// is derived at print time from the other. Choosing one would be the bug this
/// pair exists to prevent: "3 sessions" is wrong about redundancy and "5
/// sessions" is wrong about conversations, and a reader who sees only one of
/// them cannot tell which mistake they are looking at.
///
/// With a single destination the two counts are equal by construction, so the
/// lines are omitted rather than printed as two identical numbers.
///
/// It lives here, beside [`destinations_block`], because it is a statement
/// about the **view** and not about any one answer: it belongs on every page
/// that can show a merged view — the overview's headline number is the
/// distinct reading and needs the pair under it, and the list page needs it
/// under *each* answer it can give, including the zero-match one. A caller
/// that renders it on one branch of an if/else has made the other branch the
/// single place a reader can meet a merged view without the two counts.
pub(super) fn merged_counts(data: &UiData) -> String {
    if data.destinations.len() <= 1 {
        return String::new();
    }
    let raw_in_view: usize = data.destinations.iter().map(|d| d.in_view).sum();
    let doubled = raw_in_view.saturating_sub(data.sessions.len());
    format!(
        "<p class=sub><b>distinct:</b> {} session(s) in view — each session counted once, \
         however many destinations hold it.</p>\n\
         <p class=sub><b>raw:</b> {} session row(s) across the {} destinations — a session held \
         by more than one counts once per copy, so {doubled} row(s) here {verb} a second (or \
         later) copy of a session already counted.</p>\n",
        data.sessions.len(),
        raw_in_view,
        data.destinations.len(),
        verb = if doubled == 1 { "is" } else { "are" },
    )
}

/// R9 (29-UI-DESIGN §3.1/§4.1): machines that hold sessions but no activity
/// index — the same list the `/api/overview` field
/// `machines_without_activity_index` carries, which before W172 surfaced only
/// there and on one `search` hint line. Their conversation times were never
/// recorded, so their week cells and any time filter are unknown, not empty,
/// and the banner says which machines and what repairs them.
pub(super) fn machines_without_index_banner(data: &UiData) -> String {
    if data.machines_without_index.is_empty() {
        return String::new();
    }
    format!(
        "<div class=warn><b>Machines without an activity index.</b> {} machine(s) hold \
         sessions but no activity index beside the snapshot, so their conversation times \
         were never recorded: their heatmap rows, and any time filter below, are \
         <i>UNKNOWN</i> for them, not empty. Run <code>chat-stasher activity-index</code> \
         on that machine to record one.<ul>{}</ul></div>\n",
        data.machines_without_index.len(),
        data.machines_without_index
            .iter()
            .map(|m| format!("<li class=mono>{}</li>", esc(m)))
            .collect::<String>()
    )
}

pub(super) fn launch_banner(data: &UiData) -> String {
    let text = describe_selector(&data.launch);
    match text {
        None => String::new(),
        Some(text) => format!(
            "<div class=note><b>Filter in force.</b> {text}<br>\
             Every number below describes the <i>filtered</i> set, and a drill-down link is \
             further narrowed by exactly the filter the link carries — the two are a \
             conjunction, so no link can escape this filter. Clear it by reopening \
             <code>chat-stasher ui</code> with no filter flags.</div>\n",
            text = esc(&text)
        ),
    }
}

/// One constraint a [`Selector`] carries, in the one place its parts are
/// enumerated. Both things the pages say about a filter are rendered from
/// this walk and nothing else: the prose sentence ([`describe_selector`]) and
/// the CLI flags of the parity command ([`selector_cli_flags`]). A field added
/// to `Selector` must appear here once, or both renderers miss it together —
/// rather than one drifting while the other still names it.
enum Constraint<'a> {
    Machine(&'a str),
    Prefix(&'a str),
    Harnesses(&'a std::collections::BTreeSet<String>),
    Window(&'a crate::selector::TimeWindow),
}

fn constraints(selector: &Selector) -> Vec<Constraint<'_>> {
    let mut parts: Vec<Constraint<'_>> = Vec::new();
    if let Some(m) = &selector.machine {
        parts.push(Constraint::Machine(m));
    }
    if let Some(p) = &selector.session_id_prefix {
        parts.push(Constraint::Prefix(p));
    }
    if let Some(h) = &selector.harnesses {
        parts.push(Constraint::Harnesses(h));
    }
    if let Some(w) = &selector.window {
        parts.push(Constraint::Window(w));
    }
    parts
}

/// One sentence naming every constraint the page is showing, or `None` when
/// there is none. Never "no results" — the constraint itself is the answer.
pub fn describe_selector(selector: &Selector) -> Option<String> {
    let rendered: Vec<String> = constraints(selector)
        .into_iter()
        .map(|c| match c {
            Constraint::Machine(m) => format!("machine `{m}`"),
            Constraint::Prefix(p) => format!("session id starts with `{p}`"),
            Constraint::Harnesses(h) => {
                if h.is_empty() {
                    "harness list is empty (matches nothing)".to_string()
                } else {
                    format!(
                        "harness in {{{}}}",
                        h.iter().cloned().collect::<Vec<_>>().join(", ")
                    )
                }
            }
            Constraint::Window(w) => w.describe(),
        })
        .collect();
    if rendered.is_empty() {
        None
    } else {
        Some(rendered.join(" · "))
    }
}

/// The command-line flags that select exactly what `selector` selects — the
/// same flag vocabulary `search`/`export` read ([`crate::selector::SelectorArgs`]),
/// spelled so that pasting them onto `chat-stasher export` reproduces this
/// filter (29-UI-DESIGN §3.5/§6.5: the command the UI prints *is* the CLI
/// twin, generated from the same selector walk
/// [`describe_selector`] renders — reuse, not a second describer).
///
/// Every interpolateable value — machine, session prefix, the joined harness
/// list, window texts — prints as one single-quoted shell word
/// ([`crate::schedule::sh_single_quote`], XCU §2.2.2): a machine or prefix can
/// arrive from a URL's `?machine=`/`?session=`, and a value's spaces, breaks
/// or `$()` must stay the *filter's* bytes, never live syntax of the shell
/// the reader pastes the command into. Quoting is unconditional, tame values
/// too — "looks safe" is not a judgement a renderer may make about input it
/// did not choose, and round-tripping through the CLI's own flag reader sees
/// the same value either way.
///
/// `Ok` is a possibly-empty flag string (empty == no constraint, the whole
/// view). `Err` is a filter that selects something real but that **no single
/// command line can spell** — the honest answer for those is the reason, not
/// a near-miss command that would quietly select something else.
pub fn selector_cli_flags(selector: &Selector) -> Result<String, String> {
    let mut flags: Vec<String> = Vec::new();
    for c in constraints(selector) {
        match c {
            Constraint::Machine(m) => flags.push(format!("--machine {}", sh_single_quote(m))),
            Constraint::Prefix(p) => flags.push(format!("--session {}", sh_single_quote(p))),
            Constraint::Harnesses(h) => {
                if h.is_empty() {
                    return Err(
                        "the harness filter names an empty set, which matches nothing and which \
                         no command line can spell"
                            .to_string(),
                    );
                }
                flags.push(format!(
                    "--harness {}",
                    sh_single_quote(&h.iter().cloned().collect::<Vec<_>>().join(","))
                ));
            }
            Constraint::Window(w) => flags.push(window_flags(w)?),
        }
    }
    Ok(flags.join(" "))
}

/// The flag spelling of one window. Where a bound came with the text that was
/// typed for it, that text is what is printed — a round trip through the CLI
/// resolver produces the same window, and no other spelling is claimed. That
/// text goes out single-quoted like every other value: the genuine spellings
/// reach here already-validated, but the quoting is positional, so no future
/// text-bearing bound can print unquoted by accident.
fn window_flags(w: &crate::selector::TimeWindow) -> Result<String, String> {
    let bound = |unix: Option<i64>, text: Option<&str>| -> Result<String, String> {
        match (unix, text) {
            // A bound nobody can spell — the selector holds an instant with no
            // typed form — cannot be handed to the reader as if it had one.
            (Some(_), None) => Err(
                "the time window carries a bound with no command-line spelling; reopen the \
                 dashboard with the window spelled as a date flag"
                    .to_string(),
            ),
            (Some(_), Some(t)) => Ok(t.to_string()),
            (None, _) => Ok(String::new()),
        }
    };
    let mut flags: Vec<String> = Vec::new();
    match w.how {
        crate::selector::WindowHow::LocalDays => {
            let since = bound(w.since_unix, w.since_text.as_deref())?;
            let until = bound(w.until_unix, w.until_text.as_deref())?;
            if w.since_unix.is_some()
                && w.until_unix.is_some()
                && w.since_text.is_some()
                && w.since_text == w.until_text
            {
                // One inclusive local day. `--day D` is documented as
                // identical to `--since D --until D`, and the shorter form is
                // the one a reader types.
                return Ok(format!("--day {}", sh_single_quote(&since)));
            }
            if !since.is_empty() {
                flags.push(format!("--since {}", sh_single_quote(&since)));
            }
            if !until.is_empty() {
                flags.push(format!("--until {}", sh_single_quote(&until)));
            }
        }
        crate::selector::WindowHow::UnixSeconds => {
            let since = bound(w.since_unix, w.since_text.as_deref())?;
            let until = bound(w.until_unix, w.until_text.as_deref())?;
            if !since.is_empty() {
                flags.push(format!("--since-unix {}", sh_single_quote(&since)));
            }
            if !until.is_empty() {
                flags.push(format!("--until-unix {}", sh_single_quote(&until)));
            }
        }
    }
    Ok(flags.join(" "))
}

/// The flags that select the conjunction of the launch filter and a page's
/// drill-down filter — what "this view" actually is, because the page's rows
/// are the two filters applied together (the launch banner's own rule: no
/// drill-down link can escape it, and neither may the command that claims to
/// reproduce the view).
///
/// `Err` is one sentence naming the pair of filters that cannot hold at once
/// or cannot be spelled together; the caller shows the sentence and prints no
/// command, because a command that almost matches is worse than none.
pub fn conjoined_flags(launch: &Selector, query: &Selector) -> Result<String, String> {
    let machine = match (&launch.machine, &query.machine) {
        (Some(a), Some(b)) if a != b => {
            return Err(format!(
                "the launch filter names machine `{a}` and this page's filter names `{b}` — \
                 no session can match both, so the view is not spellable on one command line"
            ))
        }
        (Some(a), Some(_)) => Some(a.clone()),
        (Some(a), None) | (None, Some(a)) => Some(a.clone()),
        (None, None) => None,
    };
    let session_id_prefix = match (&launch.session_id_prefix, &query.session_id_prefix) {
        (Some(a), Some(b)) => {
            if a.starts_with(b.as_str()) {
                Some(a.clone())
            } else if b.starts_with(a.as_str()) {
                Some(b.clone())
            } else {
                return Err(format!(
                    "the launch filter's session prefix `{a}` and this page's `{b}` cannot both \
                     hold, so the view is not spellable on one command line"
                ));
            }
        }
        (Some(a), None) | (None, Some(a)) => Some(a.clone()),
        (None, None) => None,
    };
    let harnesses = match (&launch.harnesses, &query.harnesses) {
        (Some(a), Some(b)) => {
            let both = a & b;
            if both.is_empty() {
                return Err(format!(
                    "the launch filter's harnesses {{{}}} and this page's {{{}}} share none, so \
                     the view is not spellable on one command line",
                    a.iter().cloned().collect::<Vec<_>>().join(", "),
                    b.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            Some(both)
        }
        (Some(a), None) | (None, Some(a)) => Some(a.clone()),
        (None, None) => None,
    };
    let window = match (&launch.window, &query.window) {
        (Some(a), Some(b)) => Some(conjoined_window(a, b)?),
        (Some(a), None) | (None, Some(a)) => Some(a.clone()),
        (None, None) => None,
    };
    selector_cli_flags(&Selector {
        session_id_prefix,
        machine,
        harnesses,
        window,
    })
}

/// The intersection of two conversation-time windows: whichever window
/// constrains more is the bound that survives, carrying the spelling it was
/// typed with. The intersection can be genuinely empty — the filters then hold
/// no session at all, and a command line spelling of "nothing" is not the same
/// thing as an empty flag list, so it is reported rather than faked.
fn conjoined_window(
    a: &crate::selector::TimeWindow,
    b: &crate::selector::TimeWindow,
) -> Result<crate::selector::TimeWindow, String> {
    use crate::selector::TimeWindow;
    // The later lower bound and the earlier upper bound. `None` is the absence
    // of that bound's constraint, i.e. the losing side for a `max`/`min`.
    let since = match (a.since_unix, b.since_unix) {
        (Some(x), Some(y)) if x >= y => Some((x, a.since_text.clone(), a.how)),
        (Some(_), Some(y)) => Some((y, b.since_text.clone(), b.how)),
        (Some(x), None) => Some((x, a.since_text.clone(), a.how)),
        (None, Some(y)) => Some((y, b.since_text.clone(), b.how)),
        (None, None) => None,
    };
    let until = match (a.until_unix, b.until_unix) {
        (Some(x), Some(y)) if x <= y => Some((x, a.until_text.clone(), a.how)),
        (Some(_), Some(y)) => Some((y, b.until_text.clone(), b.how)),
        (Some(x), None) => Some((x, a.until_text.clone(), a.how)),
        (None, Some(y)) => Some((y, b.until_text.clone(), b.how)),
        (None, None) => None,
    };
    let how = match (&since, &until) {
        (Some((_, _, how)), Some((_, _, other))) => {
            if how != other {
                return Err(
                    "one filter's window was spelled as local calendar days and the other as \
                     unix seconds, so the intersection cannot be spelled on one command line"
                        .to_string(),
                );
            }
            *how
        }
        (Some((_, _, how)), None) | (None, Some((_, _, how))) => *how,
        (None, None) => return Err("the two filters' windows constrain nothing".to_string()),
    };
    if let (Some((s, _, _)), Some((u, _, _))) = (&since, &until) {
        if s > u {
            return Err(format!(
                "the launch filter's window ({}) and this page's window ({}) do not overlap, so \
                 no session can be in this view",
                a.describe(),
                b.describe()
            ));
        }
    }
    Ok(TimeWindow {
        since_unix: since.as_ref().map(|(v, _, _)| *v),
        until_unix: until.as_ref().map(|(v, _, _)| *v),
        how,
        since_text: since.map(|(_, t, _)| t).flatten(),
        until_text: until.map(|(_, t, _)| t).flatten(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::SelectorArgs;
    use std::collections::BTreeSet;

    /// The flags renderer's own contract: `describe_selector` is prose of the
    /// same walk, and the flags a selector prints must parse back — via the
    /// very `SelectorArgs` `search`/`export` read — into the same selector.
    /// A flag that cannot round-trip is a command the page cannot honestly
    /// print, so broken round trips must fail here rather than on a page.
    /// The line is read the way the reader's shell would read it first —
    /// quotes resolve to the value they carry — which is also why quoting
    /// every value is a no-op for this contract, never a change of flags.
    fn parse_back(flags: &str) -> crate::selector::Resolved {
        let mut args = SelectorArgs::default();
        let mut words = shell_words(flags).into_iter().peekable();
        while let Some(flag) = words.next() {
            let value = words.next().expect("every flag takes one value");
            match flag.as_str() {
                "--machine" => args.machine = Some(value),
                "--session" => args.session = Some(value),
                "--harness" => args.harness = Some(value.split(',').map(String::from).collect()),
                "--day" => args.day = Some(value),
                "--since" => args.since = Some(value),
                "--until" => args.until = Some(value),
                "--since-unix" => args.since_unix = value.parse().ok(),
                "--until-unix" => args.until_unix = value.parse().ok(),
                other => panic!("a flag nobody reads came out of the renderer: {other}"),
            }
        }
        args.resolve().expect("printed flags must resolve")
    }

    /// Split a line the way a POSIX shell reading it would (XCU §2.2.1–§2.2.3,
    /// §2.3 token recognition): single-quoted spans are literal to the next
    /// `'`, double-quoted spans to the next `"` (the renderer only ever puts a
    /// lone `'` inside those), a backslash outside quotes keeps the next
    /// character literal, and unquoted blanks end a word. Unterminated quotes
    /// are a paste hazard, not a value, so they panic rather than split.
    fn shell_words(line: &str) -> Vec<String> {
        let mut words: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut started = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\'' => {
                    started = true;
                    loop {
                        match chars.next() {
                            Some('\'') => break,
                            Some(other) => cur.push(other),
                            None => panic!("unterminated single quote — unsafe to paste"),
                        }
                    }
                }
                '"' => {
                    started = true;
                    loop {
                        match chars.next() {
                            Some('"') => break,
                            Some(other) => cur.push(other),
                            None => panic!("unterminated double quote — unsafe to paste"),
                        }
                    }
                }
                '\\' => {
                    started = true;
                    match chars.next() {
                        Some(escaped) => cur.push(escaped),
                        None => panic!("dangling escape — unsafe to paste"),
                    }
                }
                ' ' | '\t' | '\n' => {
                    if started {
                        words.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                other => {
                    started = true;
                    cur.push(other);
                }
            }
        }
        if started {
            words.push(cur);
        }
        words
    }

    /// Every shell metacharacter that appears OUTSIDE quotes in a rendered
    /// command line, excluding the spaces that separate the words themselves:
    /// no separator (`;|&`), substitution (`$()`, backticks), redirection
    /// (`<>`) or line/tab break may live outside quotes, because that alone
    /// would let a pasted line mean something the page did not say. (An
    /// unquoted *value* would also split into several words, which the
    /// `shell_words` equality catches; a page never legitimately emits tab or
    /// newline outside a value.)
    fn metachars_unquoted(line: &str) -> String {
        let mut seen = String::new();
        let mut in_single = false;
        let mut in_double = false;
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' if !in_single && !in_double => {
                    let _ = chars.next();
                }
                '\'' if !in_double => in_single = !in_single,
                '"' if !in_single => in_double = !in_double,
                '\t' | '\n' | ';' | '|' | '&' | '<' | '>' | '(' | ')' | '$' | '`' | '\''
                    if !in_single && !in_double =>
                {
                    seen.push(c);
                }
                _ => {}
            }
        }
        seen
    }

    #[test]
    fn the_printed_flags_round_trip_into_the_same_selector() {
        for args in [
            SelectorArgs::default(),
            SelectorArgs {
                machine: Some("m-3".into()),
                ..Default::default()
            },
            SelectorArgs {
                session: Some("019bf0".into()),
                machine: Some("m-3".into()),
                harness: Some(vec!["claude-code".into(), "codex".into()]),
                ..Default::default()
            },
            SelectorArgs {
                day: Some("2026-01-15".into()),
                ..Default::default()
            },
            SelectorArgs {
                since: Some("2026-01-15".into()),
                until: Some("2026-01-17".into()),
                ..Default::default()
            },
            SelectorArgs {
                since: Some("2026-01-15".into()),
                ..Default::default()
            },
            SelectorArgs {
                until: Some("2026-01-15".into()),
                ..Default::default()
            },
            SelectorArgs {
                since_unix: Some(100),
                until_unix: Some(200),
                ..Default::default()
            },
        ] {
            let resolved = args.resolve().unwrap();
            let flags = selector_cli_flags(&resolved.selector)
                .unwrap_or_else(|e| panic!("flags must spell: {e}"));
            let back = parse_back(&flags);
            assert_eq!(
                back.selector, resolved.selector,
                "flags `{flags}` do not round-trip"
            );
            // And the sentence is the same information in prose of the same
            // walk — both exist for the same selector or neither does.
            assert_eq!(
                describe_selector(&resolved.selector).is_some(),
                !flags.is_empty(),
                "prose and flags must agree on emptiness for `{flags}`"
            );
        }
    }

    /// The flags that pin the word order of the printed command, so review
    /// sees the exact string the pages print rather than deriving it.
    #[test]
    fn the_flag_words_are_the_exact_strings_the_pages_print() {
        let args = SelectorArgs {
            session: Some("019bf0".into()),
            machine: Some("m-3".into()),
            harness: Some(vec!["codex".into(), "claude-code".into()]),
            since: Some("2026-01-15".into()),
            until: Some("2026-01-16".into()),
            ..Default::default()
        };
        let s = args.resolve().unwrap().selector;
        assert_eq!(
            selector_cli_flags(&s).unwrap(),
            "--machine 'm-3' --session '019bf0' --harness 'claude-code,codex' \
             --since '2026-01-15' --until '2026-01-16'",
            "sorted harness list (a BTreeSet, not the typed order), one space between flags, \
             every value a single-quoted shell word"
        );
        // A same-day window prints its shortest honest form.
        let day = SelectorArgs {
            day: Some("2026-01-15".into()),
            ..Default::default()
        }
        .resolve()
        .unwrap()
        .selector;
        assert_eq!(selector_cli_flags(&day).unwrap(), "--day '2026-01-15'");
        // An all-constraint selector still has its prose sentence — reused,
        // not rebuilt, by the page.
        assert_eq!(
            describe_selector(&s).unwrap(),
            "machine `m-3` · session id starts with `019bf0` · harness in {claude-code, codex} \
             · local day(s) 2026-01-15 .. 2026-01-16 inclusive \
             (each day = 00:00:00–23:59:59 local)"
        );
    }

    /// The hostile-value regression W195's FIX-FIRST review demanded: a machine,
    /// session prefix, harness name or window text that carries shell syntax
    /// must reach the printed command as ONE quoted shell word, so pasting the
    /// command cannot let a value's syntax run (the payloads here are inert
    /// sentinels — if any ever executed, split a word or turned a page's filter
    /// into live syntax, the assertions below stop passing).
    #[test]
    fn hostile_selector_values_print_as_one_quoted_shell_word_each() {
        let machine = "m 3; $(pwned) `bt` 'q' \"d\"\nEnd";
        let prefix = "019 'x;$(y)";
        let harnesses: Option<BTreeSet<String>> = Some(
            ["ha r;ne$(x)s".to_string(), "tu\"ck".to_string()]
                .into_iter()
                .collect(),
        );
        let selector = Selector {
            session_id_prefix: Some(prefix.into()),
            machine: Some(machine.into()),
            harnesses,
            window: Some(crate::selector::TimeWindow {
                since_unix: Some(1),
                until_unix: Some(2),
                how: crate::selector::WindowHow::LocalDays,
                since_text: Some("2026-01-15$(rm)".into()),
                until_text: Some("2026-01-16;x".into()),
            }),
        };
        // The exact string the page is allowed to print for this view: every
        // value wrapped in single quotes, every embedded single quote spelled
        // close-double-quote-open (`'"'"'`), so a POSIX shell reading the line
        // reconstructs each value as one literal word.
        assert_eq!(
            selector_cli_flags(&selector).unwrap(),
            "--machine 'm 3; $(pwned) `bt` '\"'\"'q'\"'\"' \"d\"\nEnd' \
             --session '019 '\"'\"'x;$(y)' \
             --harness 'ha r;ne$(x)s,tu\"ck' \
             --since '2026-01-15$(rm)' --until '2026-01-16;x'",
            "every value must arrive quoted, in the selector walk's order"
        );
        // And the line is safe to paste: a shell reading it yields each value
        // back as one intact word, and holds no metacharacter outside quotes.
        let flags = selector_cli_flags(&selector).unwrap();
        assert_eq!(
            shell_words(&flags),
            vec![
                "--machine",
                machine,
                "--session",
                prefix,
                "--harness",
                "ha r;ne$(x)s,tu\"ck",
                "--since",
                "2026-01-15$(rm)",
                "--until",
                "2026-01-16;x",
            ],
            "the payloads must ride as single literal words, never as syntax"
        );
        assert_eq!(
            metachars_unquoted(&flags),
            "",
            "no separator, substitution or redirection may live outside quotes \
             (a hostile value's space is caught above, as a split word)"
        );
        // And the command still means what the page says it means: with a
        // window the CLI resolver accepts, the quoted hostile values round-trip
        // through the reader `chat-stasher export` itself uses — the very
        // `SelectorArgs` path — into the same selector. Quoting changed nothing
        // about which sessions the pasted command would select.
        let hostile = SelectorArgs {
            session: Some(prefix.into()),
            machine: Some(machine.into()),
            harness: Some(vec!["ha r;ne$(x)s".into(), "tu\"ck".into()]),
            day: Some("2026-01-15".into()),
            ..Default::default()
        }
        .resolve()
        .unwrap();
        let printed = selector_cli_flags(&hostile.selector).unwrap();
        assert_eq!(
            parse_back(&printed).selector,
            hostile.selector,
            "quoted hostile values must still select the same set via the CLI's own reader: \
             {printed}"
        );
    }

    // ------------------------------------------------------ the conjunction

    fn harnesses(list: &[&str]) -> Option<BTreeSet<String>> {
        Some(list.iter().map(|s| s.to_string()).collect())
    }

    /// What "this view" is: the launch filter AND the page's filter, both of
    /// which must appear on one command line — a command naming only one of
    /// them would quietly export more than the page shows.
    #[test]
    fn the_command_carries_both_filters_as_one_conjunction() {
        let launch = Selector {
            machine: Some("m-1".into()),
            harnesses: harnesses(&["claude-code", "codex"]),
            ..Default::default()
        };
        let query = SelectorArgs {
            day: Some("2026-01-15".into()),
            session: Some("019bf0".into()),
            ..Default::default()
        }
        .resolve()
        .unwrap()
        .selector;
        assert_eq!(
            conjoined_flags(&launch, &query).unwrap(),
            "--machine 'm-1' --session '019bf0' --harness 'claude-code,codex' --day '2026-01-15'"
        );
        // Harness sets intersect rather than union: the view a facet link
        // narrows is the set both filters keep.
        let query_harness = Selector {
            harnesses: harnesses(&["codex", "gemini"]),
            ..Default::default()
        };
        assert_eq!(
            conjoined_flags(&launch, &query_harness).unwrap(),
            "--machine 'm-1' --harness 'codex'"
        );
        // One window inside another is the inner one.
        let launch_day = SelectorArgs {
            since: Some("2026-01-01".into()),
            until: Some("2026-02-01".into()),
            ..Default::default()
        }
        .resolve()
        .unwrap()
        .selector;
        let query_day = SelectorArgs {
            day: Some("2026-01-15".into()),
            ..Default::default()
        }
        .resolve()
        .unwrap()
        .selector;
        assert_eq!(
            conjoined_flags(&launch_day, &query_day).unwrap(),
            "--day '2026-01-15'"
        );
        // A branch-filter-derived prefix narrows a launch prefix.
        let launch_prefix = Selector {
            session_id_prefix: Some("019bf0".into()),
            ..Default::default()
        };
        let query_prefix = Selector {
            session_id_prefix: Some("019bf0d-".into()),
            ..Default::default()
        };
        assert_eq!(
            conjoined_flags(&launch_prefix, &query_prefix).unwrap(),
            "--session '019bf0d-'"
        );
    }

    /// Filters that cannot hold at once are reported, never approximated: the
    /// block prints the reason and no command, because a command that
    /// "almost" matches would export a different set than the page shows.
    #[test]
    fn filters_that_cannot_both_hold_are_an_error_never_a_near_miss() {
        let launch = Selector {
            machine: Some("m-1".into()),
            harnesses: harnesses(&["claude-code"]),
            session_id_prefix: Some("019bf0".into()),
            ..Default::default()
        };
        for (query, names) in [
            (
                Selector {
                    machine: Some("m-2".into()),
                    ..Default::default()
                },
                "different machines",
            ),
            (
                Selector {
                    harnesses: harnesses(&["codex"]),
                    ..Default::default()
                },
                "disjoint harness sets",
            ),
            (
                Selector {
                    session_id_prefix: Some("0000".into()),
                    ..Default::default()
                },
                "contradictory prefixes",
            ),
        ] {
            let why = conjoined_flags(&launch, &query).expect_err(names);
            assert!(
                why.contains("not spellable"),
                "{names} must be named as unspellable, not swallowed: {why}"
            );
        }
        // Windows that do not overlap select nothing by construction — the
        // same class an inverted `--since/--until` is refused for on the CLI.
        let launch_day = SelectorArgs {
            day: Some("2026-01-15".into()),
            ..Default::default()
        }
        .resolve()
        .unwrap()
        .selector;
        let other_day = SelectorArgs {
            day: Some("2026-03-01".into()),
            ..Default::default()
        }
        .resolve()
        .unwrap()
        .selector;
        let why = conjoined_flags(&launch_day, &other_day).expect_err("disjoint windows");
        assert!(
            why.contains("do not overlap"),
            "the refusal must name the disjoint windows: {why}"
        );
        // The deprecated unix spelling cannot mix with day spellings on one
        // command line (`--since` conflicts with `--until-unix`), so neither
        // can the intersection.
        let unix = SelectorArgs {
            since_unix: Some(100),
            until_unix: Some(200),
            ..Default::default()
        }
        .resolve()
        .unwrap()
        .selector;
        let why = conjoined_flags(&launch_day, &unix).expect_err("mixed window spellings");
        assert!(
            why.contains("unix seconds"),
            "the refusal must name the mixed spellings: {why}"
        );
    }
}
