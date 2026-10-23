//! Durable record of what the local mirror has completely synced (ADR-062 /
//! LM-2).
//!
//! [`crate::pushstate`] records the last successful push per destination;
//! this file records the *mirror's* side of the same discipline. It holds the
//! set of snapshots whose closure (snapshot file + the index files of that
//! sync round + the packs that snapshot's tree references) is complete in the
//! mirror and hash-verified, and it is written **only by one successfully
//! completed sync** (`mirror sync`'s success path). A round that stopped early
//! — the round budget ran out, the free-disk threshold tripped, the remote
//! became unreachable mid-copy — writes nothing here, exactly the way a push
//! failure writes no push-state row: the fact the file names is *completed*,
//! not *attempted* (ADR-062 D6.2, ruling 2).
//!
//! That makes this file the only input a future reclaim leg may consult for
//! mirror coverage: it reads this set, never re-scans the mirror directory
//! (ADR-062 D4). The existing record survives an interrupted next round
//! untouched, so an interrupted fill widens nothing and narrows nothing —
//! coverage answers stay exactly what the last completed sync established,
//! and snapshots that arrived after it stay un-covered until some sync
//! completes again.
//!
//! A file that exists but cannot be parsed, or carries an unknown version, is
//! *unreadable* — and an unreadable record may never become deletion
//! permission, so the reclaim-side read interface in this module answers
//! "covered" only for a readable record that names the snapshot
//! ([`snapshot_covered`]; ADR-062 D6.2, the unknown-never-empty family
//! applied to mirror-state).
//!
//! Privacy: a destination name, a unix time, byte counts and snapshot ids
//! (content hashes). No paths, no session ids, no conversation content.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;

/// File name inside the state dir (`collect::default_state_dir()`).
pub const MIRROR_STATE_FILE: &str = "mirror-state.json";

/// Bumped whenever the shape below changes incompatibly. An unknown version
/// is treated as unreadable, never as a healthy record.
pub const MIRROR_STATE_VERSION: u32 = 1;

/// The whole file: one row per mirror. A machine mirrors one destination
/// (ADR-062 D1, ruling 1), so there is exactly one record with the mirrored
/// destination's declared name attached — kept as a field rather than a map
/// so a stale file from a re-pointed `mirror_of` cannot pass as current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorState {
    pub version: u32,
    /// Wall-clock seconds since the epoch at the moment the sync completed.
    pub finished_at_unix: u64,
    /// The declared name of the destination this record mirrors (`[mirror]
    /// mirror_of`).
    pub mirror_of: String,
    /// Snapshot ids whose closure was complete and hash-verified in the
    /// mirror when the last completed sync finished. A snapshot that is not
    /// in this set is *not covered*, whatever the mirror directory happens to
    /// hold.
    pub closure_complete: BTreeSet<String>,
    /// Snapshots the destination listing held when the sync completed. The
    /// covered set is a subset of this; the difference is the snapshots a
    /// later reclaim will hold back (`not-mirrored`), readable without tying
    /// that surface to this file's shape now.
    pub snapshots_on_remote: u64,
    /// Bytes the whole remote repository held (the byte-exact copy the
    /// mirror keeps). A number a status surface can print without another
    /// network listing.
    pub remote_total_bytes: u64,
}

pub fn mirror_state_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join(MIRROR_STATE_FILE)
}

/// What a read of the state file can tell us. A file that exists but cannot
/// be parsed is its own case — it is neither "never synced" nor "synced".
#[derive(Debug, Clone)]
pub enum MirrorStateRead {
    Missing,
    Unreadable(String),
    Present(MirrorState),
}

