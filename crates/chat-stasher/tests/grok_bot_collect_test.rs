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
        session_dir: None,
        movable_root: false,
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

/// The full session id of the fixture's one agent.
fn session_id() -> String {
    format!("grok-bot.synthetic-machine.{AGENT}")
}

/// Count how often one raw row content occurs in the stage body: byte-identical
/// rows delivered twice are the cross-shard duplicate shape W986 exists for.
fn row_content_count(body: &[u8], needle: &str) -> usize {
    String::from_utf8_lossy(body)
        .lines()
        .filter(|line| line.contains(needle))
        .count()
}

/// Seed one grok-bot persistence directory — one path this session is read
/// from — with the given entries, and return the replica blob's path.
fn persistence(
    sandbox_root: &std::path::Path,
    name: &str,
    entries: Vec<Value>,
) -> std::path::PathBuf {
    let dir = sandbox_root.join(name);
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join(replica_blob_name());
    fs::write(&source, replica_payload(entries)).unwrap();
    source
}

fn collect_paths(
    paths: &[&std::path::Path],
    stage: &std::path::Path,
    state: &std::path::Path,
    destination: &DestinationView<'_>,
) -> collect::CollectReport {
    let mut scan = ScanReport::default();
    for path in paths {
        scan.records.push(record(path));
    }
    collect::collect_scan_report(&scan, stage, "synthetic-machine", state, 20, destination).unwrap()
}

/// W986: the same session read from a second path delivers nothing the session
/// has already delivered. The delivered set answers for the session, so a
/// path switch — the app moving its persistence directory, a
/// `[harness_roots]` rescue feed, a second persistence directory — is not a
/// first read for a new source.
#[test]
fn a_session_read_from_a_new_path_delivers_nothing_new() {
    let sandbox = test_support::Sandbox::new();
    let path_a = persistence(
        sandbox.root(),
        "persistence-first",
        vec![entry(1, "synthetic-one"), entry(2, "synthetic-two")],
    );
    let path_b = persistence(
        sandbox.root(),
        "persistence-second",
        vec![entry(1, "synthetic-one"), entry(2, "synthetic-two")],
    );
    let stage = sandbox.data_home().join("stage");
    let state = sandbox.state_home().join("chat-stasher");
    let destination = DestinationView::unreachable("synthetic-destination");
    let session_id = session_id();

    let first = collect_paths(&[&path_a], &stage, &state, &destination);
    assert_eq!(
        first.shards_written, 1,
        "premise: the first path seeds a shard"
    );

    let second = collect_paths(&[&path_b], &stage, &state, &destination);
    assert_eq!(
        second.shards_written, 0,
        "an already-delivered session read from a new path must not seal a second shard"
    );
    assert_eq!(
        second.changed_records, 0,
        "an unchanged session delivered through a new path stays unchanged"
    );
    let body = store::concat_shards(&stage, "synthetic-machine", &session_id).unwrap();
    assert_eq!(
        row_content_count(&body, "synthetic-one"),
        1,
        "the union row must be stored exactly once"
    );
    assert_eq!(row_content_count(&body, "synthetic-two"), 1);
    assert_eq!(
        store::sealed_shard_entries(&store::session_shard_dir(
            &stage,
            "synthetic-machine",
            &session_id
        ))
        .unwrap()
        .len(),
        1
    );
}

/// W986: a new path for the same session delivers only the rows the session
/// has not delivered yet — a whole-body re-seal with the union's old rows
/// beside the new ones is the duplicate shape this task measured.
#[test]
fn a_new_path_delivers_only_rows_the_session_has_not_delivered() {
    let sandbox = test_support::Sandbox::new();
    let path_a = persistence(
        sandbox.root(),
        "persistence-first",
        vec![entry(1, "synthetic-one"), entry(2, "synthetic-two")],
    );
    let path_b = persistence(
        sandbox.root(),
        "persistence-second",
        vec![
            entry(1, "synthetic-one"),
            entry(2, "synthetic-two"),
            entry(3, "synthetic-three"),
            entry(4, "synthetic-four"),
            entry(5, "synthetic-five"),
        ],
    );
    let stage = sandbox.data_home().join("stage");
    let state = sandbox.state_home().join("chat-stasher");
    let destination = DestinationView::unreachable("synthetic-destination");
    let session_id = session_id();

    let first = collect_paths(&[&path_a], &stage, &state, &destination);
    assert_eq!(
        first.shards_written, 1,
        "premise: the first path seeds a shard"
    );

    let second = collect_paths(&[&path_b], &stage, &state, &destination);
    assert_eq!(second.shards_written, 1, "the new rows seal one new shard");
    // Three new rows and the metadata line; the two already-delivered rows
    // stay out of the payload.
    assert_eq!(
        second.lines_written, 4,
        "exactly the three new rows and the metadata line"
    );
    let body = store::concat_shards(&stage, "synthetic-machine", &session_id).unwrap();
    for needle in [
        "synthetic-one",
        "synthetic-two",
        "synthetic-three",
        "synthetic-four",
        "synthetic-five",
    ] {
        assert_eq!(
            row_content_count(&body, needle),
            1,
            "row {needle} must be stored exactly once: five distinct rows, no repeats"
        );
    }
}

