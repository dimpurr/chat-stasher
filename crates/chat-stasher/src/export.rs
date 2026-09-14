//! export — write every session the **shared selector** selects to files
//! (public issue #1).
//!
//! `export` is not a second query language. It calls
//! [`crate::search::search_sessions`] with the very same [`Selector`] the
//! `search` command builds from the same clap-flattened
//! [`SelectorArgs`](crate::selector::SelectorArgs), and writes exactly the set
//! that comes back — so `search` *is* the dry run of `export`, and a change that
//! made the two answer differently would have to change one shared function.
//! `export --dry-run` therefore prints the identical price `search --cost`
//! prints, because both read it off [`SearchReport::fulltext_cost`].
//!
//! What it writes, per selected session:
//!
//! ```text
//! <out>/<machine>/<harness>/<session-id>.jsonl
//! ```
//!
//! The bytes are the session's archived lines **in their native format**,
//! decrypted and concatenated in shard sequence order by the same rule `read`
//! uses ([`crate::store::BackupStore::read_selected_sessions`] /
//! [`crate::store::BackupStore::read_session_concat`]), so a written file is
//! byte-identical to what `read` returns for that session. With no filtering
//! flag the bytes are not touched at all — not re-encoded, not re-serialised,
//! not re-terminated.
//!
//! Two filters can rewrite a session, and both are opt-in:
//!
//! * `--turns user` keeps only the lines that are the user's own messages.
//!   That question has an answer only where the format makes it certain, and
//!   the harnesses that qualify are listed in [`USER_TURNS_HARNESSES`]. For any
//!   other harness the flag is **not** silently ignored and content is **not**
//!   silently dropped: every line is written and the session records
//!   `turns_filter: "not-supported"` in the manifest.
//! * `--trim-to-window` (only valid with a time window) drops lines whose own
//!   timestamp lies outside the window. A line whose timestamp cannot be read
//!   is **kept** and counted in `untimed_lines` — an unreadable time is not an
//!   out-of-window time, which is the same rule [`crate::selector`] applies to a
//!   whole session.
//!
//! Three states stay three states here too (invariant 1, and the reason the
//! exit codes are what they are):
//!
//! * a session the selector **chose** but that could not be read or written is
//!   listed in `sessions_failed` with the reason — the file is absent and the
//!   manifest says why, so its absence cannot be misread as "there was nothing";
//! * a session no active filter could **place** travels through from `search`
//!   into `sessions_not_placed`, with its dimension and reason;
//! * a part of the destination that could not be read travels through into
//!   `unreadable_parts`.
//!
//! Any of the three makes the exit status `3`: output was produced, but it is
//! not the whole answer, and the manifest carries the `exit_status` it will
//! return so a later reader of the directory can tell what it holds.
//!
//! Safety of the write itself: `--out` must be empty or absent unless `--force`
//! is given, nothing is ever deleted, and every path component written comes
//! from an archived id that has been checked to be a single path segment — a
//! session id containing a separator is recorded as a failure, never followed.
//!
//! Nothing below `--out` is written through a symlink either, because
//! [`std::fs::create_dir_all`] and [`std::fs::write`] both follow one: a
//! pre-existing `<out>/<machine>` — or a deeper directory, or the target file
//! itself — pointing outside `--out` would put archived conversation content
//! somewhere the user did not name. `--out` itself may be a symlink; naming it
//! *is* the choice of where to write, and that choice is honoured. A symlink
//! anywhere below it makes that session a recorded failure ([`write_refusal`]),
//! so the run is a `3` and never a quiet success; one planted at the manifest's
//! own path is refused too, as the write failure it is.
//!
//! Known gap, the same one the rest of the codebase has: Windows reserved
//! device names (`CON`, `NUL`, …) and components ending in a dot or a space are
//! not handled here. They are refused or mangled by the platform, not by this
//! module, and nothing below `--out` is written differently because of them.
//!
//! Privacy line: the manifest carries ids, counts, byte lengths, digests and
//! timestamps. It never carries conversation text, and this module never prints
//! any.

use anyhow::Context;
use rustic_core::repofile::MasterKey;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::activity::analyze_session;
use crate::json_out::TimeState;
use crate::search::{search_sessions, SessionHit};
use crate::selector::{Selector, TimeWindow, UnplacedBy, UsageError};
use crate::store::BackupStore;

/// File name of the manifest written at the root of `--out`.
pub const MANIFEST_NAME: &str = "manifest.json";

/// Directory name used for a session whose archived id carries no harness
/// prefix. Angle brackets, like `main.rs`'s `UNRESOLVED_MACHINE`: it is
/// deliberately not a value a harness could be called, so it cannot collide
/// with a real harness directory and cannot be mistaken for one.
pub const NO_HARNESS_DIR: &str = "<no-harness-prefix>";

/// Harnesses where "this line is the user's own message" is determinable from
/// the archived line alone.
///
/// Claude Code JSONL qualifies: every line is one message with a top-level
/// `type`, and a tool's output is routed through `type: "user"` with a
/// `tool_result` content block — which is precisely why `type == "user"` alone
/// is not the rule. Everything else stays out of this list until its format is
/// certain, because a wrong entry here silently deletes content.
pub const USER_TURNS_HARNESSES: &[&str] = &["claude-code"];

/// `--turns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turns {
    /// Write every archived line.
    All,
    /// Write only the user's own messages, where that is determinable.
    User,
}

impl Turns {
    pub fn as_str(self) -> &'static str {
        match self {
            Turns::All => "all",
            Turns::User => "user",
        }
    }
}

/// How `--turns user` actually applied to one session. Recorded per session
/// rather than assumed once per run, because it is a property of the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TurnsOutcome {
    /// The harness is known and the filter ran.
    Applied,
    /// The harness's format does not make the question answerable, so every
    /// line was written and nothing was dropped.
    NotSupported,
    /// `--turns all` (the default): no filter was asked for.
    NotRequested,
}

/// What the run will cost, read off the selected set before anything is
/// fetched. The same four numbers `search --cost` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ExportPlan {
    pub sessions: usize,
    pub shards: usize,
    pub data_blobs: usize,
    pub plaintext_bytes: u64,
}

/// Options that are not part of the selector.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub out: PathBuf,
    pub turns: Turns,
    /// Trim written lines to the selector's window. Rejected without a window.
    pub trim_to_window: bool,
    /// Write into a non-empty `--out`. Nothing is ever deleted either way.
    pub force: bool,
    /// Print the plan and stop.
    pub dry_run: bool,
}

/// One session that a filter could not place — carried through from `search`.
#[derive(Debug, Clone, Serialize)]
pub struct NotPlacedSession {
    pub machine: String,
    pub session_id: String,
    pub harness: Option<String>,
    /// `"time"` or `"harness"` — which active filter had no answer.
    pub dimension: &'static str,
    /// The filter that could not be evaluated, and why. User-facing.
    pub why: String,
}

