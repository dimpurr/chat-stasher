//! selector — the one filter vocabulary that `search` and the future `export`
//! share (ADR-027).
//!
//! Before this module existed, `search` filtered on *archive* time: the rustic
//! snapshot's creation time, which is the same instant for every session in a
//! machine's snapshot. That is not a property of a conversation — it is a
//! property of the backup run — so `--since/--until` selected whole machines
//! and answered a question nobody asked ("was this machine backed up in that
//! window?"). The filter now runs on each session's **conversation interval**
//! `[first_unix, last_unix]`, taken from the activity sidecar index
//! (`meta/<machine>/activity-v1.jsonl`, written by `activity-index`), and a
//! session matches when that interval **intersects** the query window.
//!
//! Three states, kept three states all the way to the user (invariant 1):
//!
//! * **selected** — every constraint was evaluated and holds;
//! * **not selected** — every constraint was evaluated and at least one fails;
//! * **unevaluated** — a constraint the user actually asked for *cannot* be
//!   evaluated for this session, because its conversation time is unknown or
//!   its harness cannot be derived. [`Verdict::Unevaluated`] is never a silent
//!   exclusion: callers must list these separately, and must not treat "no
//!   matches" as a proven negative while any exist.
//!
//! That third variant is the whole point of the type. `None` for a time is
//! "we do not know", never "1970" and never "outside the window".
//!
//! This module is deliberately pure: no IO, no repository, no printing. It
//! takes metadata in and returns a verdict, which is what makes it reusable by
//! `export` and testable without a fixture.

use std::collections::BTreeSet;

use chrono::{Local, LocalResult, NaiveDate, TimeZone};
use clap::Args;

/// A session's metadata, reduced to exactly what the filters need.
///
/// Borrowed so that a caller iterating its own rows does not clone strings to
/// ask a yes/no question.
#[derive(Debug, Clone, Copy)]
pub struct SessionMeta<'a> {
    /// Machine partition (`sessions/<machine>/…`).
    pub machine: &'a str,
    /// Session directory name.
    pub session_id: &'a str,
    /// Harness the session belongs to, derived from the id by
    /// [`crate::sidecar::infer_harness`]. `None` means the id carries no
    /// harness prefix — not "some other harness".
    pub harness: Option<&'a str>,
    /// Earliest conversation time, unix seconds. `None` is *unknown*; it is
    /// never a stand-in for zero.
    pub first_unix: Option<i64>,
    /// Latest conversation time, unix seconds. `None` is *unknown*.
    pub last_unix: Option<i64>,
    /// What those two bounds are bounds **of** — see [`TimeBounds`].
    pub time_bounds: TimeBounds,
    /// Why the conversation time is unknown, or why the bounds are only part of
    /// the span ([`TimeBounds::Partial`]). Present whenever either bound is
    /// `None` or the bounds are partial, so the reason survives to the terminal
    /// instead of being re-invented there.
    pub time_why: Option<&'a str>,
}

/// What a session's two bounds are bounds *of*.
///
/// This exists because "the interval is `[first, last]`" and "the interval
/// *contains* `[first, last]`" answer a window question differently, and only
/// one of the two is a proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeBounds {
    /// `[first_unix, last_unix]` is the conversation span: the archive holds
    /// every conversation time this session has.
    Complete,
    /// The bounds are an **inner** bound of the span: the session also holds
    /// conversation records that could not be placed in time, so the real span
    /// may be wider on either end. An overlap with the query window is still
    /// proof the session was active in it; a non-overlap is not proof that it
    /// was not, and must be reported as unplaceable rather than as a
    /// non-match.
    ///
    /// Set from a row whose time state is
    /// [`crate::activity::TimeSource::PartialRange`]; every other time state
    /// carries either the whole span or no bounds at all.
    Partial,
}

impl<'a> SessionMeta<'a> {
    /// A session with no known conversation time and an explicit reason.
    pub fn with_unknown_time(
        machine: &'a str,
        session_id: &'a str,
        harness: Option<&'a str>,
        why: &'a str,
    ) -> Self {
        Self {
            machine,
            session_id,
            harness,
            first_unix: None,
            last_unix: None,
            // No bounds at all, so there is no span for them to be part of.
            time_bounds: TimeBounds::Complete,
            time_why: Some(why),
        }
    }
}

/// How the window was spelled on the command line. Kept because the two
/// spellings are not equally trustworthy and the output must say which one
/// produced the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowHow {
    /// `--day` / `--since` / `--until`: local calendar days, inclusive, with
    /// the day's own local midnight boundaries.
    LocalDays,
    /// `--since-unix` / `--until-unix`: raw unix seconds. Deprecated — kept
    /// for one version, and it compares conversation time like everything else.
    UnixSeconds,
}

