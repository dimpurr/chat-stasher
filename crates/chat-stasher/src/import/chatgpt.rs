//! The ChatGPT official export — `conversations.json`, the DSAR shape.
//!
//! ADR-055 makes an official export the platform's own statement of what an
//! account holds, and makes ChatGPT pilot #2 (D10) — while the import itself is
//! gated: the DSAR is the *validation set* for the extension's own ChatGPT
//! backfill and must not be spent filling a backlog (27-ORACLE §0.1; the import
//! is deferred, and this parser exists so that when the gate opens the reading
//! is already specified). This module is the parser half of that groundwork and
//! nothing else: bytes in, conversation-level records out. It writes nothing,
//! reads nothing from disk, reaches no network and is wired to no command —
//! which is what makes it safe to run over a copy of a real export while the
//! archive under measurement stays untouched.
//!
//! ## The measured shape
//!
//! Measured read-only on the two single-conversation copies this machine holds
//! (141,527 B / 35 mapping nodes and 1,759,183 B / 154), together with 27-ORACLE
//! §4.2 and §4.5 for the two facts that decide the reading. Every number below
//! is from those files; no value, id or title is quoted anywhere in this crate.
//!
//! * the file is a top-level JSON **array** of conversation objects, and a single
//!   conversation object is a valid file too — that is the shape both local
//!   copies have;
//! * a conversation carries 31 keys in the smaller copy and 32 in the larger; the
//!   keys this build reads are `conversation_id` (or its alternative spelling
//!   `id` — see [`conversation_id`]), `create_time`, `update_time`, `title`,
//!   `current_node` and `mapping`. The rest (`is_archived`, `safe_urls`,
//!   `memory_scope`, `gizmo_id`, …) are not read and are reported by name in
//!   [`ExportParse::unreadable`] rather than dropped in silence. Nothing is lost
//!   by that, because the capture that matters is the export's own conversation
//!   object, kept field for field by the producer ([`super`]) — this record is a
//!   reading of it, never a substitute for it;
//! * `mapping` is an **object keyed by node id**, each node carrying exactly
//!   `id` / `parent` / `children` / `message` on all 189 nodes measured, and a
//!   node's own `id` field equals the key it is filed under on 189/189. Every
//!   link names a key, so the key is the identity this module carries;
//! * exactly one node per conversation names no parent (`"parent": null`) — the
//!   tree's root, whose `message` is `null` — and the remaining nodes descend
//!   from it: 189 nodes hold 187 messages and 6 leaves between them, so a
//!   conversation's message count depends on which leaf is walked. `children`
//!   and `parent` agree entry for entry, and no link names an id the object does
//!   not hold (0 in both directions, out of 189 nodes) — see
//!   [`TreeReading::MissingNodes`] for what a file that does would read as;
//! * `current_node` names the node the web UI last pointed at, and 27-ORACLE
//!   §4.5 measured that it is **not** the deepest branch: walking it alone
//!   manufactured a false RED on a conversation we had captured in full. So the
//!   reading this module publishes is the **longest branch**, the leaf count
//!   stays beside it so the ambiguity is visible (that is §4.5's
//!   `branch_ambiguous_sessions`), and the export's own `current_node` is
//!   carried as a stated fact about itself, never applied to anyone else's
//!   reading;
//! * every timestamp measured — the two conversations' and all 187 messages' —
//!   is a float epoch in **seconds** (magnitude 1e9, fractional), on
//!   `create_time` and `update_time` alike; this slice is specified for
//!   **seconds and milliseconds**, so both are read and the unit that was read is
//!   named ([`TimeUnit`]) instead of being assumed;
//! * a message carries 11 keys measured (`author`, `channel`, `content`,
//!   `create_time`, `end_turn`, `id`, `metadata`, `recipient`, `status`,
//!   `update_time`, `weight`). `author.role` is `user`, `assistant` or `tool` on
//!   the 187 messages measured, and `content.content_type` is one of 6 measured
//!   kinds (`text`, `code`, `thoughts`, `reasoning_recap`, `multimodal_text`,
//!   `tether_browsing_display`), each with extra keys of its own. `update_time`
//!   is `null` on 97 of the 153 messages of the larger copy — a stated absence,
//!   and read as one.
//!
//! ## Four things this module deliberately does not do
//!
//! * **It does not call a branch the conversation.** The export states which
//!   node its UI pointed at; it does not state that the conversation *is* that
//!   path, and §4.5 shows it is usually not. Every node is kept, the longest
//!   branch is measured, and the leaf count says how much choice there was.
//! * **It does not interpret a content kind.** `thoughts`, `reasoning_recap` and
//!   `multimodal_text` all occur; this build reads the *text* parts and the
//!   `content_type` label, and does not turn a kind into a role or a claim about
//!   what the message means. A part that is not a string is read as a structured
//!   part (its JSON kind is kept), not dropped and not counted as loss.
//! * **It does not read the whole DSAR.** The package of record is 3.53 GiB and
//!   the importer must stream it; nothing here pretends otherwise. This module
//!   takes the bytes of one export in memory, which is the shape the two measured
//!   copies have, and [`parse_export`] states that limit where a caller meets it.
//! * **It does not decide the import.** No bundle, no sealing, no inbox, no
//!   exit code: this is the parser an `import` arm would call, and the CLI
//!   surfaces that own those decisions are the producer's ([`super`]) and
//!   `main`'s.
//!
//! ## How the three states are kept, in this module's types
//!
//! A value field is a [`Field`] — the value the export carried, an explicit
//! `null` it wrote, a field it did not carry at all, or one it carried in a
//! shape this build cannot read — never two of those collapsed into one. A time
//! is a [`RecordedTime`]: the instant *and the unit it was written in*, or a
//! named reason it could not be placed in time, never a `0`. A mapping's own
//! links are a [`TreeReading`], whose three arms are three different claims:
//! every link resolves ([`TreeReading::Consistent`]), at least one does not
//! ([`TreeReading::MissingNodes`]), or the export states there are no nodes at
//! all ([`TreeReading::NoNodes`]). A partially fetched mapping cannot be ruled
//! out from structure alone (§4.5), so nothing here is named *complete* — and
//! the missing-link arm carries no branch reading at all, because a tree with a
//! hole in it cannot answer "which branch is the conversation". A count of zero
//! is a measurement, so it appears only where something was measured.
//!
//! Nothing is skipped in silence. A file that cannot be read as an export is an
//! [`ExportFailure`]; a conversation *inside* a readable file that cannot become
//! a record is a [`ConversationFailure`], reported beside the records with its id
//! and a named reason. A `mapping` that is absent, `null` or not an object is one
//! of those: the tree is what the record *is*, and emitting an empty one would
//! read as "a conversation with nothing in it" — a claim the export did not make.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// The platform these records belong to: the `<platform>` of
/// `chat-stasher import <platform> <export-file>` (ADR-055 D4). It is the same id
/// the live web capture of this platform uses (`activity::WEB_HARNESSES`), which
/// is what makes an imported conversation and a captured one land in one bucket
/// instead of two.
pub const PLATFORM: &str = "chatgpt";

/// Paths that name a spot in the export, for a record that has no line number.
/// `conversations[]` is one conversation, `mapping[]` one node, and a path with a
/// field name appended (`…mapping[].message.content.content_type`) names a field.
const CONVERSATION: &str = "conversations[]";
const NODE: &str = "conversations[].mapping[]";
const MESSAGE: &str = "conversations[].mapping[].message";
const CONTENT: &str = "conversations[].mapping[].message.content";
const AUTHOR: &str = "conversations[].mapping[].message.author";

/// The field names the measured shape carries at each level, restricted to the
/// ones this build **reads**. A name that is not in its row is reported as an
/// unreadable field rather than read as if it were one of these, which is the
/// only way a field the platform adds becomes visible instead of silently
/// dropped. The rows are deliberately narrower than the measured key sets: a
/// name is in a row when a value of it reaches a field of a record.
const CONVERSATION_KEYS: [&str; 7] = [
    "conversation_id",
    "create_time",
    "current_node",
    "id",
    "mapping",
    "title",
    "update_time",
];
const NODE_KEYS: [&str; 4] = ["children", "id", "message", "parent"];
const MESSAGE_KEYS: [&str; 6] = [
    "author",
    "content",
    "create_time",
    "id",
    "recipient",
    "update_time",
];
const AUTHOR_KEYS: [&str; 2] = ["name", "role"];
const CONTENT_KEYS: [&str; 2] = ["content_type", "parts"];

/// The two ids a conversation object may carry, in the order this build prefers
/// them. `conversation_id` is the measured spelling on both local copies;
/// `id` is accepted because a bulk array entry is the same object one level
/// deeper and the two spellings are what the shapes in the wild use.
const ID_KEYS: [&str; 2] = ["conversation_id", "id"];

/// Plausible window for a *conversation* timestamp, in unix seconds — the same
/// window `crate::activity` uses for the same quantity
/// (`MIN_PLAUSIBLE_SECONDS` / `MAX_PLAUSIBLE_SECONDS`, `activity.rs:1482-1483`),
/// repeated here because this module must name the **unit** it read and that
/// helper returns seconds only. A value outside every window below is
/// [`RecordedTime::Unknown`], never clamped into range and never read as `0`.
const MIN_PLAUSIBLE_SECONDS: i64 = 1_577_836_800; // 2020-01-01
const MAX_PLAUSIBLE_SECONDS: i64 = 4_102_444_800; // 2100-01-01

