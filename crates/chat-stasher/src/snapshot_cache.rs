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
//!   snapshot the repository still lists, and it replaces a walk that would have
//!   produced the same buckets. Every entry is length- and SHA-256-checked on
//!   the way in; a file that fails either check is deleted and the snapshot is
//!   walked again. A corrupt cache costs speed, never correctness — the same
//!   contract [`crate::body_cache`] states for conversation bodies.
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
/// reading the old bytes under the new rules.
const BODY_VERSION: u32 = 1;

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
}

impl SnapshotEntry {
    /// An entry for `snapshot` built from the walk's own maps.
    pub fn new(
        snapshot: &str,
        sessions: Vec<CachedSession>,
        index_files: Vec<CachedIndexFile>,
    ) -> Self {
        SnapshotEntry {
            v: BODY_VERSION,
            snapshot: snapshot.to_string(),
            sessions,
            index_files,
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
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let staging = parent.join(format!(".tmp-{}-{}", std::process::id(), next_temp_seq()));
        fs::create_dir(&staging).with_context(|| format!("create {}", staging.display()))?;
        fs::write(staging.join(MARKER_NAME), MARKER_CONTENTS)
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
    let mut file = File::create(&candidate)?;
    file.write_all(payload)?;
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
fn decode(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() < ENTRY_HEADER_LEN || raw[..8] != ENTRY_MAGIC {
        return None;
    }
    let len = u64::from_le_bytes(raw[8..16].try_into().ok()?) as usize;
    let expected = &raw[16..48];
    let body = raw.get(ENTRY_HEADER_LEN..ENTRY_HEADER_LEN + len)?;
    if Sha256::digest(body).as_slice() != expected {
        return None;
    }
    Some(body.to_vec())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
}
