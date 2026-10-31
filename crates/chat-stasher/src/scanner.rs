//! Local harness scanner — driven by the harness path registry
//! (shipped in this crate as `data/harness-registry-v1.json`), which is
//! authoritative for which
//! harness × platform roots to probe. Nothing is hardcoded here: a root is
//! only scanned when the registry says to scan it.
//!
//! Each registry cell (harness × platform) carries a `confidence`:
//!   - `unascertained`            ⇒ the cell is **not** scanned (guessing a path would
//!                            produce false negatives/positives),
//!   - `community-claim-unverified`   ⇒ scanned, but flagged **low-confidence**, so the
//!                            report can mark it instead of presenting it as fact,
//!   - `measured-locally`          ⇒ verified empirically on the machine this tool runs
//!                            on (schema/path measured live) but source not read —
//!                            scanned, not low-confidence,
//!   - `source-confirmed`/`official-docs` ⇒ scanned normally.
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
//!   * read-only — nothing is opened for writing, and no session transcript is
//!     ever read: a scan reads directory names and file sizes only, so no
//!     session content can leak into a log or report. The one document a scan
//!     does parse is Kimi Code's `session_index.jsonl`, a per-session index of
//!     ids and working directories; it is opened for reading, is not a
//!     transcript, and only contributes the cwd dimension to a record's
//!     provenance,
//!   * `.jsonl.zst` files are *recognised and listed* (compressed flag set);
//!     decompression is a later spike but silently skipping them is not an
//!     option,
//!   * missing roots are reported, not fatal.

use crate::config::Config;
use crate::json_out;
use crate::models::{HarnessSource, SessionRecord, SqliteSessionLayout};
use crate::sqlite_probe::{
    enumerate_cursor_legacy_sessions, enumerate_openclaw_sessions, enumerate_opencode_sessions,
    enumerate_sqlite_sessions, probe_sqlite_store, sqlite_millis_to_system_time,
    CursorLegacySessionRow, SqliteSessionProbe, SqliteSessionRow,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Registry file, relative to the repo root, that drives the scan. Lives
/// inside this crate (`crates/chat-stasher/data/`) so the packaged crate
/// carries it; [`resolve_registry_path`] walks up to the repo root from the
/// cwd / manifest dir and joins this against every ancestor.
pub const REGISTRY_REL_PATH: &str = "crates/chat-stasher/data/harness-registry-v1.json";

/// Optional runtime registry override. This is intentionally checked before
/// both the repository copy and the embedded fallback.
pub const REGISTRY_ENV: &str = "CHAT_STASHER_REGISTRY";

/// The shipped registry is the data file itself, not a second hand-written
/// Rust representation. The file lives inside this crate — it must, or the
/// packaged crate could not compile — and is embedded from the crate
/// manifest at compile time, so an installed binary can scan outside the repo.
const EMBEDDED_REGISTRY: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/data/harness-registry-v1.json"
));

/// Confidence values used by the registry (`harnesses[].paths.*.confidence`).
pub const CONF_CONFIRMED: &str = "source-confirmed";
pub const CONF_OFFICIAL: &str = "official-docs";
pub const CONF_COMMUNITY: &str = "community-claim-unverified";
pub const CONF_MEASURED: &str = "measured-locally";
pub const CONF_UNASCERTAINED: &str = "unascertained";

// ---------------------------------------------------------------------------
// Confidence
// ---------------------------------------------------------------------------

/// How trustworthy a registry cell is. Anything not in the known set is
/// treated as [`Confidence::Unascertained`] — conservative, never scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// `source-confirmed` — confirmed in harness source.
    Confirmed,
    /// `official-docs` — confirmed in official docs.
    OfficialDocs,
    /// `community-claim-unverified` — plausible but never verified. Scan, but flag low.
    CommunityClaim,
    /// `measured-locally` — verified empirically on this machine (path + schema read
    /// live), but the harness source was not read. Scan, not flagged low —
    /// it is verified, just not source-anchored (so it can drift on an app
    /// update without any source link to re-check).
    Measured,
    /// `unascertained` — unknown. Do **not** scan (guessing = false pos/neg).
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

/// Top level of this crate's `data/harness-registry-v1.json`.
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
    /// Product taxonomy is independent from the collector that reads a
    /// harness (for example, disk scanners and extension capture can belong
    /// to distinct groups).
    #[serde(default)]
    pub product_group: Option<String>,
    #[serde(default)]
    pub paths: RegistryPaths,
    /// Per-machine input logs this harness's own local app keeps (ADR-053 D1):
    /// "what the user typed on this machine", which is a different product
    /// kind from a session transcript. Declared on the same entry, so the
    /// registry stays the single source of truth (ADR-048 D2) and there is no
    /// new harness id and no second registry. Missing key deserializes to an
    /// empty list: a harness that declares no machine log has none.
    #[serde(default)]
    pub machine_logs: Vec<RegistryMachineLog>,
    /// Additional compatibility roots scanned alongside the primary source.
    /// Hermes uses this for its pre-database session files.
    #[serde(default)]
    pub legacy_roots: Vec<String>,
    /// Optional independent source roots for one stable harness identity.
    /// Each root may contribute a different provenance observation while its
    /// records still share the same `<harness>.<machine>.<session>` identity.
    #[serde(default)]
    pub source_roots: Vec<RegistrySourceRoot>,
    /// Dimensions this harness can record. Values are declared at the source
    /// root or measured from the source; this block is descriptive only.
    #[serde(default)]
    pub dimensions: RegistryDimensions,
    /// Retention knobs of this harness's own settings store that a
    /// doctor-class probe should read (ADR-059 draft, D5). Data first: the
    /// item records where the key is read and how its values map to
    /// retention, so the probe can never re-derive a location or guess a
    /// default. Missing key deserializes to an empty list.
    #[serde(default)]
    pub retention_probes: Vec<RegistryRetentionProbe>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Active-file sealing policy for this harness: `rename` / `no-rename` /
    /// `not-applicable`. Lives here — the *same* file that drives scanning —
    /// so the sealing allowlist can never drift from the path registry. Any
    /// missing / unknown value resolves to `no-rename` (fail-safe, see
    /// `crate::seal`). To be on the `rename` list a harness needs a `source-confirmed`
    /// platform cell **and** a non-empty `seal_source` evidence line.
    #[serde(default)]
    pub seal_policy: String,
    /// Concrete measurement / source line backing `seal_policy`. Required for
    /// `rename`; the gate in `crate::seal::seal_allowed` rejects empty values,
    /// so an uncredited harness can never be rename-sealed.
    #[serde(default)]
    pub seal_source: String,
}

/// One declared machine log: an append-only per-machine file a harness's local
/// app writes, with no native session id and no session id ever minted for it
/// (ADR-053 D1). `role` is `machine-log` for every entry of the kind and exists
/// so the vocabulary is declared in the data rather than implied by the key
/// name; consumers split a harness entry into its session products (`paths`,
/// `source_roots`) and its machine logs (`machine_logs`), and only sessions
/// feed session machinery (ADR-053 D4/D7).
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryMachineLog {
    /// Stable log id, unique inside one harness entry — `history` for Claude
    /// Code. It is a path component of the stage namespace
    /// (`machine-logs/<machine>/<harness>/<log-id>/`), so it is validated
    /// before use: an id with a separator or a `..` would place the write
    /// outside the namespace.
    pub id: String,
    /// Product role. `machine-log` today; anything else is not a machine log
    /// this build collects.
    #[serde(default)]
    pub role: String,
    /// Per-platform cell of the log file itself. Unlike a session cell this
    /// names one **file**, not a directory tree.
    #[serde(default)]
    pub paths: RegistryPaths,
    /// Environment variable that relocates the harness's config base, and with
    /// it this file (ADR-053 D7 / F14 / OQ3). For Claude Code's
    /// `CLAUDE_CONFIG_DIR` the official directory page states that every
    /// `~/.claude` path lives under the named directory instead; the log is
    /// then `<value>/<file name of the platform template>`.
    #[serde(default)]
    pub env_override: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// One declared retention knob read from a harness's own settings store
/// (ADR-059 draft, D5). The registry item is the *declaration* of the probe:
/// which settings key, from which files, how each of the key's integer
/// states maps to retention, and which other key can override it. The probe
/// itself (doctor D1/D4 wiring) consumes this data later; the declaration
/// exists so the read locations and semantics are recorded facts, never
/// things the probe re-guesses.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryRetentionProbe {
    /// Stable probe id, unique inside one harness entry
    /// (`desktop-session-cleanup`).
    pub id: String,
    /// The settings key the probe reads, e.g.
    /// `desktopSessionCleanupPeriodDays`.
    pub key: String,
    /// How the three integer states of this key map to retention. Each value
    /// is the registry's recorded statement of one state's meaning; the
    /// probe reports the state a machine is in, never a re-derived default.
    pub states: RegistryRetentionStates,
    /// Recorded interaction with other keys: a managed source that sets
    /// `cleanupPeriodDays` makes this key ignored, and a report must say
    /// which source governed the effective value (user or managed).
    #[serde(default)]
    pub managed_override: String,
    /// Where the probe may look and where it must not. Free text on purpose:
    /// it is the recorded finding (measured, with the date), not a rule the
    /// code re-derives.
    #[serde(default)]
    pub notes: String,
    /// Per-platform read locations for the key.
    #[serde(default)]
    pub platforms: RegistryRetentionPaths,
}

/// The three integer states of a [`RegistryRetentionProbe`] key, as the
/// registry records them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegistryRetentionStates {
    /// The key is absent: the harness's built-in default applies. For
    /// `desktopSessionCleanupPeriodDays` that default is 0 = no ceiling — a
    /// different answer from the missing state of `cleanupPeriodDays`
    /// itself (which defaults to 30-day cleanup).
    #[serde(default)]
    pub missing: String,
    /// Explicit 0. Allowed for keys that bound an exemption from deletion
    /// but never disable writes; also unbounded retention.
    #[serde(default)]
    pub zero: String,
    /// A positive value in days; the effective retention is the longer of
    /// this ceiling and the effective `cleanupPeriodDays`.
    #[serde(default)]
    pub positive_int: String,
}

/// Per-platform cells of a [`RegistryRetentionProbe`]: which files hold the
/// user-level and the policy-managed values of the key.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegistryRetentionPaths {
    #[serde(default)]
    pub macos: Option<RegistryRetentionPlatform>,
    #[serde(default)]
    pub linux: Option<RegistryRetentionPlatform>,
    #[serde(default)]
    pub windows: Option<RegistryRetentionPlatform>,
}

impl RegistryRetentionPaths {
    /// The read-location cell for `platform` ("macos"/"linux"/"windows"),
    /// if the registry declares one — `None` is "no declared locations for
    /// this platform", never "the key cannot exist here".
    pub fn cell_for(&self, platform: &str) -> Option<&RegistryRetentionPlatform> {
        match platform {
            "macos" => self.macos.as_ref(),
            "linux" => self.linux.as_ref(),
            "windows" => self.windows.as_ref(),
            _ => None,
        }
    }
}

/// One platform's read locations for a retention key. A cell with
/// `confidence: "measured-locally"` must declare what its measurement found
/// (the parser enforces that measured user *and* managed source lists are
/// non-empty); a cell whose managed sources were not measured declares an
/// empty list and says so in `source`, because a guessed path is not a fact.
#[derive(Debug, Clone, Deserialize)]
pub struct RegistryRetentionPlatform {
    /// Files the runtime's *user* settings layer can hold the key in.
    #[serde(default)]
    pub user_sources: Vec<String>,
    /// Files the runtime's *managed* (policy) settings layer can hold the
    /// key in. Empty means the platform's managed locations were not
    /// measured — never "there are none".
    #[serde(default)]
    pub managed_sources: Vec<String>,
    #[serde(default)]
    pub confidence: String,
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegistryDimensions {
    #[serde(default)]
    pub supported: Vec<String>,
    #[serde(default)]
    pub surfaces: Vec<String>,
    #[serde(default)]
    pub tenant_label: Option<String>,
    #[serde(default)]
    pub container_label: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegistrySourceRoot {
    pub id: String,
    pub paths: RegistryPaths,
    /// Marked `"entry"` (the only value today) when this root's
    /// [`RegistryCell::env_override`] replaces the harness's whole base
    /// directory — channel suffix included — for every build of the harness,
    /// not just this root's (ADR-054 D7). An entry-wide override that is set
    /// collapses the entry to exactly this root plus its override path, and
    /// the entry's `default_only` roots stay unsuppressed only when the user
    /// stated them explicitly themselves.
    #[serde(default)]
    pub override_scope: Option<String>,
    /// This root is active only while no entry-wide override is in force
    /// (ADR-054 D7). One registry entry's channels share one layout, and a
    /// base-replacing override (`FREEBUFF_CONFIG_DIR`) makes the channel
    /// roots meaningless: scanning the defaults as well would read the same
    /// directory twice under two root ids, or read locations the harness no
    /// longer writes.
    #[serde(default)]
    pub default_only: bool,
    #[serde(default)]
    pub provenance: crate::provenance::SessionProvenance,
    /// This root belongs to a store that MOVES its files between the roots
    /// of one registry entry, keeping the file's own name as the session's
    /// identity wherever it sits (W1007, ADR-059 draft D4): codex's
    /// `archived_sessions/` to and from which the desktop's
    /// `thread/archive` / `thread/unarchive` moves rollouts. The flag rides
    /// every record discovered through this root as
    /// [`crate::models::SessionRecord::movable_root`], where it lets the
    /// collector treat a same-id discovery at a new path as the same file —
    /// recovering its position from the stage by content instead of
    /// re-sealing bytes the archive already holds.
    #[serde(default)]
    pub movable: bool,
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
    #[serde(default, alias = "sql_timestamp_column")]
    pub sql_time_column: Option<String>,
    /// JSON path (e.g. `$.createdAt`) into `sql_value_column`, epoch millis.
    #[serde(default)]
    pub sql_time_json_path: Option<String>,
    /// True when `sql_time_column` holds Unix seconds instead of millis.
    #[serde(default)]
    pub sql_time_value_is_seconds: bool,
    /// True when `sql_time_column` holds ISO 8601 strings instead of numbers.
    #[serde(default)]
    pub sql_time_is_iso8601: bool,
    /// Optional timestamp format indicator (e.g. "iso8601").
    #[serde(default)]
    pub sql_time_format: Option<String>,
    /// Optional named qualification rule for JSON key/value stores.
    #[serde(default)]
    pub sql_qualification: Option<String>,
    /// Optional "one directory per session" rule. Present only for harnesses
    /// that give every session its own directory whose *name* is the session
    /// id; see [`SessionDirRule`].
    #[serde(default)]
    pub session_dir: Option<SessionDirRule>,
}

/// A harness that gives every session its own directory, and writes the
/// transcript at a declared path inside it.
///
/// Kimi Code is the case the literal `file` target exists for:
/// `<sessions>/<workspaceId>/session_<uuid>/agents/main/wire.jsonl`. The
/// file's own stem is the constant `wire`, so the walk in
/// [`collect_records`] cannot identify a session from the discovered path
/// alone — every session on the machine would come out with the same native
/// id (`wire`), and two different conversations would collapse into one
/// archive slot. A cell that declares this rule takes the id from the
/// directory instead.
///
/// `file_glob` is the target for stores whose transcript *name* is the
/// session id and whose path inside the session directory is not fixed:
/// Claude Desktop's Cowork sandboxes keep the transcript at
/// `​.claude/projects/<sanitized-cwd>/<sid>.jsonl`, so neither the literal
/// shape nor the directory-name id can express it. A glob rule ids every
/// record by the matched file's own stem (the `<sid>` filename), which is
/// what the sandbox's adjacent metadata json points to with its
/// `cliSessionId`; the sandbox directory's own name is not the identity
/// (ADR-059 draft, D2/D3). Two files matching the glob are two sessions,
/// not one — the id-collision concern that keeps the literal target
/// literal is answered here by the id coming from each file's own name.
///
/// Exactly one of `file` / `file_glob` must be declared per rule; the
/// registry parser enforces that at load time (see [`parse_registry`]) so
/// a data defect is a load error, never a silent empty scan.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionDirRule {
    /// Root-relative directory glob for the session directory. Without a
    /// leading `**`, slash-separated components such as `*` and `*/*` match
    /// one exact depth below the root. A leading `**` matches zero or more
    /// whole directory components before the remaining pattern, for stores
    /// whose sessions can appear at multiple depths or nest inside sessions.
    /// Other components use the same `*` glob syntax as
    /// [`RegistryCell::session_pattern`].
    pub pattern: String,
    /// Literal path from that directory to the transcript file, `/`-separated
    /// on every platform (compared component by component, so a Windows
    /// backslash path matches too). Literal on purpose: a glob here could
    /// admit a second file per session and reintroduce the id collision this
    /// rule exists to prevent. Mutually exclusive with [`Self::file_glob`].
    #[serde(default)]
    pub file: Option<String>,
    /// Glob path (same component-wise syntax as [`Self::pattern`], no
    /// leading `**`) from the session directory to the transcript file.
    /// The matched file's own filename stem — after the declared format
    /// suffix is stripped — becomes the record's native id. Mutually
    /// exclusive with [`Self::file`].
    #[serde(default)]
    pub file_glob: Option<String>,
    /// Indices (0-based, `/`-separated) into the components the pattern
    /// matched, joined by `.` to compose the native id. The default —
    /// omitted, or empty — is the last matched component, the pre-codebuff
    /// behaviour of every existing rule. Codebuff declares `[1, 3]` for
    /// `projects/*/chats/*`: the pair `<project-basename>.<chatId>` is the
    /// chat's on-disk identity, and the chat directory name alone would
    /// collapse two chats with the same millisecond id under different
    /// projects into one archive slot (ADR-054 D3). Indices outside the
    /// matched components fall back to the last component rather than
    /// composing from a subset — a wrong selection is a registry bug, not a
    /// licence to mint a narrower id. Applies to the literal `file` target
    /// only: a `file_glob` record is id'd by the matched transcript's own
    /// stem, which the pattern components never qualify.
    #[serde(default)]
    pub id_components: Vec<usize>,
    /// Additional literal file names (never globs — see [`Self::file`])
    /// sealed as raw bytes under the *same* session identity, when present
    /// beside the transcript (ADR-054 D5/D9). Absent files are a normal
    /// state, not an error: the sidecar rides along only when the harness
    /// wrote it.
    #[serde(default)]
    pub extra_files: Vec<String>,
}

impl SessionDirRule {
    /// The transcript target this rule matches files against: the literal
    /// [`Self::file`] path or the [`Self::file_glob`] pattern. The parser
    /// guarantees exactly one is declared, so the fallback is only a
    /// never-matches guard against hand-built (test) rules.
    fn transcript_target(&self) -> TranscriptTarget<'_> {
        match (self.file.as_deref(), self.file_glob.as_deref()) {
            (Some(_), Some(glob)) => TranscriptTarget::Glob(glob),
            (Some(file), None) => TranscriptTarget::Literal(file),
            (None, Some(glob)) => TranscriptTarget::Glob(glob),
            (None, None) => TranscriptTarget::Glob(""),
        }
    }
}

/// Which transcript target a [`SessionDirRule`] matches against.
#[derive(Debug, Clone, Copy)]
enum TranscriptTarget<'a> {
    /// The literal `/`-separated path from the session directory.
    Literal(&'a str),
    /// The `/`-separated component glob from the session directory.
    Glob(&'a str),
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
    let registry: HarnessRegistry = serde_json::from_str(raw).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("harness registry {source} is not valid JSON: {e}"),
        )
    })?;
    validate_registry(&registry, source)?;
    Ok(registry)
}

/// Structural checks the shape alone cannot express (serde accepts both
/// targets present, either absent, and a `**` component a `file_glob` is
/// not defined to match). They run at load time so a registry data defect is
/// a load error naming the harness and platform, never a silently empty
/// scan or a glob that quietly behaves like `*`.
fn validate_registry(registry: &HarnessRegistry, source: &str) -> io::Result<()> {
    for harness in &registry.harnesses {
        let mut cells: Vec<(String, &RegistryCell)> = Vec::new();
        for platform in ["macos", "linux", "windows"] {
            if let Some(cell) = harness.paths.cell_for(platform) {
                cells.push((format!("{}/{platform}", harness.id), cell));
            }
            for root in &harness.source_roots {
                if let Some(cell) = root.paths.cell_for(platform) {
                    cells.push((format!("{}/:{}/{platform}", harness.id, root.id), cell));
                }
            }
        }
        for (where_, cell) in cells {
            if let Some(rule) = cell.session_dir.as_ref() {
                let declared = rule.file.is_some() as u8 + rule.file_glob.is_some() as u8;
                if declared != 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "harness registry {source}: session_dir rule for {where_} must \
                             declare exactly one transcript target: `file` (literal) or \
                             `file_glob` (the matched stem ids the session)"
                        ),
                    ));
                }
                if let Some(glob) = rule.file_glob.as_deref() {
                    if glob
                        .split('/')
                        .filter(|part| !part.is_empty())
                        .any(|part| part == "**")
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "harness registry {source}: session_dir file_glob for {where_} \
                                 matches one exact component depth; a `**` component is not \
                                 defined here — write the measured layout out in components"
                            ),
                        ));
                    }
                }
            }
        }
        for probe in &harness.retention_probes {
            for (platform, cell) in [
                ("macos", probe.platforms.macos.as_ref()),
                ("linux", probe.platforms.linux.as_ref()),
                ("windows", probe.platforms.windows.as_ref()),
            ] {
                let Some(cell) = cell else { continue };
                if cell.confidence == "measured-locally"
                    && (cell.user_sources.is_empty() || cell.managed_sources.is_empty())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "harness registry {source}: retention probe `{}/{}` has a \
                             measured-locally {platform} cell, but a measured cell declares \
                             what it measured: both the user and the managed source lists \
                             must be non-empty (an unmeasured managed list is declared \
                             empty with an unascertained cell that says why)",
                            harness.id, probe.id
                        ),
                    ));
                }
                if cell.confidence != "measured-locally" && !cell.managed_sources.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "harness registry {source}: retention probe `{}/{}` declares \
                             managed sources on {platform} it does not claim to have \
                             measured; measure them or move them behind a \
                             measured-locally cell",
                            harness.id, probe.id
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
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
    /// Scanned but the root / file does not exist here. Reserved for a
    /// *measured* absence: the path was looked up and is genuinely not there.
    Missing,
    /// B82: we could not establish whether this harness's store is here at
    /// all — the metadata call failed (permissions, I/O), the path exists but
    /// is not the kind of thing the registry declares, or the store's own
    /// walk could not enumerate. This used to collapse into [`Self::Missing`]
    /// and print `missing` / `0`, which turns "I could not confirm" into
    /// "confirmed nothing". The word `Indeterminate` is the one already used
    /// for the same distinction in `destinit.rs`.
    Indeterminate,
    /// Skipped: confidence `unascertained` — scanning a guessed path is forbidden.
    SkipUnascertained,
    /// Skipped: no registry cell for this platform.
    SkipWrongPlatform,
    /// Skipped: template does not reduce to a static root (e.g. `$CWD`/`<...>`
    /// unresolvable, or harness id unknown to this build).
    SkipUnresolvable,
}

/// One declared source root of a family that this scan never verified — a
/// root the probe refused to guess at (`unascertained`), never resolved a
/// path for (`unresolvable`), or could not even establish the presence of
/// (`indeterminate`) — while at least one sibling root of the same family
/// **was** measured (ADR-059 draft D6, the qualified lower-bound rule the
/// external review of the draft made mandatory, W1006d). The root is named by
/// its registry root id (`claude-desktop-cowork`); the one shape where the
/// unverified root is the entry's primary store names it by the canonical
/// harness id, the key that store is configured under.
#[derive(Debug, Clone)]
pub struct UnverifiedRoot {
    pub id: String,
    /// Why the root stayed unverified, from the same closed vocabulary as
    /// [`probe_state_word`] — never a path, never prose.
    pub state: ProbeState,
}

/// One declared source root of a *primary-plus-roots* family, measured on its
/// own beside the family aggregate (W1009, DSK-3: doctor D3 prints one row
/// per source root; ADR-059 draft D5/D6). A row exists for every root the
/// registry declares **for this platform** — the primary store first, then
/// every additional `source_roots` entry in declaration order — so an
/// unverified root keeps its own marked row instead of being folded into a
/// number that reads as the family total. A root whose cell this platform does
/// not declare never gets a probe (the scan loop skips it before probing, the
/// same way a platform without a harness cell skips that harness:
/// `SkipWrongPlatform`'s "nothing here to have missed"), so it gets no row
/// either. All-roots families (codebuff's channels, antigravity's surfaces)
/// keep their pre-DSK-1 aggregate semantics and carry no rows, per the same
/// D6 scope the aggregate rules use.
#[derive(Debug, Clone)]
pub struct SourceRootProbe {
    /// The registry root id this row stands for — the canonical harness id
    /// for the primary store (the same id [`UnverifiedRoot`] names it by),
    /// the root's own id for every additional source root.
    pub id: String,
    /// True when this row is the family's primary store (the harness's own
    /// `paths` cells) rather than a declared `source_roots` entry.
    pub primary: bool,
    pub state: ProbeState,
    /// The root path this row's own subprobe resolved, when it resolved one.
    pub root: Option<PathBuf>,
    /// Distinct session ids this root's own walk produced. `None` when this
    /// root was not measured (the state says why). The row's scope is the
    /// root only: one session id sitting in two roots of a movable store
    /// (W1007's `thread/archive` copy) counts in each row it appears in,
    /// while the family aggregate counts it once — that is the scope each
    /// number claims, and neither is the other.
    pub record_count: Option<u64>,
    pub unreadable_count: Option<u64>,
    pub unreadable_entry_count: Option<u64>,
    /// Sum of this root's records' byte sizes. `Some(0)` for a scanned root
    /// that holds nothing (a measured empty); `None` when no walk produced
    /// records for this root.
    pub total_bytes: Option<u64>,
    pub note: String,
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
    /// Roots of this family that stayed **unverified** while a sibling root
    /// was measured. Non-empty exactly when the counts below are a
    /// *qualified lower bound* over the measured roots: `record_count` (and
    /// every sibling count) is then the exact count of the measured roots
    /// only, and must be rendered as `≥N` with these roots named beside it —
    /// never as an unknown that erases the measured part (the D6 side
    /// effect the ADR-059 draft review rejected) and never as a plain
    /// number that silently claims the family total. The rule is scoped to
    /// the primary-plus-additional-roots shape (the W1006c narrowing): a
    /// family whose every root stayed unknown keeps the older aggregate
    /// semantics and this field empty.
    pub unverified_roots: Vec<UnverifiedRoot>,
    /// One row per declared, platform-relevant source root of this family
    /// (W1009/DSK-3): the same roots the aggregate merged, measured on
    /// their own so `doctor` D3 can print one row per source root with
    /// unverified roots explicitly marked rather than folded away. Empty
    /// for every family outside the primary-plus-roots shape (see
    /// [`SourceRootProbe`] for the scope rule).
    pub source_roots: Vec<SourceRootProbe>,
    /// Session records found at this harness's root, counted by **distinct
    /// session id** — the quantity collect can address, and the quantity the
    /// multi-source-root aggregation already counted (a file count cannot
    /// stand in for it where several files share one stem). `None` when this
    /// harness cannot be enumerated at all (missing, skipped, single-file
    /// store whose schema we do not recognise) — never a fabricated zero.
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
    ///
    /// B90: `None` therefore covers two shapes of not-knowing — "nothing was
    /// enumerated" (`record_count` is `None` too, and `state` says why) and
    /// "enumeration worked but the unreadable tally itself could not be taken"
    /// (`record_count` is `Some`). Display code must tell them apart by
    /// `record_count`, and must never render either as `0`.
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
    /// SQLite). `None` means the byte probe did not finish; it is not zero.
    pub bytes: Option<u64>,
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

    /// True when this harness was **never looked at**: the probe resolved no
    /// root, was forbidden to scan a guessed path, or could not even establish
    /// whether the source exists.
    ///
    /// This is the definition `status`'s "⚠ but N harness(es) were not probed
    /// at all" warning already used (`main.rs`, `unlooked_notice`). It lives
    /// here now, as one predicate, because W285 §6 needed the same distinction
    /// in `doctor`'s coverage summary: a header counting every registry harness
    /// in its denominator — `N/12 harnesses hit` — reads as a measurement over
    /// all twelve, while some of them were never opened. Two commands each
    /// spelling their own version of "not probed" is how `doctor` and `status`
    /// start contradicting each other about the same machine.
    ///
    /// Two deliberate exclusions, both from the recorded B82 wording:
    ///   * `Missing` — looked, and nothing is there. A measured absence, and
    ///     the one state where a count of `0` is earned.
    ///   * `SkipWrongPlatform` — the registry has no cell for this platform, so
    ///     there is nothing on this machine to have missed.
    pub fn not_probed_p(&self) -> bool {
        matches!(
            self.state,
            ProbeState::SkipUnascertained
                | ProbeState::SkipUnresolvable
                | ProbeState::Indeterminate
        )
    }

    /// True when this probe actually looked — whether or not the store was
    /// there. The complement of [`Self::not_probed_p`], and the other half of
    /// the pair: `installed_p` answers "is it here", this answers "did we
    /// check", and they come apart in both directions (`Missing` looked and
    /// found nothing; every `Skip*` state found nothing because it never
    /// looked).
    pub fn probed_p(&self) -> bool {
        !self.not_probed_p()
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
    /// Roots (as resolved) that did not exist on this machine. Measured
    /// absence only — a root we could not look at goes in
    /// [`Self::indeterminate_roots`], because "skipped N non-existent source root(s)"
    /// must not count paths whose existence was never established.
    pub missing_roots: Vec<PathBuf>,
    /// Roots whose existence or type could not be established (permissions,
    /// I/O error, wrong file type, un-enumerable store).
    pub indeterminate_roots: Vec<PathBuf>,
    /// Per-harness fate, in registry order.
    pub probes: Vec<HarnessProbe>,
    /// Declared machine logs that exist on this machine (ADR-053 D1/D7), in
    /// registry order. A **separate** vector from [`Self::records`] on purpose:
    /// a machine log is not a session, so it never enters session lists,
    /// session counts, coverage or `machine_recall` (D4/D6), and no session
    /// walker can trip over it by construction. Discovery is home-keyed and
    /// never matches a session path template.
    pub machine_logs: Vec<MachineLogRecord>,
    /// Machine logs the registry declares that this pass never looked at —
    /// no cell for this platform, a cell whose confidence is `unascertained`,
    /// or a template that does not resolve. B82's rule: a declared source that
    /// was not looked at is an unknown, never an empty one.
    pub machine_logs_unlooked: usize,
    /// Machine logs that were looked at but whose shape could not be
    /// established (permission/I/O error, or a directory where the registry
    /// declares a file). Distinct from [`Self::machine_logs_unlooked`] for the
    /// same reason `indeterminate_roots` is distinct from `missing_roots`.
    pub machine_logs_indeterminate: usize,
    /// Why each [`Self::machine_logs_unlooked`] log was not looked at, in the
    /// scanner's own fixed vocabulary (never a path, never a hostname). The
    /// reasons are a closed set of registry facts — an `unascertained` cell, no
    /// cell for this platform, an unresolvable template — so a reader can tell
    /// a policy decision from an oversight.
    pub machine_logs_unlooked_reasons: Vec<&'static str>,
}

/// One declared machine log file that exists on this machine.
///
/// It carries no session id by construction: the canonical
/// `<source>.<machine>.<native-id>` grammar stays reserved for sessions
/// (ADR-002), so what identifies a machine log is the `(machine, harness,
/// log_id)` triple — which is exactly the stage namespace it is sealed under
/// (ADR-053 D4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineLogRecord {
    /// Registry harness id that declares the log (`claude-code`).
    pub harness: String,
    /// The log's own declared id (`history`).
    pub log_id: String,
    /// Absolute path of the log file, as resolved on this machine.
    pub absolute_path: PathBuf,
    /// File size in bytes at scan time.
    pub byte_size: u64,
}

/// A harness probe recognised more distinct sessions than the scanner
/// produced distinct `SessionRecord` ids for. `collect` consumes only those
/// records, so the difference is currently not archivable by this build.
/// Both counts are of distinct ids — several source files aliasing onto one
/// id is a slot merge the archive records elsewhere, not a gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveGap {
    pub harness_id: String,
    pub display_name: String,
    pub recognized_sessions: Option<u64>,
    pub session_records: usize,
}

impl ScanReport {
    /// The distinct session ids among the records each source emitted,
    /// grouped in one pass over the report's records. `collect` stages one
    /// dir per id, so both per-harness consumers of a report's records —
    /// the staged-session count behind `found_staged`
    /// ([`probe_found_staged`], used by [`scan_report_json`]) and
    /// [`archive_gaps`]'s session-records side — answer from these sets;
    /// grouping once per report keeps each consumer at one record
    /// traversal instead of one per probe. A source with no records
    /// simply has no entry: its id set is the measured empty.
    fn staged_id_sets_per_source(
        &self,
    ) -> std::collections::HashMap<HarnessSource, std::collections::BTreeSet<&str>> {
        let mut grouped: std::collections::HashMap<_, std::collections::BTreeSet<_>> =
            std::collections::HashMap::new();
        for record in &self.records {
            grouped
                .entry(record.source)
                .or_default()
                .insert(record.id.as_str());
        }
        grouped
    }

    /// Return installed harnesses whose recognised session count is greater
    /// than the number of records that `collect` can consume. Both sides
    /// count **distinct session ids** — mirroring the multi-root aggregation
    /// — so files that alias onto one id never fabricate a gap out of the
    /// missing file-to-record difference; the comparison is deliberately
    /// made against the records actually produced by this scan, and no
    /// harness allowlist is involved.
    pub fn archive_gaps(&self) -> Vec<ArchiveGap> {
        self.archive_gaps_with_id_sets(&self.staged_id_sets_per_source())
    }

