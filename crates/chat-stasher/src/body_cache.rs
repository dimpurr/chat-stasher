//! The per-machine **body cache** (ADR-034).
//!
//! W117 measured the archive's read path and split it in two: metadata is
//! already fast (rustic keeps snapshot/index/tree objects in its own cache, and
//! a warm `overview` is 4–5 s with zero remote re-reads), while *conversation
//! bodies* are re-downloaded on every run — a 107 MB session cost 22.6 s warm,
//! with `repo_cache_delta=0`. rustic never caches data packs
//! (`rustic_core-0.12.0 src/backend.rs:82`: `Pack => false`), so ADR-034 adds a
//! second, deliberately disposable cache for exactly those bytes.
//!
//! What this cache is, in the terms ADR-034 fixed:
//!
//! * **Per machine, one global quota, shared across every destination.** There
//!   is no per-destination allowance (`[cache] max_bytes`).
//! * **Content-addressed.** An entry is keyed by `(pack id, offset, length)` —
//!   the ciphertext's own coordinates. See [`CacheKey`] for why the key is not
//!   the *data blob id* the ADR names, and what that costs.
//! * **The remote's ciphertext, byte for byte.** Nothing is decrypted on the
//!   way in or out, no second key is introduced, and no plaintext ever reaches
//!   the disk.
//! * **Never trusted.** A hit is re-hashed on every read; a mismatch deletes
//!   the entry and reports a miss, so a corrupt cache costs speed and never
//!   costs correctness.
//! * **Disposable.** Losing the whole directory changes nothing but timing.
//! * **Its own files, and only its own.** A directory this cache creates carries
//!   a marker file, and a directory without it is refused rather than filled or
//!   emptied: `[cache] dir` is a path a user typed, and a typo in it must not
//!   cost them the contents of the directory it points at. Inside a marked root
//!   only files whose names match the layout below are ever written or deleted
//!   — see [`scan`] for the two levels that are the whole of it.
//!
//! Filling is bounded by LRU eviction against the quota, by a per-entry ceiling
//! (an entry larger than the whole quota can never be held), and by the
//! session rule: a session larger than a tenth of the quota is read through but
//! never stored (ADR-034's anti-thrash rule).
//!
//! Concurrency: `read` may run in several processes at once (the dashboard, the
//! hourly run, a manual read), so every write is a temp file plus a rename and
//! every eviction pass holds a cross-process lock. Nothing here assumes it is
//! the only process touching the directory.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use rustic_core::{FileType, Id, ReadBackend, RusticResult, WriteBackend};
use sha2::{Digest, Sha256};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Default quota: 2 GiB (ADR-034 decision 3: "a default of 2 GB").
///
/// It is a starting point, not a measurement: the two machines this was built
/// for are configured by hand (50 GB and 5–10 GB), and the value only decides
/// how much disk the cache may occupy before LRU starts evicting.
pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// A session larger than this fraction of the quota is never cached
/// (ADR-034 decision 3: "a single session larger than 10% of the quota is
/// not cached").
pub const SESSION_QUOTA_DENOMINATOR: u64 = 10;

/// Magic bytes at the start of every entry file.
///
/// Present so a file that is not ours — or a future format — is rejected as a
/// miss instead of being interpreted as a payload.
pub const ENTRY_MAGIC: [u8; 8] = *b"CSBODY01";

/// `magic (8) + payload length (8, little endian) + payload sha256 (32)`.
pub const ENTRY_HEADER_LEN: usize = 8 + 8 + 32;

/// Sentinel for "no session scope declared", so the write path can tell
/// "unscoped, allow" from "scoped to 0 bytes".
const NO_SESSION: u64 = u64::MAX;

/// A temporary file older than this is assumed to belong to a process that
/// died before renaming it into place, and may be evicted.
const STALE_TEMP: Duration = Duration::from_secs(3600);

/// Name of the cross-process lock file, inside the cache root.
const LOCK_NAME: &str = ".lock";

/// Name of the marker file that says a directory is **this cache's** root.
///
/// Written when the cache creates its own directory, and required before
/// anything in that directory is written or deleted: `[cache] dir` is a path a
/// user typed, and a typo in it must not turn `cache clear`, or an ordinary
/// read's eviction pass, into a delete of somebody else's files. Dot-prefixed,
/// so it is never mistaken for an entry.
const MARKER_NAME: &str = ".chat-stasher-body-cache";

/// What the marker says. A line-oriented record, so a human who opens the file
/// learns what put it there.
const MARKER_CONTENTS: &[u8] = b"chat-stasher body cache v1\n";

/// The part of the marker that is checked.
///
/// A prefix rather than the whole file: a later format may append its own line
/// to the same marker, and requiring today's bytes exactly would refuse a
/// directory this tool itself created yesterday.
const MARKER_PREFIX: &[u8] = b"chat-stasher body cache";

/// Whether an operation may use — and fill — the body cache.
///
/// This is not a per-user preference but a property of the operation, and it is
/// the reason ADR-034's anti-thrash rule cannot be broken by accident:
///
/// * [`Policy::ReadThrough`] — one session's body, read because a user asked
///   for it. Hits are served, misses are stored (`read --session`, the
///   dashboard's session load).
/// * [`Policy::Bulk`] — a scan or an integrity check. The cache is not
///   installed at all, so these runs neither fill it (no eviction pressure on
///   the data a user actually reads) nor *read* it: `verify` in particular has
///   to prove the **remote** is intact, and a cache that answered for the
///   remote would turn "the archive is corrupt" into "this laptop's cache is
///   corrupt", and the other way round (`verify`, `export`, `dest-init`,
///   `push`, `read --all-machines`, index rebuilds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Single-session body reads: read through, and store what was read.
    ReadThrough,
    /// Bulk scans and integrity checks: do not install the cache at all.
    Bulk,
}

/// Resolved cache settings for this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Directory the entries live in. Never synced, never inside the archive.
    pub root: PathBuf,
    /// Quota in bytes; `0` disables the cache.
    pub max_bytes: u64,
}

impl Settings {
    /// Whether the cache is switched on.
    pub fn enabled(&self) -> bool {
        self.max_bytes > 0
    }

    /// Open a handle. Cheap: nothing is read from disk until the first lookup.
    pub fn open(&self) -> BodyCache {
        BodyCache::new(self.root.clone(), self.max_bytes)
    }
}

/// The `[cache] max_bytes` value: a byte count, written either as a plain
/// integer or with a unit suffix.
///
/// Both spellings are accepted because the two users of this key want different
/// things: a script wants an exact number, a human writing `max_bytes = "50GB"`
/// wants to not do arithmetic. The suffixes are unambiguous on purpose —
/// `GB`/`MB`/`KB` are powers of 1000, `GiB`/`MiB`/`KiB` are powers of 1024 —
/// and a value this parser does not understand is an error, never a silent
/// default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheSize(pub u64);

impl CacheSize {
    /// The size in bytes.
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for CacheSize {
    /// Print the exact byte count plus the largest binary unit that fits, so a
    /// user reading it back can check it against what they meant to write.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = self.0;
        let (value, unit) = if bytes >= 1024 * 1024 * 1024 {
            (bytes as f64 / (1024.0 * 1024.0 * 1024.0), "GiB")
        } else if bytes >= 1024 * 1024 {
            (bytes as f64 / (1024.0 * 1024.0), "MiB")
        } else if bytes >= 1024 {
            (bytes as f64 / 1024.0, "KiB")
        } else {
            (bytes as f64, "B")
        };
        write!(f, "{bytes} B ({value:.1} {unit})")
    }
}

