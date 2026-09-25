//! Configuration handling for chat-stasher.
//!
//! The config lives at `~/.config/chat-stasher/config.toml`. A **missing** file
//! is the normal first-run state and is not an error: the tool runs with the
//! built-in defaults. A file that **exists but cannot be used** — unreadable, not
//! valid TOML, or carrying a path this tool cannot resolve — is a hard error, and
//! [`Config::load`] returns it rather than substituting defaults.
//!
//! The asymmetry is the point. `[destinations]` lists the places a copy of the
//! archive lives, so replacing a file the user wrote with the built-in defaults
//! *empties that list*: a scheduled `push` then runs as though no remote
//! destination had ever been declared, and the archive looks complete on a
//! machine whose copy was never written. "This config says nothing" and "I could
//! not read this config" are two different states (`CLAUDE.md`, invariant 1) and
//! a typo is not a statement that the user keeps no copies.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Directory + filename of the config relative to the user's config home
/// (`$XDG_CONFIG_HOME`, falling back to `~/.config`).
pub const CONFIG_RELATIVE_PATH: &str = "chat-stasher/config.toml";

/// Where the effective configuration came from.
///
/// A missing file is the normal first-run default. `Unreadable` is a different
/// thing entirely: it records that *no* effective configuration exists because
/// the file is there and could not be used, and it is the value `doctor` reports
/// in that case. Nothing ever runs on defaults under it — that is what
/// [`Config::load`] returning `Err` prevents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigSource {
    File,
    FileAfterWindowsPathRepair,
    #[default]
    DefaultsMissing,
    /// The file exists but could not be read, parsed or resolved, so there is no
    /// effective configuration and the command did not run.
    Unreadable,
}

impl ConfigSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::FileAfterWindowsPathRepair => "file_after_windows_path_repair",
            Self::DefaultsMissing => "defaults_missing",
            Self::Unreadable => "unreadable",
        }
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
    /// Where rustic keeps this machine's local metadata cache.
    ///
    /// rustic caches snapshot / index / *tree* packs — metadata, never the
    /// data packs — under a per-machine directory (`dirs::cache_dir()/rustic`,
    /// e.g. `~/Library/Caches/rustic` on macOS). It is purely a speed-up; it
    /// holds no conversation content. `None` follows rustic's own per-platform
    /// default. Ignored when [`rustic_no_cache`](Self::rustic_no_cache) is set.
    pub rustic_cache_dir: Option<String>,
    /// Completely disable the local metadata cache (rustic `no_cache`).
    ///
    /// Default `false`. The cache holds metadata only — snapshots, index and
    /// *tree* packs — so turning it off loses no archive content; every open
    /// just re-reads that metadata instead of keeping it between runs. Turn it
    /// on to stop the cache directory from growing.
    pub rustic_no_cache: Option<bool>,

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

    /// Native Messaging host settings (ADR-025, protocol v1).
    ///
    /// The whole section is optional, and every key inside it is optional too:
    /// "no `[native_host] stage`" is a *fourth* state, distinct from
    /// "declared but the path is gone". The host answers the first with `nack`
    /// `config` and the second with `nack` `stage-unavailable`, because the fix
    /// a user has to apply is different in each case.
    pub native_host: Option<NativeHostConfig>,

    /// Why `[cache]` could not be read, when it could not.
    ///
    /// Runtime metadata, like [`source`](Self::source), not part of the TOML
    /// schema — a `cache_error` line in the file is ignored.
    ///
    /// `None` means the section was absent (so the documented default quota
    /// applies) or read cleanly. `Some` means it was there and at least one of
    /// its values could not be read, and then the body cache is **off**: a
    /// mistyped quota must not activate a cache nobody asked for, and it must
    /// not take the rest of the file — `[destinations]`, the harness roots —
    /// down with it. `doctor` and `read` both report this instead of a quota.
    #[serde(skip)]
    pub cache_error: Option<String>,

    /// The `[cache]` section: this machine's body cache (ADR-034).
    ///
    /// Distinct from `rustic_cache_dir` / `rustic_no_cache` above, which are
    /// rustic's own **metadata** cache (snapshots, index, tree packs). This one
    /// holds conversation **bodies** — the bytes that made a warm `read` of a
    /// 107 MB session cost 22.6 s (W117) — and is one quota shared by every
    /// destination on this machine, never an allowance per destination.
    pub cache: Option<CacheSectionConfig>,
}

/// The `[cache]` section (ADR-034).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheSectionConfig {
    /// How much disk this machine's body cache may occupy, before the least
    /// recently used entries are evicted.
    ///
    /// Default: [`crate::body_cache::DEFAULT_MAX_BYTES`] (2 GiB). Write it as a
    /// plain byte count, or with a unit: `"50GB"` is 50 × 10⁹ bytes and
    /// `"50GiB"` is 50 × 2³⁰; both spellings are accepted so that a value meant
    /// as one thing cannot be read as the other. `0` turns the cache off
    /// entirely — reads then go to the remote as they did before ADR-034.
    ///
    /// The quota is not a promise about disk usage: it is enforced against the
    /// bytes of the entry files, and the cache is disposable, so `cache clear`
    /// reclaims all of it.
    pub max_bytes: Option<crate::body_cache::CacheSize>,
    /// Where the entries live. Default: this platform's cache directory
    /// (`~/Library/Caches/chat-stasher/body` on macOS), which is deliberate —
    /// ADR-034 requires a cache directory that takes part in no synchronisation
    /// and no backup-of-record.
    pub dir: Option<String>,
}

/// The `[native_host]` section.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NativeHostConfig {
    /// Absolute path of the stage the browser-spawned host writes to.
    ///
    /// Written by `chat-stasher install-native-host --stage <path>`. It is an
    /// absolute path on purpose: the browser starts the host with a working
    /// directory it chooses, so a relative value would resolve against a
    /// directory the user never named. The host never creates this directory —
    /// a stage that appears because a host was pointed at it is a stage
    /// nothing pushes.
    pub stage: Option<String>,
    /// Which declared destination `open_dashboard` opens (protocol §6.5), and —
    /// since W156 — the default the `ui` command uses when the config declares
    /// several and the command line named none.
    ///
    /// Hand-written, and deliberately the *only* way the extension's message
    /// learns a destination: ADR-013 forbids a default destination for
    /// retrieval, the extension that triggers the launch has no business
    /// naming — or choosing between — copies of the archive, and absent still
    /// means the dashboard cannot be opened from the popup (the host reports
    /// `nack` `config` rather than falling back to "there is only one, so it
    /// must be that one"). The `ui` command honours the same knob because one
    /// knob must keep one meaning: it opens the only declared destination
    /// without being told, and with several it opens only the one named here —
    /// or asks. Retrieval commands (`search`, `export`, `overview`) still name
    /// the copy, always.
    pub destination: Option<String>,
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
    /// Per-destination rustic cache root (overrides `rustic_cache_dir`).
    pub cache_dir: Option<String>,
    /// Per-destination `rustic_no_cache` (overrides the singular field).
    pub no_cache: Option<bool>,
}

/// What [`Config::set_native_host_stage`] did to the file.
///
/// `Unchanged` is deliberately a distinct outcome from `Updated`: it is the
/// only one that wrote no bytes, and a caller that prints "updated" for a
/// no-op teaches the user not to believe the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageKeyWrite {
    /// `[native_host] stage` was not in the file and is now.
    Added,
    /// It was there with a different value; `previous` is what it held.
    Updated { previous: String },
    /// It already held exactly this value. Nothing was written.
    Unchanged,
}

/// What [`Config::set_destination`] did to the file.
///
/// There is deliberately no `Updated` variant. A destination that is already
/// declared is the user's — `rustic init`'s precedent, which ADR-039 cites — and
/// a wizard that rewrote it could silently repoint an archive at a different
/// bucket or drop options it does not know about. So the only two outcomes are
/// "added a block that was not there" and "left a block that was".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationWrite {
    /// `[destinations.<name>]` was not in the file and now is.
    Added,
    /// `[destinations.<name>]` was already in the file. Nothing was written.
    AlreadyDeclared,
}

/// Replace `path` with `bytes` through a temp file in the same directory plus a
/// rename, so a reader — or a crash — sees either the old file or the new one,
/// never a half-written one. The temp name is dot-prefixed and pid-suffixed:
/// dot-prefixed because the config directory is not ours alone, pid-suffixed
/// because two concurrent installs must not share a temp file.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".to_string());
    let tmp = dir.join(format!(".{stem}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Best effort, and said out loud when it fails: a stray dot-file
            // left next to the user's config is debris this command is not
            // supposed to leave behind.
            if let Err(cleanup) = std::fs::remove_file(&tmp) {
                eprintln!(
                    "warning: could not remove temporary {}: {cleanup}",
                    tmp.display()
                );
            }
            Err(e)
        }
    }
}

/// Decided default archive cadence: hourly.
pub const DEFAULT_BACKUP_INTERVAL_SECS: u64 = 3600;

