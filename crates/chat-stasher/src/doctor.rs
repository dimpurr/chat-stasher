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
use crate::scanner;
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
pub const OTHER_HARNESS_DIRS: &[&str] = &[".cursor", ".windsurf", ".kimi-code"];

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
            return format!("未知（配置读不出来：{error}）");
        }
        if !self.enabled {
            "disabled (enabled=false) — safe".to_string()
        } else {
            match parse_duration_days(&self.max_age) {
                Some(d) if d > 30.0 => format!(
                    "enabled, maxAge={} (~{d:.0}d) — large, safe-ish",
                    self.max_age
                ),
                _ => format!(
                    "enabled, maxAge={} ({:.1} days) — DEFAULT-ish, dangerous",
                    self.max_age,
                    parse_duration_days(&self.max_age).unwrap_or(30.0)
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// D3 — coverage: which harnesses exist, and all of them accounted for?
// ---------------------------------------------------------------------------

/// One harness's session footprint: count, bytes, and — the column with the
/// most signal — **earliest session timestamp/mtime**, because it answers
/// "how old is the oldest thing I've kept?" It directly bounds how much
/// history survives any retention policy.
#[derive(Debug, Clone)]
pub struct HarnessFootprint {
    pub name: String,
    /// `None` means the registry never resolved a path; it is not an empty
    /// path and must not render as `未安装（）`.
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
    /// second one gets printed as 未知; see [`footprint_count_detail`].
    pub unreadable_count: Option<u64>,
    /// Directory entries/subtrees that could not be inspected. The number of
    /// sessions behind them is unknown, so this is not folded into
    /// `unreadable_count`.
    pub unreadable_entry_count: Option<u64>,
    /// `None` means the footprint was not measured; it is not an empty store.
    pub total_bytes: Option<u64>,
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
/// a number when measured, `未知` when this installed store could not be
/// measured, and `N/A` when the harness does not apply on this machine.
fn footprint_bytes_label(f: &HarnessFootprint) -> String {
    match f.total_bytes {
        Some(bytes) => format!("{} ({} B)", fmt_bytes(bytes), bytes),
        None if f.installed => "未知".to_string(),
        None => "N/A".to_string(),
    }
}

fn footprint_root_label(f: &HarnessFootprint) -> String {
    f.root
        .as_ref()
        .map(|root| root.display().to_string())
        .unwrap_or_else(|| "未知".to_string())
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
/// never resolved a root (`未查明` cell, template not statically resolvable, no
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
            note: "registry 没有本平台条目 —— 会话数未知（不是 0）".to_string(),
            ..default_footprint(name, fallback_root)
        };
    };
    let root = probe.root.clone().unwrap_or(fallback_root);
    if !probe.installed_p() {
        return HarnessFootprint {
            note: if probe.note.is_empty() {
                "registry 没有扫描这个 harness —— 会话数未知（不是 0）".to_string()
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
            "🔴 registry 缺失/无法解析 —— 会话覆盖未知，拒绝用硬编码路径假装扫全。".to_string(),
        );
    } else {
        match &claude.verdict {
            ClaudeRetention::UnsetDefault => {
                // B90: these two values used to be read from the *same*
                // `Option` and disagree about whether it was known — the date
                // printed an honest `n/a` while the day count quietly became
                // `0`. The result was one sentence, half of it measured and
                // half of it invented, and the invented half ("你的历史只还剩约
                // 0 天") is the kind that makes a reader act *now*: it reads as
                // "your archive is deleted tomorrow".
                //
                // The risk itself does not depend on the earliest session —
                // cleanupPeriodDays being unset means 30-day rotation whatever
                // is on disk — so the line is kept and only the fabricated
                // number is removed. Dropping the whole risk instead would
                // trade a false alarm for a missing one.
                risks.push(match claude_fp.earliest.and_then(|e| days_since(e).map(|d| (e, d))) {
                    Some((earliest, days_old)) => {
                        format!(
                            "🔴 Claude Code: cleanupPeriodDays 未设置 → 默认 30 天。你最早的会话是 {}（约 {days_old:.0} 天前，今天 {today}）。\
                             \n    —— 下一次清理触发时会删掉早于 30 天的第一批；你的历史只还剩约 {days_old:.0} 天。",
                            format_date(earliest)
                        )
                    }
                    None => format!(
                        "🔴 Claude Code: cleanupPeriodDays 未设置 → 默认 30 天。最早会话时间未知（本机没扫到 claude-code 会话，其时间戳读不出来，或它落在未来），因此还剩多少天无法估算（今天 {today}）。\
                         \n    —— 风险不因此消失：下一次清理触发时仍会删掉早于 30 天的第一批。"
                    ),
                });
            }
            ClaudeRetention::Safe { days, source } => {
                risks.push(format!(
                "🟡 Claude Code: cleanupPeriodDays = {days} 天（已设大值，来源 {}）→ 本机历史不至于被轮换。\
                 \n    —— 但这是 fail-destructive：任何一次 settings.json 解析失败都会静默退回 30 天并开始删；`.last-cleanup` 时间戳存在即说明清理兼职曾运行。",
                source.display()
            ));
            }
            ClaudeRetention::SmallValue { days, source } => {
                risks.push(format!(
                "🔴 Claude Code: cleanupPeriodDays = {days} 天（来源 {}）→ 低于安全阈值，仍在轮换。\
                 \n    —— 下一次清理触发时会删掉早于 {days} 天的第一批。",
                source.display()
            ));
            }
            ClaudeRetention::ParseFailed { path, error } => {
                risks.push(format!(
                "🔴🔴 Claude Code: settings 解析失败—— 这就是 fail-destructive bug 的触发条件本身：\
                 \n    —— 无论你配置过什么，现在都已静默退回 30 天开始计算删除。文件 {}：{error}",
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
            "🟡 Gemini: sessionRetention 未知（配置读不出来：{error}）—— 不用默认值猜测保留风险。"
        ));
    } else if !gem_fp.map_or(true, |f| f.installed) {
        // nothing to clean: skip silently to not cry wolf
    } else if gemini.is_dangerous() {
        // B90: `(gem_earliest, gem_days)` are two `map`s over one `Option`, so
        // the `unwrap_or(0.0)` that used to sit in the `Some(date)` arm was
        // unreachable today — and was exactly the shape of the Claude bug
        // above, one refactor away from printing an invented "0 天前". Matched
        // as a pair, the fabricated default has nowhere left to live.
        risks.push(match (gem_earliest, gem_days) {
            (Some(date), Some(days)) => {
                let over = days - 30.0;
                format!(
                    "🔴 Gemini: sessionRetention 未配置（默认 30 天、enabled=true）。你最早的会话是 {date}（约 {days:.0} 天前，今天 {today}）——已经越过 30 天门槛约 {over:.0} 天。\
                     \n    —— 清理当前尚未触发（或未执行），但下一次运行会删掉最早那批。会话就在 ~/.gemini/tmp（目录名就叫 tmp）。",
                )
            }
            _ => format!(
                "🔴 Gemini: sessionRetention 未配置（默认 30 天、enabled=true），但没有发现 session-*.json 文件可判定最早会话。\
                 \n    —— 一旦开始跑，30 天后就会开始删。"
            ),
        });
    } else {
        risks.push(format!("🟢 Gemini: {} — 无风险。", gemini.summarize()));
    }

    // --- Codex --------------------------------------------------------------
    // Scan-count dependent: omitted (not fabricated) when the scan failed.
    if !scan_failed {
        if let Some(cx) = footprints.iter().find(|f| f.name == "codex") {
            if cx.installed {
                let count = footprint_count_label(cx);
                risks.push(if cx.compressed_count > 0 {
                    format!(
                        "🟡 Codex: 源码没有按天数自动删除的机制，但 {n} 个空闲 rollout 已被压成 .jsonl.zst —— \
                         \n    —— 无需现在行动，空闲即压缩，与\"删除\"无关。",
                        n = cx.compressed_count
                    )
                } else {
                    format!(
                        "🟢 Codex: 源码没有按天数自动删除的机制（本机 {count} 个 rollout 均未压缩）——无风险。"
                    )
                });
            }
        }
    }

    // --- opencode / cursor / grok (single-SQLite stores, registry-driven) ----
    for (fp_name, label) in [
        ("opencode", "opencode"),
        ("cursor", "Cursor"),
        ("grok", "Grok"),
    ] {
        if let Some(fp) = footprints.iter().find(|f| f.name == fp_name) {
            if fp.installed {
                risks.push(format!(
                    "🟢 {label}: 单一 SQLite（{}，{}）—— 无按天数轮换；风险来自它自身的 SQLite，而非静默删会话。",
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

/// Result of one full `doctor` run.
#[derive(Debug)]
pub struct DoctorReport {
    pub config_source: crate::config::ConfigSource,
    pub claude: ClaudeCheck,
    pub gemini: GeminiRetention,
    pub footprints: Vec<HarnessFootprint>,
    pub other_present: Vec<PathBuf>,
    pub risks: Vec<String>,
    pub reclaim: ReclaimCheck,
    /// Per-harness fate decided by the path registry (`scanner::scan`).
    pub probes: Vec<scanner::HarnessProbe>,
    /// Registry-recognised sessions that are not represented by a
    /// `SessionRecord` and therefore cannot be consumed by `collect`.
    pub archive_gaps: Vec<scanner::ArchiveGap>,
    /// True when the registry-driven scan failed (registry missing/unparseable)
    /// — the coverage numbers are then *unknown*, never faked zeros.
    pub scan_failed: bool,
}

/// Run every check against the real machine and assemble the report.
pub fn run() -> DoctorReport {
    let home = crate::config::home_dir();

    // D1
    let claude = inspect_claude_settings(&home);

    // D2
    let gemini = inspect_gemini_settings(&home);

    // D3 — use the registry-driven scanner for every directory harness. The
    // doctor no longer has a second Gemini suffix/pattern implementation.
    let config = Config::load();
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

    // opencode, Cursor and Grok are single-SQLite stores driven by the registry: their
    // footprint rows are built straight from the registry probe results, so
    // the two tables can never disagree on count/bytes/times.
    for id in ["opencode", "cursor", "grok"] {
        match scan.probes.iter().find(|p| p.id == id) {
            Some(probe) => footprints.push(footprint_from_sqlite_probe(probe)),
            None => {
                let root = match id {
                    "opencode" => scanner::xdg_data_home().join("opencode/opencode.db"),
                    "cursor" => home
                        .join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"),
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

    let probes = scan.probes;
    DoctorReport {
        config_source: config.source,
        claude,
        gemini,
        footprints,
        other_present,
        risks,
        reclaim,
        probes,
        archive_gaps,
        scan_failed,
    }
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        crate::config::home_dir().join(rest)
    } else if p == "~" {
        crate::config::home_dir()
    } else {
        PathBuf::from(p)
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
    use rustic_core::{Credentials, PruneOptions, PrunePlan, Repository, RepositoryOptions};

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
    // has the wrong shape; only NotFound proves "no repository".
    match fs::metadata(&repo_root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return ReclaimCheck::OpenFailed {
                repo_root,
                error: "仓库路径存在但不是目录".to_string(),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return ReclaimCheck::NoRepo { repo_root }
        }
        Err(e) => {
            return ReclaimCheck::OpenFailed {
                repo_root,
                error: format!("无法确认仓库目录：{e}"),
            }
        }
    }

    let cfg = StoreConfig {
        repo_root: repo_root.to_string_lossy().into_owned(),
        key_file: key_file.clone(),
        connections: 1,
        options: BTreeMap::new(),
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
    let repo = match Repository::new(&RepositoryOptions::default(), &backends) {
        Ok(r) => r,
        Err(e) => {
            return ReclaimCheck::OpenFailed {
                repo_root: repo_root.clone(),
                error: format!("{e:#}"),
            }
        }
    };
    let repo = match repo.open(&Credentials::Masterkey(mk)) {
        Ok(r) => r,
        Err(e) => {
            return ReclaimCheck::OpenFailed {
                repo_root: repo_root.clone(),
                error: format!("{e:#}"),
            }
        }
    };
    let repo = match repo.to_indexed() {
        Ok(r) => r,
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

/// Default data dir for the repository + key file (mirrors main.rs).
fn default_data_root() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("chat-stasher");
    }
    crate::config::home_dir()
        .join(".local")
        .join("share")
        .join("chat-stasher")
}

// ---------------------------------------------------------------------------
// Printing — never session contents, only ids/paths/counts/bytes/timestamps.
// ---------------------------------------------------------------------------

fn footprint_count_label(f: &HarnessFootprint) -> String {
    match f.session_count {
        Some(count) => count.to_string(),
        // Installed but the store is not enumerable (schema not recognised):
        // "unknown", never a fake zero.
        None if f.installed => "未知".to_string(),
        None => "N/A".to_string(),
    }
}

fn footprint_count_detail(f: &HarnessFootprint) -> String {
    let mut parts: Vec<String> = Vec::new();
    if f.name == "cursor" {
        if let (Some(before), Some(after)) = (f.candidate_count, f.session_count) {
            parts.push(format!("过滤前 {before} / 过滤后 {after}"));
        }
    }
    // B68: the number that used to be invisible. "Filtered out" and "could not
    // be read" are different answers to "where did my sessions go", so the
    // second one gets its own count instead of hiding inside the first.
    // Silent when zero: an all-good line is byte-for-byte what it was.
    match f.unreadable_count {
        Some(unreadable) if unreadable > 0 => parts.push(format!("{unreadable} 条读不出来")),
        // Counted, and it is zero: silence, byte-for-byte as before.
        Some(_) => {}
        // B90: this row *was* enumerated (`session_count` is `Some`) and only
        // the unreadable tally failed. Saying nothing here is byte-identical
        // to the counted-zero line above, i.e. it reads as "nothing missed" —
        // which is precisely the claim that was never earned. Rows that were
        // never enumerated at all keep quiet: `installed`/`session_count`
        // already say so, and repeating it would put 未知 on every
        // not-installed harness of a healthy machine.
        None if f.session_count.is_some() => {
            parts.push("有多少条读不出来 未知（这一项本身没数出来）".to_string())
        }
        None => {}
    }
    if let Some(entries) = f.unreadable_entry_count.filter(|n| *n > 0) {
        parts.push(format!("{entries} 个不可读目录项（会话数未知）"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("（{}）", parts.join("，"))
}

pub fn print_report(r: &DoctorReport) {
    eprintln!();
    eprintln!("doctor — “你的 harness 正在偷偷删你的数据吗？”");
    eprintln!("      只读探测；只打印路径 / 计数 / 字节 / 时间戳，绝不打印会话正文。");
    if r.config_source.is_error_fallback() {
        eprintln!("config_source={}", r.config_source.label());
    }
    eprintln!();

    // D1
    eprintln!("D1 · Claude Code 的轮转设置");
    eprintln!(
        "  cleanupPeriodDays：{} （{}）",
        r.claude.verdict.label(),
        match &r.claude.verdict {
            ClaudeRetention::UnsetDefault => "未设置 → 默认 30 天 = 危险".to_string(),
            ClaudeRetention::Safe { .. } =>
                "已设大值，安全，但见 D4 的 fail-destructive 提醒".to_string(),
            ClaudeRetention::SmallValue { days, .. } => {
                format!("已显式设置 {days} 天，低于安全阈值 = 仍在轮换")
            }
            ClaudeRetention::ParseFailed { path, error } => {
                format!(
                    "🔴 文件在但解析失败（这是 fail-destructive 的触发条件）：{} — {error}",
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
    eprintln!("D2 · Gemini CLI 的保留策略");
    eprintln!("  sessionRetention：{}", r.gemini.summarize());
    let home = crate::config::home_dir();
    let present: Vec<String> = [".gemini/config.json", ".gemini/settings.json"]
        .iter()
        .filter(|p| home.join(p).is_file())
        .map(|p| home.join(p).display().to_string())
        .collect();
    if present.is_empty() {
        eprintln!("  配置文件都不存在 → 使用 CLI 内置默认值（enabled=true, maxAge=30d）。");
    } else {
        eprintln!("  来源文件：{}", present.join(", "));
    }
    eprintln!();

    // D3
    if r.scan_failed {
        eprintln!("D3 · 覆盖率 —— 🔴 registry 缺失/无法解析，会话覆盖未知。");
        eprintln!(
            "    拒绝用硬编码路径假装扫全（stderr 上方已有 “Refusing to scan with hardcoded roots”）。"
        );
        eprintln!("    仅列出与本 registry 无关、来自独立只读探测的条目：",);
        for f in &r.footprints {
            if f.name != "gemini" && f.name != "opencode" {
                continue;
            }
            if !f.installed {
                eprintln!("  {:<10} 未安装（{}）", f.name, footprint_root_label(f));
                continue;
            }
            let count = footprint_count_label(f);
            let count_detail = footprint_count_detail(f);
            let earliest = f
                .earliest
                .map(format_timestamp)
                .unwrap_or_else(|| "-".to_string());
            let latest = f
                .latest
                .map(format_timestamp)
                .unwrap_or_else(|| "-".to_string());
            eprintln!(
                "  {:<10} 会话 {:<6}{} · {} · 最早 {earliest} · 最晚 {latest}",
                f.name,
                count,
                count_detail,
                footprint_bytes_label(f)
            );
            if !f.note.is_empty() {
                eprintln!("             ({})", f.note);
            }
        }
        eprintln!();
        eprintln!("D4 · 风险汇总 —— 所以会发生什么 + 什么时候");
        for (i, risk) in r.risks.iter().enumerate() {
            eprintln!("  {}. {risk}", i + 1);
        }
        eprintln!();
        eprintln!("D5 · 仓库里有多少可回收的垃圾？");
        eprintln!("     `prune_plan` 只算不删 —— doctor 永远不执行 prune，也不碰 append_only。");
        print_reclaim(&r.reclaim);
        eprintln!();
        return;
    }

    let (known, installed) = (
        r.probes.len(),
        r.probes.iter().filter(|p| p.installed_p()).count(),
    );
    eprintln!(
        "D3 · 覆盖率 —— 本机 {installed}/{known} 个已知 harness 命中（registry v1 驱动）；轮转分析对象如下：",
    );
    for f in &r.footprints {
        if !f.installed {
            eprintln!("  {:<10} 未安装（{}）", f.name, footprint_root_label(f));
            continue;
        }
        let count = footprint_count_label(f);
        let count_detail = footprint_count_detail(f);
        let earliest = f
            .earliest
            .map(format_timestamp)
            .unwrap_or_else(|| "-".to_string());
        let latest = f
            .latest
            .map(format_timestamp)
            .unwrap_or_else(|| "-".to_string());
        eprintln!(
            "  {:<10} 会话 {:<6}{} · {} · 最早 {earliest} · 最晚 {latest}",
            f.name,
            count,
            count_detail,
            footprint_bytes_label(f)
        );
        if !f.note.is_empty() {
            eprintln!("             ({})", f.note);
        }
    }
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
            "  已装但不在本命令范围（仅探测，不分析轮转）：{}",
            others.join(", ")
        );
    }
    print_archive_gaps(&r.archive_gaps);
    print_probes(&r.probes);
    eprintln!();

    // D4
    eprintln!("D4 · 风险汇总 —— 所以会发生什么 + 什么时候");
    if r.risks.is_empty() {
        eprintln!("  （没有可合成的判断）");
    }
    for (i, risk) in r.risks.iter().enumerate() {
        eprintln!("  {}. {risk}", i + 1);
    }
    eprintln!();

    // D5 — reclaimable garbage in the archive repository (prune_plan, read-only)
    eprintln!("D5 · 仓库里有多少可回收的垃圾？");
    eprintln!("     `prune_plan` 只算不删 —— doctor 永远不执行 prune，也不碰 append_only。");
    print_reclaim(&r.reclaim);
    eprintln!();
}

/// D5 printing — shared by the normal path and the scan-failed early return.
fn print_reclaim(r: &ReclaimCheck) {
    match r {
        ReclaimCheck::NoRepo { repo_root } => {
            eprintln!(
                "  （跳过）没有仓库目录：{} —— 没仓库就没有垃圾，也不用诊断。",
                repo_root.display()
            );
        }
        ReclaimCheck::NoKey { key_file, error } => {
            eprintln!(
                "  （跳过）仓库目录在，但 masterkey 读不了（{}）：{error}—— \
                 打不开就无可规划，先确认 key 文件没丢。",
                key_file.display()
            );
        }
        ReclaimCheck::OpenFailed { repo_root, error } => {
            eprintln!(
                "  （跳过）仓库打不开 / 算不出计划：{} —— {error}",
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
                "  未被引用的 pack    : {packs_unref} 个 · {}（`packs_unref`/`size_unref`）",
                fmt_bytes(*size_unref)
            );
            eprintln!(
                "  需要 repack 的量   : {packs_repack} 个 pack · {}（`packs.repack`/`size.repack`）",
                fmt_bytes(*size_repack)
            );
            if *packs_unref > 0 || *size_unref > 0 || *packs_repack > 0 || *size_repack > 0 {
                eprintln!("  🔴 这些垃圾现在清不掉 —— 本仓库是 append_only（{append_only}）。");
                eprintln!(
                    "     `prune`/`repair`/`rewrite --forget` 都被 append_only 挡死（源码 `commands/prune.rs`）。"
                );
                eprintln!(
                    "     要清的标准流程：临时关掉 append_only → 跑一次 `rustic prune --instant-delete` → 再开回 append_only。"
                );
                eprintln!(
                    "     ⚠️ 必须带 `--instant-delete`：普通 `prune` 的默认 `--keep-delete 23h` 只把 pack 标记为待删、"
                );
                eprintln!(
                    "        23 小时宽限期后才真删 —— 实测即使关掉 append_only 也会停在 nothing to do!，垃圾根本没走。"
                );
                eprintln!(
                    "     ⚠️ 代价：`--instant-delete` 跳过这 23 小时宽限期 —— 删了就没了，没有反悔窗口；执行前先确认上面两个数字确实是垃圾。"
                );
                eprintln!(
                    "     ⚠️ 这要求临时关掉 append_only —— 那是你的安全设置：关掉到再开回之间仓库失去“防误删”的保险网，"
                );
                eprintln!("        只在这个窗口期内操作、先做一次备份，操作完立刻开回。");
                eprintln!(
                    "     doctor 是只读诊断：它只报告数字和操作步骤，绝不会替你执行 prune 或切换 append_only。"
                );
            } else {
                eprintln!("  ✅ 没有任何可回收的垃圾 —— 不用清。");
                if *append_only {
                    eprintln!(
                        "     （append_only=true，即便有垃圾也只会被挡下，不会自动发生误删。）"
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
        "  ⚠ 不可归档会话：以下 harness 已识别会话，但未产出 SessionRecord；collect 当前不会归档它们。"
    );
    for gap in gaps {
        eprintln!("{}", scanner::format_archive_gap(gap));
    }
    eprintln!(
        "  建议：不要把 scanner records 当作已识别会话总数；等对应 harness 产出 SessionRecord 后再运行 collect。"
    );
}

/// D3 supplement — the registry-driven probe table: every harness the
/// registry listed for this platform, whether it was scanned / exists here,
/// and which cells were flagged low-confidence or skipped (`未查明`, other
/// platform, template not statically resolvable).
fn print_probes(probes: &[scanner::HarnessProbe]) {
    if probes.is_empty() {
        eprintln!("  （registry 探测为空 —— 扫描未运行，或没有候选 harness。）");
        return;
    }
    let platform = scanner::current_platform();
    let hit = probes.iter().filter(|p| p.installed_p()).count();
    eprintln!(
        "  [registry] 驱动表 —— 平台={platform} · 本机命中/扫描成功 {hit}/{n} 个 harness",
        n = probes.len()
    );
    for p in probes {
        let mark = match p.state {
            scanner::ProbeState::Scanned => "已扫    ",
            scanner::ProbeState::FileTarget => "单文件  ",
            scanner::ProbeState::Missing => "不存在  ",
            scanner::ProbeState::Indeterminate => "查不出来",
            scanner::ProbeState::SkipUnascertained => "跳过(未查明) ",
            scanner::ProbeState::SkipWrongPlatform => "跨平台  ",
            scanner::ProbeState::SkipUnresolvable => "跳过(模板)   ",
        };
        let conf = if p.low_confidence_p() {
            format!("[低置信] {}", p.confidence.label())
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
                p.bytes.map(fmt_bytes).unwrap_or_else(|| "未知".to_string())
            ),
            _ => String::new(),
        };
        // 会话数，三态（与本文件其它表同一套词汇）：
        //   数字   —— 枚举成功，这就是数
        //   未知   —— 有理由认为可能有，但这次没能枚举出来
        //   N/A    —— 这台机器上不适用（registry 没有本平台的 cell）
        // B82: 以前除 Scanned/FileTarget 外一律印 "0"。跳过(未查明)、
        // 跳过(模板)、查不出来 三种都是「我没看」，印 0 就是把没查过说成
        // 查过且为空 —— 这正是本单要消灭的那句谎。「不存在」保留 0，因为
        // 那是真的查过：路径不在。
        let count = match p.state {
            scanner::ProbeState::FileTarget => match p.record_count {
                Some(c) => c.to_string(),
                None => "未知".to_string(),
            },
            // B90: `unwrap_or(0)` here is unreachable today (a Scanned probe
            // always carries a count) — which is exactly why it was worth
            // removing: it is a fallback that would print the B82 lie again
            // the moment the invariant moves. Same shape as FileTarget above.
            scanner::ProbeState::Scanned => match p.record_count {
                Some(c) => c.to_string(),
                None => "未知".to_string(),
            },
            scanner::ProbeState::Missing => "0".to_string(),
            scanner::ProbeState::SkipWrongPlatform => "N/A".to_string(),
            scanner::ProbeState::Indeterminate
            | scanner::ProbeState::SkipUnascertained
            | scanner::ProbeState::SkipUnresolvable => "未知".to_string(),
        };
        let extra = if p.note.is_empty() {
            String::new()
        } else {
            format!("  ({})", p.note)
        };
        eprintln!(
            "    {mark} {:<16} {conf:<26} 会话={:<4}{bytes:<0} {root}{extra}",
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
        eprintln!("    低置信（仅社区说法未核实，扫了但不可当作核实）：{flagged}");
    }
}

// ---------------------------------------------------------------------------
// Tests — fake home dirs: normal, missing, and broken JSON.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    /// 正常：合法 JSON + 大 cleanupPeriodDays。
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

    /// 缺失：整个 settings 树都不存在 → UnsetDefault（不是"0"，是"默认 30 天"）。
    #[test]
    fn claude_missing_settings_is_unset_default() {
        let dir = tempfile::tempdir().unwrap();
        let check = inspect_claude_settings(dir.path());
        assert_eq!(check.verdict, ClaudeRetention::UnsetDefault);
        assert!(check.verdict.is_dangerous());
    }

    /// 坏 JSON：文件在但解析失败 → ParseFailed（fail-destructive 触发条件）。
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

    /// 坏 JSON 优先于同一层里的好值：另一个 layer 合法也不能拯救 parse 失败。
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

    /// Gemini：空配置（只有 model）→ 默认 30 天 → dangerous。
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

    /// Gemini：显式 enabled=false → safe。
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
            earliest: None,
            latest: None,
            compressed_count: 0,
            recognized_files: Vec::new(),
            note: String::new(),
        }
    }

    /// **B90 / A 的显示端反证。** 会话枚举成功（`session_count` 有值），
    /// 但「有多少条读不出来」这一项本身没数出来。旧代码在这里一声不吭，
    /// 和「数过了，是 0」的输出逐字相同 —— 读的人分不出来。
    #[test]
    fn an_uncounted_unreadable_tally_shows_up_as_unknown() {
        let detail = footprint_count_detail(&footprint(None));
        assert!(
            detail.contains("未知"),
            "数不出来就要显示成「未知」，不许沉默地等同于 0；实际：{detail:?}"
        );
    }

    #[test]
    fn an_unavailable_footprint_bytes_stay_unknown() {
        let f = default_footprint("fixture", PathBuf::from("/nowhere"));
        assert_eq!(footprint_bytes_label(&f), "N/A");
        assert_eq!(footprint_bytes_label(&footprint(None)), "未知");
    }

    /// 健康机器上「它不响」：数过了、就是 0 时，这一格逐字为空。
    #[test]
    fn a_counted_zero_stays_silent() {
        assert_eq!(footprint_count_detail(&footprint(Some(0))), "");
    }

    /// 数过了、非 0 时，照旧印那个数字。
    #[test]
    fn a_counted_number_still_prints_itself() {
        assert!(footprint_count_detail(&footprint(Some(411))).contains("411 条读不出来"));
    }

    /// 压根没枚举过的行（`session_count` 也是 `None`）不该被这条新规则
    /// 拖出一句「未知」：它的状态字段已经说过了，这里多说一遍就是噪音。
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