impl serde::Serialize for CacheSize {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for CacheSize {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = CacheSize;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a byte count, either an integer or a string like \"50GB\" / \"512MiB\"",
                )
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<CacheSize, E> {
                Ok(CacheSize(v))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<CacheSize, E> {
                u64::try_from(v)
                    .map(CacheSize)
                    .map_err(|_| E::custom("cache size cannot be negative"))
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<CacheSize, E> {
                parse_size(v).map(CacheSize).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// Parse `"50GB"` / `"512MiB"` / `"1048576"` into a byte count.
///
/// The unit table is spelled out rather than inferred, so an unknown unit is an
/// error naming the accepted ones instead of a guess: `max_bytes = "50G"` is
/// rejected rather than read as 50 gigabytes, and `max_bytes = "fifty"` is
/// rejected rather than read as anything. Case is ignored (`"50gb"` and
/// `"50GB"` are the same), and the two families are kept apart: `GB` is 10⁹
/// bytes, `GiB` is 2³⁰. A fractional value (`"1.5GB"`) is refused rather than
/// rounded — the exact byte count is always writable, so a rounding rule would
/// only add a way for the file and the intent to differ.
pub fn parse_size(raw: &str) -> Result<u64, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("cache size is empty; write a byte count such as `max_bytes = 53687091200` or `max_bytes = \"50GB\"`".to_string());
    }
    let lowered = trimmed.to_ascii_lowercase();
    let digits_end = lowered
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(lowered.len());
    let (digits, suffix) = lowered.split_at(digits_end);
    if digits.is_empty() {
        return Err(format!(
            "cache size `{raw}` does not start with a number; write a byte count such as `max_bytes = 53687091200` or `max_bytes = \"50GB\"`"
        ));
    }
    let value: u64 = digits
        .parse()
        .map_err(|e| format!("cache size `{raw}` is not a whole number of bytes: {e}"))?;
    let multiplier: u64 = match suffix.trim() {
        "" | "b" => 1,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        "kib" => 1024,
        "mib" => 1024 * 1024,
        "gib" => 1024 * 1024 * 1024,
        "tib" => 1024 * 1024 * 1024 * 1024,
        other => {
            return Err(format!(
                "cache size `{raw}` has an unknown unit `{other}`; accepted units are b, kb, mb, gb, tb (powers of 1000) and kib, mib, gib, tib (powers of 1024), or no unit for bytes"
            ))
        }
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("cache size `{raw}` is larger than a u64 can hold"))
}

/// Resolve this machine's body-cache settings from the config (ADR-034).
///
/// The root defaults to the platform cache directory (`~/Library/Caches` on
/// macOS, `$XDG_CACHE_HOME` or `~/.cache` on Linux, `%LOCALAPPDATA%` on
/// Windows) rather than anything inside the data or config directory: a cache
/// directory is exactly what every sync tool and backup tool is expected to
/// skip, and ADR-034 requires the cache to take part in no synchronisation.
///
/// An unset `max_bytes` means [`DEFAULT_MAX_BYTES`] — a documented default, not
/// an unknown turned into a number. A `[cache]` section that *could not be
/// read* is a third state: not an absent section (which takes the default) and
/// not a valid one, so this is an error and the cache is off. Nothing here
/// guesses a quota from a value the user mistyped.
pub fn settings_for(config: &crate::config::Config) -> Result<Settings> {
    if let Some(problem) = config.cache_error.as_deref() {
        return Err(anyhow!("{problem}"));
    }
    let root = match config.cache.as_ref().and_then(|c| c.dir.as_deref()) {
        Some(raw) => {
            crate::config::expand_tilde(raw).map_err(|e| anyhow!("cache.dir `{raw}`: {e}"))?
        }
        None => default_root(),
    };
    // reason: an absent `[cache]` section, or a section without `max_bytes`,
    // means "use the documented default quota" — a config default, never an
    // unread value collapsed into a number.
    let max_bytes = config
        .cache
        .as_ref()
        .and_then(|c| c.max_bytes)
        .map(CacheSize::bytes)
        .unwrap_or(DEFAULT_MAX_BYTES);
    Ok(Settings { root, max_bytes })
}

/// The default cache root for this platform, or a home-relative fallback when
/// the platform has no cache directory to offer.
pub fn default_root() -> PathBuf {
    match crate::scanner::user_cache_dirs().into_iter().next() {
        Some(dir) => dir.join("chat-stasher").join("body"),
        // No platform cache directory was found (a stripped-down environment).
        // `~/.cache` is the documented XDG spelling, and the same fallback
        // `doctor` already uses for the metadata cache root.
        None => crate::config::home_dir()
            .join(".cache")
            .join("chat-stasher")
            .join("body"),
    }
}

/// Why a body cache is, or is not, in use for one operation.
///
/// Six states, not two, because "no cache" has five different causes and only
/// one of them is the user's decision. Collapsing them would mean a command
/// that silently ran uncached while the user believed a 50 GB quota was in
/// effect — the same class of mistake as recording an unknown as zero — or one
/// that wrote into a directory the user pointed at by mistake.
#[derive(Debug, Clone)]
pub enum Availability {
    /// The cache is installed for this operation.
    On(Arc<BodyCache>),
    /// `max_bytes = 0`: the user turned it off.
    Off,
    /// This operation is bulk work; ADR-034 keeps those out of the cache.
    Bulk,
    /// The configured location could not be resolved, or could not be
    /// inspected. The read continues uncached, and the reason is carried so the
    /// caller can say so rather than pretending the cache is simply off.
    Unresolved(String),
    /// The configured location is there and is not this cache's directory. The
    /// read continues uncached: nothing is written to that directory, and
    /// nothing in it is deleted.
    Foreign(String),
    /// `[cache]` was present and could not be read. The cache is off, because
    /// the quota the user meant to write is unknown — falling back to the
    /// documented default would report a quota nobody asked for as if they had
    /// asked for it. Distinct from `Foreign` because the fix is a different
    /// line of the config.
    Invalid(String),
}

impl Availability {
    /// The handle to install, or `None` for the off states.
    pub fn handle(&self) -> Option<Arc<BodyCache>> {
        match self {
            Availability::On(cache) => Some(cache.clone()),
            Availability::Off
            | Availability::Bulk
            | Availability::Unresolved(_)
            | Availability::Foreign(_)
            | Availability::Invalid(_) => None,
        }
    }
}

/// Resolve the body cache for one operation (ADR-034).
pub fn for_operation(config: &crate::config::Config, policy: Policy) -> Availability {
    if policy == Policy::Bulk {
        return Availability::Bulk;
    }
    // A `[cache]` section that could not be read is not an absent one: the
    // cache is off, and the reason travels with it. (An absent section, by
    // contrast, means the documented default quota — see `settings_for`.)
    if let Some(problem) = config.cache_error.as_deref() {
        return Availability::Invalid(problem.to_string());
    }
    match settings_for(config) {
        Ok(settings) if settings.enabled() => {
            // The directory must be this cache's before a read may store in it
            // (or delete a corrupt entry from it). Nothing there yet is the
            // normal first run: the cache creates the root, and the marker that
            // claims it, on the first store.
            match root_state(&settings.root) {
                RootState::Absent | RootState::Cache => Availability::On(Arc::new(settings.open())),
                RootState::Foreign(why) => Availability::Foreign(why),
                RootState::Unknown(why) => Availability::Unresolved(why),
            }
        }
        Ok(_) => Availability::Off,
        // A cache that cannot be resolved (a `~otheruser` path, say) is not a
        // reason to fail a read — but it is a reason to say so.
        Err(e) => Availability::Unresolved(format!("{e:#}")),
    }
}

/// One cache entry's identity: a byte range of one pack file.
///
/// ADR-034 names the *data blob id* as the key, and the premise experiment
/// (`crates/chat-stasher/tests/w120_premise_test.rs`) confirms why that would
/// be attractive: data blob ids are plaintext content hashes, so the same
/// conversation content has the same id in two repositories that use different
/// keys. That key is not reachable from here. The only seam this crate can
/// install below `rustic_core`'s decryption layer is `ReadBackend`, where a body
/// read arrives as `read_partial(FileType::Pack, pack_id, false, offset,
/// length)` (`rustic_core-0.12.0 src/index.rs:54-63`) — ciphertext coordinates,
/// with no plaintext id in sight. To key by plaintext id this layer would have
/// to hold another destination's key and re-encrypt on the way out, which
/// ADR-034 rejects (no new keys, no new formats).
///
/// So the key is the ciphertext's identity, which is the stricter half of
/// ADR-034's own fallback ("if they differ, keep one shared quota but store
/// entries per (remote + id) instead"):
/// one global quota, and entries stored apart per destination because a pack id
/// is a hash of the *encrypted* pack and therefore differs between two
/// destinations that do not share a key. The measured cost is that two
/// destinations with different keys share no body-cache entries even when the
/// content is identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    pack: String,
    offset: u32,
    length: u32,
}

impl CacheKey {
    /// The key for one byte range of one pack.
    pub fn new(pack: &Id, offset: u32, length: u32) -> Self {
        Self {
            pack: pack.to_hex().to_string(),
            offset,
            length,
        }
    }

    /// `<root>/<pack id>/<offset>-<length>`.
    ///
    /// One directory per pack groups a session's blocks together, which keeps
    /// the per-directory file count low and makes "which pack is this from"
    /// visible in `ls`.
    pub fn path(&self, root: &Path) -> PathBuf {
        root.join(&self.pack)
            .join(format!("{}-{}", self.offset, self.length))
    }
}

/// Counters for one cache handle. Every number is a count of events in this
/// process, never a guess: `hits + misses + corrupt` is the number of lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Entries served from the cache after their digest verified.
    pub hits: u64,
    /// Lookups with no entry (including one that could not be read at all).
    pub misses: u64,
    /// Entries found but rejected: wrong magic, wrong length, or a payload
    /// whose digest does not match. Each is deleted and counted as a miss too.
    pub corrupt: u64,
    /// Entries written.
    pub stored: u64,
    /// Reads not stored because a single entry is larger than the quota.
    pub skipped_too_large: u64,
    /// Reads not stored because the session being read is larger than a tenth
    /// of the quota.
    pub skipped_session: u64,
    /// Cache I/O failures, and any refusal to delete a path that is not a
    /// regular file inside the cache root (a pack directory replaced by a
    /// symlink, say). Never fatal: the read falls through to the remote.
    pub errors: u64,
}

/// Bytes and entries under a cache root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Sum of the file sizes the quota is enforced against: this cache's entry
    /// files and the temporary files it is still writing. The marker and the
    /// lock file are excluded — they are the cache's own spine, always present
    /// and never evictable, so counting them could only make an eviction pass
    /// delete something it must keep, or leave the cache permanently a few
    /// bytes over a quota it cannot shrink below.
    pub bytes: u64,
    /// Number of entry files (temporary files, the lock file and the marker are
    /// excluded).
    pub entries: usize,
    /// Files and directories under the root that this cache did not write.
    ///
    /// Counted so that a directory holding something else is never described as
    /// an empty cache, and never counted in `bytes`: they are not something the
    /// cache may shrink, so making the quota fit around them would evict the
    /// user's entries to make room for files the cache cannot delete. A
    /// directory that is not a pack directory counts as one and is not walked,
    /// so this is a count of what is there, not a measurement of a tree.
    pub foreign_entries: usize,
}

/// The body cache directory.
#[derive(Debug)]
pub struct BodyCache {
    root: PathBuf,
    max_bytes: u64,
    session_bytes: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    corrupt: AtomicU64,
    stored: AtomicU64,
    skipped_too_large: AtomicU64,
    skipped_session: AtomicU64,
    errors: AtomicU64,
}

