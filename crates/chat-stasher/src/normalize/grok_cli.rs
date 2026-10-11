//! Grok CLI — readers for the two bodies the `grok` harness id archives.
//!
//! The id covers two producers (W911): the SQLite `session_docs` row
//! this tool exports from Grok's search DB, and the native JSONL
//! transcript the Grok CLI itself keeps (`chat_history.jsonl`). The
//! export carries `title`, `updated_at` and a plain-text `content`
//! field; it preserves no message boundaries or speakers, so it stays
//! one text block with an explicitly unknown speaker. The native
//! transcript carries typed records (`system`, `user`, `reasoning`,
//! `assistant`, `tool_result`) and no timestamp field at all. A body
//! from which the native records render at least one message is read
//! from them, and the legacy summary is not appended again as another
//! turn (ADR-056 D7); a body whose native records are all recognized
//! but render nothing keeps the summary as its reading, because the
//! summary is then the only readable text the body holds. The raw
//! route keeps both observations either way.

use super::{Block, Conversation, Message, MessageTime, Role};
use serde_json::Value;

const EXPORT_SCHEMA: &str = "chat-stasher.sqlite.session.v1";
const EXPORT_TABLE: &str = "session_docs";

/// Walk one archived Grok body, which may hold either observation of
/// the session: the legacy `session_docs` export line, native
/// transcript records, or both.
///
/// Which view the derived reading takes is a per-body fact, so the
/// body is scanned once for a native record this reader will actually
/// render before any line is handled: with such a record present, the
/// export's plain-text summary is the same session observed the old
/// way and must not become another turn, while a body whose native
/// records all render nothing keeps rendering its summary exactly as
/// before — with no native reading to prefer, the summary stays the
/// body's only readable text. Lines that are neither are served the
/// harness's existing reader path (the web-platform bundle shape), so
/// no line's accounting changes with the format it turns out to be.
pub(super) fn normalize_body(body: &str, conversation: &mut Conversation) {
    // Whether the native records render at least one message is a
    // per-body fact, so the body is scanned once before any line is
    // handled. Recognition alone would be too weak a condition: a
    // body of records this reader recognizes but renders nothing
    // holds no native reading to prefer, and its legacy summary is
    // the only readable text it has — suppressing that would show
    // the session as empty where the reader in fact read a summary.
    let native_renderable = body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|value| {
            native_record_kind(&value).is_some_and(|kind| !native_blocks(&value, kind).is_empty())
        });
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                conversation.unrecognized_lines += 1;
                continue;
            }
        };
        normalize_line(&value, native_renderable, conversation);
    }
}

fn normalize_line(value: &Value, native_renderable: bool, conversation: &mut Conversation) {
    if is_session_docs_record(value) {
        if native_renderable {
            // Recognized and deliberately not rendered: the summary is
            // the legacy observation of this same session, and the
            // native records are the reading's source. It is neither an
            // unrendered nor an unrecognized line — both of those are
            // statements about lines this reader could not use, and the
            // raw route still carries the summary's bytes.
            return;
        }
        normalize_session(value, conversation);
        return;
    }
    if let Some(kind) = native_record_kind(value) {
        normalize_native_record(value, kind, conversation);
        return;
    }
    // Every other line keeps the harness's existing reader path: the
    // web-platform bundle shape, whose generic reader this id shares.
    match super::payload(value) {
        Some(payload) => super::normalize_generic(&payload, conversation),
        None => conversation.unrendered_lines += 1,
    }
}

