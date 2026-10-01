//! W257 · SRCH-1 — `search` and `read --session` must be cumulative over a
//! machine's snapshots, not bounded by its newest one.
//!
//! The failure this pins was measured on the real archive (W253): `reclaim-stage`
//! deletes a session's shard bodies from the stage once every destination has
//! proved it holds them, so a busy machine's **newest** snapshot holds only the
//! last push's batch. On m3 that was 16 sessions out of the 3178 its activity
//! index records. `search` looked at the newest snapshot only, so it answered
//! `not in this destination` — exit 1, `answer_complete: true` — about a
//! destination that held the conversation.
//!
//! ADR-021 had already made the other readers cumulative (`read --all-machines`,
//! `verify` L3); `search` and `read --session` were left behind. The fixtures
//! below reproduce the end state that exposes it, using the **real**
//! `reclaim_stage` rather than a hand-emptied stage, so the state under test is
//! the one the product produces:
//!
//! 1. two sessions pushed (snapshot A holds both bodies);
//! 2. `reclaim-stage` proves the destination holds them and deletes the bodies;
//! 3. pushed again (snapshot B holds the directories, no bodies);
//! 4. one session continues afterwards, so a third push (snapshot C) holds a
//!    *newer, shorter* copy of it.
//!
//! Then: `search` finds the reclaimed session and reports it against the
//! snapshot that actually holds it; `read --session` reads it back
//! byte-identical **without the source machine's stage root** (W253 C3);
//! `--json` emits exactly one parseable object (W253 C1).
//!
//! Everything is synthetic and local (temp dirs, generated payload): no real
//! conversation, account or hostname appears.

use chat_stasher::activity::{ActivityRow, TimeSource};
use chat_stasher::search::{report_json, search_sessions, SearchReport};
use chat_stasher::selector::Selector;
use chat_stasher::stagereclaim::{self, NamedStore};
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::Command;

#[path = "../src/test_support.rs"]
mod test_support;

const MACHINE: &str = "m-cumulative";
/// Reclaimed before the second push: its only body is in snapshot A.
const RECLAIMED: &str = "s-reclaimed-only";
/// Reclaimed, then continued, so snapshot C holds a newer copy of it.
const CONTINUED: &str = "s-continued";

fn store_config(dir: &Path, repo_name: &str) -> StoreConfig {
    StoreConfig {
        repo_root: dir.join(repo_name).to_string_lossy().into_owned(),
        key_file: dir.join(format!("{repo_name}.key")),
        connections: 1,
        options: BTreeMap::new(),
        // Tests must not share the user's rustic cache: a cached pack would
        // let a body that is no longer in the newest snapshot still read back
        // clean, which is the whole thing under test.
        cache_dir: None,
        no_cache: true,
    }
}

/// [`store_config`], but with a metadata cache rooted inside this test's own
/// temp directory. Only the cost measurement uses it: a configured run has a
/// cache (rustic's tree cache makes a repeated subtree walk cheap), so a
/// no-cache timing is a worst case rather than what an operator sees.
fn cached_store_config(dir: &Path, repo_name: &str) -> StoreConfig {
    StoreConfig {
        cache_dir: Some(dir.join(format!("cache-{repo_name}"))),
        no_cache: false,
        ..store_config(dir, repo_name)
    }
}

fn named(dir: &Path, name: &str) -> NamedStore {
    NamedStore {
        name: name.to_string(),
        cfg: store_config(dir, name),
    }
}

/// Write `nshards` sealed shards for one session. Returns the bytes the session
/// concatenates to (each line followed by exactly one newline).
fn write_session(stage: &Path, session: &str, nshards: u64, salt: &str) -> Vec<u8> {
    let mut concat = Vec::new();
    for seq in 1..=nshards {
        let lines: Vec<String> = (0..8)
            .map(|i| format!("{{\"seq\":{seq},\"i\":{i},\"s\":\"{salt}-{seq}-{i}\"}}"))
            .collect();
        store::write_sealed_shard(StageWriter::Collect, stage, MACHINE, session, &lines).unwrap();
        for line in &lines {
            concat.extend_from_slice(line.as_bytes());
            concat.push(b'\n');
        }
    }
    concat
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
            line_count: 8,
            time_source: TimeSource::Exact,
            source_zone: None,
            title: None,
            provenance: None,
            account_keys: Vec::new(),
            measured_body: None,
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
    assert!(
        summary.snapshots_in_repo > 0,
        "fixture pushed no snapshot at all"
    );
}

/// A `chat-stasher` binary run with fully isolated XDG/home env.
fn isolated_command(sandbox: &Path) -> Command {
    let home = sandbox.join("home");
    fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-stasher"));
    command
        .env("HOME", &home)
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

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The fixture every assertion below shares: A (both bodies) → reclaim → B (no
/// bodies) → continue one session → C (a body for that one).
struct Fixture {
    dir: tempfile::TempDir,
    cfg: StoreConfig,
    mk: MasterKey,
    /// What `RECLAIMED` concatenates to, as written before the first push.
    reclaimed_bytes: Vec<u8>,
    /// What `CONTINUED` concatenates to in snapshot **A** — the older, longer
    /// copy that a walk reaching past a broken newer snapshot would hand back.
    continued_old_bytes: Vec<u8>,
    /// What `CONTINUED` concatenates to in snapshot C.
    continued_bytes: Vec<u8>,
}

/// Every pack file in the repository, as a set of paths. Used to find the packs
/// a given push added: their blobs are exactly the ones that push introduced.
fn pack_files(repo: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
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
                out.insert(path.to_string_lossy().into_owned());
            }
        }
    }
    out
}

