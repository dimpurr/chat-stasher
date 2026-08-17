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
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
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
        let _ = ThreadPoolBuilder::new()
            .num_threads(connections)
            .build_global();
    }

    pub fn new(cfg: StoreConfig, machine: String) -> Self {
        Self::limit_parallelism(cfg.connections);
        BackupStore { cfg, machine }
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
pub fn next_shard_seq(stage_root: &Path, machine: &str, session_id: &str) -> u64 {
    let dir = session_shard_dir(stage_root, machine, session_id);
    sealed_shard_entries(&dir)
        .unwrap_or_default()
        .into_iter()
        .map(|(seq, _)| seq)
        .max()
        .unwrap_or(0)
        + 1
}

/// Append a batch of lines as a new sealed shard. Returns the shard's file
/// name. Callers must never append to a returned shard again (sealing rule).
pub fn write_sealed_shard(
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    lines: &[String],
) -> anyhow::Result<String> {
    write_sealed_shard_with_cap(
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
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    lines: &[String],
    bucket_cap: usize,
) -> anyhow::Result<String> {
    let bytes: Vec<Vec<u8>> = lines.iter().map(|line| line.as_bytes().to_vec()).collect();
    write_sealed_shard_bytes_with_cap(stage_root, machine, session_id, &bytes, bucket_cap)
}

/// Append a batch of arbitrary UTF-8-independent line bytes as one sealed
/// shard. Each item is written followed by exactly one newline. The final
/// shard is installed atomically, so a crash cannot leave a file that the
/// next `push` mistakes for a complete sealed shard.
pub fn write_sealed_shard_bytes_with_cap(
    stage_root: &Path,
    machine: &str,
    session_id: &str,
    lines: &[Vec<u8>],
    bucket_cap: usize,
) -> anyhow::Result<String> {
    let dir = session_shard_dir(stage_root, machine, session_id);
    fs::create_dir_all(&dir)?;
    let seq = next_shard_seq(stage_root, machine, session_id);
    let path = shard_path_with_cap(stage_root, machine, session_id, seq, bucket_cap);
    fs::create_dir_all(path.parent().expect("shard path has bucket parent"))?;
    let tmp = path.with_file_name(format!(".{}tmp", shard_filename(seq)));
    if tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    let mut f = fs::File::create(&tmp)?;
    for line in lines {
        f.write_all(line)?;
        f.write_all(b"\n")?;
    }
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

/// Write the masterkey to `cfg.key_file`.
pub fn persist_key_file(cfg: &StoreConfig, mk: &MasterKey) -> anyhow::Result<()> {
    if let Some(parent) = cfg.key_file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&cfg.key_file, serialize_key(mk)?)?;
    Ok(())
}

/// Load the masterkey from `cfg.key_file` (error when missing).
pub fn load_key_file(cfg: &StoreConfig) -> anyhow::Result<MasterKey> {
    let raw = fs::read_to_string(&cfg.key_file).with_context(|| {
        format!(
            "cannot read masterkey file {} (lost key?)",
            cfg.key_file.display()
        )
    })?;
    parse_key(&raw)
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