impl BodyCache {
    /// A handle for `root` with quota `max_bytes`.
    pub fn new(root: PathBuf, max_bytes: u64) -> Self {
        Self {
            root,
            max_bytes,
            session_bytes: AtomicU64::new(NO_SESSION),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            corrupt: AtomicU64::new(0),
            stored: AtomicU64::new(0),
            skipped_too_large: AtomicU64::new(0),
            skipped_session: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    /// The directory the entries live in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The quota in bytes.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Whether this handle may serve and store entries.
    pub fn enabled(&self) -> bool {
        self.max_bytes > 0
    }

    /// This process's counters.
    pub fn stats(&self) -> Stats {
        Stats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            corrupt: self.corrupt.load(Ordering::Relaxed),
            stored: self.stored.load(Ordering::Relaxed),
            skipped_too_large: self.skipped_too_large.load(Ordering::Relaxed),
            skipped_session: self.skipped_session.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }

    /// Declare the size of the session about to be read, so the write path can
    /// apply ADR-034's "a session larger than a tenth of the quota is never
    /// cached" rule. Reads are unaffected — only writes are gated.
    ///
    /// The scope is process-wide (one CLI process reads one session at a time)
    /// and nests: the returned guard restores whatever was declared before it.
    pub fn declare_session(&self, plaintext_bytes: u64) -> SessionScope<'_> {
        let previous = self.session_bytes.swap(plaintext_bytes, Ordering::Relaxed);
        SessionScope {
            cache: self,
            previous,
        }
    }

    /// Look an entry up. `None` means "not a usable hit" — absent, unreadable,
    /// or present but failing its digest check — and the caller must fetch the
    /// bytes from the remote. A rejected entry is deleted so the refetched copy
    /// can be stored.
    pub fn get(&self, key: &CacheKey) -> Option<Bytes> {
        if !self.enabled() {
            return None;
        }
        let path = key.path(&self.root);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                }
                self.misses.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let mut raw = Vec::new();
        if file.read_to_end(&mut raw).is_err() {
            self.errors.fetch_add(1, Ordering::Relaxed);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let Some(payload) = verify_entry(&raw) else {
            // Corrupt: drop it, so the refetch below can replace it. Deleting a
            // cache entry is not deleting archived data — ADR-034's cache is
            // disposable by construction, and the archive it mirrors is on the
            // remote.
            //
            // Only when the root is a directory this cache created **and** the
            // path is a regular file inside that root, reached without following
            // a symlink: this is the one delete on a read path, and a pack
            // directory that has been replaced by a symlink — or a `[cache] dir`
            // that points somewhere else — must not make a read delete a file it
            // does not own. A miss costs a refetch; a delete here is not this
            // cache's to make.
            let removed = root_state(&self.root) == RootState::Cache
                && canonical_root(&self.root)
                    .is_some_and(|root| matches!(remove_owned_file(&root, &path), Ok(true)));
            if !removed {
                // A corrupt file that could not be deleted is a cache directory
                // whose layout has been replaced by links, and it is counted so
                // that state is visible rather than silently tolerated.
                self.errors.fetch_add(1, Ordering::Relaxed);
            }
            self.corrupt.fetch_add(1, Ordering::Relaxed);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        // LRU is the file's mtime, so a hit has to move it. Best effort: a
        // failed touch costs eviction ordering, never correctness. See
        // [`touch_entry`] for why the handle in hand is not always enough.
        let _touched = touch_entry(&file, &path, SystemTime::now());
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(Bytes::copy_from_slice(payload))
    }

    /// Store one entry, then bring the cache back under its quota.
    ///
    /// Every refusal here is silent by design and visible in [`Stats`]: the
    /// read that produced the bytes already succeeded, and a cache that cannot
    /// accept them is a slow cache, not a failed read.
    pub fn put(&self, key: &CacheKey, payload: &[u8]) {
        if !self.enabled() {
            return;
        }
        let length = payload.len() as u64;
        if length > self.max_bytes {
            // It could never fit, so writing it would only evict other entries
            // and then be evicted itself.
            self.skipped_too_large.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let session = self.session_bytes.load(Ordering::Relaxed);
        if session != NO_SESSION && session > self.max_bytes / SESSION_QUOTA_DENOMINATOR {
            self.skipped_session.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // The root and its marker are made by the cache itself, so the first
        // store on a machine leaves a directory the cache recognises later. A
        // `[cache] dir` pointing at somebody else's directory is refused here
        // rather than filled with entries.
        if ensure_our_root(&self.root).is_err() {
            self.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if write_entry(&self.root, &key.path(&self.root), payload).is_err() {
            self.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.stored.fetch_add(1, Ordering::Relaxed);
        if self.enforce_quota().is_err() {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Bytes and entries currently held.
    ///
    /// `bytes` is the number the quota is enforced against: entry files and the
    /// temporary files still being written, and nothing else — see [`Usage`].
    /// `entries` counts only real entries: neither a temp file nor the lock file
    /// nor the marker is something a user could be shown. `foreign_entries`
    /// counts what is under the root and is not this cache's.
    pub fn usage(&self) -> std::io::Result<Usage> {
        let scanned = scan(&self.root)?;
        let mut usage = Usage {
            foreign_entries: scanned.foreign,
            ..Usage::default()
        };
        for entry in &scanned.ours {
            if entry.kind.counts_toward_quota() {
                usage.bytes += entry.bytes;
            }
            if entry.kind == Owned::Entry {
                usage.entries += 1;
            }
        }
        Ok(usage)
    }

    /// Delete every entry this cache wrote. Returns what was removed, and — in
    /// `foreign_entries` — what it found and did not touch, so the caller can
    /// say the latter out loud instead of leaving the user to wonder.
    ///
    /// Refuses outright unless the root is this cache's own directory: `clear`
    /// is the one command in the cache that destroys data, and the only data it
    /// may destroy is its own. The lock file is kept even then: it is the one
    /// name that must not be unlinked while another process holds it, or two
    /// processes could believe they hold the same lock on two different inodes.
    pub fn clear(&self) -> std::io::Result<Usage> {
        ensure_our_root(&self.root)?;
        let canonical = canonical_root(&self.root)
            .ok_or_else(|| not_ours(&self.root, "the cache root could not be resolved"))?;
        let scanned = scan(&self.root)?;
        let mut removed = Usage {
            foreign_entries: scanned.foreign,
            ..Usage::default()
        };
        for entry in &scanned.ours {
            // Entry files only: the lock, the marker and any in-flight temp file
            // stay, exactly as they did when this pass skipped dot-files.
            if entry.kind != Owned::Entry {
                continue;
            }
            match remove_owned_file(&canonical, &entry.path) {
                Ok(true) => {
                    removed.bytes += entry.bytes;
                    removed.entries += 1;
                }
                // The scan called this an entry, but it is not a regular file
                // inside the root by the time it is removed (it was swapped for
                // a link, say). It is not touched, and it is reported as left
                // alone rather than counted as cleared.
                Ok(false) => removed.foreign_entries += 1,
                Err(e) => return Err(e),
            }
        }
        remove_empty_dirs(&self.root, &canonical);
        Ok(removed)
    }

    /// Drop least-recently-used entries until the quota is met.
    ///
    /// Serialized across processes by an advisory lock on `<root>/.lock`, then
    /// re-measured under that lock: another process may have evicted already,
    /// and two processes that each evicted to their own pre-lock measurement
    /// would delete twice as much as needed.
    ///
    /// Everything this pass can delete is this cache's own: [`scan`] does not
    /// return a file the cache did not write, and the marker and the lock are
    /// not evictable. The root is checked before the lock is taken, so a
    /// `[cache] dir` pointing at somebody else's directory is refused rather
    /// than shrunk.
    fn enforce_quota(&self) -> std::io::Result<()> {
        // Cheap check first: the common case is a cache far below its quota,
        // and taking the lock for that would serialize every read.
        if self.usage()?.bytes <= self.max_bytes {
            return Ok(());
        }
        ensure_our_root(&self.root)?;
        let canonical = canonical_root(&self.root)
            .ok_or_else(|| not_ours(&self.root, "the cache root could not be resolved"))?;
        let _guard = self.lock()?;
        loop {
            let scanned = scan(&self.root)?;
            let mut total: u64 = scanned
                .ours
                .iter()
                .filter(|e| e.kind.counts_toward_quota())
                .map(|e| e.bytes)
                .sum();
            if total <= self.max_bytes {
                return Ok(());
            }
            let now = SystemTime::now();
            let mut youngest_first = scanned
                .ours
                .iter()
                // An entry is evictable whenever it is the oldest; a temp file
                // only once it is old enough to belong to a process that died
                // before renaming it into place.
                .filter(|e| e.kind.evictable() && (e.kind == Owned::Entry || is_stale(e, now)))
                .collect::<Vec<_>>();
            youngest_first.sort_by_key(|e| (e.modified, e.path.clone()));
            let mut deleted_any = false;
            for entry in youngest_first {
                if total <= self.max_bytes {
                    break;
                }
                match remove_owned_file(&canonical, &entry.path) {
                    Ok(true) => {
                        total = total.saturating_sub(entry.bytes);
                        deleted_any = true;
                    }
                    // Refused: not a regular file inside the root any more. Its
                    // bytes are not counted as freed, so the pass stops instead
                    // of pretending the quota was met.
                    Ok(false) => {}
                    Err(_) => {}
                }
            }
            if !deleted_any {
                // Nothing was removable (permissions, a read-only mount). Stop
                // rather than spin: the quota stays exceeded and the next
                // `doctor` says so.
                return Ok(());
            }
        }
    }

    /// Hold the cross-process eviction lock for the guard's lifetime.
    ///
    /// A filesystem without advisory locking (or a read-only root) returns an
    /// error here; the caller treats that as "could not enforce", which is
    /// reported through `errors` and never fails a read.
    fn lock(&self) -> std::io::Result<LockGuard> {
        // The root is made the same way it is made everywhere else — with its
        // marker — so that no path in this module can leave a markerless cache
        // directory behind for a later run to refuse. (In practice the root
        // exists by now: this pass runs after a store.)
        ensure_our_root(&self.root)?;
        let path = self.root.join(LOCK_NAME);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        file.lock()?;
        Ok(LockGuard { file })
    }
}

/// The scope of one session's declared size. Restores the previous declaration
/// when dropped, so nesting is safe.
pub struct SessionScope<'a> {
    cache: &'a BodyCache,
    previous: u64,
}

impl Drop for SessionScope<'_> {
    fn drop(&mut self) {
        self.cache
            .session_bytes
            .store(self.previous, Ordering::Relaxed);
    }
}

/// Holds the advisory lock; releasing it is the file's own drop.
struct LockGuard {
    file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _unlocked = self.file.unlock();
    }
}

/// One file found under the cache root.
struct ScannedEntry {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
    /// What this file is. Never optional: a file this cache did not write is
    /// not returned by [`scan`] at all, so nothing that deletes can reach one.
    kind: Owned,
}

/// The files this cache writes, by name and position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owned {
    /// A stored entry: `<root>/<pack id>/<offset>-<length>`.
    Entry,
    /// A half-written entry: the `write_entry` temp name.
    Temp,
    /// `<root>/.lock`, the cross-process eviction lock.
    Lock,
    /// `<root>.chat-stasher-body-cache`, the claim on the directory.
    Marker,
}

impl Owned {
    /// Whether this file's bytes are part of what the quota is enforced
    /// against. See [`Usage::bytes`] for why the marker and the lock are not.
    fn counts_toward_quota(self) -> bool {
        matches!(self, Owned::Entry | Owned::Temp)
    }

    /// Whether an eviction pass may delete this file once it is old enough.
    fn evictable(self) -> bool {
        matches!(self, Owned::Entry | Owned::Temp)
    }
}

/// Whether `name` is a pack directory as this cache writes it: the lowercase
/// hex of a pack id, and nothing else.
///
/// The round trip through [`Id`] is the check rather than a character count:
/// it is the same parser the writer's `to_hex` is paired with, so a spelling it
/// would accept but never print (uppercase hex, say) is refused instead of
/// matched loosely. Nothing this cache wrote fails it.
fn is_pack_dir_name(name: &str) -> bool {
    name.parse::<Id>()
        .map(|id| id.to_hex().as_str() == name)
        // reason: a name this cache cannot parse as a pack id is not a pack
        // directory it made. The default is "not ours", which is the direction
        // that keeps files: the caller only ever refuses to delete on it.
        .unwrap_or(false)
}

/// Whether `name` is exactly the `<offset>-<length>` an entry is written as.
///
/// Canonical decimal only: the writer is `format!("{offset}-{length}")`, so
/// `007-8` and `0-4 ` are names this cache never produced, and refusing to
/// delete one costs nothing.
fn is_entry_name(name: &str) -> bool {
    let Some((offset, length)) = name.split_once('-') else {
        return false;
    };
    let canonical = |part: &str| {
        part.parse::<u32>()
            .is_ok_and(|value| value.to_string() == part)
    };
    canonical(offset) && canonical(length)
}

/// Whether `name` is a temp file as [`write_entry`] writes one:
/// `.tmp-<pid>-<seq>`, both canonical decimals.
fn is_temp_name(name: &str) -> bool {
    match name.strip_prefix(".tmp-") {
        Some(rest) => is_entry_name(rest),
        None => false,
    }
}

/// What is at the configured cache root.
///
/// Three states where a path check would give two, because "there is nothing
/// there", "there is this cache's directory there" and "there is something else
/// there" call for three different actions — the first two may be used, and the
/// third must not be written to or deleted from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootState {
    /// Nothing is at that path yet. The cache creates it, and its marker, on
    /// the first store.
    Absent,
    /// This cache's own directory: the marker is there.
    Cache,
    /// A directory is there that this cache did not create, or a path that is
    /// not a directory at all. Nothing may be written to it or deleted from it.
    Foreign(String),
    /// What is at that path could not be determined. Distinct from `Foreign`:
    /// "could not look" is not "not ours".
    Unknown(String),
}

/// What is at `root`, and whether this cache may operate there.
pub fn root_state(root: &Path) -> RootState {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RootState::Absent,
        Err(e) => {
            return RootState::Unknown(format!(
                "the configured cache path could not be inspected: {e}"
            ))
        }
    };
    // A symlink is never followed, here or in the walk below: a `[cache] dir`
    // that is a symlink to somebody else's directory is not this cache's
    // directory to fill or empty, and following one out of the tree is how a
    // delete escapes the directory it was aimed at.
    if metadata.file_type().is_symlink() {
        return RootState::Foreign(
            "the configured cache path is a symlink, and this cache never follows one".to_string(),
        );
    }
    if !metadata.is_dir() {
        return RootState::Foreign("the configured cache path is not a directory".to_string());
    }
    match marker_present(root) {
        Ok(true) => RootState::Cache,
        Ok(false) => RootState::Foreign(format!(
            "there is a directory at the configured cache path that chat-stasher did not create \
             (no `{MARKER_NAME}` marker file), so it is not a chat-stasher body cache"
        )),
        Err(why) => RootState::Unknown(why),
    }
}

/// Whether `root` carries the marker this cache writes when it creates the
/// directory: a regular file (not a symlink, not a directory) whose contents
/// begin with [`MARKER_PREFIX`].
///
/// `Err` is "the marker is there but could not be read", which is a different
/// answer from "there is no marker" and must stay one.
fn marker_present(root: &Path) -> Result<bool, String> {
    let path = root.join(MARKER_NAME);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("`{MARKER_NAME}` could not be inspected: {e}")),
    };
    if !metadata.file_type().is_file() {
        // A directory, a symlink or a socket with that name is not a claim this
        // cache made — a definite "not ours", not an unknown.
        return Ok(false);
    }
    match fs::read(&path) {
        Ok(raw) => Ok(raw.starts_with(MARKER_PREFIX)),
        Err(e) => Err(format!("`{MARKER_NAME}` could not be read: {e}")),
    }
}

