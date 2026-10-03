//! Hermes Agent message view. The raw shard retains every source column;
//! session_model_usage remains session metadata and is never projected onto
//! individual messages.

use super::{message_time, Block, Conversation, Message, Role};
use serde_json::Value;

pub(super) fn normalize_session(value: &Value, conversation: &mut Conversation) {
    let Some(messages) = value.get("messages").and_then(Value::as_array) else {
        conversation.unrendered_lines += 1;
        return;
    };
    for row in messages {
        let Some(role) = row
            .get("role")
            .and_then(Value::as_str)
            .and_then(Role::from_str)
        else {
            conversation.unrendered_lines += 1;
            continue;
        };
        let content = row
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let Some(content) = content else {
            conversation.unrendered_lines += 1;
            continue;
        };
        conversation.push_message(Message {
            role,
            time: message_time(row.get("timestamp")),
            blocks: vec![Block::Text(content.to_string())],
        });
    }
}
