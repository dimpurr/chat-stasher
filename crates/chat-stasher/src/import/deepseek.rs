//! The DeepSeek official export — `conversations.json`.
//!
//! ADR-055 makes an official export the platform's own statement of what an
//! account holds, and makes DeepSeek pilot #1 (D10). This module reads one such
//! export: the file's bytes in, conversation-level records out, and nothing else
//! on either side. It writes nothing, reads nothing from disk — the caller hands
//! it the bytes — and is wired to no command.
//!
//! ## The measured shape
//!
//! Measured on the 2026-07-01 export (13,074,881 B; 27-ORACLE §3, TKO-OUT §1).
//! Every number below is from that file, not from a reference implementation:
//!
//! * the file is a top-level JSON **array** of 234 conversation objects;
//! * a conversation carries `id` / `inserted_at` / `updated_at` / `title` /
//!   `mapping` (234/234 each), and its `id` is the same id our
//!   `deepseek.<session id>` web-capture directory already uses (234/234), so a
//!   caller joins on it directly and this parser invents no mapping of its own;
//! * `mapping` is an object keyed by node id, each node carrying `id` / `parent`
//!   / `children` / `message` (2,166 nodes). Exactly one node per conversation is
//!   a root — `"parent": null`, and its id is `root` in all 234 — that root's
//!   `message` is `null`, and the other nodes are numbered (`"1"`, `"2"`, …).
//!   Branches are real: 59 of the 234 conversations are trees with more than one
//!   leaf, i.e. conversations whose message count depends on which leaf is walked;
//! * a node's `message` carries `files` / `model` / `inserted_at` / `fragments`
//!   (1,932 messages), and a fragment carries `type` plus whatever its type
//!   implies — `content` for the text kinds, `results` for `SEARCH`, `files` for
//!   `FILE` (2,768 fragments). The kinds measured are `REQUEST`, `RESPONSE`,
//!   `THINK`, `SEARCH`, `TOOL_SEARCH`, `TOOL_OPEN` and `FILE`.
//!
//! ## Three things this module deliberately does not do
//!
//! * **It does not pick a branch.** The export names no current branch — there is
//!   no `current_node`-style field anywhere in it — so which leaf is "the"
//!   conversation is `unknown` here and is never guessed
//!   ([`CurrentBranch::NotNamedBySource`]). Nothing is lost by that: every node is
//!   kept. ADR-055 D2 keeps branches and defaults to the branch the *source* names
//!   as current, and 27-ORACLE §4.5 keeps *measurement* on the longest leaf —
//!   two different axes, and this module conflates neither.
//! * **It does not turn fragment kinds into roles.** `REQUEST` is the turn the
//!   person asked for and `RESPONSE` is the answer, but that reading belongs to
//!   the reader, not to the shape. The record reports the kinds it read.
//! * **It does not interpret a spelling it has not measured.** Every conversation
//!   and message time in the 2026-07-01 export is an RFC 3339 string with an
//!   offset (234/234 and 1,932/1,932); any other spelling is reported as
//!   `unknown` with its reason rather than converted through a guessed unit.
//!
//! ## How the three states are kept, in this module's types
//!
//! A value field is a [`Field`] and a timestamp is a [`RecordedTime`]: the value
//! the export carried, an explicit `null` it wrote, a field it did not carry at
//! all, or one it carried in a shape this build cannot read — never two of those
//! collapsed into one. A list field is a `Field<Vec<_>>` as well, so "the export
//! carries no list here" and "the export carries an empty list" stay apart; an
//! entry inside a list that this build cannot read is counted in
//! [`ExportParse::unreadable`] and left out, which makes every list in a record
//! the list this build *read*.
//!
//! Nothing is skipped in silence. A file that cannot be read as an export is an
//! [`ExportFailure`]; a conversation inside a readable file that cannot become a
//! record is a [`ConversationFailure`], reported beside the records with its id
//! and a named reason. A conversation whose `mapping` is absent, `null`, or not
//! an object is one of those: the tree is what the record *is*, and emitting an
//! empty one would read as "a conversation with nothing in it" — a claim the
//! export did not make.

use crate::activity::TimeSource;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The platform these records belong to: the `<platform>` of
/// `chat-stasher import <platform> <export-file>` (ADR-055 D4). It is the same
/// id the live web capture of this platform uses (`activity::WEB_HARNESSES`),
/// which is what makes an imported conversation and a captured one land in one
/// bucket instead of two.
pub const PLATFORM: &str = "deepseek";

/// Paths that name a spot in the export, for a record that has no line number.
/// `conversations[]` is one conversation, `mapping[]` one node, and so on; a
/// path with a field name appended (`…mapping[].message.model`) names a field,
/// and a path with an entry marker appended (`…message.fragments[]`) names an
/// entry inside a list. A fragment's and a search result's own paths are built
/// from the level above them (`…fragments[]`, `…results[]`), so there is no
/// constant here to drift away from the derivation.
const CONVERSATION: &str = "conversations[]";
const NODE: &str = "conversations[].mapping[]";
const MESSAGE: &str = "conversations[].mapping[].message";

/// The field names the measured shape carries, one row per level. A name that is
/// not in its row is reported as an unrecognised field rather than read as if it
/// were one of these — which is the only way a field the platform adds becomes
/// visible instead of silently dropped.
const CONVERSATION_KEYS: [&str; 5] = ["id", "inserted_at", "mapping", "title", "updated_at"];
const NODE_KEYS: [&str; 4] = ["children", "id", "message", "parent"];
const MESSAGE_KEYS: [&str; 4] = ["files", "fragments", "inserted_at", "model"];
const FRAGMENT_KEYS: [&str; 4] = ["content", "files", "results", "type"];
const RESULT_KEYS: [&str; 8] = [
    "cite_index",
    "published_at",
    "query_indexes",
    "site_icon",
    "site_name",
    "snippet",
    "title",
    "url",
];
/// The two spellings of an attachment reference are different pairs of names,
/// measured on 27 entries each in the 2026-07-01 export: a message's own `files`
/// entry is `{id, file_name}` and a `FILE` fragment's is
/// `{file_id, file_name, file_size}`.
const MESSAGE_FILE_KEYS: [&str; 2] = ["file_name", "id"];
const FRAGMENT_FILE_KEYS: [&str; 3] = ["file_id", "file_name", "file_size"];

