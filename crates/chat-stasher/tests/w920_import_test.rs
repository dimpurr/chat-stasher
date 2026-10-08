//! W920 — the `import` producer skeleton, driven through the real binary.
//!
//! `chat-stasher import <platform> <export> --inbox <dir> --stage <dir>` is the
//! producer side of ADR-055 D4 as far as this slice goes: it archives the whole
//! official-export file byte-exact in a stage namespace of its own, and emits one
//! ordinary `inbox@3` bundle per conversation into a **local** inbox folder. It
//! never seals, never pushes and never reads a config or a destination, so these
//! tests can point it at a `tempfile` sandbox and mean it.
//!
//! Three properties this file exists to keep honest:
//!
//!   * **The raw export stays outside every session count.** The exclusion is
//!     structural — `stage/sessions` is the only tree the counters walk — so the
//!     assertion is that a stage holding an archived export still counts zero.
//!   * **The export file is a source, not an input to be consumed.** It is left
//!     byte-identical and in place (ADR-011 read-only).
//!   * **The exit code says which kind of "no" this was** (invariant 2): `3` for
//!     "never finished reading", `2` for "read it, and it is not importable", `1`
//!     for a write that failed after a good read. A manifest and a truncated file
//!     are different failures and must not answer with the same integer.
//!
//! The manifest case carries the security half too: a Claude manifest's
//! `export_url` values are one-time-use credentials (27-ORACLE §3.2), so the
//! refusal names the file as a manifest *without* echoing a token from it, and
//! that absence is asserted, not assumed. It is not archived either — the
//! byte-exact namespace holds exactly the exports this producer imported, so a
//! run that answers `2` or `3` has written nothing anywhere.
//!
//! Every fixture here is synthetic — a two-conversation fake `conversations.json`
//! built in this file. No real takeout, archive, destination or inbox is touched.

use chat_stasher::{import, inbox, store};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

const DID_NOT_FINISH: i32 = 3;
const FINISHED_AND_FAILED: i32 = 1;
const WRONG_INPUT: i32 = 2;
const CLEAN: i32 = 0;

const ID_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const ID_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
/// A one-time download link, planted so its absence is a fact we checked rather
/// than a property we hoped for. Not a real credential format, not reachable.
const PLANTED_URL: &str = "https://claude.ai/export/one-time-use/SECRETTOKEN1234567890";

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

/// A synthetic Claude `conversations.json`: the flat array of 27-ORACLE §3.2,
/// two conversations, text nobody wrote by hand.
fn synthetic_export() -> String {
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
                    { "uuid": format!("{id}-m0"), "sender": "human",
                      "content": [{ "type": "text", "text": format!("synthetic prompt {i}") }] },
                    { "uuid": format!("{id}-m1"), "sender": "assistant",
                      "content": [{ "type": "text", "text": format!("synthetic answer {i}") }] },
                ],
            })
        })
        .collect();
    serde_json::to_string(&conversations).expect("serialise synthetic export")
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
        let path = self.root().join("conversations.json");
        fs::write(&path, synthetic_export()).expect("write synthetic export");
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

/// An absent inbox is a run that refused before publishing one, which is the
/// same answer as an empty one for these assertions — and a different run
/// failure, which the exit code is the place to say that.
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