/// The record type a value is, or `None` when it is not one of the
/// native transcript records this build reads (`system`, `user`,
/// `reasoning`, `assistant`, `tool_result`).
///
/// Recognition grants the record type, and it rests on the field that
/// carries that type's content — a `type` key alone claims nothing,
/// and a record whose content container is not the evidenced shape is
/// counted by the generic reader rather than rendered. Every other
/// field is rendered per part instead of gating the whole record: a
/// user content array may mix text parts with parts this reader
/// cannot name, and assistant `tool_calls` hold whatever calls they
/// hold — one deviating part must not cost the record its text, so
/// the recognized parts render and the rest stay raw. A recognized
/// record that renders nothing is a measured unrendered line (see
/// [`normalize_native_record`]). The format's other record type
/// (`backend_tool_call`) is harness tool traffic and is not claimed
/// here.
fn native_record_kind(value: &Value) -> Option<&str> {
    let kind = value.get("type").and_then(Value::as_str)?;
    let recognized = match kind {
        "system" => value.get("content").and_then(Value::as_str).is_some(),
        "user" => value.get("content").and_then(Value::as_array).is_some(),
        "reasoning" => {
            value
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some()
                && value
                    .get("summary")
                    .is_none_or(|summary| summary.as_array().is_some())
        }
        "assistant" => value.get("content").and_then(Value::as_str).is_some(),
        "tool_result" => {
            value.get("content").and_then(Value::as_str).is_some()
                && value.get("tool_call_id").and_then(Value::as_str).is_some()
        }
        _ => false,
    };
    recognized.then_some(kind)
}

/// Render one native record [`native_record_kind`] recognized.
///
/// A record that renders at least one block becomes one message; a
/// record that renders nothing — recognized, with no content this
/// reader can show — is a measured unrendered line, not an empty
/// message. `kind` is the arm recognition granted, so the match
/// below never meets a type it did not clear.
fn normalize_native_record(value: &Value, kind: &str, conversation: &mut Conversation) {
    let blocks = native_blocks(value, kind);
    if blocks.is_empty() {
        conversation.unrendered_lines += 1;
        return;
    }
    conversation.push_message(Message {
        role: native_role(value, kind),
        // The native transcript records no per-message timestamp
        // (measured across the installed format: no key of any record
        // names a time), so every native time stays unknown.
        time: MessageTime::Unknown {
            why: "the Grok native transcript carries no per-message timestamp".to_string(),
        },
        blocks,
    });
}

/// The role a native record carries.
///
/// `user` is the one judgment call, and the format itself bounds it:
/// a user-type record marked with `synthetic_reason` is harness-
/// injected content (the field is the format's own discriminator), so
/// it is system content rather than a turn by the person — but no
/// field proves that an *unmarked* user-type record was written by
/// the human, so those keep the unknown-speaker state instead of
/// `type=user` being read as certain human origin. Reasoning is
/// model-side content; tool results are tool traffic.
fn native_role(value: &Value, kind: &str) -> Role {
    match kind {
        "system" => Role::System,
        "user" => {
            if value
                .get("synthetic_reason")
                .and_then(Value::as_str)
                .is_some()
            {
                Role::System
            } else {
                Role::Unknown
            }
        }
        "reasoning" | "assistant" => Role::Assistant,
        "tool_result" => Role::Tool,
        // `is_native_record` already restricted `kind` to the five
        // recognized types; this arm is the match's totality, not a
        // claim about any other type.
        _ => Role::Unknown,
    }
}

/// The renderable blocks of one native record, in the body's own
/// order. An empty result means the record was recognized but carries
/// nothing this reader can render — a measured unrendered line, not an
/// empty message.
fn native_blocks(value: &Value, kind: &str) -> Vec<Block> {
    match kind {
        "system" => text_part(value.get("content")),
        "user" => text_parts(value.get("content")),
        "reasoning" => summary_parts(value.get("summary")),
        "assistant" => {
            let mut blocks = text_part(value.get("content"));
            if let Some(calls) = value.get("tool_calls").and_then(Value::as_array) {
                blocks.extend(calls.iter().filter_map(|call| {
                    let name = call.get("name").and_then(Value::as_str)?;
                    Some(Block::ToolCall {
                        name: Some(name.to_string()),
                        // `tool_calls` did not gate the record's
                        // recognition, so a call arguing with the
                        // measured shape must not cost the record's
                        // answer text either: an arguments value is
                        // summarized the way the web-bundle reader
                        // summarizes one — a recorded string verbatim,
                        // any other recorded shape compacted — and a
                        // call with no name at all stays raw.
                        input_summary: super::compact_json(call.get("arguments")),
                        // The call record names the call; what came
                        // back is in the `tool_result` that follows
                        // it, if one was archived. Writing 0 here
                        // would claim a measurement.
                        output_bytes: None,
                    })
                }));
            }
            blocks
        }
        "tool_result" => vec![Block::ToolCall {
            name: value
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            input_summary: "tool result".to_string(),
            // The result's own bytes are the recorded output, empty
            // included: "recorded as empty" is a measurement, and
            // `None` would claim the output was not recorded at all.
            output_bytes: value.get("content").and_then(Value::as_str).map(str::len),
        }],
        // `is_native_record` already restricted `kind` to the five
        // recognized types; this arm is the match's totality, not a
        // claim about any other type.
        _ => Vec::new(),
    }
}

