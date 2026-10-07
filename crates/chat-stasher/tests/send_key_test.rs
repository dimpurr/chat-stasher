//! Portable posting keys and trusted puller policy, with synthetic secrets only.
#[path = "../src/test_support.rs"]
mod test_support;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use std::process::Command;
use test_support::Sandbox;

fn run(s: &Sandbox, args: &[&str]) -> std::process::Output {
    s.apply(&mut Command::new(env!("CARGO_BIN_EXE_chat-stasher")))
        .args(args)
        .output()
        .unwrap()
}
fn setup(s: &Sandbox) -> std::path::PathBuf {
    assert!(run(
        s,
        &[
            "inbox-init",
            "synthetic-inbox",
            "--locator",
            "memory://synthetic-inbox"
        ]
    )
    .status
    .success());
    let file = s.root().join("synthetic-credential.json");
    std::fs::write(&file, br#"{"token":"synthetic-write-only-credential"}"#).unwrap();
    file
}
fn issue(s: &Sandbox, file: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec![
        "send-key",
        "synthetic-inbox",
        "--for",
        "synthetic-cloud",
        "--credential-file",
        file.to_str().unwrap(),
    ];
    args.extend(extra);
    run(s, &args)
}
fn decode(output: &std::process::Output) -> serde_json::Value {
    assert!(output.status.success());
    let token = std::str::from_utf8(&output.stdout).unwrap().trim();
    let body = token
        .strip_prefix("cs-send-v1.")
        .expect("versioned posting key");
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(body).unwrap()).unwrap()
}
fn root(s: &Sandbox) -> std::path::PathBuf {
    s.config_home()
        .join("chat-stasher/inboxes/synthetic-inbox.keys")
}
#[test]
fn issuance_persists_only_public_policy_and_defaults_to_one_day() {
    let s = Sandbox::new();
    let file = setup(&s);
    let result = issue(&s, &file, &["--label", "synthetic-label"]);
    let key = decode(&result);
    assert_eq!(key["schema"], "chat-stasher/send-key@1");
    assert_eq!(key["locator"], "memory://synthetic-inbox");
    assert_eq!(
        key["credential"]["token"],
        "synthetic-write-only-credential"
    );
    assert_eq!(key["platform"], "synthetic-cloud");
    assert_eq!(key["label"], "synthetic-label");
    assert_eq!(
        key["expires_at"].as_u64().unwrap() - key["issued_at"].as_u64().unwrap(),
        86400
    );
    let secret: [u8; 32] = URL_SAFE_NO_PAD
        .decode(key["signing_secret"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let signing = ed25519_dalek::SigningKey::from_bytes(&secret);
    let public = URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes());
    let policy: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root(&s).join(format!("{}.json", key["key_id"].as_str().unwrap()))).unwrap(),
    )
    .unwrap();
    assert_eq!(policy["public_key"], public);
    assert_eq!(policy["expires_at"], key["expires_at"]);
    assert!(policy.get("signing_secret").is_none());
    assert!(policy.get("credential").is_none());
    assert!(key.get("identity").is_none());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("synthetic-write-only-credential"));
    assert!(!s.data_home().exists());
    assert!(!s.config_home().join("chat-stasher/config.toml").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for entry in std::fs::read_dir(root(&s)).unwrap() {
            assert_eq!(
                entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
#[test]
fn list_revoke_and_explicit_long_expiry_survive_new_processes() {
    let s = Sandbox::new();
    let file = setup(&s);
    let key = decode(&issue(&s, &file, &["--expires", "90d"]));
    assert_eq!(
        key["expires_at"].as_u64().unwrap() - key["issued_at"].as_u64().unwrap(),
        90 * 86400
    );
    let id = key["key_id"].as_str().unwrap();
    let list = run(&s, &["send-key", "synthetic-inbox", "--list"]);
    assert!(list.status.success());
    let summaries: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(summaries[0]["key_id"], id);
    assert_eq!(summaries[0]["state"], "active");
    assert!(!String::from_utf8_lossy(&list.stdout).contains("synthetic-write-only-credential"));
    assert!(run(&s, &["send-key", "synthetic-inbox", "--revoke", id])
        .status
        .success());
    assert!(run(&s, &["send-key", "synthetic-inbox", "--revoke", id])
        .status
        .success());
    let list = run(&s, &["send-key", "synthetic-inbox", "--list"]);
    let summaries: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(summaries[0]["state"], "revoked");
    assert_eq!(
        run(
            &s,
            &["send-key", "synthetic-inbox", "--revoke", &"a".repeat(64)]
        )
        .status
        .code(),
        Some(1)
    );
}
#[test]
fn invalid_usage_and_credentials_are_bounded_and_secret_safe() {
    let s = Sandbox::new();
    let file = setup(&s);
    for duration in ["0h", "forever", "18446744073709551615d"] {
        assert_eq!(
            issue(&s, &file, &["--expires", duration]).status.code(),
            Some(2)
        );
    }
    // Multiplication fits, but adding the current timestamp must not wrap.
    let near_max = format!("{}h", u64::MAX / 3600);
    assert_eq!(
        issue(&s, &file, &["--expires", &near_max]).status.code(),
        Some(2)
    );
    assert_eq!(
        run(
            &s,
            &[
                "send-key",
                "synthetic-inbox",
                "--list",
                "--for",
                "synthetic-cloud"
            ]
        )
        .status
        .code(),
        Some(2)
    );
    assert!(!root(&s).exists());
    std::fs::write(&file, b"synthetic-credential-secret-invalid-json").unwrap();
    let result = issue(&s, &file, &[]);
    assert_eq!(result.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&result.stderr).contains("synthetic-credential-secret"));
    std::fs::remove_file(&file).unwrap();
    assert_eq!(issue(&s, &file, &[]).status.code(), Some(3));
    assert!(!root(&s).exists());
}
#[test]
fn concurrent_issuance_and_revocation_keep_every_policy() {
    let s = Sandbox::new();
    let file = setup(&s);
    let keys = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| decode(&issue(&s, &file, &[]))))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    std::thread::scope(|scope| {
        for key in &keys {
            let s = &s;
            scope.spawn(move || {
                assert!(run(
                    s,
                    &[
                        "send-key",
                        "synthetic-inbox",
                        "--revoke",
                        key["key_id"].as_str().unwrap()
                    ]
                )
                .status
                .success())
            });
        }
    });
    let list = run(&s, &["send-key", "synthetic-inbox", "--list"]);
    let summaries: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(summaries.as_array().unwrap().len(), 4);
    assert!(summaries
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["state"] == "revoked"));
    assert_eq!(std::fs::read_dir(root(&s)).unwrap().count(), 8);
}
#[test]
fn corrupt_policy_is_never_reported_as_an_empty_key_list() {
    let s = Sandbox::new();
    let file = setup(&s);
    let key = decode(&issue(&s, &file, &[]));
    let path = root(&s).join(format!("{}.json", key["key_id"].as_str().unwrap()));
    std::fs::write(&path, b"synthetic-corrupt-policy-secret").unwrap();
    let result = run(&s, &["send-key", "synthetic-inbox", "--list"]);
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("synthetic-corrupt-policy-secret"));
    assert_eq!(
        std::fs::read(path).unwrap(),
        b"synthetic-corrupt-policy-secret"
    );
}

