//! Configuration handling for chat-stasher.
//!
//! The config lives at `~/.config/chat-stasher/config.toml`. Per the spike
//! requirements, a missing file is fine — the tool falls back to defaults
//! instead of erroring out. Only a *broken* TOML (or an unreadable file for a
//! reason other than "not there") is worth warning about, and even then we
//! degrade to defaults rather than aborting a scan.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    /// Explicitly pin this machine's archive partition name (ADR-018).
    ///
    /// Before ADR-018 the partition was derived from the hostname, which
    /// silently merged every machine with the same default hostname (e.g. two
    /// Macs both named `Mac`) into one partition. Now a fresh install uses a
    /// random 128-bit identity instead, and **existing** installs keep their
    /// old partition by setting `machine = "<partition-name>"` here — the one
    /// line that protects an archive built before the change. New installs
    /// leave it empty and an identity is generated automatically. An empty
    /// value counts as unset.
    pub machine: Option<String>,
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
    /// `unascertained` skip — see `scanner::probe_harness`.
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
                    expand_config_paths(&mut cfg);
                    cfg
                }
                Err(e) => match recover_windows_paths(&raw)
                    .and_then(|fixed| toml::from_str::<Config>(&fixed).ok())
                {
                    Some(mut cfg) => {
                        eprintln!(
                            "warning: config contains unescaped backslash paths (Windows spelling), read as literal paths: {}",
                            config_path().display()
                        );
                        eprintln!(
                            "         `\\` is the escape character in TOML; write 'C:\\path' (single quotes) or \"C:\\\\path\" to silence this warning"
                        );
                        cfg.source = ConfigSource::FileAfterWindowsPathRepair;
                        expand_config_paths(&mut cfg);
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

    /// Expand every path-typed field in place. Fields whose `~` cannot be
    /// expanded (missing home, `~otheruser`) — or that still contain a literal
    /// `~` component after expansion — are *dropped to their default* (`None`,
    /// an empty/removed map entry) and the problem is recorded in `problems`.
    /// A literal `~` must never survive into a path the tool then hands to the
    /// filesystem.
    ///
    /// Called from [`Config::load`] so every consumer of the effective config
    /// sees expanded paths and no consumer has to remember to expand.
    fn expand_all_paths(&mut self, problems: &mut Vec<String>) {
        expand_opt_field("archive_root", &mut self.archive_root, problems);
        expand_opt_field(
            "claude_projects_dir",
            &mut self.claude_projects_dir,
            problems,
        );
        expand_opt_field("codex_sessions_dir", &mut self.codex_sessions_dir, problems);
        expand_opt_field("rustic_repo", &mut self.rustic_repo, problems);
        expand_opt_field("rustic_key_file", &mut self.rustic_key_file, problems);

        let mut bad_harness_roots: Vec<String> = Vec::new();
        for (id, root) in &mut self.harness_roots {
            match expand_and_verify(root) {
                Ok(path) => *root = path.to_string_lossy().into_owned(),
                Err(e) => {
                    problems.push(format!("harness_roots.{id}: {e}"));
                    bad_harness_roots.push(id.clone());
                }
            }
        }
        for id in bad_harness_roots {
            self.harness_roots.remove(&id);
        }

        for (name, dest) in &mut self.destinations {
            let repo_label = format!("destinations.{name}.repo");
            expand_opt_field(&repo_label, &mut dest.repo, problems);
            let key_label = format!("destinations.{name}.key_file");
            expand_opt_field(&key_label, &mut dest.key_file, problems);
            // Backend option values are forwarded verbatim to rustic, but a
            // value that names a local file (e.g. `key`/`root` for
            // `opendal:sftp`) is a path and gets the same `~` handling. Values
            // without a leading `~` are untouched by `expand_and_verify`.
            let mut bad_options: Vec<String> = Vec::new();
            for (key, value) in &mut dest.options {
                let label = format!("destinations.{name}.options.{key}");
                match expand_and_verify(value) {
                    Ok(path) => *value = path.to_string_lossy().into_owned(),
                    Err(e) => {
                        problems.push(format!("{label}: {e}"));
                        bad_options.push(key.clone());
                    }
                }
            }
            for key in bad_options {
                dest.options.remove(&key);
            }
        }
    }
}

/// Run the post-parse path expansion over an effective config, printing a
/// warning for every field that had to be reset to its default. Loading never
/// aborts over a bad `~` (a scan must not be blocked by a config typo), but it
/// also never lets a literal `~` through: the field is dropped and the reason
/// is printed.
fn expand_config_paths(cfg: &mut Config) {
    let mut problems: Vec<String> = Vec::new();
    cfg.expand_all_paths(&mut problems);
    for problem in &problems {
        eprintln!(
            "warning: the `~` in a config path could not be expanded; the option was reset to its default (a literal `~` is never written as a path): {problem}"
        );
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
    match home_from_env() {
        Some(home) => PathBuf::from(home),
        None => {
            // Never turn a missing identity into `.`: that would make every
            // default harness path point at the caller's working tree. The
            // per-process temp quarantine is deliberately not presented as a
            // real home and is normally absent, so probes remain unknown
            // instead of reading the repository.
            std::env::temp_dir().join(format!(
                "chat-stasher-home-unavailable-{}",
                std::process::id()
            ))
        }
    }
}

/// The current user's home directory from the environment, when one is
/// declared. `$HOME` first (Unix convention), `$USERPROFILE` second (Windows
/// convention); an empty value counts as unset. `None` when neither is set —
/// the caller decides whether that is fatal.
fn home_from_env() -> Option<String> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(|value| value.to_string_lossy().into_owned())
}

/// Why a `~` in a path could not be expanded.
///
/// This is deliberately a hard error rather than a best effort: the failure
/// being guarded against — writing a masterkey into a literal `~` directory in
/// the current working directory — is worse than refusing to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TildeError {
    /// `~` / `~/` seen but no home directory is available
    /// (`$HOME` and `$USERPROFILE` both unset or empty).
    MissingHome { input: String },
    /// `~somebody/...` — another user's home. Only the current user's `~` is
    /// supported, so this is rejected rather than silently treated as a
    /// relative path (which is exactly how `~` ends up as a literal directory).
    OtherUserTilde { input: String },
    /// A path that was about to reach the filesystem still contains a literal
    /// `~` component — i.e. a `~` that was never expanded. See
    /// [`assert_no_literal_tilde`].
    LiteralTildeRemains { path: String },
}

