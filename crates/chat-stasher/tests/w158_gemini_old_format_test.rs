//! W158 — the 2026-02 gemini-cli on-disk shape, through the real pipeline.
//!
//! Up to (at least) 2026-02-13 gemini-cli wrote **one pretty-printed JSON
//! document per session** into `~/.gemini/tmp/<projectHash>/chats/session-*.json`.
//! Its physical lines are not JSON, so a reader that only tries `serde_json` on
//! each line reads nothing at all: the archived sessions came back as
//! `unknown` with `N line(s) could not be parsed as JSON` — the i7 report this
//! item came from (2026-09-25: 8 sessions, 216 lines, no time). W118 added the
//! whole-document fallback in `activity.rs`; this file pins that shape through
//! the path the archive actually uses — sealed shards, sequence-concatenated,
//! with the document split across two shard buckets — which the unit test in
//! `activity.rs` (which hands `analyze_session` lines in memory) does not.
//!
//! Three properties, because each fails differently:
//!   1. the document's own timestamps are the span, `exact`;
//!   2. the shard file's **mtime is never the time** (ADR-035): the same bytes
//!      with a 2021 mtime must report 2026;
//!   3. a document with no in-content timestamp stays `unknown` with a stated
//!      why — the fallback must not invent one from the file.
//!
//! The fixture is synthetic; only its *shape* is taken from the real archived
//! sessions (keys, nesting, indentation, one document per file).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W158 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

/// The 2026-02 gemini-cli document: RFC 3339 `startTime` / `lastUpdated`, and
/// `messages[]` with `content` as a **string** and `type` in {user, gemini}.
fn old_format_document(session_id: &str, first: &str, last: &str) -> String {
    format!(
        "{{\n  \"sessionId\": \"{session_id}\",\n  \"projectHash\": \"synthetic\",\n  \
         \"startTime\": \"{first}\",\n  \"lastUpdated\": \"{last}\",\n  \"messages\": [\n    {{\n      \
         \"id\": \"m1\",\n      \"timestamp\": \"{first}\",\n      \"type\": \"user\",\n      \
         \"content\": \"synthetic user message\"\n    }},\n    {{\n      \"id\": \"m2\",\n      \
         \"timestamp\": \"{last}\",\n      \"type\": \"gemini\",\n      \"content\": \
         \"synthetic reply\"\n    }}\n  ]\n}}\n"
    )
}

/// Write a document as a session, split across `buckets` shard files at a line
/// boundary — the way the collector's bucket cap splits a large session.
fn write_split_session(stage: &Path, machine: &str, session: &str, document: &str, buckets: usize) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    let lines: Vec<&str> = document.lines().collect();
    let per = lines.len().div_ceil(buckets);
    let mut seq = 1;
    for chunk in lines.chunks(per) {
        fs::write(dir.join(format!("{seq:06}.jsonl")), chunk.join("\n") + "\n").unwrap();
        seq += 1;
    }
}

/// The one row the stage's index holds, as JSON.
fn index_rows(stage: &Path, machine: &str) -> Vec<serde_json::Value> {
    let path = stage.join("meta").join(machine).join("activity-v1.jsonl");
    fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Every physical line of a 2026-02 document is a JSON *fragment*: the point of
/// the shape is that per-line parsing finds nothing. Pinned so a future change
/// to the fixture cannot quietly turn it into JSONL.
#[test]
fn the_old_shape_has_no_json_line_on_its_own() {
    let document = old_format_document(
        "synthetic-session",
        "2026-02-13T05:26:00.000Z",
        "2026-02-13T05:30:11.500Z",
    );
    for line in document.lines() {
        assert!(
            serde_json::from_str::<serde_json::Value>(line).is_err(),
            "a physical line of the 2026-02 shape must not parse on its own: {line:?}"
        );
    }
}

#[test]
fn old_gemini_document_is_read_as_exact_through_split_shards_and_never_by_mtime() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let machine = "mbp-gemini";
    let session = "gemini-cli.mbp-gemini.session-2026-02-13T05-26-00000000";

    // 2026-02-13T05:26:00Z .. 2026-02-13T05:30:11.500Z, in unix seconds.
    let (first, last) = (1770960360i64, 1770960611i64);
    let document = old_format_document(
        "session-2026-02-13T05-26-00000000",
        "2026-02-13T05:26:00.000Z",
        "2026-02-13T05:30:11.500Z",
    );
    write_split_session(&stage, machine, session, &document, 2);

    // Pin the shard's mtime far away from the conversation: 2021-01-01.
    let shard = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000")
        .join("000001.jsonl");
    let decoy = SystemTime::UNIX_EPOCH + Duration::from_secs(1_609_459_200);
    fs::OpenOptions::new()
        .write(true)
        .open(&shard)
        .unwrap()
        .set_modified(decoy)
        .unwrap();

    let out = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "activity-index must exit 0:\n{stderr}"
    );

    let rows = index_rows(&stage, machine);
    assert_eq!(rows.len(), 1, "one session -> one row: {rows:?}");
    let row = &rows[0];
    assert_eq!(row["harness"], "gemini-cli");
    assert_eq!(
        row["first_unix"], first,
        "the span must come from the document's own timestamps"
    );
    assert_eq!(row["last_unix"], last);
    assert_eq!(
        row["time_source"]["kind"], "exact",
        "RFC 3339 in-content timestamps are unambiguous"
    );
    assert_ne!(
        row["first_unix"], 1_609_459_200i64,
        "the shard file's mtime must never be the conversation time (ADR-035)"
    );
}

/// The fallback must not become a licence to invent a time: a document with no
/// timestamp anywhere is `unknown` with a why, not `exact` and not empty.
#[test]
fn old_gemini_document_without_a_timestamp_stays_unknown_with_a_why() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let machine = "mbp-gemini";
    let session = "gemini-cli.mbp-gemini.session-2026-02-13T05-26-00000001";

    let document = "{\n  \"sessionId\": \"synthetic\",\n  \"projectHash\": \"synthetic\",\n  \
                    \"messages\": [\n    {\n      \"id\": \"m1\",\n      \"type\": \"user\",\n      \
                    \"content\": \"synthetic user message\"\n    }\n  ]\n}\n";
    write_split_session(&stage, machine, session, document, 2);

    let out = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(out.status.success());

    let rows = index_rows(&stage, machine);
    let row = &rows[0];
    assert_eq!(
        row["time_source"]["kind"], "unknown",
        "a document with no timestamp is unknown, never a fabricated time: {row:?}"
    );
    assert!(row["first_unix"].is_null(), "no bounds were measured");
    let why = row["time_source"]["why"].as_str().unwrap_or_default();
    assert!(
        why.contains("could not be parsed as JSON"),
        "the why must say what happened to the lines: {why}"
    );
}
