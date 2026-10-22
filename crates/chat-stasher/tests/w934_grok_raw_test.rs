//! W934 — the Grok leg's raw-only state, driven through the real binary.
//!
//! ADR-055 opened the import groundwork (TKO-2: producer skeleton W901/W920,
//! raw-export archiving W902, per-platform parsers), and this file pins what
//! TKO-4's Grok leg ships while 27-ORACLE §0.1's gate table holds the
//! platform's row blocked: `import grok` archives the export **byte-exact as
//! its own object** in the `export-files/` namespace — the half the gate
//! permits — and **refuses to write a single conversation bundle**, saying
//! so (the half the gate blocks, enforced by the command itself rather than
//! by a process nobody checks).
//!
//! Why the code refuses instead of trusting the operator: the gate exists
//! to keep the extension's measurement honest — filling the extension's gaps
//! from the takeout would make "can the extension alone back everything up?"
//! unanswerable — and a rule that lives only in a document cannot stop a
//! script that was written before the document was. The refusal is the
//! product's answer in this build, and the row that flips it will be a diff
//! someone has to justify, not ambient behavior.
//!
//! Properties this file keeps honest, each on both sides of the gate:
//!
//!   * **One object per content digest.** A repeat of the same bytes — the
//!     same file again, or the same bytes under a different delivered name —
//!     seals nothing new (ADR-055 D4), while a distinct export is kept.
//!   * **The object is invisible to every session number.** Structural: the
//!     namespace is not under `sessions/`, so a stage holding the export of
//!     the Grok takeout still counts zero sessions and the ingest audit scan
//!     still sees nothing to worry about.
//!   * **The exit code says which "no" happened.** The gate refusal is an
//!     exit-1 outcome (read, understood, deliberately incomplete — what was
//!     written is real), a missing `--machine` is exit 2 (usage), a torn file
//!     is exit 3 (never read to the end, so nothing is archived and nothing
//!     about its contents is proven). Invariant 2, applied to a gate.
//!   * **The wrong input archives nothing.** A flat array is the Claude shape
//!     and is refused; a manifest is refused by name without echoing its
//!     one-time download links, on the Grok leg exactly as on the Claude
//!     leg — and in both cases the byte-exact namespace holds exactly the
//!     exports this producer accepted.
//!   * **The claude leg is unchanged.** Its skeleton run still writes bundles
//!     and still uses `import-raw/`: the gate's Grok row is what is blocked,
//!     and this file pins that the refusal did not leak across legs.
//!
//! Every fixture here is synthetic — a two-conversation fake
//! `prod-grok-backend.json` built in this file, text nobody wrote by hand.
//! No real takeout, archive, destination or inbox is touched, and the tests
//! write only into `tempfile` sandboxes.

use chat_stasher::{import, raw_export, store};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

/// The machine label the tests file the object's index row under. The same
/// shape W902's tests use, for the same reason: it names a partition that
/// exists only in the sandbox.
const MACHINE: &str = "fixture-machine";

const ID_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const ID_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
/// A one-time download link, planted so the absence of an echo is a fact
/// that was checked rather than a property that was hoped for. Not a real
/// credential format, not reachable.
const PLANTED_URL: &str = "https://example.invalid/export/one-time-use/SECRETTOKEN1234567890";

const GATE_REFUSED: i32 = 1;
const USAGE: i32 = 2;
const DID_NOT_FINISH: i32 = 3;
const CLEAN: i32 = 0;

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

/// A tiny fake `prod-grok-backend.json`: the dict of W909 §2.1 (the same
/// shape the parser's own tests build), one wrapper per id, text nobody
/// wrote by hand.
fn synthetic_grok_export(ids: &[&str]) -> String {
    let conversations: Vec<serde_json::Value> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            serde_json::json!({
                "conversation": {
                    "id": id,
                    "user_id": format!("synthetic-user-{i}"),
                    "create_time": "2026-09-01T00:00:00.000000Z",
                    "modify_time": "2026-09-02T00:00:00.000Z",
                },
                "responses": [
                    { "response": {
                        "_id": format!("{id}-r0"),
                        "conversation_id": id,
                        "parent_response_id": null,
                        "sender": "human",
                        "create_time": { "$date": { "$numberLong": "1756684800000" } },
                    }, "share_link": null },
                    { "response": {
                        "_id": format!("{id}-r1"),
                        "conversation_id": id,
                        "parent_response_id": format!("{id}-r0"),
                        "sender": "ASSISTANT",
                        "create_time": { "$date": { "$numberLong": "1756684860000" } },
                    }, "share_link": null },
                ],
            })
        })
        .collect();
    serde_json::to_string(&serde_json::json!({
        "conversations": conversations,
        "projects": [],
        "tasks": [],
        "media_posts": [],
    }))
    .expect("serialise synthetic export")
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
    /// Writes the synthetic Grok export and returns its path.
    fn export(&self) -> PathBuf {
        let path = self.root().join("prod-grok-backend.json");
        fs::write(&path, synthetic_grok_export(&[ID_A, ID_B])).expect("write synthetic export");
        path
    }
}

