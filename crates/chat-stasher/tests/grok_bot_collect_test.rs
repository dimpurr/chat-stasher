#[path = "../src/test_support.rs"]
mod test_support;

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::models::{HarnessSource, SessionRecord, SqliteSessionLayout};
use chat_stasher::scanner::ScanReport;
use chat_stasher::store;
use serde_json::{json, Value};
use std::fs;
use std::time::SystemTime;

const AGENT: &str = "11111111-2222-4333-8444-555555555555";
const ACCOUNT: &str = "grok%7Cuser_synthetic-0000";

/// The fixture blob's on-disk name: the app stores each state under the
/// base32-encoded state key with a `.blob` suffix (W321).
fn replica_blob_name() -> String {
    format!(
        "{}.blob",
        test_support::grok_bot_blob_name(&format!(
            "sand.client.slice.account.{ACCOUNT}.transcript.replicas.{AGENT}"
        ))
    )
}

/// Same, for a replica key with no `account.<ref>` segment: no tenancy.
fn unscoped_replica_blob_name() -> String {
    format!(
        "{}.blob",
        test_support::grok_bot_blob_name(&format!("sand.client.slice.transcript.replicas.{AGENT}"))
    )
}

/// The roster-shaped blob that names an agent (W321 key names).
fn roster_blob_name() -> String {
    format!(
        "{}.blob",
        test_support::grok_bot_blob_name(&format!(
            "sand.client.slice.account.{ACCOUNT}.roster.last-roster"
        ))
    )
}

/// One app-shaped entry (key names from W321/W319b) for a given sequence.
fn entry(sequence: u64, content: &str) -> Value {
    json!({
        "id": format!("synthetic-{sequence}"),
        "kind": "message",
        "message": {"content": content, "role": "user"},
        "seq": sequence,
        "timestampMs": 1780000000000u64 + sequence * 100,
    })
}

/// The full replica payload: `{"schemaVersion":…,"value":{"entries":[…]}}`
/// with the wrapper metadata key names W321 observed.
fn replica_payload(entries: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "schemaVersion": 1,
        "value": {
            "acceptedSequenceHint": 0,
            "entries": entries,
            "epochHint": "synthetic-epoch",
            "persistedAt": 1780000000000u64,
        }
    }))
    .unwrap()
}

fn record(path: &std::path::Path) -> SessionRecord {
    SessionRecord {
        id: format!("grok-bot.synthetic-machine.{AGENT}"),
        absolute_path: path.to_path_buf(),
        byte_size: fs::metadata(path).unwrap().len(),
        mtime: SystemTime::now(),
        source: HarnessSource::GrokBot,
        compressed: false,
        sqlite_layout: Some(SqliteSessionLayout::GrokBot),
        provenance: Default::default(),
    }
}

fn sequence_rows(bytes: &[u8]) -> Vec<Value> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_slice(line).ok())
        .filter(|row: &Value| row.get("seq").is_some())
        .collect()
}

#[test]
fn collection_keeps_raw_rows_and_accumulates_a_sequence_union() {
    let sandbox = test_support::Sandbox::new();
    let persistence = sandbox.root().join("persistence");
    fs::create_dir_all(&persistence).unwrap();
    let source = persistence.join(replica_blob_name());
    fs::write(
        &source,
        replica_payload(vec![entry(1, "synthetic-one"), entry(3, "synthetic-three")]),
    )
    .unwrap();

    let scan_report = |source: &std::path::Path| {
        let mut scan = ScanReport::default();
        scan.records.push(record(source));
        scan
    };
    let stage = sandbox.data_home().join("stage");
    let state = sandbox.state_home().join("chat-stasher");
    let destination = DestinationView::unreachable("synthetic-destination");

    let first_run = scan_report(&source);
    collect::collect_scan_report(
        &first_run,
        &stage,
        "synthetic-machine",
        &state,
        20,
        &destination,
    )
    .unwrap();
    let session_id = &first_run.records[0].id;
    let first_bytes = store::concat_shards(&stage, "synthetic-machine", session_id).unwrap();
    let first_rows = sequence_rows(&first_bytes);
    assert_eq!(first_rows.len(), 2);
    assert!(String::from_utf8_lossy(&first_bytes).contains("\"partial_replica\":true"));
    assert!(String::from_utf8_lossy(&first_bytes).contains("\"sequence_gaps\":[2]"));

    // The app can rewrite a stored record in place (mutable fields such as
    // `isStreaming` are in the observed key union, W319b). A changed row is
    // archived as an additional variant at the same sequence — the earlier
    // observation is never overwritten — and a new sequence merges in.
    fs::write(
        &source,
        replica_payload(vec![
            entry(2, "synthetic-two"),
            entry(3, "synthetic-three-revised"),
        ]),
    )
    .unwrap();
    let second_run = scan_report(&source);
    collect::collect_scan_report(
        &second_run,
        &stage,
        "synthetic-machine",
        &state,
        20,
        &destination,
    )
    .unwrap();
    let all_bytes = store::concat_shards(&stage, "synthetic-machine", session_id).unwrap();
    let rows = sequence_rows(&all_bytes);
    let mut sequences: Vec<u64> = rows
        .iter()
        .map(|row| row["seq"].as_u64().unwrap())
        .collect();
    sequences.sort_unstable();
    assert_eq!(sequences, vec![1, 2, 3, 3]);
    assert!(rows
        .iter()
        .any(|row| row["message"]["content"] == "synthetic-one"));
    assert!(rows
        .iter()
        .any(|row| row["message"]["content"] == "synthetic-two"));
    assert!(rows
        .iter()
        .any(|row| row["message"]["content"] == "synthetic-three"));
    assert!(rows
        .iter()
        .any(|row| row["message"]["content"] == "synthetic-three-revised"));
    assert!(
        rows.len() == 4,
        "a replayed or re-observed identical row is not archived twice"
    );

    // A third run over unchanged data delivers nothing new.
    let third_run = scan_report(&source);
    collect::collect_scan_report(
        &third_run,
        &stage,
        "synthetic-machine",
        &state,
        20,
        &destination,
    )
    .unwrap();
    let stable_bytes = store::concat_shards(&stage, "synthetic-machine", session_id).unwrap();
    assert_eq!(sequence_rows(&stable_bytes).len(), 4);
}

