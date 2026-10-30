//! WorkBuddy transcript reader (W1046). Every record shape below was measured
//! read-only on a local install (CS-WB-1, 2026-10-30) and cross-checked
//! against three independent third-party reverse-engineered schemas; no
//! official schema exists, so the reader counts everything it cannot
//! classify instead of guessing, and a record type it has never seen is
//! counted as drift rather than treated as a failure.
//!
//! One archived body holds two producers under the same session id: the
//! sqlite metadata lines the collector exports (a live row, or the bare
//! tombstone a soft-deleted row exports) and the raw `*.jsonl` transcript
//! events. This reader walks both and renders only speech:
//!
//! * `message` — `role user`: the person's text is the *last*
//!   `<user_query>...</user_query>` span inside the content blocks; a
//!   user-role record without the tag is the harness's own injection (the
//!   `<system-reminder>` memory block, `<task-notification>`, summaries), so
//!   it renders nothing and is counted as bookkeeping, never as a person
//!   speaking. `role assistant`: the `output_text` blocks concatenate into
//!   one reply.
//! * `reasoning` — `rawContent`'s `reasoning_text` blocks (falling back to
//!   the `content` blocks a few records use) render as one thinking block.
//! * `function_call` — a tool call whose `arguments` are usually a
//!   JSON-encoded string, parsed a second time when they parse and kept raw
//!   when they do not (interrupted writes were measured). Its result is
//!   found by pairing `callId`s across lines; an unclosed call keeps
//!   `output_bytes: None` ("not recorded"), never a synthesized reply.
//! * `function_call_result` — bookkeeping: its bytes surface through the
//!   paired call's `output_bytes` and the raw shard, so the line itself
//!   renders nothing and is counted once.
//! * `session-meta`, `file-history-snapshot`, `ai-title`, `summary`,
//!   `turn-metrics` — recognized metadata, counted and rendered nothing.
//!   `summary` and `turn-metrics` have only been measured on the sibling
//!   CodeBuddy CLI, which writes the same event core (the desktop reader
//!   handles them so the shared parser stays honest when that source lands).
//! * any other `type` — census drift: counted as unrecognized, and the body
//!   keeps rendering around it.
//!
//! Line order is arrival order: the source appends, and its timestamps can
//! sit at the same millisecond or even step backwards (measured), so nothing
//! here ever sorts by time. One bad line never kills a session: counts rise,
//! the rest renders.

use super::{message_time, Block, Conversation, Message, Role};
use serde_json::Value;

/// The schema tag the collector writes for WorkBuddy metadata and tombstone
/// lines; the raw transcript events carry no schema field at all.
const SESSION_SCHEMA: &str = crate::sqlite_probe::WORKBUDDY_SESSION_SCHEMA;

/// Read one whole WorkBuddy body. The walk is owned here because tool calls
/// pair with their results by `callId` across lines.
pub(super) fn normalize_body(body: &str, conversation: &mut Conversation) {
    let mut values = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => values.push(value),
            Err(_) => conversation.unrecognized_lines += 1,
        }
    }
    // Pair every tool result to its call before rendering, so a call that
    // arrives after its result line still finds it (arrival order is the
    // source's own order, but the pair must not depend on which half
    // happened to be written first within one turn).
    let mut tool_results = std::collections::BTreeMap::new();
    for value in &values {
        if event_type(value).as_deref() != Some("function_call_result") {
            continue;
        }
        let (Some(call_id), Some(output)) = (string_field(value, "callId"), value.get("output"))
        else {
            continue;
        };
        tool_results.insert(call_id, super::compact_json(Some(output)).len());
    }
    for value in &values {
        if value.get("schema").and_then(Value::as_str) == Some(SESSION_SCHEMA) {
            // Session metadata or a tombstone the collector exported: the
            // raw shard holds it whole, and the rendered view gains nothing.
            conversation.unrendered_lines += 1;
            continue;
        }
        match event_type(value).as_deref() {
            Some("message") => render_message(value, conversation),
            Some("reasoning") => render_reasoning(value, conversation),
            Some("function_call") => render_function_call(value, conversation, &mut tool_results),
            Some("function_call_result") => {
                // Recognized bookkeeping: its bytes ride the paired call's
                // `output_bytes` (counted there), and the raw line stays
                // archived. Counted once, rendered never — including the
                // result whose call never closed.
                conversation.unrendered_lines += 1;
            }
            Some("session-meta")
            | Some("file-history-snapshot")
            | Some("ai-title")
            | Some("summary")
            | Some("turn-metrics") => {
                conversation.unrendered_lines += 1;
            }
            // A type this reader has never measured is drift, not noise:
            // the count is the alarm, and the rest of the session renders.
            _ => conversation.unrecognized_lines += 1,
        }
    }
}