/// The happy path, and the two structural promises that go with it.
#[test]
fn import_emits_valid_bundles_and_keeps_the_export_out_of_session_counts() {
    let rig = Rig::new();
    let export = rig.export();
    let source_bytes = fs::read(&export).unwrap();

    let output = import_cmd(&rig, "claude", &export);
    assert_eq!(
        output.status.code(),
        Some(CLEAN),
        "a good export imports cleanly: {}",
        text(&output)
    );

    let names = bundled_names(&rig.inbox());
    assert_eq!(
        names,
        vec![
            import::bundle_file_name("claude", ID_A),
            import::bundle_file_name("claude", ID_B)
        ],
        "one bundle per conversation, and no half-written .part behind"
    );
    for name in &names {
        let bytes = fs::read(rig.inbox().join(name)).unwrap();
        inbox::check_bundle(&bytes)
            .unwrap_or_else(|e| panic!("{name} is not an inbox@3 bundle: {e}"));
    }

    // The byte-exact copy, under its own namespace, named by its own digest.
    let dir = rig.stage().join(import::IMPORT_RAW_DIR).join("claude");
    let stored = dir.join(hex_sha256(&source_bytes));
    assert_eq!(fs::read(&stored).unwrap(), source_bytes, "byte-exact");

    // The exclusion this slice is judged on: a stage holding that export still
    // counts zero sessions, because nothing here wrote under `stage/sessions`.
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

    // And it did not seal anything: the inbox files are still there, unconsumed.
    assert!(
        !rig.inbox().join("consumed").exists(),
        "import must not act like ingest"
    );
}

/// A manifest is the wrong file, refused by name, and the refusal must not
/// become a channel for echoing a one-time-use download credential.
#[test]
fn import_refuses_a_manifest_and_never_echoes_its_download_url() {
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

    let output = import_cmd(&rig, "claude", &path);
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
    assert_eq!(
        store::sealed_shard_count(&rig.stage()).unwrap(),
        0,
        "a refused input archives no sessions"
    );
}

