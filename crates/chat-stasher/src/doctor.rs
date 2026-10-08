//! `doctor` — answers the one question the product exists for:
//!
//! > "Is my CLI harness silently deleting my conversation history?"
//!
//! Each harness has a different retention story, and most of them delete by
//! default with (at best) a small warning. This module probes the *real*
//! machine state and reports what is actually about to be cleaned up, and
//! when. It is strictly read-only: it never writes to a harness directory,
//! never deletes anything, and never prints a session's contents — only
//! paths, counts, byte sizes and timestamps.
//!
//! Checks implemented here:
//!   * D1 — Claude Code: read `cleanupPeriodDays` (default 30, and the
//!     settings file failing to parse silently *reverts to 30* — the
//!     fail-destructive fallback).
//!   * D2 — Gemini CLI: read `sessionRetention` (default 30 days, disabled
//!     only if `enabled: false` is set explicitly).
//!   * D3 — Coverage: how many harnesses exist on this machine, and whether
//!     each is accounted for here.
//!   * D4 — A single human sentence synthesising "so what, and when".
//!   * D5 — How much reclaimable garbage sits in the archive repository — via
//!     `prune_plan`, which is **read-only**: it computes the plan without
//!     touching a single pack, and even works inside an append-only repo.
//!     It also says why the garbage cannot be cleaned right now (append-only)
//!     and what the safe cleanup sequence is — but never runs it.

use crate::config::Config;
use crate::json_out::{CountState, TimeState};
use crate::scanner;
use crate::sqlite_probe;
use crate::store::{self, StoreConfig};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Day threshold below which an explicit `cleanupPeriodDays` is still treated
/// as a live deletion threat. Claude Code's default is 30; we call anything
/// under half a year "not safe" so a 30-or-90-day setting never slides.
const CLEANUP_SAFE_DAYS: u64 = 180;

/// Extra harness-like directories we notice but do *not* reason about for
/// retention (they land in a "detected, out of scope" note, never silently).
///
/// `.kimi-code` used to be listed here; it was removed when Kimi Code became a
/// registry harness, because the note this feeds says "installed but out of
/// scope for this command" and that stopped being true — the scanner now
/// walks it and it has its own footprint row. Leaving it would have reported
/// the same directory twice, once as out of scope and once as scanned.
pub const OTHER_HARNESS_DIRS: &[&str] = &[".cursor", ".windsurf"];

// ---------------------------------------------------------------------------
// D1 — Claude Code `cleanupPeriodDays`
// ---------------------------------------------------------------------------

/// Outcome of inspecting one Claude Code settings layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeRetention {
    /// No settings file exists and no `cleanupPeriodDays` anywhere:
    /// Claude Code defaults to 30 days. The harness still deletes.
    UnsetDefault,
    /// A settings file parsed cleanly and `cleanupPeriodDays` is present and
    /// >= [`CLEANUP_SAFE_DAYS`]. This is the "safe" outcome — *while the file
    /// keeps parsing*. It can still revert to default 30 on a future parse
    /// failure (see [`ClaudeRetention::ParseFailed`]).
    Safe { days: u64, source: PathBuf },
    /// `cleanupPeriodDays` is set explicitly, but below the safe threshold.
    /// Effectively as dangerous as the default.
    SmallValue { days: u64, source: PathBuf },
    /// A settings file **exists but could not be parsed**. This is the exact
    /// trigger for Claude Code's fail-destructive fallback: on a JSON syntax
    /// error it silently reverts to the 30-day default and starts cleaning,
    /// no matter what was configured.
    ParseFailed { path: PathBuf, error: String },
}

impl ClaudeRetention {
    /// Short single-word label used in the summary line.
    pub fn label(&self) -> String {
        match self {
            ClaudeRetention::UnsetDefault => "unset (default 30d)".to_string(),
            ClaudeRetention::Safe { days, .. } => format!("large ({days}d)"),
            ClaudeRetention::SmallValue { days, .. } => format!("small ({days}d)"),
            ClaudeRetention::ParseFailed { .. } => "PARSE FAILED".to_string(),
        }
    }

    /// True when this state hands Claude Code a live deletion window.
    pub fn is_dangerous(&self) -> bool {
        matches!(
            self,
            ClaudeRetention::UnsetDefault
                | ClaudeRetention::SmallValue { .. }
                | ClaudeRetention::ParseFailed { .. }
        )
    }
}

/// Full result of the Claude Code retention check. One `Check` per layer lets
/// D1 report *where* a value came from and which layer choked.
#[derive(Debug, Clone)]
pub struct ClaudeCheck {
    /// One entry per settings layer: path it read, and the verdict it gives.
    pub layers: Vec<(PathBuf, ClaudeRetention)>,
    /// The merged verdict (worst of the layers: a parse failure anywhere
    /// means the harness's own merge reverts everything to 30 days).
    pub verdict: ClaudeRetention,
}

/// Layers Claude Code consults for user-level settings, in the order we try
/// them. `settings.json` and `settings.local.json` are the real merge inputs;
/// `~/.claude.json` (the app's global JSON) is probed too because it can carry
/// a `cleanupPeriodDays` override.
pub fn claude_settings_layers(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".claude").join("settings.json"),
        home.join(".claude").join("settings.local.json"),
        home.join(".claude.json"),
    ]
}

/// Classify a single settings layer from its raw file content.
fn classify_layer(path: &Path, raw: &str) -> ClaudeRetention {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Err(e) => ClaudeRetention::ParseFailed {
            path: path.to_path_buf(),
            error: format!("{e}"),
        },
        Ok(v) => match v
            .get("cleanupPeriodDays")
            .and_then(serde_json::Value::as_u64)
        {
            Some(days) if days >= CLEANUP_SAFE_DAYS => ClaudeRetention::Safe {
                days,
                source: path.to_path_buf(),
            },
            Some(days) => ClaudeRetention::SmallValue {
                days,
                source: path.to_path_buf(),
            },
            // Present but the key is not at top level (or not a number): the
            // harness reads its default 30 days for this layer.
            None => ClaudeRetention::UnsetDefault,
        },
    }
}

/// D1 — inspect every Claude Code settings layer and classify.
///
/// Defends against: a `.claude/settings.json` that *fails to parse* being
/// silently treated as "no config" (→ 30-day default cleanup), which is the
/// fail-destructive path people hit in the wild.
pub fn inspect_claude_settings(home: &Path) -> ClaudeCheck {
    let layers = claude_settings_layers(home);
    let mut results: Vec<(PathBuf, ClaudeRetention)> = Vec::new();

    for path in &layers {
        match fs::read_to_string(path) {
            Ok(raw) => results.push((path.clone(), classify_layer(path, &raw))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Layer absent — Claude Code ignores it; so do we (missing is
                // not "unset verdict", it's simply nothing to read).
            }
            Err(e) => {
                // Unreadable for a non "not found" reason: the loader will
                // fail exactly like a parse error → fail-destructive applies.
                results.push((
                    path.clone(),
                    ClaudeRetention::ParseFailed {
                        path: path.clone(),
                        error: format!("{e}"),
                    },
                ));
            }
        }
    }

    // Worst-of-merge: a parse failure in *any* layer triggers the harness's
    // fallback. Report that first.
    let parse_fail = results.iter().find_map(|(_, v)| match v {
        ClaudeRetention::ParseFailed { .. } => Some(v.clone()),
        _ => None,
    });
    let verdict = if let Some(pf) = parse_fail {
        pf
    } else {
        // Otherwise: any Safe/SmallValue wins (first, in layer order);
        // if nothing ever set the key → default 30.
        results
            .iter()
            .find_map(|(_, v)| match v {
                ClaudeRetention::Safe { .. } | ClaudeRetention::SmallValue { .. } => {
                    Some(v.clone())
                }
                _ => None,
            })
            .unwrap_or(ClaudeRetention::UnsetDefault)
    };

    ClaudeCheck {
        layers: results,
        verdict,
    }
}

// ---------------------------------------------------------------------------
// D2 — Gemini CLI `sessionRetention`
// ---------------------------------------------------------------------------

/// A parsed `sessionRetention` block, with the CLI's documented defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct GeminiRetention {
    pub enabled: bool,
    /// Raw `maxAge` string as written (e.g. `30d`).
    pub max_age: String,
    pub min_retention: String,
    /// A present settings file that could not be read is not the CLI default.
    /// Keep that uncertainty beside the parsed fields so risk synthesis cannot
    /// mistake a fallback value for a value the user actually configured.
    pub unreadable: Option<String>,
}

impl Default for GeminiRetention {
    fn default() -> Self {
        // Gemini docs: enabled=true, maxAge="30d", minRetention="1d".
        GeminiRetention {
            enabled: true,
            max_age: "30d".to_string(),
            min_retention: "1d".to_string(),
            unreadable: None,
        }
    }
}

/// Best-effort parse of a duration like `30d`, `12h`, `45m`, or a bare
/// number of days. `None` means "shape unrecognised, cannot reason about it".
pub fn parse_duration_days(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('d') {
        num.trim().parse::<f64>().ok()
    } else if let Some(num) = s.strip_suffix('h') {
        num.trim().parse::<f64>().ok().map(|v| v / 24.0)
    } else if let Some(num) = s.strip_suffix('m') {
        num.trim().parse::<f64>().ok().map(|v| v / (24.0 * 60.0))
    } else {
        s.parse::<f64>().ok()
    }
}

/// D2 — read Gemini CLI's retention policy.
///
/// Defends against: "the session folder is literally named `tmp`" — Gemini
/// keeps every session under `~/.gemini/tmp/` and will clean it on the 30-day
/// default unless `sessionRetention` was explicitly configured.
pub fn inspect_gemini_settings(home: &Path) -> GeminiRetention {
    let mut max_age = None;
    let mut enabled = None;
    let mut min_retention = None;
    let mut unreadable = None;

    for path in [
        home.join(".gemini").join("settings.json"),
        home.join(".gemini").join("config.json"),
    ] {
        let raw = match fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                // A file that exists but cannot be read is evidence about
                // neither the user's policy nor the CLI default.
                unreadable.get_or_insert_with(|| format!("{}: {e}", path.display()));
                continue;
            }
        };
        let v = match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => v,
            Err(e) => {
                // Do not continue with the default after a malformed policy;
                // that would turn an unknown retention window into a risk claim.
                unreadable.get_or_insert_with(|| format!("{}: {e}", path.display()));
                continue;
            }
        };
        let Some(sr) = v.get("sessionRetention") else {
            continue;
        };
        enabled = sr
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .or(enabled);
        max_age = sr
            .get("maxAge")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or(max_age);
        min_retention = sr
            .get("minRetention")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or(min_retention);
    }

    GeminiRetention {
        enabled: enabled.unwrap_or_else(|| GeminiRetention::default().enabled),
        max_age: max_age.unwrap_or_else(|| GeminiRetention::default().max_age),
        min_retention: min_retention.unwrap_or_else(|| GeminiRetention::default().min_retention),
        unreadable,
    }
}

impl GeminiRetention {
    /// True when the harness is actively cleaning up on a 30-ish day window.
    pub fn is_dangerous(&self) -> bool {
        self.unreadable.is_none()
            && self.enabled
            && parse_duration_days(&self.max_age).map_or(true, |d| d <= 30.0)
    }

    pub fn is_unknown(&self) -> bool {
        self.unreadable.is_some()
    }