#[test]
fn reloaded_posting_key_and_trusted_policy_enforce_crypto_scope_expiry_and_revocation() {
    use chat_stasher::{
        remote_inbox::{ArchiveProof, Refusal, RemoteInbox},
        send_key,
    };
    struct NoArchive;
    impl ArchiveProof for NoArchive {
        fn holds(&self, _: &str, _: &chat_stasher::inbox::SealOutcome) -> anyhow::Result<bool> {
            Ok(false)
        }
    }
    let s = Sandbox::new();
    let file = setup(&s);
    let issued = issue(&s, &file, &[]);
    let token = std::str::from_utf8(&issued.stdout).unwrap().trim();
    let key = send_key::SendKey::parse(token).unwrap();
    let config_root = root(&s).parent().unwrap().to_path_buf();
    let config = chat_stasher::inbox_config::load(&config_root, "synthetic-inbox")
        .unwrap()
        .unwrap();
    assert_eq!(key.recipient.to_string(), config.recipient().to_string());
    let (policies, summaries) =
        send_key::load_policies(&config_root, "synthetic-inbox", key.issued_at).unwrap();
    assert_eq!(
        policies[&key.key_id].public_key,
        key.signing.verifying_key()
    );
    assert_eq!(summaries[0].state, "active");
    assert_eq!(
        send_key::load_policies(&config_root, "synthetic-inbox", key.expires_at)
            .unwrap()
            .1[0]
            .state,
        "expired"
    );
    let rt = tokio::runtime::Runtime::new().unwrap();
    let entered = rt.enter();
    let op = opendal::Operator::new(opendal::services::Memory::default())
        .unwrap()
        .finish();
    let remote = RemoteInbox::new(opendal::blocking::Operator::new(op).unwrap(), 1024 * 1024);
    drop(entered);
    let mut bundle: serde_json::Value = serde_json::from_slice(include_bytes!(
        "../../../contracts/fixtures/inbox/cloud-account.json"
    ))
    .unwrap();
    bundle["producer"]["sendKeyId"] = key.key_id.clone().into();
    bundle["producer"]["platform"] = key.platform.clone().into();
    let bytes = serde_json::to_vec(&bundle).unwrap();
    chat_stasher::send::send_bundle(&remote, &bytes, &key.key_id, &key.recipient, &key.signing)
        .unwrap();
    let stage = s.root().join("synthetic-pull-stage");
    let report = chat_stasher::remote_inbox::pull(
        &remote,
        &config.identity,
        &policies,
        key.issued_at,
        &stage,
        "synthetic-puller",
        1024 * 1024,
        &NoArchive,
    )
    .unwrap();
    assert_eq!(report.stored, 1);
    assert_eq!(report.refused, [Refusal::Unproven]);
    let report = chat_stasher::remote_inbox::pull(
        &remote,
        &config.identity,
        &policies,
        key.expires_at,
        &stage,
        "synthetic-puller",
        1024 * 1024,
        &NoArchive,
    )
    .unwrap();
    assert_eq!(report.refused, [Refusal::ExpiredKey]);
    send_key::revoke(&config_root, "synthetic-inbox", &key.key_id).unwrap();
    let (revoked, _) =
        send_key::load_policies(&config_root, "synthetic-inbox", key.issued_at).unwrap();
    let report = chat_stasher::remote_inbox::pull(
        &remote,
        &config.identity,
        &revoked,
        key.issued_at,
        &stage,
        "synthetic-puller",
        1024 * 1024,
        &NoArchive,
    )
    .unwrap();
    assert_eq!(report.refused, [Refusal::RevokedKey]);
    let mut wrong_scope = policies;
    wrong_scope.get_mut(&key.key_id).unwrap().platform = "synthetic-other".into();
    let report = chat_stasher::remote_inbox::pull(
        &remote,
        &config.identity,
        &wrong_scope,
        key.issued_at,
        &stage,
        "synthetic-puller",
        1024 * 1024,
        &NoArchive,
    )
    .unwrap();
    assert_eq!(report.refused, [Refusal::Scope]);
}