/// One session the selector chose that could not be written.
///
/// Its absence from disk is explained here and nowhere else, which is why the
/// run cannot exit 0 while this list is non-empty.
#[derive(Debug, Clone, Serialize)]
pub struct FailedSession {
    pub machine: String,
    pub session_id: String,
    pub harness: Option<String>,
    pub why: String,
}

/// One written session file.
#[derive(Debug, Clone, Serialize)]
pub struct ExportedSession {
    pub machine: String,
    pub harness: Option<String>,
    pub session_id: String,
    /// Path of the written file, relative to `--out`.
    pub relative_path: String,
    /// Snapshot the bytes came from (the machine's newest at read time).
    pub snapshot_id: String,
    /// Earliest conversation time from the activity index — a tagged unknown
    /// with its reason, never a null and never a zero.
    pub first_message: TimeState,
    /// Latest conversation time, same encoding.
    pub last_message: TimeState,
    /// Sealed shards concatenated into the file.
    pub shard_count: usize,
    /// Lines in the session as archived, before any filter.
    pub lines_total: u64,
    /// Lines actually written.
    pub lines_written: u64,
    /// Written lines whose own timestamp could not be read. Always kept; this
    /// number exists so a trim can never look complete when it was not.
    pub untimed_lines: u64,
    pub bytes_written: u64,
    /// sha256 of the written file — of the file on disk, not of the source.
    pub sha256: String,
    pub turns_filter: TurnsOutcome,
    /// Whether the line trim ran for this session.
    pub trimmed_to_window: bool,
}

impl ExportedSession {
    /// Privacy-safe short form, the same rule the rest of the CLI uses.
    pub fn short_id(&self) -> String {
        crate::id::short_session_id(&self.session_id)
    }
}

/// The full result of one `export` run.
#[derive(Debug, Clone)]
pub struct ExportReport {
    pub destination: String,
    /// Where files were written (or would be, for `--dry-run`).
    pub out: String,
    pub selector: Selector,
    pub window: Option<TimeWindow>,
    pub turns: Turns,
    pub trim_to_window: bool,
    pub dry_run: bool,
    pub plan: ExportPlan,
    /// Sessions the selector chose. `sessions.len() != selected` means some
    /// could not be written — see [`Self::failed`].
    pub selected: usize,
    /// Sessions every active filter evaluated and rejected.
    pub not_matched: usize,
    pub sessions: Vec<ExportedSession>,
    pub failed: Vec<FailedSession>,
    pub not_placed: Vec<NotPlacedSession>,
    /// Machines that hold sessions but no activity index beside them.
    pub machines_without_index: Vec<String>,
    /// Parts of the destination that could not be read.
    pub unreadable: Vec<String>,
    pub bytes_written: u64,
    /// Path of the written manifest, `None` for a dry run.
    pub manifest_path: Option<String>,
}

impl ExportReport {
    /// Whether the destination was read in full.
    pub fn complete(&self) -> bool {
        self.unreadable.is_empty()
    }

    /// Whether the written set is the whole answer to the query the user asked.
    ///
    /// Deliberately the same shape and the same strictness as
    /// [`crate::search::SearchReport::answer_complete`]: a run that wrote files and also found
    /// a session it could not place, or a session it could not write, has not
    /// answered the question and must not exit 0.
    pub fn answer_complete(&self) -> bool {
        self.complete() && self.not_placed.is_empty() && self.failed.is_empty()
    }

    /// The exit status this run returns, also recorded in the manifest so the
    /// directory explains itself later.
    ///
    /// `3` = did not finish reading, or did not write everything it selected
    /// (partial output is still on disk, and the manifest says what is
    /// missing) · `0` = at least one session selected, nothing missing, and the
    /// answer complete · `1` = read everything, answered for every session, and
    /// nothing was selected. `2` is reserved for usage errors and is decided
    /// before this point, by the caller.
    ///
    /// A dry run reports what a real run *would* return: nothing is written, so
    /// "written fewer than selected" is its normal state and must not be read
    /// as a partial write.
    pub fn exit_status(&self) -> u8 {
        if !self.answer_complete() {
            return 3;
        }
        if self.selected == 0 {
            return 1;
        }
        if !self.dry_run && self.sessions.len() != self.selected {
            // A session was selected and is neither written nor listed as
            // failed. That cannot happen, and if it ever does, saying "0" would
            // be a claim about files nobody counted.
            return 3;
        }
        0
    }

    /// How many sessions could not be placed in time specifically, counted
    /// from the tagged dimension rather than from the reason text.
    pub fn session_time_unknown(&self) -> usize {
        self.not_placed
            .iter()
            .filter(|u| u.dimension == "time")
            .count()
    }

    /// The full manifest, as one JSON object plus a trailing newline.
    pub fn manifest_json(&self) -> String {
        let sessions: Vec<serde_json::Value> = self
            .sessions
            .iter()
            .map(|s| {
                serde_json::json!({
                    "machine": s.machine,
                    "harness": s.harness,
                    "session_id": s.session_id,
                    "relative_path": s.relative_path,
                    "snapshot_id": s.snapshot_id,
                    "first_message": s.first_message,
                    "last_message": s.last_message,
                    "shard_count": s.shard_count,
                    "lines_total": s.lines_total,
                    "lines_written": s.lines_written,
                    "untimed_lines": s.untimed_lines,
                    "bytes_written": s.bytes_written,
                    "sha256": s.sha256,
                    "turns_filter": s.turns_filter,
                    "trimmed_to_window": s.trimmed_to_window,
                })
            })
            .collect();
        let not_placed: Vec<serde_json::Value> = self
            .not_placed
            .iter()
            .map(|u| {
                serde_json::json!({
                    "machine": u.machine,
                    "harness": u.harness,
                    "session_id": u.session_id,
                    "dimension": u.dimension,
                    "why": u.why,
                })
            })
            .collect();
        let failed: Vec<serde_json::Value> = self
            .failed
            .iter()
            .map(|f| {
                serde_json::json!({
                    "machine": f.machine,
                    "harness": f.harness,
                    "session_id": f.session_id,
                    "why": f.why,
                })
            })
            .collect();
        let v = serde_json::json!({
            "tool": "chat-stasher",
            "command": "export",
            "format_version": 1,
            "destination": self.destination,
            "out": self.out,
            "flags": {
                "turns": self.turns.as_str(),
                "trim_to_window": self.trim_to_window,
                "dry_run": self.dry_run,
                "machine": self.selector.machine,
                "harness": self
                    .selector
                    .harnesses
                    .as_ref()
                    .map(|h| h.iter().cloned().collect::<Vec<_>>()),
                "session_prefix": self.selector.session_id_prefix,
                "time_window": self.window.as_ref().map(|w| serde_json::json!({
                    "how": match w.how {
                        crate::selector::WindowHow::LocalDays => "local_days",
                        crate::selector::WindowHow::UnixSeconds => "unix_seconds",
                    },
                    "since_unix": w.since_unix,
                    "until_unix": w.until_unix,
                    "description": w.describe(),
                })),
            },
            "cost": {
                "sessions": self.plan.sessions,
                "shards": self.plan.shards,
                "data_blobs": self.plan.data_blobs,
                "plaintext_bytes": self.plan.plaintext_bytes,
            },
            "selected": self.selected,
            "not_matched": self.not_matched,
            "written": self.sessions.len(),
            "bytes_written": self.bytes_written,
            "exit_status": self.exit_status(),
            "sessions": sessions,
            "sessions_failed": failed,
            "sessions_not_placed": not_placed,
            "time_unknown": self.session_time_unknown(),
            "machines_without_activity_index": self.machines_without_index,
            "unreadable_parts": self.unreadable,
        });
        let mut s = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into());
        s.push('\n');
        s
    }
}

