//! Durable, rebuildable audit sidecars. The caller retains one archive-scoped
//! join secret outside the stage and uses it on every participating machine.
//! Missing configuration means not built; unreadable configuration is an error.
use crate::message_audit::{self, BodyMetadata, JoinPolicy, MetadataSource, Projection};
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::{fs, io::Write, path::Path};

pub const KEY_FILE_ENV: &str = "CHAT_STASHER_AUDIT_KEY_FILE";

/// Read exactly 32 secret bytes. Never log the value, derive it from public IDs,
/// generate a replacement for a missing file, or copy it into the stage.
pub fn read_policy(path: &Path) -> Result<JoinPolicy> {
    let file = fs::File::open(path).map_err(|_| anyhow::anyhow!("audit join key is unreadable"))?;
    ensure!(
        file.metadata()?.len() == 32,
        "audit join key must contain exactly 32 bytes"
    );
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(33)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("audit join key is unreadable"))?;
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("audit join key must contain exactly 32 bytes"))?;
    Ok(JoinPolicy::new(key))
}

pub fn configured_policy(stage: &Path) -> Result<Option<JoinPolicy>> {
    let Some(path) = std::env::var_os(KEY_FILE_ENV) else {
        return Ok(None);
    };
    let path = Path::new(&path);
    let key_path =
        fs::canonicalize(path).map_err(|_| anyhow::anyhow!("audit join key is unreadable"))?;
    let stage_path = fs::canonicalize(stage).context("resolve audit stage")?;
    ensure!(
        !key_path.starts_with(stage_path),
        "audit join key must be outside the stage"
    );
    read_policy(&key_path).map(Some)
}

/// Atomic replacement retains the existing prefix verbatim. A killed process
/// leaves either the old complete file or the new complete file, never a torn
/// append. The temporary file is removed by RAII on ordinary failures.
fn durable_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("audit output has no parent")?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|_| anyhow::anyhow!("install audit output failed"))?;
    // Windows has no portable directory fsync; retain its shard durability
    // convention. Unix proves the rename durable before ingestion can return.
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn append(stage: &Path, machine: &str, projection: &Projection) -> Result<()> {
    crate::test_identity_guard::refuse_fixture_write(&[machine], stage)?;
    ensure!(
        !machine.is_empty() && !machine.contains(['/', '\\']) && machine != "." && machine != "..",
        "invalid audit partition"
    );
    let _lock =
        crate::inbox::lock_stage(stage).map_err(|_| anyhow::anyhow!("audit stage lock failed"))?;
    append_locked(stage, machine, projection)
}

/// Inbox callers already hold the stage lock. Other producers take it through
/// `append`; no separate lock order or in-place JSONL append can tear the tail.
pub(crate) fn append_locked(stage: &Path, machine: &str, projection: &Projection) -> Result<()> {
    let path = stage
        .join("meta")
        .join(machine)
        .join(message_audit::FILE_NAME);
    let old = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => anyhow::bail!("audit sidecar is unreadable"),
    };
    let new = message_audit::append_jsonl(&old, projection)?;
    if new != old {
        durable_replace(&path, &new)?;
    }
    Ok(())
}

/// Decode the authoritative bytes in a sealed inbox record. The incoming
/// contract and sealed-record schema are separate; legacy rows remain valid.
pub fn record_body(record: &Value) -> Result<Vec<u8>> {
    let bytes = decode_record_body(record)?;
    if record["raw"].get("encoding").is_some() {
        use sha2::{Digest, Sha256};
        let file = &record["file"];
        let start = file["byteStart"]
            .as_f64()
            .context("invalid sealed slice start")?;
        let end = file["byteEnd"]
            .as_f64()
            .context("invalid sealed slice end")?;
        ensure!(
            start >= 0.0
                && end >= start
                && start.fract() == 0.0
                && end.fract() == 0.0
                && end - start == bytes.len() as f64,
            "invalid sealed slice bounds"
        );
        let digest = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        ensure!(
            file["sha256"].as_str() == Some(digest.as_str()),
            "sealed slice digest mismatch"
        );
    }
    Ok(bytes)
}

