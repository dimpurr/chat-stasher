//! The local mirror (ADR-062, LM-2): one declared remote destination, copied
//! file by file, byte for byte, onto this machine's disk.
//!
//! What this engine does is deliberately small: the destination repository is
//! *content-addressed storage* — every file except `config` is stored under
//! the SHA-256 of its own bytes — so "mirror the repository" is exactly
//! "list the remote's files, download the ones the mirror lacks, verify each
//! one's bytes against its own name". Nothing is decrypted to copy it: the
//! mirror holds ciphertext only, written with the destination's existing key
//! material (`keys/` is copied verbatim, which is the same encrypted master
//! key, not a new one). `config` is the one non-content-addressed file, so it
//! is compared byte for byte and re-copied when it differs (ADR-062 D2, C4).
//!
//! The copy scope is the whole repository: `config`, `keys/`, `index/`,
//! `snapshots/`, `data/`. `locks/` is never copied — this build holds no
//! repository-wide lock and the mirror needs none to read ([`FileType`] in
//! the vendored rustic_core has no locks variant, so nothing below can even
//! reach such a file).
//!
//! Silence and noise are both load-bearing here:
//!
//! * A round that stops early — the round budget ran out (W964's principle,
//!   applied to sync by ruling C2), the free-disk threshold tripped (C3), or
//!   the remote became unreadable mid-copy (ADR-023's family) — **is not a
//!   failure**: the next round resumes exactly where this one stopped, and
//!   [`crate::mirrorstate`] records nothing until a round finishes, per the
//!   push-state discipline (ruling 2). Progress is a separate record
//!   ([`MIRROR_PROGRESS_FILE`]), written at the end of every round including
//!   stopped ones, so the first fill can answer "how far along" without
//!   weakening the state file's completed-only rule. A **hard failure**
//!   ([`HardFailure`]: verified source corruption, a refused local write, an
//!   over-bound listing) is the one class outside this pause family — a
//!   checked-and-failed fact is the exit `1` class and is worded as the
//!   failure it is, never as "did not finish" (invariant 2, `CLAUDE.md`).
//! * [`mirror_status_lines`] reports the first fill as progress, never as an
//!   alarm, so a multi-day first fill does not paint `status` red while it
//!   is merely large (commit C2) — and a *failed* round is the exception
//!   that does read as the failure it is.
//!
//! Connections: the same account a push draws from carries a ceiling shared
//! with every other reader (ADR-062 D2) — so the copy stage never holds more than
//! [`MIRROR_CONNECTIONS`] (2) requests in flight, independent of the
//! destination's own push concurrency, so a mirror can never eat the
//! account's connection budget (ADR-062 C2; [`crate::store::MAX_CONNECTIONS`]
//! names the shared ceiling).
//!
//! Privacy: ids, byte counts, times and the declared destination name. No
//! session ids, no paths in state files, no conversation content anywhere in
//! this module's outputs.