    /// Human summary of the policy.
    pub fn summarize(&self) -> String {
        if let Some(error) = &self.unreadable {
            return format!("unknown (config unreadable: {error})");
        }
        if !self.enabled {
            "disabled (enabled=false) — safe".to_string()
        } else {
            // B95: the dangerous arm used to end in
            // `parse_duration_days(&self.max_age).unwrap_or(30.0)`. That default
            // was only ever reached when the parse had *already* failed, so the
            // one case it served was the one case it lied about: a `maxAge` this
            // build cannot read was printed as `(30.0 days)` — a duration nothing
            // parsed — right next to the user's own literal string. Same shape as
            // the `now_unix() -> 0` and `days_since -> 0` defaults already removed.
            // `is_dangerous()` still treats an unparseable window as dangerous;
            // only the fabricated number is gone.
            match parse_duration_days(&self.max_age) {
                Some(d) if d > 30.0 => format!(
                    "enabled, maxAge={} (~{d:.0}d) — large, safe-ish",
                    self.max_age
                ),
                Some(d) => format!(
                    "enabled, maxAge={} ({d:.1} days) — DEFAULT-ish, dangerous",
                    self.max_age
                ),
                None => format!(
                    "enabled, maxAge={} (this build cannot parse the duration, treating it as dangerous) — dangerous",
                    self.max_age
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// D3 — coverage: which harnesses exist, and all of them accounted for?
// ---------------------------------------------------------------------------

/// Which clock a footprint's `earliest` / `latest` were read from.
///
/// W285 §5: doctor reported one of two different quantities under one
/// unlabelled name, and no reader could tell which. A directory harness is
/// probed from file metadata, so its footprint carries the **file mtime**; a
/// single-file SQLite store carries a per-session time column of its own
/// (the registry's `sql_time_json_path`), which is the **conversation's** time.
/// `search`, `export` and the UI report conversation time for both kinds, so
/// the two surfaces disagreed about what "a session's date" means.
///
/// Neither clock is wrong, and which one is *relevant* depends on the question:
/// a harness that rotates by file age deletes on mtime, so mtime is the clock
/// that predicts deletion; conversation time is the clock that describes the
/// conversation. The fix is to say which one a row shows rather than to pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeBasis {
    /// File metadata modification time (directory harnesses).
    FileMtime,
    /// The store's own recorded per-session time (single-file SQLite stores).
    SessionTime,
    /// Nothing was measured — `earliest` / `latest` are both `None`.
    Unmeasured,
}

impl TimeBasis {
    /// The word this clock is printed under. `Unmeasured` has no value to
    /// qualify, so it labels nothing.
    fn label(self) -> Option<&'static str> {
        match self {
            TimeBasis::FileMtime => Some("mtime"),
            TimeBasis::SessionTime => Some("session time"),
            TimeBasis::Unmeasured => None,
        }
    }
}

/// One harness's session footprint: count, bytes, and — the column with the
/// most signal — **earliest session timestamp**, because it answers
/// "how old is the oldest thing I've kept?" It directly bounds how much
/// history survives any retention policy.
#[derive(Debug, Clone)]
pub struct HarnessFootprint {
    pub name: String,
    /// `None` means the registry never resolved a path; it is not an empty
    /// path and must not render as `not installed ()`.
    pub root: Option<PathBuf>,
    /// False = not installed at all (NOT "0 sessions" — different meaning).
    pub installed: bool,
    /// `None` when the harness stores sessions in something non-enumerable
    /// by a file walk (e.g. opencode's single SQLite).
    pub session_count: Option<u64>,
    /// Raw candidate rows before any store-specific qualification rule.
    /// Cursor's doctor line prints this beside `session_count`.
    pub candidate_count: Option<u64>,
    /// Sessions this harness knows about but could not hand over. Printed
    /// only when non-zero, so an all-good run's line is unchanged.
    ///
    /// B90: `None` means the tally was not taken — either nothing was
    /// enumerated at all (`session_count` is `None` too) or enumeration
    /// worked and only this count failed (`session_count` is `Some`). The
    /// second one gets printed as unknown; see [`footprint_count_detail`].
    pub unreadable_count: Option<u64>,
    /// Directory entries/subtrees that could not be inspected. The number of
    /// sessions behind them is unknown, so this is not folded into
    /// `unreadable_count`.
    pub unreadable_entry_count: Option<u64>,
    /// `None` means the footprint was not measured; it is not an empty store.
    pub total_bytes: Option<u64>,
    /// The clock `earliest` / `latest` were read from. Printed with them, so a
    /// row can never be read as the other clock — see [`TimeBasis`].
    pub time_basis: TimeBasis,
    pub earliest: Option<SystemTime>,
    pub latest: Option<SystemTime>,
    pub compressed_count: u64,
    /// Metadata-only set of files recognised for this harness. This is kept
    /// private from CLI output and compared with the scanner's set in tests.
    pub recognized_files: Vec<PathBuf>,
    pub note: String,
}

/// Aggregate per-harness details.
pub fn coverage_from_records<'a>(
    name: &str,
    root: PathBuf,
    recs: impl Iterator<Item = &'a crate::models::SessionRecord>,
) -> HarnessFootprint {
    let recs: Vec<_> = recs.collect();
    // Records are stronger evidence than a second racy directory stat: once
    // this pass produced records, a concurrent removal cannot erase them.
    let installed = root.is_dir() || !recs.is_empty();
    let total_bytes = Some(recs.iter().map(|r| r.byte_size).sum());
    // A `SessionRecord` carries file metadata only (`byte_size` + `mtime`), so
    // this row's times are file mtimes — named as such on the D3 line, because
    // the same session's *conversation* time (what `search` / the UI show) is a
    // different number and this table never read it.
    let time_basis = TimeBasis::FileMtime;
    let earliest = recs.iter().map(|r| r.mtime).min();
    let latest = recs.iter().map(|r| r.mtime).max();
    let compressed_count = recs.iter().filter(|r| r.compressed).count() as u64;
    HarnessFootprint {
        name: name.to_string(),
        root: Some(root),
        installed,
        session_count: if installed {
            Some(recs.len() as u64)
        } else {
            None
        },
        candidate_count: None,
        unreadable_count: None,
        unreadable_entry_count: None,
        total_bytes,
        time_basis,
        earliest,
        latest,
        compressed_count,
        recognized_files: recs
            .iter()
            .map(|record| record.absolute_path.clone())
            .collect(),
        note: String::new(),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// `YYYY-MM-DD` of a timestamp, via a civil-from-days conversion.
fn format_date(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let days = secs.div_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let (yy, mm) = if m <= 2 { (y + 1, m) } else { (y, m) };
    format!("{yy:04}-{mm:02}-{d:02}")
}

fn format_timestamp(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let day_seconds = secs.rem_euclid(86400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        format_date(t),
        day_seconds / 3600,
        (day_seconds % 3600) / 60,
        day_seconds % 60
    )
}

/// Days between `t` and now, or `None` when `t` is in the future.
///
/// B94: this used to answer `0.0` for a future timestamp, which is reachable —
/// a file whose mtime sits ahead of the clock (skew, a restored archive, a
/// `touch -t`) is enough. The `0.0` then reached a sentence that reads
/// "your earliest session is 2026-08-19 (about 0 days ago, today 2026-08-18)
/// — your history has about 0 days left": a false alarm that contradicts
/// itself inside one line. `None` says the one true thing instead.
fn days_since(t: SystemTime) -> Option<f64> {
    SystemTime::now()
        .duration_since(t)
        .ok()
        .map(|d| d.as_secs_f64() / 86400.0)
}

fn fmt_bytes(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.1} GiB", b as f64 / (1 << 30) as f64)
    } else if b >= 1 << 20 {
        format!("{:.1} MiB", b as f64 / (1 << 20) as f64)
    } else if b >= 1 << 10 {
        format!("{:.1} KiB", b as f64 / (1 << 10) as f64)
    } else {
        format!("{b} B")
    }
}

/// Byte measurements use the same three-state vocabulary as session counts:
/// a number when measured, `unknown` when this installed store could not be
/// measured, and `N/A` when the harness does not apply on this machine.
fn footprint_bytes_label(f: &HarnessFootprint) -> String {
    match f.total_bytes {
        Some(bytes) => format!("{} ({} B)", fmt_bytes(bytes), bytes),
        None if f.installed => "unknown".to_string(),
        None => "N/A".to_string(),
    }
}

fn footprint_root_label(f: &HarnessFootprint) -> String {
    f.root
        .as_ref()
        .map(|root| root.display().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// The three numbers the D3 coverage summary is made of.
///
/// W285 §6: `hit/known` used to be the whole summary, with `known` being every
/// harness the registry lists for this platform. On a machine where some of
/// them were `skip(template)` / `skip(uncertain)` — never opened — that read as
/// a measurement over the whole registry, when only `probed` of them were
/// looked at. `probed` and `never_probed` are reported separately so the
/// denominator can never absorb a harness nobody opened. `hit` keeps its own
/// meaning: the harness is here. It is `0` on a machine with nothing installed,
/// which says "looked, found nothing" — not "did not look".
struct Coverage {
    hit: usize,
    probed: usize,
    known: usize,
}

impl Coverage {
    /// Harnesses that were never looked at at all — the number the old header
    /// silently added to the denominator. See
    /// [`scanner::HarnessProbe::not_probed_p`] for which states those are, and
    /// `status` for the same figure.
    fn never_probed(&self) -> usize {
        self.known - self.probed
    }
}

fn probe_coverage(probes: &[scanner::HarnessProbe]) -> Coverage {
    Coverage {
        hit: probes.iter().filter(|p| p.installed_p()).count(),
        probed: probes.iter().filter(|p| p.probed_p()).count(),
        known: probes.len(),
    }
}

/// The D3 timestamp columns, each qualified by the clock it was read from.
///
/// W285 §5: `earliest 2026-09-30T13:07:42Z` did not say whether that was the
/// session file's mtime or the conversation's own time — and the risk line
/// below then called it "your earliest session" with no qualification at all,
/// so a session restored from a backup (fresh mtime, old conversation) was
/// described as "about 0 days ago" and, under a 30-day policy, also as having
/// "only about 0 days left". `status --sessions` had always labelled its column
/// `mtime(sec)`; doctor now names the clock the same way. Naming it is the
/// honest fix rather than switching it: a harness that rotates by file age
/// deletes on mtime, so mtime is the clock that predicts deletion, while
/// conversation time is the clock that describes the conversation.
fn footprint_times(f: &HarnessFootprint) -> String {
    let stamp = |t: Option<SystemTime>| t.map(format_timestamp).unwrap_or_else(|| "-".to_string());
    match f.time_basis.label() {
        Some(clock) => format!(
            "earliest({clock}) {} · latest({clock}) {}",
            stamp(f.earliest),
            stamp(f.latest)
        ),
        None => format!(
            "earliest {} · latest {}",
            stamp(f.earliest),
            stamp(f.latest)
        ),
    }
}

/// How far the oldest session sits from a rotation threshold, said without a
/// sign flip: a session younger than the window is *not* "minus N days past" it.
///
/// W285 §5: the Gemini line read "about 0 days ago … already about -30 days
/// past the 30-day threshold" on a freshly written file. The Claude line had
/// the mirror-image of the same defect — "about 0 days ago … only about 0 days
/// left" — because it printed the session's age as the time remaining.
fn threshold_phrase(days_old: f64, threshold: f64) -> String {
    let over = days_old - threshold;
    if over >= 0.0 {
        format!("already about {over:.0} days past the {threshold:.0}-day threshold")
    } else {
        format!(
            "about {:.0} days from the {threshold:.0}-day threshold",
            -over
        )
    }
}

// ---------------------------------------------------------------------------
// D4 — risk synthesis (the whole value of this command)
// ---------------------------------------------------------------------------

/// Build a footprint row straight from a registry probe (single-file SQLite
/// harnesses: cursor, grok). Because the row *is* the probe, the footprint
/// table and the registry table share count, bytes and timestamps by
/// construction — `doctor_tables_never_contradict_any_harness` can never see
/// them drift apart.
fn footprint_from_sqlite_probe(probe: &scanner::HarnessProbe) -> HarnessFootprint {
    HarnessFootprint {
        name: probe.id.clone(),
        root: probe.root.clone(),
        installed: probe.installed_p(),
        session_count: probe.record_count,
        candidate_count: probe.candidate_count,
        unreadable_count: probe.unreadable_count,
        unreadable_entry_count: probe.unreadable_entry_count,
        total_bytes: probe.bytes,
        // The probe read the store's own time column, not the .db file's mtime,
        // so this row is on the conversation clock.
        time_basis: TimeBasis::SessionTime,
        earliest: probe.earliest,
        latest: probe.latest,
        compressed_count: 0,
        recognized_files: probe.recognized_files.clone(),
        note: probe.note.clone(),
    }
}

/// Build a footprint row for a **directory** harness from its registry probe.
///
/// The probe is the only thing that actually touched the disk, so it decides
/// both `installed` and whether a count may be claimed at all. A probe that
/// never resolved a root (`unascertained` cell, template not statically resolvable, no
/// cell for this platform) or that found no root gets `session_count: None` —
/// "unknown" — even when a directory happens to sit at the path this build
/// would otherwise have guessed. Reporting `Some(0)` there would assert
/// "I enumerated it and it is empty" about a directory nothing ever opened;
/// that is the same lie `push` refuses when it will not archive an unprovable
/// empty snapshot (`main.rs`, "refusing empty snapshot"), and the same
/// distinction `destinit::SourceStatus` draws between `KnownEmpty` and
/// `Unknown`.
///
/// Consequence for `doctor_tables_never_contradict_any_harness`: every row of
/// the footprint table now derives its `installed`/`session_count` from the
/// same probe the registry table prints, for directory harnesses exactly as
/// [`footprint_from_sqlite_probe`] already did for the single-file ones.
fn footprint_from_dir_probe<'a>(
    name: &str,
    fallback_root: PathBuf,
    probe: Option<&scanner::HarnessProbe>,
    recs: impl Iterator<Item = &'a crate::models::SessionRecord>,
) -> HarnessFootprint {
    let Some(probe) = probe else {
        return HarnessFootprint {
            note: "registry has no entry for this platform — session count unknown (not 0)"
                .to_string(),
            ..default_footprint(name, fallback_root)
        };
    };
    let root = probe.root.clone().unwrap_or(fallback_root);
    if !probe.installed_p() {
        return HarnessFootprint {
            note: if probe.note.is_empty() {
                "registry did not scan this harness — session count unknown (not 0)".to_string()
            } else {
                probe.note.clone()
            },
            ..default_footprint(name, root)
        };
    }
    let mut footprint = coverage_from_records(name, root, recs);
    footprint.installed = true;
    footprint.candidate_count = probe.candidate_count;
    footprint.unreadable_count = probe.unreadable_count;
    footprint.unreadable_entry_count = probe.unreadable_entry_count;
    if probe.unreadable_count.is_some_and(|count| count > 0)
        || probe.unreadable_entry_count.is_some_and(|count| count > 0)
    {
        footprint.note = probe.note.clone();
    }
    footprint
}

fn default_footprint(name: &str, root: PathBuf) -> HarnessFootprint {
    HarnessFootprint {
        name: name.to_string(),
        root: Some(root),
        installed: false,
        session_count: None,
        candidate_count: None,
        unreadable_count: None,
        unreadable_entry_count: None,
        total_bytes: None,
        time_basis: TimeBasis::Unmeasured,
        earliest: None,
        latest: None,
        compressed_count: 0,
        recognized_files: Vec::new(),
        note: "not installed".to_string(),
    }
}

/// Turn the D1/D2/D3 findings into a short list of "so what + when" lines.
///
/// `scan_failed` means the registry-driven scan could not run: the
/// claude/codex risk lines depend on session counts and are *omitted* rather
/// than fabricated from a bogus zero.
fn build_risks(
    claude: &ClaudeCheck,
    gemini: &GeminiRetention,
    footprints: &[HarnessFootprint],
    scan_failed: bool,
) -> Vec<String> {
    let today = format_date(SystemTime::now());
    let mut risks = Vec::new();

    // --- Claude Code -----------------------------------------------------
    let claude_fp = footprints
        .iter()
        .find(|f| f.name == "claude-code")
        .cloned()
        .unwrap_or_else(|| default_footprint("claude-code", PathBuf::new()));
    if scan_failed {
        risks.push(
            "🔴 registry missing / unparseable — session coverage unknown; refusing to fake a full scan with hardcoded paths.".to_string(),
        );
    } else {
        match &claude.verdict {
            ClaudeRetention::UnsetDefault => {
                // B90: these two values used to be read from the *same*
                // `Option` and disagree about whether it was known — the date
                // printed an honest `n/a` while the day count quietly became
                // `0`. The result was one sentence, half of it measured and
                // half of it invented, and the invented half ("your history has only about
                // 0 days left") is the kind that makes a reader act *now*: it reads as
                // "your archive is deleted tomorrow".
                //
                // The risk itself does not depend on the earliest session —
                // cleanupPeriodDays being unset means 30-day rotation whatever
                // is on disk — so the line is kept and only the fabricated
                // number is removed. Dropping the whole risk instead would
                // trade a false alarm for a missing one.
                risks.push(match claude_fp.earliest.and_then(|e| days_since(e).map(|d| (e, d))) {
                    Some((earliest, days_old)) => {
                        // W285 §5: this number is days-since-**mtime** (see
                        // `coverage_from_records`), so the sentence names the
                        // clock it is measured on and no longer calls it "your
                        // earliest session" unqualified. It also used to end
                        // "...your history has only about {days_old} days
                        // left", which is the session's *age* printed as the
                        // time remaining: a file written today gave "about 0
                        // days ago ... only about 0 days left" — a
                        // self-contradictory false alarm. Under a 30-day window
                        // the oldest batch has 30 - age days before it is
                        // deleted, so that is what is stated, in whichever
                        // direction keeps the sentence true.
                        format!(
                            "🔴 Claude Code: cleanupPeriodDays is unset → default 30 days. Your oldest session file's mtime is {} (about {days_old:.0} days ago, today {today}).\
                             \n    — cleanup deletes on that same file-mtime clock; the oldest batch is {}.",
                            format_date(earliest),
                            threshold_phrase(days_old, 30.0)
                        )
                    }
                    None => format!(
                        "🔴 Claude Code: cleanupPeriodDays is unset → default 30 days. The earliest session time is unknown (no claude-code sessions were scanned on this machine, their timestamps were unreadable, or they fall in the future), so how many days remain cannot be estimated (today {today}).\
                         \n    — the risk does not go away: the next cleanup run will still delete the first batch older than 30 days."
                    ),
                });
            }
            ClaudeRetention::Safe { days, source } => {
                risks.push(format!(
                "🟡 Claude Code: cleanupPeriodDays = {days} days (large value set, source {}) → local history is not at risk of rotation.\
                 \n    — but this is fail-destructive: any settings.json parse failure silently reverts to 30 days and starts deleting; a `.last-cleanup` timestamp existing means the cleanup job has run.",
                source.display()
            ));
            }
            ClaudeRetention::SmallValue { days, source } => {
                risks.push(format!(
                "🔴 Claude Code: cleanupPeriodDays = {days} days (source {}) → below the safe threshold, still rotating.\
                 \n    — the next cleanup run will delete the first batch older than {days} days.",
                source.display()
            ));
            }
            ClaudeRetention::ParseFailed { path, error } => {
                risks.push(format!(
                "🔴🔴 Claude Code: settings parse failed — this is the fail-destructive bug's trigger condition itself:\
                 \n    — whatever you configured, it has now silently reverted to a 30-day window and begun counting deletions. File {}: {error}",
                path.display()
            ));
            }
        }
    }

    // --- Gemini -----------------------------------------------------------
    let gem_fp = footprints.iter().find(|f| f.name == "gemini");
    let gem_earliest = gem_fp.and_then(|f| f.earliest).map(format_date);
    let gem_days = gem_fp.and_then(|f| f.earliest).and_then(days_since);
    if let Some(error) = &gemini.unreadable {
        // A failed settings read is a policy unknown, even when the session
        // directory happens to be absent; defaulting here would bless a
        // retention decision the doctor did not actually inspect.
        risks.push(format!(
            "🟡 Gemini: sessionRetention unknown (config unreadable: {error}) — not guessing the retention risk from a default."
        ));
    } else if !gem_fp.map_or(true, |f| f.installed) {
        // nothing to clean: skip silently to not cry wolf
    } else if gemini.is_dangerous() {
        // B90: `(gem_earliest, gem_days)` are two `map`s over one `Option`, so
        // the `unwrap_or(0.0)` that used to sit in the `Some(date)` arm was
        // unreachable today — and was exactly the shape of the Claude bug
        // above, one refactor away from printing an invented "0 days ago". Matched
        // as a pair, the fabricated default has nowhere left to live.
        risks.push(match (gem_earliest, gem_days) {
            (Some(date), Some(days)) => {
                // W285 §5: same two defects as the Claude line above — the
                // number is days-since-mtime and went unlabelled, and `over`
                // was printed raw, so a session younger than the window was
                // described as "already about -30 days past the 30-day
                // threshold".
                format!(
                    "🔴 Gemini: sessionRetention not configured (default 30 days, enabled=true). Your oldest session file's mtime is {date} (about {days:.0} days ago, today {today}) — {}.\
                     \n    — cleanup has not triggered (or run) yet, but the next run will delete the earliest batch. Sessions live in ~/.gemini/tmp (the directory is literally named tmp).",
                    threshold_phrase(days, 30.0)
                )
            }
            _ => format!(
                "🔴 Gemini: sessionRetention not configured (default 30 days, enabled=true), but no session-*.json files were found to determine the earliest session.\
                 \n    — once it starts running, it will begin deleting after 30 days."
            ),
        });
    } else {
        risks.push(format!("🟢 Gemini: {} — no risk.", gemini.summarize()));
    }

    // --- Codex --------------------------------------------------------------
    // Scan-count dependent: omitted (not fabricated) when the scan failed.
    if !scan_failed {
        if let Some(cx) = footprints.iter().find(|f| f.name == "codex") {
            if cx.installed {
                let count = footprint_count_label(cx);
                risks.push(if cx.compressed_count > 0 {
                    format!(
                        "🟡 Codex: the source has no day-based auto-deletion, but {n} idle rollouts have been compressed to .jsonl.zst — \
                         \n    — no action needed now; idle means compressed, unrelated to \"deletion\".",
                        n = cx.compressed_count
                    )
                } else {
                    format!(
                        "🟢 Codex: the source has no day-based auto-deletion ({count} rollouts on this machine all uncompressed) — no risk."
                    )
                });
            }
        }
    }

    // --- opencode / hermes-agent / cursor / grok (single-SQLite stores) -----
    for (fp_name, label) in [
        ("opencode", "opencode"),
        ("hermes-agent", "Hermes Agent"),
        ("cursor", "Cursor"),
        ("grok", "Grok"),
        ("zed", "Zed"),
    ] {
        if let Some(fp) = footprints.iter().find(|f| f.name == fp_name) {
            if fp.installed {
                risks.push(format!(
                    "🟢 {label}: single SQLite ({}, {}) — no day-based rotation; the risk comes from its own SQLite, not silent session deletion.",
                    footprint_root_label(fp),
                    footprint_bytes_label(fp)
                ));
            }
        }
    }

    risks
}

// ---------------------------------------------------------------------------
// The main `doctor` entry point
// ---------------------------------------------------------------------------

/// One machine-log row in `doctor` (ADR-053 D4): a declared per-machine input
/// log, its fate, and — when the configured stage can be read — how much of it
/// is sealed. Counts only; never a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineLogFootprint {
    /// Registry harness id that declares the log.
    pub harness: String,
    /// The log's own declared id.
    pub log_id: String,
    /// The declared file exists on this machine.
    pub present: bool,
    /// Sealed generations on the configured stage. `None` is "not measured" —
    /// no stage configured or readable, or no machine identity to key the
    /// namespace with — and must never be printed as `0`.
    pub generations: Option<usize>,
    /// Lines sealed across those generations, same `None` rule.
    pub lines: Option<u64>,
}

/// The machine-log half of a `doctor` report (ADR-053 D4).
///
/// Machine logs are **not** sessions: they are their own product kind, so they
/// get their own rows and their own tallies. Nothing here is summed into the
/// session footprints above, and a log that was not looked at is reported as
/// such rather than as a log that was found empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachineLogRows {
    /// One row per declared log that exists on this machine.
    pub present: Vec<MachineLogFootprint>,
    /// Declared logs this machine never looked at (no registry cell for this
    /// platform, an `unascertained` cell, an unresolvable template).
    pub unlooked: usize,
    /// Declared logs that were looked at but whose shape could not be
    /// established.
    pub indeterminate: usize,
    /// Why each unlooked log was not looked at, in the scanner's fixed
    /// vocabulary — so a reader can tell a policy decision (an `unascertained`
    /// registry cell, this platform has no cell) from an oversight.
    pub unlooked_reasons: Vec<&'static str>,
}

/// Result of one full `doctor` run.
#[derive(Debug)]
pub struct DoctorReport {
    pub config_source: crate::config::ConfigSource,
    /// `Some(reason)` when the config file exists but could not be used. Every
    /// check that reads the config was then **not performed**, and
    /// [`DoctorReport::not_checked`] names them: this field being set is exactly
    /// the condition under which the empty collections in this report mean "did
    /// not look" rather than "found nothing".
    pub config_error: Option<String>,
    pub claude: ClaudeCheck,
    pub gemini: GeminiRetention,
    pub footprints: Vec<HarnessFootprint>,
    pub other_present: Vec<PathBuf>,
    pub risks: Vec<String>,
    /// `None` when the config could not be read, so the repository this check
    /// would have opened was never named — see [`DoctorReport::config_error`].
    pub reclaim: Option<ReclaimCheck>,
    /// D6 — how much the local rustic metadata cache occupies (and whether
    /// `rustic_no_cache` has turned it off). `None` for the same reason as
    /// `reclaim`: a cache directory comes from the config, and a cache measured
    /// at a path nobody configured is not an answer.
    pub cache: Option<CacheCheck>,
    /// D9 — how much this machine's **body** cache occupies, and against which
    /// quota (ADR-034). A different cache from D6's: that one holds metadata,
    /// this one holds conversation bodies. `None` for the same reason as
    /// `reclaim` and `cache`: both the root and the quota come out of the config,
    /// so a report whose config could not be read has nothing to measure and must
    /// not report a number taken at a root nobody chose.
    pub body_cache: Option<BodyCacheCheck>,
    /// D10 — local full-text index health, keyed by declared destination.
    /// `None` means the config could not be read, not that no index exists.
    pub fts_indexes: Option<Vec<FtsIndexCheck>>,
    /// Per-harness fate decided by the path registry (`scanner::scan`).
    pub probes: Vec<scanner::HarnessProbe>,
    /// Registry-recognised sessions that are not represented by a
    /// `SessionRecord` and therefore cannot be consumed by `collect`.
    pub archive_gaps: Vec<scanner::ArchiveGap>,
    /// Declared machine logs and how much is sealed (ADR-053 D4). Separate
    /// from `footprints`, which counts sessions: a machine log that appeared in
    /// a session table would falsify that table's every number.
    pub machine_logs: MachineLogRows,
    /// True when the registry-driven scan failed (registry missing/unparseable)
    /// — the coverage numbers are then *unknown*, never faked zeros.
    pub scan_failed: bool,
    /// ADR-023 — one real connection per declared destination. Empty unless
    /// somebody probed (see [`probe_destinations`]), which is why it is filled
    /// in by the CLI rather than by [`run()`].
    pub destinations: Vec<DestinationProbe>,
    /// Identical shard content in the configured local stage. `None` means the
    /// config itself could not be read, rather than a clean stage.
    pub stage_duplicate_shards: Option<StageDuplicateCheck>,
    /// D8 — the Native Messaging registration on this machine and the stage
    /// `[native_host]` points at. `None` when the caller did not ask (the plain
    /// [`run()`] entry point fills it in; the field is an `Option` so a caller
    /// can distinguish "checked and found nothing" from "not checked").
    pub native_host: Option<NativeHostCheck>,
    /// Which archive keys this machine holds, per copy: the local repository's
    /// and each declared destination's, with whether each file is here and
    /// whether the user has declared they keep a copy. `None` when the config
    /// could not be read — every path in the inventory is derived from it, so
    /// there is no answer rather than an empty one (the same distinction
    /// `fts_indexes` and `native_host` carry).
    pub keys: Option<Vec<KeyInventoryRow>>,
}

/// D10 state for one destination's disposable full-text index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FtsIndexCheck {
    pub destination: String,
    pub state: &'static str,
    pub documents: Option<usize>,
}

/// What one read-only connection to a declared destination answered.
#[derive(Debug, Clone)]
pub struct DestinationProbe {
    pub name: String,
    pub repo_root: String,
    pub outcome: DestinationOutcome,
    /// W158 — how fresh the activity indexes in this destination are against
    /// the running CLI. `None` means **not checked** (the destination was not
    /// reached, is not configured, or holds no repository yet) — never "fresh".
    pub activity_index: Option<ActivityIndexFreshness>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageDuplicateMachine {
    pub machine: String,
    pub duplicate_sessions: usize,
    pub duplicate_shards: usize,
    pub duplicate_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageDuplicateCheck {
    pub state: &'static str,
    pub machines: Option<Vec<StageDuplicateMachine>>,
    pub why: Option<String>,
    pub repair_command: &'static str,
}

/// Whether the activity indexes a destination carries were written by the
/// running CLI. The three answers are kept apart for the same reason the rest
/// of this file keeps its tri-states apart: an archive that could not be read
/// is not an archive that is up to date.
#[derive(Debug, Clone)]
pub enum ActivityIndexFreshness {
    /// Every machine with a snapshot records a writer version at or above the
    /// running one, so every index it carries was built by this build or newer.
    Current { machines: usize },
    /// At least one machine's index was written by an older CLI — or before
    /// writer versions were recorded at all (≤0.3.0), which is the same thing
    /// seen from here.
    Behind {
        /// Machines behind the running CLI, each with the exact repair command.
        stale: Vec<StaleActivityIndex>,
        /// Machines with a snapshot in this destination (the denominator).
        machines: usize,
    },
    /// The destination's writer records could not be read: UNKNOWN, not fresh.
    Unknown { detail: String },
}

/// One machine whose archived activity index predates the running CLI.
#[derive(Debug, Clone)]
pub struct StaleActivityIndex {
    pub machine: String,
    /// The recorded writer version; `None` = no record at all (≤0.3.0 wrote
    /// none), which is *not* the same as a version read as empty.
    pub recorded_version: Option<String>,
    /// The command that rebuilds this machine's index **on the destination**,
    /// ready to paste. It names the machine, so it can never silently target
    /// another partition.
    pub repair_command: String,
}

/// The exact command that repairs one machine's archived activity index.
///
/// Kept in one place so the line `doctor` prints, the warning `search`/`export`
/// emit, and the README can never drift into three different spellings of it.
pub fn activity_index_repair_command(destination: &str, machine: &str) -> String {
    format!(
        "chat-stasher activity-index --rebuild --destination {destination} --machine {machine} \
         --stage <workspace>"
    )
}

/// How fresh this destination's activity indexes are, judged against
/// `crate::sidecar::index_writer_is_behind` for every machine it holds.
///
/// Read-only: it lists snapshots and reads each machine's small `writer.json`
/// out of its newest snapshot, exactly like `overview` and `status` do.
pub fn probe_activity_index(cfg: &StoreConfig, name: &str) -> ActivityIndexFreshness {
    let mk = match store::load_key_file(cfg) {
        Ok(mk) => mk,
        Err(e) => {
            return ActivityIndexFreshness::Unknown {
                detail: format!("cannot read the destination's masterkey: {e:#}"),
            };
        }
    };
    let archived = match crate::store::BackupStore::for_metadata_query(cfg.clone())
        .read_archived_writers(&mk)
    {
        Ok(archived) => archived,
        Err(e) => {
            return ActivityIndexFreshness::Unknown {
                detail: format!("cannot read the destination's writer versions: {e:#}"),
            };
        }
    };
    let running = env!("CARGO_PKG_VERSION");
    let mut stale = Vec::new();
    for status in
        crate::sidecar::writer_statuses(&archived.machines, &archived.records, &archived.unreadable)
    {
        // `None` is "could not be read" — the machine is neither listed as
        // behind nor counted as fresh, and `Unknown` below is not raised for
        // it either: one unreadable record must not turn a readable archive's
        // whole answer into "unknown".
        if crate::sidecar::index_writer_is_behind(
            status.chat_stasher_version.as_deref(),
            status.version_unreadable,
            running,
        ) == Some(true)
        {
            stale.push(StaleActivityIndex {
                repair_command: activity_index_repair_command(name, &status.machine),
                recorded_version: status.chat_stasher_version.clone(),
                machine: status.machine.clone(),
            });
        }
    }
    if stale.is_empty() {
        ActivityIndexFreshness::Current {
            machines: archived.machines.len(),
        }
    } else {
        ActivityIndexFreshness::Behind {
            stale,
            machines: archived.machines.len(),
        }
    }
}

/// The answers a destination can give, kept apart for the same reason the rest
/// of this file keeps its tri-states apart: "the host said there is no
/// repository", "the host did not answer" and "this destination is not filled
/// in" are three different findings, and only the first one is about the
/// archive.
#[derive(Debug, Clone)]
pub enum DestinationOutcome {
    /// The host answered. `repository_exists` is what it answered.
    Reached { repository_exists: bool },
    /// The host did not answer. `kind` is the classifier's verdict, `None`
    /// when the failure matched none of the known signatures — the raw chain
    /// is in `detail` either way, so an unclassified failure is reported
    /// verbatim rather than dropped.
    Unreachable {
        kind: Option<crate::remote_err::RemoteErrorKind>,
        detail: String,
    },
    /// No connection was even attempted: the destination has no `repo`, so
    /// there is no address to dial. Reporting this as "unreachable" would put
    /// a config mistake and a dead network in the same bucket.
    NotConfigured { detail: String },
}

/// One archive copy's key file, and whether it exists on this machine and has
/// been declared saved — reported by `doctor`/`status` so the user can see
/// which keys they hold a copy of (W281 BUG-2: every destination carries its
/// own key, and a lost one loses that destination).
#[derive(Debug, Clone)]
pub struct KeyInventoryRow {
    /// The destination this key belongs to, or `None` for the local
    /// repository's key. The name is the discriminator rather than a separate
    /// `scope` string, so a destination a user happens to call `local` cannot
    /// be confused with the local copy.
    pub name: Option<String>,
    pub path: PathBuf,
    /// Whether the key file exists on this machine.
    pub exists: bool,
    /// Whether the user has declared (never verified) they keep a copy of
    /// *this file* — three states, not two: a key that cannot be read to
    /// compare against the record is unknown, and must not be reported as
    /// either declared-saved or plainly not declared (see
    /// `keydecl::DeclaredFor`).
    pub declared_saved: crate::keydecl::DeclaredFor,
}

impl KeyInventoryRow {
    /// `"local"` or `"destination"` — the machine-readable kind, derived from
    /// [`Self::name`] so the two can never disagree.
    pub fn scope(&self) -> &'static str {
        if self.name.is_some() {
            "destination"
        } else {
            "local"
        }
    }
}

/// Which archive keys this machine holds, per copy: the local repository's and
/// each declared destination's. A destination key is its own row unless it
/// points at the local key (an override sharing the local archive's
/// declaration), because then the local row already names it.
///
/// `declared_saved` is decided by `keydecl::declared_for` — a three-state
/// comparison, carried through to the row — not by the scope alone: a
/// declaration is a statement about a *file* — that path, holding those
/// bytes — so a destination whose `key_file` moved, or whose key was re-created
/// at the same path, must not read as backed up on the strength of a record
/// made about a different file. A file that exists but cannot be read is
/// `DeclaredFor::Unreadable`, never a "saved": something unreadable sits where
/// the key was, and nobody has confirmed a copy of whatever it is.
pub fn key_inventory(config: &Config) -> Vec<KeyInventoryRow> {
    let data_root = default_data_root();
    let state_dir = crate::collect::default_state_dir();
    let declarations = crate::keydecl::load(&state_dir);
    let local_path = config
        .rustic_key_file
        .as_deref()
        .map(expand_tilde)
        .unwrap_or_else(|| data_root.join("masterkey.json"));

    let mut out = Vec::new();
    out.push(KeyInventoryRow {
        name: None,
        path: local_path.clone(),
        exists: local_path.exists(),
        declared_saved: crate::keydecl::declared_for(
            &declarations,
            crate::keydecl::LOCAL_SCOPE,
            &local_path,
        ),
    });
    let mut names: Vec<&String> = config.destinations.keys().collect();
    names.sort_unstable();
    for name in names {
        let key_file = config.destinations[name]
            .key_file
            .clone()
            .map(|raw| expand_tilde(&raw))
            .unwrap_or_else(|| data_root.join(format!("masterkey-{name}.json")));
        if key_file == local_path {
            continue;
        }
        out.push(KeyInventoryRow {
            name: Some(name.clone()),
            path: key_file.clone(),
            exists: key_file.exists(),
            declared_saved: crate::keydecl::declared_for(
                &declarations,
                &crate::keydecl::destination_scope(name),
                &key_file,
            ),
        });
    }
    out
}

/// Build the [`StoreConfig`] a declared destination connects with, or `None`
/// when it names no repository.
///
/// A local copy of what `resolve_store_config` does in the CLI, minus its
/// `exit(2)` calls: `doctor` must be able to report a half-configured
/// destination instead of dying on it.
fn destination_store_config(
    config: &Config,
    name: &str,
    entry: &crate::config::DestinationConfig,
) -> Option<StoreConfig> {
    let repo_root = entry.repo.clone()?;
    let key_file = entry
        .key_file
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| default_data_root().join(format!("masterkey-{name}.json")));
    Some(
        StoreConfig {
            repo_root,
            key_file,
            connections: 0,
            options: entry.options.clone(),
            cache_dir: entry
                .cache_dir
                .as_deref()
                .or(config.rustic_cache_dir.as_deref())
                .map(PathBuf::from),
            // reason: an unset Option<bool> here means "use the default (cache on)"
            // — a config default, not an unknown read result collapsed to false.
            no_cache: entry.no_cache.or(config.rustic_no_cache).unwrap_or(false),
        }
        .with_capped_connections(entry.connections.or(config.rustic_connections)),
    )
}

