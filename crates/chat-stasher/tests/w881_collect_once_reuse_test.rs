//! W881 — collect once, reuse in push (SYNC-B).
//!
//! A run-once pass used to scan the registry twice — once in `collect`, once
//! in the push empty-stage guard — and count the sealed stage three times:
//! the run-once audit, the push guard, and the backup's own setup. The pass
//! now carries collection's validated scan evidence and its sealed-shard count
//! into the push path, bound to the stage tree by a freshness token, so a push
//! that finds the tree changed recounts rather than trusting a count taken
//! before files were written.
//!
//! These tests drive the pass's library sequence — `collect`, the stage
//! audit, the push guard, and `BackupStore::push_with_count` — against
//! synthetic registries and temp stages, and assert the reuse contract
//! through the process-wide call counters: a quiet or changed pass performs
//! exactly one scan and one count, a shard written between collect and push
//! forces a recount and is still archived, and an unreadable stage root
//! still refuses the guard — unreadable stays distinct from empty.

use chat_stasher::collect;
use chat_stasher::collect::DestinationView;
use chat_stasher::config::Config;
use chat_stasher::scanner;
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use serde_json::json;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// Env mutation is process-global and cargo runs tests in parallel threads;
/// the call counters are process-wide too. Every test in this file takes the
/// lock, so the registry file and the counters are only ever touched by one
/// test at a time.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const MACHINE: &str = "fixture-machine";

