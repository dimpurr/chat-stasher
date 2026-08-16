//! inbox ingestion — consume ext export bundles (`deepseek-<sessionId>.json`)
//! from an inbox dir into sealed staging shards.
//!
//! Contract decisions (spike B7b):
//!
//! * **The inbox is a staging area, never a backup root.** Mutable inbox files
//!   must not live inside the repository (putting them there forces whole-file
//!   re-upload and turns the dedup into a lottery); the inbox and the stage
//!   tree are therefore distinct directories.
//! * **Each complete bundle becomes exactly one record** in a *sealed* shard
//!   at `sessions/<machine>/<id>/NNNNNN.jsonl`. Sequence numbers continue past
//!   existing shards and a sealed shard is never touched again.
//! * **`.part` files are skipped** — ext writes two-phase (`.part` first, final
//!   name second, then the `.part` is removed); a `.part` is mid-write and must
//!   not be archived.
//! * **Consumption strategy = retire to `<inbox>/consumed/`** (rename) only
//!   *after* the shard is durably sealed. Rationale: `raw.text` is the
//!   authoritative copy and deleting it outright before a push has happened
//!   risks data loss; `consumed/` keeps the inbox readable and auditable while
//!   still making a rescan cheap. The crash analysis that falls out of this
//!   ordering is:
//!     - crash before seal → inbox file untouched → clean re-consumption;
//!     - crash mid-seal → a `NNNNNN.jsonl.tmp` stub left behind, cleaned on the
//!       next run before the same sequence number is reused;
//!     - crash between seal and retire → the inbox file is still present, the
//!       next run recognises the identical bytes via the record's `fileSha256`
//!       and treats it as a duplicate instead of writing a second shard;
//!     - crash after retire → nothing left in the inbox, nothing re-consumed.
//!   No window can lose data and no window can double-write, because
//!   **idempotency is content-addressed**: two passes over the same raw bytes
//!   (same file, renamed file, or a copy from another machine) produce exactly
//!   one shard.
//! * **IDs.** The web identity axis is the *account*; these bundles carry no
//!   account field, so the id is honestly `platform.sessionId`
//!   (`deepseek.<sessionId>`) and the missing axis is flagged in the report
//!   instead of inventing one. The id is deliberately machine-independent, so
//!   two machines ingesting the same session produce the same id.
//!
//! Privacy line: only paths / counts / bytes / sha256 are ever reported — the
//! authoritative `raw.text` is archived into the sealed shards but never
//! printed.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::store;

/// Suffix of a mid-write file; never consumed.
pub const PART_SUFFIX: &str = ".part";
/// Subdirectory of the inbox where consumed files are retired to.
pub const CONSUMED_DIR: &str = "consumed";
/// Staging shard line schema (same as the bundle schema).
pub const SCHEMA: &str = "chat-stasher/inbox@1";
/// Leftover temp name pattern for a half-written shard (`000123.jsonl.tmp`).
const TMP_SUFFIX: &str = ".jsonl.tmp";

// ---------------------------------------------------------------- report

/// Per-file metadata about a successfully consumed bundle. Metadata only —
/// no content is ever carried here.
#[derive(Debug, Clone)]
pub struct Consumed {
    /// Baseline inbox file name.
    pub source_file: String,
    /// The composed id, `platform.sessionId`.
    pub id: String,
    /// Sealed shard that now holds this record, e.g. `000001.jsonl`.
    pub shard: String,
    /// File size in bytes.
    pub file_bytes: u64,
    /// sha256 of the whole bundled file (the dedup key).
    pub file_sha256: String,
    /// `bundle` when the envelope parsed, `raw` when it degraded to raw-only.
    pub kind: String,
}

/// A file whose bytes were already archived by an earlier run.
#[derive(Debug, Clone)]
pub struct Duplicate {
    pub source_file: String,
    pub id: String,
    pub file_sha256: String,
    /// The existing shard whose record carries the same `fileSha256`.
    pub matched_shard: String,
}

#[derive(Debug, Clone)]
pub struct ErrorEntry {
    pub source_file: String,
    pub message: String,
}

/// Full outcome of one `ingest` pass. `raw.text` never appears here.
#[derive(Debug, Clone, Default)]
pub struct IngestReport {
    /// Complete (non-`.part`) candidate files seen in the inbox.
    pub total_inbox_files: usize,
    /// `.part` files skipped (mid-write).
    pub part_files_seen: usize,
    pub consumed: Vec<Consumed>,
    pub duplicates: Vec<Duplicate>,
    pub errors: Vec<ErrorEntry>,
}

// -------------------------------------------------------------- envelopes

