//! W271 · SRCH-1b — the snapshot session cache must be invisible.
//!
//! W257 made `search` cumulative over every snapshot of a hostname (ADR-021),
//! which is what makes a reclaimed conversation findable, and which costs one
//! tree walk per snapshot on every run: 5.8 s for the field archive's 417
//! snapshots locally (W257), and one round trip per tree blob over
//! `opendal:sftp`, so tens of seconds there. SRCH-1b puts the result of each
//! walk — the sessions one snapshot holds — in a local, disposable,
//! per-destination cache keyed by the snapshot's id, because a snapshot's id is
//! the hash of its own contents and therefore cannot change.
//!
//! The whole risk of a cache in a tool whose first invariant is "an unknown must
//! never be recorded as empty" is that it answers for something it has no right
//! to: a snapshot the repository no longer lists, a file whose bytes changed
//! under it, or a snapshot whose tree cannot be read *now*. Each of those is a
//! test below, and each is written so that a cache which trusted itself would
//! fail it rather than merely be slower:
//!
//! 1. `cached_and_uncached_searches_agree` — the cached path and the uncached
//!    path emit byte-identical `--json`, including `snapshots_scanned`, and the
//!    second run is answered from the cache rather than by walking again.
//! 2. `an_entry_for_a_pruned_snapshot_is_dropped` — a snapshot is removed from
//!    the repository, and the warm run must report the pruned archive, not the
//!    archive as it was. The entry for the pruned snapshot must be gone.
//! 3. `a_damaged_entry_is_rebuilt_and_never_trusted` — one byte of an entry is
//!    changed so that the JSON still parses but names a session that was never
//!    archived. A cache that trusted its own bytes would report that session.
//! 4. `an_unreadable_snapshot_stays_unreadable_with_a_cache` — the newest
//!    snapshot's packs are deleted, so its tree cannot be walked. Cached or not,
//!    the answer must stay "we did not finish reading this destination".
//! 5. `a_pack_lost_after_the_cache_was_written_is_not_answered_from_it` — the
//!    same damage, but done **after** a run has filled the cache. An entry says
//!    what a snapshot held; it does not say the destination can still read it,
//!    and no entry may be allowed to stand in for a walk that would fail.
//!
//! The one field every comparison here leaves out is `snapshots_from_cache`: it
//! counts what the cache served, so a cold run and a warm run *must* differ
//! there, and comparing answers means comparing everything else. It is asserted
//! separately in each test, so a difference cannot hide behind it.
//!
//! Everything is synthetic and local (temp dirs, generated payload): no real
//! conversation, account or hostname appears. Only counts, sizes and session id
//! prefixes are printed.

use chat_stasher::activity::{ActivityRow, TimeSource};
use chat_stasher::search::{report_json, search_sessions, SearchReport};
use chat_stasher::selector::Selector;
use chat_stasher::snapshot_cache::SnapshotCache;
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MACHINE: &str = "m-cache";
/// The three sessions the fixture archives, in the order they appear.
const ONE: &str = "s-one";
const TWO: &str = "s-two";
/// Only in the second snapshot.
const THREE: &str = "s-three";

fn store_config(dir: &Path, repo_name: &str) -> StoreConfig {
    StoreConfig {
        repo_root: dir.join(repo_name).to_string_lossy().into_owned(),
        key_file: dir.join(format!("{repo_name}.key")),
        connections: 1,
        options: BTreeMap::new(),
        // The repository's *metadata* cache must be off, or a search would read
        // trees out of rustic's own per-machine cache and the walk under test
        // would not happen at all.
        cache_dir: None,
        no_cache: true,
    }
}

/// Write `nshards` sealed shards for one session.
fn write_session(stage: &Path, session: &str, nshards: u64, salt: &str) {
    for seq in 1..=nshards {
        let lines: Vec<String> = (0..4)
            .map(|i| format!("{{\"seq\":{seq},\"i\":{i},\"s\":\"{salt}-{seq}-{i}\"}}"))
            .collect();
        store::write_sealed_shard(StageWriter::Collect, stage, MACHINE, session, &lines).unwrap();
    }
}

