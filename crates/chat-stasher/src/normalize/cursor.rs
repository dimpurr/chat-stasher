//! cursor — reader for one archived Cursor composer export.
//!
//! One Cursor session is one line, exported by `collect` from one of the two
//! stores the scanner walks. Both ship the composer as the `session` field:
//!
//! * global store (`cursorDiskKV`, `sqlite_probe.rs:976-981`):
//!   `{"schema":"chat-stasher.sqlite.session.v1","table":"cursorDiskKV",
//!     "session":{"key":"composerData:<id>","value":{composer}}}`
//! * legacy per-workspace store (`ItemTable`, `sqlite_probe.rs:1082-1088`):
//!   `{"schema":"chat-stasher.cursor.legacy.session.v1","session":{composer}}`
//!
//! The conversation lives in one of two places inside the composer, and the
//! difference is a version split documented by the cursaves storage notes
//! (github.com/Callum-Ward/cursaves `docs/how-cursor-stores-chats.md`,
//! fetched 2026-09-25): older composers carry each bubble inline in
//! `conversationMap` (keyed by bubble id, ordered by
//! `fullConversationHeadersOnly`, whose entries are `{bubbleId, type}` with
//! `1` = user and `2` = assistant); the legacy workspace shape carries the
//! bubbles directly in `conversation`; and current Cursor stores each
//! bubble's body in a separate `bubbleId:{composer}:{bubble}` key that this
//! archive does not store — the capture exports the composer row only. A
//! header whose bubble is elsewhere is counted, not skipped and not called
//! empty: the body exists and is not here.
//!
//! Bubble fields measured on the local stores (2026-09-25, key names and
//! role-int vocabulary only): legacy `conversation[]` entries are
//! `{type: 1|2, bubbleId, text, richText?, context? …}`; bubbles carry no
//! per-message timestamp of their own in that shape.

use super::{message_time, Conversation, Message, Role};
use serde_json::Value;

const TYPE_USER: i64 = 1;
const TYPE_ASSISTANT: i64 = 2;

pub(super) fn normalize_session(value: &Value, conversation: &mut Conversation) {
    let Some(session) = value.get("session") else {
        conversation.unrendered_lines += 1;
        return;
    };
    // The two exports are told apart structurally, not by their schema label:
    // the global row wraps the composer in `value`, the legacy row writes the
    // composer itself. The label is evidence we can print, not a shape we
    // trust — an export written with a different label but the same structure
    // still means the same records.
    if session.get("value").is_some() {
        global(session.get("value").unwrap_or(&Value::Null), conversation);
    } else if session.get("conversation").is_some() {
        legacy_composer(session, conversation);
    } else {
        conversation.unrendered_lines += 1;
    }
}

/// A composer exported from the global `cursorDiskKV` store.
fn global(composer: &Value, conversation: &mut Conversation) {
    if !composer.is_object() {
        // The value column was not decodable JSON: `sqlite_value_to_json`
        // kept the raw string. It is content this reader cannot read, not
        // content that does not exist.
        conversation.unrendered_lines += 1;
        return;
    }
    let headers = match composer.get("fullConversationHeadersOnly") {
        Some(Value::Array(headers)) => headers,
        // A composer without the ordering index cannot place any bubble it
        // might still carry; if it also has no map there was nothing to
        // place. Either way the composer itself is the record that failed.
        _ => {
            conversation.unrendered_lines += 1;
            return;
        }
    };
    let map = composer
        .get("conversationMap")
        .and_then(Value::as_object)
        .cloned()
        // reason: an absent `conversationMap` is not an unreadable map — the
        // composer simply carries no inline bubbles, which reads exactly as
        // an empty one. A header ordering bubbles the archive does not hold
        // is counted below, so this default cannot hide body-not-here.
        .unwrap_or_default();
    let mut seen: Vec<String> = Vec::new();
    for header in headers {
        let Some(header_object) = header.as_object() else {
            conversation.unrendered_lines += 1;
            continue;
        };
        let Some(bubble_id) = header_object.get("bubbleId").and_then(Value::as_str) else {
            conversation.unrendered_lines += 1;
            continue;
        };
        seen.push(bubble_id.to_string());
        let Some(bubble) = map.get(bubble_id) else {
            // Present in the ordered conversation, absent from this archive:
            // the body lives in a `bubbleId:` key the capture does not
            // store. Counted and reachable through the raw shards.
            conversation.unrendered_lines += 1;
            continue;
        };
        let Some(role) =
            bubble_role(header_object.get("type")).or_else(|| bubble_role(bubble.get("type")))
        else {
            conversation.unrendered_lines += 1;
            continue;
        };
        push_bubble(bubble, role, conversation);
    }
    // Bubbles the ordering index does not name cannot be placed in the
    // conversation, readable or not: rendering them would invent an order.
    for bubble_id in map.keys() {
        if !seen.contains(bubble_id) {
            conversation.unrendered_lines += 1;
        }
    }
}

/// A composer exported from a legacy workspace `ItemTable` value.
fn legacy_composer(session: &Value, conversation: &mut Conversation) {
    let Some(bubbles) = session.pointer("/conversation").and_then(Value::as_array) else {
        conversation.unrendered_lines += 1;
        return;
    };
    for bubble in bubbles {
        let Some(role) = bubble_role(bubble.get("type")) else {
            conversation.unrendered_lines += 1;
            continue;
        };
        push_bubble(bubble, role, conversation);
    }
}

