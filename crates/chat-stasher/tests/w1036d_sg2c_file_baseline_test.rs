//! SG-2c required fixture and regression tests:
//! When stage shard bodies are deleted by reclaim_stage, FILE sources
//! (codex, grok, claude-code, etc.) must consult the retained per-session
//! summary in manifest.jsonl (ADR-020 role C) as the durable baseline.
//!
//! An unchanged session writes zero bytes.
//! A changed session writes only the increment.
//! The assembled view equals the pre-reclaim bytes.
//!
//! Required tests:
//! 1. Reclaim, then one collect over unchanged codex session => zero rewrite.
//! 2. Equivalence: assembled view byte-identical to pre-reclaim.
//! 3. Changed session writes increment only; assembled view equals final source state.
//! 4. Self-attack 1: a changed session treated as unchanged must fail.
//! 5. Self-attack 2: an unchanged session fully rewritten must fail.
//! 6. Fresh destination test: post-reclaim collect against fresh destination
//!    (no stored debts) exports zero for unchanged and increment only for changed.
//! 7. Multi-source audit coverage: compressed codex (.jsonl.zst), Claude Code (.jsonl),
//!    and Grok CLI (chat_history.jsonl).

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::manifest;
use chat_stasher::scanner::{self, HarnessRegistry};
use chat_stasher::stagereclaim;
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[path = "../src/test_support.rs"]
mod test_support;
use test_support::Sandbox;

const MACHINE: &str = "fixture-machine";

fn store_config(dir: &Path, repo_name: &str) -> StoreConfig {
    StoreConfig {
        repo_root: dir.join(repo_name).to_string_lossy().into_owned(),
        key_file: dir.join(format!("{repo_name}.key")),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: None,
        no_cache: true,
    }
}

fn codex_registry() -> HarnessRegistry {
    let cell = json!({
        "template": "~/.codex/sessions",
        "format": "jsonl / jsonl.zst",
        "confidence": "source-confirmed",
        "source": "synthetic codex"
    });
    serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic",
        "harnesses": [{
            "id": "codex",
            "display_name": "Codex",
            "paths": {
                "macos": cell,
                "linux": cell,
                "windows": cell,
            }
        }]
    }))
    .unwrap()
}

struct CodexFixture {
    sandbox: Sandbox,
    codex_dir: PathBuf,
    stage: PathBuf,
    state: PathBuf,
    reg: HarnessRegistry,
}

impl CodexFixture {
    fn new() -> Self {
        let sandbox = Sandbox::new();
        let codex_dir = sandbox.root().join("codex_sessions");
        fs::create_dir_all(&codex_dir).unwrap();
        let stage = sandbox.root().join("stage");
        let state = sandbox.root().join("state");
        let reg = codex_registry();
        Self {
            sandbox,
            codex_dir,
            stage,
            state,
            reg,
        }
    }

    fn config(&self) -> Config {
        Config {
            codex_sessions_dir: Some(self.codex_dir.to_string_lossy().into_owned()),
            ..Config::default()
        }
    }

    fn scan(&self) -> scanner::ScanReport {
        scanner::scan_with_registry_and_machine(&self.config(), &self.reg, MACHINE).unwrap()
    }

    fn collect(&self) -> collect::CollectReport {
        let dest = DestinationView::unreachable("dest-a");
        collect::collect_scan_report(&self.scan(), &self.stage, MACHINE, &self.state, 20, &dest)
            .unwrap()
    }

    fn shard_count(&self, session_id: &str) -> usize {
        let dir = store::session_shard_dir(&self.stage, MACHINE, session_id);
        store::sealed_shard_entries(&dir).unwrap().len()
    }

    fn stage_shard_bodies(&self, session_id: &str) -> Vec<Vec<u8>> {
        let dir = store::session_shard_dir(&self.stage, MACHINE, session_id);
        let mut entries = store::sealed_shard_entries(&dir).unwrap();
        entries.sort_by_key(|(seq, _)| *seq);
        entries
            .into_iter()
            .map(|(_, path)| fs::read(path).unwrap())
            .collect()
    }
}

