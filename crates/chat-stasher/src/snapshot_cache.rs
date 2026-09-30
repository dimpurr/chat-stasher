//! SRCH-1b — the per-destination **snapshot session cache**.
//!
//! W257 made `search` and `read --session` cumulative over a machine's
//! snapshots (ADR-021): every snapshot of a hostname is walked, newest first,
//! because `reclaim-stage` deletes a session's shard bodies from the stage once
//! every declared destination has proved it holds them. That is correct and it
//! is what makes a reclaimed conversation findable — but it costs one tree walk
//! per snapshot on **every** run. W257 measured 5.8 s for the field archive's
//! 417 snapshots locally, 13.6 ms each; over `opendal:sftp` each walk is a
//! round trip per tree blob, so the same search is tens of seconds.
//!
//! Almost all of that work is repeated. A snapshot is immutable — its id is the
//! hash of its own contents — so what a walk of snapshot *X* found can never
//! change. This module keeps that finding beside the archive, keyed by the
//! snapshot id, so a repeated search only has to walk the snapshots that were
//! not there last time.
//!
//! What is cached, exactly: the **sessions one snapshot's tree holds**, as the
//! cumulative walk buckets them — `(machine, session) -> (shard count, bytes,
//! data blob count)` — plus the archived paths of that snapshot's activity
//! indexes. Nothing else. In particular no conversation body, no shard content
//! and no decrypted byte is stored: the cached numbers come from tree metadata
//! (`node.meta.size`, `node.content.len()`), which is what the walk reads and
//! all it reads (`data_blobs_read` stays 0).
//!
//! The rules this cache is built on:
//!
//! * **It can never change an answer.** A hit is only ever accepted for a
//!   snapshot the repository still lists **and can still read**, and it replaces
//!   a walk that would have produced the same buckets. Every entry is length-
//!   and SHA-256-checked on the way in; a file that fails either check is
//!   deleted and the snapshot is walked again. A corrupt cache costs speed,
//!   never correctness — the same contract [`crate::body_cache`] states for
//!   conversation bodies.
//!
//!   "Can still read" is a second question, and answering it is why an entry
//!   carries [`SnapshotEntry::trees`]: a snapshot id proves its *contents*
//!   cannot change, not that the packs holding them are still in the
//!   destination. A hit whose trees are no longer in the repository's index, or
//!   whose packs are no longer listed, is not used — the snapshot is walked,
//!   exactly as it would have been without this cache, and the walk reports
//!   whatever is really wrong in its own words. The check itself is metadata
//!   only: no tree and no shard is downloaded to make it.
//! * **It is disposable.** Losing the whole directory changes nothing but
//!   timing. Nothing in the archive depends on it, and it is never inside the
//!   archive.
//! * **It only ever holds what is still listed.** The set of snapshots the
//!   repository currently reports is the authority; an entry whose snapshot is
//!   absent from it is deleted, so a pruned archive does not leave a cache
//!   that grows without bound or that could answer about a snapshot nobody can
//!   read.
//! * **Its own files, and only its own.** The root carries a marker file, and a
//!   directory without it is refused rather than filled or emptied, exactly as
//!   in [`crate::body_cache`]. Inside a marked root only names of the shape
//!   `<64 hex>.json` are ever written or deleted.
//!
//! What a hit is **not** allowed to replace is the reading of the activity
//! index. Only the index's *path* is cached; the index itself is dumped and
//! parsed on every run, so "the index is missing", "the index could not be
//! read" and "a line of it is malformed" keep being answered by the snapshot,
//! not by a stale note about the snapshot.
//!
//! Concurrency: a search, the dashboard and the hourly run may overlap, so
//! every write is a temp file plus a rename and every prune only unlinks. There
//! is no cross-file invariant to protect, which is why there is no lock here.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Magic bytes at the start of every entry file.
///
/// Present so a file that is not ours — or a future format — is rejected as a
/// miss instead of being interpreted as a session list.
pub const ENTRY_MAGIC: [u8; 8] = *b"CSSNAP01";

/// `magic (8) + body length (8, little endian) + body sha256 (32)`.
pub const ENTRY_HEADER_LEN: usize = 8 + 8 + 32;

