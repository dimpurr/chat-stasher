//! BackupStore — wrap `rustic_core` so a batch of sealed session shards is
//! moved into an append-only rustic repository.
//!
//! Rules fixed by spike measurements (see spike report, do not re-derive):
//!   * **Sealed shards, not chunking, is the incremental scheme.** A shard
//!     file is written once and never appended to again. Old shards keep their
//!     size+mtime so rustic's file-level parent match hits `files_unmodified`
//!     and the old files are never re-read (`data_added ≈ 0`).
//!   * **Paths are partitioned**: `sessions/<machine>/<session-id>/<bucket>/NNNNNN.jsonl`.
//!     Two machines can never claim the same path, so every machine's data is
//!     permanently addressable. The zero-padded sequence keeps lexicographic
//!     order equal to chronological order. The reader also accepts the legacy
//!     unbucketed `sessions/<machine>/<session-id>/NNNNNN.jsonl` layout.
//!   * **The chunker is left at rustic's default** (Rabin / 1 MiB) — the
//!     sealed-shard scheme does not depend on it.
//!   * **append_only is set at init time** — after the chunker is fixed, so the
//!     repository config is sealed from day one (`apply_config` is rejected in
//!     append-only repos, verified in spike A4).
//!   * **The snapshot `host` is pinned** to the same normalised machine name
//!     used for the path partition (`id::normalize_machine`), so a reinstall /
//!     rename cannot silently split one machine into several snapshot groups.
//!   * **Concurrency is a config knob, default ≤ 10.** rustic's read side fans
//!     out to ~CPU cores via rayon/pariter and knows nothing about a remote
//!     endpoint's connection limit; we cap it here so the limit exists before
//!     a remote backend is wired in (spike A4 bound the SFTP path via the
//!     backend `connections` option; locally we pin the global rayon pool,
//!     which backs `decrypt.rs`'s read fan-out).

use anyhow::{anyhow, Context};
use rayon::ThreadPoolBuilder;
use rustic_backend::BackendOptions;
use rustic_core::repofile::{MasterKey, NodeType, SnapshotFile};
use rustic_core::{
    BackupOptions, ConfigOptions, Credentials, FileType, IndexedFullStatus, KeyOptions,
    LocalSourceSaveOptions, LsOptions, NoProgressBars, ParentOptions, PathList, ProgressBars,
    Repository, RepositoryBackends, RepositoryOptions, SnapshotOptions, TimeOption,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Hard ceiling for concurrency handed to rustic.
///
/// Bounded by what a remote backend will accept (Hetzner Storage Box allows 10
/// simultaneous SFTP connections), not by anything we measured.
pub const MAX_CONNECTIONS: usize = 10;

/// Default concurrency handed to rustic.
///
/// Deliberately well below `MAX_CONNECTIONS`. D2 measured that `connections`
/// is not a free knob and does not buy speed: at `connections=10` a single
/// `read` opened a peak of 4 independent ssh ControlMasters (vs exactly 1 at
/// `connections=1`), while wall clock was 11.32 s vs 10.00 s — i.e. raising it
/// only adds masters that must later be reaped. The default is therefore a
/// measured trade-off, not the backend's limit.
pub const DEFAULT_CONNECTIONS: usize = 4;
/// Top-level directory inside the repository holding every machine's shards.
pub const SESSIONS_DIR: &str = "sessions";
/// Suffix used for every sealed shard file.
pub const SHARD_SUFFIX: &str = ".jsonl";
/// Default maximum number of sealed shards in one bucket.
///
/// G7 measured 20-shard buckets at 21,568 B fixed overhead on push 200 versus
/// 238,755 B when all shards shared one directory; the cap is therefore a
/// measured bound, not a filesystem limit.
pub const DEFAULT_SHARD_BUCKET_CAP: usize = 20;

/// The only production code paths allowed to create stage content.
///
/// The registry is deliberately kept next to the low-level stage writer: a
/// new writer must name itself here and provide its reconciliation hook before
/// it can call the writer API. The unit test below makes a missing hook a
/// visible failure instead of relying on a convention in a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageWriter {
    Collect,
    Ingest,
    Seal,
    /// `dest-init`: shards copied back from an *existing* destination because
    /// the local source no longer has them (ADR-013 difference set).
    Restore,
}

impl StageWriter {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Collect => "collect",
            Self::Ingest => "ingest",
            Self::Seal => "seal",
            Self::Restore => "restore",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StageWriterRegistration {
    pub writer: StageWriter,
    pub reconciliation_hook: Option<&'static str>,
}

pub const STAGE_WRITER_REGISTRY: &[StageWriterRegistration] = &[
    StageWriterRegistration {
        writer: StageWriter::Collect,
        reconciliation_hook: Some("collector cursor + stage shard audit"),
    },
    StageWriterRegistration {
        writer: StageWriter::Ingest,
        reconciliation_hook: Some("consumed file_sha256 audit"),
    },
    StageWriterRegistration {
        writer: StageWriter::Seal,
        reconciliation_hook: Some("stage-owned rename audit"),
    },
    StageWriterRegistration {
        writer: StageWriter::Restore,
        reconciliation_hook: Some("restored concat sha256 vs source destination"),
    },
];

/// Runtime guard used by each production writer before it mutates stage.
pub fn assert_stage_writer_audited(writer: StageWriter) -> anyhow::Result<()> {
    let Some(registration) = STAGE_WRITER_REGISTRY
        .iter()
        .find(|registration| registration.writer == writer)
    else {
        anyhow::bail!(
            "stage writer `{}` is not registered for reconciliation",
            writer.name()
        );
    };
    if registration.reconciliation_hook.is_none_or(str::is_empty) {
        anyhow::bail!(
            "stage writer `{}` has no reconciliation hook",
            writer.name()
        );
    }
    Ok(())
}

/// One field of a node, as the tree's own serialization spells it. An absent
/// field and a `null` one are different states and are printed differently.
fn render_json(value: Option<&serde_json::Value>) -> String {
    value.map_or_else(|| "(absent)".to_string(), serde_json::Value::to_string)
}

/// Everything BackupStore needs to reach a repository.
#[derive(Debug, Clone, Default)]
pub struct StoreConfig {
    /// Repository location. A plain path = local repo (this spike); the same
    /// slot later takes a backend string such as `opendal:sftp`.
    pub repo_root: String,
    /// Path of the persisted masterkey file (written on init, read on open).
    pub key_file: PathBuf,
    /// Concurrency handed to rustic, clamped into `1..=MAX_CONNECTIONS`.
    pub connections: usize,
    /// Extra backend options (e.g. `endpoint`/`user`/`key`/`root` for
    /// `opendal:sftp`). Forwarded verbatim to the backend; only keys the
    /// backend documents are honoured (unknown ones are ignored).
    pub options: BTreeMap<String, String>,
    /// Custom local metadata-cache root, forwarded as rustic's `cache_dir`.
    ///
    /// `None` follows rustic's per-machine default (`dirs::cache_dir()/rustic`,
    /// see [`rustic_cache_roots`]). Ignored when [`no_cache`](Self::no_cache)
    /// is set — rustic declares `cache_dir` and `no_cache` `conflicts_with`
    /// (rustic_core-0.12.0 `src/repository.rs:108`).
    pub cache_dir: Option<PathBuf>,
    /// Disable rustic's local metadata cache entirely (rustic `no_cache`).
    ///
    /// The cache holds snapshot / index / *tree* packs — metadata only, never
    /// the data packs — so disabling it loses no archive content; it only
    /// re-reads that metadata on every open instead of keeping it between runs.
    pub no_cache: bool,
}

impl StoreConfig {
    /// Clamp into the allowed range.
    ///
    /// Unset falls back to `DEFAULT_CONNECTIONS` (a measured trade-off);
    /// values above `MAX_CONNECTIONS` are capped (a hard backend ceiling).
    /// Raising it past the default is allowed but buys no measured speed.
    pub fn with_capped_connections(mut self, user_value: Option<usize>) -> Self {
        let n = user_value.unwrap_or(DEFAULT_CONNECTIONS);
        self.connections = n.clamp(1, MAX_CONNECTIONS);
        self
    }

