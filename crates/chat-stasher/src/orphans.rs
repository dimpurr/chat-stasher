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
//! One exclusion, deliberately narrow (W1066): a pack whose header cannot be
//! read as a pack at all — torn debris from an interrupted push, the one shape
//! [`crate::debris`]'s shared classification recognizes — does not veto the
//! packs that verify. Its bytes can never become content (no header, no
//! verified blob entries), so excluding it adds no reliance on a partial
//! verification; it is reported as debris, never adopted, and never named by
//! any index file. Everything else keeps the all-or-nothing shape: one pack
//! that reads as a pack but does not verify refuses the whole adoption.
//!
//! Adoption stops being per-open the moment a push survives it: before a push
//! writes its snapshot (W1077), and after its own index file, it names the packs
//! this open verified in **one new index file** ([`persist_verified`]). That is a pure append —
//! no existing index file is read for rewriting, removed or replaced, which is
//! what keeps it inside ADR-001/ADR-016 while `repair index` (which rewrites)
//! stays out of reach. Only a push connection writes it; read paths, `mirror`
//! and `prune-orphans` never do. Before writing, the survey runs once more and
//! any pack another client has named in the meantime is skipped, so the worst
//! two concurrent pushers can produce is two index files whose entries are
//! identical (the same ids at the same offsets of the same pack) — whichever a
//! reader's lookup hits resolves the same bytes.
//!
//! The order is the safety property (W1077). A snapshot that reuses an adopted
//! pack reaches trees and blobs only that pack holds; if the snapshot were
//! published first, a kill before the naming index file left a snapshot whose
//! trees are in no index, and a plain read failed with `Tree ID ... not found
//! in index`. So the push publishes the naming index file first and the
//! snapshot second ([`PersistOutcome::covers_dependency`] is the gate between
//! them), and a failure to name the packs fails the push **before** any
//! snapshot is written: a kill between the two leaves a harmless extra index
//! file and no snapshot, and a failed write leaves neither, so no snapshot is
//! ever published in a dependent state. The push cannot instead drop the
//! adoption and re-upload, because the backup has already deduplicated against
//! the adopted packs by the time the snapshot is about to be written; the next
//! push re-verifies and tries again.
//!
//! The adoption path compares the unindexed set before and after verification,
//! then passes the verified `IndexPacks` to a small pinned-rustic API that reads
//! index files and adds only those entries. No second pack listing occurs, so a
//! pack appearing at that point cannot enter the in-memory index.
//!
//! `docs-dev/orphan-packs.md` carries the safety argument in full.

use crate::store::StoreConfig;
use anyhow::Context;
use rustic_core::repofile::{IndexFile, IndexPack, MasterKey};
use rustic_core::{
    Credentials, FileType, IndexedFullStatus, Open, OpenStatus, ReadBackend, Repository,
    RepositoryBackends,
};
use std::collections::{BTreeMap, BTreeSet};
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
    /// Every pack the backend listed: hex id -> backend bytes. Sizes are kept,
    /// not just ids, so a caller that reports totals reports them from the same
    /// listing its unindexed set came from.
    listed: BTreeMap<String, u64>,
    indexed: BTreeSet<String>,
    indexed_packs: Vec<IndexPack>,
}

/// The backend's whole pack listing and the packs the index files disagree
/// about, from **one** listing.
///
/// [`OrphanReport`] is the diff alone, which is all the adopting open needs.
/// A report that prints "42 packs, 2 unindexed" needs both halves to describe
/// the same moment, or a push landing between two listings could make the
/// unindexed count exceed the total — so this carries the listing too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackInventory {
    /// Every pack the backend listed: hex id -> backend bytes. Id-sorted.
    pub listed: BTreeMap<String, u64>,
    /// The diff the adopting open acts on: what no index names, and what an
    /// index names and the backend lacks.
    pub report: OrphanReport,
}

