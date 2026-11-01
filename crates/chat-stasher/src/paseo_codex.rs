//! Codex identity compatibility for Paseo agent handles (ADR-056 D2/D3, PASEO-1c).
//!
//! A Paseo agent record carries the Codex session UUID it was started with.
//! That UUID *proves which rollout file the agent refers to*; it is not the
//! archive identity. The archive identity of a Codex session has always been
//! the rollout **filename stem** (`rollout-<timestamp>-<uuid>`), built through
//! [`crate::id::SessionIdentity`] by the scanner. Enabling Paseo must not mint a
//! second identity for a file that is already archived, so this module does two
//! things and no third:
//!
//! 1. **Locate** the rollout whose filename ends in the handle UUID, in the
//!    active `sessions/` root and in the sibling `archived_sessions/` root
//!    (plain `.jsonl` or compressed `.jsonl.zst`), without following symlinks
//!    and without mutating anything.
//! 2. **Validate** that the file's own first record (`session_meta`) states the
//!    same UUID, then hand back the *stem* as the identity.
//!
//! The UUID is never used to build an id. [`archive_session_id`] takes the
//! stem, so the id a file gets here is, by construction, the id the scanner
//! already gives it.
//!
//! The outcome is one of four states that mirror the distinct
//! [`crate::paseo::BodyState`] statuses: one proven candidate, no candidate,
//! a candidate that could not be read, or evidence that does not choose
//! (several candidates, or a file whose internal identity disagrees with its
//! name). "Not found", "could not read" and "cannot tell which" are never
//! folded into one another (CLAUDE.md invariant 1).

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::paseo::BodyState;

/// Longest first record accepted when validating a rollout's internal identity.
/// A Codex `session_meta` record carries instructions text and can be large;
/// anything beyond this is treated as unreadable instead of buffered.
const FIRST_RECORD_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Directory depth below a root that is walked. Codex writes
/// `YYYY/MM/DD/rollout-*.jsonl`; the bound leaves room without letting a
/// pathological tree make the walk unbounded.
const WALK_MAX_DEPTH: usize = 8;

/// Directory entries visited across both roots before the walk gives up and
/// reports the source unreadable rather than guessing.
const WALK_MAX_ENTRIES: usize = 500_000;

/// Which root a candidate was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexRoot {
    /// `<codex home>/sessions`
    Active,
    /// `<codex home>/archived_sessions`, the sibling root `thread/archive`
    /// moves rollouts into.
    Archived,
}

/// One rollout file whose name and internal identity both name the handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRollout {
    /// The rollout filename stem. This is the native id the archive identity is
    /// built from; it is unchanged by an archive move or by compression.
    pub stem: String,
    pub path: PathBuf,
    pub root: CodexRoot,
    /// `true` for `.jsonl.zst`.
    pub compressed: bool,
}

/// What looking for a handle's rollout concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexResolution {
    /// Exactly one candidate, and its internal identity agrees with its name.
    Resolved(CodexRollout),
    /// Both roots were readable (or absent) and no file carries the handle.
    Missing,
    /// A candidate or a directory could not be read. Not `Missing`.
    Unreadable { reason: String },
    /// The evidence does not choose: several candidate files, or a candidate
    /// whose internal identity disagrees with its filename. Never resolved by
    /// picking one.
    Ambiguous { reason: String },
}

impl CodexResolution {
    /// The body state to record when the rollout is *not* a verified archived
    /// body. `Resolved` returns `None`: a located file is not yet an archived
    /// body, and only a producer that has verified sealed shards may build
    /// [`BodyState::Archived`] (ADR-056 D4).
    pub fn unproved_body_state(&self) -> Option<BodyState> {
        match self {
            CodexResolution::Resolved(_) => None,
            CodexResolution::Missing => Some(BodyState::Missing),
            CodexResolution::Unreadable { reason } => Some(BodyState::Unreadable {
                reason: reason.clone(),
            }),
            CodexResolution::Ambiguous { reason } => Some(BodyState::Ambiguous {
                reason: reason.clone(),
            }),
        }
    }
}

/// The canonical archive id for a rollout stem, exactly as the scanner builds
/// it for the same file.
pub fn archive_session_id(machine: &str, stem: &str) -> String {
    crate::id::SessionIdentity {
        source_short: crate::id::SOURCE_CODEX,
        machine: machine.to_string(),
        native_id: stem.to_string(),
    }
    .id()
}

/// A complete lowercase UUID, the only spelling Codex writes into a rollout
/// name and `session_meta`. A prefix or an uppercase spelling is refused so a
/// shortened handle can never be matched to the wrong file.
pub fn is_handle_uuid(value: &str) -> bool {
    let chars: Vec<char> = value.chars().collect();
    chars.len() == 36
        && chars.iter().enumerate().all(|(index, c)| match index {
            8 | 13 | 18 | 23 => *c == '-',
            _ => c.is_ascii_digit() || ('a'..='f').contains(c),
        })
}

