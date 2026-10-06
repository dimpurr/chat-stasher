use chat_stasher::{inbox, store};
use serde_json::Value;
use std::fs;

#[test]
fn harness_bundle_stage_readback_preserves_exact_bytes_and_metadata() {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/fixtures/inbox");
    for name in [
        "harness-raw.json",
        "binary-slice.json",
        "empty-slice.json",
        "unicode-slice.json",
        "cloud-account.json",
        "harness-subagent.json",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let input = fs::read(root.join(name)).unwrap();
        let bundle: Value = serde_json::from_slice(&input).unwrap();
        let outcome = inbox::seal_payload(
            "synthetic.json",
            &input,
            tmp.path(),
            "synthetic-machine",
            100,
            None,
            None,
        )
        .unwrap();
        let inbox::SealOutcome::Stored(saved) = outcome else {
            panic!("new bundle must be stored")
        };
        let bytes = store::concat_shards(tmp.path(), "synthetic-machine", &saved.id).unwrap();
        let record: Value = serde_json::from_slice(&bytes).unwrap();
        for field in [
            "raw",
            "fidelity",
            "file",
            "harness",
            "nativeSessionId",
            "producer",
            "platformRefs",
            "dimensions",
        ] {
            assert_eq!(record.get(field), bundle.get(field), "{name}: {field}");
        }
        let original = chat_stasher::audit_store::record_body(&bundle).unwrap();
        assert_eq!(
            chat_stasher::audit_store::record_body(&record).unwrap(),
            original
        );
        assert_eq!(record["schema"], "chat-stasher/inbox@3");
        assert_eq!(record["kind"], "harness-file");
    }
}

fn policy() -> chat_stasher::message_audit::JoinPolicy {
    chat_stasher::message_audit::JoinPolicy::new([7; 32])
}

#[test]
fn legacy_migration_is_idempotent_resumable_and_never_rewrites_bodies() {
    use chat_stasher::{audit_store, message_audit};
    let tmp = tempfile::tempdir().unwrap();
    let stage = tmp.path();
    let session = "claude-code.synthetic-native";
    let raw = b"{\"type\":\"assistant\",\"message\":{\"id\":\"synthetic-id\",\"usage\":{\"input_tokens\":0}},\"cwd\":\"/synthetic/private\"}\n";
    store::write_sealed_shard_raw_with_cap(
        store::StageWriter::Collect,
        stage,
        "synthetic-machine",
        session,
        raw,
        100,
    )
    .unwrap();
    let path = store::shard_path_with_cap(stage, "synthetic-machine", session, 1, 100);
    let before = fs::read(&path).unwrap();
    let mut report = audit_store::BackfillReport::default();
    audit_store::backfill_shard(
        stage,
        "synthetic-machine",
        session,
        raw,
        &policy(),
        &mut report,
    )
    .unwrap();
    let sidecar = stage.join("meta/synthetic-machine/message-audit-v1.jsonl");
    let partial = fs::read(&sidecar).unwrap();
    let p = message_audit::decode_jsonl(&partial).unwrap();
    assert_eq!(p.rows.len(), 1);
    assert!(p.rows[0].producer.is_none());
    assert_eq!(
        p.rows[0].fidelity.source,
        message_audit::MetadataSource::Declared
    );
    assert!(!String::from_utf8(partial.clone())
        .unwrap()
        .contains("/synthetic/private"));
    let migrated = audit_store::backfill_stage(stage, "synthetic-machine", &policy()).unwrap();
    assert_eq!(migrated.rows_recognized, 1);
    assert_eq!(fs::read(&sidecar).unwrap(), partial);
    assert_eq!(fs::read(&path).unwrap(), before);
    // A sidecar can be rebuilt, with the same secret, byte for byte.
    fs::remove_file(&sidecar).unwrap();
    audit_store::backfill_stage(stage, "synthetic-machine", &policy()).unwrap();
    assert_eq!(fs::read(&sidecar).unwrap(), partial);
    // A corrupt tail must remain visible, never discarded as an empty file.
    fs::write(&sidecar, b"{\"incomplete\":").unwrap();
    assert!(audit_store::backfill_stage(stage, "synthetic-machine", &policy()).is_err());
    assert_eq!(fs::read(&sidecar).unwrap(), b"{\"incomplete\":");
    assert!(audit_store::backfill_stage(stage, "missing-machine", &policy()).is_err());
}