/// Write `meta/<machine>/activity-v1.jsonl` with one exact-time row per session.
fn write_activity_index(stage: &Path, sessions: &[&str], first: i64, last: i64) {
    let meta_dir = stage.join("meta").join(MACHINE);
    fs::create_dir_all(&meta_dir).unwrap();
    let mut body = String::new();
    for session in sessions {
        let row = ActivityRow {
            session_id: (*session).to_string(),
            machine: MACHINE.to_string(),
            harness: "claude-code".to_string(),
            first_unix: Some(first),
            last_unix: Some(last),
            line_count: 4,
            time_source: TimeSource::Exact,
            source_zone: None,
            title: None,
            provenance: None,
            account_keys: Vec::new(),
        };
        body.push_str(&serde_json::to_string(&row).unwrap());
        body.push('\n');
    }
    fs::write(meta_dir.join("activity-v1.jsonl"), body).unwrap();
}

fn push(cfg: &StoreConfig, stage: &Path, mk: &MasterKey) {
    let summary = BackupStore::new(cfg.clone(), MACHINE.to_string())
        .push(stage, mk)
        .unwrap();
    assert!(summary.snapshots_in_repo > 0, "fixture pushed nothing");
}

/// A store that reads through `cache`, and one that does not. The only
/// difference between the two search calls below is this line.
fn cached(cfg: &StoreConfig, cache: &Arc<SnapshotCache>) -> BackupStore {
    BackupStore::for_metadata_query(cfg.clone()).with_snapshot_cache(Some(cache.clone()))
}

fn uncached(cfg: &StoreConfig) -> BackupStore {
    BackupStore::for_metadata_query(cfg.clone())
}

/// The JSON a search emits, which is the whole user-visible answer: hits,
/// `snapshots_scanned`, the unreadable list and the "not in this destination"
/// verdict are all in it, so comparing it is comparing the answer.
fn json(report: &SearchReport) -> String {
    report_json(report, false)
}

/// The answer, with the one field that describes the **run** rather than the
/// archive taken out.
///
/// `snapshots_from_cache` counts what the cache served, so a cold run, a warm
/// run and a run with no cache at all report different numbers there and the
/// same answer for everything else. It is removed here and asserted on its own
/// in every test that compares two reports, so it can never be the field a real
/// difference hides behind — and it is asserted to be *present*, so a report
/// that stopped saying it cannot pass by being quiet.
fn answer(report: &SearchReport) -> serde_json::Value {
    let mut value: serde_json::Value =
        serde_json::from_str(&json(report)).expect("a report is one JSON object");
    assert!(
        value.get("snapshots_from_cache").is_some(),
        "the report must say how many snapshots came from the cache: {value}"
    );
    value
        .as_object_mut()
        .expect("a report is one JSON object")
        .remove("snapshots_from_cache");
    value
}

/// The entry file for `snapshot_id`, wherever this cache puts it.
fn entry_path(cache: &SnapshotCache, snapshot_id: &str) -> PathBuf {
    cache.root().join(format!("{snapshot_id}.json"))
}

