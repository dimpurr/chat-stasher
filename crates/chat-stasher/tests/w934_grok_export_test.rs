//! W934 — the Grok official-export parser, behind the `import` seam.
//!
//! `chat-stasher import grok <export> --inbox <dir> --stage <dir>`
//! parses a Grok `prod-grok-backend.json` payload (W909 §2: one JSON
//! object whose `conversations` key holds `{"conversation",
//! "responses"}` wrappers) and does exactly what the Claude leg of the
//! same seam does: archives the whole file byte-exact in the
//! `import-raw` stage namespace and emits one ordinary `inbox@3`
//! `web-capture` bundle per conversation into a **local** inbox folder.
//! It never seals, never pushes and never reads a config or a
//! destination, so these tests can point it at a `tempfile` sandbox
//! and mean it.
//!
//! The parser rules these tests keep honest, all measured in W909 §2:
//!
//!   * **The raw `responses` list is the complete node set.** A
//!     response deeper than the conversation's `leaf_response_id`
//!     still belongs to the record — the list, not the leaf pointer,
//!     is the measurement axis (27-ORACLE §4.4).
//!   * **Times are carried, never rebuilt.** A response whose
//!     `create_time` is a string where the export normally carries a
//!     MongoDB `$date` dict of epoch milliseconds is carried verbatim:
//!     the parser neither converts it nor rejects it, and the
//!     conversation's own `create_time`/`modify_time` survive as the
//!     platform wrote them.
//!   * **Every absence lands as unobserved.** A response without
//!     `file_attachments` and a conversation without
//!     `leaf_response_id` keep those keys absent in the record — never
//!     an empty list, never a zero, never an invented value.
//!   * **The exit code says which kind of "no" this was** (invariant
//!     2): `3` for "never finished reading", `2` for "read it, and it
//!     is not importable". A torn file, a wrong-platform shape and a
//!     manifest are three different refusals.
//!
//! Every fixture here is synthetic — a tiny fake
//! `prod-grok-backend.json` built in this file, with text nobody wrote
//! by hand. No real takeout, archive, destination or inbox is touched.

use chat_stasher::{import, inbox, store};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

const DID_NOT_FINISH: i32 = 3;
const WRONG_INPUT: i32 = 2;
const CLEAN: i32 = 0;

const ID_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const ID_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
const ID_C: &str = "cccccccc-0000-4000-8000-000000000003";
/// A one-time download link, planted so its absence is a fact we
/// checked rather than a property we hoped for. Not a real
/// credential format, not reachable.
const PLANTED_URL: &str = "https://example.invalid/export/one-time-use/SECRETTOKEN1234567890";

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
    /// Writes the synthetic export and returns its path.
    fn export(&self) -> PathBuf {
        let path = self.root().join("prod-grok-backend.json");
        fs::write(&path, synthetic_grok_export()).expect("write synthetic export");
        path
    }
    fn root(&self) -> &Path {
        self.sandbox.path()
    }
}

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

/// The happy path, and the structural promises that go with it.
#[test]
fn grok_import_emits_valid_bundles_and_keeps_the_export_out_of_session_counts() {
    let rig = Rig::new();
    let export = rig.export();
    let source_bytes = fs::read(&export).unwrap();

    let output = import_cmd(&rig, "grok", &export);
    assert_eq!(
        output.status.code(),
        Some(CLEAN),
        "a good Grok export imports cleanly: {}",
        text(&output)
    );

    let names = bundled_names(&rig.inbox());
    assert_eq!(
        names,
        vec![
            import::bundle_file_name("grok", ID_A),
            import::bundle_file_name("grok", ID_B),
            import::bundle_file_name("grok", ID_C),
        ],
        "one bundle per conversation, and no half-written .part behind"
    );
    for name in &names {
        let bytes = fs::read(rig.inbox().join(name)).unwrap();
        inbox::check_bundle(&bytes)
            .unwrap_or_else(|e| panic!("{name} is not an inbox@3 bundle: {e}"));
    }

    // The byte-exact copy, under its own namespace, named by its own
    // digest, in the grok partition.
    let dir = rig.stage().join(import::IMPORT_RAW_DIR).join("grok");
    let stored = dir.join(hex_sha256(&source_bytes));
    assert_eq!(fs::read(&stored).unwrap(), source_bytes, "byte-exact");

    // The exclusion this slice is judged on: a stage holding that export
    // still counts zero sessions, because nothing here wrote under
    // `stage/sessions`.
    assert_eq!(
        store::sealed_shard_count(&rig.stage()).unwrap(),
        0,
        "an archived export must not move a session count"
    );
    assert!(
        !rig.stage().join("sessions").exists(),
        "import never touches the sessions tree"
    );
    assert!(
        store::stage_file_sha256s(&rig.stage()).unwrap().is_empty(),
        "the ingest audit scan must not see a raw export"
    );

    // Read-only on the source (ADR-011): same bytes, same place.
    assert_eq!(fs::read(&export).unwrap(), source_bytes);
    assert!(export.exists());

    // And it did not seal anything: the inbox files are still there,
    // unconsumed.
    assert!(
        !rig.inbox().join("consumed").exists(),
        "import must not act like ingest"
    );
}

