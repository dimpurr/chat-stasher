//! RI-3e: the shared contract corpus reaches durable archive readback, and
//! migration projects the same audit facts without rewriting authoritative bytes.
use chat_stasher::{audit_store, inbox, message_audit, store};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, path::Path};

fn policy() -> message_audit::JoinPolicy {
    message_audit::JoinPolicy::new([7; 32])
}
fn archive(root: &Path) -> (store::BackupStore, rustic_core::repofile::MasterKey) {
    let cfg = store::StoreConfig {
        repo_root: root.join("repo").to_string_lossy().into_owned(),
        key_file: root.join("masterkey"),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(root.join("rustic-cache")),
        no_cache: false,
    };
    let key = rustic_core::repofile::MasterKey::new();
    let store = store::BackupStore::new(cfg, "synthetic-machine".into());
    (store, key)
}

#[test]
fn every_shared_contract_fixture_seals_or_refuses_before_writing() {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/fixtures/inbox");
    let manifest: Vec<Value> =
        serde_json::from_slice(&fs::read(corpus.join("manifest.json")).unwrap()).unwrap();
    for case in manifest {
        let name = case["file"].as_str().unwrap();
        let input = fs::read(corpus.join(name)).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().join("stage");
        if case["valid"].as_bool().unwrap() {
            fs::create_dir_all(&stage).unwrap();
        }
        let outcome = inbox::seal_payload(
            "synthetic.json",
            &input,
            &stage,
            "synthetic-machine",
            100,
            None,
            None,
        );
        if !case["valid"].as_bool().unwrap() {
            assert!(outcome.is_err(), "invalid fixture accepted: {name}");
            assert!(!stage.exists(), "invalid fixture wrote stage: {name}");
            continue;
        }
        let inbox::SealOutcome::Stored(saved) = outcome.unwrap() else {
            panic!("new fixture was not stored: {name}");
        };
        let sealed = store::concat_shards(&stage, "synthetic-machine", &saved.id).unwrap();
        let record: Value = serde_json::from_slice(&sealed).unwrap();
        let bundle: Value = serde_json::from_slice(&input).unwrap();
        assert_eq!(
            audit_store::record_body(&record).unwrap(),
            audit_store::record_body(&bundle).unwrap()
        );
        let expected = message_audit::project_bundle(&input, &saved.id, &policy()).unwrap();
        audit_store::backfill_stage(&stage, "synthetic-machine", &policy()).unwrap();
        let sidecar = stage.join("meta/synthetic-machine/message-audit-v1.jsonl");
        let audit = fs::read(&sidecar).unwrap();
        assert_eq!(
            message_audit::decode_jsonl(&audit).unwrap(),
            expected,
            "{name}"
        );
        let (archive, key) = archive(tmp.path());
        archive.push(&stage, &key).unwrap();
        fs::remove_dir_all(stage.join("sessions")).unwrap();
        fs::remove_file(&sidecar).unwrap();
        let restored = archive
            .read_session_concat("synthetic-machine", &saved.id, &key)
            .unwrap()
            .0;
        assert_eq!(restored, sealed, "archive changed sealed bytes: {name}");
        audit_store::backfill_archive(&archive, &key, &stage, "synthetic-machine", &policy())
            .unwrap();
        assert_eq!(
            fs::read(&sidecar).unwrap(),
            audit,
            "rebuild changed audit: {name}"
        );
        assert!(!stage.join("sessions").exists());
        assert_eq!(
            archive
                .read_session_concat("synthetic-machine", &saved.id, &key)
                .unwrap()
                .0,
            sealed
        );
    }
}

