//! Local harness scanner — the meat of this spike.
//!
//! Scans two roots for session files:
//!
//! | harness      | root                  | extension        |
//! |--------------|-----------------------|------------------|
//! | Claude Code  | `~/.claude/projects`  | `*.jsonl`        |
//! | Codex        | `~/.codex/sessions`   | `*.jsonl`        |
//! | Codex (idle) | same                  | `*.jsonl.zst`    |
//!
//! Hard guarantees:
//!   * read-only — nothing is opened for writing and file bodies are never
//!     read, so no session content can ever leak into a log or report,
//!   * `.jsonl.zst` files are *recognised and listed* (compressed flag set);
//!     decompression is a later spike but silently skipping them is not an
//!     option,
//!   * missing roots are reported, not fatal.

use crate::config::Config;
use crate::id::{SessionIdentity, SOURCE_CODE_DIR, SOURCE_CODEX};
use crate::models::{HarnessSource, SessionRecord};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        crate::config::home_dir().join(rest)
    } else if p == "~" {
        crate::config::home_dir()
    } else {
        PathBuf::from(p)
    }
}

/// Roots that a scan walked, and which of them were missing.
#[derive(Debug, Default)]
pub struct ScanReport {
    pub records: Vec<SessionRecord>,
    /// Roots (as configured) that did not exist on this machine.
    pub missing_roots: Vec<PathBuf>,
}

/// Walk every configured harness root and collect metadata-only records.
///
/// Missing roots are recorded in the report (so `status` can tell the user
/// "Codex has no sessions yet") and skipped; symlinked directories are not
/// chased, which keeps the walk inside the two harness trees.
pub fn scan(config: &Config) -> io::Result<ScanReport> {
    let claude_root = config
        .claude_projects_dir
        .as_deref()
        .map(expand_tilde)
        .unwrap_or_else(|| crate::config::home_dir().join(".claude").join("projects"));
    let codex_root = config
        .codex_sessions_dir
        .as_deref()
        .map(expand_tilde)
        .unwrap_or_else(|| crate::config::home_dir().join(".codex").join("sessions"));

    let machine = crate::id::machine_id();

    let mut report = ScanReport::default();
    scan_root(&claude_root, HarnessSource::ClaudeCode, &machine, &mut report)?;
    scan_root(&codex_root, HarnessSource::Codex, &machine, &mut report)?;
    Ok(report)
}

fn scan_root(
    root: &Path,
    source: HarnessSource,
    machine: &str,
    report: &mut ScanReport,
) -> io::Result<()> {
    if !root.is_dir() {
        report.missing_roots.push(root.to_path_buf());
        return Ok(());
    }

    // Iterative (stack) walk instead of recursion so deeply nested harness
    // trees can't blow the stack.
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue, // ignore unreadable subdirs, keep scanning
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue; // skip sockets, FIFOs, and (crucially) symlinks
            }
            if let Some(rec) = build_record(&path, source, machine) {
                report.records.push(rec);
            }
        }
    }
    Ok(())
}

/// Strip `*.jsonl.zst` / `*.jsonl` suffixes off a filename.
///
/// Returns `(stem, compressed)`. `*.jsonl.zst` must be matched *before*
/// `*.jsonl` or the zst files would be mangled into `.jsonl` stems.
fn strip_session_suffix(name: &str) -> (String, bool) {
    if let Some(base) = name.strip_suffix(".jsonl.zst") {
        (base.to_string(), true)
    } else if let Some(base) = name.strip_suffix(".jsonl") {
        (base.to_string(), false)
    } else {
        (name.to_string(), false)
    }
}

/// Detect the harness from a path, by matching the `.claude` / `.codex`
/// directory segments. Component-based, so it works regardless of `/` vs `\`.
fn detect_source(path: &Path) -> Option<HarnessSource> {
    use std::path::Component;
    for comp in path.components() {
        if let Component::Normal(name) = comp {
            let name = name.to_string_lossy();
            if name == ".codex" {
                return Some(HarnessSource::Codex);
            }
            if name == ".claude" {
                return Some(HarnessSource::ClaudeCode);
            }
        }
    }
    None
}

/// Turn one discovered path into a `SessionRecord`, or `None` if the file's
/// name is not a session file at all.
fn build_record(
    path: &Path,
    expected: HarnessSource,
    machine: &str,
) -> Option<SessionRecord> {
    let name = path.file_name()?.to_string_lossy();
    if !(name.ends_with(".jsonl") || name.ends_with(".jsonl.zst")) {
        return None;
    }
    let (stem, compressed) = strip_session_suffix(&name);
    if stem.is_empty() {
        return None;
    }

    let source = detect_source(path).unwrap_or(expected);
    let source_short = match source {
        HarnessSource::Codex => SOURCE_CODEX,
        HarnessSource::ClaudeCode => SOURCE_CODE_DIR,
    };

    let meta = fs::metadata(path).ok()?;
    // Cache both numeric stat results in one call to avoid a second stat.
    let byte_size = meta.len();
    let mtime: SystemTime = meta.modified().ok()?;

    let ident = SessionIdentity {
        source_short,
        machine: machine.to_string(),
        native_id: stem,
    };

    Some(SessionRecord {
        id: ident.id(),
        absolute_path: absolutize(path),
        byte_size,
        mtime,
        source,
        compressed,
    })
}

/// Make a path absolute without requiring it to exist (lexically join with
/// cwd). Paths from `read_dir` are already absolute when the root was, but
/// this is cheap insurance for handycrafted config roots.
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_stripping_matches_zst_before_jsonl() {
        assert_eq!(
            strip_session_suffix("019bf00d-97b6-7eb2-9bf8-eacbacc09765.jsonl"),
            ("019bf00d-97b6-7eb2-9bf8-eacbacc09765".to_string(), false)
        );
        assert_eq!(
            strip_session_suffix("019bf00d-97b6-7eb2-9bf8-eacbacc09765.jsonl.zst"),
            ("019bf00d-97b6-7eb2-9bf8-eacbacc09765".to_string(), true)
        );
        assert_eq!(
            strip_session_suffix("notes.txt"),
            ("notes.txt".to_string(), false)
        );
    }

    #[test]
    fn source_detection_via_directory_segment() {
        let codex = Path::new("/Users/u/.codex/sessions/2026-05-01/ab.jsonl");
        let claude = Path::new("/Users/u/.claude/projects/foo-abc/ab.jsonl");
        assert_eq!(detect_source(&codex), Some(HarnessSource::Codex));
        assert_eq!(detect_source(&claude), Some(HarnessSource::ClaudeCode));
    }

    #[test]
    fn id_uses_dot_separator() {
        let ident = SessionIdentity {
            source_short: SOURCE_CODEX,
            machine: "mbp".into(),
            native_id: "019bf00d-97b6-7eb2-9bf8-eacbacc09765".into(),
        };
        assert_eq!(ident.id(), "codex.mbp.019bf00d-97b6-7eb2-9bf8-eacbacc09765");
    }
}