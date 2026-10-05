//! W680: an OpenClaw cold transcript archive must never make `collect` fail
//! with `File name too long (os error 63)`.
//!
//! Synthetic fixtures exercise the same shard-directory constructor as collect.

use chat_stasher::id::SessionIdentity;
use chat_stasher::sqlite_probe::{openclaw_cold_native_id, openclaw_native_id};
use chat_stasher::store;
use std::fs;

/// Portable byte budget for one path component on APFS and common POSIX
/// filesystems; conservative for NTFS, which counts UTF-16 units.
const NAME_MAX: usize = 255;

/// The `<source>` label the OpenClaw harness contributes to a session id.
const OPENCLAW: &str = "openclaw";

/// A machine component in the shape `id::normalize_machine` produces (40-char
/// cap, lowercased, `[a-z0-9-]` only). This one is 12 characters.
const MACHINE: &str = "synthetic-i7";

/// An agent directory name under the OpenClaw agents root.
const AGENT: &str = "main";

/// A session id in the shape the OpenClaw SQLite store holds: a UUID.
const SESSION: &str = "11111111-2222-3333-4444-555555555555";

/// A cold transcript archive file name of the shape that produced the field
/// failure: the session's own UUID, then the `.jsonl` transcript, then the
/// `deleted.<timestamp>.<hash>` suffix OpenClaw appends when it rotates a
/// session out, then `.zst`. 112 bytes — the shape is borrowed from a report of
/// a real run, but every character here is synthetic and the timestamp, hash
/// and UUID are invented.
fn cold_file_name() -> String {
    format!("{SESSION}.jsonl.deleted.2026-09-08T21-01-01.201Z.aabbccddeeff00112233445566778899.zst")
}

/// The session id the scanner derives for one cold file, and the stage
/// directory `collect` then has to create for it.
fn cold_session_dir(stage: &std::path::Path, file_name: &str) -> std::path::PathBuf {
    let identity = SessionIdentity {
        source_short: OPENCLAW,
        machine: MACHINE.to_string(),
        native_id: openclaw_cold_native_id(AGENT, SESSION, file_name),
    };
    store::session_shard_dir(stage, MACHINE, &identity.id())
}

/// Every component of `path`, as byte counts. `NAME_MAX` applies to each one
/// separately; the total path length is a different limit and not what failed.
fn component_lengths(path: &std::path::Path) -> Vec<usize> {
    path.components()
        .map(|component| component.as_os_str().len())
        .collect()
}

/// Reproduce the kernel failure through collect's shard-directory constructor.
#[test]
fn a_long_cold_archive_name_yields_a_creatable_shard_directory() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path();
    let shard_dir = cold_session_dir(stage, &cold_file_name());

    // Attempt the filesystem operation first: unfixed code reproduces ENAMETOOLONG.
    fs::create_dir_all(&shard_dir).expect("create the synthetic cold session directory");
    for length in component_lengths(&shard_dir) {
        assert!(
            length <= NAME_MAX,
            "component exceeds the byte limit: {length}"
        );
    }
    assert!(shard_dir.is_dir());
    // The write side, which is what actually archives the session.
    fs::write(shard_dir.join("000001.jsonl"), b"{}\n").unwrap();
    let mut opened = fs::read_dir(&shard_dir).expect("the shard directory is readable");
    assert!(opened.next().is_some());
}

/// The bug was not specific to the one file name that was reported. Any cold
/// name long enough to overflow the composed id must land in the same bounded
/// shape, and a name far longer than the field report's must not push past
/// `NAME_MAX` either.
#[test]
fn cold_names_of_any_length_yield_bounded_directories() {
    let dir = tempfile::TempDir::new().unwrap();
    let stage = dir.path();
    for name in [
        cold_file_name(),
        format!("{SESSION}.jsonl.zst"),
        // A harness that appends more per rotation than the report saw.
        format!(
            "{SESSION}.jsonl.deleted.2026-09-08T21-01-01.201Z.{}.zst",
            "ab".repeat(120)
        ),
        // A session directory name that is itself at the filesystem limit.
        format!("{}.jsonl.zst", "c".repeat(240)),
    ] {
        let shard_dir = cold_session_dir(stage, &name);
        assert!(
            shard_dir
                .file_name()
                .map(|c| c.len())
                .is_some_and(|length| length <= NAME_MAX),
            "a {}-byte cold name produced a {}-byte directory name: {}",
            name.len(),
            shard_dir.file_name().map(|c| c.len()).unwrap_or(0),
            shard_dir.display()
        );
        fs::create_dir_all(&shard_dir).unwrap_or_else(|error| {
            panic!("{} could not be created: {error}", shard_dir.display())
        });
    }
}

