//! kimi-code — reader for one archived Kimi Code wire journal.
//!
//! The session body is the agent journal at
//! `<sessionDir>/agents/main/wire.jsonl`: one `{type, time, …}` record per
//! line, `time` being the writer's `Date.now()` (epoch **millis**, so
//! `message_time` labels it inferred — never exact). This is an event
//! journal, not a message log: the shapes below were read off the installed
//! Kimi Code 0.39.1 (`<KIMI_CODE_HOME>/bin/kimi`, 2026-09-25) and
//! corroborated across 41 real journals by an independent importer
//! (MemPalace issue #2180,
//! <https://github.com/MemPalace/mempalace/issues/2180>, open 2026-09-25):
//!
//! * `context.append_message` — `message = {role, content[], origin?}`.
//!   Measured: every `role: user` record is either the human's words or a
//!   machine injection, told apart by `origin.kind` (the binary's
//!   `PromptOrigin` vocabulary: `user`, `skill_activation`,
//!   `plugin_command`, `injection`, `shell_command`, `compaction_summary`,
//!   `system_trigger`, `background_task`, `cron_job`, `cron_missed`,
//!   `hook_result`, `retry`). Human kinds render as `User`, the machine
//!   kinds stay visible as `System` — folded away like the claude-code
//!   extractor folds `isMeta` lines, never dropped, because a record this
//!   reader does not understand must not become "nothing here".
//! * `context.append_loop_event` — the assistant side of the conversation.
//!   Kimi's own replay groups a step's parts into one assistant message
//!   (`step.begin {uuid}` opens one, `content.part {stepUuid, part}` appends
//!   to it, `tool.call {stepUuid, toolCallId, name, args}` is the same
//!   message's tool traffic); this reader mirrors that grouping. Parts:
//!   `text {text}`, `think {think}` (reasoning; an `encrypted` reasoning
//!   record without readable text is counted, not decrypted),
//!   `image_url/audio_url/video_url` (attachment references — the media
//!   itself was offloaded by the harness, so only the recorded type is
//!   claimed). `tool.result {toolCallId, result {output, isError, note}}`
//!   becomes its own Tool message, exactly where kimi's own history puts it.
//! * `turn.prompt` / `turn.steer` — the turn lifecycle, carrying the same
//!   input the following `context.append_message` records (the binary logs
//!   `turn.prompt {input}` before the message is appended). Rendering both
//!   would show each human turn twice, so — like codex's `event_msg` mirror
//!   — they are counted as not rendered.
//! * anything else (config, tool discovery, usage, token counting, unknown
//!   ops) — one record, one count. An op added by a future release is
//!   undetermined, not metadata; counting it keeps the gap visible instead
//!   of silently calling the session complete.

use std::collections::HashMap;

use super::{
    compact_json, message_time, AttachmentRef, Block, Conversation, Message, MessageTime, Role,
};
use serde_json::Value;

/// The `origin.kind` values the installed implementation classifies as
/// machine-authored user-role records. An origin kind that is neither `user`
/// nor on this list is *unknown*, and an unknown origin is counted unrendered
/// rather than guessed either way.
const MACHINE_ORIGINS: &[&str] = &[
    "injection",
    "shell_command",
    "system_trigger",
    "background_task",
    "skill_activation",
    "plugin_command",
    "compaction_summary",
    "cron_job",
    "cron_missed",
    "hook_result",
    "retry",
];

/// `skill_activation` and `plugin_command` are the two origins that are the
/// human's words when they were invoked from the prompt line.
const USER_SLASH_ORIGINS: &[&str] = &["skill_activation", "plugin_command"];

pub(super) fn normalize_body(body: &str, conversation: &mut Conversation) {
    let mut steps = Steps::default();
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => record(&value, conversation, &mut steps),
            Err(_) => conversation.unrecognized_lines += 1,
        }
    }
}

/// One record, without the per-body step state. `normalize` routes
/// `kimi-code` through [`normalize_body`], which keeps the state that folds a
/// step's parts into one message; this is the per-line dispatch's equivalent
/// fallback, where every part renders as its own message rather than under an
/// order the single record's stateless view cannot know.
pub(super) fn record_or_count(value: &Value, conversation: &mut Conversation) {
    let mut steps = Steps::default();
    record(value, conversation, &mut steps);
}

