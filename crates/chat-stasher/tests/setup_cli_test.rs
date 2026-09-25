use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn non_tty_setup_emits_json_missing_parameters_and_does_not_echo_stdin() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .arg("setup")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"synthetic-secret-input")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["command"], "setup");
    assert_eq!(value["missing_parameters"], serde_json::json!(["stage"]));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-secret-input"));
}

#[test]
fn non_tty_setup_accepts_named_choices_and_reports_stub_states() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args([
            "setup",
            "--stage",
            "/fixture/stage",
            "--destination",
            "archive",
            "--install-schedule",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["missing_parameters"], serde_json::json!([]));
    assert_eq!(value["steps"]["stage"], "provided");
    assert_eq!(value["steps"]["destination"], "planned");
    assert_eq!(value["steps"]["schedule"], "planned");
}

#[test]
fn non_tty_setup_refuses_invalid_config_instead_of_scanning_defaults() {
    let home = tempfile::tempdir().unwrap();
    let config_home = home.path().join("config");
    let config_dir = config_home.join("chat-stasher");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "this is not valid TOML = [").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(["setup", "--stage", "/fixture/stage"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", &config_home)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(3));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["command"], "setup");
    assert_eq!(value["healthy"], false);
    assert_eq!(value["exit_code"], 3);
    assert_eq!(value["scanner"]["kind"], "failed");
    assert!(value["scanner"]["why"]
        .as_str()
        .unwrap()
        .contains("not valid TOML"));
}

#[test]
fn setup_and_status_commands_emit_the_same_scan_json() {
    let home = tempfile::tempdir().unwrap();
    let config_home = home.path().join("config");
    let setup = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(["setup", "--stage", "/fixture/stage"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", &config_home)
        .output()
        .unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", &config_home)
        .output()
        .unwrap();

    assert_eq!(setup.status.code(), Some(0));
    let setup: serde_json::Value = serde_json::from_slice(&setup.stdout).unwrap();
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let mut status_scan = status["scanner"].clone();
    status_scan
        .as_object_mut()
        .unwrap()
        .remove("writer_versions");
    assert_eq!(setup["scanner"], status_scan);
}
