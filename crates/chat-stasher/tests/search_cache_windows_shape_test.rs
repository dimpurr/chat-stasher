//! The Windows shape of `search_test::metadata_search_finds_sessions_without_reading_data_blobs`,
//! runnable on every platform.
//!
//! That test is green on macOS and Linux and red on `windows-latest`, at
//! `search_test.rs:375` — `assert!(!report.complete())`, the line that forbids
//! "I could not finish reading this destination" from being answered as "not
//! there". Its step 6 removes every tree pack from the repository and then
//! searches. For that to mean anything, rustic's *local metadata cache* has to
//! be gone first, or the search reads the trees out of the cache and truthfully
//! reports a complete scan of a repository that is no longer readable.
//!
//! Clearing that cache means knowing where it is. rustic asks `dirs`
//! (rustic_core-0.12.0 `src/backend/cache.rs:261`), and `dirs` spells the
//! per-user cache directory differently per platform:
//!
//!   * macOS   `$HOME/Library/Caches/rustic`               (dirs-6.0.0 `src/mac.rs:9`)
//!   * Linux   `$XDG_CACHE_HOME/rustic`, else `$HOME/.cache/rustic` (`src/lin.rs:8`)
//!   * Windows `%LOCALAPPDATA%\rustic` — **not under `$HOME` at all**
//!     (`src/win.rs:10` → `dirs_sys::known_folder_local_app_data()`)
//!
//! The fixture used to hard-code the first two. On Windows both candidates miss,
//! nothing is cleared, and the invariant flips. Same family as the explicit-root
//! and `RUNNER~1` work: a directory that is right there, spelled in another
//! platform's dialect, and therefore treated as absent.
//!
//! Nothing here is `cfg!(windows)`-gated. The first test is pure — it is handed
//! the platform key and the environment as arguments, so all three spellings are
//! asserted on whichever machine runs it. The second is a live cross-check that
//! the resolution for the *running* platform finds the directory rustic actually
//! created. The third pins the mechanism itself, end to end.
//!
//! Only counts, sizes and pack ids are printed — the fixture is synthetic, so
//! there is no real conversation, account or hostname in the output.

use chat_stasher::scanner::user_cache_dirs_on;
use chat_stasher::search::{search_sessions, SearchFilter};
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// 1. the spelling, asserted from any platform
// ---------------------------------------------------------------------------

/// The Windows CI runner's home, 8.3 short name and all — the same shape
/// `scanner_windows_shape_test` uses, so the two instruments describe one
/// machine.
const WIN_HOME: &str = r"C:\Users\RUNNER~1";
const WIN_LOCAL_APP_DATA: &str = r"C:\Users\RUNNER~1\AppData\Local";

#[test]
fn the_per_user_cache_dir_is_spelled_the_way_each_platform_spells_it() {
    let win_home = PathBuf::from(WIN_HOME);
    let win = user_cache_dirs_on(
        "windows",
        &win_home,
        None,
        Some(Path::new(WIN_LOCAL_APP_DATA)),
    );
    println!("windows-shape self-check: windows -> {win:?}");
    assert_eq!(
        win.first().map(PathBuf::as_path),
        Some(Path::new(WIN_LOCAL_APP_DATA)),
        "Windows per-user cache dir is %LOCALAPPDATA% (dirs-6.0.0 src/win.rs:10), not derived from $HOME"
    );
    // The old hard-coded pair, which is what made `windows-latest` red.
    for wrong in [win_home.join("Library/Caches"), win_home.join(".cache")] {
        assert!(
            !win.contains(&wrong),
            "{} is the macOS/Linux spelling; it does not exist on Windows — clearing by it would clear nothing",
            wrong.display()
        );
    }

    // `%LOCALAPPDATA%` unset: the documented default, same fallback shape as
    // `scanner::local_appdata_dir`. Compared against `join` rather than a `\`
    // literal because the separator `join` inserts is the *running* platform's
    // (`/` here, `\` on Windows) — the component sequence is the invariant.
    let win_default = user_cache_dirs_on("windows", &win_home, None, None);
    println!("windows-shape self-check: windows, %LOCALAPPDATA% unset -> {win_default:?}");
    assert_eq!(
        win_default.first().map(PathBuf::as_path),
        Some(win_home.join("AppData/Local").as_path()),
        "with %LOCALAPPDATA% unset, fall back to %USERPROFILE%\\AppData\\Local, still not $HOME/.cache"
    );
    assert_eq!(
        win_default
            .first()
            .unwrap()
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .last()
            .map(String::as_str),
        Some("Local")
    );

    let unix_home = PathBuf::from("/home/u");
    let mac = user_cache_dirs_on("macos", &unix_home, None, None);
    println!("windows-shape self-check: macos -> {mac:?}");
    assert_eq!(
        mac.first().map(PathBuf::as_path),
        Some(Path::new("/home/u/Library/Caches"))
    );

    let linux = user_cache_dirs_on("linux", &unix_home, None, None);
    println!("windows-shape self-check: linux -> {linux:?}");
    assert_eq!(
        linux.first().map(PathBuf::as_path),
        Some(Path::new("/home/u/.cache"))
    );

    // Not Windows-only: a Linux box with `$XDG_CACHE_HOME` set fails in exactly
    // the same way if the lookup is a `$HOME` guess (dirs-6.0.0 `src/lin.rs:8`
    // reads it first). CI's ubuntu runner simply does not set it.
    let xdg = user_cache_dirs_on("linux", &unix_home, Some(Path::new("/scratch/cache")), None);
    println!("windows-shape self-check: linux, XDG_CACHE_HOME=/scratch/cache -> {xdg:?}");
    assert_eq!(
        xdg.first().map(PathBuf::as_path),
        Some(Path::new("/scratch/cache")),
        "$XDG_CACHE_HOME takes precedence over ~/.cache"
    );
}