fn record(value: &Value, conversation: &mut Conversation, steps: &mut Steps) {
    let time = message_time(value.get("time"));
    let kind = value.get("type").and_then(Value::as_str);
    match kind {
        Some("context.append_message") => append_message(value, time, conversation),
        Some("context.append_loop_event") => loop_event(value, time, conversation, steps),
        Some("turn.prompt") | Some("turn.steer") => {
            // The input mirror; see the module docs. Counted, not rendered.
            conversation.unrendered_lines += 1;
        }
        _ => conversation.unrendered_lines += 1,
    }
}

fn append_message(value: &Value, time: MessageTime, conversation: &mut Conversation) {
    let Some(message) = value.get("message") else {
        conversation.unrendered_lines += 1;
        return;
    };
    let Some(role) = message_role(message) else {
        // An origin kind we cannot classify is not a claim we can make about
        // who spoke; the record is counted and stays reachable in the raw
        // shards.
        conversation.unrendered_lines += 1;
        return;
    };
    let blocks = message_blocks(message.get("content"), conversation);
    if blocks.is_empty() {
        conversation.unrendered_lines += 1;
        return;
    }
    conversation.push_message(Message { role, time, blocks });
}

/// The role of one `context.append_message`, with the measured origin kinds
/// deciding whether a user-role record is the human or the harness.
fn message_role(message: &Value) -> Option<Role> {
    match message.get("role").and_then(Value::as_str) {
        Some("user") => {
            let origin = message.get("origin");
            let kind = origin
                .and_then(|origin| origin.get("kind"))
                .and_then(Value::as_str);
            let user_slash = || {
                origin
                    .and_then(|origin| origin.get("trigger"))
                    .and_then(Value::as_str)
                    == Some("user-slash")
            };
            // No origin recorded: kimi's journal appends `origin` on every
            // user record it writes itself, and its compaction projector
            // keeps origin-less user records ("origin === void 0 → keep"),
            // so an absent origin is the human's record, not a machine one.
            match kind {
                None | Some("user") => Some(Role::User),
                Some(kind) if USER_SLASH_ORIGINS.contains(&kind) && user_slash() => {
                    Some(Role::User)
                }
                Some(kind) if MACHINE_ORIGINS.contains(&kind) => Some(Role::System),
                _ => None,
            }
        }
        Some(role) => Role::from_str(role),
        None => None,
    }
}

fn loop_event(
    value: &Value,
    time: MessageTime,
    conversation: &mut Conversation,
    steps: &mut Steps,
) {
    let Some(event) = value.get("event") else {
        conversation.unrendered_lines += 1;
        return;
    };
    match event.get("type").and_then(Value::as_str) {
        Some("content.part") => {
            let Some(block) = content_part(event.get("part"), conversation) else {
                conversation.unrendered_lines += 1;
                return;
            };
            let index = steps.open_assistant(event.get("stepUuid"), time, conversation);
            conversation.messages[index].blocks.push(block);
        }
        Some("tool.call") => {
            // The call belongs to its step's assistant message, exactly as
            // kimi's own open-step history keeps tool calls on the step they
            // were issued in.
            let block = Block::ToolCall {
                name: event
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                input_summary: compact_json(event.get("args").or_else(|| event.get("input"))),
                // The result arrives as its own `tool.result` event.
                output_bytes: None,
            };
            let index = steps.open_assistant(event.get("stepUuid"), time, conversation);
            conversation.messages[index].blocks.push(block);
        }
        Some("tool.result") => {
            let output = event.pointer("/result/output");
            conversation.push_message(Message {
                role: Role::Tool,
                time,
                blocks: vec![Block::ToolCall {
                    name: event
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    input_summary: "tool result".to_string(),
                    output_bytes: output.map(|output| compact_json(Some(output)).len()),
                }],
            });
        }
        // `step.begin` and `step.end` are the loop's own bookkeeping (they
        // carry a uuid and usage, no content); every other event type is a
        // record this reader has no renderer for. One record, one count.
        _ => conversation.unrendered_lines += 1,
    }
}