fn fixture() -> Fixture {
    fixture_with_a_damaged_newest_snapshot(false)
}

/// The [`fixture`], with the **newest** snapshot made unreadable: the packs
/// the last push added are deleted, so its tree cannot be walked while the
/// older snapshots still read perfectly.
///
/// That is the exact shape the HIGH finding is about. Snapshot C holds a
/// **newer, shorter** copy of `CONTINUED` (and may hold anything at all), and it
/// is newer than the snapshot that holds the copy a walk would fall back to.
/// Because C cannot be read, "the session's current bytes" is unknowable — and
/// the wrong answer, an older copy returned as if it were current, is
/// indistinguishable from the right one at the call site.
///
/// Deleting the packs rather than truncating them is deliberate:
/// `require_sound_packs` refuses a pack that is present-but-shorter than the
/// index records, which would fail every read for a reason that is not the one
/// under test. An *absent* pack is explicitly not that check's question — its
/// read returns `Err`, so nothing slices anything — and that is the corrupted
/// state a real interrupted or pruned backend leaves.
fn fixture_with_a_damaged_newest_snapshot(damage: bool) -> Fixture {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let cfg = store_config(dir.path(), "repo");
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();

    let reclaimed_bytes = write_session(&stage, RECLAIMED, 2, "reclaimed");
    let continued_old_bytes = write_session(&stage, CONTINUED, 2, "continued-old");
    write_activity_index(
        &stage,
        &[RECLAIMED, CONTINUED],
        1_700_000_000,
        1_700_000_600,
    );
    push(&cfg, &stage, &mk); // snapshot A

    // The real thing: the destination proves it holds both bodies, so the stage
    // gives them up. This is what leaves the newest snapshot body-less.
    let dests = vec![named(dir.path(), "repo")];
    let report = stagereclaim::reclaim_stage(&stage, &dests, true).unwrap();
    assert!(!report.blocked(), "reclaim was blocked: {report:?}");
    assert_eq!(
        report.reclaimed.len(),
        2,
        "both sessions must be reclaimed for this fixture to mean anything"
    );
    // Prove the premise instead of assuming it: the body really is gone from
    // the stage, so the next snapshot cannot carry it.
    assert!(
        store::sealed_shard_entries(&store::session_shard_dir(&stage, MACHINE, RECLAIMED))
            .unwrap()
            .is_empty(),
        "the reclaimed session must have no shards left on the stage"
    );
    push(&cfg, &stage, &mk); // snapshot B: directories, no bodies

    // One session continues. Its sequence keeps going (never back onto the
    // archived names), so snapshot C holds a *newer and shorter* copy.
    let before_last = pack_files(Path::new(&cfg.repo_root));
    let continued_bytes = write_session(&stage, CONTINUED, 1, "continued-new");
    push(&cfg, &stage, &mk); // snapshot C

    if damage {
        let added: Vec<String> = pack_files(Path::new(&cfg.repo_root))
            .into_iter()
            .filter(|pack| !before_last.contains(pack))
            .collect();
        assert!(
            !added.is_empty(),
            "the last push must have added a pack; with none there is nothing to damage, and \
             this fixture would test nothing"
        );
        for pack in &added {
            fs::remove_file(pack).unwrap();
        }
    }

    Fixture {
        dir,
        cfg,
        mk,
        reclaimed_bytes,
        continued_old_bytes,
        continued_bytes,
    }
}

/// The core of SRCH-1: a session whose body only an older snapshot holds must
/// be found, and reported against the snapshot that holds it.
#[test]
fn search_finds_a_session_only_an_older_snapshot_holds() {
    let f = fixture();
    let store = BackupStore::new(f.cfg.clone(), MACHINE.to_string());

    let all = search_sessions(&store, &f.mk, &Selector::default()).unwrap();
    println!(
        "[W257] unfiltered: in_repo={} scanned={} seen={} hits={} complete={} answer_complete={}",
        all.snapshots_in_repo,
        all.snapshots_scanned,
        all.sessions_seen,
        all.hits.len(),
        all.complete(),
        all.answer_complete()
    );
    for h in &all.hits {
        println!(
            "[W257]   hit machine={} session={} shards={} snapshot={}",
            h.machine,
            h.short_id(),
            h.shard_count,
            h.short_snapshot()
        );
    }

    assert_eq!(all.snapshots_in_repo, 3, "A, B and C");
    assert_eq!(
        all.snapshots_scanned, all.snapshots_in_repo,
        "every snapshot must be walked — the body-less newest one is not the archive"
    );
    assert!(all.scanned_all_snapshots());
    assert_eq!(
        all.data_blobs_read, 0,
        "a metadata search must not fetch a conversation blob, however many snapshots it walks"
    );
    // The index is read once per machine, not once per snapshot: it is rebuilt
    // cumulatively, so the newest one already names every session.
    assert_eq!(all.index_files_read, 1);

    let reclaimed = all
        .hits
        .iter()
        .find(|h| h.session_id == RECLAIMED)
        .expect("the reclaimed session must be found, not dropped");
    assert_eq!(
        reclaimed.shard_count, 2,
        "the shard count comes from the snapshot that holds the body"
    );
    assert_eq!(
        reclaimed.snapshot_id.len(),
        64,
        "the hit must name a real snapshot id"
    );

    // Newest appearance wins: the continued session is reported against C, with
    // C's (shorter) shard set — not against A's older, longer copy.
    let continued = all
        .hits
        .iter()
        .find(|h| h.session_id == CONTINUED)
        .expect("the continued session must be found");
    assert_eq!(continued.shard_count, 1);
    assert_ne!(
        continued.snapshot_id, reclaimed.snapshot_id,
        "the two sessions live in different snapshots and must say so"
    );

    // A prefix query — the `search --session <id>` shape from the field test —
    // answers with the session that the archive holds.
    let by_prefix = search_sessions(
        &store,
        &f.mk,
        &Selector::default().session_id_prefix(&RECLAIMED[..4]),
    )
    .unwrap();
    assert_eq!(by_prefix.hits.len(), 1, "{:?}", by_prefix.hits.len());
    assert_eq!(by_prefix.hits[0].session_id, RECLAIMED);
    assert!(by_prefix.answer_complete());

    // This fixture's index was written before snapshot A and never rebuilt, so
    // it still carries a row with real bounds for the reclaimed session and the
    // session is placed in time. On the real archive that row is rewritten by
    // the next `activity-index` run *after* the reclaim, which records
    // `line_count: 0`, and a window query then cannot place the session — the
    // activity-index half of the same problem (SRCH-2, filed separately, not
    // fixed here). What this test pins is the half that was making sessions
    // invisible: the body is found again.
    assert_eq!(
        reclaimed.first_unix,
        Some(1_700_000_000),
        "the index row is what places a session in time, and it is unchanged by this fix"
    );
}

