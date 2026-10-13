//! Codebuff / Freebuff — reader for the `chat-messages.json` transcript
//! body (ADR-054).
//!
//! The collector seals the whole transcript as raw bytes, one full
//! generation per changed pass (D2), so an archived body holds one or
//! more complete JSON arrays run together. The *latest* generation is
//! the reading's source — the most recent observation of the session,
//! and the source's own current state; earlier ones are complete
//! histories of the same conversation and are neither re-rendered (that
//! would reprint every superseded turn as a duplicate) nor counted as a
//! defect of the body (the count would describe our framing, not the
//! source). They stay in the raw route, where every generation remains
//! readable and extractable.
//!
//! Turn rules (the ADR's read-side projection contract):
//!
//! * `variant: "user"` renders as the user; `"ai"` and `"agent"` render as
//!   the assistant; `"error"` is **not a turn** — counted unrendered and
//!   never presented as a speaker (ADR-049: count, never guess).
//! * System-injected messages are written as `"ai"` by the source itself
//!   (F19), so they read as assistant text: a known, declared limitation
//!   of the source, not something this reader may invent a role for.
//! * `content` is plain text. `blocks`, when present and non-empty, is the
//!   richer rendering of the same message: `text` blocks render (as
//!   thinking when the source marked `textType: "reasoning"`), `tool`
//!   blocks render as tool calls with their output measured or *not
//!   recorded*, and `agent` blocks render the sub-agent's text. The same
//!   message's `content` is its plain-text mirror, so a message that
//!   rendered at least one block does not also render content — duplicate
//!   prose is not new information. Unknown block types are counted
//!   unrendered, never rendered by guesswork.
//! * Per-message `timestamp` is a display clock face with no date (F8) and
//!   is never parsed or rebuilt; a message has no known time (D4). The
//!   session's start time lives in the captured manifest field with its
//!   source named, and never folds into a source that means "read from
//!   the conversation content".
//! * `metadata.runState` — the bulk of the bytes (F12) — is never walked,
//!   and attachment references stay in the raw transcript: this reader
//!   does not render attachment arrays whose field shape is unverified,
//!   and the attachment **bytes are not archived** either way (OQ5).

use super::{Block, Conversation, Message, MessageTime, Role};
use serde_json::Value;

/// Why one codebuff message carries no time (rendered with the message, so
/// the reader page says it once per message).
const NO_TIME_WHY: &str = "codebuff message timestamps are display-only clock faces with no date, so the source records no per-message time (ADR-054 D4)";

/// Walk one archived codebuff body: a stream of complete transcript
/// generations, of which the latest is the reading.
pub(super) fn normalize_body(body: &str, conversation: &mut Conversation) {
    let mut latest: Option<Value> = None;
    for item in serde_json::Deserializer::from_str(body).into_iter::<Value>() {
        match item {
            Ok(value) => {
                if value.is_array() {
                    // An earlier generation is observed, recognized, and
                    // deliberately not rendered — the same reasoning the
                    // grok summary suppressions use: it is not an
                    // unrendered or unrecognized value (both words are
                    // about content this reader could not use), and the
                    // raw route keeps it readable.
                    latest = Some(value);
                } else {
                    // Everything else in the stream is a body this reader
                    // classified but cannot read: counted, not guessed from.
                    conversation.unrendered_lines += 1;
                }
            }
            Err(_) => {
                // A stream that stops mid-value is measured once; the rest
                // of the body cannot be audited behind a broken boundary.
                conversation.unrecognized_lines += 1;
                break;
            }
        }
    }
    let Some(messages) = latest.as_ref().and_then(Value::as_array) else {
        return;
    };
    for message in messages {
        render_message(message, conversation);
    }
}

fn render_message(message: &Value, conversation: &mut Conversation) {
    let role = match message.get("variant").and_then(Value::as_str) {
        Some("user") => Role::User,
        // F19: the source also writes its system-injected messages as
        // `variant: "ai"`, so they read as assistant text here — a
        // limitation of the source this reader must not paper over with
        // an invented role.
        Some("ai") | Some("agent") => Role::Assistant,
        // `variant: "error"` is not a turn: counted, never presented as a
        // speaker.
        _ => {
            conversation.unrendered_lines += 1;
            return;
        }
    };
    let blocks = if conversation_blocks_rendered(message) {
        blocks_from_message(message, conversation)
    } else {
        content_only_blocks(message, conversation)
    };
    if blocks.is_empty() {
        conversation.unrendered_lines += 1;
        return;
    }
    conversation.push_message(Message {
        role,
        time: MessageTime::Unknown {
            why: NO_TIME_WHY.to_string(),
        },
        blocks,
    });
}