/// Find and validate the rollout for `handle` under `codex_home`.
///
/// `codex_home` is the directory holding `sessions/` and `archived_sessions/`.
/// An invalid handle is an error (the caller recorded something that is not a
/// Codex UUID); every filesystem outcome is a [`CodexResolution`].
pub fn resolve_rollout(codex_home: &Path, handle: &str) -> anyhow::Result<CodexResolution> {
    anyhow::ensure!(
        is_handle_uuid(handle),
        "codex handle is not a complete lowercase UUID"
    );
    let mut candidates: Vec<(PathBuf, CodexRoot, String, bool)> = Vec::new();
    let mut budget = WALK_MAX_ENTRIES;
    for (dir, root) in [
        (codex_home.join("sessions"), CodexRoot::Active),
        (codex_home.join("archived_sessions"), CodexRoot::Archived),
    ] {
        if let Err(reason) = walk(&dir, 0, handle, root, &mut budget, &mut candidates) {
            return Ok(CodexResolution::Unreadable { reason });
        }
    }
    match candidates.len() {
        0 => Ok(CodexResolution::Missing),
        1 => {
            let (path, root, stem, compressed) = candidates.remove(0);
            Ok(validate_candidate(path, root, stem, compressed, handle))
        }
        _ => Ok(CodexResolution::Ambiguous {
            reason: format!(
                "{} candidate rollouts carry this handle and the evidence does not choose between them",
                candidates.len()
            ),
        }),
    }
}

fn validate_candidate(
    path: PathBuf,
    root: CodexRoot,
    stem: String,
    compressed: bool,
    handle: &str,
) -> CodexResolution {
    match internal_session_id(&path, compressed) {
        Ok(Some(internal)) if internal == handle => CodexResolution::Resolved(CodexRollout {
            stem,
            path,
            root,
            compressed,
        }),
        Ok(Some(_)) => CodexResolution::Ambiguous {
            reason: "the rollout's internal session id disagrees with its filename".to_string(),
        },
        Ok(None) => CodexResolution::Unreadable {
            reason: "the rollout's first record does not state a session id".to_string(),
        },
        Err(reason) => CodexResolution::Unreadable { reason },
    }
}

/// Walk one root. A missing root is not an error (the archive root may never
/// have been created); any other read failure is.
fn walk(
    dir: &Path,
    depth: usize,
    handle: &str,
    root: CodexRoot,
    budget: &mut usize,
    out: &mut Vec<(PathBuf, CodexRoot, String, bool)>,
) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("a rollout directory could not be read".to_string()),
    };
    for entry in entries {
        let entry = entry.map_err(|_| "a rollout directory could not be read".to_string())?;
        if *budget == 0 {
            return Err("the rollout tree exceeds the walk bound".to_string());
        }
        *budget -= 1;
        // `file_type` on a DirEntry does not follow symlinks, so a link is
        // neither a file nor a directory here and is never entered or read.
        let kind = entry
            .file_type()
            .map_err(|_| "a rollout entry could not be inspected".to_string())?;
        let path = entry.path();
        if kind.is_dir() {
            if depth < WALK_MAX_DEPTH {
                walk(&path, depth + 1, handle, root, budget, out)?;
            }
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if let Some((stem, compressed)) = rollout_stem(&name, handle) {
            out.push((path, root, stem, compressed));
        }
    }
    Ok(())
}

/// `(stem, compressed)` when `name` is a rollout file for `handle`.
fn rollout_stem(name: &str, handle: &str) -> Option<(String, bool)> {
    let (stem, compressed) = if let Some(stem) = name.strip_suffix(".jsonl.zst") {
        (stem, true)
    } else {
        (name.strip_suffix(".jsonl")?, false)
    };
    let suffix = format!("-{handle}");
    (stem.starts_with("rollout-") && stem.ends_with(&suffix))
        .then(|| (stem.to_string(), compressed))
}

/// The `payload.id` of the file's first record, if it is a `session_meta`.
fn internal_session_id(path: &Path, compressed: bool) -> Result<Option<String>, String> {
    let file = fs::File::open(path).map_err(|_| "a rollout could not be opened".to_string())?;
    let reader: Box<dyn Read> = if compressed {
        Box::new(
            zstd::stream::read::Decoder::new(file)
                .map_err(|_| "a compressed rollout could not be decoded".to_string())?,
        )
    } else {
        Box::new(file)
    };
    let mut first = Vec::new();
    let mut limited = BufReader::new(reader.take(FIRST_RECORD_MAX_BYTES + 1));
    limited
        .read_until(b'\n', &mut first)
        .map_err(|_| "a rollout could not be read".to_string())?;
    if first.len() as u64 > FIRST_RECORD_MAX_BYTES {
        return Err("a rollout's first record exceeds the read bound".to_string());
    }
    let record: serde_json::Value = serde_json::from_slice(&first)
        .map_err(|_| "a rollout's first record is not JSON".to_string())?;
    if record.get("type").and_then(serde_json::Value::as_str) != Some("session_meta") {
        return Ok(None);
    }
    Ok(record
        .get("payload")
        .and_then(|payload| payload.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string))
}
