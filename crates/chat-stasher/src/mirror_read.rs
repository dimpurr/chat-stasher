//! The read side of the local mirror (ADR-062 D3 / LM-3).
//!
//! LM-2 ([`crate::mirror`]) put a byte-exact local copy of one declared
//! destination on this machine's disk. This module is what makes reads *use*
//! it:
//!
//! * **Mirror-first, per file** ([`MirrorFirstBackend`]): a content-addressed
//!   repository file the mirror holds carries exactly the bytes its own name
//!   promises — files are immutable and hash-named, so the mirror's copy and
//!   the remote's copy of the same id are the same bytes by construction. A
//!   file the mirror **lacks** (the sync is mid-fill, or this session arrived
//!   minutes ago) is an ordinary read from the remote (47 §3.6: the fallback
//!   fetches from the source, and the result is correct either way). Writes
//!   never touch the mirror; only `mirror sync` does.
//! * **Absent and corrupt are different events.** The fallback leg exists
//!   for a file the mirror *lacks* — a sync-progress fact the seam can see,
//!   because the read errors while the copy's layout holds no such file, and
//!   the remote answers, visibly. A file that is corrupt rather than absent
//!   defeats the copy's own premise (the bytes an id names, held under that
//!   id) and is deliberately a **hard failure, never a silent remote
//!   retry**, enforced by classification rather than promised by wording:
//!   a mirror read that *errors on a file the copy's disk still holds*
//!   (truncated, unreadable — the shapes that fail at the read) returns
//!   the refusal of [`corrupt_copy_refusal`], the remote is not asked,
//!   and the error names the file and the repair — `mirror sync`'s next
//!   round byte-compares the copy and re-downloads a file that no longer
//!   hashes to its name. What a corrupt file *cannot* do is answer the
//!   read with plausible bytes: bit-rot under a length-preserving shape
//!   is served as answered, and the damage surfaces where decryption and
//!   re-hashing live, above the seam — a partial read cannot re-verify a
//!   whole-file id, so detection does not belong in this seam; the read
//!   stops honestly rather than fetch replacement bytes without saying
//!   so. `verify`, remote-only, keeps answering the only integrity
//!   certification a copy may not give itself (47 §5.5) — the tampered-
//!   mirror test locks both halves: `read` over garbage fails, `verify`
//!   still answers the remote.
//! * **Listings answer remote-first.** "Which files exist" is the one question
//!   a behind mirror must not answer for the remote: a snapshot the sync has
//!   not pulled yet exists remotely, and a `search` that could not see it
//!   would be *wrong*, not merely behind — the honesty razor cuts both ways
//!   (behindness is surfaced, never guessed as completeness, and never
//!   silently *shrinking* the truth either). So presence comes from the
//!   remote while it can be asked, and falls back to the mirror when the
//!   remote cannot answer at all — the offline listing that *errors* — or
//!   answers a listing with **nothing in it**, both counted and visible in
//!   [`MirrorCounters`]. The empty-answer leg is the degraded-source shape
//!   ADR-062 D2 names: a destination wiped and made again at the same path
//!   lists empty while the copy still holds the last synced set, and the
//!   copy goes on serving it. A repository only ever lists empty for a
//!   *reason* — the remote can be a brand-new repository with nothing
//!   archived yet — and that is why the fallback concedes when the mirror
//!   holds nothing of that type either: the remote's own answer, including
//!   a genuine empty one, is what the caller gets.
//! * **`config` is the one non-content-addressed file**, so it is read
//!   remote-first with mirror fallback: a mid-fill mirror could hold yesterday's
//!   `config` bytes, and one extra metadata read per open is the price of
//!   never acting on a stale repository layout (ADR-062 D2 byte-compares it
//!   every sync round; neither direction may *prefer* the unverified copy).
//! * **Ruling 7 — mirror reads never enter the body cache** (ADR-062 D3): the
//!   mirror is a body tier, not a cache, and storing bytes the machine already
//!   holds whole would keep the same ciphertext on disk twice. The cache is
//!   therefore installed **inside** this seam and consulted only on the
//!   remote-fallback leg: a cache hit serves, a miss goes mirror-first, and
//!   only a read that actually reached the remote is stored (ruling 7: a
//!   miss that went to the remote still follows today's path). With no
//!   mirror attached, the plain
//!   [`crate::body_cache::BodyCacheBackend`] keeps today's stack bit for bit.
//! * **`verify` is none of this business** (47 §5.5, ADR-062 D3): a copy may
//!   never certify itself, so `verify` builds no [`MirrorReads`] and this
//!   wrapper is simply absent from its backends. The lock for that is the
//!   tampered-mirror fixture in `tests/mirror_read_test.rs`: `verify --level
//!   l2` passes over a mirror whose pack bytes are garbage, which is only
//!   possible if it never asked the mirror.
//!
//! Which commands opt in is also the ADR's list and nothing else: `read` /
//! `search` / `ui` / index rebuild / `export` attach [`MirrorReads`] when
//! [`decide`] finds it; `verify` does not, and neither do the write paths
//! (`collect`/`push`/`dest-init`/`reclaim`), which keep reading exactly the
//! repository they write.
//!
//! **W1053 — `--offline`, the switch that removes the fallback** (LM5
//! OFFLINE-RULING): the reaching shape above is *first, with a remote
//! behind it*. [`OfflineMirrorBackend`] is the same mirror leg with no
//! remote behind it at all: the destination's backend is never even
//! constructed (no `to_backends()` on the declared repository, so nothing
//! can dial), every listing is answered by the copy, and a file the copy
//! lacks is the refusal of [`offline_absent_refusal`] — the read reports
//! "did not finish" rather than reaching for the network, which is the
//! ruling's whole point: the three states stay apart, so an offline
//! absence is an *unknown about the archive* (`3`), never an empty result
//! and never a claimed absence. The body cache still serves what it holds
//! (the copy's bytes never enter it — ruling 7 holds offline too), and
//! rustic's own metadata cache, being local, keeps serving trees and
//! snapshots from earlier opens. `read` / `export` / `verify` take the
//! switch; `verify --offline` reads the copy by name and its verdict
//! certifies **the copy**, the same stance [`MirrorReadDecision::
//! MirrorCopyItself`] already words for `verify --repo <the copy>`
//! (47 §5.5) — a switch that names its target is not the silent mirror
//! seam that ruling forbids, and online `verify` never consults the copy
//! either way.
//!
//! Privacy: counters and state lines carry destination names, file counts
//! and byte totals. No session ids, no paths a state file would keep, no
//! conversation content.

use crate::body_cache::BodyCache;
use crate::config::Config;
use crate::mirror::{self, MirrorSettings};
use crate::mirrorstate;
use crate::store::StoreConfig;
use rustic_backend::BackendOptions;
use rustic_core::{FileType, ReadBackend, WriteBackend};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// The serving handle and its counters
// ---------------------------------------------------------------------------

/// What one command's reads did through the mirror seam. The file counts
/// are **distinct files**, never read operations: a pack a read asks for
/// in N ranges is one file and N range fetches, and only the distinct
/// count is checkable against the read it describes. The byte fields are
/// operation sums — bytes moved, every range counted at every fetch — so
/// one file's `mirror_bytes` can total more than its size, and the line
/// built from these numbers never presents a byte sum as a file's size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MirrorCounters {
    /// Distinct repository files served from the local copy
    /// (`read_full` + `read_partial`, every non-config file type): counted
    /// once per file per process, so a file re-read in several ranges —
    /// or after another read already counted it — stays one file.
    pub mirror_files: u64,
    /// Bytes those reads moved off the local disk — every range of every
    /// read counted, re-fetches included.
    pub mirror_bytes: u64,
    /// Distinct files read from the remote rather than the local copy: the
    /// 47 §3.6 fallback for a file the mirror lacks, plus the deliberate
    /// remote-first `config` read (the one non-content-addressed file), so
    /// a fully synced mirror online still reads one `config` from the
    /// remote per open — one file here, however many opens asked for it.
    /// Every one of these kept the remote's connection budget in play; the
    /// count alone is never a claim that the mirror was missing a file —
    /// the served line words it that way on purpose.
    pub remote_files: u64,
    /// Bytes those reads fetched from the remote — every fetch counted,
    /// re-fetches included.
    pub remote_bytes: u64,
    /// Listings the mirror answered when the remote would not — the
    /// offline listing that errors, and the empty answer of a destination
    /// wiped and recreated at the same path — the read that still worked
    /// because of the copy, and the reason its snapshot set is only as
    /// new as the last sync.
    pub mirror_listings: u64,
}

