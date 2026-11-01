//! Durable record of each destination's last successful push (W1012 / SG-1 C1③).
//!
//! `run-state.json` (see [`crate::runstate`]) records the *most recent pass*
//! and nothing earlier, so it cannot answer "when did destination X last
//! receive a push?" — for a multi-destination machine each hourly timer
//! pushes a different destination, and a single overwritten record cannot
//! say which one succeeded when. This file is that answer, kept per
//! destination name by the one command that performs a push, written only
//! on success: `<state_dir>/push-state.json`.
//!
//! Some destinations a timer never pushes (the machine SG-1 exists for
//! declares one and never installed its hourly timer). For those the record
//! simply has no entry, and the reader must word that as "no push on
//! record", never as "pushed a moment ago" and never as "never pushed": a
//! machine that pushed before this CLI kept records, or pushed by hand,
//! leaves the same absence behind.
//!
//! Privacy: destination names, a unix time, a machine label and a snapshot
//! count. No paths, no session ids, no conversation content.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

/// File name inside the state dir (`collect::default_state_dir()`).
pub const PUSH_STATE_FILE: &str = "push-state.json";

/// Bumped whenever the shape below changes incompatibly. An unknown version
/// is treated as unreadable, never as a healthy record.
pub const PUSH_STATE_VERSION: u32 = 1;

/// Destination key recorded when the config declares no destinations table
/// and the push went to the single configured (or default) repository. The
/// same word `reclaim-stage` prints for that shape, so one machine cannot
/// grow two labels for one archive.
pub const DEFAULT_DESTINATION: &str = "default";

/// One successful push, as durable metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushRecord {
    /// Wall-clock seconds since the epoch at the moment the push finished.
    pub finished_at_unix: u64,
    /// The machine label the push wrote its snapshot under.
    pub machine: String,
    /// Snapshots in the destination after this push (`[push] snapshots`).
    pub snapshots_in_repo: u64,
}

/// The whole file: one row per destination a push on this machine succeeded
/// against, keyed by the destination's declared name (or
/// [`DEFAULT_DESTINATION`] when there is no destinations table).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushState {
    pub version: u32,
    /// State entries, keyed by destination name.
    pub pushes: BTreeMap<String, PushRecord>,
}

impl Default for PushState {
    fn default() -> Self {
        Self {
            version: PUSH_STATE_VERSION,
            pushes: BTreeMap::new(),
        }
    }
}

pub fn push_state_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join(PUSH_STATE_FILE)
}

/// Record one successful push against `destination`, keeping every other
/// row intact. Crash-safe in the same shape as `runstate::save`.
///
/// Only `cmd_push` calls this, on the success path — the failure paths must
/// not move a destination's "last successful push" forward, because the
/// fact the field names is *success*, not *attempted*.
pub fn record_success(
    state_dir: &Path,
    destination: &str,
    machine: &str,
    snapshots_in_repo: u64,
    finished_at_unix: u64,
) -> anyhow::Result<()> {
    crate::test_identity_guard::refuse_fixture_write(&[destination, machine], state_dir)?;
    let mut state = match load(state_dir) {
        PushStateRead::Present(state) => state,
        PushStateRead::Missing => PushState::default(),
        PushStateRead::Unreadable(why) => {
            // An unreadable record is not permission to drop the other
            // destinations' rows: start from empty, and say so — the caller
            // prints the warning, the file keeps only what this run knows.
            eprintln!(
                "warning: push-state.json is unreadable ({why}); rebuilding it from this push — \
                 earlier destinations' last-push times are not known until their next push"
            );
            PushState::default()
        }
    };
    state.pushes.insert(
        destination.to_string(),
        PushRecord {
            finished_at_unix,
            machine: machine.to_string(),
            snapshots_in_repo,
        },
    );
    save(state_dir, &state)
}

