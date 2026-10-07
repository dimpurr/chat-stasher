//! W903 — the DeepSeek official-export parser, asserted from outside the crate.
//!
//! ADR-055 D4 splits the import groundwork into isolated pieces: the producer
//! skeleton (`chat-stasher import`, W901), raw-export archiving (W902), and the
//! per-platform parsers. This file covers the third one through its public
//! surface only: one export file in, conversation-level records or a named
//! failure out.
//!
//! The parser is pure and writes nothing, so this test needs no sandbox: it
//! hands `parse_export` an in-memory synthetic fixture and never touches a data
//! root. Every fixture here is invented — the ids, names and texts say
//! `fixture-`, the URLs are `.invalid`, and nothing in this file was derived
//! from a real export (CONTRIBUTING · "Before opening a change").
//!
//! The three claims worth pinning are the repo's own invariants, read at the
//! import seam:
//!
//! * an unknown is never recorded as empty — a field the export did not carry,
//!   one it wrote as `null`, and one it carried in a shape this build cannot
//!   read stay three different states;
//! * nothing is skipped silently — a conversation that cannot be read is named,
//!   and the count of records does not pretend it was never there;
//! * the export's own `id` is carried through verbatim, because that is the id
//!   our `deepseek.<session id>` web-capture session directory already uses
//!   (27-ORACLE §3), and this parser invents no mapping of its own.

use chat_stasher::import::deepseek::{
    self, AttachmentOrigin, ConversationFailureReason, Field, FragmentKind, MessageSlot,
    ParentLink, RecordedTime,
};

/// Four synthetic conversations: a branching one with attachments and search
/// results, a linear one with an explicit `null` title, a message whose
/// fragments are measured-empty and a message whose `model` cannot be read, a
/// linear one that carries no `title` at all and an unrecognised fragment kind,
/// and one that carries no `mapping` and can therefore only be a named failure.
const EXPORT: &str = r#"[
  {
    "id": "fixture-aaaaaaaa-0000-4000-8000-000000000001",
    "title": "fixture title a",
    "inserted_at": "2025-01-05T03:21:54.163000+08:00",
    "updated_at": "2025-01-05T03:22:00.000000+08:00",
    "mapping": {
      "root": { "id": "root", "parent": null, "children": ["2", "3"], "message": null },
      "2": {
        "id": "2",
        "parent": "root",
        "children": ["4"],
        "message": {
          "files": [{ "id": "fixture-file-1", "file_name": "fixture-a.txt" }],
          "model": "fixture-model",
          "inserted_at": "2025-01-05T03:21:55.000000+08:00",
          "fragments": [{ "type": "REQUEST", "content": "fixture question a" }]
        }
      },
      "3": {
        "id": "3",
        "parent": "root",
        "children": [],
        "message": {
          "files": [],
          "model": "fixture-model",
          "inserted_at": "2025-01-05T03:21:56.000000+08:00",
          "fragments": [{ "type": "THINK", "content": "fixture reasoning" }]
        }
      },
      "4": {
        "id": "4",
        "parent": "2",
        "children": [],
        "message": {
          "files": [],
          "model": "fixture-model",
          "inserted_at": "2025-01-05T03:21:57.000000+08:00",
          "fragments": [
            { "type": "RESPONSE", "content": "fixture answer a" },
            {
              "type": "SEARCH",
              "results": [
                {
                  "url": "https://example.invalid/a",
                  "title": "fixture result",
                  "snippet": "fixture snippet",
                  "cite_index": 1,
                  "published_at": 1718582400.0,
                  "site_icon": "https://example.invalid/icon",
                  "site_name": null,
                  "query_indexes": [0]
                }
              ]
            },
            {
              "type": "FILE",
              "files": [
                { "file_id": "fixture-file-1", "file_name": "fixture-a.txt", "file_size": 107841 }
              ]
            }
          ]
        }
      }
    }
  },
  {
    "id": "fixture-bbbbbbbb-0000-4000-8000-000000000002",
    "title": null,
    "conversation_template_id": "fixture-template",
    "inserted_at": "2025-02-01T10:00:00.000000+08:00",
    "updated_at": "2025-02-01T10:05:00.000000+08:00",
    "mapping": {
      "root": { "id": "root", "parent": null, "children": ["1"], "message": null },
      "1": {
        "id": "1",
        "parent": "root",
        "children": ["2"],
        "message": {
          "files": [],
          "model": "fixture-model",
          "inserted_at": "2025-02-01T10:00:01.000000+08:00",
          "fragments": [{ "type": "REQUEST", "content": "fixture question b" }]
        }
      },
      "2": {
        "id": "2",
        "parent": "1",
        "children": [],
        "message": {
          "files": [],
          "model": 7,
          "inserted_at": "2025-02-01T10:00:02.000000+08:00",
          "fragments": []
        }
      }
    }
  },
  {
    "id": "fixture-cccccccc-0000-4000-8000-000000000003"
  },
  {
    "id": "fixture-dddddddd-0000-4000-8000-000000000004",
    "inserted_at": "2025-03-01T10:00:00.000000+08:00",
    "updated_at": 1741017600,
    "mapping": {
      "root": { "id": "root", "parent": null, "children": ["1"], "message": null },
      "1": {
        "id": "1",
        "parent": "root",
        "children": [],
        "message": {
          "files": [],
          "model": "fixture-model",
          "inserted_at": "2025-03-01T10:00:01.000000+08:00",
          "fragments": [{ "type": "VIDEO", "content": "fixture media answer" }]
        }
      }
    }
  }
]
"#;