    /// rustic's repository options for this config, honouring the cache knobs.
    ///
    /// The default is exactly rustic's own [`RepositoryOptions::default()`] —
    /// cache enabled, standard per-machine cache dir — so a config that never
    /// sets the new fields behaves byte-for-byte as it did when every caller
    /// passed the default directly. `no_cache` turns the cache off entirely;
    /// `cache_dir` redirects where it lives. When `no_cache` is set the custom
    /// dir is deliberately not forwarded: rustic declares the two
    /// `conflicts_with` (rustic_core-0.12.0 `src/repository.rs:108`), so under
    /// `no_cache` a `cache_dir` would be silently ignored anyway.
    pub fn repository_options(&self) -> RepositoryOptions {
        if self.no_cache {
            RepositoryOptions::default().no_cache(true)
        } else if let Some(dir) = &self.cache_dir {
            RepositoryOptions::default().cache_dir(dir.clone())
        } else {
            RepositoryOptions::default()
        }
    }
}

/// A sealed shard batch that was handed to (or read back from) the store.
#[derive(Debug, Clone)]
pub struct PushSummary {
    pub stage_shards: usize,
    pub files_new: u64,
    pub files_changed: u64,
    pub files_unmodified: u64,
    pub data_blobs: u64,
    pub data_added: u64,
    pub data_added_packed: u64,
    pub snapshots_in_repo: usize,
    /// Whether this push actually wrote a snapshot.
    ///
    /// `false` only when the caller asked for an unchanged tree not to be
    /// published ([`BackupStore::push_only_if_changed`]) and the repository
    /// already held one equal to what the stage would produce. The rest of the
    /// summary is still filled in: it describes the comparison that was made,
    /// not a snapshot that was written.
    pub snapshot_written: bool,
    pub snapshot_host: String,
    pub repo_was_init: bool,
    /// Packs the backend held that no index file named, and what this push's
    /// open did about them. `None` means the repository was created by this
    /// push (a fresh repository has no packs at all).
    pub orphans: Option<crate::orphans::OrphanOutcome>,
}

/// BackupStore: owns repository open/init, push, and read-back.
///
/// Every operation opens the repository fresh (spike A5: a fresh open re-reads
/// the index, which is what makes the dedup semantics of a later push correct).
pub struct BackupStore {
    pub cfg: StoreConfig,
    /// Normalised machine name — used for both the path partition *and* the
    /// snapshot `host`.
    pub machine: String,
    /// The body cache this store serves conversation-body reads from, when the
    /// operation is allowed to use one (ADR-034). `None` is the default, the
    /// whole-cache-off case, and every bulk operation alike — see
    /// [`BackupStore::with_body_cache`].
    body_cache: Option<std::sync::Arc<crate::body_cache::BodyCache>>,
    /// The per-destination snapshot session cache this store's searches read
    /// from (SRCH-1b). `None` is the default and the whole-cache-off case — see
    /// [`BackupStore::with_snapshot_cache`].
    snapshot_cache: Option<std::sync::Arc<crate::snapshot_cache::SnapshotCache>>,
    /// How this store joins a session's shards: collapse unbound byte-identical
    /// replay shards by default, while retaining exact sequence-bound captured
    /// duplicates; or hand back every shard. See [`DuplicateShardPolicy`].
    shard_policy: DuplicateShardPolicy,
}

impl BackupStore {
    /// Best-effort cap on rustic's parallel fan-out right before any repository
    /// operation: pins the rayon global pool (used by `decrypt.rs` stream_list)
    /// to `connections`. Once another part of the process built a bigger pool
    /// this is a no-op — the hard bound lives at the backend layer.
    fn limit_parallelism(connections: usize) {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "Rayon's global pool is intentionally best-effort because another initializer may already own it."
        )]
        let _ = ThreadPoolBuilder::new()
            .num_threads(connections)
            .build_global();
    }

    pub fn new(cfg: StoreConfig, machine: String) -> Self {
        Self::limit_parallelism(cfg.connections);
        BackupStore {
            cfg,
            machine,
            body_cache: None,
            snapshot_cache: None,
            shard_policy: DuplicateShardPolicy::Collapse,
        }
    }

    /// Construct a store for operations that inspect repository metadata across
    /// every machine and never select a local partition. The empty field is an
    /// internal sentinel, not a machine name; partition-aware methods remain
    /// available only through [`BackupStore::new`].
    pub fn for_metadata_query(cfg: StoreConfig) -> Self {
        Self::limit_parallelism(cfg.connections);
        BackupStore {
            cfg,
            machine: String::new(),
            body_cache: None,
            snapshot_cache: None,
            shard_policy: DuplicateShardPolicy::Collapse,
        }
    }

    /// Set how this store joins a session's shards (default
    /// [`DuplicateShardPolicy::Collapse`]).
    ///
    /// The opt-out exists because unbound collapse is a *reader* decision applied
    /// to bytes that are still, and always will be, on the destination. Selecting
    /// [`DuplicateShardPolicy::KeepAll`] shows every stored shard, so an operator
    /// can see exactly what a collapse dropped. It changes no bytes anywhere.
    pub fn with_shard_policy(mut self, policy: DuplicateShardPolicy) -> Self {
        self.shard_policy = policy;
        self
    }

    /// The shard-joining policy this store reads with.
    pub fn shard_policy(&self) -> DuplicateShardPolicy {
        self.shard_policy
    }

    /// Serve this store's conversation-body reads from `cache` (ADR-034).
    ///
    /// Deliberately a per-store decision rather than a global setting, because
    /// whether a run may use the cache is a property of the *operation*: a
    /// single-session read may fill it, while `export`, `verify`, `dest-init`
    /// and `read --all-machines` are bulk work whose whole point is to be
    /// independent of what happens to be cached. `verify` in particular reads
    /// to prove the **remote** is intact, and a cache answering for the remote
    /// would move the verdict onto the wrong disk.
    ///
    /// Callers pass [`crate::body_cache::for_operation`]; the default (no call)
    /// is `None`, which is also what a store built for bulk work keeps.
    pub fn with_body_cache(
        mut self,
        cache: Option<std::sync::Arc<crate::body_cache::BodyCache>>,
    ) -> Self {
        self.body_cache = cache;
        self
    }

    /// The cache this store would serve body reads from, if any.
    pub fn body_cache(&self) -> Option<&std::sync::Arc<crate::body_cache::BodyCache>> {
        self.body_cache.as_ref()
    }

    /// Serve this store's searches from `cache` (SRCH-1b).
    ///
    /// Like [`BackupStore::with_body_cache`], a per-store decision rather than
    /// a global one: whether a run may use the cache is a property of the
    /// operation. Unlike the body cache it is not a correctness question — a
    /// snapshot cache entry can only ever reproduce the tree walk that wrote it
    /// — but the same discipline is kept, so a caller has to say when it wants
    /// one instead of a test or a bulk job silently inheriting a directory on
    /// the developer's own machine.
    ///
    /// The default (no call) is `None`: an uncached search, which is the run
    /// this tool had before SRCH-1b and the one every existing test still
    /// exercises. Callers pass [`crate::snapshot_cache::SnapshotCache::for_identity`].
    pub fn with_snapshot_cache(
        mut self,
        cache: Option<std::sync::Arc<crate::snapshot_cache::SnapshotCache>>,
    ) -> Self {
        self.snapshot_cache = cache;
        self
    }

    /// The snapshot cache this store's searches would read from, if any.
    pub fn snapshot_cache(&self) -> Option<&std::sync::Arc<crate::snapshot_cache::SnapshotCache>> {
        self.snapshot_cache.as_ref()
    }

    /// Build the backend handles.
    ///
    /// Local paths use rustic_backend's `LocalBackend` directly. When a remote
    /// backend string (`opendal:sftp`) is later configured, the proven A4
    /// wiring applies the connections cap as the backend `connections` option
    /// (`ConcurrentLimitLayer`).
    pub fn backends(&self) -> anyhow::Result<RepositoryBackends> {
        let mut opts = BackendOptions::default().repository(self.cfg.repo_root.as_str());
        if self.cfg.repo_root.starts_with("opendal:") || self.cfg.repo_root.starts_with("rest:") {
            // Built by `StoreConfig::backend_options`, which is defined at the
            // end of this file so that introducing it moved no cited line; the
            // values in it are the destination's, passed through untouched.
            opts = opts.options(self.cfg.backend_options());
        }
        let backends = opts.to_backends().context("build backend options")?;
        // ADR-034: the body cache sits *below* rustic, as a wrapper over the
        // backend handles — the only seam where a conversation body can be
        // observed as the remote's own ciphertext. `rustic_backend` hands back
        // `Arc<dyn WriteBackend>`, so this needs no fork: the wrapper delegates
        // every non-body operation unchanged.
        let Some(cache) = &self.body_cache else {
            return Ok(backends);
        };
        Ok(RepositoryBackends::new(
            crate::body_cache::BodyCacheBackend::wrap(backends.repository(), Some(cache.clone())),
            backends
                .repo_hot()
                .map(|hot| crate::body_cache::BodyCacheBackend::wrap(hot, Some(cache.clone()))),
        ))
    }

    fn repo_exists(&self, backends: &RepositoryBackends) -> anyhow::Result<bool> {
        Ok(!backends.repository().list(FileType::Config)?.is_empty())
    }

    /// Whether the configured repository has been initialised.
    pub fn repository_exists(&self) -> anyhow::Result<bool> {
        let backends = self.backends()?;
        self.repo_exists(&backends)
    }

    /// Open the repo if present, otherwise init it fresh.
    ///
    /// `append_only` is applied at init only — after the chunker (here the
    /// default) is fixed, so the config is sealed from day one.
    pub fn open_or_init(
        &self,
        mk: &MasterKey,
    ) -> anyhow::Result<(Repository<IndexedFullStatus>, bool)> {
        self.open_or_init_with_progress(mk, NoProgressBars {})
            .map(|(r, init, _)| (r, init))
    }

    /// Open an existing repository and build its index, reaching the packs no
    /// index file names (see [`crate::orphans`]).
    ///
    /// Every read path goes through here. For a repository whose index and
    /// backend agree — every healthy one — this is the plain `to_indexed()` open
    /// and nothing else. It differs only when the backend holds a pack no index
    /// file names, which is the state a killed push leaves behind: then the
    /// index is rebuilt with those packs reachable, which is what lets a read of
    /// a snapshot written after such a push resolve its content at all. The
    /// second element is what that open did, for callers that report it — a
    /// refusal is a real state, not a detail.
    pub fn open_indexed(
        &self,
        mk: &MasterKey,
    ) -> anyhow::Result<(Repository<IndexedFullStatus>, crate::orphans::OrphanOutcome)> {
        let backends = self.backends()?;
        crate::orphans::open_adopting(&self.cfg, &backends, mk)
    }

    /// Like [`BackupStore::open_or_init`], but the freshly created repository
    /// carries a caller-supplied [`ProgressBars`] so a `push` can observe the
    /// backup as it runs. All other call sites keep the silent
    /// `NoProgressBars` default.
    ///
    /// The third element is what the open found out about packs the index files
    /// do not name, and what it did about them; a repository this call creates
    /// has none, and reports `None`.
    fn open_or_init_with_progress<P: ProgressBars>(
        &self,
        mk: &MasterKey,
        pb: P,
    ) -> anyhow::Result<(
        Repository<IndexedFullStatus>,
        bool,
        Option<crate::orphans::OrphanOutcome>,
    )> {
        let backends = self.backends()?;
        let repo = Repository::new_with_progress(&self.cfg.repository_options(), &backends, pb)?;
        let creds = Credentials::Masterkey(mk.clone());
        if self.repo_exists(&backends)? {
            let opened = repo.open(&creds).context("open existing repository")?;
            let (r, outcome) = crate::orphans::index_adopting(opened, &backends, mk)?;
            Ok((r, false, Some(outcome)))
        } else {
            let config_opts = ConfigOptions::default().set_append_only(true);
            let r = repo
                .init(&creds, &KeyOptions::default(), &config_opts)
                .context("init new repository")?
                .to_indexed()
                .context("index new repository")?;
            Ok((r, true, None))
        }
    }

    /// Push the whole stage tree (`sessions/<machine>/<id>/NNNNNN.jsonl`) into
    /// a fresh snapshot. The stage root must already hold only sealed shards.
    pub fn push(&self, stage_root: &Path, mk: &MasterKey) -> anyhow::Result<PushSummary> {
        self.push_with(stage_root, mk, false)
    }

    /// Push, but let the repository decline to write the snapshot when its tree
    /// is identical to the one this machine's newest snapshot there already
    /// holds — "the destination already holds what would be published".
    ///
    /// This is the destination-side counterpart of the check `run-once` makes
    /// before it calls [`BackupStore::push`]: `push_only_if_changed` decides
    /// *whether a push is attempted*, and this decides whether one that was
    /// attempted has anything to record. Only `dest-init` uses it, because only
    /// `dest-init` can be re-run over a stage it already published: an explicit
    /// `push` is a request to record the stage as it stands, and `run-once`'s
    /// stage-side check cannot see that a *new* destination already holds the
    /// content (a fresh destination is also one where the pass wrote nothing).
    ///
    /// The comparison is rustic's own — the same content-addressing that
    /// decides "modified" everywhere else — so it covers every byte of the
    /// stage, session shards and machine metadata alike, and it is made against
    /// the snapshot group the parent search uses (this machine, the stage path).
    pub fn push_only_if_changed(
        &self,
        stage_root: &Path,
        mk: &MasterKey,
    ) -> anyhow::Result<PushSummary> {
        self.push_with(stage_root, mk, true)
    }

    fn push_with(
        &self,
        stage_root: &Path,
        mk: &MasterKey,
        skip_if_unchanged: bool,
    ) -> anyhow::Result<PushSummary> {
        // This must run before opening or backing up the repository. A stage
        // assembled for machine A must never be snapshotted as machine B.
        validate_stage_machines(stage_root, &self.machine)?;
        let stage_shards = sealed_shard_count(stage_root)?;
        // This guard used to refuse any shard-less stage: while readers looked only at the
        // newest snapshot per machine, an empty snapshot made the machine look as if it
        // held nothing. ADR-021 made those readers cumulative, so the guard now refuses
        // only when the stage holds neither sealed shards nor machine metadata (ADR-022).
        if stage_shards == 0 && !crate::metahash::has_meta_files(stage_root, &self.machine)? {
            anyhow::bail!(
                "refusing empty snapshot: stage contains no sealed shards; collect or restore the stage first"
            );
        }
        // Live progress for what is usually the longest phase of a push. The
        // reporter is driven by rustic's byte-level callbacks; `stage_shards`
        // is already counted above and gives the `shards=N/total` scale.
        let push_progress =
            std::sync::Arc::new(crate::push_progress::PushProgress::new(stage_shards as u64));
        let (r, init, orphans) = self.open_or_init_with_progress(
            mk,
            crate::push_progress::PushProgressBars::new(push_progress),
        )?;
        let snap_opt = SnapshotOptions::default().host(self.machine.clone());
        let snap = snap_opt.to_snapshot().context("build snapshot opts")?;
        let source = PathList::from_string(
            stage_root
                .canonicalize()
                .context("canonicalize stage root")?
                .to_str()
                .ok_or_else(|| anyhow!("stage root is not valid utf-8"))?,
        )
        .context("parse stage root")?
        .sanitize()
        .context("sanitize stage root")?;
        // The node metadata policy, in both halves. The parent options say which
        // fields may differ without a file counting as changed; the save options
        // say which fields the node stores at all. Both are needed, because a
        // field the comparison is told to ignore while the node still carries it
        // leaves the tree's bytes resting on a value this project has declared
        // irrelevant: Windows reports a file's creation time as `ctime`, and a
        // directory's times are the times of the writes into it, so neither of
        // them is content — and a stored value that moves on its own re-serializes
        // the tree carrying it, and every tree above it, on every push, with
        // nothing else changed. What makes a directory part of the archive is the
        // tree it holds, so dropping its times drops nothing the comparison uses,
        // while a file's mtime — what change detection reads — is untouched.
        // `docs-dev/node-metadata.md` records both measured cases and why each
        // field is dropped rather than pinned to the mtime the comparison reads.
        let build_opts = BackupOptions::default()
            .ignore_save_opts(
                LocalSourceSaveOptions::default()
                    .set_atime(TimeOption::Mtime)
                    .set_ctime(TimeOption::No)
                    .set_dir_times(TimeOption::No),
            )
            .parent_opts(
                ParentOptions::default()
                    .ignore_ctime(true)
                    .ignore_inode(true)
                    .skip_if_unchanged(skip_if_unchanged),
            );
        let snap = r
            .backup(&build_opts, &source, snap)
            .context("run rustic backup")?;

        let snaps = r.get_all_snapshots().context("list snapshots")?;
        // A skipped snapshot is never written, so it keeps the id `to_snapshot`
        // gave it and appears in no listing. Asking the listing whether it
        // holds the snapshot we were handed is a read of the repository's own
        // answer, not a flag this function sets from its own intent.
        let snapshot_written = snaps.iter().any(|stored| stored.id == snap.id);
        let summary = snap
            .summary
            .as_ref()
            .ok_or_else(|| anyhow!("backup returned no summary"))?;
        Ok(PushSummary {
            stage_shards,
            files_new: summary.files_new,
            files_changed: summary.files_changed,
            files_unmodified: summary.files_unmodified,
            data_blobs: summary.data_blobs,
            data_added: summary.data_added,
            data_added_packed: summary.data_added_packed,
            snapshots_in_repo: snaps.len(),
            snapshot_written,
            snapshot_host: snap.hostname.clone(),
            repo_was_init: init,
            orphans,
        })
    }

    /// What the newest snapshot for this store's machine stored differently
    /// from the one before it, node by node and field by field.
    ///
    /// A tree is rewritten when the *bytes* of one of its nodes move, and the
    /// only thing that re-serializes a tree on an untouched stage is a stored
    /// field that moved on its own. The push summary counts files, so it
    /// cannot name that field, and a node's bytes come from the serialization
    /// of every field it has — including ones no counter separates. This
    /// walks both snapshots through `rustic_core`, and for every node present
    /// in one and not the other, or whose serialization differs, prints each
    /// field that changed. Comparing the *serialized* form is deliberate: a
    /// field this report does not know to look for cannot hide, and the
    /// `subtree` entry it prints is the child tree id whose change is what
    /// carried the churn up through the trees above it.
    ///
    /// Diagnostics, not a hot path: it decodes every tree of both snapshots.
    /// `crate::store`'s no-op-push tests call it when a push that uploaded no
    /// content still added tree bytes, which is the state a platform's
    /// metadata quirks produce and no other output distinguishes.
    pub fn node_metadata_diff(&self, mk: &MasterKey) -> anyhow::Result<String> {
        let (repo, _adoption) = self
            .open_indexed(mk)
            .context("open repository for the node metadata diff")?;
        let mut snaps: Vec<SnapshotFile> = repo
            .get_all_snapshots()
            .context("list snapshots for the node metadata diff")?
            .into_iter()
            .filter(|snap| snap.hostname == self.machine)
            .collect();
        snaps.sort();
        let (old, new) = match snaps.as_slice() {
            [.., old, new] => (old.clone(), new.clone()),
            other => {
                return Ok(format!(
                    "node metadata diff: {} snapshot(s) for {} - nothing to compare\n",
                    other.len(),
                    self.machine
                ));
            }
        };
        let old_nodes = self.snapshot_nodes(&repo, &old)?;
        let new_nodes = self.snapshot_nodes(&repo, &new)?;

        let mut report = format!(
            "node metadata diff: {} snapshot {} (tree {}) -> snapshot {} (tree {})\n  \
             nodes: old={} new={}\n",
            self.machine,
            old.id,
            old.tree,
            new.id,
            new.tree,
            old_nodes.len(),
            new_nodes.len()
        );
        let mut changed = 0usize;
        let mut paths: BTreeSet<&String> = old_nodes.keys().collect();
        paths.extend(new_nodes.keys());
        for path in paths {
            match (old_nodes.get(path), new_nodes.get(path)) {
                (Some(before), Some(after)) if before == after => {}
                (Some(before), Some(after)) => {
                    changed += 1;
                    report.push_str(&format!("  changed: {path}\n"));
                    let mut fields: BTreeSet<&String> = before
                        .as_object()
                        .map_or_else(BTreeSet::new, |object| object.keys().collect());
                    if let Some(object) = after.as_object() {
                        fields.extend(object.keys());
                    }
                    for field in fields {
                        let was = before.get(field);
                        let now = after.get(field);
                        if was != now {
                            report.push_str(&format!(
                                "    {field}: {} -> {}\n",
                                render_json(was),
                                render_json(now)
                            ));
                        }
                    }
                }
                (Some(before), None) => {
                    changed += 1;
                    report.push_str(&format!(
                        "  removed: {path} (was {})\n",
                        render_json(Some(before))
                    ));
                }
                (None, Some(after)) => {
                    changed += 1;
                    report.push_str(&format!(
                        "  added:   {path} (now {})\n",
                        render_json(Some(after))
                    ));
                }
                (None, None) => {}
            }
        }
        if changed == 0 {
            report.push_str("  no node differs; the two snapshots store identical node metadata\n");
        }
        Ok(report)
    }

    /// Every node of one snapshot, keyed by its path inside the snapshot, in
    /// the serialized form the tree's bytes are built from.
    fn snapshot_nodes(
        &self,
        repo: &Repository<IndexedFullStatus>,
        snap: &SnapshotFile,
    ) -> anyhow::Result<BTreeMap<String, serde_json::Value>> {
        let root = repo
            .node_from_snapshot_and_path(snap, "")
            .context("read snapshot root for the node metadata diff")?;
        let mut nodes = BTreeMap::new();
        for entry in repo
            .ls(&root, &LsOptions::default())
            .context("list snapshot for the node metadata diff")?
        {
            let (path, node) = entry.context("read snapshot entry for the node metadata diff")?;
            nodes.insert(
                path.to_string_lossy().into_owned(),
                serde_json::to_value(&node).context("serialize node for the node metadata diff")?,
            );
        }
        Ok(nodes)
    }

    /// Find archived inbox `file_sha256` values in repository snapshots.
    ///
    /// This is intentionally a targeted fallback for the empty-stage guard:
    /// callers pass only hashes not found in the current stage, and the walk
    /// stops as soon as all requested hashes are found. The repository has no
    /// index for the JSON field, so the worst case still reads archived shard
    /// files; it is never used on a non-empty push.
    pub fn archived_file_sha256s(
        &self,
        mk: &MasterKey,
        wanted: &BTreeSet<String>,
    ) -> anyhow::Result<BTreeSet<String>> {
        if wanted.is_empty() {
            return Ok(BTreeSet::new());
        }
        let (repo, _adoption) = self
            .open_indexed(mk)
            .context("open repository for consumed audit")?;
        self.require_sound_packs(&repo)?;
        let snapshots = repo
            .get_all_snapshots()
            .context("list snapshots for consumed audit")?;
        let mut found = BTreeSet::new();

        for snapshot in snapshots {
            if found.len() == wanted.len() {
                break;
            }
            let root = repo
                .node_from_snapshot_and_path(&snapshot, "")
                .context("read snapshot root for consumed audit")?;
            let entries = repo
                .ls(&root, &LsOptions::default())
                .context("list snapshot for consumed audit")?
                .collect::<rustic_core::RusticResult<Vec<_>>>()
                .context("collect snapshot entries for consumed audit")?;
            for (path, node) in entries {
                if node.node_type != NodeType::File
                    || crate::readback::bucket_shard_path(&path).is_none()
                {
                    continue;
                }
                let mut bytes = Vec::new();
                repo.dump(&node, &mut bytes)
                    .context("read archived shard for consumed audit")?;
                for line in bytes.split(|byte| *byte == b'\n') {
                    let Ok(record) = serde_json::from_slice::<AuditRecord>(line) else {
                        continue;
                    };
                    if let Some(sha) = record.file_sha256 {
                        if wanted.contains(&sha) {
                            found.insert(sha);
                        }
                    }
                }
                if found.len() == wanted.len() {
                    break;
                }
            }
        }
        Ok(found)
    }

    /// Re-open the repository (fresh, so the index reflects everything stored)
    /// and return it plus the newest snapshot for `self.machine`.
    ///
    /// Only [`Self::read_session_readback`] still uses this. It is the
    /// stage-addressed reader — the one that takes the *archiving* machine's
    /// absolute stage root — and it is not on the CLI's path any more: `read
    /// --session` resolves the copy from the archive by `(machine, session id)`
    /// through [`Self::read_session_concat`], which needs no local prefix and
    /// reads cumulatively. See that function for why.
    fn open_with_newest_snapshot(
        &self,
        mk: &MasterKey,
    ) -> anyhow::Result<(Repository<IndexedFullStatus>, SnapshotFile)> {
        let (repo, _adoption) = self.open_indexed(mk)?;
        self.require_sound_packs(&repo)?;
        let snaps = repo.get_all_snapshots()?;
        let newest_for_host = snaps
            .iter()
            .filter(|s| s.hostname == self.machine)
            .max()
            .cloned()
            .ok_or_else(|| anyhow!("no snapshot for host {}", self.machine))?;
        Ok((repo, newest_for_host))
    }

    /// Relative (inside the backup root) path of one shard.
    #[allow(dead_code)]
    fn shard_in_snapshot(&self, stage_canon: &Path, session_id: &str, file_name: &str) -> PathBuf {
        stage_canon
            .strip_prefix("/")
            .unwrap_or(stage_canon)
            .join(SESSIONS_DIR)
            .join(&self.machine)
            .join(session_id)
            .join(file_name)
    }

    /// Read every sealed shard of one session (from `self.machine`'s newest
    /// snapshot) in sequence order, concatenated. Returns the bytes plus one
    /// `(name, sha256)` per shard for verification.
    pub fn read_session_readback(
        &self,
        stage_root: &Path,
        session_id: &str,
        mk: &MasterKey,
    ) -> anyhow::Result<(Vec<u8>, Vec<(String, String)>)> {
        let (repo, snap) = self.open_with_newest_snapshot(mk)?;
        let canon = stage_root
            .canonicalize()
            .context("canonicalize stage root")?;
        let dir_rel = canon
            .strip_prefix("/")
            .unwrap_or(&canon)
            .join(SESSIONS_DIR)
            .join(&self.machine)
            .join(session_id);
        let dir_node = match repo.node_from_snapshot_and_path(&snap, &dir_rel.to_string_lossy()) {
            Ok(n) => n,
            Err(e) => {
                return Err(anyhow!(
                    "session dir `{}` not in newest snapshot for host {}: {e}",
                    dir_rel.display(),
                    self.machine
                ))
            }
        };
        let entries: Vec<_> = repo
            .ls(&dir_node, &LsOptions::default())?
            .collect::<rustic_core::RusticResult<Vec<_>>>()?;
        let mut entries: Vec<_> = entries
            .into_iter()
            .filter_map(|(path, node)| {
                if node.node_type != NodeType::File {
                    return None;
                }
                let name = path.file_name()?.to_str()?.to_string();
                let seq = parse_shard_seq(&name)?;
                Some((seq, path, node))
            })
            .collect();
        entries.sort_by(|(seq_a, path_a, _), (seq_b, path_b, _)| {
            seq_a.cmp(seq_b).then_with(|| path_a.cmp(path_b))
        });
        if entries.is_empty() {
            return Err(anyhow!("session dir is empty in snapshot"));
        }
        let _scope = declare_session_scope(
            self.body_cache.as_ref(),
            entries.iter().map(|(_, _, node)| node.meta.size).sum(),
        );

        let mut concat = Vec::new();
        let mut hashes = Vec::new();
        let mut bodies = Vec::with_capacity(entries.len());
        for (_seq, _path, node) in &entries {
            let mut buf = Vec::new();
            repo.dump(&node, &mut buf).context("dump shard")?;
            bodies.push(buf);
        }
        let duplicates: BTreeSet<_> = duplicate_shard_indices(&bodies).into_iter().collect();
        for ((seq, _path, _node), (index, buf)) in entries.iter().zip(bodies.iter().enumerate()) {
            if duplicates.contains(&index) {
                continue;
            }
            let digest: [u8; 32] = Sha256::digest(&buf).into();
            concat.extend_from_slice(buf);
            hashes.push((shard_filename(*seq), hex_digest(&digest)));
        }
        Ok((concat, hashes))
    }

    /// Read one session's sealed shards cumulatively across snapshots, in global
    /// sequence order, as `(concat, [(name, sha256)])`.
    ///
    /// Unlike [`Self::read_session_readback`] this needs no local stage root:
    /// the dashboard (and `read --session`) addresses a session by
    /// `(machine, session id)`, which is exactly the partition the archive path
    /// already carries ([`crate::readback::bucket_shard_path`]). Reconstructing
    /// an absolute stage prefix just to strip it again would add a way to be
    /// wrong — the archived prefix is the source machine's, not this one's —
    /// and would put the *archiving* machine's filesystem layout in the way of
    /// reading a lost machine's conversation back.
    ///
    /// The search is cumulative over `machine`'s snapshots, newest first
    /// (ADR-021), keeping newest copies per archive-relative path and retaining
    /// older sequences absent from later snapshots. `reclaim-stage` deletes a session's bodies from
    /// the stage once every destination has proved it holds them, and every
    /// snapshot taken afterwards therefore holds the session's directory but no
    /// shards. On m3 that is 3162 of 3178 sessions in the newest snapshot, all
    /// of them present in older ones.
    ///
    /// A snapshot that cannot be walked **stops the walk** — it is not skipped.
    /// The rule the walk has to keep is not "walk until something holds the
    /// session" but "a success proves no newer snapshot holds it": the newest
    /// appearance wins (ADR-021), so the first unreadable snapshot is exactly
    /// the point past which no older copy can be shown as the current one. An
    /// older copy returned anyway would be a silent stale read — the caller
    /// asked for the session's bytes and would get a version a later snapshot
    /// may have superseded, with nothing said about it. Every such case is
    /// therefore an `Err` naming the snapshot, which the CLI reports as exit 3
    /// ("did not finish"), never as a result. The same holds when the walk ends
    /// without a holder: "not in any snapshot I could read" and "not in any
    /// snapshot" are different answers, and only the second is a negative.
    ///
    /// Payload tier: this is the one call that fetches and decrypts conversation
    /// bytes. A session no snapshot holds is an `Err`, never an empty result.
    pub fn read_session_concat(
        &self,
        machine: &str,
        session_id: &str,
        mk: &MasterKey,
    ) -> anyhow::Result<(Vec<u8>, Vec<(String, String)>)> {
        let (repo, _adoption) = self
            .open_indexed(mk)
            .context("open repository for session content")?;
        self.require_sound_packs(&repo)?;
        let snaps = repo.get_all_snapshots().context("list snapshots")?;
        let Some(host_snaps) = crate::readback::snapshots_by_host_newest_first(snaps)
            .into_iter()
            .find(|(host, _)| host == machine)
            .map(|(_, snaps)| snaps)
        else {
            return Err(anyhow!(
                "no snapshot for machine `{machine}` in this repository"
            ));
        };

        let snapshots_held = host_snaps.len();
        if let Some(result) = dump_cumulative_session(
            &repo,
            &host_snaps,
            machine,
            session_id,
            self.body_cache.as_ref(),
            self.shard_policy,
        )? {
            return Ok(result);
        }
        // Reached only when every snapshot of the machine was walked and none
        // held the session — a proof of absence rather than a failure to look.
        Err(anyhow!(
            "session `{}` holds no shards in any of the {snapshots_held} snapshots of machine \
             `{machine}` (all of them were read)",
            crate::id::short_session_id(session_id),
        ))
    }

    /// Dump the sealed shards of **several** selected sessions in one repository
    /// open, keyed by `(machine, session id)`.
    ///
    /// [`Self::read_session_concat`] answers the same question for one session
    /// and pays one repository open for it. `export` asks about a whole selected
    /// set, and on a remote backend that difference is the whole command: N
    /// opens is N handshakes, so the batch is not just an optimisation. The two
    /// paths share the cumulative per-sequence reader, which keeps their bytes
    /// identical — the property `export`'s tests pin.
    ///
    /// A session in `wanted` that no snapshot of its machine holds is
    /// **absent** from the result, never present with zero bytes; the caller
    /// must treat "asked for, not returned" as a failure, exactly like
    /// [`Self::dump_machine_sessions`].
    ///
    /// Cumulative for the same reason, and by the same rule, as
    /// [`Self::read_session_concat`]: `export` writes what `search` found, and
    /// a search that can find a reclaimed session while the export silently
    /// drops it would be a worse inconsistency than the one this fixes. The
    /// Each requested session is joined across every snapshot for its machine.
    pub fn read_selected_sessions(
        &self,
        mk: &MasterKey,
        wanted: &BTreeSet<(String, String)>,
    ) -> anyhow::Result<BTreeMap<(String, String), (Vec<u8>, Vec<(String, String)>)>> {
        let mut out = BTreeMap::new();
        if wanted.is_empty() {
            return Ok(out);
        }
        let (repo, _adoption) = self
            .open_indexed(mk)
            .context("open repository for export")?;
        self.require_sound_packs(&repo)?;
        let snaps = repo.get_all_snapshots().context("list snapshots")?;

        for (machine, host_snaps) in crate::readback::snapshots_by_host_newest_first(snaps) {
            let wanted_here: BTreeSet<&str> = wanted
                .iter()
                .filter(|(m, _)| m == &machine)
                .map(|(_, s)| s.as_str())
                .collect();
            if wanted_here.is_empty() {
                continue;
            }
            for session_id in wanted_here {
                if let Some(result) = dump_cumulative_session(
                    &repo,
                    &host_snaps,
                    &machine,
                    session_id,
                    self.body_cache.as_ref(),
                    self.shard_policy,
                )? {
                    out.insert((machine.clone(), session_id.to_string()), result);
                }
            }
        }
        Ok(out)
    }
}

