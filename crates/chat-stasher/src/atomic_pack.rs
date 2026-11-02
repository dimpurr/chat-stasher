//! Atomic pack publication and the by-name sweep of its debris (ADR-063,
//! ISSUE-006 step C).
//!
//! A push can be cut at any moment. On the SFTP backend a pack is written to
//! its *final* path, so a cut mid-pack left a torn file wearing a real name
//! that no reader could ever adopt and no append-only repository could ever
//! delete. This module is the fix, and it lives in the wrapper layer
//! (`Store::backends()`), never in rustic: for pack uploads on the SFTP and
//! local-directory families the candidate is
//!
//! 1. written under a tmp name `<id>.tmp-<unix seconds>-<16 hex>` next to the
//!    final name (D1, ruling 4 — the random part comes from the OS),
//! 2. read back **from the backend, bypassing every cache**, and hashed: the
//!    SHA-256 must equal the id the pack is stored under (ruling 1 — no
//!    cheaper mode exists),
//! 3. renamed to the final name. Rename is the act of publication: rustic and
//!    every reader only ever see final names.
//!
//! SFTP v3's rename fails when the target exists (M2), so an existing final
//! name is settled by hash first: byte-identical ⇒ delete only our own tmp and
//! count the publish a success; different ⇒ touch nothing, stop the push.
//!
//! Debris is cleaned **by name** ([`sweep`]): a tmp name can never be part of
//! the published namespace, so deleting one needs no reachability argument. It
//! runs only on a push connection (ruling 2), looks at both the timestamp in
//! the name and the backend mtime and deletes only when *both* are older than
//! seven days (ruling 5; either unreadable ⇒ keep), and also covers the two
//! older spellings (ruling 3): rustic's local `<id>-tmp-` and the
//! `.chat-stasher-tmp` staging directory of the `opendal:fs` default.
//! Read-only commands call [`survey`], which only counts.
//!
//! Where the tmp sits (D1a / M1): `data/<xx>/`, next to the final name. That
//! placement is gated by the lister fixture in
//! `tests/w1055_atomic_pack_test.rs`: rustic's listers parse each file name as
//! an id and ignore what is not one; if that fixture ever fails, the tmp moves
//! to a same-filesystem directory outside `data/` and this paragraph says so.

use anyhow::{anyhow, Context};
use bytes::Bytes;
use rustic_core::{ErrorKind, FileType, Id, ReadBackend, RusticError, RusticResult, WriteBackend};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Seven days: how old tmp debris must be, by name *and* by backend mtime,
/// before a push connection deletes it (ruling 5).
pub const SWEEP_AGE_SECS: u64 = 7 * 24 * 3600;

/// The `opendal:fs` staging directory older chat-stasher versions defaulted
/// `atomic_write_dir` to (ruling 3: inventoried and aged by name).
pub const LEGACY_STAGING_DIR: &str = ".chat-stasher-tmp";

/// The directories a tmp spelling can sit in.
const SURVEY_DIRS: [&str; 6] = [
    "data/",
    "index/",
    "snapshots/",
    "keys/",
    "locks/",
    ".chat-stasher-tmp/",
];

/// The one runtime the blocking operators run on. A pack write is rare and
/// large, so one small pool is plenty; it is built on first use only.
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        // A process that cannot start two threads cannot push at all.
        .expect("atomic-pack runtime")
});

/// One file the backend lists: path relative to the repository root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub bytes: u64,
    /// Backend mtime in unix seconds; `None` when the backend did not say.
    pub mtime: Option<u64>,
}

/// The five operations publication and sweeping need, over a repository root.
/// One implementation drives real storage ([`OperatorIo`]); tests drive the
/// same logic through fault-injecting ones.
pub trait PackIo: Send + Sync {
    fn write(&self, path: &str, bytes: Bytes) -> anyhow::Result<()>;
    fn read(&self, path: &str) -> anyhow::Result<Bytes>;
    fn exists(&self, path: &str) -> anyhow::Result<bool>;
    fn rename(&self, from: &str, to: &str) -> anyhow::Result<()>;
    fn delete(&self, path: &str) -> anyhow::Result<()>;
    /// Every file under `dir` (recursive). A missing directory is empty.
    fn list_files(&self, dir: &str) -> anyhow::Result<Vec<FileEntry>>;
}