/// The error every write-side refusal carries: the path, and why it is not the
/// cache's to use.
fn not_ours(root: &Path, why: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("{}: {why}", root.display()),
    )
}

/// Create the cache root, with the marker already inside it, in **one** step.
///
/// The staging directory and the rename are what make this atomic for every
/// other process and thread: at that path there is either nothing, or a
/// directory that already carries the marker — never the directory in between.
/// That in-between state is not a harmless instant: it is exactly what "a
/// directory this cache did not create" looks like from outside, and a `read`
/// that is filling a cold cache does store blobs from several threads at once
/// (`rustic` reads bodies in parallel). Creating the root in two steps made
/// those threads refuse their own cache — five blocks in six, on one measured
/// read — which is the failure this shape exists to prevent.
///
/// A crash between the two leaves the staging directory, not a markerless root:
/// and the staging directory is beside the root, where no walk of the cache ever
/// looks.
fn create_our_root(root: &Path) -> std::io::Result<()> {
    let parent = match root.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".tmp-{}-{}", std::process::id(), next_temp_seq()));
    fs::create_dir(&staging)?;
    fs::write(staging.join(MARKER_NAME), MARKER_CONTENTS)?;
    match fs::rename(&staging, root) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _cleaned = fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

/// Make sure `root` is a directory this cache may write to and delete from.
///
/// Creates it — and the marker that claims it — when nothing is there yet, so
/// the first store on a machine leaves a directory the cache can recognise on
/// every later run. Refuses when something is there that this cache did not
/// create: writing entries into it would fill a directory nobody offered, and
/// deleting from it is the mistake this check exists to prevent.
///
/// Losing the race to create it is not a refusal: what is at the path now is
/// what decides, and a concurrent creator that got there first leaves the same
/// marked directory this call would have made. Only a *second* look that still
/// finds no usable root is an error.
fn ensure_our_root(root: &Path) -> std::io::Result<()> {
    let creation = match root_state(root) {
        RootState::Cache => return Ok(()),
        RootState::Absent => create_our_root(root),
        RootState::Foreign(why) | RootState::Unknown(why) => return Err(not_ours(root, &why)),
    };
    match root_state(root) {
        RootState::Cache => Ok(()),
        RootState::Foreign(why) | RootState::Unknown(why) => Err(not_ours(root, &why)),
        // Nothing is there even after the attempt: the creation failed for a
        // reason the caller should see (a read-only parent, a full disk), or —
        // when the attempt itself is what failed — its own error is the honest
        // one to report.
        RootState::Absent => Err(creation
            .err()
            .unwrap_or_else(|| not_ours(root, "the cache directory could not be created"))),
    }
}

fn is_stale(entry: &ScannedEntry, now: SystemTime) -> bool {
    match now.duration_since(entry.modified) {
        Ok(age) => age > STALE_TEMP,
        // A modification time in the future is not a reason to delete someone's
        // in-progress write.
        Err(_) => false,
    }
}

/// What a walk of a cache root found: the files this cache wrote, and how many
/// things under the root it did not write.
///
/// The two halves are kept apart in the type on purpose. Every caller of this
/// function deletes something, so a file this cache did not write must not be
/// able to reach one: the foreign side is a count, and a count cannot be
/// turned back into a path by accident.
struct Scanned {
    ours: Vec<ScannedEntry>,
    /// Files and directories whose names are not this cache's, at the root or
    /// inside a pack directory. A directory that is not a pack directory counts
    /// as one entry and is not walked.
    foreign: usize,
}

