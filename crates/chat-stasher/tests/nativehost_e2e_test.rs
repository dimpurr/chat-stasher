//! Native Messaging host, end to end (`contracts/nativehost-protocol.md`).
//!
//! Every test here starts the **real binary** in a browser's argv shape and
//! hands it real frames on stdin. The process boundary is deliberate: the
//! properties that matter are about what a browser observes — the exact bytes
//! on stdout, the exit status, whether the frame was answered at all — and none
//! of them can be asserted by calling a function.
//!
//! Isolation: `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME` and
//! `XDG_CACHE_HOME` all point inside one `tempfile` directory, so no test reads
//! or writes a real config, a real stage, a real identity file or a real
//! browser directory.
//!
//! Schema discipline: every response is validated against
//! `contracts/nativehost-message.schema.json` — the committed file, read at test
//! time — rather than against a hand-copied list of field names. The validator
//! below understands the subset of JSON Schema that file actually uses, and
//! **panics on any construct it does not understand** rather than letting it
//! pass: a schema that grows a keyword the test cannot check must show up as a
//! red test, not as a silent hole.

use chat_stasher::nativehost::IDENTITY_SEQ_WINDOW;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

#[path = "../src/test_support.rs"]
mod test_support;

const CHROME_ORIGIN: &str = "chrome-extension://gihmdkkmmmkeiagjjiimacmgkdilofhi/";
const FIREFOX_ID: &str = "chat-stasher@team.iopho.com";
const FOREIGN_ORIGIN: &str = "chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/";

// --------------------------------------------------------------- schema check

fn schema() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("contracts")
        .join("nativehost-message.schema.json");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).expect("the committed schema is valid JSON")
}

/// Evaluate one schema fragment against one value.
fn check_in(root: &Value, fragment: &Value, actual: &Value) -> Result<(), String> {
    if let Some(reference) = fragment.get("$ref").and_then(Value::as_str) {
        let name = reference
            .strip_prefix("#/$defs/")
            .ok_or_else(|| format!("unsupported $ref {reference}"))?;
        let def = root["$defs"]
            .get(name)
            .ok_or_else(|| format!("no $defs/{name}"))?;
        return check_in(root, def, actual);
    }

    if let Some(branches) = fragment.get("oneOf").and_then(Value::as_array) {
        let mut reasons = Vec::new();
        for branch in branches {
            match check_in(root, branch, actual) {
                Ok(()) => return Ok(()),
                Err(reason) => reasons.push(reason),
            }
        }
        return Err(format!(
            "matches none of the oneOf branches [{}]",
            reasons.join(" | ")
        ));
    }

    if let Some(constant) = fragment.get("const") {
        if actual != constant {
            return Err(format!("expected const {constant}, got {actual}"));
        }
    }
    if let Some(values) = fragment.get("enum").and_then(Value::as_array) {
        if !values.contains(actual) {
            return Err(format!("{actual} is not one of {values:?}"));
        }
    }
    if let Some(kind) = fragment.get("type").and_then(Value::as_str) {
        let ok = match kind {
            "object" => actual.is_object(),
            "array" => actual.is_array(),
            "string" => actual.is_string(),
            "boolean" => actual.is_boolean(),
            "null" => actual.is_null(),
            "integer" => actual.is_i64() || actual.is_u64(),
            other => return Err(format!("unsupported schema type {other}")),
        };
        if !ok {
            return Err(format!("expected type {kind}, got {actual}"));
        }
    }
    if let (Some(pattern), Some(text)) = (
        fragment.get("pattern").and_then(Value::as_str),
        actual.as_str(),
    ) {
        if !pattern_matches(pattern, text) {
            return Err(format!(
                "{text:?} does not satisfy the schema pattern {pattern}"
            ));
        }
    }
    if let (Some(max), Some(text)) = (
        fragment.get("maxLength").and_then(Value::as_u64),
        actual.as_str(),
    ) {
        if text.len() as u64 > max {
            return Err(format!("length {} exceeds maxLength {max}", text.len()));
        }
    }
    if let (Some(items), Some(array)) = (fragment.get("items"), actual.as_array()) {
        for (index, element) in array.iter().enumerate() {
            check_in(root, items, element).map_err(|e| format!("[{index}]: {e}"))?;
        }
    }
    if let Some(properties) = fragment.get("properties").and_then(Value::as_object) {
        let object = actual
            .as_object()
            .ok_or_else(|| format!("expected an object, got {actual}"))?;
        if let Some(required) = fragment.get("required").and_then(Value::as_array) {
            for field in required {
                let field = field.as_str().ok_or("a `required` entry is not a string")?;
                if !object.contains_key(field) {
                    return Err(format!("missing required field `{field}` in {actual}"));
                }
            }
        }
        if fragment.get("additionalProperties") == Some(&Value::Bool(false)) {
            for key in object.keys() {
                if !properties.contains_key(key) {
                    return Err(format!(
                        "unexpected field `{key}`: the schema sets additionalProperties: false"
                    ));
                }
            }
        }
        for (key, sub) in properties {
            if let Some(value) = object.get(key) {
                check_in(root, sub, value).map_err(|e| format!("{key}: {e}"))?;
            }
        }
    }
    Ok(())
}

/// The schema's three `pattern`s, evaluated by hand. There is no regex crate in
/// this build, and this is the honest alternative: name every pattern the file
/// contains, and refuse to guess at a fourth.
fn pattern_matches(pattern: &str, text: &str) -> bool {
    match pattern {
        // $defs/requestId
        "^[A-Za-z0-9_-]{1,128}$" => {
            !text.is_empty()
                && text.len() <= 128
                && text
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }
        // $defs/sha256
        "^[0-9a-f]{64}$" => {
            text.len() == 64
                && text
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        // deliverRequest.name
        "^[a-z0-9]+-[^/\\\\]+\\.json$" => {
            match text
                .strip_suffix(".json")
                .and_then(|stem| stem.find('-').map(|dash| stem.split_at(dash)))
            {
                Some((head, tail)) => {
                    let tail = &tail[1..];
                    !head.is_empty()
                        && head
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                        && !tail.is_empty()
                        && !tail.contains('/')
                        && !tail.contains('\\')
                }
                None => false,
            }
        }
        // dashboardResponse.url (§6.5): loopback, a port, and a 64-hex token.
        "^http://127\\.0\\.0\\.1:[0-9]{1,5}/\\?token=[0-9a-f]{64}$" => {
            match text.strip_prefix("http://127.0.0.1:") {
                Some(rest) => match rest.split_once("/?token=") {
                    Some((port, token)) => {
                        !port.is_empty()
                            && port.len() <= 5
                            && port.bytes().all(|b| b.is_ascii_digit())
                            && token.len() == 64
                            && token
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    }
                    None => false,
                },
                None => false,
            }
        }
        other => panic!(
            "the schema contains a pattern this test cannot evaluate ({other}); \
             teach `pattern_matches` about it rather than letting it pass unverified"
        ),
    }
}

/// Assert a value satisfies the whole committed schema, and that the top level
/// is still the `oneOf` over the message shapes the protocol defines.
#[track_caller]
fn assert_matches_schema(value: &Value) {
    let root = schema();
    assert!(
        root.get("oneOf").is_some(),
        "the schema no longer has a top-level oneOf; this test must be updated"
    );
    if let Err(reason) = check_in(&root, &root["oneOf"], value) {
        panic!(
            "response does not match contracts/nativehost-message.schema.json: {reason}\n{value}"
        );
    }
}

// ------------------------------------------------------------------- fixture

struct Fixture {
    dir: tempfile::TempDir,
    home: PathBuf,
    stage: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path().join("home");
        let stage = dir.path().join("stage");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&stage).expect("stage");
        // A machine that has already run the CLI has an identity. The host
        // itself never creates one (see `nativehost::resolve_machine`), so
        // seed it the same way the CLI does.
        chat_stasher::identity::load_or_create(
            &home
                .join("data")
                .join("chat-stasher")
                .join("machine-identity"),
        )
        .expect("seed machine identity");
        Fixture { dir, home, stage }
    }

    fn config_path(&self) -> PathBuf {
        self.home
            .join("config")
            .join("chat-stasher")
            .join("config.toml")
    }

    fn write_config(&self, text: &str) {
        let path = self.config_path();
        fs::create_dir_all(path.parent().expect("config dir")).expect("mkdir");
        fs::write(&path, text).expect("write config");
    }

    /// Point `[native_host] stage` at this fixture's stage.
    fn configure_stage(&self) {
        self.write_config(&format!(
            "[native_host]\nstage = {}\n",
            serde_json::to_string(&self.stage.to_string_lossy()).expect("path as TOML string")
        ));
    }

    /// Configure the real host with an isolated synthetic archive masterkey.
    fn configure_stage_with_masterkey(&self) {
        let key_file = self.home.join("masterkey.json");
        let masterkey = rustic_core::repofile::MasterKey::new();
        fs::write(
            &key_file,
            chat_stasher::store::serialize_key(&masterkey).expect("serialize synthetic key"),
        )
        .expect("write synthetic masterkey");
        self.write_config(&format!(
            "rustic_key_file = {}\n[native_host]\nstage = {}\n",
            serde_json::to_string(&key_file.to_string_lossy()).expect("key path as TOML string"),
            serde_json::to_string(&self.stage.to_string_lossy())
                .expect("stage path as TOML string")
        ));
    }

    /// Run the binary with an exact argv, isolated from every real directory.
    fn run(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
            .args(args)
            .env("HOME", &self.home)
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&self.home),
            )
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_STATE_HOME", self.home.join("state"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn chat-stasher");
        child
            .stdin
            .take()
            .expect("stdin pipe")
            .write_all(stdin)
            .expect("write the request frame");
        child.wait_with_output().expect("wait for chat-stasher")
    }

    /// The Chrome launch shape: one argument, the extension origin.
    fn chrome(&self, stdin: &[u8]) -> Output {
        self.run(&[CHROME_ORIGIN], stdin)
    }

    fn session_dir(&self, machine: &str, id: &str) -> PathBuf {
        self.stage.join("sessions").join(machine).join(id)
    }

    fn shard_names(&self, machine: &str, id: &str) -> Vec<String> {
        let dir = self.session_dir(machine, id);
        let mut names: Vec<String> = chat_stasher::store::sealed_shard_entries(&dir)
            .expect("read shard entries")
            .into_iter()
            .map(|(_, path)| {
                path.file_name()
                    .expect("shard has a file name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn shard_records(&self, machine: &str, id: &str) -> Vec<Value> {
        let dir = self.session_dir(machine, id);
        let mut records = Vec::new();
        for (_, path) in chat_stasher::store::sealed_shard_entries(&dir).expect("entries") {
            let raw = fs::read_to_string(&path).expect("read shard");
            for line in raw.lines() {
                records.push(serde_json::from_str(line).expect("shard line is JSON"));
            }
        }
        records
    }
}

// -------------------------------------------------------------------- helpers

fn frame(value: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(value).expect("serialise request");
    let mut out = (body.len() as u32).to_ne_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Decode stdout as exactly one response frame — nothing before it, nothing
/// after it — and return the parsed body.
#[track_caller]
fn one_frame(stdout: &[u8]) -> Value {
    assert!(
        stdout.len() >= 4,
        "stdout is too short to hold a frame: {stdout:?}"
    );
    let declared = u32::from_ne_bytes(stdout[..4].try_into().expect("four bytes")) as usize;
    assert_eq!(
        stdout.len(),
        4 + declared,
        "stdout carries bytes outside the single response frame"
    );
    serde_json::from_slice(&stdout[4..]).expect("the response body is JSON")
}

#[track_caller]
fn exit_code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

#[track_caller]
fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A synthetic `inbox@1` bundle. Field order and shape mirror what the
/// extension serialises, so the payload a test sends is the payload the host
/// will see in production.
fn bundle(session: &str, text: &str) -> String {
    format!(
        r#"{{"schema":"chat-stasher/inbox@1","platform":"deepseek","sessionId":"{session}","capturedAt":"2026-09-12T00:00:00.000Z","parsed":{{"hasJson":true,"keys":["id"]}},"raw":{{"text":{text},"bytes":{n}}}}}"#,
        text = serde_json::to_string(text).expect("text as JSON"),
        n = text.len(),
    )
}

fn deliver_request(request_id: &str, name: &str, payload: &str) -> Value {
    json!({
        "protocol": 1,
        "type": "deliver",
        "request_id": request_id,
        "name": name,
        "payload": payload,
        "sha256": sha256_hex(payload.as_bytes()),
    })
}

fn deliver_request_with_account_id(
    request_id: &str,
    name: &str,
    payload: &str,
    account_id: &str,
) -> Value {
    let mut request = deliver_request(request_id, name, payload);
    request["account_id"] = json!(account_id);
    request
}

/// One delivered conversation, from the request to the ack.
fn deliver(fixture: &Fixture, request_id: &str, session: &str, text: &str) -> Value {
    let payload = bundle(session, text);
    let output = fixture.chrome(&frame(&deliver_request(
        request_id,
        &format!("deepseek-{session}.json"),
        &payload,
    )));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    response
}

// ---------------------------------------------------------------------- hello

#[test]
fn hello_returns_the_stage_and_the_machine() {
    let fixture = Fixture::new();
    fixture.configure_stage();

    let output = fixture.chrome(&frame(&json!({"protocol": 1, "type": "hello"})));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);

    assert_eq!(response["protocol"], 1);
    assert_eq!(response["type"], "hello");
    assert_eq!(response["ok"], true);
    assert_eq!(response["host_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        response["stage"].as_str().expect("stage is a string"),
        fixture.stage.to_string_lossy(),
        "hello must report the configured stage verbatim"
    );
    assert!(
        !response["machine"].as_str().expect("machine").is_empty(),
        "hello must report a machine id: {response}"
    );
}

#[test]
fn stdout_carries_exactly_one_frame_even_when_stderr_is_busy() {
    // A config warning is the point of this test: it proves the diagnostic went
    // to stderr by asserting stdout is *byte-exactly* one frame while stderr is
    // demonstrably non-empty.
    //
    // The warning used to be `harness_roots.codex = "~otheruser/..."`, which the
    // loader reset to its default with a warning. Since W145 an unexpandable path
    // makes the whole config unusable instead, so that fixture would now exercise
    // a refusal (`nack config`, nothing on stderr) rather than a warning — while
    // still passing, which is exactly the trap this comment is here to prevent.
    // A Windows path pasted into a basic string is the warning the loader still
    // emits and still recovers from, so the frame-vs-stderr property keeps a
    // fixture that really does warn.
    let fixture = Fixture::new();
    fixture.write_config(&format!(
        "[native_host]\nstage = {}\n\n[harness_roots]\ncodex = \"C:\\Users\\me\\AppData\\Roaming\\Codex\\sessions\"\n",
        serde_json::to_string(&fixture.stage.to_string_lossy()).expect("path")
    ));

    let output = fixture.chrome(&frame(&json!({"protocol": 1, "type": "hello"})));
    assert_eq!(exit_code(&output), 0);
    assert!(
        !output.stderr.is_empty(),
        "the fixture was supposed to make the loader warn on stderr"
    );
    // `one_frame` is the byte-level check: it fails if stdout is not exactly
    // one length-prefixed frame.
    let response = one_frame(&output.stdout);
    assert_eq!(response["type"], "hello");
}

// -------------------------------------------------------------------- deliver

#[test]
fn deliver_seals_a_shard_and_acks_it() {
    let fixture = Fixture::new();
    fixture.configure_stage();

    let payload = bundle("sess-a", "hello a");
    let sha = sha256_hex(payload.as_bytes());
    let response = deliver(&fixture, "req-1", "sess-a", "hello a");

    assert_eq!(response["type"], "ack");
    assert_eq!(response["status"], "stored");
    assert_eq!(response["request_id"], "req-1");
    assert_eq!(response["sha256"], sha.as_str());
    assert_eq!(response["shard"], "000001.jsonl");

    // Find the shard through the machine the host itself reported, so the test
    // never restates how the partition is built.
    let machine = first_machine(&fixture);
    assert_eq!(
        fixture.shard_names(&machine, "deepseek.sess-a"),
        ["000001.jsonl"]
    );

    let records = fixture.shard_records(&machine, "deepseek.sess-a");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["file_sha256"], sha.as_str());
    assert_eq!(
        records[0]["source_file"], "deepseek-sess-a.json",
        "the request's `name` is recorded as the shard's source_file"
    );
    assert_eq!(records[0]["id"], "deepseek.sess-a");
}

#[test]
fn the_same_payload_twice_is_a_duplicate_and_adds_no_shard() {
    let fixture = Fixture::new();
    fixture.configure_stage();

    let first = deliver(&fixture, "req-1", "sess-a", "hello a");
    assert_eq!(first["status"], "stored");

    let second = deliver(&fixture, "req-2", "sess-a", "hello a");
    assert_eq!(second["status"], "duplicate");
    assert_eq!(second["shard"], first["shard"]);
    assert_eq!(second["request_id"], "req-2");

    let machine = first_machine(&fixture);
    assert_eq!(
        fixture.shard_names(&machine, "deepseek.sess-a"),
        ["000001.jsonl"],
        "a duplicate must not seal a second shard"
    );
}

/// The machine id, read back from the host itself.
fn first_machine(fixture: &Fixture) -> String {
    let response = one_frame(
        &fixture
            .chrome(&frame(&json!({"protocol": 1, "type": "hello"})))
            .stdout,
    );
    response["machine"].as_str().expect("machine").to_string()
}

#[test]
fn a_sha256_that_does_not_match_the_payload_is_an_integrity_nack() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);

    let payload = bundle("sess-a", "hello a");
    let mut request = deliver_request("req-1", "deepseek-sess-a.json", &payload);
    request["sha256"] = json!("0".repeat(64));

    let output = fixture.chrome(&frame(&request));
    assert_eq!(exit_code(&output), 0);
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "nack");
    assert_eq!(response["kind"], "integrity");
    assert_eq!(response["retryable"], true);
    assert_eq!(response["request_id"], "req-1");

    assert!(
        !fixture.session_dir(&machine, "deepseek.sess-a").exists(),
        "a failed integrity check must write nothing at all"
    );
}

