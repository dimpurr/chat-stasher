//! Keyless explicit-file producer, using only synthetic files and a local inbox.
#[path = "../src/test_support.rs"]
mod test_support;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chat_stasher::{bundle_transport::BundleTransport, remote_inbox::*, send_key::SendKey};
use std::{path::PathBuf, process::Command};
use test_support::Sandbox;

struct Fixture {
    owner: Sandbox,
    producer: Sandbox,
    inbox: PathBuf,
    input: PathBuf,
    token: String,
}
fn run(s: &Sandbox, args: &[&str], token: Option<&str>) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    s.apply(&mut command).env_remove("CHAT_STASHER_SEND_KEY");
    if let Some(token) = token {
        command.env("CHAT_STASHER_SEND_KEY", token);
    }
    command.args(args).output().unwrap()
}
impl Fixture {
    fn new() -> Self {
        let owner = Sandbox::new();
        let producer = Sandbox::new();
        let inbox = owner.root().join("synthetic-inbox");
        let locator = format!("fs://{}", inbox.display());
        assert!(run(
            &owner,
            &["inbox-init", "synthetic-inbox", "--locator", &locator],
            None
        )
        .status
        .success());
        let credential = owner.root().join("synthetic-credential.json");
        std::fs::write(&credential, br#"{"token":"synthetic-posting-secret"}"#).unwrap();
        let result = run(
            &owner,
            &[
                "send-key",
                "synthetic-inbox",
                "--for",
                "synthetic-cloud",
                "--credential-file",
                credential.to_str().unwrap(),
            ],
            None,
        );
        assert!(result.status.success());
        let token = String::from_utf8(result.stdout).unwrap().trim().to_owned();
        let input = producer.root().join("synthetic-transcript.jsonl");
        std::fs::write(&input, b"{}\n").unwrap();
        Self {
            owner,
            producer,
            inbox,
            input,
            token,
        }
    }
    fn send(&self) -> std::process::Output {
        self.send_token(&self.token)
    }
    fn send_token(&self, token: &str) -> std::process::Output {
        run(
            &self.producer,
            &[
                "send",
                "--path",
                self.input.to_str().unwrap(),
                "--session",
                "synthetic-native",
            ],
            Some(token),
        )
    }
    fn objects(&self) -> Vec<PathBuf> {
        if !self.inbox.exists() {
            return Vec::new();
        }
        std::fs::read_dir(&self.inbox)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.is_file())
            .collect()
    }
    fn cursor_root(&self) -> PathBuf {
        self.producer.state_home().join("chat-stasher/send-cursors")
    }
}
struct NoArchive;
impl ArchiveProof for NoArchive {
    fn holds(&self, _: &str, _: &chat_stasher::inbox::SealOutcome) -> anyhow::Result<bool> {
        Ok(false)
    }
}
#[test]
fn cli_upload_reaches_the_existing_puller_and_sink_without_producer_identity() {
    let f = Fixture::new();
    // Producer config must not be parsed, even when it would refuse a normal command.
    std::fs::create_dir_all(f.producer.config_home().join("chat-stasher")).unwrap();
    let config = f.producer.config_home().join("chat-stasher/config.toml");
    std::fs::write(&config, "synthetic-invalid-config").unwrap();
    let result = f.send();
    assert!(
        result.status.success(),
        "send exit {:?}",
        result.status.code()
    );
    assert_eq!(result.stdout, b"send complete: uploaded=1; resumed=0\n");
    assert_eq!(f.objects().len(), 1);
    let object = &f.objects()[0];
    assert_eq!(object.file_name().unwrap().to_str().unwrap().len(), 64);
    let envelope: serde_json::Value =
        serde_json::from_slice(&std::fs::read(object).unwrap()).unwrap();
    assert!(envelope.get("raw").is_none());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("synthetic-posting-secret"));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let entered = runtime.enter();
    let op =
        opendal::Operator::new(opendal::services::Fs::default().root(f.inbox.to_str().unwrap()))
            .unwrap()
            .finish();
    let remote = RemoteInbox::new(
        opendal::blocking::Operator::new(op).unwrap(),
        10 * 1024 * 1024,
    );
    drop(entered);
    let root = f.owner.config_home().join("chat-stasher/inboxes");
    let owner = chat_stasher::inbox_config::load(&root, "synthetic-inbox")
        .unwrap()
        .unwrap();
    let now = chrono::Utc::now().timestamp() as u64;
    let (policies, _) =
        chat_stasher::send_key::load_policies(&root, "synthetic-inbox", now).unwrap();
    let stage = f.owner.root().join("synthetic-stage");
    let report = chat_stasher::remote_inbox::pull(
        &remote,
        &owner.identity,
        &policies,
        now,
        &stage,
        "synthetic-puller",
        8,
        &f.owner.root().join("synthetic-rates.sqlite3"),
        &NoArchive,
    )
    .unwrap();
    assert_eq!(report.stored, 1);
    assert_eq!(report.refused, vec![Refusal::Unproven]);
    assert_eq!(remote.list().unwrap().total_inbox_files, 1);
    assert!(!f.producer.data_home().exists());
    assert_eq!(std::fs::read(&config).unwrap(), b"synthetic-invalid-config");
    assert_eq!(
        SendKey::parse(&f.token).unwrap().platform,
        "synthetic-cloud"
    );
}
#[test]
fn cursor_resumes_across_processes_and_content_changes_upload_again() {
    let f = Fixture::new();
    assert!(f.send().status.success());
    let again = f.send();
    assert!(again.status.success());
    assert_eq!(again.stdout, b"send complete: uploaded=0; resumed=1\n");
    assert_eq!(f.objects().len(), 1);
    std::fs::write(&f.input, b"{\"synthetic\":true}\n").unwrap();
    assert!(f.send().status.success());
    assert_eq!(f.objects().len(), 2);
    for record in std::fs::read_dir(f.cursor_root()).unwrap() {
        let bytes = std::fs::read(record.unwrap().path()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-posting-secret"));
        assert!(!String::from_utf8_lossy(&bytes).contains("synthetic-transcript"));
    }
}
#[test]
fn absent_corrupt_or_unwritable_cursor_only_causes_a_safe_resend() {
    let f = Fixture::new();
    assert!(f.send().status.success());
    let record = std::fs::read_dir(f.cursor_root())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(&record, b"synthetic-corrupt-cursor").unwrap();
    assert!(f.send().status.success());
    assert_eq!(f.objects().len(), 2);
    std::fs::remove_dir_all(f.cursor_root()).unwrap();
    std::fs::write(f.cursor_root(), b"synthetic-cursor-blocker").unwrap();
    let result = f.send();
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("resume cursor unavailable"));
    assert!(f.send().status.success());
    assert_eq!(f.objects().len(), 4);
}
#[test]
fn expiry_and_invalid_secrets_refuse_before_opening_the_backend() {
    let f = Fixture::new();
    let mut record: serde_json::Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(f.token.strip_prefix("cs-send-v1.").unwrap())
            .unwrap(),
    )
    .unwrap();
    record["issued_at"] = 1.into();
    record["expires_at"] = 2.into();
    let expired = format!(
        "cs-send-v1.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&record).unwrap())
    );
    for token in [&expired, "synthetic-invalid-secret"] {
        let result = f.send_token(token);
        assert_eq!(result.status.code(), Some(1));
        assert!(!String::from_utf8_lossy(&result.stderr).contains(token));
    }
    assert!(!f.inbox.exists());
    assert!(!f.cursor_root().exists());
}
#[test]
fn usage_read_and_upload_failures_have_distinct_exit_codes_and_no_success_cursor() {
    let f = Fixture::new();
    assert_eq!(
        run(
            &f.producer,
            &[
                "send",
                "--path",
                f.input.to_str().unwrap(),
                "--session",
                "synthetic-native"
            ],
            None
        )
        .status
        .code(),
        Some(2)
    );
    assert_eq!(
        run(
            &f.producer,
            &["send", "--path", f.input.to_str().unwrap()],
            Some(&f.token)
        )
        .status
        .code(),
        Some(2)
    );
    std::fs::remove_file(&f.input).unwrap();
    assert_eq!(f.send().status.code(), Some(3));
    assert!(!f.inbox.exists());
    std::fs::write(&f.input, b"{}\n").unwrap();
    std::fs::write(&f.inbox, b"synthetic-backend-blocker").unwrap();
    let failed = f.send();
    assert_eq!(failed.status.code(), Some(3));
    assert!(!f.cursor_root().exists());
    assert!(!String::from_utf8_lossy(&failed.stderr).contains(f.inbox.to_str().unwrap()));
    std::fs::remove_file(&f.inbox).unwrap();
    assert!(f.send().status.success());
    assert_eq!(f.objects().len(), 1);
}
#[test]
fn oversized_input_is_a_completed_refusal_without_upload() {
    let f = Fixture::new();
    std::fs::write(&f.input, vec![0; 4 * 1024 * 1024 + 1]).unwrap();
    assert_eq!(f.send().status.code(), Some(1));
    assert!(!f.inbox.exists());
    assert!(!f.cursor_root().exists());
}
