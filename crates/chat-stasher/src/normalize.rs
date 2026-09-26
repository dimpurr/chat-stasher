//! Conversation normalization for the local reader.
//!
//! The archive keeps source-shaped JSONL as its raw truth. This module adds a
//! small, deliberately lossy view for people: message roles, Markdown text,
//! code, thinking, tool calls, and attachment references. Anything it cannot
//! classify remains counted and the raw route stays available.

use crate::activity::TimeSource;
use serde_json::Value;
use std::collections::HashMap;

/// One extractor file per harness, dispatched by [`normalize_value`]. A
/// harness with no arm there is served the raw view only — see
/// [`harness_has_a_reader`] — and never a generic guess about its shape.
mod codex;
mod cursor;
mod gemini_cli;
mod kimi_code;
mod opencode;

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
    Known {
        source: String,
    },
    Unknown {
        why: String,
    },
    /// The harness is known and this build has no extractor for it: the raw
    /// shards are the only view. This is a third, distinct state — not
    /// "unknown" (the harness *is* identified) and never "empty" (no claim
    /// at all is made about the body, so the zeroed counters below are
    /// unread, not measured).
    RawOnly {
        why: String,
    },
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
///
/// A harness with no extractor in this build gets no interpretation at all:
/// no lines are parsed and no counts are taken, and the returned
/// [`Provenance::RawOnly`] tells the reader to serve the raw view honestly
/// (the design's "raw view only" state, 29-UI-DESIGN UIA-7).
pub fn normalize(harness: &str, body: &str) -> Conversation {
    let mut conversation = Conversation::new(harness);
    if !harness_has_a_reader(harness) {
        mark_raw_view_only(harness, &mut conversation);
        return conversation;
    }
    if harness == "kimi-code" {
        // The wire journal needs per-body state to fold a step's parts into
        // one assistant message, so it owns its own line walk.
        kimi_code::normalize_body(body, &mut conversation);
        return conversation;
    }
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
                // `gemini_cli::document` counts a document that holds no
                // message, and each record inside one that it cannot read.
                conversation.unrendered_lines = 0;
                conversation.unrecognized_lines = 0;
                for document in &documents {
                    gemini_cli::document(document, &mut conversation);
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

/// The harnesses this build reads as a conversation. This is the registry: one
/// arm per extractor file, one row for the web-platform ids whose archived line
/// is an inbox bundle (see [`is_web_bundle`], whose reader is the generic one),
/// and the `""` row that carries no harness id at all (it keeps the structural
/// attempt and its distinct provenance-unknown state). Every other id — the
/// registry harnesses with no extractor yet (github-copilot-cli, aider, crush,
/// zed, continue) — is served the raw view only.
fn harness_has_a_reader(harness: &str) -> bool {
    matches!(
        harness,
        "claude-code"
            | "codex"
            | "gemini-cli"
            | "kimi-code"
            | "opencode"
            | "cursor"
            | "chatgpt"
            | "deepseek"
            | "claude"
            | ""
    ) || is_web_bundle(harness)
}

/// A harness whose archived line is an inbox bundle rather than a message line:
/// the platform's own JSON body sits under `raw.text`, the envelope every
/// extractor above unwraps for itself. Such a platform's reader is the generic
/// one (29-UI-DESIGN W140: the web bodies whose shape no extractor of its own
/// reads go to the generic reader), so a platform with no extractor is read
/// from its body instead of being counted as one unreadable bundle. A bundle
/// whose `raw.text` is not JSON is a body this reader cannot read at all, and
/// stays counted.
///
/// `activity::WEB_HARNESSES` is the same list the capture side and the time
/// reader use, so this rule cannot drift away from the line shape it names.
fn is_web_bundle(harness: &str) -> bool {
    crate::activity::WEB_HARNESSES.contains(&harness)
}

fn mark_raw_view_only(harness: &str, conversation: &mut Conversation) {
    conversation.provenance = Provenance::RawOnly {
        why: if harness.is_empty() {
            "the archived row did not identify a harness".to_string()
        } else {
            format!("no reader for `{harness}` records exists in this build")
        },
    };
    conversation.unrendered_lines = 0;
    conversation.unrecognized_lines = 0;
}

fn normalize_value(harness: &str, value: &Value, conversation: &mut Conversation) {
    match harness {
        "claude-code" => normalize_cli_line(harness, value, conversation),
        "codex" => codex::normalize_line(value, conversation),
        "gemini-cli" => gemini_cli::document(value, conversation),
        "kimi-code" => kimi_code::record_or_count(value, conversation),
        "opencode" => opencode::normalize_session(value, conversation),
        "cursor" => cursor::normalize_session(value, conversation),
        "chatgpt" => normalize_chatgpt(value, conversation),
        "claude" => normalize_claude_web(value, conversation),
        "deepseek" => normalize_deepseek(value, conversation),
        // An empty harness id is not a harness at all: there is no shape to
        // claim, the provenance-unknown state is already recorded, and the
        // envelope probe below is the only honest structural attempt left.
        "" => normalize_generic(value, conversation),
        // A web platform with no extractor of its own lands here: its archived
        // line is an inbox bundle rather than the platform's body, so the body
        // is unwrapped and read by the generic reader — the platform is read
        // rather than counted as one unreadable bundle. The three web platforms
        // that do have an extractor of their own are matched above and never
        // reach this arm.
        _ if is_web_bundle(harness) => match payload(value) {
            Some(body) => normalize_generic(&body, conversation),
            None => conversation.unrendered_lines += 1,
        },
        // Unreachable while `normalize` gates on `harness_has_a_reader`, and
        // this same arm is the fallback a new harness id deserves if the two
        // ever disagree: no interpretation, no counts, raw view only.
        _ => mark_raw_view_only(harness, conversation),
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
    for item in claude_active_thread(&raw, messages, conversation) {
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

/// The parent-link spellings a claude.ai body has been seen to use. Two, not
/// one, because reference implementations on this platform disagree — the same
/// pair the capture side accepts, so a body the archive accepted is a body this
/// reader can walk.
const CLAUDE_PARENT_KEYS: [&str; 2] = ["parent_message_uuid", "parent_uuid"];

/// The message a claude.ai record names as its parent, if it names one.
///
/// The first spelling that carries a **non-empty** string wins, and an empty
/// one does not end the search: the capture side reads the pair exactly this way
/// (`claudeParentKeyOf` accepts a message when *either* key holds a non-empty
/// string), so a message that spells one key `""` and the other as a real uuid
/// was accepted for capture and has to walk here too. A message whose both keys
/// are absent or empty names no parent and is a root, not an unwalkable branch.
fn claude_parent(message: &Value) -> Option<&str> {
    CLAUDE_PARENT_KEYS.iter().find_map(|key| {
        message
            .get(*key)
            .and_then(Value::as_str)
            .filter(|parent| !parent.is_empty())
    })
}

/// The messages of a claude.ai body, on its active branch, oldest first.
///
/// The body is a flat `chat_messages` array that still carries a tree: every
/// message names its parent, and `current_leaf_message_uuid` names the branch
/// that is current. The chain of parent links up from that leaf is the
/// conversation; the messages off it are abandoned branches, counted in
/// `branch_nodes` rather than rendered as if they had been said.
///
/// Three states, kept apart rather than collapsed into each other:
///
/// * **no message names a parent** — the response is flat and there is no tree
///   to follow, so every message is the conversation and nothing is hidden.
///   (Reference implementations take the same branch for this shape:
///   `resolveClaudeActiveBranch` reports a body with no parent links as
///   complete.)
/// * **the chain ends at a parent the body does not carry** — the normal end.
///   The wire ends every branch at a shared tree-root sentinel that no body
///   carries (measured on the capture side, 2026-09-24), so this is a root, not
///   a dropped message.
/// * **a leaf the body does not carry, or a cycle** — the walk proves nothing.
///   The messages are shown in the body's own order and the caller is told the
///   branch provenance is unknown: which branch is current is never guessed.
fn claude_active_thread<'a>(
    raw: &Value,
    messages: &'a [Value],
    conversation: &mut Conversation,
) -> Vec<&'a Value> {
    if !messages
        .iter()
        .any(|message| claude_parent(message).is_some())
    {
        return messages.iter().collect();
    }
    let leaf = raw
        .get("current_leaf_message_uuid")
        .and_then(Value::as_str)
        .filter(|leaf| !leaf.is_empty());
    let by_id = claude_by_id(messages);
    let chain = leaf.and_then(|leaf| claude_chain(leaf, &by_id));
    let Some(chain) = chain else {
        conversation.canonical_follows_active = false;
        return messages.iter().collect();
    };
    let thread: Vec<&Value> = chain
        .iter()
        .filter_map(|id| by_id.get(id).copied())
        .collect();
    // Every message the chain did not consume is off the active branch: an
    // abandoned sibling, a re-answer, or a record this walk cannot place. The
    // count is the array's own arithmetic, not an estimate.
    conversation.branch_nodes += messages.len().saturating_sub(thread.len());
    thread
}

/// A claude.ai body's messages keyed by the id its parent links name them with.
/// A message that carries no uuid cannot be named by any link, so it is absent
/// here — and therefore off every branch this walk can find.
fn claude_by_id(messages: &[Value]) -> HashMap<&str, &Value> {
    messages
        .iter()
        .filter_map(|message| {
            message
                .get("uuid")
                .and_then(Value::as_str)
                .map(|id| (id, message))
        })
        .collect()
}

/// Walk `parent_message_uuid` up from `leaf` to the branch root, returning the
/// ids oldest-first. `None` when the walk cannot reach a root — the leaf is not
/// in the body, or the links cycle — which is the state that must not be
/// reported as a whole conversation.
fn claude_chain<'a>(leaf: &'a str, by_id: &HashMap<&'a str, &'a Value>) -> Option<Vec<&'a str>> {
    let mut chain: Vec<&str> = Vec::new();
    let mut current = Some(leaf);
    while let Some(id) = current {
        let Some(message) = by_id.get(id) else {
            // The one parent no branch carries is the shared tree-root
            // sentinel: the walk is over, and the chain is complete.
            break;
        };
        if chain.contains(&id) {
            return None;
        }
        chain.push(id);
        current = claude_parent(message);
    }
    if chain.is_empty() {
        return None;
    }
    chain.reverse();
    Some(chain)
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

/// The fallback reader: a harness with no extractor of its own.
///
/// A web platform's archived body is the platform's own response, and two
/// top-level shapes carry messages: an object with a `messages` array, and an
/// array of records. Both are read here, so a platform we have no extractor for
/// is read rather than counted as one unreadable line. Records the reader
/// cannot turn into a message stay counted, one per record — the array's own
/// arithmetic, not a claim about the whole body.
fn normalize_generic(value: &Value, conversation: &mut Conversation) {
    let messages = match value {
        Value::Array(messages) => messages,
        _ => match value.get("messages").and_then(Value::as_array) {
            Some(messages) => messages,
            None => {
                conversation.unrendered_lines += 1;
                return;
            }
        },
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

    // The codex, gemini-cli, opencode, cursor and kimi-code fixtures live in
    // their own files (`normalize/<harness>.rs`), next to the extractors they
    // pin. This root module pins the model shared by all of them, plus the
    // claude-code reference extractor and the web-platform readers that have
    // no file of their own yet.

    /// `opencode` and `cursor` previously reached the generic reader through
    /// a test asserting a summary shape (`messages[].role/content`) that the
    /// capture side never writes — which is why the real-archive smoke in
    /// the W154 report rendered zero messages for both harnesses. The
    /// per-harness files own their real-shape fixtures now
    /// (`opencode.rs`, `cursor.rs`); this root test pins only the registry
    /// behaviour the page depends on: a harness with no reader is never
    /// handed a generic guess.
    #[test]
    fn an_unimplemented_harness_serves_the_raw_view_only_state() {
        let body = "{\"unexpected\":true}\nnot json either\n";
        let result = normalize("aider", body);
        assert_eq!(result.messages.len(), 0);
        // No line was parsed, so "0" here is unread, not measured — the
        // `RawOnly` provenance is what the reader must show instead of any
        // zero-message sentence.
        assert_eq!(result.unrendered_lines, 0);
        assert_eq!(result.unrecognized_lines, 0);
        assert!(matches!(result.provenance, Provenance::RawOnly { .. }));
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
