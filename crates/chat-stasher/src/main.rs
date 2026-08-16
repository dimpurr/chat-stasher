//! chat-stasher CLI entry point.

use chat_stasher::config::{self, Config};
use chat_stasher::reap;
use chat_stasher::scanner;
use chat_stasher::seal;
use chat_stasher::store::{self, BackupStore, StoreConfig};
use chat_stasher::verify::{CheckSummary, ReconcileReport, SessionOutcome};
use clap::{Parser, Subcommand};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "chat-stasher",
    version,
    about = "Append-only archive for every LLM conversation, across harnesses."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The five subcommands. `push`/`read` are backed by the BackupStore
/// (rustic_core); `doctor` answers one question — is a harness silently
/// deleting your history?
#[derive(Subcommand)]
enum Command {
    /// Write a commented default config if none exists (non-destructive).
    Init,
    /// Move a batch of sealed session shards into the rustic repository.
    ///
    /// The stage directory is expected to already hold only *sealed* shards at
    /// `sessions/<machine>/<session>/{NNNNNN}.jsonl`. Creates the repository
    /// on first run and persists the masterkey.
    Push {
        /// Stage directory holding the sealed shard tree.
        #[arg(long)]
        stage: PathBuf,
        /// Repository path override (default: config `rustic_repo` / data dir).
        #[arg(long)]
        repo: Option<String>,
        /// Masterkey file override (default: config `rustic_key_file` / data dir).
        #[arg(long)]
        key_file: Option<String>,
        /// Machine name for the path partition + snapshot host.
        /// Default: this machine's normalised hostname.
        #[arg(long)]
        machine: Option<String>,
        /// Concurrency cap override (default: config `rustic_connections` = 10).
        #[arg(long)]
        connections: Option<usize>,
        /// Backend option `key=value`, repeatable (e.g. `--option endpoint=ssh://host:23`).
        #[arg(long = "option")]
        options: Vec<String>,
        /// Disable ssh connection reaping after this run (for troubleshooting).
        #[arg(long)]
        no_reap: bool,
    },
    /// Report what the local harness scanner finds (read-only).
    Status,
    /// Dump one session back from the repository (sequence-concatenated) and
    /// print its sha256 for verification — or, with `--all-machines`, merge
    /// the newest snapshot of every machine and report per-session digests.
    Read {
        /// Stage directory used by push (mapping into the snapshot tree).
        /// Ignored by `--all-machines`.
        #[arg(long)]
        stage: Option<PathBuf>,
        /// Native session id to dump, e.g. `019bf00d-...`. Ignored by
        /// `--all-machines`.
        #[arg(long)]
        session: Option<String>,
        /// Cross-machine merge: every hostname's newest snapshot, all sessions
        /// (`sessions/<machine>/…`), each session's shards sequence-joined and
        /// hashed. Prints ids / shard counts / byte lengths / sha256 only.
        #[arg(long)]
        all_machines: bool,
        /// Machine partition to read. Defaults to this machine's id
        /// (unused by `--all-machines`, which reads every machine).
        #[arg(long)]
        machine: Option<String>,
        /// Repository path override.
        #[arg(long)]
        repo: Option<String>,
        /// Masterkey file override.
        #[arg(long)]
        key_file: Option<String>,
        /// Concurrency cap override.
        #[arg(long)]
        connections: Option<usize>,
        /// Backend option `key=value`, repeatable.
        #[arg(long = "option")]
        options: Vec<String>,
        /// Disable ssh connection reaping after this run (for troubleshooting).
        #[arg(long)]
        no_reap: bool,
    },
    /// Diagnostic: does any harness on this machine silently delete its
    /// sessions? Read-only (paths/counts/bytes/timestamps only).
    Doctor,
    /// Prove the archive is intact. Three independently runnable levels:
    /// l1 = rustic structure check (cheap, no payload reads), l2 = rustic content
    /// check (downloads and re-hashes every pack), l3 = reconcile against an
    /// expected manifest derived from the sealed staging tree (per session:
    /// shard count / concatenated bytes / concatenated sha256).
    Verify {
        /// Which level(s) to run: `l1`, `l2`, `l3` or `all`.
        #[arg(long, default_value = "all")]
        level: VerifyLevel,
        /// Stage directory holding the sealed shard tree (required by l3 / all).
        #[arg(long)]
        stage: Option<PathBuf>,
        /// Machine partition (default: this machine's normalised hostname).
        #[arg(long)]
        machine: Option<String>,
        /// Repository path override.
        #[arg(long)]
        repo: Option<String>,
        /// Masterkey file override.
        #[arg(long)]
        key_file: Option<String>,
        /// Concurrency cap override.
        #[arg(long)]
        connections: Option<usize>,
        /// Backend option `key=value`, repeatable.
        #[arg(long = "option")]
        options: Vec<String>,
        /// Disable ssh connection reaping after this run (for troubleshooting).
        #[arg(long)]
        no_reap: bool,
    },
    /// Consume ext inbox bundles into sealed staging shards.
    ///
    /// Reads complete `deepseek-<sessionId>.json` exports from `--inbox`
    /// (skipping `.part` files), archives each as one record in a sealed
    /// shard under `<stage>/sessions/<machine>/<id>/<bucket>/NNNNNN.jsonl`, and retires
    /// the source file to `<inbox>/consumed/`. Idempotent by content: the same
    /// bytes are never archived twice. Prints paths/counts/sha256 only.
    Ingest {
        /// Inbox directory holding the ext exports.
        #[arg(long)]
        inbox: PathBuf,
        /// Stage directory that holds the sealed `sessions/` tree.
        #[arg(long)]
        stage: PathBuf,
        /// Machine partition for `sessions/<machine>/…`.
        /// Default: this machine's normalised hostname.
        #[arg(long)]
        machine: Option<String>,
        /// Maximum sealed shards per bucket (default: 20).
        #[arg(long, default_value_t = store::DEFAULT_SHARD_BUCKET_CAP)]
        shard_bucket_cap: usize,
    },
    /// Seal one active (live) file by renaming it into the next sealed-shard
    /// slot, leaving the original path free as the new tail.
    ///
    /// Gated by `data/harness-registry-v1.json` (`seal_policy` + `seal_source`
    /// + the platform cell's `confidence`): only a harness whose policy is
    /// `rename`, with an evidence `seal_source` line **and** a `源码确认`
    /// platform cell may be renamed. Everything else (Codex = fd-holder,
    /// opencode = sqlite, any unconfirmed harness) is refused with the active
    /// file untouched — renaming an fd-holder silently drops its post-rename
    /// data.
    Seal {
        /// Registry harness id that owns the active file (e.g. `claude-code`).
        #[arg(long)]
        harness: String,
        /// Path of the live (active) file to seal. After the rename this path
        /// is free: a reopen-by-path harness starts its new tail there.
        #[arg(long)]
        active: PathBuf,
        /// Stage directory that holds the sealed `sessions/` tree.
        #[arg(long)]
        stage: PathBuf,
        /// Machine partition for `sessions/<machine>/…`.
        /// Default: this machine's normalised hostname.
        #[arg(long)]
        machine: Option<String>,
        /// Session id to seal into. Default: the active file's stem.
        #[arg(long)]
        session: Option<String>,
        /// Maximum sealed shards per bucket (default: 20).
        #[arg(long, default_value_t = store::DEFAULT_SHARD_BUCKET_CAP)]
        shard_bucket_cap: usize,
    },
}

