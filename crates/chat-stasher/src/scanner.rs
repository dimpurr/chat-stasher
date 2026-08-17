//! Local harness scanner — driven by the harness path registry
//! (`data/harness-registry-v1.json`), which is authoritative for which
//! harness × platform roots to probe. Nothing is hardcoded here: a root is
//! only scanned when the registry says to scan it.
//!
//! Each registry cell (harness × platform) carries a `confidence`:
//!   - `未查明`            ⇒ the cell is **not** scanned (guessing a path would
//!                            produce false negatives/positives),
//!   - `仅社区说法未核实`   ⇒ scanned, but flagged **low-confidence**, so the
//!                            report can mark it instead of presenting it as fact,
//!   - `本机实测`          ⇒ verified empirically on the machine this tool runs
//!                            on (schema/path measured live) but source not read —
//!                            scanned, not low-confidence,
//!   - `源码确认`/`官方文档` ⇒ scanned normally.
//!
//! Path templates are expanded before probing (`~`, `$HOME`, `$XDG_*`,
//! `%USERPROFILE%`), with the standard XDG fallbacks: `$XDG_DATA_HOME`
//! unset ⇒ `$HOME/.local/share`, `$XDG_CONFIG_HOME` unset ⇒ `$HOME/.config`,
//! `$XDG_STATE_HOME` unset ⇒ `$HOME/.local/state`. Cells for platforms other
//! than the current one, and templates that cannot be reduced to a static
//! root (e.g. CWD-relative stores), are skipped with an explicit reason.
//!
//! If the registry file is missing or unparseable the scan **fails loudly** —
//! silently falling back to hardcoded paths would make a user believe
//! everything was scanned when it was not.
//!
//! Hard guarantees:
//!   * read-only — nothing is opened for writing and file bodies are never
//!     read, so no session content can ever leak into a log or report,
//!   * `.jsonl.zst` files are *recognised and listed* (compressed flag set);
//!     decompression is a later spike but silently skipping them is not an
//!     option,
//!   * missing roots are reported, not fatal.

use crate::config::Config;
use crate::models::{HarnessSource, SessionRecord};
use crate::sqlite_probe::{probe_sqlite_store, SqliteSessionProbe};
use serde::Deserialize;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Registry file, relative to the repo root, that drives the scan.
pub const REGISTRY_REL_PATH: &str = "data/harness-registry-v1.json";

/// Confidence values used by the registry (`harnesses[].paths.*.confidence`).
pub const CONF_CONFIRMED: &str = "源码确认";
pub const CONF_OFFICIAL: &str = "官方文档";
pub const CONF_COMMUNITY: &str = "仅社区说法未核实";
pub const CONF_MEASURED: &str = "本机实测";
pub const CONF_UNASCERTAINED: &str = "未查明";

// ---------------------------------------------------------------------------
// Confidence
// ---------------------------------------------------------------------------

/// How trustworthy a registry cell is. Anything not in the known set is
/// treated as [`Confidence::Unascertained`] — conservative, never scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// `源码确认` — confirmed in harness source.
    Confirmed,
    /// `官方文档` — confirmed in official docs.
    OfficialDocs,
    /// `仅社区说法未核实` — plausible but never verified. Scan, but flag low.
    CommunityClaim,
    /// `本机实测` — verified empirically on this machine (path + schema read
    /// live), but the harness source was not read. Scan, not flagged low —
    /// it is verified, just not source-anchored (so it can drift on an app
    /// update without any source link to re-check).
    Measured,
    /// `未查明` — unknown. Do **not** scan (guessing = false pos/neg).
    Unascertained,
}

impl Confidence {
    /// Classify the registry's raw confidence string.
    pub fn classify(raw: &str) -> Confidence {
        match raw {
            CONF_CONFIRMED => Confidence::Confirmed,
            CONF_OFFICIAL => Confidence::OfficialDocs,
            CONF_COMMUNITY => Confidence::CommunityClaim,
            CONF_MEASURED => Confidence::Measured,
            _ => Confidence::Unascertained,
        }
    }

    /// True when this cell is safe to scan.
    pub fn scan_allowed(&self) -> bool {
        !matches!(self, Confidence::Unascertained)
    }

    /// True when the cell is a community claim that should be flagged in output.
    pub fn is_low_confidence(&self) -> bool {
        matches!(self, Confidence::CommunityClaim)
    }

