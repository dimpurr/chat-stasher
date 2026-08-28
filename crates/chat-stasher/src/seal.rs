//! Sealing for files already owned by chat-stasher's stage.
//!
//! The collector reads harness files by byte offset and never renames them.
//! This module remains useful for a stage-local housekeeping operation, but
//! its first operation is a path-boundary check: `--active` must resolve below
//! `--stage`, including when the active path is a symlink. A path outside that
//! boundary is rejected before any stage mutation is possible.

use crate::scanner::{Confidence, HarnessRegistry, RegistryCell, RegistryHarness};
use anyhow::Context;
use std::fs;
use std::path::Path;

/// Raw `seal_policy` values in `data/harness-registry-v1.json`.
pub const SEAL_RENAME: &str = "rename";
pub const SEAL_NO_RENAME: &str = "no-rename";
pub const SEAL_NOT_APPLICABLE: &str = "not-applicable";

/// Resolved per-harness seal policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealPolicy {
    /// Legacy registry token retained for the stage-local command gate.
    Rename,
    /// Known fd-holder, or anything unverified / unconfirmed / unknown.
    NoRename,
    /// Single-file (sqlite) or otherwise non-appending store — no active
    /// jsonl exists to rename.
    NotApplicable,
}

impl SealPolicy {
    /// Classify the registry's raw string. Anything that is not the attested
    /// `rename` value — including empty, `no-rename`, and unknown tokens — is
    /// `NoRename`. No unknown token can ever widen the allowlist.
    pub fn classify(raw: &str) -> SealPolicy {
        match raw.trim() {
            SEAL_RENAME => SealPolicy::Rename,
            SEAL_NOT_APPLICABLE => SealPolicy::NotApplicable,
            _ => SealPolicy::NoRename,
        }
    }

    /// Raw registry token this policy corresponds to.
    pub fn label(&self) -> &'static str {
        match self {
            SealPolicy::Rename => SEAL_RENAME,
            SealPolicy::NoRename => SEAL_NO_RENAME,
            SealPolicy::NotApplicable => SEAL_NOT_APPLICABLE,
        }
    }
}

/// The allowlist gate. Rename-sealing a harness is allowed only when **all** of:
///   1. the harness entry's `seal_policy` is exactly `rename`,
///   2. the harness's `seal_source` is non-empty — an attachable measurement
///      or source line is the required trailing evidence for a rename,
///   3. the current-platform cell's `source` is non-empty, and
///   4. the platform cell's `confidence` is `源码确认` — a community claim or
///      an unrecognised confidence can never clear the gate.
/// Every other combination — unconfirmed, uncredited, `no-rename`,
/// `not-applicable`, unknown tokens — returns false, i.e. **no rename**.
pub fn seal_allowed(h: &RegistryHarness, cell: &RegistryCell) -> bool {
    if SealPolicy::classify(&h.seal_policy) != SealPolicy::Rename {
        return false;
    }
    if h.seal_source.trim().is_empty() {
        return false;
    }
    if cell.source.trim().is_empty() {
        return false;
    }
    Confidence::classify(&cell.confidence) == Confidence::Confirmed
}

/// Look a harness up by its registry `id`.
pub fn harness_by_id<'a>(registry: &'a HarnessRegistry, id: &str) -> Option<&'a RegistryHarness> {
    registry.harnesses.iter().find(|h| h.id == id)
}

/// Verify that `active` belongs to our own stage tree.
///
/// Existing symlinks are canonicalised, so a link inside the stage that points
/// into a harness directory is rejected too. Missing active paths are checked
/// lexically and then rejected by [`seal_active_file`] for non-existence.
pub fn validate_active_in_stage(active: &Path, stage_root: &Path) -> anyhow::Result<()> {
    let stage = fs::canonicalize(stage_root).context("resolve --stage")?;
    let active = fs::canonicalize(active).unwrap_or_else(|_| {
        if active.is_absolute() {
            active.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(active))
                .unwrap_or_else(|_| active.to_path_buf())
        }
    });
    if active == stage || !active.starts_with(&stage) {
        anyhow::bail!("refusing seal: --active must be inside --stage; source left untouched");
    }
    Ok(())
}