/// Connect once to every declared destination, read-only.
///
/// Deliberately not called from [`run()`]: `run()` is a library entry point
/// that a dozen integration tests call, and an SFTP connect is not something a
/// test may do. The real connection belongs to the CLI, so `cmd_doctor` fills
/// the report in. The probe only ever lists — it creates nothing, so running
/// `doctor` can never be the reason a repository appears.
pub fn probe_destinations(config: &Config) -> Vec<DestinationProbe> {
    config
        .destinations
        .iter()
        .map(|(name, entry)| {
            let Some(cfg) = destination_store_config(config, name, entry) else {
                return DestinationProbe {
                    name: name.clone(),
                    repo_root: "(not configured)".to_string(),
                    outcome: DestinationOutcome::NotConfigured {
                        detail: "this destination declares no `repo`, so no connection was \
                                 attempted and nothing about it is known"
                            .to_string(),
                    },
                    activity_index: None,
                };
            };
            let repo_root = cfg.repo_root.clone();
            let (outcome, activity_index) =
                match crate::remote_err::probe_destination_connectivity(&cfg) {
                    Ok(repository_exists) => {
                        // The index check only means something once there is a
                        // repository to read; with none, "fresh" would be a
                        // statement about an archive that does not exist.
                        let activity_index = if repository_exists {
                            Some(probe_activity_index(&cfg, name))
                        } else {
                            None
                        };
                        (
                            DestinationOutcome::Reached { repository_exists },
                            activity_index,
                        )
                    }
                    Err(e) => (
                        DestinationOutcome::Unreachable {
                            kind: crate::remote_err::classify_error_str(&format!("{e:#}")),
                            detail: crate::remote_err::format_remote_error("doctor", &e, &cfg),
                        },
                        // The connection failed, so nothing was read. Saying
                        // nothing here is "not checked", which the field
                        // already means.
                        None,
                    ),
                };
            DestinationProbe {
                name: name.clone(),
                repo_root,
                outcome,
                activity_index,
            }
        })
        .collect()
}

/// JSON shape for one destination probe.
fn destination_probe_json(p: &DestinationProbe) -> serde_json::Value {
    match &p.outcome {
        DestinationOutcome::Reached { repository_exists } => serde_json::json!({
            "name": p.name,
            "repo_root": p.repo_root,
            "kind": if *repository_exists { "repository_present" } else { "repository_absent" },
            // `null` when nothing was checked (no repository yet, or the
            // connection failed) — a script must not read "not checked" as
            // "every index is current".
            "activity_index": p.activity_index.as_ref().map(activity_index_json),
        }),
        DestinationOutcome::Unreachable { kind, detail } => serde_json::json!({
            "name": p.name,
            "repo_root": p.repo_root,
            "kind": "unreachable",
            // Classifier verdict, or null when the failure matched no known
            // signature — never a fabricated category.
            "error_kind": kind.map(|k| k.slug()),
            "detail": detail,
        }),
        DestinationOutcome::NotConfigured { detail } => serde_json::json!({
            "name": p.name,
            "repo_root": p.repo_root,
            "kind": "not_configured",
            "detail": detail,
        }),
    }
}

/// JSON shape for one row of the key inventory.
///
/// `declared_saved` and `declaration_is_verified` are deliberately two fields:
/// the second is always `false`, because nothing can check that a copy exists.
/// A consumer must not read `declared_saved: true` as "this key is backed up".
///
/// `declared_state` carries the three states behind that boolean:
/// `"declared"` (the record is about this file), `"not_declared"` (a known
/// no — nothing recorded, another file, or bytes that changed since), and
/// `"unreadable"` (the file could not be read to compare — an unknown, so the
/// boolean reads `false` for the safe reason rather than a measured one).
fn key_inventory_json(row: &KeyInventoryRow) -> serde_json::Value {
    let mut value = serde_json::json!({
        "scope": row.scope(),
        "path": row.path.display().to_string(),
        "exists": row.exists,
        "declared_saved": row.declared_saved.is_declared(),
        "declared_state": row.declared_saved,
        "declaration_is_verified": false,
    });
    if let Some(name) = &row.name {
        value["name"] = serde_json::json!(name);
    }
    value
}

/// The `keys` block of a JSON report, built from a config the caller already
/// read. `status` and `doctor` both report this, so the two share one
/// implementation rather than two readings of the same declaration file.
pub fn keys_json(config: &Config) -> serde_json::Value {
    let rows = key_inventory(config);
    serde_json::json!({
        "checked": true,
        "copies": rows.iter().map(key_inventory_json).collect::<Vec<_>>(),
    })
}

/// The `[keys]` lines `status` prints: one per **destination** whose key needs
/// attention, and nothing at all otherwise.
///
/// `status`'s default body is a fixed handful of lines on purpose (see
/// [`crate::doctor::print_report`]'s counterpart in `main.rs` and B78), so this
/// follows the same rule as the activity-index line below: it speaks only when
/// there is something to say. A machine with no destinations, or whose
/// destination keys are all present and declared, prints no `[keys]` line. The
/// full inventory — including the local copy — is `doctor`'s section, and
/// [`keys_json`] carries it in `status --json` for scripts.
///
/// The local key is deliberately not a `status` alert: it is the wizard's step 3
/// and `doctor`'s first row, and a machine that has simply not archived anything
/// yet has no local key without anything being wrong.
pub fn key_alert_lines(config: &Config) -> Vec<String> {
    key_inventory(config)
        .iter()
        .filter_map(|row| {
            let name = row.name.as_deref()?;
            match (row.declared_saved, row.exists) {
                // Something unreadable sits at the key's path: this is neither
                // "not on this machine" (a path is there) nor "no copy was
                // declared" (nobody could tell). Reported as unknown, in its
                // own words, because being told "not declared" here would send
                // the user looking for a declaration they may already have.
                (crate::keydecl::DeclaredFor::Unreadable, _) => Some(format!(
                    "[keys] destination={name} key={} unreadable — unknown whether it is still \
                     the key that was declared saved; a key that cannot be read opens no copy",
                    row.path.display()
                )),
                // This machine cannot open that copy at all, whatever the user
                // has declared: the file is not here.
                (_, false) => Some(format!(
                    "[keys] destination={name} key={} is NOT on this machine — this machine \
                     cannot read that copy until it is restored there",
                    row.path.display()
                )),
                (crate::keydecl::DeclaredFor::NotDeclared, true) => Some(format!(
                    "[keys] destination={name} key={} is here but no saved copy was ever \
                     declared — back it up; a second machine opens that copy with this file",
                    row.path.display()
                )),
                (crate::keydecl::DeclaredFor::Declared, true) => None,
            }
        })
        .collect()
}

/// JSON shape for the activity-index freshness of one destination. `recorded`
/// is `null` when no writer version was recorded at all (≤0.3.0) — an absent
/// record, never an empty version string.
fn activity_index_json(freshness: &ActivityIndexFreshness) -> serde_json::Value {
    match freshness {
        ActivityIndexFreshness::Current { machines } => serde_json::json!({
            "kind": "current",
            "machines": machines,
        }),
        ActivityIndexFreshness::Behind { stale, machines } => serde_json::json!({
            "kind": "behind",
            "machines": machines,
            "stale": stale
                .iter()
                .map(|s| serde_json::json!({
                    "machine": s.machine,
                    "recorded_version": s.recorded_version,
                    "repair_command": s.repair_command,
                }))
                .collect::<Vec<_>>(),
        }),
        ActivityIndexFreshness::Unknown { detail } => serde_json::json!({
            "kind": "unknown",
            "detail": detail,
        }),
    }
}

/// The checks a run cannot perform without a usable config, in the order
/// [`print_report`] presents them. [`DoctorReport::not_checked`] returns this
/// list only for a report whose `config_error` is set.
const CHECKS_NEEDING_CONFIG: [&str; 10] = [
    "D3 harness scan, footprints and archive gaps",
    "D4 risk summary",
    "D5 repository reclaim",
    "D6 local metadata cache",
    "D9 body cache",
    "D10 full-text indexes",
    "D7 destination probes",
    "D8 native host stage",
    "D11 stage duplicate scan",
    "machine identity",
];

/// The report for a config file that exists and cannot be used.
///
/// Deliberately **not** a diagnosis: it carries the two checks that read no
/// config (D1 Claude settings, D2 Gemini settings, and the directories merely
/// listed) plus the reason, and says out loud which checks were skipped. Every
/// config-derived field is left empty, except that an unexpandable
/// `rustic_cache_dir` gets a specific unavailable result so the user knows
/// which configured path failed. `config_error` tells consumers that all other
/// empty config-derived fields mean "did not look" — this is the whole reason
/// `doctor` is the one command that does not refuse: the user needs to be told
/// *which* half of their setup is broken, and that answer is worthless if it is
/// dressed up as a healthy machine.
pub fn config_unreadable(error: String) -> DoctorReport {
    let home = crate::config::home_dir();
    let other_present = OTHER_HARNESS_DIRS
        .iter()
        .filter(|d| home.join(d).is_dir())
        .map(|d| home.join(d))
        .collect();
    let cache = error.contains("rustic_cache_dir:").then(|| CacheCheck::Unavailable {
        detail: "`rustic_cache_dir` could not be expanded; fix that value in the config and run `chat-stasher doctor` again".to_string(),
    });
    DoctorReport {
        config_source: crate::config::ConfigSource::Unreadable,
        config_error: Some(error),
        claude: inspect_claude_settings(&home),
        gemini: inspect_gemini_settings(&home),
        footprints: Vec::new(),
        other_present,
        risks: Vec::new(),
        reclaim: None,
        cache,
        // D9's root and quota both come from the config, so it is one of the
        // checks that did not run — not a body cache of zero bytes.
        body_cache: None,
        fts_indexes: None,
        probes: Vec::new(),
        archive_gaps: Vec::new(),
        // True as well: the scan is one of the checks that did not run, and this
        // is the field a consumer that predates `config_error` already reads.
        scan_failed: true,
        // Nothing was looked at, and the stage path itself comes from the
        // config that could not be read: no rows, no tallies — not zeros that
        // would read as "there are none".
        machine_logs: MachineLogRows::default(),
        destinations: Vec::new(),
        stage_duplicate_shards: None,
        native_host: None,
        // Every key path here is derived from the config (the local key's
        // override and each destination's), so an unreadable config means the
        // inventory was not computed — `None`, not an empty list.
        keys: None,
    }
}

/// Measure the machine-log rows of a doctor report (ADR-053 D4).
///
/// Read-only, counts only. It takes the declared files the scan already found
/// and — only when the configured stage is a readable directory *and* a machine
/// identity exists, since the namespace is machine-keyed — counts that
/// machine's sealed generations and the lines in them. Prompt text is never
/// read: the "lines" number counts newline bytes in the sealed shards, which
/// are the harness's own complete lines (ADR-053 D2).
fn machine_log_rows(config: &Config, scan: &scanner::ScanReport) -> MachineLogRows {
    let stage = config
        .native_host
        .as_ref()
        .and_then(|native| native.stage.as_deref())
        .map(expand_tilde)
        .filter(|root| root.is_dir());
    let machine = crate::id::machine_id();
    let mut rows = MachineLogRows {
        present: Vec::new(),
        unlooked: scan.machine_logs_unlooked,
        indeterminate: scan.machine_logs_indeterminate,
        unlooked_reasons: scan.machine_logs_unlooked_reasons.clone(),
    };
    for record in &scan.machine_logs {
        // An unmeasured stage is `None`, never 0: the namespace is machine-keyed
        // and the stage path comes from the config, so a report that could not
        // read either has no answer rather than an empty one.
        let measured = match (stage.as_deref(), machine.as_deref()) {
            (Some(stage), Some(machine)) => {
                let dir = crate::store::machine_log_shard_dir(
                    stage,
                    machine,
                    &record.harness,
                    &record.log_id,
                );
                crate::store::sealed_shard_entries(&dir)
                    .ok()
                    .and_then(|entries| {
                        crate::store::concat_shards_in_dir(&dir).ok().map(|sealed| {
                            (
                                entries.len(),
                                sealed.iter().filter(|byte| **byte == b'\n').count() as u64,
                            )
                        })
                    })
            }
            _ => None,
        };
        rows.present.push(MachineLogFootprint {
            harness: record.harness.clone(),
            log_id: record.log_id.clone(),
            present: true,
            generations: measured.map(|(generations, _)| generations),
            lines: measured.map(|(_, lines)| lines),
        });
    }
    rows
}

/// The machine-log row of one log, for both channels.
fn machine_log_footprint_line(log: &MachineLogFootprint) -> String {
    match (log.generations, log.lines) {
        (Some(generations), Some(lines)) => format!(
            "{} {}: {lines} lines sealed in {generations} generation(s)",
            log.harness, log.log_id
        ),
        _ => format!(
            "{} {}: not measured (no readable stage or no machine identity)",
            log.harness, log.log_id
        ),
    }
}

fn print_machine_logs(rows: &MachineLogRows) {
    if rows.present.is_empty() && rows.unlooked == 0 && rows.indeterminate == 0 {
        return;
    }
    eprintln!(
        "  machine logs (per-machine input logs, ADR-053 — not sessions, never counted in the table above):"
    );
    for log in &rows.present {
        eprintln!("    {}", machine_log_footprint_line(log));
    }
    if rows.unlooked > 0 || rows.indeterminate > 0 {
        eprintln!(
            "    declared but not looked at: {unlooked} unlooked, {indeterminate} indeterminate",
            unlooked = rows.unlooked,
            indeterminate = rows.indeterminate
        );
        for reason in &rows.unlooked_reasons {
            eprintln!("      {reason}");
        }
    }
}

fn inspect_stage_duplicates(config: &Config) -> StageDuplicateCheck {
    const REPAIR: &str = "chat-stasher repair-duplicates --destination <name>";
    let Some(stage) = config
        .native_host
        .as_ref()
        .and_then(|native| native.stage.as_deref())
    else {
        return StageDuplicateCheck {
            state: "not_configured",
            machines: None,
            why: Some("native_host.stage is not configured, so no stage was read".to_string()),
            repair_command: REPAIR,
        };
    };
    let root = expand_tilde(stage);
    if !root.is_dir() {
        return StageDuplicateCheck {
            state: "unknown",
            machines: None,
            why: Some("the configured stage is not a readable directory".to_string()),
            repair_command: REPAIR,
        };
    }
    let sessions_root = root.join(crate::store::SESSIONS_DIR);
    let machine_entries = match fs::read_dir(&sessions_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return StageDuplicateCheck {
                state: "clean",
                machines: Some(Vec::new()),
                why: None,
                repair_command: REPAIR,
            };
        }
        Err(error) => {
            return StageDuplicateCheck {
                state: "unknown",
                machines: None,
                why: Some(format!("cannot list configured stage sessions: {error}")),
                repair_command: REPAIR,
            };
        }
    };
    let mut machines = Vec::new();
    for machine_entry in machine_entries {
        let machine_entry = match machine_entry {
            Ok(entry) => entry,
            Err(error) => {
                return StageDuplicateCheck {
                    state: "unknown",
                    machines: None,
                    why: Some(format!("cannot list configured stage machines: {error}")),
                    repair_command: REPAIR,
                };
            }
        };
        if !machine_entry.path().is_dir() {
            continue;
        }
        let machine = machine_entry.file_name().to_string_lossy().into_owned();
        let sessions = match fs::read_dir(machine_entry.path()) {
            Ok(entries) => entries,
            Err(error) => {
                return StageDuplicateCheck {
                    state: "unknown",
                    machines: None,
                    why: Some(format!("cannot list a configured stage partition: {error}")),
                    repair_command: REPAIR,
                };
            }
        };
        let mut summary = StageDuplicateMachine {
            machine,
            duplicate_sessions: 0,
            duplicate_shards: 0,
            duplicate_bytes: 0,
        };
        for session_entry in sessions {
            let session_entry = match session_entry {
                Ok(entry) => entry,
                Err(error) => {
                    return StageDuplicateCheck {
                        state: "unknown",
                        machines: None,
                        why: Some(format!("cannot list a stage session: {error}")),
                        repair_command: REPAIR,
                    };
                }
            };
            if !session_entry.path().is_dir() {
                continue;
            }
            let mut shards = match crate::store::sealed_shard_entries(&session_entry.path()) {
                Ok(shards) => shards,
                Err(error) => {
                    return StageDuplicateCheck {
                        state: "unknown",
                        machines: None,
                        why: Some(format!("cannot list a stage session's shards: {error}")),
                        repair_command: REPAIR,
                    };
                }
            };
            shards.sort_by_key(|(sequence, _)| *sequence);
            let mut bodies = Vec::with_capacity(shards.len());
            let mut duplicate_shards = 0usize;
            let mut duplicate_bytes = 0u64;
            for (_, path) in &shards {
                match fs::read(path) {
                    Ok(body) => bodies.push(body),
                    Err(error) => {
                        return StageDuplicateCheck {
                            state: "unknown",
                            machines: None,
                            why: Some(format!("cannot read a stage shard: {error}")),
                            repair_command: REPAIR,
                        };
                    }
                }
            }
            for index in crate::store::duplicate_shard_indices(&bodies) {
                duplicate_shards += 1;
                duplicate_bytes = match duplicate_bytes.checked_add(bodies[index].len() as u64) {
                    Some(bytes) => bytes,
                    None => {
                        return StageDuplicateCheck {
                            state: "unknown",
                            machines: None,
                            why: Some("duplicate byte count overflowed".to_string()),
                            repair_command: REPAIR,
                        };
                    }
                };
            }
            if duplicate_shards > 0 {
                summary.duplicate_sessions += 1;
                summary.duplicate_shards += duplicate_shards;
                summary.duplicate_bytes = match summary.duplicate_bytes.checked_add(duplicate_bytes)
                {
                    Some(bytes) => bytes,
                    None => {
                        return StageDuplicateCheck {
                            state: "unknown",
                            machines: None,
                            why: Some("duplicate byte count overflowed".to_string()),
                            repair_command: REPAIR,
                        };
                    }
                };
            }
        }
        machines.push(summary);
    }
    let found = machines
        .iter()
        .any(|machine| machine.duplicate_sessions > 0);
    StageDuplicateCheck {
        state: if found { "duplicates_found" } else { "clean" },
        machines: Some(machines),
        why: None,
        repair_command: REPAIR,
    }
}

impl DoctorReport {
    /// The checks this run did **not** perform. Empty for a report built from a
    /// usable config; [`CHECKS_NEEDING_CONFIG`] otherwise.
    pub fn not_checked(&self) -> &'static [&'static str] {
        if self.config_error.is_some() {
            &CHECKS_NEEDING_CONFIG
        } else {
            &[]
        }
    }
}

/// Run every check against the real machine and assemble the report.
pub fn run() -> DoctorReport {
    let home = crate::config::home_dir();

    // D1
    let claude = inspect_claude_settings(&home);

    // D2
    let gemini = inspect_gemini_settings(&home);

    // The config gates more than half of this report, so an unusable one is an
    // early, explicit answer rather than a report about a machine the tool could
    // not see. Filling the config-derived sections from `Config::default()`
    // would print "no destination declared", "no repository", "no stage" —
    // findings about a config nobody read.
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => return config_unreadable(format!("{e:#}")),
    };

    // D3 — use the registry-driven scanner for every directory harness. The
    // doctor no longer has a second Gemini suffix/pattern implementation.
    let (scan, scan_failed) = match scanner::scan(&config) {
        Ok(s) => (s, false),
        Err(e) => {
            eprintln!("doctor: scan failed: {e}");
            (scanner::ScanReport::default(), true)
        }
    };
    let archive_gaps = scan.archive_gaps();

    let mut footprints = Vec::new();

    let claude_root = config
        .explicit_harness_root("claude-code")
        .map(expand_tilde)
        .unwrap_or_else(|| home.join(".claude").join("projects"));
    footprints.push(footprint_from_dir_probe(
        "claude-code",
        claude_root,
        scan.probes.iter().find(|p| p.id == "claude-code"),
        scan.records
            .iter()
            .filter(|r| r.source == crate::models::HarnessSource::ClaudeCode),
    ));

    let codex_root = config
        .explicit_harness_root("codex")
        .map(expand_tilde)
        .unwrap_or_else(|| home.join(".codex").join("sessions"));
    footprints.push(footprint_from_dir_probe(
        "codex",
        codex_root,
        scan.probes.iter().find(|p| p.id == "codex"),
        scan.records
            .iter()
            .filter(|r| r.source == crate::models::HarnessSource::Codex),
    ));

    let gemini_root = config
        .explicit_harness_root("gemini-cli")
        .map(expand_tilde)
        .unwrap_or_else(|| home.join(".gemini").join("tmp"));
    footprints.push(footprint_from_dir_probe(
        "gemini",
        gemini_root,
        scan.probes.iter().find(|p| p.id == "gemini-cli"),
        scan.records
            .iter()
            .filter(|r| r.source == crate::models::HarnessSource::GeminiCli),
    ));

    // opencode, Hermes Agent, Cursor, Grok and Zed are single-SQLite stores driven by
    // the registry: their footprint rows are built straight from the registry probe
    // results, so the two tables can never disagree on count/bytes/times.
    for id in ["opencode", "hermes-agent", "cursor", "grok", "zed"] {
        match scan.probes.iter().find(|p| p.id == id) {
            Some(probe) => footprints.push(footprint_from_sqlite_probe(probe)),
            None => {
                let root = match id {
                    "opencode" => scanner::xdg_data_home().join("opencode/opencode.db"),
                    "hermes-agent" => home.join(".hermes/state.db"),
                    "cursor" => home
                        .join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"),
                    "zed" => home.join("Library/Application Support/Zed/threads/threads.db"),
                    _ => home.join(".grok/sessions/session_search.sqlite"),
                };
                footprints.push(default_footprint(id, root));
            }
        }
    }

    let other_present = OTHER_HARNESS_DIRS
        .iter()
        .filter(|d| home.join(d).is_dir())
        .map(|d| home.join(d))
        .collect();

    // D4
    let risks = build_risks(&claude, &gemini, &footprints, scan_failed);

    // D5
    let reclaim = inspect_reclaim(&config);

    // D6
    let cache = inspect_cache(&config);

    // D9
    let body_cache = inspect_body_cache(&config);

    // D10 — local-only, no repository connection and no document payload reads.
    let fts_indexes = Some(inspect_fts_indexes(&config));

    // D11 — inspect only the stage the user explicitly configured for the
    // browser host. The check reads shard bytes and reports counts only.
    let stage_duplicate_shards = inspect_stage_duplicates(&config);

    // D8 — read-only, opens nothing but the manifests themselves. The root is
    // the machine's, resolved here because this is the one caller that means the
    // real machine: `%LOCALAPPDATA%` on Windows, and `home` everywhere else.
    let native_host_root =
        crate::nativehost::machine_root(crate::nativehost::Platform::current(), &home);
    let native_host = inspect_native_host(&config, &native_host_root);

    // ADR-053 D4 — measured read-only, and reported in its own rows.
    let machine_logs = machine_log_rows(&config, &scan);
    let probes = scan.probes;
    DoctorReport {
        config_source: config.source,
        config_error: None,
        claude,
        gemini,
        footprints,
        other_present,
        risks,
        reclaim: Some(reclaim),
        cache: Some(cache),
        body_cache: Some(body_cache),
        fts_indexes,
        probes,
        archive_gaps,
        machine_logs,
        scan_failed,
        // Filled in by the CLI: connecting to a destination is a real network
        // action and this entry point is called directly by the test suite.
        destinations: Vec::new(),
        stage_duplicate_shards: Some(stage_duplicate_shards),
        native_host: Some(native_host),
        keys: Some(key_inventory(&config)),
    }
}

