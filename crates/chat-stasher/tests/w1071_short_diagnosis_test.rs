//! W1071: every snapshot-listing read path names a short file with the same
//! recovery words; doctor stays silent when healthy and names it when short.
//!
//! <32 bytes only; a truncated-but-not-short file (>=32 bytes) is a known gap
//! and is not locked here.

use chat_stasher::deletion_control;
use chat_stasher::orphans::{short_files, short_files_words};
use chat_stasher::search::search_sessions;
use chat_stasher::selector::Selector;
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[path = "../src/test_support.rs"]
mod test_support;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn set_mtime(path: &Path, secs: u64) {
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

fn plant(root: &Path, rel: &str, bytes: &[u8], mtime: u64) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    set_mtime(&path, mtime);
}

fn store_config(repo_path: &Path, key_path: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo_path.to_string_lossy().into_owned(),
        key_file: key_path.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: None,
        no_cache: true,
    }
}

fn write_session(stage: &Path, machine: &str, session: &str, nshards: u64) {
    for seq in 1..=nshards {
        let lines: Vec<String> = (0..10)
            .map(|i| format!("{{\"seq\":{seq},\"i\":{i},\"s\":\"payload-{session}-{seq}-{i}\"}}"))
            .collect();
        store::write_sealed_shard(StageWriter::Collect, stage, machine, session, &lines).unwrap();
    }
}

struct Sandbox {
    dir: tempfile::TempDir,
    mk: MasterKey,
}

impl Sandbox {
    fn repo(&self) -> PathBuf {
        self.dir.path().join("remote-repo")
    }
    fn key(&self) -> PathBuf {
        self.dir.path().join("remote.key")
    }

    fn command(&self) -> std::process::Command {
        let home = self.dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
        command
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env(
                test_support::RUSTIC_CACHE_DIR_ENV,
                test_support::rustic_cache_root(&home),
            )
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env(
                "CHAT_STASHER_REGISTRY",
                self.dir.path().join("registry.json"),
            )
            .env_remove("CODEX_HOME")
            .env_remove("RUSTIC_REPO")
            .env_remove("RUSTIC_KEY_FILE");
        command
    }

    fn write_config(&self) {
        let config_dir = self.dir.path().join("config").join("chat-stasher");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.toml"),
            format!(
                "[destinations.storagebox]\nrepo = '{}'\nkey_file = '{}'\nno_cache = \
                 true\n\n[mirror]\nenabled = true\nmirror_of = 'storagebox'\nmin_free_gib = \
                 1.0\n",
                self.repo().display(),
                self.key().display()
            ),
        )
        .unwrap();
        fs::write(
            self.dir.path().join("registry.json"),
            r#"{"schema_version":1,"generated":"W1071 synthetic","harnesses":[]}"#,
        )
        .unwrap();
    }
}

fn sandbox_with_repository() -> Sandbox {
    let dir = tempfile::tempdir().unwrap();
    let cfg = store_config(
        &dir.path().join("remote-repo"),
        &dir.path().join("remote.key"),
    );
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    for (stage, machine) in [("stage-a", "machine-a"), ("stage-b", "machine-b")] {
        write_session(&dir.path().join(stage), machine, "session-a1", 3);
        BackupStore::new(cfg.clone(), machine.to_string())
            .push(&dir.path().join(stage), &mk)
            .unwrap();
    }
    let sandbox = Sandbox { dir, mk };
    sandbox.write_config();
    sandbox
}

/// A 0-byte file under `snapshots/` with a fresh id. The index is intact, so
/// the open succeeds and every `get_all_snapshots` fails.
fn plant_short_snapshot(repo: &Path) -> String {
    let id = "aa".repeat(32);
    plant(repo, &format!("snapshots/{id}"), &[], unix_now());
    id
}

