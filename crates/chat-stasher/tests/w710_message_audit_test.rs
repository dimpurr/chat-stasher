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
#[test]
fn deterministic_legacy_and_captured_metadata() {
    let body = r#"{"type":"assistant","timestamp":"2026-10-02T12:00:00Z","message":{"id":"secret-event","model":"synthetic","usage":{"input_tokens":0,"output_tokens":null,"future":17}}}"#;
    for schema in [1, 2, 3] {
        let input = bundle(schema, "claude-code", body);
        let p = project_bundle(&input, "claude-code.synthetic-session", &policy()).unwrap();
        assert_eq!(
            p,
            project_bundle(&input, "claude-code.synthetic-session", &policy()).unwrap()
        );
        assert_eq!(p.rows.len(), 1);
        let r = &records(&p)[0];
        assert_eq!(
            r["fidelity"]["source"],
            if schema == 3 { "captured" } else { "declared" }
        );
        assert_eq!(
            r["fidelity"]["value"],
            if schema == 3 { "full" } else { "unknown" }
        );
        assert_eq!(r["usage"]["usage"]["input_tokens"], 0);
        assert!(r["usage"]["usage"]["output_tokens"].is_null());
        assert!(r["usage"]["usage"].get("missing").is_none());
        if schema < 3 {
            assert!(r.get("producer").is_none());
        } else {
            assert_eq!(r["producer"]["source"], "captured");
        }
        assert_eq!(
            r["body_sha256"],
            Sha256::digest(body.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
    }
}
#[test]
fn sidecar_roundtrip_append_and_conflicts() {
    let p = project_bundle(
        &bundle(
            3,
            "claude-code",
            r#"{"type":"assistant","message":{"id":"synthetic-id"}}"#,
        ),
        "synthetic-session",
        &policy(),
    )
    .unwrap();
    let bytes = append_jsonl(&[], &p).unwrap();
    assert_eq!(decode_jsonl(&bytes).unwrap(), p);
    assert_eq!(append_jsonl(&bytes, &p).unwrap(), bytes);
    let mut changed = p.clone();
    changed.rows[0].model = Some("changed".into());
    assert!(append_jsonl(&bytes, &changed).is_err());
    let wrong_key = project_bundle(
        &bundle(
            3,
            "claude-code",
            r#"{"type":"assistant","message":{"id":"synthetic-id"}}"#,
        ),
        "synthetic-session",
        &JoinPolicy::new([8; 32]),
    )
    .unwrap();
    assert!(append_jsonl(&bytes, &wrong_key).is_err());
    let mut v: Value =
        serde_json::from_slice(bytes.split(|b| *b == b'\n').next().unwrap()).unwrap();
    v["schema"] = json!("chat-stasher/message-audit@99");
    let mut unsupported_version = serde_json::to_vec(&v).unwrap();
    unsupported_version.push(b'\n');
    assert!(decode_jsonl(&unsupported_version).is_err());
    let mut row = records(&p)[0].clone();
    row["text"] = json!("forbidden");
    assert!(serde_json::from_value::<chat_stasher::message_audit::AuditRow>(row).is_err());
}
#[test]
fn partial_requires_note_and_unsupported_is_visible() {
    let mut v: Value = serde_json::from_slice(&bundle(3, "claude-code", "{}")).unwrap();
    v["fidelity"] = json!({"value":"partial"});
    assert!(project_bundle(
        &serde_json::to_vec(&v).unwrap(),
        "synthetic-session",
        &policy()
    )
    .is_err());
    v["fidelity"]["note"] = json!("known missing events");
    assert!(project_bundle(
        &serde_json::to_vec(&v).unwrap(),
        "synthetic-session",
        &policy()
    )
    .is_ok());
    for (harness, body, status) in [
        ("unknown", "{}", "unsupported"),
        ("claude-code", "broken", "failed"),
        ("claude-code", "", "complete"),
        ("claude-code", "{}", "unsupported"),
    ] {
        let p = project_bundle(&bundle(2, harness, body), "synthetic-session", &policy()).unwrap();
        assert!(p.rows.is_empty());
        assert_eq!(
            serde_json::to_value(&p.outcomes[0]).unwrap()["status"],
            status
        );
    }
}
#[test]
fn field_allowlist_and_harness_variants() {
    let fixtures = [
        (
            "claude-code",
            json!({"type":"assistant","uuid":"private-uuid","parentUuid":"private-parent","cwd":"/private/cwd","isSidechain":true,"isApiErrorMessage":true,"error":{"class":"rate_limit","message":"private-error"},"message":{"id":"private-event","model":"synthetic","usage":{"future":9,"text":"private-usage"},"content":"private-text"},"title":"private-title"}),
        ),
        (
            "codex",
            json!({"type":"event_msg","id":"private-event","timestamp":"bad-private-timestamp","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":0,"cached_input_tokens":2}},"rate_limits":{"primary":{"used_percent":41.5}}}}),
        ),
        (
            "opencode",
            json!({"id":"private-event","role":"assistant","modelID":"synthetic","providerID":"synthetic-provider","tokens":{"input":0,"cache":{"read":2,"write":null}},"path":{"cwd":"/private/cwd"},"error":{"name":"SyntheticError","message":"private-error"},"time":{"created":1800000100}}),
        ),
    ];
    for (harness, event) in fixtures {
        let p = project_bundle(
            &bundle(2, harness, &event.to_string()),
            "synthetic-session",
            &policy(),
        )
        .unwrap();
        assert_eq!(p.rows.len(), 1);
        let bytes = String::from_utf8(append_jsonl(&[], &p).unwrap()).unwrap();
        for forbidden in [
            "private-event",
            "private-uuid",
            "private-parent",
            "/private/cwd",
            "private-error",
            "private-text",
            "private-title",
            "private-usage",
            "bad-private-timestamp",
        ] {
            assert!(!bytes.contains(forbidden), "{harness}: {forbidden}");
        }
        let r = &records(&p)[0];
        assert!(r["event_key"].as_str().unwrap().len() == 64);
        if harness == "opencode" {
            assert_eq!(r["usage"]["tokens"]["cache"]["read"], 2);
        }
        if harness == "codex" {
            assert_eq!(r["usage"]["rate_limits"]["primary"]["used_percent"], 41.5);
        }
    }
}

