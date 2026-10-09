//! W120 · ADR-034 premise experiment: are **data blob ids** plaintext content
//! hashes, i.e. independent of the key a repository is encrypted with?
//!
//! ADR-034's body cache is specified as content-addressed by *data block id*,
//! and its "premise that must be verified before starting work" asks exactly
//! this question, with a
//! documented fallback for either answer: same ids ⇒ one copy shared across
//! destinations; different ids ⇒ still one global quota, but stored per
//! `(remote + id)`.
//!
//! This test answers it with two local repositories built by our own `push`
//! path, so the shard layout and the encryption are the ones users actually
//! get. What that path measured (2026-09-30):
//!
//!   * A data blob id **is** the SHA-256 of the plaintext chunk it names. Every
//!     blob in both repositories — which use two different keys — hashes back
//!     to its own id, so no key material enters it. This is asserted below by
//!     reading each blob out again and re-hashing it; if a `rustic_core` upgrade
//!     ever changes how a blob id is derived, that assertion fails instead of
//!     leaving ADR-034's fallback undetermined.
//!   * The **set** of ids is not a function of the content alone. Which bytes
//!     become one chunk is decided by the repository's Rabin chunker, and its
//!     polynomial is drawn at random by `init` and then stored in that
//!     repository's own config (`rustic_core-0.12.0`
//!     `src/commands/init.rs:47` `random_poly()` →
//!     `ConfigFile::chunker_polynomial`). Two separately-initialised
//!     repositories therefore cut the same bytes in different places as soon as
//!     a file exceeds the chunker's minimum size, and one shard body shows up as
//!     two ids where the other kept it whole. This test's earlier version
//!     asserted the two id sets were equal and failed intermittently for exactly
//!     that reason: the ids it saw only in one repository were the two SHA-256s
//!     of one shard body's halves.
//!   * The honest answer to the ADR's question is therefore: **the id is
//!     key-independent, but id sets coincide only where the chunker parameters
//!     coincide** — and two independently-initialised destinations do not have
//!     the same polynomial. ADR-034's second branch ("different ids ⇒ one shared
//!     quota, entries per `(remote + id)`") is the one that applies, and is what
//!     `body_cache::CacheKey` implements by keying on the ciphertext's own
//!     coordinates.
//!
//! What is asserted, and why each of these is unconditional:
//!   * every data blob id re-hashes to the plaintext it names, in both
//!     repositories (the premise, per key);
//!   * each shard body is present in both repositories as a run of consecutive
//!     chunks — the same bytes, possibly cut differently;
//!   * every **whole file** the stage holds below the chunker's minimum size
//!     carries the same id in both repositories. Such a file is returned as a
//!     single chunk before the polynomial is ever consulted, so this is the one
//!     cross-key id equality the chunker cannot disturb; the three sessions'
//!     `shard-seq` counters are the fixture's below-minimum files;
//!   * the below-minimum blobs that are *not* a whole file are printed, never
//!     asserted: a fragment below the minimum is the tail of a body the
//!     polynomial cut, and the other key may hold those same bytes inside a
//!     longer chunk, so no id equality is promised for it. Asserting over every
//!     below-minimum blob was this test's second flake, red in CI on 2026-10-09
//!     with one repository cut as 1050139 + 262451 bytes where the other kept
//!     the body whole: the 262451-byte tail has no counterpart id, by
//!     construction;
//!   * pack ids are disjoint and equal in number, because a pack id hashes the
//!     *encrypted* pack, which is the half of the ADR-034 premise that says a
//!     ciphertext-level cache cannot be shared across destinations.
//!
//! The two repositories' chunker polynomials (and, when they differ, the cut
//! shape of each body), and every below-minimum blob that is not a whole file,
//! are printed: a differing cut is the mechanism behind the flakes this test
//! used to have, so it is reported as a measurement rather than asserted away.
//!
//! Everything here is synthetic: the shard bodies are generated in this file
//! and no real archive, key or destination is touched. The test prints only
//! counts, byte sizes and 8-hex-character prefixes.

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::{BlobType, IndexFile, IndexId, MasterKey, PackId};
use rustic_core::{Credentials, Repository, RepositoryOptions};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

/// Metadata cache under the sandbox (W289), not the real user cache the two
/// repositories would otherwise have populated; the reads below still open
/// with the cache enabled, and only the explicit `Repository::new` comparison
/// pass pins `no_cache` for its own reason.
fn cfg(repo: &Path, key: &Path, cache: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(cache.join("rustic-cache")),
        no_cache: false,
    }
}

