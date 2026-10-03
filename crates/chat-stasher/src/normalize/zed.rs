//! Zed's SQLite export reader. The capture side stores each `threads` row as
//! `session.data`; this view reads the tagged User/Agent content while leaving
//! the complete exported row available through the raw route.

use super::{compact_json, Conversation, Message, Role};
use serde_json::Value;

pub(super) fn normalize_session(value: &Value, conversation: &mut Conversation) {
    let Some(messages) = value
        .pointer("/session/data/messages")
        .and_then(Value::as_array)
    else {
        conversation.unrendered_lines += 1;
        return;
    };
    for message in messages {
        let Some(object) = message.as_object() else {
            conversation.unrendered_lines += 1;
            continue;
        };
        if object.len() != 1 {
            conversation.unrendered_lines += 1;
            continue;
        }
        let (tag, payload) = object.iter().next().expect("length checked above");
        let role = match tag.as_str() {
            "User" => Role::User,
            "Agent" => Role::Assistant,
            _ => {
                conversation.unrendered_lines += 1;
                continue;
            }
        };
        let Some(content) = payload.get("content").and_then(Value::as_array) else {
            conversation.unrendered_lines += 1;
            continue;
        };
        // Current Zed exports store tool results as an object keyed by an
        // opaque call identifier. Match the result's explicit tool_use_id to
        // the ToolUse.id; do not expose either identifier in the reader.
        let mut tool_results = std::collections::HashMap::new();
        let mut unmatched_tool_results = 0usize;
        if let Some(results) = payload.get("tool_results").filter(|value| !value.is_null()) {
            if let Some(results) = results.as_object() {
                for result in results.values() {
                    let Some(call_id) = result.get("tool_use_id").and_then(Value::as_str) else {
                        unmatched_tool_results += 1;
                        continue;
                    };
                    let output = result.get("output").or_else(|| result.get("content"));
                    let Some(output) = output else {
                        unmatched_tool_results += 1;
                        continue;
                    };
                    tool_results.insert(call_id, compact_json(Some(output)).len());
                }
            } else if let Some(results) = results.as_array() {
                // Other/future schemas remain visible as unsupported records.
                unmatched_tool_results += results.len();
            } else {
                unmatched_tool_results += 1;
            }
        }
        let mut blocks = Vec::new();
        for item in content {
            let Some(object) = item.as_object() else {
                conversation.unrendered_lines += 1;
                continue;
            };
            if object.len() != 1 {
                conversation.unrendered_lines += 1;
                continue;
            }
            let (kind, body) = object.iter().next().expect("length checked above");
            match kind.as_str() {
                "Text" => match body.as_str().filter(|text| !text.is_empty()) {
                    Some(text) => blocks.push(super::Block::Text(text.to_string())),
                    None => conversation.unrendered_lines += 1,
                },
                "Thinking" => match body
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    Some(text) => blocks.push(super::Block::Thinking(text.to_string())),
                    None => conversation.unrendered_lines += 1,
                },
                "ToolUse" => {
                    blocks.push(super::Block::ToolCall {
                        name: body.get("name").and_then(Value::as_str).map(str::to_string),
                        input_summary: compact_json(
                            body.get("input").or_else(|| body.get("raw_input")),
                        ),
                        output_bytes: body
                            .get("id")
                            .and_then(Value::as_str)
                            .and_then(|id| tool_results.remove(id)),
                    });
                }
                "Image" => {
                    // Image payload bytes and source data remain only in the
                    // raw archive. The reader reports a typed attachment, not
                    // an invented filename or an attempted media decode.
                    let attachment = super::AttachmentRef {
                        name: None,
                        media_type: None,
                        bytes: None,
                    };
                    conversation.attachments.push(attachment.clone());
                    blocks.push(super::Block::AttachmentRef(attachment));
                }
                _ => conversation.unrendered_lines += 1,
            }
        }
        conversation.unrendered_lines += unmatched_tool_results + tool_results.len();
        if blocks.is_empty() {
            conversation.unrendered_lines += 1;
            continue;
        }
        conversation.push_message(Message {
            role,
            // Zed's row stores session-level timestamps, not per-message
            // timestamps; the reader keeps the message time unknown.
            time: super::message_time(None),
            blocks,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{normalize, Block, Provenance, Role};

    #[test]
    fn zed_sqlite_export_renders_known_turns_and_counts_unknown_records() {
        let body = concat!(
            r#"{"schema":"chat-stasher.sqlite.session.v1","table":"threads","session":{"id":"synthetic-zed","data":{"messages":["#,
            r#"{"User":{"id":"u1","content":[{"Text":"synthetic question"}]}} ,"#,
            r#"{"Agent":{"content":[{"Thinking":{"text":"synthetic reasoning"}},{"Text":"synthetic answer"},{"ToolUse":{"name":"synthetic-tool","input":{"query":"synthetic argument"}}},{"FutureThing":{"payload":"synthetic unknown"}}],"tool_results":[{"future_result":"synthetic result"}]}} ,"#,
            r#"{"Compaction":{"Summary":"synthetic summary"}}]}}}"#
        );
        let result = normalize("zed", body);
        assert!(matches!(result.provenance, Provenance::Known { .. }));
        assert_eq!(result.message_total, 2);
        assert_eq!(result.messages[0].role, Role::User);
        assert_eq!(
            result.messages[0].blocks,
            vec![Block::Text("synthetic question".to_string())]
        );
        assert_eq!(result.messages[1].role, Role::Assistant);
        assert!(matches!(result.messages[1].blocks[0], Block::Thinking(_)));
        assert!(matches!(result.messages[1].blocks[1], Block::Text(_)));
        assert!(matches!(
            result.messages[1].blocks[2],
            Block::ToolCall { .. }
        ));
        assert_eq!(result.unrendered_lines, 3);
    }

    #[test]
    fn zed_tool_result_map_is_matched_to_its_call_without_exposing_ids() {
        let body = concat!(
            r#"{"schema":"chat-stasher.sqlite.session.v1","table":"threads","session":{"id":"synthetic-zed","data":{"messages":["#,
            r#"{"Agent":{"content":[{"ToolUse":{"id":"opaque-call-id","name":"synthetic-tool","input":{"query":"synthetic argument"}}}],"tool_results":{"opaque-map-key":{"tool_use_id":"opaque-call-id","tool_name":"synthetic-tool","content":"synthetic output"}}}}]}}}"#
        );
        let result = normalize("zed", body);
        assert_eq!(result.unrendered_lines, 0);
        let Block::ToolCall {
            name, output_bytes, ..
        } = &result.messages[0].blocks[0]
        else {
            panic!("expected a rendered tool call");
        };
        assert_eq!(name.as_deref(), Some("synthetic-tool"));
        assert!(output_bytes.is_some());
        assert!(!format!("{:?}", result.messages).contains("opaque-call-id"));
        assert!(!format!("{:?}", result.messages).contains("opaque-map-key"));
    }
}