/// Rename an already-stage-owned file into the next sealed-shard slot.
///
/// The active file is moved to
/// `<stage>/sessions/<machine>/<session-id>/<bucket>/NNNNNN.jsonl`, where
/// `NNNNNN` is `store::next_shard_seq` and the bucket is derived from the
/// bucket cap. This is deliberately not a harness-file operation and does not
/// create a new harness tail.
///
/// Caller must run [`seal_allowed`] first — this function does not re-check.
/// Refusing to do so for an fd-holding harness silently loses data.
pub fn seal_active_file(
    active: &Path,
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    bucket_cap: usize,
) -> anyhow::Result<u64> {
    crate::store::assert_stage_writer_audited(crate::store::StageWriter::Seal)?;
    validate_active_in_stage(active, stage_root)?;
    if !active.exists() {
        anyhow::bail!("active file does not exist: {}", active.display());
    }
    let seq = crate::store::next_shard_seq(stage_root, machine, session_id)?;
    let dest = crate::store::shard_path_with_cap(stage_root, machine, session_id, seq, bucket_cap);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        anyhow::bail!("seal target already exists: {}", dest.display());
    }
    fs::rename(active, &dest)
        .with_context(|| format!("seal rename {} -> {}", active.display(), dest.display()))?;
    Ok(seq)
}

