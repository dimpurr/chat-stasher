//! Packs that no index file references: how a killed push strands them, what an
//! open does about them, and why that is the safe direction of the two.
//!
//! rustic writes a backup's data and tree packs first and its index file last
//! (rustic_core 0.12.0, `src/archiver.rs:218`), so a push killed in between
//! leaves complete packs that nothing points at. They are not corrupt and they
//! are not a second copy of anything: they hold the *only* ciphertext of the
//! blobs they carry, and their object ids cover a fresh AEAD nonce, so
//! re-uploading the same plaintext produces different bytes under a different id
//! and a later push can never match them by re-deriving a name. Measured cost of
//! leaving them alone: an interrupted 560 MB push re-uploaded 100 % of the
//! payload and the stranded 343 MB stayed forever (W242 §2).
//!
//! The answer here is adoption, not deletion. `Repository::to_indexed_checked()`
//! lists the packs and reads the header of any pack the index does not name, so
//! the index those packs are missing is rebuilt in memory; a later `backup` then
//! finds every one of their blobs already present (rustic_core's
//! `archiver/file_archiver.rs:154` is a plain `index.has_data` test, independent
//! of any parent snapshot) and uploads none of them. The bytes stop being waste
//! because they become the content.
//!
//! Adoption is fenced by a survey, not applied unconditionally. The checked pass
//! rebuilds the index from what the *backend lists*, and one thing the plain
//! index does that this cannot is keep an entry for a pack the backend no longer
//! has — an entry the local metadata cache can still serve
//! (`crates/chat-stasher/tests/search_cache_windows_shape_test.rs:271` pins
//! that). So an open adopts only when the backend has packs the index does not
//! name **and** the index names no pack the backend lacks. A repository whose
//! index and backend already agree — every healthy one — keeps exactly the index
//! it had before this module existed.
//!
//! Why not delete the stranded packs: `repair index` and `prune` both refuse an
//! append-only repository (rustic_core `commands/repair/index.rs:44`,
//! `commands/prune.rs:1220`) and ADR-001 opened every repository with
//! `append_only:true` on purpose, so a delete path here would be this crate
//! quietly stepping around the flag the design rests on. It is also unnecessary:
//! adoption reclaims the same bytes by making them reachable, and it can never
//! touch a pack another client is still writing, because it only ever *adds* an
//! in-memory index entry.
//!
//! The guard on a single pack is completeness, not age: a pack is adopted only
//! if its header parses and its own length and blob sum agree with the file size
//! (rustic_core `repofile/packfile.rs:293-321`), which a pack still being
//! streamed to the backend fails. Age would be the wrong guard here — it would
//! have to outlast the slowest concurrent upload (minutes for a large pack) and
//! would then refuse to reuse our own just-killed push's packs, which is the
//! whole point. Indexing a *complete* pack is idempotent whoever wrote it: two
//! clients that both index one pack write the same blobs at the same offsets,
//! which is what rustic's own `repair index` does and why its `check` labels an
//! unreferenced pack "can be a parallel backup job".
//!
//! A header is not a pack, and the checked pass reads no further than the header:
//! it parses the encrypted header and compares the lengths in it with the file
//! size, so a pack whose *blob bytes* were damaged in place — with the header and
//! the length left intact — would be indexed, and a later backup would skip
//! uploading blobs no reader can decrypt. `verify_unindexed` therefore reads
//! every byte of every pack the index does not name, before the checked pass
//! runs, and compares it with the id the pack is stored under: a pack's id is
//! the SHA-256 of its own contents (rustic_core `src/blob/packer.rs:762`), so
//! nothing enters the dedup index unless it reads back as the bytes it was
//! written as. Any failure refuses the whole adoption and is reported, never
//! absorbed — the direction that costs a re-upload and never the archive.
//!
//! `docs-dev/orphan-packs.md` carries the safety argument in full.