/// verify `--level` selector.
#[derive(Clone, Copy, clap::ValueEnum)]
enum VerifyLevel {
    L1,
    L2,
    L3,
    All,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => cmd_init(),
        Command::Push {
            stage,
            repo,
            key_file,
            machine,
            connections,
            options,
            no_reap,
        } => cmd_push(
            &stage,
            repo,
            key_file,
            machine,
            connections,
            &options,
            no_reap,
        ),
        Command::Status => cmd_status(),
        Command::Read {
            stage,
            session,
            all_machines,
            machine,
            repo,
            key_file,
            connections,
            options,
            no_reap,
        } => cmd_read(
            &stage,
            &session,
            all_machines,
            machine.as_deref(),
            repo,
            key_file,
            connections,
            &options,
            no_reap,
        ),
        Command::Doctor => cmd_doctor(),
        Command::Verify {
            level,
            stage,
            machine,
            repo,
            key_file,
            connections,
            options,
            no_reap,
        } => cmd_verify(
            level,
            &stage,
            machine.as_deref(),
            repo,
            key_file,
            connections,
            &options,
            no_reap,
        ),
        Command::Ingest {
            inbox,
            stage,
            machine,
            shard_bucket_cap,
        } => cmd_ingest(&inbox, &stage, machine.as_deref(), shard_bucket_cap),
        Command::Seal {
            harness,
            active,
            stage,
            machine,
            session,
            shard_bucket_cap,
        } => cmd_seal(
            &harness,
            &active,
            &stage,
            machine.as_deref(),
            session.as_deref(),
            shard_bucket_cap,
        ),
    }
}

