//! W269 / SRCH-2 — a machine's activity index must be rebuildable from the
//! archive **alone**, and the rebuild must not write into that machine's
//! partition.
//!
//! The field failure this covers: `reclaim-stage` deletes a session's shard
//! bodies from the stage once every declared destination has proved it holds
//! every byte, so a stage rebuild over the survivors measured empty directories
//! and rewrote known rows as zero lines with an unknown time. The old repair
//! (`activity-index --rebuild --destination …`) walked the archive but then
//! **appended a snapshot** to the named machine's partition — a partition the
//! machine that owns it writes alone (ADR-017), and one whose owner may be gone
//! for good. That is the command this suite holds to a read-only contract: it
//! needs no source machine, no stage and no copy of the bodies, and the
//! destination it reads must come out byte-identical.
//!
//! Nothing here is a live machine: the archive is built with
//! `BackupStore::push` directly, exactly as the W158 suite does.

use chat_stasher::store::{BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use rustic_core::{Credentials, Repository};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run the real binary with every ambient path redirected into `sandbox`.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W269 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("cache"))
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

fn store_config(repo: &Path, key: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: None,
        no_cache: false,
    }
}

/// How many snapshots the repository holds per hostname.
fn snapshots_per_host(cfg: &StoreConfig, mk: &MasterKey) -> BTreeMap<String, usize> {
    let backends = BackupStore::for_metadata_query(cfg.clone())
        .backends()
        .unwrap();
    let repo = Repository::new(&cfg.repository_options(), &backends)
        .unwrap()
        .open(&Credentials::Masterkey(mk.clone()))
        .unwrap()
        .to_indexed()
        .unwrap();
    let mut counts = BTreeMap::new();
    for snapshot in repo.get_all_snapshots().unwrap() {
        *counts.entry(snapshot.hostname.clone()).or_insert(0) += 1;
    }
    counts
}

/// The value of one `[activity-index] <label> : <value>` line.
fn reported(stdout: &str, label: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        let rest = line.strip_prefix(&format!("[activity-index] {label}"))?;
        Some(rest.trim_start_matches([' ', ':']).trim().to_string())
    })
}

/// Every row of a JSONL activity index, keyed by session id.
fn index_rows(path: &Path) -> BTreeMap<String, serde_json::Value> {
    let raw = fs::read_to_string(path).expect("the derived index must be readable");
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let row: serde_json::Value =
                serde_json::from_str(line).expect("every index line is one JSON row");
            (row["session_id"].as_str().unwrap().to_string(), row)
        })
        .collect()
}