    /// Short label used in reports.
    pub fn label(&self) -> &'static str {
        match self {
            Confidence::Confirmed => CONF_CONFIRMED,
            Confidence::OfficialDocs => CONF_OFFICIAL,
            Confidence::CommunityClaim => CONF_COMMUNITY,
            Confidence::Measured => CONF_MEASURED,
            Confidence::Unascertained => CONF_UNASCERTAINED,
        }
    }
}

// ---------------------------------------------------------------------------
// XDG base directories
// ---------------------------------------------------------------------------

/// Core of the XDG fallback: `$KEY` if set, else `$home/<fallback_rel>`.
fn xdg_base_with(key: &str, home: &Path, fallback_rel: &str) -> PathBuf {
    match env::var_os(key) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home.join(fallback_rel),
    }
}

/// `$XDG_DATA_HOME`, falling back to `~/.local/share`.
pub fn xdg_data_home() -> PathBuf {
    xdg_base_with("XDG_DATA_HOME", &crate::config::home_dir(), ".local/share")
}

/// `$XDG_CONFIG_HOME`, falling back to `~/.config`.
pub fn xdg_config_home() -> PathBuf {
    xdg_base_with("XDG_CONFIG_HOME", &crate::config::home_dir(), ".config")
}

/// `$XDG_STATE_HOME`, falling back to `~/.local/state`.
pub fn xdg_state_home() -> PathBuf {
    xdg_base_with("XDG_STATE_HOME", &crate::config::home_dir(), ".local/state")
}

/// Currently running platform's registry key.
pub fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

// ---------------------------------------------------------------------------
// Registry schema
// ---------------------------------------------------------------------------

/// Top level of `data/harness-registry-v1.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct HarnessRegistry {
    #[serde(default)]
    pub schema_version: u32,
    pub generated: String,
    pub harnesses: Vec<RegistryHarness>,
}

/// One harness entry.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryHarness {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub paths: RegistryPaths,
    #[serde(default)]
    pub notes: Option<String>,
    /// Active-file sealing policy for this harness: `rename` / `no-rename` /
    /// `not-applicable`. Lives here — the *same* file that drives scanning —
    /// so the sealing allowlist can never drift from the path registry. Any
    /// missing / unknown value resolves to `no-rename` (fail-safe, see
    /// `crate::seal`). To be on the `rename` list a harness needs a `源码确认`
    /// platform cell **and** a non-empty `seal_source` evidence line.
    #[serde(default)]
    pub seal_policy: String,
    /// Concrete measurement / source line backing `seal_policy`. Required for
    /// `rename`; the gate in `crate::seal::seal_allowed` rejects empty values,
    /// so an uncredited harness can never be rename-sealed.
    #[serde(default)]
    pub seal_source: String,
}

/// The three platform cells. Missing keys deserialize to `None`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegistryPaths {
    #[serde(default)]
    pub macos: Option<RegistryCell>,
    #[serde(default)]
    pub linux: Option<RegistryCell>,
    #[serde(default)]
    pub windows: Option<RegistryCell>,
}

impl RegistryPaths {
    /// The cell for `platform` ("macos"/"linux"/"windows"), if any.
    pub fn cell_for(&self, platform: &str) -> Option<&RegistryCell> {
        match platform {
            "macos" => self.macos.as_ref(),
            "linux" => self.linux.as_ref(),
            "windows" => self.windows.as_ref(),
            _ => None,
        }
    }
}

/// One harness × platform cell.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryCell {
    pub template: String,
    #[serde(default)]
    pub format: String,
    /// Optional basename glob, e.g. `session-*`. This is required for
    /// generic JSON roots where settings/state/config files share the suffix.
    #[serde(default)]
    pub session_pattern: Option<String>,
    #[serde(default)]
    pub confidence: String,
    #[serde(default)]
    pub source: String,
    // --- SQLite-store schema (B27) ---------------------------------------
    // These fields let a `format: "sqlite"` cell declare how the store keeps
    // its sessions, so both the scanner and the doctor run the *same* probe
    // (`crate::sqlite_probe::spec_from_cell`) instead of each hardcoding a
    // schema. All optional: a cell without `sql_table` falls back to the
    // default opencode `session` schema (pre-B27 behaviour).
    /// Table that holds one row per session (e.g. `cursorDiskKV`).
    #[serde(default)]
    pub sql_table: Option<String>,
    /// Columns that must all exist before the store is recognised.
    #[serde(default)]
    pub sql_required_columns: Vec<String>,
    /// Key/value-store selection: column + LIKE prefix selecting session rows
    /// (e.g. `key` + `composerData:%`).
    #[serde(default)]
    pub sql_key_column: Option<String>,
    #[serde(default)]
    pub sql_key_pattern: Option<String>,
    /// Column holding the JSON that carries a session timestamp (Cursor's
    /// `value`); combined with `sql_time_json_path`.
    #[serde(default)]
    pub sql_value_column: Option<String>,
    /// Column holding a per-session epoch timestamp.
    #[serde(default)]
    pub sql_time_column: Option<String>,
    /// JSON path (e.g. `$.createdAt`) into `sql_value_column`, epoch millis.
    #[serde(default)]
    pub sql_time_json_path: Option<String>,
    /// True when `sql_time_column` holds Unix seconds instead of millis.
    #[serde(default)]
    pub sql_time_value_is_seconds: bool,
}