use crate::store::StoreConfig;
use anyhow::Context;
use rustic_core::repofile::{IndexFile, MasterKey};
use rustic_core::{
    Credentials, FileType, Id, IndexedFullStatus, Open, ReadBackend, Repository, RepositoryBackends,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// How the backend and the index files disagree.
///
/// An empty *both* is a measurement — the two agree — not a fallback: the survey
/// ran and found nothing to report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanReport {
    /// `(hex id, backend bytes)` of every pack no index file names: what a push
    /// killed between its packs and its index leaves behind. Id-sorted.
    pub unindexed: Vec<(String, u64)>,
    /// Hex ids of packs an index file names that the backend does not list.
    /// These are what the plain index can still serve out of the local metadata
    /// cache, and what the checked index would drop.
    pub missing: Vec<String>,
}

impl OrphanReport {
    /// Packs no index file names.
    #[must_use]
    pub fn unindexed_packs(&self) -> usize {
        self.unindexed.len()
    }

    /// Backend bytes those packs occupy.
    #[must_use]
    pub fn unindexed_bytes(&self) -> u64 {
        self.unindexed.iter().map(|(_, bytes)| bytes).sum()
    }
}

/// What building an index did about packs no index file references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexAdoption {
    /// The index files agreed with the backend, so the plain index was used —
    /// the open every read path made before this module existed.
    Plain,
    /// The index could not reach every pack in the backend, and the adopting
    /// index was built: those packs' blobs are visible to this open.
    Adopted,
    /// The adopting index was *not* built, so packs no index file names stay
    /// unreachable. `why` says which of the three fences stopped it: a pack that
    /// does not read back as the bytes it is stored under, an index that names a
    /// pack the backend does not have, or the checked pass failing on a pack
    /// whose header it could not use.
    Refused {
        /// Human-readable reason, naming the pack when one pack refused the
        /// open, and carrying rustic's own context chain when the checked pass
        /// itself failed.
        why: String,
    },
}

impl IndexAdoption {
    /// The refusal reason, when the adopting index could not be built.
    #[must_use]
    pub fn refused(&self) -> Option<&str> {
        match self {
            Self::Refused { why } => Some(why),
            Self::Plain | Self::Adopted => None,
        }
    }

    /// Whether this open reached the packs no index file names.
    #[must_use]
    pub fn adopted(&self) -> bool {
        matches!(self, Self::Adopted)
    }
}

/// An index over the repository, plus what one open found and did about packs
/// the index files do not name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanOutcome {
    /// What the backend held when this open started.
    pub report: OrphanReport,
    /// What the index this open built can reach. `None` only for a repository
    /// this process created, which had no packs to survey.
    pub adoption: Option<IndexAdoption>,
}

impl OrphanOutcome {
    /// The refusal reason, when this open could not adopt.
    #[must_use]
    pub fn refused(&self) -> Option<&str> {
        self.adoption.as_ref().and_then(IndexAdoption::refused)
    }

    /// Whether this open reached the packs no index file names.
    #[must_use]
    pub fn adopted(&self) -> bool {
        self.adoption.as_ref().is_some_and(IndexAdoption::adopted)
    }
}

/// Read every index file and every pack file, and diff them.
///
/// One listing plus one pass over the index; with rustic's local metadata cache
/// on (the default) that pass is served locally.
///
/// # Errors
///
/// If the backend cannot list packs, or an index file cannot be decrypted.
pub fn survey<S: Open>(
    repo: &Repository<S>,
    backends: &RepositoryBackends,
) -> anyhow::Result<OrphanReport> {
    let listed = backends
        .repository()
        .list_with_size(FileType::Pack)
        .context("list packs in the repository")?;
    let mut indexed = BTreeSet::new();
    for file in repo
        .stream_files::<IndexFile>()
        .context("stream index files")?
    {
        let (_, index) = file.context("read an index file")?;
        for pack in index.packs.iter().chain(index.packs_to_delete.iter()) {
            indexed.insert(pack.id.to_hex().to_string());
        }
    }
    let listed_ids: BTreeSet<String> = listed
        .iter()
        .map(|(id, _)| id.to_hex().to_string())
        .collect();
    let mut unindexed: Vec<(String, u64)> = listed
        .iter()
        .filter(|(id, _)| !indexed.contains(&id.to_hex().to_string()))
        .map(|(id, size)| (id.to_hex().to_string(), u64::from(*size)))
        .collect();
    unindexed.sort();
    let missing: Vec<String> = indexed.difference(&listed_ids).cloned().collect();
    Ok(OrphanReport { unindexed, missing })
}