/// The top-level `type` discriminant of one transcript event.
fn event_type(value: &Value) -> Option<String> {
    string_field(value, "type")
}

/// A string field that is present, a string, and non-empty.
fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn render_message(value: &Value, conversation: &mut Conversation) {
    match string_field(value, "role").as_deref() {
        Some("user") => render_user_message(value, conversation),
        Some("assistant") => render_assistant_message(value, conversation),
        // Measured roles are `user` and `assistant` only; a message with any
        // other speaker is bookkeeping this reader cannot attribute, never a
        // guess at who said it.
        _ => conversation.unrendered_lines += 1,
    }
}

/// The person's half of one turn. The harness wraps the actual prompt in a
/// `<user_query>` tag and pads the record with injected context (memory
/// blocks, task notifications — one measured injection was over 15k
/// characters around a 2-character question), so only the tag's span is the
/// person speaking, and only the *last* tag in the record: repeated tags
/// match the reference readers' measured behaviour.
fn render_user_message(value: &Value, conversation: &mut Conversation) {
    let text = accepted_block_text(value.get("content"), &["input_text", "output_text"]);
    let Some(text) = text else {
        // No readable content blocks at all: measured-empty, no message.
        return;
    };
    let Some(query) = user_query_span(&text) else {
        // No tag: an injection the source wrote in the user role. Not the
        // person speaking, so no message — counted, never rendered.
        conversation.unrendered_lines += 1;
        return;
    };
    if query.is_empty() {
        // The tag exists but holds nothing: measured-empty, no message.
        return;
    }
    conversation.push_message(Message {
        role: Role::User,
        time: message_time(value.get("timestamp")),
        blocks: vec![Block::Text(query.to_string())],
    });
}

/// The last `<user_query>(.*?)</user_query>` span, dot-all, measured against
/// the third-party readers this source was reverse-engineered alongside.
fn user_query_span(text: &str) -> Option<&str> {
    let start = text.rfind("<user_query>")?;
    let end = text[start..].find("</user_query>")?;
    Some(&text[start + "<user_query>".len()..start + end])
}

/// The assistant's final reply: every `output_text` block, concatenated in
/// arrival order with the reference reader's separator.
fn render_assistant_message(value: &Value, conversation: &mut Conversation) {
    // A reasoning-only reply with no output block is measured-empty here;
    // its reasoning rode the `reasoning` records.
    if let Some(text) = accepted_block_text(value.get("content"), &["output_text"]) {
        conversation.push_message(Message {
            role: Role::Assistant,
            time: message_time(value.get("timestamp")),
            blocks: vec![Block::Text(text)],
        });
    }
}

/// Concatenate the `.text` of blocks whose `type` the reader accepts.
/// `None` when the array holds no acceptable non-empty text at all — an
/// empty array and a missing array both stay "measured-empty, no message",
/// and a caller with a fallback array can fall through to it.
fn accepted_block_text(blocks: Option<&Value>, accepted: &[&str]) -> Option<String> {
    let blocks = blocks?.as_array()?;
    let pieces = blocks
        .iter()
        .filter_map(|block| {
            let kind = block.get("type").and_then(Value::as_str)?;
            let text = block.get("text").and_then(Value::as_str)?;
            (accepted.contains(&kind) && !text.is_empty()).then_some(text)
        })
        .collect::<Vec<_>>();
    (!pieces.is_empty()).then(|| pieces.join("\n\n"))
}

fn render_reasoning(value: &Value, conversation: &mut Conversation) {
    // `rawContent` usually carries `reasoning_text` blocks; a few records
    // use the `content` array instead, so both are probed — the source has
    // no official schema to promise either shape. An empty array falls
    // through to the other, and a reasoning record with neither renders
    // nothing without inventing an empty thought.
    let accepted = ["reasoning_text", "input_text"];
    let text = accepted_block_text(value.get("rawContent"), &accepted)
        .or_else(|| accepted_block_text(value.get("content"), &accepted));
    if let Some(text) = text {
        conversation.push_message(Message {
            role: Role::Assistant,
            time: message_time(value.get("timestamp")),
            blocks: vec![Block::Thinking(text)],
        });
    }
}

