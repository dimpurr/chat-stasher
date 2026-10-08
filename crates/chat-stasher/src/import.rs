//! Official-export import — the producer side of ADR-055 D4, phase-1 groundwork.
//!
//! An **official export** (a takeout) is the platform's own statement of what an
//! account holds. This module turns one export file into two things:
//!
//! 1. the whole file, archived **byte-exact** in its own stage namespace, and
//! 2. one ordinary [`inbox@3`](crate::inbox) `web-capture` bundle per
//!    conversation, written into the **LOCAL** inbox folder, which a later
//!    `ingest --inbox` carries into the archive — this module never seals
//!    anything itself.
//!
//! It is *groundwork*, and three of its limits are decisions, not omissions:
//!
//! * **The archive probe is not wired.** Phase 1 says a conversation whose
//!   normalised body already equals the archive's latest emits an observation
//!   record and no body. Reading the latest body back out of an encrypted
//!   destination is a substantial piece of work of its own, so it sits behind
//!   [`ArchivedLatest`] and this build ships only [`ArchiveNotWired`], which
//!   answers `Unknown`. `Unknown` means *emit the body* — the conservative side
//!   of invariant 1: an archive we could not read is never read as "unchanged",
//!   because that would drop a conversation's content on the strength of not
//!   having looked.
//! * **The raw namespace is new, not ADR-053's.** ADR-053's per-machine
//!   machine-log namespace does not exist in this repository yet, so raw exports
//!   land in [`IMPORT_RAW_DIR`], a clearly separated sibling of `sessions/`. It
//!   is excluded from session counts *structurally*: every counter, manifest and
//!   verifier in `store`/`verify` walks `stage/sessions` and nothing else, so an
//!   object outside it cannot be summed. It is **not** yet wired into `push` or
//!   readback, so a raw export here is local until that lands.
//! * **The export is read whole into memory.** A Claude `conversations.json` is
//!   tens of megabytes and fits comfortably; the ChatGPT DSAR (3.53 GiB) does
//!   not, and its importer is a later slice that must stream. Nothing here
//!   pretends to handle that file yet.
//!
//! Read-only on the source (ADR-011): the export file is opened for reading and
//! never moved, renamed, rewritten or deleted.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Stage namespace holding whole official-export files, byte-exact.
///
/// A sibling of `sessions/`, never inside it — see the module docs for why that
/// distinction is what keeps these objects out of every session count.
pub const IMPORT_RAW_DIR: &str = "import-raw";
/// Stage namespace holding phase-1 observation records for conversations whose
/// normalised body was already the archive's latest.
pub const IMPORT_OBSERVATIONS_DIR: &str = "import-observations";
/// Schema marker of a raw-export provenance record.
pub const RAW_RECORD_SCHEMA: &str = "chat-stasher/import-raw-record@1";
/// Schema marker of one phase-1 observation record.
pub const OBSERVATION_SCHEMA: &str = "chat-stasher/import-observation@1";
/// The inbox schema an imported conversation bundle declares — the same marker
/// `ingest` seals an `@3` capture under, because that is what these bundles are.
pub const BUNDLE_SCHEMA: &str = crate::inbox::SEALED_SCHEMA_V3;
/// The producer `kind` these bundles attribute themselves to.
///
/// It is an existing value of the closed `@3` enum (ADR-052 verified it as
/// `send`/`extension`/`collect`/`ingest`), which is the point: an import invents
/// no wire vocabulary. ADR-055 D4 proposed `collect` for this producer; the
/// dispatch for this slice named `ingest`, and the two are recorded here so the
/// difference is visible in the code rather than lost between documents.
pub const PRODUCER_KIND: &str = "ingest";
/// Why a bundle declares `fidelity: full/export` rather than `raw`: the body is
/// the platform's own conversation object, complete, but re-serialised by this
/// producer, so it is a complete *export representation*, not a byte copy. The
/// byte copy exists — it is the whole file in [`IMPORT_RAW_DIR`].
pub const FIDELITY_REPRESENTATION: &str = "export";

/// The identifier grammar this producer is willing to carry into a session id.
///
/// Deliberately stricter than the `@3` schema, which only demands a non-space
/// `sessionId`: an id outside this grammar cannot be made into a path component
/// without rewriting it, and ADR-052 D7's rule is to refuse an unsafe identifier
/// rather than sanitise it silently.
fn conversation_id_is_safe(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && id.len() <= 255
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Why `import` could not produce an answer. The three variants exist because
/// the command answers with a *different exit code* for each: collapsing them is
/// exactly what invariant 2 forbids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// The export was never read to the end: it is absent, unreadable, or its
    /// JSON stopped part-way. Nothing about its contents is proven, so the
    /// command exits `3` and not a partial success counted as whole (ADR-014).
    ReadIncomplete(String),
    /// The file was read completely and is not something this build can import —
    /// a manifest instead of a conversations file, JSON that is not an array
    /// or not valid JSON at all, or a platform with no parser. Nothing was
    /// written; exit `2`.
    WrongInput(String),
    /// Everything was read and understood, and a write still failed. Exit `1`.
    WriteFailed(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadIncomplete(m) | Self::WrongInput(m) | Self::WriteFailed(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ImportError {}

/// An official export this command knows the shape of, or will soon.
///
/// Every value is offered by `--platform` so that "is there an importer for
/// DeepSeek yet?" is a named refusal the command can state, rather than clap
/// rejecting an unrecognised word and teaching the user nothing about the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TakeoutPlatform {
    Claude,
    DeepSeek,
    ChatGpt,
    Grok,
    Gemini,
}

impl TakeoutPlatform {
    /// The platform slug, which is also the bundle's `platform` field and the
    /// name of the namespace directory under [`IMPORT_RAW_DIR`].
    pub fn slug(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::DeepSeek => "deepseek",
            Self::ChatGpt => "chatgpt",
            Self::Grok => "grok",
            Self::Gemini => "gemini",
        }
    }

    /// Does this build parse this platform's export? Only `claude` does in this
    /// slice; the order the rest land in is ADR-055 D10 (DeepSeek and ChatGPT
    /// are gated by their platform's own extension validation first).
    pub fn has_parser(self) -> bool {
        matches!(self, Self::Claude)
    }
}

/// What the archive holds as the newest normalised body of one conversation.
///
/// Three states because the question has three honest answers, and collapsing
/// `Unknown` into `Absent` is how an unreadable archive would come to justify
/// skipping a conversation's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchivedBody {
    /// The archive holds this canonical JSON as its latest body for the id.
    Held(String),
    /// The archive was read, and it holds nothing for this id.
    Absent,
    /// The archive could not be consulted. `String` names why.
    Unknown(String),
}