#[test]
fn retained_usage_fields_survive_harness_archive_and_migration() {
    let fixtures = [
        (
            "claude-code",
            json!({"type":"assistant","timestamp":"2026-10-02T12:00:00Z","cwd":"/synthetic/private","isSidechain":true,"parentUuid":"synthetic-parent","isApiErrorMessage":true,"error":{"name":"SyntheticError","message":"synthetic-secret-error"},"message":{"id":"synthetic-secret-id","model":"synthetic-model","usage":{"input_tokens":0,"output_tokens":null,"cache_creation_input_tokens":3,"cache_read_input_tokens":4,"cache_creation":{"ephemeral_5m_input_tokens":5,"ephemeral_1h_input_tokens":6},"future_counter":7},"content":"synthetic-secret-body"}}),
            "usage",
        ),
        (
            "codex",
            json!({"type":"event_msg","timestamp":"2026-10-02T12:00:00Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":0,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":4,"total_tokens":5},"total_token_usage":{"input_tokens":6,"output_tokens":null}},"rate_limits":{"primary":{"used_percent":41.5,"window_minutes":300,"resets_at":1800000000}}}}),
            "last_token_usage",
        ),
        (
            "opencode",
            json!({"role":"assistant","id":"synthetic-secret-id","providerID":"synthetic-provider","modelID":"synthetic-model","tokens":{"input":0,"output":3,"reasoning":4,"cache":{"read":5,"write":null},"future_counter":7},"error":{"name":"SyntheticError","message":"synthetic-secret-error"},"path":{"cwd":"/synthetic/private"},"time":{"created":1800000000000_i64}}),
            "tokens",
        ),
    ];
    for (harness, event, usage_field) in fixtures {
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().join("stage");
        fs::create_dir_all(&stage).unwrap();
        let body = format!("{event}\n");
        let digest = Sha256::digest(body.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let input = json!({"schema":"chat-stasher/inbox@3","kind":"harness-file","harness":harness,"nativeSessionId":"synthetic-native","file":{"role":"transcript","relPath":"synthetic.jsonl","byteStart":17,"byteEnd":17+body.len(),"sha256":digest},"raw":{"encoding":"utf-8","data":body},"fidelity":{"value":"raw"}});
        let inbox::SealOutcome::Stored(saved) = inbox::seal_payload(
            "synthetic.json",
            &serde_json::to_vec(&input).unwrap(),
            &stage,
            "synthetic-machine",
            100,
            None,
            None,
        )
        .unwrap() else {
            panic!("must store");
        };
        let (archive, key) = archive(tmp.path());
        archive.push(&stage, &key).unwrap();
        let original = archive
            .read_session_concat("synthetic-machine", &saved.id, &key)
            .unwrap()
            .0;
        fs::remove_dir_all(stage.join("sessions")).unwrap();
        audit_store::backfill_archive(&archive, &key, &stage, "synthetic-machine", &policy())
            .unwrap();
        let sidecar = stage.join("meta/synthetic-machine/message-audit-v1.jsonl");
        let bytes = fs::read(&sidecar).unwrap();
        let projection = message_audit::decode_jsonl(&bytes).unwrap();
        assert_eq!(projection.rows.len(), 1);
        let row = serde_json::to_value(&projection.rows[0]).unwrap();
        let expected_usage = if harness == "claude-code" {
            &event["message"][usage_field]
        } else if harness == "codex" {
            &event["payload"]["info"][usage_field]
        } else {
            &event[usage_field]
        };
        assert_eq!(&row["usage"][usage_field], expected_usage, "{harness}");
        if harness == "codex" {
            assert_eq!(row["usage"]["rate_limits"], event["payload"]["rate_limits"]);
            assert_eq!(
                row["usage"]["total_token_usage"],
                event["payload"]["info"]["total_token_usage"]
            );
        }
        assert_eq!(row["body_sha256"], digest);
        // A single JSON record (including its trailing newline) parses as
        // one JSON body; byte offsets are used only for multi-record JSONL.
        assert_eq!(row["record_position"], "json");
        assert_eq!(row["fidelity"]["source"], "captured");
        let text = std::str::from_utf8(&bytes).unwrap();
        for secret in [
            "synthetic-secret-id",
            "synthetic-secret-error",
            "synthetic-secret-body",
            "/synthetic/private",
        ] {
            assert!(!text.contains(secret));
        }
        audit_store::backfill_archive(&archive, &key, &stage, "synthetic-machine", &policy())
            .unwrap();
        assert_eq!(fs::read(sidecar).unwrap(), bytes);
        assert_eq!(
            archive
                .read_session_concat("synthetic-machine", &saved.id, &key)
                .unwrap()
                .0,
            original
        );
        let restored: Value = serde_json::from_slice(&original).unwrap();
        assert_eq!(
            audit_store::record_body(&restored).unwrap(),
            body.as_bytes()
        );
        let mut tampered = restored;
        tampered["raw"]["data"] = json!("synthetic-tamper");
        assert!(audit_store::record_body(&tampered).is_err());
    }
}