impl std::fmt::Display for TildeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingHome { input } => write!(
                f,
                "`{input}` starts with `~` but neither $HOME nor $USERPROFILE is set, so it cannot be expanded to a home directory"
            ),
            Self::OtherUserTilde { input } => write!(
                f,
                "`{input}` is `~username` form; this tool only supports the current user's `~` (not another user's home); use an absolute path"
            ),
            Self::LiteralTildeRemains { path } => write!(
                f,
                "`{path}` still contains a literal `~` component — almost certainly a tilde that was never expanded; start with `~/` or use an absolute path"
            ),
        }
    }
}

impl std::error::Error for TildeError {}

/// Expand a leading `~` / `~/` to the current user's home directory.
///
/// This is the single place a config (or CLI) path's `~` is resolved. Rules:
///
/// - `~` and `~/...` resolve to the home directory.
/// - Windows: `~\...` is accepted as well (the natural spelling there), and
///   the remainder is joined with the platform separator, so
///   `~/AppData/Roaming/...` works too. Note that backslashes in the *config
///   file* have no meaning here — TOML escaping is handled by the parser; by
///   the time a string reaches this function it is the literal path text.
/// - `~somebody` (another user's home) is **rejected** (`OtherUserTilde`),
///   never silently treated as a relative path, because that is precisely how
///   a literal `~` directory gets created in the current working directory.
/// - When no home is available the function **errors** (`MissingHome`) rather
///   than return a literal `~`: a `~` that reaches the filesystem is the bug
///   this whole module exists to prevent. Callers that must not abort (config
///   load) drop the field to its default and warn; callers at the disk
///   boundary (repo/key resolution) treat the error as fatal.
/// - A `~` that is *not* the first character (e.g. `stash/~/x`) is left alone
///   here; [`assert_no_literal_tilde`] is the check that rejects those.
pub fn expand_tilde(s: &str) -> Result<PathBuf, TildeError> {
    expand_tilde_with_home(s, home_from_env().as_deref())
}