/// One Kimi content part as a block, or `None` when it holds nothing
/// readable. Media parts become attachment references: the journal keeps
/// the media off-machine-file in the session blob store, so the reader
/// claims the recorded type and nothing else.
fn content_part(part: Option<&Value>, conversation: &mut Conversation) -> Option<Block> {
    let part = part?;
    let kind = part.get("type").and_then(Value::as_str)?;
    let block = match kind {
        "text" => part
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| Block::Text(text.to_string())),
        "think" => part
            .get("think")
            .and_then(Value::as_str)
            .filter(|think| !think.is_empty())
            .map(|think| Block::Thinking(think.to_string())),
        "image_url" | "audio_url" | "video_url" => {
            let attachment = AttachmentRef {
                name: None,
                media_type: Some(kind.to_string()),
                bytes: None,
            };
            conversation.attachments.push(attachment.clone());
            Some(Block::AttachmentRef(attachment))
        }
        _ => None,
    };
    block
}

/// The blocks of one `context.append_message`. Content is a part array in
/// the measured shape; a content this reader cannot turn into a block is
/// counted, and the parent's generic reader covers the non-array shapes kimi
/// never writes itself but a resume replay could have left.
fn message_blocks(content: Option<&Value>, conversation: &mut Conversation) -> Vec<Block> {
    let Some(content) = content else {
        return Vec::new();
    };
    match content {
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match content_part(Some(part), conversation) {
                Some(block) => Some(block),
                None => {
                    conversation.unrendered_lines += 1;
                    None
                }
            })
            .collect(),
        other => super::blocks_from_content(other, conversation),
    }
}

/// The open-step messages of one journal, mirroring kimi's own `ContextMemory`
/// `openSteps`: parts of one step land in one assistant message, in order.
#[derive(Default)]
struct Steps {
    open: HashMap<String, usize>,
}

impl Steps {
    fn open_assistant(
        &mut self,
        step_uuid: Option<&Value>,
        time: MessageTime,
        conversation: &mut Conversation,
    ) -> usize {
        let step_uuid = step_uuid.and_then(Value::as_str).map(str::to_string);
        // A part with no step to group by is its own assistant message:
        // rendering it inside some other step's message would invent an
        // order the journal does not record.
        let Some(step_uuid) = step_uuid else {
            conversation.push_message(Message {
                role: Role::Assistant,
                time,
                blocks: Vec::new(),
            });
            return conversation.messages.len() - 1;
        };
        if let Some(index) = self.open.get(&step_uuid) {
            return *index;
        }
        conversation.push_message(Message {
            role: Role::Assistant,
            time,
            blocks: Vec::new(),
        });
        let index = conversation.messages.len() - 1;
        self.open.insert(step_uuid, index);
        index
    }
}

#[cfg(test)]
mod tests {
    use super::super::{normalize, Block, Role};

    /// A user turn, a step of assistant output (thinking + reply + one tool
    /// call), the tool's result, and the turn-lifecycle mirrors. Every record
    /// that is not rendered is counted, and the human's input appears once.
    #[test]
    fn a_kimi_code_journal_renders_once_per_record_that_carries_content() {
        let body = concat!(
            r#"{"type":"metadata","protocol_version":"1.4","created_at":1770000000000}"#,
            "\n",
            r#"{"type":"turn.prompt","time":1770000000000,"input":[{"type":"text","text":"question"}],"origin":{"kind":"user"}}"#,
            "\n",
            r#"{"type":"config.update","time":1770000000000}"#,
            "\n",
            r#"{"type":"context.append_message","time":1770000000000,"message":{"role":"user","origin":{"kind":"user"},"content":[{"type":"text","text":"question"}]}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1770000000100,"event":{"type":"step.begin","uuid":"step-1"}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1770000000200,"event":{"type":"content.part","stepUuid":"step-1","part":{"type":"think","think":"weighing"}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1770000000300,"event":{"type":"content.part","stepUuid":"step-1","part":{"type":"text","text":"answer with `code`"}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1770000000400,"event":{"type":"tool.call","stepUuid":"step-1","toolCallId":"c1","name":"bash","args":{"cmd":"ls"}}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1770000000500,"event":{"type":"step.end","uuid":"step-1"}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1770000000600,"event":{"type":"tool.result","toolCallId":"c1","result":{"output":"two entries","isError":false}}}"#,
            "\n",
        );
        let result = normalize("kimi-code", body);
        // user message + the one assistant step message (think + text + the
        // call) + the tool result: three messages, not one per part.
        assert_eq!(result.messages.len(), 3);
        assert!(matches!(result.messages[0].role, Role::User));
        assert!(matches!(result.messages[1].role, Role::Assistant));
        assert!(matches!(result.messages[1].blocks[0], Block::Thinking(_)));
        assert!(matches!(
            result.messages[1].blocks[1],
            Block::Text(ref text) if text.contains("code")
        ));
        assert!(matches!(
            result.messages[1].blocks[2],
            Block::ToolCall { .. }
        ));
        assert!(matches!(result.messages[2].role, Role::Tool));
        assert!(matches!(
            result.messages[2].blocks[0],
            Block::ToolCall {
                output_bytes: Some(11),
                ..
            }
        ));
        // metadata, turn.prompt (mirror), config.update, step.begin,
        // step.end — every non-rendered record counted, nothing hidden.
        assert_eq!(result.unrendered_lines, 5);
        assert_eq!(result.unrecognized_lines, 0);
        // The journal stamps Date.now() millis; the reader labels the
        // inference instead of claiming an exact time.
        assert!(matches!(
            result.messages[0].time,
            super::super::MessageTime::Known {
                source: crate::activity::TimeSource::Inferred { .. },
                ..
            }
        ));
    }