/// Read one session cumulatively across a machine's snapshots. Reclaimed
/// snapshots may contain only the newest sealed sequence, so retain the newest
/// copy of each archive-relative shard path while walking every snapshot.
fn dump_cumulative_session<S: rustic_core::IndexedFull>(
    repo: &Repository<S>,
    snapshots: &[SnapshotFile],
    machine: &str,
    session_id: &str,
    body_cache: Option<&std::sync::Arc<crate::body_cache::BodyCache>>,
    policy: DuplicateShardPolicy,
) -> anyhow::Result<Option<(Vec<u8>, Vec<(String, String)>)>> {
    let mut held: BTreeMap<(u64, String), (Option<u64>, String, rustic_core::repofile::Node)> =
        BTreeMap::new();
    let mut sequence_paths = BTreeMap::new();
    let mut ambiguous = BTreeSet::new();
    let mut observations = Vec::new();
    let mut metadata_found = false;
    for snap in snapshots {
        let snap_id = snap.id.to_hex().as_str().to_string();
        let short = &snap_id[..8.min(snap_id.len())];
        let root = repo
            .node_from_snapshot_and_path(snap, "")
            .with_context(|| {
                format!(
                    "UNKNOWN: session `{}` cannot be resolved for machine `{machine}`: snapshot {short} tree root",
                    crate::id::short_session_id(session_id)
                )
            })?;
        let entries = repo
            .ls(&root, &LsOptions::default())
            .and_then(|iter| iter.collect::<rustic_core::RusticResult<Vec<_>>>())
            .with_context(|| {
                format!(
                    "UNKNOWN: session `{}` cannot be resolved for machine `{machine}`: snapshot {short} tree walk",
                    crate::id::short_session_id(session_id)
                )
            })?;
        for (path, node) in entries {
            if node.node_type != NodeType::File {
                continue;
            }
            if policy.collapses() && crate::readback::is_provenance_path(&path, machine) {
                if !metadata_found {
                    let mut bytes = Vec::new();
                    repo.dump(&node, &mut bytes)
                        .with_context(|| format!("snapshot {short} provenance metadata"))?;
                    observations = crate::provenance::parse_observations(&bytes)
                        .with_context(|| format!("parse snapshot {short} provenance metadata"))?
                        .into_iter()
                        .filter(|row| row.dimensions.is_valid() && row.session_id == session_id)
                        .collect();
                    metadata_found = true;
                }
                continue;
            }
            let Some((found_machine, found_session, shard)) =
                crate::readback::bucket_shard_path(&path)
            else {
                continue;
            };
            if found_machine != machine || found_session != session_id {
                continue;
            }
            let sequence = parse_shard_seq(&shard);
            let sort_sequence = sequence.unwrap_or(u64::MAX);
            let path_id = path.to_string_lossy().into_owned();
            if let Some(sequence) = sequence {
                if sequence_paths
                    .insert(sequence, path_id.clone())
                    .is_some_and(|previous| previous != path_id)
                {
                    ambiguous.insert(sequence);
                }
            }
            held.entry((sort_sequence, path_id))
                .or_insert((sequence, shard, node));
        }
    }
    if held.is_empty() {
        return Ok(None);
    }
    let _scope = declare_session_scope(
        body_cache,
        held.values().map(|(_, _, node)| node.meta.size).sum(),
    );
    let mut shards = Vec::with_capacity(held.len());
    for (_, (sequence, name, node)) in held {
        let mut body = Vec::new();
        repo.dump(&node, &mut body)
            .with_context(|| format!("dump shard {name}"))?;
        let sequence = sequence.filter(|sequence| !ambiguous.contains(sequence));
        shards.push((sequence, name, body));
    }
    let shard_bodies: Vec<_> = shards
        .iter()
        .map(|(seq, _, body)| (*seq, body.clone()))
        .collect();
    let observed_sequences =
        crate::provenance::verified_observation_sequences(session_id, &observations, &shard_bodies);
    let body_only: Vec<_> = shards.iter().map(|(_, _, body)| body.clone()).collect();
    let duplicates: BTreeSet<_> = if policy.collapses() {
        duplicate_shard_indices(&body_only).into_iter().collect()
    } else {
        BTreeSet::new()
    };
    let mut concat = Vec::new();
    let mut hashes = Vec::new();
    for (index, (sequence, name, body)) in shards.into_iter().enumerate() {
        if duplicates.contains(&index)
            && !sequence.is_some_and(|sequence| observed_sequences.contains(&sequence))
        {
            continue;
        }
        let digest: [u8; 32] = Sha256::digest(&body).into();
        concat.extend_from_slice(&body);
        hashes.push((name, hex_digest(&digest)));
    }
    Ok(Some((concat, hashes)))
}

