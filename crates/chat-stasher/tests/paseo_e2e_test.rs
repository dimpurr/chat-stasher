//! PASEO-1d: end-to-end for the two native sources Paseo points at, Codex and
//! Grok CLI: native input -> `run-once` (collect + push) -> `read` / `export`
//! / `search` -> archive-only recovery with the sources, the stage and the
//! collector state removed -> a re-run that must not duplicate anything.
//!
//! WHAT THIS IS NOT. The TODO row asks for frozen *real* native inputs. This
//! file runs on a schema-faithful **synthetic** freeze: the record shapes below
//! follow the Codex rollout format (`session_meta` / `response_item` /
//! `event_msg`, active `sessions/` plus sibling `archived_sessions/` with a
//! `.jsonl.zst` rollout) and the Grok CLI format (`session_search.sqlite`
//! `session_docs` row plus `chat_history.jsonl` with system / user / reasoning
//! / assistant / tool_result records), but every text, id, path and time is
//! invented. Nothing here proves behaviour against a real Codex or Grok tree;
//! that verification remains to be done on real inputs.
//!
//! The Paseo half uses only what ADR-056 already decides and the tree already
//! has: the Codex handle resolver (D3), the observation contract (D5) and the
//! rule that agent counts and distinct native-body counts are separate (D6).
//! There is no Grok resolver, no `paseo-agents-v1.jsonl` writer and no reader
//! in the product yet, so the agent observations are built by the test from
//! the archive's own `read` output and validated by the real contract code.

use chat_stasher::paseo::{
    parse_rows, AgentObservation, AgentObservationRow, AgentProjection, BodyState,
};
use chat_stasher::paseo_codex::{archive_session_id, resolve_rollout, CodexResolution};
use chat_stasher::store::{persist_key_file, StoreConfig};
use rustic_core::repofile::MasterKey;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "../src/test_support.rs"]
mod test_support;

const MACHINE: &str = "fixture-machine";

const CODEX_ACTIVE_UUID: &str = "019bf00d-0000-7000-8000-0000000000a1";
const CODEX_ACTIVE_STEM: &str = "rollout-2026-09-01T10-00-00-019bf00d-0000-7000-8000-0000000000a1";
const CODEX_ARCHIVED_UUID: &str = "019bf00d-0000-7000-8000-0000000000b2";
const CODEX_ARCHIVED_STEM: &str =
    "rollout-2026-09-02T11-30-00-019bf00d-0000-7000-8000-0000000000b2";
/// Grok session with BOTH observations: a `session_docs` row and a native
/// `chat_history.jsonl`.
const GROK_BOTH: &str = "0198f00d-1111-7000-8000-0000000000c3";
/// Grok session with only the native transcript (no row).
const GROK_NATIVE_ONLY: &str = "0198f00d-1111-7000-8000-0000000000d4";

const TOKEN_CODEX_ACTIVE: &str = "qzcodexactivealpha";
const TOKEN_CODEX_ARCHIVED: &str = "qzcodexarchivedbeta";
const TOKEN_GROK_BOTH: &str = "qzgrokbothgamma";
const TOKEN_GROK_NATIVE: &str = "qzgroknativedelta";

fn codex_id(stem: &str) -> String {
    archive_session_id(MACHINE, stem)
}

