//! Black-box verify tests: build a synthetic repository via a real push, then
//! assert the three levels behave on pristine data and on injected corruption.
//! Everything is local + synthetic, in temp dirs.

use chat_stasher::store::{self, BackupStore, StoreConfig};
use chat_stasher::verify;
use rustic_core::repofile::MasterKey;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Writes `sessions/<machine>/<session>/NNNNNN.jsonl` shards with synthetic
/// lines and returns the stage root.
fn make_stage(dir: &Path, machine: &str, sessions: &[(&str, u64)]) -> PathBuf {
    for (session, nshards) in sessions {
        for seq in 1..=*nshards {
            let lines: Vec<String> = (0..20)
                .map(|i| {
                    format!(
                        "{{\"seq\":{seq},\"i\":{i},\"s\":\"payload-{:?}-{session}-{seq}-{i}\"}}",
                        machine
                    )
                })
                .collect();
            store::write_sealed_shard(store::StageWriter::Collect, dir, machine, session, &lines)
                .unwrap();
        }
    }
    dir.to_path_buf()
}

fn build_repo(dir: &Path, connections: usize) -> (StoreConfig, MasterKey, PathBuf) {
    let stage = dir.join("stage");
    let stage = make_stage(&stage, "m-verify", &[("s-aaa", 2), ("s-bbb", 1)]);
    let cfg = StoreConfig {
        repo_root: dir.join("repo").to_string_lossy().into_owned(),
        key_file: dir.join("masterkey.json"),
        connections,
        options: Default::default(),
        cache_dir: None,
        no_cache: false,
    };
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let bs = BackupStore::new(cfg.clone(), "m-verify".to_string());
    bs.push(&stage, &mk).unwrap();
    (cfg, mk, stage)
}

#[test]
fn consumed_hash_audit_distinguishes_archived_and_missing_repo_records() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let archived = "a".repeat(64);
    store::write_sealed_shard(
        store::StageWriter::Ingest,
        &stage,
        "m-audit",
        "s-audit",
        &[format!(r#"{{"file_sha256":"{archived}"}}"#)],
    )
    .unwrap();
    let cfg = StoreConfig {
        repo_root: dir.path().join("repo").to_string_lossy().into_owned(),
        key_file: dir.path().join("masterkey.json"),
        connections: 1,
        options: Default::default(),
        cache_dir: None,
        no_cache: false,
    };
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let bs = BackupStore::new(cfg, "m-audit".to_string());
    bs.push(&stage, &mk).unwrap();

    let mut wanted = BTreeSet::new();
    wanted.insert(archived.clone());
    wanted.insert("b".repeat(64));
    let found = bs.archived_file_sha256s(&mk, &wanted).unwrap();
    assert_eq!(found, BTreeSet::from([archived]));
    drop(dir);
}

/// Recursively collect every file under a directory (rustic stores packs in
/// a nested `data/…` layout; this is layout-agnostic).
fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(rd) = fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p);
                }
            }
        }
    }
    out
}

#[test]
fn three_levels_pass_on_pristine_repo() {
    let dir = tempfile::TempDir::new().unwrap();
    let (cfg, mk, stage) = build_repo(dir.path(), 4);
    let bs = BackupStore::new(cfg, "m-verify".to_string());

    let l1 = bs.check_repo(&mk, false).unwrap();
    assert!(l1.ok(), "L1 findings: {:?}", l1.details);
    assert_eq!(l1.findings, 0);

    let l2 = bs.check_repo(&mk, true).unwrap();
    assert!(l2.ok(), "L2 findings: {:?}", l2.details);
    assert_eq!(l2.findings, 0);

    let rep = bs.reconcile_manifest(&mk, &stage).unwrap();
    assert!(rep.ok(), "L3 failures: {:?}", rep.rows);
    assert_eq!(rep.rows.len(), 2);
    assert!(rep.extra_in_archive.is_empty());
    for row in &rep.rows {
        assert_eq!(row.outcome, verify::SessionOutcome::Match);
    }
    drop(dir);
}