fn cmd_doctor() -> ExitCode {
    let report = chat_stasher::doctor::run();
    chat_stasher::doctor::print_report(&report);
    ExitCode::SUCCESS
}

fn cmd_ingest(
    inbox: &PathBuf,
    stage: &PathBuf,
    machine: Option<&str>,
    shard_bucket_cap: usize,
) -> ExitCode {
    let machine = machine
        .map(String::from)
        .unwrap_or_else(chat_stasher::id::machine_id);
    let report =
        match chat_stasher::inbox::ingest_with_cap(inbox, stage, &machine, shard_bucket_cap) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("ingest: {e:#}");
                return ExitCode::FAILURE;
            }
        };
    println!("[ingest] shard bucket cap : {shard_bucket_cap}");
    print_ingest(&report, &machine);
    ExitCode::SUCCESS
}

/// `seal` — allowlist-checked rename-sealing of one active file.
///
/// The registry (`data/harness-registry-v1.json`) is the single decision
/// source: `seal_policy` (`rename` / `no-rename` / `not-applicable`),
/// mandatory `seal_source` evidence, and the platform cell's `confidence`
/// must all clear the gate. On refusal the active file is left untouched and
/// the command exits non-zero (sealing was requested but is not permitted).
fn cmd_seal(
    harness_id: &str,
    active: &Path,
    stage: &Path,
    machine: Option<&str>,
    session: Option<&str>,
    shard_bucket_cap: usize,
) -> ExitCode {
    let registry = match scanner::load_registry_from_repo() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("seal: {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(h) = seal::harness_by_id(&registry, harness_id) else {
        eprintln!("seal: harness `{harness_id}` unknown to the registry");
        return ExitCode::FAILURE;
    };
    let policy = seal::SealPolicy::classify(&h.seal_policy);
    println!("[seal] harness        : {} ({})", h.id, h.display_name);
    println!(
        "[seal] policy         : {} (raw `{}`)",
        policy.label(),
        h.seal_policy
    );
    let Some(cell) = h.paths.cell_for(scanner::current_platform()) else {
        println!(
            "[seal] REFUSED: no `{}` registry cell -> no rename (default)",
            scanner::current_platform()
        );
        return ExitCode::FAILURE;
    };
    if !seal::seal_allowed(h, cell) {
        let reason = if policy != seal::SealPolicy::Rename {
            "registry seal_policy is not `rename`".to_string()
        } else if h.seal_source.trim().is_empty() {
            "seal_source is empty (rename needs a measured/source evidence line)".to_string()
        } else if cell.source.trim().is_empty() {
            "platform cell source is empty".to_string()
        } else {
            "platform cell confidence is not 源码确认".to_string()
        };
        println!("[seal] REFUSED: {reason}");
        println!("[seal] active untouched : {}", active.display());
        return ExitCode::FAILURE;
    }
    let machine = machine
        .map(String::from)
        .unwrap_or_else(chat_stasher::id::machine_id);
    let session = session
        .map(String::from)
        .or_else(|| active.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "unknown".to_string());
    println!("[seal] machine        : {machine}");
    println!("[seal] session        : {session}");
    println!("[seal] bucket cap     : {shard_bucket_cap}");
    match seal::seal_active_file(active, stage, &machine, &session, shard_bucket_cap) {
        Ok(seq) => {
            println!(
                "[seal] sealed          : {} -> {}/{} (seq {seq})",
                active.display(),
                store::shard_bucket_name(seq, shard_bucket_cap),
                store::shard_filename(seq),
            );
            println!(
                "[seal] original path now free: a reopen-by-path harness starts its new tail there"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("seal: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Metadata-only ingest summary: counts, paths, shard names, bytes, sha256.
/// Never prints any session content.
fn print_ingest(report: &chat_stasher::inbox::IngestReport, machine: &str) {
    println!(
        "[ingest] inbox            : candidates={} skipped_{}={}",
        report.total_inbox_files, "part", report.part_files_seen
    );
    println!("[ingest] consumed         : {}", report.consumed.len());
    for c in &report.consumed {
        println!(
            "  + {}  kind={}  bytes={}  sha256={}  -> {}  ({})",
            c.source_file, c.kind, c.file_bytes, c.file_sha256, c.shard, c.id,
        );
    }
    println!("[ingest] duplicates (same bytes already archived, not re-sealed):");
    for d in &report.duplicates {
        println!(
            "  = {}  sha256={}  matched in {}  (id {})",
            d.source_file, d.file_sha256, d.matched_shard, d.id,
        );
    }
    if !report.errors.is_empty() {
        println!("[ingest] errors           : {}", report.errors.len());
        for e in &report.errors {
            println!("  ! {}  {}", e.source_file, e.message);
        }
    }
    println!("[ingest] staging machine   : {machine}");
    println!(
        "[ingest] note             : bundles carry NO account field - the identity axis is missing, \
waiting for the adapter to supply it; ids are `platform.sessionId` (machine-independent)"
    );
}

/// Local data dir used for the default repository + key file.
fn data_root() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("chat-stasher");
    }
    config::home_dir()
        .join(".local")
        .join("share")
        .join("chat-stasher")
}

fn store_config_from(
    config: &Config,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
) -> StoreConfig {
    let data_root = data_root();
    let options = options
        .iter()
        .map(|kv| {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| format!("option must be key=value, got `{kv}`"))?;
            Ok((k.to_string(), v.to_string()))
        })
        .collect::<Result<BTreeMap<String, String>, String>>();
    match options {
        Ok(options) => StoreConfig {
            repo_root: repo
                .or_else(|| config.rustic_repo.clone())
                .unwrap_or_else(|| data_root.join("repo").to_string_lossy().into_owned()),
            key_file: key_file
                .map(PathBuf::from)
                .or_else(|| config.rustic_key_file.as_deref().map(PathBuf::from))
                .unwrap_or_else(|| data_root.join("masterkey.json")),
            connections: 0,
            options,
        }
        .with_capped_connections(connections.or(config.rustic_connections)),
        Err(e) => {
            eprintln!("option: {e}");
            std::process::exit(2);
        }
    }
}

/// Load persisted masterkey or create+persist a fresh one (repo init path).
fn masterkey(config: &StoreConfig) -> (MasterKey, bool) {
    match store::load_key_file(config) {
        Ok(mk) => (mk, false),
        Err(_) => {
            let mk = MasterKey::new();
            match store::persist_key_file(config, &mk) {
                Ok(()) => (mk, true),
                Err(e) => {
                    eprintln!("warning: could not persist masterkey: {e}");
                    (mk, true)
                }
            }
        }
    }
}

/// Reap the ssh ControlPersist masters left behind for the backend `endpoint`
/// host of this run. No-op when `--no-reap` is set or no `endpoint` option was
/// given (a local repo has no ssh masters to reap).
fn reap_remote(cfg: &StoreConfig, no_reap: bool) {
    if no_reap {
        println!("[reap] skipped (--no-reap)");
        return;
    }
    let Some(endpoint) = cfg.options.get("endpoint") else {
        return;
    };
    let Some(host) = reap::host_of_endpoint(endpoint) else {
        eprintln!("[reap] cannot parse endpoint `{endpoint}`, nothing reaped");
        return;
    };
    let n = reap::reap_masters_for_host(&host);
    println!("[reap] host {host} · ssh masters shut down: {n}");
}

fn cmd_push(
    stage: &PathBuf,
    repo: Option<String>,
    key_file: Option<String>,
    machine: Option<String>,
    connections: Option<usize>,
    options: &[String],
    no_reap: bool,
) -> ExitCode {
    let config = Config::load();
    let machine = machine.unwrap_or_else(chat_stasher::id::machine_id);
    let cfg = store_config_from(&config, repo, key_file, connections, options);
    let (mk, key_was_new) = masterkey(&cfg);
    let store = BackupStore::new(cfg.clone(), machine.clone());
    println!("[push] machine        : {machine}");
    println!("[push] repo           : {}", store.cfg.repo_root);
    println!("[push] key file       : {}", store.cfg.key_file.display());
    println!(
        "[push] connections    : {} (cap {})",
        store.cfg.connections,
        store::DEFAULT_CONNECTIONS
    );
    println!(
        "[push] reap           : {}",
        if no_reap { "OFF (--no-reap)" } else { "ON" }
    );
    let summary = match store.push(stage, &mk) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("push: {e:#}");
            reap_remote(&cfg, no_reap);
            return ExitCode::FAILURE;
        }
    };
    let was_init = summary.repo_was_init;
    println!(
        "[push] {} {}",
        if was_init {
            "INIT (new repository created)"
        } else {
            "OPEN (existing repository)"
        },
        if key_was_new {
            "· masterkey created+persisted"
        } else {
            "· masterkey loaded"
        },
    );
    println!(
        "[push] summary: files_new={} files_changed={} files_unmodified={} data_blobs={} data_added={} data_added_packed={}",
        summary.files_new,
        summary.files_changed,
        summary.files_unmodified,
        summary.data_blobs,
        summary.data_added,
        summary.data_added_packed,
    );
    println!(
        "[push] snapshot host  : {} (must equal machine)",
        summary.snapshot_host
    );
    println!("[push] snapshots      : {}", summary.snapshots_in_repo);
    reap_remote(&cfg, no_reap);
    ExitCode::SUCCESS
}

