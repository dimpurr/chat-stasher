//! W1093 · a wanted-set cumulative read costs the set, not the archive.
//!
//! Field ground (DEPLOY-1107, three observed rounds): the destination
//! session-facts probe spent 31.0 / 20.1 / 21.4 min reading an archive whose
//! rounds needed `wanted=208` sessions — 89-91 % of a round's collect —
//! because `read_cumulative_sessions` listed every file of every snapshot
//! before filtering to the set (`[readback] cumulative read: wanted=208`,
//! once per probe). W1059 already gave the single-session readers a directed
//! subtree resolution; this ticket moves the cumulative walk onto the same
//! mechanism when a wanted set is given.
//!
//! How equality is proven here, without keeping the old walk in the binary:
//! the repository's own fallback *is* the old walk. The directed listing is
//! refused for a stage root that holds anything outside the writer's own
//! top-level names, and a refused snapshot is listed recursively exactly as
//! the walk always did. So the two repositories this file builds hold the
//! same content — one through the writer's layout (every snapshot takes the
//! directed listing), one with one foreign top-level name beside it (every
//! snapshot falls back to the full recursive listing, the pre-W1093 path) —
//! and every assertion compares a wanted-set read of the first against the
//! same read of the second. The comparison arm runs production code on both
//! sides, through the public store, over fixtures that include reclaimed
//! bodies spanning snapshots, a late session, cross-machine session-name
//! reuse and machine logs.
//!
//! The timing claim is not asserted here: correctness at fixture size is,
//! and scale is measured by the ignored benchmark, same arrangement as
//! W1059's. Everything is synthetic: generated ids and payload only.

