use chat_stasher::message_audit::{append_jsonl, decode_jsonl, project_bundle, JoinPolicy};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn policy() -> JoinPolicy {
    JoinPolicy::new([7; 32])
}
fn bundle(schema: u8, harness: &str, body: &str) -> Vec<u8> {
    let mut v = json!({"schema":format!("chat-stasher/inbox@{schema}"),"platform":harness,
        "sessionId":"synthetic-session","raw":{"text":body,"bytes":body.len()}});
    if schema == 3 {
        v["kind"] = json!("web-capture");
        v["fidelity"] = json!({"value":"full","representation":"api"});
        v["producer"] = json!({"kind":"extension","version":"synthetic-v1"});
    }
    serde_json::to_vec(&v).unwrap()
}
fn records(p: &chat_stasher::message_audit::Projection) -> Vec<Value> {
    p.rows
        .iter()
        .map(|r| serde_json::to_value(r).unwrap())
        .collect()
}
fn letta_line(kind: &str, text: &str, msg_id: &str, line_id: &str) -> String {
    json!({"kind":kind,"text":text,"captured_at":"2026-10-06T01:29:49.153Z",
        "source_line_id":line_id,"source_message_id":msg_id})
    .to_string()
}

#[test]
fn letta_transcript_projects_every_record_kind() {
    let body = format!(
        "{}\n{}\n{}\n{}\n",
        letta_line("user", "private-user-text", "message-11111111-1111-1111-1111-111111111111", "user-11111111-1111-1111-1111-111111111111"),
        letta_line("assistant", "private-assistant-text", "message-22222222-2222-2222-2222-222222222222", "assistant:message-22222222-2222-2222-2222-222222222222"),
        letta_line("reasoning", "private-reasoning-text", "message-33333333-3333-3333-3333-333333333333", "message-33333333-3333-3333-3333-333333333333"),
        json!({"kind":"tool_call","name":"web_search","argsText":"private-args","resultOk":true,
            "resultText":"private-result","captured_at":"2026-10-06T01:29:49.153Z",
            "source_line_id":"chatcmpl-tool-private","source_message_id":"message-44444444-4444-4444-4444-444444444444"}).to_string(),
    );
    let p = project_bundle(
        &bundle(2, "letta", &body),
        "letta.synthetic-session",
        &policy(),
    )
    .unwrap();
    assert_eq!(p.rows.len(), 4);
    let r = records(&p);
    for row in &r {
        assert_eq!(row["harness"], "letta");
        assert_eq!(row["event_id_class"], "message-id");
        assert_eq!(row["timestamp"]["source"], "captured_at");
        assert_eq!(
            row["timestamp"]["unix_millis"],
            chrono::DateTime::parse_from_rfc3339("2026-10-06T01:29:49.153Z")
                .unwrap()
                .timestamp_millis()
        );
        assert!(row["model"].is_null());
        assert!(row["provider"].is_null());
        assert!(row["usage"].as_object().unwrap().is_empty());
        assert!(row.get("error_class").is_none());
        assert!(row.get("cwd_hash").is_none());
        assert!(row.get("parent_key").is_none());
    }
    assert_eq!(r[0]["field_states"]["kind"], "present");
    assert_eq!(r[0]["field_states"]["text"], "present");
    assert_eq!(r[0]["field_states"]["captured_at"], "present");
    assert_eq!(r[0]["field_states"]["source_line_id"], "present");
    assert_eq!(r[0]["field_states"]["source_message_id"], "present");
    assert_eq!(r[0]["field_states"]["tool_name"], "missing");
    assert_eq!(r[0]["field_states"]["tool_result_ok"], "missing");
    assert_eq!(r[3]["field_states"]["tool_name"], "present");
    assert_eq!(r[3]["field_states"]["tool_result_ok"], "present");
    let bytes = append_jsonl(&[], &p).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    for forbidden in [
        "private-user-text",
        "private-assistant-text",
        "private-reasoning-text",
        "private-args",
        "private-result",
        "11111111",
        "22222222",
        "33333333",
        "44444444",
        "chatcmpl-tool-private",
        "web_search",
    ] {
        assert!(!text.contains(forbidden), "leaked: {forbidden}");
    }
    assert_eq!(decode_jsonl(&bytes).unwrap(), p);
}

