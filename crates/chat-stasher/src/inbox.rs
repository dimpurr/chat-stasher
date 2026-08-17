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
//!   at `sessions/<machine>/<id>/<bucket>/NNNNNN.jsonl`. Sequence numbers
//!   continue past existing shards and a sealed shard is never touched again.
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
use std::collections::{BTreeMap, BTreeSet};
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
const AUDIT_STATE_VERSION: u32 = 1;
const AUDIT_STATE_FILE: &str = "consumed-audit-v1.json";

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
    /// `identity.level` carried by an `inbox@2` bundle, `None` for `@1`.
    /// Reported so a run can say how many bundles actually carried the axis
    /// instead of asserting up front that none do.
    pub identity_level: Option<String>,
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

/// Metadata-only audit of the retired inbox files. Hashes are retained only
/// as set/map keys; callers may report them, but never the file names or body.
#[derive(Debug, Clone, Default)]
pub struct ConsumedAudit {
    pub file_count: usize,
    pub total_bytes: u64,
    pub cache_hits: usize,
    pub rehashed: usize,
    pub hash_counts: BTreeMap<String, usize>,
    pub stage_covered_files: usize,
    pub stage_missing_files: usize,
    pub stage_missing_sha256: BTreeSet<String>,
}

impl ConsumedAudit {
    pub fn unique_sha256(&self) -> usize {
        self.hash_counts.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuditCacheEntry {
    bytes: u64,
    modified_ns: u128,
    sha256: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AuditState {
    version: u32,
    #[serde(default)]
    known_inboxes: BTreeSet<String>,
    #[serde(default)]
    files: BTreeMap<String, AuditCacheEntry>,
}

/// Remember the inbox used by the CLI ingest path so a later `push` can audit
/// it without making every caller repeat the path. This is chat-stasher-owned
/// state, never a harness directory.
pub fn remember_inbox(inbox: &Path, state_dir: &Path) -> anyhow::Result<()> {
    let mut state = load_audit_state(state_dir)?;
    let normalized = fs::canonicalize(inbox)
        .unwrap_or_else(|_| inbox.to_path_buf())
        .to_string_lossy()
        .into_owned();
    state.known_inboxes.insert(normalized);
    save_audit_state(state_dir, &state)
}

/// Return inboxes previously used by the CLI ingest path.
pub fn remembered_inboxes(state_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    Ok(load_audit_state(state_dir)?
        .known_inboxes
        .into_iter()
        .map(PathBuf::from)
        .collect())
}

/// Hash current `consumed/` files and compare their hashes with the stage's
/// embedded `file_sha256` set. A metadata cache avoids re-reading immutable
/// retired files on every push; a size/mtime change forces a fresh hash. The
/// first audit, replacements, and metadata failures are therefore expensive
/// and conservative, while the steady-state cost is directory metadata plus
/// a small JSON state read.
pub fn audit_consumed_against_stage(
    inboxes: &[PathBuf],
    stage: &Path,
    state_dir: &Path,
) -> anyhow::Result<ConsumedAudit> {
    let mut state = load_audit_state(state_dir)?;
    let mut audit = ConsumedAudit::default();
    let mut seen_cache_keys = BTreeSet::new();
    let mut normalized_inboxes = BTreeSet::new();

    for inbox in inboxes {
        let normalized = fs::canonicalize(inbox)
            .unwrap_or_else(|_| inbox.to_path_buf())
            .to_string_lossy()
            .into_owned();
        normalized_inboxes.insert(normalized);
        let consumed_dir = inbox.join(CONSUMED_DIR);
        let entries = match fs::read_dir(&consumed_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {}", consumed_dir.display())),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            let key = fs::canonicalize(&path)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            let metadata = fs::metadata(&key).with_context(|| {
                format!(
                    "stat consumed file for audit ({})",
                    sha256_hex(key.as_bytes())
                )
            })?;
            let bytes = metadata.len();
            let modified_ns = modified_ns(&metadata);
            seen_cache_keys.insert(key.clone());
            let sha256 = match state.files.get(&key) {
                Some(cached) if cached.bytes == bytes && cached.modified_ns == modified_ns => {
                    audit.cache_hits += 1;
                    cached.sha256.clone()
                }
                _ => {
                    let raw = fs::read(&key).with_context(|| {
                        format!(
                            "read consumed file for audit ({})",
                            sha256_hex(key.as_bytes())
                        )
                    })?;
                    let sha256 = sha256_hex(&raw);
                    state.files.insert(
                        key,
                        AuditCacheEntry {
                            bytes,
                            modified_ns,
                            sha256: sha256.clone(),
                        },
                    );
                    audit.rehashed += 1;
                    sha256
                }
            };
            audit.file_count += 1;
            audit.total_bytes += bytes;
            *audit.hash_counts.entry(sha256).or_default() += 1;
        }
    }

    state.known_inboxes.extend(normalized_inboxes);
    state.files.retain(|key, _| seen_cache_keys.contains(key));
    let stage_hashes = store::stage_file_sha256s(stage)?;
    for (sha, count) in &audit.hash_counts {
        if stage_hashes.contains(sha) {
            audit.stage_covered_files += count;
        } else {
            audit.stage_missing_files += count;
            audit.stage_missing_sha256.insert(sha.clone());
        }
    }
    save_audit_state(state_dir, &state)?;
    Ok(audit)
}

fn load_audit_state(state_dir: &Path) -> anyhow::Result<AuditState> {
    let path = state_dir.join(AUDIT_STATE_FILE);
    match fs::read(&path) {
        Ok(bytes) => {
            let state: AuditState = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse inbox audit state {}", path.display()))?;
            if state.version != AUDIT_STATE_VERSION {
                anyhow::bail!("unsupported inbox audit state version {}", state.version);
            }
            Ok(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AuditState {
            version: AUDIT_STATE_VERSION,
            ..AuditState::default()
        }),
        Err(e) => Err(e).with_context(|| format!("read inbox audit state {}", path.display())),
    }
}

fn save_audit_state(state_dir: &Path, state: &AuditState) -> anyhow::Result<()> {
    fs::create_dir_all(state_dir)
        .with_context(|| format!("create inbox audit state {}", state_dir.display()))?;
    let path = state_dir.join(AUDIT_STATE_FILE);
    let tmp = state_dir.join(format!(".{AUDIT_STATE_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(state).context("serialise inbox audit state")?;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)?;
    Ok(())
}

fn modified_ns(metadata: &fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
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
    /// `inbox@2` only. Not part of `raw`, so dropping it here loses it for
    /// good — `raw.text` cannot re-derive it.
    identity: Option<serde_json::Value>,
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
    /// The `inbox@2` identity axis, preserved verbatim when the bundle carried
    /// one. Omitted entirely for `@1` bundles, so `@1` shard lines keep their
    /// existing bytes. It is *stored* but deliberately does NOT take part in
    /// the id or the dedup key — that remains `platform.sessionId` /
    /// `file_sha256`. Changing the id is a separate decision (ADR-002);
    /// silently discarding the field is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<IdentityEnvelope>,
}

/// The `identity` envelope — the account axis an `inbox@2` bundle carries.
#[derive(Debug, Clone, Serialize)]
struct IdentityEnvelope {
    level: String,
    value: String,
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
/// `<stage>/sessions/<machine>/<id>/<bucket>/NNNNNN.jsonl` using the default
/// bucket cap.
pub fn ingest(inbox: &Path, stage: &Path, machine: &str) -> anyhow::Result<IngestReport> {
    ingest_with_cap(inbox, stage, machine, store::DEFAULT_SHARD_BUCKET_CAP)
}

/// As [`ingest`], with an explicit maximum number of shards per bucket.
pub fn ingest_with_cap(
    inbox: &Path,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
) -> anyhow::Result<IngestReport> {
    store::assert_stage_writer_audited(store::StageWriter::Ingest)?;
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
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        match consume_one(&name, path, stage, machine, &consumed_dir, bucket_cap) {
            Ok(Outcome::Consumed(c)) => report.consumed.push(c),
            Ok(Outcome::Duplicate(d)) => report.duplicates.push(d),
            Err(e) => report.errors.push(ErrorEntry {
                source_file: name,
                message: e.to_string(),
            }),
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
    bucket_cap: usize,
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
        identity: parsed.identity,
    };
    let line = serde_json::to_string(&record).context("serialise shard record")?;

    // Seal first, retire second.
    let shard = write_shard_atomic(stage, machine, &parsed.id, &[line], bucket_cap)?;
    retire(name, path, consumed_dir)?;

    Ok(Outcome::Consumed(Consumed {
        source_file: name.to_string(),
        id: parsed.id,
        shard,
        file_bytes,
        file_sha256,
        kind: record.kind.to_string(),
        identity_level: record.identity.as_ref().map(|i| i.level.clone()),
    }))
}

/// Scan a session's existing sealed shards, mapping each archived
/// `fileSha256` to the shard that first carries it.
fn existing_file_shas(session_dir: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    if !session_dir.exists() {
        return Ok(out);
    }
    let mut entries = store::sealed_shard_entries(session_dir)?;
    entries.sort_by_key(|(seq, _)| *seq);
    for (_, path) in entries {
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| store::shard_filename(0));
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
    bucket_cap: usize,
) -> anyhow::Result<String> {
    let dir = store::session_shard_dir(stage, machine, id);
    fs::create_dir_all(&dir)?;
    clean_stale_tmp(&dir)?;
    let seq = store::next_shard_seq(stage, machine, id);
    let final_path = store::shard_path_with_cap(stage, machine, id, seq, bucket_cap);
    let final_dir = final_path.parent().expect("shard path has bucket parent");
    fs::create_dir_all(final_dir)?;
    let tmp_path = final_dir.join(format!("{seq:06}{TMP_SUFFIX}"));

    let mut f =
        fs::File::create(&tmp_path).with_context(|| format!("create {}", tmp_path.display()))?;
    for l in lines {
        f.write_all(l.as_bytes())?;
        f.write_all(b"\n")?;
    }
    f.sync_all().context("sync temp shard")?;
    drop(f);
    fs::rename(&tmp_path, &final_path).with_context(|| {
        format!(
            "seal shard {} (rename {})",
            final_path.display(),
            tmp_path.display()
        )
    })?;
    fsync_dir(final_dir).ok();
    Ok(store::shard_filename(seq))
}

/// Remove shard temp stubs left by an interrupted seal.
fn clean_stale_tmp(dir: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() {
            clean_stale_tmp(&entry.path())?;
        } else if name.ends_with(TMP_SUFFIX) {
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
    fs::rename(src, &dst)
        .with_context(|| format!("retire {} -> {}", src.display(), dst.display()))?;
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
    identity: Option<IdentityEnvelope>,
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
        parsed: ParsedEnvelope {
            has_json: false,
            keys: Vec::new(),
        },
        raw: RawEnvelope {
            text: String::new(),
            bytes: 0,
        },
        identity: None,
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
            .map(|a| {
                a.iter()
                    .filter_map(|k| k.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
    }
    if let Some(v) = bundle.raw.as_ref() {
        out.raw.text = v
            .get("text")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        out.raw.bytes = v
            .get("bytes")
            .and_then(|x| x.as_u64())
            .unwrap_or(out.raw.text.len() as u64);
    }
    // `inbox@2` identity. A `level` of `default` means the axis is explicitly
    // unreliable (contract), so it is kept verbatim rather than normalised —
    // the reader has to see the difference between "absent" and "default".
    if let Some(v) = bundle.identity.as_ref() {
        if let (Some(level), Some(value)) = (
            v.get("level").and_then(|x| x.as_str()),
            v.get("value").and_then(|x| x.as_str()),
        ) {
            out.identity = Some(IdentityEnvelope {
                level: level.to_string(),
                value: value.to_string(),
            });
        }
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
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
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
        let mut names: Vec<String> = store::sealed_shard_entries(&dir)
            .unwrap()
            .into_iter()
            .map(|(_, path)| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// An `inbox@2` bundle carrying the account axis (`contracts/inbox.schema.json`).
    fn synthetic_bundle_v2(session_id: &str, raw_text: &str, level: &str, value: &str) -> String {
        format!(
            r#"{{"schema":"chat-stasher/inbox@2","platform":"deepseek","sessionId":"{sid}","method":"POST","status":200,"capturedAt":"2026-08-16T00:00:00.000Z","parsed":{{"hasJson":true,"keys":["id"]}},"raw":{{"text":{raw},"bytes":{n}}},"identity":{{"level":"{level}","value":"{value}"}}}}"#,
            sid = session_id,
            raw = serde_json::to_string(raw_text).unwrap(),
            n = raw_text.len(),
        )
    }

    fn only_shard_record(stage: &Path, machine: &str, id: &str) -> serde_json::Value {
        let dir = store::session_shard_dir(stage, machine, id);
        let entries = store::sealed_shard_entries(&dir).unwrap();
        assert_eq!(entries.len(), 1, "expected exactly one sealed shard");
        let raw = fs::read_to_string(&entries[0].1).unwrap();
        serde_json::from_str(raw.lines().next().unwrap()).unwrap()
    }

    /// B53: the `@2` identity axis must survive ingest. It is not inside
    /// `raw.text`, so dropping it here loses it permanently.
    #[test]
    fn v2_identity_is_archived_verbatim() {
        let dir = tempfile::TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let stage = dir.path().join("stage");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(
            inbox.join("deepseek-sess-v2.json"),
            synthetic_bundle_v2("sess-v2", "hello v2", "platform_uid", "uid-4242"),
        )
        .unwrap();

        let report = ingest(&inbox, &stage, "mbp-test").unwrap();
        assert_eq!(report.consumed.len(), 1);
        assert_eq!(
            report.consumed[0].identity_level.as_deref(),
            Some("platform_uid"),
        );

        let rec = only_shard_record(&stage, "mbp-test", "deepseek.sess-v2");
        assert_eq!(rec["identity"]["level"], "platform_uid");
        assert_eq!(rec["identity"]["value"], "uid-4242");
        // The axis is stored, but it must NOT have moved the id or the dedup
        // key — that is a separate (ADR-002) decision.
        assert_eq!(rec["id"], "deepseek.sess-v2");
    }

    /// An `@1` bundle has no identity, and its shard line must not grow an
    /// empty one — existing archives keep their bytes.
    #[test]
    fn v1_bundle_has_no_identity_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let stage = dir.path().join("stage");
        fs::create_dir_all(&inbox).unwrap();
        write_bundle(&inbox, "deepseek-sess-v1.json", "sess-v1", "hello v1");

        let report = ingest(&inbox, &stage, "mbp-test").unwrap();
        assert_eq!(report.consumed[0].identity_level, None);

        let rec = only_shard_record(&stage, "mbp-test", "deepseek.sess-v1");
        assert!(
            rec.get("identity").is_none(),
            "an @1 bundle must not gain an identity key",
        );
    }

    /// `level:"default"` means "unreliable", which is different from absent —
    /// the reader has to be able to tell them apart.
    #[test]
    fn default_identity_level_is_kept_not_dropped() {
        let dir = tempfile::TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let stage = dir.path().join("stage");
        fs::create_dir_all(&inbox).unwrap();
        fs::write(
            inbox.join("deepseek-sess-d.json"),
            synthetic_bundle_v2("sess-d", "hello d", "default", ""),
        )
        .unwrap();

        ingest(&inbox, &stage, "mbp-test").unwrap();
        let rec = only_shard_record(&stage, "mbp-test", "deepseek.sess-d");
        assert_eq!(rec["identity"]["level"], "default");
        assert_eq!(rec["identity"]["value"], "");
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

        assert_eq!(
            shard_names(&stage, "mbp-test", "deepseek.sess-a1"),
            vec!["000001.jsonl"]
        );
        // retired to consumed/
        assert!(inbox
            .join(CONSUMED_DIR)
            .join("deepseek-sess-a1.json")
            .exists());
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
        assert_eq!(
            shard_names(&stage, "mbp-test", "deepseek.sess-a1"),
            vec!["000001.jsonl"]
        );
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
        assert_eq!(
            shard_names(&stage, "mbp-test", "deepseek.sess-a1"),
            vec!["000001.jsonl"]
        );
        drop(dir);
    }

    #[test]
    fn same_session_different_content_produces_sealed_sequence() {
        let dir = tempfile::TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let stage = dir.path().join("stage");
        fs::create_dir_all(&inbox).unwrap();
        write_bundle(&inbox, "deepseek-sess-a1.json", "sess-a1", "hello a");
        write_bundle(
            &inbox,
            "deepseek-sess-a1-2.json",
            "sess-a1",
            "hello a (continued)",
        );

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
        fs::write(
            inbox.join("deepseek-weird-1.json"),
            "this is not json at all",
        )
        .unwrap();

        let r = ingest(&inbox, &stage, "mbp-test").unwrap();
        assert_eq!(r.errors.len(), 0, "raw-only records do not error");
        assert_eq!(r.consumed.len(), 1);
        assert_eq!(r.consumed[0].kind, "raw");
        assert_eq!(r.consumed[0].id, "deepseek.weird-1");
        assert_eq!(
            shard_names(&stage, "mbp-test", "deepseek.weird-1"),
            vec!["000001.jsonl"]
        );
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
        assert_eq!(
            shard_names(&stage, "mbp-test", "deepseek.sess-a1"),
            vec!["000001.jsonl"]
        );
        assert!(
            !session.join("000001.jsonl.tmp").exists(),
            "stale tmp cleaned"
        );
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

    #[test]
    fn consumed_audit_matches_stage_and_reuses_metadata_cache() {
        let dir = tempfile::TempDir::new().unwrap();
        let inbox = dir.path().join("inbox");
        let stage = dir.path().join("stage");
        let state = dir.path().join("state");
        fs::create_dir_all(inbox.join(CONSUMED_DIR)).unwrap();
        let bytes = b"synthetic bundle bytes";
        fs::write(inbox.join(CONSUMED_DIR).join("bundle.json"), bytes).unwrap();
        let sha = sha256_hex(bytes);
        store::write_sealed_shard(
            store::StageWriter::Ingest,
            &stage,
            "machine",
            "session",
            &[format!(r#"{{"file_sha256":"{sha}"}}"#)],
        )
        .unwrap();

        let first = audit_consumed_against_stage(&[inbox.clone()], &stage, &state).unwrap();
        assert_eq!(first.file_count, 1);
        assert_eq!(first.rehashed, 1);
        assert_eq!(first.cache_hits, 0);
        assert_eq!(first.stage_covered_files, 1);
        assert_eq!(first.stage_missing_files, 0);

        let second = audit_consumed_against_stage(&[inbox], &stage, &state).unwrap();
        assert_eq!(second.file_count, 1);
        assert_eq!(second.rehashed, 0);
        assert_eq!(second.cache_hits, 1);
        assert_eq!(second.stage_covered_files, 1);
    }
}