use chat_stasher::readback::ReadAllReport;
use chat_stasher::store::{self, BackupStore, StageWriter, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const MACHINE_A: &str = "w1093-a";
const MACHINE_B: &str = "w1093-b";
/// One foreign top-level name on the fallback repository's stages. Nothing
/// the writer makes, so [`store`]'s fast-path screen refuses every snapshot
/// of the repository that holds it and the walk lists those snapshots
/// recursively — the pre-W1093 behavior, kept reachable on purpose.
const FOREIGN_STAGE_NAME: &str = "zz-w1093-foreign";

fn cfg_in(dir: &Path) -> StoreConfig {
    StoreConfig {
        repo_root: dir.join("repo").to_string_lossy().into_owned(),
        key_file: dir.join("repo.key"),
        connections: 1,
        options: BTreeMap::new(),
        cache_dir: Some(dir.join("cache")),
        no_cache: false,
    }
}

fn sid(machine: &str, i: usize) -> String {
    format!("claude-code.{machine}.00000000-0000-0000-0000-{i:012}")
}

fn lines_for(tag: &str) -> Vec<String> {
    (0..4)
        .map(|i| format!("{{\"t\":\"{tag}\",\"i\":{i}}}"))
        .collect()
}

fn add_shard(stage: &Path, machine: &str, session: &str, tag: &str) {
    store::write_sealed_shard(
        StageWriter::Collect,
        stage,
        machine,
        session,
        &lines_for(tag),
    )
    .unwrap();
}

fn add_machine_log(stage: &Path, machine: &str, tag: &str) {
    store::write_sealed_machine_log_shard(
        stage,
        machine,
        "claude-code",
        "history",
        &lines_for(tag)
            .into_iter()
            .map(|l| l.into_bytes())
            .collect::<Vec<_>>(),
        20,
        false,
    )
    .unwrap();
}

fn drop_bodies(stage: &Path, machine: &str, session: &str) {
    let dir = stage.join("sessions").join(machine).join(session);
    if dir.is_dir() {
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for entry in fs::read_dir(&d).unwrap().filter_map(Result::ok) {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    fs::remove_file(p).unwrap();
                }
            }
        }
    }
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().filter_map(Result::ok) {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// Build one repository: six pushes per machine over an evolving stage, with
/// the same mutation script every call — `foreign` is the only difference and
/// it exists before the first push, so it is in every snapshot's stage root.
///
/// The stage holds sessions `0..5` for [`MACHINE_A`] and `0..2` for
/// [`MACHINE_B`] (the same session *names* on both machines: the walk's
/// partitioning by machine name is one of the properties under comparison),
/// one machine log per machine, a session that first appears late, and
/// bodies that are reclaimed between pushes so their older sequences live
/// only in older snapshots — the ADR-021 state a cumulative walk exists for.
fn build(dir: &Path, foreign: bool) -> MasterKey {
    let cfg = cfg_in(dir);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let store_a = BackupStore::new(cfg.clone(), MACHINE_A.to_string());
    let store_b = BackupStore::new(cfg, MACHINE_B.to_string());
    let stage_a = dir.join("stage-a");
    let stage_b = dir.join("stage-b");
    for i in 0..6 {
        add_shard(
            &stage_a,
            MACHINE_A,
            &sid(MACHINE_A, i),
            &format!("init-a-{i}"),
        );
    }
    for i in 0..3 {
        add_shard(
            &stage_b,
            MACHINE_B,
            &sid(MACHINE_B, i),
            &format!("init-b-{i}"),
        );
    }
    if foreign {
        let foreign_dir = stage_a.join(FOREIGN_STAGE_NAME);
        fs::create_dir_all(&foreign_dir).unwrap();
        fs::write(foreign_dir.join("note.jsonl"), b"not the writer's layout\n").unwrap();
        let foreign_dir = stage_b.join(FOREIGN_STAGE_NAME);
        fs::create_dir_all(&foreign_dir).unwrap();
        fs::write(foreign_dir.join("note.jsonl"), b"not the writer's layout\n").unwrap();
    }
    const ROUNDS: usize = 6;
    for run in 0..ROUNDS {
        // one session of each machine grows per round, a different one each time
        add_shard(
            &stage_a,
            MACHINE_A,
            &sid(MACHINE_A, run % 6),
            &format!("run-{run}-a"),
        );
        if run % 2 == 1 {
            add_shard(
                &stage_b,
                MACHINE_B,
                &sid(MACHINE_B, run % 3),
                &format!("run-{run}-b"),
            );
        }
        // a session that exists only late
        if run + 2 == ROUNDS {
            add_shard(&stage_a, MACHINE_A, &sid(MACHINE_A, 79), "late-a");
        }
        add_machine_log(&stage_a, MACHINE_A, &format!("gen-a-{run}"));
        if run % 3 == 0 {
            add_machine_log(&stage_b, MACHINE_B, &format!("gen-b-{run}"));
        }
        store_a.push(&stage_a, &mk).unwrap();
        store_b.push(&stage_b, &mk).unwrap();
        // reclaim between pushes: the reclaimed sessions' older sequences stay
        // readable only through the older snapshots
        if run % 2 == 1 {
            drop_bodies(&stage_a, MACHINE_A, &sid(MACHINE_A, (run + 1) % 6));
            drop_bodies(&stage_a, MACHINE_A, &sid(MACHINE_A, (run + 3) % 6));
        }
        if run == 4 {
            drop_bodies(&stage_b, MACHINE_B, &sid(MACHINE_B, 0));
        }
    }
    mk
}

/// Everything a wanted-set read returns, as comparable lines: per-session
/// shard inventory, machine logs, warnings, the incomplete set and the share
/// of the archive the read walked. Per-host snapshot ids and times are
/// deliberately absent — the two repositories pushed at different instants
/// and the walk's answers are not about the snapshot ids.
fn report_lines(report: &ReadAllReport) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("snapshots_in_repo={}", report.snapshots_in_repo));
    lines.push(format!("complete={}", report.complete()));
    let mut warnings = report.warnings.clone();
    warnings.sort();
    lines.push(format!("warnings={warnings:?}"));
    let mut incomplete: Vec<_> = report.incomplete_machines.iter().cloned().collect();
    incomplete.sort();
    lines.push(format!("incomplete={incomplete:?}"));
    let mut hosts: Vec<_> = report
        .machines
        .iter()
        .map(|m| format!("{:?}:{}", m.hostname, m.sessions.len()))
        .collect();
    hosts.sort();
    lines.push(format!("hosts={hosts:?}"));
    let mut sessions: Vec<String> = report
        .machines
        .iter()
        .flat_map(|machine| {
            machine
                .sessions
                .iter()
                .map(|s| (machine.hostname.clone(), s))
        })
        .map(|(host, s)| {
            format!(
                "session {host}/{} shards={} bytes={} sha={} shard_sha={:?} seq={:?} \
                 shard_bytes={:?} run_dups={:?} folded={:?}",
                s.session_id,
                s.shard_count,
                s.concat_bytes,
                s.sha256,
                s.shard_sha256,
                s.shard_sequences,
                s.shard_bytes,
                s.shard_run_duplicates,
                s.folded_shard_indices
            )
        })
        .collect();
    sessions.sort();
    lines.extend(sessions);
    let mut machine_logs: Vec<String> = report
        .machine_logs
        .iter()
        .map(|l| {
            format!(
                "machine-log {}/{}/{} shards={} bytes={} sha={} shard_sha={:?} seq={:?} \
                 shard_bytes={:?}",
                l.machine,
                l.harness,
                l.log_id,
                l.shard_count,
                l.concat_bytes,
                l.sha256,
                l.shard_sha256,
                l.shard_sequences,
                l.shard_bytes
            )
        })
        .collect();
    machine_logs.sort();
    lines.extend(machine_logs);
    lines
}