/// [`expand_tilde`] with the home directory supplied explicitly, so the
/// `$HOME`-missing branch is testable without mutating process env vars.
fn expand_tilde_with_home(s: &str, home: Option<&str>) -> Result<PathBuf, TildeError> {
    if s == "~" {
        return home
            .map(PathBuf::from)
            .ok_or_else(|| TildeError::MissingHome {
                input: s.to_string(),
            });
    }
    if let Some(rest) = s.strip_prefix('~') {
        let rest = match rest.strip_prefix('/') {
            Some(rest) => rest,
            None => match rest.strip_prefix('\\') {
                Some(rest) => rest,
                None => {
                    return Err(TildeError::OtherUserTilde {
                        input: s.to_string(),
                    });
                }
            },
        };
        let home = home.ok_or_else(|| TildeError::MissingHome {
            input: s.to_string(),
        })?;
        return Ok(PathBuf::from(home).join(rest));
    }
    Ok(PathBuf::from(s))
}

/// Reject a path that still contains a literal `~` component.
///
/// [`expand_tilde`] only expands a *leading* `~`; a `~` in the middle of a
/// path (e.g. `stash/~/x`) or a `~alice` component in a CLI override would
/// otherwise reach the filesystem as a directory literally named `~`. A `~`
/// component almost always means the tilde was written but never expanded —
/// the exact failure that once wrote a masterkey into a literal `~` directory
/// in the current working directory.
pub fn assert_no_literal_tilde(p: &Path) -> Result<(), TildeError> {
    for component in p.components() {
        let name = component.as_os_str().to_string_lossy();
        if name.starts_with('~') {
            return Err(TildeError::LiteralTildeRemains {
                path: p.display().to_string(),
            });
        }
    }
    Ok(())
}

/// Expand a leading `~` and then reject any residual literal `~` component.
///
/// The combined guarantee: the returned path contains no `~` component at all,
/// so it is safe to hand to the filesystem. Config load and the repo/key
/// resolution boundary both use this.
pub fn expand_and_verify(value: &str) -> Result<PathBuf, TildeError> {
    let path = expand_tilde(value)?;
    assert_no_literal_tilde(&path)?;
    Ok(path)
}

/// Expand an `Option<String>` path field in place via [`expand_and_verify`].
/// On failure, clear the field (`None`) and record a warning — a literal `~`
/// must never survive into a path the tool then hands to the filesystem.
fn expand_opt_field(label: &str, value: &mut Option<String>, problems: &mut Vec<String>) {
    let Some(raw) = value.as_deref() else { return };
    match expand_and_verify(raw) {
        Ok(path) => *value = Some(path.to_string_lossy().into_owned()),
        Err(e) => {
            problems.push(format!("{label}: {e}"));
            *value = None;
        }
    }
}

/// The template written by `init` — comments explain each knob.
pub const DEFAULT_CONFIG_TEMPLATE: &str = r#"# chat-stasher configuration
#
# Every value is optional. Omitted values fall back to the per-machine
# standard locations for each harness.
#
# Path values: a leading `~/` expands to your home directory (`~` alone is the
# home directory too). Windows: `~\` is accepted, and `/`-separated relative
# tails like `~/AppData/Roaming/...` work as well. `~username` (another user's
# home) is not supported and is rejected. If no home is available ($HOME and
# $USERPROFILE are both unset) the option is reset to its default with a
# warning — a literal `~` directory is never created.

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

