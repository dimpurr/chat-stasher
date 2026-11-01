//! W934 — the Grok parser's own rules, behind the `import` seam.
//!
//! W934 shipped in two halves. The parser half lives on: a Grok
//! `prod-grok-backend.json` payload (W909 §2: one JSON object whose
//! `conversations` key holds `{"conversation", "responses"}` wrappers)
//! is parsed by `import::parse_grok_export`, and this file pins the
//! parser's properties directly on the record it returns. The
//! conversation half is held by this build's development import gate
//! (27-ORACLE §0.1): the Grok row is blocked, so the CLI run archives
//! the export byte-exact as its own object and refuses to write a
//! bundle — that gated half is pinned end to end, through the real
//! binary, by `w934_grok_raw_test.rs`, and is not repeated here.
//!
//! What this file holds:
//!
//!   * **The parser-direct properties.** The raw `responses` list is
//!     the complete node set — never walked from the
//!     `leaf_response_id` pointer (27-ORACLE §4.4). Times are carried
//!     verbatim, never rebuilt: a response whose `create_time` is a
//!     string where the export normally carries a MongoDB `$date`
//!     dict of epoch milliseconds stays a string. Every absence lands
//!     as unobserved — never an empty list, never a zero, never an
//!     invented value. The id ladder refuses each named case.
//!   * **Two refusal-ladder cases a gated CLI run still walks** once
//!     `--machine` names the partition: a file that was read to the
//!     end and is not valid JSON is `2`, never `3` (invariant 2), and
//!     each wrong shape is refused with the reason stated — in both
//!     cases before anything is archived, so a refused input reaches
//!     no namespace.
//!
//! Every fixture here is synthetic — a tiny fake
//! `prod-grok-backend.json` built in this file, with text nobody wrote
//! by hand. No real takeout, archive, destination or inbox is touched.

use chat_stasher::{import, raw_export};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

const WRONG_INPUT: i32 = 2;

const ID_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const ID_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
const ID_C: &str = "cccccccc-0000-4000-8000-000000000003";

/// The machine label the gated CLI leg files its index row under. The
/// development import gate requires `--machine` for a grok import, and
/// giving it is what lets these ladder cases reach the parse — the step
/// they pin — instead of stopping at the usage refusal the raw test
/// file already pins. The same fixture-only shape that file uses, for
/// the same reason: it names a partition that exists only in a sandbox.
const MACHINE: &str = "fixture-machine";

/// Run the real binary with every ambient path redirected into `sandbox`.
fn run(sandbox: &Path, args: &[&str]) -> std::process::Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).expect("create sandbox home");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    cmd.args(args)
        .env("HOME", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("xdg-config"))
        .env("XDG_DATA_HOME", sandbox.join("xdg-data"))
        .env("XDG_STATE_HOME", sandbox.join("xdg-state"))
        .env("XDG_CACHE_HOME", sandbox.join("xdg-cache"))
        .env_remove("CHAT_STASHER_REGISTRY");
    cmd.output().expect("run chat-stasher")
}