#[test]
fn a_payload_that_is_not_a_bundle_is_an_invalid_bundle_nack() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);

    for bad in [
        "this is not JSON at all",
        // JSON, but missing the identity axes a bundle must carry.
        r#"{"schema":"chat-stasher/inbox@1","raw":{"text":"x","bytes":1}}"#,
        // The identity axes are there but `raw` is not.
        r#"{"schema":"chat-stasher/inbox@1","platform":"deepseek","sessionId":"s"}"#,
    ] {
        let output = fixture.chrome(&frame(&deliver_request(
            "req-bad",
            "deepseek-sess-a.json",
            bad,
        )));
        assert_eq!(exit_code(&output), 0, "payload {bad:?}");
        let response = one_frame(&output.stdout);
        assert_matches_schema(&response);
        assert_eq!(response["kind"], "invalid-bundle", "payload {bad:?}");
        assert_eq!(response["retryable"], false, "payload {bad:?}");
    }
    assert!(
        !fixture.session_dir(&machine, "deepseek.sess-a").exists(),
        "an invalid bundle must not seal a raw-only shard either"
    );
}

/// An `inbox@2` bundle carrying the W205 install identity, shaped like the one
/// `buildBundle` serialises (`apps/extension/entrypoints/background.ts`).
fn identity_bundle(
    session: &str,
    text: &str,
    install_id: &str,
    browser: &str,
    label: &str,
) -> String {
    format!(
        r#"{{"schema":"chat-stasher/inbox@2","install_id":"{install_id}","browser":"{browser}","profile_label":"{label}","platform":"deepseek","sessionId":"{session}","capturedAt":"2026-09-26T00:00:00.000Z","parsed":{{"hasJson":true,"keys":["id"]}},"raw":{{"text":{raw},"bytes":{n}}}}}"#,
        raw = serde_json::to_string(text).expect("text as JSON"),
        n = text.len(),
    )
}

#[test]
fn status_reports_replace_per_install_without_merging() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let ids = [
        "11111111-1111-4111-8111-111111111111",
        "22222222-2222-4222-8222-222222222222",
    ];
    for (index, id) in ids.iter().enumerate() {
        let request = json!({"protocol":1,"type":"status","request_id":format!("status-{index}"),"status":{
            "install_id":id,"browser":"Chrome","profile_label":if index == 0 {"Personal"} else {"Work"},
            "extension_version":"0.4.0","reported_at":"2026-09-27T12:00:00Z",
            "platforms":[{"platform":"chatgpt","captured_by_this_browser":index+1,"pending":9-index,"paused_reason":null,"account_fingerprint":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]
        }});
        let output = fixture.chrome(&frame(&request));
        assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
        let response = one_frame(&output.stdout);
        assert_matches_schema(&response);
        assert_eq!(response["type"], "status");
        assert_eq!(response["request_id"], format!("status-{index}"));
    }
    let replacement = json!({"protocol":1,"type":"status","request_id":"status-refresh","status":{
        "install_id":ids[0],"browser":"Chrome","profile_label":"Personal",
        "extension_version":"0.4.1","reported_at":"2026-09-27T12:05:00Z",
        "tick_ran":true,"tick_reason":"ran","tick_stopped":"queue-empty","tick_halted":null,
        "build_stamp":"bmtq3x1f",
        "platforms":[{"platform":"chatgpt","captured_by_this_browser":99,"pending":1,"paused_reason":null}]
    }});
    let output = fixture.chrome(&frame(&replacement));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    assert_eq!(one_frame(&output.stdout)["type"], "status");
    let machine = first_machine(&fixture);
    let status_dir = fixture.stage.join("ext-status").join(&machine);
    for (index, id) in ids.iter().enumerate() {
        let raw = fs::read(status_dir.join(format!("{id}.json"))).expect("status persisted");
        let value: Value = serde_json::from_slice(&raw).expect("valid JSON status");
        assert_eq!(value["schema"], "chat-stasher/ext-status@1");
        assert_eq!(value["install_id"], *id);
        assert_eq!(
            value["platforms"][0]["pending"],
            if index == 0 { 1 } else { 8 }
        );
        if index == 0 {
            assert_eq!(value["platforms"][0]["captured_by_this_browser"], 99);
            // W913 (c4) · The tick outcome and the build stamp are recorded
            // verbatim, `tick_halted: null` included: on the wire that null
            // is the positive fact "nothing halted", and writing the field
            // away would turn it back into the older extension's unknown.
            assert_eq!(value["tick_ran"], true);
            assert_eq!(value["tick_reason"], "ran");
            assert_eq!(value["tick_stopped"], "queue-empty");
            assert_eq!(value["tick_halted"], Value::Null);
            assert_eq!(value["build_stamp"], "bmtq3x1f");
        } else {
            // The first two requests carried none of the new fields, so
            // their records gained none: an extension that predates the
            // fields is "unknown", and a default here would say "running"
            // about an install that said nothing at all.
            assert!(
                value.get("tick_ran").is_none()
                    && value.get("tick_reason").is_none()
                    && value.get("tick_stopped").is_none()
                    && value.get("tick_halted").is_none()
                    && value.get("build_stamp").is_none()
            );
        }
        assert!(value.get("account_id").is_none());
    }
}

/// W913 (c4) · The tick-outcome and build-stamp fields are optional — an
/// extension that predates them sends none, and its reports are still
/// accepted — but a value of the wrong type is malformed, not unknown: a
/// record that archived `tick_ran: "yes"` would hand a reader a string where
/// every consumer expects a verdict. An **empty** string is malformed on the
/// same footing: these fields carry closed-set codes and an identity, none of
/// which has a blank member, so `""` is a value the record could never have
/// meant — and it would render as the word it cannot read ("last tick did
/// not run ()", " · build "). The designed nulls (`tick_halted`,
/// `build_stamp`) stay accepted; a blank is not one of them.
#[test]
fn status_tick_outcome_fields_of_the_wrong_type_are_a_bad_request() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let id = "11111111-1111-4111-8111-111111111111";
    for (why, field, value) in [
        ("tick_ran is not a boolean", "tick_ran", json!("yes")),
        ("tick_ran is not a boolean", "tick_ran", json!(1)),
        ("tick_reason is not a string", "tick_reason", json!(13)),
        (
            "tick_reason is null, which is not one of its designed states",
            "tick_reason",
            json!(null),
        ),
        (
            "tick_stopped is not a string",
            "tick_stopped",
            json!({"a": 1}),
        ),
        (
            "tick_halted is not a string or null",
            "tick_halted",
            json!(9),
        ),
        (
            "build_stamp is not a string or null",
            "build_stamp",
            json!(4.5),
        ),
        (
            "build_stamp is longer than the bound",
            "build_stamp",
            json!("b".repeat(81)),
        ),
        (
            "tick_reason is empty and names nothing",
            "tick_reason",
            json!(""),
        ),
        (
            "tick_stopped is empty and names nothing",
            "tick_stopped",
            json!(""),
        ),
        (
            "tick_halted is empty, which is not its designed null",
            "tick_halted",
            json!(""),
        ),
        (
            "build_stamp is empty, which is not its designed null",
            "build_stamp",
            json!(""),
        ),
    ] {
        let mut status = json!({
            "install_id": id, "browser": "Chrome", "profile_label": null,
            "extension_version": "0.4.0", "reported_at": "2026-09-27T12:00:00Z",
            "platforms": []
        });
        status[field] = value;
        let request = json!({"protocol":1,"type":"status","request_id":format!("status-{field}"),"status":status});
        let output = fixture.chrome(&frame(&request));
        assert_eq!(
            exit_code(&output),
            0,
            "{why}: stderr: {}",
            stderr_of(&output)
        );
        let response = one_frame(&output.stdout);
        assert_matches_schema(&response);
        assert_eq!(response["type"], "nack", "{why}");
        assert_eq!(response["kind"], "bad-request", "{why}");
        assert_eq!(response["retryable"], false, "{why}");
    }
    // Nothing was persisted: a refused report replaces no previous record,
    // exactly as §6.7 says a `nack` leaves the caller's status unconfirmed.
    assert!(
        ls_status_dir(&fixture, id).is_none(),
        "a refused report must not persist a record"
    );
}

/// The staged file for one install, or `None` when no record exists.
fn ls_status_dir(fixture: &Fixture, install_id: &str) -> Option<PathBuf> {
    let machine = first_machine(fixture);
    let dir = fixture.stage.join("ext-status").join(&machine);
    let path = dir.join(format!("{install_id}.json"));
    path.is_file().then_some(path)
}