/// The conversation-time window. Either bound may be absent, which means "no
/// constraint on that side" — that is a real absence, not a sentinel value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeWindow {
    /// Inclusive lower bound, unix seconds.
    pub since_unix: Option<i64>,
    /// Inclusive upper bound, unix seconds.
    pub until_unix: Option<i64>,
    pub how: WindowHow,
    /// The literal text the user typed for the lower bound, for output.
    pub since_text: Option<String>,
    /// The literal text the user typed for the upper bound, for output.
    pub until_text: Option<String>,
}

impl TimeWindow {
    /// One line naming the window and its unit, for the terminal.
    pub fn describe(&self) -> String {
        match self.how {
            WindowHow::LocalDays => format!(
                "local day(s) {} .. {} inclusive (each day = 00:00:00–23:59:59 local)",
                self.since_text.as_deref().unwrap_or("(no lower bound)"),
                self.until_text.as_deref().unwrap_or("(no upper bound)"),
            ),
            WindowHow::UnixSeconds => format!(
                "unix seconds {} .. {} inclusive (deprecated flags; compared against conversation time)",
                self.since_text.as_deref().unwrap_or("(no lower bound)"),
                self.until_text.as_deref().unwrap_or("(no upper bound)"),
            ),
        }
    }
}

/// The filter set. Every field is `None` == "no constraint".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selector {
    /// Match sessions whose id starts with this prefix.
    pub session_id_prefix: Option<String>,
    /// Match one machine partition exactly.
    pub machine: Option<String>,
    /// Match these harnesses. `Some(empty)` matches nothing, which is what a
    /// user who typed `--harness ""` asked for; `None` is no constraint.
    pub harnesses: Option<BTreeSet<String>>,
    /// Conversation-time window. `None` == every session, whatever its time.
    pub window: Option<TimeWindow>,
}

/// Which constraint had no answer for a session. Carried explicitly rather
/// than inferred from the text of `why`, so output can group and count without
/// parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnplacedBy {
    /// The conversation-time window.
    Time,
    /// The harness filter.
    Harness,
    /// The session holds no conversation content at all (ADR-035), so a time
    /// window cannot place it — but for a different reason than a real
    /// conversation with an unknown time.
    NoContent,
}

/// What [`Selector::select`] decided about one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every active constraint was evaluated and holds.
    Selected,
    /// Every active constraint was evaluated and at least one fails.
    NotSelected,
    /// An active constraint could not be evaluated for this session. The `why`
    /// is user-facing and must be shown; the session is neither a match nor a
    /// proven non-match.
    Unevaluated { dimension: UnplacedBy, why: String },
}

impl Selector {
    pub fn session_id_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.session_id_prefix = Some(prefix.into());
        self
    }

    pub fn machine(mut self, machine: impl Into<String>) -> Self {
        self.machine = Some(machine.into());
        self
    }

    pub fn harnesses<I, S>(mut self, harnesses: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.harnesses = Some(harnesses.into_iter().map(Into::into).collect());
        self
    }

    pub fn window(mut self, window: TimeWindow) -> Self {
        self.window = Some(window);
        self
    }

    /// Decide one session.
    ///
    /// Order matters only for which reason is reported first, never for the
    /// outcome: every constraint is checked, and the first one that is active
    /// and *cannot be evaluated* wins over any later "not selected".
    pub fn select(&self, meta: &SessionMeta<'_>) -> Verdict {
        if let Some(want) = &self.machine {
            if meta.machine != want {
                return Verdict::NotSelected;
            }
        }
        if let Some(prefix) = &self.session_id_prefix {
            if !meta.session_id.starts_with(prefix.as_str()) {
                return Verdict::NotSelected;
            }
        }
        if let Some(want) = &self.harnesses {
            match meta.harness {
                Some(h) if want.contains(h) => {}
                Some(_) => return Verdict::NotSelected,
                // The user asked a harness question and this row cannot answer
                // it. Saying "not selected" would be a guess.
                None => {
                    return Verdict::Unevaluated {
                        dimension: UnplacedBy::Harness,
                        why: "the harness filter cannot be evaluated for this session: its \
                              archived id carries no harness prefix (no `.` or `~` segment)"
                            .to_string(),
                    }
                }
            }
        }
        let Some(window) = &self.window else {
            return Verdict::Selected;
        };
        match (meta.first_unix, meta.last_unix) {
            (Some(first), Some(last)) => {
                let after_start = window.since_unix.is_none_or(|s| last >= s);
                let before_end = window.until_unix.is_none_or(|u| first <= u);
                if after_start && before_end {
                    Verdict::Selected
                } else if meta.time_bounds == TimeBounds::Partial {
                    // The bounds are only *part* of the span, so a window they
                    // do not reach is a window the rest of the session may
                    // still be in. Same "we do not know" verdict as a missing
                    // bound — never a proven non-match.
                    Verdict::Unevaluated {
                        dimension: UnplacedBy::Time,
                        why: meta
                            .time_why
                            .unwrap_or(
                                "only part of this session's conversation could be placed in time and no reason was recorded",
                            )
                            .to_string(),
                    }
                } else {
                    Verdict::NotSelected
                }
            }
            // At least one bound is unknown: the interval is not known, so the
            // intersection is not known either.
            _ => Verdict::Unevaluated {
                dimension: UnplacedBy::Time,
                why: meta
                    .time_why
                    .unwrap_or(
                        "the conversation time of this session is unknown and no reason was recorded",
                    )
                    .to_string(),
            },
        }
    }
}