/// Walk the two levels this cache writes, and nothing else.
///
/// `<root>/<pack id>/<offset>-<length>` is the whole of the layout: one
/// directory per pack id, one entry file inside it, plus the root's own
/// `.lock` and marker files, and the `.tmp-<pid>-<seq>` files a store is in the
/// middle of, which live beside the entry they are about to become. A file
/// whose name does not match that, a directory whose name is not a pack id, a
/// second level of nesting, and every symlink (a symlink out of the tree would
/// let a delete land outside it) are **not ours**: they are counted and never
/// entered. That is what keeps `clear` and the eviction pass inside the files
/// the cache itself wrote.
///
/// A directory that cannot be read is fatal rather than skipped, because the
/// caller is deciding what to delete: "could not look" must never be read as
/// "there is nothing there".
fn scan(root: &Path) -> std::io::Result<Scanned> {
    let mut out = Scanned {
        ours: Vec::new(),
        foreign: 0,
    };
    let reader = match fs::read_dir(root) {
        Ok(reader) => reader,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in reader {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        // The cache's own root-level files, by exact name, and only as regular
        // files: a directory or symlink wearing one of these names is not
        // something this cache wrote.
        if name == LOCK_NAME || name == MARKER_NAME {
            if file_type.is_file() {
                let kind = if name == LOCK_NAME {
                    Owned::Lock
                } else {
                    Owned::Marker
                };
                if let Some(entry) = measured(entry.path(), kind)? {
                    out.ours.push(entry);
                }
            } else {
                out.foreign += 1;
            }
            continue;
        }
        // Nothing else at the root is a temp file: both writers put their temp
        // where the file is going — an entry's temp inside its pack directory,
        // and the root's own creation inside a staging directory that is not the
        // root at all (see `create_our_root`).
        if file_type.is_dir() && is_pack_dir_name(&name) {
            scan_pack_dir(&entry.path(), &mut out)?;
            continue;
        }
        out.foreign += 1;
    }
    Ok(out)
}

/// One level inside a pack directory: the entry files and in-flight temp files
/// this cache writes there, and a count of anything else.
fn scan_pack_dir(dir: &Path, out: &mut Scanned) -> std::io::Result<()> {
    let reader = match fs::read_dir(dir) {
        Ok(reader) => reader,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in reader {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        // Anything that is not a regular file — a nested directory, a symlink —
        // is somebody else's, whatever it is called.
        let kind = if !file_type.is_file() {
            None
        } else if is_entry_name(&name) {
            Some(Owned::Entry)
        } else if is_temp_name(&name) {
            Some(Owned::Temp)
        } else {
            None
        };
        match kind {
            Some(kind) => {
                if let Some(entry) = measured(entry.path(), kind)? {
                    out.ours.push(entry);
                }
            }
            None => out.foreign += 1,
        }
    }
    Ok(())
}

/// One file's size and modification time, or `None` when the name is gone.
///
/// "Gone" is not a failure, and it is not rare: a store writes its entry as a
/// temp file and renames it into place, so a walk of the same pack directory —
/// which a concurrent store runs, because one `read` stores blobs from several
/// threads at once — can list a name that is no longer there by the time it is
/// looked at. Returning an error for that would make the walk fail over a file
/// that needs neither measuring nor deleting, and the `put` that caused it would
/// count the failure against a store that succeeded.
///
/// Any other error is still an error: "could not look" must never read as
/// "nothing is there".
///
/// `symlink_metadata` rather than `metadata`: callers reach this only for names
/// that are already regular files, and not following a link is the rule
/// everywhere in this module.
fn measured(path: PathBuf, kind: Owned) -> std::io::Result<Option<ScannedEntry>> {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(Some(ScannedEntry {
        path,
        bytes: metadata.len(),
        modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        kind,
    }))
}

/// The canonical location of the cache root, or `None` when it cannot be
/// resolved (it is gone, or unreadable).
///
/// Resolved once per pass, not once per file: every delete compares against it,
/// and `canonicalize` walks each component of the path.
fn canonical_root(root: &Path) -> Option<PathBuf> {
    fs::canonicalize(root).ok()
}

/// Whether `dir` is a real directory — not a symlink — whose canonical location
/// is a **strict descendant** of the canonical cache root.
///
/// `symlink_metadata` is `lstat`, so a symlink is seen as a symlink rather than
/// as the directory it points at; a symlinked pack directory therefore fails
/// here even when its target is inside the root, and `canonicalize` catches a
/// link anywhere above it.
fn is_owned_directory(canonical_root: &Path, dir: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(dir) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return false;
    }
    match fs::canonicalize(dir) {
        Ok(real) => real.starts_with(canonical_root) && real != canonical_root,
        Err(_) => false,
    }
}

/// Whether `path` is a regular file — not a symlink — inside a directory that is
/// itself inside the canonical cache root.
///
/// Both checks are `lstat`: a symlinked entry fails the first, and a symlinked
/// pack directory fails the second. This is the one predicate every delete goes
/// through, so a file outside the cache can never be reached by one, however
/// much its name looks like an entry.
fn is_owned_regular_file(canonical_root: &Path, path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return false;
    }
    match path.parent() {
        Some(parent) => is_owned_directory(canonical_root, parent),
        None => false,
    }
}

/// Delete `path` only when it is a regular file inside the cache root, reached
/// without following a symlink.
///
/// `Ok(true)` means this call deleted the file; the one other way to reach it is
/// the race where the file passed the check and vanished before the `unlink`, in
/// which case `NotFound` is treated as already-removed. `Ok(false)` means the
/// delete was **refused** because the path is not one of the cache's own regular
/// files, which the caller reports — this includes a path that was already
/// absent when checked, since [`is_owned_regular_file`]'s `lstat` fails first
/// and only a vanish after a successful check reaches the `Ok(true)` arm. `Err`
/// is a real I/O failure on a file that did pass the check.
///
/// # Accepted residual: the check and the unlink are two syscalls
///
/// [`is_owned_regular_file`] (`lstat`) and the `unlink` cannot be made one
/// operation: a process that can write to the cache root can win the window
/// between them and point the unlink at a path it planted after the check, and
/// the store side has the same window between its identity check and its
/// `rename` (see [`write_entry`]). Closing it would need a `dirfd`-relative,
/// `O_NOFOLLOW` unlink/rename that std does not expose. This is accepted, not
/// deferred: the planted-static-symlink scenario of the finding is refused
/// outright, and an adversary with write access to the cache root can already
/// poison entries inside a legitimate pack directory, so deleting outside the
/// root buys no access it does not already have.
///
/// This is the only function in the module that removes a file whose name the
/// cache recognised, and it is deliberately narrower than the names it is
/// given: the caller's `scan` already filtered by layout, and this filters
/// again by what is actually on the disk, because a name can be left behind
/// pointing somewhere else.
fn remove_owned_file(canonical_root: &Path, path: &Path) -> std::io::Result<bool> {
    if !is_owned_regular_file(canonical_root, path) {
        return Ok(false);
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
    }
}

/// Remove the pack directories left empty by eviction or `clear`. Best effort:
/// a directory that is not empty (a concurrent writer just created something in
/// it) simply stays.
///
/// Only a real directory whose name is a pack id, one level below the root: a
/// directory this cache did not name is not this cache's to remove, even when
/// it happens to be empty, and a symlink wearing a pack id fails
/// [`is_owned_directory`] rather than being followed.
fn remove_empty_dirs(root: &Path, canonical_root: &Path) {
    let Ok(reader) = fs::read_dir(root) else {
        return;
    };
    for entry in reader.flatten() {
        if !is_pack_dir_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        // An entry whose type cannot be read is skipped: this pass only removes
        // empty directories, so the only consequence of skipping one is that it
        // stays. Treating "could not stat" as "is a directory" would instead
        // try to remove something we know nothing about.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() && is_owned_directory(canonical_root, &entry.path()) {
            let _removed = fs::remove_dir(entry.path());
        }
    }
}

/// Frame `payload` as a cache entry.
fn encode_entry(payload: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(ENTRY_HEADER_LEN + payload.len());
    raw.extend_from_slice(&ENTRY_MAGIC);
    raw.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    raw.extend_from_slice(&Sha256::digest(payload));
    raw.extend_from_slice(payload);
    raw
}

/// Validate a whole entry file and return its payload, or `None` if it is not
/// a well-formed entry whose bytes hash to what was recorded for them.
///
/// This is the check ADR-034 requires on every hit ("the hash is recomputed
/// on every hit"). It
/// detects a truncated, bit-rotted or partially written file. It is not a
/// check against the remote's own hash: the blob's ciphertext digest is not
/// exposed at this layer, and the pack id covers the whole pack, which is not
/// what is stored here. A cache entry is therefore only ever as trustworthy as
/// it was when written, and it is verified against that record on every read.
fn verify_entry(raw: &[u8]) -> Option<&[u8]> {
    if raw.len() < ENTRY_HEADER_LEN || raw[..ENTRY_MAGIC.len()] != ENTRY_MAGIC {
        return None;
    }
    let length_bytes: [u8; 8] = raw[ENTRY_MAGIC.len()..ENTRY_MAGIC.len() + 8]
        .try_into()
        .ok()?;
    let length = u64::from_le_bytes(length_bytes);
    let payload = raw.get(ENTRY_HEADER_LEN..)?;
    if payload.len() as u64 != length {
        return None;
    }
    let digest = raw.get(ENTRY_MAGIC.len() + 8..ENTRY_HEADER_LEN)?;
    if Sha256::digest(payload).as_slice() != digest {
        return None;
    }
    Some(payload)
}

