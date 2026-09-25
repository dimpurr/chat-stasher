//! Conversation normalization for the local reader.
//!
//! The archive keeps source-shaped JSONL as its raw truth. This module adds a
//! small, deliberately lossy view for people: message roles, Markdown text,
//! code, thinking, tool calls, and attachment references. Anything it cannot
//! classify remains counted and the raw route stays available.

use crate::activity::TimeSource;
use serde_json::Value;

pub const DEFAULT_WINDOW: usize = 50;
pub const MAX_WINDOW: usize = 200;

/// How much of one message's source text the reader will render before it
/// shows the head and the tail and counts the rest. A single message carrying
/// a pasted file must not be able to spend a whole page's budget, and the
/// bytes that were dropped have to be a number rather than a silence.
pub const MESSAGE_BUDGET: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

impl Role {
    pub fn label(&self) -> &'static str {
        match self {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => "System",
            Role::Tool => "Tool",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "user" | "human" => Some(Self::User),
            "assistant" | "model" | "gemini" => Some(Self::Assistant),
            "system" => Some(Self::System),
            "tool" | "function" => Some(Self::Tool),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageTime {
    Known { unix: i64, source: TimeSource },
    Unknown { why: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRef {
    pub name: Option<String>,
    pub media_type: Option<String>,
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Text(String),
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    Thinking(String),
    ToolCall {
        name: Option<String>,
        input_summary: String,
        /// `None` is a record that does not carry the output — a `tool_use`
        /// names the call, the `tool_result` that follows carries what came
        /// back. It is not zero bytes, and the reader does not print it as
        /// one: "not recorded" and "recorded as empty" are different claims.
        output_bytes: Option<usize>,
    },
    AttachmentRef(AttachmentRef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub time: MessageTime,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    Known { source: String },
    Unknown { why: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub messages: Vec<Message>,
    pub message_total: usize,
    pub unrendered_lines: usize,
    pub unrecognized_lines: usize,
    pub branch_nodes: usize,
    pub canonical_follows_active: bool,
    pub attachments: Vec<AttachmentRef>,
    pub provenance: Provenance,
}

impl Conversation {
    fn new(harness: &str) -> Self {
        let provenance = if harness.is_empty() {
            Provenance::Unknown {
                why: "the archived row did not identify a harness".to_string(),
            }
        } else {
            Provenance::Known {
                source: harness.to_string(),
            }
        };
        Self {
            messages: Vec::new(),
            message_total: 0,
            unrendered_lines: 0,
            unrecognized_lines: 0,
            branch_nodes: 0,
            canonical_follows_active: true,
            attachments: Vec::new(),
            provenance,
        }
    }

    fn push_message(&mut self, message: Message) {
        self.message_total += 1;
        self.messages.push(message);
    }
}

/// Normalize the concatenated archive body. The body remains untouched; the
/// returned model is only a reader view and counts every line it cannot use.
pub fn normalize(harness: &str, body: &str) -> Conversation {
    let mut conversation = Conversation::new(harness);
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
        normalize_value(harness, &value, &mut conversation);
    }
    // `gemini-cli` does not archive JSONL: its body is pretty-printed JSON
    // documents run together, so none of its lines parse on their own and
    // every one of them is counted as unreadable. That count would be a claim
    // about the body when it is really a claim about the framing we chose, so
    // the body is read as the stream of values it is. The stream reader
    // accepts whitespace between values, which covers one document, several
    // concatenated documents, and JSONL alike. `activity::analyze_session`
    // special-cases the same harness for the same reason.
    //
    // Only a body that yielded no message at all is re-read, so no bytes that
    // already became a message are read twice.
    if harness == "gemini-cli"
        && conversation.messages.is_empty()
        && conversation.unrecognized_lines > 0
    {
        let line_unrendered = conversation.unrendered_lines;
        let line_unrecognized = conversation.unrecognized_lines;
        let documents = serde_json::Deserializer::from_str(body)
            .into_iter::<Value>()
            .collect::<Result<Vec<_>, _>>();
        match documents {
            Ok(documents) => {
                // The framing is solved, so the line pass's counts described
                // the framing we chose rather than the body, and keeping them
                // would report our own reading as the body's defect — "52
                // lines were not valid JSON" about a body that is valid JSON.
                // What replaces them is the documents' own accounting:
                // `normalize_gemini_cli` counts a document that holds no
                // message, and each record inside one that it cannot read.
                conversation.unrendered_lines = 0;
                conversation.unrecognized_lines = 0;
                for document in &documents {
                    normalize_gemini_cli(document, &mut conversation);
                }
            }
            Err(_) => {
                // Neither framing reads this body. The line counts are then the
                // only measurement there is, and a clean zero would turn
                // "unreadable" into "empty".
                conversation.unrendered_lines = line_unrendered;
                conversation.unrecognized_lines = line_unrecognized;
            }
        }
    }
    conversation
}

fn normalize_value(harness: &str, value: &Value, conversation: &mut Conversation) {
    match harness {
        "claude-code" | "kimi-code" => normalize_cli_line(harness, value, conversation),
        "codex" => normalize_codex_line(value, conversation),
        "gemini-cli" => normalize_gemini_cli(value, conversation),
        "chatgpt" => normalize_chatgpt(value, conversation),
        "claude" => normalize_claude_web(value, conversation),
        "deepseek" => normalize_deepseek(value, conversation),
        // `opencode` and `cursor` were routed to the codex extractor, which is
        // a claim that they are recorded in codex's shape. They are not: both
        // archive a session *summary* document, and `opencode`'s carries a
        // `messages` array. The generic reader is the design's fallback for a
        // harness with no extractor of its own, so they go there.
        _ => normalize_generic(value, conversation),
    }
}

fn normalize_cli_line(harness: &str, value: &Value, conversation: &mut Conversation) {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        conversation.unrendered_lines += 1;
        return;
    };
    if kind != "user" && kind != "assistant" {
        conversation.unrendered_lines += 1;
        return;
    }
    let line_time = message_time(value.get("timestamp"));
    let Some(message) = value.get("message") else {
        conversation.unrendered_lines += 1;
        return;
    };
    let content = message.get("content").unwrap_or(message);
    let blocks = blocks_from_content(content, conversation);
    if blocks.is_empty() {
        conversation.unrendered_lines += 1;
        return;
    }
    // Tool traffic is normally returned under the turn that asked for it: a
    // `user` line whose whole content is a `tool_result` is the harness handing
    // back output, not the person speaking, so it is labelled `Tool`. That
    // relabelling is only sound when *every* block is tool traffic. A turn that
    // also carries prose, thinking or code keeps the role it was recorded with
    // — calling a mixed turn `Tool` would erase who wrote the visible text.
    let role = if blocks
        .iter()
        .all(|block| matches!(block, Block::ToolCall { .. }))
    {
        Role::Tool
    } else if kind == "user" {
        Role::User
    } else {
        Role::Assistant
    };
    let mut blocks = blocks;
    if value.get("isMeta").and_then(Value::as_bool) == Some(true)
        || value.get("sidechain").and_then(Value::as_bool) == Some(true)
    {
        let text = blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) | Block::Thinking(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        blocks = vec![Block::Thinking(if text.is_empty() {
            format!("{harness} metadata or sidechain record")
        } else {
            text
        })];
    }
    conversation.push_message(Message {
        role,
        time: line_time,
        blocks,
    });
}

/// One archived codex line.
///
/// The archived body is `{"timestamp":…, "type":…, "payload":{…}}` and it is
/// `payload.type` that names the record. `response_item` holds the transcript
/// the model saw — `message`, `reasoning`, `function_call`,
/// `function_call_output` — and is what this renders. `event_msg` is the same
/// turn recorded a second time at the application level (`user_message`,
/// `agent_message`, `agent_reasoning`, `token_count`), so rendering it too
/// would show every turn twice; it is counted as not rendered, along with
/// `session_meta` and `turn_context`, which are session metadata rather than
/// conversation.
///
/// The minimal `{"payload":{"message":…}}` shape is kept because
/// `activity::analyze_session`'s fixture uses it (`activity.rs:1717`).
fn normalize_codex_line(value: &Value, conversation: &mut Conversation) {
    let time = message_time(value.get("timestamp"));
    if let Some(message) = value.pointer("/payload/message") {
        if let Some(message) = message_from_value(message, time, conversation) {
            conversation.push_message(message);
        } else {
            conversation.unrendered_lines += 1;
        }
        return;
    }
    let kind = value.get("type").and_then(Value::as_str);
    if kind == Some("response_item") {
        if let Some(item) = codex_item(value.get("payload"), time, conversation) {
            conversation.push_message(item);
        } else {
            conversation.unrendered_lines += 1;
        }
        return;
    }
    if let Some(item) = value.get("item") {
        if let Some(message) = message_from_value(item, time, conversation) {
            conversation.push_message(message);
        } else {
            conversation.unrendered_lines += 1;
        }
        return;
    }
    conversation.unrendered_lines += 1;
}

/// Turn one codex `response_item` payload into a message, or `None` when the
/// record is not part of the conversation (metadata, a token count, or a
/// reasoning record whose text was archived encrypted).
fn codex_item(
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
                blocks_from_content(payload.get("content").unwrap_or(payload), conversation);
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
                blocks: vec![Block::Thinking(text)],
            })
        }
        "function_call" | "custom_tool_call" | "local_shell_call" | "web_search_call" => {
            Some(Message {
                role: Role::Tool,
                time,
                blocks: vec![Block::ToolCall {
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
            blocks: vec![Block::ToolCall {
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

fn normalize_gemini_cli(value: &Value, conversation: &mut Conversation) {
    let Some(messages) = value.get("messages").and_then(Value::as_array) else {
        conversation.unrendered_lines += 1;
        return;
    };
    for item in messages {
        let role = match item.get("type").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("gemini") | Some("assistant") => Role::Assistant,
            _ => {
                conversation.unrendered_lines += 1;
                continue;
            }
        };
        let content = item.get("content").unwrap_or(item);
        let blocks = blocks_from_content(content, conversation);
        if blocks.is_empty() {
            conversation.unrendered_lines += 1;
            continue;
        }
        conversation.push_message(Message {
            role,
            time: message_time(item.get("timestamp")),
            blocks,
        });
    }
}

fn normalize_chatgpt(value: &Value, conversation: &mut Conversation) {
    let Some(raw) = payload(value) else {
        conversation.unrendered_lines += 1;
        conversation.canonical_follows_active = false;
        return;
    };
    let Some(mapping) = raw.get("mapping").and_then(Value::as_object) else {
        normalize_generic(&raw, conversation);
        conversation.canonical_follows_active = false;
        return;
    };
    let Some(current) = raw.get("current_node").and_then(Value::as_str) else {
        conversation.unrendered_lines += 1;
        conversation.canonical_follows_active = false;
        return;
    };
    let mut chain = Vec::new();
    let mut cursor = Some(current.to_string());
    while let Some(id) = cursor {
        if chain.contains(&id) {
            conversation.canonical_follows_active = false;
            break;
        }
        let Some(node) = mapping.get(&id) else {
            conversation.canonical_follows_active = false;
            break;
        };
        chain.push(id.clone());
        cursor = node
            .get("parent")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    conversation.branch_nodes += mapping.len().saturating_sub(chain.len());
    for id in chain.iter().rev() {
        let Some(node) = mapping.get(id) else {
            continue;
        };
        // A node whose `message` is absent or `null` is a mapping node that
        // carries no message at all — ChatGPT writes exactly that for the
        // active branch's root. It is a known-empty, not an unreadable one, so
        // it is skipped rather than counted: counting it made every
        // fully-rendered chatgpt conversation report a line it had lost, and
        // "line" is the wrong unit for a mapping node in any case.
        let Some(message) = node.get("message").filter(|message| !message.is_null()) else {
            continue;
        };
        let Some(role) = message
            .get("author")
            .and_then(|author| author.get("role"))
            .and_then(Value::as_str)
            .and_then(Role::from_str)
        else {
            conversation.unrendered_lines += 1;
            continue;
        };
        let blocks = blocks_from_content(message.get("content").unwrap_or(message), conversation);
        if blocks.is_empty() {
            conversation.unrendered_lines += 1;
            continue;
        }
        conversation.push_message(Message {
            role,
            time: message_time(message.get("create_time")),
            blocks,
        });
    }
}

fn normalize_claude_web(value: &Value, conversation: &mut Conversation) {
    let Some(raw) = payload(value) else {
        conversation.unrendered_lines += 1;
        conversation.canonical_follows_active = false;
        return;
    };
    let Some(messages) = raw.get("chat_messages").and_then(Value::as_array) else {
        normalize_generic(&raw, conversation);
        conversation.canonical_follows_active = false;
        return;
    };
    for item in messages {
        let role = item
            .get("sender")
            .or_else(|| item.get("role"))
            .and_then(Value::as_str)
            .and_then(Role::from_str);
        let Some(role) = role else {
            conversation.unrendered_lines += 1;
            continue;
        };
        let blocks = blocks_from_content(
            item.get("content")
                .or_else(|| item.get("text"))
                .unwrap_or(item),
            conversation,
        );
        if blocks.is_empty() {
            conversation.unrendered_lines += 1;
            continue;
        }
        let time = item.get("created_at").or_else(|| item.get("updated_at"));
        conversation.push_message(Message {
            role,
            time: message_time(time),
            blocks,
        });
    }
}

fn normalize_deepseek(value: &Value, conversation: &mut Conversation) {
    let Some(raw) = payload(value) else {
        conversation.unrendered_lines += 1;
        conversation.canonical_follows_active = false;
        return;
    };
    let messages = raw
        .pointer("/data/biz_data/chat_messages")
        .and_then(Value::as_array)
        .or_else(|| raw.get("messages").and_then(Value::as_array));
    let Some(messages) = messages else {
        normalize_generic(&raw, conversation);
        conversation.canonical_follows_active = false;
        return;
    };
    for item in messages {
        let Some(role) = item
            .get("role")
            .or_else(|| item.get("sender"))
            .and_then(Value::as_str)
            .and_then(Role::from_str)
        else {
            conversation.unrendered_lines += 1;
            continue;
        };
        let blocks = blocks_from_content(
            item.get("content")
                .or_else(|| item.get("text"))
                .unwrap_or(item),
            conversation,
        );
        if blocks.is_empty() {
            conversation.unrendered_lines += 1;
            continue;
        }
        conversation.push_message(Message {
            role,
            time: message_time(
                item.get("inserted_at")
                    .or_else(|| item.get("created_at"))
                    .or_else(|| item.get("updated_at")),
            ),
            blocks,
        });
    }
}

fn normalize_generic(value: &Value, conversation: &mut Conversation) {
    let Some(messages) = value.get("messages").and_then(Value::as_array) else {
        conversation.unrendered_lines += 1;
        return;
    };
    for item in messages {
        let Some(message) =
            message_from_value(item, message_time(item.get("timestamp")), conversation)
        else {
            conversation.unrendered_lines += 1;
            continue;
        };
        conversation.push_message(message);
    }
}

fn message_from_value(
    value: &Value,
    time: MessageTime,
    conversation: &mut Conversation,
) -> Option<Message> {
    if let Some(kind) = value.get("type").and_then(Value::as_str) {
        if matches!(
            kind,
            "function_call" | "tool_call" | "custom_tool_call" | "web_search_call"
        ) {
            return Some(Message {
                role: Role::Tool,
                time,
                blocks: vec![Block::ToolCall {
                    name: value
                        .get("name")
                        .or_else(|| value.get("function"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    input_summary: compact_json(
                        value.get("arguments").or_else(|| value.get("input")),
                    ),
                    output_bytes: value
                        .get("output")
                        .map(|value| compact_json(Some(value)).len()),
                }],
            });
        }
    }
    let role = value
        .get("role")
        .or_else(|| value.get("author").and_then(|a| a.get("role")))
        .and_then(Value::as_str)
        .and_then(Role::from_str)?;
    let content = value.get("content").unwrap_or(value);
    let blocks = blocks_from_content(content, conversation);
    (!blocks.is_empty()).then_some(Message { role, time, blocks })
}

fn blocks_from_content(value: &Value, conversation: &mut Conversation) -> Vec<Block> {
    match value {
        Value::String(text) => vec![Block::Text(text.clone())],
        Value::Array(items) => items
            .iter()
            .flat_map(|item| blocks_from_content(item, conversation))
            .collect(),
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                return vec![Block::Text(text.to_string())];
            }
            if let Some(code) = map.get("code").and_then(Value::as_str) {
                return vec![Block::CodeBlock {
                    language: map
                        .get("language")
                        .or_else(|| map.get("lang"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    code: code.to_string(),
                }];
            }
            let kind = map.get("type").and_then(Value::as_str);
            match kind {
                Some("text") | Some("output_text") => map
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| vec![Block::Text(text.to_string())])
                    // reason: a `text` part whose text is not a string
                    // contributes no block at all, and the caller counts the
                    // whole line as unrendered — an empty list here is "this
                    // part held nothing readable", never "the message was
                    // empty".
                    .unwrap_or_default(),
                Some("thinking") | Some("reasoning") => map
                    .get("thinking")
                    .or_else(|| map.get("text"))
                    .and_then(Value::as_str)
                    .map(|text| vec![Block::Thinking(text.to_string())])
                    // reason: same as `text` above — a reasoning part with no
                    // readable string yields no block and the line is counted.
                    .unwrap_or_default(),
                Some("tool_use") | Some("tool_call") | Some("function_call") => {
                    vec![Block::ToolCall {
                        name: map.get("name").and_then(Value::as_str).map(str::to_string),
                        input_summary: compact_json(
                            map.get("input")
                                .or_else(|| map.get("arguments"))
                                .or_else(|| map.get("content")),
                        ),
                        // The call record names the call; what came back is in
                        // the `tool_result` that follows it, if one was
                        // archived. Writing 0 here would claim a measurement.
                        output_bytes: None,
                    }]
                }
                Some("tool_result") | Some("function_result") => vec![Block::ToolCall {
                    name: map
                        .get("tool_use_id")
                        .or_else(|| map.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    input_summary: "tool result".to_string(),
                    output_bytes: map
                        .get("content")
                        .map(|value| compact_json(Some(value)).len()),
                }],
                Some("image") | Some("attachment") | Some("file") => {
                    let attachment = attachment_from_map(map);
                    conversation.attachments.push(attachment.clone());
                    vec![Block::AttachmentRef(attachment)]
                }
                _ => {
                    if let Some(parts) = map.get("parts").and_then(Value::as_array) {
                        return parts
                            .iter()
                            .flat_map(|part| blocks_from_content(part, conversation))
                            .collect();
                    }
                    if let Some(attachments) = map.get("attachments").and_then(Value::as_array) {
                        for item in attachments {
                            if let Some(item) = item.as_object() {
                                let attachment = attachment_from_map(item);
                                conversation.attachments.push(attachment);
                            }
                        }
                    }
                    Vec::new()
                }
            }
        }
        _ => Vec::new(),
    }
}

fn attachment_from_map(map: &serde_json::Map<String, Value>) -> AttachmentRef {
    AttachmentRef {
        name: map
            .get("name")
            .or_else(|| map.get("filename"))
            .or_else(|| map.get("file_name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        media_type: map
            .get("mime_type")
            .or_else(|| map.get("media_type"))
            .or_else(|| map.get("content_type"))
            .and_then(Value::as_str)
            .map(str::to_string),
        bytes: map
            .get("bytes")
            .or_else(|| map.get("size"))
            .and_then(Value::as_u64),
    }
}

fn payload(value: &Value) -> Option<Value> {
    match value.pointer("/raw/text").and_then(Value::as_str) {
        Some(text) => serde_json::from_str(text).ok(),
        None => Some(value.clone()),
    }
}

fn message_time(value: Option<&Value>) -> MessageTime {
    let Some(value) = value else {
        return MessageTime::Unknown {
            why: "the message has no recorded timestamp".to_string(),
        };
    };
    if let Some(text) = value.as_str() {
        if let Ok(time) = chrono::DateTime::parse_from_rfc3339(text) {
            return MessageTime::Known {
                unix: time.timestamp(),
                source: TimeSource::Exact,
            };
        }
    }
    let numeric = value
        .as_i64()
        .or_else(|| value.as_f64().map(|n| n.trunc() as i64));
    if let Some(raw) = numeric {
        let (unix, how) = if raw >= 946_684_800 && raw <= 4_102_444_800 {
            (raw, "numeric epoch seconds")
        } else if raw >= 946_684_800_000 && raw <= 4_102_444_800_000 {
            (raw / 1_000, "numeric epoch milliseconds")
        } else {
            return MessageTime::Unknown {
                why: "the recorded timestamp is outside the supported time window".to_string(),
            };
        };
        return MessageTime::Known {
            unix,
            source: TimeSource::Inferred {
                how: how.to_string(),
            },
        };
    }
    MessageTime::Unknown {
        why: "the recorded timestamp could not be interpreted".to_string(),
    }
}

fn compact_json(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| "unavailable".to_string()),
        None => "no input recorded".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_fixture_renders_all_message_block_kinds() {
        let body = concat!(
            r##"{"type":"user","timestamp":"2026-09-25T10:00:00Z","message":{"role":"user","content":"# Question\n\n**hello**"}}"##,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"answer"},{"type":"thinking","thinking":"private"},{"type":"tool_use","name":"search","input":{"q":"x"}},{"type":"image","name":"plot.png","content_type":"image/png","bytes":12}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"ok"}]}}"#,
        );
        let conversation = normalize("claude-code", body);
        assert_eq!(conversation.messages.len(), 3);
        assert!(matches!(conversation.messages[0].role, Role::User));
        // The assistant turn also holds a tool call; it is still the assistant
        // that wrote the prose, and the tool call is folded inside that turn.
        assert!(matches!(conversation.messages[1].role, Role::Assistant));
        assert!(conversation.messages[1]
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Text(_))));
        assert!(conversation.messages[1]
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Thinking(_))));
        assert!(conversation.messages[1]
            .blocks
            .iter()
            .any(|block| matches!(block, Block::ToolCall { .. })));
        assert_eq!(conversation.attachments.len(), 1);
        assert!(matches!(conversation.messages[2].role, Role::Tool));
    }

    #[test]
    fn codex_and_gemini_fixtures_produce_roles_and_code() {
        let codex = r#"{"timestamp":1736944496,"payload":{"message":{"role":"assistant","content":[{"type":"output_text","text":"done"},{"type":"code","language":"rust","code":"fn main() {}"}]}}}"#;
        let result = normalize("codex", codex);
        assert_eq!(result.messages.len(), 1);
        assert!(result.messages[0]
            .blocks
            .iter()
            .any(|block| matches!(block, Block::CodeBlock { .. })));

        let gemini = r#"{"messages":[{"type":"user","timestamp":"2026-09-25T10:00:00Z","content":[{"text":"hi"}]},{"type":"gemini","content":[{"text":"hello"}]}]}"#;
        let result = normalize("gemini-cli", gemini);
        assert_eq!(result.messages.len(), 2);
        assert!(matches!(result.messages[1].role, Role::Assistant));
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
        assert!(matches!(
            result.messages[2].blocks[0],
            Block::ToolCall {
                output_bytes: None,
                ..
            }
        ));
        assert!(matches!(
            result.messages[3].blocks[0],
            Block::ToolCall {
                output_bytes: Some(7),
                ..
            }
        ));
        assert!(matches!(result.messages[4].role, Role::Assistant));
        // session_meta, event_msg (mirror) and token_count are not messages.
        assert_eq!(result.unrendered_lines, 4);
    }

    /// `gemini-cli` does not archive JSONL: its body is pretty-printed JSON
    /// documents run together, so line-by-line parsing reads nothing at all.
    /// The body has to be re-read as the stream of documents it is, and the
    /// lines must not then be reported as unreadable content.
    #[test]
    fn a_gemini_cli_document_is_read_as_documents_not_as_lines() {
        let body = concat!(
            "{\n",
            "  \"sessionId\": \"s1\",\n",
            "  \"messages\": [\n",
            "    {\"id\": \"m1\", \"timestamp\": \"2026-09-25T10:00:00Z\",\n",
            "     \"type\": \"user\", \"content\": [{\"text\": \"hi\"}]},\n",
            "    {\"id\": \"m2\", \"type\": \"gemini\", \"content\": [{\"text\": \"hello\"}]}\n",
            "  ],\n",
            "  \"kind\": \"main\"\n",
            "}\n",
            // A second document in the same body, which is what the archive
            // actually holds: the walk above has to reach it too.
            "{\n",
            "  \"sessionId\": \"s1\",\n",
            "  \"messages\": [\n",
            "    {\"id\": \"m3\", \"type\": \"user\", \"content\": [{\"text\": \"again\"}]}\n",
            "  ],\n",
            "  \"kind\": \"summary\"\n",
            "}\n",
        );
        let result = normalize("gemini-cli", body);
        assert_eq!(result.messages.len(), 3, "both documents must be read");
        assert!(matches!(result.messages[0].role, Role::User));
        assert!(matches!(result.messages[1].role, Role::Assistant));
        assert!(matches!(result.messages[2].role, Role::User));
        assert_eq!(
            result.unrecognized_lines, 0,
            "the framing is ours to solve, not the body's defect"
        );
        assert_eq!(
            result.unrendered_lines, 0,
            "no record in either document went unrendered"
        );
    }

    /// A body that reads as documents but yields no message must not report
    /// the lines as invalid JSON: the framing is solved and the residue is a
    /// document with nothing readable in it, which is the honest thing to
    /// count.
    #[test]
    fn a_readable_gemini_cli_body_with_no_message_does_not_blame_the_json() {
        let body = "{\n  \"sessionId\": \"s1\",\n  \"kind\": \"main\"\n}\n";
        let result = normalize("gemini-cli", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(
            result.unrecognized_lines, 0,
            "the body is valid JSON; it simply holds no message"
        );
        assert_eq!(
            result.unrendered_lines, 1,
            "one document was framed and held no message"
        );
    }

    /// A body whose lines do not parse and whose document stream does not read
    /// either keeps the line counts: they are the only measurement there is,
    /// and replacing them with a clean zero would turn "unreadable" into
    /// "empty".
    #[test]
    fn an_unreadable_gemini_cli_body_keeps_its_counts() {
        // A document, then something that is not part of any document.
        let body = "{\n  \"nothing\": [\n    1, 2\n  ]\n}\nnot json at all\n";
        let result = normalize("gemini-cli", body);
        assert_eq!(result.messages.len(), 0);
        assert_eq!(
            result.unrecognized_lines, 6,
            "every line of an unreadable body stays counted as unreadable"
        );
        assert_eq!(result.unrendered_lines, 0);
    }

    /// `opencode` archives a session summary whose `messages` array the
    /// generic reader can already read. Routing it to the codex extractor
    /// claimed a shape it does not have.
    #[test]
    fn an_opencode_summary_is_read_by_the_generic_reader() {
        let body = r#"{"schema":"opencode/v1","session":{"agent":"build"},"orphan_parts":[],"messages":[{"role":"user","content":"q"},{"role":"assistant","content":"a"}]}"#;
        let result = normalize("opencode", body);
        assert_eq!(result.messages.len(), 2);
        assert!(matches!(result.messages[0].role, Role::User));
        assert!(matches!(result.messages[1].role, Role::Assistant));
    }

    #[test]
    fn chatgpt_fixture_follows_active_branch_and_counts_other_nodes() {
        let raw = r#"{"current_node":"n2","mapping":{"root":{"message":null,"parent":null},"n1":{"message":{"author":{"role":"user"},"content":{"parts":["q"]},"create_time":1736944496},"parent":"root"},"n2":{"message":{"author":{"role":"assistant"},"content":{"parts":["a"]}},"parent":"n1"},"branch":{"message":{"author":{"role":"assistant"},"content":{"parts":["old"]}},"parent":"n1"}}}"#;
        let line = serde_json::json!({"raw":{"text":raw}}).to_string();
        let result = normalize("chatgpt", &line);
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.branch_nodes, 1);
        assert!(result.canonical_follows_active);
        assert_eq!(
            result.unrendered_lines, 0,
            "the `\"message\": null` root carries no message at all, so it is a \
             mapping node with nothing to render — not a line the reader lost"
        );
    }

    #[test]
    fn unknown_time_and_unrecognized_lines_stay_explicit() {
        let result = normalize("", "not json\n{\"metadata\":true}\n");
        assert_eq!(result.unrecognized_lines, 1);
        assert_eq!(result.unrendered_lines, 1);
        assert!(matches!(result.provenance, Provenance::Unknown { .. }));
    }
}