/// Read one ChatGPT export file.
///
/// The input is the file's bytes exactly as the platform produced them; the same
/// bytes always produce the same records. A file that cannot be read as an export
/// at all is an [`ExportFailure`]; a conversation *inside* a readable file that
/// cannot be turned into a record is a [`ConversationFailure`], kept in
/// [`ExportParse::failures`] beside the records — never dropped, because a caller
/// that counted only the successes would report a smaller number that looks
/// complete (ADR-014, CLAUDE.md invariant 1).
///
/// The whole document is held in memory while it is read. The ChatGPT DSAR
/// measured for this platform is 3.53 GiB with 7,865 conversations, so a caller
/// must not hand *that* file here: it needs a streaming reader, and the signature
/// below is where that becomes visible rather than a surprise. What this function
/// reads is one export's bytes, which is what the measured copies are.
pub fn parse_export(bytes: &[u8]) -> Result<ExportParse, ExportFailure> {
    let document: Value = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(error) => {
            // One parse failure is two different answers (invariant 2), and
            // serde_json names the difference: `Eof`/`Io` means the input stopped
            // part-way, so the export was never read to the end and nothing about
            // its contents is proven — the caller's exit 3. Every other category
            // means the bytes were read in full and are not a JSON document.
            return Err(if error.is_eof() || error.is_io() {
                ExportFailure::NotReadToTheEnd {
                    why: error.to_string(),
                }
            } else {
                ExportFailure::NotJson {
                    why: error.to_string(),
                }
            });
        }
    };

    let mut reader = Reader::default();
    match &document {
        Value::Array(entries) => {
            for entry in entries {
                reader.conversation(entry);
            }
        }
        // A single conversation object is a valid file: that is the shape both
        // measured copies have, and one object is the array's own element.
        Value::Object(_) => reader.conversation(&document),
        other => {
            return Err(ExportFailure::NotAnExport {
                found: kind_of(other),
            })
        }
    }
    Ok(reader.finish())
}

/// Why an export file could not be read as an export at all.
///
/// `NotReadToTheEnd` and `NotJson` are two answers, not one: the first says the
/// file stopped part-way, so *any* absence in it proves nothing; the second says
/// the bytes were read in full and are not a document this build reads. Collapsing
/// them is what CLAUDE.md invariant 2 forbids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportFailure {
    /// The bytes stopped part-way through a JSON document, so the export was
    /// never read to the end. A parser position is named in `why`, never any of
    /// the export's own text.
    NotReadToTheEnd { why: String },
    /// The bytes are not JSON this build can read: invalid UTF-8, or a syntax
    /// error at the position named in `why`.
    NotJson { why: String },
    /// Valid JSON, but neither the array of conversations an export is nor one
    /// conversation object. The kind actually found is named — `"object"`,
    /// `"string"`, … — so a caller can tell the wrong file from a corrupt one.
    NotAnExport { found: &'static str },
}

/// One ChatGPT export, read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExportParse {
    /// The conversations this build could read, in the file's own order.
    pub conversations: Vec<ConversationRecord>,
    /// The conversations it could not, in the file's own order, each with its id
    /// when the entry named a readable one.
    pub failures: Vec<ConversationFailure>,
    /// Every spot where the export carried something this build did not read,
    /// keyed by path and counted. The path tells the sources apart:
    ///
    /// * a **field name** the row for its level does not carry, e.g.
    ///   `conversations[].is_archived` — how a field the platform adds becomes
    ///   visible instead of silently dropped. A real ChatGPT export has a
    ///   non-empty tally here by design: this build reads six of a conversation's
    ///   thirty-odd keys, and the rest are reported rather than pretended away;
    /// * an **entry inside a list** this build cannot read, e.g.
    ///   `…mapping[].children[]` holding a number;
    /// * a node whose own `id` field disagrees with the key it is filed under —
    ///   links use the key, so the field is not read as an identity.
    ///
    /// A value that has a **typed home** in a record is deliberately not counted
    /// here: a `parent` this build cannot read is named on the node
    /// ([`ParentLink::Unreadable`]) and a `message` that is not an object is named
    /// on the slot ([`MessageSlot::Unreadable`]), which are the stronger reports,
    /// and counting them twice would make this tally unreadable.
    pub unreadable: BTreeMap<String, usize>,
}

impl ExportParse {
    /// How many mapping nodes the records hold.
    pub fn node_count(&self) -> usize {
        self.conversations
            .iter()
            .map(|conversation| conversation.nodes.len())
            .sum()
    }

    /// How many messages the records hold — the nodes that carry a `message`
    /// object. A node whose `message` is `null` is not a message (the root in
    /// every measured copy), and a node whose `message` this build could not read
    /// contributes nothing here: its slot is [`MessageSlot::Unreadable`], which is
    /// where that is named, so this is a count of the messages this build read.
    pub fn message_count(&self) -> usize {
        self.conversations
            .iter()
            .map(|conversation| {
                conversation
                    .nodes
                    .iter()
                    .filter(|node| matches!(node.message, MessageSlot::Message(_)))
                    .count()
            })
            .sum()
    }

    /// How many records are trees with more than one leaf: the conversations
    /// whose message count depends on which leaf is walked. 27-ORACLE §4.5
    /// reports the same number as `branch_ambiguous_sessions`.
    ///
    /// A conversation whose links do not resolve has no leaf count to compare and
    /// is counted by [`ExportParse::missing_node_conversations`] instead — never
    /// here, because "this tree has one leaf" is a claim a partial tree cannot
    /// make.
    pub fn branching_conversations(&self) -> usize {
        self.conversations
            .iter()
            .filter(|conversation| match &conversation.tree {
                TreeReading::Consistent(facts) => facts.leaves > 1,
                TreeReading::MissingNodes(_) | TreeReading::NoNodes => false,
            })
            .count()
    }

    /// How many records are conversations whose mapping names an id the object
    /// does not hold, or whose links cannot be walked to a root.
    pub fn missing_node_conversations(&self) -> usize {
        self.conversations
            .iter()
            .filter(|conversation| matches!(conversation.tree, TreeReading::MissingNodes(_)))
            .count()
    }

    /// How many records are conversations the export itself states have no nodes.
    pub fn empty_conversations(&self) -> usize {
        self.conversations
            .iter()
            .filter(|conversation| matches!(conversation.tree, TreeReading::NoNodes))
            .count()
    }
}

/// A conversation in the export that this build could not turn into a record.
///
/// It is reported, never dropped: the id (when the entry carried a readable one)
/// and the reason are both kept, so a caller can say *which* conversation it
/// could not read instead of reporting a number that looks complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationFailure {
    /// The platform's own conversation id, when the entry carried one. `None`
    /// means the entry did not identify itself.
    pub id: Option<String>,
    pub reason: ConversationFailureReason,
}

/// Why one conversation could not become a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationFailureReason {
    /// The array entry is not a JSON object.
    NotAnObject { found: &'static str },
    /// The entry names no readable conversation id, or names two different ones.
    /// Without one id nothing can be joined — ADR-055 D5 groups observations by
    /// conversation id — and picking one of two would be a guess, so there is no
    /// record to emit.
    IdNotReadable { why: String },
    /// The entry carries no `mapping` at all. Deliberately not the same as an
    /// empty tree: `"mapping": {}` is the export stating there are no nodes and
    /// reads into a record (see [`TreeReading::NoNodes`]), while an absent key is
    /// a conversation whose tree this build cannot see at all.
    MappingAbsent,
    /// The entry carries `"mapping": null`: the export states there is no tree
    /// here, and whether that means "no messages" or "withheld" is not something
    /// this build claims to know.
    MappingNull,
    /// The entry carries a `mapping` that is not an object.
    MappingNotAnObject { found: &'static str },
    /// A `mapping` entry is not a JSON object, so it is not a node. Refused
    /// rather than skipped: a tree with a hole where a node should be is a
    /// conversation this build did not read, and the id would not say which.
    NodeNotAnObject { found: &'static str },
}

/// One field of the export, in the four states that must stay apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field<T> {
    /// The export carries the field with this value.
    Value(T),
    /// The export carries the field as an explicit `null`: a stated absence,
    /// which is a different claim from the field not being there at all
    /// (ADR-055 D4 keeps nulls and omissions apart).
    Null,
    /// The export does not carry the field.
    Absent,
    /// The export carries the field in a shape this build cannot read. Where it
    /// happened is named in [`ExportParse::unreadable`] when it has no typed home
    /// of its own, so an unreadable field is never mistaken for one of the three
    /// states above.
    Unreadable { why: String },
}

/// A [`Field`] holding text.
pub type TextField = Field<String>;

/// The unit a numeric epoch was written in.
///
/// Named rather than assumed, because the same number means two different
/// instants in these two units — and a reading that silently picked one would be
/// wrong by three orders of magnitude on the other, with no way for a reader to
/// tell which had happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeUnit {
    /// Epoch seconds, which is what the measured copies carry.
    Seconds,
    /// Epoch milliseconds, divided by 1000 for the instant.
    Milliseconds,
}

/// A timestamp the export carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedTime {
    /// The instant the export names, and the unit its number was written in.
    ///
    /// `unix` is whole seconds: a fractional part — the measured copies carry
    /// fractional seconds — is dropped here and kept exactly in the export's own
    /// bytes, so nothing is lost that the capture needed to keep.
    Known { unix: i64, unit: TimeUnit },
    /// The timestamp could not be placed in time, with the reason — absent,
    /// `null`, not a number, or a number outside every range this build reads.
    /// Never `0`, which would be a measurement this build did not make.
    Unknown { why: String },
}