fn parsed() -> deepseek::ExportParse {
    deepseek::parse_export(EXPORT.as_bytes())
        .expect("the fixture is a DeepSeek export this build reads")
}

fn record<'a>(parse: &'a deepseek::ExportParse, id: &str) -> &'a deepseek::ConversationRecord {
    parse
        .conversations
        .iter()
        .find(|conversation| conversation.id == id)
        .expect("the fixture carries this conversation")
}

/// The fragments one message carries, for a test that reads them; the states
/// other than a readable list have their own tests.
fn fragments_of(message: &deepseek::MessageRecord) -> &[deepseek::FragmentRecord] {
    match &message.fragments {
        Field::Value(fragments) => fragments,
        other => panic!("the fixture's fragments are readable, found {other:?}"),
    }
}

const BRANCHING: &str = "fixture-aaaaaaaa-0000-4000-8000-000000000001";
const LINEAR: &str = "fixture-bbbbbbbb-0000-4000-8000-000000000002";
const NO_MAPPING: &str = "fixture-cccccccc-0000-4000-8000-000000000003";
const NO_TITLE: &str = "fixture-dddddddd-0000-4000-8000-000000000004";

#[test]
fn the_pilot_shape_reads_end_to_end() {
    let parse = parsed();

    assert_eq!(deepseek::PLATFORM, "deepseek");
    assert_eq!(parse.conversations.len(), 3);
    assert_eq!(parse.failures.len(), 1);
    // 4 + 3 + 2 mapping nodes, of which 3 + 2 + 1 carry a message.
    assert_eq!(parse.node_count(), 9);
    assert_eq!(parse.message_count(), 6);
    // One conversation is a tree with two leaves; the other two are lines.
    assert_eq!(parse.branching_conversations(), 1);
    // Three fields this build could not read, each named where it happened.
    assert_eq!(
        parse.unreadable.len(),
        3,
        "unexpected: {:?}",
        parse.unreadable
    );

    let branching = record(&parse, BRANCHING);
    // The export's own id, verbatim: it is the `<session id>` of our
    // `deepseek.<session id>` web-capture directory, so no mapping is invented.
    assert_eq!(branching.id, BRANCHING);
    assert_eq!(
        branching.created_at,
        RecordedTime::Known {
            unix: 1_736_018_514,
            source: chat_stasher::activity::TimeSource::Exact,
        }
    );
    assert_eq!(branching.nodes.len(), 4);
    assert_eq!(branching.roots().len(), 1);
    assert_eq!(branching.leaves().len(), 2);
    assert_eq!(branching.messages().len(), 3);
    // One message-level `files` entry and one `FILE` fragment's `files` entry.
    assert_eq!(branching.attachments.len(), 2);
    assert!(branching
        .attachments
        .iter()
        .any(|attachment| attachment.origin == AttachmentOrigin::Message));
    assert!(branching
        .attachments
        .iter()
        .any(|attachment| attachment.origin == AttachmentOrigin::Fragment));
    let sized = branching
        .attachments
        .iter()
        .find(|attachment| attachment.origin == AttachmentOrigin::Fragment)
        .expect("the FILE fragment carries one");
    assert_eq!(sized.bytes, Field::Value(107_841));
    // The message-level spelling carries no size at all: absent, never zero.
    let unnamed_size = branching
        .attachments
        .iter()
        .find(|attachment| attachment.origin == AttachmentOrigin::Message)
        .expect("the message carries one");
    assert_eq!(unnamed_size.bytes, Field::Absent);
}

