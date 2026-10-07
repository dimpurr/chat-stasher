//! The Gemini Takeout activity log — `My Activity/Gemini Apps/MyActivity.json`.
//!
//! Google Takeout delivers Gemini as an **activity log**, not as conversations:
//! the file is a top-level JSON array of *activity rows*, one per thing the
//! account did with Gemini. ADR-055 D3 is written for exactly this source — an
//! activity row is evidence of a prompt, not of a conversation — and this module
//! is the parser for it: bytes in, [`Observation`]s out, and nothing else. It
//! writes nothing, reads nothing from disk and is wired to no command.
//!
//! ## The measured shape
//!
//! Measured on the 2026-09-24 Takeout payload (26,508,335 B; TKO-OUT §1), every
//! number below read off that file rather than from a reference implementation:
//!
//! * the file is a top-level JSON **array** of 1,980 activity rows;
//! * a row carries `header` / `title` / `time` / `products` / `activityControls`
//!   (1,980/1,980 each), and most carry `details` (1,933), `safeHtmlItem` (1,925),
//!   `subtitles` (413), `attachedFiles` (351) and `imageFile` (145);
//! * `time` is an RFC 3339 string (1,977 rows with milliseconds, 3 without);
//! * **a row's conversation id is in `details[].url`**, spelled
//!   `https://gemini.google.com/app/<id>` with `name` holding the same string.
//!   The `<id>` is the conversation id our live capture files as the session id
//!   `c_<id>` (`apps/extension/lib/gemini-capture.ts`: the page URL carries the
//!   id *without* its `c_` prefix, which is why the canonical form re-adds it).
//!   The row does **not** carry a top-level `id`, which is why an earlier
//!   inventory that read `r["id"]` counted zero conversation ids (TKO-OUT §1);
//!   the id is nested one level down and this parser reads it there;
//! * 1,842 rows name exactly one conversation id, 91 name more than one and 47
//!   name none at all (those 47 are `Created` / `Used` / `Cleared` / `Gave` /
//!   `Selected` activity, not `Prompted` rows).
//!
//! ## The identity rule (ADR-055 D3)
//!
//! A row's identity is decided from `details[].url` and never invented:
//!
//! * **exactly one** distinct conversation id ⇒ [`Identity::Conversation`]; the
//!   join is that id;
//! * **none** ⇒ [`Identity::Unidentified`]; the session id is the single
//!   [`UNKNOWN_SESSION_ID`] stub and the join is prompt text
//!   ([`Join::WeakPromptText`]). The stub is one constant for every id-less row —
//!   never a hash of the prompt, never a per-row minted id, which is what ADR-055
//!   D3 and ADR-053 both refuse;
//! * **more than one** ⇒ [`Identity::Ambiguous`]; several conversations are named
//!   and this build does not pick one, so the session id is the same stub and the
//!   join is marked ambiguous. Picking the first would be inventing a session
//!   boundary the source did not state.
//!
//! ## What this module deliberately does not do
//!
//! * **It does not treat a row as a conversation.** A row is one activity in a
//!   conversation, so no bundle is built and no `web-capture` session is claimed
//!   here; that is the skeleton's decision (ADR-055 D4/D5).
//! * **It does not merge rows into conversations.** Grouping the observations of
//!   one conversation is the phase-2 derived view (ADR-055 D2/D5), which reads
//!   the id this parser carries and never re-parses the file.
//! * **It does not interpret a spelling it has not measured.** `time` is read as
//!   RFC 3339; any other spelling is `unknown` with its reason rather than
//!   converted through a guessed unit.
//!
//! ## How the three states are kept
//!
//! A value field is a [`Field`] and a timestamp is a [`RecordedTime`]: the value
//! the export carried, an explicit `null` it wrote, a field it did not carry at
//! all, or one it carried in a shape this build cannot read — never two of those
//! collapsed into one. A list field is a `Field<Vec<_>>` as well, so "the export
//! carries no list here" and "the export carries an empty list" stay apart; an
//! entry inside a list that this build cannot read is counted in
//! [`ExportParse::unreadable`] and left out, which makes every list in a result
//! the list this build *read*.
//!
//! Privacy line, as everywhere in this area: a row's `title` and `safeHtmlItem`
//! are user content. The parser carries them in the returned structs — that is
//! what an observation is for — but nothing here logs, prints or reports them,
//! and the only strings that cross into a report are field names and reasons.

