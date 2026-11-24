//! W1095 · RI-4 slice A: capture fidelity carried by the archive's own sealed
//! record headers must reach the two reader-facing surfaces this slice owns —
//! the activity index row (`meta/<machine>/activity-v1.jsonl`) and the
//! single-session `read` report header (ADR-051 D3).
//!
//! Every input is synthetic and travels the production paths: the inbox@3
//! bundles go through `inbox::seal_payload` (the seal-time producer surface),
//! locally captured shards through `store::write_sealed_shard` with the
//! `Collect` writer, the archive through `BackupStore::push`, and the two
//! assertions surfaces through the real binary (`activity-index`, `read`).
//! No fixture constructs the readers' end state by hand; the only hand-planted
//! bytes are files a previous build is documented to have written (an old
//! index row, an old incremental state file) and a restored corrupted record.
//!
//! The assertions parse the index JSONL and the CLI output as plain JSON /
//! strings, so this suite states what the unfixed tree lacks (no fidelity
//! field, no fidelity header — a behavioural red, not a compile error) and
//! stays meaningful after the fix.
//!
//! ADR-051 semantics pinned here: `captured` is what the sealed record header
//! stamped at capture time (D2); a body with no capture metadata resolves to
//! an explicit `declared/unknown` naming that it was not recorded — never an
//! empty value or a guess (D1); captured values are never overridden by a
//! declared reading (D2); disagreeing captured claims become an explicit
//! captured `unknown` rather than a silent pick (D1); an unreadable field is
//! an `unknown` with that reason, never a crash and never "not recorded".

use chat_stasher::store::{self, BackupStore, StoreConfig};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "../src/test_support.rs"]
mod test_support;

const MACHINE: &str = "w1095-fixture-machine";

/// The reason a body with no capture metadata carries (the same sentence
/// `BodyMetadata::legacy` writes into the audit sidecar): pinned so the
/// display cannot silently drift into an empty value or a different euphemism.
const NOT_RECORDED: &str = "capture metadata was not recorded";
/// The reason recorded when one session's sealed records stamp disagreeing
/// capture fidelity values: the session-level value is not established.
const DISAGREEMENT: &str = "this session's sealed records disagree on capture fidelity";
/// The reason recorded when a sealed record's capture fidelity field cannot
/// be parsed: staged but unreadable is not "not recorded".
const UNREADABLE: &str = "a sealed record's capture fidelity is unreadable";

fn sandbox() -> test_support::Sandbox {
    let s = test_support::Sandbox::new();
    s.ensure_dirs();
    s
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn fixture_bytes(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/fixtures/inbox").join(name),
    )
    .expect("committed synthetic inbox fixture")
}

/// The store config with the metadata cache pinned into this sandbox (W289),
/// so no repository open reaches the real user cache root.
fn store_config(root: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: root.join("repo").to_string_lossy().into_owned(),
        key_file: root.join("masterkey.json"),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(root.join("rustic-cache")),
        no_cache: false,
    }
}

/// A seeded (but empty) synthetic survey registry, the shape the child binary
/// accepts for tests. Its content is inert to the two commands this suite
/// drives; it exists so the child environment is fully synthetic.
fn plant_registry(sandbox: &test_support::Sandbox) -> PathBuf {
    let registry = sandbox.root().join("registry.json");
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W1095 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    registry
}

/// A spawned child with every ambient path inside the sandbox (W306 fixture)
/// and the synthetic registry wired.
fn child(sandbox: &test_support::Sandbox, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command.args(args);
    sandbox.apply(&mut command);
    command
        .env("CHAT_STASHER_REGISTRY", plant_registry(sandbox))
        .env_remove("RUSTIC_REPO")
        .env_remove("RUSTIC_KEY_FILE")
        .output()
        .unwrap()
}

/// Seal-package one synthetic inbox bundle through the production seal path
/// and return the archive identity it sealed under.
fn seal(sandbox: &test_support::Sandbox, bundle: &[u8]) -> String {
    let stage = sandbox.root().join("stage");
    fs::create_dir_all(&stage).unwrap();
    match chat_stasher::inbox::seal_payload(
        "synthetic.json",
        bundle,
        &stage,
        MACHINE,
        100,
        None,
        None,
    )
    .expect("synthetic bundle seals")
    {
        chat_stasher::inbox::SealOutcome::Stored(consumed) => consumed.id,
        other => panic!("expected a fresh synthetic seal, got: {other:?}"),
    }
}