/// rustic's default for the `retry` option (`rustic_backend` opendal.rs).
const DEFAULT_RETRY: usize = 5;

/// Words for an opendal failure that say what happened to a person, instead of
/// the bare `Unexpected` kind. No path, host or account is ever included.
fn failure_words(stage: &str, error: &opendal::Error) -> anyhow::Error {
    use opendal::ErrorKind as K;
    let cause = match error.kind() {
        K::NotFound => "a directory or file the destination should hold was missing",
        K::PermissionDenied => "the destination refused permission",
        K::RateLimited => "the destination asked us to slow down",
        K::ConfigInvalid => "the destination settings are not valid",
        _ => "the connection to the destination failed or was interrupted",
    };
    let transient = if error.is_temporary() {
        " (it looks transient)"
    } else {
        ""
    };
    anyhow!("{stage} failed: {cause}{transient}")
}

/// [`PackIo`] over an opendal operator (`fs` for the local-directory family,
/// `sftp` for the remote one).
pub struct OperatorIo {
    op: opendal::blocking::Operator,
}

impl OperatorIo {
    /// Build an operator for `scheme` from the same options rustic gets.
    pub fn open(scheme: &str, options: BTreeMap<String, String>) -> anyhow::Result<Self> {
        // The same `retry` option rustic's own backend reads (`false`/`off`
        // = none, absent/`default` = 5, or a number), so the writer survives
        // the transient errors the main connection already survived (W1055
        // review M2).
        let retries = match options.get("retry").map(String::as_str) {
            Some("false" | "off") => 0,
            None | Some("default") => DEFAULT_RETRY,
            Some(number) => number.parse::<usize>().map_err(|_| {
                anyhow!("the `retry` option must be `false`, `default` or a whole number")
            })?,
        };
        let _guard = RUNTIME.enter();
        let operator = opendal::Operator::via_iter(scheme, options)
            .map_err(|_| anyhow!("could not open the destination for an atomic pack write"))?
            .layer(
                opendal::layers::RetryLayer::new()
                    .with_max_times(retries)
                    .with_jitter(),
            );
        let op = opendal::blocking::Operator::new(operator)
            .map_err(|_| anyhow!("could not start the atomic pack writer"))?;
        Ok(Self { op })
    }
}