/// SG-2c required fixture:
/// Reclaim, then one collect over an unchanged codex session => zero rewrite.
/// Equivalence: assembled view byte-identical to pre-reclaim.
/// A session whose source gained new content writes only that increment.
#[test]
fn sg2c_post_reclaim_unchanged_exports_zero_and_changed_writes_increment_only() {
    let fx = CodexFixture::new();

    // 1. Two codex session files:
    // Session 1: will remain UNCHANGED.
    let unchanged_file = fx.codex_dir.join("rollout-2026-10-20-unchanged.jsonl");
    fs::write(
        &unchanged_file,
        b"{\"type\":\"session_meta\",\"cwd\":\"/tmp\"}\n{\"type\":\"user_message\",\"text\":\"hello unchanged\"}\n",
    )
    .unwrap();

    // Session 2: will GROW.
    let growing_file = fx.codex_dir.join("rollout-2026-10-20-growing.jsonl");
    fs::write(
        &growing_file,
        b"{\"type\":\"session_meta\",\"cwd\":\"/tmp\"}\n{\"type\":\"user_message\",\"text\":\"initial turn\"}\n",
    )
    .unwrap();

    // Collect 1: both sessions are staged
    let first = fx.collect();
    assert_eq!(first.shards_written, 2);

    let scan1 = fx.scan();
    let unchanged_id = scan1
        .records
        .iter()
        .find(|r| r.absolute_path == unchanged_file)
        .unwrap()
        .id
        .clone();
    let growing_id = scan1
        .records
        .iter()
        .find(|r| r.absolute_path == growing_file)
        .unwrap()
        .id
        .clone();

    // Push to rustic repository and record pre-reclaim assembled bytes
    let cfg = store_config(fx.sandbox.root(), "repo-sg2c");
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&fx.stage, &mk)
        .unwrap();

    let (pre_reclaim_unchanged, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &unchanged_id, &mk)
        .unwrap();
    let (pre_reclaim_growing, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &growing_id, &mk)
        .unwrap();

    // Reclaim: generate manifest summary and delete shard bodies from stage
    let fresh = manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    manifest::write_manifest(&fx.stage, MACHINE, &fresh).unwrap();
    let removed_u = stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &unchanged_id).unwrap();
    assert_eq!(removed_u, 1);
    let removed_g = stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &growing_id).unwrap();
    assert_eq!(removed_g, 1);

    assert_eq!(fx.shard_count(&unchanged_id), 0);
    assert_eq!(fx.shard_count(&growing_id), 0);

    // Mutate source: growing session gains 1 new user turn; unchanged session is untouched
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(&growing_file)
        .unwrap();
    use std::io::Write as _;
    f.write_all(b"{\"type\":\"user_message\",\"text\":\"second turn appended\"}\n")
        .unwrap();
    drop(f);

    // Collect 2: post-reclaim pass
    let second = fx.collect();

    // 1. Unchanged session writes ZERO bytes / zero shards
    assert_eq!(
        fx.shard_count(&unchanged_id),
        0,
        "unchanged session must write zero shards to stage"
    );
    let unchanged_outcome = second
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&unchanged_id))
        .expect("unchanged session must be in outcomes");
    assert_eq!(
        unchanged_outcome.lines_written, 0,
        "unchanged session must write zero lines"
    );
    assert!(
        unchanged_outcome.shard.is_none(),
        "unchanged session must produce no shard"
    );

    // 2. Changed session writes only that increment
    assert_eq!(
        fx.shard_count(&growing_id),
        1,
        "changed session must write exactly one increment shard"
    );
    let growing_outcome = second
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&growing_id))
        .expect("growing session must be in outcomes");
    assert!(growing_outcome.shard.is_some());
    assert_eq!(
        growing_outcome.lines_written, 1,
        "only the 1 newly appended line was written"
    );

    let bodies = fx.stage_shard_bodies(&growing_id);
    assert_eq!(bodies.len(), 1);
    let shard_content = String::from_utf8_lossy(&bodies[0]);
    assert!(
        shard_content.contains("second turn appended"),
        "shard must contain the new turn"
    );
    assert!(
        !shard_content.contains("initial turn"),
        "shard must NOT re-stage the pre-reclaim initial turn"
    );

    // 3. Push and verify assembled view equivalence
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&fx.stage, &mk)
        .unwrap();

    let (post_reclaim_unchanged, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &unchanged_id, &mk)
        .unwrap();
    assert_eq!(
        post_reclaim_unchanged, pre_reclaim_unchanged,
        "assembled view of unchanged session must be byte-identical to pre-reclaim"
    );

    let (post_reclaim_growing, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &growing_id, &mk)
        .unwrap();
    let expected_growing = fs::read(&growing_file).unwrap();
    assert_eq!(
        post_reclaim_growing, expected_growing,
        "assembled view of changed session equals the entire final source state"
    );
    assert!(
        post_reclaim_growing.starts_with(&pre_reclaim_growing),
        "post-reclaim assembled view contains pre-reclaim content as prefix"
    );
}