    /// [`archive_gaps`] with the per-source id sets supplied, so a caller
    /// that already grouped the report's records ([`scan_report_json`])
    /// does not traverse them a second time.
    fn archive_gaps_with_id_sets(
        &self,
        staged_id_sets: &std::collections::HashMap<HarnessSource, std::collections::BTreeSet<&str>>,
    ) -> Vec<ArchiveGap> {
        self.probes
            .iter()
            .filter_map(|probe| {
                if !probe.installed_p() {
                    return None;
                }
                let Some(source) = HarnessSource::from_id(&probe.id) else {
                    return None;
                };
                let empty: std::collections::BTreeSet<&str> = Default::default();
                let session_records = staged_id_sets.get(&source).unwrap_or(&empty);
                match probe.record_count {
                    Some(0) => None,
                    Some(recognized_sessions)
                        if session_records.len() as u64 >= recognized_sessions =>
                    {
                        None
                    }
                    Some(recognized_sessions) => Some(ArchiveGap {
                        harness_id: probe.id.clone(),
                        display_name: probe.display_name.clone(),
                        recognized_sessions: Some(recognized_sessions),
                        session_records: session_records.len(),
                    }),
                    // An existing single-file store whose schema/read failed
                    // is still not consumable by `collect`; keep its count
                    // honest rather than inventing a zero.
                    None if matches!(probe.state, ProbeState::FileTarget)
                        && session_records.is_empty() =>
                    {
                        Some(ArchiveGap {
                            harness_id: probe.id.clone(),
                            display_name: probe.display_name.clone(),
                            recognized_sessions: None,
                            session_records: session_records.len(),
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
///
/// Both counts name **distinct session ids** — the probe's recognized count
/// and the record ids collect consumes — so a difference is a real shortfall
/// of this build, never an artifact of aliased ids. The reason names what the
/// two shapes of `recognized_sessions` actually mean instead of claiming a
/// store-format refusal, which was untrue whenever the difference came from
/// id aliasing (GB-2).
pub fn format_archive_gap(gap: &ArchiveGap) -> String {
    let reason = match gap.recognized_sessions {
        Some(_) => "the scanner recognised more sessions than it produced SessionRecords for, so collect archives only the session_records side — an enumeration failure of this build, not a configuration problem and not a verdict on the harness's store format",
        None => "an installed single-file store whose sessions this build could not enumerate, so collect produced no SessionRecords for it — not a configuration problem, and there is no switch to enable it",
    };
    format!(
        "  ! harness={} identified_sessions={} session_records={} source_not_collected=true — {reason}",
        gap.harness_id,
        gap.recognized_sessions
            .map(|count| count.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        gap.session_records
    )
}

// ---------------------------------------------------------------------------
// `status --json` / `doctor --json` serialisation
// ---------------------------------------------------------------------------

/// The probe state as a stable machine-readable word (never localised prose).
pub fn probe_state_word(state: ProbeState) -> &'static str {
    match state {
        ProbeState::Scanned => "scanned",
        ProbeState::FileTarget => "file_target",
        ProbeState::Missing => "missing",
        ProbeState::Indeterminate => "indeterminate",
        ProbeState::SkipUnascertained => "skip_unascertained",
        ProbeState::SkipWrongPlatform => "skip_wrong_platform",
        ProbeState::SkipUnresolvable => "skip_unresolvable",
    }
}

/// The session-count answer for one probe, as a machine-readable tri-state.
///
/// Same vocabulary as the display code (`doctor.rs` `footprint_count_label`):
/// a number when the store was enumerated, `unknown` when "we did not look" or
/// "could not enumerate", and `not_applicable` when there is no cell for this
/// platform. `Missing` stays `known(0)` — that is a *measured* absence, the
/// one case where `0` is earned.
pub fn probe_session_count(p: &HarnessProbe) -> json_out::CountState {
    use crate::json_out::CountState;
    match p.state {
        ProbeState::Scanned | ProbeState::FileTarget => match p.record_count {
            Some(n) => CountState::known(n),
            None => {
                CountState::unknown("session count could not be enumerated (store not enumerable)")
            }
        },
        ProbeState::Missing => CountState::known(0),
        ProbeState::SkipWrongPlatform => {
            CountState::not_applicable("registry has no cell for this platform")
        }
        ProbeState::Indeterminate => {
            CountState::unknown("cannot even confirm whether the source exists")
        }
        ProbeState::SkipUnascertained => {
            CountState::unknown("confidence unascertained; scanning a guessed path is forbidden")
        }
        ProbeState::SkipUnresolvable => CountState::unknown(
            "no root to scan: template not statically resolvable, or registry id unknown \
                 in this build",
        ),
    }
}

/// The unreadable-tally answer for one probe.
///
/// Mirrors `doctor.rs` `footprint_count_detail`: a row that was *never*
/// enumerated (`record_count` is `None`) has no "unreadable" concept and is
/// `not_applicable`; a row that was enumerated but whose tally itself failed
/// is `unknown`, never a quiet zero.
pub fn probe_unreadable_count(p: &HarnessProbe) -> json_out::CountState {
    use crate::json_out::CountState;
    match (p.record_count.is_some(), p.unreadable_count) {
        (_, Some(n)) => CountState::known(n),
        (true, None) => CountState::unknown("unreadable count itself could not be counted"),
        (false, None) => CountState::not_applicable(
            "this harness was not enumerated; there is no unreadable concept to report",
        ),
    }
}

/// The unreadable *entry* tally for one probe (directory entries whose session
/// cardinality is unknown). Same rule as [`probe_unreadable_count`].
pub fn probe_unreadable_entry_count(p: &HarnessProbe) -> json_out::CountState {
    use crate::json_out::CountState;
    match (p.record_count.is_some(), p.unreadable_entry_count) {
        (_, Some(n)) => CountState::known(n),
        (true, None) => CountState::unknown("unreadable entry count itself could not be counted"),
        (false, None) => CountState::not_applicable(
            "this harness was not enumerated; there is no unreadable-entry concept to report",
        ),
    }
}

/// One probe's metadata-only serialisation: the doctor's `probes` rows are
/// exactly this, and the harness rows of a full scan report (`status
/// --json`) are this plus the record-dependent staged-session count
/// ([`probe_json_with_found_staged`]) — only a scan that still holds its
/// records can answer that question, and the doctor's report keeps probes
/// and drops records. Every count is a tri-state; none can be `0`-or-null
/// because a value was never looked at.
///
/// `unverified_roots` (W1006d) is additive in the found_staged sense: it
/// appears on every row, empty wherever a family was fully measured (or
/// fully unknown — the state already says that). Where it is non-empty the
/// sibling `session_count` is a *qualified lower bound* — the exact count
/// of the measured roots only — and a consumer must not read it as the
/// family total; the listed roots are the reason it cannot be one.
/// One [`SourceRootProbe`] row as JSON (W1009/DSK-3). Shared by `probe_json`
/// and doctor's footprint serialisation so the same row can never render two
/// different answers in the two doctor tables. Counts are tri-states like
/// every other count here: a root that was not measured is `unknown` (the row
/// also names why through `state`), `not_applicable` when the platform has
/// no such root.
pub fn source_root_row_json(row: &SourceRootProbe) -> serde_json::Value {
    use crate::json_out::CountState;
    let session_count = match row.record_count {
        Some(count) => CountState::known(count),
        None => match row.state {
            ProbeState::Missing => CountState::known(0),
            ProbeState::SkipWrongPlatform => {
                CountState::not_applicable("registry has no cell for this platform")
            }
            _ => CountState::unknown(format!(
                "this root was not measured ({})",
                probe_state_word(row.state)
            )),
        },
    };
    let total_bytes = match row.total_bytes {
        Some(bytes) => CountState::known(bytes),
        None => match row.state {
            ProbeState::Missing => CountState::not_applicable("root not present"),
            ProbeState::SkipWrongPlatform => {
                CountState::not_applicable("registry has no cell for this platform")
            }
            _ => CountState::unknown(format!(
                "this root's bytes were not measured ({})",
                probe_state_word(row.state)
            )),
        },
    };
    serde_json::json!({
        "id": row.id,
        "primary": row.primary,
        "state": probe_state_word(row.state),
        "root": row.root.as_ref().map(|root| root.display().to_string()),
        "session_count": session_count,
        "total_bytes": total_bytes,
        "note": row.note,
    })
}

pub fn probe_json(p: &HarnessProbe) -> serde_json::Value {
    serde_json::json!({
        "harness": p.id,
        "display_name": p.display_name,
        "state": probe_state_word(p.state),
        "installed": p.installed_p(),
        "root": p.root.as_ref().map(|root| root.display().to_string()),
        "unverified_roots": p
            .unverified_roots
            .iter()
            .map(|root| serde_json::json!({
                "root": root.id,
                "state": probe_state_word(root.state),
            }))
            .collect::<Vec<_>>(),
        // W1009/DSK-3: additive in the same sense as `unverified_roots` —
        // present on every row, empty wherever the family is not a
        // primary-plus-roots shape. A consumer summing `source_roots`'
        // counts where it is non-empty reads a *lower bound*, and must
        // consult `unverified_roots` before reading the aggregate above as
        // a total.
        "source_roots": p
            .source_roots
            .iter()
            .map(source_root_row_json)
            .collect::<Vec<_>>(),
        "session_count": probe_session_count(p),
        "unreadable_count": probe_unreadable_count(p),
        "unreadable_entry_count": probe_unreadable_entry_count(p),
    })
}

/// The staged-session answer for one probe (GC-3), for a scan that still
/// holds its records: how many DISTINCT session ids those records carry for
/// this harness — the unit `collect` stages, one session dir per id.
///
/// Neither existing count states that unit. `session_count` is the declared
/// store's own row count (a store session must stay a store session), and
/// `total_sessions` counts `SessionRecord`s — and since W982 a harness can
/// legitimately emit several records under one id (a grok `session_docs`
/// row and its native `chat_history.jsonl` share their id deliberately, so
/// one conversation stays one conversation), so records are not the staged
/// unit either. The verify scripts needed that third number and had to
/// parse collect's human stdout for it; this is where the product states
/// it instead.
///
/// The tri-state reasons mirror [`probe_session_count`] where the shapes
/// coincide, with one deliberate difference: a positive count here is
/// `known` whatever the probe's state, because the question is not "what
/// does the source hold" but "what will collect archive from this scan" —
/// and the emitted records are the complete answer to that. Only the zero
/// cases keep the honest `unknown`: a store the scan handed no records for,
/// and a harness that was never looked at, must not read as a measured
/// empty.
pub fn probe_found_staged(p: &HarnessProbe, staged_ids: u64) -> json_out::CountState {
    use crate::json_out::CountState;
    if staged_ids > 0 {
        return CountState::known(staged_ids);
    }
    match p.state {
        ProbeState::Scanned | ProbeState::FileTarget => match p.record_count {
            Some(0) => CountState::known(0),
            // The store enumerates sessions but the scan produced no
            // records for them, so what collect will be handed — and stage —
            // is unproven, never a zero.
            Some(_) => CountState::unknown(
                "the store enumerates sessions but the scan produced no session \
                 records, so what collect will stage is unproven",
            ),
            None => {
                CountState::unknown("session count could not be enumerated (store not enumerable)")
            }
        },
        // A root that measured absent hands collect nothing, exactly as in
        // `probe_session_count`: the one case where 0 is earned.
        ProbeState::Missing => CountState::known(0),
        ProbeState::SkipWrongPlatform => {
            CountState::not_applicable("registry has no cell for this platform")
        }
        ProbeState::Indeterminate => {
            CountState::unknown("cannot even confirm whether the source exists")
        }
        ProbeState::SkipUnascertained => {
            CountState::unknown("confidence unascertained; scanning a guessed path is forbidden")
        }
        ProbeState::SkipUnresolvable => {
            // Two ways reach this state — an unresolvable template, and a
            // registry id this build does not know (`probe_harness_single`
            // skips both before any root resolves) — the why names both.
            CountState::unknown(
                "no root to scan: template not statically resolvable, or registry id unknown \
                 in this build",
            )
        }
    }
}

/// [`probe_json`] plus the `found_staged` tri-state — the per-harness count
/// of distinct staged session ids ([`probe_found_staged`]). Only a scan that
/// still holds its records can answer it, which is why the plain
/// [`probe_json`] (the doctor's metadata-only shape) does not carry the key:
/// a `doctor --json` probe row has no records to count, and writing
/// `unknown` there would state an answer its contract never asks for.
pub fn probe_json_with_found_staged(
    p: &HarnessProbe,
    found_staged: &json_out::CountState,
) -> serde_json::Value {
    let mut value = probe_json(p);
    // Every existing field stays byte-identical; the staged-session count is
    // purely additive, so consumers that read the old keys see no change.
    value["found_staged"] = serde_json::to_value(found_staged)
        .expect("CountState is a plain tagged enum and always serializes");
    value
}

/// The `scanner` object of `status --json`, for a scan that succeeded. Every
/// aggregate count is `known` here because a finished `ScanReport` is a
/// measured answer; the "could not scan at all" shape is built by the caller
/// as `{"kind":"failed","why":...}` instead.
pub fn scan_report_json(report: &ScanReport) -> serde_json::Value {
    use crate::json_out::CountState;
    let compressed = report.records.iter().filter(|r| r.compressed).count() as u64;
    let unreadable_sessions: u64 = report
        .probes
        .iter()
        .filter_map(|p| p.unreadable_count)
        .sum();
    let unreadable_entries: u64 = report
        .probes
        .iter()
        .filter_map(|p| p.unreadable_entry_count)
        .sum();
    // B90: harnesses that *were* enumerated but whose unreadable tally could
    // not be taken — same set the human `unreadable_notice` names as unknown.
    let unreadable_uncounted: u64 = report
        .probes
        .iter()
        .filter(|p| p.record_count.is_some() && p.unreadable_count.is_none())
        .count() as u64;
    // The distinct-id sets per source, grouped once, feed both per-harness
    // consumers below: each probe's `found_staged` and the
    // `archive_gap_harnesses` count. Without the shared grouping each
    // consumer would rebuild one id set per probe — a full traversal of
    // the report's records per probe.
    let staged_id_sets = report.staged_id_sets_per_source();
    let harnesses: Vec<_> = report
        .probes
        .iter()
        .map(|p| {
            // Distinct session ids among the records this harness emitted:
            // collect stages one dir per id, so this is the staged-session
            // count.
            let staged_ids = match HarnessSource::from_id(&p.id) {
                Some(source) => staged_id_sets
                    .get(&source)
                    .map_or(0, |ids| ids.len() as u64),
                // reason: a harness id the scan dispatch cannot map to a
                // source is never scanned, so it cannot have emitted
                // records — a measured empty, and the probe's own state
                // still decides the tri-state below.
                None => 0,
            };
            probe_json_with_found_staged(p, &probe_found_staged(p, staged_ids))
        })
        .collect();
    serde_json::json!({
        "kind": "ok",
        "total_sessions": CountState::known(report.records.len() as u64),
        "compressed_sessions": CountState::known(compressed),
        "missing_roots": CountState::known(report.missing_roots.len() as u64),
        "indeterminate_roots": CountState::known(report.indeterminate_roots.len() as u64),
        "archive_gap_harnesses": CountState::known(
            report
                .archive_gaps_with_id_sets(&staged_id_sets)
                .len() as u64,
        ),
        "unreadable_sessions": CountState::known(unreadable_sessions),
        "unreadable_entries": CountState::known(unreadable_entries),
        "unreadable_uncounted_harnesses": CountState::known(unreadable_uncounted),
        "harnesses": harnesses,
    })
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

/// Walk every registry harness that applies to this platform and collect
/// metadata-only records.
///
/// The registry uses the precedence documented by [`load_registry_from_repo`].
/// An explicit override that is missing or unparseable **errors** — no silent
/// fallback. Any harness whose cell is confidence `unascertained` is skipped;
/// `community-claim-unverified` cells are scanned but the probe is flagged
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

impl HarnessRegistry {
    /// The surface dimension's declared value vocabulary: the union of every
    /// harness's `dimensions.surfaces` (ADR-059 D1, DSK-5).
    ///
    /// This is what a user's `--surface` value is checked against, and it is
    /// read from the same registry the capture side records provenance from,
    /// so the word a query may ask for cannot drift from the word a source
    /// root may record. A value a harness's own capture does not re-record
    /// per session (claude-code's primary CLI library records no surface at
    /// all) is still a legal **value**: the vocabulary declares what the
    /// dimension can mean, not what every session carries — a session with no
    /// observation answers "unevaluated" under the filter, never "no" (see
    /// `crate::selector`).
    pub fn surface_vocabulary(&self) -> BTreeSet<String> {
        self.harnesses
            .iter()
            .flat_map(|harness| harness.dimensions.surfaces.iter().cloned())
            .collect()
    }
}

/// The current registry's surface vocabulary, memoised for this process
/// (DSK-5).
///
/// `search`, `export` and the dashboard all validate a requested surface
/// against the same declared set; the dashboard re-resolves the selector for
/// **every request**, so the registry is read once per process rather than
/// once per page. The registry is read through the same precedence
/// [`load_registry_from_repo`] documents — explicit override, repository
/// file, embedded copy — and a failed read stays a failed read on every
/// later call: memoising an error would be a way to report a broken
/// override only once and guess afterwards.
pub fn surface_vocabulary() -> io::Result<&'static BTreeSet<String>> {
    static VOCABULARY: std::sync::OnceLock<io::Result<BTreeSet<String>>> =
        std::sync::OnceLock::new();
    let loaded = VOCABULARY
        .get_or_init(|| load_registry_from_repo().map(|registry| registry.surface_vocabulary()));
    loaded.as_ref().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("the harness registry's surface vocabulary could not be read: {error}"),
        )
    })
}

/// Resolve `search`'s shared filter flags, validating a requested surface
/// value against the registry's declared surface vocabulary (DSK-5).
///
/// The registry is consulted **only when a surface was actually requested**:
/// a query that names no surface must behave exactly as before — all
/// surfaces are in scope by default — and must not fail because a registry
/// copy could not be read. With a surface named, an unreadable registry is
/// still a usage error rather than a search that quietly skipped the check:
/// the query asks about a dimension nobody could verify, and guessing there
/// is the "unknown recorded as empty" failure this repo forbids.
///
/// Callers print the message and exit `2`, the same way they treat
/// [`crate::selector::UsageError`] from the plain resolve — the arguments
/// cannot be turned into a verifiable filter at all.
pub fn resolve_selector_flags(
    filters: &crate::selector::SelectorArgs,
) -> Result<crate::selector::Resolved, crate::selector::UsageError> {
    let Some(surface) = filters.surface.as_deref() else {
        // No surface named: nothing to validate, the registry is not read.
        return filters.resolve();
    };
    let vocabulary = surface_vocabulary().map_err(|error| {
        crate::selector::UsageError(format!(
            "the requested `--surface {surface}` could not be checked against the harness \
             registry, so the query is refused rather than answered unverified: {error}"
        ))
    })?;
    filters.resolve_validated(vocabulary)
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

/// How many times [`scan_with_machine`] has run in this process. Test-only:
/// the collect-once-reuse path asserts a quiet run-once pass performs exactly
/// one registry scan, and this is the counter that proves it.
#[doc(hidden)]
pub static SCAN_WITH_MACHINE_CALLS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Read [`SCAN_WITH_MACHINE_CALLS`]. Test-only.
#[doc(hidden)]
pub fn scan_with_machine_calls() -> usize {
    SCAN_WITH_MACHINE_CALLS.load(std::sync::atomic::Ordering::SeqCst)
}

/// Reset [`SCAN_WITH_MACHINE_CALLS`]. Test-only.
#[doc(hidden)]
pub fn reset_scan_with_machine_calls() {
    SCAN_WITH_MACHINE_CALLS.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// Registry-driven scan with a caller-selected machine partition. Archive
/// commands use this after resolving their explicit `--machine` or the local
/// hostname, so a missing local hostname cannot block the explicit route.
pub fn scan_with_machine(config: &Config, machine: &str) -> io::Result<ScanReport> {
    SCAN_WITH_MACHINE_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let registry = load_registry_from_repo()?;
    scan_with_registry_and_machine(config, &registry, machine)
}

/// Registry-driven scan with an already-loaded registry and explicit machine
/// identity. Useful to callers that need both a custom registry and a stable
/// machine identifier without consulting the local hostname.
pub fn scan_with_registry_and_machine(
    config: &Config,
    registry: &HarnessRegistry,
    machine: &str,
) -> io::Result<ScanReport> {
    let platform = current_platform();
    let mut report = ScanReport::default();

    for h in &registry.harnesses {
        let mut probe = probe_harness(config, h, platform, &machine, &mut report);
        if h.id == "hermes-agent" {
            // The legacy compatibility roots follow the same precedence the
            // primary probe applies to its own root ("0." in `probe_harness`):
            // a root the user stated — `[harness_roots]` in the config, or the
            // cell's env override — is the harness's whole source, so the
            // `~`-templates must not reach around it into the machine's real
            // home. A platform with no registry cell is not looked at at all,
            // for the same reason the primary probe is not. Either way the
            // missing legacy records are a stated policy in the probe's note,
            // never a measurement of the user's home.
            let cell = h.paths.cell_for(platform);
            let user_stated_root = config.explicit_harness_root(&h.id).is_some()
                || cell.and_then(root_from_env_override).is_some();
            if user_stated_root {
                probe.note.push_str(
                    "; legacy compatibility roots not scanned: the harness root is user-stated",
                );
            } else if cell.is_some() {
                match scan_hermes_legacy(h, machine, &report.records) {
                    Ok(records) => {
                        let count = records.len() as u64;
                        let legacy_bytes =
                            records.iter().map(|record| record.byte_size).sum::<u64>();
                        probe.bytes = probe.bytes.map(|bytes| bytes.saturating_add(legacy_bytes));
                        probe
                            .recognized_files
                            .extend(records.iter().map(|record| record.absolute_path.clone()));
                        report.records.extend(records);
                        if let Some(value) = probe.record_count.as_mut() {
                            *value += count;
                        }
                        if let Some(value) = probe.candidate_count.as_mut() {
                            *value += count;
                        }
                        probe.note.push_str(&format!(
                            "; legacy JSON/JSONL sessions={count} after ID deduplication"
                        ));
                    }
                    Err(path) => report.indeterminate_roots.push(path),
                }
            }
        }
        report.probes.push(probe);
    }

    // Machine logs are discovered last and kept out of `records` entirely
    // (ADR-053 D4): what is scanned here is the harness's per-machine input
    // log, a different product kind from a session transcript. The session
    // loop above never sees it, so no session count, coverage number or
    // `machine_recall` bucket can move because a machine log exists.
    let (machine_logs, unlooked, indeterminate) = scan_machine_logs(registry, platform);
    report.machine_logs_unlooked = unlooked.len();
    report.machine_logs_unlooked_reasons = unlooked;
    report.machine_logs = machine_logs;
    report.machine_logs_indeterminate = indeterminate;
    Ok(report)
}

/// The persistence root the grok-bot probe would read, resolved exactly the
/// way the scan pass resolves it — the same precedence `probe_harness`
/// applies (the registry's env override first, then the config's
/// `[harness_roots]` root, then the registry template), through the same
/// helpers, so a caller needing that read outside a full scan (the
/// backfill pass's preflight) could not drift from the scan's own path.
///
/// `None` = this build would not scan grok-bot here (no registry cell for
/// this platform, or a template this build refuses to guess-scan).
pub fn grok_bot_persistence_root(config: &Config) -> Option<PathBuf> {
    let registry = load_registry_from_repo().ok()?;
    let harness = registry.harnesses.iter().find(|h| h.id == "grok-bot")?;
    let cell = harness.paths.cell_for(current_platform())?;
    if let Some((root, _)) = root_from_env_override(cell) {
        return Some(root);
    }
    if let Some(explicit_root) = config.explicit_harness_root("grok-bot").map(expand_tilde) {
        return Some(explicit_root);
    }
    if !Confidence::classify(&cell.confidence).scan_allowed() {
        // A guessed path the registry itself refuses to guess: honor the
        // same gate the scan pass honors.
        return None;
    }
    static_prefix_root(&cell.template).map(|(root, _)| root)
}

/// Where a declared machine log resolves to, or why it was not looked at.
enum MachineLogResolution {
    /// The declared file path on this platform.
    Path(PathBuf),
    /// Not looked at, with the fixed metadata reason (never a path).
    Unlooked(&'static str),
}

/// Resolve one declared machine log for `platform`.
///
/// The three reasons a declared log is *not* looked at are the scanner's
/// existing rules applied to a file source: no cell for this platform, a cell
/// whose confidence is `unascertained` (the registry's own "nobody has
/// measured this" marker), and a template that does not resolve to a concrete
/// file. A machine log is one file, so a template that carries a placeholder
/// (`<…>`) or names a directory is not a machine log and is not guessed at.
fn resolve_machine_log(entry: &RegistryMachineLog, platform: &str) -> MachineLogResolution {
    if entry.role != "machine-log" {
        return MachineLogResolution::Unlooked("role is not machine-log");
    }
    if !safe_log_component(&entry.id) {
        return MachineLogResolution::Unlooked("log id is not a usable path component");
    }
    let Some(cell) = entry.paths.cell_for(platform) else {
        return MachineLogResolution::Unlooked("no registry cell for this platform");
    };
    if cell.confidence == "unascertained" {
        return MachineLogResolution::Unlooked("registry cell confidence is unascertained");
    }
    // The env override names the harness's config base, which the platform
    // template's own leading directory stands for (`~/.claude`). The file name
    // is what the template contributes inside it.
    let override_variable = cell
        .env_override
        .as_deref()
        .or(entry.env_override.as_deref());
    if let Some(variable) = override_variable {
        if let Some(value) = env::var_os(variable).filter(|value| !value.is_empty()) {
            let Some(name) = template_file_name(&cell.template) else {
                return MachineLogResolution::Unlooked(
                    "template does not name a file to place under the override",
                );
            };
            return MachineLogResolution::Path(PathBuf::from(value).join(name));
        }
    }
    match static_prefix_root(&cell.template) {
        Some((path, true)) => MachineLogResolution::Path(path),
        Some((_, false)) => {
            MachineLogResolution::Unlooked("template names a directory, not a file")
        }
        None => MachineLogResolution::Unlooked("template is not resolvable on this platform"),
    }
}

/// Last component of a platform template, i.e. the file's own name. `None`
/// for a template that ends in a separator or carries a placeholder.
fn template_file_name(template: &str) -> Option<String> {
    if template.contains('<') {
        return None;
    }
    let name = template
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()?;
    if name.is_empty() || name.starts_with('.') {
        return None;
    }
    Some(name.to_string())
}

/// A log id is used as one path component of the stage namespace, so anything
/// that could address a different component (a separator, `..`, an empty
/// name) is refused rather than sanitized — a sanitized id would silently
/// seal a different log's content into a directory that already means
/// something else.
fn safe_log_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Walk every declared machine log and hand back the ones that exist, plus the
/// two "not read" tallies. Nothing here reads a byte of any file.
fn scan_machine_logs(
    registry: &HarnessRegistry,
    platform: &str,
) -> (Vec<MachineLogRecord>, Vec<&'static str>, usize) {
    let mut records = Vec::new();
    let mut unlooked: Vec<&'static str> = Vec::new();
    let mut indeterminate = 0usize;
    for harness in &registry.harnesses {
        for entry in &harness.machine_logs {
            let path = match resolve_machine_log(entry, platform) {
                MachineLogResolution::Path(path) => path,
                MachineLogResolution::Unlooked(reason) => {
                    unlooked.push(reason);
                    continue;
                }
            };
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => records.push(MachineLogRecord {
                    harness: harness.id.clone(),
                    log_id: entry.id.clone(),
                    absolute_path: path,
                    byte_size: metadata.len(),
                }),
                // Measured absence: the declared file is genuinely not here.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                // A directory where a file is declared, or a stat we could not
                // take: the log may hold anything, so this is not "absent".
                Ok(_) | Err(_) => indeterminate += 1,
            }
        }
    }
    (records, unlooked, indeterminate)
}

fn scan_hermes_legacy(
    harness: &RegistryHarness,
    machine: &str,
    existing: &[SessionRecord],
) -> Result<Vec<SessionRecord>, PathBuf> {
    let home = crate::config::home_dir();
    let mut records = Vec::new();
    for template in &harness.legacy_roots {
        let Some((root, _)) = static_prefix_root(template) else {
            continue;
        };
        let root = if template.starts_with("~/") {
            home.join(&template[2..])
        } else {
            root
        };
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return Err(root),
        };
        for entry in entries {
            let entry = entry.map_err(|_| root.clone())?;
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|extension| extension.to_str())
                // reason: A non-UTF-8 extension cannot match either supported suffix.
                .unwrap_or_default();
            if !matches!(ext, "json" | "jsonl") {
                continue;
            }
            let metadata = fs::metadata(&path).map_err(|_| path.clone())?;
            if !metadata.is_file() {
                continue;
            }
            let native_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| path.clone())?
                .to_string();
            if native_id.is_empty() {
                continue;
            }
            let id = crate::id::SessionIdentity {
                source_short: "hermes-agent",
                machine: machine.to_string(),
                native_id,
            }
            .id();
            if existing.iter().any(|record| record.id == id)
                || records.iter().any(|record: &SessionRecord| record.id == id)
            {
                continue;
            }
            let Ok(mtime) = metadata.modified() else {
                return Err(path);
            };
            records.push(SessionRecord {
                id,
                absolute_path: path,
                byte_size: metadata.len(),
                mtime,
                source: HarnessSource::HermesAgent,
                compressed: false,
                sqlite_layout: None,
                provenance: Default::default(),
                session_dir: None,
                movable_root: false,
            });
        }
    }
    Ok(records)
}

fn probe_harness(
    config: &Config,
    h: &RegistryHarness,
    platform: &str,
    machine: &str,
    report: &mut ScanReport,
) -> HarnessProbe {
    if h.source_roots.is_empty() {
        return probe_harness_single(config, h, platform, machine, report);
    }

    let mut subprobes = Vec::new();
    // The registry root id each subprobe stands for, parallel to
    // `subprobes`: the canonical harness id for the primary store, the
    // root's own id for every additional source root. The unverified
    // marking below names roots by these ids — a path can carry a
    // username, an id is ours to name.
    let mut subprobe_root_ids = Vec::new();
    // W1009/DSK-3: one `SourceRootProbe` row per subprobe. Filled only in
    // the primary-plus-roots shape (the D6 scope the aggregate rules use);
    // an all-roots family keeps its aggregate-only surface.
    let mut source_rows: Vec<SourceRootProbe> = Vec::new();
    let mut total = ScanReport::default();
    // DSK-1 (plan C, ADR-059 draft): an entry may declare BOTH its primary
    // store (the `paths` cells) and additional `source_roots` — the desktop
    // Cowork root beside the claude-code CLI store, the archived-rollouts
    // root beside codex's active ones. The primary store stays an implicit
    // root of the same entry, first in the aggregate, so its records, its
    // canonical `harness_roots` override and its display root keep the
    // meaning they had before any root was declared. An entry with no
    // platform cell for its primary store (antigravity, codebuff) keeps the
    // all-roots shape it has always had — nothing below changes for it.
    let primary_probeable = h.paths.cell_for(platform).is_some();
    if primary_probeable {
        let mut one = h.clone();
        one.source_roots.clear();
        let mut local = ScanReport::default();
        let probe = probe_harness_single(config, &one, platform, machine, &mut local);
        source_rows.push(source_root_row(h.id.clone(), true, &probe, &local));
        total.records.extend(local.records);
        total.missing_roots.extend(local.missing_roots);
        total.indeterminate_roots.extend(local.indeterminate_roots);
        subprobes.push(probe);
        subprobe_root_ids.push(h.id.clone());
    }
    let canonical_override = config.explicit_harness_root(&h.id).is_some();
    let root_keys: Vec<_> = h
        .source_roots
        .iter()
        .map(|root| format!("{}:{}", h.id, root.id))
        .collect();
    let has_all_root_overrides = root_keys
        .iter()
        .all(|key| config.explicit_harness_root(key).is_some());
    // The canonical override is ambiguous only when it cannot mean one root:
    // a harness whose primary `paths` cell it names is not ambiguous (it is
    // the primary's override, probed above); a harness whose cells are all
    // source_roots has no primary for it to name, so the historical refusal
    // stands there.
    if canonical_override && !has_all_root_overrides && !primary_probeable {
        return HarnessProbe {
            id: h.id.clone(),
            display_name: h.display_name.clone(),
            root: None,
            confidence: Confidence::Unascertained,
            state: ProbeState::Indeterminate,
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
            note: "a canonical harness_roots override is ambiguous across multiple source roots; declare every root with a quoted harness_roots key `<harness>:<source-root>`".to_string(),
        };
    }
    // An entry-wide env override (ADR-054 D7): a root marked
    // `override_scope: "entry"` declares a variable whose value replaces the
    // harness's whole base directory — channel suffix included — for every
    // build of the harness. While that variable is set, the entry is exactly
    // one root: the override root, reading the override value. The entry's
    // `default_only` roots are suppressed here in that state (scanning the
    // defaults as well would read the same directory twice under two root
    // ids, or read locations the harness no longer writes), with one
    // exception: a root the user stated explicitly themselves, the same
    // user-stated-path rule that outranks templates and confidence gates
    // below — the variable has nothing to say about a path the user wrote
    // down. With no such override in force the entry behaves as before:
    // every declared root scans.
    let entry_override_in_force = h.source_roots.iter().any(|root| {
        root.override_scope.as_deref() == Some("entry")
            && root
                .paths
                .cell_for(platform)
                .and_then(|cell| cell.env_override.as_deref())
                .is_some_and(|variable| {
                    env::var_os(variable).is_some_and(|value| !value.is_empty())
                })
    });
    for root in &h.source_roots {
        let Some(cell) = root.paths.cell_for(platform) else {
            continue;
        };
        if entry_override_in_force
            && root.default_only
            && config
                .explicit_harness_root(&format!("{}:{}", h.id, root.id))
                .is_none()
        {
            continue;
        }
        let mut one = h.clone();
        one.paths = root.paths.clone();
        one.source_roots.clear();
        let mut root_config = config.clone();
        // The canonical `<harness>` harness_roots key names the entry's
        // primary store, which was probed above — a source root never
        // inherits it, or every additional root would re-walk the primary's
        // override instead of its own declared template. For an all-roots
        // entry (antigravity, codebuff) this is the same removal the
        // `has_all_root_overrides` case always did, and any other canonical
        // override was refused by the guard above, so nothing changes for
        // those entries; a per-root override is re-inserted below by its
        // own key.
        root_config.harness_roots.remove(&h.id);
        if has_all_root_overrides {
            root_config.harness_roots.remove(&h.id);
        }
        // Partial per-root overrides are useful when the remaining roots
        // should use their registered defaults. A canonical override is
        // rejected above unless every root has an explicit replacement.
        if let Some(path) = config.explicit_harness_root(&format!("{}:{}", h.id, root.id)) {
            root_config.harness_roots.remove(&h.id);
            root_config
                .harness_roots
                .insert(h.id.clone(), path.to_string());
        }
        let mut local = ScanReport::default();
        let probe = probe_harness_single(&root_config, &one, platform, machine, &mut local);
        if primary_probeable {
            source_rows.push(source_root_row(root.id.clone(), false, &probe, &local));
        }
        for record in &mut local.records {
            // The cell's declared observation joins whatever the record's own
            // discovery already observed — a Kimi Code session's workspace
            // segment and indexed working directory — instead of replacing
            // it: every dimension here is a distinct observation of one
            // session, not a single authoritative answer.
            crate::provenance::merge(&mut record.provenance, &root.provenance);
            // A movable root's records carry the movability fact the
            // collector needs to treat a same-id discovery at a new path
            // as the same file rather than a fresh source (W1007).
            record.movable_root = root.movable;
            // The registry root that produced this record, for control
            // records that must name a root instead of a path (ADR-054 D6:
            // a path can carry a username; the root id is ours to name).
            // A record without session-dir facts is not a codebuff record
            // and has no root-bearing control stream to feed, so the stamp
            // only exists where the facts do.
            if let Some(facts) = record.session_dir.as_mut() {
                facts.root_id = Some(root.id.clone());
            }
        }
        total.records.extend(local.records);
        total.missing_roots.extend(local.missing_roots);
        total.indeterminate_roots.extend(local.indeterminate_roots);
        // Cells without a supported source format are treated the same way as
        // any other declared root, including their honest unknown state.
        let _ = cell;
        subprobes.push(probe);
        subprobe_root_ids.push(root.id.clone());
    }
    let any_unknown = subprobes.iter().any(HarnessProbe::not_probed_p)
        || subprobes
            .iter()
            .any(|p| p.state == ProbeState::Indeterminate);
    let any_scanned = subprobes.iter().any(HarnessProbe::installed_p);
    let all_missing =
        !subprobes.is_empty() && subprobes.iter().all(|p| p.state == ProbeState::Missing);
    let unique_records: std::collections::BTreeSet<_> = total
        .records
        .iter()
        .map(|record| record.id.as_str())
        .collect();
    let known_count = unique_records.len() as u64;
    // A root that was looked for and found ABSENT answers a known zero for
    // the aggregate once another root of the same family was actually
    // seen — but only in the primary-plus-roots shape DSK-1 introduced
    // (ADR-059 draft D6, scoped in the W1006c review): the family is
    // anchored by a PRIMARY store the entry itself declares, so the absent
    // sibling's contribution is a measured nothing, and a machine without
    // Claude Desktop must not have its known claude-code CLI count dimmed
    // to an unknown. An all-roots family (codebuff's channels, antigravity's
    // surfaces) declares no primary store, so nothing anchors it and the
    // pre-DSK-1 rule stands there: a missing sibling keeps that family's
    // combined total unknown, exactly as before this batch. The doctor's
    // footprint/probe consistency contract forced the distinction for the
    // primary shape, because the record-derived footprint could count what
    // the aggregate refused to claim. One limit guards against a semantic
    // zero: a family whose roots were ALL absent still reports unknown
    // (an absent harness is not an empty one).
    //
    // W1006d — the same review's D6 correction, one step further: a root
    // that was never LOOKED at (unascertained, unresolvable,
    // indeterminate) must not erase a measured total either. That was the
    // D6 side effect the draft shipped with — on Windows the desktop
    // Cowork cell is `unascertained`, so an installed CLI's measured count
    // turned into a blanket unknown — and the rule the W1006c paragraph
    // above still kept ("a subprobe that never looked keeps the whole
    // total unknown") is exactly what the review rejected. In the anchored
    // (primary-plus-roots) shape the measured roots' count is instead
    // reported as a QUALIFIED LOWER BOUND: the exact count of what the
    // scan measured, its unverified siblings named beside it. Never an
    // unknown — that erases a measurement already taken; never a plain
    // number — that silently claims the family total. The lower bound is
    // marked, in `unverified_roots`, everywhere the aggregate is shown.
    // When NO root of the family was measured, nothing anchors a lower
    // bound and the total stays unknown, exactly as before.
    let absent_sibling_is_zero = primary_probeable;
    let family_measured = subprobes.iter().any(HarnessProbe::probed_p);
    let qualified_lower_bound = primary_probeable && any_unknown && family_measured;
    let unverified_roots: Vec<UnverifiedRoot> = if qualified_lower_bound {
        subprobes
            .iter()
            .zip(&subprobe_root_ids)
            .filter(|(probe, _)| probe.not_probed_p())
            .map(|(probe, id)| UnverifiedRoot {
                id: id.clone(),
                state: probe.state,
            })
            .collect()
    } else {
        Vec::new()
    };
    let unreadable = subprobes.iter().try_fold(0u64, |sum, probe| {
        if absent_sibling_is_zero && probe.state == ProbeState::Missing {
            return Some(sum);
        }
        // An unverified root contributes nothing to a lower bound for the
        // same reason an absent one contributes a zero: it never looked,
        // so it holds no unreadable tally to add — the marking, not the
        // fold, is where its existence is stated. Outside the lower-bound
        // shape the fold still refuses, keeping the whole total unknown.
        if qualified_lower_bound && probe.not_probed_p() {
            return Some(sum);
        }
        sum.checked_add(probe.unreadable_count?)
    });
    let unreadable_entries = subprobes.iter().try_fold(0u64, |sum, probe| {
        if absent_sibling_is_zero && probe.state == ProbeState::Missing {
            return Some(sum);
        }
        if qualified_lower_bound && probe.not_probed_p() {
            return Some(sum);
        }
        sum.checked_add(probe.unreadable_entry_count?)
    });
    let counts_complete = !subprobes.is_empty()
        && (any_scanned || qualified_lower_bound)
        && subprobes.iter().all(|p| {
            p.record_count.is_some()
                || (absent_sibling_is_zero && p.state == ProbeState::Missing)
                || (qualified_lower_bound && p.not_probed_p())
        })
        && unreadable.is_some()
        && unreadable_entries.is_some();
    let bytes_complete = !subprobes.is_empty()
        && subprobes
            .iter()
            .all(|p| p.bytes.is_some() || (qualified_lower_bound && p.not_probed_p()));
    let confidence = subprobes
        .iter()
        .map(|p| p.confidence)
        // A combined harness can claim no stronger evidence than its weakest
        // included source root.
        .max_by_key(|c| match c {
            Confidence::Confirmed => 0,
            Confidence::OfficialDocs => 1,
            Confidence::Measured => 2,
            Confidence::CommunityClaim => 3,
            Confidence::Unascertained => 4,
        })
        .unwrap_or(Confidence::Unascertained);
    let all_unascertained = !subprobes.is_empty()
        && subprobes
            .iter()
            .all(|probe| probe.state == ProbeState::SkipUnascertained);
    let state = if all_unascertained {
        // Every root was skipped by policy before any filesystem probe. Keep
        // that precise reason; mixed measured/unknown roots remain indeterminate.
        ProbeState::SkipUnascertained
    } else if any_unknown && !qualified_lower_bound {
        ProbeState::Indeterminate
    } else if all_missing || (qualified_lower_bound && !any_scanned) {
        // The W1006d lower-bound shape with no scanned root: every MEASURED
        // root of the family is absent (that is what `probed_p` can mean
        // here once the unverified roots are subtracted), so the state that
        // tells the truth about presence is still `Missing` — while
        // `unverified_roots` and the note carry the "≥" qualification.
        ProbeState::Missing
    } else if any_scanned {
        ProbeState::Scanned
    } else {
        ProbeState::SkipWrongPlatform
    };
    // Whether the aggregate may claim a count at all: fully measured, or the
    // qualified-lower-bound shape where the claim is the measured roots'
    // count carrying its own qualification.
    let counts_claimable = counts_complete && (!any_unknown || qualified_lower_bound);
    let roots: Vec<_> = subprobes.iter().filter_map(|p| p.root.clone()).collect();
    report.records.extend(total.records);
    report.missing_roots.extend(total.missing_roots);
    report.indeterminate_roots.extend(total.indeterminate_roots);
    let note = if qualified_lower_bound {
        format!(
            "{} declared source roots checked; total is a lower bound — {} unverified \
             source root(s): {}",
            subprobes.len(),
            unverified_roots.len(),
            unverified_roots
                .iter()
                .map(|root| format!("{} ({})", root.id, probe_state_word(root.state)))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else if any_unknown {
        "one or more declared source roots could not be fully checked; total is unknown".to_string()
    } else if all_missing {
        "all declared source roots are absent".to_string()
    } else {
        format!("{} declared source roots checked", roots.len())
    };
    HarnessProbe {
        id: h.id.clone(),
        display_name: h.display_name.clone(),
        root: roots.first().cloned(),
        confidence,
        state,
        unverified_roots,
        source_roots: source_rows,
        record_count: counts_claimable.then_some(known_count),
        // An unreadable source root has no native id to deduplicate against
        // the other roots, so candidate totals remain unknown in that case.
        candidate_count: (counts_claimable
            && unreadable == Some(0)
            && unreadable_entries == Some(0))
        .then_some(known_count),
        unreadable_count: counts_claimable.then_some(unreadable).flatten(),
        unreadable_entry_count: counts_claimable.then_some(unreadable_entries).flatten(),
        earliest: subprobes.iter().filter_map(|p| p.earliest).min(),
        latest: subprobes.iter().filter_map(|p| p.latest).max(),
        bytes: (bytes_complete && (!any_unknown || qualified_lower_bound))
            .then(|| subprobes.iter().filter_map(|p| p.bytes).sum()),
        recognized_files: subprobes
            .into_iter()
            .flat_map(|p| p.recognized_files)
            .collect(),
        note,
    }
}

/// Build one [`SourceRootProbe`] row from a root's own subprobe and the same
/// scan-local record set the aggregate folded away (W1009/DSK-3).
///
/// Only the subprobe state decides which tallies may be claimed: a `Scanned`
/// root earned its byte census from the very records the walk handed out —
/// the same standard the family footprint's byte total uses in doctor's
/// `coverage_from_records`, with `Some(0)` as that standard's measured empty
/// for a scanned root that holds nothing — and any other state has no byte
/// measurement at all (`None`, never a guessed zero). Count, unreadable and
/// entry tallies, note and root path all come from the subprobe verbatim, so
/// a root's own row and the family aggregate can never disagree about what
/// that root's walk saw.
fn source_root_row(
    id: String,
    primary: bool,
    probe: &HarnessProbe,
    local: &ScanReport,
) -> SourceRootProbe {
    let total_bytes = match probe.state {
        ProbeState::Scanned => Some(local.records.iter().map(|record| record.byte_size).sum()),
        // A single-file root carries no per-record bytes in its subprobe's
        // local report; the store-level figure the probe measured stands in.
        // No primary-plus-roots family has a file-target root today; this
        // arm exists so a future one cannot silently read as `None`-bytes
        // for a store the probe did measure.
        ProbeState::FileTarget => probe.bytes,
        _ => None,
    };
    SourceRootProbe {
        id,
        primary,
        state: probe.state,
        root: probe.root.clone(),
        record_count: probe.record_count,
        unreadable_count: probe.unreadable_count,
        unreadable_entry_count: probe.unreadable_entry_count,
        total_bytes,
        note: probe.note.clone(),
    }
}

fn probe_harness_single(
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
    };

    // grok-bot is the one harness whose source cannot exist off macOS: the
    // Grok Bot desktop app has no Linux or Windows build, and the registry
    // records a macOS cell only (the `no {platform} registry entry` note the
    // generic path below would produce loses the more specific reason, so it
    // is spelled here instead). The state word is B82's own for "the registry
    // has no cell for this platform": `SkipWrongPlatform`, whose count is
    // `not_applicable` and which must not be counted as an unlooked harness —
    // there is nothing on this machine to have missed, which is a different
    // claim from "did not look". `SkipUnresolvable` would word it as the
    // latter and answer "unknown" — wrong for an app this platform cannot
    // install. The platform gate outranks a configured root, so a grok-bot
    // path written into the config on Linux or Windows does not turn the
    // reader on there either.
    if h.id == "grok-bot" && platform != "macos" {
        return HarnessProbe {
            state: ProbeState::SkipWrongPlatform,
            note: "Grok Bot local persistence reader is supported on macOS only".to_string(),
            ..base
        };
    }

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
                // B82: this fell through to `base`'s `SkipUnresolvable`, so
                // "the registry lists no cell for this platform" was reported
                // with the same word as "the template would not resolve" —
                // and, once the doctor stopped printing 0 for everything, it
                // would have been counted as a look we failed to make. There
                // is nothing here to have missed: `SkipWrongPlatform` is the
                // state that says so, and `N/A` is its column.
                return HarnessProbe {
                    state: ProbeState::SkipWrongPlatform,
                    note: format!("no {platform} registry entry"),
                    ..base
                };
            }
        },
    };

    // 2. Confidence decides whether we scan a *guessed* path at all. A
    // configured path is not a guess, so `unascertained` does not gate it:
    // refusing to count a store the user pointed us at, opened, and enumerated
    // would be its own false claim ("I could not determine"), just in the other
    // direction.
    let confidence = Confidence::classify(&cell.confidence);
    if !confidence.scan_allowed() && explicit_root.is_none() {
        return HarnessProbe {
            confidence,
            state: ProbeState::SkipUnascertained,
            note: format!("confidence=unascertained; skipped (scanning a guessed path = false negatives/positives)"),
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
                        note: format!(
                            "template cannot be statically resolved to a root path: {}",
                            cell.template
                        ),
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
            note: format!("registry id unknown in this build: {}", h.id),
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
            // reason: empty string when not using environment variable override, no extra note needed
            .unwrap_or_default()
    };

    if h.id == "openclaw" {
        return probe_openclaw_agents(base, confidence, root, env_note, source, machine, report);
    }

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
                    bytes: Some(md.len()),
                    record_count: None,
                    candidate_count: None,
                    unreadable_count: None,
                    note: "single-file store: existence and size probed only".to_string(),
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
                    let mut info = match &spec {
                        Some(spec) => crate::sqlite_probe::probe_sqlite_store_with(&root, spec),
                        None => probe_sqlite_store(&root),
                    };
                    if h.id == "hermes-agent" {
                        if let Err(actual) = crate::sqlite_probe::validate_hermes_schema(&root) {
                            info.sessions = SqliteSessionProbe::SchemaMismatch { actual };
                        }
                    }
                    probe.bytes = info.total_bytes;
                    let expected = spec
                        .as_ref()
                        .map(|s| format!("{} table({})", s.table, s.required_columns.join(", ")))
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
                                "{env_note}SQLite read-only enumeration of {expected} (bytes include .db+-wal+-shm); before filter {candidate_count} / after {count} sessions"
                            );
                            if h.id == "opencode" {
                                match enumerate_opencode_sessions(&root) {
                                    Ok(rows) => {
                                        let records: Vec<SessionRecord> = rows
                                            .into_iter()
                                            .filter_map(|row| {
                                                // An unrepresentable SQLite time is an
                                                // unreadable row, not a license to label it 1970.
                                                let mtime =
                                                    sqlite_millis_to_system_time(row.time_updated)?;
                                                Some(SessionRecord {
                                                    id: crate::id::SessionIdentity {
                                                        source_short: source.short(),
                                                        machine: machine.to_string(),
                                                        native_id: row.id,
                                                    }
                                                    .id(),
                                                    absolute_path: root.clone(),
                                                    byte_size: 0,
                                                    mtime,
                                                    source,
                                                    compressed: false,
                                                    sqlite_layout: Some(
                                                        SqliteSessionLayout::OpenCode,
                                                    ),
                                                    provenance: Default::default(),
                                                    session_dir: None,
                                                    movable_root: false,
                                                })
                                            })
                                            .collect();
                                        let record_count = records.len();
                                        report.records.extend(records);
                                        probe.unreadable_count = enumeration_gap(
                                            count,
                                            record_count,
                                            info.unreadable_count,
                                        );
                                        probe
                                            .note
                                            .push_str(&format!("; SessionRecord={record_count}"));
                                    }
                                    Err(error) => {
                                        // A failed enumeration is not zero
                                        // sessions: every known row is now a
                                        // session we cannot hand out.
                                        probe.unreadable_count =
                                            enumeration_gap(count, 0, info.unreadable_count);
                                        probe.note.push_str(&format!(
                                            "; SessionRecord enumeration failed ({error})"
                                        ));
                                    }
                                }
                            } else if h.id == "hermes-agent" || h.id == "grok" {
                                if let Some(spec) = spec.as_ref() {
                                    match enumerate_sqlite_sessions(&root, spec) {
                                        Ok(rows) => {
                                            let layout = if h.id == "hermes-agent" {
                                                SqliteSessionLayout::HermesAgent
                                            } else {
                                                SqliteSessionLayout::Grok
                                            };
                                            let records = if h.id == "hermes-agent" {
                                                hermes_records_from_rows(
                                                    rows, &root, source, machine,
                                                )
                                            } else {
                                                sqlite_records_from_rows(
                                                    rows, &root, source, machine, layout,
                                                )
                                            };
                                            let record_count = records.len();
                                            let (session_dirs, walk_unreadable) =
                                                grok_session_dirs(&root);
                                            let usage = grok_usage_records(
                                                &session_dirs,
                                                &records,
                                                machine,
                                            );
                                            let history =
                                                grok_history_records(&session_dirs, machine);
                                            report.records.extend(records);
                                            let usage_count = usage.records.len();
                                            report.records.extend(usage.records);
                                            let history_count = history.records.len();
                                            report.records.extend(history.records);
                                            probe.unreadable_count = enumeration_gap(
                                                count,
                                                record_count,
                                                info.unreadable_count,
                                            );
                                            if walk_unreadable > 0 {
                                                probe.unreadable_entry_count =
                                                    Some(walk_unreadable);
                                            }
                                            if usage.unreadable_files > 0 {
                                                probe.note.push_str(&format!(
                                                    "; usage.json unreadable={}",
                                                    usage.unreadable_files
                                                ));
                                            }
                                            if history.unreadable_files > 0 {
                                                probe.note.push_str(&format!(
                                                    "; chat_history.jsonl unreadable={}",
                                                    history.unreadable_files
                                                ));
                                            }
                                            probe.note.push_str(&format!(
                                                "; SessionRecord={record_count}; usage files={usage_count}; history files={history_count}"
                                            ));
                                        }
                                        Err(error) => {
                                            probe.unreadable_count =
                                                enumeration_gap(count, 0, info.unreadable_count);
                                            probe.note.push_str(&format!(
                                                "; SessionRecord enumeration failed ({error})"
                                            ));
                                        }
                                    }
                                }
                            } else if h.id == "zed" {
                                if let Some(spec) = spec.as_ref() {
                                    match enumerate_sqlite_sessions(&root, spec) {
                                        Ok(rows) => {
                                            let records = sqlite_records_from_rows(
                                                rows,
                                                &root,
                                                source,
                                                machine,
                                                SqliteSessionLayout::Zed,
                                            );
                                            let record_count = records.len();
                                            report.records.extend(records);
                                            probe.unreadable_count = enumeration_gap(
                                                count,
                                                record_count,
                                                info.unreadable_count,
                                            );
                                            probe.note.push_str(&format!(
                                                "; SessionRecord={record_count}"
                                            ));
                                        }
                                        Err(error) => {
                                            probe.unreadable_count =
                                                enumeration_gap(count, 0, info.unreadable_count);
                                            probe.note.push_str(&format!(
                                                "; SessionRecord enumeration failed ({error})"
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        SqliteSessionProbe::SchemaMismatch { actual } => {
                            probe.note = format!(
                                "detected SQLite but schema not recognized (expected {expected}, got {actual}) — not 0, it is un-enumerable"
                            );
                        }
                        SqliteSessionProbe::ReadFailed { error } => {
                            probe.note = format!(
                                "detected SQLite but read-only enumeration failed ({error})"
                            );
                        }
                    }
                }
                probe
            }
            // B82: this arm used to be a bare `_ => Missing("single file does not exist")`,
            // so a `metadata` call that failed on permissions, and a path that
            // exists but is a directory, both reported the store as absent.
            // Only `NotFound` is an absence we measured.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                report.missing_roots.push(root.clone());
                HarnessProbe {
                    root: Some(root),
                    confidence,
                    state: ProbeState::Missing,
                    bytes: None,
                    record_count: None,
                    note: "single-file store not present".to_string(),
                    ..base
                }
            }
            Err(e) => {
                report.indeterminate_roots.push(root.clone());
                HarnessProbe {
                    root: Some(root),
                    confidence,
                    state: ProbeState::Indeterminate,
                    bytes: None,
                    record_count: None,
                    note: format!("path unreadable, existence unknown ({e}) — not 'absent'"),
                    ..base
                }
            }
            Ok(md) => {
                report.indeterminate_roots.push(root.clone());
                HarnessProbe {
                    root: Some(root),
                    confidence,
                    state: ProbeState::Indeterminate,
                    bytes: None,
                    record_count: None,
                    note: format!(
                        "path exists but is not a file ({}) — session count unknown, not 0",
                        path_kind(&md)
                    ),
                    ..base
                }
            }
        }
    } else if !root.is_dir() {
        // Same split for directory roots: `is_dir()` is false both for a path
        // that is not there and for one we are not allowed to stat.
        match fs::metadata(&root) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                report.missing_roots.push(root.clone());
                HarnessProbe {
                    root: Some(root),
                    confidence,
                    state: ProbeState::Missing,
                    record_count: None,
                    candidate_count: None,
                    unreadable_count: None,
                    note: "directory not present".to_string(),
                    ..base
                }
            }
            Err(e) => {
                report.indeterminate_roots.push(root.clone());
                HarnessProbe {
                    root: Some(root),
                    confidence,
                    state: ProbeState::Indeterminate,
                    record_count: None,
                    candidate_count: None,
                    unreadable_count: None,
                    note: format!("directory unreadable, existence unknown ({e}) — not 'absent'"),
                    ..base
                }
            }
            Ok(md) => {
                report.indeterminate_roots.push(root.clone());
                HarnessProbe {
                    root: Some(root),
                    confidence,
                    state: ProbeState::Indeterminate,
                    record_count: None,
                    candidate_count: None,
                    unreadable_count: None,
                    note: format!(
                        "path exists but is not a directory ({}) — session count unknown, not 0",
                        path_kind(&md)
                    ),
                    ..base
                }
            }
        }
    } else {
        if h.id == "grok-bot" {
            return probe_grok_bot_harness(
                base, confidence, root, env_note, source, machine, report,
            );
        }
        if h.id == "workbuddy" {
            return probe_workbuddy_harness(
                base, confidence, root, env_note, source, machine, report,
            );
        }
        if h.id == "codebuddy" {
            return probe_codebuddy_harness(
                base, confidence, root, env_note, source, machine, report,
            );
        }
        if h.id == "github-copilot-cli" {
            return probe_github_copilot_cli_harness(
                base, confidence, root, env_note, source, machine, report,
            );
        }
        let recs = collect_records(
            &root,
            source,
            machine,
            &cell.format,
            cell.session_pattern.as_deref(),
            cell.session_dir.as_ref(),
        );
        // One archive slot addresses one session id, so the recognized count
        // is the **distinct** ids this scan produced — the same quantity the
        // multi-source-root aggregation counts as `known_count` and the
        // quantity `archive_gaps` compares it against. Until some harness
        // wrote the same filename stem in several directories, the raw file
        // count and the distinct-id count were equal; comparing raw files
        // here against collect's distinct record ids would turn pure id
        // aliasing into an "identified but unarchivable" gap (GB-2: 2848
        // files over 2840 ids read as 8 sessions this build could not
        // archive). Candidate files keep the file census either way.
        let count = recs
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len() as u64;
        let file_count = recs.records.len() as u64;
        let recognized_files = recs
            .records
            .iter()
            .map(|record| record.absolute_path.clone())
            .collect();
        report.records.extend(recs.records);
        let note = if recs.unreadable_entry_count > 0 {
            format!(
                "directory scan incomplete: {} unreadable directory entries (session count unknown)",
                recs.unreadable_entry_count
            )
        } else if recs.unreadable_count > 0 {
            format!(
                "{} unreadable (not empty — metadata could not be obtained)",
                recs.unreadable_count
            )
        } else if count == 0 {
            "directory present, but no enumerable session files".to_string()
        } else {
            String::new()
        };
        HarnessProbe {
            root: Some(root),
            confidence,
            state: ProbeState::Scanned,
            record_count: Some(count),
            candidate_count: Some(file_count + recs.unreadable_count),
            // Counted entry by entry by the directory walk: a real number.
            unreadable_count: Some(recs.unreadable_count),
            unreadable_entry_count: Some(recs.unreadable_entry_count),
            recognized_files,
            note,
            ..base
        }
    }
}

