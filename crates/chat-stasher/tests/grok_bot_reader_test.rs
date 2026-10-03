#[path = "../src/test_support.rs"]
mod test_support;

use chat_stasher::grok_bot::{merge_replica_records, read_persistence, ReplicaRecord};
use serde_json::json;
use std::fs;

#[test]
fn replica_records_are_retained_raw_and_sorted_by_sequence() {
    let _sandbox = test_support::Sandbox::new();
    let records = vec![
        ReplicaRecord::from_value(
            json!({"seq": 3, "kind": "message", "content": "synthetic-three"}),
        )
        .unwrap(),
        ReplicaRecord::from_value(
            json!({"seq": 1, "kind": "send-message", "content": "synthetic-one"}),
        )
        .unwrap(),
    ];
    let merged = merge_replica_records([], records).unwrap();
    assert_eq!(
        merged.iter().map(|r| r.sequence).collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(
        merged[0].raw,
        json!({"seq": 1, "kind": "send-message", "content": "synthetic-one"})
    );
    assert_eq!(merged[0].sequence_gaps, vec![2]);
}

#[test]
fn repeated_reads_accumulate_a_union_by_sequence() {
    let _sandbox = test_support::Sandbox::new();
    let first = vec![
        ReplicaRecord::from_value(json!({"seq": 1, "kind": "message", "id": "synthetic-a"}))
            .unwrap(),
        ReplicaRecord::from_value(json!({"seq": 4, "kind": "message", "id": "synthetic-d"}))
            .unwrap(),
    ];
    let second = vec![
        ReplicaRecord::from_value(json!({"seq": 2, "kind": "message", "id": "synthetic-b"}))
            .unwrap(),
        ReplicaRecord::from_value(json!({"seq": 4, "kind": "message", "id": "synthetic-d"}))
            .unwrap(),
    ];

    let accumulated = merge_replica_records(first, second).unwrap();
    assert_eq!(
        accumulated.iter().map(|r| r.sequence).collect::<Vec<_>>(),
        vec![1, 2, 4]
    );
    assert_eq!(accumulated[2].raw["id"], "synthetic-d");
    assert_eq!(accumulated[2].sequence_gaps, vec![3]);
}

#[test]
fn conflicting_records_for_one_sequence_are_not_silently_overwritten() {
    let _sandbox = test_support::Sandbox::new();
    let a = ReplicaRecord::from_value(json!({"seq": 2, "kind": "message", "id": "synthetic-a"}))
        .unwrap();
    let b = ReplicaRecord::from_value(json!({"seq": 2, "kind": "message", "id": "synthetic-b"}))
        .unwrap();
    assert!(merge_replica_records(vec![a], vec![b]).is_err());
}

#[test]
fn persistence_reader_keeps_raw_entries_and_gap_metadata() {
    let sandbox = test_support::Sandbox::new();
    let root = sandbox.root().join("persistence");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("transcript.replicas.11111111-2222-4333-8444-555555555555"),
        br#"[{"seq":1,"kind":"message","content":"synthetic-one","role":"user"},{"seq":3,"kind":"send-message","content":"synthetic-three","role":"assistant"}]"#,
    )
    .unwrap();

    let agents = read_persistence(&root).unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].name, None);
    assert!(agents[0].partial_replica);
    assert_eq!(agents[0].sequence_gaps, vec![2]);
    assert_eq!(agents[0].records[1].raw["content"], "synthetic-three");
}
