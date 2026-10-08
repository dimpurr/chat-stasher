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
        Self::with_limit(512)
    }
    fn with_limit(limit: usize) -> Self {
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
                "--max-objects-per-pull",
                &limit.to_string(),
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

fn pull_cli(f: &Fixture, stage: &std::path::Path) -> std::process::Output {
    run(
        &f.owner,
        &[
            "inbox-pull",
            "synthetic-inbox",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "synthetic-puller",
        ],
        None,
    )
}

fn inbox_view(f: &Fixture, command: &str) -> serde_json::Value {
    let output = run(&f.owner, &[command, "--json"], None);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["remote_inboxes"]["inboxes"].is_array());
    value["remote_inboxes"]["inboxes"][0].clone()
}

#[test]
fn inbox_visibility_persists_success_refusals_and_missing_account_without_claiming_archive() {
    let f = Fixture::new();
    let stage = f.owner.root().join("synthetic-stage");
    let before = inbox_view(&f, "status");
    assert!(before["waiting"].is_null());
    assert!(before["last_successful_pull"].is_null());
    assert_eq!(before["history_state"], "not_recorded");
    std::fs::create_dir(&f.inbox).unwrap();
    assert_eq!(pull_cli(&f, &stage).status.code(), Some(0));
    let empty = inbox_view(&f, "status");
    assert_eq!(empty["waiting"], 0);
    assert!(empty["oldest_age_secs"].is_null());
    assert!(empty["last_successful_pull"].is_u64());
    assert!(f.send().status.success());
    assert_eq!(pull_cli(&f, &stage).status.code(), Some(3));
    for command in ["status", "doctor"] {
        let view = inbox_view(&f, command);
        assert_eq!(view["waiting"], 1);
        assert!(view["oldest_age_secs"].is_u64());
        assert_eq!(view["last_successful_pull"], empty["last_successful_pull"]);
        assert_eq!(view["last_pull"]["refused"]["ArchiveRead"], 1);
        assert_eq!(view["last_pull"]["missing_account"], 1);
        assert_eq!(view["last_pull"]["exit_code"], 3);
    }
    std::fs::remove_dir_all(&f.inbox).unwrap();
    assert_eq!(pull_cli(&f, &stage).status.code(), Some(3));
    let unknown = inbox_view(&f, "doctor");
    assert!(unknown["waiting"].is_null());
    assert_eq!(unknown["last_pull"]["state"], "unknown");
    assert_eq!(
        unknown["last_successful_pull"],
        empty["last_successful_pull"]
    );
}

