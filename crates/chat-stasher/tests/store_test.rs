//! Black-box tests for BackupStore helpers: sealed-shard layout/write/read-back
//! and the masterkey file round-trip. All data is synthetic and lives in temp
//! dirs — nothing here touches a real repository or real sessions.

use chat_stasher::store;
use rustic_core::repofile::MasterKey;
use std::fs;
use std::path::PathBuf;

#[test]
fn sealed_shards_roundtrip_in_partitioned_layout() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path();
    let machine = "mbp-spike-1";
    let session = "synthetic-0001";

    let mut total = Vec::new();
    for seq in 1..=3u64 {
        let lines: Vec<String> = (0..50)
            .map(|i| {
                format!(
                    "{{\"seq\":{seq},\"i\":{i},\"s\":\"payload-{}-{}\"}}",
                    seq, i
                )
            })
            .collect();
        store::write_sealed_shard(stage, machine, session, &lines).unwrap();
        for l in &lines {
            total.extend_from_slice(l.as_bytes());
            total.push(b'\n');
        }
    }

    // Layout is exactly the partition.
    assert_eq!(
        store::session_shard_dir(stage, machine, session),
        stage.join("sessions").join(machine).join(session)
    );
    let mut names: Vec<_> = store::sealed_shard_entries(&store::session_shard_dir(stage, machine, session))
        .unwrap()
        .into_iter()
        .map(|(_, path)| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["000001.jsonl", "000002.jsonl", "000003.jsonl"]);

    // Reading the concatenation in seq order matches what was written.
    assert_eq!(store::next_shard_seq(stage, machine, session), 4);
    let expected = store::expected_concat_sha(stage, machine, session).unwrap();
    assert_eq!(expected.len(), 64);

    drop(dir);
}

#[test]
fn next_shard_seq_continues_after_latest() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path();
    store::write_sealed_shard(stage, "m", "s", &[format!("{{\"a\":1}}")]).unwrap();
    store::write_sealed_shard(stage, "m", "s", &[format!("{{\"a\":2}}")]).unwrap();
    assert_eq!(store::next_shard_seq(stage, "m", "s"), 3);
    assert_eq!(
        store::shard_path(stage, "m", "s", 3),
        PathBuf::from(stage.join("sessions/m/s/000/000003.jsonl"))
    );
    drop(dir);
}

#[test]
fn bucket_cap_keeps_single_bucket_bounded_and_reads_legacy_mix() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path();
    let machine = "m";
    let session = "s";

    for i in 0..21 {
        store::write_sealed_shard_with_cap(
            stage,
            machine,
            session,
            &[format!("{{\"i\":{i}}}")],
            20,
        )
        .unwrap();
    }
    let session_dir = store::session_shard_dir(stage, machine, session);
    let bucket_000 = session_dir.join("000");
    let bucket_001 = session_dir.join("001");
    assert_eq!(fs::read_dir(bucket_000).unwrap().count(), 20);
    assert_eq!(fs::read_dir(bucket_001).unwrap().count(), 1);

    // A legacy direct shard remains readable alongside newly bucketed shards.
    fs::write(session_dir.join("000022.jsonl"), b"legacy\n").unwrap();
    let entries = store::sealed_shard_entries(&session_dir).unwrap();
    assert_eq!(entries.len(), 22);
    assert_eq!(store::next_shard_seq(stage, machine, session), 23);
    let concat = store::concat_shards(stage, machine, session).unwrap();
    assert!(concat.ends_with(b"legacy\n"));
    drop(dir);
}

#[test]
fn masterkey_persists_and_roundtrips() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = store::StoreConfig {
        repo_root: dir.path().join("repo").to_string_lossy().into_owned(),
        key_file: dir.path().join("masterkey.json"),
        connections: 10,
        options: Default::default(),
    };
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let reloaded = store::load_key_file(&cfg).unwrap();
    assert_eq!(reloaded.mac.k, mk.mac.k);
    assert_eq!(reloaded.mac.r, mk.mac.r);
    assert_eq!(reloaded.encrypt, mk.encrypt);
    drop(dir);
}