fn probe_grok_bot_harness(
    mut base: HarnessProbe,
    confidence: Confidence,
    root: PathBuf,
    env_note: String,
    source: HarnessSource,
    machine: &str,
    report: &mut ScanReport,
) -> HarnessProbe {
    match crate::grok_bot::read_from_app_support(&root) {
        Ok(agents) => {
            let mut bytes = 0u64;
            let mut recognized_files = Vec::new();
            let mut record_count = 0usize;
            let mut gap_count = 0usize;
            let mut unreadable_count = 0usize;
            for agent in agents {
                let metadata = match fs::metadata(&agent.source_path) {
                    Ok(metadata) => metadata,
                    Err(_) => {
                        unreadable_count += 1;
                        continue;
                    }
                };
                let mtime = match metadata.modified() {
                    Ok(mtime) => mtime,
                    Err(_) => {
                        unreadable_count += 1;
                        continue;
                    }
                };
                bytes = bytes.saturating_add(metadata.len());
                gap_count += agent.sequence_gaps.len();
                record_count += 1;
                recognized_files.push(agent.source_path.clone());
                report.records.push(SessionRecord {
                    id: crate::id::SessionIdentity {
                        source_short: source.short(),
                        machine: machine.to_string(),
                        native_id: agent.agent_id,
                    }
                    .id(),
                    absolute_path: agent.source_path,
                    byte_size: metadata.len(),
                    mtime,
                    source,
                    compressed: false,
                    sqlite_layout: Some(crate::models::SqliteSessionLayout::GrokBot),
                    provenance: Default::default(),
                    session_dir: None,
                    movable_root: false,
                });
            }
            base.root = Some(root);
            base.confidence = confidence;
            base.state = ProbeState::Scanned;
            base.bytes = Some(bytes);
            base.record_count = Some(record_count as u64);
            base.candidate_count = Some(record_count as u64);
            base.unreadable_count = Some(unreadable_count as u64);
            base.recognized_files = recognized_files;
            base.note = format!(
                "{env_note}local persistence replicas; partial replica data, sequence gap metadata present (observed gaps={gap_count})"
            );
            base
        }
        Err(error) => {
            report.indeterminate_roots.push(root.clone());
            base.root = Some(root);
            base.confidence = confidence;
            base.state = ProbeState::Indeterminate;
            base.unreadable_count = Some(1);
            base.note = format!("{env_note}local replica read failed; count unknown ({error})");
            base
        }
    }
}

/// One `*.jsonl` transcript file below a cbc-family `projects/` root: the
/// session id — the file name's stem, measured equal to the store key on
/// every installation this family was measured from — together with the
/// file's own metadata. Never its bytes; a scan reads names, sizes and
/// times only.
struct CbcTranscript {
    path: PathBuf,
    mtime: SystemTime,
    byte_size: u64,
}

/// Which layouts one cbc-family `projects/` root walks (W1047): both
/// products in the family are builds of the same `cbc` CLI core, and the
/// layout is the one fact their transcript roots do not share.
enum CbcRootLayout {
    /// WorkBuddy desktop app (W1046, measured): transcripts live only at
    /// `projects/<project-slug>/<session-id>.jsonl`. A flat
    /// `projects/<id>.jsonl` file is measured and skipped, and a matched
    /// name whose metadata could not be read stays an unreadable directory
    /// entry — the sqlite store beside the root remains the source of
    /// recognized truth, so a session is joined through its store row and
    /// an unreadable file never invents one.
    SlugOnly,
    /// CodeBuddy CLI (W1047, measured): the flat `projects/<session-id>.jsonl`
    /// layout and the `projects/<project-slug>/<session-id>.jsonl` layout
    /// both exist on one installation. The CLI keeps no sqlite session
    /// store, so a matched file name is the only record that a session
    /// exists at all — a name the layout accepts whose metadata could not
    /// be read is therefore a recognized session this build cannot hand
    /// out, kept in [`CbcTranscriptWalk::recognized_unrecorded`] so the
    /// recognized-vs-archived difference becomes an archive gap and collect
    /// reports PARTIAL exit 3, never 0 — the same rule the desktop probe
    /// gets from its store rows. The `history` stem is the per-machine
    /// input log the CLI writes beside `projects/` (declared as a machine
    /// log on the registry entry), never a session.
    FlatAndSlug,
}

/// What one cbc-family transcript walk observed, all metadata only.
struct CbcTranscriptWalk {
    /// The readable transcripts, keyed by session id (the file name's
    /// stem), the newest copy per id.
    transcripts: BTreeMap<String, CbcTranscript>,
    /// Transcripts seen directly under the root — the flat CLI layout.
    /// Always 0 for [`CbcRootLayout::SlugOnly`], which skips those files.
    flat_files: u64,
    /// Transcripts seen under a `<project-slug>/` directory.
    slug_files: u64,
    /// Copies of an id already kept at a newer (mtime, path).
    stale_duplicates: u64,
    /// Directory entries whose type, listing or metadata could not be
    /// inspected: each is an untaken tally entry, never a zero.
    unreadable_entries: u64,
    /// Matched names whose metadata could not be read — recognized
    /// sessions with no record, the exit-3 arm of the CLI layout. Always 0
    /// for [`CbcRootLayout::SlugOnly`].
    recognized_unrecorded: u64,
}

/// Walk a cbc-family transcript root, `~/<product>/projects/`. Only
/// `*.jsonl` files the layout accepts are transcripts; the neighbours the
/// core writes beside them are excluded by name wherever they appear —
/// `*.file-rollback.ndjson` (a rollback journal) and any subdirectory
/// inside a project slug (the `tool-results/` derivative cache the
/// renderer reads) are never sessions, never archived, and never counted as
/// candidates. Every other subdirectory or non-jsonl file is measured and
/// skipped, the same three-state rule every scanner walk keeps.
///
/// The same session id can appear under more than one project slug — the
/// app moves projects and community tooling reports the stale copy that
/// stays behind — and one archive session answers for one id, so the walk
/// keeps one copy per id, the newest by (mtime, path), and returns how many
/// stale duplicates it skipped. Keeping one copy is a scan-side decision
/// only: the bytes of every observed copy stay on disk untouched, and
/// appends a skipped copy would later gain are picked up the day its mtime
/// makes it the newest.
fn cbc_transcript_walk(root: &Path, layout: CbcRootLayout) -> CbcTranscriptWalk {
    let accept_flat = matches!(layout, CbcRootLayout::FlatAndSlug);
    // On the CLI layout the accepted name itself is the record that a
    // session exists, so an unreadable one is a recognized session without
    // a record; on the desktop layout the store stays the source of
    // recognized truth, so it is a plain unreadable entry. The same flag is
    // also where the CLI layout's `history` machine-log exclusion applies
    // (`cbc_accept_file`), so it is named for the layout it carries.
    let cli_layout = accept_flat;
    let mut walk = CbcTranscriptWalk {
        transcripts: BTreeMap::new(),
        flat_files: 0,
        slug_files: 0,
        stale_duplicates: 0,
        unreadable_entries: 0,
        recognized_unrecorded: 0,
    };
    let root_entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        // The root exists (the caller checked) but cannot be listed: every
        // candidate below it is one untaken tally entry, not zero.
        Err(_) => {
            walk.unreadable_entries += 1;
            return walk;
        }
    };
    for entry in root_entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                walk.unreadable_entries += 1;
                continue;
            }
        };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {}
            // A flat file is a transcript only on the CLI layout; the
            // desktop app writes its `*.jsonl` sessions one level deeper,
            // so on the slug-only walk a root-level file — whatever its
            // name — is measured and skipped.
            Ok(_) => {
                if accept_flat {
                    cbc_accept_file(&entry, &mut walk, true, cli_layout);
                }
                continue;
            }
            Err(_) => {
                walk.unreadable_entries += 1;
                continue;
            }
        }
        // A directory is a project slug: its own entries are the slug
        // layout's transcripts, and anything deeper is unmeasured.
        let files = match fs::read_dir(entry.path()) {
            Ok(entries) => entries,
            Err(_) => {
                walk.unreadable_entries += 1;
                continue;
            }
        };
        for file in files {
            let file = match file {
                Ok(entry) => entry,
                Err(_) => {
                    walk.unreadable_entries += 1;
                    continue;
                }
            };
            match file.file_type() {
                // `tool-results/` and any other subdirectory never hold
                // transcripts: the glob is `*.jsonl` files only.
                Ok(kind) if kind.is_dir() => continue,
                Ok(_) => {}
                Err(_) => {
                    walk.unreadable_entries += 1;
                    continue;
                }
            }
            cbc_accept_file(&file, &mut walk, false, cli_layout);
        }
    }
    walk
}

/// Classify one `*.jsonl`-named file the walk accepted by type and record
/// it — a readable one under its stem, an unreadable one as the layout's
/// unknown arm. Only cbc-family exclusion rules apply: the rollback
/// journal suffix, and on the CLI layout the `history` input-log stem.
fn cbc_accept_file(
    file: &fs::DirEntry,
    walk: &mut CbcTranscriptWalk,
    flat: bool,
    cli_layout: bool,
) {
    let name = file.file_name().to_string_lossy().into_owned();
    // A rollback journal beside the transcripts: excluded by name.
    if name.ends_with(".file-rollback.ndjson") {
        return;
    }
    if !name.ends_with(".jsonl") {
        return;
    }
    let stem = match Path::new(&name).file_stem().and_then(|stem| stem.to_str()) {
        Some(stem) => stem.to_string(),
        None => return,
    };
    // The CLI's per-typed-line input log, which the registry entry
    // declares as a machine log: its lines are prompts the person typed on
    // this machine, not a conversation this walk can ever archive, and its
    // stem is never a session id — at either level the CLI layout accepts
    // files, flat and under a project slug, because both are inside
    // `projects/` and the registry note says exactly that. The desktop
    // entry measured no file of this name, so the slug-only walk keeps
    // accepting it (nothing there writes one into `projects/`).
    if cli_layout && stem == "history" {
        return;
    }
    let (byte_size, mtime) = match file.metadata() {
        Ok(metadata) => match (metadata.len(), metadata.modified()) {
            (byte_size, Ok(mtime)) => (byte_size, mtime),
            _ => return cbc_unreadable_name(walk, cli_layout),
        },
        Err(_) => return cbc_unreadable_name(walk, cli_layout),
    };
    let keep_newest = match walk.transcripts.get(&stem) {
        Some(existing) => (existing.mtime, &existing.path) < (mtime, &file.path()),
        None => true,
    };
    if walk.transcripts.contains_key(&stem) {
        walk.stale_duplicates += 1;
    }
    if keep_newest {
        walk.transcripts.insert(
            stem,
            CbcTranscript {
                path: file.path(),
                mtime,
                byte_size,
            },
        );
    }
    if flat {
        walk.flat_files += 1;
    } else {
        walk.slug_files += 1;
    }
}

/// A file the layout accepted by name whose metadata could not be taken.
/// One counted observation either way, never a zero: on the CLI layout the
/// name itself is the session's record, so the count joins the recognized
/// set and the recognized-vs-archived difference becomes collect's
/// PARTIAL exit-3 gap; on the desktop layout an unreadable entry stays an
/// entry, because the store beside the root — not the file name — carries
/// the recognized truth there.
fn cbc_unreadable_name(walk: &mut CbcTranscriptWalk, cli_layout: bool) {
    if cli_layout {
        walk.recognized_unrecorded += 1;
    } else {
        walk.unreadable_entries += 1;
    }
}

/// The WorkBuddy desktop app's transcript walk (W1046), the slug-only
/// layout: `~/.workbuddy/projects/<project-slug>/<session-id>.jsonl`.
fn workbuddy_transcripts(root: &Path) -> CbcTranscriptWalk {
    cbc_transcript_walk(root, CbcRootLayout::SlugOnly)
}

/// The CodeBuddy CLI's transcript walk (W1047), the both-layouts walk:
/// flat `~/.codebuddy/projects/<session-id>.jsonl` and
/// `~/.codebuddy/projects/<project-slug>/<session-id>.jsonl` — measured
/// to coexist on one installation.
fn codebuddy_transcripts(root: &Path) -> CbcTranscriptWalk {
    cbc_transcript_walk(root, CbcRootLayout::FlatAndSlug)
}

/// The canonical cbc-family session id for one native id (the store row's
/// `id`, or the transcript file name's stem when no store says anything
/// about it).
fn cbc_session_id(source: HarnessSource, machine: &str, native_id: &str) -> String {
    crate::id::SessionIdentity {
        source_short: source.short(),
        machine: machine.to_string(),
        native_id: native_id.to_string(),
    }
    .id()
}

/// Push one transcript file record — a plain append-only jsonl session whose
/// bytes the ordinary file reader collects under the shared id.
fn push_cbc_transcript_record(
    records: &mut Vec<SessionRecord>,
    source: HarnessSource,
    machine: &str,
    native_id: &str,
    transcript: &CbcTranscript,
) {
    records.push(SessionRecord {
        id: cbc_session_id(source, machine, native_id),
        absolute_path: transcript.path.clone(),
        byte_size: transcript.byte_size,
        mtime: transcript.mtime,
        source,
        compressed: false,
        sqlite_layout: None,
        provenance: Default::default(),
        session_dir: None,
        movable_root: false,
    });
}

/// Widen an (earliest, latest) span with one more observed time.
fn widen_time_span(
    earliest: &mut Option<SystemTime>,
    latest: &mut Option<SystemTime>,
    time: SystemTime,
) {
    *earliest = Some(match *earliest {
        Some(earliest) => earliest.min(time),
        None => time,
    });
    *latest = Some(match *latest {
        Some(latest) => latest.max(time),
        None => time,
    });
}

/// The counters one `fold_workbuddy_store_rows` pass produced. Every field
/// is a counted recognition or a counted miss, never an inferred one.
struct WorkBuddyRowFold {
    /// Rows that exported a tombstone record.
    tombstoned: u64,
    /// Live rows this machine holds no transcript bytes for.
    without_transcript: u64,
    /// Rows the store cannot name; recognized, never guessed an id.
    unnamed_rows: u64,
    /// Rows whose time could not be taken at all — the row's own columns
    /// unreadable as a time and the store file's own mtime absent — whether
    /// live with a transcript or already deleted: recognized, never archived,
    /// never stamped with a fake epoch, and never silently filed under the
    /// live-rows-without-a-transcript count (W1047f L3).
    untimeable_rows: u64,
    /// The recognized set: distinct row ids plus transcript ids plus unnamed
    /// rows, exactly what `record_count` earns as a measurement.
    known_sessions: u64,
    /// Transcripts whose id no store row carries; archived as orphans.
    orphan_transcripts: u64,
}

/// The probe-note segment that names untimed rows. Kept beside the fold so
/// the honest line is pinned with the bucket it reports (W1047f L3): a row
/// dropped for lack of a readable time is its own named count, never a
/// `without_transcript` mislabel — a deleted row with its transcript still on
/// disk is not a live row without a local transcript, and the printed note
/// must not say it is.
fn workbuddy_untimed_rows_note(untimed: u64) -> String {
    if untimed == 0 {
        String::new()
    } else {
        format!(
            "; {untimed} store row(s) without a readable time — recognized, not archived, not 0"
        )
    }
}