/// `read --session` must hand back the archived bytes for a session the newest
/// snapshot does not hold, addressed only by `(machine, session id)`.
#[test]
fn read_session_reads_a_reclaimed_body_back_byte_identical() {
    let f = fixture();
    let store = BackupStore::new(f.cfg.clone(), MACHINE.to_string());

    let (from_archive, hashes) = store
        .read_session_concat(MACHINE, RECLAIMED, &f.mk)
        .expect("a session the archive holds must read back, whatever the stage looks like");
    assert_eq!(
        from_archive, f.reclaimed_bytes,
        "the archived bytes must be the ones the session was pushed with"
    );
    assert_eq!(hashes.len(), 2, "one hash per shard, in sequence order");
    assert_eq!(
        hashes.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        vec!["000001.jsonl", "000002.jsonl"],
        "shards come back in sequence order, under their archived names"
    );

    // The continued session resolves to the *newest* copy — the same rule the
    // search reports it under.
    let (continued, _) = store
        .read_session_concat(MACHINE, CONTINUED, &f.mk)
        .unwrap();
    assert_eq!(continued, f.continued_bytes);

    // A machine with no snapshot at all is a different answer from "no body".
    let err = store
        .read_session_concat("no-such-machine", RECLAIMED, &f.mk)
        .expect_err("an unknown machine must be an error, not an empty read");
    assert!(err.to_string().contains("no snapshot for machine"), "{err}");
}

/// The CLI half of C3: `read --session` must work with **no** `--stage`, and
/// C1: `--json` stdout must be exactly one JSON object even when the ssh-master
/// teardown has something to say.
#[test]
fn cli_read_needs_no_stage_and_search_json_stays_parseable() {
    let f = fixture();

    // ---- C3: a session read with no stage root at all ---------------------
    let read = isolated_command(f.dir.path())
        .args(["read", "--repo"])
        .arg(&f.cfg.repo_root)
        .args(["--key-file"])
        .arg(&f.cfg.key_file)
        .args(["--machine", MACHINE, "--session", RECLAIMED])
        .output()
        .unwrap();
    let read_out = String::from_utf8_lossy(&read.stdout).into_owned();
    println!("[W257] read (no --stage) exit={:?}", read.status.code());
    assert_eq!(
        read.status.code(),
        Some(0),
        "read failed:\nstdout={read_out}\nstderr={}",
        String::from_utf8_lossy(&read.stderr)
    );
    assert!(
        read_out.contains(&format!("sha256={}", sha256_hex(&f.reclaimed_bytes))),
        "the read must print the archived session's own digest:\n{read_out}"
    );
    assert!(
        read_out.contains("[read] shards (seq order):"),
        "the shard listing must survive:\n{read_out}"
    );
    assert!(
        read_out.contains("expected src    : not compared"),
        "with no --stage there is nothing local to compare against, and the line must say \
         so rather than print a placeholder digest:\n{read_out}"
    );

    // A stage root the operator names is a comparison aid, never a requirement,
    // and a wrong one must not turn the read into a failure.
    let read_wrong_stage = isolated_command(f.dir.path())
        .args(["read", "--repo"])
        .arg(&f.cfg.repo_root)
        .args(["--key-file"])
        .arg(&f.cfg.key_file)
        .args(["--machine", MACHINE, "--session", RECLAIMED, "--stage"])
        .arg(f.dir.path().join("definitely-not-the-source-stage"))
        .output()
        .unwrap();
    assert_eq!(
        read_wrong_stage.status.code(),
        Some(0),
        "a wrong --stage must not fail the read:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&read_wrong_stage.stdout),
        String::from_utf8_lossy(&read_wrong_stage.stderr)
    );

    // ---- C1: `--json` stdout is exactly one object ------------------------
    //
    // `--keep-ssh-masters` makes the ssh-master teardown print a line on every
    // run, including a local repository with no ssh involved. That line used to
    // go to stdout, so `search --json | jq` failed on the trailing text.
    let json = isolated_command(f.dir.path())
        .args(["search", "--repo"])
        .arg(&f.cfg.repo_root)
        .args(["--key-file"])
        .arg(&f.cfg.key_file)
        .args(["--json", "--keep-ssh-masters"])
        .output()
        .unwrap();
    let json_out = String::from_utf8_lossy(&json.stdout).into_owned();
    let parsed: serde_json::Value = serde_json::from_str(&json_out).unwrap_or_else(|e| {
        panic!(
            "stdout must be exactly one JSON object ({e}); the ssh-master teardown line \
             belongs on stderr. Got:\n{json_out}"
        )
    });
    assert!(
        String::from_utf8_lossy(&json.stderr).contains("[reap]"),
        "the teardown line must still be reported, on stderr:\n{}",
        String::from_utf8_lossy(&json.stderr)
    );
    assert_eq!(parsed["snapshots_scanned"], 3);
    assert_eq!(parsed["snapshots_in_repo"], 3);
    assert_eq!(parsed["snapshots_all_scanned"], true);
    assert_eq!(parsed["answer_complete"], true);
    assert_eq!(parsed["complete"], true);
    let sessions = parsed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    assert!(sessions
        .iter()
        .any(|s| s["session_short_id"].as_str().is_some()));
    assert!(
        parsed["data_blobs_read"] == 0,
        "the metadata tier reads no payload:\n{json_out}"
    );

    // ---- the negative is still reachable, and now says how much it scanned --
    //
    // The other edge: a session nobody has. This may still print "not in this
    // destination" — and it is the only shape allowed to, because every
    // snapshot of the destination was walked. The line has to carry that count,
    // or the reader cannot tell it from the old 3-of-417 answer.
    let absent = isolated_command(f.dir.path())
        .args(["search", "--repo"])
        .arg(&f.cfg.repo_root)
        .args(["--key-file"])
        .arg(&f.cfg.key_file)
        .args(["--session", "zzzz-no-such-session"])
        .output()
        .unwrap();
    let absent_out = String::from_utf8_lossy(&absent.stdout).into_owned();
    println!("[W257] absent session -> exit={:?}", absent.status.code());
    assert_eq!(
        absent.status.code(),
        Some(1),
        "a session that is nowhere, with every snapshot read, is a proven negative:\n{absent_out}"
    );
    assert!(
        absent_out.contains("not in this destination"),
        "{absent_out}"
    );
    assert!(
        absent_out.contains("snapshots scanned: 3 of 3"),
        "the negative must say how much of the destination it looked at:\n{absent_out}"
    );
}