/// A tiny fake `prod-grok-backend.json` — the dict of W909 §2.1 with
/// its `conversations` list of wrappers, and the three sibling keys
/// the real payload carries present but empty.
///
/// Three conversations, one per focused case: `A` populated (three
/// responses, one attachment reference, a leaf pointer that is *not*
/// the deepest response), `B` with an empty message list, and `C`
/// whose single response carries a string time where the export
/// normally carries a `$date` dict, and no attachment or leaf fields
/// at all.
fn synthetic_grok_export() -> String {
    let a = serde_json::json!({
        "conversation": {
            "id": ID_A,
            "user_id": "synthetic-user-0",
            "create_time": "2026-09-01T00:00:00.000000Z",
            "modify_time": "2026-09-02T00:00:00.000Z",
            "leaf_response_id": format!("{ID_A}-r1"),
        },
        "responses": [
            {
                "response": {
                    "_id": format!("{ID_A}-r0"),
                    "conversation_id": ID_A,
                    "parent_response_id": null,
                    "create_time": { "$date": { "$numberLong": "1756684800000" } },
                    "sender": "human",
                },
                "share_link": null,
            },
            {
                "response": {
                    "_id": format!("{ID_A}-r1"),
                    "conversation_id": ID_A,
                    "parent_response_id": format!("{ID_A}-r0"),
                    "sender": "ASSISTANT",
                    "create_time": { "$date": { "$numberLong": "1756684860000" } },
                    "file_attachments": ["synthetic-asset-uuid-0"],
                },
                "share_link": null,
            },
            {
                "response": {
                    "_id": format!("{ID_A}-r2"),
                    "conversation_id": ID_A,
                    "parent_response_id": format!("{ID_A}-r1"),
                    "create_time": { "$date": { "$numberLong": "1756684920000" } },
                    "sender": "human",
                },
                "share_link": null,
            },
        ],
    });
    let b = serde_json::json!({
        "conversation": {
            "id": ID_B,
            "user_id": "synthetic-user-1",
            "create_time": "2026-09-03T00:00:00.000000Z",
            "modify_time": "2026-09-03T00:00:00.000Z",
        },
        "responses": [],
    });
    let c = serde_json::json!({
        "conversation": {
            "id": ID_C,
            "user_id": "synthetic-user-2",
            "create_time": "2026-09-04T00:00:00.000000Z",
            "modify_time": "2026-09-04T00:00:00.000Z",
        },
        "responses": [
            {
                "response": {
                    "_id": format!("{ID_C}-r0"),
                    "conversation_id": ID_C,
                    "parent_response_id": null,
                    // A row whose time is a string, not the epoch dict
                    // the export normally carries: carried verbatim.
                    "create_time": "2026-09-04T00:00:00.000Z",
                    "sender": "human",
                },
                "share_link": null,
            }
        ],
    });
    serde_json::json!({
        "conversations": [a, b, c],
        "projects": [],
        "tasks": [],
        "media_posts": [],
    })
    .to_string()
}

struct Rig {
    sandbox: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        let sandbox = tempfile::tempdir().expect("sandbox");
        fs::create_dir_all(sandbox.path().join("stage")).expect("create stage");
        Rig { sandbox }
    }
    fn stage(&self) -> PathBuf {
        self.sandbox.path().join("stage")
    }
    fn inbox(&self) -> PathBuf {
        self.sandbox.path().join("inbox")
    }
    fn root(&self) -> &Path {
        self.sandbox.path()
    }
}

/// Drive the gated grok leg with the machine label the gate requires, so
/// the refusal-ladder cases below reach the parse they are about.
fn import_cmd(rig: &Rig, platform: &str, export: &Path) -> std::process::Output {
    run(
        rig.root(),
        &[
            "import",
            platform,
            &export.to_string_lossy(),
            "--inbox",
            &rig.inbox().to_string_lossy(),
            "--stage",
            &rig.stage().to_string_lossy(),
            "--machine",
            MACHINE,
        ],
    )
}

