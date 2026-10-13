//! Codebuff / Freebuff (`manicode`) capture-side facts and control records
//! (ADR-054).
//!
//! One product, two npm names (D1); on disk the layout is
//! `~/.config/manicode[-dev|-staging]/projects/<basename>/chats/<chatId>/`,
//! and the whitelist is exactly `chat-messages.json` plus `chat-meta.json`
//! (D5) — sealed whole-file as raw bytes, one full generation per changed
//! pass (D2).
//!
//! This module owns the three capture-side disciplines the rest of the
//! pipeline needs:
//!
//! * the chat directory name is the harness's own session-start time (D4):
//!   an exact shape is accepted and the instant is *recorded at capture*
//!   with its source named — never re-read from the archive id, whose
//!   byte-bounding can drop the time-shaped tail entirely;
//! * a transcript that cannot be read whole and validated as a JSON array
//!   skips its session for the pass (D6): nothing is sealed, and a bounded
//!   closed-enum observation records the skip inside the encrypted stage;
//! * the recorded start time is cross-checked against the session
//!   directory's own mtime (D4): a directory that *precedes* the recorded
//!   time is an observation, and the recorded time still stands — a file
//!   time never overwrites a harness-written fact.
//!
//! Every record this module writes is a control record: counts, ids, fixed
//! enum values and instants only — no conversation text, no message
//! content, no `firstPrompt`, no tokens or credentials, no absolute source
//! paths (ADR-046: a path can carry a username; the root is named by its
//! registry id instead).

use std::io::Write;
use std::path::Path;

use serde::de::{IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

/// The only `CapturedStart::source` value this build writes (ADR-054 D4):
/// the start time was read out of the chat directory's own name.
pub const START_SOURCE_CHAT_DIRECTORY_NAME: &str = "chat-directory-name";

/// File name of the skip-observation stream, one JSONL line per skipped
/// session per pass, under `<stage>/meta/<machine>/` — the same
/// per-machine-metadata directory the manifest lives in, so the stream
/// travels with the stage into the encrypted archive (ADR-054 D6: "like
/// ADR-046's control stream, the observation stream stays inside the
/// encrypted archive").
pub const SKIP_FILE: &str = "codebuff-skip-v1.jsonl";

/// File name of the start-time cross-check observation stream, one JSONL
/// line per *mismatch* (a session directory whose own mtime precedes the
/// recorded start time), under `<stage>/meta/<machine>/` (ADR-054 D4).
/// Empty is the normal case; a mismatch is an anomaly worth one bounded
/// record, not a reason to rewrite the recorded time.
pub const CROSSCHECK_FILE: &str = "codebuff-crosscheck-v1.jsonl";

/// One skip observation (ADR-054 D6). Fields, exactly, and nothing else:
/// free-form text anywhere in this record is a design bug — the reason is a
/// closed enum so a skip can never smuggle content into the control tier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkipObservation {
    /// Identifies the collecting run. Minted once per pass, never per
    /// session: bounded hex, no hostname, no machine path.
    pub run_id: String,
    /// Unix seconds of the pass.
    pub pass_ts: i64,
    /// The registry `source_roots[].id` whose walk found the session —
    /// `codebuff` / `codebuff-dev` / `codebuff-staging` (with an in-force
    /// entry-wide override, the primary `codebuff` root) — never a
    /// filesystem path.
    pub root_id: String,
    /// The session's native id — the chat's on-disk identity
    /// `<project-basename>.<chatId>` — directory-name components, never a
    /// path, no content.
    pub session_native_id: String,
    /// Byte size of the unread snapshot: the read's own length when the
    /// read got that far, else the scanner's stat of the file.
    pub byte_size: u64,
    /// Closed enum, see [`TranscriptSkipReason`].
    pub reason: String,
    /// 1-based count of the observations already in the stream for this
    /// (root, session) pair, plus this one: how many passes the session has
    /// gone unread.
    pub attempt: u32,
}

