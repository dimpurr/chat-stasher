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
//! Registry source precedence is explicit runtime override, repository file,
//! then the exact registry JSON embedded at compile time. An explicit
//! override that is missing or invalid **fails loudly** — silently using the
//! embedded copy would make a user believe their edited registry was active.
//!
//! Hard guarantees:
//!   * read-only — nothing is opened for writing and file bodies are never
//!     read, so no session content can ever leak into a log or report,
//!   * `.jsonl.zst` files are *recognised and listed* (compressed flag set);
//!     decompression is a later spike but silently skipping them is not an
//!     option,
//!   * missing roots are reported, not fatal.

use crate::config::Config;
use crate::models::{HarnessSource, SessionRecord, SqliteSessionLayout};
use crate::sqlite_probe::{
    enumerate_cursor_legacy_sessions, enumerate_opencode_sessions, enumerate_sqlite_sessions,
    probe_sqlite_store, sqlite_millis_to_system_time, CursorLegacySessionRow, SqliteSessionProbe,
    SqliteSessionRow,
};
use serde::Deserialize;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Registry file, relative to the repo root, that drives the scan.
pub const REGISTRY_REL_PATH: &str = "data/harness-registry-v1.json";

/// Optional runtime registry override. This is intentionally checked before
/// both the repository copy and the embedded fallback.
pub const REGISTRY_ENV: &str = "CHAT_STASHER_REGISTRY";

/// The shipped registry is the data file itself, not a second hand-written
/// Rust representation. The repository path is resolved from the crate
/// manifest at compile time, so an installed binary can scan outside the repo.
const EMBEDDED_REGISTRY: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/harness-registry-v1.json"
));

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

/// `%APPDATA%` (Windows roaming app data), falling back to the documented
/// default `%USERPROFILE%\AppData\Roaming`.
///
/// Same shape as the XDG helpers on purpose: this is Windows' equivalent of a
/// base directory, and it is what Electron apps (Cursor, and the next one)
/// use — resolving it here rather than in one template keeps the next harness
/// from hitting the same wall.
pub fn appdata_dir() -> PathBuf {
    xdg_base_with("APPDATA", &crate::config::home_dir(), "AppData/Roaming")
}

/// `%LOCALAPPDATA%` (Windows local app data), falling back to
/// `%USERPROFILE%\AppData\Local`.
pub fn local_appdata_dir() -> PathBuf {
    xdg_base_with("LOCALAPPDATA", &crate::config::home_dir(), "AppData/Local")
}

/// Where a `dirs`-based tool keeps its per-user cache, spelled the way
/// `platform` spells it. Ordered most-specific first.
///
/// The three spellings are `dirs`' own (dirs-6.0.0 `src/mac.rs:9`,
/// `src/lin.rs:8`, `src/win.rs:10`). The Windows one is the reason this
/// function exists: there the cache is `%LOCALAPPDATA%`, which is **not** a
/// child of `$HOME`, so a `$HOME`-derived guess does not merely land in the
/// wrong place — it lands nowhere, and a caller that clears "the cache" clears
/// nothing while reporting success.
///
/// More than one candidate is returned so that a platform whose spelling cannot
/// be exercised from here still gets a second chance: callers match on content,
/// and a directory that does not exist costs nothing to skip.
///
/// Pure on purpose: every input is an argument, so the Windows spelling can be
/// asserted from a Mac. Same reason `static_prefix_root` is pure.
pub fn user_cache_dirs_on(
    platform: &str,
    home: &Path,
    xdg_cache_home: Option<&Path>,
    local_app_data: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out = match platform {
        // `dirs::cache_dir()` = `known_folder_local_app_data()`, falling back to
        // the documented `%USERPROFILE%\AppData\Local` — same shape as
        // `local_appdata_dir`.
        "windows" => vec![local_app_data
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join("AppData/Local"))],
        "macos" => vec![home.join("Library/Caches")],
        // `$XDG_CACHE_HOME` first: a Linux box that sets it breaks a
        // `~/.cache` guess in exactly the way Windows does.
        _ => vec![xdg_cache_home
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".cache"))],
    };
    // Second chance for the one spelling that cannot be exercised from the
    // other platforms: `dirs` reads the Windows path from the Known Folder API
    // rather than from `%LOCALAPPDATA%`, and the two are only *documented* to
    // agree.
    let documented_default = home.join("AppData/Local");
    if platform == "windows" && !out.contains(&documented_default) {
        out.push(documented_default);
    }
    out
}

/// [`user_cache_dirs_on`] for the running platform and environment.
pub fn user_cache_dirs() -> Vec<PathBuf> {
    let home = crate::config::home_dir();
    let xdg = env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let local = env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    user_cache_dirs_on(current_platform(), &home, xdg.as_deref(), local.as_deref())
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

    /// Any declared cell, in a fixed order.
    ///
    /// Only ever used to read the **platform-independent** half of a cell —
    /// `format` and the `sql_*` schema — when the user supplied the path
    /// themselves and this platform has no cell to supply it. A store's schema
    /// does not change across operating systems; its location does.
    pub fn any_cell(&self) -> Option<&RegistryCell> {
        self.macos
            .as_ref()
            .or(self.linux.as_ref())
            .or(self.windows.as_ref())
    }
}

/// One harness × platform cell.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryCell {
    pub template: String,
    /// Optional environment variable whose value is the harness-specific base
    /// directory. When set, scanner resolution uses it before the template.
    #[serde(default)]
    pub env_override: Option<String>,
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
    /// Column that identifies a session row (`key` for Cursor, `session_id`
    /// for Grok).
    #[serde(default)]
    pub sql_id_column: Option<String>,
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
    /// Optional named qualification rule for JSON key/value stores.
    #[serde(default)]
    pub sql_qualification: Option<String>,
}