impl Config {
    /// Load the configuration, or say why it cannot be used.
    ///
    /// Three outcomes, three states:
    ///
    /// - **file present and usable** → `Ok`, with `source` = [`ConfigSource::File`]
    ///   (or [`ConfigSource::FileAfterWindowsPathRepair`] when the backslash
    ///   recovery below is what made it parse) and every path field expanded;
    /// - **file absent** → `Ok` with the built-in defaults and
    ///   [`ConfigSource::DefaultsMissing`]. This is the normal first-run state;
    /// - **file present and unusable** → `Err`, naming the file and the reason:
    ///   line and column for a TOML error, the field name for a path that cannot
    ///   be resolved.
    ///
    /// The third case deliberately does **not** produce a defaults-filled
    /// `Config`. Defaults are the dangerous answer here rather than the safe
    /// one: they empty `[destinations]`, so a `push` that runs on them is
    /// indistinguishable from one whose config never named a destination — and
    /// the archive silently stops being copied anywhere. A caller that must keep
    /// going anyway (`doctor`, which reports what it could not check) handles the
    /// `Err` explicitly instead of being handed a fallback it cannot see.
    ///
    /// One failure inside an otherwise-fine file is still recovered rather than
    /// reported: a `[cache]` section whose values could not be read leaves the
    /// rest of the config in force, turns the body cache off and records why (see
    /// [`Config::cache_error`]). That is a *successful* load — the file was read,
    /// and every key the user wrote is either in force or named in
    /// `cache_error` — so it is `Ok`; see [`Config::recover`].
    pub fn load() -> anyhow::Result<Self> {
        let path = config_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // First run — no config yet. That is explicitly fine.
                return Ok(Config::default());
            }
            Err(e) => {
                return Err(unusable_config(&path, format!("it could not be read: {e}")));
            }
        };
        Self::from_text(&raw)
    }

    /// The text of a config file, as a config, degrading as far as the file
    /// itself allows.
    ///
    /// The recoveries below are the only degradations left; anything a recovery
    /// cannot account for is an `Err` (see [`Config::recover`]).
    fn from_text(raw: &str) -> anyhow::Result<Self> {
        match toml::from_str::<Config>(raw) {
            Ok(mut cfg) => {
                cfg.source = ConfigSource::File;
                expand_config_paths(&mut cfg)?;
                Ok(cfg)
            }
            Err(strict_error) => Self::recover(raw, &strict_error),
        }
    }

    /// What a config file means when a strict parse of it failed.
    ///
    /// Two recoveries, in order, and then a refusal. The second one is why this
    /// is a function of its own: a bad value inside `[cache]` used to replace the
    /// *entire* config with its defaults, which silently dropped every other
    /// section and — because an absent `[cache]` means the documented default
    /// quota — turned the cache on at a size nobody wrote.
    ///
    /// Neither recovery ends in a defaults-filled config. Both end in a config
    /// that is genuinely the file's meaning; when neither applies the file is
    /// refused rather than replaced, so "this file says something I cannot read"
    /// can never be answered with "this file says nothing".
    fn recover(raw: &str, strict_error: &toml::de::Error) -> anyhow::Result<Self> {
        // 1. A Windows path pasted verbatim into a basic string is not valid
        //    TOML, and rejecting the whole file over it would act as if the user
        //    never stated where their store lives.
        let repaired = recover_windows_paths(raw);
        if let Some(fixed) = repaired.as_deref() {
            if let Ok(mut cfg) = toml::from_str::<Config>(fixed) {
                warn_windows_paths();
                cfg.source = ConfigSource::FileAfterWindowsPathRepair;
                expand_config_paths(&mut cfg)?;
                return Ok(cfg);
            }
        }
        // 2. A `[cache]` section that is present and unreadable. The rest of the
        //    file is provably fine — removing that one key is what makes it
        //    parse — so the rest of the file stays in force.
        if let Some(mut cfg) = salvage_unreadable_cache(repaired.as_deref().unwrap_or(raw)) {
            if repaired.is_some() {
                warn_windows_paths();
                cfg.source = ConfigSource::FileAfterWindowsPathRepair;
            } else {
                cfg.source = ConfigSource::File;
            }
            let why = format!("`[cache]` could not be read: {}", strict_error.message());
            eprintln!(
                "warning: {why}, so the body cache is off and the rest of {} was applied",
                config_path().display()
            );
            eprintln!(
                "         fix that value to turn the cache back on; `chat-stasher doctor` reports this too"
            );
            cfg.cache_error = Some(why);
            // The salvaged file is still checked for unresolvable paths: the
            // cache recovery accounts for one broken section, not for a `~`
            // that cannot be expanded somewhere else.
            expand_config_paths(&mut cfg)?;
            return Ok(cfg);
        }
        Err(unusable_config(
            &config_path(),
            format!("it is not valid TOML: {strict_error}"),
        ))
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

    /// Write `[native_host] stage = "<stage>"` into the config file, keeping
    /// everything else in that file byte-identical.
    ///
    /// The config is a *hand-written* file — the shipped template is nothing
    /// but comments — so a serde round-trip (`toml::to_string`) is not an
    /// option: it would re-emit the file and delete every comment the user
    /// wrote. When `[native_host]` already exists the new stage is spliced into
    /// the file's own bytes, so comments, other sections, a BOM and even the
    /// line endings (CRLF, LF or a mix) all survive untouched.
    ///
    /// Three outcomes, three states, and the caller prints which one happened:
    /// the key was absent and is now there, the key was there with a different
    /// value and has been replaced (the old value is returned so it can be
    /// shown), or the key already held exactly this value and **nothing was
    /// written at all** — not even a rewrite of identical bytes, so a file the
    /// user is editing is never touched by a no-op.
    ///
    /// A config file that exists but is not valid TOML is an error, never a
    /// silent overwrite: the parse failure is the user's text, and replacing it
    /// would delete whatever they were in the middle of writing.
    ///
    /// The two helpers below operate on a span-preserving [`toml_edit::Table`]
    /// borrowed from the `ImDocument`, so the byte offsets they splice point
    /// into the original `raw` text.
    pub fn set_native_host_stage(stage: &str) -> anyhow::Result<StageKeyWrite> {
        let path = config_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Same first-run path `init` takes, reused rather than
                // re-implemented, so the key lands in the documented template
                // instead of in a one-line file of our own invention.
                Config::init_default(DEFAULT_CONFIG_TEMPLATE)
                    .with_context(|| format!("write default config {}", path.display()))?;
                std::fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?
            }
            Err(e) => {
                return Err(e).with_context(|| format!("read {}", path.display()));
            }
        };
        // Parsed into a span-preserving `ImDocument`, not a mutable
        // `DocumentMut`. The immutable form keeps each key and value's byte
        // span into `raw`, which is what lets the update below splice only the
        // changed value instead of re-serialising the whole file — a
        // `DocumentMut` can edit the tree but discards the spans, and
        // `doc.to_string()` would then re-emit every line as LF and drop each
        // `\r`.
        let doc = toml_edit::ImDocument::parse(&raw)
            .with_context(|| format!("parse {} as TOML", path.display()))?;

        let previous = doc
            .get("native_host")
            .and_then(|section| section.get("stage"))
            .and_then(|item| item.as_str())
            .map(str::to_string);

        let outcome = match &previous {
            Some(old) if old == stage => return Ok(StageKeyWrite::Unchanged),
            Some(old) => StageKeyWrite::Updated {
                previous: old.clone(),
            },
            None => StageKeyWrite::Added,
        };

        // The value spelled the way TOML spells it; both the splice and the
        // append emit this same literal so the file speaks one form.
        let literal = toml_edit::Value::from(stage).to_string();

        // `[native_host]` present in the file: splice in only the stage's bytes.
        // Absent: append the section as text at EOF — never `DocumentMut::insert`,
        // which places a root table correctly only when the root already has a
        // key-value pair, and the shipped template has none (every line is a
        // comment), so an insert there would land the table *above* the file's
        // header comment: bytes preserved, but a file nobody would have written.
        // Appending is deterministic in both shapes and leaves every pre-existing
        // byte alone; the result is re-parsed below, so the format is still
        // checked.
        let updated: String = match doc.get("native_host") {
            Some(section) => match section.as_table() {
                Some(table) => set_existing_section_stage(&raw, table, &literal)?,
                // `native_host = 5`: saying so beats either silently replacing
                // the user's value or panicking on an index.
                None => anyhow::bail!(
                    "{}: `native_host` is not a table, so `stage` cannot be set in it",
                    path.display()
                ),
            },
            None => {
                let mut next = raw.clone();
                if !next.ends_with('\n') {
                    next.push('\n');
                }
                next.push_str(&format!(
                    "\n# ---------------------------------------------------------------- native_host\n\
                     # Stage the browser-spawned Native Messaging host writes to.\n\
                     # Written by `chat-stasher install-native-host --stage <path>`. The\n\
                     # host never creates this directory; it refuses to run if it is gone.\n\
                     [native_host]\nstage = {literal}\n"
                ));
                next
            }
        };

        // The post-condition, checked rather than assumed: the text about to be
        // written must parse, and must parse back to exactly the value asked
        // for. Writing a config the tool cannot read would be worse than
        // failing here, where the user's original file is still untouched.
        let reparsed: toml_edit::DocumentMut = updated.parse().with_context(|| {
            format!(
                "the updated config for {} would not be valid TOML; nothing was written",
                path.display()
            )
        })?;
        if reparsed
            .get("native_host")
            .and_then(|section| section.get("stage"))
            .and_then(|item| item.as_str())
            != Some(stage)
        {
            anyhow::bail!(
                "the updated config for {} would not read back the recorded stage; nothing was written",
                path.display()
            );
        }

        write_atomic(&path, updated.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        Ok(outcome)
    }

    /// Whether `[destinations.<name>]` is already declared in the file, as a
    /// real table rather than a commented-out example.
    ///
    /// Read from the file, not from `Config`: the caller has a `Config` in hand,
    /// but a `Config` whose `[destinations]` section could not be read is
    /// precisely the case where the answer must not be "no, add one".
    pub fn destination_is_declared(name: &str) -> anyhow::Result<bool> {
        let path = config_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            // No config file yet ⇒ nothing is declared. Not an error: that is
            // the normal first-run state, and it is the same reading
            // `Config::load` gives it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let doc = toml_edit::ImDocument::parse(&raw)
            .with_context(|| format!("parse {} as TOML", path.display()))?;
        Ok(doc
            .get("destinations")
            .and_then(|section| section.get(name))
            .is_some())
    }

    /// Add one `[destinations.<name>]` block, with its `options` table, to the
    /// config file.
    ///
    /// Appends at EOF, for the same reason [`Config::set_native_host_stage`]
    /// does: the shipped template has no live table at all (every line is a
    /// comment), and `DocumentMut::insert` places a root table above the file's
    /// header comment when the root has no key-value pair. Appending is
    /// deterministic in every shape of the file — a file that already has
    /// `[destinations.other]`, and one that has nothing — and leaves every
    /// pre-existing byte alone. The result is re-parsed below, so the format is
    /// still checked.
    ///
    /// Writes nothing and reports [`DestinationWrite::AlreadyDeclared`] when the
    /// name is already there. That is the `rustic init` precedent ADR-039 cites
    /// (check before you overwrite, so a re-run cannot clobber a hand-edited
    /// destination): a declared destination is the user's, and the caller
    /// verifies it rather than replacing it.
    ///
    /// `options` values are written **verbatim**, which is what makes this safe
    /// for credentials: an `env:NAME` value stays the three-character prefix and
    /// a variable name, and no secret ever passes through this function. The
    /// caller is responsible for that, and `setup`'s flags give it no other way
    /// to spell a credential.
    pub fn set_destination(
        name: &str,
        repo: &str,
        options: &BTreeMap<String, String>,
    ) -> anyhow::Result<DestinationWrite> {
        // A name that is not a bare TOML key is refused rather than quoted: a
        // name containing a dot would be appended as `[destinations.a.b]`, which
        // parses as a *nested* table and would make the whole `destinations`
        // map fail to deserialize — a config the tool then cannot load at all.
        // Refusing here keeps the user's file loadable.
        if !is_bare_toml_key(name) {
            // The rule stated here is the rule `is_bare_toml_key` enforces, and
            // nothing more: a leading digit is *allowed* (`[destinations.1box]` is
            // a key called `1box`), so a message that forbade it was describing a
            // check this code does not make.
            anyhow::bail!(
                "`{name}` cannot be a destination name: a destination is written as \
                 `[destinations.<name>]`, so the name must be letters, digits, `_` or `-`"
            );
        }
        if Self::destination_is_declared(name)? {
            return Ok(DestinationWrite::AlreadyDeclared);
        }

        let path = config_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Same first-run path `init` takes, reused rather than
                // re-implemented, so the block lands in the documented template
                // instead of in a one-file invention.
                Config::init_default(DEFAULT_CONFIG_TEMPLATE)
                    .with_context(|| format!("write default config {}", path.display()))?;
                std::fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };

        let mut next = raw.clone();
        if !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&format!(
            "\n# ---------------------------------------------------------------- \
             destinations.{name}\n\
             # Written by `chat-stasher setup`. A full copy, not a shard: `dest-init` gives it\n\
             # the union of your local sources and what your other destinations hold.\n\
             [destinations.{name}]\nrepo = {repo}\n",
            repo = toml_edit::Value::from(repo).to_string()
        ));
        if !options.is_empty() {
            next.push_str(&format!("\n[destinations.{name}.options]\n"));
            for (key, value) in options {
                next.push_str(&format!(
                    "{key} = {value}\n",
                    value = toml_edit::Value::from(value.as_str()).to_string()
                ));
            }
        }

        // The post-condition, checked rather than assumed: the text about to be
        // written must parse, and every value asked for must read back from it
        // exactly. Writing a config the tool cannot read would be worse than
        // failing here, where the user's original file is still untouched.
        let reparsed: toml_edit::DocumentMut = next.parse().with_context(|| {
            format!(
                "the updated config for {} would not be valid TOML; nothing was written",
                path.display()
            )
        })?;
        let written_repo = reparsed
            .get("destinations")
            .and_then(|section| section.get(name))
            .and_then(|entry| entry.get("repo"))
            .and_then(|item| item.as_str());
        if written_repo != Some(repo) {
            anyhow::bail!(
                "the updated config for {} would not read back `destinations.{name}.repo`; \
                 nothing was written",
                path.display()
            );
        }
        for (key, value) in options {
            let written = reparsed
                .get("destinations")
                .and_then(|section| section.get(name))
                .and_then(|entry| entry.get("options"))
                .and_then(|table| table.get(key))
                .and_then(|item| item.as_str());
            if written != Some(value.as_str()) {
                anyhow::bail!(
                    "the updated config for {} would not read back \
                     `destinations.{name}.options.{key}`; nothing was written",
                    path.display()
                );
            }
        }

        // The tool must be able to *use* what was written, not merely parse it.
        // `from_text` runs the whole load path — path expansion, and the
        // `env:NAME` resolution that turns a written reference into the option
        // the backend receives — so a block that parses but does not load is
        // caught here, with the user's original file untouched.
        let loaded = Config::from_text(&next).with_context(|| {
            format!(
                "the updated config for {} would not load; nothing was written",
                path.display()
            )
        })?;
        if !loaded.destinations.contains_key(name) {
            anyhow::bail!(
                "the updated config for {} parses but declares no destination `{name}`; nothing \
                 was written",
                path.display()
            );
        }

        write_atomic(&path, next.as_bytes())
            .with_context(|| format!("write {}", path.display()))?;
        Ok(DestinationWrite::Added)
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
        expand_opt_field("rustic_cache_dir", &mut self.rustic_cache_dir, problems);

        if let Some(native_host) = self.native_host.as_mut() {
            expand_opt_field("native_host.stage", &mut native_host.stage, problems);
        }

        if let Some(cache) = self.cache.as_mut() {
            // `[cache] dir` is a path like any other, so it goes through the
            // same `~` handling. `max_bytes` is a size, not a path, and is
            // deliberately not touched here.
            expand_opt_field("cache.dir", &mut cache.dir, problems);
        }

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
            let cache_dir_label = format!("destinations.{name}.cache_dir");
            expand_opt_field(&cache_dir_label, &mut dest.cache_dir, problems);
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