/// Unified `~` expansion, delegated to `config` so every consumer shares one
/// implementation. `doctor` is read-only: an unexpandable path (missing home,
/// `~otheruser`, a literal `~` component) is warned about and probed as
/// written — the probe will simply find nothing, and doctor never writes a
/// masterkey, so no literal `~` can leak credentials here.
fn expand_tilde(p: &str) -> PathBuf {
    match crate::config::expand_and_verify(p) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("warning: could not expand path `{p}`: {e}");
            PathBuf::from(p)
        }
    }
}

// ---------------------------------------------------------------------------
// D5 — reclaimable garbage in the archive repository (`prune_plan`, read-only)
// ---------------------------------------------------------------------------

/// Outcome of probing the archive repository for reclaimable garbage.
///
/// Every non-`Ok` variant is a graceful skip: `doctor` never crashes just
/// because the repository is missing, unreadable or wrongly shaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimCheck {
    /// No repository directory at the resolved path → nothing to plan.
    NoRepo { repo_root: PathBuf },
    /// Repository directory exists but the masterkey cannot be read → the
    /// repo cannot be opened, so no plan can be computed.
    NoKey { key_file: PathBuf, error: String },
    /// Repository present, but opening / indexing / planning failed.
    OpenFailed { repo_root: PathBuf, error: String },
    /// `prune_plan` (read-only) computed and measured. Nothing was executed.
    Ok {
        /// Packs referenced by no index — the real, unreferenced garbage.
        packs_unref: u64,
        /// Bytes of the unreferenced packs.
        size_unref: u64,
        /// Packs a `prune` run would repack to recover wasted space.
        packs_repack: u64,
        /// Bytes of the packs-to-repack.
        size_repack: u64,
        /// `true` when the repo config seals `append_only` — the exact reason
        /// the actual `prune` step is blocked today.
        append_only: bool,
    },
}

impl ReclaimCheck {
    /// The measurement ran and produced numbers.
    pub fn is_ok(&self) -> bool {
        matches!(self, ReclaimCheck::Ok { .. })
    }

    /// Measured garbage exists (unreferenced packs or repack candidates).
    pub fn has_garbage(&self) -> bool {
        match self {
            ReclaimCheck::Ok {
                packs_unref,
                size_unref,
                packs_repack,
                size_repack,
                ..
            } => *packs_unref > 0 || *size_unref > 0 || *packs_repack > 0 || *size_repack > 0,
            _ => false,
        }
    }
}

/// D5 — measure the archive repo's reclaimable garbage via `prune_plan`.
///
/// `PrunePlan::from_prune_options` is **read-only**: it only computes what a
/// `prune` would do and never deletes or marks anything. It succeeds even in
/// an append-only repository — it is the *execution* (`Repository::prune`)
/// that is blocked by `append_only` (`commands/prune.rs`, verified in the
/// prior spikes): the garbage is measurable, just not removable while
/// append-only holds.
pub fn inspect_reclaim(config: &Config) -> ReclaimCheck {
    use rustic_core::{Credentials, PruneOptions, PrunePlan, Repository};

    let data_root = default_data_root();
    let repo_root = config
        .rustic_repo
        .as_deref()
        .map(expand_tilde)
        .unwrap_or_else(|| data_root.join("repo"));
    let key_file = config
        .rustic_key_file
        .as_deref()
        .map(expand_tilde)
        .unwrap_or_else(|| data_root.join("masterkey.json"));

    // Distinguish measured absence from a path we could not inspect or that
    // has the wrong shape. `NotFound` does NOT prove "no repository" on its
    // own: Windows folds `ERROR_PATH_NOT_FOUND` (a path component that is a
    // regular file) into the same error code, so absence must be *confirmed*
    // — walk up to the first existing ancestor and require it to be a
    // directory — exactly as `sqlite_probe::confirm_absence` promises.
    match fs::metadata(&repo_root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return ReclaimCheck::OpenFailed {
                repo_root,
                error: "repository path exists but is not a directory".to_string(),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Err(shape) = sqlite_probe::confirm_absence(&repo_root) {
                return ReclaimCheck::OpenFailed {
                    repo_root,
                    error: format!("could not confirm repository directory: {shape}"),
                };
            }
            return ReclaimCheck::NoRepo { repo_root };
        }
        Err(e) => {
            return ReclaimCheck::OpenFailed {
                repo_root,
                error: format!("could not confirm repository directory: {e}"),
            }
        }
    }

    let cfg = StoreConfig {
        repo_root: repo_root.to_string_lossy().into_owned(),
        key_file: key_file.clone(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: config.rustic_cache_dir.as_deref().map(expand_tilde),
        // reason: rustic_no_cache unset = rustic's default (cache on); a config
        // default, not an unknown read result collapsed to false.
        no_cache: config.rustic_no_cache.unwrap_or(false),
    };

    // The masterkey is required to decrypt the index and plan.
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            return ReclaimCheck::NoKey {
                key_file,
                error: format!("{e:#}"),
            }
        }
    };

    // Open + index the repository (same read-only path `push`/`read` use).
    // Backend options are built exactly like `BackupStore::backends` (the
    // method is private to `store`; reproduced here to keep doctor self-contained).
    let mut opts = rustic_backend::BackendOptions::default().repository(cfg.repo_root.as_str());
    if cfg.repo_root.starts_with("opendal:") || cfg.repo_root.starts_with("rest:") {
        let mut options = BTreeMap::new();
        options.insert("connections".to_string(), cfg.connections.to_string());
        opts = opts.options(options);
    }
    let backends = match opts.to_backends() {
        Ok(b) => b,
        Err(e) => {
            return ReclaimCheck::OpenFailed {
                repo_root,
                error: format!("{e:#}"),
            }
        }
    };
    let repo = match Repository::new(&cfg.repository_options(), &backends) {
        Ok(r) => r,
        Err(e) => {
            return ReclaimCheck::OpenFailed {
                repo_root: repo_root.clone(),
                error: format!("{e:#}"),
            }
        }
    };
    let repo = match repo.open(&Credentials::Masterkey(mk.clone())) {
        Ok(r) => r,
        Err(e) => {
            return ReclaimCheck::OpenFailed {
                repo_root: repo_root.clone(),
                error: format!("{e:#}"),
            }
        }
    };
    let repo = match crate::orphans::index_adopting(repo, &backends, &mk) {
        Ok((r, _adoption)) => r,
        Err(e) => {
            return ReclaimCheck::OpenFailed {
                repo_root: repo_root.clone(),
                error: format!("{e:#}"),
            }
        }
    };
    let append_only = repo.config().append_only == Some(true);

    // The meat: plan-only, read-only, allowed even under append_only.
    let plan = match PrunePlan::from_prune_options(&repo, &PruneOptions::default()) {
        Ok(p) => p,
        Err(e) => {
            return ReclaimCheck::OpenFailed {
                repo_root,
                error: format!("{e:#}"),
            }
        }
    };
    let s = &plan.stats;
    ReclaimCheck::Ok {
        packs_unref: s.packs_unref,
        size_unref: s.size_unref,
        packs_repack: s.packs.repack,
        size_repack: s.size_sum().repack,
        append_only,
    }
}

// ---------------------------------------------------------------------------
// D6 — how much does the local metadata cache actually occupy?
// ---------------------------------------------------------------------------

/// Measured bytes of one rustic cache directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheUsage {
    /// Bytes actually allocated on disk, from each file's block count.
    ///
    /// 🔴 This, not `total_bytes`, is the number that answers "how much disk is
    /// this costing me". A rustic cache is tens of thousands of tiny files
    /// (measured on a real machine: 17,051 files averaging 2,397 B against a
    /// 4 KiB block size, plus 38,395 directories) so every file rounds up to a
    /// whole block: 39.0 MiB of content occupied 90.4 MiB of disk, 2.3x more.
    /// Reporting only the logical sum understates the cost by that factor, and
    /// the shape that causes it is inherent to this cache, not incidental.
    pub disk_bytes: u64,
    /// Sum of every file size under the cache root, recursively.
    pub total_bytes: u64,
    /// Number of per-repository subdirectories directly under the root.
    /// rustic stores one directory per repository id (`Cache::new` pushes
    /// `id.to_hex()`), so this is "how many repositories share this cache".
    pub repo_dirs: usize,
}

/// Outcome of probing the machine's rustic metadata cache (ADR-001 option 6,
/// made visible). Every non-`Ok` variant is a graceful skip: `doctor` never
/// crashes just because the cache is absent or unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheCheck {
    /// `rustic_no_cache = true`: the cache is deliberately off. `leftover` is
    /// what is still on disk from before the switch was turned off — `None`
    /// means it could not be determined (dir absent / unreadable), not "zero".
    Disabled {
        root: PathBuf,
        leftover: Option<CacheUsage>,
    },
    /// Cache is on, but no rustic cache directory exists yet on this machine.
    /// The occupancy is **unknown** (never measured), never a fake `0`.
    NoCacheDir { root: PathBuf },
    /// Cache is on and the directory exists, but it could not be measured.
    Unreadable { root: PathBuf, error: String },
    /// The configured cache path could not be expanded, so there is no safe
    /// filesystem path to inspect.
    Unavailable { detail: String },
    /// Cache is on and measured.
    Ok { root: PathBuf, usage: CacheUsage },
}

impl CacheCheck {
    /// Short machine-readable tag, mirrored by `cache_json`'s `kind`.
    pub fn kind_label(&self) -> &'static str {
        match self {
            CacheCheck::Disabled { .. } => "disabled",
            CacheCheck::NoCacheDir { .. } => "no_cache_dir",
            CacheCheck::Unreadable { .. } => "unreadable",
            CacheCheck::Unavailable { .. } => "unavailable",
            CacheCheck::Ok { .. } => "ok",
        }
    }
}

/// Measure a cache root: recursive byte sum plus a count of per-repository
/// subdirectories directly under the root.
///
/// Symlinks are deliberately not followed (a symlink pointing out of the tree
/// could count an unbounded amount of disk), so only real directories and
/// regular files contribute.
fn measure_cache_dir(root: &Path) -> io::Result<CacheUsage> {
    let mut total_bytes = 0u64;
    let mut disk_bytes = 0u64;
    let mut repo_dirs = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            let path = entry.path();
            if ft.is_dir() {
                if dir.as_path() == root {
                    repo_dirs += 1;
                }
                stack.push(path);
            } else if ft.is_file() {
                let md = entry.metadata()?;
                total_bytes += md.len();
                // `st_blocks` is defined in 512-byte units regardless of the
                // filesystem's own block size, so this is the allocated size.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    disk_bytes += md.blocks() * 512;
                }
                // No portable block count on Windows: fall back to the logical
                // size and say so rather than invent an allocation figure.
                #[cfg(not(unix))]
                {
                    disk_bytes += md.len();
                }
            }
        }
    }
    Ok(CacheUsage {
        disk_bytes,
        total_bytes,
        repo_dirs,
    })
}

/// The cache root `doctor` reports on: the configured `rustic_cache_dir`, or
/// the first platform default from [`store::rustic_cache_roots`] when unset.
/// Uses the store's own list rather than guessing at a path — the Windows
/// spelling differs from the other platforms and must not be invented here.
fn resolve_cache_root(config: &Config) -> PathBuf {
    if let Some(dir) = &config.rustic_cache_dir {
        expand_tilde(dir)
    } else {
        store::rustic_cache_roots()
            .into_iter()
            .next()
            .unwrap_or_else(|| crate::config::home_dir().join(".cache").join("rustic"))
    }
}

/// D6 — measure the machine's local rustic metadata cache.
///
/// The number is deliberately the **whole machine's** rustic cache: every
/// repository this machine has ever opened writes under the same root, and —
/// as noted in the spike — test repositories accumulate there too (rustic's
/// cache is keyed by repository id, not by the temporary directory that was
/// cleaned up). The report therefore says what the number includes rather than
/// pretending it is this machine's archive alone.
pub fn inspect_cache(config: &Config) -> CacheCheck {
    let root = resolve_cache_root(config);
    // reason: unset rustic_no_cache means "use the default (cache enabled)" —
    // a config default, never an unknown turned into false.
    if config.rustic_no_cache.unwrap_or(false) {
        let leftover = measure_cache_dir(&root).ok();
        return CacheCheck::Disabled { root, leftover };
    }
    match measure_cache_dir(&root) {
        Ok(usage) => CacheCheck::Ok { root, usage },
        Err(e) if e.kind() == io::ErrorKind::NotFound => CacheCheck::NoCacheDir { root },
        Err(e) => CacheCheck::Unreadable {
            root,
            error: e.to_string(),
        },
    }
}

// ---------------------------------------------------------------------------
// D9 — how much does the local *body* cache occupy, against which quota?
// ---------------------------------------------------------------------------

/// Outcome of probing this machine's body cache (ADR-034). Every non-`Ok`
/// variant is a graceful skip: `doctor` never crashes just because the cache is
/// absent, off, or unreadable.
///
/// This is deliberately a *separate* type from [`CacheCheck`]: D6 measures
/// rustic's metadata cache, whose root comes from `rustic_cache_dir` and whose
/// occupancy is not governed by any quota, while this one holds conversation
/// bodies under `[cache] max_bytes`. One type with two meanings would make
/// `doctor --json` unable to say which cache it was answering about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyCacheCheck {
    /// `[cache] max_bytes = 0`: the cache is deliberately off. `leftover` is
    /// what is still on disk from before the switch was turned off — `None`
    /// means there is no directory to measure (which is *unknown*, and also the
    /// normal case for a machine that never had one).
    Disabled {
        root: PathBuf,
        leftover: Option<crate::body_cache::Usage>,
    },
    /// The cache is on, but no directory exists yet. Occupancy is **unknown**
    /// (never measured), never a fake `0`.
    NoCacheDir { root: PathBuf },
    /// The cache is on and the directory exists, but it could not be measured.
    Unreadable { root: PathBuf, error: String },
    /// The configured location could not be resolved at all, so there is no
    /// root to report. Distinct from "off": the user asked for a cache and it
    /// is not where they asked for it.
    Unresolved { detail: String },
    /// A directory is there that this cache did not create, so it is not the
    /// cache's occupancy, not a measured zero, and not a directory this tool
    /// will write to or delete from. Distinct from `NoCacheDir` (nothing is
    /// there) and from `Unreadable` (could not look).
    NotACacheRoot { root: PathBuf, detail: String },
    /// `[cache]` was present and could not be read, so the body cache is off
    /// and there is no quota to report. Distinct from `Disabled` (the user
    /// wrote `max_bytes = 0`) and from an absent section (the default quota):
    /// the fix is a different line of the config in each case.
    Invalid { detail: String },
    /// The cache is on and measured.
    Ok {
        root: PathBuf,
        max_bytes: u64,
        usage: crate::body_cache::Usage,
    },
}

impl BodyCacheCheck {
    /// Short machine-readable tag, mirrored by `body_cache_json`'s `kind`.
    pub fn kind_label(&self) -> &'static str {
        match self {
            BodyCacheCheck::Disabled { .. } => "disabled",
            BodyCacheCheck::NoCacheDir { .. } => "no_cache_dir",
            BodyCacheCheck::Unreadable { .. } => "unreadable",
            BodyCacheCheck::Unresolved { .. } => "unresolved",
            BodyCacheCheck::NotACacheRoot { .. } => "not_a_cache_root",
            BodyCacheCheck::Invalid { .. } => "invalid",
            BodyCacheCheck::Ok { .. } => "ok",
        }
    }
}

/// D9 — measure this machine's body cache (ADR-034).
///
/// The configured quota is reported next to the occupancy in every state that
/// has one, because "the cache is using 40 GB" is only meaningful against the
/// number the user set.
pub fn inspect_body_cache(config: &Config) -> BodyCacheCheck {
    use crate::body_cache::RootState;

    // A `[cache]` section that could not be read comes first, before any
    // resolution: there is no quota to report and no location the user's own
    // value points at, and saying "off" here would read as if they had written
    // `max_bytes = 0`.
    if let Some(problem) = config.cache_error.as_deref() {
        return BodyCacheCheck::Invalid {
            detail: problem.to_string(),
        };
    }
    let settings = match crate::body_cache::settings_for(config) {
        Ok(settings) => settings,
        Err(e) => {
            return BodyCacheCheck::Unresolved {
                detail: format!("{e:#}"),
            }
        }
    };
    match crate::body_cache::root_state(&settings.root) {
        RootState::Foreign(detail) => {
            return BodyCacheCheck::NotACacheRoot {
                root: settings.root,
                detail,
            }
        }
        RootState::Unknown(detail) => {
            return BodyCacheCheck::Unreadable {
                root: settings.root,
                error: detail,
            }
        }
        RootState::Absent | RootState::Cache => {}
    }
    if !settings.enabled() {
        let leftover = crate::body_cache::measure(&settings.root).ok().flatten();
        return BodyCacheCheck::Disabled {
            root: settings.root,
            leftover,
        };
    }
    match crate::body_cache::measure(&settings.root) {
        Ok(Some(usage)) => BodyCacheCheck::Ok {
            root: settings.root,
            max_bytes: settings.max_bytes,
            usage,
        },
        Ok(None) => BodyCacheCheck::NoCacheDir {
            root: settings.root,
        },
        Err(e) => BodyCacheCheck::Unreadable {
            root: settings.root,
            error: e.to_string(),
        },
    }
}

/// D10 — inspect only local index files; this never opens an archive.
pub fn inspect_fts_indexes(config: &Config) -> Vec<FtsIndexCheck> {
    let mut identities: Vec<(String, String)> = config
        .destinations
        .keys()
        .map(|name| (name.clone(), name.clone()))
        .collect();
    if identities.is_empty() {
        if let Some(repo) = config.rustic_repo.as_ref().or(config.archive_root.as_ref()) {
            identities.push(("single-destination".to_string(), repo.clone()));
        }
    }
    let Some(cache_root) = scanner::user_cache_dirs().into_iter().next() else {
        return identities
            .into_iter()
            .map(|(destination, _)| FtsIndexCheck {
                destination,
                state: "unavailable",
                documents: None,
            })
            .collect();
    };
    identities
        .into_iter()
        .map(|(_destination, identity)| {
            let index = crate::fts::Index::for_destination(&cache_root, &identity);
            let destination = index
                .root()
                .file_name()
                .and_then(|name| name.to_str())
                .map(|digest| format!("destination-{}", &digest[..8.min(digest.len())]))
                .unwrap_or_else(|| "destination-unknown".to_string());
            match index.check() {
                Ok(report) => {
                    let state = match report.status {
                        crate::fts::CheckStatus::Valid { .. } => "valid",
                        crate::fts::CheckStatus::Partial { .. } => "partial",
                        crate::fts::CheckStatus::Incomplete => "incomplete",
                    };
                    FtsIndexCheck {
                        destination,
                        state,
                        documents: Some(report.documents),
                    }
                }
                Err(error) => FtsIndexCheck {
                    destination,
                    state: if format!("{error:#}").contains("no local index has been built") {
                        "missing"
                    } else if format!("{error:#}").contains("corrupt")
                        || format!("{error:#}").contains("invalid")
                        || format!("{error:#}").contains("unsupported")
                    {
                        "corrupt"
                    } else {
                        "unreadable"
                    },
                    documents: None,
                },
            }
        })
        .collect()
}

/// D9 JSON. Like D6's, the byte count is a tri-state: unknown when there is
/// nothing to measure (never `0`), not_applicable when the cache is off.
fn body_cache_json(c: &BodyCacheCheck) -> serde_json::Value {
    match c {
        BodyCacheCheck::Disabled { root, leftover } => serde_json::json!({
            "kind": "disabled",
            "root": root.display().to_string(),
            "total_bytes": match leftover {
                Some(u) => CountState::known(u.bytes),
                None => CountState::not_applicable(
                    "cache is disabled and no leftover cache could be measured",
                ),
            },
            "entries": leftover.as_ref().map(|u| u.entries),
        }),
        BodyCacheCheck::NoCacheDir { root } => serde_json::json!({
            "kind": "no_cache_dir",
            "root": root.display().to_string(),
            "total_bytes": CountState::unknown("cache directory does not exist"),
        }),
        BodyCacheCheck::Unreadable { root, error } => serde_json::json!({
            "kind": "unreadable",
            "root": root.display().to_string(),
            "total_bytes": CountState::unknown(error),
            "error": error,
        }),
        BodyCacheCheck::Unresolved { detail } => serde_json::json!({
            "kind": "unresolved",
            "total_bytes": CountState::unknown(detail),
            "error": detail,
        }),
        BodyCacheCheck::NotACacheRoot { root, detail } => serde_json::json!({
            "kind": "not_a_cache_root",
            "root": root.display().to_string(),
            "total_bytes": CountState::unknown(detail),
            "error": detail,
        }),
        BodyCacheCheck::Invalid { detail } => serde_json::json!({
            "kind": "invalid",
            "total_bytes": CountState::unknown(detail),
            "error": detail,
        }),
        BodyCacheCheck::Ok {
            root,
            max_bytes,
            usage,
        } => serde_json::json!({
            "kind": "ok",
            "root": root.display().to_string(),
            "max_bytes": CountState::known(*max_bytes),
            "total_bytes": CountState::known(usage.bytes),
            "entries": usage.entries,
            // Always present in the JSON, even at zero: a script comparing two
            // machines should be able to read the count rather than infer it
            // from a missing key.
            "foreign_entries": usage.foreign_entries,
        }),
    }
}

/// D9 printing — shared by the normal path and the scan-failed early return.
fn print_body_cache(c: &BodyCacheCheck) {
    match c {
        BodyCacheCheck::Disabled { root, leftover } => {
            eprintln!("  body cache is off (cache.max_bytes = 0) — every read fetches from the destination.");
            eprintln!("  body cache root: {}", root.display());
            match leftover {
                Some(u) => eprintln!(
                    "  leftover on disk: {} ({} B) in {} entry file(s) — written before the switch, not by it",
                    fmt_bytes(u.bytes),
                    u.bytes,
                    u.entries
                ),
                None => eprintln!(
                    "  leftover on disk: unknown (no directory to measure), which is not the same as empty"
                ),
            }
        }
        BodyCacheCheck::NoCacheDir { root } => {
            eprintln!("  body cache root: {}", root.display());
            eprintln!(
                "  occupancy: unknown — the directory does not exist yet (never measured), not 0."
            );
        }
        BodyCacheCheck::Unreadable { root, error } => {
            eprintln!("  body cache root: {}", root.display());
            eprintln!("  occupancy: could not be measured: {error}");
        }
        BodyCacheCheck::Unresolved { detail } => {
            eprintln!("  body cache location could not be resolved: {detail}");
            eprintln!(
                "  fix `[cache] dir` in the config; reads keep working, and each one fetches from the destination."
            );
        }
        BodyCacheCheck::NotACacheRoot { root, detail } => {
            eprintln!("  body cache root: {}", root.display());
            eprintln!("  not a chat-stasher body cache: {detail}");
            eprintln!(
                "  nothing here is measured, written or deleted; reads keep working and fetch from the destination."
            );
            eprintln!(
                "  point `[cache] dir` at a directory chat-stasher created, or remove this one by hand."
            );
        }
        BodyCacheCheck::Invalid { detail } => {
            eprintln!("  body cache is off: {detail}");
            eprintln!(
                "  fix that value in the config; reads keep working, and each one fetches from the destination."
            );
        }
        BodyCacheCheck::Ok {
            root,
            max_bytes,
            usage,
        } => {
            eprintln!("  body cache root: {}", root.display());
            eprintln!(
                "  occupancy  : {} ({} B) in {} entry file(s)",
                fmt_bytes(usage.bytes),
                usage.bytes,
                usage.entries
            );
            eprintln!(
                "  quota      : {} ({} B) — one quota per machine, shared by every destination",
                fmt_bytes(*max_bytes),
                max_bytes
            );
            if usage.foreign_entries > 0 {
                // Said only when there is something to say: at zero the
                // occupancy above is already the whole of it.
                eprintln!(
                    "  foreign    : {} file(s) or directory(ies) here were not written by chat-stasher — not in the bytes above, and never deleted",
                    usage.foreign_entries
                );
            }
            eprintln!(
                "  note: this cache holds conversation bodies as the destination's own ciphertext. It is disposable —"
            );
            eprintln!(
                "        `chat-stasher cache clear` removes it, and the archive in the destination is untouched."
            );
        }
    }
}