/// W205c · D4, wire-level: a copied install (same `install_id`, different
/// user-named profile label) must be refused in a way an extension can act on —
/// `install-conflict` is item-scope and non-retryable, so the entry lands
/// visible and rejected instead of piling up pending behind a host-scope
/// `config`. This test was red while the refusal was kind `config`: the scope
/// table on the extension side kept the item pending forever.
#[test]
fn a_copied_install_is_refused_with_install_conflict_not_config() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);

    // The original install seals normally.
    let original = identity_bundle("sess-a", "hello a", "install-fixture", "Chrome", "Personal");
    let output = fixture.chrome(&frame(&deliver_request(
        "req-orig",
        "deepseek-sess-a.json",
        &original,
    )));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "ack");
    assert!(fixture.session_dir(&machine, "deepseek.sess-a").exists());

    // The copy — same install_id, a label the user actually named — is refused.
    let copy = identity_bundle("sess-b", "hello b", "install-fixture", "Chrome", "Work");
    let output = fixture.chrome(&frame(&deliver_request(
        "req-copy",
        "deepseek-sess-b.json",
        &copy,
    )));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "nack");
    assert_eq!(response["kind"], "install-conflict");
    assert_eq!(response["retryable"], false);
    assert!(
        response["detail"]
            .as_str()
            .expect("detail is a string")
            .contains("regenerate the install identity"),
        "the refusal must tell the later install what to do: {response}"
    );
    assert!(
        !fixture.session_dir(&machine, "deepseek.sess-b").exists(),
        "a refused delivery must seal nothing"
    );
}

/// W930 · The stage-local index of sealed install provenance, on the wire.
///
/// D4's refusal used to be produced by reading every shard in the stage on
/// every `deliver` — 196–205 s on this machine's 12 GB stage, against the
/// extension's 60 s request budget, so every delivery answered past the budget,
/// the `ack` was never read, and the debt was never settled. The evidence is now
/// written down when the seal happens (`meta/<machine>/install-provenance-v1.jsonl`)
/// and the stage is read only when the index has nothing to say about the
/// install being asked about.
///
/// Two halves, and both have to hold: the seal must write the index down, and
/// the index must never be the *authority* — a stage whose shards were sealed
/// before the index existed has to be read, and still refuses the copy.
#[test]
fn a_sealed_install_is_indexed_and_the_index_is_never_the_authority() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);
    let index = fixture
        .stage
        .join("meta")
        .join(&machine)
        .join("install-provenance-v1.jsonl");

    // ① The seal writes down what it made true.
    let original = identity_bundle("sess-a", "hello a", "install-w930", "Chrome", "Personal");
    let output = fixture.chrome(&frame(&deliver_request(
        "req-orig",
        "deepseek-sess-a.json",
        &original,
    )));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    assert_eq!(one_frame(&output.stdout)["type"], "ack");
    let written = fs::read_to_string(&index).unwrap_or_else(|error| {
        panic!(
            "the seal must index its provenance at {}: {error}",
            index.display()
        )
    });
    assert!(
        written.contains("install-w930"),
        "the index names the install that sealed: {written}"
    );

    // ② Throw the index away, the way a stage sealed before this index existed
    //    has none: the copy is refused anyway, because the shards are read.
    fs::remove_file(&index).expect("remove the index");
    let copy = identity_bundle("sess-b", "hello b", "install-w930", "Chrome", "Work");
    let output = fixture.chrome(&frame(&deliver_request(
        "req-copy",
        "deepseek-sess-b.json",
        &copy,
    )));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "nack");
    assert_eq!(response["kind"], "install-conflict");
    assert!(
        !fixture.session_dir(&machine, "deepseek.sess-b").exists(),
        "a refused delivery must seal nothing"
    );
    assert!(
        fs::read_to_string(&index)
            .expect("the read refills the index")
            .contains("install-w930"),
        "what reading the stage found is written down again"
    );
}

/// W930 · The timeout re-send waits out a held stage lock.
///
/// A `deliver` holds the stage lock across its whole seal, and a
/// seal can be the one-time cold fill of the install-provenance
/// index — measured 73.6 s on a 12 GB stage, past the extension's
/// 60 s request budget. The extension answers that timeout by
/// re-sending the *identical* request, so the re-send arrives while
/// the first request still holds the lock. Its own lock wait has to
/// outlast what is left of the fill inside its own budget, or it
/// collects `stage-unavailable` while the first request's
/// acknowledgement is still being written — and the debt stays
/// pending for the next tick, which re-fetches the conversation with
/// a new `capturedAt` and seals a second shard the byte-level
/// duplicate rule cannot recognise.
///
/// The hold below is longer than the wait used to be (10 s) with
/// margin for the spawn's own startup, and far inside what it
/// now is (the extension's 60 s budget), so this test was red
/// against the old bound: the re-send gave up at 10 s and
/// answered `stage-unavailable`, while the shard count
/// assertion below would still have held — the point is the
/// *answer*, not the count.
#[test]
fn a_delivery_waits_out_a_held_stage_lock_and_answers_duplicate() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);

    // ① The first request seals the bytes — the shard the re-send
    //    is about to ask about.
    let payload = identity_bundle(
        "sess-lock",
        "hello lock",
        "install-w930-lock",
        "Chrome",
        "Personal",
    );
    let first = fixture.chrome(&frame(&deliver_request(
        "req-lock",
        "deepseek-sess-lock.json",
        &payload,
    )));
    assert_eq!(exit_code(&first), 0, "stderr: {}", stderr_of(&first));
    assert_eq!(one_frame(&first.stdout)["type"], "ack");

    // ② Another process holds the stage lock for longer than the
    //    wait used to be: the cold fill the first request would still
    //    be running.
    let (held, release) = std::sync::mpsc::channel();
    let holder = {
        let stage = fixture.stage.clone();
        thread::spawn(move || {
            let lock = chat_stasher::inbox::lock_stage(&stage).expect("take the stage lock");
            held.send(()).expect("announce the hold");
            // Past the old 10-second bound with room for the
            // spawn's own startup, inside the new 60-second one.
            std::thread::sleep(std::time::Duration::from_secs(13));
            drop(lock);
        })
    };
    release
        .recv()
        .expect("the hold is in place before the re-send");

    // ③ The re-send arrives while the lock is held.
    let output = fixture.chrome(&frame(&deliver_request(
        "req-lock",
        "deepseek-sess-lock.json",
        &payload,
    )));
    holder.join().expect("the holder releases the lock");

    // ④ The re-send waited the hold out and answered from the stage.
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "ack");
    assert_eq!(
        response["status"], "duplicate",
        "the re-send must read the answer the first send earned, not give up on the lock"
    );
    assert_eq!(
        fixture.shard_names(&machine, "deepseek.sess-lock"),
        ["000001.jsonl"],
        "the re-send must not seal a second shard"
    );
}

/// W930 · The timeout re-send waits out a held coordination-state
/// transaction.
///
/// The stage lock is not the only lock a re-send waits behind: a
/// `deliver` holds the coordination state database's write
/// transaction (`BEGIN IMMEDIATE`) across its whole seal, so the
/// re-send arrives while the first request still holds it — and
/// even the re-send's *open* of that database waits, because
/// applying the schema writes. That wait has to outlast what is
/// left of the cold fill inside the re-send's own budget too, or
/// the re-send answers `nack` `io` and the first request's
/// acknowledgement is lost exactly as surely as a `stage-unavailable`
/// would lose it.
///
/// The old wait was a 5-second busy timeout inside a 10-second
/// retry window, so its total patience was ~10 s from the open —
/// measured against the unfixed code, a re-send behind a held
/// transaction answered `nack` `io` at half past ten. The hold
/// below is past that with margin for the spawn's own startup,
/// and far inside what the delivery path now asks for (60
/// seconds, the extension's per-request budget), so this test
/// was red against the old bound: the re-send's open gave up
/// and the response was a `nack`, not an `ack`.
#[test]
fn a_delivery_waits_out_a_held_state_transaction_and_answers_duplicate() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);

    // ① The first request seals the bytes — and opens the state
    //    database, so the schema exists and the hold below is the
    //    only lock on it.
    let payload = identity_bundle(
        "sess-db",
        "hello db",
        "install-w930-db",
        "Chrome",
        "Personal",
    );
    let first = fixture.chrome(&frame(&deliver_request(
        "req-db",
        "deepseek-sess-db.json",
        &payload,
    )));
    assert_eq!(exit_code(&first), 0, "stderr: {}", stderr_of(&first));
    assert_eq!(one_frame(&first.stdout)["type"], "ack");

    // ② Another process holds the state database's write transaction
    //    for longer than the wait used to be: the cold fill the
    //    first request would still be running inside its transaction.
    //    The path mirrors `collect::default_state_dir()` under the
    //    fixture's `XDG_DATA_HOME`.
    let state_db = fixture
        .home
        .join("data")
        .join("chat-stasher")
        .join("state")
        .join("extension-coordination.sqlite3");
    let (held, release) = std::sync::mpsc::channel();
    let holder = {
        let state_db = state_db.clone();
        thread::spawn(move || {
            let conn = rusqlite::Connection::open(&state_db).expect("open the state database");
            conn.execute_batch("BEGIN IMMEDIATE")
                .expect("take the write transaction");
            held.send(()).expect("announce the hold");
            // Past the old total patience (~10 s from the
            // open: a 5 s busy timeout, a retry, a second
            // 5 s attempt) with room for the spawn's own
            // startup, inside the new 60-second one.
            std::thread::sleep(std::time::Duration::from_secs(13));
            conn.execute_batch("ROLLBACK")
                .expect("release the write transaction");
        })
    };
    release
        .recv()
        .expect("the hold is in place before the re-send");

    // ③ The re-send arrives while the transaction is held.
    let output = fixture.chrome(&frame(&deliver_request(
        "req-db",
        "deepseek-sess-db.json",
        &payload,
    )));
    holder.join().expect("the holder releases the transaction");

    // ④ The re-send waited the hold out and answered from the stage.
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "ack");
    assert_eq!(
        response["status"], "duplicate",
        "the re-send must read the answer the first send earned, not give up on the state database"
    );
    assert_eq!(
        fixture.shard_names(&machine, "deepseek.sess-db"),
        ["000001.jsonl"],
        "the re-send must not seal a second shard"
    );
}

/// W930 · A re-send that arrives after the budget expired waits out
/// the rest of the fill.
///
/// The measured cold fill (73.6 s) outlasts the extension's 60 s
/// request budget (protocol §2), so the extension's first request is
/// still running — still holding the stage lock — when its callback
/// dies and the identical re-send arrives. The re-send's own budget
/// is a fresh 60 s, and what it must wait out is what is left of the
/// fill. A hold that merely exceeds the old 10 s bound does not
/// model this: the review's case is a hold that **began before the
/// budget expired and ends after it**, so the hold below runs from
/// before the simulated expiry to 14 s into the re-send's own
/// window — the measured fill's shape, 60 s + 14 s. Against a
/// 10 s lock bound the re-send collected `stage-unavailable` around
/// 70 s, exactly while the first request's acknowledgement was still
/// in flight, and the debt stayed pending for the next tick, which
/// re-fetches the conversation with a new `capturedAt` and seals a
/// second shard the byte-level duplicate rule cannot recognise.
#[test]
fn a_delivery_that_arrives_after_the_budget_expired_waits_out_the_fill() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);

    // ① The first request seals the bytes — the shard the re-send
    //    is about to ask about.
    let payload = identity_bundle(
        "sess-long",
        "hello long",
        "install-w930-long",
        "Chrome",
        "Personal",
    );
    let first = fixture.chrome(&frame(&deliver_request(
        "req-long",
        "deepseek-sess-long.json",
        &payload,
    )));
    assert_eq!(exit_code(&first), 0, "stderr: {}", stderr_of(&first));
    assert_eq!(one_frame(&first.stdout)["type"], "ack");

    // ② The cold fill the first request is still running when the
    //    budget expires: the stage lock held from before the expiry
    //    to 14 s into the re-send's own window.
    const BUDGET_S: u64 = 60;
    const FILL_S: u64 = 74;
    let (held, release) = std::sync::mpsc::channel();
    let holder = {
        let stage = fixture.stage.clone();
        thread::spawn(move || {
            let lock = chat_stasher::inbox::lock_stage(&stage).expect("take the stage lock");
            held.send(()).expect("announce the hold");
            std::thread::sleep(std::time::Duration::from_secs(FILL_S));
            drop(lock);
        })
    };
    release
        .recv()
        .expect("the hold is in place before the expiry");

    // ③ The extension's first request budget expires with the fill
    //    still running; the identical request is sent again.
    std::thread::sleep(std::time::Duration::from_secs(BUDGET_S));

    // ④ The re-send arrives with the fill's tail still holding the
    //    lock, inside its own fresh budget.
    let output = fixture.chrome(&frame(&deliver_request(
        "req-long",
        "deepseek-sess-long.json",
        &payload,
    )));
    holder.join().expect("the holder releases the lock");

    // ⑤ The re-send waited out the rest of the fill and answered
    //    from the stage.
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "ack");
    assert_eq!(
        response["status"], "duplicate",
        "the re-send must read the answer the first send earned, not give up on the lock"
    );
    assert_eq!(
        fixture.shard_names(&machine, "deepseek.sess-long"),
        ["000001.jsonl"],
        "the re-send must not seal a second shard"
    );
}

// ------------------------------------------------------------------ §6.6 has

/// A content fingerprint. The host compares it as an opaque string, so any 64
/// lowercase hex characters would do — but it is derived rather than written out
/// so that a test which changes its seed changes its value, the way a real
/// fingerprint tracks the body it came from.
fn fingerprint_of(seed: &str) -> String {
    sha256_hex(seed.as_bytes())
}

fn has_request(request_id: &str, platform: &str, session: &str, fingerprint: &str) -> Value {
    json!({
        "protocol": 1,
        "type": "has",
        "request_id": request_id,
        "platform": platform,
        "session_id": session,
        "fingerprint": fingerprint,
    })
}