/// One text block for a string-content record (`system`, `assistant`).
fn text_part(value: Option<&Value>) -> Vec<Block> {
    match value.and_then(Value::as_str) {
        Some(text) => vec![Block::Text(text.to_string())],
        None => Vec::new(),
    }
}

/// One text block per text part of a user record's content array. The
/// array may mix part kinds; parts this reader cannot render as text
/// (inline image bytes are neither conversation text nor an attachment
/// this build can name) stay raw rather than costing the record its
/// text — the tool-result arm keeps its own `images` raw the same way.
fn text_parts(value: Option<&Value>) -> Vec<Block> {
    match value.and_then(Value::as_array) {
        Some(parts) => parts
            .iter()
            .filter_map(|part| {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    part.get("text")
                        .and_then(Value::as_str)
                        .map(|text| Block::Text(text.to_string()))
                } else {
                    None
                }
            })
            .collect(),
        None => Vec::new(),
    }
}

/// One thinking block per summary part of a reasoning record's
/// summary. A summary that mixes part kinds renders the summary parts
/// it holds; the rest stay raw, same as a user record's mixed parts.
fn summary_parts(value: Option<&Value>) -> Vec<Block> {
    match value.and_then(Value::as_array) {
        Some(parts) => parts
            .iter()
            .filter_map(|part| {
                if part.get("type").and_then(Value::as_str) == Some("summary_text") {
                    part.get("text")
                        .and_then(Value::as_str)
                        .map(|text| Block::Thinking(text.to_string()))
                } else {
                    None
                }
            })
            .collect(),
        None => Vec::new(),
    }
}

fn is_session_docs_record(value: &Value) -> bool {
    value.get("schema").and_then(Value::as_str) == Some(EXPORT_SCHEMA)
        && value.get("table").and_then(Value::as_str) == Some(EXPORT_TABLE)
}