# Explicitly pin this machine's archive partition name.
# Existing installs set this to keep the partition they are already writing to;
# new installs leave it empty and a random identity is generated automatically.
#
# Semantics: the archive partition is no longer derived from the hostname
# (two machines with the same default hostname used to merge into one
# partition). A fresh install generates a random 128-bit identity and uses it
# as the partition; you never need to write this line. An *existing* install
# that wants to keep the partition it has been writing to must set this to that
# partition name — this is the one line that protects an archive built before
# the change. Leaving it empty is correct for new installs.
# machine = "..."

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
# registry cell for your platform is missing or marked unascertained. If the
# path does not exist, the count stays "unknown" — it never becomes 0.
#
# Single-file (SQLite) harnesses want the file itself; directory harnesses want
# the directory.
#
# Windows paths: `\` is TOML's escape character inside "double quotes", so
# prefer single quotes — 'C:\Users\me\AppData\Roaming\Cursor\User\globalStorage\state.vscdb'
# (or double every backslash). A path pasted verbatim into double quotes is
# still read, with a warning, rather than dropping your whole config.
# A leading `~/` (or `~\`) works here too and expands to your home directory.
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
    use std::env;
    use std::path::Path;
    use std::sync::Mutex;

    /// Serialises tests that mutate process env vars — cargo runs tests in
    /// parallel threads and `set_var` is process-global. (Same pattern as the
    /// scanner tests.)
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// The exact shape that made `doctor_consistency_test` red on
    /// `windows-latest`: a Windows path pasted verbatim into a basic string.
    #[test]
    fn windows_path_in_basic_string_is_recovered() {
        let raw = "[harness_roots]\ncursor = \"C:\\Users\\me\\AppData\\Roaming\\Cursor\\User\\globalStorage\\state.vscdb\"\n";
        assert!(
            toml::from_str::<Config>(raw).is_err(),
            "precondition: this config must fail strict parsing, or the recovery path would not run"
        );
        let fixed = recover_windows_paths(raw).expect("should detect unescaped backslashes");
        let cfg: Config = toml::from_str(&fixed).expect("should parse after escaping backslashes");
        assert_eq!(
            cfg.explicit_harness_root("cursor"),
            Some("C:\\Users\\me\\AppData\\Roaming\\Cursor\\User\\globalStorage\\state.vscdb"),
            "recovered path must be the literal path the user wrote"
        );
    }

    /// Recovery must not touch a file that parses: escapes TOML defines keep
    /// their meaning, and a literal `'...'` string is copied byte for byte.
    #[test]
    fn defined_escapes_and_literal_strings_are_left_alone() {
        let raw = "a = \"line\\nbreak\\tand \\u0041\"\nb = 'C:\\Users\\me'\n# comment C:\\x\n";
        assert!(
            recover_windows_paths(raw).is_none(),
            "must return None when there is nothing to fix so the caller reports the original error"
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
        assert!(
            !recovered,
            "unclosed bracket must not be rescued by this recovery path"
        );
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
        let cfg: Config =
            toml::from_str(raw).expect("this config is already valid under strict parsing");
        assert_eq!(
            cfg.explicit_harness_root("grok"),
            Some("C:\new\temp.sqlite")
        );
        assert!(recover_windows_paths(raw).is_none());
    }

    // -----------------------------------------------------------------------
    // 统一的 `~` 展开（`expand_tilde` / `assert_no_literal_tilde`）
    // -----------------------------------------------------------------------

    #[test]
    fn expand_tilde_expands_home_and_tilde_slash() {
        let home = tempfile::TempDir::new().unwrap();
        let home_s = home.path().to_string_lossy();
        assert_eq!(
            expand_tilde_with_home("~", Some(&home_s)),
            Ok(home.path().to_path_buf())
        );
        assert_eq!(
            expand_tilde_with_home("~/x/y", Some(&home_s)),
            Ok(home.path().join("x/y"))
        );
        // Windows 的 `~\` 拼写也接受（在每个平台上都按家目录展开）。
        assert_eq!(
            expand_tilde_with_home("~\\x", Some(&home_s)),
            Ok(home.path().join("x"))
        );
        // 不带 `~` 的路径原样返回。
        assert_eq!(
            expand_tilde_with_home("/abs/path", Some(&home_s)),
            Ok(PathBuf::from("/abs/path"))
        );
        assert_eq!(
            expand_tilde_with_home("rel/path", Some(&home_s)),
            Ok(PathBuf::from("rel/path"))
        );
    }

    #[test]
    fn expand_tilde_env_based_home() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::TempDir::new().unwrap();
        env::set_var("HOME", home.path());
        env::remove_var("USERPROFILE");
        assert_eq!(expand_tilde("~").unwrap(), home.path().to_path_buf());
        assert_eq!(expand_tilde("~/x").unwrap(), home.path().join("x"));
    }

    #[test]
    fn expand_tilde_missing_home_is_an_error_not_a_literal_tilde() {
        assert_eq!(
            expand_tilde_with_home("~/x", None),
            Err(TildeError::MissingHome {
                input: "~/x".into()
            })
        );
        assert_eq!(
            expand_tilde_with_home("~", None),
            Err(TildeError::MissingHome { input: "~".into() })
        );
    }

    #[test]
    fn expand_tilde_rejects_other_user_tilde() {
        let home = tempfile::TempDir::new().unwrap();
        let home_s = home.path().to_string_lossy();
        assert_eq!(
            expand_tilde_with_home("~alice/x", Some(&home_s)),
            Err(TildeError::OtherUserTilde {
                input: "~alice/x".into()
            })
        );
        assert_eq!(
            expand_tilde_with_home("~someone", Some(&home_s)),
            Err(TildeError::OtherUserTilde {
                input: "~someone".into()
            })
        );
    }

    #[test]
    fn assert_no_literal_tilde_rejects_unexpanded_component() {
        // 人为构造的“展开漏网”路径：中间的 `~`、`~用户名` 组件都必须被拒。
        assert!(assert_no_literal_tilde(Path::new("stash/~/x")).is_err());
        assert!(assert_no_literal_tilde(Path::new("/home/me/~alice")).is_err());
        // 展开后的正常绝对路径必须放行。
        let home = tempfile::TempDir::new().unwrap();
        assert!(assert_no_literal_tilde(&home.path().join("x")).is_ok());
        assert!(assert_no_literal_tilde(Path::new("/home/me/real")).is_ok());
        // 文件名里带 `~` 但不是组件开头，是合法文件名，不误伤。
        assert!(assert_no_literal_tilde(Path::new("/home/me/foo~bar")).is_ok());
    }

    #[test]
    fn expand_and_verify_catches_mid_path_tilde() {
        assert!(expand_and_verify("stash/~/x").is_err());
        assert!(expand_and_verify("~/ok").is_ok());
    }

    // -----------------------------------------------------------------------
    // 载入时逐字段展开 + 缺失 `$HOME` 时按默认值处理
    // -----------------------------------------------------------------------

    #[test]
    fn load_expands_tilde_in_every_path_field() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::TempDir::new().unwrap();
        let xdg = tempfile::TempDir::new().unwrap();
        env::set_var("HOME", home.path());
        env::set_var("XDG_CONFIG_HOME", xdg.path());
        env::remove_var("USERPROFILE");

        let cfg_dir = xdg.path().join("chat-stasher");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            r#"
