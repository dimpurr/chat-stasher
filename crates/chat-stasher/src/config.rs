//! Configuration handling for chat-stasher.
//!
//! The config lives at `~/.config/chat-stasher/config.toml`. Per the spike
//! requirements, a missing file is fine — the tool falls back to defaults
//! instead of erroring out. Only a *broken* TOML (or an unreadable file for a
//! reason other than "not there") is worth warning about, and even then we
//! degrade to defaults rather than aborting a scan.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Directory + filename of the config relative to the user's config home
/// (`$XDG_CONFIG_HOME`, falling back to `~/.config`).
pub const CONFIG_RELATIVE_PATH: &str = "chat-stasher/config.toml";

/// Everything the tool knows how to configure today.
///
/// All fields are optional on purpose: `None` means "use the default for this
/// machine". Because TOML has no `null`, `Option<T>` fields are simply
/// omitted from a freshly written template file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where archived snapshots will live once `push` is implemented.
    /// Currently unused; kept so the config schema is stable.
    pub archive_root: Option<String>,
    /// Root directory that holds Claude Code session JSONL files.
    pub claude_projects_dir: Option<String>,
    /// Root directory that holds Codex session JSONL files.
    pub codex_sessions_dir: Option<String>,
    /// Local rustic repository that `push`/`read` operate on.
    ///
    /// Default: `$XDG_DATA_HOME/chat-stasher/repo` (falls back to
    /// `~/.local/share/chat-stasher/repo`). A remote backend will later be
    /// expressed here too (e.g. `opendal:sftp`, proven in spike A4).
    pub rustic_repo: Option<String>,
    /// File that holds the persisted masterkey (`Credentials::Masterkey`).
    ///
    /// Written on repo init, read on open. Without it the repo is unreadable,
    /// so treat this file as the key to the whole archive.
    pub rustic_key_file: Option<String>,
    /// Cap on the concurrency handed to rustic (default 10).
    ///
    /// rustic's read side fans out to ~CPU cores by default; the cap is a
    /// config knob now so a remote backend's connection limit (e.g. <10 for
    /// SFTP) is honoured before it is wired in.
    pub rustic_connections: Option<usize>,

    /// How often an unattended run should archive, in seconds.
    ///
    /// Default `DEFAULT_BACKUP_INTERVAL_SECS` (hourly). The scheduling
    /// mechanism itself is **not implemented yet** — this knob records the
    /// decided default so the number lives in one place when it is.
    ///
    /// Why hourly: it bounds worst-case loss to one hour, and G7/B18 measured
    /// the cost of that cadence directly. With shards bucketed
    /// (`DEFAULT_SHARD_BUCKET_CAP`) a 32 KB hourly delta settles around 152%
    /// cumulative amplification and stays bounded; without bucketing the
    /// per-push overhead grows linearly (R^2 = 0.99950) and reached 481% by
    /// push 200. Hourly is therefore only viable *because* buckets are capped.
    pub backup_interval_secs: Option<u64>,

    /// Skip the archive run when no source file changed since the last one.
    ///
    /// Default `true`. A no-change push still writes a snapshot, and each
    /// snapshot re-writes the tree of every touched session directory — so
    /// pushing on a timer regardless of change pays that cost for nothing.
    pub push_only_if_changed: Option<bool>,
}

/// Decided default archive cadence: hourly.
pub const DEFAULT_BACKUP_INTERVAL_SECS: u64 = 3600;

impl Config {
    /// Load configuration, falling back to defaults.
    ///
    /// Returns `Ok` for both "file missing" and "file present and valid".
    /// Only a parse failure or an unexpected I/O error produces a warning
    /// (still on stderr, still non-fatal), because a scan must never be
    /// blocked by a config typo.
    pub fn load() -> Self {
        match std::fs::read_to_string(config_path()) {
            Ok(raw) => match toml::from_str(&raw) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("warning: config is not valid TOML, using defaults: {e}");
                    Config::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // First run — no config yet. That is explicitly fine.
                Config::default()
            }
            Err(e) => {
                eprintln!("warning: could not read config, using defaults: {e}");
                Config::default()
            }
        }
    }

    /// Write a documented, commented template config file if one does not
    /// already exist. Never overwrites an existing file.
    ///
    /// Written by hand (not `toml::to_string`) so the file can carry comments
    /// explaining each option to a human reading it for the first time.
    pub fn init_default(template: &str) -> std::io::Result<()> {
        let path = config_path();
        if path.exists() {
            println!("config already exists: {}", path.display());
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, template)?;
        println!("wrote default config: {}", path.display());
        Ok(())
    }
}

/// Absolute path of the config file.
///
/// Uses `$XDG_CONFIG_HOME` when set (Linux/Unix convention), otherwise the
/// user's home directory. `std::env::home_dir` is deprecated in favour of
/// reading `HOME` directly; both are shelled out with [`std::env::home_dir`]
/// avoided because it is no longer guaranteed on macOS.
pub fn config_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join(CONFIG_RELATIVE_PATH);
    }
    home_dir().join(".config").join(CONFIG_RELATIVE_PATH)
}

/// Best-effort `$HOME` / user home directory.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The template written by `init` — comments explain each knob.
pub const DEFAULT_CONFIG_TEMPLATE: &str = r#"# chat-stasher configuration
#
# Every value is optional. Omitted values fall back to the per-machine
# standard locations for each harness.

# Where `push` will archive snapshots once that step lands.
# Default: unset (a local sibling directory of this config file).
# archive_root = "~/stash/chat-stasher"

# Local rustic repository used by `push` / `read`.
# Default: $XDG_DATA_HOME/chat-stasher/repo (or ~/.local/share/chat-stasher/repo).
# rustic_repo = "~/stash/chat-stasher/repo"

# File holding the persisted masterkey (created on repo init, needed on open).
# Default: $XDG_DATA_HOME/chat-stasher/masterkey.json.
# rustic_key_file = "~/stash/chat-stasher/masterkey.json"

# Concurrency handed to rustic. Default 4; hard ceiling 10.
# Measured (D2): raising this does not buy speed (11.32 s at 10 vs 10.00 s at 1)
# but does open more ssh ControlMasters that must later be reaped.
# rustic_connections = 4

# How often an unattended run should archive, in seconds. Default 3600 (hourly).
# NOTE: the scheduler itself is not implemented yet; this records the default.
# backup_interval_secs = 3600

# Skip the run when nothing changed since the last one. Default true.
# A no-change push still writes a snapshot, which is not free.
# push_only_if_changed = true

# Override the Claude Code session root.
# Default: ~/.claude/projects
# claude_projects_dir = "~/.claude/projects"

# Override the Codex session root.
# Default: ~/.codex/sessions
# codex_sessions_dir = "~/.codex/sessions"
"#;