fn cmd_read(
    stage: &Option<PathBuf>,
    session: &Option<String>,
    all_machines: bool,
    machine: Option<&str>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    no_reap: bool,
) -> ExitCode {
    let config = Config::load();
    let machine = machine
        .map(String::from)
        .unwrap_or_else(chat_stasher::id::machine_id);
    let cfg = store_config_from(&config, repo, key_file, connections, options);
    let store = BackupStore::new(cfg.clone(), machine.clone());
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            eprintln!("read: {e}");
            reap_remote(&cfg, no_reap);
            return ExitCode::FAILURE;
        }
    };

    let code = if all_machines {
        cmd_read_all_machines(&store, &mk)
    } else {
        let stage = match stage {
            Some(s) => s,
            None => {
                eprintln!("read: `--stage` is required unless `--all-machines` is set");
                reap_remote(&cfg, no_reap);
                return ExitCode::FAILURE;
            }
        };
        let session = match session {
            Some(s) => s,
            None => {
                eprintln!("read: `--session` is required unless `--all-machines` is set");
                reap_remote(&cfg, no_reap);
                return ExitCode::FAILURE;
            }
        };
        println!("[read] machine        : {machine}");
        println!("[read] repo           : {}", store.cfg.repo_root);
        println!(
            "[read] reap           : {}",
            if no_reap { "OFF (--no-reap)" } else { "ON" }
        );
        let (bytez, hashes) = match store.read_session_readback(stage, session, &mk) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("read: {e}");
                reap_remote(&cfg, no_reap);
                return ExitCode::FAILURE;
            }
        };
        println!("[read] shards (seq order):");
        for (name, hash) in &hashes {
            println!("  {name}  sha256={hash}");
        }
        println!(
            "[read] concat len      : {}  sha256={}",
            bytez.len(),
            sha256_hex(&bytez)
        );
        println!(
            "[read] expected src    : sha256={}",
            store::expected_concat_sha(stage, &machine, session)
                .unwrap_or_else(|e| format!("<ein> {e}"))
        );
        ExitCode::SUCCESS
    };
    reap_remote(&cfg, no_reap);
    code
}

