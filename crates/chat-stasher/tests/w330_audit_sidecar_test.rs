//! W330 · AUDIT-META ④: the per-message audit sidecar is a derived
//! projection of the archived bodies, and that is all it is ever allowed to
//! be. Properties, exercised through the interfaces the ①–③ landings and the
//! RI-3 wiring left in place:
//!
//! * **rebuildable, one partition** — delete the sidecar of a stage that
//!   holds one session, rescan the bodies (stage leg via the `audit-backfill`
//!   CLI, archive leg via `backfill_archive`) and the rebuilt bytes are
//!   byte-identical to what incremental collection wrote;
//! * **rebuildable, across partitions** — with several sessions collected in
//!   an interleaved temporal order, a rebuild walks session directories in
//!   name order and each session's shards in sealed-sequence order, so it
//!   holds the same rows and outcomes in that canonical order and two
//!   rebuilds are byte-identical. The interleaved order several collects
//!   appended in cannot be reconstructed from the bodies alone, so
//!   byte-identity to the collect-written file is claimed — and proven —
//!   only for the one-partition stage. A session directory that holds no
//!   shards contributes nothing, and a partition whose body bytes cannot be
//!   parsed is accounted as an incomplete extraction while the surviving
//!   partitions still rebuild;
//! * **fail-open read side / fail-safe accounting** — a torn sidecar never
//!   stops bodies from being sealed or read; the accounting never claims
//!   success over it (collect exits 1 with the body shards intact and the
//!   corrupt bytes preserved, `audit-backfill` refuses);
//! * **bounded** — per-record sidecar cost is near-fixed, so on multi-KB
//!   records the sidecar stays a small fraction of the body bytes it
//!   describes. docs-dev/message-audit.md pins the 15% practitioner ceiling
//!   this suite enforces: above it the sidecar is a second archive, not a
//!   joinable summary.
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

/// The sidecar-size ceiling as a percentage of the body bytes it describes.
/// docs-dev/message-audit.md states this number and why it is a practitioner
/// bound, not a law of nature: per-record sidecar cost is near-fixed, so on
/// multi-KB records the sidecar is a small fraction of the bodies; a sidecar
/// that outgrows the bound is acting as a second archive, which the design
/// forbids.
const SIDECAR_BODY_CEILING_PCT: usize = 15;

fn sidecar_path(stage: &Path, machine: &str) -> PathBuf {
    stage
        .join("meta")
        .join(machine)
        .join(message_audit::FILE_NAME)
}