#[test]
fn inbox_backfill_matches_projection_and_refuses_bad_slices_before_stage_writes() {
    use chat_stasher::{audit_store, message_audit};
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/fixtures/inbox");
    for name in [
        "harness-raw.json",
        "binary-slice.json",
        "web-full.json",
        "legacy-v1.json",
        "legacy-v2.json",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let input = fs::read(root.join(name)).unwrap();
        let inbox::SealOutcome::Stored(saved) = inbox::seal_payload(
            "synthetic.json",
            &input,
            tmp.path(),
            "synthetic-machine",
            100,
            None,
            None,
        )
        .unwrap() else {
            panic!("must store")
        };
        audit_store::backfill_stage(tmp.path(), "synthetic-machine", &policy()).unwrap();
        let actual = message_audit::decode_jsonl(
            &fs::read(
                tmp.path()
                    .join("meta/synthetic-machine/message-audit-v1.jsonl"),
            )
            .unwrap(),
        )
        .unwrap();
        let expected = message_audit::project_bundle(&input, &saved.id, &policy()).unwrap();
        assert_eq!(actual, expected, "{name}");
        assert!(matches!(
            inbox::seal_payload(
                "synthetic.json",
                &input,
                tmp.path(),
                "synthetic-machine",
                100,
                None,
                None
            )
            .unwrap(),
            inbox::SealOutcome::Duplicate(_)
        ));
    }
    for name in [
        "bad-digest.json",
        "bad-length.json",
        "bad-base64.json",
        "unsafe-path-0.json",
        "missing-fidelity.json",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let stage = tmp.path().join("stage");
        assert!(inbox::seal_payload(
            "synthetic.json",
            &fs::read(root.join(name)).unwrap(),
            &stage,
            "synthetic-machine",
            100,
            None,
            None
        )
        .is_err());
        assert!(!stage.exists());
    }
}

#[path = "../src/test_support.rs"]
mod test_support;

#[test]
fn durable_ingestion_failure_then_duplicate_retry_repairs_sidecar() {
    use chat_stasher::{audit_store, message_audit};
    use std::process::Command;
    let sandbox = test_support::Sandbox::new();
    let root = sandbox.root();
    let stage = root.join("stage");
    let incoming = root.join("inbox");
    let key = root.join("audit-key");
    fs::create_dir_all(&incoming).unwrap();
    fs::write(&key, [7; 32]).unwrap();
    let body =
        r#"{"type":"assistant","message":{"id":"synthetic-event","usage":{"input_tokens":0}}}"#;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(body.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let input = serde_json::json!({"schema":"chat-stasher/inbox@3","kind":"harness-file","harness":"claude-code","nativeSessionId":"synthetic-native","file":{"role":"transcript","relPath":"synthetic.jsonl","byteStart":0,"byteEnd":body.len(),"sha256":digest},"raw":{"encoding":"utf-8","data":body},"fidelity":{"value":"raw"}});
    let input_path = incoming.join("synthetic.json");
    fs::write(&input_path, serde_json::to_vec(&input).unwrap()).unwrap();
    let sidecar = stage.join("meta/synthetic-machine/message-audit-v1.jsonl");
    fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
    // Simulate sidecar IO failure after sealing; the input must not retire.
    fs::create_dir(&sidecar).unwrap();
    let run = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        sandbox.apply(&mut cmd);
        cmd.env(audit_store::KEY_FILE_ENV, &key)
            .args(["ingest", "--machine", "synthetic-machine", "--inbox"])
            .arg(&incoming)
            .arg("--stage")
            .arg(&stage)
            .output()
            .unwrap()
    };
    assert!(!run().status.success());
    assert!(input_path.exists());
    assert_eq!(store::sealed_shard_count(&stage).unwrap(), 1);
    let body_before =
        store::concat_shards(&stage, "synthetic-machine", "claude-code.synthetic-native").unwrap();
    fs::remove_dir(&sidecar).unwrap();
    assert!(run().status.success());
    assert!(!input_path.exists());
    assert_eq!(store::sealed_shard_count(&stage).unwrap(), 1);
    assert_eq!(
        store::concat_shards(&stage, "synthetic-machine", "claude-code.synthetic-native").unwrap(),
        body_before
    );
    let actual = fs::read(&sidecar).unwrap();
    let p = message_audit::decode_jsonl(&actual).unwrap();
    assert_eq!(p.rows.len(), 1);
    assert_eq!(
        p.rows[0].fidelity.source,
        message_audit::MetadataSource::Captured
    );
    assert_eq!(
        p.rows[0].usage["usage"],
        message_audit::Usage::Object(std::collections::BTreeMap::from([(
            "input_tokens".into(),
            message_audit::Usage::Number(0.into())
        )]))
    );
    // Completed migration uses exactly the same durable-body coordinates.
    audit_store::backfill_stage(&stage, "synthetic-machine", &policy()).unwrap();
    assert_eq!(fs::read(sidecar).unwrap(), actual);
    let mut migration = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    sandbox.apply(&mut migration);
    let report = migration
        .args([
            "audit-backfill",
            "--machine",
            "synthetic-machine",
            "--stage-only",
            "--stage",
        ])
        .arg(&stage)
        .arg("--join-key-file")
        .arg(&key)
        .output()
        .unwrap();
    assert!(report.status.success());
    let stdout = String::from_utf8(report.stdout).unwrap();
    assert!(stdout.contains("scan complete"));
    assert!(stdout.contains("recognized=1"));
    assert!(!stdout.contains(&root.to_string_lossy().to_string()));
    assert!(!stdout.contains("synthetic-event"));
    fs::write(&key, b"invalid").unwrap();
    let failure = migration.output().unwrap();
    assert_eq!(failure.status.code(), Some(3));
    assert!(String::from_utf8(failure.stderr)
        .unwrap()
        .contains("coverage unknown"));
}