/// The output could not be written, after the archive had been read.
///
/// Its own type because it is *not* "the archive could not be read": every byte
/// was read, and then the run failed to put it somewhere. That is exit `1` —
/// "finished reading and failed" — not `3`.
#[derive(Debug)]
pub struct OutputWriteError(pub String);

impl std::fmt::Display for OutputWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OutputWriteError {}

/// Validate `--out`. A usage error (`2`), carried as [`UsageError`] — the same
/// type `selector` uses for a date that is not a date, because it is the same
/// claim: the arguments cannot be turned into a run. The user fixes it by
/// naming another directory or passing `--force`, and no archive question is
/// involved. Nothing is ever deleted here or later, so `--force` means
/// "overwrite the files this run writes", never "clean the directory first".
///
/// This only *checks* — it creates nothing. The directory is created by
/// [`export_sessions`] once the archive has been read, so a run that fails to
/// read leaves no empty directory behind pretending to be an output.
pub fn check_out(out: &Path, force: bool) -> Result<(), UsageError> {
    if out.as_os_str().is_empty() {
        return Err(UsageError(
            "`--out` is empty — name the directory the sessions should be written to".to_string(),
        ));
    }
    if out.exists() && !out.is_dir() {
        return Err(UsageError(format!(
            "`--out {}` exists and is not a directory — refusing to touch it",
            out.display()
        )));
    }
    if out.is_dir() && !force {
        let mut entries = std::fs::read_dir(out)
            .map_err(|e| UsageError(format!("`--out {}` cannot be listed: {e}", out.display())))?;
        if entries.next().is_some() {
            return Err(UsageError(format!(
                "`--out {}` is not empty — export would add files to a directory that already holds something. \
                 Pass `--force` to write into it anyway (nothing is ever deleted), or choose an empty directory",
                out.display()
            )));
        }
    }
    Ok(())
}

/// The exit status an `Err` out of [`export_sessions`] must produce.
///
/// Three distinct answers, and they are distinct on purpose: `2` = the command
/// cannot be turned into a run at all, `1` = the archive was read to the end and
/// the run then failed while writing (the output directory or the manifest),
/// `3` = the archive itself could not be read, so nothing here proves anything
/// about what it holds.
pub fn exit_status_for_error(e: &anyhow::Error) -> u8 {
    if e.downcast_ref::<UsageError>().is_some() {
        2
    } else if e.downcast_ref::<OutputWriteError>().is_some() {
        1
    } else {
        3
    }
}

/// One archived line with its own terminator, so re-joining the kept lines
/// reproduces the kept bytes exactly.
fn line_spans(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            out.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(&bytes[start..]);
    }
    out
}

/// Whether one archived line is a message the user themselves sent.
///
/// Only called for harnesses in [`USER_TURNS_HARNESSES`]. The rule is the one
/// the format makes certain: the line says `type: "user"` **and** it is not a
/// tool result. A line that is not JSON at all cannot say `type: "user"`, so it
/// is not a user message — under a filter that asks for user messages only,
/// that is a real answer, not an unreadable one.
fn is_user_message(line: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(line) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
        return false;
    };
    if value.get("type").and_then(|t| t.as_str()) != Some("user") {
        return false;
    }
    is_tool_result(&value)
        .map(|tool_result| !tool_result)
        .unwrap_or(true)
}

/// Whether a `type: "user"` line is actually a tool's output.
///
/// Claude Code routes a tool result through the user role, so `type == "user"`
/// on its own would keep every tool output in the export. `None` means "this
/// line does not carry a content shape we recognise as a tool result", which
/// the caller reads as "not a tool result" — a line is only ever dropped for a
/// tool result we actually found.
fn is_tool_result(value: &serde_json::Value) -> Option<bool> {
    let blocks = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())?;
    Some(
        blocks
            .iter()
            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result")),
    )
}

/// Where one line stands relative to the window.
enum LineTiming {
    /// A time was read; the flag says whether the line is inside the window.
    Timed(bool),
    /// No time could be read. Kept, and counted — "no time" is not "outside".
    Untimed,
}

/// Resolve one line's own conversation time, through the very same extractor
/// the activity index uses, so a trimmed line and an indexed session cannot
/// disagree about when something happened.
fn line_timing(harness: &str, line: &[u8], window: &TimeWindow) -> LineTiming {
    let Ok(text) = std::str::from_utf8(line) else {
        return LineTiming::Untimed;
    };
    let analysis = analyze_session(harness, &[text]);
    match (analysis.first_unix, analysis.last_unix) {
        (Some(first), Some(last)) => {
            // Intersection, inclusive on both ends — the rule `Selector::select`
            // applies to a whole session, applied to one line.
            let after_start = window.since_unix.is_none_or(|s| last >= s);
            let before_end = window.until_unix.is_none_or(|u| first <= u);
            LineTiming::Timed(after_start && before_end)
        }
        _ => LineTiming::Untimed,
    }
}

/// What one session's bytes look like after the active filters.
struct FilteredSession {
    bytes: Vec<u8>,
    lines_total: u64,
    lines_written: u64,
    untimed_lines: u64,
    turns: TurnsOutcome,
    trimmed: bool,
}

/// Apply `--turns` and `--trim-to-window` to one session's archived bytes.
///
/// With neither filter active the input is returned untouched — byte for byte,
/// which is what makes an unfiltered export identical to `read`. The turns
/// filter runs first, so `untimed_lines` counts lines that survived it: it
/// answers "of the lines I wrote, how many did I write without knowing when
/// they happened".
fn filter_session(
    harness: Option<&str>,
    raw: &[u8],
    opts: &ExportOptions,
    window: Option<&TimeWindow>,
) -> FilteredSession {
    let harness_name = harness.unwrap_or("");
    let turns = match opts.turns {
        Turns::All => TurnsOutcome::NotRequested,
        Turns::User => {
            if USER_TURNS_HARNESSES.contains(&harness_name) {
                TurnsOutcome::Applied
            } else {
                TurnsOutcome::NotSupported
            }
        }
    };
    let trimmed = opts.trim_to_window && window.is_some();
    let spans = line_spans(raw);
    let lines_total = spans.len() as u64;

    if turns == TurnsOutcome::NotRequested && !trimmed {
        return FilteredSession {
            bytes: raw.to_vec(),
            lines_total,
            lines_written: lines_total,
            untimed_lines: 0,
            turns,
            trimmed: false,
        };
    }

    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut written = 0u64;
    let mut untimed = 0u64;
    for line in spans {
        if turns == TurnsOutcome::Applied && !is_user_message(line) {
            continue;
        }
        if let Some(window) = window.filter(|_| trimmed) {
            match line_timing(harness_name, line, window) {
                LineTiming::Timed(true) => {}
                LineTiming::Timed(false) => continue,
                LineTiming::Untimed => untimed += 1,
            }
        }
        out.extend_from_slice(line);
        written += 1;
    }
    FilteredSession {
        bytes: out,
        lines_total,
        lines_written: written,
        untimed_lines: untimed,
        turns,
        trimmed,
    }
}

