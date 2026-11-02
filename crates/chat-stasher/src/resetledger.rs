//! W1057: the durable per-round record of why sessions were reset.
//!
//! A collect pass counts `reset=<n>` and the run log prints it, but the run
//! log is overwritten by the next round, and the count alone never said
//! *why*. This ledger keeps one row per finished collect pass in the state
//! directory, append-only, so a later round (or a weekly reclaim review)
//! can still be answered from the round that mattered.
//!
//! A row is counts and fixed words only: the harness id, a cause word from
//! `collect`'s own vocabulary, unix times, a short destination digest. It
//! never holds a session id, a title, a path or any session text. Only a
//! round with a reset, an unproven cursor or a mid-pass manifest re-read
//! writes a row: a quiet pass leaves the state directory untouched, and
//! `run-state.json` is what says a quiet round ran. A missing row is
//! therefore "nothing to report", never a measured zero of its own.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// File name under the state directory.
pub const FILE_NAME: &str = "reset-causes-v1.jsonl";
/// Row schema version.
pub const VERSION: u32 = 1;
/// Above this size the oldest rows are dropped on the next append.
const TRIM_ABOVE_BYTES: u64 = 2 * 1024 * 1024;
/// Rows kept after a trim.
const KEEP_ROWS: usize = 2000;

/// One finished collect pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Round {
    pub version: u32,
    pub cli_version: String,
    pub pass_started_unix: i64,
    pub pass_finished_unix: i64,
    /// First 12 hex characters of the destination digest.
    pub destination: String,
    pub scanned_records: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub reset: usize,
    pub unverified_cursors: usize,
    /// Rows the retained manifest held when the pass began; `None` is an
    /// absent or unreadable manifest, not an empty one.
    pub manifest_rows_at_start: Option<usize>,
    /// Times the pass re-read the manifest because its file changed mid-pass.
    pub manifest_reloads: usize,
    /// Sessions the re-read settled that the start-of-pass manifest could not.
    pub late_manifest_settled: usize,
    /// `{harness id → {cause word → count}}`.
    pub reset_causes: BTreeMap<String, BTreeMap<String, usize>>,
}

/// Where the ledger lives for `state_dir`.
pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE_NAME)
}

/// Append one row. A single `write_all` of one line to an append-mode handle,
/// so a concurrent reader sees whole rows or none.
pub fn append_round(state_dir: &Path, round: &Round) -> anyhow::Result<()> {
    let path = path(state_dir);
    let mut line = serde_json::to_vec(round)?;
    line.push(b'\n');
    trim_if_large(&path)?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open reset-cause ledger {}", path.display()))?;
    file.write_all(&line)
        .with_context(|| format!("append to reset-cause ledger {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("sync reset-cause ledger {}", path.display()))?;
    Ok(())
}

/// Read every row that parses; a row that does not is skipped, never guessed.
pub fn read_rounds(state_dir: &Path) -> anyhow::Result<Vec<Round>> {
    let path = path(state_dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<Round>(line).ok())
        .collect())
}

fn trim_if_large(path: &Path) -> anyhow::Result<()> {
    let len = match fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    if len <= TRIM_ABOVE_BYTES {
        return Ok(());
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    let keep = lines.len().saturating_sub(KEEP_ROWS);
    let mut kept = lines[keep..].join("\n");
    kept.push('\n');
    let temp = path.with_extension("jsonl.tmp");
    fs::write(&temp, kept).with_context(|| format!("write {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(reset: usize) -> Round {
        Round {
            version: VERSION,
            cli_version: "0.0.0".to_string(),
            pass_started_unix: 1,
            pass_finished_unix: 2,
            destination: "abc".to_string(),
            scanned_records: 3,
            changed: 1,
            unchanged: 2,
            reset,
            unverified_cursors: 0,
            manifest_rows_at_start: None,
            manifest_reloads: 0,
            late_manifest_settled: 0,
            reset_causes: BTreeMap::new(),
        }
    }

    #[test]
    fn rows_append_and_survive_later_rounds() {
        let dir = tempfile::TempDir::new().unwrap();
        append_round(dir.path(), &round(1)).unwrap();
        append_round(dir.path(), &round(2)).unwrap();
        let rows = read_rounds(dir.path()).unwrap();
        assert_eq!(rows.iter().map(|r| r.reset).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn a_missing_ledger_reads_as_no_rows_and_a_torn_row_is_skipped() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_rounds(dir.path()).unwrap().is_empty());
        append_round(dir.path(), &round(1)).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(path(dir.path()))
            .unwrap()
            .write_all(b"{\"version\":1,\"torn")
            .unwrap();
        assert_eq!(read_rounds(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn the_oldest_rows_go_first_when_the_file_grows_past_the_cap() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = path(dir.path());
        let mut big = String::new();
        let pad = "x".repeat(900);
        for i in 0..(KEEP_ROWS + 1500) {
            let mut r = round(i);
            r.cli_version = pad.clone();
            big.push_str(&serde_json::to_string(&r).unwrap());
            big.push('\n');
        }
        fs::write(&p, big).unwrap();
        assert!(fs::metadata(&p).unwrap().len() > TRIM_ABOVE_BYTES);
        append_round(dir.path(), &round(999_999)).unwrap();
        let rows = read_rounds(dir.path()).unwrap();
        assert_eq!(rows.len(), KEEP_ROWS + 1);
        assert_eq!(rows.last().unwrap().reset, 999_999);
        assert_eq!(rows.first().unwrap().reset, 1500);
    }
}