/// The `--json` shape is produced by the library, so it can be checked without
/// a repository: both new snapshot counters must be present, and the two
/// "found nothing" states must stay different sentences.
#[test]
fn report_json_and_no_hit_line_distinguish_a_partial_scan() {
    let partial = SearchReport {
        destination: "opendal:sftp".to_string(),
        snapshots_in_repo: 417,
        snapshots_scanned: 3,
        snapshots_from_cache: 0,
        sessions_seen: 4041,
        window: None,
        hits: Vec::new(),
        unplaced: Vec::new(),
        all_recall: BTreeMap::new(),
        not_matched: 4041,
        machines_without_index: Vec::new(),
        machines_with_legacy_index: Vec::new(),
        hosts: Vec::new(),
        unreadable: Vec::new(),
        data_blobs_read: 0,
        index_files_read: 1,
    };
    let json: serde_json::Value = serde_json::from_str(&report_json(&partial, false)).unwrap();
    assert_eq!(json["snapshots_all_scanned"], false);
    assert_eq!(
        json["answer_complete"], false,
        "a scan of 3 of 417 snapshots answered nothing completely"
    );
    let line = partial.no_hit_line();
    println!("[W257] partial-scan terminal line:\n    {line}");
    assert!(line.contains("UNKNOWN"), "{line}");
    assert!(
        !line.contains("not in this destination"),
        "3 of 417 snapshots is not a destination read in full:\n{line}"
    );
    assert!(line.contains("3 of 417"), "{line}");

    // The same report with every snapshot walked *is* allowed to answer.
    let complete = SearchReport {
        snapshots_scanned: 417,
        snapshots_from_cache: 0,
        ..partial
    };
    assert!(complete.scanned_all_snapshots());
    assert!(complete.answer_complete());
    let line = complete.no_hit_line();
    println!("[W257] complete-scan terminal line:\n    {line}");
    assert!(
        line.contains("not in this destination"),
        "only a full scan may say this:\n{line}"
    );
}