/// Default data dir for the repository + key file. Delegated to `config` so
/// the CLI, `doctor` and the Native Messaging host cannot disagree on where
/// this machine's identity file lives.
fn default_data_root() -> PathBuf {
    crate::config::default_data_root()
}

// ---------------------------------------------------------------------------
// D8 — is the Native Messaging host actually usable? (protocol v1)
// ---------------------------------------------------------------------------

/// What `doctor` found about one browser's registered host manifest.
///
/// Every non-`Ok` state is a *different* finding, because the fix differs:
/// not being registered is a choice, a manifest pointing at a deleted binary
/// is a stale install, and a manifest pointing into `target/` works today and
/// stops working the next time anybody runs `cargo clean`.
///
/// [`HostManifestState::NoDiscoveryPath`] exists for the same reason as the rest
/// of this enum: *this build has no path for that browser on this OS* and
/// *there is no manifest at the path we looked at* are different findings — one
/// is a gap in our matrix, the other is a fact about the machine — and
/// collapsing them would report every unsupported pair as a missing
/// registration. The topology document's rule ("an unknown must never be
/// recorded as empty") is the same rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostManifestState {
    /// This build has no discovery path for this browser on this OS (for
    /// example Arc on Linux). Nothing was looked at, so nothing is known.
    NoDiscoveryPath,
    /// No manifest at this browser's discovery path.
    NotRegistered,
    /// A manifest is there and could not be read.
    Unreadable { error: String },
    /// A manifest is there and is not JSON, or carries no usable `path`.
    Invalid { error: String },
    /// `path` names nothing on this machine.
    PathMissing { path: PathBuf },
    /// `path` exists but is not executable (Unix mode bits).
    PathNotExecutable { path: PathBuf },
    /// `path` exists and is executable, but sits inside a Cargo `target/`
    /// tree: it is a build artifact and will vanish on `cargo clean`.
    BuildArtifact { path: PathBuf },
    /// Registered, present, executable, and not a build artifact.
    Ok { path: PathBuf },
}

impl HostManifestState {
    /// Short machine-readable tag, mirrored by `native_host_json`.
    pub fn kind_label(&self) -> &'static str {
        match self {
            HostManifestState::NoDiscoveryPath => "no_discovery_path",
            HostManifestState::NotRegistered => "not_registered",
            HostManifestState::Unreadable { .. } => "unreadable",
            HostManifestState::Invalid { .. } => "invalid",
            HostManifestState::PathMissing { .. } => "path_missing",
            HostManifestState::PathNotExecutable { .. } => "path_not_executable",
            HostManifestState::BuildArtifact { .. } => "build_artifact",
            HostManifestState::Ok { .. } => "ok",
        }
    }

    /// Was a manifest found at a path this build looked at?
    ///
    /// [`HostManifestState::NoDiscoveryPath`] is deliberately not one of these:
    /// nothing was looked at, so counting it as "not registered" would move a
    /// count on the strength of a gap in our own path table.
    pub fn is_registered(&self) -> bool {
        !matches!(
            self,
            HostManifestState::NoDiscoveryPath | HostManifestState::NotRegistered
        )
    }
}

/// One browser's row of the registration matrix, read-only.
///
/// The four facts are kept apart on purpose, because they answer four different
/// questions and only their combination is actionable:
///
/// * `support` — is this browser × OS in D5's matrix at all? `None` = we do not
///   look there.
/// * `detected` — is the browser's *data directory* here? `None` = this build has
///   no probe on this platform. This says **nothing** about the extension: a
///   data directory survives an uninstall and is shared by every profile, which
///   is why no field here is called `installed`. A browser can be `detected`
///   with `NotRegistered` and an extension loaded in three profiles, or
///   `detected` with a healthy registration and no extension anywhere.
/// * `manifest` — where we looked. `None` exactly when `support` is `None`.
/// * `state` — what was there.
#[derive(Debug, Clone)]
pub struct HostManifestCheck {
    pub browser: String,
    pub support: Option<crate::nativehost::Support>,
    pub detected: Option<bool>,
    pub manifest: Option<PathBuf>,
    pub state: HostManifestState,
}

/// The `[native_host] stage` key, in the same tri-state style as the rest of
/// this file: "no key written", "the path is gone" and "the path is not a
/// directory" are three different findings for three different fixes.
///
/// There is deliberately no "the config could not be read" variant here. That
/// case no longer reaches this function at all: an unusable config makes
/// [`crate::config::Config::load`] fail, and [`run`] answers with
/// [`config_unreadable`] instead — so a `NotConfigured` reaching a caller means
/// the key really is absent, not merely unread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageConfigCheck {
    /// The key is absent. The host answers every request `nack config`.
    NotConfigured,
    /// Configured, and the directory is there.
    Present { path: PathBuf },
    /// Configured, and nothing is at that path.
    Missing { path: PathBuf },
    /// Configured, and the path is something other than a directory.
    NotADirectory { path: PathBuf },
}

impl StageConfigCheck {
    pub fn kind_label(&self) -> &'static str {
        match self {
            StageConfigCheck::NotConfigured => "not_configured",
            StageConfigCheck::Present { .. } => "present",
            StageConfigCheck::Missing { .. } => "missing",
            StageConfigCheck::NotADirectory { .. } => "not_a_directory",
        }
    }
}

/// D8 — the Native Messaging registration and the stage it writes to.
#[derive(Debug, Clone)]
pub struct NativeHostCheck {
    pub manifests: Vec<HostManifestCheck>,
    /// Never optional: [`inspect_native_host`] always has an answer, and the
    /// absent-key answer (`NotConfigured`) is a finding, not a gap.
    pub stage: StageConfigCheck,
}

/// Does this path sit inside a Cargo `target/` tree?
///
/// The same question `schedule` asks about the binary it embeds (see
/// `is_build_artifact` in `schedule.rs`), asked here about the binary a browser
/// manifest points at. It is narrower than "the file exists": a path under
/// `target/` works until somebody runs `cargo clean`, and then the host stops
/// being found with no error anywhere the user would look.
fn looks_like_build_artifact(path: &Path) -> bool {
    let parts: Vec<&str> = path
        .iter()
        .filter_map(|component| component.to_str())
        .collect();
    parts
        .windows(2)
        .any(|window| window[0] == "target" && (window[1] == "debug" || window[1] == "release"))
}

/// Is `path` executable *as far as this platform can say without running it*?
///
/// Unix answers from the mode bits. Windows has no such bit — executability
/// there is decided by the extension and by the loader — so the honest answer
/// is "the question does not exist on this platform", and a one-sided `cfg`
/// returning `false` would report every Windows registration as broken.
fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(path) {
            Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Read one browser's host manifest, if it is there.
fn inspect_host_manifest(browser: crate::nativehost::Browser, root: &Path) -> HostManifestCheck {
    let platform = crate::nativehost::Platform::current();
    let support = browser.support(platform);
    let Some(target) =
        crate::nativehost::target(platform, root, browser, crate::nativehost::HOST_NAME)
    else {
        // No discovery path is known for this combination, so nothing was
        // looked at. That is a gap in this build's matrix and not a fact about
        // the machine, and the two must not be reported as the same finding.
        return HostManifestCheck {
            browser: browser.id().to_string(),
            support,
            detected: None,
            manifest: None,
            state: HostManifestState::NoDiscoveryPath,
        };
    };
    let detected = target.detected();
    let manifest = target.manifest;

    let row = |state: HostManifestState| HostManifestCheck {
        browser: browser.id().to_string(),
        support,
        detected,
        manifest: Some(manifest.clone()),
        state,
    };

    let text = match fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return row(HostManifestState::NotRegistered)
        }
        Err(e) => {
            return row(HostManifestState::Unreadable {
                error: e.to_string(),
            })
        }
    };

    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(e) => {
            return row(HostManifestState::Invalid {
                error: e.to_string(),
            })
        }
    };
    let Some(declared) = value.get("path").and_then(|path| path.as_str()) else {
        return row(HostManifestState::Invalid {
            error: "manifest has no string `path`".to_string(),
        });
    };
    let path = PathBuf::from(declared);

    let state = if !path.exists() {
        HostManifestState::PathMissing { path }
    } else if !is_executable_file(&path) {
        HostManifestState::PathNotExecutable { path }
    } else if looks_like_build_artifact(&path) {
        HostManifestState::BuildArtifact { path }
    } else {
        HostManifestState::Ok { path }
    };
    row(state)
}

/// D8 — read every browser's host manifest and the configured stage. Read-only:
/// it opens files and stats paths, and creates nothing at all.
///
/// `root` is the discovery root to look under — on macOS and Linux the home
/// directory (`~/Library/Application Support`, `$HOME`), on Windows the
/// machine's `%LOCALAPPDATA%`. It is an argument rather than something this
/// function resolves, for the same reason `install-native-host` has
/// `--target-root`: a probe must answer about the directory it was handed and
/// nothing else. Resolving it here meant the answer depended on the *process
/// environment*, so a caller asking about one directory was silently told about
/// another — and on Windows every caller in a test binary got the same answer
/// and drove the same manifest file. [`run`] resolves the machine's root with
/// [`crate::nativehost::machine_root`] and passes it in.
pub fn inspect_native_host(config: &Config, root: &Path) -> NativeHostCheck {
    let manifests = crate::nativehost::Browser::ALL
        .iter()
        .map(|browser| inspect_host_manifest(*browser, root))
        .collect();

    // No "the config could not be read" arm here: a config that cannot be used
    // makes `Config::load` fail, and `run()` returns `config_unreadable` instead
    // of calling this at all. A caller that reaches this function holds a config
    // that was read, so the absent key below really is absent.
    let stage = match config
        .native_host
        .as_ref()
        .and_then(|section| section.stage.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        None => StageConfigCheck::NotConfigured,
        Some(declared) => {
            let path = PathBuf::from(declared);
            match fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => StageConfigCheck::Present { path },
                Ok(_) => StageConfigCheck::NotADirectory { path },
                Err(_) => StageConfigCheck::Missing { path },
            }
        }
    };

    NativeHostCheck { manifests, stage }
}

/// Which of the three states this machine's registration is in — the one slug
/// `doctor --json` and `setup --json` both carry, so the two surfaces cannot
/// disagree about the same machine.
///
/// Three states and not a boolean, because "nothing registered" is not one
/// finding: a machine with browsers to register for and none registered is a
/// user action waiting to happen, while a platform this build has no path table
/// for is a gap in *our* matrix and no amount of user action would fix it.
/// Collapsing them would turn our own gap into the user's problem.
pub fn native_host_step(check: &NativeHostCheck) -> &'static str {
    let looked_at = check
        .manifests
        .iter()
        .filter(|entry| entry.state != HostManifestState::NoDiscoveryPath)
        .count();
    if looked_at == 0 {
        return "nothing_to_look_at";
    }
    if check
        .manifests
        .iter()
        .any(|entry| entry.state.is_registered())
    {
        "registered"
    } else {
        "none_registered"
    }
}

/// D8 JSON. Public so the shape can be asserted without running the whole
/// report — `doctor::run()` reads the real home directory, which a test must
/// not do.
///
/// # Three lists, because there are three answers
///
/// `unsupported` is the pairs this build has no path for, `detected` is the
/// browsers whose data directory is here, and `detected_not_registered` is the
/// intersection that a user can actually act on: *the browser is on this machine
/// and our host is not registered with it.* None of them is derived from the
/// others, and none is a substitute for the per-browser rows in `manifests`,
/// which is where `support` and `detected` are reported per pair.
///
/// The `registered` count deliberately excludes `no_discovery_path`: a browser we
/// do not look for cannot be counted as one we failed to find.
pub fn native_host_json(check: &NativeHostCheck) -> serde_json::Value {
    let ids = |predicate: &dyn Fn(&HostManifestCheck) -> bool| -> Vec<String> {
        check
            .manifests
            .iter()
            .filter(|entry| predicate(entry))
            .map(|entry| entry.browser.clone())
            .collect()
    };

    let manifests: Vec<serde_json::Value> = check
        .manifests
        .iter()
        .map(|entry| {
            let mut value = serde_json::json!({
                "browser": entry.browser,
                "support": entry.support.map(crate::nativehost::Support::id),
                "detected": entry.detected,
                "manifest": entry.manifest.as_ref().map(|path| path.display().to_string()),
                "registered": entry.state.is_registered(),
                "kind": entry.state.kind_label(),
            });
            match &entry.state {
                HostManifestState::Unreadable { error } | HostManifestState::Invalid { error } => {
                    value["error"] = serde_json::json!(error);
                }
                HostManifestState::PathMissing { path }
                | HostManifestState::PathNotExecutable { path }
                | HostManifestState::BuildArtifact { path }
                | HostManifestState::Ok { path } => {
                    value["path"] = serde_json::json!(path.display().to_string());
                }
                HostManifestState::NoDiscoveryPath | HostManifestState::NotRegistered => {}
            }
            value
        })
        .collect();

    let stage = {
        let mut value = serde_json::json!({"kind": check.stage.kind_label()});
        match &check.stage {
            StageConfigCheck::Present { path }
            | StageConfigCheck::Missing { path }
            | StageConfigCheck::NotADirectory { path } => {
                value["path"] = serde_json::json!(path.display().to_string());
            }
            StageConfigCheck::NotConfigured => {}
        }
        value
    };

    serde_json::json!({
        "checked": true,
        "step": native_host_step(check),
        "registered": check
            .manifests
            .iter()
            .filter(|entry| entry.state.is_registered())
            .count(),
        "supported": ids(&|entry| {
            entry.support == Some(crate::nativehost::Support::Supported)
        }),
        "unverified": ids(&|entry| {
            entry.support == Some(crate::nativehost::Support::Unverified)
        }),
        "unsupported": ids(&|entry| {
            entry.state == HostManifestState::NoDiscoveryPath
        }),
        "detected": ids(&|entry| entry.detected == Some(true)),
        "detected_not_registered": ids(&|entry| {
            entry.detected == Some(true) && entry.state == HostManifestState::NotRegistered
        }),
        "manifests": manifests,
        "stage": stage,
    })
}

// ---------------------------------------------------------------------------
// `doctor --json` serialisation — pure, no IO; the report is already built.
// ---------------------------------------------------------------------------

/// The full `doctor --json` object. Same data as [`print_report`], with every
/// count a tri-state ([`CountState`]) and every time a tri-state
/// ([`TimeState`]): nothing that is unknown is ever serialised as `0`/`null`.
pub fn report_to_json(r: &DoctorReport) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "command": "doctor",
        "scan_failed": r.scan_failed,
        "config_source": r.config_source.label(),
        // `null` for a healthy run. When it is a string, every empty collection
        // in this object means "did not look", and `not_checked` names them —
        // without it, `"probes": []` would read as a machine with no harness.
        "config_error": r.config_error,
        "not_checked": r.not_checked(),
        "claude": claude_json(&r.claude),
        "gemini": gemini_json(&r.gemini),
        "footprints": r.footprints.iter().map(footprint_json).collect::<Vec<_>>(),
        // ADR-053 D4: machine logs are a product of their own, so they get
        // their own object rather than a row in `footprints`. A count that was
        // not measured is a tri-state, never a 0.
        "machine_logs": machine_logs_json(&r.machine_logs),
        "other_present": r.other_present.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "risks": r.risks.iter().map(|text| risk_json(text)).collect::<Vec<_>>(),
        "reclaim": match &r.reclaim {
            Some(reclaim) => reclaim_json(reclaim),
            // Same idiom as `native_host` below: not "no repository", but "this
            // run did not look", which is a different finding.
            None => serde_json::json!({"checked": false}),
        },
        "cache": match &r.cache {
            Some(cache) => cache_json(cache),
            None => serde_json::json!({"checked": false}),
        },
        "body_cache": match &r.body_cache {
            Some(body_cache) => body_cache_json(body_cache),
            None => serde_json::json!({"checked": false}),
        },
        "fts_indexes": match &r.fts_indexes {
            Some(indexes) => serde_json::json!({
                "checked": true,
                "destinations": indexes.iter().map(|index| serde_json::json!({
                    "destination": index.destination,
                    "state": index.state,
                    "documents": index.documents,
                })).collect::<Vec<_>>(),
            }),
            None => serde_json::json!({"checked": false}),
        },
        "archive_gaps": r.archive_gaps.iter().map(archive_gap_json).collect::<Vec<_>>(),
        "probes": r.probes.iter().map(scanner::probe_json).collect::<Vec<_>>(),
        "destinations": r.destinations.iter().map(destination_probe_json).collect::<Vec<_>>(),
        "stage_duplicate_shards": match &r.stage_duplicate_shards {
            Some(check) => serde_json::json!({
                "checked": true,
                "state": check.state,
                "machines": check.machines.as_ref().map(|machines| machines.iter().map(|machine| serde_json::json!({
                    "machine": machine.machine,
                    "duplicate_sessions": machine.duplicate_sessions,
                    "duplicate_shards": machine.duplicate_shards,
                    "duplicate_bytes": machine.duplicate_bytes,
                })).collect::<Vec<_>>()),
                "why": check.why,
                "repair_command": check.repair_command,
            }),
            None => serde_json::json!({"checked": false}),
        },
        "keys": match &r.keys {
            Some(rows) => serde_json::json!({
                "checked": true,
                "copies": rows.iter().map(key_inventory_json).collect::<Vec<_>>(),
            }),
            // Not "there are no keys": this run could not read the config the
            // inventory is derived from, so it never looked.
            None => serde_json::json!({"checked": false}),
        },
        "native_host": match &r.native_host {
            Some(check) => native_host_json(check),
            // Not the same as "there is no registration": this run did not look.
            None => serde_json::json!({"checked": false}),
        },
    })
}

/// The `machine_logs` object of `doctor --json` (ADR-053 D4).
///
/// `present` rows carry counts only — never a prompt, never a source path.
/// `generations` and `lines` are [`CountState`]s: a stage that could not be
/// read makes them `unknown`, not `0`.
fn machine_logs_json(rows: &MachineLogRows) -> serde_json::Value {
    serde_json::json!({
        "present": rows
            .present
            .iter()
            .map(|log| serde_json::json!({
                "harness": log.harness,
                "log_id": log.log_id,
                "present": log.present,
                "generations": match log.generations {
                    Some(generations) => CountState::known(generations as u64),
                    None => CountState::unknown("the configured stage could not be measured"),
                },
                "lines": match log.lines {
                    Some(lines) => CountState::known(lines),
                    None => CountState::unknown("the configured stage could not be measured"),
                },
            }))
            .collect::<Vec<_>>(),
        "unlooked": CountState::known(rows.unlooked as u64),
        "indeterminate": CountState::known(rows.indeterminate as u64),
        "unlooked_reasons": rows.unlooked_reasons,
    })
}

/// Claude Code's retention verdict, tagged like every other tri-state in this
/// file: the four distinct D1 outcomes are four distinct `kind`s.
fn claude_layer_json(v: &ClaudeRetention) -> serde_json::Value {
    match v {
        ClaudeRetention::UnsetDefault => serde_json::json!({"kind": "unset_default"}),
        ClaudeRetention::Safe { days, source } => serde_json::json!({
            "kind": "safe",
            "days": days,
            "source": source.display().to_string(),
        }),
        ClaudeRetention::SmallValue { days, source } => serde_json::json!({
            "kind": "small_value",
            "days": days,
            "source": source.display().to_string(),
        }),
        ClaudeRetention::ParseFailed { path, error } => serde_json::json!({
            "kind": "parse_failed",
            "path": path.display().to_string(),
            "error": error,
        }),
    }
}

fn claude_json(check: &ClaudeCheck) -> serde_json::Value {
    serde_json::json!({
        "dangerous": check.verdict.is_dangerous(),
        "verdict": claude_layer_json(&check.verdict),
        "layers": check.layers.iter().map(|(path, v)| serde_json::json!({
            "path": path.display().to_string(),
            "verdict": claude_layer_json(v),
        })).collect::<Vec<_>>(),
    })
}

/// Gemini's retention: a known policy or an explicit unknown (a settings file
/// that exists but could not be read). An unreadable policy is never guessed
/// from the CLI default.
fn gemini_json(g: &GeminiRetention) -> serde_json::Value {
    if let Some(error) = &g.unreadable {
        return serde_json::json!({
            "kind": "unknown",
            "why": format!("config unreadable: {error}"),
        });
    }
    serde_json::json!({
        "kind": "known",
        "enabled": g.enabled,
        "max_age": g.max_age,
        "min_retention": g.min_retention,
        "dangerous": g.is_dangerous(),
    })
}

/// One footprint row. The three-state rule is the same as the human table:
/// a count that was not measured is `unknown` (installed store we could not
/// enumerate) or `not_applicable` (not installed on this machine).
fn footprint_json(f: &HarnessFootprint) -> serde_json::Value {
    serde_json::json!({
        "name": f.name,
        "installed": f.installed,
        "root": f.root.as_ref().map(|root| root.display().to_string()),
        "session_count": match f.session_count {
            Some(n) => CountState::known(n),
            None if f.installed => CountState::unknown("session count could not be enumerated"),
            None => CountState::not_applicable("not installed"),
        },
        "candidate_count": match f.candidate_count {
            Some(n) => CountState::known(n),
            None => CountState::not_applicable("no candidate count (not a single-file store)"),
        },
        "unreadable_count": unreadable_tri(f.session_count.is_some(), f.unreadable_count),
        "unreadable_entry_count": unreadable_tri(f.session_count.is_some(), f.unreadable_entry_count),
        "total_bytes": match f.total_bytes {
            Some(bytes) => CountState::known(bytes),
            None if f.installed => CountState::unknown("byte count could not be measured"),
            None => CountState::not_applicable("not installed"),
        },
        "earliest": system_time_state(f.earliest),
        "latest": system_time_state(f.latest),
        "compressed_count": CountState::known(f.compressed_count),
        "note": f.note,
    })
}

/// The unreadable/entry tally rule, shared with the human table: counted ->
/// known (including zero); enumerated but tally failed -> unknown; never
/// enumerated -> not_applicable.
fn unreadable_tri(enumerated: bool, value: Option<u64>) -> CountState {
    match (enumerated, value) {
        (_, Some(n)) => CountState::known(n),
        (true, None) => CountState::unknown("the unreadable count itself could not be counted"),
        (false, None) => CountState::not_applicable("this harness was not enumerated"),
    }
}

fn system_time_state(t: Option<SystemTime>) -> TimeState {
    match t {
        Some(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => TimeState::known(d.as_secs() as i64),
            Err(e) => TimeState::unknown(format!(
                "timestamp before 1970 ({} seconds)",
                e.duration().as_secs()
            )),
        },
        None => TimeState::unknown("no timestamp recorded"),
    }
}

/// The leading severity emoji is promoted to a structured `severity` so a
/// script can colour an alert without parsing the emoji. The raw sentence is
/// kept verbatim in `text`.
fn risk_json(text: &str) -> serde_json::Value {
    let severity = if text.starts_with("🔴🔴") {
        "critical"
    } else if text.starts_with("🔴") {
        "red"
    } else if text.starts_with("🟡") {
        "yellow"
    } else if text.starts_with("🟢") {
        "green"
    } else {
        "none"
    };
    serde_json::json!({ "severity": severity, "text": text })
}

fn reclaim_json(r: &ReclaimCheck) -> serde_json::Value {
    match r {
        ReclaimCheck::NoRepo { repo_root } => serde_json::json!({
            "kind": "no_repo",
            "repo_root": repo_root.display().to_string(),
        }),
        ReclaimCheck::NoKey { key_file, error } => serde_json::json!({
            "kind": "no_key",
            "key_file": key_file.display().to_string(),
            "error": error,
        }),
        ReclaimCheck::OpenFailed { repo_root, error } => serde_json::json!({
            "kind": "open_failed",
            "repo_root": repo_root.display().to_string(),
            "error": error,
        }),
        ReclaimCheck::Ok {
            packs_unref,
            size_unref,
            packs_repack,
            size_repack,
            append_only,
        } => serde_json::json!({
            "kind": "ok",
            "packs_unref": packs_unref,
            "size_unref": size_unref,
            "packs_repack": packs_repack,
            "size_repack": size_repack,
            "append_only": append_only,
            "has_garbage": r.has_garbage(),
        }),
    }
}