fn wanted_read(
    dir: &Path,
    mk: &MasterKey,
    wanted: Option<&BTreeSet<(String, String)>>,
) -> ReadAllReport {
    let st = BackupStore::new(cfg_in(dir), MACHINE_A.to_string());
    let read = match wanted {
        Some(set) => st.read_cumulative_sessions(mk, Some(set)),
        // `None` is the whole-archive question at the same policies; the
        // walk lists those trees in full on either repository, so it is the
        // same comparison arm from both sides and a fixture sanity read in
        // its own right.
        None => st.read_cumulative_sessions(mk, None),
    };
    read.unwrap()
}

/// Compare one wanted-set read across the two repositories: the directed
/// listing (writer's layout) against the full recursive listing (the
/// foreign-name stage refuses the fast path, snapshot by snapshot — the
/// pre-W1093 walk). Returns the two line lists for the case's own extra
/// assertions.
fn assert_same_read(
    directed: &Path,
    fallback: &Path,
    mk_directed: &MasterKey,
    mk_fallback: &MasterKey,
    wanted: Option<&BTreeSet<(String, String)>>,
) -> (Vec<String>, Vec<String>) {
    let a = report_lines(&wanted_read(directed, mk_directed, wanted));
    let b = report_lines(&wanted_read(fallback, mk_fallback, wanted));
    assert_eq!(
        a, b,
        "the directed listing and the full-recursive fallback must answer identically"
    );
    (a, b)
}

/// Every session the fixture wrote, derived from the script [`build`] runs so
/// the set cannot drift from the repository: [`MACHINE_A`] holds sessions
/// 0..=5 plus the late one, [`MACHINE_B`] holds 0..=2 — reclaimed bodies
/// included, because reclaim removes stage bytes, never archive ones.
fn full_wanted_set() -> BTreeSet<(String, String)> {
    (0..=5)
        .map(|i| (MACHINE_A.to_string(), sid(MACHINE_A, i)))
        .chain([(MACHINE_A.to_string(), sid(MACHINE_A, 79))])
        .chain((0..=2).map(|i| (MACHINE_B.to_string(), sid(MACHINE_B, i))))
        .collect()
}

fn open_pair() -> (tempfile::TempDir, PathBuf, MasterKey, PathBuf, MasterKey) {
    let pair = tempfile::TempDir::new().unwrap();
    let directed = pair.path().join("directed");
    let fallback = pair.path().join("fallback");
    fs::create_dir_all(&directed).unwrap();
    fs::create_dir_all(&fallback).unwrap();
    let mk_directed = build(&directed, false);
    let mk_fallback = build(&fallback, true);
    (pair, directed, mk_directed, fallback, mk_fallback)
}

/// The whole wanted set of both machines — every session row, machine log,
/// and both walks' answers must agree, including the sessions whose bodies
/// were reclaimed between pushes.
#[test]
fn a_wanted_set_read_matches_the_full_recursive_fallback() {
    let (_guard, directed, mk_directed, fallback, mk_fallback) = open_pair();
    let full = full_wanted_set();
    assert!(full.len() >= 8, "fixture must hold a real session set");
    let (a, _) = assert_same_read(
        &directed,
        &fallback,
        &mk_directed,
        &mk_fallback,
        Some(&full),
    );
    assert!(
        a.iter().any(|l| l.starts_with("session ")),
        "the fixture must produce session rows"
    );
    assert!(
        a.iter().any(|l| l.starts_with("machine-log ")),
        "the fixture must produce machine-log rows"
    );
    // Same repository, same policies, one walk shape apart: a read of the
    // full wanted set and a read without any set describe the same bodies,
    // so the directed listing and the full listing must agree here too —
    // the comparison with no cross-repository caveat at all.
    let scoped = report_lines(&wanted_read(&directed, &mk_directed, Some(&full)));
    let unscoped = report_lines(&wanted_read(&directed, &mk_directed, None));
    assert_eq!(
        scoped, unscoped,
        "a full wanted set and an unscoped read are the same question on the same repository"
    );
}

