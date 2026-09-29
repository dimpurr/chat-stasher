//! Local, disposable full-text index for one archive destination.
//!
//! The index is plaintext and lives in the OS cache directory. Its directory
//! is destination-scoped, marked before use, and never part of the archive.
//! SQLite is opened and validated on every operation; an unreadable or
//! incompatible database is reported for repair instead of being replaced.

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

/// The index schema this build of the tool reads and writes.
///
/// Bumped from 2 when the completed build started recording **which** sources
/// it could not read (`build_not_indexable_ids`). That key is not decoration: a
/// coverage claim is made over those ids, and an index that recorded only the
/// *count* of failed sessions cannot answer "is this session one of them?" —
/// reading its silence as "none of them" is exactly the unknown-recorded-as-
/// empty this module refuses. There is no honest reading of a version-2 index
/// as version 3, so it is refused by version and the reader is told to rebuild,
/// which is what `validate_schema` already does for every older layout.
const SCHEMA_VERSION: i64 = 3;
const MARKER: &str = ".chat-stasher-fts";
const MARKER_CONTENT: &[u8] = b"chat-stasher fts index v1\n";

/// The shortest query the trigram tokenizer can evaluate. Below it the index
/// can return no candidate at all, so a short query is *not answerable* — a
/// different answer from "matched nothing", and one the reader must not
/// collapse into it (29-UI-DESIGN §6.2.3).
pub const MIN_QUERY_CHARS: usize = 3;

/// How many matches one query may return before the set is reported as
/// truncated. A cap is a measurement of the cap, never of the archive, so
/// callers must say *at least* this many rather than this many.
pub const MAX_QUERY_MATCHES: usize = 10_000;

/// The delimiters `snippet()` puts around a matched span. Control characters
/// rather than brackets: a conversation legitimately contains `[` and `]`, and
/// a marker that can also be conversation text cannot be parsed back out.
const MARK_OPEN: &str = "\u{1}";
const MARK_CLOSE: &str = "\u{2}";

/// One run of an excerpt, and whether the index marked it as part of the match.
///
/// The text is owned because a dropped marker can sit in the middle of a run,
/// and two runs that end up with the same flag must still come back as one:
/// adjacent segments never share a flag, so a caller may treat the list as an
/// alternation. The markers themselves never leave this module — a caller that
/// received the raw excerpt could put a control character into a page, and one
/// that split on brackets itself would reintroduce exactly the ambiguity these
/// markers exist to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub matched: bool,
    pub text: String,
}

/// Split an excerpt into marked and unmarked runs.
///
/// Unbalanced markers are resolved rather than rejected: an open mark runs to
/// the end of the string, and a close with no open is dropped. Both can only
/// come from a truncated excerpt, and neither is worth failing a page over —
/// the alternative would be printing a control character at a reader.
pub fn marked_segments(excerpt: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut current = String::new();
    let mut matched = false;
    let push = |matched: bool, text: &str, out: &mut Vec<Segment>| {
        out.push(Segment {
            matched,
            text: text.to_string(),
        });
    };
    for character in excerpt.chars() {
        let opens = character == '\u{1}';
        let closes = character == '\u{2}';
        // A close with no open is dropped, and dropping it must not split the
        // run it sits in: the flag only turns *on* at an open, and off at a
        // close only when it was on.
        if closes && !matched {
            continue;
        }
        if opens && matched {
            continue;
        }
        if opens || closes {
            if !current.is_empty() {
                push(matched, &current, &mut out);
                current.clear();
            }
            matched = opens;
            continue;
        }
        current.push(character);
    }
    if !current.is_empty() {
        push(matched, &current, &mut out);
    }
    out
}

/// One archived conversation the build must consider.
///
/// A source is either fingerprinted — its text is read only when that
/// fingerprint differs from the row already indexed — or, when the archive
/// cannot describe it at all, named with the reason it cannot be read. The
/// second case is a variant rather than an error returned by whoever collected
/// the sources, because one such shard is one session's problem: letting it
/// fail the collection is the archive-wide abort this exists to prevent (C1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceDoc {
    /// A source whose archive bytes were fingerprinted.
    Fingerprinted { id: String, source_sha256: String },
    /// A source the archive could not describe — a shard that holds bytes and
    /// names no content IDs, so there is nothing to fingerprint and nothing to
    /// read. It is recorded in [`BuildStats::not_indexable`] with this reason,
    /// and it is never matched against the previous build: an unreadable source
    /// is not an unchanged one, so it is re-attempted and re-reported on every
    /// build until the archive can describe it again.
    Unreadable { id: String, reason: String },
}

impl SourceDoc {
    /// A source whose archive bytes were fingerprinted.
    pub fn fingerprinted(id: impl Into<String>, source_sha256: impl Into<String>) -> Self {
        Self::Fingerprinted {
            id: id.into(),
            source_sha256: source_sha256.into(),
        }
    }

    /// A source the archive could not describe, with the reason to report.
    pub fn unreadable(id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Unreadable {
            id: id.into(),
            reason: reason.into(),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Fingerprinted { id, .. } | Self::Unreadable { id, .. } => id,
        }
    }
}

/// Text returned by the archive reader for a changed session.
///
/// `message_offsets` is where each indexed message's text starts in `body`,
/// as a character offset, in order; it is what turns a match position into a
/// message number, which is what a `/reader#m<n>` deep link needs. It is a
/// separate field rather than something re-derived from `body` because the
/// message boundaries are known only while the lines are being read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocText {
    pub title: String,
    pub body: String,
    pub message_offsets: Vec<usize>,
}

/// The text a build's load step produced, and how much of the archive it read
/// to produce it.
///
/// `bytes_read` is what a build reports as its volume (C6): the plaintext bytes
/// read for that source, in the same unit `FulltextCost::plaintext_bytes`
/// estimates. A loader that synthesizes text reports the length of the source
/// it stands in for.
#[derive(Debug, Clone)]
pub struct LoadedDoc {
    pub text: DocText,
    pub bytes_read: u64,
}

/// A load that failed, and how much of the archive it had read by then.
///
/// The bytes are carried on the failure rather than dropped with it because a
/// build's volume is what it *read*, not what it managed to store: a shard read
/// in full and then found unparsable was still read, and a summary that omitted
/// it would report a smaller archive read than happened (C6).
#[derive(Debug)]
pub struct LoadFailure {
    pub bytes_read: u64,
    pub error: anyhow::Error,
}

impl LoadFailure {
    pub fn new(bytes_read: u64, error: impl Into<anyhow::Error>) -> Self {
        Self {
            bytes_read,
            error: error.into(),
        }
    }
}

/// Result of an incremental build.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildStats {
    pub documents: usize,
    /// Sources the build attempted to read: every changed fingerprint, plus
    /// every source the archive could not describe at all. This is
    /// `indexed + not_indexable.len()`: every source the build took on is
    /// either stored or named as not indexable, never dropped silently.
    pub read: usize,
    pub unchanged: usize,
    pub removed: usize,
    /// Sources whose load succeeded and whose text was stored (a subset of
    /// `read`).
    pub indexed: usize,
    /// Sources whose load failed, in order, each with its reason. A malformed
    /// or unreadable shard must not abort the rest of the archive's build, and
    /// naming the session is what lets a user act on it (C1).
    pub not_indexable: Vec<(String, String)>,
    /// Sources whose load succeeded but whose extracted body was empty. They
    /// are stored and counted as indexed, yet cannot match a query; a count
    /// voices that false-negative instead of folding it into `indexed` (C2).
    pub empty_body: usize,
    /// Archive bytes read for every source the build read — including the ones
    /// whose load then failed, which were read too (C6).
    pub bytes_read: u64,
}

/// What `index check` found: whether a build finished, and if so the health of
/// what it left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    pub documents: usize,
    pub status: CheckStatus,
}

/// The recorded result of the last build attempt, so `index check` does not
/// call an index that never completed, or that only partially completed,
/// "valid" (C7).
///
/// The counters below describe **the last build**, not the index as a whole: a
/// build that changed nothing reads no sessions and reports zeroes while the
/// index still holds every document. They are reported as `last_build_*` for
/// that reason, and [`CheckReport::documents`] is the index's own size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    /// A build finished and committed, every source was indexed, and at least
    /// one document exists.
    Valid {
        indexed: usize,
        empty_body: usize,
        bytes_read: u64,
    },
    /// A build finished but either left the archive not fully searchable (some
    /// sources were not indexable) or indexed nothing at all. A zero-document
    /// index is a completed measurement of emptiness, not a valid index.
    Partial {
        indexed: usize,
        not_indexable: usize,
        empty_body: usize,
        bytes_read: u64,
    },
    /// No build has completed: the recorded state is missing or not
    /// `completed`. An index written before this accounting existed lands here
    /// too, because nothing recorded can vouch for it. The index is not usable
    /// until a build records a completed outcome.
    Incomplete,
}

/// One ranked match, complete enough to render: the label, the excerpt with
/// the matched span delimited by [`MARK_OPEN`]/[`MARK_CLOSE`], and how well it
/// ranked. Nothing here is markup — the excerpt is conversation text and the
/// caller escapes it.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedMatch {
    pub id: String,
    pub title: String,
    /// The conversation excerpt, or `None` when the body held no match.
    ///
    /// `None` is load-bearing. `snippet()` on the body returns the body's
    /// *first* 240 tokens with nothing marked when the match was in another
    /// column, which reads exactly like an excerpt of a match that is not
    /// there — measured, not assumed. The marker is therefore the test for
    /// "this excerpt is about the match": a document that matched only in its
    /// label reports no body excerpt at all, and the caller renders the label,
    /// which is the text that did match.
    pub snippet: Option<String>,
    pub rank: f64,
}

/// Every match a query produced, or as many as the cap allowed.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchSet {
    /// Matches in this answer — the whole set unless `truncated`.
    pub matches: Vec<RankedMatch>,
    /// True when the index held more matches than [`MAX_QUERY_MATCHES`]. The
    /// count above is then a floor and must be reported as one.
    pub truncated: bool,
}

