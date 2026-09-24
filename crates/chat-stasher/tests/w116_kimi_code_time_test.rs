//! W116 end-to-end: a `kimi-code` session gets a real conversation time, and a
//! `kimi-code` session that holds no conversation lands in the ADR-035
//! "no conversation content" state instead of the time-unknown tally.
//!
//! Every shard line here is a synthetic but real-shaped Kimi Code wire record:
//! `{type, time, …}` with `time` in **epoch milliseconds**, exactly as the
//! journal's writer stamps it, and an opening `metadata` record carrying
//! `created_at` instead. The record vocabulary and the millisecond unit were
//! measured on the installed Kimi Code 0.39.1 implementation on 2026-09-25 and
//! cross-checked against 41 real `wire.jsonl` files
//! (<https://github.com/MemPalace/mempalace/issues/2180>). No real conversation
//! text is anywhere in this file.
//!
//! The round trip is through the real binary and a real rustic repository:
//! `activity-index` derives the times, `push` archives them, and `overview` and
//! `search` read them back. The issue's own symptom — the "conversation-time
//! parsing is not implemented for this harness (kimi-code)" line — is asserted
//! gone.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// 2025-01-15T12:34:56Z and 2025-01-15T13:45:07Z (same local day in every
/// plausible test time zone).
const T1: i64 = 1_736_944_496;
const T2: i64 = 1_736_948_707;
const DAY: &str = "2025-01-15";

/// A session with a real conversation: the user message and the assistant's
/// loop step carry the span.
const TALKING_SESSION: &str = "kimi-code.mbp-kimi.w116talk00000000000000000000001";
/// A session that holds no conversation at all — the composition measured on
/// this machine's three real sessions: an opening metadata record plus config
/// and MCP tool-discovery records only.
const SILENT_SESSION: &str = "kimi-code.mbp-kimi.w116quiet0000000000000000000001";

fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W116 synthetic","harnesses":[]}"#,
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

fn run_cmd<'a>(sb: &Path, args: impl IntoIterator<Item = &'a str>) -> Output {
    let v: Vec<&str> = args.into_iter().collect();
    run(sb, &v)
}

/// The journal's first line: no `time`, only `created_at`.
fn metadata(ms: i64) -> String {
    format!(r#"{{"type":"metadata","protocol_version":"1.4","created_at":{ms}}}"#)
}

/// A state-rebuilding record: no conversation, but timestamped all the same.
fn bookkeeping(ty: &str, ms: i64) -> String {
    format!(r#"{{"type":"{ty}","time":{ms}}}"#)
}

/// A user message record.
fn user_message(ms: i64) -> String {
    format!(
        r#"{{"type":"context.append_message","time":{ms},"message":{{"role":"user","origin":{{"kind":"user"}},"content":[{{"type":"text","text":"synthetic"}}]}}}}"#
    )
}

/// An assistant loop step, where the assistant's output actually lives.
fn loop_event(ms: i64, event: &str) -> String {
    format!(r#"{{"type":"context.append_loop_event","time":{ms},"event":{{"type":"{event}"}}}}"#)
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

fn build_stage(stage: &Path, machine: &str) {
    write_shard(
        stage,
        machine,
        TALKING_SESSION,
        &[
            metadata(T1 * 1000 - 5),
            bookkeeping("config.update", T1 * 1000 - 3),
            user_message(T1 * 1000),
            loop_event(T2 * 1000, "step.begin"),
        ],
    );
    write_shard(
        stage,
        machine,
        SILENT_SESSION,
        &[
            metadata(T1 * 1000),
            bookkeeping("config.update", T1 * 1000),
            bookkeeping("mcp.tools_discovered", T1 * 1000 + 164),
            bookkeeping("tools.set_active_tools", T1 * 1000 + 257),
        ],
    );
}

#[test]
fn kimi_code_time_parses_and_selects_by_day() {
    let sb = tempfile::TempDir::new().unwrap();
    let stage = sb.path().join("stage");
    let machine = "mbp-kimi";
    build_stage(&stage, machine);

    // 1. `activity-index` gives the conversation session the millisecond
    //    timestamps its own records carry, and calls the config-only session
    //    "no conversation content" rather than an unknown time.
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

    let talking = lines
        .iter()
        .find(|l| l.contains("w116talk"))
        .unwrap_or_else(|| panic!("the talking session must have a row:\n{index}"));
    assert!(
        talking.contains(&format!(r#""first_unix":{T1}"#))
            && talking.contains(&format!(r#""last_unix":{T2}"#)),
        "the message times (in millis) must be the session span:\n{talking}"
    );
    assert!(
        talking.contains(r#""kind":"inferred""#),
        "an epoch-millis time is an inferred unit, never exact:\n{talking}"
    );
    assert!(
        talking.contains("millis"),
        "the inference must say which unit it inferred:\n{talking}"
    );
    assert!(
        !talking.contains("not implemented"),
        "the CLI harness must not read as unimplemented:\n{talking}"
    );

    let silent = lines
        .iter()
        .find(|l| l.contains("w116quiet"))
        .unwrap_or_else(|| panic!("the config-only session must have a row:\n{index}"));
    assert!(
        silent.contains(r#""kind":"no_conversation_content""#),
        "a session whose every record is bookkeeping holds no conversation:\n{silent}"
    );
    assert!(
        silent.contains(r#""first_unix":null"#) && silent.contains(r#""last_unix":null"#),
        "the metadata record's created_at must not become the session time:\n{silent}"
    );
    assert!(
        silent.contains(r#""line_count":4"#),
        "the size of the session is still counted:\n{silent}"
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

    // 3. `overview` names the harness, and the config-only session is not
    //    counted as a time-unknown session (ADR-035 class B).
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
        ov_out.contains("kimi-code"),
        "overview must name kimi-code:\n{ov_out}"
    );
    assert!(
        !ov_out.contains("not implemented"),
        "kimi-code must not read as unimplemented:\n{ov_out}"
    );

    let ov_json = run_cmd(
        sb.path(),
        [
            "overview",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--json",
            "--keep-ssh-masters",
        ],
    );
    let ov_json_out = String::from_utf8_lossy(&ov_json.stdout);
    assert_eq!(
        ov_json.status.code(),
        Some(0),
        "overview --json should exit 0:\n{ov_json_out}"
    );
    // The JSON document is the first line; later lines carry the reap note.
    let doc = ov_json_out.lines().next().unwrap_or_default();
    let value: serde_json::Value =
        serde_json::from_str(doc).unwrap_or_else(|e| panic!("not JSON ({e}):\n{ov_json_out}"));
    assert_eq!(
        value["summary"]["sessions"], 2,
        "both sessions are in view:\n{ov_json_out}"
    );
    assert_eq!(
        value["summary"]["unknown_time_sessions"], 0,
        "no session here has an unknown conversation time — the config-only one holds no \
         conversation, which is a different claim (ADR-035):\n{ov_json_out}"
    );
    assert_eq!(
        value["summary"]["no_conversation_content_sessions"], 1,
        "the config-only session is counted in its own bucket, not as time-unknown:\n{ov_json_out}"
    );

    // 4. `search --day` selects the conversation by the time its own records
    //    carry.
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
        "search --day must find the kimi-code conversation:\n{search_out}"
    );
    assert!(
        search_out.contains("kimi-code"),
        "the kimi-code session must be a match:\n{search_out}"
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
}