#[test]
fn posting_parser_refuses_bad_versions_material_unknown_fields_and_oversize() {
    use chat_stasher::send_key::SendKey;
    let s = Sandbox::new();
    let file = setup(&s);
    let key = decode(&issue(&s, &file, &[]));
    for (field, value) in [
        ("schema", "chat-stasher/send-key@999"),
        ("key_id", "synthetic-wrong"),
        ("signing_secret", "synthetic-invalid"),
        ("recipient", "synthetic-invalid"),
        ("identity", "synthetic-forbidden-decryption-capability"),
    ] {
        let mut modified = key.clone();
        modified[field] = value.into();
        let token = format!(
            "cs-send-v1.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&modified).unwrap())
        );
        let error = match SendKey::parse(&token) {
            Ok(_) => panic!("invalid key accepted"),
            Err(e) => e,
        };
        assert!(!error.to_string().contains("synthetic-forbidden"));
    }
    assert!(SendKey::parse("cs-send-v2.synthetic").is_err());
    assert!(SendKey::parse(&format!("cs-send-v1.{}", "a".repeat(131073))).is_err());
}

#[test]
fn known_empty_policies_are_distinct_from_undeclared_and_unsafe_state() {
    let s = Sandbox::new();
    assert_eq!(
        run(&s, &["send-key", "synthetic-inbox", "--list"])
            .status
            .code(),
        Some(1)
    );
    let file = setup(&s);
    let list = run(&s, &["send-key", "synthetic-inbox", "--list"]);
    assert!(list.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&list.stdout).unwrap(),
        serde_json::json!([])
    );
    let key = decode(&issue(&s, &file, &[]));
    let id = key["key_id"].as_str().unwrap();
    let policy_path = root(&s).join(format!("{id}.json"));
    let original = std::fs::read(&policy_path).unwrap();
    for (field, value) in [
        ("schema", "synthetic-unknown"),
        ("public_key", "synthetic-invalid"),
    ] {
        let mut modified: serde_json::Value = serde_json::from_slice(&original).unwrap();
        modified[field] = value.into();
        std::fs::write(&policy_path, serde_json::to_vec(&modified).unwrap()).unwrap();
        let list = run(&s, &["send-key", "synthetic-inbox", "--list"]);
        assert_eq!(list.status.code(), Some(1));
        assert!(list.stdout.is_empty());
    }
    std::fs::write(&policy_path, original).unwrap();
    assert!(run(&s, &["send-key", "synthetic-inbox", "--revoke", id])
        .status
        .success());
    std::fs::write(
        root(&s).join(format!("{id}.revoked")),
        b"synthetic-corrupt-revocation",
    )
    .unwrap();
    assert_eq!(
        run(&s, &["send-key", "synthetic-inbox", "--list"])
            .status
            .code(),
        Some(1)
    );
    assert_eq!(
        run(&s, &["send-key", "synthetic-inbox", "--revoke", id])
            .status
            .code(),
        Some(1)
    );
}

// Unix mode bits and symlink creation are checked here; Windows records inherit
// the configuration ACL, whose permission hardening is not implemented by this slice.
#[cfg(unix)]
#[test]
fn unsafe_policy_files_and_symlinked_directory_are_refused_without_writes() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let s = Sandbox::new();
    let file = setup(&s);
    let key = decode(&issue(&s, &file, &[]));
    let path = root(&s).join(format!("{}.json", key["key_id"].as_str().unwrap()));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        run(&s, &["send-key", "synthetic-inbox", "--list"])
            .status
            .code(),
        Some(1)
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let target = s.root().join("synthetic-policy-target");
    std::fs::rename(&path, &target).unwrap();
    symlink(&target, &path).unwrap();
    assert_eq!(
        run(&s, &["send-key", "synthetic-inbox", "--list"])
            .status
            .code(),
        Some(1)
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&target, &path).unwrap();
    let directory_target = s.root().join("synthetic-policy-directory");
    std::fs::rename(root(&s), &directory_target).unwrap();
    symlink(&directory_target, root(&s)).unwrap();
    assert_eq!(issue(&s, &file, &[]).status.code(), Some(1));
    assert_eq!(std::fs::read_dir(directory_target).unwrap().count(), 1);
}