impl MatchSet {
    pub fn len(&self) -> usize {
        self.matches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }
}

/// A query the trigram index cannot evaluate at all, kept apart from an empty
/// result: answering "nothing matched" for a one-character query would turn
/// "this index cannot look" into "I looked and it is not there".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryTooShort {
    pub chars: usize,
    pub minimum: usize,
}

/// What an index holds beyond the document count `check` returns: which
/// sessions its text came from, and when it was last written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSummary {
    /// The id — `<machine>/<session_id>` — of every document the index holds.
    ///
    /// The ids themselves rather than a count per machine, because a coverage
    /// claim is about **which** sessions can be looked up: a session replaced
    /// by another on the same machine leaves every per-machine count equal, so
    /// counts cannot tell "this view is indexed" from "a different set of
    /// sessions with the same size is indexed". The machine grouping is
    /// derived from these where it is asked for
    /// ([`crate::ui::machine_of_document_id`]), so there is one representation
    /// of what the index holds and it cannot disagree with itself.
    pub ids: std::collections::BTreeSet<String>,
    /// The ids the **last build attempt** named not indexable.
    ///
    /// A session here is one the index cannot vouch for: either it has no row
    /// at all (the build never read it) or the row it has is text from an
    /// *earlier* read, which the failed re-read did not replace. So a zero-hit
    /// answer for one of these is a hole, not an absence, and coverage must
    /// exclude them — the same distinction [`Index::check`] draws when it calls
    /// such a build `partial`.
    ///
    /// It is a set of ids and not a count because a coverage made of counts
    /// cannot say *which* session is unanswerable; replace one session by
    /// another on the same machine and every count stays equal. An id here may
    /// or may not also appear in `ids` — an unreadable source that a previous
    /// build had read is still in the index.
    pub not_indexable: std::collections::BTreeSet<String>,
    /// The index file's last modification time. This is a **file mtime**, not a
    /// recorded build time — the index records no build time of its own, and
    /// reporting a computed one would be inventing a fact about when the text
    /// it holds was read.
    pub written_unix: Option<i64>,
}

impl IndexSummary {
    /// True when the index can answer for `id`: it holds a document for the
    /// session **and** the last build did not fail to read that session.
    ///
    /// This is the one predicate a coverage claim is built from. Testing
    /// membership in `ids` alone would count a session whose latest re-read
    /// failed as covered, and a query that found nothing would then be reported
    /// as a complete answer for text the current shard never supplied.
    pub fn covers(&self, id: &str) -> bool {
        self.ids.contains(id) && !self.not_indexable.contains(id)
    }
}

/// Where a query landed inside one session.
///
/// Three states, not an `Option`: a match in the session's *label* and a match
/// the tokenizer found but which could not be located again in the stored text
/// are different answers, and only the first can become a message anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchPlace {
    /// The first occurrence sits in message `ordinal` (0-based, over the
    /// messages the index holds for this session).
    Message { ordinal: usize },
    /// The first occurrence is in the session's label, which is not a message,
    /// so there is no message to anchor to.
    Label,
    /// The index matched this document but the literal could not be found
    /// again in the stored text. Reported as itself; never guessed into a
    /// message number.
    NotRelocated,
}

/// Open the index root for a single destination.
#[derive(Debug, Clone)]
pub struct Index {
    root: PathBuf,
    db_path: PathBuf,
    /// The index file state at which [`validate_schema`] last ran cleanly in
    /// this process, shared across clones of this value. `None` until the first
    /// full validation. Read paths consult this and skip the full integrity
    /// scan while the file is unchanged, so a text query does not re-validate
    /// a multi-gigabyte index on every call (C10); a rebuild changes the state
    /// and forces the next read to re-validate exactly once.
    validated_mtime: std::sync::Arc<std::sync::Mutex<Option<DbIdentity>>>,
}

/// Extract only user/assistant text. Tool calls and tool results are not
/// indexed. Unknown JSON shapes contribute no guessed text.
pub fn extract_index_text(raw: &[u8]) -> Result<(String, String)> {
    let extracted = extract_index_document(raw)?;
    Ok((extracted.title, extracted.body))
}

/// The same extraction, keeping where each message starts in the body.
///
/// The offsets are character offsets and they are what lets a match position
/// become a message number. The join between messages is one `\n`, which is
/// deliberately not part of any message: the offset recorded for a message is
/// the position of its own first character, so the newline before it belongs
/// to no message and a match on it can only ever be [`MatchPlace::NotRelocated`]
/// (the literal is not in the text) rather than being attributed to a
/// neighbour.
pub fn extract_index_document(raw: &[u8]) -> Result<DocText> {
    let raw = std::str::from_utf8(raw).context("archived conversation is not UTF-8 JSONL")?;
    let mut title = String::new();
    let mut messages = Vec::new();
    for (line_number, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str::<serde_json::Value>(line).with_context(|| {
            format!("invalid archived JSONL record at line {}", line_number + 1)
        })?;
        if title.is_empty() {
            title = value
                .get("title")
                .or_else(|| value.get("name"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
        }
        collect_turn_text(&value, &mut messages);
    }
    let mut body = String::new();
    let mut message_offsets = Vec::with_capacity(messages.len());
    for message in &messages {
        if !body.is_empty() {
            body.push('\n');
        }
        message_offsets.push(body.chars().count());
        body.push_str(message);
    }
    Ok(DocText {
        title,
        body,
        message_offsets,
    })
}

fn collect_turn_text(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Array(values) => {
            for child in values {
                collect_turn_text(child, out);
            }
        }
        serde_json::Value::Object(object) => {
            if let Some(message) = object.get("message") {
                let role = message
                    .get("author")
                    .and_then(|author| author.get("role"))
                    .or_else(|| message.get("role"))
                    .or_else(|| object.get("role"))
                    .and_then(serde_json::Value::as_str);
                if matches!(role, Some("user" | "assistant")) {
                    if let Some(content) = message.get("content") {
                        collect_content(content, out);
                    }
                    return;
                }
            }
            let role = object
                .get("role")
                .or_else(|| object.get("speaker"))
                .or_else(|| object.get("type"))
                .and_then(serde_json::Value::as_str);
            if matches!(role, Some("user" | "assistant")) {
                if let Some(content) = object.get("content").or_else(|| object.get("text")) {
                    collect_content(content, out);
                }
            } else {
                for child in object.values() {
                    collect_turn_text(child, out);
                }
            }
        }
        _ => {}
    }
}

fn collect_content(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) if !text.is_empty() => out.push(text.clone()),
        serde_json::Value::Array(values) => {
            for child in values {
                collect_content(child, out);
            }
        }
        serde_json::Value::Object(object) => {
            if object
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|ty| ty.contains("tool"))
            {
                return;
            }
            if let Some(text) = object.get("text").and_then(serde_json::Value::as_str) {
                out.push(text.to_string());
            } else {
                for child in object.values() {
                    collect_content(child, out);
                }
            }
        }
        _ => {}
    }
}

