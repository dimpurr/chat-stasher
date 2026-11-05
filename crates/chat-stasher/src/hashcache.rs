//! Hash results remembered by file identity (ISSUE-008 C, W1083).
//!
//! Every hour this collector re-proves two things that nearly always still
//! hold: that a source JSONL's committed prefix still hashes to what the
//! stored cursor recorded, and that the stage's sealed shards still hash to
//! what a debt claims. Those proofs re-read and re-hashed gigabytes of bytes
//! that did not move — a cost proportional to the archive's total size, not
//! to the hour's change (ISSUE-008 §2: ~7.7 GB of source prefix
//! re-validation plus the stage-side shard re-reads, every round).
//!
//! This cache removes the *re-reading* while keeping the proof. It records,
//! per file, the digest and the file's own identity — size, mtime and
//! inode — as they were when the digest was last computed by actually
//! reading the bytes. On a later pass the file is stat'ed first: the same
//! identity means no write touched the file since (a write moves the size,
//! an in-place rewrite that keeps the size still moves the mtime — and the
//! identity carries all three), so the recorded digest still describes
//! those exact bytes. A different identity — or a file the cache has never
//! seen, or a cache file that cannot be read — is always a miss: the bytes
//! are re-read and re-hashed exactly as before. "Unknown" is never treated
//! as "still the same"; it is treated as "no shortcut".
//!
//! What the cache is trusted for is deliberately narrow: deciding that
//! bytes this pass did not need to re-read still hash to what they hashed
//! to when they *were* read. It never fabricates a digest for bytes nobody
//! ever read — every stored digest was computed from the real bytes at
//! store time — so deleting this cache (`hash-cache-v1.json` in the state
//! directory) only ever costs one pass of re-reading, never correctness.
//! Sealing, pushing and `verify` keep hashing the real bytes on their own
//! paths; none of them consult this cache.
//!
//! The trade it makes is the one ISSUE-008 §6 records: a rewrite that
//! keeps the same size, the same mtime and the same inode is the one shape
//! nothing can distinguish from "unchanged" by metadata — the same shape
//! the whole-store fingerprint shortcut already accepted for quiet SQLite
//! stores.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// State-directory file holding the cache. A missing or wrong-version file
/// is an empty cache (fail-open: every lookup misses).
pub const CACHE_FILE: &str = "hash-cache-v1.json";

const CACHE_VERSION: u32 = 1;

/// Above this many entries the next save retains only the entries the pass
/// actually touched, so a long-lived state directory cannot grow the file
/// without bound — reclaimed sessions leave stale entries no future pass
/// will touch again, and the cap is a bounded reset, not silent eviction of
/// live entries.
const CACHE_MAX_ENTRIES: usize = 250_000;

/// Streaming chunk: the digests are computed over byte streams the size of a
/// session body, and this path never buffers more than one chunk where the
/// pre-cache code materialized whole prefixes.
const HASH_CHUNK: usize = 256 * 1024;

/// One hash lookup's answer: the digest, what it cost, whether it came
/// from the cache, and the file's full size as the stat saw it (the stats
/// the pass's accounting wants, including on a hit, where `bytes_read` is
/// zero but the file still has a size).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashAnswer {
    pub sha256: String,
    pub bytes_read: u64,
    pub from_cache: bool,
    pub size: u64,
}

/// Hits, misses and bytes read at one call-site family, so the pass can keep
/// the source-prefix and the stage-shard tallies apart (the stage side is
/// the S3 counter, the source side folds into the pre-existing
/// prefix-validation counters).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HashCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub bytes_read: u64,
}

impl HashCacheStats {
    fn merge(self, other: Self) -> Self {
        HashCacheStats {
            hits: self.hits + other.hits,
            misses: self.misses + other.misses,
            bytes_read: self.bytes_read + other.bytes_read,
        }
    }
}

/// One file's stat identity: what a write is guaranteed to move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileIdentity {
    size: u64,
    /// Nanoseconds since the epoch; `None` means the mtime could not be
    /// taken as a forward epoch time, in which case the file is never
    /// cacheable (never stored, never a hit), only hashed.
    mtime_ns: Option<u64>,
    inode: u64,
}