#[test]
fn w1071_short_snapshot_file_is_named_with_recovery_words() {
    let sandbox = sandbox_with_repository();
    let repo = sandbox.repo();
    let store = BackupStore::new(store_config(&repo, &sandbox.key()), "machine-a".to_string());
    let backends = store.backends().unwrap();
    assert!(
        short_files(&backends).is_empty(),
        "a healthy repository has no short files"
    );
    let id = plant_short_snapshot(&repo);
    let files = short_files(&backends);
    assert_eq!(files.len(), 1, "the planted 0-byte snapshot is short");
    assert_eq!(files[0].dir, "snapshots");
    assert_eq!(files[0].id_hex, id);
    assert_eq!(files[0].bytes, 0);
    let words = short_files_words(&files);
    assert!(words.contains(&id), "the diagnosis names the file: {words}");
    assert!(
        words.contains("move exactly"),
        "the diagnosis gives the manual step: {words}"
    );
}

#[test]
fn w1071_snapshot_boundary_10_31_named_32_not() {
    let sandbox = sandbox_with_repository();
    let repo = sandbox.repo();
    let store = BackupStore::new(store_config(&repo, &sandbox.key()), "machine-a".to_string());
    let backends = store.backends().unwrap();
    let now = unix_now();
    let ten = "0a".repeat(32);
    let thirty_one = "1f".repeat(32);
    let thirty_two = "20".repeat(32);
    plant(&repo, &format!("snapshots/{ten}"), &[0u8; 10], now);
    plant(&repo, &format!("snapshots/{thirty_one}"), &[0u8; 31], now);
    plant(&repo, &format!("snapshots/{thirty_two}"), &[0u8; 32], now);
    let files = short_files(&backends);
    let ids: Vec<&str> = files.iter().map(|f| f.id_hex.as_str()).collect();
    assert!(ids.contains(&ten.as_str()), "10 bytes is short");
    assert!(ids.contains(&thirty_one.as_str()), "31 bytes is short");
    assert!(
        !ids.contains(&thirty_two.as_str()),
        "32 bytes is a complete file, never named"
    );
}

fn assert_names_short(error: anyhow::Error, id: &str, path: &str) {
    let text = format!("{error:#}");
    assert!(
        text.contains(id),
        "{path}: the error names the short file {id}: {text}"
    );
    assert!(
        text.contains("move exactly"),
        "{path}: the error gives the manual step: {text}"
    );
}

#[test]
fn w1071_other_list_paths_name_the_same_short_file() {
    let sandbox = sandbox_with_repository();
    let repo = sandbox.repo();
    let cfg = store_config(&repo, &sandbox.key());
    let id = plant_short_snapshot(&repo);

    // readback: newest-per-machine walk.
    let store = BackupStore::new(cfg.clone(), "machine-a".to_string());
    let error = store
        .read_latest_per_machine(&sandbox.mk)
        .expect_err("listing must fail with a short snapshot present");
    assert_names_short(error, &id, "read_latest_per_machine");

    // readback: archived writers.
    let error = store
        .read_archived_writers(&sandbox.mk)
        .expect_err("listing must fail with a short snapshot present");
    assert_names_short(error, &id, "read_archived_writers");

    // search.
    let error = search_sessions(&store, &sandbox.mk, &Selector::default())
        .expect_err("search listing must fail with a short snapshot present");
    assert_names_short(error, &id, "search_sessions");

    // deletion control preview inventory.
    let error =
        deletion_control::occurrence_inventory(&store, &sandbox.mk, "machine-a", "session-a1")
            .expect_err("preview listing must fail with a short snapshot present");
    assert_names_short(error, &id, "occurrence_inventory");

    // store helpers.
    let error = store
        .archived_file_sha256s(&sandbox.mk, &BTreeSet::from(["deadbeef".to_string()]))
        .expect_err("consumed-audit listing must fail with a short snapshot present");
    assert_names_short(error, &id, "archived_file_sha256s");

    let error = store
        .node_metadata_diff(&sandbox.mk)
        .expect_err("node-metadata-diff listing must fail with a short snapshot present");
    assert_names_short(error, &id, "node_metadata_diff");
}