fn unix_secs(ts: opendal::raw::Timestamp) -> Option<u64> {
    SystemTime::from(ts)
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn not_found(error: &opendal::Error) -> bool {
    error.kind() == opendal::ErrorKind::NotFound
}

impl PackIo for OperatorIo {
    fn write(&self, path: &str, bytes: Bytes) -> anyhow::Result<()> {
        self.op
            .write(path, bytes)
            .map(|_| ())
            .map_err(|e| failure_words("writing the temporary file", &e))
    }

    fn read(&self, path: &str) -> anyhow::Result<Bytes> {
        self.op
            .read(path)
            .map(|buffer| buffer.to_bytes())
            .map_err(|e| failure_words("reading the temporary file back", &e))
    }

    fn exists(&self, path: &str) -> anyhow::Result<bool> {
        self.op
            .exists(path)
            .map_err(|e| failure_words("checking whether the pack already exists", &e))
    }

    fn rename(&self, from: &str, to: &str) -> anyhow::Result<()> {
        self.op
            .rename(from, to)
            .map_err(|e| failure_words("renaming the temporary file into place", &e))
    }

    fn delete(&self, path: &str) -> anyhow::Result<()> {
        self.op
            .delete(path)
            .map_err(|e| failure_words("deleting a file", &e))
    }

    fn list_files(&self, dir: &str) -> anyhow::Result<Vec<FileEntry>> {
        let options = opendal::options::ListOptions {
            recursive: true,
            ..Default::default()
        };
        let entries = match self.op.list_options(dir, options) {
            Ok(entries) => entries,
            Err(e) if not_found(&e) => return Ok(Vec::new()),
            Err(e) => return Err(failure_words("listing the destination", &e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let meta = entry.metadata();
            if !meta.is_file() {
                continue;
            }
            let path = entry.path().trim_start_matches('/').to_string();
            // Some services omit mtime from a listing; ask once per file that
            // turns out to be a tmp candidate is the caller's business, so a
            // plain stat here would cost a round trip per pack. Only names
            // that look like tmp debris get one (see `classify`).
            let mut mtime = meta.last_modified().and_then(unix_secs);
            let mut bytes = meta.content_length();
            if mtime.is_none() && classify(&path).is_some() {
                if let Ok(stat) = self.op.stat(&path) {
                    mtime = stat.last_modified().and_then(unix_secs);
                    bytes = stat.content_length();
                }
            }
            out.push(FileEntry { path, bytes, mtime });
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- names ----

/// Which spelling of tmp debris a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TmpKind {
    /// `<id>.tmp-<unix>-<16 hex>`: this module's own (ADR-063 D1).
    Current,
    /// `<id>-tmp-`: rustic's local backend, killed between write and rename.
    LegacyLocal,
    /// A file inside the old `.chat-stasher-tmp` staging directory.
    LegacyStaging,
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The tmp name for `id_hex` started at `now` with random suffix `random`.
#[must_use]
pub fn tmp_name(id_hex: &str, now: u64, random: [u8; 8]) -> String {
    let mut suffix = String::with_capacity(16);
    for byte in random {
        suffix.push_str(&format!("{byte:02x}"));
    }
    format!("{id_hex}.tmp-{now}-{suffix}")
}

/// Parse a current-format tmp file name into `(id hex, unix seconds)`.
#[must_use]
pub fn parse_tmp_name(name: &str) -> Option<(&str, u64)> {
    let (id, rest) = name.split_once(".tmp-")?;
    let (secs, random) = rest.split_once('-')?;
    if !is_hex(id, 64) || !is_hex(random, 16) || secs.is_empty() {
        return None;
    }
    if !secs.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((id, secs.parse().ok()?))
}

/// Classify a repository-relative path as tmp debris, with the timestamp its
/// name carries (current format only). `None` is any other file: **never**
/// touched by this module.
#[must_use]
pub fn classify(path: &str) -> Option<(TmpKind, Option<u64>)> {
    if path.starts_with(LEGACY_STAGING_DIR) && path[LEGACY_STAGING_DIR.len()..].starts_with('/') {
        return Some((TmpKind::LegacyStaging, None));
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if let Some((_, secs)) = parse_tmp_name(name) {
        return Some((TmpKind::Current, Some(secs)));
    }
    if let Some(id) = name.strip_suffix("-tmp-") {
        if is_hex(id, 64) {
            return Some((TmpKind::LegacyLocal, None));
        }
    }
    None
}

// ----------------------------------------------------------- publication ----

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The final path of a pack.
fn final_path(id_hex: &str) -> String {
    format!("data/{}/{}", &id_hex[..2], id_hex)
}

/// What a successful publication did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// Verified, then renamed into place.
    Renamed,
    /// The final name already held byte-identical content (M2): only our own
    /// tmp was removed.
    AlreadyPresent,
}

/// Why a publication stopped.
#[derive(Debug)]
pub enum PublishError {
    /// An operation failed. Nothing was published under the final name.
    Io(anyhow::Error),
    /// The read-back did not hash to the id. The tmp was removed.
    VerifyMismatch,
    /// The final name exists with different bytes (M2). Nothing was touched;
    /// our verified tmp stays for a human to compare.
    Collision,
}

impl PublishError {
    fn describe(&self) -> String {
        match self {
            Self::Io(error) => format!("{error:#}"),
            Self::VerifyMismatch => "the pack read back from the destination did not hash to \
                 its id, so it was not published"
                .to_string(),
            Self::Collision => "atomic pack publication stopped: a file already holds this \
                 pack's name with different bytes; it was not touched. Someone has written a \
                 formal pack name wrong; a person needs to look"
                .to_string(),
        }
    }
}

/// Publish one pack: tmp, verify, rename (ADR-063 D1). `now` and `random`
/// are parameters so tests can name the tmp.
///
/// Whatever the failure, nothing is ever written under the final name except
/// by the final rename, which only follows a passed verification.
pub fn publish(
    io: &dyn PackIo,
    id_hex: &str,
    bytes: Bytes,
    now: u64,
    random: [u8; 8],
) -> Result<Published, PublishError> {
    publish_to(io, id_hex, bytes, &tmp_path(id_hex, now, random))
}

/// The repository-relative path of a pack's tmp file.
fn tmp_path(id_hex: &str, now: u64, random: [u8; 8]) -> String {
    format!("data/{}/{}", &id_hex[..2], tmp_name(id_hex, now, random))
}

fn publish_to(
    io: &dyn PackIo,
    id_hex: &str,
    bytes: Bytes,
    tmp: &str,
) -> Result<Published, PublishError> {
    let final_path = final_path(id_hex);
    let tmp = tmp.to_string();

    if let Err(error) = io.write(&tmp, bytes) {
        // Best effort only: a failed write may have left part of the tmp,
        // which is exactly what the 7-day sweep exists for.
        drop(io.delete(&tmp));
        return Err(PublishError::Io(error));
    }
    // Ruling 1: read back from the backend itself and hash to the id.
    let read_back = match io.read(&tmp) {
        Ok(read_back) => read_back,
        Err(error) => return Err(PublishError::Io(error)),
    };
    if sha256_hex(&read_back) != id_hex {
        drop(io.delete(&tmp));
        return Err(PublishError::VerifyMismatch);
    }
    drop(read_back);

    let target_exists = io.exists(&final_path).map_err(PublishError::Io)?;
    if !target_exists {
        match io.rename(&tmp, &final_path) {
            Ok(()) => return Ok(Published::Renamed),
            Err(error) => {
                // SFTP v3 refuses a rename over an existing name: a writer
                // that raced us may have just published the same id.
                // An existence check that itself fails is an unknown, not a
                // "no": the rename's own error is what gets reported.
                if !matches!(io.exists(&final_path), Ok(true)) {
                    return Err(PublishError::Io(error));
                }
            }
        }
    }
    settle_collision(io, &final_path, id_hex, &tmp)
}

/// M2: the final name exists. Equal hash ⇒ remove our tmp, success;
/// different ⇒ stop and touch nothing.
fn settle_collision(
    io: &dyn PackIo,
    final_path: &str,
    id_hex: &str,
    tmp: &str,
) -> Result<Published, PublishError> {
    let existing = io.read(final_path).map_err(PublishError::Io)?;
    if sha256_hex(&existing) == id_hex {
        io.delete(tmp).map_err(PublishError::Io)?;
        Ok(Published::AlreadyPresent)
    } else {
        Err(PublishError::Collision)
    }
}

fn os_random() -> anyhow::Result<[u8; 8]> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|_| anyhow!("the operating system gave no randomness"))?;
    Ok(random)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// -------------------------------------------------------------- backend ----

type IoFactory = Box<dyn Fn() -> anyhow::Result<Arc<dyn PackIo>> + Send + Sync>;

/// Wraps rustic's backend so a *pack* write becomes [`publish`]. Everything
/// else, and every read, is delegated untouched.
pub struct AtomicPackBackend {
    inner: Arc<dyn WriteBackend>,
    factory: IoFactory,
    io: Mutex<Option<Arc<dyn PackIo>>>,
    policy: RetryPolicy,
    /// Tmp files this process created, failed to remove after a failed
    /// attempt (the destination was unreachable even on a fresh connection),
    /// and will try to remove again on the next attempt that gets through.
    pending: Mutex<Vec<String>>,
}

/// How a failed pack upload is retried (W1055 review M2): each retry is a
/// whole new attempt (new tmp name) on a fresh connection, after the failed
/// attempt's tmp was removed.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts, the first included.
    pub attempts: usize,
    /// Pause before each further attempt.
    pub backoff: std::time::Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 2,
            backoff: std::time::Duration::from_secs(2),
        }
    }
}

impl std::fmt::Debug for AtomicPackBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AtomicPackBackend")
    }
}