use crate::activity::TimeSource;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The platform these observations belong to: the `<platform>` of
/// `chat-stasher import <platform> <export-file>` (ADR-055 D4). It is the same
/// id the live web capture of this platform uses (`activity::WEB_HARNESSES`),
/// which is what makes an imported observation and a captured one land in one
/// bucket instead of two.
pub const PLATFORM: &str = "gemini";

/// The session id a row that names no single conversation gets.
///
/// One constant, not a per-row value: an id-less row is a prompt with no
/// conversation to file it under, and ADR-055 D3 / ADR-053 refuse a minted id
/// because it would appear in session lists and coverage by construction. The
/// join is marked weak beside it, so a reader can never mistake the stub for a
/// conversation id.
pub const UNKNOWN_SESSION_ID: &str = "unknown";

/// The prefix our Gemini web-capture session ids carry (`gemini.c_<id>`). The
/// Takeout link spells the same id without it, so the canonical form is restored
/// here — see the module docs.
const SESSION_ID_PREFIX: &str = "c_";

/// The link host a conversation id is read from.
const CONVERSATION_HOST: &str = "gemini.google.com";
/// The link path prefix a conversation id is read from.
const CONVERSATION_PATH: &str = "/app/";

/// The path that names one row, for a record that has no line number. A field
/// or list inside it is named by appending, e.g. `[].details[]`.
const ROW: &str = "[]";

/// The field names the measured shape carries, one row per level. A name that is
/// not in its row is reported as an unrecognised field rather than read as if it
/// were one of these — which is the only way a field the platform adds becomes
/// visible instead of silently dropped.
const ROW_KEYS: [&str; 10] = [
    "activityControls",
    "attachedFiles",
    "details",
    "header",
    "imageFile",
    "products",
    "safeHtmlItem",
    "subtitles",
    "time",
    "title",
];
const LINK_KEYS: [&str; 2] = ["name", "url"];
const SAFE_HTML_KEYS: [&str; 1] = ["html"];

/// Read one Gemini Takeout `MyActivity.json`.
///
/// The input is the file's bytes exactly as the platform produced them; the same
/// bytes always produce the same observations. A file that cannot be read as the
/// activity array at all is an [`ExportFailure`]; an array entry that cannot
/// become an observation is a [`RowFailure`], kept in
/// [`ExportParse::failures`] beside the observations — never dropped, because a
/// caller that counted only the successes would report a smaller number that
/// looks complete (ADR-014, CLAUDE.md invariant 1).
///
/// The whole document is held in memory while it is read. The measured file is
/// 26 MB; a platform whose export is much larger needs a streaming reader before
/// it is wired in, and this signature says so.
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
    for (index, entry) in entries.iter().enumerate() {
        reader.row(index, entry);
    }
    Ok(reader.finish())
}

/// Why an export file could not be read as an activity log at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportFailure {
    /// The bytes are not JSON this build can read: invalid UTF-8, or a syntax
    /// error at the position named in `why` (a parser position, never any of the
    /// export's own text).
    NotJson { why: String },
    /// Valid JSON, but not the array an activity log is. The kind actually found
    /// is named — `"object"`, `"string"`, … — so a caller can tell the wrong file
    /// from a corrupt one.
    NotAnArray { found: &'static str },
}

