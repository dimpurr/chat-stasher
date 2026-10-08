//! W947 — the ChatGPT official-export parser, over its public surface.
//!
//! The parser is the ChatGPT arm of ADR-055 D4's groundwork: bytes of one export
//! in, conversation-level records out, wired to no command, writing nothing. The
//! properties this file exists to keep honest are the ones the ChatGPT shape is
//! dangerous about (27-ORACLE §4.2, §4.5):
//!
//!   * **The reading is the longest branch.** A regenerated conversation is a tree
//!     with several leaves, and `current_node` is only the one the web UI last
//!     pointed at. Walking it alone manufactured a false RED on a conversation we
//!     had captured in full, so the export's own pointer is carried *beside* the
//!     reading and never used as the conversation.
//!   * **A partial mapping is its own state, not a count of zero.** A mapping that
//!     names nodes it does not hold cannot answer "which branch is this
//!     conversation", so it carries no branch reading at all — and the counts it
//!     does carry are counts of what is there.
//!   * **An unknown is never recorded as empty** (`CLAUDE.md` · Invariant 1): a
//!     `mapping` that is absent is a named failure and not an empty tree, and a
//!     timestamp outside every range this build reads is `unknown` with a reason,
//!     never `0`.
//!   * **A file that stopped part-way is a different answer from a wrong file**
//!     (Invariant 2): `ExportFailure::NotReadToTheEnd` is not `NotJson`.
//!
//! Provenance, as every synthetic capture/probe test in this repository carries:
//! **all fixtures in this file are synthetic** — `fixture-` tokens, written here,
//! shaped like the measured export — and no request is made to `chatgpt.com` or to
//! any other host. Nothing is read from disk, no process is spawned, nothing is
//! written, and nothing leaves the machine.

use chat_stasher::import::chatgpt::{
    self, ConversationFailureReason, CurrentNodeReading, ExportFailure, RecordedTime, TextField,
    TimeUnit, TreeReading,
};
use serde_json::{json, Map, Value};

const ID_A: &str = "fixture-conversation-a";
const ID_B: &str = "fixture-conversation-b";
/// A fractional epoch second (2023-11-14), inside the plausible window.
const SECOND: f64 = 1_700_000_000.5;

/// One mapping node, as the measured shape spells it.
fn node(id: &str, parent: Value, children: Value, message: Value) -> Value {
    json!({ "id": id, "parent": parent, "children": children, "message": message })
}

/// A mapping object, keyed by each node's own id — how the export files it.
fn mapping(nodes: Vec<Value>) -> Value {
    let mut object = Map::new();
    for entry in nodes {
        let key = entry["id"]
            .as_str()
            .expect("a fixture node names itself")
            .to_string();
        object.insert(key, entry);
    }
    Value::Object(object)
}

/// A message with the keys the measured shape carries.
fn message(role: Value, text: &str) -> Value {
    json!({
        "id": "fixture-message",
        "author": { "role": role, "name": null },
        "content": { "content_type": "text", "parts": [text] },
        "create_time": SECOND,
        "recipient": "all",
        "update_time": null,
    })
}

/// A conversation object carrying the six keys this build reads.
fn conversation(id: &str, mapping: Value, current_node: Value) -> Value {
    json!({
        "conversation_id": id,
        "title": "fixture title",
        "create_time": SECOND,
        "update_time": SECOND,
        "current_node": current_node,
        "mapping": mapping,
    })
}

/// The file's bytes for a list of conversations.
fn export(conversations: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&Value::Array(conversations)).expect("fixtures serialise")
}

/// A linear conversation: one root with no message, then one turn.
fn linear(id: &str) -> Value {
    conversation(
        id,
        mapping(vec![
            node(
                "fixture-root",
                Value::Null,
                json!(["fixture-user"]),
                Value::Null,
            ),
            node(
                "fixture-user",
                json!("fixture-root"),
                json!(["fixture-assistant"]),
                message(json!("user"), "fixture prompt"),
            ),
            node(
                "fixture-assistant",
                json!("fixture-user"),
                json!([]),
                message(json!("assistant"), "fixture answer"),
            ),
        ]),
        json!("fixture-assistant"),
    )
}