impl AtomicPackBackend {
    /// Wrap `inner`, building the writer lazily from `factory` on the first
    /// pack write: a push that writes no pack, and every read command, never
    /// opens a second connection.
    pub fn around(inner: Arc<dyn WriteBackend>, factory: IoFactory) -> Arc<dyn WriteBackend> {
        Self::around_with_policy(inner, factory, RetryPolicy::default())
    }

    /// [`Self::around`] with an explicit retry policy.
    pub fn around_with_policy(
        inner: Arc<dyn WriteBackend>,
        factory: IoFactory,
        policy: RetryPolicy,
    ) -> Arc<dyn WriteBackend> {
        Arc::new(Self {
            inner,
            factory,
            io: Mutex::new(None),
            policy,
            pending: Mutex::new(Vec::new()),
        })
    }

    /// Wrap `inner` for `cfg`'s destination, or hand it back when the
    /// destination is not one of the two covered families (S3/R2 are atomic
    /// already: D2).
    pub fn wrap(
        inner: Arc<dyn WriteBackend>,
        cfg: &crate::store::StoreConfig,
    ) -> Arc<dyn WriteBackend> {
        let Some((scheme, options)) = cfg.pack_publication_target() else {
            return inner;
        };
        Self::around(
            inner,
            Box::new(move || {
                let io: Arc<dyn PackIo> = Arc::new(OperatorIo::open(&scheme, options.clone())?);
                Ok(io)
            }),
        )
    }