/// D6 JSON. The byte count is a tri-state like every other count: unknown when
/// the cache directory does not exist (never `0`), not_applicable when the
/// cache is deliberately disabled.
fn cache_json(c: &CacheCheck) -> serde_json::Value {
    match c {
        CacheCheck::Disabled { root, leftover } => serde_json::json!({
            "kind": "disabled",
            "root": root.display().to_string(),
            "total_bytes": match leftover {
                Some(u) => CountState::known(u.total_bytes),
                None => CountState::not_applicable(
                    "cache is disabled and no leftover cache could be measured",
                ),
            },
            "repo_dirs": leftover.as_ref().map(|u| u.repo_dirs),
        }),
        CacheCheck::NoCacheDir { root } => serde_json::json!({
            "kind": "no_cache_dir",
            "root": root.display().to_string(),
            "total_bytes": CountState::unknown("cache directory does not exist"),
        }),
        CacheCheck::Unreadable { root, error } => serde_json::json!({
            "kind": "unreadable",
            "root": root.display().to_string(),
            "total_bytes": CountState::unknown(error),
            "error": error,
            "next_step": "check that the configured rustic cache path is a readable directory, then run `chat-stasher doctor` again",
        }),
        CacheCheck::Unavailable { detail } => serde_json::json!({
            "kind": "unavailable",
            "total_bytes": CountState::unknown(detail),
            "next_step": "fix `rustic_cache_dir` in the config and run `chat-stasher doctor` again",
        }),
        CacheCheck::Ok { root, usage } => serde_json::json!({
            "kind": "ok",
            "root": root.display().to_string(),
            "total_bytes": CountState::known(usage.total_bytes),
            "repo_dirs": usage.repo_dirs,
            "note": "whole-machine rustic cache: every repository this machine has ever touched (including test repositories and other machines' archives), not just the configured repository",
        }),
    }
}

fn archive_gap_json(g: &scanner::ArchiveGap) -> serde_json::Value {
    serde_json::json!({
        "harness": g.harness_id,
        "display_name": g.display_name,
        "recognized_sessions": match g.recognized_sessions {
            Some(n) => CountState::known(n),
            None => CountState::unknown("could not be counted"),
        },
        "session_records": g.session_records,
    })
}

// ---------------------------------------------------------------------------
// Printing — never session contents, only ids/paths/counts/bytes/timestamps.
// ---------------------------------------------------------------------------

fn footprint_count_label(f: &HarnessFootprint) -> String {
    match f.session_count {
        Some(count) => count.to_string(),
        // Installed but the store is not enumerable (schema not recognised):
        // "unknown", never a fake zero.
        None if f.installed => "unknown".to_string(),
        None => "N/A".to_string(),
    }
}

fn footprint_count_detail(f: &HarnessFootprint) -> String {
    let mut parts: Vec<String> = Vec::new();
    if f.name == "cursor" {
        if let (Some(before), Some(after)) = (f.candidate_count, f.session_count) {
            parts.push(format!("before filter {before} / after filter {after}"));
        }
    }
    // B68: the number that used to be invisible. "Filtered out" and "could not
    // be read" are different answers to "where did my sessions go", so the
    // second one gets its own count instead of hiding inside the first.
    // Silent when zero: an all-good line is byte-for-byte what it was.
    match f.unreadable_count {
        Some(unreadable) if unreadable > 0 => parts.push(format!("{unreadable} unreadable")),
        // Counted, and it is zero: silence, byte-for-byte as before.
        Some(_) => {}
        // B90: this row *was* enumerated (`session_count` is `Some`) and only
        // the unreadable tally failed. Saying nothing here is byte-identical
        // to the counted-zero line above, i.e. it reads as "nothing missed" —
        // which is precisely the claim that was never earned. Rows that were
        // never enumerated at all keep quiet: `installed`/`session_count`
        // already say so, and repeating it would put unknown on every
        // not-installed harness of a healthy machine.
        None if f.session_count.is_some() => {
            parts.push("unreadable count unknown (it could not itself be counted)".to_string())
        }
        None => {}
    }
    if let Some(entries) = f.unreadable_entry_count.filter(|n| *n > 0) {
        parts.push(format!(
            "{entries} unreadable directory entries (session count unknown)"
        ));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("({})", parts.join(", "))
}

pub fn print_report(r: &DoctorReport) {
    eprintln!();
    eprintln!("doctor — “Is your harness silently deleting your data?”");
    eprintln!("      read-only probe; prints only paths / counts / bytes / timestamps, never session bodies.");
    // Only when there is no usable configuration. This line used to carry the
    // fallback labels (`defaults_after_parse_error`); it now carries
    // `unreadable`, and the label change is deliberate and visible: a wrapper
    // that greps `config_source=` still sees that the run had a config problem,
    // and a healthy report keeps the byte-for-byte output it had.
    if r.config_error.is_some() {
        eprintln!("config_source={}", r.config_source.label());
    }
    eprintln!();

    if let Some(error) = &r.config_error {
        // The one report that is deliberately partial, and says so before it
        // prints anything a reader could mistake for a finding. The two checks
        // below need no config, so they are still real; everything config-derived
        // is named here instead of being printed from defaults.
        // The error text already opens with the file and what is wrong with it
        // (`config file <path> exists but cannot be used: …`), so this adds the
        // alarm marker and nothing that repeats it.
        eprintln!("🔴 {error}");
        eprintln!();
        eprintln!("🔴 NOT CHECKED — these read the config, so this run has no answer for them:");
        for check in r.not_checked() {
            eprintln!("     · {check}");
        }
        if let Some(cache) = &r.cache {
            eprintln!("D6 · Local metadata cache occupancy");
            print_cache(cache);
        }
        eprintln!(
            "   An absent result above is NOT a count of zero and NOT a clean verdict: this run did \
             not look. Fix the config file and run `doctor` again."
        );
        eprintln!();
    }

    // D1
    eprintln!("D1 · Claude Code rotation settings");
    eprintln!(
        "  cleanupPeriodDays: {} ({})",
        r.claude.verdict.label(),
        match &r.claude.verdict {
            ClaudeRetention::UnsetDefault => "unset → default 30 days = dangerous".to_string(),
            ClaudeRetention::Safe { .. } =>
                "large value set, safe, but see D4's fail-destructive note".to_string(),
            ClaudeRetention::SmallValue { days, .. } => {
                format!("explicitly set {days} days, below the safe threshold = still rotating")
            }
            ClaudeRetention::ParseFailed { path, error } => {
                format!(
                    "🔴 file present but parse failed (this is the fail-destructive trigger): {} — {error}",
                    path.display()
                )
            }
        }
    );
    for (path, v) in &r.claude.layers {
        eprintln!("    {:<52} {}", path.display(), v.label());
    }
    eprintln!();

    // D2
    eprintln!("D2 · Gemini CLI retention policy");
    eprintln!("  sessionRetention: {}", r.gemini.summarize());
    let home = crate::config::home_dir();
    let present: Vec<String> = [".gemini/config.json", ".gemini/settings.json"]
        .iter()
        .filter(|p| home.join(p).is_file())
        .map(|p| home.join(p).display().to_string())
        .collect();
    if present.is_empty() {
        eprintln!(
            "  no config file exists → using the CLI built-in defaults (enabled=true, maxAge=30d)."
        );
    } else {
        eprintln!("  source file(s): {}", present.join(", "));
    }
    eprintln!();

    // D3
    // Checked before `scan_failed`, which `config_unreadable` also sets: the two
    // situations are different, and the line below is only true of one. A config
    // this run could not read means the registry was never opened — "unknown",
    // not "missing/unparseable" — and the note the `scan_failed` text points at
    // ("Refusing to scan with hardcoded roots") was never printed on this path
    // either. Reporting a registry fault here would answer a question nobody
    // asked with a cause that is not the user's (CLAUDE.md invariant 1). The
    // report ends here rather than falling through: every section after D3 in
    // the `scan_failed` branch is config-derived, and `not_checked` above has
    // already named all of it — an empty "D4 · Risk summary" heading under a
    // "did not look" banner reads as "no risks", which is the same mistake.
    if r.config_error.is_some() {
        eprintln!(
            "D3 · Coverage — NOT CHECKED: the config file could not be read, so this run never reached the path registry. Its state is unknown, not missing."
        );
        eprintln!();
        return;
    }

    if r.scan_failed {
        eprintln!("D3 · Coverage — 🔴 registry missing / unparseable, session coverage unknown.");
        eprintln!(
            "    refusing to fake a full scan with hardcoded paths (see “Refusing to scan with hardcoded roots” above in stderr)."
        );
        eprintln!("    listing only entries from independent read-only probes, unrelated to this registry:",);
        for f in &r.footprints {
            if f.name != "gemini" && f.name != "opencode" {
                continue;
            }
            if !f.installed {
                eprintln!(
                    "  {:<10} not installed ({})",
                    f.name,
                    footprint_root_label(f)
                );
                continue;
            }
            let count = footprint_count_label(f);
            let count_detail = footprint_count_detail(f);
            eprintln!(
                "  {:<10} sessions {:<6}{} · {} · {}",
                f.name,
                count,
                count_detail,
                footprint_bytes_label(f),
                footprint_times(f)
            );
            if !f.note.is_empty() {
                eprintln!("             ({})", f.note);
            }
        }
        eprintln!();
        eprintln!("D4 · Risk summary — so what happens + when");
        for (i, risk) in r.risks.iter().enumerate() {
            eprintln!("  {}. {risk}", i + 1);
        }
        eprintln!();
        // These three are `Option` only because a report whose config could not
        // be read has no repository or cache path to measure. That report has
        // returned above, so a `None` here would mean a caller assembled a report
        // by hand: printing the section without an answer would be worse than
        // leaving it out, and `not_checked` is what names it.
        if let Some(reclaim) = &r.reclaim {
            eprintln!("D5 · How much reclaimable garbage is in the repository?");
            eprintln!("     `prune_plan` computes without deleting — doctor never runs prune, nor touches append_only.");
            print_reclaim(reclaim);
            eprintln!();
        }
        if let Some(cache) = &r.cache {
            eprintln!("D6 · Local metadata cache occupancy");
            print_cache(cache);
            eprintln!();
        }
        if let Some(body_cache) = &r.body_cache {
            eprintln!("D9 · Body cache (conversation bodies, ADR-034)");
            print_body_cache(body_cache);
            eprintln!();
        }
        print_fts_indexes(&r.fts_indexes);
        print_destinations(&r.destinations);
        print_stage_duplicates(&r.stage_duplicate_shards);
        // D8 does not depend on the scan at all — it reads the browser
        // manifests and the config — so it is reported on this path too.
        if let Some(check) = &r.native_host {
            eprintln!();
            print_native_host(check);
        }
        return;
    }

    // W285 §6: the denominator is what was looked at, and the never-probed
    // remainder is named beside it rather than folded into a fraction that
    // reads as a measurement over the whole registry.
    let coverage = probe_coverage(&r.probes);
    eprintln!(
        "D3 · Coverage — {hit}/{probed} probed harnesses hit on this machine · {never} not probed (registry v1 driven); rotation analysis subjects:",
        hit = coverage.hit,
        probed = coverage.probed,
        never = coverage.never_probed(),
    );
    for f in &r.footprints {
        if !f.installed {
            eprintln!(
                "  {:<10} not installed ({})",
                f.name,
                footprint_root_label(f)
            );
            continue;
        }
        let count = footprint_count_label(f);
        let count_detail = footprint_count_detail(f);
        eprintln!(
            "  {:<10} sessions {:<6}{} · {} · {}",
            f.name,
            count,
            count_detail,
            footprint_bytes_label(f),
            footprint_times(f)
        );
        if !f.note.is_empty() {
            eprintln!("             ({})", f.note);
        }
    }
    print_machine_logs(&r.machine_logs);
    if !r.other_present.is_empty() {
        #[allow(
            clippy::unwrap_used,
            reason = "Every element of `other_present` is `home.join(d)` for a non-empty literal `d` from OTHER_HARNESS_DIRS (doctor.rs:43), so the final component is that literal and `file_name()` is always `Some`. Falling back to a placeholder here would print a directory that is not the one probed."
        )]
        let others: Vec<String> = r
            .other_present
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        eprintln!(
            "  installed but out of scope for this command (probed only, no rotation analysis): {}",
            others.join(", ")
        );
    }
    print_archive_gaps(&r.archive_gaps);
    print_probes(&r.probes);
    eprintln!();

    // D4
    eprintln!("D4 · Risk summary — so what happens + when");
    if r.risks.is_empty() {
        eprintln!("  (nothing to synthesise)");
    }
    for (i, risk) in r.risks.iter().enumerate() {
        eprintln!("  {}. {risk}", i + 1);
    }
    eprintln!();

    // D5 — reclaimable garbage in the archive repository (prune_plan, read-only).
    // `Option` because a report with no usable config has no repository path to
    // measure; see the note on the same pair on the scan-failed path above.
    if let Some(reclaim) = &r.reclaim {
        eprintln!("D5 · How much reclaimable garbage is in the repository?");
        eprintln!("     `prune_plan` computes without deleting — doctor never runs prune, nor touches append_only.");
        print_reclaim(reclaim);
        eprintln!();
    }

    // D6 — how much the local metadata cache actually occupies
    if let Some(cache) = &r.cache {
        eprintln!("D6 · Local metadata cache occupancy");
        print_cache(cache);
        eprintln!();
    }

    // D9 — how much the body cache occupies, against the quota ADR-034 gives it.
    // `Option` for the same reason as D5/D6 above: no config, no root to measure.
    if let Some(body_cache) = &r.body_cache {
        eprintln!("D9 · Body cache (conversation bodies, ADR-034)");
        print_body_cache(body_cache);
        eprintln!();
    }
    print_fts_indexes(&r.fts_indexes);

    // D7 — can each declared destination actually be reached? (ADR-023)
    print_destinations(&r.destinations);
    print_stage_duplicates(&r.stage_duplicate_shards);

    // Every archive copy has its own key, so a backup that names only the
    // local one leaves the off-site copies unreadable (W281 BUG-2). Reported
    // per copy rather than as one "key" for that reason.
    print_keys(&r.keys);

    // D8 — is the browser-side host actually usable?
    if let Some(check) = &r.native_host {
        eprintln!();
        print_native_host(check);
    }
}

fn print_stage_duplicates(check: &Option<StageDuplicateCheck>) {
    let Some(check) = check else { return };
    eprintln!("D11 · Identical duplicate shards in the configured stage");
    match check.state {
        "clean" => eprintln!("  no identical duplicate shards found"),
        "duplicates_found" => {
            if let Some(machines) = &check.machines {
                for machine in machines.iter().filter(|row| row.duplicate_sessions > 0) {
                    eprintln!(
                        "  {} · {} session(s), {} duplicate shard(s), {} bytes",
                        machine.machine,
                        machine.duplicate_sessions,
                        machine.duplicate_shards,
                        machine.duplicate_bytes
                    );
                }
            }
            eprintln!("  inspect the destination with `{}`", check.repair_command);
            eprintln!("  no shards or snapshots were removed");
        }
        _ => {
            eprintln!("  unknown");
            if let Some(why) = &check.why {
                eprintln!("  reason: {why}");
            }
            eprintln!("  inspect archived data with `{}`", check.repair_command);
        }
    }
    eprintln!();
}

/// Which key files this machine holds, per archive copy, and which of them the
/// user has said they keep a copy of.
///
/// Silent for `None` (a report whose config could not be read): the inventory is
/// derived from the config, so there is nothing to say rather than nothing to
/// list. `not_checked` already names that check as skipped.
fn print_keys(keys: &Option<Vec<KeyInventoryRow>>) {
    let Some(rows) = keys else { return };
    eprintln!();
    eprintln!("Keys · one file per archive copy — each opens only its own copy");
    eprintln!("     A second machine reads a copy with that copy's key. Back up every one.");
    for row in rows {
        // `present`/`not on this machine` rather than yes/no: the file belongs to
        // a copy that may live elsewhere, so its absence here is a fact about
        // *this* machine and not a claim that the key was never created.
        let where_ = if row.exists {
            "present on this machine"
        } else {
            "not on this machine"
        };
        // Three states, because the file's comparison against the recorded
        // declaration has three answers and two of them must not be worded as
        // each other: a key that exists but cannot be read is an *unknown*,
        // not a "NOT declared" (which says the bytes were seen and did not
        // match), and never a "declared saved".
        let declared = match row.declared_saved {
            crate::keydecl::DeclaredFor::Declared => "declared saved",
            crate::keydecl::DeclaredFor::NotDeclared => "NOT declared saved",
            crate::keydecl::DeclaredFor::Unreadable => {
                "unreadable — unknown whether the declaration covers it"
            }
        };
        let label = match &row.name {
            Some(name) => format!("{name} (destination)"),
            None => "local (this machine's archive)".to_string(),
        };
        eprintln!(
            "  {:<28} {} — {where_}; {declared}",
            label,
            row.path.display()
        );
    }
    eprintln!(
        "     A declaration is a statement, not a check: doctor cannot see your backup, so \
         `declared saved` never means the copy exists."
    );
}

fn print_fts_indexes(indexes: &Option<Vec<FtsIndexCheck>>) {
    let Some(indexes) = indexes else { return };
    eprintln!("D10 · Full-text indexes (local, per destination)");
    if indexes.is_empty() {
        eprintln!("  no destination configured");
    }
    for index in indexes {
        match (index.state, index.documents) {
            ("valid", Some(count)) => {
                eprintln!("  {} · valid · {count} document(s)", index.destination)
            }
            (state, _) => eprintln!("  {} · {state}", index.destination),
        }
    }
    eprintln!();
}

/// The one line that says what `detected` is and is not, printed before the
/// rows. Shared with the setup wizard for the same reason the rows are: two
/// surfaces describing one machine must not describe it in two vocabularies.
fn host_header_line() -> String {
    "  a browser's data directory being present is not the extension being \
     installed: it outlives an uninstall and is shared by every profile of that \
     browser, so `detected yes` with `not registered` means \"this browser is here \
     and the host is not\""
        .to_string()
}

/// One browser's row of the inventory: `browser  tier  detected  state`.
///
/// The browser column is 14 wide because `chrome-canary` is 13 characters: at
/// 12 it overflowed by one and pushed its tier and presence out of line, which
/// is the kind of misalignment that makes a reader trust the wrong column.
///
/// Public and pure so the setup wizard can render the identical row instead of
/// phrasing the same facts a second way.
pub fn host_row_line(entry: &HostManifestCheck) -> String {
    let support = entry.support.map(|tier| tier.id()).unwrap_or("-");
    let detected = match entry.detected {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    };
    let manifest = entry
        .manifest
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(this build has no path for this pair)".to_string());

    match &entry.state {
        HostManifestState::NoDiscoveryPath => format!(
            "  {:<14} {:<10} {:<7} no discovery path on this platform — not looked at",
            entry.browser, support, detected
        ),
        HostManifestState::NotRegistered => format!(
            "  {:<14} {:<10} {:<7} not registered (looked at {})",
            entry.browser, support, detected, manifest
        ),
        HostManifestState::Ok { path } => format!(
            "  {:<14} {:<10} {:<7} ok — {}",
            entry.browser,
            support,
            detected,
            path.display()
        ),
        HostManifestState::BuildArtifact { path } => format!(
            "  {:<14} {:<10} {:<7} ⚠ {} is a build artifact under target/ — it stops existing after `cargo clean`, and the browser then reports the host as missing",
            entry.browser,
            support,
            detected,
            path.display()
        ),
        HostManifestState::PathMissing { path } => format!(
            "  {:<14} {:<10} {:<7} 🔴 {} — registered, but that path does not exist",
            entry.browser,
            support,
            detected,
            path.display()
        ),
        HostManifestState::PathNotExecutable { path } => format!(
            "  {:<14} {:<10} {:<7} 🔴 {} — registered, but not executable",
            entry.browser,
            support,
            detected,
            path.display()
        ),
        HostManifestState::Unreadable { error } => format!(
            "  {:<14} {:<10} {:<7} manifest {} could not be read: {error}",
            entry.browser, support, detected, manifest
        ),
        HostManifestState::Invalid { error } => format!(
            "  {:<14} {:<10} {:<7} manifest {} is not a usable host manifest: {error}",
            entry.browser, support, detected, manifest
        ),
    }
}

/// The inventory plus its counts, with no `D8` header and no stage block — the
/// part `doctor` and the setup wizard report identically.
///
/// Read-only by construction: it formats a [`NativeHostCheck`] that was already
/// gathered, and touches nothing.
pub fn native_host_lines(check: &NativeHostCheck) -> Vec<String> {
    let mut lines = Vec::with_capacity(check.manifests.len() + 4);
    lines.push(host_header_line());
    for entry in &check.manifests {
        lines.push(host_row_line(entry));
    }

    let registered = check
        .manifests
        .iter()
        .filter(|entry| entry.state.is_registered())
        .count();
    let looked_at = check
        .manifests
        .iter()
        .filter(|entry| entry.state != HostManifestState::NoDiscoveryPath)
        .count();
    lines.push(format!(
        "  registered at a path this build looks at: {registered}/{looked_at}"
    ));

    let unsupported: Vec<&str> = check
        .manifests
        .iter()
        .filter(|entry| entry.state == HostManifestState::NoDiscoveryPath)
        .map(|entry| entry.browser.as_str())
        .collect();
    if !unsupported.is_empty() {
        lines.push(format!(
            "  outside this build's path table on {}: {} (no browser was looked for, so \
             absence here proves nothing)",
            crate::nativehost::Platform::current().id(),
            unsupported.join(", ")
        ));
    }

    let detected_not_registered: Vec<&str> = check
        .manifests
        .iter()
        .filter(|entry| {
            entry.state == HostManifestState::NotRegistered && entry.detected == Some(true)
        })
        .map(|entry| entry.browser.as_str())
        .collect();
    if !detected_not_registered.is_empty() {
        lines.push(format!(
            "  detected but not registered: {} — `chat-stasher install-native-host` \
             registers every browser found on this machine",
            detected_not_registered.join(", ")
        ));
    }
    lines
}

/// The stage verdict, one line. Shared for the same reason as the rows.
pub fn native_host_stage_line(check: &NativeHostCheck) -> String {
    match &check.stage {
        StageConfigCheck::NotConfigured => format!(
            "  stage: no `[native_host] stage` in {} — every delivery answers nack config",
            crate::config::config_path().display()
        ),
        StageConfigCheck::Present { path } => {
            format!("  stage: {} (present)", path.display())
        }
        StageConfigCheck::Missing { path } => format!(
            "  stage: 🔴 {} — configured but not on disk; every delivery answers nack stage-unavailable",
            path.display()
        ),
        StageConfigCheck::NotADirectory { path } => {
            format!("  stage: 🔴 {} — configured but is not a directory", path.display())
        }
    }
}

/// D8 printing. Read-only findings about the browser registration and the stage
/// the host would write to. No count here is a fallback: a browser with no
/// manifest says so, and "the config could not be read" is never printed as
/// "no stage is set".
fn print_native_host(check: &NativeHostCheck) {
    eprintln!("D8 · Native Messaging host (protocol v1)");
    eprintln!("     read-only: doctor never writes a manifest, and never creates the stage.");
    for line in native_host_lines(check) {
        eprintln!("{line}");
    }
    eprintln!("{}", native_host_stage_line(check));
}

/// D7 printing — shared by the normal path and the scan-failed early return.
///
/// Silent when no destination was probed, because an empty list here means
/// "nobody asked" rather than "there are none".
fn print_destinations(probes: &[DestinationProbe]) {
    if probes.is_empty() {
        return;
    }
    eprintln!("D7 · Declared destinations — one read-only connection each (ADR-023)");
    for p in probes {
        match &p.outcome {
            DestinationOutcome::Reached { repository_exists } => {
                eprintln!(
                    "  {:<12} {} — reachable; repository {}",
                    p.name,
                    p.repo_root,
                    if *repository_exists {
                        "present"
                    } else {
                        "NOT there yet (nothing has been pushed to it)"
                    }
                );
                if let Some(freshness) = &p.activity_index {
                    print_activity_index(freshness);
                }
            }
            DestinationOutcome::Unreachable { kind, detail } => {
                eprintln!(
                    "  {:<12} {} — NOT REACHED ({})",
                    p.name,
                    p.repo_root,
                    kind.map(|k| k.label()).unwrap_or("unclassified")
                );
                for line in detail.lines() {
                    eprintln!("               {line}");
                }
                eprintln!(
                    "               Whether this destination still holds the archive is UNKNOWN, \
                     not empty."
                );
            }
            DestinationOutcome::NotConfigured { detail } => {
                eprintln!(
                    "  {:<12} {} — NOTHING WAS ATTEMPTED (not configured)",
                    p.name, p.repo_root
                );
                for line in detail.lines() {
                    eprintln!("               {line}");
                }
            }
        }
    }
    eprintln!();
}