/// `read --all-machines` — group every snapshot by hostname, take each
/// hostname's newest snapshot, walk its `sessions/<machine>/` subtree, and
/// report per-session shard count / byte length / sha256. Privacy line: only
/// ids, counts, lengths and digests are printed — never session content.
fn cmd_read_all_machines(store: &BackupStore, mk: &MasterKey) -> ExitCode {
    println!("[read] mode           : all-machines (newest snapshot per hostname)");
    println!("[read] repo           : {}", store.cfg.repo_root);
    let report = match store.read_all_machines(mk) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("read: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "[read] snapshots read : {} (get_all_snapshots lists every snapshot file)",
        report.snapshots_in_repo
    );
    println!("[read] machines       : {}", report.machines.len());
    for m in &report.machines {
        println!(
            "  machine {:<16} snapshot={}  time={}  unix={}  sessions={}",
            m.hostname,
            m.snapshot_id,
            m.snapshot_time,
            m.snapshot_time_unix,
            m.sessions.len(),
        );
        for s in &m.sessions {
            println!(
                "    session {:<24} shards={:<3} bytes={:<10} sha256={}",
                s.session_id, s.shard_count, s.concat_bytes, s.sha256
            );
        }
    }
    for w in &report.warnings {
        println!("  WARN: {w}");
    }
    ExitCode::SUCCESS
}