#[test]
fn inbox_visibility_corrupt_history_and_declaration_are_unknown_and_read_only() {
    let f = Fixture::new();
    let history = f
        .owner
        .config_home()
        .join("chat-stasher/inboxes/synthetic-inbox.status.sqlite3");
    std::fs::write(&history, b"synthetic-private-corruption").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&history, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // Other platforms inherit the sandbox directory ACL.
    let view = inbox_view(&f, "status");
    assert_eq!(view["history_state"], "unknown");
    assert!(view["last_successful_pull"].is_null());
    assert_eq!(
        std::fs::read(&history).unwrap(),
        b"synthetic-private-corruption"
    );
    let declaration = history.with_file_name("synthetic-inbox.json");
    std::fs::write(&declaration, b"synthetic-invalid-declaration").unwrap();
    let output = run(&f.owner, &["doctor", "--json"], None);
    assert_eq!(output.status.code(), Some(3));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["remote_inboxes"]["inboxes"][0]["waiting"].is_null());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-private-corruption"));
}
#[test]
fn pull_cli_seals_but_preserves_objects_without_archive_proof_and_retains_hourly_quota() {
    let f = Fixture::with_limit(1);
    assert!(f.send().status.success());
    let stage = f.owner.root().join("synthetic-stage");
    let first = pull_cli(&f, &stage);
    assert_eq!(first.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&first.stdout).contains("stored=1"));
    assert!(String::from_utf8_lossy(&first.stderr).contains("ArchiveRead"));
    assert_eq!(f.objects().len(), 1);
    let ledger = f
        .owner
        .config_home()
        .join("chat-stasher/inboxes/synthetic-inbox.rates.sqlite3");
    assert!(ledger.is_file());
    // Changing stage must not change the retained per-inbox accounting identity.
    let other = f.owner.root().join("synthetic-other-stage");
    let second = pull_cli(&f, &other);
    assert_eq!(second.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&second.stderr).contains("RateLimit"));
    assert!(!other.exists());
    assert_eq!(f.objects().len(), 1);
}
#[test]
fn pull_cli_corrupt_accounting_is_unknown_and_preserves_queued_bytes() {
    let f = Fixture::new();
    assert!(f.send().status.success());
    let ledger = f
        .owner
        .config_home()
        .join("chat-stasher/inboxes/synthetic-inbox.rates.sqlite3");
    std::fs::write(&ledger, b"synthetic-corrupt-accounting").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // On non-Unix systems the file inherits the fixture directory ACL.
    let stage = f.owner.root().join("synthetic-stage");
    let result = pull_cli(&f, &stage);
    assert_eq!(result.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&result.stderr).contains("RateAccounting"));
    assert!(!stage.exists());
    assert_eq!(f.objects().len(), 1);
    assert_eq!(
        std::fs::read(ledger).unwrap(),
        b"synthetic-corrupt-accounting"
    );
    assert!(!String::from_utf8_lossy(&result.stderr).contains("synthetic-posting-secret"));
}
#[test]
fn pull_cli_missing_backend_is_unknown_not_an_empty_inbox() {
    let f = Fixture::new();
    let stage = f.owner.root().join("synthetic-stage");
    let missing = pull_cli(&f, &stage);
    assert_eq!(missing.status.code(), Some(3));
    assert!(!f.inbox.exists());
    assert!(!stage.exists());
    assert!(!String::from_utf8_lossy(&missing.stdout).contains("waiting=0"));
    std::fs::create_dir(&f.inbox).unwrap();
    let empty = pull_cli(&f, &stage);
    assert_eq!(empty.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&empty.stdout).contains("waiting=0"));
    assert!(!stage.exists());
}
#[test]
fn pull_cli_usage_and_completed_refusals_remain_distinct() {
    let f = Fixture::new();
    assert_eq!(run(&f.owner, &["inbox-pull"], None).status.code(), Some(2));
    let stage = f.owner.root().join("synthetic-stage");
    std::fs::create_dir(&f.inbox).unwrap();
    std::fs::write(f.inbox.join("a".repeat(64)), b"{}").unwrap();
    let result = pull_cli(&f, &stage);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("Envelope"));
    assert!(!stage.exists());
    assert_eq!(f.objects().len(), 1);
}

#[test]
fn pull_cli_unsafe_ledger_and_invalid_config_fail_closed() {
    let f = Fixture::new();
    assert!(f.send().status.success());
    let ledger = f
        .owner
        .config_home()
        .join("chat-stasher/inboxes/synthetic-inbox.rates.sqlite3");
    std::fs::create_dir(&ledger).unwrap();
    let stage = f.owner.root().join("synthetic-stage");
    let result = pull_cli(&f, &stage);
    assert_eq!(result.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&result.stderr).contains("RateAccounting"));
    assert!(!stage.exists());
    assert_eq!(f.objects().len(), 1);
    let record = f
        .owner
        .config_home()
        .join("chat-stasher/inboxes/synthetic-inbox.json");
    std::fs::write(&record, b"synthetic-invalid-config").unwrap();
    let invalid = pull_cli(&f, &stage);
    assert_eq!(invalid.status.code(), Some(3));
    assert!(!stage.exists());
    assert_eq!(std::fs::read(record).unwrap(), b"synthetic-invalid-config");
}