/// One entry file per snapshot, named after the snapshot.
const ENTRY_SUFFIX: &str = ".json";

/// Length of a rustic id's hex spelling (`Id::to_hex`).
const ID_HEX_LEN: usize = 64;

/// The one version this module writes, and the only one it reads back.
///
/// A body carrying anything else is a miss, not a parse attempt: a future
/// format must be free to change a field's meaning without this one silently
/// reading the old bytes under the new rules. Version 2 added [`SnapshotEntry::trees`]
/// — without it a hit could not be judged against the repository's own index,
/// which is the whole of what makes a hit admissible (see [`SnapshotEntry`]).
/// Entries written as version 1 are therefore rebuilt rather than upgraded:
/// they carry no tree list, so nothing could prove them still readable, and a
/// cache that cannot prove that is not allowed to answer at all.
const BODY_VERSION: u32 = 2;

/// Name of the marker file that says a directory is **this cache's** root.
///
/// Dot-prefixed, so it is never mistaken for an entry.
const MARKER_NAME: &str = ".chat-stasher-snapshot-cache";

/// What the marker says. A line-oriented record, so a human who opens the file
/// learns what put it there.
const MARKER_CONTENTS: &[u8] = b"chat-stasher snapshot cache v1\n";

/// The part of the marker that is checked.
///
/// A prefix rather than the whole file, for the same reason as
/// [`crate::body_cache`]'s: a later format may append its own line, and
/// requiring today's bytes exactly would refuse a directory this tool created
/// yesterday.
const MARKER_PREFIX: &[u8] = b"chat-stasher snapshot cache";

/// One session as one snapshot holds it: how many shards, how many bytes, how
/// many data blobs those shards name.
///
/// The snapshot is not a field: the whole entry belongs to one snapshot, and
/// the walk's per-snapshot accounting is what these numbers are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedSession {
    pub machine: String,
    pub session: String,
    pub shard_count: usize,
    pub bytes: u64,
    pub data_blobs: usize,
}

/// One activity index found in a snapshot's tree: the machine it describes and
/// the archived path it sits at.
///
/// The path is stored rather than the file's contents because the index is
/// re-read on every run — see the module docs. The archived path carries the
/// machine's own absolute stage prefix, so it is only ever used to look the
/// file up again in the same snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedIndexFile {
    pub machine: String,
    pub path: String,
}

/// What one snapshot's tree held, as a search walk bucketed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    pub v: u32,
    /// The snapshot this describes. Checked against the file name on read, so a
    /// file that was renamed cannot answer for another snapshot.
    pub snapshot: String,
    /// Sorted by `(machine, session)`, which is the order the walk's own
    /// `BTreeMap` produced — so the bytes are a function of the snapshot alone
    /// and two runs of the same walk write the same file.
    pub sessions: Vec<CachedSession>,
    /// Sorted by `(machine, path)`.
    pub index_files: Vec<CachedIndexFile>,
    /// Every tree blob a walk of this snapshot reads, hex, sorted: the
    /// snapshot's own tree root and the subtree of every directory below it.
    ///
    /// This is what makes a hit admissible rather than merely remembered. An
    /// entry says what the snapshot **held**, and a snapshot's id says that can
    /// never change — but neither says the repository can still *read* it. A
    /// pack can be lost from a destination after the entry was written, and the
    /// uncached path reports exactly that as an unreadable snapshot: a walk
    /// fails on the first tree it cannot fetch, and the answer comes back
    /// partial. A cache that answered from its own bytes alone would report the
    /// same archive as complete, which is the one thing this cache may never do
    /// (CLAUDE.md invariant 1: an unknown must never be recorded as empty).
    ///
    /// With the list here, a hit can be judged the way a walk would decide it,
    /// from metadata only: every one of these ids must still be in the
    /// repository's index, and the pack the index puts it in must still be in
    /// the destination's pack listing. No tree, and no shard, is fetched to
    /// ask — see `search::TreeAvailability`, which asks it.
    pub trees: Vec<String>,
}