impl PackInventory {
    /// Packs the backend lists.
    #[must_use]
    pub fn total_packs(&self) -> usize {
        self.listed.len()
    }

    /// Backend bytes those packs occupy.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.listed.values().copied().sum()
    }
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
    /// At least one pack verified, and the index this open built reaches every
    /// pack that verified. Torn debris among the unindexed candidates is set
    /// aside in [`OrphanOutcome::torn`] and is neither named here nor
    /// anywhere else.
    Adopted,
    /// Every unindexed candidate was torn debris: nothing was adoptable, and
    /// nothing was refused either. The open took the plain index; the debris
    /// is carried in [`OrphanOutcome::torn`], stays unnamed forever and is
    /// reported as what it is.
    Debris,
    /// The adopting index was *not* built, so packs no index file names stay
    /// unreachable. `why` says which of the three fences stopped it: a pack that
    /// does not read back as the bytes it is stored under, an index that names a
    /// pack the backend does not have, or a pack set that changed under the open.
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
            Self::Plain | Self::Adopted | Self::Debris => None,
        }
    }

    /// Whether this open reached the packs no index file names.
    #[must_use]
    pub fn adopted(&self) -> bool {
        matches!(self, Self::Adopted)
    }
}

/// One pack the open set aside as torn debris instead of verifying it into the
/// adoption set: an unindexed candidate whose header cannot be read as a pack
/// at all ([`crate::debris::PackClass::Torn`]), so its bytes can never become
/// content. It is never adopted, never named by any index file and never
/// touched; it is carried out so the caller can report it as debris, and it
/// stays in the repository until a deliberately planned one-off action clears
/// it (ADR-016), which is not a code path here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedTorn {
    /// The pack's id, hex.
    pub id: String,
    /// The size the backend listed for it.
    pub bytes: u64,
    /// The probe's own reason the pack cannot be read as a pack.
    pub why: String,
}

