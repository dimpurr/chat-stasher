//! LM-3 black-box tests (ADR-062 D3): the read side of the local mirror —
//! mirror-first reads, the per-file fallback, the ruling-7 cache rule, and
//! the verify lock.
//!
//! Fixture shape: a real rustic repository pushed from a synthetic stage
//! (two machines, one session each), a full mirror of it made by LM-2's
//! `sync_once`, and then reads through a [`BackupStore`] with the
//! [`chat_stasher::mirror_read`] seam attached — the same store shape
//! `read` / `search` / `export` / `index build` build.
//!
//! The three behaviours the task names, each with its fixture:
//!   * the fallback: `missing_mirror_files_fall_back_to_the_remote` —
//!     the mirror's packs are deleted, reads stay correct and the counters
//!     name the remote fallbacks; and the offline half,
//!     `an_offline_read_serves_from_the_mirror_byte_identically`.
//!   * the ruling-7 cache rule:
//!     `mirror_reads_never_enter_the_body_cache_but_remote_fallbacks_do` —
//!     both halves: mirror-served bytes store nothing, the remote fallback
//!     stores exactly as today's ADR-034 path does.
//!   * the verify lock (ADR-062 D3):
//!     `verify_reads_the_remote_even_when_the_mirror_is_tampered` — the
//!     mirror's pack bytes are garbage, `verify --level l2` still passes
//!     (it never asked the copy), while `read` over the same mirror fails
//!     (it did) — the tamper is what makes the distinguisher real, and the
//!     failing control is what proves this test can fail.
//!
//! Everything is synthetic and local, under temp dirs; the spawned-binary
//! tests redirect the full XDG envelope the way the other suites do (W306).

use chat_stasher::mirror::{self, MirrorSettings, RoundEnd, RoundEnds, SyncRequest};
use chat_stasher::mirror_read::{self, MirrorReadDecision};
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[path = "../src/test_support.rs"]
mod test_support;

// ------------------------------------------------------------- fixtures

fn store_config(repo_path: &Path, key_path: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo_path.to_string_lossy().into_owned(),
        key_file: key_path.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
        // Tests must not share the user's rustic cache (W289): a cached pack
        // would let a missing mirror file still read clean — the exact
        // confusion the fallback test exists to rule out.
        cache_dir: None,
        no_cache: true,
    }
}

/// Write `nshards` sealed shards for one session, the mirror test file's
/// shape so the payloads are identical across suites.
fn write_session(stage: &Path, machine: &str, session: &str, nshards: u64) {
    for seq in 1..=nshards {
        let lines: Vec<String> = (0..10)
            .map(|i| format!("{{\"seq\":{seq},\"i\":{i},\"s\":\"payload-{session}-{seq}-{i}\"}}"))
            .collect();
        store::write_sealed_shard(StageWriter::Collect, stage, machine, session, &lines).unwrap();
    }
}

/// The test's own destination name; the section `[destinations.storagebox]`
/// is what decides() compares, so both use this one.
const MIRROR_OF: &str = "storagebox";

struct NeverStops;
impl RoundEnds for NeverStops {
    fn round_end(&self, _next: &mirror::PlannedFile) -> Option<RoundEnd> {
        None
    }
}

/// One repository + one full mirror of it, with explicit roots: the remote
/// repository, the masterkey, the machine's data root (the mirror lives
/// under it) and the state dir the records land in.
struct Rig {
    dir: tempfile::TempDir,
    remote: StoreConfig,
    mk: MasterKey,
    data_root: PathBuf,
    state_dir: PathBuf,
    settings: MirrorSettings,
}

impl Rig {
    fn mirror_root(&self) -> PathBuf {
        mirror::mirror_dir(&self.data_root, &self.settings.mirror_of)
    }

    fn run_sync(&self) {
        let mirror_root = self.mirror_root();
        let min_free_bytes = self.settings.min_free_bytes(&mirror_root);
        mirror::sync_once(&SyncRequest {
            settings: &self.settings,
            remote: &self.remote,
            data_root: &self.data_root,
            state_dir: &self.state_dir,
            master_key: &self.mk,
            now_unix: 1_800_000_000,
            min_free_bytes,
            round_ends: &NeverStops,
        })
        .expect("the fixture's sync round completes");
    }