use crate::store::{BackupStore, StoreConfig};
use crate::{config::Config, mirrorstate, orphans};
use anyhow::Context;
use rustic_core::repofile::MasterKey;
use rustic_core::{Credentials, FileType, Id, OpenStatus, ReadBackend, Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// The ceiling this copier holds itself to, independent of the destination's
/// push concurrency: at most 2 in-flight reads against the mirrored
/// destination, because the same account's connections are a shared budget
/// push and normal reads also draw from (ADR-062 C2).
pub const MIRROR_CONNECTIONS: usize = 2;

/// Hard cap on a single worker's in-memory file read: the copy stage streams
/// files whole via `read_full`: a content-addressed file has to be hashed
/// as one unit before it is written. The cap is enforced at two points,
/// and each point holds half of the promise an honest reader may take
/// from it:
///
/// * the listing's own declared size is checked *before* the read, so a
///   declaration over the cap is refused before `read_full` allocates
///   anything (twice over, with 2 workers) — the half a listing cannot
///   disagree with itself about, and also the half that only sees what
///   the listing *says*;
/// * the returned length is checked *after* the read, so a listing that
///   under-declares and then serves more bytes is refused before any of
///   them are written — but by then the allocation has already happened,
///   and the bytes for the under-declared tail have already been held.
///
/// Both refusals end the round as a hard failure (never a pause), and
/// neither lies about a moving `read_full`: the backend read itself is
/// not length-capped, so the *declared* size is the only bound the
/// pre-read half can honor, and a lying listing meets the cap only after
/// its bytes are in memory. A size beyond either check stops the round
/// rather than letting the mirror grow toward an unbounded read.
const MAX_WHOLE_FILE_BYTES: u64 = 1 << 34;

/// File name for the per-round progress record inside the state dir. This is
/// the "X% of the first fill" record: written at the end of **every** round
/// (completed and stopped alike), so a multi-round first fill can answer
/// "how far along" without weakening the completed-only rule of
/// [`crate::mirrorstate`] (ADR-062 C2 leaves this to the implementation).
pub const MIRROR_PROGRESS_FILE: &str = "mirror-progress.json";

/// Bumped whenever the progress shape below changes incompatibly. An unknown
/// version reads back as unreadable, never as a healthy record.
pub const MIRROR_PROGRESS_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// This machine's `[mirror]` section, resolved. Deriving it from the config
/// rather than accepting raw section values keeps the default-resolution
/// rules (ruling 1 and C3) in one place.
#[derive(Debug, Clone, PartialEq)]
pub struct MirrorSettings {
    /// The on/off switch. Default false: the mirror is a whole-archive copier
    /// and nothing about a default install opts into one (ADR-001 legacy:
    /// the local full copy was always "switchable, default off").
    pub enabled: bool,
    /// The declared name of the destination mirrored (ADR-062 ruling 1: one
    /// `mirror_of`, one mirror per machine).
    pub mirror_of: String,
    /// The free-space floor the mirror refuses to write below, in GiB, when
    /// the user pinned one. `None` = use the resolved default at use time
    /// (ruling 6: max(10 GiB, 5% of the mirror volume's capacity)).
    pub min_free_gib: Option<f64>,
}

impl MirrorSettings {
    /// Resolve the section. `None` when the config declares no `[mirror]`
    /// section at all — an absent section is the default-off machine, not an
    /// error (the caller worded that).
    #[must_use]
    pub fn for_config(config: &Config) -> Option<Self> {
        let section = config.mirror.as_ref()?;
        Some(MirrorSettings {
            // reason: an absent `enabled` key must leave the whole-archive
            // copier off; the mirror is opt-in by design (ADR-062 D1).
            enabled: section.enabled.unwrap_or(false),
            // reason: a section with no mirror_of names no destination —
            // the empty string makes the resolver and status word that
            // plainly ("names no declared destination"), the same
            // at-use-site honesty every other omitted value keeps.
            mirror_of: section.mirror_of.clone().unwrap_or_default(),
            min_free_gib: section.min_free_gib,
        })
    }

    /// The free-space floor in bytes for the volume holding `mirror_root`.
    ///
    /// The configured value wins when set. Otherwise the default is
    /// max(10 GiB, 5% of the volume's capacity) (ruling 6): on the machines
    /// this mirror targets the 5% term dominates large disks, while the 10
    /// GiB floor keeps a small volume from computing a threshold no write
    /// can respect. `mirror_root` need not exist yet — a caller resolves
    /// the floor *before* the round creates the directory — so the
    /// capacity is measured on [`capacity_of_volume`]'s nearest existing
    /// ancestor, which is the volume the directory will be created on;
    /// that keeps the caller's floor equal to the one the round itself
    /// derives mid-copy. When no capacity can be queried the floor keeps
    /// the documented GiB term rather than inventing a percentage of an
    /// unknown size — never zero, which would read "unbounded".
    #[must_use]
    pub fn min_free_bytes(&self, mirror_root: &Path) -> u64 {
        const GIB: f64 = 1_073_741_824.0;
        if let Some(gib) = self.min_free_gib {
            return (gib.max(0.0) * GIB) as u64;
        }
        let floor = 10.0 * GIB;
        match capacity_of_volume(mirror_root) {
            Ok(capacity) => {
                let five_percent = 0.05 * capacity as f64;
                floor.max(five_percent) as u64
            }
            Err(_) => floor as u64,
        }
    }
}

/// The capacity of the volume `path` names, where the volume is found by
/// walking to the nearest existing ancestor: callers resolve the floor
/// before the round creates the mirror directory, and the volume
/// `fs::create_dir_all` will place that directory on is the one its
/// nearest existing ancestor is already on — so the pre-round floor
/// measures the real future volume instead of falling to the flat 10 GiB
/// the unmeasurable path used to trigger (the first round then enforced
/// two different floors; rev2's finding).
fn capacity_of_volume(path: &Path) -> anyhow::Result<u64> {
    let mut probe = path.to_path_buf();
    loop {
        if let Ok(capacity) = capacity_bytes(&probe) {
            return Ok(capacity);
        }
        let Some(parent) = probe.parent().map(Path::to_path_buf) else {
            // No ancestor exists to measure either — the honest answer
            // stays the error, and the caller keeps its documented
            // fallback.
            return capacity_bytes(&probe);
        };
        if parent == probe {
            return capacity_bytes(&probe);
        }
        probe = parent;
    }
}

/// Where the mirror of `destination` lives under a data root: the shape
/// `~/.local/share/chat-stasher/mirror/<destination>` (ADR-062 D1).
#[must_use]
pub fn mirror_dir(data_root: &Path, destination: &str) -> PathBuf {
    data_root.join("mirror").join(destination)
}

// ---------------------------------------------------------------------------
// Free disk
// ---------------------------------------------------------------------------

/// Bytes a non-privileged write can still allocate on the volume holding
/// `path`. The path or any parent must exist.
///
/// On a platform with no supported query this is an error rather than zero:
/// "could not look" must never read as "no space" or "plenty of space" — the
/// caller either skips its check with a stated reason or refuses.
pub fn available_bytes(path: &Path) -> anyhow::Result<u64> {
    available_and_capacity_bytes(path).map(|(available, _)| available)
}

/// Total capacity in bytes of the volume holding `path`.
///
/// Same error rule as [`available_bytes`].
pub fn capacity_bytes(path: &Path) -> anyhow::Result<u64> {
    available_and_capacity_bytes(path).map(|(_, capacity)| capacity)
}

#[cfg(unix)]
fn available_and_capacity_bytes(path: &Path) -> anyhow::Result<(u64, u64)> {
    let cpath = std::ffi::CString::new(
        path.as_os_str()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("path is not utf-8: {}", path.display()))?,
    )
    .context("build a c path for statvfs")?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `stat` is a valid, uninitialized buffer of exactly the type
    // statvfs writes into, and it is read only after libc succeeded.
    let rc = unsafe { libc::statvfs(cpath.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(anyhow::anyhow!(
            "statvfs({}) failed with errno {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: statvfs succeeded, so the buffer now holds a real statvfs.
    let stat = unsafe { stat.assume_init() };
    let unit = u64::from(stat.f_frsize.max(1));
    let available = u64::from(stat.f_bavail).saturating_mul(unit);
    let capacity = u64::from(stat.f_blocks).saturating_mul(unit);
    Ok((available, capacity))
}

#[cfg(windows)]
fn available_and_capacity_bytes(path: &Path) -> anyhow::Result<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use winapi::shared::ntdef::ULARGE_INTEGER;
    use winapi::um::fileapi::GetDiskFreeSpaceExW;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut available = ULARGE_INTEGER::default();
    let mut total = ULARGE_INTEGER::default();
    let mut free_total = ULARGE_INTEGER::default();
    // SAFETY: every out-pointer is a valid zeroed ULARGE_INTEGER, and the
    // directory path is null-terminated above.
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free_total) };
    if ok == 0 {
        return Err(anyhow::anyhow!(
            "GetDiskFreeSpaceExW({}) failed with error {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `QuadPart()` is winapi's u64 view of the same eight bytes the
    // call filled; reading it after a successful call is sound, and the three
    // locals above were zero-initialized by `ULARGE_INTEGER::default()`.
    let available = unsafe { *available.QuadPart() };
    let capacity = unsafe { *total.QuadPart() };
    Ok((available, capacity))
}

#[cfg(not(any(unix, windows)))]
fn available_and_capacity_bytes(path: &Path) -> anyhow::Result<(u64, u64)> {
    // No platform here ships a supported free-space query, so the honest
    // answer is the one callers turn into a stated refusal — never zero,
    // which would read as "no space" or "all the space".
    let _ = path;
    Err(anyhow::anyhow!(
        "no supported free-space query on this platform"
    ))
}

// ---------------------------------------------------------------------------
// Copy model
// ---------------------------------------------------------------------------

/// One remote repository file, by its role. The mapping to on-disk layout is
/// the backend's own (rustic_core `FileType::dirname` and the `data/xx/`
/// sharding for packs), restated here so the mirror is openable by the same
/// backend implementation (ADR-062 D2 reads the mirror "as a local backend").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Config,
    Key,
    Snapshot,
    Index,
    Pack,
}

impl FileKind {
    fn file_type(self) -> FileType {
        match self {
            Self::Config => FileType::Config,
            Self::Key => FileType::Key,
            Self::Snapshot => FileType::Snapshot,
            Self::Index => FileType::Index,
            Self::Pack => FileType::Pack,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Key => "keys",
            Self::Snapshot => "snapshots",
            Self::Index => "index",
            Self::Pack => "data",
        }
    }
}

/// One planned file: what it is, the hex id it is stored under (its own
/// SHA-256 for everything except `config`, an opaque single row for
/// `config`), and its listed remote size. The `size` field is public
/// because outside implementors of [`RoundEnds`] (the injected stoppers of
/// tests) decide on it; the identity fields stay private to the module.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    kind: FileKind,
    id: Id,
    hex: String,
    /// The remote's listed size for this file, in bytes.
    pub size: u64,
}

/// Where a file of this kind lives under the mirror root.
fn local_path(mirror_root: &Path, file: &PlannedFile) -> PathBuf {
    match file.kind {
        FileKind::Config => mirror_root.join("config"),
        FileKind::Pack => mirror_root
            .join("data")
            .join(&file.hex[..2])
            .join(&file.hex),
        _ => mirror_root.join(file.kind.name()).join(&file.hex),
    }
}

/// The hex SHA-256 of some bytes — same spelling the repository's own file
/// names use (`Id::to_hex`, see rustic_core `id.rs`).
fn hex_sha256(bytes: &[u8]) -> String {
    hex_string(&Sha256::digest(bytes))
}

/// Pure hex encoding, no hashing: this is how an already-finalized digest
/// becomes its file-name spelling. (The mirror once asked `hex_sha256` —
/// which hashes — to encode a digest, and every present file then failed
/// verification as a double-sha256; the two-step names below keep that
/// mistake from coming back.)
fn hex_string(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

// ---------------------------------------------------------------------------
// Round endings
// ---------------------------------------------------------------------------

/// Why a copy round stopped before finishing. None of these are failures of
/// the pass that ends here — the next round resumes (W964 applied to sync
/// by ADR-062 C2); the alarm is in how each reason is worded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum RoundEnd {
    /// The round's time budget ran out mid-copy; work continues next round.
    BudgetSpent,
    /// Free space on the mirror volume fell to the configured floor;
    /// writing on would risk filling the disk (ADR-062 C3's threshold, never
    /// "write until full").
    DiskBelowThreshold {
        /// Free bytes observed when the round ended.
        free_bytes: u64,
        /// The floor in effect (bytes).
        min_free_bytes: u64,
    },
    /// The remote could not be read mid-copy (network, credentials, service);
    /// ADR-023's family — a classification of the read failure, never proof
    /// the file does not exist. `why` is terminal-only: it may carry a
    /// repository path, so it is never written to a state file (the progress
    /// record gets a class word instead).
    RemoteRead { why: String },
}

/// The hard-failure classes the copy loop cannot pause out of. A hard
/// failure is a *checked and failed* fact (invariant 2, `CLAUDE.md`): the
/// source no longer holds the bytes its own listing claims, the local write
/// refused, or the listing names a file over the whole-file bound. It is
/// the caller's exit `1` family, never "did not finish" (exit `3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardClass {
    /// A content-addressed file failed verification twice in a row; not a
    /// transfer a later round can heal.
    SourceCorrupt,
    /// Writing the verified bytes to the mirror volume failed (full disk,
    /// permissions, a vanished parent); the round refuses to guess what a
    /// retry would find, but the class is honest that it may heal.
    LocalWrite,
    /// The listing named a file over [`MAX_WHOLE_FILE_BYTES`] — refused
    /// before the read when the declaration itself is over the cap, and
    /// after the read when the bytes are (the constant's doc states what
    /// each half can and cannot bound).
    OversizeListing,
}

impl HardClass {
    /// The wording each class carries on every surface, so the CLI's
    /// exit-1 line and the pass's warning cannot disagree.
    #[must_use]
    pub fn sentence(self) -> &'static str {
        match self {
            Self::SourceCorrupt => "the source proved corrupt",
            Self::LocalWrite => "a local write failed",
            Self::OversizeListing => "the listing named a file over the whole-file bound",
        }
    }
}

/// A hard failure carrying the round out as `Err` ([`sync_once`]'s exit-1
/// class). Two halves on purpose:
///
/// * `why` is **safe for a state file**: ids, sizes and class words only,
///   built by this module, path-free by construction. It is the progress
///   record's `outcome_note` when a round fails.
/// * `detail` is the terminal-only half (a local or backend error chain,
///   paths and all). It prints through `Display` for the human at the
///   keyboard, and nothing in this module writes it to disk.
#[derive(Debug, Clone)]
pub struct HardFailure {
    pub class: HardClass,
    /// State-file-safe sentence; never a raw error chain.
    pub why: String,
    /// Terminal-only error detail, if any was captured; never recorded.
    pub detail: Option<String>,
}

impl std::fmt::Display for HardFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.why)?;
        if let Some(detail) = &self.detail {
            write!(f, " ({detail})")?;
        }
        Ok(())
    }
}

impl std::error::Error for HardFailure {}

/// A usage-class refusal ([`sync_once`]'s exit-2 family): the `[mirror]`
/// declaration or the destination it names cannot be used — an unset
/// `mirror_of`, a negative `min_free_gib`, a destination with no repository
/// to mirror. Nothing was read or written when one of these is returned, so
/// no progress record is written either: usage is "nothing was done", not
/// "a round ran and stopped".
#[derive(Debug, Clone)]
pub struct MirrorUsageError(pub String);