impl Index {
    /// Resolve the private cache directory for `destination_identity`.
    pub fn for_destination(cache_root: &Path, destination_identity: &str) -> Self {
        let digest = hex(&Sha256::digest(destination_identity.as_bytes()));
        let root = cache_root.join("chat-stasher").join("fts").join(digest);
        Self {
            db_path: root.join("index.sqlite3"),
            root,
            validated_mtime: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Construct an index rooted at a caller-owned path (also useful for tests).
    pub fn at(root: PathBuf) -> Self {
        Self {
            db_path: root.join("index.sqlite3"),
            root,
            validated_mtime: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// The explicit maintenance read: validate the schema thoroughly and report
    /// what a build left behind. Missing remains distinct from corrupt: callers
    /// can offer a build command only for the former. This is the one path that
    /// is *supposed* to run the full integrity scan, so it does not use the
    /// mtime cache.
    pub fn check(&self) -> Result<CheckReport> {
        let connection = self.open_read_only()?;
        validate_schema(&connection)?;
        let documents: i64 = connection
            .query_row("SELECT count(*) FROM documents", [], |row| {
                row.get::<_, i64>(0)
            })
            .context("count indexed documents")?;
        let completed: Option<String> = connection
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'build_status'",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("read FTS index build status")?;
        let status = if completed.as_deref() != Some("completed") {
            CheckStatus::Incomplete
        } else {
            let indexed = read_meta_count(&connection, "build_indexed")?;
            let not_indexable = read_meta_count(&connection, "build_not_indexable")?;
            let empty_body = read_meta_count(&connection, "build_empty_body")?;
            let bytes_read = read_meta_count(&connection, "build_bytes_read")? as u64;
            if documents > 0 && not_indexable == 0 {
                CheckStatus::Valid {
                    indexed,
                    empty_body,
                    bytes_read,
                }
            } else {
                CheckStatus::Partial {
                    indexed,
                    not_indexable,
                    empty_body,
                    bytes_read,
                }
            }
        };
        Ok(CheckReport {
            documents: documents as usize,
            status,
        })
    }

    /// Open the database read-only without any validation, so the caller can
    /// decide which kind of check the operation needs.
    fn open_read_only(&self) -> Result<Connection> {
        if !self.db_path.exists() {
            bail!("no local index has been built; run `chat-stasher index build` (no archive read was performed)");
        }
        self.validate_owned_paths()?;
        Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).context(
            "open FTS index; it may be corrupt, use `chat-stasher index clear` then rebuild",
        )
    }

    /// The read-only connection every reader of this index starts from: a
    /// missing file and a corrupt one stay two different instructions, and
    /// both are refused before any question is asked of the index. Validation
    /// is the cheap guard — schema version, a completed build, and one full
    /// integrity scan per file mtime (C10) — not the per-query full scan of
    /// old behavior.
    fn open_valid(&self) -> Result<Connection> {
        let connection = self.open_read_only()?;
        self.cheap_validate(&connection)?;
        Ok(connection)
    }

    /// The cheap guard every read path runs: the schema is the expected
    /// version, a build has completed, and — once per index **file state** —
    /// the database passed a full integrity scan. The full scan is cached by
    /// that state, so a query pays for it once per rebuild rather than once per
    /// query.
    fn cheap_validate(&self, connection: &Connection) -> Result<()> {
        let version: String = connection
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .map_err(|error| {
                anyhow!("invalid or corrupt FTS index; use `chat-stasher index clear` then rebuild: {error}")
            })?;
        if version.parse::<i64>().ok() != Some(SCHEMA_VERSION) {
            bail!(
                "unsupported FTS index schema version; use `chat-stasher index clear` then rebuild"
            );
        }
        let completed: Option<String> = connection
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'build_status'",
                [],
                |row| row.get(0),
            )
            .map_err(|error| {
                anyhow!("invalid or corrupt FTS index; use `chat-stasher index clear` then rebuild: {error}")
            })?;
        if completed.as_deref() != Some("completed") {
            bail!("the local FTS index has no recorded completed build; run `chat-stasher index build` to record one (sessions that are unchanged are not re-read)");
        }
        let identity = db_identity(&self.db_path);
        let mut validated = self
            .validated_mtime
            .lock()
            .expect("FTS validation cache lock poisoned");
        if validation_is_current(&validated, &identity) {
            return Ok(());
        }
        validate_schema(connection)?;
        *validated = identity;
        Ok(())
    }

    /// What the index holds, and when its file was last written.
    ///
    /// Separate from [`Index::check`] because a caller that only wants the
    /// document count should not pay for reading every id, and because the two
    /// answer different questions: `check` is "is this index usable", this is
    /// "which sessions of the archive are in it".
    ///
    /// The count is `ids.len()` rather than a second `count(*)` read: two reads
    /// of one table can straddle a rebuild, and a summary whose own numbers
    /// disagree is worse than one that reports the smaller fact.
    pub fn summary(&self) -> Result<IndexSummary> {
        let connection = self.open_valid()?;
        let mut statement = connection.prepare("SELECT id FROM documents")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<std::collections::BTreeSet<String>>>()?;
        let not_indexable = read_meta_ids(&connection, "build_not_indexable_ids")?;
        let written_unix = fs::metadata(&self.db_path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_secs() as i64);
        Ok(IndexSummary {
            ids,
            not_indexable,
            written_unix,
        })
    }

    /// Every document matching `query`, best rank first.
    ///
    /// The query is matched as a **literal substring**, not as FTS5 syntax: a
    /// reader who types `trust (rule)` is looking for that text, and handing
    /// the string to FTS5 unquoted would instead make it a syntax error or a
    /// different query. Quoting it also makes the tokenizer and
    /// [`Index::placements`]' literal search mean the same thing, so a match
    /// the one finds is a match the other can place.
    ///
    /// A query shorter than [`MIN_QUERY_CHARS`] is refused as
    /// [`QueryTooShort`] rather than answered with no rows.
    pub fn matches(&self, query: &str) -> Result<Result<MatchSet, QueryTooShort>> {
        self.matches_with_cap(query, MAX_QUERY_MATCHES)
    }

    /// [`Index::matches`] with the cap as a parameter, so the truncation path
    /// can be exercised without a [`MAX_QUERY_MATCHES`]-document fixture. The
    /// cap is a bound on this process's memory, not a fact about the archive.
    pub fn matches_with_cap(
        &self,
        query: &str,
        cap: usize,
    ) -> Result<Result<MatchSet, QueryTooShort>> {
        let chars = query.chars().count();
        if chars < MIN_QUERY_CHARS {
            return Ok(Err(QueryTooShort {
                chars,
                minimum: MIN_QUERY_CHARS,
            }));
        }
        let connection = self.open_valid()?;
        // One row past the cap, so "the index held more" is an observation
        // rather than an inference from a full page.
        let mut statement = connection.prepare(
            "SELECT id, title, snippet(documents_fts, 2, ?2, ?3, '…', ?4), bm25(documents_fts) \
             FROM documents_fts WHERE documents_fts MATCH ?1 ORDER BY bm25(documents_fts) LIMIT ?5",
        )?;
        // A trigram snippet of N tokens spans about N characters: consecutive
        // trigrams overlap by two, so `tokens` ≈ the excerpt's length. 240 is
        // the design's per-hit byte ceiling (§4.6) expressed in the unit the
        // tokenizer actually counts in.
        const SNIPPET_TOKENS: i64 = 240;
        let rows = statement
            .query_map(
                params![
                    literal_match_query(query),
                    MARK_OPEN,
                    MARK_CLOSE,
                    SNIPPET_TOKENS,
                    (cap as i64) + 1
                ],
                |row| {
                    let excerpt: String = row.get(2)?;
                    Ok(RankedMatch {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        // The marker is the observation: `snippet()` marks the
                        // tokens it selected, so an unmarked excerpt means the
                        // body held no match (see the field's own doc comment).
                        snippet: excerpt.contains(MARK_OPEN).then_some(excerpt),
                        rank: row.get(3)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let truncated = rows.len() > cap;
        let mut matches = rows;
        matches.truncate(cap);
        Ok(Ok(MatchSet { matches, truncated }))
    }

    /// Where `query` first sits inside each named document.
    ///
    /// The answer comes from the same stored text the match was taken from, one
    /// `instr` per document, so the whole body is never read into this process
    /// and a wrong message number cannot come from a second text extraction that
    /// disagreed with the first. `instr` positions are character offsets, the
    /// same unit `message_offsets` is recorded in.
    ///
    /// Case folding here is SQLite's `lower()`, which folds ASCII only. A
    /// non-ASCII query can therefore match in the index (trigram folds more)
    /// and still answer [`MatchPlace::NotRelocated`] — which is why that state
    /// exists rather than being guessed into message 0.
    pub fn placements(&self, query: &str, ids: &[String]) -> Result<Vec<MatchPlace>> {
        let connection = self.open_valid()?;
        let mut statement = connection.prepare(
            "SELECT instr(lower(body), lower(?2)), instr(lower(title), lower(?2)), message_offsets \
             FROM documents WHERE id = ?1",
        )?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            let row = statement
                .query_row(params![id, query], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .optional()?;
            let Some((body_at, title_at, offsets)) = row else {
                // The index matched a document it can no longer read back:
                // that is "the match could not be placed", never message 0.
                out.push(MatchPlace::NotRelocated);
                continue;
            };
            if body_at > 0 {
                let offsets: Vec<usize> = serde_json::from_str(&offsets)
                    .context("stored message offsets are not a JSON array of numbers")?;
                // 1-based from `instr`, offsets are 0-based character offsets.
                let at = (body_at - 1) as usize;
                let ordinal = offsets.iter().rposition(|start| *start <= at);
                out.push(match ordinal {
                    Some(ordinal) => MatchPlace::Message { ordinal },
                    None => MatchPlace::NotRelocated,
                });
            } else if title_at > 0 {
                out.push(MatchPlace::Label);
            } else {
                out.push(MatchPlace::NotRelocated);
            }
        }
        Ok(out)
    }

    /// Build/update documents. `load` is called only for new or changed source
    /// fingerprints, and once per source.
    ///
    /// A failing `load` (a malformed or unreadable shard) does **not** abort the
    /// build: the session is recorded in `BuildStats::not_indexable` with its
    /// reason and the rest of the archive is still indexed (C1). The last build
    /// outcome is recorded in `index_meta` so `index check` can tell a completed
    /// build apart from one that never finished (C7). A commit failure still
    /// rolls the transaction back, leaving the previous completed build intact.
    pub fn build<F>(&self, sources: &[SourceDoc], mut load: F) -> Result<BuildStats>
    where
        F: FnMut(&str) -> std::result::Result<LoadedDoc, LoadFailure>,
    {
        let existed = self.db_path.exists();
        self.validate_owned_paths_if_present()?;
        let marker_path = self.root.join(MARKER);
        if (existed || marker_path.exists()) && !self.marker_is_ours()? {
            bail!(
                "refusing to use unowned FTS directory `{}`",
                self.root.display()
            );
        }
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create FTS directory `{}`", self.root.display()))?;
        if !marker_path.exists() {
            if self
                .root
                .read_dir()
                .context("inspect unmarked FTS directory")?
                .next()
                .is_some()
            {
                bail!("refusing to adopt a non-empty unmarked FTS directory");
            }
            let mut marker = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.root.join(MARKER))
                .context("create FTS ownership marker")?;
            use std::io::Write;
            marker
                .write_all(MARKER_CONTENT)
                .context("write FTS ownership marker")?;
            marker.sync_all().context("sync FTS ownership marker")?;
            set_file_private(&self.root.join(MARKER))?;
        }
        set_dir_private(&self.root)?;
        set_file_private(&marker_path)?;
        let connection = Connection::open(&self.db_path)
            .context("open FTS index; corrupt databases are not replaced automatically")?;
        set_file_private(&self.db_path)?;
        connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        if existed {
            validate_schema(&connection)?;
        } else {
            create_schema(&connection)?;
        }

        let tx = connection.unchecked_transaction()?;
        let mut old = std::collections::BTreeMap::new();
        {
            let mut statement = tx.prepare("SELECT id, source_sha256 FROM documents")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, source_sha) = row?;
                old.insert(id, source_sha);
            }
        }
        let mut stats = BuildStats::default();
        let mut present = std::collections::BTreeSet::new();
        for source in sources {
            let id = source.id();
            if !present.insert(id) {
                bail!("duplicate source document id in index input");
            }
            // A source the archive could not describe at all is named and
            // skipped, before any fingerprint comparison: there is no
            // fingerprint to compare, and an unreadable source is not an
            // unchanged one — it must be re-reported on every build until the
            // archive can describe it again.
            let source_sha256 = match source {
                SourceDoc::Fingerprinted { source_sha256, .. } => source_sha256,
                SourceDoc::Unreadable { id, reason } => {
                    stats.read += 1;
                    stats.not_indexable.push((id.clone(), reason.clone()));
                    continue;
                }
            };
            if old.get(id) == Some(source_sha256) {
                stats.unchanged += 1;
                continue;
            }
            stats.read += 1;
            // A failing load names the session and moves on; one malformed shard
            // must not put the whole archive out of reach (C1).
            //
            // Any row the previous build stored for this id is left alone rather
            // than deleted: the unreadable read proves nothing about what the
            // session now holds, and an abort of the whole build — the behavior
            // this replaces — likewise kept the previous build's content for it.
            // Deleting would let one transient read failure (a network hiccup on
            // an object-store destination) shrink the index, so the failed
            // session is named in `not_indexable` instead and `check` reports the
            // build `Partial`. A session the build has *never* read has no row,
            // so it is absent from the index and `search` counts it as missing.
            //
            // The bytes it read before failing are counted all the same: they
            // were read, and a volume that dropped them would understate what
            // this build cost (C6).
            let loaded = match load(id) {
                Ok(loaded) => loaded,
                Err(failure) => {
                    stats.bytes_read += failure.bytes_read;
                    stats
                        .not_indexable
                        .push((id.to_owned(), format!("{:#}", failure.error)));
                    continue;
                }
            };
            stats.bytes_read += loaded.bytes_read;
            let text = loaded.text;
            if text.body.is_empty() {
                stats.empty_body += 1;
            }
            let content_sha = digest_text(&text.title, &text.body);
            let offsets: Vec<u64> = text
                .message_offsets
                .iter()
                .map(|offset| *offset as u64)
                .collect();
            tx.execute("DELETE FROM documents_fts WHERE id = ?1", [id])?;
            tx.execute("DELETE FROM documents WHERE id = ?1", [id])?;
            tx.execute(
                "INSERT INTO documents(id, source_sha256, content_sha256, title, body, message_offsets) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    source_sha256,
                    content_sha,
                    text.title,
                    text.body,
                    serde_json::to_string(&offsets).context("serialise message offsets")?
                ],
            )?;
            tx.execute(
                "INSERT INTO documents_fts(id, title, body) VALUES (?1, ?2, ?3)",
                params![id, text.title, text.body],
            )?;
            stats.indexed += 1;
        }
        let stale: Vec<String> = old
            .keys()
            .filter(|id| !present.contains(id.as_str()))
            .cloned()
            .collect();
        for id in &stale {
            tx.execute("DELETE FROM documents_fts WHERE id = ?1", [id])?;
            tx.execute("DELETE FROM documents WHERE id = ?1", [id])?;
        }
        stats.removed = stale.len();
        stats.documents = sources.len();
        record_build_outcome(&tx, &stats)?;
        tx.commit()?;
        set_file_private(&self.db_path)?;
        Ok(stats)
    }

    /// Clear only an FTS cache directory carrying this module's ownership mark.
    pub fn clear(&self) -> Result<bool> {
        if !self.root.exists() {
            return Ok(false);
        }
        self.validate_owned_paths()?;
        if !self.marker_is_ours()? {
            bail!(
                "refusing to clear unowned FTS directory `{}`",
                self.root.display()
            );
        }
        fs::remove_dir_all(&self.root)
            .with_context(|| format!("clear FTS directory `{}`", self.root.display()))?;
        Ok(true)
    }

    fn marker_is_ours(&self) -> Result<bool> {
        match fs::read(self.root.join(MARKER)) {
            Ok(bytes) => Ok(bytes.starts_with(b"chat-stasher fts index")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).context("read FTS ownership marker"),
        }
    }

    fn validate_owned_paths_if_present(&self) -> Result<()> {
        if self.root.exists() {
            let metadata = fs::symlink_metadata(&self.root).context("inspect FTS directory")?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!("refusing non-directory or linked FTS root");
            }
        }
        if self.db_path.exists() {
            let metadata = fs::symlink_metadata(&self.db_path).context("inspect FTS database")?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("refusing non-file or linked FTS database");
            }
        }
        let marker = self.root.join(MARKER);
        if marker.exists() {
            let metadata = fs::symlink_metadata(&marker).context("inspect FTS marker")?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("refusing non-file or linked FTS ownership marker");
            }
        }
        Ok(())
    }

    fn validate_owned_paths(&self) -> Result<()> {
        self.validate_owned_paths_if_present()?;
        if !self.marker_is_ours()? {
            bail!("refusing to use or clear unowned FTS directory");
        }
        Ok(())
    }
}