    /// The same `decide` the CLI runs, against a config that mirrors the
    /// fixture's destination — the lib-side equivalent of the section the
    /// spawned-binary tests write.
    fn serving_reads(&self) -> std::sync::Arc<chat_stasher::mirror_read::MirrorReads> {
        let mut config = chat_stasher::config::Config::default();
        config.destinations.insert(
            MIRROR_OF.to_string(),
            chat_stasher::config::DestinationConfig {
                repo: Some(self.remote.repo_root.clone()),
                ..Default::default()
            },
        );
        config.mirror = Some(chat_stasher::config::MirrorSectionConfig {
            enabled: Some(true),
            mirror_of: Some(MIRROR_OF.to_string()),
            min_free_gib: None,
        });
        match mirror_read::decide(&config, &self.remote, &self.data_root) {
            MirrorReadDecision::Serving(reads) => reads,
            other => panic!("the fixture's synced mirror must serve, got {other:?}"),
        }
    }

    /// Remove every pack file from the mirror (the "mirror lacks this file"
    /// shape: config, keys, index and snapshots stay, so opens still work
    /// and only bodies and trees must fall back).
    fn strip_packs_from_mirror(&self) {
        let data = self.mirror_root().join("data");
        for entry in fs::read_dir(&data).unwrap() {
            let shard_dir = entry.unwrap().path();
            for file in fs::read_dir(&shard_dir).unwrap() {
                fs::remove_file(file.unwrap().path()).unwrap();
            }
        }
    }
}

/// Two machines, one session each, pushed; no mirror yet.
fn build_rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("remote-repo");
    let key = dir.path().join("masterkey-storagebox.json");
    let remote = store_config(&repo, &key);

    let stage_a = dir.path().join("stage-a");
    let stage_b = dir.path().join("stage-b");
    write_session(&stage_a, "machine-a", "session-a1", 2);
    write_session(&stage_b, "machine-b", "session-b1", 3);

    let mk = MasterKey::new();
    store::persist_key_file(&remote, &mk).unwrap();
    BackupStore::new(remote.clone(), "machine-a".to_string())
        .push(&stage_a, &mk)
        .unwrap();
    BackupStore::new(remote.clone(), "machine-b".to_string())
        .push(&stage_b, &mk)
        .unwrap();

    Rig {
        data_root: dir.path().join("data"),
        state_dir: dir.path().join("state"),
        dir,
        remote,
        mk,
        settings: MirrorSettings {
            enabled: true,
            mirror_of: MIRROR_OF.to_string(),
            min_free_gib: Some(0.0),
        },
    }
}

// ---------------------------------------------------- reads through the seam

/// What the remote alone answers, for byte-identical comparison: the
/// canonical read with no mirror attached — the pre-LM-3 store shape, so
/// the two reads differ by nothing but the seam.
fn canonical_remote_bytes(rig: &Rig) -> (Vec<u8>, Vec<(String, String)>) {
    let store = BackupStore::new(rig.remote.clone(), "machine-a".to_string());
    store
        .read_session_concat("machine-a", "session-a1", &rig.mk)
        .expect("the fixture's session reads through the remote")
}

/// Mirror-first means the read survives the remote disappearing: every file
/// it needs is in the copy, and the listings the open walks fall back with
/// their counter speaking. This is 47 §3.1's offline contract at store level
/// — the byte-identical answer over the copy, not a partial one.
#[test]
fn an_offline_read_serves_from_the_mirror_byte_identically() {
    let rig = build_rig();
    rig.run_sync();
    let (remote_bytes, remote_hashes) = canonical_remote_bytes(&rig);

    // Online, seam attached: the mirror serves, same bytes.
    let reads = rig.serving_reads();
    let store = BackupStore::new(rig.remote.clone(), "machine-a".to_string())
        .with_mirror_reads(Some(std::sync::Arc::clone(&reads)));
    let (bytes, hashes) = store
        .read_session_concat("machine-a", "session-a1", &rig.mk)
        .expect("the mirrored read completes online");
    assert_eq!(bytes, remote_bytes);
    assert_eq!(hashes, remote_hashes);
    assert!(
        reads.counters().mirror_reads > 0,
        "the local copy served this read: {:?}",
        reads.counters()
    );

    // The remote goes away entirely: the read must not notice.
    let gone = rig.dir.path().join("remote-repo-removed");
    fs::rename(rig.dir.path().join("remote-repo"), &gone).unwrap();
    let reads = rig.serving_reads();
    let store = BackupStore::new(rig.remote.clone(), "machine-a".to_string())
        .with_mirror_reads(Some(std::sync::Arc::clone(&reads)));
    let (bytes, hashes) = store
        .read_session_concat("machine-a", "session-a1", &rig.mk)
        .expect("an offline read serves from the local mirror (47 §3.1)");
    assert_eq!(bytes, remote_bytes, "byte-identical, whatever served");
    assert_eq!(hashes, remote_hashes);
    let counters = reads.counters();
    assert!(
        counters.mirror_listings > 0,
        "the open's listings fell back to the mirror: {counters:?}"
    );
    assert_eq!(
        counters.remote_reads, 0,
        "nothing could have come from the removed remote: {counters:?}"
    );
}