/// Render one bubble with an already-decided role, or count it when it holds
/// no text this reader can render.
fn push_bubble(bubble: &Value, role: Role, conversation: &mut Conversation) {
    let Some(text) = bubble.get("text").and_then(Value::as_str) else {
        conversation.unrendered_lines += 1;
        return;
    };
    if text.is_empty() {
        conversation.unrendered_lines += 1;
        return;
    }
    conversation.push_message(Message {
        role,
        // A bubble may carry its own `createdAt` (epoch millis); the legacy
        // inline shape does not, and `message_time` says so instead of
        // stamping the composer's creation time onto every message.
        time: message_time(bubble.get("createdAt")),
        blocks: vec![super::Block::Text(text.to_string())],
    });
}

fn bubble_role(value: Option<&Value>) -> Option<Role> {
    match value.and_then(Value::as_i64) {
        Some(TYPE_USER) => Some(Role::User),
        Some(TYPE_ASSISTANT) => Some(Role::Assistant),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{normalize, Role};

    /// The legacy workspace export: bubbles inline in `conversation`, role
    /// is the Cursor marker int (`1` = user, `2` = assistant), text is the
    /// message body. Measured on the local stores; synthetic here.
    #[test]
    fn a_legacy_cursor_composer_renders_its_conversation() {
        let body = concat!(
            r#"{"schema":"chat-stasher.cursor.legacy.session.v1","session":{"#,
            r#""composerId":"legacy-ok","createdAt":1751779149032,"#,
            r#""conversation":[{"type":1,"bubbleId":"b1","text":"question","relevantFiles":[]},"#,
            r#"{"type":2,"bubbleId":"b2","text":"answer","richText":"<done>","relevantFiles":[]},"#,
            r#"{"type":2,"bubbleId":"b3","relevantFiles":[]}]}}"#,
        );
        let result = normalize("cursor", body);
        assert_eq!(result.messages.len(), 2);
        assert!(matches!(result.messages[0].role, Role::User));
        assert!(matches!(result.messages[1].role, Role::Assistant));
        // Legacy bubbles carry no timestamp of their own; the reader says so
        // rather than stamping the composer's creation time on them.
        assert!(matches!(
            result.messages[0].time,
            super::super::MessageTime::Unknown { .. }
        ));
        assert_eq!(
            result.unrendered_lines, 1,
            "the text-less bubble is counted"
        );
    }

    /// The global-store export in the older composer shape: the ordering
    /// index names the bubbles, `conversationMap` carries their bodies.
    #[test]
    fn a_global_composer_renders_bubbles_from_conversation_map_in_order() {
        let body = concat!(
            r#"{"schema":"chat-stasher.sqlite.session.v1","table":"cursorDiskKV","#,
            r#""session":{"key":"composerData:aaaaaaaa-1111","value":{"#,
            r#""composerId":"aaaaaaaa-1111","createdAt":1751779149032,"#,
            r#""fullConversationHeadersOnly":[{"bubbleId":"b1","type":1},{"bubbleId":"b2","type":2}],"#,
            r#""conversationMap":{"b1":{"text":"question","type":1},"#,
            r#""b2":{"text":"answer","type":2},"#,
            r#""b9":{"text":"unplaced"}}}}}"#,
        );
        let result = normalize("cursor", body);
        assert_eq!(result.messages.len(), 2, "in header order");
        assert!(matches!(result.messages[0].role, Role::User));
        assert!(matches!(result.messages[1].role, Role::Assistant));
        assert!(matches!(
            result.messages[1].blocks[0],
            super::super::Block::Text(ref text) if text == "answer"
        ));
        assert_eq!(
            result.unrendered_lines, 1,
            "the map bubble no header names cannot be placed"
        );
    }

    /// Current Cursor keeps each bubble's body in a `bubbleId:` key of the
    /// global store, and the capture archives the composer row only. The
    /// headers prove the conversation exists; this reader must say the
    /// bodies are not here, not that the conversation is empty.
    #[test]
    fn a_headers_only_composer_counts_its_missing_bubbles() {
        let body = concat!(
            r#"{"schema":"chat-stasher.sqlite.session.v1","table":"cursorDiskKV","#,
            r#""session":{"key":"composerData:bbbbbbbb-2222","value":{"#,
            r#""composerId":"bbbbbbbb-2222","createdAt":1751779149032,"isArchived":false,"#,
            r#""fullConversationHeadersOnly":[{"bubbleId":"b1","type":1},{"bubbleId":"b2","type":2},{"bubbleId":"b3","type":2}],"#,
            r#""conversationMap":{}}}}"#,
        );
        let result = normalize("cursor", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(result.unrendered_lines, 3);
        assert_eq!(result.unrecognized_lines, 0);
    }

    /// The composer value column that did not decode to JSON stays a string;
    /// unreadable is not empty.
    #[test]
    fn an_undecodable_global_composer_value_is_counted() {
        let body = r#"{"schema":"chat-stasher.sqlite.session.v1","table":"cursorDiskKV","session":{"key":"composerData:cccccccc-3333","value":"not json"}}"#;
        let result = normalize("cursor", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(result.unrendered_lines, 1);
    }

    /// A composer whose headers and map agree that there is nothing at all:
    /// no message, and no count either — this is the shape the capture's
    /// qualification would exclude, received honestly if it slips through.
    #[test]
    fn an_empty_global_composer_renders_nothing_and_counts_nothing() {
        let body = concat!(
            r#"{"schema":"chat-stasher.sqlite.session.v1","table":"cursorDiskKV","#,
            r#""session":{"key":"composerData:dddddddd-4444","value":{"#,
            r#""composerId":"dddddddd-4444","createdAt":1753000000000,"#,
            r#""fullConversationHeadersOnly":[],"conversationMap":{}}}}"#,
        );
        let result = normalize("cursor", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(result.unrendered_lines, 0);
    }
}