#[test]
fn schedule_pull_renders_a_separate_executable_path_in_both_formats() {
    let f = Fixture::new();
    assert!(f.send().status.success());
    let stage = f.owner.root().join("synthetic scheduled stage");
    let binary = env!("CARGO_BIN_EXE_chat-stasher");
    for format in ["launchd", "systemd"] {
        let result = run(
            &f.owner,
            &[
                "schedule",
                "--pull",
                "synthetic-inbox",
                "--format",
                format,
                "--stage",
                stage.to_str().unwrap(),
                "--binary",
                "/synthetic/bin/chat-stasher",
                "--machine",
                "synthetic-puller",
                "--shard-bucket-cap",
                "7",
            ],
            None,
        );
        if cfg!(target_os = "windows") {
            // This build has no Windows scheduler integration. Assert its
            // refusal instead of removing the platform from coverage.
            assert_eq!(result.status.code(), Some(2));
            assert!(String::from_utf8_lossy(&result.stderr).contains("Task Scheduler"));
            return;
        }
        assert_eq!(result.status.code(), Some(0), "schedule pull must render");
        let text = String::from_utf8(result.stdout).unwrap();
        assert!(text.contains("inbox-pull"));
        assert!(text.contains("synthetic-inbox"));
        assert!(text.contains("synthetic-puller"));
        assert!(text.contains("--shard-bucket-cap"));
        assert!(!text.contains("run-once"));
        assert!(!text.contains("synthetic-posting-secret"));
        assert_eq!(f.objects().len(), 1);
        assert!(!stage.exists());
        // Execute the generated systemd argv, with only the installed binary
        // placeholder replaced by the fixture binary. This is the actual
        // scheduled path, rather than a separately constructed pull request.
        if format == "systemd" {
            let line = text.lines().find(|s| s.starts_with("ExecStart=")).unwrap();
            let argv: Vec<String> =
                serde_json::from_str(&format!("[{}]", line[10..].replace("\" \"", "\",\"")))
                    .unwrap();
            let mut command = Command::new(binary);
            f.owner.apply(&mut command);
            let pulled = command.args(&argv[1..]).output().unwrap();
            assert_eq!(pulled.status.code(), Some(3));
            assert!(String::from_utf8_lossy(&pulled.stdout).contains("waiting=1; stored=1"));
            assert!(String::from_utf8_lossy(&pulled.stderr).contains("ArchiveRead"));
            assert_eq!(f.objects().len(), 1);
        }
    }
}

#[test]
fn schedule_pull_rejects_archive_only_flags_and_unknown_inboxes() {
    let f = Fixture::new();
    let stage = f.owner.root().join("synthetic-stage");
    for extra in [
        vec!["--verify"],
        vec!["--destination", "synthetic-destination"],
        vec!["--repo", "synthetic-repo"],
        vec!["--key-file", "synthetic-key"],
        vec!["--connections", "2"],
        vec!["--option", "synthetic=value"],
        vec!["--keep-ssh-masters"],
        vec!["--unit", "reclaim-stage"],
    ] {
        let mut args = vec![
            "schedule",
            "--pull",
            "synthetic-inbox",
            "--stage",
            stage.to_str().unwrap(),
            "--binary",
            "/synthetic/bin/chat-stasher",
        ];
        args.extend(extra);
        assert_eq!(run(&f.owner, &args, None).status.code(), Some(2));
    }
    let unknown = run(
        &f.owner,
        &[
            "schedule",
            "--pull",
            "synthetic-unknown",
            "--stage",
            stage.to_str().unwrap(),
            "--binary",
            "/synthetic/bin/chat-stasher",
        ],
        None,
    );
    assert_eq!(unknown.status.code(), Some(2));
    let expected = if cfg!(target_os = "windows") {
        "Task Scheduler"
    } else {
        "initialize"
    };
    assert!(String::from_utf8_lossy(&unknown.stderr).contains(expected));
}