impl SnapshotEntry {
    /// An entry for `snapshot` built from the walk's own maps.
    pub fn new(
        snapshot: &str,
        sessions: Vec<CachedSession>,
        index_files: Vec<CachedIndexFile>,
        trees: Vec<String>,
    ) -> Self {
        SnapshotEntry {
            v: BODY_VERSION,
            snapshot: snapshot.to_string(),
            sessions,
            index_files,
            trees,
        }
    }
}

/// A per-destination store of one entry per snapshot, under the OS cache
/// directory.
#[derive(Debug)]
pub struct SnapshotCache {
    root: PathBuf,
    hits: AtomicU64,
    misses: AtomicU64,
    corrupt: AtomicU64,
    pruned: AtomicU64,
}

impl SnapshotCache {
    /// A cache rooted at exactly `root` (also how tests hand it a temp dir).
    pub fn at(root: PathBuf) -> Self {
        SnapshotCache {
            root,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            corrupt: AtomicU64::new(0),
            pruned: AtomicU64::new(0),
        }
    }

    /// The cache for one destination, under the platform cache directory.
    ///
    /// The directory is `<cache_root>/chat-stasher/snapshots/<digest>`, the same
    /// shape `fts::Index::for_destination` uses, so one destination has exactly
    /// one snapshot cache. `identity` is the destination identity the FTS index
    /// is keyed by; two destinations that name the same repository land on the
    /// same directory, which is correct — the entries are keyed by snapshot id,
    /// and a snapshot id names one immutable tree wherever it is read from.
    pub fn for_repository(cache_root: &Path, identity: &str) -> Self {
        let digest = hex(&Sha256::digest(identity.as_bytes()));
        SnapshotCache::at(
            cache_root
                .join("chat-stasher")
                .join("snapshots")
                .join(digest),
        )
    }

    /// The directory entries live in.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Entries served from the cache.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Lookups that found no usable entry — absent, or present and rejected.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Entry files that were present, failed the length or digest check, and
    /// were deleted. A subset of [`Self::misses`].
    ///
    /// Counted separately because it is the only counter that says the cache
    /// was *damaged* rather than merely cold, and "damaged" is a state whose
    /// whole handling is "rebuild it and never trust it".
    pub fn corrupt(&self) -> u64 {
        self.corrupt.load(Ordering::Relaxed)
    }

    /// Entry files deleted because their snapshot is not in the repository's
    /// listing.
    pub fn pruned(&self) -> u64 {
        self.pruned.load(Ordering::Relaxed)
    }

    /// Resolve the cache for `identity`, or `None` when it cannot be used.
    ///
    /// `None` is deliberately silent at the call site: this cache changes no
    /// answer, so a machine with no cache directory is not a state a user has
    /// to be told about — unlike the full-text index, whose absence changes
    /// what a search can *say* (`index build` exists precisely because that is
    /// a state worth naming). The two cases that reach `None` are a platform
    /// with no cache directory at all and a directory at the cache's path that
    /// this tool did not create; in the second nothing is written to it and
    /// nothing in it is deleted.
    ///
    /// The third state, `Err`, is carried to the caller rather than folded into
    /// `None`: a marked root that cannot be written to is a real failure, and a
    /// search that quietly ran uncached while the user believed otherwise would
    /// be indistinguishable from one that was simply cold.
    pub fn for_identity(identity: &str) -> Option<Arc<SnapshotCache>> {
        let cache_root = crate::scanner::user_cache_dirs().into_iter().next()?;
        let cache = SnapshotCache::for_repository(&cache_root, identity);
        match cache.ensure_root() {
            Ok(()) => Some(Arc::new(cache)),
            Err(_) => None,
        }
    }

