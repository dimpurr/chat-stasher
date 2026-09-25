//! codex — reader for one archived Codex CLI session line.
//!
//! The archived body is `{"timestamp":…, "type":…, "payload":{…}}` and it is
//! `payload.type` that names the record. `response_item` holds the transcript
//! the model saw — `message`, `reasoning`, `function_call`,
//! `function_call_output` — and is what this renders. `event_msg` is the same
//! turn recorded a second time at the application level (`user_message`,
//! `agent_message`, `agent_reasoning`, `token_count`), so rendering it too
//! would show every turn twice; it is counted as not rendered, along with
//! `session_meta` and `turn_context`, which are session metadata rather than
//! conversation.
//!
//! The minimal `{"payload":{"message":…}}` shape is kept because
//! `activity::analyze_session`'s fixture uses it (`activity.rs:1717`).

use super::{compact_json, message_time, Conversation, Message, MessageTime, Role};
use serde_json::Value;

pub(super) fn normalize_line(value: &Value, conversation: &mut Conversation) {
    let time = message_time(value.get("timestamp"));
    if let Some(message) = value.pointer("/payload/message") {
        push_or_count(
            super::message_from_value(message, time, conversation),
            conversation,
        );
        return;
    }
    let kind = value.get("type").and_then(Value::as_str);
    if kind == Some("response_item") {
        push_or_count(item(value.get("payload"), time, conversation), conversation);
        return;
    }
    if let Some(item) = value.get("item") {
        push_or_count(
            super::message_from_value(item, time, conversation),
            conversation,
        );
        return;
    }
    conversation.unrendered_lines += 1;
}

fn push_or_count(message: Option<Message>, conversation: &mut Conversation) {
    match message {
        Some(message) => conversation.push_message(message),
        None => conversation.unrendered_lines += 1,
    }
}

/// Turn one codex `response_item` payload into a message, or `None` when the
/// record is not part of the conversation (metadata, a token count, or a
/// reasoning record whose text was archived encrypted).
fn item(
    payload: Option<&Value>,
    time: MessageTime,
    conversation: &mut Conversation,
) -> Option<Message> {
    let payload = payload?;
    match payload.get("type").and_then(Value::as_str)? {
        "message" => {
            let role = payload
                .get("role")
                .and_then(Value::as_str)
                .and_then(Role::from_str)?;
            let blocks =
                super::blocks_from_content(payload.get("content").unwrap_or(payload), conversation);
            (!blocks.is_empty()).then_some(Message { role, time, blocks })
        }
        "reasoning" => {
            // The readable part of a reasoning record is its `summary`; the
            // `content` it also carries is archived encrypted, and the reader
            // does not pretend to have read it.
            let text = payload
                .get("summary")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|text| !text.is_empty())?;
            Some(Message {
                role: Role::Assistant,
                time,
                blocks: vec![super::Block::Thinking(text)],
            })
        }
        "function_call" | "custom_tool_call" | "local_shell_call" | "web_search_call" => {
            Some(Message {
                role: Role::Tool,
                time,
                blocks: vec![super::Block::ToolCall {
                    name: payload
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    input_summary: compact_json(
                        payload.get("arguments").or_else(|| payload.get("action")),
                    ),
                    // The result arrives as its own `function_call_output`.
                    output_bytes: None,
                }],
            })
        }
        "function_call_output" | "custom_tool_call_output" => Some(Message {
            role: Role::Tool,
            time,
            blocks: vec![super::Block::ToolCall {
                name: payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                input_summary: "tool result".to_string(),
                output_bytes: payload
                    .get("output")
                    .map(|value| compact_json(Some(value)).len()),
            }],
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{normalize, Block, Role};

    #[test]
    fn codex_fixture_produces_roles_and_code() {
        let body = r#"{"timestamp":1736944496,"payload":{"message":{"role":"assistant","content":[{"type":"output_text","text":"done"},{"type":"code","language":"rust","code":"fn main() {}"}]}}}"#;
        let result = normalize("codex", body);
        assert_eq!(result.messages.len(), 1);
        assert!(result.messages[0]
            .blocks
            .iter()
            .any(|block| matches!(block, Block::CodeBlock { .. })));
    }

    #[test]
    fn codex_item_types_render_call_and_output_separately() {
        let body = concat!(
            r#"{"timestamp":1736944499,"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"ls\"}","call_id":"c1"}}"#,
            "\n",
            r#"{"timestamp":1736944500,"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"3 files"}}"#,
        );
        let result = normalize("codex", body);
        assert_eq!(result.messages.len(), 2);
        assert!(matches!(result.messages[0].role, Role::Tool));
        // The call names the call and records no output; the output record is
        // the only place that can measure one.
        assert!(matches!(
            result.messages[0].blocks[0],
            Block::ToolCall {
                output_bytes: None,
                ..
            }
        ));
        assert!(matches!(
            result.messages[1].blocks[0],
            Block::ToolCall {
                output_bytes: Some(7),
                ..
            }
        ));
    }

    /// The archived codex body is `payload.type`-tagged, and `event_msg`
    /// records the same turn a second time at the application level. Rendering
    /// both would show every turn twice, so the transcript comes from
    /// `response_item` and the mirror is counted as not rendered.
    #[test]
    fn codex_renders_the_transcript_once_not_the_mirrored_event() {
        let body = concat!(
            r#"{"timestamp":1736944496,"type":"session_meta","payload":{"id":"s","cli_version":"1"}}"#,
            "\n",
            r#"{"timestamp":1736944497,"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"q"}]}}"#,
            "\n",
            r#"{"timestamp":1736944497,"type":"event_msg","payload":{"type":"user_message","message":"q"}}"#,
            "\n",
            r#"{"timestamp":1736944498,"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"weighing"}],"content":[{"encrypted_content":"opaque"}]}}"#,
            "\n",
            r#"{"timestamp":1736944499,"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"ls\"}","call_id":"c1"}}"#,
            "\n",
            r#"{"timestamp":1736944500,"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"3 files"}}"#,
            "\n",
            r#"{"timestamp":1736944501,"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}"#,
            "\n",
            r#"{"timestamp":1736944501,"type":"event_msg","payload":{"type":"agent_message","message":"done"}}"#,
            "\n",
            r#"{"timestamp":1736944502,"type":"event_msg","payload":{"type":"token_count","info":{}}}"#,
            "\n",
        );
        let result = normalize("codex", body);
        // user, reasoning, call, call output, assistant — five, not eight.
        assert_eq!(result.messages.len(), 5);
        assert!(matches!(result.messages[0].role, Role::User));
        assert!(matches!(result.messages[1].blocks[0], Block::Thinking(_)));
        assert!(matches!(result.messages[2].role, Role::Tool));
        assert!(matches!(result.messages[4].role, Role::Assistant));
        // session_meta, event_msg (mirror) and token_count are not messages.
        assert_eq!(result.unrendered_lines, 4);
    }
}
