//! ADR-055 D4 · raw export archiving.
//!
//! A platform takeout is a whole file the user downloaded — a DeepSeek export,
//! a ChatGPT DSAR archive — and importing it is a *second* read of content the
//! archive may already hold. The file itself is evidence: what the platform
//! produced, byte for byte. So it is archived once, byte-exact, as its own
//! object, and never as a conversation.
//!
//! **Where it lives.** D4 states the key exactly:
//! `<stage>/export-files/<platform>/<sha256>/<as-delivered filename>`. The
//! directory is the file's own content digest, so it *is* the object's
//! identity, and the leaf is the name the file arrived under — the platform's
//! own name, which is what keeps a package findable for the user. Nothing below
//! `export-files/` is under [`store::SESSIONS_DIR`], which is what keeps the
//! object out of the numbers by construction rather than by a subtraction:
//! every session walker, counter, manifest and readback in this crate is scoped
//! to `sessions/`, so a directory that is not under it is invisible to
//! "N sessions archived", coverage, `machine_recall`, `status` and the support
//! matrix's session columns. That is ADR-053 D4's argument for the machine-log
//! namespace, reused because it is the only argument that holds without a
//! growing list of exclusions.
//!
//! **What the digest key buys.** Three things follow from naming the directory
//! by the content, and a name-derived layout could not give any of them: a
//! byte-identical repeat of the same package — which ADR-055 F7 says is the
//! normal case, one package living in two or three places — is recognised
//! before any bytes are copied, so a repeat is a no-op instead of a
//! multi-gigabyte duplicate; two *different* exports are always both kept,
//! because they cannot collide on a key; and the same package seen from a
//! second machine is the same object, not a second body.
//!
//! **The delivered name is data, so it is checked like data.** It becomes a path
//! component, so [`validate_delivered_name`] refuses anything that would name
//! more than one component, a directory, or a hidden file: `/`, `\`, NUL,
//! control characters, `.`, `..`, a leading dot, an empty name, and anything
//! over 255 bytes. A name that is refused is a usage error
//! ([`ExportLabelError`]), never a rewrite — the archive keeps the name the user
//! actually has, and a name it cannot keep is a mistake to report rather than a
//! silent substitution. Because a byte-identical repeat under a *different* name
//! is still the same object, the name first sighted is the one the object keeps,
//! and [`SealOutcome::AlreadyArchived`] reports which name that was.
//!
//! **What is borrowed from the shard machinery, and what is not.** The object is
//! written by a registered stage writer ([`store::StageWriter::Export`], whose
//! reconciliation hook is the content address against its index row), it travels
//! through `push` because `push` backs up the whole stage tree, and it is bound
//! into the run's metadata digest through its index row, so a stage whose only
//! new content is an export still pushes instead of being refused as empty. What
//! is *not* borrowed is the shard *grammar*: a session shard is `NNNNNN.jsonl`
//! and carries a `shard-seq` counter because [`crate::stagereclaim`] can delete a
//! session's newest shard and a later append has to resume the sequence. Nothing
//! deletes or appends to an export object — one content address holds exactly one
//! object, forever — so there is no sequence to keep and the object keeps the
//! platform's own name instead. This namespace therefore has its own presence
//! check ([`held_object`], which ignores hidden files exactly as
//! [`store::sealed_shard_entries`] does) and its own path matcher
//! ([`export_object_path`]), and the shard grammar is never applied to it: a
//! consumer that wanted to parse these bytes as JSONL would be wrong twice over,
//! since the platform's export may be a zip.
//!
//! **How it reaches the destination.** `push` copies the object with no new
//! transport, and the byte-exactness of what landed is proven by
//! [`read_archived_export`], which re-derives the digest from the bytes it reads
//! and compares it against the address that was asked for. A metadata-only index
//! row per object — `<stage>/meta/<machine>/export-files-v1.jsonl` — is what
//! makes the run noticeable: [`crate::metahash`] hashes the contents of
//! `meta/<machine>/`, so an export staged beside an unchanged session set changes
//! the run's metadata digest, and `BackupStore::push`'s empty-snapshot guard has
//! something to push for even on a stage that holds no new shards. The bodies
//! deliberately do **not** enter that walk: the metahash pass reads each file it
//! lists into memory, and a 3.5 GiB DSAR is not metadata. The row is the object's
//! digest, so the content hash is bound into the run without reading the content
//! again — and it is metadata only, so it names no path and no file name; the
//! object's own name lives in the namespace, where D4 puts it.
//!
//! **Failure semantics** follow the CLAUDE.md invariant that an unknown must
//! never be recorded as empty. A bad platform label or an unusable delivered name
//! is a usage error ([`ExportLabelError`], which the CLI maps to exit 2). A source
//! that cannot be opened, or that fails partway through, propagates the
//! [`std::io::Error`] the CLI already reads as "did not finish" (exit 3) — it is
//! never a zero, and nothing is indexed for a copy that did not complete. A
//! zero-byte source is refused rather than archived as an empty object. An object
//! the archive does not hold is an error naming the snapshots that were read,
//! never an empty success. A content address that holds something that is not the
//! bytes its name claims is an error too, whether it is caught by the size on the
//! sealing path or by the digest on the reading path.
//!
//! Privacy line, as everywhere else in this area: this module returns digests,
//! byte lengths, platform labels and the file's own name; export bytes are
//! streamed from the source into a temp file and from the repository into a
//! caller-supplied sink, and are never printed, logged or returned.

