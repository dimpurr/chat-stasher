//! W961 · T0 of the SYNC-D groundwork — the activity index a full rebuild
//! writes must be a deterministic function of the stage: same stage in,
//! same bytes out, whatever order `fs::read_dir` answers in.
//!
//! The field finding (`nm/W935-OUT.md` §8): the live index held 4,526
//! ascending and 4,520 descending `session_id` transitions — not sorted —
//! because the rebuild emitted rows in readdir order. Two consequences.
//! Two full rebuilds of an *unchanged* stage could differ in bytes, and the
//! byte-for-byte comparison SYNC-D's acceptance gate needs ("incremental
//! update == full rebuild") was unassertable: it would flake on the
//! filesystem's enumeration order instead of failing on the change it
//! exists to catch.
//!
//! This test holds the canonical order both ways. Six body-carrying
//! sessions whose directory names enumerate in neither creation nor sorted
//! order (APFS hashes names; the fixture's id suffixes cover a..f), two
//! consecutive full rebuilds of the untouched stage, and three assertions:
//!
//! * the rows come out in ascending `session_id` — the canonical order
//!   itself, true of a correct rebuild on any filesystem, not a bet on
//!   readdir being unkind;
//! * the two outputs are byte-identical (with the first differing line
//!   named on failure);
//! * their sha256 digests are equal — the quantity the SYNC-D gate
//!   compares.
//!
//! The fixture is body-carrying sessions only, on purpose. A body-less
//! session directory is the reclaim / carry-forward path, whose rows are
//! the next task's equivalence matrix (W935 §4 cases 5, 9–11) — not T0's
//! territory.
//!
//! Black-box through the real binary, matching
//! `run_once_activity_index_test.rs` (`run()`, `write_shard()`, the index
//! path). Synthetic JSONL only: no archive, no real machine's config,
//! data, state or cache.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

#[path = "../src/test_support.rs"]
mod test_support;

/// Run the real binary with every ambient path redirected into `sandbox`.
///
/// `LOCALAPPDATA` is set beside `HOME` for the reason `w267` and `w269`
/// spell out: on Windows the cache root is not a child of `$HOME` at all,
/// so a child given only `HOME` reads and writes the real user's cache
/// while every path it prints looks sandboxed.
fn run(sandbox: &Path, args: &[&str]) -> Output {
    let home = sandbox.join("home");
    let registry = sandbox.join("registry.json");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        &registry,
        r#"{"schema_version":1,"generated":"W961 synthetic","harnesses":[]}"#,
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_chat-stasher"))
        .args(args)
        .env("HOME", &home)
        .env(
            test_support::RUSTIC_CACHE_DIR_ENV,
            test_support::rustic_cache_root(&home),
        )
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

const MACHINE: &str = "mbp-w961";

/// The fixture's six sessions and each one's single timestamp. The id
/// suffixes cover a..f, so ascending order can be told apart from any one
/// creation order, and the timestamps differ, so no two rows are the same
/// bytes — the equality assertions below would be weaker if they were.
const SESSIONS: [(&str, &str); 6] = [
    (
        "019bf00d-97b6-7eb2-9bf8-eacbacc0a00c",
        "2025-01-15T12:04:56.789Z",
    ),
    (
        "019bf00d-97b6-7eb2-9bf8-eacbacc0a00a",
        "2025-01-15T12:14:56.789Z",
    ),
    (
        "019bf00d-97b6-7eb2-9bf8-eacbacc0a00f",
        "2025-01-15T12:24:56.789Z",
    ),
    (
        "019bf00d-97b6-7eb2-9bf8-eacbacc0a00b",
        "2025-01-15T12:34:56.789Z",
    ),
    (
        "019bf00d-97b6-7eb2-9bf8-eacbacc0a00e",
        "2025-01-15T12:44:56.789Z",
    ),
    (
        "019bf00d-97b6-7eb2-9bf8-eacbacc0a00d",
        "2025-01-15T12:54:56.789Z",
    ),
];

fn session_id(session: &str) -> String {
    format!("claude-code.{MACHINE}.{session}")
}

/// One full rebuild of the stage through the real binary, asserted
/// successful, returning the index bytes it published.
fn rebuild(sandbox: &Path, stage: &Path) -> Vec<u8> {
    let out = run(
        sandbox,
        &[
            "activity-index",
            "--stage",
            stage.to_str().unwrap(),
            "--machine",
            MACHINE,
        ],
    );
    assert!(
        out.status.success(),
        "activity-index rebuild failed ({}):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("[activity-index] sessions : {}", SESSIONS.len())),
        "the rebuild must report the session count:\n{stdout}"
    );
    fs::read(stage.join("meta").join(MACHINE).join("activity-v1.jsonl")).unwrap()
}

/// The `session_id` of every index row, in file order.
fn row_session_ids(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| {
            serde_json::from_str::<chat_stasher::activity::ActivityRow>(line)
                .unwrap_or_else(|error| panic!("index line is not an ActivityRow ({error})"))
                .session_id
        })
        .collect()
}

