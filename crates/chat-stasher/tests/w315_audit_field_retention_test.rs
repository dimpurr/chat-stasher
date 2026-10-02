//! W315 Step 1: audit-relevant raw fields survive collection, sealing, push,
//! and readback. Every input is synthetic and lives under the shared W306
//! Sandbox; rustic's metadata cache remains enabled and is pinned inside it
//! (W289).

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::scanner::{self, HarnessRegistry};
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rusqlite::Connection;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[path = "../src/test_support.rs"]
mod test_support;

const MACHINE: &str = "w315-synthetic-machine";

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn fixture_registry() -> HarnessRegistry {
    let mut value: Value = serde_json::from_str(include_str!("../data/harness-registry-v1.json"))
        .expect("shipped registry is valid JSON");
    let entries = value["harnesses"]
        .as_array_mut()
        .expect("registry has a harness list");
    entries.retain(|entry| {
        matches!(
            entry["id"].as_str(),
            Some("claude-code" | "codex" | "opencode")
        )
    });
    assert_eq!(
        entries.len(),
        3,
        "all three harnesses are in the shipped registry"
    );
    serde_json::from_value(value).expect("filtered registry retains the shipped schema")
}

fn write_jsonl(path: &Path, records: &[Value]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, record).expect("serialize synthetic record");
        bytes.push(b'\n');
    }
    fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
    fs::write(path, &bytes).unwrap();
    bytes
}

fn create_opencode_db(path: &Path, message_data: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (
             id TEXT PRIMARY KEY, time_created INTEGER NOT NULL,
             time_updated INTEGER NOT NULL, title TEXT
         );
         CREATE TABLE message (
             id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
             time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
             data TEXT NOT NULL
         );
         CREATE TABLE part (
             id TEXT PRIMARY KEY, session_id TEXT NOT NULL, message_id TEXT NOT NULL,
             time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL
         );",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session (id, time_created, time_updated, title) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            "ses_w315_synthetic",
            1_800_000_000_i64,
            1_800_000_300_i64,
            "synthetic"
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO message (id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            "msg_w315_synthetic",
            "ses_w315_synthetic",
            1_800_000_100_i64,
            1_800_000_200_i64,
            serde_json::to_string(message_data).unwrap(),
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO part (id, session_id, message_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            "prt_w315_synthetic",
            "ses_w315_synthetic",
            "msg_w315_synthetic",
            1_800_000_150_i64,
            1_800_000_150_i64,
            r#"{"type":"text","text":"synthetic part"}"#,
        ],
    )
    .unwrap();
}

fn store_config(root: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: root.join("repo").to_string_lossy().into_owned(),
        key_file: root.join("masterkey.json"),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(test_support::rustic_cache_root(root)),
        no_cache: false,
    }
}

fn restored_value(store: &BackupStore, key: &rustic_core::repofile::MasterKey, id: &str) -> Value {
    let (bytes, _) = store.read_session_concat(MACHINE, id, key).unwrap();
    let text = std::str::from_utf8(&bytes).expect("restored JSON body is UTF-8");
    let first = text.lines().next().expect("restored body has a record");
    if first.contains("chat-stasher.opencode.session.v1") {
        serde_json::from_str(first).expect("OpenCode archive envelope is JSON")
    } else {
        json!({
            "records": text.lines().map(|line| serde_json::from_str::<Value>(line)
                .expect("restored JSONL record is JSON")).collect::<Vec<_>>()
        })
    }
}