#[test]
fn w1071_doctor_short_files_row_is_silent_when_healthy_and_names_when_short() {
    let sandbox = sandbox_with_repository();
    let repo = sandbox.repo();
    let key = sandbox.key();
    // Point doctor at the test repository through the local-repo config keys
    // the reclaim check already reads.
    let config_path = sandbox
        .dir
        .path()
        .join("config")
        .join("chat-stasher")
        .join("config.toml");
    fs::write(
        &config_path,
        format!(
            "rustic_repo = '{}'\nrustic_key_file = '{}'\nno_cache = true\n",
            repo.display(),
            key.display()
        ),
    )
    .unwrap();

    // Healthy: the row is silent in text and empty-but-checked in JSON.
    let out = sandbox
        .command()
        .arg("doctor")
        .arg("--json")
        .output()
        .expect("run isolated doctor --json");
    assert!(
        out.status.success(),
        "a healthy doctor run exits 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json must emit JSON ({e}): {stdout}"));
    let short = &value["short_files"];
    assert!(
        short.is_object(),
        "doctor --json carries a short_files section: {stdout}"
    );
    assert_eq!(
        short["checked"],
        serde_json::json!(true),
        "a readable repository was checked: {short}"
    );
    assert_eq!(
        short["files"],
        serde_json::json!([]),
        "a healthy repository lists no short files: {short}"
    );
    // Human text is a separate run: --json suppresses it.
    let out = sandbox
        .command()
        .arg("doctor")
        .output()
        .expect("run isolated doctor");
    assert!(
        out.status.success(),
        "a healthy doctor run exits 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Short index"),
        "a healthy doctor prints no short-files row: {stderr}"
    );

    // W1071 review fix (H1): the size listing that could not be taken.
    // Before the fix every failure collapsed to the healthy silence —
    // `{"checked": true, "files": []}` and no terminal row — which words a
    // listing the run could not take as a checked-and-clean one. Each leg
    // breaks one precondition of the listing, and every assertion is on
    // what the shipped CLI prints, because the two surfaces a user parses
    // are the whole point. (The repository-directory-is-a-file leg needs
    // no spawned CLI; doctor.rs pins it as an exact-equality unit test.)
    let config_path = sandbox
        .dir
        .path()
        .join("config")
        .join("chat-stasher")
        .join("config.toml");
    let run_doctor = |config_text: String, what: &str| -> (serde_json::Value, String) {
        fs::write(&config_path, config_text).unwrap();
        let out = sandbox
            .command()
            .arg("doctor")
            .arg("--json")
            .output()
            .unwrap_or_else(|e| panic!("{what}: run isolated doctor --json ({e})"));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let value: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("{what}: doctor --json must emit JSON ({e}): {stdout}"));
        let out = sandbox
            .command()
            .arg("doctor")
            .output()
            .unwrap_or_else(|e| panic!("{what}: run isolated doctor ({e})"));
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        (value, stderr)
    };

    // (1) No repository directory at the configured path.
    let (value, stderr) = run_doctor(
        format!(
            "rustic_repo = '{}'\nrustic_key_file = '{}'\nno_cache = true\n",
            sandbox.dir.path().join("no-such-repo").display(),
            key.display()
        ),
        "no repository directory",
    );
    let short = &value["short_files"];
    assert_eq!(
        short["checked"],
        serde_json::json!(false),
        "a repository directory that is not there is a did-not-check, never a clean listing: {short}"
    );
    assert!(
        short["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "the section carries why it could not check: {short}"
    );
    assert!(
        stderr.contains("Short index/snapshot files") && stderr.contains("no repository directory"),
        "the human table keeps its row and says why, instead of dropping it: {stderr}"
    );

    // (2) Repository directory present, but the masterkey cannot be read.
    let (value, stderr) = run_doctor(
        format!(
            "rustic_repo = '{}'\nrustic_key_file = '{}'\nno_cache = true\n",
            repo.display(),
            sandbox.dir.path().join("no-such.key").display()
        ),
        "no key",
    );
    let short = &value["short_files"];
    assert_eq!(
        short["checked"],
        serde_json::json!(false),
        "a repository that cannot be opened is a did-not-check: {short}"
    );
    let reason = short["reason"].as_str().unwrap_or_default().to_string();
    assert!(
        reason.contains("masterkey"),
        "the reason names the leg that broke: {short}"
    );
    assert!(
        stderr.contains("UNKNOWN") && stderr.contains("unknown, not absent"),
        "the human row words the hole the same way the timed-out family does: {stderr}"
    );

    // A repository path that exists as a file cannot be listed at all; the
    // leg where the CLI cannot reach it is pinned unit-side in doctor.rs
    // (the local backend normalizes even that directory shape away). The
    // third state a size listing cannot fake here — checked-and-clean —
    // is the healthy leg at the end of this test.

    // Back to the real repository for the short-file leg.
    fs::write(
        &config_path,
        format!(
            "rustic_repo = '{}'\nrustic_key_file = '{}'\nno_cache = true\n",
            repo.display(),
            key.display()
        ),
    )
    .unwrap();

    // Short: the same row names the file in both outputs.
    let id = plant_short_snapshot(&repo);
    let out = sandbox
        .command()
        .arg("doctor")
        .arg("--json")
        .output()
        .expect("run isolated doctor --json with a short file");
    assert!(
        out.status.success(),
        "a short-files finding is a completed diagnosis, not an incomplete read: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("doctor --json must emit JSON ({e}): {stdout}"));
    let files = value["short_files"]["files"]
        .as_array()
        .unwrap_or_else(|| panic!("short_files.files must be an array: {value}"));
    assert!(
        files.iter().any(|f| f["id"] == id.as_str()),
        "doctor --json names the short file {id}: {value}"
    );
    let out = sandbox
        .command()
        .arg("doctor")
        .output()
        .expect("run isolated doctor with a short file");
    assert!(
        out.status.success(),
        "a short-files finding is a completed diagnosis, not an incomplete read: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&id),
        "doctor text names the short file {id}: {stderr}"
    );
    assert!(
        stderr.contains("move exactly"),
        "doctor text gives the manual step: {stderr}"
    );
}

/// W1071 review fix (M1): the raw-export readback names a short snapshot
/// with the same recovery words every other list path uses — the `main`-
/// adjacent path the pre-fix test set never drove, which the review called
/// out: no test exercised `read_archived_export` against a short file.
#[test]
fn w1071_raw_export_readback_names_the_short_file() {
    let sandbox = sandbox_with_repository();
    let repo = sandbox.repo();
    let id = plant_short_snapshot(&repo);
    let store = BackupStore::new(store_config(&repo, &sandbox.key()), "machine-a".to_string());
    // The address format is validated before the listing, so a syntactically
    // valid address the archive does not hold still reaches the snapshot
    // listing — which is the leg the short file breaks.
    let address = "ab".repeat(32);
    let error = chat_stasher::raw_export::read_archived_export(
        &store,
        &sandbox.mk,
        "machine-a",
        "deepseek",
        &address,
        &mut std::io::sink(),
    )
    .expect_err("the export readback listing must fail with a short snapshot present");
    assert_names_short(error, &id, "read_archived_export");
}

/// W1071 review fix (M1): `index build` — the FTS leg of `main`'s listing
/// paths — exits 3 (an unfinished read, not a failed-with-index build) and
/// names the short snapshot with the recovery words. Driven through the
/// CLI because the build closure is not separately callable.
#[test]
fn w1071_index_build_names_the_short_file_and_exits_unfinished() {
    let sandbox = sandbox_with_repository();
    let repo = sandbox.repo();
    let config_dir = sandbox.dir.path().join("config").join("chat-stasher");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[destinations.storagebox]\nrepo = '{}'\nkey_file = '{}'\nno_cache = true\n",
            repo.display(),
            sandbox.key().display()
        ),
    )
    .unwrap();
    let id = plant_short_snapshot(&repo);
    let out = sandbox
        .command()
        .args([
            "index",
            "build",
            "--destination",
            "storagebox",
            "--keep-ssh-masters",
        ])
        .output()
        .expect("run isolated index build");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(3),
        "a build that could not list the archive is an unfinished read, never an index over a repository it could not see: {stderr}"
    );
    assert!(
        stderr.contains(&id),
        "index build names the short file {id}: {stderr}"
    );
    assert!(
        stderr.contains("move exactly"),
        "index build gives the manual step: {stderr}"
    );
}