/// The snapshot files the repository really holds, by walking its `snapshots`
/// directory rather than assuming a flat layout.
fn snapshot_files(repo: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![repo.join("snapshots")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Every pack file in the repository. Used to find the packs one push added:
/// their blobs are exactly the ones that push introduced.
fn pack_files(repo: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![repo.join("data")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// One host, two snapshots. Snapshot A holds `ONE` and `TWO`; snapshot B holds
/// all three, because the activity index is rewritten cumulatively on every run
/// exactly as `activity-index` does it.
struct Fixture {
    /// Held for the fixture's lifetime: dropping it deletes the repository.
    #[allow(
        dead_code,
        reason = "the temporay directory is kept alive by being owned here"
    )]
    dir: tempfile::TempDir,
    cfg: StoreConfig,
    mk: MasterKey,
    cache: Arc<SnapshotCache>,
}

fn fixture() -> Fixture {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let cfg = store_config(dir.path(), "repo");
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();

    write_session(&stage, ONE, 2, "one");
    write_session(&stage, TWO, 2, "two");
    write_activity_index(&stage, &[ONE, TWO], 1_700_000_000, 1_700_000_600);
    push(&cfg, &stage, &mk); // snapshot A

    write_session(&stage, THREE, 1, "three");
    write_activity_index(&stage, &[ONE, TWO, THREE], 1_700_090_000, 1_700_090_600);
    push(&cfg, &stage, &mk); // snapshot B

    // `stage` is moved into the struct after the pushes, so the struct's own
    // copy is what the pack helpers below take.
    let cache = Arc::new(SnapshotCache::at(dir.path().join("snapshot-cache")));
    Fixture {
        dir,
        cfg,
        mk,
        cache,
    }
}

impl Fixture {
    fn repo(&self) -> PathBuf {
        PathBuf::from(&self.cfg.repo_root)
    }

    fn search_cached(&self) -> SearchReport {
        search_sessions(
            &cached(&self.cfg, &self.cache),
            &self.mk,
            &Selector::default(),
        )
        .unwrap()
    }

    fn search_uncached(&self) -> SearchReport {
        search_sessions(&uncached(&self.cfg), &self.mk, &Selector::default()).unwrap()
    }
}

/// The cache must not be able to change an answer, and the second run must
/// actually be answered from it — a cache that is never consulted would pass an
/// equality assertion on its own.
#[test]
fn cached_and_uncached_searches_agree() {
    let f = fixture();

    let cold = f.search_cached();
    assert_eq!(
        f.cache.misses(),
        2,
        "both snapshots are walked on a cold cache"
    );
    assert_eq!(f.cache.hits(), 0);

    let warm = f.search_cached();
    assert_eq!(
        f.cache.hits(),
        2,
        "the warm run must be answered by the cache, not by another walk"
    );
    assert_eq!(
        f.cache.misses(),
        2,
        "a warm run adds no miss: nothing was walked again"
    );
    assert_eq!(f.cache.corrupt(), 0);

    let plain = f.search_uncached();

    println!(
        "[W271] json equality: uncached={} B, cold-cached={} B, warm-cached={} B; \
         sessions_seen={} scanned={}/{} from_cache={}/{}/{} hits={}",
        json(&plain).len(),
        json(&cold).len(),
        json(&warm).len(),
        warm.sessions_seen,
        warm.snapshots_scanned,
        warm.snapshots_in_repo,
        plain.snapshots_from_cache,
        cold.snapshots_from_cache,
        warm.snapshots_from_cache,
        warm.hits.len()
    );

    assert_eq!(
        answer(&cold),
        answer(&plain),
        "a cold cache must return exactly the uncached answer"
    );
    assert_eq!(
        answer(&warm),
        answer(&plain),
        "a warm cache must return exactly the uncached answer"
    );
    // And the count that *is* allowed to differ, asserted rather than ignored:
    // a cache that was never consulted would otherwise pass both comparisons
    // above without ever having served anything.
    assert_eq!(
        plain.snapshots_from_cache, 0,
        "an uncached run has no cache"
    );
    assert_eq!(cold.snapshots_from_cache, 0, "a cold cache serves nothing");
    assert_eq!(
        warm.snapshots_from_cache, 2,
        "a warm run is answered from the cache, and says so"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json(&warm)).unwrap()["snapshots_from_cache"],
        serde_json::json!(2),
        "and the --json report must carry it, not just the report struct"
    );

    // The numbers the answer is built from, asserted directly so a change to
    // the JSON shape cannot quietly stop this from covering them.
    assert_eq!(warm.snapshots_in_repo, 2);
    assert_eq!(warm.snapshots_scanned, 2);
    assert_eq!(warm.sessions_seen, 3);
    assert_eq!(warm.hits.len(), 3);
    assert!(warm.answer_complete(), "every snapshot was accounted for");
}

/// A snapshot the repository no longer lists must not be answered for, and must
/// not leave an entry behind.
///
/// The pruned snapshot is the **newest** one, so the surviving snapshot moves
/// from depth 1 to depth 0 — which is also what proves an entry is a property
/// of the snapshot alone and not of where in the walk order it was written: its
/// activity index path was recorded while it was not the newest, and the warm
/// run reads it while it is.
#[test]
fn an_entry_for_a_pruned_snapshot_is_dropped() {
    let f = fixture();

    let cold = f.search_cached();
    let newest = cold.hosts[0].snapshot_id.clone();
    let entry = entry_path(&f.cache, &newest);
    assert!(entry.exists(), "the cold run must have written an entry");
    assert_eq!(cold.sessions_seen, 3);

    // Prune it: remove the snapshot from the repository's own listing.
    let file = snapshot_files(&f.repo())
        .into_iter()
        .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(newest.as_str()))
        .expect("the newest snapshot file is in the repository");
    fs::remove_file(&file).unwrap();

    let warm = f.search_cached();

    // What the pruned archive really holds, read with no cache at all.
    let plain = f.search_uncached();

    println!(
        "[W271] after pruning snapshot {}: uncached sessions_seen={} hits={} ; \
         warm sessions_seen={} hits={} ; index_files_read={}/{} ; entry_present={}",
        &newest[..8],
        plain.sessions_seen,
        plain.hits.len(),
        warm.sessions_seen,
        warm.hits.len(),
        plain.index_files_read,
        warm.index_files_read,
        entry.exists()
    );

    assert_eq!(warm.snapshots_in_repo, 1, "the pruned listing");
    assert_eq!(plain.sessions_seen, 2, "the pruned archive holds two");
    assert_eq!(
        answer(&warm),
        answer(&plain),
        "the warm run must answer for the archive that is there now"
    );
    assert!(
        !entry.exists(),
        "an entry for a snapshot the repository no longer lists must be dropped"
    );
    assert_eq!(f.cache.pruned(), 1);
    assert_eq!(
        warm.index_files_read, 1,
        "the surviving snapshot's index must still be read: its path was \
         recorded when it was not the newest snapshot"
    );
    assert!(warm.answer_complete());
}