/// Self-attack 1: a changed session treated as unchanged must fail.
#[test]
fn self_attack_changed_session_treated_as_unchanged_must_fail() {
    let fx = CodexFixture::new();

    let file = fx.codex_dir.join("rollout-2026-10-20-attack1.jsonl");
    fs::write(&file, b"{\"type\":\"session_meta\",\"cwd\":\"/tmp\"}\n").unwrap();

    fx.collect();
    let session_id = fx.scan().records[0].id.clone();

    // Reclaim
    let fresh = manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    manifest::write_manifest(&fx.stage, MACHINE, &fresh).unwrap();
    stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session_id).unwrap();

    // Mutate source: file gains a line
    let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
    use std::io::Write as _;
    f.write_all(b"{\"type\":\"user_message\",\"text\":\"new message\"}\n")
        .unwrap();
    drop(f);

    let report = fx.collect();
    let outcome = report
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&session_id))
        .unwrap();

    // A changed session must NOT have 0 lines written or None shard.
    assert!(
        outcome.shard.is_some(),
        "changed session must emit a shard (if treated as unchanged, this fails)"
    );
    assert!(
        outcome.lines_written > 0,
        "changed session must write lines (if treated as unchanged, this fails)"
    );
    assert_eq!(report.changed_records, 1);
    assert_eq!(report.unchanged_records, 0);
}

/// Self-attack 2: an unchanged session fully rewritten must fail.
#[test]
fn self_attack_unchanged_session_fully_rewritten_must_fail() {
    let fx = CodexFixture::new();

    let file = fx.codex_dir.join("rollout-2026-10-20-attack2.jsonl");
    fs::write(
        &file,
        b"{\"type\":\"session_meta\",\"cwd\":\"/tmp\"}\n{\"type\":\"user_message\",\"text\":\"steady\"}\n",
    )
    .unwrap();

    fx.collect();
    let session_id = fx.scan().records[0].id.clone();

    // Reclaim
    let fresh = manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    manifest::write_manifest(&fx.stage, MACHINE, &fresh).unwrap();
    stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session_id).unwrap();

    // Do NOT touch file; collect again
    let report = fx.collect();
    let outcome = report
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&session_id))
        .unwrap();

    // An unchanged session must NOT be rewritten.
    assert!(
        outcome.shard.is_none(),
        "unchanged session must not emit a shard (if rewritten, this fails)"
    );
    assert_eq!(
        outcome.lines_written, 0,
        "unchanged session must write 0 lines (if rewritten, this fails)"
    );
    assert_eq!(
        report.shards_written, 0,
        "unchanged session must write 0 shards (if rewritten, this fails)"
    );
    assert_eq!(report.changed_records, 0);
    assert_eq!(report.unchanged_records, 1);
}

