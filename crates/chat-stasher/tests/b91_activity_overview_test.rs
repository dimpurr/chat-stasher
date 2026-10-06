//! B91 end-to-end: `activity-index` writes the sidecar, `push` archives it,
//! and `overview` reads it back out of the repository and renders it.
//!
//! This is the ADR-017 round-trip pinned as a black-box test through the real
//! binary and a real rustic repository. The three exit-code rules of `overview`
//! are asserted directly: 0 = read and rendered an index, 1 = read the whole
//! repo and there is no index anywhere (a real empty), and the machine-with-
//! snapshot-but-no-index case is *named*, never silently dropped.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

#[path = "../src/test_support.rs"]
mod test_support;

fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"B91 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("rh-cache"))
        .env("CHAT_STASHER_REGISTRY", &registry)
        .output()
        .unwrap()
}

/// One synthetic claude-code line with an RFC 3339 timestamp.
fn cc_line(ts: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
    )
}

/// One synthetic claude-code line that also states where the session ran and
/// which tenancy authored it.
fn cc_line_with_provenance(ts: &str, cwd: &str, organization: &str) -> String {
    format!(
        r#"{{"parentUuid":null,"type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":"{ts}","cwd":"{cwd}","ownerOrganizationUuid":"{organization}"}}"#
    )
}

/// Write one sealed shard for a session named `session` under `machine`.
fn write_shard(stage: &Path, machine: &str, session: &str, lines: &[String]) {
    let dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("000001.jsonl"), lines.join("\n") + "\n").unwrap();
}

fn sandbox() -> tempfile::TempDir {
    tempfile::TempDir::new().unwrap()
}

/// Happy path: index a stage, push it, and read the overview back. The machine
/// and harness must appear and the command must exit 0.
#[test]
fn activity_index_then_push_then_overview_roundtrips() {
    let sb = sandbox();
    let stage = sb.path().join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    write_shard(
        &stage,
        machine,
        session,
        &[
            cc_line("2025-01-15T12:34:56.789Z"),
            cc_line("2025-01-15T13:45:07Z"),
        ],
    );

    // 1. Build the sidecar.
    let out = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(
        out.status.success(),
        "activity-index failed: {:?}",
        out.status
    );
    let index = stage.join("meta").join(machine).join("activity-v1.jsonl");
    assert!(index.exists(), "activity index was not written");
    assert_eq!(fs::read_to_string(&index).unwrap().lines().count(), 1);

    // 2. Push the stage (sessions/ and meta/ together) into a fresh repo.
    let repo = sb.path().join("repo");
    let key = sb.path().join("keys").join("masterkey.json");
    let push = run(
        sb.path(),
        &[
            "push",
            "--stage",
            stage.to_str().unwrap(),
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            machine,
            "--keep-ssh-masters",
        ],
    );
    assert!(
        push.status.success(),
        "push failed: {:?}\n{}",
        push.status,
        String::from_utf8_lossy(&push.stderr)
    );

    // 3. Read the overview back.
    let ov = run(
        sb.path(),
        &[
            "overview",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--width",
            "80",
            "--keep-ssh-masters",
        ],
    );
    let stdout = String::from_utf8_lossy(&ov.stdout);
    assert_eq!(
        ov.status.code(),
        Some(0),
        "overview should exit 0, got {:?}\n{}",
        ov.status,
        stdout
    );
    assert!(
        stdout.contains("mbp-test"),
        "overview must name the machine:\n{stdout}"
    );
    assert!(
        stdout.contains("claude-code"),
        "overview must name the harness:\n{stdout}"
    );
    assert!(
        !stdout.contains("index missing"),
        "an indexed machine must not be flagged missing:\n{stdout}"
    );
}

#[test]
fn activity_index_counts_an_identical_shard_once() {
    let sb = sandbox();
    let stage = sb.path().join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    write_shard(
        &stage,
        machine,
        session,
        &[
            cc_line("2025-01-15T12:34:56.789Z"),
            cc_line("2025-01-15T13:45:07Z"),
        ],
    );
    let shard_dir = stage
        .join("sessions")
        .join(machine)
        .join(session)
        .join("000");
    let first = fs::read(shard_dir.join("000001.jsonl")).unwrap();
    fs::write(shard_dir.join("000002.jsonl"), first).unwrap();

    let out = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(
        out.status.success(),
        "activity-index failed: {:?}",
        out.status
    );
    let index = stage.join("meta").join(machine).join("activity-v1.jsonl");
    let row: serde_json::Value =
        serde_json::from_str(fs::read_to_string(index).unwrap().trim()).unwrap();
    assert_eq!(row["line_count"], 2, "duplicate shard lines count once");
}

