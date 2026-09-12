//! The export file — `nativehost-protocol.md` §8.
//!
//! When the host cannot be reached the extension can write everything it has
//! not delivered into one `*.jsonl` file, one `payload` string per line, and
//! hand it to `chat-stasher ingest --inbox <dir>`. The properties this file
//! pins:
//!
//! * a line's identity is the SHA-256 of the line **without its newline** —
//!   the same key `deliver` uses, which is what lets a payload that already
//!   arrived over Native Messaging be recognised here instead of archived
//!   twice;
//! * the file retires to `consumed/` only when *every* line was sealed or found
//!   to be a duplicate — otherwise it stays, with one error naming the line;
//! * a re-run creates no new shards, because every line that landed is
//!   content-addressed.
//!
//! Everything runs against a temp `HOME`/`XDG_*`; no real config, stage or
//! browser directory is read.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const CHROME_ORIGIN: &str = "chrome-extension://gihmdkkmmmkeiagjjiimacmgkdilofhi/";

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    stage: PathBuf,
    inbox: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path().join("home");
        let stage = dir.path().join("stage");
        let inbox = dir.path().join("inbox");
        for path in [&home, &stage, &inbox] {
            fs::create_dir_all(path).expect("create fixture dir");
        }
        let fixture = Fixture {
            _dir: dir,
            home,
            stage,
            inbox,
        };
        // `machine` is pinned so the host and `ingest` provably write to the
        // same partition: the whole point of the first test below is that the
        // two channels share one dedup scope, and an auto-generated identity
        // for the host plus an explicit `--machine` for ingest would put them
        // in different directories and make that test pass for the wrong
        // reason.
        fixture.write_config(&format!(
            "machine = \"export-test-machine\"\n\n[native_host]\nstage = {}\n",
            serde_json::to_string(&fixture.stage.to_string_lossy()).expect("path")
        ));
        fixture
    }

    fn write_config(&self, text: &str) {
        let path = self
            .home
            .join("config")
            .join("chat-stasher")
            .join("config.toml");
        fs::create_dir_all(path.parent().expect("config dir")).expect("mkdir");
        fs::write(&path, text).expect("write config");
    }

    fn command(&self, program: &str, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = Command::new(program)
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
            .expect("spawn");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(stdin)
            .expect("write stdin");
        child.wait_with_output().expect("wait")
    }

    /// Send one payload through the real host, so the export test starts from
    /// something that genuinely arrived over Native Messaging.
    fn deliver_via_host(&self, request_id: &str, name: &str, payload: &str) -> Value {
        let body = serde_json::to_vec(&serde_json::json!({
            "protocol": 1,
            "type": "deliver",
            "request_id": request_id,
            "name": name,
            "payload": payload,
            "sha256": sha256_hex(payload.as_bytes()),
        }))
        .expect("serialise");
        let mut frame = (body.len() as u32).to_ne_bytes().to_vec();
        frame.extend_from_slice(&body);

        let output = self.command(env!("CARGO_BIN_EXE_chat-stasher"), &[CHROME_ORIGIN], &frame);
        assert_eq!(output.status.code(), Some(0), "host failed");
        let declared = u32::from_ne_bytes(output.stdout[..4].try_into().expect("prefix")) as usize;
        assert_eq!(output.stdout.len(), 4 + declared, "stdout is not one frame");
        serde_json::from_slice(&output.stdout[4..]).expect("response JSON")
    }

    fn ingest(&self) -> Output {
        self.command(
            env!("CARGO_BIN_EXE_chat-stasher"),
            &[
                "ingest",
                "--inbox",
                &self.inbox.to_string_lossy(),
                "--stage",
                &self.stage.to_string_lossy(),
                "--machine",
                "export-test-machine",
            ],
            b"",
        )
    }

    fn write_export(&self, name: &str, lines: &[&str]) -> PathBuf {
        let path = self.inbox.join(name);
        let mut text = String::new();
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        fs::write(&path, text).expect("write export");
        path
    }

    fn shard_names(&self, id: &str) -> Vec<String> {
        let dir = self
            .stage
            .join("sessions")
            .join("export-test-machine")
            .join(id);
        let mut names: Vec<String> = chat_stasher::store::sealed_shard_entries(&dir)
            .expect("read shards")
            .into_iter()
            .map(|(_, path)| {
                path.file_name()
                    .expect("name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    fn shard_records(&self, id: &str) -> Vec<Value> {
        let dir = self
            .stage
            .join("sessions")
            .join("export-test-machine")
            .join(id);
        let mut records = Vec::new();
        for (_, path) in chat_stasher::store::sealed_shard_entries(&dir).expect("shards") {
            let raw = fs::read_to_string(&path).expect("read shard");
            for line in raw.lines() {
                records.push(serde_json::from_str(line).expect("shard line"));
            }
        }
        records
    }

    fn consumed_dir(&self) -> PathBuf {
        self.inbox.join("consumed")
    }

    fn stdout(output: &Output) -> String {
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn bundle(session: &str, text: &str) -> String {
    format!(
        r#"{{"schema":"chat-stasher/inbox@1","platform":"deepseek","sessionId":"{session}","capturedAt":"2026-09-12T00:00:00.000Z","parsed":{{"hasJson":true,"keys":["id"]}},"raw":{{"text":{text},"bytes":{n}}}}}"#,
        text = serde_json::to_string(text).expect("text as JSON"),
        n = text.len(),
    )
}

/// A host-delivered payload reappearing as an export line must be recognised,
/// not archived twice: both channels content-address by the same key.
#[test]
fn an_export_line_that_already_arrived_by_host_is_a_duplicate() {
    let fixture = Fixture::new();

    let delivered = bundle("sess-exp", "already delivered");
    let acked = fixture.deliver_via_host("req-1", "deepseek-sess-exp.json", &delivered);
    assert_eq!(acked["status"], "stored", "ack: {acked}");

    let fresh_b = bundle("sess-exp", "second conversation body");
    let fresh_c = bundle("sess-exp", "third conversation body");
    let export = fixture.write_export(
        "chat-stasher-export-20260912T000000Z.jsonl",
        &[&delivered, &fresh_b, "", &fresh_c],
    );

    let output = fixture.ingest();
    assert_eq!(
        output.status.code(),
        Some(0),
        "ingest stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = Fixture::stdout(&output);
    assert!(text.contains("[ingest] consumed         : 2"), "{text}");
    assert!(text.contains("export files    : 1"), "{text}");
    assert!(text.contains("blank lines skipped: 1"), "{text}");

    // Three shards: the one the host sealed, plus B and C from the export.
    assert_eq!(
        fixture.shard_names("deepseek.sess-exp"),
        ["000001.jsonl", "000002.jsonl", "000003.jsonl"]
    );

    // Every line was sealed or recognised, so the file retired.
    assert!(
        !export.exists(),
        "an export whose every line landed must be retired"
    );
    assert!(
        fixture
            .consumed_dir()
            .join("chat-stasher-export-20260912T000000Z.jsonl")
            .is_file(),
        "the export was not moved to consumed/"
    );

    // The synthetic source_file records which line each shard came from.
    let records = fixture.shard_records("deepseek.sess-exp");
    let sources: Vec<&str> = records
        .iter()
        .map(|record| record["source_file"].as_str().expect("source_file"))
        .collect();
    assert!(
        sources.contains(&"chat-stasher-export-20260912T000000Z.jsonl#2"),
        "line 2 must carry its own line number: {sources:?}"
    );
    assert!(
        sources.contains(&"chat-stasher-export-20260912T000000Z.jsonl#4"),
        "the blank line must not shift the numbering: {sources:?}"
    );
    // The hash is of the line without its newline — the same key `deliver`
    // recorded for the identical payload.
    let delivered_sha = sha256_hex(delivered.as_bytes());
    assert!(
        records
            .iter()
            .any(|record| record["file_sha256"] == delivered_sha.as_str()),
        "the delivered payload's hash must be the one the export line produced"
    );
}

/// One bad line must not cost the good ones their shards, and must not make the
/// file disappear: the user needs something left to look at.
#[test]
fn a_bad_line_leaves_the_export_in_place_and_names_the_line() {
    let fixture = Fixture::new();

    let good_one = bundle("sess-bad", "first good body");
    let good_two = bundle("sess-bad", "second good body");
    let export = fixture.write_export(
        "chat-stasher-export-20260912T010101Z.jsonl",
        &[&good_one, "{\"payload\": \"truncated", &good_two],
    );

    let output = fixture.ingest();
    assert_eq!(
        output.status.code(),
        Some(1),
        "a failed line must leave ingest non-zero; stdout: {}",
        Fixture::stdout(&output)
    );
    let text = Fixture::stdout(&output);
    assert!(
        text.contains("chat-stasher-export-20260912T010101Z.jsonl#2"),
        "the error must name the line it came from:\n{text}"
    );
    assert!(text.contains("[ingest] errors           : 1"), "{text}");

    // The two good lines were sealed anyway.
    assert_eq!(
        fixture.shard_names("deepseek.sess-bad"),
        ["000001.jsonl", "000002.jsonl"]
    );
    // ...and the file is still in the inbox.
    assert!(
        export.is_file(),
        "an export with an unsealed line must NOT be retired"
    );
    assert!(!fixture
        .consumed_dir()
        .join("chat-stasher-export-20260912T010101Z.jsonl")
        .exists());
}

/// The re-run is the whole reason the file is left behind: it must cost
/// nothing and add nothing.
#[test]
fn re_running_a_failed_export_seals_nothing_new() {
    let fixture = Fixture::new();

    let good_one = bundle("sess-retry", "first body");
    let good_two = bundle("sess-retry", "second body");
    fixture.write_export(
        "chat-stasher-export-20260912T020202Z.jsonl",
        &[&good_one, "not json at all", &good_two],
    );

    let first = fixture.ingest();
    assert_eq!(first.status.code(), Some(1));
    let after_first = fixture.shard_names("deepseek.sess-retry");
    assert_eq!(after_first.len(), 2);

    let second = fixture.ingest();
    assert_eq!(
        second.status.code(),
        Some(1),
        "the bad line is still bad; stdout: {}",
        Fixture::stdout(&second)
    );
    let text = Fixture::stdout(&second);
    assert!(text.contains("[ingest] consumed         : 0"), "{text}");
    assert!(text.contains("[ingest] errors           : 1"), "{text}");
    assert_eq!(
        fixture.shard_names("deepseek.sess-retry"),
        after_first,
        "a re-run must not seal a second shard for the same bytes"
    );
}

/// An empty export has nothing to seal, so every line in it (vacuously) landed.
/// It retires, and it does not fabricate a shard.
#[test]
fn an_empty_export_retires_without_sealing_anything() {
    let fixture = Fixture::new();
    let export = fixture.write_export("chat-stasher-export-20260912T030303Z.jsonl", &[]);

    let output = fixture.ingest();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}",
        Fixture::stdout(&output)
    );
    assert!(!export.exists());
    assert!(fixture
        .consumed_dir()
        .join("chat-stasher-export-20260912T030303Z.jsonl")
        .is_file());
    assert_eq!(
        fixture.shard_names("deepseek.sess-exp"),
        Vec::<String>::new()
    );
}

/// A `*.json` file in the same inbox still takes the established path: an
/// unparseable one is archived raw-only rather than rejected. The stricter
/// treatment is for the machine-produced export lines only.
#[test]
fn a_plain_json_file_keeps_the_raw_fallback_and_the_export_does_not() {
    let fixture = Fixture::new();
    fs::write(fixture.inbox.join("deepseek-weird.json"), "not json").expect("write");
    fixture.write_export("chat-stasher-export-20260912T040404Z.jsonl", &["not json"]);

    let output = fixture.ingest();
    let text = Fixture::stdout(&output);
    assert_eq!(
        output.status.code(),
        Some(1),
        "the export line is an error; stdout: {text}"
    );
    // The raw-only record for the *.json file is a real shard.
    assert_eq!(
        fixture.shard_names("deepseek.weird"),
        ["000001.jsonl"],
        "stdout: {text}"
    );
    // Nothing was sealed for the export line.
    assert!(
        fixture.shard_names("deepseek.sess-exp").is_empty(),
        "the export line must not become a raw record"
    );
    // The *.json file took the raw path and retired; the export did not retire.
    assert!(!fixture.inbox.join("deepseek-weird.json").exists());
    assert!(fixture
        .inbox
        .join("chat-stasher-export-20260912T040404Z.jsonl")
        .exists());
}
