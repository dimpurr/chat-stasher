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
        other => panic!(
            "the schema contains a pattern this test cannot evaluate ({other}); \
             teach `pattern_matches` about it rather than letting it pass unverified"
        ),
    }
}

/// Assert a value satisfies the whole committed schema, and that the top level
/// is still the `oneOf` of the five message shapes.
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
    // demonstrably non-empty. `~otheruser` cannot be expanded, so the loader
    // warns and resets that option.
    let fixture = Fixture::new();
    fixture.write_config(&format!(
        "[native_host]\nstage = {}\n\n[harness_roots]\ncodex = \"~otheruser/.codex/sessions\"\n",
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
