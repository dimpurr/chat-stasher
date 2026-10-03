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
//! `0.2.0-rc.2`, session format v4, 2026-10-03; 7 sessions on one machine, the
//! largest a live session that had reached 372 frames while still being
//! written, so a frame count here is a reading and not a ceiling) by decoding
//! every frame and recording **field names, counts and digests only** — never
//! message text:
//!
//! * `session` — the header frame: `{version, id, createdAt, cwd, isSeeded,
//!   delegationDepth, agentPreset, parentSession?, origin?}`. It is metadata,
//!   not a message: it is recognised here and carries nothing to render.
//! * `user/message` — `data = {content[], source, role, id}`, where
//!   `data.source.kind` is the origin. Measured on this machine across every
//!   `user/message` record: `user` 11, `agent-instructions` 8,
//!   `runtime-context` 7, `skill-catalog` 7, `subagent-settled` 6,
//!   `agent-message` 2 — so only 11 of 41 are the person's words, and the
//!   other 30 are `dsh` injecting its own context into the transcript. `user`
//!   renders as `User`; the five measured machine kinds stay visible as
//!   `System` rather than being shown as if the human had typed them; any
//!   other kind, or a record with no `source.kind`, is *unknown* and is
//!   counted unrendered rather than guessed either way — the same rule
//!   `kimi_code.rs` applies to `origin.kind`.
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
//! * **packed chunk rows** — `text-chunks`, `reasoning-chunks` and
//!   `tool-call-chunks`, shape `{type, seq0, time0, dt[], texts[]}` where
//!   member `k` has `seq = seq0 + k`. These are what upstream
//!   `session-persistence-jsonl` writes when `packChunks` is on (documented
//!   default: true): a run of three or more consecutive assistant deltas
//!   becomes one row instead of one record per delta, so a reader that knows
//!   only the event-type table loses most assistant text, reasoning and
//!   tool-argument fragments. **These three types were observed zero times on
//!   this machine**: no packed row and no `assistant/chunk` record appears in
//!   its archived sessions. The shape and the `seq0 + k` member rule
//!   therefore come from upstream's documentation and a third-party
//!   re-implementation, not from a local observation, and the mapping below
//!   is deliberately conservative for exactly that reason: `text-chunks` and
//!   `reasoning-chunks` render only when `texts` is an array of strings, and
//!   `tool-call-chunks` is never rendered at all — what its members mean is
//!   unmeasured here, so rendering them would fabricate tool calls that the
//!   session may not contain. It is counted instead, as is any of the three
//!   whose `texts` is missing or not an array of strings. The row's payload is
//!   read from the record's own `texts` and, failing that, from `data.texts`:
//!   upstream documents it beside `seq0`/`time0`, every other record in this
//!   build keeps its payload under `data`, and with neither shape observed
//!   here this reader accepts both rather than assume one.
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

use super::{blocks_from_content, message_time, Block, Conversation, Message, Role};
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

/// The measured `data.source.kind` values that mark a `user/message` as the
/// harness injecting its own context rather than the person speaking. A kind
/// that is neither `user` nor on this list is *unknown*, and an unknown origin
/// is counted unrendered rather than guessed either way — the same rule
/// `kimi_code.rs` applies to `origin.kind`.
const MACHINE_SOURCE_KINDS: &[&str] = &[
    "agent-instructions",
    "runtime-context",
    "skill-catalog",
    "subagent-settled",
    "agent-message",
];

/// The packed chunk row types. They share one shape, so they share one arm;
/// `tool-call-chunks` is counted inside [`push_chunk`] rather than rendered.
const PACKED_CHUNK_TYPES: &[&str] = &["text-chunks", "reasoning-chunks", "tool-call-chunks"];

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
        "user/message" => {
            let Some(role) = user_message_role(data) else {
                // The origin kind is neither the human nor a measured machine
                // kind, so who spoke is undetermined; the record is counted and
                // stays reachable in the raw shards.
                conversation.unrendered_lines += 1;
                return;
            };
            (role, data.and_then(|data| data.get("content")))
        }
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
        // A packed chunk row carries no content part array: its text is the
        // `texts` member list, so it is built and pushed here rather than
        // routed through the content path below.
        chunk if PACKED_CHUNK_TYPES.contains(&chunk) => {
            push_chunk(chunk, value, conversation);
            return;
        }
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