/// One conversation, as the export states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationRecord {
    /// The export's own conversation id, carried verbatim: it is the
    /// `<session id>` of our `chatgpt.<session id>` web-capture directory, so a
    /// caller joins on it and no id mapping is invented here. Whether it is safe
    /// to carry into a path is the producer's rule (`conversation_id_is_safe`,
    /// `import.rs`), applied where the bundle is written rather than here.
    pub id: String,
    /// `create_time`: when the platform says the conversation was created.
    pub created: RecordedTime,
    /// `update_time`: when the platform says it last changed.
    pub updated: RecordedTime,
    /// `title`. User content: the parser carries it, and nothing in this crate
    /// may log, report or commit it.
    pub title: TextField,
    /// The export's own `current_node`, read as the statement about itself that it
    /// is — see [`CurrentNodeReading`]. It is never used as "the" branch.
    pub current_node: CurrentNodeReading,
    /// Every mapping node the export carried, in the object's own order. The whole
    /// tree is kept: a node off whichever branch the UI last pointed at is a
    /// *kept branch*, not a dropped line (ADR-055 D2).
    pub nodes: Vec<NodeRecord>,
    /// What the object's own links say the tree is — see [`TreeReading`].
    pub tree: TreeReading,
}

impl ConversationRecord {
    /// The nodes the export marks as the tree's root: `"parent": null`, or no
    /// `parent` key at all. The two are kept apart on the node itself
    /// ([`ParentLink::Root`] / [`ParentLink::Absent`]) because they are different
    /// claims; both mean "no stated parent", which is what a root is.
    pub fn roots(&self) -> Vec<&NodeRecord> {
        self.nodes
            .iter()
            .filter(|node| matches!(node.parent, ParentLink::Root | ParentLink::Absent))
            .collect()
    }
}

/// One `mapping` entry.
///
/// The mapping is an object keyed by node id, so the **key** is the identity:
/// every `parent` link and `children` entry names a key. A node's own `id` field
/// is kept beside it as [`NodeRecord::stated_id`] and is not used to join
/// anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    /// The key this node is filed under, which is what links reference.
    pub id: String,
    /// The node's own `id` field, when it carries one. It equals [`NodeRecord::id`]
    /// on every measured node; when it does not, the disagreement is counted in
    /// [`ExportParse::unreadable`] rather than used.
    pub stated_id: TextField,
    /// Where this node sits in the tree, as the export spells it.
    pub parent: ParentLink,
    /// The node's `children`. Every entry names a key; an entry that names an id
    /// the object does not hold is what [`TreeReading::MissingNodes`] reports.
    pub children: Field<Vec<String>>,
    /// What this node says about its message.
    pub message: MessageSlot,
}

/// Where a node sits in the tree, as the export spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentLink {
    /// `"parent": null` — a stated root.
    Root,
    /// The node carries no `parent` key. Also a root for the walk, and a
    /// different claim from [`ParentLink::Root`]: the export did not say.
    Absent,
    /// The key this node descends from.
    Node(String),
    /// `parent` is carried in a shape this build cannot read as an id.
    Unreadable { why: String },
}

/// What a `mapping` node says about its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageSlot {
    /// The node carries a message object.
    Message(Box<MessageRecord>),
    /// `"message": null` — the export states this node has no message. The root
    /// node of every measured conversation is one of these.
    None,
    /// The node carries no `message` key at all.
    Absent,
    /// `message` is carried in a shape this build cannot read.
    Unreadable { why: String },
}

/// One message: a node's `message` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    /// `id` of the message, when the export carries one.
    pub id: TextField,
    /// `create_time`, in the unit the export wrote it in.
    pub time: RecordedTime,
    /// `update_time`. `null` on most messages of the larger measured copy — a
    /// stated absence, and read as one rather than as "unchanged since creation".
    pub updated: RecordedTime,
    /// `author.role` — the only thing this build reads `author` for.
    pub author_role: TextField,
    /// `author.name`, carried because it is there; a role is not inferred from it.
    pub author_name: TextField,
    /// `recipient`, carried verbatim (`"all"` on the measured messages).
    pub recipient: TextField,
    /// `content.content_type` — the label the export puts on its own content, kept
    /// as a label and never turned into a role by this module.
    pub content_type: TextField,
    /// `content.parts`, one entry per element. A list that cannot be read at all
    /// is [`Field::Unreadable`]; an element that is not a string is *read* as a
    /// structured part rather than dropped, because a multimodal element is
    /// content the export did carry.
    pub parts: Field<Vec<PartReading>>,
}

impl MessageRecord {
    /// The message's text: the string parts, concatenated, in the order they
    /// appear. `None` when `parts` is `null`, absent or unreadable — three
    /// different reasons, each kept on the field itself — and `Some("")` when the
    /// export states an empty list, which is a measured zero.
    ///
    /// Only string parts contribute. That is the same rule the oracle's own
    /// comparison uses (27-ORACLE §4.5's axis reads `parts` and joins its
    /// strings), so the character count below is the count that comparison
    /// measures.
    pub fn text(&self) -> Option<String> {
        match &self.parts {
            Field::Value(parts) => {
                let mut text = String::new();
                for part in parts {
                    if let PartReading::Text(piece) = part {
                        text.push_str(piece);
                    }
                }
                Some(text)
            }
            Field::Null | Field::Absent | Field::Unreadable { .. } => None,
        }
    }

    /// Whether this message is a *turn*: one of the four roles a conversation is
    /// made of, in the vocabulary both the export and the oracle's comparison use.
    /// A message whose role this build could not read is not a turn — it is a
    /// message whose role was not read, which is named on the field.
    pub fn is_a_turn(&self) -> bool {
        match &self.author_role {
            Field::Value(role) => matches!(role.as_str(), "user" | "assistant" | "system" | "tool"),
            Field::Null | Field::Absent | Field::Unreadable { .. } => false,
        }
    }
}

/// One `content.parts[]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartReading {
    /// A string part: a piece of the message's text.
    Text(String),
    /// An element that is not a string, kept by the JSON kind it is. A multimodal
    /// part is one of these; so is anything else this build does not interpret,
    /// and `found` is what makes the two tellable apart in a report.
    Structured { found: &'static str },
}

/// What a conversation's own links say the tree is.
///
/// Three arms, three claims. `Consistent` is a statement about the links, never
/// about the fetch: 27-ORACLE §4.5 records that a partially fetched mapping
/// cannot be ruled out from structure alone, because a truncated fetch also
/// presents leaves — so no arm here is named *complete*, and the counts below
/// describe the object as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeReading {
    /// Every id the object names resolves inside it, and the walk reaches a root:
    /// the tree can be walked, and [`TreeFacts::longest_branch`] is the branch
    /// 27-ORACLE §4.5 measures on.
    Consistent(TreeFacts),
    /// At least one id the object names is not in the object, or a link cannot be
    /// walked to a root. There is **no branch reading** in this arm, deliberately:
    /// which branch is "the" conversation is exactly what a partial tree cannot
    /// answer, and the leaves such a tree presents are not the conversation's
    /// leaves (§4.5). The counts it does carry are counts of what is here.
    MissingNodes(MissingNodes),
    /// The mapping holds no nodes: the export states this conversation has nothing
    /// in it. A measurement, and a different claim from a mapping that is absent
    /// (a [`ConversationFailureReason::MappingAbsent`]).
    NoNodes,
}

/// What a tree this build could walk holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFacts {
    /// Every mapping node the object holds.
    pub nodes: usize,
    /// The nodes whose `message` is an object — see [`ExportParse::message_count`].
    pub messages: usize,
    /// Nodes that name no parent (a stated `null`, or no `parent` key).
    pub roots: usize,
    /// Nodes no other node names as its parent. More than one means the message
    /// count depends on which leaf is walked, which is what
    /// [`ExportParse::branching_conversations`] counts.
    pub leaves: usize,
    /// The branch with the most text, which is the reading §4.5 measures on.
    pub longest_branch: Branch,
    /// The branch the export's own `current_node` walks, when it names a node the
    /// mapping holds. `None` when it names none (see the record's
    /// [`ConversationRecord::current_node`]), which is also the case for a
    /// conversation whose export states no current node at all.
    pub current_branch: Option<Branch>,
}

impl TreeFacts {
    /// Whether the export's own `current_node` walks to the same end as the
    /// longest branch — 27-ORACLE §4.5's fact, and the one that made a
    /// `current_node`-only reading report a false RED. `None` when the export
    /// names no current node this build could walk.
    pub fn current_node_is_the_longest_leaf(&self) -> Option<bool> {
        self.current_branch
            .as_ref()
            .map(|branch| branch.end == self.longest_branch.end)
    }
}

/// One branch of a tree: the walk from an endpoint up to its root, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// The node the walk started from. For `longest_branch` this is a leaf; for
    /// the branch through `current_node` it is whatever that field names, because
    /// the export does not promise its UI pointed at a leaf.
    pub end: String,
    /// How many nodes the walk passes through.
    pub nodes: usize,
    /// How many of them are messages that carry text and whose role is one of the
    /// four a conversation is made of — the list the oracle's comparison reads.
    pub turns: usize,
    /// The text characters those turns hold, on the platform's own definition of
    /// a message's text ([`MessageRecord::text`]): the axis the oracle compares
    /// on, and the reason a longer *branch* is preferred to a longer *node list*.
    pub characters: usize,
}

impl Branch {
    /// The order two branches are compared in: characters first, then turns, then
    /// nodes. Written as one function so the comparison used to pick
    /// [`TreeFacts::longest_branch`] is not a second, quieter definition of what
    /// "longest" means.
    fn rank(&self) -> (usize, usize, usize) {
        (self.characters, self.turns, self.nodes)
    }
}