fn decode_record_body(record: &Value) -> Result<Vec<u8>> {
    match record["raw"]["encoding"].as_str() {
        Some("base64") => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(
                    record["raw"]["data"]
                        .as_str()
                        .context("missing sealed slice")?,
                )
                .map_err(|_| anyhow::anyhow!("invalid sealed slice encoding"))
        }
        Some("utf-8") => Ok(record["raw"]["data"]
            .as_str()
            .context("missing sealed slice")?
            .as_bytes()
            .to_vec()),
        Some(_) => anyhow::bail!("unsupported sealed slice encoding"),
        None => Ok(record["raw"]["text"]
            .as_str()
            .context("missing sealed body")?
            .as_bytes()
            .to_vec()),
    }
}

fn record_projection(record: &Value, session: &str, key: &JoinPolicy) -> Result<Projection> {
    let harness = record
        .get("harness")
        .or_else(|| record.get("platform"))
        .and_then(Value::as_str)
        .context("missing sealed harness")?;
    let mut meta = BodyMetadata::legacy(session.into(), harness.into());
    if let Some(fidelity) = record.get("fidelity") {
        meta.fidelity.source = MetadataSource::Captured;
        meta.fidelity.fidelity = serde_json::from_value(fidelity.clone())
            .map_err(|_| anyhow::anyhow!("invalid sealed fidelity"))?;
    }
    if let Some(producer) = record.get("producer") {
        meta.producer = Some(message_audit::ProducerMetadata {
            source: MetadataSource::Captured,
            producer: serde_json::from_value(producer.clone())
                .map_err(|_| anyhow::anyhow!("invalid sealed producer"))?,
        });
    }
    if record["kind"] == "harness-file" || record.get("file").is_some() {
        meta.native_session = record["nativeSessionId"].as_str().map(String::from);
        meta.subagent = Some(record["file"]["role"] == "subagent");
        meta.parent_native_session = record["file"]["parentNativeSessionId"]
            .as_str()
            .map(String::from);
    }
    message_audit::project_body(&record_body(record)?, &meta, key)
}

/// One physical sealed shard is the body unit for native captures, so ingestion
/// and migration use identical digest and position coordinates, even after
/// reclaim. Inbox shards unwrap each exact captured body instead.
pub fn project_shard(raw: &[u8], session: &str, key: &JoinPolicy) -> Result<Vec<Projection>> {
    let lines: Vec<_> = raw
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    let sealed = lines
        .first()
        .and_then(|line| serde_json::from_slice::<Value>(line).ok())
        .is_some_and(|v| v.get("file_sha256").is_some() && v.get("schema").is_some());
    if sealed {
        lines
            .into_iter()
            .map(|line| {
                let v: Value = serde_json::from_slice(line)
                    .map_err(|_| anyhow::anyhow!("unreadable sealed record"))?;
                ensure!(
                    matches!(
                        v["schema"].as_str(),
                        Some(
                            "chat-stasher/inbox@1"
                                | "chat-stasher/inbox@2"
                                | "chat-stasher/inbox@3"
                        )
                    ),
                    "unsupported sealed record version"
                );
                if v["kind"] == "harness-resend" {
                    // Provenance is not another body. Malformed references are
                    // an incomplete read, never a successful empty projection.
                    ensure!(
                        v.get("raw").is_none()
                            && v["schema"] == "chat-stasher/inbox@3"
                            && v["content_ref"].as_str().is_some_and(|sha| {
                                sha.len() == 64
                                    && sha
                                        .bytes()
                                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                            }),
                        "invalid sealed resend reference"
                    );
                    return Ok(None);
                }
                record_projection(&v, session, key).map(Some)
            })
            .collect::<Result<Vec<_>>>()
            .map(|rows| rows.into_iter().flatten().collect())
    } else {
        let harness = session.split('.').next().context("missing audit harness")?;
        Ok(vec![message_audit::project_body(
            raw,
            &BodyMetadata::legacy(session.into(), harness.into()),
            key,
        )?])
    }
}

