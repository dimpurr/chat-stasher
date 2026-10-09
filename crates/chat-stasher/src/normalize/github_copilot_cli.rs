//! GitHub Copilot CLI — reader for the `session-store.db` export.
//!
//! The export [`crate::sqlite_probe::read_github_copilot_session`] writes
//! carries one `sessions` row plus the session's `turns` rows, in one JSON
//! line. A turn is a recorded pair: the `user_message` the person sent and
//! the `assistant_response` that came back (`turn_index` orders them). Each
//! side is rendered as its own message, because that is what the pair is —
//! two speakers, one turn — and a side that is missing was never a blank
//! speaker (the export keeps it `null`/absent; `missing ≠ empty`).
//!
//! A turn whose `user_message`/`assistant_response` is not text (or holds
//! nothing readable) is counted, not guessed into prose. The same rule serves
//! a whole export whose shape is not this schema: the count says "here are
//! bytes this reader could not read", and the raw route keeps them.
//!
//! Per-message time is not claimed. The schema this reader is built from was
//! read out of the shipped v1.0.80 build artifacts, and no real
//! `session-store.db` has ever been observed, so every message time stays
//! unknown with the reason written down — the recorded-but-unmeasured
//! `timestamp` column travels in the raw export and nowhere else.

use super::{Block, Conversation, Message, MessageTime, Role};
use serde_json::Value;

const EXPORT_SCHEMA: &str = "chat-stasher.github-copilot-cli.session.v1";

/// The store's own column spelling for the two speakers a turn records, in
/// the order the pair is written down.
const TURN_SIDE_COLUMNS: [&str; 2] = ["user_message", "assistant_response"];

fn turn_side_role(column: &str) -> Role {
    if column == "user_message" {
        Role::User
    } else {
        Role::Assistant
    }
}

pub(super) fn normalize_session(value: &Value, conversation: &mut Conversation) {
    if value.get("schema").and_then(Value::as_str) != Some(EXPORT_SCHEMA) {
        // Not this reader's export (an events.jsonl raw shard archived by an
        // older build is the everyday case). Nothing is interpreted; the bytes
        // are counted once and stay available through the raw route.
        conversation.unrendered_lines += 1;
        return;
    }
    let Some(session) = value.get("session").and_then(Value::as_object) else {
        conversation.unrendered_lines += 1;
        return;
    };
    if session.get("id").and_then(Value::as_str).is_none() {
        conversation.unrendered_lines += 1;
        return;
    }
    let Some(turns) = value.get("turns").and_then(Value::as_array) else {
        // A session row without its turns array is an export half this reader
        // cannot read, not a session with an empty conversation.
        conversation.unrendered_lines += 1;
        return;
    };
    for turn in turns {
        let Some(turn) = turn.as_object() else {
            conversation.unrendered_lines += 1;
            continue;
        };
        let mut rendered = false;
        for column in TURN_SIDE_COLUMNS {
            let Some(text) = turn
                .get(column)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            else {
                // A missing, null, empty or mistyped side observes no message;
                // it is never given a fabricated one, and whole-turn counting
                // below keeps the record visible.
                continue;
            };
            conversation.push_message(Message {
                role: turn_side_role(column),
                time: message_time_unknown(),
                blocks: vec![Block::Text(text.to_string())],
            });
            rendered = true;
        }
        if !rendered {
            // The store recorded a turn whose neither side this reader can
            // render — a count, so the session still says how much it holds
            // that was not read.
            conversation.unrendered_lines += 1;
        }
    }
}

fn message_time_unknown() -> MessageTime {
    MessageTime::Unknown {
        why: "GitHub Copilot CLI message time is not claimed: the session-store schema was read from build artifacts and no real store has been measured"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::{normalize, Provenance};
    use serde_json::json;

    /// One export line, exactly the shape the collector seals: a single JSON
    /// object holding the session row and the turn rows.
    fn export(session: serde_json::Value, turns: serde_json::Value) -> String {
        serde_json::to_string(&serde_json::json!({
            "schema": "chat-stasher.github-copilot-cli.session.v1",
            "session": session,
            "turns": turns,
        }))
        .unwrap()
    }

    #[test]
    fn copilot_export_renders_each_turn_side_in_order() {
        let body = export(
            json!({"id":"synthetic-copilot","cwd":"/synthetic/cwd","repository":"dimpurr/synthetic-repo"}),
            json!([
                {"session_id":"synthetic-copilot","turn_index":0,"user_message":"synthetic question","assistant_response":"synthetic answer","timestamp":"2026-10-08T10:00:01Z"},
                {"session_id":"synthetic-copilot","turn_index":1,"user_message":"synthetic follow-up","assistant_response":null,"timestamp":"2026-10-08T10:00:02Z"}
            ]),
        );
        let conversation = normalize("github-copilot-cli", &body);
        assert!(matches!(conversation.provenance, Provenance::Known { .. }));
        assert_eq!(conversation.messages.len(), 3);
        assert_eq!(conversation.messages[0].role, Role::User);
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::Text(text) if text == "synthetic question"
        ));
        assert_eq!(conversation.messages[1].role, Role::Assistant);
        assert!(matches!(
            &conversation.messages[1].blocks[0],
            Block::Text(text) if text == "synthetic answer"
        ));
        assert_eq!(conversation.messages[2].role, Role::User);
        assert!(matches!(
            &conversation.messages[2].blocks[0],
            Block::Text(text) if text == "synthetic follow-up"
        ));
        assert_eq!(conversation.unrendered_lines, 0);
        for message in &conversation.messages {
            assert!(matches!(message.time, MessageTime::Unknown { .. }));
        }
    }

    #[test]
    fn a_turn_without_any_readable_side_is_counted_not_guessed() {
        let body = export(
            json!({"id":"synthetic-copilot"}),
            json!([
                {"session_id":"synthetic-copilot","turn_index":0,"user_message":"","assistant_response":null},
                {"session_id":"synthetic-copilot","turn_index":1,"user_message":{"packed":"synthetic payload"},"assistant_response":"synthetic answer"}
            ]),
        );
        let conversation = normalize("github-copilot-cli", &body);
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(conversation.messages[0].role, Role::Assistant);
        // One wholly unreadable turn (no readable side), one turn that
        // rendered: the unreadable one is counted, the empty-user side is not
        // double-counted.
        assert_eq!(conversation.unrendered_lines, 1);
    }

    #[test]
    fn a_session_row_with_no_turns_is_measured_empty() {
        let body = export(json!({"id":"synthetic-copilot"}), json!([]));
        let conversation = normalize("github-copilot-cli", &body);
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 0);
        assert!(matches!(conversation.provenance, Provenance::Known { .. }));
    }

    #[test]
    fn an_export_missing_its_turns_array_is_counted_not_read_as_empty() {
        let body = export(json!({"id":"synthetic-copilot"}), json!(null));
        let conversation = normalize("github-copilot-cli", &body);
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 1);
    }

    #[test]
    fn a_body_that_is_not_this_export_shape_is_never_interpreted() {
        // The shape an older build archived raw: one events.jsonl line.
        let line = r#"{"event":"session.start","data":{"id":"synthetic-copilot"},"timestamp":"2026-10-08T10:00:00Z"}"#;
        let conversation = normalize("github-copilot-cli", line);
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 1);
        assert!(matches!(conversation.provenance, Provenance::Known { .. }));
    }

    #[test]
    fn an_export_without_a_session_id_is_counted() {
        let body = export(json!({"cwd":"/synthetic/cwd"}), json!([]));
        let conversation = normalize("github-copilot-cli", &body);
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 1);
    }
}
