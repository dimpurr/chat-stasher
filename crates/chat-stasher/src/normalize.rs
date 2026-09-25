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
        output_bytes: usize,
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
    conversation
}

fn normalize_value(harness: &str, value: &Value, conversation: &mut Conversation) {
    match harness {
        "claude-code" | "kimi-code" => normalize_cli_line(harness, value, conversation),
        "codex" | "opencode" | "cursor" => normalize_codex_line(harness, value, conversation),
        "gemini-cli" => normalize_gemini_cli(value, conversation),
        "chatgpt" => normalize_chatgpt(value, conversation),
        "claude" => normalize_claude_web(value, conversation),
        "deepseek" => normalize_deepseek(value, conversation),
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

fn normalize_codex_line(harness: &str, value: &Value, conversation: &mut Conversation) {
    if let Some(message) = value.pointer("/payload/message") {
        let time = message_time(value.get("timestamp"));
        if let Some(message) = message_from_value(message, time, conversation) {
            conversation.push_message(message);
        } else {
            conversation.unrendered_lines += 1;
        }
        return;
    }
    if value.get("type").and_then(Value::as_str) == Some("response_item") {
        if let Some(item) = value.pointer("/payload/item") {
            let time = message_time(value.get("timestamp"));
            if let Some(message) = message_from_value(item, time, conversation) {
                conversation.push_message(message);
            } else {
                conversation.unrendered_lines += 1;
            }
            return;
        }
    }
    if let Some(item) = value.get("item") {
        let time = message_time(value.get("timestamp"));
        if let Some(message) = message_from_value(item, time, conversation) {
            conversation.push_message(message);
        } else {
            conversation.unrendered_lines += 1;
        }
        return;
    }
    let _ = harness;
    conversation.unrendered_lines += 1;
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
        let Some(message) = node.get("message") else {
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
                        .map(|value| compact_json(Some(value)))
                        .map(|s| s.len())
                        .unwrap_or(0),
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
                    .unwrap_or_default(),
                Some("thinking") | Some("reasoning") => map
                    .get("thinking")
                    .or_else(|| map.get("text"))
                    .and_then(Value::as_str)
                    .map(|text| vec![Block::Thinking(text.to_string())])
                    .unwrap_or_default(),
                Some("tool_use") | Some("tool_call") | Some("function_call") => {
                    vec![Block::ToolCall {
                        name: map.get("name").and_then(Value::as_str).map(str::to_string),
                        input_summary: compact_json(
                            map.get("input")
                                .or_else(|| map.get("arguments"))
                                .or_else(|| map.get("content")),
                        ),
                        output_bytes: 0,
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
                        .map(|value| compact_json(Some(value)))
                        .map(|s| s.len())
                        .unwrap_or(0),
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

    #[test]
    fn chatgpt_fixture_follows_active_branch_and_counts_other_nodes() {
        let raw = r#"{"current_node":"n2","mapping":{"root":{"message":null,"parent":null},"n1":{"message":{"author":{"role":"user"},"content":{"parts":["q"]},"create_time":1736944496},"parent":"root"},"n2":{"message":{"author":{"role":"assistant"},"content":{"parts":["a"]}},"parent":"n1"},"branch":{"message":{"author":{"role":"assistant"},"content":{"parts":["old"]}},"parent":"n1"}}}"#;
        let line = serde_json::json!({"raw":{"text":raw}}).to_string();
        let result = normalize("chatgpt", &line);
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.branch_nodes, 1);
        assert!(result.canonical_follows_active);
    }

    #[test]
    fn unknown_time_and_unrecognized_lines_stay_explicit() {
        let result = normalize("", "not json\n{\"metadata\":true}\n");
        assert_eq!(result.unrecognized_lines, 1);
        assert_eq!(result.unrendered_lines, 1);
        assert!(matches!(result.provenance, Provenance::Unknown { .. }));
    }
}