/// One Gemini Takeout activity log, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportParse {
    /// The observations this build could read, in the file's own order.
    pub observations: Vec<Observation>,
    /// The entries it could not, in the file's own order, each with the array
    /// index it sat at.
    pub failures: Vec<RowFailure>,
    /// Every spot where the export carried something this build did not read,
    /// keyed by path and counted. The path tells the three sources apart:
    ///
    /// * a **field name** the measured shape does not carry, e.g.
    ///   `[].conversationMetadata` — how a field the platform adds becomes
    ///   visible instead of silently dropped;
    /// * a known field carried in a **shape** this build cannot read, e.g.
    ///   `[].time` holding a number;
    /// * an **entry inside a list** this build cannot read, e.g.
    ///   `[].details[]` holding a string.
    ///
    /// An **absent** `time` is not counted here: nothing was carried, so there is
    /// no spot this build failed to read, and the absence is stated on the
    /// observation as [`RecordedTime::Unknown`]. A `time` that *was* carried in a
    /// spelling or shape this build cannot read is counted, because that is a spot
    /// it failed to read — and it is `Unknown` on the observation as well, the
    /// value-side report of the same spot.
    pub unreadable: BTreeMap<String, usize>,
}

impl ExportParse {
    /// How many rows carry exactly one conversation id.
    pub fn identified_rows(&self) -> usize {
        self.observations
            .iter()
            .filter(|observation| matches!(observation.identity, Identity::Conversation { .. }))
            .count()
    }

    /// How many rows name no conversation id at all.
    pub fn unidentified_rows(&self) -> usize {
        self.observations
            .iter()
            .filter(|observation| matches!(observation.identity, Identity::Unidentified))
            .count()
    }

    /// How many rows name more than one conversation id.
    pub fn ambiguous_rows(&self) -> usize {
        self.observations
            .iter()
            .filter(|observation| matches!(observation.identity, Identity::Ambiguous { .. }))
            .count()
    }

    /// Every distinct conversation id the observations name, sorted. A caller
    /// uses this to join without re-reading the file.
    pub fn conversation_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .observations
            .iter()
            .flat_map(Observation::conversation_ids)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

/// An array entry that this build could not turn into an observation.
///
/// It is reported, never dropped: the index and the reason are both kept, so a
/// caller can say *which* entry it could not read instead of reporting a number
/// that looks complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowFailure {
    pub index: usize,
    pub reason: RowFailureReason,
}

/// Why one array entry could not become an observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowFailureReason {
    /// The entry is not a JSON object.
    NotAnObject { found: &'static str },
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

/// Which conversation(s) a row's `details[].url` names.
///
/// The three variants exist because the three answers need three different
/// readings, and collapsing them is how a prompt would come to be filed under an
/// invented session (ADR-055 D3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// Exactly one distinct conversation id. The id is in canonical form — the
    /// link's own spelling with our `c_` prefix restored — and it is the id our
    /// `gemini.<session id>` web-capture directory already uses.
    Conversation { id: String },
    /// The row names no conversation id: it is a prompt with no conversation to
    /// file it under, and [`UNKNOWN_SESSION_ID`] stands in.
    Unidentified,
    /// The row names more than one distinct conversation id. This build does not
    /// choose among them, so [`UNKNOWN_SESSION_ID`] stands in and the join is
    /// marked ambiguous; `candidates` is how many were named.
    Ambiguous { candidates: usize },
}

/// How an observation can be joined to a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Join {
    /// The row carries a conversation id; the join is on it.
    ConversationId,
    /// The row carries no conversation id; the only possible join is the prompt
    /// text, which is weak by construction (TKO-OUT §1: the HTML and the
    /// `.index.jsonl` are lossy).
    WeakPromptText,
    /// The row names several conversations, so no single id is chosen and the
    /// join is not usable until a reader resolves the ambiguity.
    AmbiguousConversationIds,
}

/// One link the export carried, as `{name, url}`.
///
/// Both fields are [`TextField`] because either may be absent: measured, every
/// `details` entry carries both, while a `subtitles` entry may carry only
/// `name`. The two states are not the same claim and are not stored as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// `name`. On a `details` entry this is the conversation URL itself; on a
    /// `subtitles` entry it is a label.
    pub name: TextField,
    /// `url`.
    pub url: TextField,
}

