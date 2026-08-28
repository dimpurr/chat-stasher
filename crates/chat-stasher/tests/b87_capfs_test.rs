//! B87-CAPFS: prove shard bucket limits from the filesystem, not from a
//! reported counter. Every write below uses synthetic bytes in a tempfile.

use chat_stasher::{readback, store};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug)]
struct ObservedLayout {
    _sandbox: tempfile::TempDir,
    bucket_counts: BTreeMap<String, usize>,
    shard_buckets: BTreeMap<u64, String>,
    total_entries: usize,
}

fn write_and_measure(cap: usize, writes: usize) -> ObservedLayout {
    let sandbox = tempfile::tempdir().unwrap();
    let stage = sandbox.path();
    let machine = "b87-machine";
    let session = "b87-session";

    for _ in 0..writes {
        store::write_sealed_shard_with_cap(
            store::StageWriter::Collect,
            stage,
            machine,
            session,
            &["b87 synthetic shard".to_string()],
            cap,
        )
        .unwrap();
    }

    let session_dir = store::session_shard_dir(stage, machine, session);
    let mut bucket_counts = BTreeMap::new();
    let mut shard_buckets = BTreeMap::new();

    for bucket_entry in fs::read_dir(&session_dir).unwrap() {
        let bucket_entry = bucket_entry.unwrap();
        // The session dir also holds the shard sequence counter metadata file
        // (ADR-020 Phase 2); only bucket directories take part in the census.
        if !bucket_entry.file_type().unwrap().is_dir() {
            continue;
        }
        let bucket_name = bucket_entry.file_name().to_string_lossy().into_owned();
        let children: Vec<_> = fs::read_dir(bucket_entry.path())
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        bucket_counts.insert(bucket_name.clone(), children.len());

        for child in children {
            assert!(child.file_type().unwrap().is_file());
            let shard_name = child.file_name().to_string_lossy().into_owned();
            let seq = store::parse_shard_seq(&shard_name).unwrap();
            assert!(shard_buckets.insert(seq, bucket_name.clone()).is_none());
        }
    }

    let total_entries = bucket_counts.values().sum();
    ObservedLayout {
        _sandbox: sandbox,
        bucket_counts,
        shard_buckets,
        total_entries,
    }
}

fn assert_case(cap: usize, writes: usize) -> ObservedLayout {
    let observed = write_and_measure(cap, writes);
    let effective_cap = cap.max(1);
    let expected_buckets = writes.div_ceil(effective_cap);

    assert_eq!(observed.bucket_counts.len(), expected_buckets);
    assert_eq!(observed.total_entries, writes);
    assert_eq!(observed.shard_buckets.len(), writes);
    assert_eq!(
        observed.shard_buckets.keys().copied().collect::<Vec<_>>(),
        (1..=writes as u64).collect::<Vec<_>>()
    );
    assert!(observed
        .bucket_counts
        .values()
        .all(|count| *count <= effective_cap));

    println!(
        "B87 cap={cap} bucket_count={} total_entries={}",
        observed.bucket_counts.len(),
        observed.total_entries
    );
    for (bucket, count) in &observed.bucket_counts {
        println!("B87 cap={cap} bucket={bucket} entries={count}");
    }
    observed
}

fn assert_boundary(observed: &ObservedLayout, cap: usize) {
    let at_cap = cap as u64;
    let after_cap = at_cap + 1;
    assert_eq!(
        observed.shard_buckets.get(&at_cap),
        Some(&"000".to_string())
    );
    assert_eq!(
        observed.shard_buckets.get(&after_cap),
        Some(&"001".to_string())
    );
}

#[test]
fn b87_filesystem_counts_bound_every_requested_cap() {
    let default_cap = assert_case(store::DEFAULT_SHARD_BUCKET_CAP, 50);
    assert_boundary(&default_cap, store::DEFAULT_SHARD_BUCKET_CAP);

    let cap_one = assert_case(1, 8);
    assert_boundary(&cap_one, 1);

    let cap_seven = assert_case(7, 22);
    assert_boundary(&cap_seven, 7);

    let cap_zero = assert_case(0, 8);
    assert_eq!(cap_zero.bucket_counts, cap_one.bucket_counts);
    assert_eq!(cap_zero.shard_buckets, cap_one.shard_buckets);
    assert_eq!(store::shard_bucket_name(0, 0), "000");
    assert_eq!(
        store::shard_bucket_name(1, 0),
        store::shard_bucket_name(1, 1)
    );
    assert_eq!(
        store::shard_bucket_name(2, 0),
        store::shard_bucket_name(2, 1)
    );
    assert_eq!(cap_zero.shard_buckets.get(&1), Some(&"000".to_string()));
    assert_eq!(cap_zero.shard_buckets.get(&2), Some(&"001".to_string()));
}

#[test]
fn b87_four_digit_bucket_name_is_parsed_by_path_depth() {
    let bucket = store::shard_bucket_name(20_001, 20);
    assert_eq!(bucket, "1000");
    assert_eq!(
        readback::bucket_shard_path(Path::new(
            "/synthetic-stage/sessions/b87-machine/b87-session/1000/020001.jsonl",
        )),
        Some((
            "b87-machine".to_string(),
            "b87-session".to_string(),
            "020001.jsonl".to_string(),
        ))
    );
}
