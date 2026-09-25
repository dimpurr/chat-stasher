//! Local, disposable full-text index for one archive destination.
//!
//! The index is plaintext and lives in the OS cache directory. Its directory
//! is destination-scoped, marked before use, and never part of the archive.
//! SQLite is opened and validated on every operation; an unreadable or
//! incompatible database is reported for repair instead of being replaced.

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: i64 = 1;
const MARKER: &str = ".chat-stasher-fts";
const MARKER_CONTENT: &[u8] = b"chat-stasher fts index v1\n";

/// One archived conversation requiring a content read only when its source
/// fingerprint differs from the row already indexed.
#[derive(Debug, Clone)]
pub struct SourceDoc {
    pub id: String,
    pub source_sha256: String,
}

/// Text returned by the archive reader for a changed session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocText {
    pub title: String,
    pub body: String,
}

/// Result of an incremental build.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BuildStats {
    pub documents: usize,
    pub read: usize,
    pub unchanged: usize,
    pub removed: usize,
}

/// A private search result returned to future CLI/UI readers.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub id: String,
    pub title: String,
    pub snippet: String,
    pub rank: f64,
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
    Ok((title, messages.join("\n")))
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
        if !self.db_path.exists() {
            bail!("no local index has been built; run `chat-stasher index build` (no archive read was performed)");
        }
        self.validate_owned_paths()?;
        let connection =
            Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).context(
                "open FTS index; it may be corrupt, use `chat-stasher index clear` then rebuild",
            )?;
        validate_schema(&connection)?;
        connection
            .query_row("SELECT count(*) FROM documents", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count as usize)
            .context("count indexed documents")
    }

    /// Search titles and user/assistant text with SQLite's trigram tokenizer.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        if query.chars().count() < 3 {
            return Ok(Vec::new());
        }
        let connection =
            Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).context(
                "open FTS index; it may be corrupt, use `chat-stasher index clear` then rebuild",
            )?;
        validate_schema(&connection)?;
        let mut statement = connection.prepare(
            "SELECT id, title, snippet(documents_fts, 2, '[', ']', '…', 18), bm25(documents_fts) \
             FROM documents_fts WHERE documents_fts MATCH ?1 ORDER BY bm25(documents_fts) LIMIT ?2",
        )?;
        let hits = statement
            .query_map(params![query, limit as i64], |row| {
                Ok(SearchHit {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    snippet: row.get(2)?,
                    rank: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(hits)
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
            tx.execute("DELETE FROM documents_fts WHERE id = ?1", [&source.id])?;
            tx.execute("DELETE FROM documents WHERE id = ?1", [&source.id])?;
            tx.execute(
                "INSERT INTO documents(id, source_sha256, content_sha256, title, body) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![source.id, source.source_sha256, content_sha, text.title, text.body],
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

fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE index_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO index_meta(key, value) VALUES ('schema_version', '1');
         CREATE TABLE documents (
             id TEXT PRIMARY KEY,
             source_sha256 TEXT NOT NULL,
             content_sha256 TEXT NOT NULL,
             title TEXT NOT NULL,
             body TEXT NOT NULL
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
                Ok(DocText {
                    title: id.into(),
                    body: "synthetic".into(),
                })
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
                Ok(DocText {
                    title: id.into(),
                    body: "synthetic changed".into(),
                })
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
        one.build(&source, |_| {
            Ok(DocText {
                title: "one".into(),
                body: "synthetic hedgehog material".into(),
            })
        })
        .unwrap();
        two.build(&source, |_| {
            Ok(DocText {
                title: "two".into(),
                body: "synthetic blueberry material".into(),
            })
        })
        .unwrap();
        assert_ne!(one.root(), two.root());
        assert_eq!(one.check().unwrap(), 1);
        assert_eq!(two.check().unwrap(), 1);
        assert_eq!(one.search("hedgehog", 10).unwrap().len(), 1);
        assert_eq!(one.search("blueberry", 10).unwrap().len(), 0);
        assert_eq!(two.search("blueberry", 10).unwrap().len(), 1);
        assert_eq!(two.search("hedgehog", 10).unwrap().len(), 0);
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
            .build(&sources, |_| {
                Ok(DocText {
                    title: "synthetic title".into(),
                    body: "synthetic body".into(),
                })
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
                |_| {
                    Ok(DocText {
                        title: "synthetic title".into(),
                        body: "The quiet hedgehog walks at night.".into(),
                    })
                },
            )
            .unwrap();
        let hits = index.search("hedgehog", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "synthetic/session");
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
}