    fn io(&self) -> anyhow::Result<Arc<dyn PackIo>> {
        let mut slot = self
            .io
            .lock()
            .map_err(|_| anyhow!("the atomic pack writer lock was poisoned"))?;
        if let Some(io) = slot.as_ref() {
            return Ok(Arc::clone(io));
        }
        let io = (self.factory)()?;
        *slot = Some(Arc::clone(&io));
        Ok(io)
    }

    /// Drop the cached connection and open a fresh one, remembering it.
    fn fresh_io(&self) -> anyhow::Result<Arc<dyn PackIo>> {
        let mut slot = self
            .io
            .lock()
            .map_err(|_| anyhow!("the atomic pack writer lock was poisoned"))?;
        *slot = None;
        let io = (self.factory)()?;
        *slot = Some(Arc::clone(&io));
        Ok(io)
    }

    /// Best-effort removal of `tmp` (and anything an earlier failure could
    /// not remove) on a fresh connection. Returns how many files remain.
    fn clean_up(&self, tmp: &str) -> usize {
        // A poisoned lock still holds the list; losing it would forget files.
        let mut wanted: Vec<String> = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        wanted.push(tmp.to_string());
        let mut left = Vec::new();
        match self.fresh_io() {
            Ok(io) => {
                for path in wanted {
                    if io.delete(&path).is_err() {
                        left.push(path);
                    }
                }
            }
            Err(_) => left = wanted,
        }
        let count = left.len();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(left);
        count
    }