/// Allowlist-checked sealing: calls [`seal_active_file`] only when
/// [`seal_allowed`] passes. `Ok(None)` means the allowlist excluded the
/// harness (active file untouched); `Ok(Some(seq))` means it was renamed.
pub fn maybe_seal_active(
    h: &RegistryHarness,
    cell: &RegistryCell,
    active: &Path,
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    bucket_cap: usize,
) -> anyhow::Result<Option<u64>> {
    if !seal_allowed(h, cell) {
        return Ok(None);
    }
    seal_active_file(active, stage_root, machine, session_id, bucket_cap).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::{CONF_CONFIRMED, CONF_UNASCERTAINED};
    use crate::store::{self, session_shard_dir, write_sealed_shard};
    use serde_json::json;
    use std::fs;

    fn harness(id: &str, policy: &str, seal_source: &str) -> RegistryHarness {
        serde_json::from_value(json!({
            "id": id,
            "display_name": id,
            "seal_policy": policy,
            "seal_source": seal_source,
        }))
        .unwrap()
    }

    fn cell(confidence: &str, source: &str) -> RegistryCell {
        serde_json::from_value(json!({
            "template": "~/x/",
            "format": "jsonl",
            "confidence": confidence,
            "source": source,
        }))
        .unwrap()
    }

    #[test]
    fn classify_three_states_and_fail_safe_default() {
        assert_eq!(SealPolicy::classify("rename"), SealPolicy::Rename);
        assert_eq!(SealPolicy::classify("no-rename"), SealPolicy::NoRename);
        assert_eq!(
            SealPolicy::classify("not-applicable"),
            SealPolicy::NotApplicable
        );
        // Unknown and missing tokens resolve to no-rename, never rename.
        assert_eq!(SealPolicy::classify(""), SealPolicy::NoRename);
        assert_eq!(SealPolicy::classify("   "), SealPolicy::NoRename);
        assert_eq!(SealPolicy::classify("holds fd"), SealPolicy::NoRename);
        assert_eq!(SealPolicy::classify("weekly"), SealPolicy::NoRename);
        assert_eq!(SealPolicy::classify("RENAME"), SealPolicy::NoRename);
    }

    /// The allowlist is the fail-safe: only a `rename`-policy harness with a
    /// confirmed cell, a cell source, and a seal evidence line clears it.
    #[test]
    fn rename_requires_confirmed_cell_and_evidence_lines() {
        let confirmed = cell(CONF_CONFIRMED, "measured in spike A9");
        let h = harness("claude-code", "rename", "A9: 201 samples lsof=0");
        assert!(seal_allowed(&h, &confirmed));

        // fd-holder: explicit no-rename.
        let codex = harness("codex", "no-rename", "codex-rs recorder holds fd");
        assert!(!seal_allowed(&codex, &confirmed));

        // sqlite store: not applicable.
        let opencode = harness("opencode", "not-applicable", "single sqlite");
        assert!(!seal_allowed(&opencode, &confirmed));

        // rename policy but no evidence line -> excluded.
        let uncredited = harness("claude-code", "rename", "");
        assert!(!seal_allowed(&uncredited, &confirmed));

        // rename policy + evidence but the CELL is a community claim -> excluded.
        let community = cell("community-claim-unverified", "forum post");
        assert!(!seal_allowed(&h, &community));

        // rename policy + evidence but the cell has no source -> excluded.
        let sourceless = cell(CONF_CONFIRMED, "");
        assert!(!seal_allowed(&h, &sourceless));

        // Unknown policy token cannot widen the allowlist.
        let unknown = harness("foo", "guess", "meh");
        assert!(!seal_allowed(&unknown, &confirmed));
    }

    /// Negative self-check (the fail-safe that must not be glossed over): when
    /// the registry marks a harness as fd-holding / unconfirmed, the sealing
    /// path must NOT move the active file — byte-for-byte untouched.
    #[test]
    fn fd_holder_and_unconfirmed_harness_never_rename() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let active = stage
            .join("sessions")
            .join("m")
            .join("s")
            .join("active.jsonl");
        fs::create_dir_all(active.parent().unwrap()).unwrap();
        let active_bytes = b"append-ready tail bytes\n".to_vec();
        fs::write(&active, &active_bytes).unwrap();

        // Case 1: registry says no-rename (codex, fd-holder).
        let codex = harness("codex", "no-rename", "codex-rs holds fd");
        let (stage_root, machine, session) = (&stage, "m", "s");
        let out = maybe_seal_active(
            &codex,
            &cell(CONF_CONFIRMED, "source"),
            &active,
            stage_root,
            machine,
            session,
            20,
        )
        .unwrap();
        assert_eq!(out, None, "no-rename harness must be refused, got {out:?}");
        assert!(active.exists(), "fd-holder's active file must not move");
        assert_eq!(
            fs::read(&active).unwrap(),
            active_bytes,
            "fd-holder's active file must stay byte-identical"
        );
        assert!(
            store::sealed_shard_entries(&session_shard_dir(stage_root, machine, session))
                .unwrap()
                .is_empty()
        );

        // Case 2: rename policy but UNCONFIRMED cell -> also no rename.
        let claude = harness("claude-code", "rename", "A9 measured");
        let unconfirmed = cell(CONF_UNASCERTAINED, "");
        let out = maybe_seal_active(
            &claude,
            &unconfirmed,
            &active,
            stage_root,
            machine,
            session,
            20,
        )
        .unwrap();
        assert_eq!(out, None, "unconfirmed cell must be refused");
        assert!(
            active.exists(),
            "unconfirmed harness's active file must not move"
        );
        drop(dir);
    }

    /// When the allowlist lets a harness through, the rename lands in the next
    /// sealed-shard slot and the original path is left free for a new tail.
    #[test]
    fn seal_active_file_renames_into_next_seq_and_frees_original_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path();
        write_sealed_shard(
            store::StageWriter::Collect,
            stage,
            "m",
            "s",
            &[format!("{{\"base\":1}}")],
        )
        .unwrap();

        let active = session_shard_dir(stage, "m", "s").join("active.jsonl");
        fs::write(&active, b"tail-line\n").unwrap();

        let seq = seal_active_file(&active, stage, "m", "s", 20).unwrap();
        assert_eq!(seq, 2, "next_seq counts the existing first shard");

        let dest = store::shard_path(stage, "m", "s", 2);
        assert!(dest.exists(), "renamed shard must exist at {dest:?}");
        assert_eq!(fs::read(dest).unwrap(), b"tail-line\n");
        assert!(
            !active.exists(),
            "original path must be free after the rename"
        );

        // Original path as the new tail: a fresh active file can start there.
        fs::write(&active, b"new-tail-1\n").unwrap();
        assert!(active.exists());
        assert_eq!(store::next_shard_seq(stage, "m", "s").unwrap(), 3);
        drop(dir);
    }

    #[test]
    fn active_path_outside_stage_is_refused_and_left_untouched() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let active = dir.path().join("harness-live.jsonl");
        fs::create_dir_all(&stage).unwrap();
        fs::write(&active, b"must-remain-at-source\n").unwrap();

        let err = seal_active_file(&active, &stage, "m", "s", 20)
            .expect_err("a harness path outside our stage must be rejected");
        assert!(err.to_string().contains("inside --stage"));
        assert!(active.exists(), "rejected source must remain in place");
        assert_eq!(fs::read(&active).unwrap(), b"must-remain-at-source\n");
        assert!(
            store::sealed_shard_entries(&session_shard_dir(&stage, "m", "s"))
                .unwrap()
                .is_empty()
        );
    }

    /// `harness_by_id` resolves the registry the allowlist is read from.
    #[test]
    fn harness_by_id_resolves_from_registry() {
        let registry: HarnessRegistry = serde_json::from_value(json!({
            "schema_version": 1,
            "generated": "2026-08-16",
            "harnesses": [
                { "id": "claude-code", "display_name": "Claude Code",
                  "seal_policy": "rename", "seal_source": "A9 measured" },
                { "id": "codex", "display_name": "Codex",
                  "seal_policy": "no-rename", "seal_source": "" }
            ]
        }))
        .unwrap();
        assert_eq!(
            harness_by_id(&registry, "claude-code")
                .unwrap()
                .display_name,
            "Claude Code"
        );
        assert_eq!(
            SealPolicy::classify(&harness_by_id(&registry, "codex").unwrap().seal_policy),
            SealPolicy::NoRename
        );
        assert!(harness_by_id(&registry, "gemini-cli").is_none());
    }
}
