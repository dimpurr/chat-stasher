//! W4d — a resumed Antigravity session keeps provenance across a reclaimed body.
//!
//! This exercises the real scanner, collector, archive push/readback, reclaim,
//! and archive-only activity-index rebuild using only synthetic transcript
//! lines in a temporary directory. Assertions inspect aggregate dimensions,
//! hashes, and byte equality; the fixture is not a real conversation.

use chat_stasher::collect::{self, DestinationView};
use chat_stasher::config::Config;
use chat_stasher::manifest;
use chat_stasher::scanner::{self, HarnessRegistry};
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use sha2::Digest;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MACHINE: &str = "fixture-machine";

#[path = "../src/test_support.rs"]
mod test_support;

fn registry_value(app_root: &Path, cli_root: &Path) -> serde_json::Value {
    let cell = |root: &Path| {
        serde_json::json!({
            "template": root.to_string_lossy(),
            "format": "jsonl",
            "confidence": "source-confirmed",
            "source": "synthetic fixture",
            "session_dir": {
                "pattern": "*",
                "file": ".system_generated/logs/transcript.jsonl"
            }
        })
    };
    let paths = |root: &Path| match scanner::current_platform() {
        "macos" => serde_json::json!({"macos": cell(root)}),
        "linux" => serde_json::json!({"linux": cell(root)}),
        "windows" => serde_json::json!({"windows": cell(root)}),
        platform => panic!("unexpected platform: {platform}"),
    };
    serde_json::json!({
        "schema_version": 1,
        "generated": "synthetic fixture",
        "harnesses": [{
            "id": "google-antigravity",
            "display_name": "synthetic Antigravity",
            "source_roots": [
                {
                    "id": "antigravity-app",
                    "paths": paths(app_root),
                    "provenance": {"surface": ["app"]}
                },
                {
                    "id": "antigravity-cli",
                    "paths": paths(cli_root),
                    "provenance": {"surface": ["cli"]}
                }
            ]
        }]
    })
}

fn add_app_source_root(registry: &mut serde_json::Value, root: &Path, id: &str) {
    let cell = serde_json::json!({
        "template": root.to_string_lossy(),
        "format": "jsonl",
        "confidence": "source-confirmed",
        "source": "synthetic fixture",
        "session_dir": {
            "pattern": "*",
            "file": ".system_generated/logs/transcript.jsonl"
        }
    });
    let paths = match scanner::current_platform() {
        "macos" => serde_json::json!({"macos": cell}),
        "linux" => serde_json::json!({"linux": cell}),
        "windows" => serde_json::json!({"windows": cell}),
        platform => panic!("unexpected platform: {platform}"),
    };
    registry["harnesses"][0]["source_roots"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "id": id,
            "paths": paths,
            "provenance": {"surface": ["app"]}
        }));
}

fn scan(registry: &HarnessRegistry) -> scanner::ScanReport {
    scanner::scan_with_registry_and_machine(&Config::default(), registry, MACHINE).unwrap()
}

fn write_transcript(root: &Path, session: &str, bytes: &[u8]) {
    let path = root
        .join(session)
        .join(".system_generated/logs/transcript.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn store_config(repo: &Path, key: &Path, sandbox: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(sandbox.join("rustic-cache")),
        no_cache: false,
    }
}

fn reachable_destination(cfg: &StoreConfig, mk: &MasterKey) -> DestinationView<'static> {
    let archive = BackupStore::new(cfg.clone(), MACHINE.to_string());
    let mk = mk.clone();
    DestinationView::new(collect::destination_id(&cfg.repo_root), move |wanted| {
        let readback = archive.read_cumulative_sessions(&mk, Some(wanted))?;
        Ok(collect::archive_facts_from_readback(&readback))
    })
}

fn write_sandbox_config(sandbox: &Path, repo: &Path, key: &Path) {
    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "machine = '{MACHINE}'\nrustic_cache_dir = '{}'\n\n[destinations.fixture]\nrepo = '{}'\nkey_file = '{}'\n",
            sandbox.join("rustic-cache").display(),
            repo.display(),
            key.display()
        ),
    )
    .unwrap();
}

fn run_cli(sandbox: &Path, registry: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("USERPROFILE", &home)
        .env("LOCALAPPDATA", home.join("AppData").join("Local"))
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("cache"))
        .env("CHAT_STASHER_REGISTRY", registry)
        .output()
        .unwrap()
}

fn run_rebuild(sandbox: &Path, registry: &Path) -> Output {
    run_cli(
        sandbox,
        registry,
        &[
            "activity-index",
            "--rebuild",
            "--destination",
            "fixture",
            "--machine",
            MACHINE,
        ],
    )
}

fn reported(stdout: &str, label: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let rest = line.strip_prefix(&format!("[activity-index] {label}"))?;
        Some(rest.trim_start_matches([' ', ':']).trim().to_string())
    })
}

fn index_rows(path: &Path) -> BTreeMap<String, serde_json::Value> {
    fs::read_to_string(path)
        .expect("activity index is readable")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            (row["session_id"].as_str().unwrap().to_string(), row)
        })
        .collect()
}