impl std::fmt::Display for MirrorUsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for MirrorUsageError {}

/// The stop-decision hook the copy loop asks before every file. Production
/// runs use [`DeadlineAndDisk`]; tests inject deterministic stoppers without
/// racing real clocks or disk pressure.
///
/// `Sync` because the two copy workers share one stopper across scoped
/// threads — every stop rule is a read-only predicate.
pub trait RoundEnds: Sync {
    /// Whether this round should end now, before starting `next_file`. The
    /// answer is asked *before* each file, so a budget that is already spent
    /// stops before it moves data it cannot finish.
    fn round_end(&self, next_file: &PlannedFile) -> Option<RoundEnd>;
}

/// The production stop-rule: this round's deadline (None = unlimited, an
/// in-process caller's choice) plus the free-disk floor.
pub struct DeadlineAndDisk {
    /// When this round must stop copying. Budget checks happen *between*
    /// files — a file already in flight is allowed to finish, which is what
    /// keeps a partial file from being the norm.
    deadline: Option<Instant>,
    min_free_bytes: u64,
    mirror_root: PathBuf,
}

impl DeadlineAndDisk {
    #[must_use]
    pub fn new(deadline: Option<Instant>, min_free_bytes: u64, mirror_root: PathBuf) -> Self {
        DeadlineAndDisk {
            deadline,
            min_free_bytes,
            mirror_root,
        }
    }
}

impl RoundEnds for DeadlineAndDisk {
    fn round_end(&self, next_file: &PlannedFile) -> Option<RoundEnd> {
        if let Some(deadline) = self.deadline {
            if Instant::now() >= deadline {
                return Some(RoundEnd::BudgetSpent);
            }
        }
        // A file that alone would breach the floor ends the round before it
        // starts: the check is a threshold on what would remain after the
        // write, not on what the disk holds now.
        if let Ok(free) = available_bytes(&self.mirror_root) {
            if free.saturating_sub(next_file.size) < self.min_free_bytes {
                return Some(RoundEnd::DiskBelowThreshold {
                    free_bytes: free,
                    min_free_bytes: self.min_free_bytes,
                });
            }
        }
        // A failed free-space query leaves the honest answer to the caller:
        // the round proceeds rather than pausing forever on a platform
        // quirk, because an unknown amount of free space is not proof of
        // danger any more than it is proof of safety — the write attempt's
        // own error is the next signal (unreachable on unix and windows
        // today; see the cfg triple in this module).
        None
    }
}

// ---------------------------------------------------------------------------
// Sync outcome
// ---------------------------------------------------------------------------

/// What one completed `sync_once` call did, with the round's counters riding
/// along for the command that reports them (`sync_once` also wrote them
/// into the durable records, so a caller that only needs the class can
/// ignore the report half).
#[derive(Debug)]
pub enum SyncOutcome {
    /// The mirror now holds every file the remote listed, hash-verified; the
    /// closure record is written.
    Completed,
    /// The round refused before starting: the pre-estimate of missing bytes
    /// plus the headroom does not fit the free space (ADR-062 C3), so this
    /// round moved no data at all. C3's human numbers ride along.
    RefusedSpace {
        /// What the whole remaining fill needs, including headroom.
        needed_bytes: u64,
        /// The missing files' bytes, the download itself.
        missing_bytes: u64,
        /// The headroom the user configured (or the default floor).
        headroom_bytes: u64,
        /// What was free when the estimate ran.
        free_bytes: u64,
    },
    /// The round ended part-way: budget, disk floor, or remote readability.
    /// Nothing is lost — every file this round finished writing is
    /// hash-verified and the next call resumes from the listing.
    Stopped(RoundEnd),
}

/// Counters one round leaves behind, for the lines `mirror sync` prints, the
/// progress record, and the completed record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub mirror_of: String,
    pub remote_files: u64,
    pub remote_total_bytes: u64,
    /// Files already present and hash-verified when the round began.
    pub present_files: u64,
    pub present_bytes: u64,
    /// Files this round downloaded and hash-verified.
    pub downloaded_files: u64,
    pub downloaded_bytes: u64,
    /// Byte size the mirror still lacks relative to this listing.
    pub missing_bytes_remaining: u64,
    /// Snapshot files in the listing (the closure candidates).
    pub snapshots_remote: u64,
    /// Snapshots whose closure the completed round could claim.
    pub covered_snapshots: u64,
}

/// Everything one sync round needs. `master_key` is the destination's own
/// key file as every store operation loads it — the mirror is opened with
/// the destination's password, not a new one (ADR-062 C4).
pub struct SyncRequest<'a> {
    pub settings: &'a MirrorSettings,
    /// The resolved destination this mirror mirrors (`mirror_of`, resolved
    /// by the CLI exactly like any other destination consumer).
    pub remote: &'a StoreConfig,
    /// The machine's data root (`config::default_data_root()`); the mirror
    /// lives under `<data root>/mirror/<mirror_of>`.
    pub data_root: &'a Path,
    /// The state dir that holds mirror-state.json and the progress record.
    pub state_dir: &'a Path,
    /// The destination's masterkey, for opening the copied mirror read-only
    /// and computing the closure set from the mirror's own index.
    pub master_key: &'a MasterKey,
    /// Wall time of the epoch, supplied so records are testable.
    pub now_unix: u64,
    /// The free-disk floor for this round, in bytes — computed once by the
    /// caller ([`MirrorSettings::min_free_bytes`]) so the round's
    /// pre-estimate and the caller-built stop-rule share one floor
    /// (rev2's finding: when the round derives its own, a first round can
    /// enforce two — the caller's pre-creation fallback and the
    /// engine's — and the mid-round one is the weaker). The floor is the
    /// caller's to pass because the [`RoundEnds`] stop-rule already is.
    pub min_free_bytes: u64,
    /// The round's stop-rule.
    pub round_ends: &'a dyn RoundEnds,
}