fn serving_counters(store: &BackupStore) -> chat_stasher::mirror_read::MirrorCounters {
    store
        .mirror_reads()
        .expect("the store under test carries the seam")
        .counters()
}

/// The fallback the ADR words as an ordinary read (47 §3.6): the mirror
/// holds the metadata but not the packs, so every pack read crosses to the
/// remote — and the answer is still byte-identical, with the counters
/// saying which legs moved where.
#[test]
fn missing_mirror_files_fall_back_to_the_remote() {
    let rig = build_rig();
    rig.run_sync();
    let (remote_bytes, _hashes) = canonical_remote_bytes(&rig);
    rig.strip_packs_from_mirror();

    let reads = rig.serving_reads();
    let store = BackupStore::new(rig.remote.clone(), "machine-a".to_string())
        .with_mirror_reads(Some(reads));
    let (bytes, _hashes) = store
        .read_session_concat("machine-a", "session-a1", &rig.mk)
        .expect("the missing files read from the remote, an ordinary read");
    assert_eq!(bytes, remote_bytes, "the fallback changes no bytes");
    let counters = serving_counters(&store);
    assert!(
        counters.remote_reads > 0,
        "the packs came from the remote: {counters:?}"
    );
    assert!(
        counters.mirror_reads > 0,
        "the immutable metadata still served from the mirror: {counters:?}"
    );
    assert_eq!(
        counters.mirror_listings, 0,
        "the remote was listable — presence stayed the remote's to say"
    );
}

/// Ruling 7, both halves (ADR-062 D3): mirror-served body reads store
/// nothing — the same ciphertext must never live twice on this machine —
/// while the remote fallback still fills the cache exactly as ADR-034's
/// path always did (ruling 7's other half: today's path, unchanged).
#[test]
fn mirror_reads_never_enter_the_body_cache_but_remote_fallbacks_do() {
    let rig = build_rig();
    rig.run_sync();

    // First half: a full mirror serves; the cache stays empty.
    let cache_root = rig.dir.path().join("body-cache");
    let cache = std::sync::Arc::new(chat_stasher::body_cache::BodyCache::new(
        cache_root.clone(),
        64 * 1024 * 1024,
    ));
    let reads = rig.serving_reads();
    let store = BackupStore::new(rig.remote.clone(), "machine-a".to_string())
        .with_body_cache(Some(std::sync::Arc::clone(&cache)))
        .with_mirror_reads(Some(reads));
    let (mirror_bytes, _hashes) = store
        .read_session_concat("machine-a", "session-a1", &rig.mk)
        .expect("the mirrored session reads");
    assert_eq!(
        cache.stats().stored,
        0,
        "mirror reads never store (ruling 7)"
    );
    let usage = cache.usage().unwrap();
    assert_eq!(usage.entries, 0, "no entry files either: {usage:?}");
    assert!(
        serving_counters(&store).mirror_reads > 0,
        "the mirror really served this read"
    );

    // Second half: the packs leave the mirror; the remote fallback serves
    // the same bytes and *does* fill the cache — today's path, unchanged.
    rig.strip_packs_from_mirror();
    let reads_fallback = rig.serving_reads();
    let store = BackupStore::new(rig.remote.clone(), "machine-a".to_string())
        .with_body_cache(Some(std::sync::Arc::clone(&cache)))
        .with_mirror_reads(Some(reads_fallback));
    let (fallback_bytes, _hashes) = store
        .read_session_concat("machine-a", "session-a1", &rig.mk)
        .expect("the fallback read completes");
    assert_eq!(fallback_bytes, mirror_bytes, "same bytes, different legs");
    assert!(
        cache.stats().stored > 0,
        "the remote fallback fills the cache (ruling 7's other half)"
    );
    assert_eq!(cache.stats().errors, 0, "the cache stays healthy");
}

