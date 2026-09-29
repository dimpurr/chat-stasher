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
//! The answer here is adoption, not deletion. This code verifies each
//! unindexed pack, builds its `IndexPack` from the verified header, and passes
//! that exact set to rustic's `to_indexed_with_packs`; this code supplies the
//! existing index entries too, so rustic lists neither index nor pack files
//! during this conversion. A later `backup`
//! then finds every one of their blobs already present (rustic_core's
//! `archiver/file_archiver.rs:154` is a plain `index.has_data` test, independent
//! of any parent snapshot) and uploads none of them. The bytes stop being waste
//! because they become the content.
//!
//! Adoption is fenced by a survey, not applied unconditionally. One thing the
//! plain index does that adoption must preserve is keep an entry for a pack the
//! backend no longer has — an entry the local metadata cache can still serve
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
//! `verify_unindexed` reads each such pack's header *and every blob that header
//! names*: the header must decrypt with the repository
//! key, every blob must decrypt and decompress to the length the header gives,
//! and every blob's plaintext must hash to the id the header names. The pack's
//! bytes must also hash to the id the pack is stored under — a pack's id is the
//! SHA-256 of its own contents (rustic_core `src/blob/packer.rs:762`) — which is
//! the check a pack that was damaged and *renamed to the hash of its new bytes*
//! would otherwise slip through. Any failure refuses the whole adoption and is
//! reported, never absorbed — the direction that costs a re-upload and never the
//! archive. `crate::packcheck` holds the format and crypto reading, and the
//! reason it does not call rustic's own `check_pack`.
//!
//! The adoption path compares the unindexed set before and after verification,
//! then passes the verified `IndexPack`s to a small pinned-rustic API that reads
//! index files and adds only those entries. No second pack listing occurs, so a
//! pack appearing at that point cannot enter the in-memory index.
//!
//! `docs-dev/orphan-packs.md` carries the safety argument in full.

use crate::store::StoreConfig;
use anyhow::Context;
use rustic_core::repofile::{IndexFile, IndexPack, MasterKey};
use rustic_core::{
    Credentials, FileType, IndexedFullStatus, Open, ReadBackend, Repository, RepositoryBackends,
};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

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
    /// cache, and what rebuilding without a refusal would drop.
    pub missing: Vec<String>,
}

/// The exact repository inventory captured by one survey.
struct Survey {
    report: OrphanReport,
    listed: BTreeSet<String>,
    indexed: BTreeSet<String>,
    indexed_packs: Vec<IndexPack>,
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
    /// pack the backend does not have, or verification failing on a pack
    /// whose header it could not use.
    Refused {
        /// Human-readable reason, naming the pack when one pack refused the
        /// open, and carrying the verifier's reason when a pack failed.
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
    Ok(survey_inventory(repo, backends)?.report)
}

fn survey_inventory<S: Open>(
    repo: &Repository<S>,
    backends: &RepositoryBackends,
) -> anyhow::Result<Survey> {
    let listed = backends
        .repository()
        .list_with_size(FileType::Pack)
        .context("list packs in the repository")?;
    let mut indexed = BTreeSet::new();
    let mut indexed_packs = Vec::new();
    for file in repo
        .stream_files::<IndexFile>()
        .context("stream index files")?
    {
        let (_, index) = file.context("read an index file")?;
        indexed_packs.extend(index.packs.iter().cloned());
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
    Ok(Survey {
        report: OrphanReport { unindexed, missing },
        listed: listed_ids,
        indexed,
        indexed_packs,
    })
}

/// Read every pack the index does not name — its header and every blob that
/// header declares — and refuse the first one that does not check out.
///
/// `crate::packcheck::verify_pack` holds the three checks and the citations they
/// mirror; what this adds is the granularity: one pack's failure refuses the
/// whole adoption, and the reason names the pack (by the first 12 hex characters
/// of its id) so the operator can find it.
///
/// # Errors
///
/// A reason naming the pack when it does not read back as the bytes it is
/// stored under, when its header does not decrypt or does not add up to the
/// file, or when any blob in it does not decrypt to the content of the id its
/// header gives: a pack still being written, a damaged one, or one the backend
/// cannot read at all.
fn verify_unindexed(
    backends: &RepositoryBackends,
    unindexed: &[(String, u64)],
    mk: &MasterKey,
) -> Result<Vec<IndexPack>, String> {
    let be = backends.repository();
    unindexed
        .iter()
        .map(|(hex, listed)| crate::packcheck::verify_pack(&be, hex, *listed, mk))
        .collect()
}

/// A test-only rendezvous, for the one window an outside process cannot
/// schedule: between the survey's pack listing and the adopting pass's.
///
/// Set `CHAT_STASHER_TEST_HOLD_OPEN_AFTER_SURVEY` to a directory and the open
/// creates `reached` inside it and then waits for `go`, so a test can add
/// anything it likes — an index file, a pack — while the open is parked exactly
/// there. The wait is bounded so a test that dies cannot park a real run
/// forever. Unset — every run that is not that test — this creates nothing,
/// reads nothing, and costs one environment lookup.
fn hold_after_survey() {
    let Ok(dir) = std::env::var("CHAT_STASHER_TEST_HOLD_OPEN_AFTER_SURVEY") else {
        return;
    };
    let dir = Path::new(&dir);
    #[allow(
        clippy::let_underscore_must_use,
        reason = "A test rendezvous that cannot be staged leaves the test to time out, which is the failure it should report."
    )]
    let _ = std::fs::create_dir_all(dir);
    #[allow(
        clippy::let_underscore_must_use,
        reason = "As above: the test that did not observe `reached` is the report."
    )]
    let _ = std::fs::write(dir.join("reached"), b"");
    let go = dir.join("go");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !go.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Test-only rendezvous at the point where rustic previously listed packs