/// Persist crash-safely: temp file + rename, same shape as `runstate::save`.
fn save(state_dir: &Path, state: &PushState) -> anyhow::Result<()> {
    fs::create_dir_all(state_dir)
        .with_context(|| format!("create state dir {}", state_dir.display()))?;
    let path = push_state_path(state_dir);
    let tmp = path.with_file_name(format!(".{PUSH_STATE_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(state).context("serialise push state")?;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// What a read of the state file can tell us. A file that exists but cannot
/// be parsed is its own case — it is neither "never pushed" nor "pushed".
#[derive(Debug, Clone)]
pub enum PushStateRead {
    Missing,
    Unreadable(String),
    Present(PushState),
}

pub fn load(state_dir: &Path) -> PushStateRead {
    let path = push_state_path(state_dir);
    match fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => PushStateRead::Missing,
        Err(e) => PushStateRead::Unreadable(e.kind().to_string()),
        Ok(bytes) => match serde_json::from_slice::<PushState>(&bytes) {
            Ok(state) if state.version == PUSH_STATE_VERSION => PushStateRead::Present(state),
            Ok(state) => PushStateRead::Unreadable(format!("unknown version {}", state.version)),
            Err(e) => PushStateRead::Unreadable(format!("malformed json: {e}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> PushRecord {
        PushRecord {
            finished_at_unix: 1_800_000_000,
            machine: "m-unit".to_string(),
            snapshots_in_repo: 7,
        }
    }

    /// The whole point of the file: two destinations, two independent
    /// timestamps, neither overwritten by the other.
    #[test]
    fn two_destinations_keep_independent_records() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        record_success(&state_dir, "storagebox", "m-unit", 7, 1_800_000_000).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        record_success(&state_dir, "r2", "m-unit", 1, before).unwrap();
        match load(&state_dir) {
            PushStateRead::Present(state) => {
                assert_eq!(state.pushes.len(), 2, "one row per destination");
                assert_eq!(state.pushes["storagebox"], record());
                assert_eq!(state.pushes["r2"].finished_at_unix, before);
            }
            other => panic!("expected a present state, got {other:?}"),
        }
    }

    /// A later push to one destination refreshes only that row.
    #[test]
    fn a_refresh_keeps_the_other_destination_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        record_success(&state_dir, "a", "m", 1, 100).unwrap();
        record_success(&state_dir, "b", "m", 2, 200).unwrap();
        record_success(&state_dir, "a", "m", 3, 300).unwrap();
        match load(&state_dir) {
            PushStateRead::Present(state) => {
                assert_eq!(state.pushes["a"].finished_at_unix, 300);
                assert_eq!(state.pushes["b"].finished_at_unix, 200);
            }
            other => panic!("expected a present state, got {other:?}"),
        }
    }

    /// Three states, kept apart the way the rest of the file does: missing,
    /// unreadable, present. "Unreadable" is what an unknown version reads
    /// as, so a future shape can never be silently folded into a healthy
    /// record.
    #[test]
    fn missing_unreadable_and_present_stay_three_states() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        assert!(matches!(load(&state_dir), PushStateRead::Missing));

        fs::create_dir_all(&state_dir).unwrap();
        fs::write(push_state_path(&state_dir), "not json").unwrap();
        assert!(matches!(load(&state_dir), PushStateRead::Unreadable(_)));

        fs::write(
            push_state_path(&state_dir),
            r#"{ "version": 99, "pushes": {} }"#,
        )
        .unwrap();
        match load(&state_dir) {
            PushStateRead::Unreadable(why) => assert!(why.contains("unknown version"), "{why}"),
            other => panic!("expected Unreadable, got {other:?}"),
        }

        record_success(&state_dir, "d", "m", 1, 5).unwrap();
        assert!(matches!(load(&state_dir), PushStateRead::Present(_)));
    }

    /// An unreadable file does not destroy the other rows silently: the
    /// write proceeds (the alternative would lose this push's fact forever)
    /// and the reader gets a rebuilt file holding what this run knows.
    #[test]
    fn an_unreadable_file_is_rebuilt_rather_than_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(push_state_path(&state_dir), "corrupted").unwrap();
        record_success(&state_dir, "d", "m", 1, 5).unwrap();
        match load(&state_dir) {
            PushStateRead::Present(state) => assert_eq!(state.pushes.len(), 1),
            other => panic!("expected a rebuilt present state, got {other:?}"),
        }
    }
}