// ------------------------------------------------------------ CLI contract

/// A `chat-stasher` binary run with the full isolation envelope.
fn binary_command(sandbox: &Path) -> std::process::Command {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
        .env("XDG_CONFIG_HOME", sandbox.join("config"))
        .env("XDG_DATA_HOME", sandbox.join("data"))
        .env("XDG_STATE_HOME", sandbox.join("state"))
        .env("XDG_CACHE_HOME", sandbox.join("cache"))
        .env_remove("CODEX_HOME")
        .env_remove("RUSTIC_REPO")
        .env_remove("RUSTIC_KEY_FILE");
    command
}

fn write_config(sandbox: &Path, mirror_block: &str) {
    let config_dir = sandbox.join("config").join("chat-stasher");
    fs::create_dir_all(&config_dir).unwrap();
    let repo = sandbox.join("remote-repo");
    let key = sandbox.join("remote.key");
    fs::write(
        config_dir.join("config.toml"),
        format!(
            "[destinations.storagebox]\nrepo = \"{}\"\nkey_file = \"{}\"\nno_cache = \
             true\n\n[mirror]\n{mirror_block}",
            repo.display(),
            key.display()
        ),
    )
    .unwrap();
}

/// Push a two-machine synthetic repository and fill the mirror into exactly
/// the data root the spawned binary will resolve (its XDG_DATA_HOME), so the
/// CLI's `decide` finds the copy the fixture made.
fn cli_fixture_with_mirror(sandbox: &Path) {
    let stage_a = sandbox.join("stage-a");
    let stage_b = sandbox.join("stage-b");
    write_session(&stage_a, "machine-a", "session-a1", 2);
    write_session(&stage_b, "machine-b", "session-b1", 2);
    let cfg = store_config(&sandbox.join("remote-repo"), &sandbox.join("remote.key"));
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    for (stage, machine) in [(&stage_a, "machine-a"), (&stage_b, "machine-b")] {
        BackupStore::new(cfg.clone(), machine.to_string())
            .push(stage, &mk)
            .unwrap();
    }
    let data_root = sandbox.join("data").join("chat-stasher");
    let state_dir = sandbox.join("state").join("chat-stasher");
    let settings = MirrorSettings {
        enabled: true,
        mirror_of: MIRROR_OF.to_string(),
        min_free_gib: Some(0.0),
    };
    let min_free_bytes = settings.min_free_bytes(&mirror::mirror_dir(&data_root, MIRROR_OF));
    mirror::sync_once(&SyncRequest {
        settings: &settings,
        remote: &cfg,
        data_root: &data_root,
        state_dir: &state_dir,
        master_key: &mk,
        now_unix: 1_800_000_000,
        min_free_bytes,
        round_ends: &NeverStops,
    })
    .expect("the fixture's first fill completes");
}

fn mirror_root_of(sandbox: &Path) -> PathBuf {
    sandbox
        .join("data")
        .join("chat-stasher")
        .join("mirror")
        .join(MIRROR_OF)
}