/// A file whose bytes changed under it must never be trusted, even when it
/// still parses.
///
/// The byte changed here keeps the JSON valid and changes a session id, so a
/// cache that read its own bytes would report a session that was never
/// archived — a wrong answer, not a slow one. The digest in the entry header is
/// what stands between those two outcomes.
#[test]
fn a_damaged_entry_is_rebuilt_and_never_trusted() {
    let f = fixture();

    let cold = f.search_cached();
    let newest = cold.hosts[0].snapshot_id.clone();
    let entry = entry_path(&f.cache, &newest);
    let before = fs::read(&entry).unwrap();

    // Five bytes for five: `s-one` -> `s-ONE` inside `sessions[].session`.
    // The length, the magic and the JSON syntax are all untouched.
    let needle = ONE.as_bytes();
    let at = before
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("a session id is spelled in the entry");
    let mut damaged = before.clone();
    damaged[at..at + needle.len()].copy_from_slice(b"s-ONE");
    assert_ne!(damaged, before);
    assert!(
        serde_json::from_slice::<serde_json::Value>(&damaged[48..]).is_ok(),
        "the damage must leave a parseable body, or this proves nothing"
    );
    fs::write(&entry, &damaged).unwrap();

    let warm = f.search_cached();
    let plain = f.search_uncached();

    println!(
        "[W271] one entry byte changed: corrupt={} misses={} hits={} ; \
         hits={} ; rebuilt={}",
        f.cache.corrupt(),
        f.cache.misses(),
        f.cache.hits(),
        warm.hits.len(),
        fs::read(&entry).unwrap() == before
    );

    assert_eq!(f.cache.corrupt(), 1, "the damaged entry must be rejected");
    assert_eq!(
        answer(&warm),
        answer(&plain),
        "a damaged entry must be rebuilt, never trusted"
    );
    assert_eq!(warm.hits.len(), 3);
    assert_eq!(
        fs::read(&entry).unwrap(),
        before,
        "the run that noticed the damage must have rewritten the entry"
    );
}