fn render_function_call(
    value: &Value,
    conversation: &mut Conversation,
    tool_results: &mut std::collections::BTreeMap<String, usize>,
) {
    let name = string_field(value, "name");
    if name.is_none() {
        // A call the record cannot even name is bookkeeping, not a tool use
        // the reader can show.
        conversation.unrendered_lines += 1;
        return;
    }
    // `arguments` is usually a JSON-encoded string that parses on the
    // second pass; when one does not (interrupted writes were measured) the
    // raw string stays instead of a pretend parse.
    let input_summary = match value.get("arguments") {
        Some(Value::String(raw)) => match serde_json::from_str::<Value>(raw) {
            Ok(parsed) => super::compact_json(Some(&parsed)),
            Err(_) => raw.clone(),
        },
        other => super::compact_json(other),
    };
    let output_bytes =
        string_field(value, "callId").and_then(|call_id| tool_results.remove(&call_id));
    conversation.push_message(Message {
        role: Role::Tool,
        time: message_time(value.get("timestamp")),
        blocks: vec![Block::ToolCall {
            name,
            input_summary,
            // `None` is the claim "the result was never recorded"; an
            // unclosed call is never synthesized a reply.
            output_bytes,
        }],
    });
}

#[cfg(test)]
mod tests {
    use crate::activity::TimeSource;
    use crate::normalize::{normalize, Block, MessageTime, Provenance, Role};

    /// The parent module's own summary helper: the byte counts below must
    /// move with it if it ever changes.
    fn super_compact_json(value: Option<&serde_json::Value>) -> String {
        crate::normalize::compact_json(value)
    }