/// Deserialize the registry from `path`. Returns a descriptive error on any
/// failure (missing file, bad JSON, wrong shape) — the caller must treat this
/// as a **loud failure**, never fall back to hardcoded roots.
pub fn load_registry(path: &Path) -> io::Result<HarnessRegistry> {
    let raw = fs::read_to_string(path)?;
    serde_json::from_str(&raw).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("harness registry {} is not valid JSON: {e}", path.display()),
        )
    })
}

/// Locate the registry by walking **up from the cwd and the package manifest
/// dir**, so the CLI works from the repo root *and* from inside the crate.
fn resolve_registry_path() -> Option<PathBuf> {
    let mut starts = vec![env::current_dir().ok()];
    if let Some(manifest) = env::var_os("CARGO_MANIFEST_DIR") {
        starts.push(Some(PathBuf::from(manifest)));
    }
    for start in starts {
        if let Some(mut dir) = start {
            loop {
                let candidate = dir.join(REGISTRY_REL_PATH);
                if candidate.is_file() {
                    return Some(candidate);
                }
                match dir.parent() {
                    Some(p) => dir = p.to_path_buf(),
                    None => break,
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Probe model
// ---------------------------------------------------------------------------

/// What happened to one harness on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeState {
    /// Directory root walked, session files collected.
    Scanned,
    /// Single-file store (sqlite.db) probed for existence + size.
    FileTarget,
    /// Scanned but the root / file does not exist here.
    Missing,
    /// Skipped: confidence `未查明` — scanning a guessed path is forbidden.
    SkipUnascertained,
    /// Skipped: no registry cell for this platform.
    SkipWrongPlatform,
    /// Skipped: template does not reduce to a static root (e.g. `$CWD`/`<...>`
    /// unresolvable, or harness id unknown to this build).
    SkipUnresolvable,
}

/// One harness's fate on this machine, as decided by the registry.
#[derive(Debug, Clone)]
pub struct HarnessProbe {
    pub id: String,
    pub display_name: String,
    /// Path actually probed (registry-derived, config overrides for claude/codex
    /// still win). `None` for skips that never resolved a path.
    pub root: Option<PathBuf>,
    pub confidence: Confidence,
    pub state: ProbeState,
    /// Session records found at this harness's root. `None` when this harness
    /// cannot be enumerated at all (missing, skipped, single-file store whose
    /// schema we do not recognise) — never a fabricated zero.
    pub record_count: Option<u64>,
    /// Earliest / latest session timestamp, for single-file SQLite stores that
    /// carry one (via the registry-declared schema). `None` when unknown.
    pub earliest: Option<SystemTime>,
    pub latest: Option<SystemTime>,
    /// For single-file stores: the store's bytes on disk (`.db` + sidecars for
    /// SQLite). 0 otherwise.
    pub bytes: u64,
    pub note: String,
}

impl HarnessProbe {
    /// True when this harness actually exists on the machine.
    pub fn installed_p(&self) -> bool {
        matches!(self.state, ProbeState::Scanned | ProbeState::FileTarget)
    }

    /// True when the cell was a community claim the report must flag.
    pub fn low_confidence_p(&self) -> bool {
        self.confidence.is_low_confidence()
    }
}

/// Roots that a scan walked, and which of them were missing.
#[derive(Debug, Default)]
pub struct ScanReport {
    pub records: Vec<SessionRecord>,
    /// Roots (as resolved) that did not exist on this machine.
    pub missing_roots: Vec<PathBuf>,
    /// Per-harness fate, in registry order.
    pub probes: Vec<HarnessProbe>,
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

/// Walk every registry harness that applies to this platform and collect
/// metadata-only records.
///
/// The registry is located by walking up from the cwd / package manifest dir.
/// If it is missing or unparseable this **errors** — no silent fallback. Any
/// harness whose cell is confidence `未查明` is skipped; `仅社区说法未核实`
/// cells are scanned but the probe is flagged low-confidence.
pub fn scan(config: &Config) -> io::Result<ScanReport> {
    let registry = load_registry_from_repo()?;
    scan_with_registry(config, &registry)
}

/// Resolve + parse the registry from the repo layout (walking up from the cwd
/// and `CARGO_MANIFEST_DIR`). Shared by the scanner and the sealing allowlist
/// (`crate::seal`) so both read the exact same file.
pub fn load_registry_from_repo() -> io::Result<HarnessRegistry> {
    let Some(path) = resolve_registry_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "harness registry not found: {REGISTRY_REL_PATH} (searched from cwd \
                 and CARGO_MANIFEST_DIR upward). Refusing to scan with hardcoded roots."
            ),
        ));
    };
    load_registry(&path)
}

/// Registry-driven scan against an already-loaded [`HarnessRegistry`];
/// separate entry so tests can feed it a scratch registry without touching
/// the user's HOME or the repo's `data/` file.
pub fn scan_with_registry(config: &Config, registry: &HarnessRegistry) -> io::Result<ScanReport> {
    let machine = crate::id::machine_id();
    let platform = current_platform();
    let mut report = ScanReport::default();

    for h in &registry.harnesses {
        let probe = probe_harness(config, h, platform, &machine, &mut report);
        report.probes.push(probe);
    }
    Ok(report)
}

fn probe_harness(
    config: &Config,
    h: &RegistryHarness,
    platform: &str,
    machine: &str,
    report: &mut ScanReport,
) -> HarnessProbe {
    let base = HarnessProbe {
        id: h.id.clone(),
        display_name: h.display_name.clone(),
        root: None,
        confidence: Confidence::Unascertained,
        state: ProbeState::SkipUnresolvable,
        record_count: None,
        earliest: None,
        latest: None,
        bytes: 0,
        note: String::new(),
    };

    // 1. Current-platform cell must exist.
    let Some(cell) = h.paths.cell_for(platform) else {
        return HarnessProbe {
            note: format!("无 {platform} 平台条目"),
            ..base
        };
    };

    // 2. Confidence decides whether we scan at all.
    let confidence = Confidence::classify(&cell.confidence);
    if !confidence.scan_allowed() {
        return HarnessProbe {
            confidence,
            state: ProbeState::SkipUnascertained,
            note: format!("confidence=未查明，跳过不去扫（扫猜的路径 = 假阴/假阳）"),
            ..base
        };
    }

    // 3. The template must reduce to a static root we can probe.
    let Some((mut root, is_file)) = static_prefix_root(&cell.template) else {
        return HarnessProbe {
            confidence,
            state: ProbeState::SkipUnresolvable,
            note: format!("模板无法静态解析为根路径：{}", cell.template),
            ..base
        };
    };

    // 4. Config overrides for the two legacy harnesses still win (and keep the
    //    sandboxed integration tests deterministic).
    root = root_for_id(&h.id, root, config);

    // 5. This build must know the harness, or we refuse to label its files.
    let Some(source) = HarnessSource::from_id(&h.id) else {
        return HarnessProbe {
            confidence,
            state: ProbeState::SkipUnresolvable,
            note: format!("registry id 在此 build 中未知：{}", h.id),
            ..base
        };
    };

    // 6. Single-file stores are probed for existence; directory roots are walked.
    if is_file {
        match fs::metadata(&root) {
            Ok(md) if md.is_file() => {
                let mut probe = HarnessProbe {
                    root: Some(root.clone()),
                    confidence,
                    state: ProbeState::FileTarget,
                    bytes: md.len(),
                    record_count: None,
                    note: "单文件存储，只探测存在与大小".to_string(),
                    ..base
                };
                if cell.format == "sqlite" {
                    // The registry-driven scan and the doctor footprint table
                    // share ONE SQLite probe (`crate::sqlite_probe`), so the
                    // same harness can never show two different counts again.
                    // The schema spec also comes from the registry cell, so
                    // the scan and the doctor enumerate with the same query.
                    let spec = crate::sqlite_probe::spec_from_cell(cell);
                    let info = match &spec {
                        Some(spec) => crate::sqlite_probe::probe_sqlite_store_with(&root, spec),
                        None => probe_sqlite_store(&root),
                    };
                    probe.bytes = info.total_bytes;
                    let expected = spec
                        .map(|s| {
                            format!(
                                "{}表({})",
                                s.table,
                                s.required_columns.join(", ")
                            )
                        })
                        .unwrap_or_else(|| {
                            "session(id, time_created, time_updated)".to_string()
                        });
                    match info.sessions {
                        SqliteSessionProbe::Known {
                            count,
                            earliest,
                            latest,
                        } => {
                            probe.record_count = Some(count);
                            probe.earliest = earliest;
                            probe.latest = latest;
                            probe.note = format!(
                                "SQLite 只读枚举 {expected}（bytes 含 .db+-wal+-shm）；{count} 会话"
                            );
                        }
                        SqliteSessionProbe::SchemaMismatch { actual } => {
                            probe.note = format!(
                                "检测到 SQLite 但表结构不认识（期望 {expected}，实到 {actual}）—— 不是 0，是无法枚举"
                            );
                        }
                        SqliteSessionProbe::ReadFailed { error } => {
                            probe.note = format!("检测到 SQLite 但只读枚举失败（{error}）");
                        }
                    }
                }
                probe
            }
            _ => {
                report.missing_roots.push(root.clone());
                HarnessProbe {
                    root: Some(root),
                    confidence,
                    state: ProbeState::Missing,
                    bytes: 0,
                    record_count: None,
                    note: "单文件不存在".to_string(),
                    ..base
                }
            }
        }
    } else if !root.is_dir() {
        report.missing_roots.push(root.clone());
        HarnessProbe {
            root: Some(root),
            confidence,
            state: ProbeState::Missing,
            record_count: None,
            note: "目录不存在".to_string(),
            ..base
        }
    } else {
        let recs = collect_records(
            &root,
            source,
            machine,
            &cell.format,
            cell.session_pattern.as_deref(),
        );
        let count = recs.len() as u64;
        report.records.extend(recs);
        HarnessProbe {
            root: Some(root),
            confidence,
            state: ProbeState::Scanned,
            record_count: Some(count),
            note: if count == 0 {
                "目录在，但没有可枚举的会话文件".to_string()
            } else {
                String::new()
            },
            ..base
        }
    }
}

/// Config overrides for claude-code / codex keep working on top of the
/// registry's default template.
fn root_for_id(id: &str, registry_root: PathBuf, config: &Config) -> PathBuf {
    let override_path = match id {
        "claude-code" => config.claude_projects_dir.as_deref(),
        "codex" => config.codex_sessions_dir.as_deref(),
        _ => None,
    };
    override_path.map(expand_tilde).unwrap_or(registry_root)
}

/// Expand `~`, `$HOME`, `$XDG_*`, `%USERPROFILE%` per the XDG fallback rules,
/// then cut at the first `<placeholder>` / unknown-`$VAR`-shaped token so an
/// un-resolvable template still yields its static root prefix.
///
/// Returns `(root, is_file)`. `is_file` is true when the whole static prefix
/// points at a single file (an extension-bearing basename, e.g. `opencode.db`).
/// Returns `None` for templates that cannot be anchored at all
/// (e.g. `$CWD/...`).
fn static_prefix_root(template: &str) -> Option<(PathBuf, bool)> {
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    let mut seen_placeholder = false;

    while let Some(ch) = rest.chars().next() {
        match ch {
            '<' => {
                // Directory prefix ends here; the tail is per-session detail
                // we cannot resolve statically.
                if out.ends_with('/') {
                    out.pop();
                }
                out = out.trim_end_matches('/').trim_end_matches('\\').to_string();
                seen_placeholder = true;
                break;
            }
            '~' => {
                if rest == "~" {
                    // Template is exactly `~`.
                    return Some((PathBuf::from(out).join(home_dir()), false));
                }
                if let Some(tail) = rest.strip_prefix("~/") {
                    out.push_str(&home_dir().to_string_lossy());
                    out.push('/');
                    rest = tail;
                    continue;
                }
                return None; // `~something` — not a shape we know.
            }
            '$' => {
                let name_len = rest[1..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .map(|i| i + 1)
                    .unwrap_or(rest.len());
                if name_len == rest.len() {
                    return None; // trailing `$` — cannot resolve.
                }
                let name = &rest[1..name_len];
                match name {
                    "HOME" => out.push_str(&home_dir().to_string_lossy()),
                    "XDG_DATA_HOME" => out.push_str(&xdg_data_home().to_string_lossy()),
                    "XDG_CONFIG_HOME" => out.push_str(&xdg_config_home().to_string_lossy()),
                    "XDG_STATE_HOME" => out.push_str(&xdg_state_home().to_string_lossy()),
                    // `$CODEX_HOME` & friends are per-install overrides this tool
                    // does not resolve → the template cannot be anchored.
                    _ => return None,
                }
                rest = &rest[name_len..];
            }
            '%' => {
                // `%VAR%` (Windows style); only `%USERPROFILE%` is known.
                let end = rest[1..].find('%').map(|i| i + 1);
                match end {
                    Some(end) if &rest[1..end] == "USERPROFILE" => {
                        out.push_str(&home_dir().to_string_lossy());
                        rest = &rest[end + 1..];
                    }
                    _ => return None,
                }
            }
            _ => {
                out.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
    }

    if seen_placeholder {
        let trimmed = out.trim_end_matches(['/', '\\']);
        return Some((PathBuf::from(trimmed), false));
    }
    let is_file = looks_like_file_path(&out);
    // Keep the *actual* target path: for single-file stores this is the file
    // (stat'd directly); for directory roots the dir itself.
    Some((PathBuf::from(out.trim_end_matches(['/', '\\'])), is_file))
}

/// `~/Library/App Support/Zed/threads/threads.db` → `threads.db` looks like a
/// file (basename has a dotted extension, dirname does not).
fn looks_like_file_path(s: &str) -> bool {
    let comp = s
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("");
    comp.contains('.') && !comp.starts_with('.')
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        home_dir().join(rest)
    } else if p == "~" {
        home_dir()
    } else {
        PathBuf::from(p)
    }
}

fn home_dir() -> PathBuf {
    crate::config::home_dir()
}

/// Walk a directory tree (iteratively, symlinks not chased) and collect
/// metadata-only records for session files.
fn collect_records(
    root: &Path,
    source: HarnessSource,
    machine: &str,
    format: &str,
    session_pattern: Option<&str>,
) -> Vec<SessionRecord> {
    let mut records = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue, // ignore unreadable subdirs, keep scanning
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue; // skip sockets, FIFOs, and (crucially) symlinks
            }
            if let Some(rec) = build_record(&path, source, machine, format, session_pattern) {
                records.push(rec);
            }
        }
    }
    records
}