fn archived_body(archive: &BackupStore, mk: &MasterKey, session_id: &str) -> Vec<u8> {
    let mut body = Vec::new();
    let report = archive
        .for_each_archived_session_shards_with_policy(
            mk,
            MACHINE,
            store::DuplicateShardPolicy::Collapse,
            |id, shards| {
                if id == session_id {
                    for (_, shard) in shards {
                        body.extend_from_slice(shard);
                    }
                }
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(
        report.sessions, 1,
        "the archive walk found one synthetic session"
    );
    body
}

fn assert_session_payload_readers(
    archive: &BackupStore,
    mk: &MasterKey,
    session_id: &str,
    expected: &[u8],
) {
    let (body, _) = archive
        .read_session_concat(MACHINE, session_id, mk)
        .unwrap();
    assert_eq!(
        body, expected,
        "single-session read must merge archived sequences"
    );
    let wanted = [(MACHINE.to_string(), session_id.to_string())]
        .into_iter()
        .collect();
    let selected = archive.read_selected_sessions(mk, &wanted).unwrap();
    let (body, _) = selected
        .get(&(MACHINE.to_string(), session_id.to_string()))
        .expect("selected read includes the requested archived session");
    assert_eq!(
        body, expected,
        "selected-session read must merge archived sequences"
    );
}

fn cached_archive_reader(cfg: &StoreConfig, root: &Path) -> BackupStore {
    let cache = chat_stasher::body_cache::BodyCache::new(root.join("body-cache"), 1 << 20);
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .with_body_cache(Some(std::sync::Arc::new(cache)))
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn tree_digest(root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut files = BTreeMap::<String, Vec<u8>>::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut hash = Sha256::new();
    for (path, bytes) in files {
        hash.update(path.as_bytes());
        hash.update([0]);
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cross_root_reclaimed_resume(full_replay: bool) {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    let app_root = root.join("source/z-antigravity-app/brain");
    let cli_root = root.join("source/antigravity-cli/brain");
    let third_root = root.join("source/zz-antigravity-third/brain");
    fs::create_dir_all(&app_root).unwrap();
    let mut registry_json = registry_value(&app_root, &cli_root);
    add_app_source_root(&mut registry_json, &third_root, "antigravity-third-app");
    let registry: HarnessRegistry = serde_json::from_value(registry_json.clone()).unwrap();
    let registry_path = root.join("registry.json");
    fs::write(&registry_path, serde_json::to_vec(&registry_json).unwrap()).unwrap();

    let session = "session-same-uuid";
    let first_line = b"{\"created_at\":\"2026-10-01T10:00:00Z\",\"type\":\"USER_INPUT\",\"content\":\"synthetic first\"}\n";
    let second_line = b"{\"created_at\":\"2026-10-01T10:00:01Z\",\"type\":\"PLANNER_RESPONSE\",\"content\":\"synthetic second\"}\n";
    let third_line = b"{\"created_at\":\"2026-10-01T10:00:02Z\",\"type\":\"PLANNER_RESPONSE\",\"content\":\"synthetic third\"}\n";
    let complete_body = [first_line.as_slice(), second_line.as_slice()].concat();
    let extended_body = [complete_body.as_slice(), third_line.as_slice()].concat();
    write_transcript(&cli_root, session, first_line);

    let stage = root.join("stage");
    let state = root.join("collector-state");
    let repo = root.join("repository");
    let key = root.join("keys/masterkey.json");
    let cfg = store_config(&repo, &key, root);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let destination = || DestinationView::unreachable(collect::destination_id(&cfg.repo_root));

    let first_scan = scan(&registry);
    assert_eq!(first_scan.records.len(), 1);
    let session_id = first_scan.records[0].id.clone();
    assert!(session_id.starts_with("google-antigravity."));
    let first =
        collect::collect_scan_report(&first_scan, &stage, MACHINE, &state, 20, &destination())
            .unwrap();
    assert_eq!(first.lines_written, 1);
    assert_eq!(
        store::concat_shards(&stage, MACHINE, &session_id).unwrap(),
        first_line
    );

    let archive = BackupStore::new(cfg.clone(), MACHINE.to_string());
    archive.push(&stage, &mk).unwrap();

    // Mirror reclaim-stage's ordering: persist the body identity before the
    // body is removed, then call the same deletion helper the command uses.
    let baseline = manifest::generate_manifest(&stage, MACHINE).unwrap();
    manifest::write_manifest(&stage, MACHINE, &baseline).unwrap();
    assert_eq!(
        chat_stasher::stagereclaim::reclaim_session_body(&stage, MACHINE, &session_id).unwrap(),
        1
    );

    // APP may contribute only the new record or replay the full session from a
    // second root. Both must leave the archive with the same logical session.
    let app_input = if full_replay {
        complete_body.as_slice()
    } else {
        second_line.as_slice()
    };
    write_transcript(&app_root, session, app_input);
    let resumed_scan = scan(&registry);
    assert_eq!(resumed_scan.records.len(), 2);
    let destination_after_reclaim = reachable_destination(&cfg, &mk);
    let resumed = collect::collect_scan_report(
        &resumed_scan,
        &stage,
        MACHINE,
        &state,
        20,
        &destination_after_reclaim,
    )
    .unwrap();
    assert!(resumed.errors.is_empty());
    assert_eq!(
        resumed.lines_written,
        app_input.iter().filter(|byte| **byte == b'\n').count()
    );
    assert_eq!(
        store::concat_shards(&stage, MACHINE, &session_id).unwrap(),
        app_input,
        "a fresh root preserves its complete source body; overlap is ambiguous"
    );
    let resumed_shards =
        store::sealed_shard_entries(&store::session_shard_dir(&stage, MACHINE, &session_id))
            .unwrap();
    assert_eq!(resumed_shards.len(), 1);
    assert_eq!(
        resumed_shards[0].0, 2,
        "the reclaimed sequence is not reused"
    );
    assert_eq!(
        sha256_hex(&fs::read(&resumed_shards[0].1).unwrap()),
        sha256_hex(app_input)
    );

    let observations = chat_stasher::provenance::read_observations(&stage, MACHINE).unwrap();
    let session_observations: Vec<_> = observations
        .iter()
        .filter(|observation| observation.session_id == session_id)
        .collect();
    assert_eq!(
        session_observations.len(),
        2,
        "the historical CLI observation stays append-only alongside APP"
    );
    let current_body_observations: Vec<_> = session_observations
        .iter()
        .filter(|observation| observation.body_sha256 == sha256_hex(app_input))
        .collect();
    assert_eq!(current_body_observations.len(), 1);
    assert!(current_body_observations
        .iter()
        .all(|observation| observation.body_shard_count == 1));
    assert_eq!(current_body_observations[0].dimensions.surface, ["app"]);
    assert_eq!(current_body_observations[0].body_shard_sequences, [2]);
    assert_eq!(
        session_observations
            .iter()
            .filter(|observation| observation.body_sha256 == sha256_hex(first_line))
            .count(),
        1,
        "the stale body-bound observation remains in the append-only sidecar"
    );

    let after_second_push_body = [first_line.as_slice(), app_input].concat();
    archive.push(&stage, &mk).unwrap();
    assert_eq!(
        archived_body(&archive, &mk, &session_id),
        after_second_push_body
    );
    assert_session_payload_readers(
        &cached_archive_reader(&cfg, root),
        &mk,
        &session_id,
        &after_second_push_body,
    );

    let wanted = [(MACHINE.to_string(), session_id.clone())]
        .into_iter()
        .collect();
    let readback = archive
        .read_cumulative_sessions(&mk, Some(&wanted))
        .unwrap();
    assert!(readback.complete());
    let archived = readback
        .machines
        .iter()
        .flat_map(|machine| machine.sessions.iter())
        .find(|session| session.machine == MACHINE && session.session_id == session_id)
        .expect("session is present in cumulative readback after push B");
    assert_eq!(archived.shard_count, 2);
    assert_eq!(archived.concat_bytes, after_second_push_body.len() as u64);
    assert_eq!(archived.sha256, sha256_hex(&after_second_push_body));
    assert_eq!(
        archived.shard_bytes,
        [first_line.len() as u64, app_input.len() as u64]
    );
    assert_eq!(
        archived.shard_sha256,
        [sha256_hex(first_line), sha256_hex(app_input)]
    );
    let facts = collect::archive_facts_from_readback(&readback);
    let fact = &facts[&(MACHINE.to_string(), session_id.clone())];
    assert_eq!(fact.shard_count, 2);
    assert_eq!(fact.concat_bytes, after_second_push_body.len() as u64);
    assert_eq!(fact.concat_sha256, sha256_hex(&after_second_push_body));
    let mut archived_sequences = Vec::new();
    archive
        .for_each_archived_session_shards_with_policy(
            &mk,
            MACHINE,
            store::DuplicateShardPolicy::KeepAll,
            |id, shards| {
                if id == session_id {
                    archived_sequences.extend(shards.iter().map(|(sequence, _)| *sequence));
                }
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(archived_sequences, [Some(1), Some(2)]);

    // Reclaim the resumed body too, then let a third independent root capture
    // A+B+C. A fresh root's overlap with archived A+B is ambiguous, so its
    // complete source bytes are preserved as sequence 3.
    let baseline = manifest::generate_manifest(&stage, MACHINE).unwrap();
    manifest::write_manifest(&stage, MACHINE, &baseline).unwrap();
    assert_eq!(
        chat_stasher::stagereclaim::reclaim_session_body(&stage, MACHINE, &session_id).unwrap(),
        1
    );
    write_transcript(&third_root, session, &extended_body);
    let third_scan = scan(&registry);
    assert_eq!(third_scan.records.len(), 3);
    let third = collect::collect_scan_report(
        &third_scan,
        &stage,
        MACHINE,
        &state,
        20,
        &reachable_destination(&cfg, &mk),
    )
    .unwrap();
    assert!(third.errors.is_empty());
    assert_eq!(third.lines_written, 3);
    assert_eq!(
        store::concat_shards(&stage, MACHINE, &session_id).unwrap(),
        extended_body
    );
    let third_shards =
        store::sealed_shard_entries(&store::session_shard_dir(&stage, MACHINE, &session_id))
            .unwrap();
    assert_eq!(third_shards.len(), 1);
    assert_eq!(third_shards[0].0, 3);
    assert_eq!(
        sha256_hex(&fs::read(&third_shards[0].1).unwrap()),
        sha256_hex(&extended_body)
    );
    archive.push(&stage, &mk).unwrap();
    let after_third_push_body =
        [after_second_push_body.as_slice(), extended_body.as_slice()].concat();
    assert_eq!(
        archived_body(&archive, &mk, &session_id),
        after_third_push_body
    );
    assert_session_payload_readers(
        &cached_archive_reader(&cfg, root),
        &mk,
        &session_id,
        &after_third_push_body,
    );
    write_sandbox_config(root, &repo, &key);
    let repo_before_rebuild = tree_digest(&repo);
    let rebuilt = run_rebuild(root, &registry_path);
    let stdout = String::from_utf8_lossy(&rebuilt.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&rebuilt.stderr).into_owned();
    assert!(
        rebuilt.status.success(),
        "archive-only rebuild failed:\n{stdout}\n{stderr}"
    );
    let index = PathBuf::from(reported(&stdout, "derived").expect("derived index path"));
    assert!(
        index.starts_with(root),
        "derived index escaped fixture: {index:?}"
    );
    let row = index_rows(&index)
        .remove(&session_id)
        .expect("the cumulative session is indexed");
    let expected_line_count = if full_replay { 6 } else { 5 };
    assert_eq!(row["line_count"].as_u64(), Some(expected_line_count));
    assert_eq!(
        row["dimensions"]["surface"],
        serde_json::json!(["app", "cli"])
    );
    assert_eq!(
        tree_digest(&repo),
        repo_before_rebuild,
        "read-only rebuild changed the archive"
    );

    let baseline = manifest::generate_manifest(&stage, MACHINE).unwrap();
    manifest::write_manifest(&stage, MACHINE, &baseline).unwrap();
    assert_eq!(
        chat_stasher::stagereclaim::reclaim_session_body(&stage, MACHINE, &session_id).unwrap(),
        1
    );

    // A subsequent scan of all unchanged CLI, APP, and third-root inputs is a
    // no-op: archive reconciliation must not duplicate rows or rewrite debt.
    let state_before_rescan = tree_digest(&state);
    let stage_before_rescan = tree_digest(&stage);
    let unchanged_scan = scan(&registry);
    let unchanged = collect::collect_scan_report(
        &unchanged_scan,
        &stage,
        MACHINE,
        &state,
        20,
        &reachable_destination(&cfg, &mk),
    )
    .unwrap();
    assert!(unchanged.errors.is_empty());
    assert_eq!(unchanged.lines_written, 0);
    assert_eq!(
        tree_digest(&stage),
        stage_before_rescan,
        "an unchanged rescan creates no new shard"
    );
    assert_eq!(
        tree_digest(&state),
        state_before_rescan,
        "unchanged rescan changed destination debt state"
    );
    assert_eq!(
        archived_body(&archive, &mk, &session_id),
        after_third_push_body
    );
    assert_eq!(
        tree_digest(&repo),
        repo_before_rebuild,
        "rescan changed the archive"
    );
}

#[test]
fn new_root_same_prefix_is_preserved_while_original_shard_is_staged() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    let app_root = root.join("source/z-antigravity-app/brain");
    let cli_root = root.join("source/antigravity-cli/brain");
    fs::create_dir_all(&app_root).unwrap();
    let registry_json = registry_value(&app_root, &cli_root);
    let registry: HarnessRegistry = serde_json::from_value(registry_json.clone()).unwrap();
    let registry_path = root.join("registry.json");
    fs::write(&registry_path, serde_json::to_vec(&registry_json).unwrap()).unwrap();

    let session = "session-stage-prefix-new-root";
    let first_line = b"{\"created_at\":\"2026-10-04T10:00:00Z\",\"type\":\"USER_INPUT\",\"content\":\"synthetic first\"}\n";
    let second_line = b"{\"created_at\":\"2026-10-04T10:00:01Z\",\"type\":\"PLANNER_RESPONSE\",\"content\":\"synthetic second\"}\n";
    let app_body = [first_line.as_slice(), second_line.as_slice()].concat();
    let expected_archive = [first_line.as_slice(), app_body.as_slice()].concat();
    write_transcript(&cli_root, session, first_line);

    let stage = root.join("stage");
    let state = root.join("collector-state");
    let repo = root.join("repository");
    let key = root.join("keys/masterkey.json");
    let cfg = store_config(&repo, &key, root);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let registry_first = scan(&registry);
    assert_eq!(registry_first.records.len(), 1);
    let session_id = registry_first.records[0].id.clone();
    let unreachable = DestinationView::unreachable(collect::destination_id(&cfg.repo_root));
    let first =
        collect::collect_scan_report(&registry_first, &stage, MACHINE, &state, 20, &unreachable)
            .unwrap();
    assert_eq!(first.lines_written, 1);
    assert_eq!(
        store::concat_shards(&stage, MACHINE, &session_id).unwrap(),
        first_line
    );

    write_transcript(&app_root, session, &app_body);
    let registry_second = scan(&registry);
    assert_eq!(registry_second.records.len(), 2);
    let second =
        collect::collect_scan_report(&registry_second, &stage, MACHINE, &state, 20, &unreachable)
            .unwrap();
    assert!(second.errors.is_empty());
    assert_eq!(second.lines_written, 2);
    assert_eq!(
        store::concat_shards(&stage, MACHINE, &session_id).unwrap(),
        expected_archive
    );
    let mut shards =
        store::sealed_shard_entries(&store::session_shard_dir(&stage, MACHINE, &session_id))
            .unwrap();
    shards.sort_by_key(|(sequence, _)| *sequence);
    assert_eq!(shards.len(), 2);
    assert_eq!(shards[0].0, 1);
    assert_eq!(shards[1].0, 2);
    assert_eq!(
        sha256_hex(&fs::read(&shards[0].1).unwrap()),
        sha256_hex(first_line)
    );
    assert_eq!(
        sha256_hex(&fs::read(&shards[1].1).unwrap()),
        sha256_hex(&app_body)
    );
    let observations = chat_stasher::provenance::read_observations(&stage, MACHINE).unwrap();
    let cli = observations
        .iter()
        .find(|observation| {
            observation.session_id == session_id && observation.dimensions.surface == ["cli"]
        })
        .unwrap();
    let app = observations
        .iter()
        .find(|observation| {
            observation.session_id == session_id && observation.dimensions.surface == ["app"]
        })
        .unwrap();
    assert_eq!(cli.body_shard_sequences, [1]);
    assert_eq!(cli.body_sha256, sha256_hex(first_line));
    assert_eq!(app.body_shard_sequences, [1, 2]);
    assert_eq!(app.body_sha256, sha256_hex(&expected_archive));

    let archive = BackupStore::new(cfg.clone(), MACHINE.to_string());
    archive.push(&stage, &mk).unwrap();
    assert_eq!(archived_body(&archive, &mk, &session_id), expected_archive);
    assert_session_payload_readers(
        &cached_archive_reader(&cfg, root),
        &mk,
        &session_id,
        &expected_archive,
    );
    let readback = archive.read_all_machines(&mk).unwrap();
    let row = readback
        .machines
        .iter()
        .flat_map(|machine| machine.sessions.iter())
        .find(|session| session.session_id == session_id)
        .unwrap();
    assert_eq!(row.concat_bytes, expected_archive.len() as u64);
    assert_eq!(row.sha256, sha256_hex(&expected_archive));
    write_sandbox_config(root, &repo, &key);
    let rebuilt = run_rebuild(root, &registry_path);
    let stdout = String::from_utf8_lossy(&rebuilt.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&rebuilt.stderr).into_owned();
    assert!(
        rebuilt.status.success(),
        "activity-index rebuild failed:\n{stdout}\n{stderr}"
    );
    let index = PathBuf::from(reported(&stdout, "derived").expect("derived index path"));
    let row = index_rows(&index).remove(&session_id).unwrap();
    assert_eq!(row["line_count"].as_u64(), Some(3));
}

#[test]
fn cross_root_tail_after_reclaim_preserves_original_surface() {
    cross_root_reclaimed_resume(false);
}

#[test]
fn fresh_root_full_prefix_is_preserved_after_reclaim() {
    cross_root_reclaimed_resume(true);
}

#[test]
fn same_source_appends_identical_turn_and_default_readers_keep_both() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    let app_root = root.join("source/antigravity-app/brain");
    let cli_root = root.join("source/antigravity-cli/brain");
    fs::create_dir_all(&app_root).unwrap();
    let registry_json = registry_value(&app_root, &cli_root);
    let registry: HarnessRegistry = serde_json::from_value(registry_json.clone()).unwrap();
    let registry_path = root.join("registry.json");
    fs::write(&registry_path, serde_json::to_vec(&registry_json).unwrap()).unwrap();

    let session = "session-identical-turn";
    let repeated = b"{\"created_at\":\"2026-10-03T10:00:00Z\",\"type\":\"USER_INPUT\",\"content\":\"synthetic identical event\"}\n";
    let expected_body = [repeated.as_slice(), repeated.as_slice()].concat();
    let source_path = cli_root
        .join(session)
        .join(".system_generated/logs/transcript.jsonl");
    write_transcript(&cli_root, session, repeated);

    let stage = root.join("stage");
    let state = root.join("collector-state");
    let repo = root.join("repository");
    let key = root.join("keys/masterkey.json");
    let cfg = store_config(&repo, &key, root);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let first_scan = scan(&registry);
    assert_eq!(first_scan.records.len(), 1);
    let session_id = first_scan.records[0].id.clone();
    let unreachable = DestinationView::unreachable(collect::destination_id(&cfg.repo_root));
    let first =
        collect::collect_scan_report(&first_scan, &stage, MACHINE, &state, 20, &unreachable)
            .unwrap();
    assert_eq!(first.lines_written, 1);

    let archive = BackupStore::new(cfg.clone(), MACHINE.to_string());
    archive.push(&stage, &mk).unwrap();
    let manifest = manifest::generate_manifest(&stage, MACHINE).unwrap();
    manifest::write_manifest(&stage, MACHINE, &manifest).unwrap();
    assert_eq!(
        chat_stasher::stagereclaim::reclaim_session_body(&stage, MACHINE, &session_id).unwrap(),
        1
    );

    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(&source_path)
        .unwrap()
        .write_all(repeated)
        .unwrap();
    let resumed_scan = scan(&registry);
    assert_eq!(resumed_scan.records.len(), 1);
    let resumed = collect::collect_scan_report(
        &resumed_scan,
        &stage,
        MACHINE,
        &state,
        20,
        &reachable_destination(&cfg, &mk),
    )
    .unwrap();
    assert!(resumed.errors.is_empty());
    assert_eq!(resumed.lines_written, 1);
    assert_eq!(
        store::concat_shards(&stage, MACHINE, &session_id).unwrap(),
        repeated
    );
    let resumed_shards =
        store::sealed_shard_entries(&store::session_shard_dir(&stage, MACHINE, &session_id))
            .unwrap();
    assert_eq!(resumed_shards.len(), 1);
    assert_eq!(resumed_shards[0].0, 2);
    assert_eq!(
        sha256_hex(&fs::read(&resumed_shards[0].1).unwrap()),
        sha256_hex(repeated)
    );

    archive.push(&stage, &mk).unwrap();
    assert_eq!(archived_body(&archive, &mk, &session_id), expected_body);
    assert_session_payload_readers(
        &cached_archive_reader(&cfg, root),
        &mk,
        &session_id,
        &expected_body,
    );
    let repo_before_readers = tree_digest(&repo);
    let default_readback = archive.read_all_machines(&mk).unwrap();
    assert!(default_readback.complete());
    let default_row = default_readback
        .machines
        .iter()
        .flat_map(|machine| machine.sessions.iter())
        .find(|session| session.machine == MACHINE && session.session_id == session_id)
        .expect("default readback includes the repeated-turn session");
    assert_eq!(default_row.concat_bytes, expected_body.len() as u64);
    assert_eq!(default_row.sha256, sha256_hex(&expected_body));
    let mut default_archive_body = Vec::new();
    archive
        .for_each_archived_session(&mk, MACHINE, |id, body| {
            if id == session_id {
                default_archive_body.extend_from_slice(body);
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(default_archive_body, expected_body);

    write_sandbox_config(root, &repo, &key);
    let rebuilt = run_rebuild(root, &registry_path);
    let stdout = String::from_utf8_lossy(&rebuilt.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&rebuilt.stderr).into_owned();
    assert!(
        rebuilt.status.success(),
        "archive-only rebuild failed:\n{stdout}\n{stderr}"
    );
    let index = PathBuf::from(reported(&stdout, "derived").expect("derived index path"));
    let row = index_rows(&index)
        .remove(&session_id)
        .expect("session is indexed");
    assert_eq!(row["line_count"].as_u64(), Some(2));

    let search = run_cli(
        root,
        &registry_path,
        &[
            "search",
            "--destination",
            "fixture",
            "--text",
            "synthetic identical event",
            "--scan",
            "--json",
        ],
    );
    let search_stdout = String::from_utf8_lossy(&search.stdout).into_owned();
    let search_stderr = String::from_utf8_lossy(&search.stderr).into_owned();
    assert!(
        search.status.success(),
        "search scan failed:\n{search_stdout}\n{search_stderr}"
    );
    let search_json: serde_json::Value = serde_json::from_str(search_stdout.trim()).unwrap();
    assert_eq!(search_json["mode"], "scan");
    assert_eq!(search_json["selected"].as_u64(), Some(1));
    assert_eq!(search_json["matched"].as_u64(), Some(1));
    assert_eq!(
        tree_digest(&repo),
        repo_before_readers,
        "read-only readers changed archive"
    );
}

fn assert_identical_turn_resume(session: &str, replay: &[u8]) {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    let app_root = root.join("source/z-antigravity-app/brain");
    let cli_root = root.join("source/antigravity-cli/brain");
    fs::create_dir_all(&app_root).unwrap();
    let registry_json = registry_value(&app_root, &cli_root);
    let registry: HarnessRegistry = serde_json::from_value(registry_json.clone()).unwrap();
    let registry_path = root.join("registry.json");
    fs::write(&registry_path, serde_json::to_vec(&registry_json).unwrap()).unwrap();

    let repeated = b"{\"created_at\":\"2026-10-02T10:00:00Z\",\"type\":\"USER_INPUT\",\"content\":\"synthetic repeated turn\"}\n";
    assert!(!repeated.is_empty());
    write_transcript(&cli_root, session, repeated);

    let stage = root.join("stage");
    let state = root.join("collector-state");
    let repo = root.join("repository");
    let key = root.join("keys/masterkey.json");
    let cfg = store_config(&repo, &key, root);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let first_scan = scan(&registry);
    assert_eq!(first_scan.records.len(), 1);
    let session_id = first_scan.records[0].id.clone();
    let unreachable = DestinationView::unreachable(collect::destination_id(&cfg.repo_root));
    let first =
        collect::collect_scan_report(&first_scan, &stage, MACHINE, &state, 20, &unreachable)
            .unwrap();
    assert_eq!(first.lines_written, 1);

    let archive = BackupStore::new(cfg.clone(), MACHINE.to_string());
    archive.push(&stage, &mk).unwrap();
    let manifest = manifest::generate_manifest(&stage, MACHINE).unwrap();
    manifest::write_manifest(&stage, MACHINE, &manifest).unwrap();
    assert_eq!(
        chat_stasher::stagereclaim::reclaim_session_body(&stage, MACHINE, &session_id).unwrap(),
        1
    );

    write_transcript(&app_root, session, replay);
    let resumed_scan = scan(&registry);
    assert_eq!(resumed_scan.records.len(), 2);
    let resumed = collect::collect_scan_report(
        &resumed_scan,
        &stage,
        MACHINE,
        &state,
        20,
        &reachable_destination(&cfg, &mk),
    )
    .unwrap();
    assert!(resumed.errors.is_empty());
    let replay_lines = replay.iter().filter(|byte| **byte == b'\n').count();
    assert_eq!(resumed.lines_written, replay_lines);
    assert_eq!(
        store::concat_shards(&stage, MACHINE, &session_id).unwrap(),
        replay,
        "a fresh root preserves its complete observed body"
    );
    let shards =
        store::sealed_shard_entries(&store::session_shard_dir(&stage, MACHINE, &session_id))
            .unwrap();
    assert_eq!(shards.len(), 1);
    assert_eq!(shards[0].0, 2);
    assert_eq!(
        sha256_hex(&fs::read(&shards[0].1).unwrap()),
        sha256_hex(replay)
    );

    let observations = chat_stasher::provenance::read_observations(&stage, MACHINE).unwrap();
    let app = observations
        .iter()
        .find(|observation| {
            observation.session_id == session_id && observation.dimensions.surface == ["app"]
        })
        .expect("resumed app observation exists");
    assert_eq!(app.body_shard_sequences, [2]);
    assert_eq!(app.body_sha256, sha256_hex(replay));

    let full_archive_body = [repeated.as_slice(), replay].concat();
    archive.push(&stage, &mk).unwrap();
    assert_eq!(archived_body(&archive, &mk, &session_id), full_archive_body);
    let wanted = [(MACHINE.to_string(), session_id.clone())]
        .into_iter()
        .collect();
    let readback = archive
        .read_cumulative_sessions(&mk, Some(&wanted))
        .unwrap();
    let facts = collect::archive_facts_from_readback(&readback);
    let fact = &facts[&(MACHINE.to_string(), session_id.clone())];
    assert_eq!(fact.shard_count, 2);
    assert_eq!(fact.concat_bytes, full_archive_body.len() as u64);
    assert_eq!(fact.concat_sha256, sha256_hex(&full_archive_body));

    write_sandbox_config(root, &repo, &key);
    fs::remove_dir_all(&stage).unwrap();
    let repo_before = tree_digest(&repo);
    let rebuilt = run_rebuild(root, &registry_path);
    let stdout = String::from_utf8_lossy(&rebuilt.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&rebuilt.stderr).into_owned();
    assert!(
        rebuilt.status.success(),
        "archive-only rebuild failed:\n{stdout}\n{stderr}"
    );
    let index = PathBuf::from(reported(&stdout, "derived").expect("derived index path"));
    let row = index_rows(&index)
        .remove(&session_id)
        .expect("repeated-turn session is indexed");
    assert_eq!(row["line_count"].as_u64(), Some((1 + replay_lines) as u64));
    assert_eq!(
        row["dimensions"]["surface"],
        serde_json::json!(["app", "cli"])
    );
    assert_eq!(
        tree_digest(&repo),
        repo_before,
        "read-only rebuild changed the archive"
    );
}

#[test]
fn exact_length_fresh_root_is_preserved_when_prefix_is_ambiguous() {
    let repeated = b"{\"created_at\":\"2026-10-02T10:00:00Z\",\"type\":\"USER_INPUT\",\"content\":\"synthetic repeated turn\"}\n";
    // Equal source/archive lengths are ambiguous: this can be a replay or a
    // second identical event. Read it fully so no legitimate repeat is lost.
    assert_identical_turn_resume("session-repeat-exact-length", repeated);
}

#[test]
fn same_source_suffix_after_reclaim_keeps_both_surfaces_in_archive_only_rebuild() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    let app_root = root.join("source/antigravity-app/brain");
    let cli_root = root.join("source/antigravity-cli/brain");
    fs::create_dir_all(&app_root).unwrap();
    let registry_json = registry_value(&app_root, &cli_root);
    let registry: HarnessRegistry = serde_json::from_value(registry_json.clone()).unwrap();
    let registry_path = root.join("registry.json");
    fs::write(&registry_path, serde_json::to_vec(&registry_json).unwrap()).unwrap();

    let session = "session-same-source";
    let first_line = b"{\"created_at\":\"2026-10-01T10:00:00Z\",\"type\":\"USER_INPUT\",\"content\":\"synthetic first\"}\n";
    let second_line = b"{\"created_at\":\"2026-10-01T10:00:01Z\",\"type\":\"PLANNER_RESPONSE\",\"content\":\"synthetic second\"}\n";
    let complete_body = [first_line.as_slice(), second_line.as_slice()].concat();
    write_transcript(&cli_root, session, first_line);

    let stage = root.join("stage");
    let state = root.join("collector-state");
    let repo = root.join("repository");
    let key = root.join("keys/masterkey.json");
    let cfg = store_config(&repo, &key, root);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let destination = || DestinationView::unreachable(collect::destination_id(&cfg.repo_root));

    let first_scan = scan(&registry);
    assert_eq!(first_scan.records.len(), 1);
    let session_id = first_scan.records[0].id.clone();
    let first =
        collect::collect_scan_report(&first_scan, &stage, MACHINE, &state, 20, &destination())
            .unwrap();
    assert_eq!(first.lines_written, 1);
    assert_eq!(first_scan.records[0].provenance.surface, ["cli"]);

    let archive = BackupStore::new(cfg.clone(), MACHINE.to_string());
    archive.push(&stage, &mk).unwrap();
    let baseline = manifest::generate_manifest(&stage, MACHINE).unwrap();
    manifest::write_manifest(&stage, MACHINE, &baseline).unwrap();
    assert_eq!(
        chat_stasher::stagereclaim::reclaim_session_body(&stage, MACHINE, &session_id).unwrap(),
        1
    );

    // Resume the same JSONL input by appending one complete record. Antigravity
    // scanner provenance is changed in the supplied report to simulate the
    // source observation changing from CLI to app; only that typed observation
    // is simulated, never a shard or sidecar row.
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(
            cli_root
                .join(session)
                .join(".system_generated/logs/transcript.jsonl"),
        )
        .unwrap()
        .write_all(second_line)
        .unwrap();
    let mut resumed_scan = scan(&registry);
    assert_eq!(resumed_scan.records.len(), 1);
    let mut app_observation = chat_stasher::provenance::SessionProvenance::default();
    app_observation.insert_surface("app");
    resumed_scan.records[0].provenance = app_observation;
    let destination_after_reclaim = reachable_destination(&cfg, &mk);
    let resumed = collect::collect_scan_report(
        &resumed_scan,
        &stage,
        MACHINE,
        &state,
        20,
        &destination_after_reclaim,
    )
    .unwrap();
    assert!(resumed.errors.is_empty());
    assert_eq!(resumed.lines_written, 1);
    let shards =
        store::sealed_shard_entries(&store::session_shard_dir(&stage, MACHINE, &session_id))
            .unwrap();
    assert_eq!(shards.len(), 1);
    assert_eq!(
        shards[0].0, 2,
        "the reclaimed sequence number is not reused"
    );
    assert_eq!(
        store::concat_shards(&stage, MACHINE, &session_id).unwrap(),
        second_line
    );

    let observations = chat_stasher::provenance::read_observations(&stage, MACHINE).unwrap();
    let session_observations: Vec<_> = observations
        .iter()
        .filter(|observation| observation.session_id == session_id)
        .collect();
    assert_eq!(session_observations.len(), 2);
    let cli = session_observations
        .iter()
        .find(|observation| observation.dimensions.surface == ["cli"])
        .unwrap();
    let app = session_observations
        .iter()
        .find(|observation| observation.dimensions.surface == ["app"])
        .unwrap();
    assert_eq!(cli.body_shard_sequences, [1]);
    assert_eq!(cli.body_sha256, sha256_hex(first_line));
    assert_eq!(app.body_shard_sequences, [2]);
    assert_eq!(app.body_shard_count, 1);
    assert_eq!(app.body_sha256, sha256_hex(second_line));

    archive.push(&stage, &mk).unwrap();
    assert_eq!(archived_body(&archive, &mk, &session_id), complete_body);
    write_sandbox_config(root, &repo, &key);
    fs::remove_dir_all(&stage).unwrap();
    let repo_before = tree_digest(&repo);
    let rebuilt = run_rebuild(root, &registry_path);
    let stdout = String::from_utf8_lossy(&rebuilt.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&rebuilt.stderr).into_owned();
    assert!(
        rebuilt.status.success(),
        "archive-only rebuild failed:\n{stdout}\n{stderr}"
    );
    let index = PathBuf::from(reported(&stdout, "derived").expect("derived index path"));
    assert!(
        index.starts_with(root),
        "derived index escaped fixture: {index:?}"
    );
    let row = index_rows(&index)
        .remove(&session_id)
        .expect("the reclaimed session is in the rebuilt index");
    assert_eq!(row["line_count"].as_u64(), Some(2));
    assert_eq!(
        row["dimensions"]["surface"],
        serde_json::json!(["app", "cli"])
    );
    assert_eq!(
        tree_digest(&repo),
        repo_before,
        "read-only rebuild changed the archive"
    );
}