/// Declare the plaintext size of the session about to be dumped, so ADR-034's
/// "a session larger than a tenth of the quota is never cached" rule can be
/// applied by the cache to its *writes* while leaving reads alone.
///
/// The size is summed from the snapshot's own node metadata before a byte is
/// fetched, so the rule is decided by the archive, not by what arrived. A
/// missing or short size can only cause a store that would have been allowed —
/// never a wrong read — and a shard larger than the whole quota is refused by
/// the cache's own per-entry ceiling.
///
/// The returned guard restores the previous declaration when the dump ends, so
/// a batch that dumps several sessions in one repository open stays correct.
fn declare_session_scope<'a>(
    body_cache: Option<&'a std::sync::Arc<crate::body_cache::BodyCache>>,
    plaintext_bytes: u64,
) -> Option<crate::body_cache::SessionScope<'a>> {
    body_cache.map(|cache| cache.declare_session(plaintext_bytes))
}

/// sha256 of concatenated source shards on disk (the expected value).
pub fn expected_concat_sha(
    stage_root: &Path,
    machine: &str,
    session_id: &str,
) -> anyhow::Result<String> {
    concat_sha_of(stage_root, machine, session_id)
}

/// Concatenated bytes of all sealed shards of a session (seq order).
pub fn concat_shards(
    stage_root: &Path,
    machine: &str,
    session_id: &str,
) -> anyhow::Result<Vec<u8>> {
    let dir = session_shard_dir(stage_root, machine, session_id);
    let mut entries = sealed_shard_entries(&dir)?;
    entries.sort_by_key(|(seq, _)| *seq);
    let mut all = Vec::new();
    for (_, path) in entries {
        let bytes = fs::read(path)?;
        all.extend_from_slice(&bytes);
    }
    Ok(all)
}

fn concat_sha_of(stage_root: &Path, machine: &str, session_id: &str) -> anyhow::Result<String> {
    let all = concat_shards(stage_root, machine, session_id)?;
    Ok(hex_digest(&Sha256::digest(&all)))
}

// -------------------------------------------------------------------- shards

/// `<stage>/sessions/<machine>/<session_id>` — the partitioned directory.
pub fn session_shard_dir(stage_root: &Path, machine: &str, session_id: &str) -> PathBuf {
    stage_root.join(SESSIONS_DIR).join(machine).join(session_id)
}

#[derive(Debug, Deserialize)]
struct AuditRecord {
    file_sha256: Option<String>,
}

