//! deepseek-harness — reader for one archived DeepSeek Harness (`dsh`) session.
//!
//! `dsh` keeps one session per directory, `<DSH_HOME>/sessions/<cwd-slug>/<session-id>/`,
//! written as `session.v4.jsonl.zstd`: a **concatenation of Zstandard frames**
//! whose first frame is the session header and whose later frames each hold one
//! append batch of `{type, seq, time, data[, surfaceOp][, sourceEventSeqs]}`
//! records. The archive holds the decoded lines — the same route `codex`'s
//! `.jsonl.zst` already takes — so this reader sees a plain JSONL stream and
//! never touches the compression. (The frame structure still matters upstream:
//! the extension is `.jsonl.zstd`, not `.jsonl.zst`, and a reader that decodes
//! only the first frame reads one record out of hundreds.)
//!
//! The shapes below were measured on the installed build (macOS app
//! `0.2.0-rc.2`, session format v4, 2026-10-03; 7 sessions, 232 frames on one
//! machine) by decoding every frame and recording **field names, counts and
//! digests only** — never message text:
//!
//! * `session` — the header frame: `{version, id, createdAt, cwd, isSeeded,
//!   delegationDepth, agentPreset, parentSession?, origin?}`. It is metadata,
//!   not a message: it is recognised here and carries nothing to render.
//! * `user/message` — `data = {content[], source, role, id}`.
//! * `assistant/message` — `data = {turn, step, message, usage, stream,
//!   interrupted?}`, the text being `data.message.content[]` with
//!   `data.message = {role, content[], source, id}`. `usage` (token counts) and
//!   `stream` (per-chunk timing) are telemetry, not conversation.
//! * `system/message` — `data = {turn, step, message}`, same `message.content[]`.
//! * `tool/result` — `data = {turn, step, message, meta?}`, the output being
//!   `data.message.content[]`; `data.message.isError` marks a failed call. It is
//!   the harness handing output back rather than the person speaking, so it is
//!   labelled `Tool`, exactly as the harness itself records the role.
//! * content parts measured: `{type: "text", text}` and
//!   `{type: "tool_use", id, name, arguments}`. Both are already read by
//!   [`super::blocks_from_content`], so this extractor introduces no part
//!   vocabulary of its own.
//! * `tool/call` — `data = {turn, step, callId, name, arguments}` — is the **same
//!   call** the `assistant/message` already carries as a `tool_use` part, matched
//!   by `callId`. Rendering both would show one call twice, so it is recognised
//!   and stays unrendered.
//! * every other measured record (`turn/start`, `step/start`, `step/end`,
//!   `turn/end`, `session/title`, `assistant/attempt`, `request/header`,
//!   `request/context`, `approval/policy`, `permission/preset`, `sandbox/mode`,
//!   `command/run`, `command/done`, `agent/inbox/spliced`, `subagent/descriptor`,
//!   `subagent/catalog`, `session/title-llm-request`,
//!   `session-log-deepseek/delivery-accepted`,
//!   `web/deepseek-search-llm-request`) is a lifecycle or companion fact: this
//!   build knows what it is and that it carries no message text, so it is not
//!   counted. A type this build does **not** know is a different claim — `dsh`
//!   is a developer preview that states compatibility-breaking changes are
//!   expected, so an unrecognised record is undetermined rather than metadata,
//!   and it is counted so the gap stays visible instead of the session reading
//!   as complete.
//!
//! Time: the envelope's `time` is the writer's `Date.now()` in **epoch
//! milliseconds**, and `data.message` carries no timestamp of its own, so every
//! message time here is the writing clock rather than a time the content itself
//! records. ADR-035 keeps those two states apart, and
//! [`super::message_time`] labels which of them this is.

use super::{blocks_from_content, message_time, Conversation, Message, Role};
use serde_json::Value;

/// The measured `type` values that are records of this session rather than
/// message text. A value on this list is classified and carries nothing to
/// render; a value off it is counted (see the module docs).
const NON_MESSAGE_TYPES: &[&str] = &[
    "session",
    "turn/start",
    "turn/end",
    "step/start",
    "step/end",
    "session/title",
    "session/title-llm-request",
    "assistant/attempt",
    "tool/call",
    "request/header",
    "request/context",
    "approval/policy",
    "permission/preset",
    "sandbox/mode",
    "command/run",
    "command/done",
    "agent/inbox/spliced",
    "subagent/descriptor",
    "subagent/catalog",
    "session-log-deepseek/delivery-accepted",
    "web/deepseek-search-llm-request",
];

