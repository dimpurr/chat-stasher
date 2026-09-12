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
        cache_dir: Some(dir.join("cache")),
        no_cache: false,
    };
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let bs = BackupStore::new(cfg.clone(), "m-verify".to_string());
    bs.push(&stage, &mk).unwrap();
    (cfg, mk, stage)
}

/// A located data blob within its on-disk pack file.
#[derive(Debug, Clone)]
struct DataBlobTarget {
    pack_path: PathBuf,
    offset: usize,
    length: usize,
}

/// Query rustic's index to locate every Data blob's exact offset and length
/// within its pack file on disk. This is 100% deterministic and does not rely
/// on guesswork about pack file layout or blob order.
fn locate_data_blobs(cfg: &StoreConfig, mk: &MasterKey) -> Vec<DataBlobTarget> {
    use rustic_core::repofile::{BlobType, IndexFile, IndexId};

    let bs = BackupStore::new(cfg.clone(), "m-verify".to_string());
    let (repo, _) = bs.open_or_init(mk).expect("open repo to inspect index");
    let index_ids: Vec<IndexId> = repo.list::<IndexId>().expect("list index").collect();
    let repo_root = Path::new(&cfg.repo_root);
    let all_packs = collect_files(&repo_root.join("data"));

    let mut targets = Vec::new();
    for id in index_ids {
        let index: IndexFile = repo.get_file::<IndexFile>(&id).expect("get index file");
        for pack in index.packs {
            let pack_hex = pack.id.to_hex();
            let pack_name = pack_hex.as_str();
            let pack_path = all_packs
                .iter()
                .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(pack_name))
                .unwrap_or_else(|| panic!("pack file {pack_name} not found in data/"));

            for blob in pack.blobs {
                if blob.tpe == BlobType::Data {
                    targets.push(DataBlobTarget {
                        pack_path: pack_path.clone(),
                        offset: blob.location.offset as usize,
                        length: blob.location.length as usize,
                    });
                }
            }
        }
    }
    targets
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
        cache_dir: Some(dir.path().join("cache")),
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
    // Locate every data blob via rustic's index and flip one byte strictly
    // inside each data blob's ciphertext payload. Tree blobs, index files, and
    // pack headers remain pristine.
    let dir = tempfile::TempDir::new().unwrap();
    let (cfg, mk, stage) = build_repo(dir.path(), 4);
    let bs = BackupStore::new(cfg.clone(), "m-verify".to_string());

    let data_blobs = locate_data_blobs(&cfg, &mk);
    assert!(!data_blobs.is_empty(), "expected at least one data blob");
    for target in &data_blobs {
        let mut bytes = fs::read(&target.pack_path).unwrap();
        let flip_idx = target.offset + (target.length / 2);
        bytes[flip_idx] ^= 0x01;
        fs::write(&target.pack_path, &bytes).unwrap();
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
    // Locate a data blob via the index and flip a single byte strictly inside
    // its payload ciphertext. Because only a data blob (and no tree blob,
    // pack size, or pack header) is modified, L1 (structural metadata check)
    // must still pass, while L2 (content verification) must detect the
    // corruption and fail.
    let dir = tempfile::TempDir::new().unwrap();
    let (cfg, mk, _stage) = build_repo(dir.path(), 4);
    let bs = BackupStore::new(cfg.clone(), "m-verify".to_string());

    let data_blobs = locate_data_blobs(&cfg, &mk);
    assert!(!data_blobs.is_empty(), "expected at least one data blob");
    let target = &data_blobs[0];

    let mut bytes = fs::read(&target.pack_path).unwrap();
    let flip_idx = target.offset + (target.length / 2);
    bytes[flip_idx] ^= 0x01;
    fs::write(&target.pack_path, &bytes).unwrap();

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

#[test]
fn core_e2e_reclaimed_session_survives_subsequent_push_and_matches_l3() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-verify";
    let session_1 = "s-reclaimed";
    let session_2 = "s-active";

    // 1. Write session 1 to stage
    store::write_sealed_shard(
        store::StageWriter::Collect,
        &stage,
        machine,
        session_1,
        &[r#"{"seq":1,"msg":"hello from reclaimed session"}"#.to_string()],
    )
    .unwrap();

    let cfg = StoreConfig {
        repo_root: dir.path().join("repo").to_string_lossy().into_owned(),
        key_file: dir.path().join("masterkey.json"),
        connections: 1,
        options: Default::default(),
        cache_dir: Some(dir.path().join("cache")),
        no_cache: false,
    };
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let bs = BackupStore::new(cfg.clone(), machine.to_string());

    // 2. Initial push: snapshot 1 contains session 1
    bs.push(&stage, &mk).unwrap();

    // 3. Reclaim session 1
    let dests = vec![chat_stasher::stagereclaim::NamedStore {
        name: "local-repo".to_string(),
        cfg: cfg.clone(),
    }];
    let reclaim_report = chat_stasher::stagereclaim::reclaim_stage(&stage, &dests, true).unwrap();
    assert!(!reclaim_report.blocked());
    assert_eq!(reclaim_report.reclaimed.len(), 1);

    // Verify session 1 body is gone from stage
    assert_eq!(
        store::sealed_shard_entries(&store::session_shard_dir(&stage, machine, session_1))
            .unwrap()
            .len(),
        0
    );

    // 4. Now stage a new session 2
    store::write_sealed_shard(
        store::StageWriter::Collect,
        &stage,
        machine,
        session_2,
        &[r#"{"seq":1,"msg":"hello from active session"}"#.to_string()],
    )
    .unwrap();

    // Second push: snapshot 2 contains session 2, but NOT session 1!
    bs.push(&stage, &mk).unwrap();

    // 5. Verify L3 reconcile
    let rep = bs.reconcile_manifest(&mk, &stage).unwrap();

    let s1_row = rep
        .rows
        .iter()
        .find(|r| r.session_id == session_1)
        .expect("session_1 must be in expected manifest rows");

    assert_eq!(
        s1_row.outcome,
        verify::SessionOutcome::Match,
        "ADR-021: reclaimed session must match via cumulative search, but got: {:?}",
        s1_row.outcome
    );
    assert_eq!(s1_row.basis, verify::ExpectationBasis::StoredManifest);

    let s2_row = rep
        .rows
        .iter()
        .find(|r| r.session_id == session_2)
        .expect("session_2 must be in expected manifest rows");
    assert_eq!(s2_row.outcome, verify::SessionOutcome::Match);
    assert_eq!(s2_row.basis, verify::ExpectationBasis::DerivedFromStageBody);

    assert!(rep.ok(), "L3 reconcile should pass: {:?}", rep.rows);
    drop(dir);
}

#[test]
fn core_e2e_reclaimed_session_missing_in_all_snapshots_reports_missing_in_archive() {
    let dir = tempfile::tempdir().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-verify";
    let session_1 = "s-reclaimed";
    let session_2 = "s-active";

    // 1. Setup session 1 on stage
    store::write_sealed_shard(
        store::StageWriter::Collect,
        &stage,
        machine,
        session_1,
        &[r#"{"seq":1,"msg":"hello from session 1"}"#.to_string()],
    )
    .unwrap();

    let cfg = StoreConfig {
        repo_root: dir.path().join("repo").to_string_lossy().into_owned(),
        key_file: dir.path().join("masterkey.json"),
        connections: 1,
        options: Default::default(),
        cache_dir: Some(dir.path().join("cache")),
        no_cache: false,
    };
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let bs = BackupStore::new(cfg.clone(), machine.to_string());

    // 2. Initial push: snapshot 1 contains session 1
    bs.push(&stage, &mk).unwrap();

    // 3. Reclaim session 1
    let dests = vec![chat_stasher::stagereclaim::NamedStore {
        name: "local-repo".to_string(),
        cfg: cfg.clone(),
    }];
    let reclaim_report = chat_stasher::stagereclaim::reclaim_stage(&stage, &dests, true).unwrap();
    assert!(!reclaim_report.blocked());
    assert_eq!(reclaim_report.reclaimed.len(), 1);

    // 4. Now stage a new session 2
    store::write_sealed_shard(
        store::StageWriter::Collect,
        &stage,
        machine,
        session_2,
        &[r#"{"seq":1,"msg":"hello from active session"}"#.to_string()],
    )
    .unwrap();

    // Second push: snapshot 2 contains session 2
    bs.push(&stage, &mk).unwrap();

    // Delete snapshot 1 to simulate true loss of historical snapshot
    let snaps_dir = dir.path().join("repo").join("snapshots");
    let mut snap_files: Vec<_> = fs::read_dir(&snaps_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    snap_files.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).unwrap());
    assert_eq!(snap_files.len(), 2);
    fs::remove_file(&snap_files[0]).unwrap();

    // 5. Verify L3 reconcile
    let rep = bs.reconcile_manifest(&mk, &stage).unwrap();

    let s1_row = rep
        .rows
        .iter()
        .find(|r| r.session_id == session_1)
        .expect("session_1 must be in expected manifest rows");

    assert_eq!(
        s1_row.outcome,
        verify::SessionOutcome::MissingInArchive,
        "Reclaimed session absent from all remaining snapshots must report MissingInArchive"
    );
    assert_eq!(s1_row.basis, verify::ExpectationBasis::StoredManifest);

    let s2_row = rep
        .rows
        .iter()
        .find(|r| r.session_id == session_2)
        .expect("session_2 must be in expected manifest rows");
    assert_eq!(s2_row.outcome, verify::SessionOutcome::Match);
    assert_eq!(s2_row.basis, verify::ExpectationBasis::DerivedFromStageBody);

    assert_eq!(rep.failed(), 1);
    assert_eq!(rep.unverifiable(), 0);
    assert_eq!(rep.matched(), 1);
    assert!(!rep.ok());
    drop(dir);
}

#[test]
fn core_e2e_reclaimed_session_corrupt_manifest_reports_unverifiable() {
    let dir = tempfile::tempdir().unwrap();
    let stage = dir.path().join("stage");
    let machine = "m-verify";
    let session_1 = "s-reclaimed";

    // 1. Setup session 1 on stage
    store::write_sealed_shard(
        store::StageWriter::Collect,
        &stage,
        machine,
        session_1,
        &[r#"{"seq":1,"msg":"hello from session 1"}"#.to_string()],
    )
    .unwrap();

    let cfg = StoreConfig {
        repo_root: dir.path().join("repo").to_string_lossy().into_owned(),
        key_file: dir.path().join("masterkey.json"),
        connections: 1,
        options: Default::default(),
        cache_dir: Some(dir.path().join("cache")),
        no_cache: false,
    };
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let bs = BackupStore::new(cfg.clone(), machine.to_string());

    // 2. Initial push: snapshot 1 contains session 1
    bs.push(&stage, &mk).unwrap();

    // 3. Reclaim session 1
    let dests = vec![chat_stasher::stagereclaim::NamedStore {
        name: "local-repo".to_string(),
        cfg: cfg.clone(),
    }];
    let reclaim_report = chat_stasher::stagereclaim::reclaim_stage(&stage, &dests, true).unwrap();
    assert!(!reclaim_report.blocked());
    assert_eq!(reclaim_report.reclaimed.len(), 1);

    // 4. Corrupt the manifest file for machine m-verify
    let manifest_path = stage.join("meta").join(machine).join("manifest-v1.jsonl");
    fs::write(&manifest_path, "this is corrupt json\n").unwrap();

    // 5. Verify L3 reconcile
    let rep = bs.reconcile_manifest(&mk, &stage).unwrap();

    let s1_row = rep
        .rows
        .iter()
        .find(|r| r.session_id == session_1)
        .expect("session_1 must be in expected manifest rows");

    assert!(
        matches!(s1_row.outcome, verify::SessionOutcome::Unverifiable { .. }),
        "Corrupted manifest must report Unverifiable, got {:?}",
        s1_row.outcome
    );
    assert_eq!(s1_row.basis, verify::ExpectationBasis::StoredManifest);

    assert_eq!(rep.unverifiable(), 1);
    assert_eq!(rep.failed(), 0);
    assert!(!rep.ok());
    drop(dir);
}