/// A file that stops mid-JSON was never read to the end. That is `3`, and the
/// difference between `3` and `2` is the whole content of invariant 2.
#[test]
fn import_calls_a_truncated_export_a_read_that_never_finished() {
    let rig = Rig::new();
    let full = synthetic_export();
    let path = rig.root().join("conversations.json");
    fs::write(&path, &full[..full.len() - 40]).expect("write truncated export");

    let output = import_cmd(&rig, "claude", &path);
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

/// A file that was read in full and is not JSON is invalid input, not a
/// read that stopped part-way: `2`, not `3`. A truncated file and a
/// malformed one are different failures and must not answer with the
/// same integer (invariant 2).
#[test]
fn import_calls_a_malformed_export_a_completed_read_of_invalid_input() {
    let rig = Rig::new();
    let path = rig.root().join("conversations.json");
    // A trailing comma: complete bytes that are not a JSON document.
    fs::write(&path, b"[{\"uuid\": \"x\",}]").expect("write malformed export");

    let output = import_cmd(&rig, "claude", &path);
    let shown = text(&output);
    assert_eq!(
        output.status.code(),
        Some(WRONG_INPUT),
        "read in full and not JSON is 2, not 3: {shown}"
    );
    assert!(
        bundled_names(&rig.inbox()).is_empty(),
        "a refused input writes nothing"
    );
    assert!(
        !rig.stage().join(import::IMPORT_RAW_DIR).exists(),
        "bytes that never parsed are not an export, so the byte-exact \
         namespace holds exactly the files this producer imported — and \
         nothing it refused"
    );
}

/// An absent export is the same class of failure as a truncated one: it was
/// never read. Not a usage error — the arguments were fine.
#[test]
fn import_calls_a_missing_export_a_read_that_never_started() {
    let rig = Rig::new();
    let output = import_cmd(&rig, "claude", &rig.root().join("no-such-export.json"));
    assert_eq!(
        output.status.code(),
        Some(DID_NOT_FINISH),
        "never read is 3: {}",
        text(&output)
    );
}

/// Platforms are named by `--platform` even where no parser exists yet, so the
/// answer is "not this build", not "unrecognized value".
#[test]
fn import_names_a_platform_it_has_no_parser_for() {
    let rig = Rig::new();
    let export = rig.export();
    let output = import_cmd(&rig, "deepseek", &export);
    let shown = text(&output);
    assert_eq!(
        output.status.code(),
        Some(WRONG_INPUT),
        "no parser is a wrong input for this build, and nothing was read: {shown}"
    );
    assert!(
        shown.contains("deepseek"),
        "the refusal must name it: {shown}"
    );
    assert_eq!(store::sealed_shard_count(&rig.stage()).unwrap(), 0);
    assert!(
        !rig.stage().join(import::IMPORT_RAW_DIR).exists(),
        "a platform with no parser archives nothing, not even the file"
    );
}

/// `import` never manufactures a stage: a typo in `--stage` must not scatter an
/// import-raw tree into the sandbox, and must be refused before reading.
#[test]
fn import_refuses_a_stage_that_is_not_there_and_creates_none() {
    let sandbox = tempfile::tempdir().expect("sandbox");
    let home = sandbox.path().join("home");
    fs::create_dir_all(&home).expect("home");
    fs::create_dir_all(sandbox.path().join("stage-parent")).expect("stage parent");
    let export = sandbox.path().join("conversations.json");
    fs::write(&export, synthetic_export()).expect("write synthetic export");
    let missing = sandbox.path().join("stage-parent/stage");

    let output = run(
        sandbox.path(),
        &[
            "import",
            "claude",
            &export.to_string_lossy(),
            "--inbox",
            &sandbox.path().join("inbox").to_string_lossy(),
            "--stage",
            &missing.to_string_lossy(),
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(WRONG_INPUT),
        "an absent stage is a wrong argument, not a failed read: {}",
        text(&output)
    );
    assert!(!missing.exists(), "import must not create a stage");
}

/// The archive read-back is deliberately unwired in this build, and the report
/// has to say so rather than let "0 observations" read as "everything changed".
/// The rule itself — equal body ⇒ an observation record and no bundle — is
/// exercised here through the CLI so the shipped default is the one on record.
#[test]
fn import_reports_the_unwired_archive_probe_instead_of_hiding_it() {
    let rig = Rig::new();
    let export = rig.export();
    let output = import_cmd(&rig, "claude", &export);
    let shown = text(&output);
    assert_eq!(output.status.code(), Some(CLEAN), "{shown}");
    assert!(
        shown.contains("unwired"),
        "the run must state that the archive comparison was not consulted: {shown}"
    );
    assert!(
        shown.contains(&import::IMPORT_RAW_DIR.to_string()),
        "the report must say where the byte-exact export went: {shown}"
    );
    // Every conversation got a body, because the probe could not say otherwise.
    assert_eq!(bundled_names(&rig.inbox()).len(), 2);
    assert!(
        !rig.stage().join(import::IMPORT_OBSERVATIONS_DIR).exists(),
        "an unwired probe writes no observation records"
    );
}

/// A write that fails after a complete read is `1`, the third code. Here the
/// inbox path is an existing *file*, so the bundles cannot be published even
/// though the export parsed.
#[test]
fn import_keeps_a_write_failure_apart_from_a_read_failure() {
    let rig = Rig::new();
    let export = rig.export();
    fs::create_dir_all(rig.root().join("blocked")).expect("blocked parent");
    let blocked = rig.root().join("blocked/inbox");
    fs::write(&blocked, b"not a directory\n").expect("occupy the inbox path");

    let output = run(
        rig.sandbox.path(),
        &[
            "import",
            "claude",
            &export.to_string_lossy(),
            "--inbox",
            &blocked.to_string_lossy(),
            "--stage",
            &rig.stage().to_string_lossy(),
        ],
    );
    let shown = text(&output);
    assert_eq!(
        output.status.code(),
        Some(FINISHED_AND_FAILED),
        "the export was read and understood; only the write failed, so 1: {shown}"
    );
    // The byte-exact export did land, which is why this is 1 and not 2: the run
    // got further than a wrong input does, and says so.
    assert!(
        rig.stage()
            .join(import::IMPORT_RAW_DIR)
            .join("claude")
            .exists(),
        "the raw archive step ran before the write that failed"
    );
}

fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