/// W158 printing — how fresh this destination's activity indexes are, and the
/// exact command that repairs the ones written by an older CLI.
///
/// Only shown once there is something to say: a destination whose indexes are
/// current gets no line, and one that could not be read says so rather than
/// staying silent (silence would read as "fine").
fn print_activity_index(freshness: &ActivityIndexFreshness) {
    match freshness {
        ActivityIndexFreshness::Current { machines } => {
            eprintln!(
                "               activity index: current on all {machines} machine(s) — every \
                 archived index was written by this build or newer"
            );
        }
        ActivityIndexFreshness::Behind { stale, machines } => {
            eprintln!(
                "               activity index: ⚠ {} of {machines} machine(s) carry an index \
                 written by an older chat-stasher — the times those machines show are that \
                 older build's reading, not this one's:",
                stale.len()
            );
            for s in stale {
                eprintln!(
                    "                 • {} — writer version {}",
                    s.machine,
                    s.recorded_version
                        .as_deref()
                        .unwrap_or("not recorded (written by ≤0.3.0)")
                );
                eprintln!("                   rebuild it with: {}", s.repair_command);
            }
            // The repair command above can succeed without this warning ever
            // clearing: a machine that is not this one has its index rebuilt
            // read-only into a local derived file, because a partition's index
            // is written only by the machine that owns it (ADR-017). Say which
            // is which, rather than let the user run it twice and conclude the
            // command is broken.
            eprintln!(
                "                 (a partition's index is repaired in the archive only for the \
                 machine that owns it; another machine's is rebuilt read-only, into a local \
                 derived index)"
            );
        }
        ActivityIndexFreshness::Unknown { detail } => {
            eprintln!("               activity index: UNKNOWN — {detail}");
        }
    }
}

/// D6 printing — shared by the normal path and the scan-failed early return.
fn print_cache(c: &CacheCheck) {
    match c {
        CacheCheck::Disabled { root, leftover } => {
            eprintln!(
                "  cache is disabled (rustic_no_cache = true) — no metadata cache is written."
            );
            eprintln!(
                "  cache root (may still hold data from before the switch): {}",
                root.display()
            );
            match leftover {
                Some(u) => eprintln!(
                    "  leftover on disk: {} ({} B) across {} repository cache dir(s) — written before the switch, not by it",
                    fmt_bytes(u.total_bytes),
                    u.total_bytes,
                    u.repo_dirs
                ),
                None => eprintln!("  leftover on disk: unknown (cache root could not be measured)"),
            }
        }
        CacheCheck::NoCacheDir { root } => {
            eprintln!(
                "  cache root: {} — does not exist yet; local cache occupancy is unknown (never measured), not 0.",
                root.display()
            );
        }
        CacheCheck::Unreadable { root, error } => {
            eprintln!(
                "  cache root: {} — could not be measured: {error}; check that this is a readable directory, then run `chat-stasher doctor` again.",
                root.display()
            );
        }
        CacheCheck::Unavailable { detail } => {
            eprintln!("  cache-path check: unavailable — {detail}");
        }
        CacheCheck::Ok { root, usage } => {
            eprintln!("  cache root: {}", root.display());
            eprintln!(
                "  disk used  : {} ({} B) across {} repository cache dir(s)",
                fmt_bytes(usage.disk_bytes),
                usage.disk_bytes,
                usage.repo_dirs
            );
            eprintln!(
                "  content    : {} ({} B) — logical size of the files themselves",
                fmt_bytes(usage.total_bytes),
                usage.total_bytes
            );
            if usage.disk_bytes > usage.total_bytes.saturating_mul(3) / 2 {
                eprintln!(
                    "  note: disk used is well above content size because this cache is many tiny files and each one rounds up to a whole filesystem block. The first number is what reclaiming it would give you back."
                );
            }
            eprintln!(
                "  note: this is the whole machine's rustic cache — it includes every repository this machine has ever touched,"
            );
            eprintln!(
                "        including test repositories and other machines' archives, not just the configured repository's metadata."
            );
        }
    }
}

/// D5 printing — shared by the normal path and the scan-failed early return.
fn print_reclaim(r: &ReclaimCheck) {
    match r {
        ReclaimCheck::NoRepo { repo_root } => {
            eprintln!(
                "  (skipped) no repository directory: {} — no repo means no garbage, and nothing to diagnose.",
                repo_root.display()
            );
        }
        ReclaimCheck::NoKey { key_file, error } => {
            eprintln!(
                "  (skipped) repository directory present, but the masterkey cannot be read ({}): {error} — \
                 nothing can be planned if it cannot be opened; first confirm the key file is not missing.",
                key_file.display()
            );
        }
        ReclaimCheck::OpenFailed { repo_root, error } => {
            eprintln!(
                "  (skipped) repository cannot be opened / plan cannot be computed: {} — {error}",
                repo_root.display()
            );
        }
        ReclaimCheck::Ok {
            packs_unref,
            size_unref,
            packs_repack,
            size_repack,
            append_only,
        } => {
            eprintln!(
                "  unreferenced packs   : {packs_unref} · {} (`packs_unref`/`size_unref`)",
                fmt_bytes(*size_unref)
            );
            eprintln!(
                "  repack candidates   : {packs_repack} packs · {} (`packs.repack`/`size.repack`)",
                fmt_bytes(*size_repack)
            );
            if *packs_unref > 0 || *size_unref > 0 || *packs_repack > 0 || *size_repack > 0 {
                eprintln!("  🔴 this garbage cannot be cleaned right now — this repository is append_only ({append_only}).");
                eprintln!(
                    "     `prune`/`repair`/`rewrite --forget` are all blocked by append_only (source `commands/prune.rs`)."
                );
                eprintln!(
                    "     standard clean-up sequence: temporarily disable append_only → run `rustic prune --instant-delete` → re-enable append_only."
                );
                eprintln!(
                    "     ⚠️ must use `--instant-delete`: plain `prune`'s default `--keep-delete 23h` only marks packs as pending delete,"
                );
                eprintln!(
                    "        and actually deletes only after the 23-hour grace period — in practice even with append_only off it stalls at nothing to do!, and the garbage never leaves."
                );
                eprintln!(
                    "     ⚠️ cost: `--instant-delete` skips the 23-hour grace period — once deleted it is gone, with no undo window; before running, confirm the two numbers above really are garbage."
                );
                eprintln!(
                    "     ⚠️ this requires temporarily disabling append_only — that is your safety setting: between disabling and re-enabling, the repository loses its “no-accidental-delete” safety net,"
                );
                eprintln!("        operate only within this window, take a backup first, and re-enable immediately when done.");
                eprintln!(
                    "     doctor is a read-only diagnostic: it only reports numbers and steps, and will never run prune or toggle append_only for you."
                );
            } else {
                eprintln!("  ✅ no reclaimable garbage — nothing to clean.");
                if *append_only {
                    eprintln!(
                        "     (append_only=true; even if there were garbage it would only be blocked, never silently deleted.)"
                    );
                }
            }
        }
    }
    eprintln!();
}

fn print_archive_gaps(gaps: &[scanner::ArchiveGap]) {
    if gaps.is_empty() {
        return;
    }
    eprintln!(
        "  ⚠ non-archivable sessions: the following harnesses recognised sessions but produced no SessionRecord; collect will not archive them for now."
    );
    for gap in gaps {
        eprintln!("{}", scanner::format_archive_gap(gap));
    }
    eprintln!(
        "  advice: do not treat scanner records as the total of recognised sessions; wait until the corresponding harness produces SessionRecords before running collect."
    );
}

/// D3 supplement — the registry-driven probe table: every harness the
/// registry listed for this platform, whether it was scanned / exists here,
/// and which cells were flagged low-confidence or skipped (`unascertained`, other
/// platform, template not statically resolvable).
fn print_probes(probes: &[scanner::HarnessProbe]) {
    if probes.is_empty() {
        eprintln!(
            "  [registry] no probes — the scan did not run, or there are no candidate harnesses."
        );
        return;
    }
    let platform = scanner::current_platform();
    // W285 §6: same split as the D3 header above — the denominator is what was
    // looked at, and the never-probed remainder is named rather than folded
    // into a fraction that looks like a measurement over the whole registry.
    let coverage = probe_coverage(probes);
    eprintln!(
        "  [registry] driven table — platform={platform} · {hit}/{probed} probed harnesses hit / scanned successfully · {never} not probed",
        hit = coverage.hit,
        probed = coverage.probed,
        never = coverage.never_probed(),
    );
    for p in probes {
        let mark = match p.state {
            scanner::ProbeState::Scanned => "scanned     ",
            scanner::ProbeState::FileTarget => "single-file ",
            scanner::ProbeState::Missing => "missing     ",
            scanner::ProbeState::Indeterminate => "indeterminate",
            scanner::ProbeState::SkipUnascertained => "skip(uncertain) ",
            scanner::ProbeState::SkipWrongPlatform => "cross-platform",
            scanner::ProbeState::SkipUnresolvable => "skip(template)  ",
        };
        let conf = if p.low_confidence_p() {
            format!("[low-confidence] {}", p.confidence.label())
        } else {
            format!("[{}]", p.confidence.label())
        };
        let root = p
            .root
            .as_ref()
            .map(|r| r.display().to_string())
            .unwrap_or_else(|| "-".to_string());
        let bytes = match p.state {
            scanner::ProbeState::FileTarget => format!(
                " bytes={}",
                p.bytes
                    .map(fmt_bytes)
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            _ => String::new(),
        };
        // Session count, three-state (same vocabulary as other tables in this file):
        //   number  —— enumeration succeeded, this is the count
        //   unknown —— reason to believe it may exist, but could not be enumerated this time
        //   N/A     —— not applicable on this machine (registry has no cell for this platform)
        // B82: previously printed "0" across the board except for Scanned/FileTarget. skip(uncertain),
        // skip(template), and could-not-determine are all "I didn't look"; printing 0 equates not-checked
        // with checked-and-empty — exactly the falsehood eliminated here. "missing" retains 0 because
        // it was genuinely checked: the path does not exist.
        let count = match p.state {
            scanner::ProbeState::FileTarget => match p.record_count {
                Some(c) => c.to_string(),
                None => "unknown".to_string(),
            },
            // B90: `unwrap_or(0)` here is unreachable today (a Scanned probe
            // always carries a count) — which is exactly why it was worth
            // removing: it is a fallback that would print the B82 lie again
            // the moment the invariant moves. Same shape as FileTarget above.
            scanner::ProbeState::Scanned => match p.record_count {
                Some(c) => c.to_string(),
                None => "unknown".to_string(),
            },
            scanner::ProbeState::Missing => "0".to_string(),
            scanner::ProbeState::SkipWrongPlatform => "N/A".to_string(),
            scanner::ProbeState::Indeterminate
            | scanner::ProbeState::SkipUnascertained
            | scanner::ProbeState::SkipUnresolvable => "unknown".to_string(),
        };
        let extra = if p.note.is_empty() {
            String::new()
        } else {
            format!("  ({})", p.note)
        };
        eprintln!(
            "    {mark} {:<16} {conf:<26} sessions={:<4}{bytes:<0} {root}{extra}",
            p.display_name, count
        );
    }
    let flagged = probes
        .iter()
        .filter(|p| p.low_confidence_p())
        .map(|p| p.display_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if !flagged.is_empty() {
        eprintln!("    low-confidence (community claims only, scanned but cannot be treated as verified): {flagged}");
    }
}

// ---------------------------------------------------------------------------
// Tests — fake home dirs: normal, missing, and broken JSON.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// D6 exists to answer "how much disk is this costing me". A rustic cache
    /// is tens of thousands of tiny files, so the logical byte sum is far
    /// below what is actually allocated; reporting only the logical figure
    /// answers a different question than the one asked.
    #[test]
    fn cache_usage_counts_directories_and_logical_bytes() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        for i in 0..8u32 {
            let d = root.join(format!("repo-{i}"));
            std::fs::create_dir_all(&d).expect("mkdir");
            std::fs::write(d.join("f"), b"x").expect("write");
        }
        let usage = measure_cache_dir(root).expect("measure");
        assert_eq!(usage.repo_dirs, 8);
        assert_eq!(usage.total_bytes, 8, "logical sum is 8 one-byte files");
    }

    /// The allocation figure, asserted where the platform reports one.
    ///
    /// Unix-only: `disk_bytes > total_bytes` needs `metadata.blocks()` — the
    /// POSIX stat field that is the *only* way to learn a file's allocation.
    /// Windows does not expose per-file allocation through `std::fs::Metadata`,
    /// so `measure_cache_dir` falls back to `disk_bytes == total_bytes` there
    /// and the inequality cannot hold. The property is platform-shaped, not an
    /// omission: the cross-platform half lives in
    /// `cache_usage_counts_directories_and_logical_bytes`, and this test pins
    /// the allocation half on the platform that reports it.
    #[cfg(unix)]
    #[test]
    fn cache_usage_reports_allocation_not_just_logical_size() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        // 8 files of 1 byte each: 8 logical bytes, but each occupies a whole
        // filesystem block.
        for i in 0..8u32 {
            let d = root.join(format!("repo-{i}"));
            std::fs::create_dir_all(&d).expect("mkdir");
            std::fs::write(d.join("f"), b"x").expect("write");
        }
        let usage = measure_cache_dir(root).expect("measure");
        assert_eq!(usage.repo_dirs, 8);
        assert_eq!(usage.total_bytes, 8, "logical sum is 8 one-byte files");
        assert!(
            usage.disk_bytes > usage.total_bytes,
            "allocation must exceed the logical sum for sub-block files; got disk={} logical={}",
            usage.disk_bytes,
            usage.total_bytes
        );
    }

    use super::*;

    #[test]
    fn configured_stage_duplicate_scan_counts_and_names_the_repair_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stage = tmp.path().join("stage");
        let dir = stage
            .join(crate::store::SESSIONS_DIR)
            .join("machine-one")
            .join("session-one")
            .join("000");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("000001.jsonl"), b"synthetic\n").unwrap();
        fs::write(dir.join("000002.jsonl"), b"synthetic\n").unwrap();
        let mut config = Config::default();
        config
            .native_host
            .get_or_insert_with(Default::default)
            .stage = Some(stage.to_string_lossy().into_owned());

        let check = inspect_stage_duplicates(&config);
        assert_eq!(check.state, "duplicates_found");
        let machine = &check.machines.as_ref().unwrap()[0];
        assert_eq!(machine.duplicate_sessions, 1);
        assert_eq!(machine.duplicate_shards, 1);
        assert_eq!(machine.duplicate_bytes, b"synthetic\n".len() as u64);
        assert!(check
            .repair_command
            .starts_with("chat-stasher repair-duplicates"));
    }

    fn write(path: &Path, content: &str) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    /// Normal: valid JSON + large cleanupPeriodDays.
    #[test]
    fn claude_large_value_is_safe() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".claude/settings.json"),
            r#"{ "cleanupPeriodDays": 99999, "permissions": {"deny":["TodoWrite"]} }"#,
        );
        let check = inspect_claude_settings(dir.path());
        assert_eq!(
            check.verdict,
            ClaudeRetention::Safe {
                days: 99999,
                source: dir.path().join(".claude/settings.json")
            }
        );
        assert!(!check.verdict.is_dangerous());
    }

    /// Missing: entire settings tree absent → UnsetDefault (not "0", but "default 30 days").
    #[test]
    fn claude_missing_settings_is_unset_default() {
        let dir = tempfile::tempdir().unwrap();
        let check = inspect_claude_settings(dir.path());
        assert_eq!(check.verdict, ClaudeRetention::UnsetDefault);
        assert!(check.verdict.is_dangerous());
    }

    /// Broken JSON: file exists but parse fails → ParseFailed (fail-destructive trigger condition).
    #[test]
    fn claude_broken_json_is_parse_failed() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".claude/settings.json"),
            r#"{ "cleanupPeriodDays": 99999, "#,
        );
        let check = inspect_claude_settings(dir.path());
        assert!(matches!(check.verdict, ClaudeRetention::ParseFailed { .. }));
        assert!(check.verdict.is_dangerous());
        assert!(!check.layers.is_empty());
    }

    /// Broken JSON takes precedence over valid values in same layer: another valid layer cannot rescue parse failure.
    #[test]
    fn broken_json_outranks_valid_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".claude/settings.json"),
            r#"{ "cleanupPeriodDays": 99999, "#,
        );
        write(
            &dir.path().join(".claude/settings.local.json"),
            r#"{ "cleanupPeriodDays": 99999 }"#,
        );
        let check = inspect_claude_settings(dir.path());
        assert!(matches!(check.verdict, ClaudeRetention::ParseFailed { .. }));
    }

    /// Gemini: empty config (model only) → default 30 days → dangerous.
    #[test]
    fn gemini_unset_retention_is_default_dangerous() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".gemini/config.json"),
            r#"{ "model": "gemini-3.1-pro-preview" }"#,
        );
        let r = inspect_gemini_settings(dir.path());
        assert!(r.enabled);
        assert_eq!(r.max_age, "30d");
        assert!(r.is_dangerous());
    }

    /// Gemini: explicit enabled=false → safe.
    #[test]
    fn gemini_explicit_disable_is_safe() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join(".gemini/config.json"),
            r#"{ "sessionRetention": { "enabled": false, "maxAge": "30d" } }"#,
        );
        let r = inspect_gemini_settings(dir.path());
        assert!(!r.enabled);
        assert!(!r.is_dangerous());
    }

    /// Duration parser used by Gemini summary.
    #[test]
    fn duration_days_parsing() {
        assert_eq!(parse_duration_days("30d"), Some(30.0));
        assert_eq!(parse_duration_days("12h"), Some(0.5));
        assert_eq!(parse_duration_days("45m"), Some(45.0 / 1440.0));
        assert_eq!(parse_duration_days("99999"), Some(99999.0));
        assert_eq!(parse_duration_days("garbage"), None);
    }

    /// Formatter sanity: a known epoch → known date.
    #[test]
    fn date_formatting() {
        let t = UNIX_EPOCH + std::time::Duration::from_secs(1752105600); // 2025-07-10
        assert_eq!(format_date(t), "2025-07-10");
    }

    /// W158 — the JSON shape a script reads to find a stale index. The
    /// repair command is the point of the field, so it must be present
    /// verbatim, and an absent writer record must render `null` rather than an
    /// empty string (which would read as "a version we read as blank").
    #[test]
    fn activity_index_freshness_json_names_the_repair_command() {
        let behind = ActivityIndexFreshness::Behind {
            machines: 3,
            stale: vec![StaleActivityIndex {
                machine: "dims-macbook-pro-17".to_string(),
                recorded_version: None,
                repair_command: activity_index_repair_command("storagebox", "dims-macbook-pro-17"),
            }],
        };
        let v = activity_index_json(&behind);
        assert_eq!(v["kind"], "behind");
        assert_eq!(v["machines"], 3);
        assert_eq!(v["stale"][0]["machine"], "dims-macbook-pro-17");
        assert_eq!(v["stale"][0]["recorded_version"], serde_json::Value::Null);
        assert_eq!(
            v["stale"][0]["repair_command"],
            "chat-stasher activity-index --rebuild --destination storagebox \
             --machine dims-macbook-pro-17 --stage <workspace>"
        );

        let current = ActivityIndexFreshness::Current { machines: 3 };
        assert_eq!(activity_index_json(&current)["kind"], "current");
        let unknown = ActivityIndexFreshness::Unknown {
            detail: "unreachable".to_string(),
        };
        assert_eq!(activity_index_json(&unknown)["kind"], "unknown");
    }
}

#[cfg(test)]
mod b90_unknown_count_tests {
    use super::*;

    fn footprint(unreadable: Option<u64>) -> HarnessFootprint {
        HarnessFootprint {
            name: "opencode".to_string(),
            root: Some(PathBuf::from("/nowhere/store.db")),
            installed: true,
            session_count: Some(3),
            candidate_count: Some(414),
            unreadable_count: unreadable,
            unreadable_entry_count: Some(0),
            total_bytes: None,
            time_basis: TimeBasis::Unmeasured,
            earliest: None,
            latest: None,
            compressed_count: 0,
            recognized_files: Vec::new(),
            note: String::new(),
        }
    }

    /// **B90 / A display-side counterproof.** Session enumeration succeeded (`session_count` has value),
    /// but the "how many could not be read" tally itself could not be counted. Old code was silent here,
    /// byte-identical to "counted, and it is 0" — readers could not tell the difference.
    #[test]
    fn an_uncounted_unreadable_tally_shows_up_as_unknown() {
        let detail = footprint_count_detail(&footprint(None));
        assert!(
            detail.contains("unknown"),
            "a failed tally must display as 'unknown', not silently equal 0; got: {detail:?}"
        );
    }

    #[test]
    fn an_unavailable_footprint_bytes_stay_unknown() {
        let f = default_footprint("fixture", PathBuf::from("/nowhere"));
        assert_eq!(footprint_bytes_label(&f), "N/A");
        assert_eq!(footprint_bytes_label(&footprint(None)), "unknown");
    }

    /// On a healthy machine "it stays silent": counted, and when it is 0, this cell is empty.
    #[test]
    fn a_counted_zero_stays_silent() {
        assert_eq!(footprint_count_detail(&footprint(Some(0))), "");
    }

    /// Counted and non-zero: prints the number as before.
    #[test]
    fn a_counted_number_still_prints_itself() {
        assert!(footprint_count_detail(&footprint(Some(411))).contains("411 unreadable"));
    }

    /// A row never enumerated at all (`session_count` is also `None`) should not be dragged
    /// into saying "unknown" by this rule: its status field already said so, repeating it is noise.
    #[test]
    fn a_row_that_was_never_enumerated_stays_quiet() {
        let mut f = footprint(None);
        f.session_count = None;
        f.candidate_count = None;
        assert_eq!(footprint_count_detail(&f), "");
    }

    /// B91/C old-behaviour counterexample: the default row uses `0` for bytes
    /// even though this row means "the harness was not inspected".  Zero is a
    /// measured empty footprint, not an unknown one.
    #[test]
    fn an_unavailable_footprint_must_not_collapse_bytes_to_zero() {
        let f = default_footprint("fixture", PathBuf::from("/nowhere"));
        assert_eq!(f.total_bytes, None);
        assert_eq!(footprint_bytes_label(&f), "N/A");
    }
}

#[cfg(test)]
mod json_tests {
    use super::*;

    /// A synthetic footprint row with the three-state fields exercised.
    fn fp() -> HarnessFootprint {
        HarnessFootprint {
            name: "opencode".to_string(),
            root: Some(PathBuf::from("/nowhere/store.db")),
            installed: true,
            session_count: Some(3),
            candidate_count: Some(414),
            unreadable_count: None,
            unreadable_entry_count: Some(0),
            total_bytes: Some(4096),
            time_basis: TimeBasis::SessionTime,
            earliest: Some(UNIX_EPOCH),
            latest: None,
            compressed_count: 0,
            recognized_files: Vec::new(),
            note: String::new(),
        }
    }

    fn probe() -> scanner::HarnessProbe {
        scanner::HarnessProbe {
            id: "claude-code".to_string(),
            display_name: "Claude Code".to_string(),
            root: Some(PathBuf::from("/nowhere/.claude")),
            confidence: crate::scanner::Confidence::Confirmed,
            state: scanner::ProbeState::Scanned,
            record_count: Some(2),
            candidate_count: Some(2),
            unreadable_count: Some(0),
            unreadable_entry_count: Some(0),
            earliest: Some(UNIX_EPOCH),
            latest: Some(UNIX_EPOCH),
            bytes: None,
            recognized_files: Vec::new(),
            note: String::new(),
        }
    }

    /// The whole report assembled from synthetic parts — no IO.
    fn report() -> DoctorReport {
        DoctorReport {
            config_source: crate::config::ConfigSource::DefaultsMissing,
            config_error: None,
            claude: ClaudeCheck {
                layers: vec![(
                    PathBuf::from("/nowhere/.claude/settings.json"),
                    ClaudeRetention::UnsetDefault,
                )],
                verdict: ClaudeRetention::UnsetDefault,
            },
            gemini: GeminiRetention::default(),
            footprints: vec![fp()],
            other_present: vec![PathBuf::from("/nowhere/.cursor")],
            risks: vec![
                "🔴 Claude Code: cleanupPeriodDays is unset → default 30 days.".to_string(),
                "🟢 Gemini: disabled — no risk.".to_string(),
            ],
            reclaim: Some(ReclaimCheck::NoRepo {
                repo_root: PathBuf::from("/nowhere/repo"),
            }),
            cache: Some(CacheCheck::NoCacheDir {
                root: PathBuf::from("/nowhere/rustic"),
            }),
            body_cache: Some(BodyCacheCheck::NoCacheDir {
                root: PathBuf::from("/nowhere/body"),
            }),
            fts_indexes: Some(Vec::new()),
            probes: vec![probe()],
            archive_gaps: Vec::new(),
            machine_logs: MachineLogRows::default(),
            scan_failed: false,
            destinations: Vec::new(),
            native_host: None,
            stage_duplicate_shards: None,
            keys: Some(Vec::new()),
        }
    }

