//! Named remote inbox initialization, using only isolated synthetic locators.
#[path = "../src/test_support.rs"]
mod test_support;
use std::process::Command;
use test_support::Sandbox;

fn init(sandbox: &Sandbox, name: &str, locator: &str) -> std::process::Output {
    sandbox
        .apply(&mut Command::new(env!("CARGO_BIN_EXE_chat-stasher")))
        .args(["inbox-init", name, "--locator", locator])
        .output()
        .unwrap()
}
fn record(sandbox: &Sandbox, name: &str) -> std::path::PathBuf {
    sandbox
        .config_home()
        .join("chat-stasher/inboxes")
        .join(format!("{name}.json"))
}
#[test]
fn initialization_persists_one_recipient_without_archive_or_machine_identity() {
    let sandbox = Sandbox::new();
    let result = init(&sandbox, "synthetic-inbox", "memory://synthetic-inbox");
    assert!(result.status.success());
    let bytes = std::fs::read(record(&sandbox, "synthetic-inbox")).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["schema"], "chat-stasher/remote-inbox-config@1");
    assert_eq!(value["locator"], "memory://synthetic-inbox");
    let identity: age::x25519::Identity = value["identity"].as_str().unwrap().parse().unwrap();
    assert_eq!(value["recipient"], identity.to_public().to_string());
    assert!(!String::from_utf8_lossy(&result.stdout).contains("AGE-SECRET-KEY"));
    assert!(!sandbox.data_home().exists());
    assert!(!sandbox
        .config_home()
        .join("chat-stasher/config.toml")
        .exists());
    assert!(
        init(&sandbox, "synthetic-inbox", "memory://synthetic-inbox")
            .status
            .success()
    );
    assert_eq!(
        std::fs::read(record(&sandbox, "synthetic-inbox")).unwrap(),
        bytes
    );
    assert!(!init(&sandbox, "synthetic-inbox", "memory://other")
        .status
        .success());
    assert_eq!(
        std::fs::read(record(&sandbox, "synthetic-inbox")).unwrap(),
        bytes
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(record(&sandbox, "synthetic-inbox"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
#[test]
fn invalid_names_and_corrupt_records_are_refused_without_replacement() {
    let sandbox = Sandbox::new();
    for name in ["../escape", "", "a/b"] {
        assert!(!init(&sandbox, name, "memory://synthetic-inbox")
            .status
            .success());
    }
    assert!(!sandbox.config_home().exists());
    assert!(
        init(&sandbox, "synthetic-inbox", "memory://synthetic-inbox")
            .status
            .success()
    );
    let path = record(&sandbox, "synthetic-inbox");
    std::fs::write(&path, b"synthetic-corrupt-secret").unwrap();
    let result = init(&sandbox, "synthetic-inbox", "memory://synthetic-inbox");
    assert!(!result.status.success());
    assert!(!String::from_utf8_lossy(&result.stderr).contains("synthetic-corrupt-secret"));
    assert_eq!(std::fs::read(path).unwrap(), b"synthetic-corrupt-secret");
}
#[test]
fn concurrent_initialization_keeps_the_same_complete_record() {
    let sandbox = Sandbox::new();
    std::thread::scope(|scope| {
        let runs: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| init(&sandbox, "synthetic-inbox", "memory://synthetic-inbox")))
            .collect();
        for run in runs {
            assert!(run.join().unwrap().status.success());
        }
    });
    let dir = record(&sandbox, "synthetic-inbox")
        .parent()
        .unwrap()
        .to_path_buf();
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1);
}
#[test]
fn recipient_mismatch_and_unknown_version_are_not_silently_reinitialized() {
    for field in ["recipient", "schema"] {
        let sandbox = Sandbox::new();
        assert!(
            init(&sandbox, "synthetic-inbox", "memory://synthetic-inbox")
                .status
                .success()
        );
        let path = record(&sandbox, "synthetic-inbox");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value[field] = "synthetic-invalid".into();
        let bytes = serde_json::to_vec(&value).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            !init(&sandbox, "synthetic-inbox", "memory://synthetic-inbox")
                .status
                .success()
        );
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
}
#[test]
fn credential_bearing_or_empty_locators_are_usage_errors() {
    let sandbox = Sandbox::new();
    for locator in [
        "",
        "s3://synthetic:secret@bucket",
        "s3://bucket?secret=synthetic",
        "s3://bucket#synthetic",
        "not-a-locator",
    ] {
        let result = init(&sandbox, "synthetic-inbox", locator);
        assert_eq!(result.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&result.stderr).contains(locator) || locator.is_empty());
    }
    assert!(!sandbox.config_home().exists());
}
#[cfg(unix)]
#[test]
fn symlink_and_non_private_records_are_refused() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let sandbox = Sandbox::new();
    assert!(
        init(&sandbox, "synthetic-inbox", "memory://synthetic-inbox")
            .status
            .success()
    );
    let path = record(&sandbox, "synthetic-inbox");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        !init(&sandbox, "synthetic-inbox", "memory://synthetic-inbox")
            .status
            .success()
    );
    let target = sandbox.root().join("synthetic-target");
    std::fs::rename(&path, &target).unwrap();
    symlink(&target, &path).unwrap();
    assert!(
        !init(&sandbox, "synthetic-inbox", "memory://synthetic-inbox")
            .status
            .success()
    );
    assert!(std::fs::symlink_metadata(path)
        .unwrap()
        .file_type()
        .is_symlink());
}
#[cfg(unix)]
#[test]
fn symlinked_inbox_directory_is_refused_even_with_an_existing_record() {
    use std::os::unix::fs::symlink;
    let sandbox = Sandbox::new();
    assert!(
        init(&sandbox, "synthetic-inbox", "memory://synthetic-inbox")
            .status
            .success()
    );
    let root = record(&sandbox, "synthetic-inbox")
        .parent()
        .unwrap()
        .to_path_buf();
    let target = sandbox.root().join("synthetic-inbox-directory");
    std::fs::rename(&root, &target).unwrap();
    symlink(&target, &root).unwrap();
    assert!(
        !init(&sandbox, "synthetic-inbox", "memory://synthetic-inbox")
            .status
            .success()
    );
}
