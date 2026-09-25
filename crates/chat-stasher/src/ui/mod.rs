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
//!
//! The rendering is split one module per route family so a change to one
//! surface moves one file:
//!
//! * [`html`] — escaping, byte/instant/age formatting, the stylesheet and the
//!   page chrome (`head`, `footer`, the two banners, `describe_selector`).
//! * [`overview`] — `/`, the machine table, the machine × source matrix, the
//!   weekly heatmap and the time-unknown lists.
//! * [`sessions`] — `/sessions`, `/session` and the raw `/content` view.
//! * [`reader`] — `/reader` and the message-block rendering.
//! * [`json`] — `/api/overview` and `/api/sessions`.
//! * [`facets`] — skeleton for the facet bar (29-UI-DESIGN §2, not yet built).
//!
//! Everything below is the shared model, the selector bridge and the router.

use std::collections::BTreeSet;

use crate::activity::TimeSource;
use crate::overview::OverviewRow;
use crate::search::{HostSnapshot, SearchReport, SessionLabel};
use crate::selector::{
    Resolved, Selector, SelectorArgs, SessionMeta, TimeBounds, UnplacedBy, UsageError, Verdict,
};
use crate::view::Response;

pub(crate) mod facets;
pub(crate) mod html;
pub(crate) mod json;
pub(crate) mod overview;
pub(crate) mod reader;
pub(crate) mod sessions;

pub use html::describe_selector;

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
    /// The label this session lists under, resolved from its index row with
    /// the same honesty states as [`SessionLabel`] documents.
    pub title: SessionLabel,
    pub provenance: Option<crate::activity::ProjectProvenance>,
    pub line_count: u64,
    /// The snapshot's own time — the backup run, not the conversation's.
    pub archive_time_unix: i64,
    pub data_blobs: usize,
}

/// The label a session with no harness prefix gets in the matrix. Not a harness
/// id, and deliberately shaped so it cannot be mistaken for one.
pub const NO_HARNESS: &str = "(no harness prefix)";

/// The destination label `cmd_ui` passes when the dashboard was opened with
/// `--repo` instead of a declared destination name. Shared by value between
/// the two modules on purpose — a shared *string* without a shared constant
/// is how a page ends up comparing against a word nobody owns.
pub const EXPLICIT_REPO_LABEL: &str = "(explicit --repo)";

/// The two honest words a known label's provenance renders as — one sentence
/// each so the reader never has to guess whether the harness wrote the label
/// or we quoted it from the conversation. Constants (not rebuilt strings)
/// because they also travel into the cells' title attributes verbatim.
const PROVENANCE_HARNESS_TITLE: &str = "label source: the harness's own title";
const PROVENANCE_FIRST_USER_LINE: &str = "label source: the first user line";

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
            time_source: crate::overview::TimeSource::from(&self.time_source),
            provenance: self.provenance.clone(),
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
            // The index's own answer, never re-derived from the bounds.
            time_bounds: if self.time_source.bounds_are_partial() {
                TimeBounds::Partial
            } else {
                TimeBounds::Complete
            },
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
    /// Machines whose activity index predates labels: their sessions' labels
    /// read as unknown, and the list page explains each such machine once —
    /// never once per row.
    pub machines_with_legacy_index: Vec<String>,
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
                title: h.title.clone(),
                provenance: h.provenance.clone(),
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
            machines_with_legacy_index: report.machines_with_legacy_index.clone(),
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
        "/" => Some(Response::html(
            200,
            "OK",
            overview::page_overview(data, token),
        )),
        "/sessions" => Some(sessions::list_page(params, token, data)),
        "/session" => Some(sessions::one_session_page(params, token, data)),
        "/content" => Some(sessions::content_page(params, data, content)),
        "/reader" => Some(reader::reader_page(params, token, data, content)),
        "/api/overview" => Some(Response::json(200, "OK", json::json_overview(data))),
        "/api/sessions" => Some(match selector_from_query(params) {
            Ok(r) => Response::json(
                200,
                "OK",
                json::json_sessions(&select(&data.sessions, &r.selector), &r, token, data),
            ),
            Err(e) => Response::json(400, "Bad Request", json::json_filter_error(data, &e)),
        }),
        _ => None,
    }
}

pub(super) fn index_param<'a>(params: &Query, data: &'a UiData) -> Option<&'a UiSession> {
    let raw = param(params, "i")?;
    let index: usize = raw.parse().ok()?;
    data.session_at(index)
}

/// Every route this server answers, in the order the 404 body names them.
///
/// The one place the route table lives: `view::route` builds its 404 wording
/// from this, and the router test proves [`handle`] answers each of them.
pub const ROUTES: [&str; 7] = [
    "/",
    "/sessions",
    "/session",
    "/reader",
    "/content",
    "/api/overview",
    "/api/sessions",
];