    /// Top-level field-name stability. Bumping a key here is a breaking change
    /// for every script that parsed `doctor --json`, so the names are pinned.
    ///
    /// ADR-023 added `destinations`, and this list is the place that change is
    /// forced to be visible: it is a new key, not a renamed or removed one, so
    /// a script that reads the keys it already knew about keeps working.
    ///
    /// The Native Messaging work added `native_host` under the same rule. The
    /// assertion below is unchanged — the same exact-list comparison — so the
    /// addition could not have been made quietly.
    ///
    /// ADR-034 added `body_cache`, again as a new key: the existing `cache`
    /// keeps its meaning (rustic's metadata cache, D6) and a script reading it
    /// is unaffected. The two caches are reported apart on purpose — one is
    /// governed by a quota and the other is not, and a single merged number
    /// could not be compared against either.
    ///
    /// D10 adds the local FTS inventory as a new field; its per-destination
    /// state does not require a destination connection or archive read.
    ///
    /// `keys` follows the same rule: every archive copy has its own key file
    /// (ADR-013/ADR-039), so "which keys does this machine hold" has no answer
    /// to fold into an existing field. New key, nothing renamed or removed.
    #[test]
    fn doctor_json_top_level_field_names_are_stable() {
        let v = report_to_json(&report());
        let obj = v.as_object().expect("doctor json is an object");
        assert_eq!(
            obj.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "archive_gaps",
                "body_cache",
                "cache",
                "claude",
                "command",
                "config_error",
                "config_source",
                "destinations",
                "footprints",
                "fts_indexes",
                "gemini",
                "keys",
                "machine_logs",
                "native_host",
                "not_checked",
                "other_present",
                "probes",
                "reclaim",
                "risks",
                "scan_failed",
                "schema_version",
                "stage_duplicate_shards",
            ]
        );
    }

    /// A footprint whose unreadable tally was never taken is `unknown`, never
    /// a quiet zero — the same B90 rule the human table follows.
    #[test]
    fn doctor_json_uncounted_unreadable_is_unknown() {
        let v = report_to_json(&report());
        assert_eq!(
            v["footprints"][0]["unreadable_count"],
            serde_json::json!({"kind":"unknown","why":"the unreadable count itself could not be counted"})
        );
        // candidate_count / session_count are measured numbers.
        assert_eq!(
            v["footprints"][0]["candidate_count"],
            serde_json::json!({"kind":"known","count":414})
        );
        assert_eq!(
            v["footprints"][0]["session_count"],
            serde_json::json!({"kind":"known","count":3})
        );
        // latest is absent -> unknown, never null.
        assert_eq!(
            v["footprints"][0]["latest"],
            serde_json::json!({"kind":"unknown","why":"no timestamp recorded"})
        );
    }

    /// Risk severity is structured, and the reclaim shape is tagged.
    #[test]
    fn doctor_json_risk_severity_and_reclaim_kind() {
        let v = report_to_json(&report());
        assert_eq!(v["risks"][0]["severity"], serde_json::json!("red"));
        assert_eq!(v["risks"][1]["severity"], serde_json::json!("green"));
        assert_eq!(
            v["risks"][0]["text"],
            serde_json::json!("🔴 Claude Code: cleanupPeriodDays is unset → default 30 days.")
        );
        assert_eq!(v["reclaim"]["kind"], serde_json::json!("no_repo"));
    }

    /// `scan_failed` is a real boolean carried through; a scan-failed report
    /// never hides the fact that coverage is unknown.
    #[test]
    fn doctor_json_carries_scan_failed() {
        let mut r = report();
        r.scan_failed = true;
        let v = report_to_json(&r);
        assert_eq!(v["scan_failed"], serde_json::json!(true));
    }

    // -----------------------------------------------------------------------
    // D6 — local metadata cache occupancy
    // -----------------------------------------------------------------------

    /// ADR-001 option 6, made visible: a cache root that does not exist yet is
    /// "unknown" (never measured), NEVER a fake `0` — the same lie this repo
    /// has paid for before (unknown counts serialised as zero).
    #[test]
    fn cache_report_is_unknown_when_cache_dir_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("no-such-cache");
        let config = Config {
            rustic_cache_dir: Some(missing.to_string_lossy().into_owned()),
            rustic_no_cache: None,
            ..Config::default()
        };
        let check = inspect_cache(&config);
        assert_eq!(check.kind_label(), "no_cache_dir");
        let v = cache_json(&check);
        assert_eq!(v["kind"], serde_json::json!("no_cache_dir"));
        assert_eq!(
            v["total_bytes"],
            serde_json::json!({"kind":"unknown","why":"cache directory does not exist"})
        );
        assert_ne!(
            v["total_bytes"],
            serde_json::json!({"kind":"known","count":0}),
            "a missing cache dir must never read as a measured empty"
        );
    }

    /// A real directory measures its recursive bytes and counts one subdir per
    /// repository cached on this machine (rustic caches each repo under
    /// `<root>/<repository-id>`).
    #[test]
    fn body_cache_report_is_unknown_when_the_cache_dir_is_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("no-such-body-cache");
        let config = Config {
            cache: Some(crate::config::CacheSectionConfig {
                max_bytes: None,
                dir: Some(missing.to_string_lossy().into_owned()),
            }),
            ..Config::default()
        };
        let check = inspect_body_cache(&config);
        assert_eq!(check.kind_label(), "no_cache_dir");
        let v = body_cache_json(&check);
        assert_eq!(v["kind"], serde_json::json!("no_cache_dir"));
        assert_eq!(
            v["total_bytes"],
            serde_json::json!({"kind":"unknown","why":"cache directory does not exist"})
        );
        assert_ne!(
            v["total_bytes"],
            serde_json::json!({"kind":"known","count":0}),
            "a missing body-cache dir must never read as a measured empty"
        );
    }

    /// A real body cache measures its bytes and its entry count, and reports
    /// the quota it is measured against — a byte count without the quota it
    /// must stay under answers a different question.
    #[test]
    fn body_cache_report_measures_a_real_dir_and_names_the_quota() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("body");
        let cache = crate::body_cache::BodyCache::new(root.clone(), 1 << 20);
        // Two entries of 4 bytes each, stored through the cache's own writer so
        // the measurement covers real entries rather than arbitrary files.
        let hex = "ab".repeat(32);
        let key = crate::body_cache::CacheKey::new(
            &hex.parse::<rustic_core::Id>().expect("hex id"),
            0,
            4,
        );
        cache.put(&key, b"aaaa");
        let config = Config {
            cache: Some(crate::config::CacheSectionConfig {
                max_bytes: Some(crate::body_cache::CacheSize(1 << 20)),
                dir: Some(root.to_string_lossy().into_owned()),
            }),
            ..Config::default()
        };
        let check = inspect_body_cache(&config);
        assert_eq!(check.kind_label(), "ok");
        let v = body_cache_json(&check);
        assert_eq!(v["kind"], serde_json::json!("ok"));
        assert_eq!(
            v["total_bytes"],
            serde_json::json!({"kind":"known","count":
                crate::body_cache::ENTRY_HEADER_LEN + 4}),
            "the measured bytes must be what is actually on the disk"
        );
        assert_eq!(v["entries"], serde_json::json!(1));
        assert_eq!(
            v["max_bytes"],
            serde_json::json!({"kind":"known","count": 1048576}),
            "the quota is part of the answer, not a separate command"
        );
    }

    /// `max_bytes = 0` is a deliberate switch, not an empty cache: it is
    /// reported as `disabled`, and a leftover directory is measured rather than
    /// hidden.
    #[test]
    fn body_cache_report_distinguishes_off_from_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("body");

        // Off with nothing on disk: not_applicable, never a measured zero.
        let config = Config {
            cache: Some(crate::config::CacheSectionConfig {
                max_bytes: Some(crate::body_cache::CacheSize(0)),
                dir: Some(root.to_string_lossy().into_owned()),
            }),
            ..Config::default()
        };
        let check = inspect_body_cache(&config);
        assert_eq!(check.kind_label(), "disabled");
        let v = body_cache_json(&check);
        assert_eq!(
            v["total_bytes"],
            serde_json::json!({
                "kind": "not_applicable",
                "why": "cache is disabled and no leftover cache could be measured"
            })
        );

        // Off with something left over from before: measured and reported.
        let cache = crate::body_cache::BodyCache::new(root.clone(), 1 << 20);
        let hex = "cd".repeat(32);
        let key = crate::body_cache::CacheKey::new(
            &hex.parse::<rustic_core::Id>().expect("hex id"),
            8,
            4,
        );
        cache.put(&key, b"bbbb");
        let v = body_cache_json(&inspect_body_cache(&config));
        assert_eq!(
            v["total_bytes"],
            serde_json::json!({"kind":"known","count":
                crate::body_cache::ENTRY_HEADER_LEN + 4}),
            "a cache switched off must still show what it left on the disk"
        );
        assert_eq!(v["entries"], serde_json::json!(1));
    }

    /// A `[cache]` value that could not be read is a finding of its own: not
    /// `disabled` (which is the user's own switch), not a missing directory,
    /// and never a quota.
    #[test]
    fn body_cache_report_names_an_unreadable_cache_section() {
        let config = Config {
            cache_error: Some(
                "`[cache]` could not be read: cache size `50G` has an unknown unit `G`".to_string(),
            ),
            ..Config::default()
        };
        let check = inspect_body_cache(&config);
        assert_eq!(check.kind_label(), "invalid");
        let v = body_cache_json(&check);
        assert_eq!(v["kind"], serde_json::json!("invalid"));
        assert!(
            v["error"].as_str().unwrap_or_default().contains("50G"),
            "the report must quote the value to fix: {v}"
        );
        assert_ne!(
            v["total_bytes"],
            serde_json::json!({"kind":"known","count":0}),
            "a cache that could not be configured is not a measured empty one"
        );
    }

    /// A directory that is not the cache's own is reported as such, and its
    /// bytes are not presented as this cache's occupancy.
    #[test]
    fn body_cache_report_distinguishes_another_directory_from_an_empty_cache() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("someone-elses-dir");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("keep-me.txt"), b"not a cache entry").unwrap();
        let config = Config {
            cache: Some(crate::config::CacheSectionConfig {
                max_bytes: Some(crate::body_cache::CacheSize(1 << 20)),
                dir: Some(root.to_string_lossy().into_owned()),
            }),
            ..Config::default()
        };

        let check = inspect_body_cache(&config);
        assert_eq!(check.kind_label(), "not_a_cache_root");
        let v = body_cache_json(&check);
        assert_eq!(v["kind"], serde_json::json!("not_a_cache_root"));
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains(".chat-stasher-body-cache"),
            "the report must name the marker it looked for: {v}"
        );
        assert_ne!(
            v["total_bytes"],
            serde_json::json!({"kind":"known","count":17}),
            "another directory's bytes are not this cache's occupancy"
        );
    }

    #[test]
    fn cache_report_measures_a_real_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("rustic");
        fs::create_dir_all(root.join("repo-a-0123456789abcdef")).unwrap();
        fs::create_dir_all(root.join("repo-b-0123456789abcdef")).unwrap();
        fs::write(
            root.join("repo-a-0123456789abcdef").join("index.jsonl"),
            vec![0u8; 100],
        )
        .unwrap();
        fs::write(
            root.join("repo-b-0123456789abcdef").join("snapshot.jsonl"),
            vec![0u8; 50],
        )
        .unwrap();
        let tag = b"Signature: 8a477f597d28d172789f06886806bc55\n";
        fs::write(root.join("CACHEDIR.TAG"), tag).unwrap();

        let usage = measure_cache_dir(&root).unwrap();
        assert_eq!(usage.total_bytes, (100 + 50 + tag.len()) as u64);
        assert_eq!(usage.repo_dirs, 2);

        // Through the full report + JSON serialisation.
        let config = Config {
            rustic_cache_dir: Some(root.to_string_lossy().into_owned()),
            rustic_no_cache: None,
            ..Config::default()
        };
        let v = cache_json(&inspect_cache(&config));
        assert_eq!(v["kind"], serde_json::json!("ok"));
        assert_eq!(
            v["total_bytes"],
            serde_json::json!({"kind":"known","count":(100 + 50 + tag.len())})
        );
        assert_eq!(v["repo_dirs"], serde_json::json!(2));
        assert!(
            v["note"]
                .as_str()
                .unwrap_or_default()
                .contains("whole-machine"),
            "the JSON must say explicitly that the number covers every repository, not just the configured one"
        );
    }

    /// The switch is visible: `rustic_no_cache = true` reports "disabled"
    /// rather than pretending there is no cache directory. With no leftover to
    /// measure (the configured root does not exist) the byte count is
    /// `not_applicable`, not a number and not `0`.
    #[test]
    fn cache_report_is_disabled_when_no_cache_set() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("never-written");
        let config = Config {
            rustic_cache_dir: Some(root.to_string_lossy().into_owned()),
            rustic_no_cache: Some(true),
            ..Config::default()
        };
        let v = cache_json(&inspect_cache(&config));
        assert_eq!(v["kind"], serde_json::json!("disabled"));
        assert_eq!(
            v["total_bytes"]["kind"],
            serde_json::json!("not_applicable"),
            "a deliberately-disabled cache is not a number; the switch itself is the answer"
        );
    }
}

/// The `doctor --json` contract itself: what a machine consumer parses when
/// the run checked nothing. `json_tests` above pins the populated shapes; this
/// pins the envelope, the not-checked tag and the collection types against a
/// minimal report — the only shape guaranteed to exist on every machine.
#[cfg(test)]
mod report_to_json_shape_tests {
    use super::*;

    /// The smallest report that is still a report: config read fine, no
    /// optional check filled in, no rows found.
    fn minimal_report() -> DoctorReport {
        DoctorReport {
            config_source: crate::config::ConfigSource::DefaultsMissing,
            config_error: None,
            claude: ClaudeCheck {
                layers: Vec::new(),
                verdict: ClaudeRetention::UnsetDefault,
            },
            gemini: GeminiRetention::default(),
            footprints: Vec::new(),
            other_present: Vec::new(),
            risks: Vec::new(),
            reclaim: None,
            cache: None,
            body_cache: None,
            fts_indexes: None,
            probes: Vec::new(),
            archive_gaps: Vec::new(),
            machine_logs: MachineLogRows::default(),
            scan_failed: false,
            destinations: Vec::new(),
            stage_duplicate_shards: None,
            native_host: None,
            keys: None,
        }
    }

    /// An unchecked section is `{"checked": false}` — never `null`, never `[]`,
    /// never absent. Those three are three different claims ("we looked and it
    /// is empty" / "we do not know" / "this field is not part of the schema"),
    /// and a consumer distinguishing them is the whole point of the tag.
    #[test]
    fn report_to_json_marks_every_unchecked_section_as_checked_false() {
        let v = report_to_json(&minimal_report());
        for key in [
            "reclaim",
            "cache",
            "body_cache",
            "fts_indexes",
            "keys",
            "native_host",
            "stage_duplicate_shards",
        ] {
            assert!(
                v.get(key).is_some(),
                "{key} must be present in doctor --json"
            );
            let value = &v[key];
            assert_eq!(
                value,
                &serde_json::json!({"checked": false}),
                "{key} for a section that was not checked must be exactly {{\"checked\": false}}"
            );
        }
    }

    /// The envelope every script reads first: schema version, command name,
    /// scan health, config provenance, config error.
    #[test]
    fn report_to_json_pins_the_envelope() {
        let v = report_to_json(&minimal_report());
        assert_eq!(v["schema_version"], serde_json::json!(1));
        assert_eq!(v["command"], serde_json::json!("doctor"));
        assert_eq!(v["scan_failed"], serde_json::json!(false));
        assert!(
            v["config_source"].is_string(),
            "config_source is the label string, not the enum variant name in Rust form"
        );
        assert_eq!(
            v["config_source"],
            serde_json::json!(crate::config::ConfigSource::DefaultsMissing.label())
        );
        assert!(
            v["config_error"].is_null(),
            "a healthy report has no config error, and null is the only encoding of that"
        );
    }

    /// `not_checked` is an array of strings in both directions: empty when the
    /// config was usable (everything was checked or legitimately not run), and
    /// the `CHECKS_NEEDING_CONFIG` names themselves when it was not.
    #[test]
    fn report_to_json_serialises_not_checked_as_an_array_of_strings() {
        let v = report_to_json(&minimal_report());
        let not_checked = v["not_checked"]
            .as_array()
            .expect("not_checked must be an array");
        assert!(
            not_checked.is_empty(),
            "a healthy report checked everything it names: {not_checked:?}"
        );
        assert!(not_checked.iter().all(serde_json::Value::is_string));

        let mut unhealthy = minimal_report();
        unhealthy.config_error = Some("config.toml could not be read".to_string());
        let v = report_to_json(&unhealthy);
        let not_checked = v["not_checked"]
            .as_array()
            .expect("not_checked must be an array");
        let expected: Vec<_> = CHECKS_NEEDING_CONFIG
            .iter()
            .map(|name| serde_json::json!(name))
            .collect();
        assert_eq!(
            not_checked, &expected,
            "an unreadable config names the checks it skipped"
        );
        assert!(
            not_checked.iter().all(serde_json::Value::is_string),
            "not_checked holds check names as plain strings: {not_checked:?}"
        );
    }

    /// `claude` and `gemini` are objects with their tri-state shapes pinned:
    /// the verdict tag, the layer list, and the four-field known policy.
    #[test]
    fn report_to_json_claude_and_gemini_are_objects() {
        let v = report_to_json(&minimal_report());

        let claude = v["claude"]
            .as_object()
            .expect("claude must be an object, not null or a string");
        let mut claude_keys: Vec<_> = claude.keys().map(String::as_str).collect();
        claude_keys.sort_unstable();
        assert_eq!(claude_keys, ["dangerous", "layers", "verdict"]);
        assert_eq!(
            v["claude"]["verdict"],
            serde_json::json!({"kind": "unset_default"})
        );
        assert_eq!(v["claude"]["dangerous"], serde_json::json!(true));
        assert_eq!(v["claude"]["layers"], serde_json::json!([]));

        let gemini = v["gemini"]
            .as_object()
            .expect("gemini must be an object, not null or a string");
        let mut gemini_keys: Vec<_> = gemini.keys().map(String::as_str).collect();
        gemini_keys.sort_unstable();
        assert_eq!(
            gemini_keys,
            ["dangerous", "enabled", "kind", "max_age", "min_retention"]
        );
        assert_eq!(v["gemini"]["kind"], serde_json::json!("known"));
        assert_eq!(v["gemini"]["enabled"], serde_json::json!(true));
        assert_eq!(v["gemini"]["max_age"], serde_json::json!("30d"));
        assert_eq!(v["gemini"]["min_retention"], serde_json::json!("1d"));
        // The documented default policy *is* the 30-day cleanup window.
        assert_eq!(v["gemini"]["dangerous"], serde_json::json!(true));
    }

    /// The per-row fields are arrays — a consumer iterates them — and a
    /// minimal report serialises each as an empty array rather than null.
    #[test]
    fn report_to_json_serialises_the_row_fields_as_arrays() {
        let v = report_to_json(&minimal_report());
        for key in [
            "footprints",
            "other_present",
            "risks",
            "archive_gaps",
            "probes",
            "destinations",
        ] {
            assert!(
                v[key].is_array(),
                "{key} must be an array (possibly empty), got {:?}",
                v[key]
            );
        }
    }
}

#[cfg(test)]
mod w285_coverage_and_clock_tests {
    use super::*;
    use crate::scanner::{Confidence, HarnessProbe, ProbeState};

    fn probe(id: &str, state: ProbeState) -> HarnessProbe {
        HarnessProbe {
            id: id.to_string(),
            display_name: id.to_string(),
            root: None,
            confidence: Confidence::Unascertained,
            state,
            record_count: None,
            candidate_count: None,
            unreadable_count: None,
            unreadable_entry_count: None,
            earliest: None,
            latest: None,
            bytes: None,
            recognized_files: Vec::new(),
            note: String::new(),
        }
    }

    /// W285 §6, as arithmetic: a harness nobody looked at is in `known` and in
    /// `never_probed`, and never in the denominator the summary shows. The old
    /// header reported `hit/known` — "0/12" for a machine where two of the
    /// twelve had no resolvable root and were never opened.
    #[test]
    fn never_probed_harnesses_leave_the_denominator_and_are_counted_beside_it() {
        let probes = vec![
            probe("scanned", ProbeState::Scanned),
            probe("missing", ProbeState::Missing),
            probe("unresolvable", ProbeState::SkipUnresolvable),
            probe("unascertained", ProbeState::SkipUnascertained),
            probe("indeterminate", ProbeState::Indeterminate),
            probe("other-platform", ProbeState::SkipWrongPlatform),
        ];
        let coverage = probe_coverage(&probes);

        assert_eq!(coverage.known, 6, "every registry harness is still counted");
        assert_eq!(
            coverage.hit, 1,
            "only the scanned harness is here; a missing one is a look that found nothing"
        );
        assert_eq!(
            coverage.probed, 3,
            "scanned + missing + wrong-platform were looked at or do not apply"
        );
        assert_eq!(
            coverage.never_probed(),
            3,
            "the two Skip* cells and the indeterminate one were never established, and are named separately"
        );
        assert_eq!(
            coverage.hit + coverage.never_probed(),
            4,
            "the defect this replaces: `hit/known` would have printed 1/6 here"
        );
    }

    /// W285 §5: the two clocks are distinguishable in what doctor prints, so a
    /// directory row can never be read as the conversation time `search` shows.
    #[test]
    fn each_footprint_row_names_the_clock_its_timestamps_came_from() {
        let mut dir_row = default_footprint("claude-code", PathBuf::from("/nowhere"));
        dir_row.installed = true;
        dir_row.time_basis = TimeBasis::FileMtime;
        dir_row.earliest = Some(UNIX_EPOCH);
        dir_row.latest = Some(UNIX_EPOCH);
        assert_eq!(
            footprint_times(&dir_row),
            "earliest(mtime) 1970-01-01T00:00:00Z · latest(mtime) 1970-01-01T00:00:00Z"
        );

        let mut sqlite_row = default_footprint("opencode", PathBuf::from("/nowhere/store.db"));
        sqlite_row.installed = true;
        sqlite_row.time_basis = TimeBasis::SessionTime;
        sqlite_row.earliest = Some(UNIX_EPOCH);
        sqlite_row.latest = Some(UNIX_EPOCH);
        assert_eq!(
            footprint_times(&sqlite_row),
            "earliest(session time) 1970-01-01T00:00:00Z · latest(session time) 1970-01-01T00:00:00Z"
        );

        let unmeasured = default_footprint("gemini", PathBuf::from("/nowhere"));
        assert_eq!(
            footprint_times(&unmeasured),
            "earliest - · latest -",
            "an unmeasured row claims no clock at all"
        );
    }

    /// W285 §5: the sentence a reader acts on. A session younger than the window
    /// must not be described as "already about -30 days past" the threshold, and
    /// a session older than it must not be described as being still ahead of it.
    #[test]
    fn the_threshold_phrase_never_flips_a_sign() {
        assert_eq!(
            threshold_phrase(0.0, 30.0),
            "about 30 days from the 30-day threshold"
        );
        assert_eq!(
            threshold_phrase(29.0, 30.0),
            "about 1 days from the 30-day threshold"
        );
        assert_eq!(
            threshold_phrase(30.0, 30.0),
            "already about 0 days past the 30-day threshold"
        );
        assert_eq!(
            threshold_phrase(47.0, 30.0),
            "already about 17 days past the 30-day threshold"
        );
        assert!(
            !threshold_phrase(0.0, 30.0).contains("past"),
            "a session younger than the window is ahead of the threshold, never past it"
        );
        assert!(
            !threshold_phrase(47.0, 30.0).contains("from"),
            "a session older than the window is past the threshold, not still approaching it"
        );
    }
}