#[test]
fn letta_event_key_and_uuid_join() {
    let body = letta_line(
        "assistant",
        "private-text",
        "message-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        "assistant:message-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
    ) + "\n";
    let p = project_bundle(&bundle(2, "letta", &body), "synthetic-session", &policy()).unwrap();
    assert_eq!(p.rows.len(), 1);
    let r = &records(&p)[0];
    assert_eq!(r["event_id_class"], "message-id");
    assert_eq!(r["event_key"].as_str().unwrap().len(), 64);
    let join = r["join_keys"].as_object().unwrap();
    assert_eq!(join.len(), 2);
    assert_eq!(
        join["message-id"].as_str().unwrap(),
        r["event_key"].as_str().unwrap()
    );
    assert_eq!(
        join["uuid"].as_str().unwrap().len(),
        64,
        "uuid join key is derived from the message id suffix"
    );
    assert_ne!(
        join["uuid"].as_str().unwrap(),
        join["message-id"].as_str().unwrap()
    );
    let other = project_bundle(
        &bundle(2, "letta", &body),
        "synthetic-session",
        &JoinPolicy::new([8; 32]),
    )
    .unwrap();
    assert_ne!(r["event_key"], records(&other)[0]["event_key"]);
}

#[test]
fn letta_missing_message_id_falls_back_to_position() {
    let body = json!({"kind":"user","text":"private-text","captured_at":"2026-10-06T01:29:49.153Z",
        "source_line_id":"user-99999999-9999-9999-9999-999999999999"})
    .to_string()
        + "\n";
    let p = project_bundle(&bundle(2, "letta", &body), "synthetic-session", &policy()).unwrap();
    assert_eq!(p.rows.len(), 1);
    let r = &records(&p)[0];
    assert_eq!(r["event_id_class"], "position");
    assert!(r["event_key"].as_str().unwrap().len() == 64);
    assert_eq!(r["field_states"]["source_message_id"], "missing");
    assert!(r["join_keys"].as_object().unwrap().is_empty());
    let bytes = append_jsonl(&[], &p).unwrap();
    assert_eq!(decode_jsonl(&bytes).unwrap(), p);
}

#[test]
fn letta_null_missing_and_invalid_field_states_stay_distinct() {
    let body = format!(
        "{}\n{}\n{}\n",
        json!({"kind":"user","text":null,"captured_at":null,"source_line_id":"user-11111111-1111-1111-1111-111111111111","source_message_id":"message-11111111-1111-1111-1111-111111111111"}).to_string(),
        json!({"kind":"assistant","captured_at":"2026-10-06T01:29:49.153Z","source_line_id":"assistant:message-22222222-2222-2222-2222-222222222222","source_message_id":"message-22222222-2222-2222-2222-222222222222"}).to_string(),
        json!({"kind":"tool_call","name":42,"resultOk":"yes","captured_at":"2026-10-06T01:29:49.153Z","source_line_id":"chatcmpl-tool-3","source_message_id":"message-33333333-3333-3333-3333-333333333333"}).to_string(),
    );
    let p = project_bundle(&bundle(2, "letta", &body), "synthetic-session", &policy()).unwrap();
    assert_eq!(p.rows.len(), 3);
    let r = records(&p);
    assert_eq!(r[0]["field_states"]["text"], "null");
    assert_eq!(r[0]["field_states"]["captured_at"], "null");
    assert_eq!(r[0]["timestamp"]["classification"], "null");
    assert_eq!(r[1]["field_states"]["text"], "missing");
    assert_eq!(r[1]["field_states"]["captured_at"], "present");
    assert_eq!(r[1]["timestamp"]["classification"], "parsed");
    assert_eq!(r[2]["field_states"]["tool_name"], "invalid");
    assert_eq!(r[2]["field_states"]["tool_result_ok"], "invalid");
    assert_eq!(r[2]["field_states"]["text"], "missing");
    let bytes = append_jsonl(&[], &p).unwrap();
    assert_eq!(decode_jsonl(&bytes).unwrap(), p);
}