impl FileIdentity {
    fn of(metadata: &fs::Metadata) -> Self {
        let mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|moment| {
                moment
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
            });
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            metadata.ino()
        };
        // reason: Windows exposes no stable per-file inode number in std;
        // nothing on that platform stores a nonzero one, so all entries
        // there share the same field value and size/mtime still do the
        // distinguishing.
        #[cfg(not(unix))]
        let inode = 0;
        FileIdentity {
            size: metadata.len(),
            mtime_ns,
            inode,
        }
    }
}

/// One sealed shard as a directory entry records it: sequence, path and the
/// file identity, so a cache hit means every constituent file is unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ShardIdentity {
    sequence: u64,
    path: String,
    #[serde(flatten)]
    file: FileIdentity,
}

/// What one cache key remembers. The variant fixes the *question* the entry
/// answers, so a "whole file" answer can never be served to a "prefix"
/// question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
enum CacheValue {
    /// sha256 of the whole file's bytes.
    #[serde(rename = "file")]
    File {
        #[serde(flatten)]
        file: FileIdentity,
        sha256: String,
    },
    /// sha256 of exactly the file's first `offset` bytes.
    #[serde(rename = "prefix")]
    Prefix {
        #[serde(flatten)]
        file: FileIdentity,
        offset: u64,
        sha256: String,
    },
    /// sha256 of the concatenation (sequence order) of `files`' first
    /// `covered` bytes — the stage-coverage digest. Stored only when the
    /// read produced exactly `covered` bytes.
    #[serde(rename = "dir")]
    Dir {
        files: Vec<ShardIdentity>,
        covered: u64,
        sha256: String,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: BTreeMap<String, CacheValue>,
}

/// The cache. Constructed once per pass — [`HashCache::load`] from the state
/// directory, [`HashCache::disabled`] where caching is out of scope — used
/// from the read paths, then persisted once per pass ([`HashCache::save`]).
pub struct HashCache {
    path: Option<PathBuf>,
    entries: BTreeMap<String, CacheValue>,
    touched: HashSet<String>,
    source: HashCacheStats,
    stage: HashCacheStats,
}

impl Default for HashCache {
    fn default() -> Self {
        HashCache::disabled()
    }
}

impl HashCache {
    /// The pass's cache, from the state directory. A missing file is a cold
    /// cache; an unreadable or wrong-version one is discarded rather than
    /// trusted — an unknown cache is every-entry-miss, never a stale hit.
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(CACHE_FILE);
        let entries = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<CacheFile>(&bytes) {
                Ok(file) if file.version == CACHE_VERSION => file.entries,
                _ => BTreeMap::new(),
            },
            Err(_) => BTreeMap::new(),
        };
        HashCache {
            path: Some(path),
            entries,
            touched: HashSet::new(),
            source: HashCacheStats::default(),
            stage: HashCacheStats::default(),
        }
    }

    /// A cache that never stores and never hits: the callers with no pass
    /// state of their own (one-shot import legs, isolated read-path tests)
    /// keep today's read-everything behaviour.
    pub fn disabled() -> Self {
        HashCache {
            path: None,
            entries: BTreeMap::new(),
            touched: HashSet::new(),
            source: HashCacheStats::default(),
            stage: HashCacheStats::default(),
        }
    }

    /// The source-prefix tallies (committed-prefix validation of sources).
    pub fn source_stats(&self) -> HashCacheStats {
        self.source
    }

    /// The stage-shard tallies (`dir_covers`, stage shard facts).
    pub fn stage_stats(&self) -> HashCacheStats {
        self.stage
    }

    /// Both tallies merged — the module's own tests and the S4 print line.
    pub fn stats(&self) -> HashCacheStats {
        self.source.merge(self.stage)
    }

    fn is_same_file(stored: &FileIdentity, current: &FileIdentity) -> bool {
        stored.mtime_ns.is_some()
            && stored.size == current.size
            && stored.mtime_ns == current.mtime_ns
            && stored.inode == current.inode
    }


    /// sha256 of the file's first `offset` bytes — the committed prefix a
    /// stored JSONL cursor re-proves each pass. `stat` is the metadata the
    /// caller already took, so answering costs no extra stat; `None` and
    /// the file is stat'ed here. An `offset` past the end hashes fewer
    /// bytes and therefore disagrees with any digest recorded over exactly
    /// `offset` bytes — the honest answer, and the entry is not stored.
    pub fn prefix_sha256(
        &mut self,
        path: &Path,
        offset: u64,
        stat: Option<&fs::Metadata>,
    ) -> anyhow::Result<HashAnswer> {
        let metadata = match stat {
            Some(metadata) => metadata.clone(),
            None => fs::metadata(path)
                .with_context(|| format!("stat hashed prefix {}", path_digest(path)))?,
        };
        let identity = FileIdentity::of(&metadata);
        let key = format!("p:{}", path_digest(path));
        if let Some(CacheValue::Prefix {
            file,
            offset: cached_offset,
            sha256,
        }) = self.entries.get(&key)
        {
            if *cached_offset == offset && Self::is_same_file(file, &identity) {
                self.source.hits += 1;
                self.touched.insert(key);
                return Ok(HashAnswer {
                    sha256: sha256.clone(),
                    bytes_read: 0,
                    from_cache: true,
                    size: metadata.len(),
                });
            }
        }
        self.source.misses += 1;
        let digest = sha256_of_len(path, offset)?;
        self.source.bytes_read += digest.bytes_read;
        if identity.mtime_ns.is_some() && digest.bytes_read == offset {
            self.touched.insert(key.clone());
            self.entries.insert(
                key,
                CacheValue::Prefix {
                    file: identity,
                    offset,
                    sha256: digest.sha256.clone(),
                },
            );
        }
        Ok(HashAnswer {
            sha256: digest.sha256,
            bytes_read: digest.bytes_read,
            from_cache: false,
            size: metadata.len(),
        })
    }

    /// sha256 of one file's whole bytes (a sealed shard).
    pub fn file_sha256(
        &mut self,
        path: &Path,
        stat: Option<&fs::Metadata>,
    ) -> anyhow::Result<HashAnswer> {
        let metadata = match stat {
            Some(metadata) => metadata.clone(),
            None => fs::metadata(path)
                .with_context(|| format!("stat hashed file {}", path_digest(path)))?,
        };
        let identity = FileIdentity::of(&metadata);
        let key = format!("f:{}", path_digest(path));
        if let Some(CacheValue::File { file, sha256 }) = self.entries.get(&key) {
            if Self::is_same_file(file, &identity) {
                self.stage.hits += 1;
                self.touched.insert(key);
                return Ok(HashAnswer {
                    sha256: sha256.clone(),
                    bytes_read: 0,
                    from_cache: true,
                    size: metadata.len(),
                });
            }
        }
        self.stage.misses += 1;
        let digest = sha256_of_len(path, u64::MAX)?;
        self.stage.bytes_read += digest.bytes_read;
        if identity.mtime_ns.is_some() {
            self.touched.insert(key.clone());
            self.entries.insert(
                key,
                CacheValue::File {
                    file: identity,
                    sha256: digest.sha256.clone(),
                },
            );
        }
        Ok(HashAnswer {
            sha256: digest.sha256,
            bytes_read: digest.bytes_read,
            from_cache: false,
            size: metadata.len(),
        })
    }

    /// sha256 over the concatenation (in the given sequence order) of the
    /// shards' first `covered` bytes — the `dir_covers` prefix test. The
    /// caller supplies the listing it already enumerated; this function
    /// stats each shard, so a hit means every constituent file is
    /// unchanged. A read that runs out of bytes before `covered` is
    /// reported as the short read it is — and not stored.
    pub fn shard_concat_sha256(
        &mut self,
        dir: &Path,
        shards: &[(u64, PathBuf)],
        covered: u64,
    ) -> anyhow::Result<HashAnswer> {
        let key = format!("d:{}", path_digest(dir));
        let mut identities = Vec::with_capacity(shards.len());
        for (sequence, path) in shards {
            let metadata = fs::metadata(path)
                .with_context(|| format!("stat hashed shard {}", path_digest(path)))?;
            identities.push(ShardIdentity {
                sequence: *sequence,
                path: path_digest(path),
                file: FileIdentity::of(&metadata),
            });
        }
        if let Some(CacheValue::Dir {
            files,
            covered: cached_covered,
            sha256,
        }) = self.entries.get(&key)
        {
            if *cached_covered == covered && *files == identities {
                self.stage.hits += 1;
                self.touched.insert(key);
                let size = identities.iter().map(|shard| shard.file.size).sum();
                return Ok(HashAnswer {
                    sha256: sha256.clone(),
                    bytes_read: 0,
                    from_cache: true,
                    size,
                });
            }
        }
        self.stage.misses += 1;
        // One streaming digest across the shard files: a concatenation's
        // hash is over the joined bytes, never over the pieces' digests.
        let mut hasher = Sha256::new();
        let mut bytes_read = 0u64;
        for (_, path) in shards {
            if bytes_read >= covered {
                break;
            }
            let mut file = fs::File::open(path)
                .with_context(|| format!("open hashed shard {}", path_digest(path)))?;
            let mut chunk = vec![0u8; HASH_CHUNK];
            while bytes_read < covered {
                let want = chunk
                    .len()
                    .min(usize::try_from(covered - bytes_read).unwrap_or(chunk.len()));
                let read = file.read(&mut chunk[..want])?;
                if read == 0 {
                    break;
                }
                hasher.update(&chunk[..read]);
                bytes_read += read as u64;
            }
        }
        let sha256 = hex_lower(&hasher.finalize());
        self.stage.bytes_read += bytes_read;
        let complete = bytes_read == covered;
        let size = identities.iter().map(|shard| shard.file.size).sum();
        if complete {
            self.touched.insert(key.clone());
            self.entries.insert(
                key,
                CacheValue::Dir {
                    files: identities,
                    covered,
                    sha256: sha256.clone(),
                },
            );
        }
        Ok(HashAnswer {
            sha256,
            bytes_read,
            from_cache: false,
            size,
        })
    }

    /// Persist once per pass: tmp + fsync + rename, the durability the debt
    /// state file uses. A pass that touched nothing writes nothing, so an
    /// untouched pass leaves the file exactly as it was.
    pub fn save(&mut self) -> anyhow::Result<()> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        if self.touched.is_empty() {
            return Ok(());
        }
        if self.entries.len() > CACHE_MAX_ENTRIES {
            let touched = std::mem::take(&mut self.touched);
            self.entries.retain(|key, _value| touched.contains(key));
            self.touched = touched;
        }
        let out = CacheFile {
            version: CACHE_VERSION,
            entries: std::mem::take(&mut self.entries),
        };
        let bytes = serde_json::to_vec(&out).context("serialise hash cache")?;
        let tmp = path.with_file_name(format!(".{CACHE_FILE}.tmp"));
        {
            let mut file = fs::File::create(&tmp)
                .with_context(|| format!("create hash cache tmp {}", tmp.display()))?;
            std::io::Write::write_all(&mut file, &bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        self.entries = out.entries;
        Ok(())
    }
}