/// Run one mirror sync round (ADR-062 D2): list → decide missing → copy with
/// verification, honoring the pre-estimate, the stop-rule and the ≤2
/// connection ceiling; on completion compute the closure set and record it.
///
/// # Errors
///
/// Three classes, and the caller must keep them apart:
///
/// * `MirrorUsageError` — the declaration or the destination is unusable.
///   Nothing was done; the CLI's exit `2`.
/// * `HardFailure` — a checked-and-failed fact: the source no longer holds
///   the bytes its own listing claims (a re-read cannot heal it), a local
///   write refused, or the listing named a file over the whole-file bound.
///   The CLI's exit `1`, with the class's own sentence.
/// * Everything else — a read that did not finish (ADR-023's family) —
///   propagates as an ordinary error; the CLI's exit `3`, a pause the next
///   round resumes.
///
/// [`SyncOutcome`] variants are the pause-and-resume class.
pub fn sync_once(req: &SyncRequest) -> anyhow::Result<(SyncReport, SyncOutcome)> {
    anyhow::ensure!(
        !req.settings.mirror_of.trim().is_empty(),
        MirrorUsageError(
            "[mirror] mirror_of is empty; name the declared destination to mirror".to_string()
        )
    );
    anyhow::ensure!(
        req.settings
            .min_free_gib
            .is_none_or(|gib| gib.is_finite() && gib >= 0.0),
        MirrorUsageError(format!(
            "[mirror] min_free_gib must be 0 or a positive number of GiB, not {:?}",
            req.settings.min_free_gib
        ))
    );
    let mirror_root = mirror_dir(req.data_root, &req.settings.mirror_of);

    let store = BackupStore::for_metadata_query(req.remote.clone());
    let backends = store.backends().context("build the destination backend")?;
    let be = backends.repository();

    // ---- List what the remote holds, one file inventory ----
    let mut planned = Vec::new();
    let mut snapshot_ids = BTreeSet::new();
    for kind in [
        FileKind::Config,
        FileKind::Key,
        FileKind::Index,
        FileKind::Snapshot,
        FileKind::Pack,
    ] {
        let listed = be
            .list_with_size(kind.file_type())
            .with_context(|| format!("list {} in the destination", kind.name()))?;
        for (id, size) in listed {
            let hex = id.to_hex().to_string();
            if kind == FileKind::Snapshot {
                snapshot_ids.insert(hex.clone());
            }
            planned.push(PlannedFile {
                kind,
                id,
                hex,
                size: u64::from(size),
            });
        }
    }
    let remote_total_bytes = planned.iter().map(|file| file.size).sum::<u64>();
    let config_count = planned
        .iter()
        .filter(|file| file.kind == FileKind::Config)
        .count();
    anyhow::ensure!(
        config_count <= 1,
        "the destination listing holds {config_count} config files; a repository has one"
    );
    if config_count == 0 {
        // A destination with no `config` file is not a repository, so
        // nothing about this command's inputs can work: it is a usage
        // problem (exit 2), reported before the mirror directory is
        // created — a refused round must not leave an empty tree behind.
        return Err(anyhow::Error::new(MirrorUsageError(format!(
            "the destination at {} has not been initialised (no config file listed); \
             run a push first — a mirror copies an existing repository, and there is \
             no repository there yet",
            req.remote.repo_root
        ))));
    }

    // The mirror directory is created only once the destination proved to
    // be a repository: the earlier ensure!s and the uninitialised refusal
    // above leave nothing on disk.
    fs::create_dir_all(&mirror_root)
        .with_context(|| format!("create mirror dir {}", mirror_root.display()))?;

    // ---- Classify: present-and-verified, or missing ----
    // `config` has no content address, so its check is a byte comparison
    // against the one copy the remote holds; every content-addressed file is
    // checked by hashing the local bytes and comparing against its own name
    // — a local file that no longer matches its name (tamper, rot, a torn
    // write) is missing again and re-downloaded (mismatch ⇒ re-download).
    let remote_config = planned
        .iter()
        .find(|file| file.kind == FileKind::Config)
        .map(|file| {
            be.read_full(FileType::Config, &file.id)
                .map(|bytes| bytes.to_vec())
                .context("read the destination config for comparison")
        })
        .transpose()?;
    let mut present_files = 0_u64;
    let mut present_bytes = 0_u64;
    let mut is_present = vec![false; planned.len()];
    for (index, file) in planned.iter().enumerate() {
        let path = local_path(&mirror_root, file);
        let verified = match file.kind {
            FileKind::Config => match (fs::read(&path), remote_config.as_deref()) {
                (Ok(local), Some(remote)) => local.as_slice() == remote,
                _ => false,
            },
            _ => match sha256_file(&path) {
                Ok(actual) => actual == file.hex,
                Err(_) => false,
            },
        };
        if verified {
            present_files += 1;
            present_bytes += file.size;
            is_present[index] = true;
        }
    }
    let missing_bytes: u64 = planned
        .iter()
        .enumerate()
        .filter(|(index, _)| !is_present[*index])
        .map(|(_, file)| file.size)
        .sum();

    // ---- C3 pre-estimate: refuse before the first byte moves ----
    // The caller computed this floor before the round, on the same volume
    // (min_free_bytes measures the nearest existing ancestor while the
    // mirror directory does not exist yet), so its stop-rule and this
    // pre-estimate are one floor — not two the round could disagree with
    // itself over (rev2's finding).
    let min_free_bytes = req.min_free_bytes;
    let free_now =
        available_bytes(&mirror_root).context("query free space on the mirror volume")?;
    let needed_bytes = missing_bytes.saturating_add(min_free_bytes);
    if free_now < needed_bytes {
        let report = SyncReport {
            mirror_of: req.settings.mirror_of.clone(),
            remote_files: planned.len() as u64,
            remote_total_bytes,
            present_files,
            present_bytes,
            missing_bytes_remaining: missing_bytes,
            snapshots_remote: snapshot_ids.len() as u64,
            ..SyncReport::default()
        };
        write_progress(
            req,
            &report,
            Ok(ProgressOutcome::Refused(
                missing_bytes,
                min_free_bytes,
                free_now,
            )),
        )?;
        return Ok((
            report,
            SyncOutcome::RefusedSpace {
                needed_bytes,
                missing_bytes,
                headroom_bytes: min_free_bytes,
                free_bytes: free_now,
            },
        ));
    }

    // ---- Copy, ≤2 in flight ----
    // Small metadata first: config, keys, then index and snapshots, packs
    // last by ascending size — the ordering makes a first fill useful as
    // early as possible (an index and snapshot set is the minimum a mirror
    // leg needs to assert anything) and gives large packs the tail of the
    // budget where a cut is cheapest to resume.
    let mut queue: Vec<&PlannedFile> = planned
        .iter()
        .enumerate()
        .filter(|(index, _)| !is_present[*index])
        .map(|(_, file)| file)
        .collect();
    queue.sort_by_key(|file| {
        (
            match file.kind {
                FileKind::Config => 0,
                FileKind::Key => 1,
                FileKind::Index => 2,
                FileKind::Snapshot => 3,
                FileKind::Pack => 4,
            },
            file.size,
        )
    });

    let counters = Counters::default();
    let stop = Mutex::new(None::<StopCause>);
    std::thread::scope(|scope| {
        for _ in 0..MIRROR_CONNECTIONS.min(queue.len()) {
            scope.spawn(|| {
                worker(&mirror_root, &be, &queue, req.round_ends, &counters, &stop);
            });
        }
    });
    let downloaded_files = counters.files.load(Ordering::SeqCst);
    let downloaded_bytes = counters.bytes.load(Ordering::SeqCst);
    let ended = stop.into_inner().ok().flatten();
    let hard_report = |downloaded_files: u64, downloaded_bytes: u64| SyncReport {
        mirror_of: req.settings.mirror_of.clone(),
        remote_files: planned.len() as u64,
        remote_total_bytes,
        present_files,
        present_bytes,
        downloaded_files,
        downloaded_bytes,
        missing_bytes_remaining: missing_bytes.saturating_sub(downloaded_bytes),
        snapshots_remote: snapshot_ids.len() as u64,
        ..SyncReport::default()
    };
    match ended {
        Some(StopCause::Hard(failure)) => {
            // The one class that is not a pause (invariant 2, `CLAUDE.md`):
            // a checked-and-failed fact is recorded as `failed`, and its
            // `why` is built path-free by this module, so the progress
            // record still honours the no-paths privacy claim. The raw
            // error (`detail`) stays on the terminal via the returned
            // error chain.
            write_progress(
                req,
                &hard_report(downloaded_files, downloaded_bytes),
                Err(failure.why.clone()),
            )?;
            return Err(anyhow::Error::new(failure.clone()));
        }
        Some(StopCause::Pause(end)) => {
            write_progress(
                req,
                &hard_report(downloaded_files, downloaded_bytes),
                Ok(ProgressOutcome::Stopped(&end)),
            )?;
            return Ok((
                hard_report(downloaded_files, downloaded_bytes),
                SyncOutcome::Stopped(end),
            ));
        }
        None => {}
    }
    anyhow::ensure!(
        downloaded_files == queue.len() as u64,
        "the copy loop ended without finishing or stopping; {} of {} missing files copied",
        downloaded_files,
        queue.len()
    );

    // ---- Completed: closure and records ----
    let (covered, coverage_anomaly) = closure_snapshot_ids(&mirror_root, req.master_key);
    let covered_snapshots = covered.len() as u64;
    if let Some(anomaly) = &coverage_anomaly {
        // Loud, never fatal: a completed copy is a fact even when the
        // remote's own index points at a pack it does not list — the
        // covered set records only what can be claimed, which is nothing
        // this round, and the next round re-writes the record when the
        // remote's stuck push finishes.
        eprintln!("[mirror] coverage anomaly this round: {anomaly}");
    }
    mirrorstate::save(
        req.state_dir,
        &mirrorstate::MirrorState {
            version: mirrorstate::MIRROR_STATE_VERSION,
            finished_at_unix: req.now_unix,
            mirror_of: req.settings.mirror_of.clone(),
            closure_complete: covered,
            snapshots_on_remote: snapshot_ids.len() as u64,
            remote_total_bytes,
        },
    )
    .context("write mirror-state.json after a completed sync")?;
    let report = SyncReport {
        mirror_of: req.settings.mirror_of.clone(),
        remote_files: planned.len() as u64,
        remote_total_bytes,
        present_files,
        present_bytes,
        downloaded_files,
        downloaded_bytes,
        missing_bytes_remaining: 0,
        snapshots_remote: snapshot_ids.len() as u64,
        covered_snapshots,
    };
    write_progress(req, &report, Ok(ProgressOutcome::Complete))?;
    Ok((report, SyncOutcome::Completed))
}

/// Stream a file's bytes through SHA-256 without holding it whole in memory
/// (the sweep hashes gigabytes every round; a whole-file read per present
/// file would multiply the mirror's first-fill memory by its file count).
fn sha256_file(path: &Path) -> anyhow::Result<String> {
    use std::io::BufRead;
    let file = fs::File::open(path)
        .with_context(|| format!("open {} for the tamper sweep", path.display()))?;
    let mut reader = std::io::BufReader::with_capacity(1 << 22, file);
    let mut hasher = Sha256::new();
    loop {
        let chunk = reader
            .fill_buf()
            .with_context(|| format!("read {} for the tamper sweep", path.display()))?;
        if chunk.is_empty() {
            break;
        }
        hasher.update(chunk);
        let len = chunk.len();
        reader.consume(len);
    }
    Ok(hex_string(&hasher.finalize()))
}

#[derive(Default)]
struct Counters {
    next: AtomicUsize,
    files: AtomicU64,
    bytes: AtomicU64,
}

/// What ended the round: a pause ([`RoundEnd`], resumable by design) or a
/// hard failure ([`HardFailure`], the checked-and-failed class that must not
/// be worded as "did not finish").
#[derive(Debug, Clone)]
enum StopCause {
    Pause(RoundEnd),
    Hard(HardFailure),
}

/// Whether a stop has been recorded this round. A poisoned lock reads as
/// stopped: the only way it can poison is a worker unwinding through a
/// guard, and letting the sibling keep draining the queue after that would
/// hide the panic from the join's own failure mode.
fn stop_set(stop: &Mutex<Option<StopCause>>) -> bool {
    stop.lock().map(|guard| guard.is_some()).unwrap_or(true)
}

/// Record the round's ending; the first reason wins. A poisoned lock leaves
/// the flag unset (and [`stop_set`] reads that as stopped) — nothing
/// reliable remains to coordinate, and the join reports the panic instead.
fn set_stop_once(stop: &Mutex<Option<StopCause>>, end: StopCause) {
    if let Ok(mut guard) = stop.lock() {
        if guard.is_none() {
            *guard = Some(end);
        }
    }
}