    /// Make sure the root exists and is this cache's to write to and delete
    /// from.
    ///
    /// Creates it — marker included, atomically — when nothing is there yet.
    /// Refuses anything else, because a directory nobody offered is not a place
    /// to fill with entries or to delete from.
    pub fn ensure_root(&self) -> Result<()> {
        match marker_present(&self.root) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(why) => {
                return Err(anyhow::Error::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{}: {why}", self.root.display()),
                )))
            }
        }
        // Nothing there yet, or something that is not ours. `create_dir` with
        // the marker already inside, then a rename, so no other process ever
        // observes the root without its marker — the shape `body_cache` uses
        // for the same reason.
        let parent = match self.root.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        create_dirs_private(parent).with_context(|| format!("create {}", parent.display()))?;
        let staging = parent.join(format!(".tmp-{}-{}", std::process::id(), next_temp_seq()));
        create_dir_private(&staging).with_context(|| format!("create {}", staging.display()))?;
        private_file(&staging.join(MARKER_NAME), MARKER_CONTENTS)
            .with_context(|| format!("write {MARKER_NAME} in {}", staging.display()))?;
        match fs::rename(&staging, &self.root) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Lost the race to a concurrent creator, or a real failure.
                // Either way: what is at the path now is what decides.
                let _cleaned = fs::remove_dir_all(&staging);
                if marker_present(&self.root) == Ok(true) {
                    return Ok(());
                }
                Err(anyhow::Error::new(e).context(format!(
                    "create snapshot cache root {}",
                    self.root.display()
                )))
            }
        }
    }

    /// Count a rejected entry and remove it, then miss.
    ///
    /// The removal is the "never trusted" half of the contract made durable:
    /// the bytes at this name have been proved not to be what was written
    /// there, so leaving them in place would mean re-reading them on every
    /// later run — and if the snapshot cannot be walked on some later run, the
    /// file would stay there indefinitely, permanently disagreeing with the
    /// repository. A failed unlink is not reported: the miss is what the caller
    /// acts on, and the entry is only a cache.
    fn reject(&self, path: &Path) -> Option<SnapshotEntry> {
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.corrupt.fetch_add(1, Ordering::Relaxed);
        let _removed = fs::remove_file(path);
        None
    }

    /// The entry for `snapshot_id`, or `None` for a miss.
    ///
    /// A miss covers every way an entry can fail to be usable, and all of them
    /// mean the same thing to the caller: walk the snapshot. A file that is
    /// there but does not verify is deleted here, so a damaged cache heals
    /// on the run that notices it rather than being re-examined forever.
    pub fn load(&self, snapshot_id: &str) -> Option<SnapshotEntry> {
        let path = self.root.join(entry_name(snapshot_id)?);
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            Err(_) => {
                // Present but unreadable (permissions, a directory in the
                // way). Not counted as corrupt: nothing here proves the bytes
                // are wrong, only that they could not be looked at.
                self.misses.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let Some(body) = decode(&raw) else {
            return self.reject(&path);
        };
        let entry: SnapshotEntry = match serde_json::from_slice(&body) {
            Ok(entry) => entry,
            Err(_) => return self.reject(&path),
        };
        if entry.v != BODY_VERSION || entry.snapshot != snapshot_id {
            // A valid file that is not an answer about this snapshot: a body
            // from a future format, or a file renamed onto this name. Both are
            // misses; neither is trusted.
            return self.reject(&path);
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(entry)
    }

    /// Write `entry`, replacing any file of that name.
    ///
    /// The write is a temp file plus a rename, so a reader sees the old entry
    /// or the new one and never a half-written one.
    pub fn store(&self, entry: &SnapshotEntry) -> Result<()> {
        self.ensure_root()?;
        let name = entry_name(&entry.snapshot)
            .ok_or_else(|| anyhow::anyhow!("`{}` is not a snapshot id", entry.snapshot))?;
        let body = serde_json::to_vec(entry).context("encode snapshot cache entry")?;
        let path = self.root.join(name);
        write_entry(&path, &encode(&body)).with_context(|| format!("write {}", path.display()))
    }

    /// Delete every entry whose snapshot is **not** in `listed`.
    ///
    /// `listed` is the set of snapshot ids the repository reports right now.
    /// The archive is append-only, so this normally deletes nothing; it exists
    /// so that the cache cannot outlive the listing that justified it — a
    /// snapshot that is gone from the repository must not leave behind an entry
    /// that something could later mistake for a readable snapshot.
    ///
    /// Best-effort, and returns how many files it removed. A file it cannot
    /// unlink is left where it is rather than reported: this is cache hygiene,
    /// and every unread entry is already treated as a miss by [`Self::load`].
    pub fn retain(&self, listed: &BTreeSet<String>) -> usize {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(_) => return 0,
        };
        let mut removed = 0usize;
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Only names this cache writes are ever considered, and only
            // regular files: the marker and any temp file are left alone.
            let Some(id) = entry_stem(name) else { continue };
            if listed.contains(id) {
                continue;
            }
            if fs::remove_file(entry.path()).is_ok() {
                removed += 1;
            }
        }
        if removed > 0 {
            self.pruned.fetch_add(removed as u64, Ordering::Relaxed);
        }
        removed
    }
}