/// Return the suffixes declared by a registry `format` cell.
fn format_suffixes(format: &str) -> Vec<String> {
    format
        .split(|c: char| matches!(c, '/' | '+' | ','))
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .map(|kind| format!(".{kind}"))
        .collect()
}

/// Strip the longest suffix declared by the registry from a filename.
///
/// Returns `(stem, compressed)`; no filename extension is hardcoded here.
fn strip_format_suffix(name: &str, format: &str) -> Option<(String, bool)> {
    let mut suffixes = format_suffixes(format);
    suffixes.sort_by_key(|suffix| std::cmp::Reverse(suffix.len()));
    suffixes.into_iter().find_map(|suffix| {
        name.strip_suffix(&suffix).map(|base| {
            let compressed = suffix.ends_with(".zst");
            (base.to_string(), compressed)
        })
    })
}

/// Match the small basename-glob syntax used by the registry (`*` only).
fn matches_session_pattern(name: &str, pattern: Option<&str>) -> bool {
    let Some(pattern) = pattern else {
        return true;
    };
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return name == pattern;
    }
    if !name.starts_with(parts[0]) {
        return false;
    }
    let mut cursor = parts[0].len();
    for part in &parts[1..parts.len() - 1] {
        let Some(offset) = name[cursor..].find(part) else {
            return false;
        };
        cursor += offset + part.len();
    }
    name[cursor..].ends_with(parts.last().copied().unwrap_or_default())
}