/// Run the post-parse path expansion over an effective config, returning an
/// error when any field could not be resolved.
///
/// A configured path that cannot be expanded is a config this tool cannot act
/// on, and *acting on the default instead* is the failure this whole module is
/// arranged to prevent (a `rustic_repo` of `~/stash/repo` silently becoming the
/// built-in repo path would push to a different repository than the one written
/// down). The fields are still cleared in place — a literal `~` is never handed
/// to the filesystem — but the caller does not keep the resulting config.
///
/// [`resolve_option_env_refs`] runs in the same pass, and its failures are
/// deliberately not part of this refusal: an `env:NAME` reference that cannot be
/// resolved **removes** the option instead of substituting a value, and the
/// reason is printed. Removing a key cannot silently point a destination at a
/// value the file did not write — the destination is left without the option its
/// author named, and the run that needs it fails loudly — which is not the
/// "act on a default nobody asked for" failure this function exists to refuse.
/// See `docs/install.md` §4.5 for the operator-facing contract.
fn expand_config_paths(cfg: &mut Config) -> anyhow::Result<()> {
    let mut problems: Vec<String> = Vec::new();
    cfg.expand_all_paths(&mut problems);
    if !problems.is_empty() {
        return Err(unusable_config(
            &config_path(),
            format!(
                "it sets a path this tool cannot resolve: {}",
                problems.join("; ")
            ),
        ));
    }
    let mut env_problems: Vec<String> = Vec::new();
    resolve_option_env_refs(cfg, &mut env_problems);
    for problem in &env_problems {
        eprintln!("warning: backend option environment reference was omitted: {problem}");
    }
    Ok(())
}

/// The error for a config file that exists but cannot be used.
///
/// One shape for all three causes (unreadable, not valid TOML, a path that
/// cannot be resolved) so the user sees the same two things every time: the file
/// that has to be fixed, and what is wrong with it. The hint names the one action
/// that turns this back into a working run — and deliberately spells out that a
/// *missing* config file is a different, normal state, because the failure being
/// reported is easy to misread as "you have no config".
fn unusable_config(path: &Path, reason: String) -> anyhow::Error {
    anyhow::anyhow!(
        "config file {} exists but cannot be used: {reason}\n\
         hint: fix that file, or move it aside — an absent config file is the normal first-run \
         state and runs with the built-in defaults; this one exists and is not being ignored",
        path.display()
    )
}

/// Replacement for an `env:NAME` option value, or the reason there is none.
///
/// The two states are kept apart here rather than inside the loop so that every
/// arm is reachable in a test on every platform: an environment variable whose
/// value is not valid Unicode cannot be planted portably, but the `Err` it
/// produces can be written down as a value.
enum EnvReference {
    /// The variable was set and non-empty; this is the secret.
    Resolved(String),
    /// The reference could not be used. Carries the message to print, which
    /// names the option and the variable and never the value.
    Unusable(String),
}

/// Turn one lookup outcome into a value or a named reason.
///
/// `NotPresent` and `NotUnicode` are different states with different fixes, so
/// they get different messages: collapsing them into one "unavailable" would
/// tell the operator to look for a variable that is already set.
fn classify_env_reference(
    label: &str,
    name: &str,
    lookup: Result<String, std::env::VarError>,
) -> EnvReference {
    match lookup {
        Ok(value) if !value.is_empty() => EnvReference::Resolved(value),
        Ok(_) => EnvReference::Unusable(format!(
            "{label}: environment variable {name} is set but empty"
        )),
        Err(std::env::VarError::NotUnicode(_)) => EnvReference::Unusable(format!(
            "{label}: environment variable {name} is set to a value that is not valid Unicode"
        )),
        Err(std::env::VarError::NotPresent) => EnvReference::Unusable(format!(
            "{label}: environment variable {name} is not set in this process"
        )),
    }
}

