//! ADR-022: content-addressed hash tracking for stage metadata and extension status reports.
//!
//! Tracks changes to files in `<stage>/meta/<machine>/` (such as `machine.json`,
//! `manifest-v1.jsonl`, `activity-v1.jsonl`, and `label-by-*.json`), plus
//! `<stage>/ext-status/`, so metadata and per-install status changes trigger a
//! snapshot push even when no new conversation shards were written.
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

/// Count machine metadata and extension status files, ignoring hidden/temp files.
pub fn count_meta_files(stage: &Path, machine: &str) -> anyhow::Result<usize> {
    Ok(stage_metadata_files(stage, machine)?.len())
}

/// Check if this machine has any pushable metadata or extension status files.
pub fn has_meta_files(stage: &Path, machine: &str) -> anyhow::Result<bool> {
    Ok(count_meta_files(stage, machine)? > 0)
}

fn stage_metadata_files(stage: &Path, machine: &str) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    for (namespace, dir) in [
        ("meta", stage.join(META_DIR).join(machine)),
        ("ext-status", stage.join("ext-status")),
    ] {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
        };
        for entry in entries {
            let entry = entry?;
            let kind = entry.file_type()?;
            // EXT-13 · Status records are keyed by `(machine, install_id)`, so
            // the machine is a directory. Descending exactly one level here is
            // what keeps "the status alone is content, and changing it triggers
            // a push" true of the new layout — a walker that only looked at
            // files would silently stop seeing every report and the stage would
            // never push again. The digest keeps the relative key, so the two
            // layouts hash differently and the first push after the migration
            // says "changed", as it should.
            if kind.is_dir() {
                if namespace != "ext-status" {
                    continue;
                }
                let sub = entry.file_name().to_string_lossy().into_owned();
                if sub.starts_with('.') {
                    continue;
                }
                for inner in fs::read_dir(entry.path())? {
                    let inner = inner?;
                    if !inner.file_type()?.is_file() {
                        continue;
                    }
                    let name = inner.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') || !name.ends_with(".json") {
                        continue;
                    }
                    files.push((format!("{namespace}/{sub}/{name}"), inner.path()));
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || (namespace == "ext-status" && !name.ends_with(".json")) {
                continue;
            }
            files.push((format!("{namespace}/{name}"), entry.path()));
        }
    }
    Ok(files)
}

