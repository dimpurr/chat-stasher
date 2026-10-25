//! W330 · AUDIT-META ④: the per-message audit sidecar is a derived
//! projection of the archived bodies, and that is all it is ever allowed to
//! be. Three properties, exercised through the interfaces the ①–③ landings
//! and the RI-3 wiring left in place:
//!
//! * **rebuildable** — delete the sidecar, rescan the bodies (stage leg via
//!   the `audit-backfill` CLI, archive leg via `backfill_archive`) and the
//!   rebuilt bytes are byte-identical to what incremental collection wrote;
//! * **fail-open read side / fail-safe accounting** — a torn sidecar never
//!   stops bodies from being sealed or read; the accounting never claims
//!   success over it (collect exits 1 with the body shards intact and the
//!   corrupt bytes preserved, `audit-backfill` refuses);
//! * **bounded** — the sidecar stays smaller than the body bytes it
//!   describes, so it is a joinable summary, never a second archive.
//!
//! The join key is a synthetic fixture value inside the sandbox, mirroring
//! the W711 corpus: the product never creates one, it is only ever read.
use chat_stasher::{audit_store, message_audit, store};
use serde_json::json;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::{collections::BTreeMap, fs};

#[path = "../src/test_support.rs"]
mod test_support;

fn policy() -> message_audit::JoinPolicy {
    message_audit::JoinPolicy::new([7; 32])
}

fn sidecar_path(stage: &Path, machine: &str) -> PathBuf {
    stage
        .join("meta")
        .join(machine)
        .join(message_audit::FILE_NAME)
}

/// One opaque Claude Code assistant record. The filler keeps the fixture
/// honest about the size relation between a raw record and its audit row
/// (`38-AUDIT-METADATA.md` measured ~7% of raw bytes on real mixed bodies;
/// real records run in the multiple-KB range these synthetic ones imitate)
/// without containing or revealing any conversation text.
fn assistant_line(i: usize) -> Vec<u8> {
    let record = json!({
        "type": "assistant",
        "timestamp": "2026-10-25T09:00:00Z",
        "cwd": "/synthetic/private",
        "isSidechain": false,
        "parentUuid": "synthetic-parent",
        "isApiErrorMessage": false,
        "message": {
            "id": format!("synthetic-event-{i}"),
            "model": "synthetic-model",
            "usage": {"input_tokens": i + 1, "output_tokens": 3, "cache_read_input_tokens": 4},
            "content": "synthetic".repeat(480),
        },
    });
    let mut bytes = serde_json::to_vec(&record).unwrap();
    bytes.push(b'\n');
    bytes
}

/// The claude-code cell, registry and audit key, in the W711 collect-test
/// shape. Collect runs through the CLI so the join key travels through the
/// same environment variable the product reads, and every collect pass a
/// user would run is a pass this suite actually takes.
struct CollectFixture {
    stage: PathBuf,
    registry: PathBuf,
    key: PathBuf,
    source_session: PathBuf,
    sandbox: test_support::Sandbox,
}

fn collect_fixture() -> CollectFixture {
    let sandbox = test_support::Sandbox::new();
    sandbox.ensure_dirs();
    let root = sandbox.root();
    let source = root.join("sources");
    fs::create_dir_all(&source).unwrap();
    let source_session = source.join("019bf00d-97b6-7eb2-9bf8-eacbacc09765.jsonl");
    let cell = json!({"template": source, "format": "jsonl", "session_pattern": "*.jsonl", "confidence": "measured-locally", "source": "synthetic fixture"});
    let registry = root.join("registry.json");
    fs::write(
        &registry,
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "generated": "synthetic",
            "harnesses": [
                {"id": "claude-code", "display_name": "Claude Code",
                 "paths": {"macos": cell, "linux": cell, "windows": cell}}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let key = root.join("audit-key");
    fs::write(&key, [7; 32]).unwrap();
    let stage = root.join("stage");
    CollectFixture {
        stage,
        registry,
        key,
        source_session,
        sandbox,
    }
}

impl CollectFixture {
    fn append_source_line(&self, i: usize) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.source_session)
            .unwrap();
        file.write_all(&assistant_line(i)).unwrap();
    }

    fn collect(&self) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        let cmd = self.sandbox.apply(&mut cmd);
        cmd.env("CHAT_STASHER_REGISTRY", &self.registry)
            .env(audit_store::KEY_FILE_ENV, &self.key)
            .args(["collect", "--machine", "synthetic-machine", "--stage"])
            .arg(&self.stage)
            .arg("--repo")
            .arg(self.sandbox.root().join("repo"))
            .output()
            .unwrap()
    }

    fn audit_backfill_stage_only(&self) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        let cmd = self.sandbox.apply(&mut cmd);
        cmd.args([
            "audit-backfill",
            "--machine",
            "synthetic-machine",
            "--stage-only",
            "--stage",
        ])
        .arg(&self.stage)
        .arg("--join-key-file")
        .arg(&self.key)
        .output()
        .unwrap()
    }

    /// The one archived session id, discovered from the stage so the test
    /// does not restate the id-format derivation.
    fn sole_session_id(&self, machine: &str) -> String {
        let mut names: Vec<String> = fs::read_dir(self.stage.join("sessions").join(machine))
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        assert_eq!(names.len(), 1, "one synthetic session is collected");
        names.pop().unwrap()
    }
}