/// again, after this open has verified its exact adoption set.
fn hold_after_verification() {
    let Ok(dir) = std::env::var("CHAT_STASHER_TEST_HOLD_OPEN_AFTER_VERIFICATION") else {
        return;
    };
    let dir = Path::new(&dir);
    #[allow(
        clippy::let_underscore_must_use,
        reason = "A test rendezvous that cannot be staged leaves the test to time out, which is the failure it should report."
    )]
    let _ = std::fs::create_dir_all(dir);
    #[allow(
        clippy::let_underscore_must_use,
        reason = "As above: the test that did not observe `reached` is the report."
    )]
    let _ = std::fs::write(dir.join("reached"), b"");
    let go = dir.join("go");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !go.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
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
    index_adopting(opened, backends, mk)
}

/// The plain-index fallback every refusal returns: the index this repository had
/// before this module existed, plus the reason it was not adopted.
///
/// # Errors
///
/// If the plain index cannot be built.
fn refused<S: Open>(
    repo: Repository<S>,
    report: OrphanReport,
    why: String,
) -> anyhow::Result<(Repository<IndexedFullStatus>, OrphanOutcome)> {
    let repo = repo.to_indexed().context("index repository")?;
    Ok((
        repo,
        OrphanOutcome {
            report,
            adoption: Some(IndexAdoption::Refused { why }),
        },
    ))
}

/// Build the in-memory index of an already-opened repository, including verified
/// packs no index file names.
///
/// # Errors
///
/// If the survey or index construction cannot read repository data.
pub fn index_adopting<S: Open>(
    repo: Repository<S>,
    backends: &RepositoryBackends,
    mk: &MasterKey,
) -> anyhow::Result<(Repository<IndexedFullStatus>, OrphanOutcome)> {
    let initial = survey_inventory(&repo, backends)?;
    let report = initial.report;
    if report.unindexed.is_empty() {
        // The index and the backend agree. Take the plain index: it is what
        // every read path took before this module existed, and it keeps entries
        // for packs the backend lacks — which the local metadata cache can still
        // serve.
        return Ok((
            repo.to_indexed().context("index repository")?,
            OrphanOutcome {
                report,
                adoption: Some(IndexAdoption::Plain),
            },
        ));
    }

    // Refuse to adopt over an index that already names a pack the backend does
    // not have: rebuilding from only the backend's listing would drop that
    // entry and take the cache-served reads with it. The
    // safe direction is to leave the index alone and re-upload; the caller
    // reports the refusal.
    if !report.missing.is_empty() {
        return refused(
            repo,
            report.clone(),
            format!(
                "{} pack(s) this index names are absent from the backend",
                report.missing.len()
            ),
        );
    }

    // Verify every header and blob, and keep the exact IndexPacks produced by
    // that verification. Rustic receives these entries directly; it must not
    // list packs again after verification, because a pack appearing in that
    // second listing would otherwise enter the dedup index unread.
    let verified = match verify_unindexed(backends, &report.unindexed, mk) {
        Ok(verified) => verified,
        Err(why) => return refused(repo, report, why),
    };

    // Recheck for an index file that arrived after the first survey, then pin
    // the test hook at the former relisting point. Anything arriving after this
    // point is excluded because rustic receives only `verified`.
    // The test that parks an open here and injects an index file is
    // `an_index_file_injected_between_the_survey_and_the_adopting_pass_...`; a
    // pack injected here is covered by the regression test for verified-set
    // adoption below.
    hold_after_survey();
    let after_verification = survey_inventory(&repo, backends)?;
    if after_verification.report.unindexed != report.unindexed
        || after_verification.listed != initial.listed
        || after_verification.indexed != initial.indexed
    {
        return refused(
            repo,
            report.clone(),
            "packs no index file names appeared after the survey".to_string(),
        );
    }
    hold_after_verification();
    let repo = repo
        .to_indexed_with_packs(initial.indexed_packs, verified)
        .context("index repository with verified unindexed packs")?;
    Ok((
        repo,
        OrphanOutcome {
            report,
            adoption: Some(IndexAdoption::Adopted),
        },
    ))
}