    /// Machine injections are user-role records the harness wrote, measured on
    /// real journals (~58% of `context.append_message` rows): they stay in
    /// the conversation as `System`, never silently dropped and never called
    /// the human's words. An origin kind this build does not know is counted.
    #[test]
    fn machine_origins_render_as_system_and_unknown_origins_are_counted() {
        let body = concat!(
            r#"{"type":"context.append_message","time":1770000000000,"message":{"role":"user","origin":{"kind":"user"},"content":[{"type":"text","text":"human words"}]}}"#,
            "\n",
            r#"{"type":"context.append_message","time":1770000000100,"message":{"role":"user","origin":{"kind":"injection","variant":"local-command-stdout"},"content":[{"type":"text","text":"<local-command-stdout></local-command-stdout>"}]}}"#,
            "\n",
            r#"{"type":"context.append_message","time":1770000000200,"message":{"role":"user","origin":{"kind":"skill_activation","trigger":"user-slash"},"content":[{"type":"text","text":"/slash skill words"}]}}"#,
            "\n",
            r#"{"type":"context.append_message","time":1770000000300,"message":{"role":"user","origin":{"kind":"future_kind"},"content":[{"type":"text","text":"?"}]}}"#,
        );
        let result = normalize("kimi-code", body);
        assert_eq!(result.messages.len(), 3);
        assert!(matches!(result.messages[0].role, Role::User));
        assert!(matches!(result.messages[1].role, Role::System));
        assert!(matches!(result.messages[2].role, Role::User));
        assert_eq!(
            result.unrendered_lines, 1,
            "the unknown origin kind is counted, not guessed"
        );
    }

    /// A journal of nothing but bookkeeping is not an empty conversation and
    /// not an error: this is the composition activity-index measured on this
    /// machine, and every record is accounted for.
    #[test]
    fn a_bookkeeping_only_journal_counts_every_record() {
        let body = concat!(
            r#"{"type":"metadata","protocol_version":"1.4","created_at":1770000000000}"#,
            "\n",
            r#"{"type":"config.update","time":1770000000000}"#,
            "\n",
            r#"{"type":"mcp.tools_discovered","time":1770000000164}"#,
            "\n",
        );
        let result = normalize("kimi-code", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(result.unrendered_lines, 3);
        assert_eq!(result.unrecognized_lines, 0);
    }

    /// `context.append_loop_event` events this build has no renderer for
    /// (here: a future event type) are counted, never silently skipped — an
    /// unknown op must not read as "nothing here".
    #[test]
    fn an_unknown_loop_event_is_counted_not_dropped() {
        let body = r#"{"type":"context.append_loop_event","time":1770000000000,"event":{"type":"future.event","data":{}}}"#;
        let result = normalize("kimi-code", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(result.unrendered_lines, 1);
    }

    /// An assistant step that never got a part (interrupted session, step
    /// opened and closed with no output) must not leave an empty message: it
    /// was a step, not a turn anyone can read.
    #[test]
    fn an_outputless_step_leaves_no_empty_message() {
        let body = concat!(
            r#"{"type":"context.append_loop_event","time":1770000000000,"event":{"type":"step.begin","uuid":"s"}}"#,
            "\n",
            r#"{"type":"context.append_loop_event","time":1770000000100,"event":{"type":"step.end","uuid":"s"}}"#,
        );
        let result = normalize("kimi-code", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(result.unrendered_lines, 2);
    }
}