#[test]
fn raw_harness_slice_positions_parent_joins_and_partial_outcomes() {
    let parent = json!({"type":"assistant","uuid":"synthetic-parent","message":{"id":"synthetic-provider-id"}});
    let worker = json!({"type":"assistant","uuid":"synthetic-worker","parentUuid":"synthetic-parent","isSidechain":true,"message":{"usage":{"input_tokens":0}}});
    let body = format!("{}\n{}\nbroken\n", parent, worker);
    let sha = Sha256::digest(body.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let v = json!({"schema":"chat-stasher/inbox@3","kind":"harness-file","harness":"claude-code","nativeSessionId":"synthetic-native",
        "file":{"role":"subagent","relPath":"subagents/synthetic.jsonl","byteStart":0,"byteEnd":body.len(),"sha256":sha,"parentNativeSessionId":"synthetic-parent-session"},
        "raw":{"encoding":"utf-8","data":body},"fidelity":{"value":"raw"},"producer":{"kind":"send","version":"synthetic-v1","sendKeyId":"synthetic-public-key"}});
    let p = project_bundle(
        &serde_json::to_vec(&v).unwrap(),
        "claude-code.synthetic-native",
        &policy(),
    )
    .unwrap();
    assert_eq!(p.rows.len(), 2);
    assert_eq!(p.rows[0].record_position, "byte:0");
    assert_eq!(
        p.rows[1].record_position,
        format!("byte:{}", parent.to_string().len() + 1)
    );
    assert_eq!(
        p.rows[1].parent_key.as_ref(),
        p.rows[0].join_keys.get("uuid")
    );
    assert_eq!(p.rows[1].sidechain, Some(true));
    assert_eq!(p.rows[0].subagent, Some(true));
    let o = serde_json::to_value(&p.outcomes[0]).unwrap();
    assert_eq!(o["status"], "partial");
    assert_eq!(o["malformed"], 1);
    assert_eq!(decode_jsonl(&append_jsonl(&[], &p).unwrap()).unwrap(), p);
    let mut parent_bundle = v.clone();
    parent_bundle["nativeSessionId"] = json!("synthetic-parent-session");
    parent_bundle["file"]["role"] = json!("transcript");
    parent_bundle["file"]
        .as_object_mut()
        .unwrap()
        .remove("parentNativeSessionId");
    let parent_projection = project_bundle(
        &serde_json::to_vec(&parent_bundle).unwrap(),
        "claude-code.synthetic-parent-session",
        &policy(),
    )
    .unwrap();
    assert_eq!(
        p.rows[0].parent_key.as_ref(),
        parent_projection.rows[0].join_keys.get("native-session")
    );
    let mut binary = v.clone();
    binary["raw"] = json!({"encoding":"base64","data":"/w=="});
    binary["file"]["byteEnd"] = json!(1);
    binary["file"]["sha256"] = json!(Sha256::digest([255])
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>());
    let p = project_bundle(
        &serde_json::to_vec(&binary).unwrap(),
        "synthetic-session",
        &policy(),
    )
    .unwrap();
    assert!(p.rows.is_empty());
    assert_eq!(
        serde_json::to_value(&p.outcomes[0]).unwrap()["status"],
        "failed"
    );
}

#[test]
fn aggregate_opencode_and_key_domain_separation() {
    let event = json!({"id":"synthetic-id","data":{"role":"assistant","providerID":"synthetic","modelID":"synthetic","tokens":{"input":0,"cache":{"write":null}},"time":{"created":1800000100000_i64}}});
    let body=json!({"schema":"chat-stasher.opencode.session.v1","messages":[event,event],"session":{"title":"private-title"}}).to_string();
    let p = project_bundle(
        &bundle(2, "opencode", &body),
        "synthetic-session",
        &policy(),
    )
    .unwrap();
    assert_eq!(p.rows.len(), 2);
    assert_eq!(p.rows[0].event_key, p.rows[1].event_key);
    assert_ne!(p.rows[0].record_position, p.rows[1].record_position);
    let bytes = append_jsonl(&[], &p).unwrap();
    assert_eq!(decode_jsonl(&bytes).unwrap(), p);
    assert!(!String::from_utf8(bytes.clone())
        .unwrap()
        .contains("private-title"));
    assert!(decode_jsonl(&bytes[..bytes.len() - 1]).is_err());
    let mut injected: Value =
        serde_json::from_slice(bytes.split(|b| *b == b'\n').next().unwrap()).unwrap();
    injected["row"]["timestamp"]["source"] = json!("private-text");
    let mut bad = serde_json::to_vec(&injected).unwrap();
    bad.push(b'\n');
    assert!(decode_jsonl(&bad).is_err());
    let mut v: Value = serde_json::from_slice(&bundle(2, "opencode", &body)).unwrap();
    v["fidelity"] = json!({"value":"raw"});
    v["producer"] = json!({"kind":"collect","version":"invented"});
    let old = project_bundle(
        &serde_json::to_vec(&v).unwrap(),
        "synthetic-session",
        &policy(),
    )
    .unwrap();
    assert_eq!(old, p, "legacy envelopes cannot invent capture metadata");
    let other = project_bundle(
        &bundle(2, "opencode", &body),
        "synthetic-session",
        &JoinPolicy::new([8; 32]),
    )
    .unwrap();
    assert_ne!(p.rows[0].event_key, other.rows[0].event_key);
}

#[test]
fn null_missing_and_measured_empty_stay_distinct() {
    let body=json!([{"type":"assistant","isSidechain":null,"parentUuid":null,"cwd":null,"error":null,"message":{"id":null,"provider":null,"usage":null}},
        {"type":"assistant","message":{"usage":{"input_tokens":0}}}]).to_string();
    let p = project_bundle(
        &bundle(2, "claude-code", &body),
        "synthetic-session",
        &policy(),
    )
    .unwrap();
    let r = records(&p);
    assert_eq!(r[0]["field_states"]["sidechain"], "null");
    assert_eq!(r[1]["field_states"]["sidechain"], "missing");
    for field in ["message_id", "provider", "error"] {
        assert_eq!(r[0]["field_states"][field], "null");
        assert_eq!(r[1]["field_states"][field], "missing");
    }
    assert!(r[0]["usage"].get("usage").unwrap().is_null());
    assert_eq!(decode_jsonl(&append_jsonl(&[], &p).unwrap()).unwrap(), p);
    let empty = project_bundle(
        &bundle(2, "claude-code", "[]"),
        "synthetic-session",
        &policy(),
    )
    .unwrap();
    assert!(empty.rows.is_empty());
    assert_eq!(
        serde_json::to_value(&empty.outcomes[0]).unwrap()["status"],
        "complete"
    );
    let p = project_bundle(
        &bundle(2, "claude-code", "{\"type\":\"assistant\"}"),
        "synthetic-session",
        &policy(),
    )
    .unwrap();
    let mut row = records(&p)[0].clone();
    row["fidelity"]["title"] = json!("private-title");
    assert!(serde_json::from_value::<chat_stasher::message_audit::AuditRow>(row).is_err());
}
