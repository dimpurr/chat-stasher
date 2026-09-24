//! W120 · ADR-034 premise experiment: are **data blob ids** plaintext content
//! hashes, i.e. identical in two repositories that hold the same content under
//! **different keys**?
//!
//! ADR-034's body cache is specified as content-addressed by *data block id*,
//! and its "开工前必须验证的前提" asks exactly this question, with a
//! documented fallback for either answer: same ids ⇒ one copy shared across
//! destinations; different ids ⇒ still one global quota, but stored per
//! `(remote + id)`.
//!
//! This test answers it with two local repositories built by our own `push`
//! path, so the chunker, the shard layout and the encryption are the ones users
//! actually get. It therefore also pins the answer: if a `rustic_core` upgrade
//! ever changes how a blob id is derived, this test says so instead of leaving
//! ADR-034's fallback undetermined.
//!
//! What is compared:
//!   * the **set of data blob ids** in each repository, and
//!   * the **set of pack ids** in each repository.
//!
//! Everything here is synthetic: the shard bodies are generated in this file
//! and no real archive, key or destination is touched. The test prints only
//! counts and 8-hex-character prefixes.

use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::{BlobType, IndexFile, IndexId, MasterKey, PackId};
use rustic_core::{Credentials, Repository, RepositoryOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn cfg(repo: &Path, key: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: None,
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

/// Write the same sealed stage into `repo` under `mk`, then return the
/// repository's `(data blob ids, pack ids)`.
fn push_and_read_ids(
    repo: &Path,
    key: &Path,
    mk: &MasterKey,
    stage: &Path,
) -> (BTreeSet<String>, BTreeSet<PackId>) {
    store::persist_key_file(&cfg(repo, key), mk).expect("persist key");
    let store = BackupStore::new(cfg(repo, key), "w120-premise".to_string());
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
        .expect("credentials");

    let mut blob_ids = BTreeSet::new();
    let mut pack_ids = BTreeSet::new();
    for index_id in repo.list::<IndexId>().expect("list index files") {
        let file: IndexFile = repo.get_file(&index_id).expect("read index file");
        for pack in file.packs {
            pack_ids.insert(pack.id);
            for blob in pack.blobs {
                if blob.tpe == BlobType::Data {
                    blob_ids.insert(blob.id.to_string());
                }
            }
        }
    }
    (blob_ids, pack_ids)
}

fn short(ids: &BTreeSet<String>) -> Vec<String> {
    ids.iter().map(|id| id[..8].to_string()).collect()
}

#[test]
fn same_content_different_keys_same_data_blob_ids_different_pack_ids() {
    let sandbox = tempfile::tempdir().expect("tempdir");
    let root = sandbox.path();

    // One stage, pushed twice into two repositories with two different keys.
    let stage = root.join("stage");
    let bodies = [
        synthetic_shard(40, 32_768, "alpha"),
        synthetic_shard(40, 32_768, "beta"),
        synthetic_shard(40, 32_768, "gamma"),
    ];
    for (n, body) in bodies.iter().enumerate() {
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
    let (blobs_a, packs_a) = push_and_read_ids(
        &root.join("repo-a"),
        &root.join("key-a.json"),
        &mk_a,
        &stage,
    );
    let (blobs_b, packs_b) = push_and_read_ids(
        &root.join("repo-b"),
        &root.join("key-b.json"),
        &mk_b,
        &stage,
    );

    println!(
        "premise: data blobs a={} b={} shared={} · packs a={} b={} shared={}",
        blobs_a.len(),
        blobs_b.len(),
        blobs_a.intersection(&blobs_b).count(),
        packs_a.len(),
        packs_b.len(),
        packs_a.intersection(&packs_b).count(),
    );
    println!("premise: data blob ids (first 8) a={:?}", short(&blobs_a));
    println!("premise: data blob ids (first 8) b={:?}", short(&blobs_b));
    println!(
        "premise: pack ids (first 8) a={:?}",
        packs_a
            .iter()
            .map(|p| p.to_string()[..8].to_string())
            .collect::<Vec<_>>()
    );
    println!(
        "premise: pack ids (first 8) b={:?}",
        packs_b
            .iter()
            .map(|p| p.to_string()[..8].to_string())
            .collect::<Vec<_>>()
    );

    assert!(
        !blobs_a.is_empty(),
        "no data blob was indexed; the fixture is too small to answer the question"
    );
    // The measured fact ADR-034 asks for: data blob ids are plaintext content
    // hashes, so the same content in two differently-keyed repositories has the
    // *same* ids.
    assert_eq!(
        blobs_a, blobs_b,
        "ADR-034 premise: data blob ids of identical content under two different keys must match"
    );
    // The other half of the measurement, and the reason a ciphertext-level
    // cache cannot reuse entries across destinations: pack ids are hashes of
    // the encrypted pack, so two keys produce two disjoint sets.
    assert!(
        packs_a.is_disjoint(&packs_b),
        "ADR-034 premise: pack ids of the same content under two different keys must differ"
    );
    assert_eq!(
        packs_a.len(),
        packs_b.len(),
        "both repositories must hold the same number of packs, or the comparison is not like-for-like"
    );
}