/// Rediscover the machine's sealed shards in sequence order.
fn shards_by_seq(fx: &CollectFixture, session: &str) -> Vec<(u64, PathBuf)> {
    let mut shards = store::sealed_shard_entries(&store::session_shard_dir(
        &fx.stage,
        "synthetic-machine",
        session,
    ))
    .unwrap();
    shards.sort_by_key(|(seq, _)| *seq);
    shards
}

#[test]
fn rebuilt_stage_sidecar_matches_collect_written_bytes_after_deletion() {
    let fx = collect_fixture();
    let machine = "synthetic-machine";
    for i in 0..3 {
        fx.append_source_line(i);
        let output = fx.collect();
        assert!(
            output.status.success(),
            "collect pass {i} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let session = fx.sole_session_id(machine);
    let sidecar = sidecar_path(&fx.stage, machine);
    let collect_written = fs::read(&sidecar).unwrap();
    let projection = message_audit::decode_jsonl(&collect_written).unwrap();
    assert_eq!(projection.rows.len(), 3);
    assert_eq!(projection.outcomes.len(), 3);
    let text = String::from_utf8(collect_written.clone()).unwrap();
    // The model label is a retained audit field (38-AUDIT-METADATA §2); the
    // raw event id and the raw cwd are the ones that must never enter, keyed
    // as they are.
    for raw in ["synthetic-event-", "/synthetic/private"] {
        assert!(!text.contains(raw), "raw {raw} leaked into the sidecar");
    }
    let body = store::concat_shards(&fx.stage, machine, &session).unwrap();
    assert_eq!(body.len(), 3 * assistant_line(0).len());
    assert!(
        collect_written.len() < body.len(),
        "sidecar {} bytes must stay a summary of the {} body bytes",
        collect_written.len(),
        body.len()
    );
    // Delete the sidecar and rebuild from the bodies alone, twice: the
    // rebuilt bytes must be identical to the bytes incremental collection
    // wrote, and identical between rebuilds.
    for _ in 0..2 {
        fs::remove_file(&sidecar).unwrap();
        let rebuild = fx.audit_backfill_stage_only();
        assert!(
            rebuild.status.success(),
            "stage-only rebuild failed: {}",
            String::from_utf8_lossy(&rebuild.stderr)
        );
        let stdout = String::from_utf8(rebuild.stdout).unwrap();
        assert!(stdout.contains("recognized=3"), "stdout: {stdout}");
        let rebuilt = fs::read(&sidecar).unwrap();
        assert!(
            rebuilt == collect_written,
            "rebuild must reproduce the collect-written sidecar byte for byte ({} vs {} bytes)",
            rebuilt.len(),
            collect_written.len()
        );
    }
}

#[test]
fn torn_sidecar_keeps_bodies_sealed_and_read_but_never_claims_success() {
    let fx = collect_fixture();
    let machine = "synthetic-machine";
    fx.append_source_line(0);
    let clean = fx.collect();
    assert!(clean.status.success());
    let session = fx.sole_session_id(machine);
    let sidecar = sidecar_path(&fx.stage, machine);
    let pass_one = fs::read(&sidecar).unwrap();
    assert_eq!(shards_by_seq(&fx, &session).len(), 1);
    // Fail-open read side: the body stays readable through the canonical
    // stage reader while the sidecar holds unparseable bytes.
    let torn = b"{\"torn\": tru";
    fs::write(&sidecar, torn).unwrap();
    assert!(message_audit::decode_jsonl(torn).is_err());
    let expected_bodies = [assistant_line(0), assistant_line(1)].concat();
    assert_eq!(
        store::concat_shards(&fx.stage, machine, &session).unwrap(),
        assistant_line(0)
    );
    // Fail-safe accounting: the pass that follows seals the new body bytes
    // but reports failure instead of success, and preserves the torn tail.
    fx.append_source_line(1);
    let dirty = fx.collect();
    assert_eq!(
        dirty.status.code(),
        Some(1),
        "collect must not claim success over a torn sidecar: {}",
        String::from_utf8_lossy(&dirty.stderr)
    );
    assert_eq!(
        shards_by_seq(&fx, &session).len(),
        2,
        "body must not be blocked"
    );
    assert_eq!(fs::read(&sidecar).unwrap(), torn.to_vec());
    assert_eq!(
        store::concat_shards(&fx.stage, machine, &session).unwrap(),
        expected_bodies
    );
    let mut migration = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    let migration = fx.sandbox.apply(&mut migration);
    let refused = migration
        .args([
            "audit-backfill",
            "--machine",
            "synthetic-machine",
            "--stage-only",
            "--stage",
        ])
        .arg(&fx.stage)
        .arg("--join-key-file")
        .arg(&fx.key)
        .output()
        .unwrap();
    assert_eq!(
        refused.status.code(),
        Some(3),
        "audit-backfill must refuse over a torn sidecar, not rebuild or rewrite it"
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("coverage unknown"));
    assert_eq!(fs::read(&sidecar).unwrap(), torn.to_vec());
    // The documented repair for derived metadata is deletion + rebuild. The
    // rebuilt bytes must continue the collect-written prefix exactly: what
    // pass two would have appended over the clean sidecar, the rebuild now
    // writes after re-deriving pass one.
    fs::remove_file(&sidecar).unwrap();
    let repair = fx.audit_backfill_stage_only();
    assert!(
        repair.status.success(),
        "repair rebuild failed: {}",
        String::from_utf8_lossy(&repair.stderr)
    );
    let mut expected = pass_one;
    for (_, path) in shards_by_seq(&fx, &session).iter().skip(1) {
        let raw = fs::read(path).unwrap();
        for p in audit_store::project_shard(&raw, &session, &policy()).unwrap() {
            expected = message_audit::append_jsonl(&expected, &p).unwrap();
        }
    }
    assert!(
        fs::read(&sidecar).unwrap() == expected,
        "repair rebuild must continue the collect-written prefix byte for byte ({} bytes)",
        expected.len()
    );
    // Normal operation resumes: an unchanged pass neither adds rows nor
    // fails.
    let settled = fx.collect();
    assert!(settled.status.success());
    assert_eq!(fs::read(&sidecar).unwrap(), expected);
    assert_eq!(shards_by_seq(&fx, &session).len(), 2);
}

#[test]
fn archive_backfills_collect_bodies_into_byte_identical_sidecar() {
    let fx = collect_fixture();
    let machine = "synthetic-machine";
    for i in 0..2 {
        fx.append_source_line(i);
        let output = fx.collect();
        assert!(output.status.success());
    }
    let session = fx.sole_session_id(machine);
    let stage = &fx.stage;
    let sidecar = sidecar_path(stage, machine);
    let collect_written = fs::read(&sidecar).unwrap();
    let bodies = store::concat_shards(stage, machine, &session).unwrap();
    let cfg = store::StoreConfig {
        repo_root: fx
            .sandbox
            .root()
            .join("repo")
            .to_string_lossy()
            .into_owned(),
        key_file: fx.sandbox.root().join("masterkey"),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(fx.sandbox.rustic_cache_dir()),
        no_cache: false,
    };
    let masterkey = rustic_core::repofile::MasterKey::new();
    let archive = store::BackupStore::new(cfg, machine.to_string());
    drop(archive.open_or_init(&masterkey).unwrap());
    archive.push(stage, &masterkey).unwrap();
    // Machine-loss posture: the stage holds neither bodies nor sidecar.
    fs::remove_dir_all(stage.join("sessions")).unwrap();
    fs::remove_file(&sidecar).unwrap();
    let restored = archive
        .read_session_concat(machine, &session, &masterkey)
        .unwrap()
        .0;
    assert_eq!(restored, bodies, "bodies must not depend on the sidecar");
    audit_store::backfill_archive(&archive, &masterkey, stage, machine, &policy()).unwrap();
    assert!(
        fs::read(&sidecar).unwrap() == collect_written,
        "archive rebuild must reproduce the collect-written sidecar byte for byte ({} bytes)",
        collect_written.len()
    );
    assert!(!stage.join("sessions").exists());
    audit_store::backfill_archive(&archive, &masterkey, stage, machine, &policy()).unwrap();
    assert_eq!(fs::read(&sidecar).unwrap(), collect_written);
}