#[test]
fn l1_catches_a_missing_pack() {
    let dir = tempfile::TempDir::new().unwrap();
    let (cfg, mk, _stage) = build_repo(dir.path(), 4);
    let bs = BackupStore::new(cfg.clone(), "m-verify".to_string());

    let repo_root = Path::new(&cfg.repo_root);
    let packs = collect_files(&repo_root.join("data"));
    assert!(!packs.is_empty(), "expected at least one pack file");
    fs::remove_file(&packs[0]).unwrap();

    let l1 = bs.check_repo(&mk, false).unwrap();
    assert!(l1.errors >= 1, "L1 missed a deleted pack: {:?}", l1.details);
    assert!(!l1.ok());
    drop(dir);
}

#[test]
fn l2_and_l3_catch_a_payload_byte_flip() {
    // Flip one byte inside every pack (headers sit at the end, so byte 1 is
    // inside blob payloads). rustic_core serves *some* packs from an on-disk
    // cache, so to be certain at least the data pack is hit we corrupt all of
    // them — every one must then be re-verified (L2) or re-read (L3).
    let dir = tempfile::TempDir::new().unwrap();
    let (cfg, mk, stage) = build_repo(dir.path(), 4);
    let bs = BackupStore::new(cfg.clone(), "m-verify".to_string());

    let repo_root = Path::new(&cfg.repo_root);
    let packs = collect_files(&repo_root.join("data"));
    assert!(!packs.is_empty());
    for pack in &packs {
        let mut bytes = fs::read(pack).unwrap();
        bytes[1] ^= 0x01;
        fs::write(pack, &bytes).unwrap();
    }

    // L2 re-hashes every pack, so this must fail.
    let l2 = bs.check_repo(&mk, true).unwrap();
    assert!(!l2.ok(), "L2 missed the byte flips: {:?}", l2.details);

    // L3 must not report OK on bytes that no longer decrypt to the same shard.
    let l3 = bs.reconcile_manifest(&mk, &stage);
    assert!(
        l3.is_err(),
        "L3 must not report OK on a corrupted data pack"
    );
    drop(dir);
}

#[test]
fn l1_does_not_verify_payload_bytes_but_l2_does() {
    // rust_core serves some packs from an on-disk cache, so first locate a
    // *data-bearing* pack: corrupting it makes the read-back fail. Then, with
    // a one-byte payload flip in place, L1 (structure) must still pass while
    // L2 (content) must fail.
    let dir = tempfile::TempDir::new().unwrap();
    let (cfg, mk, stage) = build_repo(dir.path(), 4);
    let bs = BackupStore::new(cfg.clone(), "m-verify".to_string());

    let repo_root = Path::new(&cfg.repo_root);
    let packs = collect_files(&repo_root.join("data"));
    let mut data_pack = None;
    for pack in &packs {
        let mut bytes = fs::read(pack).unwrap();
        bytes[1] ^= 0x01;
        fs::write(pack, &bytes).unwrap();
        let hit = bs.reconcile_manifest(&mk, &stage).is_err();
        bytes[1] ^= 0x01;
        fs::write(pack, &bytes).unwrap();
        if hit {
            data_pack = Some(pack.clone());
            break;
        }
    }
    let pack = data_pack.expect("no data-bearing pack found");
    let mut bytes = fs::read(&pack).unwrap();
    bytes[1] ^= 0x01;
    fs::write(&pack, &bytes).unwrap();

    let l1 = bs.check_repo(&mk, false).unwrap();
    assert!(
        l1.ok(),
        "L1 reported payload damage it should not read: {:?}",
        l1.details
    );
    let l2 = bs.check_repo(&mk, true).unwrap();
    assert!(
        !l2.ok(),
        "L2 must catch the payload byte flip: {:?}",
        l2.details
    );
    drop(dir);
}
