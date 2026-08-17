//! B48 · search prototype — metadata tier, on a repository built by this test.
//!
//! What these tests are for, in order of importance:
//!
//! 1. the metadata search really finds sessions in a real rustic repository
//!    (filters: session-id prefix / machine / time window);
//! 2. **it does not read data blobs** — proven dynamically by removing every
//!    data pack from the repository and showing the search still answers while
//!    the existing payload read path (`dump_machine_sessions`) fails;
//! 3. the payload tier's price is measured, not guessed;
//! 4. "not in this destination" and "I could not finish reading this
//!    destination" are two different answers.
//!
//! Nothing here prints payload bytes: only counts, byte totals and 8-char id
//! prefixes. The fixture content is synthetic (a deterministic LCG), so there
//! is no real conversation, account or hostname anywhere in the output.

use chat_stasher::readback;
use chat_stasher::search::{search_sessions, SearchFilter};
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const SHARD_BYTES: usize = 256 * 1024;
const SHARDS_PER_SESSION: usize = 4;
/// Packs at or above this size are data packs in this fixture; tree packs are
/// three orders of magnitude smaller. The test asserts the gap instead of
/// trusting the constant.
const DATA_PACK_MIN_BYTES: u64 = 64 * 1024;

/// Deterministic, badly-compressible filler so data packs cannot shrink into
/// the size class of tree packs.
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

/// Build one machine's stage and push it into `repo`.
fn push_machine(repo: &Path, root: &Path, machine: &str, sessions: &[&str], mk: &MasterKey) {
    let stage = root.join(format!("stage-{machine}"));
    for (s, session) in sessions.iter().enumerate() {
        for shard in 0..SHARDS_PER_SESSION {
            let seed = (s * 100 + shard) as u64 + machine.len() as u64;
            store::write_sealed_shard_bytes_with_cap(
                StageWriter::Collect,
                &stage,
                machine,
                session,
                &[filler(seed, SHARD_BYTES)],
                store::DEFAULT_SHARD_BUCKET_CAP,
            )
            .unwrap();
        }
    }
    let store = BackupStore::new(cfg(repo, &root.join("key.json")), machine.to_string());
    let summary = store.push(&stage, mk).unwrap();
    assert!(summary.files_new > 0, "fixture pushed nothing");
}

/// Every pack file in a local repository, with its size.
fn packs(repo: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let data = repo.join("data");
    for sub in fs::read_dir(&data).unwrap() {
        let sub = sub.unwrap();
        if !sub.file_type().unwrap().is_dir() {
            continue;
        }
        for pack in fs::read_dir(sub.path()).unwrap() {
            let pack = pack.unwrap();
            let len = pack.metadata().unwrap().len();
            out.push((pack.path(), len));
        }
    }
    out.sort();
    out
}

/// Total bytes of every file below `dir` (recursively); 0 when absent.
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            total += dir_bytes(&entry.path());
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

fn move_all(paths: &[PathBuf], to: &Path) {
    fs::create_dir_all(to).unwrap();
    for p in paths {
        fs::rename(p, to.join(p.file_name().unwrap())).unwrap();
    }
}

fn move_back(from: &Path, repo: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let dest = repo.join("data").join(&name[..2]).join(&name);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::rename(entry.path(), dest).unwrap();
    }
}