archive_root = "~/arch"
claude_projects_dir = "~/cproj"
codex_sessions_dir = "~/csess"
rustic_repo = "~/repo"
rustic_key_file = "~/key.json"

[harness_roots]
claude-code = "~/cc"
grok = "~/groks/s.sqlite"

[destinations.d1]
repo = "~/dest/repo"
key_file = "~/dest/key.json"
[destinations.d1.options]
key = "~/dest/ssh-key"
root = "~/dest/remote"
"#,
        )
        .unwrap();

        let cfg = Config::load();
        let h = home.path();
        assert_eq!(
            cfg.archive_root.as_deref(),
            Some(h.join("arch").to_str().unwrap())
        );
        assert_eq!(
            cfg.claude_projects_dir.as_deref(),
            Some(h.join("cproj").to_str().unwrap())
        );
        assert_eq!(
            cfg.codex_sessions_dir.as_deref(),
            Some(h.join("csess").to_str().unwrap())
        );
        assert_eq!(
            cfg.rustic_repo.as_deref(),
            Some(h.join("repo").to_str().unwrap())
        );
        assert_eq!(
            cfg.rustic_key_file.as_deref(),
            Some(h.join("key.json").to_str().unwrap())
        );
        assert_eq!(
            cfg.explicit_harness_root("claude-code"),
            Some(h.join("cc").to_str().unwrap())
        );
        assert_eq!(
            cfg.explicit_harness_root("grok"),
            Some(h.join("groks/s.sqlite").to_str().unwrap())
        );

        let d1 = cfg.destinations.get("d1").expect("d1 present");
        assert_eq!(
            d1.repo.as_deref(),
            Some(h.join("dest/repo").to_str().unwrap())
        );
        assert_eq!(
            d1.key_file.as_deref(),
            Some(h.join("dest/key.json").to_str().unwrap())
        );
        assert_eq!(
            d1.options.get("key").map(String::as_str),
            Some(h.join("dest/ssh-key").to_str().unwrap())
        );
        assert_eq!(
            d1.options.get("root").map(String::as_str),
            Some(h.join("dest/remote").to_str().unwrap())
        );
    }

    #[test]
    fn load_drops_tilde_field_when_home_missing() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let xdg = tempfile::TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", xdg.path());
        env::remove_var("HOME");
        env::remove_var("USERPROFILE");

        let cfg_dir = xdg.path().join("chat-stasher");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            r#"