/// A tree whose own links do not all resolve.
///
/// This is the partial-fetch state, and it exists because "how many messages does
/// this conversation have" is not a question a tree with a hole in it can answer:
/// a count taken here would be a smaller number that looks complete (ADR-014).
/// Every field is a count of what the object *does* hold, or a count of the links
/// that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingNodes {
    /// How many nodes the object holds.
    pub nodes: usize,
    /// How many of them carry a `message` object.
    pub messages: usize,
    /// `parent` links that do not resolve: an id the object does not hold, or a
    /// value this build could not read as an id.
    pub unresolved_parents: usize,
    /// `children` entries that do not resolve, the same two ways.
    pub unresolved_children: usize,
    /// The export's `current_node` names an id the object does not hold.
    pub unresolved_current_node: bool,
    /// Some node's ancestor chain revisits a node, so no walk reaches a root.
    /// A cycle is not a tree, and nothing is guessed about which node was meant.
    pub cyclic: bool,
}

impl MissingNodes {
    /// Whether every link the object states resolved, so there was nothing to
    /// report. This module cannot construct a `MissingNodes` for which this is
    /// true — it is what a caller asserts a partial reading on, and what makes
    /// the arm's name a claim rather than a label.
    pub fn everything_resolved(&self) -> bool {
        self.unresolved_parents == 0
            && self.unresolved_children == 0
            && !self.unresolved_current_node
            && !self.cyclic
    }
}

/// The export's own `current_node`, and whether the mapping holds what it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentNodeReading {
    /// The export names this node, and the mapping holds it.
    Named { id: String },
    /// The field is absent: the export names no current node.
    Absent,
    /// The field is `null`: the export states there is none.
    Null,
    /// The field is carried in a shape this build cannot read as an id.
    Unreadable { why: String },
    /// The export names a node the mapping does not hold — an unresolved link like
    /// any other, and the reason the tree reads as
    /// [`TreeReading::MissingNodes`].
    Unresolved { id: String },
}

/// Collects the records, the failures and the unreadable tally as one export is
/// read, so the readers below can note what they could not read without carrying
/// a tally through every call.
#[derive(Default)]
struct Reader {
    conversations: Vec<ConversationRecord>,
    failures: Vec<ConversationFailure>,
    unreadable: BTreeMap<String, usize>,
}

impl Reader {
    fn finish(self) -> ExportParse {
        ExportParse {
            conversations: self.conversations,
            failures: self.failures,
            unreadable: self.unreadable,
        }
    }

    /// Count one spot where the export carried something this build did not read.
    fn note(&mut self, path: &str) {
        *self.unreadable.entry(path.to_string()).or_insert(0) += 1;
    }

    /// Every field name at one level that the row for that level does not carry.
    fn note_unrecognised(&mut self, object: &Map<String, Value>, known: &[&str], path: &str) {
        for name in object.keys() {
            if !known.contains(&name.as_str()) {
                self.note(&format!("{path}.{name}"));
            }
        }
    }

    /// One field carried in a shape this build cannot read. The record's own
    /// field is its typed home, so the spot is named on the field and not in the
    /// tally; the path stays in the reason so a report can still say where in the
    /// export it happened.
    fn wrong_shape<T>(&mut self, path: &str, key: &str, carried: &str) -> Field<T> {
        Field::Unreadable {
            why: format!("{path}.{key}: `{key}` is carried as {carried}"),
        }
    }

    fn text(&mut self, object: &Map<String, Value>, key: &str, path: &str) -> TextField {
        match object.get(key) {
            None => Field::Absent,
            Some(Value::Null) => Field::Null,
            Some(Value::String(text)) => Field::Value(text.clone()),
            Some(other) => self.wrong_shape(path, key, kind_of(other)),
        }
    }

    /// A list of strings, one entry per readable element. An entry this build
    /// cannot read is counted in [`ExportParse::unreadable`] and left out, which
    /// makes the list a record holds the list this build *read*; the field's own
    /// states — absent, `null`, unreadable — stay on the field.
    fn string_list(
        &mut self,
        object: &Map<String, Value>,
        key: &str,
        path: &str,
    ) -> Field<Vec<String>> {
        match object.get(key) {
            None => Field::Absent,
            Some(Value::Null) => Field::Null,
            Some(Value::Array(items)) => {
                let entry = format!("{path}.{key}[]");
                let mut strings = Vec::with_capacity(items.len());
                for item in items {
                    match item.as_str() {
                        Some(text) => strings.push(text.to_string()),
                        None => self.note(&entry),
                    }
                }
                Field::Value(strings)
            }
            Some(other) => self.wrong_shape(path, key, kind_of(other)),
        }
    }

    fn recorded_time(
        &mut self,
        object: &Map<String, Value>,
        key: &str,
        path: &str,
    ) -> RecordedTime {
        match object.get(key) {
            None => RecordedTime::Unknown {
                why: format!("`{key}` is absent"),
            },
            Some(Value::Null) => RecordedTime::Unknown {
                why: format!("`{key}` is null"),
            },
            Some(Value::Number(number)) => match epoch_seconds(number) {
                Some((unix, unit)) => RecordedTime::Known { unix, unit },
                None => {
                    self.note(&format!("{path}.{key}"));
                    RecordedTime::Unknown {
                        why: format!(
                            "`{key}` is a number outside the seconds and milliseconds ranges \
                             this build reads, so its unit is not a guess this build will make"
                        ),
                    }
                }
            },
            Some(other) => {
                self.note(&format!("{path}.{key}"));
                RecordedTime::Unknown {
                    why: format!("`{key}` is carried as {}", kind_of(other)),
                }
            }
        }
    }

    /// One conversation of the file. It either becomes a record or a named
    /// failure; there is no third outcome that says nothing.
    fn conversation(&mut self, entry: &Value) {
        let Value::Object(object) = entry else {
            self.failures.push(ConversationFailure {
                id: None,
                reason: ConversationFailureReason::NotAnObject {
                    found: kind_of(entry),
                },
            });
            return;
        };
        self.note_unrecognised(object, &CONVERSATION_KEYS, CONVERSATION);

        let id = match conversation_id(object) {
            Ok(id) => id,
            Err(reason) => {
                self.failures.push(ConversationFailure { id: None, reason });
                return;
            }
        };

        let nodes = match object.get("mapping") {
            None => return self.fail(id, ConversationFailureReason::MappingAbsent),
            Some(Value::Null) => return self.fail(id, ConversationFailureReason::MappingNull),
            Some(Value::Object(mapping)) => match self.nodes(mapping) {
                Ok(nodes) => nodes,
                Err(reason) => return self.fail(id, reason),
            },
            Some(other) => {
                return self.fail(
                    id,
                    ConversationFailureReason::MappingNotAnObject {
                        found: kind_of(other),
                    },
                )
            }
        };

        let mut current_node = match object.get("current_node") {
            None => CurrentNodeReading::Absent,
            Some(Value::Null) => CurrentNodeReading::Null,
            Some(Value::String(node)) => CurrentNodeReading::Named { id: node.clone() },
            Some(other) => CurrentNodeReading::Unreadable {
                why: format!("`current_node` is carried as {}", kind_of(other)),
            },
        };
        let tree = read_tree(&nodes, &mut current_node);

        // Read one field at a time: each read may note something in the tally, so
        // the record is assembled from locals rather than inside the push.
        let created = self.recorded_time(object, "create_time", CONVERSATION);
        let updated = self.recorded_time(object, "update_time", CONVERSATION);
        let title = self.text(object, "title", CONVERSATION);
        self.conversations.push(ConversationRecord {
            id,
            created,
            updated,
            title,
            current_node,
            nodes,
            tree,
        });
    }

    fn fail(&mut self, id: String, reason: ConversationFailureReason) {
        self.failures.push(ConversationFailure {
            id: Some(id),
            reason,
        });
    }

    fn nodes(
        &mut self,
        mapping: &Map<String, Value>,
    ) -> Result<Vec<NodeRecord>, ConversationFailureReason> {
        let mut nodes = Vec::with_capacity(mapping.len());
        for (key, value) in mapping {
            let Value::Object(object) = value else {
                return Err(ConversationFailureReason::NodeNotAnObject {
                    found: kind_of(value),
                });
            };
            self.note_unrecognised(object, &NODE_KEYS, NODE);
            let stated_id = self.text(object, "id", NODE);
            if let TextField::Value(stated) = &stated_id {
                if stated != key {
                    // Links use the key, so a field that disagrees with it is a
                    // value this build did not read — counted, never used.
                    self.note(&format!("{NODE}.id"));
                }
            }
            let parent = match object.get("parent") {
                None => ParentLink::Absent,
                Some(Value::Null) => ParentLink::Root,
                Some(Value::String(parent)) => ParentLink::Node(parent.clone()),
                Some(other) => ParentLink::Unreadable {
                    why: format!("`parent` is carried as {}", kind_of(other)),
                },
            };
            let children = self.string_list(object, "children", NODE);
            let message = match object.get("message") {
                None => MessageSlot::Absent,
                Some(Value::Null) => MessageSlot::None,
                Some(Value::Object(message)) => {
                    MessageSlot::Message(Box::new(self.message(message)))
                }
                Some(other) => MessageSlot::Unreadable {
                    why: format!("`message` is carried as {}", kind_of(other)),
                },
            };
            nodes.push(NodeRecord {
                id: key.clone(),
                stated_id,
                parent,
                children,
                message,
            });
        }
        Ok(nodes)
    }