    #[test]
    fn nine_event_types_render_and_count_their_bookkeeping() {
        let body = concat!(
            r#"{"type":"session-meta","sessionId":"synthetic-session-id","cwd":"/synthetic/project","hostKind":"synthetic-host"}"#,
            "\n",
            r#"{"id":"synthetic-m1","timestamp":1780000000000,"type":"message","role":"user","content":[{"type":"input_text","text":"<system-reminder data-role=\"user-context\">synthetic injected memory block listing projects and skills</system-reminder><user_query>synthetic genuine question</user_query>"}],"sessionId":"synthetic-session-id","cwd":"/synthetic/project"}"#,
            "\n",
            r#"{"id":"synthetic-r1","timestamp":1780000000500,"type":"reasoning","rawContent":[{"type":"reasoning_text","text":"synthetic chain of thought"}],"content":[],"sessionId":"synthetic-session-id","cwd":"/synthetic/project"}"#,
            "\n",
            r#"{"id":"synthetic-r1","timestamp":1780000000900,"type":"function_call","callId":"synthetic-call-id","name":"Read","arguments":"{\"path\":\"synthetic-file.json\"}","sessionId":"synthetic-session-id","cwd":"/synthetic/project"}"#,
            "\n",
            r#"{"id":"synthetic-r1","timestamp":1780000000800,"type":"function_call_result","callId":"synthetic-call-id","name":"Read","status":"completed","output":{"type":"text","text":"synthetic tool output"},"sessionId":"synthetic-session-id","cwd":"/synthetic/project"}"#,
            "\n",
            r#"{"id":"synthetic-r1","timestamp":1780000001200,"type":"message","role":"assistant","content":[{"type":"output_text","text":"synthetic first reply paragraph"},{"type":"output_text","text":"synthetic second reply paragraph"}],"sessionId":"synthetic-session-id","cwd":"/synthetic/project"}"#,
            "\n",
            r#"{"id":"synthetic-snap","timestamp":1780000001300,"type":"file-history-snapshot","isSnapshotUpdate":false,"snapshot":{"messageId":"synthetic-message-id","trackedFileBackups":{}},"cwd":"/synthetic/project"}"#,
            "\n",
            r#"{"timestamp":1780000001400,"type":"ai-title","aiTitle":"synthetic ai title","sessionId":"synthetic-session-id","cwd":"/synthetic/project"}"#,
            "\n",
            r#"{"type":"summary","summary":"synthetic compaction summary","sessionId":"synthetic-session-id","cwd":"/synthetic/project"}"#,
            "\n",
            r#"{"type":"turn-metrics","sessionId":"synthetic-session-id","cwd":"/synthetic/project"}"#,
            "\n",
            r#"{"schema":"chat-stasher.workbuddy.session.v1","session":"synthetic-session-id","deleted":true}"#,
            "\n",
            r#"{"schema":"chat-stasher.workbuddy.session.v1","session":{"id":"synthetic-session-id","title":"synthetic store title"}}"#,
            "\n",
            r#"{"timestamp":1780000001500,"type":"future-shape","sessionId":"synthetic-session-id"}"#,
            "\n",
        );
        let conversation = normalize("workbuddy", body);
        assert!(matches!(conversation.provenance, Provenance::Known { .. }));
        assert_eq!(
            conversation.messages.len(),
            4,
            "user, reasoning, call, assistant"
        );
        assert!(matches!(
            conversation.messages[0].blocks[0],
            Block::Text(ref text) if text == "synthetic genuine question"
        ));
        assert!(matches!(conversation.messages[0].role, Role::User));
        assert!(matches!(
            conversation.messages[1].blocks[0],
            Block::Thinking(ref text) if text == "synthetic chain of thought"
        ));
        assert!(matches!(conversation.messages[1].role, Role::Assistant));
        let Block::ToolCall {
            name,
            input_summary,
            output_bytes,
        } = &conversation.messages[2].blocks[0]
        else {
            panic!("expected a rendered tool call");
        };
        assert_eq!(name.as_deref(), Some("Read"));
        assert_eq!(input_summary, r#"{"path":"synthetic-file.json"}"#);
        let expected_output_bytes = super_compact_json(Some(&serde_json::json!({
            "type": "text",
            "text": "synthetic tool output"
        })))
        .len();
        assert_eq!(*output_bytes, Some(expected_output_bytes));
        assert!(matches!(conversation.messages[2].role, Role::Tool));
        assert!(matches!(
            conversation.messages[3].blocks[0],
            Block::Text(ref text) if text == "synthetic first reply paragraph\n\nsynthetic second reply paragraph"
        ));
        assert!(matches!(conversation.messages[3].role, Role::Assistant));
        // session-meta, the result line, file-history-snapshot, ai-title,
        // summary, turn-metrics, the tombstone and the metadata line.
        assert_eq!(conversation.unrendered_lines, 8);
        // The unknown type is named, and never fails the rest of the body.
        assert_eq!(conversation.unrecognized_lines, 1);
        assert!(matches!(
            conversation.messages[0].time,
            MessageTime::Known {
                unix: 1780000000,
                source: TimeSource::Inferred { .. }
            }
        ));
        // Identifiers stay raw: callId, sessionId and argument paths ride
        // the shard, never the rendered model.
        let serialized = format!("{conversation:?}");
        for id in [
            "synthetic-session-id",
            "synthetic-call-id",
            "synthetic-message-id",
        ] {
            assert!(
                !serialized.contains(id),
                "`{id}` reached the rendered model"
            );
        }
    }

    #[test]
    fn a_user_record_without_the_tag_is_an_injection_not_a_person() {
        let body = concat!(
            r#"{"id":"synthetic-n1","timestamp":1780000000000,"type":"message","role":"user","content":[{"type":"input_text","text":"<task-notification>synthetic injected notification</task-notification>"}],"sessionId":"synthetic-session-id"}"#,
            "\n",
        );
        let conversation = normalize("workbuddy", body);
        assert_eq!(conversation.messages.len(), 0);
        assert_eq!(conversation.unrendered_lines, 1);
        assert_eq!(conversation.unrecognized_lines, 0);
        assert!(!format!("{conversation:?}").contains("synthetic injected notification"));
    }

    #[test]
    fn the_last_user_query_tag_wins_as_the_measured_extractors_read_it() {
        let body = concat!(
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<user_query>synthetic earlier draft</user_query><user_query>synthetic final wording</user_query>"}]}"#,
            "\n",
        );
        let conversation = normalize("workbuddy", body);
        assert_eq!(conversation.messages.len(), 1);
        assert!(matches!(
            conversation.messages[0].blocks[0],
            Block::Text(ref text) if text == "synthetic final wording"
        ));
    }

    #[test]
    fn user_query_text_spans_newlines_like_the_dot_all_match() {
        let body = concat!(
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<user_query>synthetic line one\nsynthetic line two</user_query>"}]}"#,
            "\n",
        );
        let conversation = normalize("workbuddy", body);
        assert_eq!(conversation.messages.len(), 1);
        assert!(matches!(
            conversation.messages[0].blocks[0],
            Block::Text(ref text) if text == "synthetic line one\nsynthetic line two"
        ));
    }

    #[test]
    fn an_unclosed_call_keeps_its_result_unclaimed_and_the_answer_unsynthesized() {
        let body = concat!(
            r#"{"type":"function_call","callId":"synthetic-unmatched-call","name":"Grep","arguments":"{}"}"#,
            "\n",
            r#"{"type":"function_call","callId":"synthetic-quiet-call","name":"Bash","arguments":"{\"command\":\"synthetic command\"}"}"#,
            "\n",
            r#"{"type":"function_call_result","callId":"synthetic-orphan-result","name":"Bash","output":{"type":"text","text":"synthetic orphan result"}}"#,
            "\n",
        );
        let conversation = normalize("workbuddy", body);
        assert_eq!(conversation.messages.len(), 2);
        for message in &conversation.messages {
            let Block::ToolCall { output_bytes, .. } = &message.blocks[0] else {
                panic!("expected tool calls");
            };
            assert_eq!(*output_bytes, None, "no result was ever recorded");
        }
        // The orphan result is bookkeeping whose call never arrived.
        assert_eq!(conversation.unrendered_lines, 1);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    #[test]
    fn a_later_call_still_pairs_the_result_written_earlier() {
        let body = concat!(
            r#"{"type":"function_call_result","callId":"synthetic-eager-result","name":"Read","output":[{"type":"input_text","text":"synthetic early result"}]}"#,
            "\n",
            r#"{"type":"function_call","callId":"synthetic-eager-result","name":"Read","arguments":"null"}"#,
            "\n",
        );
        let conversation = normalize("workbuddy", body);
        assert_eq!(conversation.messages.len(), 1);
        let Block::ToolCall { output_bytes, .. } = &conversation.messages[0].blocks[0] else {
            panic!("expected a tool call");
        };
        assert_eq!(
            *output_bytes,
            Some(
                super_compact_json(Some(&serde_json::json!([
                    {"type": "input_text", "text": "synthetic early result"}
                ])))
                .len()
            )
        );
        // The whole result line is still one bookkeeping count.
        assert_eq!(conversation.unrendered_lines, 1);
    }

    #[test]
    fn unparseable_arguments_stay_raw_never_a_pretend_parse() {
        let body = concat!(
            r#"{"type":"function_call","callId":"synthetic-torn-call","name":"WebFetch","arguments":"{\"url\":\"https://synthetic-page"}"#,
            "\n",
        );
        let conversation = normalize("workbuddy", body);
        // The torn JSON inside the arguments value, not the line: the record
        // still renders with what it honestly holds.
        assert_eq!(conversation.messages.len(), 1);
        let Block::ToolCall { input_summary, .. } = &conversation.messages[0].blocks[0] else {
            panic!("expected a tool call");
        };
        // The second parse fails on the truncated inner document, so the
        // raw string is the summary.
        assert_eq!(input_summary, r#"{"url":"https://synthetic-page"#);
    }

    #[test]
    fn reasoning_through_the_content_array_still_renders() {
        let body = concat!(
            r#"{"type":"reasoning","content":[{"type":"input_text","text":"synthetic reasoning via content"}],"rawContent":[]}"#,
            "\n",
        );
        let conversation = normalize("workbuddy", body);
        assert_eq!(conversation.messages.len(), 1);
        assert!(matches!(
            conversation.messages[0].blocks[0],
            Block::Thinking(ref text) if text == "synthetic reasoning via content"
        ));
    }

    #[test]
    fn an_empty_assistant_reply_renders_nothing_and_counts_nothing() {
        let body = concat!(
            r#"{"type":"message","role":"assistant","content":[]}"#,
            "\n",
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":""}]}"#,
            "\n",
        );
        let conversation = normalize("workbuddy", body);
        assert_eq!(conversation.messages.len(), 0);
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    #[test]
    fn arrival_order_survives_timestamp_inversions() {
        let body = concat!(
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<user_query>synthetic later line stamped earlier</user_query>"}],"timestamp":1779999999000}"#,
            "\n",
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"<user_query>synthetic earlier line stamped later</user_query>"}],"timestamp":1780000001000}"#,
            "\n",
        );
        let conversation = normalize("workbuddy", body);
        assert_eq!(conversation.messages.len(), 2);
        assert!(matches!(
            conversation.messages[0].blocks[0],
            Block::Text(ref text) if text == "synthetic later line stamped earlier"
        ));
        assert!(matches!(
            conversation.messages[1].blocks[0],
            Block::Text(ref text) if text == "synthetic earlier line stamped later"
        ));
    }
}