/// One Gemini activity row, read as an import observation.
///
/// The struct carries the row field-for-field (ADR-052 D6: no field projection
/// at capture, nulls and omissions preserved) plus the identity decision. The
/// content fields — `title`, `safe_html` — are user content: carried, never
/// logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// The row's position in the export array, for a caller that has to name it.
    pub index: usize,
    /// `time`.
    pub time: RecordedTime,
    /// The conversation(s) `details[].url` names — see [`Identity`].
    pub identity: Identity,
    /// `header`. Measured: `Gemini Apps`, and `Gemini in Google Messages` once.
    pub header: TextField,
    /// `title`. The activity title: `Prompted <text>` for a prompt, a short verb
    /// phrase for the rest. User content.
    pub title: TextField,
    /// `products`.
    pub products: Field<Vec<String>>,
    /// `activityControls`.
    pub activity_controls: Field<Vec<String>>,
    /// `details`: the links the activity carried. The conversation id is read
    /// from here.
    pub details: Field<Vec<Link>>,
    /// `subtitles`.
    pub subtitles: Field<Vec<Link>>,
    /// `safeHtmlItem[].html`: the prompt's HTML form. User content.
    pub safe_html: Field<Vec<String>>,
    /// `imageFile`.
    pub image_file: TextField,
    /// `attachedFiles`.
    pub attached_files: Field<Vec<String>>,
}

impl Observation {
    /// The session id this observation files under: the canonical conversation id
    /// when the row named exactly one, and [`UNKNOWN_SESSION_ID`] otherwise.
    pub fn session_id(&self) -> &str {
        match &self.identity {
            Identity::Conversation { id } => id,
            Identity::Unidentified | Identity::Ambiguous { .. } => UNKNOWN_SESSION_ID,
        }
    }

    /// How this observation can be joined — see [`Join`].
    pub fn join(&self) -> Join {
        match self.identity {
            Identity::Conversation { .. } => Join::ConversationId,
            Identity::Unidentified => Join::WeakPromptText,
            Identity::Ambiguous { .. } => Join::AmbiguousConversationIds,
        }
    }

    /// The distinct conversation ids the row's `details` named, in first-seen
    /// order. Empty when it named none.
    pub fn conversation_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        if let Field::Value(details) = &self.details {
            for link in details {
                if let Field::Value(url) = &link.url {
                    if let Some(id) = conversation_id_from_url(url) {
                        if !ids.contains(&id) {
                            ids.push(id);
                        }
                    }
                }
            }
        }
        ids
    }
}

/// Collects the observations, the failures and the unreadable tally as one
/// export is read, so the readers below can note what they could not read
/// without carrying a tally through every call.
#[derive(Default)]
struct Reader {
    observations: Vec<Observation>,
    failures: Vec<RowFailure>,
    unreadable: BTreeMap<String, usize>,
}

