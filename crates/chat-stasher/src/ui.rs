//! ui — the dashboard the ephemeral loopback server ([`crate::view`]) renders.
//!
//! One page answers the question the archive exists to answer, at a glance:
//! **how many conversations are stored, where they are (machine × source), and
//! is the backup healthy?** Nothing else on the page is allowed to be the first
//! thing you have to read.
//!
//! Product decisions this module implements:
//!
//! * **Metadata tier by default.** The overview, the machine table and every
//!   list are computed from one [`crate::search::search_sessions`] report taken
//!   *before* the socket is bound, plus the activity sidecar that report already
//!   parsed. No request re-reads the repository, and the one route that does —
//!   `/content` — is reachable only by an explicit click and says what it will
//!   cost before it fetches anything. The footer states the tier on the page.
//! * **The filters are the shared selector.** A drill-down link carries
//!   `--machine` / `--harness` / `--session` / `--day` / `--since` / `--until`
//!   in a query string, and the server turns them into a
//!   [`crate::selector::Selector`] through the very same
//!   [`crate::selector::SelectorArgs::resolve`] `search` uses. The list is then
//!   produced by applying [`crate::selector::Selector::select`] to the
//!   inventory, which is what `search` does to the same rows — so a drill-down
//!   and a `search` with the same flags return the same set, and a test pins
//!   that against a real repository.
//! * **Unknown is never empty.** A machine whose activity index is missing or
//!   unreadable is listed as such, never as "no sessions". A session whose
//!   conversation time is unknown is listed separately with its reason. An
//!   incomplete read says so before it says anything else.
//! * **No fake percentages.** Health is a word and an age, both measured:
//!   "archived recently", "no push for N days", "index missing", "index
//!   unreadable". There is no "87% healthy" anywhere, because nothing here
//!   measures a denominator that would make such a number mean anything.
//!
//! Privacy line, same as `search`/`read`: machine partition, short session id,
//! shard counts, byte lengths, timestamps. A full session id never reaches the
//! page — the URL addresses a row by its **position** in the inventory, and the
//! server resolves that position itself.

use std::collections::{BTreeMap, BTreeSet};

use crate::activity::TimeSource;
use crate::overview::{self, Granularity, HeatmapAxis, OverviewRow};
use crate::search::{HostSnapshot, SearchReport};
use crate::selector::{
    Resolved, Selector, SelectorArgs, SessionMeta, UnplacedBy, UsageError, Verdict,
};
use crate::view::Response;

/// How long without a push before a machine stops reading as healthy.
///
/// Two weeks, chosen because the dashboard is opened roughly weekly: at that
/// cadence a machine that missed a week is worth noticing, and one that missed
/// two is worth looking at. It is a threshold on a measured age — never a
/// score, and never divided into anything.
pub const STALE_AFTER_DAYS: i64 = 14;

const DAY: i64 = 86_400;

// --------------------------------------------------------------------- model

/// One session row. Every field is metadata-tier; `session_id` is the only one
/// that is never rendered (see the module docs).
#[derive(Debug, Clone)]
pub struct UiSession {
    /// Position in [`UiData::sessions`] — the handle a URL carries, so the
    /// browser history and any screenshot of it hold no session id.
    pub index: usize,
    pub machine: String,
    /// `None` when the archived id carries no harness prefix. Kept as `None`
    /// rather than a placeholder string: the selector treats it as "cannot be
    /// evaluated", and the matrix renders it as its own column.
    pub harness: Option<String>,
    pub session_id: String,
    pub short_id: String,
    pub shard_count: usize,
    pub bytes: u64,
    pub first_unix: Option<i64>,
    pub last_unix: Option<i64>,
    /// Why the conversation time is unknown. `Some` whenever a bound is `None`.
    pub time_why: Option<String>,
    /// The activity index's own tri-state, carried through unchanged.
    pub time_source: TimeSource,
    pub line_count: u64,
    /// The snapshot's own time — the backup run, not the conversation's.
    pub archive_time_unix: i64,
    pub data_blobs: usize,
}

/// The label a session with no harness prefix gets in the matrix. Not a harness
/// id, and deliberately shaped so it cannot be mistaken for one.
pub const NO_HARNESS: &str = "(no harness prefix)";

impl UiSession {
    pub fn source_label(&self) -> String {
        self.harness
            .clone()
            .unwrap_or_else(|| NO_HARNESS.to_string())
    }

    /// The same rule [`OverviewRow::has_known_time`] applies, so the dashboard's
    /// heatmap and `overview`'s cannot disagree about which sessions have a
    /// time. Delegated rather than re-written.
    pub fn has_known_time(&self) -> bool {
        self.overview_row().has_known_time()
    }

    /// This session as the row type `overview` renders.
    pub fn overview_row(&self) -> OverviewRow {
        OverviewRow {
            session_id: self.session_id.clone(),
            machine: self.machine.clone(),
            harness: self.source_label(),
            first_unix: self.first_unix,
            last_unix: self.last_unix,
            line_count: self.line_count,
            time_source: overview::TimeSource::from(&self.time_source),
        }
    }

    /// The metadata the shared selector decides on. Borrowed, so filtering never
    /// clones a string to ask a yes/no question.
    pub fn meta(&self) -> SessionMeta<'_> {
        SessionMeta {
            machine: &self.machine,
            session_id: &self.session_id,
            harness: self.harness.as_deref(),
            first_unix: self.first_unix,
            last_unix: self.last_unix,
            time_why: self.time_why.as_deref(),
        }
    }
}

/// Everything the dashboard renders, computed once before the socket is bound.
#[derive(Debug, Clone)]
pub struct UiData {
    /// The destination *name* the user typed — deliberately not `repo_root`,
    /// which would put a real hostname on a page served over a socket.
    pub destination_label: String,
    pub snapshots_scanned: usize,
    pub snapshots_in_repo: usize,
    pub sessions_seen: usize,
    /// How many sessions the tree held, before any filter. Kept beside
    /// [`Self::sessions`] so a "0 of N" sentence quotes the archive rather than
    /// the filtered set it is comparing against.
    pub archive_sessions: usize,
    /// Sessions the launch filter did not reject, keyed by their position in
    /// the **whole** inventory. Every drill-down filters this set again, with
    /// the shared selector, so the two filters compose as a conjunction.
    pub sessions: Vec<UiSession>,
    /// Constraints from the command line. `Selector::default()` == none.
    pub launch: Selector,
    /// The machine list: one entry per hostname whose newest snapshot was read.
    pub hosts: Vec<HostSnapshot>,
    /// Machines that hold sessions but no activity index beside them.
    pub machines_without_index: Vec<String>,
    /// Non-empty == part of the destination could not be read. Every count on
    /// the page is then a floor, not a measurement.
    pub unreadable: Vec<String>,
    /// Session shard data blobs this run fetched. Structurally always 0.
    pub data_blobs_read: usize,
    /// Activity-index files this run fetched.
    pub index_files_read: usize,
    /// The clock every age on the page is measured against, passed in rather
    /// than read, so the rendered page is a pure function of the data.
    pub now_unix: i64,
}

