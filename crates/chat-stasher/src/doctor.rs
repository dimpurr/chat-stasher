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

// ---------------------------------------------------------------------------
// The per-check deadline (W1043) — every check is traced before it runs and
// bounded while it runs, so a wedged step can never freeze the whole report
// ---------------------------------------------------------------------------

/// Check labels, shared by the trace a check prints before running, the
/// `timed_out` notes a run records, and the sections of the report — so the
/// line a hang leaves behind names the same check the report's section does.
/// `D7 destination probe (<name>) …` labels are composed at runtime instead.
pub const D1_CHECK: &str = "D1 Claude Code rotation settings";
pub const D2_CHECK: &str = "D2 Gemini CLI retention policy";
pub const CONFIG_CHECK: &str = "configuration file";
pub const D3_SCAN_CHECK: &str = "D3 harness registry and scan";
pub const DESKTOP_RETENTION_CHECK: &str = "D1\u{00bd} Desktop/Cowork retention settings";
pub const OTHER_HARNESS_CHECK: &str = "detected out-of-scope harness directories";
pub const RECLAIM_CHECK: &str = "D5 repository reclaim";
pub const CACHE_CHECK: &str = "D6 local metadata cache";
pub const BODY_CACHE_CHECK: &str = "D9 body cache";
pub const FTS_CHECK: &str = "D10 full-text indexes";
pub const STAGE_DUP_CHECK: &str = "D11 stage duplicate scan";
pub const NATIVE_HOST_CHECK: &str = "D8 native host stage";
pub const MACHINE_LOGS_CHECK: &str = "machine log rows";
pub const KEYS_CHECK: &str = "key inventory";
pub const DEST_RECONCILE_CHECK: &str = "D12 destination reconciliation";
pub const MIRROR_CHECK: &str = "D13 local mirror";
pub const INBOX_STATUS_CHECK: &str = "remote inbox status";

/// Per-check budgets, in seconds. A budget is a *diagnosis* bound, not an
/// archive bound: every check above is a read of a few files, a bounded walk
/// or one connection probe, and the value is only a floor against a hang
/// (`reader_guard` documents why the bulk commands are deliberately not
/// bounded instead). A check that needs more time than this on a healthy
/// machine is a bug in the check, not a budget to raise.
pub const SETTINGS_BUDGET_SECS: u64 = 30;
pub const CONFIG_BUDGET_SECS: u64 = 30;
pub const SCAN_BUDGET_SECS: u64 = 240;
pub const BACKED_WALK_BUDGET_SECS: u64 = 90;
pub const RECLAIM_BUDGET_SECS: u64 = 240;
pub const INBOX_BUDGET_SECS: u64 = 30;
pub const DESTINATION_CONNECT_BUDGET_SECS: u64 = 45;
pub const DESTINATION_READ_BUDGET_SECS: u64 = 120;

/// One check's budget after the test override. The default is the check's
/// own; `CHAT_STASHER_DOCTOR_CHECK_BUDGET_SECS` replaces **every** check's
/// budget with one number (floored at 1), so a fixture can turn a hang into
/// a short wait without owning a second copy of the table. Anything below 1
/// would stop being a bound at all.
///
/// W1043f: the override is a product lever, not a test secret — it is the
/// one knob a user with a healthy-but-slow remote has, and `docs/cli.md`'s
/// `doctor` section documents it (including how it is applied). The
/// override also cannot disable the bound: an unset variable, an
/// unparsable value or a zero all fall back to the check's own budget.
pub fn check_budget_secs(default_secs: u64) -> u64 {
    std::env::var("CHAT_STASHER_DOCTOR_CHECK_BUDGET_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs >= 1)
        // reason: an unset variable, an unparsable value or a zero all fall
        // back to the check's own budget — an override nobody can read must
        // not disable the bound, and must not change it silently either.
        .unwrap_or(default_secs)
}

/// What one check that outlived its budget owes every reader: the same
/// label the `doctor: checking …` trace printed before it ran, and the
/// budget it exceeded.
///
/// Deliberately not a "failed" verdict: a timeout is an **unknown**, exactly
/// like `config_error` — the check did not answer, it did not find nothing —
/// and the surrounding machinery keeps the two from being worded as each
/// other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckTimedOut {
    /// The check's label — the words the trace printed, the words the
    /// INCOMPLETE line and the report section use.
    pub check: String,
    /// The per-check budget that expired, in seconds.
    pub budget_secs: u64,
}

impl CheckTimedOut {
    /// The JSON shape of one note, under the report's `timed_out` array.
    fn json(&self) -> serde_json::Value {
        serde_json::json!({"check": self.check, "budget_secs": self.budget_secs})
    }
}

/// Runs doctor's checks one at a time, each behind its own deadline (W1043).
///
/// Two properties, both owed to a doctor user whose machine is half-broken —
/// the one user this command is *for*:
///
/// 1. **A trace before every check.** `doctor: checking <label>` goes to
///    stderr before the check starts, so a wedged run always has its culprit
///    named on a line that was already flushed. Before this harness, the
///    whole report printed at the end, so a hang produced zero output and no
///    clue (the incident this module half was written for: a run with a
///    wedged destination probe waited 90 s on an idle machine and said
///    nothing).
/// 2. **A deadline per check.** A check that outlives its budget is
///    abandoned — the worker thread is left blocked where it is and the run
///    continues — and the note that it timed out rides in the report
///    ([`DoctorReport::timed_out`]), where `doctor` words the section as
///    unknown, exits 3 and prints the checks that *did* answer. Skipping
///    with a timeout note is the only honest answer a hang permits.
///
/// A panic inside the check propagates unchanged (`resume_unwind` of the
/// payload the worker's join hands back): a panicking check is a bug to
/// fix, not an unknown to word around, and intercepting it here would
/// silently change a crash into a partial report.
///
/// The abandoned thread cannot be joined on the timeout path — joining is
/// the hang again — so it is detached and keeps whatever it was blocked on
/// until the process exits; the check is read-only, so the only thing it
/// leaks is the wait it was already stuck in. `std::thread::scope` is not an
/// option here for exactly that reason: a scope joins on exit, so a stuck
/// worker would convert the budget right back into a hang.
#[derive(Debug, Default)]
pub struct CheckRunner {
    notes: Vec<CheckTimedOut>,
}

impl CheckRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// The checks that outlived their budget, in the order they ran.
    pub fn notes(&self) -> &[CheckTimedOut] {
        &self.notes
    }

    /// Whether `check` timed out, and with which budget — the one place the
    /// report, the printer and the exit code agree on when they ask.
    pub fn timed_out_after(&self, check: &str) -> Option<u64> {
        self.notes
            .iter()
            .find(|note| note.check == check)
            .map(|note| note.budget_secs)
    }

    /// Run one check behind its deadline: print its trace, let it answer
    /// within the budget, or record the note and return `None`.
    ///
    /// `None` is exclusively the timeout: a finished check always returns
    /// `Some`, whatever it measured, and a panicking one never returns.
    pub fn run<T, F>(&mut self, check: &str, default_budget_secs: u64, f: F) -> Option<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let budget = std::time::Duration::from_secs(check_budget_secs(default_budget_secs));
        eprintln!("doctor: checking {check}");
        // The closure travels in a carrier so the spawn-failure path below
        // can still run the check itself: `Builder::spawn` consumes its
        // argument, and a worker that was never created is the one failure
        // that must not lose the question it was handed.
        let carrier = std::sync::Arc::new(std::sync::Mutex::new(Some(f)));
        let (answer, answers) = std::sync::mpsc::channel();
        let carried = std::sync::Arc::clone(&carrier);
        let worker = std::thread::Builder::new()
            .name("doctor-check".to_string())
            .spawn(move || {
                // Take the closure out under the lock, release the guard, and
                // only then run it: the sender drops when this scope ends
                // whether the check answered or panicked, which is all the
                // caller needs — an answer, or a disconnect meaning "ended
                // without one".
                let taken = match carried.lock() {
                    Ok(mut slot) => slot.take(),
                    Err(poisoned) => poisoned.into_inner().take(),
                };
                if let Some(f) = taken {
                    #[allow(
                        clippy::let_underscore_must_use,
                        reason = "a send that fails means the caller's budget already expired \
                                  and the receiver is gone — the answer is rightfully discarded, \
                                  and the error says nothing either side should act on"
                    )]
                    let _ = answer.send(f());
                }
            });
        let handle = match worker {
            Ok(handle) => handle,
            Err(_) => {
                // reason: a spawn failure is a machine out of threads, and
                // the bound itself is dead without one — running the check
                // keeps the answer honest at the cost of a deadline nothing
                // could have enforced anyway. The carrier still holds the
                // closure because a thread that was never created is the one
                // branch that never took it.
                let recovered = match carrier.lock() {
                    Ok(mut slot) => slot.take(),
                    Err(poisoned) => poisoned.into_inner().take(),
                };
                match recovered {
                    Some(f) => return Some(f()),
                    None => {
                        std::unreachable!(
                            "the doctor-check carrier was stripped by a worker that was never \
                             created"
                        );
                    }
                }
            }
        };
        match answers.recv_timeout(budget) {
            Ok(value) => Some(value),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.notes.push(CheckTimedOut {
                    check: check.to_string(),
                    budget_secs: budget.as_secs(),
                });
                None
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // The worker ended without answering: a panic. Hand its
                // payload back to this thread and let it unwind exactly as
                // it would have without the harness.
                match handle.join() {
                    Ok(()) => std::unreachable!(
                        "a doctor-check worker only ends without answering by panicking, and the \
                         carrier is only stripped by a thread that never existed"
                    ),
                    Err(payload) => std::panic::resume_unwind(payload),
                }
            }
        }
    }
}
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
    /// The settings read did not answer within its deadline (W1043): no
    /// layer on this machine was read. An unknown, kept apart from
    /// [`Self::UnsetDefault`] (a measured "no file says anything") and from
    /// [`Self::ParseFailed`] (a measured broken file) because the wording
    /// those two carry — the default-30 risk, the fail-destructive alarm —
    /// would be a claim about files nobody read. [`Self::is_dangerous`]
    /// answers `false` for the same reason it does for [`Self::Safe`]: the
    /// flag is a finding's verdict, and a timeout is not a finding; D4 words
    /// the unknown itself.
    TimedOut,
}

impl ClaudeRetention {
    /// Short single-word label used in the summary line.
    pub fn label(&self) -> String {
        match self {
            ClaudeRetention::UnsetDefault => "unset (default 30d)".to_string(),
            ClaudeRetention::Safe { days, .. } => format!("large ({days}d)"),
            ClaudeRetention::SmallValue { days, .. } => format!("small ({days}d)"),
            ClaudeRetention::ParseFailed { .. } => "PARSE FAILED".to_string(),
            ClaudeRetention::TimedOut => "TIMED OUT".to_string(),
        }
    }

    /// True when this state hands Claude Code a live deletion window.
    pub fn is_dangerous(&self) -> bool {
        match self {
            ClaudeRetention::UnsetDefault
            | ClaudeRetention::SmallValue { .. }
            // A parse failure reverts the runtime to its 30-day default, so
            // the window is live whatever the file intended — the flag IS a
            // finding here.
            | ClaudeRetention::ParseFailed { .. } => true,
            // A timeout is not a finding: answering `true` here would word a
            // non-answer as the measured 30-day window, which is exactly the
            // collapse `ParseFailed` and `UnsetDefault` exist to stay apart.
            // D4 words the unknown itself.
            ClaudeRetention::Safe { .. } | ClaudeRetention::TimedOut => false,
        }
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
// D1½ — Claude Code `desktopSessionCleanupPeriodDays` (Desktop/Cowork
// transcripts), the registry-declared retention probe (ADR-059 draft D5,
// DSK-0a / W1009 DSK-3).
// ---------------------------------------------------------------------------

/// Which settings layer one probed file belongs to. The registry declares
/// both lists per platform; the probe must report which one governed the
/// effective value, or "managed overridden" reads as "the user set X"
/// (DSK-0a, the retention-probe semantics section, item 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopSourceKind {
    User,
    Managed,
}

impl DesktopSourceKind {
    fn word(self) -> &'static str {
        match self {
            DesktopSourceKind::User => "user",
            DesktopSourceKind::Managed => "managed",
        }
    }
}

/// What one settings file said (or why it could not say anything). The three
/// broken states are kept apart for the same reason every other tri-state in
/// this file is: an absent file is a measured absence, an unreadable or
/// unparseable one is an unknown, and a value this probe cannot interpret is
/// its own finding — never a silent default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopSourceOutcome {
    /// The declared file is not on this machine. For managed locations this
    /// is the ordinary case (measured locally on two machines: every
    /// declared managed location was absent on both).
    Absent,
    /// The file is there but its bytes could not be read (permissions, I/O).
    Unreadable(String),
    /// The file read but is not parseable JSON. The runtime's fail-destructive
    /// fallback applies to its own load — the desktop key's fallback is the
    /// built-in default, which for *this* key is 0 = no ceiling (the
    /// opposite direction of `cleanupPeriodDays`' 30-day fallback) — so this
    /// is reported loudly, never assumed.
    ParseFailed(String),
    /// The file holds the key but the value is not a plain nonnegative
    /// integer (the runtime's schema is `int().nonnegative().optional()`; an
    /// invalid value is never applied — but which value it *would* apply is
    /// exactly what no probe may guess).
    InvalidValue(String),
    /// Parsed cleanly. Each key is `None` when the file simply does not set
    /// it — an absent key is a fact about the file, not about retention.
    Read {
        desktop_days: Option<u64>,
        cleanup_days: Option<u64>,
    },
}

/// One settings source the probe consulted, in declared order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopRetentionSource {
    pub path: PathBuf,
    /// The declared list the registry put this file in. Reported so the
    /// effective value can never be attributed to the wrong layer.
    pub kind: DesktopSourceKind,
    pub outcome: DesktopSourceOutcome,
}

impl DesktopRetentionSource {
    /// One `D1`-table-style line for this source.
    fn line(&self) -> String {
        let said = match &self.outcome {
            DesktopSourceOutcome::Absent => "absent".to_string(),
            DesktopSourceOutcome::Unreadable(e) => format!("unreadable: {e}"),
            DesktopSourceOutcome::ParseFailed(e) => format!("parse failed: {e}"),
            DesktopSourceOutcome::InvalidValue(w) => format!("invalid value: {w}"),
            DesktopSourceOutcome::Read {
                desktop_days,
                cleanup_days,
            } => {
                let mut parts = Vec::new();
                parts.push(match desktop_days {
                    Some(days) => format!("{days}"),
                    None => "not set".to_string(),
                });
                if let Some(days) = cleanup_days {
                    parts.push(format!("cleanupPeriodDays={days}"));
                }
                parts.join(", ")
            }
        };
        format!(
            "    {:<52} [{:<7}] {}",
            self.path.display(),
            self.kind.word(),
            said
        )
    }
}

/// The three readable states of `desktopSessionCleanupPeriodDays`, plus the
/// managed-override shape that short-circuits all three, plus unknown. The
/// semantics are the registry's recorded ones (`retention_probes` of
/// `claude-code`, written in ADR-059 draft D5 from DSK-0a's measurement):
///
/// * **missing** — the built-in default 0 applies: no ceiling. This is NOT
///   the `cleanupPeriodDays` missing-state default of 30; Desktop/Cowork
///   transcripts are *exempt* from that sweep by default. Never worded as
///   30 here.
/// * **explicit 0** — also no ceiling (0 is allowed because this key bounds
///   an exemption from deletion; it never disables writes).
/// * **positive** — a hard ceiling in days; the effective retention is the
///   *longer* of this ceiling and the effective `cleanupPeriodDays`.
/// * **managed override** — a managed source that sets `cleanupPeriodDays`
///   makes this key ignored entirely, so the report must name the managed
///   source that governed.
#[derive(Debug, Clone, PartialEq)]
pub enum DesktopRetention {
    /// The key is not set in any readable source → default 0 = no ceiling.
    DefaultZero,
    /// The key is explicitly 0 in `source` → no ceiling, by choice.
    ExplicitZero { source: PathBuf },
    /// A positive ceiling in `source`. `cleanup_days` is the effective
    /// `cleanupPeriodDays` D1 measured (None when D1 could not classify,
    /// e.g. a parse failure); `effective_days` is the longer of the two
    /// periods where both are known.
    Ceiling {
        days: u64,
        source: PathBuf,
        cleanup_days: Option<u64>,
        effective_days: Option<u64>,
    },
    /// A managed source sets `cleanupPeriodDays` → the desktop key is
    /// ignored entirely; `days` is the policy value in `source`.
    ManagedOverride { days: u64, source: PathBuf },
    /// A source that could have spoken could not be read, did not parse,
    /// or held a value this probe cannot interpret. Never fallen back to
    /// the default — the runtime's own parse-failure fallback for a
    /// *deletion-optional* key would be "no ceiling", but reporting that
    /// as the user's configuration is exactly the "parsed failure read as
    /// unset" lie D1 exists to prevent.
    Unknown { why: String },
}

impl DesktopRetention {
    /// The effective retention in days for Desktop/Cowork transcripts, when
    /// known: unbounded shapes are `None` (no ceiling — kept until deleted
    /// another way), a ceiling/override carries its effective number.
    fn effective_days(&self) -> Option<u64> {
        match self {
            DesktopRetention::DefaultZero | DesktopRetention::ExplicitZero { .. } => None,
            DesktopRetention::Ceiling { effective_days, .. } => *effective_days,
            DesktopRetention::ManagedOverride { days, .. } => Some(*days),
            DesktopRetention::Unknown { .. } => None,
        }
    }

    /// True when this state hands Claude Code a live deletion window for
    /// desktop-hosted transcripts — the same threshold D1 applies to
    /// `cleanupPeriodDays` (never a separate standard the two knobs drift
    /// into): a bounded retention under half a year is "still rotating".
    fn is_dangerous(&self) -> bool {
        matches!(self, DesktopRetention::Unknown { .. })
            || self
                .effective_days()
                .is_some_and(|days| days < CLEANUP_SAFE_DAYS)
    }

    /// Short label for the D1 block's headline.
    pub fn label(&self) -> String {
        match self {
            DesktopRetention::DefaultZero => {
                "not set → default 0 = no ceiling (≠ the 30-day cleanupPeriodDays default)"
                    .to_string()
            }
            DesktopRetention::ExplicitZero { source } => {
                format!("explicit 0 = no ceiling ({})", source.display())
            }
            DesktopRetention::Ceiling {
                days,
                source,
                effective_days: _,
                cleanup_days: _,
            } => format!(
                "{days} days ({}); effective retention is the longer of this and cleanupPeriodDays",
                source.display()
            ),
            DesktopRetention::ManagedOverride { days, source } => format!(
                "ignored — cleanupPeriodDays is managed ({days} days, {})",
                source.display()
            ),
            DesktopRetention::Unknown { why } => format!("unknown ({why})"),
        }
    }
}

/// The full result of the Desktop retention probe.
#[derive(Debug, Clone)]
pub struct DesktopRetentionCheck {
    /// Registry probe id (`desktop-session-cleanup`) and the settings key.
    pub probe_id: String,
    pub key: String,
    /// Every source the probe consulted, in the registry's declared order —
    /// user sources first, then managed — with what each said. Presence + a
    /// kind word per row; never file contents.
    pub sources: Vec<DesktopRetentionSource>,
    /// The merged verdict, per [`DesktopRetention`].
    pub state: DesktopRetention,
}

/// The effective `cleanupPeriodDays` as D1 measured it, for the
/// longer-of-two comparison. Unset → the runtime's documented 30-day default
/// (the same number D1's own label prints); a parse failure → `None`, which
/// the ceiling state carries so the comparison never fabricates a floor.
fn d1_effective_cleanup_days(verdict: &ClaudeRetention) -> Option<u64> {
    match verdict {
        // reason: Claude Code's documented built-in default when the key is
        // unset is 30 days (D1 prints the same number); this is the runtime's
        // documented default, not an unknown collapsed into a concrete value.
        ClaudeRetention::UnsetDefault => Some(30),
        ClaudeRetention::Safe { days, .. } | ClaudeRetention::SmallValue { days, .. } => {
            Some(*days)
        }
        // `TimedOut` sits beside `ParseFailed` on purpose: the CLI side of the
        // longer-of-two comparison is unknown, so the longer of the two is
        // unknown — `DesktopRetention::Ceiling`'s `effective_days` must stay
        // `None` rather than borrow a default from a file nobody read.
        ClaudeRetention::ParseFailed { .. } | ClaudeRetention::TimedOut => None,
    }
}