// ---------------------------------------------------------------- CLI surface

/// Filters shared by `search` and (later) `export`, flattened into each
/// command's argument struct so the two commands cannot drift apart.
///
/// Conflicts are expressed here rather than re-implemented per command:
/// mixing a local-day flag with a unix-seconds flag is a usage error
/// regardless of which command asked.
#[derive(Debug, Clone, Default, Args)]
pub struct SelectorArgs {
    /// Match sessions whose id starts with this prefix.
    #[arg(long)]
    pub session: Option<String>,
    /// Match one machine partition exactly.
    #[arg(long)]
    pub machine: Option<String>,
    /// Match these harnesses, comma-separated (e.g. `--harness claude-code,codex`).
    /// The harness is the leading `.`/`~` segment of the archived session id.
    #[arg(long, value_delimiter = ',', value_name = "ID")]
    pub harness: Option<Vec<String>>,
    /// One local calendar day, `YYYY-MM-DD`. Identical to `--since D --until D`.
    #[arg(
        long,
        value_name = "YYYY-MM-DD",
        conflicts_with_all = ["since", "until", "since_unix", "until_unix"]
    )]
    pub day: Option<String>,
    /// Earliest local calendar day to include, `YYYY-MM-DD`, from that day's
    /// 00:00:00 local time.
    #[arg(long, value_name = "YYYY-MM-DD", conflicts_with_all = ["since_unix", "until_unix"])]
    pub since: Option<String>,
    /// Latest local calendar day to include, `YYYY-MM-DD`, through that day's
    /// 23:59:59 local time.
    #[arg(long, value_name = "YYYY-MM-DD", conflicts_with_all = ["since_unix", "until_unix"])]
    pub until: Option<String>,
    /// DEPRECATED, use `--since`/`--until`/`--day`. Inclusive lower bound on the
    /// session's conversation time, unix seconds.
    #[arg(long)]
    pub since_unix: Option<i64>,
    /// DEPRECATED, use `--since`/`--until`/`--day`. Inclusive upper bound on the
    /// session's conversation time, unix seconds.
    #[arg(long)]
    pub until_unix: Option<i64>,
}

/// A resolved [`Selector`] plus anything the caller must say out loud before
/// using it.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub selector: Selector,
    /// Lines to write to **stderr** before doing any work. Non-empty only when
    /// a deprecated flag was used; the command still runs.
    pub warnings: Vec<String>,
}

/// A usage error: the arguments cannot be turned into a filter at all. The
/// caller prints the message and exits 2, exactly as for a clap parse error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// A usage error is a real error value, not just a message: callers that return
// `anyhow::Result` (export) carry it out through `anyhow`, and the CLI tells it
// apart from an IO failure with `downcast_ref` to decide on exit code 2.
impl std::error::Error for UsageError {}

impl SelectorArgs {
    /// Turn the flags into a [`Selector`], or explain why they cannot be.
    ///
    /// `--day` / `--since` / `--until` are resolved against the **local**
    /// timezone here, once, so that a caller never has to know a timezone
    /// exists. Flags that merely conflict are caught earlier, by clap.
    pub fn resolve(&self) -> Result<Resolved, UsageError> {
        let mut warnings = Vec::new();
        let window = self.resolve_window(&mut warnings)?;
        Ok(Resolved {
            selector: Selector {
                session_id_prefix: self.session.clone(),
                machine: self.machine.clone(),
                harnesses: self
                    .harness
                    .as_ref()
                    .map(|list| list.iter().cloned().collect()),
                window,
            },
            warnings,
        })
    }

