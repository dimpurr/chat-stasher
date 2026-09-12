//! ADR-022: content-addressed hash tracking for metadata sidecars (`meta/<machine>/`).
//!
//! Tracks changes to files in `<stage>/meta/<machine>/` (such as `machine.json`,
//! `manifest-v1.jsonl`, `activity-v1.jsonl`, and `label-by-*.json`) so that changes
//! to metadata trigger a snapshot push even when no new conversation shards were
//! written.
//!
//! Invariants (ADR-022):
//! - Compares file *contents*, never mtimes: `activity-v1.jsonl` is rewritten
//!   periodically, but if its content has not changed, it must not cause hourly
//!   superfluous pushes.
//! - Missing or unreadable record is treated as changed: failing closed towards
//!   creating a snapshot rather than silently dropping metadata.
//! - Record is updated *only* after a push succeeds; failed pushes never advance it.

use anyhow::Context;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Directory under stage containing machine sidecar metadata.
pub const META_DIR: &str = "meta";

/// File prefix for the pushed meta hash record under state dir.
pub const PUSHED_META_PREFIX: &str = "pushed-meta-";

/// Path to the recorded meta hash for `machine` inside `state_dir`.
pub fn pushed_meta_record_path(state_dir: &Path, machine: &str) -> PathBuf {
    let digest = crate::runstate::machine_digest(machine);
    state_dir.join(format!("{PUSHED_META_PREFIX}{digest}.sha256"))
}

/// Count regular files under `<stage>/meta/<machine>/`, ignoring hidden/temp files.
pub fn count_meta_files(stage: &Path, machine: &str) -> anyhow::Result<usize> {
    let meta_dir = stage.join(META_DIR).join(machine);
    let entries = match fs::read_dir(&meta_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("read {}", meta_dir.display())),
    };
    let mut count = 0;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        count += 1;
    }
    Ok(count)
}

/// Check if `<stage>/meta/<machine>/` has any non-hidden regular files.
pub fn has_meta_files(stage: &Path, machine: &str) -> anyhow::Result<bool> {
    Ok(count_meta_files(stage, machine)? > 0)
}

/// Compute a deterministic SHA-256 digest over the names and contents of all
/// non-hidden regular files in `<stage>/meta/<machine>/`.
///
/// Returns `Ok(None)` if `<stage>/meta/<machine>/` does not exist or contains no files.
pub fn compute_meta_hash(stage: &Path, machine: &str) -> anyhow::Result<Option<String>> {
    let meta_dir = stage.join(META_DIR).join(machine);
    let entries = match fs::read_dir(&meta_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", meta_dir.display())),
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        files.push((name, entry.path()));
    }

    if files.is_empty() {
        return Ok(None);
    }

    // Sort by file name for deterministic hashing across platforms / filesystems.
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (name, path) in files {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        let content =
            fs::read(&path).with_context(|| format!("read meta file {}", path.display()))?;
        let content_hash = Sha256::digest(&content);
        hasher.update(content_hash);
    }

    let digest = hasher.finalize();
    Ok(Some(hex_digest(&digest)))
}

/// Result of reading the recorded pushed meta hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushedMetaRead {
    Missing,
    Unreadable(String),
    Present(String),
}

/// Load the recorded pushed meta hash from `<state_dir>/pushed-meta-<digest>.sha256`.
pub fn load_pushed_meta_hash(state_dir: &Path, machine: &str) -> PushedMetaRead {
    let path = pushed_meta_record_path(state_dir, machine);
    match fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => PushedMetaRead::Missing,
        Err(e) => PushedMetaRead::Unreadable(e.to_string()),
        Ok(s) => {
            let trimmed = s.trim();
            if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
                PushedMetaRead::Present(trimmed.to_string())
            } else {
                PushedMetaRead::Unreadable("invalid hex sha256 in record".to_string())
            }
        }
    }
}

/// Check if `<stage>/meta/<machine>/` has changed since the last successful push.
///
/// Rules (ADR-022):
/// - No meta exists now and no record was ever written -> `false`
/// - No meta exists now but a record exists -> `true` (meta was removed)
/// - Meta exists now and record is missing/unreadable -> `true` (safe direction)
/// - Meta exists now and record exists -> `current != record`
pub fn check_meta_changed(stage: &Path, machine: &str, state_dir: &Path) -> anyhow::Result<bool> {
    let current = compute_meta_hash(stage, machine)?;
    let read = load_pushed_meta_hash(state_dir, machine);
    match (current, read) {
        (None, PushedMetaRead::Missing) => Ok(false),
        (None, PushedMetaRead::Present(_)) => Ok(true),
        (Some(_), PushedMetaRead::Missing) => Ok(true),
        (_, PushedMetaRead::Unreadable(_)) => Ok(true),
        (Some(curr), PushedMetaRead::Present(prev)) => Ok(curr != prev),
    }
}