#[test]
fn run_once_does_not_consume_a_declared_remote_inbox() {
    let f = Fixture::new();
    assert!(f.send().status.success());
    let before = std::fs::read(&f.objects()[0]).unwrap();
    let stage = f.owner.root().join("synthetic-stage");
    let repo = f.owner.root().join("synthetic-repo");
    let key = f.owner.root().join("synthetic-masterkey.json");
    let result = run(
        &f.owner,
        &[
            "run-once",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            "synthetic-puller",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
        None,
    );
    assert_eq!(result.status.code(), Some(0));
    assert_eq!(f.objects().len(), 1);
    assert_eq!(std::fs::read(&f.objects()[0]).unwrap(), before);
    assert!(!stage.join("sessions/synthetic-puller").exists());
    assert!(!f
        .owner
        .config_home()
        .join("chat-stasher/inboxes/synthetic-inbox.rates.sqlite3")
        .exists());
}

// Native scheduler install helpers use executable POSIX shims here. Windows
// has no scheduler integration; its refusal is asserted by the render test.
#[cfg(unix)]
#[test]
fn schedule_pull_install_and_teardown_are_named_and_isolated() {
    let f = Fixture::new();
    let stage = f.owner.root().join("synthetic-stage");
    let stub = f.owner.root().join("synthetic-scheduler");
    let calls = f.owner.root().join("synthetic-calls");
    test_support::plant_executable(&stub, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$SYNTHETIC_CALLS\"\ncase \"$*\" in\n *is-active*) exit 1;;\n print*) test -f \"$SYNTHETIC_LOADED\"; exit $?;;\n bootstrap*) touch \"$SYNTHETIC_LOADED\";;\n bootout*) rm -f \"$SYNTHETIC_LOADED\";;\nesac\nexit 0\n");
    let invoke = |format: &str, action: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        f.owner.apply(&mut command);
        command
            .env("CHAT_STASHER_LAUNCHCTL", &stub)
            .env("CHAT_STASHER_SYSTEMCTL", &stub)
            .env("CHAT_STASHER_LAUNCHD_DOMAIN", "gui/12345")
            .env("SYNTHETIC_CALLS", &calls)
            .env("SYNTHETIC_LOADED", f.owner.root().join("synthetic-loaded"));
        command.args([
            "schedule",
            action,
            "--pull",
            "synthetic-inbox",
            "--format",
            format,
        ]);
        if action == "install" {
            command.args([
                "--stage",
                stage.to_str().unwrap(),
                "--binary",
                "/synthetic/bin/chat-stasher",
            ]);
        }
        command.output().unwrap()
    };
    let launch_label = "com.chat-stasher.inbox-pull.synthetic-inbox";
    let timer = "chat-stasher-inbox-pull-synthetic-inbox.timer";
    let launch_dir = f.owner.home().join("Library/LaunchAgents");
    let system_dir = f.owner.home().join(".config/systemd/user");
    std::fs::create_dir_all(&launch_dir).unwrap();
    std::fs::create_dir_all(&system_dir).unwrap();
    let archive_plist = launch_dir.join("com.chat-stasher.run-once.plist");
    let archive_timer = system_dir.join("chat-stasher-run-once.timer");
    std::fs::write(&archive_plist, "synthetic-existing-archive").unwrap();
    std::fs::write(&archive_timer, "synthetic-existing-archive").unwrap();
    let failing = f.owner.root().join("synthetic-failing-scheduler");
    test_support::plant_executable(
        &failing,
        "#!/bin/sh\ncase \"$*\" in\n *enable*) exit 1;;\nesac\nexit 0\n",
    );
    let mut failed = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    f.owner.apply(&mut failed);
    let failed = failed
        .env("CHAT_STASHER_SYSTEMCTL", &failing)
        .args([
            "schedule",
            "install",
            "--pull",
            "synthetic-inbox",
            "--format",
            "systemd",
            "--stage",
            stage.to_str().unwrap(),
            "--binary",
            "/synthetic/bin/chat-stasher",
        ])
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(!system_dir.join(timer).exists());
    assert!(!system_dir
        .join(timer.replace(".timer", ".service"))
        .exists());
    for format in ["launchd", "systemd"] {
        assert_eq!(invoke(format, "install").status.code(), Some(0));
        assert!(!stage.exists());
    }
    assert!(launch_dir.join(format!("{launch_label}.plist")).is_file());
    assert!(system_dir.join(timer).is_file());
    // Teardown must still work after declaration loss and without --stage.
    std::fs::remove_file(
        f.owner
            .config_home()
            .join("chat-stasher/inboxes/synthetic-inbox.json"),
    )
    .unwrap();
    for format in ["launchd", "systemd"] {
        let removed = invoke(format, "uninstall");
        assert_eq!(removed.status.code(), Some(0));
        let count = if format == "systemd" { 2 } else { 1 };
        assert_eq!(
            String::from_utf8(removed.stdout).unwrap(),
            format!("[schedule] removed pull unit files: {count}\n")
        );
        let again = invoke(format, "uninstall");
        assert_eq!(again.status.code(), Some(0));
        assert_eq!(again.stdout, b"[schedule] removed pull unit files: 0\n");
    }
    assert!(!launch_dir.join(format!("{launch_label}.plist")).exists());
    assert!(!system_dir.join(timer).exists());
    assert_eq!(
        std::fs::read_to_string(archive_plist).unwrap(),
        "synthetic-existing-archive"
    );
    assert_eq!(
        std::fs::read_to_string(archive_timer).unwrap(),
        "synthetic-existing-archive"
    );
    let calls = std::fs::read_to_string(calls).unwrap();
    assert!(calls.contains("bootstrap gui/12345"));
    assert!(calls.contains(&format!("bootout gui/12345 {launch_label}")));
    assert!(calls.contains(&format!("enable --now {timer}")));
    assert!(calls.contains(&format!("disable --now {timer}")));
    assert!(!calls.contains("run-once"));
}