/// A raw streaming digest: hash the first `wanted` bytes of `path` (`u64::MAX`
/// reads to EOF). Never buffers more than one chunk. `bytes_read` says what
/// the file actually produced, so callers can tell "the prefix really is
/// `offset` bytes" from "the file ran out first".
fn sha256_of_len(path: &Path, wanted: u64) -> anyhow::Result<HashAnswer> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("open hashed file {}", path_digest(path)))?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; HASH_CHUNK];
    let mut remaining = wanted;
    let mut bytes_read = 0u64;
    loop {
        let want = chunk
            .len()
            .min(usize::try_from(remaining).unwrap_or(chunk.len()));
        if want == 0 {
            break;
        }
        let read = file.read(&mut chunk[..want])?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
        bytes_read += read as u64;
        remaining -= read as u64;
    }
    let digest = hex_lower(&hasher.finalize());
    Ok(HashAnswer {
        sha256: digest,
        bytes_read,
        from_cache: false,
        size: 0,
    })
}

/// Whether two stats describe the same, unchanged file — the rule every
/// cached answer is stored under, exposed for the read paths that stat a
/// file before and after a window (delta read, restore attempt) and need to
/// know whether the identity a cached digest was taken under still holds.
/// The "before" of the pair decides cacheability: a stat without a readable
/// epoch mtime is never the same file, only a less-certain one.
pub fn same_file_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    HashCache::is_same_file(&FileIdentity::of(before), &FileIdentity::of(after))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The digest a *cached* key is stored under names the file by path — the
/// same redaction the collector's own reports use, so the cache file (and
/// this module's errors) cannot leak a directory name the logs already
/// refuse to print.
fn path_digest(path: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hex_lower(&hasher.finalize())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sha2_to_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex_lower(&hasher.finalize())
    }

    #[test]
    fn prefix_hits_only_while_the_file_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jsonl");
        fs::write(&source, b"0123456789abcdef").unwrap();
        let mut cache = HashCache::load(dir.path());
        let answer = cache.prefix_sha256(&source, 5, None).unwrap();
        assert_eq!(answer.sha256, sha2_to_hex(b"01234"));
        assert!(!answer.from_cache);
        assert_eq!(answer.bytes_read, 5);
        cache.save().unwrap();

        let mut second = HashCache::load(dir.path());
        let answer = second.prefix_sha256(&source, 5, None).unwrap();
        assert!(answer.from_cache, "same stat, same prefix: hit");
        assert_eq!(answer.bytes_read, 0);
        assert_eq!(second.source_stats().hits, 1);

        // Any write — an append, the shape ordinary sources grow by —
        // invalidates the entry and costs a real read again.
        fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap()
            .write_all(b"more\n")
            .unwrap();
        let mut third = HashCache::load(dir.path());
        let answer = third.prefix_sha256(&source, 5, None).unwrap();
        assert!(!answer.from_cache);
        assert_eq!(answer.bytes_read, 5, "the miss re-reads the prefix bytes");
    }

    #[test]
    fn an_in_place_rewrite_of_the_same_size_still_misses() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jsonl");
        fs::write(&source, b"0123456789abcdef").unwrap();
        let mut cache = HashCache::load(dir.path());
        let _ = cache.prefix_sha256(&source, 5, None).unwrap();
        cache.save().unwrap();
        // Force a distinguishable mtime on a same-size rewrite.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(1);
        fs::write(&source, b"0123456789abcdef").unwrap();
        fs::File::options()
            .write(true)
            .open(&source)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let mut cache = HashCache::load(dir.path());
        let answer = cache.prefix_sha256(&source, 5, None).unwrap();
        // The bytes did not change, so the digest agrees — but it was
        // re-read to know that.
        assert_eq!(answer.sha256, sha2_to_hex(b"01234"));
        assert!(!answer.from_cache);
        assert_eq!(answer.bytes_read, 5);
    }

    #[test]
    fn a_truncated_prefix_never_fakes_the_recorded_length() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jsonl");
        fs::write(&source, b"0123456789").unwrap();
        let mut cache = HashCache::load(dir.path());
        let answer = cache.prefix_sha256(&source, 10, None).unwrap();
        assert_eq!(answer.bytes_read, 10);
        cache.save().unwrap();
        fs::write(&source, b"01234").unwrap();
        let mut cache = HashCache::load(dir.path());
        let answer = cache.prefix_sha256(&source, 10, None).unwrap();
        assert_eq!(answer.bytes_read, 5, "the file no longer holds the prefix");
        assert_ne!(answer.sha256, sha2_to_hex(b"0123456789"));
    }

    #[test]
    fn a_zero_offset_prefix_is_still_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jsonl");
        fs::write(&source, b"0123456789").unwrap();
        let mut cache = HashCache::load(dir.path());
        let answer = cache.prefix_sha256(&source, 0, None).unwrap();
        assert_eq!(answer.sha256, sha2_to_hex(b""));
        let answer = cache.prefix_sha256(&source, 0, None).unwrap();
        assert!(answer.from_cache, "the empty prefix caches like any other");
    }

    #[test]
    fn shard_concats_hit_while_every_shard_identity_holds() {
        let dir = tempfile::tempdir().unwrap();
        let shards_dir = dir.path().join("shards");
        fs::create_dir_all(&shards_dir).unwrap();
        let first_shard = shards_dir.join("0001.jsonl");
        let second_shard = shards_dir.join("0002.jsonl");
        fs::write(&first_shard, b"one\n").unwrap();
        fs::write(&second_shard, b"two\n").unwrap();
        let shards = vec![(1u64, first_shard.clone()), (2u64, second_shard.clone())];
        let mut cache = HashCache::load(dir.path());
        let answer = cache.shard_concat_sha256(&shards_dir, &shards, 8).unwrap();
        assert_eq!(answer.sha256, sha2_to_hex(b"one\ntwo\n"));
        assert_eq!(answer.bytes_read, 8);
        cache.save().unwrap();

        let mut second = HashCache::load(dir.path());
        let answer = second.shard_concat_sha256(&shards_dir, &shards, 8).unwrap();
        assert!(answer.from_cache, "every shard unchanged: the merge is a hit");
        assert_eq!(answer.sha256, sha2_to_hex(b"one\ntwo\n"));
        assert_eq!(second.stage_stats().bytes_read, 0);

        // A later sealed shard changes the listing: the covered prefix this
        // value speaks for must be re-established by reading.
        let third_shard = shards_dir.join("0003.jsonl");
        fs::write(&third_shard, b"three\n").unwrap();
        let grown = vec![
            (1u64, first_shard.clone()),
            (2u64, second_shard.clone()),
            (3u64, third_shard.clone()),
        ];
        let answer = second
            .shard_concat_sha256(&shards_dir, &grown, 14)
            .unwrap();
        assert_eq!(answer.bytes_read, 14);
        assert_eq!(answer.sha256, sha2_to_hex(b"one\ntwo\nthree\n"));
    }

    #[test]
    fn a_short_shard_concat_is_reported_short_and_never_stored() {
        let dir = tempfile::tempdir().unwrap();
        let shards_dir = dir.path().join("shards");
        fs::create_dir_all(&shards_dir).unwrap();
        let shard = shards_dir.join("0001.jsonl");
        fs::write(&shard, b"one\n").unwrap();
        let shards = vec![(1u64, shard.clone())];
        let mut cache = HashCache::load(dir.path());
        let answer = cache.shard_concat_sha256(&shards_dir, &shards, 100).unwrap();
        assert_eq!(answer.bytes_read, 4, "the shard holds fewer bytes than asked");
        assert_ne!(answer.sha256, sha2_to_hex(b"one\n"));
        cache.save().unwrap();
        // Not stored: the next pass re-reads rather than trusting a digest
        // that never covered what a fact claims.
        let mut second = HashCache::load(dir.path());
        let answer = second.shard_concat_sha256(&shards_dir, &shards, 100).unwrap();
        assert!(!answer.from_cache);
        assert_eq!(answer.bytes_read, 4);
    }

    #[test]
    fn a_corrupt_cache_file_fails_open_to_all_misses() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join(CACHE_FILE);
        fs::write(&state_file, b"{not json").unwrap();
        let source = dir.path().join("source.jsonl");
        fs::write(&source, b"0123456789").unwrap();
        let mut cache = HashCache::load(dir.path());
        let answer = cache.prefix_sha256(&source, 4, None).unwrap();
        assert_eq!(
            answer.bytes_read, 4,
            "an unreadable cache reads like an empty one"
        );
        // And the save replaces the corrupt file with a good one.
        cache.save().unwrap();
        let raw = fs::read(&state_file).unwrap();
        assert!(
            serde_json::from_slice::<CacheFile>(&raw).is_ok(),
            "the save must leave a parseable file"
        );
    }

    #[test]
    fn a_disabled_cache_never_stores() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jsonl");
        fs::write(&source, b"0123456789").unwrap();
        let mut cache = HashCache::disabled();
        let answer = cache.prefix_sha256(&source, 4, None).unwrap();
        assert_eq!(answer.bytes_read, 4);
        cache.save().unwrap();
        assert!(!dir.path().join(CACHE_FILE).exists());
    }

    #[test]
    fn an_untouched_pass_leaves_the_cache_file_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join(CACHE_FILE);
        let before = b"{\"version\":1,\"entries\":{}}";
        fs::write(&state_file, before).unwrap();
        let mut cache = HashCache::load(dir.path());
        cache.save().unwrap();
        assert_eq!(fs::read(&state_file).unwrap(), before);
    }
}