fn run(command: &mut std::process::Command) -> (i32, String, String) {
    let out = command.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The verify lock (ADR-062 D3: `verify` never reads the mirror — a copy
/// may never certify itself). The mirror's pack bytes are replaced with
/// garbage of the same length, which is the distinguisher:
///
/// * `verify --level l2` reads the remote (untampered) and passes — over a
///   mirror it did not consult. If `verify` ever grows a mirror seam, its
///   re-hash of the garbage fails the level and this test goes red with
///   exit 1.
/// * `read`, the mirror-first reader, fails over the same mirror — the
///   control that proves the tamper is load-bearing and the seam is live.
#[test]
fn verify_reads_the_remote_even_when_the_mirror_is_tampered() {
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();
    cli_fixture_with_mirror(sandbox);
    write_config(
        sandbox,
        "enabled = true\nmirror_of = \"storagebox\"\nmin_free_gib = 1.0",
    );

    // Tamper every pack file in the mirror, keeping the byte length so no
    // size-based refusal can fire early and pretend the answer came from
    // anywhere but the corrupted bytes themselves.
    let data = mirror_root_of(sandbox).join("data");
    for entry in fs::read_dir(&data).unwrap() {
        let shard_dir = entry.unwrap().path();
        for file in fs::read_dir(&shard_dir).unwrap() {
            let path = file.unwrap().path();
            let len = fs::metadata(&path).unwrap().len() as usize;
            fs::write(&path, vec![0xaau8; len]).unwrap();
        }
    }

    // verify: remote-only, so the intact remote passes. The machine filter
    // is the verify command's own conventional spelling over a repo whose
    // two partitions both hold checks to make.
    let (code, stdout, stderr) = run(binary_command(sandbox).args([
        "verify",
        "--level",
        "l2",
        "--destination",
        MIRROR_OF,
        "--machine",
        "machine-a",
    ]));
    assert_eq!(
        code, 0,
        "verify answers the remote even with a tampered mirror:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("[verify] RESULT         : OK"),
        "the verdict is the remote's own: {stdout}"
    );

    // read: mirror-first, so the corrupted copy fails the read — the
    // counterfactual that makes the lock meaningful. Any exit here
    // that claims success would mean the read somehow avoided the
    // mirror, and the tamper would no longer test anything.
    let (code, stdout, stderr) = run(binary_command(sandbox).args([
        "read",
        "--destination",
        MIRROR_OF,
        "--machine",
        "machine-a",
        "--session",
        "session-a1",
    ]));
    assert_ne!(
        code, 0,
        "the mirror-first read must trip over the tampered copy:\n{stdout}\n{stderr}"
    );
    assert!(
        !stdout.contains("[read] concat len"),
        "no completed read may be reported over corrupted bytes: {stdout}"
    );
}

/// The visible-status contract (ADR-062 D3: a mirror that cannot serve
/// falls back to the remote and says so): a
/// serving read announces the seam and what served after it, and a never
/// synced mirror falls back to the remote with the reason on the report.
#[test]
fn read_states_the_mirror_it_reads_through() {
    // Serving: the line names the destination, and the served line counts.
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();
    cli_fixture_with_mirror(sandbox);
    write_config(
        sandbox,
        "enabled = true\nmirror_of = \"storagebox\"\nmin_free_gib = 1.0",
    );
    let (code, stdout, stderr) = run(binary_command(sandbox).args([
        "read",
        "--destination",
        MIRROR_OF,
        "--machine",
        "machine-a",
        "--session",
        "session-a1",
    ]));
    assert_eq!(code, 0, "a mirrored read succeeds:\n{stdout}\n{stderr}");
    assert!(
        stdout.contains("[read] mirror        : first `storagebox`"),
        "the serving line names the seam: {stdout}"
    );
    assert!(
        stdout.contains("[read] mirror served  :"),
        "the served line reports the counts: {stdout}"
    );

    // Enabled but never synced: every read falls back to the remote, the
    // line says why, and the read still succeeds (47 §3.6).
    let sb = tempfile::TempDir::new().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage-a");
    write_session(&stage, "machine-a", "session-a1", 2);
    let cfg = store_config(&sandbox.join("remote-repo"), &sandbox.join("remote.key"));
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    BackupStore::new(cfg, "machine-a".to_string())
        .push(&stage, &mk)
        .unwrap();
    write_config(sandbox, "enabled = true\nmirror_of = \"storagebox\"");
    let (code, stdout, stderr) = run(binary_command(sandbox).args([
        "read",
        "--destination",
        MIRROR_OF,
        "--machine",
        "machine-a",
        "--session",
        "session-a1",
    ]));
    assert_eq!(code, 0, "the fallback read succeeds:\n{stdout}\n{stderr}");
    let line = stdout
        .lines()
        .find(|l| l.starts_with("[read] mirror "))
        .unwrap_or_else(|| panic!("the unavailable mirror states itself: {stdout}"));
    assert!(
        line.contains("unavailable"),
        "the state is named as unavailable: {line}"
    );
    assert!(
        line.contains("mirror sync"),
        "the reason names the fix: {line}"
    );
    assert!(
        line.contains("reads the remote"),
        "the sentence says where the read went: {line}"
    );
}