/// Fold the store's enumerated `sessions` rows against the transcript walk
/// (W1046): push the row/tombstone and transcript records, count the orphans,
/// and hand back the probe's tallies. Split from `probe_workbuddy_harness`
/// (W1047f L3) for the same reason the codebuddy accounting is split from its
/// probe: the arm a filesystem cannot be made to produce on POSIX — a row
/// with neither a readable row time nor a readable store-file time, because a
/// unix stat of a live sqlite file always supplies a modified time — is worth
/// pinning directly against the fold's own accounting.
#[allow(clippy::too_many_arguments)]
fn fold_workbuddy_store_rows(
    rows: Vec<crate::sqlite_probe::WorkBuddySessionRow>,
    transcripts: &BTreeMap<String, CbcTranscript>,
    db: &std::path::Path,
    db_modified: Option<SystemTime>,
    source: HarnessSource,
    machine: &str,
    records: &mut Vec<SessionRecord>,
    earliest: &mut Option<SystemTime>,
    latest: &mut Option<SystemTime>,
) -> WorkBuddyRowFold {
    let mut tombstoned = 0u64;
    let mut without_transcript = 0u64;
    let mut unnamed_rows = 0u64;
    let mut untimeable_rows = 0u64;
    let mut known_ids: BTreeSet<String> = BTreeSet::new();
    for row in rows {
        if row.id.is_empty() {
            // A row the store cannot name is still a recognized
            // session; it is never guessed an id from neighbours.
            unnamed_rows += 1;
            continue;
        }
        known_ids.insert(row.id.clone());
        let transcript = transcripts.get(&row.id);
        if !row.deleted && transcript.is_none() {
            // The unknown arm: a live store row this machine
            // holds no transcript bytes for. No record is
            // emitted; the recognized-vs-archived difference is
            // what `collect` turns into PARTIAL exit 3, never 0.
            without_transcript += 1;
            continue;
        }
        // An unrepresentable row time falls back to the store
        // file's own mtime, the hermes precedent; a row with
        // neither is not archived — a named gap, never a fake
        // epoch, and never counted as a live row without a
        // transcript (W1047f L3): it is the row's time that is
        // missing, live or deleted, transcript or none.
        let mtime = match row
            .time_ms
            .and_then(sqlite_millis_to_system_time)
            .or(db_modified)
        {
            Some(mtime) => mtime,
            None => {
                untimeable_rows += 1;
                continue;
            }
        };
        records.push(SessionRecord {
            id: cbc_session_id(source, machine, &row.id),
            absolute_path: db.to_path_buf(),
            byte_size: 0,
            mtime,
            source,
            compressed: false,
            sqlite_layout: Some(SqliteSessionLayout::WorkBuddy),
            provenance: Default::default(),
            session_dir: None,
            movable_root: false,
        });
        if row.deleted {
            tombstoned += 1;
        }
        if let Some(transcript) = transcript {
            push_cbc_transcript_record(records, source, machine, &row.id, transcript);
        }
        widen_time_span(earliest, latest, mtime);
    }
    let mut orphan_transcripts = 0u64;
    for (stem, transcript) in transcripts {
        if !known_ids.contains(stem) {
            known_ids.insert(stem.clone());
            orphan_transcripts += 1;
            push_cbc_transcript_record(records, source, machine, stem, transcript);
        }
    }
    let known_sessions = (known_ids.len() as u64).saturating_add(unnamed_rows);
    WorkBuddyRowFold {
        tombstoned,
        without_transcript,
        unnamed_rows,
        untimeable_rows,
        known_sessions,
        orphan_transcripts,
    }
}

/// Probe the WorkBuddy dual store (W1046): sessions enumerate from the
/// `sessions` table of the `workbuddy.db` that sits beside the registry
/// root, and each row joins to its `*.jsonl` transcript below the root. One
/// session id is carried by up to two `SessionRecord`s, exactly the grok
/// row/history shape: a virtual sqlite record whose `absolute_path` is the
/// db (its cursor is keyed `path\0id`, so it never collides with the
/// transcript file's own byte-offset cursor), plus the transcript file
/// record that the ordinary append-only jsonl reader collects.
///
/// * a row with `deleted_at IS NOT NULL` is a tombstone: the virtual record
///   exports a bare tombstone line, and a still-present transcript keeps
///   being archived beside it — both facts stay, neither invented, neither
///   dropped;
/// * a live row without a matching transcript jsonl (a transport this
///   machine holds no local transcript for, or the rows whose jsonl a
///   project move left behind) is a recognized session with no archivable
///   bytes: no record is emitted for it, the recognized-vs-archived
///   difference the report computes becomes an archive gap, and `collect`
///   reports PARTIAL exit 3 — never 0;
/// * a transcript whose id has no store row archives under its own id: the
///   union of store rows and transcript ids is the recognized set, so such
///   an orphan is recognized *and* archived, never a gap and never hidden
///   behind the rows that are.
fn probe_workbuddy_harness(
    mut base: HarnessProbe,
    confidence: Confidence,
    root: PathBuf,
    env_note: String,
    source: HarnessSource,
    machine: &str,
    report: &mut ScanReport,
) -> HarnessProbe {
    base.root = Some(root.clone());
    base.confidence = confidence;
    // The slug-only walk never recognizes a name it could not read (the
    // store carries the recognized truth), so this probe's
    // recognized-vs-archived arithmetic runs on the store rows below.
    let walk = workbuddy_transcripts(&root);
    let transcripts = walk.transcripts;
    let walk_unreadable = walk.unreadable_entries;
    let stale_duplicates = walk.stale_duplicates;
    let transcript_count = transcripts.len() as u64;
    let recognized_files = transcripts
        .values()
        .map(|transcript| transcript.path.clone())
        .collect::<Vec<_>>();
    let transcript_bytes = transcripts
        .values()
        .map(|transcript| transcript.byte_size)
        .sum::<u64>();
    let mut earliest = None;
    let mut latest = None;
    for transcript in transcripts.values() {
        widen_time_span(&mut earliest, &mut latest, transcript.mtime);
    }
    let db = match root.parent().map(|parent| parent.join("workbuddy.db")) {
        Some(db) => db,
        None => {
            base.state = ProbeState::Indeterminate;
            base.note = format!(
                "{env_note}the registry root has no parent directory, so the sibling store is unresolvable — session count unknown, not 0"
            );
            return base;
        }
    };
    match fs::metadata(&db) {
        // Measured absence of the sqlite half: the transcripts below the
        // root still archive, each under its own file-name id.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // A walk that listed nothing and could not read an entry proves
            // no clean zero: without the store, the transcripts are the
            // only recognized set, and an untakeable tally stays unknown.
            let walk_clean = !(transcripts.is_empty() && walk_unreadable > 0);
            for (stem, transcript) in &transcripts {
                push_cbc_transcript_record(&mut report.records, source, machine, stem, transcript);
            }
            let walk_note = if walk_unreadable > 0 {
                format!(
                    "; transcript walk incomplete: {walk_unreadable} unreadable directory entries"
                )
            } else {
                String::new()
            };
            base.state = ProbeState::Scanned;
            base.record_count = walk_clean.then_some(transcript_count);
            base.candidate_count = walk_clean.then_some(transcript_count);
            base.unreadable_count = walk_clean.then_some(0);
            base.unreadable_entry_count = Some(walk_unreadable);
            base.bytes = Some(transcript_bytes);
            base.earliest = earliest;
            base.latest = latest;
            base.recognized_files = recognized_files;
            base.note = format!(
                "{env_note}workbuddy.db is not present beside the projects root; {transcript_count} transcript file(s) archived from files only — the store's session count is unknown, not 0{walk_note}"
            );
            base
        }
        Err(e) => {
            report.indeterminate_roots.push(db.clone());
            base.state = ProbeState::Indeterminate;
            base.note = format!(
                "{env_note}workbuddy.db unreadable, existence unknown ({e}) — not 'absent'"
            );
            base
        }
        Ok(md) if !md.is_file() => {
            report.indeterminate_roots.push(db.clone());
            base.state = ProbeState::Indeterminate;
            base.note = format!(
                "{env_note}workbuddy.db exists but is not a file ({}) — session count unknown, not 0",
                path_kind(&md)
            );
            base
        }
        Ok(_) => match crate::sqlite_probe::enumerate_workbuddy_sessions(&db) {
            Err(error) => {
                // The transcripts still archive; whether the store holds
                // sessions beyond them is exactly what this error leaves
                // unread, so the session count is unknown and stays so.
                for (stem, transcript) in &transcripts {
                    push_cbc_transcript_record(
                        &mut report.records,
                        source,
                        machine,
                        stem,
                        transcript,
                    );
                }
                base.state = ProbeState::Scanned;
                base.unreadable_entry_count = Some(walk_unreadable);
                base.bytes = Some(transcript_bytes);
                base.earliest = earliest;
                base.latest = latest;
                base.recognized_files = recognized_files;
                base.note = format!(
                    "{env_note}WorkBuddy store enumeration failed: {error}; session count unknown, not 0; {transcript_count} transcript file(s) archived from files only"
                );
                base
            }
            Ok(rows) => {
                let row_count = rows.len() as u64;
                let db_modified = fs::metadata(&db)
                    .and_then(|metadata| metadata.modified())
                    .ok();
                let db_bytes = crate::sqlite_probe::sqlite_store_bytes(&db).ok();
                let fold = fold_workbuddy_store_rows(
                    rows,
                    &transcripts,
                    &db,
                    db_modified,
                    source,
                    machine,
                    &mut report.records,
                    &mut earliest,
                    &mut latest,
                );
                let walk_note = if walk_unreadable > 0 {
                    format!(
                        "; transcript walk incomplete: {walk_unreadable} unreadable directory entries"
                    )
                } else {
                    String::new()
                };
                base.state = ProbeState::Scanned;
                base.record_count = Some(fold.known_sessions);
                base.candidate_count = Some(fold.known_sessions);
                base.unreadable_count =
                    Some(fold.without_transcript + fold.unnamed_rows + fold.untimeable_rows);
                base.unreadable_entry_count = Some(walk_unreadable);
                // reason: the sqlite byte tally already failed open once
                // here; the transcripts' bytes are measured, the store's are
                // a live-only reading and zero is not an honest fallback
                // for it — the probe either counted all three files or
                // reports the transcripts' half alone.
                base.bytes = db_bytes.map_or(Some(transcript_bytes), |db_bytes| {
                    Some(db_bytes + transcript_bytes)
                });
                base.earliest = earliest;
                base.latest = latest;
                base.recognized_files = recognized_files;
                base.note = format!(
                    "{env_note}WorkBuddy dual store: {row_count} session row(s) in the sqlite store (tombstoned={tombstoned}); {transcript_count} transcript file(s) (stale duplicates skipped={stale_duplicates}, without a store row={orphan_transcripts}); live rows without a local transcript jsonl={without_transcript} — recognized, not archived, not 0{untimed_note}{walk_note}",
                    tombstoned = fold.tombstoned,
                    orphan_transcripts = fold.orphan_transcripts,
                    without_transcript = fold.without_transcript,
                    untimed_note = workbuddy_untimed_rows_note(fold.untimeable_rows),
                );
                base
            }
        },
    }
}

/// Probe the CodeBuddy CLI transcript root (W1047): the CLI is the other
/// build of the same `cbc` core the WorkBuddy desktop app wraps (the
/// ruling CS-WB-2 records the same event schema on both, with the CLI the
/// one that adds `summary` and `turn-metrics` lines the W1046 reader
/// already handles), and its sessions live one whole per
/// `*.jsonl` transcript below the registry root — the flat
/// `projects/<session-id>.jsonl` layout and the
/// `projects/<slug>/<session-id>.jsonl` layout both measured on this
/// machine. There is no sqlite sessions store to read, so there is no
/// tombstone arm (the CLI keeps no deletion marker this build can observe)
/// and no row-join: each transcript archives on its own file-name stem
/// through the ordinary append-only jsonl reader.
///
/// The unknown arm is the walk itself, with the file name standing in for
/// the role the store row plays on the desktop side. A `*.jsonl` name the
/// layout accepted whose metadata could not be read is a session this
/// source recognizes and this build cannot hand out: the walk counts it,
/// no record is emitted for it, and the recognized-vs-archived difference
/// the report computes becomes an archive gap, so `collect` reports
/// PARTIAL exit 3 — never 0. Directory entries that could not be typed or
/// listed stay the same unreadable-entry tallies every scanner walk keeps,
/// and the `history` input log (a machine log this registry entry declares)
/// is excluded by name at either level this layout accepts files, flat and
/// under a project slug, never a session.
fn probe_codebuddy_harness(
    mut base: HarnessProbe,
    confidence: Confidence,
    root: PathBuf,
    env_note: String,
    source: HarnessSource,
    machine: &str,
    report: &mut ScanReport,
) -> HarnessProbe {
    base.root = Some(root.clone());
    base.confidence = confidence;
    let walk = codebuddy_transcripts(&root);
    let transcripts = &walk.transcripts;
    for (stem, transcript) in transcripts {
        push_cbc_transcript_record(&mut report.records, source, machine, stem, transcript);
    }
    codebuddy_probe_accounting(base, walk, env_note)
}

/// Assemble the CodeBuddy probe's counts and note from one walk. Split from
/// the probe because the walk's arms are worth pinning beyond what a
/// filesystem can be made to produce: on POSIX the stat of a file inside a
/// readable directory succeeds whatever the file's own mode bits say, so
/// the state the real walker hands back when an accepted name gives no
/// metadata is produced directly by the internal test instead.
fn codebuddy_probe_accounting(
    mut base: HarnessProbe,
    walk: CbcTranscriptWalk,
    env_note: String,
) -> HarnessProbe {
    let transcripts = walk.transcripts;
    let transcript_count = transcripts.len() as u64;
    let recognized_files = transcripts
        .values()
        .map(|transcript| transcript.path.clone())
        .collect::<Vec<_>>();
    let transcript_bytes = transcripts
        .values()
        .map(|transcript| transcript.byte_size)
        .sum::<u64>();
    let mut earliest = None;
    let mut latest = None;
    for transcript in transcripts.values() {
        widen_time_span(&mut earliest, &mut latest, transcript.mtime);
    }
    // The recognized set: every stem the walk accepted, readable or not.
    let known_sessions = transcript_count + walk.recognized_unrecorded;
    // A walk that listed nothing and could not read an entry — with no
    // recognized names at all — proves no clean zero: the session count
    // stays unknown rather than the measured emptiness a readable walk
    // earns.
    let walk_clean =
        !(transcripts.is_empty() && walk.recognized_unrecorded == 0 && walk.unreadable_entries > 0);
    let mut note = format!(
        "{env_note}CodeBuddy transcript store: {known_sessions} session file(s) in the flat and project-directory layouts (flat={}, under a project directory={}), no sqlite session store beside them; stale duplicates skipped={}",
        walk.flat_files, walk.slug_files, walk.stale_duplicates
    );
    if !walk_clean {
        note = format!(
            "{env_note}CodeBuddy transcript store: the walk could not read every entry below the projects root, so the session count is unknown, not 0 — nothing was recognized as a session and nothing is claimed as archivable"
        );
    } else if walk.recognized_unrecorded > 0 {
        note.push_str(&format!(
            "; {unrecorded} transcript file(s) present but unreadable — recognized, not archived, not 0",
            unrecorded = walk.recognized_unrecorded
        ));
    }
    if walk.unreadable_entries > 0 {
        note.push_str(&format!(
            "; transcript walk incomplete: {walk_unreadable} unreadable directory entries",
            walk_unreadable = walk.unreadable_entries
        ));
    }
    base.state = ProbeState::Scanned;
    base.record_count = walk_clean.then_some(known_sessions);
    base.candidate_count = walk_clean.then_some(known_sessions);
    base.unreadable_count = walk_clean.then_some(walk.recognized_unrecorded);
    base.unreadable_entry_count = Some(walk.unreadable_entries);
    base.bytes = Some(transcript_bytes);
    base.earliest = earliest;
    base.latest = latest;
    base.recognized_files = recognized_files;
    base.note = note;
    base
}

/// GitHub Copilot CLI keeps its persisted sessions in one SQLite store,
/// `session-store.db` directly under its home, and writes a per-session
/// `events.jsonl` beside them. This reader is built on the store (the upstream's
/// own documentation of `sessions`/`turns` is the schema
/// [`crate::sqlite_probe::github_copilot_cli_schema`] encodes); the per-session
/// events files are *not* enumerated, because the layout below
/// `session-state/` has never been measured and a basename-only walk would give
/// every session the same `events` stem. Session records therefore come from
/// the store's `sessions` table, one per row.
///
/// When the store is absent, the count is not silently zero: the events files
/// the CLI also writes are looked for by their exact, writer-confirmed name,
/// and finding any means sessions exist that this build cannot enumerate — an
/// unknown, carried in the note. Absent store and absent events together are a
/// measured "nothing to read".
fn probe_github_copilot_cli_harness(
    base: HarnessProbe,
    confidence: Confidence,
    root: PathBuf,
    env_note: String,
    source: HarnessSource,
    machine: &str,
    report: &mut ScanReport,
) -> HarnessProbe {
    let db = root.join("session-store.db");
    let metadata = match fs::metadata(&db) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let (events, unreadable_entries) = count_events_jsonl_files(&root);
            if events > 0 || unreadable_entries > 0 {
                let note = format!(
                    "{env_note}session-store.db not present, but {events} events.jsonl file(s) were seen under the root (with {unreadable_entries} unreadable entries); the per-session events layout is unmeasured, so the session count from them is unknown"
                );
                return HarnessProbe {
                    root: Some(root),
                    confidence,
                    state: ProbeState::Indeterminate,
                    record_count: None,
                    candidate_count: None,
                    unreadable_count: None,
                    // The events walk ran, so its tally is a counted number —
                    // `Some(0)` says "looked and none unreadable", not "unknown".
                    unreadable_entry_count: Some(unreadable_entries),
                    recognized_files: Vec::new(),
                    note,
                    ..base
                };
            }
            return HarnessProbe {
                root: Some(root),
                confidence,
                state: ProbeState::Scanned,
                record_count: Some(0),
                candidate_count: Some(0),
                unreadable_count: Some(0),
                unreadable_entry_count: Some(unreadable_entries),
                recognized_files: Vec::new(),
                note: format!(
                    "{env_note}no session-store.db and no events.jsonl under the Copilot home — no persisted sessions to read"
                ),
                ..base
            };
        }
        Err(e) => {
            report.indeterminate_roots.push(db);
            return HarnessProbe {
                root: Some(root),
                confidence,
                state: ProbeState::Indeterminate,
                record_count: None,
                candidate_count: None,
                unreadable_count: None,
                note: format!(
                    "{env_note}session-store.db unreadable, existence unknown ({e}) — not 'absent'"
                ),
                ..base
            };
        }
    };
    if !metadata.is_file() {
        report.indeterminate_roots.push(db);
        return HarnessProbe {
            root: Some(root),
            confidence,
            state: ProbeState::Indeterminate,
            record_count: None,
            candidate_count: None,
            unreadable_count: None,
            note: format!(
                "{env_note}session-store.db exists but is not a file ({}) — session count unknown, not 0",
                path_kind(&metadata)
            ),
            ..base
        };
    }
    let Ok(store_modified) = metadata.modified() else {
        report.indeterminate_roots.push(db);
        return HarnessProbe {
            root: Some(root),
            confidence,
            state: ProbeState::Indeterminate,
            record_count: None,
            candidate_count: None,
            unreadable_count: None,
            note: format!(
                "{env_note}session-store.db mtime unreadable — session records cannot be dated for staging"
            ),
            ..base
        };
    };
    let spec = crate::sqlite_probe::github_copilot_cli_schema();
    let info = crate::sqlite_probe::probe_sqlite_store_with(&db, &spec);
    let expected = "sessions table(id, cwd, repository)".to_string();
    let mut probe = HarnessProbe {
        root: Some(root),
        confidence,
        state: ProbeState::Scanned,
        recognized_files: vec![db.clone()],
        ..base
    };
    probe.bytes = info.total_bytes;
    match info.sessions {
        SqliteSessionProbe::Known {
            count,
            candidate_count,
            ..
        } => {
            let rows = match enumerate_sqlite_sessions(&db, &spec) {
                Ok(rows) => rows,
                Err(error) => {
                    probe.unreadable_count = enumeration_gap(count, 0, info.unreadable_count);
                    probe.note = format!(
                        "{env_note}SQLite read-only enumeration of {expected} (bytes include .db+-wal+-shm) failed: {error}"
                    );
                    return probe;
                }
            };
            let records = copilot_records_from_rows(rows, &db, store_modified, source, machine);
            let emitted = records.len();
            let record_count = records.len() as u64;
            report.records.extend(records);
            probe.record_count = Some(count);
            probe.candidate_count = Some(candidate_count);
            probe.unreadable_count = enumeration_gap(count, emitted, info.unreadable_count);
            probe.note = format!(
                "{env_note}SQLite read-only enumeration of {expected} (bytes include .db+-wal+-shm); before filter {candidate_count} / after {count} sessions; SessionRecord={record_count}"
            );
        }
        SqliteSessionProbe::SchemaMismatch { actual } => {
            probe.note = format!(
                "{env_note}detected SQLite but schema not recognized (expected {expected}, got {actual}) — not 0, it is un-enumerable"
            );
        }
        SqliteSessionProbe::ReadFailed { error } => {
            probe.note =
                format!("{env_note}detected SQLite but read-only enumeration failed ({error})");
        }
    }
    probe
}

/// Copilot session records from enumerated `sessions` rows. The spec declares
/// no timestamp column (none was measured on a real store), so every record
/// carries the store file's own mtime — the same fallback the Hermes reader
/// uses — and the collect-side cursor is a content hash, never a time.
fn copilot_records_from_rows(
    rows: Vec<SqliteSessionRow>,
    db: &Path,
    store_modified: SystemTime,
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
            absolute_path: db.to_path_buf(),
            byte_size: 0,
            mtime: row.mtime.unwrap_or(store_modified),
            source,
            compressed: false,
            sqlite_layout: Some(SqliteSessionLayout::GitHubCopilotCli),
            provenance: Default::default(),
            session_dir: None,
            movable_root: false,
        })
        .collect()
}

/// Count the `events.jsonl` files a Copilot home holds, by the exact name the
/// shipped CLI's own event writer uses (`<sessionStatePath>/events.jsonl`).
/// Counting by name claims nothing about which session a file belongs to: the
/// per-session layout is unmeasured, so the count is only ever reported as
/// "sessions exist that this build cannot enumerate", never as a session count.
fn count_events_jsonl_files(root: &Path) -> (u64, u64) {
    let mut events = 0u64;
    let mut unreadable_entries = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                unreadable_entries += 1;
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    unreadable_entries += 1;
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => {
                    unreadable_entries += 1;
                    continue;
                }
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() && entry.file_name() == "events.jsonl" {
                events += 1;
            }
        }
    }
    (events, unreadable_entries)
}

/// Name the kind of thing a path turned out to be, for the one message that
/// has to say "this is not what the registry declared".
fn path_kind(md: &fs::Metadata) -> &'static str {
    if md.is_dir() {
        "directory"
    } else if md.is_file() {
        "file"
    } else {
        "neither file nor directory"
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
    } else if template.contains(".kimi-code/") {
        // `KIMI_CODE_HOME` is Kimi Code's data directory, i.e. the `.kimi-code`
        // layer itself; what hangs below it is the constant `sessions/`.
        ".kimi-code/"
    } else if template.contains(".dsh/") {
        // `DSH_HOME` is DeepSeek Harness's home directory, i.e. the `.dsh`
        // layer itself; `sessions/` hangs below it. A cell that declares
        // an env override this table does not know would resolve to the template
        // instead, silently reading the default location while the registry
        // claims the override works — which is what happened here until the
        // real binary was run against a moved home.
        ".dsh/"
    } else if template.contains(".config/manicode/") {
        // `FREEBUFF_CONFIG_DIR` is Codebuff/Freebuff's config base, i.e. the
        // `.config/manicode` layer itself; nothing hangs below it in the
        // template (ADR-054 F3/F7: the value replaces the whole base,
        // channel suffix included, for every build of the harness). The
        // marker must not swallow the `-dev`/`-staging` siblings: only this
        // exact spelling matches, so a dev-channel template can never
        // resolve through another channel's override.
        ".config/manicode/"
    } else if template.contains(".copilot/") {
        // `COPILOT_HOME` replaces the whole `.copilot` layer: the CLI resolves
        // its home from that variable before joining anything below it (the
        // registry's v1.0.80 source note — "COPILOT_HOME/configDir can
        // override" — declares exactly this).
        ".copilot/"
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
        .filter_map(|row| {
            // Missing SQLite time is carried as a row gap, never as a fake epoch.
            let mtime = row.mtime?;
            Some(SessionRecord {
                id: crate::id::SessionIdentity {
                    source_short: source.short(),
                    machine: machine.to_string(),
                    native_id: row.id,
                }
                .id(),
                absolute_path: db.to_path_buf(),
                byte_size: 0,
                mtime,
                source,
                compressed: false,
                sqlite_layout: Some(layout),
                provenance: Default::default(),
                session_dir: None,
                movable_root: false,
            })
        })
        .collect()
}

/// One Grok session directory: `sessions/<working-directory>/<session-id>/`.
struct GrokSessionDir {
    path: std::path::PathBuf,
    session_id: String,
}

/// Walk the session directories that sit beside the search DB:
/// `~/.grok/sessions/<sanitized-cwd>/<session-uuid>/`. Both sidecar
/// collectors — the `usage.json` manifest and the native
/// `chat_history.jsonl` transcript — must see the very same set of session
/// directories, so the walk is shared rather than repeated: a collector that
/// walked on its own could drift from its sibling and, one day, associate a
/// sidecar with the wrong session population.
///
/// Returns the directories plus the count of directory entries the walk could
/// not read. A non-directory entry is not an unreadable one — it is measured
/// and skipped, the same three-state rule every scanner walk keeps.
fn grok_session_dirs(db: &Path) -> (Vec<GrokSessionDir>, u64) {
    let Some(root) = db.parent() else {
        return (Vec::new(), 0);
    };
    let mut dirs = Vec::new();
    let mut unreadable_entries = 0;
    let cwd_dirs = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => {
            return (Vec::new(), 1);
        }
    };
    for cwd_entry in cwd_dirs {
        let cwd_entry = match cwd_entry {
            Ok(entry) => entry,
            Err(_) => {
                unreadable_entries += 1;
                continue;
            }
        };
        match cwd_entry.file_type() {
            Ok(kind) if kind.is_dir() => {}
            Ok(_) => continue,
            Err(_) => {
                unreadable_entries += 1;
                continue;
            }
        }
        let session_dirs = match fs::read_dir(cwd_entry.path()) {
            Ok(entries) => entries,
            Err(_) => {
                unreadable_entries += 1;
                continue;
            }
        };
        for session_entry in session_dirs {
            let session_entry = match session_entry {
                Ok(entry) => entry,
                Err(_) => {
                    unreadable_entries += 1;
                    continue;
                }
            };
            match session_entry.file_type() {
                Ok(kind) if kind.is_dir() => {}
                Ok(_) => continue,
                Err(_) => {
                    unreadable_entries += 1;
                    continue;
                }
            }
            dirs.push(GrokSessionDir {
                path: session_entry.path(),
                session_id: session_entry.file_name().to_string_lossy().into_owned(),
            });
        }
    }
    (dirs, unreadable_entries)
}

/// Additional raw usage files stored under
/// `sessions/<working-directory>/<session-id>/usage.json`. A usage record gets
/// a stable `.usage` identity suffix and retains the original JSON bytes
/// through the ordinary whole-file collector.
#[derive(Debug, Default)]
struct GrokUsageScan {
    records: Vec<SessionRecord>,
    unreadable_files: u64,
}

fn grok_usage_records(
    dirs: &[GrokSessionDir],
    sessions: &[SessionRecord],
    machine: &str,
) -> GrokUsageScan {
    let known: BTreeSet<String> = sessions
        .iter()
        .filter_map(|record| record.id.splitn(3, '.').nth(2).map(str::to_owned))
        .collect();
    let mut scan = GrokUsageScan::default();
    for dir in dirs {
        if !known.contains(&dir.session_id) {
            continue;
        }
        let path = dir.path.join("usage.json");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                scan.unreadable_files += 1;
                continue;
            }
        };
        let mtime = match metadata.modified() {
            Ok(mtime) => mtime,
            Err(_) => {
                scan.unreadable_files += 1;
                continue;
            }
        };
        let usage_id = crate::id::SessionIdentity {
            source_short: "grok",
            machine: machine.to_string(),
            native_id: format!("{}.usage", dir.session_id),
        }
        .id();
        // The native suffix associates usage with its source session; the
        // composed ID may be bounded when that suffix exceeds NAME_MAX.
        scan.records.push(SessionRecord {
            id: usage_id,
            absolute_path: absolutize(&path),
            byte_size: metadata.len(),
            mtime,
            source: HarnessSource::Grok,
            compressed: false,
            sqlite_layout: None,
            provenance: Default::default(),
            session_dir: None,
            movable_root: false,
        });
    }
    scan
}

/// The native Grok CLI transcripts — `chat_history.jsonl`, one directory per
/// session under the same sessions root the usage sidecars live in.
///
/// Every session that holds the file produces a record, including sessions
/// the `session_docs` store has no row for: a session with no row was
/// previously invisible to the archive entirely, and the native transcript is
/// then the session's only observation. The record's id is the session's own
/// identity — the same id a `session_docs` row record gets — so a session
/// with both producers seals both observations into ONE body, which is the
/// shape the reader expects (a body that renders a native turn must not
/// append the legacy plain-text summary again, while the raw body keeps
/// both). The record is an ordinary JSONL file record (no SQLite layout), so
/// the collector gives it the same append-only, byte-accurate cursor every
/// other file source gets: the first pass archives the file whole, and a pass
/// after the CLI appended to it archives only the new bytes.
#[derive(Debug, Default)]
struct GrokHistoryScan {
    records: Vec<SessionRecord>,
    unreadable_files: u64,
}

fn grok_history_records(dirs: &[GrokSessionDir], machine: &str) -> GrokHistoryScan {
    let mut scan = GrokHistoryScan::default();
    for dir in dirs {
        let path = dir.path.join("chat_history.jsonl");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                scan.unreadable_files += 1;
                continue;
            }
        };
        let mtime = match metadata.modified() {
            Ok(mtime) => mtime,
            Err(_) => {
                scan.unreadable_files += 1;
                continue;
            }
        };
        let id = crate::id::SessionIdentity {
            source_short: "grok",
            machine: machine.to_string(),
            native_id: dir.session_id.clone(),
        }
        .id();
        scan.records.push(SessionRecord {
            id,
            absolute_path: absolutize(&path),
            byte_size: metadata.len(),
            mtime,
            source: HarnessSource::Grok,
            compressed: false,
            sqlite_layout: None,
            provenance: Default::default(),
            session_dir: None,
            movable_root: false,
        });
    }
    scan
}

fn hermes_records_from_rows(
    rows: Vec<SqliteSessionRow>,
    db: &Path,
    source: HarnessSource,
    machine: &str,
) -> Vec<SessionRecord> {
    let Ok(modified) = fs::metadata(db).and_then(|metadata| metadata.modified()) else {
        return Vec::new();
    };
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
            mtime: row.mtime.unwrap_or(modified),
            source,
            compressed: false,
            sqlite_layout: Some(SqliteSessionLayout::HermesAgent),
            provenance: Default::default(),
            session_dir: None,
            movable_root: false,
        })
        .collect()
}

/// Sessions a probe knew about but never handed out as `SessionRecord`s.
///
/// `known` is the probe's post-filter count, `emitted` the records actually
/// produced, `unreadable_rows` the candidates the probe itself already
/// classified as undecodable. The sum is the answer to "known N, retrieved M,
/// K unreadable" — it never includes rows the qualification filter rejected
/// after reading them, because those are not failures.
///
/// B90: `unreadable_rows` is now itself three-state. When the probe could not
/// take that tally (`None`), the sum has no honest value — a gap of "however
/// many rows enumeration dropped, plus an unknown number" is unknown, not the
/// first half. Returning `Some(known - emitted)` there would print a number
/// that is provably too small while looking exact.
fn enumeration_gap(known: u64, emitted: usize, unreadable_rows: Option<u64>) -> Option<u64> {
    Some(unreadable_rows? + known.saturating_sub(emitted as u64))
}

/// The one-clause note appended when — and only when — something was in fact
/// unreadable. An all-good scan appends nothing, so the "all good" output and
/// its exit code are byte-for-byte what they were.
fn unreadable_note(
    unreadable: Option<u64>,
    unreadable_stores: u64,
    enumerate_error: Option<&str>,
) -> String {
    let mut note = String::new();
    match unreadable {
        Some(n) if n > 0 => note.push_str(&format!(
            "; {n} unreadable (not empty — body not available locally)"
        )),
        // Counted, and the count is zero: the clean machine's note keeps its
        // pre-B68 bytes.
        Some(_) => {}
        // B90: the tally itself failed. Silence here would be byte-identical
        // to "counted, zero" — the exact confusion this ticket removes.
        None => note.push_str("; unreadable count unknown (this tally itself was not counted)"),
    }
    if unreadable_stores > 0 {
        note.push_str(&format!(
            "; {unreadable_stores} store(s) could not be opened/read"
        ));
    }
    if let Some(error) = enumerate_error {
        note.push_str(&format!("; enumeration failed ({error})"));
    }
    note
}

/// The one clause appended when Cursor's legacy walk hit something it could
/// not read. Empty string when the walk was complete — a clean machine's
/// output does not grow a word (the same contract as [`unreadable_note`]).
fn legacy_ignorance_note(scan: &crate::sqlite_probe::CursorLegacyScan) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(error) = &scan.enumerate_error {
        parts.push(format!(
            "enumeration failed ({error}), session count unknown"
        ));
    }
    if scan.unreadable_stores > 0 {
        parts.push(format!(
            "{} store(s) could not be opened/read (session count unknown)",
            scan.unreadable_stores
        ));
    }
    if scan.unreadable_entries > 0 {
        parts.push(format!(
            "{} unreadable directory entries (session count unknown)",
            scan.unreadable_entries
        ));
    }
    parts.join("; ")
}

/// Legacy workspaces we could not look into, as a count of *entries* whose
/// session cardinality is unknown — never as a count of sessions. An
/// un-enumerable `workspaceStorage` is one such entry: the whole subtree.
fn legacy_unknown_entries(scan: &crate::sqlite_probe::CursorLegacyScan) -> u64 {
    scan.unreadable_stores + scan.unreadable_entries + u64::from(scan.enumerate_error.is_some())
}

/// `None` when the legacy walk was clean, so probes that never had this field
/// keep printing exactly what they printed before.
fn legacy_entry_count_field(scan: &crate::sqlite_probe::CursorLegacyScan) -> Option<u64> {
    scan.saw_ignorance().then(|| legacy_unknown_entries(scan))
}

/// Either the established "nothing there" sentence or the list of things we
/// could not read — never both.
fn legacy_tail_note(ignorance: &str) -> String {
    if ignorance.is_empty() {
        " no readable composer data found".to_string()
    } else {
        format!(" did not finish reading: {ignorance} — cannot conclude it is absent")
    }
}

fn cursor_legacy_records_from_rows(
    rows: Vec<CursorLegacySessionRow>,
    source: HarnessSource,
    machine: &str,
) -> Vec<SessionRecord> {
    rows.into_iter()
        .filter_map(|row| {
            // Legacy Cursor's optional createdAt has the same unknown state.
            let mtime = row.mtime?;
            Some(SessionRecord {
                id: crate::id::SessionIdentity {
                    source_short: source.short(),
                    machine: machine.to_string(),
                    native_id: row.id,
                }
                .id(),
                absolute_path: row.db,
                byte_size: 0,
                mtime,
                source,
                compressed: false,
                sqlite_layout: Some(SqliteSessionLayout::CursorLegacy),
                provenance: Default::default(),
                session_dir: None,
                movable_root: false,
            })
        })
        .collect()
}