/// One `has` question, run through the real binary and schema-checked.
#[track_caller]
fn ask_has(fixture: &Fixture, request: &Value) -> Value {
    let output = fixture.chrome(&frame(request));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    response
}

/// `deliver` a bundle whose request carries a `fingerprint` — the shape a W50c
/// extension sends. `deliver_request` deliberately leaves it out, so the tests
/// that use this one are the ones asking about fingerprints.
#[track_caller]
fn deliver_with_fingerprint(
    fixture: &Fixture,
    request_id: &str,
    session: &str,
    text: &str,
    fingerprint: &str,
) -> Value {
    let payload = bundle(session, text);
    let mut request = deliver_request(request_id, &format!("deepseek-{session}.json"), &payload);
    request["fingerprint"] = json!(fingerprint);
    let output = fixture.chrome(&frame(&request));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    response
}

/// Every path under `root`, relative and sorted — used to prove a read-only
/// request wrote nothing, without having to name what it might have written.
fn tree(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            out.push(
                path.strip_prefix(root)
                    .expect("under root")
                    .to_string_lossy()
                    .into_owned(),
            );
            if entry.file_type().expect("file type").is_dir() {
                stack.push(path);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn a_delivered_fingerprint_is_held_and_the_answer_names_the_shard() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let fingerprint = fingerprint_of("sess-a body v1");

    let ack = deliver_with_fingerprint(&fixture, "req-1", "sess-a", "hello a", &fingerprint);
    assert_eq!(
        ack["type"], "ack",
        "the fingerprint must not change delivery"
    );

    let response = ask_has(
        &fixture,
        &has_request("req-has-1", "deepseek", "sess-a", &fingerprint),
    );
    assert_eq!(response["type"], "has");
    assert_eq!(response["ok"], true);
    assert_eq!(response["request_id"], "req-has-1");
    assert_eq!(response["held"], true);
    assert_eq!(response["shard"], ack["shard"]);
}

#[test]
fn a_fingerprint_no_shard_carries_is_answered_not_held() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);

    // Nothing at all has been delivered for this conversation.
    let response = ask_has(
        &fixture,
        &has_request("req-has-1", "deepseek", "sess-never", &fingerprint_of("x")),
    );
    assert_eq!(response["held"], false, "an empty stage holds nothing");
    assert_eq!(response["shard"], Value::Null);

    // A conversation that *was* delivered, asked about a different body.
    deliver_with_fingerprint(
        &fixture,
        "req-1",
        "sess-a",
        "hello a",
        &fingerprint_of("the body that was stored"),
    );
    let other = ask_has(
        &fixture,
        &has_request(
            "req-has-2",
            "deepseek",
            "sess-a",
            &fingerprint_of("a body nobody stored"),
        ),
    );
    assert_eq!(other["held"], false);
    assert_eq!(other["shard"], Value::Null);

    // `held: false` is a measurement, not an absence: the shard it answered about
    // is still there, untouched.
    assert_eq!(
        fixture.shard_names(&machine, "deepseek.sess-a"),
        ["000001.jsonl"]
    );
}

/// 🔴 The answer comes from the archive, so a delivery's fingerprint can only
/// answer for the conversation whose directory the delivery went into. This is
/// the property that makes "a replaced archive at the same path" survivable: a
/// lookup that finds nothing says so instead of falling back to a remembered
/// record of the delivery.
#[test]
fn the_lookup_is_scoped_to_the_conversation_that_delivered_it() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let fingerprint = fingerprint_of("one body, one conversation");

    deliver_with_fingerprint(&fixture, "req-1", "sess-a", "hello a", &fingerprint);

    let elsewhere = ask_has(
        &fixture,
        &has_request("req-has-1", "deepseek", "sess-b", &fingerprint),
    );
    assert_eq!(
        elsewhere["held"], false,
        "another conversation's directory must not answer for this one"
    );

    let other_platform = ask_has(
        &fixture,
        &has_request("req-has-2", "chatgpt", "sess-a", &fingerprint),
    );
    assert_eq!(other_platform["held"], false);

    let here = ask_has(
        &fixture,
        &has_request("req-has-3", "deepseek", "sess-a", &fingerprint),
    );
    assert_eq!(
        here["held"], true,
        "and the conversation that did deliver it still answers yes"
    );
}

/// 🔴 A shard sealed without a fingerprint — every shard from before this field
/// existed, and every one sealed from an inbox file or an export line — is not
/// matched by one. The answer is `held: false`, so the conversation is delivered
/// once more and the shard written then carries its fingerprint. Stated as a
/// cost rather than hidden: the alternative would be to guess a fingerprint for
/// content that has none, and a guess here skips a conversation.
#[test]
fn a_shard_sealed_without_a_fingerprint_is_not_matched_by_one() {
    let fixture = Fixture::new();
    fixture.configure_stage();

    let response = deliver(&fixture, "req-1", "sess-a", "hello a");
    assert_eq!(response["status"], "stored");

    let machine = first_machine(&fixture);
    let records = fixture.shard_records(&machine, "deepseek.sess-a");
    assert!(
        records[0].get("fingerprint").is_none(),
        "a delivery without a fingerprint must not record one: {}",
        records[0]
    );

    let asked = ask_has(
        &fixture,
        &has_request(
            "req-has-1",
            "deepseek",
            "sess-a",
            &fingerprint_of("anything"),
        ),
    );
    assert_eq!(asked["held"], false);
}

/// 🔴 The review finding, at the level the host can see: the record of a
/// delivery lives in the shard, so an archive that is replaced or recreated at
/// the same path holds nothing and says so. Nothing here is remembered.
#[test]
fn a_recreated_session_directory_holds_nothing() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let fingerprint = fingerprint_of("the body that was stored");
    deliver_with_fingerprint(&fixture, "req-1", "sess-a", "hello a", &fingerprint);

    let machine = first_machine(&fixture);
    let dir = fixture.session_dir(&machine, "deepseek.sess-a");
    assert_eq!(
        ask_has(
            &fixture,
            &has_request("req-has-1", "deepseek", "sess-a", &fingerprint)
        )["held"],
        true
    );

    // The same path, emptied and recreated — a restored or rebuilt archive.
    fs::remove_dir_all(&dir).expect("remove the session directory");
    fs::create_dir_all(&dir).expect("recreate it, empty");

    let after = ask_has(
        &fixture,
        &has_request("req-has-2", "deepseek", "sess-a", &fingerprint),
    );
    assert_eq!(
        after["held"], false,
        "a recreated archive at the same path must not answer for content it does not hold"
    );
    assert_eq!(after["shard"], Value::Null);
}

#[test]
fn has_writes_nothing_at_all() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    deliver_with_fingerprint(
        &fixture,
        "req-1",
        "sess-a",
        "hello a",
        &fingerprint_of("stored"),
    );

    let before = tree(&fixture.stage);
    assert!(
        !before.is_empty(),
        "the fixture must have something to preserve"
    );

    for request in [
        has_request("req-has-1", "deepseek", "sess-a", &fingerprint_of("stored")),
        has_request("req-has-2", "deepseek", "sess-b", &fingerprint_of("never")),
    ] {
        let response = ask_has(&fixture, &request);
        assert_eq!(response["ok"], true);
    }

    assert_eq!(
        tree(&fixture.stage),
        before,
        "`has` is read-only: the stage must be byte-identical in shape afterwards"
    );
}

/// 🔴 "Could not look" is not "nothing is held". A directory that cannot be read
/// is a `nack`, which the extension reads as "not known to be held" ⇒ deliver.
/// Answering `held: false` here would be the invariant this repository treats as
/// least acceptable, inverted: an unknown recorded as a measurement.
#[test]
fn a_lookup_that_cannot_read_the_directory_is_a_nack_not_a_not_held() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);

    // A *file* where the conversation's directory would be: the path exists, and
    // it cannot be listed. Portable, unlike a permission bit.
    let dir = fixture.session_dir(&machine, "deepseek.sess-a");
    fs::create_dir_all(dir.parent().expect("parent")).expect("sessions dir");
    fs::write(&dir, b"not a directory").expect("write the blocker");

    let response = ask_has(
        &fixture,
        &has_request("req-has-1", "deepseek", "sess-a", &fingerprint_of("x")),
    );
    assert_eq!(response["type"], "nack");
    assert_eq!(response["kind"], "io");
    assert_eq!(response["retryable"], true);
    assert_eq!(response["request_id"], "req-has-1");
}

#[test]
fn a_malformed_has_request_is_a_bad_request() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let fingerprint = fingerprint_of("x");

    for (why, request) in [
        (
            "fingerprint is not 64 lowercase hex",
            json!({"protocol": 1, "type": "has", "request_id": "r",
                   "platform": "deepseek", "session_id": "s", "fingerprint": "nope"}),
        ),
        (
            "session_id is missing",
            json!({"protocol": 1, "type": "has", "request_id": "r",
                   "platform": "deepseek", "fingerprint": fingerprint}),
        ),
        (
            "platform is empty",
            json!({"protocol": 1, "type": "has", "request_id": "r",
                   "platform": "", "session_id": "s", "fingerprint": fingerprint}),
        ),
        (
            "request_id is malformed",
            json!({"protocol": 1, "type": "has", "request_id": "has spaces",
                   "platform": "deepseek", "session_id": "s", "fingerprint": fingerprint}),
        ),
    ] {
        let output = fixture.chrome(&frame(&request));
        assert_eq!(exit_code(&output), 0, "{why}");
        let response = one_frame(&output.stdout);
        assert_matches_schema(&response);
        assert_eq!(response["kind"], "bad-request", "{why}");
        assert_eq!(response["retryable"], false, "{why}");
    }
}

/// A delivery request that carries a malformed `fingerprint` must not be archived
/// as if it had carried none: the field is checked the same way `sha256` is, and
/// a shard whose fingerprint nobody can read is one that can never answer `has`.
#[test]
fn a_malformed_fingerprint_on_a_delivery_is_a_bad_request() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);

    let payload = bundle("sess-a", "hello a");
    let mut request = deliver_request("req-1", "deepseek-sess-a.json", &payload);
    request["fingerprint"] = json!("NOT-HEX");

    let output = fixture.chrome(&frame(&request));
    assert_eq!(exit_code(&output), 0);
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["kind"], "bad-request");
    assert_eq!(response["retryable"], false);
    assert!(
        !fixture.session_dir(&machine, "deepseek.sess-a").exists(),
        "a refused delivery must seal nothing"
    );
}

#[test]
fn the_shapes_the_has_message_produces_match_the_committed_schema() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let fingerprint = fingerprint_of("stored");
    deliver_with_fingerprint(&fixture, "req-1", "sess-a", "hello a", &fingerprint);

    // Both answers, and both are run through `assert_matches_schema` inside
    // `ask_has`. The explicit assertion here is that the response carries exactly
    // the fields §6.6 lists — `additionalProperties: false` is what makes an
    // extra field a red test rather than an ignored one.
    let held = ask_has(
        &fixture,
        &has_request("req-has-1", "deepseek", "sess-a", &fingerprint),
    );
    // Sorted, because `serde_json::Map` is a `BTreeMap` in this build: the point
    // is the *set* of fields, not the order they were serialised in.
    let mut keys: Vec<String> = held
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        ["held", "ok", "protocol", "request_id", "shard", "type"]
    );
}

#[test]
fn the_other_install_count_request_and_response_match_the_committed_schema() {
    assert_matches_schema(&json!({
        "protocol": 1,
        "type": "other_installs",
        "request_id": "other-count-1",
        "install_id": "11111111-1111-4111-8111-111111111111"
    }));
    assert_matches_schema(&json!({
        "protocol": 1,
        "type": "other_installs",
        "ok": true,
        "request_id": "other-count-1",
        "count": 2
    }));
}

// -------------------------------------------------------------- bad requests

#[test]
fn an_unsupported_or_missing_protocol_is_a_protocol_version_nack() {
    let fixture = Fixture::new();
    fixture.configure_stage();

    for request in [
        json!({"protocol": 2, "type": "hello"}),
        json!({"type": "hello"}),
        json!({"protocol": 2, "type": "deliver", "request_id": "r", "name": "a-b.json", "payload": "{}", "sha256": "0".repeat(64)}),
    ] {
        let output = fixture.chrome(&frame(&request));
        assert_eq!(exit_code(&output), 0);
        let response = one_frame(&output.stdout);
        assert_matches_schema(&response);
        assert_eq!(response["kind"], "protocol-version", "request {request}");
        assert_eq!(response["supported"], json!([1]));
        assert_eq!(response["retryable"], false);
    }
}

#[test]
fn unknown_types_and_malformed_fields_are_bad_requests() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let payload = bundle("sess-a", "hello a");

    let cases: Vec<Value> = vec![
        json!({"protocol": 1, "type": "teleport"}),
        json!({"protocol": 1}),
        json!({"protocol": 1, "type": "deliver"}),
        json!({"protocol": 1, "type": "deliver", "request_id": "", "name": "a-b.json", "payload": payload, "sha256": sha256_hex(payload.as_bytes())}),
        json!({"protocol": 1, "type": "deliver", "request_id": "a".repeat(129), "name": "a-b.json", "payload": payload, "sha256": sha256_hex(payload.as_bytes())}),
        json!({"protocol": 1, "type": "deliver", "request_id": "has space", "name": "a-b.json", "payload": payload, "sha256": sha256_hex(payload.as_bytes())}),
        json!({"protocol": 1, "type": "deliver", "request_id": "req-1", "name": "NoDash.json", "payload": payload, "sha256": sha256_hex(payload.as_bytes())}),
        json!({"protocol": 1, "type": "deliver", "request_id": "req-1", "name": "deepseek-a/b.json", "payload": payload, "sha256": sha256_hex(payload.as_bytes())}),
        json!({"protocol": 1, "type": "deliver", "request_id": "req-1", "name": "deepseek-a.txt", "payload": payload, "sha256": sha256_hex(payload.as_bytes())}),
        json!({"protocol": 1, "type": "deliver", "request_id": "req-1", "name": "deepseek-a.json", "payload": payload, "sha256": "ABC"}),
    ];

    for request in cases {
        let output = fixture.chrome(&frame(&request));
        assert_eq!(exit_code(&output), 0, "request {request}");
        let response = one_frame(&output.stdout);
        assert_matches_schema(&response);
        assert_eq!(response["kind"], "bad-request", "request {request}");
        assert_eq!(response["retryable"], false, "request {request}");
    }
}

