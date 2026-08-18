//! Configuration handling for chat-stasher.
//!
//! The config lives at `~/.config/chat-stasher/config.toml`. Per the spike
//! requirements, a missing file is fine — the tool falls back to defaults
//! instead of erroring out. Only a *broken* TOML (or an unreadable file for a
//! reason other than "not there") is worth warning about, and even then we
//! degrade to defaults rather than aborting a scan.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Directory + filename of the config relative to the user's config home
/// (`$XDG_CONFIG_HOME`, falling back to `~/.config`).
pub const CONFIG_RELATIVE_PATH: &str = "chat-stasher/config.toml";

/// Where the effective configuration came from.  A missing file is the normal
/// first-run default; the two error variants mean defaults were used after a
/// config read/parse failure and must remain visible to machine consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigSource {
    File,
    FileAfterWindowsPathRepair,
    #[default]
    DefaultsMissing,
    DefaultsAfterReadError,
    DefaultsAfterParseError,
}

impl ConfigSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::FileAfterWindowsPathRepair => "file_after_windows_path_repair",
            Self::DefaultsMissing => "defaults_missing",
            Self::DefaultsAfterReadError => "defaults_after_read_error",
            Self::DefaultsAfterParseError => "defaults_after_parse_error",
        }
    }

    pub fn is_error_fallback(self) -> bool {
        matches!(
            self,
            Self::DefaultsAfterReadError | Self::DefaultsAfterParseError
        )
    }
}

/// Everything the tool knows how to configure today.
///
/// All fields are optional on purpose: `None` means "use the default for this
/// machine". Because TOML has no `null`, `Option<T>` fields are simply
/// omitted from a freshly written template file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Provenance of this effective configuration. It is runtime metadata, not
    /// part of the TOML schema.
    #[serde(skip)]
    pub source: ConfigSource,
    /// Where archived snapshots will live once `push` is implemented.
    /// Kept so the config schema is stable.
    pub archive_root: Option<String>,
    /// Root directory that holds Claude Code session JSONL files.
    /// Alias of `harness_roots["claude-code"]`, kept for compatibility.
    pub claude_projects_dir: Option<String>,
    /// Root directory that holds Codex session JSONL files.
    /// Alias of `harness_roots["codex"]`.
    pub codex_sessions_dir: Option<String>,
    /// Explicit per-harness store roots, keyed by **registry harness id**
    /// (`claude-code`, `codex`, `gemini-cli`, `opencode`, `cursor`, `grok`, …).
    ///
    /// This is the user *stating* where a harness keeps its sessions, which is
    /// categorically different from the path registry *guessing* one. The
    /// registry's per-platform template and its `confidence` gate exist to stop
    /// the scanner walking a guessed path; neither applies to a path the user
    /// wrote down. So an entry here outranks the template and bypasses the
    /// `未查明` skip — see `scanner::probe_harness`.
    ///
    /// It does **not** relax "unknown is not empty": a configured path that
    /// does not exist still probes as missing and still reports an unknown
    /// session count, never `0`.
    pub harness_roots: BTreeMap<String, String>,
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
    /// The schedule command reads this value when rendering launchd/systemd
    /// templates; the scheduler itself remains an external one-shot runner.
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

    /// Named destinations. More than one may be declared; each carries its own
    /// repository and its own key.
    ///
    /// ADR-013: a destination is a *place a copy lives*, not a shard of one
    /// archive — so "add a destination" means "keep one more full copy". The
    /// singular `rustic_*` fields above stay as the single-destination
    /// (pre-ADR-013) mode; once this table is non-empty every command that
    /// reaches a repository has to say **which** one, because picking one
    /// silently is exactly the failure this table exists to prevent.
    pub destinations: BTreeMap<String, DestinationConfig>,
}