use anyhow::{anyhow, Context};
use rustic_core::repofile::{MasterKey, NodeType};
use rustic_core::LsOptions;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use crate::message_audit::{Fidelity, FidelityMetadata, MetadataSource};
use crate::store::{self, BackupStore};

/// Top-level stage namespace holding raw export objects (ADR-055 D4).
pub const EXPORT_FILES_DIR: &str = "export-files";
/// Metadata-only index of the export objects this machine has archived.
pub const EXPORT_INDEX_FILE: &str = "export-files-v1.jsonl";
/// `schema` value of every row of [`EXPORT_INDEX_FILE`].
pub const EXPORT_INDEX_SCHEMA: &str = "chat-stasher/export-file@1";

/// A platform label becomes a single path component, so it is validated before
/// any directory is created under it: `/`, `\`, `..` and a leading `.` are not
/// rejected as unparsable spellings, they are rejected because they are the
/// shapes that would let one argument write outside the namespace.
const PLATFORM_MAX_LEN: usize = 64;

/// Longest delivered file name accepted, in bytes. Every filesystem this archive
/// is built on (APFS, ext4, NTFS) tops out at 255, so this refuses at the point
/// where the name would stop being writable rather than after a failed copy.
const EXPORT_NAME_MAX_LEN: usize = 255;

/// Prefix and suffix of an in-flight copy. It is hidden on purpose: a partial
/// copy must never be mistaken for the object, and both the presence check and
/// the metahash walk skip hidden names. Because a delivered name may not begin
/// with `.`, no delivered name can collide with this shape.
const TEMP_PREFIX: &str = ".";
const TEMP_SUFFIX: &str = ".tmp";

/// Copy buffer for the hash pass and the copy pass alike. The objects are whole
/// platform takeouts, up to gigabytes, so nothing here reads one into memory.
const STREAM_BUFFER_BYTES: usize = 128 * 1024;

/// A label or a file name the caller did not supply usably.
///
/// A distinct type so the CLI can answer a usage mistake with exit 2 instead of
/// the exit 1 that a failed archive operation gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportLabelError(pub String);

impl std::fmt::Display for ExportLabelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ExportLabelError {}

/// Is `platform` safe and well-formed enough to become a path component?
pub fn validate_platform(platform: &str) -> Result<(), ExportLabelError> {
    let reject = |why: &str| {
        Err(ExportLabelError(format!(
            "platform label `{}` is not usable: {why}",
            crate::id::bounded_path_component(platform, PLATFORM_MAX_LEN)
        )))
    };
    if platform.is_empty() {
        return reject("it is empty");
    }
    if platform.len() > PLATFORM_MAX_LEN {
        return reject(&format!("it is longer than {PLATFORM_MAX_LEN} bytes"));
    }
    if !platform.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    }) {
        return reject(
            "a platform label may contain only lowercase letters, digits, `.`, `_` and `-`",
        );
    }
    if !platform.as_bytes()[0].is_ascii_alphanumeric() {
        return reject("it must start with a letter or a digit");
    }
    Ok(())
}

/// Is `name` — the name the export file was delivered under — usable as the one
/// path component D4 gives it?
///
/// This is a *refusal*, never a rewrite: the archive keeps the name the user's
/// file has, and a name that cannot be kept as itself is reported. A name is
/// refused when it would name more than one component, a directory, or a hidden
/// file, or when it is empty, unreadable as text, or longer than a filesystem
/// can hold.
pub fn validate_delivered_name(name: &str) -> Result<(), ExportLabelError> {
    let reject = |why: &str| {
        Err(ExportLabelError(format!(
            "export file name `{}` is not usable: {why}",
            crate::id::bounded_path_component(name, EXPORT_NAME_MAX_LEN)
        )))
    };
    if name.is_empty() {
        return reject("it is empty");
    }
    if name.len() > EXPORT_NAME_MAX_LEN {
        return reject(&format!("it is longer than {EXPORT_NAME_MAX_LEN} bytes"));
    }
    if name == "." || name == ".." {
        return reject("it would name a directory rather than an object");
    }
    if name.starts_with('.') {
        return reject("a hidden name would be invisible to this namespace's own reader");
    }
    if name.contains('/') || name.contains('\\') {
        return reject("it would name more than one path component");
    }
    if name
        .chars()
        .any(|c| c.is_control() || c == '\u{7f}' || c == '\u{0}')
    {
        return reject("it carries a control character");
    }
    Ok(())
}