/// Collect the `file_sha256` values embedded by the ingest writer in sealed
/// stage shards. Other stage writers legitimately have no such field and are
/// ignored by this metadata-only scan.
pub fn stage_file_sha256s(stage_root: &Path) -> anyhow::Result<BTreeSet<String>> {
    let sessions_root = stage_root.join(SESSIONS_DIR);
    let machines = match fs::read_dir(&sessions_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", sessions_root.display())),
    };
    let mut out = BTreeSet::new();
    for machine in machines {
        let machine = machine?;
        if !machine.file_type()?.is_dir() {
            continue;
        }
        for session in fs::read_dir(machine.path())? {
            let session = session?;
            if !session.file_type()?.is_dir() {
                continue;
            }
            for (_, shard) in sealed_shard_entries(&session.path())? {
                let raw = fs::read(&shard)
                    .with_context(|| format!("read stage shard for audit ({})", shard.display()))?;
                for line in raw.split(|byte| *byte == b'\n') {
                    let Ok(record) = serde_json::from_slice::<AuditRecord>(line) else {
                        continue;
                    };
                    if let Some(sha) = record.file_sha256 {
                        out.insert(sha);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Reject a stage containing any machine partition other than `expected`.
/// Machine values are represented by full SHA-256 digests in the diagnostic,
/// never by hostnames.
pub fn validate_stage_machines(stage_root: &Path, expected: &str) -> anyhow::Result<()> {
    let sessions_root = stage_root.join(SESSIONS_DIR);
    let machines = match fs::read_dir(&sessions_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", sessions_root.display())),
    };
    let mut unexpected = Vec::new();
    for entry in machines {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != expected {
            unexpected.push(name);
        }
    }
    if unexpected.is_empty() {
        return Ok(());
    }
    let fingerprints: Vec<String> = unexpected
        .iter()
        .map(|name| machine_fingerprint(name))
        .collect();
    anyhow::bail!(
        "stage machine mismatch: expected_machine_sha256={} unexpected_machine_dirs={} unexpected_machine_sha256={}",
        machine_fingerprint(expected),
        unexpected.len(),
        fingerprints.join(",")
    )
}

/// Privacy-safe machine identity used in diagnostics.
pub fn machine_fingerprint(machine: &str) -> String {
    hex_digest(&Sha256::digest(machine.as_bytes()))
}

/// Count sealed shards across every machine/session in a stage. Directories
/// without a shard do not make an archive look non-empty, and unrelated files
/// are ignored. This is the push guard that distinguishes a retained stage
/// with no new content from an empty stage that must not create a snapshot.
pub fn sealed_shard_count(stage_root: &Path) -> anyhow::Result<usize> {
    let sessions_root = stage_root.join(SESSIONS_DIR);
    let machines = match fs::read_dir(&sessions_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).with_context(|| format!("read {}", sessions_root.display())),
    };
    let mut count = 0;
    for machine in machines {
        let machine = machine?;
        if !machine.file_type()?.is_dir() {
            continue;
        }
        for session in fs::read_dir(machine.path())? {
            let session = session?;
            if session.file_type()?.is_dir() {
                count += sealed_shard_entries(&session.path())?.len();
            }
        }
    }
    Ok(count)
}

/// Absolute path of shard `seq` using the default bucket cap.
pub fn shard_path(stage_root: &Path, machine: &str, session_id: &str, seq: u64) -> PathBuf {
    shard_path_with_cap(
        stage_root,
        machine,
        session_id,
        seq,
        DEFAULT_SHARD_BUCKET_CAP,
    )
}

/// Absolute path of shard `seq` with an explicit bucket cap.
pub fn shard_path_with_cap(
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    seq: u64,
    bucket_cap: usize,
) -> PathBuf {
    session_shard_dir(stage_root, machine, session_id)
        .join(shard_bucket_name(seq, bucket_cap))
        .join(shard_filename(seq))
}

/// Bucket name for a one-based shard sequence. Sequence 1..=CAP is `000`.
pub fn shard_bucket_name(seq: u64, bucket_cap: usize) -> String {
    let cap = bucket_cap.max(1) as u64;
    format!("{:03}", seq.saturating_sub(1) / cap)
}

/// `000001.jsonl` style file name for a sequence number.
pub fn shard_filename(seq: u64) -> String {
    format!("{seq:06}{SHARD_SUFFIX}")
}

/// Parse a sealed shard file name back into its sequence number.
pub fn parse_shard_seq(name: &str) -> Option<u64> {
    let base = name.strip_suffix(SHARD_SUFFIX)?;
    if base.len() != 6 || !base.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    base.parse().ok()
}

/// Name of the persisted per-(machine, session) shard sequence counter file,
/// stored next to the sealed shards it governs. The name can never collide
/// with a shard: `parse_shard_seq` requires exactly six digits plus `.jsonl`.
pub const SHARD_SEQ_FILE: &str = "shard-seq";

/// The three semantic outcomes of reading the shard sequence counter.
///
/// Mirrors [`KeyFileState`]: `Missing` is the only outcome that permits
/// seeding the counter from the existing shard set; `Unusable` hard-fails so
/// an unreadable or corrupt counter is never silently treated as 0 — that is
/// exactly how a reclaim would otherwise reset the sequence and collide with
/// archived shards.
#[derive(Debug)]
pub enum ShardSeqState {
    /// The counter file is confirmed absent: the path resolves through real
    /// directories to a missing leaf (never a broken path masquerading as
    /// absence).
    Missing,
    /// The counter was read and parsed successfully.
    Loaded(u64),
    /// The counter path exists but cannot be used; the inner class is actionable.
    Unusable(ShardSeqError),
}

#[derive(Debug)]
pub enum ShardSeqError {
    Read(anyhow::Error),
    Parse(anyhow::Error),
}

impl ShardSeqError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Read(error) | Self::Parse(error) => error,
        }
    }
}

/// Path of the shard sequence counter for one (machine, session).
pub fn shard_seq_file(stage_root: &Path, machine: &str, session_id: &str) -> PathBuf {
    session_shard_dir(stage_root, machine, session_id).join(SHARD_SEQ_FILE)
}

/// Decide whether a `NotFound` from reading the shard sequence file really
/// means "the counter is not there", or only means "the path could not be
/// resolved". The two are the same error on Windows: `ERROR_PATH_NOT_FOUND`
/// is folded into `io::ErrorKind::NotFound`, so a path component that is a
/// regular file looks exactly like a missing counter. (Mirrors
/// `sqlite_probe::confirm_absence`, which keeps the same promise.)
///
/// Absence is *confirmed*, never inferred: walk up until some ancestor exists
/// and require it to be a directory. An ancestor that exists but is not a
/// directory is a shape error. Ancestors that are themselves absent are fine —
/// a session that was never written has no directory at all, and that is
/// genuine absence, not a failure.
fn confirm_shard_seq_absence(path: &Path) -> Result<(), std::io::Error> {
    let mut ancestor = path.parent();
    while let Some(dir) = ancestor {
        match fs::metadata(dir) {
            Ok(md) if md.is_dir() => return Ok(()),
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    format!(
                        "path component {} is not a directory, so the absence of {} is unproven",
                        dir.display(),
                        path.display()
                    ),
                ))
            }
            // This ancestor is absent too; keep walking. The whole subtree
            // simply may not exist.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ancestor = dir.parent(),
            Err(e) => return Err(e),
        }
    }
    // Ran out of ancestors without meeting anything: nothing along the path
    // exists, which is absence, not failure.
    Ok(())
}

/// Read and classify the shard sequence counter without losing the filesystem
/// error kind. `Missing` is produced only by a *confirmed* absence: the
/// `NotFound` must survive [`confirm_shard_seq_absence`], which walks up to
/// the first existing ancestor and requires it to be a directory. Anything
/// else — including a `NotFound` caused by a path component being a regular
/// file, which Windows folds into the same error code — is `Unusable` and must
/// be repaired, not treated as zero.
pub fn load_shard_seq_state(stage_root: &Path, machine: &str, session_id: &str) -> ShardSeqState {
    let path = shard_seq_file(stage_root, machine, session_id);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Windows maps a path whose component is a regular file to the
            // same `NotFound` as a genuinely absent file. Absence must be
            // confirmed — a broken path is an unknown, never a fresh counter.
            return match confirm_shard_seq_absence(&path) {
                Ok(()) => ShardSeqState::Missing,
                Err(shape) => ShardSeqState::Unusable(ShardSeqError::Read(
                    anyhow::Error::new(shape).context(format!(
                        "cannot read shard sequence file {} (do not delete this file)",
                        path.display()
                    )),
                )),
            };
        }
        Err(error) => {
            return ShardSeqState::Unusable(ShardSeqError::Read(
                anyhow::Error::new(error).context(format!(
                    "cannot read shard sequence file {} (do not delete this file)",
                    path.display()
                )),
            ));
        }
    };
    match raw.trim().parse::<u64>() {
        Ok(seq) => ShardSeqState::Loaded(seq),
        Err(error) => ShardSeqState::Unusable(ShardSeqError::Parse(
            anyhow::Error::new(error).context(format!(
                "cannot parse shard sequence file {} (do not delete this file)",
                path.display()
            )),
        )),
    }
}

/// Highest shard sequence already present in the session dir (0 when none).
///
/// Seeds the counter on first run so a pre-existing stage migrates seamlessly:
/// an existing stage already has shards, so the counter must never start at 0.
fn derive_shard_high_water(
    stage_root: &Path,
    machine: &str,
    session_id: &str,
) -> anyhow::Result<u64> {
    let dir = session_shard_dir(stage_root, machine, session_id);
    Ok(sealed_shard_entries(&dir)?
        .into_iter()
        .map(|(seq, _)| seq)
        .max()
        // reason: the `?` above has already turned any read failure into an
        // error, so reaching here means the entry list is provably empty and 0
        // is the honest "no shards yet" high-water mark.
        .unwrap_or(0))
}

/// Persist the shard sequence counter durably: temp file -> fsync -> rename,
/// plus an fsync of the parent directory so the rename survives a power cut.
///
/// The counter is written BEFORE the shard it numbers, so the on-disk
/// high-watermark is never behind the shard set it governs — a stage reclaim
/// that deletes shard files can therefore never reset the next sequence
/// (ADR-020 Phase 2).
fn persist_shard_seq(
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    seq: u64,
) -> anyhow::Result<()> {
    let path = shard_seq_file(stage_root, machine, session_id);
    let parent = path.parent().expect("shard seq file has a parent");
    fs::create_dir_all(parent)
        .with_context(|| format!("create shard seq directory {}", parent.display()))?;
    let tmp = parent.join(format!(".{}.tmp", SHARD_SEQ_FILE));
    if tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    let mut f = fs::File::create(&tmp)
        .with_context(|| format!("create shard seq temp file {}", tmp.display()))?;
    f.write_all(seq.to_string().as_bytes())
        .with_context(|| format!("write shard seq temp file {}", tmp.display()))?;
    f.sync_all()
        .with_context(|| format!("fsync shard seq temp file {}", tmp.display()))?;
    drop(f);
    fs::rename(&tmp, &path)
        .with_context(|| format!("move shard seq into place {}", path.display()))?;
    // Without this the rename can still be lost on a power cut, i.e. the
    // counter would reset to the pre-rename value (or vanish) next boot.
    #[cfg(unix)]
    fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("fsync shard seq directory {}", parent.display()))?;
    Ok(())
}

/// Reserve the next sequence number for a sealed session shard set and persist
/// the advanced high-watermark counter before the caller writes the shard.
///
/// The counter, not the directory listing, is the source of sequence numbers:
/// a stage reclaim that deletes already-archived shard files must not make the
/// next sequence fall back and collide with archived shards (ADR-020 Phase 2).
/// On first run the counter does not exist yet, so the initial high-watermark
/// is derived from the shards already on disk (seamless migration — never 0).
pub fn next_shard_seq(stage_root: &Path, machine: &str, session_id: &str) -> anyhow::Result<u64> {
    let high_water = match load_shard_seq_state(stage_root, machine, session_id) {
        ShardSeqState::Missing => derive_shard_high_water(stage_root, machine, session_id)?,
        ShardSeqState::Loaded(seq) => seq,
        ShardSeqState::Unusable(error) => return Err(error.into_anyhow()),
    };
    let next = high_water + 1;
    persist_shard_seq(stage_root, machine, session_id, next)?;
    Ok(next)
}

/// Append a batch of lines as a new sealed shard. Returns the shard's file
/// name. Callers must never append to a returned shard again (sealing rule).
pub fn write_sealed_shard(
    writer: StageWriter,
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    lines: &[String],
) -> anyhow::Result<String> {
    write_sealed_shard_with_cap(
        writer,
        stage_root,
        machine,
        session_id,
        lines,
        DEFAULT_SHARD_BUCKET_CAP,
    )
}