// ---------------------------------------------------------------------------
// 2 + 3. the live cross-check and the mechanism, on whatever platform runs this
// ---------------------------------------------------------------------------

const SHARD_BYTES: usize = 8 * 1024;

fn filler(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        out.extend_from_slice(format!("{:016x}", state).as_bytes());
    }
    out.truncate(len);
    out
}

fn cfg(repo: &Path, key: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: repo.to_string_lossy().into_owned(),
        key_file: key.to_path_buf(),
        connections: 1,
        options: BTreeMap::new(),
    }
}

/// Every pack file in a local repository, with its size.
fn packs(repo: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    for sub in fs::read_dir(repo.join("data")).unwrap() {
        let sub = sub.unwrap();
        if !sub.file_type().unwrap().is_dir() {
            continue;
        }
        for pack in fs::read_dir(sub.path()).unwrap() {
            let pack = pack.unwrap();
            out.push((pack.path(), pack.metadata().unwrap().len()));
        }
    }
    out.sort();
    out
}

fn dir_holds_any(dir: &Path, names: &[String]) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if names.contains(&entry.file_name().to_string_lossy().into_owned()) {
            return true;
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
            && dir_holds_any(&entry.path(), names)
        {
            return true;
        }
    }
    false
}

/// Cache directories for *this* repository, found by content under the roots
/// `store::rustic_cache_roots` names. Nothing outside those roots is touched,
/// and a directory is only a candidate if it demonstrably holds one of this
/// fixture's pack ids.
fn cache_dirs_for(pack_names: &[String]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for base in store::rustic_cache_roots() {
        let Ok(entries) = fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && dir_holds_any(&entry.path(), pack_names)
            {
                found.push(entry.path());
            }
        }
    }
    found
}

/// A one-machine repository plus the pack ids it is made of.
fn fixture(root: &Path) -> (BackupStore, MasterKey, PathBuf, Vec<String>) {
    let repo = root.join("repo");
    let mk = MasterKey::new();
    let stage = root.join("stage");
    for shard in 0..2u64 {
        store::write_sealed_shard_bytes_with_cap(
            StageWriter::Collect,
            &stage,
            "m-alpha",
            "aa11session-one",
            &[filler(shard, SHARD_BYTES)],
            store::DEFAULT_SHARD_BUCKET_CAP,
        )
        .unwrap();
    }
    let store = BackupStore::new(cfg(&repo, &root.join("key.json")), "m-alpha".to_string());
    assert!(
        store.push(&stage, &mk).unwrap().files_new > 0,
        "fixture pushed nothing"
    );
    let names = packs(&repo)
        .iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    (store, mk, repo, names)
}