/// Whether `name` is a legal environment-variable name for an `env:NAME`
/// reference: a non-empty run of `A-Z`, `0-9` and `_`, not starting with a
/// digit.
///
/// One definition, used in two places that must agree — the config loader that
/// resolves such a reference ([`resolve_option_env_refs`]) and the wizard that
/// *writes* one from a name the user typed (`setup`'s `--remote-…-env` flags).
/// If they disagreed, the wizard could write a reference its own loader refuses,
/// and the destination would come back missing a credential with no explanation
/// of where it went.
///
/// The strictness carries a second duty at the writing end: a pasted credential
/// is not a legal variable name (AWS keys contain lowercase letters; a secret
/// access key contains `/` and `+`; a token contains `.` or `-`), so a secret
/// typed where a variable name was meant is refused by shape rather than
/// silently written into the config as a literal `env:` reference that would
/// never resolve.
pub fn is_env_reference_name(name: &str) -> bool {
    let mut chars = name.chars();
    // The first character cannot be a digit. `env:1FOO` names a variable no
    // shell can export, so accepting it accepts a reference that can only
    // resolve if something other than a shell set the variable.
    //
    // Written as two steps rather than one `enumerate`-indexed `match` because
    // that is what this used to be, and the first-character arm was
    // **unreachable**: the arm below it keyed on `_` for the index, so it
    // matched every first character the first arm did. The rule was stated and
    // not enforced, and a test written for the wizard (whose messages tell the
    // user this rule) is what found it.
    match chars.next() {
        Some(first) if first.is_ascii_uppercase() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

/// Whether `name` can be written as a **bare** TOML key in a table header —
/// the only form [`Config::set_destination`] writes.
///
/// Deliberately narrower than what TOML accepts. A quoted key (`"my.dest"`) is
/// legal TOML and would work in a header, but a *dotted* name is what this has
/// to exclude: `[destinations.a.b]` is a nested table, not a destination called
/// `a.b`, and `destinations` would stop deserializing as a map of
/// [`DestinationConfig`] — taking the whole config file down with it. Refusing
/// the shape is the difference between "that name is not allowed" and "your
/// config no longer loads".
///
/// No first-character rule, unlike [`is_env_reference_name`]: TOML's bare keys
/// are `A-Za-z0-9_-`, and a leading digit is legal there — `[destinations.1box]`
/// is a key called `1box`, and nothing about that is ambiguous in a header.
fn is_bare_toml_key(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// Replace backend option values spelled `env:NAME` with the matching process
/// environment value. Invalid, empty, non-Unicode, and unset references are all
/// omitted, each with its own message; the value itself must never enter a log,
/// and neither must a malformed reference, which may be a secret pasted by
/// mistake.
fn resolve_option_env_refs(cfg: &mut Config, problems: &mut Vec<String>) {
    for (destination, entry) in &mut cfg.destinations {
        let mut unresolved = Vec::new();
        for (key, value) in &mut entry.options {
            let Some(name) = value.strip_prefix("env:") else {
                continue;
            };
            let label = format!("destinations.{destination}.options.{key}");
            if !is_env_reference_name(name) {
                // Deliberately does not echo `name`: a typo here is often a
                // secret written where a variable name was meant.
                problems.push(format!("{label}: invalid environment variable reference"));
                unresolved.push(key.clone());
                continue;
            }
            match classify_env_reference(&label, name, std::env::var(name)) {
                EnvReference::Resolved(secret) => *value = secret,
                EnvReference::Unusable(reason) => {
                    problems.push(reason);
                    unresolved.push(key.clone());
                }
            }
        }
        for key in unresolved {
            entry.options.remove(&key);
        }
    }
}

/// The two lines a backslash-repaired config is reported with.
fn warn_windows_paths() {
    eprintln!(
        "warning: config contains unescaped backslash paths (Windows spelling), read as literal paths: {}",
        config_path().display()
    );
    eprintln!(
        "         `\\` is the escape character in TOML; write 'C:\\path' (single quotes) or \"C:\\\\path\" to silence this warning"
    );
}

/// The config a file means when its only unreadable part is `[cache]`.
///
/// `None` unless removing the `cache` key is what makes the text parse — which
/// is exactly the case where every other key is fine and `[cache]` is the sole
/// problem. A file broken anywhere else keeps the caller's existing behaviour:
/// this recovery is deliberately scoped to the one section the body cache owns,
/// so it can never quietly promote a *different* broken section into a config
/// that looks valid.
fn salvage_unreadable_cache(text: &str) -> Option<Config> {
    let mut table: toml::Table = text.parse().ok()?;
    table.remove("cache")?;
    Config::deserialize(toml::Value::Table(table)).ok()
}

/// Characters that may legally follow a backslash inside a TOML basic string,
/// excluding the two hex escapes (`\uXXXX` / `\UXXXXXXXX`) handled separately.
///
/// Seven, and five of them stand for a **control character**: `\b` (0x08), `\t`,
/// `\n`, `\f`, `\r`. That is why they are the ambiguous half of the table, and
/// why [`repair_basic_string`] rewrites them too once a string is known to be
/// carrying a Windows path: no path contains a control character.
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

/// Whether the basic string at `chars[start..end]` holds a backslash that is not
/// a TOML escape — which is the same statement as "this string carries a raw
/// Windows path", because a backslash that is not an escape is a separator.
///
/// Walks escape by escape rather than character by character: the second
/// backslash of a `\\` is not a separator, and reading it as one would put a
/// string that is spelled correctly into the case [`repair_basic_string`] calls
/// `strong`.
fn string_carries_a_raw_path(chars: &[char], start: usize, end: usize) -> bool {
    let mut i = start;
    while i < end {
        if chars[i] != '\\' {
            i += 1;
            continue;
        }
        if is_toml_escape(chars, i) {
            i += 2;
            continue;
        }
        return true;
    }
    false
}

/// Rewrite the basic string at `chars[start..end]` as the literal text the user
/// wrote, and say whether anything had to be doubled.
///
/// `strong` is set for a string that is already known to carry a raw Windows
/// path ([`string_carries_a_raw_path`]). It is what makes `\b`, `\t`, `\n`, `\f`
/// and `\r` separators too, and the reason is one sentence long: those five are
/// the escapes that stand for a control character, and no path contains a
/// control character. So in a string that is already a raw path,
/// `C:\Users\me\body-cache` is a path and `C:` + backspace + `ody-cache` is not
/// anything the user can have meant. Leaving them alone is exactly how a
/// repaired `[cache] dir` reached the body cache as a name Windows refuses
/// (`ERROR_INVALID_NAME`, "The filename, directory name, or volume label syntax
/// is incorrect", `os error 123`).
///
/// The three escapes that do **not** stand for one are still left alone even
/// here. `\\` is already a single literal backslash and doubling it would make
/// two; `\"` is the string's own delimiter and doubling it would end the string
/// early; `\uXXXX` / `\UXXXXXXXX` spell their payload out digit by digit, so a
/// separator is not what one looks like. (A control character written that way —
/// `\u0008` — therefore still survives a repair. It is reachable only by writing
/// those digits where a path separator belongs, which is not something a path
/// spells, and it is a value the strict parse already accepted as text.)
///
/// Every backslash that is not an escape, and every control escape in a `strong`
/// string, is doubled: the escape machinery of the *repair* is the same as the
/// one TOML reads, so `\\b` is a separator followed by the letter `b`.
fn repair_basic_string(chars: &[char], start: usize, end: usize, strong: bool) -> (String, bool) {
    let mut out = String::with_capacity(end - start + 8);
    let mut rewrote = false;
    let mut i = start;
    while i < end {
        let c = chars[i];
        if c != '\\' {
            out.push(c);
            i += 1;
            continue;
        }
        let next = chars.get(i + 1).copied();
        let doubles = !is_toml_escape(chars, i)
            || (strong && matches!(next, Some('b' | 't' | 'n' | 'f' | 'r')));
        if doubles {
            // Two backslashes, then the character after this one is read as
            // ordinary text — which is what turns `\b` into one separator.
            out.push('\\');
            out.push('\\');
            rewrote = true;
            i += 1;
            continue;
        }
        // An escape TOML defines, copied whole so its payload is never
        // re-examined as if it were text.
        out.push('\\');
        match next {
            Some(next) => {
                out.push(next);
                i += 2;
            }
            None => i += 1,
        }
    }
    (out, rewrote)
}

/// Emit one basic string — the text between its delimiters, `chars[start..end]` —
/// into `out`, repaired.
///
/// Whether the strong rule applies is decided here, once, at the end of the
/// string: it is a property of *this* string, so a file recovered for one path
/// keeps every other string's escapes exactly as they were.
fn emit_basic_string(
    out: &mut String,
    chars: &[char],
    start: usize,
    end: usize,
    rewrote: &mut bool,
) {
    if start >= end {
        return;
    }
    let strong = string_carries_a_raw_path(chars, start, end);
    let (fixed, changed) = repair_basic_string(chars, start, end, strong);
    *rewrote |= changed;
    out.push_str(&fixed);
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
/// TOML defines — with one boundary, which is the whole point of the second
/// half of this walk. A basic string that is *already* carrying a raw Windows
/// path (see [`string_carries_a_raw_path`]) has its control escapes read as
/// separators as well, because a control character cannot appear in a path and
/// the file did not parse as written anyway. Everything outside such a string —
/// and every escape in every string that needed no repair — is copied as it is.
///
/// The repair is decided per string and only at its closing delimiter, so each
/// basic string's text is held in `chars[start..]` until then rather than
/// emitted as it is read. Holding it is also what makes the rule checkable
/// rather than cumulative: one string's repair cannot reach into the next one.
fn recover_windows_paths(raw: &str) -> Option<String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len() + 32);
    let mut span = TomlSpan::Outside;
    // Where the basic string being walked begins: the index just past its
    // opening delimiter. Only meaningful while `span` is `Basic`/`MultiBasic`.
    let mut start = 0;
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
                        start = i;
                        span = TomlSpan::MultiBasic;
                        continue;
                    }
                    span = TomlSpan::Basic;
                    start = i + 1;
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
                // The closing delimiter, and with it the end of the string this
                // call has been holding: one repair decision, made once.
                let delimiters = if span == TomlSpan::MultiBasic {
                    (c == '"' && chars[i + 1..].starts_with(&['"', '"'])).then_some(3)
                } else {
                    (c == '"').then_some(1)
                };
                if let Some(delimiters) = delimiters {
                    emit_basic_string(&mut out, &chars, start, i, &mut rewrote);
                    for _ in 0..delimiters {
                        out.push('"');
                    }
                    i += delimiters;
                    span = TomlSpan::Outside;
                    continue;
                }
                if c == '\n' && span == TomlSpan::Basic {
                    // Unterminated basic string; the strict error stands.
                    emit_basic_string(&mut out, &chars, start, i, &mut rewrote);
                    out.push(c);
                    span = TomlSpan::Outside;
                    i += 1;
                    continue;
                }
                // A backslash takes the character after it with it, whatever
                // that character is: that is what makes `\"` not close the
                // string, and finding the end of the string is the only thing
                // this walk needs to know TOML's escape syntax for. Whether the
                // bytes mean an escape or a separator is decided at the end.
                i += if c == '\\' && chars.get(i + 1).is_some() {
                    2
                } else {
                    1
                };
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    // A string still open at the end of the file is emitted the same way, so the
    // text handed back is never missing a span. It cannot parse either way, and
    // the caller reports the original error when it does not.
    if let TomlSpan::Basic | TomlSpan::MultiBasic = span {
        emit_basic_string(&mut out, &chars, start, chars.len(), &mut rewrote);
    }

    rewrote.then_some(out)
}

/// Absolute path of the config file.
///
/// Uses `$XDG_CONFIG_HOME` when set (Linux/Unix convention), otherwise the
/// user's home directory. `std::env::home_dir` is deprecated in favour of
/// reading `HOME` directly; both are shelled out with [`std::env::home_dir`]
/// avoided because it is no longer guaranteed on macOS.
///
/// `CONFIG_RELATIVE_PATH` is split into its components before being joined,
/// rather than handed to `Path::join` as one literal. `join` does not translate
/// the separator it is given, so on Windows the single literal came back out as
/// `…\chat-stasher/config.toml` — a spelling no Windows tool prints, in the one
/// message that tells the user which file to go and fix. Joining component by
/// component yields the path in the platform's own spelling, and it is the same
/// file either way: `/` is a separator to the Windows file APIs too, which is
/// exactly why the wrong spelling survived — it opened the right file while
/// naming it wrong.
pub fn config_path() -> PathBuf {
    let config_home = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(xdg) => PathBuf::from(xdg),
        None => home_dir().join(".config"),
    };
    Path::new(CONFIG_RELATIVE_PATH)
        .components()
        .fold(config_home, |path, part| path.join(part.as_os_str()))
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

/// `$XDG_DATA_HOME/chat-stasher`, or `~/.local/share/chat-stasher`.
///
/// One definition for what had been three copies (the CLI's, `doctor`'s and the
/// host's). They must agree exactly: this directory holds the machine identity
/// file whose 128-bit value *is* this machine's archive partition, so a second
/// spelling of the path would silently create a second identity and split the
/// archive in two.
pub fn default_data_root() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("chat-stasher");
    }
    home_dir().join(".local").join("share").join("chat-stasher")
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
///   this whole module exists to prevent. Every caller treats that error as
///   fatal, including config load: a configured path that cannot be resolved
///   makes the config unusable rather than quietly falling back to a different
///   path than the one written down.
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
/// On failure, clear the field (`None`) and record the reason — a literal `~`
/// must never survive into a path the tool then hands to the filesystem. The
/// recorded reason is what makes [`Config::load`] fail, so the cleared field is
/// never part of a config a caller gets to act on.
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
# home) is not supported and is rejected. If a path here cannot be resolved (no
# home available, or `~username`), the commands that read this file refuse to run
# and say which option is at fault; they never substitute a default for it — a
# `~` directory is never created, and a path is never silently replaced.
#
# A file that exists must be readable and valid: a typo stops every command that
# reads it, with the file, the line and the reason. Delete or move this file
# aside instead if you want the built-in defaults — an absent config file is the
# normal first-run state.

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

# Where rustic keeps this machine's local metadata cache.
#
# rustic caches snapshot / index / *tree* packs — metadata only, never the
# data packs with your conversations — under a per-machine directory
# (`dirs::cache_dir()/rustic`; ~/Library/Caches/rustic on macOS,
# ~/.cache/rustic on Linux, %LOCALAPPDATA%\rustic on Windows). The cache is
# purely a speed-up for re-opening the repository; it holds no archive content.
#
# Default: follow rustic's own per-platform location. Set this to move the
# cache somewhere you can see (and, if you ever need to, delete) it.
# rustic_cache_dir = "~/Library/Caches/rustic"
#
# Completely disable the local metadata cache (rustic `no_cache`). Default
# false. Because the cache holds metadata only — never the data packs —
# disabling it loses no archive content; every open just re-reads the
# metadata instead of keeping it between runs. `doctor` reports how much the
# cache currently occupies, so you can see what this switch is saving.
# rustic_no_cache = false

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