/// Append a batch of lines as a new sealed shard in the bucket selected by its
/// sequence. Existing shards are never moved when the cap changes.
pub fn write_sealed_shard_with_cap(
    writer: StageWriter,
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    lines: &[String],
    bucket_cap: usize,
) -> anyhow::Result<String> {
    let bytes: Vec<Vec<u8>> = lines.iter().map(|line| line.as_bytes().to_vec()).collect();
    write_sealed_shard_bytes_with_cap(writer, stage_root, machine, session_id, &bytes, bucket_cap)
}

/// Append a batch of arbitrary UTF-8-independent line bytes as one sealed
/// shard. Each item is written followed by exactly one newline. The final
/// shard is installed atomically, so a crash cannot leave a file that the
/// next `push` mistakes for a complete sealed shard.
pub fn write_sealed_shard_bytes_with_cap(
    writer: StageWriter,
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    lines: &[Vec<u8>],
    bucket_cap: usize,
) -> anyhow::Result<String> {
    write_sealed_shard_bytes_with_repeat_policy(
        writer, stage_root, machine, session_id, lines, bucket_cap, false,
    )
}

/// Seal a source capture even when its bytes equal an earlier shard.
/// JSONL collection uses this for validated appended bytes or a fresh source
/// whose provenance cannot establish cross-root event identity. Other paths
/// use the idempotent default writer.
pub fn write_sealed_shard_bytes_allow_exact_repeat_with_cap(
    writer: StageWriter,
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    lines: &[Vec<u8>],
    bucket_cap: usize,
) -> anyhow::Result<String> {
    write_sealed_shard_bytes_with_repeat_policy(
        writer, stage_root, machine, session_id, lines, bucket_cap, true,
    )
}

fn write_sealed_shard_bytes_with_repeat_policy(
    writer: StageWriter,
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    lines: &[Vec<u8>],
    bucket_cap: usize,
    allow_exact_repeat: bool,
) -> anyhow::Result<String> {
    let mut raw = Vec::new();
    for line in lines {
        raw.extend_from_slice(line);
        raw.push(b'\n');
    }
    write_sealed_shard_raw_with_policy(
        writer,
        stage_root,
        machine,
        session_id,
        &raw,
        bucket_cap,
        allow_exact_repeat,
    )
}

/// Install `raw` verbatim as the next sealed shard — no line framing is added.
///
/// This is what a *restore* needs: a shard copied back from a destination has
/// to land byte-for-byte, or the concatenated sha256 that both the stage and
/// the archive compute independently stops matching and every cursor speaking
/// for that session becomes unverifiable.
pub fn write_sealed_shard_raw_with_cap(
    writer: StageWriter,
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    raw: &[u8],
    bucket_cap: usize,
) -> anyhow::Result<String> {
    write_sealed_shard_raw_with_policy(
        writer, stage_root, machine, session_id, raw, bucket_cap, false,
    )
}

fn write_sealed_shard_raw_with_policy(
    writer: StageWriter,
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    raw: &[u8],
    bucket_cap: usize,
    allow_exact_repeat: bool,
) -> anyhow::Result<String> {
    assert_stage_writer_audited(writer)?;
    crate::shard_writer::write_shard(
        stage_root,
        machine,
        session_id,
        bucket_cap,
        crate::shard_writer::ShardSource::Store {
            raw,
            writer,
            allow_exact_repeat,
        },
    )
    .map(crate::shard_writer::ShardWrite::filename)
}

/// Find an already sealed shard with the same SHA-256 in one session.
pub fn find_duplicate_shard(
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    raw: &[u8],
) -> anyhow::Result<Option<String>> {
    let dir = session_shard_dir(stage_root, machine, session_id);
    let mut existing = sealed_shard_entries(&dir)?;
    existing.sort_by_key(|(seq, _)| *seq);
    let wanted_hash = Sha256::digest(raw);
    for (seq, path) in existing {
        let existing_bytes = fs::read(&path).with_context(|| {
            format!("read existing shard {} for duplicate check", path.display())
        })?;
        if Sha256::digest(&existing_bytes) == wanted_hash {
            return Ok(Some(shard_filename(seq)));
        }
    }
    Ok(None)
}

/// Environment variable the **index builders** read: set it to a non-empty value
/// other than `0`, `false`, `no` or `off` and an activity or full-text index
/// keeps every sealed shard instead of collapsing a whole-content replay.
///
/// The CLI has its own explicit `--no-collapse` flag on `read` and `export`;
/// this variable is the same opt-out for the index paths, which have no flag of
/// their own. It is read per build and changes nothing that is stored.
pub const KEEP_ALL_SHARDS_ENV: &str = "CHAT_STASHER_NO_COLLAPSE";

/// How a reader joins a session's sealed shards into its body.
///
/// The default is [`Self::Collapse`]: a run of shards that byte-for-byte replays
/// the session's complete preceding shard sequence is dropped to its first copy,
/// because that shape is exactly what a re-seal leaves and no reader can tell it
/// from new content that happens to repeat the whole session. The stored shards
/// are never touched either way — this decides only what the reader returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DuplicateShardPolicy {
    /// Collapse a complete-prefix replay run to its first copy.
    #[default]
    Collapse,
    /// Keep every shard, replays included. The opt-out.
    KeepAll,
}

impl DuplicateShardPolicy {
    /// The policy [`KEEP_ALL_SHARDS_ENV`] asks for in this process.
    pub fn from_env() -> Self {
        Self::from_env_value(std::env::var_os(KEEP_ALL_SHARDS_ENV).as_deref())
    }

    /// The policy a variable's *value* asks for. An unset, empty, `0`, `false`,
    /// `no` or `off` value is [`Self::Collapse`]; anything else is
    /// [`Self::KeepAll`]. Split out from [`Self::from_env`] so a test can decide
    /// a value without mutating the process environment.
    pub fn from_env_value(value: Option<&std::ffi::OsStr>) -> Self {
        let keep = match value {
            None => false,
            Some(value) => {
                let value = value.to_string_lossy();
                let value = value.trim();
                !value.is_empty()
                    && !matches!(
                        value.to_ascii_lowercase().as_str(),
                        "0" | "false" | "no" | "off"
                    )
            }
        };
        if keep {
            Self::KeepAll
        } else {
            Self::Collapse
        }
    }

    /// Whether a reader following this policy drops whole-content replays.
    pub fn collapses(self) -> bool {
        matches!(self, Self::Collapse)
    }
}

/// Indices in a later run that replay the complete preceding shard sequence.
///
/// Sequence order is the caller's order. Each replay shard must equal its
/// corresponding preceding shard; concatenated byte ranges are never compared.
/// A repeated individual shard that is not a complete replay remains present.
pub fn duplicate_shard_indices<T: Eq>(shards: &[T]) -> Vec<usize> {
    let mut duplicates = Vec::new();
    let mut preceding = Vec::new();
    let mut index = 0;
    while index < shards.len() {
        if !preceding.is_empty()
            && index + preceding.len() <= shards.len()
            && preceding
                .iter()
                .enumerate()
                .all(|(offset, previous)| shards[index + offset] == shards[*previous])
        {
            duplicates.extend(index..index + preceding.len());
            index += preceding.len();
            continue;
        }
        preceding.push(index);
        index += 1;
    }
    duplicates
}

/// Keep the first sequence and later content unless a full shard-by-shard
/// replay of that sequence follows it.
pub fn unique_shard_bodies(shards: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    select_shard_bodies(shards, DuplicateShardPolicy::Collapse)
}

/// Apply a [`DuplicateShardPolicy`] to a session's shards: drop the copies a
/// whole-content replay repeats under [`DuplicateShardPolicy::Collapse`], or
/// return every shard unchanged under [`DuplicateShardPolicy::KeepAll`].
pub fn select_shard_bodies(shards: Vec<Vec<u8>>, policy: DuplicateShardPolicy) -> Vec<Vec<u8>> {
    if !policy.collapses() {
        return shards;
    }
    let duplicates: BTreeSet<_> = duplicate_shard_indices(&shards).into_iter().collect();
    shards
        .into_iter()
        .enumerate()
        .filter_map(|(index, shard)| (!duplicates.contains(&index)).then_some(shard))
        .collect()
}

/// The contiguous runs of a session's shard sequence that a
/// [`DuplicateShardPolicy::Collapse`] read drops, each as
/// `(first_index, shard_count)` in sequence order.
///
/// [`duplicate_shard_indices`] is the single source of which shards are dropped;
/// this only groups its ascending indices so the repair inventory can name each
/// collapsed run rather than a flat count. It returns no runs for
/// [`DuplicateShardPolicy::KeepAll`], because that read drops nothing.
pub fn collapsed_shard_runs<T: Eq>(
    shards: &[T],
    policy: DuplicateShardPolicy,
) -> Vec<(usize, usize)> {
    if !policy.collapses() {
        return Vec::new();
    }
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for index in duplicate_shard_indices(shards) {
        match runs.last_mut() {
            Some((start, len)) if *start + *len == index => *len += 1,
            _ => runs.push((index, 1)),
        }
    }
    runs
}

/// Find sealed shards in both layouts: legacy files directly under the
/// session directory and new files one directory below it. The result carries
/// the parsed global sequence so callers can sort across buckets.
pub fn sealed_shard_entries(session_dir: &Path) -> anyhow::Result<Vec<(u64, PathBuf)>> {
    let mut out = Vec::new();
    let rd = match fs::read_dir(session_dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("read session shard dir {}", session_dir.display()))
        }
    };
    for entry in rd {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_file() {
            if let Some(seq) = parse_shard_seq(&entry.file_name().to_string_lossy()) {
                out.push((seq, path));
            }
        } else if file_type.is_dir() {
            for child in fs::read_dir(&path)? {
                let child = child?;
                if !child.file_type()?.is_file() {
                    continue;
                }
                if let Some(seq) = parse_shard_seq(&child.file_name().to_string_lossy()) {
                    out.push((seq, child.path()));
                }
            }
        }
    }
    Ok(out)
}

// ----------------------------------------------------------- rustic's cache

/// Where `rustic_core` keeps this machine's local metadata cache.
///
/// Opens honour the per-config cache knobs ([`StoreConfig::cache_dir`],
/// [`StoreConfig::no_cache`]); when neither is set they behave exactly like the
/// `RepositoryOptions::default()` this list used to document — cache enabled,
/// falling back to `dirs::cache_dir()/rustic` (rustic_core-0.12.0
/// `src/backend/cache.rs:261`, `src/repository.rs:549`). `dirs` spells that
/// directory differently per platform, and on Windows it is not under `$HOME`
/// at all (dirs-6.0.0 `src/win.rs:10` → `known_folder_local_app_data()`).
///
/// The cache holds metadata only — snapshots, index, and *tree* packs
/// (rustic_core-0.12.0 `src/backend.rs:82`, `src/blob.rs:54`) — so anything
/// that needs to observe a repository whose metadata has changed underneath it
/// has to find this directory, and must not guess at it from `$HOME`.
///
/// A list, not one path: the Windows spelling cannot be exercised from the
/// other two platforms, so an extra candidate is cheap insurance — callers
/// match on content, and a candidate that does not exist costs nothing.
pub fn rustic_cache_roots() -> Vec<PathBuf> {
    crate::scanner::user_cache_dirs()
        .into_iter()
        .map(|base| base.join("rustic"))
        .collect()
}

// ------------------------------------------------------------------- keys

/// Serialise a `MasterKey` (it is `serde.Deserialize`d from the same json by
/// `parse_key`). The masterkey is the repository's only key — losing it means
/// the repo is unreadable forever (verified in spike A6).
pub fn serialize_key(mk: &MasterKey) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(mk)?)
}

pub fn parse_key(raw: &str) -> anyhow::Result<MasterKey> {
    Ok(serde_json::from_str(raw)?)
}

/// The three semantic outcomes of opening the key path.
///
/// This is deliberately not folded into anyhow::Result: adding context to a
/// plain error makes it unsafe for the repo-init caller to distinguish
/// NotFound from an existing file that could not be read or parsed. Missing
/// is the only outcome that permits key creation; Unusable keeps read and
/// parse failures separate so the user gets the right repair instruction.
#[derive(Debug)]
pub enum KeyFileState {
    /// The key path does not exist (io::ErrorKind::NotFound).
    Missing,
    /// The key was read and parsed successfully.
    Loaded(MasterKey),
    /// The path exists but cannot be used; the inner class is actionable.
    Unusable(KeyFileError),
}

#[derive(Debug)]
pub enum KeyFileError {
    Read(anyhow::Error),
    Parse(anyhow::Error),
}

impl KeyFileError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Read(error) | Self::Parse(error) => error,
        }
    }
}