/// Whether the message carries a non-empty `blocks` array to walk.
fn conversation_blocks_rendered(message: &Value) -> bool {
    message
        .get("blocks")
        .and_then(Value::as_array)
        .is_some_and(|blocks| !blocks.is_empty())
}

fn content_only_blocks(message: &Value, _conversation: &mut Conversation) -> Vec<Block> {
    match message.get("content").and_then(Value::as_str) {
        Some(content) if !content.is_empty() => vec![Block::Text(content.to_string())],
        _ => Vec::new(),
    }
}

fn blocks_from_message(message: &Value, conversation: &mut Conversation) -> Vec<Block> {
    let blocks = message
        .get("blocks")
        .and_then(Value::as_array)
        .expect("checked by the caller");
    let mut rendered = Vec::new();
    for block in blocks {
        match render_block(block) {
            Some(block) => rendered.push(block),
            None => conversation.unrendered_lines += 1,
        }
    }
    if rendered.is_empty() {
        // The blocks carried nothing this reader can show but the message
        // still has its plain-text mirror: that is the readable text, the
        // same reasoning grok's reader applies when the native records
        // render nothing and the summary stays the reading.
        return content_only_blocks(message, conversation);
    }
    rendered
}

fn render_block(block: &Value) -> Option<Block> {
    match block.get("type").and_then(Value::as_str)? {
        "text" => {
            let content = block.get("content").and_then(Value::as_str)?;
            if content.is_empty() {
                return None;
            }
            Some(
                if block.get("textType").and_then(Value::as_str) == Some("reasoning") {
                    Block::Thinking(content.to_string())
                } else {
                    Block::Text(content.to_string())
                },
            )
        }
        "tool" => {
            let name = block
                .get("toolName")
                .and_then(Value::as_str)
                .map(str::to_string);
            let input_summary = summarize_json_value(block.get("input"));
            // `output` is absent or non-string → Not recorded, which the
            // renderer prints as its own state, never as zero bytes.
            let output_bytes = block
                .get("output")
                .and_then(Value::as_str)
                .map(|output| output.len());
            Some(Block::ToolCall {
                name,
                input_summary,
                output_bytes,
            })
        }
        "agent" => {
            // A sub-agent's own message embedded in this one: its text is a
            // source-written string; the nested `blocks` it may carry stay
            // in the raw transcript, unwalked and uncounted here.
            let content = block.get("content").and_then(Value::as_str)?;
            if content.is_empty() {
                return None;
            }
            Some(Block::Text(content.to_string()))
        }
        _ => None,
    }
}

/// A bounded, content-free-shape summary of a tool input for the tool-call
/// line: the serialized JSON, truncated on a UTF-8 boundary so a pasted
/// file inside an input cannot spend the page's budget.
fn summarize_json_value(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match serde_json::to_string(value) {
        Ok(text) if !text.is_empty() => truncate_utf8(&text, 80),
        _ => String::new(),
    }
}

