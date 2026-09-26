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

const SCHEMA_VERSION: i64 = 2;
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

/// One archived conversation requiring a content read only when its source
/// fingerprint differs from the row already indexed.
#[derive(Debug, Clone)]
pub struct SourceDoc {
    pub id: String,
    pub source_sha256: String,
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

/// Result of an incremental build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BuildStats {
    pub documents: usize,
    pub read: usize,
    pub unchanged: usize,
    pub removed: usize,
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
    /// The index file's last modification time. This is a **file mtime**, not a
    /// recorded build time — the index records no build time of its own, and
    /// reporting a computed one would be inventing a fact about when the text
    /// it holds was read.
    pub written_unix: Option<i64>,
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
        }
    }

    /// Construct an index rooted at a caller-owned path (also useful for tests).
    pub fn at(root: PathBuf) -> Self {
        Self {
            db_path: root.join("index.sqlite3"),
            root,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Open read-only and validate the schema. Missing remains distinct from
    /// corrupt: callers can offer a build command only for the former.
    pub fn check(&self) -> Result<usize> {
        let connection = self.open_valid()?;
        connection
            .query_row("SELECT count(*) FROM documents", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count as usize)
            .context("count indexed documents")
    }

    /// The read-only connection every reader of this index starts from: a
    /// missing file and a corrupt one stay two different instructions, and
    /// both are refused before any question is asked of the index.
    fn open_valid(&self) -> Result<Connection> {
        if !self.db_path.exists() {
            bail!("no local index has been built; run `chat-stasher index build` (no archive read was performed)");
        }
        self.validate_owned_paths()?;
        let connection =
            Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).context(
                "open FTS index; it may be corrupt, use `chat-stasher index clear` then rebuild",
            )?;
        validate_schema(&connection)?;
        Ok(connection)
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
        let written_unix = fs::metadata(&self.db_path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_secs() as i64);
        Ok(IndexSummary { ids, written_unix })
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
        let connection =
            Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).context(
                "open FTS index; it may be corrupt, use `chat-stasher index clear` then rebuild",
            )?;
        validate_schema(&connection)?;
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
        let connection =
            Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).context(
                "open FTS index; it may be corrupt, use `chat-stasher index clear` then rebuild",
            )?;
        validate_schema(&connection)?;
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
    /// fingerprints. A failing read leaves the current transaction untouched.
    pub fn build<F>(&self, sources: &[SourceDoc], mut load: F) -> Result<BuildStats>
    where
        F: FnMut(&str) -> Result<DocText>,
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
            if !present.insert(source.id.as_str()) {
                bail!("duplicate source document id in index input");
            }
            if old.get(&source.id) == Some(&source.source_sha256) {
                stats.unchanged += 1;
                continue;
            }
            let text = load(&source.id).with_context(|| "read changed archived document")?;
            let content_sha = digest_text(&text.title, &text.body);
            let offsets: Vec<u64> = text
                .message_offsets
                .iter()
                .map(|offset| *offset as u64)
                .collect();
            tx.execute("DELETE FROM documents_fts WHERE id = ?1", [&source.id])?;
            tx.execute("DELETE FROM documents WHERE id = ?1", [&source.id])?;
            tx.execute(
                "INSERT INTO documents(id, source_sha256, content_sha256, title, body, message_offsets) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    source.id,
                    source.source_sha256,
                    content_sha,
                    text.title,
                    text.body,
                    serde_json::to_string(&offsets).context("serialise message offsets")?
                ],
            )?;
            tx.execute(
                "INSERT INTO documents_fts(id, title, body) VALUES (?1, ?2, ?3)",
                params![source.id, text.title, text.body],
            )?;
            stats.read += 1;
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
    connection.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE index_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO index_meta(key, value) VALUES ('schema_version', '2');
         CREATE TABLE documents (
             id TEXT PRIMARY KEY,
             source_sha256 TEXT NOT NULL,
             content_sha256 TEXT NOT NULL,
             title TEXT NOT NULL,
             body TEXT NOT NULL,
             message_offsets TEXT NOT NULL
         );
         CREATE VIRTUAL TABLE documents_fts USING fts5(id UNINDEXED, title, body, tokenize='trigram');
         COMMIT;",
    )?;
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

    #[test]
    fn incremental_build_reads_only_changed_sources() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let sources = vec![
            SourceDoc {
                id: "a".into(),
                source_sha256: "sha-a1".into(),
            },
            SourceDoc {
                id: "b".into(),
                source_sha256: "sha-b1".into(),
            },
        ];
        let calls = Cell::new(0);
        index
            .build(&sources, |id| {
                calls.set(calls.get() + 1);
                Ok(doc(id, "synthetic"))
            })
            .unwrap();
        assert_eq!(calls.get(), 2);
        let changed = vec![
            SourceDoc {
                id: "a".into(),
                source_sha256: "sha-a1".into(),
            },
            SourceDoc {
                id: "b".into(),
                source_sha256: "sha-b2".into(),
            },
        ];
        let calls = Cell::new(0);
        let stats = index
            .build(&changed, |id| {
                calls.set(calls.get() + 1);
                Ok(doc(id, "synthetic changed"))
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
        let source = [SourceDoc {
            id: "same-id".into(),
            source_sha256: "same-sha".into(),
        }];
        one.build(&source, |_| Ok(doc("one", "synthetic hedgehog material")))
            .unwrap();
        two.build(&source, |_| Ok(doc("two", "synthetic blueberry material")))
            .unwrap();
        assert_ne!(one.root(), two.root());
        assert_eq!(one.check().unwrap(), 1);
        assert_eq!(two.check().unwrap(), 1);
        assert_eq!(one.matches("hedgehog").unwrap().unwrap().len(), 1);
        assert_eq!(one.matches("blueberry").unwrap().unwrap().len(), 0);
        assert_eq!(two.matches("blueberry").unwrap().unwrap().len(), 1);
        assert_eq!(two.matches("hedgehog").unwrap().unwrap().len(), 0);
    }

    #[test]
    fn corrupt_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        index.build(&[], |_| Ok(DocText::default())).unwrap();
        fs::write(index.db_path(), b"not sqlite").unwrap();
        let error = index.check().unwrap_err().to_string();
        assert!(error.contains("corrupt") || error.contains("invalid"));
    }

    #[test]
    fn inconsistent_open_database_is_refused_by_check_and_build() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::at(dir.path().join("index"));
        let sources = [SourceDoc {
            id: "synthetic/session".into(),
            source_sha256: "synthetic-source".into(),
        }];
        index
            .build(&sources, |_| Ok(doc("synthetic title", "synthetic body")))
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
                Ok(DocText::default())
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
        index.build(&[], |_| Ok(DocText::default())).unwrap();
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
                &[SourceDoc {
                    id: "synthetic/session".into(),
                    source_sha256: "synthetic-source".into(),
                }],
                |_| Ok(doc("synthetic title", "The quiet hedgehog walks at night.")),
            )
            .unwrap();
        let set = index.matches("hedgehog").unwrap().unwrap();
        assert_eq!(set.len(), 1);
        assert_eq!(set.matches[0].id, "synthetic/session");
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
                &[SourceDoc {
                    id: "synthetic/session".into(),
                    source_sha256: "synthetic-source".into(),
                }],
                |_| Ok(text.clone()),
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
            SourceDoc {
                id: "machine-one/session-a".into(),
                source_sha256: "sha-a".into(),
            },
            SourceDoc {
                id: "machine-one/session-b".into(),
                source_sha256: "sha-b".into(),
            },
            SourceDoc {
                id: "machine-two/session-c".into(),
                source_sha256: "sha-c".into(),
            },
            // No separator at all: the whole id is its own machine, and an id
            // that is not `<machine>/<session>` must survive the summary
            // verbatim rather than be truncated into one that looks like it.
            SourceDoc {
                id: "separatorless".into(),
                source_sha256: "sha-d".into(),
            },
        ];
        index
            .build(&sources, |id| Ok(doc(id, "synthetic body")))
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
}
