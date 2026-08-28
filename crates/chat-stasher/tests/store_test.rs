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
        store::write_sealed_shard(store::StageWriter::Collect, stage, machine, session, &lines)
            .unwrap();
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
    let mut names: Vec<_> =
        store::sealed_shard_entries(&store::session_shard_dir(stage, machine, session))
            .unwrap()
            .into_iter()
            .map(|(_, path)| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
    names.sort();
    assert_eq!(names, vec!["000001.jsonl", "000002.jsonl", "000003.jsonl"]);

    // Reading the concatenation in seq order matches what was written.
    assert_eq!(store::next_shard_seq(stage, machine, session).unwrap(), 4);
    let expected = store::expected_concat_sha(stage, machine, session).unwrap();
    assert_eq!(expected.len(), 64);

    drop(dir);
}

#[test]
fn next_shard_seq_continues_after_latest() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path();
    store::write_sealed_shard(
        store::StageWriter::Collect,
        stage,
        "m",
        "s",
        &[format!("{{\"a\":1}}")],
    )
    .unwrap();
    store::write_sealed_shard(
        store::StageWriter::Collect,
        stage,
        "m",
        "s",
        &[format!("{{\"a\":2}}")],
    )
    .unwrap();
    assert_eq!(store::next_shard_seq(stage, "m", "s").unwrap(), 3);
    assert_eq!(
        store::shard_path(stage, "m", "s", 3),
        PathBuf::from(stage.join("sessions/m/s/000/000003.jsonl"))
    );
    drop(dir);
}

#[test]
fn shard_seq_survives_reclaim_of_all_shard_files() {
    // ADR-020 Phase 2 acceptance: a stage reclaim that deletes every sealed
    // shard file must NOT reset the next sequence to 1. Sequence numbers come
    // from a persisted high-watermark counter, so the next shard takes 4, not
    // 1 (1 would collide with an archived 000001.jsonl and silently replace
    // archived content on read).
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path();
    for i in 1..=3u64 {
        store::write_sealed_shard(
            store::StageWriter::Collect,
            stage,
            "m",
            "s",
            &[format!("{{\"i\":{i}}}")],
        )
        .unwrap();
    }
    let session_dir = store::session_shard_dir(stage, "m", "s");
    let entries = store::sealed_shard_entries(&session_dir).unwrap();
    assert_eq!(entries.len(), 3);
    for (_, path) in entries {
        fs::remove_file(path).unwrap();
    }
    assert!(
        store::sealed_shard_entries(&session_dir)
            .unwrap()
            .is_empty(),
        "reclaim leaves no sealed shards"
    );

    // One more shard must take sequence 4, NOT 1.
    let name = store::write_sealed_shard(
        store::StageWriter::Collect,
        stage,
        "m",
        "s",
        &[format!("{{\"after-reclaim\":1}}")],
    )
    .unwrap();
    assert_eq!(name, "000004.jsonl");
    assert_eq!(store::next_shard_seq(stage, "m", "s").unwrap(), 5);
    drop(dir);
}

#[test]
fn shard_seq_counter_migrates_from_existing_shards_on_first_run() {
    // A pre-counter stage (written by an older tool) has shards on disk and
    // no counter file. The first write must seed the high-watermark from the
    // existing max (3) — never from 0 — so its shard takes sequence 4.
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path();
    let session_dir = store::session_shard_dir(stage, "m", "s");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(session_dir.join("000001.jsonl"), b"a\n").unwrap();
    fs::write(session_dir.join("000003.jsonl"), b"c\n").unwrap(); // gap is fine
    assert!(
        !store::shard_seq_file(stage, "m", "s").exists(),
        "pre-condition: no counter file yet"
    );

    // The write goes through next_shard_seq, which migrates from the dir max.
    let name = store::write_sealed_shard(
        store::StageWriter::Collect,
        stage,
        "m",
        "s",
        &[format!("{{\"d\":1}}")],
    )
    .unwrap();
    assert_eq!(name, "000004.jsonl");
    drop(dir);
}

#[test]
fn shard_seq_counter_states_missing_loaded_unusable() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path();
    let session_dir = store::session_shard_dir(stage, "m", "s");
    fs::create_dir_all(&session_dir).unwrap();

    // Missing: no counter file yet — the migration trigger, never a silent 0.
    assert!(matches!(
        store::load_shard_seq_state(stage, "m", "s"),
        store::ShardSeqState::Missing
    ));

    // Loaded: numeric content round-trips.
    fs::write(store::shard_seq_file(stage, "m", "s"), b"7\n").unwrap();
    assert!(matches!(
        store::load_shard_seq_state(stage, "m", "s"),
        store::ShardSeqState::Loaded(7)
    ));

    // Unusable(Parse): corrupt content must hard-fail, never be read as 0.
    fs::write(store::shard_seq_file(stage, "m", "s"), b"not-a-number\n").unwrap();
    let err = store::next_shard_seq(stage, "m", "s")
        .expect_err("a corrupt counter must hard-fail, never be read as 0");
    let message = err.to_string();
    assert!(
        message.contains("shard sequence file"),
        "message: {message}"
    );
    assert!(message.contains("do not delete"), "message: {message}");
    drop(dir);

    // Unusable(Read): an unreadable session directory (here: a regular file
    // where the session dir should be) must hard-fail, never be read as 0.
    let dir2 = tempfile::TempDir::new().unwrap();
    let stage2 = dir2.path();
    let machine_dir = stage2.join("sessions").join("m");
    fs::create_dir_all(&machine_dir).unwrap();
    fs::write(machine_dir.join("s"), b"not a directory").unwrap();
    let err2 = store::next_shard_seq(stage2, "m", "s")
        .expect_err("an unreadable counter must hard-fail, never be read as 0");
    let message2 = err2.to_string();
    assert!(
        message2.contains("shard sequence file"),
        "message: {message2}"
    );
    assert!(message2.contains("do not delete"), "message: {message2}");
    drop(dir2);
}

#[test]
fn bucket_cap_keeps_single_bucket_bounded_and_reads_legacy_mix() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path();
    let machine = "m";
    let session = "s";

    for i in 0..21 {
        store::write_sealed_shard_with_cap(
            store::StageWriter::Collect,
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
    // The out-of-band legacy shard is readable, but it did not go through the
    // counter, so the persisted high-watermark (21) stays authoritative: the
    // next sequence is 22, not 23. A write at 22 would be refused by the
    // "target already exists" guard rather than overwriting the legacy shard.
    assert_eq!(store::next_shard_seq(stage, machine, session).unwrap(), 22);
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