/// Move one entry to the most-recently-used end of the LRU, best effort.
///
/// The recency signal is the entry file's mtime, and nothing else moves it: a
/// store writes a file whose mtime is the store time, and every eviction pass
/// sorts by what it finds ([`enforce_quota`]). So a hit that does not move the
/// mtime is not a hit as far as eviction is concerned — the entry keeps its old
/// position and eviction degenerates into store order, which is not an LRU.
///
/// `handle` is the read handle the caller already holds, and on Unix that is
/// enough for a file the caller owns. **On Windows it is not**, and that is why
/// this is a function rather than one line at the call site: `File::set_times`
/// is `SetFileTime` on *that* handle (`library/std/src/sys/fs/windows.rs` in the
/// standard library), it needs `FILE_WRITE_ATTRIBUTES`, and `File::open` asks for
/// `GENERIC_READ`, which does not carry that right. The call fails with
/// "Access is denied" and the touch is silently lost. A second handle, opened
/// for writing, does carry it — which is the same remedy the standard library
/// uses for its own path-based `set_times`, and the same one the Rust project
/// applied to its bootstrap (`rust-lang/rust#127849`, closed).
///
/// The open is a fallback and not the default so that the platform where the
/// handle in hand suffices pays no second syscall per hit; a cache hit is on
/// the hot path of every `read`.
///
/// Failure is swallowed by the caller: the bytes are already verified and
/// already in hand, so a touch that cannot happen costs eviction ordering and
/// never correctness — a read-only cache still serves, it just stops ordering
/// by use.
///
/// Coverage: on Unix the fallback below cannot be reached (an owned file's
/// times move through a read handle), so no local test exercises the Windows
/// path and the `windows-latest` job is where it runs. It is deliberately not
/// behind a `cfg`: it compiles and type-checks on every platform, so an error
/// in it breaks the build here rather than one OS's CI, and the caller reaches
/// it on any platform where the first attempt fails.
fn touch_entry(handle: &File, path: &Path, when: SystemTime) -> std::io::Result<()> {
    let times = FileTimes::new().set_modified(when);
    match handle.set_times(times) {
        Ok(()) => Ok(()),
        Err(_) => OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|writable| writable.set_times(times)),
    }
}

/// Write one entry: temp file in the target directory, then rename into place.
///
/// The rename is what makes this safe without a lock: a reader either sees the
/// old file or the new one, never a half-written one, and two processes storing
/// the same key write identical bytes.
///
/// The pack directory is created here rather than with `create_dir_all`, and
/// only after it is proven to be a real directory inside the cache root: a
/// symlinked pack directory would make the temp file and the rename land
/// outside the cache, and a rename is a delete of whatever name it replaced.
///
/// The check and the rename are still two syscalls; the accepted race window
/// that leaves is documented on [`remove_owned_file`].
fn write_entry(root: &Path, path: &Path, payload: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "entry path has no parent")
    })?;
    let canonical = canonical_root(root)
        .ok_or_else(|| not_ours(root, "the cache root could not be resolved"))?;
    match fs::create_dir(parent) {
        Ok(()) => {}
        // Another thread or process got there first; the identity check below
        // decides whether what is there is usable.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    if !is_owned_directory(&canonical, parent) {
        return Err(not_ours(
            parent,
            "the pack directory is not a regular directory inside the cache root",
        ));
    }
    // Dot-prefixed and pid-suffixed: dot-prefixed so it is never mistaken for
    // an entry, pid-suffixed so two processes never share a temp file.
    let candidate = parent.join(format!(".tmp-{}-{}", std::process::id(), next_temp_seq()));
    let mut file = File::create(&candidate)?;
    file.write_all(&encode_entry(payload))?;
    drop(file);
    match fs::rename(&candidate, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Another process evicted the temp file between create and rename,
            // or the rename failed for a real reason. Either way the caller
            // treats this as "not stored"; drop the temp file if it is still
            // ours.
            let _removed = fs::remove_file(&candidate);
            Err(e)
        }
    }
}

/// A per-process counter, so two temp files written in the same millisecond by
/// the same process cannot collide.
fn next_temp_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// A backend that serves **data blob** reads from the body cache and delegates
/// everything else.
///
/// Which reads are body reads is decided by `rustic_core` itself, not guessed:
/// a data blob arrives as `read_partial(FileType::Pack, id, cacheable=false,
/// ..)` because `IndexEntry::read_data` passes `blob_type.is_cacheable()`
/// (`rustic_core-0.12.0 src/index.rs:54-63`), and `BlobType::Data` is the
/// non-cacheable one (`src/blob.rs:54-59`). Tree reads therefore pass straight
/// through to rustic's own metadata cache, and the two caches never hold the
/// same bytes.
pub struct BodyCacheBackend {
    inner: Arc<dyn WriteBackend>,
    cache: Arc<BodyCache>,
}

impl BodyCacheBackend {
    /// Wrap `inner` so its data-blob reads go through `cache`.
    pub fn new(inner: Arc<dyn WriteBackend>, cache: Arc<BodyCache>) -> Self {
        Self { inner, cache }
    }

    /// Wrap `inner`, or hand it back unchanged when there is no cache.
    pub fn wrap(
        inner: Arc<dyn WriteBackend>,
        cache: Option<Arc<BodyCache>>,
    ) -> Arc<dyn WriteBackend> {
        match cache {
            Some(cache) => Arc::new(Self::new(inner, cache)),
            None => inner,
        }
    }

    /// Whether this read is a conversation-body read.
    fn is_body_read(tpe: FileType, cacheable: bool) -> bool {
        tpe == FileType::Pack && !cacheable
    }
}

impl ReadBackend for BodyCacheBackend {
    fn location(&self) -> String {
        self.inner.location()
    }

    fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>> {
        self.inner.list_with_size(tpe)
    }

    fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes> {
        // A whole-pack read is never a body read: it is either a tree pack or
        // an integrity check, and ADR-034 keeps bulk work out of the cache.
        self.inner.read_full(tpe, id)
    }

    fn read_partial(
        &self,
        tpe: FileType,
        id: &Id,
        cacheable: bool,
        offset: u32,
        length: u32,
    ) -> RusticResult<Bytes> {
        if !Self::is_body_read(tpe, cacheable) {
            return self.inner.read_partial(tpe, id, cacheable, offset, length);
        }
        let key = CacheKey::new(id, offset, length);
        if let Some(bytes) = self.cache.get(&key) {
            return Ok(bytes);
        }
        let bytes = self
            .inner
            .read_partial(tpe, id, cacheable, offset, length)?;
        self.cache.put(&key, &bytes);
        Ok(bytes)
    }

    fn warmup_path(&self, tpe: FileType, id: &Id) -> String {
        self.inner.warmup_path(tpe, id)
    }

    fn needs_warm_up(&self) -> bool {
        self.inner.needs_warm_up()
    }

    fn warm_up(&self, tpe: FileType, id: &Id) -> RusticResult<()> {
        self.inner.warm_up(tpe, id)
    }
}

impl WriteBackend for BodyCacheBackend {
    fn create(&self) -> RusticResult<()> {
        self.inner.create()
    }

    fn write_bytes(&self, tpe: FileType, id: &Id, cacheable: bool, buf: Bytes) -> RusticResult<()> {
        self.inner.write_bytes(tpe, id, cacheable, buf)
    }

    fn remove(&self, tpe: FileType, id: &Id, cacheable: bool) -> RusticResult<()> {
        self.inner.remove(tpe, id, cacheable)
    }
}