/// The identity of one repository file in the counters: the type's
/// directory name plus the id — config's non-addressed zero id can never
/// collide with a content-addressed file's, and the pair is all the
/// distinctness a check of the line needs.
type FileKey = (&'static str, rustic_core::Id);

#[derive(Debug, Default)]
struct CounterSet {
    /// Bytes moved per distinct file served from the copy.
    mirror: std::sync::Mutex<std::collections::BTreeMap<FileKey, u64>>,
    /// Bytes moved per distinct file read from the remote.
    remote: std::sync::Mutex<std::collections::BTreeMap<FileKey, u64>>,
    mirror_listings: AtomicU64,
}

impl CounterSet {
    /// One read's contribution: `len` bytes moved for `(tpe, id)` on the
    /// leg `from_mirror` — the file's first appearance sets its entry for
    /// the count, every fetch adds its bytes.
    fn note_read(&self, from_mirror: bool, tpe: FileType, id: &rustic_core::Id, len: usize) {
        let leg = if from_mirror {
            &self.mirror
        } else {
            &self.remote
        };
        let mut leg = leg.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *leg.entry((tpe.dirname(), *id)).or_insert(0) += len as u64;
    }

    /// (distinct files, bytes moved) for one leg — the two halves the
    /// served line prints.
    fn tally(leg: &std::sync::Mutex<std::collections::BTreeMap<FileKey, u64>>) -> (u64, u64) {
        let leg = leg.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (leg.len() as u64, leg.values().sum())
    }
}

/// The handle a command attaches to a [`crate::store::BackupStore`] to make
/// its reads mirror-first. Built only by [`decide`], so a store that was never
/// handed one keeps today's remote-only reads untouched — `verify`'s
/// guarantee rides on exactly that.
#[derive(Debug)]
pub struct MirrorReads {
    /// The declared destination this seam serves (the `[mirror] mirror_of`
    /// name, which the decision already proved this store resolves to).
    pub mirror_of: String,
    /// The copied repository's root: `<data root>/mirror/<mirror_of>`
    /// (ADR-062 D1's layout, opened as a local backend — C4: same directory
    /// shape, destination's existing key, no new plaintext).
    pub mirror_root: PathBuf,
    counters: CounterSet,
}

impl MirrorReads {
    /// A snapshot of what this process's reads did through the seam:
    /// distinct files per leg, bytes moved per leg, listings answered.
    #[must_use]
    pub fn counters(&self) -> MirrorCounters {
        let (mirror_files, mirror_bytes) = CounterSet::tally(&self.counters.mirror);
        let (remote_files, remote_bytes) = CounterSet::tally(&self.counters.remote);
        MirrorCounters {
            mirror_files,
            mirror_bytes,
            remote_files,
            remote_bytes,
            mirror_listings: self.counters.mirror_listings.load(Ordering::Relaxed),
        }
    }

    /// The one-line report of what served: the visible-status half of the
    /// fallback rule — a read that went around the mirror says so with the
    /// numbers that describe it, never silently. "read from the remote"
    /// covers both kinds of crossing deliberately: a file the mirror lacks
    /// (the 47 §3.6 fallback — counted, visible) and the remote-first
    /// `config` read that never tried the mirror; wording the second as a
    /// fallback would print "1 fell back" for a fully synced mirror that
    /// was never missing a file, which is a line the read it describes
    /// could not check out. The counts are distinct files — the number a
    /// reader can check against the copy's own directory — so the same
    /// word "file(s)" is used only where the number really is files.
    #[must_use]
    pub fn served_line(&self) -> String {
        let counters = self.counters();
        format!(
            "{} file(s) ({} B) from the local mirror, {} file(s) ({} B) read from the \
             remote, {} listing(s) answered locally",
            counters.mirror_files,
            counters.mirror_bytes,
            counters.remote_files,
            counters.remote_bytes,
            counters.mirror_listings
        )
    }

    /// The same report for an offline run (W1053): the phrase the ruling
    /// names — *served from mirror* — plus the numbers, with the remote leg
    /// worded as the zero it is by construction rather than as a
    /// fallback count. An offline run's `remote_files` is a structural `0`
    /// (the backend never existed), and printing the online sentence's
    /// "0 file(s) (0 B) read from the remote" would read as a measurement
    /// of traffic rather than as the switch's promise kept.
    #[must_use]
    pub fn offline_served_line(&self) -> String {
        let counters = self.counters();
        format!(
            "served from mirror `{}` — {} file(s) ({} B) from the local copy, {} listing(s) \
             answered locally, nothing from the remote (--offline; this run opened no remote \
             connection)",
            self.mirror_of, counters.mirror_files, counters.mirror_bytes, counters.mirror_listings
        )
    }

    /// The pre-read announcement an offline run prints (W1053): the
    /// switch's promise in one line, before any step can wedge — the copy
    /// is the only source, the remote will not be contacted, and a file the
    /// copy lacks is the end of this read rather than a fetch. It replaces
    /// the serving state line offline (which promises the opposite: "the
    /// remote answers what it lacks").
    #[must_use]
    pub fn offline_state_line(&self) -> String {
        format!(
            "served from mirror `{}` — the local copy is this read's only source: the \
             destination is not contacted, and a file the copy lacks ends the read (reported, \
             never silently fetched)",
            self.mirror_of
        )
    }
}

// ---------------------------------------------------------------------------
// The decision: does this store read mirror-first?
// ---------------------------------------------------------------------------

/// Whether the reads of one resolved destination go through the mirror
/// (ADR-062 D3's "no new switch": the `[mirror]` section already on disk is
/// the only input, and every state below has one honest rendering).
#[derive(Debug)]
pub enum MirrorReadDecision {
    /// This machine declares no `[mirror]` section: the default-off machine
    /// (ADR-001 ⑥), reads the remote exactly as before. Silent — there is
    /// no mirror event to report. The one store that is not this state
    /// even with no section is the copy itself: it is recognized by path
    /// before any section is consulted ([`MirrorCopyItself`] below), so
    /// removing a section can never silence a copy that is still on disk.
    Unconfigured,
    /// The section exists but the switch is off (ADR-062 D1: switch off
    /// means stop syncing and read from the source). A configured machine
    /// gets one line saying so; nothing was read
    /// from any mirror.
    SwitchOff,
    /// The mirror is enabled but cannot serve this read — an unset
    /// `mirror_of`, a destination it does not name, a repo path that cannot
    /// be resolved, or no local copy on disk yet. The command reads the
    /// remote and says why (the fall-back-plus-visible-status rule; `why` is a
    /// human sentence that names what to change).
    Unusable { mirror_of: String, why: String },
    /// The mirror is on and usable, but this store is a different declared
    /// destination (or an explicit `--repo` elsewhere): reading another
    /// archive is not a mirror event, so this state is silent like
    /// `Unconfigured`.
    OtherDestination,
    /// The store resolves inside the copies directory
    /// (`<data root>/mirror/<name>`, name in `name`): the command is
    /// reading this machine's local copy **itself**, not the mirrored
    /// destination's repository. Never serving — the copy has no copy of
    /// its own to read first — and loud, because "reads the copy" is a
    /// fact about every answer it returns. Recognized by path, before the
    /// section and switch states are consulted, so a copy that outlived
    /// its section or outlived the switch still states itself.
    /// `verify` over this store is the spelling the ADR's absolute wording
    /// hides: the copy *is* the mirror, so the one line says a verdict
    /// here certifies the copy, never the destination's repository (47
    /// §5.5).
    MirrorCopyItself { name: String },
    /// Enabled, usable, and this store resolves to the mirrored destination:
    /// reads go mirror-first with per-file fallback (ADR-062 D3 / 47 §5.5).
    Serving(Arc<MirrorReads>),
}

impl MirrorReadDecision {
    /// The handle to attach to the store, when there is one.
    #[must_use]
    pub fn handle(&self) -> Option<Arc<MirrorReads>> {
        match self {
            Self::Serving(reads) => Some(Arc::clone(reads)),
            _ => None,
        }
    }

    /// Whether an offline run (W1053 `--offline`) can be served by this
    /// machine at all: only a `Serving` mirror has a copy to read instead
    /// of the destination, and [`Self::MirrorCopyItself`] is the copy —
    /// a repository that is already local, so a plain read of it *is* the
    /// offline shape (the switch attaches no seam and the copy's own
    /// `state_line` states what is being read). Everything else — no
    /// section, the switch off, an unusable mirror, another destination —
    /// has no offline leg, and the caller must refuse rather than dial.
    #[must_use]
    pub fn offline_capable(&self) -> bool {
        matches!(self, Self::Serving(_) | Self::MirrorCopyItself { .. })
    }

    /// The refusal sentence an offline run owes for the one machine state
    /// that cannot serve it: `None` exactly when [`Self::offline_capable`]
    /// is true, so the two cannot drift apart. The sentences stay in the
    /// decision's own vocabulary (they re-word the same states
    /// [`Self::state_line`] renders for the online fallback) because the
    /// difference is the promise: online, an unusable mirror means "this
    /// command reads the remote"; offline, the same state means the run
    /// has no local copy to read at all — nothing may dial.
    #[must_use]
    pub fn offline_refusal(&self) -> Option<String> {
        match self {
            Self::Serving(_) | Self::MirrorCopyItself { .. } => None,
            Self::Unconfigured => Some(
                "this machine declares no `[mirror]` section, so an offline read has no local \
                 copy to serve from — give the destination a mirror (a `[mirror]` section is \
                 the one the `mirror` commands read) and fill it with `chat-stasher mirror \
                 sync` first"
                    .to_string(),
            ),
            Self::SwitchOff => Some(
                "the `[mirror]` switch is off (`enabled = false`), so there is no local copy \
                 an offline read may serve from — turn the section on and run `chat-stasher \
                 mirror sync`, or run without `--offline`"
                    .to_string(),
            ),
            Self::Unusable { why, .. } => Some(format!(
                "the configured mirror cannot serve this read ({why}) — an offline run may \
                 not fall back to the destination, so fix the mirror (usually `chat-stasher \
                 mirror sync`) and re-run"
            )),
            Self::OtherDestination => Some(
                "the store resolves to a repository the configured mirror does not copy, and \
                 an offline run may not reach the destination to read it — `--offline` serves \
                 the mirrored destination only"
                    .to_string(),
            ),
        }
    }

    /// The one-line state a command prints before it reads. `None` for the
    /// two states that are nobody's business (no section; another
    /// destination) — a machine that never configured a mirror must see no
    /// mirror noise on every read, and one that reads a second, un-mirrored
    /// archive has not had a mirror event either.
    #[must_use]
    pub fn state_line(&self) -> Option<String> {
        match self {
            Self::Unconfigured | Self::OtherDestination => None,
            Self::SwitchOff => {
                Some("off ([mirror] enabled = false; reading the remote source)".to_string())
            }
            Self::Unusable { why, .. } => Some(format!(
                "unavailable ({why}); this command reads the remote"
            )),
            Self::MirrorCopyItself { name } => Some(format!(
                "this store is the local mirror copy `{name}` itself — reads serve the copy, \
                 and only the copy: `verify` here certifies `{name}`'s copy, never the \
                 mirrored destination's repository"
            )),
            Self::Serving(reads) => Some(format!(
                "first `{}` — the local copy serves what it holds; the remote answers what \
                 it lacks",
                reads.mirror_of
            )),
        }
    }
}

/// Decide whether `resolved`'s reads go mirror-first (ADR-062 D3).
///
/// The comparison is on the **repository** the store resolves to, not the
/// flag that picked it: `--destination storagebox` and
/// `--repo <storagebox's repo>` both read the mirrored repository and both
/// are served mirror-first, because the copy is of the repository, while a
/// `--repo` pointing anywhere else is a different archive the mirror says
/// nothing about.
///
/// One recognition is deliberately ordered **before** every other state,
/// section and switch included: a store that resolves inside the copies
/// directory is the copy itself, a fact the on-disk layout declares with
/// no help from `[mirror]` — so a copy that outlived its section, or
/// outlived the switch being thrown, still states itself rather than
/// borrow the section's silence. For `verify` that ordering is the one
/// line that keeps a verdict over this machine's copy from passing as the
/// copied destination's own (47 §5.5).
///
/// `data_root` is passed in rather than read from the environment so tests
/// isolate as explicitly as the LM-2 engine does — the caller's one
/// `config::default_data_root()` call keeps production on the real root.
#[must_use]
pub fn decide(config: &Config, resolved: &StoreConfig, data_root: &Path) -> MirrorReadDecision {
    // The copy itself is recognized by path, first of all: the layout —
    // `<data root>/mirror/<name>`, one path component below the copies
    // directory — is what the sync's own writes declare, no section
    // required, and `mirror_copy_name` keeps the name off the path for the
    // same reason (`mirror_read.rs`'s `MirrorCopyItself` doc).
    if let Some(name) = mirror_copy_name(data_root, &resolved.repo_root) {
        return MirrorReadDecision::MirrorCopyItself { name };
    }
    let Some(settings) = MirrorSettings::for_config(config) else {
        return MirrorReadDecision::Unconfigured;
    };
    if !settings.enabled {
        return MirrorReadDecision::SwitchOff;
    }
    let unusable = |why: String| MirrorReadDecision::Unusable {
        mirror_of: settings.mirror_of.clone(),
        why,
    };
    if settings.mirror_of.trim().is_empty() {
        return unusable(
            "`[mirror] mirror_of` is empty; name the declared destination to mirror".to_string(),
        );
    }
    let Some(entry) = config.destinations.get(&settings.mirror_of) else {
        return unusable(format!(
            "`[mirror] mirror_of = \"{}\"` names no declared destination",
            settings.mirror_of
        ));
    };
    let Some(raw_repo) = &entry.repo else {
        return unusable(format!(
            "the mirrored destination `{}` declares no `repo`",
            settings.mirror_of
        ));
    };
    // reason: the resolution mirrors `resolve_store_config_checked`'s
    // destination arm (expand_and_verify, then compare) without inheriting
    // its exit-on-failure guarantees — a path the *this* machine cannot even
    // resolve is an unusable state to state, not a usage error to exit on:
    // the command is reading some other store and must still work.
    let repo_root = match crate::config::expand_and_verify(raw_repo) {
        Ok(path) => path,
        Err(e) => {
            return unusable(format!(
                "the mirrored destination's `repo = \"{raw_repo}\"` cannot be resolved ({e})"
            ));
        }
    };
    if Path::new(&resolved.repo_root) != repo_root {
        // Every repo-root spelling inside this arm is "another archive":
        // the only stores that are not (the copies themselves) were already
        // recognized and answered at the top of `decide`, before the
        // section and switch states — which is what keeps a copy that
        // retired its section from landing here as silent.
        return MirrorReadDecision::OtherDestination;
    }
    let mirror_root = mirror::mirror_dir(data_root, &settings.mirror_of);
    if !mirror_root.is_dir() {
        return unusable(
            "no local copy is on disk yet — no mirror sync round has filled it; start it with \
             `chat-stasher mirror sync`"
                .to_string(),
        );
    }
    MirrorReadDecision::Serving(Arc::new(MirrorReads {
        mirror_of: settings.mirror_of,
        mirror_root,
        counters: CounterSet::default(),
    }))
}

/// The copy a resolved store names, when it is one of this data root's
/// copies: the single path component below [`mirror::copies_dir`]. That
/// component is the mirrored destination's name when the `[mirror]`
/// section still names it — and stays the copy's own name when it does
/// not, which is why the answer comes off the path and not the config: a
/// copy that outlived its section still states itself. `None` for
/// everything else, including a repository merely nested inside the
/// copies directory, which the sync's layout never wrote.
fn mirror_copy_name(data_root: &Path, repo_root: &str) -> Option<String> {
    let relative = Path::new(repo_root)
        .strip_prefix(mirror::copies_dir(data_root))
        .ok()?;
    // The layout is exactly `<copies dir>/<destination>`: deeper than one
    // component is not a copy, so it reads as any other repository would.
    if relative.components().count() != 1 {
        return None;
    }
    let name = relative.components().next()?.as_os_str().to_str()?;
    (!name.is_empty()).then(|| name.to_string())
}

// ---------------------------------------------------------------------------
// Behindness where reads happen (47 §5.5's "local copy as of HH:MM")
// ---------------------------------------------------------------------------

/// The behindness sentence a serving mirror owes where reads happen (47
/// §5.5's own wording: the ui shows "the local copy as of HH:MM"). Built
/// from the
/// durable records LM-2 already keeps, with their three-state honesty: an
/// unreadable record is never a claim, a missing one is never "never", and a
/// record naming another destination is stale (mirrorstate's own expiry
/// rule), not this machine's copy.
///
/// LM-4 owns the settled field family across `status` / `doctor` / menubar;
/// this sentence is the read-surface stake in the same ground and keeps to
/// the wording the ADR quotes. Its two fill sentences (a failed round, a
/// first fill in progress) word the pair through the one spelling every
/// surface shares (`mirror::first_fill_numbers`) — rev1 ②/L1: a bare
/// verified percentage is exactly the number the incident misread on this
/// surface, and the dashboard and `status` must not show two different
/// numbers for the same mirror either.
#[must_use]
pub fn behindness_sentence(state_dir: &Path, mirror_of: &str) -> String {
    let state = mirrorstate::load(state_dir);
    let progress = mirror::load_progress(state_dir);
    // The fill states speak first when they apply: a failed round or a
    // first fill in progress is what the reader of a dashboard needs to
    // know about the copy their reads are using — a completed time earned
    // before the failure would read as current.
    if let mirror::MirrorProgressRead::Present(record) = &progress {
        if record.mirror_of == mirror_of {
            if record.outcome == "failed" {
                // reason: a failed round whose record carries no note
                // prints the failure with its numbers and nothing after the
                // colon — an incomplete sentence, never an invented cause
                // (the same honesty mirror.rs's own status wording keeps).
                let note = record.outcome_note.clone().unwrap_or_default();
                return format!(
                    "local copy's last sync round failed — {}: {note} — reads \
                     continue from what the mirror already holds and fall back to the remote",
                    mirror::first_fill_numbers(
                        &mirror::record_downloaded_state(record),
                        record.mirror_bytes,
                        record.remote_total_bytes
                    )
                );
            }
            if !record.complete {
                return format!(
                    "local copy's first sync is in progress: {} — reads fall \
                     back to the remote for what has not synced; this is not a failure",
                    mirror::first_fill_numbers(
                        &mirror::record_downloaded_state(record),
                        record.mirror_bytes,
                        record.remote_total_bytes
                    )
                );
            }
        }
    }
    match state {
        // The one state the sentence exists for: a completed sync this
        // mirror wrote, naming this destination. `as of HH:MM UTC` — 47
        // §5.5's wording; the age family (hours behind, days behind) is
        // LM-4's to field with shared wording across the three surfaces.
        mirrorstate::MirrorStateRead::Present(record) if record.mirror_of == mirror_of => {
            format!(
                "local copy as of {} UTC — the mirror serves what it holds; the remote \
                 answers what it lacks",
                sync_hhmm(record.finished_at_unix)
            )
        }
        mirrorstate::MirrorStateRead::Present(record) => format!(
            "local copy unreadable — the record names `{}`, which this machine does not mirror",
            record.mirror_of
        ),
        mirrorstate::MirrorStateRead::Unreadable(why) => {
            format!("local copy unreadable — mirror-state.json cannot be read: {why}")
        }
        mirrorstate::MirrorStateRead::Missing => match progress {
            mirror::MirrorProgressRead::Present(record) if record.mirror_of != mirror_of => {
                format!(
                    "local copy unreadable — the progress record names `{}`, not this \
                     destination",
                    record.mirror_of
                )
            }
            mirror::MirrorProgressRead::Unreadable(why) => {
                format!("local copy unreadable — the progress record cannot be read: {why}")
            }
            _ => "local copy present but no completed sync is recorded".to_string(),
        },
    }
}

/// The last completed sync's wall time as `HH:MM`, UTC — the clock every
/// surface in this tool already agrees on. An unrepresentable time is the
/// three-dash honesty answer, never 00:00 (a stamp that reads midnight).
fn sync_hhmm(unix: u64) -> String {
    i64::try_from(unix)
        .ok()
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map_or_else(
            || "--:--".to_string(),
            |time| time.format("%H:%M").to_string(),
        )
}

// ---------------------------------------------------------------------------
// The backend seam
// ---------------------------------------------------------------------------

/// The mirror-first read seam: one wrapper holding both backends, installed
/// **below** everything else rustic does (its metadata cache layers above, in
/// `Repository::open_raw`), and holding the body cache **inside** for
/// exactly one purpose — the ruling-7 rule that decides which reads may
/// store. Delegation, per operation:
///
/// * `list_with_size` — remote first, mirror fallback on **error or an
///   empty answer** (both counted). A successful non-empty listing is the
///   remote's word about what exists, and a behind mirror must never
///   shrink it (the honesty razor cuts both ways). The empty-answer leg
///   is the degraded-source shape ADR-062 D2 names: a destination wiped
///   and recreated at the same path answers a listing with nothing while
///   the copy still holds the last synced set, and the copy keeps serving
///   it — which is also why the fallback concedes when the mirror holds
///   nothing of that type either (see `list_with_size` below): the
///   remote's own answer, including the genuine empty a brand-new
///   repository owes its reader, is what the caller gets. The counter
///   makes every firing visible where the command reports itself.
/// * `read_full` / `read_partial` — `config` remote-first with mirror
///   fallback (the one non-content-addressed file); every other type
///   mirror-first with remote fallback, and the fallback exists for a file
///   the mirror *lacks*, never for a corrupt one — a read that fails while
///   the file is present on the copy is classified exactly so (the layout
///   knows) and answers with the hard failure the module rule above
///   names, and the counters stay at zero for the refused leg. A body
///   read through a body-cache-enabled store additionally consults the
///   cache first, and stores only what the fallback fetched from the
///   remote.
/// * writes, warm-ups and locations — the remote's, unchanged: this seam is
///   part of the *read* story and never mutates the mirror (only `mirror
///   sync` writes it).
pub(crate) struct MirrorFirstBackend {
    mirror: Arc<dyn WriteBackend>,
    remote: Arc<dyn WriteBackend>,
    /// The body cache this store would otherwise have wrapped around the
    /// remote (ADR-034), consulted only on the fallback leg (ruling 7).
    body_cache: Option<Arc<BodyCache>>,
    reads: Arc<MirrorReads>,
}

impl MirrorFirstBackend {
    /// Wrap `remote` so this store's reads take the local copy first. The
    /// mirror is opened as a local backend over the copied directory (C4) —
    /// a plain path backend builds for any path string; a mirror that never
    /// filled simply has nothing to serve and every read falls through.
    pub(crate) fn wrap(
        remote: Arc<dyn WriteBackend>,
        reads: Arc<MirrorReads>,
        body_cache: Option<Arc<BodyCache>>,
    ) -> anyhow::Result<Arc<dyn WriteBackend>> {
        let options =
            BackendOptions::default().repository(reads.mirror_root.to_string_lossy().into_owned());
        let mirror = options
            .to_backends()
            .map_err(|e| {
                anyhow::anyhow!(
                    "build the local backend over the mirror of `{}`: {e:#}",
                    reads.mirror_of
                )
            })?
            .repository();
        Ok(Arc::new(Self {
            mirror,
            remote,
            body_cache,
            reads,
        }))
    }

    /// One immutable file read, mirror-first with remote fallback, counted.
    /// `tpe` is never `Config` — that file has its own direction. Returns the
    /// bytes and whether they came from the mirror, because ruling 7 keys on
    /// provenance and only the read itself can say where its bytes came from
    /// (a counter comparison could not, under rustic's read fan-out).
    ///
    /// The fallback leg exists for a file the mirror **lacks** — the read
    /// errors, the remote answers, visibly. A file that **fails** its read
    /// while sitting on the copy's disk — truncated, unreadable, damaged
    /// under a valid name — is a different event, and the seam classifies
    /// it by that one fact ([`mirror_file_path`] exists ⇒ the refusal of
    /// [`corrupt_copy_refusal`]): a hard error, never a silent remote
    /// retry, exactly as the module header's absent-and-corrupt rule
    /// words it. A file that *answers* this read (the same-length garbage
    /// of the tampered fixture) is served as answered, and the damage
    /// surfaces above the seam where decryption and re-hashing live — a
    /// partial read cannot re-verify a whole-file id, so the seam claims
    /// no right to judge healthy bytes, only to refuse broken ones.
    fn read_local_first(
        &self,
        tpe: FileType,
        id: &rustic_core::Id,
        read: impl Fn(&dyn WriteBackend) -> rustic_core::RusticResult<bytes::Bytes>,
    ) -> rustic_core::RusticResult<(bytes::Bytes, bool)> {
        match read(&self.mirror) {
            Ok(bytes) => {
                self.reads.counters.note_read(true, tpe, id, bytes.len());
                Ok((bytes, true))
            }
            Err(error) => {
                let file = mirror_file_path(&self.reads.mirror_root, tpe, id);
                if file.exists() {
                    // Present but unreadable: the hard failure the header
                    // promises — the remote is not asked, the counters
                    // record nothing (no read happened, so nothing may
                    // read as sync-progress fallback), and the error names
                    // the file and the repair.
                    return Err(corrupt_copy_refusal(tpe, id, *error));
                }
                let bytes = read(&self.remote)?;
                self.reads.counters.note_read(false, tpe, id, bytes.len());
                Ok((bytes, false))
            }
        }
    }
}

/// The path a repository file occupies inside the copied repository — the
/// same layout `rustic_backend`'s local backend resolves (the backend the
/// mirror is opened as, and the exact layout LM-2's engine writes). This
/// is how the seam classifies a failed mirror read: the file either sits
/// on the disk the layout names or it does not — presence is the one fact
/// that separates "corrupt but present" (the hard failure) from "the
/// mirror lacks it" (the ordinary fallback), and no read-error text has
/// to be parsed to learn it.
fn mirror_file_path(mirror_root: &Path, tpe: FileType, id: &rustic_core::Id) -> PathBuf {
    let hex = id.to_hex();
    match tpe {
        FileType::Pack => mirror_root.join("data").join(&hex[0..2]).join(hex),
        FileType::Config => mirror_root.join("config"),
        _ => mirror_root.join(tpe.dirname()).join(hex),
    }
}

/// The hard failure a present-but-unreadable mirror file earns, per the
/// module header's absent-and-corrupt rule: a corrupt copy is never a
/// silent remote retry, so the error states the class, names the file,
/// and names the repair — `mirror sync` byte-compares every file it
/// copies and re-downloads one that no longer hashes to its name, which
/// is the repair that fixes exactly this shape — while keeping the copy's
/// own read error attached, so the damage is reported, not summarised
/// away.
fn corrupt_copy_refusal(
    tpe: FileType,
    id: &rustic_core::Id,
    error: rustic_core::RusticError,
) -> Box<rustic_core::RusticError> {
    let file = if tpe == FileType::Config {
        "`config` file".to_string()
    } else {
        format!("`{}` file `{}`", tpe.dirname(), id.to_hex().as_str())
    };
    rustic_core::RusticError::with_source(
        rustic_core::ErrorKind::Backend,
        format!(
            "The local mirror's {file} is present but cannot be read — this is disk damage \
             under a name the copy still declares, a hard failure and never a silent remote \
             retry, so the read stops here rather than substitute the remote's bytes for \
             the damaged copy. Run `chat-stasher mirror sync` to re-download the file \
             (each round byte-compares the copy and replaces a file that no longer hashes \
             to its name), or delete the mirror directory to re-fill it from scratch.",
        ),
        error,
    )
}

/// The refusal an offline read owes for a file the copy does not hold
/// (W1053): the online seam's ordinary fallback is a remote fetch, and
/// `--offline` exists to make exactly that fetch impossible — so the file's
/// absence is the end of this read, worded as the unknown it is. It never
/// reads as "the archive does not hold it": the remote was not asked, so
/// nothing here is a claim about the archive (the caller words the exit
/// `3`, "did not finish"). The repair is named for what it is — the
/// mirror is mid-fill or behind, and `mirror sync` is the leg that fills
/// it.
fn offline_absent_refusal(
    tpe: FileType,
    id: &rustic_core::Id,
    mirror_of: &str,
    error: rustic_core::RusticError,
) -> Box<rustic_core::RusticError> {
    let file = if tpe == FileType::Config {
        "`config` file".to_string()
    } else {
        format!("`{}` file `{}`", tpe.dirname(), id.to_hex().as_str())
    };
    rustic_core::RusticError::with_source(
        rustic_core::ErrorKind::Backend,
        format!(
            "This `--offline` read serves the local mirror of `{mirror_of}` only, and that \
             copy does not hold the {file} this read asked for — the remote was not \
             contacted, so the file's absence from the copy proves nothing about the \
             archive and this read did not finish. Run `chat-stasher mirror sync` to fill \
             what the copy lacks, or re-run without `--offline` to let the destination \
             answer.",
        ),
        error,
    )
}

// ---------------------------------------------------------------------------
// The offline backend (W1053): the mirror leg with no remote behind it
// ---------------------------------------------------------------------------

/// The `--offline` read seam: the copy's backend, and nothing else (W1053).
///
/// [`MirrorFirstBackend`] keeps the destination's backend behind the copy
/// for the files the copy lacks; this backend holds no destination backend
/// at all, which is the switch's whole contract — the declared repository's
/// options are never even turned into a backend, so no opendal operator is
/// built, no ssh master is spawned, and no socket is opened anywhere in the
/// process. Delegation, per operation:
///
/// * `list_with_size` — the copy's answer, full stop. The online rule —
///   presence stays the remote's while it can be asked — has no leg here:
///   the listing the copy gives is the listing the read gets, counted as a
///   locally answered listing so the served line says so.
/// * `read_full` / `read_partial` — the copy, with the body cache consulted
///   first exactly as [`MirrorFirstBackend`] does on its non-config legs
///   (a hit serves; a miss reads the copy; nothing is stored — ruling 7
///   holds offline too, because the bytes in question are the copy's own).
///   A read that errors on a file the copy's layout says is present is the
///   same hard [`corrupt_copy_refusal`] as online; a file the copy lacks
///   is [`offline_absent_refusal`] — never a fetch, never an empty answer.
/// * `config` — the copy's (the online remote-first direction exists to
///   avoid yesterday's layout; offline has one source, and the copy's own
///   `config` is the layout this open runs on).
/// * writes — refused: the readers that take `--offline` are read-only,
///   and a write through the offline seam would be a mutation of a copy
///   only `mirror sync` may write. The refusal is loud, not delegated.
pub(crate) struct OfflineMirrorBackend {
    mirror: Arc<dyn WriteBackend>,
    /// The store's identity stays the declared destination: error messages
    /// and `repo` lines continue to name the repository the config
    /// declares, never this machine's copy of it.
    location: String,
    /// The mirrored destination the copy holds (for the refusal's repair).
    mirror_of: String,
    /// The body cache this store would otherwise have wrapped around the
    /// remote (ADR-034), consulted first like the online seam's fallback
    /// ordering (ruling 7: the copy's bytes are never stored).
    body_cache: Option<Arc<BodyCache>>,
    reads: Arc<MirrorReads>,
}

impl OfflineMirrorBackend {
    /// Build the offline seam over the copy: a plain local backend over the
    /// mirrored directory (C4, the same construction [`MirrorFirstBackend`]
    /// uses for its mirror leg), and nothing else — the caller must pass
    /// `location`, the config-declared repository string, because the
    /// destination's backend is never built and there is nothing to ask
    /// `location()` of.
    pub(crate) fn wrap(
        reads: Arc<MirrorReads>,
        body_cache: Option<Arc<BodyCache>>,
        location: String,
    ) -> anyhow::Result<Arc<dyn WriteBackend>> {
        let options =
            BackendOptions::default().repository(reads.mirror_root.to_string_lossy().into_owned());
        let mirror = options
            .to_backends()
            .map_err(|e| {
                anyhow::anyhow!(
                    "build the local backend over the mirror of `{}`: {e:#}",
                    reads.mirror_of
                )
            })?
            .repository();
        Ok(Arc::new(Self {
            mirror,
            location,
            mirror_of: reads.mirror_of.clone(),
            body_cache,
            reads,
        }))
    }

    /// One read from the copy, classified at the same line the online seam
    /// classifies: a present-but-unreadable file is the corruption refusal,
    /// an absent one is the offline refusal.
    fn read_from_copy(
        &self,
        tpe: FileType,
        id: &rustic_core::Id,
        read: impl Fn(&dyn WriteBackend) -> rustic_core::RusticResult<bytes::Bytes>,
    ) -> rustic_core::RusticResult<bytes::Bytes> {
        match read(&self.mirror) {
            Ok(bytes) => {
                self.reads.counters.note_read(true, tpe, id, bytes.len());
                Ok(bytes)
            }
            Err(error) => {
                if mirror_file_path(&self.reads.mirror_root, tpe, id).exists() {
                    return Err(corrupt_copy_refusal(tpe, id, *error));
                }
                Err(offline_absent_refusal(tpe, id, &self.mirror_of, *error))
            }
        }
    }
}

impl ReadBackend for OfflineMirrorBackend {
    fn location(&self) -> String {
        // The store's identity stays the destination, exactly as the online
        // seam keeps it: the copy is where bytes come from, not what this
        // repository is called.
        self.location.clone()
    }

    fn list_with_size(
        &self,
        tpe: FileType,
    ) -> rustic_core::RusticResult<Vec<(rustic_core::Id, u32)>> {
        // No remote to defer to: the copy's answer is the whole answer. An
        // empty or erroneous listing is passed through for the caller to
        // word (an empty copy of a genuinely archived repository is the
        // mid-rollback/degraded shape the online seam also lets the caller
        // see, never silently padded).
        let answer = self.mirror.list_with_size(tpe)?;
        if !answer.is_empty() {
            self.reads
                .counters
                .mirror_listings
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(answer)
    }

    fn read_full(
        &self,
        tpe: FileType,
        id: &rustic_core::Id,
    ) -> rustic_core::RusticResult<bytes::Bytes> {
        self.read_from_copy(tpe, id, |backend| backend.read_full(tpe, id))
    }

    fn read_partial(
        &self,
        tpe: FileType,
        id: &rustic_core::Id,
        cacheable: bool,
        offset: u32,
        length: u32,
    ) -> rustic_core::RusticResult<bytes::Bytes> {
        // Ruling 7's offline half: the cache may serve what an earlier
        // remote fetch stored; the copy serves the miss; and nothing this
        // backend reads is ever stored (the same ciphertext is already on
        // the disk, whole).
        if crate::body_cache::BodyCacheBackend::is_body_read(tpe, cacheable) {
            if let Some(cache) = &self.body_cache {
                let key = crate::body_cache::CacheKey::new(id, offset, length);
                if let Some(bytes) = cache.get(&key) {
                    return Ok(bytes);
                }
                let bytes = self.read_from_copy(tpe, id, |backend| {
                    backend.read_partial(tpe, id, cacheable, offset, length)
                })?;
                return Ok(bytes);
            }
        }
        self.read_from_copy(tpe, id, |backend| {
            backend.read_partial(tpe, id, cacheable, offset, length)
        })
    }

    fn warmup_path(&self, tpe: FileType, id: &rustic_core::Id) -> String {
        self.mirror.warmup_path(tpe, id)
    }

    fn needs_warm_up(&self) -> bool {
        // The copy is a local backend: it needs no warming.
        false
    }

    fn warm_up(&self, tpe: FileType, id: &rustic_core::Id) -> rustic_core::RusticResult<()> {
        self.mirror.warm_up(tpe, id)
    }
}

impl WriteBackend for OfflineMirrorBackend {
    fn create(&self) -> rustic_core::RusticResult<()> {
        offline_write_refusal()
    }

    fn write_bytes(
        &self,
        _tpe: FileType,
        _id: &rustic_core::Id,
        _cacheable: bool,
        _buf: bytes::Bytes,
    ) -> rustic_core::RusticResult<()> {
        offline_write_refusal()
    }

    fn remove(
        &self,
        _tpe: FileType,
        _id: &rustic_core::Id,
        _cacheable: bool,
    ) -> rustic_core::RusticResult<()> {
        offline_write_refusal()
    }
}

/// The refusal a write through the offline seam earns: `--offline` marks a
/// read-only run, and the mirror is a copy only `mirror sync` writes — a
/// write here would be a second, unverified copier (ADR-062 D2's rejected
/// alternative) or a write to a repository the switch promised not to
/// contact. Loud and typed, never silently routed.
fn offline_write_refusal() -> rustic_core::RusticResult<()> {
    Err(rustic_core::RusticError::new(
        rustic_core::ErrorKind::Backend,
        format!(
            "A write was attempted through an `--offline` store — `--offline` serves reads from \
             the local mirror only and never constructs the destination's backend, so this \
             write was refused before anything was written. Run the command without \
             `--offline` to write the declared repository.",
        ),
    ))
}

impl ReadBackend for MirrorFirstBackend {
    fn location(&self) -> String {
        // The store's identity stays the destination: error messages,
        // `repo` lines and `--json` outputs continue to name the repository
        // the config declares, never this machine's copy of it.
        self.remote.location()
    }

    fn list_with_size(
        &self,
        tpe: FileType,
    ) -> rustic_core::RusticResult<Vec<(rustic_core::Id, u32)>> {
        let remote_answer = self.remote.list_with_size(tpe);
        if matches!(&remote_answer, Ok(listed) if !listed.is_empty()) {
            // The remote's non-empty answer stands — presence is its to say
            // while it can say it, and a behind mirror must never shrink it.
            return remote_answer;
        }
        // The empty or erroneous answer is the degraded-source shape: a
        // destination wiped and recreated at the same path lists nothing
        // while the copy still holds the last synced set, so the mirror's
        // answer is what reads run on here (ADR-062 D2), counted so the
        // command's report says so. When the mirror holds nothing of this
        // type either, the remote's own answer is what the caller gets —
        // including the genuine "no repository" (or the brand-new, empty
        // one) an empty remote owes its reader.
        match self.mirror.list_with_size(tpe) {
            Ok(mirror) if !mirror.is_empty() => {
                self.reads
                    .counters
                    .mirror_listings
                    .fetch_add(1, Ordering::Relaxed);
                Ok(mirror)
            }
            _ => remote_answer,
        }
    }

    fn read_full(
        &self,
        tpe: FileType,
        id: &rustic_core::Id,
    ) -> rustic_core::RusticResult<bytes::Bytes> {
        if tpe == FileType::Config {
            // The one non-content-addressed file: remote-first, so a
            // mid-fill mirror can never hand yesterday's repository layout
            // to an open, and the mirror still answers offline (the remote
            // errors → fallback below, the same shape as the listing rule).
            return match self.remote.read_full(tpe, id) {
                Ok(bytes) => {
                    let len = bytes.len();
                    self.reads.counters.note_read(false, tpe, id, len);
                    Ok(bytes)
                }
                Err(_) => match self.mirror.read_full(tpe, id) {
                    Ok(bytes) => {
                        let len = bytes.len();
                        self.reads.counters.note_read(true, tpe, id, len);
                        Ok(bytes)
                    }
                    // Two honest failures, not one: a config present on the
                    // copy but unreadable is the same corruption the
                    // mirror-first legs refuse (a hard failure, never a
                    // second retry — there is no third leg); absent, the
                    // mirror's own short answer is the news: the remote
                    // was unreachable and no copy holds the layout.
                    Err(error) => {
                        if mirror_file_path(&self.reads.mirror_root, tpe, id).exists() {
                            return Err(corrupt_copy_refusal(tpe, id, *error));
                        }
                        Err(error)
                    }
                },
            };
        }
        self.read_local_first(tpe, id, |backend| backend.read_full(tpe, id))
            .map(|(bytes, _from_mirror)| bytes)
    }

    fn read_partial(
        &self,
        tpe: FileType,
        id: &rustic_core::Id,
        cacheable: bool,
        offset: u32,
        length: u32,
    ) -> rustic_core::RusticResult<bytes::Bytes> {
        // Ruling 7's half a body-cache-enabled store owes: a hit serves from
        // the cache as today (entries can only ever have come from remote
        // fetches — mirror-served bytes are never stored), and on a miss the
        // mirror serves without storing. Storing happens on the fallback leg
        // alone, below — ADR-034's whole cache policy, plus one rule.
        if crate::body_cache::BodyCacheBackend::is_body_read(tpe, cacheable) {
            if let Some(cache) = &self.body_cache {
                let key = crate::body_cache::CacheKey::new(id, offset, length);
                if let Some(bytes) = cache.get(&key) {
                    return Ok(bytes);
                }
                let (bytes, from_mirror) = self.read_local_first(tpe, id, |backend| {
                    backend.read_partial(tpe, id, cacheable, offset, length)
                })?;
                if !from_mirror {
                    // Only bytes that actually crossed from the remote may
                    // enter the cache (ruling 7: mirror reads never do — the
                    // same ciphertext is already on disk, whole).
                    cache.put(&key, &bytes);
                }
                return Ok(bytes);
            }
        }
        self.read_local_first(tpe, id, |backend| {
            backend.read_partial(tpe, id, cacheable, offset, length)
        })
        .map(|(bytes, _from_mirror)| bytes)
    }

    fn warmup_path(&self, tpe: FileType, id: &rustic_core::Id) -> String {
        self.remote.warmup_path(tpe, id)
    }

    fn needs_warm_up(&self) -> bool {
        self.remote.needs_warm_up()
    }

    fn warm_up(&self, tpe: FileType, id: &rustic_core::Id) -> rustic_core::RusticResult<()> {
        // reason: warm-ups prime a remote's cold tier; the local copy needs
        // no warming, and routing this to the remote is what keeps the
        // backend's own warm-up semantics (the only semantics that exist)
        // authoritative.
        self.remote.warm_up(tpe, id)
    }
}

impl WriteBackend for MirrorFirstBackend {
    fn create(&self) -> rustic_core::RusticResult<()> {
        self.remote.create()
    }

    fn write_bytes(
        &self,
        tpe: FileType,
        id: &rustic_core::Id,
        cacheable: bool,
        buf: bytes::Bytes,
    ) -> rustic_core::RusticResult<()> {
        // Writes are the remote's alone: only `mirror sync` writes the copy
        // (LM-2), and a read-side seam that also wrote would be a second,
        // unverified copier (ADR-062 D2's rejected alternative).
        self.remote.write_bytes(tpe, id, cacheable, buf)
    }

    fn remove(
        &self,
        tpe: FileType,
        id: &rustic_core::Id,
        cacheable: bool,
    ) -> rustic_core::RusticResult<()> {
        self.remote.remove(tpe, id, cacheable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(
        destinations: &[(&str, &str)],
        mirror: Option<(&bool, &str)>,
    ) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        for (name, repo) in destinations {
            config.destinations.insert(
                (*name).to_string(),
                crate::config::DestinationConfig {
                    repo: Some((*repo).to_string()),
                    ..Default::default()
                },
            );
        }
        if let Some((enabled, mirror_of)) = mirror {
            config.mirror = Some(crate::config::MirrorSectionConfig {
                enabled: Some(*enabled),
                mirror_of: Some((*mirror_of).to_string()),
                min_free_gib: None,
            });
        }
        config
    }

    fn resolved(repo: &str) -> StoreConfig {
        StoreConfig {
            repo_root: repo.to_string(),
            ..Default::default()
        }
    }

    /// A decision helper that builds, for a temp data root, the state a
    /// serving decision needs: a mirror directory that exists.
    fn temp_mirror(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::mirror::mirror_dir(dir.path(), name);
        std::fs::create_dir_all(&root).unwrap();
        (dir, root)
    }

    /// No `[mirror]` section: the default-off machine. Silent, remote-only,
    /// and the handle is `None` — the guarantee `verify` relies on when it
    /// builds no mirror seam at all.
    #[test]
    fn no_section_is_unconfigured_and_silent() {
        let config = config_with(&[("storagebox", "/remote/repo")], None);
        let decision = decide(&config, &resolved("/remote/repo"), Path::new("/data"));
        assert!(matches!(decision, MirrorReadDecision::Unconfigured));
        assert!(decision.handle().is_none());
        assert!(decision.state_line().is_none());
    }

    /// The switch is off: reads go to the source (ADR-062 D1), stated in one
    /// line because the machine did declare a mirror — and never serving.
    #[test]
    fn a_disabled_switch_reads_the_source_and_says_so() {
        let enabled = false;
        let config = config_with(
            &[("storagebox", "/remote/repo")],
            Some((&enabled, "storagebox")),
        );
        let decision = decide(&config, &resolved("/remote/repo"), Path::new("/data"));
        assert!(matches!(decision, MirrorReadDecision::SwitchOff));
        assert!(decision.handle().is_none());
        let line = decision.state_line().unwrap_or_default();
        assert!(line.contains("off"), "{line}");
        assert!(line.contains("enabled = false"), "{line}");
    }

    /// Every unusable shape stays loud — unset `mirror_of`, a destination
    /// that is not declared, an unresolvable repo — because the fallback to
    /// the remote is exactly what happens, and happening invisibly is what
    /// the honesty rules forbid.
    #[test]
    fn unusable_mirrors_fall_back_to_the_remote_visibly() {
        let enabled = true;
        // mirror_of empty.
        let config = config_with(&[("storagebox", "/remote/repo")], Some((&enabled, "")));
        let decision = decide(&config, &resolved("/remote/repo"), Path::new("/data"));
        match &decision {
            MirrorReadDecision::Unusable { why, .. } => {
                assert!(why.contains("mirror_of"), "{why}");
            }
            other => panic!("empty mirror_of is unusable, got {other:?}"),
        }
        assert!(decision.state_line().is_some());

        // names no declared destination.
        let config = config_with(&[("storagebox", "/remote/repo")], Some((&enabled, "nope")));
        let decision = decide(&config, &resolved("/remote/repo"), Path::new("/data"));
        match &decision {
            MirrorReadDecision::Unusable { why, .. } => {
                assert!(why.contains("names no declared destination"), "{why}");
            }
            other => panic!("an undeclared mirror_of is unusable, got {other:?}"),
        }

        // no local copy on disk yet.
        let config = config_with(
            &[("storagebox", "/remote/repo")],
            Some((&enabled, "storagebox")),
        );
        let decision = decide(
            &config,
            &resolved("/remote/repo"),
            Path::new("/definitely/not/a/data/root"),
        );
        match &decision {
            MirrorReadDecision::Unusable { why, .. } => {
                assert!(why.contains("mirror sync"), "{why}");
            }
            other => panic!("no on-disk copy is unusable, got {other:?}"),
        }
        let line = decision.state_line().unwrap_or_default();
        assert!(line.contains("reads the remote"), "{line}");
        assert!(decision.handle().is_none());
    }

    /// Another destination — the second declared copy, or an explicit
    /// `--repo` elsewhere — is not a mirror event: silent, remote, unchanged.
    #[test]
    fn another_destination_is_silent_and_remote() {
        let enabled = true;
        let config = config_with(
            &[("storagebox", "/remote/repo"), ("r2", "/other/repo")],
            Some((&enabled, "storagebox")),
        );
        let decision = decide(&config, &resolved("/other/repo"), Path::new("/data"));
        assert!(matches!(decision, MirrorReadDecision::OtherDestination));
        assert!(decision.handle().is_none());
        assert!(decision.state_line().is_none());
    }

    /// The serving shape: the seam exists only when the store resolves to
    /// the mirrored repository — `--repo` pointing at the same repo counts
    /// (it is the same archive), a root that merely shares the string-style
    /// of the path does not mislead the comparison, and the handle names the
    /// destination it serves.
    #[test]
    fn a_mirrored_repository_serves_mirror_first() {
        let enabled = true;
        let (_dir, _root) = temp_mirror("storagebox");
        let data_root = _dir.path();
        let config = config_with(
            &[("storagebox", "/remote/repo")],
            Some((&enabled, "storagebox")),
        );
        let decision = decide(&config, &resolved("/remote/repo"), data_root);
        match &decision {
            MirrorReadDecision::Serving(reads) => {
                assert_eq!(reads.mirror_of, "storagebox");
                assert_eq!(
                    reads.mirror_root,
                    crate::mirror::mirror_dir(data_root, "storagebox")
                );
                assert_eq!(
                    reads.counters(),
                    MirrorCounters::default(),
                    "a fresh handle has served nothing yet"
                );
            }
            other => panic!("a filled mirror serves, got {other:?}"),
        }
        let line = decision.state_line().unwrap_or_default();
        assert!(line.contains("first"), "{line}");
        assert!(line.contains("local copy"), "{line}");
        // A trailing-slash-vs-not spelling of the same root is the same
        // path component-wise, so it keeps serving; a different path is
        // silent.
        let decision = decide(&config, &resolved("/remote/repo/"), data_root);
        assert!(matches!(decision, MirrorReadDecision::Serving(_)));
    }

    /// The behindness sentence's three honest states on the records LM-2
    /// keeps: a completed sync names its time (the HH:MM form 47 §5.5
    /// words), a first fill in progress says its percentage and that reads
    /// fall back, and no completed record never claims a time. The full
    /// read-through behaviour is exercised in
    /// `tests/mirror_read_test.rs` against real records.
    #[test]
    fn behindness_states_match_the_records() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        // No records at all: a directory exists, nothing has completed —
        // the sentence says so, with no time.
        let sentence = behindness_sentence(&state_dir, "storagebox");
        assert!(sentence.contains("no completed sync"), "{sentence}");

        // A completed record naming this destination answers as of HH:MM.
        let state = crate::mirrorstate::MirrorState {
            version: crate::mirrorstate::MIRROR_STATE_VERSION,
            finished_at_unix: 1_800_000_000,
            mirror_of: "storagebox".to_string(),
            closure_complete: std::collections::BTreeSet::new(),
            snapshots_on_remote: 1,
            remote_total_bytes: 4096,
        };
        crate::mirrorstate::save(&state_dir, &state).unwrap();
        let sentence = behindness_sentence(&state_dir, "storagebox");
        assert!(
            sentence.starts_with("local copy as of"),
            "the completed-sync sentence leads with the time, per 47 §5.5: {sentence}"
        );
        assert!(
            sentence.contains(':'),
            "an HH:MM time is present: {sentence}"
        );
        assert!(!sentence.contains("--:--"), "{sentence}");

        // The same record naming another destination is stale, not dated.
        let sentence = behindness_sentence(&state_dir, "storagebox-v2");
        assert!(sentence.contains("unreadable"), "{sentence}");
        assert!(sentence.contains("does not mirror"), "{sentence}");
    }

    /// The behindness sentence's two loudest branches, produced from the
    /// real records LM-2 writes (`mirror-progress.json`, the same shape
    /// `write_progress` lays down) rather than from hand-built strings:
    /// a failed round — with its note, and without one, where nothing may
    /// be invented after the colon — the first fill in progress, and the
    /// precedence rule that a failed round outranks a completed time
    /// earned before it, so a reader never mistakes a dated-but-broken
    /// copy for a current one.
    #[test]
    fn the_failed_round_and_first_fill_sentences_come_from_real_records() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let record =
            |outcome: &str, complete: bool, note: Option<&str>| crate::mirror::MirrorProgress {
                version: crate::mirror::MIRROR_PROGRESS_VERSION,
                mirror_of: "storagebox".to_string(),
                round_finished_at_unix: 1_800_000_300,
                complete,
                outcome: outcome.to_string(),
                outcome_note: note.map(str::to_string),
                remote_total_bytes: 4096,
                mirror_bytes: 1024,
                remote_files: 4,
                mirror_files: 1,
                orphan_files: None,
                orphan_bytes: None,
                downloaded_bytes: None,
                downloaded_files: None,
                torn_files: 0,
                torn_bytes: 0,
            };
        let write = |progress: &crate::mirror::MirrorProgress| {
            std::fs::write(
                crate::mirror::mirror_progress_path(&state_dir),
                serde_json::to_vec(progress).unwrap(),
            )
            .unwrap();
        };

        // A failed round that carries a note: the failure, the two numbers
        // in the one spelling every surface shares, and the note — reads
        // continue from what the copy holds.
        write(&record(
            "failed",
            false,
            Some("the destination could not be read mid-fill (a read error); the raw error is not recorded in state files"),
        ));
        let sentence = behindness_sentence(&state_dir, "storagebox");
        assert!(
            sentence.starts_with("local copy's last sync round failed — downloaded unknown"),
            "the loudest sentence leads with the failure and its numbers: {sentence}"
        );
        assert!(
            sentence.contains(
                "downloaded unknown (the round that wrote the progress record did not \
                 measure the bytes on disk), 1.0 KiB of 4.0 KiB (25%) hash-verified"
            ),
            "the pair rides the read surface in the shared spelling, downloaded first: {sentence}"
        );
        assert!(
            sentence.contains("the destination could not be read mid-fill"),
            "the record's own note is the sentence's cause: {sentence}"
        );
        assert!(
            sentence.contains(
                "reads continue from what the mirror already holds and fall back to the remote"
            ),
            "and it says where reads go on from there: {sentence}"
        );

        // The same record with no note: the numbers stand, and nothing is
        // invented after the colon — an incomplete sentence, never a
        // fabricated cause (the comment this test defends in code).
        write(&record("failed", false, None));
        let sentence = behindness_sentence(&state_dir, "storagebox");
        assert!(
            sentence.contains("failed — downloaded unknown"),
            "{sentence}"
        );
        let after_colon = sentence
            .split_once("hash-verified: ")
            .unwrap_or_else(|| panic!("the sentence carries its numbers: {sentence}"))
            .1;
        assert!(
            after_colon.trim_start().starts_with("— reads continue"),
            "an empty note invents nothing after the colon, only whitespace before the \
             continuation: {sentence}"
        );

        // A completed sync earned before the failure must not read as the
        // copy's age: the fill states speak first, and a failed round
        // says itself even when a dated record exists.
        let state = crate::mirrorstate::MirrorState {
            version: crate::mirrorstate::MIRROR_STATE_VERSION,
            finished_at_unix: 1_800_000_000,
            mirror_of: "storagebox".to_string(),
            closure_complete: std::collections::BTreeSet::new(),
            snapshots_on_remote: 1,
            remote_total_bytes: 4096,
        };
        crate::mirrorstate::save(&state_dir, &state).unwrap();
        let sentence = behindness_sentence(&state_dir, "storagebox");
        assert!(
            sentence.contains("last sync round failed"),
            "the failure outranks an earlier completed time: {sentence}"
        );
        assert!(
            !sentence.contains("as of"),
            "no dated copy while the last round failed reads as current: {sentence}"
        );

        // First fill in progress: the two numbers in the shared spelling,
        // downloaded first, its fallback wording, and its own "not a
        // failure" honesty.
        write(&record(
            "stopped",
            false,
            Some("round budget spent mid-fill; the next round resumes"),
        ));
        let sentence = behindness_sentence(&state_dir, "storagebox");
        assert!(
            sentence.starts_with(
                "local copy's first sync is in progress: downloaded unknown (the round that \
                 wrote the progress record did not measure the bytes on disk), 1.0 KiB of \
                 4.0 KiB (25%) hash-verified — reads fall back to the remote for what has \
                 not synced; this is not a failure"
            ),
            "first-fill is its own loud sentence: {sentence}"
        );
    }

    /// `served_line` quotes the counters, and both count **files** —
    /// rev2 ①: hand-set counters had once proven only that the line echoes
    /// them, never that they count files, so this drives real reads
    /// through real backends and checks both standards at once. The
    /// counts: one file served from the copy in three range reads is one
    /// file, every range's bytes moved; one absent file read from the
    /// remote is the other leg's one file. And the words: the remote
    /// crossings are worded as reads, never as fallbacks — a fully synced
    /// mirror is never "missing" the config it never tried to keep.
    /// The whole-repository form of the same lock, against an independent
    /// count of the copy's directory, is in `tests/mirror_read_test.rs`.
    #[test]
    fn the_served_line_quotes_the_counters() {
        use rustic_core::Id;

        let (_dir, root) = temp_mirror("storagebox");
        let remote_dir = tempfile::tempdir().unwrap();
        let options =
            BackendOptions::default().repository(remote_dir.path().to_string_lossy().into_owned());
        let remote: Arc<dyn WriteBackend> = options.to_backends().unwrap().repository();

        // The copy holds one pack; the remote holds nothing (any byte that
        // answered came from the copy) and one index file the copy lacks —
        // the ordinary absent-file fallback, on the remote's side for once.
        let pack_id: Id = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
            .parse()
            .unwrap();
        let pack = vec![0x5au8; 100];
        let shard = root.join("data").join("00");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join(pack_id.to_hex()), &pack).unwrap();
        let index_id: Id = "010102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
            .parse()
            .unwrap();
        let index = vec![0x77u8; 30];
        let remote_index = remote_dir.path().join("index");
        std::fs::create_dir_all(&remote_index).unwrap();
        std::fs::write(remote_index.join(index_id.to_hex()), &index).unwrap();

        let reads = MirrorReads {
            mirror_of: "storagebox".to_string(),
            mirror_root: root,
            counters: CounterSet::default(),
        };
        let reads = Arc::new(reads);
        let backend = MirrorFirstBackend::wrap(remote, Arc::clone(&reads), None).unwrap();

        // One pack, three ranged reads — one file, 110 bytes moved.
        for (offset, length) in [(0, 40), (40, 50), (10, 20)] {
            let bytes = backend
                .read_partial(FileType::Pack, &pack_id, false, offset, length)
                .expect("the copy answers this range");
            assert_eq!(
                bytes.to_vec(),
                &pack[offset as usize..(offset + length) as usize]
            );
        }
        // One absent file, one remote read.
        let bytes = backend
            .read_full(FileType::Index, &index_id)
            .expect("the absent file reads from the remote");
        assert_eq!(bytes.to_vec(), index);

        let counters = reads.counters();
        assert_eq!(
            counters,
            MirrorCounters {
                mirror_files: 1,
                mirror_bytes: 110,
                remote_files: 1,
                remote_bytes: 30,
                mirror_listings: 0,
            },
            "one file per leg no matter how many reads moved it"
        );
        let line = reads.served_line();
        assert!(
            line.contains("1 file(s) (110 B) from the local mirror"),
            "the line quotes the mirror leg's files and bytes: {line}"
        );
        assert!(
            line.contains("1 file(s) (30 B) read from the remote"),
            "and the remote leg's: {line}"
        );
        assert!(
            line.contains("listing(s) answered locally"),
            "the listing count is a visible family member: {line}"
        );
        assert!(
            !line.contains("fell back"),
            "the config read is a deliberate crossing, and this line never claims which \
             crossing was a fallback: {line}"
        );
    }

    /// A store that resolves inside the copies directory is the copy
    /// itself, not "another destination": it is the one `--repo` spelling
    /// that reads this machine's mirror, so it gets a line (for `verify`,
    /// the line that keeps a verdict over the copy from passing as a
    /// verdict over the destination) and never a seam handle — the copy
    /// has no copy of its own. The name comes off the path, so a copy
    /// that outlived its config section still states itself, and a
    /// repository merely nested deeper in the directory is not a copy.
    #[test]
    fn a_store_inside_the_copies_dir_is_the_copy_itself() {
        let enabled = true;
        let (dir, _root) = temp_mirror("storagebox");
        let data_root = dir.path();
        let config = config_with(
            &[("storagebox", "/remote/repo")],
            Some((&enabled, "storagebox")),
        );

        let copy = crate::mirror::mirror_dir(data_root, "storagebox");
        let decision = decide(&config, &resolved(&copy.to_string_lossy()), data_root);
        match &decision {
            MirrorReadDecision::MirrorCopyItself { name } => {
                assert_eq!(name, "storagebox");
                assert!(
                    decision.handle().is_none(),
                    "the copy never reads through a seam"
                );
            }
            other => panic!("the copy's own path classifies as the copy, got {other:?}"),
        }
        let line = decision.state_line().unwrap_or_default();
        assert!(
            line.contains("local mirror copy `storagebox` itself"),
            "the line names the copy under read: {line}"
        );
        assert!(
            line.contains("verify"),
            "the line states what a verdict over the copy certifies: {line}"
        );

        // A different copy in the same directory — a name the section does
        // not declare — states itself too; the layout is what recognized
        // it, not the section.
        let retired = crate::mirror::mirror_dir(data_root, "retired-name");
        let decision = decide(&config, &resolved(&retired.to_string_lossy()), data_root);
        assert!(matches!(
            decision,
            MirrorReadDecision::MirrorCopyItself { name } if name == "retired-name"
        ));

        // Nested deeper than the layout: an ordinary other-destination
        // read, silent as before.
        let nested = retired.join("inner-repo");
        let decision = decide(&config, &resolved(&nested.to_string_lossy()), data_root);
        assert!(matches!(decision, MirrorReadDecision::OtherDestination));

        // Just the copies directory itself is no repository the sync wrote;
        // it stays the ordinary silent spelling.
        let decision = decide(
            &config,
            &resolved(&crate::mirror::copies_dir(data_root).to_string_lossy()),
            data_root,
        );
        assert!(matches!(decision, MirrorReadDecision::OtherDestination));
    }

    /// The copy-itself warning is a fact about the path, so it must not
    /// depend on what `[mirror]` declares: a copy that outlived its section
    /// (the section removed entirely) or outlived the switch being thrown
    /// (`enabled = false`) still states itself — for `verify --repo <data
    /// root>/mirror/<name>` that is the one line keeping a copy's verdict
    /// from passing as the destination's own. The layout speaks before the
    /// section states do.
    #[test]
    fn a_copy_states_itself_without_a_section_or_a_live_switch() {
        let enabled = false;
        let (dir, _root) = temp_mirror("storagebox");
        let data_root = dir.path();

        // The section is gone entirely: the retired copy still states itself
        // — loudly, which is exactly what Unconfigured's silence hides.
        let config = config_with(&[("storagebox", "/remote/repo")], None);
        let retired = crate::mirror::mirror_dir(data_root, "retired-name");
        let decision = decide(&config, &resolved(&retired.to_string_lossy()), data_root);
        match &decision {
            MirrorReadDecision::MirrorCopyItself { name } => {
                assert_eq!(
                    name, "retired-name",
                    "a copy that outlived its section states itself under its own name"
                );
            }
            other => panic!("a copy that outlived its section still states itself, got {other:?}"),
        }
        assert!(
            decision.state_line().is_some(),
            "and the copy is not silent about being read"
        );
        assert!(
            decision.handle().is_none(),
            "the copy never reads through a seam"
        );

        // The switch is off: the same fact, the same line — never the
        // `off` line the switch state prints for a remote store.
        let config = config_with(
            &[("storagebox", "/remote/repo")],
            Some((&enabled, "storagebox")),
        );
        let copy = crate::mirror::mirror_dir(data_root, "storagebox");
        let decision = decide(&config, &resolved(&copy.to_string_lossy()), data_root);
        match &decision {
            MirrorReadDecision::MirrorCopyItself { name } => {
                assert_ne!(name, "retired-name");
                assert_eq!(name, "storagebox");
            }
            other => {
                panic!("a disabled switch must not silence the copy-itself line, got {other:?}")
            }
        }
        let line = decision.state_line().unwrap_or_default();
        assert!(
            line.contains("local mirror copy"),
            "the copy's own line, not the switch's: {line}"
        );
        assert!(
            !line.contains("enabled = false"),
            "the switch's line belongs to remote stores, not to the copy: {line}"
        );
    }

    /// A corrupt mirror file is served as answered, never secretly
    /// repaired: the fallback exists for absence (the read errors), and
    /// bit-rot is deliberately a hard failure above the seam — a partial
    /// read cannot re-verify a whole-file id, and substituting the
    /// remote's bytes for a lying disk would read as healthy. This locks
    /// the seam's half (garbage under a valid id is the mirror's answer,
    /// counted as one); the tampered-mirror test in
    /// `tests/mirror_read_test.rs` locks the command's half (the read
    /// fails, `verify` still answers the remote).
    #[test]
    fn a_corrupt_mirror_file_is_served_as_answered_never_secretly_repaired() {
        use rustic_core::Id;

        let (_dir, root) = temp_mirror("storagebox");
        // A real local backend over an empty directory plays the remote:
        // it holds nothing, so any bytes this read returns can only have
        // come from the copy.
        let remote_dir = tempfile::tempdir().unwrap();
        let options =
            BackendOptions::default().repository(remote_dir.path().to_string_lossy().into_owned());
        let remote: Arc<dyn WriteBackend> = options.to_backends().unwrap().repository();

        let id: Id = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
            .parse()
            .unwrap();
        let garbage = vec![0xaau8; 17];
        let shard = root.join("data").join("00");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join(id.to_hex()), &garbage).unwrap();

        let reads = Arc::new(MirrorReads {
            mirror_of: "storagebox".to_string(),
            mirror_root: root,
            counters: CounterSet::default(),
        });
        let backend = MirrorFirstBackend::wrap(remote, Arc::clone(&reads), None).unwrap();
        let bytes = backend
            .read_full(FileType::Pack, &id)
            .expect("the copy answers this read, garbage and all");
        assert_eq!(
            bytes.to_vec(),
            garbage,
            "the seam returns the copy's bytes untouched — detection is not its job"
        );
        assert_eq!(
            reads.counters(),
            MirrorCounters {
                mirror_files: 1,
                mirror_bytes: 17,
                remote_files: 0,
                remote_bytes: 0,
                mirror_listings: 0,
            },
            "the crossing never happened: one mirror file, nothing from the remote"
        );
    }

    /// rev2 ② at seam level: a mirror file that is **present but fails its
    /// read** — the truncated shape, a file too short for the ranges a
    /// pack read asks for — is a hard failure, never a silent remote
    /// retry. The remote here deliberately holds the intact bytes, so the
    /// forbidden fallback would *succeed* and look like the ordinary
    /// absence retry: only the refusal, with nothing counted on either
    /// leg, keeps this green. The path's own arc is the control: delete
    /// the stub file and the same read is the ordinary fallback again —
    /// presence is the whole difference between refusal and retry.
    #[test]
    fn a_present_but_unreadable_mirror_file_is_a_hard_failure_never_a_silent_retry() {
        use rustic_core::Id;

        let (_dir, root) = temp_mirror("storagebox");
        let remote_dir = tempfile::tempdir().unwrap();
        let options =
            BackendOptions::default().repository(remote_dir.path().to_string_lossy().into_owned());
        let remote: Arc<dyn WriteBackend> = options.to_backends().unwrap().repository();

        let id: Id = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
            .parse()
            .unwrap();
        let intact = vec![0x33u8; 17];
        let remote_shard = remote_dir.path().join("data").join("00");
        std::fs::create_dir_all(&remote_shard).unwrap();
        std::fs::write(remote_shard.join(id.to_hex()), &intact).unwrap();
        // The copy holds a truncated stub under the same valid id: present,
        // size-listed, unreadable at the range the read asks for.
        let shard = root.join("data").join("00");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join(id.to_hex()), vec![0x44u8; 5]).unwrap();

        let reads = MirrorReads {
            mirror_of: "storagebox".to_string(),
            mirror_root: root,
            counters: CounterSet::default(),
        };
        let reads = Arc::new(reads);
        let backend = MirrorFirstBackend::wrap(remote, Arc::clone(&reads), None).unwrap();

        let error = backend
            .read_partial(FileType::Pack, &id, false, 0, 17)
            .expect_err(
                "the truncated copy must refuse this read — the intact remote must not rescue it",
            );
        let said = error.to_string();
        assert!(
            said.contains("present but cannot be read"),
            "the refusal states the class of event: {said}"
        );
        assert!(
            said.contains("mirror sync"),
            "the refusal names the repair: {said}"
        );
        assert_eq!(
            reads.counters(),
            MirrorCounters::default(),
            "the read stopped at the damage: nothing served, nothing fell back, no \
             counter may read disk damage as sync progress"
        );

        // Presence is the whole difference: the same file absent is the
        // fallback's own shape — the remote answers, visibly, counted.
        std::fs::remove_file(shard.join(id.to_hex())).unwrap();
        let bytes = backend
            .read_partial(FileType::Pack, &id, false, 0, 17)
            .expect("an absent file reads from the remote — the ordinary retry");
        assert_eq!(
            bytes.to_vec(),
            intact,
            "the absent-file read serves the remote's intact bytes"
        );
        assert_eq!(
            reads.counters(),
            MirrorCounters {
                mirror_files: 0,
                mirror_bytes: 0,
                remote_files: 1,
                remote_bytes: 17,
                mirror_listings: 0,
            },
            "counted as the one remote crossing it was"
        );
    }

    // ------------------------------------------------------------------
    // W1053: the offline seam
    // ------------------------------------------------------------------

    /// `--offline` can be served by exactly the decisions that hold bytes
    /// this machine can read without dialing anything: a serving mirror,
    /// and the copy itself. Every other state refuses, with one sentence
    /// naming the fix — and `offline_refusal()` returning `None` is the
    /// same predicate as `offline_capable()`, which is the lock that keeps
    /// the caller from printing a refusal for a state it then serves.
    #[test]
    fn offline_capability_and_refusals_follow_the_decision() {
        let enabled = true;
        let data_root = Path::new("/data");

        // Unconfigured: no copy exists to serve an offline read.
        let config = config_with(&[("storagebox", "/remote/repo")], None);
        let decision = decide(&config, &resolved("/remote/repo"), data_root);
        assert!(!decision.offline_capable());
        let refusal = decision.offline_refusal().expect("unconfigured refuses");
        assert!(refusal.contains("no `[mirror]` section"), "{refusal}");
        assert!(refusal.contains("mirror sync"), "{refusal}");

        // The switch is off: reads must go to the source, never offline.
        let disabled = false;
        let config = config_with(
            &[("storagebox", "/remote/repo")],
            Some((&disabled, "storagebox")),
        );
        let decision = decide(&config, &resolved("/remote/repo"), data_root);
        assert!(!decision.offline_capable());
        let refusal = decision.offline_refusal().expect("switch off refuses");
        assert!(refusal.contains("enabled = false"), "{refusal}");
        assert!(refusal.contains("mirror sync"), "{refusal}");

        // Enabled but no copy on disk: unusable, and the sentence carries
        // the why the online state line already worded.
        let config = config_with(
            &[("storagebox", "/remote/repo")],
            Some((&enabled, "storagebox")),
        );
        let decision = decide(
            &config,
            &resolved("/remote/repo"),
            Path::new("/definitely/not/a/data/root"),
        );
        assert!(!decision.offline_capable());
        let refusal = decision
            .offline_refusal()
            .expect("an unfilled mirror refuses");
        assert!(
            refusal.contains("mirror sync"),
            "the repair is the fill: {refusal}"
        );
        assert!(
            refusal.contains("may not fall back to the destination"),
            "the refusal states the offline promise: {refusal}"
        );

        // Another destination: silence is the online rule, but an offline
        // run may not become a quiet remote read — it refuses, loudly.
        let config = config_with(
            &[("storagebox", "/remote/repo"), ("r2", "/other/repo")],
            Some((&enabled, "storagebox")),
        );
        let decision = decide(&config, &resolved("/other/repo"), data_root);
        assert!(!decision.offline_capable());
        let refusal = decision
            .offline_refusal()
            .expect("a different archive refuses");
        assert!(
            refusal.contains("does not copy"),
            "the refusal says which mirror would have served: {refusal}"
        );

        // Serving: capable, and there is no refusal sentence to print.
        let (_dir, _root) = temp_mirror("storagebox");
        let config = config_with(
            &[("storagebox", "/remote/repo")],
            Some((&enabled, "storagebox")),
        );
        let decision = decide(&config, &resolved("/remote/repo"), _dir.path());
        assert!(decision.offline_capable());
        assert!(
            decision.offline_refusal().is_none(),
            "a serving mirror has no refusal to pair with its capacity"
        );

        // The copy itself: already local, so `--offline` needs no seam and
        // no refusal — a plain read of it is the offline shape.
        let copy = crate::mirror::mirror_dir(_dir.path(), "storagebox");
        let decision = decide(&config, &resolved(&copy.to_string_lossy()), _dir.path());
        assert!(matches!(
            decision,
            MirrorReadDecision::MirrorCopyItself { .. }
        ));
        assert!(decision.offline_capable());
        assert!(decision.offline_refusal().is_none());
    }

    /// The offline seam answers from the copy and refuses what the copy
    /// lacks, worded as the ruling requires: the refusal names `--offline`,
    /// names the destination, says the remote was not contacted and that the
    /// absence proves nothing — never "cannot read", never an empty answer.
    /// A present-but-unreadable file keeps the online corruption refusal
    /// (damage is damage offline too), a write through the seam is refused
    /// before anything exists to write, and the copy held the *only* intact
    /// bytes nowhere else — there is no remote behind this backend at all.
    #[test]
    fn the_offline_backend_serves_the_copy_and_refuses_what_it_lacks() {
        use rustic_core::Id;

        let (_dir, root) = temp_mirror("storagebox");
        let id: Id = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
            .parse()
            .unwrap();
        let intact = vec![0x7bu8; 17];
        let shard = root.join("data").join("00");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join(id.to_hex()), &intact).unwrap();

        let reads = Arc::new(MirrorReads {
            mirror_of: "storagebox".to_string(),
            mirror_root: root.clone(),
            counters: CounterSet::default(),
        });
        let backend =
            OfflineMirrorBackend::wrap(Arc::clone(&reads), None, "opendal:s3".to_string()).unwrap();

        // The identity stays the destination the config declares.
        assert_eq!(backend.location(), "opendal:s3");

        // A held file serves, byte-identical and counted on the copy's leg.
        let bytes = backend
            .read_full(FileType::Pack, &id)
            .expect("the copy serves the file it holds");
        assert_eq!(bytes.to_vec(), intact);

        // The offline refusal: same layout knowledge as the online seam, so
        // the two roads apart from absence stay apart here too — absent is
        // the offline refusal, the present-but-truncated one corruption.
        let said = {
            let absent: Id = "020102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
                .parse()
                .unwrap();
            let error = backend
                .read_full(FileType::Pack, &absent)
                .expect_err("an offline read refuses what the copy lacks");
            error.to_string()
        };
        assert!(
            said.contains("`--offline`"),
            "the refusal names the switch that owned the rules: {said}"
        );
        assert!(
            said.contains("mirror of `storagebox`"),
            "and the destination whose copy fell short: {said}"
        );
        assert!(
            said.contains("the remote was not contacted"),
            "and the promise kept: {said}"
        );
        assert!(
            said.contains("proves nothing"),
            "an offline absence is never a claim about the archive: {said}"
        );
        assert!(said.contains("mirror sync"), "the repair is named: {said}");

        // Present but unreadable: the corruption refusal, damage being
        // damage offline too — the remote must not rescue it (there is no
        // remote to rescue it, and the wording must not suggest one was
        // asked).
        std::fs::write(shard.join(id.to_hex()), vec![0x5au8; 5]).unwrap();
        let said = backend
            .read_partial(FileType::Pack, &id, false, 0, 17)
            .expect_err("a truncated file refuses the read")
            .to_string();
        assert!(
            said.contains("present but cannot be read"),
            "the corruption class keeps its own words offline: {said}"
        );
        assert!(said.contains("disk damage"), "and its own cause: {said}");
        std::fs::write(shard.join(id.to_hex()), &intact).unwrap();

        // A write through the offline seam is refused before anything is
        // written: `--offline` marks a read-only run and only `mirror sync`
        // may write the copy.
        let said = backend
            .write_bytes(FileType::Pack, &id, true, bytes.clone())
            .expect_err("writes refuse through the offline seam")
            .to_string();
        assert!(
            said.contains("serves reads from the local mirror only"),
            "the refusal states why a write cannot happen offline: {said}"
        );
        assert!(
            said.contains("without `--offline`"),
            "and names the run that writes: {said}"
        );

        // The served line: the ruling's phrase, the copy's leg, no remote
        // leg presented as a measured zero.
        let line = reads.offline_served_line();
        assert!(
            line.starts_with("served from mirror `storagebox`"),
            "every offline output states served from mirror: {line}"
        );
        assert!(
            line.contains("nothing from the remote (--offline"),
            "the remote half words the switch, not a measurement: {line}"
        );
        assert!(
            !line.contains("read from the remote"),
            "the offline line never borrows the online line's remote wording: {line}"
        );
        // And the pre-read announcement promises the same thing.
        let state = reads.offline_state_line();
        assert!(
            state.contains("served from mirror `storagebox`"),
            "the pre-read line uses the phrase too: {state}"
        );
        assert!(
            state.contains("the destination is not contacted"),
            "and states the promise in full: {state}"
        );
    }

    /// The offline listing is the copy's answer, and a non-empty one is
    /// counted as locally answered so the served line says so; the store
    /// the CLI builds over this seam reports listings from the copy the
    /// same way the online fallback's degraded-source leg does.
    #[test]
    fn an_offline_listing_answer_is_the_copys_and_is_counted() {
        use rustic_core::Id;

        let (_dir, root) = temp_mirror("storagebox");
        let id: Id = "030102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
            .parse()
            .unwrap();
        let shard = root.join("data").join("03");
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join(id.to_hex()), vec![0x11u8; 9]).unwrap();

        let reads = Arc::new(MirrorReads {
            mirror_of: "storagebox".to_string(),
            mirror_root: root,
            counters: CounterSet::default(),
        });
        let backend =
            OfflineMirrorBackend::wrap(Arc::clone(&reads), None, "/declared/repo".to_string())
                .unwrap();
        let listed = backend
            .list_with_size(FileType::Pack)
            .expect("the copy answers the listing");
        assert_eq!(listed.len(), 1, "one pack, the copy's own answer");
        assert_eq!(
            reads.counters().mirror_listings,
            1,
            "a non-empty offline listing is a locally answered listing"
        );

        // The empty answer is passed through uncounted — the same shape the
        // online seam gives the caller for it to word, never padded, never
        // silently treated as a remote shortcoming (there is no remote).
        let empty = backend
            .list_with_size(FileType::Index)
            .expect("an empty listing is an answer, not an error");
        assert!(empty.is_empty());
        assert_eq!(
            reads.counters().mirror_listings,
            1,
            "an empty offline listing is not a served-listing event"
        );
    }
}