/// The archive side of the phase-1 rule.
///
/// An object, not a function pointer, so that a later slice can hand
/// `import` a real read-back probe (destination-aware, ADR-012) without
/// changing anything this module decides.
pub trait ArchivedLatest {
    /// Newest normalised body for `(platform, conversation_id)`, or the named
    /// reason it cannot be answered.
    fn latest(&self, platform: &str, conversation_id: &str) -> ArchivedBody;

    /// One line saying what kind of probe this is, for the report. It is the
    /// probe's own claim, never inferred by the caller from a count.
    fn describe(&self) -> String;
}

/// The probe this build ships: it can answer nothing, and says so.
#[derive(Debug, Clone, Copy)]
pub struct ArchiveNotWired;

impl ArchivedLatest for ArchiveNotWired {
    fn latest(&self, _platform: &str, _conversation_id: &str) -> ArchivedBody {
        ArchivedBody::Unknown(
            "the archive read-back is not wired into the import producer in this build".to_string(),
        )
    }

    fn describe(&self) -> String {
        "unwired — every conversation is treated as new".to_string()
    }
}

/// What one conversation of an export becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationPlan {
    /// Emit an `@3` bundle carrying the body.
    EmitBody,
    /// Emit an observation record and no body: the normalised conversation is
    /// equal to the archive's latest, so there is nothing new to archive.
    ObservationOnly,
}

/// The phase-1 comparison rule, as a pure function.
///
/// Only [`ArchivedBody::Held`] with an equal body may suppress a bundle. `Absent`
/// is "nothing here yet" and `Unknown` is "we did not look successfully"; both
/// must produce a body, and `Unknown` must produce one *even though* a later
/// `ingest` will recognise the identical bytes as a duplicate — because the
/// alternative is to trust a skipped body to an archive we just admitted we
/// could not read.
pub fn plan_conversation(latest: &ArchivedBody, canonical: &str) -> ConversationPlan {
    match latest {
        ArchivedBody::Held(previous) if previous == canonical => ConversationPlan::ObservationOnly,
        _ => ConversationPlan::EmitBody,
    }
}

/// Deterministic, order-normalised JSON for version identity (ADR-055 F12).
///
/// Object keys are sorted at every depth; array order is the source's own,
/// because for these platforms the order *is* content. Two exports that differ
/// only in how the platform happened to lay out its keys therefore compare equal,
/// while a message added, removed or edited changes the string.
///
/// The sort is stated rather than assumed: `serde_json`'s default map is a
/// `BTreeMap`, so this crate as configured already hands back sorted keys and the
/// explicit sort changes nothing. It is written down because that default is a
/// feature flag away from being a `HashMap` (`preserve_order`), and equality that
/// quietly depends on which one the build happened to select is worse than no
/// equality at all.
///
/// Known, stated losses: a JSON object repeating a key has already been folded
/// by `serde_json` before this sees it, and a float is re-spelled in Rust's
/// shortest round-trip form. Neither is reachable in the exports measured so far
/// (27-ORACLE §3.2), and both would affect *equality*, never the archived bytes —
/// the whole file is kept byte-exact alongside, which is where the authority is.
pub fn canonical_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &serde_json::Value, out: &mut String) {
    use serde_json::Value as V;
    match value {
        V::Null => out.push_str("null"),
        V::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        V::Number(n) => out.push_str(&n.to_string()),
        V::String(s) => {
            out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string()))
        }
        V::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        V::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(
                    &serde_json::to_string(key.as_str()).unwrap_or_else(|_| "\"\"".to_string()),
                );
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// One conversation of an export, kept as the platform wrote it.
#[derive(Debug, Clone)]
pub struct ExportConversation {
    /// The platform's own conversation id.
    pub conversation_id: String,
    /// The whole conversation object, field for field (ADR-052 D6: no field
    /// projection at capture, nulls and omissions preserved).
    pub object: serde_json::Value,
}

/// Parsed an export file into conversations, plus the ones it could not.
#[derive(Debug)]
pub struct ParsedExport {
    pub conversations: Vec<ExportConversation>,
    /// One entry per conversation this producer refused to emit, naming its
    /// index and the reason. A refusal is an exit-1 condition, never a skip.
    pub rejections: Vec<String>,
}