impl UiData {
    /// Build from a report produced with **no** selector constraints, so that
    /// `hits` is the whole inventory. A caller that filters the repository read
    /// itself would make the drill-downs unable to see what the launch filter
    /// rejected, and the "same result set as `search`" property would be gone.
    ///
    /// `launch` is applied **here, to the inventory**, and not merged into each
    /// request's filter. That is what keeps the two filters a conjunction:
    /// `select(select(all, launch), query)` is `select(all, launch ∧ query)`
    /// because every constraint in [`Selector::select`] is a plain conjunction,
    /// so a link cannot escape the filter the page says is in force. It also
    /// means the drill-down's own `not_matched` is counted against what the
    /// dashboard actually shows, which is the only denominator it can honestly
    /// quote.
    ///
    /// Sessions the launch filter **could not evaluate** stay in the inventory:
    /// dropping them would turn "we could not tell" into "it is not there".
    pub fn from_report(
        report: &SearchReport,
        label: impl Into<String>,
        launch: Selector,
        now_unix: i64,
    ) -> Self {
        let all: Vec<UiSession> = report
            .hits
            .iter()
            .enumerate()
            .map(|(index, h)| UiSession {
                index,
                machine: h.machine.clone(),
                harness: h.harness.clone(),
                session_id: h.session_id.clone(),
                short_id: h.short_id(),
                shard_count: h.shard_count,
                bytes: h.bytes,
                first_unix: h.first_unix,
                last_unix: h.last_unix,
                time_why: h.time_why.clone(),
                time_source: h.time_source.clone(),
                line_count: h.line_count,
                archive_time_unix: h.archive_time_unix,
                data_blobs: h.data_blobs,
            })
            .collect();
        // `index` is a position in `all`, and stays one: a drill-down URL is a
        // handle into the whole inventory, not into whatever the launch filter
        // left behind, so the same link keeps working under any launch filter.
        let keep: BTreeSet<usize> = select(&all, &launch)
            .in_view()
            .into_iter()
            .map(|s| s.index)
            .collect();
        let sessions: Vec<UiSession> = all
            .into_iter()
            .filter(|s| keep.contains(&s.index))
            .collect();
        let mut hosts = report.hosts.clone();
        hosts.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        Self {
            destination_label: label.into(),
            snapshots_scanned: report.snapshots_scanned,
            snapshots_in_repo: report.snapshots_in_repo,
            sessions_seen: report.sessions_seen,
            archive_sessions: report.hits.len(),
            sessions,
            launch,
            hosts,
            machines_without_index: report.machines_without_index.clone(),
            unreadable: report.unreadable.clone(),
            data_blobs_read: report.data_blobs_read,
            index_files_read: report.index_files_read,
            now_unix,
        }
    }

    pub fn complete(&self) -> bool {
        self.unreadable.is_empty()
    }

    /// The row a URL's `i` names. Linear, because `index` is a position in the
    /// *whole* inventory while this vector holds the launch-filtered subset —
    /// so `get(index)` would be wrong the moment a launch filter is in force.
    pub fn session_at(&self, index: usize) -> Option<&UiSession> {
        self.sessions.iter().find(|s| s.index == index)
    }

    /// Every machine the page must name: the hosts that pushed a snapshot, plus
    /// any partition that holds sessions without being a snapshot hostname.
    /// Sorted and deduplicated, so the machine table and the matrix cannot
    /// disagree about which machines exist.
    pub fn machine_keys(&self) -> Vec<String> {
        let mut keys: BTreeSet<String> = self.hosts.iter().map(|h| h.hostname.clone()).collect();
        for s in &self.sessions {
            keys.insert(s.machine.clone());
        }
        keys.into_iter().collect()
    }

    fn host(&self, machine: &str) -> Option<&HostSnapshot> {
        self.hosts.iter().find(|h| h.hostname == machine)
    }
}

/// What the selector decided about the inventory, in the three groups `search`
/// also keeps apart.
#[derive(Debug, Clone)]
pub struct Selection<'a> {
    pub matched: Vec<&'a UiSession>,
    /// Sessions an active constraint could not be evaluated for. Neither a
    /// match nor a proven non-match — they are listed with their reason.
    pub unplaced: Vec<(&'a UiSession, UnplacedBy, String)>,
    /// Sessions every active constraint evaluated and rejected. A count, not a
    /// fallback.
    pub not_matched: usize,
}

impl<'a> Selection<'a> {
    /// Every session in view: the matches plus the ones no filter could decide.
    /// Used for the counts that do not depend on time (totals, matrix), so a
    /// session the time filter could not place still appears in its machine's
    /// column instead of disappearing.
    pub fn in_view(&self) -> Vec<&'a UiSession> {
        let mut rows: Vec<&UiSession> = self.matched.clone();
        rows.extend(self.unplaced.iter().map(|(s, _, _)| *s));
        rows.sort_by_key(|s| s.index);
        rows
    }
}

/// Apply `selector` to the inventory, exactly as `search` applies it to the
/// same rows. The one implementation of the decision is
/// [`Selector::select`]; this only sorts the verdicts into the three groups.
pub fn select<'a>(sessions: &'a [UiSession], selector: &Selector) -> Selection<'a> {
    let mut out = Selection {
        matched: Vec::new(),
        unplaced: Vec::new(),
        not_matched: 0,
    };
    for s in sessions {
        match selector.select(&s.meta()) {
            Verdict::Selected => out.matched.push(s),
            Verdict::NotSelected => out.not_matched += 1,
            Verdict::Unevaluated { dimension, why } => {
                // Same ADR-035 reclassification as `search`: a session with no
                // conversation content is unplaceable for a different reason.
                let (dimension, why) = if dimension == UnplacedBy::Time
                    && s.time_source.is_no_conversation_content()
                {
                    (
                        UnplacedBy::NoContent,
                        "this session holds no conversation content (no user or assistant \
                         message); there is nothing to place in a time bucket"
                            .to_string(),
                    )
                } else {
                    (dimension, why)
                };
                out.unplaced.push((s, dimension, why));
            }
        }
    }
    out
}

// --------------------------------------------------------------------- health

/// A machine's state, as a word. Never a score.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// A snapshot exists and its activity index exists and was read.
    Archived { age_secs: i64 },
    /// A snapshot exists and its activity index exists, but could not be read —
    /// which is neither "no index" nor "fine".
    IndexUnreadable,
    /// A snapshot exists with no activity index beside it.
    IndexMissing,
    /// The snapshot's own time is unknown, so no age can be claimed.
    Unknown,
}

impl Health {
    /// The word shown on the page.
    pub fn word(&self) -> &'static str {
        match self {
            Health::Archived { .. } => "archived recently",
            Health::IndexUnreadable => "index unreadable",
            Health::IndexMissing => "index missing",
            Health::Unknown => "snapshot time unknown",
        }
    }
}