/// The file name one snapshot's entry is stored under, or `None` when `id` is
/// not the shape of a snapshot id.
///
/// Validating here is what keeps a cache miss from becoming a path traversal:
/// the id arrives from the repository, but the name is built from it, and only
/// 64 hex characters ever build one.
fn entry_name(id: &str) -> Option<String> {
    if !is_id_hex(id) {
        return None;
    }
    Some(format!("{id}{ENTRY_SUFFIX}"))
}

/// The snapshot id an entry file name carries, or `None` when the name is not
/// one this cache writes.
fn entry_stem(name: &str) -> Option<&str> {
    let stem = name.strip_suffix(ENTRY_SUFFIX)?;
    if is_id_hex(stem) {
        Some(stem)
    } else {
        None
    }
}

/// Whether `raw` is exactly the 64 lowercase hex characters `Id::to_hex`
/// produces.
fn is_id_hex(raw: &str) -> bool {
    raw.len() == ID_HEX_LEN
        && raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether the directory at `root` carries this cache's marker.
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

/// Write one entry: temp file in the root, then rename into place.
///
/// The rename is what makes this safe without a lock — a reader either sees the
/// old file or the new one, never a half-written one.
fn write_entry(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "entry path has no parent")
    })?;
    // Dot-prefixed and pid-suffixed: dot-prefixed so a walk of the root cannot
    // mistake it for an entry, pid-suffixed so two processes never share one.
    let candidate = parent.join(format!(".tmp-{}-{}", std::process::id(), next_temp_seq()));
    let mut file = create_private_file(&candidate)?;
    file.write_all(payload)?;
    // Set here and not only at creation: these bytes are a session list in
    // plaintext, and a temp file this process inherited with looser bits would
    // otherwise be the mode the entry ends up with, since the rename carries
    // the inode across.
    set_file_private(&candidate)?;
    drop(file);
    match fs::rename(&candidate, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // A real failure, or another process removed the temp file between
            // create and rename. Either way: not stored, and the temp file is
            // dropped if it is still ours.
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

/// `magic | length | sha256(body) | body`.
fn encode(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ENTRY_HEADER_LEN + body.len());
    out.extend_from_slice(&ENTRY_MAGIC);
    out.extend_from_slice(&(body.len() as u64).to_le_bytes());
    out.extend_from_slice(&Sha256::digest(body));
    out.extend_from_slice(body);
    out
}

/// The body of an entry file, or `None` when the bytes are not one this cache
/// wrote, or do not survive being re-hashed.
///
/// The digest is checked, not merely stored: this is the check that makes a
/// damaged file a miss rather than a wrong answer, because the bytes that reach
/// the parser are the bytes that were verified.
///
/// The declared length is a number **this file chose**, so it is arithmetic on
/// untrusted input and is done with checked operations: a length that does not
/// fit a `usize`, or that runs past the end of what was read, is a rejection —
/// never a panic, and never a slice. The body must also end exactly at the end
/// of the file: a file with bytes after the body is not the file this cache
/// wrote, and the digest covers the body alone, so accepting one would let an
/// unverified tail ride along inside a verified entry.
fn decode(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() < ENTRY_HEADER_LEN || raw[..8] != ENTRY_MAGIC {
        return None;
    }
    let declared = u64::from_le_bytes(raw[8..16].try_into().ok()?);
    let len = usize::try_from(declared).ok()?;
    let end = ENTRY_HEADER_LEN.checked_add(len)?;
    if end != raw.len() {
        return None;
    }
    let expected = &raw[16..48];
    let body = raw.get(ENTRY_HEADER_LEN..end)?;
    if Sha256::digest(body).as_slice() != expected {
        return None;
    }
    Some(body.to_vec())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---- owner-only permissions -----------------------------------------------
//
// An entry is a **plaintext** list of session ids, machine ids and archived
// stage paths — the same class of material the FTS index holds, and it is made
// owner-only for the same reason. The mode is asked for explicitly at creation
// and then set again, because a mode asked for at creation is filtered through
// the process umask (so `0o600` under `umask 022` is `0o600` but under a umask
// that strips owner bits is not what was asked for), and because a file that
// already exists is not re-created by `create`.
//
// On a platform with no mode bits these are no-ops: the property does not exist
// there, and the OS cache directory the root lives under is already the user's
// own. See `crate::fts`, which states the same for the index database.

#[cfg(unix)]
fn set_file_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_dir_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_dir_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Create one directory, owner-only.
///
/// `mode` is passed to `mkdir`, so the umask gets a say in it — which is why
/// the mode is set on the result as well, exactly as for a file.
#[cfg(unix)]
fn create_dir_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)?;
    set_dir_private(path)
}

