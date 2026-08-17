//! Read-only collection of scanner records into sealed local stage shards.
//!
//! A harness file is never renamed, opened for writing, or marked in the
//! harness directory. The collector keeps its byte offset and a SHA-256 of
//! the committed prefix in a state file under chat-stasher's own data
//! directory. Only bytes after a matching prefix are considered new. A
//! mismatch (shorter file or changed committed prefix) resets the read to
//! byte zero: that deliberately prefers measurable duplicate reads over a
//! silent omission.

use crate::config::Config;
use crate::models::SessionRecord;
use crate::scanner;
use crate::store;
use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const STATE_VERSION: u32 = 1;
const STATE_FILE: &str = "offsets-v1.json";
const READ_RETRIES: usize = 3;

/// Durable cursor for one source file. `prefix_len` is intentionally repeated
/// alongside `offset`: the state is self-describing and a partially written
/// or hand-edited state cannot silently widen the reusable prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OffsetEntry {
    pub offset: u64,
    pub prefix_len: u64,
    pub prefix_sha256: String,
    pub compressed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OffsetState {
    version: u32,
    files: BTreeMap<String, OffsetEntry>,
}

/// Metadata-only result of one collected source. The source path is represented
/// by a digest so CLI output cannot disclose a harness directory name.
#[derive(Debug, Clone)]
pub struct CollectOutcome {
    pub session_prefix: String,
    pub source_path_sha256: String,
    pub source_bytes: u64,
    pub bytes_read: u64,
    pub prefix_bytes_validated: u64,
    pub lines_written: usize,
    pub shard: Option<String>,
    pub reset: bool,
    pub compressed: bool,
}

/// A source that could not be collected. Its path is represented only by a
/// digest; the command prints counts and this digest, never the source path.
#[derive(Debug, Clone)]
pub struct CollectError {
    pub session_prefix: String,
    pub source_path_sha256: String,
}

/// A state/stage mismatch repaired by forcing this session through the normal
/// reset read path. The reason is intentionally fixed metadata, never source
/// content or a real harness path.
#[derive(Debug, Clone)]
pub struct ReconcileNotice {
    pub session_prefix: String,
    pub reason: &'static str,
}

/// Complete metadata-only summary of one `collect` pass.
#[derive(Debug, Clone, Default)]
pub struct CollectReport {
    pub scanned_records: usize,
    /// Harnesses with recognised sessions that produced fewer
    /// `SessionRecord`s; these sessions were not consumed by this pass.
    pub archive_gaps: Vec<scanner::ArchiveGap>,
    pub changed_records: usize,
    pub unchanged_records: usize,
    pub reset_records: usize,
    pub shards_written: usize,
    pub lines_written: usize,
    pub source_bytes_read: u64,
    pub delta_bytes_read: u64,
    pub prefix_bytes_validated: u64,
    pub outcomes: Vec<CollectOutcome>,
    pub errors: Vec<CollectError>,
    pub reconciliations: Vec<ReconcileNotice>,
}

/// Default private state directory. It is owned by chat-stasher, never a
/// harness directory and never inside the stage tree that `push` archives.
pub fn default_state_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(xdg).join("chat-stasher").join("state")
    } else {
        crate::config::home_dir()
            .join(".local")
            .join("share")
            .join("chat-stasher")
            .join("state")
    }
}

/// Metadata-only evidence used by `push` before it decides what an empty
/// stage means. A committed read is an offset entry whose durable offset is
/// greater than zero; zero-byte sources do not count as read content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PushStageCheck {
    pub stage_shards: usize,
    pub scanner_records: usize,
    pub scanner_sqlite_sessions: u64,
    pub scanner_sqlite_unknown: usize,
    pub committed_reads: usize,
}

impl PushStageCheck {
    /// An empty stage is a normal no-op only when every metadata source agrees
    /// that there is nothing to archive. Any positive signal is conservative:
    /// the caller must keep the existing failure path instead of creating an
    /// empty snapshot.
    pub fn empty_stage_is_safe(&self) -> bool {
        self.stage_shards == 0
            && self.scanner_records == 0
            && self.scanner_sqlite_sessions == 0
            && self.scanner_sqlite_unknown == 0
            && self.committed_reads == 0
    }
}