/// What SRCH-1 costs, on a repository built to the shape that makes it
/// necessary: many snapshots, and a newest one that holds almost nothing.
///
/// SRCH-1 walks **every** snapshot of a machine where it used to walk one, so
/// the cost question is "what does one extra snapshot of tree metadata add",
/// and the answer has to be measured rather than argued. `mac` on the real
/// destination had 417 snapshots and 3178 indexed sessions but 16 bodies in its
/// newest snapshot; this fixture cannot be that big inside a test, so it is
/// scaled down and the per-snapshot figure is what the report quotes.
///
/// Correctness is asserted; the timings are printed. A wall-clock assertion
/// would be a flaky test, and the thing that must not regress silently here is
/// the *rule* (every snapshot walked, no shard read), not a millisecond count.
#[test]
fn cost_of_walking_every_snapshot_is_measured() {
    const SNAPSHOTS: usize = 24;
    const SESSIONS: usize = 20;

    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let cfg = cached_store_config(dir.path(), "repo");
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();

    let sessions: Vec<String> = (0..SESSIONS)
        .map(|i| format!("claude-code.{MACHINE}.00000000-0000-0000-0000-{i:012}"))
        .collect();
    let names: Vec<&str> = sessions.iter().map(String::as_str).collect();
    for (i, session) in sessions.iter().enumerate() {
        write_session(&stage, session, 1, &format!("cost-{i}"));
    }

    // One push per "run", and each run rewrites the activity index the way a
    // real one does — so every snapshot's tree really is a different tree, not
    // the same blobs listed again.
    for run in 0..SNAPSHOTS {
        write_activity_index(
            &stage,
            &names,
            1_700_000_000 + (run as i64) * 86_400,
            1_700_000_600 + (run as i64) * 86_400,
        );
        push(&cfg, &stage, &mk);
    }

    let store = BackupStore::new(cfg.clone(), MACHINE.to_string());

    // Warm the repository metadata cache once, so the measurement below is the
    // walk and not the first-open cost.
    let _ = search_sessions(&store, &mk, &Selector::default()).unwrap();

    let started = std::time::Instant::now();
    let report = search_sessions(&store, &mk, &Selector::default()).unwrap();
    let cumulative_ms = started.elapsed().as_millis();

    // A one-snapshot repository with the same session set, to separate the
    // fixed cost of a search from the marginal cost of a snapshot.
    // The same shape with almost no sessions, to separate "cost per snapshot"
    // from "cost per session per snapshot". Both numbers are needed before any
    // figure can be extrapolated to a real machine.
    const SMALL_SESSIONS: usize = 2;
    let small_root = dir.path().join("small");
    let small_cfg = cached_store_config(&small_root, "repo");
    store::persist_key_file(&small_cfg, &mk).unwrap();
    let small_stage = small_root.join("stage");
    let small_sessions: Vec<String> = (0..SMALL_SESSIONS)
        .map(|i| format!("claude-code.{MACHINE}.00000000-0000-0000-0000-{i:012}"))
        .collect();
    let small_names: Vec<&str> = small_sessions.iter().map(String::as_str).collect();
    for (i, session) in small_sessions.iter().enumerate() {
        write_session(&small_stage, session, 1, &format!("small-{i}"));
    }
    for run in 0..SNAPSHOTS {
        write_activity_index(
            &small_stage,
            &small_names,
            1_700_000_000 + (run as i64) * 86_400,
            1_700_000_600 + (run as i64) * 86_400,
        );
        push(&small_cfg, &small_stage, &mk);
    }
    let small_store = BackupStore::new(small_cfg, MACHINE.to_string());
    let _ = search_sessions(&small_store, &mk, &Selector::default()).unwrap();
    let started = std::time::Instant::now();
    let small = search_sessions(&small_store, &mk, &Selector::default()).unwrap();
    let small_ms = started.elapsed().as_millis();
    let small_marginal = small_ms / SNAPSHOTS as u128;

    let solo_root = dir.path().join("solo");
    let solo_cfg = cached_store_config(&solo_root, "repo");
    store::persist_key_file(&solo_cfg, &mk).unwrap();
    let solo_stage = solo_root.join("stage");
    for (i, session) in sessions.iter().enumerate() {
        write_session(&solo_stage, session, 1, &format!("cost-{i}"));
    }
    write_activity_index(&solo_stage, &names, 1_700_000_000, 1_700_000_600);
    push(&solo_cfg, &solo_stage, &mk);
    let solo_store = BackupStore::new(solo_cfg, MACHINE.to_string());
    let _ = search_sessions(&solo_store, &mk, &Selector::default()).unwrap();
    let started = std::time::Instant::now();
    let solo = search_sessions(&solo_store, &mk, &Selector::default()).unwrap();
    let solo_ms = started.elapsed().as_millis();
    assert_eq!(
        solo.hits.len(),
        SESSIONS,
        "the one-snapshot baseline must find the same sessions, or the subtraction is meaningless"
    );
    assert_eq!(solo.snapshots_in_repo, 1);

    // The same repository with no metadata cache at all: the pessimistic bound,
    // and the number this test measures against the code as shipped, since the
    // rest of the suite runs cacheless for isolation. A configured run sits
    // between the two.
    let cold_store = BackupStore::new(store_config(dir.path(), "repo"), MACHINE.to_string());
    let started = std::time::Instant::now();
    let cold = search_sessions(&cold_store, &mk, &Selector::default()).unwrap();
    let cold_ms = started.elapsed().as_millis();

    let marginal = cumulative_ms.saturating_sub(solo_ms) / (SNAPSHOTS as u128 - 1);
    let cold_marginal = cold_ms.saturating_sub(solo_ms) / (SNAPSHOTS as u128 - 1);
    println!(
        "[W257] cost: sessions={SESSIONS} snapshots={SNAPSHOTS}\n\
         [W257]   warm cache: 1 snapshot={solo_ms}ms, {SNAPSHOTS} snapshots={cumulative_ms}ms, \
         marginal={marginal}ms/snapshot\n\
         [W257]   no cache:   {SNAPSHOTS} snapshots={cold_ms}ms, marginal={cold_marginal}ms/snapshot\n\
         [W257]   sessions={SMALL_SESSIONS} snapshots={SNAPSHOTS}: {small_ms}ms, \
         per_snapshot_incl_fixed={small_marginal}ms (one open + one index read are part of it)\n\
         [W257]   payload: data_blobs_read={} index_files_read={} (cold {} / {})",
        report.data_blobs_read,
        report.index_files_read,
        cold.data_blobs_read,
        cold.index_files_read
    );
    assert_eq!(
        cold.hits.len(),
        SESSIONS,
        "the cacheless run must find exactly the same sessions"
    );
    assert_eq!(small.hits.len(), SMALL_SESSIONS);
    assert_eq!(small.snapshots_scanned, SNAPSHOTS);

    assert_eq!(report.snapshots_in_repo, SNAPSHOTS);
    assert_eq!(
        report.snapshots_scanned, SNAPSHOTS,
        "every snapshot must be walked, not just the newest"
    );
    assert_eq!(
        report.hits.len(),
        SESSIONS,
        "and every session must still be found exactly once"
    );
    assert_eq!(report.sessions_seen, SESSIONS, "no session counted twice");
    assert_eq!(
        report.data_blobs_read, 0,
        "walking {SNAPSHOTS} snapshots must not read a single conversation blob"
    );
    assert_eq!(
        report.index_files_read, 1,
        "the activity index is read once per machine, not once per snapshot"
    );
    // The session bodies are all still in the stage here, so every session is
    // reported against the newest snapshot — the first-appearance rule.
    assert!(
        report.hits.iter().all(|h| h.snapshot_id
            == report
                .hits
                .iter()
                .map(|x| x.snapshot_id.clone())
                .next()
                .unwrap()),
        "with no reclaim every session's newest appearance is the same snapshot"
    );
}