/// Persist crash-safely: temp file + rename, same shape as
/// [`crate::pushstate`].
///
/// Only [`crate::mirror`] calls this, on a sync that completed — the failure
/// and stopped-round paths must not move "what the mirror holds" forward,
/// because the fact the field names is *completed*, not *attempted*.
pub(crate) fn save(state_dir: &Path, state: &MirrorState) -> anyhow::Result<()> {
    fs::create_dir_all(state_dir)?;
    let path = mirror_state_path(state_dir);
    let tmp = path.with_file_name(format!(".{MIRROR_STATE_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(state)?;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load(state_dir: &Path) -> MirrorStateRead {
    let path = mirror_state_path(state_dir);
    match fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => MirrorStateRead::Missing,
        Err(e) => MirrorStateRead::Unreadable(e.kind().to_string()),
        Ok(bytes) => match serde_json::from_slice::<MirrorState>(&bytes) {
            Ok(state) if state.version == MIRROR_STATE_VERSION => MirrorStateRead::Present(state),
            Ok(state) => MirrorStateRead::Unreadable(format!("unknown version {}", state.version)),
            Err(e) => MirrorStateRead::Unreadable(format!("malformed json: {e}")),
        },
    }
}

/// The reclaim leg's read-side interface (ADR-062 D4/D6.2, wired by the
/// reclaim implementation on its own schedule): is snapshot `snapshot_id`'s
/// closure complete in the mirror?
///
/// The answer is `true` only when the state file is readable and its
/// completed-sync set names the snapshot. A missing file — the first fill has
/// simply never finished — and an unreadable one — corrupt or an unknown
/// version — both answer `false`: an unreadable record may never become
/// deletion permission, and the caller is expected to report the held-back
/// session as `not-mirrored` with the corresponding alarm, not to delete it.
#[must_use]
pub fn snapshot_covered(state: &MirrorStateRead, snapshot_id: &str) -> bool {
    match state {
        MirrorStateRead::Present(state) => state.closure_complete.contains(snapshot_id),
        MirrorStateRead::Missing | MirrorStateRead::Unreadable(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> MirrorState {
        let mut closure_complete = BTreeSet::new();
        closure_complete.insert("a".repeat(64));
        MirrorState {
            version: MIRROR_STATE_VERSION,
            finished_at_unix: 1_800_000_000,
            mirror_of: "storagebox".to_string(),
            closure_complete,
            snapshots_on_remote: 1,
            remote_total_bytes: 4096,
        }
    }

    /// Missing, unreadable and present stay three states, the same way the
    /// other state files keep them — an unknown version may never fold into
    /// a healthy record.
    #[test]
    fn missing_unreadable_and_present_stay_three_states() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        assert!(matches!(load(&state_dir), MirrorStateRead::Missing));

        fs::create_dir_all(&state_dir).unwrap();
        fs::write(mirror_state_path(&state_dir), "not json").unwrap();
        assert!(matches!(load(&state_dir), MirrorStateRead::Unreadable(_)));

        fs::write(
            mirror_state_path(&state_dir),
            r#"{ "version": 99, "finished_at_unix": 1, "mirror_of": "d", "closure_complete": [], "snapshots_on_remote": 0, "remote_total_bytes": 0 }"#,
        )
        .unwrap();
        match load(&state_dir) {
            MirrorStateRead::Unreadable(why) => assert!(why.contains("unknown version"), "{why}"),
            other => panic!("expected Unreadable, got {other:?}"),
        }

        save(&state_dir, &state()).unwrap();
        assert!(matches!(load(&state_dir), MirrorStateRead::Present(_)));
    }

    /// The reclaim-side interface: covered only for a readonly record that
    /// names the snapshot; missing and unreadable both answer false, because
    /// an unreadable record may never become deletion permission.
    #[test]
    fn coverage_requires_a_readable_record_that_names_the_snapshot() {
        let covered = "a".repeat(64);
        let uncovered = "b".repeat(64);
        let present = MirrorStateRead::Present(state());
        assert!(snapshot_covered(&present, &covered));
        assert!(!snapshot_covered(&present, &uncovered));

        let dir = tempfile::tempdir().unwrap();
        let missing = load(&dir.path().join("state"));
        assert!(!snapshot_covered(&missing, &covered));

        let unreadable = MirrorStateRead::Unreadable("malformed json".to_string());
        assert!(!snapshot_covered(&unreadable, &covered));
    }

    /// The save is crash-safe in the same shape as the other state files:
    /// a successful round leaves the real file and no temp file behind.
    #[test]
    fn a_successful_save_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        save(&state_dir, &state()).unwrap();
        assert!(mirror_state_path(&state_dir).is_file());
        assert!(!mirror_state_path(&state_dir)
            .with_file_name(".mirror-state.json.tmp")
            .exists());
    }
}