/// SHA-256 of exactly the text that will be indexed.
pub fn digest_text(title: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    hasher.update([0]);
    hasher.update(body.as_bytes());
    hex(&hasher.finalize())
}

/// The reader's query as one FTS5 phrase, so the string they typed is the
/// string that is searched for. A `"` inside it is doubled, which is how FTS5
/// quotes a quote; everything else is literal, so no other character can turn
/// a reader's text into query syntax.
fn literal_match_query(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

fn create_schema(connection: &Connection) -> Result<()> {
    let tx = connection.unchecked_transaction()?;
    tx.execute_batch(
        "CREATE TABLE index_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE documents (
             id TEXT PRIMARY KEY,
             source_sha256 TEXT NOT NULL,
             content_sha256 TEXT NOT NULL,
             title TEXT NOT NULL,
             body TEXT NOT NULL,
             message_offsets TEXT NOT NULL
         );
         CREATE VIRTUAL TABLE documents_fts USING fts5(id UNINDEXED, title, body, tokenize='trigram');",
    )?;
    // The version is written from the constant the reader compares against, not
    // repeated in the DDL: two copies of it are two things that can disagree,
    // and the one nobody reads is the one that would rot.
    tx.execute(
        "INSERT INTO index_meta(key, value) VALUES ('schema_version', ?1), ('build_status', 'incomplete')",
        [SCHEMA_VERSION],
    )?;
    tx.commit()?;
    Ok(())
}

fn validate_schema(connection: &Connection) -> Result<()> {
    let version = connection
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| anyhow!("invalid or corrupt FTS index; use `chat-stasher index clear` then rebuild: {error}"))?;
    if version.parse::<i64>().ok() != Some(SCHEMA_VERSION) {
        bail!("unsupported FTS index schema version; use `chat-stasher index clear` then rebuild");
    }
    let integrity = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        .map_err(|error| {
            anyhow!("corrupt FTS index; use `chat-stasher index clear` then rebuild: {error}")
        })?;
    if integrity != "ok" {
        bail!("corrupt FTS index; use `chat-stasher index clear` then rebuild");
    }
    connection
        .query_row("SELECT count(*) FROM documents_fts", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| {
            anyhow!("corrupt FTS index; use `chat-stasher index clear` then rebuild: {error}")
        })?;
    let inconsistent = connection
        .query_row(
            "SELECT count(*) FROM documents d \
             LEFT JOIN documents_fts f ON f.id = d.id \
             WHERE f.id IS NULL OR f.title != d.title OR f.body != d.body",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| {
            anyhow!("corrupt FTS index; use `chat-stasher index clear` then rebuild: {error}")
        })?;
    let extra = connection
        .query_row(
            "SELECT count(*) FROM documents_fts f \
             LEFT JOIN documents d ON d.id = f.id WHERE d.id IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| {
            anyhow!("corrupt FTS index; use `chat-stasher index clear` then rebuild: {error}")
        })?;
    if inconsistent != 0 || extra != 0 {
        bail!("corrupt FTS index; use `chat-stasher index clear` then rebuild");
    }
    Ok(())
}