#[test]
fn audit_fields_survive_collect_seal_push_and_readback() {
    let sandbox = test_support::Sandbox::new();
    sandbox.ensure_dirs();
    let root = sandbox.root();

    let claude_parent = json!({
        "type": "assistant", "userType": "external", "isSidechain": false,
        "entrypoint": "cli", "timestamp": "2026-10-02T12:00:00.000Z",
        "cwd": "/synthetic/project", "sessionId": "claude-parent-w315",
        "message": {
            "id": "msg_claude_parent_w315", "model": "claude-synthetic-v1",
            "usage": {"input_tokens": 11, "output_tokens": 7, "cache_read_input_tokens": 3,
                      "cache_creation_input_tokens": 2, "future_usage_counter": 19},
            "content": [{"type": "text", "text": "synthetic parent"}]
        }
    });
    let claude_api_error = json!({
        "type": "assistant", "userType": "external", "isSidechain": false,
        "entrypoint": "cli", "timestamp": "2026-10-02T12:01:00.000Z",
        "cwd": "/synthetic/project", "sessionId": "claude-parent-w315",
        "isApiErrorMessage": true,
        "error": {"status": 429, "class": "synthetic_rate_limit", "message": "synthetic API error"},
        "message": {"id": "msg_claude_error_w315", "model": "claude-synthetic-v1",
                    "usage": {"input_tokens": 0, "output_tokens": 0, "future_error_usage": 23}}
    });
    let claude_worker = json!({
        "type": "assistant", "userType": "external", "isSidechain": true,
        "entrypoint": "agent", "timestamp": "2026-10-02T12:02:00.000Z",
        "cwd": "/synthetic/project", "sessionId": "agent-worker-w315",
        "parentSessionId": "claude-parent-w315",
        "message": {"id": "msg_claude_worker_w315", "model": "claude-synthetic-v1",
                    "usage": {"input_tokens": 5, "output_tokens": 4, "future_worker_counter": 29},
                    "content": [{"type": "text", "text": "synthetic worker"}]}
    });
    let claude_tool_result = json!({
        "type": "user", "timestamp": "2026-10-02T12:03:00.000Z",
        "cwd": "/synthetic/project", "sessionId": "claude-parent-w315",
        "message": {"id": "msg_claude_tool_w315", "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "tool_w315",
                                 "content": "synthetic probe result"}]}
    });

    let claude_root = root.join("sources/claude");
    let claude_parent_path = claude_root.join("project/claude-parent-w315.jsonl");
    let claude_worker_path = claude_root.join("project/subagents/agent-worker-w315.jsonl");
    let claude_parent_bytes = write_jsonl(
        &claude_parent_path,
        &[
            claude_parent.clone(),
            claude_api_error.clone(),
            claude_tool_result.clone(),
        ],
    );
    let claude_worker_bytes =
        write_jsonl(&claude_worker_path, std::slice::from_ref(&claude_worker));

    let codex_event = json!({
        "type": "event_msg", "timestamp": "2026-10-02T12:04:00.000Z",
        "event_msg": {"type": "token_count", "turn_context": {"model": "codex-synthetic-v2"},
            "info": {"last_token_usage": {"input_tokens": 31, "cached_input_tokens": 8,
                "output_tokens": 13, "reasoning_output_tokens": 5, "future_usage_counter": 37}},
            "rate_limits": {
                "limit_id": "synthetic_plan_w315",
                "primary": {"window_minutes": 300, "used_percent": 41.5,
                            "resets_at": "2026-10-02T17:00:00Z"},
                "secondary": {"window_minutes": 10080, "used_percent": 12.25,
                              "resets_at": "2026-10-09T12:00:00Z"}
            }
        }
    });
    let codex_path = root.join("sources/codex/2026/10/02/rollout-w315.jsonl");
    let codex_bytes = write_jsonl(&codex_path, std::slice::from_ref(&codex_event));

    let opencode_data = json!({
        "role": "assistant", "providerID": "provider-synthetic", "modelID": "opencode-synthetic-v3",
        "time": {"created": 1_800_000_100_i64, "completed": 1_800_000_200_i64},
        "tokens": {"input": 17, "output": 9, "reasoning": 4,
                   "cache": {"read": 6, "write": 2}, "future_tokens": 43},
        "error": {"name": "SyntheticProviderError", "statusCode": 503,
                  "message": "synthetic provider error"},
        "path": {"cwd": "/synthetic/opencode-project", "root": "/synthetic/opencode-project"}
    });
    let opencode_db = root.join("sources/opencode/opencode.db");
    create_opencode_db(&opencode_db, &opencode_data);

    let registry = fixture_registry();
    let config = Config {
        harness_roots: [
            (
                "claude-code".to_string(),
                claude_root.to_string_lossy().into_owned(),
            ),
            (
                "codex".to_string(),
                root.join("sources/codex").to_string_lossy().into_owned(),
            ),
            (
                "opencode".to_string(),
                opencode_db.to_string_lossy().into_owned(),
            ),
        ]
        .into_iter()
        .collect(),
        ..Config::default()
    };
    let scan = scanner::scan_with_registry(&config, &registry).expect("scan synthetic sources");
    assert_eq!(
        scan.records.len(),
        4,
        "parent, worker, Codex, and OpenCode sessions found"
    );

    let stage = root.join("stage");
    let state = root.join("collector-state");
    let destination = DestinationView::unreachable("w315-local-fixture");
    let collected = collect::collect_scan_report(&scan, &stage, MACHINE, &state, 20, &destination)
        .expect("collect and seal all synthetic sessions");
    assert_eq!(collected.changed_records, 4);

    let cfg = store_config(root);
    let key = rustic_core::repofile::MasterKey::new();
    store::persist_key_file(&cfg, &key).unwrap();
    let archive = BackupStore::new(cfg, MACHINE.to_string());
    archive
        .push(&stage, &key)
        .expect("push to temporary local repository");

    let id_for_path = |path: &Path| {
        scan.records
            .iter()
            .find(|record| record.absolute_path == path)
            .expect("scanned source path")
            .id
            .clone()
    };
    for (source_path, source_bytes) in [
        (claude_parent_path.as_path(), claude_parent_bytes.as_slice()),
        (claude_worker_path.as_path(), claude_worker_bytes.as_slice()),
        (codex_path.as_path(), codex_bytes.as_slice()),
    ] {
        let id = id_for_path(source_path);
        let (restored, _) = archive.read_session_concat(MACHINE, &id, &key).unwrap();
        assert_eq!(restored, source_bytes, "raw JSONL bytes preserved for {id}");
        assert_eq!(
            digest(&restored),
            digest(source_bytes),
            "SHA-256 preserved for {id}"
        );
    }

    let parent = restored_value(&archive, &key, &id_for_path(&claude_parent_path));
    let records: Vec<Value> = std::str::from_utf8(&claude_parent_bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(parent, json!({"records": records}));
    let worker = restored_value(&archive, &key, &id_for_path(&claude_worker_path));
    assert_eq!(worker, json!({"records": [claude_worker]}));
    assert_eq!(
        parent["records"][0]["message"]["model"],
        "claude-synthetic-v1"
    );
    assert_eq!(
        parent["records"][0]["message"]["id"],
        "msg_claude_parent_w315"
    );
    assert_eq!(
        parent["records"][0]["message"]["usage"]["future_usage_counter"],
        19
    );
    assert_eq!(parent["records"][1]["isApiErrorMessage"], true);
    assert_eq!(parent["records"][1]["error"]["status"], 429);
    assert_eq!(parent["records"][0]["type"], "assistant");
    assert_eq!(parent["records"][0]["userType"], "external");
    assert_eq!(parent["records"][0]["isSidechain"], false);
    assert_eq!(parent["records"][0]["entrypoint"], "cli");
    assert_eq!(
        parent["records"][0]["timestamp"],
        "2026-10-02T12:00:00.000Z"
    );
    assert_eq!(parent["records"][0]["cwd"], "/synthetic/project");
    assert_eq!(parent["records"][0]["sessionId"], "claude-parent-w315");
    assert_eq!(worker["records"][0]["isSidechain"], true);
    assert_eq!(
        worker["records"][0]["parentSessionId"],
        "claude-parent-w315"
    );
    assert_eq!(
        parent["records"][2]["message"]["content"][0]["type"],
        "tool_result"
    );

    let codex = restored_value(&archive, &key, &id_for_path(&codex_path));
    assert_eq!(codex, json!({"records": [codex_event]}));
    let rate_limits = &codex["records"][0]["event_msg"]["rate_limits"];
    assert_eq!(
        codex["records"][0]["event_msg"]["turn_context"]["model"],
        "codex-synthetic-v2"
    );
    assert_eq!(
        codex["records"][0]["event_msg"]["info"]["last_token_usage"]["future_usage_counter"],
        37
    );
    assert_eq!(rate_limits["limit_id"], "synthetic_plan_w315");
    assert_eq!(rate_limits["primary"]["window_minutes"], 300);
    assert_eq!(rate_limits["primary"]["used_percent"], 41.5);
    assert_eq!(rate_limits["primary"]["resets_at"], "2026-10-02T17:00:00Z");
    assert_eq!(rate_limits["secondary"]["window_minutes"], 10080);
    assert_eq!(rate_limits["secondary"]["used_percent"], 12.25);
    assert_eq!(
        rate_limits["secondary"]["resets_at"],
        "2026-10-09T12:00:00Z"
    );

    let opencode_id = id_for_path(&opencode_db);
    let opencode = restored_value(&archive, &key, &opencode_id);
    let message_row = &opencode["messages"][0];
    let restored_data = &message_row["data"];
    assert_eq!(message_row["id"], "msg_w315_synthetic");
    assert_eq!(message_row["time_created"], 1_800_000_100_i64);
    assert_eq!(restored_data, &opencode_data);
    assert_eq!(
        digest(&serde_json::to_vec(restored_data).unwrap()),
        digest(&serde_json::to_vec(&opencode_data).unwrap()),
        "OpenCode row JSON has the same canonical SHA-256 after restore"
    );
    assert_eq!(restored_data["providerID"], "provider-synthetic");
    assert_eq!(restored_data["modelID"], "opencode-synthetic-v3");
    assert_eq!(restored_data["tokens"]["future_tokens"], 43);
    assert_eq!(restored_data["tokens"]["cache"]["read"], 6);
    assert_eq!(restored_data["tokens"]["cache"]["write"], 2);
    assert_eq!(restored_data["error"]["statusCode"], 503);
    assert_eq!(restored_data["path"]["cwd"], "/synthetic/opencode-project");
}

/// Step 2 adds archive source-path provenance. Keep the expected assertion in
/// the suite as an ignored TODO so the missing `subagents/` attribution is
/// visible without claiming it is retained in Step 1.
#[test]
#[ignore = "expected failure until Step 2 archives source-path provenance"]
fn subagents_source_path_provenance_is_retained() {
    let archived_source_path_class: Option<&str> = None;
    assert_eq!(archived_source_path_class, Some("subagents/"));
}