/// One named destination. Fields left unset fall back to the same defaults the
/// single-destination mode uses, so a destination can be declared with nothing
/// but a `repo`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DestinationConfig {
    /// Repository location (local path today, backend string later).
    pub repo: Option<String>,
    /// Masterkey file for *this* destination. Two destinations sharing one key
    /// file is legal but is not the default: each destination is expected to
    /// carry its own key.
    pub key_file: Option<String>,
    /// Per-destination concurrency cap (clamped like `rustic_connections`).
    pub connections: Option<usize>,
    /// Backend options forwarded verbatim (e.g. `endpoint` for `opendal:sftp`).
    pub options: BTreeMap<String, String>,
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
            Ok(raw) => match toml::from_str::<Config>(&raw) {
                Ok(mut cfg) => {
                    cfg.source = ConfigSource::File;
                    cfg
                }
                Err(e) => match recover_windows_paths(&raw)
                    .and_then(|fixed| toml::from_str::<Config>(&fixed).ok())
                {
                    Some(mut cfg) => {
                        eprintln!(
                            "warning: config 里有未转义的反斜杠路径（Windows 写法），已按字面路径读取: {}",
                            config_path().display()
                        );
                        eprintln!(
                            "         TOML 里 `\\` 是转义符；写成 'C:\\path' (单引号) 或 \"C:\\\\path\" 可去掉这条警告"
                        );
                        cfg.source = ConfigSource::FileAfterWindowsPathRepair;
                        cfg
                    }
                    None => {
                        eprintln!("warning: config is not valid TOML, using defaults: {e}");
                        Config {
                            source: ConfigSource::DefaultsAfterParseError,
                            ..Config::default()
                        }
                    }
                },
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // First run — no config yet. That is explicitly fine.
                Config::default()
            }
            Err(e) => {
                eprintln!("warning: could not read config, using defaults: {e}");
                Config {
                    source: ConfigSource::DefaultsAfterReadError,
                    ..Config::default()
                }
            }
        }
    }

    /// The store root the user explicitly declared for registry harness `id`,
    /// if any. Empty strings count as "not set" so a stray `foo = ""` cannot
    /// silently re-point a scan at the current directory.
    ///
    /// `harness_roots` wins over the two legacy single-harness fields, which
    /// remain aliases for the same thing.
    pub fn explicit_harness_root(&self, id: &str) -> Option<&str> {
        let legacy = match id {
            "claude-code" => self.claude_projects_dir.as_deref(),
            "codex" => self.codex_sessions_dir.as_deref(),
            _ => None,
        };
        self.harness_roots
            .get(id)
            .map(String::as_str)
            .or(legacy)
            .filter(|value| !value.is_empty())
    }

    /// Write a documented, commented template config file if one does not
    /// already exist. Never overwrites an existing file.
    ///
    /// Written by hand (not `toml::to_string`) so the file can carry comments
    /// explaining each option to a human reading it for the first time.
    pub fn init_default(template: &str) -> std::io::Result<()> {
        let path = config_path();
        if path.exists() {
            eprintln!("config already exists: {}", path.display());
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, template)?;
        eprintln!("wrote default config: {}", path.display());
        Ok(())
    }
}

/// Characters that may legally follow a backslash inside a TOML basic string,
/// excluding the two hex escapes (`\uXXXX` / `\UXXXXXXXX`) handled separately.
const TOML_SIMPLE_ESCAPES: [char; 7] = ['b', 't', 'n', 'f', 'r', '"', '\\'];

/// String-literal state while walking a config file. Only basic (double-quoted)
/// strings treat `\` as an escape, so only those may be rewritten.
#[derive(PartialEq, Eq, Clone, Copy)]
enum TomlSpan {
    Outside,
    Comment,
    Literal,
    MultiLiteral,
    Basic,
    MultiBasic,
}

/// True when `chars[i]` (a backslash) begins an escape TOML actually defines.
fn is_toml_escape(chars: &[char], i: usize) -> bool {
    let Some(&next) = chars.get(i + 1) else {
        return false;
    };
    if TOML_SIMPLE_ESCAPES.contains(&next) {
        return true;
    }
    // Line-ending backslash: in a multi-line basic string a backslash followed
    // by nothing but whitespace-to-end-of-line trims the newline. Doubling that
    // one would change a string TOML already reads correctly.
    if next == '\n' || next == '\r' {
        return true;
    }
    if next == ' ' || next == '\t' {
        return matches!(
            chars[i + 1..]
                .iter()
                .find(|c| **c != ' ' && **c != '\t')
                .copied(),
            Some('\n') | Some('\r')
        );
    }
    let digits = match next {
        'u' => 4,
        'U' => 8,
        _ => return false,
    };
    chars.len() > i + 1 + digits
        && chars[i + 2..i + 2 + digits]
            .iter()
            .all(|c| c.is_ascii_hexdigit())
}