fn grok_id(session: &str) -> String {
    format!("grok.{MACHINE}.{session}")
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// --------------------------------------------------------------- the freeze

fn codex_rollout(uuid: &str, token: &str) -> Vec<u8> {
    let lines = [
        format!(
            r#"{{"timestamp":"2026-09-01T10:00:00.000Z","type":"session_meta","payload":{{"id":"{uuid}","timestamp":"2026-09-01T10:00:00.000Z","cwd":"/synthetic/work","originator":"codex_cli_rs","cli_version":"0.0.0-synthetic","source":"cli","instructions":null}}}}"#
        ),
        format!(
            r#"{{"timestamp":"2026-09-01T10:00:01.000Z","type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"synthetic question {token}"}}]}}}}"#
        ),
        format!(
            r#"{{"timestamp":"2026-09-01T10:00:01.000Z","type":"event_msg","payload":{{"type":"user_message","message":"synthetic question {token}"}}}}"#
        ),
        r#"{"timestamp":"2026-09-01T10:00:02.000Z","type":"response_item","payload":{"type":"reasoning","summary":[],"content":null,"encrypted_content":"synthetic-opaque"}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:03.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"ls\"}","call_id":"call-1"}}"#.to_string(),
        r#"{"timestamp":"2026-09-01T10:00:04.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"3 files"}}"#.to_string(),
        format!(
            r#"{{"timestamp":"2026-09-01T10:00:05.000Z","type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"synthetic answer {token}"}}]}}}}"#
        ),
        r#"{"timestamp":"2026-09-01T10:00:06.000Z","type":"event_msg","payload":{"type":"token_count","info":{}}}"#.to_string(),
    ];
    let mut body = lines.join("\n");
    body.push('\n');
    body.into_bytes()
}

fn grok_history(token: &str) -> Vec<u8> {
    let lines = [
        r#"{"type":"system","content":"synthetic system text"}"#.to_string(),
        r#"{"type":"user","synthetic_reason":"system_reminder","content":[{"type":"text","text":"synthetic injected reminder"}]}"#.to_string(),
        format!(
            r#"{{"type":"user","content":[{{"type":"text","text":"synthetic user turn {token}"}}]}}"#
        ),
        r#"{"type":"reasoning","encrypted_content":"synthetic-ciphertext","id":"r1","status":"completed","summary":[{"type":"summary_text","text":"synthetic reasoning summary"}]}"#.to_string(),
        r#"{"type":"assistant","content":"synthetic tool preface","tool_calls":[{"name":"synthetic_tool","arguments":"{\"q\":\"x\"}","id":"call-1"}]}"#.to_string(),
        r#"{"type":"tool_result","content":"synthetic tool output","tool_call_id":"call-1"}"#.to_string(),
        format!(r#"{{"type":"assistant","content":"synthetic answer {token}"}}"#),
    ];
    let mut body = lines.join("\n");
    body.push('\n');
    body.into_bytes()
}

/// The frozen native inputs: what each source file's bytes are, so the
/// archive can be compared with the source rather than with itself.
struct Freeze {
    codex_active: Vec<u8>,
    codex_archived_plain: Vec<u8>,
    codex_archived_zst: Vec<u8>,
    grok_both_history: Vec<u8>,
    grok_native_history: Vec<u8>,
}

impl Freeze {
    fn new() -> Freeze {
        let codex_archived_plain = codex_rollout(CODEX_ARCHIVED_UUID, TOKEN_CODEX_ARCHIVED);
        Freeze {
            codex_active: codex_rollout(CODEX_ACTIVE_UUID, TOKEN_CODEX_ACTIVE),
            codex_archived_zst: zstd::encode_all(&codex_archived_plain[..], 3).unwrap(),
            codex_archived_plain,
            grok_both_history: grok_history(TOKEN_GROK_BOTH),
            grok_native_history: grok_history(TOKEN_GROK_NATIVE),
        }
    }

    /// Plant the freeze under `home` exactly where each tool keeps it.
    fn plant(&self, home: &Path) {
        let codex = home.join(".codex");
        let active = codex.join("sessions/2026/09/01");
        fs::create_dir_all(&active).unwrap();
        fs::write(
            active.join(format!("{CODEX_ACTIVE_STEM}.jsonl")),
            &self.codex_active,
        )
        .unwrap();
        let archived = codex.join("archived_sessions/2026/09/02");
        fs::create_dir_all(&archived).unwrap();
        fs::write(
            archived.join(format!("{CODEX_ARCHIVED_STEM}.jsonl.zst")),
            &self.codex_archived_zst,
        )
        .unwrap();

        let sessions = home.join(".grok/sessions");
        fs::create_dir_all(&sessions).unwrap();
        let conn = rusqlite::Connection::open(sessions.join("session_search.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_docs (session_id TEXT PRIMARY KEY, cwd TEXT NOT NULL, updated_at INTEGER NOT NULL, title TEXT NOT NULL, content TEXT NOT NULL, content_hash TEXT NOT NULL);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_docs VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                GROK_BOTH,
                "/synthetic/work",
                1_788_000_000_i64,
                "synthetic title",
                format!("synthetic legacy summary {TOKEN_GROK_BOTH}"),
                "synthetic hash"
            ],
        )
        .unwrap();
        drop(conn);
        for (session, history) in [
            (GROK_BOTH, &self.grok_both_history),
            (GROK_NATIVE_ONLY, &self.grok_native_history),
        ] {
            let dir = sessions.join("synthetic-cwd").join(session);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("chat_history.jsonl"), history).unwrap();
        }
    }
}

// --------------------------------------------------------------- the harness

/// One machine's world: HOME, config/state/cache dirs and a registry that
/// carries the shipped Codex and Grok cells (the shapes are copied, in every
/// platform slot, so the test runs the same code on any host OS).
struct World {
    root: tempfile::TempDir,
}

impl World {
    fn new() -> World {
        let world = World {
            root: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(world.home()).unwrap();
        world.write_registry();
        world
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn home(&self) -> PathBuf {
        self.path().join("home")
    }

    fn stage(&self) -> PathBuf {
        self.path().join("stage")
    }

    fn write_registry(&self) {
        let codex_cell = |template: &str, confidence: &str| {
            serde_json::json!({
                "template": template, "env_override": "CODEX_HOME",
                "format": "jsonl / jsonl.zst", "confidence": confidence,
                "source": "synthetic PASEO-1d fixture"
            })
        };
        let grok_cell = serde_json::json!({
            "template": "~/.grok/sessions/session_search.sqlite",
            "format": "sqlite", "confidence": "measured-locally",
            "source": "synthetic PASEO-1d fixture",
            "sql_table": "session_docs", "sql_id_column": "session_id",
            "sql_required_columns": ["session_id", "updated_at"],
            "sql_time_column": "updated_at", "sql_time_value_is_seconds": true
        });
        let all = |cell: &serde_json::Value| serde_json::json!({"macos": cell, "linux": cell, "windows": cell});
        let registry = serde_json::json!({
            "schema_version": 1,
            "generated": "PASEO-1d synthetic",
            "harnesses": [
                {
                    "id": "codex",
                    "display_name": "synthetic codex",
                    "paths": all(&codex_cell("~/.codex/sessions/", "source-confirmed")),
                    "source_roots": [{
                        "id": "codex-archived",
                        "paths": all(&codex_cell("~/.codex/archived_sessions/", "measured-locally")),
                        "movable": true,
                        "provenance": {"status": ["archived"]}
                    }]
                },
                {
                    "id": "grok",
                    "display_name": "synthetic grok",
                    "paths": all(&grok_cell)
                }
            ]
        });
        fs::write(
            self.path().join("registry.json"),
            serde_json::to_vec_pretty(&registry).unwrap(),
        )
        .unwrap();
    }

    fn cli(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        cmd.env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(self.path()),
            )
            .env(
                "XDG_CONFIG_HOME",
                test_support::consent_config_home(self.path().join("config"), &["codex", "grok"]),
            )
            .env("XDG_DATA_HOME", self.path().join("data"))
            .env("XDG_STATE_HOME", self.path().join("state"))
            .env("XDG_CACHE_HOME", self.path().join("cache"))
            // Windows keeps the search index under %LOCALAPPDATA% (not XDG);
            // without this the recovery world would inherit the first world's
            // index by sharing its repo path, and "no index yet" would be false.
            .env("LOCALAPPDATA", self.path().join("localappdata"))
            .env("APPDATA", self.path().join("appdata"))
            .env("CHAT_STASHER_REGISTRY", self.path().join("registry.json"))
            .env_remove("CODEX_HOME")
            .env_remove("RUSTIC_REPO")
            .env_remove("RUSTIC_KEY_FILE");
        cmd
    }
}

/// The archive under test lives outside every `World` so a world can be
/// thrown away whole.
struct Archive {
    dir: tempfile::TempDir,
}

impl Archive {
    fn new() -> Archive {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive { dir };
        let cfg = StoreConfig {
            repo_root: archive.repo().to_string_lossy().into_owned(),
            key_file: archive.key(),
            connections: 1,
            options: Default::default(),
            cache_dir: None,
            no_cache: true,
        };
        persist_key_file(&cfg, &MasterKey::new()).unwrap();
        archive
    }

    fn repo(&self) -> PathBuf {
        self.dir.path().join("repo")
    }

    fn key(&self) -> PathBuf {
        self.dir.path().join("masterkey.json")
    }

    fn args(&self) -> Vec<String> {
        vec![
            "--repo".into(),
            self.repo().to_string_lossy().into_owned(),
            "--key-file".into(),
            self.key().to_string_lossy().into_owned(),
        ]
    }

    /// Every object path in the repository, to compare before/after a re-run.
    fn objects(&self) -> BTreeSet<String> {
        let repo = self.repo();
        let mut names = BTreeSet::new();
        let mut stack = vec![repo.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    names.insert(
                        path.strip_prefix(&repo)
                            .unwrap()
                            .components()
                            .map(|c| c.as_os_str().to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                            .join("/"),
                    );
                }
            }
        }
        names
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn run_once(world: &World, archive: &Archive) -> Output {
    let output = world
        .cli()
        .args(["run-once", "--stage"])
        .arg(world.stage())
        .args(["--machine", MACHINE, "--keep-ssh-masters"])
        .args(archive.args())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "run-once failed ({:?}):\n{}",
        output.status.code(),
        text(&output)
    );
    output
}

/// `session <full id> shards=N bytes=N sha256=<hex>` rows of `read`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadRow {
    shards: u64,
    bytes: u64,
    sha256: String,
}

fn read_all(world: &World, archive: &Archive) -> BTreeMap<String, ReadRow> {
    let output = world
        .cli()
        .args(["read", "--all-machines", "--full-ids", "--keep-ssh-masters"])
        .args(archive.args())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "read failed:\n{}",
        text(&output)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut rows = BTreeMap::new();
    for line in stdout.lines() {
        let Some(rest) = line.trim_start().strip_prefix("session ") else {
            continue;
        };
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        let field = |name: &str| {
            tokens
                .iter()
                .find_map(|token| token.strip_prefix(name))
                .unwrap_or_else(|| panic!("read row lacks {name}: {line}"))
        };
        rows.insert(
            tokens[0].to_string(),
            ReadRow {
                shards: field("shards=").parse().unwrap(),
                bytes: field("bytes=").parse().unwrap(),
                sha256: field("sha256=").to_string(),
            },
        );
    }
    rows
}

/// The search index is a local, derived structure: each world builds its own
/// from the archive. Before it exists a search is UNKNOWN (exit 3), never an
/// empty answer.
fn assert_search_unknown_until_indexed(world: &World, archive: &Archive) {
    let output = world
        .cli()
        .args([
            "search",
            "--text",
            TOKEN_CODEX_ACTIVE,
            "--json",
            "--keep-ssh-masters",
        ])
        .args(archive.args())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "a search with no index must be unknown, not empty:
{}",
        text(&output)
    );
}