/// The whole point of the item: m3 is gone, and its index is recoverable from
/// the archive next to it — with the times it actually had, and without the
/// destination gaining a byte.
#[test]
fn a_lost_machines_index_rebuilds_from_the_archive_and_leaves_the_destination_alone() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    // The machine whose index is being recovered, and the one doing the
    // recovering (this sandbox's config names it, so `--machine mac` is
    // unambiguously another machine's partition).
    let lost = "mac";
    let here = "mbp-here";
    let stage = sandbox.join("stage");
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let cfg = store_config(&repo, &key);
    let mk = MasterKey::new();
    chat_stasher::store::persist_key_file(&cfg, &mk).unwrap();

    let oldest = "claude-code.mac.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
    let reclaimed = "claude-code.mac.019bf00d-97b6-7eb2-9bf8-eacbacc09766";
    write_shard(
        &stage,
        lost,
        oldest,
        &[
            cc_line("2025-01-15T12:34:56Z"),
            cc_line("2025-01-15T13:45:07Z"),
        ],
    );
    write_shard(&stage, lost, reclaimed, &[cc_line("2025-03-02T08:00:00Z")]);
    BackupStore::new(cfg.clone(), lost.to_string())
        .push(&stage, &mk)
        .unwrap();

    // The field shape: the body is reclaimed from the stage once the archive
    // has proved it holds it, and the next push therefore carries only the
    // session directory and its `shard-seq` counter. `reclaimed`'s bytes stay
    // in the FIRST snapshot; the newest snapshot holds no shard for it at all.
    chat_stasher::stagereclaim::reclaim_session_body(&stage, lost, reclaimed).unwrap();
    BackupStore::new(cfg.clone(), lost.to_string())
        .push(&stage, &mk)
        .unwrap();
    let before = snapshots_per_host(&cfg, &mk);
    assert_eq!(
        before.get(lost),
        Some(&2),
        "two snapshots for the lost machine"
    );

    // A config naming THIS machine's partition, so the rebuild of `lost` is
    // another machine's partition and must take the read-only path.
    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "machine = \"{here}\"\n\n[destinations.fixture]\nrepo = \"{}\"\nkey_file = \"{}\"\n",
            repo.display(),
            key.display()
        ),
    )
    .unwrap();

    // No `--stage`: the whole point is that the source machine's stage is not
    // needed, and neither is any other.
    let out = run(
        sandbox,
        &[
            "activity-index",
            "--rebuild",
            "--destination",
            "fixture",
            "--machine",
            lost,
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "rebuild should exit 0:\n{stdout}\n{stderr}"
    );

    let derived = reported(&stdout, "derived").expect("stdout names the derived index");
    let derived = PathBuf::from(derived);
    let rows = index_rows(&derived);

    let row = rows
        .get(reclaimed)
        .unwrap_or_else(|| panic!("the reclaimed session must have a row:\n{stdout}"));
    assert_eq!(
        row["line_count"].as_u64(),
        Some(1),
        "the archived body must be measured, not its missing stage copy: {row}"
    );
    assert_eq!(
        row["first_unix"].as_i64(),
        Some(1740902400),
        "2025-03-02T08:00:00Z must come back as the session's real time: {row}"
    );
    assert_eq!(row["last_unix"].as_i64(), Some(1740902400));

    // The session whose body only the OLDER snapshot holds — the walk is
    // cumulative, so it is measured too rather than read as empty.
    let row = rows
        .get(oldest)
        .unwrap_or_else(|| panic!("the older-snapshot session must have a row:\n{stdout}"));
    assert_eq!(row["line_count"].as_u64(), Some(2), "row: {row}");
    assert_eq!(row["first_unix"].as_i64(), Some(1736944496));
    assert_eq!(row["last_unix"].as_i64(), Some(1736948707));

    // Read-only: the destination gained nothing.
    let after = snapshots_per_host(&cfg, &mk);
    assert_eq!(
        after, before,
        "a read-only rebuild must not append a snapshot"
    );
}

/// The other half of the same contract: the rebuild is refused, loudly, when
/// the walk cannot finish — an unreadable snapshot must not render as a
/// complete index over fewer sessions.
#[test]
fn a_machine_the_destination_does_not_hold_is_an_error_not_an_empty_index() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let cfg = store_config(&repo, &key);
    let mk = MasterKey::new();
    chat_stasher::store::persist_key_file(&cfg, &mk).unwrap();
    write_shard(
        &stage,
        "mbp-here",
        "claude-code.mbp-here.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
        &[cc_line("2025-01-15T12:34:56Z")],
    );
    BackupStore::new(cfg.clone(), "mbp-here".to_string())
        .push(&stage, &mk)
        .unwrap();

    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "machine = \"mbp-here\"\n\n[destinations.fixture]\nrepo = \"{}\"\nkey_file = \"{}\"\n",
            repo.display(),
            key.display()
        ),
    )
    .unwrap();

    let out = run(
        sandbox,
        &[
            "activity-index",
            "--rebuild",
            "--destination",
            "fixture",
            "--machine",
            "a-machine-that-never-pushed",
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(3),
        "an unwalkable machine is 'did not finish', not an empty index:\n{stderr}"
    );
    assert!(
        !out.stdout.is_empty() || !stderr.is_empty(),
        "the refusal must say something"
    );
}