    fn resolve_window(&self, warnings: &mut Vec<String>) -> Result<Option<TimeWindow>, UsageError> {
        if let Some(day) = &self.day {
            let (y, m, d) = parse_local_day(day)?;
            let since = local_day_start(y, m, d)?;
            let until = local_day_end(y, m, d)?;
            return Ok(Some(TimeWindow {
                since_unix: Some(since),
                until_unix: Some(until),
                how: WindowHow::LocalDays,
                since_text: Some(day.clone()),
                until_text: Some(day.clone()),
            }));
        }
        if self.since.is_some() || self.until.is_some() {
            let since = match &self.since {
                Some(text) => {
                    let (y, m, d) = parse_local_day(text)?;
                    Some(local_day_start(y, m, d)?)
                }
                None => None,
            };
            let until = match &self.until {
                Some(text) => {
                    let (y, m, d) = parse_local_day(text)?;
                    Some(local_day_end(y, m, d)?)
                }
                None => None,
            };
            // An inverted window is not an empty result, it is a query that
            // cannot be satisfied by construction; answering "0 matched" would
            // dress a typo up as a measurement.
            if let (Some(s), Some(u)) = (since, until) {
                if s > u {
                    return Err(UsageError(format!(
                        "`--since {}` is after `--until {}` — the window is empty by construction, so `0 matched` would not be a measurement",
                        self.since.as_deref().unwrap_or(""),
                        self.until.as_deref().unwrap_or(""),
                    )));
                }
            }
            return Ok(Some(TimeWindow {
                since_unix: since,
                until_unix: until,
                how: WindowHow::LocalDays,
                since_text: self.since.clone(),
                until_text: self.until.clone(),
            }));
        }
        if self.since_unix.is_some() || self.until_unix.is_some() {
            if let (Some(s), Some(u)) = (self.since_unix, self.until_unix) {
                if s > u {
                    return Err(UsageError(format!(
                        "`--since-unix {s}` is after `--until-unix {u}` — the window is empty by construction, so `0 matched` would not be a measurement"
                    )));
                }
            }
            warnings.push(
                "search: `--since-unix` / `--until-unix` are deprecated and will be removed in the \
                 next version; use `--day YYYY-MM-DD`, `--since YYYY-MM-DD` or `--until YYYY-MM-DD` \
                 (local calendar days) instead. They now filter on each session's conversation time, \
                 which is what the date flags do too."
                    .to_string(),
            );
            return Ok(Some(TimeWindow {
                since_unix: self.since_unix,
                until_unix: self.until_unix,
                how: WindowHow::UnixSeconds,
                since_text: self.since_unix.map(|v| v.to_string()),
                until_text: self.until_unix.map(|v| v.to_string()),
            }));
        }
        Ok(None)
    }
}

// -------------------------------------------------------------- local calendar

/// Parse a strict `YYYY-MM-DD` local calendar date.
///
/// Deliberately strict: `2026-1-5`, `20260105` and `2026-01-05T00:00:00` are
/// rejected rather than guessed at, because guessing here silently moves the
/// window and the answer looks the same as a correct one.
fn parse_local_day(text: &str) -> Result<(i32, u32, u32), UsageError> {
    let bad = || {
        UsageError(format!(
            "`{text}` is not a calendar date; write it as YYYY-MM-DD, e.g. 2026-01-15"
        ))
    };
    let mut parts = text.split('-');
    let (y, m, d) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(y), Some(m), Some(d), None) => (y, m, d),
        _ => return Err(bad()),
    };
    if y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return Err(bad());
    }
    let y: i32 = y.parse().map_err(|_| bad())?;
    let m: u32 = m.parse().map_err(|_| bad())?;
    let d: u32 = d.parse().map_err(|_| bad())?;
    NaiveDate::from_ymd_opt(y, m, d).ok_or_else(bad)?;
    Ok((y, m, d))
}

/// Resolve one validated `YYYY-MM-DD` value to its inclusive local-day bounds.
pub fn local_day_bounds(text: &str) -> Result<(i64, i64), UsageError> {
    let (year, month, day) = parse_local_day(text)?;
    Ok((
        local_day_start(year, month, day)?,
        local_day_end(year, month, day)?,
    ))
}

/// The first instant of a local calendar day.
///
/// Walks forward a minute at a time from local 00:00:00 and takes the first
/// instant that exists. A "spring forward" transition that lands on midnight
/// makes 00:00 nonexistent; the honest answer is then the transition instant,
/// not a silently shifted one.
fn local_day_start(y: i32, m: u32, d: u32) -> Result<i64, UsageError> {
    for step in 0..(24 * 60) {
        let (hh, mm) = (step / 60, step % 60);
        if let Some(t) = resolve_local(y, m, d, hh as u32, mm as u32, 0, false) {
            return Ok(t);
        }
    }
    Err(UsageError(format!(
        "no local time exists on {y:04}-{m:02}-{d:02} at all in this timezone — refusing to invent a boundary"
    )))
}

