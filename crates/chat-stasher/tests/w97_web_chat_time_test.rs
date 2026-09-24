//! W97 end-to-end: a browser-extension web chat capture gets a real
//! conversation time, and `search --day` / `export --since --until` select it by
//! that time.
//!
//! Every shard line here is a synthetic but real-shaped **inbox bundle**: the
//! platform's own body as a JSON string under `raw.text`, exactly as
//! `inbox::seal_payload` writes it. No real conversation text is anywhere in
//! this file — only timestamps in the platform's documented field names.
//!
//! The round trip is through the real binary and a real rustic repository:
//! `activity-index` derives the times, `push` archives them, and `overview`,
//! `search` and `export` read them back. The issue's own symptom — the
//! "conversation-time parsing is not implemented" line — is asserted gone.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// 2025-01-15T12:34:56Z and 2025-01-15T13:45:07Z (same local day in every
/// plausible test time zone).
const T1: i64 = 1_736_944_496;
const T2: i64 = 1_736_948_707;
const DAY: &str = "2025-01-15";

const WEB_SESSION: &str = "chatgpt.mbp-web.w97web0000000000000000000000001";
const LIST_SESSION: &str = "claude.mbp-web.w97list0000000000000000000000001";

fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W97 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

/// One inbox bundle line. `body` is the platform's own payload, kept as a
/// string under `raw.text`.
fn bundle_line(platform: &str, session: &str, body: &str) -> String {
    serde_json::json!({
        "schema": "chat-stasher/inbox@2",
        "platform": platform,
        "sessionId": session,
        "raw": { "text": body, "bytes": body.len() },
    })
    .to_string()
}

fn write_shard(stage: &Path, machine: &str, session: &str, lines: &[String]) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("000001.jsonl"), lines.join("\n") + "\n").unwrap();
}

/// The fixture: a ChatGPT detail body (message times only) and a claude.ai
/// list-only body (no messages), both under one machine.
fn build_stage(stage: &Path, machine: &str) {
    let chatgpt = serde_json::json!({
        "title": "synthetic",
        "mapping": {
            "n1": { "message": { "create_time": T1 } },
            "n2": { "message": { "create_time": T2 } },
        },
    })
    .to_string();
    write_shard(
        stage,
        machine,
        WEB_SESSION,
        &[bundle_line(
            "chatgpt",
            "w97web0000000000000000000000001",
            &chatgpt,
        )],
    );

    let claude = serde_json::json!({
        "uuid": "synthetic",
        "created_at": "2025-01-15T12:30:00.000Z",
        "updated_at": "2025-01-15T12:40:00.000Z",
    })
    .to_string();
    write_shard(
        stage,
        machine,
        LIST_SESSION,
        &[bundle_line(
            "claude",
            "w97list0000000000000000000000001",
            &claude,
        )],
    );
}

fn run_cmd<'a>(sb: &Path, args: impl IntoIterator<Item = &'a str>) -> Output {
    let v: Vec<&str> = args.into_iter().collect();
    run(sb, &v)
}

