#[path = "../src/test_support.rs"]
mod test_support;

use chat_stasher::grok_bot::{
    merge_replica_records, read_persistence, sequence_gaps, ReplicaRecord,
};
use serde_json::{json, Value};
use std::fs;

/// Synthetic agent id, never a real one.
const AGENT: &str = "11111111-2222-4333-8444-555555555555";
/// Second synthetic agent id, used to prove a replica blob never names itself.
const SELF_NAMED: &str = "99999999-8888-4777-8666-555555555555";
/// Synthetic account fragment. The app scopes its state keys with a
/// `grok%7C…` account segment (W321: key names only; the value is synthetic).
const ACCOUNT: &str = "grok%7Cuser_synthetic-0000";

fn state_key(leaf: &str) -> String {
    format!("sand.client.slice.account.{ACCOUNT}.{leaf}")
}

fn replica_key(agent_id: &str) -> String {
    format!("sand.client.slice.account.{ACCOUNT}.transcript.replicas.{agent_id}")
}

fn blob_name(key: &str) -> String {
    format!("{}.blob", test_support::grok_bot_blob_name(key))
}

/// The app wraps every persisted value as `{"schemaVersion":…,"value":…}`
/// (W321); replicas hold `entries`, the roster holds `rows`.
fn wrapped(value: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"schemaVersion": 1, "value": value})).unwrap()
}

fn replica_bytes(entries: &[Value]) -> Vec<u8> {
    wrapped(json!({
        "acceptedSequenceHint": 0,
        "entries": entries,
        "epochHint": "synthetic-epoch",
        "persistedAt": 1780000000000u64,
    }))
}

/// One transcript entry shaped like the app's own: entry key names from W321
/// (`id`, `kind`, `message`, `seq`, `timestampMs`), the nested message object
/// carrying the `role`/`content` key names W319b observed.
fn entry(sequence: u64, content: &str) -> Value {
    json!({
        "id": format!("synthetic-{sequence}"),
        "kind": "message",
        "message": {"content": content, "role": "user"},
        "seq": sequence,
        "timestampMs": 1780000000000u64 + sequence * 100,
    })
}

fn record(sequence: u64, content: &str) -> ReplicaRecord {
    ReplicaRecord::from_value(entry(sequence, content)).unwrap()
}

#[test]
fn sequence_gaps_span_one_through_the_max_observed() {
    let _sandbox = test_support::Sandbox::new();
    // Sequence positions are numbered from one (W321), so 3 and 5 make 1, 2
    // and 4 missing — the head positions are gaps like any other.
    assert_eq!(sequence_gaps([3u64, 5]), vec![1, 2, 4]);
    assert_eq!(sequence_gaps([1u64, 3]), vec![2]);
    assert_eq!(sequence_gaps(Vec::<u64>::new()), Vec::<u64>::new());
}

#[test]
fn merge_keeps_raw_records_sorted_by_sequence() {
    let _sandbox = test_support::Sandbox::new();
    let merged = merge_replica_records(
        [],
        vec![record(3, "synthetic-three"), record(1, "synthetic-one")],
    );
    assert_eq!(
        merged
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(merged[0].raw["message"]["content"], "synthetic-one");
    assert_eq!(merged[1].raw["message"]["content"], "synthetic-three");
}

#[test]
fn repeated_reads_accumulate_a_union_by_sequence() {
    let _sandbox = test_support::Sandbox::new();
    let first = vec![record(1, "synthetic-one"), record(4, "synthetic-four")];
    let second = vec![record(2, "synthetic-two"), record(4, "synthetic-four")];

    let accumulated = merge_replica_records(first, second);
    assert_eq!(
        accumulated
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 4]
    );
    assert_eq!(accumulated[2].raw["message"]["content"], "synthetic-four");
}

