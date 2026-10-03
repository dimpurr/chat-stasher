//! Grok CLI — reader for the `session_docs` row exported from its search DB.
//!
//! The export carries `title`, `updated_at`, and a plain-text `content` field.
//! The content is useful for reading and full-text search, but the row does
//! not preserve message boundaries or speakers. Keep it as one text block with
//! an explicitly unknown speaker; do not infer turns from lines or punctuation.

use super::{Block, Conversation, Message, MessageTime, Role};
use serde_json::Value;

const EXPORT_SCHEMA: &str = "chat-stasher.sqlite.session.v1";
const EXPORT_TABLE: &str = "session_docs";

pub(super) fn is_session_docs_record(value: &Value) -> bool {
    value.get("schema").and_then(Value::as_str) == Some(EXPORT_SCHEMA)
        && value.get("table").and_then(Value::as_str) == Some(EXPORT_TABLE)
}

pub(super) fn normalize_session(value: &Value, conversation: &mut Conversation) {
    if !is_session_docs_record(value) {
        conversation.unrendered_lines += 1;
        return;
    }
    let Some(content) = value
        .get("session")
        .and_then(|session| session.get("content"))
        .and_then(Value::as_str)
    else {
        conversation.unrendered_lines += 1;
        return;
    };
    if content.is_empty() {
        return;
    }
    conversation.push_message(Message {
        role: Role::Unknown,
        time: MessageTime::Unknown {
            why: "Grok CLI stores a session update time, not per-message times".to_string(),
        },
        blocks: vec![Block::Text(content.to_string())],
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::{normalize, Provenance};

    #[test]
    fn exported_search_document_keeps_text_without_inventing_speakers_or_times() {
        let line = r#"{"schema":"chat-stasher.sqlite.session.v1","table":"session_docs","session":{"session_id":"opaque","updated_at":1789384914,"title":"synthetic title","content":"synthetic question\nsynthetic answer"}}"#;
        let conversation = normalize("grok", line);
        assert!(matches!(conversation.provenance, Provenance::Known { .. }));
        assert_eq!(conversation.messages.len(), 1);
        assert!(matches!(conversation.messages[0].role, Role::Unknown));
        assert!(matches!(
            conversation.messages[0].time,
            MessageTime::Unknown { .. }
        ));
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::Text(text) if text == "synthetic question\nsynthetic answer"
        ));
    }

    #[test]
    fn exported_row_with_empty_content_is_measured_empty() {
        let line = r#"{"schema":"chat-stasher.sqlite.session.v1","table":"session_docs","session":{"session_id":"opaque","updated_at":1789384914,"title":"synthetic title","content":""}}"#;
        let conversation = normalize("grok", line);
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
        assert!(matches!(conversation.provenance, Provenance::Known { .. }));
    }

    #[test]
    fn cli_reader_does_not_claim_a_web_bundle_as_a_sqlite_row() {
        let line = r#"{"schema":"chat-stasher.sqlite.session.v1","table":"other","session":{"content":"synthetic body"}}"#;
        let conversation = normalize("grok", line);
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 1);
    }

    #[test]
    fn grok_web_bundle_keeps_the_generic_web_reader() {
        let line = r#"{"raw":{"text":"{\"messages\":[{\"role\":\"user\",\"content\":\"synthetic question\"}]}"}}"#;
        let conversation = normalize("grok", line);
        assert_eq!(conversation.messages.len(), 1);
        assert!(matches!(conversation.messages[0].role, Role::User));
    }
}