pub(crate) fn record_shard(
    stage: &Path,
    machine: &str,
    session: &str,
    raw: &[u8],
    key: &JoinPolicy,
    locked: bool,
) -> Result<()> {
    for p in project_shard(raw, session, key)? {
        if locked {
            append_locked(stage, machine, &p)?;
        } else {
            append(stage, machine, &p)?;
        }
    }
    Ok(())
}

/// Reopen the installed source under the stage lock. This also repairs a retry
/// after the shard was installed but audit generation never completed.
pub(crate) fn record_collected_shard(
    stage: &Path,
    machine: &str,
    session: &str,
    filename: &str,
    key: &JoinPolicy,
) -> Result<()> {
    let _lock = crate::inbox::lock_stage(stage)?;
    let path = crate::store::sealed_shard_entries(&crate::store::session_shard_dir(
        stage, machine, session,
    ))?
    .into_iter()
    .find(|(_, p)| p.file_name().is_some_and(|name| name == filename))
    .map(|(_, p)| p)
    .context("audit sealed body is missing")?;
    let body = fs::read(&path).map_err(|_| anyhow::anyhow!("audit sealed body is unreadable"))?;
    // The shared shard writer synced the file before installing it. Reopening
    // read-only for a second flush is not supported by Windows.
    #[cfg(unix)]
    fs::File::open(path.parent().context("sealed shard has no parent")?)?.sync_all()?;
    record_shard(stage, machine, session, &body, key, true)
}

#[derive(Debug, Default, serde::Serialize)]
pub struct BackfillReport {
    pub shards_scanned: u64,
    pub bodies_scanned: u64,
    pub rows_recognized: u64,
    pub incomplete_extractions: u64,
}

/// Shared resumable migration engine: successful bodies commit independently;
/// retries reconcile body digest/position identities against the sidecar itself.
/// No cursor can claim coverage for bytes not read or rows not durably written.
pub fn backfill_shard(
    stage: &Path,
    machine: &str,
    session: &str,
    raw: &[u8],
    key: &JoinPolicy,
    report: &mut BackfillReport,
) -> Result<()> {
    for p in project_shard(raw, session, key)? {
        append(stage, machine, &p)?;
        report.bodies_scanned += p.outcomes.len() as u64;
        report.rows_recognized += p.rows.len() as u64;
        report.incomplete_extractions += p
            .outcomes
            .iter()
            .filter(|o| o.status != message_audit::ExtractionStatus::Complete)
            .count() as u64;
    }
    report.shards_scanned += 1;
    Ok(())
}

pub fn backfill_stage(stage: &Path, machine: &str, key: &JoinPolicy) -> Result<BackfillReport> {
    let mut report = BackfillReport::default();
    let sessions = stage.join("sessions").join(machine);
    // Missing/unreadable source is incomplete, never a completed zero-row scan.
    let mut entries = fs::read_dir(sessions)
        .map_err(|_| anyhow::anyhow!("audit migration source is unreadable or missing"))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let session = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid audit session identity"))?;
        for (_, path) in crate::store::sealed_shard_entries(&entry.path())? {
            let bytes = fs::read(path)
                .map_err(|_| anyhow::anyhow!("audit migration body is unreadable"))?;
            backfill_shard(stage, machine, &session, &bytes, key, &mut report)?;
        }
    }
    Ok(report)
}

/// Read archived shards cumulatively, including reclaimed stage history. Writes
/// only the caller's partition in its stage; no source shard or snapshot changes.
pub fn backfill_archive(
    store: &crate::store::BackupStore,
    masterkey: &rustic_core::repofile::MasterKey,
    stage: &Path,
    machine: &str,
    key: &JoinPolicy,
) -> Result<BackfillReport> {
    let mut report = BackfillReport::default();
    store.for_each_archived_session_shards_with_policy(
        masterkey,
        machine,
        crate::store::DuplicateShardPolicy::KeepAll,
        |session, shards| {
            for (_, bytes) in shards {
                backfill_shard(stage, machine, session, bytes, key, &mut report)?;
            }
            Ok(())
        },
    )?;
    Ok(report)
}