/// `verify` — prove the archive is intact, level by level. Each level prints
/// its own verdict; the exit code is FAILURE if any requested level failed.
fn cmd_verify(
    level: VerifyLevel,
    stage: &Option<PathBuf>,
    machine: Option<&str>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    no_reap: bool,
) -> ExitCode {
    let config = Config::load();
    let machine = machine
        .map(String::from)
        .unwrap_or_else(chat_stasher::id::machine_id);
    let cfg = store_config_from(&config, repo, key_file, connections, options);
    let store = BackupStore::new(cfg.clone(), machine.clone());
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            eprintln!("verify: {e}");
            reap_remote(&cfg, no_reap);
            return ExitCode::FAILURE;
        }
    };
    let need_stage = matches!(level, VerifyLevel::L3 | VerifyLevel::All);
    let stage = match (need_stage, stage) {
        (true, Some(s)) => s.clone(),
        (true, None) => {
            eprintln!("verify: `--stage` is required for level l3 / all");
            reap_remote(&cfg, no_reap);
            return ExitCode::FAILURE;
        }
        (false, _) => PathBuf::from("."),
    };

    println!("[verify] repo           : {}", store.cfg.repo_root);
    println!("[verify] machine        : {machine}");
    println!(
        "[verify] reap           : {}",
        if no_reap { "OFF (--no-reap)" } else { "ON" }
    );

    let mut failed = 0usize;
    match level {
        VerifyLevel::L1 => run_check(&store, &mk, false, "L1 structure", &mut failed),
        VerifyLevel::L2 => run_check(&store, &mk, true, "L2 content", &mut failed),
        VerifyLevel::L3 => {
            println!("[verify] stage          : {}", stage.display());
            run_reconcile(&store, &mk, &stage, &mut failed);
        }
        VerifyLevel::All => {
            run_check(&store, &mk, false, "L1 structure", &mut failed);
            run_check(&store, &mk, true, "L2 content", &mut failed);
            println!("[verify] stage          : {}", stage.display());
            run_reconcile(&store, &mk, &stage, &mut failed);
        }
    }

    reap_remote(&cfg, no_reap);
    if failed == 0 {
        println!("[verify] RESULT         : OK");
        ExitCode::SUCCESS
    } else {
        println!("[verify] RESULT         : FAILED ({failed} level(s) reported failures)");
        ExitCode::FAILURE
    }
}

fn run_check(store: &BackupStore, mk: &MasterKey, data: bool, name: &str, failed: &mut usize) {
    match store.check_repo(mk, data) {
        Ok(summary) => {
            print_check_summary(&summary, name);
            if !summary.ok() {
                *failed += 1;
            }
        }
        Err(e) => {
            eprintln!("verify: {name} failed to run: {e:#}");
            *failed += 1;
        }
    }
}

fn print_check_summary(s: &CheckSummary, name: &str) {
    let kind = if s.read_data {
        "read_data=true"
    } else {
        "read_data=false"
    };
    println!(
        "[verify] {name:<16} ok={:<5} findings={:<3} errors={:<3} warns={:<3} ({kind}) took {:?}",
        s.ok(),
        s.findings,
        s.errors,
        s.warns,
        s.duration
    );
    for detail in &s.details {
        println!("  ! {detail}");
    }
}