/// Read one DeepSeek export file.
///
/// The input is the file's bytes exactly as the platform produced them; the same
/// bytes always produce the same records. A file that cannot be read as an
/// export at all is an [`ExportFailure`]; a conversation *inside* a readable file
/// that cannot be turned into a record is a [`ConversationFailure`], kept in
/// [`ExportParse::failures`] beside the records — never dropped, because a caller
/// that counted only the successes would report a smaller number that looks
/// complete (ADR-014, CLAUDE.md invariant 1).
///
/// The whole document is held in memory while it is read. The DeepSeek export
/// measured here is 13 MB; a platform whose export is much larger than that needs
/// a streaming reader before it is wired in, and this signature says so.
pub fn parse_export(bytes: &[u8]) -> Result<ExportParse, ExportFailure> {
    let document: Value =
        serde_json::from_slice(bytes).map_err(|error| ExportFailure::NotJson {
            why: error.to_string(),
        })?;
    let found = kind_of(&document);
    let Value::Array(entries) = document else {
        return Err(ExportFailure::NotAnArray { found });
    };
    let mut reader = Reader::default();
    for entry in &entries {
        reader.conversation(entry);
    }
    Ok(reader.finish())
}

/// Why an export file could not be read as an export at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportFailure {
    /// The bytes are not JSON this build can read: invalid UTF-8, or a syntax
    /// error at the position named in `why` (a parser position, never any of the
    /// export's own text).
    NotJson { why: String },
    /// Valid JSON, but not the array an export is. The kind actually found is
    /// named — `"object"`, `"string"`, … — so a caller can tell the wrong file
    /// from a corrupt one.
    NotAnArray { found: &'static str },
}

/// One DeepSeek export, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportParse {
    /// The conversations this build could read, in the file's own order.
    pub conversations: Vec<ConversationRecord>,
    /// The conversations it could not, in the file's own order, each with its id
    /// when the entry named a readable one.
    pub failures: Vec<ConversationFailure>,
    /// Every spot where the export carried something this build did not read,
    /// keyed by path and counted. The path tells the three sources apart:
    ///
    /// * a **field name** the measured shape does not carry, e.g.
    ///   `conversations[].conversation_template_id` — how a field the platform
    ///   adds becomes visible instead of silently dropped;
    /// * a known field carried in a **shape** this build cannot read, e.g.
    ///   `conversations[].updated_at` holding a number;
    /// * an **entry inside a list** this build cannot read, e.g.
    ///   `…fragments[].files[]` holding a string.
    ///
    /// An unknown *value* that has a typed home in a record is deliberately not
    /// counted here: a fragment `type` this build has never seen is named on the
    /// fragment itself ([`FragmentKind::Unknown`]), which is the stronger report,
    /// and counting it twice would make this tally unreadable.
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

    /// How many messages the records hold — the nodes that carry one.
    pub fn message_count(&self) -> usize {
        self.conversations
            .iter()
            .map(|conversation| conversation.messages().len())
            .sum()
    }

    /// How many records are trees with more than one leaf: the conversations
    /// whose message count depends on which leaf is walked. 27-ORACLE §4.5
    /// reports the same number as `branch_ambiguous_sessions`.
    pub fn branching_conversations(&self) -> usize {
        self.conversations
            .iter()
            .filter(|conversation| conversation.leaves().len() > 1)
            .count()
    }

    /// How many fragments the records hold, counted by the label the export
    /// spelled ([`FragmentKind::label`]) — a report that counts by kind without
    /// interpreting any of them. A message whose `fragments` this build could
    /// not read contributes nothing here; it is named in
    /// [`ExportParse::unreadable`] instead, so this count is a count of the
    /// fragments this build read.
    pub fn fragment_counts(&self) -> BTreeMap<&str, usize> {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for conversation in &self.conversations {
            for message in conversation.messages() {
                if let Field::Value(fragments) = &message.fragments {
                    for fragment in fragments {
                        *counts.entry(fragment.kind.label()).or_insert(0) += 1;
                    }
                }
            }
        }
        counts
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
    /// The entry's `id` is absent, empty, or not a string. Without it nothing
    /// can be joined — ADR-055 D5 groups observations by conversation id — so
    /// there is no record to emit.
    IdNotReadable { why: String },
    /// The entry carries no `mapping` at all. This is deliberately not the same
    /// as an empty tree: `"mapping": {}` is the export stating there are no
    /// nodes and reads into a record with zero of them, while an absent key is a
    /// conversation whose tree this build cannot see (ADR-015).
    MappingAbsent,
    /// The entry carries `"mapping": null`: the export states there is no tree
    /// here, and whether that means "no messages" or "withheld" is not something
    /// this build claims to know.
    MappingNull,
    /// The entry carries a `mapping` that is not an object.
    MappingNotAnObject { found: &'static str },
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
    /// happened is named in [`ExportParse::unreadable`], so an unreadable field
    /// is never mistaken for one of the three states above.
    Unreadable { why: String },
}

/// A [`Field`] holding text.
pub type TextField = Field<String>;

/// A [`Field`] holding an integer. The export spells its integers two ways —
/// `1718582400` and `1718582400.0` both occur in the 2026-07-01 file, the first
/// on `cite_index` and `query_indexes`, the second on `published_at` — and both
/// are read as the same value; a number that is not integral, or is not a
/// number, is [`Field::Unreadable`].
pub type IntegerField = Field<i64>;

/// A timestamp the export carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedTime {
    /// The instant the export names, and how this build read it: `Exact` for the
    /// RFC 3339 timestamp with an offset, which is the only spelling the measured
    /// export uses.
    Known { unix: i64, source: TimeSource },
    /// The timestamp could not be placed in time, with the reason — absent, not a
    /// string, or a string that is not RFC 3339. Never `0`, which would be a
    /// measurement this build did not make.
    Unknown { why: String },
}

/// One conversation, as the export states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationRecord {
    /// The export's own conversation id, carried verbatim: it is the
    /// `<session id>` of our `deepseek.<session id>` web-capture directory
    /// (27-ORACLE §3 measured the join on 234/234), so a caller joins on it and
    /// no mapping is invented here.
    pub id: String,
    /// `inserted_at`: when the platform says the conversation was created.
    pub created_at: RecordedTime,
    /// `updated_at`: when the platform says it last changed.
    pub updated_at: RecordedTime,
    /// `title`. User content: the parser carries it, and nothing in this crate
    /// may log, report or commit it.
    pub title: TextField,
    /// Every mapping node the export carried, in the order this parser read them
    /// (deterministic for the same bytes, and not necessarily the file's own
    /// order). The tree is kept whole — a node off whichever branch is current is
    /// a *kept branch*, not a dropped line (ADR-055 D2).
    pub nodes: Vec<NodeRecord>,
    /// Which branch the export names as current — see [`CurrentBranch`].
    pub current_branch: CurrentBranch,
    /// Every attachment reference the conversation carries, from both places the
    /// shape spells one.
    pub attachments: Vec<AttachmentRef>,
}

impl ConversationRecord {
    /// The nodes the export marks as the tree's root (`"parent": null`).
    pub fn roots(&self) -> Vec<&NodeRecord> {
        self.nodes
            .iter()
            .filter(|node| node.parent == ParentLink::Root)
            .collect()
    }