/// W986: state files written before the session key carry the same sessions
/// under their source paths (in this build's key shape, `<path>\0<session
/// id>`). Those entries still name their session, so loading them re-keys
/// each one onto its session without re-delivering anything — several path
/// entries for one session union their delivered sets into that one entry.
#[test]
fn path_keyed_debts_migrate_to_the_session_key_without_re_delivery() {
    let sandbox = test_support::Sandbox::new();
    let path_a = persistence(
        sandbox.root(),
        "persistence-first",
        vec![entry(1, "synthetic-one"), entry(2, "synthetic-two")],
    );
    let path_b = persistence(
        sandbox.root(),
        "persistence-second",
        vec![entry(1, "synthetic-one"), entry(2, "synthetic-two")],
    );
    let path_c = persistence(
        sandbox.root(),
        "persistence-third",
        vec![entry(1, "synthetic-one"), entry(2, "synthetic-two")],
    );
    let stage = sandbox.data_home().join("stage");
    let state = sandbox.state_home().join("chat-stasher");
    let destination = DestinationView::unreachable("synthetic-destination");
    let session_id = session_id();

    let first = collect_paths(&[&path_a], &stage, &state, &destination);
    assert_eq!(
        first.shards_written, 1,
        "premise: the first pass seeds a shard"
    );

    // Re-shape the state file into the pre-session-key shape an older build
    // wrote: the same debt entry, keyed first by one source path, then split
    // across two earlier paths — each holding part of the delivered set, so
    // only their union covers the session.
    let old_key = |path: &std::path::Path| {
        format!(
            "{}\0{session_id}",
            path.canonicalize().unwrap().to_string_lossy()
        )
    };
    let state_path = state.join("debts-v2.json");
    let mut state_file: Value =
        serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    let files = state_file["destinations"]["synthetic-destination"]["files"]
        .as_object_mut()
        .unwrap();
    let (recorded_key, entry) = files
        .iter()
        .next()
        .map(|(key, value)| (key.clone(), value.clone()))
        .expect("the first pass must have recorded this session's debt");
    assert_eq!(entry["session_id"], session_id);
    assert!(
        !recorded_key.contains('\0') || recorded_key.contains(&session_id),
        "premise: the recorded key is the one entry the first pass wrote"
    );
    let digests = entry["cursor"]["grok_bot_row_digests"]
        .as_array()
        .expect("the first pass must have recorded the delivered digests")
        .clone();
    assert_eq!(digests.len(), 2);
    files.clear();
    let mut split = entry.clone();
    split["cursor"]["grok_bot_row_digests"] = Value::Array(vec![digests[0].clone()]);
    files.insert(old_key(&path_a), split);
    let mut split = entry.clone();
    split["cursor"]["grok_bot_row_digests"] = Value::Array(vec![digests[1].clone()]);
    files.insert(old_key(&path_c), split);
    fs::write(&state_path, serde_json::to_vec_pretty(&state_file).unwrap()).unwrap();

    // Reading the same session through a third path delivers nothing: the
    // path-keyed entries seeded the session's delivered set at load time,
    // and their union — not either one alone — is the delivered set.
    let second = collect_paths(&[&path_b], &stage, &state, &destination);
    assert_eq!(
        second.shards_written, 0,
        "migrated path entries must seed the session's delivered set, not a first read"
    );
    let body = store::concat_shards(&stage, "synthetic-machine", &session_id).unwrap();
    assert_eq!(
        row_content_count(&body, "synthetic-one"),
        1,
        "no re-delivery through either migrated path entry"
    );
    assert_eq!(row_content_count(&body, "synthetic-two"), 1);
}

/// W986: the rescue shape — two roots the same session is read through in
/// one and the same pass (a real cache beside a `[harness_roots]` feed) —
/// delivers each row once across the pass: the first path seeds the
/// session's delivered set, the second path adds only its new rows to it.
#[test]
fn two_paths_of_one_session_in_one_pass_deliver_the_union_once() {
    let sandbox = test_support::Sandbox::new();
    let path_a = persistence(
        sandbox.root(),
        "persistence-first",
        vec![entry(1, "synthetic-one"), entry(2, "synthetic-shared")],
    );
    let path_b = persistence(
        sandbox.root(),
        "persistence-second",
        vec![
            entry(2, "synthetic-shared"),
            entry(3, "synthetic-exclusive-to-the-feed"),
        ],
    );
    let stage = sandbox.data_home().join("stage");
    let state = sandbox.state_home().join("chat-stasher");
    let destination = DestinationView::unreachable("synthetic-destination");
    let session_id = session_id();

    let first = collect_paths(&[&path_a, &path_b], &stage, &state, &destination);
    let body = store::concat_shards(&stage, "synthetic-machine", &session_id).unwrap();
    for needle in [
        "synthetic-one",
        "synthetic-shared",
        "synthetic-exclusive-to-the-feed",
    ] {
        assert_eq!(
            row_content_count(&body, needle),
            1,
            "row {needle} must be delivered once across both paths of the same pass"
        );
    }
    assert_eq!(first.errors.len(), 0);
    // Both records answered under one session key, so a replay of the same
    // two-path scan delivers nothing at all.
    let second = collect_paths(&[&path_a, &path_b], &stage, &state, &destination);
    assert_eq!(second.shards_written, 0);
    assert_eq!(second.lines_written, 0);
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