/// Collect the metadata needed to distinguish a genuinely new/empty machine
/// from a stage that disappeared after collection. This reads registry
/// metadata and the collector cursor only; it never reads session bodies.
pub fn inspect_stage_for_push(
    config: &Config,
    stage: &Path,
    state_dir: &Path,
) -> anyhow::Result<PushStageCheck> {
    let scan = scanner::scan(config).context("scan harness sessions for empty-stage guard")?;
    let state = load_state(&state_dir.join(STATE_FILE))?;
    let scanner_sqlite_sessions = scan
        .probes
        .iter()
        .filter(|probe| matches!(probe.state, scanner::ProbeState::FileTarget))
        .filter_map(|probe| probe.record_count)
        .sum();
    let scanner_sqlite_unknown = scan
        .probes
        .iter()
        .filter(|probe| matches!(probe.state, scanner::ProbeState::FileTarget))
        .filter(|probe| probe.record_count.is_none())
        .count();
    Ok(PushStageCheck {
        stage_shards: store::sealed_shard_count(stage)?,
        scanner_records: scan.records.len(),
        scanner_sqlite_sessions,
        scanner_sqlite_unknown,
        committed_reads: state
            .files
            .values()
            .filter(|entry| entry.offset > 0)
            .count(),
    })
}

/// Scan every registry record and incrementally stage it.
pub fn collect(
    config: &Config,
    stage: &Path,
    machine: &str,
    state_dir: &Path,
    bucket_cap: usize,
) -> anyhow::Result<CollectReport> {
    let scan = scanner::scan(config).context("scan harness sessions")?;
    collect_scan_report(&scan, stage, machine, state_dir, bucket_cap)
}

/// Collection entry point split out for tests: the scanner report is supplied
/// by a synthetic registry, while the real CLI always calls [`collect`].
pub fn collect_scan_report(
    scan: &scanner::ScanReport,
    stage: &Path,
    machine: &str,
    state_dir: &Path,
    bucket_cap: usize,
) -> anyhow::Result<CollectReport> {
    store::assert_stage_writer_audited(store::StageWriter::Collect)?;
    fs::create_dir_all(stage).with_context(|| format!("create stage {}", stage.display()))?;
    fs::create_dir_all(state_dir)
        .with_context(|| format!("create collector state {}", state_dir.display()))?;
    let state_path = state_dir.join(STATE_FILE);
    let mut state = load_state(&state_path)?;
    let mut report = CollectReport {
        scanned_records: scan.records.len(),
        archive_gaps: scan.archive_gaps(),
        ..CollectReport::default()
    };

    let mut records = scan.records.clone();
    records.sort_by(|a, b| a.absolute_path.cmp(&b.absolute_path));
    for record in records {
        let key = source_key(&record.absolute_path);
        let old = state.files.get(&key).cloned();
        let missing_stage_shard = old.as_ref().is_some_and(|entry| entry.offset > 0)
            && store::sealed_shard_entries(&store::session_shard_dir(stage, machine, &record.id))?
                .is_empty();
        if missing_stage_shard {
            report.reconciliations.push(ReconcileNotice {
                session_prefix: id_prefix(&record.id),
                reason: "state offset > 0 but stage has no sealed shard",
            });
        }
        match collect_one(
            &record,
            old.as_ref(),
            missing_stage_shard,
            stage,
            machine,
            bucket_cap,
        ) {
            Ok(processed) => {
                let outcome = processed.outcome;
                let changed = outcome.bytes_read > 0 || outcome.reset;
                if changed {
                    report.changed_records += 1;
                } else {
                    report.unchanged_records += 1;
                }
                if outcome.reset {
                    report.reset_records += 1;
                }
                report.source_bytes_read += outcome.bytes_read;
                report.delta_bytes_read += outcome.bytes_read;
                report.prefix_bytes_validated += outcome.prefix_bytes_validated;
                if outcome.shard.is_some() {
                    report.shards_written += 1;
                }
                report.lines_written += outcome.lines_written;

                if old.as_ref() != Some(&processed.state) {
                    state.files.insert(key, processed.state);
                    save_state(&state_path, &state)?;
                }
                report.outcomes.push(outcome);
            }
            Err(_) => report.errors.push(CollectError {
                session_prefix: id_prefix(&record.id),
                source_path_sha256: path_digest(&record.absolute_path),
            }),
        }
    }
    Ok(report)
}

#[derive(Debug)]
struct ReadData {
    source_len: u64,
    base_offset: u64,
    bytes: Vec<u8>,
    bytes_read: u64,
    prefix_bytes_validated: u64,
    reset: bool,
}

#[derive(Debug)]
struct Processed {
    outcome: CollectOutcome,
    state: OffsetEntry,
}

fn collect_one(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
) -> anyhow::Result<Processed> {
    if record.compressed || is_zstd_path(&record.absolute_path) {
        Ok(process_compressed(
            record,
            old,
            force_reset,
            stage,
            machine,
            bucket_cap,
        )?)
    } else if is_jsonl_path(&record.absolute_path) {
        Ok(process_jsonl(
            record,
            old,
            force_reset,
            stage,
            machine,
            bucket_cap,
        )?)
    } else {
        Ok(process_whole_file(
            record,
            old,
            force_reset,
            stage,
            machine,
            bucket_cap,
        )?)
    }
}

