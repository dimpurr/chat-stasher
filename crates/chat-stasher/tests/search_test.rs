//! B48 · search prototype — metadata tier, on a repository built by this test.
//!
//! What these tests are for, in order of importance:
//!
//! 1. the metadata search really finds sessions in a real rustic repository
//!    (filters: session-id prefix / machine / harness / conversation-time
//!    window);
//! 2. **it does not read a session shard** — proven dynamically by removing
//!    every data pack from the repository and showing the search still answers
//!    while the existing payload read path (`dump_machine_sessions`) fails.
//!    Note the precision of that claim after ADR-027: reading the activity
//!    sidecar *does* fetch blobs, because in a rustic repository every file's
//!    bytes are blobs, and this fixture has no sidecar. What is proven is that
//!    no *conversation* blob is read, which is the promise that matters;
//!    `data_blobs_read` counts it and `index_files_read` is counted apart;
//! 3. the payload tier's price is measured, not guessed;
//! 4. "not in this destination", "I could not finish reading this destination"
//!    and "I read it all but could not place every session in time" are three
//!    different answers;
//! 5. the time window filters on the conversation's own interval, not on when
//!    the backup ran (see `conversation_time_filter_ignores_snapshot_time`).
//!
//! Nothing here prints payload bytes: only counts, byte totals and 8-char id
//! prefixes. The fixture content is synthetic (a deterministic LCG), so there
//! is no real conversation, account or hostname anywhere in the output.

use chat_stasher::activity::{ActivityRow, TimeSource};
use chat_stasher::readback;
use chat_stasher::search::search_sessions;
use chat_stasher::selector::{Selector, SelectorArgs};
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
        cache_dir: None,
        no_cache: false,
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

/// A raw unix-seconds window, built the way the deprecated `--since-unix` /
/// `--until-unix` flags build it, so these tests exercise the same shape the
/// CLI produces.
fn window_from(since: i64, until: i64) -> chat_stasher::selector::TimeWindow {
    SelectorArgs {
        since_unix: Some(since),
        until_unix: Some(until),
        ..Default::default()
    }
    .resolve()
    .unwrap()
    .selector
    .window
    .expect("an explicit since/until pair must produce a window")
}

/// One activity-index row for a session whose conversation ran
/// `[first, last]`, with times recorded as exact.
fn index_row(machine: &str, session: &str, first: i64, last: i64) -> ActivityRow {
    ActivityRow {
        session_id: session.to_string(),
        machine: machine.to_string(),
        harness: "claude-code".to_string(),
        first_unix: Some(first),
        last_unix: Some(last),
        line_count: 4,
        time_source: TimeSource::Exact,
        source_zone: None,
    }
}

/// Write `meta/<machine>/activity-v1.jsonl` into a stage directory.
fn write_activity_index(stage: &Path, machine: &str, rows: &[ActivityRow]) {
    let meta_dir = stage.join("meta").join(machine);
    fs::create_dir_all(&meta_dir).unwrap();
    let mut body = String::new();
    for row in rows {
        body.push_str(&serde_json::to_string(row).unwrap());
        body.push('\n');
    }
    fs::write(meta_dir.join("activity-v1.jsonl"), body).unwrap();
}