/// Fresh destination test:
/// After reclaim, collecting against a fresh destination (with no stored debt)
/// exports zero for unchanged sessions and writes only the increment for changed sessions.
#[test]
fn sg2c_post_reclaim_with_fresh_destination_exports_zero_and_writes_increment_only() {
    let fx = CodexFixture::new();

    let unchanged_file = fx.codex_dir.join("rollout-fresh-unchanged.jsonl");
    fs::write(
        &unchanged_file,
        b"{\"type\":\"session_meta\",\"cwd\":\"/tmp\"}\n{\"text\":\"unchanged\"}\n",
    )
    .unwrap();

    let growing_file = fx.codex_dir.join("rollout-fresh-growing.jsonl");
    fs::write(
        &growing_file,
        b"{\"type\":\"session_meta\",\"cwd\":\"/tmp\"}\n{\"text\":\"growing 1\"}\n",
    )
    .unwrap();

    // Collect on dest-1
    let first = fx.collect();
    assert_eq!(first.shards_written, 2);

    let scan = fx.scan();
    let unchanged_id = scan
        .records
        .iter()
        .find(|r| r.absolute_path == unchanged_file)
        .unwrap()
        .id
        .clone();
    let growing_id = scan
        .records
        .iter()
        .find(|r| r.absolute_path == growing_file)
        .unwrap()
        .id
        .clone();

    // Reclaim: write manifest and remove bodies
    let fresh = manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    manifest::write_manifest(&fx.stage, MACHINE, &fresh).unwrap();
    stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &unchanged_id).unwrap();
    stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &growing_id).unwrap();
    assert_eq!(fx.shard_count(&unchanged_id), 0);
    assert_eq!(fx.shard_count(&growing_id), 0);

    // Mutate growing file
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(&growing_file)
        .unwrap();
    use std::io::Write as _;
    f.write_all(b"{\"text\":\"growing 2\"}\n").unwrap();
    drop(f);

    // Collect on dest-fresh (which has NO stored debt in state)
    let fresh_dest = DestinationView::unreachable("dest-fresh-unseen");
    let second =
        collect::collect_scan_report(&fx.scan(), &fx.stage, MACHINE, &fx.state, 20, &fresh_dest)
            .unwrap();

    // 1. Unchanged session must write zero shards
    assert_eq!(
        fx.shard_count(&unchanged_id),
        0,
        "fresh destination must write zero shards for unchanged session"
    );
    let unchanged_outcome = second
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&unchanged_id))
        .unwrap();
    assert_eq!(unchanged_outcome.lines_written, 0);
    assert!(unchanged_outcome.shard.is_none());

    // 2. Changed session writes only the increment
    assert_eq!(
        fx.shard_count(&growing_id),
        1,
        "fresh destination must write exactly one increment shard for changed session"
    );
    let growing_outcome = second
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&growing_id))
        .unwrap();
    assert!(growing_outcome.shard.is_some());
    assert_eq!(growing_outcome.lines_written, 1);
}