/// The whole prototype in one repository: filters, the no-data-blob proof, the
/// payload-tier price, and the two "found nothing" answers.
#[test]
fn metadata_search_finds_sessions_without_reading_data_blobs() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let repo = root.join("repo");
    let mk = MasterKey::new();

    push_machine(
        &repo,
        root,
        "m-alpha",
        &["aa11session-one", "bb22session-two"],
        &mk,
    );
    push_machine(&repo, root, "m-beta", &["aa11session-three"], &mk);

    let store = BackupStore::new(cfg(&repo, &root.join("key.json")), "m-alpha".to_string());

    // ---- 1. unfiltered metadata search -------------------------------------
    let t0 = Instant::now();
    let all = search_sessions(&store, &mk, &SearchFilter::default()).unwrap();
    let search_ms = t0.elapsed().as_millis();
    println!("\n[B48] === metadata search, no filter ===");
    println!(
        "[B48] snapshots_in_repo={} snapshots_scanned={} sessions_seen={} hits={} complete={} data_blobs_read={} elapsed_ms={}",
        all.snapshots_in_repo,
        all.snapshots_scanned,
        all.sessions_seen,
        all.hits.len(),
        all.complete(),
        all.data_blobs_read,
        search_ms
    );
    for h in &all.hits {
        println!(
            "[B48] hit machine={} session={} shards={} bytes={} data_blobs={} snapshot={} activity_unix={}",
            h.machine,
            h.short_id(),
            h.shard_count,
            h.bytes,
            h.data_blobs,
            h.short_snapshot(),
            h.activity_unix
        );
    }
    assert_eq!(all.hits.len(), 3);
    assert_eq!(all.snapshots_scanned, 2, "newest snapshot per machine");
    assert_eq!(all.data_blobs_read, 0);
    assert!(all.complete());
    for h in &all.hits {
        assert_eq!(h.shard_count, SHARDS_PER_SESSION);
        // +1 byte per shard: the writer frames each line with a newline.
        assert_eq!(h.bytes as usize, (SHARD_BYTES + 1) * SHARDS_PER_SESSION);
        assert!(h.data_blobs >= SHARDS_PER_SESSION);
    }

    // ---- 2. filters ---------------------------------------------------------
    let by_prefix = search_sessions(
        &store,
        &mk,
        &SearchFilter::default().session_id_prefix("aa11"),
    )
    .unwrap();
    println!(
        "[B48] filter session_id_prefix=aa11 -> hits={} ({})",
        by_prefix.hits.len(),
        by_prefix
            .hits
            .iter()
            .map(|h| format!("{}/{}", h.machine, h.short_id()))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert_eq!(by_prefix.hits.len(), 2, "one per machine");

    let by_machine =
        search_sessions(&store, &mk, &SearchFilter::default().machine("m-beta")).unwrap();
    println!(
        "[B48] filter machine=m-beta -> hits={} sessions_seen={}",
        by_machine.hits.len(),
        by_machine.sessions_seen
    );
    assert_eq!(by_machine.hits.len(), 1);

    let now = all.hits[0].activity_unix;
    let in_window = search_sessions(
        &store,
        &mk,
        &SearchFilter::default()
            .since_unix(now - 3600)
            .until_unix(now + 3600),
    )
    .unwrap();
    let future =
        search_sessions(&store, &mk, &SearchFilter::default().since_unix(now + 3600)).unwrap();
    println!(
        "[B48] filter window[-1h,+1h] -> hits={} ; window[+1h,..] -> hits={}",
        in_window.hits.len(),
        future.hits.len()
    );
    assert_eq!(in_window.hits.len(), 3);
    assert_eq!(future.hits.len(), 0);

    // ---- 3. the two "found nothing" terminal lines --------------------------
    println!(
        "[B48] no-hit line (complete scan):\n    {}",
        future.no_hit_line()
    );
    assert!(future.complete());
    assert!(future.no_hit_line().contains("not in this destination"));

    // ---- 4. payload-tier price, metadata-derived ----------------------------
    let cost = all.fulltext_cost();
    println!(
        "[B48] fulltext cost for {} sessions: shards={} data_blobs={} plaintext_bytes={}",
        cost.sessions, cost.shards, cost.data_blobs, cost.plaintext_bytes
    );

    // ...and the same price paid for real, through the existing read path.
    let wanted: BTreeSet<String> = all
        .hits
        .iter()
        .filter(|h| h.machine == "m-alpha")
        .map(|h| h.session_id.clone())
        .collect();
    let t1 = Instant::now();
    let dumped = store
        .dump_machine_sessions(&mk, "m-alpha", &wanted)
        .unwrap();
    let dump_ms = t1.elapsed().as_millis();
    let dumped_bytes: usize = dumped.values().flatten().map(Vec::len).sum();
    println!(
        "[B48] measured payload read for {} sessions of m-alpha: bytes={} elapsed_ms={} (metadata search over the whole repo: elapsed_ms={})",
        dumped.len(),
        dumped_bytes,
        dump_ms,
        search_ms
    );
    assert_eq!(dumped_bytes, 2 * (SHARD_BYTES + 1) * SHARDS_PER_SESSION);

    // ---- 5. the proof: remove every data pack, search again -----------------
    let all_packs = packs(&repo);
    let (data_packs, meta_packs): (Vec<_>, Vec<_>) = all_packs
        .iter()
        .partition(|(_, len)| *len >= DATA_PACK_MIN_BYTES);
    let data_bytes: u64 = data_packs.iter().map(|(_, l)| *l).sum();
    let meta_bytes: u64 = meta_packs.iter().map(|(_, l)| *l).sum();
    println!(
        "[B48] repo packs: data_packs={} data_pack_bytes={} | tree_packs={} tree_pack_bytes={}",
        data_packs.len(),
        data_bytes,
        meta_packs.len(),
        meta_bytes
    );
    println!(
        "[B48] repo bytes by tier: metadata(snapshots+index+tree packs)={} payload(data packs)={}",
        dir_bytes(&repo.join("snapshots")) + dir_bytes(&repo.join("index")) + meta_bytes,
        data_bytes
    );
    assert!(!data_packs.is_empty() && !meta_packs.is_empty());
    assert!(
        meta_packs.iter().all(|(_, l)| *l < DATA_PACK_MIN_BYTES / 4),
        "the two pack size classes must be unambiguous"
    );

    // rustic keeps a local metadata cache (`<cache>/rustic/<repo-id>/…`).
    // Clear it, or the next two steps would be testing that cache instead of
    // the repository.
    let pack_names: Vec<String> = all_packs
        .iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    println!(
        "[B48] rustic metadata cache cleared: {} dir(s)",
        clear_rustic_cache(&pack_names).len()
    );

    let quarantine = root.join("quarantine-data-packs");
    move_all(
        &data_packs
            .iter()
            .map(|(p, _)| p.clone())
            .collect::<Vec<_>>(),
        &quarantine,
    );

    let without_data = search_sessions(&store, &mk, &SearchFilter::default())
        .expect("metadata search must still work with every data pack removed");
    println!(
        "[B48] with ALL {} data packs removed: hits={} complete={} data_blobs_read={}",
        data_packs.len(),
        without_data.hits.len(),
        without_data.complete(),
        without_data.data_blobs_read
    );
    assert_eq!(without_data.hits.len(), 3);
    assert!(without_data.complete());

    // Same repository state, payload path: must fail. This is what makes the
    // line above evidence rather than a claim.
    let payload = store.dump_machine_sessions(&mk, "m-alpha", &wanted);
    println!(
        "[B48] payload read on the same state: is_err={} ({})",
        payload.is_err(),
        payload
            .as_ref()
            .err()
            .map(|e| e.to_string().lines().next().unwrap_or("").to_string())
            .unwrap_or_else(|| "unexpected success".into())
    );
    assert!(
        payload.is_err(),
        "if the payload path still works, the data packs were not really gone"
    );

    move_back(&quarantine, &repo);

    // ---- 6. UNKNOWN, not empty: break the metadata the search needs ---------
    println!(
        "[B48] rustic metadata cache cleared again: {} dir(s)",
        clear_rustic_cache(&pack_names).len()
    );
    let meta_paths: Vec<PathBuf> = meta_packs.iter().map(|(p, _)| p.clone()).collect();
    let quarantine2 = root.join("quarantine-tree-packs");
    move_all(&meta_paths, &quarantine2);
    let broken = search_sessions(&store, &mk, &SearchFilter::default());
    match &broken {
        Ok(report) => {
            println!(
                "[B48] tree packs removed -> Ok(hits={} complete={} unreadable={})",
                report.hits.len(),
                report.complete(),
                report.unreadable.len()
            );
            println!(
                "[B48] no-hit line (partial scan):\n    {}",
                report.no_hit_line()
            );
            assert!(
                !report.complete(),
                "a destination that cannot be fully read must never look empty"
            );
            assert!(report.no_hit_line().contains("UNKNOWN"));
        }
        Err(e) => {
            println!(
                "[B48] tree packs removed -> Err({}) — surfaced as a failure, not as 0 hits",
                e.to_string().lines().next().unwrap_or("")
            );
        }
    }
    move_back(&quarantine2, &repo);
    let _ = readback::bucket_shard_path(Path::new("/x/sessions/m/s/000/000001.jsonl"));
}

/// Delete rustic's local metadata cache for this repository, if any.
///
/// The cache directory is named after the *decrypted* config id, which a test
/// cannot compute without opening the repo, so the cache is instead identified
/// by content: the cache dir that holds one of this repository's pack ids.
/// Returns the directories that were actually removed.
fn clear_rustic_cache(pack_names: &[String]) -> Vec<PathBuf> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let mut removed = Vec::new();
    for base in [
        home.join("Library/Caches/rustic"),
        home.join(".cache/rustic"),
    ] {
        let Ok(entries) = fs::read_dir(&base) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if dir_holds_any(&entry.path(), pack_names) && fs::remove_dir_all(entry.path()).is_ok()
            {
                removed.push(entry.path());
            }
        }
    }
    removed
}

fn dir_holds_any(dir: &Path, names: &[String]) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if names.contains(&name) {
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