/// Whether `name` is safe to use as a single path component below `--out`.
///
/// This is the whole of "never writes outside `--out`": a name that is not
/// exactly one ordinary segment (empty, `.`, `..`, or carrying a separator) is
/// refused and the session is recorded as failed, never written somewhere else.
fn is_safe_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

impl SessionHit {
    /// Directory name for this session's harness inside `--out`.
    fn harness_dir(&self) -> &str {
        self.harness.as_deref().unwrap_or(NO_HARNESS_DIR)
    }
}

/// Export every session the selector selects.
///
/// `on_plan` is called once, after the selection is known and **before any
/// session byte is fetched** — that is what lets the caller print the price
/// first and still let the run proceed in one call. A `--dry-run` stops right
/// there: no directory is created, no file is written, and the returned report
/// carries the exit status the real run would have returned.
///
/// A failure to open or read the repository is an `Err`, exactly as in
/// `search`: "I could not read this destination" must never be rendered as an
/// empty export. A failure to write one session is not an `Err` — the rest of
/// the export is still worth having — it is a [`FailedSession`], and it makes
/// the exit status 3.
pub fn export_sessions(
    store: &BackupStore,
    mk: &MasterKey,
    selector: &Selector,
    opts: &ExportOptions,
    on_plan: &dyn Fn(&ExportPlan),
) -> anyhow::Result<ExportReport> {
    if opts.trim_to_window && selector.window.is_none() {
        // A no-op flag is a lie about what the run did. Same family as an
        // inverted window in `selector`: refuse before reading anything.
        return Err(anyhow::Error::new(UsageError(
            "`--trim-to-window` needs a time window to trim to: pass `--day`, `--since` or `--until` as well"
                .to_string(),
        )));
    }
    // The same `--out` is refused in a dry run as in a real one: a dry run that
    // said "fine" about a directory the real run would reject would be a plan
    // nobody can act on. Nothing is created here — see the `create_dir_all`
    // after the read, which is what keeps a failed read from leaving an empty
    // directory that looks like an output.
    check_out(&opts.out, opts.force).map_err(anyhow::Error::new)?;

    let report = search_sessions(store, mk, selector)?;
    let cost = report.fulltext_cost();
    let plan = ExportPlan {
        sessions: cost.sessions,
        shards: cost.shards,
        data_blobs: cost.data_blobs,
        plaintext_bytes: cost.plaintext_bytes,
    };
    on_plan(&plan);

    let mut out = ExportReport {
        destination: report.destination.clone(),
        out: opts.out.display().to_string(),
        selector: selector.clone(),
        window: report.window.clone(),
        turns: opts.turns,
        trim_to_window: opts.trim_to_window,
        dry_run: opts.dry_run,
        plan,
        selected: report.hits.len(),
        not_matched: report.not_matched,
        sessions: Vec::new(),
        failed: Vec::new(),
        not_placed: report
            .unplaced
            .iter()
            .map(|u| NotPlacedSession {
                machine: u.machine.clone(),
                session_id: u.session_id.clone(),
                harness: u.harness.clone(),
                dimension: match u.dimension {
                    UnplacedBy::Time => "time",
                    UnplacedBy::Harness => "harness",
                },
                why: u.why.clone(),
            })
            .collect(),
        machines_without_index: report.machines_without_index.clone(),
        unreadable: report.unreadable.clone(),
        bytes_written: 0,
        manifest_path: None,
    };
    if opts.dry_run {
        return Ok(out);
    }

    let wanted: BTreeSet<(String, String)> = report
        .hits
        .iter()
        .map(|h| (h.machine.clone(), h.session_id.clone()))
        .collect();
    let dumped = store.read_selected_sessions(mk, &wanted)?;

    // Everything that could fail before the first byte is now behind us: the
    // archive was read. Only now is the output directory created, so a read
    // that failed leaves nothing on disk that could be mistaken for an output.
    if let Err(e) = std::fs::create_dir_all(&opts.out) {
        return Err(anyhow::Error::new(OutputWriteError(format!(
            "`--out {}` could not be created: {e} — the archive was read, but nothing was written",
            opts.out.display()
        ))));
    }

    for hit in &report.hits {
        let bytes = match dumped.get(&(hit.machine.clone(), hit.session_id.clone())) {
            Some((bytes, _shards)) => bytes,
            None => {
                out.failed.push(FailedSession {
                    machine: hit.machine.clone(),
                    session_id: hit.session_id.clone(),
                    harness: hit.harness.clone(),
                    why: format!(
                        "the newest snapshot of machine `{}` no longer holds this session, so its \
                         bytes could not be read back — the session is absent from this export, \
                         which is not the same as it being empty",
                        hit.machine
                    ),
                });
                continue;
            }
        };
        let filtered = filter_session(hit.harness.as_deref(), bytes, opts, report.window.as_ref());
        let relative = format!(
            "{}/{}/{}.jsonl",
            hit.machine,
            hit.harness_dir(),
            hit.session_id
        );
        if let Some(bad) = first_unsafe_component(hit) {
            out.failed.push(FailedSession {
                machine: hit.machine.clone(),
                session_id: hit.session_id.clone(),
                harness: hit.harness.clone(),
                why: format!(
                    "`{bad}` is not usable as a single directory name below `--out`, and this \
                     command never writes outside it; no file was written for this session"
                ),
            });
            continue;
        }
        let path = opts.out.join(&relative);
        if let Some(why) = write_refusal(&opts.out, &path) {
            out.failed.push(FailedSession {
                machine: hit.machine.clone(),
                session_id: hit.session_id.clone(),
                harness: hit.harness.clone(),
                why: format!(
                    "refusing to write `{}`: {why} — this command never writes outside `--out`",
                    path.display()
                ),
            });
            continue;
        }
        if let Err(e) = write_file(&path, &filtered.bytes) {
            out.failed.push(FailedSession {
                machine: hit.machine.clone(),
                session_id: hit.session_id.clone(),
                harness: hit.harness.clone(),
                why: format!("could not write `{}`: {e}", path.display()),
            });
            continue;
        }
        let why_time = hit.time_why.as_deref();
        out.bytes_written += filtered.bytes.len() as u64;
        out.sessions.push(ExportedSession {
            machine: hit.machine.clone(),
            harness: hit.harness.clone(),
            session_id: hit.session_id.clone(),
            relative_path: relative,
            snapshot_id: hit.snapshot_id.clone(),
            first_message: time_state(hit.first_unix, why_time),
            last_message: time_state(hit.last_unix, why_time),
            shard_count: hit.shard_count,
            lines_total: filtered.lines_total,
            lines_written: filtered.lines_written,
            untimed_lines: filtered.untimed_lines,
            bytes_written: filtered.bytes.len() as u64,
            sha256: hex_digest(&Sha256::digest(&filtered.bytes)),
            turns_filter: filtered.turns,
            trimmed_to_window: filtered.trimmed,
        });
    }

    let manifest = opts.out.join(MANIFEST_NAME);
    // The manifest is a write below `--out` too, and the same rule holds: a
    // symlink planted at `<out>/manifest.json` would put it outside. Refused
    // here rather than recorded, because there is no manifest left to record it
    // in — this is the same "read everything, then failed to write" state as an
    // unwritable manifest, so it is the same error and the same exit `1`.
    if let Some(why) = write_refusal(&opts.out, &manifest) {
        return Err(anyhow::Error::new(OutputWriteError(format!(
            "refusing to write the manifest `{}`: {why} — the session files above are on disk, \
             but without the manifest nothing records which sessions they are or that the run \
             finished",
            manifest.display()
        ))));
    }
    if let Err(e) = std::fs::write(&manifest, out.manifest_json())
        .with_context(|| format!("write manifest `{}`", manifest.display()))
    {
        return Err(anyhow::Error::new(OutputWriteError(format!(
            "{e:#} — the session files above are on disk, but without the manifest nothing records \
             which sessions they are or that the run finished"
        ))));
    }
    out.manifest_path = Some(manifest.display().to_string());
    Ok(out)
}