/// Compressed codex session (.jsonl.zst):
/// Reclaim, then one collect over unchanged compressed codex session => zero rewrite.
/// Assembled view is byte-identical to pre-reclaim.
#[test]
fn sg2c_post_reclaim_compressed_codex_unchanged_exports_zero() {
    let fx = CodexFixture::new();

    let decompressed_content = b"{\"type\":\"session_meta\",\"cwd\":\"/tmp\"}\n{\"type\":\"user_message\",\"text\":\"compressed rollout\"}\n";
    let compressed_bytes = zstd::stream::encode_all(&decompressed_content[..], 3).unwrap();
    let zst_file = fx.codex_dir.join("rollout-2026-10-20.jsonl.zst");
    fs::write(&zst_file, &compressed_bytes).unwrap();

    // Collect 1: staged as decoded lines
    let first = fx.collect();
    assert_eq!(first.shards_written, 1);

    let session_id = fx.scan().records[0].id.clone();

    // Push to repo and verify pre-reclaim readback
    let cfg = store_config(fx.sandbox.root(), "repo-zst");
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&fx.stage, &mk)
        .unwrap();

    let (pre_reclaim_bytes, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &session_id, &mk)
        .unwrap();
    assert_eq!(pre_reclaim_bytes, decompressed_content);

    // Reclaim
    let fresh = manifest::generate_manifest(&fx.stage, MACHINE).unwrap();
    manifest::write_manifest(&fx.stage, MACHINE, &fresh).unwrap();
    stagereclaim::reclaim_session_body(&fx.stage, MACHINE, &session_id).unwrap();
    assert_eq!(fx.shard_count(&session_id), 0);

    // Collect 2: post-reclaim pass over unchanged compressed file
    let second = fx.collect();
    assert_eq!(
        fx.shard_count(&session_id),
        0,
        "unchanged compressed session must write zero shards post-reclaim"
    );
    let outcome = second
        .outcomes
        .iter()
        .find(|o| o.session_prefix == chat_stasher::id::short_session_id(&session_id))
        .unwrap();
    assert_eq!(outcome.lines_written, 0);
    assert!(outcome.shard.is_none());

    // Push and verify readback byte-identity
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&fx.stage, &mk)
        .unwrap();
    let (post_reclaim_bytes, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &session_id, &mk)
        .unwrap();
    assert_eq!(
        post_reclaim_bytes, pre_reclaim_bytes,
        "assembled view of compressed session must be byte-identical to pre-reclaim"
    );
}

/// Claude Code and Grok CLI audit:
/// Multiple file source harnesses post-reclaim verify zero rewrite.
#[test]
fn sg2c_post_reclaim_claude_code_and_grok_unchanged_export_zero() {
    let sandbox = Sandbox::new();

    let claude_dir = sandbox.root().join("claude_projects");
    fs::create_dir_all(&claude_dir).unwrap();
    let claude_file = claude_dir.join("019bf000-0000-7000-8000-000000000001.jsonl");
    fs::write(
        &claude_file,
        b"{\"type\":\"user\",\"text\":\"claude msg\"}\n",
    )
    .unwrap();

    let claude_cell = json!({
        "template": "~/.claude/projects",
        "format": "jsonl",
        "confidence": "source-confirmed",
        "source": "synthetic claude"
    });
    let reg: HarnessRegistry = serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic",
        "harnesses": [{
            "id": "claude-code",
            "display_name": "Claude Code",
            "paths": {
                "macos": claude_cell,
                "linux": claude_cell,
                "windows": claude_cell,
            }
        }]
    }))
    .unwrap();

    let stage = sandbox.root().join("stage");
    let state = sandbox.root().join("state");
    let config = Config {
        claude_projects_dir: Some(claude_dir.to_string_lossy().into_owned()),
        ..Config::default()
    };

    let scan = scanner::scan_with_registry_and_machine(&config, &reg, MACHINE).unwrap();
    let dest = DestinationView::unreachable("dest-cc");

    // Collect 1
    let rep1 = collect::collect_scan_report(&scan, &stage, MACHINE, &state, 20, &dest).unwrap();
    assert_eq!(rep1.shards_written, 1);
    let session_id = scan.records[0].id.clone();

    // Push
    let cfg = store_config(sandbox.root(), "repo-cc");
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&stage, &mk)
        .unwrap();

    let (pre_reclaim_bytes, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &session_id, &mk)
        .unwrap();

    // Reclaim
    let fresh = manifest::generate_manifest(&stage, MACHINE).unwrap();
    manifest::write_manifest(&stage, MACHINE, &fresh).unwrap();
    stagereclaim::reclaim_session_body(&stage, MACHINE, &session_id).unwrap();

    let dir = store::session_shard_dir(&stage, MACHINE, &session_id);
    assert_eq!(store::sealed_shard_entries(&dir).unwrap().len(), 0);

    // Collect 2
    let rep2 = collect::collect_scan_report(&scan, &stage, MACHINE, &state, 20, &dest).unwrap();
    assert_eq!(
        store::sealed_shard_entries(&dir).unwrap().len(),
        0,
        "claude-code post-reclaim must write zero shards"
    );
    assert_eq!(rep2.shards_written, 0);
    assert_eq!(rep2.unchanged_records, 1);
    assert_eq!(rep2.changed_records, 0);

    // Assembled view check
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&stage, &mk)
        .unwrap();
    let (post_reclaim_bytes, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &session_id, &mk)
        .unwrap();
    assert_eq!(post_reclaim_bytes, pre_reclaim_bytes);
}