/// One copy worker. Runs until the queue is drained or the round is ordered
/// to stop; every finished file is already on disk, whole and renamed,
/// when the worker exits — a stopped round leaves no "half file" state a
/// resume has to trust, so the next round can simply list and diff again.
fn worker(
    mirror_root: &Path,
    be: &std::sync::Arc<dyn rustic_core::WriteBackend>,
    queue: &[&PlannedFile],
    round_ends: &dyn RoundEnds,
    counters: &Counters,
    stop: &Mutex<Option<StopCause>>,
) {
    loop {
        // Claim the next file, then decide about it: the stop-rule needs the
        // file's size (the disk floor asks what this write would leave), and
        // a stop taken after a claim releases the claim again, so a round
        // that reports "drained" really copied every missing file.
        let index = counters.next.fetch_add(1, Ordering::SeqCst);
        let Some(file) = queue.get(index) else { return };
        if stop_set(stop) {
            counters.next.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        if let Some(end) = round_ends.round_end(file) {
            counters.next.fetch_sub(1, Ordering::SeqCst);
            set_stop_once(stop, StopCause::Pause(end));
            return;
        }
        match copy_one(mirror_root, be, file) {
            Ok(bytes) => {
                counters.files.fetch_add(1, Ordering::SeqCst);
                counters.bytes.fetch_add(bytes, Ordering::SeqCst);
            }
            Err(end) => {
                counters.next.fetch_sub(1, Ordering::SeqCst);
                set_stop_once(stop, end);
                return;
            }
        }
    }
}

/// Copy one file: read whole ([`MAX_WHOLE_FILE_BYTES`] bounded), verify its
/// bytes against its own name, write atomically (temp + rename). A hash
/// mismatch is re-downloaded once, exactly once — a second mismatch is not
/// a transfer problem but the source itself no longer holding the bytes its
/// own listing claims, and that is reported as the hard error it is.
fn copy_one(
    mirror_root: &Path,
    be: &std::sync::Arc<dyn rustic_core::WriteBackend>,
    file: &PlannedFile,
) -> Result<u64, StopCause> {
    let label = if file.kind == FileKind::Config {
        "config".to_string()
    } else {
        format!(
            "file {}/{}",
            file.kind.name(),
            file.hex.get(..12).unwrap_or(&file.hex)
        )
    };
    // The pre-read half of the whole-file cap (see
    // [`MAX_WHOLE_FILE_BYTES`]): the listing declared this size, so it is
    // refused *before* `read_full` allocates it — twice over with 2
    // workers. The half a lying listing meets is the post-read check
    // below, which fires after the read has already held the bytes the
    // declaration understated.
    if file.size > MAX_WHOLE_FILE_BYTES {
        return Err(StopCause::Hard(HardFailure {
            class: HardClass::OversizeListing,
            why: format!(
                "{label} is listed at {} bytes, over the {} whole-file bound; the read \
                 was refused rather than allocation the copy stage cannot bound",
                file.size, MAX_WHOLE_FILE_BYTES
            ),
            detail: None,
        }));
    }
    let attempts = if file.kind == FileKind::Config { 1 } else { 2 };
    for attempt in 0..attempts {
        let bytes = be.read_full(file.kind.file_type(), &file.id).map_err(|e| {
            // ADR-023's family exactly because it is terminal-only: the
            // chain `why` can carry a repository path or endpoint, so it
            // may flow to a pause message on a terminal, never to a state
            // file.
            StopCause::Pause(RoundEnd::RemoteRead {
                why: format!("{e:#}"),
            })
        })?;
        if bytes.len() as u64 > MAX_WHOLE_FILE_BYTES {
            // The listing under-declared: the bytes are already in memory
            // (the cap's post-read half — the pre-read guard above fired
            // first in the normal case, exactly because it only sees the
            // declared size).
            return Err(StopCause::Hard(HardFailure {
                class: HardClass::OversizeListing,
                why: format!(
                    "{label} reads at {} bytes, over the {} whole-file bound",
                    bytes.len(),
                    MAX_WHOLE_FILE_BYTES
                ),
                detail: None,
            }));
        }
        let verified = match file.kind {
            // config is the one non-content-addressed file: byte-copied,
            // never name-verified (ADR-062 D2, C4).
            FileKind::Config => true,
            _ => hex_sha256(&bytes) == file.hex,
        };
        if verified {
            write_atomic(&local_path(mirror_root, file), &bytes).map_err(|e| {
                // `why` is safe for a state file by construction: the
                // error's io class only, never the path (the raw chain is
                // the terminal-only `detail`).
                StopCause::Hard(HardFailure {
                    class: HardClass::LocalWrite,
                    why: format!(
                        "write {label}: the local write failed ({})",
                        io_class_word(&e)
                    ),
                    detail: Some(format!("{e:#}")),
                })
            })?;
            return Ok(bytes.len() as u64);
        }
        if attempt + 1 == attempts {
            let short = file.hex.get(..12).unwrap_or(&file.hex);
            return Err(StopCause::Hard(HardFailure {
                class: HardClass::SourceCorrupt,
                why: format!(
                    "file {}/{} hashed against its own name twice and failed both times; \
                     the destination no longer holds the bytes its listing claims — \
                     this is source corruption, not a transfer a later round can heal",
                    file.kind.name(),
                    short
                ),
                detail: None,
            }));
        }
    }
    Err(StopCause::Hard(HardFailure {
        class: HardClass::SourceCorrupt,
        why: "the copy loop fell through its own attempt count".to_string(),
        detail: None,
    }))
}

/// One short io-class word for a failed write, for state-file-safe notes.
/// io error Display text is generally path-free, but "generally" is not a
/// boundary a privacy claim can rest on, so the class word is derived from
/// `ErrorKind` — the raw error stays terminal-only.
fn io_class_word(error: &anyhow::Error) -> String {
    let kind = error
        .root_cause()
        .downcast_ref::<std::io::Error>()
        .map(std::io::Error::kind);
    match kind {
        Some(std::io::ErrorKind::PermissionDenied) => "permission denied".to_string(),
        Some(std::io::ErrorKind::StorageFull) | Some(std::io::ErrorKind::QuotaExceeded) => {
            "no space left".to_string()
        }
        Some(std::io::ErrorKind::NotFound) => "a path component is missing".to_string(),
        Some(std::io::ErrorKind::TimedOut) => "the write timed out".to_string(),
        // reason: an unmapped io kind still deserves an honest class word
        // rather than the none `map` would leave behind; "an i/o error"
        // asserts the class (an io failure) without inventing a detail the
        // kind does not carry.
        _ => "an i/o error".to_string(),
    }
}

/// Temp file + rename, the write shape every store path here already uses: a
/// reader never sees a partial file, and a name that exists always holds
/// exactly the bytes its content address claims.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("file name is not utf-8: {}", path.display()))?
    ));
    let mut file = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Closure (ADR-062 D4, C1)
// ---------------------------------------------------------------------------