/// One opaque Claude Code assistant record. The filler sizes each record the
/// way real harness records run — multiple KB per message — because the
/// sidecar's per-record cost is near-fixed: a row plus its body outcome costs
/// roughly the same bytes whatever the message says, so the sidecar's
/// fraction of the body bytes it describes is a property of record size.
/// docs-dev/message-audit.md states the 15% ceiling the bounded assertions
/// below enforce. The record itself stays opaque: no conversation text, real
/// id, or real path.
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
            "content": "synthetic".repeat(2000),
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

    /// Append one record to the named source session instead of the fixture's
    /// original one, so a stage can grow several collected partitions.
    fn append_source_line_in(&self, stem: &str, i: usize) {
        let path = self
            .source_session
            .parent()
            .unwrap()
            .join(format!("{stem}.jsonl"));
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
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

    /// The machine's collected session directory names, in name order.
    fn session_dir_names(&self, machine: &str) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.stage.join("sessions").join(machine))
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        names
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
    // The model label is a retained audit field; the raw event id and the
    // raw cwd are the values that must never enter, keyed as they are
    // (docs-dev/message-audit.md lists exactly what the sidecar carries).
    for raw in ["synthetic-event-", "/synthetic/private"] {
        assert!(!text.contains(raw), "raw {raw} leaked into the sidecar");
    }
    let body = store::concat_shards(&fx.stage, machine, &session).unwrap();
    assert_eq!(body.len(), 3 * assistant_line(0).len());
    // Bounded, as a ratio that can actually turn red: the sidecar may cost
    // at most `SIDECAR_BODY_CEILING_PCT` percent of the body bytes it
    // describes (docs-dev/message-audit.md states the ceiling). The second
    // assertion keeps the first one honest: this corpus must sit close
    // enough to the ceiling that a roughly-2x per-record sidecar cost
    // regression is red, so a fixture edit that quietly parks the ratio far
    // below the bound defangs the ceiling the first assertion means to hold.
    assert!(
        collect_written.len() * 100 <= body.len() * SIDECAR_BODY_CEILING_PCT,
        "sidecar {} bytes exceed the {}% ceiling over {} body bytes",
        collect_written.len(),
        SIDECAR_BODY_CEILING_PCT,
        body.len()
    );
    assert!(
        collect_written.len() * 2 * 100 > body.len() * SIDECAR_BODY_CEILING_PCT,
        "corpus sits too far under the {}% ceiling ({} of {} bytes): the bounded assertion above can no longer turn red on a ~2x sidecar cost regression",
        SIDECAR_BODY_CEILING_PCT,
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

/// AUDIT-META ④ across partitions. Collect appends rows in the temporal
/// order of its passes, which interleaves sessions a rebuild cannot see: the
/// bodies carry per-session sequence numbers, never cross-session timestamps
/// of collection, so a rebuild can be deterministic (canonically ordered,
/// byte-stable between rebuilds) but can never reproduce that interleave.
/// This test pins exactly what is true for a multi-session stage — same rows,
/// same outcomes, canonical order, byte-identical rebuilds — and proves the
/// fixture really interleaves, so the order the rebuild cannot reproduce is
/// present to be not-reproduced. The single-partition test above is the one
/// that proves byte-identity to the collect-written file. The same stage is
/// then grown by two partitions the collectors did not write — an empty
/// session directory and a directory holding one unparseable shard — to pin
/// that neither blocks the surviving partitions' rebuild: the first
/// contributes nothing, the second is accounted as an incomplete extraction
/// with the scan still completing.
#[test]
fn stage_rebuild_across_partitions_is_set_equal_canonical_and_fails_open() {
    let fx = collect_fixture();
    let machine = "synthetic-machine";
    let stems = [
        "0189a100-0000-7000-8000-000000000001",
        "0189b200-0000-7000-8000-000000000002",
        "0189c300-0000-7000-8000-000000000003",
    ];
    // Interleaved passes, in a temporal order that is deliberately not the
    // sessions' directory-name order: pass 1 collects stem 2, pass 2 stem 0,
    // pass 3 stem 1, pass 4 stem 0 again — so stem 0 is collected in two
    // passes and owns a two-shard partition, the way a real resumed session
    // does.
    let passes: [usize; 4] = [2, 0, 1, 0];
    for (round, stem) in passes.iter().enumerate() {
        fx.append_source_line_in(stems[*stem], round);
        let output = fx.collect();
        assert!(
            output.status.success(),
            "collect pass {round} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let names = fx.session_dir_names(machine);
    assert_eq!(names.len(), 3, "three collected partitions must exist");
    for (name, stem) in names.iter().zip(stems.iter()) {
        assert!(
            name.ends_with(stem),
            "session directory {name} must derive from source {stem}"
        );
    }
    let sidecar = sidecar_path(&fx.stage, machine);
    let collect_written = fs::read(&sidecar).unwrap();
    let before = message_audit::decode_jsonl(&collect_written).unwrap();
    assert_eq!(before.rows.len(), 4);
    assert_eq!(before.outcomes.len(), 4);
    // What incremental collection wrote, in append order: one row plus one
    // outcome per pass, across partitions, in the passes' temporal order.
    let temporal: Vec<&str> = passes.iter().map(|s| names[*s].as_str()).collect();
    assert_eq!(
        before
            .rows
            .iter()
            .map(|row| row.session.as_str())
            .collect::<Vec<_>>(),
        temporal,
        "collect must append across partitions in pass order"
    );
    assert_eq!(
        before
            .outcomes
            .iter()
            .map(|o| o.session.as_str())
            .collect::<Vec<_>>(),
        temporal
    );
    // Delete and rebuild: canonical order is session-directory name, then
    // sealed sequence within a session, which for these passes is [0, 0, 1, 2].
    fs::remove_file(&sidecar).unwrap();
    let rebuild = fx.audit_backfill_stage_only();
    assert!(
        rebuild.status.success(),
        "multi-partition rebuild failed: {}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
    let stdout = String::from_utf8_lossy(&rebuild.stdout).into_owned();
    assert!(
        stdout.contains("recognized=4") && stdout.contains("incomplete_extractions=0"),
        "unexpected rebuild report: {stdout}"
    );
    let rebuilt = fs::read(&sidecar).unwrap();
    let after = message_audit::decode_jsonl(&rebuilt).unwrap();
    // Set equality: the rebuild holds the same rows and the same outcomes as
    // the collect-written sidecar, byte for byte per entry.
    let rows_by_id = |p: &message_audit::Projection| {
        let mut ids: Vec<String> = p
            .rows
            .iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect();
        ids.sort();
        ids
    };
    let outcomes_by_id = |p: &message_audit::Projection| {
        let mut ids: Vec<String> = p
            .outcomes
            .iter()
            .map(|o| serde_json::to_string(o).unwrap())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(
        rows_by_id(&before),
        rows_by_id(&after),
        "rebuild must hold the same rows as incremental collection"
    );
    assert_eq!(
        outcomes_by_id(&before),
        outcomes_by_id(&after),
        "rebuild must hold the same body outcomes as incremental collection"
    );
    // Canonical order: partitions in name order, and each partition's rows in
    // the order its shards were sealed (per-session temporal order).
    let canonical: Vec<&str> = [0usize, 0, 1, 2]
        .iter()
        .map(|s| names[*s].as_str())
        .collect();
    assert_eq!(
        after
            .rows
            .iter()
            .map(|row| row.session.as_str())
            .collect::<Vec<_>>(),
        canonical,
        "rebuild must walk partitions in name order"
    );
    assert_eq!(
        after
            .outcomes
            .iter()
            .map(|o| o.session.as_str())
            .collect::<Vec<_>>(),
        canonical
    );
    // The full expected sequence, via an independent oracle: stable-sort the
    // collect-written rows by session name — per partition, collect order is
    // sealed order — and the rebuild must match it, row for row and outcome
    // for outcome, not just session by session.
    let mut expected_rows: Vec<&message_audit::AuditRow> = before.rows.iter().collect();
    expected_rows.sort_by(|a, b| a.session.cmp(&b.session));
    assert_eq!(
        expected_rows
            .iter()
            .map(|row| serde_json::to_string(*row).unwrap())
            .collect::<Vec<_>>(),
        after
            .rows
            .iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect::<Vec<_>>(),
        "each partition's rows must rebuild in sealed sequence order"
    );
    let mut expected_outcomes: Vec<&message_audit::ExtractionOutcome> =
        before.outcomes.iter().collect();
    expected_outcomes.sort_by(|a, b| a.session.cmp(&b.session));
    assert_eq!(
        expected_outcomes
            .iter()
            .map(|o| serde_json::to_string(*o).unwrap())
            .collect::<Vec<_>>(),
        after
            .outcomes
            .iter()
            .map(|o| serde_json::to_string(o).unwrap())
            .collect::<Vec<_>>(),
        "each partition's outcomes must rebuild in sealed sequence order"
    );
    // The two orders genuinely differ on this fixture — if they ever stop
    // differing, the claims above no longer cover the interleaved case and
    // the fixture must interleave again.
    assert_ne!(
        before.rows, after.rows,
        "fixture must keep the temporal order a rebuild cannot reproduce; strengthen the interleave if this fires"
    );
    // A rerun over the live sidecar appends nothing (all identities already
    // present), and a second delete-and-rebuild is byte-identical: the
    // canonical walk makes rebuilds reproducible, which directory-listing
    // order alone never did.
    let rerun = fx.audit_backfill_stage_only();
    assert!(
        rerun.status.success(),
        "rerun over the rebuilt sidecar failed: {}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(
        fs::read(&sidecar).unwrap(),
        rebuilt,
        "rerun must append nothing"
    );
    fs::remove_file(&sidecar).unwrap();
    let second = fx.audit_backfill_stage_only();
    assert!(second.status.success());
    assert_eq!(
        fs::read(&sidecar).unwrap(),
        rebuilt,
        "two rebuilds must be byte-identical"
    );
    // An empty session partition contributes no entries: the rebuild keeps
    // producing the same bytes with it present.
    let prefix = names[0]
        .rsplit_once('.')
        .expect("session directory name has a native id after the last dot")
        .0
        .to_string();
    let empty_name = format!("{prefix}.0189d400-0000-7000-8000-000000000004");
    fs::create_dir_all(fx.stage.join("sessions").join(machine).join(&empty_name)).unwrap();
    fs::remove_file(&sidecar).unwrap();
    let rebuild = fx.audit_backfill_stage_only();
    assert!(
        rebuild.status.success(),
        "an empty session partition must not fail the rebuild: {}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
    assert_eq!(
        fs::read(&sidecar).unwrap(),
        rebuilt,
        "an empty session partition must contribute no sidecar entries"
    );
    // A partition whose body bytes cannot be parsed is an incomplete
    // extraction, never a blocker for the surviving partitions: the scan
    // completes (exit 0), the good entries rebuild byte-identically, the bad
    // partition's failed outcome is appended last (its directory name sorts
    // after the good partitions), and the accounting still counts it.
    let bad_name = format!("{prefix}.0189e500-0000-7000-8000-000000000005");
    let bad_dir = fx.stage.join("sessions").join(machine).join(&bad_name);
    fs::create_dir_all(&bad_dir).unwrap();
    fs::write(
        bad_dir.join(store::shard_filename(1)),
        b"not jsonl at all\n",
    )
    .unwrap();
    fs::remove_file(&sidecar).unwrap();
    let rebuild = fx.audit_backfill_stage_only();
    assert!(
        rebuild.status.success(),
        "an unparseable foreign partition must fail open, not refuse the machine's rebuild: {}",
        String::from_utf8_lossy(&rebuild.stderr)
    );
    let stdout = String::from_utf8_lossy(&rebuild.stdout).into_owned();
    assert!(
        stdout.contains("recognized=4") && stdout.contains("incomplete_extractions=1"),
        "the bad partition must be accounted, not swallowed: {stdout}"
    );
    let after_bad_bytes = fs::read(&sidecar).unwrap();
    assert!(
        after_bad_bytes.starts_with(rebuilt.as_slice()),
        "the surviving partitions' entries must rebuild byte-identically around the bad partition"
    );
    let after_bad = message_audit::decode_jsonl(&after_bad_bytes).unwrap();
    assert_eq!(
        after_bad.rows.len(),
        4,
        "a failed extraction adds rows for nobody"
    );
    assert_eq!(
        after_bad.outcomes.len(),
        5,
        "the bad partition's outcome must be present"
    );
    let failed = after_bad.outcomes.last().unwrap();
    assert_eq!(failed.session, bad_name);
    assert_eq!(failed.status, message_audit::ExtractionStatus::Failed);
    assert_eq!(failed.recognized, 0);
    assert_eq!(failed.malformed, 1);
    // Fail-open read side: the good partition's bodies are still readable
    // through the canonical stage reader while the bad partition sits in the
    // same machine's tree.
    assert_eq!(
        store::concat_shards(&fx.stage, machine, &names[0]).unwrap(),
        [assistant_line(1), assistant_line(3)].concat(),
        "the two-shard partition must still read exactly its sealed bytes"
    );
    // Normal operation resumes over the accounted partition: an unchanged
    // collect pass neither fails nor rewrites the sidecar.
    let settled = fx.collect();
    assert!(
        settled.status.success(),
        "collect must not fail over an accounted bad partition: {}",
        String::from_utf8_lossy(&settled.stderr)
    );
    assert_eq!(
        fs::read(&sidecar).unwrap(),
        after_bad_bytes,
        "an unchanged pass must not touch the sidecar"
    );
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