/// Push the stage into a fresh synthetic repository (the real archive write)
/// and return the repo/key paths the children read through.
fn archive(sandbox: &test_support::Sandbox) -> (PathBuf, PathBuf) {
    let cfg = store_config(sandbox.root());
    let key = rustic_core::repofile::MasterKey::new();
    store::persist_key_file(&cfg, &key).unwrap();
    let store = BackupStore::new(cfg, MACHINE.to_string());
    store
        .push(&sandbox.root().join("stage"), &key)
        .expect("synthetic archive push");
    (sandbox.root().join("repo"), sandbox.root().join("masterkey.json"))
}

/// Build the activity index over the stage with the real binary and require
/// success. Returns the stdout text.
fn run_activity_index(root: &Path, sandbox: &test_support::Sandbox, incremental: bool) -> String {
    let mut args = vec![
        "activity-index".to_string(),
        "--stage".to_string(),
        root.join("stage").to_string_lossy().into_owned(),
        "--machine".to_string(),
        MACHINE.to_string(),
    ];
    if incremental {
        args.push("--incremental".into());
    }
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = child(sandbox, &args_ref);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "activity-index failed: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// Read one session's row out of the built activity index (plain JSON, so the
/// assertion survives either era of the row schema).
fn index_row(root: &Path, session: &str) -> Value {
    let index = root
        .join("stage")
        .join("meta")
        .join(MACHINE)
        .join("activity-v1.jsonl");
    let text = fs::read_to_string(&index)
        .unwrap_or_else(|e| panic!("read activity index {}: {e}", index.display()));
    for line in text.lines() {
        let row: Value =
            serde_json::from_str(line).expect("activity index lines are JSON objects");
        if row["session_id"] == json!(session) {
            return row;
        }
    }
    panic!("session `{session}` has no activity index row");
}

/// The full-session read report from the real binary, over the synthetic
/// repository. Returns the stdout text after requiring success.
fn run_read(root: &Path, sandbox: &test_support::Sandbox, session: &str) -> String {
    let (repo, key) = archive_paths(root);
    let out = child(
        sandbox,
        &[
            "read",
            "--machine",
            MACHINE,
            "--session",
            session,
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "read failed: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

fn archive_paths(root: &Path) -> (PathBuf, PathBuf) {
    (root.join("repo"), root.join("masterkey.json"))
}

fn fidelity_line(stdout: &str) -> String {
    stdout
        .lines()
        .find(|line| line.starts_with("[read] fidelity"))
        .expect("the read report carries a fidelity header line")
        .to_string()
}

/// 1 · A web capture, sealed with `captured full/api` at capture time, must
/// surface that exact fidelity in both the index row and the read header.
#[test]
fn inbox_v3_captured_full_api_reaches_the_activity_row_and_read_header() {
    let sb = sandbox();
    let root = sb.root();
    seal(&sb, &fixture_bytes("web-full.json"));
    archive(&sb);
    run_activity_index(root, &sb, false);

    let row = index_row(root, "deepseek.synthetic-session");
    assert_eq!(
        row["fidelity"],
        json!({"source": "captured", "value": "full", "representation": "api"}),
        "the index row carries the sealed record's captured fidelity",
    );

    let stdout = run_read(root, &sb, "deepseek.synthetic-session");
    assert_eq!(
        fidelity_line(&stdout),
        "[read] fidelity       : captured full (representation: api)",
        "the read report shows the captured fidelity, not empty, not declared"
    );
}

/// 2 · `captured partial` with its note and `captured unknown` with its reason
/// must survive the whole path byte-honestly — the note and the reason are the
/// point of the field, not decoration.
#[test]
fn captured_partial_note_and_unknown_reason_are_preserved_end_to_end() {
    let sb = sandbox();
    let root = sb.root();
    // Same synthetic raw body, different native ids, so the two bundles seal
    // as two sessions rather than deduping into one.
    let partial = fixture_bytes("harness-partial.json");
    let partial = String::from_utf8(partial)
        .unwrap()
        .replace("synthetic-native", "synthetic-native-partial")
        .into_bytes();
    let unknown = fixture_bytes("harness-unknown.json");
    let unknown = String::from_utf8(unknown)
        .unwrap()
        .replace("synthetic-native", "synthetic-native-unknown")
        .into_bytes();
    let partial_session = seal(&sb, &partial);
    let unknown_session = seal(&sb, &unknown);
    archive(&sb);
    run_activity_index(root, &sb, false);

    let row = index_row(root, &partial_session);
    assert_eq!(
        row["fidelity"],
        json!({"source": "captured", "value": "partial", "note": "Synthetic missing events"}),
    );
    assert_eq!(
        fidelity_line(&run_read(root, &sb, &partial_session)),
        "[read] fidelity       : captured partial (note: Synthetic missing events)",
    );

    let row = index_row(root, &unknown_session);
    assert_eq!(
        row["fidelity"],
        json!({"source": "captured", "value": "unknown", "reason": "Synthetic unverified source"}),
    );
    assert_eq!(
        fidelity_line(&run_read(root, &sb, &unknown_session)),
        "[read] fidelity       : captured unknown (reason: Synthetic unverified source)",
    );
}

/// 3 · A locally captured session (raw harness lines, sealed through the
/// collect writer) has no capture metadata anywhere in its body; both reader
/// surfaces must say so explicitly — `declared unknown (+ reason)`, never an
/// empty value and never an invented fidelity.
#[test]
fn local_capture_without_capture_metadata_reads_as_explicit_declared_unknown() {
    let sb = sandbox();
    let root = sb.root();
    let session = "claude-code.w1095-fixture-machine.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    let stage = root.join("stage");
    fs::create_dir_all(&stage).unwrap();
    store::write_sealed_shard(
        store::StageWriter::Collect,
        &stage,
        MACHINE,
        session,
        &[r#"{"sessionId":"s","type":"user","message":{"role":"user","content":"hi"},"uuid":"u1","timestamp":"2025-01-15T12:34:56.789Z"}"#
            .to_string()],
    )
    .expect("synthetic local capture seals");
    archive(&sb);
    run_activity_index(root, &sb, false);

    let row = index_row(root, session);
    assert_eq!(
        row["fidelity"],
        json!({"source": "declared", "value": "unknown", "reason": NOT_RECORDED}),
        "a body with no capture metadata is explicit declared/unknown with the reason on record",
    );
    assert_eq!(
        fidelity_line(&run_read(root, &sb, session)),
        format!("[read] fidelity       : declared unknown (reason: {NOT_RECORDED})"),
    );
}

/// 4 · An old inbox@1 bundle (a whole schema generation predating capture
/// metadata) is archived by the current tree; both surfaces must read it as
/// explicit declared/unknown for the missing-metadata reason.
#[test]
fn legacy_bundle_missing_fidelity_reads_as_explicit_declared_unknown() {
    let sb = sandbox();
    let root = sb.root();
    seal(&sb, &fixture_bytes("legacy-v1.json"));
    archive(&sb);
    run_activity_index(root, &sb, false);

    let row = index_row(root, "deepseek.synthetic-session");
    assert_eq!(
        row["fidelity"],
        json!({"source": "declared", "value": "unknown", "reason": NOT_RECORDED}),
    );
    assert_eq!(
        fidelity_line(&run_read(root, &sb, "deepseek.synthetic-session")),
        format!("[read] fidelity       : declared unknown (reason: {NOT_RECORDED})"),
    );
}

/// 5 · Captured is never overridden by a declared reading:
///   - one session holds both a legacy record (whose fallback reading is
///     declared/unknown) and a modern record stamped captured — the output
///     must be the captured value;
///   - a previous build's state file and index row (the shapes an era before
///     fidelity would have written) may not carry a stale declared reading
///     forward: rebuilding from the body re-derives the captured value.
#[test]
fn captured_fidelity_is_not_overridden_by_a_declared_peer_nor_stale_index_state() {
    let sb = sandbox();
    let root = sb.root();
    let session = "deepseek.synthetic-session";
    seal(&sb, &fixture_bytes("legacy-v1.json"));
    seal(&sb, &fixture_bytes("web-full.json"));
    archive(&sb);

    // Files only a previous build writes: an incremental state file of the
    // previous state format, and a previous index row carrying a declared
    // fidelity for this session. Both parse in the current tree (additive
    // reading); neither may make the captured value invisible.
    let meta = root
        .join("stage")
        .join("meta")
        .join(MACHINE);
    fs::create_dir_all(&meta).unwrap();
    let previous_state = json!({
        "format": 3,
        "policy": "keep-all",
        "sessions": {
            session: {
                "shards": [{"seq": 1, "bytes": 2, "sha256": sha256_hex(b"{}")}],
                "measured": {"shard_count": 1, "concat_sha256": sha256_hex(b"{}")},
                "observations": null,
            }
        }
    });
    fs::write(
        meta.join(".activity-index-state-v1.json"),
        serde_json::to_string(&previous_state).unwrap(),
    )
    .unwrap();
    fs::write(
        meta.join("activity-v1.jsonl"),
        format!(
            "{{\"session_id\":\"{session}\",\"machine\":\"{MACHINE}\",\"harness\":\"deepseek\",\"first_unix\":null,\"last_unix\":null,\"line_count\":1,\"time_source\":{{\"kind\":\"unknown\",\"why\":\"synthetic previous build\"}},\"fidelity\":{{\"source\":\"declared\",\"value\":\"unknown\",\"reason\":\"synthetic stale declared row\"}}}}\n"
        ),
    )
    .unwrap();

    let stdout = run_activity_index(root, &sb, true);
    assert!(
        !stdout.contains("declared"),
        "the index build must not report a declared carried row: {stdout}"
    );
    let row = index_row(root, session);
    assert_eq!(
        row["fidelity"],
        json!({"source": "captured", "value": "full", "representation": "api"}),
        "captured fidelity survives a legacy peer record and a stale declared row",
    );
    assert_ne!(row["fidelity"]["source"], json!("declared"));

    assert_eq!(
        fidelity_line(&run_read(root, &sb, session)),
        "[read] fidelity       : captured full (representation: api)",
    );
}

/// 6 · Two captures of one session that honestly stamp different fidelity
/// values must not collapse to either one: the session-level value is not
/// established, and the output says so as a captured unknown with the
/// disagreement on record — never a silent pick and never the declared
/// fallback.
#[test]
fn disagreeing_captured_values_become_captured_unknown_never_a_silent_pick() {
    let sb = sandbox();
    let root = sb.root();
    let session = "deepseek.synthetic-session";
    seal(&sb, &fixture_bytes("web-full.json"));
    // A second, later capture of the same session: a different raw body (so
    // it does not dedupe) whose producer stamped a different fidelity value.
    let second = json!({
        "schema": "chat-stasher/inbox@3",
        "platform": "deepseek",
        "sessionId": "synthetic-session",
        "kind": "web-capture",
        "raw": {"text": "{\"capture\":2}", "bytes": 13},
        "fidelity": {"value": "raw"},
    });
    seal(&sb, &serde_json::to_vec(&second).unwrap());
    archive(&sb);
    run_activity_index(root, &sb, false);

    let row = index_row(root, session);
    assert_eq!(
        row["fidelity"],
        json!({"source": "captured", "value": "unknown", "reason": DISAGREEMENT}),
    );
    assert_eq!(
        fidelity_line(&run_read(root, &sb, session)),
        format!("[read] fidelity       : captured unknown (reason: {DISAGREEMENT})"),
    );
}

/// 7 · A sealed record whose capture fidelity field cannot be parsed (the
/// bytes a restored, corrupted record would write back) is an explicit
/// unknown naming the unreadable metadata — the index build and the read both
/// complete (exit 0), and a runtime registry that cannot be parsed changes
/// neither output: slice A's resolution reads the archived record headers,
/// never the harness registry.
#[test]
fn unreadable_capture_metadata_is_explicit_unknown_and_ignores_a_broken_registry() {
    let sb = sandbox();
    let root = sb.root();
    let session = "deepseek.synthetic-session";
    let stage = root.join("stage");
    fs::create_dir_all(&stage).unwrap();
    let record = json!({
        "schema": "chat-stasher/inbox@3",
        "kind": "web-capture",
        "platform": "deepseek",
        "sessionId": "synthetic-session",
        "file_sha256": sha256_hex(b"{\"corrupt\":true}"),
        "file_bytes": 15,
        "raw": {"text": "{\"corrupt\":true}", "bytes": 15},
        "fidelity": {"value": "not-a-fidelity-state"},
    });
    store::write_sealed_shard(
        store::StageWriter::Collect,
        &stage,
        MACHINE,
        session,
        &[serde_json::to_string(&record).unwrap()],
    )
    .expect("the synthetic corrupted record seals");
    archive(&sb);

    // A child with the runtime registry pointing at unreadable bytes: neither
    // reader surface consults it for fidelity, so its state changes nothing.
    let broken_registry_child = |sandbox: &test_support::Sandbox, args: &[&str]| -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        command.args(args);
        sandbox.apply(&mut command);
        let broken = sandbox.root().join("broken-registry.json");
        fs::write(&broken, ")not json(").unwrap();
        command.env("CHAT_STASHER_REGISTRY", broken);
        command.output().unwrap()
    };

    let out = broken_registry_child(
        &sb,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "the index build completes over unreadable capture metadata: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row = index_row(root, session);
    assert_eq!(
        row["fidelity"],
        json!({"source": "captured", "value": "unknown", "reason": UNREADABLE}),
    );

    let (repo, key_file) = archive_paths(root);
    let out = broken_registry_child(
        &sb,
        &[
            "read",
            "--machine",
            MACHINE,
            "--session",
            session,
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key_file.to_str().unwrap(),
            "--keep-ssh-masters",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "read completes: {stdout}");
    assert_eq!(
        fidelity_line(&stdout),
        format!("[read] fidelity       : captured unknown (reason: {UNREADABLE})"),
    );
}