# ----------------------------------------------------------------- cache
# This machine's *body* cache (ADR-034). Separate from `rustic_cache_dir`
# above, which is rustic's own metadata cache: this one holds the conversation
# bodies themselves, the bytes that otherwise come down the wire again on every
# read (a 107 MB session measured 22.6 s warm on a remote destination).
#
# What it stores is the destination's own ciphertext, byte for byte, under a
# content address. Nothing is decrypted to store it, no second key is
# introduced, no plaintext is written, and nothing here is ever read back
# without being re-hashed first — a damaged entry is discarded and re-fetched,
# so the cache can only ever cost speed. Deleting the directory is always safe:
# `chat-stasher cache clear` does exactly that, and `doctor` reports how much it
# currently occupies.
#
# One quota per machine, shared by every destination: there is no per-destination
# allowance to divide up. Least-recently-used entries are evicted when the quota
# is reached; a session larger than a tenth of the quota is read through without
# being stored, so one oversized conversation cannot sweep the cache.
#
# Bulk work — `verify`, `export`, `dest-init`, `push`, `read --all-machines` —
# never enters the cache, in either direction. `verify` in particular reads to
# prove the *destination* is intact, and a cache answering for it would move the
# verdict onto the wrong disk.
#
# [cache]
# How much disk the cache may occupy. Write plain bytes or a unit: "50GB" is
# 50 x 10^9, "50GiB" is 50 x 2^30. 0 turns the cache off entirely (reads then
# go to the destination every time). Default: 2 GiB.
# max_bytes = "10GiB"
#
# Where the entries live. Default: this platform's cache directory
# (~/Library/Caches/chat-stasher/body on macOS), chosen so that no sync or
# backup tool treats it as data worth carrying.
# dir = "~/Library/Caches/chat-stasher/body"

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
# `--destination <name>` (or an explicit `--repo`). Retrieval commands (`search`,
# `export`, `overview`) have no default destination: naming the copy is their
# rule. The `ui` dashboard is the one place that reads a default — it opens the
# only destination declared, or the one `[native_host] destination` names when
# several are.
#
# [destinations.laptop]
# repo = "~/stash/chat-stasher/repo"
# key_file = "~/stash/chat-stasher/masterkey.json"
#
# [destinations.storagebox]
# repo = "opendal:sftp"
# key_file = "~/stash/chat-stasher/masterkey-storagebox.json"
# connections = 4
# Per-destination cache knobs. Same semantics as `rustic_cache_dir` /
# `rustic_no_cache` above, scoped to this destination's repository.
# cache_dir = "~/Library/Caches/rustic-storagebox"
# no_cache = false
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

    #[test]
    fn load_resolves_backend_option_environment_references() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        const SECRET: &str = "CHAT_STASHER_TEST_OPTION_SECRET";
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let old_userprofile = std::env::var_os("USERPROFILE");
        let old_secret = std::env::var_os(SECRET);
        let home = tempfile::TempDir::new().unwrap();
        let xdg = tempfile::TempDir::new().unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", xdg.path());
        std::env::remove_var("USERPROFILE");
        std::env::set_var(SECRET, "test-only-secret-value");
        let config_dir = xdg.path().join("chat-stasher");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            "[destinations.d1.options]\nsecret = \"env:CHAT_STASHER_TEST_OPTION_SECRET\"\n",
        )
        .unwrap();

        // `Config::load` refuses a file it cannot act on, and this one is
        // loadable: the reference resolves, and no path in it needs expanding.
        let cfg =
            Config::load().expect("a config whose only option is a set env reference must load");
        let resolved = cfg.destinations["d1"].options.get("secret");
        assert!(resolved.is_some_and(|value| value == "test-only-secret-value"));

        for (name, value) in [
            ("HOME", old_home),
            ("XDG_CONFIG_HOME", old_xdg),
            ("USERPROFILE", old_userprofile),
            (SECRET, old_secret),
        ] {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[test]
    fn backend_option_environment_references_resolve_and_missing_values_are_omitted() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        const PRESENT: &str = "CHAT_STASHER_TEST_OPTION_SECRET";
        const MISSING: &str = "CHAT_STASHER_TEST_OPTION_MISSING";
        let old_present = std::env::var_os(PRESENT);
        let old_missing = std::env::var_os(MISSING);
        std::env::set_var(PRESENT, "test-only-secret-value");
        std::env::remove_var(MISSING);

        let mut cfg: Config = toml::from_str(
            "[destinations.d1.options]\nsecret = \"env:CHAT_STASHER_TEST_OPTION_SECRET\"\nmissing = \"env:CHAT_STASHER_TEST_OPTION_MISSING\"\n",
        )
        .unwrap();
        let mut problems = Vec::new();
        resolve_option_env_refs(&mut cfg, &mut problems);

        let options = &cfg.destinations["d1"].options;
        assert!(options
            .get("secret")
            .is_some_and(|value| value == "test-only-secret-value"));
        assert!(!options.contains_key("missing"));
        assert_eq!(problems.len(), 1);
        assert!(!problems[0].contains("test-only-secret-value"));

        match old_present {
            Some(value) => std::env::set_var(PRESENT, value),
            None => std::env::remove_var(PRESENT),
        }
        match old_missing {
            Some(value) => std::env::set_var(MISSING, value),
            None => std::env::remove_var(MISSING),
        }
    }

    /// Every arm of the classifier is reachable without planting an environment
    /// variable, including the one this platform cannot portably plant (a value
    /// that is not valid Unicode). "Set but empty", "set to non-Unicode bytes",
    /// and "not set" stay three states with three repairs, and none of the
    /// messages may quote the value.
    #[test]
    fn env_reference_failures_keep_their_distinct_causes() {
        const MARKER: &str = "marker-that-must-not-be-quoted";
        let label = "destinations.d1.options.secret";
        let name = "CHAT_STASHER_TEST_OPTION_SECRET";
        let ask = |lookup: Result<String, std::env::VarError>| {
            classify_env_reference(label, name, lookup)
        };

        assert!(matches!(
            ask(Ok("resolved".to_string())),
            EnvReference::Resolved(ref value) if value == "resolved"
        ));

        let cases = [
            (Ok(String::new()), "is set but empty"),
            (
                Err(std::env::VarError::NotUnicode(std::ffi::OsString::from(
                    MARKER,
                ))),
                "not valid Unicode",
            ),
            (
                Err(std::env::VarError::NotPresent),
                "is not set in this process",
            ),
        ];
        let mut reasons = Vec::new();
        for (lookup, expected) in cases {
            match ask(lookup) {
                EnvReference::Resolved(_) => panic!("a failure was read as a value"),
                EnvReference::Unusable(reason) => reasons.push((reason, expected)),
            }
        }
        for (reason, expected) in &reasons {
            assert!(reason.contains(expected), "unexpected reason: {reason}");
            assert!(reason.contains(label), "reason lost the option label");
            assert!(reason.contains(name), "reason lost the variable name");
            assert!(!reason.contains(MARKER), "reason quoted the value");
        }
        // Three failures, three messages: a distinction that collapses into one
        // string is not a distinction the operator can act on.
        let distinct: std::collections::BTreeSet<&String> =
            reasons.iter().map(|(reason, _)| reason).collect();
        assert_eq!(distinct.len(), 3);
    }

    /// What counts as a variable name, on both sides of every boundary.
    ///
    /// This predicate is shared by the loader that resolves `env:NAME` and by
    /// the wizard that writes one, so a wrong answer here is either a config
    /// that loads a reference the tool cannot use, or a reference the tool
    /// writes and its own loader drops.
    #[test]
    fn an_env_reference_name_is_a_shell_variable_name() {
        for good in ["A", "_", "_X9", "CHAT_STASHER_R2_ACCESS_KEY_ID", "A1_B2"] {
            assert!(is_env_reference_name(good), "`{good}` is a variable name");
        }
        // A leading digit is the case a `match` on `(index, ch)` could not
        // express, because the arm for the remaining characters matched a first
        // character too: the rule was stated in prose and never enforced.
        // `env:1FOO` names a variable no shell can export, so it is refused now.
        for bad in [
            "",
            "1FOO",
            "9",
            "lowercase",
            "Mixed_Case",
            "HAS-DASH",
            "HAS.DOT",
            "HAS SLASH/",
            "UNICODE_É",
        ] {
            assert!(
                !is_env_reference_name(bad),
                "`{bad}` is not a variable name"
            );
        }
        // An access key id is the shape this cannot catch, written down here so
        // the wizard's second guard (the `AKIA`/`ASIA` prefix rule) has a
        // reason to exist that a reader can check.
        assert!(is_env_reference_name("AKIAIOSFODNN7EXAMPLE"));
    }

    /// What counts as a destination name. Narrower than TOML on purpose: a
    /// dotted name would be appended as a *nested* table and would stop the
    /// whole `destinations` map deserializing.
    #[test]
    fn a_destination_name_is_a_bare_toml_key() {
        for good in ["laptop", "storagebox", "r2", "my-dest", "_x", "1box", "A1"] {
            assert!(is_bare_toml_key(good), "`{good}` is a bare TOML key");
        }
        for bad in ["", "my.dest", "my dest", "my/dest", "naïve", "a\"b"] {
            assert!(!is_bare_toml_key(bad), "`{bad}` is not a bare TOML key");
        }
    }

    /// The block the wizard writes, read back by the tool that has to use it.
    ///
    /// The property under test is not "the text looks right" but "the config
    /// that was written is the config the loader produces" — including the
    /// `env:NAME` reference staying a reference at the file level while the
    /// loader resolves it in memory.
    #[test]
    fn a_destination_block_round_trips_with_its_credential_references() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::TempDir::new().unwrap();
        let config_home = home.path().join("config");
        let old_home = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", &config_home);

        let mut options = BTreeMap::new();
        options.insert("bucket".to_string(), "a-bucket".to_string());
        options.insert(
            "endpoint".to_string(),
            "https://example.invalid".to_string(),
        );
        options.insert("access_key_id".to_string(), "env:VAR_A".to_string());
        options.insert("secret_access_key".to_string(), "env:VAR_B".to_string());

        let _ = Config::set_destination("box", "opendal:s3", &options).unwrap();
        // The file on disk carries the *reference*, not a resolved value: this
        // is what "no secret in the config" means at the byte level.
        let text = std::fs::read_to_string(config_path()).unwrap();
        assert!(text.contains("env:VAR_A"), "{text}");
        assert!(text.contains("env:VAR_B"), "{text}");
        assert!(text.contains("[destinations.box]"), "{text}");
        assert!(text.contains("repo = \"opendal:s3\""), "{text}");

        // And the tool reads its own block back with every value intact.
        let loaded = Config::load().unwrap();
        let entry = loaded.destinations.get("box").expect("the block loads");
        assert_eq!(entry.repo.as_deref(), Some("opendal:s3"));
        assert_eq!(
            entry.options.get("bucket").map(String::as_str),
            Some("a-bucket")
        );
        // Neither variable is set, so the reference is *omitted* rather than
        // substituted — a credential error rather than a silently empty value.
        assert!(!entry.options.contains_key("access_key_id"));
        assert!(!entry.options.contains_key("secret_access_key"));

        // A second call is a no-op that says so: a destination already declared
        // is the user's, and a wizard re-run must not rewrite it.
        assert_eq!(
            Config::set_destination("box", "opendal:sftp", &BTreeMap::new()).unwrap(),
            DestinationWrite::AlreadyDeclared
        );
        assert_eq!(
            std::fs::read_to_string(config_path()).unwrap(),
            text,
            "an already-declared destination must leave the file byte-for-byte as it was"
        );

        // A name that would be appended as a nested table is refused, so the
        // file cannot be broken by a name typed at a prompt.
        let dotted = Config::set_destination("a.b", "opendal:s3", &BTreeMap::new());
        assert!(dotted.is_err());
        assert_eq!(std::fs::read_to_string(config_path()).unwrap(), text);

        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match old_xdg {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

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

    /// A repaired path keeps **every** separator a separator, including the ones
    /// that collide with a TOML control escape.
    ///
    /// This is the shape that shipped broken, verbatim from `w120_body_cache_test`
    /// on `windows-latest`: the cache root is `<tmp>\.tmpXXXXXX\body-cache`, every
    /// separator but the last is a backslash TOML does not define (so the file is
    /// repaired), and `\b` is one it does. Left alone, the last separator became a
    /// backspace, the repaired text parsed, and the body cache was handed a path
    /// Windows refuses (`os error 123`) and reported itself unavailable with an
    /// error nobody can act on. A path separator is one backslash; a control
    /// character is never a path.
    #[test]
    fn a_repaired_path_keeps_every_separator() {
        let raw = "[cache]\ndir = \"C:\\Users\\me\\AppData\\Local\\Temp\\.tmpC2DdfN\\body-cache\"\nmax_bytes = \"10MB\"\n";
        assert!(
            toml::from_str::<Config>(raw).is_err(),
            "precondition: the raw file must fail strict parsing, or this repair would not run"
        );
        let fixed = recover_windows_paths(raw).expect("unescaped backslashes must be repaired");
        let cfg: Config = toml::from_str(&fixed).expect("the repaired file must parse");
        let dir = cfg
            .cache
            .as_ref()
            .and_then(|cache| cache.dir.as_deref())
            .expect("`cache.dir` survives the repair");
        assert_eq!(
            dir, "C:\\Users\\me\\AppData\\Local\\Temp\\.tmpC2DdfN\\body-cache",
            "every separator of a repaired Windows path must stay a separator"
        );
        assert!(
            !dir.chars().any(char::is_control),
            "the repaired path carries a control character, which no path can"
        );
    }

    /// The strong rule is a property of one string, not of the file: repairing a
    /// Windows path must not re-read the escapes in a string that needed no
    /// repair. Same file, one raw path and one ordinary string.
    #[test]
    fn a_repair_does_not_reach_another_string() {
        let raw = "[cache]\ndir = \"C:\\Users\\me\\body-cache\"\n[harness_roots]\ngrok = \"first\\nsecond\"\n";
        let fixed = recover_windows_paths(raw).expect("the raw path needs repair");
        let cfg: Config = toml::from_str(&fixed).expect("the repaired file must parse");
        assert_eq!(
            cfg.cache.as_ref().and_then(|cache| cache.dir.as_deref()),
            Some("C:\\Users\\me\\body-cache")
        );
        assert_eq!(
            cfg.explicit_harness_root("grok"),
            Some("first\nsecond"),
            "a string that needed no repair keeps the escape it was written with"
        );
    }

    /// The other half of the same rule: a separator that is already spelled
    /// correctly (`\\`) is left alone, because it is one literal backslash and
    /// doubling it again would make two. The strong rule is about the
    /// *ambiguous* escapes, not about every backslash it meets — and a correct
    /// separator next to a raw one does not rescue the raw one.
    #[test]
    fn an_already_escaped_separator_is_not_doubled_again() {
        let raw = "[harness_roots]\ncursor = \"C:\\Users\\me\\\\other\\body\"\n";
        let fixed = recover_windows_paths(raw).expect("the raw separators need repair");
        let cfg: Config = toml::from_str(&fixed).expect("the repaired file must parse");
        assert_eq!(
            cfg.explicit_harness_root("cursor"),
            Some("C:\\Users\\me\\other\\body"),
            "one correctly escaped separator stays one separator"
        );
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
    // Unified `~` expansion (`expand_tilde` / `assert_no_literal_tilde`)
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
        // Windows `~\` spelling is also accepted (expanded to home directory on every platform).
        assert_eq!(
            expand_tilde_with_home("~\\x", Some(&home_s)),
            Ok(home.path().join("x"))
        );
        // Paths without `~` are returned unchanged.
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
        // Synthetic "escaped expansion" paths: mid-path `~` and `~username` components must be rejected.
        assert!(assert_no_literal_tilde(Path::new("stash/~/x")).is_err());
        assert!(assert_no_literal_tilde(Path::new("/home/me/~alice")).is_err());
        // Normal absolute paths after expansion must be allowed.
        let home = tempfile::TempDir::new().unwrap();
        assert!(assert_no_literal_tilde(&home.path().join("x")).is_ok());
        assert!(assert_no_literal_tilde(Path::new("/home/me/real")).is_ok());
        // `~` inside a filename not at component start is a valid filename, do not false-positive.
        assert!(assert_no_literal_tilde(Path::new("/home/me/foo~bar")).is_ok());
    }

    #[test]
    fn expand_and_verify_catches_mid_path_tilde() {
        assert!(expand_and_verify("stash/~/x").is_err());
        assert!(expand_and_verify("~/ok").is_ok());
    }

    // -----------------------------------------------------------------------
    // Per-field expansion on load + fallback to defaults when `$HOME` is missing
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

        let cfg = Config::load().expect("this fixture is a valid config");
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

    /// ADR-020 Phase 5: the cache knobs parse, and the path one goes through
    /// the same `~` expansion as every other path field.
    #[test]
    fn cache_fields_parse_and_expand() {
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
rustic_cache_dir = "~/caches/rustic"
rustic_no_cache = true

[destinations.d1]
repo = "~/dest/repo"
cache_dir = "~/caches/rustic-d1"
no_cache = false
"#,
        )
        .unwrap();

        let cfg = Config::load().expect("this fixture is a valid config");
        let h = home.path();
        assert_eq!(
            cfg.rustic_cache_dir.as_deref(),
            Some(h.join("caches/rustic").to_str().unwrap())
        );
        assert_eq!(cfg.rustic_no_cache, Some(true));
        let d1 = cfg.destinations.get("d1").expect("d1 present");
        assert_eq!(
            d1.cache_dir.as_deref(),
            Some(h.join("caches/rustic-d1").to_str().unwrap())
        );
        assert_eq!(d1.no_cache, Some(false));
    }

    /// The knobs are optional: an absent config leaves them at their defaults
    /// (`None`/`None`), which store.rs maps to "cache on, standard dir".
    #[test]
    fn cache_fields_default_when_absent() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.rustic_cache_dir, None);
        assert_eq!(cfg.rustic_no_cache, None);
        let cfg: Config = toml::from_str("[destinations.d1]\nrepo = \"x\"\n").unwrap();
        assert_eq!(cfg.destinations["d1"].cache_dir, None);
        assert_eq!(cfg.destinations["d1"].no_cache, None);
    }

    /// The `[cache]` section is a *new* key family (the body cache of
    /// ADR-034), and it must not be confused with the `rustic_*` metadata-cache
    /// knobs above: both may be present in one config, and each keeps its own
    /// value and location.
    #[test]
    fn body_cache_section_parses_alongside_the_metadata_cache_knobs() {
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
rustic_cache_dir = "~/caches/rustic"

[cache]
dir = "~/caches/bodies"
max_bytes = "50GB"
"#,
        )
        .unwrap();

        let cfg = Config::load().expect("this fixture is a usable config");
        let h = home.path();
        assert_eq!(
            cfg.rustic_cache_dir.as_deref(),
            Some(h.join("caches/rustic").to_str().unwrap()),
            "the metadata cache root must be untouched by the new section"
        );
        let cache = cfg.cache.as_ref().expect("[cache] present");
        assert_eq!(
            cache.dir.as_deref(),
            Some(h.join("caches/bodies").to_str().unwrap()),
            "cache.dir goes through the same `~` expansion as every other path"
        );
        let settings =
            crate::body_cache::settings_for(&cfg).expect("body cache settings must resolve");
        assert_eq!(settings.max_bytes, 50_000_000_000);
        assert_eq!(settings.root, h.join("caches/bodies"));

        // A plain integer is the other accepted spelling, and both must mean
        // exactly what they say.
        let cfg: Config = toml::from_str("[cache]\nmax_bytes = 5368709120\n").unwrap();
        assert_eq!(
            crate::body_cache::settings_for(&cfg)
                .expect("resolve")
                .max_bytes,
            5_368_709_120
        );
        // `"0"` is the documented way to switch the cache off, and it must
        // arrive as a real zero rather than as "unset".
        let cfg: Config = toml::from_str("[cache]\nmax_bytes = \"0\"\n").unwrap();
        let settings = crate::body_cache::settings_for(&cfg).expect("resolve");
        assert_eq!(settings.max_bytes, 0);
        assert!(!settings.enabled());
    }

    /// An absent `[cache]` section, or a section without `max_bytes`, means the
    /// documented default quota — never an error and never "off".
    #[test]
    fn body_cache_defaults_when_absent() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.cache.is_none());
        let settings = crate::body_cache::settings_for(&cfg).expect("resolve");
        assert_eq!(settings.max_bytes, crate::body_cache::DEFAULT_MAX_BYTES);
        assert!(settings.enabled());

        let cfg: Config = toml::from_str("[cache]\n").unwrap();
        let settings = crate::body_cache::settings_for(&cfg).expect("resolve");
        assert_eq!(settings.max_bytes, crate::body_cache::DEFAULT_MAX_BYTES);
        // The default root is this platform's cache directory, which is never
        // inside the data or config directory an archive lives in.
        assert!(
            !settings
                .root
                .starts_with(crate::config::default_data_root()),
            "the body cache must not default into the archive's own directory"
        );
    }

    /// A size the parser does not understand is an error. A config file that
    /// silently fell back to the default would report a quota the user never
    /// asked for as if they had asked for it.
    #[test]
    fn an_unparsable_body_cache_size_is_a_config_error() {
        let err = toml::from_str::<Config>("[cache]\nmax_bytes = \"50G\"\n")
            .expect_err("an unknown unit must not be read as a number");
        let text = err.to_string();
        assert!(
            text.contains("50G"),
            "the error must quote the value the user wrote: {text}"
        );
    }

    /// A `[cache]` value the parser rejects must not take the rest of the file
    /// down with it, and must not turn the cache on at the default quota. The
    /// cache goes off, the reason travels with it, and every other section is
    /// still in force.
    #[test]
    fn an_unreadable_cache_section_leaves_the_rest_of_the_config_in_force() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let xdg = tempfile::TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", xdg.path());

        let cfg_dir = xdg.path().join("chat-stasher");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            r#"
