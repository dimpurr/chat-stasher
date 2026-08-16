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
pub const OTHER_HARNESS_DIRS: &[&str] = &[".cursor", ".windsurf", ".kimi-code", ".grok"];

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
        Ok(v) => match v.get("cleanupPeriodDays").and_then(serde_json::Value::as_u64) {
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
                ClaudeRetention::Safe { .. } | ClaudeRetention::SmallValue { .. } => Some(v.clone()),
                _ => None,
            })
            .unwrap_or(ClaudeRetention::UnsetDefault)
    };

    ClaudeCheck { layers: results, verdict }
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
}

impl Default for GeminiRetention {
    fn default() -> Self {
        // Gemini docs: enabled=true, maxAge="30d", minRetention="1d".
        GeminiRetention {
            enabled: true,
            max_age: "30d".to_string(),
            min_retention: "1d".to_string(),
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

    for path in [
        home.join(".gemini").join("settings.json"),
        home.join(".gemini").join("config.json"),
    ] {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let Some(sr) = v.get("sessionRetention") else {
            continue;
        };
        enabled = sr.get("enabled").and_then(serde_json::Value::as_bool).or(enabled);
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
    }
}

impl GeminiRetention {
    /// True when the harness is actively cleaning up on a 30-ish day window.
    pub fn is_dangerous(&self) -> bool {
        self.enabled && parse_duration_days(&self.max_age).map_or(true, |d| d <= 30.0)
    }

    /// Human summary of the policy.
    pub fn summarize(&self) -> String {
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
/// most signal — **earliest mtime**, because it is the one that answers
/// "how old is the oldest thing I've kept?" It directly bounds how much
/// history survives any retention policy.
#[derive(Debug, Clone)]
pub struct HarnessFootprint {
    pub name: String,
    pub root: PathBuf,
    /// False = not installed at all (NOT "0 sessions" — different meaning).
    pub installed: bool,
    /// `None` when the harness stores sessions in something non-enumerable
    /// by a file walk (e.g. opencode's single SQLite).
    pub session_count: Option<u64>,
    pub total_bytes: u64,
    pub earliest: Option<SystemTime>,
    pub latest: Option<SystemTime>,
    pub compressed_count: u64,
    pub note: String,
}

/// Aggregate per-harness details.
pub fn coverage_from_records<'a>(
    name: &str,
    root: PathBuf,
    recs: impl Iterator<Item = &'a crate::models::SessionRecord>,
) -> HarnessFootprint {
    let recs: Vec<_> = recs.collect();
    let installed = root.is_dir();
    let total_bytes = recs.iter().map(|r| r.byte_size).sum();
    let earliest = recs.iter().map(|r| r.mtime).min();
    let latest = recs.iter().map(|r| r.mtime).max();
    let compressed_count = recs.iter().filter(|r| r.compressed).count() as u64;
    HarnessFootprint {
        name: name.to_string(),
        root,
        installed,
        session_count: if installed { Some(recs.len() as u64) } else { None },
        total_bytes,
        earliest,
        latest,
        compressed_count,
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

fn days_since(t: SystemTime) -> f64 {
    match SystemTime::now().duration_since(t) {
        Ok(d) => d.as_secs_f64() / 86400.0,
        Err(_) => 0.0,
    }
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

fn gemini_footprint(home: &Path) -> HarnessFootprint {
    let root = home.join(".gemini").join("tmp");
    if !root.is_dir() {
        return HarnessFootprint {
            name: "gemini".to_string(),
            root,
            installed: false,
            session_count: None,
            total_bytes: 0,
            earliest: None,
            latest: None,
            compressed_count: 0,
            note: "not installed (no ~/.gemini/tmp)".to_string(),
        };
    }
    let mut count = 0;
    let mut total = 0u64;
    let mut earliest: Option<SystemTime> = None;
    let mut latest: Option<SystemTime> = None;
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            // session files (vs logs.json / checkpoint.json / bin/rg)
            if name.starts_with("session-") && name.ends_with(".json") {
                if let Ok(md) = fs::metadata(&path) {
                    count += 1;
                    total += md.len();
                    let m = md.modified().unwrap_or(UNIX_EPOCH);
                    earliest = Some(earliest.map_or(m, |e: SystemTime| e.min(m)));
                    latest = Some(latest.map_or(m, |l: SystemTime| l.max(m)));
                }
            }
        }
    }
    HarnessFootprint {
        name: "gemini".to_string(),
        root,
        installed: true,
        session_count: Some(count),
        total_bytes: total,
        earliest,
        latest,
        compressed_count: 0,
        note: format!("{count} session files under ~/.gemini/tmp"),
    }
}

fn opencode_footprint(home: &Path) -> HarnessFootprint {
    let root = home.join(".local").join("share").join("opencode");
    let db = root.join("opencode.db");
    if !db.is_file() {
        return HarnessFootprint {
            name: "opencode".to_string(),
            root: root.clone(),
            installed: db.exists() || root.exists(),
            session_count: None,
            total_bytes: 0,
            earliest: None,
            latest: None,
            compressed_count: 0,
            note: "not installed (no opencode.db)".to_string(),
        };
    }
    let mut total = 0u64;
    let mut latest: Option<SystemTime> = None;
    for f in fs::read_dir(&root).into_iter().flatten().flatten() {
        let p = f.path();
        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("opencode.db") {
                if let Ok(md) = fs::metadata(&p) {
                    total += md.len();
                    let m = md.modified().unwrap_or(UNIX_EPOCH);
                    latest = Some(latest.map_or(m, |l: SystemTime| l.max(m)));
                }
            }
        }
    }
    HarnessFootprint {
        name: "opencode".to_string(),
        root,
        installed: true,
        // Sessions live inside one SQLite; enumerating them needs a sqlite
        // reader we are not adding in this spike. Honest "N/A", not a guess.
        session_count: None,
        total_bytes: total,
        earliest: None,
        latest,
        compressed_count: 0,
        note: "single SQLite (opencode.db); count not enumerable here".to_string(),
    }
}

// ---------------------------------------------------------------------------
// D4 — risk synthesis (the whole value of this command)
// ---------------------------------------------------------------------------

fn default_footprint(name: &str, root: PathBuf) -> HarnessFootprint {
    HarnessFootprint {
        name: name.to_string(),
        root,
        installed: false,
        session_count: None,
        total_bytes: 0,
        earliest: None,
        latest: None,
        compressed_count: 0,
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
            let earliest = claude_fp.earliest.map(format_date).unwrap_or_else(|| "n/a".to_string());
            let days_old = claude_fp.earliest.map(days_since).unwrap_or(0.0);
            risks.push(format!(
                "🔴 Claude Code: cleanupPeriodDays 未设置 → 默认 30 天。你最早的会话是 {earliest}（约 {days_old:.0} 天前，今天 {today}）。\
                 \n    —— 下一次清理触发时会删掉早于 30 天的第一批；你的历史只还剩约 {days_old:.0} 天。"
            ));
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
    let gem_days = gem_fp.and_then(|f| f.earliest).map(days_since);
    if !gem_fp.map_or(true, |f| f.installed) {
        // nothing to clean: skip silently to not cry wolf
    } else if gemini.is_dangerous() {
        risks.push(match gem_earliest {
            Some(date) => {
                let over = gem_days.unwrap_or(0.0) - 30.0;
                format!(
                    "🔴 Gemini: sessionRetention 未配置（默认 30 天、enabled=true）。你最早的会话是 {date}（约 {:.0} 天前，今天 {today}）——已经越过 30 天门槛约 {over:.0} 天。\
                     \n    —— 清理当前尚未触发（或未执行），但下一次运行会删掉最早那批。会话就在 ~/.gemini/tmp（目录名就叫 tmp）。",
                    gem_days.unwrap_or(0.0),
                )
            }
            None => format!(
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
                let count = cx.session_count.unwrap_or(0);
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

    // --- opencode ------------------------------------------------------------
    if let Some(oc) = footprints.iter().find(|f| f.name == "opencode") {
        if oc.installed {
            risks.push(format!(
                "🟢 opencode: 单一 SQLite（{}，{}）—— v1.2.0 起无按天数轮换；风险来自它自身的 SQLite，而非静默删会话。",
                oc.root.display(),
                fmt_bytes(oc.total_bytes)
            ));
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
    pub claude: ClaudeCheck,
    pub gemini: GeminiRetention,
    pub footprints: Vec<HarnessFootprint>,
    pub other_present: Vec<PathBuf>,
    pub risks: Vec<String>,
    pub reclaim: ReclaimCheck,
    /// Per-harness fate decided by the path registry (`scanner::scan`).
    pub probes: Vec<scanner::HarnessProbe>,
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

    // D3 — reuse the crate's scanner for claude-code & codex session files,
    // walk gemini/openccde roots directly.
    let config = Config::load();
    let (scan, scan_failed) = match scanner::scan(&config) {
        Ok(s) => (s, false),
        Err(e) => {
            eprintln!("doctor: scan failed: {e}");
            (scanner::ScanReport::default(), true)
        }
    };

    let mut footprints = Vec::new();

    let claude_root = config
        .claude_projects_dir
        .as_deref()
        .map(expand_tilde)
        .unwrap_or_else(|| home.join(".claude").join("projects"));
    footprints.push(coverage_from_records(
        "claude-code",
        claude_root,
        scan.records.iter().filter(|r| r.source == crate::models::HarnessSource::ClaudeCode),
    ));

    let codex_root = config
        .codex_sessions_dir
        .as_deref()
        .map(expand_tilde)
        .unwrap_or_else(|| home.join(".codex").join("sessions"));
    footprints.push(coverage_from_records(
        "codex",
        codex_root,
        scan.records.iter().filter(|r| r.source == crate::models::HarnessSource::Codex),
    ));

    footprints.push(gemini_footprint(&home));
    footprints.push(opencode_footprint(&home));

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
    DoctorReport { claude, gemini, footprints, other_present, risks, reclaim, probes, scan_failed }
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
    use rustic_core::{
        Credentials, PruneOptions, PrunePlan, Repository, RepositoryOptions,
    };

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

    // No repository configured and none at the default location → nothing to
    // plan. This is the "no repo / no remote wired yet" skip.
    if !repo_root.is_dir() {
        return ReclaimCheck::NoRepo { repo_root };
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
        Err(e) => return ReclaimCheck::NoKey { key_file, error: format!("{e:#}") },
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
        Err(e) => return ReclaimCheck::OpenFailed { repo_root, error: format!("{e:#}") },
    };
    let repo = match Repository::new(&RepositoryOptions::default(), &backends) {
        Ok(r) => r,
        Err(e) => return ReclaimCheck::OpenFailed { repo_root: repo_root.clone(), error: format!("{e:#}") },
    };
    let repo = match repo.open(&Credentials::Masterkey(mk)) {
        Ok(r) => r,
        Err(e) => return ReclaimCheck::OpenFailed { repo_root: repo_root.clone(), error: format!("{e:#}") },
    };
    let repo = match repo.to_indexed() {
        Ok(r) => r,
        Err(e) => return ReclaimCheck::OpenFailed { repo_root: repo_root.clone(), error: format!("{e:#}") },
    };
    let append_only = repo.config().append_only == Some(true);

    // The meat: plan-only, read-only, allowed even under append_only.
    let plan = match PrunePlan::from_prune_options(&repo, &PruneOptions::default()) {
        Ok(p) => p,
        Err(e) => return ReclaimCheck::OpenFailed { repo_root, error: format!("{e:#}") },
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
    crate::config::home_dir().join(".local").join("share").join("chat-stasher")
}

// ---------------------------------------------------------------------------
// Printing — never session contents, only ids/paths/counts/bytes/timestamps.
// ---------------------------------------------------------------------------

pub fn print_report(r: &DoctorReport) {
    println!();
    println!("doctor — “你的 harness 正在偷偷删你的数据吗？”");
    println!("      只读探测；只打印路径 / 计数 / 字节 / 时间戳，绝不打印会话正文。");
    println!();

    // D1
    println!("D1 · Claude Code 的轮转设置");
    println!(
        "  cleanupPeriodDays：{} （{}）",
        r.claude.verdict.label(),
        match &r.claude.verdict {
            ClaudeRetention::UnsetDefault => "未设置 → 默认 30 天 = 危险".to_string(),
            ClaudeRetention::Safe { .. } => "已设大值，安全，但见 D4 的 fail-destructive 提醒".to_string(),
            ClaudeRetention::SmallValue { days, .. } => {
                format!("已显式设置 {days} 天，低于安全阈值 = 仍在轮换")
            }
            ClaudeRetention::ParseFailed { path, error } => {
                format!("🔴 文件在但解析失败（这是 fail-destructive 的触发条件）：{} — {error}", path.display())
            }
        }
    );
    for (path, v) in &r.claude.layers {
        println!("    {:<52} {}", path.display(), v.label());
    }
    println!();

    // D2
    println!("D2 · Gemini CLI 的保留策略");
    println!("  sessionRetention：{}", r.gemini.summarize());
    let home = crate::config::home_dir();
    let present: Vec<String> = [".gemini/config.json", ".gemini/settings.json"]
        .iter()
        .filter(|p| home.join(p).is_file())
        .map(|p| home.join(p).display().to_string())
        .collect();
    if present.is_empty() {
        println!("  配置文件都不存在 → 使用 CLI 内置默认值（enabled=true, maxAge=30d）。");
    } else {
        println!("  来源文件：{}", present.join(", "));
    }
    println!();

    // D3
    if r.scan_failed {
        println!("D3 · 覆盖率 —— 🔴 registry 缺失/无法解析，会话覆盖未知。");
        println!(
            "    拒绝用硬编码路径假装扫全（stderr 上方已有 “Refusing to scan with hardcoded roots”）。"
        );
        println!(
            "    仅列出与本 registry 无关、来自独立只读探测的条目：",
        );
        for f in &r.footprints {
            if f.name != "gemini" && f.name != "opencode" {
                continue;
            }
            if !f.installed {
                println!("  {:<10} 未安装（{}）", f.name, f.root.display());
                continue;
            }
            let count = f.session_count.map(|c| c.to_string()).unwrap_or_else(|| "N/A".to_string());
            let earliest = f.earliest.map(format_date).unwrap_or_else(|| "-".to_string());
            let latest = f.latest.map(format_date).unwrap_or_else(|| "-".to_string());
            println!(
                "  {:<10} 会话 {:<6} · {} · 最早 {earliest} · 最晚 {latest}",
                f.name,
                count,
                fmt_bytes(f.total_bytes)
            );
            if !f.note.is_empty() {
                println!("             ({})", f.note);
            }
        }
        println!();
        println!("D4 · 风险汇总 —— 所以会发生什么 + 什么时候");
        for (i, risk) in r.risks.iter().enumerate() {
            println!("  {}. {risk}", i + 1);
        }
        println!();
        println!("D5 · 仓库里有多少可回收的垃圾？");
        println!("     `prune_plan` 只算不删 —— doctor 永远不执行 prune，也不碰 append_only。");
        print_reclaim(&r.reclaim);
        println!();
        return;
    }

    let (known, installed) = (r.probes.len(), r.probes.iter().filter(|p| p.installed_p()).count());
    println!(
        "D3 · 覆盖率 —— 本机 {installed}/{known} 个已知 harness 命中（registry v1 驱动）；轮转分析对象如下：",
    );
    for f in &r.footprints {
        if !f.installed {
            println!(
                "  {:<10} 未安装（{}）",
                f.name,
                f.root.display()
            );
            continue;
        }
        let count = f.session_count.map(|c| c.to_string()).unwrap_or_else(|| "N/A".to_string());
        let earliest = f.earliest.map(format_date).unwrap_or_else(|| "-".to_string());
        let latest = f.latest.map(format_date).unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<10} 会话 {:<6} · {} · 最早 {earliest} · 最晚 {latest}",
            f.name,
            count,
            fmt_bytes(f.total_bytes)
        );
        if !f.note.is_empty() {
            println!("             ({})", f.note);
        }
    }
    if !r.other_present.is_empty() {
        let others: Vec<String> = r
            .other_present
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        println!("  已装但不在本命令范围（仅探测，不分析轮转）：{}", others.join(", "));
    }
    print_probes(&r.probes);
    println!();

    // D4
    println!("D4 · 风险汇总 —— 所以会发生什么 + 什么时候");
    if r.risks.is_empty() {
        println!("  （没有可合成的判断）");
    }
    for (i, risk) in r.risks.iter().enumerate() {
        println!("  {}. {risk}", i + 1);
    }
    println!();

    // D5 — reclaimable garbage in the archive repository (prune_plan, read-only)
    println!("D5 · 仓库里有多少可回收的垃圾？");
    println!("     `prune_plan` 只算不删 —— doctor 永远不执行 prune，也不碰 append_only。");
    print_reclaim(&r.reclaim);
    println!();
}

/// D5 printing — shared by the normal path and the scan-failed early return.
fn print_reclaim(r: &ReclaimCheck) {
    match r {
        ReclaimCheck::NoRepo { repo_root } => {
            println!(
                "  （跳过）没有仓库目录：{} —— 没仓库就没有垃圾，也不用诊断。",
                repo_root.display()
            );
        }
        ReclaimCheck::NoKey { key_file, error } => {
            println!(
                "  （跳过）仓库目录在，但 masterkey 读不了（{}）：{error}—— \
                 打不开就无可规划，先确认 key 文件没丢。",
                key_file.display()
            );
        }
        ReclaimCheck::OpenFailed { repo_root, error } => {
            println!(
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
            println!(
                "  未被引用的 pack    : {packs_unref} 个 · {}（`packs_unref`/`size_unref`）",
                fmt_bytes(*size_unref)
            );
            println!(
                "  需要 repack 的量   : {packs_repack} 个 pack · {}（`packs.repack`/`size.repack`）",
                fmt_bytes(*size_repack)
            );
            if *packs_unref > 0 || *size_unref > 0 || *packs_repack > 0 || *size_repack > 0 {
                println!("  🔴 这些垃圾现在清不掉 —— 本仓库是 append_only（{append_only}）。");
                println!(
                    "     `prune`/`repair`/`rewrite --forget` 都被 append_only 挡死（源码 `commands/prune.rs`）。"
                );
                println!(
                    "     要清的标准流程：临时关掉 append_only → 跑一次 `rustic prune --instant-delete` → 再开回 append_only。"
                );
                println!(
                    "     ⚠️ 必须带 `--instant-delete`：普通 `prune` 的默认 `--keep-delete 23h` 只把 pack 标记为待删、"
                );
                println!(
                    "        23 小时宽限期后才真删 —— 实测即使关掉 append_only 也会停在 nothing to do!，垃圾根本没走。"
                );
                println!(
                    "     ⚠️ 代价：`--instant-delete` 跳过这 23 小时宽限期 —— 删了就没了，没有反悔窗口；执行前先确认上面两个数字确实是垃圾。"
                );
                println!(
                    "     ⚠️ 这要求临时关掉 append_only —— 那是你的安全设置：关掉到再开回之间仓库失去“防误删”的保险网，"
                );
                println!(
                    "        只在这个窗口期内操作、先做一次备份，操作完立刻开回。"
                );
                println!(
                    "     doctor 是只读诊断：它只报告数字和操作步骤，绝不会替你执行 prune 或切换 append_only。"
                );
            } else {
                println!("  ✅ 没有任何可回收的垃圾 —— 不用清。");
                if *append_only {
                    println!("     （append_only=true，即便有垃圾也只会被挡下，不会自动发生误删。）");
                }
            }
        }
    }
    println!();
}

/// D3 supplement — the registry-driven probe table: every harness the
/// registry listed for this platform, whether it was scanned / exists here,
/// and which cells were flagged low-confidence or skipped (`未查明`, other
/// platform, template not statically resolvable).
fn print_probes(probes: &[scanner::HarnessProbe]) {
    if probes.is_empty() {
        println!("  （registry 探测为空 —— 扫描未运行，或没有候选 harness。）");
        return;
    }
    let platform = scanner::current_platform();
    let hit = probes.iter().filter(|p| p.installed_p()).count();
    println!(
        "  [registry] 驱动表 —— 平台={platform} · 本机命中/扫描成功 {hit}/{n} 个 harness",
        n = probes.len()
    );
    for p in probes {
        let mark = match p.state {
            scanner::ProbeState::Scanned => "已扫    ",
            scanner::ProbeState::FileTarget => "单文件  ",
            scanner::ProbeState::Missing => "不存在  ",
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
            scanner::ProbeState::FileTarget => format!(" bytes={}", fmt_bytes(p.bytes)),
            _ => String::new(),
        };
        let extra = if p.note.is_empty() {
            String::new()
        } else {
            format!("  ({})", p.note)
        };
        println!(
            "    {mark} {:<16} {conf:<26} 会话={:<4}{bytes:<0} {root}{extra}",
            p.display_name, p.record_count
        );
    }
    let flagged = probes
        .iter()
        .filter(|p| p.low_confidence_p())
        .map(|p| p.display_name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if !flagged.is_empty() {
        println!("    低置信（仅社区说法未核实，扫了但不可当作核实）：{flagged}");
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
            ClaudeRetention::Safe { days: 99999, source: dir.path().join(".claude/settings.json") }
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
        write(&dir.path().join(".claude/settings.json"), r#"{ "cleanupPeriodDays": 99999, "#);
        let check = inspect_claude_settings(dir.path());
        assert!(matches!(check.verdict, ClaudeRetention::ParseFailed { .. }));
        assert!(check.verdict.is_dangerous());
        assert!(!check.layers.is_empty());
    }

    /// 坏 JSON 优先于同一层里的好值：另一个 layer 合法也不能拯救 parse 失败。
    #[test]
    fn broken_json_outranks_valid_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".claude/settings.json"), r#"{ "cleanupPeriodDays": 99999, "#);
        write(&dir.path().join(".claude/settings.local.json"), r#"{ "cleanupPeriodDays": 99999 }"#);
        let check = inspect_claude_settings(dir.path());
        assert!(matches!(check.verdict, ClaudeRetention::ParseFailed { .. }));
    }

    /// Gemini：空配置（只有 model）→ 默认 30 天 → dangerous。
    #[test]
    fn gemini_unset_retention_is_default_dangerous() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".gemini/config.json"), r#"{ "model": "gemini-3.1-pro-preview" }"#);
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