/// Deserialize the registry from `path`. Returns a descriptive error on any
/// failure (missing file, bad JSON, wrong shape).
pub fn load_registry(path: &Path) -> io::Result<HarnessRegistry> {
    let raw = fs::read_to_string(path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("harness registry {} could not be read: {e}", path.display()),
        )
    })?;
    let source = path.display().to_string();
    parse_registry(&raw, &source)
}

fn parse_registry(raw: &str, source: &str) -> io::Result<HarnessRegistry> {
    serde_json::from_str(raw).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("harness registry {source} is not valid JSON: {e}"),
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
    /// Candidate rows before a store-specific qualification rule. For Cursor
    /// this is the raw `composerData:%` count shown beside `record_count`.
    pub candidate_count: Option<u64>,
    /// Sessions this harness *knows about* but that never became a
    /// `SessionRecord`: undecodable rows, metadata-only index entries whose
    /// body lives somewhere unreachable, and rows the probe counted but
    /// enumeration failed to return. `None` when nothing was enumerated at
    /// all (the state already says so); `Some(0)` means "known == handed out".
    ///
    /// This is deliberately *not* folded into `candidate_count - record_count`:
    /// that difference is the filter ("we looked, it really has nothing"),
    /// this field is the failure ("it says it has something, we could not get
    /// at it"). Merging them is the lie B68 removes.
    pub unreadable_count: Option<u64>,
    /// Directory entries whose type or contents could not be inspected. This
    /// is separate from `unreadable_count`: an inaccessible directory may
    /// contain zero, one, or many session files, so its session cardinality is
    /// unknown rather than one.
    pub unreadable_entry_count: Option<u64>,
    /// Earliest / latest session timestamp, for single-file SQLite stores that
    /// carry one (via the registry-declared schema). `None` when unknown.
    pub earliest: Option<SystemTime>,
    pub latest: Option<SystemTime>,
    /// For single-file stores: the store's bytes on disk (`.db` + sidecars for
    /// SQLite). 0 otherwise.
    pub bytes: u64,
    /// Metadata-only set of files recognised for this harness. The doctor uses
    /// the same set when it builds its footprint row; it is never printed.
    pub recognized_files: Vec<PathBuf>,
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

/// A harness probe recognised sessions, but the scanner produced fewer
/// `SessionRecord`s for it. `collect` consumes only those records, so the
/// difference is currently not archivable by this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveGap {
    pub harness_id: String,
    pub display_name: String,
    pub recognized_sessions: Option<u64>,
    pub session_records: usize,
}

impl ScanReport {
    /// Return installed harnesses whose recognised session count is greater
    /// than the number of records that `collect` can consume. The comparison
    /// is deliberately made against the records actually produced by this
    /// scan; no harness allowlist is involved.
    pub fn archive_gaps(&self) -> Vec<ArchiveGap> {
        self.probes
            .iter()
            .filter_map(|probe| {
                if !probe.installed_p() {
                    return None;
                }
                let Some(source) = HarnessSource::from_id(&probe.id) else {
                    return None;
                };
                let session_records = self
                    .records
                    .iter()
                    .filter(|record| record.source == source)
                    .count();
                match probe.record_count {
                    Some(0) => None,
                    Some(recognized_sessions) if session_records as u64 >= recognized_sessions => {
                        None
                    }
                    Some(recognized_sessions) => Some(ArchiveGap {
                        harness_id: probe.id.clone(),
                        display_name: probe.display_name.clone(),
                        recognized_sessions: Some(recognized_sessions),
                        session_records,
                    }),
                    // An existing single-file store whose schema/read failed
                    // is still not consumable by `collect`; keep its count
                    // honest rather than inventing a zero.
                    None if matches!(probe.state, ProbeState::FileTarget)
                        && session_records == 0 =>
                    {
                        Some(ArchiveGap {
                            harness_id: probe.id.clone(),
                            display_name: probe.display_name.clone(),
                            recognized_sessions: None,
                            session_records,
                        })
                    }
                    None => None,
                }
            })
            .collect()
    }
}

/// Render the same metadata-only warning for every CLI surface that reports
/// scanner results. The counts come from the `ArchiveGap`, not from a harness
/// name list.
pub fn format_archive_gap(gap: &ArchiveGap) -> String {
    format!(
        "  ! harness={} identified_sessions={} session_records={} source_not_collected=true — 本版本还不支持归档这个 harness 的存储格式（不是你的配置问题，也没有可以设置的开关）",
        gap.harness_id,
        gap.recognized_sessions
            .map(|count| count.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        gap.session_records
    )
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

/// Walk every registry harness that applies to this platform and collect
/// metadata-only records.
///
/// The registry uses the precedence documented by [`load_registry_from_repo`].
/// An explicit override that is missing or unparseable **errors** — no silent
/// fallback. Any harness whose cell is confidence `未查明` is skipped;
/// `仅社区说法未核实` cells are scanned but the probe is flagged
/// low-confidence.
pub fn scan(config: &Config) -> io::Result<ScanReport> {
    let registry = load_registry_from_repo()?;
    scan_with_registry(config, &registry)
}

/// Resolve + parse the registry. An explicit `CHAT_STASHER_REGISTRY` path is
/// authoritative and any read/parse error is returned without fallback. When
/// it is unset, the repository copy remains the development-time override;
/// outside a checkout the exact compile-time embedded data file is used.
/// Shared by the scanner and the sealing allowlist (`crate::seal`) so both
/// read the exact same registry source.
pub fn load_registry_from_repo() -> io::Result<HarnessRegistry> {
    if let Some(path) = env::var_os(REGISTRY_ENV) {
        return load_registry(Path::new(&path));
    }
    let Some(path) = resolve_registry_path() else {
        let source = format!("embedded {REGISTRY_REL_PATH}");
        return parse_registry(EMBEDDED_REGISTRY, &source);
    };
    load_registry(&path)
}

/// Registry-driven scan against an already-loaded [`HarnessRegistry`];
/// separate entry so tests can feed it a scratch registry without touching
/// the user's HOME or the repo's `data/` file.
pub fn scan_with_registry(config: &Config, registry: &HarnessRegistry) -> io::Result<ScanReport> {
    let machine = crate::id::machine_id().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Other,
            "machine identity unavailable; supply --machine <name>",
        )
    })?;
    scan_with_registry_and_machine(config, registry, &machine)
}