/// "A snapshot whose tree cannot be read" must survive the cache: the cached
/// run and the uncached run must both say the destination was not finished,
/// with the same `snapshots scanned N of M` — never that a partial reading was
/// a complete one.
///
/// The newest snapshot's packs are deleted, so its tree cannot be walked while
/// the older snapshot still reads perfectly. That also means no entry is ever
/// written for it: a snapshot that could not be read is retried on every run
/// rather than remembered as unreadable.
#[test]
fn an_unreadable_snapshot_stays_unreadable_with_a_cache() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let cfg = store_config(dir.path(), "repo");
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();

    write_session(&stage, ONE, 2, "one");
    write_activity_index(&stage, &[ONE], 1_700_000_000, 1_700_000_600);
    push(&cfg, &stage, &mk); // snapshot A · readable

    let before = pack_files(Path::new(&cfg.repo_root));
    write_session(&stage, THREE, 1, "three");
    write_activity_index(&stage, &[ONE, THREE], 1_700_090_000, 1_700_090_600);
    push(&cfg, &stage, &mk); // snapshot B · about to be unreadable
    let after = pack_files(Path::new(&cfg.repo_root));
    let added: Vec<PathBuf> = after.into_iter().filter(|p| !before.contains(p)).collect();
    assert!(!added.is_empty(), "the second push wrote no pack");
    for pack in &added {
        fs::remove_file(pack).unwrap();
    }

    let cache = Arc::new(SnapshotCache::at(dir.path().join("snapshot-cache")));
    let cold = search_sessions(&cached(&cfg, &cache), &mk, &Selector::default()).unwrap();
    // Counted around the warm run alone: the cold run had to walk both
    // snapshots, so a running total says nothing about what the warm one did.
    let hits_before = cache.hits();
    let misses_before = cache.misses();
    let warm = search_sessions(&cached(&cfg, &cache), &mk, &Selector::default()).unwrap();
    let plain = search_sessions(&uncached(&cfg), &mk, &Selector::default()).unwrap();

    println!(
        "[W271] damaged newest snapshot: unreadable(cold)={} scanned={}/{} ; \
         unreadable(warm)={} scanned={}/{} ; warm hit={} miss={}",
        cold.unreadable.len(),
        cold.snapshots_scanned,
        cold.snapshots_in_repo,
        warm.unreadable.len(),
        warm.snapshots_scanned,
        warm.snapshots_in_repo,
        cache.hits() - hits_before,
        cache.misses() - misses_before
    );

    assert_eq!(cold.snapshots_in_repo, 2);
    assert_eq!(
        cold.snapshots_scanned, 1,
        "the damaged snapshot is not read"
    );
    assert_eq!(cold.unreadable.len(), 1);
    assert!(!cold.scanned_all_snapshots());
    assert!(!cold.complete(), "a partial reading is not a finished one");

    assert_eq!(
        answer(&warm),
        answer(&plain),
        "a cache must not turn an unreadable snapshot into a readable one"
    );
    assert_eq!(warm.snapshots_scanned, 1);
    assert_eq!(warm.unreadable.len(), 1);
    assert!(!warm.complete());
    assert_eq!(
        cache.hits() - hits_before,
        1,
        "the readable snapshot is served from the cache; the unreadable one is walked again"
    );
    assert_eq!(
        cache.misses() - misses_before,
        1,
        "a snapshot with no entry is walked every run, never remembered as unreadable"
    );
    assert!(
        !entry_present_for(&cache, &cold),
        "no entry may be written for a snapshot that could not be read"
    );
}