/// A deterministic, compressible synthetic shard body: `lines` JSONL records of
/// roughly `line_bytes` each. Deterministic so both repositories receive
/// byte-identical content, which is the whole point of the experiment.
fn synthetic_shard(lines: usize, line_bytes: usize, tag: &str) -> Vec<u8> {
    let filler = "x".repeat(line_bytes);
    let mut body = String::new();
    for i in 0..lines {
        body.push_str(&format!(
            r#"{{"type":"user","i":{i},"tag":"{tag}","text":"{filler}"}}"#
        ));
        body.push('\n');
    }
    body.into_bytes()
}

/// Lower-case hex of a SHA-256, the spelling a blob id is compared in.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// One repository's ids, plus the chunker parameters the comparison needs.
struct RepoIds {
    /// Data blob id (hex) → the plaintext chunk that id names.
    blobs: BTreeMap<String, Vec<u8>>,
    packs: BTreeSet<PackId>,
    /// The chunker's minimum chunk size, as this repository's config records
    /// it. A file below it is always exactly one chunk.
    chunk_min_size: usize,
    /// The Rabin polynomial this repository was initialised with. Printed, not
    /// asserted: it is drawn per `init`, and it is what decides the cut.
    chunker_polynomial: String,
}

impl RepoIds {
    /// The plaintext of every data blob, as a set of byte strings — a chunk's
    /// own bytes are its identity for the coverage check below.
    fn plaintexts(&self) -> BTreeSet<&[u8]> {
        self.blobs.values().map(Vec::as_slice).collect()
    }
}

/// Write the same sealed stage into `repo`, then read back what the repository
/// actually stored: every pack id, and every data blob id with the plaintext
/// that id names.
fn push_and_read_ids(
    repo: &Path,
    key: &Path,
    cache: &Path,
    mk: &MasterKey,
    stage: &Path,
) -> RepoIds {
    store::persist_key_file(&cfg(repo, key, cache), mk).expect("persist key");
    let store = BackupStore::new(cfg(repo, key, cache), "w120-premise".to_string());
    let summary = store.push(stage, mk).expect("push fixture");
    assert!(
        summary.files_new > 0,
        "the fixture must actually write files, or the experiment compares two empty repositories"
    );

    let backends = store.backends().expect("backends");
    // `no_cache` on: the experiment is about what is *in the repository*, and a
    // local metadata cache left behind by an earlier run must not be able to
    // answer for it.
    let opts = RepositoryOptions::default().no_cache(true);
    let repo = Repository::new(&opts, &backends)
        .expect("open")
        .open(&Credentials::Masterkey(mk.clone()))
        .expect("credentials")
        // Re-reading a data blob by id needs the index, which is exactly the
        // state `cat_blob` is defined on.
        .to_indexed()
        .expect("index");

    let mut blobs = BTreeMap::new();
    let mut packs = BTreeSet::new();
    for index_id in repo.list::<IndexId>().expect("list index files") {
        let file: IndexFile = repo.get_file(&index_id).expect("read index file");
        for pack in file.packs {
            packs.insert(pack.id);
            for blob in pack.blobs {
                if blob.tpe == BlobType::Data {
                    // The full 64-hex id: `Display` on an `Id` is the 8-character
                    // short form, which is not enough to read the blob back and
                    // not enough to compare against a SHA-256.
                    let id = blob.id.to_hex().as_str().to_string();
                    // Read the chunk back through the same layer a reader uses,
                    // so the id is checked against the bytes it names rather
                    // than against a hash computed inside the store.
                    let plaintext = repo
                        .cat_blob(BlobType::Data, &id)
                        .expect("read data blob")
                        .to_vec();
                    blobs.insert(id, plaintext);
                }
            }
        }
    }
    RepoIds {
        blobs,
        packs,
        chunk_min_size: repo.config().chunk_min_size(),
        chunker_polynomial: repo.config().chunker_polynomial.clone(),
    }
}

/// How this repository stores `body`: the lengths of the consecutive chunks its
/// bytes are cut into, in order. `None` means the repository does not hold those
/// bytes at all.
///
/// The walk takes the longest chunk that is a prefix of what is left, which is
/// the only correct choice here: two chunks of one file cannot overlap, so if a
/// chunk ends at some position, no chunk can both start at 0 and end after it.
fn cut_lengths(body: &[u8], plaintexts: &BTreeSet<&[u8]>) -> Option<Vec<usize>> {
    let mut cuts = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        let chunk = plaintexts
            .iter()
            .filter(|chunk| rest.starts_with(chunk))
            .max_by_key(|chunk| chunk.len())?;
        cuts.push(chunk.len());
        rest = &rest[chunk.len()..];
    }
    Some(cuts)
}

