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

fn record(path: &std::path::Path) -> SessionRecord {
    SessionRecord {
        id: format!("grok-bot.synthetic-machine.{AGENT}"),
        absolute_path: path.to_path_buf(),
        byte_size: fs::metadata(path).unwrap().len(),
        mtime: SystemTime::now(),
        source: HarnessSource::GrokBot,
        compressed: false,
        sqlite_layout: Some(SqliteSessionLayout::GrokBot),
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
    let source = persistence.join(format!("transcript.replicas.{AGENT}"));
    let first = json!([
        {"seq":1,"kind":"message","role":"user","content":"synthetic-one"},
        {"seq":3,"kind":"send-message","role":"assistant","content":"synthetic-three"}
    ]);
    fs::write(&source, serde_json::to_vec(&first).unwrap()).unwrap();

    let mut scan = ScanReport::default();
    scan.records.push(record(&source));
    let stage = sandbox.data_home().join("stage");
    let state = sandbox.state_home().join("chat-stasher");
    let destination = DestinationView::unreachable("synthetic-destination");
    collect::collect_scan_report(&scan, &stage, "synthetic-machine", &state, 20, &destination)
        .unwrap();
    let first_bytes =
        store::concat_shards(&stage, "synthetic-machine", &scan.records[0].id).unwrap();
    let first_rows = sequence_rows(&first_bytes);
    assert_eq!(first_rows.len(), 2);
    assert!(String::from_utf8_lossy(&first_bytes).contains("\"partial_replica\":true"));
    assert!(String::from_utf8_lossy(&first_bytes).contains("\"sequence_gaps\":[2]"));

    let second = json!([
        {"seq":2,"kind":"message","role":"user","content":"synthetic-two"},
        {"seq":3,"kind":"send-message","role":"assistant","content":"synthetic-three"}
    ]);
    fs::write(&source, serde_json::to_vec(&second).unwrap()).unwrap();
    scan.records[0] = record(&source);
    collect::collect_scan_report(&scan, &stage, "synthetic-machine", &state, 20, &destination)
        .unwrap();
    let all_bytes = store::concat_shards(&stage, "synthetic-machine", &scan.records[0].id).unwrap();
    let rows = sequence_rows(&all_bytes);
    let mut sequences: Vec<u64> = rows
        .iter()
        .map(|row| row["seq"].as_u64().unwrap())
        .collect();
    sequences.sort_unstable();
    assert_eq!(sequences, vec![1, 2, 3]);
    assert_eq!(rows.len(), 3, "replayed seq 3 is not archived twice");
    assert!(rows.iter().any(|row| row["content"] == "synthetic-one"));
    assert!(rows.iter().any(|row| row["content"] == "synthetic-two"));
}