/// Detect the harness from a path, by matching the `.claude` / `.codex`
/// directory segments. Component-based, so it works regardless of `/` vs `\`.
fn detect_source(path: &Path) -> Option<HarnessSource> {
    use std::path::Component;
    for comp in path.components() {
        if let Component::Normal(name) = comp {
            let name = name.to_string_lossy();
            if name == ".codex" {
                return Some(HarnessSource::Codex);
            }
            if name == ".claude" {
                return Some(HarnessSource::ClaudeCode);
            }
        }
    }
    None
}

/// Turn one discovered path into a `SessionRecord`, or `None` if the file's
/// name is not a session file at all.
///
/// A file qualifies only when its suffix is declared by the registry's
/// `format` and its basename matches the registry's optional
/// `session_pattern`. A generic JSON cell without a pattern is rejected: this
/// name-only fail-safe keeps settings/state/config JSON out of the session
/// count without reading or guessing from file contents.
fn build_record(
    path: &Path,
    expected: HarnessSource,
    machine: &str,
    format: &str,
    session_pattern: Option<&str>,
) -> Option<SessionRecord> {
    let name = path.file_name()?.to_string_lossy();
    let (stem, compressed) = strip_format_suffix(&name, format)?;
    if format_suffixes(format)
        .iter()
        .any(|suffix| suffix == ".json")
        && session_pattern.is_none()
    {
        return None;
    }
    if !matches_session_pattern(&name, session_pattern) || stem.is_empty() {
        return None;
    }

    let source = detect_source(path).unwrap_or(expected);
    let source_short = source.short();

    let meta = fs::metadata(path).ok()?;
    // Cache both numeric stat results in one call to avoid a second stat.
    let byte_size = meta.len();
    let mtime: SystemTime = meta.modified().ok()?;

    let ident = crate::id::SessionIdentity {
        source_short,
        machine: machine.to_string(),
        native_id: stem,
    };

    Some(SessionRecord {
        id: ident.id(),
        absolute_path: absolutize(path),
        byte_size,
        mtime,
        source,
        compressed,
    })
}