/// The bound must not be a truncation. Two cold archives of one native session
/// are separate records — the scanner's own test says so — and real rotated
/// names differ only in their trailing timestamp and hash, which is exactly the
/// part a prefix truncation would drop.
#[test]
fn two_cold_archives_of_one_session_stay_distinct_when_bounded() {
    let base = format!("{SESSION}.jsonl.deleted.2026-09-08T21-0");
    let first = cold_session_dir(
        std::path::Path::new("/stage"),
        &format!("{base}1-01.201Z.aa.zst"),
    );
    let second = cold_session_dir(
        std::path::Path::new("/stage"),
        &format!("{base}9-01.201Z.bb.zst"),
    );
    assert_ne!(
        first, second,
        "two cold archives of one native session collapsed into one directory"
    );
    let third = cold_session_dir(
        std::path::Path::new("/stage"),
        &format!("{base}1-01.202Z.aa.zst"),
    );
    assert_ne!(
        first, third,
        "cold archives differing only in their trailing timestamp collapsed"
    );
}

/// Requirement: a session already archived under a short id must keep exactly
/// that id. Re-identifying an existing session would file it under a second
/// name and break the append-only guarantee, so the short form is pinned to the
/// literal bytes the current code produces — derived here from the documented
/// rule (`oc-` + hex agent + `-` + hex session + `~cold-` + hex file name), not
/// copied out of the implementation.
#[test]
fn a_short_cold_name_keeps_exactly_its_current_id() {
    let name = "sess-short.jsonl.zst";
    let native = openclaw_cold_native_id(AGENT, SESSION, name);
    assert_eq!(
        native,
        concat!(
            "oc-6d61696e",
            "-31313131313131312d323232322d333333332d343434342d353535353535353535353535",
            "~cold-736573732d73686f72742e6a736f6e6c2e7a7374",
        ),
        "a cold id that already fits must not be rewritten"
    );
    let identity = SessionIdentity {
        source_short: OPENCLAW,
        machine: MACHINE.to_string(),
        native_id: native,
    };
    assert_eq!(
        identity.id(),
        concat!(
            "openclaw.synthetic-i7.oc-6d61696e",
            "-31313131313131312d323232322d333333332d343434342d353535353535353535353535",
            "~cold-736573732d73686f72742e6a736f6e6c2e7a7374",
        )
    );
    // The live (non-cold) id for the same session, which the scanner builds
    // from the SQLite rows and appends a generation to.
    assert_eq!(
        openclaw_native_id(AGENT, SESSION),
        "oc-6d61696e-31313131313131312d323232322d333333332d343434342d353535353535353535353535"
    );
    assert_eq!(
        SessionIdentity {
            source_short: OPENCLAW,
            machine: MACHINE.to_string(),
            native_id: format!("{}~g3", openclaw_native_id(AGENT, SESSION)),
        }
        .id(),
        concat!(
            "openclaw.synthetic-i7.oc-6d61696e",
            "-31313131313131312d323232322d333333332d343434342d353535353535353535353535~g3",
        )
    );
}