machine = "m-alpha"

[destinations.d1]
repo = "/nonexistent/repo"

[cache]
max_bytes = "50G"
"#,
        )
        .unwrap();

        let cfg = Config::load().expect("the cache recovery is a successful load, not a refusal");
        // The typo does not cost the user the rest of the file: this is the
        // half of the change that has nothing to do with the cache itself.
        assert_eq!(cfg.machine.as_deref(), Some("m-alpha"));
        assert!(
            cfg.destinations.contains_key("d1"),
            "an unreadable `[cache]` must not drop `[destinations]` either: {:?}",
            cfg.destinations.keys().collect::<Vec<&String>>()
        );
        // And it does not silently activate a cache nobody asked for.
        let problem = cfg
            .cache_error
            .as_deref()
            .expect("the reason must be recorded on the config");
        assert!(
            problem.contains("50G"),
            "the reason must quote the value the user wrote: {problem}"
        );
        assert!(
            crate::body_cache::settings_for(&cfg).is_err(),
            "a cache whose quota could not be read has no quota to report"
        );
        match crate::body_cache::for_operation(&cfg, crate::body_cache::Policy::ReadThrough) {
            crate::body_cache::Availability::Invalid(why) => {
                assert!(why.contains("50G"), "the state carries the reason: {why}");
            }
            other => {
                panic!("an unreadable `[cache]` must not read as off, let alone as on: {other:?}")
            }
        }
    }

    /// The recovery is scoped to `[cache]`: a file broken anywhere else is
    /// refused outright, so this can never quietly promote a *different* broken
    /// section into a config that looks valid.
    ///
    /// Its pair is the test directly above: there `[cache]` is the *only* broken
    /// thing and the load succeeds with `cache_error` set. Here the cache is not
    /// the problem, and the same recovery must not fire.
    #[test]
    fn the_cache_recovery_does_not_mask_a_break_somewhere_else() {
        // `connections` is not a number, and `[cache]` is absent. Removing a
        // `cache` key cannot be what makes this parse — there is no such key —
        // so the refusal has to be about the real break.
        let other_break =
            "machine = \"m-alpha\"\n[destinations.d1]\nrepo = \"/tmp/x\"\nconnections = \"lots\"\n";
        let err = Config::from_text(other_break)
            .expect_err("a break outside `[cache]` must refuse the file, not default it");
        let text = format!("{err:#}");
        assert!(
            text.contains("not valid TOML"),
            "the refusal must name the real problem: {text}"
        );
        assert!(
            text.contains("connections"),
            "the refusal must point at the offending key: {text}"
        );

        // A readable `[cache]` next to a break elsewhere: removing the cache
        // key does not make the file parse, so this is not the cache recovery
        // either — and the refusal must not be phrased as a cache problem.
        let mixed =
            "[cache]\nmax_bytes = 0\n[destinations.d1]\nrepo = \"/tmp/x\"\nconnections = \"lots\"\n";
        let err = Config::from_text(mixed)
            .expect_err("a readable `[cache]` must not buy a break elsewhere an exemption");
        let text = format!("{err:#}");
        assert!(
            !text.contains("could not be read"),
            "the readable `[cache]` section is not what failed here: {text}"
        );
        assert!(
            text.contains("connections"),
            "the refusal must point at the offending key: {text}"
        );
    }

    /// The config path is spelled with this platform's separator all the way
    /// through — asserted on the printed string, not on `Path` equality.
    ///
    /// `PathBuf` compares component-wise, so on Windows
    /// `…\chat-stasher/config.toml` and `…\chat-stasher\config.toml` are *equal
    /// paths*: the file opens either way, which is why the wrong spelling
    /// survived unnoticed. It is not invisible, though. This is the string the
    /// refusal message sends the user to go and fix, and a Windows shell does
    /// not accept it. The comparison is therefore on `display()` — the form the
    /// user is shown — against a path built by joining the same components the
    /// tool joins.
    #[test]
    fn config_path_is_spelled_with_this_platforms_separator() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let xdg = tempfile::TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", xdg.path());

        let expected = xdg.path().join("chat-stasher").join("config.toml");
        assert_eq!(
            config_path().display().to_string(),
            expected.display().to_string(),
            "the path the tool prints must be the one this platform spells"
        );
    }

    /// A configured path that cannot be expanded makes the whole config
    /// unusable. This used to reset the field to its (possibly different)
    /// default and carry on — which meant `rustic_repo = "~/repo"` with no
    /// `$HOME` silently became the *built-in* repo path, i.e. a push to a
    /// different repository than the one written down.
    #[test]
    fn load_refuses_when_a_path_cannot_be_expanded() {
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

        let err = Config::load().expect_err("an unresolvable path must not fall back to defaults");
        let text = format!("{err:#}");
        // The user has to be told which file and which option; the raw
        // `MissingHome` text alone names neither.
        assert!(
            text.contains(&cfg_dir.join("config.toml").display().to_string()),
            "the error must name the config file: {text}"
        );
        assert!(
            text.contains("rustic_repo") && text.contains("destinations.d1.key_file"),
            "the error must name every option that could not be resolved: {text}"
        );
    }

    /// The other half of the asymmetry: a *missing* file is not an error and
    /// still yields the built-in defaults.
    #[test]
    fn load_without_a_config_file_is_defaults_not_an_error() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let xdg = tempfile::TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", xdg.path());

        let cfg = Config::load().expect("an absent config file is the normal first-run state");
        assert_eq!(cfg.source, ConfigSource::DefaultsMissing);
        assert!(cfg.destinations.is_empty());
    }

    /// A file that exists and is not TOML is an error naming the file and the
    /// position — never a defaults-filled `Config`.
    #[test]
    fn load_refuses_invalid_toml_instead_of_using_defaults() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let xdg = tempfile::TempDir::new().unwrap();
        env::set_var("XDG_CONFIG_HOME", xdg.path());

        let cfg_dir = xdg.path().join("chat-stasher");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let path = cfg_dir.join("config.toml");
        std::fs::write(&path, "this is not valid TOML = [\n").unwrap();

        let err = Config::load().expect_err("invalid TOML must not fall back to defaults");
        let text = format!("{err:#}");
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(
            text.contains("line 1"),
            "a TOML error must carry its position: {text}"
        );
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

    // -----------------------------------------------------------------------
    // `set_existing_section_stage` and `insert_missing_stage_line` regression
    // tests for defects found in R83 review (fixed in this worker branch).
    // -----------------------------------------------------------------------

    /// A `stage.flag = true` dotted key inside `[native_host]` makes
    /// `toml_edit` produce an implicit table item with no byte span.  The
    /// previous code called `.expect()` on that span and panicked.  After the
    /// fix the function must return an error (not panic) and must not modify
    /// any data.
    #[test]
    fn dotted_stage_key_returns_error_not_panic() {
        let raw = "[native_host]\nmachine = \"desk\"\nstage.flag = true\n";
        let doc = toml_edit::ImDocument::parse(raw).unwrap();
        let table = doc
            .get("native_host")
            .and_then(|s| s.as_table())
            .expect("fixture has [native_host]");
        let literal = toml_edit::Value::from("C:/stage").to_string();
        let result = set_existing_section_stage(raw, table, &literal);
        assert!(
            result.is_err(),
            "a dotted stage key must return an error, not panic; got: {result:?}"
        );
        let msg = format!("{:?}", result.unwrap_err());
        assert!(
            msg.contains("dotted") || msg.contains("implicit") || msg.contains("table"),
            "error message should mention the dotted/implicit-table shape; got: {msg}"
        );
    }

    /// A `[native_host]` section with no `stage` key whose last line has no
    /// trailing newline, inside a CRLF file.  The previous code inserted a lone
    /// `\n` instead of `\r\n`.
    ///
    /// Fixture bytes: `# c1\r\n[native_host]\r\nmachine = "desk"` (no final
    /// newline).
    #[test]
    fn insert_stage_in_crlf_file_with_no_trailing_newline_uses_crlf_separator() {
        let raw = "# c1\r\n[native_host]\r\nmachine = \"desk\"";
        let doc = toml_edit::ImDocument::parse(raw).unwrap();
        let table = doc
            .get("native_host")
            .and_then(|s| s.as_table())
            .expect("fixture has [native_host]");
        let literal = toml_edit::Value::from("C:/stage").to_string();
        let result =
            set_existing_section_stage(raw, table, &literal).expect("insert should succeed");
        assert!(
            result.contains("\r\nstage = "),
            "inserted stage line must be preceded by CRLF, not a lone LF; \
             result bytes: {:?}",
            result.as_bytes()
        );
        let lone_lf_count = result
            .bytes()
            .enumerate()
            .filter(|&(i, b)| b == b'\n' && (i == 0 || result.as_bytes()[i - 1] != b'\r'))
            .count();
        assert_eq!(
            lone_lf_count,
            0,
            "result must contain no lone LF bytes; result bytes: {:?}",
            result.as_bytes()
        );
        let reparsed: toml_edit::DocumentMut = result.parse().expect("must parse back");
        assert_eq!(
            reparsed
                .get("native_host")
                .and_then(|s| s.get("stage"))
                .and_then(|i| i.as_str()),
            Some("C:/stage")
        );
    }

    /// The root-level dotted spelling `native_host.stage.flag = true` produces
    /// the same implicit `stage` table as the in-section form, so it must take
    /// the same error path instead of panicking.
    #[test]
    fn root_dotted_stage_key_returns_error_not_panic() {
        let raw = "native_host.stage.flag = true\n";
        let doc = toml_edit::ImDocument::parse(raw).unwrap();
        let table = doc
            .get("native_host")
            .and_then(|s| s.as_table())
            .expect("fixture has an implicit native_host table");
        let literal = toml_edit::Value::from("C:/stage").to_string();
        let result = set_existing_section_stage(raw, table, &literal);
        assert!(
            result.is_err(),
            "a root dotted stage key must return an error, not panic; got: {result:?}"
        );
    }

    /// An empty `[native_host]` header as the file's last line with no trailing
    /// newline, in a CRLF file, must get a `\r\n` before the inserted line.
    #[test]
    fn insert_stage_in_empty_crlf_section_at_eof_uses_crlf_separator() {
        let raw = "# c1\r\n[native_host]";
        let doc = toml_edit::ImDocument::parse(raw).unwrap();
        let table = doc
            .get("native_host")
            .and_then(|s| s.as_table())
            .expect("fixture has [native_host]");
        let literal = toml_edit::Value::from("C:/stage").to_string();
        let result =
            set_existing_section_stage(raw, table, &literal).expect("insert should succeed");
        assert!(
            result.contains("[native_host]\r\nstage = "),
            "empty section insert must be preceded by CRLF, not a lone LF; \
             result bytes: {:?}",
            result.as_bytes()
        );
        let lone_lf_count = result
            .bytes()
            .enumerate()
            .filter(|&(i, b)| b == b'\n' && (i == 0 || result.as_bytes()[i - 1] != b'\r'))
            .count();
        assert_eq!(
            lone_lf_count,
            0,
            "result must contain no lone LF bytes; result bytes: {:?}",
            result.as_bytes()
        );
        let reparsed: toml_edit::DocumentMut = result.parse().expect("must parse back");
        assert_eq!(
            reparsed
                .get("native_host")
                .and_then(|s| s.get("stage"))
                .and_then(|i| i.as_str()),
            Some("C:/stage")
        );
    }

    /// When `[native_host]` is followed by `[native_host.child]`, the stage
    /// line must be inserted into `[native_host]`'s own key block (before the
    /// child header), not after the child's last key.
    #[test]
    fn insert_stage_succeeds_when_native_host_has_child_subtable() {
        let raw = "[native_host]\nmachine = \"desk\"\n[native_host.child]\nfoo = 1\n";
        let doc = toml_edit::ImDocument::parse(raw).unwrap();
        let table = doc
            .get("native_host")
            .and_then(|s| s.as_table())
            .expect("fixture has [native_host]");
        let literal = toml_edit::Value::from("/tmp/stage").to_string();
        let result = set_existing_section_stage(raw, table, &literal)
            .expect("insert with child subtable must succeed");
        let stage_pos = result.find("stage =").expect("stage line must be present");
        let child_pos = result
            .find("[native_host.child]")
            .expect("child header must be present");
        assert!(
            stage_pos < child_pos,
            "stage must be inserted before [native_host.child]; \
             stage_pos={stage_pos} child_pos={child_pos};\n{result}"
        );
        let reparsed: toml_edit::DocumentMut = result.parse().expect("must parse back");
        assert_eq!(
            reparsed
                .get("native_host")
                .and_then(|s| s.get("stage"))
                .and_then(|i| i.as_str()),
            Some("/tmp/stage")
        );
    }

    /// Update path: an existing `stage` line with a CRLF ending, an inline
    /// comment, a Windows backslash path, and no trailing newline at EOF.  Only
    /// the value bytes may change; the comment, the CRLF and the absent trailing
    /// newline must all survive untouched.
    #[test]
    fn update_stage_with_comment_crlf_backslash_no_trailing_newline() {
        let raw = "[native_host]\r\nstage = 'C:\\Users\\old' # keep this comment";
        let doc = toml_edit::ImDocument::parse(raw).unwrap();
        let table = doc
            .get("native_host")
            .and_then(|s| s.as_table())
            .expect("fixture has [native_host]");
        let new_stage = "C:\\Users\\new";
        let literal = toml_edit::Value::from(new_stage).to_string();
        let result = set_existing_section_stage(raw, table, &literal).expect("update must succeed");
        assert!(
            result.contains("# keep this comment"),
            "inline comment must survive the splice; result: {result:?}"
        );
        assert!(
            !result.ends_with('\n'),
            "absent trailing newline must remain absent; result: {result:?}"
        );
        let lone_lf_count = result
            .bytes()
            .enumerate()
            .filter(|&(i, b)| b == b'\n' && (i == 0 || result.as_bytes()[i - 1] != b'\r'))
            .count();
        assert_eq!(
            lone_lf_count,
            0,
            "result must not introduce a lone LF; result bytes: {:?}",
            result.as_bytes()
        );
        let reparsed: toml_edit::DocumentMut = result.parse().expect("must parse back");
        assert_eq!(
            reparsed
                .get("native_host")
                .and_then(|s| s.get("stage"))
                .and_then(|i| i.as_str()),
            Some(new_stage)
        );
    }
}