/// An index over the repository, plus what one open found and did about packs
/// the index files do not name.
///
/// `verified` and `torn` carry this open's own classifications, so this type
/// no longer derives `PartialEq`/`Eq` (`IndexPack` does not carry them);
/// callers compare states through `report` and `adoption`, not whole opens.
#[derive(Debug, Clone, Default)]
pub struct OrphanOutcome {
    /// What the backend held when this open started.
    pub report: OrphanReport,
    /// What the index this open built can reach. `None` only for a repository
    /// this process created, which had no packs to survey.
    pub adoption: Option<IndexAdoption>,
    /// The `IndexPack` entries this open verified from the unindexed
    /// candidates (empty unless the adoption reached them). The push
    /// connection persists exactly these; every other caller must not write.
    pub verified: Vec<IndexPack>,
    /// The unindexed candidates excluded as torn debris, id-sorted, with the
    /// probe's own reason each. Empty unless the open found debris.
    pub torn: Vec<ExcludedTorn>,
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

/// [`survey`], keeping the pack listing the diff was taken from.
///
/// # Errors
///
/// If the backend cannot list packs, or an index file cannot be decrypted.
pub fn survey_packs<S: Open>(
    repo: &Repository<S>,
    backends: &RepositoryBackends,
) -> anyhow::Result<PackInventory> {
    let survey = survey_inventory(repo, backends)?;
    Ok(PackInventory {
        listed: survey.listed,
        report: survey.report,
    })
}

/// Open an existing repository **without** building an index.
///
/// [`open_adopting`] is what a read path calls, and it verifies and adopts the
/// packs no index file names. This is for a caller that only wants to *look* at
/// them — `prune-orphans` — and must not change what any index can reach.
///
/// # Errors
///
/// If the repository cannot be opened with this key.
pub fn open_for_survey(
    cfg: &StoreConfig,
    backends: &RepositoryBackends,
    mk: &MasterKey,
) -> anyhow::Result<Repository<OpenStatus>> {
    Repository::new(&cfg.repository_options(), backends)
        .context("build repository")?
        .open(&Credentials::Masterkey(mk.clone()))
        .context("open existing repository")
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
    let listed: BTreeMap<String, u64> = listed
        .iter()
        .map(|(id, size)| (id.to_hex().to_string(), u64::from(*size)))
        .collect();
    let mut unindexed: Vec<(String, u64)> = listed
        .iter()
        .filter(|(hex, _)| !indexed.contains(*hex))
        .map(|(hex, size)| (hex.clone(), *size))
        .collect();
    unindexed.sort();
    // Both sets are ordered by the same string keys, so this keeps the id order
    // the callers reported before sizes were carried alongside them.
    let missing: Vec<String> = indexed
        .iter()
        .filter(|hex| !listed.contains_key(*hex))
        .cloned()
        .collect();
    Ok(Survey {
        report: OrphanReport { unindexed, missing },
        listed,
        indexed,
        indexed_packs,
    })
}

/// Verify each pack the index does not name for adoption, setting torn
/// debris aside.
///
/// This is the adopting open's form of [`verify_candidates`]: every pack that
/// reads as a pack is verified in full — its header, every blob that header
/// declares, and its own bytes against its own id — and one pack that reads as
/// a pack but does not verify refuses the whole adoption, which keeps the dedup
/// index free of any reliance on a partial verification. The one exclusion is
/// the pack that cannot be read as a pack at all: torn debris from an
/// interrupted push, which is classified by the one shared function every
/// scanning reader uses ([`crate::debris::classify_pack`] on exactly the two
/// facts held here: the survey established no index file names it, and the
/// verifier could not read its header). Its bytes can never become content,
/// so it neither enters the adoption set nor vetoes the packs that verified;
/// it is carried out to be reported as debris. A refusal is returned as
/// [`crate::packcheck::PackError`] so the caller can keep its current
/// wording; the torn exclusions are not an error.
///
/// # Errors
///
/// A reason naming the pack when it does not read back as the bytes it is
/// stored under, when its header reads but any blob it declares does not, or
/// when the backend would not serve its bytes — every non-torn refusal, which
/// refuses the whole adoption exactly as before W1066.
fn verify_for_adoption(
    backends: &RepositoryBackends,
    unindexed: &[(String, u64)],
    mk: &MasterKey,
) -> Result<(Vec<IndexPack>, Vec<ExcludedTorn>), crate::packcheck::PackError> {
    let be = backends.repository();
    let mut verified = Vec::new();
    let mut torn = Vec::new();
    for (hex, listed) in unindexed {
        match crate::packcheck::verify_pack(&be, hex, *listed, mk) {
            Ok(pack) => verified.push(pack),
            Err(err) => {
                let check = Err::<(), _>(&err);
                match crate::debris::classify_pack(Some(false), &check) {
                    // Torn debris: excluded, reported, never a refusal. The
                    // survey already established that no index file names
                    // this pack, which is the other input the classification
                    // needs.
                    crate::debris::PackClass::Torn { why } => torn.push(ExcludedTorn {
                        id: hex.clone(),
                        bytes: *listed,
                        why,
                    }),
                    _ => return Err(err),
                }
            }
        }
    }
    Ok((verified, torn))
}

/// Verify each pack the index does not name, keeping the verdict on **every**
/// one of them instead of stopping at the first failure.
///
/// [`verify_for_adoption`] is the adopting open's form: one non-torn failure
/// refuses the whole adoption, so it stops there, and torn debris is excluded
/// rather than refused. A read-only report needs the other granularity — which
/// packs verified, which did not, and the reason for each — so `prune-orphans`
/// calls this. The checks are the same either way;
/// `crate::packcheck::verify_pack` holds them. The error is
/// [`crate::packcheck::PackError`] so the caller can tell the two shapes of
/// refusal apart: a pack whose header cannot be read at all (unindexed ⇒
/// debris from an interrupted push, `crate::debris`) and every other refusal,
/// which stays unknown.
#[must_use]
pub fn verify_candidates(
    backends: &RepositoryBackends,
    unindexed: &[(String, u64)],
    mk: &MasterKey,
) -> Vec<Result<IndexPack, crate::packcheck::PackError>> {
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

/// Test-only rendezvous at the point where a push is about to persist the
/// packs this open verified: after the pre-write survey, immediately before
/// the new index file is written.
///
/// Set `CHAT_STASHER_TEST_HOLD_PERSIST_BEFORE_WRITE` to a directory and the
/// persist step creates `reached` inside it and waits for `go`, so a test can
/// hold two pushes right at the window where both have surveyed but neither
/// has written — the two-client double-write — or inject an index file that
/// names the packs while the writer is parked. The wait is bounded like the
/// other holds; unset — every run that is not that test — this creates
/// nothing, reads nothing, and costs one environment lookup.
fn hold_persist_before_write() {
    let Ok(dir) = std::env::var("CHAT_STASHER_TEST_HOLD_PERSIST_BEFORE_WRITE") else {
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

/// What the push connection's persist step did with the packs its open
/// verified. When the push is about to publish a snapshot, anything but
/// [`Self::covers_dependency`] fails the push before the snapshot is written
/// (W1077); a pack that is not named stays unindexed **on disk**, which is the
/// state every earlier push left behind too, so the next push re-derives it and
/// tries again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistOutcome {
    /// One new index file now names every pack this open verified, so later
    /// opens find them named and skip the re-read their adoption used to
    /// cost. `index` is the new file's id, hex.
    Named {
        /// How many packs the new index file names.
        packs: usize,
        /// The id of the index file that was written.
        index: String,
    },
    /// Every pack this open verified was already named by an index file when
    /// the write was about to happen (another client persisted them first), so
    /// nothing was written and nothing needed to be.
    AlreadyNamed {
        /// How many verified packs were found named.
        packs: usize,
    },
    /// Nothing was written and the packs are not all named: a listing the
    /// backend would not serve, a verified pack that is no longer listed, or a
    /// write that failed; `why` says which. The packs are no worse off than
    /// before this push ran, but a snapshot must not depend on them.
    NotWritten {
        /// What happened, worded for the push report.
        why: String,
    },
}

impl PersistOutcome {
    /// Whether the packs this open verified are now named in an index file.
    #[must_use]
    pub fn named(&self) -> bool {
        matches!(self, Self::Named { .. })
    }

    /// Whether every pack this open verified is now named by some index file,
    /// by this push's file or by another client's: the condition a snapshot
    /// that reuses those packs needs before it may be published.
    #[must_use]
    pub fn covers_dependency(&self) -> bool {
        matches!(self, Self::Named { .. } | Self::AlreadyNamed { .. })
    }
}

/// Persist the packs this open verified as one new index file, appended to the
/// repository's index files. Push-connection only: this must never be called
/// from a read path, `mirror` or `prune-orphans`, every one of which must stay
/// write-free; the push that adopts the packs is the connection whose data was
/// proven to itself, which is the one place writing costs nothing to trust.
///
/// Idempotency is by re-survey, not by lock: the survey runs once more
/// immediately before the write, and any pack that is already named by an
/// index file — this push's own, or a concurrent client's — is skipped. A
/// healthy repository therefore pays one extra index pass per push that
/// adopted. Two clients that still name the same pack write two index files
/// whose entries are identical (the same ids at the same offsets of the same
/// pack), so whichever a reader's lookup hits resolves the same bytes; an
/// unreferenced pack is never one of them, because only verified packs reach
/// this function. Nothing is ever deleted or rewritten, per ADR-001/ADR-016.
///
/// This function never errors; every failure lands in
/// [`PersistOutcome::NotWritten`]. What a caller does with it depends on
/// whether a snapshot depends on the packs: the push calls it before the
/// snapshot is published and fails there unless
/// [`PersistOutcome::covers_dependency`] (W1077).
#[must_use]
pub fn persist_verified(
    repo: &Repository<IndexedFullStatus>,
    backends: &RepositoryBackends,
    verified: &[IndexPack],
) -> PersistOutcome {
    if verified.is_empty() {
        return PersistOutcome::NotWritten {
            why: "this open verified no pack, so there was nothing to name".to_string(),
        };
    }
    let fresh = match survey_inventory(repo, backends) {
        Ok(fresh) => fresh,
        Err(why) => {
            return PersistOutcome::NotWritten {
                why: format!(
                    "the pre-write survey over the repository failed, so nothing was named: {}",
                    crate::prune_orphans::redact(&why)
                ),
            };
        }
    };
    // Three facts decide each pack: still listed by the backend, and named by
    // no index file the pre-write survey read. A pack that got named in the
    // meantime is done: someone else persisted it, or this push's own index
    // file covered it, and re-naming it would only duplicate what a reader
    // already merges. A pack that is neither named nor listed cannot be named
    // honestly (an append-only backend never drops one, so this is the
    // unheard-of case); a snapshot cannot depend on it, so it is reported as
    // not covered rather than skipped.
    let mut survivors = Vec::with_capacity(verified.len());
    let mut vanished = 0usize;
    for pack in verified {
        let hex = pack.id.to_hex().to_string();
        if fresh.indexed.contains(&hex) {
            continue;
        }
        if !fresh.listed.contains_key(&hex) {
            vanished += 1;
            continue;
        }
        survivors.push(pack.clone());
    }
    if vanished > 0 {
        return PersistOutcome::NotWritten {
            why: format!(
                "{vanished} pack(s) this push verified are neither listed by the backend nor \
                 named by an index file, so nothing was written"
            ),
        };
    }
    if survivors.is_empty() {
        return PersistOutcome::AlreadyNamed {
            packs: verified.len(),
        };
    }
    hold_persist_before_write();
    match repo.append_index_packs(survivors.clone()) {
        Ok(Some(index)) => PersistOutcome::Named {
            packs: survivors.len(),
            index: index.to_hex().to_string(),
        },
        Ok(None) => PersistOutcome::NotWritten {
            why: "the naming index file ended up with no pack entries, so nothing was \
                  written"
                .to_string(),
        },
        Err(why) => PersistOutcome::NotWritten {
            why: format!(
                "writing the new index file failed, so the pack(s) stay unnamed on disk: {}",
                crate::prune_orphans::redact(&anyhow::Error::from(why))
            ),
        },
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
            verified: Vec::new(),
            torn: Vec::new(),
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
    index_adopting_unexplained(repo, backends, mk)
        .map_err(|error| explain_short_files(backends, error))
}

/// The smallest a stored (encrypted) repository file can be: a 16-byte IV and
/// a 16-byte MAC around an empty body. Anything shorter is not a file rustic
/// ever wrote whole.
pub const MIN_ENCRYPTED_FILE: u32 = 32;

/// An index or snapshot file too short to be a complete one (W1068).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortFile {
    /// `index` or `snapshots`.
    pub dir: &'static str,
    pub id_hex: String,
    pub bytes: u32,
}

/// Why one directory's size listing could not be taken, for
/// [`short_files_checked`]: the directory (`index` or `snapshots`) and the
/// error the backend answered with (W1071 review H1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortFileListingError {
    /// `index` or `snapshots`.
    pub dir: &'static str,
    /// The listing error, flattened.
    pub error: String,
}

/// The fallible shape of [`short_files`]: every short file the two size
/// listings named, and every listing that could not be taken (W1071 review
/// H1). A failed listing never hides what the other directory found —
/// both halves travel out together, so the doctor row that asks this
/// question can word a failure as unknown (never as a measured empty)
/// while the explain path keeps whatever the answered listing saw.
#[must_use]
pub fn short_files_checked(
    backends: &RepositoryBackends,
) -> (Vec<ShortFile>, Vec<ShortFileListingError>) {
    let mut files = Vec::new();
    let mut failures = Vec::new();
    for (tpe, dir) in [
        (FileType::Index, "index"),
        (FileType::Snapshot, "snapshots"),
    ] {
        match backends.repository().list_with_size(tpe) {
            Ok(listed) => {
                for (id, bytes) in listed {
                    if bytes < MIN_ENCRYPTED_FILE {
                        files.push(ShortFile {
                            dir,
                            id_hex: id.to_hex().to_string(),
                            bytes,
                        });
                    }
                }
            }
            Err(error) => failures.push(ShortFileListingError {
                dir,
                error: format!("{error:#}"),
            }),
        }
    }
    files.sort_by(|a, b| (a.dir, &a.id_hex).cmp(&(b.dir, &b.id_hex)));
    (files, failures)
}

/// Index and snapshot files under their final names that are shorter than any
/// complete file can be. Read-only: one listing per directory, sizes only.
/// A listing that fails is skipped here — this only ever runs to explain a
/// failure that already happened, so it names files it actually saw and
/// passes the original error through unchanged when it saw none; the doctor
/// row asks the same question through [`short_files_checked`] and words a
/// failed listing as unknown instead (W1071 review H1).
#[must_use]
pub fn short_files(backends: &RepositoryBackends) -> Vec<ShortFile> {
    short_files_checked(backends).0
}

/// The plain words for a repository that holds short index or snapshot files.
#[must_use]
pub fn short_files_words(files: &[ShortFile]) -> String {
    let names = files
        .iter()
        .map(|f| format!("`{}/{}` ({} bytes)", f.dir, f.id_hex, f.bytes))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "the repository holds {} file(s) too short to be complete: {names}. A file this short \
         is what a push cut off while writing it leaves behind (index and snapshot files were \
         written straight to their final name before this version); it never held anything \
         readable, and the packs of that push are still in the repository (packs no index \
         names are re-adopted by the next open). Nothing was changed. To recover: move exactly the file(s) named above out of the \
         repository directory on the destination (move, do not delete; keep them aside), then \
         run the command again",
        files.len()
    )
}

/// Turn an open/index/snapshot-listing failure into one that names the short
/// file when there is one; otherwise hand the error back unchanged. Only ever
/// called after a failure, so a healthy repository pays nothing.
pub fn explain_short_files(backends: &RepositoryBackends, error: anyhow::Error) -> anyhow::Error {
    let files = short_files(backends);
    if files.is_empty() {
        return error;
    }
    error.context(short_files_words(&files))
}

fn index_adopting_unexplained<S: Open>(
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
                verified: Vec::new(),
                torn: Vec::new(),
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

    // Verify every header and blob of every candidate, keeping the exact
    // IndexPacks that verification produced and setting torn debris aside
    // (W1066: debris cannot veto the packs that verify). Rustic receives
    // these entries directly; it must not list packs again after
    // verification, because a pack appearing in that second listing would
    // otherwise enter the dedup index unread.
    let (verified, torn) = match verify_for_adoption(backends, &report.unindexed, mk) {
        Ok(verified) => verified,
        Err(why) => return refused(repo, report, why.to_string()),
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
        .to_indexed_with_packs(initial.indexed_packs, verified.clone())
        .context("index repository with verified unindexed packs")?;
    // Nothing verified means every candidate was torn debris: the plain index
    // serves everything the index files name, and the debris is reported as
    // debris — a state apart from both adoption and refusal, because neither
    // happened and neither is due.
    let adoption = if verified.is_empty() {
        IndexAdoption::Debris
    } else {
        IndexAdoption::Adopted
    };
    Ok((
        repo,
        OrphanOutcome {
            report,
            adoption: Some(adoption),
            verified,
            torn,
        },
    ))
}