/// Compute a deterministic SHA-256 digest over the names and contents of all
/// non-hidden metadata files for this machine, including `ext-status/*.json`.
///
/// Returns `Ok(None)` if neither metadata directory contains a file.
pub fn compute_meta_hash(stage: &Path, machine: &str) -> anyhow::Result<Option<String>> {
    let mut files = stage_metadata_files(stage, machine)?;

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

    #[test]
    fn extension_status_alone_is_content_and_changes_trigger_push() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let state_dir = dir.path().join("state");
        let machine = "mac-test";
        let status_dir = stage.join("ext-status");
        fs::create_dir_all(&status_dir).expect("create status dir");

        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false).expect("empty"),
            (false, false)
        );
        let status = status_dir.join("install.json");
        fs::write(&status, r#"{"reported_at":"2026-09-27T00:00:00Z"}"#).expect("write");
        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false).expect("new status"),
            (true, true)
        );

        save_pushed_meta_hash(&stage, machine, &state_dir).expect("record successful push");
        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false)
                .expect("unchanged status"),
            (true, false)
        );

        fs::write(&status, r#"{"reported_at":"2026-09-27T01:00:00Z"}"#).expect("update");
        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false)
                .expect("changed status"),
            (true, true)
        );
    }

    /// EXT-13 · The keyed layout puts the machine between the directory and the
    /// file, so a walker that only looked at files would stop seeing every
    /// status record — and `has_meta_files` would answer "nothing to push" while
    /// reports piled up. The property above is asserted on the flat layout
    /// alone, which is exactly why it could not catch that; this pins it on the
    /// shape the host now writes.
    #[test]
    fn keyed_extension_status_is_content_and_changes_trigger_push() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let state_dir = dir.path().join("state");
        let machine = "mac-test";
        let status_dir = stage.join("ext-status").join(machine);
        fs::create_dir_all(&status_dir).expect("create status dir");

        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false).expect("empty"),
            (false, false),
            "an empty stage has nothing to push"
        );
        let status = status_dir.join("install-123.json");
        fs::write(&status, r#"{"reported_at":"2026-09-27T00:00:00Z"}"#).expect("write");
        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false).expect("new status"),
            (true, true),
            "a keyed status record alone is content"
        );

        save_pushed_meta_hash(&stage, machine, &state_dir).expect("record successful push");
        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false)
                .expect("unchanged status"),
            (true, false)
        );

        fs::write(&status, r#"{"reported_at":"2026-09-27T01:00:00Z"}"#).expect("update");
        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false)
                .expect("changed status"),
            (true, true)
        );
    }

    /// The migration keeps the pre-migration file, so both layouts can hold a
    /// record for one install at once. They are two files with two relative
    /// keys, and the digest has to see both — a walker that picked one shape
    /// would make the other's changes invisible to the push decision.
    #[test]
    fn both_status_layouts_are_counted_towards_one_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let flat = stage.join("ext-status").join("install-123.json");
        fs::create_dir_all(flat.parent().expect("dir")).expect("create");
        fs::write(&flat, r#"{"reported_at":"2026-09-27T00:00:00Z"}"#).expect("write flat");
        let only_flat = compute_meta_hash(&stage, "mac-test")
            .expect("hash")
            .expect("some");

        let keyed = stage
            .join("ext-status")
            .join("mac-test")
            .join("install-123.json");
        fs::create_dir_all(keyed.parent().expect("dir")).expect("create keyed dir");
        fs::write(&keyed, r#"{"reported_at":"2026-09-27T00:00:00Z"}"#).expect("write keyed");
        assert_eq!(
            count_meta_files(&stage, "mac-test").expect("count"),
            2,
            "both layouts are seen"
        );
        let both = compute_meta_hash(&stage, "mac-test")
            .expect("hash")
            .expect("some");
        assert_ne!(
            only_flat, both,
            "adding a keyed record changes the digest, so the next push is not skipped"
        );
    }

    // ---------------------------------------------------------------------
    // ADR-022's fail-closed half. The tests above pin the happy path and one
    // corrupt-record arm; nothing yet pinned the invariant the module header
    // states — "missing or unreadable record is treated as changed: failing
    // closed towards creating a snapshot rather than silently dropping
    // metadata" — nor what the stage side has to do for that promise to hold.
    // ---------------------------------------------------------------------

    /// The record read has three states and the fail-closed rule is written
    /// against all three. A read that answered `Missing` for a file it could
    /// not open, or `Present` for one it could not parse, would make the
    /// unreadable arm unreachable and the invariant untested rather than
    /// satisfied — which is why the three are pinned apart here.
    #[test]
    fn unreadable_record_is_a_state_of_its_own() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state_dir = dir.path().join("state");
        fs::create_dir_all(&state_dir).expect("create state dir");
        let machine = "mac-test";
        let record = pushed_meta_record_path(&state_dir, machine);

        assert_eq!(
            load_pushed_meta_hash(&state_dir, machine),
            PushedMetaRead::Missing,
            "no record at all is Missing, not an error and not an empty digest"
        );

        // Bytes that are not UTF-8 cannot be a hex digest. `read_to_string`
        // refuses them, and that refusal is a state of its own — reporting it
        // as Missing would claim the machine has never pushed.
        fs::write(&record, [0xff, 0xfe, 0xfd, 0x80]).expect("write invalid utf-8");
        let not_utf8 = load_pushed_meta_hash(&state_dir, machine);
        assert!(
            matches!(not_utf8, PushedMetaRead::Unreadable(_)),
            "a record that is not valid UTF-8 is Unreadable, got {not_utf8:?}"
        );

        // Readable, and still not a digest: wrong alphabet, and separately
        // wrong length. Both are Unreadable rather than a `Present` string
        // that could never equal a computed hash.
        for (label, bytes) in [
            ("not hex", "corrupt content".to_string()),
            ("63 hex chars", "a".repeat(63)),
            ("65 hex chars", "a".repeat(65)),
            (
                "a non-hex digit in hex length",
                format!("{}z", "a".repeat(63)),
            ),
        ] {
            fs::write(&record, &bytes).expect("write");
            assert_eq!(
                load_pushed_meta_hash(&state_dir, machine),
                PushedMetaRead::Unreadable("invalid hex sha256 in record".to_string()),
                "a record that is {label} is Unreadable"
            );
        }

        // A record that cannot be opened at all. A directory where the record
        // belongs fails the read on every platform, so this arm needs no
        // permission trick and runs everywhere.
        fs::remove_file(&record).expect("remove file record");
        fs::create_dir(&record).expect("record path is a directory");
        let unopenable = load_pushed_meta_hash(&state_dir, machine);
        assert!(
            matches!(unopenable, PushedMetaRead::Unreadable(_)),
            "a record that cannot be read is Unreadable, got {unopenable:?}"
        );
        fs::remove_dir(&record).expect("remove record directory");

        // And the one state that is allowed to say "unchanged": a digest, with
        // the newline the record is written with trimmed off.
        let hash = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";
        fs::write(&record, format!("{hash}\n")).expect("write digest");
        assert_eq!(
            load_pushed_meta_hash(&state_dir, machine),
            PushedMetaRead::Present(hash.to_string()),
            "a 64-char hex record with a trailing newline is Present"
        );
    }

    /// The record is keyed by machine, and two machines share one stage
    /// directory. A record that answered for both would make machine B's
    /// metadata look already-pushed the moment machine A pushed.
    #[test]
    fn a_record_answers_only_for_its_own_machine() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let state_dir = dir.path().join("state");
        // Two machines, one stage, and byte-identical metadata on each.
        for machine in ["mac-a", "mac-b"] {
            let meta_dir = stage.join(META_DIR).join(machine);
            fs::create_dir_all(&meta_dir).expect("create_dir");
            fs::write(
                meta_dir.join("machine.json"),
                r#"{"machine_id":"mac-test"}"#,
            )
            .expect("write");
        }
        let hash = compute_meta_hash(&stage, "mac-a")
            .expect("hash")
            .expect("some");
        assert_eq!(
            compute_meta_hash(&stage, "mac-b")
                .expect("hash")
                .expect("some"),
            hash,
            "identical metadata under the two machines hashes identically, so only \
             the record can keep them apart"
        );

        assert_ne!(
            pushed_meta_record_path(&state_dir, "mac-a"),
            pushed_meta_record_path(&state_dir, "mac-b"),
            "the record path carries the machine digest, so the two cannot collide"
        );
        record_pushed_meta_hash(&state_dir, "mac-a", Some(&hash)).expect("record a");
        assert_eq!(
            load_pushed_meta_hash(&state_dir, "mac-a"),
            PushedMetaRead::Present(hash.clone())
        );
        assert_eq!(
            load_pushed_meta_hash(&state_dir, "mac-b"),
            PushedMetaRead::Missing,
            "machine b has never pushed, so it has no record to answer with"
        );
        assert!(
            check_meta_changed(&stage, "mac-b", &state_dir).expect("mac b"),
            "machine a's record must not make machine b's identical metadata look pushed"
        );
    }

    /// The change decision, as a table. Only a `Present` record that matches
    /// what is on disk may answer "unchanged": `Missing`, `Unreadable`, and a
    /// differing digest all mean the same thing to the push, which is "push".
    /// The existing lifecycle test walks the metadata-present arms; the two
    /// arms where metadata is *absent* are the ones a fail-closed regression
    /// would take silently, because both of them pair with a record state that
    /// reads as benign.
    #[test]
    fn only_a_matching_record_answers_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let state_dir = dir.path().join("state");
        let machine = "mac-test";
        let meta_dir = stage.join(META_DIR).join(machine);
        fs::create_dir_all(&meta_dir).expect("create_dir");
        let machine_json = meta_dir.join("machine.json");
        let record = pushed_meta_record_path(&state_dir, machine);

        // No metadata and no record: there is nothing to push, and answering
        // "changed" here would push an empty stage on every run forever.
        assert!(!check_meta_changed(&stage, machine, &state_dir).expect("empty"));

        // Metadata arrives with no record: changed, because we cannot prove it
        // was ever pushed.
        fs::write(&machine_json, r#"{"name":"test"}"#).expect("write");
        assert!(check_meta_changed(&stage, machine, &state_dir).expect("no record"));
        let hash = compute_meta_hash(&stage, machine)
            .expect("hash")
            .expect("some");

        // The matching record: the only unchanged answer there is.
        record_pushed_meta_hash(&state_dir, machine, Some(&hash)).expect("record");
        assert!(!check_meta_changed(&stage, machine, &state_dir).expect("match"));

        // A different digest: changed.
        record_pushed_meta_hash(&state_dir, machine, Some(&"b".repeat(64))).expect("record");
        assert!(check_meta_changed(&stage, machine, &state_dir).expect("other digest"));

        // Back to a match, then a record that parses as nothing: changed.
        record_pushed_meta_hash(&state_dir, machine, Some(&hash)).expect("record");
        fs::write(&record, "not a digest").expect("corrupt");
        assert_eq!(
            load_pushed_meta_hash(&state_dir, machine),
            PushedMetaRead::Unreadable("invalid hex sha256 in record".to_string())
        );
        assert!(check_meta_changed(&stage, machine, &state_dir).expect("unreadable"));

        // A record that cannot even be opened: still changed, not Missing.
        fs::remove_file(&record).expect("remove");
        fs::create_dir(&record).expect("mkdir");
        assert!(check_meta_changed(&stage, machine, &state_dir).expect("unopenable"));
        fs::remove_dir(&record).expect("rmdir");
        assert!(
            check_meta_changed(&stage, machine, &state_dir).expect("record gone"),
            "a deleted record is Missing, which fails closed exactly the same way"
        );

        // Metadata gone, record still standing: changed, because a previous
        // push carried something that is no longer there.
        fs::remove_file(&machine_json).expect("remove meta");
        record_pushed_meta_hash(&state_dir, machine, Some(&hash)).expect("record");
        assert!(check_meta_changed(&stage, machine, &state_dir).expect("meta removed"));

        // Metadata gone and nothing recorded: unchanged, the one pair where
        // "nothing to push" is a fact rather than an absence of knowledge.
        fs::remove_file(&record).expect("remove record");
        assert!(!check_meta_changed(&stage, machine, &state_dir).expect("empty"));
    }

    /// The same fail-closed answer at the level `run-once` actually reads, and
    /// with the arms that decide *whether there is a stage at all*: an
    /// unreadable record with metadata present is content and a change, never
    /// the "(false, false)" that means "nothing here, stay quiet".
    #[test]
    fn an_unreadable_record_is_content_and_a_change_for_run_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let state_dir = dir.path().join("state");
        let machine = "mac-test";
        let meta_dir = stage.join(META_DIR).join(machine);
        fs::create_dir_all(&meta_dir).expect("create_dir");
        fs::write(meta_dir.join("machine.json"), r#"{"name":"test"}"#).expect("write");
        let record = pushed_meta_record_path(&state_dir, machine);
        let hash = compute_meta_hash(&stage, machine)
            .expect("hash")
            .expect("some");

        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false).expect("no record"),
            (true, true),
            "metadata with no record is content and a change"
        );

        record_pushed_meta_hash(&state_dir, machine, Some(&hash)).expect("record");
        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false).expect("match"),
            (true, false),
            "a matching record is the only quiet answer"
        );

        fs::write(&record, "not a digest").expect("corrupt");
        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false).expect("unreadable"),
            (true, true),
            "a record we cannot read must not make run-once skip the stage"
        );

        fs::remove_file(&record).expect("remove");
        fs::create_dir(&record).expect("mkdir");
        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false).expect("unopenable"),
            (true, true),
            "a record we cannot open must not make run-once skip the stage"
        );
        fs::remove_dir(&record).expect("rmdir");
    }

    /// ADR-022 fails closed on the record side; the stage side has the same
    /// shape and was just as untested. `compute_meta_hash` answers `Ok(None)`
    /// when the listing found nothing, and the decision reads `(None, Missing)`
    /// as "unchanged" — so a file the listing returned and then could not read
    /// has to be an `Err`, never `Ok(None)`. `Ok(None)` there would report "we
    /// hold no metadata" for a stage that does, and the push would be skipped.
    ///
    /// Unix-only: the fault is injected by `chmod`-ing the file to 0o000, a
    /// POSIX permission feature Windows has no standard-API equivalent of. The
    /// property is not left unasserted there — the decision table above and
    /// `a_metadata_path_that_cannot_be_listed_is_not_an_empty_stage` below pin
    /// it from the arms every platform can express.
    #[test]
    #[cfg(unix)]
    fn metadata_that_becomes_unreadable_is_an_error_not_absent_metadata() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let state_dir = dir.path().join("state");
        let machine = "mac-test";
        let meta_dir = stage.join(META_DIR).join(machine);
        fs::create_dir_all(&meta_dir).expect("create_dir");
        let activity = meta_dir.join("activity-v1.jsonl");
        fs::write(&activity, "{\"captured\":1}\n").expect("write");

        // Readable: the listing returns it and the digest is over its content.
        assert!(compute_meta_hash(&stage, machine)
            .expect("readable")
            .is_some());

        fs::set_permissions(&activity, fs::Permissions::from_mode(0o000)).expect("lock");
        if fs::read(&activity).is_ok() {
            // Permissions cannot express unreadability here (root), so the case
            // is not reachable on this machine. Say so instead of passing
            // quietly, the way a lock helper that assumes it worked would.
            eprintln!("metahash: sandbox cannot make a file unreadable (root?), case skipped");
            fs::set_permissions(&activity, fs::Permissions::from_mode(0o600)).expect("unlock");
            return;
        }

        // Still listed — only the read fails.
        assert_eq!(
            count_meta_files(&stage, machine).expect("count"),
            1,
            "the file is still in the listing; what changed is that it cannot be read"
        );
        let hashed = compute_meta_hash(&stage, machine);
        let changed = check_meta_changed(&stage, machine, &state_dir);
        let evaluated = evaluate_run_once_change(&stage, machine, &state_dir, 0, false);
        fs::set_permissions(&activity, fs::Permissions::from_mode(0o600)).expect("unlock");

        let message = match hashed {
            Err(e) => format!("{e:#}"),
            Ok(None) => panic!("metadata we hold but cannot read must be an Err, not Ok(None)"),
            Ok(Some(hash)) => panic!("metadata we cannot read must not hash as content: {hash}"),
        };
        assert!(
            message.contains("activity-v1.jsonl"),
            "the error has to name the file it could not read: {message}"
        );
        assert!(
            changed.is_err(),
            "a stage whose metadata cannot be read is an error, not \"unchanged\""
        );
        assert!(
            evaluated.is_err(),
            "run-once must not report a quiet stage when metadata could not be read"
        );
    }

    /// The portable arm of that property, and the arm a user can hit without
    /// anyone's permissions being wrong: a metadata path that cannot be listed
    /// is an error, not an empty stage. `stage/meta/<machine>` being a file is
    /// how to say that with no fault injection at all, so it runs on every
    /// platform — and the consequence is asserted at the level `run-once`
    /// reads, because `(false, false)` there is exactly "skip this stage".
    #[test]
    fn a_metadata_path_that_cannot_be_listed_is_not_an_empty_stage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let state_dir = dir.path().join("state");
        let machine = "mac-test";
        fs::create_dir_all(stage.join(META_DIR)).expect("create meta root");
        fs::write(
            stage.join(META_DIR).join(machine),
            "this path is a file, not the machine's metadata directory",
        )
        .expect("write");

        assert!(
            count_meta_files(&stage, machine).is_err(),
            "a metadata path we cannot list is an error, not zero files"
        );
        assert!(
            has_meta_files(&stage, machine).is_err(),
            "\"nothing to push\" is not the answer for a path we never read"
        );
        assert!(
            compute_meta_hash(&stage, machine).is_err(),
            "a metadata path we cannot list must not hash as \"no metadata\""
        );
        assert!(
            check_meta_changed(&stage, machine, &state_dir).is_err(),
            "an unlistable metadata path is an error, not \"unchanged\""
        );
        assert!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false).is_err(),
            "run-once must not report (false, false) for a stage it could not read"
        );
    }

    /// Hidden files, hidden directories and non-`.json` status files are not
    /// metadata. A `.tmp` left by an interrupted write, a `.DS_Store`, or a
    /// half-written editor swap file must not turn into a push every run, and
    /// the cheapest way to prove they are invisible is to hash a stage with
    /// them against a stage without them.
    ///
    /// The comparison is one digest apart from a weaker count assertion on
    /// purpose: a name that is skipped for the count but still read for the
    /// digest would push, and the count would stay green.
    #[test]
    fn hidden_and_non_json_files_are_skipped_in_both_namespaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let machine = "mac-test";

        let clean = dir.path().join("clean");
        let clean_meta = clean.join(META_DIR).join(machine);
        fs::create_dir_all(&clean_meta).expect("create meta dir");
        fs::write(clean_meta.join("machine.json"), r#"{"machine_id":"x"}"#).expect("write");
        fs::write(clean_meta.join("activity-v1.jsonl"), "{\"captured\":1}\n").expect("write");
        let clean_status = clean.join("ext-status").join(machine);
        fs::create_dir_all(&clean_status).expect("create status dir");
        fs::write(
            clean_status.join("install-1.json"),
            r#"{"reported_at":"2026-09-27T00:00:00Z"}"#,
        )
        .expect("write");
        let clean_hash = compute_meta_hash(&clean, machine)
            .expect("clean hash")
            .expect("some");
        assert_eq!(
            count_meta_files(&clean, machine).expect("count"),
            3,
            "three files are really metadata, and both meta sidecars count"
        );

        // The same stage, plus everything the filters are supposed to drop.
        let noisy = dir.path().join("noisy");
        let noisy_meta = noisy.join(META_DIR).join(machine);
        fs::create_dir_all(noisy_meta.join(".hidden")).expect("create hidden dir");
        fs::create_dir_all(noisy_meta.join("nested")).expect("create nested dir");
        fs::write(noisy_meta.join("machine.json"), r#"{"machine_id":"x"}"#).expect("write");
        fs::write(noisy_meta.join("activity-v1.jsonl"), "{\"captured\":1}\n").expect("write");
        fs::write(noisy_meta.join(".activity-v1.jsonl.swp"), "swap").expect("write");
        fs::write(noisy_meta.join(".DS_Store"), "\0\0").expect("write");
        fs::write(noisy_meta.join(".hidden").join("machine.json"), "{}").expect("write");
        fs::write(noisy_meta.join("nested").join("machine.json"), "{}").expect("write");
        let noisy_status = noisy.join("ext-status").join(machine);
        fs::create_dir_all(noisy_status.join(".hidden-dir")).expect("create hidden dir");
        fs::create_dir_all(noisy.join("ext-status").join(".hidden-top")).expect("create hidden");
        fs::write(
            noisy_status.join("install-1.json"),
            r#"{"reported_at":"2026-09-27T00:00:00Z"}"#,
        )
        .expect("write");
        fs::write(noisy_status.join("notes.txt"), "not a status report").expect("write");
        fs::write(noisy_status.join(".install-2.json"), "{}").expect("write");
        fs::write(noisy_status.join(".DS_Store"), "\0\0").expect("write");
        fs::write(
            noisy_status.join(".hidden-dir").join("install-3.json"),
            "{}",
        )
        .expect("write");
        fs::write(
            noisy
                .join("ext-status")
                .join(".hidden-top")
                .join("install-4.json"),
            "{}",
        )
        .expect("write");
        fs::write(
            noisy.join("ext-status").join("notes.txt"),
            "not a status report",
        )
        .expect("write");

        assert_eq!(
            count_meta_files(&noisy, machine).expect("count"),
            3,
            "hidden entries, nested directories and non-.json files are not metadata"
        );
        assert_eq!(
            compute_meta_hash(&noisy, machine)
                .expect("noisy hash")
                .expect("some"),
            clean_hash,
            "skipped files must not reach the digest, or every editor swap file becomes a push"
        );

        // The extension filter belongs to `ext-status` alone, and it has to.
        // `activity-v1.jsonl` and `manifest-v1.jsonl` are metadata that must
        // trigger a push and neither ends in `.json`, so a blanket "only .json"
        // rule over both namespaces would drop exactly those changes.
        fs::write(
            clean_meta.join("manifest-v1.jsonl"),
            "{\"session\":\"synthetic\"}\n",
        )
        .expect("write");
        assert_eq!(
            count_meta_files(&clean, machine).expect("count"),
            4,
            "a .jsonl sidecar in meta/ is metadata, so the .json filter cannot apply there"
        );
        assert_ne!(
            compute_meta_hash(&clean, machine)
                .expect("hash")
                .expect("some"),
            clean_hash,
            "and it changes the digest, which is the point of tracking it"
        );
    }

    /// EXT-13's `(machine, install_id)` keying is why the walker descends one
    /// level and no more, and takes `.json` only. Exactly one level is the
    /// whole shape: a second level is not a status record the host writes, and
    /// a status record the walker cannot see is a report that never reaches
    /// the archive. Pinned on the digest rather than on a count, because the
    /// count is not what decides whether a push happens.
    #[test]
    fn ext_status_is_walked_exactly_one_level_deep_and_only_takes_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let machine = "mac-test";
        let status_dir = stage.join("ext-status").join(machine);
        fs::create_dir_all(status_dir.join("nested").join("deeper")).expect("create");
        fs::write(
            status_dir.join("install-1.json"),
            r#"{"reported_at":"2026-09-27T00:00:00Z"}"#,
        )
        .expect("write keyed");
        fs::write(
            stage.join("ext-status").join("install-flat.json"),
            r#"{"reported_at":"2026-09-27T00:00:00Z"}"#,
        )
        .expect("write flat");

        assert_eq!(
            count_meta_files(&stage, machine).expect("count"),
            2,
            "one keyed record and the legacy flat record, and nothing else"
        );
        let baseline = compute_meta_hash(&stage, machine)
            .expect("baseline")
            .expect("some");

        // Everything a deeper or non-.json walk would pick up, plus everything
        // it must not.
        fs::write(status_dir.join("notes.txt"), "not a report").expect("write");
        fs::write(status_dir.join("install-1.json.tmp"), "{}").expect("write");
        fs::write(status_dir.join("nested").join("install-2.json"), "{}").expect("write");
        fs::write(
            status_dir
                .join("nested")
                .join("deeper")
                .join("install-3.json"),
            "{}",
        )
        .expect("write");
        fs::write(status_dir.join("nested").join("notes.txt"), "nope").expect("write");
        fs::write(stage.join("ext-status").join("notes.txt"), "not a report").expect("write");
        assert_eq!(
            compute_meta_hash(&stage, machine)
                .expect("hash")
                .expect("some"),
            baseline,
            "one level of .json only: nothing added above reaches the digest"
        );

        // The counted file, changed and then renamed: both must move the digest,
        // the rename because the relative key is part of what is hashed.
        fs::write(
            status_dir.join("install-1.json"),
            r#"{"reported_at":"2026-09-27T01:00:00Z"}"#,
        )
        .expect("update");
        let changed = compute_meta_hash(&stage, machine)
            .expect("changed")
            .expect("some");
        assert_ne!(
            changed, baseline,
            "a status record's content is the push signal"
        );
        fs::rename(
            status_dir.join("install-1.json"),
            status_dir.join("install-2.json"),
        )
        .expect("rename");
        assert_ne!(
            compute_meta_hash(&stage, machine)
                .expect("renamed")
                .expect("some"),
            changed,
            "the relative key is hashed, so a record that moved is a different record"
        );
    }

    /// ADR-022's first invariant: the digest is over contents, never mtimes.
    /// `activity-v1.jsonl` is rewritten periodically by the collector, so an
    /// mtime in the digest would mean an hourly push forever; and the reverse
    /// matters just as much, because a digest that ignores content would stop
    /// pushing at all.
    #[test]
    fn the_digest_is_content_but_not_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let machine = "mac-test";
        let status_dir = stage.join("ext-status").join(machine);
        fs::create_dir_all(&status_dir).expect("create status dir");
        let report = status_dir.join("install-1.json");
        let body = r#"{"reported_at":"2026-09-27T00:00:00Z","pending":1}"#;
        fs::write(&report, body).expect("write");

        let first = compute_meta_hash(&stage, machine)
            .expect("hash")
            .expect("some");

        // A much older mtime, and a fresh one, on identical contents.
        let old = SystemTime::now() - Duration::from_secs(90 * 86400);
        fs::OpenOptions::new()
            .write(true)
            .open(&report)
            .expect("open")
            .set_modified(old)
            .expect("set_modified");
        assert_eq!(
            compute_meta_hash(&stage, machine)
                .expect("hash")
                .expect("some"),
            first,
            "an mtime two years ago must not change the digest"
        );
        fs::write(&report, body).expect("rewrite identical");
        assert_eq!(
            compute_meta_hash(&stage, machine)
                .expect("hash")
                .expect("some"),
            first,
            "rewriting the same bytes with a new mtime must not change the digest"
        );

        // Any content difference, however small, must change it.
        fs::write(
            &report,
            r#"{"reported_at":"2026-09-27T00:00:00Z","pending":2}"#,
        )
        .expect("write");
        assert_ne!(
            compute_meta_hash(&stage, machine)
                .expect("hash")
                .expect("some"),
            first,
            "one changed field must change the digest"
        );
        // Adding a file changes it too: absence of metadata is not metadata.
        fs::write(status_dir.join("install-2.json"), body).expect("write");
        assert_ne!(
            compute_meta_hash(&stage, machine)
                .expect("hash")
                .expect("some"),
            first,
            "a second report must change the digest"
        );
    }

    /// ADR-022's second invariant: the record advances only after a push
    /// succeeds. `compute_meta_hash` is what `push` calls *before* the backup,
    /// so it must not touch the record, and a record write that fails must
    /// leave the previous record standing — otherwise the change that push
    /// failed to carry is recorded as pushed and never pushed at all.
    #[test]
    fn the_record_advances_only_when_a_push_records_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let state_dir = dir.path().join("state");
        let machine = "mac-test";
        let meta_dir = stage.join(META_DIR).join(machine);
        fs::create_dir_all(&meta_dir).expect("create_dir");
        let machine_json = meta_dir.join("machine.json");
        fs::write(&machine_json, r#"{"name":"first"}"#).expect("write");
        let record = pushed_meta_record_path(&state_dir, machine);

        // What `push` does before the backup: hash, and write nothing.
        let first = compute_meta_hash(&stage, machine)
            .expect("hash")
            .expect("some");
        assert!(
            !record.exists(),
            "computing the hash must not write the record"
        );
        assert!(
            check_meta_changed(&stage, machine, &state_dir).expect("no record"),
            "a push that never reached its recording step must leave the change pending"
        );

        // The push succeeded, so the caller records exactly what it carried.
        record_pushed_meta_hash(&state_dir, machine, Some(&first)).expect("record");
        assert!(!check_meta_changed(&stage, machine, &state_dir).expect("recorded"));

        // Metadata changes again, and the recording of it fails: the temp path
        // is occupied by a directory, which fails the create on every platform.
        fs::write(&machine_json, r#"{"name":"second"}"#).expect("write");
        let second = compute_meta_hash(&stage, machine)
            .expect("hash")
            .expect("some");
        assert!(check_meta_changed(&stage, machine, &state_dir).expect("changed"));
        let tmp = record.with_extension("sha256.tmp");
        fs::create_dir_all(&tmp).expect("block the temp path");
        let written = record_pushed_meta_hash(&state_dir, machine, Some(&second));
        fs::remove_dir(&tmp).expect("unblock");
        assert!(
            written.is_err(),
            "a record that cannot be written is an error, not a silent success"
        );
        assert_eq!(
            load_pushed_meta_hash(&state_dir, machine),
            PushedMetaRead::Present(first.clone()),
            "a failed recording must leave the previous record standing"
        );
        assert!(
            check_meta_changed(&stage, machine, &state_dir).expect("still pending"),
            "so the next run still sees the change the failed recording would have advanced"
        );

        // And a successful recording does advance it.
        record_pushed_meta_hash(&state_dir, machine, Some(&second)).expect("record");
        assert!(!check_meta_changed(&stage, machine, &state_dir).expect("recorded"));

        // Recording "there is no metadata" retires the record rather than
        // leaving a digest on disk to be compared against nothing.
        fs::remove_file(&machine_json).expect("remove meta");
        assert!(check_meta_changed(&stage, machine, &state_dir).expect("removed"));
        record_pushed_meta_hash(&state_dir, machine, None).expect("retire");
        assert_eq!(
            load_pushed_meta_hash(&state_dir, machine),
            PushedMetaRead::Missing,
            "the retired record is gone, not an empty digest"
        );
        assert!(!check_meta_changed(&stage, machine, &state_dir).expect("retired"));
    }

    /// An empty stage is `Ok(None)`, and that is a measurement rather than a
    /// fallback — so it is pinned for the case the existing test does not
    /// reach: both namespaces present on disk and holding nothing, which is
    /// what a stage looks like before the collector's first write.
    #[test]
    fn an_empty_stage_is_no_metadata_not_an_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stage = dir.path().join("stage");
        let state_dir = dir.path().join("state");
        let machine = "mac-test";
        fs::create_dir_all(stage.join(META_DIR).join(machine)).expect("create meta dir");
        fs::create_dir_all(stage.join("ext-status").join(machine)).expect("create status dir");

        assert_eq!(count_meta_files(&stage, machine).expect("count"), 0);
        assert!(!has_meta_files(&stage, machine).expect("has_meta"));
        assert_eq!(compute_meta_hash(&stage, machine).expect("hash"), None);
        assert_eq!(
            evaluate_run_once_change(&stage, machine, &state_dir, 0, false).expect("empty"),
            (false, false),
            "nothing to push and nothing to say"
        );

        // A record standing alone against an empty stage is a change: the
        // metadata a previous push carried is gone.
        record_pushed_meta_hash(&state_dir, machine, Some(&"c".repeat(64))).expect("record");
        assert!(check_meta_changed(&stage, machine, &state_dir).expect("removed"));
    }
}