/// Record the just-finished build in `index_meta`, in the same transaction that
/// wrote the documents, so a committed `build_status = 'completed'` is atomic
/// with the rows it describes. `index check` reads these to distinguish a
/// completed build from an interrupted one, and to report what it left behind.
fn record_build_outcome(connection: &Connection, stats: &BuildStats) -> Result<()> {
    let put = |key: &str, value: &str| -> Result<()> {
        connection.execute(
            "INSERT INTO index_meta(key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    };
    put("build_status", "completed")?;
    put("build_indexed", &stats.indexed.to_string())?;
    put(
        "build_not_indexable",
        &stats.not_indexable.len().to_string(),
    )?;
    // Both the count and the ids: the count is the build's own report of how
    // many sessions it could not read, and the ids are what a reader needs to
    // tell whether *this* session is one of them. A count alone cannot answer
    // that, and answering it with "not one of them" would turn a hole into a
    // proven absence (C1/C7).
    put(
        "build_not_indexable_ids",
        &serde_json::to_string(
            &stats
                .not_indexable
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<Vec<String>>(),
        )
        .context("serialise the not-indexable source ids")?,
    )?;
    put("build_empty_body", &stats.empty_body.to_string())?;
    put("build_bytes_read", &stats.bytes_read.to_string())?;
    Ok(())
}

/// Read the list of source ids a completed build named not indexable, in the
/// order it named them.
///
/// The key is written by every completed build, so its absence is not "no
/// session failed" — it is an index from a version that did not record them,
/// and that index is refused by its schema version before it gets here.
fn read_meta_ids(connection: &Connection, key: &str) -> Result<std::collections::BTreeSet<String>> {
    let value: String = connection
        .query_row(
            "SELECT value FROM index_meta WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .map_err(|error| anyhow!("corrupt FTS index build metadata: {error}"))?;
    serde_json::from_str(&value).map_err(|error| {
        anyhow!("corrupt FTS index build metadata: `{key}` is not a JSON array of ids: {error}")
    })
}

/// Read a numeric build-health counter recorded by a completed build.
fn read_meta_count(connection: &Connection, key: &str) -> Result<usize> {
    let value: String = connection
        .query_row(
            "SELECT value FROM index_meta WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .map_err(|error| anyhow!("corrupt FTS index build metadata: {error}"))?;
    value
        .parse::<usize>()
        .map_err(|_| anyhow!("corrupt FTS index build metadata: non-numeric `{key}`"))
}

/// How many bytes of a SQLite file carry its header.
const SQLITE_HEADER_LEN: usize = 100;

/// Where the header's 4-byte big-endian *file change counter* sits: incremented
/// by SQLite on every write transaction that commits to the file, and thereby
/// the database's own record of having changed.
const SQLITE_CHANGE_COUNTER_OFFSET: usize = 24;

/// The cheap identity of the index file, and the key the full-integrity scan is
/// cached on.
///
/// The mtime alone was not enough. A database can change without the mtime
/// moving — a rebuild inside one mtime tick, a copy that preserves times — and
/// the cache then vouched for a scan that ran *before* the change, so a read
/// answered from an index nothing had checked. The length catches a rewrite
/// that happens to keep both of the others, and the change counter is the one
/// component the file's own writer maintains: it moves for a committed write
/// transaction whether or not a clock or a timestamp moved with it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DbIdentity {
    modified: Option<std::time::SystemTime>,
    len: u64,
    change_counter: u32,
}

/// The identity of the file at `path`, or `None` when there is nothing to key
/// on: the file cannot be read, or it is too short to carry a header. `None` is
/// an absent measurement, never a stand-in for "unchanged".
fn db_identity(path: &Path) -> Option<DbIdentity> {
    use std::io::Read;
    let metadata = fs::metadata(path).ok()?;
    let mut header = [0u8; SQLITE_HEADER_LEN];
    fs::File::open(path).ok()?.read_exact(&mut header).ok()?;
    Some(DbIdentity {
        modified: metadata.modified().ok(),
        len: metadata.len(),
        change_counter: u32::from_be_bytes([
            header[SQLITE_CHANGE_COUNTER_OFFSET],
            header[SQLITE_CHANGE_COUNTER_OFFSET + 1],
            header[SQLITE_CHANGE_COUNTER_OFFSET + 2],
            header[SQLITE_CHANGE_COUNTER_OFFSET + 3],
        ]),
    })
}

/// True when a full validation has already run for exactly this file state.
///
/// An absent identity never counts, in either position: "the file could not be
/// read" is not "it has not changed", and a cache that held an absence must not
/// be made to equal a later absence — that comparison is how a stat failure
/// skipped the scan the check exists to run.
fn validation_is_current(cached: &Option<DbIdentity>, current: &Option<DbIdentity>) -> bool {
    current.is_some() && cached == current
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(unix)]
fn set_file_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set private permissions on `{}`", path.display()))
}

#[cfg(not(unix))]
fn set_file_private(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_dir_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("set private permissions on `{}`", path.display()))
}

#[cfg(not(unix))]
fn set_dir_private(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A document whose body was never split into messages. The tests that use
    /// it are about matching and storage, not about message placement, and an
    /// empty offset list is exactly what a one-blob document has.
    fn doc(title: &str, body: &str) -> DocText {
        DocText {
            title: title.into(),
            body: body.into(),
            message_offsets: Vec::new(),
        }
    }

    /// A document of `count` messages, numbered, one per line — the shape the
    /// archive reader produces.
    fn numbered(body: &[&str]) -> DocText {
        let mut text = doc("synthetic title", "");
        for message in body {
            if !text.body.is_empty() {
                text.body.push('\n');
            }
            text.message_offsets.push(text.body.chars().count());
            text.body.push_str(message);
        }
        text
    }

    /// Wrap a `DocText` for a build closure, with a synthetic byte count.
    fn the(text: DocText, bytes_read: u64) -> LoadedDoc {
        LoadedDoc { text, bytes_read }
    }

    #[test]
    fn incremental_build_reads_only_changed_sources() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let sources = vec![
            SourceDoc::fingerprinted("a", "sha-a1"),
            SourceDoc::fingerprinted("b", "sha-b1"),
        ];
        let calls = Cell::new(0);
        index
            .build(&sources, |id| {
                calls.set(calls.get() + 1);
                Ok(the(doc(id, "synthetic"), 0))
            })
            .unwrap();
        assert_eq!(calls.get(), 2);
        let changed = vec![
            SourceDoc::fingerprinted("a", "sha-a1"),
            SourceDoc::fingerprinted("b", "sha-b2"),
        ];
        let calls = Cell::new(0);
        let stats = index
            .build(&changed, |id| {
                calls.set(calls.get() + 1);
                Ok(the(doc(id, "synthetic changed"), 0))
            })
            .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(stats.read, 1);
        assert_eq!(stats.unchanged, 1);
    }

    #[test]
    fn destinations_have_isolated_roots() {
        let dir = tempfile::tempdir().unwrap();
        let one = Index::for_destination(dir.path(), "synthetic destination one");
        let two = Index::for_destination(dir.path(), "synthetic destination two");
        let source = [SourceDoc::fingerprinted("same-id", "same-sha")];
        one.build(&source, |_| {
            Ok(the(doc("one", "synthetic hedgehog material"), 0))
        })
        .unwrap();
        two.build(&source, |_| {
            Ok(the(doc("two", "synthetic blueberry material"), 0))
        })
        .unwrap();
        assert_ne!(one.root(), two.root());
        assert_eq!(one.check().unwrap().documents, 1);
        assert_eq!(two.check().unwrap().documents, 1);
        assert_eq!(one.matches("hedgehog").unwrap().unwrap().len(), 1);
        assert_eq!(one.matches("blueberry").unwrap().unwrap().len(), 0);
        assert_eq!(two.matches("blueberry").unwrap().unwrap().len(), 1);
        assert_eq!(two.matches("hedgehog").unwrap().unwrap().len(), 0);
    }

    #[test]
    fn corrupt_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        index
            .build(&[], |_| Ok(the(DocText::default(), 0)))
            .unwrap();
        fs::write(index.db_path(), b"not sqlite").unwrap();
        let error = index.check().unwrap_err().to_string();
        assert!(error.contains("corrupt") || error.contains("invalid"));
    }

    #[test]
    fn inconsistent_open_database_is_refused_by_check_and_build() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let sources = [SourceDoc::fingerprinted(
            "synthetic/session",
            "synthetic-source",
        )];
        index
            .build(&sources, |_| {
                Ok(the(doc("synthetic title", "synthetic body"), 0))
            })
            .unwrap();
        Connection::open(index.db_path())
            .unwrap()
            .execute("DELETE FROM documents_fts", [])
            .unwrap();

        assert!(index.check().unwrap_err().to_string().contains("corrupt"));
        let read = Cell::new(false);
        let error = index
            .build(&sources, |_| {
                read.set(true);
                Ok(the(DocText::default(), 0))
            })
            .unwrap_err();
        assert!(error.to_string().contains("corrupt"));
        assert!(!read.get());
    }

    #[cfg(unix)]
    #[test]
    fn database_permissions_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        index
            .build(&[], |_| Ok(the(DocText::default(), 0)))
            .unwrap();
        assert_eq!(
            fs::metadata(index.db_path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn trigram_search_matches_a_synthetic_substring() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        index
            .build(
                &[SourceDoc::fingerprinted(
                    "synthetic/session",
                    "synthetic-source",
                )],
                |_| {
                    Ok(the(
                        doc("synthetic title", "The quiet hedgehog walks at night."),
                        0,
                    ))
                },
            )
            .unwrap();
        let set = index.matches("hedgehog").unwrap().unwrap();
        assert_eq!(set.len(), 1);
        assert_eq!(set.matches[0].id, "synthetic/session");
    }

    #[test]
    fn trigram_search_matches_title_body_and_cjk_known_answers() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        index
            .build(
                &[
                    SourceDoc::fingerprinted("synthetic/title-hit", "synthetic-title-source"),
                    SourceDoc::fingerprinted("synthetic/body-hit", "synthetic-body-source"),
                    SourceDoc::fingerprinted("synthetic/cjk-hit", "synthetic-cjk-source"),
                ],
                |id| {
                    Ok(the(
                        match id {
                            "synthetic/title-hit" => {
                                doc("synthetic apricot title", "ordinary synthetic body")
                            }
                            "synthetic/body-hit" => doc("ordinary title", "synthetic indigo body"),
                            // Written as escapes, not as the characters themselves:
                            // T5 refuses literal CJK anywhere under `crates/`, and a
                            // test of CJK matching is not an exception to that.
                            _ => doc(
                                "synthetic title",
                                "\u{5408}\u{6210}\u{4e2d}\u{6587}\u{68c0}\u{7d22}\u{6837}\u{672c}",
                            ),
                        },
                        0,
                    ))
                },
            )
            .unwrap();

        // Each query was written into exactly one document, and both searchable
        // columns are exercised: the known answer is the id, so a column that
        // quietly stopped being indexed fails as the wrong id, not as a short set.
        let ids = |query: &str| {
            index
                .matches(query)
                .unwrap()
                .unwrap()
                .matches
                .into_iter()
                .map(|found| found.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids("apricot"), vec!["synthetic/title-hit"]);
        assert_eq!(ids("indigo"), vec!["synthetic/body-hit"]);
        assert_eq!(
            ids("\u{4e2d}\u{6587}\u{68c0}\u{7d22}"),
            vec!["synthetic/cjk-hit"],
            "a four-character CJK query is above the trigram floor and must match"
        );
        // Below the floor the trigram tokenizer has no token to look for, so the
        // same characters are refused rather than answered with an empty set:
        // reporting "cannot evaluate" as "nothing matched" is the one answer
        // this API exists to make unavailable.
        assert_eq!(
            index.matches("\u{4e2d}\u{6587}").unwrap(),
            Err(QueryTooShort {
                chars: 2,
                minimum: 3,
            })
        );
        assert!(index.matches("\u{4e2d}").unwrap().is_err());
    }

    #[test]
    fn extraction_keeps_user_and_assistant_text_only() {
        let raw = br#"{"title":"synthetic title","message":{"author":{"role":"user"},"content":{"parts":["synthetic question"]}}}
{"role":"assistant","content":"synthetic answer"}
{"role":"tool","content":"synthetic tool output"}"#;
        let (title, body) = extract_index_text(raw).unwrap();
        assert_eq!(title, "synthetic title");
        assert!(body.contains("synthetic question"));
        assert!(body.contains("synthetic answer"));
        assert!(!body.contains("synthetic tool output"));
    }

    #[test]
    fn extraction_refuses_malformed_jsonl_instead_of_indexing_partial_text() {
        assert!(extract_index_text(b"{bad json}\n").is_err());
    }

    /// Build one-session indexes for the placement tests below.
    fn index_of(text: DocText) -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        index
            .build(
                &[SourceDoc::fingerprinted(
                    "synthetic/session",
                    "synthetic-source",
                )],
                |_| Ok(the(text.clone(), 0)),
            )
            .unwrap();
        (dir, index)
    }

    #[test]
    fn a_match_is_placed_in_the_message_it_sits_in() {
        let (_dir, index) = index_of(numbered(&[
            "the first synthetic message",
            "the second synthetic message mentions hedgehogs",
            "the third synthetic message",
        ]));
        let set = index.matches("hedgehog").unwrap().unwrap();
        assert_eq!(set.len(), 1);
        let places = index
            .placements("hedgehog", &[set.matches[0].id.clone()])
            .unwrap();
        assert_eq!(places, vec![MatchPlace::Message { ordinal: 1 }]);
        // The same document, matched in its own first and last messages: the
        // ordinal is read from the match, not from the document.
        let places = index
            .placements("third synthetic", &["synthetic/session".to_string()])
            .unwrap();
        assert_eq!(places, vec![MatchPlace::Message { ordinal: 2 }]);
    }

    #[test]
    fn a_short_query_is_refused_rather_than_answered_with_no_rows() {
        let (_dir, index) = index_of(numbered(&["the quiet hedgehog"]));
        // Two characters: the trigram tokenizer has no token to look for, so
        // the only honest answers are "cannot evaluate" or "unknown" — never
        // the empty match list a caller would print as "nothing matched".
        assert_eq!(
            index.matches("he").unwrap(),
            Err(QueryTooShort {
                chars: 2,
                minimum: 3,
            })
        );
        // One character that *is* present in the text takes the same path.
        assert!(index.matches("h").unwrap().is_err());
        assert_eq!(index.matches("hedgehog").unwrap().unwrap().len(), 1);
    }

    #[test]
    fn a_label_only_match_is_not_anchored_to_a_message_and_has_no_body_excerpt() {
        let (_dir, index) = index_of(numbered(&["a body without the word"]));
        // The word is in the title column, so the hit is real and there is no
        // message to jump to: `Label`, not message 0.
        let set = index.matches("synthetic title").unwrap().unwrap();
        assert_eq!(set.len(), 1);
        assert_eq!(
            set.matches[0].snippet, None,
            "snippet() hands back the body's opening tokens unmarked when the match \
             was in another column; that must not reach a reader as an excerpt"
        );
        let places = index
            .placements("synthetic title", &[set.matches[0].id.clone()])
            .unwrap();
        assert_eq!(places, vec![MatchPlace::Label]);
    }

    #[test]
    fn a_body_match_has_an_excerpt_and_the_two_column_readings_agree() {
        let (_dir, index) = index_of(numbered(&["a body with the hedgehog in it"]));
        let set = index.matches("hedgehog").unwrap().unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.matches[0].snippet.is_some());
        // Two independent readings of the same question — the tokenizer's
        // marker and the literal relocation — say the same thing here, which
        // is what lets a page show an excerpt and an anchor together.
        let places = index
            .placements("hedgehog", &[set.matches[0].id.clone()])
            .unwrap();
        assert_eq!(places, vec![MatchPlace::Message { ordinal: 0 }]);
    }

    #[test]
    fn a_document_without_message_boundaries_is_not_placed() {
        let (_dir, index) = index_of(doc("synthetic title", "the quiet hedgehog"));
        let set = index.matches("hedgehog").unwrap().unwrap();
        assert_eq!(set.len(), 1);
        // The body holds the match, but no boundary was recorded, so no
        // message number can be claimed: `NotRelocated`, never message 0.
        let places = index
            .placements("hedgehog", &[set.matches[0].id.clone()])
            .unwrap();
        assert_eq!(places, vec![MatchPlace::NotRelocated]);
    }

    #[test]
    fn a_match_is_placed_regardless_of_ascii_case() {
        let (_dir, index) = index_of(numbered(&["the quiet hedgehog", "another message"]));
        let places = index
            .placements("HEDGEHOG", &["synthetic/session".to_string()])
            .unwrap();
        assert_eq!(places, vec![MatchPlace::Message { ordinal: 0 }]);
    }

    #[test]
    fn an_empty_cap_reports_the_truncation_instead_of_zero_matches() {
        let (_dir, index) = index_of(numbered(&["the quiet hedgehog"]));
        let set = index.matches_with_cap("hedgehog", 0).unwrap().unwrap();
        assert!(set.is_empty());
        assert!(
            set.truncated,
            "an empty set under a cap of zero is a floor, not a measurement"
        );
    }

    #[test]
    fn marked_segments_split_on_control_characters_and_resolve_imbalance() {
        let marked = |excerpt| {
            marked_segments(excerpt)
                .into_iter()
                .map(|segment| (segment.matched, segment.text))
                .collect::<Vec<_>>()
        };
        let one = format!("a {MARK_OPEN}b{MARK_CLOSE} c");
        assert_eq!(
            marked(&one),
            vec![
                (false, "a ".to_string()),
                (true, "b".to_string()),
                (false, " c".to_string())
            ]
        );
        // Brackets in the conversation are text, not markers.
        assert_eq!(
            marked("[not a marker]"),
            vec![(false, "[not a marker]".to_string())]
        );
        // An open mark with no close runs to the end: what a truncated
        // excerpt looks like.
        let open = format!("a {MARK_OPEN}b");
        assert_eq!(
            marked(&open),
            vec![(false, "a ".to_string()), (true, "b".to_string())]
        );
        // A close with no open is dropped rather than printed.
        assert_eq!(
            marked(&format!("a{MARK_CLOSE}b")),
            vec![(false, "ab".to_string())]
        );
    }

    #[test]
    fn summary_reports_the_ids_it_holds_and_a_file_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let sources = vec![
            SourceDoc::fingerprinted("machine-one/session-a", "sha-a"),
            SourceDoc::fingerprinted("machine-one/session-b", "sha-b"),
            SourceDoc::fingerprinted("machine-two/session-c", "sha-c"),
            // No separator at all: the whole id is its own machine, and an id
            // that is not `<machine>/<session>` must survive the summary
            // verbatim rather than be truncated into one that looks like it.
            SourceDoc::fingerprinted("separatorless", "sha-d"),
        ];
        index
            .build(&sources, |id| Ok(the(doc(id, "synthetic body"), 0)))
            .unwrap();
        let summary = index.summary().unwrap();
        assert_eq!(
            summary.ids,
            std::collections::BTreeSet::from([
                "machine-one/session-a".to_string(),
                "machine-one/session-b".to_string(),
                "machine-two/session-c".to_string(),
                "separatorless".to_string(),
            ]),
            "the summary is the set of ids the index holds, spelled exactly as stored"
        );
        assert!(
            summary.written_unix.is_some(),
            "a file the OS just wrote has an mtime"
        );
    }

    #[test]
    fn an_older_schema_is_refused_with_the_rebuild_instruction() {
        let (_dir, index) = index_of(doc("synthetic title", "the quiet hedgehog"));
        // What a v1 index looks like to this build: the version it writes is
        // the one that moves, so stamping the old number back is exactly the
        // state a reader upgrading has on disk.
        Connection::open(index.db_path())
            .unwrap()
            .execute(
                "UPDATE index_meta SET value = '1' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        let error = format!("{:#}", index.check().unwrap_err());
        assert!(error.contains("unsupported"), "{error}");
        assert!(error.contains("index clear"), "{error}");
    }

    #[test]
    fn a_snippet_marks_the_matched_span_with_characters_conversation_cannot_contain() {
        let (_dir, index) = index_of(numbered(&["before the hedgehog after"]));
        let set = index.matches("hedgehog").unwrap().unwrap();
        let snippet = set.matches[0]
            .snippet
            .as_deref()
            .expect("a body match has an excerpt");
        // Markers are C0 control characters, so brackets that appear in the
        // conversation text are not mistaken for the matched span.
        assert!(
            snippet.contains(&format!("{MARK_OPEN}hedgehog{MARK_CLOSE}")),
            "snippet did not mark the match: {snippet:?}"
        );
        assert!(
            snippet.chars().count() <= 260,
            "a 240-token trigram excerpt is about 240 characters, not {}",
            snippet.chars().count()
        );
    }

    /// The offsets are **characters**, not bytes — which is the unit SQLite's
    /// `instr()` counts in, and therefore the unit `placements` compares
    /// against. A multi-byte script makes the two readings differ and so makes
    /// the test able to fail: the tokenizer is trigram partly *because* it
    /// handles scripts whose characters are not one byte each, and the unit is
    /// the same question for every such script.
    #[test]
    fn offsets_are_character_offsets_over_the_joined_body() {
        let raw = "{\"role\":\"user\",\"content\":\"привет\"}\n{\"role\":\"assistant\",\"content\":\"second\"}\n"
            .as_bytes();
        let extracted = extract_index_document(raw).unwrap();
        assert_eq!(extracted.body, "привет\nsecond");
        // Six characters of `привет` plus the one-character join.
        assert_eq!(extracted.message_offsets, vec![0, 7]);
        // The byte offset of the same position is 13: two bytes per character,
        // plus the join. Pinning both readings is what shows the field is the
        // character one, which is the unit `instr()` reports.
        assert_eq!(extracted.body.find("second"), Some(13));
        // Byte 13 and character 7 are the same position, and the offset that
        // was recorded for the second message is 7 — the character one.
        assert_eq!(extracted.body[..13].chars().count(), 7);
        assert_eq!(extracted.message_offsets[1], 7);
    }

    /// One source reads with a malformed body and one reads fine. The build
    /// must complete, record the bad one as not indexable with its reason, and
    /// leave the good one searchable — the archive is not rendered unreadable
    /// by a single unparsable session (C1).
    #[test]
    fn a_malformed_shard_is_recorded_not_indexable_and_the_build_continues() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let sources = vec![
            SourceDoc::fingerprinted("machine-one/good", "sha-good"),
            SourceDoc::fingerprinted("machine-one/bad", "sha-bad"),
            SourceDoc::fingerprinted("machine-one/good-later", "sha-later"),
        ];
        let stats = index
            .build(&sources, |id| {
                if id == "machine-one/bad" {
                    return Err(LoadFailure::new(
                        0,
                        anyhow!("invalid archived JSONL record at line 1"),
                    ));
                }
                Ok(the(doc(id, "the quiet hedgehog walks"), 21))
            })
            .unwrap();
        // Failure of one session is captured, not fatal.
        assert_eq!(stats.documents, 3);
        assert_eq!(stats.read, 3);
        assert_eq!(stats.indexed, 2);
        assert_eq!(stats.bytes_read, 42);
        assert_eq!(stats.not_indexable.len(), 1);
        let (bad_id, reason) = &stats.not_indexable[0];
        assert_eq!(bad_id, "machine-one/bad");
        assert!(reason.contains("invalid archived JSONL record"), "{reason}");
        // Both good sessions are indexed and searchable; the bad one is absent.
        let set = index.matches("hedgehog").unwrap().unwrap();
        let mut ids: Vec<&str> = set.matches.iter().map(|m| m.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["machine-one/good", "machine-one/good-later"]);
        assert!(!index.summary().unwrap().ids.contains("machine-one/bad"));
        assert!(
            index.check().unwrap().status
                == CheckStatus::Partial {
                    indexed: 2,
                    not_indexable: 1,
                    empty_body: 0,
                    bytes_read: 42,
                }
        );
    }

    /// A source that read fine once and then reads as malformed keeps the text
    /// the earlier build stored, and the failed re-read is named rather than
    /// silently deleting it. An unreadable read is not evidence that the
    /// session is gone, so dropping the row would let one transient failure
    /// shrink the index (C1).
    #[test]
    fn a_failed_reload_keeps_the_last_readable_text_and_names_the_failure() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let first = [SourceDoc::fingerprinted("machine-one/session", "sha-v1")];
        index
            .build(&first, |_| Ok(the(doc("t", "the quiet hedgehog walks"), 7)))
            .unwrap();
        assert_eq!(index.matches("hedgehog").unwrap().unwrap().len(), 1);
        assert!(index.summary().unwrap().covers("machine-one/session"));
        // The shard changes and the re-read fails: the fingerprint differs, so
        // the source is attempted and the attempt fails.
        let second = [SourceDoc::fingerprinted("machine-one/session", "sha-v2")];
        let stats = index
            .build(&second, |_| {
                Err(LoadFailure::new(
                    0,
                    anyhow!("invalid archived JSONL record at line 1"),
                ))
            })
            .unwrap();
        assert_eq!(stats.read, 1);
        assert_eq!(stats.indexed, 0);
        assert_eq!(stats.not_indexable.len(), 1);
        // The previous text is still searchable, so a failed re-read did not
        // subtract from what the archive could already answer.
        assert_eq!(index.matches("hedgehog").unwrap().unwrap().len(), 1);
        let summary = index.summary().unwrap();
        assert!(summary.ids.contains("machine-one/session"));
        // But the index no longer vouches for what it holds for that session:
        // the text is from an earlier read, so the session is named as not
        // answerable and coverage must leave it out. `ids` holding it is not
        // enough — that is the state this distinction exists for.
        assert!(summary.not_indexable.contains("machine-one/session"));
        assert!(!summary.covers("machine-one/session"));
        // And the build says so: one session could not be re-read.
        assert_eq!(
            index.check().unwrap().status,
            CheckStatus::Partial {
                indexed: 0,
                not_indexable: 1,
                empty_body: 0,
                bytes_read: 0,
            }
        );
        // A later build that reads it again heals the coverage: the record is
        // of the *last* attempt, so it does not outlive the failure that made
        // it.
        let third = [SourceDoc::fingerprinted("machine-one/session", "sha-v3")];
        index
            .build(&third, |_| {
                Ok(the(doc("t", "the blueberry bush stands"), 7))
            })
            .unwrap();
        let healed = index.summary().unwrap();
        assert!(healed.not_indexable.is_empty());
        assert!(healed.covers("machine-one/session"));
    }

    /// A source the archive cannot describe at all — non-empty, with no content
    /// IDs — is named with its reason and the rest of the build carries on.
    /// This is the same per-session tolerance a malformed shard gets, for the
    /// case where there is not even a fingerprint to compare, and it is what
    /// stops one such shard failing the whole archive's index (C1).
    #[test]
    fn an_unreadable_source_is_named_and_the_build_continues() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let sources = [
            SourceDoc::fingerprinted("machine-one/good", "sha-good"),
            SourceDoc::unreadable(
                "machine-one/undescribed",
                "non-empty archived shard has no content IDs",
            ),
            SourceDoc::fingerprinted("machine-one/later", "sha-later"),
        ];
        let loads = Cell::new(0);
        let stats = index
            .build(&sources, |id| {
                loads.set(loads.get() + 1);
                Ok(the(doc(id, "the quiet hedgehog walks"), 5))
            })
            .unwrap();
        assert_eq!(stats.documents, 3);
        assert_eq!(stats.read, 3);
        assert_eq!(stats.indexed, 2);
        assert_eq!(stats.bytes_read, 10);
        assert_eq!(
            loads.get(),
            2,
            "a source with no fingerprint is never loaded: there is nothing to compare"
        );
        assert_eq!(
            stats.not_indexable,
            vec![(
                "machine-one/undescribed".to_string(),
                "non-empty archived shard has no content IDs".to_string(),
            )]
        );
        // The two readable sessions are searchable; the undescribed one is in
        // neither the index nor the covered set.
        let summary = index.summary().unwrap();
        assert!(!summary.ids.contains("machine-one/undescribed"));
        assert!(summary.covers("machine-one/good"));
        assert!(!summary.covers("machine-one/undescribed"));
        // A second build re-reports it instead of calling it unchanged: an
        // unreadable source is not an unchanged one, and `check` must keep
        // saying `partial` until the archive can describe it again.
        let again = index
            .build(&sources, |id| {
                Ok(the(doc(id, "the quiet hedgehog walks"), 5))
            })
            .unwrap();
        assert_eq!(again.unchanged, 2);
        assert_eq!(again.read, 1);
        assert_eq!(again.not_indexable.len(), 1);
        assert_eq!(again.bytes_read, 0, "nothing was read for it");
        // The counters are the *last* build's: it indexed nothing, read nothing
        // for the undescribed source, and still names it.
        assert!(
            index.check().unwrap().status
                == CheckStatus::Partial {
                    indexed: 0,
                    not_indexable: 1,
                    empty_body: 0,
                    bytes_read: 0,
                }
        );
    }

    /// A source whose body extracts to nothing is stored and counted, but the
    /// count is reported separately from `indexed` so coverage cannot call a
    /// document "indexed" when it cannot match a query (C2).
    #[test]
    fn an_empty_body_is_counted_and_surfaced_by_check() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let sources = vec![
            SourceDoc::fingerprinted("machine-one/empty", "sha-empty"),
            SourceDoc::fingerprinted("machine-one/full", "sha-full"),
        ];
        let stats = index
            .build(&sources, |id| {
                let body = if id == "machine-one/full" {
                    "the quiet hedgehog walks"
                } else {
                    ""
                };
                Ok(the(doc(id, body), 10))
            })
            .unwrap();
        assert_eq!(stats.indexed, 2);
        assert_eq!(stats.empty_body, 1);
        assert!(
            index.check().unwrap().status
                == CheckStatus::Valid {
                    indexed: 2,
                    empty_body: 1,
                    bytes_read: 20,
                }
        );
    }

    /// `index check` must never call an index that did not finish a build
    /// "valid": an interrupted build leaves `build_status` incomplete, and the
    /// next check says so rather than reading the empty schema as healthy (C7).
    #[test]
    fn check_reports_an_interrupted_build_as_incomplete_not_valid() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        // Simulate a build that created the schema but never committed a
        // completed status: the schema exists, zero documents, and the meta
        // marker is still the create-time default.
        index
            .build(&[], |_| Ok(the(DocText::default(), 0)))
            .unwrap();
        Connection::open(index.db_path())
            .unwrap()
            .execute(
                "INSERT INTO index_meta(key, value) VALUES ('build_status', 'incomplete') \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [],
            )
            .unwrap();
        let report = index.check().unwrap();
        assert_eq!(report.documents, 0);
        assert_eq!(report.status, CheckStatus::Incomplete);
        // The index is unusable for queries, with a message that names the
        // rebuild, not a lie about being valid.
        let err = index.matches("hedgehog").unwrap_err().to_string();
        assert!(err.contains("no recorded completed build"), "{err}");
        assert!(err.contains("index build"), "{err}");
    }

    /// Rebuilding the index changes the file's mtime, and only that change
    /// forces a re-validation: the cheap guard must not hold on to a stale
    /// validation across a rebuild any more than it must re-validate a quiet
    /// index on every query (SRCH-6). A rebuild that swaps the body is seen by
    /// the next read and the new text is searchable.
    #[test]
    fn a_rebuild_invalidates_the_validation_cache_and_new_text_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let sources = [SourceDoc::fingerprinted("synthetic/session", "source-v1")];
        index
            .build(&sources, |_| {
                Ok(the(doc("t", "the quiet hedgehog walks"), 0))
            })
            .unwrap();
        assert_eq!(index.matches("hedgehog").unwrap().unwrap().len(), 1);
        // A second build changes the source fingerprint and the body; the mtime
        // moves, so the cached validation is not trusted for the old content.
        let updated = [SourceDoc::fingerprinted("synthetic/session", "source-v2")];
        index
            .build(&updated, |_| {
                Ok(the(doc("t", "the blueberry bush stands"), 0))
            })
            .unwrap();
        // The old term is gone from the rebuilt index and the new one is found:
        // the cheap guard re-validated on the mtime change instead of trusting
        // the pre-rebuild stamp.
        let old_hits = index.matches("hedgehog").unwrap().unwrap();
        assert_eq!(old_hits.len(), 0);
        let new_hits = index.matches("blueberry").unwrap().unwrap();
        assert_eq!(new_hits.len(), 1);
    }

    /// A changed index file must be re-validated even when the change does not
    /// move its mtime.
    ///
    /// The validation cache was keyed on the file mtime alone, so a database
    /// that changed inside one mtime tick — or one written through anything
    /// that preserves times — kept the stamp of the scan that ran *before* the
    /// change, and the next read answered from an index nothing had checked.
    /// The tamper here is the one `validate_schema` exists to refuse: a
    /// document present in `documents` and absent from `documents_fts`, which
    /// the query below would otherwise answer from, with a wrong zero.
    #[test]
    fn a_change_the_mtime_does_not_show_is_still_validated() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let sources = [SourceDoc::fingerprinted("machine-one/session", "source-v1")];
        index
            .build(&sources, |_| {
                Ok(the(doc("t", "the quiet hedgehog walks"), 0))
            })
            .unwrap();
        // One query validates the index and caches the file state it saw.
        assert_eq!(index.matches("hedgehog").unwrap().unwrap().len(), 1);
        let before = fs::metadata(index.db_path()).unwrap().modified().unwrap();
        Connection::open(index.db_path())
            .unwrap()
            .execute(
                "DELETE FROM documents_fts WHERE id = 'machine-one/session'",
                [],
            )
            .unwrap();
        // Put the time back: the only thing left that can show the change is
        // the database's own header.
        fs::OpenOptions::new()
            .write(true)
            .open(index.db_path())
            .unwrap()
            .set_modified(before)
            .unwrap();
        assert_eq!(
            fs::metadata(index.db_path()).unwrap().modified().unwrap(),
            before,
            "the test's premise: the change left the mtime where it was"
        );
        let error = index.matches("hedgehog").unwrap_err().to_string();
        assert!(error.contains("corrupt FTS index"), "{error}");
    }

    /// An identity that cannot be read is not an unchanged index.
    ///
    /// The cache compared `Option<SystemTime>` with `==`, so a file that could
    /// not be statted (`None`) equalled a cache that had never held anything
    /// (`None`) and the full scan was skipped — "we could not look" read as "it
    /// has not changed". The identity is only a key when it exists.
    #[test]
    fn an_index_file_that_cannot_be_read_is_never_a_cached_clean_validation() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent.sqlite3");
        assert!(db_identity(&absent).is_none());
        let short = dir.path().join("short.sqlite3");
        fs::write(&short, [0u8; 4]).unwrap();
        assert!(
            db_identity(&short).is_none(),
            "a file too short to carry a header has no identity"
        );
        let present = dir.path().join("present.sqlite3");
        fs::write(&present, [0u8; 512]).unwrap();
        let identity = db_identity(&present).expect("a readable 512-byte file has an identity");
        assert!(validation_is_current(
            &Some(identity.clone()),
            &Some(identity)
        ));
        assert!(!validation_is_current(&None, &None));
        let other = dir.path().join("other.sqlite3");
        fs::write(&other, [0u8; 512]).unwrap();
        let other = db_identity(&other).expect("a readable 512-byte file has an identity");
        assert!(!validation_is_current(&Some(other), &None));
    }

    /// A build's volume is what it **read**, not what it managed to store: a
    /// shard read in full and then found unparsable was still read, and the
    /// summary must count it rather than reporting a smaller archive read than
    /// happened (C6).
    #[test]
    fn bytes_read_counts_a_shard_whose_extraction_failed() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let sources = [SourceDoc::fingerprinted("machine-one/session", "source-v1")];
        let stats = index
            .build(&sources, |_| {
                // The loader read 21 bytes of the shard and then failed to
                // extract them, which is what the archive reader for a
                // pretty-printed-JSON shard does.
                Err(LoadFailure::new(
                    21,
                    anyhow!("invalid archived JSONL record at line 1"),
                ))
            })
            .unwrap();
        assert_eq!(
            stats.bytes_read, 21,
            "the shard was read before its extraction failed"
        );
        assert_eq!(stats.not_indexable.len(), 1);
        assert_eq!(
            stats.read,
            stats.indexed + stats.not_indexable.len(),
            "every source the build took on is either stored or named"
        );
    }

    /// The marker word that makes document `n` findable on its own.
    ///
    /// Fixed width matters: the trigram tokenizer answers a quoted query as a
    /// substring, so `marker1` would also match `marker10` and the query would
    /// stop hitting exactly one document.
    fn large_marker(n: usize) -> String {
        format!("hedgehogmarker{n:04}")
    }

    /// Deterministic, varied, mostly-ASCII body of roughly `target_chars`,
    /// carrying `keyword` exactly once so the large-index query can target one
    /// document. Varied enough for the trigram tokenizer to build a realistic
    /// index, deterministic enough to be reproducible.
    fn large_body(seed: u64, target_chars: usize, keyword: &str) -> String {
        let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut next = || {
            state = state.rotate_left(5).wrapping_mul(0x2545_F491_4F6C_DD1D);
            (state >> 32) as usize
        };
        let words = [
            "hedgehog",
            "blueberry",
            "alpine",
            "stream",
            "morning",
            "sun",
            "fir",
            "quiet",
            "pauses",
            "beside",
            "every",
            "before",
            "clears",
            "walks",
            "over",
            "bush",
            "tree",
        ];
        let keyword_at = next() % 997;
        let mut out = String::with_capacity(target_chars + 8);
        let mut i = 0usize;
        while out.len() < target_chars {
            let word = if i % 997 == keyword_at {
                keyword
            } else {
                words[next() % words.len()]
            };
            out.push_str(word);
            i += 1;
            if out.len() < target_chars {
                out.push(' ');
            }
        }
        out
    }

    /// The SRCH-6 latency bound on a large index, run explicitly because it
    /// builds a multi-gigabyte database:
    ///
    /// ```text
    /// cargo test -p chat-stasher --lib fts:: -- --ignored query_latency_on_a_large_index_is_bounded
    /// CS_FTS_LATENCY_MIB=256 cargo test -p chat-stasher --lib fts:: -- --ignored query_latency_on_a_large_index_is_bounded
    /// ```
    ///
    /// A text query must answer without re-validating the whole index. The probe
    /// is a query that matches exactly **one** document, because that is what
    /// isolates the defect: a query matching many documents costs real work in
    /// `snippet()`/`bm25()` proportional to matches × body length, which grows
    /// with the *query* and is not the thing SRCH-6 is about. What grows with
    /// index size regardless of the query is validation, and before SRCH-6 each
    /// query re-ran the full integrity scan (W255 C10 measured >12 minutes per
    /// query at archive scale). This bound is RED against that and GREEN now.
    #[test]
    #[ignore = "builds a large synthetic index; run explicitly with --ignored"]
    fn query_latency_on_a_large_index_is_bounded() {
        use std::time::Instant;
        // MiB of body text to index. The default reaches a GiB — the scale
        // W255 C10 measured against.
        let total_mib: u64 = std::env::var("CS_FTS_LATENCY_MIB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1024);
        let total_chars: usize = (total_mib as usize) * 1024 * 1024;
        let docs = 512;
        let per_doc = total_chars / docs;
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let sources: Vec<SourceDoc> = (0..docs)
            .map(|n| SourceDoc::fingerprinted(format!("large/{n}"), format!("source-{n}")))
            .collect();
        let started = Instant::now();
        index
            .build(&sources, |id| {
                let n: usize = id.strip_prefix("large/").unwrap().parse().unwrap();
                let body = large_body(n as u64, per_doc, &large_marker(n));
                let chars = body.chars().count();
                Ok(the(
                    DocText {
                        title: "synthetic title".to_string(),
                        body,
                        message_offsets: Vec::new(),
                    },
                    chars as u64,
                ))
            })
            .unwrap();
        let build_secs = started.elapsed().as_secs_f64();
        // Every document is present, so the latency below is measured over a
        // complete index rather than over whatever the build happened to finish.
        assert_eq!(index.summary().unwrap().ids.len(), docs);
        let query = large_marker(docs / 2);
        // One warm query validates the index once and populates the mtime cache.
        let warm = index.matches(&query).unwrap().unwrap();
        assert_eq!(warm.len(), 1, "the marker is unique to one document");
        const QUERIES: usize = 25;
        let mut worst = 0.0f64;
        for _ in 0..QUERIES {
            let t = Instant::now();
            let set = index.matches(&query).unwrap().unwrap();
            assert_eq!(set.len(), 1);
            worst = worst.max(t.elapsed().as_secs_f64());
        }
        // A per-query integrity_check over a several-GB database is measured in
        // seconds to minutes (W255 C10); a validated query is milliseconds. 0.5 s
        // is a generous ceiling for the line between the two.
        const MAX_WORST_SECS: f64 = 0.5;
        eprintln!(
            "large-index latency: docs={docs} text_MiB={total_mib} \
             build={build_secs:.1}s worst_query={worst:.4}s max={MAX_WORST_SECS}s"
        );
        assert!(
            worst <= MAX_WORST_SECS,
            "worst query over {total_mib} MiB took {worst:.4}s; \
             a text query must not re-validate the whole index each time"
        );
    }
}