/// A conversation with two leaves whose `current_node` is the shorter one.
fn branching(id: &str) -> Value {
    conversation(
        id,
        mapping(vec![
            node(
                "fixture-root",
                Value::Null,
                json!(["fixture-user"]),
                Value::Null,
            ),
            node(
                "fixture-user",
                json!("fixture-root"),
                json!(["fixture-short", "fixture-long"]),
                message(json!("user"), "fixture prompt"),
            ),
            node(
                "fixture-short",
                json!("fixture-user"),
                json!([]),
                message(json!("assistant"), "fixture short"),
            ),
            node(
                "fixture-long",
                json!("fixture-user"),
                json!([]),
                message(json!("assistant"), "fixture much longer answer"),
            ),
        ]),
        json!("fixture-short"),
    )
}

#[test]
fn the_measured_shape_reads_end_to_end() {
    let parsed = chatgpt::parse_export(&export(vec![linear(ID_A), linear(ID_B)]))
        .expect("a well-formed export");

    assert_eq!(chatgpt::PLATFORM, "chatgpt");
    assert_eq!(parsed.conversations.len(), 2);
    assert!(parsed.failures.is_empty());
    assert_eq!(parsed.node_count(), 6);
    assert_eq!(parsed.message_count(), 4);
    assert_eq!(parsed.branching_conversations(), 0);
    assert_eq!(parsed.missing_node_conversations(), 0);
    assert_eq!(parsed.empty_conversations(), 0);

    let first = &parsed.conversations[0];
    assert_eq!(first.id, ID_A);
    // The id is the platform's own, carried verbatim: the join with our
    // `chatgpt.<session id>` web-capture directory is on it (ADR-055 D5).
    assert_eq!(
        format!("{}.{}", chatgpt::PLATFORM, first.id),
        "chatgpt.fixture-conversation-a"
    );
    assert_eq!(first.title, TextField::Value("fixture title".to_string()));
    assert_eq!(first.roots().len(), 1, "one node names no parent");
    assert_eq!(
        first.current_node,
        CurrentNodeReading::Named {
            id: "fixture-assistant".to_string()
        }
    );
    match &first.tree {
        TreeReading::Consistent(facts) => {
            assert_eq!((facts.nodes, facts.messages), (3, 2));
            assert_eq!((facts.roots, facts.leaves), (1, 1));
            assert_eq!(facts.longest_branch.turns, 2);
            assert_eq!(facts.current_node_is_the_longest_leaf(), Some(true));
        }
        other => panic!("expected a walked tree, got {other:?}"),
    }
    // The keys this build does not read are named, not dropped in silence.
    assert!(
        parsed.unreadable.is_empty(),
        "this fixture carries only keys the parser reads: {:?}",
        parsed.unreadable
    );
}

/// 27-ORACLE §4.5, through the public surface: the reading follows the longest
/// branch, and the export's own `current_node` is a statement about itself.
#[test]
fn the_reading_is_the_longest_branch_where_the_export_s_pointer_is_not() {
    let parsed =
        chatgpt::parse_export(&export(vec![branching(ID_A)])).expect("a well-formed export");
    assert_eq!(parsed.branching_conversations(), 1);
    let TreeReading::Consistent(facts) = &parsed.conversations[0].tree else {
        panic!(
            "expected a walked tree, got {:?}",
            parsed.conversations[0].tree
        );
    };
    assert_eq!(facts.leaves, 2);
    assert_eq!(facts.longest_branch.end, "fixture-long");
    assert_eq!(
        facts
            .current_branch
            .as_ref()
            .map(|branch| branch.end.as_str()),
        Some("fixture-short")
    );
    assert_eq!(
        facts.current_node_is_the_longest_leaf(),
        Some(false),
        "the export's pointer is not the deepest branch, and the reading says so"
    );
}

#[test]
fn both_epoch_units_are_read_and_the_unit_is_named() {
    let mut in_millis = linear(ID_A);
    in_millis["create_time"] = json!((SECOND * 1000.0).round());
    let parsed = chatgpt::parse_export(&export(vec![in_millis])).expect("a well-formed export");
    assert_eq!(
        parsed.conversations[0].created,
        RecordedTime::Known {
            unix: SECOND as i64,
            unit: TimeUnit::Milliseconds,
        }
    );

    let parsed = chatgpt::parse_export(&export(vec![linear(ID_A)])).expect("a well-formed export");
    assert_eq!(
        parsed.conversations[0].created,
        RecordedTime::Known {
            unix: SECOND as i64,
            unit: TimeUnit::Seconds,
        }
    );

    // A number outside both ranges is unknown, never zero and never clamped.
    let mut nonsense = linear(ID_A);
    nonsense["create_time"] = json!(7);
    let parsed = chatgpt::parse_export(&export(vec![nonsense])).expect("a well-formed export");
    assert!(matches!(
        parsed.conversations[0].created,
        RecordedTime::Unknown { .. }
    ));
}