    fn message(&mut self, object: &Map<String, Value>) -> MessageRecord {
        self.note_unrecognised(object, &MESSAGE_KEYS, MESSAGE);

        let (author_role, author_name) = match object.get("author") {
            None => (TextField::Absent, TextField::Absent),
            Some(Value::Null) => (TextField::Null, TextField::Null),
            Some(Value::Object(author)) => {
                self.note_unrecognised(author, &AUTHOR_KEYS, AUTHOR);
                (
                    self.text(author, "role", AUTHOR),
                    self.text(author, "name", AUTHOR),
                )
            }
            Some(other) => {
                self.note(AUTHOR);
                let why = format!("`author` is carried as {}", kind_of(other));
                (
                    Field::Unreadable { why: why.clone() },
                    Field::Unreadable { why },
                )
            }
        };

        let (content_type, parts) = match object.get("content") {
            None => (TextField::Absent, Field::Absent),
            Some(Value::Null) => (TextField::Null, Field::Null),
            Some(Value::Object(content)) => {
                self.note_unrecognised(content, &CONTENT_KEYS, CONTENT);
                (
                    self.text(content, "content_type", CONTENT),
                    self.parts(content),
                )
            }
            Some(other) => {
                self.note(CONTENT);
                let why = format!("`content` is carried as {}", kind_of(other));
                (
                    Field::Unreadable { why: why.clone() },
                    Field::Unreadable { why },
                )
            }
        };

        MessageRecord {
            id: self.text(object, "id", MESSAGE),
            time: self.recorded_time(object, "create_time", MESSAGE),
            updated: self.recorded_time(object, "update_time", MESSAGE),
            author_role,
            author_name,
            recipient: self.text(object, "recipient", MESSAGE),
            content_type,
            parts,
        }
    }

    fn parts(&mut self, content: &Map<String, Value>) -> Field<Vec<PartReading>> {
        match content.get("parts") {
            None => Field::Absent,
            Some(Value::Null) => Field::Null,
            Some(Value::Array(items)) => {
                let mut parts = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Value::String(text) => parts.push(PartReading::Text(text.clone())),
                        // Read, not dropped: the element is content the export
                        // carried, and its JSON kind is what tells a report what
                        // kind of content it was.
                        other => parts.push(PartReading::Structured {
                            found: kind_of(other),
                        }),
                    }
                }
                Field::Value(parts)
            }
            Some(other) => self.wrong_shape(CONTENT, "parts", kind_of(other)),
        }
    }
}

/// The conversation id the export states, or why it does not state one.
///
/// Two spellings are accepted because the two shapes in the wild use one each; a
/// conversation that carries both must agree with itself, and this build joins on
/// nothing when they do not — either choice would be a guess about which id names
/// the conversation, and ADR-055 D5 groups observations by that id.
fn conversation_id(object: &Map<String, Value>) -> Result<String, ConversationFailureReason> {
    let mut stated: Vec<(&str, &str)> = Vec::new();
    let mut null: Vec<&str> = Vec::new();
    for key in ID_KEYS {
        match object.get(key) {
            None => {}
            // A null is a stated absence: the ladder moves past it, and when
            // nothing is stated at all it is what the refusal names.
            Some(Value::Null) => null.push(key),
            Some(Value::String(id)) => {
                if id.is_empty() {
                    return Err(ConversationFailureReason::IdNotReadable {
                        why: format!("`{key}` is an empty string"),
                    });
                }
                stated.push((key, id.as_str()));
            }
            Some(other) => {
                return Err(ConversationFailureReason::IdNotReadable {
                    why: format!("`{key}` is carried as {}", kind_of(other)),
                })
            }
        }
    }
    if stated.is_empty() {
        return Err(ConversationFailureReason::IdNotReadable {
            why: if null.is_empty() {
                format!("carries neither `{}` nor `{}`", ID_KEYS[0], ID_KEYS[1])
            } else {
                format!("`{}` is null", null.join("`, `"))
            },
        });
    }
    let first = stated[0];
    if let Some(second) = stated.get(1) {
        if first.1 != second.1 {
            return Err(ConversationFailureReason::IdNotReadable {
                why: format!(
                    "`{}` and `{}` disagree, and this build does not choose between two ids",
                    first.0, second.0
                ),
            });
        }
    }
    Ok(first.1.to_string())
}

/// What a conversation's own links say, and the resolution of its `current_node`
/// (which is one of those links and is written back in place).
fn read_tree(nodes: &[NodeRecord], current: &mut CurrentNodeReading) -> TreeReading {
    let mut index: BTreeMap<&str, usize> = BTreeMap::new();
    for (position, node) in nodes.iter().enumerate() {
        index.insert(node.id.as_str(), position);
    }

    let mut unresolved_parents = 0usize;
    let mut unresolved_children = 0usize;
    let mut named_as_parent: BTreeSet<&str> = BTreeSet::new();
    for node in nodes {
        match &node.parent {
            ParentLink::Node(parent) => {
                if index.contains_key(parent.as_str()) {
                    named_as_parent.insert(parent.as_str());
                } else {
                    unresolved_parents += 1;
                }
            }
            ParentLink::Unreadable { .. } => unresolved_parents += 1,
            ParentLink::Root | ParentLink::Absent => {}
        }
        if let Field::Value(children) = &node.children {
            for child in children {
                if !index.contains_key(child.as_str()) {
                    unresolved_children += 1;
                }
            }
        }
    }

    // An unreadable `parent` is an unresolved link; a `children` list this build
    // could not read is named on the field and not counted here, because a list
    // that was not read cannot say whether it named anything absent.
    let unresolved_current_node = match &*current {
        CurrentNodeReading::Named { id } => !index.contains_key(id.as_str()),
        _ => false,
    };
    if unresolved_current_node {
        if let CurrentNodeReading::Named { id } = &*current {
            let id = id.clone();
            *current = CurrentNodeReading::Unresolved { id };
        }
    }

    let nodes_count = nodes.len();
    let messages = nodes
        .iter()
        .filter(|node| matches!(node.message, MessageSlot::Message(_)))
        .count();
    let cyclic = has_a_cycle(nodes, &index);

    if unresolved_parents > 0 || unresolved_children > 0 || unresolved_current_node || cyclic {
        return TreeReading::MissingNodes(MissingNodes {
            nodes: nodes_count,
            messages,
            unresolved_parents,
            unresolved_children,
            unresolved_current_node,
            cyclic,
        });
    }
    if nodes.is_empty() {
        return TreeReading::NoNodes;
    }

    let roots = nodes
        .iter()
        .filter(|node| matches!(node.parent, ParentLink::Root | ParentLink::Absent))
        .count();
    let leaves: BTreeSet<&str> = index
        .keys()
        .copied()
        .filter(|id| !named_as_parent.contains(id))
        .collect();
    // A mapping with nodes, no unresolved link and no cycle always has at least one
    // node no other node names as a parent: if every node had a child the parent
    // relation would have to contain a cycle. So this iterator is not empty in the
    // reading this module publishes — with the missing-link test above disabled it
    // becomes reachable (a cyclic mapping then falls through to here), which is how
    // the unit tests under this module were shown to fail rather than pass.
    let longest_branch = match leaves
        .iter()
        .map(|leaf| branch_of(nodes, &index, leaf))
        .max_by(|left, right| left.rank().cmp(&right.rank()))
    {
        Some(branch) => branch,
        None => return TreeReading::NoNodes,
    };

    let current_branch = match &*current {
        CurrentNodeReading::Named { id } => Some(branch_of(nodes, &index, id)),
        _ => None,
    };

    TreeReading::Consistent(TreeFacts {
        nodes: nodes_count,
        messages,
        roots,
        leaves: leaves.len(),
        longest_branch,
        current_branch,
    })
}

/// Whether some node's ancestor chain revisits a node, so no walk from it reaches
/// a root. A cycle is not a tree; nothing is guessed about which node was meant,
/// and every node it touches is reported through the `MissingNodes` state.
fn has_a_cycle(nodes: &[NodeRecord], index: &BTreeMap<&str, usize>) -> bool {
    for node in nodes {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        seen.insert(node.id.as_str());
        let mut cursor = match &node.parent {
            ParentLink::Node(parent) => Some(parent.as_str()),
            _ => None,
        };
        while let Some(id) = cursor {
            let Some(position) = index.get(id) else { break };
            if !seen.insert(id) {
                return true;
            }
            cursor = match &nodes[*position].parent {
                ParentLink::Node(parent) => Some(parent.as_str()),
                _ => None,
            };
        }
    }
    false
}

/// The branch that ends at `end`: the walk up to the root, oldest first.
///
/// This is the oracle's own reading (27-ORACLE §4.5's axis — `_chatgpt_chain` and
/// its text rule), reproduced here so that the count this module publishes and the
/// count the import's acceptance measures are the same quantity. One difference,
/// stated: the oracle's walk silently stops when a parent names a node the
/// mapping does not hold, and this module reports that as
/// [`TreeReading::MissingNodes`] instead — a partial tree is a state, not a
/// shorter branch.
fn branch_of(nodes: &[NodeRecord], index: &BTreeMap<&str, usize>, end: &str) -> Branch {
    let mut chain: Vec<&NodeRecord> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut cursor = Some(end);
    while let Some(id) = cursor {
        let Some(position) = index.get(id) else { break };
        if !seen.insert(id) {
            break;
        }
        let node = &nodes[*position];
        chain.push(node);
        cursor = match &node.parent {
            ParentLink::Node(parent) => Some(parent.as_str()),
            ParentLink::Root | ParentLink::Absent | ParentLink::Unreadable { .. } => None,
        };
    }

    let mut turns = 0usize;
    let mut characters = 0usize;
    for node in &chain {
        let MessageSlot::Message(message) = &node.message else {
            continue;
        };
        if !message.is_a_turn() {
            continue;
        }
        let Some(text) = message.text() else { continue };
        if text.trim().is_empty() {
            continue;
        }
        turns += 1;
        characters += text.chars().count();
    }

    Branch {
        end: end.to_string(),
        nodes: chain.len(),
        turns,
        characters,
    }
}