/// Registry-driven scan with a caller-selected machine partition. Archive
/// commands use this after resolving their explicit `--machine` or the local
/// hostname, so a missing local hostname cannot block the explicit route.
pub fn scan_with_machine(config: &Config, machine: &str) -> io::Result<ScanReport> {
    let registry = load_registry_from_repo()?;
    scan_with_registry_and_machine(config, &registry, machine)
}

fn scan_with_registry_and_machine(
    config: &Config,
    registry: &HarnessRegistry,
    machine: &str,
) -> io::Result<ScanReport> {
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
        candidate_count: None,
        unreadable_count: None,
        unreadable_entry_count: None,
        earliest: None,
        latest: None,
        bytes: 0,
        recognized_files: Vec::new(),
        note: String::new(),
    };

    // 0. A configured root is the *user stating* where this harness lives. The
    // registry's template and its confidence gate both exist to keep us from
    // walking a path **we** guessed; neither has anything to say about a path
    // the user wrote down. So this is resolved first and outranks both.
    let explicit_root = config.explicit_harness_root(&h.id).map(expand_tilde);

    // 1. Current-platform cell must exist — unless the user supplied the path,
    // in which case any cell still describes the platform-independent half
    // (format + SQL schema) and only the location was missing.
    let cell = match h.paths.cell_for(platform) {
        Some(cell) => cell,
        None => match explicit_root.as_ref().and_then(|_| h.paths.any_cell()) {
            Some(cell) => cell,
            None => {
                return HarnessProbe {
                    note: format!("无 {platform} 平台条目"),
                    ..base
                }
            }
        },
    };

    // 2. Confidence decides whether we scan a *guessed* path at all. A
    // configured path is not a guess, so `未查明` does not gate it: refusing to
    // count a store the user pointed us at, opened, and enumerated would be its
    // own false claim ("I could not determine"), just in the other direction.
    let confidence = Confidence::classify(&cell.confidence);
    if !confidence.scan_allowed() && explicit_root.is_none() {
        return HarnessProbe {
            confidence,
            state: ProbeState::SkipUnascertained,
            note: format!("confidence=未查明，跳过不去扫（扫猜的路径 = 假阴/假阳）"),
            ..base
        };
    }

    // 3. Resolve the registry-declared environment override first — an
    // explicitly exported variable is at least as explicit as the config file,
    // and this order predates the config table. Then the configured root, then
    // the template.
    let (root, is_file, used_env_override, used_config_root) = match root_from_env_override(cell) {
        Some((root, is_file)) => (root, is_file, true, false),
        None => match explicit_root {
            Some(root) => {
                let is_file = explicit_root_is_file(&root);
                (root, is_file, false, true)
            }
            None => match static_prefix_root(&cell.template) {
                Some((root, is_file)) => (root, is_file, false, false),
                None => {
                    return HarnessProbe {
                        confidence,
                        state: ProbeState::SkipUnresolvable,
                        note: format!("模板无法静态解析为根路径：{}", cell.template),
                        ..base
                    }
                }
            },
        },
    };

    // 5. This build must know the harness, or we refuse to label its files.
    let Some(source) = HarnessSource::from_id(&h.id) else {
        return HarnessProbe {
            confidence,
            state: ProbeState::SkipUnresolvable,
            note: format!("registry id 在此 build 中未知：{}", h.id),
            ..base
        };
    };

    let env_note = if used_config_root {
        format!("config harness_roots.{}; ", h.id)
    } else {
        cell.env_override
            .as_deref()
            .filter(|_| used_env_override)
            .map(|name| format!("env_override=${name}; "))
            .unwrap_or_default()
    };

    let schema = schema_cell(h, cell);

    // Cursor's old layout is a directory of workspace databases, not the
    // global single-file target declared by the current layout.
    if h.id == "cursor" && cell.format == "sqlite" {
        return probe_cursor_harness(base, confidence, root, env_note, schema, machine, report);
    }

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
                    candidate_count: None,
                    unreadable_count: None,
                    note: "单文件存储，只探测存在与大小".to_string(),
                    recognized_files: vec![root.clone()],
                    ..base
                };
                if cell.format == "sqlite" {
                    // The registry-driven scan and the doctor footprint table
                    // share ONE SQLite probe (`crate::sqlite_probe`), so the
                    // same harness can never show two different counts again.
                    // The schema spec also comes from the registry cell, so
                    // the scan and the doctor enumerate with the same query.
                    let spec = crate::sqlite_probe::spec_from_cell(schema);
                    let info = match &spec {
                        Some(spec) => crate::sqlite_probe::probe_sqlite_store_with(&root, spec),
                        None => probe_sqlite_store(&root),
                    };
                    probe.bytes = info.total_bytes;
                    let expected = spec
                        .as_ref()
                        .map(|s| format!("{}表({})", s.table, s.required_columns.join(", ")))
                        .unwrap_or_else(|| "session(id, time_created, time_updated)".to_string());
                    match info.sessions {
                        SqliteSessionProbe::Known {
                            count,
                            candidate_count,
                            earliest,
                            latest,
                        } => {
                            probe.record_count = Some(count);
                            probe.candidate_count = Some(candidate_count);
                            probe.earliest = earliest;
                            probe.latest = latest;
                            probe.note = format!(
                                "{env_note}SQLite 只读枚举 {expected}（bytes 含 .db+-wal+-shm）；过滤前 {candidate_count} / 过滤后 {count} 会话"
                            );
                            if h.id == "opencode" {
                                match enumerate_opencode_sessions(&root) {
                                    Ok(rows) => {
                                        let records: Vec<SessionRecord> = rows
                                            .into_iter()
                                            .map(|row| SessionRecord {
                                                id: crate::id::SessionIdentity {
                                                    source_short: source.short(),
                                                    machine: machine.to_string(),
                                                    native_id: row.id,
                                                }
                                                .id(),
                                                absolute_path: root.clone(),
                                                byte_size: 0,
                                                mtime: sqlite_millis_to_system_time(
                                                    row.time_updated,
                                                )
                                                .unwrap_or(UNIX_EPOCH),
                                                source,
                                                compressed: false,
                                                sqlite_layout: Some(SqliteSessionLayout::OpenCode),
                                            })
                                            .collect();
                                        let record_count = records.len();
                                        report.records.extend(records);
                                        probe.unreadable_count = Some(enumeration_gap(
                                            count,
                                            record_count,
                                            info.unreadable_count,
                                        ));
                                        probe
                                            .note
                                            .push_str(&format!("; SessionRecord={record_count}"));
                                    }
                                    Err(error) => {
                                        // A failed enumeration is not zero
                                        // sessions: every known row is now a
                                        // session we cannot hand out.
                                        probe.unreadable_count =
                                            Some(enumeration_gap(count, 0, info.unreadable_count));
                                        probe.note.push_str(&format!(
                                            "; SessionRecord 枚举失败（{error}）"
                                        ));
                                    }
                                }
                            } else if h.id == "grok" {
                                if let Some(spec) = spec.as_ref() {
                                    match enumerate_sqlite_sessions(&root, spec) {
                                        Ok(rows) => {
                                            let records = sqlite_records_from_rows(
                                                rows,
                                                &root,
                                                source,
                                                machine,
                                                SqliteSessionLayout::Grok,
                                            );
                                            let record_count = records.len();
                                            report.records.extend(records);
                                            probe.unreadable_count = Some(enumeration_gap(
                                                count,
                                                record_count,
                                                info.unreadable_count,
                                            ));
                                            probe.note.push_str(&format!(
                                                "; SessionRecord={record_count}"
                                            ));
                                        }
                                        Err(error) => {
                                            probe.unreadable_count = Some(enumeration_gap(
                                                count,
                                                0,
                                                info.unreadable_count,
                                            ));
                                            probe.note.push_str(&format!(
                                                "; SessionRecord 枚举失败（{error}）"
                                            ));
                                        }
                                    }
                                }
                            }
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
            candidate_count: None,
            unreadable_count: None,
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
        let count = recs.records.len() as u64;
        let recognized_files = recs
            .records
            .iter()
            .map(|record| record.absolute_path.clone())
            .collect();
        report.records.extend(recs.records);
        let note = if recs.unreadable_entry_count > 0 {
            format!(
                "目录扫描不完整：{} 个不可读目录项（会话数未知）",
                recs.unreadable_entry_count
            )
        } else if recs.unreadable_count > 0 {
            format!(
                "{} 条读不出来（不是空，是取不到元数据）",
                recs.unreadable_count
            )
        } else if count == 0 {
            "目录在，但没有可枚举的会话文件".to_string()
        } else {
            String::new()
        };
        HarnessProbe {
            root: Some(root),
            confidence,
            state: ProbeState::Scanned,
            record_count: Some(count),
            candidate_count: Some(count + recs.unreadable_count),
            unreadable_count: Some(recs.unreadable_count),
            unreadable_entry_count: Some(recs.unreadable_entry_count),
            recognized_files,
            note,
            ..base
        }
    }
}

/// Resolve an env override as the base directory represented by the registry
/// template. The current cells use stable harness markers: Cursor's override
/// is its `User` directory, Codex's is its `.codex` directory, Gemini's is its
/// `.gemini` directory, and opencode's `OPENCODE_DB` is a database filename
/// (absolute values stay absolute; relative values live below XDG data).
fn root_from_env_override(cell: &RegistryCell) -> Option<(PathBuf, bool)> {
    let variable = cell.env_override.as_deref()?;
    let value = env::var_os(variable).filter(|value| !value.is_empty())?;
    if variable == "OPENCODE_DB" {
        let value = PathBuf::from(value);
        if value == Path::new(":memory:") {
            return Some((value, true));
        }
        return Some((
            if value.is_absolute() {
                value
            } else {
                xdg_data_home().join(value)
            },
            true,
        ));
    }
    let template = cell.template.replace('\\', "/");
    let marker = if template.contains("Cursor/User/") {
        "Cursor/User/"
    } else if template.contains(".codex/") {
        ".codex/"
    } else if template.contains(".gemini/") {
        ".gemini/"
    } else {
        return None;
    };
    let suffix = template.split_once(marker)?.1;
    let static_suffix = suffix.split('<').next()?.trim_end_matches('/');
    if static_suffix.is_empty() {
        return Some((PathBuf::from(value), false));
    }
    let root = PathBuf::from(value).join(static_suffix);
    let is_file = looks_like_file_path(static_suffix);
    Some((root, is_file))
}

fn sqlite_records_from_rows(
    rows: Vec<SqliteSessionRow>,
    db: &Path,
    source: HarnessSource,
    machine: &str,
    layout: SqliteSessionLayout,
) -> Vec<SessionRecord> {
    rows.into_iter()
        .map(|row| SessionRecord {
            id: crate::id::SessionIdentity {
                source_short: source.short(),
                machine: machine.to_string(),
                native_id: row.id,
            }
            .id(),
            absolute_path: db.to_path_buf(),
            byte_size: 0,
            mtime: row.mtime.unwrap_or(UNIX_EPOCH),
            source,
            compressed: false,
            sqlite_layout: Some(layout),
        })
        .collect()
}

/// Sessions a probe knew about but never handed out as `SessionRecord`s.
///
/// `known` is the probe's post-filter count, `emitted` the records actually
/// produced, `unreadable_rows` the candidates the probe itself already
/// classified as undecodable. The sum is the answer to "已知 N 条，取回 M 条,
/// K 条读不出来" — it never includes rows the qualification filter rejected
/// after reading them, because those are not failures.
fn enumeration_gap(known: u64, emitted: usize, unreadable_rows: u64) -> u64 {
    unreadable_rows + known.saturating_sub(emitted as u64)
}

/// The one-clause note appended when — and only when — something was in fact
/// unreadable. An all-good scan appends nothing, so the "一切正常" output and
/// its exit code are byte-for-byte what they were.
fn unreadable_note(
    unreadable: u64,
    unreadable_stores: u64,
    enumerate_error: Option<&str>,
) -> String {
    let mut note = String::new();
    if unreadable > 0 {
        note.push_str(&format!(
            "；{unreadable} 条读不出来（不是空，是取不到正文）"
        ));
    }
    if unreadable_stores > 0 {
        note.push_str(&format!("；{unreadable_stores} 个库打不开/读不懂"));
    }
    if let Some(error) = enumerate_error {
        note.push_str(&format!("；枚举失败（{error}）"));
    }
    note
}

fn cursor_legacy_records_from_rows(
    rows: Vec<CursorLegacySessionRow>,
    source: HarnessSource,
    machine: &str,
) -> Vec<SessionRecord> {
    rows.into_iter()
        .map(|row| SessionRecord {
            id: crate::id::SessionIdentity {
                source_short: source.short(),
                machine: machine.to_string(),
                native_id: row.id,
            }
            .id(),
            absolute_path: row.db,
            byte_size: 0,
            mtime: row.mtime.unwrap_or(UNIX_EPOCH),
            source,
            compressed: false,
            sqlite_layout: Some(SqliteSessionLayout::CursorLegacy),
        })
        .collect()
}

/// Probe Cursor's current global store and, when it cannot yield a qualified
/// composer count, its legacy workspaceStorage stores. This keeps the fallback
/// source visible in doctor output while preserving one Cursor footprint row.
fn probe_cursor_harness(
    base: HarnessProbe,
    confidence: Confidence,
    global_db: PathBuf,
    env_note: String,
    cell: &RegistryCell,
    machine: &str,
    report: &mut ScanReport,
) -> HarnessProbe {
    let Some(user_dir) = cursor_user_dir_from_global_db(&global_db) else {
        return HarnessProbe {
            confidence,
            state: ProbeState::SkipUnresolvable,
            note: format!("{env_note}Cursor globalStorage 路径无法推导 User 根目录"),
            ..base
        };
    };
    let workspace_storage = user_dir.join("workspaceStorage");

    let global_info = if global_db.is_file() {
        let spec = crate::sqlite_probe::spec_from_cell(cell);
        Some(match spec {
            Some(spec) => crate::sqlite_probe::probe_sqlite_store_with(&global_db, &spec),
            None => probe_sqlite_store(&global_db),
        })
    } else {
        None
    };

    let global_needs_fallback = match global_info.as_ref().map(|info| &info.sessions) {
        None => true,
        Some(SqliteSessionProbe::Known { count, .. }) => *count == 0,
        Some(SqliteSessionProbe::SchemaMismatch { .. })
        | Some(SqliteSessionProbe::ReadFailed { .. }) => true,
    };
    let global_summary = match global_info.as_ref().map(|info| &info.sessions) {
        Some(SqliteSessionProbe::Known {
            candidate_count,
            count,
            ..
        }) => format!("global cursorDiskKV 过滤前 {candidate_count} / 过滤后 {count}；"),
        Some(SqliteSessionProbe::SchemaMismatch { .. }) => {
            "global cursorDiskKV 表结构不认识；".to_string()
        }
        Some(SqliteSessionProbe::ReadFailed { .. }) => {
            "global cursorDiskKV 只读枚举失败；".to_string()
        }
        None => "global cursorDiskKV 不存在；".to_string(),
    };
    if global_needs_fallback {
        if let Some(legacy) =
            crate::sqlite_probe::probe_cursor_legacy_workspace_storage(&workspace_storage)
        {
            if let SqliteSessionProbe::Known {
                candidate_count,
                count,
                earliest,
                latest,
            } = legacy.sessions
            {
                // B68: `.ok().unwrap_or_default()` used to turn a failed
                // enumeration into an empty Vec while `record_count` kept the
                // probe's count — "known 414, handed out 0" with nothing said.
                // The failure now travels into both the note and the counted
                // gap below.
                let (records, enumerate_error) =
                    match enumerate_cursor_legacy_sessions(&workspace_storage) {
                        Ok(rows) => (
                            cursor_legacy_records_from_rows(rows, HarnessSource::Cursor, machine),
                            None,
                        ),
                        Err(error) => (Vec::new(), Some(error)),
                    };
                let record_count = records.len();
                let unreadable = enumeration_gap(count, record_count, legacy.unreadable_count);
                let gap_note = unreadable_note(
                    unreadable,
                    legacy.unreadable_stores,
                    enumerate_error.as_deref(),
                );
                let recognized_files = records
                    .iter()
                    .map(|record| record.absolute_path.clone())
                    .collect();
                report.records.extend(records);
                return HarnessProbe {
                    root: Some(workspace_storage),
                    confidence,
                    state: ProbeState::FileTarget,
                    record_count: Some(count),
                    candidate_count: Some(candidate_count),
                    unreadable_count: Some(unreadable),
                    earliest,
                    latest,
                    bytes: legacy.total_bytes,
                    recognized_files,
                    note: format!(
                        "{env_note}来自 legacy workspaceStorage；{global_summary}SQLite 只读枚举 ItemTable；过滤前 {candidate_count} / 过滤后 {count} 会话；SessionRecord={record_count}{gap_note}"
                    ),
                    ..base
                };
            }
        }
    }

    let Some(info) = global_info else {
        report.missing_roots.push(global_db.clone());
        return HarnessProbe {
            root: Some(global_db),
            confidence,
            state: ProbeState::Missing,
            note: format!(
                "{env_note}globalStorage/state.vscdb 不存在，legacy workspaceStorage 也未找到可读 composer 数据"
            ),
            ..base
        };
    };

    match info.sessions {
        SqliteSessionProbe::Known {
            candidate_count,
            count,
            earliest,
            latest,
        } => {
            // B68: the `.ok()` here silently produced an empty record set
            // while `record_count` still reported the probe's count.
            let (records, enumerate_error) = match crate::sqlite_probe::spec_from_cell(cell) {
                Some(spec) => match enumerate_sqlite_sessions(&global_db, &spec) {
                    Ok(rows) => (
                        sqlite_records_from_rows(
                            rows,
                            &global_db,
                            HarnessSource::Cursor,
                            machine,
                            SqliteSessionLayout::CursorGlobal,
                        ),
                        None,
                    ),
                    Err(error) => (Vec::new(), Some(error)),
                },
                None => (
                    Vec::new(),
                    Some("registry cell 未声明 SQLite schema".to_string()),
                ),
            };
            let record_count = records.len();
            let unreadable = enumeration_gap(count, record_count, info.unreadable_count);
            let gap_note = unreadable_note(
                unreadable,
                info.unreadable_stores,
                enumerate_error.as_deref(),
            );
            let recognized_files = records
                .iter()
                .map(|record| record.absolute_path.clone())
                .collect();
            report.records.extend(records);
            HarnessProbe {
                root: Some(global_db),
                confidence,
                state: ProbeState::FileTarget,
                record_count: Some(count),
                candidate_count: Some(candidate_count),
                unreadable_count: Some(unreadable),
                earliest,
                latest,
                bytes: info.total_bytes,
                recognized_files,
                note: format!(
                    "{env_note}SQLite 只读枚举 cursorDiskKV表(key, value)；过滤前 {candidate_count} / 过滤后 {count} 会话；SessionRecord={record_count}{gap_note}"
                ),
                ..base
            }
        }
        SqliteSessionProbe::SchemaMismatch { actual } => HarnessProbe {
            root: Some(global_db),
            confidence,
            state: ProbeState::FileTarget,
            bytes: info.total_bytes,
            note: format!(
                "{env_note}检测到 SQLite 但表结构不认识（期望 cursorDiskKV(key, value)，实到 {actual}）；legacy workspaceStorage 未找到可读 composer 数据"
            ),
            ..base
        },
        SqliteSessionProbe::ReadFailed { error } => HarnessProbe {
            root: Some(global_db),
            confidence,
            state: ProbeState::FileTarget,
            bytes: info.total_bytes,
            note: format!(
                "{env_note}检测到 SQLite 但只读枚举失败（{error}）；legacy workspaceStorage 未找到可读 composer 数据"
            ),
            ..base
        },
    }
}

fn cursor_user_dir_from_global_db(global_db: &Path) -> Option<PathBuf> {
    let global_storage = global_db.parent()?;
    if global_storage.file_name()?.to_string_lossy() != "globalStorage" {
        return None;
    }
    Some(global_storage.parent()?.to_path_buf())
}

/// The cell that declares this harness's SQLite schema.
///
/// A store's table layout does not change across operating systems; only its
/// location does. So a platform cell that omits `sql_table` is a gap in *that
/// row*, not a claim that the schema differs — borrow the declaration from
/// whichever cell carries one instead of falling back to opencode's default
/// `session` schema and then reporting "表结构不认识" about a store we can
/// perfectly well read. A borrowed schema that genuinely does not fit still
/// reports `SchemaMismatch` (unknown), never a count.
fn schema_cell<'a>(h: &'a RegistryHarness, cell: &'a RegistryCell) -> &'a RegistryCell {
    if cell.sql_table.is_some() {
        return cell;
    }
    [&h.paths.macos, &h.paths.linux, &h.paths.windows]
        .into_iter()
        .flatten()
        .find(|candidate| candidate.sql_table.is_some())
        .unwrap_or(cell)
}

/// Is a configured root a single-file store or a directory to walk?
///
/// What is on disk decides, because that is the thing we are about to probe.
/// When nothing is there the basename shape decides — and either answer then
/// lands on `Missing`/`目录不存在`, i.e. an unknown count, never a fake `0`.
fn explicit_root_is_file(root: &Path) -> bool {
    match fs::metadata(root) {
        Ok(md) => md.is_file(),
        Err(_) => looks_like_file_path(&root.to_string_lossy()),
    }
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
                // `~` means "home" only as the *leading* token of the template
                // — the same rule [`expand_tilde`] applies to a configured
                // path. Anywhere else it is an ordinary filename character:
                // Windows spells a long user name's 8.3 short name `RUNNER~1`,
                // so `C:\Users\RUNNER~1\...` is a perfectly resolvable absolute
                // path and must not be mistaken for an unknown `~something`.
                if !out.is_empty() {
                    out.push('~');
                    rest = &rest[1..];
                    continue;
                }
                if rest == "~" {
                    // Template is exactly `~`.
                    return Some((home_dir(), false));
                }
                if let Some(tail) = rest.strip_prefix("~/") {
                    out.push_str(&home_dir().to_string_lossy());
                    out.push('/');
                    rest = tail;
                    continue;
                }
                return None; // leading `~something` — not a shape we know.
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
                // `%VAR%` (Windows style). The known set mirrors the XDG one:
                // the standard Windows base directories, resolved from the
                // environment when exported and from their documented
                // `%USERPROFILE%` defaults otherwise. Anything else is a
                // per-install override this tool does not resolve.
                let end = rest[1..].find('%').map(|i| i + 1);
                let Some(end) = end else { return None };
                match &rest[1..end] {
                    "USERPROFILE" => out.push_str(&home_dir().to_string_lossy()),
                    "APPDATA" => out.push_str(&appdata_dir().to_string_lossy()),
                    "LOCALAPPDATA" => out.push_str(&local_appdata_dir().to_string_lossy()),
                    _ => return None,
                }
                rest = &rest[end + 1..];
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
#[derive(Debug)]
struct DirectoryScan {
    records: Vec<SessionRecord>,
    /// Candidate session files whose metadata could not be handed over. The
    /// filename and declared suffix make this an exact session count.
    unreadable_count: u64,
    /// Entries/subtrees for which the scanner cannot know how many session
    /// files are inside.
    unreadable_entry_count: u64,
}

fn collect_records(
    root: &Path,
    source: HarnessSource,
    machine: &str,
    format: &str,
    session_pattern: Option<&str>,
) -> DirectoryScan {
    let mut records = Vec::new();
    let mut unreadable_count = 0;
    let mut unreadable_entry_count = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => {
                unreadable_entry_count += 1;
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    unreadable_entry_count += 1;
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => {
                    unreadable_entry_count += 1;
                    continue;
                }
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue; // skip sockets, FIFOs, and (crucially) symlinks
            }
            match build_record(&path, source, machine, format, session_pattern) {
                RecordBuild::Record(record) => records.push(record),
                RecordBuild::Unreadable => unreadable_count += 1,
                RecordBuild::NotSession => {}
            }
        }
    }
    DirectoryScan {
        records,
        unreadable_count,
        unreadable_entry_count,
    }
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
#[derive(Debug)]
enum RecordBuild {
    NotSession,
    Unreadable,
    Record(SessionRecord),
}

fn build_record(
    path: &Path,
    expected: HarnessSource,
    machine: &str,
    format: &str,
    session_pattern: Option<&str>,
) -> RecordBuild {
    let Some(name) = path.file_name().map(|name| name.to_string_lossy()) else {
        return RecordBuild::NotSession;
    };
    let Some((stem, compressed)) = strip_format_suffix(&name, format) else {
        return RecordBuild::NotSession;
    };
    if format_suffixes(format)
        .iter()
        .any(|suffix| suffix == ".json")
        && session_pattern.is_none()
    {
        return RecordBuild::NotSession;
    }
    if !matches_session_pattern(&name, session_pattern) || stem.is_empty() {
        return RecordBuild::NotSession;
    }

    let source = detect_source(path).unwrap_or(expected);
    let source_short = source.short();

    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(_) => return RecordBuild::Unreadable,
    };
    // Cache both numeric stat results in one call to avoid a second stat.
    let byte_size = meta.len();
    let mtime: SystemTime = match meta.modified() {
        Ok(mtime) => mtime,
        Err(_) => return RecordBuild::Unreadable,
    };

    let ident = crate::id::SessionIdentity {
        source_short,
        machine: machine.to_string(),
        native_id: stem,
    };

    RecordBuild::Record(SessionRecord {
        id: ident.id(),
        absolute_path: absolutize(path),
        byte_size,
        mtime,
        source,
        compressed,
        sqlite_layout: None,
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

    /// A `~` inside a path component is a filename character, not a home
    /// marker — Windows 8.3 short names (`RUNNER~1`) are the everyday case.
    /// A *leading* `~something` is still refused: unknown stays unknown.
    #[test]
    fn tilde_inside_a_component_is_a_literal_not_home() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        env::set_var("HOME", dir.path());

        let (root, is_file) =
            static_prefix_root(r"C:\Users\RUNNER~1\AppData\Local\Temp\gemini\chats").unwrap();
        assert_eq!(
            root,
            PathBuf::from(r"C:\Users\RUNNER~1\AppData\Local\Temp\gemini\chats")
        );
        assert!(!is_file);

        let (root, _) = static_prefix_root("/var/folders/a~b/T/.tmpX/chats").unwrap();
        assert_eq!(root, PathBuf::from("/var/folders/a~b/T/.tmpX/chats"));

        // `%USERPROFILE%` expansion still happens, and a short name after it is
        // still literal.
        let (root, _) = static_prefix_root(r"%USERPROFILE%\PROGRA~1\x").unwrap();
        assert_eq!(
            root,
            PathBuf::from(format!("{}\\PROGRA~1\\x", dir.path().display()))
        );

        // Leading forms are untouched.
        assert_eq!(static_prefix_root("~").unwrap().0, dir.path());
        assert_eq!(
            static_prefix_root("~/.claude/projects").unwrap().0,
            dir.path().join(".claude/projects")
        );
        assert!(
            static_prefix_root("~foo/bar").is_none(),
            "开头的 `~something` 依然锚不住：未知不能当成空"
        );
        let _ = dir;
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
    fn registry_env_override_wins_for_cursor_user_dir() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::TempDir::new().unwrap();
        env::set_var("CURSOR_USER_DIR", dir.path());
        let cell: RegistryCell = serde_json::from_value(serde_json::json!({
            "template": "~/Library/Application Support/Cursor/User/globalStorage/state.vscdb",
            "env_override": "CURSOR_USER_DIR",
            "format": "sqlite"
        }))
        .unwrap();
        let (root, is_file) = root_from_env_override(&cell).unwrap();
        assert_eq!(root, dir.path().join("globalStorage/state.vscdb"));
        assert!(is_file);
        env::remove_var("CURSOR_USER_DIR");
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

    /// All three platform cells carry the same unascertained entry on purpose.
    /// The subject here is what the scanner does with an unascertained cell, not
    /// which platform it is running on — and a registry that only fills in
    /// `macos` makes the assertion pass on a developer's Mac while producing
    /// zero probes anywhere else. That is not a weaker test, it is a test that
    /// silently stops testing, which is how it reached CI green on macOS and red
    /// on Linux.
    fn unascertained_registry() -> HarnessRegistry {
        serde_json::from_str(&format!(
            r#"{{
              "schema_version": 1,
              "generated": "2026-08-16",
              "harnesses": [
                {{ "id": "claude-code", "display_name": "Claude Code",
                   "paths": {{
                     "macos": {{ "template": "~/whatever/", "format": "jsonl",
                       "confidence": "{CONF_UNASCERTAINED}", "source": "x" }},
                     "linux": {{ "template": "~/whatever/", "format": "jsonl",
                       "confidence": "{CONF_UNASCERTAINED}", "source": "x" }},
                     "windows": {{ "template": "~/whatever/", "format": "jsonl",
                       "confidence": "{CONF_UNASCERTAINED}", "source": "x" }}
                   }} }}
              ]
            }}"#
        ))
        .unwrap()
    }

    #[test]
    fn unascertained_cell_is_skipped_not_scanned() {
        // Nothing tells the scanner where this harness lives, so the only path
        // available is the one the registry itself calls unverified: skip it.
        let report = scan_with_registry(&Config::default(), &unascertained_registry()).unwrap();
        assert!(report.records.is_empty());
        assert_eq!(report.probes.len(), 1);
        assert_eq!(report.probes[0].state, ProbeState::SkipUnascertained);
        assert!(!report.probes[0].installed_p());
    }

    /// The other side of the same gate: `未查明` says *we* could not verify a
    /// path, which has nothing to say about a path the **user** wrote down. A
    /// configured root is walked, and its real count reported — refusing to
    /// count a store we opened would be its own false claim.
    #[test]
    fn configured_root_is_scanned_even_when_the_cell_is_unascertained() {
        let home = tempfile::TempDir::new().unwrap();
        let root = home.path().join("elsewhere");
        fs::create_dir_all(root.join("repo-abc")).unwrap();
        fs::write(root.join("repo-abc/019bf00d-0000.jsonl"), "{}\n").unwrap();

        let config = Config {
            harness_roots: [("claude-code".to_string(), root.to_string_lossy().into())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let report = scan_with_registry(&config, &unascertained_registry()).unwrap();
        assert_eq!(report.probes.len(), 1);
        assert_eq!(report.probes[0].state, ProbeState::Scanned);
        assert_eq!(report.probes[0].root.as_deref(), Some(root.as_path()));
        assert_eq!(report.probes[0].record_count, Some(1));
        assert_eq!(report.records.len(), 1);
    }

    /// …and a configured root that is **not there** stays unknown. The gate
    /// moved; "unknown is not empty" did not.
    #[test]
    fn configured_root_that_does_not_exist_is_unknown_not_zero() {
        let home = tempfile::TempDir::new().unwrap();
        let root = home.path().join("never-created");
        let config = Config {
            harness_roots: [("claude-code".to_string(), root.to_string_lossy().into())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let report = scan_with_registry(&config, &unascertained_registry()).unwrap();
        assert_eq!(report.probes[0].state, ProbeState::Missing);
        assert_eq!(report.probes[0].record_count, None);
        assert!(!report.probes[0].installed_p());
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
    fn explicit_registry_override_is_authoritative() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let previous = env::var_os(REGISTRY_ENV);
        let dir = tempfile::TempDir::new().unwrap();
        let override_path = dir.path().join("registry.json");
        fs::write(
            &override_path,
            br#"{"schema_version":1,"generated":"test","harnesses":[]}"#,
        )
        .unwrap();

        env::set_var(REGISTRY_ENV, &override_path);
        let loaded = load_registry_from_repo().unwrap();
        assert!(loaded.harnesses.is_empty());

        env::set_var(REGISTRY_ENV, dir.path().join("missing.json"));
        let missing = load_registry_from_repo().unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);

        fs::write(&override_path, b"not json").unwrap();
        env::set_var(REGISTRY_ENV, &override_path);
        let invalid = load_registry_from_repo().unwrap_err();
        assert_eq!(invalid.kind(), io::ErrorKind::InvalidData);

        match previous {
            Some(value) => env::set_var(REGISTRY_ENV, value),
            None => env::remove_var(REGISTRY_ENV),
        }
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