rustic_repo = "~/repo"
[destinations.d1]
key_file = "~/dest/key.json"
"#,
        )
        .unwrap();

        let cfg = Config::load();
        // Field reset to default (None); a literal `~` must never remain as a path.
        assert_eq!(cfg.rustic_repo, None);
        let d1 = cfg.destinations.get("d1").expect("d1 present");
        assert_eq!(d1.key_file, None);
    }

    #[test]
    fn default_template_is_valid_toml() {
        // All examples in the template are comments, so parsing must yield an
        // empty Config (and not panic).
        let cfg: Config =
            toml::from_str(DEFAULT_CONFIG_TEMPLATE).expect("the template itself must parse");
        assert!(cfg.harness_roots.is_empty());
        assert!(cfg.destinations.is_empty());
    }

    /// ADR-018: the `machine` field pins an existing install's partition name.
    #[test]
    fn machine_field_parses_and_template_documents_it() {
        let cfg: Config = toml::from_str("machine = \"mac\"\n").unwrap();
        assert_eq!(cfg.machine.as_deref(), Some("mac"));
        // An empty value must be treated as unset, never as a partition named "".
        let cfg: Config = toml::from_str("machine = \"\"\n").unwrap();
        assert!(
            cfg.machine.as_deref().map(str::is_empty).unwrap_or(false),
            "an empty machine value is allowed to parse and is filtered at use site"
        );
        assert!(
            DEFAULT_CONFIG_TEMPLATE.contains("machine = \"...\""),
            "the init template must document the machine field"
        );
    }
}