/// The snapshot ids whose closure is complete in the mirror, with the
/// honesty clauses that keep it from over-claiming.
///
/// C1's mechanism, restated as the check it runs (ADR-062 D4/D6.1): the
/// closure of snapshot S is S's file + the round's index files + the packs
/// S's tree references — *in the mirror*, which is why the snapshot set
/// here is the mirror's own listing, not the remote's: the question is
/// "what does this directory vouch for", never "what does the remote hold".
///
/// After a *copied-in-full* round the file-and-index legs are known good
/// (every listed file was downloaded and hash-verified), so what remains is
/// the pack leg. Walked honestly, that needs a decrypted tree-walk per
/// snapshot — which the copy's own invariant makes unnecessary: every blob
/// an existing snapshot's tree references was indexed by its own completed
/// push (rustic writes packs first, index last, snapshot after —
/// `crate::orphans`' module doc), so *if no pack the mirror's index files
/// name is missing from the mirror* then every pack any mirrored snapshot
/// can reference is present. The survey is one index read over the mirror
/// itself — no tree walk, no per-snapshot guesswork, and per-snapshot only
/// in what it writes: membership of the covered set.
///
/// When the index *does* name a pack the mirror lacks (a push elsewhere
/// died between its packs and its index, or a cleanup removed data its
/// index still points at), no snapshot's closure can be claimed
/// per-snapshot without the walk — so the honest answer is the covered set
/// is *empty this round*, loudly: the reclaim leg holds everything back
/// (the safe direction, ADR-062 D4's last bullet), and the next completed
/// round re-writes the record once the mirror's contents cohere again.
pub fn closure_snapshot_ids(
    mirror_root: &Path,
    master_key: &MasterKey,
) -> (BTreeSet<String>, Option<String>) {
    let mirror_store = StoreConfig {
        repo_root: mirror_root.to_string_lossy().into_owned(),
        key_file: PathBuf::new(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: None,
        no_cache: true,
    };
    let opened = open_mirror_for_survey(&mirror_store, master_key);
    let (backends, repo) = match opened {
        Ok(pair) => pair,
        Err(why) => {
            return (
                BTreeSet::new(),
                Some(format!(
                    "the copied mirror cannot be opened as a repository to check the \
                     closure ({why}); no snapshot's closure is claimed this round"
                )),
            );
        }
    };
    // S's own snapshot file is the first leg of S's closure, so the
    // candidate set comes from the mirror's backend listing — a snapshot
    // the mirror does not hold is not covered here no matter what the
    // remote holds.
    let snapshot_ids: BTreeSet<String> = match backends.repository().list(FileType::Snapshot) {
        Ok(ids) => ids.into_iter().map(|id| id.to_hex().to_string()).collect(),
        Err(why) => {
            return (
                BTreeSet::new(),
                Some(format!(
                    "the mirror's snapshot files cannot be listed to check the closure ({why:#}); \
                     no snapshot's closure is claimed this round"
                )),
            );
        }
    };
    let survey = match orphans::survey_packs(&repo, &backends) {
        Ok(survey) => survey,
        Err(why) => {
            return (
                BTreeSet::new(),
                Some(format!(
                    "the mirror's index files cannot be read to check the closure ({why:#}); \
                     no snapshot's closure is claimed this round"
                )),
            );
        }
    };
    if survey.report.missing.is_empty() {
        // Every pack an index file names is present in the mirror, so every
        // snapshot file the mirror holds has its whole closure (the
        // argument above); an unindexed pack cannot break any snapshot's
        // closure because no snapshot references it.
        return (snapshot_ids, None);
    }
    let count = survey.report.missing.len();
    (
        BTreeSet::new(),
        Some(format!(
            "{count} pack(s) the mirror's index files name are absent from the mirror, \
             so no snapshot's closure can be claimed this round"
        )),
    )
}

/// Open the copied repository at survey level: config + key + index files —
/// no index build, no pack reads, nothing written ([`orphans::open_for_survey`]
/// is the read-only open; the cache is off because the mirror is already
/// local and the per-repo cache must never land in the user's cache root).
fn open_mirror_for_survey(
    mirror_store: &StoreConfig,
    master_key: &MasterKey,
) -> anyhow::Result<(rustic_core::RepositoryBackends, Repository<OpenStatus>)> {
    let store = BackupStore::for_metadata_query(mirror_store.clone());
    let backends = store
        .backends()
        .context("build the mirror's local backend")?;
    let repo = Repository::new(&mirror_store.repository_options(), &backends)
        .context("build the mirror repository handle")?
        .open(&Credentials::Masterkey(master_key.clone()))
        .context("open the copied mirror")?;
    Ok((backends, repo))
}

// ---------------------------------------------------------------------------
// Progress record
// ---------------------------------------------------------------------------

/// The per-round progress record (ADR-062 C2's "X% of the first fill"):
/// written at the end of *every* round — completed, stopped, refused,
/// failed — so the display can answer "how far along" without ever weakening
/// the completed-only rule of [`crate::mirrorstate`] (ruling 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorProgress {
    pub version: u32,
    /// The declared destination this progress belongs to.
    pub mirror_of: String,
    /// Wall time of the epoch at the end of the round this record holds.
    pub round_finished_at_unix: u64,
    /// Whether the last round that wrote this completed the fill: the
    /// mirror holds every file that round's listing had.
    pub complete: bool,
    /// Machine-friendly outcome of the last round: "complete", "stopped",
    /// "refused" or "failed" — the last is the hard-failure class
    /// ([`HardFailure`]) — and a free-form human note about it in
    /// [`Self::outcome_note`] when there is one.
    pub outcome: String,
    /// The human note for the last outcome (why it stopped, why it
    /// refused, why it failed), when there is one.
    pub outcome_note: Option<String>,
    /// Bytes the last round's listing held in total (the " of" denominator).
    pub remote_total_bytes: u64,
    /// Bytes of that listing present and hash-verified in the mirror at the
    /// end of the round (the "X%" numerator).
    pub mirror_bytes: u64,
    /// Files in the listing.
    pub remote_files: u64,
    /// Files present and hash-verified at the end of the round.
    pub mirror_files: u64,
}

impl MirrorProgress {
    /// The first-fill percentage, 0..=100 — "already downloaded ÷ remote
    /// total", cross-round (C2). Unrepresentable denominators stay honest:
    /// an empty listing is 100 of nothing, not 0%.
    #[must_use]
    pub fn percent(&self) -> u64 {
        if self.remote_total_bytes == 0 {
            return 100;
        }
        self.mirror_bytes.saturating_mul(100) / self.remote_total_bytes
    }
}

pub fn mirror_progress_path(state_dir: &Path) -> PathBuf {
    state_dir.join(MIRROR_PROGRESS_FILE)
}

/// What one round was, for the progress record's outcome fields. The `Err`
/// half takes the *note text itself*, already redacted by the caller: state
/// files carry no paths, so no raw error chain is ever formatted inside
/// `write_progress`.
enum ProgressOutcome<'a> {
    Complete,
    Stopped(&'a RoundEnd),
    /// (missing bytes, headroom bytes, free bytes) — for the refusal note.
    Refused(u64, u64, u64),
}