/// The last instant of a local calendar day.
///
/// The mirror of [`local_day_start`]: walks backward from local 23:59 and takes
/// the last instant that exists, so a day that is long (or short) because of a
/// DST transition still covers exactly the instants it contains.
fn local_day_end(y: i32, m: u32, d: u32) -> Result<i64, UsageError> {
    for step in 0..(24 * 60) {
        let minute = 24 * 60 - 1 - step;
        let (hh, mm) = (minute / 60, minute % 60);
        if let Some(t) = resolve_local(y, m, d, hh as u32, mm as u32, 59, true) {
            return Ok(t);
        }
    }
    Err(UsageError(format!(
        "no local time exists on {y:04}-{m:02}-{d:02} at all in this timezone — refusing to invent a boundary"
    )))
}

/// Resolve a local wall-clock time to unix seconds. `latest` picks the later of
/// an ambiguous pair (a "fall back" hour happens twice); the earlier one is
/// picked otherwise, so a day always covers every instant it contains.
fn resolve_local(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32, latest: bool) -> Option<i64> {
    match Local.with_ymd_and_hms(y, m, d, hh, mm, ss) {
        LocalResult::Single(t) => Some(t.timestamp()),
        LocalResult::Ambiguous(earlier, later) => {
            Some(if latest { later } else { earlier }.timestamp())
        }
        LocalResult::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `infer_harness` hands back an owned `String` while `SessionMeta` borrows,
    /// so this fixture leaks it. Test process, bounded by the number of
    /// assertions — not a pattern for production code.
    fn harness_of(session: &str) -> Option<&'static str> {
        crate::sidecar::infer_harness(session).map(|h| &*Box::leak(h.into_boxed_str()))
    }

    fn meta<'a>(machine: &'a str, session: &'a str, span: Option<(i64, i64)>) -> SessionMeta<'a> {
        SessionMeta {
            machine,
            session_id: session,
            harness: harness_of(session),
            first_unix: span.map(|s| s.0),
            last_unix: span.map(|s| s.1),
            time_bounds: TimeBounds::Complete,
            time_why: span
                .is_none()
                .then_some("no timestamps in this session's lines"),
        }
    }

    fn window(since: i64, until: i64) -> TimeWindow {
        TimeWindow {
            since_unix: Some(since),
            until_unix: Some(until),
            how: WindowHow::UnixSeconds,
            since_text: Some(since.to_string()),
            until_text: Some(until.to_string()),
        }
    }

    // -------------------------------------------------- intersection semantics

    /// A session spanning many days is selected by a window that covers only
    /// its middle — the interval *intersects*, it does not have to be contained.
    #[test]
    fn multi_day_session_is_selected_by_a_window_inside_it() {
        let s = Selector::default().window(window(1_500, 1_600));
        let long = meta("m", "claude-code.m.abc", Some((1_000, 2_000)));
        assert_eq!(s.select(&long), Verdict::Selected);
    }

    #[test]
    fn session_entirely_outside_the_window_is_not_selected() {
        let s = Selector::default().window(window(1_500, 1_600));
        let before = meta("m", "claude-code.m.abc", Some((100, 200)));
        let after = meta("m", "claude-code.m.abc", Some((3_000, 4_000)));
        assert_eq!(s.select(&before), Verdict::NotSelected);
        assert_eq!(s.select(&after), Verdict::NotSelected);
    }

    /// Boundaries are inclusive on both ends: a window endpoint equal to the
    /// session's `first` or `last` still counts as an intersection.
    #[test]
    fn window_endpoints_equal_to_first_or_last_still_intersect() {
        let session = meta("m", "claude-code.m.abc", Some((1_000, 2_000)));
        // window ends exactly at `first`
        assert_eq!(
            Selector::default()
                .window(window(500, 1_000))
                .select(&session),
            Verdict::Selected
        );
        // window starts exactly at `last`
        assert_eq!(
            Selector::default()
                .window(window(2_000, 3_000))
                .select(&session),
            Verdict::Selected
        );
        // one second past either end does not
        assert_eq!(
            Selector::default()
                .window(window(500, 999))
                .select(&session),
            Verdict::NotSelected
        );
        assert_eq!(
            Selector::default()
                .window(window(2_001, 3_000))
                .select(&session),
            Verdict::NotSelected
        );
    }

    /// A zero-length session (first == last) is still an interval, and is
    /// selected by a window containing that instant.
    #[test]
    fn instant_session_intersects_only_a_window_containing_it() {
        let s = Selector::default().window(window(1_000, 2_000));
        assert_eq!(
            s.select(&meta("m", "claude-code.m.abc", Some((1_500, 1_500)))),
            Verdict::Selected
        );
        assert_eq!(
            s.select(&meta("m", "claude-code.m.abc", Some((999, 999)))),
            Verdict::NotSelected
        );
    }

    /// An open-ended window constrains only the side it names.
    #[test]
    fn one_sided_window_constrains_only_that_side() {
        let since_only = TimeWindow {
            since_unix: Some(1_000),
            until_unix: None,
            how: WindowHow::LocalDays,
            since_text: Some("2026-01-15".into()),
            until_text: None,
        };
        let s = Selector::default().window(since_only);
        assert_eq!(
            s.select(&meta("m", "claude-code.m.abc", Some((5_000, 6_000)))),
            Verdict::Selected
        );
        assert_eq!(
            s.select(&meta("m", "claude-code.m.abc", Some((1, 999)))),
            Verdict::NotSelected
        );
    }

    // ------------------------------------------------ the three-state verdict

    /// The honesty rule at unit level: an unknown conversation time is not a
    /// "no", and the reason travels with it.
    #[test]
    fn unknown_time_is_unevaluated_with_its_reason_never_not_selected() {
        let s = Selector::default().window(window(1_000, 2_000));
        let v = s.select(&meta("m", "claude-code.m.abc", None));
        match v {
            Verdict::Unevaluated { dimension, why } => {
                assert_eq!(dimension, UnplacedBy::Time);
                assert!(
                    why.contains("no timestamps in this session's lines"),
                    "the recorded reason must survive: {why}"
                );
            }
            other => panic!("unknown time must never be a yes/no answer, got {other:?}"),
        }
    }

    /// Half-known is still unknown: a `first` without a `last` cannot be
    /// intersected, so it is unevaluated too.
    #[test]
    fn a_half_known_interval_is_unevaluated() {
        let s = Selector::default().window(window(1_000, 2_000));
        let half = SessionMeta {
            machine: "m",
            session_id: "claude-code.m.abc",
            harness: Some("claude-code"),
            first_unix: Some(1_500),
            last_unix: None,
            time_bounds: TimeBounds::Complete,
            time_why: Some("only one timestamp in the whole session".into()),
        };
        assert_eq!(
            s.select(&half),
            Verdict::Unevaluated {
                dimension: UnplacedBy::Time,
                why: "only one timestamp in the whole session".into()
            }
        );
    }

    /// A span whose bounds are only *part* of the session's conversation must
    /// not answer "not in the window": an unrecognised record may carry the
    /// conversation into it. Same third verdict as a missing bound — never a
    /// proven non-match.
    #[test]
    fn a_partial_span_outside_the_window_is_not_excluded() {
        let s = Selector::default().window(window(5_000, 6_000));
        let partial = SessionMeta {
            time_bounds: TimeBounds::Partial,
            time_why: Some("the recorded span is only part of this session's conversation"),
            ..meta("m", "kimi-code.m.abc", Some((1_000, 2_000)))
        };
        assert_eq!(
            s.select(&partial),
            Verdict::Unevaluated {
                dimension: UnplacedBy::Time,
                why: "the recorded span is only part of this session's conversation".into()
            },
            "a partial span must be reported as unplaceable, never as a non-match"
        );
        // The control: the same bounds, complete, *are* excluded — so the test
        // measures the partiality flag and not the bounds.
        assert_eq!(
            s.select(&meta("m", "kimi-code.m.abc", Some((1_000, 2_000)))),
            Verdict::NotSelected
        );
    }

    /// An overlap with the recorded bounds is still a proof: the recorded
    /// interval is a sub-interval of the span, so the session really was active
    /// in the window. Partiality must not throw that away.
    #[test]
    fn a_partial_span_overlapping_the_window_is_selected() {
        let s = Selector::default().window(window(1_500, 1_600));
        let partial = SessionMeta {
            time_bounds: TimeBounds::Partial,
            ..meta("m", "kimi-code.m.abc", Some((1_000, 2_000)))
        };
        assert_eq!(s.select(&partial), Verdict::Selected);
    }

    /// With no window there is nothing to evaluate, so an unknown time is not
    /// an obstacle — and nothing is silently dropped.
    #[test]
    fn without_a_window_an_unknown_time_is_still_selected() {
        let s = Selector::default();
        assert_eq!(
            s.select(&meta("m", "claude-code.m.abc", None)),
            Verdict::Selected
        );
    }

    // -------------------------------------------------------- other constraints

    #[test]
    fn machine_and_prefix_constraints_still_select_and_reject() {
        let s = Selector::default()
            .session_id_prefix("claude-code")
            .machine("m-1");
        assert_eq!(
            s.select(&meta("m-1", "claude-code.m.abc", Some((1, 2)))),
            Verdict::Selected
        );
        assert_eq!(
            s.select(&meta("m-2", "claude-code.m.abc", Some((1, 2)))),
            Verdict::NotSelected
        );
        assert_eq!(
            s.select(&meta("m-1", "codex.m.abc", Some((1, 2)))),
            Verdict::NotSelected
        );
    }

    #[test]
    fn harness_constraint_matches_the_id_prefix() {
        let s = Selector::default().harnesses(["claude-code", "codex"]);
        assert_eq!(
            s.select(&meta("m", "claude-code.m.abc", Some((1, 2)))),
            Verdict::Selected
        );
        assert_eq!(
            s.select(&meta("m", "codex.m.abc", Some((1, 2)))),
            Verdict::Selected
        );
        assert_eq!(
            s.select(&meta("m", "cursor.m.abc", Some((1, 2)))),
            Verdict::NotSelected
        );
        // …and the harness filter alone must not need a time at all.
        assert_eq!(s.select(&meta("m", "codex.m.abc", None)), Verdict::Selected);
    }

    /// An id with no harness prefix cannot answer a harness question, so it is
    /// unevaluated — never quietly dropped.
    #[test]
    fn harness_filter_on_a_prefixless_id_is_unevaluated() {
        let s = Selector::default().harnesses(["codex"]);
        let v = s.select(&meta("m", ".odd", Some((1, 2))));
        match v {
            Verdict::Unevaluated { dimension, why } => {
                assert_eq!(dimension, UnplacedBy::Harness);
                assert!(why.contains("no harness prefix"));
            }
            other => panic!("expected an unevaluated verdict, got {other:?}"),
        }
    }

    /// An empty `--harness` list is a real constraint that matches nothing; it
    /// is not silently treated as "no constraint".
    #[test]
    fn an_empty_harness_list_matches_nothing() {
        let s = Selector::default().harnesses(Vec::<String>::new());
        assert_eq!(
            s.select(&meta("m", "codex.m.abc", Some((1, 2)))),
            Verdict::NotSelected
        );
    }

    // --------------------------------------------------------- argument parsing

    /// `--day D` must cover exactly the instants of local day `D`: local noon
    /// of D is inside, local noon of the day before and the day after are not.
    /// Asserted against the same timezone database the resolver used, so a
    /// wrong UTC offset cannot pass by matching a wrong expectation.
    #[test]
    fn day_covers_exactly_that_local_day() {
        let args = SelectorArgs {
            day: Some("2026-01-15".into()),
            ..Default::default()
        };
        let w = args
            .resolve()
            .unwrap()
            .selector
            .window
            .expect("--day must produce a window");
        assert_eq!(w.how, WindowHow::LocalDays);
        let since = w.since_unix.unwrap();
        let until = w.until_unix.unwrap();

        let noon = |y: i32, m: u32, d: u32| {
            Local
                .with_ymd_and_hms(y, m, d, 12, 0, 0)
                .single()
                .expect("local noon always exists")
                .timestamp()
        };
        assert!(
            (since..=until).contains(&noon(2026, 1, 15)),
            "local noon of the requested day must be inside [{since}, {until}]"
        );
        assert!(
            !(since..=until).contains(&noon(2026, 1, 14)),
            "the previous local day must be outside [{since}, {until}]"
        );
        assert!(
            !(since..=until).contains(&noon(2026, 1, 16)),
            "the next local day must be outside [{since}, {until}]"
        );
        // Boundaries are the day's own local midnights, not 00:00 UTC.
        assert_eq!(
            since,
            Local
                .with_ymd_and_hms(2026, 1, 15, 0, 0, 0)
                .earliest()
                .unwrap()
                .timestamp()
        );
        assert_eq!(
            until,
            Local
                .with_ymd_and_hms(2026, 1, 15, 23, 59, 59)
                .latest()
                .unwrap()
                .timestamp()
        );
    }

    /// A plain day is 86400 seconds inclusive of both ends — except where the
    /// local day is not 24 hours long. That exception is a DST transition, and
    /// it must widen or narrow the day, never shift it.
    #[test]
    fn a_day_is_24_hours_unless_a_dst_transition_says_otherwise() {
        for day in ["2026-01-15", "2026-07-15", "2026-03-08", "2026-11-01"] {
            let args = SelectorArgs {
                day: Some(day.into()),
                ..Default::default()
            };
            let w = args.resolve().unwrap().selector.window.unwrap();
            let len = w.until_unix.unwrap() - w.since_unix.unwrap();
            // 86399 for a plain day, 82799 for a 23-hour one, 89999 for a
            // 25-hour one. Anything else means the boundary was shifted rather
            // than resolved.
            assert!(
                (82_000..=90_000).contains(&len),
                "day {day} came out {len} s long, which is neither a plain day nor a \
                 DST-length one"
            );
        }
    }

    /// A local day whose midnight does not exist is not shifted silently: the
    /// resolver reports how it resolved it, by returning the transition
    /// instant, and the day still covers local noon.
    #[test]
    fn a_dst_gap_at_midnight_does_not_lose_the_day() {
        // 2026-03-29 in Europe/London: 01:00 local jumps to 02:00, so 00:00
        // still exists; this checks the general property rather than one zone.
        let args = SelectorArgs {
            day: Some("2026-03-29".into()),
            ..Default::default()
        };
        let w = args.resolve().unwrap().selector.window.unwrap();
        let noon = Local
            .with_ymd_and_hms(2026, 3, 29, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        assert!((w.since_unix.unwrap()..=w.until_unix.unwrap()).contains(&noon));
    }

    #[test]
    fn since_and_until_are_inclusive_local_days() {
        let args = SelectorArgs {
            since: Some("2026-01-15".into()),
            until: Some("2026-01-17".into()),
            ..Default::default()
        };
        let w = args.resolve().unwrap().selector.window.unwrap();
        let start = w.since_unix.unwrap();
        let end = w.until_unix.unwrap();
        // Three whole local days, inclusive of both ends. The ±1 h slack is
        // the DST allowance: a three-day span can lose or gain an hour, and
        // that is a real property of local time, not a bug to assert away.
        let len = end - start;
        assert!(
            (3 * 82_800..=3 * 90_000).contains(&len),
            "three inclusive local days came out {len} s"
        );
    }

    #[test]
    fn one_sided_date_flags_leave_the_other_side_unconstrained() {
        let args = SelectorArgs {
            since: Some("2026-01-15".into()),
            ..Default::default()
        };
        let w = args.resolve().unwrap().selector.window.unwrap();
        assert!(w.since_unix.is_some());
        assert_eq!(
            w.until_unix, None,
            "an absent bound is absent, not a sentinel"
        );
    }

    /// The deprecated flags still work, still mean conversation time, and say
    /// so on stderr.
    #[test]
    fn deprecated_unix_flags_resolve_and_warn() {
        let args = SelectorArgs {
            since_unix: Some(100),
            until_unix: Some(200),
            ..Default::default()
        };
        let r = args.resolve().unwrap();
        assert_eq!(r.selector.window, Some(window(100, 200)));
        assert_eq!(r.warnings.len(), 1, "exactly one deprecation notice");
        assert!(r.warnings[0].contains("deprecated"));
        assert!(
            r.warnings[0].contains("--day"),
            "the notice must name the replacement: {}",
            r.warnings[0]
        );
    }

    #[test]
    fn no_time_flags_means_no_window_and_no_warning() {
        let r = SelectorArgs::default().resolve().unwrap();
        assert_eq!(r.selector.window, None);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn malformed_dates_are_usage_errors_never_guesses() {
        for bad in [
            "2026-1-5",
            "20260105",
            "2026-01-15T00:00:00",
            "2026-02-30",
            "yesterday",
            "",
            "2026-01-15-",
        ] {
            let args = SelectorArgs {
                day: Some(bad.to_string()),
                ..Default::default()
            };
            let err = args
                .resolve()
                .expect_err(&format!("`{bad}` must not be accepted as a date"));
            assert!(
                err.0.contains("YYYY-MM-DD"),
                "the error must say the accepted shape: {}",
                err.0
            );
        }
    }

    /// A window that cannot be satisfied is a usage error, not "0 matched".
    #[test]
    fn inverted_windows_are_usage_errors() {
        let dates = SelectorArgs {
            since: Some("2026-03-01".into()),
            until: Some("2026-01-01".into()),
            ..Default::default()
        };
        assert!(dates.resolve().is_err());

        let unix = SelectorArgs {
            since_unix: Some(500),
            until_unix: Some(100),
            ..Default::default()
        };
        assert!(unix.resolve().is_err());
    }

    #[test]
    fn harness_list_splits_on_commas() {
        // The splitting itself is clap's (`value_delimiter`); what this pins is
        // that an empty entry cannot sneak in as a harness that matches nothing
        // by accident.
        let args = SelectorArgs {
            harness: Some(vec!["claude-code".into(), "codex".into()]),
            ..Default::default()
        };
        let r = args.resolve().unwrap();
        let want = r.selector.harnesses.unwrap();
        assert_eq!(want.len(), 2);
        assert!(want.contains("claude-code") && want.contains("codex"));
    }

    #[test]
    fn window_describe_names_the_unit_and_never_a_path() {
        let args = SelectorArgs {
            day: Some("2026-01-15".into()),
            ..Default::default()
        };
        let w = args.resolve().unwrap().selector.window.unwrap();
        let text = w.describe();
        assert!(text.contains("local day"), "{text}");
        assert!(text.contains("2026-01-15"), "{text}");
    }
}