fn process_jsonl(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
) -> anyhow::Result<Processed> {
    let data = read_jsonl_delta(&record.absolute_path, old, force_reset)?;
    let (lines, committed_delta) = complete_lines(&data.bytes);
    let new_offset = data.base_offset + committed_delta as u64;
    let new_state = if lines.is_empty() && !data.reset {
        old.cloned().unwrap_or(OffsetEntry {
            offset: 0,
            prefix_len: 0,
            prefix_sha256: sha256_hex(&[]),
            compressed: false,
        })
    } else {
        plain_state(&record.absolute_path, new_offset)?
    };
    let shard = if lines.is_empty() {
        None
    } else {
        Some(store::write_sealed_shard_bytes_with_cap(
            store::StageWriter::Collect,
            stage,
            machine,
            &record.id,
            &lines,
            bucket_cap,
        )?)
    };
    Ok(Processed {
        state: new_state,
        outcome: CollectOutcome {
            session_prefix: id_prefix(&record.id),
            source_path_sha256: path_digest(&record.absolute_path),
            source_bytes: data.source_len,
            bytes_read: data.bytes_read,
            prefix_bytes_validated: data.prefix_bytes_validated,
            lines_written: lines.len(),
            shard,
            reset: data.reset,
            compressed: false,
        },
    })
}

fn process_whole_file(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
) -> anyhow::Result<Processed> {
    let bytes = fs::read(&record.absolute_path)
        .with_context(|| format!("read source bytes ({})", path_digest(&record.absolute_path)))?;
    let source_len = bytes.len() as u64;
    let digest = sha256_hex(&bytes);
    if !force_reset
        && old.is_some_and(|entry| {
            !entry.compressed
                && entry.offset == source_len
                && entry.prefix_len == source_len
                && entry.prefix_sha256 == digest
        })
    {
        return Ok(Processed {
            state: old.expect("checked above").clone(),
            outcome: unchanged_outcome(record, source_len, false),
        });
    }
    let lines = if bytes.is_empty() {
        Vec::new()
    } else {
        vec![bytes]
    };
    let shard = if lines.is_empty() {
        None
    } else {
        Some(store::write_sealed_shard_bytes_with_cap(
            store::StageWriter::Collect,
            stage,
            machine,
            &record.id,
            &lines,
            bucket_cap,
        )?)
    };
    let reset = old.is_some();
    Ok(Processed {
        state: OffsetEntry {
            offset: source_len,
            prefix_len: source_len,
            prefix_sha256: digest,
            compressed: false,
        },
        outcome: CollectOutcome {
            session_prefix: id_prefix(&record.id),
            source_path_sha256: path_digest(&record.absolute_path),
            source_bytes: source_len,
            bytes_read: source_len,
            prefix_bytes_validated: 0,
            lines_written: lines.len(),
            shard,
            reset,
            compressed: false,
        },
    })
}

fn process_compressed(
    record: &SessionRecord,
    old: Option<&OffsetEntry>,
    force_reset: bool,
    stage: &Path,
    machine: &str,
    bucket_cap: usize,
) -> anyhow::Result<Processed> {
    let compressed = fs::read(&record.absolute_path).with_context(|| {
        format!(
            "read compressed source ({})",
            path_digest(&record.absolute_path)
        )
    })?;
    let source_len = compressed.len() as u64;
    let digest = sha256_hex(&compressed);
    if !force_reset
        && old.is_some_and(|entry| {
            entry.compressed
                && entry.offset == source_len
                && entry.prefix_len == source_len
                && entry.prefix_sha256 == digest
        })
    {
        return Ok(Processed {
            state: old.expect("checked above").clone(),
            outcome: unchanged_outcome(record, source_len, true),
        });
    }
    let decoded = zstd::stream::decode_all(&compressed[..]).context("decompress jsonl.zst")?;
    let (lines, _) = complete_lines(&decoded);
    let shard = if lines.is_empty() {
        None
    } else {
        Some(store::write_sealed_shard_bytes_with_cap(
            store::StageWriter::Collect,
            stage,
            machine,
            &record.id,
            &lines,
            bucket_cap,
        )?)
    };
    let state = if lines.is_empty() {
        OffsetEntry {
            offset: 0,
            prefix_len: 0,
            prefix_sha256: sha256_hex(&[]),
            compressed: true,
        }
    } else {
        OffsetEntry {
            offset: source_len,
            prefix_len: source_len,
            prefix_sha256: digest,
            compressed: true,
        }
    };
    Ok(Processed {
        state,
        outcome: CollectOutcome {
            session_prefix: id_prefix(&record.id),
            source_path_sha256: path_digest(&record.absolute_path),
            source_bytes: source_len,
            bytes_read: source_len,
            prefix_bytes_validated: 0,
            lines_written: lines.len(),
            shard,
            reset: old.is_some(),
            compressed: true,
        },
    })
}