fn write_progress(
    req: &SyncRequest,
    report: &SyncReport,
    outcome: Result<ProgressOutcome<'_>, String>,
) -> anyhow::Result<()> {
    let (complete, outcome_word, note): (bool, &str, Option<String>) = match outcome {
        Ok(ProgressOutcome::Complete) => (true, "complete", None),
        Ok(ProgressOutcome::Stopped(end)) => (
            false,
            "stopped",
            Some(match end {
                RoundEnd::BudgetSpent => {
                    "round budget spent mid-fill; the next round resumes".to_string()
                }
                RoundEnd::DiskBelowThreshold {
                    free_bytes,
                    min_free_bytes,
                } => format!(
                    "free disk fell below the mirror's floor ({free_bytes} free, \
                     floor {min_free_bytes} bytes); this round paused before \
                     writing toward a full disk"
                ),
                // `why` is terminal-only (it can carry a repository path),
                // so the recorded note keeps only the error's *class* — the
                // one privacy boundary this module states for its state
                // files ("no paths", see the module doc).
                RoundEnd::RemoteRead { why } => {
                    let class = crate::remote_err::classify_error_str(why)
                        .map(|kind| kind.label().to_string())
                        // reason: an unclassified read failure has no honest
                        // class word, and inventing a guess ("network",
                        // "credentials") is exactly the over-claim the
                        // three-state rule forbids; "a read error" states
                        // the fact without the chain.
                        .unwrap_or_else(|| "a read error".to_string());
                    format!(
                        "the destination could not be read mid-fill ({class}); the raw \
                         error is not recorded in state files"
                    )
                }
            }),
        ),
        Ok(ProgressOutcome::Refused(missing, headroom, free)) => (
            false,
            "refused",
            Some(format!(
                "not started: the remaining fill is {missing} bytes, the headroom \
                 floor is {headroom} bytes, and only {free} bytes are free"
            )),
        ),
        Err(note) => (false, "failed", Some(note)),
    };
    let progress = MirrorProgress {
        version: MIRROR_PROGRESS_VERSION,
        mirror_of: req.settings.mirror_of.clone(),
        round_finished_at_unix: req.now_unix,
        complete,
        outcome: outcome_word.to_string(),
        outcome_note: note,
        remote_total_bytes: report.remote_total_bytes,
        mirror_bytes: report.present_bytes + report.downloaded_bytes,
        remote_files: report.remote_files,
        mirror_files: report.present_files + report.downloaded_files,
    };
    fs::create_dir_all(req.state_dir)?;
    let path = mirror_progress_path(req.state_dir);
    let tmp = path.with_file_name(format!(".{MIRROR_PROGRESS_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(&progress)?;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// What a read of the progress record can tell us — the same three states
/// every durable record here keeps, so an unreadable progress file can never
/// be read as "never started" or "finished".
#[derive(Debug, Clone)]
pub enum MirrorProgressRead {
    Missing,
    Unreadable(String),
    Present(MirrorProgress),
}

pub fn load_progress(state_dir: &Path) -> MirrorProgressRead {
    let path = mirror_progress_path(state_dir);
    match fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => MirrorProgressRead::Missing,
        Err(e) => MirrorProgressRead::Unreadable(e.kind().to_string()),
        Ok(bytes) => match serde_json::from_slice::<MirrorProgress>(&bytes) {
            Ok(progress) if progress.version == MIRROR_PROGRESS_VERSION => {
                MirrorProgressRead::Present(progress)
            }
            Ok(progress) => {
                MirrorProgressRead::Unreadable(format!("unknown version {}", progress.version))
            }
            Err(e) => MirrorProgressRead::Unreadable(format!("malformed json: {e}")),
        },
    }
}

// ---------------------------------------------------------------------------
// Status surfaces
// ---------------------------------------------------------------------------

/// The mirror's advisory lines for `status` (same family as
/// [`crate::doctor::key_alert_lines`]: a bounded handful of sentences, silent
/// when the machine has no mirror or the mirror is healthy and filled, and
/// never a red over a first fill that is merely large — progress, not
/// failure (ADR-062 C2).
#[must_use]
pub fn mirror_status_lines(config: &Config, state_dir: &Path) -> Vec<String> {
    let Some(settings) = MirrorSettings::for_config(config) else {
        return Vec::new();
    };
    if !settings.enabled {
        return Vec::new();
    }
    let mut lines = Vec::new();
    if !config.destinations.contains_key(&settings.mirror_of) {
        lines.push(format!(
            "[mirror] enabled, but `[mirror] mirror_of = \"{}\"` names no declared destination; \
             declare it under `[destinations.{0}]` or repoint the mirror",
            settings.mirror_of
        ));
        return lines;
    }
    let progress = load_progress(state_dir);
    let state = mirrorstate::load(state_dir);
    // A record that names some other destination is not coverage for this
    // one (mirrorstate.rs's own doc for `mirror_of`): repoint `[mirror]
    // mirror_of` and the old record's closure set answers about a directory
    // this machine does not mirror anymore, so the mismatch is stated and
    // reclaim keeps holding back.
    let state_names_other = matches!(
        &state,
        mirrorstate::MirrorStateRead::Present(record) if record.mirror_of != settings.mirror_of
    );
    if state_names_other {
        let other = match &state {
            mirrorstate::MirrorStateRead::Present(record) => record.mirror_of.clone(),
            _ => String::new(),
        };
        lines.push(format!(
            "[mirror] mirror-state.json names `{other}`, but this machine mirrors `{}`; \
             the record is stale, so coverage is unknown — reclaim must hold sessions \
             back until a completed round rewrites it",
            settings.mirror_of
        ));
    }
    match (&progress, &state) {
        (MirrorProgressRead::Missing, mirrorstate::MirrorStateRead::Missing) => {
            lines.push(format!(
                "[mirror] enabled for `{}` but has never run; start the first fill with \
                 `chat-stasher mirror sync` — it spans several rounds (ADR-062 D2)",
                settings.mirror_of
            ));
        }
        (MirrorProgressRead::Unreadable(why), _) => {
            lines.push(format!(
                "[mirror] the progress record is unreadable ({why}); whether the first \
                 fill is progressing is unknown, not zero — the next round rewrites it"
            ));
        }
        (MirrorProgressRead::Present(progress), mirrorstate::MirrorStateRead::Unreadable(why)) => {
            if progress.complete {
                lines.push(format!(
                    "[mirror] the last round completed but mirror-state.json is unreadable \
                     ({why}); mirror coverage is unknown, so reclaim must hold sessions back \
                     until a completed round rewrites it"
                ));
            } else {
                lines.push(format!(
                    "[mirror] first sync of `{}` in progress: {}% ({} of {}) — reclaim holds \
                     sessions back until it completes; the state record is also unreadable ({why})",
                    progress.mirror_of,
                    progress.percent(),
                    progress.mirror_bytes,
                    progress.remote_total_bytes
                ));
            }
        }
        (MirrorProgressRead::Present(progress), _) => {
            if progress.outcome == "failed" {
                // The one outcome that is not the pause family (invariant 2,
                // `CLAUDE.md`): a round that checked and failed — corrupt
                // source, refused write — reads as the failure it is, with
                // the class sentence the record carries. The paused-line
                // below is deliberately not printed too: its "none of which
                // is a failure" tail would say the opposite in the same
                // breath.
                // reason: a missing note on a failed outcome prints the
                // failure with its percentages and no class sentence — an
                // incomplete sentence, never a fabricated cause.
                let note = progress.outcome_note.clone().unwrap_or_default();
                lines.push(format!(
                    "[mirror] the last mirror round failed at {}% ({} of {}): {note} — the \
                     next round resumes only after the cause is fixed; reclaim holds \
                     sessions back until a round completes",
                    progress.percent(),
                    progress.mirror_bytes,
                    progress.remote_total_bytes
                ));
            } else if !progress.complete {
                lines.push(format!(
                    "[mirror] first sync of `{}` in progress: {}% ({} of {}) — reclaim holds \
                     sessions back until it completes; rounds end by budget, disk floor or \
                     reachability, none of which is a failure",
                    progress.mirror_of,
                    progress.percent(),
                    progress.mirror_bytes,
                    progress.remote_total_bytes
                ));
            }
            // Complete and present: silent, like every healthy surface in
            // this command — [`Self`] exists to make gaps visible, not to
            // narrate health. LM-4 owns the steady-state fields (size, last
            // sync time, behindness) across the three surfaces.
        }
        (MirrorProgressRead::Missing, mirrorstate::MirrorStateRead::Unreadable(why)) => {
            lines.push(format!(
                "[mirror] mirror-state.json is unreadable ({why}), and no progress record \
                 exists; mirror coverage is unknown, so reclaim must hold sessions back"
            ));
        }
        (MirrorProgressRead::Missing, mirrorstate::MirrorStateRead::Present(_)) => {
            // A completed record with no progress record: re-created by the
            // next round; not a state worth alarming on, but worth stating.
            lines.push(
                "[mirror] the progress record is missing while the state record exists; \
                       the next round rewrites both"
                    .to_string(),
            );
        }
    }
    lines
}

/// The `status --json` mirror object (ADR-062 C2): the same facts the prose
/// lines say, in the tagged-`kind` vocabulary the rest of `--json` keeps —
/// an unknown or not-yet-started fill is `unknown`/`not_applicable` with its
/// reason, never a zero percent that reads as measured.
///
/// LM-4 owns the settled field names across the three surfaces; the names
/// here exist so the honest states are on the wire from the first day.
#[must_use]
pub fn status_json(config: &Config, state_dir: &Path) -> serde_json::Value {
    let Some(settings) = MirrorSettings::for_config(config) else {
        return serde_json::json!({
            "configured": false,
            "first_sync": {"kind": "not_applicable", "why": "this machine declares no [mirror] section"},
        });
    };
    let declared = config.destinations.contains_key(&settings.mirror_of);
    let first_sync = if !settings.enabled {
        serde_json::json!({
            "kind": "not_applicable",
            "why": "the [mirror] switch is off",
        })
    } else if !declared {
        serde_json::json!({
            "kind": "not_applicable",
            "why": format!("`mirror_of = \"{}\"` names no declared destination", settings.mirror_of),
        })
    } else {
        match load_progress(state_dir) {
            MirrorProgressRead::Present(progress) => serde_json::json!({
                "kind": "known",
                "percent": progress.percent(),
                "complete": progress.complete,
                "mirror_bytes": progress.mirror_bytes,
                "remote_total_bytes": progress.remote_total_bytes,
                "outcome": progress.outcome,
            }),
            MirrorProgressRead::Unreadable(why) => serde_json::json!({
                "kind": "unknown",
                "why": format!("the progress record is unreadable: {why}"),
            }),
            MirrorProgressRead::Missing => serde_json::json!({
                "kind": "unknown",
                "why": "no sync round has run yet",
            }),
        }
    };
    let closure_record = match mirrorstate::load(state_dir) {
        mirrorstate::MirrorStateRead::Present(state) if state.mirror_of == settings.mirror_of => {
            serde_json::json!({
                "kind": "known",
                "covered_snapshots": state.closure_complete.len(),
                "snapshots_on_remote": state.snapshots_on_remote,
                "finished_at_unix": state.finished_at_unix,
            })
        }
        // A record naming some other destination is not coverage for this
        // one: unknown, with the two names so `--json` says what was
        // compared rather than a bare null.
        mirrorstate::MirrorStateRead::Present(state) => serde_json::json!({
            "kind": "unknown",
            "why": format!(
                "mirror-state.json names `{}`, but this machine mirrors `{}`; the record \
                 is stale, so no snapshot's closure can be claimed for this destination",
                state.mirror_of, settings.mirror_of
            ),
        }),
        mirrorstate::MirrorStateRead::Unreadable(why) => serde_json::json!({
            "kind": "unknown",
            "why": format!("mirror-state.json is unreadable: {why}"),
        }),
        mirrorstate::MirrorStateRead::Missing => serde_json::json!({
            "kind": "unknown",
            "why": "no sync round has completed yet",
        }),
    };
    serde_json::json!({
        "configured": true,
        "enabled": settings.enabled,
        "mirror_of": settings.mirror_of,
        "declared": declared,
        "first_sync": first_sync,
        "closure_record": closure_record,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pushstate;

    fn temp_root() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("vol");
        std::fs::create_dir_all(&root).unwrap();
        (dir, root)
    }

    /// Free-space and capacity return real non-zero numbers for any existing
    /// path on the two platforms this tool ships on, and an error rather
    /// than zero on any other (never "unknown" misread as "no space").
    #[test]
    fn disk_queries_measure_or_refuse() {
        let (_dir, root) = temp_root();
        let available = available_bytes(&root).unwrap();
        let capacity = capacity_bytes(&root).unwrap();
        assert!(available > 0, "a temp dir volume has free space");
        assert!(capacity > 0, "a temp dir volume has capacity");
        assert!(available <= capacity);
        assert!(available_bytes(Path::new("/no/such/path/anywhere")).is_err());
    }

    /// The default floor is max(10 GiB, 5% of capacity) — ruling 6 — and an
    /// explicit value wins; a value below zero is clamped to 0.0 before the
    /// multiply.
    #[test]
    fn min_free_default_is_ten_gib_or_five_percent() {
        let (_dir, root) = temp_root();
        let unset = MirrorSettings {
            enabled: false,
            mirror_of: "storagebox".to_string(),
            min_free_gib: None,
        };
        let floor = unset.min_free_bytes(&root);
        let ten_gib = 10_u64 * 1_073_741_824;
        let capacity = capacity_bytes(&root).unwrap();
        let expected = ten_gib.max(capacity / 20);
        assert_eq!(floor, expected);

        let pinned = MirrorSettings {
            min_free_gib: Some(42.5),
            ..unset
        };
        assert_eq!(
            pinned.min_free_bytes(&root),
            (42.5 * 1_073_741_824.0) as u64
        );
    }

    /// A caller resolves the default floor *before* the round creates the
    /// mirror directory, so the directory need not exist for the default
    /// to mean what its doc says: the capacity is measured on the nearest
    /// existing ancestor — the volume the directory will be created on —
    /// and the pre-round floor equals the in-round one. Without this the
    /// first round enforced two floors: the flat 10 GiB mid-round (its
    /// caller could not query a path that did not exist yet) and the
    /// documented max(10 GiB, 5% of capacity) at the pre-estimate.
    ///
    /// On a volume whose 5% is under the 10 GiB floor the two answers
    /// coincide by arithmetic, so the capacity-relative assertion below
    /// only bites on the large volumes the double floor actually
    /// mis-floored.
    #[test]
    fn the_floor_for_a_mirror_dir_that_does_not_exist_measures_its_future_volume() {
        let (_dir, root) = temp_root();
        let unset = MirrorSettings {
            enabled: false,
            mirror_of: "storagebox".to_string(),
            min_free_gib: None,
        };
        let future = root.join("mirror").join("storagebox");
        assert!(!future.exists());
        assert_eq!(
            unset.min_free_bytes(&future),
            unset.min_free_bytes(&root),
            "the not-yet-created mirror dir is floored by the same volume as \
             its existing ancestor"
        );
        let ten_gib = 10_u64 * 1_073_741_824;
        let five_percent = capacity_bytes(&root).unwrap() / 20;
        if five_percent > ten_gib {
            assert_eq!(unset.min_free_bytes(&future), five_percent);
        }
    }

    /// Local layout matches the backend's own: packs under data/xx/, the
    /// rest under their section dir, config at the root — the copied
    /// directory must open with the same backend that wrote the original.
    #[test]
    fn local_paths_mirror_backend_layout() {
        let root = Path::new("/mirror");
        let mut id_bytes = [0_u8; 32];
        id_bytes[0] = 0xab;
        let hex = "ab".to_string() + &"cd".repeat(31);
        let pack = PlannedFile {
            kind: FileKind::Pack,
            id: Id::from_hex(&hex).unwrap(),
            hex: hex.clone(),
            size: 10,
        };
        assert_eq!(
            local_path(root, &pack),
            root.join("data").join("ab").join(&hex)
        );
        let snapshot = PlannedFile {
            kind: FileKind::Snapshot,
            id: pack.id,
            hex: hex.clone(),
            size: 10,
        };
        assert_eq!(
            local_path(root, &snapshot),
            root.join("snapshots").join(&hex)
        );
        let config = PlannedFile {
            kind: FileKind::Config,
            id: Id::default(),
            hex: String::new(),
            size: 10,
        };
        assert_eq!(local_path(root, &config), root.join("config"));
    }

    /// The progress percentage is "downloaded ÷ total" and stays honest on
    /// the edges: an empty listing is complete (100), never a divide.
    #[test]
    fn progress_percent_is_download_over_total() {
        let progress = MirrorProgress {
            version: MIRROR_PROGRESS_VERSION,
            mirror_of: "storagebox".to_string(),
            round_finished_at_unix: 1,
            complete: false,
            outcome: "stopped".to_string(),
            outcome_note: None,
            remote_total_bytes: 0,
            mirror_bytes: 0,
            remote_files: 0,
            mirror_files: 0,
        };
        assert_eq!(progress.percent(), 100);
        let partial = MirrorProgress {
            remote_total_bytes: 10_000,
            mirror_bytes: 2_500,
            ..progress
        };
        assert_eq!(partial.percent(), 25);
    }

    /// The progress record round-trips and keeps its three read states.
    #[test]
    fn progress_record_round_trips_and_ages_three_states() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        assert!(matches!(
            load_progress(&state_dir),
            MirrorProgressRead::Missing
        ));

        fs::create_dir_all(&state_dir).unwrap();
        fs::write(mirror_progress_path(&state_dir), "not json").unwrap();
        assert!(matches!(
            load_progress(&state_dir),
            MirrorProgressRead::Unreadable(_)
        ));

        let progress = MirrorProgress {
            version: MIRROR_PROGRESS_VERSION,
            mirror_of: "d".to_string(),
            round_finished_at_unix: 5,
            complete: true,
            outcome: "complete".to_string(),
            outcome_note: None,
            remote_total_bytes: 100,
            mirror_bytes: 100,
            remote_files: 3,
            mirror_files: 3,
        };
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            mirror_progress_path(&state_dir),
            serde_json::to_vec_pretty(&progress).unwrap(),
        )
        .unwrap();
        match load_progress(&state_dir) {
            MirrorProgressRead::Present(read_back) => {
                assert_eq!(read_back.percent(), 100);
                assert_eq!(read_back.mirror_files, 3);
            }
            other => panic!("expected present progress, got {other:?}"),
        }
    }

    /// closure_snapshot_ids demands a real mirror-shaped tree (config, keys,
    /// index files) to open: an empty directory yields the empty set *with*
    /// the anomaly sentence, never a covered claim — the exact shape the
    /// reclaim leg needs to hold back instead of deleting.
    #[test]
    fn closure_of_a_non_repository_claims_nothing_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let (covered, anomaly) = closure_snapshot_ids(dir.path(), &MasterKey::new());
        assert!(covered.is_empty());
        assert!(anomaly.is_some());
        let why = anomaly.unwrap_or_default();
        assert!(why.contains("cannot be opened"), "{why}");
    }

    /// The tamper sweep's streaming hash must equal the whole-file hash:
    /// the file names it is compared against are single sha256s, and a
    /// double-hashed sweep (the exact bug fixed alongside this test) made
    /// every present file fail verification and re-download every round.
    #[test]
    fn streaming_tamper_sweep_hash_equals_whole_file_hash() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.bin");
        let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &bytes).unwrap();
        assert_eq!(sha256_file(&p).unwrap(), hex_sha256(&bytes));
    }

    /// MAX_WHOLE_FILE_BYTES' pre-read half fires on the *listing's own*
    /// declared size, before the read allocates anything — the half of
    /// the cap an honestly-declared size cannot escape. The post-read
    /// half, which the constant's doc names as the half an
    /// under-declaring listing meets, catches the served bytes only
    /// after the read and is unreachable through a local file's truthful
    /// metadata. Assertions are on the guard's own class and wording, so
    /// a regression that folds the cap back after the read again (the
    /// pre-fix shape) goes red through the class, not through timing.
    #[test]
    fn an_oversized_listing_is_refused_before_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = StoreConfig {
            repo_root: dir.path().join("repo").to_string_lossy().into_owned(),
            key_file: PathBuf::new(),
            connections: 1,
            options: BTreeMap::new(),
            cache_dir: None,
            no_cache: true,
        };
        let be = BackupStore::for_metadata_query(store)
            .backends()
            .expect("a local-path backend always builds")
            .repository();
        let hex = "ab".to_string() + &"cd".repeat(31);
        let huge = PlannedFile {
            kind: FileKind::Pack,
            id: Id::from_hex(&hex).unwrap(),
            hex: hex.clone(),
            size: MAX_WHOLE_FILE_BYTES + 1,
        };
        match copy_one(dir.path().join("mirror").as_path(), &be, &huge) {
            Err(StopCause::Hard(failure)) => {
                assert_eq!(failure.class, HardClass::OversizeListing);
                assert!(failure.why.contains("listed at"), "{}", failure.why);
                assert!(failure.why.contains("whole-file bound"), "{}", failure.why);
            }
            other => panic!("the over-bound listing must fail hard, got {other:?}"),
        }
    }

    /// The state-file-safe write-class word: an io error's *kind* only, so
    /// the note a failed round records cannot leak the local path the raw
    /// chain carries.
    #[test]
    fn io_class_word_keeps_paths_out_of_state_file_notes() {
        let e = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("create /a/very/private/mirror/dir");
        assert_eq!(io_class_word(&e), "permission denied");
        let word = io_class_word(&e);
        assert!(
            !word.contains("private") && !word.contains('/'),
            "the class word must not carry the error's path: {word}"
        );
        let unmapped = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "unmapped kind",
        ));
        assert_eq!(io_class_word(&unmapped), "an i/o error");
    }

    /// The atomic write leaves no temp file behind and overwrites an
    /// existing target byte-identically.
    #[test]
    fn atomic_writes_leave_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("data").join("ab").join("abcd");
        write_atomic(&target, b"one").unwrap();
        write_atomic(&target, b"two").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"two");
        assert!(!target
            .with_file_name(format!(
                ".{}.tmp",
                target.file_name().unwrap().to_str().unwrap()
            ))
            .exists());
    }

    /// `pushstate`'s default label discipline: the mirror never writes rows
    /// into push-state, this module only reads the pattern — a compile-time
    /// promise the two state files stay separate.
    #[test]
    fn state_files_are_distinct_names() {
        assert_eq!(pushstate::PUSH_STATE_FILE, "push-state.json");
        assert_eq!(mirrorstate::MIRROR_STATE_FILE, "mirror-state.json");
        assert_eq!(MIRROR_PROGRESS_FILE, "mirror-progress.json");
    }
}
