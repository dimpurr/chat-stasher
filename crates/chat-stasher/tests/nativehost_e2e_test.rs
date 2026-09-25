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

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

    /// Run the binary with an exact argv, isolated from every real directory.
    fn run(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
            .args(args)
            .env("HOME", &self.home)
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
                newest_mtime: Some(1_760_000_000),
            },
            StageSession {
                harness: None,
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
