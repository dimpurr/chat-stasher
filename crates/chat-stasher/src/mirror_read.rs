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
//!   because the read errors and the remote answers, visibly. A file that
//!   is corrupt rather than absent defeats the copy's own premise (the bytes
//!   an id names, held under that id) and is deliberately a **hard failure,
//!   never a silent remote retry**: a partial read cannot re-verify a
//!   whole-file id, so detection does not belong in this seam; the damage
//!   surfaces where decryption and re-hashing live, above it, and the read
//!   stops honestly rather than fetch replacement bytes without saying so.
//!   `verify`, remote-only, keeps answering the only integrity
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

/// What one command's reads did through the mirror seam. Every number is a
/// count of events in this process, never a guess, so a line built from them
/// can be checked against the read it describes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MirrorCounters {
    /// Immutable repository files served from the local copy
    /// (`read_full` + `read_partial`, every non-config file type).
    pub mirror_reads: u64,
    /// Bytes those reads moved off the local disk.
    pub mirror_bytes: u64,
    /// Files read from the remote rather than the local copy: the 47
    /// §3.6 fallback for a file the mirror lacks, plus the deliberate
    /// remote-first `config` read (the one non-content-addressed file), so
    /// a fully synced mirror online still reads one `config` from the
    /// remote per open. Every one of these kept the remote's connection
    /// budget in play; the number alone is never a claim that the mirror
    /// was missing a file — the served line words it that way on purpose.
    pub remote_reads: u64,
    /// Bytes those reads fetched from the remote.
    pub remote_bytes: u64,
    /// Listings the mirror answered when the remote would not — the
    /// offline listing that errors, and the empty answer of a destination
    /// wiped and recreated at the same path — the read that still worked
    /// because of the copy, and the reason its snapshot set is only as
    /// new as the last sync.
    pub mirror_listings: u64,
}