/// Make a path absolute without requiring it to exist (lexically join with
/// cwd). Paths from `read_dir` are already absolute when the root was, but
/// this is cheap insurance for handycrafted config roots.
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialises tests that mutate process env vars — cargo runs tests in
    /// parallel threads and `set_var` is process-global.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn xdg_data_home_unset_falls_back_to_local_share() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::TempDir::new().unwrap();
        env::remove_var("XDG_DATA_HOME");
        assert_eq!(
            xdg_base_with("XDG_DATA_HOME", home.path(), ".local/share"),
            home.path().join(".local/share")
        );
    }

    #[test]
    fn xdg_data_home_set_wins_over_fallback() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::TempDir::new().unwrap();
        let data = tempfile::TempDir::new().unwrap();
        env::set_var("XDG_DATA_HOME", data.path());
        assert_eq!(
            xdg_base_with("XDG_DATA_HOME", home.path(), ".local/share"),
            data.path().to_path_buf()
        );
    }

    #[test]
    fn xdg_config_and_state_fallbacks() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::TempDir::new().unwrap();
        env::remove_var("XDG_CONFIG_HOME");
        env::remove_var("XDG_STATE_HOME");
        assert_eq!(
            xdg_base_with("XDG_CONFIG_HOME", home.path(), ".config"),
            home.path().join(".config")
        );
        assert_eq!(
            xdg_base_with("XDG_STATE_HOME", home.path(), ".local/state"),
            home.path().join(".local/state")
        );
    }

    #[test]
    fn template_expansion_handles_tilde_home_and_placeholder() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        env::set_var("HOME", dir.path());
        let home_abs = dir.path().to_path_buf();

        let (root, is_file) =
            static_prefix_root("~/Library/Application Support/Zed/threads/threads.db").unwrap();
        assert_eq!(
            root,
            home_abs.join("Library/Application Support/Zed/threads/threads.db")
        );
        assert!(is_file);

        let (root, is_file) =
            static_prefix_root("~/.claude/projects/<sanitized-cwd>/<uuid>.jsonl").unwrap();
        assert_eq!(root, home_abs.join(".claude").join("projects"));
        assert!(!is_file);

        let (root, is_file) = static_prefix_root("~/.local/share/opencode/opencode.db").unwrap();
        assert_eq!(root, home_abs.join(".local/share/opencode/opencode.db"));
        assert!(is_file);

        let (root, is_file) = static_prefix_root("~/.copilot/").unwrap();
        assert_eq!(root, home_abs.join(".copilot"));
        assert!(!is_file);

        let _ = dir;
    }

    #[test]
    fn template_expansion_uses_xdg_data_home_when_set() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::TempDir::new().unwrap();
        let data = tempfile::TempDir::new().unwrap();
        env::set_var("HOME", home.path());
        env::set_var("XDG_DATA_HOME", data.path());

        let expanded = static_prefix_root("$XDG_DATA_HOME/zed/threads/threads.db").unwrap();
        assert_eq!(
            expanded.0,
            data.path().join("zed").join("threads/threads.db")
        );
        assert!(expanded.1);

        env::remove_var("XDG_DATA_HOME");
        let (root, _) = static_prefix_root("$XDG_DATA_HOME/zed/threads/threads.db").unwrap();
        assert_eq!(
            root,
            home.path().join(".local/share/zed/threads/threads.db")
        );
    }

    #[test]
    fn template_with_unknown_var_is_unresolvable() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        env::set_var("HOME", dir.path());
        assert!(static_prefix_root("$CODEX_HOME/sessions/").is_none());
        assert!(static_prefix_root("$CWD/.aider.chat.history.md").is_none());
        let _ = dir;
    }

    #[test]
    fn confidence_classification_and_skip_rules() {
        assert_eq!(Confidence::classify("源码确认"), Confidence::Confirmed);
        assert_eq!(Confidence::classify("官方文档"), Confidence::OfficialDocs);
        assert_eq!(
            Confidence::classify("仅社区说法未核实"),
            Confidence::CommunityClaim
        );
        assert_eq!(Confidence::classify("本机实测"), Confidence::Measured);
        assert_eq!(Confidence::classify("未查明"), Confidence::Unascertained);
        assert_eq!(
            Confidence::classify("某未知措辞"),
            Confidence::Unascertained
        );

        assert!(!Confidence::Unascertained.scan_allowed());
        assert!(Confidence::Confirmed.scan_allowed());
        assert!(Confidence::CommunityClaim.scan_allowed());
        assert!(Confidence::Measured.scan_allowed());
        assert!(Confidence::CommunityClaim.is_low_confidence());
        assert!(!Confidence::Measured.is_low_confidence());
        assert!(!Confidence::Confirmed.is_low_confidence());
        assert_eq!(Confidence::Measured.label(), CONF_MEASURED);
    }

    #[test]
    fn unascertained_cell_is_skipped_not_scanned() {
        let home = tempfile::TempDir::new().unwrap();
        let registry: HarnessRegistry = serde_json::from_str(&format!(
            r#"{{
              "schema_version": 1,
              "generated": "2026-08-16",
              "harnesses": [
                {{ "id": "claude-code", "display_name": "Claude Code",
                   "paths": {{ "macos": {{ "template": "~/whatever/", "format": "jsonl",
                   "confidence": "{CONF_UNASCERTAINED}", "source": "x" }} }} }}
              ]
            }}"#
        ))
        .unwrap();
        let config = Config {
            claude_projects_dir: Some(home.path().join("whatever").to_string_lossy().into()),
            ..Default::default()
        };
        let report = scan_with_registry(&config, &registry).unwrap();
        assert!(report.records.is_empty());
        assert_eq!(report.probes.len(), 1);
        assert_eq!(report.probes[0].state, ProbeState::SkipUnascertained);
        assert!(!report.probes[0].installed_p());
        let _ = home;
    }

    #[test]
    fn registry_missing_file_errors_loudly() {
        let p = tempfile::TempDir::new()
            .unwrap()
            .path()
            .join("does-not-exist.json");
        let err = load_registry(&p).unwrap_err();
        assert!(
            err.to_string().contains("harness-registry") || err.kind() == io::ErrorKind::NotFound
        );
    }

    #[test]
    fn suffix_stripping_matches_longest_registry_suffix() {
        assert_eq!(
            strip_format_suffix(
                "019bf00d-97b6-7eb2-9bf8-eacbacc09765.jsonl",
                "jsonl / jsonl.zst",
            ),
            Some(("019bf00d-97b6-7eb2-9bf8-eacbacc09765".to_string(), false))
        );
        assert_eq!(
            strip_format_suffix(
                "019bf00d-97b6-7eb2-9bf8-eacbacc09765.jsonl.zst",
                "jsonl / jsonl.zst",
            ),
            Some(("019bf00d-97b6-7eb2-9bf8-eacbacc09765".to_string(), true))
        );
        assert_eq!(strip_format_suffix("notes.txt", "jsonl"), None);
    }

    #[test]
    fn session_pattern_separates_json_from_config_names() {
        assert!(matches_session_pattern(
            "session-2026-01-id.json",
            Some("session-*")
        ));
        assert!(!matches_session_pattern("settings.json", Some("session-*")));
        assert!(!matches_session_pattern("state.json", Some("session-*")));
        assert!(!matches_session_pattern("config.json", Some("session-*")));
    }

    #[test]
    fn source_detection_via_directory_segment() {
        let codex = Path::new("/Users/u/.codex/sessions/2026-05-01/ab.jsonl");
        let claude = Path::new("/Users/u/.claude/projects/foo-abc/ab.jsonl");
        assert_eq!(detect_source(&codex), Some(HarnessSource::Codex));
        assert_eq!(detect_source(&claude), Some(HarnessSource::ClaudeCode));
    }

    #[test]
    fn id_uses_dot_separator() {
        let ident = crate::id::SessionIdentity {
            source_short: HarnessSource::Codex.short(),
            machine: "mbp".into(),
            native_id: "019bf00d-97b6-7eb2-9bf8-eacbacc09765".into(),
        };
        assert_eq!(ident.id(), "codex.mbp.019bf00d-97b6-7eb2-9bf8-eacbacc09765");
    }
}