/// TICKET-4D-02 · the dimensions a Claude Code transcript states about itself
/// reach the written index: the working directories the session ran in and the
/// organization that authored it.
///
/// This is the end-to-end half of the projection, and it is where the reading
/// could be lost: the index rebuild also folds in the typed dimensions an
/// archived *capture envelope* carries, and for a harness's own records there
/// are none — so a rebuild that replaces the row's dimensions with that reading
/// rather than merging it would publish an empty `dimensions` for every session
/// whose provenance this module had just read.
#[test]
fn activity_index_records_the_cwd_and_tenancy_the_transcript_states() {
    let sb = sandbox();
    let stage = sb.path().join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    // A session that moved: two directories, one organization.
    write_shard(
        &stage,
        machine,
        session,
        &[
            cc_line_with_provenance("2025-01-15T12:34:56.789Z", "/w/one/apps", "org-fixture"),
            cc_line_with_provenance("2025-01-15T13:45:07Z", "/w/one", "org-fixture"),
        ],
    );

    let out = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(
        out.status.success(),
        "activity-index failed: {:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let index = stage.join("meta").join(machine).join("activity-v1.jsonl");
    let row: serde_json::Value =
        serde_json::from_str(fs::read_to_string(index).unwrap().trim()).unwrap();
    assert_eq!(
        row["dimensions"]["cwd"],
        serde_json::json!(["/w/one", "/w/one/apps"]),
        "both directories the session ran in are recorded, in one stable order"
    );
    assert_eq!(
        row["dimensions"]["tenant"],
        serde_json::json!(["org-fixture"])
    );
    assert!(
        row["dimensions"].get("container").is_none(),
        "a path is not a repository identity, so nothing becomes a container: {}",
        row["dimensions"]
    );
}

/// A transcript whose records state no tenancy and no directory leaves both
/// dimensions **absent** from the index line — unobserved, not an empty string
/// and not a placeholder the reader could mistake for a fact.
#[test]
fn activity_index_leaves_an_unrecorded_dimension_absent() {
    let sb = sandbox();
    let stage = sb.path().join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    write_shard(
        &stage,
        machine,
        session,
        &[
            r#"{"type":"user","message":{"role":"user","content":"hi"},"timestamp":"2025-01-15T12:34:56.789Z"}"#.to_string(),
        ],
    );

    let out = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(
        out.status.success(),
        "activity-index failed: {:?}",
        out.status
    );

    let index = stage.join("meta").join(machine).join("activity-v1.jsonl");
    let row: serde_json::Value =
        serde_json::from_str(fs::read_to_string(index).unwrap().trim()).unwrap();
    assert!(
        row.get("dimensions").is_none(),
        "no dimension was observed, so no dimension object is written: {}",
        row
    );
}

/// One synthetic gemini-cli session document stating its `projectHash`.
fn gemini_document(session_id: &str, project_hash: &str) -> String {
    format!(
        r#"{{"sessionId":"{session_id}","projectHash":"{project_hash}","startTime":"2026-04-05T14:06:44.334Z","lastUpdated":"2026-04-05T14:09:50.892Z","messages":[{{"id":"m1","timestamp":"2026-04-05T14:06:44.334Z","type":"user","content":[{{"text":"hi"}}]}}],"kind":"main"}}"#
    )
}

/// TICKET-4D-07 · the `projectHash` a gemini-cli session document states about
/// itself reaches the written index as the session's `container` — a
/// repository/workspace identity, never a working directory.
#[test]
fn activity_index_records_the_project_hash_a_gemini_cli_document_states() {
    let sb = sandbox();
    let stage = sb.path().join("stage");
    let machine = "mbp-test";
    let session = "gemini-cli.mbp-test.session-2026-04-05T14-06-b9f717c6";
    write_shard(
        &stage,
        machine,
        session,
        &[gemini_document(
            "session-2026-04-05T14-06-b9f717c6",
            "79906c0e722ab1cb44a014d1b9ff4ff177a50fa7bbaf296a14f6fdaad1d294aa",
        )],
    );

    let out = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(
        out.status.success(),
        "activity-index failed: {:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let index = stage.join("meta").join(machine).join("activity-v1.jsonl");
    let row: serde_json::Value =
        serde_json::from_str(fs::read_to_string(index).unwrap().trim()).unwrap();
    assert_eq!(
        row["dimensions"]["container"],
        serde_json::json!(["79906c0e722ab1cb44a014d1b9ff4ff177a50fa7bbaf296a14f6fdaad1d294aa"]),
        "the document's own project hash is the session's container: {row}"
    );
    assert!(
        row["dimensions"].get("cwd").is_none(),
        "a digest is not a path, so it never becomes a cwd: {}",
        row["dimensions"]
    );
}

/// One synthetic opencode export envelope for a session whose own row states
/// whatever session fields the caller spells in. The export shape is the one
/// `sqlite_probe.rs` seals: one line per session, the whole `session` row
/// beside its messages.
fn oc_envelope(session: &str) -> String {
    format!(
        r#"{{"schema":"chat-stasher.opencode.session.v1","session":{{{session}}},"messages":[],"orphan_parts":[]}}"#
    )
}

/// TICKET-4D-01 · the facts an opencode export states about its session reach
/// the written index: the directory it ran in, the project it belonged to, and
/// the archive fact its row recorded.
#[test]
fn activity_index_records_the_dimensions_an_opencode_export_states() {
    let sb = sandbox();
    let stage = sb.path().join("stage");
    let machine = "mbp-test";
    let session = "opencode.mbp-test.sess-1-opencode-4d";
    write_shard(
        &stage,
        machine,
        session,
        &[oc_envelope(
            r#""id":"sess-1-opencode-4d","directory":"/w/one","project_id":"project-fixture","time_archived":1770000000000"#,
        )],
    );

    let out = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(
        out.status.success(),
        "activity-index failed: {:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let index = stage.join("meta").join(machine).join("activity-v1.jsonl");
    let row: serde_json::Value =
        serde_json::from_str(fs::read_to_string(index).unwrap().trim()).unwrap();
    assert_eq!(
        row["dimensions"]["cwd"],
        serde_json::json!(["/w/one"]),
        "the directory the session ran in is recorded as a path"
    );
    assert_eq!(
        row["dimensions"]["container"],
        serde_json::json!(["project-fixture"]),
        "the project key the harness recorded is recorded as a container"
    );
    assert_eq!(
        row["dimensions"]["status"],
        serde_json::json!(["archived"]),
        "a recorded archive moment is an archived session"
    );
}

/// An opencode row that recorded no project and no archive moment states those
/// two dimensions not at all, and the written index must not state them
/// either: the fields are absent from the line, never empty and never inferred.
#[test]
fn activity_index_leaves_an_opencode_dimension_the_export_stated_nothing_about_absent() {
    let sb = sandbox();
    let stage = sb.path().join("stage");
    let machine = "mbp-test";
    let session = "opencode.mbp-test.sess-1-opencode-4d";
    write_shard(
        &stage,
        machine,
        session,
        &[oc_envelope(
            r#""id":"sess-1-opencode-4d","directory":"/w/one","project_id":null,"time_archived":null"#,
        )],
    );

    let out = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(
        out.status.success(),
        "activity-index failed: {:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let index = stage.join("meta").join(machine).join("activity-v1.jsonl");
    let row: serde_json::Value =
        serde_json::from_str(fs::read_to_string(index).unwrap().trim()).unwrap();
    assert_eq!(
        row["dimensions"]["cwd"],
        serde_json::json!(["/w/one"]),
        "the one dimension the export did state is the one recorded"
    );
    assert!(
        row["dimensions"].get("container").is_none(),
        "no project key was stated, and none is invented from the directory: {}",
        row["dimensions"]
    );
    assert!(
        row["dimensions"].get("status").is_none(),
        "a null time_archived is an unobserved status, never an `active` one: {}",
        row["dimensions"]
    );
}

/// A machine that has a snapshot but no activity index must be *named* — it
/// must never vanish silently (that would fold "no index" into "no sessions").
#[test]
fn machine_with_snapshot_but_no_index_is_listed_missing() {
    let sb = sandbox();
    let stage = sb.path().join("stage");
    let machine = "mbp-test";
    write_shard(
        &stage,
        machine,
        "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
        &[cc_line("2025-01-15T12:34:56.789Z")],
    );
    // Deliberately NO meta/... — the machine archives sessions but never ran
    // activity-index, so it has a snapshot with no index.
    let repo = sb.path().join("repo");
    let key = sb.path().join("keys").join("masterkey.json");
    let push = run(
        sb.path(),
        &[
            "push",
            "--stage",
            stage.to_str().unwrap(),
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            machine,
            "--keep-ssh-masters",
        ],
    );
    assert!(push.status.success(), "push failed: {:?}", push.status);

    let ov = run(
        sb.path(),
        &[
            "overview",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--width",
            "80",
            "--keep-ssh-masters",
        ],
    );
    let stdout = String::from_utf8_lossy(&ov.stdout);
    // No index anywhere -> a real "there is none", exit 1 (never 3, and never 0).
    assert_eq!(
        ov.status.code(),
        Some(1),
        "overview with no index must exit 1, got {:?}\n{}",
        ov.status,
        stdout
    );
    assert!(
        stdout.contains("mbp-test") && stdout.contains("index missing"),
        "the snapshot-without-index machine must be named as missing:\n{stdout}"
    );
}

/// `overview --json --summary` over a real repository: aggregate totals, one
/// record per machine and per source, and exactly 30 local days — and
/// deliberately no per-session array, which is the variant's whole point for a
/// large archive.
#[test]
fn overview_json_summary_is_aggregate_only() {
    let sb = sandbox();
    let stage = sb.path().join("stage");
    let machine = "mbp-test";
    let session = "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    write_shard(
        &stage,
        machine,
        session,
        &[
            cc_line("2025-01-15T12:34:56.789Z"),
            cc_line("2025-01-15T13:45:07Z"),
        ],
    );

    let out = run(
        sb.path(),
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            machine,
        ],
    );
    assert!(
        out.status.success(),
        "activity-index failed: {:?}",
        out.status
    );

    let repo = sb.path().join("repo");
    let key = sb.path().join("keys").join("masterkey.json");
    let push = run(
        sb.path(),
        &[
            "push",
            "--stage",
            stage.to_str().unwrap(),
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--machine",
            machine,
            "--keep-ssh-masters",
        ],
    );
    assert!(push.status.success(), "push failed: {:?}", push.status);

    // Deliberately no `--keep-ssh-masters`: the `[reap] …` line that flag
    // prints goes to stdout and would break the one-JSON-object contract (a
    // pre-existing defect recorded at `tests/w15_ui_test.rs:375`). A local
    // `--repo` has no endpoint, so reaping prints nothing.
    let ov = run(
        sb.path(),
        &[
            "overview",
            "--json",
            "--summary",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
        ],
    );
    assert_eq!(
        ov.status.code(),
        Some(0),
        "overview --summary must exit 0\n{}",
        String::from_utf8_lossy(&ov.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&ov.stdout).unwrap();
    assert_eq!(v["schema_version"], serde_json::json!(1));
    assert_eq!(v["variant"], serde_json::json!("summary"));
    assert!(
        v.get("sessions").is_none(),
        "the summary variant must not carry the per-session array: {v}"
    );
    assert!(
        v["totals"]["sessions"].as_u64().unwrap() >= 1,
        "totals must count the archived session: {v}"
    );
    assert_eq!(v["machines"][0]["machine"], serde_json::json!("mbp-test"));
    assert!(
        v["machines"][0]["display"].is_string(),
        "the machine record carries a display name: {v}"
    );
    assert_eq!(v["sources"][0]["harness"], serde_json::json!("claude-code"));
    assert!(
        v["sources"][0]["last_saved_unix"].is_number(),
        "a known-time source has a numeric last_saved_unix: {v}"
    );
    assert_eq!(v["days"].as_array().unwrap().len(), 30);
}

/// An unreadable / un-openable repository is "could not finish reading": exit 3,
/// never 1. "No index" (1) must stay distinct from "could not look" (3).
#[test]
fn unopenable_repo_is_exit_3_not_exit_1() {
    let sb = sandbox();
    let repo = sb.path().join("repo-does-not-exist");
    let key = sb.path().join("keys").join("masterkey.json");
    let ov = run(
        sb.path(),
        &[
            "overview",
            "--repo",
            repo.to_str().unwrap(),
            "--key-file",
            key.to_str().unwrap(),
            "--width",
            "80",
            "--keep-ssh-masters",
        ],
    );
    assert_eq!(
        ov.status.code(),
        Some(3),
        "an unreadable repository must exit 3 (did not finish reading), not 1\n{}",
        String::from_utf8_lossy(&ov.stdout)
    );
}
