//! Delivery operations for waiting inbox bundles. Native messaging feeds the
//! sink directly and does not use this polling transport (ADR-050 D1).

use anyhow::Context;
use std::{
    fs,
    path::{Path, PathBuf},
};

const CONSUMED_DIR: &str = "consumed";
const PART_SUFFIX: &str = ".part";

pub struct ListedBundle<I> {
    pub name: String,
    pub item: I,
}

/// A listing carries stat failures as well as candidates: unknown is not empty.
pub struct BundleListing<I> {
    pub items: Vec<ListedBundle<I>>,
    pub part_files_seen: usize,
    pub total_inbox_files: usize,
    pub errors: Vec<(String, String)>,
}

impl<I> Default for BundleListing<I> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            part_files_seen: 0,
            total_inbox_files: 0,
            errors: Vec::new(),
        }
    }
}

/// The sink must finish sealing every payload in an item before retiring it.
pub trait BundleTransport {
    type Item;
    fn list(&self) -> anyhow::Result<BundleListing<Self::Item>>;
    fn fetch(&self, item: &Self::Item) -> anyhow::Result<Vec<u8>>;
    fn retire(&self, name: &str, item: &Self::Item) -> anyhow::Result<()>;
}

/// Existing local inbox semantics, including exports, .part files and symlinks.
pub struct LocalFolder {
    inbox: PathBuf,
    consumed_dir: PathBuf,
}

impl LocalFolder {
    pub fn new(inbox: &Path) -> anyhow::Result<Self> {
        let consumed_dir = inbox.join(CONSUMED_DIR);
        fs::create_dir_all(&consumed_dir)
            .with_context(|| format!("create {}", consumed_dir.display()))?;
        Ok(Self {
            inbox: inbox.to_path_buf(),
            consumed_dir,
        })
    }
}

impl BundleTransport for LocalFolder {
    type Item = PathBuf;
    fn list(&self) -> anyhow::Result<BundleListing<PathBuf>> {
        let mut listing = BundleListing::default();
        let mut candidates: Vec<PathBuf> = Vec::new();
        let mut metadata_errors = 0usize;

        for entry in fs::read_dir(&self.inbox)
            .with_context(|| format!("read inbox {}", self.inbox.display()))?
        {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == CONSUMED_DIR {
                continue; // our own retirement dir, never rescanned
            }
            match fs::metadata(entry.path()) {
                Ok(m) if m.is_file() => {}
                Ok(_) => continue, // subdirectories are not inbox candidates
                Err(e) => {
                    // A failed stat leaves the entry's kind unknown; skipping it
                    // would make the remaining candidate count look complete.
                    metadata_errors += 1;
                    listing
                        .errors
                        .push((name, format!("metadata unreadable: {e}")));
                    continue;
                }
            }
            if name.ends_with(PART_SUFFIX) {
                listing.part_files_seen += 1;
                continue; // two-phase ext, mid-write
            }
            candidates.push(entry.path());
        }
        candidates.sort(); // deterministic seq order across runs
        listing.total_inbox_files = candidates.len() + metadata_errors;

        listing.items = candidates
            .into_iter()
            .map(|path| {
                let name = path
                    .file_name()
                    // reason: directory entries always have a file name; keep the existing empty fallback.
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                ListedBundle { name, item: path }
            })
            .collect();
        Ok(listing)
    }
    fn fetch(&self, item: &PathBuf) -> anyhow::Result<Vec<u8>> {
        fs::read(item).with_context(|| format!("read {}", item.display()))
    }
    fn retire(&self, name: &str, item: &PathBuf) -> anyhow::Result<()> {
        retire_file(name, item, &self.consumed_dir)
    }
}

/// Rename a consumed inbox file into `<inbox>/consumed/` (atomic same-fs).
fn retire_file(name: &str, src: &Path, consumed_dir: &Path) -> anyhow::Result<()> {
    let dst = consumed_dir.join(name);
    if dst.exists() {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "Removing an existing destination is best-effort; the required rename below reports whether retirement succeeded."
        )]
        let _ = fs::remove_file(&dst);
    }
    fs::rename(src, &dst)
        .with_context(|| format!("retire {} -> {}", src.display(), dst.display()))?;
    // B74: deliberately best-effort, and deliberately *not* the same rule as
    // the shard writer's seal. If this rename is lost to a power cut the file is
    // simply back in the inbox, and the next run reads it, matches its
    // `fileSha256` against the already-sealed shard and reports a duplicate —
    // nothing is lost, nothing is written twice. Guarding an idempotent,
    // self-healing step with a fatal error would block a user over something a
    // re-run already fixes, so the cost of the swallowed Result here is one
    // redundant re-read, not a durability lie.
    fsync_dir(consumed_dir).ok();
    Ok(())
}

/// Best-effort directory fsync (durability of renames).
fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    let f = fs::File::open(dir)?;
    f.sync_all()
}