/// How much of a pack is read at a time while verifying it: a pack is verified
/// by reading all of it, and a bounded buffer means a large pack cannot turn the
/// check that protects it into an allocation. (rustic's own `check --read-data`
/// reads a whole pack into memory instead.)
const VERIFY_CHUNK: u32 = 8 * 1024 * 1024;

/// Read every pack the index does not name, all of it, and compare it with the
/// id it is stored under.
///
/// A pack's id **is** the SHA-256 of the pack file's own contents — rustic names
/// a pack after the bytes it just wrote (`rustic_core` `src/blob/packer.rs:762`)
/// and its own `check --read-data` recomputes exactly that
/// (`commands/check.rs:736`) — so comparing the two covers every byte of the
/// pack: each blob's ciphertext, the encrypted header, and the trailing length
/// field. One flipped bit anywhere moves it. That is strictly wider than
/// decrypting each blob and re-deriving its id (which covers the blobs, and
/// neither the header bytes nor the length): the question this answers is not
/// "is this blob decryptable" but "is this pack the bytes its author wrote",
/// which is the property a dedup entry has to rest on.
///
/// # Errors
///
/// A reason naming the pack (by the first 12 hex characters of its id) when it
/// does not read back as the bytes it is named for: a pack still being written,
/// a damaged one, or one the backend cannot read at all.
fn verify_unindexed(
    backends: &RepositoryBackends,
    unindexed: &[(String, u64)],
) -> Result<(), String> {
    let be = backends.repository();
    for (hex, listed) in unindexed {
        let short = hex.get(..12).unwrap_or(hex);
        let id: Id = hex
            .parse()
            .map_err(|err| format!("pack {short} is not a valid id: {err}"))?;
        let size = u32::try_from(*listed).map_err(|_| {
            format!("pack {short} is listed as {listed} bytes, which is larger than a pack can be")
        })?;
        let mut hasher = Sha256::new();
        let mut offset: u32 = 0;
        while offset < size {
            let want = VERIFY_CHUNK.min(size - offset);
            // `false`: read the stored bytes, never a cached copy of them.
            let chunk = be
                .read_partial(FileType::Pack, &id, false, offset, want)
                .map_err(|err| format!("pack {short} could not be read: {err:#}"))?;
            if chunk.len() < want as usize {
                return Err(format!(
                    "pack {short} ended after {} of {size} bytes",
                    offset.saturating_add(chunk.len() as u32)
                ));
            }
            hasher.update(&chunk);
            offset += want;
        }
        let computed = hex_digest(&hasher.finalize());
        if computed != *hex {
            return Err(format!(
                "pack {short} is not the bytes it is stored under: its contents hash to {}",
                computed.get(..12).unwrap_or(&computed)
            ));
        }
    }
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Open an existing repository and build its index, reaching the packs no index
/// file names. One call for every read path.
///
/// # Errors
///
/// If the repository cannot be opened or indexed.
pub fn open_adopting(
    cfg: &StoreConfig,
    backends: &RepositoryBackends,
    mk: &MasterKey,
) -> anyhow::Result<(Repository<IndexedFullStatus>, OrphanOutcome)> {
    let opened = Repository::new(&cfg.repository_options(), backends)
        .context("build repository")?
        .open(&Credentials::Masterkey(mk.clone()))
        .context("open existing repository")?;
    index_adopting(opened, cfg, backends, mk)
}

/// Build the in-memory index of an already-opened repository, reaching every
/// pack in the backend — including packs no index file names.
///
/// The repository is consumed, and `to_indexed_checked` takes it by value, so
/// the refusal path rebuilds the open state from `cfg` and `backends`. That
/// re-open does not carry the caller's
/// [`ProgressBars`](rustic_core::ProgressBars), which only the rare refusal path
/// can notice.
///
/// # Errors
///
/// If the survey cannot read the index, or the repository cannot be re-opened on
/// the refusal path.
pub fn index_adopting<S: Open>(
    repo: Repository<S>,
    cfg: &StoreConfig,
    backends: &RepositoryBackends,
    mk: &MasterKey,
) -> anyhow::Result<(Repository<IndexedFullStatus>, OrphanOutcome)> {
    let report = survey(&repo, backends)?;
    if report.unindexed.is_empty() {
        // The index and the backend agree. Take the plain index: it is what
        // every read path took before this module existed, and it keeps entries
        // for packs the backend lacks — which the local metadata cache can still
        // serve.
        let repo = repo.to_indexed().context("index repository")?;
        return Ok((
            repo,
            OrphanOutcome {
                report,
                adoption: Some(IndexAdoption::Plain),
            },
        ));
    }

    // Refuse to adopt over an index that already names a pack the backend does
    // not have: the checked pass rebuilds the index from the backend's listing,
    // so it would drop that entry and take the cache-served reads with it. The
    // safe direction is to leave the index alone and re-upload; the caller
    // reports the refusal.
    if !report.missing.is_empty() {
        let why = format!(
            "{} pack(s) this index names are absent from the backend",
            report.missing.len()
        );
        let repo = repo.to_indexed().context("index repository")?;
        return Ok((
            repo,
            OrphanOutcome {
                report,
                adoption: Some(IndexAdoption::Refused { why }),
            },
        ));
    }

    // Nothing enters the dedup index unverified. The checked pass validates each
    // pack's *header* and the lengths declared in it against the file's size; it
    // never reads a blob, so a pack damaged in place — header and length intact,
    // ciphertext not — passes it, and a later backup would then skip uploading
    // blobs no reader can decrypt. Reading every byte of every pack the index
    // does not name first is what makes that impossible rather than unlikely.
    // The refusal is the whole adoption, not the one pack: a partial adoption
    // would leave which blobs are reachable decided by which packs happened to
    // verify, which is a state nothing downstream could reason about.
    if let Err(why) = verify_unindexed(backends, &report.unindexed) {
        let repo = repo.to_indexed().context("index repository")?;
        return Ok((
            repo,
            OrphanOutcome {
                report,
                adoption: Some(IndexAdoption::Refused { why }),
            },
        ));
    }

    match repo.to_indexed_checked() {
        Ok(repo) => Ok((
            repo,
            OrphanOutcome {
                report,
                adoption: Some(IndexAdoption::Adopted),
            },
        )),
        Err(err) => {
            // An unreadable pack is what refuses this in practice: the adopting
            // pass reads the header of every pack the index does not name and
            // gives up rather than index a pack whose bytes it cannot validate.
            // Nothing readable is lost by falling back — the pack that refused
            // the pass is one no index names, so no snapshot can reach it.
            let why = format!("{err:#}");
            let reopened = Repository::new(&cfg.repository_options(), backends)
                .context("re-open the repository")?
                .open(&Credentials::Masterkey(mk.clone()))
                .context("re-open the repository")?
                .to_indexed()
                .context("index the repository")?;
            Ok((
                reopened,
                OrphanOutcome {
                    report,
                    adoption: Some(IndexAdoption::Refused { why }),
                },
            ))
        }
    }
}