/// Why a codebuff transcript was skipped for its pass (ADR-054 D6). A closed
/// enum on purpose: the reason is a word from this list, never a diagnostic
/// string, an error message, or file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptSkipReason {
    /// Bytes were read but are not parsable JSON — a torn or truncated
    /// transcript (the write is atomic at source, so this is a crash
    /// mid-write until the next pass proves otherwise).
    TornJson,
    /// Parsable JSON, but not an array — the harness's own reader refuses
    /// anything else, and so does ours.
    NotAnArray,
    /// The file could not be opened or read (permissions, or the file went
    /// missing between scan and read).
    Unreadable,
    /// Any other I/O failure during the read.
    IoError,
}

impl TranscriptSkipReason {
    /// The closed-enum word recorded in [`SkipObservation::reason`].
    pub fn as_str(self) -> &'static str {
        match self {
            TranscriptSkipReason::TornJson => "torn-json",
            TranscriptSkipReason::NotAnArray => "not-an-array",
            TranscriptSkipReason::Unreadable => "unreadable",
            TranscriptSkipReason::IoError => "io-error",
        }
    }
}

/// One start-time cross-check mismatch (ADR-054 D4): the session
/// directory's own mtime is earlier than the recorded start time. The
/// observation stands beside the recorded value — the recorded time is
/// never overwritten by a file time, and this row is what says the check
/// ran and what it saw.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossCheckObservation {
    /// Unix seconds of the pass.
    pub pass_ts: i64,
    /// The registry `source_roots[].id` — never a path (see
    /// [`SkipObservation::root_id`]).
    pub root_id: String,
    /// The session's native id (`<project-basename>.<chatId>`).
    pub session_native_id: String,
    /// The recorded start time, verbatim.
    pub recorded_start: String,
    /// `"chat-directory-name"` — the closed set the value was captured from.
    pub recorded_start_source: String,
    /// The session directory's own mtime, unix seconds.
    pub dir_mtime_unix: i64,
    /// Closed value: the only result this stream ever records, because a
    /// clean cross-check writes nothing.
    pub result: String,
}

/// The only `CrossCheckObservation::result` value: the directory's own
/// mtime precedes the recorded start time.
pub const CROSSCHECK_RESULT_PRECEDES: &str = "dir-mtime-precedes-recorded-start";

// -------------------------------------------------------------------- D4 time

/// Accept a codebuff chat directory name only when it is the harness's own
/// encoding of a session-start instant (ADR-054 F5/D4):
/// `YYYY-MM-DDThh-mm-ss.mmmZ`, with the two `:` restored in the time half
/// and read as UTC. Anything else — a renamed folder, a hand-made
/// directory, a future format — records **no** start time: never
/// approximated, never reconstructed, and never silently replaced by a
/// file time. A shape that matches but is not a real instant (day 32, hour
/// 25) is also refused: "read it as UTC" is a claim about a time that
/// exists.
pub fn captured_start_from_chat_dir_name(name: &str) -> Option<crate::manifest::CapturedStart> {
    // The exact accepted shape, checked byte by byte because a glob would
    // accept more than the harness's own clock ever writes:
    //
    //     2026-10-05T18-07-03.513Z
    //
    let bytes = name.as_bytes();
    let accepted = bytes.len() == 24
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[10] == b'T'
        && bytes[11..13].iter().all(u8::is_ascii_digit)
        && bytes[13] == b'-'
        && bytes[14..16].iter().all(u8::is_ascii_digit)
        && bytes[16] == b'-'
        && bytes[17..19].iter().all(u8::is_ascii_digit)
        && bytes[19] == b'.'
        && bytes[20..23].iter().all(u8::is_ascii_digit)
        && bytes[23] == b'Z';
    if !accepted {
        return None;
    }
    // The shape check guarantees pure ASCII, so the byte slices below are
    // character boundaries. Restore the two `:` the file system cannot
    // carry: `<date>T<hh>:<mm>:<ss>.<mmm>Z`, read as UTC.
    let rfc3339 = format!("{}:{}:{}", &name[..13], &name[14..16], &name[17..]);
    // A real instant or no time at all: the digit shape alone does not
    // make "2026-10-32" a date the calendar accepts.
    chrono::DateTime::parse_from_rfc3339(&rfc3339).ok()?;
    Some(crate::manifest::CapturedStart {
        time: rfc3339,
        source: START_SOURCE_CHAT_DIRECTORY_NAME.to_string(),
    })
}