/// The HIGH finding the review of `b4b31fc` raised: a **successful** read must
/// prove that no newer snapshot holds the session.
///
/// The walk resolves a session by its newest appearance (ADR-021), so the
/// snapshot it falls back to is only the session's current copy while every
/// newer snapshot has been shown not to hold it. Skipping a snapshot that could
/// not be walked breaks exactly that: the session is handed back from an older
/// snapshot as if it were current, with nothing said. This fixture is built so
/// the difference is observable — snapshot C holds a *newer, shorter* copy of
/// `CONTINUED`, and its packs are gone, so a walk that skips it returns the
/// older, longer copy from snapshot A and calls it the answer.
#[test]
fn a_read_refuses_an_older_copy_when_a_newer_snapshot_is_unreadable() {
    let f = fixture_with_a_damaged_newest_snapshot(true);

    // The premise, proven rather than assumed: the older copy really is a
    // different, longer answer than the one snapshot C holds. Without this, a
    // stale read would be indistinguishable from a correct one.
    assert_ne!(
        f.continued_old_bytes, f.continued_bytes,
        "the fixture must hold two different copies of the same session, or this test is vacuous"
    );

    let store = BackupStore::new(f.cfg.clone(), MACHINE.to_string());

    let err = store
        .read_session_concat(MACHINE, CONTINUED, &f.mk)
        .expect_err(
            "a snapshot newer than the one that holds the session could not be read, so the \
             session's current bytes are unknown — an older copy must never be returned as a \
             successful read",
        );
    let said = err.to_string();
    println!("[W257] read over a damaged newest snapshot: {said}");

    // It must say UNKNOWN, and it must name what it could not read — an operator
    // cannot act on "something went wrong".
    assert!(said.contains("UNKNOWN"), "{said}");
    assert!(
        said.contains("tree walk") || said.contains("tree root"),
        "the failure must name the snapshot it could not walk: {said}"
    );
    assert!(
        said.contains("cannot be resolved for machine"),
        "the failure must name the machine it is a partial answer for: {said}"
    );
    // And specifically: not the stale copy.
    assert!(
        !said.contains(&format!("sha256={}", sha256_hex(&f.continued_old_bytes))),
        "{said}"
    );

    // The rule is about the *machine's* newer snapshots, not about this one
    // session: `RECLAIMED` is only in snapshot A, and C is newer than A, so C
    // could have held a newer copy of it too. The same UNKNOWN applies.
    let err = store
        .read_session_concat(MACHINE, RECLAIMED, &f.mk)
        .expect_err(
            "a session whose only known copy is older than an unreadable snapshot is not \
             resolved either — the unreadable one might hold a newer copy",
        );
    assert!(err.to_string().contains("UNKNOWN"), "{err}");
}

/// The same rule for `search`, and the shape the terminal output has to have:
/// an unreadable snapshot makes the answer incomplete, so a negative can never
/// be printed.
#[test]
fn search_is_incomplete_and_prints_no_negative_over_an_unreadable_snapshot() {
    let f = fixture_with_a_damaged_newest_snapshot(true);
    let store = BackupStore::new(f.cfg.clone(), MACHINE.to_string());

    let report = search_sessions(&store, &f.mk, &Selector::default()).unwrap();
    println!(
        "[W257] damaged: in_repo={} scanned={} unreadable={:?} answer_complete={}",
        report.snapshots_in_repo,
        report.snapshots_scanned,
        report.unreadable,
        report.answer_complete()
    );

    assert_eq!(report.snapshots_in_repo, 3, "A, B and C");
    assert_eq!(
        report.snapshots_scanned, 2,
        "C could not be walked, and the count must not claim it was"
    );
    assert_eq!(
        report.unreadable.len(),
        1,
        "the snapshot that could not be walked must be recorded, not swallowed: {:?}",
        report.unreadable
    );
    assert!(
        report.unreadable[0].contains("snapshot") && report.unreadable[0].contains("tree walk"),
        "{:?}",
        report.unreadable
    );

    // Both halves of "did we look everywhere" are false, and neither may be
    // mistaken for an answer.
    assert!(!report.complete(), "a snapshot could not be read");
    assert!(
        !report.scanned_all_snapshots(),
        "2 of 3 snapshots walked is not the whole destination"
    );
    assert!(
        !report.answer_complete(),
        "nothing may be concluded from a destination half of which was not looked at"
    );

    // A negative must not be printable, whatever the query.
    let line = report.no_hit_line();
    println!("[W257] damaged terminal line:\n    {line}");
    assert!(line.contains("UNKNOWN"), "{line}");
    assert!(
        !line.contains("not in this destination"),
        "one unreadable snapshot makes every miss unproven:\n{line}"
    );

    // The hits that were found are still real — the walk stops at the
    // unreadable snapshot, it does not abandon what it already saw.
    let continued = report
        .hits
        .iter()
        .find(|h| h.session_id == CONTINUED)
        .expect("the copy in snapshot A is still readable and must be reported");
    assert_eq!(
        continued.shard_count, 2,
        "the reported copy is A's, the only one that can be read"
    );
    assert_eq!(report.hits.len(), 2, "both sessions are still found");
}