/// Rewrite `[native_host]` inside an existing `raw` so that only the bytes of
/// the `stage` value change, and return the new full text. The `table` is
/// borrowed from a span-preserving `ImDocument` of `raw`, so the spans it
/// exposes are byte offsets into `raw`.
///
/// Splices the new value's TOML literal over the old value's byte span, or,
/// when the key is absent, inserts a line for it at the section's last line. In
/// both cases every byte outside the changed span — comments, other sections, a
/// BOM, and the line endings (CRLF, LF or a mix) — is preserved.
///
/// Returns an error when the `stage` key exists but is a dotted or implicit
/// table (`stage.flag = true`), which has no byte span to splice. In that case
/// the file is left untouched.
fn set_existing_section_stage(
    raw: &str,
    table: &toml_edit::Table,
    literal: &str,
) -> anyhow::Result<String> {
    match table.get("stage") {
        // The key is already there: replace exactly its bytes. The span covers
        // the whole TOML literal including its quotes, so the splice swaps
        // value-for-value and touches nothing else.
        Some(stage_item) => {
            // An implicit dotted table (`stage.flag = true`) is represented as an
            // Item::Table with no byte span. Splicing is impossible and the
            // value cannot be read back as a string, so refuse without touching
            // the file.
            let span = stage_item.span().ok_or_else(|| {
                anyhow::anyhow!(
                    "`native_host.stage` is a dotted or implicit table, not a string; \
                     `stage` cannot be set while that shape is present"
                )
            })?;
            let mut out = String::with_capacity(raw.len() + literal.len().max(span.len()));
            out.push_str(&raw[..span.start]);
            out.push_str(literal);
            out.push_str(&raw[span.end..]);
            Ok(out)
        }
        // The key is missing inside the section: append a line for it, reusing
        // the line ending of the line it follows so a CRLF file stays CRLF and
        // a mixed file stays mixed.
        None => insert_missing_stage_line(raw, table, literal),
    }
}

