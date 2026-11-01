//! Per-shard digest records, written at seal time next to the shards they
//! describe (SYNC-D groundwork for the incremental activity index).
//!
//! A sealed shard is written once and never rewritten or appended to
//! (`shard_writer`), so a digest computed by that same writer over the bytes
//! it just installed keeps describing the file for as long as the file
//! exists. The activity index uses these records as its change signal between
//! builds: a smaller-cost scheme keyed on size or mtime cannot see a
//! same-size rewrite, and a record re-hashed on every read would cost what
//! the increment exists to remove — so the trust lives here, at the write,
//! where the bytes are already in hand.
//!
//! The file is deliberately NOT named `*.jsonl`: archive-side shard walks
//! match on the `NNNNNN.jsonl` *suffix*, and `parse_shard_seq` ignores any
//! other shape, so `shard-facts` is invisible to every shard enumerator.
//! Like `shard-seq`, it lives in the session directory beside the body it
//! describes.
//!
//! The ADR-034 body-cache stance is inherited deliberately: the record file
//! is disposable and never authority. A missing, unreadable or damaged file
//! does not fail a build — [`read_in_dir`] answers `None` and the reader
//! falls back to reading the shard bodies in full. A stale row that claims
//! to describe bytes it did not measure is the one failure this design
//! refuses to produce.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

/// Name of the per-session record file, in the session directory beside the
/// shards. The name's shape keeps it invisible to shard enumeration
/// (`parse_shard_seq`), to the archive-side suffix walkers, and to `extra-*`
/// sidecar handling in `stagereclaim` (which removes it by name instead).
pub const FILE_NAME: &str = "shard-facts";

/// One written shard's identity: its sequence number, its byte length and
/// the sha256 of its raw bytes, as recorded by the writer that installed it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShardFact {
    pub seq: u64,
    pub bytes: u64,
    pub sha256: String,
}

/// The digest `{}`-formatted the way the rest of the stage records digests:
/// lowercase hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The fact for a shard whose raw `bytes` were just installed at `seq`.
pub fn fact_for(seq: u64, bytes: &[u8]) -> ShardFact {
    ShardFact {
        seq,
        bytes: bytes.len() as u64,
        sha256: hex(&Sha256::digest(bytes)),
    }
}

/// Record one shard's fact, merging into the session directory's existing
/// record file, atomically (temp file → sync → rename).
///
/// Best-effort by design: the shard is already durable when this runs, and a
/// record that never landed only costs the next activity-index build a body
/// re-read, so a failure to record must not turn a durable seal into a failed
/// write. The caller drops the error.
pub fn record_in_dir(dir: &Path, fact: &ShardFact) -> std::io::Result<()> {
    // reason: an existing record that cannot be read is treated as no
    // record at all and rebuilt from this fact alone — never partially
    // merged, and never trusted to describe shards it cannot name.
    let mut entries: BTreeMap<u64, ShardFact> = read_in_dir(dir).unwrap_or_default();
    entries.insert(fact.seq, fact.clone());
    let mut body = String::new();
    for entry in entries.values() {
        body.push_str(
            &serde_json::to_string(entry)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?,
        );
        body.push('\n');
    }
    // The same durability contract `manifest::write_manifest` and
    // `store::persist_shard_seq_in_dir` hold: a complete body written to a
    // sibling temp file, fsynced, then renamed over the target, so a crash
    // can only leave the previous complete record.
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{FILE_NAME}.tmp"));
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, dir.join(FILE_NAME))?;
    Ok(())
}

/// The session directory's recorded facts, keyed by sequence number.
///
/// `None` — no file, or any line of it unreadable or unparseable — means the
/// reader knows nothing about the recorded identity of these shards and must
/// obtain it from the bodies themselves. That is the ADR-034 disposable
/// stance: a damaged cache entry degrades to a full read, never to a shorter
/// answer.
pub fn read_in_dir(dir: &Path) -> Option<BTreeMap<u64, ShardFact>> {
    let raw = match fs::read_to_string(dir.join(FILE_NAME)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return None,
    };
    let mut facts: BTreeMap<u64, ShardFact> = BTreeMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Any damaged line forfeits the whole file: a partially trusted
        // record is exactly the "stale row with a plausible face" the index
        // refuses to produce.
        let fact: ShardFact = serde_json::from_str(line).ok()?;
        facts.insert(fact.seq, fact);
    }
    Some(facts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_read_round_trips_in_sequence_order() {
        let holder = tempfile::tempdir().unwrap();
        let dir = holder.path();
        assert!(
            read_in_dir(dir).is_none(),
            "no record file yet means no recorded identity"
        );
        record_in_dir(dir, &fact_for(1, b"alpha")).unwrap();
        record_in_dir(dir, &fact_for(2, b"beta")).unwrap();
        let facts = read_in_dir(dir).expect("record file must read back");
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[&1], fact_for(1, b"alpha"));
        assert_eq!(facts[&2], fact_for(2, b"beta"));
    }

    #[test]
    fn a_damaged_line_forfeits_the_whole_file() {
        let holder = tempfile::tempdir().unwrap();
        let dir = holder.path();
        record_in_dir(dir, &fact_for(1, b"alpha")).unwrap();
        fs::write(dir.join(FILE_NAME), "{\"seq\":1,\"bytes\":9\n{\"broken\"\n").unwrap();
        assert!(
            read_in_dir(dir).is_none(),
            "a damaged record must read as no identity at all, never as a partial one"
        );
    }
}