/// The recorded start time as a `SystemTime`, for the D4 cross-check.
/// `None` when the value does not parse back — an unparseable recorded time
/// is left un-compared, never guessed.
pub fn captured_start_as_system_time(
    recorded: &crate::manifest::CapturedStart,
) -> Option<std::time::SystemTime> {
    let parsed = chrono::DateTime::parse_from_rfc3339(&recorded.time).ok()?;
    let millis = u64::try_from(parsed.timestamp_millis()).ok()?;
    std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(millis))
}

// ------------------------------------------------------------ D6 boxed verdict

/// The outcome of reading one codebuff transcript snapshot (ADR-054 D6):
/// a valid JSON array, or the closed-enum reason it must skip its session
/// this pass. The byte buffer travels with the verdict so the caller
/// records the snapshot's own size and seals **this** buffer only — never a
/// re-read of the path (D6 rev 2: one snapshot for validation and
/// sealing).
#[derive(Debug)]
pub enum TranscriptRead {
    /// The snapshot is a parseable JSON array; seal exactly these bytes.
    Array(Vec<u8>),
    /// The snapshot cannot be sealed this pass. `bytes` carries what the
    /// read got — `None` when the file could not be opened at all — so the
    /// skip observation can state the measured size. Its absence proves
    /// nothing about the contents and nothing is sealed either way.
    Rejected {
        bytes: Option<Vec<u8>>,
        reason: TranscriptSkipReason,
    },
}

/// A `Deserialize` that accepts only a JSON array and walks it without
/// building values: validation costs a token pass over the bytes, never a
/// materialized tree — the whole-file shape already holds one full copy of
/// the transcript and must not pay for a second. Element shapes are
/// deliberately not checked: raw fidelity (ADR-054 D2) seals what the
/// harness wrote, including messages this build cannot name.
#[derive(Debug)]
struct ArrayOnly;

impl<'de> Deserialize<'de> for ArrayOnly {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ArrayVisitor;
        impl<'de> Visitor<'de> for ArrayVisitor {
            type Value = ArrayOnly;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a JSON array — the harness's own reader refuses anything else, so does ours",
                )
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(ArrayOnly)
            }
        }
        deserializer.deserialize_seq(ArrayVisitor)
    }
}

/// Read the transcript file into one buffer and validate it as a JSON
/// array (ADR-054 D6). Read failures map onto the closed enum:
/// `unreadable` for a file that could not even be opened (permissions, or
/// missing between scan and read — proving nothing about its contents),
/// `io-error` for any other I/O failure, `torn-json` for bytes that do not
/// parse, `not-an-array` for bytes that parse into anything else.
pub fn read_transcript(path: &Path) -> TranscriptRead {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            let reason = match error.kind() {
                std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::NotFound
                | std::io::ErrorKind::IsADirectory => TranscriptSkipReason::Unreadable,
                _ => TranscriptSkipReason::IoError,
            };
            return TranscriptRead::Rejected {
                bytes: None,
                reason,
            };
        }
    };
    match serde_json::from_slice::<ArrayOnly>(&bytes) {
        Ok(ArrayOnly) => TranscriptRead::Array(bytes),
        // Errors raised by our own arity visitor classify as `Data`
        // (invalid type for the array we demanded); a torn or truncated
        // file classifies as anything else. The two spellings must stay
        // distinct because D6 makes them two different words.
        Err(error) => TranscriptRead::Rejected {
            reason: match error.classify() {
                serde_json::error::Category::Data => TranscriptSkipReason::NotAnArray,
                _ => TranscriptSkipReason::TornJson,
            },
            bytes: Some(bytes),
        },
    }
}

// ------------------------------------------------- observation stream writes

fn meta_file_path(stage: &Path, machine: &str, file: &str) -> std::path::PathBuf {
    stage
        .join(crate::manifest::META_DIR)
        .join(machine)
        .join(file)
}