/// The first published id component that cannot be a path segment, if any.
fn first_unsafe_component(hit: &SessionHit) -> Option<String> {
    for name in [
        Some(hit.machine.as_str()),
        hit.harness.as_deref(),
        Some(hit.session_id.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        if !is_safe_component(name) {
            return Some(name.to_string());
        }
    }
    None
}

fn time_state(unix: Option<i64>, why: Option<&str>) -> TimeState {
    match unix {
        Some(unix) => TimeState::known(unix),
        None => TimeState::unknown(
            why.unwrap_or("no conversation time was recorded for this session")
                .to_string(),
        ),
    }
}

/// Why `path`, whose destination is below `out`, must not be written — or
/// `None` when it may be.
///
/// `out` itself is not inspected: naming a symlink as `--out` is the user's
/// explicit choice of destination, and honouring it is the whole point of that
/// choice. Everything *below* `out` must be ordinary, because
/// [`std::fs::create_dir_all`] and [`std::fs::write`] both follow a symlink: a
/// pre-existing `<out>/<machine>` (or a deeper directory, or the target file
/// itself) pointing outside `out` would put archived conversation content
/// outside the directory the user named.
///
/// Every component below `out` that already exists is inspected with
/// [`std::fs::symlink_metadata`], which sees a symlink as a symlink rather than
/// as whatever it points at. A component that is absent is fine — it will be
/// created, and a thing that is not there cannot be a link. A component that
/// exists but cannot be inspected is *not* fine: "unreadable" is not "absent",
/// and writing through something whose nature is unknown is the state this
/// refuses. The target itself, when it is already there, must be a regular
/// file: a directory or a device would make the write fail or do something the
/// user did not ask for.
///
/// This inspects before the write, it is not an atomic open: it closes the case
/// of a symlink that is already there. A concurrent process that swaps a
/// component in between this check and the write is beyond what it can see.
fn write_refusal(out: &Path, path: &Path) -> Option<String> {
    let Ok(relative) = path.strip_prefix(out) else {
        return Some(format!(
            "`{}` is not below `--out {}`",
            path.display(),
            out.display()
        ));
    };
    let mut current = out.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Some(format!("`{}` is a symlink", current.display()));
            }
            // An ordinary component. Whether it is a directory is not this
            // check's business: `create_dir_all` below says so, in its own
            // words, if it is not.
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Some(format!(
                    "`{}` exists but could not be inspected: {e}",
                    current.display()
                ));
            }
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => Some(format!(
            "`{}` exists and is not a regular file",
            path.display()
        )),
        _ => None,
    }
}

fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The header the caller prints **before** the run: what was asked for, so the
/// price that follows can be read against it. The cost line itself belongs to
/// the plan callback, which fires once the selection is known and before any
/// session byte is fetched.
pub fn print_header(
    destination: &str,
    opts: &ExportOptions,
    window: Option<&TimeWindow>,
) -> Vec<String> {
    let mut lines = vec![
        format!("[export] destination  : {destination}"),
        format!("[export] out          : {}", opts.out.display()),
    ];
    match window {
        Some(w) => lines.push(format!("[export] time window  : {}", w.describe())),
        None => lines.push(
            "[export] time window  : none — every session is selected, whatever its time"
                .to_string(),
        ),
    }
    lines.push(format!(
        "[export] flags        : turns={} trim_to_window={}{}",
        opts.turns.as_str(),
        opts.trim_to_window,
        if opts.dry_run { " dry_run=true" } else { "" },
    ));
    lines
}

/// One line naming what the run will cost. Called once per run, before the
/// payload is fetched; the four numbers are `search --cost`'s, read off the
/// same [`ExportPlan`], so the two commands cannot quote different prices.
pub fn print_plan(plan: &ExportPlan) -> String {
    format!(
        "[export] cost         : sessions={}  shards={}  data_blobs={}  plaintext_bytes={}",
        plan.sessions, plan.shards, plan.data_blobs, plan.plaintext_bytes
    )
}