/// Second chance for a config whose only problem is a Windows path pasted
/// verbatim into a basic string.
///
/// `cursor = "C:\Users\me\AppData\Roaming\Cursor\...\state.vscdb"` is the way a
/// Windows user naturally writes a path down — and it is not valid TOML, because
/// `\U` starts an 8-hex-digit escape. Rejecting the whole file over it means the
/// tool acts as if the user never stated where their store lives, which is the
/// same false claim `harness_roots` exists to prevent, one layer earlier.
///
/// So: every backslash that does **not** begin an escape TOML defines is doubled
/// (inside basic strings only — literal `'...'` strings, comments and bare keys
/// are copied untouched). Returns `None` when there was nothing to rewrite, so
/// the caller reports the original parse error rather than a misleading one.
///
/// This is strictly a *recovery* path: it only runs after a strict parse has
/// already failed, and it can never change the meaning of an escape sequence
/// TOML defines, because those are the ones it leaves alone.
fn recover_windows_paths(raw: &str) -> Option<String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len() + 32);
    let mut span = TomlSpan::Outside;
    let mut rewrote = false;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match span {
            TomlSpan::Outside => {
                if c == '#' {
                    span = TomlSpan::Comment;
                } else if c == '\'' {
                    if chars[i + 1..].starts_with(&['\'', '\'']) {
                        out.push_str("'''");
                        i += 3;
                        span = TomlSpan::MultiLiteral;
                        continue;
                    }
                    span = TomlSpan::Literal;
                } else if c == '"' {
                    if chars[i + 1..].starts_with(&['"', '"']) {
                        out.push_str("\"\"\"");
                        i += 3;
                        span = TomlSpan::MultiBasic;
                        continue;
                    }
                    span = TomlSpan::Basic;
                }
            }
            TomlSpan::Comment => {
                if c == '\n' {
                    span = TomlSpan::Outside;
                }
            }
            // Literal strings keep backslashes verbatim — nothing to fix, and
            // rewriting one would change its value.
            TomlSpan::Literal => {
                if c == '\'' || c == '\n' {
                    span = TomlSpan::Outside;
                }
            }
            TomlSpan::MultiLiteral => {
                if c == '\'' && chars[i + 1..].starts_with(&['\'', '\'']) {
                    out.push_str("'''");
                    i += 3;
                    span = TomlSpan::Outside;
                    continue;
                }
            }
            TomlSpan::Basic | TomlSpan::MultiBasic => {
                if c == '\\' {
                    if is_toml_escape(&chars, i) {
                        // Copy the escape lead-in whole so its payload is never
                        // re-examined as if it were text.
                        out.push('\\');
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    out.push_str("\\\\");
                    rewrote = true;
                    i += 1;
                    continue;
                }
                if c == '"' {
                    if span == TomlSpan::MultiBasic {
                        if chars[i + 1..].starts_with(&['"', '"']) {
                            out.push_str("\"\"\"");
                            i += 3;
                            span = TomlSpan::Outside;
                            continue;
                        }
                    } else {
                        span = TomlSpan::Outside;
                    }
                } else if c == '\n' && span == TomlSpan::Basic {
                    // Unterminated basic string; the strict error stands.
                    span = TomlSpan::Outside;
                }
            }
        }
        out.push(c);
        i += 1;
    }

    rewrote.then_some(out)
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
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home);
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
        return PathBuf::from(profile);
    }
    // Never turn a missing identity into `.`: that would make every default
    // harness path point at the caller's working tree. The per-process temp
    // quarantine is deliberately not presented as a real home and is normally
    // absent, so probes remain unknown instead of reading the repository.
    std::env::temp_dir().join(format!(
        "chat-stasher-home-unavailable-{}",
        std::process::id()
    ))
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
# The scheduler template reads this value; it is not installed automatically.
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

# ---------------------------------------------------------------- harness_roots
# Tell the tool where a harness actually keeps its sessions, keyed by the
# registry harness id. Use this when your install is not where the shipped path
# registry looks — or when the registry has no verified path for your platform
# at all (it then refuses to guess, and reports "unknown" rather than 0).
#
# A path you write here is a statement, not a guess: it is probed even when the
# registry cell for your platform is missing or marked 未查明. If the path does
# not exist, the count stays "unknown" — it never becomes 0.
#
# Single-file (SQLite) harnesses want the file itself; directory harnesses want
# the directory.
#
# Windows paths: `\` is TOML's escape character inside "double quotes", so
# prefer single quotes — 'C:\Users\me\AppData\Roaming\Cursor\User\globalStorage\state.vscdb'
# (or double every backslash). A path pasted verbatim into double quotes is
# still read, with a warning, rather than dropping your whole config.
#
# [harness_roots]
# cursor = "~/.config/Cursor/User/globalStorage/state.vscdb"
# grok = "~/.grok/sessions/session_search.sqlite"
# opencode = "~/.local/share/opencode/opencode.db"