/// The role of one `user/message`, decided by the measured `source.kind`
/// vocabulary. `None` means the origin is unknown, which the caller counts.
fn user_message_role(data: Option<&Value>) -> Option<Role> {
    match data
        .and_then(|data| data.get("source"))
        .and_then(|source| source.get("kind"))
        .and_then(Value::as_str)
    {
        Some("user") => Some(Role::User),
        Some(kind) if MACHINE_SOURCE_KINDS.contains(&kind) => Some(Role::System),
        _ => None,
    }
}

/// One packed chunk row, when `packChunks` grouped a run of assistant deltas
/// into one record. The members are read only in the shape upstream documents
/// — `texts` an array of strings — and each way of failing that shape is
/// counted instead of guessed; see the module docs for why this is deliberately
/// narrow and why `tool-call-chunks` renders nothing at all.
fn push_chunk(kind: &str, value: &Value, conversation: &mut Conversation) {
    if kind == "tool-call-chunks" {
        // Not rendered because the members are unmeasured here. Counting keeps
        // the text this reader declined to interpret visible; inventing a
        // `ToolCall` block from it would claim a call the session may not hold.
        conversation.unrendered_lines += 1;
        return;
    }
    let Some(text) = joined_member_text(value) else {
        conversation.unrendered_lines += 1;
        return;
    };
    let block = if kind == "reasoning-chunks" {
        Block::Thinking(text)
    } else {
        Block::Text(text)
    };
    conversation.push_message(Message {
        role: Role::Assistant,
        // A packed row stamps the batch, `time0` being the first member's
        // clock; the envelope's own `time` wins where a writer kept it.
        time: message_time(value.get("time").or_else(|| value.get("time0"))),
        blocks: vec![block],
    });
}