/// Every regular file under `stage` whose bytes are below `min` — the files the
/// chunker returns as one chunk before it ever consults its polynomial — as
/// (path relative to `stage`, bytes), sorted by path so a failure names the same
/// file every run.
///
/// A *fragment* of a larger file is deliberately not among them: it is not a
/// file, and its bytes are a run of chunker output whose boundaries came from
/// the previous cut.
fn stage_files_below(stage: &Path, min: usize) -> Vec<(String, Vec<u8>)> {
    let mut found = Vec::new();
    let mut dirs = vec![stage.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).expect("read stage directory") {
            let entry = entry.expect("stage entry");
            let path = entry.path();
            let kind = entry.file_type().expect("stage entry type");
            if kind.is_dir() {
                dirs.push(path);
            } else if kind.is_file() {
                let bytes = fs::read(&path).expect("read stage file");
                if bytes.len() < min {
                    let name = path
                        .strip_prefix(stage)
                        .expect("stage file is under the stage root")
                        .to_string_lossy()
                        .into_owned();
                    found.push((name, bytes));
                }
            }
        }
    }
    found.sort();
    found
}

#[test]
fn same_content_different_keys_plaintext_blob_ids_different_pack_ids() {
    let sandbox = tempfile::tempdir().expect("tempdir");
    let root = sandbox.path();

    // One stage, pushed twice into two repositories with two different keys.
    let stage = root.join("stage");
    let bodies = [
        ("alpha", synthetic_shard(40, 32_768, "alpha")),
        ("beta", synthetic_shard(40, 32_768, "beta")),
        ("gamma", synthetic_shard(40, 32_768, "gamma")),
    ];
    for (n, (_, body)) in bodies.iter().enumerate() {
        store::write_sealed_shard_raw_with_cap(
            StageWriter::Collect,
            &stage,
            "w120-premise",
            &format!("w120-premise-{n}"),
            body,
            store::DEFAULT_SHARD_BUCKET_CAP,
        )
        .expect("write sealed shard");
    }

    let mk_a = MasterKey::new();
    let mk_b = MasterKey::new();
    let a = push_and_read_ids(
        &root.join("repo-a"),
        &root.join("key-a.json"),
        root,
        &mk_a,
        &stage,
    );
    let b = push_and_read_ids(
        &root.join("repo-b"),
        &root.join("key-b.json"),
        root,
        &mk_b,
        &stage,
    );

    let plain_a = a.plaintexts();
    let plain_b = b.plaintexts();
    // The cut shape is the measurement that used to be an assertion: report both
    // repositories' shape for every body, whatever it is.
    let cuts = |plaintexts: &BTreeSet<&[u8]>| {
        bodies
            .iter()
            .map(|(tag, body)| (*tag, cut_lengths(body, plaintexts)))
            .collect::<Vec<_>>()
    };

    println!(
        "premise: data blobs a={} b={} shared={} · packs a={} b={} shared={}",
        a.blobs.len(),
        b.blobs.len(),
        a.blobs
            .keys()
            .filter(|id| b.blobs.contains_key(*id))
            .count(),
        a.packs.len(),
        b.packs.len(),
        a.packs.intersection(&b.packs).count(),
    );
    println!(
        "premise: chunker min a={} b={} · polynomial a={} b={}",
        a.chunk_min_size, b.chunk_min_size, a.chunker_polynomial, b.chunker_polynomial
    );
    println!("premise: cut lengths a={:?}", cuts(&plain_a));
    println!("premise: cut lengths b={:?}", cuts(&plain_b));
    println!(
        "premise: data blob ids (first 8) a={:?}",
        a.blobs
            .keys()
            .map(|id| id[..8].to_string())
            .collect::<Vec<_>>()
    );
    println!(
        "premise: data blob ids (first 8) b={:?}",
        b.blobs
            .keys()
            .map(|id| id[..8].to_string())
            .collect::<Vec<_>>()
    );
    println!(
        "premise: pack ids (first 8) a={:?}",
        a.packs
            .iter()
            .map(|p| p.to_string()[..8].to_string())
            .collect::<Vec<_>>()
    );
    println!(
        "premise: pack ids (first 8) b={:?}",
        b.packs
            .iter()
            .map(|p| p.to_string()[..8].to_string())
            .collect::<Vec<_>>()
    );

    // The premise itself, measured in both repositories: a data blob id is the
    // SHA-256 of the plaintext chunk it names, under either key.
    for (label, ids) in [("a", &a), ("b", &b)] {
        assert!(
            !ids.blobs.is_empty(),
            "no data blob was indexed in repo-{label}; the fixture is too small to answer the question"
        );
        for (id, plaintext) in &ids.blobs {
            assert_eq!(
                &sha256_hex(plaintext),
                id,
                "repo-{label}: data blob {} does not hash to its own plaintext, so the id is not a plaintext content hash",
                &id[..8]
            );
        }
    }

    // Both repositories hold the same bytes: the cut may differ, the content
    // may not.
    for (tag, body) in &bodies {
        for (label, plaintexts) in [("a", &plain_a), ("b", &plain_b)] {
            let cut = cut_lengths(body, plaintexts);
            assert!(
                cut.is_some(),
                "repo-{label} does not hold shard body `{tag}` as a run of consecutive chunks"
            );
        }
    }

    // What IS key-independent across the two repositories: a *whole file* below
    // the chunker's minimum size is returned as one chunk before the polynomial
    // is consulted, so its bytes carry the same id under both keys. This is
    // asked of the stage's own files, not of every blob below the minimum: a
    // fragment below the minimum is the tail of a larger body the polynomial
    // cut, and the other key can hold those same bytes inside a longer chunk, so
    // a fragment promises nothing across keys. (The fixture's below-minimum
    // files are the three sessions' `shard-seq` counters.)
    assert_eq!(
        a.chunk_min_size, b.chunk_min_size,
        "the two repositories must be compared under the same chunker sizes, or the comparison is not like-for-like"
    );
    let below_minimum_files = stage_files_below(&stage, a.chunk_min_size);
    assert!(
        !below_minimum_files.is_empty(),
        "the fixture holds no file below the chunker minimum, so the one cross-key id equality this test asserts would be vacuous"
    );
    for (name, bytes) in &below_minimum_files {
        let id = sha256_hex(bytes);
        let in_a = a.blobs.get(&id);
        let in_b = b.blobs.get(&id);
        assert!(
            in_a.is_some() && in_b.is_some(),
            "`{name}` ({} bytes, below the chunker minimum) is not stored under its own id {} in both repositories (present in a={}, b={})",
            bytes.len(),
            &id[..8],
            in_a.is_some(),
            in_b.is_some()
        );
        assert_eq!(
            in_b, in_a,
            "a whole file below the chunker minimum is one chunk under any polynomial, so `{name}` must carry the same id under both keys"
        );
    }

    // Reported, never asserted: the below-minimum blobs that are not a whole
    // file. Each is a tail some polynomial cut, and the other repository may
    // hold those bytes inside a longer chunk, so its id is not expected on both
    // sides — this is the flake the assertion above used to have, kept as a
    // measurement.
    let fragments = |ids: &RepoIds| -> Vec<String> {
        ids.blobs
            .iter()
            .filter(|(_, plaintext)| {
                plaintext.len() < ids.chunk_min_size
                    && !below_minimum_files
                        .iter()
                        .any(|(_, bytes)| bytes.as_slice() == plaintext.as_slice())
            })
            .map(|(id, _)| id[..8].to_string())
            .collect()
    };
    println!(
        "premise: below-minimum files={} ({} bytes, their ids asserted equal under both keys)",
        below_minimum_files.len(),
        below_minimum_files
            .iter()
            .map(|(_, bytes)| bytes.len())
            .sum::<usize>()
    );
    println!(
        "premise: below-minimum fragments (first 8, measured and not asserted) a={:?} b={:?}",
        fragments(&a),
        fragments(&b)
    );

    // The other half of the measurement, and the reason a ciphertext-level cache
    // cannot reuse entries across destinations: pack ids are hashes of the
    // encrypted pack, so two keys produce two disjoint sets.
    assert!(
        a.packs.is_disjoint(&b.packs),
        "ADR-034 premise: pack ids of the same content under two different keys must differ"
    );
    assert_eq!(
        a.packs.len(),
        b.packs.len(),
        "both repositories must hold the same number of packs, or the comparison is not like-for-like"
    );
}