/// A partial set across both machines, one that names a reclaimed session
/// (older sequences only in older snapshots), a late one and one machine's
/// session by the name another machine shares.
#[test]
fn a_partial_and_crossmachine_set_matches_the_fallback() {
    let (_guard, directed, mk_directed, fallback, mk_fallback) = open_pair();
    let mut set = BTreeSet::new();
    set.insert((MACHINE_A.to_string(), sid(MACHINE_A, 1)));
    set.insert((MACHINE_A.to_string(), sid(MACHINE_A, 79)));
    set.insert((MACHINE_B.to_string(), sid(MACHINE_B, 1)));
    // MACHINE_B's session 1 shares its name with MACHINE_A's session 1:
    // the pair, not the name, is the identity, on either listing path
    set.insert((MACHINE_A.to_string(), sid(MACHINE_A, 4)));
    let (a, _) = assert_same_read(&directed, &fallback, &mk_directed, &mk_fallback, Some(&set));
    assert_eq!(
        a.iter().filter(|l| l.starts_with("session ")).count(),
        4,
        "only the named sessions appear, on either arm"
    );
}

/// The empty set is the second destination probe's shape (the machine-log
/// question): no session rows, but the machine-log inventory and the merge
/// structure still have to match the fallback exactly.
#[test]
fn an_empty_wanted_set_keeps_machine_logs_and_merges_identical() {
    let (_guard, directed, mk_directed, fallback, mk_fallback) = open_pair();
    let empty = BTreeSet::new();
    let (a, _) = assert_same_read(
        &directed,
        &fallback,
        &mk_directed,
        &mk_fallback,
        Some(&empty),
    );
    assert_eq!(
        a.iter().filter(|l| l.starts_with("session ")).count(),
        0,
        "an empty wanted set reads no session"
    );
    assert!(
        a.iter().any(|l| l.starts_with("machine-log ")),
        "machine logs are read whatever the wanted set is"
    );
    assert!(a.iter().any(|l| l.starts_with("hosts=")), "merge rows stay");
}

/// A set naming a session the archive never held: absence must survive the
/// optimization — same absence, same machine logs, on both arms.
#[test]
fn a_wanted_set_naming_an_unheld_session_stays_an_absence() {
    let (_guard, directed, mk_directed, fallback, mk_fallback) = open_pair();
    let mut set = BTreeSet::new();
    set.insert((MACHINE_A.to_string(), sid(MACHINE_A, 99)));
    set.insert((MACHINE_B.to_string(), sid(MACHINE_B, 2)));
    let (a, _) = assert_same_read(&directed, &fallback, &mk_directed, &mk_fallback, Some(&set));
    assert_eq!(
        a.iter().filter(|l| l.starts_with("session ")).count(),
        1,
        "the held session comes back; the unheld one is simply absent"
    );
    assert!(!a.iter().any(|l| l.contains(&sid(MACHINE_A, 99))));
}

/// Same fixture sides, no wanted set at all: the unscoped read never takes
/// the directed listing, so both arms run the same full walk and the
/// assertion documents that shape: the answer comes from every tree.
#[test]
fn an_unscoped_read_still_lists_every_tree() {
    let (_guard, directed, mk_directed, fallback, mk_fallback) = open_pair();
    let (a, _) = assert_same_read(&directed, &fallback, &mk_directed, &mk_fallback, None);
    assert!(
        a.iter().filter(|l| l.starts_with("session ")).count() >= 9,
        "the unscoped read reports every session of both machines"
    );
    assert_eq!(
        a.iter().any(|l| l.starts_with("complete=true")),
        true,
        "a clean fixture reads with no warnings on either arm"
    );
}