#[test]
fn web_chat_time_parses_and_selects_by_day_and_export() {
    let sb = tempfile::TempDir::new().unwrap();
    let stage = sb.path().join("stage");
    let machine = "mbp-web";
    build_stage(&stage, machine);

    // 1. activity-index derives message-derived time for chatgpt and
    //    list-updated time for the claude body with no messages.
    let idx = run_cmd(
        sb.path(),
        [
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(
        idx.status.success(),
        "activity-index failed: {:?}",
        idx.status
    );
    let index =
        fs::read_to_string(stage.join("meta").join(machine).join("activity-v1.jsonl")).unwrap();
    let lines: Vec<&str> = index.lines().collect();
    assert_eq!(lines.len(), 2, "one activity row per session: {index}");
    assert!(
        lines.iter().any(|l| l.contains(r#""kind":"messages""#)),
        "the chatgpt row must be message-derived:\n{index}"
    );
    assert!(
        lines.iter().any(|l| l.contains(r#""kind":"list-updated""#)),
        "the claude list-only row must be marked list-updated:\n{index}"
    );
    assert!(
        index.contains(&T1.to_string()) && index.contains(&T2.to_string()),
        "the chatgpt message times must be in the index:\n{index}"
    );

    // 2. Push, then read everything back through the real commands.
    let repo = sb.path().join("repo");
    let key = sb.path().join("keys").join("masterkey.json");
    let push = run_cmd(
        sb.path(),
        [
            "push",
            "--stage",
            stage.to_str().unwrap(),
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            machine,
            "--keep-ssh-masters",
        ],
    );
    assert!(push.status.success(), "push failed: {:?}", push.status);

    // 3. `overview` names the harness and no longer reports "not implemented".
    let ov = run_cmd(
        sb.path(),
        [
            "overview",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--width",
            "80",
            "--keep-ssh-masters",
        ],
    );
    let ov_out = String::from_utf8_lossy(&ov.stdout);
    assert_eq!(
        ov.status.code(),
        Some(0),
        "overview should exit 0:\n{ov_out}"
    );
    assert!(
        ov_out.contains("chatgpt"),
        "overview must name chatgpt:\n{ov_out}"
    );
    assert!(
        !ov_out.contains("not implemented"),
        "the web harness must not read as unimplemented:\n{ov_out}"
    );

    // 4. `search --day` selects the chatgpt conversation by its message time.
    let search = run_cmd(
        sb.path(),
        [
            "search",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--day",
            DAY,
            "--keep-ssh-masters",
        ],
    );
    let search_out = String::from_utf8_lossy(&search.stdout);
    assert_eq!(
        search.status.code(),
        Some(0),
        "search --day must find the conversation:\n{search_out}"
    );
    assert!(
        search_out.contains("chatgpt"),
        "the chatgpt session must be a match:\n{search_out}"
    );

    // A day it did not happen on selects nothing (exit 1, not 3).
    let none = run_cmd(
        sb.path(),
        [
            "search",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--day",
            "2026-01-15",
            "--keep-ssh-masters",
        ],
    );
    assert_eq!(
        none.status.code(),
        Some(1),
        "an unrelated day must select nothing:\n{}",
        String::from_utf8_lossy(&none.stdout)
    );

    // 5. `export --since --until` writes the conversation by the same time.
    let out = sb.path().join("out");
    let export = run_cmd(
        sb.path(),
        [
            "export",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--since",
            DAY,
            "--until",
            DAY,
            "--out",
            out.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    assert_eq!(
        export.status.code(),
        Some(0),
        "export --since --until must write the conversation:\n{}{}",
        String::from_utf8_lossy(&export.stdout),
        String::from_utf8_lossy(&export.stderr)
    );
    let written = out
        .join(machine)
        .join("chatgpt")
        .join(format!("{WEB_SESSION}.jsonl"));
    assert!(written.exists(), "expected {} to exist", written.display());
    let manifest = fs::read_to_string(out.join("manifest.json")).unwrap();
    assert!(
        manifest.contains(WEB_SESSION),
        "the manifest must list the chatgpt session:\n{manifest}"
    );
    let m: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    let session = m["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == WEB_SESSION)
        .expect("the chatgpt session must be in the manifest");
    assert_eq!(
        session["first_message"]["kind"], "known",
        "the chatgpt session's first message time must be known: {session}"
    );
    assert_eq!(
        session["time_source"]["kind"], "messages",
        "the chatgpt interval must be marked message-derived: {session}"
    );
    let list_only = m["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["session_id"] == LIST_SESSION)
        .expect("the claude list-only session must be in the manifest");
    assert_eq!(
        list_only["time_source"]["kind"], "list-updated",
        "the claude list-only interval must be marked list-updated: {list_only}"
    );
}