/// Parse a Claude export's `conversations.json`.
///
/// The shape is 27-ORACLE §3.2: one zip per category, and the `conversations`
/// category is a single **flat array** of conversation objects. Only that file is
/// accepted here; the manifest is refused by name, because the manifest's
/// `export_url` values are one-time-use credentials that must never be echoed.
pub fn parse_claude_conversations(bytes: &[u8]) -> Result<ParsedExport, ImportError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| {
        // One parse failure is two different answers (invariant 2), and
        // serde_json names the difference: `Eof` means the input stopped
        // part-way, so the export was never read to the end and nothing
        // about its contents is proven — exit `3`. Every other category
        // means the bytes were read in full and are not a JSON document:
        // a completed read of invalid input — exit `2`. `Io` cannot occur
        // while reading a byte slice, but it would also mean the read did
        // not finish, so it stays on the `3` side.
        if e.is_eof() || e.is_io() {
            ImportError::ReadIncomplete(format!(
                "export did not parse as JSON and was not read to the end: {e}"
            ))
        } else {
            ImportError::WrongInput(format!(
                "export was read in full but is not valid JSON: {e}"
            ))
        }
    })?;

    if let Some(files) = value.get("data_files") {
        let count = files.as_array().map_or(0, Vec::len);
        // reason: a manifest whose category list we could not count is an unknown
        // denominator, and reporting zero categories would claim we read them all.
        return Err(ImportError::WrongInput(format!(
            "this is an export manifest ({count} data files listed), not a conversations file. \
             Unzip the `conversations` category and pass its `conversations.json`; the manifest's \
             download links are one-time-use credentials and are never read further."
        )));
    }

    let items = value.as_array().ok_or_else(|| {
        ImportError::WrongInput(
            "a Claude conversations file is one flat JSON array of conversation objects; \
             this file is not"
                .to_string(),
        )
    })?;

    let mut parsed = ParsedExport {
        conversations: Vec::with_capacity(items.len()),
        rejections: Vec::new(),
    };
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (index, item) in items.iter().enumerate() {
        let Some(object) = item.as_object() else {
            parsed
                .rejections
                .push(format!("entry {index}: not a conversation object"));
            continue;
        };
        let raw_id = object.get("uuid").and_then(|v| v.as_str());
        let Some(id) = raw_id.map(str::trim) else {
            // An absent id is not an empty id: say which of the two it was.
            parsed.rejections.push(format!(
                "entry {index}: {}",
                if object.contains_key("uuid") {
                    "conversation id is not a readable string"
                } else {
                    "no conversation id"
                }
            ));
            continue;
        };
        if !conversation_id_is_safe(id) {
            parsed
                .rejections
                .push(format!("entry {index}: conversation id is unsafe to carry"));
            continue;
        }
        if !seen.insert(id.to_string()) {
            // Two entries under one id would be two bodies for one session id,
            // and which one the reader would see is decided by shard order.
            parsed.rejections.push(format!(
                "entry {index}: conversation id repeats within this export"
            ));
            continue;
        }
        parsed.conversations.push(ExportConversation {
            conversation_id: id.to_string(),
            object: item.clone(),
        });
    }
    Ok(parsed)
}

/// The `@3` `web-capture` bundle for one imported conversation.
///
/// Three deliberate absences:
///
/// * **No `capturedAt`.** Capture time belongs to the run, not to the bundle:
///   ADR-035 takes time from content only, and the run's own moment is recorded
///   in the raw-export provenance record and the observation records. Leaving it
///   out is also what makes the bundle of an unchanged conversation byte-identical
///   across takeouts, so the archive's existing content-addressed dedup gives
///   ADR-055 D5's "100 takeouts, one body" without a new mechanism.
/// * **No `fingerprint`.** That field is the extension's own derivation
///   (`recapture.ts`, sealed by `ingest_export_file`), and the host never
///   re-derives it (protocol §6.6). A producer computing a different value under
///   the same name is how a dedup key becomes a lie; absence stays absence.
/// * **No `account`.** A conversations file does not prove an account. Only
///   `light_metadata-*/users.json` does (27-ORACLE §3.2), and that is a later
///   slice; until then `identity.level: default` states the unreliability
///   explicitly instead of leaving it to be inferred.
pub fn build_bundle(platform: &str, conversation_id: &str, canonical: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": BUNDLE_SCHEMA,
        "kind": "web-capture",
        "platform": platform,
        "sessionId": conversation_id,
        "identity": { "level": "default", "value": "" },
        "fidelity": { "value": "full", "representation": FIDELITY_REPRESENTATION },
        "producer": {
            "kind": PRODUCER_KIND,
            "version": env!("CARGO_PKG_VERSION"),
            "platform": platform,
        },
        "raw": { "text": canonical, "bytes": canonical.len() },
    })
}

/// A file name for one conversation's bundle, bounded to one path component.
///
/// Short ids keep their exact spelling (`claude-<uuid>.json`, the documented
/// inbox filename convention), so a reader recognises them; an over-long one is
/// the shared bounded form, which stays collision-safe. The bound applies to the
/// stem, so the suffix survives even in the bounded form — a name that lost its
/// extension would be a different kind of file in the inbox.
pub fn bundle_file_name(platform: &str, conversation_id: &str) -> String {
    const EXTENSION: &str = ".json";
    let stem = crate::id::bounded_path_component(
        &format!("{platform}-{conversation_id}"),
        crate::id::MAX_PATH_COMPONENT_BYTES - EXTENSION.len(),
    );
    format!("{stem}{EXTENSION}")
}