// ------------------------------------------------------------ config / stage

#[test]
fn a_config_without_the_stage_key_is_a_config_nack_naming_the_fix() {
    let fixture = Fixture::new();
    fixture.write_config("# nothing configured yet\n");

    let output = fixture.chrome(&frame(&json!({"protocol": 1, "type": "hello"})));
    assert_eq!(exit_code(&output), 0);
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["kind"], "config");
    assert_eq!(response["retryable"], false);
    let detail = response["detail"].as_str().expect("detail");
    assert!(
        detail.contains("install-native-host --stage"),
        "the fix command must be in the detail: {detail}"
    );

    // §6.6's refusal of the same situation, for the same reason: a host with no
    // stage cannot answer the question, and must not answer it with "not held".
    let asked = ask_has(
        &fixture,
        &has_request("req-has-1", "deepseek", "sess-a", &fingerprint_of("x")),
    );
    assert_eq!(asked["type"], "nack");
    assert_eq!(asked["kind"], "config");
}

#[test]
fn a_missing_stage_is_stage_unavailable_and_is_still_missing_afterwards() {
    let fixture = Fixture::new();
    let absent = fixture.dir.path().join("never-created");
    fixture.write_config(&format!(
        "[native_host]\nstage = {}\n",
        serde_json::to_string(&absent.to_string_lossy()).expect("path")
    ));

    for request in [
        json!({"protocol": 1, "type": "hello"}),
        deliver_request("req-1", "deepseek-sess-a.json", &bundle("sess-a", "hi")),
        // §6.6 resolves the target exactly as `deliver` does, so "the stage is
        // not there" must reach the extension as the same refusal rather than as
        // `held: false` — which would read as "asked, nothing held".
        has_request("req-2", "deepseek", "sess-a", &fingerprint_of("x")),
    ] {
        let output = fixture.chrome(&frame(&request));
        assert_eq!(exit_code(&output), 0);
        let response = one_frame(&output.stdout);
        assert_matches_schema(&response);
        assert_eq!(response["kind"], "stage-unavailable", "request {request}");
        assert_eq!(response["retryable"], true, "request {request}");
    }

    // §4: "The host never creates a stage." The strongest form of that claim is
    // that the directory is still not there after both request types ran.
    assert!(
        !absent.exists(),
        "the host created a stage: {}",
        absent.display()
    );
}

// -------------------------------------------------------------- launch shapes

#[test]
fn firefox_argv_reaches_host_mode() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let manifest = fixture.home.join("com.chat_stasher.host.json");

    let output = fixture.run(
        &[manifest.to_str().expect("utf-8 path"), FIREFOX_ID],
        &frame(&json!({"protocol": 1, "type": "hello"})),
    );
    assert_eq!(
        exit_code(&output),
        0,
        "Firefox launch must serve a request; stderr: {}",
        stderr_of(&output)
    );
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "hello");
    assert_eq!(response["ok"], true);
}

#[test]
fn a_foreign_chrome_extension_id_writes_nothing_and_exits_non_zero() {
    let fixture = Fixture::new();
    fixture.configure_stage();

    let output = fixture.run(
        &[FOREIGN_ORIGIN],
        &frame(&json!({"protocol": 1, "type": "hello"})),
    );
    assert_ne!(exit_code(&output), 0, "a foreign origin must not be served");
    assert!(
        output.stdout.is_empty(),
        "a refused launch must leave stdout byte-empty, got {:?}",
        output.stdout
    );
    assert!(
        !output.stderr.is_empty(),
        "a refused launch must say why on stderr"
    );
}

#[test]
fn a_foreign_firefox_addon_id_is_refused_the_same_way() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let manifest = fixture.home.join("com.chat_stasher.host.json");

    let output = fixture.run(
        &[
            manifest.to_str().expect("utf-8 path"),
            "some-other@addon.example",
        ],
        &frame(&json!({"protocol": 1, "type": "hello"})),
    );
    assert_ne!(exit_code(&output), 0);
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
    assert!(!output.stderr.is_empty());
}

#[test]
fn an_ordinary_command_line_still_goes_through_the_cli() {
    // The recognition must not swallow commands: `--self-test` is a normal
    // subcommand and still reaches clap. It prints one plain JSON line — not a
    // frame — which is the unchanged self-test contract.
    let fixture = Fixture::new();
    let output = fixture.run(&["native-host", "--self-test"], b"");
    assert_eq!(exit_code(&output), 0);
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(text.lines().count(), 1, "stdout is not one line: {text:?}");
    let value: Value = serde_json::from_str(text.trim_end()).expect("self-test line is JSON");
    assert_eq!(value["mode"], "self-test");
}

// ------------------------------------------------------------------ framing

#[test]
fn an_over_the_cap_prefix_is_nacked_without_a_body() {
    let fixture = Fixture::new();
    fixture.configure_stage();

    // Only the prefix is sent and stdin is then closed. If the host tried to
    // read the body it would see EOF and answer nothing at all, so receiving a
    // `too-large` nack *is* the proof that the body was never read.
    let prefix = ((chat_stasher::nativehost::MAX_REQUEST_BYTES + 1) as u32).to_ne_bytes();
    let output = fixture.chrome(&prefix);
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "nack");
    assert_eq!(response["kind"], "too-large");
    assert_eq!(response["retryable"], false);
    assert!(response["request_id"].is_null());
}

#[test]
fn a_frame_that_ends_early_writes_nothing_and_exits_non_zero() {
    let fixture = Fixture::new();
    fixture.configure_stage();

    // A header that stops after two bytes...
    let output = fixture.chrome(&[0x10, 0x00]);
    assert_ne!(exit_code(&output), 0);
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);

    // ...and a body that stops before its declared length.
    let mut truncated = frame(&json!({"protocol": 1, "type": "hello"}));
    truncated.truncate(truncated.len() - 3);
    let output = fixture.chrome(&truncated);
    assert_ne!(exit_code(&output), 0);
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
    assert!(
        !output.stderr.is_empty(),
        "an unreadable frame must be reported on stderr"
    );
}

// --------------------------------------------------------------- concurrency

#[test]
fn eight_concurrent_hosts_for_one_session_seal_eight_distinct_shards() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);

    let mut payloads = Vec::new();
    let mut children = Vec::new();
    for index in 0..8u32 {
        let text = format!("concurrent payload {index}");
        let payload = bundle("sess-race", &text);
        payloads.push(payload.clone());
        let request = deliver_request(
            &format!("req-{index}"),
            &format!("deepseek-sess-race-{index}.json"),
            &payload,
        );

        // Spawned, fed and *not* waited for: all eight are in flight at once,
        // which is the only way this exercises the stage lock.
        let mut child = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
            .arg(CHROME_ORIGIN)
            .env("HOME", &fixture.home)
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&fixture.home),
            )
            .env("XDG_CONFIG_HOME", fixture.home.join("config"))
            .env("XDG_DATA_HOME", fixture.home.join("data"))
            .env("XDG_STATE_HOME", fixture.home.join("state"))
            .env("XDG_CACHE_HOME", fixture.home.join("cache"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn host");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(&frame(&request))
            .expect("write frame");
        children.push(child);
    }

    let mut shards = Vec::new();
    for (index, child) in children.into_iter().enumerate() {
        let output = child.wait_with_output().expect("wait");
        assert_eq!(
            exit_code(&output),
            0,
            "host {index} failed; stderr: {}",
            stderr_of(&output)
        );
        let response = one_frame(&output.stdout);
        assert_matches_schema(&response);
        assert_eq!(
            response["status"], "stored",
            "host {index} did not store: {response}"
        );
        assert_eq!(
            response["sha256"],
            sha256_hex(payloads[index].as_bytes()).as_str()
        );
        shards.push(response["shard"].as_str().expect("shard name").to_string());
    }

    let mut unique = shards.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        8,
        "two hosts picked the same shard name: {shards:?}"
    );

    // Every shard the acks named exists, and holds exactly the payload whose
    // hash that ack reported.
    let on_disk = fixture.shard_names(&machine, "deepseek.sess-race");
    assert_eq!(on_disk.len(), 8, "shards on disk: {on_disk:?}");
    for shard in &shards {
        assert!(
            on_disk.contains(shard),
            "ack named {shard}, which is not on disk: {on_disk:?}"
        );
    }
    let records = fixture.shard_records(&machine, "deepseek.sess-race");
    assert_eq!(records.len(), 8);
    for (index, payload) in payloads.iter().enumerate() {
        let sha = sha256_hex(payload.as_bytes());
        assert!(
            records
                .iter()
                .any(|record| record["file_sha256"] == sha.as_str()),
            "no shard record carries the hash of payload {index}"
        );
    }
}

#[test]
fn a_machine_without_an_identity_is_a_config_nack_and_no_identity_is_created() {
    let fixture = Fixture::new();
    let identity = fixture
        .home
        .join("data")
        .join("chat-stasher")
        .join("machine-identity");
    fs::remove_file(&identity).expect("drop the seeded identity");
    fixture.configure_stage();

    let out = fixture.chrome(&frame(&serde_json::json!({"protocol": 1, "type": "hello"})));
    let response = one_frame(&out.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "nack", "{response}");
    assert_eq!(response["kind"], "config", "{response}");
    assert_eq!(response["retryable"], false, "{response}");
    let detail = response["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("never creates"),
        "detail must say why: {detail}"
    );
    assert!(
        detail.contains("XDG_DATA_HOME"),
        "detail must name the likely cause: {detail}"
    );

    let response = deliver(&fixture, "r-no-id", "sess-no-identity", "hello");
    assert_matches_schema(&response);
    assert_eq!(response["kind"], "config", "{response}");

    assert!(
        !identity.exists(),
        "the host must not mint a machine identity"
    );
    assert!(
        !fixture.stage.join("sessions").exists(),
        "nothing may be written when the machine is unknown"
    );
}

// ------------------------------------------------------------------ §6.4 summary

impl Fixture {
    /// What `hello` reports for the machine, so a test never restates how the
    /// partition is built. Same helper the deliver tests use.
    fn machine(&self) -> String {
        first_machine(self)
    }

    /// Seed `run-state.json` the way `run-once` writes it: the most recent pass,
    /// with the outcome that decides whether a push time can be read off it.
    fn seed_run_state(&self, outcome: &str, finished_at_unix: u64) {
        let dir = self.home.join("data").join("chat-stasher").join("state");
        fs::create_dir_all(&dir).expect("state dir");
        let state = json!({
            "version": 1,
            "finished_at_unix": finished_at_unix,
            "duration_ms": 1200,
            "outcome": outcome,
            "failed_step": null,
            "shards_written": 0,
            "stage_shards": 0,
            "snapshot_created": outcome == "completed",
            "collect_errors": 0,
            "archive_gaps": 0,
            "machine_digest": "0123456789ab",
        });
        fs::write(
            dir.join("run-state.json"),
            serde_json::to_vec_pretty(&state).expect("state json"),
        )
        .expect("write run state");
    }

    /// Point `[native_host]` at this fixture's stage and name a destination.
    fn configure_dashboard(&self, destination: &str) {
        self.write_config(&format!(
            "[native_host]\nstage = {stage}\ndestination = {destination}\n\n\
             [destinations.laptop]\nrepo = {repo}\n\n\
             [destinations.storagebox]\nrepo = {repo}\n",
            stage = serde_json::to_string(&self.stage.to_string_lossy()).expect("stage"),
            destination =
                serde_json::to_string(destination).expect("a destination name is a TOML string"),
            repo = serde_json::to_string(&self.dir.path().join("nowhere").to_string_lossy())
                .expect("repo"),
        ));
    }

    fn summary(&self) -> Value {
        let out = self.chrome(&frame(&json!({"protocol": 1, "type": "summary"})));
        assert_eq!(exit_code(&out), 0, "stderr: {}", stderr_of(&out));
        let response = one_frame(&out.stdout);
        assert_matches_schema(&response);
        response
    }
}

#[test]
fn summary_counts_the_stage_by_harness_and_reports_a_recorded_push() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    deliver(&fixture, "r-1", "sess-a", "hello a");
    deliver(&fixture, "r-2", "sess-b", "hello b");
    const PUSH_AT: u64 = 1_760_000_000;
    fixture.seed_run_state("completed", PUSH_AT);

    let response = fixture.summary();
    assert_eq!(response["type"], "summary");
    assert_eq!(response["ok"], true);
    assert_eq!(response["window_hours"], 24);
    assert_eq!(response["complete"], true, "{response}");

    assert_eq!(
        response["sessions"]["total"],
        json!({"kind": "known", "count": 2}),
        "the two delivered sessions are the stage's whole content: {response}"
    );
    assert_eq!(
        response["sessions"]["last_24h"],
        json!({"kind": "known", "count": 2}),
        "both shards were written just now: {response}"
    );
    assert_eq!(
        response["last_push"],
        json!({"kind": "known", "unix": PUSH_AT})
    );

    let buckets = response["sessions"]["by_harness"]
        .as_array()
        .expect("by_harness is an array");
    assert_eq!(buckets.len(), 1, "{response}");
    assert_eq!(buckets[0]["harness"], "deepseek");
    assert_eq!(buckets[0]["total"]["count"], 2);
    assert_eq!(buckets[0]["last_24h"]["count"], 2);

    // The counts are the whole answer: no session id, no shard name, no path
    // but the stage `hello` already returns.
    let text = response.to_string();
    assert!(!text.contains("sess-a"), "{text}");
    assert!(!text.contains("sess-b"), "{text}");
    assert!(!text.contains("000001"), "{text}");
}