/// Read one integer settings key out of a parsed settings file. `Ok(None)`
/// = the file does not set it; `Err(_) ` = the key is present but this
/// probe cannot interpret it (never silently dropped: the caller reports it).
fn settings_int(value: &serde_json::Value, key: &str) -> Result<Option<u64>, String> {
    match value.get(key) {
        None => Ok(None),
        Some(v) => match v.as_u64() {
            Some(days) => Ok(Some(days)),
            None => Err(format!(
                "{key} is present but not a nonnegative integer (found {})",
                kind_word(v)
            )),
        },
    }
}

/// The JSON kind word for a value this probe cannot read as a day count —
/// so an `InvalidValue` row says what was actually there, without printing a
/// value that may carry anything else.
fn kind_word(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(n) if n.is_f64() => "a non-integer number",
        serde_json::Value::Number(_) => "a negative number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Read one declared settings source and classify what it said.
fn inspect_desktop_source(
    template: &str,
    kind: DesktopSourceKind,
    key: &str,
) -> DesktopRetentionSource {
    // The registry's template is a settings **file**: expand it through the
    // same expander every session root uses, refuse (as one refused probe
    // row, not a crash) if it cannot be anchored, and treat an unanchorable
    // template as an unreadable source — this build cannot read a value from
    // a path it cannot name.
    let path = match scanner::static_prefix_root(template) {
        // reason: an unanchorable template (a variable this build does not
        // resolve) still *names a file that may exist*; reporting the source
        // as unreadable keeps the three states — absent / broken / read —
        // intact instead of dropping the source and reading its absence as
        // "the key is not set".
        Some((path, _)) => path,
        None => {
            return DesktopRetentionSource {
                path: PathBuf::from(template),
                kind,
                outcome: DesktopSourceOutcome::Unreadable(format!(
                    "template cannot be resolved to a path this build knows"
                )),
            };
        }
    };
    let outcome = match fs::read_to_string(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => DesktopSourceOutcome::Absent,
        Err(e) => DesktopSourceOutcome::Unreadable(format!("{e}")),
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Err(e) => DesktopSourceOutcome::ParseFailed(format!("{e}")),
            Ok(v) => match (settings_int(&v, key), settings_int(&v, "cleanupPeriodDays")) {
                (Err(desktop), _) => DesktopSourceOutcome::InvalidValue(desktop),
                (_, Err(cleanup)) => DesktopSourceOutcome::InvalidValue(cleanup),
                (Ok(desktop_days), Ok(cleanup_days)) => DesktopSourceOutcome::Read {
                    desktop_days,
                    cleanup_days,
                },
            },
        },
    };
    DesktopRetentionSource {
        path,
        kind,
        outcome,
    }
}

/// D1½ — read the registry-declared `desktopSessionCleanupPeriodDays` probe
/// (claude-code's `retention_probes`, ADR-059 draft D5) and classify the
/// machine's state, so D4 can state the Desktop/Cowork retention risk
/// without re-deriving a location or a default.
///
/// Reads exactly the sources the registry declares for this platform — the
/// User settings file plus managed policy files — and never looks under the
/// desktop app's Application Support tree (measured there and disproved as a
/// source, DSK-0a). `None` when this registry declares no such probe (a
/// scratch registry, an older registry) — the check is then *not performed*
/// rather than performed against guessed paths.
pub fn inspect_desktop_retention(
    registry: &scanner::HarnessRegistry,
    claude: &ClaudeCheck,
) -> Option<DesktopRetentionCheck> {
    let harness = registry.harnesses.iter().find(|h| h.id == "claude-code")?;
    let probe = harness
        .retention_probes
        .iter()
        .find(|probe| probe.key == "desktopSessionCleanupPeriodDays")?;
    let platform = scanner::current_platform();
    let cell = probe.platforms.cell_for(platform)?;
    let mut sources = Vec::new();
    for template in &cell.user_sources {
        sources.push(inspect_desktop_source(
            template,
            DesktopSourceKind::User,
            &probe.key,
        ));
    }
    for template in &cell.managed_sources {
        sources.push(inspect_desktop_source(
            template,
            DesktopSourceKind::Managed,
            &probe.key,
        ));
    }

    // Managed override first (DSK-0a item 3): a managed source that sets
    // `cleanupPeriodDays` makes the desktop key ignored entirely, whatever
    // any user file says — so this must be decided before the key's own
    // value, or "overridden by org policy" reads as "the user set X".
    let managed_override = sources.iter().find_map(|source| match source {
        DesktopRetentionSource {
            kind: DesktopSourceKind::Managed,
            outcome:
                DesktopSourceOutcome::Read {
                    cleanup_days: Some(days),
                    ..
                },
            path,
            ..
        } => Some((*days, path.clone())),
        _ => None,
    });
    let state = if let Some((days, source)) = managed_override {
        DesktopRetention::ManagedOverride { days, source }
    } else {
        // The key's own value, in the runtime's precedence order: managed
        // (policySettings) before user — the scope's own "User or managed"
        // recorded in the registry, and the same order the runtime's settings
        // merge consults (DSK-0a §3: policySettings first).
        let desktop_value = sources
            .iter()
            .filter(|source| source.kind == DesktopSourceKind::Managed)
            .chain(
                sources
                    .iter()
                    .filter(|source| source.kind == DesktopSourceKind::User),
            )
            .find_map(|source| match source {
                DesktopRetentionSource {
                    outcome:
                        DesktopSourceOutcome::Read {
                            desktop_days: Some(days),
                            ..
                        },
                    path,
                    ..
                } => Some((*days, path.clone())),
                _ => None,
            });
        // Anything that could have spoken but could not be read keeps the
        // state unknown — the same rule D1 applies to an unparsed layer,
        // and the opposite direction of this key's fail-open runtime
        // fallback, which is exactly why it must be said rather than lived.
        let broken = sources.iter().find_map(|source| match &source.outcome {
            DesktopSourceOutcome::Unreadable(e)
            | DesktopSourceOutcome::ParseFailed(e)
            | DesktopSourceOutcome::InvalidValue(e) => {
                Some(format!("{}: {e}", source.path.display()))
            }
            _ => None,
        });
        match desktop_value {
            Some((days, source)) => {
                // 0 is allowed and means no ceiling (the key bounds an
                // exemption from deletion; it never disables writes).
                if days == 0 {
                    DesktopRetention::ExplicitZero { source }
                } else {
                    let cleanup_days = d1_effective_cleanup_days(&claude.verdict);
                    // The ceiling is a hard cap; the effective retention is
                    // the *longer* of the two periods — never max-of-knowns
                    // with a fabricated floor when the CLI side is unknown.
                    let effective_days = cleanup_days.map(|cleanup| days.max(cleanup));
                    DesktopRetention::Ceiling {
                        days,
                        source,
                        cleanup_days,
                        effective_days,
                    }
                }
            }
            None => match broken {
                Some(why) => DesktopRetention::Unknown { why },
                None => DesktopRetention::DefaultZero,
            },
        }
    };
    Some(DesktopRetentionCheck {
        probe_id: probe.id.clone(),
        key: probe.key.clone(),
        sources,
        state,
    })
}

impl DesktopRetentionCheck {
    /// The `--settings` invisibility note, printed with the state: a value
    /// handed to a single run through the CLI's `--settings` flag is real to
    /// that run and invisible to a disk probe — its absence here is expected,
    /// not a gap (DSK-0a item 4).
    fn invisibility_note() -> &'static str {
        "probe reads the registry-declared user and managed settings files only; a \
         one-run --settings value is invisible on disk and its absence is expected"
    }

    /// The D4 "so what" line for this state.
    fn risk_line(&self) -> String {
        let today = format_date(SystemTime::now());
        match &self.state {
            DesktopRetention::DefaultZero => format!(
                "🟢 Claude Desktop/Cowork transcripts: {} is not set → default 0 = no \
                 ceiling. Desktop/Cowork transcripts are exempt from the cleanupPeriodDays \
                 sweep by default and are kept until deleted some other way — this is NOT \
                 the 30-day default that governs CLI transcripts (today {today}). \
                 — {}",
                self.key,
                Self::invisibility_note()
            ),
            DesktopRetention::ExplicitZero { source } => format!(
                "🟢 Claude Desktop/Cowork transcripts: {} = 0 (explicitly set by {}) → \
                 no ceiling; kept until deleted some other way (today {today}). \
                 — 0 is allowed here because the key bounds an exemption from deletion; \
                 it never disables writes. {}",
                self.key,
                source.display(),
                Self::invisibility_note()
            ),
            DesktopRetention::Ceiling {
                days,
                source,
                cleanup_days,
                effective_days,
            } => {
                let head = if self.state.is_dangerous() {
                    format!(
                        "🔴 Claude Desktop/Cowork transcripts: {} = {days} days",
                        self.key
                    )
                } else {
                    format!(
                        "🟡 Claude Desktop/Cowork transcripts: {} = {days} days",
                        self.key
                    )
                };
                let effective = match (cleanup_days, effective_days) {
                    (Some(cleanup), Some(effective)) => format!(
                        "the effective retention is the longer of the two periods = \
                         {effective} days (cleanupPeriodDays = {cleanup})"
                    ),
                    // D1's parse failure: the ceiling is a fact; the floor
                    // under it is not, and saying `days` is the effective
                    // retention would fabricate the smaller number.
                    _ => "the effective retention cannot be computed: the \
                          cleanupPeriodDays state is unknown (settings parse failed)"
                        .to_string(),
                };
                format!(
                    "{head} (source {}) → the desktop-kept transcripts now carry a \
                     hard age cap where they were exempt before. {effective}. \
                     — {}",
                    source.display(),
                    Self::invisibility_note()
                )
            }
            DesktopRetention::ManagedOverride { days, source } => {
                let emoji = if self.state.is_dangerous() {
                    "🔴"
                } else {
                    "🟡"
                };
                format!(
                    "{emoji} Claude Desktop/Cowork transcripts: {} is ignored — a managed \
                     source ({}) sets cleanupPeriodDays = {days} days, so org policy \
                     governs rotation and the desktop exemption is out of the user's \
                     hands. — the report names the managing source so this never \
                     reads as \"the user set X\"; {}",
                    self.key,
                    source.display(),
                    Self::invisibility_note()
                )
            }
            DesktopRetention::Unknown { why } => format!(
                "🟡 Claude Desktop/Cowork transcripts: {} state unknown ({why}) — not \
                 guessing the retention risk from a default. — {}",
                self.key,
                Self::invisibility_note()
            ),
        }
    }

    /// The D1-block lines: the state headline, then one row per source read.
    fn print(&self) {
        eprintln!(
            "  {} (Desktop/Cowork transcripts): {}",
            self.key,
            self.state.label()
        );
        for source in &self.sources {
            eprintln!("{}", source.line());
        }
        eprintln!("    ({})", Self::invisibility_note());
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
    /// True when the probe could not establish whether the source is
    /// present at all: an indeterminate probe, a skip shape
    /// (`skip_unascertained` / `skip_unresolvable` — a root the
    /// registry never resolved or declined to scan), or a harness
    /// with no probe row at all. Distinct from a measured absence
    /// (`missing`: looked, and not there) and from a harness the
    /// registry does not list for this platform (`skip_wrong_platform`)
    /// — those two know the source is not here, so their
    /// `installed: false` is a measurement, while a `false` here
    /// would be a guess.
    pub presence_unknown: bool,
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
    /// Registry ids of the harness family's roots that stayed unverified
    /// while a sibling root was measured (ADR-059 draft D6, W1006d).
    /// Non-empty exactly when `session_count` is a *qualified lower
    /// bound* — the exact count of the measured roots only — and the
    /// label prints it as `≥N` beside the note that names why. Empty for
    /// a fully measured row and for a fully unknown one (the state says
    /// that already, and there is no measured count to qualify).
    pub unverified_roots: Vec<String>,
    /// One row per declared source root of this family (W1009/DSK-3):
    /// the probe's own per-root rows, carried through so the footprint
    /// table can print one row per source root beside the aggregate —
    /// with unverified roots explicitly marked instead of folded into a
    /// number that reads as the family total. Copied from the probe
    /// (`footprint_from_dir_probe`) like `unverified_roots` is, so the
    /// two doctor tables can never disagree about a root either. Empty
    /// for families outside the primary-plus-roots shape.
    pub source_roots: Vec<scanner::SourceRootProbe>,
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
    // Sessions counted by identity, records kept by observation. The two are
    // the same number for every ordinary store — one transcript file per
    // session — and come apart exactly once: the same session id discovered
    // through more than one declared source root of the same harness family
    // (W1007's codex uuid sitting in both `sessions/` and the
    // `archived_sessions/` the store moves it through; a copy left behind is
    // the same thread, not a second one). The registry probe table's
    // aggregate counts unique ids for the same reason, and doctor's two
    // tables must agree, so the count here is the distinct-id count too.
    // Everything below keeps a per-record view on purpose: the byte total,
    // the mtimes and the recognised-file set describe the files that are
    // physically there, and a copy is one more file on disk even when it is
    // not one more conversation.
    let distinct_sessions = recs
        .iter()
        .map(|r| r.id.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    HarnessFootprint {
        name: name.to_string(),
        root: Some(root),
        installed,
        presence_unknown: false,
        session_count: if installed {
            Some(distinct_sessions as u64)
        } else {
            None
        },
        candidate_count: None,
        unreadable_count: None,
        unreadable_entry_count: None,
        total_bytes,
        unverified_roots: Vec::new(),
        source_roots: Vec::new(),
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
        None if f.installed || f.presence_unknown => "unknown".to_string(),
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
        presence_unknown: probe.not_probed_p(),
        session_count: probe.record_count,
        candidate_count: probe.candidate_count,
        unreadable_count: probe.unreadable_count,
        unreadable_entry_count: probe.unreadable_entry_count,
        total_bytes: probe.bytes,
        // W1006d: a single-file family has no additional roots today, but
        // the marking travels the same seam the counts do, so a future
        // sqlite entry declaring source_roots cannot lose it here.
        unverified_roots: probe
            .unverified_roots
            .iter()
            .map(|root| root.id.clone())
            .collect(),
        // A single-file family today has no additional source roots, but the
        // W1009 rows travel the same seam the counts do — a future sqlite
        // entry that declares `source_roots` cannot lose them here.
        source_roots: probe.source_roots.clone(),
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
            presence_unknown: true,
            note: "registry has no entry for this platform — session count unknown (not 0)"
                .to_string(),
            ..default_footprint(name, fallback_root)
        };
    };
    let root = probe.root.clone().unwrap_or(fallback_root);
    if !probe.installed_p() {
        // W1006d: the lower-bound shape with no scanned root — the
        // measured primary was found ABSENT while a declared sibling
        // stayed unverified. The family is partially unknown, so this row
        // is presence-unknown (never a measured "not installed"), and the
        // session count is the probe's qualified floor: what the measured
        // roots hold (`≥0`), never a bare 0 that reads as the family
        // total. `doctor_consistency_test` joins this to the registry
        // probe row, so taking the count from the probe is what keeps the
        // two tables from contradicting each other.
        if !probe.unverified_roots.is_empty() {
            return HarnessFootprint {
                installed: false,
                presence_unknown: true,
                session_count: probe.record_count,
                candidate_count: probe.candidate_count,
                unreadable_count: probe.unreadable_count,
                unreadable_entry_count: probe.unreadable_entry_count,
                unverified_roots: probe
                    .unverified_roots
                    .iter()
                    .map(|root| root.id.clone())
                    .collect(),
                // W1009: a partially-unknown family still names its roots —
                // the unverified one's own row is where the mark lives now.
                source_roots: probe.source_roots.clone(),
                note: if probe.note.is_empty() {
                    "one or more declared source roots are unverified; total is a lower \
                     bound"
                        .to_string()
                } else {
                    probe.note.clone()
                },
                ..default_footprint(name, root)
            };
        }
        return HarnessFootprint {
            presence_unknown: probe.not_probed_p(),
            note: if probe.note.is_empty() {
                "registry did not scan this harness — session count unknown (not 0)".to_string()
            } else {
                probe.note.clone()
            },
            // W1009: carry the per-root rows for the data (a family whose
            // roots were all measured absent still has rows); the human
            // table decides which of them to print.
            source_roots: probe.source_roots.clone(),
            ..default_footprint(name, root)
        };
    }
    let mut footprint = coverage_from_records(name, root, recs);
    footprint.installed = true;
    footprint.source_roots = probe.source_roots.clone();
    footprint.candidate_count = probe.candidate_count;
    footprint.unreadable_count = probe.unreadable_count;
    footprint.unreadable_entry_count = probe.unreadable_entry_count;
    if probe.unreadable_count.is_some_and(|count| count > 0)
        || probe.unreadable_entry_count.is_some_and(|count| count > 0)
    {
        footprint.note = probe.note.clone();
    }
    // W1006d: the installed lower-bound shape (a scanned primary beside an
    // unverified sibling). The count stays the records' own — the same set
    // the probe counted, so the two tables agree by construction — and the
    // marking plus the note are what keeps it from reading as a family
    // total. The note wins over any unreadable wording above for the same
    // reason the count wins over silence: the qualification is the more
    // important of two true things this row has to say.
    if !probe.unverified_roots.is_empty() {
        footprint.unverified_roots = probe
            .unverified_roots
            .iter()
            .map(|root| root.id.clone())
            .collect();
        footprint.note = probe.note.clone();
    }
    footprint
}

fn default_footprint(name: &str, root: PathBuf) -> HarnessFootprint {
    HarnessFootprint {
        name: name.to_string(),
        root: Some(root),
        installed: false,
        presence_unknown: false,
        session_count: None,
        candidate_count: None,
        unreadable_count: None,
        unreadable_entry_count: None,
        total_bytes: None,
        unverified_roots: Vec::new(),
        source_roots: Vec::new(),
        time_basis: TimeBasis::Unmeasured,
        earliest: None,
        latest: None,
        compressed_count: 0,
        recognized_files: Vec::new(),
        note: "not installed".to_string(),
    }
}