fn normalize_session(value: &Value, conversation: &mut Conversation) {
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

    // --- Native `chat_history.jsonl` recognition (ADR-056 D7) ---
    //
    // The fixtures below are synthetic. Their shapes were measured on
    // the installed Grok CLI's native transcript: one JSON record per
    // line, `type` in {system, user, reasoning, assistant,
    // tool_result}, no timestamp field anywhere, user content as an
    // array of `{"type":"text","text":…}` items, reasoning as an
    // `encrypted_content` string plus a `summary` array of
    // `{"type":"summary_text","text":…}` items, assistant tool calls
    // as `{"name":…,"arguments":…,"id":…}`, and tool results carrying
    // `content` plus the `tool_call_id` they answer. The mixed-part
    // fixtures are deliberately wider than the measurement (the format
    // demonstrably carries image parts), because recognition rests on
    // each type's content container and parts render per part.

    #[test]
    fn a_full_native_history_renders_every_recorded_type_in_order() {
        let body = concat!(
            r#"{"type":"system","content":"synthetic system text"}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"synthetic user turn"}]}"#,
            "\n",
            r#"{"type":"reasoning","encrypted_content":"synthetic-ciphertext","id":"r1","status":"completed","summary":[{"type":"summary_text","text":"synthetic reasoning"}]}"#,
            "\n",
            r#"{"type":"assistant","content":"synthetic answer"}"#,
            "\n",
            r#"{"type":"tool_result","content":"synthetic output","tool_call_id":"call-1"}"#,
            "\n",
        );
        let conversation = normalize("grok", body);
        assert_eq!(conversation.messages.len(), 5);
        assert!(matches!(conversation.messages[0].role, Role::System));
        // A user-type record's author is not proven by the format (see
        // the role test below), so it keeps the unknown-speaker state.
        assert!(matches!(conversation.messages[1].role, Role::Unknown));
        // Reasoning is model-side content.
        assert!(matches!(conversation.messages[2].role, Role::Assistant));
        assert!(matches!(conversation.messages[3].role, Role::Assistant));
        // A tool result is tool traffic, never a human turn.
        assert!(matches!(conversation.messages[4].role, Role::Tool));
        // The native transcript records no per-message timestamp, so
        // every time stays unknown rather than guessed.
        for message in &conversation.messages {
            assert!(matches!(message.time, MessageTime::Unknown { .. }));
        }
    }

    #[test]
    fn native_user_record_renders_its_text_without_claiming_a_human_speaker() {
        let line = r#"{"type":"user","content":[{"type":"text","text":"synthetic user turn"}]}"#;
        let conversation = normalize("grok", line);
        assert_eq!(conversation.messages.len(), 1);
        // The format marks *injected* user-type records with
        // `synthetic_reason`, but no field proves a user-type record
        // was written by the person. The speaker therefore stays
        // unknown — the format's own uncertainty, preserved — instead
        // of `type=user` being read as certain human origin.
        assert!(matches!(conversation.messages[0].role, Role::Unknown));
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::Text(text) if text == "synthetic user turn"
        ));
        assert!(matches!(
            conversation.messages[0].time,
            MessageTime::Unknown { .. }
        ));
    }

    #[test]
    fn native_user_record_with_synthetic_reason_is_not_a_human_turn() {
        let line = r#"{"type":"user","synthetic_reason":"system_reminder","content":[{"type":"text","text":"synthetic reminder"}]}"#;
        let conversation = normalize("grok", line);
        assert_eq!(conversation.messages.len(), 1);
        // `synthetic_reason` is the format's own discriminator: the
        // harness injected this user-type record, so it is system
        // content, not a turn by the person. The text is still
        // rendered — the record is recognized, not dropped.
        assert!(matches!(conversation.messages[0].role, Role::System));
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::Text(text) if text == "synthetic reminder"
        ));
    }

    #[test]
    fn native_system_record_renders_as_system() {
        let line = r#"{"type":"system","content":"synthetic system text"}"#;
        let conversation = normalize("grok", line);
        assert_eq!(conversation.messages.len(), 1);
        assert!(matches!(conversation.messages[0].role, Role::System));
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::Text(text) if text == "synthetic system text"
        ));
    }

    #[test]
    fn native_assistant_record_renders_text_and_its_tool_calls() {
        let line = r#"{"type":"assistant","content":"synthetic answer","model_id":"synthetic-model","reasoning_effort":"high","tool_calls":[{"name":"synthetic_tool","arguments":"{\"q\":\"x\"}","id":"call-1"}]}"#;
        let conversation = normalize("grok", line);
        assert_eq!(conversation.messages.len(), 1);
        assert!(matches!(conversation.messages[0].role, Role::Assistant));
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::Text(text) if text == "synthetic answer"
        ));
        // The call record names the call; what came back is in the
        // `tool_result` that follows it, so no output size is claimed.
        assert!(matches!(
            &conversation.messages[0].blocks[1],
            Block::ToolCall { name, input_summary, output_bytes }
                if name.as_deref() == Some("synthetic_tool")
                    && input_summary == "{\"q\":\"x\"}"
                    && output_bytes.is_none()
        ));
    }

    #[test]
    fn native_tool_result_record_is_tool_traffic_with_measured_output() {
        let line =
            r#"{"type":"tool_result","content":"synthetic tool output","tool_call_id":"call-1"}"#;
        let conversation = normalize("grok", line);
        assert_eq!(conversation.messages.len(), 1);
        assert!(matches!(conversation.messages[0].role, Role::Tool));
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::ToolCall { name, input_summary, output_bytes }
                if name.as_deref() == Some("call-1")
                    && input_summary == "tool result"
                    && *output_bytes == Some("synthetic tool output".len())
        ));
    }

    #[test]
    fn native_tool_result_images_are_not_text_and_stay_unrendered() {
        let line = r#"{"type":"tool_result","content":"synthetic tool output","tool_call_id":"call-1","images":[{"type":"image","url":"data:synthetic-image-bytes"}]}"#;
        let conversation = normalize("grok", line);
        assert_eq!(conversation.messages.len(), 1);
        // Inline image bytes are not conversation text and not an
        // attachment reference this build can name; the raw archive
        // keeps them.
        assert!(conversation.attachments.is_empty());
        let rendered = conversation.messages[0]
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) | Block::Thinking(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!rendered
            .iter()
            .any(|text| text.contains("data:synthetic-image-bytes")));
    }

    #[test]
    fn native_reasoning_record_renders_its_recorded_summary_and_keeps_encrypted_content_raw() {
        let line = r#"{"type":"reasoning","encrypted_content":"synthetic-ciphertext-blob","id":"r1","status":"completed","summary":[{"type":"summary_text","text":"synthetic reasoning summary"}]}"#;
        let conversation = normalize("grok", line);
        assert_eq!(conversation.messages.len(), 1);
        assert!(matches!(conversation.messages[0].role, Role::Assistant));
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::Thinking(text) if text == "synthetic reasoning summary"
        ));
        // The encrypted reasoning stays raw: no derived block carries
        // it, so it can reach neither the reader nor the search index
        // as plaintext.
        for message in &conversation.messages {
            for block in &message.blocks {
                let text = match block {
                    Block::Text(text) | Block::Thinking(text) => text,
                    _ => continue,
                };
                assert!(!text.contains("synthetic-ciphertext-blob"));
            }
        }
    }

    #[test]
    fn native_reasoning_record_without_a_summary_is_counted_not_emptied() {
        let line = r#"{"type":"reasoning","encrypted_content":"synthetic-ciphertext-blob","id":"r1","status":"completed","summary":[]}"#;
        let conversation = normalize("grok", line);
        assert!(conversation.messages.is_empty());
        // Recognized, but its only content is encrypted: a line the
        // reader could not render, which is not the same measurement
        // as an empty one.
        assert_eq!(conversation.unrendered_lines, 1);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    #[test]
    fn mixed_native_and_legacy_body_reads_native_records_without_appending_the_summary() {
        let body = concat!(
            r#"{"schema":"chat-stasher.sqlite.session.v1","table":"session_docs","session":{"session_id":"opaque","updated_at":1789384914,"title":"synthetic title","content":"synthetic legacy summary"}}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"synthetic user turn"}]}"#,
            "\n",
            r#"{"type":"assistant","content":"synthetic answer"}"#,
            "\n",
        );
        let conversation = normalize("grok", body);
        assert_eq!(conversation.messages.len(), 2);
        let rendered = rendered_texts(&conversation);
        assert!(rendered.contains(&"synthetic user turn"));
        assert!(rendered.contains(&"synthetic answer"));
        // The summary is the same session observed the legacy way:
        // with a native reading present it must not become another
        // turn. The raw route still keeps both observations.
        assert!(!rendered.contains(&"synthetic legacy summary"));
    }

    #[test]
    fn legacy_summary_after_native_records_is_still_not_appended() {
        let body = concat!(
            r#"{"type":"user","content":[{"type":"text","text":"synthetic user turn"}]}"#,
            "\n",
            r#"{"schema":"chat-stasher.sqlite.session.v1","table":"session_docs","session":{"session_id":"opaque","updated_at":1789384914,"title":"synthetic title","content":"synthetic legacy summary"}}"#,
            "\n",
        );
        let conversation = normalize("grok", body);
        assert_eq!(conversation.messages.len(), 1);
        let rendered = rendered_texts(&conversation);
        assert!(rendered.contains(&"synthetic user turn"));
        assert!(!rendered.contains(&"synthetic legacy summary"));
    }

    #[test]
    fn a_body_whose_native_records_render_nothing_keeps_the_legacy_summary() {
        // Recognition alone must not suppress the summary: native
        // records that are all recognized but render nothing leave no
        // native reading to prefer, and the summary is then the body's
        // only readable text. The native lines are still counted for
        // what they are — recognized, nothing renderable.
        let body = concat!(
            r#"{"schema":"chat-stasher.sqlite.session.v1","table":"session_docs","session":{"session_id":"opaque","updated_at":1789384914,"title":"synthetic title","content":"synthetic legacy summary"}}"#,
            "\n",
            r#"{"type":"reasoning","encrypted_content":"synthetic-ciphertext-blob","id":"r1","status":"completed","summary":[]}"#,
            "\n",
        );
        let conversation = normalize("grok", body);
        assert_eq!(conversation.messages.len(), 1);
        let rendered = rendered_texts(&conversation);
        assert!(rendered.contains(&"synthetic legacy summary"));
        assert_eq!(conversation.unrendered_lines, 1);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    #[test]
    fn a_legacy_only_body_with_a_trailing_unparseable_line_keeps_summary_and_count() {
        // An unparseable line is never a native record, so it cannot
        // turn suppression on: the summary stays the reading and the
        // line stays counted as unrecognized.
        let body = concat!(
            r#"{"schema":"chat-stasher.sqlite.session.v1","table":"session_docs","session":{"session_id":"opaque","updated_at":1789384914,"title":"synthetic title","content":"synthetic legacy summary"}}"#,
            "\nnot json\n",
        );
        let conversation = normalize("grok", body);
        assert_eq!(conversation.messages.len(), 1);
        let rendered = rendered_texts(&conversation);
        assert!(rendered.contains(&"synthetic legacy summary"));
        assert_eq!(conversation.unrecognized_lines, 1);
        assert_eq!(conversation.unrendered_lines, 0);
    }

    #[test]
    fn native_body_with_an_unparseable_line_keeps_counting_it() {
        let body = concat!(
            r#"{"type":"user","content":[{"type":"text","text":"synthetic user turn"}]}"#,
            "\nnot json\n",
        );
        let conversation = normalize("grok", body);
        assert_eq!(conversation.messages.len(), 1);
        // Reader errors are not suppressed by native recognition.
        assert_eq!(conversation.unrecognized_lines, 1);
    }

    #[test]
    fn native_shaped_json_that_does_not_match_the_format_is_counted_not_claimed() {
        // A `type` alone is not a native record: there is no content
        // to render, and the line is counted, not claimed as a
        // message with zero text.
        let bare = normalize("grok", r#"{"type":"user"}"#);
        assert!(bare.messages.is_empty());
        assert_eq!(bare.unrendered_lines, 1);
        // A user record whose content is not the evidenced text array
        // is not claimed either.
        let wrong = normalize("grok", r#"{"type":"user","content":"synthetic user turn"}"#);
        assert!(wrong.messages.is_empty());
        assert_eq!(wrong.unrendered_lines, 1);
    }

    #[test]
    fn native_user_record_with_only_unrenderable_parts_is_counted_not_emptied() {
        let line = r#"{"type":"user","content":[{"type":"image","url":"data:synthetic"}]}"#;
        let conversation = normalize("grok", line);
        assert!(conversation.messages.is_empty());
        // The record is native and recognized, but its only content is
        // parts this reader cannot render as text — a line the reader
        // could not use, not an empty message.
        assert_eq!(conversation.unrendered_lines, 1);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    #[test]
    fn native_user_record_with_mixed_parts_renders_its_text_and_keeps_the_rest_raw() {
        // The format demonstrably carries image parts; a user record
        // that mixes one into its content array must not cost the
        // prose its text parts carry. The text parts render, the image
        // part stays raw, and the line is read, not counted.
        let line = concat!(
            r#"{"type":"user","content":["#,
            r#"{"type":"text","text":"synthetic user turn"},"#,
            r#"{"type":"image","url":"data:synthetic"},"#,
            r#"{"type":"text","text":"synthetic user follow-up"}"#,
            r#"]}"#,
        );
        let conversation = normalize("grok", line);
        assert_eq!(conversation.messages.len(), 1);
        let rendered = rendered_texts(&conversation);
        assert_eq!(
            rendered,
            vec!["synthetic user turn", "synthetic user follow-up"]
        );
        // Inline image bytes are neither text nor an attachment this
        // build can name; they reach no block and no attachment list.
        assert!(conversation.attachments.is_empty());
        assert_eq!(conversation.unrendered_lines, 0);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    #[test]
    fn native_user_record_with_mixed_parts_is_a_renderable_reading_for_suppression() {
        // The scan that decides whether the legacy summary stays must
        // count a mixed-parts record, not only perfectly shaped ones:
        // its text parts render, so the native reading exists.
        let body = concat!(
            r#"{"schema":"chat-stasher.sqlite.session.v1","table":"session_docs","session":{"session_id":"opaque","updated_at":1789384914,"title":"synthetic title","content":"synthetic legacy summary"}}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"synthetic user turn"},{"type":"image","url":"data:synthetic"}]}"#,
            "\n",
        );
        let conversation = normalize("grok", body);
        let rendered = rendered_texts(&conversation);
        assert!(rendered.contains(&"synthetic user turn"));
        assert!(!rendered.contains(&"synthetic legacy summary"));
    }

    #[test]
    fn native_reasoning_record_with_mixed_summary_parts_renders_the_summary_parts() {
        let line = r#"{"type":"reasoning","encrypted_content":"synthetic-ciphertext","summary":[{"type":"summary_text","text":"synthetic reasoning"},{"type":"image","url":"data:synthetic"},{"type":"summary_text","text":"synthetic reasoning tail"}]}"#;
        let conversation = normalize("grok", line);
        assert_eq!(conversation.messages.len(), 1);
        let thinking = conversation.messages[0]
            .blocks
            .iter()
            .filter_map(|block| match block {
                Block::Thinking(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            thinking,
            vec!["synthetic reasoning", "synthetic reasoning tail"]
        );
        assert_eq!(conversation.unrendered_lines, 0);
    }

    #[test]
    fn native_reasoning_record_without_a_summary_key_is_counted_not_emptied() {
        // The shape the format writes when it recorded no thinking
        // summary at all: the `summary` key is absent, and the record
        // is native while carrying nothing this reader can render.
        let line = r#"{"type":"reasoning","encrypted_content":"synthetic-ciphertext-blob","id":"r1","status":"completed"}"#;
        let conversation = normalize("grok", line);
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 1);
        assert_eq!(conversation.unrecognized_lines, 0);
    }

    #[test]
    fn native_assistant_record_with_object_shaped_arguments_keeps_its_answer_text() {
        // A tool call whose `arguments` is recorded as an object
        // rather than the measured string must not cost the record
        // its answer text: the call renders with its arguments
        // compacted, the way the web-bundle reader summarizes one.
        let line = r#"{"type":"assistant","content":"synthetic answer","tool_calls":[{"name":"synthetic_tool","arguments":{"q":"x"},"id":"call-1"}]}"#;
        let conversation = normalize("grok", line);
        assert_eq!(conversation.messages.len(), 1);
        assert!(matches!(
            &conversation.messages[0].blocks[0],
            Block::Text(text) if text == "synthetic answer"
        ));
        assert!(matches!(
            &conversation.messages[0].blocks[1],
            Block::ToolCall { name, input_summary, output_bytes }
                if name.as_deref() == Some("synthetic_tool")
                    && input_summary == "{\"q\":\"x\"}"
                    && output_bytes.is_none()
        ));
        assert_eq!(conversation.unrendered_lines, 0);
    }

    #[test]
    fn backend_tool_call_records_are_counted_not_rendered() {
        // `backend_tool_call` is a sixth type the installed format
        // carries, but it is not one of the five ADR-056 D7 names: it
        // is harness tool traffic this reader does not render, so it
        // stays counted and its text is not claimed as a message.
        let line = r#"{"type":"backend_tool_call","kind":{"action":{"query":"synthetic query","sources":[],"type":"search"},"id":"tool-1","status":"completed","tool_type":"web_search"}}"#;
        let conversation = normalize("grok", line);
        assert!(conversation.messages.is_empty());
        assert_eq!(conversation.unrendered_lines, 1);
    }

    /// The text blocks of a conversation, for duplication assertions.
    fn rendered_texts(conversation: &crate::normalize::Conversation) -> Vec<&str> {
        conversation
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}
