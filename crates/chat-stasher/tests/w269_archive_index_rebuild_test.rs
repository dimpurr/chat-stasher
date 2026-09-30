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
///
/// `%LOCALAPPDATA%` is set beside `HOME` for the reason `w267` spells out: on
/// Windows the cache root is not a child of `$HOME` at all, so a child given
/// only `HOME` writes the real user's cache while every path it prints looks
/// sandboxed. This suite is the one that makes the difference visible, because
/// the read-only rebuild publishes its **derived index** into exactly that
/// cache: with the root left ambient, the two read-only tests below resolve one
/// path outside both of their sandboxes and race to replace it, and the loser's
/// `tempfile::persist` returns `Access is denied` — which is what made this
/// suite red on `windows-latest` while every other platform stayed green.
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
        .env("LOCALAPPDATA", home.join("AppData").join("Local"))
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

/// The `config.toml` a sandbox's child loads: the machine that owns this
/// sandbox, and one `fixture` destination.
///
/// Every value goes in a TOML **literal** string (`'…'`), not a basic one. A
/// basic string treats `\` as its escape character, so the `"C:\…"` a Windows
/// `Path::display()` produces is not valid TOML at all: it parses only because
/// the loader recovers it and prints an unescaped-backslash warning. A fixture
/// must not lean on the recovery path — it would make the Windows run load a
/// config by a route the unix run never takes, and it would put a warning on
/// the stderr of every Windows test that reads it.
fn write_sandbox_config(sandbox: &Path, machine: &str, repo: &Path, key: &Path) {
    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "machine = '{machine}'\n\n[destinations.fixture]\nrepo = '{}'\nkey_file = '{}'\n",
            repo.display(),
            key.display()
        ),
    )
    .unwrap();
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

