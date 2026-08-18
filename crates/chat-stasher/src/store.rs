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
    BackupOptions, ConfigOptions, Credentials, FileType, IndexedFullStatus, KeyOptions, LsOptions,
    ParentOptions, PathList, Repository, RepositoryBackends, RepositoryOptions, SnapshotOptions,
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

/// Everything BackupStore needs to reach a repository.
#[derive(Debug, Clone)]
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
    pub snapshot_host: String,
    pub repo_was_init: bool,
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
        BackupStore { cfg, machine }
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
        }
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
            let mut options = BTreeMap::new();
            options.insert("connections".to_string(), self.cfg.connections.to_string());
            options.extend(self.cfg.options.iter().map(|(k, v)| (k.clone(), v.clone())));
            opts = opts.options(options);
        }
        opts.to_backends().context("build backend options")
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
        let backends = self.backends()?;
        let repo = Repository::new(&RepositoryOptions::default(), &backends)?;
        let creds = Credentials::Masterkey(mk.clone());
        if self.repo_exists(&backends)? {
            let r = repo
                .open(&creds)
                .context("open existing repository")?
                .to_indexed()
                .context("index repository")?;
            Ok((r, false))
        } else {
            let config_opts = ConfigOptions::default().set_append_only(true);
            let r = repo
                .init(&creds, &KeyOptions::default(), &config_opts)
                .context("init new repository")?
                .to_indexed()
                .context("index new repository")?;
            Ok((r, true))
        }
    }

    /// Push the whole stage tree (`sessions/<machine>/<id>/NNNNNN.jsonl`) into
    /// a fresh snapshot. The stage root must already hold only sealed shards.
    pub fn push(&self, stage_root: &Path, mk: &MasterKey) -> anyhow::Result<PushSummary> {
        // This must run before opening or backing up the repository. A stage
        // assembled for machine A must never be snapshotted as machine B.
        validate_stage_machines(stage_root, &self.machine)?;
        let stage_shards = sealed_shard_count(stage_root)?;
        if stage_shards == 0 {
            anyhow::bail!(
                "refusing empty snapshot: stage contains no sealed shards; collect or restore the stage first"
            );
        }
        let (r, init) = self.open_or_init(mk)?;
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
        let build_opts = BackupOptions::default().parent_opts(
            ParentOptions::default()
                .ignore_ctime(true)
                .ignore_inode(true),
        );
        let snap = r
            .backup(&build_opts, &source, snap)
            .context("run rustic backup")?;

        let snaps = r.get_all_snapshots().context("list snapshots")?;
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
            snapshot_host: snap.hostname.clone(),
            repo_was_init: init,
        })
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
        let backends = self.backends()?;
        let repo = Repository::new(&RepositoryOptions::default(), &backends)?
            .open(&Credentials::Masterkey(mk.clone()))
            .context("open repository for consumed audit")?
            .to_indexed()
            .context("index repository for consumed audit")?;
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
    fn open_with_newest_snapshot(
        &self,
        mk: &MasterKey,
    ) -> anyhow::Result<(Repository<IndexedFullStatus>, SnapshotFile)> {
        let backends = self.backends()?;
        let repo = Repository::new(&RepositoryOptions::default(), &backends)?
            .open(&Credentials::Masterkey(mk.clone()))?
            .to_indexed()?;
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

        let mut concat = Vec::new();
        let mut hashes = Vec::new();
        for (seq, _path, node) in entries {
            let mut buf = Vec::new();
            repo.dump(&node, &mut buf).context("dump shard")?;
            concat.extend_from_slice(&buf);
            let hash = hex_digest(&Sha256::digest(&buf));
            hashes.push((shard_filename(seq), hash));
        }
        Ok((concat, hashes))
    }
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

/// Next sequence number for a sealed session shard set (1 + highest existing).
pub fn next_shard_seq(stage_root: &Path, machine: &str, session_id: &str) -> anyhow::Result<u64> {
    let dir = session_shard_dir(stage_root, machine, session_id);
    Ok(sealed_shard_entries(&dir)?
        .into_iter()
        .map(|(seq, _)| seq)
        .max()
        // reason: session 目录下尚无任何 sealed shard 时序列号从 0 起算（+1 得首个分片序号 1）
        .unwrap_or(0)
        + 1)
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
    let mut raw = Vec::new();
    for line in lines {
        raw.extend_from_slice(line);
        raw.push(b'\n');
    }
    write_sealed_shard_raw_with_cap(writer, stage_root, machine, session_id, &raw, bucket_cap)
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
    assert_stage_writer_audited(writer)?;
    let dir = session_shard_dir(stage_root, machine, session_id);
    fs::create_dir_all(&dir)?;
    let seq = next_shard_seq(stage_root, machine, session_id)?;
    let path = shard_path_with_cap(stage_root, machine, session_id, seq, bucket_cap);
    fs::create_dir_all(path.parent().expect("shard path has bucket parent"))?;
    let tmp = path.with_file_name(format!(".{}tmp", shard_filename(seq)));
    if tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    let mut f = fs::File::create(&tmp)?;
    f.write_all(raw)?;
    f.sync_all()?;
    drop(f);
    if path.exists() {
        anyhow::bail!("sealed shard target already exists: {}", path.display());
    }
    fs::rename(&tmp, &path)?;
    Ok(shard_filename(seq))
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
/// Every repository this crate opens does so with `RepositoryOptions::default()`
/// (`open_or_init`, `consumed_shas`, `open_with_newest_snapshot`), which leaves
/// `no_cache = false` and `cache_dir = None` — so rustic falls back to
/// `dirs::cache_dir()/rustic` (rustic_core-0.12.0 `src/backend/cache.rs:261`,
/// `src/repository.rs:549`). `dirs` spells that directory differently per
/// platform, and on Windows it is not under `$HOME` at all
/// (dirs-6.0.0 `src/win.rs:10` → `known_folder_local_app_data()`).
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
/// `docs/threat-model.md` says so rather than implying protection we do not
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn connections_are_clamped_to_ceiling() {
        let cfg = StoreConfig {
            repo_root: "/tmp/x".into(),
            key_file: PathBuf::from("/tmp/x.key"),
            options: BTreeMap::new(),
            connections: 0,
        }
        .with_capped_connections(None);
        assert_eq!(cfg.connections, DEFAULT_CONNECTIONS);
        let cfg2 = StoreConfig {
            repo_root: "/tmp/x".into(),
            key_file: PathBuf::from("/tmp/x.key"),
            options: BTreeMap::new(),
            connections: 0,
        }
        .with_capped_connections(Some(3));
        assert_eq!(cfg2.connections, 3);
        let cfg3 = StoreConfig {
            repo_root: "/tmp/x".into(),
            key_file: PathBuf::from("/tmp/x.key"),
            options: BTreeMap::new(),
            connections: 0,
        }
        .with_capped_connections(Some(100));
        // Over-large values clamp to the hard backend ceiling, NOT to the
        // (deliberately lower) measured default — the two are separate knobs.
        assert_eq!(cfg3.connections, MAX_CONNECTIONS);
        assert!(
            DEFAULT_CONNECTIONS < MAX_CONNECTIONS,
            "default must stay below the ceiling: raising concurrency buys no \
             measured speed (D2) but does open masters that must be reaped"
        );
        let cfg4 = StoreConfig {
            repo_root: "/tmp/x".into(),
            key_file: PathBuf::from("/tmp/x.key"),
            options: BTreeMap::new(),
            connections: 0,
        }
        .with_capped_connections(Some(0));
        assert_eq!(cfg4.connections, 1);
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