/// The name an export file was delivered under, taken from the path the caller
/// supplied.
///
/// The file is named as the user has it — D4's "as-delivered filename" — so the
/// name comes from the source path rather than from a second argument that could
/// disagree with it.
pub fn delivered_name_of(source: &Path) -> Result<&str, ExportLabelError> {
    let Some(name) = source.file_name() else {
        return Err(ExportLabelError(format!(
            "the export file path {} names no file",
            source.display()
        )));
    };
    let Some(name) = name.to_str() else {
        return Err(ExportLabelError(format!(
            "the export file name in {} is not valid UTF-8, so the archive cannot record \
             the name it was delivered under",
            source.display()
        )));
    };
    Ok(name)
}

/// The minimum a machine name needs to stay one path component.
///
/// Other stage writers take the machine from config or the identity file and
/// trust it, and `push` checks the partitions that exist
/// ([`store::validate_stage_machines`]). This namespace has no such check behind
/// it — an index row filed outside `meta/<machine>/` would simply be invisible
/// to the metahash walk — so the name is checked here instead.
fn validate_machine_component(machine: &str) -> Result<(), ExportLabelError> {
    if machine.is_empty()
        || machine == "."
        || machine == ".."
        || machine.contains('/')
        || machine.contains('\\')
    {
        return Err(ExportLabelError(format!(
            "machine `{}` cannot name an archive partition",
            crate::id::bounded_path_component(machine, PLATFORM_MAX_LEN)
        )));
    }
    Ok(())
}

/// `<stage>/export-files`
pub fn export_files_root(stage: &Path) -> PathBuf {
    stage.join(EXPORT_FILES_DIR)
}

/// `<stage>/export-files/<platform>/<content_sha256>` — the directory holding
/// exactly one raw export object, named by the bytes it holds.
pub fn export_object_dir(stage: &Path, platform: &str, content_sha256: &str) -> PathBuf {
    export_files_root(stage).join(platform).join(content_sha256)
}

/// `<stage>/export-files/<platform>/<content_sha256>/<delivered_name>` — the
/// object itself, at D4's key.
pub fn export_object_file(
    stage: &Path,
    platform: &str,
    content_sha256: &str,
    delivered_name: &str,
) -> PathBuf {
    export_object_dir(stage, platform, content_sha256).join(delivered_name)
}

/// `<stage>/meta/<machine>/export-files-v1.jsonl`
pub fn export_index_path(stage: &Path, machine: &str) -> PathBuf {
    stage
        .join(crate::metahash::META_DIR)
        .join(machine)
        .join(EXPORT_INDEX_FILE)
}

/// One archived export object, as recorded in the index.
///
/// Metadata only: the digest and length of the bytes, never a path, never the
/// file's name, never the content. `fidelity` is ADR-055 D4's stamp for this
/// object — `raw`, a byte-identical copy of what the platform produced.
///
/// `(machine, platform, content_sha256)` is the object's whole identity: the
/// address is what a reader asks for, and the object's own name is a name in the
/// namespace rather than a fact about the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportFileRow {
    pub schema: String,
    pub machine: String,
    pub platform: String,
    pub content_sha256: String,
    pub bytes: u64,
    pub captured_at_unix: i64,
    pub fidelity: FidelityMetadata,
}

/// The three states of reading one machine's export index, kept apart for the
/// same reason [`crate::manifest::ManifestFileState`] keeps them apart: "no
/// index has ever been written here" and "an index exists that records nothing"
/// are different facts, and a reader must not turn the first into the second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportIndexState {
    Missing,
    Empty,
    Loaded(Vec<ExportFileRow>),
}

/// What one call to [`seal_export_file`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealOutcome {
    /// The namespace did not hold these bytes; it does now, under the name the
    /// file was delivered with.
    Installed {
        content_sha256: String,
        bytes: u64,
        delivered_name: String,
        path: PathBuf,
    },
    /// The namespace already held these exact bytes, so nothing was copied.
    /// `held_name` is the name the object is stored under, which is the name of
    /// the first sighting when a repeat arrived under a different one.
    AlreadyArchived {
        content_sha256: String,
        bytes: u64,
        held_name: String,
    },
}