/// The `texts` members of one packed chunk row, joined into one string, or
/// `None` when the row does not carry an array of strings (or carries no
/// non-empty text at all).
///
/// The join has **no separator**: the members are consecutive deltas of one
/// stream, so concatenation is what reconstructs the text, and any separator
/// would insert a character the assistant never emitted.
fn joined_member_text(value: &Value) -> Option<String> {
    let texts = value
        .get("texts")
        .or_else(|| value.get("data").and_then(|data| data.get("texts")))?
        .as_array()?;
    let mut joined = String::new();
    for member in texts {
        joined.push_str(member.as_str()?);
    }
    // An empty `texts`, or members that are all empty strings, hold nothing to
    // render: counted by the caller, never pushed as an empty message.
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{normalize, Block, Provenance, Role};
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
        r#"{"type":"user/message","seq":2,"time":1791032332798,"data":{"content":[{"type":"text","text":"hi"}],"source":{"kind":"user"},"role":"user","id":"m1"}}"#,
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
        assert_eq!(
            result.messages.len(),
            3,
            "user, assistant and the tool result"
        );
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

    /// The harness writes its own context into `user/message` records: measured
    /// on this machine, 30 of 41 such records were injections. A skill
    /// catalogue is the sharpest case — rendered as `User` it reads as
    /// something the person typed — so it stays visible as `System`.
    #[test]
    fn a_machine_sourced_user_message_renders_as_system_not_user() {
        // `r##…##` because the fixture text itself contains a `"#` sequence.
        let body = r##"{"type":"user/message","seq":1,"time":1791032332798,"data":{"content":[{"type":"text","text":"# Skills"}],"source":{"kind":"skill-catalog"},"role":"user","id":"m1"}}"##;
        let result = normalize("deepseek-harness", body);
        assert_eq!(
            result.messages.len(),
            1,
            "the injected context stays visible, not dropped"
        );
        assert_eq!(result.messages[0].role, Role::System);
        assert_ne!(
            result.messages[0].role,
            Role::User,
            "a skill catalogue must never be attributed to the human"
        );
        assert_eq!(
            result.unrendered_lines, 0,
            "a measured source kind is classified, not a gap"
        );
    }

    /// The measured human origin still renders as the person's turn.
    #[test]
    fn a_human_sourced_user_message_renders_as_user() {
        let body = r#"{"type":"user/message","seq":1,"time":1791032332798,"data":{"content":[{"type":"text","text":"hi"}],"source":{"kind":"user"},"role":"user","id":"m1"}}"#;
        let result = normalize("deepseek-harness", body);
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].role, Role::User);
        assert_eq!(result.unrendered_lines, 0);
    }

    /// A source kind this build does not know, and a record with no
    /// `source.kind` at all, are *unknown* rather than machine or human: both
    /// are counted and neither is rendered, so an unknown origin never becomes
    /// a guess about who spoke.
    #[test]
    fn an_unrecognised_user_message_source_is_counted_not_guessed() {
        let body = concat!(
            r#"{"type":"user/message","seq":1,"time":1791032332798,"data":{"content":[{"type":"text","text":"?"}],"source":{"kind":"future-origin"},"role":"user","id":"m1"}}"#,
            "\n",
            r#"{"type":"user/message","seq":2,"time":1791032332799,"data":{"content":[{"type":"text","text":"?"}],"role":"user","id":"m2"}}"#,
        );
        let result = normalize("deepseek-harness", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(result.unrendered_lines, 2);
        assert_eq!(
            result.unrecognized_lines, 0,
            "both records are valid JSON; they are unknown, not unreadable"
        );
    }

    /// Packed chunk rows: `text-chunks` and `reasoning-chunks` carry their
    /// members in `texts`, and the members are joined with no separator because
    /// they are consecutive deltas of one stream. `tool-call-chunks` is counted
    /// and rendered as nothing — see the module docs for why.
    #[test]
    fn packed_chunk_rows_render_their_text_and_count_tool_call_chunks() {
        let body = concat!(
            r#"{"type":"text-chunks","seq0":1,"time0":1791032332798,"dt":[1,1],"texts":["Hel","lo"]}"#,
            "\n",
            r#"{"type":"reasoning-chunks","seq0":3,"time0":1791032332799,"dt":[1],"texts":["weighing"]}"#,
            "\n",
            r#"{"type":"tool-call-chunks","seq0":4,"time0":1791032332800,"dt":[1],"texts":["{\"path\":\"/tmp/a\"}"]}"#,
            "\n",
            r#"{"type":"text-chunks","seq0":5,"time0":1791032332801,"dt":[1],"data":{"texts":["under data"]}}"#,
        );
        let result = normalize("deepseek-harness", body);
        assert_eq!(
            result.messages.len(),
            3,
            "the tool-call row renders nothing at all"
        );
        assert_eq!(result.messages[0].role, Role::Assistant);
        assert_eq!(
            result.messages[0].blocks,
            vec![Block::Text("Hello".to_string())],
            "the members are one stream, joined with no separator"
        );
        assert_eq!(
            result.messages[1].blocks,
            vec![Block::Thinking("weighing".to_string())]
        );
        assert_eq!(
            result.messages[2].blocks,
            vec![Block::Text("under data".to_string())],
            "the payload is also read from `data.texts`, the envelope's own place"
        );
        assert_eq!(
            result.unrendered_lines, 1,
            "the unmeasured tool-call chunks stay counted"
        );
    }

    /// The documented member shape is the only one that renders: `texts`
    /// missing, not an array, holding a non-string member, or empty is counted
    /// rather than read, so a row this reader cannot interpret never becomes a
    /// message that looks authored.
    #[test]
    fn a_packed_chunk_row_without_readable_members_is_counted() {
        let body = concat!(
            r#"{"type":"text-chunks","seq0":1,"time0":1791032332798,"dt":[1]}"#,
            "\n",
            r#"{"type":"text-chunks","seq0":2,"time0":1791032332799,"dt":[1],"texts":"a string, not a list"}"#,
            "\n",
            r#"{"type":"text-chunks","seq0":3,"time0":1791032332800,"dt":[1],"texts":["ok",7]}"#,
            "\n",
            r#"{"type":"text-chunks","seq0":4,"time0":1791032332801,"dt":[1],"texts":[]}"#,
        );
        let result = normalize("deepseek-harness", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(result.unrendered_lines, 4);
        assert_eq!(result.unrecognized_lines, 0);
    }

    /// A message record whose content holds nothing readable is counted. It must
    /// not become a message with no blocks, and the run must not read as a
    /// complete session with three messages and no gap.
    #[test]
    fn a_message_record_with_no_readable_part_is_counted_not_dropped() {
        let body = concat!(
            r#"{"type":"user/message","seq":1,"time":1791032332798,"data":{"content":[],"source":{"kind":"user"},"role":"user","id":"m1"}}"#,
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
                r#"{{"type":"{kind}","seq":1,"time":1791032332798,"data":{{"turn":1,"step":1,"source":{{"kind":"user"}},"content":[{{"type":"text","text":"x"}}],"message":{{"content":[{{"type":"text","text":"x"}}]}}}}}}"#
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
