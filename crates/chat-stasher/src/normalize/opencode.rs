//! opencode — reader for one archived opencode session export.
//!
//! opencode keeps its sessions in a three-table SQLite store; `collect`
//! exports exactly one session per line as
//! `{"schema":"chat-stasher.opencode.session.v1","session":{…},
//!   "messages":[…],"orphan_parts":[…]}`
//! (`sqlite_probe.rs:826-835`), where every message row carries its `data`
//! JSON (`{"role":…}`) and the rows of the `part` table that belong to it in
//! `parts`. Part `data` types and field names were measured on the local
//! store (2026-09-25, key/type vocabularies only):
//!
//! | `part.data.type` | fields read                          | block        |
//! |------------------|--------------------------------------|--------------|
//! | `text`           | `text`                               | Text         |
//! | `reasoning`      | `text`                               | Thinking     |
//! | `tool`           | `tool`, `state.input`, `state.output`| ToolCall     |
//! | `step-start` / `step-finish` / `patch` | (bookkeeping, diffs) | counted |
//!
//! A part this reader cannot render is counted — the session is one line in
//! the archive, so the counter counts records (message rows, part rows,
//! envelope), the same document-granularity wording `gemini_cli` inherits.

use super::{compact_json, message_time, Conversation, Message, Role};
use serde_json::Value;

pub(super) fn normalize_session(value: &Value, conversation: &mut Conversation) {
    let Some(messages) = value.get("messages").and_then(Value::as_array) else {
        conversation.unrendered_lines += 1;
        return;
    };
    for row in messages {
        let Some(role) = row
            .pointer("/data/role")
            .and_then(Value::as_str)
            .and_then(Role::from_str)
        else {
            conversation.unrendered_lines += 1;
            continue;
        };
        let mut blocks = Vec::new();
        for part in row
            .get("parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            blocks.push(match part_to_block(part, conversation) {
                Some(block) => block,
                None => continue,
            });
        }
        if blocks.is_empty() {
            // No part of this message was renderable. The parts themselves
            // were counted inside `part_to_block`, so the message row is
            // counted once as the record that rendered nothing.
            conversation.unrendered_lines += 1;
            continue;
        }
        conversation.push_message(Message {
            role,
            time: message_time(row.get("time_created")),
            blocks,
        });
    }
    // Parts whose message row is gone (compaction deletes messages, not the
    // parts this build already staged) arrive without a position in the
    // conversation. Rendering them would invent an order, so each is counted
    // and the raw shards remain the place they can be read.
    let orphans = value
        .get("orphan_parts")
        .and_then(Value::as_array)
        .map(|parts| parts.len())
        // reason: the exporter writes the `orphan_parts` key on every session
        // line (`sqlite_probe.rs` builds both keys unconditionally), so an
        // absent key is a session with no orphan parts — a counted zero, not
        // an unmeasured one.
        .unwrap_or(0);
    conversation.unrendered_lines += orphans;
}

/// One `part` row of the export, or `None` when it holds nothing this reader
/// can render. `None` records the miss in the given conversation's counter.
fn part_to_block(part: &Value, conversation: &mut Conversation) -> Option<super::Block> {
    let data = part.get("data").unwrap_or(part);
    let kind = data.get("type").and_then(Value::as_str);
    let block = match kind {
        Some("text") => data
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| super::Block::Text(text.to_string())),
        Some("reasoning") => data
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| super::Block::Thinking(text.to_string())),
        Some("tool") => {
            let state = data.get("state");
            Some(super::Block::ToolCall {
                name: data.get("tool").and_then(Value::as_str).map(str::to_string),
                input_summary: compact_json(state.and_then(|state| state.get("input"))),
                // A tool still running, or one whose state was not archived,
                // has no output: not zero bytes, just none recorded.
                output_bytes: state
                    .and_then(|state| state.get("output"))
                    .map(|output| compact_json(Some(output)).len()),
            })
        }
        _ => None,
    };
    if block.is_none() {
        conversation.unrendered_lines += 1;
    }
    block
}

#[cfg(test)]
mod tests {
    use super::super::{normalize, Block, Role};