/// A pack lost **after** the cache was written must not be answered from it.
///
/// This is the direction the test above cannot reach: there the damage is done
/// first, so no entry for the damaged snapshot is ever written, and a cache
/// that trusted its own bytes would pass it. Here every snapshot is walked and
/// cached while the archive is healthy, and only then does the destination lose
/// its packs.
///
/// A snapshot's id proves its *contents* cannot change. It says nothing about
/// whether the destination still holds them, and the uncached path answers that
/// question by failing: its walk cannot fetch a tree, so the snapshot lands in
/// `unreadable` and the answer comes back partial. A hit accepted on the
/// strength of its own bytes would report the same archive as complete — a
/// wrong answer, not a slower one, and the one thing this cache may never do.
/// The failure it must reach instead is the same one, in the same words, that
/// the uncached run gives.
#[test]
fn a_pack_lost_after_the_cache_was_written_is_not_answered_from_it() {
    let f = fixture();

    // 1 · a healthy run fills the cache: both snapshots are walked and written.
    let cold = f.search_cached();
    assert_eq!(cold.snapshots_scanned, 2, "both snapshots were walked");
    assert_eq!(cold.snapshots_from_cache, 0, "nothing was cached yet");
    assert!(cold.answer_complete(), "the archive is readable so far");
    for host in &cold.hosts {
        assert!(
            entry_path(&f.cache, &host.snapshot_id).exists(),
            "every snapshot has an entry before the damage"
        );
    }

    // 2 · the destination loses every pack it holds. Nothing recreates them;
    //     the snapshot files, the key and the index files are all still there,
    //     which is exactly what a partially lost destination looks like.
    let packs = pack_files(&f.repo());
    assert!(!packs.is_empty(), "the fixture wrote no packs");
    for pack in &packs {
        fs::remove_file(pack).unwrap();
    }

    // 3 · the warm run must answer what the archive is *now*: unreadable on
    //     every snapshot, from the cache or not.
    let warm = f.search_cached();
    let plain = f.search_uncached();

    println!(
        "[W271] every pack removed after a warm cache: \
         warm scanned={}/{} unreadable={} from_cache={} ; \
         uncached scanned={}/{} unreadable={}",
        warm.snapshots_scanned,
        warm.snapshots_in_repo,
        warm.unreadable.len(),
        warm.snapshots_from_cache,
        plain.snapshots_scanned,
        plain.snapshots_in_repo,
        plain.unreadable.len()
    );

    assert_eq!(
        answer(&warm),
        answer(&plain),
        "a cache must not answer for a destination that can no longer be read"
    );
    assert_eq!(
        warm.snapshots_scanned, 0,
        "no snapshot could be walked, so none was accounted for"
    );
    assert_eq!(
        warm.snapshots_from_cache, 0,
        "an entry whose trees are gone is not an answer"
    );
    assert_eq!(
        warm.unreadable.len(),
        warm.snapshots_in_repo,
        "every snapshot is reported as unreadable, not silently skipped"
    );
    assert!(!warm.complete());
    assert!(!warm.answer_complete());
    assert_eq!(
        warm.sessions_seen, 0,
        "and the sessions the entries still name are NOT reported: they are \
         exactly what a cache trusting itself would have answered with"
    );
}