/// Archive one raw export file byte-exact under `platform`, once.
///
/// The source is opened read-only and left exactly where it is: this copies, it
/// never moves, renames or rewrites what the user downloaded (ADR-011 — the
/// archive takes a copy and the user keeps their file).
///
/// Content-addressed, so the same bytes offered twice — from the same path or
/// from three different ones — return [`SealOutcome::AlreadyArchived`] without
/// copying or re-indexing, and two different files always get two objects.
pub fn seal_export_file(
    stage: &Path,
    machine: &str,
    platform: &str,
    source: &Path,
) -> anyhow::Result<SealOutcome> {
    validate_platform(platform)?;
    validate_machine_component(machine)?;
    let delivered_name = delivered_name_of(source)?;
    validate_delivered_name(delivered_name)?;
    store::assert_stage_writer_audited(store::StageWriter::Export)?;
    crate::test_identity_guard::refuse_fixture_write(&[platform, machine], stage)?;

    // Two passes on purpose. The first hashes the source so the second knows
    // whether it has to copy anything at all: on a repeat of a 3.5 GiB package
    // the whole cost is the hash, not a second copy of it.
    let (content_sha256, bytes) =
        hash_file(source).with_context(|| format!("read the export file {}", source.display()))?;
    if bytes == 0 {
        anyhow::bail!(
            "the export file {} holds no bytes; an empty object is not an archive record",
            source.display()
        );
    }

    let dir = export_object_dir(stage, platform, &content_sha256);
    let held = held_object(&dir)
        .with_context(|| format!("read the export object directory {}", dir.display()))?;
    if let Some(held_name) = held {
        // The object is here. Its name is the digest of its bytes, so a size
        // that disagrees with the hash pass is the archive holding something
        // other than what it claims — and a stat is free where a re-read of a
        // multi-gigabyte package is not.
        let held_path = dir.join(&held_name);
        let held_bytes = fs::metadata(&held_path)
            .with_context(|| format!("stat the archived export object {}", held_path.display()))?
            .len();
        if held_bytes != bytes {
            anyhow::bail!(
                "the export object {} holds {held_bytes} bytes but its content address \
                 {content_sha256} is the digest of {bytes} bytes; the archive's copy is not the \
                 bytes its name claims",
                held_path.display()
            );
        }
        // The row is repaired on this path too, because a crash lands between the
        // rename and the row, and a repeat of the same file is the run that
        // notices.
        ensure_index_row(stage, machine, platform, &content_sha256, bytes)?;
        return Ok(SealOutcome::AlreadyArchived {
            content_sha256,
            bytes,
            held_name,
        });
    }
    let path = install_object(&dir, delivered_name, source, &content_sha256, bytes)?;
    ensure_index_row(stage, machine, platform, &content_sha256, bytes)?;
    Ok(SealOutcome::Installed {
        content_sha256,
        bytes,
        delivered_name: delivered_name.to_string(),
        path,
    })
}

/// The name of the one object a content-address directory holds, if it holds
/// one.
///
/// Hidden entries are skipped: an interrupted copy is written to
/// `.<name>.tmp` and is not the object. One content address, one object — two
/// entries mean the namespace was written by something other than
/// [`seal_export_file`], and choosing one of them silently is exactly the kind
/// of guess this codebase refuses.
fn held_object(dir: &Path) -> anyhow::Result<Option<String>> {
    let entries = namespace_files(dir)?;
    match entries.len() {
        0 => Ok(None),
        1 => entries
            .into_iter()
            .next()
            .map(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
                    .ok_or_else(|| {
                        anyhow!(
                            "the export object {} is not named in valid UTF-8",
                            path.display()
                        )
                    })
            })
            .transpose(),
        n => Err(anyhow!(
            "the export object directory {} holds {n} objects; a content address holds exactly one",
            dir.display()
        )),
    }
}

/// Every non-hidden file under one content-address directory, sorted.
///
/// A missing directory is an empty namespace, not an error. A non-hidden entry
/// that is not a file is an error: this namespace is built by
/// [`seal_export_file`] and nothing else, so anything else in it is a fact the
/// caller has to see rather than skip.
fn namespace_files(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    let mut files = Vec::new();
    let mut unexpected = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if entry.file_type()?.is_file() {
            files.push(entry.path());
        } else {
            unexpected.push(name);
        }
    }
    if !unexpected.is_empty() {
        anyhow::bail!(
            "the export object directory {} holds entries that are not objects: {}",
            dir.display(),
            unexpected.join(", ")
        );
    }
    files.sort();
    Ok(files)
}