#[test]
fn letta_unsupported_kind_and_malformed_lines_are_counted() {
    let body = format!(
        "{}\n{}\nbroken\n",
        letta_line("user", "private-text", "message-11111111-1111-1111-1111-111111111111", "user-11111111-1111-1111-1111-111111111111"),
        json!({"kind":"system","text":"private-system-text","captured_at":"2026-10-06T01:29:49.153Z","source_line_id":"system-1","source_message_id":"message-22222222-2222-2222-2222-222222222222"}).to_string(),
    );
    let p = project_bundle(&bundle(2, "letta", &body), "synthetic-session", &policy()).unwrap();
    assert_eq!(p.rows.len(), 1);
    let o = serde_json::to_value(&p.outcomes[0]).unwrap();
    assert_eq!(o["status"], "partial");
    assert_eq!(o["recognized"], 1);
    assert_eq!(o["unsupported"], 1);
    assert_eq!(o["malformed"], 1);
    let bytes = append_jsonl(&[], &p).unwrap();
    assert!(!String::from_utf8(bytes)
        .unwrap()
        .contains("private-system-text"));
    assert_eq!(decode_jsonl(&append_jsonl(&[], &p).unwrap()).unwrap(), p);
}

#[test]
fn letta_empty_and_whitespace_bodies_are_complete() {
    for body in ["", "   \n\n  "] {
        let p = project_bundle(&bundle(2, "letta", body), "synthetic-session", &policy()).unwrap();
        assert!(p.rows.is_empty());
        assert_eq!(
            serde_json::to_value(&p.outcomes[0]).unwrap()["status"],
            "complete"
        );
    }
}

#[test]
fn letta_legacy_envelope_cannot_invent_capture_metadata() {
    let body = letta_line(
        "assistant",
        "private-text",
        "message-11111111-1111-1111-1111-111111111111",
        "assistant:message-11111111-1111-1111-1111-111111111111",
    ) + "\n";
    let mut v: Value = serde_json::from_slice(&bundle(3, "letta", &body)).unwrap();
    v["fidelity"] = json!({"value":"full","representation":"api"});
    v["producer"] = json!({"kind":"collect","version":"invented"});
    let bytes = serde_json::to_vec(&v).unwrap();
    let p = project_bundle(&bytes, "synthetic-session", &policy()).unwrap();
    assert_eq!(p.rows.len(), 1);
    assert_eq!(records(&p)[0]["fidelity"]["source"], "captured");
    assert_eq!(records(&p)[0]["fidelity"]["value"], "full");
    assert_eq!(records(&p)[0]["producer"]["source"], "captured");
    let mut v: Value = serde_json::from_slice(&bundle(2, "letta", &body)).unwrap();
    v["fidelity"] = json!({"value":"raw"});
    v["producer"] = json!({"kind":"collect","version":"invented"});
    let old = project_bundle(
        &serde_json::to_vec(&v).unwrap(),
        "synthetic-session",
        &policy(),
    )
    .unwrap();
    assert_eq!(
        records(&old)[0]["fidelity"]["source"],
        "declared",
        "legacy envelopes cannot invent capture metadata"
    );
    assert!(records(&old)[0].get("producer").is_none());
}

#[test]
fn letta_body_digest_covers_exact_bytes() {
    let body = letta_line(
        "user",
        "private-text",
        "message-11111111-1111-1111-1111-111111111111",
        "user-11111111-1111-1111-1111-111111111111",
    ) + "\n";
    let p = project_bundle(&bundle(2, "letta", &body), "synthetic-session", &policy()).unwrap();
    let sha = Sha256::digest(body.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_eq!(records(&p)[0]["body_sha256"], sha);
}