/// The bundle for a Grok conversation carries the whole wrapper as its
/// body: the conversation's own times, the complete response list —
/// including the response deeper than the named leaf — and the
/// attachment references, all as the platform wrote them.
#[test]
fn the_grok_bundle_body_is_the_whole_wrapper_field_for_field() {
    let rig = Rig::new();
    let export = rig.export();
    let output = import_cmd(&rig, "grok", &export);
    assert_eq!(output.status.code(), Some(CLEAN), "{}", text(&output));

    let bytes = fs::read(rig.inbox().join(import::bundle_file_name("grok", ID_A))).unwrap();
    let bundle: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(bundle["platform"], "grok");
    assert_eq!(bundle["sessionId"], ID_A);
    assert_eq!(bundle["fidelity"]["representation"], "export");

    let body: serde_json::Value =
        serde_json::from_str(bundle["raw"]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["conversation"]["id"], ID_A);
    // The conversation's own times, carried — never derived from the
    // responses, never reformatted.
    assert_eq!(
        body["conversation"]["create_time"],
        "2026-09-01T00:00:00.000000Z"
    );
    assert_eq!(
        body["conversation"]["modify_time"],
        "2026-09-02T00:00:00.000Z"
    );
    // The raw list is the complete node set: three responses, the third
    // deeper than the leaf the conversation names.
    let responses = body["responses"].as_array().unwrap();
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[2]["response"]["_id"], format!("{ID_A}-r2"));
    // The attachment reference survives as a reference.
    assert_eq!(
        responses[1]["response"]["file_attachments"][0],
        "synthetic-asset-uuid-0"
    );
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

/// A file that stops mid-JSON was never read to the end. That is `3`,
/// and the difference between `3` and `2` is the whole content of
/// invariant 2.
#[test]
fn grok_import_calls_a_torn_export_a_read_that_never_finished() {
    let rig = Rig::new();
    let full = synthetic_grok_export();
    let path = rig.root().join("prod-grok-backend.json");
    fs::write(&path, &full[..full.len() - 40]).expect("write truncated export");

    let output = import_cmd(&rig, "grok", &path);
    assert_eq!(
        output.status.code(),
        Some(DID_NOT_FINISH),
        "unreadable-to-the-end is 3: {}",
        text(&output)
    );
    assert!(
        bundled_names(&rig.inbox()).is_empty(),
        "a run that never read the file must not emit a partial conversation set"
    );
    assert!(
        !rig.stage().join(import::IMPORT_RAW_DIR).exists(),
        "bytes that never parsed are not an export, so the byte-exact namespace holds \
         exactly the files this producer imported — and nothing it refused"
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
        !rig.stage().join(import::IMPORT_RAW_DIR).exists(),
        "a refused input archives nothing"
    );
}

/// A Claude manifest offered to the Grok parser is refused by name, and
/// the refusal must not become a channel for echoing a one-time-use
/// download credential.
#[test]
fn grok_import_refuses_a_manifest_by_name_and_never_echoes_its_url() {
    let rig = Rig::new();
    let manifest = serde_json::json!({
        "version": "1.0",
        "total_files": 6,
        "data_files": [
            { "category": "conversations", "part": 0,
              "filename": "conversations-000.zip", "export_url": PLANTED_URL }
        ]
    })
    .to_string();
    let path = rig.root().join("manifest-1.json");
    fs::write(&path, &manifest).expect("write synthetic manifest");

    let output = import_cmd(&rig, "grok", &path);
    let shown = text(&output);
    assert_eq!(
        output.status.code(),
        Some(WRONG_INPUT),
        "a manifest was read completely and is the wrong input, so 2 and not 3: {shown}"
    );
    assert!(shown.contains("manifest"), "{shown}");
    assert!(
        !shown.contains(PLANTED_URL) && !shown.contains("SECRETTOKEN1234567890"),
        "the refusal echoed a one-time-use export_url"
    );
    assert!(
        bundled_names(&rig.inbox()).is_empty(),
        "a refused input writes nothing"
    );
    assert!(
        !rig.stage().join(import::IMPORT_RAW_DIR).exists(),
        "a manifest must not reach the byte-exact namespace: its `export_url` values \
         are one-time-use credentials, and archiving a refused file would keep them"
    );
}

/// Re-importing the same Grok export is a no-op for the raw namespace:
/// the byte-exact file is stored once, and the same bundles are
/// rewritten byte-identically.
#[test]
fn a_second_grok_import_of_the_same_export_stores_nothing_new() {
    let rig = Rig::new();
    let export = rig.export();

    let first = import_cmd(&rig, "grok", &export);
    assert_eq!(first.status.code(), Some(CLEAN), "{}", text(&first));
    let second = import_cmd(&rig, "grok", &export);
    assert_eq!(second.status.code(), Some(CLEAN), "{}", text(&second));

    let shown = text(&second);
    assert!(
        shown.contains("already archived") || shown.contains("0 new"),
        "the report should say the raw export was already there: {shown}"
    );
    assert_eq!(
        bundled_names(&rig.inbox()).len(),
        3,
        "one bundle per conversation, not two"
    );
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