fn run_reconcile(store: &BackupStore, mk: &MasterKey, stage: &Path, failed: &mut usize) {
    match store.reconcile_manifest(mk, stage) {
        Ok(report) => {
            print_reconcile(&report);
            if !report.ok() {
                *failed += 1;
            }
        }
        Err(e) => {
            eprintln!("verify: L3 reconcile failed to run: {e:#}");
            *failed += 1;
        }
    }
}

fn print_reconcile(r: &ReconcileReport) {
    println!(
        "[verify] L3 reconcile     : machines={} expected={} took {:?}",
        r.machines,
        r.rows.len(),
        r.duration
    );
    for row in &r.rows {
        let mark = if row.outcome == SessionOutcome::Match {
            "ok "
        } else {
            "!! "
        };
        match &row.outcome {
            SessionOutcome::Match => println!(
                "  {mark} {:<12} {:<20} shards={:<2} bytes={:<10} sha={}",
                row.machine, row.session_id, row.observed_shards, row.observed_bytes, row.observed_sha
            ),
            SessionOutcome::MissingInArchive => println!(
                "  {mark} {:<12} {:<20} MISSING IN ARCHIVE",
                row.machine, row.session_id
            ),
            SessionOutcome::ShardCountMismatch { expected, observed } => println!(
                "  {mark} {:<12} {:<20} SHARD COUNT expected={expected} observed={observed}",
                row.machine, row.session_id
            ),
            SessionOutcome::ByteLengthMismatch { expected, observed } => println!(
                "  {mark} {:<12} {:<20} BYTE LENGTH expected={expected} observed={observed}",
                row.machine, row.session_id
            ),
            SessionOutcome::ShaMismatch { expected, observed } => println!(
                "  {mark} {:<12} {:<20} SHA MISMATCH\n      expected={expected}\n      observed={observed}",
                row.machine, row.session_id
            ),
        }
    }
    for (m, s) in &r.extra_in_archive {
        println!("  !? {m:<12} {s:<20} in archive but NOT in expected manifest (informational)");
    }
    println!(
        "[verify] L3 verdict       : {}",
        if r.ok() { "OK" } else { "FAILED" }
    );
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let out = Sha256::digest(bytes);
    hex_digest(&out)
}

fn cmd_init() -> ExitCode {
    match Config::init_default(config::DEFAULT_CONFIG_TEMPLATE) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("init: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_status() -> ExitCode {
    let config = Config::load();
    let report = match scanner::scan(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("status: scan failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    print_status(&report);
    ExitCode::SUCCESS
}

/// Human-readable summary + one metadata line per record.
///
/// Only ids, paths, sizes, mtimes and flags ever reach stdout — never the
/// content of a session.
fn print_status(report: &scanner::ScanReport) {
    // One line per source actually found (registry-driven, so any harness
    // id from data/harness-registry-v1.json can appear here).
    let mut per_source: std::collections::BTreeMap<&str, usize> = Default::default();
    for rec in &report.records {
        *per_source.entry(rec.source.short()).or_default() += 1;
    }
    let compressed = report.records.iter().filter(|r| r.compressed).count();

    println!();
    for (src, n) in &per_source {
        println!("  {src:<22} sessions : {n}");
    }
    println!(
        "  total                : {}  ({} compressed)",
        report.records.len(),
        compressed
    );
    for miss in &report.missing_roots {
        println!("  (missing root, skipped: {})", miss.display());
    }
    println!();

    if report.records.is_empty() {
        println!("  no sessions found.");
        return;
    }

    println!(
        "  {:<14} {:>12} {:>14}  {:<3}  {}",
        "source", "bytes", "mtime(sec)", "zst", "id"
    );
    for rec in &report.records {
        let secs = rec
            .mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        println!(
            "  {:<14} {:>12} {:>14}  {:<3}  {}",
            rec.source.short(),
            rec.byte_size,
            secs,
            if rec.compressed { "zst" } else { "   " },
            rec.id,
        );
    }
    println!();
}
