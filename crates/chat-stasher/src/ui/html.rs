//! html — escaping, measurements, the stylesheet and the page chrome.
//!
//! Every page is built from these: one escaping function for element content
//! and quoted attributes, unit-carrying byte/instant/age formats (never a bare
//! ratio), and the three statements [`footer`] puts on every page.

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

/// One sentence naming every constraint the page is showing, or `None` when
/// there is none. Never "no results" — the constraint itself is the answer.
pub fn describe_selector(selector: &Selector) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(m) = &selector.machine {
        parts.push(format!("machine `{m}`"));
    }
    if let Some(p) = &selector.session_id_prefix {
        parts.push(format!("session id starts with `{p}`"));
    }
    if let Some(h) = &selector.harnesses {
        if h.is_empty() {
            parts.push("harness list is empty (matches nothing)".to_string());
        } else {
            parts.push(format!(
                "harness in {{{}}}",
                h.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
    }
    if let Some(w) = &selector.window {
        parts.push(w.describe());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}