/// Append one skip observation to the machine's stream, counting the
/// observations already in the stream for the same (root, session) pair so
/// [`SkipObservation::attempt`] says how many passes the session has gone
/// unread. Appending is deliberate: the stream is a per-pass history, and
/// rewriting it would turn "unread three passes in a row" back into
/// "unread once".
pub fn append_skip_observation(
    stage: &Path,
    machine: &str,
    observation: SkipObservation,
) -> anyhow::Result<()> {
    let path = meta_file_path(stage, machine, SKIP_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::Error::new(e).context(format!("create {}", parent.display())))?;
    }
    // ADR-054 D6 names the pair: (root, session), reason not included. The
    // count is "how many passes this session has gone unread", and a session
    // that was torn and then stopped opening is unread through all of it.
    let attempt = 1 + observation_count_matching(&path, |row| {
        row.root_id == observation.root_id && row.session_native_id == observation.session_native_id
    });
    let row = SkipObservation {
        attempt,
        ..observation
    };
    append_jsonl_line(&path, &row)
}

/// Read every skip observation of one machine, oldest line first. A file
/// that is absent observes nothing. A line that does not parse is a
/// hardened control stream that is corrupt — refused loudly rather than
/// skipped quietly, because a skip observation is the one record that says a
/// session was deliberately *not* archived.
pub fn read_skip_observations(stage: &Path, machine: &str) -> anyhow::Result<Vec<SkipObservation>> {
    let path = meta_file_path(stage, machine, SKIP_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("read {}", path.display()))),
    };
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: SkipObservation = serde_json::from_str(line).map_err(|e| {
            anyhow::Error::new(e).context(format!("parse {} line {}", path.display(), index + 1))
        })?;
        rows.push(row);
    }
    Ok(rows)
}

fn observation_count_matching(path: &Path, matches: impl Fn(&SkipObservation) -> bool) -> u32 {
    // reason: the count is a convenience the appended row explains ("how many
    // passes so far"), never a gate on recording the skip itself (D6). A
    // stream too broken to count reads as no proven attempts — this pass is
    // the first provable one — and the loud read path for humans
    // (`read_skip_observations`) still refuses the broken stream rather
    // than hiding it.
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<SkipObservation>(line).ok())
        .filter(|row| matches(row))
        .count() as u32
}

/// Append one start-time cross-check mismatch observation (ADR-054 D4).
/// Written only on mismatch: a clean cross-check is the normal state and
/// records nothing.
pub fn append_crosscheck_observation(
    stage: &Path,
    machine: &str,
    observation: CrossCheckObservation,
) -> anyhow::Result<()> {
    let path = meta_file_path(stage, machine, CROSSCHECK_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::Error::new(e).context(format!("create {}", parent.display())))?;
    }
    append_jsonl_line(&path, &observation)
}

fn append_jsonl_line(path: &Path, row: &impl Serialize) -> anyhow::Result<()> {
    let mut line =
        serde_json::to_string(row).map_err(|e| anyhow::Error::new(e).context(serialize_error()))?;
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(line.as_bytes()))
        .map_err(|e| anyhow::Error::new(e).context(format!("append {}", path.display())))?;
    Ok(())
}