/// The state this slice exists for. A partial mapping is its own state, it is
/// counted as partial rather than as a conversation with no messages, and it is
/// **not** counted among the branch-ambiguous conversations — a tree with a hole
/// in it has no leaf count to compare.
#[test]
fn a_mapping_with_missing_nodes_is_its_own_state_and_not_a_zero() {
    let mut broken = branching(ID_A);
    broken["mapping"]["fixture-user"]["children"] =
        json!(["fixture-short", "fixture-long", "fixture-not-here"]);
    let parsed = chatgpt::parse_export(&export(vec![branching(ID_B), broken]))
        .expect("a well-formed export");

    assert_eq!(parsed.conversations.len(), 2);
    assert_eq!(parsed.missing_node_conversations(), 1);
    assert_eq!(
        parsed.branching_conversations(),
        1,
        "only the conversation whose tree could be walked has a leaf count"
    );
    let TreeReading::MissingNodes(missing) = &parsed.conversations[1].tree else {
        panic!(
            "expected the partial state, got {:?}",
            parsed.conversations[1].tree
        );
    };
    assert_eq!(missing.unresolved_children, 1);
    assert_eq!((missing.nodes, missing.messages), (4, 3));
    assert!(!missing.everything_resolved());
    // The nodes are still kept whole: nothing is thrown away for being partial.
    assert_eq!(parsed.conversations[1].nodes.len(), 4);
}

#[test]
fn an_empty_conversation_is_not_a_partial_one_and_not_a_failure() {
    let parsed = chatgpt::parse_export(&export(vec![conversation(
        ID_A,
        mapping(vec![]),
        Value::Null,
    )]))
    .expect("a well-formed export");
    assert!(parsed.failures.is_empty());
    assert_eq!(parsed.empty_conversations(), 1);
    assert_eq!(parsed.missing_node_conversations(), 0);
    assert_eq!(parsed.message_count(), 0, "a measured zero");
    assert_eq!(parsed.conversations[0].tree, TreeReading::NoNodes);
}

/// Nothing is skipped in silence: a conversation that cannot become a record is a
/// named failure beside the records, with its id when it named one (ADR-014).
#[test]
fn nothing_is_skipped_in_silence() {
    let mut no_mapping = linear(ID_B);
    no_mapping
        .as_object_mut()
        .expect("a fixture conversation is an object")
        .remove("mapping");
    let parsed = chatgpt::parse_export(&export(vec![
        linear(ID_A),
        no_mapping,
        json!("fixture not an object"),
    ]))
    .expect("a well-formed export");

    assert_eq!(
        parsed.conversations.len(),
        1,
        "the one readable conversation"
    );
    assert_eq!(parsed.failures.len(), 2);
    assert_eq!(parsed.failures[0].id.as_deref(), Some(ID_B));
    assert_eq!(
        parsed.failures[0].reason,
        ConversationFailureReason::MappingAbsent
    );
    assert!(parsed.failures[1].id.is_none());
    assert!(matches!(
        parsed.failures[1].reason,
        ConversationFailureReason::NotAnObject { .. }
    ));
}

#[test]
fn a_file_that_stopped_part_way_is_not_the_same_answer_as_a_wrong_file() {
    let whole = export(vec![linear(ID_A)]);
    assert!(matches!(
        chatgpt::parse_export(&whole[..whole.len() / 2]),
        Err(ExportFailure::NotReadToTheEnd { .. })
    ));
    assert!(matches!(
        chatgpt::parse_export(b"[{\"conversation_id\": }]"),
        Err(ExportFailure::NotJson { .. })
    ));
    assert!(matches!(
        chatgpt::parse_export(b"42"),
        Err(ExportFailure::NotAnExport { found: "a number" })
    ));
}