/// Shard one session into a stage (`SHARDS_PER_SESSION` shards, like the rest
/// of the fixture) so the push has real payload to carry.
fn write_session_shards(stage: &Path, machine: &str, session: &str, salt: u64) {
    for shard in 0..SHARDS_PER_SESSION {
        store::write_sealed_shard_bytes_with_cap(
            StageWriter::Collect,
            stage,
            machine,
            session,
            &[filler(salt + shard as u64, SHARD_BYTES)],
            store::DEFAULT_SHARD_BUCKET_CAP,
        )
        .unwrap();
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
    let all = search_sessions(&store, &mk, &Selector::default()).unwrap();
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
            "[B48] hit machine={} session={} shards={} bytes={} data_blobs={} snapshot={} archive_time_unix={}",
            h.machine,
            h.short_id(),
            h.shard_count,
            h.bytes,
            h.data_blobs,
            h.short_snapshot(),
            h.archive_time_unix
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
    let by_prefix =
        search_sessions(&store, &mk, &Selector::default().session_id_prefix("aa11")).unwrap();
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

    let by_machine = search_sessions(&store, &mk, &Selector::default().machine("m-beta")).unwrap();
    println!(
        "[B48] filter machine=m-beta -> hits={} sessions_seen={}",
        by_machine.hits.len(),
        by_machine.sessions_seen
    );
    assert_eq!(by_machine.hits.len(), 1);

    // The harness filter reads the leading `.`/`~` segment of the archived id —
    // the same function `activity-index` uses to fill its `harness` column, so
    // the two cannot disagree about what a session is. These fixture ids carry
    // no delimiter at all, which makes each whole id its own harness; that is
    // the documented behaviour of `infer_harness`, not a special case here.
    let by_harness = search_sessions(
        &store,
        &mk,
        &Selector::default().harnesses(["aa11session-one"]),
    )
    .unwrap();
    let by_other_harness =
        search_sessions(&store, &mk, &Selector::default().harnesses(["claude-code"])).unwrap();
    println!(
        "[B48] filter harness=aa11session-one -> hits={} ; harness=claude-code -> hits={} not_matched={} unplaced={}",
        by_harness.hits.len(),
        by_other_harness.hits.len(),
        by_other_harness.not_matched,
        by_other_harness.unplaced.len()
    );
    assert_eq!(by_harness.hits.len(), 1);
    assert_eq!(by_harness.hits[0].session_id, "aa11session-one");
    assert_eq!(
        by_harness.hits[0].harness.as_deref(),
        Some("aa11session-one"),
        "the reported harness is the one the filter matched on"
    );
    assert_eq!(by_other_harness.hits.len(), 0);
    assert_eq!(
        by_other_harness.not_matched, 3,
        "a harness nobody has is a real negative, evaluated for every session"
    );
    assert!(by_other_harness.unplaced.is_empty());
    assert!(by_other_harness.answer_complete());

    // A harness filter combined with a machine filter is an AND, not a
    // fallback: one session matches both, and the third matches neither.
    let by_both = search_sessions(
        &store,
        &mk,
        &Selector::default()
            .machine("m-alpha")
            .harnesses(["aa11session-one"]),
    )
    .unwrap();
    assert_eq!(by_both.hits.len(), 1);

    // ---- 2b. a time window against a fixture with NO activity index --------
    //
    // This fixture pushes shards only, so no `meta/<machine>/activity-v1.jsonl`
    // exists. Under ADR-027 that is not "these sessions are outside the
    // window" — their conversation time was never recorded, and a window is a
    // question this archive cannot answer. Every session must land in the
    // unplaced list, and the answer must not be presented as a negative.
    let now = all.hits[0].archive_time_unix;
    let in_window = search_sessions(
        &store,
        &mk,
        &Selector::default().window(window_from(now - 3600, now + 3600)),
    )
    .unwrap();
    println!(
        "[B48] window[-1h,+1h] with no activity index -> hits={} unplaced={} complete={} answer_complete={}",
        in_window.hits.len(),
        in_window.unplaced.len(),
        in_window.complete(),
        in_window.answer_complete()
    );
    assert_eq!(in_window.hits.len(), 0);
    assert_eq!(
        in_window.unplaced.len(),
        3,
        "no index means no session can be placed in time, and none may be dropped"
    );
    assert_eq!(in_window.session_time_unknown(), 3);
    assert!(in_window.complete(), "the destination itself was read");
    assert!(
        !in_window.answer_complete(),
        "a window over an archive with no index is unanswerable, not empty"
    );
    assert!(in_window.no_hit_line().contains("UNKNOWN"));
    assert!(!in_window.no_hit_line().contains("not in this destination"));
    assert!(
        in_window
            .unplaced
            .iter()
            .all(|u| u.why.contains("no activity index")),
        "each unplaced session must carry the reason, not just a count"
    );

    // ---- 3. the "found nothing" terminal line, honestly reached ------------
    //
    // A constraint that *can* be evaluated ends in a real negative: the
    // destination was read in full, every session was evaluated, and none
    // matched. That is the only situation allowed to say "not in this
    // destination", and it is still reachable.
    let no_such_machine =
        search_sessions(&store, &mk, &Selector::default().machine("no-such-machine")).unwrap();
    println!(
        "[B48] filter machine=no-such-machine -> hits={} not_matched={} unplaced={}",
        no_such_machine.hits.len(),
        no_such_machine.not_matched,
        no_such_machine.unplaced.len()
    );
    assert_eq!(no_such_machine.hits.len(), 0);
    assert_eq!(no_such_machine.not_matched, 3);
    assert!(no_such_machine.unplaced.is_empty());
    assert!(no_such_machine.answer_complete());
    println!(
        "[B48] no-hit line (complete scan):\n    {}",
        no_such_machine.no_hit_line()
    );
    assert!(no_such_machine
        .no_hit_line()
        .contains("not in this destination"));

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
    let cleared = clear_rustic_cache(&pack_names);
    println!(
        "[B48] rustic metadata cache cleared: {} dir(s)",
        cleared.len()
    );
    // Say so *here* if the cache was not found. Without this, a lookup that
    // silently clears nothing resurfaces far below as "the search is broken"
    // (step 6) — which is precisely how this test read on `windows-latest`.
    assert_eq!(
        cleared.len(),
        1,
        "rustic definitely created cache; cleared {} here = cache not found. roots={:?}",
        cleared.len(),
        store::rustic_cache_roots()
    );

    let quarantine = root.join("quarantine-data-packs");
    move_all(
        &data_packs
            .iter()
            .map(|(p, _)| p.clone())
            .collect::<Vec<_>>(),
        &quarantine,
    );

    let without_data = search_sessions(&store, &mk, &Selector::default())
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
    let cleared_again = clear_rustic_cache(&pack_names);
    println!(
        "[B48] rustic metadata cache cleared again: {} dir(s)",
        cleared_again.len()
    );
    assert_eq!(
        cleared_again.len(),
        1,
        "as above: cleared {} cache dirs = assertion below would test cache instead of repository. roots={:?}",
        cleared_again.len(),
        store::rustic_cache_roots()
    );
    let meta_paths: Vec<PathBuf> = meta_packs.iter().map(|(p, _)| p.clone()).collect();
    let quarantine2 = root.join("quarantine-tree-packs");
    move_all(&meta_paths, &quarantine2);
    let broken = search_sessions(&store, &mk, &Selector::default());
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

#[test]
fn no_content_activity_row_survives_real_search_index_read() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let repo = root.join("repo");
    let mk = MasterKey::new();
    let machine = "m-no-content";
    let session = "claude-code.m-no-content.aaaaaaaa-0000-0000-0000-000000000099";
    let stage = root.join("stage");

    write_session_shards(&stage, machine, session, 99);
    let mut row = index_row(machine, session, 0, 0);
    row.first_unix = None;
    row.last_unix = None;
    row.line_count = 0;
    row.time_source = TimeSource::NoConversationContent;
    write_activity_index(&stage, machine, &[row]);
    let store = BackupStore::new(cfg(&repo, &root.join("key.json")), machine.to_string());
    assert!(store.push(&stage, &mk).unwrap().files_new > 0);

    let result = search_sessions(&store, &mk, &Selector::default()).unwrap();
    assert_eq!(result.hits.len(), 1);
    assert_eq!(
        result.hits[0].time_source,
        TimeSource::NoConversationContent
    );
    assert_eq!(result.hits[0].time_why, None);
    assert_eq!(result.machine_window_summary()[0].time_unknown, 0);
    let json: serde_json::Value =
        serde_json::from_str(&chat_stasher::search::report_json(&result, false)).unwrap();
    assert_eq!(
        json["sessions"][0]["first_unix"]["kind"],
        "no_conversation_content"
    );
    assert_eq!(
        json["sessions"][0]["last_unix"]["kind"],
        "no_conversation_content"
    );

    let window = search_sessions(
        &store,
        &mk,
        &Selector::default().window(window_from(1_704_067_200, 1_704_067_201)),
    )
    .unwrap();
    assert_eq!(window.unplaced.len(), 1);
    assert_eq!(
        window.unplaced[0].dimension,
        chat_stasher::selector::UnplacedBy::NoContent
    );
    assert_eq!(window.machine_window_summary()[0].time_unknown, 0);
}

/// Delete rustic's local metadata cache for this repository, if any.
///
/// The cache directory is named after the *decrypted* config id, which a test
/// cannot compute without opening the repo, so the cache is instead identified
/// by content: the cache dir that holds one of this repository's pack ids.
/// Returns the directories that were actually removed.
///
/// *Where* to look is not guessed here: `store::rustic_cache_roots` spells it
/// the way each platform spells it. This used to be a hand-written
/// `$HOME/Library/Caches` + `$HOME/.cache` pair, which is wrong on Windows
/// (the cache lives under `%LOCALAPPDATA%`, not under `$HOME`) — the cache
/// then survived, step 6 below read the tree packs out of it, and a repository
/// that could not be read looked complete. See
/// `search_cache_windows_shape_test.rs`.
fn clear_rustic_cache(pack_names: &[String]) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    for base in store::rustic_cache_roots() {
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

/// ADR-027 · the time window filters on the **conversation**, not on the
/// backup run.
///
/// The regression nail. Two machines are pushed *now*, so under the old
/// implementation — which compared the rustic snapshot time — a window around
/// "now" matched every session in the archive, whatever the conversations were
/// about. Here every conversation happened in 2024 while the push happened in
/// 2026, and a window around the push must match **nothing**.
///
/// This test was shown failing against the pre-change filter before the change
/// landed; see the W11 report, step 4.
#[test]
fn conversation_time_filter_ignores_snapshot_time() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let repo = root.join("repo");
    let mk = MasterKey::new();

    // A fixed instant in 2024, far from any plausible "now" in a test run, so
    // "the window around the push" and "the window around the conversation"
    // cannot accidentally overlap.
    const JAN_2024: i64 = 1_704_067_200; // 2024-01-01T00:00:00Z
    const DAY: i64 = 86_400;

    let sessions: &[(&str, &str, i64, i64)] = &[
        (
            "m-alpha",
            "claude-code.m-alpha.aaaaaaaa-0000-0000-0000-000000000001",
            JAN_2024,
            JAN_2024 + 2 * DAY,
        ),
        (
            "m-alpha",
            "claude-code.m-alpha.aaaaaaaa-0000-0000-0000-000000000002",
            JAN_2024 + 10 * DAY,
            JAN_2024 + 10 * DAY,
        ),
        (
            "m-beta",
            "codex.m-beta.bbbbbbbb-0000-0000-0000-000000000001",
            JAN_2024 + 40 * DAY,
            JAN_2024 + 45 * DAY,
        ),
    ];

    for machine in ["m-alpha", "m-beta"] {
        let stage = root.join(format!("stage-{machine}"));
        let mut rows = Vec::new();
        for (i, (m, session, first, last)) in sessions.iter().enumerate() {
            if *m != machine {
                continue;
            }
            write_session_shards(&stage, machine, session, i as u64 * 100 + 1);
            rows.push(index_row(m, session, *first, *last));
        }
        write_activity_index(&stage, machine, &rows);
        let store = BackupStore::new(cfg(&repo, &root.join("key.json")), machine.to_string());
        assert!(
            store.push(&stage, &mk).unwrap().files_new > 0,
            "fixture pushed nothing"
        );
    }

    let store = BackupStore::new(cfg(&repo, &root.join("key.json")), "m-alpha".to_string());

    // ---- 1. the snapshots really are new, and the conversations really are not
    let all = search_sessions(&store, &mk, &Selector::default()).unwrap();
    assert_eq!(all.hits.len(), 3);
    assert_eq!(all.snapshots_scanned, 2);
    let snapshot_time = all
        .hits
        .iter()
        .map(|h| h.archive_time_unix)
        .max()
        .expect("three hits");
    println!(
        "[W11] fixture: snapshot_time_unix={snapshot_time} conversation spans={:?}",
        all.hits
            .iter()
            .map(|h| (h.first_unix, h.last_unix))
            .collect::<Vec<_>>()
    );
    assert!(
        snapshot_time > JAN_2024 + 365 * DAY,
        "the fixture must push well after the conversations, or this test proves nothing"
    );
    for h in &all.hits {
        let first = h.first_unix.expect("the fixture index has both bounds");
        let last = h.last_unix.expect("the fixture index has both bounds");
        assert!(
            last < JAN_2024 + 100 * DAY,
            "every conversation must be far from the push, got {first}..{last}"
        );
    }
    // Every session in one machine's snapshot shares that snapshot's time.
    // That is exactly why it cannot answer "when was this conversation".
    let mut per_machine: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
    for h in &all.hits {
        per_machine
            .entry(h.machine.as_str())
            .or_default()
            .insert(h.archive_time_unix);
    }
    assert!(
        per_machine.values().all(|times| times.len() == 1),
        "one snapshot time per machine, by construction: {per_machine:?}"
    );
    assert_eq!(all.index_files_read, 2, "one activity index per machine");
    assert_eq!(all.data_blobs_read, 0, "no shard was read");

    // ---- 2. THE NAIL: a window around the push matches nothing -------------
    let around_the_push = search_sessions(
        &store,
        &mk,
        &Selector::default().window(window_from(snapshot_time - 3600, snapshot_time + 3600)),
    )
    .unwrap();
    println!(
        "[W11] window around the push -> hits={} not_matched={} unplaced={} answer_complete={}",
        around_the_push.hits.len(),
        around_the_push.not_matched,
        around_the_push.unplaced.len(),
        around_the_push.answer_complete()
    );
    assert_eq!(
        around_the_push.hits.len(),
        0,
        "the push happened in the window, but no conversation did — a session must not be selected by when it was backed up"
    );
    assert_eq!(around_the_push.not_matched, 3, "all three were evaluated");
    assert!(
        around_the_push.unplaced.is_empty(),
        "these sessions have known times, so nothing is unplaceable"
    );
    assert!(
        around_the_push.answer_complete(),
        "and therefore this zero really is a negative"
    );
    assert!(around_the_push
        .no_hit_line()
        .contains("not in this destination"));

    // ---- 3. the same sessions ARE found by a window around the conversation -
    let january = search_sessions(
        &store,
        &mk,
        &Selector::default().window(window_from(JAN_2024, JAN_2024 + 20 * DAY)),
    )
    .unwrap();
    println!(
        "[W11] window over Jan 2024 -> hits={} ({})",
        january.hits.len(),
        january
            .hits
            .iter()
            .map(|h| format!("{}/{}", h.machine, h.short_id()))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert_eq!(january.hits.len(), 2, "the two January sessions");
    assert_eq!(january.not_matched, 1, "the February one is out");

    // ---- 4. intersection, not containment ----------------------------------
    // The first session spans Jan 1–3. A one-day window on Jan 2 lies strictly
    // inside it and must still select it.
    let inside = search_sessions(
        &store,
        &mk,
        &Selector::default().window(window_from(JAN_2024 + DAY, JAN_2024 + DAY + 3600)),
    )
    .unwrap();
    println!(
        "[W11] window strictly inside a multi-day session -> hits={}",
        inside.hits.len()
    );
    assert_eq!(inside.hits.len(), 1);
    assert_eq!(inside.hits[0].machine, "m-alpha");

    // ---- 5. the boundaries count -------------------------------------------
    // A window ending exactly at the session's `first`, and one starting
    // exactly at its `last`, both intersect it.
    let ends_at_first = search_sessions(
        &store,
        &mk,
        &Selector::default().window(window_from(JAN_2024 - 10 * DAY, JAN_2024)),
    )
    .unwrap();
    let starts_at_last = search_sessions(
        &store,
        &mk,
        &Selector::default().window(window_from(JAN_2024 + 2 * DAY, JAN_2024 + 5 * DAY)),
    )
    .unwrap();
    println!(
        "[W11] window ending at first -> hits={} ; window starting at last -> hits={}",
        ends_at_first.hits.len(),
        starts_at_last.hits.len()
    );
    assert_eq!(ends_at_first.hits.len(), 1, "inclusive at the lower edge");
    assert_eq!(starts_at_last.hits.len(), 1, "inclusive at the upper edge");

    // One second later, and the same session is out.
    let misses_by_a_second = search_sessions(
        &store,
        &mk,
        &Selector::default().window(window_from(JAN_2024 - 10 * DAY, JAN_2024 - 1)),
    )
    .unwrap();
    assert_eq!(misses_by_a_second.hits.len(), 0);
    assert_eq!(misses_by_a_second.not_matched, 3);
}

/// A machine whose activity index is missing must not have its sessions read
/// as "outside the window", and one whose index exists but cannot be read must
/// make the whole answer incomplete rather than empty.
#[test]
fn a_missing_or_unreadable_index_is_never_an_absence() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let repo = root.join("repo");
    let mk = MasterKey::new();

    const WHEN: i64 = 1_704_067_200;
    const OTHER: i64 = 1_704_067_200 + 400 * 86_400;

    // m-indexed gets a real index; m-bare gets none at all.
    let indexed = "claude-code.m-indexed.aaaaaaaa-0000-0000-0000-000000000001";
    let bare = "claude-code.m-bare.bbbbbbbb-0000-0000-0000-000000000001";
    for (machine, session, with_index) in [("m-indexed", indexed, true), ("m-bare", bare, false)] {
        let stage = root.join(format!("stage-{machine}"));
        write_session_shards(&stage, machine, session, 7);
        if with_index {
            write_activity_index(&stage, machine, &[index_row(machine, session, WHEN, WHEN)]);
        }
        let store = BackupStore::new(cfg(&repo, &root.join("key.json")), machine.to_string());
        store.push(&stage, &mk).unwrap();
    }

    let store = BackupStore::new(cfg(&repo, &root.join("key.json")), "m-indexed".to_string());

    // A window on the indexed session's day: that one is a real match, and the
    // un-indexed machine's session is listed as unplaceable — never dropped,
    // and never counted as a non-match.
    let report = search_sessions(
        &store,
        &mk,
        &Selector::default().window(window_from(WHEN - 60, WHEN + 60)),
    )
    .unwrap();
    println!(
        "[W11] one machine indexed, one not -> hits={} not_matched={} unplaced={} without_index={:?} complete={} answer_complete={}",
        report.hits.len(),
        report.not_matched,
        report.unplaced.len(),
        report.machines_without_index,
        report.complete(),
        report.answer_complete()
    );
    assert_eq!(report.hits.len(), 1);
    assert_eq!(report.hits[0].session_id, indexed);
    assert_eq!(
        report.machines_without_index,
        vec!["m-bare".to_string()],
        "a machine with sessions but no index is named, not silently skipped"
    );
    assert_eq!(report.unplaced.len(), 1);
    assert_eq!(report.unplaced[0].session_id, bare);
    assert!(
        report.unplaced[0].why.contains("no activity index"),
        "the reason must name the missing index: {}",
        report.unplaced[0].why
    );
    assert!(
        report.complete(),
        "the destination itself was readable in full"
    );
    assert!(
        !report.answer_complete(),
        "…but the window is not answerable for every session, so exit 1 would be a lie"
    );

    // Without a time window nothing is unplaceable, so the same archive
    // answers completely — the missing index only matters when it is needed.
    let no_window = search_sessions(&store, &mk, &Selector::default()).unwrap();
    assert_eq!(no_window.hits.len(), 2);
    assert!(no_window.answer_complete());

    // A window that excludes the indexed session: the only real negative is
    // still not a complete answer, because m-bare remains unplaced.
    let elsewhere = search_sessions(
        &store,
        &mk,
        &Selector::default().window(window_from(OTHER, OTHER + 60)),
    )
    .unwrap();
    assert_eq!(elsewhere.hits.len(), 0);
    assert_eq!(elsewhere.unplaced.len(), 1);
    assert!(elsewhere.complete());
    assert!(!elsewhere.answer_complete());
    assert!(elsewhere.no_hit_line().contains("UNKNOWN"));
    assert!(!elsewhere.no_hit_line().contains("not in this destination"));

    let stage = root.join("stage-m-indexed");
    let index_path = stage
        .join("meta")
        .join("m-indexed")
        .join("activity-v1.jsonl");
    let push_again = || {
        let store = BackupStore::new(cfg(&repo, &root.join("key.json")), "m-indexed".to_string());
        store.push(&stage, &mk).unwrap();
    };

    // ---- a partially readable index ----------------------------------------
    // One good row and one unparseable line: the good row is still used (half
    // an answer is worth having), but the read is reported as incomplete, so
    // nobody concludes from it that the archive is fully understood.
    let mut partially = fs::read_to_string(&index_path).unwrap();
    partially.push_str("{ this is not JSON }\n");
    fs::write(&index_path, &partially).unwrap();
    push_again();
    let store = BackupStore::new(cfg(&repo, &root.join("key.json")), "m-indexed".to_string());

    let partial = search_sessions(
        &store,
        &mk,
        &Selector::default().window(window_from(WHEN - 60, WHEN + 60)),
    )
    .unwrap();
    println!(
        "[W11] partially malformed index -> hits={} unplaced={} unreadable={} complete={}",
        partial.hits.len(),
        partial.unplaced.len(),
        partial.unreadable.len(),
        partial.complete()
    );
    assert_eq!(
        partial.hits.len(),
        1,
        "the one row that parses is still used — a partial index is not thrown away"
    );
    assert!(
        !partial.complete(),
        "…but a partial index is a partial read, and must not look like a clean archive"
    );
    assert!(partial.unreadable.iter().any(|u| u.contains("malformed")));

    // ---- an index that parses as nothing at all ----------------------------
    // Now the machine has an index that cannot be used at all. Its session must
    // become unplaceable with a reason, and the destination must still be
    // reported as incompletely read — never as a machine with no sessions.
    fs::write(&index_path, "not jsonl at all\nneither is this\n").unwrap();
    push_again();
    let store = BackupStore::new(cfg(&repo, &root.join("key.json")), "m-indexed".to_string());
    let unusable = search_sessions(
        &store,
        &mk,
        &Selector::default().window(window_from(WHEN - 60, WHEN + 60)),
    )
    .unwrap();
    println!(
        "[W11] unusable index -> hits={} unplaced={} unreadable={} complete={} answer_complete={}",
        unusable.hits.len(),
        unusable.unplaced.len(),
        unusable.unreadable.len(),
        unusable.complete(),
        unusable.answer_complete()
    );
    assert_eq!(unusable.hits.len(), 0);
    assert!(
        unusable.unplaced.iter().any(|u| u.session_id == indexed),
        "the indexed session must be listed as unplaceable, not dropped"
    );
    assert!(unusable.unplaced.iter().any(|u| u.session_id == bare));
    assert!(
        !unusable.complete(),
        "an index that cannot be read is a part of the destination that could not be read"
    );
    assert!(!unusable.answer_complete());
}
