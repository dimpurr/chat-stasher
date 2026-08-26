//! B81 — a directory scan must not present a readable subset as a complete scan.
//!
//! The inaccessible fixture is a directory rather than a chmod'd regular file:
//! metadata-only scanning can still stat a file whose read bit is absent, while
//! a directory with no search permission makes the failed `read_dir` path real.
//! The blocked directory contains two fixture names, but the scanner must report
//! the session cardinality as unknown rather than guessing that one entry means
//! one session.
//!
//! Every CLI invocation uses a temporary HOME/XDG tree and a scratch registry.
//! The test writes fixture bodies but never reads them.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::json;
use sha2::{Digest, Sha256};

const CLEAN_STATUS_BODY_SHA256: &str =
    "496303cf8515a15ccd016f5d9dfd4ef5563c1251f057cc9e75d0e4bec17e4461";

fn write_registry(sandbox: &Path, root: &Path) -> PathBuf {
    let cell = json!({
        "template": root,
        "format": "jsonl",
        "session_pattern": "*.jsonl",
        "confidence": "本机实测",
        "source": "B81 test fixture"
    });
    let registry = json!({
        "schema_version": 1,
        "generated": "B81",
        "harnesses": [{
            "id": "claude-code",
            "display_name": "Claude Code",
            "paths": {"macos": cell, "linux": cell, "windows": cell}
        }]
    });
    let path = sandbox.join("registry.json");
    let bytes = serde_json::to_vec(&registry).expect("serialize scratch registry");
    fs::write(&path, bytes).expect("write scratch registry");
    path
}

fn run_cli_args(sandbox: &Path, registry: &Path, args: &[String]) -> Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).expect("create isolated home");
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("CHAT_STASHER_REGISTRY", registry)
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .output()
        .expect("run chat-stasher command")
}

fn run_cli(sandbox: &Path, registry: &Path, command: &str) -> Output {
    run_cli_args(sandbox, registry, &[command.to_string()])
}

fn run_collect(sandbox: &Path, registry: &Path) -> Output {
    run_cli_args(
        sandbox,
        registry,
        &[
            "collect".to_string(),
            "--stage".to_string(),
            sandbox.join("stage").display().to_string(),
            "--machine".to_string(),
            "b81-partialscan".to_string(),
        ],
    )
}

fn status_body(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.starts_with("[run-once]"))
        .map(|line| format!("{line}\n"))
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn plant_clean_fixture(sandbox: &Path) -> (PathBuf, PathBuf) {
    let root = sandbox.join("source");
    fs::create_dir_all(&root).expect("create clean source root");
    fs::write(root.join("session.jsonl"), b"fixture\n").expect("write clean session fixture");
    let registry = write_registry(sandbox, &root);
    (root, registry)
}

#[cfg(unix)]
fn plant_partial_fixture(sandbox: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let root = sandbox.join("source");
    let blocked = root.join("blocked");
    fs::create_dir_all(&blocked).expect("create partial source root");
    fs::write(root.join("readable.jsonl"), b"fixture\n").expect("write readable session fixture");
    fs::write(blocked.join("hidden-a.jsonl"), b"fixture\n").expect("write hidden fixture a");
    fs::write(blocked.join("hidden-b.jsonl"), b"fixture\n").expect("write hidden fixture b");

    let mut permissions = fs::metadata(&blocked)
        .expect("stat blocked directory")
        .permissions();
    permissions.set_mode(0o0);
    fs::set_permissions(&blocked, permissions).expect("remove blocked directory permissions");

    let registry = write_registry(sandbox, &root);
    (root, registry, blocked)
}

#[cfg(unix)]
#[test]
fn partial_directory_scan_is_reported_at_entry_granularity() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let (_root, registry, blocked) = plant_partial_fixture(sandbox.path());
    let status = run_cli(sandbox.path(), &registry, "status");
    let doctor = run_cli(sandbox.path(), &registry, "doctor");
    let collect = run_collect(sandbox.path(), &registry);

    let mut permissions = fs::metadata(&blocked)
        .expect("restore blocked directory permissions")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&blocked, permissions).expect("restore blocked directory permissions");

    let status_text = status_body(&status);
    let doctor_text = String::from_utf8_lossy(&doctor.stderr);
    let collect_text = format!(
        "{}{}",
        String::from_utf8_lossy(&collect.stdout),
        String::from_utf8_lossy(&collect.stderr)
    );
    assert_eq!(status.status.code(), Some(1));
    assert_eq!(doctor.status.code(), Some(0));
    assert_eq!(collect.status.code(), Some(0));
    assert!(
        status_text.contains("unreadable directory item")
            && status_text.contains("session count unknown"),
        "status must expose partial directory coverage: {status_text}"
    );
    assert!(
        doctor_text.contains("unreadable directory")
            && doctor_text.contains("session count unknown"),
        "doctor must expose partial directory coverage: {doctor_text}"
    );
    assert!(
        collect_text.contains("scan partial") && collect_text.contains("unreadable_entries=1"),
        "collect must expose partial directory coverage without blocking the pass: {collect_text}"
    );
}

#[test]
fn all_readable_directory_scan_keeps_status_bytes_identical() {
    let sandbox = tempfile::tempdir().expect("create sandbox");
    let (_root, registry) = plant_clean_fixture(sandbox.path());
    let output = run_cli(sandbox.path(), &registry, "status");
    let body = status_body(&output);

    assert_eq!(output.status.code(), Some(1));
    assert!(!body.contains("unreadable directory"));
    assert!(!body.contains("cannot read"));
    assert_eq!(
        sha256_hex(body.as_bytes()),
        CLEAN_STATUS_BODY_SHA256,
        "clean directory scan changed status bytes: {body}"
    );
}