/// Read one archived `dsh` record. Returns nothing: everything the reader
/// learned is in `conversation`.
pub(super) fn record(value: &Value, conversation: &mut Conversation) {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        // A record with no `type` claims nothing about the vocabulary, so it is
        // unreadable rather than unrecognised-but-benign.
        conversation.unrendered_lines += 1;
        return;
    };
    let data = value.get("data");
    let (role, content) = match kind {
        "user/message" => (Role::User, data.and_then(|data| data.get("content"))),
        "assistant/message" => (
            Role::Assistant,
            data.and_then(|data| data.get("message"))
                .and_then(|message| message.get("content")),
        ),
        "system/message" => (
            Role::System,
            data.and_then(|data| data.get("message"))
                .and_then(|message| message.get("content")),
        ),
        "tool/result" => (
            Role::Tool,
            data.and_then(|data| data.get("message"))
                .and_then(|message| message.get("content")),
        ),
        other if NON_MESSAGE_TYPES.contains(&other) => return,
        _ => {
            conversation.unrendered_lines += 1;
            return;
        }
    };
    let Some(content) = content else {
        // The record names itself as a message and does not carry the field the
        // message lives in. That is drift, not an empty message.
        conversation.unrendered_lines += 1;
        return;
    };
    let blocks = blocks_from_content(content, conversation);
    if blocks.is_empty() {
        // A message record whose content holds no readable part is counted, so
        // "this reader could not read it" never becomes "the message was empty".
        conversation.unrendered_lines += 1;
        return;
    }
    conversation.push_message(Message {
        role,
        time: message_time(value.get("time")),
        blocks,
    });
}

#[cfg(test)]
mod tests {
    use super::super::{normalize, Provenance, Role};
    use serde_json::Value;
    use std::io::Write;

    /// One synthetic session in the archived shape: the header frame, then a
    /// human turn, an assistant turn that both writes text and calls a tool, the
    /// tool's result, and the lifecycle records around them.
    const BODY: &str = concat!(
        r#"{"type":"session","version":4,"id":"session-0000","createdAt":1,"cwd":"/tmp/w","isSeeded":false,"delegationDepth":0,"agentPreset":"standard"}"#,
        "\n",
        r#"{"type":"turn/start","seq":1,"time":1791032332791,"data":{"turn":1}}"#,
        "\n",
        r#"{"type":"user/message","seq":2,"time":1791032332798,"data":{"content":[{"type":"text","text":"hi"}],"source":{},"role":"user","id":"m1"}}"#,
        "\n",
        r#"{"type":"assistant/message","seq":3,"time":1791032339953,"data":{"turn":1,"step":1,"message":{"role":"assistant","content":[{"type":"text","text":"hello"},{"type":"tool_use","id":"c1","name":"read","arguments":{"path":"a"}}],"source":{},"id":"m2"},"usage":{"inputTokens":1,"outputTokens":2,"cacheReadTokens":0,"cacheWriteTokens":0,"totalTokens":3},"stream":[]}}"#,
        "\n",
        r#"{"type":"tool/call","seq":4,"time":1791032339954,"data":{"turn":1,"step":1,"callId":"c1","name":"read","arguments":{"path":"a"}}}"#,
        "\n",
        r#"{"type":"tool/result","seq":5,"time":1791032339980,"data":{"turn":1,"step":1,"message":{"role":"tool","source":{},"toolCallId":"c1","content":[{"type":"text","text":"file body"}],"isError":false,"id":"m3"}}}"#,
        "\n",
        r#"{"type":"step/end","seq":6,"time":1791032339981,"data":{"turn":1,"step":1}}"#,
    );

    #[test]
    fn a_dsh_session_reads_as_its_four_message_records() {
        let result = normalize("deepseek-harness", BODY);
        assert!(
            matches!(result.provenance, Provenance::Known { .. }),
            "the harness has a reader, so it is not RawOnly"
        );
        assert_eq!(result.messages.len(), 3, "user, assistant and the tool result");
        assert!(matches!(result.messages[0].role, Role::User));
        assert!(matches!(result.messages[1].role, Role::Assistant));
        assert!(matches!(result.messages[2].role, Role::Tool));
        assert_eq!(
            result.messages[1].blocks.len(),
            2,
            "the assistant turn carries its text and its tool call"
        );
        assert_eq!(
            result.unrendered_lines, 0,
            "every record in the body was classified"
        );
        assert_eq!(result.unrecognized_lines, 0);
    }