/// Stream `source` into a fresh object at `<dir>/<delivered_name>`.
///
/// The copy is hashed as it is written and compared against the digest the
/// directory is already named for, so "byte-exact" is a measured property of the
/// installed file rather than an intention: a source that changed between the
/// hash pass and the copy, or a read that stopped early, fails here and leaves
/// nothing installed and nothing indexed.
fn install_object(
    dir: &Path,
    delivered_name: &str,
    source: &Path,
    content_sha256: &str,
    expected_bytes: u64,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(dir)
        .with_context(|| format!("create the export object directory {}", dir.display()))?;
    let path = dir.join(delivered_name);
    let tmp = dir.join(format!("{TEMP_PREFIX}{delivered_name}{TEMP_SUFFIX}"));
    if path.exists() {
        return Err(anyhow!(
            "the export object {} already exists; refusing to overwrite a sealed object",
            path.display()
        ));
    }
    if tmp.exists() {
        // Only an interrupted copy can leave a temp here, and this directory's
        // name is the digest of the bytes it holds, so what is being discarded
        // was written for this very object.
        #[allow(
            clippy::let_underscore_must_use,
            reason = "Clearing an interrupted copy is best-effort; the copy that follows either succeeds or reports the failure the caller needs."
        )]
        let _ = fs::remove_file(&tmp);
    }

    let copied = write_copy(source, &tmp);
    let (written, digest) = match copied {
        Ok(done) => done,
        Err(error) => {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "Deleting the partial copy is best-effort cleanup; the copy error is the failure that must reach the caller."
            )]
            let _ = fs::remove_file(&tmp);
            return Err(error).with_context(|| {
                format!("copy the export file {} into the stage", source.display())
            });
        }
    };
    if digest != content_sha256 || written != expected_bytes {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "Deleting the rejected copy is best-effort cleanup; the mismatch below is the failure the caller needs."
        )]
        let _ = fs::remove_file(&tmp);
        return Err(anyhow!(
            "the export file {} changed while it was being copied: the copy reads as {written} \
             bytes with sha256 {digest}, the address being sealed holds {expected_bytes} bytes as \
             {content_sha256}; nothing was archived",
            source.display()
        ));
    }
    fs::rename(&tmp, &path)
        .inspect_err(|_error| {
            // The partial copy must not sit in the namespace: it has no name the
            // reader recognises, but leaving it there makes the directory a place
            // where an interrupted write lives on. Best-effort either way — the
            // rename failure is what the caller has to see.
            #[allow(
                clippy::let_underscore_must_use,
                reason = "Removing the temp after a failed rename is best-effort; the rename error is the failure the caller needs."
            )]
            let _ = fs::remove_file(&tmp);
        })
        .with_context(|| format!("install the export object {}", path.display()))?;
    // ADR-025's discipline, applied to a whole-file copy: prove the rename is
    // durable before the caller is told the object is archived. Windows keeps
    // its existing behaviour without a directory fsync.
    #[cfg(unix)]
    fs::File::open(dir)
        .and_then(|dir| dir.sync_all())
        .with_context(|| {
            format!(
                "prove the export object durable (fsync dir {})",
                dir.display()
            )
        })?;
    Ok(path)
}

/// Copy `source` into `tmp`, hashing every byte that passes, and sync it.
fn write_copy(source: &Path, tmp: &Path) -> io::Result<(u64, String)> {
    let mut read = io::BufReader::with_capacity(STREAM_BUFFER_BYTES, fs::File::open(source)?);
    let mut file = fs::File::create(tmp)?;
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    loop {
        let buffer = read.fill_buf()?;
        if buffer.is_empty() {
            break;
        }
        let taken = buffer.len();
        file.write_all(&buffer[..taken])?;
        hasher.update(&buffer[..taken]);
        written += taken as u64;
        read.consume(taken);
    }
    file.flush()?;
    file.sync_all()?;
    Ok((written, hex_digest(&hasher.finalize())))
}

