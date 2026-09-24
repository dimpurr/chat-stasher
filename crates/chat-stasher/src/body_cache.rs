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
/// an unknown turned into a number.
pub fn settings_for(config: &crate::config::Config) -> Result<Settings> {
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
/// Four states, not two, because "no cache" has three different causes and only
/// one of them is the user's decision. Collapsing them would mean a command
/// that silently ran uncached while the user believed a 50 GB quota was in
/// effect — the same class of mistake as recording an unknown as zero.
#[derive(Debug, Clone)]
pub enum Availability {
    /// The cache is installed for this operation.
    On(Arc<BodyCache>),
    /// `max_bytes = 0`: the user turned it off.
    Off,
    /// This operation is bulk work; ADR-034 keeps those out of the cache.
    Bulk,
    /// The configured location could not be resolved. The read continues
    /// uncached, and the reason is carried so the caller can say so rather than
    /// pretending the cache is simply off.
    Unresolved(String),
}

impl Availability {
    /// The handle to install, or `None` for the three off states.
    pub fn handle(&self) -> Option<Arc<BodyCache>> {
        match self {
            Availability::On(cache) => Some(cache.clone()),
            Availability::Off | Availability::Bulk | Availability::Unresolved(_) => None,
        }
    }
}

/// Resolve the body cache for one operation (ADR-034).
pub fn for_operation(config: &crate::config::Config, policy: Policy) -> Availability {
    if policy == Policy::Bulk {
        return Availability::Bulk;
    }
    match settings_for(config) {
        Ok(settings) if settings.enabled() => Availability::On(Arc::new(settings.open())),
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
    /// Cache I/O failures. Never fatal: the read falls through to the remote.
    pub errors: u64,
}

/// Bytes and entries under a cache root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Sum of file sizes. This is what the quota is enforced against.
    pub bytes: u64,
    /// Number of entry files (temporary files and the lock file are excluded).
    pub entries: usize,
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
            let _removed = fs::remove_file(&path);
            self.corrupt.fetch_add(1, Ordering::Relaxed);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        // LRU is the file's mtime, so a hit has to move it. Best effort: a
        // failed touch costs eviction ordering, never correctness.
        let _touched = file.set_times(FileTimes::new().set_modified(SystemTime::now()));
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
        if write_entry(&key.path(&self.root), payload).is_err() {
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
    /// `bytes` is **every** file under the root, in-flight temporary files and
    /// the lock file included: it is a measurement of the disk, and a number
    /// that excluded a 1 MiB blob because it was mid-rename would be a
    /// measurement of something else. `entries` counts only real entries —
    /// neither a temp file nor the lock file is something a user could be
    /// shown.
    pub fn usage(&self) -> std::io::Result<Usage> {
        let mut usage = Usage::default();
        for entry in scan(&self.root)? {
            usage.bytes += entry.bytes;
            if !entry.transient {
                usage.entries += 1;
            }
        }
        Ok(usage)
    }

    /// Delete every entry. Returns what was removed. The lock file is kept: it
    /// is the one name that must not be unlinked while another process holds
    /// it, or two processes could believe they hold the same lock on two
    /// different inodes.
    pub fn clear(&self) -> std::io::Result<Usage> {
        let mut removed = Usage::default();
        for entry in scan(&self.root)? {
            if entry.transient {
                continue;
            }
            match fs::remove_file(&entry.path) {
                Ok(()) => {
                    removed.bytes += entry.bytes;
                    removed.entries += 1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        remove_empty_dirs(&self.root);
        Ok(removed)
    }

    /// Drop least-recently-used entries until the quota is met.
    ///
    /// Serialized across processes by an advisory lock on `<root>/.lock`, then
    /// re-measured under that lock: another process may have evicted already,
    /// and two processes that each evicted to their own pre-lock measurement
    /// would delete twice as much as needed.
    fn enforce_quota(&self) -> std::io::Result<()> {
        // Cheap check first: the common case is a cache far below its quota,
        // and taking the lock for that would serialize every read.
        if self.usage()?.bytes <= self.max_bytes {
            return Ok(());
        }
        let _guard = self.lock()?;
        loop {
            let entries = scan(&self.root)?;
            let mut total: u64 = entries.iter().map(|e| e.bytes).sum();
            if total <= self.max_bytes {
                return Ok(());
            }
            let now = SystemTime::now();
            let mut youngest_first = entries
                .iter()
                .filter(|e| !e.is_lock && (!e.transient || is_stale(e, now)))
                .collect::<Vec<_>>();
            youngest_first.sort_by_key(|e| (e.modified, e.path.clone()));
            let mut deleted_any = false;
            for entry in youngest_first {
                if total <= self.max_bytes {
                    break;
                }
                match fs::remove_file(&entry.path) {
                    Ok(()) => {
                        total = total.saturating_sub(entry.bytes);
                        deleted_any = true;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        total = total.saturating_sub(entry.bytes);
                    }
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
        fs::create_dir_all(&self.root)?;
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
    /// A temporary file still being written, or the lock file: counted in the
    /// total but not an entry a user could be shown.
    transient: bool,
    is_lock: bool,
}

fn is_stale(entry: &ScannedEntry, now: SystemTime) -> bool {
    match now.duration_since(entry.modified) {
        Ok(age) => age > STALE_TEMP,
        // A modification time in the future is not a reason to delete someone's
        // in-progress write.
        Err(_) => false,
    }
}

/// Every file under `root`, recursively. Symlinks are not followed (a symlink
/// out of the tree would let `clear` or LRU delete something outside the
/// cache), and a directory that cannot be read is skipped rather than fatal.
fn scan(root: &Path) -> std::io::Result<Vec<ScannedEntry>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let reader = match fs::read_dir(&dir) {
            Ok(reader) => reader,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in reader {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let metadata = entry.metadata()?;
            let transient = name.starts_with('.');
            out.push(ScannedEntry {
                path,
                bytes: metadata.len(),
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                transient,
                is_lock: name == LOCK_NAME,
            });
        }
    }
    Ok(out)
}

/// Remove directories left empty by eviction or `clear`. Best effort: a
/// directory that is not empty (a concurrent writer just created something in
/// it) simply stays.
fn remove_empty_dirs(root: &Path) {
    let mut dirs = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(reader) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in reader.flatten() {
            // An entry whose type cannot be read is skipped: this pass only
            // removes empty directories, so the only consequence of skipping
            // one is that it stays. Treating "could not stat" as "is a
            // directory" would instead try to remove something we know nothing
            // about.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                let path = entry.path();
                stack.push(path.clone());
                dirs.push(path);
            }
        }
    }
    // Deepest first, so a parent that becomes empty is removed in the same pass.
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    for dir in dirs {
        let _removed = fs::remove_dir(&dir);
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

/// Write one entry: temp file in the target directory, then rename into place.
///
/// The rename is what makes this safe without a lock: a reader either sees the
/// old file or the new one, never a half-written one, and two processes storing
/// the same key write identical bytes.
fn write_entry(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "entry path has no parent")
    })?;
    fs::create_dir_all(parent)?;
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
/// when the directory does not exist (which is *unmeasured*, not zero).
pub fn measure(root: &Path) -> std::io::Result<Option<Usage>> {
    if !root.exists() {
        return Ok(None);
    }
    Ok(Some(BodyCache::new(root.to_path_buf(), 0).usage()?))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let cache = BodyCache::new(dir.path().to_path_buf(), 1 << 20);
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
        let cache = BodyCache::new(dir.path().to_path_buf(), 1 << 20);
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
        let cache = BodyCache::new(dir.path().to_path_buf(), 1 << 20);
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
        let cache = BodyCache::new(dir.path().to_path_buf(), quota);
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
        let cache = BodyCache::new(dir.path().to_path_buf(), 100);
        let k = key(20, 0, 200);
        cache.put(&k, &[0u8; 200]);
        assert_eq!(cache.stats().stored, 0);
        assert_eq!(cache.stats().skipped_too_large, 1);
        assert!(!k.path(cache.root()).exists());
    }

    #[test]
    fn a_session_over_a_tenth_of_the_quota_is_not_stored() {
        let dir = tempdir();
        let cache = BodyCache::new(dir.path().to_path_buf(), 1_000);
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
        let cache = BodyCache::new(dir.path().to_path_buf(), 1_000);
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
        let cache = BodyCache::new(dir.path().to_path_buf(), 1 << 20);
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

    #[test]
    fn usage_counts_entries_and_ignores_transient_files() {
        let dir = tempdir();
        let cache = BodyCache::new(dir.path().to_path_buf(), 1 << 20);
        cache.put(&key(50, 0, 4), b"aaaa");
        let transient = cache.root().join(".tmp-in-flight");
        fs::write(&transient, b"partial").expect("write temp");
        let usage = cache.usage().expect("usage");
        assert_eq!(usage.entries, 1, "a temp file is not an entry");
        assert!(
            usage.bytes > ENTRY_HEADER_LEN as u64 + 4,
            "but its bytes are on the disk the quota is about"
        );
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