/// The session id one line names, or a placeholder when it is not a row —
/// the failure report names positions, not row text: labels are
/// conversation-derived, synthetic here, and opaque reporting is the habit
/// that keeps it that way.
fn session_id_of(line: &str) -> String {
    serde_json::from_str::<chat_stasher::activity::ActivityRow>(line)
        .map(|row| row.session_id)
        .unwrap_or_else(|_| "<unparseable line>".to_string())
}

/// Where two index byte-strings first disagree — line number plus the two
/// rows' session ids, enough to find a difference from a log that carries
/// no row text.
fn first_difference(first: &[u8], second: &[u8]) -> String {
    let a = String::from_utf8_lossy(first);
    let b = String::from_utf8_lossy(second);
    for (number, (left, right)) in a.lines().zip(b.lines()).enumerate() {
        if left != right {
            return format!(
                "first difference at line {}: `{}` vs `{}`",
                number + 1,
                session_id_of(left),
                session_id_of(right)
            );
        }
    }
    if a.lines().count() != b.lines().count() {
        return format!(
            "line counts differ: {} vs {}",
            a.lines().count(),
            b.lines().count()
        );
    }
    "no line differs, but the bytes do (a trailing newline?)".to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Two consecutive full rebuilds of one unchanged stage must write
/// byte-identical indexes, and the rows must be in the canonical order —
/// ascending `session_id`, never the order `fs::read_dir` answered in.
///
/// Nothing is touched between the rebuilds: no collect, no push, not even
/// a shard mtime. The second rebuild therefore runs against a stage that
/// *does* hold the first rebuild's index — the previous-row read is part
/// of the path under test — and must still come out with the same bytes.
#[test]
fn two_full_rebuilds_of_one_stage_write_identical_bytes() {
    let sb = tempfile::tempdir().unwrap();
    let sandbox = sb.path();
    let stage = sandbox.join("stage");
    for (suffix, ts) in SESSIONS {
        write_shard(&stage, MACHINE, &session_id(suffix), &[cc_line(ts)]);
    }

    let first = rebuild(sandbox, &stage);
    let second = rebuild(sandbox, &stage);

    // The canonical order itself. This does not lean on readdir being
    // unkind to the fixture: a correct rebuild is ascending on any
    // filesystem, and the pre-fix build is red on any filesystem whose
    // readdir is not sorted (APFS hashes names; do not run this against a
    // filesystem whose readdir is sorted and call the order proven — the
    // byte-equality below is what holds there).
    let ids = row_session_ids(&first);
    assert_eq!(ids.len(), SESSIONS.len(), "one row per session: {ids:?}");
    for pair in ids.windows(2) {
        assert!(
            pair[0] < pair[1],
            "rows must be in ascending session_id order, got: {ids:?}"
        );
    }

    // sha256 first — the quantity the SYNC-D gate compares — then the
    // bytes themselves, so a shortened or reordered file fails on the
    // evidence with the first differing line named.
    assert_eq!(
        sha256_hex(&first),
        sha256_hex(&second),
        "two consecutive full rebuilds of an unchanged stage must share one sha256"
    );
    assert_eq!(
        first,
        second,
        "two consecutive full rebuilds of an unchanged stage must be byte-identical ({})",
        first_difference(&first, &second)
    );
}