    fn publish_with_retry(&self, id_hex: &str, bytes: &Bytes) -> Result<(), String> {
        let attempts = self.policy.attempts.max(1);
        let mut io = self.io().map_err(|e| unreachable_words(&e))?;
        let mut last = String::new();
        let mut left = 0;
        for attempt in 1..=attempts {
            let random = os_random().map_err(|e| format!("{e:#}"))?;
            let tmp = tmp_path(id_hex, unix_now(), random);
            match publish_to(&*io, id_hex, bytes.clone(), &tmp) {
                Ok(_) => {
                    // A connection that works again can also finish earlier
                    // cleanup.
                    if self.pending.lock().is_ok_and(|p| !p.is_empty()) {
                        self.clean_up(&tmp);
                    }
                    return Ok(());
                }
                Err(PublishError::Collision) => return Err(PublishError::Collision.describe()),
                Err(failure) => {
                    last = failure.describe();
                    left = self.clean_up(&tmp);
                    if attempt < attempts {
                        std::thread::sleep(self.policy.backoff);
                        match self.io() {
                            Ok(fresh) => io = fresh,
                            Err(e) => {
                                last = unreachable_words(&e);
                                break;
                            }
                        }
                    }
                }
            }
        }
        let cleanup = if left == 0 {
            "the temporary files the failed attempt(s) left were removed".to_string()
        } else {
            format!(
                "{left} temporary file(s) could not be removed right now because the destination \
                 was unreachable; a later push removes them (after 7 days at the latest)"
            )
        };
        Err(format!(
            "uploading a pack to the destination failed after {attempts} attempt(s): {last}. \
             Nothing was published under a final name, and {cleanup}. The next push tries again"
        ))
    }
}

fn unreachable_words(error: &anyhow::Error) -> String {
    format!("the destination could not be reached for the pack upload ({error:#})")
}

fn backend_error(text: String) -> Box<RusticError> {
    RusticError::new(ErrorKind::Backend, text)
}

impl ReadBackend for AtomicPackBackend {
    fn location(&self) -> String {
        self.inner.location()
    }

    fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>> {
        self.inner.list_with_size(tpe)
    }

    fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes> {
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
        self.inner.read_partial(tpe, id, cacheable, offset, length)
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

impl WriteBackend for AtomicPackBackend {
    fn create(&self) -> RusticResult<()> {
        self.inner.create()
    }

    fn write_bytes(&self, tpe: FileType, id: &Id, cacheable: bool, buf: Bytes) -> RusticResult<()> {
        if tpe != FileType::Pack {
            return self.inner.write_bytes(tpe, id, cacheable, buf);
        }
        self.publish_with_retry(&id.to_hex(), &buf)
            .map_err(backend_error)
    }

    fn remove(&self, tpe: FileType, id: &Id, cacheable: bool) -> RusticResult<()> {
        self.inner.remove(tpe, id, cacheable)
    }
}

// ----------------------------------------------------------------- sweep ----

/// One tmp-debris file and what is known about its age.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmpEntry {
    pub kind: TmpKind,
    pub path: String,
    pub bytes: u64,
    /// The unix seconds in the name (current format only).
    pub name_secs: Option<u64>,
    pub mtime: Option<u64>,
}

/// Whether the by-name sweep may delete an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Age {
    /// Older than seven days by every clock the spelling carries.
    Due,
    /// Younger than seven days by at least one clock.
    Young,
    /// A clock the spelling needs could not be read: never deleted (ruling 5).
    Unknown,
}

impl TmpEntry {
    /// Ruling 5: the current format needs the name's timestamp **and** the
    /// backend mtime, both older than seven days. The two older spellings
    /// carry no timestamp, so the backend mtime is their only clock.
    #[must_use]
    pub fn age(&self, now: u64) -> Age {
        let old = |secs: u64| now.saturating_sub(secs) > SWEEP_AGE_SECS;
        match (self.kind, self.name_secs, self.mtime) {
            (TmpKind::Current, Some(name), Some(mtime)) => {
                if old(name) && old(mtime) {
                    Age::Due
                } else {
                    Age::Young
                }
            }
            (TmpKind::Current, _, _) => Age::Unknown,
            (_, _, Some(mtime)) => {
                if old(mtime) {
                    Age::Due
                } else {
                    Age::Young
                }
            }
            (_, _, None) => Age::Unknown,
        }
    }
}