fn build_index(world: &World, archive: &Archive) {
    let output = world
        .cli()
        .args(["index", "build", "--keep-ssh-masters"])
        .args(archive.args())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "index build:
{}",
        text(&output)
    );
}

/// Run a search; return (matched count from `--json`, plain-text report). The
/// JSON report carries counts only, so the id check reads the plain report.
fn search(world: &World, archive: &Archive, needle: &str) -> (u64, String) {
    let run = |json: bool| {
        let mut cmd = world.cli();
        cmd.args(["search", "--text", needle]);
        if json {
            cmd.arg("--json");
        }
        let output = cmd
            .args(["--keep-ssh-masters"])
            .args(archive.args())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "search for `{needle}` failed:\n{}",
            text(&output)
        );
        output
    };
    let value: serde_json::Value = serde_json::from_slice(&run(true).stdout)
        .unwrap_or_else(|e| panic!("search --json is not one JSON object: {e}"));
    assert_eq!(
        value["index_covered"], 4,
        "every session must be covered by the index: {value}"
    );
    assert_eq!(value["index_missing"], 0);
    assert_eq!(value["index_not_indexable"], 0);
    let plain = text(&run(false));
    println!("plain search report:\n{plain}");
    (value["matched"].as_u64().unwrap(), plain)
}

