//! manifest — persistent per-session summaries so L3 can verify after the
//! sealed shard body is reclaimed (ADR-020 Phase 3).
//!
//! L3 reconcile currently derives its expected manifest by re-hashing the
//! sealed shards still on the staging disk ([`crate::verify::expected_manifest`]).
//! Once the archived body is reclaimed, that source is gone. What L3 actually
//! needs is not the bytes but the digest triple derived from them —
//! `(shard_count, concat_bytes, concat_sha256)`, about 100 bytes per session —
//! so this module keeps that triple as a durable per-session summary while the
//! body still exists.
//!
//! Each summary is one JSONL line, persisted at
//! `<stage>/meta/<machine>/manifest-v1.jsonl` — the same directory the
//! activity sidecar uses, which is verified not to interfere with the
//! `sessions/` path resolution of verify / readback.
//!
//! Hard rule, matching the activity index: a *missing* summary file is not the
//! same as a summary that says "zero sessions". `Missing` means "no baseline
//! exists"; `Empty` means "a baseline exists and expects zero sessions". L3
//! must fail when the body is gone and no baseline exists — a vacuous pass
//! would be "proving" nothing against nothing.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::store;

/// Directory under the stage holding per-machine metadata sidecars.
pub const META_DIR: &str = "meta";
/// File name of the version-1 session-summary index.
pub const MANIFEST_FILE: &str = "manifest-v1.jsonl";

/// One persistent session summary: the digest triple L3 reconciles against,
/// plus the bookkeeping timestamp of when the summary was sealed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionManifest {
    /// Native session id (the `sessions/<machine>/<id>` directory name).
    pub session_id: String,
    /// Machine partition the session was sealed under.
    pub machine: String,
    /// Number of sealed shards concatenated, in sequence order.
    pub shard_count: usize,
    /// Byte length of the concatenated payload.
    pub concat_bytes: u64,
    /// sha256 of the concatenated payload.
    pub concat_sha256: String,
    /// Unix seconds when this summary was generated (bookkeeping only, not
    /// part of the reconciliation comparison).
    pub sealed_at_unix: i64,
}

/// Result of reading one machine's manifest file, keeping the three
/// distinguishable states apart. `Missing` is deliberately not an empty
/// baseline: "no summary exists" and "a summary exists that expects zero
/// sessions" are different facts with different L3 consequences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestFileState {
    /// `<stage>/meta/<machine>/manifest-v1.jsonl` does not exist at all.
    Missing,
    /// The file exists but contains zero rows (a legitimate zero-session
    /// baseline, distinct from missing).
    Empty,
    /// The file exists and parsed into one or more rows.
    Loaded(Vec<SessionManifest>),
}

/// All stored summaries found across every machine, plus how many manifest
/// files that came from. `files == 0` is "no baseline anywhere", which L3 must
/// treat as an error, not as an empty expectation list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredManifest {
    /// Number of `manifest-v1.jsonl` files found under `<stage>/meta/*/`.
    pub files: usize,
    /// Every row across those files.
    pub rows: Vec<SessionManifest>,
}

/// Absolute path of one machine's manifest file.
pub fn manifest_path(stage: &Path, machine: &str) -> PathBuf {
    stage.join(META_DIR).join(machine).join(MANIFEST_FILE)
}

/// Current unix time in whole seconds.
pub fn now_unix_seconds() -> anyhow::Result<i64> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before the unix epoch")?;
    Ok(duration.as_secs() as i64)
}

/// Generate summaries for one machine by hashing its sealed shards right now.
pub fn generate_manifest(stage: &Path, machine: &str) -> anyhow::Result<Vec<SessionManifest>> {
    generate_manifest_at(stage, machine, now_unix_seconds()?)
}

/// Generate summaries for one machine at an explicit seal time (deterministic,
/// for tests and for callers that already know the seal time).
pub fn generate_manifest_at(
    stage: &Path,
    machine: &str,
    sealed_at_unix: i64,
) -> anyhow::Result<Vec<SessionManifest>> {
    let machine_dir = stage.join(store::SESSIONS_DIR).join(machine);
    let rd = match fs::read_dir(&machine_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", machine_dir.display())),
    };
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let session_id = entry.file_name().to_string_lossy().into_owned();
        // Same derivation as `verify::expected_manifest`: count the sealed
        // shards and hash their concatenation in global sequence order.
        let shard_count = store::sealed_shard_entries(&entry.path())?.len();
        // A session directory with no sealed shard has no body to summarise.
        // Two ways to get here and both must be skipped:
        //   * the body was reclaimed (`stagereclaim`) — the session dir deliberately
        //     survives because it still holds `shard-seq`, and the authoritative
        //     summary for it now lives in the *stored* manifest, not on disk;
        //   * the session was just created and nothing has been sealed yet.
        // Emitting a row here would overwrite a reclaimed session's real summary
        // with an empty one — i.e. silently destroy the only remaining evidence
        // of what the archive is supposed to hold.
        if shard_count == 0 {
            continue;
        }
        let concat = store::concat_shards(stage, machine, &session_id)?;
        out.push(SessionManifest {
            session_id,
            machine: machine.to_string(),
            shard_count,
            concat_bytes: concat.len() as u64,
            concat_sha256: hex_digest(&Sha256::digest(&concat)),
            sealed_at_unix,
        });
    }
    out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    Ok(out)
}