#[test]
fn schedule_pull_configuration_failure_stays_incomplete() {
    let f = Fixture::new();
    let declaration = f
        .owner
        .config_home()
        .join("chat-stasher/inboxes/synthetic-inbox.json");
    std::fs::write(declaration, "synthetic-invalid").unwrap();
    let result = run(
        &f.owner,
        &[
            "schedule",
            "--pull",
            "synthetic-inbox",
            "--stage",
            "synthetic-stage",
            "--binary",
            "/synthetic/bin/chat-stasher",
        ],
        None,
    );
    if cfg!(target_os = "windows") {
        assert_eq!(result.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&result.stderr).contains("Task Scheduler"));
    } else {
        assert_eq!(result.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&result.stderr).contains("configuration unavailable"));
    }
}

fn sealed_capture(stage: &std::path::Path) -> serde_json::Value {
    let mut pending = vec![stage.to_path_buf()];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(std::fs::read_dir(path).unwrap().map(|e| e.unwrap().path()));
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            let bytes = std::fs::read_to_string(path).unwrap();
            for line in bytes.lines() {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                if value["kind"] == "harness-file" {
                    return value;
                }
            }
        }
    }
    panic!("synthetic capture missing");
}

#[test]
fn explicit_harness_capture_preserves_known_and_unknown_account_at_the_sink() {
    for account in [Some("synthetic-account"), None, Some("")] {
        let f = Fixture::new();
        let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        f.producer.apply(&mut command);
        command
            .env("CHAT_STASHER_SEND_KEY", &f.token)
            .env_remove("SYNTHETIC_ACCOUNT")
            .args([
                "send",
                "--path",
                f.input.to_str().unwrap(),
                "--session",
                "synthetic-native",
                "--harness",
                "claude-code",
                "--account-env",
                "SYNTHETIC_ACCOUNT",
            ]);
        if let Some(account) = account {
            command.env("SYNTHETIC_ACCOUNT", account);
        }
        let result = command.output().unwrap();
        assert_eq!(result.status.code(), Some(0));
        let known = account.is_some_and(|s| !s.is_empty());
        assert_eq!(
            String::from_utf8_lossy(&result.stderr).contains("account axis unknown"),
            !known
        );
        let stage = f.owner.root().join("synthetic-stage");
        assert_eq!(pull_cli(&f, &stage).status.code(), Some(3));
        let record = sealed_capture(&stage);
        assert_eq!(
            record["id"],
            if known {
                "claude-code.synthetic-account.synthetic-native"
            } else {
                "claude-code.synthetic-native"
            }
        );
        assert_eq!(record["fidelity"]["value"], "unknown");
        assert_eq!(
            record["dimensions"],
            serde_json::json!({"surface": ["cloud"]})
        );
        assert_eq!(record["producer"]["platform"], "synthetic-cloud");
        assert_eq!(
            record["producer"]["sendKeyId"],
            SendKey::parse(&f.token).unwrap().key_id
        );
        if known {
            assert_eq!(
                record["identity"],
                serde_json::json!({"level":"platform_uid", "value":"synthetic-account"})
            );
        } else {
            assert!(record.get("identity").is_none());
        }
        assert_eq!(
            inbox_view(&f, "status")["last_pull"]["missing_account"],
            usize::from(!known)
        );
        assert!(!f.producer.data_home().exists());
    }
}

#[test]
fn harness_selection_refuses_unknown_names_and_unreadable_registry_without_upload() {
    let f = Fixture::new();
    for harness in ["synthetic-unregistered", "unknown"] {
        let result = run(
            &f.producer,
            &[
                "send",
                "--path",
                f.input.to_str().unwrap(),
                "--session",
                "synthetic-native",
                "--harness",
                harness,
            ],
            Some(&f.token),
        );
        assert_eq!(result.status.code(), Some(1));
        assert!(!f.inbox.exists());
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    f.producer.apply(&mut command);
    let result = command
        .env("CHAT_STASHER_SEND_KEY", &f.token)
        .env(
            "CHAT_STASHER_REGISTRY",
            f.producer.root().join("synthetic-missing-registry"),
        )
        .args([
            "send",
            "--path",
            f.input.to_str().unwrap(),
            "--session",
            "synthetic-native",
            "--harness",
            "claude-code",
        ])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(3));
    assert!(!f.inbox.exists());
    assert!(!f.cursor_root().exists());
}