/// The 404 body `view::route` sends when [`handle`] has no route for a path.
pub fn no_route_message() -> String {
    format!("ui: no such route ({})\n", ROUTES.join(", "))
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
            title: crate::search::SessionLabel::NoLabelRecorded,
            provenance: None,
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
            machines_with_legacy_index: Vec::new(),
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

    /// `m-3` as `search` can actually report it — a machine only enters the
    /// no-index list once it holds sessions (search.rs fills the same field
    /// the JSON field `machines_without_activity_index` carries). One session
    /// whose conversation time is unknown *because the machine has no
    /// activity index*: the why string rebuilt the same way `search_sessions`
    /// builds it.
    pub fn no_index_data() -> UiData {
        let mut r = report();
        let machine = "m-3";
        let why = format!(
            "machine `{machine}` has no activity index (`meta/{machine}/activity-v1.jsonl`) \
             in this snapshot, so its conversation time was never recorded"
        );
        let mut h = hit(
            machine,
            "claude-code.m-3.019bf00d-97b6-7eb2-9bf8-eacbacc09767",
            80,
            1,
            None,
        );
        h.time_why = Some(why.clone());
        h.time_source = ActivityTimeSource::Unknown { why };
        r.hits.push(h);
        r.sessions_seen += 1;
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
    use super::html::{fmt_age, fmt_bytes};
    use super::reader::reader_window;
    use super::*;

    use crate::activity::TitleSource;
    use crate::overview::{Granularity, HeatmapAxis};

    fn req(target: &str, data: &UiData, src: &dyn ContentSource) -> Response {
        let (path, params) = split_target(target);
        handle(path, &params, "t", data, src)
            .unwrap_or_else(|| panic!("`{target}` must be a known route"))
    }

    /// The route table, the 404 wording and the router are one list: every
    /// route [`ROUTES`] names is answered by [`handle`] and appears verbatim in
    /// [`no_route_message`], and an unknown path is answered by neither.
    #[test]
    fn the_route_table_the_404_body_and_the_router_agree() {
        let d = fixture::data();
        for route in ROUTES {
            let (path, params) = split_target(route);
            assert!(
                handle(path, &params, "t", &d, &NoContent).is_some(),
                "`{route}` is in ROUTES but the router does not answer it"
            );
            assert!(
                no_route_message().contains(route),
                "`{route}` is in ROUTES but not named in the 404 body"
            );
        }
        let (path, params) = split_target("/no-such-route");
        assert!(
            handle(path, &params, "t", &d, &NoContent).is_none(),
            "a path outside ROUTES must not be answered"
        );
        assert!(no_route_message().starts_with("ui: no such route ("));
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

    #[test]
    fn session_page_shows_later_project_and_preserves_unknown_capture_fact() {
        let mut d = fixture::data();
        d.sessions[0].provenance = Some(crate::activity::ProjectProvenance {
            captured: Some(
                serde_json::json!({"workspace":"unknown","project":"unknown","archived":false}),
            ),
            effective_project: Some(
                serde_json::json!({"id":"project-fixture","name":"Synthetic Project"}),
            ),
            supplement: Some(serde_json::json!({
                "workspace":"workspace-fixture",
                "project":{"id":"project-fixture","name":"Synthetic Project"},
                "source":"project-list",
                "observedAt":"2026-09-25T12:00:00.000Z"
            })),
        });
        let page = req("/session?i=0", &d, &NoContent).body;
        assert!(
            page.contains("Synthetic Project"),
            "effective project must be visible"
        );
        assert!(
            page.contains("learned later from project-list at 2026-09-25T12:00:00.000Z"),
            "source and observation time must be visible"
        );
        assert!(
            page.contains("capture recorded project: unknown"),
            "the immutable capture-time unknown must remain visible"
        );
    }

    /// The same sentence, for a capture that recorded **nothing**: an archive
    /// written before provenance existed, later given a supplement. "We never
    /// wrote it down" is not "it was written down as unknown" (CLAUDE.md
    /// invariant 1), and this sentence is the one ADR-043 mandates, so it has to
    /// keep them apart — the no-supplement branch already says "not recorded".
    #[test]
    fn session_page_says_not_recorded_when_the_capture_recorded_no_provenance() {
        let mut d = fixture::data();
        d.sessions[0].provenance = Some(crate::activity::ProjectProvenance {
            captured: None,
            effective_project: Some(
                serde_json::json!({"id":"project-fixture","name":"Synthetic Project"}),
            ),
            supplement: Some(serde_json::json!({
                "workspace":"workspace-fixture",
                "project":{"id":"project-fixture","name":"Synthetic Project"},
                "source":"project-list",
                "observedAt":"2026-09-25T12:00:00.000Z"
            })),
        });
        let page = req("/session?i=0", &d, &NoContent).body;
        assert!(
            page.contains("capture recorded project: not recorded"),
            "a capture that recorded no provenance must say so: {page}"
        );
        assert!(
            !page.contains("capture recorded project: unknown"),
            "a capture that recorded nothing is not a capture that recorded \
             unknown: {page}"
        );
    }

    /// `GET /api/sessions` is the machine-readable half of the same page, and it
    /// must carry provenance too — the CTO's "shown by the CLI / UI" surface is both. A
    /// session with no record carries no key, so a consumer cannot read "not
    /// written down" as "no project".
    #[test]
    fn api_sessions_json_carries_project_provenance() {
        let mut d = fixture::data();
        d.sessions[0].provenance = Some(crate::activity::ProjectProvenance {
            captured: Some(
                serde_json::json!({"workspace":"unknown","project":"unknown","archived":false}),
            ),
            effective_project: Some(
                serde_json::json!({"id":"project-fixture","name":"Synthetic Project"}),
            ),
            supplement: Some(serde_json::json!({
                "workspace":"workspace-fixture",
                "project":{"id":"project-fixture","name":"Synthetic Project"},
                "source":"project-list",
                "observedAt":"2026-09-25T12:00:00.000Z"
            })),
        });
        let body = req("/api/sessions", &d, &NoContent).body;
        let v: serde_json::Value = serde_json::from_str(&body).expect("the route must send JSON");
        let sessions = v["sessions"].as_array().unwrap();
        assert_eq!(
            sessions[0]["provenance"]["captured"]["project"], "unknown",
            "the capture-time fact must travel unchanged, marker included"
        );
        assert_eq!(
            sessions[0]["provenance"]["effectiveProject"]["name"], "Synthetic Project",
            "the later attribution is what the API shows as the project"
        );
        assert_eq!(
            sessions[0]["provenance"]["supplement"]["source"],
            "project-list"
        );
        assert_eq!(
            sessions[0]["provenance"]["supplement"]["observedAt"],
            "2026-09-25T12:00:00.000Z"
        );
        assert!(
            sessions[1].get("provenance").is_none(),
            "a session with no provenance record carries no key at all, never null: {}",
            sessions[1]
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
    fn only_the_body_routes_reach_the_payload_tier() {
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
                "{target} reached the payload tier; only /content and /reader may"
            );
        }
        // …and the instrument can say yes, once per body route, for the row the
        // URL asked for.
        for target in ["/content?i=1", "/reader?i=1"] {
            let r = req(target, &d, &src);
            assert_eq!(r.status, 200, "{target}");
            assert_eq!(
                src.calls.borrow().len(),
                1,
                "{target} must fetch exactly once"
            );
            assert_eq!(
                src.calls.borrow()[0].1,
                "deepseek.d41f6a2b9c0e47aaaa1111",
                "{target} must fetch the row the URL asked for"
            );
            src.calls.borrow_mut().clear();
        }
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

    #[test]
    fn reader_renders_roles_markdown_code_tools_attachments_and_safe_html() {
        struct ReaderSource;
        impl ContentSource for ReaderSource {
            fn fetch(&self, _machine: &str, _session_id: &str) -> Result<Content, String> {
                // The matrix this test group exists for is W139's four
                // vectors: `script`, `onerror=`, `javascript:` and an
                // over-wide base64 payload. Two are in the first line; the
                // last two are below, because an `onerror=` handler only fires
                // on a real element and a wide payload is the case where a
                // truncating renderer would be tempted to emit a fragment.
                let mut body = concat!(
                    r##"{"type":"user","timestamp":"2026-09-25T10:00:00Z","message":{"role":"user","content":"# Question\n\n**hello** <script>alert(1)</script>\n[javascript](javascript:alert(1))"}}"##,
                    "\n",
                    r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"code","language":"rust","code":"fn main() {}"},{"type":"thinking","thinking":"private"},{"type":"tool_use","name":"search","input":{"q":"x"}},{"type":"image","name":"plot.png","content_type":"image/png","bytes":12}]}}"#,
                    "\n",
                    r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"ok"}]}}"#,
                    "\n",
                    r#"{"type":"user","message":{"role":"user","content":"<img src=x onerror=alert(1)>"}}"#,
                    "\n",
                )
                .to_string();
                // A `data:` target carrying 8 KiB of base64 — wide enough that
                // an eliding or splitting renderer would cut it, and a target
                // the link predicate must print rather than emit.
                body.push_str(
                    "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\
                     \"[wide](data:text/html;base64,",
                );
                body.push_str(&"QUFB".repeat(2048));
                body.push_str(")\"}}\n");
                Ok(Content {
                    shards: Vec::new(),
                    concat_sha256: "aa".repeat(32),
                    bytes: body.len(),
                    body,
                })
            }
        }
        let response = req("/reader?i=0", &fixture::data(), &ReaderSource);
        assert_eq!(response.status, 200);
        assert!(
            response.body.contains("<h1>Question</h1>"),
            "{}",
            response.body
        );
        assert!(response.body.contains("User"), "{}", response.body);
        assert!(response.body.contains("Assistant"), "{}", response.body);
        // An RFC 3339 timestamp is recorded, and the header says so.
        assert!(
            response.body.contains("</time> · exact"),
            "{}",
            response.body
        );
        assert!(response.body.contains("Tool"), "{}", response.body);
        assert!(
            response.body.contains("<details class=thinking>"),
            "{}",
            response.body
        );
        assert!(
            response.body.contains("<details class=tool>"),
            "{}",
            response.body
        );
        // A `tool_use` names a call; the output arrives in the `tool_result`
        // that follows. The call's own size is unrecorded, not zero.
        assert!(
            response
                .body
                .contains("Tool call: search · output not recorded"),
            "{}",
            response.body
        );
        // …and the result that *was* archived reports its measured size.
        assert!(
            response.body.contains("Tool call: call-1 · output 2 B"),
            "{}",
            response.body
        );
        assert!(
            response.body.contains("Attachment reference"),
            "{}",
            response.body
        );
        assert!(
            response.body.contains("&lt;script&gt;"),
            "{}",
            response.body
        );
        assert!(
            !response.body.contains("<script>alert"),
            "{}",
            response.body
        );
        assert!(
            !response.body.contains("href=\"javascript:"),
            "{}",
            response.body
        );
        // W139's other two vectors. An `onerror=` handler needs a real
        // element, and no element is built from archived text: the payload is
        // shown, escaped, and the page holds no tag for the handler to hang
        // on. The literal substring `src=` still appears — inside the escaped
        // text, which is the point — so the assertion is about tags, not
        // substrings.
        assert!(
            response.body.contains("&lt;img src=x onerror=alert(1)&gt;"),
            "the onerror payload is shown escaped: {}",
            response.body
        );
        assert!(!response.body.contains("<img"), "{}", response.body);
        // The over-wide base64 target stays text, whole, and not a link.
        assert!(
            response.body.contains("wide (data:text/html;base64,"),
            "{}",
            response.body
        );
        assert!(
            response.body.contains("QUFB)"),
            "the wide payload is rendered to its end, not cut: {}",
            response.body
        );
        assert!(!response.body.contains("href=\"data:"), "{}", response.body);
        assert!(
            !response.body.contains("<a href=\"data"),
            "{}",
            response.body
        );
    }

    #[test]
    fn reader_keeps_unknown_time_and_provenance_visible() {
        struct UnknownSource;
        impl ContentSource for UnknownSource {
            fn fetch(&self, _machine: &str, _session_id: &str) -> Result<Content, String> {
                Ok(Content {
                    shards: Vec::new(),
                    concat_sha256: "bb".repeat(32),
                    bytes: 32,
                    body: "{\"messages\":[{\"role\":\"user\",\"content\":\"archived\"}]}\n"
                        .to_string(),
                })
            }
        }
        let mut data = fixture::data();
        data.sessions[0].harness = None;
        let response = req("/reader?i=0", &data, &UnknownSource);
        assert_eq!(response.status, 200);
        assert!(
            response.body.contains("Provenance unknown"),
            "{}",
            response.body
        );
        assert!(response.body.contains("time unknown"), "{}", response.body);
    }

    /// A reader fixture whose message count is known: three one-line turns.
    struct ThreeMessages;
    impl ContentSource for ThreeMessages {
        fn fetch(&self, _machine: &str, _session_id: &str) -> Result<Content, String> {
            Ok(Content {
                shards: Vec::new(),
                concat_sha256: "dd".repeat(32),
                bytes: 96,
                body: (0..3)
                    .map(|n| {
                        format!(
                            "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"m{n}\"}}}}"
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            })
        }
    }

    /// A window starting past the last message is an empty window. It is not a
    /// missing conversation and not a usage error, and it has to offer a way
    /// back — an offset that resolves to nothing must not read as a finding.
    #[test]
    fn a_window_past_the_last_message_is_an_empty_window_not_a_missing_one() {
        let r = req("/reader?i=0&m=9", &fixture::data(), &ThreeMessages);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("no messages in this window"), "{}", r.body);
        assert!(!r.body.contains("no conversation content"), "{}", r.body);
        assert!(r.body.contains("← previous messages"), "{}", r.body);
        assert!(r.body.contains("first messages"), "{}", r.body);
    }

    /// Zero messages is the empty conversation, and it is named as that rather
    /// than as a failed read.
    #[test]
    fn an_empty_conversation_is_named_empty() {
        struct EmptyBody;
        impl ContentSource for EmptyBody {
            fn fetch(&self, _m: &str, _s: &str) -> Result<Content, String> {
                Ok(Content {
                    shards: Vec::new(),
                    concat_sha256: "ee".repeat(32),
                    bytes: 0,
                    body: String::new(),
                })
            }
        }
        let r = req("/reader?i=0", &fixture::data(), &EmptyBody);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("no conversation content"), "{}", r.body);
        assert!(!r.body.contains("Reader coverage"), "{}", r.body);
    }

    /// A body that has lines and no recognisable message is not an empty
    /// conversation. Rendering zero messages must not upgrade "we could not
    /// read this shape" into "there is nothing here" (ADR-035 vs ADR-031).
    #[test]
    fn a_body_with_no_recognisable_message_is_not_an_empty_conversation() {
        struct ForeignShape;
        impl ContentSource for ForeignShape {
            fn fetch(&self, _m: &str, _s: &str) -> Result<Content, String> {
                Ok(Content {
                    shards: Vec::new(),
                    concat_sha256: "ff".repeat(32),
                    bytes: 48,
                    body: "{\"unexpected\":true}\nnot json either\n".to_string(),
                })
            }
        }
        let r = req("/reader?i=0", &fixture::data(), &ForeignShape);
        assert_eq!(r.status, 200);
        assert!(
            r.body.contains("could be rendered as a message"),
            "{}",
            r.body
        );
        assert!(!r.body.contains("no conversation content"), "{}", r.body);
        assert!(r.body.contains("1 line(s) were not rendered"), "{}", r.body);
        assert!(
            r.body.contains("1 line(s) were not valid JSON"),
            "{}",
            r.body
        );
    }

    /// A window that cannot be read is a usage error, and a window never
    /// silently widens to the whole conversation.
    #[test]
    fn a_bad_window_is_a_usage_error_and_a_window_never_widens() {
        let data = fixture::data();
        for target in [
            "/reader?i=0&n=0",
            "/reader?i=0&n=x",
            "/reader?i=0&m=x",
            "/reader?n=1",
        ] {
            assert_eq!(
                req(target, &data, &ThreeMessages).status,
                400,
                "{target} must be a usage error"
            );
        }

        let one = req("/reader?i=0&m=0&n=1", &data, &ThreeMessages).body;
        assert!(one.contains("Showing messages 1–1 of 3"), "{one}");
        assert!(one.contains("id=\"m0\""), "{one}");
        assert!(!one.contains("id=\"m1\""), "{one}");
        assert!(one.contains("next messages →"), "{one}");
        assert!(one.contains("last messages"), "{one}");

        let (_, params) = split_target("/reader?n=9999");
        assert_eq!(
            reader_window(&params).unwrap(),
            (0, crate::normalize::MAX_WINDOW)
        );
    }

    /// One message must not be able to spend the whole window's budget. The
    /// head and the tail are kept, the bytes in between are counted, and the
    /// count is exact — an elision note that rounds would be a guess.
    #[test]
    fn one_huge_message_is_elided_and_counted_not_rendered_whole() {
        const FILLER: usize = 300 * 1024;
        struct HugeMessage;
        impl ContentSource for HugeMessage {
            fn fetch(&self, _m: &str, _s: &str) -> Result<Content, String> {
                let filler = "a".repeat(FILLER);
                Ok(Content {
                    shards: Vec::new(),
                    concat_sha256: "11".repeat(32),
                    bytes: filler.len(),
                    body: serde_json::json!({
                        "type": "user",
                        "message": {"role": "user", "content": filler},
                    })
                    .to_string(),
                })
            }
        }
        let html = req("/reader?i=0", &fixture::data(), &HugeMessage).body;
        let elided = FILLER - crate::normalize::MESSAGE_BUDGET;
        assert!(
            html.contains(&format!("[{elided} bytes elided")),
            "the dropped byte count must be the measured one; {elided} expected"
        );
        assert!(
            html.contains("/content?i=0"),
            "the elision must be auditable"
        );
        assert!(
            html.len() < 100 * 1024,
            "one message must not blow the budget; page was {} bytes",
            html.len()
        );
    }

    /// The reader follows no off-host link and loads no external resource. An
    /// external target is therefore printed as text — shown, not followed and
    /// not hidden — while a local target stays a link, parentheses included.
    #[test]
    fn off_host_links_are_text_and_local_targets_keep_their_parentheses() {
        struct Links;
        impl ContentSource for Links {
            fn fetch(&self, _m: &str, _s: &str) -> Result<Content, String> {
                let content = "off [docs](https://example.invalid/x) local \
                               [here](/reader?i=0) nested [n](/a(b)c) js \
                               [j](javascript:alert(1)) psl [p](//a.invalid/x) \
                               bs [b](/\\a.invalid/x) dbs [d](\\a.invalid/x) \
                               frag [f](#section) end";
                Ok(Content {
                    shards: Vec::new(),
                    concat_sha256: "22".repeat(32),
                    bytes: content.len(),
                    body: serde_json::json!({
                        "type": "user",
                        "message": {"role": "user", "content": content},
                    })
                    .to_string(),
                })
            }
        }
        let html = req("/reader?i=0", &fixture::data(), &Links).body;
        assert!(
            html.contains("docs (https://example.invalid/x)"),
            "an off-host target is shown as text: {html}"
        );
        assert!(html.contains("j (javascript:alert(1))"), "{html}");
        assert!(!html.contains("href=\"https://"), "{html}");
        assert!(!html.contains("href=\"javascript:"), "{html}");
        assert!(html.contains("href=\"/reader?i=0\""), "{html}");
        assert!(
            html.contains("href=\"/a(b)c\""),
            "a target's own parentheses are part of it: {html}"
        );
        assert!(
            html.contains("href=\"#section\""),
            "a fragment is local: {html}"
        );

        // A protocol-relative target is not a path, however much its first
        // byte looks like one, and the URL parser reads a leading backslash as
        // a slash. Each is off-host and must be printed rather than linked.
        assert!(html.contains("p (//a.invalid/x)"), "{html}");
        assert!(html.contains("b (/\\a.invalid/x)"), "{html}");
        assert!(html.contains("d (\\a.invalid/x)"), "{html}");
        assert!(!html.contains("href=\"//"), "{html}");
        assert!(!html.contains("href=\"/\\"), "{html}");

        // The vectors above are the ones this test happens to name; the claim
        // is general. Every `href` the page emits is swept, because escaping
        // turns a `"` from archived text into `&quot;`, so a literal `href="`
        // can only begin a real attribute.
        let hrefs: Vec<&str> = html
            .split("href=\"")
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .collect();
        assert!(
            !hrefs.is_empty(),
            "the sweep found no href to check: {html}"
        );
        for href in hrefs {
            assert!(
                href.starts_with('#')
                    || (href.starts_with('/')
                        && !href.starts_with("//")
                        && !href.starts_with("/\\")),
                "the page emitted an off-host href {href:?}"
            );
        }
    }

    /// A time the reader had to interpret is labelled as interpreted. Calling
    /// a derived time "exact" would be the reader inventing provenance.
    #[test]
    fn a_derived_timestamp_is_named_as_derived() {
        struct NumericTime;
        impl ContentSource for NumericTime {
            fn fetch(&self, _m: &str, _s: &str) -> Result<Content, String> {
                Ok(Content {
                    shards: Vec::new(),
                    concat_sha256: "33".repeat(32),
                    bytes: 72,
                    body: "{\"type\":\"user\",\"timestamp\":1736944496,\
                           \"message\":{\"role\":\"user\",\"content\":\"t\"}}\n"
                        .to_string(),
                })
            }
        }
        let html = req("/reader?i=0", &fixture::data(), &NumericTime).body;
        assert!(
            html.contains("inferred (numeric epoch seconds)"),
            "a numeric epoch is an inferred time: {html}"
        );
        assert!(!html.contains("· exact"), "{html}");
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

    #[test]
    fn no_content_does_not_make_a_time_filtered_absence_unknown() {
        let mut d = fixture::data();
        d.sessions.truncate(1);
        d.archive_sessions = 1;
        d.sessions[0].first_unix = None;
        d.sessions[0].last_unix = None;
        d.sessions[0].time_why = None;
        d.sessions[0].time_source = TimeSource::NoConversationContent;

        let html = req("/sessions?day=2030-01-01", &d, &NoContent).body;
        assert!(html.contains("Not in this destination"), "{html}");
        assert!(!html.contains("UNKNOWN"), "{html}");
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

    /// R9 (W172): the banner names exactly the machines the JSON field
    /// `machines_without_activity_index` carries — same list, same count, with
    /// the repair command on the page.
    #[test]
    fn the_banner_names_exactly_the_machines_the_json_field_names() {
        let d = fixture::data();
        let html = req("/", &d, &NoContent).body;
        assert!(
            html.contains("Machines without an activity index."),
            "the banner must render: {html}"
        );
        assert!(
            html.contains("1 machine(s) hold sessions but no activity index"),
            "{html}"
        );
        assert!(
            html.contains("<li class=mono>m-3</li>"),
            "the machine list must name each one: {html}"
        );
        assert!(
            html.contains("chat-stasher activity-index"),
            "the banner must name the repair command: {html}"
        );
        // …and it mirrors the JSON field set for set, not in its own wording
        // alone.
        let (path, params) = split_target("/api/overview");
        let v: serde_json::Value =
            serde_json::from_str(&handle(path, &params, "t", &d, &NoContent).unwrap().body)
                .unwrap();
        assert_eq!(
            v["machines_without_activity_index"],
            serde_json::json!(["m-3"]),
        );
        // An empty list renders no banner at all: a warning about nothing is a
        // lie about the object it describes.
        let mut d = fixture::data();
        d.machines_without_index.clear();
        assert!(!req("/", &d, &NoContent)
            .body
            .contains("Machines without an activity index."));
    }

    /// R9 (W172): a machine that holds sessions but no index is a whole row of
    /// unknown in the heatmap — never a row of empty cells — and the reason is
    /// on the page where the '?' appears.
    #[test]
    fn a_heatmap_row_without_an_index_reads_unknown_never_empty() {
        let d = fixture::no_index_data();
        let html = req("/", &d, &NoContent).body;
        let heat = html
            .split("<h2>Activity, by week</h2>")
            .nth(1)
            .expect("the heatmap section must exist");
        let m3 = heat
            .split("<tr><td class=mono><a href=\"/sessions?machine=m-3")
            .nth(1)
            .expect("the m-3 row must be in the heatmap")
            .split("</tr>")
            .next()
            .expect("the row must close");
        assert!(
            m3.contains("UNKNOWN — no activity index for this machine"),
            "every week cell must carry the unknown reason: {m3}"
        );
        assert!(
            !m3.contains(": no sessions"),
            "an unmeasured week must not read as an empty one: {m3}"
        );
        assert!(
            !m3.contains("since="),
            "no week may link a time filter for a machine whose weeks were never \
             measured: {m3}"
        );
        assert!(
            m3.contains("time unknown — no activity index for this machine"),
            "the '?' column must say why: {m3}"
        );
        assert!(
            heat.contains(
                "<span class=mono>m-3</span> has no activity index, so its sessions cannot \
                 be placed in time"
            ),
            "the reason line must use the CLI's own words: {heat}"
        );
        assert!(
            heat.contains("chat-stasher activity-index"),
            "the reason line must name the repair command: {heat}"
        );
    }

    /// The pure R9 repro at page level: no in-view session has a known time
    /// *because the machine has no index* — there is no axis to draw, and the
    /// reason line must still name the machine instead of leaving a blank wall.
    #[test]
    fn a_heatmap_without_a_time_axis_still_names_the_no_index_machines() {
        let mut d = fixture::no_index_data();
        d.sessions.retain(|s| s.machine == "m-3");
        let html = req("/", &d, &NoContent).body;
        assert!(
            html.contains("No session in view has a known conversation time"),
            "{html}"
        );
        assert!(
            html.contains(
                "<span class=mono>m-3</span> has no activity index, so its sessions cannot \
                 be placed in time"
            ),
            "the no-axis page must still carry the reason: {html}"
        );
    }

    /// OQ-2 (W172): an archive that holds nothing is a served, honest page —
    /// the state that used to exit before any socket was bound. Only a
    /// complete read may claim absence; a partial one keeps the zero a floor,
    /// and a launch filter that matches nothing is not an empty archive.
    #[test]
    fn an_archive_that_holds_nothing_is_an_honest_page_not_an_exit() {
        let mut d = fixture::data();
        d.sessions.clear();
        d.archive_sessions = 0;
        let r = req("/", &d, &NoContent);
        assert_eq!(r.status, 200);
        assert!(
            r.body.contains("(this destination holds no sessions)"),
            "the measured absence must be stated: {}",
            r.body
        );
        assert!(
            r.body.contains("(no sessions in view)"),
            "the heatmap's empty paragraph must stay scoped to the view: {}",
            r.body
        );

        // The same zero, with parts of the destination unreadable: a claim of
        // absence would record an unknown as empty.
        d.unreadable
            .push("host `m-9`: snapshot cccccccc tree walk failed".into());
        let r = req("/", &d, &NoContent);
        assert!(
            !r.body.contains("(this destination holds no sessions)"),
            "{}",
            r.body
        );
        assert!(r.body.contains("floor"), "{}", r.body);

        // A launch filter that matched everything away is a filtered view, not
        // an empty archive: the absence sentence must not appear.
        let mut filtered = Selector::default();
        filtered.machine = Some("m-zz".into());
        let d = UiData::from_report(&fixture::report(), "dest-under-test", filtered, NOW);
        let html = req("/", &d, &NoContent).body;
        assert!(
            !html.contains("(this destination holds no sessions)"),
            "a filter, not an archive, is why this set is empty: {html}"
        );
        assert!(
            html.contains("<span class=v>0</span><span class=l>sessions in view</span>"),
            "the headline must stay scoped to the view: {html}"
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
        let hd =
            crate::overview::heatmap_data(&rows, 80, HeatmapAxis::Machine, Some(Granularity::Week));
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

    // ---------------------------------------------------------- W156 labels

    /// The fixture archive with every label state on view: a harness title
    /// (m-1), a truncated first-user-line label (m-2), a machine whose index
    /// predates labels (flagged at machine level, on m-1), and a session with
    /// no label recorded (m-2).
    fn labelled() -> UiData {
        let mut d = fixture::data();
        d.machines_with_legacy_index = vec!["m-1".into()];
        d.sessions[0].title = SessionLabel::Known {
            text: "Fix the parser retry loop".into(),
            source: TitleSource::HarnessTitle,
            truncated: false,
        };
        d.sessions[1].title = SessionLabel::LegacyIndex;
        d.sessions[2].title = SessionLabel::Known {
            text: "a first-user-line fixture label that is deliberately longer than the \
                   capping limit"
                .into(),
            source: TitleSource::FirstUserLine,
            truncated: true,
        };
        d.sessions[3].title = SessionLabel::NoLabelRecorded;
        d
    }

    /// The list page shows every label state in its own words, with each
    /// known label's provenance on its row, and explains the pre-label
    /// machine once at the top — never once per row.
    #[test]
    fn the_list_shows_each_label_state_in_its_own_words() {
        let d = labelled();
        let html = req("/sessions", &d, &NoContent).body;
        assert!(html.contains("<th>label</th>"), "{html}");
        assert!(html.contains("Fix the parser retry loop"), "{html}");
        assert!(
            html.contains("label source: the harness's own title"),
            "the title's provenance travels with the row: {html}"
        );
        assert!(html.contains("label source: the first user line"), "{html}");
        assert!(
            html.contains("\u{2026}"),
            "a truncated label is visibly stopped: {html}"
        );
        assert!(html.contains("<td>no label recorded</td>"), "{html}");
        assert_eq!(
            html.match_indices(">label unknown</td>").count(),
            1,
            "only the pre-label machine's row may say it: {html}"
        );
        assert_eq!(
            html.match_indices("Label coverage is partial").count(),
            1,
            "one note per affected machine, not one per row: {html}"
        );
        assert!(html.contains("m-1"), "{html}");
        assert!(
            html.contains("activity-index --rebuild"),
            "the note names the repair command: {html}"
        );
        // A fresh machine's page gets no coverage note at all.
        let fresh = req("/sessions?machine=m-2", &d, &NoContent).body;
        assert!(!fresh.contains("Label coverage is partial"), "{fresh}");
    }

    /// A session the index holds no row for is the fourth wire state — it is
    /// `unknown` with a why, never `no_label`: no row read no content.
    #[test]
    fn a_session_without_a_row_is_unknown_with_a_why() {
        let mut d = fixture::data();
        let why = "machine `m-2`'s activity index in this snapshot has no row for this \
                   session, so its label was never recorded"
            .to_string();
        d.sessions[3].title = SessionLabel::Unknown { why };
        let body = req("/api/sessions", &d, &NoContent).body;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let row = v["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["index"] == 3)
            .unwrap();
        assert_eq!(row["title"]["state"], serde_json::json!("unknown"));
        assert!(row["title"]["why"].as_str().unwrap().contains("no row"));
        let html = req("/sessions", &d, &NoContent).body;
        assert_eq!(
            html.match_indices(">label unknown</td>").count(),
            1,
            "{html}"
        );
    }

    /// `/api/sessions` pins the §5.3 contract: schema_version 2, a label
    /// object per row, and the machine-level legacy list at the top level.
    #[test]
    fn api_sessions_pins_the_label_contract_at_schema_two() {
        let d = labelled();
        let body = req("/api/sessions", &d, &NoContent).body;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["schema_version"], serde_json::json!(2), "{body}");
        assert_eq!(
            v["machines_with_legacy_index"],
            serde_json::json!(["m-1"]),
            "{body}"
        );
        let expect = |i: usize, state: &str| {
            assert_eq!(
                v["sessions"][i]["title"]["state"],
                serde_json::json!(state),
                "row {i} of {body}"
            )
        };
        expect(0, "known");
        expect(1, "legacy_index");
        expect(2, "known");
        expect(3, "no_label");
        assert_eq!(
            v["sessions"][0]["title"]["text"],
            serde_json::json!("Fix the parser retry loop")
        );
        assert_eq!(
            v["sessions"][0]["title"]["source"],
            serde_json::json!("harness_title")
        );
        assert_eq!(
            v["sessions"][0]["title"]["truncated"],
            serde_json::json!(false)
        );
        assert_eq!(
            v["sessions"][2]["title"]["source"],
            serde_json::json!("first_user_line")
        );
        assert_eq!(
            v["sessions"][2]["title"]["truncated"],
            serde_json::json!(true)
        );
        // A recorded absence carries no text field at all — an empty string
        // could be confused with one.
        assert!(v["sessions"][3]["title"]["text"].is_null(), "{body}");
    }

    /// The session page shows the label with the same honesty, plus its
    /// provenance row — which exists only when there is a label to attribute.
    #[test]
    fn the_session_page_shows_label_and_source_rows() {
        let d = labelled();
        let known = req("/session?i=0", &d, &NoContent).body;
        assert!(known.contains("<th>label</th>"), "{known}");
        assert!(known.contains("Fix the parser retry loop"), "{known}");
        assert!(known.contains("<th>label source</th>"), "{known}");
        assert!(known.contains("the harness's own title"), "{known}");
        let legacy = req("/session?i=1", &d, &NoContent).body;
        assert!(
            legacy.contains("label unknown"),
            "the same word as the list, not a new one: {legacy}"
        );
        assert!(!legacy.contains("no label recorded"), "{legacy}");
        assert!(
            !legacy.contains("<th>label source</th>"),
            "nothing to attribute for a pre-label row: {legacy}"
        );
        assert!(
            legacy.contains("predates labels"),
            "the row's title attribute says why: {legacy}"
        );
    }
}

// ---------------------------------------------------- byte-identity capture

/// The capture that proves the W170 `ui.rs` → `ui/*` split is a pure move.
///
/// Every route's response is frozen to a file under `tests/fixtures/ui-bodies/`.
/// The split moved code between files and must not have changed one byte of what
/// the server says, so this replays the same deterministic fixture through
/// [`handle`] and compares the status line, the content type and the body
/// against the committed capture. Before the split the captures were written
/// from the unsplit module; after it, this test is the proof they are unchanged.
///
/// The fixture is the in-crate synthetic archive rather than a built
/// repository: a real `ui` run embeds the launch clock (`now_unix`) in every
/// page, and a real push stamps its own snapshot time, so those bodies are not
/// reproducible byte-for-byte. Behaviour against a real repository is covered
/// separately by `tests/w15_ui_test.rs`.
///
/// Regenerate, only after reading the diff of an intentional change:
/// `UPDATE_UI_GOLDEN=1 cargo test -p chat-stasher --lib golden`.
#[cfg(test)]
mod golden {
    use super::fixture::{self, CountingContent};
    use super::*;

    /// Which payload-tier stub a case goes through. The default in the design
    /// is "the payload tier is disabled" ([`NoContent`]), and pinning that
    /// refusal's wording is the point of the `denied` cases.
    enum Source {
        Counting,
        Denied,
        Conversation,
    }

    /// Three recognisable turns, so the window cases exercise a real window
    /// rather than the "no conversation content" branch.
    struct Conversation;

    impl ContentSource for Conversation {
        fn fetch(&self, _machine: &str, _session_id: &str) -> Result<Content, String> {
            Ok(Content {
                shards: Vec::new(),
                concat_sha256: "9c".repeat(32),
                bytes: 96,
                body: (0..3)
                    .map(|n| {
                        format!(
                            "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"turn {n}\"}}}}"
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            })
        }
    }

    const CASES: &[(&str, &str, Source)] = &[
        ("overview", "/", Source::Counting),
        ("sessions", "/sessions", Source::Counting),
        (
            "sessions-machine",
            "/sessions?machine=m-1",
            Source::Counting,
        ),
        ("session-zero", "/session?i=0", Source::Counting),
        ("session-two", "/session?i=2", Source::Counting),
        ("content-one", "/content?i=1", Source::Counting),
        ("reader-one", "/reader?i=1", Source::Counting),
        ("reader-window", "/reader?i=1&m=0&n=1", Source::Counting),
        ("reader-multi", "/reader?i=0", Source::Conversation),
        (
            "reader-multi-window",
            "/reader?i=0&m=0&n=1",
            Source::Conversation,
        ),
        (
            "reader-multi-past-end",
            "/reader?i=0&m=9",
            Source::Conversation,
        ),
        ("content-denied", "/content?i=1", Source::Denied),
        ("reader-denied", "/reader?i=1", Source::Denied),
        ("api-overview", "/api/overview", Source::Counting),
        ("api-sessions", "/api/sessions", Source::Counting),
        (
            "api-sessions-filtered",
            "/api/sessions?machine=m-1&harness=claude-code",
            Source::Counting,
        ),
        (
            "unresolvable-filter-html",
            "/sessions?day=yesterday",
            Source::Counting,
        ),
        (
            "unresolvable-filter-json",
            "/api/sessions?day=yesterday",
            Source::Counting,
        ),
        ("unknown-row", "/session?i=9999", Source::Counting),
        ("bad-reader-window", "/reader?i=1&n=0", Source::Counting),
    ];

    fn rendered(target: &str, source: &Source) -> String {
        let (path, params) = split_target(target);
        let counting = CountingContent::default();
        let conversation = Conversation;
        let content: &dyn ContentSource = match source {
            Source::Counting => &counting,
            Source::Denied => &NoContent,
            Source::Conversation => &conversation,
        };
        let response = handle(path, &params, "golden-token", &fixture::data(), content)
            .unwrap_or_else(|| panic!("`{target}` must be a known route"));
        format!(
            "status: {} {}\ncontent-type: {}\n\n{}",
            response.status, response.reason, response.content_type, response.body
        )
    }

    fn golden_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ui-bodies")
            .join(format!("{name}.response.txt"))
    }

    #[test]
    fn every_route_body_is_byte_identical_to_the_recorded_capture() {
        let update = std::env::var_os("UPDATE_UI_GOLDEN").is_some();
        if update {
            std::fs::create_dir_all(golden_path("overview").parent().unwrap()).unwrap();
        }
        for (name, target, source) in CASES {
            let live = rendered(target, source);
            if update {
                std::fs::write(golden_path(name), &live).unwrap();
                continue;
            }
            let recorded = std::fs::read_to_string(golden_path(name)).unwrap_or_else(|e| {
                panic!(
                    "missing capture {}: {e}; regenerate with UPDATE_UI_GOLDEN=1",
                    golden_path(name).display()
                )
            });
            assert_eq!(
                live,
                recorded,
                "`{target}` changed since the capture in {}",
                golden_path(name).display()
            );
        }
        // A stray `UPDATE_UI_GOLDEN` must never read as a passing comparison.
        assert!(
            !update,
            "UPDATE_UI_GOLDEN was set: captures were rewritten, not compared"
        );
    }
}