#[test]
fn summary_reports_an_unrecorded_push_as_unknown_and_not_as_a_time() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    deliver(&fixture, "r-1", "sess-a", "hello a");

    // No run-state.json at all: run-once has never completed here.
    let response = fixture.summary();
    assert_eq!(response["complete"], false, "{response}");
    let why = response["last_push"]["why"].as_str().unwrap_or_default();
    assert!(
        why.contains("no run-once pass has ever been recorded"),
        "{why}"
    );
    assert_eq!(
        response["sessions"]["total"],
        json!({"kind": "known", "count": 1}),
        "an unknown push time says nothing about the counts: {response}"
    );

    // A passing run that pushed nothing is also not a push time — the record
    // holds only the most recent pass, so the earlier one is simply not here.
    fixture.seed_run_state("noop", 1_760_000_000);
    let response = fixture.summary();
    assert_eq!(response["complete"], false, "{response}");
    let why = response["last_push"]["why"].as_str().unwrap_or_default();
    assert!(why.contains("no change"), "{why}");
    assert!(why.contains("only the most recent pass"), "{why}");
}

#[test]
fn summary_of_an_empty_stage_is_a_measured_zero() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    fixture.seed_run_state("completed", 1_760_000_000);

    // A stage that exists and has never received a bundle: the answer is zero,
    // and it is a measurement rather than an unknown.
    let response = fixture.summary();
    assert_eq!(response["complete"], true, "{response}");
    assert_eq!(
        response["sessions"]["total"],
        json!({"kind": "known", "count": 0})
    );
    assert_eq!(
        response["sessions"]["last_24h"],
        json!({"kind": "known", "count": 0})
    );
    assert_eq!(response["sessions"]["by_harness"], json!([]));
}

/// The honesty rule, through the real binary: a machine partition nobody can
/// list must never be counted as `0` sessions, and the reason must name where
/// the read stopped without naming a path inside the stage.
///
/// Unix-only, because "a directory that exists and cannot be read" is injected
/// with file permissions and Windows has no standard-API equivalent. The
/// property itself is pinned on every platform by
/// `nativehost::tests::an_unreadable_partition_is_unknown_never_zero`, which
/// feeds `build_summary` a scan directly; this test is the filesystem half.
#[cfg(unix)]
#[test]
fn summary_reports_an_unreadable_partition_as_unknown_never_zero() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fixture.configure_stage();
    deliver(&fixture, "r-1", "sess-a", "hello a");

    let locked = fixture
        .stage
        .join("sessions")
        .join("ffffffffffffffffffffffffffffffff");
    fs::create_dir_all(&locked).expect("a second machine partition");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("lock the partition");
    if fs::read_dir(&locked).is_ok() {
        eprintln!("w30: this sandbox cannot make a directory unreadable (root?), case skipped");
        return;
    }

    let response = fixture.summary();
    assert_eq!(response["complete"], false, "{response}");
    assert_eq!(
        response["sessions"]["total"]["kind"], "unknown",
        "a partition that could not be listed must not become a number: {response}"
    );
    let why = response["sessions"]["total"]["why"]
        .as_str()
        .unwrap_or_default();
    assert!(why.contains("lower bound"), "{why}");
    let fingerprint = chat_stasher::store::machine_fingerprint("ffffffffffffffffffffffffffffffff");
    assert!(
        why.contains(&fingerprint),
        "the reason names the partition: {why}"
    );
    assert_eq!(
        response["sessions"]["by_harness"],
        json!([]),
        "a split of a lower bound is not a measurement: {response}"
    );
}

#[test]
fn summary_refuses_a_request_with_a_field_it_does_not_define() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    deliver(&fixture, "r-1", "sess-a", "hello a");

    let response = one_frame(
        &fixture
            .chrome(&frame(
                &json!({"protocol": 1, "type": "summary", "machine": "somewhere"}),
            ))
            .stdout,
    );
    assert_matches_schema(&response);
    assert_eq!(response["type"], "nack", "{response}");
    assert_eq!(response["kind"], "bad-request", "{response}");
    assert_eq!(response["retryable"], false, "{response}");
    assert!(
        response["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("machine"),
        "the refusal names the field it will not take: {response}"
    );
}

#[test]
fn summary_without_a_configured_stage_is_the_same_refusal_hello_gives() {
    let fixture = Fixture::new();
    let response = fixture.summary();
    assert_eq!(response["type"], "nack", "{response}");
    assert_eq!(response["kind"], "config", "{response}");
    assert!(
        response["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("install-native-host"),
        "detail names the fix: {response}"
    );
}

// ------------------------------------------------------- §6.5 open_dashboard

#[test]
fn open_dashboard_without_a_configured_destination_is_refused_before_anything_starts() {
    let fixture = Fixture::new();
    fixture.configure_stage();

    let out = fixture.chrome(&frame(&json!({"protocol": 1, "type": "open_dashboard"})));
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr_of(&out));
    let response = one_frame(&out.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "nack", "{response}");
    assert_eq!(response["kind"], "config", "{response}");
    assert_eq!(response["retryable"], false, "{response}");
    let detail = response["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("[native_host] destination"), "{detail}");
    assert!(detail.contains("no default destination"), "{detail}");
}

// -------------------------------------------------------- EXT-3 coordination

#[test]
fn coordination_concurrent_first_claimants_have_one_winner() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let barrier = Arc::new(Barrier::new(2));
    let responses = thread::scope(|scope| {
        let mut workers = Vec::new();
        for (request_id, install_id) in [("race-a", "install-a"), ("race-b", "install-b")] {
            let barrier = Arc::clone(&barrier);
            let fixture = &fixture;
            workers.push(scope.spawn(move || {
                let input = frame(&json!({"protocol":1,"type":"coordination",
                    "request_id":request_id,"mode":"claim","platform":"chatgpt","install_id":install_id}));
                barrier.wait();
                let out = fixture.chrome(&input);
                assert_eq!(exit_code(&out), 0, "stderr: {}", stderr_of(&out));
                let response = one_frame(&out.stdout);
                assert_matches_schema(&response);
                response
            }));
        }
        workers
            .into_iter()
            .map(|worker| worker.join().expect("claim process joins"))
            .collect::<Vec<_>>()
    });
    assert_eq!(
        responses
            .iter()
            .filter(|reply| reply["granted"] == true)
            .count(),
        1,
        "simultaneous processes cannot both own the first lease: {responses:?}"
    );
}

#[test]
fn coordination_serializes_installs_propagates_cooldown_and_expires_leases() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let ask = |request_id: &str, mode: &str, install_id: &str| {
        let out = fixture.chrome(&frame(&json!({"protocol":1,"type":"coordination",
            "request_id":request_id,"mode":mode,"platform":"chatgpt","install_id":install_id})));
        assert_eq!(exit_code(&out), 0, "stderr: {}", stderr_of(&out));
        let response = one_frame(&out.stdout);
        assert_matches_schema(&response);
        response
    };
    let first = ask("claim-a", "claim", "install-a");
    assert_eq!(first["granted"], true);
    let contender = ask("claim-b", "claim", "install-b");
    assert_eq!(contender["granted"], false);
    assert_eq!(contender["active_installs"], 2);
    assert_eq!(contender["gentle"], true);

    let db = fixture
        .home
        .join("data/chat-stasher/state/extension-coordination.sqlite3");
    let conn = rusqlite::Connection::open(db).expect("coordination database exists");
    conn.execute(
        "UPDATE ext_platform_v2 SET lease_until=0 WHERE platform='chatgpt' AND account_key=''",
        [],
    )
    .expect("expire lease");
    drop(conn);
    let after_expiry = ask("claim-c", "claim", "install-b");
    assert_eq!(
        after_expiry["granted"], true,
        "expired lease can be claimed"
    );

    // Native messaging starts a process per request. Bound the host's clock by
    // the call boundaries instead of assuming startup takes less than a second.
    // The cooldown is a wall-clock deadline, and each of the two requests below
    // is answered by a *separate* host process that reads the clock again
    // (`nativehost.rs`: `wait = cooldown_until - now`). Bound both the deadline
    // and the wall-clock elapsed time.
    let cooldown_armed = Instant::now();
    let rate_started = chrono::Utc::now().timestamp_millis();
    let limited = fixture.chrome(&frame(&json!({"protocol":1,"type":"coordination",
        "request_id":"rate-a","mode":"rate_limit","platform":"chatgpt","install_id":"install-b",
        "status":429,"retry_after_ms":300_000})));
    let rate_finished = chrono::Utc::now().timestamp_millis();
    assert_eq!(exit_code(&limited), 0, "stderr: {}", stderr_of(&limited));
    let limited = one_frame(&limited.stdout);
    assert_matches_schema(&limited);
    assert_eq!(
        limited["wait_ms"], 300_000,
        "the acknowledged cooldown is the header it was given: {limited}"
    );
    let deadline = limited["cooldown_until"]
        .as_i64()
        .expect("cooldown deadline");
    assert!(
        (rate_started + 300_000..=rate_finished + 300_000).contains(&deadline),
        "Retry-After sets a full 300s cooldown: {limited}"
    );
    let token_started = chrono::Utc::now().timestamp_millis();
    let blocked = fixture.chrome(&frame(&json!({"protocol":1,"type":"coordination",
        "request_id":"token-a","mode":"token","platform":"chatgpt","install_id":"install-b","segment":"detail"})));
    let elapsed = cooldown_armed.elapsed().as_millis() as i64;
    let token_finished = chrono::Utc::now().timestamp_millis();
    assert_eq!(exit_code(&blocked), 0, "stderr: {}", stderr_of(&blocked));
    let blocked = one_frame(&blocked.stdout);
    assert_matches_schema(&blocked);
    assert_eq!(blocked["granted"], false);
    assert_eq!(
        blocked["cooldown_until"], limited["cooldown_until"],
        "the cooldown reaches every install as the same deadline: {blocked}"
    );
    let wait = blocked["wait_ms"].as_i64().expect("remaining cooldown");
    assert!(
        ((deadline - token_finished).max(0)..=(deadline - token_started).max(0)).contains(&wait),
        "cooldown reaches every install with the remaining wait: {blocked}"
    );
    assert!(
        wait <= 300_000,
        "a wait can only be the cooldown that is left, never longer: {blocked}"
    );
    assert!(
        wait + elapsed >= 300_000,
        "cooldown reaches every install: {blocked}; {wait}ms left after {elapsed}ms \
         of test wall clock is short of the 300000ms header"
    );
}

#[test]
fn coordination_rate_limit_keeps_owner_until_last_holder_releases() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let send = |request_id: &str, mode: &str, install_id: &str| {
        let out = fixture.chrome(&frame(&json!({"protocol":1,"type":"coordination",
            "request_id":request_id,"mode":mode,"platform":"claude","install_id":install_id})));
        assert_eq!(exit_code(&out), 0, "stderr: {}", stderr_of(&out));
        one_frame(&out.stdout)
    };
    assert_eq!(send("claim", "claim", "install-a")["granted"], true);
    let out = fixture.chrome(&frame(&json!({"protocol":1,"type":"coordination",
        "request_id":"limited","mode":"rate_limit","platform":"claude","install_id":"install-a",
        "status":429,"retry_after_ms":120_000})));
    assert_eq!(exit_code(&out), 0, "stderr: {}", stderr_of(&out));
    let db = fixture
        .home
        .join("data/chat-stasher/state/extension-coordination.sqlite3");
    let conn = rusqlite::Connection::open(db).expect("coordination database exists");
    let owner: Option<String> = conn
        .query_row(
            "SELECT owner FROM ext_platform_v2 WHERE platform='claude' AND account_key=''",
            [],
            |row| row.get(0),
        )
        .expect("platform row exists");
    assert_eq!(owner.as_deref(), Some("install-a"));
    assert_eq!(send("release", "release", "install-a")["granted"], true);
    let owner: Option<String> = conn
        .query_row(
            "SELECT owner FROM ext_platform_v2 WHERE platform='claude' AND account_key=''",
            [],
            |row| row.get(0),
        )
        .expect("platform row exists after release");
    assert_eq!(owner, None);
}