/// An absent inbox is a run that refused before publishing one, which is
/// the same answer as an empty one for these assertions — and a
/// different run failure, which the exit code is the place to say that.
fn bundled_names(inbox_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(inbox_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| {
            entry
                .expect("inbox entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn text(output: &std::process::Output) -> String {
    let mut s = String::from_utf8_lossy(&output.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&output.stderr));
    s
}

/// The record the parser returns keeps every absence as an absence: the
/// string-time response has no `file_attachments` key, and its
/// conversation has no `leaf_response_id` key — unobserved, never
/// empty, never zero.
#[test]
fn every_absence_in_the_grok_record_lands_as_unobserved() {
    let parsed = import::parse_grok_export(synthetic_grok_export().as_bytes()).unwrap();
    assert_eq!(parsed.conversations.len(), 3);
    assert!(parsed.rejections.is_empty());

    // B: the message list was present and empty — a measurement.
    let b = &parsed.conversations[1];
    assert_eq!(b.conversation_id, ID_B);
    assert!(b.object["responses"].as_array().unwrap().is_empty());

    // C: the message list has one response whose time is a string, and
    // whose attachment and leaf fields the platform never wrote.
    let c = &parsed.conversations[2];
    assert_eq!(c.conversation_id, ID_C);
    let response = &c.object["responses"][0]["response"];
    assert_eq!(
        response["create_time"], "2026-09-04T00:00:00.000Z",
        "a string time is carried verbatim, not converted"
    );
    assert!(
        response.get("file_attachments").is_none(),
        "an attachment list the platform did not write stays absent"
    );
    assert!(
        c.object["conversation"].get("leaf_response_id").is_none(),
        "a leaf pointer the platform did not write stays absent"
    );
}

/// The id ladder, as named refusals: a row without a readable, safe,
/// unique id is refused with the reason stated, and nothing is emitted
/// for it. Absent and unreadable are different answers.
#[test]
fn grok_parser_refuses_rows_without_a_readable_id() {
    let cases: Vec<(serde_json::Value, &str)> = vec![
        (
            serde_json::json!({ "responses": [] }),
            "no conversation object",
        ),
        (
            serde_json::json!({ "conversation": { "create_time": "2026-09-01T00:00:00.000000Z" }, "responses": [] }),
            "no conversation id",
        ),
        (
            serde_json::json!({ "conversation": { "id": 12, "responses": [] } }),
            "conversation id is not a readable string",
        ),
        (
            serde_json::json!({ "conversation": { "id": "../escape", "responses": [] } }),
            "conversation id is unsafe to carry",
        ),
        (
            // An oversized id cannot become a path component without
            // rewriting it, so it is refused rather than truncated.
            serde_json::json!({ "conversation": { "id": "a".repeat(400), "responses": [] } }),
            "conversation id is unsafe to carry",
        ),
    ];
    for (wrapper, expected_reason) in cases {
        let export = serde_json::json!({
            "conversations": [wrapper],
            "projects": [],
            "tasks": [],
            "media_posts": [],
        })
        .to_string();
        let parsed = import::parse_grok_export(export.as_bytes()).unwrap();
        assert!(
            parsed.conversations.is_empty(),
            "{expected_reason}: nothing is emitted for a refused row"
        );
        assert_eq!(parsed.rejections.len(), 1, "{expected_reason}");
        assert!(
            parsed.rejections[0].contains(expected_reason),
            "{expected_reason}: got {}",
            parsed.rejections[0]
        );
    }
}

/// A file that was read to the end and is not valid JSON is a
/// completed read of invalid input: `2`, not `3`. The exit code
/// must not claim a read was incomplete when it finished.
#[test]
fn grok_import_calls_a_complete_but_malformed_export_wrong_input() {
    let rig = Rig::new();
    let full = synthetic_grok_export();
    let path = rig.root().join("prod-grok-backend.json");
    fs::write(&path, format!("{full} {{")).expect("write malformed export");

    let output = import_cmd(&rig, "grok", &path);
    let shown = text(&output);
    assert_eq!(
        output.status.code(),
        Some(WRONG_INPUT),
        "a fully read malformed document is wrong input, so 2 and not 3: {shown}"
    );
    assert!(shown.contains("read to the end"), "{shown}");
    assert!(
        bundled_names(&rig.inbox()).is_empty(),
        "a refused input writes nothing"
    );
    assert!(
        !rig.stage().join(raw_export::EXPORT_FILES_DIR).exists(),
        "bytes that never parsed are not an export: the object namespace holds \
         exactly the files this producer accepted"
    );
    assert!(
        !rig.stage().join(import::IMPORT_RAW_DIR).exists(),
        "a refused input archives nothing in either namespace"
    );
}

/// The Grok payload is a dict, not Claude's flat array, and a dict
/// without a `conversations` key is not a Grok export either. Both
/// were read completely and are the wrong input: `2`, not `3`.
#[test]
fn grok_import_refuses_the_wrong_file_shape_as_wrong_input() {
    let rig = Rig::new();

    let claude_shaped = rig.root().join("conversations.json");
    fs::write(&claude_shaped, format!("[{{\"uuid\":\"{ID_A}\"}}]")).expect("write flat array");
    let output = import_cmd(&rig, "grok", &claude_shaped);
    let shown = text(&output);
    assert_eq!(
        output.status.code(),
        Some(WRONG_INPUT),
        "a flat array is the wrong shape for a Grok import, and it was read: {shown}"
    );
    assert!(shown.contains("not an object"), "{shown}");

    let no_key = rig.root().join("no-conversations.json");
    fs::write(&no_key, br#"{"projects":[]}"#).expect("write dict without conversations");
    let output = import_cmd(&rig, "grok", &no_key);
    let shown = text(&output);
    assert_eq!(
        output.status.code(),
        Some(WRONG_INPUT),
        "a dict without `conversations` is the wrong input: {shown}"
    );
    assert!(shown.contains("`conversations`"), "{shown}");

    assert!(
        bundled_names(&rig.inbox()).is_empty(),
        "a refused input writes nothing"
    );
    assert!(
        !rig.stage().join(raw_export::EXPORT_FILES_DIR).exists(),
        "a refused input archives nothing"
    );
    assert!(
        !rig.stage().join(import::IMPORT_RAW_DIR).exists(),
        "a refused input archives nothing in either namespace"
    );
}