/// Serialise one summary as a single JSONL line (trailing newline included).
pub fn to_jsonl(row: &SessionManifest) -> String {
    // Cannot fail for this shape: plain strings, two ints, a usize and a u64.
    serde_json::to_string(row).expect("SessionManifest serializes to JSON") + "\n"
}

/// Write a machine's summaries atomically (temp file + rename) so a torn write
/// can never be mistaken for a valid baseline.
pub fn write_manifest(stage: &Path, machine: &str, rows: &[SessionManifest]) -> anyhow::Result<()> {
    let path = manifest_path(stage, machine);
    let parent = path.parent().context("manifest path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let mut body = String::new();
    for row in rows {
        body.push_str(&to_jsonl(row));
    }
    let tmp = parent.join(format!(".{MANIFEST_FILE}.tmp"));
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(body.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Read one machine's manifest file back, distinguishing Missing / Empty /
/// Loaded.
pub fn read_manifest(stage: &Path, machine: &str) -> anyhow::Result<ManifestFileState> {
    let path = manifest_path(stage, machine);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ManifestFileState::Missing);
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let mut rows = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: SessionManifest = serde_json::from_str(line)
            .with_context(|| format!("parse {} line {}", path.display(), idx + 1))?;
        rows.push(row);
    }
    if rows.is_empty() {
        Ok(ManifestFileState::Empty)
    } else {
        Ok(ManifestFileState::Loaded(rows))
    }
}

/// Read every machine's stored summaries. `files == 0` means no baseline
/// exists anywhere. A corrupt line, or a row whose `machine` field does not
/// match the directory it was read from, is a hard error — a baseline that is
/// silently weakened is worse than none.
pub fn stored_manifest_rows(stage: &Path) -> anyhow::Result<StoredManifest> {
    let meta_dir = stage.join(META_DIR);
    let rd = match fs::read_dir(&meta_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(StoredManifest {
                files: 0,
                rows: Vec::new(),
            });
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", meta_dir.display())),
    };
    let mut rows = Vec::new();
    let mut files = 0;
    for entry in rd {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let machine = entry.file_name().to_string_lossy().into_owned();
        match read_manifest(stage, &machine)? {
            ManifestFileState::Missing => {}
            ManifestFileState::Empty => files += 1,
            ManifestFileState::Loaded(mut machine_rows) => {
                files += 1;
                for row in &mut machine_rows {
                    if row.machine != machine {
                        anyhow::bail!(
                            "manifest row machine mismatch: {} carries machine `{}` inside the `{}` directory",
                            manifest_path(stage, &machine).display(),
                            row.machine,
                            machine
                        );
                    }
                }
                rows.append(&mut machine_rows);
            }
        }
    }
    Ok(StoredManifest { files, rows })
}

/// Compare the stored summaries against a freshly derived manifest. They must
/// agree on the session set and on every session's digest triple;
/// `sealed_at_unix` is bookkeeping and is not compared. Any disagreement is an
/// error — a summary that disagrees with the body it was derived from is
/// evidence of a corrupt or stale baseline.
pub fn compare_manifests(
    stored: &[SessionManifest],
    derived: &[SessionManifest],
) -> anyhow::Result<()> {
    let key = |row: &SessionManifest| (row.machine.clone(), row.session_id.clone());
    let stored_map: BTreeMap<(String, String), &SessionManifest> =
        stored.iter().map(|row| (key(row), row)).collect();
    let derived_map: BTreeMap<(String, String), &SessionManifest> =
        derived.iter().map(|row| (key(row), row)).collect();

    for (key, stored_row) in &stored_map {
        if !derived_map.contains_key(key) {
            anyhow::bail!(
                "manifest mismatch: stored summary has session `{}` on machine `{}` but the stage no longer derives it",
                stored_row.session_id,
                stored_row.machine
            );
        }
    }
    for (key, derived_row) in &derived_map {
        if !stored_map.contains_key(key) {
            anyhow::bail!(
                "manifest mismatch: the stage derives session `{}` on machine `{}` but no stored summary has it",
                derived_row.session_id,
                derived_row.machine
            );
        }
    }
    for (key, stored_row) in &stored_map {
        let derived_row = derived_map.get(key).expect("presence checked above");
        if stored_row.shard_count != derived_row.shard_count
            || stored_row.concat_bytes != derived_row.concat_bytes
            || stored_row.concat_sha256 != derived_row.concat_sha256
        {
            anyhow::bail!(
                "manifest mismatch for session `{}` on machine `{}`: stored=({} shards, {} bytes, sha256 {}) derived=({} shards, {} bytes, sha256 {})",
                stored_row.session_id,
                stored_row.machine,
                stored_row.shard_count,
                stored_row.concat_bytes,
                stored_row.concat_sha256,
                derived_row.shard_count,
                derived_row.concat_bytes,
                derived_row.concat_sha256,
            );
        }
    }
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A deterministic sealed shard: `seq` and `i` are in the line, so
    /// different shards hash differently.
    fn write_session(stage: &Path, machine: &str, session: &str, nshards: u64) {
        for seq in 1..=nshards {
            let lines: Vec<String> = (0..10)
                .map(|i| format!("{{\"seq\":{seq},\"i\":{i}}}"))
                .collect();
            store::write_sealed_shard(store::StageWriter::Collect, stage, machine, session, &lines)
                .unwrap();
        }
    }

    #[test]
    fn manifest_path_lands_in_meta_machine_dir() {
        let p = manifest_path(Path::new("/stage"), "mbp-2");
        assert_eq!(p, PathBuf::from("/stage/meta/mbp-2/manifest-v1.jsonl"));
    }

    #[test]
    fn now_unix_seconds_is_a_plausible_recent_epoch() {
        let now = now_unix_seconds().unwrap();
        // A conversation-archive timestamp must be a plausible unix time: not 0
        // (the "no time" sentinel), and not the distant future.
        assert!(now > 1_577_836_800, "now should be after 2020, got {now}");
        assert!(now < 4_102_444_800, "now should be before 2100, got {now}");
    }

    #[test]
    fn generate_manifest_without_explicit_time_still_produces_rows() {
        let dir = TempDir::new().unwrap();
        let stage = dir.path();
        write_session(stage, "m-now", "s-1", 1);
        let rows = generate_manifest(stage, "m-now").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].sealed_at_unix > 1_577_836_800,
            "seal time should be a recent unix time"
        );
        drop(dir);
    }

    #[test]
    fn generate_read_roundtrip_is_consistent() {
        let dir = TempDir::new().unwrap();
        let stage = dir.path();
        let machine = "m-roundtrip";
        write_session(stage, machine, "s-1", 2);
        write_session(stage, machine, "s-2", 1);

        let rows = generate_manifest_at(stage, machine, 1_736_944_496).unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.machine, machine);
            assert_eq!(row.sealed_at_unix, 1_736_944_496);
        }

        write_manifest(stage, machine, &rows).unwrap();
        let path = manifest_path(stage, machine);
        assert!(path.is_file(), "manifest file must exist after write");

        match read_manifest(stage, machine).unwrap() {
            ManifestFileState::Loaded(back) => {
                assert_eq!(back, rows, "read-back must equal the generated rows");
            }
            other => panic!("expected Loaded, got {other:?}"),
        }

        // Every stored row must round-trip through the JSONL encoding.
        let first = &rows[0];
        let line = to_jsonl(first);
        assert!(line.ends_with('\n'));
        let back: SessionManifest = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(back, *first);
        drop(dir);
    }

    #[test]
    fn read_manifest_three_states_missing_empty_loaded() {
        let dir = TempDir::new().unwrap();
        let stage = dir.path();
        let machine = "m-states";

        // No file yet -> Missing, never "says zero".
        assert_eq!(
            read_manifest(stage, machine).unwrap(),
            ManifestFileState::Missing
        );

        // An existing but empty file -> Empty (a real zero-session baseline).
        let path = manifest_path(stage, machine);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "").unwrap();
        assert_eq!(
            read_manifest(stage, machine).unwrap(),
            ManifestFileState::Empty
        );

        // A file with one row -> Loaded.
        write_session(stage, machine, "s-1", 1);
        let rows = generate_manifest_at(stage, machine, 42).unwrap();
        assert_eq!(rows.len(), 1, "machine now has one session");
        write_manifest(stage, machine, &rows).unwrap();
        let state = read_manifest(stage, machine).unwrap();
        match state {
            ManifestFileState::Loaded(back) => {
                assert_eq!(back, rows, "read-back must equal what was written")
            }
            _ => panic!("expected Loaded"),
        }
        drop(dir);
    }

    #[test]
    fn read_manifest_missing_is_not_treated_as_zero() {
        let dir = TempDir::new().unwrap();
        let stage = dir.path();
        // Distinct states must be distinguishable by equality: a caller that
        // collapses Missing into "0 rows" would be lying about a baseline.
        assert_ne!(
            ManifestFileState::Missing,
            ManifestFileState::Empty,
            "missing and empty must never compare equal"
        );
        assert_eq!(
            read_manifest(stage, "no-machine").unwrap(),
            ManifestFileState::Missing
        );
        drop(dir);
    }

    #[test]
    fn read_manifest_rejects_a_corrupt_line() {
        let dir = TempDir::new().unwrap();
        let stage = dir.path();
        let machine = "m-corrupt";
        let rows = generate_manifest_at(stage, machine, 7).unwrap();
        write_manifest(stage, machine, &rows).unwrap();
        let path = manifest_path(stage, machine);
        let mut body = fs::read_to_string(&path).unwrap();
        body.push_str("this is not json\n");
        fs::write(&path, body).unwrap();
        let err = read_manifest(stage, machine).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("parse"), "error should mention parsing: {msg}");
        drop(dir);
    }

    #[test]
    fn stored_manifest_rows_zero_files_when_no_baseline() {
        let dir = TempDir::new().unwrap();
        let stored = stored_manifest_rows(dir.path()).unwrap();
        assert_eq!(stored.files, 0);
        assert!(stored.rows.is_empty());
        drop(dir);
    }

    #[test]
    fn stored_manifest_rows_collects_across_machines_and_validates_machine() {
        let dir = TempDir::new().unwrap();
        let stage = dir.path();
        write_session(stage, "m-a", "s-a", 1);
        write_session(stage, "m-b", "s-b", 1);
        let rows_a = generate_manifest_at(stage, "m-a", 1).unwrap();
        let rows_b = generate_manifest_at(stage, "m-b", 2).unwrap();
        write_manifest(stage, "m-a", &rows_a).unwrap();
        write_manifest(stage, "m-b", &rows_b).unwrap();

        let stored = stored_manifest_rows(stage).unwrap();
        assert_eq!(stored.files, 2);
        assert_eq!(stored.rows.len(), 2);
        drop(dir);
    }

    #[test]
    fn stored_manifest_rows_rejects_a_mismatched_machine_row() {
        let dir = TempDir::new().unwrap();
        let stage = dir.path();
        let machine = "m-labelled";
        // Write a row whose machine field is wrong for the directory it sits in.
        let path = manifest_path(stage, machine);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let wrong = SessionManifest {
            session_id: "s-1".into(),
            machine: "m-somewhere-else".into(),
            shard_count: 1,
            concat_bytes: 0,
            concat_sha256: "0".repeat(64),
            sealed_at_unix: 1,
        };
        fs::write(&path, to_jsonl(&wrong)).unwrap();
        let err = stored_manifest_rows(stage).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("machine"),
            "error should mention the machine: {msg}"
        );
        drop(dir);
    }

    #[test]
    fn compare_manifests_accepts_identical_and_ignores_seal_time() {
        let a = SessionManifest {
            session_id: "s-1".into(),
            machine: "m".into(),
            shard_count: 2,
            concat_bytes: 99,
            concat_sha256: "abc".into(),
            sealed_at_unix: 100,
        };
        let mut b = a.clone();
        // Bookkeeping time is not part of the comparison.
        b.sealed_at_unix = 999;
        assert!(compare_manifests(&[a], &[b]).is_ok());
    }

    #[test]
    fn compare_manifests_rejects_a_digest_triplet_change() {
        let base = SessionManifest {
            session_id: "s-1".into(),
            machine: "m".into(),
            shard_count: 2,
            concat_bytes: 99,
            concat_sha256: "abc".into(),
            sealed_at_unix: 100,
        };
        let mut other = base.clone();
        other.concat_sha256 = "different".into();
        let err = compare_manifests(&[base], &[other]).unwrap_err();
        assert!(err.to_string().contains("mismatch"));
    }

    #[test]
    fn compare_manifests_rejects_a_missing_session() {
        let stored = SessionManifest {
            session_id: "s-1".into(),
            machine: "m".into(),
            shard_count: 1,
            concat_bytes: 1,
            concat_sha256: "a".into(),
            sealed_at_unix: 1,
        };
        let derived = SessionManifest {
            session_id: "s-2".into(),
            machine: "m".into(),
            shard_count: 1,
            concat_bytes: 1,
            concat_sha256: "a".into(),
            sealed_at_unix: 2,
        };
        let err = compare_manifests(&[stored], &[derived]).unwrap_err();
        assert!(err.to_string().contains("session"));
    }
}