impl Reader {
    fn finish(self) -> ExportParse {
        ExportParse {
            observations: self.observations,
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

    /// A list of `{name, url}` links. An entry this build cannot read is counted
    /// and left out.
    fn link_list(
        &mut self,
        object: &Map<String, Value>,
        key: &str,
        path: &str,
    ) -> Field<Vec<Link>> {
        match object.get(key) {
            None => Field::Absent,
            Some(Value::Null) => Field::Null,
            Some(Value::Array(items)) => {
                let entry = format!("{path}.{key}[]");
                let mut links = Vec::with_capacity(items.len());
                for item in items {
                    if let Some(link) = self.link(item, &entry) {
                        links.push(link);
                    }
                }
                Field::Value(links)
            }
            Some(other) => self.wrong_shape(path, key, carried_as(other)),
        }
    }

    fn link(&mut self, value: &Value, path: &str) -> Option<Link> {
        let Some(object) = value.as_object() else {
            self.note(path);
            return None;
        };
        self.note_unrecognised(object, &LINK_KEYS, path);
        let name = self.text(object, "name", path);
        let url = self.text(object, "url", path);
        Some(Link { name, url })
    }

    /// `safeHtmlItem`: an array of `{html}` objects, read as the `html` strings.
    fn html_list(
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
                let mut html = Vec::with_capacity(items.len());
                for item in items {
                    let Some(object) = item.as_object() else {
                        self.note(&entry);
                        continue;
                    };
                    self.note_unrecognised(object, &SAFE_HTML_KEYS, &entry);
                    match object.get("html") {
                        Some(Value::String(text)) => html.push(text.clone()),
                        Some(_) => self.note(&format!("{entry}.html")),
                        None => {}
                    }
                }
                Field::Value(html)
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

    fn row(&mut self, index: usize, entry: &Value) {
        let Some(object) = entry.as_object() else {
            self.failures.push(RowFailure {
                index,
                reason: RowFailureReason::NotAnObject {
                    found: kind_of(entry),
                },
            });
            return;
        };
        self.note_unrecognised(object, &ROW_KEYS, ROW);

        let time = self.recorded_time(object, "time", ROW);
        let header = self.text(object, "header", ROW);
        let title = self.text(object, "title", ROW);
        let products = self.string_list(object, "products", ROW);
        let activity_controls = self.string_list(object, "activityControls", ROW);
        let details = self.link_list(object, "details", ROW);
        let subtitles = self.link_list(object, "subtitles", ROW);
        let safe_html = self.html_list(object, "safeHtmlItem", ROW);
        let image_file = self.text(object, "imageFile", ROW);
        let attached_files = self.string_list(object, "attachedFiles", ROW);

        let identity = identity_from_details(&details);
        self.observations.push(Observation {
            index,
            time,
            identity,
            header,
            title,
            products,
            activity_controls,
            details,
            subtitles,
            safe_html,
            image_file,
            attached_files,
        });
    }
}

/// The conversation id a `details[].url` names, in canonical form, when the link
/// is a Gemini conversation link.
///
/// The measured link is `https://gemini.google.com/app/<id>`; the id is returned
/// with our `c_` prefix restored so it equals the session id our live capture
/// files (`gemini.c_<id>`). A link to any other host or path — a `gems/view`
/// link, an external source, a share link — names no conversation here and is
/// carried only as a link.
fn conversation_id_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let rest = rest.strip_prefix(CONVERSATION_HOST)?;
    let rest = rest.strip_prefix(CONVERSATION_PATH)?;
    let id = match rest.find(['?', '#']) {
        Some(end) => &rest[..end],
        None => rest,
    };
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(format!("{SESSION_ID_PREFIX}{id}"))
}

/// The identity decision, as a pure function of the `details` field.
fn identity_from_details(details: &Field<Vec<Link>>) -> Identity {
    let mut ids: Vec<String> = Vec::new();
    if let Field::Value(links) = details {
        for link in links {
            if let Field::Value(url) = &link.url {
                if let Some(id) = conversation_id_from_url(url) {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
        }
    }
    if ids.is_empty() {
        return Identity::Unidentified;
    }
    if ids.len() > 1 {
        return Identity::Ambiguous {
            candidates: ids.len(),
        };
    }
    // Exactly one: `swap_remove(0)` on a one-element vector, so no unwrap.
    Identity::Conversation {
        id: ids.swap_remove(0),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The measured row shape: a `Prompted` row whose `details` names one
    /// conversation, plus the fields the real file carries. Text is invented.
    fn prompted_row(id: &str) -> Value {
        json!({
            "header": "Gemini Apps",
            "title": "Prompted fixture question",
            "time": "2026-01-02T03:04:05.006Z",
            "products": ["Gemini Apps"],
            "activityControls": ["Gemini Apps Activity"],
            "details": [
                { "name": format!("https://gemini.google.com/app/{id}"),
                  "url": format!("https://gemini.google.com/app/{id}") }
            ],
            "safeHtmlItem": [{ "html": "<b>fixture question</b>" }]
        })
    }

    fn parse(fixture: Value) -> ExportParse {
        parse_export(fixture.to_string().as_bytes())
            .expect("the fixture is an activity log this build reads")
    }

    #[test]
    fn the_measured_shape_reads_into_one_identified_observation() {
        let parse = parse(json!([prompted_row("0123456789abcdef")]));

        assert_eq!(PLATFORM, "gemini");
        assert!(parse.failures.is_empty());
        assert!(parse.unreadable.is_empty(), "{:?}", parse.unreadable);
        let observation = &parse.observations[0];
        assert_eq!(observation.index, 0);
        assert_eq!(
            observation.identity,
            Identity::Conversation {
                id: "c_0123456789abcdef".to_string()
            }
        );
        assert_eq!(observation.session_id(), "c_0123456789abcdef");
        assert_eq!(observation.join(), Join::ConversationId);
        assert_eq!(parse.identified_rows(), 1);
        assert_eq!(
            observation.time,
            RecordedTime::Known {
                unix: 1_767_323_045,
                source: TimeSource::Exact,
            }
        );
        assert_eq!(
            observation.title,
            Field::Value("Prompted fixture question".to_string())
        );
        assert_eq!(
            observation.safe_html,
            Field::Value(vec!["<b>fixture question</b>".to_string()])
        );
        assert_eq!(
            parse.conversation_ids(),
            vec!["c_0123456789abcdef".to_string()]
        );
    }

    #[test]
    fn a_row_with_no_conversation_id_gets_the_stub_and_a_weak_join() {
        let parse = parse(json!([
            {
                "header": "Gemini Apps",
                "title": "Created fixture gem",
                "time": "2026-01-02T03:04:05Z",
                "products": ["Gemini Apps"],
                "activityControls": ["Gemini Apps Activity"]
            },
            {
                "header": "Gemini Apps",
                "title": "Prompted fixture with a non-conversation link",
                "time": "2026-01-02T03:04:06.000Z",
                "details": [{ "name": "fixture", "url": "https://gemini.google.com/gems/view" }]
            }
        ]));

        assert_eq!(parse.unidentified_rows(), 2);
        assert_eq!(parse.identified_rows(), 0);
        for observation in &parse.observations {
            assert_eq!(observation.identity, Identity::Unidentified);
            assert_eq!(observation.session_id(), UNKNOWN_SESSION_ID);
            assert_eq!(observation.join(), Join::WeakPromptText);
            assert!(observation.conversation_ids().is_empty());
        }
        // The stub is one constant shared by every id-less row, never a per-row
        // value: two rows with different prompts still file under the same id.
        assert_eq!(
            parse.observations[0].session_id(),
            parse.observations[1].session_id()
        );
        assert!(parse.conversation_ids().is_empty());
    }

    #[test]
    fn a_row_that_names_several_conversations_is_ambiguous_not_the_first() {
        let parsed = parse(json!([
            {
                "header": "Gemini Apps",
                "title": "Prompted fixture with two conversations",
                "time": "2026-01-02T03:04:05.000Z",
                "details": [
                    { "name": "a", "url": "https://gemini.google.com/app/aaaaaaaaaaaaaaaa" },
                    { "name": "b", "url": "https://gemini.google.com/app/bbbbbbbbbbbbbbbb" }
                ]
            }
        ]));

        let observation = &parsed.observations[0];
        assert_eq!(observation.identity, Identity::Ambiguous { candidates: 2 });
        assert_eq!(observation.session_id(), UNKNOWN_SESSION_ID);
        assert_eq!(observation.join(), Join::AmbiguousConversationIds);
        assert_eq!(
            observation.conversation_ids(),
            vec![
                "c_aaaaaaaaaaaaaaaa".to_string(),
                "c_bbbbbbbbbbbbbbbb".to_string()
            ]
        );
        assert_eq!(parsed.ambiguous_rows(), 1);
        // A repeated id is one conversation, not two.
        let repeated = parse(json!([
            {
                "header": "Gemini Apps",
                "title": "Prompted fixture repeating one id",
                "details": [
                    { "name": "a", "url": "https://gemini.google.com/app/aaaaaaaaaaaaaaaa" },
                    { "name": "a", "url": "https://gemini.google.com/app/aaaaaaaaaaaaaaaa" }
                ]
            }
        ]));
        assert_eq!(
            repeated.observations[0].identity,
            Identity::Conversation {
                id: "c_aaaaaaaaaaaaaaaa".to_string()
            }
        );
    }

    #[test]
    fn a_conversation_id_is_read_from_the_app_link_and_never_from_another_host() {
        assert_eq!(
            conversation_id_from_url("https://gemini.google.com/app/0123456789abcdef"),
            Some("c_0123456789abcdef".to_string())
        );
        assert_eq!(
            conversation_id_from_url("https://gemini.google.com/app/0123456789abcdef?hl=en"),
            Some("c_0123456789abcdef".to_string())
        );
        assert_eq!(
            conversation_id_from_url("https://gemini.google.com/gems/view"),
            None
        );
        assert_eq!(
            conversation_id_from_url("https://chatgpt.com/c/0123456789abcdef"),
            None
        );
        assert_eq!(
            conversation_id_from_url("https://gemini.google.com/app/"),
            None
        );
        assert_eq!(conversation_id_from_url("not a url"), None);
    }

    #[test]
    fn every_state_of_a_field_stays_its_own_state() {
        let parse = parse(json!([
            {
                "header": "Gemini Apps",
                "title": null,
                "time": "2026-01-02T03:04:05.000Z",
                "products": [],
                "details": null
            },
            {
                "header": "Gemini Apps",
                "time": "2026-01-02T03:04:06.000Z",
                "products": 7,
                "conversationMetadata": "fixture-extra"
            }
        ]));

        let first = &parse.observations[0];
        // `title` written as `null` is a stated absence...
        assert_eq!(first.title, Field::Null);
        // ... a field not carried at all is a different state...
        assert_eq!(parse.observations[1].title, Field::Absent);
        // ... and an empty list the export really wrote is a measurement.
        assert_eq!(first.products, Field::Value(Vec::new()));
        // A known field in an unreadable shape is the fourth state, counted
        // where it happened.
        assert!(matches!(
            parse.observations[1].products,
            Field::Unreadable { .. }
        ));
        assert_eq!(parse.unreadable.get("[].products"), Some(&1));
        // A field name the measured shape does not carry is visible, not dropped.
        assert_eq!(parse.unreadable.get("[].conversationMetadata"), Some(&1));
        // `details: null` is a stated absence, not "no id was readable".
        assert_eq!(first.details, Field::Null);
        assert_eq!(first.identity, Identity::Unidentified);
    }

    #[test]
    fn a_time_spelling_this_build_has_not_measured_is_unknown_not_an_epoch() {
        let parse = parse(json!([
            { "header": "Gemini Apps", "title": "fixture a", "time": "not a time" },
            { "header": "Gemini Apps", "title": "fixture b", "time": 1767325445 },
            { "header": "Gemini Apps", "title": "fixture c" }
        ]));

        for observation in &parse.observations {
            assert!(
                matches!(observation.time, RecordedTime::Unknown { .. }),
                "{:?}",
                observation.time
            );
        }
        assert!(!parse
            .observations
            .iter()
            .any(|observation| matches!(observation.time, RecordedTime::Known { unix: 0, .. })));
        assert_eq!(parse.unreadable.get("[].time"), Some(&2));
    }

    #[test]
    fn nothing_is_skipped_silently() {
        let parse = parse(json!([
            prompted_row("aaaaaaaaaaaaaaaa"),
            "fixture-not-an-object",
            42
        ]));

        assert_eq!(parse.observations.len(), 1);
        assert_eq!(parse.failures.len(), 2);
        assert_eq!(parse.failures[0].index, 1);
        assert_eq!(
            parse.failures[0].reason,
            RowFailureReason::NotAnObject { found: "string" }
        );
        assert_eq!(parse.failures[1].index, 2);
        assert_eq!(
            parse.failures[1].reason,
            RowFailureReason::NotAnObject { found: "number" }
        );
    }

    #[test]
    fn a_file_that_is_not_an_activity_log_is_a_named_failure() {
        assert!(matches!(
            parse_export(b"{ not json"),
            Err(ExportFailure::NotJson { .. })
        ));
        assert_eq!(
            parse_export(b"{\"rows\": []}"),
            Err(ExportFailure::NotAnArray { found: "object" })
        );
        // An export that really does hold no row is a measurement, not a
        // failure: zero observations, zero failures.
        let empty = parse_export(b"[]").expect("an empty log is readable");
        assert!(empty.observations.is_empty());
        assert!(empty.failures.is_empty());
    }
}