/// Hash a file by streaming it, returning `(sha256, bytes)`.
pub fn hash_file(path: &Path) -> io::Result<(String, u64)> {
    let mut read = io::BufReader::with_capacity(STREAM_BUFFER_BYTES, fs::File::open(path)?);
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    loop {
        let buffer = read.fill_buf()?;
        if buffer.is_empty() {
            break;
        }
        let taken = buffer.len();
        hasher.update(buffer);
        bytes += taken as u64;
        read.consume(taken);
    }
    Ok((hex_digest(&hasher.finalize()), bytes))
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_content_address(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Append this machine's index row for one archived object, unless it already
/// records that object.
///
/// Called *after* the bytes are installed, which is the only ordering that keeps
/// the index honest: a crash between the rename and this append leaves an object
/// with no row, and the next run finds the object present and writes the row it
/// was missing. The reverse — a row with no object — would be a claim about the
/// archive that is not true.
fn ensure_index_row(
    stage: &Path,
    machine: &str,
    platform: &str,
    content_sha256: &str,
    bytes: u64,
) -> anyhow::Result<()> {
    let rows = match read_export_index(stage, machine)? {
        ExportIndexState::Missing | ExportIndexState::Empty => Vec::new(),
        ExportIndexState::Loaded(rows) => rows,
    };
    if rows
        .iter()
        .any(|row| row.platform == platform && row.content_sha256 == content_sha256)
    {
        return Ok(());
    }
    let path = export_index_path(stage, machine);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let row = ExportFileRow {
        schema: EXPORT_INDEX_SCHEMA.to_string(),
        machine: machine.to_string(),
        platform: platform.to_string(),
        content_sha256: content_sha256.to_string(),
        bytes,
        captured_at_unix: chrono::Utc::now().timestamp(),
        fidelity: FidelityMetadata {
            source: MetadataSource::Captured,
            fidelity: Fidelity::Raw,
        },
    };
    let line = serde_json::to_string(&row).context("serialize the export index row")?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("append to {}", path.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("append to {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", path.display()))?;
    Ok(())
}

/// Read one machine's export index.
///
/// A line that does not parse, a row naming another machine, or a content
/// address that is not a lowercase sha256 is a hard error: an index that is
/// silently skipped would make an archived object look unarchived, which is the
/// same mistake as recording an unknown as empty.
pub fn read_export_index(stage: &Path, machine: &str) -> anyhow::Result<ExportIndexState> {
    let path = export_index_path(stage, machine);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ExportIndexState::Missing)
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut rows = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: ExportFileRow = serde_json::from_str(line)
            .with_context(|| format!("parse {} line {}", path.display(), index + 1))?;
        if row.schema != EXPORT_INDEX_SCHEMA {
            return Err(anyhow!(
                "{} line {}: unknown export index schema `{}`",
                path.display(),
                index + 1,
                row.schema
            ));
        }
        if row.machine != machine {
            return Err(anyhow!(
                "{} line {}: the row names machine `{}` but is stored inside the `{machine}` directory",
                path.display(),
                index + 1,
                row.machine
            ));
        }
        if !is_content_address(&row.content_sha256) {
            return Err(anyhow!(
                "{} line {}: `{}` is not a lowercase sha256 content address",
                path.display(),
                index + 1,
                row.content_sha256
            ));
        }
        if row.bytes == 0 {
            return Err(anyhow!(
                "{} line {}: an archived export object cannot be zero bytes",
                path.display(),
                index + 1
            ));
        }
        if row.fidelity.fidelity != Fidelity::Raw {
            return Err(anyhow!(
                "{} line {}: a raw export object's fidelity is `raw`, not {:?}",
                path.display(),
                index + 1,
                row.fidelity.fidelity
            ));
        }
        rows.push(row);
    }
    if rows.is_empty() {
        Ok(ExportIndexState::Empty)
    } else {
        Ok(ExportIndexState::Loaded(rows))
    }
}

/// Bucket an archived file path into `(platform, content_sha256,
/// delivered_name)` when its trailing components are D4's
/// `export-files/<platform>/<key>/<delivered filename>`.
///
/// Trailing components only, exactly like [`crate::readback::bucket_shard_path`]:
/// the archived tree mirrors each machine's *absolute* stage path minus the
/// leading `/`, so the stage prefix differs per machine and cannot be matched.
/// The `sessions` marker is deliberately not consulted, which is why a session
/// readback never sees an export object and this never sees a session. A leaf
/// that is hidden, or a key that is not a lowercase sha256, is not an object this
/// namespace writes.
pub fn export_object_path(path: &Path) -> Option<(String, String, String)> {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    let marker = comps
        .iter()
        .rposition(|component| *component == EXPORT_FILES_DIR)?;
    let rest = &comps[marker + 1..];
    if rest.len() != 3 {
        return None;
    }
    let (platform, key, delivered_name) = (rest[0], rest[1], rest[2]);
    if validate_platform(platform).is_err() || !is_content_address(key) {
        return None;
    }
    if delivered_name.is_empty() || delivered_name.starts_with('.') {
        return None;
    }
    Some((
        platform.to_string(),
        key.to_string(),
        delivered_name.to_string(),
    ))
}

/// What the archive holds for one export object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedExport {
    pub machine: String,
    pub platform: String,
    pub content_sha256: String,
    pub bytes: u64,
    /// The name the object is stored under — the name the file was delivered
    /// with, on the first machine that archived these bytes.
    pub delivered_name: String,
    /// Short id of the snapshot the verified copy was read from.
    pub snapshot: String,
}