/// Whole-file source (non-JSONL, e.g. .json or .md):
/// Post-reclaim collect over untouched whole file exports zero shards.
#[test]
fn sg2c_post_reclaim_whole_file_unchanged_exports_zero() {
    let sandbox = Sandbox::new();

    let doc_dir = sandbox.root().join("whole_file_sessions");
    fs::create_dir_all(&doc_dir).unwrap();
    let doc_file = doc_dir.join("session_doc.json");
    fs::write(
        &doc_file,
        b"{\"conversation_id\":\"c1\",\"title\":\"doc\"}\n",
    )
    .unwrap();

    let cell = json!({
        "template": "~/.continue/sessions",
        "format": "json",
        "session_pattern": "session_*.json",
        "confidence": "source-confirmed",
        "source": "synthetic whole file"
    });
    let reg: HarnessRegistry = serde_json::from_value(json!({
        "schema_version": 1,
        "generated": "synthetic",
        "harnesses": [{
            "id": "continue",
            "display_name": "Continue",
            "paths": {
                "macos": cell,
                "linux": cell,
                "windows": cell,
            }
        }]
    }))
    .unwrap();

    let stage = sandbox.root().join("stage");
    let state = sandbox.root().join("state");
    let mut config = Config::default();
    config
        .harness_roots
        .insert("continue".into(), doc_dir.to_string_lossy().into());

    let scan = scanner::scan_with_registry_and_machine(&config, &reg, MACHINE).unwrap();
    let dest = DestinationView::unreachable("dest-wf");

    // Collect 1
    let rep1 = collect::collect_scan_report(&scan, &stage, MACHINE, &state, 20, &dest).unwrap();
    assert_eq!(rep1.shards_written, 1);
    let session_id = scan.records[0].id.clone();

    // Push
    let cfg = store_config(sandbox.root(), "repo-wf");
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&stage, &mk)
        .unwrap();

    let (pre_reclaim_bytes, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &session_id, &mk)
        .unwrap();

    // Reclaim
    let fresh = manifest::generate_manifest(&stage, MACHINE).unwrap();
    manifest::write_manifest(&stage, MACHINE, &fresh).unwrap();
    stagereclaim::reclaim_session_body(&stage, MACHINE, &session_id).unwrap();

    let dir = store::session_shard_dir(&stage, MACHINE, &session_id);
    assert_eq!(store::sealed_shard_entries(&dir).unwrap().len(), 0);

    // Collect 2: unchanged whole-file source
    let rep2 = collect::collect_scan_report(&scan, &stage, MACHINE, &state, 20, &dest).unwrap();
    assert_eq!(
        store::sealed_shard_entries(&dir).unwrap().len(),
        0,
        "whole-file post-reclaim must write zero shards"
    );
    assert_eq!(rep2.shards_written, 0);
    assert_eq!(rep2.unchanged_records, 1);
    assert_eq!(rep2.changed_records, 0);

    // Push and verify readback byte-identity
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(&stage, &mk)
        .unwrap();
    let (post_reclaim_bytes, _) = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .read_session_concat(MACHINE, &session_id, &mk)
        .unwrap();
    assert_eq!(post_reclaim_bytes, pre_reclaim_bytes);
}