fn import_grok(rig: &Rig, export: &Path, machine: Option<&str>) -> std::process::Output {
    let mut args: Vec<String> = vec![
        "import".to_string(),
        "grok".to_string(),
        export.to_string_lossy().into_owned(),
        "--inbox".to_string(),
        rig.inbox().to_string_lossy().into_owned(),
        "--stage".to_string(),
        rig.stage().to_string_lossy().into_owned(),
    ];
    if let Some(machine) = machine {
        args.push("--machine".to_string());
        args.push(machine.to_string());
    }
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run(rig.root(), &borrowed)
}

fn text(output: &std::process::Output) -> String {
    let mut s = String::from_utf8_lossy(&output.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&output.stderr));
    s
}

fn names(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

/// The exit-1 test: the raw export archives as its own object and the gate
/// refuses every conversation bundle. Every structural promise is asserted
/// here: byte-exactness, D4's key, the index row, the untouched inbox, the
/// absent skeleton namespace, and the stage that still counts zero sessions.
#[test]
fn import_grok_archives_the_export_as_an_object_and_refuses_all_bundles() {
    let rig = Rig::new();
    let export = rig.export();
    let bytes = fs::read(&export).unwrap();

    let output = import_grok(&rig, &export, Some(MACHINE));
    let shown = text(&output);
    assert_eq!(
        output.status.code(),
        Some(GATE_REFUSED),
        "a gated platform's run read and archived the export, and refused the \
         observation import on purpose: what was written is real but the run is \
         incomplete, so 1 and not 0: {shown}"
    );
    assert!(
        shown.contains("refused"),
        "the run must say so rather than let 0 bundles read as a success: {shown}"
    );

    // The object, byte-exact at D4's key, keeping the delivered name.
    let sha = hex_sha256(&bytes);
    let object =
        raw_export::export_object_file(&rig.stage(), "grok", &sha, "prod-grok-backend.json");
    assert!(
        object.exists(),
        "the export must be archived as its own object at {}: {shown}",
        object.display()
    );
    assert_eq!(
        fs::read(&object).unwrap(),
        bytes,
        "byte-exact, as delivered"
    );

    // The metadata-only index row, binding the object into the run.
    match raw_export::read_export_index(&rig.stage(), MACHINE).unwrap() {
        raw_export::ExportIndexState::Loaded(rows) => {
            assert_eq!(rows.len(), 1, "{rows:?}");
            assert_eq!(rows[0].platform, "grok");
            assert_eq!(rows[0].content_sha256, sha);
            assert_eq!(rows[0].bytes, bytes.len() as u64);
        }
        other => panic!("the index row must exist for a run that archived an object: {other:?}"),
    }

    // Zero conversations left the run, by refusal and not by chance.
    assert!(
        !rig.inbox().exists(),
        "a gated run refuses every bundle: the inbox must not even be created, {shown}"
    );
    assert!(
        !rig.stage().join(import::IMPORT_RAW_DIR).exists(),
        "the gated leg archives the export as its own object; writing the \
         skeleton's import-raw copy too would archive the same bytes twice: {shown}"
    );
    assert_eq!(
        store::sealed_shard_count(&rig.stage()).unwrap(),
        0,
        "an archived export must not move a session count"
    );
    assert!(
        store::stage_file_sha256s(&rig.stage()).unwrap().is_empty(),
        "the ingest audit scan must not see an export object"
    );

    // Read-only on the source (ADR-011).
    assert_eq!(fs::read(&export).unwrap(), bytes);
    assert!(export.exists());
}

/// The one-object rule: a byte-identical repeat seals nothing — the same
/// file again, and the same bytes under a different delivered name, are the
/// same object (ADR-055 D4), and the gate's refusal does not turn either
/// into a second copy.
#[test]
fn a_byte_identical_grok_export_repeats_into_one_object_only() {
    let rig = Rig::new();
    let export = rig.export();
    let bytes = fs::read(&export).unwrap();
    let sha = hex_sha256(&bytes);

    let first = import_grok(&rig, &export, Some(MACHINE));
    assert_eq!(first.status.code(), Some(GATE_REFUSED), "{}", text(&first));

    // The same bytes, delivered under a different name.
    let again = rig.root().join("export copy (1).json");
    fs::write(&again, &bytes).expect("write the renamed repeat");
    let second = import_grok(&rig, &again, Some(MACHINE));
    let shown = text(&second);
    assert_eq!(
        second.status.code(),
        Some(GATE_REFUSED),
        "the gate does not lift for a repeat: {shown}"
    );
    assert!(
        shown.contains("already archived"),
        "a repeat must report the object it already has: {shown}"
    );

    let platform_dir = rig.stage().join(raw_export::EXPORT_FILES_DIR).join("grok");
    assert_eq!(
        names(&platform_dir),
        vec![sha.clone()],
        "one object per content digest"
    );
    let object_dir = platform_dir.join(&sha);
    assert_eq!(
        names(&object_dir),
        vec!["prod-grok-backend.json".to_string()],
        "the object keeps the name of its first sighting"
    );
    match raw_export::read_export_index(&rig.stage(), MACHINE).unwrap() {
        raw_export::ExportIndexState::Loaded(rows) => {
            assert_eq!(rows.len(), 1, "one index row for one object: {rows:?}");
        }
        other => panic!("the index row must still be there: {other:?}"),
    }
}

/// A distinct export is a second object: the repeat rule above is about
/// identical bytes, and everything else is kept (invariant 3).
#[test]
fn a_distinct_grok_export_is_kept_as_its_own_object() {
    let rig = Rig::new();
    let first = rig.export();
    let first_out = import_grok(&rig, &first, Some(MACHINE));
    assert_eq!(
        first_out.status.code(),
        Some(GATE_REFUSED),
        "{}",
        text(&first_out)
    );

    let second = rig.root().join("prod-grok-backend.json");
    fs::write(&second, synthetic_grok_export(&[ID_A])).expect("write a distinct export");
    let second_out = import_grok(&rig, &second, Some(MACHINE));
    assert_eq!(
        second_out.status.code(),
        Some(GATE_REFUSED),
        "{}",
        text(&second_out)
    );

    let platform_dir = rig.stage().join(raw_export::EXPORT_FILES_DIR).join("grok");
    assert_eq!(names(&platform_dir).len(), 2, "both objects are kept");
}

/// A gated platform's object carries an index row under a machine's stage
/// partition, so the operator names the machine — and a run without the
/// label is a usage error that reads nothing and archives nothing.
#[test]
fn import_grok_refuses_to_run_without_the_machine_label() {
    let rig = Rig::new();
    let export = rig.export();

    let output = import_grok(&rig, &export, None);
    let shown = text(&output);
    assert_eq!(
        output.status.code(),
        Some(USAGE),
        "a missing --machine is a usage error, so 2 and not a silent default: {shown}"
    );
    assert!(
        shown.contains("--machine"),
        "the refusal must name the missing argument: {shown}"
    );
    assert!(
        !rig.stage().join(raw_export::EXPORT_FILES_DIR).exists(),
        "a usage error archives nothing"
    );
    assert!(
        !rig.stage().join(import::IMPORT_RAW_DIR).exists(),
        "a usage error archives nothing, in either namespace"
    );
    assert!(
        !rig.inbox().exists(),
        "a usage error writes nothing anywhere"
    );
}

/// The label is not a global knob: an ungated platform's run does not read
/// it, and a passed-but-unread argument is reported rather than ignored.
#[test]
fn the_skeleton_leg_refuses_a_machine_label_it_does_not_read() {
    let rig = Rig::new();
    let export = rig.root().join("conversations.json");
    fs::write(&export, synthetic_claude_export()).expect("write synthetic claude export");

    let args: Vec<String> = vec![
        "import".to_string(),
        "claude".to_string(),
        export.to_string_lossy().into_owned(),
        "--inbox".to_string(),
        rig.inbox().to_string_lossy().into_owned(),
        "--stage".to_string(),
        rig.stage().to_string_lossy().into_owned(),
        "--machine".to_string(),
        MACHINE.to_string(),
    ];
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = run(rig.root(), &borrowed);
    let shown = text(&output);
    assert_eq!(
        output.status.code(),
        Some(USAGE),
        "an argument the run does not read is a usage error, never an ignored \
         value: {shown}"
    );
    assert!(
        shown.contains("claude") && shown.contains("--machine"),
        "the refusal must name both the platform and the argument: {shown}"
    );
    assert!(
        !rig.stage().join(raw_export::EXPORT_FILES_DIR).exists(),
        "nothing was archived"
    );
}

/// A torn file was never read to the end: exit 3, nothing proven, nothing
/// archived in either namespace.
#[test]
fn import_grok_calls_a_truncated_export_a_read_that_never_finished() {
    let rig = Rig::new();
    let full = synthetic_grok_export(&[ID_A, ID_B]);
    let export = rig.root().join("prod-grok-backend.json");
    fs::write(&export, &full[..full.len() - 40]).expect("write truncated export");

    let output = import_grok(&rig, &export, Some(MACHINE));
    assert_eq!(
        output.status.code(),
        Some(DID_NOT_FINISH),
        "unreadable-to-the-end is 3: {}",
        text(&output)
    );
    assert!(
        !rig.stage().join(raw_export::EXPORT_FILES_DIR).exists(),
        "bytes that never parsed are not an export: the object namespace holds \
         exactly the files this producer accepted"
    );
    assert!(!rig.inbox().exists());
}

/// A Claude-shaped flat array was read in full and is the wrong input for
/// the Grok leg: exit 2, nothing archived.
#[test]
fn import_grok_refuses_the_wrong_shape_and_archives_nothing() {
    let rig = Rig::new();
    let export = rig.root().join("prod-grok-backend.json");
    fs::write(&export, b"[{\"conversation\": {\"id\": \"x\"}}]").expect("write claude-shaped file");

    let output = import_grok(&rig, &export, Some(MACHINE));
    assert_eq!(
        output.status.code(),
        Some(USAGE),
        "read in full and not a Grok export is 2: {}",
        text(&output)
    );
    assert!(
        !rig.stage().join(raw_export::EXPORT_FILES_DIR).exists(),
        "a refused input archives nothing"
    );
    assert!(!rig.inbox().exists());
}

/// A manifest is refused by name on the Grok leg too, and the refusal must
/// not echo the one-time download links it lists.
#[test]
fn import_grok_refuses_a_manifest_and_never_echoes_its_download_url() {
    let rig = Rig::new();
    let manifest = serde_json::json!({
        "data_files": [
            { "filename": "prod-grok-backend.json", "export_url": PLANTED_URL }
        ]
    })
    .to_string();
    let path = rig.root().join("grok-manifest.json");
    fs::write(&path, &manifest).expect("write synthetic manifest");

    let output = import_grok(&rig, &path, Some(MACHINE));
    let shown = text(&output);
    assert_eq!(
        output.status.code(),
        Some(USAGE),
        "a manifest was read completely and is the wrong input: {shown}"
    );
    assert!(shown.contains("manifest"), "{shown}");
    assert!(
        !shown.contains(PLANTED_URL) && !shown.contains("SECRETTOKEN1234567890"),
        "the refusal echoed a one-time-use export_url"
    );
    assert!(
        !rig.stage().join(raw_export::EXPORT_FILES_DIR).exists(),
        "a manifest must not reach the byte-exact namespace"
    );
}

/// The scarce property this file must never break by accident: the refusal
/// lives on one leg. Claude's run is the W920 skeleton and stays exactly as
/// it was — bundles into the inbox, the import-raw copy, no object.
#[test]
fn the_claude_leg_is_unchanged_by_the_grok_gate() {
    let rig = Rig::new();
    let export = rig.root().join("conversations.json");
    fs::write(&export, synthetic_claude_export()).expect("write synthetic claude export");

    let args: Vec<String> = vec![
        "import".to_string(),
        "claude".to_string(),
        export.to_string_lossy().into_owned(),
        "--inbox".to_string(),
        rig.inbox().to_string_lossy().into_owned(),
        "--stage".to_string(),
        rig.stage().to_string_lossy().into_owned(),
    ];
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = run(rig.root(), &borrowed);
    let shown = text(&output);
    assert_eq!(
        output.status.code(),
        Some(CLEAN),
        "the ungated claude run keeps the skeleton's contract: {shown}"
    );
    assert_eq!(
        names(&rig.inbox()).len(),
        2,
        "one bundle per conversation: {shown}"
    );
    let bytes = fs::read(&export).unwrap();
    assert!(
        rig.stage()
            .join(import::IMPORT_RAW_DIR)
            .join("claude")
            .join(hex_sha256(&bytes))
            .exists(),
        "the skeleton's byte-exact copy stays where W920 put it: {shown}"
    );
    assert!(
        !rig.stage().join(raw_export::EXPORT_FILES_DIR).exists(),
        "the claude leg archives no object in this build: {shown}"
    );
}

/// A Claude `conversations.json`, for the regression leg: the flat array of
/// 27-ORACLE §3.2, two conversations, text nobody wrote by hand.
fn synthetic_claude_export() -> String {
    let conversations: Vec<serde_json::Value> = [ID_A, ID_B]
        .iter()
        .enumerate()
        .map(|(i, id)| {
            serde_json::json!({
                "uuid": id,
                "name": format!("synthetic title {i}"),
                "created_at": "2026-06-01T00:00:00.000Z",
                "updated_at": "2026-06-02T00:00:00.000Z",
                "chat_messages": [
                    { "uuid": format!("{id}-m0"), "sender": "human", "text": format!("synthetic prompt {i}") },
                    { "uuid": format!("{id}-m1"), "sender": "assistant", "text": format!("synthetic answer {i}") },
                ],
            })
        })
        .collect();
    serde_json::to_string(&conversations).expect("serialise synthetic export")
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
