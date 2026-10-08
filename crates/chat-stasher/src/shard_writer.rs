//! Shared shard allocation and installation, with each caller's existing policy.
//!
//! Locks and acknowledgements remain with the callers. In particular, inbox
//! writes prove directory durability before the sink returns to its caller;
//! collection and stage-local rename keep their historical fsync behavior.

use crate::store::{self, StageWriter};
use anyhow::Context;
use std::{fs, io::Write, path::Path};

/// Leftover temp name pattern for a half-written inbox shard.
const INBOX_TMP_SUFFIX: &str = ".jsonl.tmp";

/// The source determines framing, replay, temporary-file and durability policy.
pub(crate) enum ShardSource<'a> {
    Inbox(&'a [String]),
    Store {
        raw: &'a [u8],
        writer: StageWriter,
        allow_exact_repeat: bool,
    },
    Active {
        path: &'a Path,
        raw: &'a [u8],
    },
}

/// An allocated sequence is returned directly; existing names use the legacy
/// parser. A fresh allocation can exceed that parser's six-digit name limit.
pub(crate) enum ShardWrite {
    Existing(String),
    Allocated(u64),
}

impl ShardWrite {
    pub(crate) fn filename(self) -> String {
        match self {
            Self::Existing(name) => name,
            Self::Allocated(seq) => store::shard_filename(seq),
        }
    }

    pub(crate) fn sequence(self) -> Option<u64> {
        match self {
            Self::Existing(name) => store::parse_shard_seq(&name),
            Self::Allocated(seq) => Some(seq),
        }
    }
}

/// Allocate and install a shard without changing the caller's lock boundary.
pub(crate) fn write_shard(
    stage: &Path,
    machine: &str,
    id: &str,
    bucket_cap: usize,
    source: ShardSource<'_>,
) -> anyhow::Result<ShardWrite> {
    let framed;
    let raw = match &source {
        ShardSource::Inbox(lines) => {
            crate::test_identity_guard::refuse_fixture_write(&[id], stage)?;
            framed = lines
                .iter()
                .flat_map(|line| line.as_bytes().iter().copied().chain([b'\n']))
                .collect::<Vec<_>>();
            framed.as_slice()
        }
        ShardSource::Store { raw, .. } | ShardSource::Active { raw, .. } => raw,
    };
    let dir = store::session_shard_dir(stage, machine, id);
    // Active-file sealing historically creates the bucket only after dedup.
    if !matches!(source, ShardSource::Active { .. }) {
        fs::create_dir_all(&dir)?;
    }
    // Restore reproduces the archive's physical shard set, including repeated
    // historical shards. A verified source delta may also repeat earlier bytes.
    let dedup = !matches!(
        source,
        ShardSource::Store {
            writer: StageWriter::Restore,
            ..
        } | ShardSource::Store {
            allow_exact_repeat: true,
            ..
        }
    );
    if dedup {
        if let Some(existing) = store::find_duplicate_shard(stage, machine, id, raw)? {
            return Ok(ShardWrite::Existing(existing));
        }
    }
    if matches!(source, ShardSource::Inbox(_)) {
        clean_stale_tmp(&dir)?;
    }
    let seq = store::next_shard_seq(stage, machine, id)?;
    let path = store::shard_path_with_cap(stage, machine, id, seq, bucket_cap);
    let parent = path.parent().expect("shard path has bucket parent");
    fs::create_dir_all(parent)?;
    if let ShardSource::Active { path: active, .. } = source {
        if path.exists() {
            anyhow::bail!("seal target already exists: {}", path.display());
        }
        fs::rename(active, &path)
            .with_context(|| format!("seal rename {} -> {}", active.display(), path.display()))?;
        note_install_provenance(stage, machine, raw);
        return Ok(ShardWrite::Allocated(seq));
    }
    let inbox = matches!(source, ShardSource::Inbox(_));
    let tmp = if inbox {
        parent.join(format!("{seq:06}{INBOX_TMP_SUFFIX}"))
    } else {
        path.with_file_name(format!(".{}tmp", store::shard_filename(seq)))
    };
    if !inbox && tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    let mut f = if inbox {
        fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?
    } else {
        fs::File::create(&tmp)?
    };
    match source {
        ShardSource::Inbox(lines) => {
            for line in lines {
                f.write_all(line.as_bytes())?;
                f.write_all(b"\n")?;
            }
            f.sync_all().context("sync temp shard")?;
        }
        ShardSource::Store { raw, .. } => {
            f.write_all(raw)?;
            f.sync_all()?;
        }
        ShardSource::Active { .. } => unreachable!("active source returned after rename"),
    }
    drop(f);
    if !inbox && path.exists() {
        anyhow::bail!("sealed shard target already exists: {}", path.display());
    }
    let renamed = fs::rename(&tmp, &path);
    if inbox {
        renamed
            .with_context(|| format!("seal shard {} (rename {})", path.display(), tmp.display()))?;
        // B74 / ADR-025: this directory fsync proves the shard rename durable
        // before the sink may acknowledge or retire the inbox item. Failure
        // propagates, leaving the input available for a content-addressed retry.
        // Retirement's directory fsync is best-effort: losing that rename only
        // causes a duplicate on retry. Opening a directory is a Unix idiom;
        // Windows retains its existing behavior without a directory fsync.
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .with_context(|| {
                format!(
                    "prove sealed shard durable (fsync dir {})",
                    parent.display()
                )
            })?;
    } else {
        renamed?;
    }
    note_install_provenance(stage, machine, raw);
    Ok(ShardWrite::Allocated(seq))
}

/// Keep the install-provenance index ahead of the tree this shard write just
/// changed (W930).
///
/// Every producer funnels through [`write_shard`] — the host's `deliver`,
/// `ingest`, `import`, the collectors' active-file sealing, restore included —
/// and that is the property the index's mtime gate rests on: a session tree
/// entry newer than the index can only be a write that did not come from this
/// tool, which is exactly the out-of-band change the gate re-reads the stage
/// for. A collector's record carries no install identity, so its note is a
/// renewal of the index's mtime (the content already describes the tree);
/// an identity the index does not hold is appended, and one it already holds
/// renews the mtime as well — an index left behind on the second delivery of
/// one install was the bug that turned every delivery into a full-stage
/// read on a 12 GB stage.
///
/// Best-effort by contract and never a reason to fail a durable shard write:
/// the stage is the archive and the index is derived from it, so a lost note
/// costs the next question one re-read, never a shard.
fn note_install_provenance(stage: &Path, machine: &str, raw: &[u8]) {
    crate::install_provenance::note_shard_written(stage, machine, raw);
}

/// Remove shard temp stubs left by an interrupted seal.
fn clean_stale_tmp(dir: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() {
            clean_stale_tmp(&entry.path())?;
        } else if name.ends_with(INBOX_TMP_SUFFIX) {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "Stale temporary-file cleanup is intentionally best-effort; directory traversal errors still propagate."
            )]
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}