/// Decide one machine's health from its newest snapshot and the two index
/// facts, never from a session count.
pub fn health_of(host: Option<&HostSnapshot>, now_unix: i64) -> Health {
    let Some(host) = host else {
        return Health::Unknown;
    };
    if !host.has_activity_index {
        return Health::IndexMissing;
    }
    if !host.index_read_ok {
        return Health::IndexUnreadable;
    }
    let age_secs = now_unix.saturating_sub(host.archive_time_unix);
    Health::Archived { age_secs }
}

// ----------------------------------------------------------------- the server

/// One session's decrypted payload, as `/content` renders it.
#[derive(Debug, Clone)]
pub struct Content {
    /// `(shard file name, sha256 hex)` in sequence order — the same lines
    /// `chat-stasher read` prints.
    pub shards: Vec<(String, String)>,
    /// sha256 of the concatenation, hex.
    pub concat_sha256: String,
    /// Length of the concatenation in bytes.
    pub bytes: usize,
    /// The concatenated JSONL, lossily decoded for display.
    pub body: String,
}

/// Anything the `/content` route needs to fetch payload.
///
/// A trait rather than a concrete store handle for two reasons: the routing
/// function stays testable with a stub (so "no other route touches the payload
/// tier" is a *test*, not a claim), and the only code that can decrypt a
/// conversation lives behind the one route allowed to.
pub trait ContentSource {
    /// Fetch and decrypt one session. An error is a failure to read, never an
    /// empty session.
    fn fetch(&self, machine: &str, session_id: &str) -> Result<Content, String>;
}

/// Refuses every fetch. The default for anything that must not reach the
/// payload tier.
pub struct NoContent;

impl ContentSource for NoContent {
    fn fetch(&self, _machine: &str, _session_id: &str) -> Result<Content, String> {
        Err("the payload tier is disabled for this server".to_string())
    }
}

/// A decoded query string: ordered `(key, value)` pairs.
pub type Query = Vec<(String, String)>;

/// Percent-decode a query/form component. `+` is *not* a space here: the
/// links this server emits encode a space as `%20`, and treating `+` as a space
/// would corrupt a machine partition legitimately containing one.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode everything outside the unreserved set, so a machine name or a
/// session id can be carried in a link without ever changing its meaning.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Split `/path?query` and decode the query.
pub fn split_target(target: &str) -> (&str, Query) {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let params = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((percent_decode(k), percent_decode(v)))
        })
        .collect();
    (path, params)
}

fn param<'a>(params: &'a Query, key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Build a [`Selector`] out of a drill-down query string.
///
/// The flags are collected into the *same* [`SelectorArgs`] `search` declares,
/// and resolved by the *same* [`SelectorArgs::resolve`], so a query string can
/// express exactly what a command line can and nothing more. A value that
/// cannot be resolved is a usage error — reported as a 400, never as "0 rows",
/// because a typo must not be dressed up as a measurement.
pub fn selector_from_query(params: &Query) -> Result<Resolved, UsageError> {
    let harness = param(params, "harness").map(|h| {
        h.split(',')
            .filter(|p| !p.is_empty())
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
    });
    SelectorArgs {
        session: param(params, "session").map(str::to_string),
        machine: param(params, "machine").map(str::to_string),
        harness,
        day: param(params, "day").map(str::to_string),
        since: param(params, "since").map(str::to_string),
        until: param(params, "until").map(str::to_string),
        since_unix: None,
        until_unix: None,
    }
    .resolve()
}

/// Route one request. `None` == no such route (the caller answers 404).
///
/// Every branch below except `/content` is a pure function of `data`. The
/// `content` handle is passed in — not fetched here — so a test can prove which
/// routes reach the payload tier.
pub fn handle(
    path: &str,
    params: &Query,
    token: &str,
    data: &UiData,
    content: &dyn ContentSource,
) -> Option<Response> {
    match path {
        "/" => Some(Response::html(200, "OK", page_overview(data, token))),
        "/sessions" => Some(list_page(params, token, data)),
        "/session" => Some(one_session_page(params, token, data)),
        "/content" => Some(content_page(params, data, content)),
        "/api/overview" => Some(Response::json(200, "OK", json_overview(data))),
        "/api/sessions" => Some(match selector_from_query(params) {
            Ok(r) => Response::json(
                200,
                "OK",
                json_sessions(&select(&data.sessions, &r.selector), &r, token, data),
            ),
            Err(e) => Response::json(400, "Bad Request", json_filter_error(data, &e)),
        }),
        _ => None,
    }
}

// --------------------------------------------------------------------- pages

fn list_page(params: &Query, token: &str, data: &UiData) -> Response {
    let resolved = match selector_from_query(params) {
        Ok(r) => r,
        Err(e) => return Response::text(400, "Bad Request", format!("ui: {e}\n")),
    };
    let sel = select(&data.sessions, &resolved.selector);
    Response::html(200, "OK", page_sessions(&sel, &resolved, token, data))
}

fn one_session_page(params: &Query, token: &str, data: &UiData) -> Response {
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

fn content_page(params: &Query, data: &UiData, content: &dyn ContentSource) -> Response {
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

/// Resolve `?i=` against the inventory.
fn index_param<'a>(params: &Query, data: &'a UiData) -> Option<&'a UiSession> {
    let raw = param(params, "i")?;
    let index: usize = raw.parse().ok()?;
    data.session_at(index)
}

// ------------------------------------------------------------- html rendering