fn dimensions_observation(
    stage: &std::path::Path,
    session_id: &str,
) -> chat_stasher::provenance::ProvenanceObservation {
    let observations =
        chat_stasher::provenance::read_observations(stage, "synthetic-machine").unwrap();
    observations
        .into_iter()
        .find(|observation| observation.session_id == session_id)
        .expect("the collected session carries a provenance observation")
}

#[test]
fn collection_projects_the_tenant_and_agent_into_dimensions() {
    let sandbox = test_support::Sandbox::new();
    let persistence = sandbox.root().join("persistence");
    fs::create_dir_all(&persistence).unwrap();
    let source = persistence.join(replica_blob_name());
    fs::write(&source, replica_payload(vec![entry(1, "synthetic-one")])).unwrap();
    // The roster names the agent, so `container` holds both identities the
    // ticket defines: the agent uuid and the name the user knows it by.
    fs::write(
        persistence.join(roster_blob_name()),
        serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "value": {"rows": [{
                "id": AGENT,
                "name": "synthetic-agent-name",
                "createdAt": 1780000000000u64,
                "updatedAt": 1780000000000u64,
            }]}
        }))
        .unwrap(),
    )
    .unwrap();

    let mut scan = ScanReport::default();
    scan.records.push(record(&source));
    let stage = sandbox.data_home().join("stage");
    let state = sandbox.state_home().join("chat-stasher");
    let destination = DestinationView::unreachable("synthetic-destination");
    collect::collect_scan_report(&scan, &stage, "synthetic-machine", &state, 20, &destination)
        .unwrap();

    let observation = dimensions_observation(&stage, &scan.records[0].id);
    assert_eq!(observation.dimensions.tenant, [ACCOUNT]);
    assert_eq!(
        observation.dimensions.container,
        [AGENT, "synthetic-agent-name"]
    );
}

#[test]
fn an_unscoped_state_key_leaves_tenant_unobserved_but_names_the_container() {
    let sandbox = test_support::Sandbox::new();
    let persistence = sandbox.root().join("persistence");
    fs::create_dir_all(&persistence).unwrap();
    let source = persistence.join(unscoped_replica_blob_name());
    fs::write(&source, replica_payload(vec![entry(1, "synthetic-one")])).unwrap();

    let mut scan = ScanReport::default();
    scan.records.push(record(&source));
    let stage = sandbox.data_home().join("stage");
    let state = sandbox.state_home().join("chat-stasher");
    let destination = DestinationView::unreachable("synthetic-destination");
    collect::collect_scan_report(&scan, &stage, "synthetic-machine", &state, 20, &destination)
        .unwrap();

    // No `account.<ref>` in the key states no tenancy: the tenant dimension
    // stays empty — unobserved, never a placeholder account — while the
    // agent the key does name still lands in `container`.
    let observation = dimensions_observation(&stage, &scan.records[0].id);
    assert!(observation.dimensions.tenant.is_empty());
    assert_eq!(observation.dimensions.container, [AGENT]);
}