/// Point [`scanner::REGISTRY_ENV`] at the fixture registry for the duration
/// of one test, and put the previous value back on drop — panic included.
/// Field order is the design: `_env` drops (restoring the environment) while
/// `_lock` is still held.
struct EnvGuard {
    _env: Vec<(&'static str, Option<OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn acquire(registry: &Path) -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os(scanner::REGISTRY_ENV);
        std::env::set_var(scanner::REGISTRY_ENV, registry);
        Self {
            _env: vec![(scanner::REGISTRY_ENV, previous)],
            _lock: lock,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, previous) in self._env.drain(..) {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// A one-harness registry: the scan must never reach a real harness
/// directory, whatever HOME the test process carries.
fn write_registry(dir: &Path) -> PathBuf {
    let cell = json!({
        "template": "~/.claude/projects",
        "format": "jsonl",
        "confidence": "source-confirmed",
        "source": "W881 synthetic fixture"
    });
    let paths = match scanner::current_platform() {
        "macos" => json!({"macos": cell}),
        "linux" => json!({"linux": cell}),
        "windows" => json!({"windows": cell}),
        platform => panic!("unexpected platform: {platform}"),
    };
    let registry = dir.join("registry.json");
    fs::write(
        &registry,
        serde_json::to_string(&json!({
            "schema_version": 1,
            "generated": "W881 synthetic",
            "harnesses": [{
                "id": "claude-code",
                "display_name": "synthetic",
                "paths": paths
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    registry
}

struct Fixture {
    _dir: tempfile::TempDir,
    registry: PathBuf,
    source_root: PathBuf,
    stage: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let registry = write_registry(dir.path());
        let source_root = dir.path().join("source");
        fs::create_dir_all(&source_root).unwrap();
        fs::write(source_root.join("session.jsonl"), b"one\ntwo\n").unwrap();
        let stage = dir.path().join("stage");
        let state = dir.path().join("state");
        Self {
            _dir: dir,
            registry,
            source_root,
            stage,
            state,
        }
    }

    fn root(&self) -> &Path {
        self._dir.path()
    }

    fn config(&self) -> Config {
        Config {
            claude_projects_dir: Some(self.source_root.to_string_lossy().into_owned()),
            ..Config::default()
        }
    }
}

fn dest<'a>() -> DestinationView<'a> {
    DestinationView::unreachable("fixture-destination")
}

/// The run-once pass's library sequence: collect, then the stage audit, then
/// the push guard built from the evidence collection already produced —
/// the shape `run_once_pass` feeds `cmd_push` on a push path.
fn pass_sequence(
    fx: &Fixture,
) -> (
    collect::CollectReport,
    store::CountedStageShards,
    collect::PushStageCheck,
) {
    let report =
        collect::collect(&fx.config(), &fx.stage, MACHINE, &fx.state, 20, &dest()).unwrap();
    let counted = store::CountedStageShards::count(&fx.stage).unwrap();
    let check =
        collect::inspect_stage_for_push_with_evidence(&report.scan, &fx.stage, &fx.state, &counted)
            .unwrap();
    (report, counted, check)
}

/// A destination rooted inside the fixture: the cache moves with it (W289),
/// so opening the repository never touches the real user cache.
fn cfg_for(fx: &Fixture) -> StoreConfig {
    StoreConfig {
        repo_root: fx.root().join("repo").to_string_lossy().into_owned(),
        key_file: fx.root().join("masterkey.json"),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(fx.root().join("rustic-cache")),
        no_cache: false,
    }
}

/// Session ids and shard counts the destination actually holds.
fn archived_sessions(cfg: &StoreConfig, mk: &MasterKey) -> BTreeMap<String, usize> {
    let store = BackupStore::new(cfg.clone(), MACHINE.to_string());
    store
        .read_all_machines(mk)
        .unwrap()
        .machines
        .into_iter()
        .flat_map(|m| m.sessions)
        .map(|s| (s.session_id, s.shard_count))
        .collect()
}

/// Set a directory's mtime without touching its contents, so a token change
/// is visible even on a filesystem with coarse mtime granularity.
#[cfg(unix)]
fn set_dir_mtime(path: &Path, when: SystemTime) {
    use std::os::unix::ffi::OsStrExt;
    use std::time::UNIX_EPOCH;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let secs = when.duration_since(UNIX_EPOCH).unwrap();
    let times = [
        libc::timeval {
            tv_sec: secs.as_secs() as libc::time_t,
            tv_usec: 0,
        },
        libc::timeval {
            tv_sec: secs.as_secs() as libc::time_t,
            tv_usec: 0,
        },
    ];
    let rc = unsafe { libc::utimes(c_path.as_ptr(), times.as_ptr()) };
    assert_eq!(rc, 0, "utimes failed for {}", path.display());
}

/// No-op on Windows: NTFS timestamps have 100 ns granularity, so the
/// shard write itself reliably moves the directory mtime the token
/// reads — there is no coarse-granularity filesystem to defeat, and
/// `std` cannot open a directory handle to set times on Windows anyway.
#[cfg(not(unix))]
fn set_dir_mtime(_path: &Path, _when: SystemTime) {}

/// Seal the fixture's session, then run a quiet pass over it.
fn seal_and_quiet_pass(fx: &Fixture) {
    let first = collect::collect(&fx.config(), &fx.stage, MACHINE, &fx.state, 20, &dest()).unwrap();
    assert_eq!(first.shards_written, 1);
    let (report, _, _) = pass_sequence(fx);
    assert_eq!(report.shards_written, 0);
    assert_eq!(report.changed_records, 0);
}

/// Criterion 1 — a quiet pass scans once and counts once. The push guard
/// reuses the collection's scan evidence and the audit's count instead of
/// rescanning the registry and recounting the stage.
#[test]
fn quiet_pass_performs_exactly_one_scan_and_one_count() {
    let fx = Fixture::new();
    let _env = EnvGuard::acquire(&fx.registry);

    seal_and_quiet_pass(&fx);

    scanner::reset_scan_with_machine_calls();
    store::reset_sealed_shard_count_calls();

    let (report, counted, check) = pass_sequence(&fx);

    assert_eq!(report.shards_written, 0);
    assert_eq!(report.changed_records, 0);
    assert_eq!(counted.count, 1);
    assert_eq!(check.stage_shards, 1);
    assert_eq!(check.scanner_records, 1);
    assert_eq!(scanner::scan_with_machine_calls(), 1);
    assert_eq!(store::sealed_shard_count_calls(), 1);
}

/// Criterion 2 — a changed pass (the kind that pushes) also scans once and
/// counts once: the push path adds no scan and no count of its own.
#[test]
fn changed_pass_performs_exactly_one_scan_and_one_count() {
    let fx = Fixture::new();
    let _env = EnvGuard::acquire(&fx.registry);

    let first = collect::collect(&fx.config(), &fx.stage, MACHINE, &fx.state, 20, &dest()).unwrap();
    assert_eq!(first.shards_written, 1);

    // The source grows: the next pass is a changed pass.
    fs::OpenOptions::new()
        .append(true)
        .open(fx.source_root.join("session.jsonl"))
        .unwrap()
        .write_all(b"three\n")
        .unwrap();

    scanner::reset_scan_with_machine_calls();
    store::reset_sealed_shard_count_calls();

    let (report, counted, check) = pass_sequence(&fx);

    assert!(report.changed_records > 0);
    assert_eq!(counted.count, 2);
    assert_eq!(check.stage_shards, 2);
    assert_eq!(scanner::scan_with_machine_calls(), 1);
    assert_eq!(store::sealed_shard_count_calls(), 1);
}

/// Criterion 3 — a quiet push reuses the collected count: the backup setup
/// finds the freshness token still matching and counts nothing.
#[test]
fn quiet_push_reuses_the_collected_count() {
    let fx = Fixture::new();
    let _env = EnvGuard::acquire(&fx.registry);

    seal_and_quiet_pass(&fx);
    let (_, counted, _) = pass_sequence(&fx);

    let cfg = cfg_for(&fx);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();

    store::reset_sealed_shard_count_calls();
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push_with_count(&fx.stage, &mk, Some(&counted))
        .unwrap();

    assert_eq!(store::sealed_shard_count_calls(), 0);
    let archived = archived_sessions(&cfg, &mk);
    assert_eq!(archived.len(), 1);
    assert_eq!(archived.values().next(), Some(&1));
}

/// Criterion 4 — a shard written between collect and push forces a recount
/// (the count is bound to a tree that no longer exists) and is still
/// archived: the backup walks the live tree, so the new session is included.
#[test]
fn shard_written_between_collect_and_push_forces_recount_and_is_archived() {
    let fx = Fixture::new();
    let _env = EnvGuard::acquire(&fx.registry);

    seal_and_quiet_pass(&fx);
    let (report, counted, _) = pass_sequence(&fx);
    assert_eq!(counted.count, 1);

    // A concurrent collect seals a new session into the stage after the
    // audit's count was taken.
    let machine_dir = fx.stage.join("sessions").join(MACHINE);
    let new_session = machine_dir.join("019bf00d-0000-7eb2-9bf8-000000000099");
    fs::create_dir_all(&new_session).unwrap();
    fs::write(new_session.join("000000.jsonl"), b"{\"k\":1}\n").unwrap();
    // Make the tree change visible even on a coarse-mtime filesystem.
    set_dir_mtime(&machine_dir, SystemTime::now() + Duration::from_secs(3600));

    // The count is bound to the old tree: the guard must not trust it.
    assert_eq!(counted.fresh_count(&fx.stage), None);

    store::reset_sealed_shard_count_calls();
    let check =
        collect::inspect_stage_for_push_with_evidence(&report.scan, &fx.stage, &fx.state, &counted)
            .unwrap();
    assert_eq!(check.stage_shards, 2);
    assert_eq!(store::sealed_shard_count_calls(), 1);

    let cfg = cfg_for(&fx);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push_with_count(&fx.stage, &mk, Some(&counted))
        .unwrap();
    // The backup setup found the same stale token and recounted too.
    assert_eq!(store::sealed_shard_count_calls(), 2);

    let archived = archived_sessions(&cfg, &mk);
    assert_eq!(archived.len(), 2);
    assert!(archived.values().all(|count| *count == 1));
}

/// Criterion 5 — a shard sealed into an existing session changes only that
/// session's mtime; the token catches it too, so a count is never trusted
/// across a write the counter could not see.
#[test]
fn shard_written_into_existing_session_breaks_the_count_token() {
    let fx = Fixture::new();
    let _env = EnvGuard::acquire(&fx.registry);

    seal_and_quiet_pass(&fx);
    let (_, counted, _) = pass_sequence(&fx);
    assert_eq!(counted.count, 1);

    let machine_dir = fx.stage.join("sessions").join(MACHINE);
    let session_dir = machine_dir
        .read_dir()
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .expect("fixture session dir");
    fs::write(session_dir.join("000001.jsonl"), b"{\"k\":2}\n").unwrap();
    set_dir_mtime(&session_dir, SystemTime::now() + Duration::from_secs(3600));

    assert_eq!(counted.fresh_count(&fx.stage), None);
    assert_eq!(store::sealed_shard_count(&fx.stage).unwrap(), 2);
}

/// Criterion 6 — an unreadable sessions root refuses the guard even when
/// collection's evidence is available: the token fails closed, the recount
/// fails, and the push is blocked. Unreadable stays distinct from empty.
///
/// Unix-only: the unreadable root is injected with chmod 0o000, and
/// Windows has no standard-API equivalent (its read denial lives in ACLs
/// `std::fs::Permissions` cannot express). The fail-closed property the
/// injection guards — a stage whose count cannot be retaken blocks the
/// push rather than reading as empty — is exercised on every platform by
/// the token-mismatch cases above, which force the same recount path.
#[cfg(unix)]
#[test]
fn unreadable_sessions_root_refuses_even_with_collected_evidence() {
    use std::os::unix::fs::PermissionsExt;

    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping: chmod-based unreadable injection does not block root");
        return;
    }

    let fx = Fixture::new();
    let _env = EnvGuard::acquire(&fx.registry);

    seal_and_quiet_pass(&fx);
    let (report, counted, _) = pass_sequence(&fx);
    assert_eq!(counted.count, 1);

    let sessions_root = fx.stage.join("sessions");
    fs::set_permissions(&sessions_root, fs::Permissions::from_mode(0o000)).unwrap();
    let via_evidence =
        collect::inspect_stage_for_push_with_evidence(&report.scan, &fx.stage, &fx.state, &counted);
    let standalone = collect::inspect_stage_for_push(&fx.config(), &fx.stage, &fx.state, MACHINE);
    fs::set_permissions(&sessions_root, fs::Permissions::from_mode(0o755)).unwrap();

    assert!(via_evidence.is_err());
    assert!(standalone.is_err());
}

/// Criterion 7 — a stage that never held anything answers "empty", not
/// "refused": the guard's reuse path must not turn a readable empty stage
/// into a failure.
#[test]
fn empty_stage_is_safe_while_unreadable_is_not() {
    let fx = Fixture::new();

    let counted = store::CountedStageShards::count(&fx.stage).unwrap();
    assert_eq!(counted.count, 0);
    let check = collect::inspect_stage_for_push_with_evidence(
        &collect::ScanEvidence::default(),
        &fx.stage,
        &fx.state,
        &counted,
    )
    .unwrap();
    assert_eq!(check.stage_shards, 0);
    assert!(check.empty_stage_is_safe());
}