    /// The nodes the export carries no children for — `children` read as an empty
    /// list. A node whose `children` this build could not read is *not* claimed as
    /// a leaf; it is named in [`ExportParse::unreadable`] instead.
    pub fn leaves(&self) -> Vec<&NodeRecord> {
        self.nodes
            .iter()
            .filter(|node| matches!(&node.children, Field::Value(children) if children.is_empty()))
            .collect()
    }

    /// The nodes that carry a message, in node order.
    pub fn messages(&self) -> Vec<&MessageRecord> {
        self.nodes
            .iter()
            .filter_map(|node| match &node.message {
                MessageSlot::Carries(message) => Some(message),
                MessageSlot::NoMessage | MessageSlot::Unknown { .. } => None,
            })
            .collect()
    }
}

/// Which branch the export names as current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentBranch {
    /// The export names the node the source points at as its current one. This is
    /// the shape ChatGPT's `mapping` + `current_node` has; a DeepSeek conversation
    /// never produces it.
    Named { node_id: String },
    /// The export names no current branch. That is a fact about the export, not an
    /// unreadable value: DeepSeek carries no `current_node`-style field, so which
    /// leaf is "the" conversation is `unknown` and is never guessed (ADR-055 D2).
    /// Branch structure is still complete — every node is kept — and a caller that
    /// needs one path has to choose an axis and say so; 27-ORACLE §4.5 chooses the
    /// longest leaf for *measurement*.
    NotNamedBySource,
}

/// One `mapping` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    /// The id the tree's own links name this node by — the `mapping` key. The
    /// export also spells it in the node's `id` field, and the two agreed on every
    /// one of the 2,166 measured nodes; a node where they disagree is reported in
    /// [`ExportParse::unreadable`] and carries the key, because the key is what
    /// the links point at.
    pub id: String,
    pub parent: ParentLink,
    /// `children`. An entry that is not a string is not a child this build can
    /// name: it is counted in [`ExportParse::unreadable`] and left out, rather
    /// than written as an empty name.
    pub children: Field<Vec<String>>,
    pub message: MessageSlot,
}

/// Where a node sits in the tree, as the export spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentLink {
    /// `"parent": null`: this is a root. The measured shape has exactly one per
    /// conversation (and names it `root` in all 234), but this parser reads the
    /// link rather than the name.
    Root,
    /// The id of the node this one hangs from.
    Node(String),
    /// `parent` is absent, or carries something that is not a string: the node's
    /// place in the tree is not something this build can claim.
    Unknown { why: String },
}

/// What a `mapping` node says about its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageSlot {
    /// `"message": null` — measured on the root node of every conversation: the
    /// node carries no message at all. This is a known-empty, not an unreadable
    /// one (the same distinction the live-capture reader makes for ChatGPT's
    /// `"message": null` root), and it is not a message in any count.
    NoMessage,
    /// The message the node carries.
    Carries(MessageRecord),
    /// `message` is absent, or carries something that is not an object.
    Unknown { why: String },
}

/// One message: a node's `message` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    /// The message's own `inserted_at`.
    pub time: RecordedTime,
    /// `model`.
    pub model: TextField,
    /// `fragments`, in the export's own order. An empty list is the export's own
    /// measurement — "this message has no fragments", which 5 of the 1,932
    /// measured messages state — and stays a count of zero rather than becoming an
    /// unknown. There is deliberately no accessor that flattens the other states
    /// into an empty slice: a caller that renders fragments has to say what it
    /// does with a list it could not read.
    pub fragments: Field<Vec<FragmentRecord>>,
    /// `files` on the message itself. The shape spells an attachment here too,
    /// with its own pair of field names.
    pub files: Field<Vec<AttachmentRef>>,
}

/// One `fragments[]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FragmentRecord {
    pub kind: FragmentKind,
    /// `content`. Absent on the kinds that carry none — every `SEARCH`, `FILE`,
    /// `TOOL_SEARCH` and `TOOL_OPEN` fragment measured — and `Field::Value("")`
    /// when the export really did record an empty one. The two are not the same
    /// claim and are not stored as the same value.
    pub content: TextField,
    /// `results`, on a `SEARCH` fragment. 32 of the 83 measured `SEARCH` fragments
    /// carry an empty list, which is a measurement.
    pub results: Field<Vec<SearchResultRef>>,
    /// `files`, on a `FILE` fragment.
    pub files: Field<Vec<AttachmentRef>>,
}

/// What a fragment says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FragmentKind {
    /// `REQUEST` — the turn the person asked for.
    Request,
    /// `RESPONSE` — the answer.
    Response,
    /// `THINK` — the model's reasoning.
    Thinking,
    /// `SEARCH` — a web search, whose hits are in the fragment's `results`.
    Search,
    /// `TOOL_SEARCH`.
    ToolSearch,
    /// `TOOL_OPEN`.
    ToolOpen,
    /// `FILE` — a file the turn carried, named in the fragment's `files`.
    File,
    /// A kind this build does not know, spelled as the export spelled it. It is
    /// kept rather than folded into a kind this build does know, and its text —
    /// when it carries any — is kept with it: an unrecognised kind is a fragment
    /// this build cannot classify, never a fragment it may treat as empty.
    Unknown { spelled: String },
    /// `type` is absent, or carries something that is not a string: the fragment's
    /// kind is `unknown`, with the reason spelled.
    Unreadable { why: String },
}

impl FragmentKind {
    /// The kind the export spelled, or the state that keeps an unrecognised
    /// spelling apart from a kind this build knows.
    fn read(spelled: &str) -> Self {
        match spelled {
            "REQUEST" => Self::Request,
            "RESPONSE" => Self::Response,
            "THINK" => Self::Thinking,
            "SEARCH" => Self::Search,
            "TOOL_SEARCH" => Self::ToolSearch,
            "TOOL_OPEN" => Self::ToolOpen,
            "FILE" => Self::File,
            other => Self::Unknown {
                spelled: other.to_string(),
            },
        }
    }

    /// The kind as a report reads it: the export's own spelling for a kind this
    /// build knows, the export's spelling for one it does not — so an
    /// unrecognised kind is named rather than counted as one of ours — and
    /// `unnamed` for a fragment whose `type` this build could not read.
    pub fn label(&self) -> &str {
        match self {
            Self::Request => "REQUEST",
            Self::Response => "RESPONSE",
            Self::Thinking => "THINK",
            Self::Search => "SEARCH",
            Self::ToolSearch => "TOOL_SEARCH",
            Self::ToolOpen => "TOOL_OPEN",
            Self::File => "FILE",
            Self::Unknown { spelled } => spelled,
            Self::Unreadable { .. } => "unnamed",
        }
    }
}