    /// The export the capture side writes: message rows carry `data.role`,
    /// and each `parts[]` row carries its own `data.type`. This is the shape
    /// `sqlite_probe.rs:826-835` seals, measured against the local store —
    /// the previous test asserted a `{"schema":"opencode/v1"}` summary shape
    /// with `messages[].content` that no archive holds, which is why the
    /// real-archive smoke rendered zero opencode messages.
    #[test]
    fn an_opencode_export_renders_messages_and_parts() {
        let body = concat!(
            r#"{"schema":"chat-stasher.opencode.session.v1","session":{"id":"s1","time_created":1770000000000,"time_updated":1770000099999},"messages":["#,
            r#"{"id":"m1","session_id":"s1","time_created":1770000000000,"time_updated":1770000000000,"data":{"role":"user"},"parts":["#,
            r#"{"id":"p1","message_id":"m1","session_id":"s1","time_created":1770000000000,"time_updated":1770000000000,"data":{"type":"text","text":"question"}},"#,
            r#"{"id":"p2","message_id":"m1","session_id":"s1","time_created":1770000000100,"time_updated":1770000000100,"data":{"type":"text","text":""}}]},"#,
            r#"{"id":"m2","session_id":"s1","time_created":1770000000500,"time_updated":1770000000500,"data":{"role":"assistant"},"parts":["#,
            r#"{"id":"p3","message_id":"m2","session_id":"s1","time_created":1770000000500,"time_updated":1770000000500,"data":{"type":"reasoning","text":"weighing"}},"#,
            r#"{"id":"p4","message_id":"m2","session_id":"s1","time_created":1770000000600,"time_updated":1770000000600,"data":{"type":"text","text":"answer with `code`"}},"#,
            r#"{"id":"p5","message_id":"m2","session_id":"s1","time_created":1770000000700,"time_updated":1770000000700,"data":{"type":"step-start"}},"#,
            r#"{"id":"p6","message_id":"m2","session_id":"s1","time_created":1770000000800,"time_updated":1770000000800,"data":{"type":"tool","tool":"bash","state":{"status":"completed","input":{"cmd":"ls"},"output":"two entries"}}}]}],"#,
            r#""orphan_parts":["#,
            r#"{"id":"p9","message_id":"mGone","session_id":"s1","time_created":1770000000900,"time_updated":1770000000900,"data":{"type":"text","text":"orphan"}}]}"#,
        );
        let result = normalize("opencode", body);
        assert_eq!(result.messages.len(), 2, "user + assistant turn");
        assert!(matches!(result.messages[0].role, Role::User));
        assert!(matches!(result.messages[1].role, Role::Assistant));
        // The empty text part rendered no block, but it was counted — empty
        // recorded content is a fact about the part, not a loss.
        assert_eq!(result.messages[0].blocks.len(), 1);
        assert!(matches!(result.messages[1].blocks[0], Block::Thinking(_)));
        assert!(matches!(
            result.messages[1].blocks[1],
            Block::Text(ref text) if text.contains("code")
        ));
        assert!(matches!(
            result.messages[1].blocks[2],
            Block::ToolCall {
                ref input_summary,
                output_bytes: Some(11),
                ..
            } if input_summary.contains("cmd")
        ));
        assert_eq!(
            result.unrendered_lines, 3,
            "empty text part + step-start + one orphan: every one counted"
        );
        assert_eq!(result.unrecognized_lines, 0);
    }

    /// An export whose message rows are all unrenderable (unknown role, no
    /// data) and whose only part is bookkeeping: nothing rendered, everything
    /// counted — the coverage line and the raw link stay the honest report.
    #[test]
    fn an_opencode_export_with_no_renderable_record_counts_it_all() {
        let body = concat!(
            r#"{"schema":"chat-stasher.opencode.session.v1","session":{"id":"s2"},"messages":[{"#,
            r#""id":"m1","session_id":"s2","time_created":1770000000000,"time_updated":1770000000000,"data":{"role":"caller"},"parts":[{"#,
            r#""id":"p1","message_id":"m1","session_id":"s2","time_created":1770000000000,"time_updated":1770000000000,"data":{"type":"step-finish","reason":"stop","tokens":{"total":1}}}]},{"#,
            r#""id":"m2","session_id":"s2","time_created":1770000000001,"time_updated":1770000000001,"data":{"role":"user"},"parts":[]}],"orphan_parts":[]}"#,
        );
        let result = normalize("opencode", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(
            result.unrendered_lines, 2,
            "the unknown-role message row (its parts travel with it) + the empty-parts message"
        );
    }

    /// A line that is not the export envelope at all: one record, counted.
    #[test]
    fn a_foreign_opencode_line_is_one_unrendered_record() {
        let result = normalize("opencode", "{\"unexpected\":true}\n");
        assert_eq!(result.messages.len(), 0);
        assert_eq!(result.unrendered_lines, 1);
    }
}