/// Turn the D1/D2/D1½/D3 findings into a short list of "so what + when" lines.
///
/// `scan_failed` means the registry-driven scan could not run: the
/// claude/codex risk lines depend on session counts and are *omitted* rather
/// than fabricated from a bogus zero. `checks` carries the per-check
/// budgets that expired (W1043): every timed-out check's lines are replaced
/// by one that words the unknown, never a finding built from the
/// placeholder the run assembled to keep the report's shape.
fn build_risks(
    claude: &ClaudeCheck,
    gemini: &GeminiRetention,
    desktop_retention: &Option<DesktopRetentionCheck>,
    footprints: &[HarnessFootprint],
    scan_failed: bool,
    checks: &CheckRunner,
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
        // W1043: a scan that never answered is a different finding from a
        // registry that could not be parsed, and the reader who waited for
        // the budget deserves the sentence that names what happened.
        if let Some(secs) = checks.timed_out_after(D3_SCAN_CHECK) {
            risks.push(format!(
                "🔴 the harness scan did not answer within {secs}s — session coverage unknown; \
                 refusing to fake a full scan with hardcoded paths."
            ));
        } else {
            risks.push(
                "🔴 registry missing / unparseable — session coverage unknown; refusing to fake a full scan with hardcoded paths."
                    .to_string(),
            );
        }
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
            // W1043: the settings read never answered, so no layer was read
            // and no window — neither the 30-day default nor a configured
            // one — was measured. Saying "unset" or "parse failed" here
            // would each be a claim about files nobody read; the exact
            // budget is in the report's `timed_out` block and D1's line.
            ClaudeRetention::TimedOut => {
                risks.push(
                    "🔴 Claude Code: the settings read did not answer within its deadline — \
                     whether cleanupPeriodDays puts your sessions in a deletion window is \
                     UNKNOWN, not the 30-day default and not proof of safety. Re-run `doctor` \
                     once the files answer."
                        .to_string(),
                );
            }
        }
    }

    // --- Claude Desktop/Cowork retention (D1½ → D4, W1009/DSK-3) --------
    // Present only when the registry declares the probe — a registry that
    // declares none gets no line, which reads as "not checked here", never
    // as "no risk". The line speaks even when the scan failed, because the
    // settings files it reads are not scan-derived (the same standing the
    // Claude line above has within its own scan-dependent half).
    if let Some(check) = desktop_retention {
        risks.push(check.risk_line());
    }

    // --- Gemini -----------------------------------------------------------
    let gem_fp = footprints.iter().find(|f| f.name == "gemini");
    let gem_earliest = gem_fp.and_then(|f| f.earliest).map(format_date);
    let gem_days = gem_fp.and_then(|f| f.earliest).and_then(days_since);
    if let Some(secs) = checks.timed_out_after(D2_CHECK) {
        // W1043: same standing as the D1 `TimedOut` arm — a settings read
        // that never answered is not "unchanged" and not "the default", and
        // certainly not a policy the doctor may quote a window from.
        risks.push(format!(
            "🟡 Gemini: sessionRetention UNKNOWN — the settings read did not answer within \
             {secs}s. The retention risk is not being guessed from a default; re-run `doctor` \
             once the files answer."
        ));
    } else if let Some(error) = &gemini.unreadable {
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
    /// The configuration this report was measured from — the already-read
    /// result [`run`]'s own config check (W1043) produced behind its deadline,
    /// and the one config any caller layering a config-derived step on top of
    /// this report must reuse. `None` exactly when
    /// [`config_error`](Self::config_error) is `Some`: with no usable config
    /// there is nothing to reuse. Re-reading the file instead —
    /// [`Config::load`] is a deadline-less `fs::read_to_string` with no cache —
    /// would hand a wedged config read a second, unbounded chance to hang the
    /// command after the bounded first read already reported it timed out (the
    /// review's W1043f High), and would let a file edited mid-run disagree
    /// with the rest of the report. The CLI's destination probes
    /// ([`probe_destinations`]) read this field and nothing else, so
    /// `destinations` can only fill from a config the report itself says was
    /// read — `config_error`, `not_checked` and a measured destination can
    /// never tell three stories about one run.
    pub config: Option<crate::config::Config>,
    pub claude: ClaudeCheck,
    pub gemini: GeminiRetention,
    /// D1½ — the registry-declared `desktopSessionCleanupPeriodDays` probe
    /// (ADR-059 draft D5, W1009/DSK-3): the Desktop/Cowork retention state
    /// and the sources that decided it. `None` when this registry declares
    /// no such probe (so the check was not performed — never "performed and
    /// found nothing"); `Some` even on a config the run could not read,
    /// because the settings files it probes are not config-derived.
    pub desktop_retention: Option<DesktopRetentionCheck>,
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
    /// W1012 / SG-1 C1: the four-fact destination reconciliation — timer
    /// push coverage, timer-resolvable credentials, last recorded push,
    /// reclaim share — from [`crate::destdiag::reconcile`], which owns the
    /// wording all three surfaces share. `None` for the same reason as
    /// `keys`: every fact is derived from the config and the paths under
    /// its defaults.
    ///
    /// Unlike `destinations` (D7) this is filled by the library's own
    /// [`run()`]: it reads only files (timer unit files, state records),
    /// never the network, so the test-suite-visible entry point can keep
    /// producing it.
    pub dest_reconcile: Option<crate::destdiag::Reconciliation>,
    /// D13 — the local mirror's settled field family and worded rows
    /// (ADR-062 D3/D6.3, LM-4): size, last sync, snapshots behind,
    /// orphans. Built by [`crate::mirror::mirror_status`], the one
    /// function that owns the wording, so this page shows the same rows
    /// `status` prints and the menu bar displays. `None` for the same
    /// reason as `dest_reconcile`: `mirror_of`, the state records and
    /// the mirror directory all derive from a config this run could not
    /// read. Filled by the library's own [`run()`] — file reads and one
    /// directory walk only, never a destination connection.
    pub mirror: Option<crate::mirror::MirrorStatus>,
    /// W1043 — the checks that outlived their own deadline, in the order
    /// they ran. Not empty ⇒ each named check's section is an **unknown**:
    /// its field carries a placeholder (`None`, a `TimedOut` verdict, an
    /// empty collection, a `ScanReport::default`) that no reader may take
    /// as a measurement, the report prints a block saying so before any
    /// section, and the CLI exits 3 — the same "did not finish" family as
    /// [`Self::config_error`], never exit 0 for a half-inspected machine.
    pub timed_out: Vec<CheckTimedOut>,
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
    /// Interrupted-push debris at this destination (ISSUE-006 step A), by
    /// the shared classification. `None` means **not checked** for the same
    /// reasons [`Self::activity_index`] has; a checked-but-unestablished
    /// answer is the `NotChecked` variant with its reason, never a zero.
    pub debris: Option<DebrisProbe>,
}

/// What a connection classified as debris, or why it could not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebrisProbe {
    /// The survey finished and measured.
    Measured(Box<DebrisAtDestination>),
    /// The survey could not establish anything — the fail-closed convention:
    /// an unreadable key or an unfinished read is an unknown, never "no
    /// debris".
    NotChecked {
        /// Redacted reason, in the shape the rest of D7's lines carry.
        why: String,
    },
}

/// Interrupted-push debris at one declared destination, as the shared
/// classification measured it: torn and stranded report **separate** counts
/// and byte totals, and an unclassifiable pack counts as unknown — never as
/// debris and never as a zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebrisAtDestination {
    /// Torn debris: no index file names it and it cannot be read as a pack.
    /// It belongs to no backup and can be ignored
    /// ([`crate::debris::INTERRUPTED_PUSH_SENTENCE`] — the row's sentence).
    pub torn: usize,
    /// Backend bytes those torn packs occupy.
    pub torn_bytes: u64,
    /// Stranded content: complete, verified, adoptable by the next open.
    pub stranded: usize,
    /// Backend bytes those stranded packs occupy.
    pub stranded_bytes: u64,
    /// Packs whose bytes could not be classified, so their status is unknown.
    pub unknown: usize,
    /// Backend bytes those unknown packs occupy.
    pub unknown_bytes: u64,
}

impl DebrisAtDestination {
    /// From the read-only inventory `prune-orphans` computes — the same survey,
    /// so `doctor`'s lines and a pasted `prune-orphans` report describe the
    /// same measured set.
    #[must_use]
    pub fn from_inventory(inventory: &crate::prune_orphans::Inventory) -> Self {
        Self {
            torn: inventory.torn_packs(),
            torn_bytes: inventory.torn_bytes(),
            stranded: inventory.stranded_packs(),
            stranded_bytes: inventory.stranded_bytes(),
            unknown: inventory.unknown_packs(),
            unknown_bytes: inventory.unknown_bytes(),
        }
    }

    /// Whether there is any debris fact a D7 line owes the reader. A measured
    /// empty is silence, which the doctor's healthy-run convention keeps.
    #[must_use]
    pub fn has_anything_to_say(&self) -> bool {
        self.torn > 0 || self.stranded > 0 || self.unknown > 0
    }
}

/// Classify the debris at one declared destination's repository, read-only.
///
/// Only called once the destination answered and holds a repository; a key
/// that cannot be read or a survey that cannot finish is
/// [`DebrisProbe::NotChecked`] — the doctor's unknown, never a zero. The
/// costs track `prune-orphans`'s (one listing, one index pass, the
/// unindexed set only): a healthy destination adds nothing but those two
/// passes, and debris is the one reason a destination is worth this look.
fn probe_debris(cfg: &StoreConfig) -> DebrisProbe {
    let mk = match store::load_key_file(cfg) {
        Ok(mk) => mk,
        Err(error) => {
            return DebrisProbe::NotChecked {
                why: format!("cannot read the destination's masterkey: {error:#}"),
            };
        }
    };
    let backends = match crate::store::BackupStore::for_metadata_query(cfg.clone()).backends() {
        Ok(backends) => backends,
        Err(error) => {
            return DebrisProbe::NotChecked {
                why: crate::prune_orphans::redact(&error),
            };
        }
    };
    match crate::prune_orphans::survey(cfg, &backends, &mk) {
        Ok(inventory) => {
            DebrisProbe::Measured(Box::new(DebrisAtDestination::from_inventory(&inventory)))
        }
        Err(error) => DebrisProbe::NotChecked {
            why: crate::prune_orphans::error_line(&error),
        },
    }
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
/// repository", "the host did not answer", "this destination is not filled
/// in" and "the probe's own deadline expired first" are four different
/// findings, and only the first one is about the archive.
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
    /// The probe outlived its own deadline (W1043): the connection neither
    /// completed nor failed within it, so the destination's reachability
    /// **and** its repository are unknown. Deliberately its own variant and
    /// not [`Self::Unreachable`]: unreachable is a *measured* "the host
    /// said no", while this is "my deadline expired first" — collapsing the
    /// two words a decision about the archive from a decision about the
    /// clock.
    ProbeTimedOut { seconds: u64 },
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
pub fn destination_store_config(
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

/// The store configuration of a machine with no `[destinations.<name>]` —
/// the pre-ADR-013 single-repository mode, defined **once** because three
/// hand-kept copies of its defaults are how the CLI and the diagnostics
/// drift (W1034f2, rev4 Low 1): the CLI's `store_config_from` layers its
/// CLI-argument expansion over each config-sourced half and falls back to
/// this for every *unset* half, and `destdiag`'s destination proof reads
/// exactly this for the destination-less machine, so a change to the
/// defaults can never again move one reader and not the other.
///
/// The config-sourced values arrive already `~`-expanded at load
/// (`config::expand_and_verify`); this builder performs no expansion and no
/// I/O of its own.
pub fn home_repository_store_config(config: &Config) -> StoreConfig {
    let data_root = default_data_root();
    StoreConfig {
        repo_root: config
            .rustic_repo
            .clone()
            .unwrap_or_else(|| data_root.join("repo").to_string_lossy().into_owned()),
        key_file: config
            .rustic_key_file
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| data_root.join("masterkey.json")),
        connections: 0,
        options: std::collections::BTreeMap::new(),
        cache_dir: config.rustic_cache_dir.as_deref().map(PathBuf::from),
        // reason: rustic_no_cache unset means rustic's own default (cache
        // on) — a config default, not an unknown read result collapsed to
        // false
        no_cache: config.rustic_no_cache.unwrap_or(false),
    }
    .with_capped_connections(config.rustic_connections)
}

/// Connect once to every declared destination, read-only.
///
/// Deliberately not called from [`run()`]: `run()` is a library entry point
/// that a dozen integration tests call, and an SFTP connect is not something a
/// test may do. The real connection belongs to the CLI, so `cmd_doctor` fills
/// the report in. The probe only ever lists — it creates nothing, so running
/// `doctor` can never be the reason a repository appears.
///
/// W1043: each of the three reads per destination (the connection, the
/// activity indexes, the debris survey) runs behind its own deadline, so a
/// destination that accepts the connection and then answers nothing can no
/// longer freeze the whole report. A check that outlives its budget is
/// abandoned and reported as an unknown ([`DestinationOutcome::ProbeTimedOut`]
/// for the connection, a `timed out` note in the returned list for the two
/// reads that only make sense once it was reachable); the destinations that
/// did answer are probed and reported as usual.
pub fn probe_destinations(config: &Config) -> (Vec<DestinationProbe>, Vec<CheckTimedOut>) {
    let mut checks = CheckRunner::new();
    let probes = config
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
                    debris: None,
                };
            };
            let repo_root = cfg.repo_root.clone();
            let connect_check = format!("D7 destination probe ({name}) — connection");
            let activity_check = format!("D7 destination probe ({name}) — activity index");
            let debris_check = format!("D7 destination probe ({name}) — debris");
            // The classification runs on the worker so the caller only ever
            // sees the classified shapes: `Ok(exists)` is a measured answer,
            // `Err((kind, detail))` carries the classifier's verdict and
            // the formatted chain, and `None` is the deadline — the only
            // thing this step's `run` may add on top of the raw probe.
            let reached = {
                let cfg = cfg.clone();
                checks.run(&connect_check, DESTINATION_CONNECT_BUDGET_SECS, move || {
                    crate::remote_err::probe_destination_connectivity(&cfg).map_err(|e| {
                        (
                            crate::remote_err::classify_error_str(&format!("{e:#}")),
                            crate::remote_err::format_remote_error("doctor", &e, &cfg),
                        )
                    })
                })
            };

            // The outcome, with the timeout its own state: a deadline that
            // expired is unknown, not a refused connection. The gate below
            // is taken before the match because the match may move the
            // probe's error fields into the outcome.
            let repository_available = matches!(reached, Some(Ok(true)));
            let outcome = match reached {
                Some(Ok(exists)) => DestinationOutcome::Reached {
                    repository_exists: exists,
                },
                Some(Err((kind, detail))) => DestinationOutcome::Unreachable { kind, detail },
                // The seconds the runner itself recorded for this check's
                // note (W1043f): one source for the number, not a second
                // re-derivation of the override — so the printed line, the
                // JSON and the `timed_out` note can never disagree about
                // what the budget was.
                None => DestinationOutcome::ProbeTimedOut {
                    seconds: match checks.timed_out_after(&connect_check) {
                        Some(secs) => secs,
                        None => std::unreachable!(
                            "a connect-check timeout on its own label always records its \
                             budget — the same `run` call that returned None pushed the note"
                        ),
                    },
                },
            };

            // The two reads that only mean something once the destination
            // answered and holds a repository; with none, "fresh" would be a
            // statement about an archive that does not exist. `None` keeps
            // meaning "not checked", and a timed-out read leaves it that way
            // while the note in `checks` says why.
            let activity_index = if repository_available {
                let cfg = cfg.clone();
                let name = name.clone();
                checks.run(&activity_check, DESTINATION_READ_BUDGET_SECS, move || {
                    probe_activity_index(&cfg, &name)
                })
            } else {
                None
            };
            // Debris needs the same two things the activity check does — a
            // repository and its key — and it is the same read-only family:
            // D7 already dialed the destination to say "reachable", and the
            // debris it can leave behind is the question an interrupted
            // push makes worth an answer.
            let debris = if repository_available {
                let cfg = cfg.clone();
                checks.run(&debris_check, DESTINATION_READ_BUDGET_SECS, move || {
                    probe_debris(&cfg)
                })
            } else {
                None
            };
            DestinationProbe {
                name: name.clone(),
                repo_root,
                outcome,
                activity_index,
                debris,
            }
        })
        .collect();
    (probes, checks.notes().to_vec())
}