/// The figure SRCH-1b exists for, at the scale the archive actually has: what
/// one search costs over **400+ snapshots**, with and without the cache.
///
/// W257 measured the uncached run at this scale (417 snapshots, 5.8 s locally,
/// 13.6 ms per snapshot) and the number is the reason for the cache: over
/// `opendal:sftp` every one of those snapshots is at least one round trip, so
/// the same search is tens of seconds. This builds the same shape at 417 and
/// reports the three runs side by side — uncached, cached cold, cached warm —
/// on one repository and one machine, so the comparison is not across
/// fixtures.
///
/// `#[ignore]`d because building 417 snapshots is minutes of work: it is a
/// measurement, not a gate. Run it explicitly:
///
/// ```text
/// cargo test -p chat-stasher --test w271_snapshot_cache_test -- --ignored --nocapture field_scale
/// ```
///
/// The assertions are the cheap structural ones, so a harness that stopped
/// consulting the cache cannot go quiet: the warm run must issue **zero** tree
/// walks and must still return the uncached answer byte for byte.
#[test]
#[ignore = "manual wall-clock measurement; builds 417 snapshots"]
fn field_scale_cold_against_warm_over_four_hundred_snapshots() {
    /// The field archive's own count (`mac`, W253 §B).
    const SNAPSHOTS: usize = 417;
    /// Enough sessions that the per-session term is visible without making the
    /// build an hour long. The field machine indexes 3178.
    const SESSIONS: usize = 40;

    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    // rustic's own metadata cache on, as a configured run has: without it the
    // uncached baseline would be a worst case rather than what an operator
    // sees, and the comparison against the snapshot cache would flatter it.
    let cfg = StoreConfig {
        cache_dir: Some(dir.path().join("rustic-cache")),
        no_cache: false,
        ..store_config(dir.path(), "repo")
    };
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();

    let sessions: Vec<String> = (0..SESSIONS)
        .map(|i| format!("claude-code.{MACHINE}.00000000-0000-0000-0000-{i:012}"))
        .collect();
    let names: Vec<&str> = sessions.iter().map(String::as_str).collect();
    for (i, session) in sessions.iter().enumerate() {
        write_session(&stage, session, 1, &format!("field-{i}"));
    }

    // One push per "run", each rewriting the activity index the way a real run
    // does, so every snapshot's tree really is a different tree.
    let started = std::time::Instant::now();
    for run in 0..SNAPSHOTS {
        write_activity_index(
            &stage,
            &names,
            1_700_000_000 + (run as i64) * 86_400,
            1_700_000_600 + (run as i64) * 86_400,
        );
        push(&cfg, &stage, &mk);
    }
    let build_ms = started.elapsed().as_millis();

    // Warm the repository's own metadata cache once, so every measurement below
    // is a repeated run rather than the first open of a cold repository.
    let _warm_up = search_sessions(&uncached(&cfg), &mk, &Selector::default()).unwrap();

    let started = std::time::Instant::now();
    let plain = search_sessions(&uncached(&cfg), &mk, &Selector::default()).unwrap();
    let plain_ms = started.elapsed().as_millis();

    let cache = Arc::new(SnapshotCache::at(dir.path().join("snapshot-cache")));
    let started = std::time::Instant::now();
    let cold = search_sessions(&cached(&cfg, &cache), &mk, &Selector::default()).unwrap();
    let cold_ms = started.elapsed().as_millis();
    let cold_walks = cache.misses();
    let cold_hits = cache.hits();

    // Counted around the warm run alone: a running total says nothing about
    // what the run that is being reported actually did.
    let hits_before = cache.hits();
    let misses_before = cache.misses();
    let started = std::time::Instant::now();
    let warm = search_sessions(&cached(&cfg, &cache), &mk, &Selector::default()).unwrap();
    let warm_ms = started.elapsed().as_millis();
    let warm_hits = cache.hits() - hits_before;
    let warm_walks = cache.misses() - misses_before;

    let entry_bytes: u64 = fs::read_dir(cache.root())
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();

    println!(
        "[W271] FIELD SCALE: snapshots={SNAPSHOTS} sessions={SESSIONS} (build {build_ms}ms)\n\
         [W271]   search, no snapshot cache = {plain_ms}ms\n\
         [W271]   search, cache cold        = {cold_ms}ms  ({cold_walks} snapshots walked, {:.2}ms each)\n\
         [W271]   search, cache warm        = {warm_ms}ms  ({warm_hits} entries read, {warm_walks} walks)\n\
         [W271]   speedup warm vs uncached  = {:.1}x  ; the cache is {entry_bytes} B on disk\n\
         [W271]   data_blobs_read: plain={} cold={} warm={} ; index_files_read={} / {} / {}",
        cold_ms as f64 / cold_walks.max(1) as f64,
        plain_ms as f64 / warm_ms.max(1) as f64,
        plain.data_blobs_read,
        cold.data_blobs_read,
        warm.data_blobs_read,
        plain.index_files_read,
        cold.index_files_read,
        warm.index_files_read,
    );

    // The properties, as opposed to the timings.
    assert_eq!(plain.snapshots_in_repo, SNAPSHOTS);
    assert_eq!(plain.snapshots_scanned, SNAPSHOTS);
    assert_eq!(warm.snapshots_scanned, SNAPSHOTS);
    assert_eq!(plain.hits.len(), SESSIONS);
    assert_eq!(warm.hits.len(), SESSIONS);
    assert_eq!(
        answer(&warm),
        answer(&plain),
        "417 snapshots answered from the cache must equal 417 snapshots walked"
    );
    assert_eq!(
        warm.snapshots_from_cache, SNAPSHOTS,
        "every one of them is answered from the cache, and the report says so"
    );
    assert_eq!(
        cold_hits, 0,
        "a cold cache has nothing to serve, so the cold run must walk every snapshot"
    );
    assert_eq!(
        warm_walks, 0,
        "a warm run over an unchanged repository walks nothing at all"
    );
    assert_eq!(
        warm_hits, SNAPSHOTS as u64,
        "every snapshot is answered by its own entry"
    );
    assert_eq!(plain.data_blobs_read, 0, "no conversation body is fetched");
    assert_eq!(warm.data_blobs_read, 0);
    assert_eq!(warm.index_files_read, 1, "the index is still read, once");
}

/// Whether any entry file exists for the snapshot the cold run found unreadable.
///
/// The unreadable one is the newest, so it is the id in `hosts[0]`; the check is
/// on the file rather than on a counter because "we stored nothing for it" is
/// the property, and a counter could be right while the file is there.
fn entry_present_for(cache: &SnapshotCache, cold: &SearchReport) -> bool {
    entry_path(cache, &cold.hosts[0].snapshot_id).exists()
}