/// One `results[]` entry on a `SEARCH` fragment.
///
/// Every field is carried as the export wrote it. Two of them are numbers whose
/// unit this build does not interpret: `published_at` (measured range
/// 959,472,000 – 1,755,388,800, which is epoch seconds by value, and spelled with
/// a decimal point) and `cite_index` (1–10). Where the export wrote `null` — 1,175
/// of the 1,669 measured entries have a null `cite_index`, and 306 a null
/// `site_name` — that null is [`Field::Null`], not a missing field and not a zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResultRef {
    pub url: TextField,
    /// The hit's own title. User content: carried, never logged.
    pub title: TextField,
    /// The hit's snippet. User content: carried, never logged.
    pub snippet: TextField,
    pub cite_index: IntegerField,
    pub published_at: IntegerField,
    pub site_icon: TextField,
    pub site_name: TextField,
    pub query_indexes: Field<Vec<i64>>,
}

/// One attachment reference, as the export spells it.
///
/// The shape spells these two ways, and this build does not claim the two id
/// spellings name the same id namespace, so where a reference came from is kept
/// rather than merged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRef {
    pub origin: AttachmentOrigin,
    /// `file_id` on a fragment's reference, `id` on a message's.
    pub id: TextField,
    /// `file_name` in both spellings.
    pub name: TextField,
    /// `file_size`, which only the fragment spelling carries. On a message's
    /// reference this is [`Field::Absent`] — the field the shape has there is not
    /// carried, which is a different claim from a size of zero.
    pub bytes: IntegerField,
}