#[test]
fn account_selection_requires_harness_and_account_changes_invalidate_resume() {
    let f = Fixture::new();
    assert_eq!(
        run(
            &f.producer,
            &[
                "send",
                "--path",
                f.input.to_str().unwrap(),
                "--session",
                "synthetic-native",
                "--account-env",
                "SYNTHETIC_ACCOUNT"
            ],
            Some(&f.token)
        )
        .status
        .code(),
        Some(2)
    );
    for (account, expected) in [
        ("synthetic-account-a", 1),
        ("synthetic-account-a", 1),
        ("synthetic-account-b", 2),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        f.producer.apply(&mut command);
        let result = command
            .env("CHAT_STASHER_SEND_KEY", &f.token)
            .env("SYNTHETIC_ACCOUNT", account)
            .args([
                "send",
                "--path",
                f.input.to_str().unwrap(),
                "--session",
                "synthetic-native",
                "--harness",
                "claude-code",
                "--account-env",
                "SYNTHETIC_ACCOUNT",
            ])
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(0));
        assert_eq!(f.objects().len(), expected);
        assert!(!String::from_utf8_lossy(&result.stdout).contains(account));
        assert!(!String::from_utf8_lossy(&result.stderr).contains(account));
    }
}

#[test]
fn malformed_account_is_refused_without_echoing_or_uploading() {
    let f = Fixture::new();
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    f.producer.apply(&mut command);
    let account = "synthetic.account/private";
    let result = command
        .env("CHAT_STASHER_SEND_KEY", &f.token)
        .env("SYNTHETIC_ACCOUNT", account)
        .args([
            "send",
            "--path",
            f.input.to_str().unwrap(),
            "--session",
            "synthetic-native",
            "--harness",
            "claude-code",
            "--account-env",
            "SYNTHETIC_ACCOUNT",
        ])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(!String::from_utf8_lossy(&result.stderr).contains(account));
    assert!(!f.inbox.exists());
    assert!(!f.cursor_root().exists());
}

fn declare_archives(f: &Fixture) -> Vec<chat_stasher::store::StoreConfig> {
    let configs: Vec<_> = ["synthetic-a", "synthetic-b"]
        .into_iter()
        .map(|name| chat_stasher::store::StoreConfig {
            repo_root: f.owner.root().join(name).to_string_lossy().into_owned(),
            key_file: f.owner.root().join(format!("{name}.key")),
            connections: 1,
            cache_dir: Some(f.owner.root().join(format!("{name}-cache"))),
            ..Default::default()
        })
        .collect();
    let mut text = String::new();
    for (name, cfg) in ["synthetic-a", "synthetic-b"].into_iter().zip(&configs) {
        text.push_str(&format!(
            "[destinations.{name}]\nrepo = {}\nkey_file = {}\ncache_dir = {}\n",
            serde_json::to_string(&cfg.repo_root).unwrap(),
            serde_json::to_string(&cfg.key_file.to_str().unwrap()).unwrap(),
            serde_json::to_string(&cfg.cache_dir.as_ref().unwrap().to_str().unwrap()).unwrap(),
        ));
        let key = rustic_core::repofile::MasterKey::new();
        chat_stasher::store::persist_key_file(cfg, &key).unwrap();
        chat_stasher::store::BackupStore::new(cfg.clone(), "synthetic-puller".into())
            .open_or_init(&key)
            .unwrap();
    }
    std::fs::write(f.owner.config_home().join("chat-stasher/config.toml"), text).unwrap();
    configs
}

fn push_archive(cfg: &chat_stasher::store::StoreConfig, stage: &std::path::Path) {
    let key = chat_stasher::store::load_key_file(cfg).unwrap();
    chat_stasher::store::BackupStore::new(cfg.clone(), "synthetic-puller".into())
        .push(stage, &key)
        .unwrap();
}