# ---------------------------------------------------------------- destinations
# Named destinations. Declare as many as you keep copies in. Each one is a
# *full copy*, not a shard: `dest-init` gives a new destination the union of
# your local sources and what your existing destinations already hold.
#
# Once this table is non-empty, commands that reach a repository require
# `--destination <name>` (or an explicit `--repo`). There is deliberately no
# default destination: retrieval must name the copy it is reading.
#
# [destinations.laptop]
# repo = "~/stash/chat-stasher/repo"
# key_file = "~/stash/chat-stasher/masterkey.json"
#
# [destinations.storagebox]
# repo = "opendal:sftp"
# key_file = "~/stash/chat-stasher/masterkey-storagebox.json"
# connections = 4
# [destinations.storagebox.options]
# endpoint = "ssh://example:23"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The exact shape that made `doctor_consistency_test` red on
    /// `windows-latest`: a Windows path pasted verbatim into a basic string.
    #[test]
    fn windows_path_in_basic_string_is_recovered() {
        let raw = "[harness_roots]\ncursor = \"C:\\Users\\me\\AppData\\Roaming\\Cursor\\User\\globalStorage\\state.vscdb\"\n";
        assert!(
            toml::from_str::<Config>(raw).is_err(),
            "前提：这份 config 严格解析必须是失败的，否则本恢复路径根本不会跑"
        );
        let fixed = recover_windows_paths(raw).expect("应当识别出未转义的反斜杠");
        let cfg: Config = toml::from_str(&fixed).expect("补转义后应当解析成功");
        assert_eq!(
            cfg.explicit_harness_root("cursor"),
            Some("C:\\Users\\me\\AppData\\Roaming\\Cursor\\User\\globalStorage\\state.vscdb"),
            "恢复出来的必须是用户写下的那条字面路径"
        );
    }

    /// Recovery must not touch a file that parses: escapes TOML defines keep
    /// their meaning, and a literal `'...'` string is copied byte for byte.
    #[test]
    fn defined_escapes_and_literal_strings_are_left_alone() {
        let raw = "a = \"line\\nbreak\\tand \\u0041\"\nb = 'C:\\Users\\me'\n# comment C:\\x\n";
        assert!(
            recover_windows_paths(raw).is_none(),
            "没有可修的反斜杠时必须返回 None，好让调用方报原始错误"
        );
    }

    /// A config that is broken for some *other* reason must not be silently
    /// "recovered" into something that parses — the original error stands.
    #[test]
    fn unrelated_syntax_error_is_not_recovered() {
        let raw = "[harness_roots\ncursor = \"C:\\Users\\me\"\n";
        let recovered = recover_windows_paths(raw)
            .map(|fixed| toml::from_str::<Config>(&fixed).is_ok())
            .unwrap_or(false);
        assert!(!recovered, "括号都没闭合，不该被这条恢复路径救活");
    }

    /// Recovery is a spelling fix, never a permission slip: a path that does not
    /// exist is still just a path — nothing here invents a count or a store.
    #[test]
    fn recovery_only_changes_spelling_not_meaning() {
        let raw = "[harness_roots]\ngrok = \"C:\\missing\\store.sqlite\"\n";
        let fixed = recover_windows_paths(raw).unwrap();
        let cfg: Config = toml::from_str(&fixed).unwrap();
        assert_eq!(
            cfg.explicit_harness_root("grok"),
            Some("C:\\missing\\store.sqlite")
        );
        assert!(!Path::new(cfg.explicit_harness_root("grok").unwrap()).exists());
    }

    /// The boundary this recovery deliberately does **not** cross: a path whose
    /// every backslash sequence happens to be a TOML escape (`\n`, `\t`, …) is
    /// valid TOML already, so the strict parse succeeds and recovery is never
    /// consulted. `"C:\new"` therefore still means `C:` + newline + `ew`.
    /// Guessing otherwise would be overriding a file that parsed — the opposite
    /// of taking the user at their word.
    #[test]
    fn a_path_that_is_already_valid_toml_is_not_second_guessed() {
        let raw = "[harness_roots]\ngrok = \"C:\\new\\temp.sqlite\"\n";
        let cfg: Config = toml::from_str(raw).expect("这份 config 严格解析本来就成立");
        assert_eq!(
            cfg.explicit_harness_root("grok"),
            Some("C:\new\temp.sqlite")
        );
        assert!(recover_windows_paths(raw).is_none());
    }
}