#[test]
fn account_id_messages_resolve_masterkey_scope_for_coordination_and_delivery() {
    let fixture = Fixture::new();
    fixture.configure_stage_with_masterkey();
    let account_a = "synthetic-account-a";
    let account_b = "synthetic-account-b";
    let ask = |request_id: &str, mode: &str, install_id: &str, account_id: &str| {
        let out = fixture.chrome(&frame(&json!({"protocol":1,"type":"coordination",
            "request_id":request_id,"mode":mode,"platform":"deepseek","install_id":install_id,
            "account_id":account_id,"status":429,"segment":"detail"})));
        assert_eq!(exit_code(&out), 0, "stderr: {}", stderr_of(&out));
        let response = one_frame(&out.stdout);
        assert_matches_schema(&response);
        response
    };

    assert_eq!(
        ask("claim-a1", "claim", "install-a1", account_a)["granted"],
        true
    );
    let same_account = ask("claim-a2", "claim", "install-a2", account_a);
    assert_eq!(same_account["granted"], false);
    assert_eq!(same_account["active_installs"], 2);
    assert_eq!(
        ask("claim-b1", "claim", "install-b1", account_b)["granted"],
        true
    );

    let rate_limit = ask("limit-a", "rate_limit", "install-a1", account_a);
    assert_eq!(rate_limit["wait_ms"], 60_000);
    assert_eq!(
        ask("token-a", "token", "install-a1", account_a)["granted"],
        false
    );
    assert_eq!(
        ask("token-b", "token", "install-b1", account_b)["granted"],
        true
    );

    let db = fixture
        .home
        .join("data/chat-stasher/state/extension-coordination.sqlite3");
    let conn = rusqlite::Connection::open(&db).expect("coordination database exists");
    let keys = conn
        .prepare("SELECT DISTINCT account_key FROM ext_platform_v2 WHERE platform='deepseek'")
        .expect("prepare account scopes")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query account scopes")
        .collect::<Result<Vec<_>, _>>()
        .expect("read account scopes");
    assert_eq!(keys.len(), 2, "only the two account scopes are stored");
    assert!(keys
        .iter()
        .all(|key| { key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()) }));
    assert_ne!(keys[0], keys[1]);
    drop(conn);
    let database_bytes = fs::read(&db).expect("read isolated coordination database");
    assert!(!database_bytes
        .windows(account_a.len())
        .any(|window| window == account_a.as_bytes()));
    assert!(!database_bytes
        .windows(account_b.len())
        .any(|window| window == account_b.as_bytes()));

    let payload = bundle("sess-account-key", "synthetic delivery body");
    let delivery = fixture.chrome(&frame(&deliver_request_with_account_id(
        "deliver-account-key",
        "deepseek-sess-account-key.json",
        &payload,
        account_a,
    )));
    assert_eq!(exit_code(&delivery), 0, "stderr: {}", stderr_of(&delivery));
    let ack = one_frame(&delivery.stdout);
    assert_matches_schema(&ack);
    assert_eq!(ack["status"], "stored");
    let machine = first_machine(&fixture);
    let record = fixture.shard_records(&machine, "deepseek.sess-account-key");
    assert_eq!(record.len(), 1);
    let sealed_key = record[0]["account_key"]
        .as_str()
        .expect("sealed account key");
    assert_eq!(sealed_key.len(), 64);
    assert!(keys.contains(&sealed_key.to_string()));
    let serialized_record = serde_json::to_vec(&record[0]).expect("serialize sealed record");
    assert!(!serialized_record
        .windows(account_a.len())
        .any(|window| window == account_a.as_bytes()));
    assert!(!serialized_record
        .windows(account_b.len())
        .any(|window| window == account_b.as_bytes()));
}

#[test]
fn open_dashboard_naming_an_undeclared_destination_lists_what_is_declared() {
    let fixture = Fixture::new();
    fixture.configure_dashboard("elsewhere");

    let out = fixture.chrome(&frame(&json!({"protocol": 1, "type": "open_dashboard"})));
    let response = one_frame(&out.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["kind"], "config", "{response}");
    let detail = response["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("elsewhere"), "{detail}");
    assert!(
        detail.contains("laptop"),
        "declared names are listed: {detail}"
    );
    assert!(detail.contains("storagebox"), "{detail}");
}

#[test]
fn open_dashboard_that_cannot_read_the_archive_reports_why_without_naming_the_repository() {
    let fixture = Fixture::new();
    fixture.configure_dashboard("laptop");

    // The destination is declared and its key file does not exist, so the real
    // `chat-stasher ui` starts, fails to read anything, and exits 3 without
    // ever binding a socket. The host must report that as a nack — and must not
    // relay the child's stderr, which would carry the repository path.
    let out = fixture.chrome(&frame(&json!({"protocol": 1, "type": "open_dashboard"})));
    let response = one_frame(&out.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "nack", "{response}");
    assert_eq!(response["kind"], "io", "{response}");
    assert_eq!(response["retryable"], true, "{response}");
    let detail = response["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("did not start"), "{detail}");
    assert!(
        detail.contains("exited 3"),
        "the exit status is reported: {detail}"
    );
    assert!(
        !detail.contains(&fixture.dir.path().to_string_lossy().into_owned()),
        "no path from the child's own output may reach the extension: {detail}"
    );
}

#[test]
fn the_shapes_the_new_queries_produce_match_the_committed_schema() {
    use chat_stasher::json_out::{CountState, TimeState};
    use chat_stasher::nativehost::{build_summary, dashboard_response, StageScan, StageSession};

    // `open_dashboard`'s success response cannot be reached end to end without a
    // real repository (the launch itself is pinned by the stubbed-spawn unit
    // tests), so its shape is checked here against the committed schema rather
    // than against a hand-copied list of fields.
    assert_matches_schema(&dashboard_response(
        "http://127.0.0.1:51234/?token=abababababababababababababababababababababababababababababababab",
    ));

    // The same for the two states of a `summary` that a fixture stage cannot
    // produce: everything measured, and everything unreadable.
    let known = StageScan {
        sessions: vec![
            StageSession {
                harness: Some("deepseek".to_string()),
                session_id: "deepseek.synthetic-1".to_string(),
                newest_mtime: Some(1_760_000_000),
            },
            StageSession {
                harness: None,
                session_id: String::new(),
                newest_mtime: None,
            },
        ],
        unreadable: Vec::new(),
        time_unknown: Vec::new(),
    };
    assert_matches_schema(&build_summary(
        &known,
        TimeState::known(1_759_000_000),
        24,
        Ok(1_760_000_001),
    ));

    let unreadable = StageScan {
        sessions: Vec::new(),
        unreadable: vec!["a machine partition could not be listed: Permission denied".to_string()],
        time_unknown: Vec::new(),
    };
    let summary = build_summary(
        &unreadable,
        TimeState::unknown("no run-once pass has ever been recorded"),
        24,
        Ok(1_760_000_001),
    );
    assert_eq!(
        summary["sessions"]["total"]["kind"], "unknown",
        "sanity: this really is the unknown shape: {summary}"
    );
    assert_eq!(
        summary["sessions"]["total"],
        serde_json::to_value(CountState::unknown(
            summary["sessions"]["total"]["why"].as_str().expect("a why"),
        ))
        .expect("the unknown shape serialises"),
        "the wire shape is json_out::CountState's, verbatim: {summary}"
    );
    assert_matches_schema(&summary);
}

// ---------------------------------------------------------------- W248 / EXT-13
//
// ADR-045 (accepted 2026-09-29): an `install_id` names an installation
// *lineage*, not a live browser profile, and a copied profile carries the same
// `install_id` with no other field that could tell the two apart. The
// host-observable signal is the **sequence and its nonce**: each instance keeps
// a monotonic `report_seq`, incremented for every status report and every
// capture delivery, and mints a fresh random `report_nonce` with each sequence
// and persists the pair. Two live writers on one `(machine, install_id)`
// eventually *allocate the same number*, and a sequence the host has already
// recorded with one nonce arriving under another is two allocations of one
// number — which no single writer can produce.
//
// The CTO decision of 2026-09-29 replaced an earlier rule that read a
// *regression* as the signal. Reports legitimately arrive out of order, a send
// that times out is retried, and a service worker restarts between allocating a
// sequence and sending it — none of which a second writer is required to
// explain, so the old rule accused honest installs. `report_seq` in the tests
// below is therefore never compared as a number on its own.
//
// These tests pin the host behaviours the ADR makes load-bearing:
// ① status is stored per `(machine, install_id)` and the legacy flat
//    `ext-status/<install_id>.json` is read as a legacy observation of this
//    machine and never lost; ② one sequence under two different nonces marks
//    the id `identity-conflict`, while a repeat of the *same* pair, a higher
//    sequence, an out-of-order one, and one the host no longer retains are all
//    accepted; ③ a conflicted id claims no lease and delivers nothing
//    **retryably** — the item stays queued instead of being dropped or
//    rejected. An extension that sends no `report_seq` stays accepted as
//    "seq unknown" and can neither create nor clear a conflict.

/// Read one status record the host persisted, from either storage shape.
fn read_status_record(stage: &Path, machine: &str, install_id: &str) -> Value {
    let keyed = stage
        .join("ext-status")
        .join(machine)
        .join(format!("{install_id}.json"));
    let raw =
        fs::read(&keyed).unwrap_or_else(|e| panic!("read keyed status {}: {e}", keyed.display()));
    serde_json::from_slice(&raw).expect("keyed status is valid JSON")
}

/// A `status` request shaped like `reportInstallStatus` serialises it.
///
/// `stamp` is the `(report_seq, report_nonce)` pair the extension allocates as
/// one unit; `None` is an extension older than both fields, which sends
/// neither.
fn status_request(
    request_id: &str,
    install_id: &str,
    reported_at: &str,
    stamp: Option<(u64, &str)>,
) -> Value {
    let mut status = json!({
        "install_id": install_id,
        "browser": "Chrome",
        "profile_label": "Personal",
        "extension_version": "0.4.0",
        "reported_at": reported_at,
        "platforms": [{
            "platform": "chatgpt",
            "captured_by_this_browser": 3,
            "pending": 1,
            "paused_reason": null,
        }],
    });
    if let Some((seq, nonce)) = stamp {
        status["report_seq"] = json!(seq);
        status["report_nonce"] = json!(nonce);
    }
    json!({"protocol": 1, "type": "status", "request_id": request_id, "status": status})
}

fn report_status(
    fixture: &Fixture,
    request_id: &str,
    install_id: &str,
    at: &str,
    stamp: Option<(u64, &str)>,
) -> Value {
    let output = fixture.chrome(&frame(&status_request(request_id, install_id, at, stamp)));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    response
}

/// Ask the host what it knows about one install, from the read-only query the
/// popup uses. The status *record* is what the dashboard reads; this is what a
/// popup can ask at any moment, including on an install whose backfill is off
/// and which therefore never reports.
fn identity_state(fixture: &Fixture, request_id: &str, install_id: &str) -> Value {
    let response = one_frame(
        &fixture
            .chrome(&frame(&json!({
                "protocol": 1, "type": "identity_state",
                "request_id": request_id, "install_id": install_id,
            })))
            .stdout,
    );
    assert_matches_schema(&response);
    response
}

/// `true` when the host reports the id shared, asserted so a test cannot read a
/// missing field as a quiet `false`.
fn is_conflicted(fixture: &Fixture, request_id: &str, install_id: &str) -> bool {
    let state = identity_state(fixture, request_id, install_id);
    assert_eq!(state["ok"], true, "{state}");
    state["identity_conflict"]
        .as_bool()
        .unwrap_or_else(|| panic!("identity_conflict is not a boolean: {state}"))
}

/// ② Two copies allocate the same number and mint different nonces, which is
/// the only positive evidence of cloning the protocol has. It must mark the id,
/// put the evidence on the record, and keep the *whole* id out of the archive.
#[test]
fn one_sequence_under_two_nonces_marks_the_install_identity_conflicted() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);
    let install = "33333333-3333-4333-8333-333333333333";

    let first = report_status(
        &fixture,
        "w248-1",
        install,
        "2026-09-29T10:00:00Z",
        Some((10, "nonce-original")),
    );
    assert_eq!(first["type"], "status");
    assert!(
        !is_conflicted(&fixture, "w248-a", install),
        "one writer is not a conflict"
    );
    assert_eq!(
        read_status_record(&fixture.stage, &machine, install)["report_seq"],
        10
    );

    // The copy: same `storage.local`, so it starts from the same counter and
    // mints a nonce of its own for the number it allocates.
    report_status(
        &fixture,
        "w248-2",
        install,
        "2026-09-29T10:05:00Z",
        Some((10, "nonce-copy")),
    );
    assert!(
        is_conflicted(&fixture, "w248-b", install),
        "one sequence under two nonces is proof that two writers share this install id"
    );
    let record = read_status_record(&fixture.stage, &machine, install);
    assert_eq!(record["identity_conflict"], true);
    assert_eq!(
        record["report_seq"], 10,
        "the staged record is a report and carries what that report said; the \
         host's own record of it lives in its state, not in the report"
    );
    assert!(
        record.get("identity_conflict_evidence").is_some(),
        "the record says which observation set the flag: {record}"
    );
}

/// ② The rule is about nonces, and **not** about order. A late arrival, a
/// retried send, and an out-of-order delivery all carry sequences of their own,
/// and every one of them used to be read as a copy. This is the regression the
/// CTO decision replaced.
#[test]
fn distinct_sequences_that_arrive_out_of_order_are_not_a_conflict() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let install = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";

    // Ascending first, so the host has a record to regress against.
    report_status(
        &fixture,
        "w248-1",
        install,
        "2026-09-29T10:00:00Z",
        Some((5, "nonce-5")),
    );
    report_status(
        &fixture,
        "w248-2",
        install,
        "2026-09-29T10:01:00Z",
        Some((6, "nonce-6")),
    );
    // Now a frame the host never saw arrives after them — the first send failed
    // at the transport, the retry went out with a fresh allocation, and the
    // original then turns up late.
    report_status(
        &fixture,
        "w248-3",
        install,
        "2026-09-29T10:02:00Z",
        Some((4, "nonce-4")),
    );
    assert!(
        !is_conflicted(&fixture, "w248-a", install),
        "a lower sequence the host has never seen is not a second allocation"
    );
    // And a restarted worker resuming past the value it had already reserved.
    report_status(
        &fixture,
        "w248-4",
        install,
        "2026-09-29T10:03:00Z",
        Some((7, "nonce-7")),
    );
    assert!(
        !is_conflicted(&fixture, "w248-b", install),
        "the window is what a repeat is compared against, not the highest value"
    );
}