fn unchanged_outcome(record: &SessionRecord, source_len: u64, compressed: bool) -> CollectOutcome {
    CollectOutcome {
        session_prefix: id_prefix(&record.id),
        source_path_sha256: path_digest(&record.absolute_path),
        source_bytes: source_len,
        bytes_read: 0,
        prefix_bytes_validated: source_len,
        lines_written: 0,
        shard: None,
        reset: false,
        compressed,
    }
}

fn read_jsonl_delta(
    path: &Path,
    old: Option<&OffsetEntry>,
    force_reset: bool,
) -> anyhow::Result<ReadData> {
    for _ in 0..READ_RETRIES {
        let before = fs::metadata(path)?.len();
        let reusable = old.filter(|entry| {
            !force_reset
                && !entry.compressed
                && entry.prefix_len == entry.offset
                && entry.offset <= before
        });
        if let Some(entry) = reusable {
            let prefix = read_range(path, 0, entry.offset)?;
            let expected = entry.prefix_sha256.clone();
            if sha256_hex(&prefix) == expected {
                let delta = read_range(path, entry.offset, before - entry.offset)?;
                let after = fs::metadata(path)?.len();
                if after != before {
                    continue;
                }
                // Re-check the committed prefix after reading the delta. If a
                // writer rewrote the prefix during this pass, retry instead
                // of combining bytes from two different file versions.
                let prefix_after = read_range(path, 0, entry.offset)?;
                if sha256_hex(&prefix_after) != expected {
                    continue;
                }
                return Ok(ReadData {
                    source_len: before,
                    base_offset: entry.offset,
                    bytes: delta,
                    bytes_read: before - entry.offset,
                    prefix_bytes_validated: entry.offset,
                    reset: false,
                });
            }
        }

        // The file is shorter or the committed prefix changed. Read the
        // complete current snapshot and commit only complete lines from it.
        let full = read_range(path, 0, before)?;
        let after = fs::metadata(path)?.len();
        if after != before {
            continue;
        }
        return Ok(ReadData {
            source_len: before,
            base_offset: 0,
            bytes: full,
            bytes_read: before,
            prefix_bytes_validated: reusable.map(|entry| entry.offset).unwrap_or(0),
            reset: old.is_some(),
        });
    }
    Err(anyhow!(
        "source changed during three consistent-read attempts"
    ))
}

fn complete_lines(bytes: &[u8]) -> (Vec<Vec<u8>>, usize) {
    let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return (Vec::new(), 0);
    };
    let complete_len = last_newline + 1;
    let mut lines: Vec<Vec<u8>> = bytes[..complete_len]
        .split(|byte| *byte == b'\n')
        .map(|line| line.to_vec())
        .collect();
    // `split` returns one empty item after the final delimiter; that delimiter
    // is the boundary, not another record. Empty records between two newlines
    // remain in the vector and are preserved.
    lines.pop();
    (lines, complete_len)
}

fn plain_state(path: &Path, offset: u64) -> anyhow::Result<OffsetEntry> {
    let prefix = read_range(path, 0, offset)?;
    Ok(OffsetEntry {
        offset,
        prefix_len: offset,
        prefix_sha256: sha256_hex(&prefix),
        compressed: false,
    })
}

fn load_state(path: &Path) -> anyhow::Result<OffsetState> {
    match fs::read(path) {
        Ok(bytes) => {
            let state: OffsetState = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse collector state {}", path.display()))?;
            if state.version != STATE_VERSION {
                return Err(anyhow!(
                    "unsupported collector state version {}",
                    state.version
                ));
            }
            Ok(state)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(OffsetState {
            version: STATE_VERSION,
            files: BTreeMap::new(),
        }),
        Err(e) => Err(e).with_context(|| format!("read collector state {}", path.display())),
    }
}

fn save_state(path: &Path, state: &OffsetState) -> anyhow::Result<()> {
    let tmp = path.with_file_name(format!(".{STATE_FILE}.tmp"));
    let bytes = serde_json::to_vec_pretty(state).context("serialise collector state")?;
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

fn source_key(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn path_digest(path: &Path) -> String {
    sha256_hex(source_key(path).as_bytes())
}

fn id_prefix(id: &str) -> String {
    id.chars().take(8).collect()
}

fn is_jsonl_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

fn is_zstd_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zst"))
}

fn read_range(path: &Path, start: u64, len: u64) -> anyhow::Result<Vec<u8>> {
    let len: usize = len
        .try_into()
        .map_err(|_| anyhow!("source range is too large for this process"))?;
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0u8; len];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