    /// The header, the lifecycle records and the mirrored `tool/call` are
    /// classified, not unreadable. Counting them would turn a healthy session
    /// into a page of "records this build could not use".
    #[test]
    fn classified_records_are_not_reported_as_unrendered() {
        let result = normalize("deepseek-harness", BODY);
        assert_eq!(
            result.unrendered_lines, 0,
            "session, turn/start, tool/call and step/end are all known"
        );
    }

    /// `dsh` is a developer preview that says compatibility-breaking changes are
    /// expected. A record type this build does not know must stay visible as a
    /// count — reading the session as complete would be the silent claim the
    /// archive exists to prevent.
    #[test]
    fn an_unknown_record_type_stays_counted() {
        // `BODY` is a `const`, and `concat!` only takes literals: the extra
        // record is appended rather than concatenated at compile time.
        let body = format!(
            "{BODY}\n{}",
            r#"{"type":"future/op","seq":7,"time":1791032339990,"data":{}}"#
        );
        let result = normalize("deepseek-harness", &body);
        assert_eq!(result.messages.len(), 3, "the known records still read");
        assert_eq!(
            result.unrendered_lines, 1,
            "one unrecognised record is one count, not zero"
        );
        assert_eq!(
            result.unrecognized_lines, 0,
            "the record is valid JSON; it is unknown, not unreadable"
        );
    }

    /// A message record whose content holds nothing readable is counted. It must
    /// not become a message with no blocks, and the run must not read as a
    /// complete session with three messages and no gap.
    #[test]
    fn a_message_record_with_no_readable_part_is_counted_not_dropped() {
        let body = concat!(
            r#"{"type":"user/message","seq":1,"time":1791032332798,"data":{"content":[],"role":"user","id":"m1"}}"#,
            "\n",
            r#"{"type":"assistant/message","seq":2,"time":1791032339953,"data":{"turn":1,"step":1,"message":{"role":"assistant","content":[{"type":"text"}],"id":"m2"}}}"#,
        );
        let result = normalize("deepseek-harness", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(
            result.unrendered_lines, 2,
            "an empty content list and a text part with no text are both gaps"
        );
    }

    /// A message record that names itself as one and does not carry `data` at
    /// all is drift, and drift is counted rather than read as an empty message.
    #[test]
    fn a_message_record_without_its_data_is_counted() {
        let body = r#"{"type":"user/message","seq":1,"time":1791032332798}"#;
        let result = normalize("deepseek-harness", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(result.unrendered_lines, 1);
    }

    /// The premise the DSH adapter rests on, pinned here because the reader above
    /// cannot state it: a `dsh` session file is a **concatenation** of Zstandard
    /// frames, and the decode the archive will use has to return every frame.
    ///
    /// A decoder that stops at the first frame is not hypothetical — Node's
    /// `zlib.zstdDecompressSync` does exactly that, and reading a 200 KB session
    /// with it yields one record. `zstd::stream::decode_all` must not.
    #[test]
    fn concatenated_zstd_frames_decode_as_one_stream() {
        let frames: Vec<Vec<u8>> = ["first", "second", "third"]
            .iter()
            .map(|line| {
                let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 0)
                    .expect("an encoder over a Vec cannot fail to construct");
                encoder
                    .write_all(format!("{line}\n").as_bytes())
                    .expect("writing into a Vec encoder cannot fail");
                encoder
                    .finish()
                    .expect("finishing a Vec encoder cannot fail")
            })
            .collect();
        let concatenated: Vec<u8> = frames.concat();
        let decoded = zstd::stream::decode_all(&concatenated[..])
            .expect("a concatenation of valid frames is a valid stream");
        assert_eq!(
            String::from_utf8_lossy(&decoded),
            "first\nsecond\nthird\n",
            "every frame must be decoded, not only the first"
        );
    }

    /// The measured record vocabulary, kept next to the reader so a `dsh` release
    /// that renames a type fails here rather than silently rendering less.
    #[test]
    fn the_measured_message_vocabulary_is_the_one_this_reader_dispatches() {
        for kind in [
            "user/message",
            "assistant/message",
            "system/message",
            "tool/result",
        ] {
            let record: Value = serde_json::from_str(&format!(
                r#"{{"type":"{kind}","seq":1,"time":1791032332798,"data":{{"turn":1,"step":1,"content":[{{"type":"text","text":"x"}}],"message":{{"content":[{{"type":"text","text":"x"}}]}}}}}}"#
            ))
            .expect("the synthetic record is JSON");
            let mut conversation = super::super::Conversation::new("deepseek-harness");
            super::record(&record, &mut conversation);
            assert_eq!(
                conversation.messages.len(),
                1,
                "`{kind}` is a message record this reader must read"
            );
        }
    }
}