/// ② The same pair again is one allocation arriving twice — a replayed frame, a
/// retry whose response was lost — and it is answered as the duplicate it is
/// rather than as a second writer.
#[test]
fn the_same_sequence_and_nonce_again_is_an_idempotent_retry() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let install = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";

    let first = report_status(
        &fixture,
        "w248-1",
        install,
        "2026-09-29T10:00:00Z",
        Some((9, "nonce-9")),
    );
    assert_eq!(first["type"], "status");
    let replayed = report_status(
        &fixture,
        "w248-2",
        install,
        "2026-09-29T10:05:00Z",
        Some((9, "nonce-9")),
    );
    assert_eq!(
        replayed["type"], "status",
        "the retry is accepted, not refused: {replayed}"
    );
    assert!(
        !is_conflicted(&fixture, "w248-a", install),
        "one allocation seen twice is not two writers"
    );
}

/// ② The window is bounded, and a sequence older than everything it retains is
/// accepted **without judgement** — the host has no recorded nonce to compare
/// it against, and "we cannot tell" must not be reported as "we checked". This
/// pins both the bound and what happens past it.
#[test]
fn a_sequence_below_the_retained_window_is_accepted_without_judgement() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let install = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";

    // More allocations than the window keeps, so sequence 1 is forgotten.
    let newest = IDENTITY_SEQ_WINDOW as u64 + 5;
    for seq in 1..=newest {
        report_status(
            &fixture,
            &format!("w248-fill-{seq}"),
            install,
            "2026-09-29T10:00:00Z",
            Some((seq, "nonce-fill")),
        );
    }
    assert!(
        !is_conflicted(&fixture, "w248-a", install),
        "one writer filling its own window is not a conflict"
    );
    // A different nonce for a sequence the host no longer retains is *not*
    // judged: it is below the window, which the contract says is accepted.
    report_status(
        &fixture,
        "w248-b",
        install,
        "2026-09-29T10:10:00Z",
        Some((1, "nonce-someone-else")),
    );
    assert!(
        !is_conflicted(&fixture, "w248-c", install),
        "a sequence below the retained window cannot be compared, so it must not be judged"
    );
    // A sequence the window *does* retain is judged, and this is the same
    // observation that would have been a conflict above the bound.
    report_status(
        &fixture,
        "w248-d",
        install,
        "2026-09-29T10:11:00Z",
        Some((newest, "nonce-someone-else")),
    );
    assert!(
        is_conflicted(&fixture, "w248-d2", install),
        "the retained window is what a repeat is judged against"
    );
}

/// ③ While an id is conflicted, neither copy can archive: delivery is refused
/// with an item-scope *retryable* `identity-conflict`, so the capture stays in
/// the outbox rather than being dropped or marked rejected. The lease is
/// refused too, because backfill is the only bulk-request path.
#[test]
fn a_conflicted_install_neither_seals_nor_claims_the_lease() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);
    let install = "44444444-4444-4444-8444-444444444444";

    report_status(
        &fixture,
        "w248-1",
        install,
        "2026-09-29T10:00:00Z",
        Some((10, "nonce-original")),
    );
    report_status(
        &fixture,
        "w248-2",
        install,
        "2026-09-29T10:05:00Z",
        Some((10, "nonce-copy")),
    );
    assert!(is_conflicted(&fixture, "w248-a", install));

    let payload = identity_bundle("sess-a", "hello a", install, "Chrome", "Personal");
    let output = fixture.chrome(&frame(&deliver_request(
        "w248-deliver",
        "deepseek-sess-a.json",
        &payload,
    )));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "nack");
    assert_eq!(
        response["kind"], "identity-conflict",
        "the refusal has to be nameable: {response}"
    );
    assert_eq!(
        response["retryable"], true,
        "a conflict needs a person, but the bytes must stay queued for when they act"
    );
    assert!(
        !fixture.session_dir(&machine, "deepseek.sess-a").exists(),
        "a conflicted install must seal nothing"
    );

    let claim = one_frame(
        &fixture
            .chrome(&frame(&json!({
                "protocol": 1, "type": "coordination", "request_id": "w248-claim",
                "mode": "claim", "platform": "chatgpt", "install_id": install,
            })))
            .stdout,
    );
    assert_matches_schema(&claim);
    assert_eq!(claim["type"], "nack");
    assert_eq!(claim["kind"], "identity-conflict");
}

/// ③ A **delivery** is wire traffic too, and for an install whose backfill is
/// switched off it is the only wire traffic there is: that install never runs a
/// tick, so it never sends a status report. Two such copies must still be told
/// apart, and neither copy's bytes may be filed under an id that has just been
/// proved ambiguous.
#[test]
fn a_delivery_can_establish_the_conflict_it_is_then_refused_by() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);
    let install = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";

    let original = identity_bundle("sess-a", "hello a", install, "Chrome", "Personal");
    let mut request = deliver_request("w248-1", "deepseek-sess-a.json", &original);
    request["report_seq"] = json!(3);
    request["report_nonce"] = json!("nonce-original");
    let output = fixture.chrome(&frame(&request));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    assert_eq!(one_frame(&output.stdout)["type"], "ack");
    assert!(fixture.session_dir(&machine, "deepseek.sess-a").exists());
    assert!(!is_conflicted(&fixture, "w248-state-1", install));

    // The copy's delivery is what proves the id is shared — and it is refused on
    // that same observation, so nothing of its bytes reaches the stage.
    let copy = identity_bundle("sess-b", "hello b", install, "Chrome", "Personal");
    let mut request = deliver_request("w248-2", "deepseek-sess-b.json", &copy);
    request["report_seq"] = json!(3);
    request["report_nonce"] = json!("nonce-copy");
    let output = fixture.chrome(&frame(&request));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "nack", "{response}");
    assert_eq!(response["kind"], "identity-conflict", "{response}");
    assert_eq!(response["retryable"], true, "{response}");
    assert!(
        !fixture.session_dir(&machine, "deepseek.sess-b").exists(),
        "the delivery that established the conflict must not also archive"
    );
    assert!(is_conflicted(&fixture, "w248-state-2", install));
}

/// ② The flag is sticky: once the id is known to be shared, a later
/// well-ordered report from the *other* writer must not clear it. Otherwise the
/// first copy to report a higher value would silently re-legitimise the id.
#[test]
fn a_higher_report_seq_does_not_forget_a_known_conflict() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);
    let install = "55555555-5555-4555-8555-555555555555";

    report_status(
        &fixture,
        "w248-1",
        install,
        "2026-09-29T10:00:00Z",
        Some((10, "nonce-original")),
    );
    report_status(
        &fixture,
        "w248-2",
        install,
        "2026-09-29T10:05:00Z",
        Some((10, "nonce-copy")),
    );
    report_status(
        &fixture,
        "w248-3",
        install,
        "2026-09-29T10:10:00Z",
        Some((11, "nonce-original")),
    );
    assert!(is_conflicted(&fixture, "w248-a", install));
    assert_eq!(
        read_status_record(&fixture.stage, &machine, install)["identity_conflict"],
        true
    );
}

/// ② An extension that predates `report_seq` stays accepted, as "seq unknown".
/// Unknown is not a value: it can neither manufacture a conflict from a single
/// writer nor erase one that a sequence already proved — and it cannot arrive
/// as a nonce with no sequence, which is not an unknown but a broken message.
#[test]
fn an_extension_without_report_seq_is_accepted_as_seq_unknown() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);
    let install = "66666666-6666-4666-8666-666666666666";

    let first = report_status(&fixture, "w248-1", install, "2026-09-29T10:00:00Z", None);
    assert_eq!(first["type"], "status");
    let record = read_status_record(&fixture.stage, &machine, install);
    assert!(
        record.get("report_seq").is_none(),
        "an unrecorded sequence is absent, never zero: {record}"
    );
    assert!(!is_conflicted(&fixture, "w248-a", install));

    // A second report with no sequence is not a repeat: nothing was counted,
    // and nothing was compared.
    report_status(&fixture, "w248-2", install, "2026-09-29T10:30:00Z", None);
    assert!(!is_conflicted(&fixture, "w248-b", install));

    // A nonce on its own names no allocation. It is a malformed message rather
    // than the "unknown" the host is willing to record.
    let mut request = status_request("w248-3", install, "2026-09-29T10:40:00Z", None);
    request["status"]["report_nonce"] = json!("nonce-with-no-sequence");
    let output = fixture.chrome(&frame(&request));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(response["type"], "nack", "{response}");
    assert_eq!(response["kind"], "bad-request", "{response}");
    assert!(!is_conflicted(&fixture, "w248-b2", install));

    // A collision still marks it, and an old extension reporting afterwards must
    // not clear the flag it cannot see.
    report_status(
        &fixture,
        "w248-4",
        install,
        "2026-09-29T11:00:00Z",
        Some((10, "nonce-original")),
    );
    report_status(
        &fixture,
        "w248-5",
        install,
        "2026-09-29T11:05:00Z",
        Some((10, "nonce-copy")),
    );
    report_status(&fixture, "w248-6", install, "2026-09-29T11:10:00Z", None);
    assert!(is_conflicted(&fixture, "w248-c", install));
}

/// ① Status is stored under `(machine, install_id)`, and a legacy flat
/// `ext-status/<install_id>.json` is read as this machine's earlier observation
/// of the same install: its daily-report history carries into the keyed record,
/// and the legacy file itself is left where it is. A migration that deleted it
/// would destroy the only local copy of a report whose successor may never be
/// written (the stage is not the archive, and nothing here is re-derivable).
#[test]
fn legacy_status_is_migrated_as_a_legacy_observation_and_never_deleted() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let machine = first_machine(&fixture);
    let install = "77777777-7777-4777-8777-777777777777";

    let legacy_dir = fixture.stage.join("ext-status");
    fs::create_dir_all(&legacy_dir).expect("legacy status dir");
    let legacy_path = legacy_dir.join(format!("{install}.json"));
    let legacy = json!({
        "install_id": install,
        "browser": "Chrome",
        "profile_label": "Personal",
        "extension_version": "0.4.0",
        "reported_at": "2026-09-28T10:00:00Z",
        "machine": machine,
        "schema": "chat-stasher/ext-status@1",
        "daily_report_streak": 4,
        "reported_daily": true,
        "platforms": [{"platform": "chatgpt", "captured_by_this_browser": 1, "pending": 2, "paused_reason": null}],
    });
    let legacy_bytes = serde_json::to_vec(&legacy).expect("legacy as bytes");
    fs::write(&legacy_path, &legacy_bytes).expect("write legacy status");

    // Twenty hours later — inside the 18–30h band, so the legacy streak is
    // *advanced* rather than inherited unchanged. A record with no predecessor
    // would read 1 here, which is what makes this number evidence.
    report_status(
        &fixture,
        "w248-1",
        install,
        "2026-09-29T06:00:00Z",
        Some((1, "nonce-legacy")),
    );

    let record = read_status_record(&fixture.stage, &machine, install);
    assert_eq!(
        record["daily_report_streak"], 5,
        "the legacy observation is the predecessor of this one, not a fresh install: {record}"
    );
    assert_eq!(record["reported_daily"], true);
    assert_eq!(record["machine"], machine.as_str());
    assert_eq!(record["legacy_migration"], true);
    assert_eq!(
        fs::read(&legacy_path).expect("legacy status still readable"),
        legacy_bytes,
        "the legacy record is not lost, rewritten, or deleted by the migration"
    );
}

/// ① A report whose `install_id` does not match its own key is a broken record,
/// and the reader must not be able to fold two machines' reports into one key.
/// Here the host is asked to key by the machine it resolved, so a status whose
/// caller-supplied `machine` disagrees is refused rather than stored.
#[test]
fn a_status_report_carrying_a_foreign_machine_is_refused() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let install = "88888888-8888-4888-8888-888888888888";

    let mut request = status_request(
        "w248-1",
        install,
        "2026-09-29T10:00:00Z",
        Some((1, "nonce-foreign")),
    );
    request["status"]["machine"] = json!("some-other-machine");
    let output = fixture.chrome(&frame(&request));
    assert_eq!(exit_code(&output), 0, "stderr: {}", stderr_of(&output));
    let response = one_frame(&output.stdout);
    assert_matches_schema(&response);
    assert_eq!(
        response["type"], "nack",
        "the host adds `machine` itself; a caller-supplied one is not a field this message defines: {response}"
    );
}

/// ④ `identity_state` is the read-only answer the popup needs: it names the
/// conflict without asking the host to write anything, so it works on an
/// install whose backfill is off and which therefore never reports status.
#[test]
fn identity_state_names_a_conflict_no_status_report_has_declared() {
    let fixture = Fixture::new();
    fixture.configure_stage();
    let install = "99999999-9999-4999-8999-999999999999";

    let quiet = identity_state(&fixture, "w248-quiet", install);
    assert_eq!(quiet["type"], "identity_state");
    assert_eq!(
        quiet["identity_conflict"], false,
        "an install the host has never heard of is not a conflict"
    );

    report_status(
        &fixture,
        "w248-1",
        install,
        "2026-09-29T10:00:00Z",
        Some((10, "nonce-original")),
    );
    report_status(
        &fixture,
        "w248-2",
        install,
        "2026-09-29T10:05:00Z",
        Some((10, "nonce-copy")),
    );
    assert_eq!(
        identity_state(&fixture, "w248-answer", install)["identity_conflict"],
        true
    );
}