fn truncate_utf8(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::super::{normalize, Provenance};

    fn transcript(body: &str) -> super::super::Conversation {
        normalize("codebuff", body)
    }

    fn message(variant: &str, content: &str) -> String {
        format!(
            r#"{{"id":"m","variant":"{variant}","content":"{content}","timestamp":"04:16 AM"}}"#
        )
    }

    #[test]
    fn turns_render_by_variant_and_errors_are_counted_not_spoken() {
        let body = format!(
            "[{},{},{}]",
            message("user", "hello"),
            message("ai", "hi there"),
            message("error", "boom")
        );
        let conversation = transcript(&body);
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(
            conversation.messages[0].blocks,
            vec![super::super::Block::Text("hello".to_string())]
        );
        assert_eq!(conversation.messages[1].role, super::super::Role::Assistant);
        // The error variant is unrendered, not a third turn.
        assert_eq!(conversation.unrendered_lines, 1);
        assert!(matches!(conversation.provenance, Provenance::Known { .. }));
    }

    #[test]
    fn per_message_times_stay_unknown_with_no_clock_guess() {
        let body = format!("[{}]", message("user", "hello"));
        let conversation = transcript(&body);
        match &conversation.messages[0].time {
            super::super::MessageTime::Unknown { why } => {
                assert!(
                    why.contains("no date"),
                    "the why names the source's defect: {why}"
                );
            }
            other => panic!("a display clock face must not become a known time: {other:?}"),
        }
    }

    #[test]
    fn the_latest_generation_is_the_reading_and_earlier_ones_are_not_duplicates() {
        let old = format!("[{}]", message("user", "old turn"));
        let new = format!(
            "[{},{}]",
            message("user", "old turn"),
            message("ai", "new turn")
        );
        let conversation = transcript(&format!("{old}{new}"));
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
        // The second generation's content is what rendered; the first is a
        // complete superseded history, still byte-present in the raw route.
        assert_eq!(
            conversation.messages[1].blocks,
            vec![super::super::Block::Text("new turn".to_string())]
        );
    }

    #[test]
    fn an_empty_array_is_a_successful_read_of_an_empty_chat() {
        let conversation = transcript("[]");
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    #[test]
    fn text_blocks_render_and_content_is_not_rendered_twice() {
        let body = r#"[{"variant":"ai","content":"mirror text","blocks":[
            {"type":"text","content":"block text"}
        ]}]"#;
        let conversation = transcript(body);
        assert_eq!(conversation.messages.len(), 1);
        assert_eq!(
            conversation.messages[0].blocks,
            vec![super::super::Block::Text("block text".to_string())]
        );
    }

    #[test]
    fn reasoning_blocks_render_as_thinking() {
        let body = r#"[{"variant":"ai","content":"","blocks":[
            {"type":"text","content":"thinking out loud","textType":"reasoning"}
        ]}]"#;
        let conversation = transcript(body);
        assert_eq!(
            conversation.messages[0].blocks,
            vec![super::super::Block::Thinking(
                "thinking out loud".to_string()
            )]
        );
    }

    #[test]
    fn a_tool_block_measures_output_and_names_the_call() {
        let body = r#"[{"variant":"user","content":"","blocks":[
            {"type":"tool","toolName":"read_file","input":{"path":"a.js"},"output":"1"}
        ]}]"#;
        let conversation = transcript(body);
        let blocks = &conversation.messages[0].blocks;
        match &blocks[0] {
            super::super::Block::ToolCall {
                name,
                input_summary,
                output_bytes,
            } => {
                assert_eq!(name.as_deref(), Some("read_file"));
                assert!(input_summary.contains("a.js"));
                assert_eq!(*output_bytes, Some(1));
            }
            other => panic!("expected a tool call block, got {other:?}"),
        }
    }

    #[test]
    fn missing_tool_output_is_not_recorded_not_zero() {
        let body = r#"[{"variant":"user","content":"","blocks":[
            {"type":"tool","toolName":"ls","input":{}}
        ]}]"#;
        let conversation = transcript(body);
        match &conversation.messages[0].blocks[0] {
            super::super::Block::ToolCall { output_bytes, .. } => {
                assert_eq!(*output_bytes, None);
            }
            other => panic!("expected a tool call block, got {other:?}"),
        }
    }

    #[test]
    fn unknown_block_types_and_missing_variants_are_counted_unrendered() {
        // An unknown block type is counted unrendered; a message whose
        // blocks all render nothing and whose own `content` is empty
        // renders nothing either, so it is a *second* unrendered count and
        // no turn — never an empty message presented as one.
        let body = r#"[{"variant":"user","content":"","blocks":[
            {"type":"sponsored-proposal"}
        ]}]"#;
        let conversation = transcript(body);
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 2);
    }

    #[test]
    fn a_message_without_a_variant_is_unrendered_not_guessed() {
        let body = r#"[{"content":"variant-less message"}]"#;
        let conversation = transcript(body);
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 1);
    }

    #[test]
    fn a_message_with_only_unrenderable_blocks_falls_back_to_its_content() {
        let body = r#"[{"variant":"user","content":"plain mirror","blocks":[
            {"type":"sponsored-proposal"}
        ]}]"#;
        let conversation = transcript(body);
        assert_eq!(
            conversation.messages[0].blocks,
            vec![super::super::Block::Text("plain mirror".to_string())]
        );
    }

    #[test]
    fn a_message_with_no_content_and_no_blocks_is_unrendered() {
        let body = r#"[{"variant":"user","content":""}]"#;
        let conversation = transcript(body);
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 1);
    }

    #[test]
    fn an_agent_block_renders_the_sub_agents_text() {
        let body = r#"[{"variant":"ai","content":"","blocks":[
            {"type":"agent","agentName":"planner","content":"sub-agent text","status":"complete"}
        ]}]"#;
        let conversation = transcript(body);
        assert_eq!(
            conversation.messages[0].blocks,
            vec![super::super::Block::Text("sub-agent text".to_string())]
        );
    }

    #[test]
    fn the_reader_never_pretends_a_non_array_body_was_empty() {
        // `null` is a whole non-array value, not a torn one: measured as
        // unrendered, and the conversation stays empty — an honest zero,
        // because nothing claimed a turn.
        let conversation = transcript("null");
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 1);
    }

    #[test]
    fn a_broken_stream_is_measured_not_hidden() {
        let conversation = transcript("{\"variant\":\"user\"  ");
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrecognized_lines, 1);
    }
}