#[test]
fn pull_cli_requires_readback_from_every_destination_and_survives_reclamation() {
    let f = Fixture::new();
    let configs = declare_archives(&f);
    let stage = f.owner.root().join("synthetic-stage");
    assert!(f.send().status.success());
    let object = std::fs::read(&f.objects()[0]).unwrap();
    let first = pull_cli(&f, &stage);
    assert_eq!(
        first.status.code(),
        Some(1),
        "readable empty archives are unproven"
    );
    assert!(String::from_utf8_lossy(&first.stderr).contains("Unproven"));
    push_archive(&configs[0], &stage);
    assert_eq!(pull_cli(&f, &stage).status.code(), Some(1));
    assert_eq!(std::fs::read(&f.objects()[0]).unwrap(), object);
    push_archive(&configs[1], &stage);
    let proven = pull_cli(&f, &stage);
    assert_eq!(proven.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&proven.stdout).contains("duplicates=1"));
    assert!(f.objects().is_empty());
    let expected = chat_stasher::verify::expected_manifest(&stage).unwrap();
    for cfg in &configs {
        let store = chat_stasher::store::BackupStore::new(cfg.clone(), "synthetic-puller".into());
        let key = chat_stasher::store::load_key_file(cfg).unwrap();
        assert!(store.reconcile_manifest(&key, &stage).unwrap().ok());
        let (body, _) = store
            .read_session_concat("synthetic-puller", &expected[0].session_id, &key)
            .unwrap();
        let row: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            chat_stasher::audit_store::record_body(&row).unwrap(),
            b"{}\n"
        );
    }
    let destinations: Vec<_> = configs
        .iter()
        .enumerate()
        .map(|(i, cfg)| chat_stasher::stagereclaim::NamedStore {
            name: format!("synthetic-{i}"),
            cfg: cfg.clone(),
        })
        .collect();
    assert!(
        !chat_stasher::stagereclaim::reclaim_stage(&stage, &destinations, true)
            .unwrap()
            .blocked()
    );
    for cfg in &configs {
        let store = chat_stasher::store::BackupStore::new(cfg.clone(), "synthetic-puller".into());
        let key = chat_stasher::store::load_key_file(cfg).unwrap();
        let report = store.reconcile_manifest(&key, &stage).unwrap();
        assert!(report.ok());
        assert_eq!(report.stored_basis_rows(), 1);
    }
    // An old destination copy cannot prove the newly sealed sequence after reclaim.
    std::fs::write(f.inbox.join("a".repeat(64)), object).unwrap();
    let resealed = pull_cli(&f, &stage);
    assert_eq!(resealed.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&resealed.stdout).contains("stored=1"));
    for cfg in &configs {
        push_archive(cfg, &stage);
    }
    assert_eq!(pull_cli(&f, &stage).status.code(), Some(0));
    assert!(f.objects().is_empty());
    for cfg in &configs {
        let store = chat_stasher::store::BackupStore::new(cfg.clone(), "synthetic-puller".into());
        let key = chat_stasher::store::load_key_file(cfg).unwrap();
        let (body, hashes) = store
            .read_session_concat("synthetic-puller", &expected[0].session_id, &key)
            .unwrap();
        assert_eq!(hashes.len(), 2);
        for line in body.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
            let row: serde_json::Value = serde_json::from_slice(line).unwrap();
            assert_eq!(
                chat_stasher::audit_store::record_body(&row).unwrap(),
                b"{}\n"
            );
        }
        // L3 currently uses the on-disk suffix as its full baseline once a
        // reclaimed session is appended. Preserve the precise detected gap;
        // composing retained prefixes with new suffixes is separate work.
        let report = store.reconcile_manifest(&key, &stage).unwrap();
        assert!(!report.ok());
        assert_eq!(
            report.rows[0].outcome,
            chat_stasher::verify::SessionOutcome::ShardCountMismatch {
                expected: 1,
                observed: 2,
            }
        );
    }
}

#[test]
fn pull_cli_unknown_destination_dominates_readable_absence() {
    let f = Fixture::new();
    let configs = declare_archives(&f);
    assert!(f.send().status.success());
    std::fs::remove_file(&configs[1].key_file).unwrap();
    let stage = f.owner.root().join("synthetic-stage");
    let result = pull_cli(&f, &stage);
    assert_eq!(result.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&result.stderr).contains("ArchiveRead"));
    assert_eq!(f.objects().len(), 1);
    assert!(!String::from_utf8_lossy(&result.stderr).contains(configs[1].repo_root.as_str()));
}