/// Read a numeric epoch as unix seconds, naming the unit its magnitude says it is
/// written in.
///
/// The two windows are the two units this slice is specified for; a magnitude
/// outside both is `None` — a value this build does not guess at. The windows are
/// `crate::activity`'s own plausible conversation-timestamp range, so the crate
/// reads one epoch one way; the difference is that this reader also says *which*
/// unit it found, which the unit is not recoverable from afterwards otherwise.
fn epoch_seconds(number: &serde_json::Number) -> Option<(i64, TimeUnit)> {
    let whole = number
        .as_i64()
        .or_else(|| {
            let value = number.as_f64()?;
            if value.is_finite() && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
                Some(value.trunc() as i64)
            } else {
                None
            }
        })
        .filter(|value| *value > 0)?;

    if (MIN_PLAUSIBLE_SECONDS..=MAX_PLAUSIBLE_SECONDS).contains(&whole) {
        Some((whole, TimeUnit::Seconds))
    } else if (MIN_PLAUSIBLE_SECONDS * 1000..=MAX_PLAUSIBLE_SECONDS * 1000).contains(&whole) {
        Some((whole / 1000, TimeUnit::Milliseconds))
    } else {
        None
    }
}

/// The JSON kind of a value, for a report. A kind, never a value.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------- fixtures
    //
    // Synthetic, and shaped like the measured files: reserved `fixture-` tokens
    // only, no real id, title or text anywhere. A node's key is its own id, which
    // is how the export files its mapping.

    const ID: &str = "fixture-conversation";
    /// A fractional epoch second (2023-11-14), inside the plausible window.
    const SECOND: f64 = 1_700_000_000.5;

    /// One mapping node, as the measured shape spells it.
    fn node(id: &str, parent: Value, children: Value, message: Value) -> Value {
        serde_json::json!({
            "id": id,
            "parent": parent,
            "children": children,
            "message": message,
        })
    }

    /// A mapping object, keyed by each node's own id.
    fn mapping(nodes: Vec<Value>) -> Value {
        let mut object = Map::new();
        for entry in nodes {
            let key = entry["id"]
                .as_str()
                .expect("a fixture node names itself")
                .to_string();
            object.insert(key, entry);
        }
        Value::Object(object)
    }

    /// A message carrying the keys the measured shape carries.
    fn message(role: Value, parts: Value) -> Value {
        serde_json::json!({
            "id": "fixture-message",
            "author": { "role": role, "name": null },
            "content": { "content_type": "text", "parts": parts },
            "create_time": SECOND,
            "recipient": "all",
            "update_time": null,
        })
    }

    /// A conversation object carrying the six keys this build reads.
    fn conversation(id: &str, mapping: Value, current_node: Value) -> Value {
        serde_json::json!({
            "conversation_id": id,
            "title": "fixture title",
            "create_time": SECOND,
            "update_time": SECOND,
            "current_node": current_node,
            "mapping": mapping,
        })
    }

    /// The file's bytes for a list of conversations.
    fn export(conversations: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&Value::Array(conversations)).expect("fixtures serialise")
    }

    /// One conversation read out of its own bytes, which is also the shape both
    /// measured copies have.
    fn read(conversation: Value) -> ConversationRecord {
        let parsed = parse_export(&serde_json::to_vec(&conversation).expect("fixture")).unwrap();
        assert!(parsed.failures.is_empty(), "{:?}", parsed.failures);
        assert_eq!(parsed.conversations.len(), 1);
        parsed.conversations.into_iter().next().unwrap()
    }

    /// `root -> user -> assistant`: the shape every measured conversation has, one
    /// root whose `message` is `null`.
    fn linear(id: &str) -> Value {
        conversation(
            id,
            mapping(vec![
                node(
                    "fixture-root",
                    Value::Null,
                    serde_json::json!(["fixture-user"]),
                    Value::Null,
                ),
                node(
                    "fixture-user",
                    serde_json::json!("fixture-root"),
                    serde_json::json!(["fixture-assistant"]),
                    message(
                        serde_json::json!("user"),
                        serde_json::json!(["fixture prompt"]),
                    ),
                ),
                node(
                    "fixture-assistant",
                    serde_json::json!("fixture-user"),
                    serde_json::json!([]),
                    message(
                        serde_json::json!("assistant"),
                        serde_json::json!(["fixture answer"]),
                    ),
                ),
            ]),
            serde_json::json!("fixture-assistant"),
        )
    }

    /// A conversation with two leaves whose `current_node` is the **shorter** one:
    /// 27-ORACLE §4.5's measured shape, and the one that made a `current_node`-only
    /// reading report a false RED.
    fn branching(id: &str) -> Value {
        conversation(
            id,
            mapping(vec![
                node(
                    "fixture-root",
                    Value::Null,
                    serde_json::json!(["fixture-user"]),
                    Value::Null,
                ),
                node(
                    "fixture-user",
                    serde_json::json!("fixture-root"),
                    serde_json::json!(["fixture-short", "fixture-long"]),
                    message(
                        serde_json::json!("user"),
                        serde_json::json!(["fixture prompt"]),
                    ),
                ),
                node(
                    "fixture-short",
                    serde_json::json!("fixture-user"),
                    serde_json::json!([]),
                    message(
                        serde_json::json!("assistant"),
                        serde_json::json!(["fixture short"]),
                    ),
                ),
                node(
                    "fixture-long",
                    serde_json::json!("fixture-user"),
                    serde_json::json!([]),
                    message(
                        serde_json::json!("assistant"),
                        serde_json::json!(["fixture much longer answer"]),
                    ),
                ),
            ]),
            serde_json::json!("fixture-short"),
        )
    }

    fn facts(record: &ConversationRecord) -> &TreeFacts {
        match &record.tree {
            TreeReading::Consistent(facts) => facts,
            other => panic!("expected a walked tree, got {other:?}"),
        }
    }

    fn insert(object: &mut Value, key: &str, value: Value) {
        object
            .as_object_mut()
            .expect("a fixture conversation is an object")
            .insert(key.to_string(), value);
    }

    // --------------------------------------------------------- the whole file

    #[test]
    fn a_well_formed_export_reads_into_one_record() {
        let record = read(linear(ID));
        assert_eq!(record.id, ID);
        assert_eq!(
            record.created,
            RecordedTime::Known {
                unix: SECOND as i64,
                unit: TimeUnit::Seconds,
            }
        );
        assert_eq!(record.title, TextField::Value("fixture title".to_string()));
        assert_eq!(
            record.current_node,
            CurrentNodeReading::Named {
                id: "fixture-assistant".to_string()
            }
        );
        assert_eq!(record.nodes.len(), 3);
        assert_eq!(record.roots().len(), 1, "one node names no parent");

        let facts = facts(&record);
        assert_eq!((facts.nodes, facts.messages), (3, 2));
        assert_eq!((facts.roots, facts.leaves), (1, 1));
        assert_eq!(facts.longest_branch.end, "fixture-assistant");
        assert_eq!(facts.longest_branch.nodes, 3);
        assert_eq!(facts.longest_branch.turns, 2);
        assert_eq!(
            facts.longest_branch.characters,
            "fixture prompt".len() + "fixture answer".len()
        );
        assert_eq!(facts.current_node_is_the_longest_leaf(), Some(true));
    }

    #[test]
    fn a_bulk_export_of_several_conversations_keeps_the_file_s_order() {
        let parsed = parse_export(&export(vec![linear(ID), branching("fixture-two")])).unwrap();
        assert_eq!(parsed.conversations.len(), 2);
        assert_eq!(parsed.conversations[0].id, ID);
        assert_eq!(parsed.conversations[1].id, "fixture-two");
        assert_eq!(parsed.node_count(), 7);
        assert_eq!(parsed.message_count(), 5);
    }

    /// 27-ORACLE §4.5 as an assertion: the reading is the longest branch, and the
    /// export's own `current_node` is carried beside it rather than used as the
    /// conversation.
    #[test]
    fn the_reading_is_the_longest_branch_and_not_the_current_node() {
        let record = read(branching(ID));
        let facts = facts(&record);
        assert_eq!(facts.leaves, 2, "a branch-ambiguous conversation");
        assert_eq!(facts.longest_branch.end, "fixture-long");
        assert_eq!(facts.longest_branch.turns, 2);
        assert_eq!(
            facts.longest_branch.characters,
            "fixture prompt".len() + "fixture much longer answer".len()
        );
        assert_eq!(
            facts
                .current_branch
                .as_ref()
                .map(|branch| branch.end.as_str()),
            Some("fixture-short"),
            "the export's own pointer is kept, and is the shorter leaf"
        );
        assert_eq!(facts.current_node_is_the_longest_leaf(), Some(false));
    }

    /// A tie has to be broken reproducibly, or two runs would publish two readings
    /// of one file. The larger end id wins, and the leaf count stays in the reading
    /// so the ambiguity is visible.
    #[test]
    fn two_equal_branches_are_compared_the_same_way_every_time() {
        let tie = conversation(
            ID,
            mapping(vec![
                node(
                    "fixture-root",
                    Value::Null,
                    serde_json::json!(["fixture-a", "fixture-b"]),
                    Value::Null,
                ),
                node(
                    "fixture-a",
                    serde_json::json!("fixture-root"),
                    serde_json::json!([]),
                    message(
                        serde_json::json!("user"),
                        serde_json::json!(["fixture same"]),
                    ),
                ),
                node(
                    "fixture-b",
                    serde_json::json!("fixture-root"),
                    serde_json::json!([]),
                    message(
                        serde_json::json!("user"),
                        serde_json::json!(["fixture same"]),
                    ),
                ),
            ]),
            serde_json::json!("fixture-a"),
        );
        let record = read(tie);
        let facts = facts(&record);
        assert_eq!(facts.leaves, 2);
        assert_eq!(facts.longest_branch.end, "fixture-b");
        assert_eq!(facts.longest_branch.turns, 1);
    }

    // ------------------------------------------------------------- timestamps

    #[test]
    fn an_epoch_in_milliseconds_is_read_and_named() {
        let in_seconds = read(linear(ID));
        assert_eq!(
            in_seconds.created,
            RecordedTime::Known {
                unix: SECOND as i64,
                unit: TimeUnit::Seconds,
            }
        );

        let mut object = linear(ID);
        insert(
            &mut object,
            "create_time",
            serde_json::json!((SECOND * 1000.0).round()),
        );
        let in_millis = read(object);
        assert_eq!(
            in_millis.created,
            RecordedTime::Known {
                unix: SECOND as i64,
                unit: TimeUnit::Milliseconds,
            },
            "the same instant in the other unit, with the unit said out loud"
        );
    }

    #[test]
    fn a_fractional_second_keeps_the_second_it_names() {
        let record = read(linear(ID));
        let RecordedTime::Known { unix, unit } = record.created else {
            panic!("a measured create_time is an instant: {:?}", record.created);
        };
        assert_eq!(unit, TimeUnit::Seconds);
        assert_eq!(unix, 1_700_000_000, "the second the fraction belongs to");
    }

    /// Invariant 1 on the time axis: a number outside every range this build reads
    /// is `unknown` with a reason, never `0` — a zero would be a measurement.
    #[test]
    fn a_number_outside_both_ranges_is_unknown_and_never_zero() {
        for value in [
            serde_json::json!(1_600),
            serde_json::json!(-1),
            serde_json::json!(9.9e15),
        ] {
            let mut object = linear(ID);
            insert(&mut object, "create_time", value);
            match read(object).created {
                RecordedTime::Unknown { why } => {
                    assert!(why.contains("outside the seconds"), "{why}")
                }
                other => panic!("expected unknown, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_null_time_and_an_absent_time_are_two_reasons_and_neither_is_zero() {
        let mut null = linear(ID);
        insert(&mut null, "update_time", Value::Null);
        match read(null).updated {
            RecordedTime::Unknown { why } => assert!(why.contains("null"), "{why}"),
            other => panic!("expected unknown, got {other:?}"),
        }

        let mut absent = linear(ID);
        absent
            .as_object_mut()
            .expect("a fixture conversation is an object")
            .remove("update_time");
        match read(absent).updated {
            RecordedTime::Unknown { why } => assert!(why.contains("absent"), "{why}"),
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    // --------------------------------------------------------------- mapping

    /// The state this slice exists for: a mapping that names nodes it does not hold
    /// is its **own** state, with no branch reading — not a tree with zero of
    /// something, and not a shorter branch.
    #[test]
    fn a_mapping_with_missing_nodes_is_its_own_state() {
        let mut object = linear(ID);
        object["mapping"]["fixture-root"]["children"] =
            serde_json::json!(["fixture-user", "fixture-not-here"]);
        let record = read(object);
        let TreeReading::MissingNodes(missing) = &record.tree else {
            panic!("expected the partial state, got {:?}", record.tree);
        };
        assert_eq!(missing.unresolved_children, 1);
        assert_eq!(missing.unresolved_parents, 0);
        assert!(!missing.unresolved_current_node);
        assert!(!missing.cyclic);
        assert!(!missing.everything_resolved());
        // The counts it does carry are counts of what is here.
        assert_eq!((missing.nodes, missing.messages), (3, 2));
        // And the nodes themselves are still kept, whole.
        assert_eq!(record.nodes.len(), 3);
    }

    #[test]
    fn a_parent_link_that_names_an_absent_node_is_missing_nodes_too() {
        let mut object = linear(ID);
        insert(
            &mut object["mapping"]["fixture-user"],
            "parent",
            serde_json::json!("fixture-not-here"),
        );
        let record = read(object);
        let TreeReading::MissingNodes(missing) = &record.tree else {
            panic!("expected the partial state, got {:?}", record.tree);
        };
        assert_eq!(missing.unresolved_parents, 1);
    }

    /// A `current_node` the mapping does not hold is one of those links: the export
    /// names something it does not carry, so the tree reads as partial and the field
    /// itself says what it named.
    #[test]
    fn a_current_node_the_mapping_does_not_hold_is_a_missing_node() {
        let mut object = linear(ID);
        insert(
            &mut object,
            "current_node",
            serde_json::json!("fixture-not-here"),
        );
        let record = read(object);
        assert_eq!(
            record.current_node,
            CurrentNodeReading::Unresolved {
                id: "fixture-not-here".to_string()
            }
        );
        let TreeReading::MissingNodes(missing) = &record.tree else {
            panic!("expected the partial state, got {:?}", record.tree);
        };
        assert!(missing.unresolved_current_node);
    }

    /// An empty mapping is a measurement ("this conversation has no nodes") and a
    /// different state from a partial one. Both live beside a mapping that is
    /// absent, which is a failure — three answers, not one.
    #[test]
    fn an_empty_mapping_is_its_own_state_and_not_a_missing_one() {
        let record = read(conversation(ID, mapping(vec![]), Value::Null));
        assert_eq!(record.tree, TreeReading::NoNodes);
        assert!(record.nodes.is_empty());
        assert_eq!(record.current_node, CurrentNodeReading::Null);

        let parsed = parse_export(&export(vec![conversation(
            ID,
            mapping(vec![]),
            Value::Null,
        )]))
        .unwrap();
        assert_eq!(parsed.empty_conversations(), 1);
        assert_eq!(parsed.missing_node_conversations(), 0);
        assert_eq!(parsed.message_count(), 0, "a measured zero");
    }

    /// A `mapping: {}` that *also* names a current node is not an empty
    /// conversation: the export states a node it does not hold, so the honest
    /// reading is the partial one.
    #[test]
    fn an_empty_mapping_that_names_a_current_node_is_not_a_measured_empty() {
        let record = read(conversation(
            ID,
            mapping(vec![]),
            serde_json::json!("fixture-not-here"),
        ));
        let TreeReading::MissingNodes(missing) = &record.tree else {
            panic!("expected the partial state, got {:?}", record.tree);
        };
        assert_eq!(missing.nodes, 0);
        assert!(missing.unresolved_current_node);
    }

    #[test]
    fn a_mapping_that_is_absent_or_null_or_not_an_object_is_a_named_failure() {
        let absent = {
            let mut object = linear(ID);
            object
                .as_object_mut()
                .expect("a fixture conversation is an object")
                .remove("mapping");
            object
        };
        let null = {
            let mut object = linear(ID);
            insert(&mut object, "mapping", Value::Null);
            object
        };
        let not_an_object = {
            let mut object = linear(ID);
            insert(
                &mut object,
                "mapping",
                serde_json::json!("fixture not a mapping"),
            );
            object
        };
        for (object, expected) in [
            (absent, ConversationFailureReason::MappingAbsent),
            (null, ConversationFailureReason::MappingNull),
            (
                not_an_object,
                ConversationFailureReason::MappingNotAnObject { found: "a string" },
            ),
        ] {
            let parsed = parse_export(&export(vec![object])).unwrap();
            assert!(parsed.conversations.is_empty());
            assert_eq!(parsed.failures.len(), 1);
            assert_eq!(parsed.failures[0].id.as_deref(), Some(ID));
            assert_eq!(parsed.failures[0].reason, expected);
        }
    }

    #[test]
    fn a_node_that_is_not_an_object_fails_the_conversation() {
        let mut object = linear(ID);
        insert(
            &mut object["mapping"],
            "fixture-user",
            serde_json::json!("fixture not a node"),
        );
        let parsed = parse_export(&export(vec![object])).unwrap();
        assert!(parsed.conversations.is_empty());
        assert_eq!(
            parsed.failures[0].reason,
            ConversationFailureReason::NodeNotAnObject { found: "a string" }
        );
    }

    #[test]
    fn a_mapping_whose_chain_revisits_a_node_is_not_walked() {
        let object = conversation(
            ID,
            mapping(vec![
                node(
                    "fixture-a",
                    serde_json::json!("fixture-b"),
                    serde_json::json!([]),
                    Value::Null,
                ),
                node(
                    "fixture-b",
                    serde_json::json!("fixture-a"),
                    serde_json::json!([]),
                    Value::Null,
                ),
            ]),
            Value::Null,
        );
        let record = read(object);
        let TreeReading::MissingNodes(missing) = &record.tree else {
            panic!("expected the partial state, got {:?}", record.tree);
        };
        assert!(missing.cyclic);
    }

    // -------------------------------------------------------------- the ids

    #[test]
    fn an_id_under_either_spelling_is_read_and_carried_verbatim() {
        let mut object = linear("fixture-id-with-CAPS_and-dashes");
        insert(
            &mut object,
            "id",
            serde_json::json!("fixture-id-with-CAPS_and-dashes"),
        );
        let record = read(object);
        assert_eq!(record.id, "fixture-id-with-CAPS_and-dashes");
    }

    #[test]
    fn two_ids_that_disagree_are_refused_rather_than_chosen() {
        let mut object = linear(ID);
        insert(&mut object, "id", serde_json::json!("fixture-other-id"));
        let parsed = parse_export(&export(vec![object])).unwrap();
        assert!(parsed.conversations.is_empty());
        assert_eq!(parsed.failures.len(), 1);
        match &parsed.failures[0].reason {
            ConversationFailureReason::IdNotReadable { why } => {
                assert!(why.contains("disagree"), "{why}")
            }
            other => panic!("expected a refused id, got {other:?}"),
        }
    }

    #[test]
    fn an_entry_without_a_readable_id_says_which_of_the_two_it_was() {
        let mut object = linear(ID);
        object
            .as_object_mut()
            .expect("a fixture conversation is an object")
            .remove("conversation_id");
        let parsed = parse_export(&export(vec![object])).unwrap();
        assert_eq!(parsed.failures.len(), 1);
        assert!(
            parsed.failures[0].id.is_none(),
            "an entry that names no id cannot be named back"
        );
        match &parsed.failures[0].reason {
            ConversationFailureReason::IdNotReadable { why } => {
                assert!(why.contains("neither"), "{why}")
            }
            other => panic!("expected a refused id, got {other:?}"),
        }
    }

    // ---------------------------------------------------- the file's own shape

    /// Invariant 2 at the file level: a file that stopped part-way is a different
    /// answer from a file that was read in full and is not an export.
    #[test]
    fn a_file_that_stopped_part_way_is_not_the_same_answer_as_a_wrong_file() {
        let whole = export(vec![linear(ID)]);
        let cut = &whole[..whole.len() / 2];
        assert!(matches!(
            parse_export(cut),
            Err(ExportFailure::NotReadToTheEnd { .. })
        ));
        assert!(matches!(
            parse_export(b"[{\"conversation_id\": }]"),
            Err(ExportFailure::NotJson { .. })
        ));
        for bytes in [b"42".as_slice(), b"\"fixture\"".as_slice()] {
            assert!(matches!(
                parse_export(bytes),
                Err(ExportFailure::NotAnExport { .. })
            ));
        }
    }

    /// A JSON object that is not a conversation is one named failure — never an
    /// export that read zero conversations, which is what a silent skip looks like.
    #[test]
    fn an_object_that_is_not_a_conversation_is_a_named_failure() {
        let parsed = parse_export(b"{\"fixture\":1}").unwrap();
        assert!(parsed.conversations.is_empty());
        assert_eq!(parsed.failures.len(), 1);
        assert!(matches!(
            parsed.failures[0].reason,
            ConversationFailureReason::IdNotReadable { .. }
        ));
    }

    #[test]
    fn an_entry_that_is_not_an_object_is_named_as_one() {
        let bytes = serde_json::to_vec(&serde_json::json!([1, "fixture"])).unwrap();
        let parsed = parse_export(&bytes).unwrap();
        assert_eq!(parsed.failures.len(), 2);
        assert!(matches!(
            parsed.failures[0].reason,
            ConversationFailureReason::NotAnObject { found: "a number" }
        ));
        assert!(matches!(
            parsed.failures[1].reason,
            ConversationFailureReason::NotAnObject { found: "a string" }
        ));
    }

    // -------------------------------------------- the tally of what was not read

    #[test]
    fn a_field_this_build_does_not_read_is_named_in_the_tally() {
        let mut object = linear(ID);
        insert(&mut object, "is_archived", serde_json::json!(true));
        insert(&mut object, "gizmo_id", Value::Null);
        let parsed = parse_export(&export(vec![object])).unwrap();
        assert_eq!(
            parsed.unreadable.get("conversations[].is_archived"),
            Some(&1usize)
        );
        assert_eq!(
            parsed.unreadable.get("conversations[].gizmo_id"),
            Some(&1usize)
        );
        assert!(
            !parsed.unreadable.contains_key("conversations[].mapping"),
            "a key this build read is not in the tally"
        );
    }

    #[test]
    fn a_node_id_field_that_disagrees_with_its_key_is_counted_and_not_used() {
        let mut object = linear(ID);
        insert(
            &mut object["mapping"]["fixture-user"],
            "id",
            serde_json::json!("fixture-other"),
        );
        let parsed = parse_export(&export(vec![object])).unwrap();
        assert_eq!(
            parsed.unreadable.get("conversations[].mapping[].id"),
            Some(&1usize)
        );
        let user = parsed.conversations[0]
            .nodes
            .iter()
            .find(|node| node.id == "fixture-user")
            .expect("the key is the identity, so the node is reachable by it");
        assert_eq!(
            user.stated_id,
            TextField::Value("fixture-other".to_string()),
            "the field is kept as what the export stated, beside the identity"
        );
    }

    #[test]
    fn an_unreadable_children_entry_is_counted_and_the_rest_is_read() {
        let mut object = linear(ID);
        object["mapping"]["fixture-root"]["children"] = serde_json::json!(["fixture-user", 7]);
        let parsed = parse_export(&export(vec![object])).unwrap();
        assert_eq!(
            parsed
                .unreadable
                .get("conversations[].mapping[].children[]"),
            Some(&1usize)
        );
    }

    // ------------------------------------------------------------- messages

    #[test]
    fn structured_parts_are_read_as_structured_and_not_dropped() {
        let mut object = linear(ID);
        object["mapping"]["fixture-assistant"]["message"]["content"]["parts"] =
            serde_json::json!(["fixture text", { "asset_pointer": "fixture-asset" }, 7]);
        let record = read(object);
        let assistant = record
            .nodes
            .iter()
            .find(|node| node.id == "fixture-assistant")
            .expect("the assistant node");
        let MessageSlot::Message(message) = &assistant.message else {
            panic!("expected a message, got {:?}", assistant.message);
        };
        assert_eq!(
            message.parts,
            Field::Value(vec![
                PartReading::Text("fixture text".to_string()),
                PartReading::Structured { found: "an object" },
                PartReading::Structured { found: "a number" },
            ])
        );
        assert_eq!(message.text().as_deref(), Some("fixture text"));
    }

    /// A `parts` list that is `null` and one that is `[]` are two different claims,
    /// and neither is a text this build invented.
    #[test]
    fn a_null_parts_list_is_not_an_empty_one() {
        let mut null = linear(ID);
        insert(
            &mut null["mapping"]["fixture-assistant"]["message"]["content"],
            "parts",
            Value::Null,
        );
        let record = read(null);
        let assistant = record
            .nodes
            .iter()
            .find(|node| node.id == "fixture-assistant")
            .expect("the assistant node");
        let MessageSlot::Message(message) = &assistant.message else {
            panic!("expected a message");
        };
        assert_eq!(message.parts, Field::Null);
        assert_eq!(message.text(), None);

        let empty = read(linear(ID));
        let root = empty
            .nodes
            .iter()
            .find(|node| node.id == "fixture-root")
            .expect("the root node");
        assert_eq!(root.message, MessageSlot::None, "a stated absent message");
    }

    #[test]
    fn only_turns_with_text_are_counted_on_a_branch() {
        let object = conversation(
            ID,
            mapping(vec![
                node(
                    "fixture-root",
                    Value::Null,
                    serde_json::json!(["fixture-tool"]),
                    Value::Null,
                ),
                node(
                    "fixture-tool",
                    serde_json::json!("fixture-root"),
                    serde_json::json!(["fixture-empty"]),
                    message(
                        serde_json::json!("tool"),
                        serde_json::json!(["fixture tool"]),
                    ),
                ),
                node(
                    "fixture-empty",
                    serde_json::json!("fixture-tool"),
                    serde_json::json!(["fixture-thinking"]),
                    message(serde_json::json!("assistant"), serde_json::json!([])),
                ),
                node(
                    "fixture-thinking",
                    serde_json::json!("fixture-empty"),
                    serde_json::json!([]),
                    serde_json::json!({
                        "id": "fixture-message",
                        "author": { "role": "fixture-role" },
                        "content": { "content_type": "thoughts", "parts": ["fixture thought"] },
                        "create_time": SECOND,
                    }),
                ),
            ]),
            Value::Null,
        );
        let record = read(object);
        let facts = facts(&record);
        assert_eq!(facts.messages, 3, "three nodes carry a message object");
        assert_eq!(facts.longest_branch.turns, 1, "only the `tool` turn counts");
        assert_eq!(facts.longest_branch.characters, "fixture tool".len());
    }

    #[test]
    fn the_platform_id_is_the_id_the_live_capture_uses() {
        assert_eq!(PLATFORM, "chatgpt");
        assert!(
            crate::activity::WEB_HARNESSES.contains(&PLATFORM),
            "an imported conversation and a captured one must land in one bucket"
        );
    }

    /// A `conversation_id` that is present and `null` is a stated absence: the
    /// ladder moves to the other spelling, the key that was read is not reported
    /// as unread, and when nothing is stated at all the refusal names the null
    /// rather than calling the key absent.
    #[test]
    fn an_id_key_that_is_null_is_named_as_null_and_does_not_stop_the_ladder() {
        let mut object = linear(ID);
        insert(&mut object, "conversation_id", Value::Null);
        insert(
            &mut object,
            "id",
            serde_json::json!("fixture_from_the_other_spelling"),
        );
        let parsed = parse_export(&export(vec![object])).unwrap();
        assert!(parsed.failures.is_empty(), "{:?}", parsed.failures);
        assert_eq!(
            parsed.conversations[0].id,
            "fixture_from_the_other_spelling"
        );
        assert!(
            !parsed.unreadable.contains_key("conversations[].id"),
            "the key the ladder read is not reported as one it did not: {:?}",
            parsed.unreadable
        );

        let mut null = linear(ID);
        insert(&mut null, "conversation_id", Value::Null);
        let parsed = parse_export(&export(vec![null])).unwrap();
        assert_eq!(parsed.failures.len(), 1);
        match &parsed.failures[0].reason {
            ConversationFailureReason::IdNotReadable { why } => {
                assert!(why.contains("null"), "{why}")
            }
            other => panic!("expected a refused id, got {other:?}"),
        }
    }
}