/// Measure a cache root for `doctor`: the bytes and entries it holds, `None`
/// when there is nothing of this cache's to measure.
///
/// `None` covers both "no directory there yet" and "a directory that is not
/// this cache's", and the caller must ask [`root_state`] which of the two it
/// is: they are different findings, and neither is a measured zero. The second
/// is never measured at all — a directory this cache did not create is not its
/// occupancy, and walking it to add up bytes would be measuring somebody else's
/// disk.
pub fn measure(root: &Path) -> std::io::Result<Option<Usage>> {
    match root_state(root) {
        RootState::Cache => Ok(Some(BodyCache::new(root.to_path_buf(), 0).usage()?)),
        RootState::Absent | RootState::Foreign(_) => Ok(None),
        RootState::Unknown(why) => Err(std::io::Error::other(why)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every cache below is rooted at `<tempdir>/body`, a directory the cache
    // creates (with its marker) on the first store. The temporary directory
    // itself is *not* a valid root — it exists and this cache did not create it
    // — which is the point of `clear_refuses_a_directory_the_cache_did_not_create`.

    /// A pack id that is a function of `n`, so two keys in one test cannot
    /// collide by accident.
    fn key(n: u8, offset: u32, length: u32) -> CacheKey {
        let hex = format!("{:02x}", n.wrapping_mul(7)).repeat(32);
        CacheKey::new(&hex.parse::<Id>().expect("hex id"), offset, length)
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn sizes_parse_with_and_without_units() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("512"), Ok(512));
        assert_eq!(parse_size(" 1048576 "), Ok(1_048_576));
        assert_eq!(parse_size("2kb"), Ok(2_000));
        assert_eq!(parse_size("2MB"), Ok(2_000_000));
        assert_eq!(parse_size("50GB"), Ok(50_000_000_000));
        assert_eq!(parse_size("50 gb"), Ok(50_000_000_000));
        assert_eq!(parse_size("512KiB"), Ok(512 * 1024));
        assert_eq!(parse_size("50GiB"), Ok(50 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1TiB"), Ok(1024 * 1024 * 1024 * 1024));
    }

    #[test]
    fn an_unparsable_size_is_an_error_not_a_default() {
        for bad in ["", "   ", "GB", "fifty", "1.5GB", "10XB", "-5"] {
            assert!(
                parse_size(bad).is_err(),
                "`{bad}` must be refused rather than silently defaulted"
            );
        }
        // The error names the accepted units, so the fix is in the message.
        let message = parse_size("10XB").expect_err("must fail");
        assert!(
            message.contains("kib"),
            "message must list units: {message}"
        );
    }

    #[test]
    fn entry_round_trips_and_a_flipped_byte_is_a_miss() {
        let dir = tempdir();
        let cache = BodyCache::new(dir.path().join("body"), 1 << 20);
        let k = key(1, 0, 16);
        let payload = b"ciphertext bytes";
        cache.put(&k, payload);
        assert_eq!(cache.get(&k).as_deref(), Some(&payload[..]));
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().stored, 1);

        // One flipped bit anywhere in the file must be detected.
        let path = k.path(cache.root());
        let mut raw = fs::read(&path).expect("read entry");
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        fs::write(&path, &raw).expect("write corrupt entry");

        assert_eq!(cache.get(&k), None, "a corrupt entry must not be served");
        assert_eq!(cache.stats().corrupt, 1);
        assert!(
            !path.exists(),
            "a corrupt entry is deleted so the refetch can replace it"
        );

        // The refetch stores a good copy, which the next read serves.
        cache.put(&k, payload);
        assert_eq!(cache.get(&k).as_deref(), Some(&payload[..]));
    }

    #[test]
    fn a_truncated_entry_is_a_miss() {
        let dir = tempdir();
        let cache = BodyCache::new(dir.path().join("body"), 1 << 20);
        let k = key(2, 8, 4);
        cache.put(&k, b"abcd");
        let path = k.path(cache.root());
        let raw = fs::read(&path).expect("read entry");
        fs::write(&path, &raw[..raw.len() - 2]).expect("truncate entry");
        assert_eq!(cache.get(&k), None);
        assert_eq!(cache.stats().corrupt, 1);
    }

    #[test]
    fn a_file_that_is_not_ours_is_a_miss_not_a_payload() {
        let dir = tempdir();
        let cache = BodyCache::new(dir.path().join("body"), 1 << 20);
        let k = key(3, 0, 4);
        let path = k.path(cache.root());
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, b"not a cache entry at all").expect("write");
        assert_eq!(cache.get(&k), None);
        assert_eq!(cache.stats().corrupt, 1);
    }

    #[test]
    fn quota_evicts_the_least_recently_used_entry() {
        let dir = tempdir();
        // Room for three 100-byte payloads and their headers, not four.
        let quota = 3 * (ENTRY_HEADER_LEN as u64 + 100);
        let cache = BodyCache::new(dir.path().join("body"), quota);
        let keys = [key(10, 0, 100), key(11, 0, 100), key(12, 0, 100)];
        for (i, k) in keys.iter().enumerate() {
            cache.put(k, &[b'a' + i as u8; 100]);
            // Distinct, ordered mtimes: the LRU order must be decided by the
            // clock, not by directory iteration order.
            let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + i as u64);
            set_mtime(&k.path(cache.root()), when);
        }
        // Touch the oldest, so entry 10 becomes the most recent.
        assert!(cache.get(&keys[0]).is_some());
        // The touch is the whole of the LRU: a hit that did not move the mtime
        // leaves entry 10 the oldest, and it is then the one evicted. Asserted
        // on its own because such a hit is exactly what Windows did (see
        // [`touch_entry`]), and it would otherwise be reported two asserts down
        // as "the wrong entry was evicted" — the symptom, not the cause.
        let touched = keys[0]
            .path(cache.root())
            .metadata()
            .expect("stat the touched entry")
            .modified()
            .expect("a regular file has an mtime");
        assert!(
            touched > SystemTime::UNIX_EPOCH + Duration::from_secs(1_002),
            "a hit must move the entry's mtime, or eviction is store order and not an LRU"
        );

        cache.put(&key(13, 0, 100), &[b'd'; 100]);
        assert!(cache.usage().expect("usage").bytes <= quota);
        assert!(
            !keys[1].path(cache.root()).exists(),
            "the untouched oldest entry must be the one evicted"
        );
        assert!(
            keys[0].path(cache.root()).exists(),
            "a hit must move an entry out of the LRU position"
        );
        assert!(keys[2].path(cache.root()).exists());
    }

    #[test]
    fn an_entry_larger_than_the_quota_is_never_stored() {
        let dir = tempdir();
        let cache = BodyCache::new(dir.path().join("body"), 100);
        let k = key(20, 0, 200);
        cache.put(&k, &[0u8; 200]);
        assert_eq!(cache.stats().stored, 0);
        assert_eq!(cache.stats().skipped_too_large, 1);
        assert!(!k.path(cache.root()).exists());
    }

    /// A cold cache filled by several threads at once stores everything.
    ///
    /// One `read` calls `put` from several threads — rustic reads bodies in
    /// parallel — so the first store creates the root while other threads are
    /// already asking whether that directory is the cache's. If creating it
    /// were two steps (make the directory, then write the marker), those
    /// threads would see a directory with no marker and refuse the store,
    /// because "a directory this cache did not create" is exactly what that
    /// looks like from outside. So the creation is one step: at that path there
    /// is either nothing, or a directory that already carries the marker.
    #[test]
    fn a_cold_root_filled_by_several_threads_stores_everything() {
        const THREADS: u8 = 8;
        const ROUNDS: u8 = 20;
        let dir = tempdir();
        for round in 0..ROUNDS {
            // A fresh root per round: the race is in the creation, so a single
            // round would only ever exercise it once.
            let cache = Arc::new(BodyCache::new(
                dir.path().join(format!("body-{round}")),
                1 << 20,
            ));
            let start = Arc::new(std::sync::Barrier::new(THREADS as usize));
            let threads: Vec<_> = (0..THREADS)
                .map(|n| {
                    let cache = cache.clone();
                    let start = start.clone();
                    std::thread::spawn(move || {
                        start.wait();
                        cache.put(&key(n.wrapping_add(round), 0, 4), b"aaaa");
                    })
                })
                .collect();
            for thread in threads {
                thread.join().expect("thread must not panic");
            }
            assert_eq!(
                cache.stats().errors,
                0,
                "round {round}: a store was refused, so the root was seen without its marker"
            );
            assert_eq!(
                cache.stats().stored,
                u64::from(THREADS),
                "round {round}: every store must land"
            );
            assert_eq!(root_state(cache.root()), RootState::Cache);
        }
    }

    #[test]
    fn a_session_over_a_tenth_of_the_quota_is_not_stored() {
        let dir = tempdir();
        let cache = BodyCache::new(dir.path().join("body"), 1_000);
        let k = key(30, 0, 10);
        {
            // 101 > 1000 / 10: refused for the whole scope, and the refusal is
            // counted under its own name, not as an error.
            let _scope = cache.declare_session(101);
            cache.put(&k, &[0u8; 10]);
            assert_eq!(cache.stats().stored, 0);
            assert_eq!(cache.stats().skipped_session, 1);
            assert!(!k.path(cache.root()).exists());
        }
        {
            // Exactly a tenth is allowed: the rule is "larger than", and a
            // boundary that depended on the comparison's direction would be a
            // coin flip for whoever sets a quota to match a session size.
            let _scope = cache.declare_session(100);
            cache.put(&k, &[0u8; 10]);
            assert_eq!(cache.stats().stored, 1);
        }
        // Outside the scope the per-entry ceiling is the only limit.
        assert_eq!(cache.stats().skipped_session, 1);
    }

    #[test]
    fn session_scopes_nest_and_restore() {
        let dir = tempdir();
        let cache = BodyCache::new(dir.path().join("body"), 1_000);
        {
            let _outer = cache.declare_session(50);
            assert_eq!(cache.session_bytes.load(Ordering::Relaxed), 50);
            {
                let _inner = cache.declare_session(500);
                assert_eq!(cache.session_bytes.load(Ordering::Relaxed), 500);
            }
            assert_eq!(
                cache.session_bytes.load(Ordering::Relaxed),
                50,
                "the inner scope must restore the outer declaration, not clear it"
            );
        }
        assert_eq!(cache.session_bytes.load(Ordering::Relaxed), NO_SESSION);
    }

    #[test]
    fn clear_removes_entries_and_keeps_the_lock() {
        let dir = tempdir();
        let cache = BodyCache::new(dir.path().join("body"), 1 << 20);
        cache.put(&key(40, 0, 4), b"aaaa");
        cache.put(&key(41, 0, 4), b"bbbb");
        let lock = cache.root().join(LOCK_NAME);
        fs::write(&lock, b"").expect("create lock file");
        assert_eq!(cache.usage().expect("usage").entries, 2);

        let removed = cache.clear().expect("clear");
        assert_eq!(removed.entries, 2);
        assert_eq!(removed.bytes, 2 * (ENTRY_HEADER_LEN as u64 + 4));
        assert_eq!(cache.usage().expect("usage").entries, 0);
        assert!(
            lock.exists(),
            "unlinking the lock while another process holds it would let two \
             processes lock two different inodes"
        );
    }

    /// A directory this cache did not create is refused, loudly, and nothing in
    /// it is touched — a `[cache] dir` with a typo in it must not be emptied.
    #[test]
    fn clear_refuses_a_directory_the_cache_did_not_create() {
        let dir = tempdir();
        let root = dir.path().join("not-a-cache");
        fs::create_dir_all(root.join("sub")).expect("mkdir");
        fs::write(root.join("notes.txt"), b"someone else's file").expect("write");
        fs::write(root.join("sub").join("keep.txt"), b"and another").expect("write");

        let cache = BodyCache::new(root.clone(), 1 << 20);
        let err = cache
            .clear()
            .expect_err("a directory without the cache's own marker must not be cleared");
        assert!(
            err.to_string().contains(".chat-stasher-body-cache"),
            "the refusal must name the marker it looked for, so the fix is in the \
             message: {err}"
        );
        assert!(
            root.join("notes.txt").exists(),
            "an unrelated file was deleted"
        );
        assert!(
            root.join("sub").join("keep.txt").exists(),
            "an unrelated subdirectory was emptied"
        );
        assert!(
            !root.join(".chat-stasher-body-cache").exists(),
            "the refusal must not write the marker: the next run would then treat \
             this directory as the cache's own"
        );
    }

    /// Everything under a cache root that this cache did not write is left
    /// alone: at the root, in a directory of its own, inside one of the cache's
    /// own pack directories, and one level deeper than the layout goes.
    #[test]
    fn clear_leaves_files_the_cache_did_not_write() {
        let dir = tempdir();
        let root = dir.path().join("body");
        let cache = BodyCache::new(root.clone(), 1 << 20);
        // Storing is what creates the root, marker included.
        let k = key(40, 0, 4);
        cache.put(&k, b"aaaa");
        cache.put(&key(41, 0, 4), b"bbbb");
        let pack = k.path(&root).parent().expect("pack dir").to_path_buf();

        fs::write(root.join("notes.txt"), b"someone else's file").expect("write");
        fs::create_dir_all(root.join("sub")).expect("mkdir");
        fs::write(root.join("sub").join("keep.txt"), b"and another").expect("write");
        fs::write(pack.join("stray.txt"), b"not ours either").expect("write");
        // A directory whose name is not a pack id, holding a file named exactly
        // like an entry: only the layout makes a file ours, not the name alone.
        let decoy = root.join("not-a-pack");
        fs::create_dir_all(&decoy).expect("mkdir");
        fs::write(decoy.join("0-4"), b"entry-shaped, not an entry").expect("write");
        // And a pack-id directory one level too deep to be part of the layout.
        let deep = root.join("aabb").join("ccdd");
        fs::create_dir_all(&deep).expect("mkdir");
        fs::write(deep.join("deep.txt"), b"one level too far").expect("write");

        let removed = cache.clear().expect("clear a cache directory it created");
        assert_eq!(
            removed.entries, 2,
            "the two entries are the only things that go"
        );
        for survivor in [
            root.join("notes.txt"),
            root.join("sub").join("keep.txt"),
            pack.join("stray.txt"),
            decoy.join("0-4"),
            deep.join("deep.txt"),
        ] {
            assert!(
                survivor.exists(),
                "{} was deleted, and this cache did not write it",
                survivor.display()
            );
        }
        assert!(!k.path(&root).exists(), "the entries themselves must go");
        assert_eq!(cache.usage().expect("usage").entries, 0);
    }

    /// Quota eviction deletes the same set `clear` does. A file this cache did
    /// not write is not merely left alone — it is not picked as the oldest
    /// thing to evict, which is what would happen if the LRU pass could see it.
    #[test]
    fn eviction_leaves_files_the_cache_did_not_write() {
        let dir = tempdir();
        let quota = 3 * (ENTRY_HEADER_LEN as u64 + 100);
        let cache = BodyCache::new(dir.path().join("body"), quota);
        let keys = [key(70, 0, 100), key(71, 0, 100), key(72, 0, 100)];
        for (i, k) in keys.iter().enumerate() {
            cache.put(k, &[b'a' + i as u8; 100]);
            set_mtime(
                &k.path(cache.root()),
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + i as u64),
            );
        }
        // Both foreign files are older than every entry, so a pass that could
        // see them would delete them first.
        let foreign = cache.root().join("keep-me.txt");
        fs::write(&foreign, b"not a cache entry").expect("write");
        set_mtime(&foreign, SystemTime::UNIX_EPOCH);
        let pack = keys[0].path(cache.root());
        let stray = pack.parent().expect("pack dir").join("stray.txt");
        fs::write(&stray, b"nor this").expect("write");
        set_mtime(&stray, SystemTime::UNIX_EPOCH);

        // One entry more than the quota holds, which is what starts eviction.
        cache.put(&key(73, 0, 100), &[b'd'; 100]);

        assert!(foreign.exists(), "eviction deleted a file it did not write");
        assert!(stray.exists(), "eviction deleted a file it did not write");
        assert!(
            cache.usage().expect("usage").bytes <= quota,
            "the quota must still be met"
        );
        assert!(
            !keys[0].path(cache.root()).exists(),
            "the oldest entry of this cache is what the quota costs"
        );
        assert!(keys[2].path(cache.root()).exists());
    }

    #[test]
    fn usage_counts_entries_and_ignores_transient_files() {
        let dir = tempdir();
        let cache = BodyCache::new(dir.path().join("body"), 1 << 20);
        let k = key(50, 0, 4);
        cache.put(&k, b"aaaa");
        // An in-flight write, where the cache actually puts one: inside the pack
        // directory, beside the entry it is about to become. (A dot-file at the
        // root would be a file this cache never writes there — see
        // `usage_reports_files_the_cache_did_not_write`.)
        let in_flight = k
            .path(cache.root())
            .parent()
            .expect("pack dir")
            .join(format!(".tmp-{}-77", std::process::id()));
        fs::write(&in_flight, b"partial").expect("write temp");
        let usage = cache.usage().expect("usage");
        assert_eq!(usage.entries, 1, "a temp file is not an entry");
        assert!(
            usage.bytes > ENTRY_HEADER_LEN as u64 + 4,
            "but its bytes are on the disk the quota is about"
        );
        assert_eq!(
            usage.foreign_entries, 0,
            "a temp file the cache wrote is not foreign either"
        );
    }

    /// The marker is what makes a directory the cache's own, and the cache
    /// writes it when it creates that directory.
    #[test]
    fn storing_writes_the_marker_that_claims_the_root() {
        let dir = tempdir();
        let cache = BodyCache::new(dir.path().join("body"), 1 << 20);
        assert!(!cache.root().exists(), "nothing is created before a store");

        cache.put(&key(80, 0, 4), b"aaaa");
        let marker = cache.root().join(".chat-stasher-body-cache");
        assert!(
            marker.is_file(),
            "the first store must claim the directory it created"
        );
        let raw = fs::read(&marker).expect("read marker");
        assert!(
            raw.starts_with(b"chat-stasher body cache"),
            "the marker must say what wrote it"
        );
        assert_eq!(root_state(cache.root()), RootState::Cache);
    }

    /// A store into a directory the cache did not create is refused: filling a
    /// directory the user pointed at by mistake is the write-side half of what
    /// `clear` refuses on the delete side.
    #[test]
    fn put_refuses_a_directory_the_cache_did_not_create() {
        let dir = tempdir();
        let root = dir.path().join("someone-elses-dir");
        fs::create_dir_all(&root).expect("mkdir");
        let cache = BodyCache::new(root.clone(), 1 << 20);
        let k = key(90, 0, 4);
        cache.put(&k, b"aaaa");

        assert_eq!(cache.stats().stored, 0, "nothing may be stored there");
        assert_eq!(
            cache.stats().errors,
            1,
            "the refusal is counted, not silent"
        );
        assert!(
            !k.path(&root).exists(),
            "no entry may be written into a directory this cache did not create"
        );
        assert!(
            !root.join(".chat-stasher-body-cache").exists(),
            "and the refusal must not claim the directory"
        );
        assert_eq!(cache.get(&k), None);
    }

    /// Foreign files and directories are counted — so a directory with
    /// something else in it is never described as an empty cache — and their
    /// bytes are never the quota's business.
    #[test]
    fn usage_reports_files_the_cache_did_not_write() {
        let dir = tempdir();
        let cache = BodyCache::new(dir.path().join("body"), 1 << 20);
        let k = key(100, 0, 4);
        cache.put(&k, b"aaaa");
        assert_eq!(cache.usage().expect("usage").foreign_entries, 0);

        fs::write(cache.root().join("notes.txt"), b"not a cache entry").expect("write");
        let sub = cache.root().join("sub");
        fs::create_dir_all(&sub).expect("mkdir");
        // A directory counts as one and is not walked, however much is inside.
        fs::write(sub.join("deep.txt"), b"and another").expect("write");

        let usage = cache.usage().expect("usage");
        assert_eq!(usage.foreign_entries, 2, "a file and a directory");
        assert_eq!(
            usage.bytes,
            ENTRY_HEADER_LEN as u64 + 4,
            "the quota is enforced against this cache's own bytes only: a \
             directory the cache cannot shrink must not cost it its entries"
        );
        assert_eq!(usage.entries, 1);
    }

    #[test]
    fn a_disabled_cache_stores_and_serves_nothing() {
        let dir = tempdir();
        // A subdirectory, so "the cache wrote nothing" is a claim about the
        // cache and not about the temporary directory the test already made.
        let cache = BodyCache::new(dir.path().join("body"), 0);
        let k = key(60, 0, 4);
        cache.put(&k, b"aaaa");
        assert_eq!(cache.get(&k), None);
        assert_eq!(cache.stats().stored, 0);
        assert!(
            !cache.root().exists(),
            "a disabled cache writes no directory"
        );
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open entry to touch");
        file.set_times(FileTimes::new().set_modified(when))
            .expect("set mtime");
    }
}
