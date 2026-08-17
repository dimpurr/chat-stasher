//! chat-stasher CLI entry point.

use chat_stasher::config::{self, Config};
use chat_stasher::destinit::SourceStatus;
use chat_stasher::reap;
use chat_stasher::scanner;
use chat_stasher::schedule;
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
    /// Collect one pass, push only when configured and changed, then exit.
    ///
    /// Normal outcomes return 0: `result: NOOP` means no snapshot was created,
    /// while `result: COMPLETED` means a snapshot was created. Non-zero means
    /// a real error. The command is safe to invoke again.
    RunOnce {
        /// Stage directory that holds the sealed session shard tree.
        #[arg(long)]
        stage: PathBuf,
        /// Machine partition for the stage and snapshot host.
        #[arg(long)]
        machine: Option<String>,
        /// Maximum sealed shards per bucket (default: 20).
        #[arg(long, default_value_t = store::DEFAULT_SHARD_BUCKET_CAP)]
        shard_bucket_cap: usize,
        /// Named destination from the config. Required once the config
        /// declares any destination; there is no default.
        #[arg(long)]
        destination: Option<String>,
        /// Repository path override.
        #[arg(long)]
        repo: Option<String>,
        /// Masterkey file override.
        #[arg(long)]
        key_file: Option<String>,
        /// Concurrency cap override.
        #[arg(long)]
        connections: Option<usize>,
        /// Backend option key=value, repeatable.
        #[arg(long = "option")]
        options: Vec<String>,
        /// Do the cheap repository structure check (L1) after the cycle.
        #[arg(long)]
        verify: bool,
        /// Disable ssh connection reaping after this run.
        #[arg(long)]
        no_reap: bool,
    },
    /// Render a launchd plist or systemd user service/timer; never installs it.
    Schedule {
        /// Template format to render.
        #[arg(long, value_enum, default_value = "launchd")]
        format: schedule::Format,
        /// Stage path embedded in the one-shot command.
        #[arg(long)]
        stage: PathBuf,
        /// Write to this plist path, or systemd directory. Without it, print.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Binary path embedded in the template (defaults to this executable).
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Add the cheap L1 verify pass after each archive cycle.
        #[arg(long)]
        verify: bool,
    },
    /// Move a batch of sealed session shards into the rustic repository.
    ///
    /// The stage directory is expected to already hold only *sealed* shards at
    /// `sessions/<machine>/<session>/{NNNNNN}.jsonl`. Creates the repository
    /// on first run and persists the masterkey.
    Push {
        /// Stage directory holding the sealed shard tree.
        #[arg(long)]
        stage: PathBuf,
        /// Inbox whose consumed/ directory must be accounted for when stage
        /// is empty. If omitted, the most recent CLI ingest inboxes are used.
        #[arg(long)]
        inbox: Option<PathBuf>,
        /// Named destination from the config. Required once the config
        /// declares any destination; there is no default.
        #[arg(long)]
        destination: Option<String>,
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
    /// Is the scheduled archive actually working? Plus what the local harness
    /// scanner finds (read-only).
    ///
    /// The first line answers the question the timer cannot: it reads the
    /// `run-state.json` written by the last `run-once` pass. Three distinct
    /// answers, and only the first is a healthy one:
    ///
    /// * last run recent and successful -> exit 0
    /// * last run FAILED -> exit 1, saying which step failed
    /// * no run recorded at all, or nothing has run for longer than
    ///   4x `backup_interval_secs` (minimum 1 hour) -> exit 1. A dead timer
    ///   usually leaves a *successful* last run behind, so this overdue check
    ///   is the only thing that catches it.
    ///
    /// Never prints session content: counts, timestamps and digests only.
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
        /// Named destination from the config. Required once the config
        /// declares any destination; there is no default.
        #[arg(long)]
        destination: Option<String>,
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
        /// Named destination from the config. Required once the config
        /// declares any destination; there is no default.
        #[arg(long)]
        destination: Option<String>,
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
    /// Initialise a new destination as a *full extra copy* (ADR-013).
    ///
    /// Order is fixed: re-collect from the local sources first (they are the
    /// truth, and rereading them puts no load on an existing destination),
    /// then copy back only what an existing destination holds and the local
    /// source no longer does, then push the result. The new destination ends
    /// up with `local ∪ existing destinations`.
    ///
    /// A source destination that cannot be consulted makes the difference set
    /// *incomplete*: that is reported and the command exits non-zero. It is
    /// never treated as "that destination had nothing extra".
    DestInit {
        /// Destination to initialise. Must be declared in the config, unless
        /// an explicit `--repo` is given instead.
        #[arg(long)]
        destination: Option<String>,
        /// Stage directory that holds the sealed `sessions/` tree.
        #[arg(long)]
        stage: PathBuf,
        /// Machine partition for `sessions/<machine>/…`.
        #[arg(long)]
        machine: Option<String>,
        /// Maximum sealed shards per bucket (default: 20).
        #[arg(long, default_value_t = store::DEFAULT_SHARD_BUCKET_CAP)]
        shard_bucket_cap: usize,
        /// Existing destination to compute the difference set against,
        /// repeatable. Default: every other destination in the config.
        #[arg(long = "from")]
        from: Vec<String>,
        /// Repository path override for the destination being initialised.
        #[arg(long)]
        repo: Option<String>,
        /// Masterkey file override for the destination being initialised.
        #[arg(long)]
        key_file: Option<String>,
        /// Concurrency cap override.
        #[arg(long)]
        connections: Option<usize>,
        /// Backend option `key=value`, repeatable.
        #[arg(long = "option")]
        options: Vec<String>,
        /// Disable ssh connection reaping after this run.
        #[arg(long)]
        no_reap: bool,
    },
    /// Search one destination's archive by session metadata.
    ///
    /// Metadata tier only: this walks snapshot/index/tree objects and never
    /// fetches or decrypts a data blob, which is why it is cheap. On a local
    /// three-session fixture the metadata walk read 11,761 bytes against
    /// 1,206,285 bytes of data packs — two orders of magnitude apart. Use
    /// `--cost` to see what a full-text pass over the current hits *would*
    /// cost before asking for one; full-text matching is not implemented.
    ///
    /// One destination per run, always named: there is no automatic merge
    /// across destinations, and no default destination to search "everything".
    ///
    /// Exit codes distinguish the three answers, because two of them look the
    /// same and mean opposite things: `0` matched something, `1` read the whole
    /// destination and nothing matched, `3` could not finish reading it — so
    /// "nothing matched" is unproven. `2` is a usage error, as elsewhere.
    Search {
        /// Destination to search. Required unless an explicit `--repo` is given.
        #[arg(long)]
        destination: Option<String>,
        /// Match sessions whose id starts with this prefix.
        #[arg(long)]
        session: Option<String>,
        /// Match one machine partition exactly.
        #[arg(long)]
        machine: Option<String>,
        /// Lower bound (inclusive) on session activity time, unix seconds.
        #[arg(long)]
        since_unix: Option<i64>,
        /// Upper bound (inclusive) on session activity time, unix seconds.
        #[arg(long)]
        until_unix: Option<i64>,
        /// Also report what a full-text pass over the hits would cost.
        #[arg(long)]
        cost: bool,
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
        /// Disable ssh connection reaping after this run.
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
    /// Read every scanner session returned by `status` into our own stage.
    ///
    /// Harness sources are opened read-only. File-backed JSONL sources use a
    /// durable byte offset plus committed-prefix SHA-256; opencode SQLite
    /// sessions use a durable logical high-water cursor. The cursor state lives
    /// under chat-stasher's own data directory, not under any harness directory.
    Collect {
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
        /// Named destination from the config. Read state is kept per
        /// destination, so a different destination is a different debt set.
        #[arg(long)]
        destination: Option<String>,
        /// Destination repository this pass collects *for*. Read state is kept
        /// per destination, so a different value is a different debt set.
        #[arg(long)]
        repo: Option<String>,
        /// Masterkey file override for the destination above.
        #[arg(long)]
        key_file: Option<String>,
    },
    /// Seal one file already inside our stage into the next sealed-shard slot.
    /// This command never renames a harness-owned path.
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
        /// Path of a file already inside --stage to seal. Paths outside the
        /// stage are rejected and left untouched.
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
        Command::RunOnce {
            stage,
            machine,
            shard_bucket_cap,
            destination,
            repo,
            key_file,
            connections,
            options,
            verify,
            no_reap,
        } => cmd_run_once(
            &stage,
            machine,
            shard_bucket_cap,
            destination,
            repo,
            key_file,
            connections,
            &options,
            verify,
            no_reap,
        ),
        Command::Schedule {
            format,
            stage,
            output,
            binary,
            verify,
        } => cmd_schedule(format, &stage, output, binary, verify),
        Command::Push {
            stage,
            inbox,
            destination,
            repo,
            key_file,
            machine,
            connections,
            options,
            no_reap,
        } => cmd_push(
            &stage,
            inbox,
            destination,
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
            destination,
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
            destination,
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
            destination,
            repo,
            key_file,
            connections,
            options,
            no_reap,
        } => cmd_verify(
            level,
            &stage,
            machine.as_deref(),
            destination,
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
        Command::Collect {
            stage,
            machine,
            shard_bucket_cap,
            destination,
            repo,
            key_file,
        } => cmd_collect(
            &stage,
            machine.as_deref(),
            shard_bucket_cap,
            destination,
            repo,
            key_file,
        ),
        Command::DestInit {
            destination,
            stage,
            machine,
            shard_bucket_cap,
            from,
            repo,
            key_file,
            connections,
            options,
            no_reap,
        } => cmd_dest_init(
            destination,
            &stage,
            machine.as_deref(),
            shard_bucket_cap,
            &from,
            repo,
            key_file,
            connections,
            &options,
            no_reap,
        ),
        Command::Search {
            destination,
            session,
            machine,
            since_unix,
            until_unix,
            cost,
            repo,
            key_file,
            connections,
            options,
            no_reap,
        } => cmd_search(
            destination,
            session,
            machine,
            since_unix,
            until_unix,
            cost,
            repo,
            key_file,
            connections,
            &options,
            no_reap,
        ),
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

/// `search` — metadata-tier query against exactly one named destination.
///
/// Three things this deliberately does not do. It does not pick a destination
/// for you: searching "everywhere" would have to merge answers from archives
/// that are not required to agree, so the destination is always named. It does
/// not read payload: the walk stays on snapshot/index/tree objects, and
/// `--cost` reports what payload *would* cost instead of quietly fetching it.
/// And it does not collapse "nothing is here" into "I could not look" — those
/// get different sentences and different exit codes, because a backup tool that
/// answers "not found" when it means "unreadable" is worse than one that fails.
///
/// Privacy line, same as `read`: ids, counts, byte lengths and times only —
/// never session content.
#[allow(clippy::too_many_arguments)]
fn cmd_search(
    destination: Option<String>,
    session: Option<String>,
    machine: Option<String>,
    since_unix: Option<i64>,
    until_unix: Option<i64>,
    cost: bool,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    no_reap: bool,
) -> ExitCode {
    let config = Config::load();
    if destination.is_none() && repo.is_none() {
        eprintln!(
            "search: name the destination to search (`--destination <name>`, or an explicit `--repo`)"
        );
        eprintln!(
            "search: there is no default destination and no cross-destination merge — archives are not required to agree"
        );
        return ExitCode::from(2);
    }
    let cfg = resolve_store_config(
        &config,
        destination.as_deref(),
        repo,
        key_file,
        connections,
        options,
    );
    // The machine filter is a *query* over `sessions/<machine>/`, not this
    // machine's identity: searching an archive for another machine's sessions
    // is the normal case, so this must not default to the local machine id.
    let store = BackupStore::new(cfg.clone(), chat_stasher::id::machine_id());
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            eprintln!("search: {e}");
            eprintln!("search: without the key nothing was read — this is not an empty result");
            reap_remote(&cfg, no_reap);
            // Deliberately 3, not 1. A missing key means the archive was never
            // consulted, which belongs with "could not finish reading", not
            // with "read it all and nothing matched". Reusing 1 here would make
            // a lost key indistinguishable from a genuine empty answer.
            return ExitCode::from(3);
        }
    };

    let mut filter = chat_stasher::search::SearchFilter::default();
    if let Some(p) = session {
        filter = filter.session_id_prefix(p);
    }
    if let Some(m) = machine {
        filter = filter.machine(m);
    }
    if let Some(t) = since_unix {
        filter = filter.since_unix(t);
    }
    if let Some(t) = until_unix {
        filter = filter.until_unix(t);
    }

    let report = match chat_stasher::search::search_sessions(&store, &mk, &filter) {
        Ok(r) => r,
        Err(e) => {
            // Could not open the repository at all. This is the case that must
            // never be rendered as "nothing matched".
            eprintln!("search: cannot read `{}`: {e}", cfg.repo_root);
            eprintln!("search: this is not an empty destination — the archive was not read");
            reap_remote(&cfg, no_reap);
            return ExitCode::from(3);
        }
    };

    println!("[search] destination  : {}", report.destination);
    println!(
        "[search] snapshots    : {} scanned / {} in repo",
        report.snapshots_scanned, report.snapshots_in_repo
    );
    println!("[search] sessions seen: {}", report.sessions_seen);
    println!("[search] data blobs read: {}", report.data_blobs_read);
    for path in &report.unreadable {
        println!("  !! unreadable: {path}");
    }

    let code = if report.hits.is_empty() {
        println!("{}", report.no_hit_line());
        if report.complete() {
            ExitCode::from(1)
        } else {
            ExitCode::from(3)
        }
    } else {
        println!("[search] matched      : {}", report.hits.len());
        for hit in &report.hits {
            println!(
                "  {}  machine={}  shards={}  bytes={}  snapshot={}  activity_unix={}",
                hit.short_id(),
                hit.machine,
                hit.shard_count,
                hit.bytes,
                hit.short_snapshot(),
                hit.activity_unix
            );
        }
        if !report.complete() {
            // Hits *and* unreadable parts: the hits are real, the absence of
            // further hits is not established. Say so, and do not exit 0.
            println!(
                "search: PARTIAL — the matches above are real, but `{}` could not be read in full ({} unreadable), so there may be more",
                report.destination,
                report.unreadable.len()
            );
            ExitCode::from(3)
        } else {
            ExitCode::SUCCESS
        }
    };

    if cost {
        let c = report.fulltext_cost();
        println!("[search] full-text pass over these hits would need:");
        println!(
            "  sessions={}  shards={}  data_blobs={}  plaintext_bytes={}",
            c.sessions, c.shards, c.data_blobs, c.plaintext_bytes
        );
        println!("  (not performed — full-text matching is not implemented)");
    }

    reap_remote(&cfg, no_reap);
    code
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
    let state_dir = chat_stasher::collect::default_state_dir();
    if let Err(e) = chat_stasher::inbox::remember_inbox(inbox, &state_dir) {
        eprintln!("ingest: cannot persist consumed-inbox audit pointer: {e:#}");
        return ExitCode::FAILURE;
    }
    println!("[ingest] shard bucket cap : {shard_bucket_cap}");
    print_ingest(&report, &machine);
    ExitCode::SUCCESS
}

/// Build the destination this pass collects for.
///
/// The archive probe is deliberately lazy: opening the repository and reading
/// every snapshot back is expensive, and a run whose debts are all still owed
/// on the stage never has to ask. Every failure path — no repository yet, no
/// persisted key, an unreachable backend — returns `Err`, which the collector
/// reads as "cannot be verified" and therefore as "unread". It must never
/// create a masterkey as a side effect of collecting.
fn destination_view<'a>(
    cfg: &'a StoreConfig,
    machine: &'a str,
) -> chat_stasher::collect::DestinationView<'a> {
    chat_stasher::collect::DestinationView::new(
        chat_stasher::collect::destination_id(&cfg.repo_root),
        move || {
            let store = BackupStore::new(cfg.clone(), machine.to_string());
            if !store.repository_exists()? {
                anyhow::bail!("destination repository is not initialised");
            }
            let mk = store::load_key_file(cfg)?;
            let observation = store.read_all_machines(&mk)?;
            Ok(chat_stasher::collect::archive_facts_from_readback(
                &observation,
            ))
        },
    )
}