#[test]
fn no_branch_is_claimed_as_current_because_the_export_names_none() {
    let parse = parsed();
    let branching = record(&parse, BRANCHING);

    assert_eq!(
        branching.current_branch,
        deepseek::CurrentBranch::NotNamedBySource
    );
    // Both leaves are kept whole: a branch the source did not name as current is
    // a kept branch (ADR-055 D2), not a dropped line.
    let mut ids: Vec<&str> = branching
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["2", "3", "4", "root"]);
    assert_eq!(branching.roots()[0].message, MessageSlot::NoMessage);
    assert_eq!(branching.roots()[0].parent, ParentLink::Root);
    assert_eq!(
        branching.roots()[0].children,
        Field::Value(vec!["2".to_string(), "3".to_string()])
    );
}

#[test]
fn every_state_of_a_field_stays_its_own_state() {
    let parse = parsed();

    // `title` is carried, written as `null`, and not carried — three states.
    assert!(matches!(record(&parse, BRANCHING).title, Field::Value(_)));
    assert_eq!(record(&parse, LINEAR).title, Field::Null);
    assert_eq!(record(&parse, NO_TITLE).title, Field::Absent);

    // A known field carrying a shape this build cannot read is a fourth state.
    let linear = record(&parse, LINEAR);
    let unreadable_model = linear
        .messages()
        .into_iter()
        .find(|message| matches!(message.model, Field::Unreadable { .. }))
        .expect("one message spells `model` as a number");
    assert!(matches!(unreadable_model.model, Field::Unreadable { .. }));
    // ... and it is counted where it happened, so it is not silently dropped.
    assert_eq!(
        parse
            .unreadable
            .get("conversations[].mapping[].message.model"),
        Some(&1)
    );
    assert_eq!(
        parse
            .unreadable
            .get("conversations[].conversation_template_id"),
        Some(&1)
    );
    // A timestamp spelling this build has not measured is `unknown`, and the
    // unix epoch is never written in its place.
    assert!(matches!(
        record(&parse, NO_TITLE).updated_at,
        RecordedTime::Unknown { .. }
    ));
    assert_eq!(parse.unreadable.get("conversations[].updated_at"), Some(&1));
    assert!(parse.conversations.iter().all(|conversation| !matches!(
        conversation.updated_at,
        RecordedTime::Known { unix: 0, .. }
    )));
}

#[test]
fn a_measured_empty_fragment_list_is_a_zero_and_not_an_unknown() {
    let parse = parsed();
    let linear = record(&parse, LINEAR);
    let empty = linear
        .messages()
        .into_iter()
        .find(
            |message| matches!(&message.fragments, Field::Value(fragments) if fragments.is_empty()),
        )
        .expect("one message carries `\"fragments\": []`");

    // The export's own measurement: the message is reached, it carries no
    // fragment, and nothing about that fact is unreadable.
    assert!(fragments_of(empty).is_empty());
    assert!(!parse
        .unreadable
        .keys()
        .any(|path| path.contains("fragments")));
    assert_eq!(parse.message_count(), 6);
}

#[test]
fn an_unrecognised_fragment_kind_is_named_and_its_text_is_kept() {
    let parse = parsed();
    let no_title = record(&parse, NO_TITLE);
    let fragment = &fragments_of(no_title.messages()[0])[0];

    assert_eq!(
        fragment.kind,
        FragmentKind::Unknown {
            spelled: "VIDEO".to_string()
        }
    );
    // An unknown kind is a fragment this build cannot classify, not a fragment
    // it may treat as empty.
    assert!(matches!(fragment.content, Field::Value(_)));
}

#[test]
fn nothing_is_skipped_silently() {
    let parse = parsed();

    let failure = parse
        .failures
        .iter()
        .find(|failure| failure.id.as_deref() == Some(NO_MAPPING))
        .expect("the conversation without a `mapping` is named, not dropped");
    assert_eq!(failure.reason, ConversationFailureReason::MappingAbsent);
    // The three readable conversations are the only records: the failing one is
    // reported beside them rather than counted as a fourth read success.
    assert_eq!(parse.conversations.len(), 3);
}

#[test]
fn a_file_that_is_not_an_export_is_a_named_failure() {
    assert!(matches!(
        deepseek::parse_export(b"{ not json"),
        Err(deepseek::ExportFailure::NotJson { .. })
    ));
    assert_eq!(
        deepseek::parse_export(b"{\"conversations\": []}"),
        Err(deepseek::ExportFailure::NotAnArray { found: "object" })
    );
    // An export that really does hold no conversation is a measurement, not a
    // failure: zero records, zero failures.
    let empty = deepseek::parse_export(b"[]").expect("an empty export is readable");
    assert!(empty.conversations.is_empty());
    assert!(empty.failures.is_empty());
}