/// The resolution is not fiction: on the platform actually running this test,
/// `store::rustic_cache_roots` must contain the directory rustic just created.
///
/// This is the half that goes red on `windows-latest` and only there — which is
/// exactly why the pure test above exists as well.
#[test]
fn rustic_cache_roots_finds_the_cache_rustic_actually_wrote() {
    let dir = tempfile::TempDir::new().unwrap();
    let (store, mk, _repo, pack_names) = fixture(dir.path());
    search_sessions(&store, &mk, &SearchFilter::default()).unwrap();

    let found = cache_dirs_for(&pack_names);
    println!(
        "windows-shape self-check: platform={} roots={:?} cache_dirs_found={}",
        chat_stasher::scanner::current_platform(),
        store::rustic_cache_roots(),
        found.len()
    );
    assert_eq!(
        found.len(),
        1,
        "rustic always creates a cache when opening a repo (rustic_core src/repository.rs:549); \
         rustic_cache_roots() must point at it; found {}",
        found.len()
    );
    for d in found {
        fs::remove_dir_all(d).unwrap();
    }
}

/// The mechanism, end to end: a repository whose tree packs are gone reads as
/// *complete* for as long as rustic's metadata cache still has them, and reads
/// as UNKNOWN once the cache is really cleared.
///
/// This is the load-bearing premise of `search_test`'s step 6, stated as its own
/// test so that "the cache clearing silently did nothing" can never again show
/// up as "the search is broken".
#[test]
fn tree_packs_are_only_really_gone_once_the_metadata_cache_is_gone() {
    let dir = tempfile::TempDir::new().unwrap();
    let (store, mk, repo, pack_names) = fixture(dir.path());
    search_sessions(&store, &mk, &SearchFilter::default()).unwrap();

    // Tree packs are the small ones; data packs carry the 8 KiB shards.
    let all_packs = packs(&repo);
    let tree_packs: Vec<PathBuf> = all_packs
        .iter()
        .filter(|(_, len)| *len < 4 * 1024)
        .map(|(p, _)| p.clone())
        .collect();
    assert!(!tree_packs.is_empty(), "fixture produced no tree pack");
    let quarantine = dir.path().join("quarantine");
    fs::create_dir_all(&quarantine).unwrap();
    for p in &tree_packs {
        fs::rename(p, quarantine.join(p.file_name().unwrap())).unwrap();
    }

    // Cache still warm: the repository is unreadable, and the search cannot
    // tell, because it is not reading the repository.
    let warm = search_sessions(&store, &mk, &SearchFilter::default()).unwrap();
    println!(
        "windows-shape self-check: {} tree pack(s) removed, cache WARM -> hits={} complete={} unreadable={}",
        tree_packs.len(),
        warm.hits.len(),
        warm.complete(),
        warm.unreadable.len()
    );
    assert!(
        warm.complete(),
        "premise failed: with the cache warm it should read completely, actual complete={}",
        warm.complete()
    );

    // Now clear it the way the fixture is supposed to, and the same repository
    // state answers UNKNOWN.
    let cleared = cache_dirs_for(&pack_names);
    println!(
        "windows-shape self-check: cache dirs cleared = {}",
        cleared.len()
    );
    assert_eq!(
        cleared.len(),
        1,
        "clearing must actually clear something; clearing 0 is exactly what made windows-latest red"
    );
    for d in &cleared {
        fs::remove_dir_all(d).unwrap();
    }

    let cold = search_sessions(&store, &mk, &SearchFilter::default());
    match &cold {
        Ok(report) => {
            println!(
                "windows-shape self-check: cache COLD -> hits={} complete={} unreadable={}",
                report.hits.len(),
                report.complete(),
                report.unreadable.len()
            );
            assert!(
                !report.complete(),
                "an un-finished destination must never look empty; hits={} unreadable={}",
                report.hits.len(),
                report.unreadable.len()
            );
            assert!(report.no_hit_line().contains("UNKNOWN"));
        }
        Err(e) => println!(
            "windows-shape self-check: cache COLD -> Err({}) — surfaces as a failure, not as 0 hits",
            e.to_string().lines().next().unwrap_or("")
        ),
    }

    for d in cache_dirs_for(&pack_names) {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "The integration test removes its temporary cache directories on a best-effort basis after assertions."
        )]
        let _ = fs::remove_dir_all(d);
    }
}