#[test]
fn destination_proof_binds_partition_and_full_shard_bytes_without_body_cache() {
    use chat_stasher::{inbox, remote_inbox_archive::DestinationArchive, store};
    let f = Fixture::new();
    let configs = declare_archives(&f);
    let stage = f.owner.root().join("synthetic-stage");
    assert!(f.send().status.success());
    assert_eq!(pull_cli(&f, &stage).status.code(), Some(1));
    let record = sealed_capture(&stage);
    let id = record["id"].as_str().unwrap();
    let path =
        store::sealed_shard_entries(&store::session_shard_dir(&stage, "synthetic-puller", id))
            .unwrap()[0]
            .1
            .clone();
    let original = std::fs::read(&path).unwrap();
    let outcome = inbox::SealOutcome::Duplicate(inbox::Duplicate {
        source_file: "synthetic-object".into(),
        id: id.into(),
        file_sha256: record["file_sha256"].as_str().unwrap().into(),
        matched_shard: path.file_name().unwrap().to_str().unwrap().into(),
    });
    assert!(!DestinationArchive {
        stage: &stage,
        destinations: &[]
    }
    .holds("synthetic-puller", &outcome)
    .unwrap());
    let other = f.owner.root().join("synthetic-other-stage");
    std::fs::create_dir(&other).unwrap();
    let mut other_record = record.clone();
    other_record["machine"] = "synthetic-other-puller".into();
    store::write_sealed_shard(
        store::StageWriter::Collect,
        &other,
        "synthetic-other-puller",
        id,
        &[serde_json::to_string(&other_record).unwrap()],
    )
    .unwrap();
    for cfg in &configs {
        let key = store::load_key_file(cfg).unwrap();
        store::BackupStore::new(cfg.clone(), "synthetic-other-puller".into())
            .push(&other, &key)
            .unwrap();
    }
    let proof = DestinationArchive {
        stage: &stage,
        destinations: &configs,
    };
    assert!(
        !proof.holds("synthetic-puller", &outcome).unwrap(),
        "another partition cannot prove this shard"
    );
    for cfg in &configs {
        push_archive(cfg, &stage);
    }
    assert!(proof.holds("synthetic-puller", &outcome).unwrap());
    let mut changed = record;
    changed["captured_at"] = "2026-10-08T00:00:00Z".into();
    std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(
        !proof.holds("synthetic-puller", &outcome).unwrap(),
        "a matching bundle digest alone is insufficient"
    );
    std::fs::write(&path, original).unwrap();
    assert!(proof.holds("synthetic-puller", &outcome).unwrap());
    // Metadata caching stays enabled. Destroy the actual packs after a good
    // read: neither a previous proof nor cached metadata can stand in for bytes.
    assert!(!configs[1].no_cache);
    let mut pending = vec![std::path::Path::new(&configs[1].repo_root).join("data")];
    let mut destroyed = 0;
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(std::fs::read_dir(path).unwrap().map(|e| e.unwrap().path()));
        } else {
            std::fs::write(path, b"synthetic-corrupt-pack").unwrap();
            destroyed += 1;
        }
    }
    assert!(destroyed > 0);
    assert!(proof.holds("synthetic-puller", &outcome).is_err());
    let retry = pull_cli(&f, &stage);
    assert_eq!(retry.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&retry.stderr).contains("ArchiveRead"));
    assert_eq!(f.objects().len(), 1);
}

#[test]
fn pull_cli_proves_resend_provenance_and_original_content_independently() {
    let f = Fixture::new();
    let configs = declare_archives(&f);
    let stage = f.owner.root().join("synthetic-stage");
    assert!(f.send().status.success());
    assert_eq!(pull_cli(&f, &stage).status.code(), Some(1));
    let renamed = f.producer.root().join("synthetic-renamed.jsonl");
    std::fs::write(&renamed, b"{}\n").unwrap();
    assert!(run(
        &f.producer,
        &[
            "send",
            "--path",
            renamed.to_str().unwrap(),
            "--session",
            "synthetic-native"
        ],
        Some(&f.token)
    )
    .status
    .success());
    assert_eq!(pull_cli(&f, &stage).status.code(), Some(1));
    let record = sealed_capture(&stage);
    let dir = chat_stasher::store::session_shard_dir(
        &stage,
        "synthetic-puller",
        record["id"].as_str().unwrap(),
    );
    let mut entries = chat_stasher::store::sealed_shard_entries(&dir).unwrap();
    entries.sort_by_key(|(seq, _)| *seq);
    assert_eq!(entries.len(), 2);
    let resend: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&entries[1].1).unwrap()).unwrap();
    assert_eq!(resend["kind"], "harness-resend");
    assert!(resend.get("raw").is_none());
    // Publish only provenance; the original body remains solely in stage.
    let original = std::fs::read(&entries[0].1).unwrap();
    std::fs::remove_file(&entries[0].1).unwrap();
    for cfg in &configs {
        push_archive(cfg, &stage);
    }
    std::fs::write(&entries[0].1, original).unwrap();
    let missing_content = pull_cli(&f, &stage);
    assert_eq!(missing_content.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing_content.stderr).contains("Unproven"));
    assert_eq!(f.objects().len(), 2);
    for cfg in &configs {
        push_archive(cfg, &stage);
    }
    assert_eq!(pull_cli(&f, &stage).status.code(), Some(0));
    assert!(f.objects().is_empty());
}