/// Read one export object back out of the archive into `sink`, proving it.
///
/// Cumulative like every other "what do I have archived" reader
/// ([`crate::readback`]): the newest snapshot of `machine` that holds the object
/// answers, and every snapshot of that machine is walked to find it. Newest-wins
/// is trivially safe here because a directory's name *is* the digest of its
/// content, so any copy of one address is the same bytes by construction — which
/// is also what makes the comparison below a corruption check rather than a
/// version dispute.
///
/// The object's own name is returned rather than required, because the name a
/// repeat was delivered under is not part of the object's identity: the address
/// is. The bytes are streamed into `sink` while being hashed, and the digest is
/// compared with the address that was asked for, so `Ok` means the archive holds
/// these exact bytes. On `Err` the sink may hold a prefix of a partially read
/// object and the caller must discard it: a truncated copy that was never
/// verified is not a read.
///
/// Passing [`std::io::sink`] reads and verifies without keeping the bytes, which
/// is what a proof needs; passing a file is what hands the object to a reader
/// that wants it. Absence is an error naming how many snapshots were read, never
/// an empty success.
pub fn read_archived_export<W: Write>(
    archive: &BackupStore,
    mk: &MasterKey,
    machine: &str,
    platform: &str,
    content_sha256: &str,
    sink: &mut W,
) -> anyhow::Result<ArchivedExport> {
    validate_platform(platform)?;
    if !is_content_address(content_sha256) {
        return Err(anyhow!(
            "`{content_sha256}` is not a lowercase sha256 content address"
        ));
    }
    let (repo, _adoption) = archive
        .open_indexed(mk)
        .context("open repository for export readback")?;
    archive.require_sound_packs(&repo)?;
    let snaps = repo.get_all_snapshots().context("list snapshots")?;
    let Some((_, host_snaps)) = crate::readback::snapshots_by_host_newest_first(snaps)
        .into_iter()
        .find(|(host, _)| host == machine)
    else {
        return Err(anyhow!(
            "no snapshot for machine `{machine}` in this repository"
        ));
    };
    let snapshots_held = host_snaps.len();

    for snap in &host_snaps {
        let snap_id = snap.id.to_hex().as_str().to_string();
        let short = &snap_id[..8.min(snap_id.len())];
        let root = repo
            .node_from_snapshot_and_path(snap, "")
            .with_context(|| format!("snapshot {short}: cannot read tree root"))?;
        let entries = repo
            .ls(&root, &LsOptions::default())
            .and_then(|iter| iter.collect::<rustic_core::RusticResult<Vec<_>>>())
            .with_context(|| format!("snapshot {short}: cannot list tree"))?;
        for (path, node) in entries {
            if node.node_type != NodeType::File {
                continue;
            }
            let Some((found_platform, found_key, found_name)) = export_object_path(&path) else {
                continue;
            };
            if found_platform != platform || found_key != content_sha256 {
                continue;
            }
            let mut writer = HashingWriter {
                inner: sink,
                hasher: Sha256::new(),
                bytes: 0,
            };
            repo.dump(&node, &mut writer).with_context(|| {
                format!("snapshot {short}: cannot read the archived export object {content_sha256}")
            })?;
            let digest = hex_digest(&writer.hasher.clone().finalize());
            let bytes = writer.bytes;
            if digest != content_sha256 {
                return Err(anyhow!(
                    "the archived export object for platform `{platform}` reads back as {bytes} \
                     bytes with sha256 {digest}, but its content address is {content_sha256}: the \
                     archive's copy is not the bytes its name claims"
                ));
            }
            return Ok(ArchivedExport {
                machine: machine.to_string(),
                platform: platform.to_string(),
                content_sha256: content_sha256.to_string(),
                bytes,
                delivered_name: found_name,
                snapshot: short.to_string(),
            });
        }
    }
    Err(anyhow!(
        "the export object {content_sha256} for platform `{platform}` is not archived by machine \
         `{machine}` in any of its {snapshots_held} snapshots (all of them were read)"
    ))
}