/// Probe Cursor's current global store and, when it cannot yield a qualified
/// composer count, its legacy workspaceStorage stores. This keeps the fallback
/// source visible in doctor output while preserving one Cursor footprint row.
fn probe_openclaw_agents(
    mut probe: HarnessProbe,
    confidence: Confidence,
    agents_root: PathBuf,
    env_note: String,
    source: HarnessSource,
    machine: &str,
    report: &mut ScanReport,
) -> HarnessProbe {
    probe.root = Some(agents_root.clone());
    probe.confidence = confidence;
    probe.state = ProbeState::Scanned;
    probe.record_count = Some(0);
    probe.unreadable_count = Some(0);
    probe.bytes = Some(0);
    let agents = match fs::read_dir(&agents_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return HarnessProbe {
                state: ProbeState::Missing,
                record_count: None,
                unreadable_count: None,
                bytes: None,
                note: format!("{env_note}agent root absent"),
                ..probe
            };
        }
        Err(error) => {
            return HarnessProbe {
                state: ProbeState::Indeterminate,
                record_count: None,
                unreadable_count: None,
                note: format!("{env_note}agent root unreadable: {error}"),
                ..probe
            }
        }
    };
    let mut known = 0_u64;
    let mut unreadable = 0_u64;
    let mut total_bytes = 0_u64;
    let mut bytes_complete = true;
    let mut agents_seen = 0_u64;
    let mut skipped_rows = 0_u64;
    let mut unenumerable_stores: Vec<String> = Vec::new();
    for entry in agents {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        let agent_id = entry.file_name().to_string_lossy().into_owned();
        let agent_dir = entry.path();
        let db = agent_dir.join("agent").join("openclaw-agent.sqlite");
        // Only archive rows that actually became records may suppress their
        // cold file: a store whose enumeration failed, or rows it could not
        // identify, must not silence a cold archive they failed to preserve.
        let mut archive_ids = std::collections::HashSet::new();
        match fs::metadata(&db) {
            Ok(metadata) if metadata.is_file() => {
                agents_seen += 1;
                let info = probe_sqlite_store(&db);
                match info.total_bytes {
                    Some(bytes) => total_bytes = total_bytes.saturating_add(bytes),
                    None => {
                        bytes_complete = false;
                        unreadable += 1;
                    }
                }
                match (enumerate_openclaw_sessions(&db), metadata.modified()) {
                    (Ok(enumeration), Ok(mtime)) => {
                        skipped_rows += enumeration.unreadable_rows;
                        unreadable += enumeration.unreadable_rows;
                        let row_count = enumeration.rows.len() as u64;
                        archive_ids = enumeration
                            .rows
                            .iter()
                            .filter(|row| row.generation.is_some())
                            .map(|row| row.session_id.clone())
                            .collect();
                        for row in enumeration.rows {
                            let base_id =
                                crate::sqlite_probe::openclaw_native_id(&agent_id, &row.session_id);
                            let native = match row.generation {
                                Some(generation) => format!("{base_id}~g{generation}"),
                                None => base_id,
                            };
                            report.records.push(SessionRecord {
                                id: crate::id::SessionIdentity {
                                    source_short: source.short(),
                                    machine: machine.to_string(),
                                    native_id: native,
                                }
                                .id(),
                                absolute_path: db.clone(),
                                byte_size: 0,
                                mtime,
                                source,
                                compressed: false,
                                sqlite_layout: Some(SqliteSessionLayout::OpenClaw),
                                provenance: Default::default(),
                                session_dir: None,
                                movable_root: false,
                            });
                        }
                        known += row_count;
                    }
                    // The database mtime is unknown, so its rows cannot
                    // become records; they are counted unreadable, and the
                    // cold files stay (nothing suppresses them).
                    (Ok(enumeration), Err(_)) => {
                        skipped_rows += enumeration.unreadable_rows;
                        unreadable += enumeration.unreadable_rows + enumeration.rows.len() as u64;
                    }
                    (Err(error), _) => {
                        unreadable += 1;
                        unenumerable_stores.push(format!("{agent_id}: {error}"));
                    }
                }
                probe.recognized_files.push(db);
            }
            Ok(_) => unreadable += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => unreadable += 1,
        }
        scan_openclaw_cold_files(
            &agent_id,
            &agent_dir,
            &archive_ids,
            source,
            machine,
            report,
            &mut known,
            &mut unreadable,
            &mut total_bytes,
        );
    }
    probe.record_count = Some(known);
    probe.unreadable_count = Some(unreadable);
    probe.bytes = bytes_complete.then_some(total_bytes);
    let mut note = format!("{env_note}OpenClaw read-only per-agent SQLite scan; {agents_seen} agent database(s), cold session archives deduplicated against archive rows");
    if skipped_rows > 0 {
        note += &format!("; {skipped_rows} unreadable SQLite row(s) skipped");
    }
    if !unenumerable_stores.is_empty() {
        note += &format!(
            "; {} unenumerable agent store(s), first: {}",
            unenumerable_stores.len(),
            unenumerable_stores[0]
        );
    }
    probe.note = note;
    report.records.sort_by(|a, b| a.id.cmp(&b.id));
    probe
}

