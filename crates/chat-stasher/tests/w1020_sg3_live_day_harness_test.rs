//! W1020 — the SG-3 live-day gate's byte-comparison harness (ADR-061 D7).
//!
//! The ADR's live-day gate runs one real deputy session for a day on m1max
//! and then checks the two things fixtures cannot: (1) written bytes stay
//! within `max(1.2 × measured growth, 64 KiB)` per hour — measured from the
//! run-state counters `collect` records (`scripts/dev/sg3-live-day-report.py`
//! reads them); (2) the archived session's assembled default view equals a
//! fresh export of the source byte for byte at quiescence — which is what
//! this harness checks, read-only, against the machine's real database and
//! stage:
//!
//! ```sh
//! SG3_LIVE_DB=~/Library/Application\ Support/opencode/opencode.db \
//! SG3_LIVE_STAGE=<the machine's real stage root> \
//! SG3_LIVE_SID=<a session uuid that was collected today> \
//!     cargo test -p chat-stasher --test w1020_sg3_live_day_harness_test \
//!      -- --ignored --nocapture
//! ```
//!
//! Rules this harness holds to (the red lines of the live gate):
//!
//! * **read-only:** the database is opened exactly the way the collector
//!   opens it (`mode=ro`, one `BEGIN`), and the stage is only read. Nothing
//!   is written, no destination is contacted, no archive is touched by this
//!   run — the gate measures the production flows, it does not run them.
//! * **no numbers are invented here:** the harness asserts the byte contract
//!   only; the amplification ratio comes from the run-state counters the
//!   production collect wrote during the day, read by the report script.
//! * **quiescence:** run the comparison while the source session is not
//!   being written (the byte contract holds at quiescence, D3).
//! * **reclaim caveat:** the comparison is against the local stage's shard
//!   set. A session whose stage body was reclaimed mid-day still compares
//!   correctly as long as any part of its stage set remains; if the session
//!   was fully reclaimed, re-run the comparison against a fresh `collect`
//!   window, or compare through the destination's `read --session` digest
//!   instead (same contract, archived rather than staged bytes).

use chat_stasher::opencode_records::BodyView;
use chat_stasher::sqlite_probe;
use chat_stasher::store;
use std::path::{Path, PathBuf};

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

#[test]
#[ignore = "live-day harness: reads this machine's real opencode database \
            and stage; set SG3_LIVE_DB, SG3_LIVE_STAGE and SG3_LIVE_SID to run"]
fn the_live_session_assembles_to_the_sources_current_state() {
    let Some(db) = env_path("SG3_LIVE_DB") else {
        panic!(
            "set SG3_LIVE_DB to the opencode database path (the harness opens it \
             read-only, exactly as collect does)"
        )
    };
    let Some(stage) = env_path("SG3_LIVE_STAGE") else {
        panic!("set SG3_LIVE_STAGE to the machine's chat-stasher stage root")
    };
    let Some(sid) = std::env::var("SG3_LIVE_SID").ok().filter(|v| !v.is_empty()) else {
        panic!("set SG3_LIVE_SID to the opencode session id (its uuid)")
    };

    // The archived session directory: <stage>/sessions/*/opencode.*.<sid>.
    let machine = discover_machine(&stage, &sid);
    let session_id = format!("opencode.{machine}.{sid}");
    let dir = store::session_shard_dir(&stage, &machine, &session_id);
    let mut shards = store::sealed_shard_entries(&dir)
        .unwrap_or_else(|error| panic!("cannot list {}: {error}", dir.display()));
    assert!(
        !shards.is_empty(),
        "session {sid} holds no staged shards (fully reclaimed? see the harness doc: \
         re-run after a fresh collect window, or compare through `read --session`)"
    );
    shards.sort_by_key(|(seq, _)| *seq);
    let bodies: Vec<Vec<u8>> = shards
        .iter()
        .map(|(_, path)| std::fs::read(path).expect("read the staged shard"))
        .collect();
    let total_bytes: u64 = bodies.iter().map(|body| body.len() as u64).sum();

    // The assembled default view of the staged set.
    let assembled = chat_stasher::opencode_records::assemble_session_body(
        &session_id,
        &bodies,
        BodyView::Current,
    )
    .expect("the staged set must assemble");

    // The fresh export of the source's current state, through the same
    // serializer the collector and the assembly share.
    let sweep = sqlite_probe::read_opencode_sweep(&db, &sid)
        .expect("read the live session (read-only transaction)");
    let fresh = chat_stasher::opencode_records::compose_v1_line(
        &sweep.session_row,
        &sweep.message_rows,
        &sweep.part_rows,
    )
    .expect("compose the fresh export");

    println!(
        "[sg3-live] machine={machine} sid_prefix={:?}",
        &sid[..8.min(sid.len())]
    );
    println!(
        "[sg3-live] staged_shards={} stage_bytes={} assembled_bytes={} fresh_bytes={}",
        bodies.len(),
        total_bytes,
        assembled.len().saturating_sub(1),
        fresh.len()
    );
    assert_eq!(
        &assembled[..assembled.len() - 1],
        &fresh[..],
        "the assembled default view must equal the fresh export byte for byte at quiescence"
    );
    println!("[sg3-live] byte contract: PASS");
}

/// Which machine partition staged this session. No hostname is assumed: the
/// stage layout is walked and the directory that names this native id wins.
fn discover_machine(stage: &Path, sid: &str) -> String {
    let sessions = stage.join("sessions");
    let entries = std::fs::read_dir(&sessions)
        .unwrap_or_else(|error| panic!("cannot list {}: {error}", sessions.display()));
    for entry in entries {
        let machine_dir = match entry {
            Ok(entry) => entry.path(),
            Err(error) => panic!("cannot list {}: {error}", sessions.display()),
        };
        let Some(machine) = machine_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Ok(records) = std::fs::read_dir(&machine_dir) else {
            continue;
        };
        for record in records {
            let name = record
                .ok()
                .and_then(|entry| entry.file_name().into_string().ok())
                .unwrap_or_default();
            if name.splitn(3, '.').nth(2) == Some(sid) {
                return machine.to_string();
            }
        }
    }
    panic!(
        "no staged opencode session for id {sid} under {} (sessions/<machine>/opencode.<machine>.<sid>)",
        sessions.display()
    );
}