#[test]
fn historical_destination_backfill_includes_reclaimed_shards_and_resumes() {
    use chat_stasher::{audit_store, message_audit};
    let tmp = tempfile::tempdir().unwrap();
    let stage = tmp.path().join("stage");
    let cfg = store::StoreConfig {
        repo_root: tmp.path().join("repo").to_string_lossy().into_owned(),
        key_file: tmp.path().join("masterkey"),
        connections: 1,
        options: std::collections::BTreeMap::new(),
        cache_dir: Some(tmp.path().join("rustic-cache")),
        no_cache: false,
    };
    let mk = rustic_core::repofile::MasterKey::new();
    let backup = store::BackupStore::new(cfg, "synthetic-machine".into());
    drop(backup.open_or_init(&mk).unwrap());
    let session = "claude-code.synthetic-native";
    let body = b"{\"type\":\"assistant\",\"message\":{\"id\":\"synthetic-first\"}}\n";
    store::write_sealed_shard_raw_with_cap(
        store::StageWriter::Collect,
        &stage,
        "synthetic-machine",
        session,
        body,
        100,
    )
    .unwrap();
    backup.push(&stage, &mk).unwrap();
    let source = store::shard_path_with_cap(&stage, "synthetic-machine", session, 1, 100);
    let digest_before = fs::read(&source).unwrap();
    fs::remove_file(source).unwrap();
    store::write_sealed_shard_raw_with_cap(
        store::StageWriter::Collect,
        &stage,
        "synthetic-machine",
        session,
        b"{\"type\":\"user\",\"uuid\":\"synthetic-second\"}\n",
        100,
    )
    .unwrap();
    backup.push(&stage, &mk).unwrap();
    fs::remove_dir_all(stage.join("sessions")).unwrap();
    let report =
        audit_store::backfill_archive(&backup, &mk, &stage, "synthetic-machine", &policy())
            .unwrap();
    assert_eq!(report.shards_scanned, 2);
    assert_eq!(report.rows_recognized, 2);
    let sidecar = stage.join("meta/synthetic-machine/message-audit-v1.jsonl");
    let once = fs::read(&sidecar).unwrap();
    audit_store::backfill_archive(&backup, &mk, &stage, "synthetic-machine", &policy()).unwrap();
    assert_eq!(fs::read(&sidecar).unwrap(), once);
    assert_eq!(message_audit::decode_jsonl(&once).unwrap().rows.len(), 2);
    assert!(
        !stage.join("sessions").exists(),
        "backfill must not restore/reseal bodies"
    );
    let archived = backup
        .read_session_concat("synthetic-machine", session, &mk)
        .unwrap();
    assert!(archived.0.starts_with(&digest_before));
}

#[test]
fn collect_writes_audit_after_sealing_and_repeat_is_idempotent() {
    use chat_stasher::{audit_store, message_audit};
    use std::process::Command;
    let sandbox = test_support::Sandbox::new();
    sandbox.ensure_dirs();
    let root = sandbox.root();
    let source = root.join("sources");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("019bf00d-97b6-7eb2-9bf8-eacbacc09765.jsonl"), b"{\"type\":\"assistant\",\"message\":{\"id\":\"synthetic-event\",\"usage\":{\"input_tokens\":0}}}\n").unwrap();
    let cell = serde_json::json!({"template":source,"format":"jsonl","session_pattern":"*.jsonl","confidence":"measured-locally","source":"synthetic fixture"});
    let registry = root.join("registry.json");
    fs::write(&registry, serde_json::to_vec(&serde_json::json!({"schema_version":1,"generated":"synthetic","harnesses":[{"id":"claude-code","display_name":"Claude Code","paths":{"macos":cell,"linux":cell,"windows":cell}}]})).unwrap()).unwrap();
    let key = root.join("audit-key");
    fs::write(&key, [7; 32]).unwrap();
    let stage = root.join("stage");
    let run = || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        sandbox.apply(&mut cmd);
        cmd.env("CHAT_STASHER_REGISTRY", &registry)
            .env(audit_store::KEY_FILE_ENV, &key)
            .args(["collect", "--machine", "synthetic-machine", "--stage"])
            .arg(&stage)
            .arg("--repo")
            .arg(root.join("repo"))
            .output()
            .unwrap()
    };
    assert!(run().status.success());
    let sidecar = stage.join("meta/synthetic-machine/message-audit-v1.jsonl");
    let once = fs::read(&sidecar).unwrap();
    assert_eq!(message_audit::decode_jsonl(&once).unwrap().rows.len(), 1);
    assert!(run().status.success());
    assert_eq!(fs::read(&sidecar).unwrap(), once);
    audit_store::backfill_stage(&stage, "synthetic-machine", &policy()).unwrap();
    assert_eq!(fs::read(&sidecar).unwrap(), once);
}