/// The CLI end of the same rule: exit 3, the stale digest never printed, and
/// the scan line carrying both numbers.
#[test]
fn cli_read_and_search_are_unknown_over_an_unreadable_snapshot() {
    let f = fixture_with_a_damaged_newest_snapshot(true);

    // `read --session` must not succeed with the older copy. Before the fix it
    // exited 0 and printed A's digest, which is a stale read indistinguishable
    // from a correct one.
    let read = isolated_command(f.dir.path())
        .args(["read", "--repo"])
        .arg(&f.cfg.repo_root)
        .args(["--key-file"])
        .arg(&f.cfg.key_file)
        .args(["--machine", MACHINE, "--session", CONTINUED])
        .output()
        .unwrap();
    let read_out = String::from_utf8_lossy(&read.stdout).into_owned();
    let read_err = String::from_utf8_lossy(&read.stderr).into_owned();
    println!(
        "[W257] read over a damaged newest snapshot: exit={:?}",
        read.status.code()
    );
    assert_eq!(
        read.status.code(),
        Some(3),
        "an unreadable newer snapshot is \"did not finish\", not a result:\n{read_out}\n{read_err}"
    );
    assert!(
        !read_out.contains(&format!("sha256={}", sha256_hex(&f.continued_old_bytes))),
        "the older copy must never be printed as the session's bytes:\n{read_out}"
    );
    assert!(
        read_err.contains("UNKNOWN"),
        "the reason must reach stderr, where a failed run's diagnostics go:\n{read_err}"
    );

    // A session that is nowhere: still no negative, because C was not read.
    let absent = isolated_command(f.dir.path())
        .args(["search", "--repo"])
        .arg(&f.cfg.repo_root)
        .args(["--key-file"])
        .arg(&f.cfg.key_file)
        .args(["--session", "zzzz-no-such-session"])
        .output()
        .unwrap();
    let absent_out = String::from_utf8_lossy(&absent.stdout).into_owned();
    println!(
        "[W257] absent over a damaged snapshot -> exit={:?}",
        absent.status.code()
    );
    assert_eq!(
        absent.status.code(),
        Some(3),
        "a miss over a destination that was not read in full is not a negative:\n{absent_out}"
    );
    assert!(
        !absent_out.contains("not in this destination"),
        "the negative line must be unreachable while a snapshot is unreadable:\n{absent_out}"
    );
    assert!(
        absent_out.contains("snapshots scanned: 2 of 3 in repo, 1 unreadable"),
        "the scan line must carry the unreadable count, not just the fraction:\n{absent_out}"
    );

    // And the same two numbers on the machine-readable surface.
    let json = isolated_command(f.dir.path())
        .args(["search", "--repo"])
        .arg(&f.cfg.repo_root)
        .args(["--key-file"])
        .arg(&f.cfg.key_file)
        .args(["--json"])
        .output()
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&String::from_utf8_lossy(&json.stdout))
        .expect("stdout must stay exactly one JSON object");
    assert_eq!(json.status.code(), Some(3), "{parsed}");
    assert_eq!(parsed["snapshots_scanned"], 2);
    assert_eq!(parsed["snapshots_in_repo"], 3);
    assert_eq!(parsed["snapshots_all_scanned"], false);
    assert_eq!(parsed["complete"], false);
    assert_eq!(parsed["answer_complete"], false);
    assert_eq!(
        parsed["unreadable_parts"].as_array().unwrap().len(),
        1,
        "{parsed}"
    );
}