/// Write the masterkey to `cfg.key_file`, owner-readable only where the
/// platform can express that, and **do not return `Ok` until it is on the
/// disk**.
///
/// The mode is set *when the file is created*, not afterwards: a `write` then
/// `set_permissions` pair leaves a window in which the only key to the whole
/// archive is world-readable, and that window is exactly what an unprivileged
/// process on a shared machine would wait for. On platforms without unix modes
/// the file inherits whatever the filesystem gives it, and
/// `docs-dev/threat-model.md` says so rather than implying protection we do not
/// provide.
///
/// Durability is part of the contract, not a bonus: this function's `Ok` is
/// what the CLI turns into the words "masterkey created+persisted", and the
/// caller then writes an archive that *only this key can ever open*. A plain
/// `write` that is still in the page cache would make those words a lie after a
/// power cut — the archive would survive and the key would not. So the key goes
/// down the same route as a sealed shard (`seal_shard`): temp file -> fsync ->
/// rename, plus an fsync of the directory so the rename itself is durable.
pub fn persist_key_file(cfg: &StoreConfig, mk: &MasterKey) -> anyhow::Result<()> {
    let parent = cfg
        .key_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let name = cfg
        .key_file
        .file_name()
        .with_context(|| {
            format!(
                "masterkey path has no file name: {}",
                cfg.key_file.display()
            )
        })?
        .to_owned();
    fs::create_dir_all(&parent)
        .with_context(|| format!("create masterkey directory {}", parent.display()))?;
    // The directory listing alone reveals nothing secret, but a 0700 parent
    // keeps the key out of reach even if a later write forgets its own mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        #[allow(
            clippy::let_underscore_must_use,
            reason = "This existing Unix hardening call is intentionally best-effort; key serialization and the write remain fallible below."
        )]
        let _ = fs::set_permissions(&parent, fs::Permissions::from_mode(0o700));
    }
    let body = serialize_key(mk)?;
    let tmp = parent.join(format!(".{}.tmp", name.to_string_lossy()));

    {
        #[cfg(unix)]
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut f = options
            .open(&tmp)
            .with_context(|| format!("create masterkey file {}", tmp.display()))?;
        // A leftover temp file keeps its old mode when reopened, so tighten it
        // before any key material is written into it.
        #[cfg(unix)]
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        f.write_all(body.as_bytes())
            .with_context(|| format!("write masterkey file {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync masterkey file {}", tmp.display()))?;
    }

    fs::rename(&tmp, &cfg.key_file)
        .with_context(|| format!("move masterkey into place {}", cfg.key_file.display()))?;

    // Without this the rename can still be lost on a power cut, i.e. the key
    // file would simply not exist next boot. Failing here is deliberate: an
    // unproven key is not a persisted key.
    #[cfg(unix)]
    fs::File::open(&parent)
        .and_then(|dir| dir.sync_all())
        .with_context(|| format!("fsync masterkey directory {}", parent.display()))?;

    Ok(())
}

/// Load the masterkey from `cfg.key_file` (error when missing).
pub fn load_key_file(cfg: &StoreConfig) -> anyhow::Result<MasterKey> {
    match load_key_file_state(cfg) {
        KeyFileState::Missing => Err(anyhow!(
            "cannot read masterkey file {} (lost key?)",
            cfg.key_file.display()
        )),
        KeyFileState::Loaded(mk) => Ok(mk),
        KeyFileState::Unusable(error) => Err(error.into_anyhow()),
    }
}