/// Helper for `run-once` to evaluate whether content is present and whether
/// changes occurred in collected shards or metadata sidecars.
///
/// Returns `(has_content, changed)`.
///
/// A metadata directory that cannot be read is an error, not "no metadata":
/// treating it as empty would let a shard-less stage be skipped silently while
/// its declaration or retained summaries never reach the archive. The caller
/// reports it the same way as a failed shard count.
pub fn evaluate_run_once_change(
    stage: &Path,
    machine: &str,
    state_dir: &Path,
    stage_shards: usize,
    shards_changed: bool,
) -> anyhow::Result<(bool, bool)> {
    let has_meta = has_meta_files(stage, machine)?;
    let meta_changed = check_meta_changed(stage, machine, state_dir)?;
    let has_content = stage_shards > 0 || has_meta;
    let changed = shards_changed || meta_changed;
    Ok((has_content, changed))
}

/// Record `hash` as the metadata state that the last successful push carried.
///
/// The hash must be computed *before* the push, not re-read afterwards: if the
/// metadata changes between the backup and this call (a `machine-declare`
/// landing mid-push), re-reading would record content that was never pushed,
/// and that change would then never be pushed at all.
pub fn record_pushed_meta_hash(
    state_dir: &Path,
    machine: &str,
    hash: Option<&str>,
) -> anyhow::Result<()> {
    fs::create_dir_all(state_dir)
        .with_context(|| format!("create state dir {}", state_dir.display()))?;
    let path = pushed_meta_record_path(state_dir, machine);
    if let Some(hash) = hash {
        let tmp = path.with_extension("sha256.tmp");
        let mut file = fs::File::create(&tmp)
            .with_context(|| format!("create temp file {}", tmp.display()))?;
        writeln!(file, "{hash}").with_context(|| format!("write to {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp.display()))?;
        drop(file);
        fs::rename(&tmp, &path)
            .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))?;
    } else if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("remove stale pushed meta record {}", path.display()))?;
    }
    Ok(())
}

/// Persist the current meta hash into `<state_dir>/pushed-meta-<digest>.sha256`.
///
/// Must be called only after a push successfully completes.
/// Uses atomic write (temp file + sync + rename) to guard against partial writes.
pub fn save_pushed_meta_hash(stage: &Path, machine: &str, state_dir: &Path) -> anyhow::Result<()> {
    let current_hash = compute_meta_hash(stage, machine)?;
    record_pushed_meta_hash(state_dir, machine, current_hash.as_deref())
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn empty_stage_has_no_meta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        assert!(!has_meta_files(&stage, "m1").expect("has_meta"));
        assert_eq!(count_meta_files(&stage, "m1").expect("count"), 0);
        assert_eq!(compute_meta_hash(&stage, "m1").expect("hash"), None);
    }

    #[test]
    fn content_change_changes_hash_but_mtime_does_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let meta_dir = stage.join("meta").join("m1");
        fs::create_dir_all(&meta_dir).expect("create_dir");

        let activity = meta_dir.join("activity-v1.jsonl");
        fs::write(&activity, "line 1\n").expect("write");

        let hash1 = compute_meta_hash(&stage, "m1")
            .expect("hash1")
            .expect("some");

        // Touch mtime two days ago
        let old_time = SystemTime::now() - Duration::from_secs(2 * 86400);
        fs::OpenOptions::new()
            .write(true)
            .open(&activity)
            .expect("open")
            .set_modified(old_time)
            .expect("set_modified");

        let hash2 = compute_meta_hash(&stage, "m1")
            .expect("hash2")
            .expect("some");
        assert_eq!(hash1, hash2, "mtime change must not affect content hash");

        // Change content
        fs::write(&activity, "line 1\nline 2\n").expect("write");
        let hash3 = compute_meta_hash(&stage, "m1")
            .expect("hash3")
            .expect("some");
        assert_ne!(hash1, hash3, "content change must change content hash");
    }

    #[test]
    fn check_meta_changed_lifecycle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let state_dir = dir.path().join("state");
        let machine = "mac-test";

        // 1. Stage has no meta, no record -> no change
        assert!(!check_meta_changed(&stage, machine, &state_dir).expect("check"));

        // 2. Add machine.json -> missing record => changed
        let meta_dir = stage.join("meta").join(machine);
        fs::create_dir_all(&meta_dir).expect("create_dir");
        fs::write(meta_dir.join("machine.json"), "{\"name\":\"test\"}").expect("write");
        assert!(check_meta_changed(&stage, machine, &state_dir).expect("check"));

        // 3. Save hash (simulating push success) -> now unchanged
        save_pushed_meta_hash(&stage, machine, &state_dir).expect("save");
        assert!(!check_meta_changed(&stage, machine, &state_dir).expect("check"));

        // 4. Touch file / rewrite same content -> still unchanged
        fs::write(meta_dir.join("machine.json"), "{\"name\":\"test\"}").expect("write");
        assert!(!check_meta_changed(&stage, machine, &state_dir).expect("check"));

        // 5. Corrupt record file -> unreadable => changed (fails safe)
        let record_path = pushed_meta_record_path(&state_dir, machine);
        fs::write(&record_path, "corrupt content").expect("write");
        assert!(check_meta_changed(&stage, machine, &state_dir).expect("check"));

        // 6. Resave valid record -> unchanged again
        save_pushed_meta_hash(&stage, machine, &state_dir).expect("save");
        assert!(!check_meta_changed(&stage, machine, &state_dir).expect("check"));

        // 7. Change machine.json -> changed
        fs::write(meta_dir.join("machine.json"), "{\"name\":\"modified\"}").expect("write");
        assert!(check_meta_changed(&stage, machine, &state_dir).expect("check"));
    }
}