fn serialize_error() -> &'static str {
    // Both stream rows are plain strings and numbers; a serialization
    // failure means a control record grew a field that cannot serialize,
    // which the shape tests below catch.
    "serialize a codebuff observation row"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_dir_name_shape_decides_the_captured_start() {
        let captured =
            captured_start_from_chat_dir_name("2026-10-05T18-07-03.513Z").expect("shape accepted");
        assert_eq!(captured.time, "2026-10-05T18:07:03.513Z");
        assert_eq!(captured.source, START_SOURCE_CHAT_DIRECTORY_NAME);
    }

    #[test]
    fn non_matching_chat_dir_names_record_no_start_time() {
        for name in [
            "",
            "2026-10-05T18-07-03.51Z",
            "2026-10-05T18-07-03.5133Z",
            "2026-10-05T18-07-03.513",
            "2026-10-05 18-07-03.513Z",
            "2026-10-05T18-07-03,513Z",
            "x2026-10-05T18-07-03.513Z",
            "project",
        ] {
            assert!(
                captured_start_from_chat_dir_name(name).is_none(),
                "{name:?} must record no start time"
            );
        }
    }

    #[test]
    fn a_calendar_impossible_shape_is_refused_even_with_matching_digits() {
        // Day 32 and hour 25 match the digit shape but are not instants: a
        // recorded time must be "read as UTC", which requires a real one.
        assert!(captured_start_from_chat_dir_name("2026-10-32T18-07-03.513Z").is_none());
        assert!(captured_start_from_chat_dir_name("2026-10-05T25-07-03.513Z").is_none());
        assert!(captured_start_from_chat_dir_name("2026-02-30T18-07-03.513Z").is_none());
        assert!(captured_start_from_chat_dir_name("2023-02-29T18-07-03.513Z").is_none());
    }

    #[test]
    fn a_leap_day_is_a_real_instant() {
        assert!(
            captured_start_from_chat_dir_name("2024-02-29T18-07-03.513Z").is_some(),
            "2024 is a leap year, so the 29th of February is a start time"
        );
    }

    #[test]
    fn transcript_validation_accepts_arrays_of_any_shape() {
        let dir = tempfile::tempdir().unwrap();
        for body in [
            &b"[]"[..],
            b"[1, 2, 3]",
            b"[{\"variant\":\"user\",\"content\":\"hello\"}]",
            b"  [ ]  ",
        ] {
            let path = dir.path().join("chat-messages.json");
            std::fs::write(&path, body).unwrap();
            match read_transcript(&path) {
                TranscriptRead::Array(bytes) => assert_eq!(bytes, body),
                TranscriptRead::Rejected { reason, .. } => {
                    panic!("{:?} must validate as an array, got {:?}", body, reason)
                }
            }
        }
    }

    #[test]
    fn not_an_array_parses_but_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for body in [
            br#"{"messageCount":3}"# as &[u8],
            br#""scalar""#,
            b"null",
            b"42",
        ] {
            let path = dir.path().join("chat-messages.json");
            std::fs::write(&path, body).unwrap();
            match read_transcript(&path) {
                TranscriptRead::Rejected {
                    reason: TranscriptSkipReason::NotAnArray,
                    bytes: Some(bytes),
                } => assert_eq!(bytes, body),
                other => panic!("{:?} must be a `not-an-array` skip, got {other:?}", body),
            }
        }
    }

    #[test]
    fn a_torn_transcript_is_measured_but_never_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chat-messages.json");
        let torn: &[u8] = br#"[{"variant":"user","content":"trunca"#;
        std::fs::write(&path, torn).unwrap();
        match read_transcript(&path) {
            TranscriptRead::Rejected {
                reason: TranscriptSkipReason::TornJson,
                bytes: Some(bytes),
            } => {
                assert_eq!(bytes, torn.to_vec());
                assert!(!bytes.is_empty(), "the snapshot's size is measurable");
            }
            other => panic!("torn file must be rejected with a measured size, got {other:?}"),
        }
        // Garbage after a valid array is just as unparsable.
        std::fs::write(&path, b"[]garbage").unwrap();
        match read_transcript(&path) {
            TranscriptRead::Rejected {
                reason: TranscriptSkipReason::TornJson,
                ..
            } => {}
            other => panic!("trailing garbage must be torn, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_file_is_unreadable_and_proves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chat-messages.json");
        match read_transcript(&path) {
            TranscriptRead::Rejected {
                reason: TranscriptSkipReason::Unreadable,
                bytes: None,
            } => {}
            other => panic!("missing file must be an `unreadable` skip, got {other:?}"),
        }
    }

    #[test]
    fn skip_observations_count_attempts_and_carry_exactly_the_bounded_fields() {
        let stage = tempfile::tempdir().unwrap();
        for pass_ts in [1_760_000_000, 1_760_003_600] {
            append_skip_observation(
                stage.path(),
                "synthetic-machine",
                SkipObservation {
                    run_id: "run-1".to_string(),
                    pass_ts,
                    root_id: "codebuff".to_string(),
                    session_native_id: "proj.2026-10-05T18-07-03.513Z".to_string(),
                    byte_size: 1_024,
                    reason: TranscriptSkipReason::TornJson.as_str().to_string(),
                    attempt: 0,
                },
            )
            .unwrap();
        }
        let rows = read_skip_observations(stage.path(), "synthetic-machine").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].attempt, 1);
        assert_eq!(rows[1].attempt, 2);
        let value = serde_json::to_value(&rows[0]).unwrap();
        let object = value.as_object().expect("row serializes to an object");
        let mut fields: Vec<&str> = object.keys().map(String::as_str).collect();
        fields.sort_unstable();
        // The field set is the contract (ADR-054 D6): exactly these seven,
        // never a free-form diagnostic.
        assert_eq!(
            fields,
            [
                "attempt",
                "byte_size",
                "pass_ts",
                "reason",
                "root_id",
                "run_id",
                "session_native_id"
            ]
        );
    }

    #[test]
    fn observations_of_two_sessions_count_their_own_attempts() {
        let stage = tempfile::tempdir().unwrap();
        for session in [
            "proj-a.2026-10-05T18-07-03.513Z",
            "proj-b.2026-10-05T18-07-03.513Z",
        ] {
            for pass_ts in [1, 2, 3] {
                append_skip_observation(
                    stage.path(),
                    "synthetic-machine",
                    SkipObservation {
                        run_id: format!("run-{pass_ts}"),
                        pass_ts,
                        root_id: "codebuff".to_string(),
                        session_native_id: session.to_string(),
                        byte_size: 10,
                        reason: TranscriptSkipReason::TornJson.as_str().to_string(),
                        attempt: 0,
                    },
                )
                .unwrap();
            }
        }
        let rows = read_skip_observations(stage.path(), "synthetic-machine").unwrap();
        assert_eq!(rows.len(), 6);
        for session in [
            "proj-a.2026-10-05T18-07-03.513Z",
            "proj-b.2026-10-05T18-07-03.513Z",
        ] {
            let last = rows
                .iter()
                .rfind(|row| row.session_native_id == session)
                .expect("three rows per session");
            assert_eq!(last.attempt, 3, "{session} has three passes recorded");
        }
    }

    #[test]
    fn a_reason_change_does_not_restart_the_attempt_count() {
        // ADR-054 D6 counts by the (root, session) pair, not by reason: a
        // session that was torn and then stopped opening is unread through
        // all of it, and the streak must not silently shrink.
        let stage = tempfile::tempdir().unwrap();
        for reason in [
            TranscriptSkipReason::TornJson,
            TranscriptSkipReason::TornJson,
            TranscriptSkipReason::Unreadable,
        ] {
            append_skip_observation(
                stage.path(),
                "synthetic-machine",
                SkipObservation {
                    run_id: "run-1".to_string(),
                    pass_ts: 1,
                    root_id: "codebuff".to_string(),
                    session_native_id: "proj.2026-10-05T18-07-03.513Z".to_string(),
                    byte_size: 5,
                    reason: reason.as_str().to_string(),
                    attempt: 0,
                },
            )
            .unwrap();
        }
        let rows = read_skip_observations(stage.path(), "synthetic-machine").unwrap();
        assert_eq!(rows[0].attempt, 1);
        assert_eq!(rows[1].attempt, 2);
        assert_eq!(
            rows[2].attempt, 3,
            "the unread streak survives a reason change"
        );
    }

    #[test]
    fn an_absent_observation_file_is_no_observations() {
        let stage = tempfile::tempdir().unwrap();
        assert!(read_skip_observations(stage.path(), "synthetic-machine")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_control_stream_row_carries_no_content_field() {
        // The canary discipline, structurally: a row built from ordinary
        // skip facts must serialize without any free-form text beyond the
        // closed enum. The serialized line never contains message content
        // because no field can.
        let row = SkipObservation {
            run_id: "run-1".to_string(),
            pass_ts: 1_760_000_000,
            root_id: "codebuff-staging".to_string(),
            session_native_id: "my-project.2026-10-05T18-07-03.513Z".to_string(),
            byte_size: 9,
            reason: TranscriptSkipReason::NotAnArray.as_str().to_string(),
            attempt: 1,
        };
        let line = serde_json::to_string(&row).unwrap();
        for forbidden in ["content", "prompt", "text", "firstPrompt"] {
            assert!(
                !line.contains(forbidden),
                "no control row may carry a {forbidden} field: {line}"
            );
        }
    }
}