/// Which of the two places the shape spells an attachment reference in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentOrigin {
    /// `mapping[].message.files[]` — `{id, file_name}`.
    Message,
    /// `mapping[].message.fragments[].files[]` on a `FILE` fragment —
    /// `{file_id, file_name, file_size}`.
    Fragment,
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

    /// Every field name at one level that the measured shape does not carry.
    fn note_unrecognised(&mut self, object: &Map<String, Value>, known: &[&str], path: &str) {
        for name in object.keys() {
            if !known.contains(&name.as_str()) {
                self.note(&format!("{path}.{name}"));
            }
        }
    }

    /// One field that is carried in a shape this build cannot read.
    fn wrong_shape<T>(&mut self, path: &str, key: &str, carried: &str) -> Field<T> {
        self.note(&format!("{path}.{key}"));
        Field::Unreadable {
            why: format!("`{key}` is carried as {carried}"),
        }
    }

    fn text(&mut self, object: &Map<String, Value>, key: &str, path: &str) -> TextField {
        match object.get(key) {
            None => Field::Absent,
            Some(Value::Null) => Field::Null,
            Some(Value::String(text)) => Field::Value(text.clone()),
            Some(other) => self.wrong_shape(path, key, carried_as(other)),
        }
    }

    fn integer(&mut self, object: &Map<String, Value>, key: &str, path: &str) -> IntegerField {
        match object.get(key) {
            None => Field::Absent,
            Some(Value::Null) => Field::Null,
            Some(other) => match integral(other) {
                Some(value) => Field::Value(value),
                None => self.wrong_shape(path, key, carried_as(other)),
            },
        }
    }

    /// A list of strings, one entry per readable element. An entry this build
    /// cannot read is counted and left out; the field's own states — absent,
    /// `null`, unreadable — are kept on the field.
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
            Some(other) => self.wrong_shape(path, key, carried_as(other)),
        }
    }

    fn integer_list(
        &mut self,
        object: &Map<String, Value>,
        key: &str,
        path: &str,
    ) -> Field<Vec<i64>> {
        match object.get(key) {
            None => Field::Absent,
            Some(Value::Null) => Field::Null,
            Some(Value::Array(items)) => {
                let entry = format!("{path}.{key}[]");
                let mut numbers = Vec::with_capacity(items.len());
                for item in items {
                    match integral(item) {
                        Some(value) => numbers.push(value),
                        None => self.note(&entry),
                    }
                }
                Field::Value(numbers)
            }
            Some(other) => self.wrong_shape(path, key, carried_as(other)),
        }
    }

    fn recorded_time(
        &mut self,
        object: &Map<String, Value>,
        key: &str,
        path: &str,
    ) -> RecordedTime {
        match object.get(key) {
            Some(Value::String(text)) => match chrono::DateTime::parse_from_rfc3339(text) {
                Ok(time) => RecordedTime::Known {
                    unix: time.timestamp(),
                    source: TimeSource::Exact,
                },
                Err(_) => {
                    self.note(&format!("{path}.{key}"));
                    RecordedTime::Unknown {
                        why: format!("`{key}` is not an RFC 3339 timestamp"),
                    }
                }
            },
            Some(other) => {
                self.note(&format!("{path}.{key}"));
                RecordedTime::Unknown {
                    why: format!(
                        "`{key}` is carried as {} and the only spelling this build reads is the \
                         export's RFC 3339 one",
                        carried_as(other)
                    ),
                }
            }
            None => RecordedTime::Unknown {
                why: format!("the record carries no `{key}`"),
            },
        }
    }

    fn conversation(&mut self, entry: &Value) {
        let Some(object) = entry.as_object() else {
            self.failures.push(ConversationFailure {
                id: None,
                reason: ConversationFailureReason::NotAnObject {
                    found: kind_of(entry),
                },
            });
            return;
        };
        self.note_unrecognised(object, &CONVERSATION_KEYS, CONVERSATION);
        let Some(id) = self.conversation_id(object) else {
            return;
        };
        let Some(mapping) = self.mapping(object, &id) else {
            return;
        };

        let mut nodes = Vec::with_capacity(mapping.len());
        let mut attachments = Vec::new();
        for (key, value) in mapping {
            let Some(node) = self.node(key, value) else {
                continue;
            };
            if let MessageSlot::Carries(message) = &node.message {
                if let Field::Value(files) = &message.files {
                    attachments.extend(files.iter().cloned());
                }
                if let Field::Value(fragments) = &message.fragments {
                    for fragment in fragments {
                        if let Field::Value(files) = &fragment.files {
                            attachments.extend(files.iter().cloned());
                        }
                    }
                }
            }
            nodes.push(node);
        }

        let created_at = self.recorded_time(object, "inserted_at", CONVERSATION);
        let updated_at = self.recorded_time(object, "updated_at", CONVERSATION);
        let title = self.text(object, "title", CONVERSATION);
        self.conversations.push(ConversationRecord {
            id,
            created_at,
            updated_at,
            title,
            nodes,
            // The measured export names no current branch anywhere in the file;
            // see `CurrentBranch::NotNamedBySource`.
            current_branch: CurrentBranch::NotNamedBySource,
            attachments,
        });
    }

    /// The entry's conversation id, or a named failure.
    fn conversation_id(&mut self, object: &Map<String, Value>) -> Option<String> {
        let why = match object.get("id") {
            Some(Value::String(id)) if !id.is_empty() => return Some(id.clone()),
            Some(Value::String(_)) => "the entry's `id` is empty".to_string(),
            Some(other) => format!("the entry's `id` is carried as {}", carried_as(other)),
            None => "the entry carries no `id`".to_string(),
        };
        self.failures.push(ConversationFailure {
            id: None,
            reason: ConversationFailureReason::IdNotReadable { why },
        });
        None
    }

    /// The entry's `mapping`, or a named failure. A conversation whose tree this
    /// build cannot see is not emitted as a record with an empty one.
    fn mapping<'a>(
        &mut self,
        object: &'a Map<String, Value>,
        id: &str,
    ) -> Option<&'a Map<String, Value>> {
        let reason = match object.get("mapping") {
            Some(Value::Object(mapping)) => return Some(mapping),
            Some(Value::Null) => ConversationFailureReason::MappingNull,
            Some(other) => ConversationFailureReason::MappingNotAnObject {
                found: kind_of(other),
            },
            None => ConversationFailureReason::MappingAbsent,
        };
        self.failures.push(ConversationFailure {
            id: Some(id.to_string()),
            reason,
        });
        None
    }

    fn node(&mut self, key: &str, value: &Value) -> Option<NodeRecord> {
        let Some(object) = value.as_object() else {
            self.note(NODE);
            return None;
        };
        self.note_unrecognised(object, &NODE_KEYS, NODE);
        // The tree's links name nodes by the `mapping` key, so that is the id this
        // record carries. A node whose own `id` field is absent, unreadable, or a
        // different string is reported rather than preferred: the links are what
        // the rest of the tree can actually be walked with.
        let id = match object.get("id").and_then(Value::as_str) {
            Some(spelled) if spelled == key => key.to_string(),
            _ => {
                self.note(&format!("{NODE}.id"));
                key.to_string()
            }
        };
        let parent = match object.get("parent") {
            Some(Value::Null) => ParentLink::Root,
            Some(Value::String(parent)) => ParentLink::Node(parent.clone()),
            Some(other) => {
                self.note(&format!("{NODE}.parent"));
                ParentLink::Unknown {
                    why: format!("`parent` is carried as {}", carried_as(other)),
                }
            }
            None => {
                self.note(&format!("{NODE}.parent"));
                ParentLink::Unknown {
                    why: "the node carries no `parent`".to_string(),
                }
            }
        };
        let children = self.string_list(object, "children", NODE);
        let message = match object.get("message") {
            Some(Value::Null) => MessageSlot::NoMessage,
            Some(Value::Object(message)) => MessageSlot::Carries(self.message(message)),
            Some(other) => {
                self.note(&format!("{NODE}.message"));
                MessageSlot::Unknown {
                    why: format!("`message` is carried as {}", carried_as(other)),
                }
            }
            None => {
                self.note(&format!("{NODE}.message"));
                MessageSlot::Unknown {
                    why: "the node carries no `message`".to_string(),
                }
            }
        };
        Some(NodeRecord {
            id,
            parent,
            children,
            message,
        })
    }

    fn message(&mut self, object: &Map<String, Value>) -> MessageRecord {
        self.note_unrecognised(object, &MESSAGE_KEYS, MESSAGE);
        let time = self.recorded_time(object, "inserted_at", MESSAGE);
        let model = self.text(object, "model", MESSAGE);
        let files = self.attachment_list(object, "files", MESSAGE, AttachmentOrigin::Message);
        let fragments = self.fragment_list(object, "fragments", MESSAGE);
        MessageRecord {
            time,
            model,
            fragments,
            files,
        }
    }

    fn fragment_list(
        &mut self,
        object: &Map<String, Value>,
        key: &str,
        path: &str,
    ) -> Field<Vec<FragmentRecord>> {
        match object.get(key) {
            None => Field::Absent,
            Some(Value::Null) => Field::Null,
            Some(Value::Array(items)) => {
                let entry = format!("{path}.{key}[]");
                let mut fragments = Vec::with_capacity(items.len());
                for item in items {
                    if let Some(fragment) = self.fragment(item, &entry) {
                        fragments.push(fragment);
                    }
                }
                Field::Value(fragments)
            }
            Some(other) => self.wrong_shape(path, key, carried_as(other)),
        }
    }

    fn fragment(&mut self, value: &Value, path: &str) -> Option<FragmentRecord> {
        let Some(object) = value.as_object() else {
            self.note(path);
            return None;
        };
        self.note_unrecognised(object, &FRAGMENT_KEYS, path);
        let kind = match object.get("type") {
            Some(Value::String(spelled)) => FragmentKind::read(spelled),
            Some(other) => {
                self.note(&format!("{path}.type"));
                FragmentKind::Unreadable {
                    why: format!("`type` is carried as {}", carried_as(other)),
                }
            }
            None => FragmentKind::Unreadable {
                why: "the fragment carries no `type`".to_string(),
            },
        };
        let content = self.text(object, "content", path);
        let results = self.result_list(object, "results", path);
        let files = self.attachment_list(object, "files", path, AttachmentOrigin::Fragment);
        Some(FragmentRecord {
            kind,
            content,
            results,
            files,
        })
    }

    fn result_list(
        &mut self,
        object: &Map<String, Value>,
        key: &str,
        path: &str,
    ) -> Field<Vec<SearchResultRef>> {
        match object.get(key) {
            None => Field::Absent,
            Some(Value::Null) => Field::Null,
            Some(Value::Array(items)) => {
                let entry = format!("{path}.{key}[]");
                let mut results = Vec::with_capacity(items.len());
                for item in items {
                    if let Some(result) = self.result(item, &entry) {
                        results.push(result);
                    }
                }
                Field::Value(results)
            }
            Some(other) => self.wrong_shape(path, key, carried_as(other)),
        }
    }

    fn result(&mut self, value: &Value, path: &str) -> Option<SearchResultRef> {
        let Some(object) = value.as_object() else {
            self.note(path);
            return None;
        };
        self.note_unrecognised(object, &RESULT_KEYS, path);
        Some(SearchResultRef {
            url: self.text(object, "url", path),
            title: self.text(object, "title", path),
            snippet: self.text(object, "snippet", path),
            cite_index: self.integer(object, "cite_index", path),
            published_at: self.integer(object, "published_at", path),
            site_icon: self.text(object, "site_icon", path),
            site_name: self.text(object, "site_name", path),
            query_indexes: self.integer_list(object, "query_indexes", path),
        })
    }

    fn attachment_list(
        &mut self,
        object: &Map<String, Value>,
        key: &str,
        path: &str,
        origin: AttachmentOrigin,
    ) -> Field<Vec<AttachmentRef>> {
        match object.get(key) {
            None => Field::Absent,
            Some(Value::Null) => Field::Null,
            Some(Value::Array(items)) => {
                let entry = format!("{path}.{key}[]");
                let mut attachments = Vec::with_capacity(items.len());
                for item in items {
                    if let Some(attachment) = self.attachment(item, &entry, origin) {
                        attachments.push(attachment);
                    }
                }
                Field::Value(attachments)
            }
            Some(other) => self.wrong_shape(path, key, carried_as(other)),
        }
    }

    fn attachment(
        &mut self,
        value: &Value,
        path: &str,
        origin: AttachmentOrigin,
    ) -> Option<AttachmentRef> {
        let Some(object) = value.as_object() else {
            self.note(path);
            return None;
        };
        let (id_key, known) = match origin {
            AttachmentOrigin::Message => ("id", &MESSAGE_FILE_KEYS[..]),
            AttachmentOrigin::Fragment => ("file_id", &FRAGMENT_FILE_KEYS[..]),
        };
        self.note_unrecognised(object, known, path);
        let id = self.text(object, id_key, path);
        let name = self.text(object, "file_name", path);
        let bytes = match origin {
            AttachmentOrigin::Fragment => self.integer(object, "file_size", path),
            // The message-level spelling carries no size at all (measured: 27 of
            // 27 are `{id, file_name}`), so there is no field here to read. A size
            // the platform ever adds to that spelling is an unrecognised field
            // name in the tally, not a value guessed here.
            AttachmentOrigin::Message => Field::Absent,
        };
        Some(AttachmentRef {
            origin,
            id,
            name,
            bytes,
        })
    }
}