#[cfg(not(unix))]
fn create_dir_private(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}

/// Create `path` and every missing directory above it, each owner-only.
///
/// `create_dir_all` cannot do this: it makes every component with the umask's
/// mode, so the cache's own directories would be as readable as the machine's
/// default. Only the components this call creates are given a mode — a
/// directory that is already there (`~/.cache`, say) is left exactly as it is,
/// because it is not this cache's to change.
fn create_dirs_private(path: &Path) -> std::io::Result<()> {
    let mut missing: Vec<&Path> = Vec::new();
    let mut cursor = Some(path);
    while let Some(dir) = cursor {
        if dir.as_os_str().is_empty() || dir.exists() {
            break;
        }
        missing.push(dir);
        cursor = dir.parent();
    }
    // Topmost first, so each level is created inside one that already exists.
    for dir in missing.iter().rev() {
        match create_dir_private(dir) {
            Ok(()) => {}
            // Another process created it between the check above and now: the
            // same race `ensure_root`'s rename handles, and not an error.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Create `path` for writing, owner-only from the first byte.
#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<File> {
    File::create(path)
}

/// Write `contents` to `path`, owner-only.
fn private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut file = create_private_file(path)?;
    file.write_all(contents)?;
    set_file_private(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(byte: u8) -> String {
        format!("{byte:02x}").repeat(ID_HEX_LEN / 2)
    }

    fn entry(snapshot: &str) -> SnapshotEntry {
        SnapshotEntry::new(
            snapshot,
            vec![CachedSession {
                machine: "m".to_string(),
                session: "s".to_string(),
                shard_count: 2,
                bytes: 40,
                data_blobs: 1,
            }],
            vec![CachedIndexFile {
                machine: "m".to_string(),
                path: "/stage/meta/m/activity-v1.jsonl".to_string(),
            }],
            // Two trees, because a snapshot's walk reads its root and at least
            // one directory under it — the shape every entry this cache writes
            // has, so the round-trip test above is a round trip of the real
            // thing.
            vec![id(0x01), id(0x02)],
        )
    }

    /// A stored entry comes back byte-identical, and is counted as a hit.
    #[test]
    fn a_stored_entry_reads_back() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = SnapshotCache::at(dir.path().join("snapshots"));
        let snapshot = id(0xab);
        cache.store(&entry(&snapshot)).unwrap();
        let loaded = cache.load(&snapshot).expect("a stored entry is a hit");
        assert_eq!(loaded, entry(&snapshot));
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 0);
        assert_eq!(cache.corrupt(), 0);
    }

    /// An entry file whose bytes changed under it is a miss, not a wrong
    /// answer, and it is deleted so the next run rebuilds it.
    #[test]
    fn a_damaged_entry_is_a_miss_and_is_removed() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = SnapshotCache::at(dir.path().join("snapshots"));
        let snapshot = id(0xcd);
        cache.store(&entry(&snapshot)).unwrap();
        let path = cache.root().join(format!("{snapshot}{ENTRY_SUFFIX}"));
        let mut raw = fs::read(&path).unwrap();
        // Flip one byte of the body, leaving the length and magic intact: the
        // digest is the only thing that can catch this.
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        fs::write(&path, &raw).unwrap();
        assert!(
            cache.load(&snapshot).is_none(),
            "damaged bytes are not read"
        );
        assert_eq!(cache.corrupt(), 1);
        assert_eq!(cache.hits(), 0);
        assert!(
            fs::metadata(&path).is_err(),
            "a damaged entry is removed, not left to be re-read"
        );
    }

    /// A file that was renamed onto another snapshot's name answers nothing.
    #[test]
    fn an_entry_named_for_another_snapshot_is_a_miss() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = SnapshotCache::at(dir.path().join("snapshots"));
        let written = id(0x01);
        cache.store(&entry(&written)).unwrap();
        let other = id(0x02);
        fs::rename(
            cache.root().join(format!("{written}{ENTRY_SUFFIX}")),
            cache.root().join(format!("{other}{ENTRY_SUFFIX}")),
        )
        .unwrap();
        assert!(cache.load(&other).is_none());
        assert_eq!(cache.corrupt(), 1);
    }

    /// An entry whose snapshot is no longer listed is deleted; one that is
    /// listed and one foreign file are both left alone.
    #[test]
    fn retain_deletes_only_unlisted_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let cache = SnapshotCache::at(dir.path().join("snapshots"));
        let kept = id(0x11);
        let dropped = id(0x22);
        cache.store(&entry(&kept)).unwrap();
        cache.store(&entry(&dropped)).unwrap();
        let foreign = cache.root().join("notes.txt");
        fs::write(&foreign, b"not ours to delete").unwrap();
        let listed: BTreeSet<String> = [kept.clone()].into_iter().collect();
        assert_eq!(cache.retain(&listed), 1);
        assert_eq!(cache.pruned(), 1);
        assert!(cache.load(&kept).is_some());
        assert!(!cache
            .root()
            .join(format!("{dropped}{ENTRY_SUFFIX}"))
            .exists());
        assert!(foreign.exists(), "a file this cache did not write is kept");
    }

    /// A directory that exists and is not this cache's is refused rather than
    /// filled: `write_entry` would replace whatever was at the entry's name.
    #[test]
    fn a_directory_without_the_marker_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("snapshots");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("somebody-elses-file"), b"x").unwrap();
        let cache = SnapshotCache::at(root.clone());
        assert!(cache.store(&entry(&id(0x33))).is_err());
        assert!(cache.load(&id(0x33)).is_none());
        assert!(root.join("somebody-elses-file").exists());
    }

    /// The name built from a snapshot id is always one this cache writes, and
    /// nothing that is not an id builds a name at all.
    #[test]
    fn only_snapshot_ids_build_entry_names() {
        assert!(entry_name(&id(0x00)).is_some());
        assert!(entry_name("").is_none());
        assert!(entry_name("../escape").is_none());
        assert!(entry_name(&"A".repeat(ID_HEX_LEN)).is_none(), "upper case");
        assert!(entry_name(&"g".repeat(ID_HEX_LEN)).is_none(), "not hex");
        assert!(entry_name(&"a".repeat(ID_HEX_LEN - 1)).is_none(), "short");
        assert_eq!(
            entry_stem(&format!("{}{ENTRY_SUFFIX}", id(0x7f))),
            Some(id(0x7f)).as_deref()
        );
        assert!(entry_stem(MARKER_NAME).is_none());
        assert!(entry_stem(".tmp-1-2").is_none());
    }

    /// The header check rejects truncated files, a wrong magic and a wrong
    /// length before any parser sees the bytes.
    #[test]
    fn decode_rejects_anything_but_a_verified_body() {
        let body = b"hello";
        let good = encode(body);
        assert_eq!(decode(&good).as_deref(), Some(&body[..]));
        assert!(decode(&good[..ENTRY_HEADER_LEN - 1]).is_none(), "truncated");
        let mut wrong_magic = good.clone();
        wrong_magic[0] ^= 0xff;
        assert!(decode(&wrong_magic).is_none());
        let mut wrong_len = good.clone();
        wrong_len[8] = wrong_len[8].wrapping_add(1);
        assert!(decode(&wrong_len).is_none());
    }

    /// A file with bytes after the declared body is not an entry, even though
    /// its body alone verifies.
    ///
    /// The digest is over the body and the length says where the body ends, so
    /// a tail is bytes nothing here verified — the exact-file integrity claim
    /// is that an accepted file is one this cache wrote, from its first byte to
    /// its last.
    #[test]
    fn decode_rejects_a_trailing_byte() {
        let body = b"hello";
        let mut raw = encode(body);
        assert!(decode(&raw).as_deref() == Some(&body[..]));
        raw.push(0x00);
        assert!(
            decode(&raw).is_none(),
            "a body that verifies with bytes after it is not a file this cache wrote"
        );
        // The same tail, and a length that was grown to cover it: now the
        // digest is the thing that fails, so neither route admits it.
        let mut grown = encode(body);
        grown[8] = 6;
        grown.push(0x00);
        assert!(decode(&grown).is_none());
    }

    /// A length that cannot be an address is rejected rather than added to the
    /// header length.
    ///
    /// Nothing checks the declared length before it is used, so a file that
    /// says it carries `u64::MAX` bytes must come back as a miss. On a 64-bit
    /// machine the addition itself is what would wrap, which is the panic this
    /// pins; on a 32-bit one the conversion refuses first. Either way the
    /// caller sees a miss and rebuilds — never a crash, and never an entry
    /// parsed from a body that was never there.
    #[test]
    fn decode_rejects_a_length_that_is_not_an_address() {
        let body = b"hello";
        let mut raw = encode(body);
        raw[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode(&raw).is_none(), "an impossible length is a miss");
        // The header length itself, so the sum is exactly one past the end of
        // what `usize` can hold on any platform that can hold this file.
        raw[8..16].copy_from_slice(&(usize::MAX as u64).to_le_bytes());
        assert!(decode(&raw).is_none());
        // And the largest length that can still be added to the header without
        // wrapping, which is simply more bytes than are present.
        raw[8..16].copy_from_slice(&((usize::MAX - ENTRY_HEADER_LEN) as u64).to_le_bytes());
        assert!(decode(&raw).is_none());
    }

    /// The root, the marker and an entry file are owner-only.
    ///
    /// The entry file is checked after a rewrite too, which is the case a mode
    /// asked for only at creation would miss: `create` does not re-mode a file
    /// that is already there, and `write_entry` writes into a name this process
    /// chose but a previous run may have left behind.
    ///
    /// The property is POSIX mode bits, so it exists on unix and does not exist
    /// on Windows, where the OS cache directory is already the user's own and
    /// the platform's ACLs come from there. That is stated rather than silently
    /// skipped: there is nothing to assert on the other side, not a second
    /// behaviour that is being left untested.
    #[cfg(unix)]
    #[test]
    fn the_root_the_marker_and_entries_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;

        let dir = tempfile::TempDir::new().unwrap();
        // A directory that is already there with bits this cache did not choose
        // — `~/.cache`, and everything above it — and, below it, the two
        // levels this cache does create.
        let existing = dir.path().join("existing");
        fs::create_dir(&existing).unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o755)).unwrap();
        let cache = SnapshotCache::at(existing.join("a").join("snapshots"));
        let snapshot = id(0x5a);
        cache.store(&entry(&snapshot)).unwrap();

        assert_eq!(mode(cache.root()), 0o700, "the cache root is owner-only");
        assert_eq!(
            mode(&cache.root().join(MARKER_NAME)),
            0o600,
            "the marker is owner-only"
        );
        let entry_file = cache.root().join(format!("{snapshot}{ENTRY_SUFFIX}"));
        assert_eq!(mode(&entry_file), 0o600, "an entry is owner-only");

        // A stale entry file with loose bits must be tightened by the rewrite
        // rather than inherited: this is the `create` on an existing name.
        fs::set_permissions(&entry_file, fs::Permissions::from_mode(0o644)).unwrap();
        cache.store(&entry(&snapshot)).unwrap();
        assert_eq!(
            mode(&entry_file),
            0o600,
            "a rewrite must leave the entry owner-only"
        );

        // The intermediate directory the cache created is owner-only, and the
        // one that was already there is exactly as it was left.
        assert_eq!(mode(&existing.join("a")), 0o700);
        assert_eq!(
            mode(&existing),
            0o755,
            "a directory this cache did not create must not be changed"
        );
    }
}