/// A `Write` that counts and hashes what passes through it.
struct HashingWriter<'a, W: Write> {
    inner: &'a mut W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> Write for HashingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SHARD_SUFFIX;

    const KEY: &str = "2d5b825b1b182de959f33f5dddc31ee2196e91eb3b6801650c38d2145cad23a9";

    #[test]
    fn a_platform_label_becomes_exactly_one_path_component() {
        for ok in ["chatgpt", "deepseek", "claude-code", "x.ai", "g1_2"] {
            assert!(validate_platform(ok).is_ok(), "`{ok}` must be usable");
        }
        for bad in [
            "",
            "..",
            ".",
            "../elsewhere",
            "a/b",
            "a\\b",
            "ChatGPT",
            "space case",
            "\u{7d20}",
            "nul\0",
            "-leading",
            &"x".repeat(PLATFORM_MAX_LEN + 1),
        ] {
            let error = validate_platform(bad).expect_err("the label must be refused");
            assert!(error.0.contains("platform label"), "{error}");
        }
    }

    #[test]
    fn a_delivered_name_stays_one_readable_component_or_is_refused() {
        for ok in [
            "OpenAI-export.zip",
            "conversations.json",
            "2025-01-05_2026-07-01.conversations.json",
            "MyActivity (1).json",
            "chatgpt-dsar 2026-10-03.tar.gz",
        ] {
            assert!(
                validate_delivered_name(ok).is_ok(),
                "`{ok}` is a name the user's file can have"
            );
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden-export.zip",
            "a/b.zip",
            "a\\b.zip",
            "nul\0.zip",
            "two\nlines.json",
            "tab\tname.json",
            "del\u{7f}name.json",
            "back\\slash.zip",
            &"x".repeat(EXPORT_NAME_MAX_LEN + 1),
        ] {
            let error = validate_delivered_name(bad).expect_err("the name must be refused");
            assert!(error.0.contains("export file name"), "{error}");
        }
    }

    #[test]
    fn the_matcher_reads_the_namespace_and_nothing_else() {
        let stage = Path::new("/Users/p/scratch/stage");
        let object = export_object_file(stage, "deepseek", KEY, "OpenAI-export.zip");
        assert_eq!(
            object,
            stage
                .join(EXPORT_FILES_DIR)
                .join("deepseek")
                .join(KEY)
                .join("OpenAI-export.zip"),
            "the object sits at the key D4 states"
        );
        assert_eq!(
            export_object_path(&object),
            Some((
                "deepseek".to_string(),
                KEY.to_string(),
                "OpenAI-export.zip".to_string()
            ))
        );
        // A stage prefix the archive kept, as `readback` sees it.
        let prefixed = Path::new("/Users/p/scratch/stage").join(&object);
        assert!(export_object_path(&prefixed).is_some());

        // A session shard, an index row, a hidden leaf, a bucketed path, a
        // directory key that is not a digest and a platform that is not a label
        // are none of them export objects.
        let session = stage
            .join(store::SESSIONS_DIR)
            .join("m")
            .join("chatgpt.6a4b")
            .join("000")
            .join(format!("000001{SHARD_SUFFIX}"));
        assert_eq!(export_object_path(&session), None);
        let index = stage
            .join(crate::metahash::META_DIR)
            .join("m")
            .join(EXPORT_INDEX_FILE);
        assert_eq!(export_object_path(&index), None);
        assert_eq!(
            export_object_path(
                &export_object_dir(stage, "deepseek", KEY).join(".OpenAI-export.zip.tmp")
            ),
            None,
            "an interrupted copy is not an object"
        );
        assert_eq!(
            export_object_path(
                &export_object_dir(stage, "deepseek", KEY)
                    .join("000")
                    .join("OpenAI-export.zip")
            ),
            None
        );
        assert_eq!(
            export_object_path(&export_object_dir(stage, "deepseek", "not-a-digest").join("x.zip")),
            None
        );
        assert_eq!(
            export_object_path(&export_object_dir(stage, "../escape", KEY).join("x.zip")),
            None
        );
    }

    #[test]
    fn sealing_distinguishes_a_source_it_cannot_read_from_a_name_it_cannot_keep() {
        let temp = tempfile::tempdir().expect("temp root");
        let stage = temp.path().join("stage");
        let missing = stage.join("nowhere.jsonl");
        let error = seal_export_file(&stage, "synthetic-machine", "deepseek", &missing)
            .expect_err("an absent source is a failure, never a zero");
        assert!(
            error.downcast_ref::<io::Error>().is_some(),
            "a source that cannot be opened must stay an io::Error so the CLI reads \
             it as did-not-finish: {error}"
        );
        let error = seal_export_file(&stage, "synthetic-machine", "../escape", &missing)
            .expect_err("a traversal label must be refused");
        assert!(
            error.downcast_ref::<ExportLabelError>().is_some(),
            "{error}"
        );
        // A name that cannot be kept is refused before anything is read, and the
        // refusal says which name — it never rewrites the file into one it likes.
        let hidden = temp.path().join(".deepseek-export.bin");
        let error = seal_export_file(&stage, "synthetic-machine", "deepseek", &hidden)
            .expect_err("a hidden name must be refused");
        assert!(
            error.downcast_ref::<ExportLabelError>().is_some(),
            "{error}"
        );
        assert!(!stage.join(EXPORT_FILES_DIR).exists());
    }

    #[test]
    fn a_zero_byte_export_is_refused_not_archived_as_empty() {
        let temp = tempfile::tempdir().expect("temp root");
        let stage = temp.path().join("stage");
        let empty = temp.path().join("empty.tar");
        fs::write(&empty, b"").expect("write the empty source");
        let error = seal_export_file(&stage, "synthetic-machine", "deepseek", &empty)
            .expect_err("an empty object is not an archive record");
        assert!(error.to_string().contains("no bytes"), "{error}");
        assert!(!stage.join(EXPORT_FILES_DIR).exists());
        assert!(matches!(
            read_export_index(&stage, "synthetic-machine").unwrap(),
            ExportIndexState::Missing
        ));
    }

    #[test]
    fn a_corrupt_index_row_is_an_error_not_a_skipped_line() {
        let temp = tempfile::tempdir().expect("temp root");
        let stage = temp.path().join("stage");
        let path = export_index_path(&stage, "synthetic-machine");
        fs::create_dir_all(path.parent().unwrap()).expect("meta dir");
        fs::write(&path, "{ not json }\n").expect("write the corrupt row");
        assert!(read_export_index(&stage, "synthetic-machine").is_err());
        fs::write(
            &path,
            r#"{"schema":"chat-stasher/export-file@1","machine":"other-machine","platform":"deepseek","content_sha256":"a","bytes":1,"captured_at_unix":1,"fidelity":{"source":"captured","value":"raw"}}"#,
        )
        .expect("write the foreign row");
        let error = read_export_index(&stage, "synthetic-machine")
            .expect_err("a row filed under the wrong machine must be refused");
        assert!(error.to_string().contains("other-machine"), "{error}");
    }
}