/// A content digest of every file under `root`, path and bytes both.
///
/// The read-only contract is "the destination did not change", and a snapshot
/// count is a proxy for it: a rebuild that rewrote a snapshot's metadata, or
/// left an index file behind in the repository, would keep the count and break
/// the contract. This makes the claim measurable on the fixture.
fn tree_digest(root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                files.insert(rel, fs::read(&path).unwrap());
            }
        }
    }
    let mut hasher = Sha256::new();
    for (rel, bytes) in files {
        hasher.update(rel.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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

/// The derived index a read-only rebuild reports must be written **inside** the
/// sandbox it was given.
///
/// The file is a cache under the platform cache root, and that root is not
/// derived from `$HOME` on every platform: on Windows it is `%LOCALAPPDATA%`, so
/// a child handed `HOME` alone publishes to the real user's cache and every path
/// the run prints still looks sandboxed. Asserting containment is what turns
/// that escape into this suite's own failure instead of a driver-side accident —
/// with two tests resolving the one ambient path, the race for it is what
/// actually surfaced on `windows-latest`, and a run that lost no race would have
/// stayed green while reading and writing the runner's real cache.
fn assert_derived_is_sandboxed(sandbox: &Path, derived: &str) {
    assert!(
        Path::new(derived).starts_with(sandbox),
        "the derived index must be written inside the sandbox, not to the real \
         user's cache: {derived}"
    );
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
    let repo_before = tree_digest(&repo);

    // A config naming THIS machine's partition, so the rebuild of `lost` is
    // another machine's partition and must take the read-only path.
    write_sandbox_config(sandbox, here, &repo, &key);

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
    assert_derived_is_sandboxed(sandbox, &derived);
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
    assert_eq!(
        tree_digest(&repo),
        repo_before,
        "a read-only rebuild must not change one byte of the destination"
    );
}

/// The fixture for the *current* machine's side of the same question: this
/// sandbox's config names `mbp-here`, the archive holds `mbp-here`'s sessions,
/// and the stage that machine would have is the sandbox's own `stage` path —
/// which the caller either deletes or never has.
///
/// Returns `(repo, key, session id)`; the masterkey is read back from the key
/// file so a test can count the repository's snapshots.
fn own_machine_archive(sandbox: &Path) -> (PathBuf, PathBuf, String) {
    let machine = "mbp-here";
    let session = "claude-code.mbp-here.019bf00d-97b6-7eb2-9bf8-eacbacc09765".to_string();
    let stage = sandbox.join("stage");
    let repo = sandbox.join("repo");
    let key = sandbox.join("keys").join("masterkey.json");
    let cfg = store_config(&repo, &key);
    let mk = MasterKey::new();
    chat_stasher::store::persist_key_file(&cfg, &mk).unwrap();

    write_shard(
        &stage,
        machine,
        &session,
        &[
            cc_line("2025-01-15T12:34:56Z"),
            cc_line("2025-01-15T13:45:07Z"),
        ],
    );
    BackupStore::new(cfg.clone(), machine.to_string())
        .push(&stage, &mk)
        .unwrap();

    write_sandbox_config(sandbox, machine, &repo, &key);
    (repo, key, session)
}

/// SRCH-2 review, High 2: a machine whose stage is gone must be able to rebuild
/// **its own** index from the archive. The publishing repair restores the shards
/// into `--stage` first, so with no `--stage` at all there is nothing to restore
/// into — and the rebuild must then be the read-only one rather than a usage
/// error, because for this machine the archive is the only copy left.
#[test]
fn the_current_machine_with_no_stage_rebuilds_read_only() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let (repo, key, session) = own_machine_archive(sandbox);
    let cfg = store_config(&repo, &key);
    let mk = chat_stasher::store::load_key_file(&cfg).unwrap();

    let before = snapshots_per_host(&cfg, &mk);
    assert_eq!(before.get("mbp-here"), Some(&1), "one snapshot to read");
    let repo_before = tree_digest(&repo);

    let out = run(
        sandbox,
        &[
            "activity-index",
            "--rebuild",
            "--destination",
            "fixture",
            "--machine",
            "mbp-here",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "the own-machine rebuild with no stage should exit 0, not refuse:\n{stdout}\n{stderr}"
    );

    // The real conversation span, derived from the archive and published to a
    // local derived index rather than into the machine's partition.
    let derived = reported(&stdout, "derived").expect("stdout names the derived index");
    assert_derived_is_sandboxed(sandbox, &derived);
    assert!(
        Path::new(&derived).is_file(),
        "the derived index must exist at {derived}"
    );
    let rows = index_rows(Path::new(&derived));
    let row = rows
        .get(&session)
        .unwrap_or_else(|| panic!("the session must have a row:\n{stdout}"));
    assert_eq!(row["line_count"].as_u64(), Some(2), "row: {row}");
    assert_eq!(row["first_unix"].as_i64(), Some(1736944496), "row: {row}");

    // And nothing was appended: this machine's partition is untouched.
    assert_eq!(
        snapshots_per_host(&cfg, &mk),
        before,
        "a read-only rebuild must not append a snapshot"
    );
    assert_eq!(
        tree_digest(&repo),
        repo_before,
        "a read-only rebuild must not change one byte of the destination"
    );
}

/// The same contract for a `--stage` that is named but is not a directory:
/// there is nowhere to restore the shards into, so the rebuild is read-only
/// rather than a refusal — and it says so, because an operator who expected a
/// repaired snapshot needs to know why none was appended.
#[test]
fn the_current_machine_with_a_missing_stage_rebuilds_read_only() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let (repo, key, session) = own_machine_archive(sandbox);
    let cfg = store_config(&repo, &key);
    let mk = chat_stasher::store::load_key_file(&cfg).unwrap();
    let before = snapshots_per_host(&cfg, &mk);
    let repo_before = tree_digest(&repo);

    let gone = sandbox.join("stage-gone");
    assert!(!gone.exists(), "the fixture must not have this directory");
    let out = run(
        sandbox,
        &[
            "activity-index",
            "--rebuild",
            "--destination",
            "fixture",
            "--machine",
            "mbp-here",
            "--stage",
            gone.to_str().unwrap(),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "a missing stage directory must not refuse the rebuild:\n{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains("is not a directory"),
        "the run must say the stage was unusable:\n{stderr}"
    );

    let derived = reported(&stdout, "derived").expect("stdout names the derived index");
    assert_derived_is_sandboxed(sandbox, &derived);
    let rows = index_rows(Path::new(&derived));
    let row = rows
        .get(&session)
        .unwrap_or_else(|| panic!("the session must have a row:\n{stdout}"));
    assert_eq!(row["first_unix"].as_i64(), Some(1736944496), "row: {row}");
    assert_eq!(
        snapshots_per_host(&cfg, &mk),
        before,
        "a read-only rebuild must not append a snapshot"
    );
    assert_eq!(
        tree_digest(&repo),
        repo_before,
        "a read-only rebuild must not change one byte of the destination"
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

    write_sandbox_config(sandbox, "mbp-here", &repo, &key);

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