#[derive(Debug, Default)]
struct CounterSet {
    mirror_reads: AtomicU64,
    mirror_bytes: AtomicU64,
    remote_reads: AtomicU64,
    remote_bytes: AtomicU64,
    mirror_listings: AtomicU64,
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
    /// A snapshot of what this process's reads did through the seam.
    #[must_use]
    pub fn counters(&self) -> MirrorCounters {
        MirrorCounters {
            mirror_reads: self.counters.mirror_reads.load(Ordering::Relaxed),
            mirror_bytes: self.counters.mirror_bytes.load(Ordering::Relaxed),
            remote_reads: self.counters.remote_reads.load(Ordering::Relaxed),
            remote_bytes: self.counters.remote_bytes.load(Ordering::Relaxed),
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
    /// could not check out.
    #[must_use]
    pub fn served_line(&self) -> String {
        let counters = self.counters();
        format!(
            "{} file(s) ({} B) from the local mirror, {} file(s) ({} B) read from the \
             remote, {} listing(s) answered locally",
            counters.mirror_reads,
            counters.mirror_bytes,
            counters.remote_reads,
            counters.remote_bytes,
            counters.mirror_listings
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
    /// (ADR-001 ⑥), reads the remote exactly as before. Silent — there is no
    /// mirror event to report.
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
    /// fact about every answer it returns. `verify` over this store is
    /// the spelling the ADR's absolute wording hides: the copy *is* the
    /// mirror, so the one line says a verdict here certifies the copy,
    /// never the destination's repository (47 §5.5).
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
/// `data_root` is passed in rather than read from the environment so tests
/// isolate as explicitly as the LM-2 engine does — the caller's one
/// `config::default_data_root()` call keeps production on the real root.
#[must_use]
pub fn decide(config: &Config, resolved: &StoreConfig, data_root: &Path) -> MirrorReadDecision {
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
        // One spelling inside that arm is not "another archive": a store
        // under the copies directory is one of this machine's local
        // copies, and reading it is reading the copy itself. Every
        // mirror-aware command owes that one line — for `verify` it is
        // also the spelling where silence would let a copy's verdict pass
        // as the destination's own (47 §5.5: a copy may not certify
        // itself).
        if let Some(name) = mirror_copy_name(data_root, &resolved.repo_root) {
            return MirrorReadDecision::MirrorCopyItself { name };
        }
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
/// the wording the ADR quotes.
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
                    "local copy's last sync round failed at {}% ({} of {} B): {note} — reads \
                     continue from what the mirror already holds and fall back to the remote",
                    record.percent(),
                    record.mirror_bytes,
                    record.remote_total_bytes
                );
            }
            if !record.complete {
                return format!(
                    "local copy's first sync is in progress: {}% ({} of {} B) — reads fall \
                     back to the remote for what has not synced; this is not a failure",
                    record.percent(),
                    record.mirror_bytes,
                    record.remote_total_bytes
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
///   the mirror *lacks*, never for a corrupt one — a damaged copy is a
///   hard failure by the module rule above. A body read through a
///   body-cache-enabled store additionally consults the cache first, and
///   stores only what the fallback fetched from the remote.
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
    /// errors, the remote answers, visibly. A file that is corrupt rather
    /// than absent answers this read and the damage surfaces above the
    /// seam, deliberately (the module header's absent-and-corrupt rule):
    /// a partial read cannot re-verify a whole-file id, so the seam has no
    /// honest way to detect bit-rot, and silently substituting the remote's
    /// bytes for a lying disk is a repair nobody asked the read to make.
    fn read_local_first(
        &self,
        read: impl Fn(&dyn WriteBackend) -> rustic_core::RusticResult<bytes::Bytes>,
    ) -> rustic_core::RusticResult<(bytes::Bytes, bool)> {
        match read(&self.mirror) {
            Ok(bytes) => {
                self.count_read(true, bytes.len());
                Ok((bytes, true))
            }
            Err(_) => {
                let bytes = read(&self.remote)?;
                self.count_read(false, bytes.len());
                Ok((bytes, false))
            }
        }
    }

    fn count_read(&self, from_mirror: bool, len: usize) {
        let len = len as u64;
        if from_mirror {
            self.reads
                .counters
                .mirror_reads
                .fetch_add(1, Ordering::Relaxed);
            self.reads
                .counters
                .mirror_bytes
                .fetch_add(len, Ordering::Relaxed);
        } else {
            self.reads
                .counters
                .remote_reads
                .fetch_add(1, Ordering::Relaxed);
            self.reads
                .counters
                .remote_bytes
                .fetch_add(len, Ordering::Relaxed);
        }
    }
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
                    self.count_read(false, len);
                    Ok(bytes)
                }
                Err(_) => {
                    let bytes = self.mirror.read_full(tpe, id)?;
                    let len = bytes.len();
                    self.count_read(true, len);
                    Ok(bytes)
                }
            };
        }
        self.read_local_first(|backend| backend.read_full(tpe, id))
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
                let (bytes, from_mirror) = self.read_local_first(|backend| {
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
        self.read_local_first(|backend| backend.read_partial(tpe, id, cacheable, offset, length))
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

    /// `served_line`'s numbers are the counters the seam moved — the line a
    /// command prints after its reads must be checkable against the same
    /// handle, so the visibility never drifts from the facts. That
    /// standard covers the words as well as the counts: the remote-first
    /// `config` read is one of `remote_reads`, so a line that called every
    /// one of them a fallback would misdescribe a fully synced mirror.
    #[test]
    fn the_served_line_quotes_the_counters() {
        let (_dir, root) = temp_mirror("storagebox");
        let reads = MirrorReads {
            mirror_of: "storagebox".to_string(),
            mirror_root: root,
            counters: CounterSet::default(),
        };
        reads.counters.mirror_reads.store(7, Ordering::Relaxed);
        reads.counters.mirror_bytes.store(700, Ordering::Relaxed);
        reads.counters.remote_reads.store(2, Ordering::Relaxed);
        reads.counters.remote_bytes.store(40, Ordering::Relaxed);
        reads.counters.mirror_listings.store(1, Ordering::Relaxed);
        let line = reads.served_line();
        assert!(line.contains("7 file(s)"), "{line}");
        assert!(line.contains("2 file(s)"), "{line}");
        assert!(line.contains("listing(s)"), "{line}");
        assert!(
            line.contains("read from the remote"),
            "remote crossings are worded as reads, not fallbacks: {line}"
        );
        assert!(
            !line.contains("fell back"),
            "the config read is a deliberate crossing, and this line never claims which \
             crossing was a fallback: {line}"
        );
        assert_eq!(
            reads.counters(),
            MirrorCounters {
                mirror_reads: 7,
                mirror_bytes: 700,
                remote_reads: 2,
                remote_bytes: 40,
                mirror_listings: 1,
            },
            "the line above was built from exactly these counters"
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
        std::fs::write(shard.join(id.to_hex().to_string()), &garbage).unwrap();

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
                mirror_reads: 1,
                mirror_bytes: 17,
                remote_reads: 0,
                remote_bytes: 0,
                mirror_listings: 0,
            },
            "the crossing never happened: one mirror read, nothing from the remote"
        );
    }
}