/// Lenient view of the ext bundle. Field names mirror the JSON keys verbatim so
/// a mis-formed field is captured leniently instead of failing the whole parse;
/// unknown fields are ignored by serde's default.
#[allow(non_snake_case, dead_code)]
#[derive(Debug, Deserialize)]
struct Bundle {
    schema: Option<String>,
    platform: Option<String>,
    sessionId: Option<String>,
    capturedAt: Option<String>,
    parsed: Option<serde_json::Value>,
    raw: Option<serde_json::Value>,
}

/// Record stored inside each sealed shard (one JSONL line per bundle).
#[derive(Debug, Serialize)]
struct ShardRecord {
    schema: &'static str,
    kind: &'static str,
    id: String,
    platform: String,
    session_id: String,
    source_file: String,
    file_sha256: String,
    file_bytes: u64,
    captured_at: Option<String>,
    parsed: ParsedEnvelope,
    raw: RawEnvelope,
}

/// The `parsed` envelope — best-effort, re-derivable from `raw`.
#[derive(Debug, Serialize)]
struct ParsedEnvelope {
    has_json: bool,
    keys: Vec<String>,
}

/// The `raw` envelope — authoritative by contract.
#[derive(Debug, Serialize)]
struct RawEnvelope {
    text: String,
    bytes: u64,
}

/// Lightweight view for the content-dedup scan of existing shard lines.
#[derive(Debug, Deserialize)]
struct LookupRecord {
    file_sha256: Option<String>,
}

// ---------------------------------------------------------------- ingest

/// Consume every complete bundle in `inbox` into sealed shards under
/// `<stage>/sessions/<machine>/<id>/NNNNNN.jsonl`.
pub fn ingest(inbox: &Path, stage: &Path, machine: &str) -> anyhow::Result<IngestReport> {
    let consumed_dir = inbox.join(CONSUMED_DIR);
    fs::create_dir_all(&consumed_dir)
        .with_context(|| format!("create {}", consumed_dir.display()))?;
    fs::create_dir_all(stage).with_context(|| format!("create {}", stage.display()))?;

    let mut report = IngestReport::default();
    let mut candidates: Vec<PathBuf> = Vec::new();

    for entry in fs::read_dir(inbox).with_context(|| format!("read inbox {}", inbox.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == CONSUMED_DIR {
            continue; // our own retirement dir, never rescanned
        }
        match entry.metadata() {
            Ok(m) if m.is_file() => {}
            _ => continue, // subdirectories / unreadable entries skipped
        }
        if name.ends_with(PART_SUFFIX) {
            report.part_files_seen += 1;
            continue; // two-phase ext, mid-write
        }
        candidates.push(entry.path());
    }
    candidates.sort(); // deterministic seq order across runs
    report.total_inbox_files = candidates.len();

    for path in &candidates {
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        match consume_one(&name, path, stage, machine, &consumed_dir) {
            Ok(Outcome::Consumed(c)) => report.consumed.push(c),
            Ok(Outcome::Duplicate(d)) => report.duplicates.push(d),
            Err(e) => report.errors.push(ErrorEntry { source_file: name, message: e.to_string() }),
        }
    }
    Ok(report)
}

enum Outcome {
    Consumed(Consumed),
    Duplicate(Duplicate),
}

fn consume_one(
    name: &str,
    path: &Path,
    stage: &Path,
    machine: &str,
    consumed_dir: &Path,
) -> anyhow::Result<Outcome> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let file_sha256 = sha256_hex(&bytes);
    let file_bytes = bytes.len() as u64;

    let parsed = parse_bundle(name, &bytes);
    let session_dir = store::session_shard_dir(stage, machine, &parsed.id);

    // Content-addressed idempotency: identical raw bytes already archived?
    let known = existing_file_shas(&session_dir)?;
    if let Some(matched_shard) = known.get(&file_sha256) {
        retire(name, path, consumed_dir)?; // still retire, so it is not rescanned
        return Ok(Outcome::Duplicate(Duplicate {
            source_file: name.to_string(),
            id: parsed.id.clone(),
            file_sha256,
            matched_shard: matched_shard.clone(),
        }));
    }

    let record = ShardRecord {
        schema: SCHEMA,
        kind: parsed.kind,
        id: parsed.id.clone(),
        platform: parsed.platform,
        session_id: parsed.session_id,
        source_file: name.to_string(),
        file_sha256: file_sha256.clone(),
        file_bytes,
        captured_at: parsed.captured_at,
        parsed: parsed.parsed,
        raw: parsed.raw,
    };
    let line = serde_json::to_string(&record).context("serialise shard record")?;

    // Seal first, retire second.
    let shard = write_shard_atomic(stage, machine, &parsed.id, &[line])?;
    retire(name, path, consumed_dir)?;

    Ok(Outcome::Consumed(Consumed {
        source_file: name.to_string(),
        id: parsed.id,
        shard,
        file_bytes,
        file_sha256,
        kind: record.kind.to_string(),
    }))
}

