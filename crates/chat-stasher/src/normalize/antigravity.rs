//! Google Antigravity's measured transcript JSONL reader.
//!
//! The source emits typed events. Only the user/model message event types are
//! rendered as conversation; the other known event families stay counted as
//! unrendered until their semantics are verified.

use super::{blocks_from_content, compact_json, message_time, Block, Conversation, Message, Role};
use serde_json::Value;

pub(super) fn normalize_line(value: &Value, conversation: &mut Conversation) {
    let kind = value.get("type").and_then(Value::as_str);
    let role = match kind {
        Some("USER_INPUT") => Role::User,
        Some("PLANNER_RESPONSE") => Role::Assistant,
        _ => {
            conversation.unrendered_lines += 1;
            return;
        }
    };
    let time = message_time(value.get("created_at"));
    let mut blocks = Vec::new();
    if let Some(content) = value.get("content") {
        blocks.extend(blocks_from_content(content, conversation));
    }
    if role == Role::Assistant {
        if let Some(thinking) = value
            .get("thinking")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            blocks.push(Block::Thinking(thinking.to_string()));
        }
    }
    if let Some(calls) = value.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            blocks.push(Block::ToolCall {
                name: call.get("name").and_then(Value::as_str).map(str::to_string),
                input_summary: compact_json(call.get("arguments").or_else(|| call.get("input"))),
                output_bytes: None,
            });
        }
    }
    if blocks.is_empty() {
        conversation.unrendered_lines += 1;
    } else {
        conversation.push_message(Message { role, time, blocks });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{normalize, Block, Role};

    #[test]
    fn measured_user_and_planner_records_render_and_other_events_remain_counted() {
        let body = concat!(
            r#"{"created_at":"2026-10-01T10:00:00Z","type":"USER_INPUT","content":"question"}"#,
            "\n",
            r#"{"created_at":"2026-10-01T10:00:01Z","type":"PLANNER_RESPONSE","content":"answer","thinking":"reason","tool_calls":[{"name":"run"}]}"#,
            "\n",
            r#"{"created_at":"2026-10-01T10:00:02Z","type":"CHECKPOINT"}"#,
        );
        let result = normalize("google-antigravity", body);
        assert_eq!(result.messages.len(), 2);
        assert!(matches!(result.messages[0].role, Role::User));
        assert!(matches!(result.messages[1].role, Role::Assistant));
        assert!(result.messages[1]
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Thinking(_))));
        assert!(result.messages[1]
            .blocks
            .iter()
            .any(|block| matches!(block, Block::ToolCall { .. })));
        assert_eq!(result.unrendered_lines, 1);
        assert_eq!(result.unrecognized_lines, 0);
    }
}