#[allow(clippy::too_many_arguments)]
fn scan_openclaw_cold_files(
    agent_id: &str,
    agent_dir: &Path,
    sqlite_archive_ids: &std::collections::HashSet<String>,
    source: HarnessSource,
    machine: &str,
    report: &mut ScanReport,
    known: &mut u64,
    unreadable: &mut u64,
    total_bytes: &mut u64,
) {
    let sessions_dir = agent_dir.join("sessions");
    let files = match fs::read_dir(sessions_dir) {
        Ok(files) => files,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(_) => {
            *unreadable += 1;
            return;
        }
    };
    for file in files {
        let file = match file {
            Ok(file) => file,
            Err(_) => {
                *unreadable += 1;
                continue;
            }
        };
        let path = file.path();
        if path.extension().and_then(|e| e.to_str()) != Some("zst") {
            continue;
        }
        let name = file.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.split(".jsonl").next() else {
            continue;
        };
        if stem.is_empty() || sqlite_archive_ids.contains(stem) {
            continue;
        }
        match file.metadata() {
            Ok(metadata) if metadata.is_file() => match metadata.modified() {
                Ok(mtime) => {
                    // Counted only once the record exists: an mtime failure
                    // is reported unreadable without also inflating `known`.
                    *known += 1;
                    *total_bytes = total_bytes.saturating_add(metadata.len());
                    report.records.push(SessionRecord {
                        id: crate::id::SessionIdentity {
                            source_short: source.short(),
                            machine: machine.to_string(),
                            native_id: crate::sqlite_probe::openclaw_cold_native_id(
                                agent_id, stem, &name,
                            ),
                        }
                        .id(),
                        absolute_path: path,
                        byte_size: metadata.len(),
                        mtime,
                        source,
                        compressed: true,
                        sqlite_layout: None,
                        provenance: Default::default(),
                        session_dir: None,
                        movable_root: false,
                    });
                }
                Err(_) => *unreadable += 1,
            },
            _ => *unreadable += 1,
        }
    }
}

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
            note: format!(
                "{env_note}Cursor globalStorage path cannot derive the User root directory"
            ),
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
        }) => format!("global cursorDiskKV before filter {candidate_count} / after {count}; "),
        Some(SqliteSessionProbe::SchemaMismatch { .. }) => {
            "global cursorDiskKV schema not recognized; ".to_string()
        }
        Some(SqliteSessionProbe::ReadFailed { .. }) => {
            "global cursorDiskKV read-only enumeration failed; ".to_string()
        }
        None => "global cursorDiskKV not present; ".to_string(),
    };
    // B82: the legacy walk now hands back what it could *not* read as well as
    // what it could. `legacy_scan` is consulted twice: once for the probe it
    // produced, and once at the bottom, where "no legacy composer data" must
    // not be printed if the walk in fact never got to look.
    let legacy_scan = if global_needs_fallback {
        crate::sqlite_probe::probe_cursor_legacy_workspace_storage(&workspace_storage)
    } else {
        crate::sqlite_probe::CursorLegacyScan::default()
    };
    let legacy_ignorance_note = legacy_ignorance_note(&legacy_scan);
    if global_needs_fallback {
        if let Some(legacy) = legacy_scan.probe.clone() {
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
                    unreadable_count: unreadable,
                    earliest,
                    latest,
                    bytes: legacy.total_bytes,
                    recognized_files,
                    note: format!(
                        "{env_note}from legacy workspaceStorage; {global_summary}SQLite read-only enumeration of ItemTable; before filter {candidate_count} / after {count} sessions; SessionRecord={record_count}{gap_note}"
                    ),
                    ..base
                };
            }
        }
    }

    let Some(info) = global_info else {
        // B82: "globalStorage missing + legacy not found" was printed even when the
        // legacy walk had been refused entry. An absence we could not verify
        // is not an absence; it is `Indeterminate`, and it keeps its unreadable
        // entry count so `status` can say "session count unknown".
        if legacy_scan.saw_ignorance() {
            report.indeterminate_roots.push(workspace_storage.clone());
            return HarnessProbe {
                root: Some(workspace_storage),
                confidence,
                state: ProbeState::Indeterminate,
                unreadable_entry_count: Some(legacy_unknown_entries(&legacy_scan)),
                note: format!(
                    "{env_note}globalStorage/state.vscdb not present; legacy workspaceStorage not fully read: {legacy_ignorance_note} — cannot conclude it is absent"
                ),
                ..base
            };
        }
        report.missing_roots.push(global_db.clone());
        return HarnessProbe {
            root: Some(global_db),
            confidence,
            state: ProbeState::Missing,
            note: format!(
                "{env_note}globalStorage/state.vscdb not present, and no readable composer data found in legacy workspaceStorage"
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
                    Some("registry cell does not declare a SQLite schema".to_string()),
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
                unreadable_count: unreadable,
                earliest,
                latest,
                bytes: info.total_bytes,
                recognized_files,
                note: format!(
                    "{env_note}SQLite read-only enumeration of cursorDiskKV table(key, value); before filter {candidate_count} / after {count} sessions; SessionRecord={record_count}{gap_note}"
                ),
                ..base
            }
        }
        // Both tails end in "legacy workspaceStorage no readable composer
        // data found". B82: that sentence is only allowed when the legacy walk
        // actually completed; otherwise it says what it could not read.
        SqliteSessionProbe::SchemaMismatch { actual } => HarnessProbe {
            root: Some(global_db),
            confidence,
            state: ProbeState::FileTarget,
            bytes: info.total_bytes,
            unreadable_entry_count: legacy_entry_count_field(&legacy_scan),
            note: format!(
                "{env_note}detected SQLite but schema not recognized (expected cursorDiskKV(key, value), got {actual}); legacy workspaceStorage{}",
                legacy_tail_note(&legacy_ignorance_note)
            ),
            ..base
        },
        SqliteSessionProbe::ReadFailed { error } => HarnessProbe {
            root: Some(global_db),
            confidence,
            state: ProbeState::FileTarget,
            bytes: info.total_bytes,
            unreadable_entry_count: legacy_entry_count_field(&legacy_scan),
            note: format!(
                "{env_note}detected SQLite but read-only enumeration failed ({error}); legacy workspaceStorage{}",
                legacy_tail_note(&legacy_ignorance_note)
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
/// `session` schema and then reporting "schema not recognized" about a store we can
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
/// lands on `Missing`/`directory does not exist`, i.e. an unknown count, never a fake `0`.
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
///
/// `pub(crate)` since W1009 (DSK-3): doctor's Desktop retention probe
/// expands the registry's declared settings-file templates through the same
/// expander every session root uses, so a template can never resolve two
/// different ways in one binary.
pub(crate) fn static_prefix_root(template: &str) -> Option<(PathBuf, bool)> {
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
                // B96: the guard used to be `name_len == rest.len()`, which is
                // true for two different things — a bare trailing `$` (no name
                // at all) and a template that *ends* in a variable, e.g. the
                // whole template being `$XDG_DATA_HOME`. The second one is
                // perfectly resolvable, and rejecting it reports a harness whose
                // root IS knowable as `sessions=unknown`. That is this repo's rule run
                // backwards: turning something already known into "unknown".
                // No registry entry ends in a bare variable today, so this was
                // unreachable — but the shape was wrong. `name_len == 1` is the
                // real "there is no name after the `$`" test.
                if name_len == 1 {
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

/// Unified `~` expansion, delegated to `config` so every consumer shares one
/// implementation. The scanner is read-only: an unexpandable path (missing
/// home, `~otheruser`, a literal `~` component) is warned about and probed as
/// written — the probe will simply find nothing, and the scanner never writes
/// a masterkey, so no literal `~` can leak credentials here.
fn expand_tilde(p: &str) -> PathBuf {
    match crate::config::expand_and_verify(p) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("warning: cannot expand path `{p}`: {e}");
            PathBuf::from(p)
        }
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

/// Where a directory sits relative to the cell's [`SessionDirRule`].
///
/// The state travels down the walk instead of being recomputed per file, so a
/// file at any depth "sees" its session directory. Ordinary exact-depth rules
/// open the first matching scope and descendants keep it. A leading-`**` rule
/// may match again below that scope; then the nearest matching directory owns
/// the transcript. [`relpath_matches`] decides whether the file is that
/// session's transcript. The root itself never counts.
#[derive(Debug, Clone)]
enum SessionScope {
    /// No ancestor directory matched the rule's `pattern`.
    Outside,
    /// The current matching session-directory ancestor: its path and
    /// (usually) name — the native id. A rule that declares
    /// `id_components` composes the id from selected matched components
    /// instead (ADR-054 D3: codebuff's `<basename>.<chatId>`). Ordinary
    /// rules keep the first scope; recursive rules replace it when a
    /// nested directory matches.
    ///
    /// `container` is the segment immediately above the session
    /// directory when that segment is a workspace identity: Kimi
    /// Code's measured layout is `sessions/<workspaceId>/<sessionId>`,
    /// so the parent segment of a session directory is its workspace.
    /// It is observed only at that exact depth — a session directly
    /// under the root has no workspace segment, and a nested one
    /// carries an implementation directory above it, neither of which
    /// is a workspace — and only for the harness whose layout gives
    /// that segment a workspace meaning. Unrecognized hierarchies
    /// leave it `None`, and a `None` container is *not observed*,
    /// never "no workspace".
    ///
    /// `chat_dir_name` and `dir_mtime` ride the scope only for the
    /// harness whose facts are captured from the session directory
    /// itself (codebuff, ADR-054 D4): the chat directory's own name
    /// for the start-time record, and the directory's own mtime for
    /// the cross-check. They stay `None` for every other harness, and
    /// no later stage may re-derive either from the archive id.
    In {
        dir: PathBuf,
        id: String,
        container: Option<String>,
        chat_dir_name: Option<String>,
        dir_mtime: Option<std::time::SystemTime>,
    },
}

/// Stat a matched codebuff chat directory for its capture-time facts
/// (ADR-054 D4): the directory's own name — which is the session start time
/// in the harness's encoding — and its own mtime (plus ctime under Unix,
/// whichever is earlier) for the cross-check that a recorded start time is not
/// preceded by the directory. `None` when the metadata cannot be read: an
/// unstattable directory observes nothing, and the facts stay unknown rather
/// than being fabricated from elsewhere.
fn codebuff_chat_dir_facts(dir: &Path) -> Option<(String, std::time::SystemTime)> {
    let name = dir.file_name()?.to_string_lossy().into_owned();
    let metadata = fs::metadata(dir).ok()?;
    let mtime = metadata.modified().ok();
    // std exposes no ctime on Windows; there the mtime alone guards the
    // cross-check. On Unix both are read and the earlier one compared, so an
    // either-way-precedence is caught on the platforms that can see it.
    let ctime = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // reason: a ctime the platform reports before the epoch (an
            // exotically back-dated inode) cannot live in the u64 seconds
            // this stat feeds; the epoch itself stands in, which still
            // precedes every chat directory name the D4 pattern accepts.
            let secs = u64::try_from(metadata.ctime()).unwrap_or(0);
            // reason: ctime_nsec is unsigned already; the try_from exists
            // only for type juggling, and a nanos field that overflows u32
            // (never seen on any supported filesystem) clamps to zero
            // seconds-of-nanos, a sub-millisecond error in a cross-check
            // that compares whole seconds.
            let nanos = u32::try_from(metadata.ctime_nsec()).unwrap_or(0);
            std::time::UNIX_EPOCH.checked_add(std::time::Duration::new(secs, nanos))
        }
        #[cfg(not(unix))]
        {
            None
        }
    };
    let earlier = mtime.zip(ctime).map(|(mtime, ctime)| mtime.min(ctime));
    let dir_mtime = earlier.or(mtime)?;
    Some((name, dir_mtime))
}

/// The scope a child directory inherits: unchanged, or replaced when the
/// child's own name matches.
fn descend_scope(
    parent: &SessionScope,
    dir: &Path,
    root: &Path,
    rule: Option<&SessionDirRule>,
    source: HarnessSource,
) -> SessionScope {
    let Some(rule) = rule else {
        return parent.clone();
    };
    let Ok(relative) = dir.strip_prefix(root) else {
        return parent.clone();
    };
    let components: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    let pattern: Vec<&str> = rule
        .pattern
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    let (recursive, parts) = match pattern.split_first() {
        Some((&"**", rest)) => (true, rest),
        _ => (false, pattern.as_slice()),
    };
    if parts.is_empty() {
        return parent.clone();
    }
    if !recursive && matches!(parent, SessionScope::In { .. }) {
        // Non-recursive rules are anchored to one exact depth below the root;
        // implementation directories inside an open session cannot rematch.
        return parent.clone();
    }
    let start = if recursive {
        let Some(start) = components.len().checked_sub(parts.len()) else {
            return parent.clone();
        };
        start
    } else {
        if components.len() != parts.len() {
            return parent.clone();
        }
        0
    };
    if !components[start..]
        .iter()
        .zip(parts)
        .all(|(name, pattern)| matches_session_pattern(name, Some(pattern)))
    {
        return parent.clone();
    }
    let Some(name) = components.last().cloned() else {
        return parent.clone();
    };
    // The matched window: for the non-recursive rules every existing
    // `session_dir` harness uses, the pattern covers exactly these
    // components root-relative; for a recursive rule the `**` prefix matched
    // the components above them.
    let matched = &components[start..];
    // The native id defaults to the session directory's own name — the
    // pre-codebuff behaviour every existing rule relies on. `id_components`
    // composes it from selected matched components instead, joined by `.`:
    // codebuff's chats key themselves by the *pair* `<project-basename>.<chatId>`
    // (projects/<basename>/chats/<chatId>, indices [1, 3]), and the name
    // alone is unique only within one project basename — taking the last
    // component would hand two chats under different basenames with the same
    // timestamp id the same archive slot, the exact collapse this rule
    // exists to prevent (ADR-054 D3). An out-of-range index set falls back
    // to the last component: a wrong selection is a registry bug to be
    // caught in review and tests, not a licence to compose from a subset
    // and mint a narrower id.
    let id = if !rule.id_components.is_empty()
        && rule
            .id_components
            .iter()
            .all(|index| *index < matched.len())
    {
        rule.id_components
            .iter()
            .map(|index| matched[*index].clone())
            .collect::<Vec<String>>()
            .join(".")
    } else {
        name.clone()
    };
    // Kimi Code's measured layout is `<root>/<workspaceId>/<sessionId>`:
    // the segment immediately above the session directory is a workspace
    // identity, and only at that exact depth. One level (a session directly
    // under the root) has no workspace segment above it, and a deeper
    // match carries an implementation directory above the session, which is
    // not a workspace; both leave the container unobserved rather than
    // recording a non-workspace segment as one. The other harness that
    // declares a two-level rule (DeepSeek Harness) names its segment by a
    // lossy cwd slug, which is a path derivative and never a container
    // identity (`cwd ≠ repo identity`), so only Kimi Code reads the parent
    // segment as a workspace. The depth is counted from the walked root,
    // so the segment is a workspace identity only when that root is the
    // harness's own sessions directory — the same anchoring the
    // per-session index (`kimi_work_dirs`) is held to for the cwd it
    // reads.
    let container =
        (source == HarnessSource::KimiCode && components.len() == 2).then(|| components[0].clone());
    // Codebuff's chat directory name *is* the session start time (ADR-054
    // F5), and its directory mtime crosses checks the time (D4): both are
    // recorded at capture, from the scan that just matched the directory —
    // later stages must never parse either out of the archive id, which can
    // be bounded and lose the tail entirely. No other session-dir harness
    // carries these facts, so the walk stats nothing for them.
    let codebuff_facts = (source == HarnessSource::Codebuff)
        .then(|| codebuff_chat_dir_facts(dir))
        .flatten();
    let (chat_dir_name, dir_mtime) = match codebuff_facts {
        Some((dir_name, dir_mtime)) => (Some(dir_name), Some(dir_mtime)),
        None => (None, None),
    };
    SessionScope::In {
        dir: dir.to_path_buf(),
        id,
        container,
        chat_dir_name,
        dir_mtime,
    }
}

/// True when `file`'s path below `dir` is exactly `expected` (`/`-separated,
/// component-wise, so `\`-separated paths on Windows compare equal).
fn relpath_matches(dir: &Path, file: &Path, expected: &str) -> bool {
    let Ok(rel) = file.strip_prefix(dir) else {
        return false;
    };
    let want: Vec<&str> = expected.split('/').filter(|s| !s.is_empty()).collect();
    let got: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    got.len() == want.len() && got.iter().zip(&want).all(|(got, want)| got == want)
}

/// True when `file`'s path below `dir` matches the `/`-separated component
/// glob `glob`: same depth, each component matched with the same `*` syntax
/// [`matches_session_pattern`] applies (so a Windows backslash relpath
/// compares equal too). The depth is exact on purpose — a `session_dir`
/// rule with a `file_glob` declares a measured layout, and anything at a
/// different depth is noise the rule must reject, not a session.
fn relpath_glob_matches(dir: &Path, file: &Path, glob: &str) -> bool {
    let Ok(rel) = file.strip_prefix(dir) else {
        return false;
    };
    let want: Vec<&str> = glob.split('/').filter(|s| !s.is_empty()).collect();
    let got: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    got.len() == want.len()
        && got
            .iter()
            .zip(&want)
            .all(|(got, want)| matches_session_pattern(got, Some(want)))
}

#[allow(clippy::too_many_arguments)]
fn collect_records(
    root: &Path,
    source: HarnessSource,
    machine: &str,
    format: &str,
    session_pattern: Option<&str>,
    session_dir: Option<&SessionDirRule>,
) -> DirectoryScan {
    // Kimi Code keeps a one-line-per-session index at
    // `<home>/session_index.jsonl` (`sessionId`, `sessionDir`,
    // `workDir`); the sessions root it is walked through is
    // `<home>/sessions`, so the index sits one level above it. Its
    // `workDir` column is the working directory the source itself
    // recorded for a session, which makes it the session's observed
    // cwd. An absent, unreadable or malformed index observes
    // nothing, and so does a session the index holds no line for.
    // Both Kimi dimensions are anchored to that root being the
    // harness's own sessions directory: the index is looked up at the
    // root's parent, and the workspace segment is the one exactly two
    // levels below the root. A scan root that is not that directory —
    // a `harness_roots` override pointing at `<home>` itself or at a
    // session directory — leaves the container and the indexed cwd
    // unobserved, never mislabeled.
    let session_index = if source == HarnessSource::KimiCode {
        kimi_work_dirs(root)
    } else {
        BTreeMap::new()
    };
    let mut records = Vec::new();
    let mut unreadable_count = 0;
    let mut unreadable_entry_count = 0;
    let mut stack: Vec<(PathBuf, SessionScope)> = vec![(root.to_path_buf(), SessionScope::Outside)];
    while let Some((dir, scope)) = stack.pop() {
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
                let child = descend_scope(&scope, &path, root, session_dir, source);
                stack.push((path, child));
                continue;
            }
            if !file_type.is_file() {
                continue; // skip sockets, FIFOs, and (crucially) symlinks
            }
            match build_record(
                &path,
                source,
                machine,
                format,
                session_pattern,
                &scope,
                session_dir,
                &session_index,
            ) {
                RecordBuild::Record(record) => records.push(record),
                RecordBuild::Unreadable => unreadable_count += 1,
                RecordBuild::NotSession => {}
            }
        }
    }
    // Whitelist sidecars (ADR-054 D5/D9): a session-directory rule that
    // declares `extra_files` seals those literal names beside the
    // transcript under the *same* session identity whenever they exist.
    // They attach here, after the walk, because the transcript file and
    // its sidecars are visited in directory order and the record is the one
    // thing tying them together. Absent files observe nothing — a session
    // without its sidecars is still archived — and an unreadable sidecar is
    // skipped the same way (its bytes are optional derived data, and a
    // missing observation never blocks the transcript).
    if let Some(rule) = session_dir.filter(|rule| !rule.extra_files.is_empty()) {
        for record in &mut records {
            let Some(chat_dir) = record.absolute_path.parent() else {
                continue;
            };
            let facts = record.session_dir.get_or_insert_with(Default::default);
            for name in &rule.extra_files {
                let sidecar = chat_dir.join(name);
                match fs::metadata(&sidecar) {
                    Ok(md) if md.is_file() => {
                        facts.extra_files.push(crate::models::ExtraFileSource {
                            name: name.clone(),
                            byte_size: md.len(),
                            path: absolutize(&sidecar),
                        })
                    }
                    _ => {}
                }
            }
        }
    }
    DirectoryScan {
        records,
        unreadable_count,
        unreadable_entry_count,
    }
}

/// Kimi Code's per-session index, as a `sessionId` → `workDir` map.
///
/// The index lives beside the sessions root (`<home>/sessions`), so
/// it is `<root>`'s parent directory's `session_index.jsonl`. Each
/// line is a JSON object; a line that is not JSON, or that carries
/// no `sessionId` or no non-empty `workDir`, contributes nothing —
/// absence is not an empty working directory.
///
/// "Beside the sessions root" is the anchoring contract: a walk root
/// that is not the harness's `sessions` directory itself — a
/// `harness_roots` override pointed at `<home>` or at a session
/// directory — looks the index up under the wrong parent and observes
/// no working directory, the same outcome as a session the index
/// holds no line for.
fn kimi_work_dirs(root: &Path) -> BTreeMap<String, String> {
    let Some(index) = root.parent().map(|home| home.join("session_index.jsonl")) else {
        return BTreeMap::new();
    };
    let Ok(bytes) = fs::read_to_string(&index) else {
        return BTreeMap::new();
    };
    let mut work_dirs = BTreeMap::new();
    for line in bytes.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(session) = value.get("sessionId").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(work_dir) = value.get("workDir").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !session.is_empty() && !work_dir.is_empty() {
            work_dirs.insert(session.to_string(), work_dir.to_string());
        }
    }
    work_dirs
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
            // The registry declares the format, so this reads the declaration
            // rather than guessing from a filename: both the `zst` spelling
            // (codex's `jsonl.zst`) and the `zstd` spelling (DeepSeek
            // Harness's `jsonl.zstd`) are zstd. `compressed` decides whether
            // the bytes are stored as decoded lines or as opaque payload, so a
            // source that says `.zstd` while this reads false would be archived
            // as compressed bytes and rendered as binary.
            let compressed = suffix.ends_with(".zst") || suffix.ends_with(".zstd");
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
    // reason: precondition checked parts.len() >= 2, parts.last() is guaranteed Some; unwrap_or_default is type-safety fallback
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

/// Claude Code's Workflows runs write one journal per run at
/// `…/<parent-session>/subagents/workflows/wf_<run-uuid>/journal.jsonl`. The
/// filename stem is the harness constant `journal` — identical for every run
/// — so a stem id collapses every workflow run on the machine into one
/// archive slot, and the raw file count standing against collect's distinct
/// record ids made that aliasing masquerade as an unarchivable gap (GB-2).
/// The workflow-run directory names the run structurally — `wf_` and the
/// trailing run id are harness-generated constants, like the `subagents` and
/// `workflows` segments above it — so for exactly this layout the directory
/// name is the native id. Nothing else re-ids: ordinary `<uuid>.jsonl`
/// session files, `agent-<hex>.jsonl` transcripts at any depth, and any
/// `journal.jsonl` not directly inside a `wf_<…>` directory under
/// `subagents/workflows` keep the filename-stem id.
fn claude_code_workflow_journal_native_id(path: &Path, stem: &str) -> Option<String> {
    if stem != "journal" {
        return None;
    }
    let run_dir = path.parent()?;
    let wf = run_dir.file_name()?.to_string_lossy().into_owned();
    let run = wf.strip_prefix("wf_")?;
    if run.is_empty() {
        return None;
    }
    let workflows = run_dir.parent()?;
    if workflows.file_name()?.to_string_lossy() != "workflows" {
        return None;
    }
    let subagents = workflows.parent()?;
    if subagents.file_name()?.to_string_lossy() != "subagents" {
        return None;
    }
    Some(wf)
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

#[allow(clippy::too_many_arguments)]
fn build_record(
    path: &Path,
    expected: HarnessSource,
    machine: &str,
    format: &str,
    session_pattern: Option<&str>,
    scope: &SessionScope,
    session_dir: Option<&SessionDirRule>,
    session_index: &BTreeMap<String, String>,
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

    // A cell that declares a session directory takes the native id from that
    // directory and accepts only the one file path the rule names. A file with
    // no such directory above it, or one that is not that session's declared
    // transcript, is simply not a session of this harness — it is never
    // silently labelled with the constant filename stem, which is what would
    // merge every session into one slot. The same scope carries the workspace
    // segment above the session directory (its container, when the layout
    // gives that segment a workspace meaning) and, for a harness that keeps
    // a per-session index, the working directory the index recorded.
    //
    // The record's own source detection runs before the id is chosen, so the
    // claude-code structural rule below sees the same source the record will
    // be labelled with — a harness_roots override relocates a tree without
    // losing the rule, and a file detected as another harness never gains it.
    let source = detect_source(path).unwrap_or(expected);
    // The codebuff facts ride the scope; every other harness leaves them
    // unset, and a record outside any session directory has none to take.
    let codebuff_scope = match scope {
        SessionScope::In {
            chat_dir_name,
            dir_mtime,
            ..
        } if source == HarnessSource::Codebuff => Some((chat_dir_name, dir_mtime)),
        _ => None,
    };
    let (native_id, container, indexed_cwd) = match (session_dir, scope) {
        (
            Some(rule),
            SessionScope::In {
                dir, id, container, ..
            },
        ) => {
            match rule.transcript_target() {
                TranscriptTarget::Literal(file) => {
                    if !relpath_matches(dir, path, file) {
                        return RecordBuild::NotSession;
                    }
                    let cwd = session_index.get(id.as_str()).cloned();
                    (id.clone(), container.clone(), cwd)
                }
                TranscriptTarget::Glob(glob) => {
                    // The glob is the file-name-ids-the-session shape
                    // (ADR-059 draft D2/D3): the matched transcript's own
                    // stem — with the declared format suffix stripped by
                    // the caller above — is the native id, so a sandbox
                    // directory named `local_<uuid>` never becomes the
                    // identity and two transcripts in one sandbox are two
                    // sessions, never one.
                    if !relpath_glob_matches(dir, path, glob) {
                        return RecordBuild::NotSession;
                    }
                    let cwd = session_index.get(stem.as_str()).cloned();
                    (stem.clone(), container.clone(), cwd)
                }
            }
        }
        (Some(_), SessionScope::Outside) => return RecordBuild::NotSession,
        // Claude Code's workflow journals sit at a fixed path whose stem is
        // a constant, so exactly that layout ids by its workflow-run
        // directory; every claude-code file outside it keeps the stem id.
        (None, _) if source == HarnessSource::ClaudeCode => (
            claude_code_workflow_journal_native_id(path, &stem).unwrap_or_else(|| stem.clone()),
            None,
            None,
        ),
        (None, _) => (stem, None, None),
    };

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
        native_id,
    };

    // The 4D dimensions this record's own discovery observed: the
    // workspace segment above the session directory (container) and
    // the working directory the source's index recorded (cwd). Both
    // stay unobserved when the layout or the index provides neither,
    // and neither is ever inferred from the harness name.
    let mut provenance = crate::provenance::SessionProvenance::default();
    if let Some(container) = container {
        provenance.insert_container(container);
    }
    if let Some(work_dir) = indexed_cwd {
        provenance.insert_cwd(work_dir);
    }

    // The session-directory facts codebuff captures at discovery (ADR-054
    // D4/D9): the chat directory name's own instant, parsed here while the
    // actual name is still in hand — a name that does not match the accepted
    // shape records no start time rather than approximating one, and no
    // later stage re-reads the time out of the archive id, whose bounding
    // can drop the timestamp entirely — and the directory mtime for the
    // cross-check. `extra_files` are attached to the record after the walk
    // by [`collect_records`], which sees the sidecars beside the transcript
    // it already found.
    let session_dir_facts = codebuff_scope.map(|(chat_dir_name, dir_mtime)| {
        crate::models::SessionDirFacts {
            // The multi-root probe stamps the registry root id after this
            // scan; a record that skips it carries no root to name.
            root_id: None,
            captured_start: chat_dir_name
                .as_deref()
                .and_then(crate::codebuff::captured_start_from_chat_dir_name),
            dir_mtime: *dir_mtime,
            extra_files: Vec::new(),
        }
    });

    RecordBuild::Record(SessionRecord {
        id: ident.id(),
        absolute_path: absolutize(path),
        byte_size,
        mtime,
        source,
        compressed,
        sqlite_layout: None,
        provenance,
        session_dir: session_dir_facts,
        movable_root: false,
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
    use crate::test_support::Sandbox;
    use std::sync::Mutex;

    /// Serialises tests that mutate process env vars — cargo runs tests in
    /// parallel threads and `set_var` is process-global.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A one-harness claude-code cell whose declared root is the sandbox
    /// home's `~/.claude/projects`, so the GB-2 tests can plant harness-shaped
    /// trees and walk them through the same registry path production uses.
    fn claude_code_directory_registry() -> HarnessRegistry {
        let cell = serde_json::json!({
            "template": "~/.claude/projects",
            "format": "jsonl",
            "confidence": "measured-locally",
            "source": "synthetic fixture"
        });
        let paths = match current_platform() {
            "macos" => serde_json::json!({"macos": cell}),
            "linux" => serde_json::json!({"linux": cell}),
            "windows" => serde_json::json!({"windows": cell}),
            platform => panic!("unexpected platform: {platform}"),
        };
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{"id": "claude-code", "display_name": "synthetic", "paths": paths}]
        }))
        .unwrap()
    }

    /// A claude-code-shaped registry with the DSK-1 (plan C) shape: the CLI
    /// store stays the entry's primary `paths`, and the desktop Cowork store
    /// is a `claude-desktop-cowork` source root beside it — the same harness
    /// id, a different surface, expressed per-root.
    fn claude_code_cli_plus_desktop_registry() -> HarnessRegistry {
        let platform_key = current_platform();
        let paths = serde_json::json!({ platform_key: {
            "template": "~/.claude/projects",
            "format": "jsonl",
            "confidence": "measured-locally",
            "source": "synthetic fixture"
        }});
        let desktop = serde_json::json!({
            "template": "~/Library/Application Support/Claude/local-agent-mode-sessions/",
            "format": "jsonl",
            "confidence": "measured-locally",
            "source": "synthetic fixture",
            "session_dir": {
                "pattern": "*/*/local_*",
                "file_glob": ".claude/projects/*/*.jsonl"
            }
        });
        let desktop_paths = serde_json::json!({ platform_key: desktop });
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{
                "id": "claude-code",
                "display_name": "synthetic",
                "paths": paths,
                "source_roots": [{
                    "id": "claude-desktop-cowork",
                    "paths": desktop_paths,
                    "provenance": { "surface": ["desktop-cowork"] }
                }]
            }]
        }))
        .unwrap()
    }

    /// The DSK-1 claude-code shape with the desktop Cowork root UNVERIFIED
    /// on this platform: the registry declares the root's cell but refuses
    /// to vouch for its path (`unascertained`), so the probe skips it by
    /// policy — the exact cell the shipped registry carries for Windows,
    /// replayable on every platform for the aggregation rules the external
    /// review of ADR-059 draft D6 made mandatory (W1006d).
    fn claude_code_cli_plus_unverified_desktop_registry() -> HarnessRegistry {
        let platform_key = current_platform();
        let paths = serde_json::json!({ platform_key: {
            "template": "~/.claude/projects",
            "format": "jsonl",
            "confidence": "measured-locally",
            "source": "synthetic fixture"
        }});
        let desktop = serde_json::json!({
            "template": "~/Library/Application Support/Claude/local-agent-mode-sessions/",
            "format": "jsonl",
            "confidence": "unascertained",
            "source": "synthetic fixture"
        });
        let desktop_paths = serde_json::json!({ platform_key: desktop });
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{
                "id": "claude-code",
                "display_name": "synthetic",
                "paths": paths,
                "source_roots": [{
                    "id": "claude-desktop-cowork",
                    "paths": desktop_paths,
                    "provenance": { "surface": ["desktop-cowork"] }
                }]
            }]
        }))
        .unwrap()
    }

    /// A claude-code-shaped registry whose WHOLE family stays unverified on
    /// this platform: the primary cell is measurable but its template is
    /// not statically resolvable, and the desktop root's cell is
    /// `unascertained` — one root never resolved a path, one the scan
    /// refuses to guess at, and nothing in the family was ever measured.
    fn claude_code_all_unverified_registry() -> HarnessRegistry {
        let platform_key = current_platform();
        let paths = serde_json::json!({ platform_key: {
            "template": "$CWD/sessions",
            "format": "jsonl",
            "confidence": "measured-locally",
            "source": "synthetic fixture"
        }});
        let desktop = serde_json::json!({
            "template": "~/Library/Application Support/Claude/local-agent-mode-sessions/",
            "format": "jsonl",
            "confidence": "unascertained",
            "source": "synthetic fixture"
        });
        let desktop_paths = serde_json::json!({ platform_key: desktop });
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{
                "id": "claude-code",
                "display_name": "synthetic",
                "paths": paths,
                "source_roots": [{
                    "id": "claude-desktop-cowork",
                    "paths": desktop_paths,
                    "provenance": { "surface": ["desktop-cowork"] }
                }]
            }]
        }))
        .unwrap()
    }

    /// The all-unverified family with BOTH cells skipped by the same
    /// policy reason (`unascertained`), for the `skip unascertained`
    /// aggregate word — the confidence gate runs before any path is
    /// resolved, so a vouched-for template under an unvouched confidence
    /// is still a refusal to scan, not a path choice.
    fn claude_code_all_unverified_registry_with_unascertained_primary() -> HarnessRegistry {
        let platform_key = current_platform();
        let paths = serde_json::json!({ platform_key: {
            "template": "~/.claude/projects",
            "format": "jsonl",
            "confidence": "unascertained",
            "source": "synthetic fixture"
        }});
        let desktop = serde_json::json!({
            "template": "~/Library/Application Support/Claude/local-agent-mode-sessions/",
            "format": "jsonl",
            "confidence": "unascertained",
            "source": "synthetic fixture"
        });
        let desktop_paths = serde_json::json!({ platform_key: desktop });
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{
                "id": "claude-code",
                "display_name": "synthetic",
                "paths": paths,
                "source_roots": [{
                    "id": "claude-desktop-cowork",
                    "paths": desktop_paths,
                    "provenance": { "surface": ["desktop-cowork"] }
                }]
            }]
        }))
        .unwrap()
    }

    /// A codex-shaped registry with the DSK-1 movable-root shape: the active
    /// rollouts root stays the entry's primary `paths`, and the archive the
    /// desktop's `thread/archive` moves rollouts into is a `codex-archived`
    /// source root beside it — the same store, the same ids.
    fn codex_plus_archived_registry() -> HarnessRegistry {
        let platform_key = current_platform();
        let sessions = serde_json::json!({
            "template": "~/.codex/sessions/",
            "format": "jsonl / jsonl.zst",
            "confidence": "measured-locally",
            "source": "synthetic fixture"
        });
        let archived = serde_json::json!({
            "template": "~/.codex/archived_sessions/",
            "format": "jsonl / jsonl.zst",
            "confidence": "measured-locally",
            "source": "synthetic fixture"
        });
        let paths = serde_json::json!({ platform_key: sessions });
        let archived_paths = serde_json::json!({ platform_key: archived });
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{
                "id": "codex",
                "display_name": "synthetic",
                "paths": paths,
                "source_roots": [{
                    "id": "codex-archived",
                    "paths": archived_paths
                }]
            }]
        }))
        .unwrap()
    }

    /// A roots-only family shaped like the codebuff channels: every declared
    /// root is a sibling beside the others and the entry names no primary
    /// `paths` store at all — the all-roots shape antigravity and codebuff
    /// have always had. DSK-1's measured-zero sibling rule is scoped away
    /// from this shape, so its aggregation must not change semantics here.
    fn all_roots_channel_registry() -> HarnessRegistry {
        let platform_key = current_platform();
        let stable = serde_json::json!({
            "template": "~/.synthetic-manicode/stable/",
            "format": "jsonl",
            "confidence": "measured-locally",
            "source": "synthetic fixture"
        });
        let dev = serde_json::json!({
            "template": "~/.synthetic-manicode/dev/",
            "format": "jsonl",
            "confidence": "measured-locally",
            "source": "synthetic fixture"
        });
        let stable_paths = serde_json::json!({ platform_key: stable });
        let dev_paths = serde_json::json!({ platform_key: dev });
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{
                "id": "codebuff",
                "display_name": "synthetic",
                "source_roots": [
                    { "id": "codebuff", "paths": stable_paths },
                    { "id": "codebuff-dev", "paths": dev_paths }
                ]
            }]
        }))
        .unwrap()
    }

    fn synthetic_grok_registry() -> HarnessRegistry {
        let cell = serde_json::json!({
            "template": "~/.grok/sessions/session_search.sqlite",
            "format": "sqlite",
            "confidence": "measured-locally",
            "source": "synthetic fixture",
            "sql_table": "session_docs",
            "sql_id_column": "session_id",
            "sql_required_columns": ["session_id", "updated_at"],
            "sql_time_column": "updated_at",
            "sql_time_value_is_seconds": true
        });
        let paths = match current_platform() {
            "macos" => serde_json::json!({"macos": cell}),
            "linux" => serde_json::json!({"linux": cell}),
            "windows" => serde_json::json!({"windows": cell}),
            platform => panic!("unexpected platform: {platform}"),
        };
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{"id": "grok", "display_name": "Grok CLI fixture", "paths": paths}]
        }))
        .unwrap()
    }

    #[test]
    fn multi_root_registry_deduplicates_native_ids_and_keeps_weakest_confidence() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        let specs = [
            (
                "antigravity-cli",
                "cli",
                "measured-locally",
                &["overlap-uuid", "cli-only"][..],
            ),
            (
                "antigravity-ide",
                "ide",
                "source-confirmed",
                &["overlap-uuid"][..],
            ),
            // A declared but unascertained root makes the combined probe
            // unknown and must keep the weakest confidence, even though the
            // other roots were scanned successfully.
            ("antigravity-app", "app", "unascertained", &["app-only"][..]),
        ];
        let cell = |template: String, confidence: &str| {
            serde_json::json!({
                "template": template,
                "format": "jsonl",
                "confidence": confidence,
                "source": "synthetic fixture",
                "session_dir": { "pattern": "*", "file": ".system_generated/logs/transcript.jsonl" }
            })
        };
        let mut roots = Vec::new();
        for (root_name, surface, confidence, sessions) in specs {
            for session in sessions {
                let path = sandbox
                    .home()
                    .join(".gemini")
                    .join(root_name)
                    .join("brain")
                    .join(session)
                    .join(".system_generated/logs/transcript.jsonl");
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, b"{\"created_at\":\"2026-10-01T10:00:00Z\",\"type\":\"USER_INPUT\",\"content\":\"synthetic\"}\n").unwrap();
            }
            let template = format!("~/.gemini/{root_name}/brain/");
            let paths = match current_platform() {
                "macos" => serde_json::json!({"macos": cell(template, confidence)}),
                "linux" => serde_json::json!({"linux": cell(template, confidence)}),
                "windows" => serde_json::json!({"windows": cell(template, confidence)}),
                platform => panic!("unexpected platform: {platform}"),
            };
            roots.push(serde_json::json!({
                "id": root_name,
                "paths": paths,
                "provenance": { "surface": [surface] }
            }));
        }
        let registry: HarnessRegistry = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{ "id": "google-antigravity", "display_name": "fixture", "source_roots": roots }]
        })).unwrap();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let probe = &report.probes[0];
        let unique: std::collections::BTreeSet<_> = report
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect();
        assert_eq!(
            unique.len(),
            2,
            "overlapping native UUIDs count once; probe={probe:?}; roots={:?}",
            report
                .records
                .iter()
                .map(|r| &r.absolute_path)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            report
                .records
                .iter()
                .filter(|record| record.id.ends_with("overlap-uuid"))
                .count(),
            2,
            "each source observation remains available to collection"
        );
        assert_eq!(
            probe.record_count, None,
            "the skipped root makes the total unknown"
        );
        assert_eq!(probe.state, ProbeState::Indeterminate);
        assert_eq!(
            probe.confidence,
            Confidence::Unascertained,
            "the weakest root bounds aggregate confidence"
        );
        let mut surfaces: Vec<_> = report
            .records
            .iter()
            .map(|record| record.provenance.surface[0].as_str())
            .collect();
        surfaces.sort_unstable();
        assert_eq!(surfaces, ["cli", "cli", "ide"]);
    }

    /// GB-2. Claude Code's Workflows feature writes one journal per workflow
    /// run at
    /// `~/.claude/projects/<project>/<parent-session>/subagents/workflows/wf_<run-uuid>/journal.jsonl`.
    /// The filename stem is the harness constant `journal`, identical for
    /// every run, so a stem id collapses every workflow run into a single
    /// archive slot and — worse — the raw file count standing against
    /// collect's distinct record ids read the shortfall as an unarchivable
    /// gap, blocking every run-once snapshot on the affected machine. The
    /// workflow-run directory is the structural identity of a run, so this
    /// regression says one journal per run under two different parents
    /// yields two records, two distinct ids, and no gap.
    #[test]
    fn claude_code_workflow_journals_get_one_session_id_per_workflow_run() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        let projects = sandbox.home().join(".claude").join("projects");
        let run_a = "wf_019d7a00-0000-7000-8000-00000000000a";
        let run_b = "wf_019d7b00-0000-7000-8000-00000000000b";
        for (project, parent, run) in [
            (
                "-fixture-proj-a",
                "019d7a01-0000-7000-8000-000000000001",
                run_a,
            ),
            (
                "-fixture-proj-b",
                "019d7b02-0000-7000-8000-000000000002",
                run_b,
            ),
        ] {
            let journal = projects
                .join(project)
                .join(parent)
                .join("subagents")
                .join("workflows")
                .join(run)
                .join("journal.jsonl");
            fs::create_dir_all(journal.parent().unwrap()).unwrap();
            fs::write(&journal, b"{\"synthetic\":\"workflow-journal\"}\n").unwrap();
        }
        let registry = claude_code_directory_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let ids: std::collections::BTreeSet<String> = report
            .records
            .iter()
            .map(|record| record.id.as_str().to_string())
            .collect();
        let expected: std::collections::BTreeSet<String> = [
            format!("claude-code.synthetic-machine.{run_a}"),
            format!("claude-code.synthetic-machine.{run_b}"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            report.records.len(),
            2,
            "one journal file per workflow run, each its own record; records={:?}",
            report
                .records
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            ids, expected,
            "the wf_<run-uuid> directory is the native id of a workflow journal"
        );
        assert_eq!(
            report.probes[0].record_count,
            Some(2),
            "the probe counts the sessions collect can address"
        );
        assert!(
            report.archive_gaps().is_empty(),
            "two journals with distinct ids are not an archive gap; gaps={:?}",
            report.archive_gaps()
        );
    }

    /// GB-2. The ordinary claude-code tree keeps its ids: session transcripts
    /// named `<uuid>.jsonl` and subagent transcripts named `agent-<hex>.jsonl`
    /// (at either depth — directly under `subagents/` or inside a workflow
    /// run's directory) still id by their own filename stem, on the m1max-style
    /// tree that has no workflow journals at all.
    #[test]
    fn claude_code_ordinary_tree_keeps_filename_stem_ids() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        let projects = sandbox.home().join(".claude").join("projects");
        let files = [
            "-fixture-proj-a/019d7c01-0000-7000-8000-000000000001.jsonl",
            "-fixture-proj-a/019d7c02-0000-7000-8000-000000000002/subagents/agent-0f1e2d3c4b5a6978.jsonl",
            "-fixture-proj-a/019d7c02-0000-7000-8000-000000000002/subagents/workflows/wf_019d7c03-0000-7000-8000-000000000003/agent-1122334455667788.jsonl",
            "-fixture-proj-b/019d7c04-0000-7000-8000-000000000004/subagents/agent-99aabbccddeeff00.jsonl",
        ];
        let expected: std::collections::BTreeSet<String> = files
            .iter()
            .map(|rel| {
                let stem = rel
                    .rsplit('/')
                    .next()
                    .expect("fixture path names a file")
                    .trim_end_matches(".jsonl");
                format!("claude-code.synthetic-machine.{stem}")
            })
            .collect();
        for rel in files {
            let path = projects.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"{\"synthetic\":\"ordinary\"}\n").unwrap();
        }
        let registry = claude_code_directory_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let ids: std::collections::BTreeSet<_> = report
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect();
        assert_eq!(report.records.len(), expected.len());
        assert_eq!(
            ids.len(),
            report.records.len(),
            "no two stems alias on an ordinary tree"
        );
        assert_eq!(
            ids.iter()
                .map(|id| id.to_string())
                .collect::<std::collections::BTreeSet<_>>(),
            expected,
            "ordinary sessions and agent transcripts id by filename stem"
        );
        assert_eq!(
            report.probes[0].record_count,
            Some(4),
            "the recognized count still covers every record"
        );
        assert!(
            report.archive_gaps().is_empty(),
            "an ordinary tree is gap-free; gaps={:?}",
            report.archive_gaps()
        );
    }

    /// GB-2 counting. Two files with the same filename stem in different
    /// directories alias to one session id. That is a real loss of
    /// distinctness the archive should honestly show as one session — but it
    /// is *not* an "identified but unarchivable" gap: collect consumes every
    /// record, just under one slot. The recognized count must therefore be
    /// the distinct ids on both sides of the comparison, exactly as the
    /// multi-source-root aggregation counts them.
    #[test]
    fn aliased_filename_stems_warp_the_count_but_report_no_archive_gap() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        let projects = sandbox.home().join(".claude").join("projects");
        for project in ["-fixture-proj-a", "-fixture-proj-b"] {
            let path = projects.join(project).join("dup-stem.jsonl");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"{\"synthetic\":\"aliased\"}\n").unwrap();
        }
        let registry = claude_code_directory_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        assert_eq!(report.records.len(), 2);
        let distinct: std::collections::BTreeSet<_> = report
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect();
        assert_eq!(
            distinct.len(),
            1,
            "the fixture really does alias: one stem shared by two files"
        );
        assert_eq!(
            report.probes[0].record_count,
            Some(1),
            "the recognized count is the distinct session ids collect can address"
        );
        assert!(
            report.archive_gaps().is_empty(),
            "pure id aliasing is not an archive gap: collect consumed both records; gaps={:?}",
            report.archive_gaps()
        );
    }

    /// GB-2 message. A gap with a concrete recognized count must name the
    /// actual shortfall cause — sessions the scanner could not turn into
    /// SessionRecords — and must never claim the build cannot archive the
    /// harness's store format, which was untrue whenever the difference was
    /// an artifact of aliased ids.
    #[test]
    fn archive_gap_message_names_the_shortfall_not_a_store_format_refusal() {
        let counted = ArchiveGap {
            harness_id: "claude-code".to_string(),
            display_name: "Claude Code".to_string(),
            recognized_sessions: Some(9),
            session_records: 8,
        };
        let text = format_archive_gap(&counted);
        assert!(
            text.contains("harness=claude-code")
                && text.contains("identified_sessions=9")
                && text.contains("session_records=8")
                && text.contains("source_not_collected=true"),
            "the machine-readable fields stay: {text}"
        );
        assert!(
            !text.contains("this build cannot archive this harness's store format"),
            "the message must not claim a store-format refusal: {text}"
        );
        assert!(
            text.contains("recognised more sessions than it produced SessionRecords for"),
            "the counted shape says what the shortfall is: {text}"
        );

        let unread = ArchiveGap {
            recognized_sessions: None,
            harness_id: "opencode".to_string(),
            display_name: "OpenCode".to_string(),
            session_records: 0,
        };
        let text = format_archive_gap(&unread);
        assert!(
            text.contains("identified_sessions=unknown")
                && text.contains("source_not_collected=true")
                && text.contains("no switch to enable it"),
            "the existing single-file-store shape keeps its own honest wording: {text}"
        );
    }

    /// The workflow-journal id rule is structural, so its edges are decided by
    /// the layout alone: the one matching shape is a `journal.jsonl` directly
    /// inside a `wf_<run>` directory that sits inside `subagents/workflows`.
    #[test]
    fn claude_code_journal_rule_matches_only_the_workflows_layout() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            // The measured layout: one run of Claude Code's Workflows.
            (
                "projects/proj/parent/subagents/workflows/wf_019d7d00-0000-7000-8000-00000000000d/journal.jsonl",
                "journal",
                Some("wf_019d7d00-0000-7000-8000-00000000000d"),
            ),
            // A subagent transcript inside the same run keeps the stem id.
            (
                "projects/proj/parent/subagents/workflows/wf_019d7d00-0000-7000-8000-00000000000d/agent-0f1e2d3c4b5a6978.jsonl",
                "agent-0f1e2d3c4b5a6978",
                None,
            ),
            // A journal nested one level deeper than the run directory.
            (
                "projects/proj/parent/subagents/workflows/wf_019d7d00-0000-7000-8000-00000000000d/deeper/journal.jsonl",
                "journal",
                None,
            ),
            // The workflow directory without its wf_ marker.
            (
                "projects/proj/parent/subagents/workflows/run-no-prefix/journal.jsonl",
                "journal",
                None,
            ),
            // The wf_ marker with nothing after it.
            (
                "projects/proj/parent/subagents/workflows/wf_/journal.jsonl",
                "journal",
                None,
            ),
            // Missing the workflows segment between parent and run.
            (
                "projects/proj/parent/subagents/wf_019d7d00-0000-7000-8000-00000000000d/journal.jsonl",
                "journal",
                None,
            ),
            // Missing the subagents segment above the run.
            (
                "projects/proj/parent/workflows/wf_019d7d00-0000-7000-8000-00000000000d/journal.jsonl",
                "journal",
                None,
            ),
            // The harness-generated segment names are lowercase on every
            // measured platform; a recased tree is not the measured layout
            // and its journal falls back to the stem id.
            (
                "projects/proj/parent/Subagents/workflows/wf_019d7d00-0000-7000-8000-00000000000d/journal.jsonl",
                "journal",
                None,
            ),
            (
                "projects/proj/parent/subagents/Workflows/wf_019d7d00-0000-7000-8000-00000000000d/journal.jsonl",
                "journal",
                None,
            ),
        ];
        for (path, stem, expected) in cases {
            assert_eq!(
                claude_code_workflow_journal_native_id(Path::new(path), stem).as_deref(),
                *expected,
                "layout `{path}` id must be {expected:?}"
            );
        }
    }

    /// GB-2 (W980 Low). The workflow-journal rule's layout match is
    /// exact-case: the segment names `subagents` and `workflows` are
    /// harness-generated constants, lowercase on every measured platform,
    /// and a recased tree (`Subagents`, `Workflows`) is not the measured
    /// layout. Its journal must fall back to the filename-stem id — the
    /// honest pre-W978 behavior — rather than guessing at a renamed
    /// segment, while the lowercase runs in the same tree keep their
    /// workflow-run ids, which is what makes this scan contrast the two
    /// spellings end to end.
    #[test]
    fn recased_workflow_segments_fall_back_to_the_stem_id() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        let projects = sandbox.home().join(".claude").join("projects");
        let run_a = "wf_019d7e00-0000-7000-8000-00000000000a";
        let run_b = "wf_019d7e00-0000-7000-8000-00000000000b";
        let run_c = "wf_019d7e00-0000-7000-8000-00000000000c";
        for (project, parent, segments, run) in [
            (
                "-fixture-proj-a",
                "019d7e01-0000-7000-8000-000000000001",
                "subagents/workflows",
                run_a,
            ),
            (
                "-fixture-proj-b",
                "019d7e02-0000-7000-8000-000000000002",
                "subagents/workflows",
                run_b,
            ),
            (
                "-fixture-proj-c",
                "019d7e03-0000-7000-8000-000000000003",
                "Subagents/Workflows",
                run_c,
            ),
        ] {
            let journal = projects
                .join(project)
                .join(parent)
                .join(segments)
                .join(run)
                .join("journal.jsonl");
            fs::create_dir_all(journal.parent().unwrap()).unwrap();
            fs::write(&journal, b"{\"synthetic\":\"workflow-journal\"}\n").unwrap();
        }
        let registry = claude_code_directory_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let ids: std::collections::BTreeSet<String> = report
            .records
            .iter()
            .map(|record| record.id.as_str().to_string())
            .collect();
        let expected: std::collections::BTreeSet<String> = [
            format!("claude-code.synthetic-machine.{run_a}"),
            format!("claude-code.synthetic-machine.{run_b}"),
            "claude-code.synthetic-machine.journal".to_string(),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            report.records.len(),
            3,
            "the recased tree is still scanned; every journal is a record"
        );
        assert_eq!(
            ids, expected,
            "the lowercase measured layout ids by its run directories; the recased tree falls back to the stem"
        );
        assert_eq!(
            report.probes[0].record_count,
            Some(3),
            "the fallback stem is a distinct id, so the probe still counts every session"
        );
        assert!(
            report.archive_gaps().is_empty(),
            "the fallback stem is honest, not an archive gap; gaps={:?}",
            report.archive_gaps()
        );
    }

    #[test]
    fn session_dir_supports_root_relative_multilevel_patterns() {
        let sandbox = Sandbox::new();
        let root = sandbox.root().join("sessions");
        let transcript = root.join("workspace-a/session-uuid/transcript.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, b"{}\n").unwrap();
        let scan = collect_records(
            &root,
            HarnessSource::GoogleAntigravity,
            "synthetic-machine",
            "jsonl",
            None,
            Some(&SessionDirRule {
                pattern: "*/*".to_string(),
                file: Some("transcript.jsonl".to_string()),
                file_glob: None,
                id_components: Vec::new(),
                extra_files: Vec::new(),
            }),
        );
        assert_eq!(scan.records.len(), 1);
        assert!(scan.records[0].id.ends_with(".session-uuid"));
        assert_eq!(scan.records[0].absolute_path, transcript);
    }

    #[test]
    fn multisource_platform_absence_keeps_all_metrics_unknown() {
        let platform = current_platform();
        let other = match platform {
            "macos" => "linux",
            "linux" => "windows",
            "windows" => "macos",
            _ => unreachable!(),
        };
        let registry: HarnessRegistry = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{
                "id": "google-antigravity",
                "display_name": "fixture",
                "source_roots": [{
                    "id": "only-other-platform",
                    "paths": { (other): {"template": "/unused", "format": "jsonl", "confidence": "measured-locally"} },
                    "provenance": {"surface": ["app"]}
                }]
            }]
        })).unwrap();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic").unwrap();
        let probe = &report.probes[0];
        assert_eq!(probe.state, ProbeState::SkipWrongPlatform);
        assert_eq!(probe.record_count, None);
        assert_eq!(probe.unreadable_count, None);
        assert_eq!(probe.bytes, None);
    }

    #[test]
    fn canonical_override_is_not_reused_for_every_source_root() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        let cell = |id: &str, template: &str, surface: &str| {
            let one = serde_json::json!({
                "template": template,
                "format": "jsonl",
                "confidence": "measured-locally",
                "session_dir": {"pattern": "*", "file": "transcript.jsonl"}
            });
            let paths = match current_platform() {
                "macos" => serde_json::json!({"macos": one}),
                "linux" => serde_json::json!({"linux": one}),
                "windows" => serde_json::json!({"windows": one}),
                other => panic!("unexpected platform {other}"),
            };
            serde_json::json!({"id": id, "paths": paths, "provenance": {"surface": [surface]}})
        };
        let registry: HarnessRegistry = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{
                "id": "google-antigravity",
                "display_name": "fixture",
                "source_roots": [
                    cell("cli", "~/cli", "cli"),
                    cell("app", "~/app", "app")
                ]
            }]
        }))
        .unwrap();
        let config = Config {
            harness_roots: [(
                "google-antigravity".to_string(),
                sandbox.home().to_string_lossy().into_owned(),
            )]
            .into_iter()
            .collect(),
            ..Config::default()
        };
        let report = scan_with_registry_and_machine(&config, &registry, "synthetic").unwrap();
        let probe = &report.probes[0];
        assert_eq!(probe.state, ProbeState::Indeterminate);
        assert_eq!(probe.record_count, None);
        assert!(report.records.is_empty());
        assert!(probe.note.contains("ambiguous"));
    }

    #[test]
    fn partial_per_root_override_keeps_default_for_other_roots() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        let cell = |template: &str, surface: &str| {
            let one = serde_json::json!({
                "template": template,
                "format": "jsonl",
                "confidence": "measured-locally",
                "session_dir": {"pattern": "*", "file": "transcript.jsonl"}
            });
            let paths = match current_platform() {
                "macos" => serde_json::json!({"macos": one}),
                "linux" => serde_json::json!({"linux": one}),
                "windows" => serde_json::json!({"windows": one}),
                other => panic!("unexpected platform {other}"),
            };
            serde_json::json!({"id": surface, "paths": paths, "provenance": {"surface": [surface]}})
        };
        let registry: HarnessRegistry = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{
                "id": "google-antigravity",
                "display_name": "fixture",
                "source_roots": [cell("~/.gemini/cli/", "cli"), cell("~/.gemini/app/", "app")]
            }]
        }))
        .unwrap();
        let cli_file = sandbox
            .home()
            .join(".gemini/cli/session-cli/transcript.jsonl");
        let default_app = sandbox
            .home()
            .join(".gemini/app/session-default/transcript.jsonl");
        let override_root = sandbox.home().join("synthetic-override");
        let override_app = override_root.join("session-app/transcript.jsonl");
        for path in [&cli_file, &default_app, &override_app] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"{}\n").unwrap();
        }
        let config = Config {
            harness_roots: [(
                "google-antigravity:app".to_string(),
                override_root.to_string_lossy().into_owned(),
            )]
            .into_iter()
            .collect(),
            ..Config::default()
        };
        let report = scan_with_registry_and_machine(&config, &registry, "synthetic").unwrap();
        assert_eq!(report.records.len(), 2);
        assert!(report
            .records
            .iter()
            .any(|row| row.absolute_path == cli_file));
        assert!(report
            .records
            .iter()
            .any(|row| row.absolute_path == override_app));
        assert!(!report
            .records
            .iter()
            .any(|row| row.absolute_path == default_app));
    }

    /// A claude-code-shaped sandbox home for the DSK-1 fixtures: the real
    /// Cowork layout, nested three levels under the given desktop root,
    /// with the sandbox's own noise (audit log, uploads, config-clone
    /// history) planted beside the one true transcript. Returns the
    /// transcript path and the sid it is named by. The root is a parameter
    /// so the registry tests can spell the real app-support path and the
    /// unit test can root the same tree anywhere.
    fn plant_cowork_session(store_root: &Path) -> (PathBuf, &'static str) {
        let sid = "019d5c02-0000-7000-8000-0000000000d2";
        let account = store_root.join("space-fixture").join("account-fixture");
        fs::create_dir_all(&account).unwrap();
        let home_dir_name = "local_019d5c00-0000-7000-8000-0000000000b3";
        let session_home = account.join(home_dir_name);
        // The metadata json the app keeps beside the sandbox: fields the
        // reader may use for title/time only, never identity.
        fs::write(
            account.join(format!("{home_dir_name}.json")),
            format!(r#"{{"cliSessionId":"{sid}","title":"synthetic fixture title"}}"#).as_bytes(),
        )
        .unwrap();
        let transcript = session_home
            .join(".claude")
            .join("projects")
            .join("-Users-fixture-desktop")
            .join(format!("{sid}.jsonl"));
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, b"{\"synthetic\":\"cowork-transcript\"}\n").unwrap();
        for rel in [
            "audit.jsonl",
            "uploads/upload.jsonl",
            ".claude/history.jsonl",
            // A jsonl directly under projects/ (no sanitized-cwd layer) is
            // not the measured session shape, and must not count as one.
            ".claude/projects/loose-notes.jsonl",
        ] {
            let noise = session_home.join(rel);
            fs::create_dir_all(noise.parent().unwrap()).unwrap();
            fs::write(&noise, b"{\"synthetic\":\"noise\"}\n").unwrap();
        }
        (transcript, sid)
    }

    /// DSK-1 (ADR-059 draft D1/D2/D3). The desktop Cowork store is a second
    /// source root of the claude-code harness, declared beside the entry's
    /// primary `paths`. The scan must keep both stores: the primary CLI root
    /// keeps producing its records exactly as before — a harness that
    /// declares source_roots must not lose the store its `paths` names — and
    /// the desktop root contributes its sandbox transcripts, each id'd by
    /// its own jsonl filename and carrying the `desktop-cowork` surface
    /// observation, with the sandbox's noise and the metadata json counted
    /// as nothing.
    #[test]
    fn desktop_cowork_root_scans_sandbox_transcripts_beside_the_cli_store() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());

        let cli_sid = "019d5c01-0000-7000-8000-0000000000c1";
        let cli_file = sandbox
            .home()
            .join(".claude/projects/-fixture-proj")
            .join(format!("{cli_sid}.jsonl"));
        fs::create_dir_all(cli_file.parent().unwrap()).unwrap();
        fs::write(&cli_file, b"{\"synthetic\":\"cli-session\"}\n").unwrap();

        let desktop_store = sandbox
            .home()
            .join("Library/Application Support/Claude/local-agent-mode-sessions");
        let (transcript, cowork_sid) = plant_cowork_session(&desktop_store);

        let registry = claude_code_cli_plus_desktop_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let ids: std::collections::BTreeSet<String> = report
            .records
            .iter()
            .map(|record| record.id.as_str().to_string())
            .collect();
        let expected: std::collections::BTreeSet<String> = [
            format!("claude-code.synthetic-machine.{cli_sid}"),
            format!("claude-code.synthetic-machine.{cowork_sid}"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            ids, expected,
            "the primary CLI root and the desktop root each contribute their sessions; records={:?}",
            report
                .records
                .iter()
                .map(|record| (record.id.as_str(), record.absolute_path.clone()))
                .collect::<Vec<_>>()
        );
        let desktop = report
            .records
            .iter()
            .find(|record| record.id.ends_with(cowork_sid))
            .expect("the cowork record exists");
        assert_eq!(
            desktop.provenance.surface,
            ["desktop-cowork"],
            "the desktop root's declared observation rides the record"
        );
        assert_eq!(
            desktop.absolute_path, transcript,
            "the record is the sandbox transcript, not the metadata json"
        );
        assert!(
            report.records.iter().all(|record| record
                .absolute_path
                .file_name()
                .map(|name| name != "audit.jsonl")
                .unwrap_or(true)),
            "the sandbox's audit log is not a session; noise must not count"
        );
        let probe = &report.probes[0];
        assert_eq!(probe.state, ProbeState::Scanned);
        assert_eq!(
            probe.record_count,
            Some(2),
            "the aggregate count spans the primary and the desktop root"
        );
    }

    /// DSK-1 (ADR-059 draft D4). Codex `thread/archive` moves a rollout from
    /// `sessions/` to `archived_sessions/` and `thread/unarchive` moves it
    /// back; the archived directory is therefore a *movable* source root of
    /// the same harness. The rollout's id comes from its filename, so the
    /// move must not fork the archive-holding identity: the id set before,
    /// between and after the archive round-trip is the same one session.
    #[test]
    fn codex_archive_move_and_unarchive_keep_one_session_id() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());

        let sid = "rollout-2026-09-01T10-00-00-019bf00d-0000-7000-8000-00000000000a";
        let active = sandbox
            .home()
            .join(".codex/sessions/2026/09/01")
            .join(format!("{sid}.jsonl"));
        let archived = sandbox
            .home()
            .join(".codex/archived_sessions/2026/09/01")
            .join(format!("{sid}.jsonl"));
        fs::create_dir_all(active.parent().unwrap()).unwrap();
        fs::write(&active, b"{\"synthetic\":\"rollout\"}\n").unwrap();
        // Both roots are planted, though the aggregate rules below would
        // tolerate the archived one being absent (a scanned primary keeps
        // the family's total known): this test is about the id stability
        // of the archive move, not the family's aggregate counting rules.
        fs::create_dir_all(archived.parent().unwrap()).unwrap();

        let registry = codex_plus_archived_registry();
        let scan_ids = |report: &ScanReport| -> Vec<String> {
            let mut ids: Vec<String> = report
                .records
                .iter()
                .map(|record| record.id.as_str().to_string())
                .collect();
            ids.sort();
            ids
        };
        let expected_id = format!("codex.synthetic-machine.{sid}");

        let before =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        assert_eq!(
            scan_ids(&before),
            std::slice::from_ref(&expected_id),
            "the active rollout is scanned before the archive move"
        );
        assert_eq!(before.probes[0].record_count, Some(1));

        // thread/archive: the rollout moves into the archived directory.
        fs::create_dir_all(archived.parent().unwrap()).unwrap();
        fs::rename(&active, &archived).unwrap();
        let archived_report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        assert_eq!(
            scan_ids(&archived_report),
            std::slice::from_ref(&expected_id),
            "the archived root re-scans the same session: the move must not fork the id"
        );
        assert_eq!(
            archived_report.records[0].absolute_path, archived,
            "the record follows the file to its new root"
        );

        // thread/unarchive: the rollout moves back; a rescan of the same
        // tree is also idempotent — scanning twice changes nothing.
        fs::rename(&archived, &active).unwrap();
        let back =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        assert_eq!(
            scan_ids(&back),
            [expected_id],
            "the id survives the round trip"
        );
        let rescan =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        assert_eq!(
            scan_ids(&rescan),
            scan_ids(&back),
            "a repeated scan is idempotent"
        );
    }

    /// DSK-1. Overriding the canonical `harness_roots["claude-code"]` key
    /// keeps its historical meaning once source_roots exist: it names the
    /// entry's primary store, and does not become an ambiguous
    /// every-root-override the way it would for a harness whose registry
    /// cells are all source_roots.
    #[test]
    fn canonical_override_keeps_naming_the_primary_store_when_source_roots_exist() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());

        let desktop_store = sandbox
            .home()
            .join("Library/Application Support/Claude/local-agent-mode-sessions");
        let (_, cowork_sid) = plant_cowork_session(&desktop_store);
        let moved_projects = sandbox.home().join("moved-projects");
        let cli_sid = "019d5c03-0000-7000-8000-0000000000c3";
        let moved_cli = moved_projects
            .join("-fixture-proj")
            .join(format!("{cli_sid}.jsonl"));
        fs::create_dir_all(moved_cli.parent().unwrap()).unwrap();
        fs::write(&moved_cli, b"{\"synthetic\":\"moved-cli-session\"}\n").unwrap();

        let config = Config {
            harness_roots: [(
                "claude-code".to_string(),
                moved_projects.to_string_lossy().into_owned(),
            )]
            .into_iter()
            .collect(),
            ..Config::default()
        };
        let registry = claude_code_cli_plus_desktop_registry();
        let report =
            scan_with_registry_and_machine(&config, &registry, "synthetic-machine").unwrap();
        let probe = &report.probes[0];
        assert_ne!(
            probe.state,
            ProbeState::Indeterminate,
            "the canonical override is not ambiguous once it names the primary store; note={}",
            probe.note
        );
        let ids: std::collections::BTreeSet<String> = report
            .records
            .iter()
            .map(|record| record.id.as_str().to_string())
            .collect();
        assert!(
            ids.contains(&format!("claude-code.synthetic-machine.{cli_sid}")),
            "the primary root follows the canonical override; ids={ids:?}"
        );
        assert!(
            ids.contains(&format!("claude-code.synthetic-machine.{cowork_sid}")),
            "the desktop root still scans at its declared default; ids={ids:?}"
        );
    }

    /// DSK-1. A declared desktop root that is simply NOT THERE on this
    /// machine (no Claude Desktop app installed) must not dim the harness's
    /// known CLI count to an unknown: the root was looked for and found
    /// absent, which is a measured zero contribution once the primary root
    /// anchored the family by being scanned. The doctor's footprint row
    /// counts records directly, so an aggregate that refused to claim what
    /// the records already count is the exact two-table contradiction
    /// `doctor_consistency_test` exists to catch.
    #[test]
    fn an_absent_declared_root_keeps_the_scanned_primary_count_known() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());

        let cli_sid = "019d5c04-0000-7000-8000-0000000000c4";
        let cli_file = sandbox
            .home()
            .join(".claude/projects/-fixture-proj")
            .join(format!("{cli_sid}.jsonl"));
        fs::create_dir_all(cli_file.parent().unwrap()).unwrap();
        fs::write(&cli_file, b"{\"synthetic\":\"cli-session\"}\n").unwrap();
        // The desktop root is deliberately NOT planted: the machine has no
        // Claude Desktop install, which is the ordinary case.

        let registry = claude_code_cli_plus_desktop_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let probe = &report.probes[0];
        assert_eq!(
            report.records.len(),
            1,
            "the CLI record is collected either way; records={:?}",
            report
                .records
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            probe.record_count,
            Some(1),
            "a scanned primary plus an absent sibling root is a known total, not an unknown; note={}",
            probe.note
        );
        assert_eq!(probe.state, ProbeState::Scanned);

        // And the symmetric shape — only the desktop store, no CLI store —
        // keeps the desktop session countable the same way.
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        let desktop_store = sandbox
            .home()
            .join("Library/Application Support/Claude/local-agent-mode-sessions");
        let (transcript, desktop_sid) = plant_cowork_session(&desktop_store);
        let _ = transcript;
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let probe = &report.probes[0];
        assert_eq!(
            probe.record_count,
            Some(1),
            "the absent CLI root contributes a known zero to the desktop count; note={}",
            probe.note
        );
        assert!(
            report
                .records
                .iter()
                .any(|record| record.id.ends_with(desktop_sid)),
            "the desktop session itself was collected"
        );

        // But a family whose roots were ALL absent stays unknown — an
        // absent store is not an empty one.
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let probe = &report.probes[0];
        assert_eq!(probe.state, ProbeState::Missing);
        assert_eq!(
            probe.record_count, None,
            "an all-missing family is unknown, never a semantic zero"
        );
    }

    /// W1006d — ADR-059 draft D6, the one required change of the external
    /// review: an *unverified* additional root (one the scan never looked
    /// at — a cell the registry will not vouch for) must not make an
    /// already-measured CLI total disappear. The Windows shape of the
    /// shipped registry is exactly this family: CLI store scanned, desktop
    /// Cowork root `unascertained`. The aggregate must report the measured
    /// roots' count as a QUALIFIED LOWER BOUND — `record_count` set to what
    /// the scan measured, the unverified sibling named in
    /// `unverified_roots`, `status`/`doctor` rendering it `≥N` — never the
    /// blanket unknown the pre-W1006d rule produced (the rejected D6 side
    /// effect), never a bare number that silently claims the family total.
    #[test]
    fn an_unverified_sibling_qualifies_the_measured_total_as_a_lower_bound() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());

        let cli_sid = "019d5c05-0000-7000-8000-0000000000c5";
        let cli_file = sandbox
            .home()
            .join(".claude/projects/-fixture-proj")
            .join(format!("{cli_sid}.jsonl"));
        fs::create_dir_all(cli_file.parent().unwrap()).unwrap();
        fs::write(&cli_file, b"{\"synthetic\":\"cli-session\"}\n").unwrap();
        // The desktop root's cell is `unascertained`, so the machine's real
        // desktop directory — planted or not — is never looked at; plant
        // nothing, which is the ordinary machine that has the CLI only.

        let registry = claude_code_cli_plus_unverified_desktop_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let probe = &report.probes[0];
        assert_eq!(
            report.records.len(),
            1,
            "the CLI record is collected either way; records={:?}",
            report
                .records
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            probe.state,
            ProbeState::Scanned,
            "the measured primary keeps the family available; note={}",
            probe.note
        );
        assert_eq!(
            probe.record_count,
            Some(1),
            "the measured CLI count is reported, not erased by the unverified sibling; note={}",
            probe.note
        );
        assert_eq!(
            probe
                .unverified_roots
                .iter()
                .map(|root| (root.id.as_str(), root.state))
                .collect::<Vec<_>>(),
            vec![("claude-desktop-cowork", ProbeState::SkipUnascertained)],
            "the unverified root is named so the count reads as a lower bound"
        );
        assert!(
            probe.note.contains("lower bound") && probe.note.contains("claude-desktop-cowork"),
            "the note qualifies the total and names the root; note={}",
            probe.note
        );

        // The same family with the CLI store absent as well: the primary
        // root was still MEASURED (found absent), so the rule that only an
        // all-unknown family stays unknown does not bite — the total is the
        // measured zero, still qualified by the unverified sibling
        // (`≥0`), never an unqualified 0.
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let probe = &report.probes[0];
        assert_eq!(
            probe.state,
            ProbeState::Missing,
            "the measured primary was found absent; note={}",
            probe.note
        );
        assert_eq!(
            probe.record_count,
            Some(0),
            "the measured absence is kept as the lower bound's floor, but the mark must \
             keep it from reading as an exact total; note={}",
            probe.note
        );
        assert_eq!(
            probe.unverified_roots.len(),
            1,
            "the unverified sibling still qualifies the missing family's total; note={}",
            probe.note
        );
    }

    /// W1009/DSK-3: doctor D3 prints one row per source root, so the probe
    /// carries one [`SourceRootProbe`] per declared, platform-relevant root
    /// beside the aggregate — the primary first (named by the canonical
    /// harness id, marked `primary`), then every additional root by its own
    /// registry id, each with its own walk's count and bytes. The rows are
    /// the roots' own scopes: a root's count is exact, and the aggregate is
    /// where the D6 lower-bound marking stays.
    #[test]
    fn a_primary_plus_roots_family_carries_one_row_per_source_root() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());

        let cli_file = sandbox
            .home()
            .join(".claude/projects/-fixture-proj")
            .join("019d5c07-0000-7000-8000-0000000000c7.jsonl");
        fs::create_dir_all(cli_file.parent().unwrap()).unwrap();
        fs::write(&cli_file, b"{\"synthetic\":\"cli-session\"}\n").unwrap();
        let desktop_store = sandbox
            .home()
            .join("Library/Application Support/Claude/local-agent-mode-sessions");
        plant_cowork_session(&desktop_store);

        let registry = claude_code_cli_plus_desktop_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let probe = &report.probes[0];
        assert_eq!(probe.source_roots.len(), 2, "one row per declared root");
        let (primary, cowork) = (&probe.source_roots[0], &probe.source_roots[1]);
        assert!(
            primary.primary,
            "the first row is the family's primary store"
        );
        assert_eq!(primary.id, "claude-code");
        assert_eq!(primary.state, ProbeState::Scanned);
        assert_eq!(primary.record_count, Some(1));
        assert_eq!(
            primary.total_bytes,
            Some(cli_file.metadata().unwrap().len()),
            "a scanned root's byte census is the sum of its records' sizes"
        );
        assert!(
            !cowork.primary,
            "the desktop root is an additional source root, not the primary"
        );
        assert_eq!(cowork.id, "claude-desktop-cowork");
        assert_eq!(cowork.state, ProbeState::Scanned);
        assert_eq!(cowork.record_count, Some(1));
        assert!(
            cowork.total_bytes.is_some_and(|bytes| bytes > 0),
            "the desktop root's own walk measured its own bytes"
        );
    }

    /// W1009/DSK-3: an unverified root keeps a row of its own — the whole
    /// point of the per-root breakdown is that a root the scan never looked
    /// at cannot be folded into a number that reads as the family total.
    /// The row carries the skip state and no fabricated count or bytes,
    /// while the aggregate keeps the W1006d qualified lower bound.
    #[test]
    fn an_unverified_source_root_keeps_its_own_marked_row() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());

        let cli_file = sandbox
            .home()
            .join(".claude/projects/-fixture-proj")
            .join("019d5c08-0000-7000-8000-0000000000c8.jsonl");
        fs::create_dir_all(cli_file.parent().unwrap()).unwrap();
        fs::write(&cli_file, b"{\"synthetic\":\"cli-session\"}\n").unwrap();

        let registry = claude_code_cli_plus_unverified_desktop_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let probe = &report.probes[0];
        assert_eq!(probe.source_roots.len(), 2);
        let (primary, cowork) = (&probe.source_roots[0], &probe.source_roots[1]);
        assert_eq!(primary.state, ProbeState::Scanned);
        assert_eq!(primary.record_count, Some(1));
        assert_eq!(
            (cowork.id.as_str(), cowork.state),
            ("claude-desktop-cowork", ProbeState::SkipUnascertained),
            "the unverified root's row says why it never measured"
        );
        assert_eq!(
            cowork.record_count, None,
            "a root nobody looked at carries no count, not a zero"
        );
        assert_eq!(
            cowork.total_bytes, None,
            "a root nobody looked at carries no byte census either"
        );
    }

    /// W1009/DSK-3: a family whose every root was looked for and found
    /// absent still carries its rows — each a measured absence (`0`, the
    /// state saying it) — while the aggregate keeps the W1006c "all absent
    /// is unknown, not empty" rule.
    #[test]
    fn an_all_absent_family_carries_measured_absent_rows() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());

        let registry = claude_code_cli_plus_desktop_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let probe = &report.probes[0];
        assert_eq!(probe.record_count, None, "all absent stays unknown");
        assert_eq!(probe.source_roots.len(), 2);
        for row in &probe.source_roots {
            assert_eq!(
                (row.state, row.record_count, row.total_bytes),
                (ProbeState::Missing, None, None),
                "each missing root's row is the measured absence itself; id={}",
                row.id
            );
        }
    }

    /// W1009/DSK-3: the rows are scoped to the D6 primary-plus-roots shape.
    /// An all-roots family (codebuff's channels, antigravity's surfaces)
    /// keeps the aggregate surface it has always had — no rows — so this
    /// change widens no family's output it was not scoped for.
    #[test]
    fn an_all_roots_family_carries_no_source_root_rows() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());

        let registry = all_roots_channel_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let probe = &report.probes[0];
        assert!(
            probe.source_roots.is_empty(),
            "all-roots families keep the aggregate-only surface"
        );
    }

    /// W1006d — the other half of the rule: only when ALL roots of the
    /// family stayed unverified does the combined total stay unknown. No
    /// root was measured, so nothing anchors a lower bound and the
    /// aggregate keeps the pre-DSK-1 unknown — with no unverified-roots
    /// marking, because there is no measured count for it to qualify.
    #[test]
    fn a_family_whose_roots_all_stayed_unknown_keeps_the_total_unknown() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());

        let registry = claude_code_all_unverified_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        let probe = &report.probes[0];
        assert_eq!(probe.state, ProbeState::Indeterminate);
        assert_eq!(
            probe.record_count, None,
            "no root of the family was measured, so the total is unknown — a lower bound \
             needs a measured floor; note={}",
            probe.note
        );
        assert!(
            probe.unverified_roots.is_empty(),
            "an unknown total carries no lower-bound marking; there is no measured count \
             for it to qualify; note={}",
            probe.note
        );
        assert_eq!(
            probe.note,
            "one or more declared source roots could not be fully checked; total is unknown"
        );

        // And the sub-case where every root was skipped by the SAME policy
        // reason: the aggregate keeps the precise `skip unascertained`
        // word rather than collapsing it into indeterminate.
        let both_unascertained = claude_code_all_unverified_registry_with_unascertained_primary();
        let report = scan_with_registry_and_machine(
            &Config::default(),
            &both_unascertained,
            "synthetic-machine",
        )
        .unwrap();
        let probe = &report.probes[0];
        assert_eq!(
            probe.state,
            ProbeState::SkipUnascertained,
            "an all-unascertained family keeps the precise skip word"
        );
        assert_eq!(
            probe.record_count, None,
            "the family total stays unknown when every root stayed unknown"
        );
        assert!(probe.unverified_roots.is_empty());
    }

    /// W1006c (review of DSK-1): the measured-zero sibling rule is scoped to
    /// the primary-plus-additional-roots shape ADR-059 draft D6 introduced,
    /// and does not leak into the all-roots families that predate it. A
    /// channel family (codebuff stable/dev/staging, antigravity's surfaces)
    /// has no primary store that anchors it, so a scanned channel plus a
    /// looked-for-but-absent sibling keeps the family's *combined* total
    /// unknown — exactly the pre-DSK-1 rule, restored for these entries so
    /// their aggregate semantics do not change under an unrelated harness's
    /// desktop-store work.
    #[test]
    fn an_absent_sibling_in_an_all_roots_family_keeps_the_total_unknown() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());

        let stable = sandbox
            .home()
            .join(".synthetic-manicode/stable")
            .join("probe-stable-01.jsonl");
        fs::create_dir_all(stable.parent().unwrap()).unwrap();
        fs::write(&stable, b"{\"synthetic\":\"channel-session\"}\n").unwrap();
        // The dev-channel root is deliberately NOT planted: looked for and
        // absent, the ordinary machine that never ran a dev build.

        let registry = all_roots_channel_registry();
        let report =
            scan_with_registry_and_machine(&Config::default(), &registry, "synthetic-machine")
                .unwrap();
        assert_eq!(
            report.records.len(),
            1,
            "the scanned channel's record is collected either way"
        );
        let probe = &report.probes[0];
        assert_eq!(
            probe.state,
            ProbeState::Scanned,
            "the family is available through its scanned channel"
        );
        assert_eq!(
            probe.record_count, None,
            "an all-roots family keeps the pre-DSK-1 rule: an absent sibling root leaves the combined total unknown — no primary store anchors this family, so the measured-zero rule does not apply; note={}",
            probe.note
        );
    }

    /// A `file_glob` session-directory rule selects only the transcripts the
    /// declared glob names and takes each record's native id from the matched
    /// file's own stem — the metadata json beside the sandbox, the audit log,
    /// the uploads and the config-clone files are not sessions, and a
    /// mismatched depth is not either.
    #[test]
    fn session_dir_file_glob_takes_the_id_from_the_transcript_filename() {
        let sandbox = Sandbox::new();
        let root = sandbox.root().join("lam");
        let (transcript, sid) = plant_cowork_session(&root);
        let scan = collect_records(
            &root,
            HarnessSource::ClaudeCode,
            "synthetic-machine",
            "jsonl",
            None,
            Some(&SessionDirRule {
                pattern: "*/*/local_*".to_string(),
                file: None,
                file_glob: Some(".claude/projects/*/*.jsonl".to_string()),
                id_components: Vec::new(),
                extra_files: Vec::new(),
            }),
        );
        assert_eq!(
            scan.records.len(),
            1,
            "only the sandbox transcript counts; records={:?}",
            scan.records
                .iter()
                .map(|record| record.absolute_path.clone())
                .collect::<Vec<_>>()
        );
        assert!(scan.records[0].id.ends_with(sid));
        assert_eq!(scan.records[0].absolute_path, transcript);
        assert_eq!(
            scan.records[0].byte_size,
            transcript.metadata().unwrap().len()
        );
    }

    /// A session-directory rule must declare exactly one transcript target:
    /// the literal `file` or the `file_glob`. Both at once is ambiguous data,
    /// neither is a rule that can never match, and neither may reach the
    /// walk (a registry is data anyone can edit; it is validated at load so
    /// the defect is a load error, not an empty scan).
    #[test]
    fn session_dir_rules_declare_exactly_one_transcript_target() {
        let cell = |session_dir: &str| {
            let platform = current_platform();
            format!(
                r#"{{ "schema_version": 1, "generated": "synthetic fixture",
                      "harnesses": [
                        {{ "id": "claude-code", "display_name": "fixture",
                           "paths": {{ "{platform}": {{
                             "template": "/fixture", "format": "jsonl",
                             "confidence": "measured-locally",
                             "source": "synthetic fixture",
                             "session_dir": {session_dir} }}}}
                        }} ]}}"#
            )
        };
        let neither = cell(r#"{ "pattern": "*" }"#);
        let err = parse_registry(neither.as_str(), "neither fixture")
            .expect_err("a rule with no transcript target must not load");
        assert!(
            err.to_string().contains("exactly one"),
            "the load error must say what to fix: {err}"
        );

        let both = cell(
            r#"{ "pattern": "*", "file": "transcript.jsonl",
                 "file_glob": "*/*.jsonl" }"#,
        );
        let err = parse_registry(both.as_str(), "both fixture")
            .expect_err("a rule with two transcript targets must not load");
        assert!(
            err.to_string().contains("exactly one"),
            "the load error must say what to fix: {err}"
        );

        let glob_only = cell(r#"{ "pattern": "*", "file_glob": "*/*.jsonl" }"#);
        parse_registry(glob_only.as_str(), "glob fixture")
            .expect("a file_glob rule without a literal file loads");

        let literal_only = cell(r#"{ "pattern": "*", "file": "transcript.jsonl" }"#);
        parse_registry(literal_only.as_str(), "literal fixture")
            .expect("a literal-file rule without a glob loads");
    }

    /// DSK-1 (ADR-059 draft D5). The shipped registry declares the desktop
    /// transcript retention probe, in the shape the later doctor wiring will
    /// read: the settings key, its three integer states, the longer-of-two
    /// interaction, the managed-override reporting duty, and — only where
    /// the read locations were measured — the user and managed files to
    /// read. An unmeasured platform declares no managed files and says why,
    /// because an empty measurement is a fact and a guessed path is not.
    #[test]
    fn registry_declares_the_desktop_session_cleanup_retention_probe() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let previous = env::var_os(REGISTRY_ENV);
        env::remove_var(REGISTRY_ENV);

        let registry = load_registry_from_repo().expect("the shipped registry must load");
        let claude = registry
            .harnesses
            .iter()
            .find(|h| h.id == "claude-code")
            .expect("the shipped registry must carry claude-code");
        let probe = claude
            .retention_probes
            .iter()
            .find(|probe| probe.key == "desktopSessionCleanupPeriodDays")
            .expect("claude-code must declare the desktop transcript retention probe");
        assert_eq!(probe.id, "desktop-session-cleanup");
        for state in [
            &probe.states.missing,
            &probe.states.zero,
            &probe.states.positive_int,
        ] {
            assert!(
                !state.is_empty(),
                "every integer state is recorded: {probe:?}"
            );
        }
        assert!(
            probe.states.missing.contains("0"),
            "a missing key defaults to 0 (unbounded), which is NOT the cleanupPeriodDays default of 30 days"
        );
        assert!(
            probe
                .managed_override
                .to_ascii_lowercase()
                .contains("cleanupperioddays"),
            "the managed override is reported against cleanupPeriodDays: {}",
            probe.managed_override
        );
        let macos = probe
            .platforms
            .macos
            .as_ref()
            .expect("the measured platform cell exists");
        assert!(
            macos
                .user_sources
                .iter()
                .any(|path| path.ends_with(".claude/settings.json")),
            "the user settings file is where the User scope is read: {:?}",
            macos.user_sources
        );
        assert!(
            !macos.managed_sources.is_empty(),
            "a measured cell never claims 'no managed files': the managed source list is a fact"
        );
        assert!(
            macos
                .source
                .to_ascii_lowercase()
                .contains("application support"),
            "the measured cell states the desktop app-support tree is a disproved source: {}",
            macos.source
        );
        for (platform, cell, reason) in [
            (
                "linux",
                probe.platforms.linux.as_ref(),
                "the linux cell declares the user file and no measured managed list",
            ),
            (
                "windows",
                probe.platforms.windows.as_ref(),
                "the windows cell declares the user file and no measured managed list",
            ),
        ] {
            let Some(cell) = cell else {
                panic!("the {platform} cell exists with its {} state", reason)
            };
            assert!(
                cell.user_sources
                    .iter()
                    .any(|path| path.contains("settings.json")),
                "{platform}: the user settings file is declared: {:?}",
                cell.user_sources
            );
            assert!(
                cell.managed_sources.is_empty(),
                "{platform}: managed locations were not measured, so none are guessed"
            );
        }

        match previous {
            Some(value) => env::set_var(REGISTRY_ENV, value),
            None => env::remove_var(REGISTRY_ENV),
        }
    }

    /// DSK-5 (ADR-059): the shipped registry declares the surface dimension's
    /// value vocabulary, and the two values the ADR landed for claude-code
    /// — `cli` and `desktop-cowork` — are in the union a `--surface` value
    /// is validated against. The union is the whole registry's, not one
    /// harness's: `ide`/`app` (antigravity) and `web` (deepseek-harness)
    /// are declared by their own entries, and `resolve`'s refusal lists
    /// exactly this set.
    #[test]
    fn the_shipped_registry_declares_the_surface_vocabulary() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let previous = env::var_os(REGISTRY_ENV);
        env::remove_var(REGISTRY_ENV);

        let registry = load_registry_from_repo().expect("the shipped registry must load");
        let vocabulary = registry.surface_vocabulary();
        for value in ["cli", "desktop-cowork", "ide", "app", "web"] {
            assert!(
                vocabulary.contains(value),
                "the shipped registry must declare `{value}` as a surface value: {vocabulary:?}"
            );
        }

        match previous {
            Some(value) => env::set_var(REGISTRY_ENV, value),
            None => env::remove_var(REGISTRY_ENV),
        }
    }

    /// The vocabulary is a **union** with duplicates removed, taken from every
    /// harness's `dimensions.surfaces` alike: claude-code declares
    /// `desktop-cowork` where another harness declares the same `cli`, and
    /// neither harness's list shadows the other's. A scratch registry proves
    /// the fold without depending on the shipped data's exact contents.
    #[test]
    fn the_surface_vocabulary_is_the_sorted_deduplicated_union() {
        // The DSK-1 scratch registry declares no dimensions, so its union is
        // empty — this is also the proof that a harness entry without a
        // `dimensions` block contributes nothing, rather than an error.
        assert!(claude_code_cli_plus_desktop_registry()
            .surface_vocabulary()
            .is_empty());

        let registry: HarnessRegistry = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [
                {
                    "id": "claude-code",
                    "display_name": "synthetic",
                    "dimensions": {
                        "supported": ["surface"],
                        "surfaces": ["desktop-cowork", "cli"]
                    },
                    "paths": {}
                },
                {
                    "id": "google-antigravity",
                    "display_name": "synthetic",
                    "dimensions": {
                        "supported": ["surface"],
                        "surfaces": ["cli", "ide", "app"]
                    },
                    "paths": {}
                },
                {
                    "id": "codex",
                    "display_name": "synthetic",
                    "paths": {}
                }
            ]
        }))
        .unwrap();
        let expected: BTreeSet<String> = ["app", "cli", "desktop-cowork", "ide"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(registry.surface_vocabulary(), expected);
    }

    /// [`resolve_selector_flags`] reads the registry only when a surface was
    /// asked for (DSK-5: the no-flag run must behave exactly as before), so
    /// a flag-absent resolve cannot fail because of the registry at all.
    #[test]
    fn flags_without_a_surface_resolve_without_touching_the_registry() {
        let args = crate::selector::SelectorArgs {
            machine: Some("m".to_string()),
            ..Default::default()
        };
        let resolved = resolve_selector_flags(&args)
            .expect("a flag-absent resolve must not depend on the registry at all");
        assert_eq!(resolved.selector.machine.as_deref(), Some("m"));
    }

    #[test]
    fn grok_usage_json_is_scanned_and_linked_to_its_sqlite_session() {
        let sandbox = Sandbox::new();
        let sessions = sandbox.home().join(".grok/sessions");
        let db = sessions.join("session_search.sqlite");
        fs::create_dir_all(&sessions).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL, updated_at INTEGER NOT NULL, title TEXT NOT NULL, content TEXT NOT NULL, content_hash TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_docs VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                "synthetic-session-001",
                "/synthetic/work",
                1_780_000_000_i64,
                "synthetic title",
                "synthetic body",
                "synthetic hash"
            ],
        )
        .unwrap();
        drop(conn);

        // Grok's session usage is a sidecar two directories below `sessions`.
        let usage = sessions
            .join("synthetic-cwd")
            .join("synthetic-session-001")
            .join("usage.json");
        fs::create_dir_all(usage.parent().unwrap()).unwrap();
        fs::write(
            &usage,
            br#"{"modelUsage":{"grok-test":{"inputTokens":12,"cachedReadTokens":7,"outputTokens":3,"totalTokens":15}}}"#,
        )
        .unwrap();

        let config = Config {
            harness_roots: [("grok".to_string(), db.to_string_lossy().into())]
                .into_iter()
                .collect(),
            ..Config::default()
        };
        let report = scan_with_registry_and_machine(
            &config,
            &synthetic_grok_registry(),
            "synthetic-machine",
        )
        .unwrap();

        let sqlite_id = "grok.synthetic-machine.synthetic-session-001";
        assert!(report.records.iter().any(|record| record.id == sqlite_id));
        let usage_record = report
            .records
            .iter()
            .find(|record| record.absolute_path == usage)
            .expect(
                "usage.json should be emitted as a separate raw-file record linked by session id",
            );
        assert_eq!(usage_record.id, format!("{sqlite_id}.usage"));
        assert_eq!(usage_record.sqlite_layout, None);
    }

    #[test]
    fn grok_chat_history_files_are_scanned_including_sessions_without_a_row() {
        let sandbox = Sandbox::new();
        let sessions = sandbox.home().join(".grok/sessions");
        let db = sessions.join("session_search.sqlite");
        fs::create_dir_all(&sessions).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL, updated_at INTEGER NOT NULL, title TEXT NOT NULL, content TEXT NOT NULL, content_hash TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_docs VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                "synthetic-session-005",
                "/synthetic/work",
                1_780_000_000_i64,
                "synthetic title",
                "synthetic body",
                "synthetic hash"
            ],
        )
        .unwrap();
        drop(conn);

        let history_with_row = sessions
            .join("synthetic-cwd")
            .join("synthetic-session-005")
            .join("chat_history.jsonl");
        fs::create_dir_all(history_with_row.parent().unwrap()).unwrap();
        fs::write(
            &history_with_row,
            br#"{"type":"user","content":[{"type":"text","text":"synthetic user turn"}]}"#,
        )
        .unwrap();
        let history_native_only = sessions
            .join("synthetic-cwd")
            .join("synthetic-session-006")
            .join("chat_history.jsonl");
        fs::create_dir_all(history_native_only.parent().unwrap()).unwrap();
        fs::write(
            &history_native_only,
            br#"{"type":"assistant","content":"synthetic answer"}"#,
        )
        .unwrap();

        let config = Config {
            harness_roots: [("grok".to_string(), db.to_string_lossy().into())]
                .into_iter()
                .collect(),
            ..Config::default()
        };
        let report = scan_with_registry_and_machine(
            &config,
            &synthetic_grok_registry(),
            "synthetic-machine",
        )
        .unwrap();

        // One record for a session whose row exists: the shared id means the
        // transcript seals into the session's own body.
        let with_row_id = "grok.synthetic-machine.synthetic-session-005";
        assert_eq!(
            report
                .records
                .iter()
                .filter(|record| record.id == with_row_id && record.sqlite_layout.is_none())
                .count(),
            1
        );
        let history_record = report
            .records
            .iter()
            .find(|record| record.id == with_row_id && record.sqlite_layout.is_none())
            .expect("the chat_history.jsonl record shares the row session id");
        assert_eq!(history_record.absolute_path, history_with_row);

        // A session the store has no row for still yields its record: the
        // native transcript is then the session's only observation.
        assert_eq!(
            report
                .records
                .iter()
                .filter(|record| record.id == "grok.synthetic-machine.synthetic-session-006")
                .count(),
            1
        );

        // The probe note reports every population collect will archive.
        let probe = &report.probes[0];
        assert!(probe
            .note
            .contains("SessionRecord=1; usage files=0; history files=2"));
    }

    #[test]
    fn openclaw_scans_multiple_agents_and_deduplicates_sqlite_cold_archive_files() {
        let sandbox = crate::test_support::Sandbox::new();
        let agents = sandbox.root().join("agents");
        let db = agents.join("synthetic-agent/agent/openclaw-agent.sqlite");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        fs::create_dir_all(agents.join("synthetic-agent/sessions")).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_meta(agent_id TEXT);
             INSERT INTO schema_meta VALUES ('synthetic-agent');
             CREATE TABLE session_windows(session_id TEXT, session_key TEXT, started_at TEXT);
             INSERT INTO session_windows VALUES ('live-window', 'logical-session', '2026-01-01T00:00:00Z');
             CREATE TABLE transcript_events(session_id TEXT, seq INTEGER, event_json TEXT, created_at TEXT);
             INSERT INTO transcript_events VALUES ('live-window', 1, '{\"id\":\"synthetic-event\",\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"provider\":\"synthetic-provider\",\"model\":\"synthetic-model\",\"usage\":{\"input\":2}}}', '2026-01-01T00:00:01Z');
             CREATE TABLE session_transcript_archives(session_id TEXT, generation INTEGER, session_key TEXT, reason TEXT, encoding TEXT, archive_blob BLOB, archive_sha256 TEXT, archive_name TEXT, created_at TEXT, published_at TEXT);
             INSERT INTO session_transcript_archives VALUES ('cold-db', 3, 'logical-cold', 'deleted', 'zstd', X'00', '6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d', 'cold-db.jsonl.zst', '2026-01-02T00:00:00Z', NULL);",
        ).unwrap();
        drop(conn);
        fs::write(
            agents.join("synthetic-agent/sessions/cold-db.jsonl.deleted.synthetic.zst"),
            b"duplicate cold archive",
        )
        .unwrap();
        fs::write(
            agents.join("synthetic-agent/sessions/cold-file.jsonl.deleted.synthetic.zst"),
            b"unmatched cold archive",
        )
        .unwrap();
        fs::write(
            agents.join("synthetic-agent/sessions/cold-file.jsonl.deleted.synthetic-later.zst"),
            b"another generation of the same cold session",
        )
        .unwrap();
        let second_db = agents.join("synthetic-child/agent/openclaw-agent.sqlite");
        fs::create_dir_all(second_db.parent().unwrap()).unwrap();
        let second = rusqlite::Connection::open(second_db).unwrap();
        second.execute_batch(
            "CREATE TABLE schema_meta(agent_id TEXT);
             INSERT INTO schema_meta VALUES ('synthetic-child');
             CREATE TABLE session_windows(session_id TEXT, session_key TEXT, started_at TEXT);
             CREATE TABLE transcript_events(session_id TEXT, seq INTEGER, event_json TEXT, created_at TEXT);
             CREATE TABLE session_transcript_archives(session_id TEXT, generation INTEGER, session_key TEXT, reason TEXT, encoding TEXT, archive_blob BLOB, archive_sha256 TEXT, archive_name TEXT, created_at TEXT, published_at TEXT);",
        ).unwrap();
        drop(second);
        fs::create_dir_all(agents.join("synthetic-archive-only/sessions")).unwrap();
        fs::write(
            agents.join("synthetic-archive-only/sessions/cold-only.jsonl.deleted.synthetic.zst"),
            b"synthetic cold archive without database",
        )
        .unwrap();

        let registry = parse_registry(&format!(r#"{{"schema_version":1,"generated":"synthetic","harnesses":[{{"id":"openclaw","display_name":"OpenClaw","product_group":"agent-platforms","paths":{{"macos":{{"template":"~/agents","format":"sqlite","confidence":"official-docs","source":"synthetic"}},"linux":{{"template":"~/agents","format":"sqlite","confidence":"official-docs","source":"synthetic"}},"windows":{{"template":"~/agents","format":"sqlite","confidence":"official-docs","source":"synthetic"}}}}}}]}}"#), "synthetic registry").unwrap();
        let config = Config {
            harness_roots: [(
                "openclaw".to_string(),
                agents.to_string_lossy().into_owned(),
            )]
            .into_iter()
            .collect(),
            ..Config::default()
        };
        let report =
            scan_with_registry_and_machine(&config, &registry, "synthetic-machine").unwrap();
        assert_eq!(
            registry.harnesses[0].product_group.as_deref(),
            Some("agent-platforms")
        );
        let ids: Vec<_> = report
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect();
        assert_eq!(ids.len(), 5, "one live window, its SQLite archive, and three unmatched cold files; the matching SQLite file is excluded");
        assert_eq!(
            ids.iter().collect::<std::collections::HashSet<_>>().len(),
            ids.len(),
            "separate cold archive files for one native session must not collide"
        );
        let prefix = crate::sqlite_probe::openclaw_native_id("synthetic-agent", "");
        assert!(ids
            .iter()
            .any(|id| id.ends_with(&format!("{prefix}6c6976652d77696e646f77"))));
        assert!(ids
            .iter()
            .any(|id| id.ends_with(&format!("{prefix}636f6c642d6462~g3"))));
        let cold_file_prefix = format!("{prefix}636f6c642d66696c65~cold-");
        assert_eq!(
            ids.iter()
                .filter(|id| id.contains(&cold_file_prefix))
                .count(),
            2,
            "both files for the same native session are retained"
        );
        let archive_only_prefix =
            crate::sqlite_probe::openclaw_native_id("synthetic-archive-only", "cold-only");
        assert!(ids
            .iter()
            .any(|id| id.contains(&format!("{archive_only_prefix}~cold-"))));
        assert_eq!(report.probes[0].record_count, Some(5));
    }

    /// W325/GLM review. A store containing one legacy-import window with a NULL
    /// `started_at` must still enumerate — the windows must become records AND
    /// the SQLite archive ids must still reach the cold-file dedup: the cold
    /// file matching an archived generation is skipped, an unmatched cold file
    /// is kept, and nothing is reported unreadable. Before the review fix,
    /// the one NULL row aborted the whole enumeration, so both windows, the
    /// archive row and the dedup silently vanished behind a single unexplained
    /// unreadable counter.
    #[test]
    fn openclaw_scan_survives_null_started_at_and_dedups_cold_archives() {
        let sandbox = crate::test_support::Sandbox::new();
        let agents = sandbox.root().join("agents");
        let db = agents.join("synthetic-agent/agent/openclaw-agent.sqlite");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        fs::create_dir_all(agents.join("synthetic-agent/sessions")).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_meta(agent_id TEXT);
             INSERT INTO schema_meta VALUES ('synthetic-agent');
             CREATE TABLE session_windows(session_id TEXT, session_key TEXT, started_at TEXT);
             INSERT INTO session_windows VALUES ('legacy-import', 'logical-a', NULL);
             INSERT INTO session_windows VALUES ('current-window', 'logical-b', '2026-01-02T00:00:00Z');
             CREATE TABLE transcript_events(session_id TEXT, seq INTEGER, event_json TEXT, created_at TEXT);
             INSERT INTO transcript_events VALUES ('current-window', 1, '{\"id\":\"synthetic-event\",\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"provider\":\"synthetic-provider\",\"model\":\"synthetic-model\",\"usage\":{\"input\":2}}}', '2026-01-02T00:00:01Z');
             CREATE TABLE session_transcript_archives(session_id TEXT, generation INTEGER, session_key TEXT, reason TEXT, encoding TEXT, archive_blob BLOB, archive_sha256 TEXT, archive_name TEXT, created_at TEXT, published_at TEXT);
             INSERT INTO session_transcript_archives VALUES ('cold-db', 3, 'logical-cold', 'deleted', 'zstd', X'00', '6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d', 'cold-db.jsonl.zst', '2026-01-02T00:00:00Z', NULL);",
        )
        .unwrap();
        drop(conn);
        fs::write(
            agents.join("synthetic-agent/sessions/cold-db.jsonl.deleted.synthetic.zst"),
            b"duplicate of the generation the SQLite archive already retained",
        )
        .unwrap();
        fs::write(
            agents.join("synthetic-agent/sessions/cold-keep.jsonl.zst"),
            b"cold archive with no SQLite counterpart",
        )
        .unwrap();
        let registry = parse_registry(&format!(r#"{{"schema_version":1,"generated":"synthetic","harnesses":[{{"id":"openclaw","display_name":"OpenClaw","product_group":"agent-platforms","paths":{{"macos":{{"template":"~/agents","format":"sqlite","confidence":"official-docs","source":"synthetic"}},"linux":{{"template":"~/agents","format":"sqlite","confidence":"official-docs","source":"synthetic"}},"windows":{{"template":"~/agents","format":"sqlite","confidence":"official-docs","source":"synthetic"}}}}}}]}}"#), "synthetic registry").unwrap();
        let config = Config {
            harness_roots: [(
                "openclaw".to_string(),
                agents.to_string_lossy().into_owned(),
            )]
            .into_iter()
            .collect(),
            ..Config::default()
        };
        let report =
            scan_with_registry_and_machine(&config, &registry, "synthetic-machine").unwrap();
        let ids: Vec<_> = report
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect();
        assert_eq!(
            ids.len(),
            4,
            "two windows, the SQLite archive row, and the one unmatched cold file"
        );
        assert_eq!(
            report.probes[0].record_count,
            Some(4),
            "every enumerable row became a record"
        );
        assert_eq!(
            report.probes[0].unreadable_count,
            Some(0),
            "a NULL started_at is a readable value, not an unreadable one"
        );
        let prefix = crate::sqlite_probe::openclaw_native_id("synthetic-agent", "");
        assert!(ids
            .iter()
            .any(|id| id.ends_with(&format!("{prefix}6c65676163792d696d706f7274"))));
        assert!(ids
            .iter()
            .any(|id| id.ends_with(&format!("{prefix}63757272656e742d77696e646f77"))));
        assert_eq!(
            ids.iter().filter(|id| id.contains("~g3")).count(),
            1,
            "the SQLite archive for generation 3 is its own record"
        );
        assert!(
            !ids.iter().any(|id| id.contains("636f6c642d6462~cold-")),
            "the cold file matching the SQLite archive row is deduplicated away"
        );
        assert!(
            ids.iter().any(|id| id.contains("636f6c642d6b656570~cold-")),
            "the unmatched cold file is kept"
        );
    }

    /// W325/GLM review. An agent database the probe cannot enumerate must say
    /// so: the error string used to be discarded behind a bare unreadable
    /// `+1`. The note carries the first failure's reason, the unreadable
    /// counter still counts the store, and — new — sibling agents keep
    /// scanning and deduplicating normally instead of losing their records to
    /// one broken store's bookkeeping.
    #[test]
    fn openclaw_unenumerable_agent_store_is_reported_in_the_probe_note() {
        let sandbox = crate::test_support::Sandbox::new();
        let agents = sandbox.root().join("agents");
        let broken = agents.join("synthetic-broken/agent/openclaw-agent.sqlite");
        fs::create_dir_all(broken.parent().unwrap()).unwrap();
        fs::create_dir_all(agents.join("synthetic-broken/sessions")).unwrap();
        fs::create_dir_all(agents.join("synthetic-healthy/sessions")).unwrap();
        let conn = rusqlite::Connection::open(&broken).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_meta(agent_id TEXT);
             INSERT INTO schema_meta VALUES ('synthetic-broken');
             CREATE TABLE transcript_events(session_id TEXT, seq INTEGER, event_json TEXT, created_at TEXT);
             CREATE TABLE session_transcript_archives(session_id TEXT, generation INTEGER, session_key TEXT, reason TEXT, encoding TEXT, archive_blob BLOB, archive_sha256 TEXT, archive_name TEXT, created_at TEXT, published_at TEXT);",
        )
        .unwrap();
        drop(conn);
        fs::write(
            agents.join("synthetic-broken/sessions/cold-broken.jsonl.zst"),
            b"cold archive of a store that cannot be enumerated",
        )
        .unwrap();
        let healthy = agents.join("synthetic-healthy/agent/openclaw-agent.sqlite");
        fs::create_dir_all(healthy.parent().unwrap()).unwrap();
        let conn = rusqlite::Connection::open(&healthy).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_meta(agent_id TEXT);
             INSERT INTO schema_meta VALUES ('synthetic-healthy');
             CREATE TABLE session_windows(session_id TEXT, session_key TEXT, started_at TEXT);
             INSERT INTO session_windows VALUES ('live-window', 'logical-live', '2026-01-01T00:00:00Z');
             CREATE TABLE transcript_events(session_id TEXT, seq INTEGER, event_json TEXT, created_at TEXT);
             CREATE TABLE session_transcript_archives(session_id TEXT, generation INTEGER, session_key TEXT, reason TEXT, encoding TEXT, archive_blob BLOB, archive_sha256 TEXT, archive_name TEXT, created_at TEXT, published_at TEXT);
             INSERT INTO session_transcript_archives VALUES ('cold-dup', 3, 'logical-cold', 'deleted', 'zstd', X'00', '6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d', 'cold-dup.jsonl.zst', '2026-01-02T00:00:00Z', NULL);",
        )
        .unwrap();
        drop(conn);
        fs::write(
            agents.join("synthetic-healthy/sessions/cold-dup.jsonl.deleted.synthetic.zst"),
            b"duplicate of the generation the healthy store already archived",
        )
        .unwrap();
        let registry = parse_registry(&format!(r#"{{"schema_version":1,"generated":"synthetic","harnesses":[{{"id":"openclaw","display_name":"OpenClaw","product_group":"agent-platforms","paths":{{"macos":{{"template":"~/agents","format":"sqlite","confidence":"official-docs","source":"synthetic"}},"linux":{{"template":"~/agents","format":"sqlite","confidence":"official-docs","source":"synthetic"}},"windows":{{"template":"~/agents","format":"sqlite","confidence":"official-docs","source":"synthetic"}}}}}}]}}"#), "synthetic registry").unwrap();
        let config = Config {
            harness_roots: [(
                "openclaw".to_string(),
                agents.to_string_lossy().into_owned(),
            )]
            .into_iter()
            .collect(),
            ..Config::default()
        };
        let report =
            scan_with_registry_and_machine(&config, &registry, "synthetic-machine").unwrap();
        let ids: Vec<_> = report
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect();
        assert_eq!(
            ids.len(),
            3,
            "broken store's cold file, healthy window, healthy archive"
        );
        assert_eq!(report.probes[0].record_count, Some(3));
        assert_eq!(
            report.probes[0].unreadable_count,
            Some(1),
            "the unenumerable store is one counted unreadable store"
        );
        assert!(
            report.probes[0].note.contains("unenumerable agent store")
                && report.probes[0].note.contains("session_windows"),
            "the probe note must report the reason the store could not be enumerated, got: {}",
            report.probes[0].note
        );
        assert!(
            !ids.iter().any(|id| id.contains("636f6c642d647570~cold-")),
            "the healthy agent's dedup is unaffected by the broken store"
        );
    }

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
            "a leading `~something` still refuses to anchor: unknown must not be treated as empty"
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
        assert_eq!(
            Confidence::classify("source-confirmed"),
            Confidence::Confirmed
        );
        assert_eq!(
            Confidence::classify("official-docs"),
            Confidence::OfficialDocs
        );
        assert_eq!(
            Confidence::classify("community-claim-unverified"),
            Confidence::CommunityClaim
        );
        assert_eq!(
            Confidence::classify("measured-locally"),
            Confidence::Measured
        );
        assert_eq!(
            Confidence::classify("unascertained"),
            Confidence::Unascertained
        );
        assert_eq!(
            Confidence::classify("some-unknown-token"),
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

    #[test]
    fn all_unascertained_source_roots_keep_policy_skip_and_unknown_counts() {
        let mut registry = unascertained_registry();
        let harness = &mut registry.harnesses[0];
        harness.source_roots = ["cli", "ide", "app"]
            .into_iter()
            .map(|id| RegistrySourceRoot {
                id: id.to_string(),
                paths: harness.paths.clone(),
                override_scope: None,
                default_only: false,
                provenance: Default::default(),
                movable: false,
            })
            .collect();
        let report = scan_with_registry(&Config::default(), &registry).unwrap();
        assert!(report.records.is_empty());
        assert_eq!(report.probes.len(), 1);
        let probe = &report.probes[0];
        assert_eq!(probe.state, ProbeState::SkipUnascertained);
        assert_eq!(probe.confidence, Confidence::Unascertained);
        assert_eq!(probe.record_count, None);
        assert_eq!(probe.candidate_count, None);
        assert_eq!(probe.bytes, None);
        assert!(!probe.installed_p());
    }

    /// The other side of the same gate: `unascertained` says *we* could not verify a
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
    fn recursive_session_dir_pattern_matches_zero_or_more_prefix_components() {
        fn id(scope: &SessionScope) -> Option<&str> {
            match scope {
                SessionScope::Outside => None,
                SessionScope::In { id, .. } => Some(id),
            }
        }

        let root = Path::new("/fixture/sessions");
        let rule = SessionDirRule {
            pattern: "**/session_*".to_string(),
            file: Some("agents/main/wire.jsonl".to_string()),
            file_glob: None,
            id_components: Vec::new(),
            extra_files: Vec::new(),
        };
        for path in [
            root.join("session_zero-prefix"),
            root.join("workspace").join("session_one-prefix"),
            root.join("workspace")
                .join("project")
                .join("session_two-prefix"),
        ] {
            let scope = descend_scope(
                &SessionScope::Outside,
                &path,
                root,
                Some(&rule),
                HarnessSource::KimiCode,
            );
            assert_eq!(id(&scope), path.file_name().and_then(|name| name.to_str()));
        }
    }

    #[test]
    fn recursive_session_dir_pattern_uses_nearest_match_but_keeps_implementation_dirs() {
        fn id(scope: &SessionScope) -> Option<&str> {
            match scope {
                SessionScope::Outside => None,
                SessionScope::In { id, .. } => Some(id),
            }
        }

        let root = Path::new("/fixture/sessions");
        let rule = SessionDirRule {
            pattern: "**/session_*".to_string(),
            file: Some("agents/main/wire.jsonl".to_string()),
            file_glob: None,
            id_components: Vec::new(),
            extra_files: Vec::new(),
        };
        let outer_path = root.join("workspace").join("session_outer");
        let outer = descend_scope(
            &SessionScope::Outside,
            &outer_path,
            root,
            Some(&rule),
            HarnessSource::KimiCode,
        );
        assert_eq!(id(&outer), Some("session_outer"));

        let implementation = outer_path.join("agents");
        let still_outer = descend_scope(
            &outer,
            &implementation,
            root,
            Some(&rule),
            HarnessSource::KimiCode,
        );
        assert_eq!(id(&still_outer), Some("session_outer"));

        let inner_path = outer_path.join("session_inner");
        let inner = descend_scope(
            &outer,
            &inner_path,
            root,
            Some(&rule),
            HarnessSource::KimiCode,
        );
        assert_eq!(id(&inner), Some("session_inner"));
    }

    #[test]
    fn ordinary_two_level_session_dir_pattern_stays_exact_and_nonrecursive() {
        fn id(scope: &SessionScope) -> Option<&str> {
            match scope {
                SessionScope::Outside => None,
                SessionScope::In { id, .. } => Some(id),
            }
        }

        let root = Path::new("/fixture/sessions");
        let rule = SessionDirRule {
            pattern: "*/*".to_string(),
            file: Some("transcript.jsonl".to_string()),
            file_glob: None,
            id_components: Vec::new(),
            extra_files: Vec::new(),
        };
        let root_session = root.join("workspace").join("session_one");
        let scope = descend_scope(
            &SessionScope::Outside,
            &root_session,
            root,
            Some(&rule),
            HarnessSource::GoogleAntigravity,
        );
        assert_eq!(id(&scope), Some("session_one"));

        let implementation = root_session.join("implementation").join("session_false");
        let preserved = descend_scope(
            &scope,
            &implementation,
            root,
            Some(&rule),
            HarnessSource::GoogleAntigravity,
        );
        assert_eq!(id(&preserved), Some("session_one"));

        let too_deep = root.join("workspace").join("project").join("session_two");
        let rejected = descend_scope(
            &SessionScope::Outside,
            &too_deep,
            root,
            Some(&rule),
            HarnessSource::GoogleAntigravity,
        );
        assert_eq!(id(&rejected), None);
    }

    #[test]
    fn ordinary_session_dir_patterns_keep_exact_root_relative_depth() {
        fn id(scope: &SessionScope) -> Option<&str> {
            match scope {
                SessionScope::Outside => None,
                SessionScope::In { id, .. } => Some(id),
            }
        }

        let root = Path::new("/fixture/sessions");
        for (pattern, matching, too_deep) in [
            (
                "*",
                root.join("session_one"),
                root.join("workspace").join("session_false"),
            ),
            (
                "*/session_*",
                root.join("workspace").join("session_one"),
                root.join("workspace").join("agents").join("session_false"),
            ),
        ] {
            let rule = SessionDirRule {
                pattern: pattern.to_string(),
                file: Some("agents/main/wire.jsonl".to_string()),
                file_glob: None,
                id_components: Vec::new(),
                extra_files: Vec::new(),
            };
            let matched = descend_scope(
                &SessionScope::Outside,
                &matching,
                root,
                Some(&rule),
                HarnessSource::KimiCode,
            );
            assert_eq!(id(&matched), Some("session_one"), "pattern: {pattern}");
            let rejected = descend_scope(
                &SessionScope::Outside,
                &too_deep,
                root,
                Some(&rule),
                HarnessSource::KimiCode,
            );
            assert_eq!(id(&rejected), None, "pattern: {pattern}");
        }
    }

    #[test]
    fn kimi_workspace_segment_is_the_container_only_at_the_measured_depth() {
        fn container(scope: &SessionScope) -> Option<&str> {
            match scope {
                SessionScope::Outside => None,
                SessionScope::In {
                    container: Some(value),
                    ..
                } => Some(value),
                SessionScope::In {
                    container: None, ..
                } => None,
            }
        }

        let root = Path::new("/fixture/sessions");
        let rule = SessionDirRule {
            pattern: "**/session_*".to_string(),
            file: Some("agents/main/wire.jsonl".to_string()),
            file_glob: None,
            id_components: Vec::new(),
            extra_files: Vec::new(),
        };

        // The measured layout: <root>/<workspaceId>/<sessionId>.
        let measured = root.join("workspace_id").join("session_one");
        let scope = descend_scope(
            &SessionScope::Outside,
            &measured,
            root,
            Some(&rule),
            HarnessSource::KimiCode,
        );
        assert_eq!(container(&scope), Some("workspace_id"));

        // A session directly under the root has no workspace segment
        // above it, so no container is observed — never an empty one.
        let direct = root.join("session_one");
        let scope = descend_scope(
            &SessionScope::Outside,
            &direct,
            root,
            Some(&rule),
            HarnessSource::KimiCode,
        );
        assert_eq!(container(&scope), None);

        // A session nested below another directory carries an
        // implementation directory above it, which is not a workspace.
        let nested = root.join("workspace_id").join("agents").join("session_two");
        let scope = descend_scope(
            &SessionScope::Outside,
            &nested,
            root,
            Some(&rule),
            HarnessSource::KimiCode,
        );
        assert_eq!(container(&scope), None);

        // DeepSeek Harness's two-level layout names its segment by a
        // lossy cwd slug: a path derivative, never a container
        // identity, so the parent segment stays unobserved for it.
        let slug = root.join("cwd-slug").join("session_three");
        let scope = descend_scope(
            &SessionScope::Outside,
            &slug,
            root,
            Some(&rule),
            HarnessSource::DeepSeekHarness,
        );
        assert_eq!(container(&scope), None);
    }

    #[test]
    fn source_detection_via_directory_segment() {
        let codex = Path::new("/Users/u/.codex/sessions/2026-05-01/ab.jsonl");
        let claude = Path::new("/Users/u/.claude/projects/foo-abc/ab.jsonl");
        assert_eq!(detect_source(codex), Some(HarnessSource::Codex));
        assert_eq!(detect_source(claude), Some(HarnessSource::ClaudeCode));
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

    /// B96: a template that *ends* in a variable is resolvable; only a bare
    /// trailing `$` is not. The old guard (`name_len == rest.len()`) rejected
    /// both, which reported a knowable root as unknown — this repo's rule run
    /// backwards. Both directions are asserted so the guard cannot drift back.
    #[test]
    fn template_ending_in_a_variable_resolves_and_a_bare_dollar_does_not() {
        assert!(
            static_prefix_root("$XDG_DATA_HOME").is_some(),
            "a template that is exactly one variable has a knowable root"
        );
        assert!(
            static_prefix_root("$HOME").is_some(),
            "same for $HOME with nothing after it"
        );
        assert!(
            static_prefix_root("$HOME/.claude/projects").is_some(),
            "the shape every registry entry uses today must keep working"
        );
        assert!(
            static_prefix_root("$").is_none(),
            "a bare trailing `$` still has no name after it"
        );
        assert!(
            static_prefix_root("$CODEX_HOME").is_none(),
            "an unresolved per-install override is still refused, by name"
        );
    }

    /// W285 §6 — "never probed" is its own state, and the predicate that names
    /// it covers exactly the three states B82 recorded, no more and no fewer.
    ///
    /// All seven states are asserted, so the partition is pinned rather than
    /// assumed: `probed_p` and `not_probed_p` are the two questions `doctor`'s
    /// coverage summary and `status`'s warning ask of every probe, and a state
    /// moved from one group to the other is a change in what those reports
    /// claim — this is the test that makes it visible.
    #[test]
    fn never_probed_covers_the_three_unlooked_states_and_nothing_else() {
        let probe = |state: ProbeState| HarnessProbe {
            id: "x".to_string(),
            display_name: "x".to_string(),
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
        };

        // Either an answer about the machine exists (a count, or a measured
        // absence), or the question does not apply to this platform.
        for state in [
            ProbeState::Scanned,
            ProbeState::FileTarget,
            ProbeState::Missing,
            // No cell for this platform — nothing here to have missed.
            ProbeState::SkipWrongPlatform,
        ] {
            assert!(
                !probe(state).not_probed_p(),
                "{state:?} was looked at (or is not applicable), so it must not be counted as never probed"
            );
            assert!(probe(state).probed_p(), "{state:?} must count as probed");
        }

        // Never looked at: the probe resolved no root, was forbidden to try, or
        // could not even establish whether the source exists. The third is
        // B82's own wording, kept here so `doctor` cannot re-decide it alone.
        for state in [
            ProbeState::SkipUnascertained,
            ProbeState::SkipUnresolvable,
            ProbeState::Indeterminate,
        ] {
            assert!(
                probe(state).not_probed_p(),
                "{state:?} never established anything and must be counted as never probed"
            );
            assert!(
                !probe(state).probed_p(),
                "{state:?} must not count as probed"
            );
        }
    }

    /// GC-3 / W993 — `found_staged`, the per-harness staged-session tri-state.
    ///
    /// The positive branch is `known` whatever the probe state, because the
    /// count is measured from the records the scan actually emitted and
    /// `collect` stages exactly those; every zero is only `known` when the
    /// absence itself was measured, and stays `unknown` / `not_applicable`
    /// otherwise — an unlooked or un-enumerable harness must never read as
    /// a staged zero. Every case below is one state or one shape, so the
    /// partition is asserted, not assumed.
    #[test]
    fn found_staged_states_the_measured_unit_and_keeps_unproven_zeros_honest() {
        fn probe_with(state: ProbeState, record_count: Option<u64>) -> HarnessProbe {
            HarnessProbe {
                id: "synthetic".to_string(),
                display_name: "synthetic".to_string(),
                root: None,
                confidence: Confidence::Measured,
                state,
                unverified_roots: Vec::new(),
                source_roots: Vec::new(),
                record_count,
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

        use crate::json_out::CountState;

        // Measured from emitted records: known even where session_count
        // would be unknown (record_count None), because this question —
        // what collect will be handed — the scan's own emission answers.
        // The count stands for a two-records-one-id population collapsing
        // to one staged session, the W982 grok shape the key exists for.
        // Only the record-producing states are asserted here: emitting a
        // record requires a resolved, readable root, so no scan path pairs
        // `Indeterminate` or a skip state with a positive staged count, and
        // pinning such an impossible shape would state it as if it could
        // occur.
        for state in [ProbeState::Scanned, ProbeState::FileTarget] {
            assert_eq!(
                probe_found_staged(&probe_with(state, None), 1),
                CountState::known(1),
                "{state:?}: emitted records are the complete answer to what collect stages"
            );
        }

        // A scanned store that measured empty: the earned zero.
        assert_eq!(
            probe_found_staged(&probe_with(ProbeState::Scanned, Some(0)), 0),
            CountState::known(0)
        );
        assert_eq!(
            probe_found_staged(&probe_with(ProbeState::FileTarget, Some(0)), 0),
            CountState::known(0)
        );
        // Missing root: same earned zero as `probe_session_count`.
        assert_eq!(
            probe_found_staged(&probe_with(ProbeState::Missing, None), 0),
            CountState::known(0)
        );

        // Unproven zeros: a store that enumerates sessions but handed the
        // scan no records (the enumeration-failure shape), and a store the
        // scan could not enumerate at all — neither may claim a staged 0.
        assert_eq!(
            probe_found_staged(&probe_with(ProbeState::FileTarget, Some(9)), 0),
            CountState::unknown(
                "the store enumerates sessions but the scan produced no session \
                 records, so what collect will stage is unproven"
            )
        );
        assert_eq!(
            probe_found_staged(&probe_with(ProbeState::FileTarget, None), 0),
            CountState::unknown("session count could not be enumerated (store not enumerable)")
        );
        assert_eq!(
            probe_found_staged(&probe_with(ProbeState::Indeterminate, None), 0),
            CountState::unknown("cannot even confirm whether the source exists")
        );
        assert_eq!(
            probe_found_staged(&probe_with(ProbeState::SkipUnascertained, None), 0),
            CountState::unknown("confidence unascertained; scanning a guessed path is forbidden")
        );
        assert_eq!(
            probe_found_staged(&probe_with(ProbeState::SkipUnresolvable, None), 0),
            CountState::unknown(
                "no root to scan: template not statically resolvable, or registry id unknown \
                 in this build"
            )
        );
        assert_eq!(
            probe_found_staged(&probe_with(ProbeState::SkipWrongPlatform, None), 0),
            CountState::not_applicable("registry has no cell for this platform")
        );

        // The JSON shape: additive key on the scan-report side, absent on
        // the doctor's metadata-only side, and every existing field
        // byte-identical on both.
        let probe = probe_with(ProbeState::FileTarget, Some(4));
        let plain = probe_json(&probe);
        let staged = probe_json_with_found_staged(&probe, &probe_found_staged(&probe, 6));
        assert!(
            plain.get("found_staged").is_none(),
            "the metadata-only shape does not carry the key"
        );
        for (key, value) in plain.as_object().unwrap().iter() {
            assert_eq!(
                staged.get(key).unwrap(),
                value,
                "found_staged must be additive: {key} changed"
            );
        }
        assert_eq!(
            staged["found_staged"],
            serde_json::json!({"kind": "known", "count": 6})
        );
    }

    /// The per-harness grouping behind `found_staged`: distinct ids among
    /// THIS harness's records only — two records sharing one id (the grok
    /// row and its native transcript) stage one session, another harness's
    /// records never leak into the count, and `total_sessions` keeps
    /// counting records (GC-3's three counts deliberately disagree).
    #[test]
    fn scan_report_json_counts_staged_ids_per_harness_not_records() {
        fn record(source: HarnessSource, native_id: &str) -> SessionRecord {
            SessionRecord {
                id: crate::id::SessionIdentity {
                    source_short: source.short(),
                    machine: "synthetic-machine".to_string(),
                    native_id: native_id.to_string(),
                }
                .id(),
                absolute_path: PathBuf::from("/synthetic"),
                byte_size: 0,
                mtime: SystemTime::UNIX_EPOCH,
                source,
                compressed: false,
                sqlite_layout: None,
                provenance: Default::default(),
                session_dir: None,
                movable_root: false,
            }
        }
        let mut report = ScanReport::default();
        // Grok: one session observed twice (row + native transcript), plus
        // its usage sidecar under its own id — 3 records, 2 staged ids.
        report
            .records
            .push(record(HarnessSource::Grok, "synthetic-a"));
        report
            .records
            .push(record(HarnessSource::Grok, "synthetic-a"));
        report
            .records
            .push(record(HarnessSource::Grok, "synthetic-a.usage"));
        // Codex: a foreign harness whose records must not leak.
        report
            .records
            .push(record(HarnessSource::Codex, "synthetic-b"));
        for (id, display, state, count) in [
            ("grok", "Grok CLI", ProbeState::FileTarget, 9u64),
            ("codex", "Codex", ProbeState::Scanned, 1),
        ] {
            report.probes.push(HarnessProbe {
                id: id.to_string(),
                display_name: display.to_string(),
                root: None,
                confidence: Confidence::Measured,
                state,
                unverified_roots: Vec::new(),
                source_roots: Vec::new(),
                record_count: Some(count),
                candidate_count: Some(count),
                unreadable_count: Some(0),
                unreadable_entry_count: Some(0),
                earliest: None,
                latest: None,
                bytes: Some(0),
                recognized_files: Vec::new(),
                note: String::new(),
            });
        }

        let value = scan_report_json(&report);
        assert_eq!(
            value["total_sessions"],
            serde_json::json!({"kind": "known", "count": 4}),
            "total_sessions keeps counting SessionRecords"
        );
        let harnesses = value["harnesses"].as_array().unwrap();
        assert_eq!(
            harnesses[0]["found_staged"],
            serde_json::json!({"kind": "known", "count": 2}),
            "grok stages two ids: the row-shared session id and the usage id"
        );
        assert_eq!(
            harnesses[1]["found_staged"],
            serde_json::json!({"kind": "known", "count": 1}),
            "codex keeps records == staged ids"
        );
    }

    fn synthetic_copilot_registry() -> HarnessRegistry {
        let cell = serde_json::json!({
            "template": "~/.copilot/",
            "format": "sqlite + jsonl",
            "confidence": "source-confirmed",
            "source": "synthetic fixture",
            "env_override": "COPILOT_HOME"
        });
        let paths = match current_platform() {
            "macos" => serde_json::json!({"macos": cell}),
            "linux" => serde_json::json!({"linux": cell}),
            "windows" => serde_json::json!({"windows": cell}),
            platform => panic!("unexpected platform: {platform}"),
        };
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "generated": "synthetic fixture",
            "harnesses": [{"id": "github-copilot-cli", "display_name": "GitHub Copilot CLI fixture", "paths": paths}]
        }))
        .unwrap()
    }

    /// Write a synthetic Copilot store. `home` is the Copilot home itself
    /// (`~/.copilot` under the template, `$COPILOT_HOME` under the override),
    /// so both resolutions can share one fixture.
    fn write_copilot_store(home: &Path, drained: bool) {
        fs::create_dir_all(home).unwrap();
        let db = home.join("session-store.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        let sessions = if drained {
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT)"
        } else {
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT,
                repository TEXT,
                branch TEXT,
                summary TEXT,
                created_at TEXT,
                updated_at TEXT
            )"
        };
        conn.execute_batch(&format!(
            "{sessions}; CREATE TABLE turns (session_id TEXT, turn_index INTEGER, user_message TEXT, assistant_response TEXT);"
        ))
        .unwrap();
        if !drained {
            conn.execute(
                "INSERT INTO sessions (id, cwd, repository, branch, summary, created_at, updated_at)
                 VALUES ('synthetic-copilot-x', '/synthetic/cwd', 'dimpurr/synthetic-repo', 'main', 'synthetic summary', '2026-10-08T10:00:00Z', '2026-10-08T11:00:00Z')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (id, cwd, repository, branch, summary, created_at, updated_at)
                 VALUES ('synthetic-copilot-y', NULL, NULL, NULL, NULL, NULL, NULL)",
                [],
            )
            .unwrap();
        }
        drop(conn);
    }

    #[test]
    fn copilot_probe_enumerates_sessions_from_the_store() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        env::remove_var("COPILOT_HOME");
        write_copilot_store(&sandbox.home().join(".copilot"), false);

        let report = scan_with_registry_and_machine(
            &Config::default(),
            &synthetic_copilot_registry(),
            "synthetic-machine",
        )
        .unwrap();
        let probe = &report.probes[0];
        assert_eq!(probe.id, "github-copilot-cli");
        assert_eq!(probe.record_count, Some(2));
        assert!(probe.note.contains("sessions table(id, cwd, repository)"));
        let copilot_records: Vec<&SessionRecord> = report
            .records
            .iter()
            .filter(|record| record.id.starts_with("github-copilot-cli."))
            .collect();
        assert_eq!(copilot_records.len(), 2);
        assert!(copilot_records.iter().all(|record| record
            .sqlite_layout
            .is_some_and(|layout| layout == SqliteSessionLayout::GitHubCopilotCli)));
        assert!(copilot_records
            .iter()
            .any(|record| record.id.ends_with("synthetic-copilot-x")));
    }

    #[test]
    fn copilot_probe_home_env_override_wins_over_the_template() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox_home = tempfile::TempDir::new().unwrap();
        let sandbox_override = tempfile::TempDir::new().unwrap();
        env::set_var("HOME", sandbox_home.path());
        env::set_var("COPILOT_HOME", sandbox_override.path());

        let report = scan_with_registry_and_machine(
            &Config::default(),
            &synthetic_copilot_registry(),
            "synthetic-machine",
        )
        .unwrap();
        let probe = &report.probes[0];
        // The override is empty so far: no store, no events — but the probe
        // must have resolved the override, not the still-empty template home.
        assert!(probe.note.contains("env_override=$COPILOT_HOME"));
        assert_eq!(
            probe.root,
            Some(sandbox_override.path().to_path_buf()),
            "COPILOT_HOME must win over the registry template"
        );

        write_copilot_store(sandbox_override.path(), false);
        let report = scan_with_registry_and_machine(
            &Config::default(),
            &synthetic_copilot_registry(),
            "synthetic-machine",
        )
        .unwrap();
        assert_eq!(report.probes[0].record_count, Some(2));
        assert_eq!(report.records.len(), 2);

        env::remove_var("COPILOT_HOME");
        env::set_var("HOME", tempfile::TempDir::new().unwrap().path());
    }

    #[test]
    fn copilot_probe_without_store_and_without_events_is_measured_zero() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        env::remove_var("COPILOT_HOME");
        fs::create_dir_all(sandbox.home().join(".copilot/ide")).unwrap();
        fs::write(sandbox.home().join(".copilot/ide/synthetic.lock"), b"lock").unwrap();

        let report = scan_with_registry_and_machine(
            &Config::default(),
            &synthetic_copilot_registry(),
            "synthetic-machine",
        )
        .unwrap();
        let probe = &report.probes[0];
        assert_eq!(probe.record_count, Some(0));
        assert!(probe.note.contains("no persisted sessions to read"));
        assert!(report.records.is_empty());
    }

    #[test]
    fn copilot_events_without_a_store_keep_the_session_count_unknown() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        env::remove_var("COPILOT_HOME");
        let session_state = sandbox.home().join(".copilot/session-state");
        fs::create_dir_all(&session_state).unwrap();
        fs::write(
            session_state.join("events.jsonl"),
            b"{\"event\":\"session.start\"}\n",
        )
        .unwrap();

        let report = scan_with_registry_and_machine(
            &Config::default(),
            &synthetic_copilot_registry(),
            "synthetic-machine",
        )
        .unwrap();
        let probe = &report.probes[0];
        assert_eq!(
            probe.record_count, None,
            "events the reader cannot map to a session must not be counted as 0"
        );
        assert!(probe.note.contains("events.jsonl"));
        assert!(probe.note.contains("unknown"));
        assert!(report.records.is_empty());
    }

    #[test]
    fn copilot_drifted_store_is_un_enumerable_not_empty() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let sandbox = Sandbox::new();
        env::set_var("HOME", sandbox.home());
        env::remove_var("COPILOT_HOME");
        write_copilot_store(&sandbox.home().join(".copilot"), true);

        let report = scan_with_registry_and_machine(
            &Config::default(),
            &synthetic_copilot_registry(),
            "synthetic-machine",
        )
        .unwrap();
        let probe = &report.probes[0];
        assert_eq!(probe.record_count, None);
        assert!(probe.note.contains("un-enumerable"));
        assert!(report.records.is_empty());
    }

    /// The W1047 recognized-by-name arm: a `*.jsonl` file the CodeBuddy CLI
    /// walk accepted by name whose metadata could not be taken is a
    /// recognized session without a record — collect's PARTIAL exit-3 gap,
    /// never an unreadable entry and never zero. POSIX cannot be made to
    /// fail that stat portably (a readable parent directory makes stat
    /// succeed whatever the file's own mode bits say), so this pins the
    /// walk's accounting directly: the walk built below is exactly the
    /// state the real walker hands back for one accepted name that gave no
    /// metadata, and the report arithmetic reads the probe that accounting
    /// assembles.
    #[test]
    fn a_codebuddy_name_without_metadata_is_a_recognized_gap_never_an_entry() {
        let dir = tempfile::tempdir().unwrap();
        let walk = CbcTranscriptWalk {
            transcripts: [(
                "synthetic-session-701".to_string(),
                CbcTranscript {
                    path: dir.path().join("synthetic-session-701.jsonl"),
                    mtime: SystemTime::UNIX_EPOCH,
                    byte_size: 84,
                },
            )]
            .into_iter()
            .collect(),
            flat_files: 2,
            slug_files: 0,
            stale_duplicates: 0,
            unreadable_entries: 0,
            recognized_unrecorded: 1,
        };
        let base = HarnessProbe {
            id: "codebuddy".to_string(),
            display_name: "CodeBuddy".to_string(),
            root: None,
            confidence: Confidence::Unascertained,
            state: ProbeState::Scanned,
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
        };
        let probe = codebuddy_probe_accounting(base, walk, String::new());
        assert_eq!(probe.record_count, Some(2), "readable and name-only alike");
        assert_eq!(
            probe.unreadable_count,
            Some(1),
            "a recognized session without a record"
        );
        assert_eq!(
            probe.unreadable_entry_count,
            Some(0),
            "a matched name is never reported as a directory entry"
        );
        assert!(probe.note.contains(
            "1 transcript file(s) present but unreadable — recognized, not archived, not 0"
        ));

        // The report arithmetic collect reads: recognized two, one record —
        // the named gap that turns the run partial, exit 3 never 0.
        let mut report = ScanReport::default();
        report.records.push(SessionRecord {
            id: cbc_session_id(
                HarnessSource::CodeBuddy,
                "synthetic-machine",
                "synthetic-session-701",
            ),
            absolute_path: dir.path().join("synthetic-session-701.jsonl"),
            byte_size: 84,
            mtime: SystemTime::UNIX_EPOCH,
            source: HarnessSource::CodeBuddy,
            compressed: false,
            sqlite_layout: None,
            provenance: Default::default(),
            session_dir: None,
            movable_root: false,
        });
        report.probes.push(probe);
        let gaps = report.archive_gaps();
        assert_eq!(gaps.len(), 1, "collect reports PARTIAL: exit 3, never 0");
        assert_eq!(gaps[0].harness_id, "codebuddy");
        assert_eq!(gaps[0].recognized_sessions, Some(2));
        assert_eq!(gaps[0].session_records, 1);
    }

    /// The W1047f L3 arm: a store row whose time cannot be taken at all —
    /// the row's own columns do not read as a time, and the store file's own
    /// mtime is absent too — is dropped without a fake epoch, and until this
    /// fix it was dropped *into the `without_transcript` bucket*, which
    /// printed it as a live row without a local transcript. A deleted row
    /// whose transcript is still on disk, and a live row whose transcript is
    /// still on disk, are never that, and the note must not say they are.
    /// POSIX cannot stage this state through the filesystem (a stat of a
    /// live sqlite file always supplies a modified time), so the fold is
    /// pinned directly against fabricated rows — the same precedent as the
    /// codebuddy accounting test above.
    #[test]
    fn a_row_without_any_readable_time_is_named_not_mislabeled() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("workbuddy.db");
        let mut transcripts = std::collections::BTreeMap::new();
        transcripts.insert(
            "synthetic-session-801".to_string(),
            CbcTranscript {
                path: dir.path().join("synthetic-session-801.jsonl"),
                mtime: SystemTime::UNIX_EPOCH,
                byte_size: 64,
            },
        );
        transcripts.insert(
            "synthetic-session-802".to_string(),
            CbcTranscript {
                path: dir.path().join("synthetic-session-802.jsonl"),
                mtime: SystemTime::UNIX_EPOCH,
                byte_size: 64,
            },
        );
        let rows = vec![
            // The review's exact case: a tombstoned row whose transcript is
            // still present, with no time any column can name.
            crate::sqlite_probe::WorkBuddySessionRow {
                id: "synthetic-session-801".to_string(),
                deleted: true,
                transport: None,
                time_ms: None,
            },
            // The live twin of the same failure: transcript present, no
            // readable time — no more a "live row without a transcript".
            crate::sqlite_probe::WorkBuddySessionRow {
                id: "synthetic-session-802".to_string(),
                deleted: false,
                transport: Some("local".to_string()),
                time_ms: None,
            },
        ];
        let mut records = Vec::new();
        let mut earliest = None;
        let mut latest = None;
        let fold = fold_workbuddy_store_rows(
            rows,
            &transcripts,
            &db,
            // The store-file fallback is absent: the one input POSIX cannot
            // take away from a real run, so the only honest states left to
            // the fold are the row's own columns — both unreadable here.
            None,
            HarnessSource::WorkBuddy,
            "synthetic-machine",
            &mut records,
            &mut earliest,
            &mut latest,
        );
        assert_eq!(fold.untimeable_rows, 2, "both rows are the untimed arm");
        assert_eq!(
            fold.without_transcript, 0,
            "neither row is a live row without a local transcript"
        );
        assert_eq!(fold.tombstoned, 0, "neither row reached the tombstone arm");
        assert_eq!(fold.orphan_transcripts, 0);
        assert_eq!(fold.known_sessions, 2, "both rows stay recognized");
        assert!(
            records.is_empty(),
            "no record was emitted: never a fake epoch, neither the rows' \
             virtual records nor their transcripts'"
        );
        // The named gap: the probe's note segment is the honest line, and it
        // is neither the without-transcript count nor silence.
        let note = workbuddy_untimed_rows_note(fold.untimeable_rows);
        assert!(note
            .contains("2 store row(s) without a readable time — recognized, not archived, not 0"));
        assert!(!note.contains("without a local transcript"));
        assert_eq!(workbuddy_untimed_rows_note(0), String::new());
    }
}