/// Read and classify the masterkey without losing the filesystem error kind.
pub fn load_key_file_state(cfg: &StoreConfig) -> KeyFileState {
    let raw = match fs::read_to_string(&cfg.key_file) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return KeyFileState::Missing;
        }
        Err(error) => {
            return KeyFileState::Unusable(KeyFileError::Read(anyhow::Error::new(error).context(
                format!("cannot read masterkey file {}", cfg.key_file.display()),
            )));
        }
    };
    match parse_key(&raw) {
        Ok(mk) => KeyFileState::Loaded(mk),
        Err(error) => KeyFileState::Unusable(KeyFileError::Parse(error.context(format!(
            "cannot parse masterkey file {}",
            cfg.key_file.display()
        )))),
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// A second `impl` block, kept at the end of the file on purpose: several
// documents cite this file by line range (`docs-dev/privacy.md`,
// `docs-dev/threat-model.md`, `README.md`, `docs-dev/install.md`), so code added
// anywhere above the last cited line re-numbers anchors that have not changed
// — the same reason `Cargo.toml` appends dependencies instead of inserting
// them. Nothing here needs to sit beside its siblings.
impl StoreConfig {
    /// The option map handed to a remote backend: the connections cap plus the
    /// destination's own options, values untouched.
    ///
    /// Split out of `Store::backends` so a test can assert what reaches the
    /// backend without opening anything. The credential switches are why that
    /// matters: `disable_config_load` and `disable_ec2_metadata` are the
    /// backend's own booleans (opendal-service-s3 0.57.0 `src/backend.rs` lines
    /// 856-861), this tool holds options as the strings a TOML file holds, and
    /// the backend reads them through a deserialiser that accepts `"true"` and
    /// `"on"` (`opendal-core-0.57.0 src/raw/serde_util.rs` lines 121-127). A
    /// value rewritten here — trimmed, lower-cased, or dropped for looking
    /// redundant — would leave a credential source switched on, and nothing
    /// downstream would say so.
    pub fn backend_options(&self) -> BTreeMap<String, String> {
        let mut options = BTreeMap::new();
        options.insert("connections".to_string(), self.connections.to_string());
        options.extend(self.options.iter().map(|(k, v)| (k.clone(), v.clone())));
        options
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn ri2_characterize_store_shard_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let raw = b"{\"synthetic\":1}\r\n\xffunterminated";
        let name = write_sealed_shard_raw_with_cap(
            StageWriter::Collect,
            dir.path(),
            "synthetic-machine",
            "synthetic-session",
            raw,
            1,
        )
        .unwrap();
        assert_eq!(name, "000001.jsonl");
        let path = shard_path_with_cap(dir.path(), "synthetic-machine", "synthetic-session", 1, 1);
        assert!(fs::read(path).unwrap() == raw);
        assert_eq!(
            write_sealed_shard_raw_with_cap(
                StageWriter::Collect,
                dir.path(),
                "synthetic-machine",
                "synthetic-session",
                raw,
                1
            )
            .unwrap(),
            name
        );
        assert_eq!(
            write_sealed_shard_raw_with_cap(
                StageWriter::Restore,
                dir.path(),
                "synthetic-machine",
                "synthetic-session",
                raw,
                1
            )
            .unwrap(),
            "000002.jsonl"
        );
        assert!(
            fs::read(shard_path_with_cap(
                dir.path(),
                "synthetic-machine",
                "synthetic-session",
                2,
                1
            ))
            .unwrap()
                == raw
        );
        let lines = vec![b"synthetic".to_vec(), vec![], vec![0xff]];
        write_sealed_shard_bytes_with_cap(
            StageWriter::Collect,
            dir.path(),
            "synthetic-machine",
            "synthetic-lines",
            &lines,
            1,
        )
        .unwrap();
        assert!(
            fs::read(shard_path_with_cap(
                dir.path(),
                "synthetic-machine",
                "synthetic-lines",
                1,
                1
            ))
            .unwrap()
                == b"synthetic\n\n\xff\n"
        );
    }
    use super::*;
    use std::fs;

    #[test]
    fn connections_are_clamped_to_ceiling() {
        let cfg = StoreConfig {
            repo_root: "/tmp/x".into(),
            key_file: PathBuf::from("/tmp/x.key"),
            options: BTreeMap::new(),
            connections: 0,
            cache_dir: None,
            no_cache: false,
        }
        .with_capped_connections(None);
        assert_eq!(cfg.connections, DEFAULT_CONNECTIONS);
        let cfg2 = StoreConfig {
            repo_root: "/tmp/x".into(),
            key_file: PathBuf::from("/tmp/x.key"),
            options: BTreeMap::new(),
            connections: 0,
            cache_dir: None,
            no_cache: false,
        }
        .with_capped_connections(Some(3));
        assert_eq!(cfg2.connections, 3);
        let cfg3 = StoreConfig {
            repo_root: "/tmp/x".into(),
            key_file: PathBuf::from("/tmp/x.key"),
            options: BTreeMap::new(),
            connections: 0,
            cache_dir: None,
            no_cache: false,
        }
        .with_capped_connections(Some(100));
        // Over-large values clamp to the hard backend ceiling, NOT to the
        // (deliberately lower) measured default — the two are separate knobs.
        assert_eq!(cfg3.connections, MAX_CONNECTIONS);
        // Both sides are constants, so the guard is a compile-time check:
        // a const block keeps it failing at build time instead of at test
        // time (clippy::assertions_on_constants).
        const {
            assert!(
                DEFAULT_CONNECTIONS < MAX_CONNECTIONS,
                "default must stay below the ceiling: raising concurrency buys no \
                 measured speed (D2) but does open masters that must be reaped"
            );
        };
        let cfg4 = StoreConfig {
            repo_root: "/tmp/x".into(),
            key_file: PathBuf::from("/tmp/x.key"),
            options: BTreeMap::new(),
            connections: 0,
            cache_dir: None,
            no_cache: false,
        }
        .with_capped_connections(Some(0));
        assert_eq!(cfg4.connections, 1);
    }

    /// ADR-020 Phase 5: the cache knobs must actually reach rustic's
    /// `RepositoryOptions`. Pure and injectable — no repository, no remote.
    #[test]
    fn repository_options_forwards_cache_switches() {
        // Default (fields unset): exactly rustic's own default — cache on,
        // standard per-machine cache dir. Callers that never set the new
        // fields must get byte-for-byte what `RepositoryOptions::default()`
        // used to produce.
        let cfg = StoreConfig {
            repo_root: "/tmp/x".into(),
            key_file: PathBuf::from("/tmp/x.key"),
            options: BTreeMap::new(),
            connections: 1,
            cache_dir: None,
            no_cache: false,
        };
        let opts = cfg.repository_options();
        assert!(!opts.no_cache, "default must keep the cache enabled");
        assert!(
            opts.cache_dir.is_none(),
            "default must keep rustic's standard per-machine cache dir"
        );

        // no_cache=true turns the cache off; a custom dir is deliberately not
        // forwarded (rustic declares the two conflicts_with).
        let cfg = StoreConfig {
            no_cache: true,
            cache_dir: Some(PathBuf::from("/tmp/cache")),
            ..cfg.clone()
        };
        let opts = cfg.repository_options();
        assert!(opts.no_cache, "no_cache must reach RepositoryOptions");
        assert!(
            opts.cache_dir.is_none(),
            "a custom dir under no_cache is contradictory and must not be forwarded"
        );

        // cache_dir set: cache stays on but is redirected.
        let cfg = StoreConfig {
            no_cache: false,
            cache_dir: Some(PathBuf::from("/tmp/cache")),
            ..cfg
        };
        let opts = cfg.repository_options();
        assert!(!opts.no_cache);
        assert_eq!(
            opts.cache_dir.as_deref(),
            Some(Path::new("/tmp/cache")),
            "cache_dir must reach RepositoryOptions"
        );
    }

    /// The credential switches must reach the backend exactly as written.
    ///
    /// This covers this tool's half of the claim and only that half: it says
    /// `disable_ec2_metadata = "true"` in a config file becomes the string
    /// `true` under that key in the map the backend is built from. Whether the
    /// backend then honours it is the pinned crate's behaviour, cited in
    /// `docs-dev/install.md` §4.5 against that crate, not asserted here — opendal
    /// is not a direct dependency of this crate, so a test that called into its
    /// config deserialiser would add an edge to the build graph. Pure and
    /// injectable — no backend is built, so this opens nothing.
    #[test]
    fn backend_options_forward_credential_switches_verbatim() {
        let mut options = BTreeMap::new();
        options.insert("disable_config_load".to_string(), "true".to_string());
        options.insert("disable_ec2_metadata".to_string(), "true".to_string());
        // Values a normalising layer would be tempted to touch: a bool spelled
        // the other way the backend's deserialiser accepts, and a key in the
        // case a TOML writer might have chosen.
        options.insert("SKIP_SIGNATURE".to_string(), "off".to_string());
        let cfg = StoreConfig {
            repo_root: "opendal:s3".into(),
            key_file: PathBuf::from("/tmp/x.key"),
            options,
            connections: 4,
            cache_dir: None,
            no_cache: false,
        };

        let forwarded = cfg.backend_options();
        assert_eq!(
            forwarded.get("disable_config_load").map(String::as_str),
            Some("true"),
            "a credential switch must reach the backend unrewritten"
        );
        assert_eq!(
            forwarded.get("disable_ec2_metadata").map(String::as_str),
            Some("true"),
            "the metadata switch is a separate option and must be forwarded too"
        );
        assert_eq!(
            forwarded.get("SKIP_SIGNATURE").map(String::as_str),
            Some("off"),
            "keys and values are the backend's, not ours to normalise"
        );
        assert_eq!(
            forwarded.get("connections").map(String::as_str),
            Some("4"),
            "the connections cap rides in the same map"
        );
    }

    #[test]
    fn shard_names_roundtrip() {
        assert_eq!(shard_filename(1), "000001.jsonl");
        assert_eq!(shard_filename(42), "000042.jsonl");
        assert_eq!(parse_shard_seq("000001.jsonl"), Some(1));
        assert_eq!(parse_shard_seq("000042.jsonl"), Some(42));
        assert_eq!(parse_shard_seq("000000.jsonl"), Some(0));
        assert_eq!(parse_shard_seq("000001.json"), None);
        assert_eq!(parse_shard_seq("000001.txt"), None);
        assert_eq!(parse_shard_seq("0000000.jsonl"), None); // wrong width
    }

    #[test]
    fn duplicate_shard_detection_requires_a_shard_by_shard_complete_replay() {
        let a = b"alpha\n".to_vec();
        let b = b"beta\n".to_vec();
        let ab = [a.as_slice(), b.as_slice()].concat();
        assert_eq!(
            duplicate_shard_indices(&[a.clone(), b.clone(), a.clone(), b.clone()]),
            vec![2, 3]
        );
        assert!(duplicate_shard_indices(&[a.clone(), b.clone(), a.clone()]).is_empty());
        assert!(duplicate_shard_indices(&[a, b, ab]).is_empty());
    }

    #[test]
    fn keep_all_policy_returns_every_shard_and_collapse_drops_the_run() {
        let a = b"alpha\n".to_vec();
        let b = b"beta\n".to_vec();
        let resealed = vec![a.clone(), b.clone(), a.clone(), b.clone()];

        assert_eq!(
            select_shard_bodies(resealed.clone(), DuplicateShardPolicy::Collapse),
            vec![a.clone(), b.clone()],
            "the default keeps the first sequence and drops the replay"
        );
        assert_eq!(
            select_shard_bodies(resealed.clone(), DuplicateShardPolicy::KeepAll),
            resealed,
            "the opt-out returns every stored shard unchanged"
        );

        assert_eq!(
            collapsed_shard_runs(&resealed, DuplicateShardPolicy::Collapse),
            vec![(2, 2)],
            "the replay is one run starting at index 2"
        );
        assert!(
            collapsed_shard_runs(&resealed, DuplicateShardPolicy::KeepAll).is_empty(),
            "the opt-out collapses nothing, so it names no run"
        );

        // A single-shard session sealed twice: the shape the ruling calls out.
        let doubled = vec![a.clone(), a.clone()];
        assert_eq!(
            collapsed_shard_runs(&doubled, DuplicateShardPolicy::Collapse),
            vec![(1, 1)]
        );
        assert_eq!(
            select_shard_bodies(doubled.clone(), DuplicateShardPolicy::Collapse),
            vec![a.clone()]
        );
        assert_eq!(
            select_shard_bodies(doubled, DuplicateShardPolicy::KeepAll).len(),
            2
        );
    }

    #[test]
    fn keep_all_env_value_is_truthy_only_when_explicitly_on() {
        assert_eq!(
            DuplicateShardPolicy::from_env_value(None),
            DuplicateShardPolicy::Collapse
        );
        for on in ["1", "true", "yes", "on", "keep-all", "TRUE"] {
            assert_eq!(
                DuplicateShardPolicy::from_env_value(Some(std::ffi::OsStr::new(on))),
                DuplicateShardPolicy::KeepAll,
                "`{on}` must turn the opt-out on"
            );
        }
        for off in ["", "  ", "0", "false", "no", "off", "FALSE"] {
            assert_eq!(
                DuplicateShardPolicy::from_env_value(Some(std::ffi::OsStr::new(off))),
                DuplicateShardPolicy::Collapse,
                "`{off}` must leave the default collapse in place"
            );
        }
    }

    #[test]
    fn shard_path_lays_out_partition() {
        let stage = Path::new("/tmp/stage");
        assert_eq!(
            shard_path(stage, "mbp-2", "sess-1", 7),
            PathBuf::from("/tmp/stage/sessions/mbp-2/sess-1/000/000007.jsonl")
        );
    }

    #[test]
    fn every_registered_stage_writer_has_a_reconciliation_hook() {
        assert_eq!(STAGE_WRITER_REGISTRY.len(), 4);
        for registration in STAGE_WRITER_REGISTRY {
            assert!(
                registration
                    .reconciliation_hook
                    .is_some_and(|hook| !hook.is_empty()),
                "stage writer {} is missing its reconciliation hook",
                registration.writer.name()
            );
            assert_stage_writer_audited(registration.writer).unwrap();
        }
    }

    #[test]
    fn writing_an_identical_shard_for_the_same_session_is_a_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let first = write_sealed_shard_raw_with_cap(
            StageWriter::Collect,
            &stage,
            "machine-one",
            "session-one",
            b"same body\n",
            20,
        )
        .unwrap();
        let replay = write_sealed_shard_raw_with_cap(
            StageWriter::Collect,
            &stage,
            "machine-one",
            "session-one",
            b"same body\n",
            20,
        )
        .unwrap();

        assert_eq!(
            replay, first,
            "an identical replay names the existing shard"
        );
        let shards =
            sealed_shard_entries(&session_shard_dir(&stage, "machine-one", "session-one")).unwrap();
        assert_eq!(shards.len(), 1, "the writer must not append a duplicate");
        assert_eq!(fs::read(&shards[0].1).unwrap(), b"same body\n");
    }

    /// W292: `push_only_if_changed` is the push-level half of the no-op check
    /// `dest-init` needs — `push_only_if_changed` the *config knob* decides
    /// whether a push is attempted, and this decides whether an attempted one
    /// has anything to write. The control is the first assertion: a plain push
    /// has no such check, which is what makes the second push's absent snapshot
    /// a property of this call and not of the repository.
    #[test]
    fn push_only_if_changed_leaves_an_identical_tree_unpublished() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let machine = "w292-fixture".to_string();
        write_sealed_shard(
            StageWriter::Collect,
            &stage,
            &machine,
            "session-one",
            &["synthetic".to_string()],
        )
        .unwrap();
        let store_for = |name: &str| {
            BackupStore::new(
                StoreConfig {
                    repo_root: dir.path().join(name).to_string_lossy().into_owned(),
                    key_file: dir.path().join(format!("{name}-key.json")),
                    connections: 1,
                    options: BTreeMap::new(),
                    // W289: the cache stays on, only its location moves.
                    cache_dir: Some(dir.path().join("rustic-cache")),
                    no_cache: false,
                },
                machine.clone(),
            )
        };
        let mk = MasterKey::new();

        // Control: two plain pushes of one unchanged stage, two snapshots —
        // this call is a request to record the stage as it stands.
        let plain = store_for("plain-repo");
        assert!(plain.push(&stage, &mk).unwrap().snapshot_written);
        let second_plain = plain.push(&stage, &mk).unwrap();
        assert!(second_plain.snapshot_written, "{second_plain:?}");
        assert_eq!(second_plain.snapshots_in_repo, 2, "{second_plain:?}");

        // The same stage, through the check: the first seeds the destination,
        // the second is compared against what it holds and left unpublished.
        let checked = store_for("checked-repo");
        let seeded = checked.push_only_if_changed(&stage, &mk).unwrap();
        assert!(
            seeded.snapshot_written,
            "a fresh destination must be seeded"
        );
        let repeated = checked.push_only_if_changed(&stage, &mk).unwrap();
        assert!(
            !repeated.snapshot_written,
            "an unchanged stage must not be published a second time: {repeated:?}"
        );
        assert_eq!(repeated.files_new, 0, "nothing was uploaded: {repeated:?}");
        assert_eq!(repeated.data_added, 0, "no data was added: {repeated:?}");
        assert_eq!(
            repeated.snapshots_in_repo, seeded.snapshots_in_repo,
            "the destination's snapshot count must not move: {repeated:?}"
        );

        // ...and a stage that changed is published again.
        write_sealed_shard(
            StageWriter::Collect,
            &stage,
            &machine,
            "session-two",
            &["synthetic".to_string()],
        )
        .unwrap();
        let after_change = checked.push_only_if_changed(&stage, &mk).unwrap();
        assert!(
            after_change.snapshot_written,
            "new content must still be published: {after_change:?}"
        );
        assert_eq!(
            after_change.snapshots_in_repo,
            seeded.snapshots_in_repo + 1,
            "{after_change:?}"
        );
    }

    #[test]
    fn push_rejects_a_stage_partition_owned_by_another_machine_before_backup() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        write_sealed_shard(
            StageWriter::Collect,
            &stage,
            "machine-a",
            "session-a",
            &["synthetic".to_string()],
        )
        .unwrap();
        let repo = dir.path().join("repo");
        let cfg = StoreConfig {
            repo_root: repo.to_string_lossy().into_owned(),
            key_file: dir.path().join("key.json"),
            connections: 1,
            options: BTreeMap::new(),
            cache_dir: None,
            no_cache: false,
        };
        let store = BackupStore::new(cfg, "machine-b".to_string());
        let err = store.push(&stage, &MasterKey::new()).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("stage machine mismatch"));
        assert!(message.contains("unexpected_machine_dirs=1"));
        assert!(
            !repo.exists(),
            "machine validation must run before repository initialisation"
        );
        #[allow(
            clippy::let_underscore_must_use,
            reason = "Test-directory cleanup is intentionally best-effort after the assertions have completed."
        )]
        let _ = fs::remove_dir_all(repo);
    }

    /// The masterkey is the only thing standing between another process running
    /// as you and the whole archive, so it must not land with the umask's
    /// default mode. The control assertion matters as much as the subject: a
    /// plain `fs::write` in the same directory is checked first, so a test that
    /// passes because the filesystem hands out 0600 anyway would be caught.
    ///
    /// Unix-only: the whole mechanism is the POSIX mode bit — including the
    /// control, which needs a filesystem that visibly hands out a loose default
    /// mode so the test can tell a fix apart from the default. Windows has no
    /// mode bits (its `std::fs::Permissions` carries an ACL read/write pair,
    /// not owner/group/other), so neither the subject nor the control can be
    /// expressed. The owner-only restriction on Windows is enforced through
    /// `persist_identity`'s ACL-equivalent path elsewhere and is not asserted
    /// here; the file's *writable only by the owner* property is covered on
    /// Windows by the sibling roundtrip test
    /// `normal_path_still_creates_a_key_that_the_next_run_loads` (b66), which
    /// proves the key lands and loads on every platform.
    #[cfg(unix)]
    #[test]
    fn masterkey_file_is_owner_only_and_a_plain_write_is_not() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("cs-keyperm-{}", std::process::id()));
        #[allow(
            clippy::let_underscore_must_use,
            reason = "Test setup cleanup is intentionally best-effort before recreating the temporary directory."
        )]
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Control: prove the instrument can see a *loose* mode here.
        let loose = dir.join("control.txt");
        fs::write(&loose, b"x").unwrap();
        let loose_mode = fs::metadata(&loose).unwrap().permissions().mode() & 0o777;
        assert_ne!(
            loose_mode, 0o600,
            "control file came out 0600 on its own; this test could not tell a fix from the default"
        );

        let cfg = StoreConfig {
            repo_root: dir.join("repo").display().to_string(),
            key_file: dir.join("masterkey.json"),
            connections: 1,
            options: BTreeMap::new(),
            cache_dir: None,
            no_cache: false,
        };
        persist_key_file(&cfg, &MasterKey::new()).unwrap();
        let mode = fs::metadata(&cfg.key_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "masterkey file mode was {mode:o}, expected 600"
        );

        // Rewriting an existing key file must not relax it either.
        persist_key_file(&cfg, &MasterKey::new()).unwrap();
        let again = fs::metadata(&cfg.key_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(again, 0o600, "rewrite left mode {again:o}, expected 600");

        #[allow(
            clippy::let_underscore_must_use,
            reason = "Test-directory cleanup is intentionally best-effort after the permission assertions."
        )]
        let _ = fs::remove_dir_all(&dir);
    }
}