fn export_all(world: &World, archive: &Archive, out: &Path) -> Output {
    world
        .cli()
        .args(["export", "--out"])
        .arg(out)
        .args(["--keep-ssh-masters"])
        .args(archive.args())
        .output()
        .unwrap()
}

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

// ------------------------------------------------------------------- checks

/// The four native sessions this fixture must archive, with the source-derived
/// digest where the source determines the body exactly.
fn expected_ids() -> BTreeSet<String> {
    [
        codex_id(CODEX_ACTIVE_STEM),
        codex_id(CODEX_ARCHIVED_STEM),
        grok_id(GROK_BOTH),
        grok_id(GROK_NATIVE_ONLY),
    ]
    .into_iter()
    .collect()
}

fn assert_bodies_match_the_freeze(rows: &BTreeMap<String, ReadRow>, freeze: &Freeze, when: &str) {
    assert_eq!(
        rows.keys().cloned().collect::<BTreeSet<_>>(),
        expected_ids(),
        "{when}: the archive must hold exactly the four native sessions, each once"
    );
    // The collector archives a .jsonl.zst rollout decoded, so its body is the
    // plain rollout bytes, not the compressed file.
    for (id, bytes) in [
        (codex_id(CODEX_ACTIVE_STEM), &freeze.codex_active),
        (codex_id(CODEX_ARCHIVED_STEM), &freeze.codex_archived_plain),
        (grok_id(GROK_NATIVE_ONLY), &freeze.grok_native_history),
    ] {
        let row = &rows[&id];
        assert_eq!(row.bytes, bytes.len() as u64, "{when}: {id} byte count");
        assert_eq!(
            row.sha256,
            sha256_hex(bytes),
            "{when}: {id} is not the source bytes"
        );
    }
    // A Grok session with both observations is ONE body that keeps the native
    // transcript byte for byte next to the legacy row; it is larger than the
    // native file alone and is never a second session.
    let both = &rows[&grok_id(GROK_BOTH)];
    assert!(
        both.bytes > freeze.grok_both_history.len() as u64,
        "{when}: the grok body must carry the native transcript and the legacy row"
    );
}