/// Write one bundle into the inbox, atomically.
///
/// Two-phase like the extension (`.part` first, final name second) because
/// `ingest` skips a `.part` and must never seal half a bundle.
fn publish_bundle(inbox: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, ImportError> {
    let part = inbox.join(format!("{name}.part"));
    let final_path = inbox.join(name);
    let mut file = fs::File::create(&part)
        .map_err(|e| ImportError::WriteFailed(format!("create {}: {e}", part.display())))?;
    file.write_all(bytes)
        .map_err(|e| ImportError::WriteFailed(format!("write {}: {e}", part.display())))?;
    file.sync_all()
        .map_err(|e| ImportError::WriteFailed(format!("sync {}: {e}", part.display())))?;
    drop(file);
    fs::rename(&part, &final_path)
        .map_err(|e| ImportError::WriteFailed(format!("publish {}: {e}", final_path.display())))?;
    // Best-effort, and deliberately not fatal, for the same reason B74 makes the
    // retirement fsync best-effort: if the directory entry is lost to a power cut
    // the bundle is simply absent, and the next `import` pass writes it again from
    // the same bytes. Failing the run over a durability question a re-run answers
    // would block the user on something that self-heals.
    fsync_dir(inbox).ok();
    Ok(final_path)
}

fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    // Opening a directory to fsync it is a Unix idiom; Windows has no
    // portable directory fsync, so every other directory fsync in this
    // crate (shard_writer, audit_store, identity, inbox_config) is gated
    // behind cfg(unix) and Windows keeps the rename as its durable step.
    // This helper follows that convention: a no-op there, so the required
    // call in `archive_raw_export` still holds on Unix, and the
    // best-effort one in `publish_bundle` stays best-effort everywhere.
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// The result of archiving the whole export file.
#[derive(Debug, Clone)]
pub struct RawArchive {
    /// Where the byte-exact copy now lives.
    pub path: PathBuf,
    /// sha256 of those bytes, which is also the file's name.
    pub sha256: String,
    pub bytes: u64,
    /// `false` when these exact bytes were already archived, so the run stored
    /// nothing new. A repeat of a byte-identical package is a no-op, not a
    /// second copy (27-ORACLE F7: the same export often lives in 2–3 places).
    pub newly_stored: bool,
}

/// Archive the export file byte-exact under `<stage>/<IMPORT_RAW_DIR>/<platform>/`.
///
/// Content-addressed by its own sha256, which is what makes a repeat a no-op
/// without a journal to get out of step with the disk.
pub fn archive_raw_export(
    stage: &Path,
    platform: &str,
    source_name: &str,
    bytes: &[u8],
) -> Result<RawArchive, ImportError> {
    let dir = stage.join(IMPORT_RAW_DIR).join(platform);
    let sha256 = sha256_hex(bytes);
    let target = dir.join(&sha256);

    match fs::metadata(&target) {
        Ok(existing) if existing.len() == bytes.len() as u64 => {
            // The name is the digest of the bytes we hold and the length matches;
            // re-copying would rewrite identical bytes under a lock nobody holds.
            // The provenance record is re-published regardless, because a run that
            // died between the two writes left a body with no record naming it.
            write_raw_record(&dir, &sha256, source_name, bytes.len() as u64)?;
            return Ok(RawArchive {
                path: target,
                sha256,
                bytes: bytes.len() as u64,
                newly_stored: false,
            });
        }
        Ok(existing) => {
            return Err(ImportError::WriteFailed(format!(
                "{} already exists with {} bytes, which disagrees with the {} bytes digesting to \
                 this name — refusing to overwrite an archived export",
                target.display(),
                existing.len(),
                bytes.len()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(ImportError::ReadIncomplete(format!(
                "stat {}: {e}",
                target.display()
            )))
        }
    }

    fs::create_dir_all(&dir)
        .map_err(|e| ImportError::WriteFailed(format!("create {}: {e}", dir.display())))?;
    let part = dir.join(format!("{sha256}.tmp"));
    let mut file = fs::File::create(&part)
        .map_err(|e| ImportError::WriteFailed(format!("create {}: {e}", part.display())))?;
    file.write_all(bytes)
        .map_err(|e| ImportError::WriteFailed(format!("write {}: {e}", part.display())))?;
    file.sync_all()
        .map_err(|e| ImportError::WriteFailed(format!("sync {}: {e}", part.display())))?;
    drop(file);
    fs::rename(&part, &target)
        .map_err(|e| ImportError::WriteFailed(format!("seal {}: {e}", target.display())))?;
    // Unlike the inbox's best-effort dir fsync, this one is required: the
    // promise of this namespace is that the platform's bytes survived the
    // run. On Unix that promise is proven by the directory fsync below;
    // Windows has no portable directory fsync (see `fsync_dir`), so there
    // the atomic rename is the durable step, as it is for every other
    // sealed write in this crate.
    fsync_dir(&dir)
        .map_err(|e| ImportError::WriteFailed(format!("sync {}: {e}", dir.display())))?;
    write_raw_record(&dir, &sha256, source_name, bytes.len() as u64)?;

    Ok(RawArchive {
        path: target,
        sha256,
        bytes: bytes.len() as u64,
        newly_stored: true,
    })
}

/// The metadata record beside a raw export: which file produced these bytes.
///
/// The body's own name is a digest and says nothing about its origin, and the
/// user's export file may not outlive this run. Only the file *name* is recorded,
/// never its directory — an absolute path in the stage would publish the shape of
/// the machine that wrote it.
fn write_raw_record(
    dir: &Path,
    sha256: &str,
    source_name: &str,
    bytes: u64,
) -> Result<(), ImportError> {
    let record = RawRecord {
        schema: RAW_RECORD_SCHEMA,
        sha256: sha256.to_string(),
        bytes,
        source_name: source_name.to_string(),
        stored_at: chrono::Utc::now().to_rfc3339(),
    };
    write_json_record(&dir.join(format!("{sha256}.provenance.json")), &record)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RawRecord {
    schema: &'static str,
    sha256: String,
    bytes: u64,
    source_name: String,
    stored_at: String,
}

/// One phase-1 observation: this export saw this conversation, and its
/// normalised body equalled the archive's latest, so no body was emitted.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ObservationRecord {
    schema: &'static str,
    platform: String,
    conversation_id: String,
    /// sha256 of the canonical JSON, so a later slice can join this row to the
    /// version the body carries without re-normalising anything.
    canonical_sha256: String,
    /// Which export file this sighting came from, by its archived digest.
    export_sha256: String,
    observed_at: String,
    /// Why there is no body, in the record itself. A row that only *implies* it
    /// omitted a body would be read as an empty conversation.
    body: &'static str,
}

fn write_json_record<T: Serialize>(path: &Path, record: &T) -> Result<(), ImportError> {
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| ImportError::WriteFailed(format!("serialise record: {e}")))?;
    let part = path.with_extension("tmp");
    let mut file = fs::File::create(&part)
        .map_err(|e| ImportError::WriteFailed(format!("create {}: {e}", part.display())))?;
    file.write_all(&bytes)
        .map_err(|e| ImportError::WriteFailed(format!("write {}: {e}", part.display())))?;
    file.sync_all()
        .map_err(|e| ImportError::WriteFailed(format!("sync {}: {e}", part.display())))?;
    drop(file);
    fs::rename(&part, path)
        .map_err(|e| ImportError::WriteFailed(format!("publish {}: {e}", path.display())))?;
    Ok(())
}

/// Everything one `import` pass decided, and nothing it read.
///
/// Every field is a count, a digest, a bounded name or a path. No conversation
/// text crosses this boundary, and the strings in [`ImportReport::rejections`]
/// name an index and a reason rather than the entry that failed.
#[derive(Debug)]
pub struct ImportReport {
    pub platform: &'static str,
    /// The stage object holding the whole file, byte-exact.
    pub raw: RawArchive,
    pub conversations_seen: usize,
    pub bundles_written: usize,
    pub observations_written: usize,
    pub rejections: Vec<String>,
    /// What kind of archive probe answered — the probe's own words.
    pub archive_probe: String,
}

/// Run one import pass: archive the file, then emit per conversation.
///
/// The inbox and the raw namespace are written; `sessions/` never is, and
/// nothing here seals or pushes. `ingest --inbox` is a separate, explicit step.
pub fn run(
    platform: TakeoutPlatform,
    export: &Path,
    inbox: &Path,
    stage: &Path,
    archive: &dyn ArchivedLatest,
) -> Result<ImportReport, ImportError> {
    let slug = platform.slug();
    if !platform.has_parser() {
        return Err(ImportError::WrongInput(format!(
            "this build has no {slug} export parser: {slug} is named by `--platform`, \
             but its importer is a later slice"
        )));
    }

    if !stage.is_dir() {
        return Err(ImportError::WrongInput(format!(
            "stage {} is not a directory; import never creates a stage",
            stage.display()
        )));
    }

    let bytes = fs::read(export)
        .map_err(|e| ImportError::ReadIncomplete(format!("read {}: {e}", export.display())))?;
    let source_name = export
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".to_string());

    // Parse before archiving, not after. The raw namespace's promise is that the
    // platform's own bytes of an *imported* export survived the run, and a file
    // this producer refuses has no business being kept byte-exact on the strength
    // of having been named on a command line: a Claude manifest is valid JSON
    // whose `export_url` values are one-time-use credentials, and archiving it
    // first would write those tokens into the stage and then exit 2 as if nothing
    // had happened. So a run that ends `2` or `3` has written nothing anywhere.
    let parsed = parse_claude_conversations(&bytes)?;
    let raw = archive_raw_export(stage, slug, &source_name, &bytes)?;
    publish_inbox_dir(inbox)?;

    let mut bundles_written = 0usize;
    let mut observations_written = 0usize;
    for conversation in &parsed.conversations {
        let canonical = canonical_json(&conversation.object);
        let latest = archive.latest(slug, &conversation.conversation_id);
        match plan_conversation(&latest, &canonical) {
            ConversationPlan::EmitBody => {
                let bundle = build_bundle(slug, &conversation.conversation_id, &canonical);
                let text = serde_json::to_string(&bundle)
                    .map_err(|e| ImportError::WriteFailed(format!("serialise bundle: {e}")))?;
                let name = bundle_file_name(slug, &conversation.conversation_id);
                publish_bundle(inbox, &name, text.as_bytes())?;
                bundles_written += 1;
            }
            ConversationPlan::ObservationOnly => {
                let dir = stage
                    .join(IMPORT_OBSERVATIONS_DIR)
                    .join(slug)
                    .join(&raw.sha256);
                fs::create_dir_all(&dir).map_err(|e| {
                    ImportError::WriteFailed(format!("create {}: {e}", dir.display()))
                })?;
                let record = ObservationRecord {
                    schema: OBSERVATION_SCHEMA,
                    platform: slug.to_string(),
                    conversation_id: conversation.conversation_id.clone(),
                    canonical_sha256: sha256_hex(canonical.as_bytes()),
                    export_sha256: raw.sha256.clone(),
                    observed_at: chrono::Utc::now().to_rfc3339(),
                    // Stated in the record, because a row that merely lacks a body
                    // would be indistinguishable from one that found none.
                    body: "not-emitted: normalised body equals the archive's latest",
                };
                let name = bundle_file_name(slug, &conversation.conversation_id);
                write_json_record(&dir.join(name), &record)?;
                observations_written += 1;
            }
        }
    }

    Ok(ImportReport {
        platform: slug,
        raw,
        conversations_seen: parsed.conversations.len() + parsed.rejections.len(),
        bundles_written,
        observations_written,
        rejections: parsed.rejections,
        archive_probe: archive.describe(),
    })
}

/// The inbox must exist before a bundle lands in it; unlike the stage, an inbox
/// is a drop folder this command may legitimately create.
fn publish_inbox_dir(inbox: &Path) -> Result<(), ImportError> {
    fs::create_dir_all(inbox)
        .map_err(|e| ImportError::WriteFailed(format!("create inbox {}: {e}", inbox.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A tiny fake Claude `conversations.json`: the flat array of §3.2, with the
    /// fields the real export carries and text nobody wrote by hand.
    fn synthetic_claude_export(ids: &[&str]) -> String {
        let conversations: Vec<serde_json::Value> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                serde_json::json!({
                    "uuid": id,
                    "name": format!("synthetic-title-{i}"),
                    "created_at": "2026-06-01T00:00:00.000Z",
                    "updated_at": "2026-06-02T00:00:00.000Z",
                    "chat_messages": [
                        {
                            "uuid": format!("{id}-m0"),
                            "sender": "human",
                            "text": format!("synthetic prompt {i}"),
                            "content": [{ "type": "text", "text": format!("synthetic prompt {i}") }],
                            "created_at": "2026-06-01T00:00:00.000Z",
                            "parent_message_uuid": id,
                        },
                        {
                            "uuid": format!("{id}-m1"),
                            "sender": "assistant",
                            "text": format!("synthetic answer {i}"),
                            "content": [{ "type": "text", "text": format!("synthetic answer {i}") }],
                            "created_at": "2026-06-01T00:01:00.000Z",
                            "parent_message_uuid": format!("{id}-m0"),
                        },
                    ],
                })
            })
            .collect();
        serde_json::to_string(&conversations).unwrap()
    }

    const ID_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const ID_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";

    /// A probe over an in-memory map, so the phase-1 rule is testable without an
    /// archive. Keys are `(platform, conversation_id)`.
    struct MapProbe {
        held: BTreeMap<(String, String), String>,
    }

    impl MapProbe {
        fn empty() -> Self {
            Self {
                held: BTreeMap::new(),
            }
        }
    }

    impl ArchivedLatest for MapProbe {
        fn latest(&self, platform: &str, conversation_id: &str) -> ArchivedBody {
            match self
                .held
                .get(&(platform.to_string(), conversation_id.to_string()))
            {
                Some(body) => ArchivedBody::Held(body.clone()),
                None => ArchivedBody::Absent,
            }
        }

        fn describe(&self) -> String {
            "synthetic in-memory probe".to_string()
        }
    }

    // ------------------------------------------------------ canonical JSON

    #[test]
    fn canonical_json_sorts_keys_at_every_depth_and_ignores_input_order() {
        let left: serde_json::Value =
            serde_json::from_str(r#"{"b":1,"a":{"d":[1,2],"c":"x"}}"#).unwrap();
        let right: serde_json::Value =
            serde_json::from_str(r#"{"a":{"c":"x","d":[1,2]},"b":1}"#).unwrap();
        assert_eq!(canonical_json(&left), canonical_json(&right));
        assert_eq!(canonical_json(&left), r#"{"a":{"c":"x","d":[1,2]},"b":1}"#);
    }

    #[test]
    fn canonical_json_keeps_array_order_as_content() {
        let a: serde_json::Value = serde_json::from_str(r#"[{"i":1},{"i":2}]"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(r#"[{"i":2},{"i":1}]"#).unwrap();
        assert_ne!(canonical_json(&a), canonical_json(&b));
    }

    /// Invariant 1 on the comparison axis: a field that is `null`, a field that
    /// is absent, and a field that is the empty string are three different
    /// conversations, and equality must not flatten them.
    #[test]
    fn canonical_json_keeps_null_absent_and_empty_apart() {
        let null: serde_json::Value = serde_json::from_str(r#"{"a":null,"b":1}"#).unwrap();
        let absent: serde_json::Value = serde_json::from_str(r#"{"b":1}"#).unwrap();
        let empty: serde_json::Value = serde_json::from_str(r#"{"a":"","b":1}"#).unwrap();
        assert_ne!(canonical_json(&null), canonical_json(&absent));
        assert_ne!(canonical_json(&null), canonical_json(&empty));
        assert_ne!(canonical_json(&absent), canonical_json(&empty));
    }

    // --------------------------------------------------- phase-1 decision

    #[test]
    fn an_equal_archived_body_plans_an_observation_and_no_body() {
        let latest = ArchivedBody::Held(r#"{"uuid":"x"}"#.to_string());
        assert_eq!(
            plan_conversation(&latest, r#"{"uuid":"x"}"#),
            ConversationPlan::ObservationOnly
        );
    }

    #[test]
    fn a_changed_archived_body_plans_a_body() {
        let latest = ArchivedBody::Held(r#"{"uuid":"x"}"#.to_string());
        assert_eq!(
            plan_conversation(&latest, r#"{"uuid":"y"}"#),
            ConversationPlan::EmitBody
        );
    }

    #[test]
    fn an_absent_archive_plans_a_body() {
        assert_eq!(
            plan_conversation(&ArchivedBody::Absent, "{}"),
            ConversationPlan::EmitBody
        );
    }

    /// The one that matters. `Unknown` is not `Absent` and not `Held`: an archive
    /// we could not read must never be the reason a conversation's content is
    /// skipped, because the skipping is the thing that cannot be undone.
    #[test]
    fn an_unreadable_archive_still_plans_a_body() {
        let unknown = ArchivedBody::Unknown("destination unreachable".to_string());
        assert_eq!(
            plan_conversation(&unknown, "{}"),
            ConversationPlan::EmitBody
        );
        // The probe this build ships answers exactly that way, for every id.
        assert!(matches!(
            ArchiveNotWired.latest("claude", ID_A),
            ArchivedBody::Unknown(_)
        ));
    }

    // ------------------------------------------------------- the parser

    #[test]
    fn parse_reads_the_flat_array_and_keeps_every_field() {
        let export = synthetic_claude_export(&[ID_A, ID_B]);
        let parsed = parse_claude_conversations(export.as_bytes()).unwrap();
        assert_eq!(parsed.conversations.len(), 2);
        assert!(parsed.rejections.is_empty());
        assert_eq!(parsed.conversations[0].conversation_id, ID_A);
        // Field-for-field: the parser projects nothing.
        let obj = &parsed.conversations[0].object;
        assert_eq!(obj["name"], "synthetic-title-0");
        assert_eq!(obj["chat_messages"].as_array().unwrap().len(), 2);
        assert!(obj.get("uuid").is_some());
    }

    #[test]
    fn a_truncated_export_is_a_read_failure_not_a_short_answer() {
        let export = synthetic_claude_export(&[ID_A, ID_B]);
        let cut = &export[..export.len() - 20];
        let err = parse_claude_conversations(cut.as_bytes()).unwrap_err();
        assert!(matches!(err, ImportError::ReadIncomplete(_)), "{err}");
    }

    /// The other side of the same boundary: bytes that were read in
    /// full and are not a JSON document. The read *finished*, so the
    /// failure is about the input (exit `2`), not about how far the
    /// read got (exit `3`). A trailing comma is malformed JSON that
    /// serde_json classifies as syntax, not as an unexpected end.
    #[test]
    fn a_complete_but_malformed_export_is_wrong_input_not_an_incomplete_read() {
        let err = parse_claude_conversations(b"[{\"uuid\": \"x\",}]").unwrap_err();
        let ImportError::WrongInput(message) = err else {
            panic!(
                "a fully read malformed document is invalid input, not an \
                 incomplete read: {err}"
            );
        };
        assert!(message.contains("read in full"), "{message}");
    }

    #[test]
    fn a_manifest_is_refused_by_name_and_its_urls_are_not_read() {
        let manifest = serde_json::json!({
            "version": "1.0",
            "total_files": 6,
            "data_files": [
                { "category": "conversations", "part": 0, "filename": "conversations-000.zip",
                  "export_url": "https://example.invalid/one-time-secret-token" }
            ]
        })
        .to_string();
        let err = parse_claude_conversations(manifest.as_bytes()).unwrap_err();
        let ImportError::WrongInput(message) = err else {
            panic!("a manifest is a wrong input, not a failed read: {err}");
        };
        assert!(message.contains("manifest"), "{message}");
        // The refusal must not become a way to echo a one-time credential.
        assert!(
            !message.contains("one-time-secret-token"),
            "the refusal echoed a download link"
        );
    }

    #[test]
    fn a_non_array_file_is_refused_as_the_wrong_input() {
        let err = parse_claude_conversations(b"{\"conversations\":[]}").unwrap_err();
        assert!(matches!(err, ImportError::WrongInput(_)), "{err}");
    }

    #[test]
    fn conversations_without_a_safe_id_are_refused_not_sanitised() {
        let cases = [
            (
                serde_json::json!({ "chat_messages": [] }),
                "no conversation id",
            ),
            (
                serde_json::json!({ "uuid": 12, "chat_messages": [] }),
                "not a readable string",
            ),
            (
                serde_json::json!({ "uuid": "../escape", "chat_messages": [] }),
                "unsafe",
            ),
            (
                serde_json::json!({ "uuid": "", "chat_messages": [] }),
                "unsafe",
            ),
        ];
        for (object, expected_reason) in cases {
            let bytes = serde_json::to_vec(&vec![object]).unwrap();
            let parsed = parse_claude_conversations(&bytes).unwrap();
            assert!(
                parsed.conversations.is_empty(),
                "{expected_reason}: nothing is emitted for a refused id"
            );
            assert_eq!(parsed.rejections.len(), 1, "{expected_reason}");
            assert!(
                parsed.rejections[0].contains(expected_reason),
                "{expected_reason}: got {}",
                parsed.rejections[0]
            );
        }
    }

    #[test]
    fn a_repeated_id_in_one_export_is_refused_once_emitted_once() {
        let bytes = serde_json::to_vec(&vec![
            serde_json::json!({ "uuid": ID_A, "chat_messages": [] }),
            serde_json::json!({ "uuid": ID_A, "chat_messages": [] }),
        ])
        .unwrap();
        let parsed = parse_claude_conversations(&bytes).unwrap();
        assert_eq!(parsed.conversations.len(), 1);
        assert_eq!(parsed.rejections.len(), 1);
        assert!(
            parsed.rejections[0].contains("repeats"),
            "{}",
            parsed.rejections[0]
        );
    }

    // -------------------------------------------------------- the bundle

    #[test]
    fn an_imported_conversation_is_a_valid_inbox_v3_web_capture() {
        let export = synthetic_claude_export(&[ID_A]);
        let parsed = parse_claude_conversations(export.as_bytes()).unwrap();
        let canonical = canonical_json(&parsed.conversations[0].object);
        let bundle = build_bundle("claude", ID_A, &canonical);
        let bytes = serde_json::to_vec(&bundle).unwrap();
        crate::inbox::check_bundle(&bytes)
            .unwrap_or_else(|e| panic!("the bundle this producer writes is not a bundle: {e}"));
    }

    #[test]
    fn a_bundle_declares_export_fidelity_and_names_no_volatile_provenance() {
        let bundle = build_bundle("claude", ID_A, "{}");
        assert_eq!(bundle["schema"], BUNDLE_SCHEMA);
        assert_eq!(bundle["kind"], "web-capture");
        assert_eq!(bundle["platform"], "claude");
        assert_eq!(bundle["sessionId"], ID_A);
        assert_eq!(bundle["fidelity"]["value"], "full");
        assert_eq!(bundle["fidelity"]["representation"], "export");
        assert_eq!(bundle["producer"]["kind"], "ingest");
        assert_eq!(bundle["producer"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(bundle["identity"]["level"], "default");
        // The three deliberate absences — see `build_bundle`.
        assert!(bundle.get("capturedAt").is_none());
        assert!(bundle.get("fingerprint").is_none());
        assert!(bundle.get("account").is_none());
        // `raw.bytes` is the byte length of what `raw.text` holds, not a guess.
        let text = bundle["raw"]["text"].as_str().unwrap();
        assert_eq!(bundle["raw"]["bytes"].as_u64().unwrap(), text.len() as u64);
    }

    /// The property the archive's existing dedup relies on for "100 takeouts,
    /// one body": nothing in the bundle depends on the wall clock.
    #[test]
    fn the_same_conversation_produces_the_same_bundle_bytes_twice() {
        let export = synthetic_claude_export(&[ID_A]);
        let first = serde_json::to_string(&{
            let parsed = parse_claude_conversations(export.as_bytes()).unwrap();
            build_bundle(
                "claude",
                ID_A,
                &canonical_json(&parsed.conversations[0].object),
            )
        })
        .unwrap();
        let second = serde_json::to_string(&{
            let parsed = parse_claude_conversations(export.as_bytes()).unwrap();
            build_bundle(
                "claude",
                ID_A,
                &canonical_json(&parsed.conversations[0].object),
            )
        })
        .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn bundle_names_stay_one_path_component() {
        let short = bundle_file_name("claude", ID_A);
        assert_eq!(short, format!("claude-{ID_A}.json"));
        let long = bundle_file_name("claude", &"a".repeat(400));
        assert!(
            long.len() <= crate::id::MAX_PATH_COMPONENT_BYTES,
            "{} bytes",
            long.len()
        );
        assert!(long.ends_with(".json"), "{long}");
    }

    // ------------------------------------------------- the raw namespace

    #[test]
    fn a_raw_export_is_archived_byte_exact_and_a_repeat_stores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        fs::create_dir_all(&stage).unwrap();
        let bytes = b"[{\"uuid\":\"synthetic\"}]".to_vec();

        let first = archive_raw_export(&stage, "claude", "conversations.json", &bytes).unwrap();
        assert!(first.newly_stored);
        assert_eq!(fs::read(&first.path).unwrap(), bytes);
        assert_eq!(first.sha256, {
            // The name is the digest of the bytes it holds.
            use sha2::Digest;
            Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        });

        let second = archive_raw_export(&stage, "claude", "conversations.json", &bytes).unwrap();
        assert!(!second.newly_stored, "a byte-identical repeat is a no-op");
        assert_eq!(first.path, second.path);

        // A distinct export is kept, never overwritten (invariant 3).
        let other = archive_raw_export(&stage, "claude", "conversations-2.json", b"[]").unwrap();
        assert!(other.newly_stored);
        assert_ne!(other.path, first.path);
        assert_eq!(fs::read(&first.path).unwrap(), bytes);
    }

    /// The exclusion is structural, and this is the assertion that keeps it
    /// honest: the namespace must not be able to move a session count.
    #[test]
    fn a_raw_export_never_enters_session_counts() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        fs::create_dir_all(&stage).unwrap();
        archive_raw_export(
            &stage,
            "claude",
            "conversations.json",
            b"[{\"uuid\":\"s\"}]",
        )
        .unwrap();
        assert_eq!(crate::store::sealed_shard_count(&stage).unwrap(), 0);
        assert!(
            crate::store::stage_file_sha256s(&stage).unwrap().is_empty(),
            "the ingest audit scan must not see a raw export"
        );
    }

    #[test]
    fn a_refused_stage_is_refused_rather_than_created() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-stage");
        let err = run(
            TakeoutPlatform::Claude,
            Path::new("synthetic.json"),
            &dir.path().join("inbox"),
            &missing,
            &ArchiveNotWired,
        )
        .unwrap_err();
        assert!(matches!(err, ImportError::WrongInput(_)), "{err}");
        assert!(!missing.exists(), "import must not manufacture a stage");
    }

    #[test]
    fn a_platform_without_a_parser_is_refused_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        fs::create_dir_all(&stage).unwrap();
        for platform in [
            TakeoutPlatform::DeepSeek,
            TakeoutPlatform::ChatGpt,
            TakeoutPlatform::Grok,
            TakeoutPlatform::Gemini,
        ] {
            let err = run(
                platform,
                &dir.path().join("whatever.json"),
                &dir.path().join("inbox"),
                &stage,
                &ArchiveNotWired,
            )
            .unwrap_err();
            assert!(
                matches!(err, ImportError::WrongInput(_)),
                "{platform:?}: {err}"
            );
            assert!(
                err.to_string().contains(platform.slug()),
                "{platform:?}: the refusal must name the platform"
            );
        }
    }

    // ------------------------------------------------------- the run

    #[test]
    fn a_run_writes_bundles_into_the_inbox_and_touches_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        let inbox = dir.path().join("inbox");
        fs::create_dir_all(&stage).unwrap();
        let export = dir.path().join("conversations.json");
        let text = synthetic_claude_export(&[ID_A, ID_B]);
        fs::write(&export, &text).unwrap();

        let report = run(
            TakeoutPlatform::Claude,
            &export,
            &inbox,
            &stage,
            &ArchiveNotWired,
        )
        .unwrap();

        assert_eq!(report.conversations_seen, 2);
        assert_eq!(report.bundles_written, 2);
        assert_eq!(report.observations_written, 0);
        assert!(report.rejections.is_empty());
        // Unwired probe ⇒ bodies for everything, and the report says why.
        assert!(
            report.archive_probe.contains("unwired"),
            "{}",
            report.archive_probe
        );

        let written: Vec<String> = fs::read_dir(&inbox)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(written.len(), 2, "{written:?}");
        assert!(!written.iter().any(|n| n.ends_with(".part")), "{written:?}");
        for name in &written {
            let bytes = fs::read(inbox.join(name)).unwrap();
            crate::inbox::check_bundle(&bytes)
                .unwrap_or_else(|e| panic!("{name} is not a valid bundle: {e}"));
        }
        assert_eq!(crate::store::sealed_shard_count(&stage).unwrap(), 0);
        assert_eq!(
            fs::read_to_string(&export).unwrap(),
            text,
            "the source was not touched"
        );
    }

    /// The phase-1 rule end to end, with a probe that actually holds the body.
    #[test]
    fn an_unchanged_conversation_writes_an_observation_and_no_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        let inbox = dir.path().join("inbox");
        fs::create_dir_all(&stage).unwrap();
        let export = dir.path().join("conversations.json");
        let text = synthetic_claude_export(&[ID_A, ID_B]);
        fs::write(&export, &text).unwrap();

        let parsed = parse_claude_conversations(text.as_bytes()).unwrap();
        let mut probe = MapProbe::empty();
        // The archive already holds A exactly as this export states it, and holds
        // a *different* body for B.
        probe.held.insert(
            ("claude".to_string(), ID_A.to_string()),
            canonical_json(&parsed.conversations[0].object),
        );
        probe.held.insert(
            ("claude".to_string(), ID_B.to_string()),
            "{\"changed\":true}".to_string(),
        );

        let report = run(TakeoutPlatform::Claude, &export, &inbox, &stage, &probe).unwrap();
        assert_eq!(report.bundles_written, 1, "only the changed conversation");
        assert_eq!(report.observations_written, 1, "only the unchanged one");

        let names: Vec<String> = fs::read_dir(&inbox)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![bundle_file_name("claude", ID_B)]);

        let observations = stage
            .join(IMPORT_OBSERVATIONS_DIR)
            .join("claude")
            .join(&report.raw.sha256);
        let files: Vec<String> = fs::read_dir(&observations)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, vec![bundle_file_name("claude", ID_A)]);
        let record: serde_json::Value =
            serde_json::from_slice(&fs::read(observations.join(&files[0])).unwrap()).unwrap();
        assert_eq!(record["schema"], OBSERVATION_SCHEMA);
        assert_eq!(record["conversationId"], ID_A);
        assert_eq!(record["platform"], "claude");
        assert_eq!(record["exportSha256"], report.raw.sha256);
        assert!(record["canonicalSha256"].as_str().unwrap().len() == 64);
        // The record states, in itself, that it carries no body.
        assert!(
            record["body"].as_str().unwrap().starts_with("not-emitted:"),
            "{record}"
        );
    }

    /// Re-importing the same file into the same inbox must not grow the tree.
    #[test]
    fn a_second_run_of_the_same_export_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        let inbox = dir.path().join("inbox");
        fs::create_dir_all(&stage).unwrap();
        let export = dir.path().join("conversations.json");
        fs::write(&export, synthetic_claude_export(&[ID_A])).unwrap();

        let first = run(
            TakeoutPlatform::Claude,
            &export,
            &inbox,
            &stage,
            &ArchiveNotWired,
        )
        .unwrap();
        let second = run(
            TakeoutPlatform::Claude,
            &export,
            &inbox,
            &stage,
            &ArchiveNotWired,
        )
        .unwrap();
        assert!(!second.raw.newly_stored, "the raw file is stored once");
        assert_eq!(first.raw.sha256, second.raw.sha256);
        assert_eq!(first.bundles_written, second.bundles_written);
        let entries = fs::read_dir(&inbox).unwrap().count();
        assert_eq!(entries, 1, "one bundle per conversation, not two");
    }
}