/// The read-only inventory of tmp debris: what doctor and `prune-orphans`
/// report (ruling 6) and what [`sweep`] acts on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TmpSurvey {
    pub entries: Vec<TmpEntry>,
    /// Directories that could not be listed: the survey is incomplete there,
    /// which is an unknown and never a zero.
    pub unlisted: usize,
}

impl TmpSurvey {
    #[must_use]
    pub fn files(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.bytes).sum()
    }

    /// Entries a push connection would delete now.
    #[must_use]
    pub fn due(&self, now: u64) -> usize {
        self.entries
            .iter()
            .filter(|e| e.age(now) == Age::Due)
            .count()
    }

    /// Whether there is anything a surface owes the reader. A healthy
    /// repository is silent; an unlisted directory is not silence.
    #[must_use]
    pub fn has_anything_to_say(&self) -> bool {
        !self.entries.is_empty() || self.unlisted > 0
    }
}

/// List the tmp debris under `io`'s root. Read-only.
pub fn survey(io: &dyn PackIo) -> TmpSurvey {
    let mut out = TmpSurvey::default();
    for dir in SURVEY_DIRS {
        match io.list_files(dir) {
            Ok(files) => {
                for file in files {
                    if let Some((kind, name_secs)) = classify(&file.path) {
                        out.entries.push(TmpEntry {
                            kind,
                            path: file.path,
                            bytes: file.bytes,
                            name_secs,
                            mtime: file.mtime,
                        });
                    }
                }
            }
            Err(_) => out.unlisted += 1,
        }
    }
    out.entries.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub removed: usize,
    pub removed_bytes: u64,
    pub kept_young: usize,
    pub kept_unknown_age: usize,
    pub failed: usize,
    pub unlisted: usize,
}

/// Delete tmp debris that is due. **Push connections only** (ruling 2): this
/// is called from the push path and nowhere else. A failure to delete one
/// file never stops the others and never fails the push.
pub fn sweep(io: &dyn PackIo, now: u64) -> SweepReport {
    let found = survey(io);
    let mut report = SweepReport {
        unlisted: found.unlisted,
        ..SweepReport::default()
    };
    for entry in &found.entries {
        match entry.age(now) {
            Age::Due => match io.delete(&entry.path) {
                Ok(()) => {
                    report.removed += 1;
                    report.removed_bytes += entry.bytes;
                }
                Err(_) => report.failed += 1,
            },
            Age::Young => report.kept_young += 1,
            Age::Unknown => report.kept_unknown_age += 1,
        }
    }
    report
}

/// The covered destination's [`PackIo`], or `None` for a family this module
/// leaves alone (S3/R2, rest).
pub fn io_for_config(cfg: &crate::store::StoreConfig) -> anyhow::Result<Option<Arc<dyn PackIo>>> {
    let Some((scheme, options)) = cfg.pack_publication_target() else {
        return Ok(None);
    };
    let io: Arc<dyn PackIo> =
        Arc::new(OperatorIo::open(&scheme, options).context("open the destination")?);
    Ok(Some(io))
}

/// Run the sweep for a push on `cfg`'s destination. `None`: the destination
/// is not a covered family. Errors opening the destination are returned for
/// the caller to word; they must not fail the push.
pub fn sweep_for_config(cfg: &crate::store::StoreConfig) -> anyhow::Result<Option<SweepReport>> {
    Ok(io_for_config(cfg)?.map(|io| sweep(&*io, unix_now())))
}

/// The read-only survey for `cfg`'s destination (doctor, `prune-orphans`).
pub fn survey_for_config(cfg: &crate::store::StoreConfig) -> anyhow::Result<Option<TmpSurvey>> {
    Ok(io_for_config(cfg)?.map(|io| survey(&*io)))
}