/// The human report, printed after the run. Lines only — the manifest is the
/// machine-readable form of the same facts.
pub fn print_report(report: &ExportReport) -> Vec<String> {
    let mut lines = Vec::new();
    if report.dry_run {
        lines.push(
            "[export] dry run      : nothing was written, no directory was created".to_string(),
        );
        lines.push(format!(
            "[export] would select : {} session(s), {} not matched, {} could not be placed",
            report.selected,
            report.not_matched,
            report.not_placed.len()
        ));
        lines.push(format!("[export] exit status  : {}", report.exit_status()));
        return lines;
    }

    lines.push(format!(
        "[export] written      : {} of {} selected session(s), {} bytes",
        report.sessions.len(),
        report.selected,
        report.bytes_written
    ));
    for s in &report.sessions {
        lines.push(format!(
            "  {}  machine={}  harness={}  shards={}  lines={}/{}  untimed={}  bytes={}  sha256={}  turns={}  trim={}",
            s.short_id(),
            s.machine,
            s.harness.as_deref().unwrap_or("unknown"),
            s.shard_count,
            s.lines_written,
            s.lines_total,
            s.untimed_lines,
            s.bytes_written,
            &s.sha256[..s.sha256.len().min(12)],
            match s.turns_filter {
                TurnsOutcome::Applied => "applied",
                TurnsOutcome::NotSupported => "not-supported",
                TurnsOutcome::NotRequested => "not-requested",
            },
            if s.trimmed_to_window { "on" } else { "off" },
        ));
    }
    if report.session_time_unknown() > 0 {
        lines.push(format!(
            "[export] time unknown : {}",
            report.session_time_unknown()
        ));
    }
    for machine in &report.machines_without_index {
        lines.push(format!(
            "  !! no activity index for machine `{machine}` — its sessions cannot be placed in time"
        ));
    }
    for path in &report.unreadable {
        lines.push(format!("  !! unreadable: {path}"));
    }
    if !report.not_placed.is_empty() {
        lines.push(format!(
            "[export] not placed   : {} (NOT 'not matched' — an active filter had no answer for these, so they are absent from the export)",
            report.not_placed.len()
        ));
        for u in &report.not_placed {
            lines.push(format!(
                "  {}  machine={}  harness={}  why: {}",
                crate::id::short_session_id(&u.session_id),
                u.machine,
                u.harness.as_deref().unwrap_or("unknown"),
                u.why
            ));
        }
    }
    if !report.failed.is_empty() {
        lines.push(format!(
            "[export] failed       : {} selected session(s) could not be written (their files are absent; this run did not produce them)",
            report.failed.len()
        ));
        for f in &report.failed {
            lines.push(format!(
                "  {}  machine={}  why: {}",
                crate::id::short_session_id(&f.session_id),
                f.machine,
                f.why
            ));
        }
    }
    if report.selected == 0 {
        lines.push(format!(
            "[export] nothing selected: 0 of {} session(s) matched in `{}`{}",
            report.selected + report.not_matched,
            report.destination,
            if report.not_placed.is_empty() {
                ""
            } else {
                " — and some sessions could not be placed, so this is NOT 'there was nothing'"
            }
        ));
    }
    if let Some(manifest) = &report.manifest_path {
        lines.push(format!("[export] manifest     : {manifest}"));
    }
    lines.push(format!("[export] exit status  : {}", report.exit_status()));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::WindowHow;

    fn window(since: i64, until: i64) -> TimeWindow {
        TimeWindow {
            since_unix: Some(since),
            until_unix: Some(until),
            how: WindowHow::UnixSeconds,
            since_text: Some(since.to_string()),
            until_text: Some(until.to_string()),
        }
    }

    fn opts(turns: Turns, trim: bool) -> ExportOptions {
        ExportOptions {
            out: PathBuf::from("/nonexistent-for-unit-tests"),
            turns,
            trim_to_window: trim,
            force: false,
            dry_run: true,
        }
    }

    const USER_LINE: &str = r#"{"type":"user","message":{"role":"user","content":"hello"},"timestamp":"2026-01-15T10:00:00Z"}"#;
    const ASSISTANT_LINE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]},"timestamp":"2026-01-15T10:00:05Z"}"#;
    const TOOL_RESULT_LINE: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"output"}]},"timestamp":"2026-01-15T10:00:09Z"}"#;
    /// Unix seconds of `USER_LINE`'s own timestamp, so a window in these tests
    /// is built around the fixture rather than around a hand-computed guess.
    const SESSION_DAY_START: i64 = 1_768_471_200; // 2026-01-15T10:00:00Z

    fn joined(lines: &[&str]) -> Vec<u8> {
        let mut s = String::new();
        for l in lines {
            s.push_str(l);
            s.push('\n');
        }
        s.into_bytes()
    }

    // -------------------------------------------------------- byte identity

    /// With no filter the input bytes come back untouched. This is the unit-level
    /// form of "an unfiltered export file equals what `read` returns": the
    /// filter is not allowed to normalise a line ending, a trailing newline, or
    /// anything else it did not have to touch.
    #[test]
    fn no_filter_returns_the_archived_bytes_unchanged() {
        // No trailing newline on the last line, deliberately: a re-serialiser
        // would add one, and byte identity would be gone.
        let raw = b"{\"a\":1}\n{\"b\":2}".to_vec();
        let f = filter_session(Some("claude-code"), &raw, &opts(Turns::All, false), None);
        assert_eq!(f.bytes, raw);
        assert_eq!(f.lines_total, 2);
        assert_eq!(f.lines_written, 2);
        assert_eq!(f.untimed_lines, 0);
        assert_eq!(f.turns, TurnsOutcome::NotRequested);
    }

    /// `--turns user` keeps the user's own messages, drops the assistant's, and
    /// drops a tool result — which arrives with `type: "user"` and would be
    /// kept by the naive rule.
    #[test]
    fn user_turns_keep_user_messages_and_not_tool_results() {
        let raw = joined(&[USER_LINE, ASSISTANT_LINE, TOOL_RESULT_LINE]);
        let f = filter_session(Some("claude-code"), &raw, &opts(Turns::User, false), None);
        assert_eq!(f.turns, TurnsOutcome::Applied);
        assert_eq!(f.lines_total, 3);
        assert_eq!(f.lines_written, 1, "only the user's own message survives");
        let text = String::from_utf8(f.bytes).unwrap();
        assert_eq!(text, format!("{USER_LINE}\n"));
    }

    /// A user message written as content blocks (no `tool_result` among them)
    /// is still the user's own message.
    #[test]
    fn user_message_with_content_blocks_is_kept() {
        let line =
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#;
        let raw = joined(&[line]);
        let f = filter_session(Some("claude-code"), &raw, &opts(Turns::User, false), None);
        assert_eq!(f.lines_written, 1);
    }

    /// A harness whose format cannot answer the question is never silently
    /// filtered: every line is written and the outcome says so.
    #[test]
    fn an_unsupported_harness_is_recorded_not_silently_dropped() {
        let raw = joined(&[USER_LINE, ASSISTANT_LINE]);
        let f = filter_session(Some("codex"), &raw, &opts(Turns::User, false), None);
        assert_eq!(f.turns, TurnsOutcome::NotSupported);
        assert_eq!(
            f.lines_written, 2,
            "nothing may be dropped for this harness"
        );
        assert_eq!(f.bytes, raw);
    }

    /// An id with no harness prefix cannot answer it either.
    #[test]
    fn a_session_without_a_harness_prefix_is_also_not_supported() {
        let raw = joined(&[USER_LINE]);
        let f = filter_session(None, &raw, &opts(Turns::User, false), None);
        assert_eq!(f.turns, TurnsOutcome::NotSupported);
        assert_eq!(f.lines_written, 1);
    }

    // ------------------------------------------------------------- trimming

    /// Lines outside the window go, lines inside stay, and a line whose time
    /// cannot be read is kept *and counted* — never dropped as if it were out.
    #[test]
    fn trim_keeps_untimed_lines_and_counts_them() {
        let inside = r#"{"type":"user","message":{"role":"user","content":"in"},"timestamp":"2026-01-15T12:00:00Z"}"#;
        let outside = r#"{"type":"user","message":{"role":"user","content":"out"},"timestamp":"2026-01-20T12:00:00Z"}"#;
        let timeless = r#"{"type":"user","message":{"role":"user","content":"when?"}}"#;
        let raw = joined(&[inside, outside, timeless]);
        // Covers `inside` (2026-01-15T12:00:00Z, +2 h from the fixture head)
        // and nothing else: `outside` is five days later.
        let w = window(SESSION_DAY_START + 3_600, SESSION_DAY_START + 3 * 3_600);
        let f = filter_session(Some("claude-code"), &raw, &opts(Turns::All, true), Some(&w));
        assert!(f.trimmed);
        assert_eq!(f.lines_total, 3);
        assert_eq!(
            f.lines_written, 2,
            "the out-of-window line is the only drop"
        );
        assert_eq!(f.untimed_lines, 1, "the timeless line is kept AND counted");
        let text = String::from_utf8(f.bytes).unwrap();
        assert!(text.contains(r#""content":"in""#));
        assert!(text.contains(r#""content":"when?""#));
        assert!(!text.contains(r#""content":"out""#));
    }

    /// A harness the time extractor does not know yields no readable time for
    /// any line, so a trim keeps everything and says how much it kept blindly.
    #[test]
    fn trim_on_an_unknown_harness_keeps_everything_and_says_so() {
        let raw = joined(&[USER_LINE, ASSISTANT_LINE]);
        let w = window(0, 1);
        let f = filter_session(Some("cursor"), &raw, &opts(Turns::All, true), Some(&w));
        assert_eq!(f.lines_written, 2);
        assert_eq!(f.untimed_lines, 2);
    }

    /// The turns filter runs before the trim: `untimed_lines` counts lines that
    /// are actually in the file, not lines that were already dropped.
    #[test]
    fn untimed_lines_counts_only_lines_that_were_written() {
        let timeless_user = r#"{"type":"user","message":{"role":"user","content":"when?"}}"#;
        let no_time_assistant =
            r#"{"type":"assistant","message":{"role":"assistant","content":"hi"}}"#;
        let raw = joined(&[USER_LINE, timeless_user, no_time_assistant]);
        // Covers USER_LINE's own timestamp (2026-01-15T10:00:00Z), so that line
        // is timed and kept; the timeless user line is kept and counted.
        let w = window(SESSION_DAY_START - 3_600, SESSION_DAY_START + 3_600);
        let f = filter_session(
            Some("claude-code"),
            &raw,
            &opts(Turns::User, true),
            Some(&w),
        );
        assert_eq!(
            f.lines_written, 2,
            "two user lines, the assistant line is out"
        );
        assert_eq!(
            f.untimed_lines, 1,
            "only the timeless user line is counted: the assistant line was dropped by --turns, \
             so it is not an untimed written line"
        );
    }

    // ------------------------------------------------------------- path safety

    #[test]
    fn unsafe_path_components_are_refused() {
        assert!(is_safe_component("claude-code"));
        assert!(is_safe_component("019bf00d-97b6-7eb2"));
        assert!(!is_safe_component(""));
        assert!(!is_safe_component("."));
        assert!(!is_safe_component(".."));
        assert!(!is_safe_component("a/b"));
        assert!(!is_safe_component("a\\b"));
        assert!(!is_safe_component("../escape"));
    }

    /// A symlink anywhere below `--out` is refused — the directory on the way
    /// down, the file itself, and a target that is not a regular file — while
    /// `--out` itself may be a symlink, because naming it *is* the choice of
    /// where to write.
    #[cfg(unix)]
    #[test]
    fn a_symlink_below_out_is_refused_but_out_itself_may_be_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let out = root.join("out");
        std::fs::create_dir_all(out.join("m-alpha/claude-code")).unwrap();

        // A path that does not exist yet, and one that holds a regular file
        // from an earlier run: both are ordinary.
        let file = out.join("m-alpha/claude-code/s.jsonl");
        assert_eq!(write_refusal(&out, &file), None, "an absent path is fine");
        std::fs::write(&file, b"previous run").unwrap();
        assert_eq!(
            write_refusal(&out, &file),
            None,
            "overwriting a regular file is what --force is for"
        );

        // A symlinked directory on the way down.
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::rename(out.join("m-alpha"), root.join("real-alpha")).unwrap();
        std::os::unix::fs::symlink(&outside, out.join("m-alpha")).unwrap();
        let why = write_refusal(&out, &out.join("m-alpha/claude-code/s.jsonl"))
            .expect("a symlinked directory must be refused");
        assert!(why.contains("symlink"), "{why}");

        // The target file itself, a link to a file outside `out`.
        std::fs::write(root.join("victim.jsonl"), b"do not touch").unwrap();
        std::fs::create_dir_all(out.join("m-beta/codex")).unwrap();
        let link = out.join("m-beta/codex/linked.jsonl");
        std::os::unix::fs::symlink(root.join("victim.jsonl"), &link).unwrap();
        let why = write_refusal(&out, &link).expect("a symlinked file must be refused");
        assert!(why.contains("symlink"), "{why}");

        // A target that is not a file at all.
        std::fs::create_dir_all(out.join("m-beta/codex/a-directory")).unwrap();
        let why = write_refusal(&out, &out.join("m-beta/codex/a-directory"))
            .expect("a directory is not a file to overwrite");
        assert!(why.contains("not a regular file"), "{why}");

        // `--out` itself as a symlink: the user named it, so it is honoured.
        let linked_out = root.join("linked-out");
        std::os::unix::fs::symlink(&out, &linked_out).unwrap();
        assert_eq!(
            write_refusal(&linked_out, &linked_out.join("m-beta/codex/s.jsonl")),
            None,
            "the destination the user named may itself be a symlink"
        );
    }

    // ------------------------------------------------------------ the refusal

    #[test]
    fn a_non_empty_out_is_refused_without_force() {
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("existing.txt"), b"keep me").unwrap();

        let err = check_out(&out, false).expect_err("a non-empty --out must be refused");
        assert!(
            err.0.contains("--force"),
            "the refusal must name the flag that overrides it: {}",
            err.0
        );
        assert!(out.join("existing.txt").exists(), "nothing may be deleted");

        check_out(&out, true).expect("--force must allow it");
        assert!(
            out.join("existing.txt").exists(),
            "--force overwrites what it writes, it never deletes"
        );
        assert_eq!(
            std::fs::read_dir(&out).unwrap().count(),
            1,
            "check_out only checks: it creates and removes nothing"
        );
    }

    /// An absent `--out` is accepted but **not** created here — the directory
    /// appears only after the archive has been read, so a run that fails to read
    /// cannot leave an empty directory behind.
    #[test]
    fn an_empty_or_absent_out_is_accepted_and_not_created_by_the_check() {
        let dir = tempfile::TempDir::new().unwrap();
        let absent = dir.path().join("brand-new");
        check_out(&absent, false).expect("an absent --out is accepted");
        assert!(!absent.exists(), "the check creates nothing");
        check_out(&absent, false).expect("and it is still accepted the second time");

        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        check_out(&empty, false).expect("an empty --out is fine");

        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let err = check_out(&file, true).expect_err("a file is not a directory");
        assert!(err.0.contains("not a directory"), "{}", err.0);
    }

    // ------------------------------------------------------------ exit contract

    fn report_with(
        selected: usize,
        written: usize,
        not_placed: usize,
        unreadable: usize,
    ) -> ExportReport {
        ExportReport {
            destination: "d".into(),
            out: "/tmp/out".into(),
            selector: Selector::default(),
            window: None,
            turns: Turns::All,
            trim_to_window: false,
            dry_run: false,
            plan: ExportPlan {
                sessions: selected,
                shards: 0,
                data_blobs: 0,
                plaintext_bytes: 0,
            },
            selected,
            not_matched: 0,
            sessions: (0..written)
                .map(|i| ExportedSession {
                    machine: "m".into(),
                    harness: Some("claude-code".into()),
                    session_id: format!("claude-code.m.aaaaaaaa-0000-0000-0000-00000000000{i}"),
                    relative_path: "m/claude-code/x.jsonl".into(),
                    snapshot_id: "abcdef0123456789".into(),
                    first_message: TimeState::known(1),
                    last_message: TimeState::known(2),
                    shard_count: 1,
                    lines_total: 1,
                    lines_written: 1,
                    untimed_lines: 0,
                    bytes_written: 3,
                    sha256: "0".repeat(64),
                    turns_filter: TurnsOutcome::NotRequested,
                    trimmed_to_window: false,
                })
                .collect(),
            failed: Vec::new(),
            not_placed: (0..not_placed)
                .map(|i| NotPlacedSession {
                    machine: "m".into(),
                    session_id: format!("codex.m.bbbbbbbb-0000-0000-0000-00000000000{i}"),
                    harness: Some("codex".into()),
                    dimension: "time",
                    why: "no activity index".into(),
                })
                .collect(),
            machines_without_index: Vec::new(),
            unreadable: (0..unreadable).map(|i| format!("part {i}")).collect(),
            bytes_written: 3 * written as u64,
            manifest_path: Some("/tmp/out/manifest.json".into()),
        }
    }

    /// The four answers, each reachable: wrote something (0), read it all and
    /// selected nothing (1), and two distinct kinds of "not everything" (3).
    #[test]
    fn exit_status_separates_written_from_nothing_from_unfinished() {
        assert_eq!(report_with(2, 2, 0, 0).exit_status(), 0);
        assert_eq!(report_with(0, 0, 0, 0).exit_status(), 1);
        assert_eq!(
            report_with(1, 1, 1, 0).exit_status(),
            3,
            "a session the query asked about could not be placed: the written set is not the answer"
        );
        assert_eq!(
            report_with(1, 1, 0, 1).exit_status(),
            3,
            "part of the destination was unreadable: there may be more"
        );
    }

    /// A dry run writes nothing by design, so an empty written list is not a
    /// partial write and must not turn "would have written 3" into a 3.
    #[test]
    fn a_dry_run_reports_what_a_real_run_would() {
        let mut report = report_with(3, 0, 0, 0);
        report.dry_run = true;
        assert_eq!(report.exit_status(), 0);
        let v: serde_json::Value = serde_json::from_str(&report.manifest_json()).unwrap();
        assert_eq!(v["exit_status"], 0);
        assert_eq!(v["selected"], 3, "the dry run still reports the selection");
        // …and the same selection, written for real with one write missing, is
        // still a 3: the exemption is the dry run's, not the empty list's.
        let mut real = report_with(3, 0, 0, 0);
        real.dry_run = false;
        assert_eq!(real.exit_status(), 3);
    }

    /// A session that was selected and then failed to write is a `3`, not a
    /// silent `0` — its file is absent and only the manifest says why.
    #[test]
    fn a_failed_write_makes_the_run_incomplete() {
        let mut report = report_with(2, 1, 0, 0);
        report.failed.push(FailedSession {
            machine: "m".into(),
            session_id: "claude-code.m.aaaaaaaa-0000-0000-0000-000000000009".into(),
            harness: Some("claude-code".into()),
            why: "could not write".into(),
        });
        assert!(!report.answer_complete());
        assert_eq!(report.exit_status(), 3);
        let v: serde_json::Value = serde_json::from_str(&report.manifest_json()).unwrap();
        assert_eq!(v["exit_status"], 3);
        assert_eq!(v["sessions_failed"].as_array().unwrap().len(), 1);
    }

    /// The manifest's own record of the run has to agree with the exit code the
    /// process returns, or the directory lies about itself after the fact.
    #[test]
    fn the_manifest_records_the_exit_status_it_will_return() {
        for (selected, written, placed, unreadable) in [
            (2usize, 2usize, 0usize, 0usize),
            (0, 0, 0, 0),
            (1, 1, 1, 0),
            (1, 1, 0, 1),
        ] {
            let report = report_with(selected, written, placed, unreadable);
            let v: serde_json::Value = serde_json::from_str(&report.manifest_json()).unwrap();
            assert_eq!(
                v["exit_status"].as_u64().unwrap() as u8,
                report.exit_status()
            );
            assert_eq!(v["written"].as_u64().unwrap() as usize, written);
            assert_eq!(v["selected"].as_u64().unwrap() as usize, selected);
            assert_eq!(v["time_unknown"].as_u64().unwrap() as usize, placed);
        }
    }

    /// The manifest carries the flags that were applied, per session, so a
    /// reader can tell an exported user-turns file from a whole-session one
    /// without guessing from the file's contents.
    #[test]
    fn the_manifest_records_the_turns_outcome_per_session() {
        let mut report = report_with(1, 1, 0, 0);
        report.turns = Turns::User;
        report.trim_to_window = true;
        report.sessions[0].turns_filter = TurnsOutcome::NotSupported;
        report.sessions[0].trimmed_to_window = true;
        report.sessions[0].untimed_lines = 4;
        let v: serde_json::Value = serde_json::from_str(&report.manifest_json()).unwrap();
        assert_eq!(v["flags"]["turns"], "user");
        assert_eq!(v["flags"]["trim_to_window"], true);
        assert_eq!(v["sessions"][0]["turns_filter"], "not-supported");
        assert_eq!(v["sessions"][0]["trimmed_to_window"], true);
        assert_eq!(v["sessions"][0]["untimed_lines"], 4);
    }

    /// An unknown conversation time is a tagged unknown with its reason in the
    /// manifest — never a null a reader would take for zero.
    #[test]
    fn an_unknown_conversation_time_is_tagged_in_the_manifest() {
        let mut report = report_with(1, 1, 0, 0);
        report.sessions[0].first_message = TimeState::unknown("no activity index".to_string());
        report.sessions[0].last_message = TimeState::unknown("no activity index".to_string());
        let v: serde_json::Value = serde_json::from_str(&report.manifest_json()).unwrap();
        assert_eq!(v["sessions"][0]["first_message"]["kind"], "unknown");
        assert_eq!(
            v["sessions"][0]["first_message"]["why"],
            "no activity index"
        );
        assert!(v["sessions"][0]["first_message"]["unix"].is_null());
    }
}
