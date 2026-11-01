//! W918 — the Gemini Takeout activity parser, asserted from outside the crate.
//!
//! ADR-055 D4 splits the import groundwork into isolated pieces: the producer
//! skeleton (`chat-stasher import`, W901), raw-export archiving (W902), and the
//! per-platform parsers. This file covers the Gemini one through its public
//! surface only: one `MyActivity.json` in, observation records or a named
//! failure out.
//!
//! The parser is pure and writes nothing, so this test needs no sandbox: it
//! hands `parse_export` an in-memory synthetic fixture and never touches a data
//! root. Every fixture here is invented — the ids are made of repeated hex
//! digits, the text says `fixture-`, and nothing in this file was derived from a
//! real export (CONTRIBUTING · "Before opening a change").
//!
//! The claims worth pinning from outside are the repo's own invariants read at
//! the import seam:
//!
//! * an unknown is never recorded as empty — a field the export did not carry,
//!   one it wrote as `null`, and one it carried in a shape this build cannot
//!   read stay three different states;
//! * nothing is skipped silently — a row that cannot be read is named, and the
//!   count of observations does not pretend it was never there;
//! * an id-less row is never given a minted session id: it files under the one
//!   [`UNKNOWN_SESSION_ID`] stub with a weak-join marker, so the stub can never
//!   be mistaken for a conversation (ADR-055 D3).

use chat_stasher::import::gemini::{
    self, ExportFailure, Field, Identity, Join, RecordedTime, RowFailureReason, UNKNOWN_SESSION_ID,
};

/// Three synthetic rows: one `Prompted` row that names exactly one conversation,
/// one activity row that names none, and one that names two. Plus one entry that
/// is not an object and can therefore only be a named failure.
const LOG: &str = r#"[
  {
    "header": "Gemini Apps",
    "title": "Prompted fixture one",
    "time": "2026-03-04T05:06:07.008Z",
    "products": ["Gemini Apps"],
    "activityControls": ["Gemini Apps Activity"],
    "details": [
      { "name": "https://gemini.google.com/app/aaaaaaaaaaaaaaaa",
        "url": "https://gemini.google.com/app/aaaaaaaaaaaaaaaa" }
    ],
    "safeHtmlItem": [{ "html": "<b>fixture one</b>" }],
    "attachedFiles": ["fixture-a.pdf"]
  },
  {
    "header": "Gemini Apps",
    "title": "Created fixture gem",
    "time": "2026-03-04T05:06:08.000Z",
    "products": ["Gemini Apps"],
    "activityControls": ["Gemini Apps Activity"],
    "details": null
  },
  {
    "header": "Gemini Apps",
    "title": "Prompted fixture two conversations",
    "time": "2026-03-04T05:06:09.000Z",
    "details": [
      { "name": "a", "url": "https://gemini.google.com/app/aaaaaaaaaaaaaaaa" },
      { "name": "b", "url": "https://gemini.google.com/app/bbbbbbbbbbbbbbbb" }
    ]
  },
  "fixture-not-an-object"
]"#;

fn parsed() -> gemini::ExportParse {
    gemini::parse_export(LOG.as_bytes()).expect("the fixture is an activity log this build reads")
}

#[test]
fn the_measured_shape_reads_end_to_end() {
    let parse = parsed();

    assert_eq!(gemini::PLATFORM, "gemini");
    assert_eq!(parse.observations.len(), 3);
    assert_eq!(parse.failures.len(), 1);
    assert_eq!(parse.failures[0].index, 3);
    assert_eq!(
        parse.failures[0].reason,
        RowFailureReason::NotAnObject { found: "string" }
    );
    // One row names one conversation, one names none, one names two.
    assert_eq!(parse.identified_rows(), 1);
    assert_eq!(parse.unidentified_rows(), 1);
    assert_eq!(parse.ambiguous_rows(), 1);
    assert_eq!(
        parse.conversation_ids(),
        vec![
            "c_aaaaaaaaaaaaaaaa".to_string(),
            "c_bbbbbbbbbbbbbbbb".to_string()
        ]
    );
}

#[test]
fn the_one_conversation_row_joins_on_the_id_our_capture_uses() {
    let parse = parsed();
    let row = &parse.observations[0];

    assert_eq!(
        row.identity,
        Identity::Conversation {
            id: "c_aaaaaaaaaaaaaaaa".to_string()
        }
    );
    assert_eq!(row.session_id(), "c_aaaaaaaaaaaaaaaa");
    assert_eq!(row.join(), Join::ConversationId);
    assert_eq!(
        row.time,
        RecordedTime::Known {
            unix: 1_772_600_767,
            source: chat_stasher::activity::TimeSource::Exact,
        }
    );
    assert_eq!(row.title, Field::Value("Prompted fixture one".to_string()));
    assert_eq!(
        row.safe_html,
        Field::Value(vec!["<b>fixture one</b>".to_string()])
    );
    assert_eq!(
        row.attached_files,
        Field::Value(vec!["fixture-a.pdf".to_string()])
    );
}

#[test]
fn an_id_less_row_gets_the_stub_and_never_a_minted_id() {
    let parse = parsed();
    let row = &parse.observations[1];

    assert_eq!(row.identity, Identity::Unidentified);
    assert_eq!(row.session_id(), UNKNOWN_SESSION_ID);
    assert_eq!(row.join(), Join::WeakPromptText);
    // `details: null` is a stated absence: no id was named, and none is invented.
    assert_eq!(row.details, Field::Null);
    assert!(row.conversation_ids().is_empty());
}

#[test]
fn a_row_that_names_two_conversations_is_ambiguous_not_the_first() {
    let parse = parsed();
    let row = &parse.observations[2];

    assert_eq!(row.identity, Identity::Ambiguous { candidates: 2 });
    assert_eq!(row.session_id(), UNKNOWN_SESSION_ID);
    assert_eq!(row.join(), Join::AmbiguousConversationIds);
    assert_eq!(row.conversation_ids().len(), 2);
}

#[test]
fn a_file_that_is_not_an_activity_log_is_a_named_failure() {
    assert!(matches!(
        gemini::parse_export(b"{ not json"),
        Err(ExportFailure::NotJson { .. })
    ));
    assert_eq!(
        gemini::parse_export(b"{\"rows\": []}"),
        Err(ExportFailure::NotAnArray { found: "object" })
    );
    // An export that really does hold no row is a measurement, not a failure.
    let empty = gemini::parse_export(b"[]").expect("an empty log is readable");
    assert!(empty.observations.is_empty());
    assert!(empty.failures.is_empty());
}