/// One word per ADR-015 state. Deliberately three *different* words: calling
/// all three "skipped" is the failure mode this exists to prevent.
fn source_status_word(status: SourceStatus) -> &'static str {
    match status {
        SourceStatus::Consulted => "consulted",
        SourceStatus::KnownEmpty => "never-built",
        SourceStatus::SuspectedLoss => "SUSPECTED-LOSS",
        SourceStatus::Unknown => "unknown",
    }
}

fn join_or_none(names: &[&str]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// `dest-init` — ADR-013. Give a new destination the union of the local
/// sources and every existing destination, in that order.
///
/// Exit code is non-zero when the difference set could not be computed in
/// full, even if the local part pushed fine: a destination that is initialised
/// from an incomplete union must not look like a finished one.
#[allow(clippy::too_many_arguments)]
fn cmd_dest_init(
    destination: Option<String>,
    stage: &Path,
    machine: Option<&str>,
    shard_bucket_cap: usize,
    from: &[String],
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    no_reap: bool,
) -> ExitCode {
    let config = Config::load();
    if destination.is_none() && repo.is_none() {
        eprintln!(
            "dest-init: name the destination being initialised (`--destination <name>`, or an explicit `--repo`)"
        );
        return ExitCode::from(2);
    }
    let machine = machine
        .map(String::from)
        .unwrap_or_else(chat_stasher::id::machine_id);
    let target = resolve_store_config(
        &config,
        destination.as_deref(),
        repo,
        key_file,
        connections,
        options,
    );

    // Which existing destinations the difference set is computed against.
    // Default = every *other* declared destination; naming them explicitly is
    // always allowed, naming one that does not exist never is.
    let source_names: Vec<String> = if from.is_empty() {
        config
            .destinations
            .keys()
            .filter(|name| Some(name.as_str()) != destination.as_deref())
            .cloned()
            .collect()
    } else {
        from.to_vec()
    };
    let state_dir = chat_stasher::collect::default_state_dir();
    let mut sources = Vec::new();
    for name in &source_names {
        if Some(name.as_str()) == destination.as_deref() {
            eprintln!("dest-init: `--from {name}` is the destination being initialised");
            return ExitCode::from(2);
        }
        if !config.destinations.contains_key(name) {
            eprintln!("dest-init: `--from {name}` is not declared in the config");
            return ExitCode::from(2);
        }
        let cfg = resolve_store_config(&config, Some(name), None, None, None, &[]);
        // Read before step 1 runs: step 1 writes this machine's state for the
        // *target*, and we want the record as it stood before this command.
        let previously_recorded = chat_stasher::collect::destination_has_record(
            &state_dir,
            &chat_stasher::collect::destination_id(&cfg.repo_root),
        );
        sources.push(chat_stasher::destinit::SourceDestination {
            name: name.clone(),
            cfg,
            previously_recorded,
        });
    }

    println!(
        "[dest-init] destination   : sha256={} (new copy)",
        chat_stasher::collect::destination_id(&target.repo_root)
    );
    println!(
        "[dest-init] machine       : sha256={}",
        store::machine_fingerprint(&machine)
    );
    println!("[dest-init] stage         : {}", stage.display());
    println!(
        "[dest-init] sources       : {} ({})",
        sources.len(),
        if source_names.is_empty() {
            "no existing destination declared".to_string()
        } else {
            source_names.join(", ")
        }
    );

    // Step 1 — the local sources are the truth, and rereading them costs the
    // existing destinations nothing.
    println!("[dest-init] step 1        : re-collect from the local sources");
    let view = destination_view(&target, &machine);
    let report = match chat_stasher::collect::collect(
        &config,
        stage,
        &machine,
        &state_dir,
        shard_bucket_cap,
        &view,
    ) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("dest-init: local re-collect failed: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    print_collect_report(&report, stage, &state_dir, &machine);
    let local_failed = !report.errors.is_empty();

    // Step 2 — only what an existing destination has and the local source no
    // longer produced.
    println!(
        "[dest-init] step 2        : difference set (existing destination has it, local re-collect did not produce it)"
    );
    let diff = chat_stasher::destinit::fill_difference(stage, &machine, shard_bucket_cap, &sources);
    for source in &diff.sources {
        println!(
            "  source {:<16} sha256={} state={} reachable={} sessions_here={} other_machines={} missing_locally={} restored={} shards={} failed={}",
            source.name,
            source.destination_id,
            source_status_word(source.status),
            source.reachable,
            source.sessions_for_this_machine,
            source.sessions_other_machines,
            source.missing_locally,
            source.restored_sessions,
            source.restored_shards,
            source.failed_sessions.len(),
        );
        if let Some(reason) = &source.unreachable_reason {
            println!("    reason: {reason}");
        }
        match source.status {
            SourceStatus::KnownEmpty => println!(
                "    NEVER-BUILT: no repository at that location, and this machine has no record of \
                 ever collecting for it. It was never built, so it holds nothing and cannot be \
                 holding a copy we need. Not counted against the union."
            ),
            SourceStatus::SuspectedLoss => println!(
                "    !! SUSPECTED DATA LOSS: this machine DOES have a record of collecting for this \
                 destination, and its archive can no longer be read. This is NOT an empty \
                 destination — it may have been holding the only remaining copy of some sessions. \
                 Do not re-create it blank: find the original first."
            ),
            SourceStatus::Unknown => println!(
                "    UNKNOWN: we could not establish whether a repository is there at all, and we \
                 have no record of collecting for it. Unknown is not empty — this could be a \
                 destination that was never built, or one that is merely unreachable right now."
            ),
            SourceStatus::Consulted => {}
        }
        if source.sessions_other_machines > 0 {
            println!(
                "    WARNING: {} session(s) belong to another machine partition and were NOT copied \
                 (the stage is single-partition by construction; copying them would re-attribute \
                 another machine's history to this one). They remain only in the source destination.",
                source.sessions_other_machines
            );
        }
    }
    println!(
        "[dest-init] restored      : sessions={} shards={}",
        diff.restored_sessions, diff.restored_shards
    );
    println!(
        "[dest-init] never built   : {} ({})",
        diff.known_empty().len(),
        join_or_none(&diff.known_empty())
    );
    println!(
        "[dest-init] suspect lost  : {} ({})",
        diff.suspected_loss().len(),
        join_or_none(&diff.suspected_loss())
    );
    println!(
        "[dest-init] unknown       : {} ({})",
        diff.unknown().len(),
        join_or_none(&diff.unknown())
    );
    println!("[dest-init] diff complete : {}", diff.diff_complete);

    // Step 3 — push whatever is now on the stage. This runs even when the
    // difference set is incomplete: keeping what we do know is strictly better
    // than dropping it, and the non-zero exit still says the union is unproven.
    let stage_shards = match store::sealed_shard_count(stage) {
        Ok(count) => count,
        Err(e) => {
            eprintln!("dest-init: cannot audit stage: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let mut push_failed = false;
    if stage_shards == 0 {
        println!("[dest-init] step 3        : nothing on the stage, no snapshot created");
    } else {
        println!("[dest-init] step 3        : push the union to the new destination");
        let (mk, _) = masterkey(&target);
        let store = BackupStore::new(target.clone(), machine.clone());
        match store.push(stage, &mk) {
            Ok(summary) => println!(
                "[dest-init] push          : stage_shards={} files_new={} files_unmodified={} data_added={} snapshots={}",
                summary.stage_shards,
                summary.files_new,
                summary.files_unmodified,
                summary.data_added,
                summary.snapshots_in_repo,
            ),
            Err(e) => {
                eprintln!("dest-init: push failed: {e:#}");
                push_failed = true;
            }
        }
    }
    reap_remote(&target, no_reap);

    // Suspected loss outranks a merely incomplete difference set: the same
    // exit code, but a completely different thing to go and do about it.
    let lost = diff.suspected_loss();
    if !lost.is_empty() {
        eprintln!(
            "dest-init: result: SUSPECTED-DATA-LOSS exit_code=1 — {} destination(s) that this machine has \
             collected for before can no longer be read: {}. This is NOT \"one fewer destination to copy \
             from\": each of them may have been the last place some sessions still existed. The new \
             destination has been given everything else, but it is NOT proven to hold the union. Go find \
             those archives before you re-create anything.",
            lost.len(),
            lost.join(", "),
        );
        return ExitCode::FAILURE;
    }
    if !diff.diff_complete {
        let unknown = diff.unknown();
        eprintln!(
            "dest-init: result: INCOMPLETE exit_code=1 — the difference set could not be computed in full. \
             {} destination(s) could not be consulted or copied from ({}), so it is UNKNOWN whether this new \
             destination holds the union. Unknown is not empty, and it is not loss either — we cannot tell \
             whether these were ever built. Re-run once they are reachable.",
            unknown.len().max(1),
            join_or_none(&unknown),
        );
        return ExitCode::FAILURE;
    }
    if local_failed || push_failed {
        eprintln!("[dest-init] result: ERROR exit_code=1 local_errors={local_failed} push_failed={push_failed}");
        return ExitCode::FAILURE;
    }
    println!("[dest-init] result: COMPLETED exit_code=0 union=local+existing-destinations");
    ExitCode::SUCCESS
}

fn cmd_collect(
    stage: &Path,
    machine: Option<&str>,
    shard_bucket_cap: usize,
    destination: Option<String>,
    repo: Option<String>,
    key_file: Option<String>,
) -> ExitCode {
    let config = Config::load();
    let machine = machine
        .map(String::from)
        .unwrap_or_else(chat_stasher::id::machine_id);
    let state_dir = chat_stasher::collect::default_state_dir();
    let store_cfg =
        resolve_store_config(&config, destination.as_deref(), repo, key_file, None, &[]);
    let view = destination_view(&store_cfg, &machine);
    let report = match chat_stasher::collect::collect(
        &config,
        stage,
        &machine,
        &state_dir,
        shard_bucket_cap,
        &view,
    ) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("collect: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    print_collect_report(&report, stage, &state_dir, &machine);
    if !report.errors.is_empty() {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn print_collect_report(
    report: &chat_stasher::collect::CollectReport,
    stage: &Path,
    state_dir: &Path,
    machine: &str,
) {
    println!("[collect] stage           : {}", stage.display());
    println!("[collect] state           : {}", state_dir.display());
    println!("[collect] machine         : {machine}");
    println!(
        "[collect] destination     : sha256={} (read state is kept per destination)",
        report.destination_id
    );
    println!(
        "[collect] legacy state    : ignored={} (a pre-destination state file proves nothing to any destination)",
        report.legacy_state_ignored
    );
    println!(
        "[collect] unverified      : {} cursor(s) could not prove themselves and were reread",
        report.unverified_cursors
    );
    println!(
        "[collect] scanner records : {} (only SessionRecord values; not the full recognised-session count)",
        report.scanned_records
    );
    println!(
        "[collect] opencode records: {} (one virtual SessionRecord per SQLite session)",
        report.scanned_opencode_records
    );
    println!(
        "[collect] cursor records  : {} (one virtual SessionRecord per qualified composer)",
        report.scanned_cursor_records
    );
    println!(
        "[collect] grok records    : {} (one virtual SessionRecord per session_docs row)",
        report.scanned_grok_records
    );
    println!(
        "[collect] not archivable  : {} harness(es) recognised sessions without enough SessionRecord values",
        report.archive_gaps.len()
    );
    for gap in &report.archive_gaps {
        println!("{}", scanner::format_archive_gap(gap));
    }
    println!(
        "[collect] changed={} unchanged={} reset={} shards={} lines={}",
        report.changed_records,
        report.unchanged_records,
        report.reset_records,
        report.shards_written,
        report.lines_written
    );
    println!(
        "[collect] read bytes      : delta_or_full={} prefix_validated={}",
        report.delta_bytes_read, report.prefix_bytes_validated
    );
    if !report.reconciliations.is_empty() {
        println!(
            "[collect] reconciled      : {} session(s) forced through reset",
            report.reconciliations.len()
        );
        for notice in &report.reconciliations {
            println!(
                "  ! session={} reason={}",
                notice.session_prefix, notice.reason
            );
        }
    }
    for outcome in &report.outcomes {
        println!(
            "  + session={} path_sha256={} source_bytes={} read_bytes={} prefix_bytes={} lines={} shard={} reset={} compressed={}",
            outcome.session_prefix,
            outcome.source_path_sha256,
            outcome.source_bytes,
            outcome.bytes_read,
            outcome.prefix_bytes_validated,
            outcome.lines_written,
            outcome.shard.as_deref().unwrap_or("none"),
            outcome.reset,
            outcome.compressed,
        );
    }
    if !report.errors.is_empty() {
        println!("[collect] errors          : {}", report.errors.len());
        for error in &report.errors {
            println!(
                "  ! session={} path_sha256={} source_not_collected=true",
                error.session_prefix, error.source_path_sha256
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// One scheduled pass. Wraps [`run_once_pass`] with the durable record the
/// timer-visibility feature needs: whatever happened — success, no-op or
/// failure — one `run-state.json` is written before we return.
///
/// Deliberately silent: a write failure is reported on stderr only, and
/// nothing about this record is printed on the happy path, so scheduled runs
/// look exactly as they did before.
fn cmd_run_once(
    stage: &Path,
    machine: Option<String>,
    shard_bucket_cap: usize,
    destination: Option<String>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    verify: bool,
    no_reap: bool,
) -> ExitCode {
    let started = std::time::Instant::now();
    let (code, mut state) = run_once_pass(
        stage,
        machine,
        shard_bucket_cap,
        destination,
        repo,
        key_file,
        connections,
        options,
        verify,
        no_reap,
    );
    state.duration_ms = started.elapsed().as_millis() as u64;
    let state_dir = chat_stasher::collect::default_state_dir();
    if let Err(e) = chat_stasher::runstate::save(&state_dir, &state) {
        eprintln!("[run-once] warning: run-state not recorded: {e:#}");
    }
    code
}

#[allow(clippy::too_many_arguments)]
fn run_once_pass(
    stage: &Path,
    machine: Option<String>,
    shard_bucket_cap: usize,
    destination: Option<String>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    verify: bool,
    no_reap: bool,
) -> (ExitCode, chat_stasher::runstate::RunState) {
    use chat_stasher::runstate::{RunOutcome, RunState};

    let config = Config::load();
    let machine_name = machine.clone().unwrap_or_else(chat_stasher::id::machine_id);
    // Pessimistic starting point: until a step proves otherwise this pass is
    // recorded as a failure, so an unexpected exit can never be read as "ok".
    let mut state = RunState::new(RunOutcome::Error, Some("start"), &machine_name, 0);
    let state_dir = chat_stasher::collect::default_state_dir();
    // The destination is resolved from the same overrides this run will push
    // to, so `collect` accrues debt against the repository `push` settles it
    // against — not against whatever the config happens to default to.
    let collect_cfg = resolve_store_config(
        &config,
        destination.as_deref(),
        repo.clone(),
        key_file.clone(),
        connections,
        options,
    );
    let view = destination_view(&collect_cfg, &machine_name);
    let report = match chat_stasher::collect::collect(
        &config,
        stage,
        &machine_name,
        &state_dir,
        shard_bucket_cap,
        &view,
    ) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("[run-once] result: ERROR exit_code=1 collect={e:#}");
            state.failed_step = Some("collect".to_string());
            return (ExitCode::FAILURE, state);
        }
    };
    state.shards_written = report.shards_written;
    state.collect_errors = report.errors.len();
    state.archive_gaps = report.archive_gaps.len();
    print_collect_report(&report, stage, &state_dir, &machine_name);
    if !report.errors.is_empty() || !report.archive_gaps.is_empty() {
        eprintln!(
            "[run-once] result: ERROR exit_code=1 collect_incomplete errors={} archive_gaps={}",
            report.errors.len(),
            report.archive_gaps.len()
        );
        state.failed_step = Some("collect-incomplete".to_string());
        return (ExitCode::FAILURE, state);
    }

    let stage_shards = match store::sealed_shard_count(stage) {
        Ok(count) => count,
        Err(e) => {
            eprintln!("[run-once] result: ERROR exit_code=1 stage_audit={e:#}");
            state.failed_step = Some("stage-audit".to_string());
            return (ExitCode::FAILURE, state);
        }
    };
    state.stage_shards = stage_shards;
    let changed = report.changed_records > 0 || report.shards_written > 0;
    let only_if_changed = config.push_only_if_changed.unwrap_or(true);
    let should_push = stage_shards > 0 && (!only_if_changed || changed);
    if !should_push {
        println!(
            "[run-once] push skipped: changed={} push_only_if_changed={} stage_shards={}",
            changed, only_if_changed, stage_shards
        );
        if verify {
            let cfg = resolve_store_config(
                &config,
                destination.as_deref(),
                repo.clone(),
                key_file.clone(),
                connections,
                options,
            );
            let verifier = BackupStore::new(cfg.clone(), machine_name.clone());
            match verifier.repository_exists() {
                Ok(true) => {
                    let code = cmd_verify(
                        VerifyLevel::L1,
                        &None,
                        Some(&machine_name),
                        destination.clone(),
                        repo,
                        key_file,
                        connections,
                        options,
                        no_reap,
                    );
                    if code != ExitCode::SUCCESS {
                        eprintln!("[run-once] result: ERROR exit_code=1 verify=l1");
                        state.failed_step = Some("verify".to_string());
                        return (ExitCode::FAILURE, state);
                    }
                }
                Ok(false) => {
                    println!("[run-once] verify skipped: no repository exists yet");
                }
                Err(e) => {
                    eprintln!("[run-once] result: ERROR exit_code=1 verify_preflight={e:#}");
                    state.failed_step = Some("verify-preflight".to_string());
                    return (ExitCode::FAILURE, state);
                }
            }
        }
        println!("[run-once] result: NOOP snapshot=not-created exit_code=0");
        state.outcome = RunOutcome::Noop;
        state.failed_step = None;
        state.snapshot_created = false;
        return (ExitCode::SUCCESS, state);
    }

    let push_code = cmd_push(
        &stage.to_path_buf(),
        None,
        destination.clone(),
        repo.clone(),
        key_file.clone(),
        machine.clone(),
        connections,
        options,
        no_reap,
    );
    if push_code != ExitCode::SUCCESS {
        eprintln!("[run-once] result: ERROR exit_code=1 push_failed");
        state.failed_step = Some("push".to_string());
        return (ExitCode::FAILURE, state);
    }

    if verify {
        let code = cmd_verify(
            VerifyLevel::L1,
            &None,
            Some(&machine_name),
            destination,
            repo,
            key_file,
            connections,
            options,
            no_reap,
        );
        if code != ExitCode::SUCCESS {
            eprintln!("[run-once] result: ERROR exit_code=1 verify=l1");
            state.failed_step = Some("verify".to_string());
            return (ExitCode::FAILURE, state);
        }
    }
    println!("[run-once] result: COMPLETED snapshot=created exit_code=0");
    state.outcome = RunOutcome::Completed;
    state.failed_step = None;
    state.snapshot_created = true;
    (ExitCode::SUCCESS, state)
}

fn cmd_schedule(
    format: schedule::Format,
    stage: &Path,
    output: Option<PathBuf>,
    binary: Option<PathBuf>,
    verify: bool,
) -> ExitCode {
    let config = Config::load();
    let interval = match schedule::interval_secs(&config) {
        Ok(interval) => interval,
        Err(e) => {
            eprintln!("schedule: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let binary = match binary {
        Some(path) => absolute_path(&path),
        None => match std::env::current_exe() {
            Ok(path) => path,
            Err(e) => {
                eprintln!("schedule: cannot resolve current executable: {e}");
                return ExitCode::FAILURE;
            }
        },
    };
    let stage = absolute_path(stage);
    let files = schedule::render(
        format,
        &binary,
        &stage,
        interval,
        verify,
        &config::home_dir(),
    );
    let paths = match output {
        Some(output) => match schedule::write_templates(format, &output, &files) {
            Ok(paths) => {
                for path in &paths {
                    println!("[schedule] wrote template: {}", path.display());
                }
                paths
            }
            Err(e) => {
                eprintln!("schedule: {e:#}");
                return ExitCode::FAILURE;
            }
        },
        None => {
            for file in &files {
                println!("===== {} =====", file.name);
                print!("{}", file.content);
            }
            Vec::new()
        }
    };
    println!("[schedule] interval_secs: {interval}");
    println!("[schedule] install is NOT automatic.");
    if paths.is_empty() {
        match format {
            schedule::Format::Launchd => println!(
                "[schedule] save the plist as \"$HOME/Library/LaunchAgents/{label}.plist\" first.",
                label = schedule::LAUNCHD_LABEL
            ),
            schedule::Format::Systemd => {
                println!("[schedule] save both units under \"$HOME/.config/systemd/user/\" first.")
            }
        }
        println!("[schedule] installation requires you to execute this command yourself:");
        println!("{}", schedule::install_command_for_saved(format));
    } else {
        println!("[schedule] you must execute this command yourself to install:");
        println!("{}", schedule::install_command(format, &paths));
    }
    ExitCode::SUCCESS
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
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
    if let Err(e) = seal::validate_active_in_stage(active, stage) {
        eprintln!("seal: {e}");
        return ExitCode::FAILURE;
    }
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
            println!("[seal] source was stage-owned; no harness path was changed");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("seal: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Metadata-only ingest summary: counts, shard names, bytes, sha256, and
/// session-id prefixes. Source file names and error text are intentionally not
/// printed because they may contain project or account identifiers.
fn print_ingest(report: &chat_stasher::inbox::IngestReport, machine: &str) {
    println!(
        "[ingest] inbox            : candidates={} skipped_{}={}",
        report.total_inbox_files, "part", report.part_files_seen
    );
    println!("[ingest] consumed         : {}", report.consumed.len());
    for c in &report.consumed {
        println!(
            "  + kind={}  bytes={}  sha256={}  -> {}  (session={})",
            c.kind,
            c.file_bytes,
            c.file_sha256,
            c.shard,
            short_session_id(&c.id),
        );
    }
    println!("[ingest] duplicates (same bytes already archived, not re-sealed):");
    for d in &report.duplicates {
        println!(
            "  = sha256={}  matched in {}  (session={})",
            d.file_sha256,
            d.matched_shard,
            short_session_id(&d.id),
        );
    }
    if !report.errors.is_empty() {
        println!("[ingest] errors           : {}", report.errors.len());
    }
    println!(
        "[ingest] staging machine   : sha256={}",
        store::machine_fingerprint(machine)
    );
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

/// Resolve which destination a command operates on.
///
/// ADR-013 product rule: **there is no default destination.** Once the config
/// declares any named destination, a command that reaches a repository has to
/// say which one — no "there is only one, use it" convenience, because that is
/// precisely the shortcut that later reads the wrong copy. The only implicit
/// path left is the pre-ADR-013 single-destination mode (`rustic_repo`, or its
/// data-dir default) and it exists only while the `destinations` table is
/// empty, i.e. while there is nothing to choose *between*.
///
/// Errors exit with code 2 (usage), like clap's own argument errors.
#[allow(clippy::too_many_arguments)]
fn resolve_store_config(
    config: &Config,
    destination: Option<&str>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
) -> StoreConfig {
    let Some(name) = destination else {
        if repo.is_none() && !config.destinations.is_empty() {
            let mut names: Vec<&str> = config.destinations.keys().map(String::as_str).collect();
            names.sort_unstable();
            eprintln!(
                "destination: the config declares {} destination(s) — pass `--destination <name>` (there is no default). Declared: {}",
                names.len(),
                names.join(", ")
            );
            std::process::exit(2);
        }
        return store_config_from(config, repo, key_file, connections, options);
    };
    let Some(entry) = config.destinations.get(name) else {
        let mut names: Vec<&str> = config.destinations.keys().map(String::as_str).collect();
        names.sort_unstable();
        eprintln!(
            "destination: `{name}` is not declared in the config. Declared: {}",
            if names.is_empty() {
                "(none)".to_string()
            } else {
                names.join(", ")
            }
        );
        std::process::exit(2);
    };
    let Some(repo_root) = repo.or_else(|| entry.repo.clone()) else {
        eprintln!("destination: `{name}` has no `repo` set (and no --repo was given)");
        std::process::exit(2);
    };
    let mut merged: BTreeMap<String, String> = entry.options.clone();
    for kv in options {
        match kv.split_once('=') {
            Some((k, v)) => {
                merged.insert(k.to_string(), v.to_string());
            }
            None => {
                eprintln!("option: option must be key=value, got `{kv}`");
                std::process::exit(2);
            }
        }
    }
    StoreConfig {
        repo_root,
        key_file: key_file
            .map(PathBuf::from)
            .or_else(|| entry.key_file.as_deref().map(PathBuf::from))
            // Per-destination default: one key file per destination, so a new
            // destination never silently adopts another one's key.
            .unwrap_or_else(|| data_root().join(format!("masterkey-{name}.json"))),
        connections: 0,
        options: merged,
    }
    .with_capped_connections(
        connections
            .or(entry.connections)
            .or(config.rustic_connections),
    )
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
    inbox: Option<PathBuf>,
    destination: Option<String>,
    repo: Option<String>,
    key_file: Option<String>,
    machine: Option<String>,
    connections: Option<usize>,
    options: &[String],
    no_reap: bool,
) -> ExitCode {
    let config = Config::load();
    let machine = machine.unwrap_or_else(chat_stasher::id::machine_id);
    let state_dir = chat_stasher::collect::default_state_dir();
    let cfg = resolve_store_config(
        &config,
        destination.as_deref(),
        repo,
        key_file,
        connections,
        options,
    );
    let stage_check =
        match chat_stasher::collect::inspect_stage_for_push(&config, stage, &state_dir) {
            Ok(check) => check,
            Err(e) => {
                eprintln!("push: cannot establish empty-stage safety: {e:#}");
                return ExitCode::FAILURE;
            }
        };
    println!(
        "[push] stage check   : shards={} scanner_records={} sqlite_sessions={} sqlite_unknown={} committed_reads={}",
        stage_check.stage_shards,
        stage_check.scanner_records,
        stage_check.scanner_sqlite_sessions,
        stage_check.scanner_sqlite_unknown,
        stage_check.committed_reads,
    );
    if stage_check.stage_shards == 0 {
        let inboxes = match inbox {
            Some(inbox) => vec![inbox],
            None => match chat_stasher::inbox::remembered_inboxes(&state_dir) {
                Ok(inboxes) => inboxes,
                Err(_) => Vec::new(),
            },
        };
        let consumed =
            match chat_stasher::inbox::audit_consumed_against_stage(&inboxes, stage, &state_dir) {
                Ok(audit) => audit,
                Err(_) => {
                    eprintln!("push: cannot establish consumed-inbox audit for empty stage");
                    return ExitCode::FAILURE;
                }
            };
        let store = BackupStore::new(cfg.clone(), machine.clone());
        let mut repo_covered_files = 0usize;
        let mut archive_error = false;
        if !consumed.stage_missing_sha256.is_empty() {
            match store.repository_exists() {
                Ok(true) => match store::load_key_file(&cfg) {
                    Ok(mk) => {
                        match store.archived_file_sha256s(&mk, &consumed.stage_missing_sha256) {
                            Ok(found) => {
                                repo_covered_files = consumed
                                    .hash_counts
                                    .iter()
                                    .filter(|(sha, _)| found.contains(*sha))
                                    .map(|(_, count)| *count)
                                    .sum();
                            }
                            Err(_) => archive_error = true,
                        }
                    }
                    Err(_) => archive_error = true,
                },
                Ok(false) => {}
                Err(_) => archive_error = true,
            }
        }
        let missing_files = consumed
            .file_count
            .saturating_sub(consumed.stage_covered_files + repo_covered_files);
        println!(
            "[push] consumed audit: inboxes={} files={} bytes={} unique_sha256={} cache_hits={} rehashed={} stage_covered={} repo_covered={} missing={}",
            inboxes.len(),
            consumed.file_count,
            consumed.total_bytes,
            consumed.unique_sha256(),
            consumed.cache_hits,
            consumed.rehashed,
            consumed.stage_covered_files,
            repo_covered_files,
            missing_files,
        );
        if stage_check.empty_stage_is_safe() && missing_files == 0 && !archive_error {
            println!(
                "[push] no archivable content this run: stage, scanner, collector, and consumed audit agree"
            );
            return ExitCode::SUCCESS;
        }
        if missing_files > 0 || archive_error {
            eprintln!(
                "push: refusing empty snapshot: consumed files are not proven in stage or repository"
            );
            return ExitCode::FAILURE;
        }
        eprintln!(
            "push: refusing empty snapshot: stage contains no sealed shards; collect or restore the stage first"
        );
        return ExitCode::FAILURE;
    }
    let (mk, key_was_new) = masterkey(&cfg);
    let store = BackupStore::new(cfg.clone(), machine.clone());
    println!(
        "[push] machine        : sha256={}",
        store::machine_fingerprint(&machine)
    );
    println!(
        "[push] repo            : sha256={}",
        sha256_hex(store.cfg.repo_root.as_bytes())
    );
    println!(
        "[push] key file       : sha256={}",
        sha256_hex(store.cfg.key_file.to_string_lossy().as_bytes())
    );
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
    println!("[push] stage shards   : {}", summary.stage_shards);
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
        "[push] snapshot host  : sha256={} (must equal machine)",
        store::machine_fingerprint(&summary.snapshot_host)
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
    destination: Option<String>,
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
    let cfg = resolve_store_config(
        &config,
        destination.as_deref(),
        repo,
        key_file,
        connections,
        options,
    );
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
                "    session {:<8} shards={:<3} bytes={:<10} sha256={}",
                short_session_id(&s.session_id),
                s.shard_count,
                s.concat_bytes,
                s.sha256
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
#[allow(clippy::too_many_arguments)]
fn cmd_verify(
    level: VerifyLevel,
    stage: &Option<PathBuf>,
    machine: Option<&str>,
    destination: Option<String>,
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
    let cfg = resolve_store_config(
        &config,
        destination.as_deref(),
        repo,
        key_file,
        connections,
        options,
    );
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
        let session = short_session_id(&row.session_id);
        let mark = if row.outcome == SessionOutcome::Match {
            "ok "
        } else {
            "!! "
        };
        match &row.outcome {
            SessionOutcome::Match => println!(
                "  {mark} {:<12} {:<20} shards={:<2} bytes={:<10} sha={}",
                row.machine, session, row.observed_shards, row.observed_bytes, row.observed_sha
            ),
            SessionOutcome::MissingInArchive => println!(
                "  {mark} {:<12} {:<20} MISSING IN ARCHIVE",
                row.machine, session
            ),
            SessionOutcome::ShardCountMismatch { expected, observed } => println!(
                "  {mark} {:<12} {:<20} SHARD COUNT expected={expected} observed={observed}",
                row.machine, session
            ),
            SessionOutcome::ByteLengthMismatch { expected, observed } => println!(
                "  {mark} {:<12} {:<20} BYTE LENGTH expected={expected} observed={observed}",
                row.machine, session
            ),
            SessionOutcome::ShaMismatch { expected, observed } => println!(
                "  {mark} {:<12} {:<20} SHA MISMATCH\n      expected={expected}\n      observed={observed}",
                row.machine, session
            ),
        }
    }
    for (m, s) in &r.extra_in_archive {
        println!(
            "  !? {m:<12} {:<20} in archive but NOT in expected manifest (informational)",
            short_session_id(s)
        );
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

fn short_session_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod decision_surface_tests {
    use super::*;
    use clap::CommandFactory;
    use std::fs;

    #[test]
    fn seal_help_and_active_guard_follow_decision() {
        let mut command = Cli::command();
        let seal_command = command
            .find_subcommand_mut("seal")
            .expect("seal subcommand must remain user-visible");
        let mut help = Vec::new();
        seal_command.write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();
        assert!(
            !help.contains("reopen-by-path") && !help.contains("original path now free"),
            "seal help must not describe changing a harness live-file path"
        );
        assert!(
            help.contains("inside --stage"),
            "seal help must state the stage-only boundary"
        );

        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let outside = dir.path().join("outside.jsonl");
        fs::create_dir_all(&stage).unwrap();
        fs::write(&outside, b"fixture\n").unwrap();
        let err = seal::seal_active_file(&outside, &stage, "m", "s", 20)
            .expect_err("--active outside --stage must be rejected");
        assert!(err.to_string().contains("inside --stage"));
        assert!(outside.exists());
    }

    #[test]
    fn status_marks_harness_sessions_without_session_records() {
        let dir = tempfile::tempdir().unwrap();
        let mut report = scanner::ScanReport {
            records: Vec::new(),
            missing_roots: Vec::new(),
            probes: vec![scanner::HarnessProbe {
                id: "opencode".to_string(),
                display_name: "fixture harness".to_string(),
                root: Some(dir.path().join("store.db")),
                confidence: scanner::Confidence::Confirmed,
                state: scanner::ProbeState::FileTarget,
                record_count: Some(1),
                candidate_count: Some(1),
                earliest: None,
                latest: None,
                bytes: 1,
                recognized_files: Vec::new(),
                note: String::new(),
            }],
        };

        let output = render_archive_gap_notice(&report);
        assert!(
            output.contains("不可归档"),
            "status must mark recognised sessions that have no SessionRecord: {output}"
        );

        report.records.push(chat_stasher::models::SessionRecord {
            id: "opencode.fixture.session".to_string(),
            absolute_path: dir.path().join("exported.jsonl"),
            byte_size: 1,
            mtime: std::time::SystemTime::UNIX_EPOCH,
            source: chat_stasher::models::HarnessSource::OpenCode,
            compressed: false,
            sqlite_layout: Some(chat_stasher::models::SqliteSessionLayout::OpenCode),
        });
        let output = render_archive_gap_notice(&report);
        assert!(
            !output.contains("不可归档"),
            "the marker must disappear once the harness produces a SessionRecord: {output}"
        );
    }
}

fn cmd_init() -> ExitCode {
    match Config::init_default(config::DEFAULT_CONFIG_TEMPLATE) {
        Ok(()) => {
            println!(
                "next: chat-stasher collect --stage <stage-dir> && chat-stasher push --stage <stage-dir> && chat-stasher verify --stage <stage-dir>"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("init: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_status() -> ExitCode {
    let config = Config::load();
    let verdict = run_state_verdict(&config);
    println!("[run-once] {}", verdict.line);

    let report = match scanner::scan(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("status: scan failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    print_status(&report);
    if verdict.healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Read the last `run-once` record and turn it into one sentence.
///
/// The cadence comes from the same config the scheduler templates use, so the
/// overdue threshold tracks whatever the user actually scheduled. If the
/// cadence is unusable, fall back to the default rather than skipping the
/// overdue check — silently dropping it is the failure this exists to catch.
fn run_state_verdict(config: &Config) -> chat_stasher::runstate::Verdict {
    use chat_stasher::runstate;

    let interval = schedule::interval_secs(config).unwrap_or(config::DEFAULT_BACKUP_INTERVAL_SECS);
    let state_dir = chat_stasher::collect::default_state_dir();
    let read = runstate::load(&state_dir);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    runstate::summarize(&read, now, runstate::stale_after_secs(interval))
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
    print!("{}", render_archive_gap_notice(report));
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
            short_session_id(&rec.id),
        );
    }
    println!();
}

fn render_archive_gap_notice(report: &scanner::ScanReport) -> String {
    use std::fmt::Write as _;

    let gaps = report.archive_gaps();
    if gaps.is_empty() {
        return String::new();
    }
    let mut output = String::new();
    writeln!(
        output,
        "  ⚠ 不可归档会话：以下 harness 已识别会话，但未产出 SessionRecord；collect 当前不会归档它们。"
    )
    .unwrap();
    for gap in &gaps {
        writeln!(output, "{}", scanner::format_archive_gap(gap)).unwrap();
    }
    writeln!(
        output,
        "  建议：不要把 scanner records 当作已识别会话总数；等对应 harness 产出 SessionRecord 后再运行 collect。"
    )
    .unwrap();
    output
}