/// The JSON kind of a value as a bare noun, for a caller that names it.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The same, phrased for a sentence: "carried as a number", "carried as null".
fn carried_as(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The integer a JSON number names, in either spelling the export uses: `7` and
/// `7.0` are the same value here, and anything that is not an integral number
/// within the range `f64` can hold exactly is not read rather than rounded or
/// truncated.
fn integral(value: &Value) -> Option<i64> {
    if let Some(raw) = value.as_i64() {
        return Some(raw);
    }
    let float = value.as_f64()?;
    if float.is_finite() && float.fract() == 0.0 && float.abs() <= 9_007_199_254_740_992.0 {
        Some(float as i64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// One conversation in the measured shape: a root named `root` whose message
    /// is `null`, two numbered message nodes, and a `mapping` keyed by node id.
    fn linear_conversation() -> Value {
        json!({
            "id": "fixture-00000000-0000-4000-8000-000000000001",
            "title": "fixture title",
            "inserted_at": "2025-01-05T03:21:54.163000+08:00",
            "updated_at": "2025-01-05T03:25:00.000000+08:00",
            "mapping": {
                "root": { "id": "root", "parent": null, "children": ["1"], "message": null },
                "1": {
                    "id": "1",
                    "parent": "root",
                    "children": ["2"],
                    "message": {
                        "files": [],
                        "model": "fixture-model",
                        "inserted_at": "2025-01-05T03:21:54.587000+08:00",
                        "fragments": [{ "type": "REQUEST", "content": "fixture question" }]
                    }
                },
                "2": {
                    "id": "2",
                    "parent": "1",
                    "children": [],
                    "message": {
                        "files": [],
                        "model": "fixture-model",
                        "inserted_at": "2025-01-05T03:21:55.000000+08:00",
                        "fragments": [{ "type": "RESPONSE", "content": "fixture answer" }]
                    }
                }
            }
        })
    }

    fn parse(fixture: Value) -> ExportParse {
        parse_export(fixture.to_string().as_bytes())
            .expect("the fixture is an export this build reads")
    }

    /// The fragments one message carries, for a test that reads them. The
    /// states other than `Value` are covered by their own tests; a test that
    /// reads fragment contents is about the readable case.
    fn fragments_of(message: &MessageRecord) -> &[FragmentRecord] {
        match &message.fragments {
            Field::Value(fragments) => fragments,
            other => panic!("the fixture's fragments are readable, found {other:?}"),
        }
    }

    /// The `mapping` of the one-conversation fixture, for a test that changes it.
    fn mapping_mut(fixture: &mut Value) -> &mut Map<String, Value> {
        fixture
            .get_mut("mapping")
            .and_then(Value::as_object_mut)
            .expect("the fixture carries a mapping")
    }

    /// The `message` of one node of the one-conversation fixture.
    fn message_mut<'a>(fixture: &'a mut Value, node: &str) -> &'a mut Map<String, Value> {
        mapping_mut(fixture)
            .get_mut(node)
            .and_then(Value::as_object_mut)
            .and_then(|node| node.get_mut("message"))
            .and_then(Value::as_object_mut)
            .expect("the fixture carries that node's message")
    }

    #[test]
    fn the_measured_shape_reads_into_one_record() {
        let parse = parse(json!([linear_conversation()]));

        assert!(parse.failures.is_empty());
        assert!(parse.unreadable.is_empty(), "{:?}", parse.unreadable);
        let conversation = &parse.conversations[0];
        assert_eq!(
            conversation.id,
            "fixture-00000000-0000-4000-8000-000000000001"
        );
        assert_eq!(
            conversation.created_at,
            RecordedTime::Known {
                unix: 1_736_018_514,
                source: TimeSource::Exact,
            }
        );
        assert_eq!(conversation.current_branch, CurrentBranch::NotNamedBySource);
        assert_eq!(conversation.nodes.len(), 3);
        assert_eq!(conversation.messages().len(), 2);
        assert_eq!(conversation.roots().len(), 1);
        assert_eq!(conversation.leaves().len(), 1);
        // The root carries no message at all: a known-empty, not a message and
        // not an unreadable one.
        let root = conversation.roots()[0];
        assert_eq!(root.id, "root");
        assert_eq!(root.parent, ParentLink::Root);
        assert_eq!(root.message, MessageSlot::NoMessage);
        assert_eq!(
            fragments_of(conversation.messages()[0])[0].kind,
            FragmentKind::Request
        );
        assert_eq!(
            fragments_of(conversation.messages()[0])[0].content,
            Field::Value("fixture question".to_string())
        );
        assert_eq!(parse.node_count(), 3);
        assert_eq!(parse.message_count(), 2);
        assert_eq!(parse.branching_conversations(), 0);
    }

    #[test]
    fn a_branch_is_kept_even_though_no_leaf_is_named_current() {
        let mut fixture = linear_conversation();
        mapping_mut(&mut fixture).insert(
            "3".to_string(),
            json!({
                "id": "3",
                "parent": "1",
                "children": [],
                "message": {
                    "files": [],
                    "model": "fixture-model",
                    "inserted_at": "2025-01-05T03:21:56.000000+08:00",
                    "fragments": [{ "type": "RESPONSE", "content": "fixture other answer" }]
                }
            }),
        );
        let parse = parse(json!([fixture]));
        let conversation = &parse.conversations[0];

        assert_eq!(conversation.nodes.len(), 4);
        assert_eq!(conversation.leaves().len(), 2);
        assert_eq!(parse.branching_conversations(), 1);
        assert_eq!(conversation.current_branch, CurrentBranch::NotNamedBySource);
        assert!(parse.unreadable.is_empty(), "{:?}", parse.unreadable);
    }

    #[test]
    fn an_empty_tree_is_a_record_and_a_missing_one_is_a_named_failure() {
        let parse = parse(json!([
            { "id": "fixture-a", "mapping": {} },
            { "id": "fixture-b", "mapping": null },
            { "id": "fixture-c", "mapping": "fixture-not-an-object" },
            { "id": "fixture-d" }
        ]));

        // `"mapping": {}` is the export stating there are no nodes, so it is a
        // record with zero of them — not a failure, and not an unknown.
        assert_eq!(parse.conversations.len(), 1);
        assert!(parse.conversations[0].nodes.is_empty());
        assert_eq!(parse.node_count(), 0);

        let reasons: Vec<&ConversationFailureReason> = parse
            .failures
            .iter()
            .map(|failure| &failure.reason)
            .collect();
        assert_eq!(
            reasons,
            vec![
                &ConversationFailureReason::MappingNull,
                &ConversationFailureReason::MappingNotAnObject { found: "string" },
                &ConversationFailureReason::MappingAbsent,
            ]
        );
        // Each failure still names the conversation it is about.
        let ids: Vec<Option<&str>> = parse
            .failures
            .iter()
            .map(|failure| failure.id.as_deref())
            .collect();
        assert_eq!(
            ids,
            vec![Some("fixture-b"), Some("fixture-c"), Some("fixture-d")]
        );
        assert!(parse.unreadable.is_empty(), "{:?}", parse.unreadable);
    }

    #[test]
    fn a_node_this_build_cannot_read_is_counted_and_the_rest_of_the_tree_survives() {
        let parse = parse(json!([{
            "id": "fixture-a",
            "mapping": { "root": 7, "1": null, "2": {} }
        }]));

        // The two entries that are not objects cannot become nodes; the third is
        // a node whose four fields are all absent, which is a different state
        // from each of them being empty.
        assert_eq!(parse.conversations.len(), 1);
        let conversation = &parse.conversations[0];
        assert_eq!(conversation.nodes.len(), 1);
        let node = &conversation.nodes[0];
        assert_eq!(node.id, "2");
        assert!(matches!(node.parent, ParentLink::Unknown { .. }));
        assert_eq!(node.children, Field::Absent);
        assert!(matches!(node.message, MessageSlot::Unknown { .. }));
        assert_eq!(
            parse.unreadable,
            BTreeMap::from([
                ("conversations[].mapping[]".to_string(), 2),
                ("conversations[].mapping[].id".to_string(), 1),
                ("conversations[].mapping[].parent".to_string(), 1),
                ("conversations[].mapping[].message".to_string(), 1),
            ])
        );
    }

    #[test]
    fn a_conversation_that_does_not_identify_itself_is_named() {
        let parse = parse(json!([
            "fixture-not-an-object",
            { "mapping": {} },
            { "id": "", "mapping": {} },
            { "id": 7, "mapping": {} }
        ]));

        assert!(parse.conversations.is_empty());
        assert_eq!(parse.failures.len(), 4);
        assert_eq!(
            parse.failures[0].reason,
            ConversationFailureReason::NotAnObject { found: "string" }
        );
        assert_eq!(parse.failures[0].id, None);
        for failure in &parse.failures[1..] {
            assert!(matches!(
                failure.reason,
                ConversationFailureReason::IdNotReadable { .. }
            ));
            assert_eq!(failure.id, None);
        }
    }

    #[test]
    fn a_child_this_build_cannot_read_is_counted_and_not_written_as_a_name() {
        let mut fixture = linear_conversation();
        let root = mapping_mut(&mut fixture)
            .get_mut("root")
            .and_then(Value::as_object_mut)
            .expect("the fixture carries the root node");
        root.insert("children".to_string(), json!(["1", 7]));

        let parse = parse(json!([fixture]));
        let conversation = &parse.conversations[0];
        let root = conversation.roots()[0];
        assert_eq!(root.children, Field::Value(vec!["1".to_string()]));
        assert_eq!(
            parse.unreadable.get("conversations[].mapping[].children[]"),
            Some(&1)
        );
    }

    #[test]
    fn a_node_id_that_disagrees_with_its_key_carries_the_key_and_is_reported() {
        let mut fixture = linear_conversation();
        let node = mapping_mut(&mut fixture)
            .get_mut("1")
            .and_then(Value::as_object_mut)
            .expect("the fixture carries node 1");
        node.insert("id".to_string(), json!("fixture-other-id"));

        let parse = parse(json!([fixture]));
        let conversation = &parse.conversations[0];
        // The tree's links name the node by its key, so that is what the record
        // carries; the disagreement is reported rather than resolved silently.
        assert!(conversation.nodes.iter().any(|node| node.id == "1"));
        assert_eq!(
            parse.unreadable.get("conversations[].mapping[].id"),
            Some(&1)
        );
    }

    #[test]
    fn a_time_in_a_spelling_this_build_has_not_measured_is_unknown_and_named() {
        let mut fixture = linear_conversation();
        let object = fixture.as_object_mut().expect("the fixture is an object");
        object.insert("updated_at".to_string(), json!(1_736_018_514));
        object.remove("inserted_at");

        let parse = parse(json!([fixture]));
        let conversation = &parse.conversations[0];
        assert!(matches!(
            conversation.updated_at,
            RecordedTime::Unknown { .. }
        ));
        assert!(matches!(
            conversation.created_at,
            RecordedTime::Unknown { .. }
        ));
        // A spelling that was carried but not read is counted; a field the export
        // did not carry is not, because nothing was there to read.
        assert_eq!(parse.unreadable.get("conversations[].updated_at"), Some(&1));
        assert_eq!(parse.unreadable.get("conversations[].inserted_at"), None);
    }

    #[test]
    fn fragment_kinds_are_labelled_and_counted_by_their_own_spelling() {
        let mut fixture = linear_conversation();
        message_mut(&mut fixture, "2").insert(
            "fragments".to_string(),
            json!([
                { "type": "THINK", "content": "fixture reasoning" },
                { "type": "VIDEO", "content": "fixture media" },
                { "type": "SEARCH", "results": [] },
                { "type": "FILE", "files": [] }
            ]),
        );

        let parse = parse(json!([fixture]));
        assert_eq!(
            parse.fragment_counts(),
            BTreeMap::from([
                ("FILE", 1),
                ("REQUEST", 1),
                ("SEARCH", 1),
                ("THINK", 1),
                ("VIDEO", 1),
            ])
        );
        let unknown = &fragments_of(parse.conversations[0].messages()[1])[1];
        assert_eq!(
            unknown.kind,
            FragmentKind::Unknown {
                spelled: "VIDEO".to_string()
            }
        );
        assert_eq!(unknown.kind.label(), "VIDEO");
        // An unrecognised kind is named on the fragment, not counted as an
        // unreadable field — and its text is kept.
        assert_eq!(unknown.content, Field::Value("fixture media".to_string()));
        assert!(parse.unreadable.is_empty(), "{:?}", parse.unreadable);
    }

    #[test]
    fn a_fragment_with_no_readable_type_is_unreadable_and_never_text() {
        let mut fixture = linear_conversation();
        message_mut(&mut fixture, "2").insert(
            "fragments".to_string(),
            json!([
                { "content": "fixture text" },
                { "type": 7, "content": "fixture other text" }
            ]),
        );

        let parse = parse(json!([fixture]));
        let fragments = fragments_of(parse.conversations[0].messages()[1]);
        assert!(matches!(fragments[0].kind, FragmentKind::Unreadable { .. }));
        assert!(matches!(fragments[1].kind, FragmentKind::Unreadable { .. }));
        assert_eq!(fragments[0].kind.label(), "unnamed");
        // The text is still the export's text: an unreadable kind is not a reason
        // to treat the fragment as empty.
        assert_eq!(
            fragments[0].content,
            Field::Value("fixture text".to_string())
        );
        // Only the carried-but-unreadable `type` is counted.
        assert_eq!(
            parse
                .unreadable
                .get("conversations[].mapping[].message.fragments[].type"),
            Some(&1)
        );
    }

    #[test]
    fn attachment_references_keep_their_two_spellings_and_their_nulls() {
        let mut fixture = linear_conversation();
        let message = message_mut(&mut fixture, "2");
        message.insert(
            "files".to_string(),
            json!([{ "id": "fixture-file-1", "file_name": "fixture-a.txt" }]),
        );
        message.insert(
            "fragments".to_string(),
            json!([{
                "type": "FILE",
                "files": [
                    { "file_id": "fixture-file-1", "file_name": "fixture-a.txt", "file_size": 107841 },
                    { "file_id": "fixture-file-2", "file_name": "fixture-b.txt", "file_size": null }
                ]
            }]),
        );

        let parse = parse(json!([fixture]));
        let conversation = &parse.conversations[0];
        assert_eq!(conversation.attachments.len(), 3);
        let message_level = conversation
            .attachments
            .iter()
            .find(|attachment| attachment.origin == AttachmentOrigin::Message)
            .expect("the message-level spelling is read");
        assert_eq!(message_level.id, Field::Value("fixture-file-1".to_string()));
        // The message spelling carries no size: absent, never zero.
        assert_eq!(message_level.bytes, Field::Absent);
        let fragment_level: Vec<&AttachmentRef> = conversation
            .attachments
            .iter()
            .filter(|attachment| attachment.origin == AttachmentOrigin::Fragment)
            .collect();
        assert_eq!(
            fragment_level[0].id,
            Field::Value("fixture-file-1".to_string())
        );
        assert_eq!(fragment_level[0].bytes, Field::Value(107_841));
        // A `null` size is the export's own null, not a size of zero.
        assert_eq!(fragment_level[1].bytes, Field::Null);
        assert!(parse.unreadable.is_empty(), "{:?}", parse.unreadable);
    }

    #[test]
    fn search_results_keep_their_numbers_and_their_nulls() {
        let mut fixture = linear_conversation();
        message_mut(&mut fixture, "2").insert(
            "fragments".to_string(),
            json!([{
                "type": "SEARCH",
                "results": [{
                    "url": "https://example.invalid/a",
                    "title": "fixture result",
                    "snippet": "fixture snippet",
                    "cite_index": null,
                    "published_at": 1718582400.0,
                    "site_icon": "https://example.invalid/icon",
                    "site_name": null,
                    "query_indexes": [0, 1]
                }]
            }]),
        );

        let parse = parse(json!([fixture]));
        let results = &fragments_of(parse.conversations[0].messages()[1])[0].results;
        let Field::Value(results) = results else {
            panic!("the fixture carries one readable result");
        };
        assert_eq!(results.len(), 1);
        // `1718582400.0` is the same value as `1718582400`.
        assert_eq!(results[0].published_at, Field::Value(1_718_582_400));
        assert_eq!(results[0].cite_index, Field::Null);
        assert_eq!(results[0].site_name, Field::Null);
        assert_eq!(results[0].query_indexes, Field::Value(vec![0_i64, 1]));
        assert!(parse.unreadable.is_empty(), "{:?}", parse.unreadable);
    }

    #[test]
    fn an_unrecognised_field_name_is_counted_at_its_own_path() {
        let mut fixture = linear_conversation();
        let object = fixture.as_object_mut().expect("the fixture is an object");
        object.insert(
            "conversation_template_id".to_string(),
            json!("fixture-template"),
        );
        object.insert("title".to_string(), json!(7));

        let parse = parse(json!([fixture]));
        assert_eq!(parse.conversations.len(), 1);
        assert_eq!(
            parse
                .unreadable
                .get("conversations[].conversation_template_id"),
            Some(&1)
        );
        // A known field carried in a shape this build cannot read is a separate
        // state, and lands under the field's own path.
        assert_eq!(parse.unreadable.get("conversations[].title"), Some(&1));
        assert!(matches!(
            parse.conversations[0].title,
            Field::Unreadable { .. }
        ));
    }

    #[test]
    fn the_platform_id_is_the_web_capture_id() {
        // An imported conversation and a captured one must land in one bucket, so
        // the import platform id is the capture harness id, not a new one.
        assert!(crate::activity::WEB_HARNESSES.contains(&PLATFORM));
    }

    #[test]
    fn a_file_that_is_not_an_export_is_a_named_failure() {
        assert!(matches!(
            parse_export(b"{\"id\": 1,"),
            Err(ExportFailure::NotJson { .. })
        ));
        assert_eq!(
            parse_export(b"null"),
            Err(ExportFailure::NotAnArray { found: "null" })
        );
        assert_eq!(
            parse_export(b"\"fixture\""),
            Err(ExportFailure::NotAnArray { found: "string" })
        );
        // An export that really holds no conversation is a measurement.
        let empty = parse_export(b"[]").expect("an empty export is readable");
        assert!(empty.conversations.is_empty());
        assert!(empty.failures.is_empty());
        assert!(empty.unreadable.is_empty());
    }

    #[test]
    fn integral_numbers_are_read_in_both_spellings_and_never_rounded() {
        assert_eq!(integral(&json!(7)), Some(7));
        assert_eq!(integral(&json!(7.0)), Some(7));
        assert_eq!(integral(&json!(1_718_582_400.0)), Some(1_718_582_400));
        assert_eq!(integral(&json!(7.5)), None);
        assert_eq!(integral(&json!("7")), None);
        assert_eq!(integral(&json!(null)), None);
    }
}