#[test]
fn codex_and_grok_survive_source_and_stage_removal_without_duplicates() {
    let freeze = Freeze::new();
    let archive = Archive::new();

    // ---- 1. native input -> collect + push -----------------------------
    let first = World::new();
    freeze.plant(&first.home());
    let pushed = text(&run_once(&first, &archive));
    assert!(
        pushed.contains("snapshot=created"),
        "the first run-once must create a snapshot:\n{pushed}"
    );

    // ---- 2. read ---------------------------------------------------------
    let rows = read_all(&first, &archive);
    assert_bodies_match_the_freeze(&rows, &freeze, "first read");

    // Collecting the same sources again changes nothing.
    let objects_before = archive.objects();
    let noop = text(&run_once(&first, &archive));
    assert!(
        noop.contains("snapshot=not-created"),
        "an unchanged second run-once must be a no-op:\n{noop}"
    );
    assert_eq!(
        archive.objects(),
        objects_before,
        "a no-op run wrote objects"
    );
    assert_eq!(read_all(&first, &archive), rows);

    // ---- 3. search -------------------------------------------------------
    assert_search_unknown_until_indexed(&first, &archive);
    build_index(&first, &archive);
    for (token, expected) in [
        (TOKEN_CODEX_ACTIVE, codex_id(CODEX_ACTIVE_STEM)),
        (TOKEN_CODEX_ARCHIVED, codex_id(CODEX_ARCHIVED_STEM)),
        (TOKEN_GROK_BOTH, grok_id(GROK_BOTH)),
        (TOKEN_GROK_NATIVE, grok_id(GROK_NATIVE_ONLY)),
    ] {
        let (matched, _plain) = search(&first, &archive, token);
        assert_eq!(
            matched, 1,
            "search for {token} must find exactly {expected}"
        );
    }

    // ---- 4. export -------------------------------------------------------
    let out_first = first.path().join("export-first");
    let exported = export_all(&first, &archive, &out_first);
    assert_eq!(
        exported.status.code(),
        Some(0),
        "export failed:\n{}",
        text(&exported)
    );
    let exported_text: String = files_under(&out_first)
        .iter()
        .map(|p| fs::read_to_string(p).unwrap_or_default())
        .collect();
    for token in [
        TOKEN_CODEX_ACTIVE,
        TOKEN_CODEX_ARCHIVED,
        TOKEN_GROK_BOTH,
        TOKEN_GROK_NATIVE,
    ] {
        assert!(
            exported_text.contains(token),
            "export lacks the text for {token}"
        );
    }

    // ---- 5. Paseo: agents counted apart from distinct native bodies ----------
    // Resolve through the real D3 Codex resolver while the source still exists.
    let codex_home = first.home().join(".codex");
    let mut observations = Vec::new();
    for (agent, uuid, stem) in [
        (
            "11111111-2222-4333-8444-555555555551",
            CODEX_ACTIVE_UUID,
            CODEX_ACTIVE_STEM,
        ),
        // A second agent pointing at the SAME native session.
        (
            "11111111-2222-4333-8444-555555555552",
            CODEX_ACTIVE_UUID,
            CODEX_ACTIVE_STEM,
        ),
        (
            "11111111-2222-4333-8444-555555555553",
            CODEX_ARCHIVED_UUID,
            CODEX_ARCHIVED_STEM,
        ),
    ] {
        let rollout = match resolve_rollout(&codex_home, uuid).unwrap() {
            CodexResolution::Resolved(rollout) => rollout,
            other => panic!("expected the freeze's rollout to resolve, got {other:?}"),
        };
        assert_eq!(rollout.stem, stem);
        observations.push(archived_observation(
            agent,
            "codex",
            uuid,
            &codex_id(&rollout.stem),
            &rows,
        ));
    }
    // Grok has no Paseo resolver yet (ADR-056 D3 leaves its identity evidence
    // open); the observation records the handle and the archive's own facts.
    observations.push(archived_observation(
        "11111111-2222-4333-8444-555555555554",
        "grok",
        GROK_NATIVE_ONLY,
        &grok_id(GROK_NATIVE_ONLY),
        &rows,
    ));
    let lines: String = observations
        .iter()
        .map(|o| {
            AgentObservationRow::new(o.clone(), "2026-11-01T00:00:00Z")
                .to_line()
                .unwrap()
        })
        .map(|line| format!("{line}\n"))
        .collect();
    let parsed = parse_rows(&lines).unwrap();
    let agents: BTreeSet<_> = parsed.iter().map(|r| r.agent_id.clone()).collect();
    let bodies: BTreeSet<_> = parsed
        .iter()
        .filter_map(|r| match &r.body {
            BodyState::Archived {
                canonical_session_id,
                ..
            } => Some(canonical_session_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(agents.len(), 4, "four agents");
    assert_eq!(
        bodies.len(),
        3,
        "three distinct native bodies: agents are not sessions"
    );
    assert!(bodies.is_subset(&expected_ids()));

    // ---- 6. archive-only recovery ---------------------------------------
    // Sources, stage, collector state and every local cache go; only the
    // archive and its key remain. A different machine world reads it.
    let first_dir = first.path().to_path_buf();
    drop(first);
    assert!(!first_dir.exists(), "the first world must be gone");

    let recovery = World::new();
    assert!(!recovery.home().join(".codex").exists());
    assert!(!recovery.home().join(".grok").exists());
    assert!(!recovery.stage().exists());
    let recovered = read_all(&recovery, &archive);
    assert_eq!(
        recovered, rows,
        "archive-only read differs from the pre-removal read"
    );
    assert_bodies_match_the_freeze(&recovered, &freeze, "archive-only read");
    // The Codex handle can no longer be located (the source is gone), and that
    // is Missing, not a different identity; the archived body still reads.
    assert_eq!(
        resolve_rollout(&recovery.home().join(".codex"), CODEX_ACTIVE_UUID).unwrap(),
        CodexResolution::Missing
    );
    assert_search_unknown_until_indexed(&recovery, &archive);
    build_index(&recovery, &archive);
    for (token, id) in [
        (TOKEN_CODEX_ACTIVE, codex_id(CODEX_ACTIVE_STEM)),
        (TOKEN_GROK_NATIVE, grok_id(GROK_NATIVE_ONLY)),
    ] {
        let (matched, _plain) = search(&recovery, &archive, token);
        assert_eq!(
            matched, 1,
            "archive-only search for {token} must still find {id}"
        );
    }
    let out_recovered = recovery.path().join("export-recovered");
    let exported = export_all(&recovery, &archive, &out_recovered);
    assert_eq!(
        exported.status.code(),
        Some(0),
        "archive-only export:\n{}",
        text(&exported)
    );
    let recovered_text: String = files_under(&out_recovered)
        .iter()
        .map(|p| fs::read_to_string(p).unwrap_or_default())
        .collect();
    assert!(
        recovered_text.contains(TOKEN_CODEX_ARCHIVED),
        "archive-only export lost a body"
    );

    // Running the collector on a machine whose sources are gone must not
    // remove or alter anything archived (append-only).
    let objects_gone = archive.objects();
    let empty = recovery
        .cli()
        .args(["run-once", "--stage"])
        .arg(recovery.stage())
        .args(["--machine", MACHINE, "--keep-ssh-masters"])
        .args(archive.args())
        .output()
        .unwrap();
    println!("run-once with no sources: exit={:?}", empty.status.code());
    assert!(
        archive.objects().is_superset(&objects_gone),
        "a collect over vanished sources removed archive objects"
    );
    assert_eq!(read_all(&recovery, &archive), rows);

    // ---- 7. re-run: the same native files reappear on a fresh stage ----------
    freeze.plant(&recovery.home());
    run_once(&recovery, &archive);
    let after = read_all(&recovery, &archive);
    assert_eq!(
        after.keys().collect::<BTreeSet<_>>(),
        rows.keys().collect::<BTreeSet<_>>(),
        "re-collecting minted a session the archive did not hold before"
    );
    // The sessions whose body comes from ONE source file replay whole and the
    // reader collapses the replay; the two-source Grok session re-seals only
    // its history shard (the row's shard is held), so the re-sealed shard is a
    // replay of the sequence's tail and collapses the same way (W1065). Same
    // bytes, same digest, for every session. (`shards=` counts raw shard
    // entries across snapshots and is allowed to grow; the body is what must
    // not.)
    for id in expected_ids() {
        assert_eq!(
            after[&id].bytes, rows[&id].bytes,
            "{id}: body changed on re-collect"
        );
        assert_eq!(
            after[&id].sha256, rows[&id].sha256,
            "{id}: body changed on re-collect"
        );
    }
    assert_bodies_match_the_freeze(&after, &freeze, "post-re-collect read");
}

/// W1065 acceptance. After the stage and the collector state are lost,
/// re-collecting UNCHANGED sources replays only part of a Grok session that
/// has both a `session_docs` row and a `chat_history.jsonl`: the row's shard
/// is recognised as held, the history's is sealed again. A re-sealed shard
/// that matches the tail of the session's preceding shard sequence is a
/// partial replay and the reader must eat it, so the body after the
/// re-collect equals the body before it — including the digest of the joined
/// shards. Duplicates a pre-W1065 build already archived read the same way,
/// because this is a read decision; nothing is re-collected or deleted.
#[test]
fn recollect_after_total_local_loss_does_not_duplicate_a_two_source_grok_session() {
    let freeze = Freeze::new();
    let archive = Archive::new();
    let first = World::new();
    freeze.plant(&first.home());
    run_once(&first, &archive);
    let before = read_all(&first, &archive);
    drop(first);

    let again = World::new();
    freeze.plant(&again.home());
    run_once(&again, &archive);
    let after = read_all(&again, &archive);
    let id = grok_id(GROK_BOTH);
    assert_eq!(
        (after[&id].bytes, &after[&id].sha256),
        (before[&id].bytes, &before[&id].sha256),
        "the two-source grok body changed on a re-collect of unchanged sources"
    );
}

/// The W1065 negative: genuine new content must not be swallowed. The machine
/// loses its stage and state; while it is lost the session's history GROWS;
/// the re-collect re-seals that grown history whole. Its bytes differ from the
/// tail shard the archive already holds, so it is new content, not a replay —
/// the read body must carry it. A collapse rule that compared prefixes or
/// concatenated byte ranges would swallow exactly this record; the tail rule
/// keeps it.
#[test]
fn recollect_after_total_local_loss_keeps_genuinely_new_history_content() {
    const TOKEN_GROK_NEW: &str = "qzgroknewcontentepsilon";
    let freeze = Freeze::new();
    let mut grown = Freeze::new();
    grown.grok_both_history = {
        let mut bytes = freeze.grok_both_history.clone();
        bytes.extend_from_slice(
            format!(r#"{{"type":"user","content":[{{"type":"text","text":"synthetic turn added while lost {TOKEN_GROK_NEW}"}}]}}"#).as_bytes(),
        );
        bytes.push(b'\n');
        bytes
    };
    let id = grok_id(GROK_BOTH);

    let archive = Archive::new();
    let first = World::new();
    freeze.plant(&first.home());
    run_once(&first, &archive);
    let before = read_all(&first, &archive);
    drop(first);

    let again = World::new();
    grown.plant(&again.home());
    run_once(&again, &archive);
    let after = read_all(&again, &archive);
    assert!(
        after[&id].bytes > before[&id].bytes,
        "the grown history was swallowed: {} bytes before, {} after",
        before[&id].bytes,
        after[&id].bytes
    );
    assert_ne!(
        after[&id].sha256, before[&id].sha256,
        "new content changed no bytes"
    );

    // What read kept, the export must write: the new turn reaches the user.
    let out_grown = again.path().join("export-grown");
    let exported = export_all(&again, &archive, &out_grown);
    assert_eq!(
        exported.status.code(),
        Some(0),
        "export of the grown session failed:\n{}",
        text(&exported)
    );
    let exported_text: String = files_under(&out_grown)
        .iter()
        .map(|p| fs::read_to_string(p).unwrap_or_default())
        .collect();
    assert!(
        exported_text.contains(TOKEN_GROK_NEW),
        "the genuinely new turn is missing from the export"
    );
    assert!(
        exported_text.contains(TOKEN_GROK_BOTH),
        "the original turn must stay"
    );
}

/// W1065 rework acceptance (verdict H1): a *genuine consecutive repeat* —
/// a user turn whose bytes equal the turn before it, sealed by a validated
/// increment — is the `[A,B,B]` shape, and every reader must keep it: read,
/// export, the full-text index, the activity index and doctor's duplicate
/// scan alike. The repeated delta shards carry the NEXT sequence numbers
/// (`[S, d, d]` sealed as sequences 1, 2, 3), so no re-seal evidence exists
/// and a fold that compared content alone would swallow a real user turn.
///
/// One archiving world, three collects: the freeze whole, then the same
/// append consumed twice in two consecutive collect cycles — the collector's
/// validated-increment path (`base_offset > 0`, exact byte repeats allowed)
/// is the production shape this pins, not a hand-built stage.
#[test]
fn a_validated_consecutive_repeated_turn_survives_read_export_fts_activity_and_doctor() {
    const TOKEN: &str = "qzdupturnkappa";
    const REPEAT: &str = r#"{"timestamp":"2026-09-01T10:00:07.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"synthetic repeated turn qzdupturnkappa"}]}}"#;
    let freeze = Freeze::new();
    let archive = Archive::new();
    let world = World::new();
    freeze.plant(&world.home());
    run_once(&world, &archive);
    let before = read_all(&world, &archive);
    let id = codex_id(CODEX_ACTIVE_STEM);
    let active = world
        .home()
        .join(".codex")
        .join("sessions")
        .join("2026")
        .join("09")
        .join("01")
        .join(format!("{CODEX_ACTIVE_STEM}.jsonl"));

    let append = |turn: &str| {
        let mut bytes = fs::read(&active).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'), "the source stays line-complete");
        bytes.extend_from_slice(turn.as_bytes());
        bytes.push(b'\n');
        fs::write(&active, &bytes).unwrap();
    };
    append(REPEAT);
    run_once(&world, &archive);
    append(REPEAT);
    run_once(&world, &archive);
    let after = read_all(&world, &archive);

    // The whole source the archive must hold: the original rollout plus the
    // same delta twice.
    let mut expected_body = freeze.codex_active.clone();
    for _ in 0..2 {
        expected_body.extend_from_slice(REPEAT.as_bytes());
        expected_body.push(b'\n');
    }

    // ---- read ------------------------------------------------------------
    assert_eq!(
        after[&id].shards, 3,
        "two increment cycles after a whole seal are three shards"
    );
    assert_eq!(
        after[&id].bytes,
        expected_body.len() as u64,
        "read must keep both copies of the repeated turn — collapsing the \
         second copy on content alone eats a real user turn"
    );
    assert_eq!(
        after[&id].sha256,
        sha256_hex(&expected_body),
        "the body is the source: original plus the same turn twice"
    );
    assert!(
        after[&id].bytes > before[&id].bytes + REPEAT.len() as u64,
        "both appended turns are present in the read body"
    );

    // ---- export ----------------------------------------------------------
    let out = world.path().join("export-repeated");
    let exported = export_all(&world, &archive, &out);
    assert_eq!(
        exported.status.code(),
        Some(0),
        "export of the repeated-turn session failed:\n{}",
        text(&exported)
    );
    let exported_text: String = files_under(&out)
        .iter()
        .map(|p| fs::read_to_string(p).unwrap_or_default())
        .collect();
    assert_eq!(
        exported_text.matches(TOKEN).count(),
        2,
        "the exported file must carry the repeated turn exactly twice, once \
         per real occurrence"
    );

    // ---- activity index ---------------------------------------------------
    let activity_index = world
        .stage()
        .join("meta")
        .join(MACHINE)
        .join("activity-v1.jsonl");
    let mut activity_row: Option<serde_json::Value> = None;
    for line in fs::read_to_string(&activity_index)
        .expect("run-once builds the activity index")
        .lines()
    {
        let row: serde_json::Value = serde_json::from_str(line).expect("activity rows are JSONL");
        if row["session_id"].as_str() == Some(id.as_str()) {
            activity_row = Some(row);
        }
    }
    let row = activity_row.expect("the codex session has an activity row");
    let expected_lines = freeze.codex_active.iter().filter(|b| **b == b'\n').count() + 2;
    assert_eq!(
        row["line_count"].as_u64(),
        Some(expected_lines as u64),
        "the activity index must count both repeated turns"
    );

    // ---- full-text index --------------------------------------------------
    build_index(&world, &archive);
    let (matched, _plain) = search(&world, &archive, TOKEN);
    assert_eq!(matched, 1, "the session that repeats the turn matches");
    let index_path = files_under(world.path())
        .into_iter()
        .find(|p| p.file_name().map(|n| n == "index.sqlite3").unwrap_or(false))
        .expect("the world-local fts index file");
    let fts = rusqlite::Connection::open(&index_path)
        .expect("the fts index opens read-write in its own world");
    let body: String = fts
        .query_row(
            "SELECT body FROM documents WHERE id = ?1",
            [format!("{MACHINE}/{id}")],
            |row| row.get(0),
        )
        .expect("the codex document is indexed under the machine-partitioned id");
    assert_eq!(
        body.matches(TOKEN).count(),
        2,
        "the full-text document must carry the repeated turn twice — folding \
         the second copy removes a real turn from every search over it"
    );

    // ---- doctor -----------------------------------------------------------
    // Doctor counts the copies a Collapse read would fold, so the configured
    // stage of this world — which holds exactly the [S, d, d] sequence — must
    // report clean: the repeated turn is content, not a foldable duplicate.
    let config_dir = world.path().join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!("[native_host]\nstage = {:?}\n", world.stage()),
    )
    .unwrap();
    let doctor = world.cli().args(["doctor", "--json"]).output().unwrap();
    let report: serde_json::Value =
        serde_json::from_slice(&doctor.stdout).expect("doctor --json prints one object");
    let d11 = &report["stage_duplicate_shards"];
    assert_eq!(
        d11["checked"].as_bool(),
        Some(true),
        "D11 must have run against the configured stage: {d11}"
    );
    assert_eq!(
        d11["state"].as_str(),
        Some("clean"),
        "a genuine consecutive repeat must not read as foldable duplicates: {d11}"
    );
    for machine in d11["machines"]
        .as_array()
        .expect("D11 lists every stage machine")
    {
        assert_eq!(
            machine["duplicate_shards"].as_u64(),
            Some(0),
            "no stage machine may count the repeated turn as a foldable copy: {machine}"
        );
    }
}

fn archived_observation(
    agent_id: &str,
    provider: &str,
    native_handle: &str,
    canonical: &str,
    rows: &BTreeMap<String, ReadRow>,
) -> AgentObservation {
    let row = &rows[canonical];
    let observation = AgentObservation {
        daemon_key: "fixture-daemon".to_string(),
        agent_id: agent_id.to_string(),
        provider: provider.to_string(),
        native_session_id: Some(native_handle.to_string()),
        projection: AgentProjection::default(),
        body: BodyState::Archived {
            canonical_session_id: canonical.to_string(),
            shard_sequences: (1..=row.shards).collect(),
            body_sha256: row.sha256.clone(),
        },
    };
    observation.validate().unwrap();
    observation
}