/// Requirement: the same session maps to the same id on every machine and on
/// every run. The native id is derived only from the source values — nothing in
/// its construction may read a clock, a random source, or the machine — so two
/// machines scanning the same store agree, and one machine scanning twice
/// agrees with itself.
#[test]
fn the_id_is_a_pure_function_of_the_source_values() {
    let name = cold_file_name();
    assert_eq!(
        openclaw_cold_native_id(AGENT, SESSION, &name),
        openclaw_cold_native_id(AGENT, SESSION, &name),
        "two calls over one file name disagree"
    );
    let on_this_machine = SessionIdentity {
        source_short: OPENCLAW,
        machine: MACHINE.to_string(),
        native_id: openclaw_cold_native_id(AGENT, SESSION, &name),
    }
    .id();
    let on_another = SessionIdentity {
        source_short: OPENCLAW,
        machine: "synthetic-thinker".to_string(),
        native_id: openclaw_cold_native_id(AGENT, SESSION, &name),
    }
    .id();
    assert_ne!(
        on_this_machine, on_another,
        "the machine component is not there"
    );
    // Same session, two machines: the ids differ only where the machine is
    // spelled, which is exactly what a per-machine partition is for.
    assert!(
        on_this_machine
            .strip_prefix(OPENCLAW)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|rest| rest.starts_with(&format!("{MACHINE}.")))
            && on_another
                .strip_prefix(OPENCLAW)
                .and_then(|rest| rest.strip_prefix('.'))
                .is_some_and(|rest| rest.starts_with("synthetic-thinker.")),
        "an id does not read as <source>.<machine>.<native-id>: {on_this_machine}"
    );
    // Different sessions and different agents must not land on one id.
    let other_session = SessionIdentity {
        source_short: OPENCLAW,
        machine: MACHINE.to_string(),
        native_id: openclaw_cold_native_id(AGENT, "07bd54e9-1f68-4d94-bc76-8b0e1e3ff4a1", &name),
    }
    .id();
    assert_ne!(on_this_machine, other_session);
    let other_agent = SessionIdentity {
        source_short: OPENCLAW,
        machine: MACHINE.to_string(),
        native_id: openclaw_cold_native_id("ops", SESSION, &name),
    }
    .id();
    assert_ne!(on_this_machine, other_agent);
}

/// The same class of bug on the other id the extension supplies: a chat
/// platform's `sessionId` is accepted up to 512 characters by the native host,
/// and `inbox::session_dir_id` turns it into a stage directory name with no
/// length rule of its own. `has` and `deliver` must keep naming the same
/// directory, so the bound belongs inside that one shared function.
#[test]
fn an_extension_session_id_at_the_protocol_limit_still_names_a_directory() {
    let long = "s".repeat(512);
    let id = chat_stasher::inbox::session_dir_id("deepseek", &long);
    let stage = tempfile::TempDir::new().unwrap();
    let dir = store::session_shard_dir(stage.path(), MACHINE, &id);
    assert!(
        dir.file_name().is_some_and(|c| c.len() <= NAME_MAX),
        "a 512-character session id produced a {}-byte directory name",
        dir.file_name().map(|c| c.len()).unwrap_or(0)
    );
    fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("{} could not be created: {error}", dir.display()));
    // Short ids are untouched: the platform's own directory name is still the
    // prefix, so an existing export layout is unchanged.
    assert_eq!(
        chat_stasher::inbox::session_dir_id("deepseek", "sess-a1"),
        "deepseek.sess-a1"
    );
    assert_ne!(
        chat_stasher::inbox::session_dir_id("deepseek", &long),
        chat_stasher::inbox::session_dir_id("deepseek", &format!("{long}x")),
        "two 512-character ids collapsed into one directory"
    );
}

/// A 180-byte native ID fitted before this fix and must not be shortened.
#[test]
fn a_previously_creatable_cold_id_above_176_bytes_is_unchanged() {
    let file = "f".repeat(50);
    let expected = format!("oc-6d61696e-73~cold-{}", "66".repeat(50));
    // Use a longer session so the native component crosses the former cap.
    let session = "s".repeat(32);
    let native = openclaw_cold_native_id("main", &session, &file);
    let expected_long = format!("oc-6d61696e-{}~cold-{}", "73".repeat(32), "66".repeat(50));
    assert_eq!(native, expected_long);
    assert_eq!(
        SessionIdentity {
            source_short: OPENCLAW,
            machine: MACHINE.into(),
            native_id: native
        }
        .id(),
        format!("{OPENCLAW}.{MACHINE}.{expected_long}")
    );
    assert_eq!(openclaw_cold_native_id("main", "s", &file), expected);
}