/// The figure the review of `b4b31fc` asked for, at the scale the archive
/// actually has: what `search` and `read --session` cost over **400+ snapshots**.
///
/// The check in the shipped suite (`cost_of_walking_every_snapshot_is_measured`)
/// walks 24 snapshots and fits a slope from it. That is the right shape for a
/// gate and the wrong shape for the question "is this acceptable on the real
/// archive", which has 417 snapshots on the busiest machine. This builds the
/// same shape at 417 and reports the numbers directly.
///
/// `#[ignore]`d because building 417 snapshots is minutes of work: it is a
/// measurement, not a gate. Run it explicitly:
///
/// ```text
/// cargo test -p chat-stasher --test w257_cumulative_search_test -- --ignored --nocapture field_scale
/// ```
///
/// **Bytes** are reported from the two sources that can be stated exactly:
/// the counters the search itself reports (`data_blobs_read`,
/// `index_files_read`), and the on-disk size of the tiers a search fetches as
/// whole files (`snapshots/`, `index/`). Tree blobs travel *inside* the data
/// packs, so their byte volume is not separable from the pack that carries
/// them — what is separable, and what the whole change turns on, is that no
/// conversation body is fetched at all.
#[test]
#[ignore = "manual wall-clock measurement; builds 417 snapshots"]
fn field_scale_cost_of_search_and_read_over_four_hundred_snapshots() {
    /// The field archive's own count (`mac`, W253 §B).
    const SNAPSHOTS: usize = 417;
    /// Enough sessions that the per-session term is visible, without making the
    /// build an hour long. The field machine indexes 3178.
    const SESSIONS: usize = 40;

    fn dir_bytes(dir: &Path) -> u64 {
        let mut total = 0;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(meta) = path.metadata() {
                    total += meta.len();
                }
            }
        }
        total
    }

    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path().join("stage");
    let cfg = cached_store_config(dir.path(), "repo");
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();

    let sessions: Vec<String> = (0..SESSIONS)
        .map(|i| format!("claude-code.{MACHINE}.00000000-0000-0000-0000-{i:012}"))
        .collect();
    let names: Vec<&str> = sessions.iter().map(String::as_str).collect();

    // One push per "run", each rewriting the activity index the way a real run
    // does, so every snapshot's tree really is a different tree.
    let mut payload_bytes = 0u64;
    let mut session_bytes: Vec<u64> = Vec::with_capacity(SESSIONS);
    for (i, session) in sessions.iter().enumerate() {
        let written = write_session(&stage, session, 1, &format!("field-{i}")).len() as u64;
        payload_bytes += written;
        session_bytes.push(written);
    }
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

    let repo = Path::new(&cfg.repo_root).to_path_buf();
    let snapshots_bytes = dir_bytes(&repo.join("snapshots"));
    let index_bytes = dir_bytes(&repo.join("index"));
    let data_bytes = dir_bytes(&repo.join("data"));

    let store = BackupStore::new(cfg.clone(), MACHINE.to_string());

    // Cold: no metadata cache is populated for this repository yet in this
    // process. Warm: the second call, which is what a repeated run sees.
    let cold_store = BackupStore::new(store_config(dir.path(), "repo"), MACHINE.to_string());
    let started = std::time::Instant::now();
    let cold = search_sessions(&cold_store, &mk, &Selector::default()).unwrap();
    let cold_ms = started.elapsed().as_millis();

    let _ = search_sessions(&store, &mk, &Selector::default()).unwrap();
    let started = std::time::Instant::now();
    let warm = search_sessions(&store, &mk, &Selector::default()).unwrap();
    let warm_ms = started.elapsed().as_millis();

    // The same repository with a single snapshot, to separate the walk's
    // per-snapshot cost from everything else a search pays once.
    let solo_root = dir.path().join("solo");
    let solo_cfg = cached_store_config(&solo_root, "repo");
    store::persist_key_file(&solo_cfg, &mk).unwrap();
    let solo_stage = solo_root.join("stage");
    for (i, session) in sessions.iter().enumerate() {
        write_session(&solo_stage, session, 1, &format!("field-{i}"));
    }
    write_activity_index(&solo_stage, &names, 1_700_000_000, 1_700_000_600);
    push(&solo_cfg, &solo_stage, &mk);
    let solo_store = BackupStore::new(solo_cfg, MACHINE.to_string());
    let _ = search_sessions(&solo_store, &mk, &Selector::default()).unwrap();
    let started = std::time::Instant::now();
    let solo = search_sessions(&solo_store, &mk, &Selector::default()).unwrap();
    let solo_ms = started.elapsed().as_millis();

    // One session read back out of the 417-snapshot repository.
    let target = &sessions[0];
    let started = std::time::Instant::now();
    let (bytes, hashes) = store.read_session_concat(MACHINE, target, &mk).unwrap();
    let read_ms = started.elapsed().as_millis();

    println!(
        "[W257] FIELD SCALE: snapshots={SNAPSHOTS} sessions={SESSIONS} \
         (build {build_ms}ms)\n\
         [W257]   search, no cache   = {cold_ms}ms\n\
         [W257]   search, warm cache = {warm_ms}ms\n\
         [W257]   search, 1 snapshot = {solo_ms}ms  -> marginal \
         {:.2}ms/snapshot\n\
         [W257]   read --session     = {read_ms}ms for {} B in {} shard(s)\n\
         [W257]   payload: data_blobs_read={} (cold) / {} (warm) / {} (solo), \
         index_files_read={} / {} / {}\n\
         [W257]   bytes on disk: snapshots={snapshots_bytes} B, index={index_bytes} B, \
         data packs={data_bytes} B; one session's payload={payload_bytes} B",
        warm_ms.saturating_sub(solo_ms) as f64 / (SNAPSHOTS - 1) as f64,
        bytes.len(),
        hashes.len(),
        cold.data_blobs_read,
        warm.data_blobs_read,
        solo.data_blobs_read,
        cold.index_files_read,
        warm.index_files_read,
        solo.index_files_read,
    );

    // The properties, as opposed to the timings: a 417-snapshot search must
    // still walk every snapshot, read no conversation body, and read the
    // activity index once.
    assert_eq!(cold.snapshots_in_repo, SNAPSHOTS);
    assert_eq!(cold.snapshots_scanned, SNAPSHOTS);
    assert_eq!(cold.hits.len(), SESSIONS);
    assert_eq!(cold.sessions_seen, SESSIONS);
    assert_eq!(
        cold.data_blobs_read, 0,
        "no conversation body may be fetched"
    );
    assert_eq!(cold.index_files_read, 1);
    assert_eq!(warm.data_blobs_read, 0);
    assert_eq!(
        bytes.len() as u64,
        session_bytes[0],
        "the read must hand back exactly the shard the session was written with"
    );
    assert_eq!(hashes.len(), 1);
}
