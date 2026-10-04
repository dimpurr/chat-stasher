//! W478: a cache path the doctor cannot use is unknown and actionable, never
//! a measured zero. Both reports run against isolated synthetic paths.

#[path = "../src/test_support.rs"]
mod test_support;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn doctor(sandbox: &Path, json: bool) -> Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    let registry = sandbox.join("registry.json");
    fs::write(
        &registry,
        br#"{"schema_version":1,"generated":"W478 synthetic","harnesses":[]}"#,
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command.arg("doctor");
    if json {
        command.arg("--json");
    }
    command
        .current_dir(sandbox)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("xdg-cache"))
        .env("CHAT_STASHER_REGISTRY", registry)
        .env_remove(test_support::RUSTIC_CACHE_DIR_ENV)
        .env_remove("CODEX_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("OPENCODE_DB")
        .env_remove("CURSOR_USER_DIR")
        .output()
        .unwrap()
}

fn write_config(sandbox: &Path, value: &str) {
    let path = sandbox.join("config/chat-stasher/config.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("rustic_cache_dir = {value:?}\n")).unwrap();
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn non_directory_cache_path_is_unknown_in_human_and_json_reports() {
    let sandbox = tempfile::tempdir().unwrap();
    let cache_path = sandbox.path().join("synthetic-cache-file");
    fs::write(&cache_path, b"synthetic fixture").unwrap();
    write_config(sandbox.path(), &cache_path.to_string_lossy());

    let human = doctor(sandbox.path(), false);
    assert_eq!(human.status.code(), Some(0), "{}", combined(&human));
    let human_text = combined(&human);
    assert!(human_text.contains("cache root:"), "{human_text}");
    assert!(human_text.contains("could not be measured"), "{human_text}");
    assert!(human_text.contains("readable directory"), "{human_text}");
    assert!(!human_text.contains("disk used  : 0 B"), "{human_text}");

    let json = doctor(sandbox.path(), true);
    assert_eq!(json.status.code(), Some(0), "{}", combined(&json));
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["cache"]["kind"], "unreadable", "{value}");
    assert_eq!(value["cache"]["total_bytes"]["kind"], "unknown", "{value}");
    assert!(value["cache"]["total_bytes"]["why"]
        .as_str()
        .is_some_and(|why| !why.is_empty()));
    assert!(value["cache"]["next_step"]
        .as_str()
        .unwrap()
        .contains("readable directory"));
    assert_eq!(fs::read(&cache_path).unwrap(), b"synthetic fixture");
}

#[test]
fn unexpandable_cache_path_is_unavailable_in_human_and_json_reports() {
    let sandbox = tempfile::tempdir().unwrap();
    write_config(sandbox.path(), "~synthetic-w478-user/cache");

    let human = doctor(sandbox.path(), false);
    assert_eq!(human.status.code(), Some(3), "{}", combined(&human));
    let human_text = combined(&human);
    assert!(
        human_text.contains("cache-path check: unavailable"),
        "{human_text}"
    );
    assert!(
        human_text.contains("fix that value in the config"),
        "{human_text}"
    );

    let json = doctor(sandbox.path(), true);
    assert_eq!(json.status.code(), Some(3), "{}", combined(&json));
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["cache"]["kind"], "unavailable", "{value}");
    assert_eq!(value["cache"]["total_bytes"]["kind"], "unknown", "{value}");
    assert!(value["cache"]["next_step"]
        .as_str()
        .unwrap()
        .contains("rustic_cache_dir"));
    assert!(value["config_error"]
        .as_str()
        .unwrap()
        .contains("rustic_cache_dir"));
    assert!(
        !sandbox.path().join("~synthetic-w478-user").exists(),
        "doctor must not create the unexpanded synthetic cache path"
    );
}