#[test]
fn a_changed_record_at_one_sequence_is_kept_as_an_additional_variant() {
    let _sandbox = test_support::Sandbox::new();
    // The observed key union includes mutable fields such as `isStreaming`
    // (W319b), so the app can legitimately rewrite a record at one position.
    // Both raw observations are kept — first seen first — and re-observing
    // the same pair duplicates neither.
    let first = vec![record(3, "synthetic-three")];
    let second = vec![
        record(3, "synthetic-three"),
        record(3, "synthetic-three-revised"),
    ];

    let merged = merge_replica_records(first, second.clone());
    let contents = merged
        .iter()
        .map(|record| record.raw["message"]["content"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(contents, vec!["synthetic-three", "synthetic-three-revised"]);

    let remerged = merge_replica_records(merged, second);
    assert_eq!(
        remerged
            .iter()
            .map(|record| record.raw["message"]["content"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["synthetic-three", "synthetic-three-revised"]
    );
}

#[test]
fn reads_base32_named_replica_blobs_wrapped_like_local_storage() {
    let sandbox = test_support::Sandbox::new();
    let root = sandbox.root().join("persistence");
    fs::create_dir_all(&root).unwrap();
    // The unencoded migration sentinel (W321); the reader must skip what it
    // does not model, never stumble on it.
    fs::write(
        root.join(".migrated-from-local-storage"),
        b"migrated-sentinel-24byte",
    )
    .unwrap();
    fs::write(
        root.join(blob_name(&replica_key(AGENT))),
        replica_bytes(&[entry(1, "synthetic-one"), entry(3, "synthetic-three")]),
    )
    .unwrap();

    let agents = read_persistence(&root).unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].agent_id, AGENT);
    assert_eq!(agents[0].name, None);
    assert!(agents[0].partial_replica);
    assert_eq!(agents[0].sequence_gaps, vec![2]);
    assert_eq!(agents[0].max_sequence, Some(3));
    assert_eq!(agents[0].records.len(), 2);
    assert_eq!(
        agents[0].records[0].raw["message"]["content"],
        "synthetic-one"
    );
    assert_eq!(agents[0].records[0].raw["seq"], 1);
    assert!(agents[0]
        .source_path
        .ends_with(blob_name(&replica_key(AGENT))));
}

#[test]
fn head_gaps_before_the_first_stored_sequence_are_reported() {
    let sandbox = test_support::Sandbox::new();
    let root = sandbox.root().join("persistence");
    fs::create_dir_all(&root).unwrap();
    // W319b observed real replica sequence values starting at 2: the history
    // before the first stored row must be reported as absent, not implied.
    fs::write(
        root.join(blob_name(&replica_key(AGENT))),
        replica_bytes(&[entry(2, "synthetic-two"), entry(4, "synthetic-four")]),
    )
    .unwrap();

    let agents = read_persistence(&root).unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].sequence_gaps, vec![1, 3]);
    assert_eq!(agents[0].max_sequence, Some(4));
}

#[test]
fn roster_blob_names_an_agent_and_no_replica_names_itself() {
    let sandbox = test_support::Sandbox::new();
    let root = sandbox.root().join("persistence");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join(blob_name(&replica_key(AGENT))),
        replica_bytes(&[entry(1, "synthetic-one")]),
    )
    .unwrap();
    // A name-shaped object planted inside a replica blob: if the name pass
    // wrongly scanned replica payloads, this agent would carry a name.
    let mut decoy = entry(1, "synthetic-self-named");
    decoy["id"] = json!(SELF_NAMED);
    decoy["name"] = json!("synthetic-self-name");
    fs::write(
        root.join(blob_name(&replica_key(SELF_NAMED))),
        replica_bytes(&[decoy]),
    )
    .unwrap();
    // The transcript-selection state key holds an agent id, not a replica
    // (W321 lists it among the non-replica state keys); it must not become
    // an agent.
    fs::write(
        root.join(blob_name(&state_key("selection.last-agent"))),
        wrapped(json!({"agentId": AGENT})),
    )
    .unwrap();
    // Roster row key names from W321; values synthetic.
    fs::write(
        root.join(blob_name(&state_key("roster.last-roster"))),
        wrapped(json!({"rows": [{
            "id": AGENT,
            "name": "synthetic-agent-name",
            "description": "synthetic-description",
            "title": "synthetic-title",
            "createdAt": 1780000000000u64,
            "updatedAt": 1780000000000u64,
        }]})),
    )
    .unwrap();

    let agents = read_persistence(&root).unwrap();
    assert_eq!(agents.len(), 2);
    let named = agents.iter().find(|agent| agent.agent_id == AGENT).unwrap();
    assert_eq!(named.name.as_deref(), Some("synthetic-agent-name"));
    let decoy_agent = agents
        .iter()
        .find(|agent| agent.agent_id == SELF_NAMED)
        .unwrap();
    assert_eq!(decoy_agent.name, None);
}

#[test]
fn two_account_scoped_keys_for_one_agent_merge_into_one_agent() {
    let sandbox = test_support::Sandbox::new();
    let root = sandbox.root().join("persistence");
    fs::create_dir_all(&root).unwrap();
    // More than one state key can end in the same agent uuid (two account
    // scopes); they are observations of one agent, not two sessions.
    let scoped = |account: &str| {
        format!("sand.client.slice.account.grok%7Cuser_{account}.transcript.replicas.{AGENT}")
    };
    fs::write(
        root.join(blob_name(&scoped("synthetic-first"))),
        replica_bytes(&[entry(1, "synthetic-one")]),
    )
    .unwrap();
    fs::write(
        root.join(blob_name(&scoped("synthetic-second"))),
        replica_bytes(&[entry(3, "synthetic-three")]),
    )
    .unwrap();

    let agents = read_persistence(&root).unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(
        agents[0]
            .records
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(agents[0].sequence_gaps, vec![2]);
    assert_eq!(agents[0].max_sequence, Some(3));
}

#[test]
fn an_empty_persistence_directory_is_read_as_no_agents() {
    let sandbox = test_support::Sandbox::new();
    let root = sandbox.root().join("persistence");
    fs::create_dir_all(&root).unwrap();
    let agents = read_persistence(&root).unwrap();
    assert!(agents.is_empty());
}