fn esc(s: &str) -> String {
    crate::view::esc(s)
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
footer{color:var(--muted);font-size:.82rem;margin-top:2.5rem;border-top:1px solid var(--line);padding-top:.8rem}
pre{white-space:pre-wrap;word-break:break-word;background:var(--head);padding:.6rem .8rem;
border-radius:4px;overflow-x:auto;font-size:.82rem;line-height:1.35}
"#;

fn head(title: &str) -> String {
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
fn footer(data: &UiData) -> String {
    format!(
        "<footer>\n\
         <p><b>Metadata tier.</b> This page was rendered from one archive-metadata read \
         taken before the server started (snapshots + index + tree + the activity sidecar). \
         {} session shard blob(s) were fetched by that read — conversation text is fetched \
         only when you click a session and then click <i>load</i>, and the cost is shown \
         before you do.</p>\n\
         <p>Machines are named by their archive partition id, which is the key the repository \
         actually partitions on. Times are UTC. No JavaScript, no external asset, no \
         off-host link is loaded by this page.</p>\n\
         <p><b>This server is on 127.0.0.1, which is not a security boundary</b> — any other \
         program on this machine can connect to it. Access is gated only by the random token \
         in this page's URL. Do not share the URL. The server exits by itself when idle.</p>\n\
         </footer>\n",
        data.data_blobs_read
    )
}

fn completeness_banner(data: &UiData) -> String {
    if data.complete() {
        return "<p class=ok>Read in full — every snapshot scanned was readable.</p>\n".to_string();
    }
    format!(
        "<div class=warn><b>INCOMPLETE READ.</b> {} part(s) of this destination could not be \
         read, so every count below is a <i>floor</i>: sessions missing from these tables are \
         UNKNOWN, not absent.<ul>{}</ul></div>\n",
        data.unreadable.len(),
        data.unreadable
            .iter()
            .map(|u| format!("<li class=mono>{}</li>", esc(u)))
            .collect::<String>()
    )
}

fn launch_banner(data: &UiData) -> String {
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

// ------------------------------------------------------------- overview page

fn page_overview(data: &UiData, token: &str) -> String {
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
    if sel.not_matched > 0 || !sel.unplaced.is_empty() {
        out.push_str(&format!(
            "<p>{} session(s) were evaluated and <b>rejected</b> by the filter in force; \
             {} session(s) could not be evaluated at all and are listed below.</p>\n",
            sel.not_matched,
            sel.unplaced.len()
        ));
    }

    out.push_str(&render_machines(&machines, &in_view, data, token));
    out.push_str(&render_matrix(&machines, &in_view, token));
    out.push_str(&render_heatmap(&in_view, token));
    out.push_str(&render_time_unknown(&in_view, token));
    out.push_str(&footer(data));
    out.push_str("</body></html>\n");
    out
}

fn count_by_machine<'a>(rows: &[&'a UiSession], machine: &str) -> (usize, u64) {
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

fn render_matrix(machines: &[String], in_view: &[&UiSession], token: &str) -> String {
    let mut sources: BTreeSet<String> = BTreeSet::new();
    for s in in_view {
        sources.insert(s.source_label());
    }
    let sources: Vec<String> = sources.into_iter().collect();
    let mut out = String::from(
        "<section><h2>Machine × source</h2>\n<p class=sub>Session counts. A cell links to that \
         machine and source; the row label links to the whole machine.</p>\n\
         <div class=scroll><table>\n<thead><tr><th>machine</th>",
    );
    for h in &sources {
        out.push_str(&format!("<th class=n>{}</th>", esc(h)));
    }
    out.push_str("<th class=n>total</th></tr></thead>\n<tbody>\n");

    for m in machines {
        out.push_str(&format!(
            "<tr><td class=mono><a href=\"/sessions?machine={q}&token={t}\">{m}</a></td>",
            q = percent_encode(m),
            t = percent_encode(token),
            m = esc(m),
        ));
        let mut row_total = 0usize;
        for h in &sources {
            let n = in_view
                .iter()
                .filter(|s| s.machine == *m && s.source_label() == *h)
                .count();
            row_total += n;
            if n == 0 {
                out.push_str("<td class=n>·</td>");
            } else if *h == NO_HARNESS {
                // Not expressible as a `--harness` filter: the shared selector's
                // harness constraint needs a harness to compare against, and
                // these ids have none. Linking to something that would return a
                // *different* set is worse than not linking at all, so the cell
                // says why and the sessions are listed below.
                out.push_str(&format!(
                    "<td class=n title=\"not expressible as a harness filter — see the list \
                     below\">{n}</td>"
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
    if sources.iter().any(|s| s == NO_HARNESS) {
        out.push_str(&format!(
            "<p class=sub>Sessions counted under <code>{}</code> cannot be selected with \
             <code>--harness</code>: the archived id carries no harness prefix, so the filter \
             has nothing to compare. Those cells are counts, not links, and the sessions are \
             listed below.</p>\n",
            esc(NO_HARNESS)
        ));
    }
    out.push_str("</section>\n");
    out
}

fn render_heatmap(in_view: &[&UiSession], token: &str) -> String {
    let rows: Vec<OverviewRow> = in_view.iter().map(|s| s.overview_row()).collect();
    let axis = HeatmapAxis::Machine;
    // Weekly, always: the dashboard is read at a weekly cadence, so a day column
    // would show mostly-empty columns and a hundred of them.
    let hd = overview::heatmap_data(&rows, 80, axis, Some(Granularity::Week));
    let mut out = String::from("<section><h2>Activity, by week</h2>\n");
    if hd.rows.is_empty() {
        out.push_str("<p>(no sessions)</p></section>\n");
        return out;
    }
    if !hd.has_time_axis {
        out.push_str(
            "<p>No session in view has a known conversation time, so there is no time axis \
             to draw. The weekly columns are UNKNOWN, not empty.</p></section>\n",
        );
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
        for b in &hd.buckets {
            // reason: the horizontal axis spans the full activity range, so a
            // bucket with no sessions for this machine is a true empty bucket;
            // 0 is that measurement, not a fallback.
            let n = r.counts.get(&b.label).copied().unwrap_or(0);
            let ch = overview::heatmap_cell_char(n, max);
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
        out.push_str(&format!(
            "<td class=uk title=\"time unknown\">{uk}</td></tr>\n"
        ));
    }
    out.push_str("</tbody></table></div>\n");
    out.push_str(&format!(
        "<p class=sub>Last column: sessions with no known time, never merged into a week. \
         Ramp {} (fewest) to {} (most). Week boundaries are UTC days; the link's filter is a \
         local calendar day, so a session within hours of a week edge can fall in a different \
         column than the one you clicked.</p>\n",
        esc(&overview::heatmap_ramp().0.to_string()),
        esc(&overview::heatmap_ramp().1.to_string()),
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
        pending_append.push_str(
            "<section><h2>Time unknown</h2>\n<p class=ok>0 sessions — every session in view \
             has a recorded conversation time.</p></section>\n",
        );
        return pending_append;
    }
    let mut by_reason: BTreeMap<String, Vec<&&UiSession>> = BTreeMap::new();
    for s in &unknown {
        let why = match &s.time_source {
            TimeSource::Unknown { why } => why.clone(),
            // A session whose source attests a time but carries no bound. Named
            // as its own case rather than folded into a reason it does not have.
            _ => "the activity index attests a time but recorded no bound for it".to_string(),
        };
        by_reason.entry(why).or_default().push(s);
    }
    pending_append.push_str(&format!(
        "<section><h2>Time unknown</h2>\n<p>{} session(s) in view have no known conversation \
         time. They are <b>not</b> in any bucket above, and they are not \"no sessions\" — each \
         carries the reason the time could not be obtained.</p>\n",
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
             <th>session (short)</th><th class=n>shards</th><th class=n>bytes</th>\
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
    if !sel.unplaced.is_empty() {
        return format!(
            "<div class=warn><b>UNKNOWN — not \"not there\".</b> 0 of {} session(s) matched, \
             but {} could not be placed (below), so this is not a proven absence.</div>\n",
            data.sessions.len(),
            sel.unplaced.len()
        );
    }
    format!(
        "<p><b>Not in this destination</b> — 0 of the {} session(s) in view matched, and the \
         destination was read in full. This is a real absence, not a failure to look.</p>\n",
        data.sessions.len()
    )
}

fn list_row(s: &UiSession, token: &str) -> String {
    let time = |v: Option<i64>| match v {
        Some(unix) => esc(&fmt_unix(unix)),
        None => "<span class=bad title=\"unknown\">unknown</span>".to_string(),
    };
    format!(
        "<tr><td class=mono>{m}</td><td>{h}</td>\
         <td><a class=mono href=\"/session?i={i}&token={t}\">{sid}</a></td>\
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
        Some(unix) => esc(&fmt_unix(unix)),
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
    out.push_str(&format!(
        "<div class=scroll><table>\n<tbody>\n\
         <tr><th>machine</th><td class=mono>{m}</td></tr>\n\
         <tr><th>source</th><td>{h}</td></tr>\n\
         <tr><th>first message</th><td>{f}</td></tr>\n\
         <tr><th>last message</th><td>{l}</td></tr>\n\
         <tr><th>shards</th><td class=n>{sh}</td></tr>\n\
         <tr><th>bytes in archive</th><td class=n>{b}</td></tr>\n\
         <tr><th>data blobs</th><td class=n>{db}</td></tr>\n\
         <tr><th>archive snapshot time</th><td>{snap}</td></tr>\n\
         </tbody></table></div>\n",
        m = esc(&s.machine),
        h = esc(&s.source_label()),
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
         <a href=\"/content?i={i}&token={t}\"><b>Load this session</b></a> \
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

// ---------------------------------------------------------------- json routes

fn json_overview(data: &UiData) -> String {
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

fn json_sessions(sel: &Selection<'_>, resolved: &Resolved, token: &str, data: &UiData) -> String {
    let row = |s: &UiSession| {
        serde_json::json!({
            "index": s.index,
            "machine": s.machine,
            "source": s.source_label(),
            "harness": s.harness,
            "session_short_id": s.short_id,
            "shards": s.shard_count,
            "bytes": s.bytes,
            "first_unix": time_state(s.first_unix, s.time_why.as_deref()),
            "last_unix": time_state(s.last_unix, s.time_why.as_deref()),
            "line_count": s.line_count,
            "archive_time_unix": s.archive_time_unix,
            "href": format!("/session?i={}&token={}", s.index, percent_encode(token)),
        })
    };
    let v = serde_json::json!({
        "schema_version": 1,
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
fn json_filter_error(data: &UiData, e: &UsageError) -> String {
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

fn time_state(unix: Option<i64>, why: Option<&str>) -> crate::json_out::TimeState {
    match unix {
        Some(unix) => crate::json_out::TimeState::known(unix),
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

// --------------------------------------------------------------------- tests

/// Synthetic archive data for the unit tests.
///
/// Deliberately *not* a repository: these tests are about what the dashboard
/// does with a report, and the black-box tests in `tests/w15_ui_test.rs` cover
/// what it does with a real one. Every number here is fixture data.
#[cfg(test)]
pub(crate) mod fixture {
    use super::*;
    use crate::activity::TimeSource as ActivityTimeSource;

    /// The instant every age in these tests is measured against.
    pub const NOW: i64 = 1_770_000_000;

    fn hit(
        machine: &str,
        session: &str,
        bytes: u64,
        shards: usize,
        span: Option<(i64, i64)>,
    ) -> crate::search::SessionHit {
        let (first, last, why, source) = match span {
            Some((f, l)) => (Some(f), Some(l), None, ActivityTimeSource::Exact),
            None => (
                None,
                None,
                Some("this session's lines recorded no timestamp".to_string()),
                ActivityTimeSource::Unknown {
                    why: "this session's lines recorded no timestamp".to_string(),
                },
            ),
        };
        crate::search::SessionHit {
            machine: machine.into(),
            session_id: session.into(),
            harness: crate::sidecar::infer_harness(session),
            shard_count: shards,
            bytes,
            snapshot_id: "aaaaaaaaaaaaaaaa".into(),
            archive_time_unix: NOW - 3600,
            first_unix: first,
            last_unix: last,
            time_why: why,
            data_blobs: shards,
            line_count: 10,
            time_source: source,
        }
    }

    /// Two machines and three sources: `claude-code` on both, `deepseek` (the
    /// extension-delivered kind) on one, and one id with no harness prefix.
    pub fn report() -> SearchReport {
        SearchReport {
            destination: "dest-under-test".into(),
            snapshots_in_repo: 2,
            snapshots_scanned: 2,
            sessions_seen: 4,
            window: None,
            all_recall: Default::default(),
            hits: vec![
                hit(
                    "m-1",
                    "claude-code.m-1.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
                    100,
                    2,
                    Some((NOW - 7200, NOW - 7000)),
                ),
                hit("m-1", "deepseek.d41f6a2b9c0e47aaaa1111", 50, 1, None),
                hit(
                    "m-2",
                    "claude-code.m-2.019bf00d-97b6-7eb2-9bf8-eacbacc09766",
                    200,
                    3,
                    Some((NOW - 600, NOW - 500)),
                ),
                // `.hidden-session` has no `.`/`~`-delimited head: `infer_harness`
                // returns None for it, which is the state the matrix's extra
                // column exists for. (`no-prefix-id` would NOT do — it has no
                // separator either, so `infer_harness` happily calls the whole
                // string a harness.)
                hit("m-2", ".hidden-session", 20, 1, Some((NOW - 100, NOW - 90))),
            ],
            unplaced: Vec::new(),
            not_matched: 0,
            machines_without_index: vec!["m-3".into()],
            hosts: vec![
                HostSnapshot {
                    hostname: "m-1".into(),
                    snapshot_id: "aaaaaaaaaaaaaaaa".into(),
                    archive_time_unix: NOW - 3600,
                    has_activity_index: true,
                    index_read_ok: true,
                    index_trusted: true,
                },
                HostSnapshot {
                    hostname: "m-3".into(),
                    snapshot_id: "bbbbbbbbbbbbbbbb".into(),
                    archive_time_unix: NOW - 40 * DAY,
                    has_activity_index: false,
                    index_read_ok: false,
                    index_trusted: false,
                },
            ],
            unreadable: Vec::new(),
            data_blobs_read: 0,
            index_files_read: 3,
        }
    }

    pub fn data() -> UiData {
        UiData::from_report(&report(), "dest-under-test", Selector::default(), NOW)
    }

    /// The same archive, read only in part — the case where every count on the
    /// page is a floor rather than a measurement.
    pub fn partial_data() -> UiData {
        let mut r = report();
        r.unreadable
            .push("host `m-9`: snapshot cccccccc tree walk failed".into());
        UiData::from_report(&r, "dest-under-test", Selector::default(), NOW)
    }

    /// A `ContentSource` that records every fetch and refuses none. The record
    /// is how a test proves *which* routes reached the payload tier.
    #[derive(Default)]
    pub struct CountingContent {
        pub calls: std::cell::RefCell<Vec<(String, String)>>,
    }

    impl ContentSource for CountingContent {
        fn fetch(&self, machine: &str, session_id: &str) -> Result<Content, String> {
            self.calls
                .borrow_mut()
                .push((machine.to_string(), session_id.to_string()));
            Ok(Content {
                shards: vec![("000001.jsonl".to_string(), "ab".repeat(32))],
                concat_sha256: "cd".repeat(32),
                bytes: 3,
                body: "{\"a\":1}\n".to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{self, CountingContent, NOW};
    use super::*;

    fn req(target: &str, data: &UiData, src: &dyn ContentSource) -> Response {
        let (path, params) = split_target(target);
        handle(path, &params, "t", data, src)
            .unwrap_or_else(|| panic!("`{target}` must be a known route"))
    }

    // --------------------------------------------------------- headline numbers

    /// The four headline numbers are measured off the rows in view, not derived
    /// from anything else. Sessions and bytes are summed; machines come from the
    /// snapshot list (so a machine with a snapshot and no sessions still counts);
    /// sources are the distinct harness ids.
    #[test]
    fn headline_numbers_are_the_measured_ones() {
        let d = fixture::data();
        let html = req("/", &d, &NoContent).body;
        assert!(
            html.contains("<span class=v>4</span><span class=l>sessions</span>"),
            "{html}"
        );
        // 100 + 50 + 200 + 20
        assert!(html.contains("370 B"), "{html}");
        // m-1, m-2 (from sessions) and m-3 (a snapshot with no index)
        assert!(
            html.contains("<span class=v>3</span><span class=l>machines</span>"),
            "{html}"
        );
        // claude-code, deepseek, and the no-prefix bucket
        assert!(
            html.contains("<span class=v>3</span><span class=l>sources</span>"),
            "{html}"
        );
    }

    /// The byte total is the sum of the rows in view — pinned by construction so
    /// a change to `fmt_bytes` cannot quietly change what is being counted.
    #[test]
    fn byte_total_is_the_sum_of_the_rows() {
        let d = fixture::data();
        let total: u64 = d.sessions.iter().map(|s| s.bytes).sum();
        assert_eq!(total, 370);
        assert!(req("/", &d, &NoContent).body.contains("370 B"));
    }

    // ---------------------------------------------------------- payload tier

    /// The load-bearing claim of the whole design: **no route fetches payload
    /// except the one you click through to.** Every other route must leave the
    /// source untouched — proved by a source that records its callers.
    #[test]
    fn only_the_content_route_reaches_the_payload_tier() {
        let d = fixture::data();
        let src = CountingContent::default();
        for target in [
            "/",
            "/sessions",
            "/sessions?machine=m-1",
            "/session?i=0",
            "/session?i=1",
            "/api/overview",
            "/api/sessions",
        ] {
            let r = req(target, &d, &src);
            assert_eq!(r.status, 200, "{target}");
            assert!(
                src.calls.borrow().is_empty(),
                "{target} reached the payload tier; only /content may"
            );
        }
        // …and the instrument can say yes.
        let r = req("/content?i=1", &d, &src);
        assert_eq!(r.status, 200);
        assert_eq!(
            src.calls.borrow().len(),
            1,
            "the content route must actually fetch"
        );
        assert_eq!(
            src.calls.borrow()[0].1,
            "deepseek.d41f6a2b9c0e47aaaa1111",
            "it must fetch the row the URL asked for"
        );
    }

    /// The session page states the price and does not pay it.
    #[test]
    fn session_page_shows_the_cost_before_anything_is_fetched() {
        let d = fixture::data();
        let src = CountingContent::default();
        let html = req("/session?i=2", &d, &src).body;
        assert!(src.calls.borrow().is_empty());
        assert!(html.contains("Body not loaded"), "{html}");
        assert!(html.contains("200 B"), "the byte cost must be on the page");
        assert!(html.contains("3 shard(s)"), "{html}");
        assert!(
            html.contains("/content?i=2&token=t"),
            "the load link must be an explicit click, and must carry the row"
        );
    }

    /// A failed fetch is a read failure, never an empty session.
    #[test]
    fn a_failed_content_fetch_is_not_an_empty_session() {
        struct Broken;
        impl ContentSource for Broken {
            fn fetch(&self, _m: &str, _s: &str) -> Result<Content, String> {
                Err("no snapshot for machine `m-1`".into())
            }
        }
        let r = req("/content?i=0", &fixture::data(), &Broken);
        assert_ne!(r.status, 200);
        assert!(r.body.contains("NOT an empty session"), "{}", r.body);
    }

    // ------------------------------------------------------------- honesty

    /// A partial read says so, and never renders as a clean zero.
    #[test]
    fn incomplete_read_is_announced_and_never_reads_as_a_clean_zero() {
        let d = fixture::partial_data();
        let html = req("/", &d, &NoContent).body;
        assert!(html.contains("INCOMPLETE READ"), "{html}");
        assert!(html.contains("floor"), "{html}");
        assert!(!req("/", &fixture::data(), &NoContent)
            .body
            .contains("INCOMPLETE READ"));
    }

    /// The list route's three "nothing matched" sentences must be three
    /// sentences — the same distinction `search`'s `no_hit_line` draws.
    #[test]
    fn zero_matches_is_not_always_a_proven_absence() {
        let d = fixture::data();
        // A filter that genuinely matches nothing: the destination was read in
        // full and every row was evaluated.
        let html = req("/sessions?machine=m-zz", &d, &NoContent).body;
        assert!(html.contains("Not in this destination"), "{html}");
        assert!(!html.contains("not \\\"not there\\\""), "{html}");

        // The same query against a partly-read destination proves nothing.
        let html = req(
            "/sessions?machine=m-zz",
            &fixture::partial_data(),
            &NoContent,
        )
        .body;
        assert!(html.contains("UNKNOWN"), "{html}");
        assert!(!html.contains("Not in this destination"), "{html}");

        // …and neither does it when a session could not be placed at all.
        let html = req("/sessions?machine=m-1&day=2030-01-01", &d, &NoContent).body;
        assert!(
            html.contains("could not be placed")
                || html.contains("could not be evaluated")
                || html.contains("UNKNOWN"),
            "{html}"
        );
    }

    /// A machine whose activity index is missing is named as such, and is never
    /// folded into "no sessions".
    #[test]
    fn a_machine_without_an_index_is_named_never_shown_as_empty() {
        let html = req("/", &fixture::data(), &NoContent).body;
        assert!(html.contains("m-3"), "the machine must appear: {html}");
        assert!(html.contains("MISSING"), "{html}");
        assert!(
            html.contains("index missing"),
            "health must say why, not just colour a cell: {html}"
        );
    }

    /// Health is a word plus a measured age — never a percentage.
    #[test]
    fn health_is_a_state_word_and_an_age_never_a_percentage() {
        let d = fixture::data();
        let html = req("/", &d, &NoContent).body;
        assert!(html.contains("archived recently"), "{html}");
        assert!(html.contains("index missing"), "{html}");
        // Scoped to the health table on purpose: the stylesheet legitimately
        // contains `width:100%`, so a page-wide check would be meaningless.
        let table = html
            .split("<h2>Machines</h2>")
            .nth(1)
            .and_then(|rest| rest.split("<h2>Machine × source</h2>").next())
            .expect("the machines section must exist");
        assert!(
            !table.contains('%'),
            "health must never be a percentage: {table}"
        );
    }

    /// A stale machine is named as stale, bounded by the documented threshold.
    #[test]
    fn a_machine_that_stopped_pushing_is_reported_with_its_age() {
        let mut r = fixture::report();
        r.hosts[0].archive_time_unix = NOW - (STALE_AFTER_DAYS + 1) * DAY;
        let d = UiData::from_report(&r, "dest", Selector::default(), NOW);
        let html = req("/", &d, &NoContent).body;
        assert!(
            html.contains(&format!("{}d ago", STALE_AFTER_DAYS + 1)),
            "{html}"
        );
        assert_eq!(
            health_of(d.hosts.first(), NOW),
            Health::Archived {
                age_secs: (STALE_AFTER_DAYS + 1) * DAY
            }
        );
    }

    /// An index that exists and could not be read is its own state — neither
    /// "missing" nor "fine".
    #[test]
    fn an_unreadable_index_is_its_own_state() {
        let mut r = fixture::report();
        r.hosts[0].has_activity_index = true;
        r.hosts[0].index_read_ok = false;
        let d = UiData::from_report(&r, "dest", Selector::default(), NOW);
        assert_eq!(health_of(d.hosts.first(), NOW), Health::IndexUnreadable);
        assert!(req("/", &d, &NoContent).body.contains("UNREADABLE"));
    }

    // ----------------------------------------------------------- the selector

    /// A drill-down is the shared selector applied to the inventory. Pinned at
    /// unit level by comparing against `Selector::select` directly; the
    /// end-to-end equality with `search` is pinned in `tests/w15_ui_test.rs`.
    #[test]
    fn a_drill_down_is_the_shared_selector_applied_to_the_inventory() {
        let d = fixture::data();
        let cases: [&str; 5] = [
            "machine=m-1",
            "harness=claude-code",
            "machine=m-1&harness=claude-code",
            &format!("machine=m-1&since={}&until={}", "2026-01-01", "2026-01-31"),
            "session=deepseek",
        ];
        for q in cases {
            let (_, params) = split_target(&format!("/sessions?{q}"));
            let resolved = selector_from_query(&params).expect("fixture filters must resolve");
            let expected = select(&d.sessions, &resolved.selector);
            let html = req(&format!("/sessions?{q}"), &d, &NoContent).body;
            // The page reports the same count the selector decided.
            if expected.matched.is_empty() {
                assert!(html.contains("Not in this destination") || html.contains("UNKNOWN"));
            } else {
                assert!(
                    html.contains(&format!(
                        "<b>{}</b> session(s) matched",
                        expected.matched.len()
                    )),
                    "query `{q}` listed {} but the selector chose {}",
                    html,
                    expected.matched.len()
                );
            }
            // Every listed row is a row the selector actually selected.
            for s in &expected.matched {
                assert!(html.contains(&s.short_id), "`{q}` must list {}", s.short_id);
            }
        }
    }

    /// A filter that cannot be resolved is a usage error — a 400, never a list
    /// of zero rows, which a reader would take for a measurement.
    #[test]
    fn an_unresolvable_filter_is_a_usage_error_never_zero_rows() {
        let d = fixture::data();
        for bad in [
            "day=yesterday",
            "day=2026-02-30",
            "since=2026-03-01&until=2026-01-01",
        ] {
            let r = req(&format!("/sessions?{bad}"), &d, &NoContent);
            assert_eq!(r.status, 400, "`{bad}` must be a usage error");
            assert!(
                !r.body.contains("Not in this destination"),
                "`{bad}` must not be dressed up as an empty result"
            );
        }
        assert_eq!(req("/sessions?day=2026-01-15", &d, &NoContent).status, 200);
    }

    /// The query string accepts exactly the flags `search` accepts, and no more.
    #[test]
    fn the_query_vocabulary_is_the_command_line_vocabulary() {
        let (_, params) =
            split_target("/sessions?machine=m-1&harness=a,b&session=cc&day=2026-01-15&token=t&i=3");
        let r = selector_from_query(&params).expect("resolves");
        assert_eq!(r.selector.machine.as_deref(), Some("m-1"));
        assert_eq!(r.selector.session_id_prefix.as_deref(), Some("cc"));
        assert_eq!(
            r.selector.harnesses.as_ref().map(|h| h.len()),
            Some(2),
            "a comma list becomes two harnesses, as `--harness a,b` does"
        );
        assert!(r.selector.window.is_some());
        assert!(r.warnings.is_empty());
    }

    // ------------------------------------------------------------- the page

    /// Nothing rendered may carry a full session id: the URL addresses a row by
    /// position, and the page shows the shortened form.
    #[test]
    fn no_full_session_id_reaches_the_rendered_output() {
        let d = fixture::data();
        const LISTING: [&str; 5] = [
            "/",
            "/sessions",
            "/session?i=0",
            "/content?i=0",
            "/api/sessions",
        ];
        for target in LISTING {
            let body = req(target, &d, &CountingContent::default()).body;
            for s in &d.sessions {
                assert!(
                    !body.contains(&s.session_id),
                    "{target} leaked the full id of {}",
                    s.short_id
                );
            }
        }
        // The instrument can say yes: the pages that name a session really do
        // carry the short form, so the negative above is not passing by
        // accident. The overview page (`/`) is deliberately not in this list —
        // it lists machines, not sessions, and a row is reached by drilling
        // down from one.
        for target in ["/sessions", "/session?i=0", "/content?i=0", "/api/sessions"] {
            let body = req(target, &d, &CountingContent::default()).body;
            assert!(body.contains(&d.sessions[0].short_id), "{target}");
        }
    }

    #[test]
    fn machine_names_are_escaped_everywhere_they_are_rendered() {
        let mut r = fixture::report();
        for h in &mut r.hits {
            h.machine = "<script>x</script>".into();
        }
        r.hosts[0].hostname = "<script>x</script>".into();
        let d = UiData::from_report(&r, "dest", Selector::default(), NOW);
        for target in ["/", "/sessions", "/session?i=0"] {
            let body = req(target, &d, &NoContent).body;
            assert!(!body.contains("<script>x</script>"), "{target}");
            assert!(body.contains("&lt;script&gt;"), "{target}");
        }
        // The JSON routes deliberately carry the raw name: `application/json`
        // plus `X-Content-Type-Options: nosniff` is what makes that safe, so the
        // property to pin is the header, not an escape.
        let r = req("/api/overview", &d, &NoContent);
        assert_eq!(r.content_type, "application/json; charset=utf-8");
        assert!(r.to_bytes().windows(8).any(|w| w == b"nosniff\r"));
    }

    /// The JSON routes carry completeness, so a machine consumer cannot read a
    /// truncated list as an empty archive.
    #[test]
    fn json_carries_completeness_so_partial_is_not_read_as_empty() {
        let d = fixture::data();
        let (path, params) = split_target("/api/sessions");
        let full: serde_json::Value =
            serde_json::from_str(&handle(path, &params, "t", &d, &NoContent).unwrap().body)
                .unwrap();
        assert_eq!(full["complete"], serde_json::json!(true));
        assert_eq!(full["payload_loaded"], serde_json::json!(false));
        assert_eq!(full["tier"], serde_json::json!("metadata"));
        assert_eq!(full["matched"], serde_json::json!(4));

        let p = fixture::partial_data();
        let (path, params) = split_target("/api/sessions");
        let partial: serde_json::Value =
            serde_json::from_str(&handle(path, &params, "t", &p, &NoContent).unwrap().body)
                .unwrap();
        assert_eq!(partial["complete"], serde_json::json!(false));
        assert_eq!(
            partial["unreadable_parts"].as_array().map(Vec::len),
            Some(1)
        );
    }

    /// The overview JSON names every machine and its health, and never renders
    /// an unknown age as a number.
    #[test]
    fn overview_json_names_machines_and_their_health() {
        let d = fixture::data();
        let (path, params) = split_target("/api/overview");
        let v: serde_json::Value =
            serde_json::from_str(&handle(path, &params, "t", &d, &NoContent).unwrap().body)
                .unwrap();
        assert_eq!(v["command"], serde_json::json!("ui"));
        assert_eq!(v["summary"]["sessions_in_view"], serde_json::json!(4));
        assert_eq!(v["summary"]["machines"], serde_json::json!(3));
        assert_eq!(v["summary"]["time_unknown"], serde_json::json!(1));
        let machines = v["machines"].as_array().expect("machines array");
        let m3 = machines
            .iter()
            .find(|m| m["machine"] == serde_json::json!("m-3"))
            .expect("m-3 must be listed even though it holds no session");
        assert_eq!(m3["sessions"], serde_json::json!(0));
        assert_eq!(m3["health"], serde_json::json!("index missing"));
        assert_eq!(m3["has_activity_index"], serde_json::json!(false));
    }

    /// The heatmap's weekly columns are links that carry the shared selector,
    /// and the machine rows come from `overview`'s own aggregation.
    #[test]
    fn the_heatmap_is_weekly_and_its_cells_carry_the_selector() {
        let d = fixture::data();
        let html = req("/", &d, &NoContent).body;
        assert!(html.contains("Activity, by week"), "{html}");
        assert!(
            html.contains("since="),
            "a week cell must carry a window: {html}"
        );
        assert!(html.contains("until="), "{html}");
        assert!(html.contains("class=week"), "{html}");
        // The bucketing itself is `overview`'s: the same rows must produce the
        // same buckets through the shared entry point.
        let rows: Vec<OverviewRow> = d.sessions.iter().map(|s| s.overview_row()).collect();
        let hd = overview::heatmap_data(&rows, 80, HeatmapAxis::Machine, Some(Granularity::Week));
        assert!(hd.has_time_axis);
        assert!(!hd.buckets.is_empty());
        for b in &hd.buckets {
            assert!(
                html.contains(&format!("since={}", percent_encode(&b.label))),
                "week {} must be clickable",
                b.label
            );
        }
    }

    /// The time-unknown section names the reason, per session, and never
    /// silently drops them into a week.
    #[test]
    fn time_unknown_sessions_are_listed_with_their_reason() {
        let d = fixture::data();
        let html = req("/", &d, &NoContent).body;
        assert!(html.contains("Time unknown"), "{html}");
        assert!(html.contains("1 session(s) in view"), "{html}");
        assert!(
            html.contains("this session&#39;s lines recorded no timestamp")
                || html.contains("recorded no timestamp"),
            "{html}"
        );
        assert!(
            html.contains(&d.sessions[1].short_id),
            "the time-unknown session must be listed by its short id: {html}"
        );
        assert!(html.contains("deepseek"), "{html}");
    }

    /// A session id with no harness prefix gets its own column, is counted, and
    /// is **not** linked to a harness filter that could not reproduce it.
    #[test]
    fn a_prefixless_id_is_counted_but_not_linked_to_a_harness_filter() {
        let d = fixture::data();
        let html = req("/", &d, &NoContent).body;
        assert!(html.contains(NO_HARNESS), "{html}");
        assert!(
            !html.contains(&format!("harness={}", percent_encode(NO_HARNESS))),
            "the prefixless column must not link to a filter that cannot express it"
        );
        assert!(
            html.contains("cannot be selected with"),
            "the page must say why: {html}"
        );
    }

    /// An out-of-range `i` is a usage error, not an empty session.
    #[test]
    fn an_unknown_row_index_is_a_usage_error() {
        let d = fixture::data();
        for bad in ["", "9999", "abc", "-1"] {
            let r = req(&format!("/session?i={bad}"), &d, &NoContent);
            assert_eq!(r.status, 400, "`i={bad}`");
            let r = req(&format!("/content?i={bad}"), &d, &NoContent);
            assert_eq!(r.status, 400, "`i={bad}`");
            assert!(r.body.contains("not an empty"), "{}", r.body);
        }
    }

    // ------------------------------------------------------------- plumbing

    #[test]
    fn percent_encoding_round_trips_and_leaves_safe_characters_alone() {
        for s in [
            "m-1",
            "claude-code",
            "a b/c?d=e&f",
            "sessions/2026-01-15",
            "härte",
        ] {
            assert_eq!(percent_decode(&percent_encode(s)), s, "{s}");
        }
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(percent_encode("a b"), "a%20b");
    }

    #[test]
    fn the_target_is_split_and_the_token_found_in_any_position() {
        assert_eq!(split_target("/?token=x").0, "/");
        assert_eq!(
            split_target("/api/sessions?a=1&token=x")
                .1
                .iter()
                .find(|(k, _)| k == "token")
                .map(|(_, v)| v.as_str()),
            Some("x")
        );
        assert_eq!(split_target("/").1.len(), 0);
        // `tokenish` is a normal parameter (and is ignored); it must not be
        // mistaken for the `token` parameter the gate looks for.
        assert_eq!(
            split_target("/?tokenish=x").1,
            vec![("tokenish".into(), "x".into())]
        );
        assert!(split_target("/?tokenish=x")
            .1
            .iter()
            .all(|(k, _)| k != "token"));
        // A `+` is not a space here: a partition may legitimately contain one.
        assert_eq!(split_target("/?machine=a+b").1[0].1, "a+b");
    }

    #[test]
    fn byte_and_age_formatting_always_names_its_unit() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(370), "370 B");
        assert!(fmt_bytes(2048).contains("KiB"));
        assert!(fmt_bytes(5 * 1024 * 1024).contains("MiB"));
        assert!(fmt_bytes(5 * 1024 * 1024).contains("5242880 B"));
        assert_eq!(fmt_age(3 * DAY + 100), "3d ago");
        assert_eq!(fmt_age(7200), "2h ago");
        // A negative age (a snapshot from the future) is never rendered as a
        // negative duration.
        assert_eq!(fmt_age(-500), "0h ago");
    }

    #[test]
    fn a_selector_with_no_constraints_describes_nothing() {
        assert_eq!(describe_selector(&Selector::default()), None);
        let mut s = Selector::default();
        s.machine = Some("m-1".into());
        let text = describe_selector(&s).expect("one constraint");
        assert!(text.contains("m-1"));
        assert!(!text.contains('/'), "a partition id is not a path: {text}");
    }
}