/// Insert `stage = <literal>` as a new line at the end of an existing,
/// non-empty section (or straight under `[native_host]` when the section has
/// no rows), so every pre-existing byte is kept. `table` is the same
/// span-preserving borrow described on [`set_existing_section_stage`].
///
/// When `[native_host]` is followed by a child subtable such as
/// `[native_host.child]`, the child appears as a `Table` item in the iterator.
/// Only direct key-value items (i.e. `item.is_value()`) are considered when
/// locating the insertion point so the new line lands in `[native_host]`'s own
/// key block, not under the child.
fn insert_missing_stage_line(
    raw: &str,
    table: &toml_edit::Table,
    literal: &str,
) -> anyhow::Result<String> {
    // `base` is the byte offset of the end of the line the new one follows:
    // the section's last direct key-value row, or the `[native_host]` header
    // when empty or when every child entry is a subtable.
    //
    // Only `is_value()` items are direct key-value pairs; a child subtable such
    // as `[native_host.child]` is an `Item::Table` (`is_table() == true`) and
    // its span covers the child's entire block — inserting there would put
    // `stage` under the child, not under `[native_host]`.
    let base = match table
        .iter()
        .filter(|(_, item)| item.is_value())
        .last()
        .and_then(|(_, item)| item.span())
    {
        Some(span) => span.end,
        None => {
            // Either the section has no direct key-value rows (empty or
            // only subtables), or spans were not preserved. Insert straight
            // after the section header.
            table
                .span()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "`native_host` section has no byte span; \
                         `stage` cannot be inserted"
                    )
                })?
                .end
        }
    };

    // The line we follow may be the file's last and carry no newline of its
    // own, so there is nothing local to copy. Read the file's own convention
    // instead — but only from line breaks that are *structure*: a `\r\n` that
    // is data inside a string must not choose the separator. See
    // [`dominant_line_ending`].
    let dominant_ending = dominant_line_ending(raw, base);

    // The character after `base`, past the line we are appending to.
    let Some(newline) = raw[base..].find('\n') else {
        // The line we follow is the file's last and carries no newline: our
        // line gets its own separator using the file's dominant ending, and the
        // file's absent trailing newline stays absent.
        let mut out = String::with_capacity(raw.len() + literal.len() + dominant_ending.len());
        out.push_str(raw);
        out.push_str(dominant_ending);
        out.push_str(&format!("stage = {literal}"));
        return Ok(out);
    };
    let nl = base + newline; // absolute offset of that newline
    let ending = if raw[..nl].ends_with('\r') {
        "\r\n"
    } else {
        "\n"
    };

    let mut out = String::with_capacity(raw.len() + literal.len() + ending.len());
    out.push_str(&raw[..nl + 1]);
    out.push_str(&format!("stage = {literal}{ending}"));
    out.push_str(&raw[nl + 1..]);
    Ok(out)
}

/// Decide which line ending a line appended at the end of the file should use,
/// when the line it follows carries no newline of its own.
///
/// Only line breaks that are *structure* count. A `\r\n` that is data inside a
/// TOML string — most easily a multi-line string — is string content, not the
/// file's line-ending convention, and must not choose the separator. The
/// string literals are read back from the same text through the span-preserving
/// parser, so the count matches the lines the file really lays out.
///
/// The majority ending wins, so a mostly-LF file with one stray CRLF, or a CRLF
/// that is only string data, stays LF. When the two counts tie there is no
/// dominant convention; the ending that terminates the line the new one follows
/// — the nearest newline before `before` — is reused, and a file with no line
/// break at all falls back to `\n`.
fn dominant_line_ending(raw: &str, before: usize) -> &'static str {
    let bytes = raw.as_bytes();
    let mut string_spans: Vec<std::ops::Range<usize>> = Vec::new();
    // `raw` was parsed by the caller, so this succeeds for every input that
    // reaches here; the empty-span fallback only avoids a second panic path.
    if let Ok(doc) = toml_edit::ImDocument::parse(raw) {
        collect_string_spans(doc.as_table(), &mut string_spans);
    }

    let (mut crlf, mut lf) = (0usize, 0usize);
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'\n' || string_spans.iter().any(|s| i >= s.start && i < s.end) {
            continue;
        }
        if i > 0 && bytes[i - 1] == b'\r' {
            crlf += 1;
        } else {
            lf += 1;
        }
    }

    if crlf > lf {
        "\r\n"
    } else if lf > crlf {
        "\n"
    } else {
        preceding_line_ending(raw, before).unwrap_or("\n")
    }
}

/// The line ending that terminates the line before `before`, when the text has
/// one: `\r\n` if the nearest preceding `\n` is preceded by `\r`, else `\n`.
fn preceding_line_ending(raw: &str, before: usize) -> Option<&'static str> {
    let bytes = raw.as_bytes();
    let mut i = before.min(bytes.len());
    while i > 0 {
        i -= 1;
        if bytes[i] == b'\n' {
            return Some(if i > 0 && bytes[i - 1] == b'\r' {
                "\r\n"
            } else {
                "\n"
            });
        }
    }
    None
}

/// Collect the byte spans of every string literal in `table`, including those
/// nested in inline tables and arrays, so line breaks inside a string can be
/// told apart from the ones that lay out the file.
fn collect_string_spans(table: &toml_edit::Table, spans: &mut Vec<std::ops::Range<usize>>) {
    for (_, item) in table.iter() {
        collect_item_string_spans(item, spans);
    }
}

fn collect_item_string_spans(item: &toml_edit::Item, spans: &mut Vec<std::ops::Range<usize>>) {
    match item {
        toml_edit::Item::Value(value) => collect_value_string_spans(value, spans),
        toml_edit::Item::Table(table) => collect_string_spans(table, spans),
        toml_edit::Item::ArrayOfTables(tables) => {
            for table in tables.iter() {
                collect_string_spans(table, spans);
            }
        }
        toml_edit::Item::None => {}
    }
}

fn collect_value_string_spans(value: &toml_edit::Value, spans: &mut Vec<std::ops::Range<usize>>) {
    match value {
        toml_edit::Value::String(_) => {
            if let Some(span) = value.span() {
                spans.push(span);
            }
        }
        toml_edit::Value::Array(array) => {
            for value in array.iter() {
                collect_value_string_spans(value, spans);
            }
        }
        toml_edit::Value::InlineTable(table) => {
            for (_, value) in table.iter() {
                collect_value_string_spans(value, spans);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod line_ending_tests {
    use super::*;

    /// Insert `stage = "C:/stage"` into a fixture's `[native_host]` section.
    fn insert_into(raw: &str) -> String {
        let doc = toml_edit::ImDocument::parse(raw).expect("fixture must be valid TOML");
        let table = doc
            .get("native_host")
            .and_then(|s| s.as_table())
            .expect("fixture must have [native_host]");
        let literal = toml_edit::Value::from("C:/stage").to_string();
        set_existing_section_stage(raw, table, &literal).expect("insert should succeed")
    }

    /// R83b fixture: 21 LF breaks and a single CRLF on the first line. On main
    /// the lone CRLF made the appended line use a CRLF separator.
    #[test]
    fn insert_after_last_line_uses_lf_when_crlf_is_a_minority() {
        let mut raw = String::from("# intro\r\n");
        for i in 0..20 {
            raw.push_str(&format!("k{i} = {i}\n"));
        }
        raw.push_str("[native_host]\nmachine = \"desk\"");
        let result = insert_into(&raw);
        assert!(
            result.contains("machine = \"desk\"\nstage = "),
            "a mostly-LF file must get an LF separator; result bytes: {:?}",
            result.as_bytes()
        );
        assert!(
            !result.contains("machine = \"desk\"\r\nstage = "),
            "the lone CRLF must not be read as the file's ending; result bytes: {:?}",
            result.as_bytes()
        );
    }

    /// R83b fixture: the only CRLF is data inside a multi-line string; every
    /// structural line break is LF.
    #[test]
    fn insert_after_last_line_ignores_crlf_inside_a_multiline_string() {
        let raw = "[native_host]\nnote = \"\"\"\nkeep\r\nme\n\"\"\"\nmachine = \"desk\"";
        let result = insert_into(raw);
        assert!(
            result.contains("machine = \"desk\"\nstage = "),
            "a CRLF inside a string must not choose the separator; result bytes: {:?}",
            result.as_bytes()
        );
        assert!(
            !result.contains("machine = \"desk\"\r\nstage = "),
            "the separator must not be CRLF; result bytes: {:?}",
            result.as_bytes()
        );
    }

    /// A pure-CRLF file keeps its CRLF when a line is appended at EOF.
    #[test]
    fn insert_after_last_line_keeps_crlf_in_a_pure_crlf_file() {
        let raw = "[native_host]\r\nmachine = \"desk\"";
        let result = insert_into(raw);
        assert!(
            result.contains("machine = \"desk\"\r\nstage = "),
            "a pure-CRLF file must keep a CRLF separator; result bytes: {:?}",
            result.as_bytes()
        );
    }

    /// A pure-LF file keeps its LF and gains no `\r`.
    #[test]
    fn insert_after_last_line_keeps_lf_in_a_pure_lf_file() {
        let raw = "[native_host]\nmachine = \"desk\"";
        let result = insert_into(raw);
        assert!(
            result.contains("machine = \"desk\"\nstage = "),
            "a pure-LF file must keep an LF separator; result bytes: {:?}",
            result.as_bytes()
        );
        assert!(
            !result.contains('\r'),
            "a pure-LF file must gain no CR byte; result bytes: {:?}",
            result.as_bytes()
        );
    }

    /// A tie in the structural-break counts has no dominant ending, so the
    /// ending of the line the new one follows is reused.
    #[test]
    fn insert_after_last_line_on_a_tie_reuses_the_followed_line_ending() {
        let raw = "[native_host]\r\nmachine = \"desk\"\nnote = \"x\"";
        let result = insert_into(raw);
        assert!(
            result.contains("note = \"x\"\nstage = "),
            "a tie must reuse the ending of the followed line; result bytes: {:?}",
            result.as_bytes()
        );
    }
}