/// Scan a session's existing sealed shards, mapping each archived
/// `fileSha256` to the shard that first carries it.
fn existing_file_shas(session_dir: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    if !session_dir.exists() {
        return Ok(out);
    }
    let mut names: Vec<String> = fs::read_dir(session_dir)
        .with_context(|| format!("read {}", session_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|n| n.ends_with(store::SHARD_SUFFIX))
        .collect();
    names.sort();
    for name in names {
        let raw = fs::read_to_string(session_dir.join(&name))
            .with_context(|| format!("read {}", session_dir.join(&name).display()))?;
        for line in raw.lines() {
            if let Ok(r) = serde_json::from_str::<LookupRecord>(line) {
                if let Some(sha) = r.file_sha256 {
                    out.entry(sha).or_insert_with(|| name.clone());
                }
            }
        }
    }
    Ok(out)
}

/// Write one sealed shard crash-safely: temp file + fsync + atomic rename.
/// Stale temp files from an interrupted earlier write are cleaned first so the
/// same sequence number is reused, not skipped.
fn write_shard_atomic(
    stage: &Path,
    machine: &str,
    id: &str,
    lines: &[String],
) -> anyhow::Result<String> {
    let dir = store::session_shard_dir(stage, machine, id);
    fs::create_dir_all(&dir)?;
    clean_stale_tmp(&dir)?;
    let seq = store::next_shard_seq(stage, machine, id);
    let final_path = store::shard_path(stage, machine, id, seq);
    let tmp_path = dir.join(format!("{seq:06}{TMP_SUFFIX}"));

    let mut f = fs::File::create(&tmp_path).with_context(|| format!("create {}", tmp_path.display()))?;
    for l in lines {
        f.write_all(l.as_bytes())?;
        f.write_all(b"\n")?;
    }
    f.sync_all().context("sync temp shard")?;
    drop(f);
    fs::rename(&tmp_path, &final_path).with_context(|| {
        format!("seal shard {} (rename {})", final_path.display(), tmp_path.display())
    })?;
    fsync_dir(&dir).ok();
    Ok(store::shard_filename(seq))
}

/// Remove shard temp stubs left by an interrupted seal.
fn clean_stale_tmp(dir: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(TMP_SUFFIX) {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// Rename a consumed inbox file into `<inbox>/consumed/` (atomic same-fs).
fn retire(name: &str, src: &Path, consumed_dir: &Path) -> anyhow::Result<()> {
    let dst = consumed_dir.join(name);
    if dst.exists() {
        let _ = fs::remove_file(&dst);
    }
    fs::rename(src, &dst).with_context(|| format!("retire {} -> {}", src.display(), dst.display()))?;
    fsync_dir(consumed_dir).ok();
    Ok(())
}

/// Best-effort directory fsync (durability of renames).
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    let f = fs::File::open(dir)?;
    f.sync_all()
}

// ---------------------------------------------------------------- parsing

struct ParseOutcome {
    id: String,
    platform: String,
    session_id: String,
    captured_at: Option<String>,
    kind: &'static str,
    parsed: ParsedEnvelope,
    raw: RawEnvelope,
}

/// Parse a bundle; a total failure degrades to a `kind=raw` record whose raw
/// holds the whole file — `raw.text` is authoritative, never dropped.
fn parse_bundle(name: &str, bytes: &[u8]) -> ParseOutcome {
    let fallback_id = sanitize_component(&native_id_from_name(name));
    let default_platform = "deepseek".to_string();

    let mut out = ParseOutcome {
        id: format!("{}.{}", default_platform, fallback_id),
        platform: default_platform,
        session_id: fallback_id,
        captured_at: None,
        kind: "raw",
        parsed: ParsedEnvelope { has_json: false, keys: Vec::new() },
        raw: RawEnvelope { text: String::new(), bytes: 0 },
    };

    let bundle: Bundle = match serde_json::from_slice(bytes) {
        Ok(b) => b,
        Err(_) => {
            // Degrade: raw = whole file, re-derivable from the archived record.
            out.raw.text = String::from_utf8_lossy(bytes).into_owned();
            out.raw.bytes = bytes.len() as u64;
            return out;
        }
    };

    let platform = sanitize_component(bundle.platform.as_deref().unwrap_or("deepseek"));
    let session = sanitize_component(
        bundle
            .sessionId
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(out.session_id.as_str()),
    );
    out.kind = "bundle";
    out.platform = platform.clone();
    out.session_id = session.clone();
    out.id = format!("{platform}.{session}");
    out.captured_at = bundle.capturedAt.filter(|s| !s.is_empty());

    if let Some(v) = bundle.parsed.as_ref() {
        out.parsed.has_json = v.get("hasJson").and_then(|x| x.as_bool()).unwrap_or(false);
        out.parsed.keys = v
            .get("keys")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect())
            .unwrap_or_default();
    }
    if let Some(v) = bundle.raw.as_ref() {
        out.raw.text = v.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
        out.raw.bytes = v
            .get("bytes")
            .and_then(|x| x.as_u64())
            .unwrap_or(out.raw.text.len() as u64);
    }
    out
}

/// `deepseek-<sessionId>.json` -> `<sessionId>` (best-effort fallback id).
fn native_id_from_name(name: &str) -> String {
    let mut s = name.to_string();
    if let Some(stripped) = s.strip_prefix("deepseek-") {
        s = stripped.to_string();
    }
    if let Some(stripped) = s.strip_suffix(PART_SUFFIX) {
        s = stripped.to_string();
    }
    if let Some(stripped) = s.strip_suffix(".json") {
        s = stripped.to_string();
    }
    if s.is_empty() {
        s = "unknown".to_string();
    }
    s
}

/// Keep an id component filesystem-safe and `:`-free (drives `sessionShardDir`).
fn sanitize_component(raw: &str) -> String {
    let out: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' { c } else { '-' })
        .collect();
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let out = Sha256::digest(bytes);
    out.iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn synthetic_bundle(session_id: &str, raw_text: &str) -> String {
        format!(
            r#"{{"schema":{schema},"platform":"deepseek","sessionId":"{sid}","url":"https://chat.deepseek.com/a/chat/s/{sid}","method":"POST","status":200,"capturedAt":"2026-08-16T00:00:00.000Z","parsed":{{"hasJson":true,"keys":["id","content"]}},"raw":{{"text":{raw},"bytes":{n}}}}}"#,
            schema = serde_json::to_string(SCHEMA).unwrap(),
            sid = session_id,
            raw = serde_json::to_string(raw_text).unwrap(),
            n = raw_text.len(),
        )
    }

    fn write_bundle(inbox: &Path, name: &str, session_id: &str, raw_text: &str) {
        fs::write(inbox.join(name), synthetic_bundle(session_id, raw_text)).unwrap();
    }

    fn shard_names(stage: &Path, machine: &str, id: &str) -> Vec<String> {
        let dir = store::session_shard_dir(stage, machine, id);
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(store::SHARD_SUFFIX))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn skips_part_files_and_seals_bundles() {
        let dir = tempfile::TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let stage = dir.path().join("stage");
        fs::create_dir_all(&inbox).unwrap();

        write_bundle(&inbox, "deepseek-sess-a1.json", "sess-a1", "hello a");
        fs::write(inbox.join("deepseek-sess-b1.json.part"), "not-json-partial").unwrap();

        let report = ingest(&inbox, &stage, "mbp-test").unwrap();
        assert_eq!(report.part_files_seen, 1, ".part file must be skipped");
        assert_eq!(report.total_inbox_files, 1);
        assert_eq!(report.consumed.len(), 1);
        assert_eq!(report.duplicates.len(), 0);
        assert_eq!(report.errors.len(), 0);
        assert_eq!(report.consumed[0].id, "deepseek.sess-a1");
        assert_eq!(report.consumed[0].shard, "000001.jsonl");
        assert!(report.consumed[0].file_sha256.len() == 64);

        assert_eq!(shard_names(&stage, "mbp-test", "deepseek.sess-a1"), vec!["000001.jsonl"]);
        // retired to consumed/
        assert!(inbox.join(CONSUMED_DIR).join("deepseek-sess-a1.json").exists());
        assert!(!inbox.join("deepseek-sess-a1.json").exists());
        drop(dir);
    }

    #[test]
    fn consuming_same_file_twice_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let stage = dir.path().join("stage");
        fs::create_dir_all(&inbox).unwrap();
        write_bundle(&inbox, "deepseek-sess-a1.json", "sess-a1", "hello a");

        let r1 = ingest(&inbox, &stage, "mbp-test").unwrap();
        assert_eq!(r1.consumed.len(), 1);

        // Simulate the crash window "shard sealed, inbox file not yet retired":
        // copy the file back into the inbox root and run again.
        fs::copy(
            inbox.join(CONSUMED_DIR).join("deepseek-sess-a1.json"),
            inbox.join("deepseek-sess-a1.json"),
        )
        .unwrap();

        let r2 = ingest(&inbox, &stage, "mbp-test").unwrap();
        assert_eq!(r2.consumed.len(), 0, "no second shard may be produced");
        assert_eq!(r2.duplicates.len(), 1, "identical bytes recognised");
        assert_eq!(r2.duplicates[0].matched_shard, "000001.jsonl");
        assert_eq!(shard_names(&stage, "mbp-test", "deepseek.sess-a1"), vec!["000001.jsonl"]);
        drop(dir);
    }

    #[test]
    fn identical_bytes_from_another_filename_are_a_duplicate() {
        let dir = tempfile::TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let stage = dir.path().join("stage");
        fs::create_dir_all(&inbox).unwrap();
        let original = synthetic_bundle("sess-a1", "hello a");
        fs::write(inbox.join("deepseek-sess-a1.json"), &original).unwrap();
        fs::write(inbox.join("deepseek-sess-a1-copy.json"), &original).unwrap();

        let r = ingest(&inbox, &stage, "mbp-test").unwrap();
        assert_eq!(r.consumed.len(), 1);
        assert_eq!(r.duplicates.len(), 1);
        assert_eq!(shard_names(&stage, "mbp-test", "deepseek.sess-a1"), vec!["000001.jsonl"]);
        drop(dir);
    }

    #[test]
    fn same_session_different_content_produces_sealed_sequence() {
        let dir = tempfile::TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let stage = dir.path().join("stage");
        fs::create_dir_all(&inbox).unwrap();
        write_bundle(&inbox, "deepseek-sess-a1.json", "sess-a1", "hello a");
        write_bundle(&inbox, "deepseek-sess-a1-2.json", "sess-a1", "hello a (continued)");

        let r = ingest(&inbox, &stage, "mbp-test").unwrap();
        assert_eq!(r.consumed.len(), 2);
        assert_eq!(r.duplicates.len(), 0);
        assert_eq!(
            shard_names(&stage, "mbp-test", "deepseek.sess-a1"),
            vec!["000001.jsonl", "000002.jsonl"]
        );
        drop(dir);
    }

    #[test]
    fn unparseable_file_is_archived_as_raw_not_dropped() {
        let dir = tempfile::TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let stage = dir.path().join("stage");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(inbox.join("deepseek-weird-1.json"), "this is not json at all").unwrap();

        let r = ingest(&inbox, &stage, "mbp-test").unwrap();
        assert_eq!(r.errors.len(), 0, "raw-only records do not error");
        assert_eq!(r.consumed.len(), 1);
        assert_eq!(r.consumed[0].kind, "raw");
        assert_eq!(r.consumed[0].id, "deepseek.weird-1");
        assert_eq!(shard_names(&stage, "mbp-test", "deepseek.weird-1"), vec!["000001.jsonl"]);
        drop(dir);
    }

    #[test]
    fn stale_tmp_does_not_break_or_duplicate_sequence() {
        let dir = tempfile::TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let stage = dir.path().join("stage");
        fs::create_dir_all(&inbox).unwrap();
        write_bundle(&inbox, "deepseek-sess-a1.json", "sess-a1", "hello a");

        // Pre-seed a leftover from a killed seal.
        let session = store::session_shard_dir(&stage, "mbp-test", "deepseek.sess-a1");
        fs::create_dir_all(&session).unwrap();
        fs::write(session.join("000001.jsonl.tmp"), "partial-garbage").unwrap();

        let r = ingest(&inbox, &stage, "mbp-test").unwrap();
        assert_eq!(r.consumed.len(), 1);
        assert_eq!(shard_names(&stage, "mbp-test", "deepseek.sess-a1"), vec!["000001.jsonl"]);
        assert!(!session.join("000001.jsonl.tmp").exists(), "stale tmp cleaned");
        drop(dir);
    }

    #[test]
    fn ids_never_contain_colon() {
        let dir = tempfile::TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let stage = dir.path().join("stage");
        fs::create_dir_all(&inbox).unwrap();
        // Pathological sessionId forcing sanitisation.
        let data = r#"{"schema":"x","platform":"deepseek","sessionId":"a/b:c d","raw":{"text":"t","bytes":1}}"#;
        fs::write(inbox.join("deepseek-weird.json"), data).unwrap();
        let r = ingest(&inbox, &stage, "mbp-test").unwrap();
        let id = &r.consumed[0].id;
        assert!(!id.contains(':'), "no colon in id: {id}");
        assert_eq!(id, "deepseek.a-b-c-d");
        drop(dir);
    }
}