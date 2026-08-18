//! B91 — time source and configuration provenance regression tests.
//!
//! These tests are intentionally written against the old implementation first:
//! the time-label and config-provenance assertions must be red before the fix.
//! Every CLI run uses only paths below a `tempfile` directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn isolated_env(sandbox: &Path, args: &[&str], registry: &Path) -> Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("XDG_CACHE_HOME", sandbox.join("xdg-cache"))
        .env("CHAT_STASHER_REGISTRY", registry)
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .env_remove("CURSOR_USER_DIR")
        .output()
        .unwrap()
}

fn registry_for_empty_fixture(sandbox: &Path) -> PathBuf {
    let root = sandbox.join("empty-source");
    fs::create_dir_all(&root).unwrap();
    let registry = sandbox.join("registry.json");
    fs::write(
        &registry,
        format!(
            r#"{{"schema_version":1,"generated":"B91","harnesses":[{{"id":"claude-code","display_name":"Claude Code","paths":{{"macos":{{"template":{},"format":"jsonl","session_pattern":"*.jsonl","confidence":"本机实测","source":"B91"}},"linux":{{"template":{},"format":"jsonl","session_pattern":"*.jsonl","confidence":"本机实测","source":"B91"}},"windows":{{"template":{},"format":"jsonl","session_pattern":"*.jsonl","confidence":"本机实测","source":"B91"}}}}}}]}}"#,
            serde_json::to_string(&root).unwrap(),
            serde_json::to_string(&root).unwrap(),
            serde_json::to_string(&root).unwrap(),
        ),
    )
    .unwrap();
    registry
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A's old behaviour counterexample: the public query is described as a
/// session-activity filter even though the metadata available to it is the
/// archive snapshot/tree time.
#[test]
fn search_time_window_is_explicitly_named_archive_time() {
    let sandbox = tempfile::tempdir().unwrap();
    let registry = registry_for_empty_fixture(sandbox.path());
    let out = isolated_env(sandbox.path(), &["search", "--help"], &registry);
    let text = combined(&out);
    assert!(
        text.contains("archive time") || text.contains("归档时间"),
        "time-window help must identify archive time, not session activity time:\n{text}"
    );
}

/// B's old behaviour counterexample: malformed config warns on stderr but
/// status/doctor expose no machine-readable fact that defaults were used.
#[test]
fn config_parse_fallback_is_visible_to_status_and_doctor() {
    let sandbox = tempfile::tempdir().unwrap();
    let registry = registry_for_empty_fixture(sandbox.path());
    let config = sandbox.path().join("xdg-config/chat-stasher/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "this is not valid TOML = [\n").unwrap();

    for command in ["status", "doctor"] {
        let out = isolated_env(sandbox.path(), &[command], &registry);
        let text = combined(&out);
        assert!(
            text.contains("config_source=defaults_after_parse_error"),
            "{command} must expose that defaults came from a config parse failure;\n{text}"
        );
    }
}

/// Health guard: a fully inspectable empty fixture must not acquire a cloud of
/// new `未知` labels merely because byte provenance became explicit.
#[test]
fn healthy_fixture_status_and_doctor_unknown_counts_are_stable() {
    let sandbox = tempfile::tempdir().unwrap();
    let registry = registry_for_empty_fixture(sandbox.path());
    let status = isolated_env(sandbox.path(), &["status"], &registry);
    let doctor = isolated_env(sandbox.path(), &["doctor"], &registry);
    let status_text = combined(&status);
    let doctor_text = combined(&doctor);
    let status_unknown = status_text.matches("未知").count();
    let doctor_unknown = doctor_text.matches("未知").count();
    println!(
        "B91 healthy fixture: status_unknown={status_unknown} doctor_unknown={doctor_unknown}"
    );
    println!("B91 status:\n{status_text}");
    println!("B91 doctor:\n{doctor_text}");
    assert_eq!(status_unknown, 0, "healthy status grew an unexplained 未知");
    assert_eq!(
        doctor_unknown, 1,
        "healthy doctor must retain exactly its existing unknown earliest-session risk"
    );
    assert!(doctor_text.contains("最早会话时间未知"));
}