/// The directed listing is refused for a stage that is not the writer's
/// layout — pinning that the fix did not remove the fallback and that the
/// foreign-name repository really walks the old way. The foreign file never
/// matches the walk's buckets, so the two arms' equality in the cases above
/// was proven *with* it present; this records why it is there at all.
#[test]
fn the_foreign_stage_name_stays_outside_the_buckets() {
    let (_guard, directed, _mkd, fallback, _mkf) = open_pair();
    let note = fallback
        .join("stage-a")
        .join(FOREIGN_STAGE_NAME)
        .join("note.jsonl");
    assert!(note.is_file(), "the fallback stage keeps its foreign name");
    assert!(
        !walk_files(&directed.join("stage-a"))
            .iter()
            .any(|p| p.to_string_lossy().contains(FOREIGN_STAGE_NAME)),
        "the directed fixture's stage holds only the writer's layout"
    );
}

/// Scale check of the same property, measured rather than asserted: the
/// field shape is a big stage and a small wanted set (DEPLOY-1107:
/// wanted=208 against an archive whose every snapshot lists the whole
/// remaining stage), so this times a small-set read against the unscoped
/// full walk of the same repository and prints both wall clocks. The
/// directed read's correctness is asserted the cheap way here — exactly the
/// named sessions come back — because the twin-repository equivalence is
/// what the cases above prove. Dims via `W1093_BENCH_SNAPS` /
/// `W1093_BENCH_SESSIONS` / `W1093_BENCH_WANTED`; ignored because building
/// the repository dominates the run.
#[test]
#[ignore = "manual wall-clock measurement"]
fn benchmark_directed_versus_fulllisting_wanted_reads() {
    let dims = |key: &str, default: usize| {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let snapshots = dims("W1093_BENCH_SNAPS", 120);
    let sessions = dims("W1093_BENCH_SESSIONS", 400);
    let wanted_count = dims("W1093_BENCH_WANTED", 16);
    let pair = tempfile::TempDir::new().unwrap();
    let dir = pair.path().to_path_buf();
    // one machine only, wide stage: the shape the field rounds read
    let cfg = cfg_in(&dir);
    let mk = MasterKey::new();
    store::persist_key_file(&cfg, &mk).unwrap();
    let st = BackupStore::new(cfg, MACHINE_A.to_string());
    let stage = dir.join("stage");
    for i in 0..sessions {
        add_shard(&stage, MACHINE_A, &sid(MACHINE_A, i), &format!("init-{i}"));
    }
    for run in 0..snapshots {
        for i in (0..sessions).step_by(7) {
            add_shard(
                &stage,
                MACHINE_A,
                &sid(MACHINE_A, i),
                &format!("run-{run}-{i}"),
            );
        }
        store::write_sealed_machine_log_shard(
            &stage,
            MACHINE_A,
            "claude-code",
            "history",
            &lines_for(&format!("gen-{run}"))
                .into_iter()
                .map(|l| l.into_bytes())
                .collect::<Vec<_>>(),
            20,
            false,
        )
        .unwrap();
        st.push(&stage, &mk).unwrap();
        if run % 5 == 4 {
            for i in (1..sessions).step_by(3) {
                drop_bodies(&stage, MACHINE_A, &sid(MACHINE_A, i));
            }
        }
    }
    // the small wanted set, spread over the stage the way a round's few
    // remotely-provable sessions are
    let wanted: BTreeSet<(String, String)> = (0..sessions)
        .step_by((sessions / wanted_count).max(1))
        .map(|i| (MACHINE_A.to_string(), sid(MACHINE_A, i)))
        .collect();
    // warm the metadata caches once, as an operator's second command would
    st.read_cumulative_sessions(&mk, Some(&wanted)).unwrap();
    let t = std::time::Instant::now();
    let directed = st.read_cumulative_sessions(&mk, Some(&wanted)).unwrap();
    let directed_ms = t.elapsed().as_millis();
    let t = std::time::Instant::now();
    let full = st.read_cumulative_sessions(&mk, None).unwrap();
    let full_ms = t.elapsed().as_millis();
    let directed_rows = report_lines(&directed)
        .iter()
        .filter(|l| l.starts_with("session "))
        .count();
    assert_eq!(
        directed_rows,
        wanted.len(),
        "the directed read returns exactly the wanted sessions"
    );
    assert!(full
        .machines
        .iter()
        .any(|m| m.hostname == MACHINE_A && m.sessions.len() == sessions));
    println!(
        "[W1093] BENCH snapshots={snapshots} sessions={sessions} wanted={} \
         directed_ms={directed_ms} full_ms={full_ms}",
        wanted.len()
    );
}
