//! chat-stasher CLI entry point.

use clap::{Parser, Subcommand};
use chat_stasher::config::{self, Config};
use chat_stasher::models::HarnessSource;
use chat_stasher::reap;
use chat_stasher::scanner;
use chat_stasher::store::{self, BackupStore, StoreConfig};
use rustic_core::repofile::MasterKey;
use std::collections::BTreeMap;
use std::path::PathBuf;
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
    /// Consume ext inbox bundles into sealed staging shards.
    ///
    /// Reads complete `deepseek-<sessionId>.json` exports from `--inbox`
    /// (skipping `.part` files), archives each as one record in a sealed
    /// shard under `<stage>/sessions/<machine>/<id>/NNNNNN.jsonl`, and retires
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
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => cmd_init(),
        Command::Push { stage, repo, key_file, machine, connections, options, no_reap } => {
            cmd_push(&stage, repo, key_file, machine, connections, &options, no_reap)
        }
        Command::Status => cmd_status(),
        Command::Read { stage, session, all_machines, machine, repo, key_file, connections, options, no_reap } => {
            cmd_read(
                &stage,
                &session,
                all_machines,
                machine.as_deref(),
                repo,
                key_file,
                connections,
                &options,
                no_reap,
            )
        }
        Command::Doctor => cmd_doctor(),
        Command::Ingest { inbox, stage, machine } => cmd_ingest(&inbox, &stage, machine.as_deref()),
    }
}

fn cmd_doctor() -> ExitCode {
    let report = chat_stasher::doctor::run();
    chat_stasher::doctor::print_report(&report);
    ExitCode::SUCCESS
}

fn cmd_ingest(inbox: &PathBuf, stage: &PathBuf, machine: Option<&str>) -> ExitCode {
    let machine = machine.map(String::from).unwrap_or_else(chat_stasher::id::machine_id);
    let report = match chat_stasher::inbox::ingest(inbox, stage, &machine) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ingest: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    print_ingest(&report, &machine);
    ExitCode::SUCCESS
}

/// Metadata-only ingest summary: counts, paths, shard names, bytes, sha256.
/// Never prints any session content.
fn print_ingest(report: &chat_stasher::inbox::IngestReport, machine: &str) {
    println!("[ingest] inbox            : candidates={} skipped_{}={}",
        report.total_inbox_files, "part", report.part_files_seen);
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
    config::home_dir().join(".local").join("share").join("chat-stasher")
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
    println!("[push] connections    : {} (cap {})", store.cfg.connections, store::DEFAULT_CONNECTIONS);
    println!("[push] reap           : {}", if no_reap { "OFF (--no-reap)" } else { "ON" });
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
        if was_init { "INIT (new repository created)" } else { "OPEN (existing repository)" },
        if key_was_new { "· masterkey created+persisted" } else { "· masterkey loaded" },
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
    println!("[push] snapshot host  : {} (must equal machine)", summary.snapshot_host);
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
    let machine = machine.map(String::from).unwrap_or_else(chat_stasher::id::machine_id);
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
        println!("[read] reap           : {}", if no_reap { "OFF (--no-reap)" } else { "ON" });
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
            store::expected_concat_sha(stage, &machine, session).unwrap_or_else(|e| format!("<ein> {e}"))
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
    let (claude, codex) = report
        .records
        .iter()
        .fold((0usize, 0usize), |(c, x), r| match r.source {
            HarnessSource::ClaudeCode => (c + 1, x),
            HarnessSource::Codex => (c, x + 1),
        });
    let compressed = report.records.iter().filter(|r| r.compressed).count();

    println!();
    println!("  claude-code sessions : {claude}");
    println!("  codex sessions       : {codex}");
    println!("  total                : {}  ({} compressed)", report.records.len(), compressed);
    for miss in &report.missing_roots {
        println!("  (missing root, skipped: {})", miss.display());
    }
    println!();

    if report.records.is_empty() {
        println!("  no sessions found.");
        return;
    }

    println!("  {:<14} {:>12} {:>14}  {:<3}  {}", "source", "bytes", "mtime(sec)", "zst", "id");
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