/// JSON shape for one destination probe.
fn destination_probe_json(p: &DestinationProbe) -> serde_json::Value {
    let debris = match &p.debris {
        None => serde_json::Value::Null,
        Some(DebrisProbe::Measured(debris)) => serde_json::json!({
            "kind": "measured",
            "torn": debris.torn,
            "torn_bytes": debris.torn_bytes,
            "stranded": debris.stranded,
            "stranded_bytes": debris.stranded_bytes,
            "unknown": debris.unknown,
            "unknown_bytes": debris.unknown_bytes,
            "torn_sentence": crate::debris::INTERRUPTED_PUSH_SENTENCE,
            // Every candidate's earliest-seen time: the backend interface
            // carries no modification time, and this document never
            // fabricates a clock (ISSUE-006 step A).
            "first_seen": {"kind": "unknown", "why": crate::debris::NO_MTIME_SENTENCE},
        }),
        Some(DebrisProbe::NotChecked { why }) => {
            serde_json::json!({"kind": "not_checked", "why": why})
        }
    };
    match &p.outcome {
        DestinationOutcome::Reached { repository_exists } => serde_json::json!({
            "name": p.name,
            "repo_root": p.repo_root,
            "kind": if *repository_exists { "repository_present" } else { "repository_absent" },
            // `null` when nothing was checked (no repository yet, or the
            // connection failed) — a script must not read "not checked" as
            // "every index is current".
            "activity_index": p.activity_index.as_ref().map(activity_index_json),
            // `null` for the same "not checked" reasons above; a checked
            // answer never folds an unknown into a zero.
            "debris": debris,
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
        DestinationOutcome::ProbeTimedOut { seconds } => serde_json::json!({
            "name": p.name,
            "repo_root": p.repo_root,
            "kind": "probe_timed_out",
            // Not `unreachable`, which is a measured answer: the probe's
            // deadline expired with no answer in either direction, so both
            // this destination's reachability and its repository are
            // unknown; `activity_index` and `debris` above are `null` for
            // the same reason — not checked, never "fine".
            "budget_secs": seconds,
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
const CHECKS_NEEDING_CONFIG: [&str; 12] = [
    "D3 harness scan, footprints and archive gaps",
    "D4 risk summary",
    "D5 repository reclaim",
    "D6 local metadata cache",
    "D9 body cache",
    "D10 full-text indexes",
    "D7 destination probes",
    "D8 native host stage",
    "D11 stage duplicate scan",
    "D12 destination reconciliation (timers, credentials, last push, reclaim)",
    "D13 local mirror (size, last sync, snapshots behind, orphans)",
    "machine identity",
];

/// The report for a config file that exists and cannot be used.
///
/// Deliberately **not** a diagnosis: it carries the checks that read no
/// config (D1 Claude settings, D2 Gemini settings, and the directories
/// merely listed) plus the reason, and says out loud which checks were
/// skipped. Every config-derived field is left empty, except that an
/// unexpandable `rustic_cache_dir` gets a specific unavailable result so the
/// user knows which configured path failed. `config_error` tells consumers
/// that all other empty config-derived fields mean "did not look" — this is
/// the whole reason `doctor` is the one command that does not refuse: the
/// user needs to be told *which* half of their setup is broken, and that
/// answer is worthless if it is dressed up as a healthy machine.
///
/// W1043: takes the D1/D2 results [`run`] already measured (behind their own
/// deadlines) rather than re-reading the same files unbounded, and keeps
/// filling `timed_out` for the checks that run from here, so a wedged
/// settings file cannot turn the *broken-config* answer into a hang either.
pub fn config_unreadable(
    mut checks: CheckRunner,
    error: String,
    claude: ClaudeCheck,
    gemini: GeminiRetention,
) -> DoctorReport {
    let home = crate::config::home_dir();
    let other_present = {
        let home = home.clone();
        checks
            .run(OTHER_HARNESS_CHECK, SETTINGS_BUDGET_SECS, move || {
                OTHER_HARNESS_DIRS
                    .iter()
                    .filter(|d| home.join(d).is_dir())
                    .map(|d| home.join(d))
                    .collect::<Vec<_>>()
            })
            // reason: same placeholder rule as the settings pair above —
            // the `timed_out` note qualifies it.
            .unwrap_or_default()
    };
    let cache = error.contains("rustic_cache_dir:").then(|| CacheCheck::Unavailable {
        detail: "`rustic_cache_dir` could not be expanded; fix that value in the config and run `chat-stasher doctor` again".to_string(),
    });
    // D1½ reads the same class of file D1 does, through the registry's
    // declared probe locations, so a broken config does not silence it
    // either (W1009/DSK-3) — and its own deadline keeps a wedged settings
    // file from hanging the broken-config answer.
    let desktop_retention = {
        let claude = claude.clone();
        checks
            .run(DESKTOP_RETENTION_CHECK, SETTINGS_BUDGET_SECS, move || {
                scanner::load_registry_from_repo()
                    .ok()
                    .and_then(|registry| inspect_desktop_retention(&registry, &claude))
            })
            .flatten()
    };
    DoctorReport {
        config_source: crate::config::ConfigSource::Unreadable,
        config_error: Some(error),
        // W1043f: nothing was read, so there is nothing to reuse — a report
        // whose config check never answered carries no config, and the CLI
        // must not substitute one by reading the file again.
        config: None,
        claude,
        gemini,
        desktop_retention,
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
        // W1012 / SG-1 C1: same source (the config every destination name
        // and credential reference comes from), same "not computed".
        dest_reconcile: None,
        // LM-4 / ADR-062 D13: every mirror fact is config-derived (the
        // mirrored destination's name, its state records' location, the
        // mirror directory under the data root), so an unreadable config
        // means this check did not run.
        mirror: None,
        timed_out: checks.notes().to_vec(),
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
///
/// The scan's fields are taken by value so a W1043 check can run the
/// computation on a worker thread (the scanner's report is not `Clone`, and
/// these fields are the whole of its dependence on it).
fn machine_log_rows_from(
    config: Config,
    machine_logs: Vec<scanner::MachineLogRecord>,
    unlooked: usize,
    indeterminate: usize,
    unlooked_reasons: Vec<&'static str>,
) -> MachineLogRows {
    let stage = config
        .native_host
        .as_ref()
        .and_then(|native| native.stage.as_deref())
        .map(expand_tilde)
        .filter(|root| root.is_dir());
    let machine = crate::id::machine_id();
    let mut rows = MachineLogRows {
        present: Vec::new(),
        unlooked,
        indeterminate,
        unlooked_reasons,
    };
    for record in &machine_logs {
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

/// The machine-log rows of the report, for both channels.
///
/// A wedged machine-log check (W1043f2) prints its own sub-block line — the
/// same UNKNOWN idiom every timed-out section gets — because the rows the
/// runner would have built are the default empty set, which on paper reads
/// as "no logs exist": the one state that must never be worded as a
/// measurement. With no deadline note the old shapes stand: truly empty
/// rows stay silent under the coverage table, and non-empty rows print.
fn print_machine_logs(rows: &MachineLogRows, timed_out_after_secs: Option<u64>) {
    if let Some(secs) = timed_out_after_secs {
        print_timed_out_section(
            "  machine logs (per-machine input logs, ADR-053 — not sessions, never counted in the table above)",
            MACHINE_LOGS_CHECK,
            secs,
            "the machine-log rows are unknown, not empty",
        );
        return;
    }
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

    /// The budget that expired for `check`, when that check outlived its
    /// deadline (W1043) — the one place a printer asks "did this check
    /// answer?" so every renderer words the same timeout the same way.
    pub fn check_timed_out_after(&self, check: &str) -> Option<u64> {
        self.timed_out
            .iter()
            .find(|note| note.check == check)
            .map(|note| note.budget_secs)
    }
}

/// Run every check against the real machine and assemble the report.
///
/// W1043: every check runs through [`CheckRunner`] — its trace is on stderr
/// before it starts, and it answers within its own budget or is abandoned
/// with a [`CheckTimedOut`] note in [`DoctorReport::timed_out`]. A report
/// with a non-empty `timed_out` carries placeholders in exactly the sections
/// its notes name; the notes are the one place that says so, and the CLI
/// turns them into the same exit 3 that an unreadable config gets — never a
/// clean-looking 0 for a machine only half of which was inspected.
///
/// W1043f: the config the run read rides out in
/// [`DoctorReport::config`], so the CLI's config-derived steps (the
/// destination probes) reuse the bounded read instead of issuing a second
/// deadline-less one.
pub fn run() -> DoctorReport {
    let mut checks = CheckRunner::new();
    let home = crate::config::home_dir();

    // D1 (measured even later against a broken config: these settings files
    // are not config-derived).
    let claude = {
        let home = home.clone();
        checks
            .run(D1_CHECK, SETTINGS_BUDGET_SECS, move || {
                inspect_claude_settings(&home)
            })
            // reason: the `TimedOut` verdict is not a placeholder that can
            // be misread — it owns every word every renderer gives it, and
            // `layers` stays empty because no layer was read.
            .unwrap_or(ClaudeCheck {
                layers: Vec::new(),
                verdict: ClaudeRetention::TimedOut,
            })
    };

    // D2 — on a timeout the synthesized `unreadable` field keeps any path
    // that somehow reads the struct from seeing a policy; the guarded
    // renderers (D2's line, D4, `gemini_json`) word it themselves.
    let gemini = {
        let home = home.clone();
        checks
            .run(D2_CHECK, SETTINGS_BUDGET_SECS, move || {
                inspect_gemini_settings(&home)
            })
            // reason: the defaults are the struct's own (never shown: every
            // renderer of `gemini` checks `timed_out` first); `unreadable` is
            // the honest unknown, not a policy.
            .unwrap_or_else(|| GeminiRetention {
                enabled: GeminiRetention::default().enabled,
                max_age: GeminiRetention::default().max_age,
                min_retention: GeminiRetention::default().min_retention,
                unreadable: Some(
                    "the settings read did not answer within its deadline".to_string(),
                ),
            })
    };

    // The config gates more than half of this report, so an unusable one is an
    // early, explicit answer rather than a report about a machine the tool could
    // not see. Filling the config-derived sections from `Config::default()`
    // would print "no destination declared", "no repository", "no stage" —
    // findings about a config nobody read.
    let config = match checks.run(CONFIG_CHECK, CONFIG_BUDGET_SECS, Config::load) {
        Some(Ok(config)) => config,
        Some(Err(e)) => return config_unreadable(checks, format!("{e:#}"), claude, gemini),
        None => {
            // The seconds the runner itself recorded for this check's note
            // (W1043f's D7 precedent, applied to the config site the round
            // after the review flagged it): one source for the number, not a
            // second re-derivation of the env override — so the sentence,
            // the `timed_out` note and any later reader of the budget can
            // never disagree about what this run waited.
            let secs = match checks.timed_out_after(CONFIG_CHECK) {
                Some(secs) => secs,
                None => std::unreachable!(
                    "a config-check timeout on its own label always records its \
                     budget — the same `run` call that returned None pushed the note"
                ),
            };
            let error = format!(
                "reading the configuration did not answer within {secs}s — the file exists \
                 but was never read to the end, so whether it is usable is UNKNOWN, not \
                 invalid"
            );
            return config_unreadable(checks, error, claude, gemini);
        }
    };

    // D3 — use the registry-driven scanner for every directory harness. The
    // doctor no longer has a second Gemini suffix/pattern implementation.
    // The registry is loaded once and shared with the D1½ retention probe
    // (W1009/DSK-3): the scan that failed is the same registry the probe
    // would have read, so the two checks can never disagree about what the
    // registry declares.
    let (registry, scan, scan_failed) = {
        let config = config.clone();
        checks
            .run(
                D3_SCAN_CHECK,
                SCAN_BUDGET_SECS,
                move || match scanner::load_registry_from_repo() {
                    Ok(registry) => match scanner::scan_with_registry(&config, &registry) {
                        Ok(s) => (Some(registry), s, false),
                        Err(e) => {
                            eprintln!("doctor: scan failed: {e}");
                            (Some(registry), scanner::ScanReport::default(), true)
                        }
                    },
                    Err(e) => {
                        eprintln!("doctor: scan failed: {e}");
                        (None, scanner::ScanReport::default(), true)
                    }
                },
            )
            // reason: on a timeout the scan never answered, so the honest
            // shape is exactly this — no registry, no report, scan_failed
            // true — while the `timed_out` note keeps the placeholder from
            // being read as "no registry is declared".
            .unwrap_or((None, scanner::ScanReport::default(), true))
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
                // No probe row: the registry in effect has no entry
                // for this harness, so its presence was never
                // established — the same skip shape as a `skip_*`
                // probe, not a measured absence.
                footprints.push(HarnessFootprint {
                    presence_unknown: true,
                    note:
                        "registry has no probe row for this harness — session count unknown (not 0)"
                            .to_string(),
                    ..default_footprint(id, root)
                });
            }
        }
    }

    let other_present = {
        let home = home.clone();
        checks
            .run(OTHER_HARNESS_CHECK, SETTINGS_BUDGET_SECS, move || {
                OTHER_HARNESS_DIRS
                    .iter()
                    .filter(|d| home.join(d).is_dir())
                    .map(|d| home.join(d))
                    .collect::<Vec<_>>()
            })
            // reason: an empty list under a `timed_out` note is "did not
            // look", and the report's block says so — silent absence is the
            // fail-open this note must prevent.
            .unwrap_or_default()
    };

    // D1½ — the Desktop/Cowork retention state, from the same registry the
    // scan ran with. `None` only when that registry declares no probe or
    // could not be read — in which shape `scan_failed` is true too (or the
    // config is unreadable, which `config_unreadable` handles), and the
    // report says the check was not performed rather than finding nothing.
    let desktop_retention = match registry.clone() {
        Some(registry) => {
            let claude = claude.clone();
            checks
                .run(DESKTOP_RETENTION_CHECK, SETTINGS_BUDGET_SECS, move || {
                    inspect_desktop_retention(&registry, &claude)
                })
                .flatten()
        }
        // reason: no registry to probe against — the check was not
        // performed, which `None` is the established shape for.
        None => None,
    };

    // D4
    let risks = build_risks(
        &claude,
        &gemini,
        &desktop_retention,
        &footprints,
        scan_failed,
        &checks,
    );

    // D5
    let reclaim = {
        let config = config.clone();
        checks.run(RECLAIM_CHECK, RECLAIM_BUDGET_SECS, move || {
            inspect_reclaim(&config)
        })
    };

    // D6
    let cache = {
        let config = config.clone();
        checks.run(CACHE_CHECK, BACKED_WALK_BUDGET_SECS, move || {
            inspect_cache(&config)
        })
    };

    // D9
    let body_cache = {
        let config = config.clone();
        checks.run(BODY_CACHE_CHECK, BACKED_WALK_BUDGET_SECS, move || {
            inspect_body_cache(&config)
        })
    };

    // D10 — local-only, no repository connection and no document payload reads.
    let fts_indexes = {
        let config = config.clone();
        checks
            .run(FTS_CHECK, BACKED_WALK_BUDGET_SECS, move || {
                Some(inspect_fts_indexes(&config))
            })
            .flatten()
    };

    // D11 — inspect only the stage the user explicitly configured for the
    // browser host. The check reads shard bytes and reports counts only.
    let stage_duplicate_shards = {
        let config = config.clone();
        checks.run(STAGE_DUP_CHECK, BACKED_WALK_BUDGET_SECS, move || {
            inspect_stage_duplicates(&config)
        })
    };

    // D8 — read-only, opens nothing but the manifests themselves. The root is
    // the machine's, resolved here because this is the one caller that means the
    // real machine: `%LOCALAPPDATA%` on Windows, and `home` everywhere else.
    let native_host = {
        let config = config.clone();
        let native_host_root =
            crate::nativehost::machine_root(crate::nativehost::Platform::current(), &home);
        checks.run(NATIVE_HOST_CHECK, BACKED_WALK_BUDGET_SECS, move || {
            inspect_native_host(&config, &native_host_root)
        })
    };

    // ADR-053 D4 — measured read-only, and reported in its own rows.
    let machine_logs = {
        let config = config.clone();
        let machine_logs_declared = scan.machine_logs.clone();
        let unlooked = scan.machine_logs_unlooked;
        let indeterminate = scan.machine_logs_indeterminate;
        let unlooked_reasons = scan.machine_logs_unlooked_reasons.clone();
        checks
            .run(MACHINE_LOGS_CHECK, BACKED_WALK_BUDGET_SECS, move || {
                machine_log_rows_from(
                    config,
                    machine_logs_declared,
                    unlooked,
                    indeterminate,
                    unlooked_reasons,
                )
            })
            // reason: the default rows carry no presence claims at all —
            // zero unlooked here would otherwise read as "there are none",
            // which the `timed_out` block is the qualifier for.
            .unwrap_or_default()
    };
    let probes = scan.probes;

    // W1012 / SG-1 C1 — the four-fact destination reconciliation, filled here
    // rather than in the CLI because every input is a file read (the
    // installed timer unit files, the state records the product itself
    // writes, and — for W1034's upgrade-window proof — the snapshot
    // listing of a repository on this machine's own *un-mounted*
    // filesystem: no destination is dialed, and no mount root —
    // `/Volumes/…` among them — is read either (the gate
    // [`crate::store::repository_is_local`] carries both halves), so the
    // test-suite-visible entry point can keep producing it. The platform's
    // format follows the same split `setup_schedule_format` makes in the
    // CLI.
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        // reason: NOT an honest default — the same (a)-fragile
        // pre-1970-clock branch `runstate::now_unix` documents. Reached
        // only by a clock before the epoch; the wrong stamp lands in a
        // staleness age so the reconciliation would report the records as
        // ancient rather than as fresh.
        .unwrap_or(0);
    let dest_reconcile = {
        let config = config.clone();
        let home = home.clone();
        let now = now_unix;
        checks.run(DEST_RECONCILE_CHECK, BACKED_WALK_BUDGET_SECS, move || {
            crate::destdiag::reconcile(&crate::destdiag::Inputs {
                config: &config,
                format: match cfg!(target_os = "macos") {
                    true => crate::schedule::Format::Launchd,
                    false => crate::schedule::Format::Systemd,
                },
                home: &home,
                state_dir: &crate::collect::default_state_dir(),
                now_unix: now,
                // The lib-side half of the same platform fact the CLI decides
                // through `schedule_platform_refusal`: Windows schedules through a
                // Task Scheduler this build has no reader for, so the
                // reconciliation must not read any directory's silence there as
                // "no timer pushes". `schedule::scheduler_readable_on` is the one
                // place this fact lives, so the reader's capability and the test's
                // branch cannot drift apart the way three hand-rolled copies did.
                scheduler_readable: crate::schedule::scheduler_readable_on(std::env::consts::OS),
                // The one stage the config itself names — expanded at load, the
                // same source the D11 duplicate scan reads, so the affected
                // ADR-060 D5 cap fact and the duplicate scan walk one tree.
                stage: config
                    .native_host
                    .as_ref()
                    .and_then(|native_host| native_host.stage.as_deref())
                    .map(std::path::Path::new),
            })
        })
    };

    // D13 (ADR-062 D3/D6.3, LM-4): the mirror's settled field family, from
    // the one function that owns the wording all three surfaces share —
    // the same `now` the reconciliation used, so a printed age in the
    // steady row agrees with every other age this report words. Reads the
    // state records and walks the mirror directory; no destination is
    // dialed.
    let mirror = {
        let config = config.clone();
        let now = now_unix;
        checks.run(MIRROR_CHECK, BACKED_WALK_BUDGET_SECS, move || {
            crate::mirror::mirror_status(
                &config,
                &crate::config::default_data_root(),
                &crate::collect::default_state_dir(),
                now,
            )
        })
    };

    let keys = {
        let config = config.clone();
        checks.run(KEYS_CHECK, BACKED_WALK_BUDGET_SECS, move || {
            key_inventory(&config)
        })
    };

    DoctorReport {
        config_source: config.source,
        config_error: None,
        // W1043f — the one config this report was measured from, handed to
        // the caller so no config-derived step has to re-read the file
        // unbounded. `ConfigSource` is `Copy`, so reading `config.source`
        // above moved nothing out.
        config: Some(config),
        claude,
        gemini,
        desktop_retention,
        footprints,
        other_present,
        risks,
        reclaim,
        cache,
        body_cache,
        fts_indexes,
        probes,
        archive_gaps,
        machine_logs,
        scan_failed,
        // Filled in by the CLI: connecting to a destination is a real network
        // action and this entry point is called directly by the test suite.
        destinations: Vec::new(),
        stage_duplicate_shards,
        native_host,
        keys,
        dest_reconcile,
        mirror,
        timed_out: checks.notes().to_vec(),
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
        // W1043: the checks that outlived their own deadline. Not empty ⇒ the
        // named checks' sections below carry placeholders, exactly as under
        // `config_error`: a script must read an absent/null section as "did
        // not look", never as a clean zero, and the named checks are why.
        "timed_out": r.timed_out.iter().map(CheckTimedOut::json).collect::<Vec<_>>(),
        "claude": match r.check_timed_out_after(D1_CHECK) {
            // The synthesized `TimedOut` verdict would be a measurement
            // about layers nobody read; the note above says the unknown.
            Some(secs) => serde_json::json!({
                "checked": false,
                "timed_out_after_secs": secs,
            }),
            None => claude_json(&r.claude),
        },
        "gemini": match r.check_timed_out_after(D2_CHECK) {
            Some(secs) => serde_json::json!({
                "checked": false,
                "timed_out_after_secs": secs,
            }),
            None => gemini_json(&r.gemini),
        },
        // W1009/DSK-3: the Desktop/Cowork retention probe, additive in the
        // same `checked` style as `keys` — `checked: false` means this run
        // never looked (the registry declares no probe / could not be read),
        // which no state value could say by itself.
        "desktop_retention": match (&r.desktop_retention, r.check_timed_out_after(DESKTOP_RETENTION_CHECK)) {
            (Some(check), _) => desktop_retention_json(check),
            (None, Some(secs)) => serde_json::json!({"checked": false, "timed_out_after_secs": secs}),
            (None, None) => serde_json::json!({"checked": false}),
        },
        "footprints": r.footprints.iter().map(footprint_json).collect::<Vec<_>>(),
        // ADR-053 D4: machine logs are a product of their own, so they get
        // their own object rather than a row in `footprints`. A count that was
        // not measured is a tri-state, never a 0.
        "machine_logs": match r.check_timed_out_after(MACHINE_LOGS_CHECK) {
            // The default rows would carry "0 unlooked" as a measured count;
            // the note above owns the unknown instead.
            Some(secs) => serde_json::json!({
                "checked": false,
                "timed_out_after_secs": secs,
            }),
            None => machine_logs_json(&r.machine_logs),
        },
        "other_present": r.other_present.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "risks": r.risks.iter().map(|text| risk_json(text)).collect::<Vec<_>>(),
        "reclaim": match (&r.reclaim, r.check_timed_out_after(RECLAIM_CHECK)) {
            (Some(reclaim), _) => reclaim_json(reclaim),
            // Same idiom as `native_host` below: not "no repository", but "this
            // run did not look", which is a different finding. With a deadline
            // note (W1043f) the section also says when it walked away.
            (None, Some(secs)) => {
                serde_json::json!({"checked": false, "timed_out_after_secs": secs})
            }
            (None, None) => serde_json::json!({"checked": false}),
        },
        "cache": match (&r.cache, r.check_timed_out_after(CACHE_CHECK)) {
            (Some(cache), _) => cache_json(cache),
            (None, Some(secs)) => {
                serde_json::json!({"checked": false, "timed_out_after_secs": secs})
            }
            (None, None) => serde_json::json!({"checked": false}),
        },
        "body_cache": match (&r.body_cache, r.check_timed_out_after(BODY_CACHE_CHECK)) {
            (Some(body_cache), _) => body_cache_json(body_cache),
            (None, Some(secs)) => {
                serde_json::json!({"checked": false, "timed_out_after_secs": secs})
            }
            (None, None) => serde_json::json!({"checked": false}),
        },
        "fts_indexes": match (&r.fts_indexes, r.check_timed_out_after(FTS_CHECK)) {
            (Some(indexes), _) => serde_json::json!({
                "checked": true,
                "destinations": indexes.iter().map(|index| serde_json::json!({
                    "destination": index.destination,
                    "state": index.state,
                    "documents": index.documents,
                })).collect::<Vec<_>>(),
            }),
            (None, Some(secs)) => {
                serde_json::json!({"checked": false, "timed_out_after_secs": secs})
            }
            (None, None) => serde_json::json!({"checked": false}),
        },
        "archive_gaps": r.archive_gaps.iter().map(archive_gap_json).collect::<Vec<_>>(),
        "probes": r.probes.iter().map(scanner::probe_json).collect::<Vec<_>>(),
        "destinations": r.destinations.iter().map(destination_probe_json).collect::<Vec<_>>(),
        "stage_duplicate_shards": match (&r.stage_duplicate_shards, r.check_timed_out_after(STAGE_DUP_CHECK)) {
            (Some(check), _) => serde_json::json!({
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
            (None, Some(secs)) => {
                serde_json::json!({"checked": false, "timed_out_after_secs": secs})
            }
            // Not "there are no duplicates": this run could not read the config
            // the stage comes from, so it never looked.
            (None, None) => serde_json::json!({"checked": false}),
        },
        "keys": match (&r.keys, r.check_timed_out_after(KEYS_CHECK)) {
            (Some(rows), _) => serde_json::json!({
                "checked": true,
                "copies": rows.iter().map(key_inventory_json).collect::<Vec<_>>(),
            }),
            // Not "there are no keys" and not "checked and none were found":
            // with a deadline note this run walked away from the inventory
            // walk (W1043f); without one it could not read the config the
            // inventory is derived from, so it never looked.
            (None, Some(secs)) => {
                serde_json::json!({"checked": false, "timed_out_after_secs": secs})
            }
            (None, None) => serde_json::json!({"checked": false}),
        },
        // W1012 / SG-1 C1: the four facts and the shared alert wording, in
        // the same tag-first shape the rest of this document uses. `checked:
        // false` is the did-not-look case, distinct from every "checked and
        // the answer is" kind inside.
        "destination_reconciliation": match (&r.dest_reconcile, r.check_timed_out_after(DEST_RECONCILE_CHECK)) {
            (Some(reconciliation), _) => {
                let value = crate::destdiag::destinations_json(reconciliation);
                serde_json::json!({
                    "checked": true,
                    "destinations": value["destinations"],
                    "alerts": value["alerts"],
                    "reclaim": value["reclaim"],
                    // ADR-060 D5: the stage-cap fact rides beside the
                    // facts — measured / unknown, never a zero-shaped
                    // silence, the same shape `status --json` prints.
                    "stage_cap": value["stage_cap"],
                })
            }
            (None, Some(secs)) => {
                serde_json::json!({"checked": false, "timed_out_after_secs": secs})
            }
            (None, None) => serde_json::json!({"checked": false}),
        },
        "native_host": match (&r.native_host, r.check_timed_out_after(NATIVE_HOST_CHECK)) {
            (Some(check), _) => native_host_json(check),
            // Not the same as "there is no registration": this run did not look.
            (None, Some(secs)) => {
                serde_json::json!({"checked": false, "timed_out_after_secs": secs})
            }
            (None, None) => serde_json::json!({"checked": false}),
        },
        // LM-4 / ADR-062 D13: the mirror's settled field family in the same
        // tagged-`kind` shape `status --json`'s `local.mirror` carries, so
        // the two objects can never disagree about a mirror; `checked:
        // false` is the did-not-look case on the config-unreadable path,
        // and carries the walked-away-at seconds when the walk wedged.
        "mirror": match (&r.mirror, r.check_timed_out_after(MIRROR_CHECK)) {
            (Some(status), _) => serde_json::json!({
                "checked": true,
                "configured": status.configured,
                "enabled": status.enabled,
                "mirror_of": status.mirror_of,
                "declared": status.declared,
                "size": status.size,
                "last_sync": status.last_sync,
                "snapshots_behind": status.snapshots_behind,
                "orphans": status.orphans,
                "rows": status.rows.iter().map(|row| serde_json::json!({
                    "severity": row.severity,
                    "sentence": row.sentence,
                })).collect::<Vec<_>>(),
            }),
            (None, Some(secs)) => {
                serde_json::json!({"checked": false, "timed_out_after_secs": secs})
            }
            (None, None) => serde_json::json!({"checked": false}),
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
        ClaudeRetention::TimedOut => serde_json::json!({"kind": "timed_out"}),
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

/// The `desktop_retention` object of `doctor --json` (W1009/DSK-3). Every
/// source the probe consulted is a row whose outcome *is* its state, and the
/// merged verdict is tagged so a script can never read `unbounded` as
/// `bounded` or a managed override as a user choice. `effective_days` is
/// the state's own answer to "how long do Desktop/Cowork transcripts
/// survive": `unbounded` carries the never-a-number shapes, and a `null`
/// inside `ceiling` means the longer-of-two comparison itself is unknown
/// (D1 could not classify the CLI side), never the absence of a cap.
fn desktop_retention_json(check: &DesktopRetentionCheck) -> serde_json::Value {
    let sources: Vec<serde_json::Value> = check
        .sources
        .iter()
        .map(|source| {
            let outcome = match &source.outcome {
                DesktopSourceOutcome::Absent => serde_json::json!({"kind": "absent"}),
                DesktopSourceOutcome::Unreadable(e) => {
                    serde_json::json!({"kind": "unreadable", "why": e})
                }
                DesktopSourceOutcome::ParseFailed(e) => {
                    serde_json::json!({"kind": "parse_failed", "why": e})
                }
                DesktopSourceOutcome::InvalidValue(w) => {
                    serde_json::json!({"kind": "invalid_value", "why": w})
                }
                DesktopSourceOutcome::Read {
                    desktop_days,
                    cleanup_days,
                } => serde_json::json!({
                    "kind": "read",
                    "desktop_session_cleanup_period_days": desktop_days,
                    "cleanup_period_days": cleanup_days,
                }),
            };
            serde_json::json!({
                "path": source.path.display().to_string(),
                "kind": source.kind.word(),
                "outcome": outcome,
            })
        })
        .collect();
    let state = match &check.state {
        DesktopRetention::DefaultZero => serde_json::json!({
            "kind": "default_zero_unbounded",
            "unbounded": true,
            "effective_days": serde_json::Value::Null,
        }),
        DesktopRetention::ExplicitZero { source } => serde_json::json!({
            "kind": "explicit_zero_unbounded",
            "unbounded": true,
            "source": source.display().to_string(),
            "effective_days": serde_json::Value::Null,
        }),
        DesktopRetention::Ceiling {
            days,
            source,
            cleanup_days,
            effective_days,
        } => serde_json::json!({
            "kind": "ceiling",
            "unbounded": false,
            "days": days,
            "source": source.display().to_string(),
            "cleanup_period_days": cleanup_days,
            "effective_days": effective_days,
        }),
        DesktopRetention::ManagedOverride { days, source } => serde_json::json!({
            "kind": "managed_override",
            "unbounded": false,
            "ignored_key": true,
            "days": days,
            "source": source.display().to_string(),
            "effective_days": days,
        }),
        DesktopRetention::Unknown { why } => serde_json::json!({
            "kind": "unknown",
            "unbounded": serde_json::Value::Null,
            "why": why,
            "effective_days": serde_json::Value::Null,
        }),
    };
    serde_json::json!({
        "checked": true,
        "probe_id": check.probe_id,
        "key": check.key,
        "sources": sources,
        "state": state,
        // The D1½ line the D4 risk is synthesized from, carried verbatim so
        // a script and a human never read two different verdicts.
        "risk_line": check.risk_line(),
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

/// One footprint row. The three-state rule is the same as the human
/// table: a count that was not measured is `unknown` (an installed
/// store we could not enumerate, or a source whose presence was
/// never established) or `not_applicable` (a measured absence: not
/// installed on this machine).
fn footprint_json(f: &HarnessFootprint) -> serde_json::Value {
    let presence_reason = if f.note.is_empty() {
        "source presence could not be determined"
    } else {
        &f.note
    };
    serde_json::json!({
        "name": f.name,
        "installed": if f.presence_unknown {
            serde_json::json!(CountState::unknown(presence_reason))
        } else {
            serde_json::json!(f.installed)
        },
        "root": f.root.as_ref().map(|root| root.display().to_string()),
        // W1006d: additive in the `found_staged` sense — present on every
        // row, empty where the family was fully measured (or fully
        // unknown: the state fields already say that). Where it is
        // non-empty, `session_count` is a qualified lower bound over the
        // measured roots only; a consumer summing these counts without
        // consulting this key reads a floor as a total.
        "unverified_roots": f.unverified_roots,
        // W1009/DSK-3: the family's per-root rows, rendered by the same
        // scanner-side function the registry probe rows use, so the two
        // doctor tables can never disagree about a root. Additive in the
        // `unverified_roots` sense: present on every row, empty where the
        // family is not a primary-plus-roots shape. Where it is non-empty
        // the family's `session_count` stays the D6-qualified aggregate —
        // a consumer summing these rows' counts is summing a floor.
        "source_roots": f
            .source_roots
            .iter()
            .map(scanner::source_root_row_json)
            .collect::<Vec<_>>(),
        "session_count": match f.session_count {
            Some(n) => CountState::known(n),
            None if f.installed || f.presence_unknown => CountState::unknown(
                if f.note.is_empty() {
                    "session count could not be enumerated"
                } else {
                    &f.note
                },
            ),
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
            None if f.installed || f.presence_unknown => CountState::unknown(
                if f.note.is_empty() {
                    "byte count could not be measured"
                } else {
                    &f.note
                },
            ),
            None => CountState::not_applicable("not installed"),
        },
        "earliest": system_time_state(f.earliest),
        "latest": system_time_state(f.latest),
        "compressed_count": if f.presence_unknown {
            CountState::unknown(presence_reason)
        } else {
            CountState::known(f.compressed_count)
        },
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
    // W1006d: a count beside unverified roots is a *qualified lower
    // bound* — the measured roots' exact count. `≥` is the whole point:
    // an unqualified `N` would silently claim the family total, and
    // `unknown` would erase a measurement already taken. Either of those
    // is the exact rendering the ADR-059 draft review rejected.
    if let Some(count) = f.session_count {
        if f.unverified_roots.is_empty() {
            return count.to_string();
        }
        return format!("≥{count}");
    }
    // Installed but the store is not enumerable (schema not recognised):
    // "unknown", never a fake zero. A source whose presence was
    // never established is the same answer for the same reason.
    if f.installed || f.presence_unknown {
        "unknown".to_string()
    } else {
        "N/A".to_string()
    }
}

/// One source root's own D3 row (W1009/DSK-3, ADR-059 draft D5/D6): the
/// per-root companion of the family row above it, printed one per declared
/// root so the count a reader acts on is never "the family total" by
/// accident.
///
/// The count's scope is the root only — one session id sitting in two roots
/// of a movable store counts in each row it appears in, while the family row
/// above counts it once — which is also why this row never carries a `≥`:
/// a measured root's own count is exact (the family aggregate is where the
/// D6 lower-bound marking lives). A root this scan never looked at stays
/// `unknown` **with its state word and an explicit tie to that marking** —
/// the row exists precisely so an unverified root cannot be silently folded
/// into a number that reads as the family total.
///
/// Never a path in the label: the id is ours to name (D6's rule), and the
/// `primary` role word is the registry's own term for the harness's `paths`
/// store.
fn source_root_row_line(row: &scanner::SourceRootProbe) -> String {
    let label = if row.primary {
        "primary".to_string()
    } else {
        row.id.clone()
    };
    let state = scanner::probe_state_word(row.state);
    let unverified = matches!(
        row.state,
        scanner::ProbeState::SkipUnascertained
            | scanner::ProbeState::SkipUnresolvable
            | scanner::ProbeState::Indeterminate
    );
    let count = match row.record_count {
        Some(count) => count.to_string(),
        None => match row.state {
            // A root that was looked for and found absent earns its zero —
            // the same measured-absence rule every other count here follows.
            scanner::ProbeState::Missing => "0".to_string(),
            scanner::ProbeState::SkipWrongPlatform => "N/A".to_string(),
            // reason: a root this scan never measured — `state` and the mark
            // below name why; a number here would be the exact fabricated
            // zero the family aggregate refuses.
            _ => "unknown".to_string(),
        },
    };
    let bytes = match row.total_bytes {
        Some(bytes) => format!("{} ({} B)", fmt_bytes(bytes), bytes),
        None => match row.state {
            scanner::ProbeState::Missing => "root not present".to_string(),
            scanner::ProbeState::SkipWrongPlatform => "N/A".to_string(),
            _ => "unknown".to_string(),
        },
    };
    let mark = if unverified {
        " — unverified root: never looked at; the family count above is a \
         ≥ lower bound over its measured roots only"
            .to_string()
    } else {
        String::new()
    };
    format!(
        "    {:<24} sessions {:<7} · {:<24} [{}]{}",
        label, count, bytes, state, mark
    )
}

/// Whether a family's per-root rows are worth their lines: a family whose
/// every visible root was measured absent already said "not installed", and
/// repeating measured zeros one level down is noise — while a family with
/// any measuring **or any unverified root** keeps its rows, because the
/// marks are the point of the change and `never silently smaller` wins over
/// quiet (W1009/DSK-3).
fn source_root_rows_worth_printing(rows: &[scanner::SourceRootProbe]) -> bool {
    rows.iter().any(|row| {
        !matches!(
            row.state,
            scanner::ProbeState::Missing | scanner::ProbeState::SkipWrongPlatform
        )
    })
}

/// Print the per-root rows of one footprint row's family (W1009/DSK-3).
fn print_source_root_rows(f: &HarnessFootprint) {
    if !f.source_roots.is_empty() && source_root_rows_worth_printing(&f.source_roots) {
        for row in &f.source_roots {
            let line = source_root_row_line(row);
            eprintln!("{line}");
            if !row.note.is_empty() {
                eprintln!("        ({})", row.note);
            }
        }
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

/// W1043f — the one line a section prints when its check outlived its
/// deadline and the section's field carries no answer.
///
/// D1, D2, D3 and D7 already had a dedicated per-section line for this; the
/// nine `Option`-shaped sections below (and D1½) used to disappear from the
/// page instead, which contradicts the rule this file states for the
/// config-unreadable early return — an absent section under a "did not look"
/// banner reads as "no findings", the same mistake an absent heading is. A
/// timed-out section now gets its own heading plus one UNKNOWN sentence, so
/// the table of contents a reader scans stays complete and every hole is
/// named where it sits, not only in the 🔴 block. `check` is the exact label
/// the `doctor: checking …` trace printed before the check ran, so the line
/// re-answers the trace that named the wedge.
fn print_timed_out_section(heading: &str, check: &str, secs: u64, what: &str) {
    eprintln!(
        "{heading} — 🔴 UNKNOWN: the {check} check did not answer within {secs}s, so {what}."
    );
    eprintln!();
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

    // W1043 — the checks that never answered. Named before any section so a
    // reader sees which sections are holes before reading a single finding,
    // in the same register as the NOT CHECKED block above: a section listed
    // here is a question this run walked away from at its deadline, not an
    // answer, and the sections that follow are the ones that did answer.
    if !r.timed_out.is_empty() {
        eprintln!("🔴 CHECK TIMED OUT — these checks outlived their own deadline, so their sections are UNKNOWN, not zero:");
        for note in &r.timed_out {
            eprintln!(
                "     · {} (no answer within {}s)",
                note.check, note.budget_secs
            );
        }
        eprintln!(
            "   The other sections below did answer and stand as printed. An exit code of 3 \
             (not 0) is what carries this: a machine only half of which was inspected is not a \
             healthy machine."
        );
        eprintln!();
    }

    // D1
    eprintln!("D1 · Claude Code rotation settings");
    if let Some(secs) = r.check_timed_out_after(D1_CHECK) {
        eprintln!(
            "  cleanupPeriodDays: UNKNOWN — the settings read did not answer within {secs}s \
             (no answer, not the 30-day default and not proof it is safe)."
        );
    } else {
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
                // Reachable only for a hand-built report where D1's verdict was
                // set without the check being run through the goalpost: the
                // label already words the unknown.
                ClaudeRetention::TimedOut => "the settings read did not answer".to_string(),
            }
        );
        for (path, v) in &r.claude.layers {
            eprintln!("    {:<52} {}", path.display(), v.label());
        }
    }
    // D1½ — the Desktop/Cowork half of Claude Code's retention story
    // (W1009/DSK-3): a second, separately-governed key the sweep reads.
    // Its state is next to D1's because it *is* a Claude Code rotation
    // setting; the synthesized risk is D4's.
    if let Some(check) = &r.desktop_retention {
        eprintln!();
        check.print();
    } else if let Some(secs) = r.check_timed_out_after(DESKTOP_RETENTION_CHECK) {
        // W1043f: the same dedicated line D1 itself got above — a registry
        // that declares no probe stays silent on purpose, but a wedged
        // desktop settings read must not vanish from the page.
        eprintln!(
            "  Desktop/Cowork transcripts: UNKNOWN — the desktop settings read did not \
             answer within {secs}s (no answer, so neither the desktop exemption nor a \
             cap was confirmed)."
        );
    }
    eprintln!();

    // D2
    eprintln!("D2 · Gemini CLI retention policy");
    if let Some(secs) = r.check_timed_out_after(D2_CHECK) {
        eprintln!(
            "  sessionRetention: UNKNOWN — the settings read did not answer within {secs}s \
             (no answer, so whether Gemini cleans is unknown, not the 30-day default)."
        );
    } else {
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
        // W1043: a scan that outlived its deadline is a different D3 line
        // from a registry that could not be parsed — the reader who waited
        // for the budget is told which of the two they got.
        if let Some(secs) = r.check_timed_out_after(D3_SCAN_CHECK) {
            eprintln!(
                "D3 · Coverage — 🔴 the registry-driven scan did not answer within {secs}s, \
                 session coverage unknown."
            );
        } else {
            eprintln!(
                "D3 · Coverage — 🔴 registry missing / unparseable, session coverage unknown."
            );
        }
        eprintln!(
            "    refusing to fake a full scan with hardcoded paths (see “Refusing to scan with hardcoded roots” above in stderr)."
        );
        eprintln!("    listing only entries from independent read-only probes, unrelated to this registry:",);
        for f in &r.footprints {
            if f.name != "gemini" && f.name != "opencode" {
                continue;
            }
            if !f.installed && !f.presence_unknown {
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
        // The machine-log rows check runs behind the scan's verdict, not
        // behind the scan itself (its deadline is its own), so a wedge
        // there keeps its name on this path too — otherwise text and
        // `--json` would tell two stories about a section whose check did
        // not answer (`machine_logs` carries `{"checked": false}` and the
        // walked-away-at seconds under `--json`; W1043f2). With no note
        // the helper stays silent, which is the shape this branch always
        // had for a measured — possibly empty — row list.
        print_machine_logs(&r.machine_logs, r.check_timed_out_after(MACHINE_LOGS_CHECK));
        eprintln!();
        eprintln!("D4 · Risk summary — so what happens + when");
        for (i, risk) in r.risks.iter().enumerate() {
            eprintln!("  {}. {risk}", i + 1);
        }
        eprintln!();
        // These three are `Option` because a report whose config could not
        // be read has no repository or cache path to measure. That report has
        // returned above; here W1043f gives a `None` its third and only other
        // meaning — the check outlived its budget — a dedicated UNKNOWN line
        // rather than a silently absent section (a caller that assembled a
        // report by hand keeps the old silence).
        if let Some(reclaim) = &r.reclaim {
            eprintln!("D5 · How much reclaimable garbage is in the repository?");
            eprintln!("     `prune_plan` computes without deleting — doctor never runs prune, nor touches append_only.");
            print_reclaim(reclaim);
            eprintln!();
        } else if let Some(secs) = r.check_timed_out_after(RECLAIM_CHECK) {
            print_timed_out_section(
                "D5 · How much reclaimable garbage is in the repository?",
                RECLAIM_CHECK,
                secs,
                "the reclaimable garbage is unknown, not zero",
            );
        }
        if let Some(cache) = &r.cache {
            eprintln!("D6 · Local metadata cache occupancy");
            print_cache(cache);
            eprintln!();
        } else if let Some(secs) = r.check_timed_out_after(CACHE_CHECK) {
            print_timed_out_section(
                "D6 · Local metadata cache occupancy",
                CACHE_CHECK,
                secs,
                "the cache's occupancy is unknown, not zero",
            );
        }
        if let Some(body_cache) = &r.body_cache {
            eprintln!("D9 · Body cache (conversation bodies, ADR-034)");
            print_body_cache(body_cache);
            eprintln!();
        } else if let Some(secs) = r.check_timed_out_after(BODY_CACHE_CHECK) {
            print_timed_out_section(
                "D9 · Body cache (conversation bodies, ADR-034)",
                BODY_CACHE_CHECK,
                secs,
                "the body cache's occupancy is unknown, not zero",
            );
        }
        print_fts_indexes(&r.fts_indexes, r.check_timed_out_after(FTS_CHECK));
        print_destinations(&r.destinations);
        print_stage_duplicates(
            &r.stage_duplicate_shards,
            r.check_timed_out_after(STAGE_DUP_CHECK),
        );
        // D8 does not depend on the scan at all — it reads the browser
        // manifests and the config — so it is reported on this path too.
        if let Some(check) = &r.native_host {
            eprintln!();
            print_native_host(check);
        } else if let Some(secs) = r.check_timed_out_after(NATIVE_HOST_CHECK) {
            print_timed_out_section(
                "D8 · Native Messaging host (protocol v1)",
                NATIVE_HOST_CHECK,
                secs,
                "the host registration and the stage are unknown, not absent",
            );
        }
        // D12, D13 and the key inventory are not gated on the scan either:
        // `run()` measures all three whether the registry answered or not,
        // so this path owes the reader the same three sections below that
        // the normal path prints. The shared dispatch keeps every shape
        // one story — a measured section prints what it measured, a
        // wedged one keeps its heading and words the unknown (W1043f2 —
        // before this, a wedged or measured D12/D13/keys had no heading in
        // text on this path while `--json` answered `{"checked": false …}`
        // beside measured sections).
        print_dest_reconcile(
            &r.dest_reconcile,
            r.check_timed_out_after(DEST_RECONCILE_CHECK),
        );
        print_mirror(&r.mirror, r.check_timed_out_after(MIRROR_CHECK));
        print_keys(&r.keys, r.check_timed_out_after(KEYS_CHECK));
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
        if !f.installed && !f.presence_unknown {
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
        // W1009/DSK-3 (45-DESKTOP-STORES §5 item 4): one row per source root
        // beside the family row, unverified roots explicitly marked per the
        // D6 lower-bound language (`≥N` stays on the family row above).
        print_source_root_rows(f);
    }
    print_machine_logs(&r.machine_logs, r.check_timed_out_after(MACHINE_LOGS_CHECK));
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
    // W1043f adds the same else-branch that path got: a `None` with a
    // deadline note is a wedged check that must not silently drop its section.
    if let Some(reclaim) = &r.reclaim {
        eprintln!("D5 · How much reclaimable garbage is in the repository?");
        eprintln!("     `prune_plan` computes without deleting — doctor never runs prune, nor touches append_only.");
        print_reclaim(reclaim);
        eprintln!();
    } else if let Some(secs) = r.check_timed_out_after(RECLAIM_CHECK) {
        print_timed_out_section(
            "D5 · How much reclaimable garbage is in the repository?",
            RECLAIM_CHECK,
            secs,
            "the reclaimable garbage is unknown, not zero",
        );
    }

    // D6 — how much the local metadata cache actually occupies
    if let Some(cache) = &r.cache {
        eprintln!("D6 · Local metadata cache occupancy");
        print_cache(cache);
        eprintln!();
    } else if let Some(secs) = r.check_timed_out_after(CACHE_CHECK) {
        print_timed_out_section(
            "D6 · Local metadata cache occupancy",
            CACHE_CHECK,
            secs,
            "the cache's occupancy is unknown, not zero",
        );
    }

    // D9 — how much the body cache occupies, against the quota ADR-034 gives it.
    // `Option` for the same reason as D5/D6 above: no config, no root to measure.
    if let Some(body_cache) = &r.body_cache {
        eprintln!("D9 · Body cache (conversation bodies, ADR-034)");
        print_body_cache(body_cache);
        eprintln!();
    } else if let Some(secs) = r.check_timed_out_after(BODY_CACHE_CHECK) {
        print_timed_out_section(
            "D9 · Body cache (conversation bodies, ADR-034)",
            BODY_CACHE_CHECK,
            secs,
            "the body cache's occupancy is unknown, not zero",
        );
    }
    print_fts_indexes(&r.fts_indexes, r.check_timed_out_after(FTS_CHECK));

    // D7 — can each declared destination actually be reached? (ADR-023)
    print_destinations(&r.destinations);

    // D12 — does the declared set agree with what the timers and the
    // reclaim actually do on this machine? (W1012 / SG-1 C1)
    print_dest_reconcile(
        &r.dest_reconcile,
        r.check_timed_out_after(DEST_RECONCILE_CHECK),
    );

    // D13 — the local mirror's settled field family (ADR-062 D3/D6.3, LM-4)
    print_mirror(&r.mirror, r.check_timed_out_after(MIRROR_CHECK));

    print_stage_duplicates(
        &r.stage_duplicate_shards,
        r.check_timed_out_after(STAGE_DUP_CHECK),
    );

    // Every archive copy has its own key, so a backup that names only the
    // local one leaves the off-site copies unreadable (W281 BUG-2). Reported
    // per copy rather than as one "key" for that reason.
    print_keys(&r.keys, r.check_timed_out_after(KEYS_CHECK));

    // D8 — is the browser-side host actually usable?
    if let Some(check) = &r.native_host {
        eprintln!();
        print_native_host(check);
    } else if let Some(secs) = r.check_timed_out_after(NATIVE_HOST_CHECK) {
        print_timed_out_section(
            "D8 · Native Messaging host (protocol v1)",
            NATIVE_HOST_CHECK,
            secs,
            "the host registration and the stage are unknown, not absent",
        );
    }
}

/// W1043f: a `None` with a deadline note is a wedged check, and gets the
/// dedicated UNKNOWN line every timed-out section gets instead of silence;
/// a `None` without one stays silent (the report's config was unreadable —
/// a story `not_checked` already tells — or a caller assembled the report
/// by hand).
fn print_stage_duplicates(check: &Option<StageDuplicateCheck>, timed_out_after_secs: Option<u64>) {
    match (check, timed_out_after_secs) {
        (Some(check), _) => {
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
        (None, Some(secs)) => print_timed_out_section(
            "D11 · Identical duplicate shards in the configured stage",
            STAGE_DUP_CHECK,
            secs,
            "the duplicate count is unknown, not zero",
        ),
        (None, None) => {}
    }
}

/// Which key files this machine holds, per archive copy, and which of them the
/// user has said they keep a copy of.
///
/// Silent for a `None` with no deadline note (a report whose config could not
/// be read): the inventory is derived from the config, so there is nothing to
/// say rather than nothing to list. `not_checked` already names that check as
/// skipped. A `None` that carries a note (W1043f) is a wedged walk instead,
/// and prints the section's UNKNOWN line.
fn print_keys(keys: &Option<Vec<KeyInventoryRow>>, timed_out_after_secs: Option<u64>) {
    match (keys, timed_out_after_secs) {
        (Some(rows), _) => {
            eprintln!();
            eprintln!("Keys · one file per archive copy — each opens only its own copy");
            eprintln!(
                "     A second machine reads a copy with that copy's key. Back up every one."
            );
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
        (None, Some(secs)) => print_timed_out_section(
            "Keys · one file per archive copy — each opens only its own copy",
            KEYS_CHECK,
            secs,
            "which key files this machine holds is unknown, not empty",
        ),
        (None, None) => {}
    }
}

/// W1043f: the same `Option` + deadline-note dispatch the stage-duplicate and
/// key sections use — a wedged walk keeps its heading and words the unknown.
fn print_fts_indexes(indexes: &Option<Vec<FtsIndexCheck>>, timed_out_after_secs: Option<u64>) {
    match (indexes, timed_out_after_secs) {
        (Some(indexes), _) => {
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
        (None, Some(secs)) => print_timed_out_section(
            "D10 · Full-text indexes (local, per destination)",
            FTS_CHECK,
            secs,
            "every index's health is unknown, not absent",
        ),
        (None, None) => {}
    }
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
                if let Some(debris) = &p.debris {
                    print_debris(debris);
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
            DestinationOutcome::ProbeTimedOut { seconds } => {
                eprintln!(
                    "  {:<12} {} — NO ANSWER WITHIN {seconds}s (the probe outlived its deadline)",
                    p.name, p.repo_root
                );
                eprintln!(
                    "               Reachability is UNKNOWN, not absent: the connection neither \
                     completed nor failed before the deadline, so whether this destination holds \
                     the archive is a question this run did not get to answer."
                );
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

/// D7 printing — interrupted-push debris at this destination (ISSUE-006 step
/// A), as the shared classification measured it.
///
/// Only spoken when there is debris to name: a destination whose packs are
/// all indexed holds none, and a line reading "0 torn, 0 stranded" would be
/// noise the doctor's healthy-run convention exists to avoid. Torn and
/// stranded carry **separate** counts and byte totals, torn rides the one
/// sentence every surface shares
/// ([`crate::debris::INTERRUPTED_PUSH_SENTENCE`]), and the earliest-seen time
/// stays the honest unknown it is ([`crate::debris::NO_MTIME_SENTENCE`]) —
/// never a fabricated clock. A survey that could not establish anything says
/// UNKNOWN with its reason, in the same shape the activity-index line keeps.
fn print_debris(debris: &DebrisProbe) {
    let measured = match debris {
        DebrisProbe::NotChecked { why } => {
            eprintln!("               debris: UNKNOWN — {why}");
            return;
        }
        DebrisProbe::Measured(measured) => measured,
    };
    if !measured.has_anything_to_say() {
        return;
    }
    if measured.torn > 0 {
        eprintln!(
            "               debris: {} torn pack(s) ({}) — {}",
            measured.torn,
            fmt_bytes(measured.torn_bytes),
            crate::debris::INTERRUPTED_PUSH_SENTENCE
        );
    }
    if measured.stranded > 0 {
        eprintln!(
            "               debris: {} stranded pack(s) ({}) — complete and verified; the \
             next open adopts them, `prune-orphans --destination <name>` reports the set in full",
            measured.stranded,
            fmt_bytes(measured.stranded_bytes)
        );
    }
    if measured.unknown > 0 {
        eprintln!(
            "               debris: {} pack(s) ({}) with an unknown status — neither torn \
             nor verified; `prune-orphans --destination <name>` carries each one's reason",
            measured.unknown,
            fmt_bytes(measured.unknown_bytes)
        );
    }
    eprintln!(
        "               debris: earliest-seen time unknown — {}",
        crate::debris::NO_MTIME_SENTENCE
    );
}

/// D12 printing — the four-fact destination reconciliation (W1012 / SG-1
/// C1). The facts come from [`crate::destdiag::reconcile`] and the alerts
/// are its wording, printed unchanged: `status`'s `[dest]` / `[reclaim]`
/// lines, this page and the menu bar app all display one vocabulary, which
/// is the requirement the "one function" in `destdiag` exists to satisfy.
///
/// The alert lines — sentence and fix, the English gloss and repair advice
/// a degraded destination carries — arrive *inside*
/// [`crate::destdiag::doctor_lines`]: its tail renders every alert and each
/// one's fix the same way `status` renders its loop, so a page that shows
/// the D12 table cell (the mandated sentence alone, a table row) always
/// carries the gloss and the roads under it. W1034f2's note: this function
/// has no `all_alerts` loop of its own, and the rev4 review read exactly
/// that loop's absence as the gloss "not printed anywhere on the page" —
/// the alerts' path into this page is the `doctor_lines` tail, asserted on
/// the D12 step of `w1023_sg2_degraded_reclaim_test`.
///
/// Silent when the reconciliation was not computed (config unreadable —
/// `not_checked` names it there) and prints nothing for a machine with
/// nothing declared beyond its default-repository row: a start.md machine
/// with one timer has facts, and its printed lines are the facts, not
/// findings — the alert lines stay empty and stay unprinted. A `None` that
/// carries a deadline note (W1043f) is a wedged reconcile instead, and
/// prints the section's UNKNOWN line.
fn print_dest_reconcile(
    reconciliation: &Option<crate::destdiag::Reconciliation>,
    timed_out_after_secs: Option<u64>,
) {
    match (reconciliation, timed_out_after_secs) {
        (Some(reconciliation), _) => {
            eprintln!("D12 · Declared destinations vs installed timers — push coverage, credentials, last push, reclaim");
            eprintln!(
                "     read-only: reads the timer files, the reclaim/push state records, and — for the \
                  upgrade-window proof — one local repository's snapshot listing; never opens a \
                  destination behind a backend string, never reads one under a mounted volume."
            );
            for line in crate::destdiag::doctor_lines(reconciliation) {
                eprintln!("{line}");
            }
            eprintln!();
        }
        (None, Some(secs)) => print_timed_out_section(
            "D12 · Declared destinations vs installed timers — push coverage, credentials, last push, reclaim",
            DEST_RECONCILE_CHECK,
            secs,
            "every one of the four facts is unknown, not zero",
        ),
        (None, None) => {}
    }
}

/// D13 printing — the local mirror's settled field family (ADR-062
/// D3/D6.3, LM-4). The sentences are [`crate::mirror::mirror_status`]'s
/// own — the same words `status` prints and the menu bar displays —
/// because one vocabulary across the three surfaces is the requirement
/// ADR-062 ruling 8 states. Silent when no `[mirror]` section exists:
/// a machine that mirrors nothing has no mirror facts to check, the same
/// silence `key_alerts` keeps for a machine with every key present.
///
/// The one doctor-only line is the orphan's handling instruction
/// (ADR-062 D2: orphans are never deleted automatically, `doctor` reports
/// the reclaimable bytes, the user cleans by the documented manual act):
/// orphans are never reclaimed automatically (ADR-001), and the manual
/// act is the one `docs/cli.md` documents. A `None` that carries a
/// deadline note (W1043f) is a wedged mirror walk instead, and prints the
/// section's UNKNOWN line.
fn print_mirror(status: &Option<crate::mirror::MirrorStatus>, timed_out_after_secs: Option<u64>) {
    match (status, timed_out_after_secs) {
        (Some(status), _) => {
            if !status.configured {
                return;
            }
            eprintln!("D13 · Local mirror — size, last sync, snapshots behind, orphans (ADR-062)");
            eprintln!(
                "     read-only: reads the sync state records and walks the mirror directory; \
                 never opens a destination, never deletes anything."
            );
            if !status.enabled {
                // With a leftover copy on disk, the rows [`mirror_status`] worded
                // already carry its measured size (rev0 ⑧: the ADR's every-byte
                // promise shows its number, the same sentence `status` and the
                // menu bar print); with nothing left, the off-state keeps its one
                // doctor-only wording.
                if status.rows.is_empty() {
                    eprintln!(
                        "  [mirror] configured but `enabled = false` — this machine mirrors nothing; \
                         the mirror directory, if one is left from before, keeps every byte it holds \
                         until you delete it"
                    );
                } else {
                    for row in &status.rows {
                        eprintln!("  {}", row.sentence);
                    }
                }
                eprintln!();
                return;
            }
            for row in &status.rows {
                eprintln!("  {}", row.sentence);
            }
            if let crate::mirror::MirrorUsageState::Known { files, bytes } = &status.orphans {
                if *files > 0 {
                    eprintln!(
                        "  orphans are never deleted automatically (ADR-001): a remote cleanup left \
                         files this mirror no longer needs ({} reclaimable); the manual cleanup is \
                         documented under `mirror` in the CLI docs — removing the whole directory \
                         and re-running `mirror sync` rebuilds a copy without them",
                        crate::destdiag::fmt_bytes(*bytes)
                    );
                }
            }
            eprintln!();
        }
        (None, Some(secs)) => print_timed_out_section(
            "D13 · Local mirror — size, last sync, snapshots behind, orphans (ADR-062)",
            MIRROR_CHECK,
            secs,
            "the mirror's facts are unknown, not zero",
        ),
        (None, None) => {}
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
        // W1006d: a count beside unverified roots is the measured roots'
        // floor — `≥N` (or `≥0`), never the plain number, which is exactly
        // the silent-family-total reading the ADR-059 draft review
        // rejected. The unverified roots themselves are already named in
        // the probe's note, printed in the same row.
        let count = match p.state {
            scanner::ProbeState::FileTarget | scanner::ProbeState::Scanned => {
                match p.record_count {
                    Some(c) if !p.unverified_roots.is_empty() => format!("≥{c}"),
                    // B90: `unwrap_or(0)` here is unreachable today (a Scanned
                    // probe always carries a count) — which is exactly why it
                    // was worth removing: it is a fallback that would print the
                    // B82 lie again the moment the invariant moves.
                    Some(c) => c.to_string(),
                    None => "unknown".to_string(),
                }
            }
            scanner::ProbeState::Missing => {
                if p.unverified_roots.is_empty() {
                    "0".to_string()
                } else {
                    "≥0".to_string()
                }
            }
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

    /// W1034f2 (rev4 Low 1): the destination-less machine's repository and
    /// key file, from the one definition the CLI's `store_config_from` falls
    /// back to and `destdiag`'s proof reads — so a change to the default
    /// repository location moves both readers together, and this test pins
    /// that the definition is still the data-dir one (unconfigured) and
    /// still passes config-sourced values through untouched.
    #[test]
    fn home_repository_store_config_is_the_single_default_definition() {
        let base = home_repository_store_config(&Config::default());
        let data_root = default_data_root();
        assert_eq!(
            base.repo_root,
            data_root.join("repo").to_string_lossy().into_owned(),
            "an unconfigured machine keeps the data-dir repository default"
        );
        assert_eq!(
            base.key_file,
            data_root.join("masterkey.json"),
            "an unconfigured machine keeps the data-dir key default"
        );
        assert_eq!(base.connections, crate::store::DEFAULT_CONNECTIONS);

        let mut config = Config::default();
        config.rustic_repo = Some("/srv/backup-repo".to_string());
        config.rustic_key_file = Some("/srv/backup.key".to_string());
        config.rustic_cache_dir = Some("/srv/cache".to_string());
        let configured = home_repository_store_config(&config);
        assert_eq!(configured.repo_root, "/srv/backup-repo");
        assert_eq!(
            configured.key_file,
            std::path::PathBuf::from("/srv/backup.key")
        );
        assert_eq!(
            configured.cache_dir,
            Some(std::path::PathBuf::from("/srv/cache")),
            "config-sourced values pass through unexpanded — the config loader \
             expanded them, and expansion gates are the CLI wrapper's own"
        );
    }

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
            presence_unknown: false,
            session_count: Some(3),
            candidate_count: Some(414),
            unreadable_count: unreadable,
            unreadable_entry_count: Some(0),
            total_bytes: None,
            unverified_roots: Vec::new(),
            source_roots: Vec::new(),
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
            presence_unknown: false,
            session_count: Some(3),
            candidate_count: Some(414),
            unreadable_count: None,
            unreadable_entry_count: Some(0),
            total_bytes: Some(4096),
            unverified_roots: Vec::new(),
            source_roots: Vec::new(),
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
            unverified_roots: Vec::new(),
            source_roots: Vec::new(),
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
            config: None,
            claude: ClaudeCheck {
                layers: vec![(
                    PathBuf::from("/nowhere/.claude/settings.json"),
                    ClaudeRetention::UnsetDefault,
                )],
                verdict: ClaudeRetention::UnsetDefault,
            },
            gemini: GeminiRetention::default(),
            // No registry in this no-IO fixture, so the retention probe
            // was not performed — `checked: false`, never a state.
            desktop_retention: None,
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
            dest_reconcile: None,
            mirror: None,

            timed_out: Vec::new(),
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
    ///
    /// W1009 (DSK-3) adds `desktop_retention` under the same rule: the
    /// Desktop/Cowork retention probe is a distinct check (its own sources,
    /// its own three-state vocabulary, `checked: false` when the registry
    /// declares no probe) that has no half of an existing field to live in.
    ///
    /// W1012 (SG-1 C1) adds `destination_reconciliation` the same way: the
    /// four-fact per-destination reconciliation is a new check with its own
    /// tri-state vocabulary (`checked: false` when the config was never
    /// read), reported beside `destinations` — D7's live probes and the
    /// reconciliation's file-derived facts answer different questions about
    /// the same names, so neither could live in the other.
    ///
    /// W1025 (LM-4 / ADR-062 D13) adds `mirror` the same way: the local
    /// mirror's owned-words field family (size, last sync, snapshots
    /// behind, orphans, rows) is a new check over its own records and
    /// its own directory, in the tagged shapes `status --json`'s
    /// `local.mirror` already carries, so the two objects cannot
    /// disagree about one mirror.
    ///
    /// W1043 adds `timed_out` the same way: the checks that outlived their
    /// own deadline are a new fact about the *run* (which sections are
    /// holes) rather than about the machine, and no section's shape could
    /// carry it — a placeholder value must never be presentable as a
    /// measurement, so the note that says "this section did not answer"
    /// lives one level up, where a script reads it before any section.
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
                "desktop_retention",
                "destination_reconciliation",
                "destinations",
                "footprints",
                "fts_indexes",
                "gemini",
                "keys",
                "machine_logs",
                "mirror",
                "native_host",
                "not_checked",
                "other_present",
                "probes",
                "reclaim",
                "risks",
                "scan_failed",
                "schema_version",
                "stage_duplicate_shards",
                "timed_out",
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

    /// A skip shape (`skip_unascertained` / `skip_unresolvable`) or a
    /// no-probe row never established whether the source is present, so
    /// `installed` and every count stay `unknown` with the reason —
    /// never a boolean absence claim, and never a measured zero for a
    /// tally nobody took.
    #[test]
    fn doctor_json_skip_shape_keeps_presence_and_counts_unknown() {
        let reason = "confidence=unascertained; skipped (scanning a guessed path = false negatives/positives)"
            .to_string();
        let mut r = report();
        r.footprints[0] = HarnessFootprint {
            installed: false,
            presence_unknown: true,
            session_count: None,
            candidate_count: None,
            unreadable_count: None,
            unreadable_entry_count: None,
            total_bytes: None,
            earliest: None,
            latest: None,
            compressed_count: 0,
            note: reason.clone(),
            ..fp()
        };
        let row = &report_to_json(&r)["footprints"][0];
        let unknown = serde_json::json!({"kind": "unknown", "why": reason});
        assert_eq!(row["installed"], unknown);
        assert_eq!(row["session_count"], unknown);
        assert_eq!(row["total_bytes"], unknown);
        assert_eq!(row["compressed_count"], unknown);
        assert_ne!(
            row["compressed_count"],
            serde_json::json!({"kind": "known", "count": 0}),
            "a source nobody probed has no compressed-session tally; 0 would be a measurement nobody made"
        );
    }

    /// The two states that *earn* their numbers: an enumerated store
    /// reports a measured count — a measured zero included — and a
    /// measured absence keeps `installed: false` with its measured
    /// zero. The skip shape above must not have loosened either.
    #[test]
    fn doctor_json_keeps_measured_zero_and_known_absence() {
        let present = report_to_json(&report());
        assert_eq!(
            present["footprints"][0]["installed"],
            serde_json::json!(true)
        );
        assert_eq!(
            present["footprints"][0]["compressed_count"],
            serde_json::json!({"kind": "known", "count": 0})
        );

        let mut absent = report();
        absent.footprints[0] = HarnessFootprint {
            installed: false,
            presence_unknown: false,
            session_count: None,
            total_bytes: None,
            note: "not installed".to_string(),
            ..fp()
        };
        let row = &report_to_json(&absent)["footprints"][0];
        assert_eq!(row["installed"], serde_json::json!(false));
        assert_eq!(
            row["compressed_count"],
            serde_json::json!({"kind": "known", "count": 0})
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
            config: None,
            claude: ClaudeCheck {
                layers: Vec::new(),
                verdict: ClaudeRetention::UnsetDefault,
            },
            gemini: GeminiRetention::default(),
            desktop_retention: None,
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
            dest_reconcile: None,
            mirror: None,

            timed_out: Vec::new(),
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
            "mirror",
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
            unverified_roots: Vec::new(),
            source_roots: Vec::new(),
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

    /// The footprint row must inherit the probe's presence verdict:
    /// every never-probed state (`skip_unascertained`,
    /// `skip_unresolvable`, indeterminate) leaves presence unknown,
    /// while `missing` (looked, and found nothing) and `skip_wrong_platform`
    /// (no cell for this platform) keep the measured absence.
    #[test]
    fn footprint_rows_inherit_the_probes_presence_verdict() {
        let sqlite_row =
            |state: ProbeState| footprint_from_sqlite_probe(&probe("sqlite-store", state));
        for state in [
            ProbeState::SkipUnascertained,
            ProbeState::SkipUnresolvable,
            ProbeState::Indeterminate,
        ] {
            let row = sqlite_row(state);
            assert!(
                row.presence_unknown && !row.installed,
                "{state:?} never established presence, so the row must not claim absence"
            );
        }
        for state in [ProbeState::Missing, ProbeState::SkipWrongPlatform] {
            let row = sqlite_row(state);
            assert!(
                !row.presence_unknown && !row.installed,
                "{state:?} is a measured absence or non-applicability, not an unknown presence"
            );
        }

        // A directory harness with no probe row at all (the registry in
        // effect has no entry for it): presence unknown, never absence.
        let no_probe = footprint_from_dir_probe(
            "claude-code",
            PathBuf::from("/nowhere/.claude/projects"),
            None,
            std::iter::empty(),
        );
        assert!(
            no_probe.presence_unknown && !no_probe.installed,
            "a row with no probe never established presence"
        );

        // A resolved-but-absent directory root is a measured absence.
        let missing = footprint_from_dir_probe(
            "claude-code",
            PathBuf::from("/nowhere/.claude/projects"),
            Some(&probe("claude-code", ProbeState::Missing)),
            std::iter::empty(),
        );
        assert!(
            !missing.presence_unknown && !missing.installed,
            "a probe that looked and found nothing is a measured absence"
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

#[cfg(test)]
mod w1006d_lower_bound_tests {
    use super::*;

    /// W1006d (ADR-059 draft D6): the doctor's two count surfaces state a
    /// *qualified lower bound* as one — a `≥` prefixed number in the human
    /// tables, the unverified roots listed in the JSON row, and never a
    /// plain number that reads as the family total, never an unknown that
    /// erases the measured part (the D6 side effect the draft's external review
    /// rejected).
    #[test]
    fn a_marked_footprint_prints_a_lower_bound_and_names_its_unverified_roots() {
        let mut f = default_footprint("claude-code", PathBuf::from("/nowhere/projects"));
        f.installed = true;
        f.session_count = Some(2);
        f.unverified_roots = vec!["claude-desktop-cowork".to_string()];
        assert_eq!(
            footprint_count_label(&f),
            "≥2",
            "a count beside unverified roots is a floor, not the family total"
        );
        let v = footprint_json(&f);
        assert_eq!(
            v["unverified_roots"],
            serde_json::json!(["claude-desktop-cowork"]),
            "the JSON row lists the roots the count excludes"
        );
        assert_eq!(
            v["session_count"],
            serde_json::json!({"kind": "known", "count": 2})
        );

        // The measured-absent floor: the family's presence is partially
        // unknown, so the row is presence-unknown (never a measured "not
        // installed") and its floor renders `≥0`, not a plain 0.
        f.installed = false;
        f.presence_unknown = true;
        f.session_count = Some(0);
        assert_eq!(footprint_count_label(&f), "≥0");
    }

    #[test]
    fn an_unmarked_footprint_keeps_its_plain_number() {
        let mut f = default_footprint("claude-code", PathBuf::from("/nowhere/projects"));
        f.installed = true;
        f.session_count = Some(2);
        assert_eq!(footprint_count_label(&f), "2");
        assert_eq!(
            footprint_json(&f)["unverified_roots"],
            serde_json::json!([]),
            "a fully measured family keeps its exact-count reading"
        );
    }

    fn session_record(i: u8) -> crate::models::SessionRecord {
        crate::models::SessionRecord {
            id: format!("claude-code.fixture.0000000{i}"),
            absolute_path: PathBuf::from(format!("/nowhere/projects/s{i}.jsonl")),
            byte_size: 1,
            mtime: UNIX_EPOCH,
            source: crate::models::HarnessSource::ClaudeCode,
            compressed: false,
            sqlite_layout: None,
            provenance: Default::default(),
            session_dir: None,
            movable_root: false,
        }
    }

    /// The dir-probe seam is where the footprint row and the registry probe
    /// row could drift apart (`doctor_consistency_test` joins them), so the
    /// lower-bound marking and count must both travel from the probe:
    /// count for count, root id for root id, in the scanned shape and in
    /// the measured-absent shape alike.
    #[test]
    fn the_dir_probe_carries_the_marking_in_the_scanned_and_measured_absent_shapes() {
        let marked = |state, count| scanner::HarnessProbe {
            id: "claude-code".to_string(),
            display_name: "Claude Code".to_string(),
            root: Some(PathBuf::from("/nowhere/projects")),
            confidence: crate::scanner::Confidence::Confirmed,
            state,
            unverified_roots: vec![scanner::UnverifiedRoot {
                id: "claude-desktop-cowork".to_string(),
                state: scanner::ProbeState::SkipUnascertained,
            }],
            source_roots: Vec::new(),
            record_count: Some(count),
            candidate_count: Some(count),
            unreadable_count: Some(0),
            unreadable_entry_count: Some(0),
            earliest: None,
            latest: None,
            bytes: None,
            recognized_files: Vec::new(),
            note: "2 declared source roots checked; total is a lower bound — 1 unverified \
                   source root(s): claude-desktop-cowork (skip_unascertained)"
                .to_string(),
        };

        let records = vec![session_record(0), session_record(1)];
        let records_ref: Vec<&crate::models::SessionRecord> = records.iter().collect();
        let scanned = marked(scanner::ProbeState::Scanned, 2);
        let fp = footprint_from_dir_probe(
            "claude-code",
            PathBuf::from("/fallback"),
            Some(&scanned),
            records_ref.into_iter(),
        );
        assert_eq!(fp.session_count, Some(2), "count for count");
        assert_eq!(
            fp.unverified_roots,
            vec!["claude-desktop-cowork".to_string()],
            "root id for root id"
        );
        assert_eq!(fp.note, scanned.note, "note for note");

        let absent = marked(scanner::ProbeState::Missing, 0);
        let fp = footprint_from_dir_probe(
            "claude-code",
            PathBuf::from("/fallback"),
            Some(&absent),
            [].into_iter(),
        );
        assert_eq!(
            fp.session_count,
            Some(0),
            "the measured-absent floor, taken from the probe so the tables agree"
        );
        assert!(
            fp.presence_unknown,
            "a family with an unverified sibling is partially unknown, never a measured \
             \"not installed\""
        );
        assert_eq!(
            fp.unverified_roots,
            vec!["claude-desktop-cowork".to_string()]
        );
        assert_eq!(fp.session_count, absent.record_count);
    }
}

#[cfg(test)]
mod w1009_doctor_desktop_tests {
    use super::*;
    use crate::scanner::{HarnessRegistry, ProbeState, SourceRootProbe};

    // -----------------------------------------------------------------
    // D3 — per-root rows
    // -----------------------------------------------------------------

    fn row(id: &str, primary: bool, state: ProbeState) -> SourceRootProbe {
        SourceRootProbe {
            id: id.to_string(),
            primary,
            state,
            root: Some(PathBuf::from("/nowhere/roots").join(id)),
            record_count: (state == ProbeState::Scanned).then_some(1),
            unreadable_count: Some(0),
            unreadable_entry_count: Some(0),
            total_bytes: (state == ProbeState::Scanned).then_some(23),
            note: String::new(),
        }
    }

    /// W1009/DSK-3, the row's scope rule: a measured root's own count is
    /// exact — no `≥`, because the D6 lower-bound marking belongs to the
    /// family aggregate the row sits under, and a root cannot qualify itself.
    #[test]
    fn a_scanned_root_row_prints_its_exact_count_and_state() {
        let line = source_root_row_line(&row("codex-archived", false, ProbeState::Scanned));
        assert!(
            line.contains("codex-archived") && line.contains("sessions 1"),
            "the row names its root and its own count; got: {line:?}"
        );
        assert!(
            line.contains("[scanned]"),
            "the state word travels with the row; got: {line:?}"
        );
        assert!(
            !line.contains('≥'),
            "a per-root count is the root's own measurement, never a lower bound"
        );
        assert!(
            !line.contains("primary"),
            "an additional root is named by id"
        );
    }

    /// The primary row is named by its role, not by a guessed store name —
    /// `primary` is the registry's own term for the harness's `paths` store.
    #[test]
    fn the_primary_row_is_named_by_its_role() {
        let line = source_root_row_line(&row("claude-code", true, ProbeState::Scanned));
        assert!(
            line.contains("primary") && !line.contains("claude-code"),
            "the primary row's label is the role word; got: {line:?}"
        );
    }

    /// The unverified mark: a root this scan never looked at prints `unknown`
    /// with its state word and the explicit tie back to the family aggregate
    /// — exactly the "explicitly marked" D6 language asks for, so the family
    /// number can never silently count as exact.
    #[test]
    fn an_unverified_root_row_prints_the_mark_and_no_count() {
        let mut unverified = row(
            "claude-desktop-cowork",
            false,
            ProbeState::SkipUnascertained,
        );
        unverified.record_count = None;
        unverified.total_bytes = None;
        let line = source_root_row_line(&unverified);
        assert!(
            line.contains("sessions unknown"),
            "a root nobody looked at carries no count; got: {line:?}"
        );
        assert!(
            line.contains("[skip_unascertained]"),
            "the row names why it never measured; got: {line:?}"
        );
        assert!(
            line.contains("unverified root") && line.contains("lower bound"),
            "the mark ties the row to the family's ≥ aggregate; got: {line:?}"
        );
    }

    /// A measured absence is the one count that is earned: a root that was
    /// looked for and found absent prints `0` — the same rule every other
    /// count in this file follows.
    #[test]
    fn a_missing_root_row_prints_its_earned_zero() {
        let mut missing = row("codex-archived", false, ProbeState::Missing);
        missing.record_count = None;
        missing.total_bytes = None;
        let line = source_root_row_line(&missing);
        assert!(
            line.contains("sessions 0") && line.contains("root not present"),
            "a missing root's row is the measured absence; got: {line:?}"
        );
        assert!(
            !line.contains("unknown") && !line.contains("unverified"),
            "a measured absence is neither an unknown nor a mark; got: {line:?}"
        );
    }

    /// The print rule: rows stay quiet only when every visible root was
    /// measured absent (the family already said "not installed"); anything
    /// else — a measurement, an unverified root, an indeterminate one —
    /// keeps its rows, because `never silently smaller` beats quiet.
    #[test]
    fn rows_print_unless_every_root_was_measured_absent() {
        assert!(
            !source_root_rows_worth_printing(&[]),
            "no rows, nothing to print"
        );
        assert!(
            !source_root_rows_worth_printing(&[
                row("claude-code", true, ProbeState::Missing),
                row("claude-desktop-cowork", false, ProbeState::Missing),
            ]),
            "an all-absent family already said 'not installed'; rows would repeat zeros"
        );
        assert!(
            source_root_rows_worth_printing(&[
                row("claude-code", true, ProbeState::Scanned),
                row("claude-desktop-cowork", false, ProbeState::Missing),
            ]),
            "a scanned primary keeps its rows beside the measured-absent sibling"
        );
        assert!(
            source_root_rows_worth_printing(&[
                row("claude-code", true, ProbeState::Missing),
                row(
                    "claude-desktop-cowork",
                    false,
                    ProbeState::SkipUnascertained
                ),
            ]),
            "the W1006e floor shape keeps its rows — the unverified mark is the point"
        );
    }

    /// The two doctor tables share one renderer per root, so they can never
    /// disagree about a root (`doctor_consistency_test` joins the family
    /// counts; this pins the root rows to the same single source).
    #[test]
    fn the_footprint_row_and_the_registry_probe_row_agree_per_root() {
        let mut probe = scanner::HarnessProbe {
            id: "claude-code".to_string(),
            display_name: "Claude Code".to_string(),
            root: Some(PathBuf::from("/nowhere/projects")),
            confidence: crate::scanner::Confidence::Confirmed,
            state: ProbeState::Scanned,
            unverified_roots: Vec::new(),
            source_roots: vec![
                row("claude-code", true, ProbeState::Scanned),
                row("claude-desktop-cowork", false, ProbeState::Scanned),
            ],
            record_count: Some(2),
            candidate_count: Some(2),
            unreadable_count: Some(0),
            unreadable_entry_count: Some(0),
            earliest: None,
            latest: None,
            bytes: None,
            recognized_files: Vec::new(),
            note: String::new(),
        };
        let records = [w1006d_lower_bound_records(0), w1006d_lower_bound_records(1)];
        let footprint = footprint_from_dir_probe(
            "claude-code",
            PathBuf::from("/fallback"),
            Some(&probe),
            records.iter(),
        );
        assert_eq!(footprint.source_roots.len(), 2);
        assert_eq!(
            footprint.source_roots[0].id, probe.source_roots[0].id,
            "the footprint row carries the probe's own rows, id for id"
        );
        let footprint_json_rows: Vec<serde_json::Value> = footprint_json(&footprint)
            ["source_roots"]
            .as_array()
            .unwrap()
            .clone();
        let probe_json_rows: Vec<serde_json::Value> = scanner::probe_json(&probe)["source_roots"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(
            footprint_json_rows, probe_json_rows,
            "the two tables render each root through the same function"
        );
        probe.source_roots.clear();
    }

    // -----------------------------------------------------------------
    // D1½ — the desktop retention probe
    // -----------------------------------------------------------------

    /// A scratch registry whose retention probe declares *absolute* source
    /// files inside `dir` — no `~` expansion, so the test never touches the
    /// machine's real settings files and needs no env mutation.
    fn retention_registry(dir: &Path) -> HarnessRegistry {
        let platform_key = scanner::current_platform();
        let cell = serde_json::json!({
            "user_sources": [dir.join("user-settings.json").to_str().unwrap()],
            "managed_sources": [dir.join("managed-settings.json").to_str().unwrap()],
            "confidence": "measured-locally",
            "source": "synthetic fixture",
        });
        let platforms = serde_json::json!({ platform_key: cell });
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{
                "id": "claude-code",
                "display_name": "synthetic",
                "paths": {},
                "retention_probes": [{
                    "id": "desktop-session-cleanup",
                    "key": "desktopSessionCleanupPeriodDays",
                    "states": {
                        "missing": "synthetic missing",
                        "zero": "synthetic zero",
                        "positive_int": "synthetic positive"
                    },
                    "platforms": platforms,
                }],
            }]
        }))
        .unwrap()
    }

    fn claude_verdict(days: Option<u64>) -> ClaudeCheck {
        let verdict = match days {
            None => ClaudeRetention::UnsetDefault,
            Some(days) => ClaudeRetention::Safe {
                days,
                source: PathBuf::from("/nowhere/.claude/settings.json"),
            },
        };
        ClaudeCheck {
            layers: vec![(
                PathBuf::from("/nowhere/.claude/settings.json"),
                verdict.clone(),
            )],
            verdict,
        }
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn inspect(dir: &Path, user: &str, managed: &str, days: Option<u64>) -> DesktopRetentionCheck {
        write(dir, "user-settings.json", user);
        write(dir, "managed-settings.json", managed);
        inspect_desktop_retention(&retention_registry(dir), &claude_verdict(days))
            .expect("the fixture registry declares the probe")
    }

    /// Missing → the built-in default 0 = no ceiling — and the state is
    /// worded as exactly that, never as the 30-day CLI default, because the
    /// desktop exemption is governed by a *different* key with a different
    /// default (DSK-0a, the retention-probe semantics section, item 2).
    #[test]
    fn a_missing_desktop_key_is_the_unbounded_default_never_thirty() {
        let dir = tempfile::tempdir().unwrap();
        let check = inspect(&dir.path(), "{\"cleanupPeriodDays\": 90}", "{}", Some(90));
        assert!(matches!(check.state, DesktopRetention::DefaultZero));
        let line = check.risk_line();
        assert!(
            line.contains("default 0 = no ceiling"),
            "the line states the real default; got: {line:?}"
        );
        assert!(
            line.contains("NOT the 30-day"),
            "the line distinguishes the CLI default explicitly; got: {line:?}"
        );
        assert!(
            !check.state.is_dangerous(),
            "unbounded retention is not a deletion window"
        );
    }

    /// Explicit 0 is the same retention as the default (no ceiling) but a
    /// different fact about who set it — the state says the user did.
    #[test]
    fn an_explicit_zero_is_unbounded_by_choice() {
        let dir = tempfile::tempdir().unwrap();
        let check = inspect(
            &dir.path(),
            "{\"desktopSessionCleanupPeriodDays\": 0, \"cleanupPeriodDays\": 36500}",
            "{}",
            Some(36500),
        );
        match &check.state {
            DesktopRetention::ExplicitZero { source } => assert_eq!(
                source,
                &dir.path().join("user-settings.json"),
                "the state names which file set the explicit zero"
            ),
            other => panic!("expected ExplicitZero, got {other:?}"),
        }
    }

    /// A positive ceiling makes the effective retention the LONGER of the two
    /// periods — never the smaller one, and never the ceiling alone when D1
    /// could classify the CLI side. Both sides of the max are pinned.
    #[test]
    fn a_positive_ceiling_takes_the_longer_of_the_two_periods() {
        let dir = tempfile::tempdir().unwrap();
        // Desktop ceiling below the CLI cleanup: the CLI side wins.
        let check = inspect(
            &dir.path(),
            "{\"desktopSessionCleanupPeriodDays\": 14, \"cleanupPeriodDays\": 36500}",
            "{}",
            Some(36500),
        );
        match &check.state {
            DesktopRetention::Ceiling {
                days,
                cleanup_days,
                effective_days,
                ..
            } => {
                assert_eq!(*days, 14);
                assert_eq!(*cleanup_days, Some(36500));
                assert_eq!(*effective_days, Some(36500));
            }
            other => panic!("expected Ceiling, got {other:?}"),
        }
        // Desktop ceiling above the CLI cleanup: the ceiling wins — the key
        // bounds an exemption, so it can only loosen, never tighten the
        // effective retention below cleanupPeriodDays.
        let check = inspect(
            &dir.path(),
            "{\"desktopSessionCleanupPeriodDays\": 20000, \"cleanupPeriodDays\": 30}",
            "{}",
            Some(30),
        );
        match &check.state {
            DesktopRetention::Ceiling { effective_days, .. } => {
                assert_eq!(*effective_days, Some(20000))
            }
            other => panic!("expected Ceiling, got {other:?}"),
        }
        // Unset cleanupPeriodDays means the runtime's 30-day default is the
        // comparison floor — and a ceiling below the safe threshold is a
        // live deletion window for transcripts that were exempt before.
        let check = inspect(
            &dir.path(),
            "{\"desktopSessionCleanupPeriodDays\": 15}",
            "{}",
            None,
        );
        match &check.state {
            DesktopRetention::Ceiling {
                cleanup_days,
                effective_days,
                ..
            } => {
                assert_eq!(*cleanup_days, Some(30));
                assert_eq!(*effective_days, Some(30));
            }
            other => panic!("expected Ceiling, got {other:?}"),
        }
        assert!(
            check.state.is_dangerous(),
            "an effective 30 days is the same live window D1 flags for the CLI side"
        );
        let line = check.risk_line();
        assert!(
            line.starts_with('🔴'),
            "a small effective ceiling is red; got: {line:?}"
        );
    }

    /// When D1's cleanupPeriodDays layer could not classify (parse failure),
    /// the longer-of-two comparison itself is unknown — stated as such,
    /// never silently dropped to the ceiling's own number.
    #[test]
    fn an_unclassifiable_cleanup_side_keeps_the_comparison_unknown() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path(),
            "user-settings.json",
            "{\"desktopSessionCleanupPeriodDays\": 90}",
        );
        write(&dir.path(), "managed-settings.json", "{}");
        // A parse-failed D1 verdict, which no day count can be built from.
        let parse_failed = ClaudeCheck {
            layers: vec![(
                PathBuf::from("/nowhere/.claude/settings.json"),
                ClaudeRetention::UnsetDefault,
            )],
            verdict: ClaudeRetention::ParseFailed {
                path: PathBuf::from("/nowhere/.claude/settings.json"),
                error: "synthetic parse failure".to_string(),
            },
        };
        let check = inspect_desktop_retention(&retention_registry(dir.path()), &parse_failed)
            .expect("the fixture registry declares the probe");
        match &check.state {
            DesktopRetention::Ceiling {
                cleanup_days,
                effective_days,
                ..
            } => {
                assert_eq!(*cleanup_days, None);
                assert_eq!(
                    *effective_days, None,
                    "the comparison cannot fabricate a floor from a parse failure"
                );
            }
            other => panic!("expected Ceiling with unknown floor, got {other:?}"),
        }
        let line = check.risk_line();
        assert!(
            line.contains("cannot be computed"),
            "the risk line states the comparison itself is unknown; got: {line:?}"
        );
    }

    /// The managed override (DSK-0a item 3): a managed source that sets
    /// `cleanupPeriodDays` makes the desktop key ignored entirely, whatever
    /// the user's file says — the state must name the managing source, or
    /// "overridden by org policy" reads as "the user set X".
    #[test]
    fn a_managed_cleanup_period_overrides_and_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let check = inspect(
            &dir.path(),
            "{\"desktopSessionCleanupPeriodDays\": 20000, \"cleanupPeriodDays\": 36500}",
            "{\"cleanupPeriodDays\": 45}",
            Some(45),
        );
        match &check.state {
            DesktopRetention::ManagedOverride { days, source } => {
                assert_eq!(*days, 45);
                assert_eq!(
                    source,
                    &dir.path().join("managed-settings.json"),
                    "the state names the managed source that governs"
                );
            }
            other => panic!("expected ManagedOverride, got {other:?}"),
        }
        let line = check.risk_line();
        assert!(
            line.contains("ignored") && line.contains("managed"),
            "the line says the desktop key is ignored, and why; got: {line:?}"
        );
        assert!(
            check.state.is_dangerous(),
            "a 45-day policy window is a live deletion window"
        );
    }

    /// The runtime's own precedence for the key: managed (policySettings)
    /// before user — the scope is "User or managed", so a managed desktop
    /// value wins over the user's, while a managed cleanup override never
    /// leaves room for the user's desktop value at all.
    #[test]
    fn a_managed_desktop_value_outranks_the_user_one() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path(),
            "user-settings.json",
            "{\"desktopSessionCleanupPeriodDays\": 5}",
        );
        write(
            dir.path(),
            "managed-settings.json",
            "{\"desktopSessionCleanupPeriodDays\": 400}",
        );
        let check = inspect_desktop_retention(
            &retention_registry(dir.path()),
            &claude_verdict(Some(36500)),
        )
        .expect("the fixture registry declares the probe");
        match &check.state {
            DesktopRetention::Ceiling {
                days,
                source,
                effective_days,
                ..
            } => {
                assert_eq!(*days, 400, "the managed value wins for this key");
                assert_eq!(
                    source,
                    &dir.path().join("managed-settings.json"),
                    "the governed value is attributed to the managed file"
                );
                assert_eq!(*effective_days, Some(36500));
            }
            other => panic!("expected Ceiling from the managed value, got {other:?}"),
        }
    }

    /// A settings file that is there but cannot be parsed (or holds a value
    /// this probe cannot interpret) keeps the state unknown — the runtime's
    /// fail-destructive fallback for this key would read as *unbounded*, but
    /// reporting that as if it were the configured value is exactly the
    /// "parse failure read as unset" lie D1 exists to prevent.
    #[test]
    fn broken_or_uninterpretable_sources_stay_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let check = inspect(
            &dir.path(),
            "{\"desktopSessionCleanupPeriodDays\": ",
            "{}",
            None,
        );
        assert!(
            matches!(check.state, DesktopRetention::Unknown { .. }),
            "a parse failure is unknown, never the (fail-open) default; got {:?}",
            check.state
        );
        let check = inspect(
            &dir.path(),
            "{\"desktopSessionCleanupPeriodDays\": -7}",
            "{}",
            None,
        );
        match &check.state {
            DesktopRetention::Unknown { why } => assert!(
                why.contains("not a nonnegative integer"),
                "an invalid value says what was found; got: {why:?}"
            ),
            other => panic!("expected Unknown for an invalid value, got {other:?}"),
        }
        let line = check.risk_line();
        assert!(
            line.starts_with('🟡') && line.contains("not guessing"),
            "an unknown state is yellow and refuses to guess; got: {line:?}"
        );
    }

    // The desktop key present in an unreadable file + the key's real value
    // in others? A file that exists but cannot be read: `fs::read_to_string`
    // with a directory in the path is the deterministic shape.
    #[test]
    fn an_unreadable_source_keeps_the_state_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let registry = retention_registry(dir.path());
        let unreadable = dir.path().join("user-settings.json");
        std::fs::create_dir(&unreadable).unwrap();
        std::fs::write(dir.path().join("managed-settings.json"), "{}").unwrap();
        let check = inspect_desktop_retention(&registry, &claude_verdict(None))
            .expect("the fixture registry declares the probe");
        assert!(
            matches!(check.state, DesktopRetention::Unknown { .. }),
            "a source that cannot be read keeps the state unknown; got {:?}",
            check.state
        );
    }

    /// A registry that declares no such probe means the check was not
    /// performed — `None`, never a state, never a default. The same answer
    /// a platform without a retention cell gives.
    #[test]
    fn a_registry_without_the_probe_is_not_checked() {
        let dir = tempfile::tempdir().unwrap();
        let no_probe: HarnessRegistry = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{
                "id": "claude-code",
                "display_name": "synthetic",
                "paths": {},
            }]
        }))
        .unwrap();
        assert!(
            inspect_desktop_retention(&no_probe, &claude_verdict(None)).is_none(),
            "no declared probe → not performed"
        );
        // A probe whose cells name only *other* platforms: same answer.
        let platform_key = scanner::current_platform();
        let other = if platform_key == "macos" {
            "linux"
        } else {
            "macos"
        };
        let only_elsewhere: HarnessRegistry = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{
                "id": "claude-code",
                "display_name": "synthetic",
                "paths": {},
                "retention_probes": [{
                    "id": "desktop-session-cleanup",
                    "key": "desktopSessionCleanupPeriodDays",
                    "states": {},
                    "platforms": { other: {
                        "user_sources": ["/nowhere/elsewhere/settings.json"],
                        "managed_sources": [],
                        "confidence": "measured-locally",
                        "source": "synthetic fixture"
                    }},
                }],
            }]
        }))
        .unwrap();
        assert!(
            inspect_desktop_retention(&only_elsewhere, &claude_verdict(None)).is_none(),
            "no cell for this platform → not performed"
        );
    }

    /// The D4 line rides the risks list when (and only when) the check ran —
    /// a script and a human read the same verdict, and the risks list grows
    /// by exactly one Claude-desktop line beside the existing Claude one.
    #[test]
    fn build_risks_carries_the_desktop_line_only_when_checked() {
        let dir = tempfile::tempdir().unwrap();
        let check = inspect(&dir.path(), "{}", "{}", None);
        let claude = claude_verdict(Some(36500));
        let gemini = GeminiRetention {
            enabled: false,
            ..GeminiRetention::default()
        };
        let with = build_risks(
            &claude,
            &gemini,
            &Some(check.clone()),
            &[],
            false,
            &CheckRunner::new(),
        );
        assert_eq!(with.len(), 3, "claude + desktop + gemini lines");
        assert!(
            with[1].starts_with('🟢') && with[1].contains("Claude Desktop/Cowork"),
            "the desktop line sits beside the Claude line, green for the unbounded default; got: {:?}",
            with[1]
        );
        let without = build_risks(&claude, &gemini, &None, &[], false, &CheckRunner::new());
        assert_eq!(without.len(), 2, "no declared probe → no desktop line");
        assert!(
            !without
                .iter()
                .any(|line| line.contains("Claude Desktop/Cowork")),
            "the only Claude lines left are the pre-existing ones"
        );
        // JSON mirrors the state: checked with tagged kinds, unchecked null.
        let value = desktop_retention_json(&check);
        assert_eq!(
            value["checked"],
            serde_json::json!(true),
            "a performed check says so"
        );
        assert_eq!(
            value["state"]["kind"],
            serde_json::json!("default_zero_unbounded")
        );
    }

    /// The sources table: one row per declared file, stating its kind and
    /// what it said — a managed source that set nothing renders "not set"
    /// beside an absent one, so which layer spoke is always readable.
    #[test]
    fn every_declared_source_gets_a_row_with_its_kind() {
        let dir = tempfile::tempdir().unwrap();
        let check = inspect(
            &dir.path(),
            "{\"desktopSessionCleanupPeriodDays\": 60}",
            "{\"cleanupPeriodDays\": 90}",
            Some(90),
        );
        assert_eq!(check.sources.len(), 2);
        let user = &check.sources[0];
        assert_eq!(user.kind, DesktopSourceKind::User);
        assert_eq!(
            user.outcome,
            DesktopSourceOutcome::Read {
                desktop_days: Some(60),
                cleanup_days: None,
            }
        );
        let managed = &check.sources[1];
        assert_eq!(managed.kind, DesktopSourceKind::Managed);
        // The managed file sets cleanupPeriodDays, so the override rule
        // decides the state — the desktop key the user file set is ignored
        // entirely. The source rows still record what each file said,
        // which is what makes the override readable instead of silent.
        assert_eq!(
            managed.outcome,
            DesktopSourceOutcome::Read {
                desktop_days: None,
                cleanup_days: Some(90),
            }
        );
        assert!(
            matches!(
                check.state,
                DesktopRetention::ManagedOverride { days: 90, .. }
            ),
            "a managed cleanupPeriodDays overrides the desktop key; got {:?}",
            check.state
        );
    }

    /// The JSON state variants a script needs: a managed override is tagged
    /// `ignored_key`, matching the "never read the override as a user
    /// choice" rule in two channels, not one.
    #[test]
    fn the_json_tags_the_managed_override_shape() {
        let dir = tempfile::tempdir().unwrap();
        let check = inspect(
            &dir.path(),
            "{\"desktopSessionCleanupPeriodDays\": 400}",
            "{\"cleanupPeriodDays\": 30}",
            None,
        );
        let value = desktop_retention_json(&check);
        assert_eq!(
            value["state"]["kind"],
            serde_json::json!("managed_override")
        );
        assert_eq!(
            value["state"]["ignored_key"],
            serde_json::json!(true),
            "a script must be able to tell a policy override from a user choice"
        );
        assert_eq!(value["state"]["days"], serde_json::json!(30));
    }

    /// The `--settings` invisibility note travels with every state — a
    /// one-run flag value that never lands on disk is expected to be absent
    /// from this probe, and the reader must not mistake that absence for a
    /// gap (DSK-0a item 4).
    #[test]
    fn every_state_carries_the_settings_flag_invisibility_note() {
        let dir = tempfile::tempdir().unwrap();
        let check = inspect(&dir.path(), "{}", "{}", None);
        assert!(
            check.risk_line().contains("--settings"),
            "the note names the invisible source; got: {}",
            check.risk_line()
        );
        // And the D1-block headline states the state, not just the risk.
        let label = check.state.label();
        assert!(
            label.contains("default 0 = no ceiling"),
            "the label answers 'what is the desktop retention' directly; got: {label:?}"
        );
    }

    // Helper: synthetic SessionRecords for the dir-probe seam test above,
    // in the shape W1006d's `session_record` helper uses.
    fn w1006d_lower_bound_records(i: u8) -> crate::models::SessionRecord {
        crate::models::SessionRecord {
            id: format!("claude-code.fixture.0000000{i}"),
            absolute_path: PathBuf::from(format!("/nowhere/projects/s{i}.jsonl")),
            byte_size: 1,
            mtime: UNIX_EPOCH,
            source: crate::models::HarnessSource::ClaudeCode,
            compressed: false,
            sqlite_layout: None,
            provenance: Default::default(),
            session_dir: None,
            movable_root: false,
        }
    }
}

#[cfg(test)]
mod w1043_deadline_tests {
    //! W1043 — a check that never answers must not become a hang or a lie.
    //!
    //! Rollback proof: before the change, `doctor` had no per-check deadline
    //! at all — one wedged check (a destination probe holding a connection
    //! that never answered was the observed case) froze the whole command
    //! with zero output, because every section printed only at the end.
    //! Each fixture below asserts the deadline machinery's answer, so
    //! reverting the harness makes the fixture red (or makes the wiped
    //! `timed_out` field fail to compile).

    use super::*;

    /// A check that answers within its budget passes its answer through
    /// unchanged and records nothing.
    #[test]
    fn a_check_that_answers_within_its_budget_passes_through() {
        let mut checks = CheckRunner::new();
        let answer = checks
            .run("D-test answers", 5, || 41 + 1)
            .expect("a check that answers must return Some");
        assert_eq!(answer, 42);
        assert!(checks.notes().is_empty(), "nothing timed out");
        assert_eq!(checks.timed_out_after("D-test answers"), None);
    }

    /// A check that never answers is abandoned at its budget: the note
    /// carries the label the trace printed, the run continues (this test
    /// would not finish otherwise), and the answer is `None` — the timeout
    /// and nothing else.
    #[test]
    fn a_check_that_outlives_its_budget_is_abandoned_with_a_note() {
        let mut checks = CheckRunner::new();
        let started = std::time::Instant::now();
        let answer: Option<u64> = checks.run("D-test wedged", 1, || {
            std::thread::sleep(std::time::Duration::from_secs(30));
            7
        });
        assert!(answer.is_none(), "the budget must be the only path to None");
        let waited = started.elapsed();
        // The abandoned worker keeps its own 30s sleep (detached — joining it
        // would be the hang again); this test only waits for the 1s budget,
        // with slack for a busy CI machine.
        assert!(
            waited < std::time::Duration::from_secs(20),
            "the run must continue once the budget expires, not wait the check out; waited \
             {waited:?}"
        );
        let [note] = checks.notes() else {
            panic!("exactly one note: {:?}", checks.notes());
        };
        assert_eq!(note.check, "D-test wedged");
        assert_eq!(note.budget_secs, 1);
        assert_eq!(checks.timed_out_after("D-test wedged"), Some(1));
    }

    /// A panicking check propagates its panic: the deadline machinery fences
    /// the wedge, not the bug.
    #[test]
    #[should_panic(expected = "synthetic checkpoint panic")]
    fn a_panicking_check_propagates_its_panic() {
        let mut checks = CheckRunner::new();
        let _ = checks.run("D-test panics", 5, || -> u8 {
            panic!("synthetic checkpoint panic");
        });
    }

    /// A timed-out D1 renders as an unknown in every channel: the verdict is
    /// its own `TimedOut` state, the JSON hides the placeholder behind
    /// `checked: false`, and the risk line words the unknown rather than the
    /// 30-day default.
    #[test]
    fn a_timed_out_d1_is_unknown_in_verdict_json_and_risk_line() {
        let mut report = minimal_w1043_report();
        report.claude = ClaudeCheck {
            layers: Vec::new(),
            verdict: ClaudeRetention::TimedOut,
        };
        report.timed_out.push(CheckTimedOut {
            check: D1_CHECK.to_string(),
            budget_secs: 30,
        });
        // JSON: `timed_out` named the check, and `claude` answers "not
        // checked" rather than a measurement about layers nobody read.
        let value = report_to_json(&report);
        assert_eq!(
            value["timed_out"],
            serde_json::json!([{"check": D1_CHECK, "budget_secs": 30}])
        );
        assert_eq!(
            value["claude"],
            serde_json::json!({"checked": false, "timed_out_after_secs": 30})
        );
        // The risk line words the unknown; it must not borrow the 30-day
        // default plain `UnsetDefault` would have stated.
        let checks = CheckRunner::new();
        let risks = build_risks(
            &report.claude,
            &report.gemini,
            &report.desktop_retention,
            &report.footprints,
            false,
            &checks,
        );
        let line = risks
            .iter()
            .find(|line| line.contains("Claude Code: the settings read did not answer"))
            .expect("the timed-out D1 risk line exists");
        assert!(
            line.contains("UNKNOWN, not the 30-day default and not proof of safety"),
            "the risk words the unknown, not a default: {line}"
        );
        // `is_dangerous` stays false: a non-answer is not a finding.
        assert!(!report.claude.verdict.is_dangerous());
        // `d1_effective_cleanup_days` keeps the CLI side of the Desktop
        // longer-of-two comparison unknown rather than borrowing a default.
        assert_eq!(d1_effective_cleanup_days(&report.claude.verdict), None);
    }

    /// A timed-out destination probe is its own outcome, never
    /// `unreachable`: the deadline expiring is a decision about the clock,
    /// and the JSON carries a kind a script can never confuse with the
    /// measured "the host said no".
    #[test]
    fn a_probe_timed_out_destination_is_not_an_unreachable_one() {
        let probe = DestinationProbe {
            name: "synthetic-destination".to_string(),
            repo_root: "opendal:synthetic".to_string(),
            outcome: DestinationOutcome::ProbeTimedOut { seconds: 45 },
            activity_index: None,
            debris: None,
        };
        let value = destination_probe_json(&probe);
        assert_eq!(value["kind"], serde_json::json!("probe_timed_out"));
        assert_eq!(value["budget_secs"], serde_json::json!(45));
        assert_eq!(
            value["activity_index"],
            serde_json::json!(null),
            "nothing after a deadline is 'not checked', never fine"
        );
        // `budget_secs` is the only number the probe outcome carries — the
        // runner-recorded budget (W1043f L6), not a second source that could
        // disagree with the `timed_out` note the run also prints.
    }

    /// W1043f (M1): a timed-out `Option`-shaped section keeps the `checked:
    /// false` placeholder a config-unreadable report gets, and adds the
    /// seconds it walked away at — the same shape D1/D2/machine_logs already
    /// had — so `--json` never words a wedged check's placeholder as a
    /// bare "did not look"; and without a note the plain `{"checked":
    /// false}` stands, so the config-unreadable story is unchanged.
    #[test]
    fn a_timed_out_option_section_carries_the_deadline_under_checked_false() {
        // Every `Option` section of `--json`, with the check label that owns
        // its deadline and the note that a wedged run leaves behind. The
        // labels are the same consts `run()` traces, so a section can never
        // claim a budget a different check waited out.
        let sections: [(&str, &str); 10] = [
            ("desktop_retention", DESKTOP_RETENTION_CHECK),
            ("reclaim", RECLAIM_CHECK),
            ("cache", CACHE_CHECK),
            ("body_cache", BODY_CACHE_CHECK),
            ("fts_indexes", FTS_CHECK),
            ("stage_duplicate_shards", STAGE_DUP_CHECK),
            ("keys", KEYS_CHECK),
            ("destination_reconciliation", DEST_RECONCILE_CHECK),
            ("native_host", NATIVE_HOST_CHECK),
            ("mirror", MIRROR_CHECK),
        ];
        for (key, check) in sections {
            let mut report = minimal_w1043_report();
            report.timed_out.push(CheckTimedOut {
                check: check.to_string(),
                budget_secs: 90,
            });
            let value = report_to_json(&report);
            assert_eq!(
                value[key],
                serde_json::json!({"checked": false, "timed_out_after_secs": 90}),
                "{key} must word the deadline, not a bare did-not-look"
            );
            // The note names the check itself, so a script that only reads
            // one section finds the culprit there too.
            assert_eq!(
                value["timed_out"],
                serde_json::json!([{"check": check, "budget_secs": 90}]),
                "the note rides at the top level: {value}"
            );
            // And without a note the plain did-not-look shape stands — the
            // config-unreadable report keeps the byte shape it always had.
            let value = report_to_json(&minimal_w1043_report());
            assert_eq!(
                value[key],
                serde_json::json!({"checked": false}),
                "{key} without a note is the plain not-checked tag"
            );
        }
    }

    /// W1043f (High): the report a wedged or broken config read produces
    /// carries **no** config to reuse — the CLI must not be able to
    /// substitute one by re-reading the file, which is the unbounded second
    /// read the review found. `not_checked` and `config_error` stay the one
    /// story about what was inspected.
    #[test]
    fn an_unreadable_config_report_carries_no_config_to_reuse() {
        let report = config_unreadable(
            CheckRunner::new(),
            "reading the configuration did not answer within 2s — the file exists but was \
             never read to the end, so whether it is usable is UNKNOWN, not invalid"
                .to_string(),
            ClaudeCheck {
                layers: Vec::new(),
                verdict: ClaudeRetention::TimedOut,
            },
            GeminiRetention::default(),
        );
        assert!(
            report.config.is_none(),
            "no usable config was read, so there is nothing for a caller to reuse"
        );
        assert!(report.config_error.is_some());
        assert!(
            !report.not_checked().is_empty(),
            "the did-not-look story is carried by not_checked"
        );
        // The healthy half of the invariant is enforced where it runs: a
        // report whose config was read carries it — the spawned-doctor
        // regression (`w1043_doctor_hang_test`) pins the end-to-end reuse
        // and the carrier's Some half against a planted config.
    }

    /// The smallest report whose sections the deadline machinery can hollow
    /// out: same skeleton `report_to_json_shape_tests` uses, with the
    /// optional checks left unfilled.
    fn minimal_w1043_report() -> DoctorReport {
        DoctorReport {
            config_source: crate::config::ConfigSource::DefaultsMissing,
            config_error: None,
            config: None,
            claude: ClaudeCheck {
                layers: Vec::new(),
                verdict: ClaudeRetention::UnsetDefault,
            },
            gemini: GeminiRetention::default(),
            desktop_retention: None,
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
            dest_reconcile: None,
            mirror: None,
            timed_out: Vec::new(),
        }
    }
}