/// Current unix seconds, for surfaces that need to ask [`TmpSurvey::due`].
#[must_use]
pub fn now_secs() -> u64 {
    unix_now()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "ab00000000000000000000000000000000000000000000000000000000000001";

    #[test]
    fn a_tmp_name_round_trips_and_nothing_else_parses() {
        let name = tmp_name(ID, 1_700_000_000, [0xde, 0xad, 0xbe, 0xef, 1, 2, 3, 4]);
        assert_eq!(name, format!("{ID}.tmp-1700000000-deadbeef01020304"));
        assert_eq!(parse_tmp_name(&name), Some((ID, 1_700_000_000)));
        // Not tmp names: the id alone, a short random part, a non-id prefix.
        assert_eq!(parse_tmp_name(ID), None);
        assert_eq!(parse_tmp_name(&format!("{ID}.tmp-17-dead")), None);
        assert_eq!(parse_tmp_name("zz.tmp-17-deadbeef01020304"), None);
        assert_eq!(parse_tmp_name(&format!("{ID}.tmp--deadbeef01020304")), None);
    }

    #[test]
    fn only_the_three_spellings_classify_and_a_real_pack_never_does() {
        assert_eq!(classify(&format!("data/ab/{ID}")), None);
        assert_eq!(classify("config"), None);
        assert_eq!(classify("index/notes.txt"), None);
        assert_eq!(
            classify(&format!("data/ab/{ID}.tmp-5-0011223344556677")),
            Some((TmpKind::Current, Some(5)))
        );
        assert_eq!(
            classify(&format!("index/ab/{ID}-tmp-")),
            Some((TmpKind::LegacyLocal, None))
        );
        assert_eq!(
            classify(".chat-stasher-tmp/anything"),
            Some((TmpKind::LegacyStaging, None))
        );
        // A neighbour directory that merely starts with the staging name.
        assert_eq!(classify(".chat-stasher-tmp-other/x"), None);
    }

    fn entry(kind: TmpKind, name_secs: Option<u64>, mtime: Option<u64>) -> TmpEntry {
        TmpEntry {
            kind,
            path: "p".into(),
            bytes: 1,
            name_secs,
            mtime,
        }
    }

    #[test]
    fn age_needs_both_clocks_for_the_current_format_and_unreadable_never_deletes() {
        let now = 10_000_000;
        let old = now - SWEEP_AGE_SECS - 1;
        let edge = now - SWEEP_AGE_SECS;
        let young = now - 60;
        let current = |n, m| entry(TmpKind::Current, n, m).age(now);
        assert_eq!(current(Some(old), Some(old)), Age::Due);
        // Exactly seven days is not yet older than seven days.
        assert_eq!(current(Some(edge), Some(edge)), Age::Young);
        // The conservative clock wins in both directions.
        assert_eq!(current(Some(old), Some(young)), Age::Young);
        assert_eq!(current(Some(young), Some(old)), Age::Young);
        // Either clock unreadable: keep, and say so.
        assert_eq!(current(Some(old), None), Age::Unknown);
        assert_eq!(current(None, Some(old)), Age::Unknown);
        // A name stamped in the future (skewed clock) is young, not due.
        assert_eq!(current(Some(now + 9_999), Some(old)), Age::Young);
        // The older spellings carry no stamp: the backend mtime alone.
        assert_eq!(
            entry(TmpKind::LegacyLocal, None, Some(old)).age(now),
            Age::Due
        );
        assert_eq!(
            entry(TmpKind::LegacyStaging, None, Some(young)).age(now),
            Age::Young
        );
        assert_eq!(
            entry(TmpKind::LegacyLocal, None, None).age(now),
            Age::Unknown
        );
    }

    #[test]
    fn a_bare_opendal_kind_never_reaches_the_user() {
        let error = opendal::Error::new(opendal::ErrorKind::Unexpected, "secret-host path");
        let words = failure_words("writing the temporary file", &error).to_string();
        assert!(
            words.starts_with("writing the temporary file failed: "),
            "{words}"
        );
        assert!(words.contains("failed or was interrupted"), "{words}");
        assert!(
            !words.contains("Unexpected") && !words.contains("secret-host"),
            "{words}"
        );
        let denied = opendal::Error::new(opendal::ErrorKind::PermissionDenied, "x");
        assert!(failure_words("renaming", &denied)
            .to_string()
            .contains("refused permission"));
    }
}