/// The selector the cross-key assertion above rests on, pinned on its own: only
/// a whole file below the minimum is returned. A run of bytes below the minimum
/// that is not a file's whole content — the fragment the old assertion read out
/// of the repository and asserted over — is not, and no recursion into the
/// stage's directory tree is missed.
#[test]
fn below_minimum_selection_is_whole_files_only() {
    let sandbox = tempfile::tempdir().expect("tempdir");
    let stage = sandbox.path().join("stage");
    let session = stage
        .join("sessions")
        .join("w120-premise")
        .join("w120-premise-0");
    fs::create_dir_all(&session).expect("create session dir");
    // The writer's own below-minimum file: the shard-sequence counter.
    fs::write(session.join("shard-seq"), b"1").expect("write shard-seq");
    // A shard body, above any minimum this test passes.
    fs::write(session.join("000000.jsonl"), vec![b'x'; 4096]).expect("write shard");

    let files = stage_files_below(&stage, 1024);
    assert_eq!(
        files.len(),
        1,
        "only the 1-byte counter is below the minimum, not the 4096-byte body: {:?}",
        files.iter().map(|(name, _)| name).collect::<Vec<_>>()
    );
    assert_eq!(files[0].0, "sessions/w120-premise/w120-premise-0/shard-seq");
    assert_eq!(files[0].1, b"1");

    // Nothing below the minimum is still an answer, and it is not an error.
    assert!(stage_files_below(&stage, 1).is_empty());
}
