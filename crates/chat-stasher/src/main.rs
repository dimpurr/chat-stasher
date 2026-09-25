//! chat-stasher CLI entry point.

use anyhow::Context;
use chat_stasher::activity;
use chat_stasher::config::{self, Config};
use chat_stasher::destinit::SourceStatus;
use chat_stasher::identity;
use chat_stasher::nativehost;
use chat_stasher::overview;
use chat_stasher::readback;
use chat_stasher::reap;
use chat_stasher::scanner;
use chat_stasher::schedule;
use chat_stasher::seal;
use chat_stasher::sidecar;
use chat_stasher::stagereclaim::{self, BlockedKind, NamedStore};
use chat_stasher::store::{self, BackupStore, StoreConfig};
use chat_stasher::verify::{CheckSummary, ExpectationBasis, ReconcileReport, SessionOutcome};
use clap::{Parser, Subcommand};
use rustic_core::repofile::{MasterKey, NodeType};
use rustic_core::{Credentials, LsOptions, Repository};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const UNRESOLVED_MACHINE: &str = "<machine-identity-unavailable>";

/// Narrate one line on stdout, and let a failed write be a non-event.
///
/// `println!` panics when its write fails, which is the right default for a
/// command whose output *is* stdout. `ui` is not one: its product is the
/// socket. A reader that stops reading — `chat-stasher ui | head -1`, a
/// terminal that goes away, a supervisor that takes the URL and closes — must
/// not take the dashboard down, least of all in the window between announcing
/// the URL and accepting the first request.
///
/// Measured, not hypothetical: narration dying on `Broken pipe` is what made
/// `tests/w15_ui_test.rs` flaky. Its harness reads the URL and closes the pipe
/// (its `Ui::start` drops the reader it took); the next line printed killed the
/// process before `serve` was ever entered, so the first request was answered
/// by nothing and the client read zero bytes.
///
/// Split from [`say`] so that a writer which always fails can be handed in — the
/// same reason `view::route` takes no socket.
///
/// Spelled as `write_fmt` plus an explicit newline rather than `writeln!(out,
/// "{args}")`, which would work but would put a `"{args}"` entry in
/// `docs-dev/output-inventory.txt`: that file is a human-readable list of
/// user-visible text, and a formatter is not one.
fn say_to(out: &mut dyn std::io::Write, args: std::fmt::Arguments<'_>) {
    #[allow(
        clippy::let_underscore_must_use,
        reason = "Dropping the error is this function's whole job: the dashboard's output is the socket, so a closed stdout is not a failure of the command."
    )]
    {
        let _ = out.write_fmt(args);
        let _ = out.write_all(b"\n");
    }
}

/// [`say_to`] on this process's own stdout.
fn say(args: std::fmt::Arguments<'_>) {
    say_to(&mut std::io::stdout(), args);
}

/// `println!`'s shape with [`say`]'s failure behaviour. The call sites keep
/// their exact text, so a diff that only changes the macro name cannot have
/// altered a message.
macro_rules! say {
    ($($arg:tt)*) => {
        say(format_args!($($arg)*))
    };
}

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

/// Arguments of the ephemeral dashboard server, shared by `ui` and by `view`
/// while it remains an alias.
///
/// The filters are the **shared** [`chat_stasher::selector::SelectorArgs`], not
/// a second set of flags: the drill-down links on the page are resolved through
/// the same type, so a dashboard opened with `--day X` and a link carrying
/// `day=X` cannot mean different things.
#[derive(Debug, Clone, clap::Args)]
struct UiArgs {
    /// Destination to open. Optional when the config makes the choice
    /// unambiguous: with exactly one `[destinations.<name>]` declared it
    /// opens that one, and with several it opens the one
    /// `[native_host] destination` names — or lists them and exits 2 when
    /// no default is recorded. An explicit `--repo` or `--destination`
    /// always wins. There is still no cross-destination merge: one dashboard
    /// serves exactly one archive.
    #[arg(long)]
    destination: Option<String>,
    /// Filters applied when the dashboard opens. Omit them to see the whole
    /// archive; every filter is also reachable as a link on the page.
    #[command(flatten)]
    filters: chat_stasher::selector::SelectorArgs,
    /// Do NOT launch a browser; just print the URL. Correct on headless or
    /// remote machines, where opening a browser is meaningless or wrong.
    #[arg(long)]
    no_open: bool,
    /// Exit after this many seconds with no request (0 = never idle out,
    /// still exits on Ctrl+C).
    #[arg(long, default_value_t = chat_stasher::view::DEFAULT_IDLE_SECS)]
    idle_timeout: u64,
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
    /// Keep the ssh ControlMaster processes open after this run (do not shut them down).
    #[arg(long)]
    keep_ssh_masters: bool,
}

#[derive(Subcommand, Clone, Copy)]
enum ScheduleAction {
    /// Render, write, and load launchd agents.
    Install,
    /// Unload and remove launchd agents.
    Uninstall,
}

/// Subcommands. `push`/`read` are backed by the BackupStore
/// (rustic_core); `doctor` answers one question — is a harness silently
/// deleting your history?
#[derive(Subcommand)]
enum Command {
    /// Write a commented default config if none exists (non-destructive).
    Init,
    /// Walk through first-run setup: scan, then the local first save (which
    /// creates the encrypted local repository and its masterkey), then the
    /// remote destination (which is written, connected to, and — for a host
    /// nobody has met before — left stopped until you have checked its key out
    /// of band). The scheduler step is still a stub.
    ///
    /// The local first save runs two `run-once` passes and reads the result
    /// back, so a first run archives once, then proves the second pass adds
    /// nothing, reports the run-state verdict, and reads one session back out
    /// of the repository. The remote step writes one `[destinations.<name>]`
    /// block and then runs `dest-init` against it. Non-TTY runs do the same
    /// work without prompting and report it as one JSON object.
    ///
    /// No flag of this command takes a secret as its value: the two credential
    /// flags name the environment variables that hold one.
    Setup {
        /// Stage directory that will hold sealed session shards. Interactive
        /// runs offer the default for this platform; this flag states it.
        #[arg(long)]
        stage: Option<PathBuf>,
        /// Name of the destination to configure and verify. Omit it to skip the
        /// remote step — the wizard will say plainly what that means for the
        /// archive. A name already declared in the config is verified as it
        /// stands and never rewritten.
        #[arg(long)]
        destination: Option<String>,
        /// Include the scheduler-install stub in the walkthrough.
        #[arg(long)]
        install_schedule: bool,
        /// Declare that you have your own copy of the masterkey (the
        /// interactive run asks you to type "I saved it elsewhere" instead).
        /// This is a declaration, not a verification: nothing here, and
        /// nothing anywhere else, checks that a copy exists.
        #[arg(long)]
        masterkey_saved_elsewhere: bool,
        /// Print one JSON object, including any missing named parameters.
        /// Non-TTY runs always use this contract and never prompt.
        #[arg(long)]
        json: bool,
        /// The remote destination step: which recipe, and the names and
        /// locations it is spelled with. Every parameter here is optional as a
        /// *type* — what is required is decided per kind, so a missing one can
        /// be named rather than guessed at.
        #[command(flatten)]
        remote: SetupRemoteArgs,
    },
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
        /// Keep the ssh ControlMaster processes open after this run (do not shut them down).
        #[arg(long)]
        keep_ssh_masters: bool,
    },
    /// Render a launchd plist or systemd user service/timer. The optional
    /// install/uninstall actions manage macOS launchd agents.
    Schedule {
        /// Optional action. Without it, only render the templates.
        #[command(subcommand)]
        action: Option<ScheduleAction>,
        /// Template format to render.
        #[arg(long, value_enum, default_value = "launchd", global = true)]
        format: schedule::Format,
        /// Which scheduled job to render. `run-once` (default) is the hourly
        /// archive cycle. `reclaim-stage` is the weekly stage reclamation that
        /// deletes staged shard bodies once every destination proves it holds
        /// them — unrelated to the ssh connection reaping that
        /// `--keep-ssh-masters` disables.
        #[arg(long, value_enum, default_value = "run-once", global = true)]
        unit: schedule::Unit,
        /// Stage path embedded in the one-shot command. Required for render
        /// and install; not needed for uninstall.
        #[arg(long, global = true)]
        stage: Option<PathBuf>,
        /// Write to this plist path, or systemd directory. Without it, print.
        /// With more than one destination the output must be a directory (one
        /// plist per destination for launchd, one service+timer pair for
        /// systemd).
        #[arg(long, global = true)]
        output: Option<PathBuf>,
        /// Binary path embedded in the template. Defaults to the current
        /// executable when it is outside target/. A build executable falls back
        /// to ~/.local/bin/chat-stasher or a Homebrew bin; target/ paths are
        /// rejected. Pass an installed binary path for a durable service.
        #[arg(long, global = true)]
        binary: Option<PathBuf>,
        /// Named destination forwarded to run-once. Repeat it to render or
        /// install one unit per declared destination. Not valid with unit
        /// reclaim-stage.
        #[arg(long, global = true)]
        destination: Vec<String>,
        /// Repository path override forwarded to the scheduled command
        /// (`reclaim-stage` honours it for a single-destination config only).
        #[arg(long, global = true)]
        repo: Option<String>,
        /// Masterkey file override forwarded to the scheduled command
        /// (`reclaim-stage` honours it for a single-destination config only).
        #[arg(long, global = true)]
        key_file: Option<String>,
        /// Concurrency cap override forwarded to the scheduled command.
        #[arg(long, global = true)]
        connections: Option<usize>,
        /// Backend option `key=value`, repeatable, forwarded to the scheduled
        /// command.
        #[arg(long = "option", global = true)]
        options: Vec<String>,
        /// Machine partition forwarded to `run-once`.
        #[arg(long, global = true)]
        machine: Option<String>,
        /// Maximum sealed shards per bucket forwarded to `run-once`.
        #[arg(long, global = true)]
        shard_bucket_cap: Option<usize>,
        /// Add the cheap L1 verify pass after each archive cycle.
        #[arg(long, global = true)]
        verify: bool,
        /// Keep the ssh ControlMaster processes open after the scheduled command
        /// runs (do not shut them down). This is about ssh connection masters
        /// only — it never disables the stage reclamation that `--unit
        /// reclaim-stage` performs.
        #[arg(long, global = true)]
        keep_ssh_masters: bool,
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
        /// Default: config `machine`, else this machine's identity (generated
        /// on first use); use `--machine` explicitly to choose the partition.
        #[arg(long)]
        machine: Option<String>,
        /// Concurrency cap override (default: config `rustic_connections` = 4).
        #[arg(long)]
        connections: Option<usize>,
        /// Backend option `key=value`, repeatable (e.g. `--option endpoint=ssh://host:23`).
        #[arg(long = "option")]
        options: Vec<String>,
        /// Keep the ssh ControlMaster processes open after this run (do not shut them down).
        #[arg(long)]
        keep_ssh_masters: bool,
    },
    /// Is the scheduled archive actually working? Plus what the local harness
    /// scanner finds (read-only). Add `--destination` to list archived writer
    /// versions and identify machines behind the newest writer.
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
    ///
    /// Default output is a handful of lines and nothing more: the verdict
    /// above, then aggregate scan counts (per-harness session totals, skipped
    /// roots, unarchivable-session warning). It stays that size whether the
    /// machine holds three sessions or three thousand, so the verdict never
    /// scrolls away.
    ///
    /// `--sessions` adds the per-session metadata table on top of that —
    /// one line per session (harness / bytes / mtime / first 8 chars of the
    /// id). That is the troubleshooting view; it can be hundreds of lines.
    Status {
        /// Also print one metadata line per session found (can be hundreds).
        #[arg(long)]
        sessions: bool,
        /// Print exactly one JSON object to stdout and nothing else (the
        /// human lines still go to stderr). Tri-state fields are tagged
        /// `{"kind":"known",…}` / `{"kind":"unknown","why":…}` /
        /// `{"kind":"not_applicable",…}` — an unknown is never serialised as
        /// `0`, `null` or a missing field. `run_state.kind` is
        /// `known` / `missing` / `unreadable`; `scanner.kind` is `ok` /
        /// `failed`. `exit_semantics` documents what the exit code means.
        #[arg(long)]
        json: bool,
        /// Also list archived per-machine writer versions from a destination.
        #[arg(long)]
        destination: Option<String>,
        /// Repository path override for archived writer versions.
        #[arg(long)]
        repo: Option<String>,
        /// Masterkey file override for archived writer versions.
        #[arg(long)]
        key_file: Option<String>,
        /// Concurrency cap override.
        #[arg(long)]
        connections: Option<usize>,
        /// Backend option `key=value`, repeatable.
        #[arg(long = "option")]
        options: Vec<String>,
        /// Keep ssh ControlMaster processes open after this run.
        #[arg(long)]
        keep_ssh_masters: bool,
    },
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
        /// Print full session ids in per-session rows (default: privacy-safe short ids).
        #[arg(long)]
        full_ids: bool,
        /// Machine partition to read. Uses this machine's id when available;
        /// if it is unavailable, use `--machine` explicitly. Unused by
        /// `--all-machines`, which reads every machine.
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
        /// Keep the ssh ControlMaster processes open after this run (do not shut them down).
        #[arg(long)]
        keep_ssh_masters: bool,
    },
    /// Diagnostic: does any harness on this machine silently delete its
    /// sessions? Read-only (paths/counts/bytes/timestamps only).
    Doctor {
        /// Print exactly one JSON object to stdout and nothing else. Tri-state
        /// fields are tagged `{"kind":"known",…}` / `{"kind":"unknown","why":…}`
        /// / `{"kind":"not_applicable",…}` — an unknown is never serialised as
        /// `0`, `null` or a missing field. `claude.verdict.kind` is one of
        /// `unset_default` / `safe` / `small_value` / `parse_failed`;
        /// `reclaim.kind` is `ok` / `no_repo` / `no_key` / `open_failed`.
        #[arg(long)]
        json: bool,
    },
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
        /// Print full session ids in L3 rows (default: privacy-safe short ids).
        #[arg(long)]
        full_ids: bool,
        /// Machine partition (default: config `machine`, else this machine's
        /// identity; use `--machine` explicitly to choose the partition).
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
        /// Keep the ssh ControlMaster processes open after this run (do not shut them down).
        #[arg(long)]
        keep_ssh_masters: bool,
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
        /// Keep the ssh ControlMaster processes open after this run (do not shut them down).
        #[arg(long)]
        keep_ssh_masters: bool,
        /// Record this destination's ssh host key in `~/.ssh/known_hosts` after
        /// printing its fingerprint. Off by default and never implied: without
        /// this flag nothing is ever written to `known_hosts`, so the first
        /// connection of an unattended run cannot silently trust a host.
        #[arg(long)]
        trust_host: bool,
    },
    /// Search one destination's archive by session metadata.
    ///
    /// Metadata tier only: this walks snapshot/index/tree objects plus each
    /// machine's small activity index, and never fetches or decrypts a
    /// session's conversation shard, which is why it is cheap. On a local
    /// three-session fixture the metadata walk read 11,761 bytes against
    /// 1,206,285 bytes of data packs — two orders of magnitude apart. Use
    /// `--cost` to see what a full-text pass over the current hits *would*
    /// cost before asking for one; full-text matching is not implemented.
    ///
    /// One destination per run, always named: there is no automatic merge
    /// across destinations, and no default destination to search "everything".
    ///
    /// The time window filters on the **conversation's own activity interval**,
    /// not on when the backup ran: a session matches when `[first message,
    /// last message]` intersects the window. Those times come from the
    /// `activity-index` sidecar (`meta/<machine>/activity-v1.jsonl`) carried in
    /// the archive, so `--day 2026-01-15` finds a conversation held that day
    /// even if the machine was last pushed months later.
    ///
    /// A session whose conversation time is unknown is **never silently
    /// excluded**. It is listed separately with the reason, and while a time
    /// window is active it does not count as a match — which also means a
    /// "0 matched" answer cannot be trusted while any remain, so the exit code
    /// is `3` rather than `1`.
    ///
    /// Exit codes distinguish the three answers, because two of them look the
    /// same and mean opposite things: `0` matched something, `1` read the whole
    /// destination and answered for every session and nothing matched, `3`
    /// could not finish — either reading it, or placing every session in time —
    /// so "nothing matched" is unproven. `2` is a usage error, as elsewhere.
    Search {
        /// Destination to search. Required unless an explicit `--repo` is given.
        #[arg(long)]
        destination: Option<String>,
        /// The shared filters (session / machine / harness / time window).
        #[command(flatten)]
        filters: chat_stasher::selector::SelectorArgs,
        /// Emit one JSON object on stdout instead of the human report. The
        /// three groups (matched / not matched / could not be placed) stay
        /// separate fields, so a consumer cannot read an unknown as an absence.
        #[arg(long)]
        json: bool,
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
        /// Keep the ssh ControlMaster processes open after this run (do not shut them down).
        #[arg(long)]
        keep_ssh_masters: bool,
    },
    /// Write every session the shared selector selects to files.
    ///
    /// One destination per run, always named, exactly like `search` — and the
    /// selection is literally the same code path: `export` writes the sessions
    /// `search` returns for the same flags, so `search` is the dry run of this
    /// command with a different name, and `--dry-run` prints the same price
    /// `search --cost` reports before anything is fetched.
    ///
    /// Layout: `<out>/<machine>/<harness>/<session-id>.jsonl`, each file holding
    /// that session's archived lines in their native format, byte-identical to
    /// what `read` returns for it. Plus `<out>/manifest.json`: per session its
    /// machine, harness, id, first and last message time, shard count, bytes
    /// written, sha256 of the written file and the filters that were applied —
    /// and, at the top level, the sessions no filter could place, the machines
    /// whose activity index could not be read, the sessions that could not be
    /// written, and the exit status this run returns.
    ///
    /// `--turns user` keeps only the lines that are the user's own messages.
    /// That is answerable only where the harness's format makes it certain (see
    /// `export::USER_TURNS_HARNESSES`); elsewhere every line is written and the
    /// session records `turns_filter: "not-supported"`. The flag is never
    /// silently ignored and content is never silently dropped.
    ///
    /// `--trim-to-window` drops lines whose own timestamp is outside
    /// `--day`/`--since`/`--until`. A line whose timestamp cannot be read is
    /// kept and counted in `untimed_lines`.
    ///
    /// `--out` must be empty or absent unless `--force` is given. Nothing is
    /// ever deleted, and nothing is ever written outside `--out`.
    ///
    /// Exit codes, the same family `search` uses: `0` wrote at least one
    /// session and answered every session the query touched · `1` read the
    /// whole destination and selected nothing · `3` did not finish — part of
    /// the archive was unreadable, a session could not be placed, or a selected
    /// session could not be written — so the output on disk is real but
    /// incomplete, and the manifest says what is missing · `2` usage error.
    Export {
        /// Destination to export from. Required unless an explicit `--repo` is given.
        #[arg(long)]
        destination: Option<String>,
        /// Directory to write into. Must be empty or absent unless `--force`.
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        /// The shared filters (session / machine / harness / time window).
        #[command(flatten)]
        filters: chat_stasher::selector::SelectorArgs,
        /// Which lines to write: `all` (default), or `user` for only the user's
        /// own messages — kept only for harnesses whose format makes that
        /// certain; for any other harness every line is written and the
        /// manifest records `turns_filter: "not-supported"`.
        #[arg(long, value_enum, default_value = "all")]
        turns: TurnsArg,
        /// With a time window, also drop lines whose own timestamp lies outside
        /// it. Lines whose time cannot be read are kept and counted in the
        /// manifest's `untimed_lines`. Requires a window.
        #[arg(long)]
        trim_to_window: bool,
        /// Write into a non-empty `--out`. Nothing is ever deleted, so files
        /// from an earlier export stay where they are.
        #[arg(long)]
        force: bool,
        /// Print the plan and stop: no directory is created and no file is
        /// written.
        #[arg(long)]
        dry_run: bool,
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
        /// Keep the ssh ControlMaster processes open after this run (do not shut them down).
        #[arg(long)]
        keep_ssh_masters: bool,
    },
    /// Open the archive dashboard in a browser: totals, the machine × source
    /// matrix, a weekly activity heatmap, and drill-down into any cell.
    ///
    /// Binds a short-lived HTTP server on `127.0.0.1` with an OS-assigned port
    /// (never `0.0.0.0`, never a fixed port), prints the URL, optionally opens
    /// your browser, and exits when idle or on Ctrl+C. Nothing is installed:
    /// no launchd, no systemd, no background process, no resident daemon.
    ///
    /// THREAT, stated plainly: loopback is NOT a security boundary. Every other
    /// program running on this machine can connect to `127.0.0.1`, so this is
    /// not "safe because it's local". Access is gated by a random token that is
    /// generated fresh on every launch and appears only in the printed URL —
    /// never in a file, never in a log. Requests without that exact token are
    /// refused (403), and any method other than GET is refused (405). Treat the
    /// URL as a secret for the lifetime of the process.
    ///
    /// Metadata tier: the dashboard and every list are rendered from one read of
    /// snapshot + index + tree metadata plus the activity sidecar. The only
    /// conversation text that read carries is each session's one-line label
    /// (at most 100 characters, recorded in the activity index); a session's
    /// full conversation is fetched and decrypted only when you click it and
    /// then click "load", and the byte cost is printed before you do. Exit
    /// codes: 0 served the dashboard — even an archive that holds nothing,
    /// which is served as an honest empty page rather than not starting; 3
    /// could not finish reading (or no key), and is also the exit status after
    /// serving an archive that could not be read in full; 2 is a usage error.
    Ui(UiArgs),
    /// Deprecated alias for `ui`; prints a one-line notice on stderr and behaves
    /// identically. Kept for one release.
    View(UiArgs),
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
        /// Default: config `machine`, else this machine's identity (generated
        /// on first use); use `--machine` explicitly to choose the partition.
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
    ///
    /// Exit codes: 0 = every recognised session was archivable and was
    /// collected; 3 = PARTIAL — the pass ran, but at least one harness
    /// recognised sessions this build cannot archive (the `not archivable`
    /// line), so what was written is real but incomplete; 1 = the pass failed
    /// (collect itself errored, or individual sources could not be read).
    Collect {
        /// Stage directory that holds the sealed `sessions/` tree.
        #[arg(long)]
        stage: PathBuf,
        /// Machine partition for `sessions/<machine>/…`.
        /// Default: config `machine`, else this machine's identity (generated
        /// on first use); use `--machine` explicitly to choose the partition.
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
    /// `rename`, with an evidence `seal_source` line **and** a `source
    /// confirmed` platform cell may be renamed. Everything else (Codex = fd-holder,
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
        /// Default: config `machine`, else this machine's identity (generated
        /// on first use); use `--machine` explicitly to choose the partition.
        #[arg(long)]
        machine: Option<String>,
        /// Session id to seal into. Default: the active file's stem.
        #[arg(long)]
        session: Option<String>,
        /// Maximum sealed shards per bucket (default: 20).
        #[arg(long, default_value_t = store::DEFAULT_SHARD_BUCKET_CAP)]
        shard_bucket_cap: usize,
    },
    /// Reclaim the sealed shard body from the staging tree, but only where the
    /// archive provably holds every byte (ADR-020 Phase 4).
    ///
    /// The stage is an unbounded local full copy; nothing ever shrinks it.
    /// This command retires the shard body once every declared destination
    /// proves it holds each session. Proof means asking the archive itself —
    /// never the local cursor — for every session's digest triple (shard
    /// count / concatenated bytes / concatenated sha256) and matching it
    /// against the stage. A destination that cannot be consulted is
    /// "unproven" and blocks the reclaim: it is never treated as "does not
    /// have it". Every declared destination must prove it holds each session,
    /// or
    /// nothing is deleted.
    ///
    /// Default is a dry run that reports what would be reclaimed and deletes
    /// nothing; pass `--apply` to actually remove the body. `--apply` writes
    /// each machine's retained summaries first, then deletes session by
    /// session, keeping each session's persistent `shard-seq` counter so the
    /// next sequence never falls back onto archived shard names.
    ///
    /// Exit codes: 0 = reclaimable (dry run) or reclaimed (apply); 1 =
    /// blocked — at least one destination is unreachable, partial, or fails
    /// to hold a session, so nothing was deleted; 2 = usage error.
    ReclaimStage {
        /// Stage directory that holds the sealed `sessions/` tree.
        #[arg(long)]
        stage: PathBuf,
        /// Actually delete the proven shard body. Default: dry run (report only).
        #[arg(long)]
        apply: bool,
        /// Repository path override (single-destination config only).
        #[arg(long)]
        repo: Option<String>,
        /// Masterkey file override (single-destination config only).
        #[arg(long)]
        key_file: Option<String>,
        /// Concurrency cap override (single-destination config only).
        #[arg(long)]
        connections: Option<usize>,
        /// Backend option `key=value`, repeatable (single-destination config only).
        #[arg(long = "option")]
        options: Vec<String>,
        /// Keep the ssh ControlMaster processes open after this run (do not shut them down).
        #[arg(long)]
        keep_ssh_masters: bool,
    },
    /// Register this executable as the browsers' Native Messaging host
    /// (ADR-014 step 2) — or, with `--uninstall`, remove that registration.
    ///
    /// ADR-014 exists because `chrome.downloads` with `saveAs: false` is
    /// overridden by the browser-level "ask where to save each file before
    /// downloading" preference, which no extension flag can suppress. At up to
    /// 200 backfilled conversations a day that is 200 modal dialogs.
    ///
    /// This is one command instead of the five physical actions Native
    /// Messaging otherwise costs: it renders the host manifest (`name` /
    /// `description` / the absolute `path` of *this* executable / `type:
    /// stdio` / the pinned extension allowlist) and drops it into each
    /// installed browser's discovery directory. No elevation: everything is
    /// per-user.
    ///
    /// Every path written, left alone, skipped or removed is printed
    /// absolutely. "No error" is not the same claim as "a file landed".
    ///
    /// Idempotent: run it twice and there is exactly one manifest per browser,
    /// byte-identical, exit 0 both times. Exit codes: 0 = at least one manifest
    /// is in place (or, for `--uninstall`, removal finished), 3 = nothing was
    /// written because no known browser was found, 2 = usage error (bad host
    /// name / extension id / relative binary path), 1 = an action failed.
    InstallNativeHost {
        /// Browser to register with, repeatable. Default: every browser whose
        /// data directory exists on this machine. Naming one explicitly writes
        /// it even when that directory is absent.
        #[arg(long = "browser", value_enum)]
        browsers: Vec<nativehost::Browser>,
        /// Discovery root override (macOS default: `~/Library/Application
        /// Support`). This is what makes the command testable without writing
        /// into a real browser directory.
        #[arg(long)]
        target_root: Option<PathBuf>,
        /// Which OS path layout to use. Default: this platform.
        #[arg(long, value_enum)]
        platform: Option<nativehost::Platform>,
        /// Host binary recorded in the manifest. Default: this executable
        /// (`std::env::current_exe()`), which is the only value that stays
        /// correct after `cargo install --force`.
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Host name override. Chromium's grammar is `[a-z0-9_.]` only.
        #[arg(long, default_value = nativehost::HOST_NAME)]
        host_name: String,
        /// Chrome/Chromium/Edge/Brave/Vivaldi extension id.
        #[arg(long, default_value = nativehost::CHROME_EXTENSION_ID)]
        extension_id: String,
        /// Firefox add-on id (`browser_specific_settings.gecko.id`).
        #[arg(long, default_value = nativehost::FIREFOX_EXTENSION_ID)]
        firefox_extension_id: String,
        /// Remove the manifests this command writes — and nothing else.
        #[arg(long)]
        uninstall: bool,
        /// Windows only: skip the `reg.exe` step and only write the JSON.
        #[arg(long)]
        no_registry: bool,
        /// Stage the browser-spawned host is allowed to write to. Recorded in
        /// the config as `[native_host] stage`. The directory must already
        /// exist and be a directory: the host never creates a stage, and a
        /// stage that appears because a host was pointed at it is a stage
        /// nothing pushes. The edit preserves the config file's comments and
        /// everything else in it.
        #[arg(long)]
        stage: Option<PathBuf>,
    },
    /// The Native Messaging host process itself.
    ///
    /// Runs the same one-request-one-response loop the browser launch uses:
    /// reads one length-prefixed request frame from stdin, writes one response
    /// frame to stdout, exits 0. Useful for driving the host by hand before any
    /// browser is involved. Each process serves exactly one request — the
    /// browser starts one process per message, and the host is built for that.
    ///
    /// Nothing but that one frame may ever reach stdout: Chromium reads stdout
    /// as a `u32` frame length, so a stray log line is read as a multi-gigabyte
    /// frame and kills the pipe. Diagnostics go to stderr. A frame that ends
    /// early (EOF inside the length prefix or the body) is answered with
    /// silence and a non-zero exit, never with a response.
    ///
    /// `--self-test` prints one line of JSON on stdout and exits 0, unchanged:
    /// it answers "does the host process start at all?" without needing a
    /// request.
    NativeHost {
        /// Print one line of JSON describing this host, then exit 0.
        #[arg(long)]
        self_test: bool,
    },
    /// Build the activity sidecar index for one machine partition (ADR-017).
    ///
    /// Walks `<stage>/sessions/<machine>/<session>/<bucket>/NNNNNN.jsonl`,
    /// concatenates each session's shards in global sequence order, and feeds
    /// every line through the conversation-time extractor to produce one
    /// `ActivityRow` per session. The rows are written as JSONL to
    /// `<stage>/meta/<machine>/activity-v1.jsonl` — the file the `overview`
    /// command reads back out of a destination.
    ///
    /// With `--rebuild --destination`, every archived session for that machine
    /// is restored to a temporary child of `--stage`, the full index is
    /// rebuilt, and a new complete snapshot is appended. Existing snapshots
    /// remain immutable. No snapshot is written until the scan and every
    /// session restore finish, and only the named machine partition is pushed.
    /// The rebuild is safe to re-run, but each run starts over and is not
    /// resumable. A same-machine snapshot found before publication restarts the
    /// rebuild so the appended snapshot cannot hide that newer session.
    ///
    /// The harness label is inferred from the session directory name: the
    /// canonical archived ids are `<source>.<machine>.<native-id>`, so the
    /// harness is the leading dot-segment (`claude-code.<m>.<uuid>` ->
    /// `claude-code`); short forms are handled too (`opencode~abc123` ->
    /// `opencode`, `cursor.d~xxx` -> `cursor`).
    ///
    /// Metadata, with one declared exception: the extraction keeps a timestamp
    /// per line and throws the line away, and — for claude-code sessions —
    /// additionally keeps the capped one-line label the dashboard lists the
    /// session under (the harness's own title, or the head of the session's
    /// first user line; see the `ui` labels). That label — at most 100
    /// characters, with its provenance — is the one piece of
    /// conversation-derived text the metadata tier ever holds. Nothing else is
    /// ever kept, and the report is counts and the output path only.
    ///
    /// Exit codes: `0` = the whole partition was read and the index written;
    /// `3` = the stage could not be read (or reading was interrupted) — nothing
    /// (complete) was written; `1` = every session was read but the index file
    /// itself could not be written; `2` = usage error. Same family as
    /// `collect`/`search`: `3` means "did not finish / never started", `1`
    /// means "finished and then failed".
    ActivityIndex {
        /// Local stage directory, or an existing workspace directory for a
        /// destination rebuild. Temporary restored shards are removed after
        /// the new snapshot is written.
        #[arg(long)]
        stage: Option<PathBuf>,
        /// Machine partition for `sessions/<machine>/…`.
        /// Default: config `machine`, else this machine's identity (generated
        /// on first use); use `--machine` explicitly to choose the partition.
        #[arg(long)]
        machine: Option<String>,
        /// Explicitly rebuild the complete partition index. The command has
        /// always rebuilt rather than incrementally patched; this spelling is
        /// provided for repair workflows and makes that intent visible.
        #[arg(long)]
        rebuild: bool,
        /// Destination to rebuild from and append the repaired snapshot to.
        /// Requires `--rebuild`; either this or `--repo` must be named.
        #[arg(long)]
        destination: Option<String>,
        /// Repository path override for destination rebuilds.
        #[arg(long)]
        repo: Option<String>,
        /// Masterkey file override for destination rebuilds.
        #[arg(long)]
        key_file: Option<String>,
        /// Concurrency cap override for destination rebuilds.
        #[arg(long)]
        connections: Option<usize>,
        /// Backend option `key=value`, repeatable.
        #[arg(long = "option")]
        options: Vec<String>,
        /// Keep ssh ControlMaster processes open after this run.
        #[arg(long)]
        keep_ssh_masters: bool,
    },
    /// Write this machine's display-name declaration (ADR-018).
    ///
    /// Writes `<stage>/meta/<machine>/machine.json` — a `MachineDeclaration`
    /// that carries the human display name, the OS and a first-seen time. The
    /// machine partition is resolved exactly like every other command: config
    /// `machine` first, then the identity file (generated when missing). The
    /// next `push` archives the whole stage root, so this file rides along.
    ///
    /// The display name is taken from `--display-name` when given; otherwise
    /// the system's LocalHostName (`scutil --get LocalHostName` on macOS,
    /// `hostname` elsewhere) is used. If neither is available the field is
    /// written **empty** and the command says so — a name is never invented.
    ///
    /// Exit codes: `0` = the declaration was written; `3` = the machine
    /// partition could not be resolved or the identity is unusable; `1` = the
    /// file could not be written; `2` = usage error.
    MachineDeclare {
        /// Stage directory to write the declaration into (`<stage>/meta/…`).
        #[arg(long)]
        stage: PathBuf,
        /// Human display name for this machine. Default: system LocalHostName.
        #[arg(long)]
        display_name: Option<String>,
    },
    /// Give a dead or sold machine a display name (ADR-018).
    ///
    /// Writes `<stage>/meta/<target>/label-by-<local-identity>.json` — a
    /// `LabelRecord` expressing *this* machine's opinion about what `<target>`
    /// should be called. It exists for machines that can no longer declare for
    /// themselves (sold, dead, in a different building). Each file has exactly
    /// one writer, so concurrent machines never fight over it; the overview
    /// shows the label with the latest `written_at_unix`.
    ///
    /// Exit codes: `0` = the label was written; `3` = the local identity could
    /// not be resolved; `1` = the file could not be written; `2` = usage error.
    MachineLabel {
        /// Stage directory to write the label into (`<stage>/meta/…`).
        #[arg(long)]
        stage: PathBuf,
        /// Machine id (partition) being labelled, e.g. a 32-hex identity.
        #[arg(long)]
        target: String,
        /// Display-name override for the target.
        #[arg(long)]
        label: String,
    },
    /// Draw the machine × harness activity overview of one destination
    /// (ADR-017).
    ///
    /// Opens the destination repository, takes each host's newest snapshot
    /// (the same cross-machine merge as `read --all-machines`), and for every
    /// snapshot recursively finds the activity index files at
    /// `meta/<machine>/activity-v1.jsonl`. The paths carry each machine's own
    /// absolute stage prefix, so they are matched by the trailing
    /// `meta` / `<machine>` / `activity-v1.jsonl` marker — never by a local
    /// path. The indexes are parsed into overview rows and rendered as a
    /// machine × harness matrix, a time-unknown tally and a width-adaptive
    /// heatmap. Prints counts, machine names and time spans only — never
    /// conversation content.
    ///
    /// A machine that has a snapshot but **no** activity index is listed
    /// explicitly as "index missing" — it never vanishes silently, because a missing
    /// index is not the same thing as "that machine had no sessions".
    ///
    /// Exit codes: `0` = at least one index was read and rendered; `1` = the
    /// whole repository was read and there is no activity index at all (a real
    /// "there is none", not a failure to look); `3` = the repository could not
    /// be read in full (open / snapshot / tree / dump failure) — the absences
    /// below are unproven; `2` = usage error.
    Overview {
        /// Destination to open. Required unless an explicit `--repo` is given:
        /// there is no default destination.
        #[arg(long)]
        destination: Option<String>,
        /// Terminal width for the matrix/heatmap (default: 100).
        #[arg(long, default_value_t = 100)]
        width: usize,
        /// Print exactly one JSON object to stdout and nothing else (the
        /// human matrix/heatmap is suppressed). Per-session `first_unix` /
        /// `last_unix` and `time_source` are tri-state tagged
        /// `{"kind":"known",…}` / `{"kind":"unknown","why":…}` — an unknown
        /// time is never `null`. `no_index_anywhere` is `true` on exit 1
        /// (read in full, no index anywhere); a failed read (exit 3) puts the
        /// reason in `error`.
        #[arg(long)]
        json: bool,
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
        /// Keep the ssh ControlMaster processes open after this run (do not shut them down).
        #[arg(long)]
        keep_ssh_masters: bool,
    },
    /// Manage this machine's body cache (ADR-034).
    ///
    /// The body cache holds the conversation bodies a read would otherwise pull
    /// from a destination again, as the destination's own ciphertext. It is
    /// disposable by design: nothing here is part of the archive, and deleting
    /// all of it changes nothing except how long the next read takes. Without a
    /// subcommand it prints the location, the quota and the current occupancy,
    /// so a cache you never meant to have is visible before you clear it.
    Cache {
        /// What to do; omit to see the current occupancy.
        #[command(subcommand)]
        action: Option<CacheAction>,
    },
}

/// `cache` subcommands.
#[derive(Clone, Copy, clap::Subcommand)]
enum CacheAction {
    /// Delete every cached body block.
    ///
    /// Only the cache directory is touched — never the archive, never a
    /// destination, never a key. Entries are re-fetched from the destination
    /// the next time a session is read.
    Clear,
}

/// export `--turns` selector. A value enum rather than a string so an unknown
/// value is a clap usage error (exit 2) instead of being quietly read as
/// `all` — a filter that silently does nothing is the failure this flag exists
/// to avoid.
#[derive(Clone, Copy, clap::ValueEnum)]
enum TurnsArg {
    All,
    User,
}

impl TurnsArg {
    fn to_turns(self) -> chat_stasher::export::Turns {
        match self {
            TurnsArg::All => chat_stasher::export::Turns::All,
            TurnsArg::User => chat_stasher::export::Turns::User,
        }
    }
}

/// verify `--level` selector.
#[derive(Clone, Copy, clap::ValueEnum)]
enum VerifyLevel {
    L1,
    L2,
    L3,
    All,
}

/// The remote kinds `setup` can spell out by itself (WIZ-3, ADR-039 step 3).
///
/// A value enum for the same reason [`TurnsArg`] is one: an unknown kind is a
/// clap usage error (exit 2) rather than a silent fallback to whichever recipe
/// the code happens to match first. Which of these is *recommended* is not
/// decided here — see [`SETUP_RECOMMENDED_REMOTE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum SetupRemoteKind {
    /// SSH/SFTP: any host reachable with `ssh` that gives you a path.
    Sftp,
    /// S3-compatible object storage. Cloudflare R2 is the instance this project
    /// has tested end to end (ADR-036 / W121); the recipe is the documented one
    /// at `docs/install.md` §4.5.
    S3,
}

impl SetupRemoteKind {
    /// Stable slug for the terminal and for `--json`.
    fn slug(self) -> &'static str {
        match self {
            Self::Sftp => "sftp",
            Self::S3 => "s3",
        }
    }

    /// The `repo` string the backend is selected by. Written into the config.
    fn repo(self) -> &'static str {
        match self {
            Self::Sftp => "opendal:sftp",
            Self::S3 => "opendal:s3",
        }
    }

    /// One line of prose, used when the candidates are listed.
    fn blurb(self) -> &'static str {
        match self {
            Self::Sftp => "SSH/SFTP — any host you can reach with ssh and write a path on",
            Self::S3 => {
                "S3-compatible object storage — Cloudflare R2 is the service this project has \
                 tested end to end"
            }
        }
    }

    /// The parameters this kind cannot be written without, in the order they are
    /// asked for. Names match the clap argument ids, so a report of what is
    /// missing and the flag that would supply it are the same word.
    fn required_parameters(self) -> &'static [&'static str] {
        match self {
            Self::Sftp => &["remote_endpoint", "remote_user"],
            Self::S3 => &[
                "remote_endpoint",
                "remote_bucket",
                "remote_access_key_id_env",
                "remote_secret_key_env",
            ],
        }
    }
}

/// The remote the wizard recommends, in one place.
///
/// ADR-039 decision 3 makes this a *switchable* fact rather than prose: before
/// ADR-036 (W121's R2 acceptance) passed, the recommended remote was SFTP and R2
/// was marked "being verified"; once it passed, R2 became the recommendation,
/// with the other candidates staying explicitly selectable.
///
/// W121 is in main (merge `930722e`) and its acceptance is recorded in
/// `nm/W121-OUT.md:160-166` (seed / push / read / verify green against R2), so
/// the switch is on the S3 recipe. Nothing user-facing names a recommendation of
/// its own — if decision 3 is reversed, this constant is the only line that
/// changes and every sentence that mentions the recommendation reads it.
const SETUP_RECOMMENDED_REMOTE: SetupRemoteKind = SetupRemoteKind::S3;

/// The parameters of the remote step, exactly as the caller supplied them.
///
/// Every field here is either a name or a location. **No field can carry a
/// secret**, and that is the point of the shape rather than an accident of it:
/// the two credential fields are the *names of environment variables*, which the
/// wizard writes into the config as `env:NAME` and never resolves itself. So a
/// secret cannot reach the config file as plaintext, and it cannot reach
/// `argv` — where it would be captured by the process list and by any log that
/// echoes the command — because there is no flag that would accept one.
#[derive(Debug, Default, Clone, clap::Args)]
struct SetupRemoteArgs {
    /// Which recipe to write.
    #[arg(long, value_enum, value_name = "KIND")]
    remote: Option<SetupRemoteKind>,
    /// SFTP: `ssh://<host>:<port>`. S3: the service endpoint, e.g.
    /// `https://<account-id>.r2.cloudflarestorage.com`.
    #[arg(long, value_name = "VALUE")]
    remote_endpoint: Option<String>,
    /// SFTP only: the ssh user.
    #[arg(long, value_name = "USER")]
    remote_user: Option<String>,
    /// SFTP only: the private key to authenticate with. Optional — leaving it
    /// out lets the backend use the ssh agent and its own default keys, which is
    /// a real configuration rather than a gap.
    #[arg(long, value_name = "PATH")]
    remote_ssh_key: Option<String>,
    /// S3 only: the bucket (or container) the repository lives in.
    #[arg(long, value_name = "BUCKET")]
    remote_bucket: Option<String>,
    /// S3 only: the region. R2 is single-region and names it `auto`; another
    /// S3-compatible service names its own.
    #[arg(long, value_name = "REGION", default_value_t = String::from("auto"))]
    remote_region: String,
    /// Both kinds: the prefix inside the account/bucket that the repository
    /// lives under. Optional, and omitted rather than defaulted: a prefix the
    /// user did not choose would put their archive somewhere they did not name.
    #[arg(long, value_name = "PREFIX")]
    remote_root: Option<String>,
    /// S3 only: **the name of** the environment variable holding the access key
    /// id. Not the key id itself.
    #[arg(long, value_name = "VAR")]
    remote_access_key_id_env: Option<String>,
    /// S3 only: **the name of** the environment variable holding the secret
    /// access key. Not the secret.
    #[arg(long, value_name = "VAR")]
    remote_secret_key_env: Option<String>,
    /// Declare that this host's key was checked out of band, which authorizes
    /// recording it in `~/.ssh/known_hosts`. A declaration, not a verification:
    /// nothing here, and nothing anywhere else, can check that the comparison
    /// happened. Without it the wizard stops at an unknown host and writes
    /// nothing (ADR-039, rejected option E).
    #[arg(long)]
    trust_host: bool,
}

/// Stack reserved for the thread that does all the real work.
///
/// Windows gives the main thread 1 MiB; Unix gives 8. That difference is not
/// academic here: `#[derive(Parser)]` expands into a builder chain that walks
/// every subcommand and every argument, and in a debug build — no inlining, no
/// stack-slot reuse — that chain alone measures ~944 KiB on arm64. Measured
/// 2026-08-28 by lowering `ulimit -s`: the debug binary survives 944 KiB and
/// dies at 900, and `--version` dies at exactly the same threshold as `push`,
/// which is what proves the cost is argument parsing rather than any command's
/// own work. On Windows CI the same chain sat just over the 1 MiB line and the
/// process died with STATUS_STACK_OVERFLOW before `main` could print anything.
///
/// A release build needs roughly a seventh of that (survives 256 KiB, dies at
/// 128), so shipped binaries were never at risk — but every Windows test that
/// invokes the binary runs the debug build, so Windows CI could not be green.
///
/// Growing the stack is the fix rather than trimming subcommands because the
/// requirement scales with the CLI surface, and a CLI that must stay small to
/// keep running is a constraint nobody would remember. 16 MiB leaves room for
/// the parser chain to double and still not be the thing that breaks.
const WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

fn main() -> ExitCode {
    // Everything runs on a thread we size ourselves, so the platform's main
    // thread limit stops being part of the contract. `join()` propagates a
    // panic by re-panicking here, which keeps the existing panic behaviour
    // (message on stderr, non-zero exit) rather than swallowing it.
    match std::thread::Builder::new()
        .name("chat-stasher".to_string())
        .stack_size(WORKER_STACK_BYTES)
        .spawn(run)
    {
        Ok(handle) => match handle.join() {
            Ok(code) => code,
            Err(panic) => std::panic::resume_unwind(panic),
        },
        // Spawning can fail when the OS refuses the thread (hitting a process
        // or memory limit). Falling back to the main thread is better than
        // dying: the smaller stack is only fatal for debug builds, and a
        // release build has ample room.
        Err(err) => {
            eprintln!(
                "warning: could not start the {} MiB worker thread ({err}); \
                 continuing on the main thread, which may overflow on Windows",
                WORKER_STACK_BYTES / (1024 * 1024)
            );
            run()
        }
    }
}

fn run() -> ExitCode {
    // Before clap, not after: a browser starts this binary with no subcommand
    // and with arguments (an origin, or a manifest path and an add-on id) that
    // clap would reject as unknown. `nativehost-protocol.md` §3 makes this
    // recognition the *first* thing the process does, and the two refusals here
    // are refusals to be a host — never a fallthrough into command parsing,
    // which would answer a browser with a usage error on stderr and nothing at
    // all on stdout.
    match nativehost::detect_launch(&std::env::args_os().collect::<Vec<_>>()) {
        nativehost::Launch::Host => return nativehost::serve_stdin(),
        nativehost::Launch::Foreign { origin } => {
            eprintln!(
                "refusing to serve {origin}: this build is the Native Messaging host for \
                 chrome-extension://{}/ and {}",
                nativehost::CHROME_EXTENSION_ID,
                nativehost::FIREFOX_EXTENSION_ID
            );
            return ExitCode::from(2);
        }
        nativehost::Launch::CommandLine => {}
    }

    let cli = Cli::parse();
    match cli.command {
        Command::Init => cmd_init(),
        Command::Setup {
            stage,
            destination,
            install_schedule,
            masterkey_saved_elsewhere,
            json,
            remote,
        } => cmd_setup(
            stage,
            destination,
            install_schedule,
            masterkey_saved_elsewhere,
            json,
            remote,
        ),
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
            keep_ssh_masters,
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
            keep_ssh_masters,
        ),
        Command::Schedule {
            action,
            unit,
            format,
            stage,
            output,
            binary,
            destination,
            repo,
            key_file,
            connections,
            options,
            machine,
            shard_bucket_cap,
            verify,
            keep_ssh_masters,
        } => cmd_schedule(
            action,
            unit,
            format,
            stage.as_deref(),
            output,
            binary,
            destination,
            repo,
            key_file,
            connections,
            options,
            machine,
            shard_bucket_cap,
            verify,
            keep_ssh_masters,
        ),
        Command::Push {
            stage,
            inbox,
            destination,
            repo,
            key_file,
            machine,
            connections,
            options,
            keep_ssh_masters,
        } => cmd_push(
            &stage,
            inbox,
            destination,
            repo,
            key_file,
            machine,
            connections,
            &options,
            keep_ssh_masters,
        ),
        Command::Status {
            sessions,
            json,
            destination,
            repo,
            key_file,
            connections,
            options,
            keep_ssh_masters,
        } => cmd_status(
            sessions,
            json,
            destination,
            repo,
            key_file,
            connections,
            &options,
            keep_ssh_masters,
        ),
        Command::Read {
            stage,
            session,
            all_machines,
            full_ids,
            machine,
            destination,
            repo,
            key_file,
            connections,
            options,
            keep_ssh_masters,
        } => cmd_read(
            &stage,
            &session,
            all_machines,
            full_ids,
            machine.as_deref(),
            destination,
            repo,
            key_file,
            connections,
            &options,
            keep_ssh_masters,
        ),
        Command::Doctor { json } => cmd_doctor(json),
        Command::Cache { action } => cmd_cache(action),
        Command::Verify {
            level,
            stage,
            full_ids,
            machine,
            destination,
            repo,
            key_file,
            connections,
            options,
            keep_ssh_masters,
        } => cmd_verify(
            level,
            &stage,
            full_ids,
            machine.as_deref(),
            destination,
            repo,
            key_file,
            connections,
            &options,
            keep_ssh_masters,
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
            keep_ssh_masters,
            trust_host,
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
            keep_ssh_masters,
            trust_host,
        ),
        Command::Search {
            destination,
            filters,
            json,
            cost,
            repo,
            key_file,
            connections,
            options,
            keep_ssh_masters,
        } => cmd_search(
            destination,
            &filters,
            json,
            cost,
            repo,
            key_file,
            connections,
            &options,
            keep_ssh_masters,
        ),
        Command::Export {
            destination,
            out,
            filters,
            turns,
            trim_to_window,
            force,
            dry_run,
            repo,
            key_file,
            connections,
            options,
            keep_ssh_masters,
        } => cmd_export(
            destination,
            &out,
            &filters,
            turns,
            trim_to_window,
            force,
            dry_run,
            repo,
            key_file,
            connections,
            &options,
            keep_ssh_masters,
        ),
        Command::Ui(args) => cmd_ui(args, None),
        Command::View(args) => cmd_ui(
            args,
            Some(
                "view: `chat-stasher view` is deprecated and will be removed in the next release; \
                 it is now an alias for `chat-stasher ui`.",
            ),
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
        Command::ReclaimStage {
            stage,
            apply,
            repo,
            key_file,
            connections,
            options,
            keep_ssh_masters,
        } => cmd_reclaim_stage(
            &stage,
            apply,
            repo,
            key_file,
            connections,
            &options,
            keep_ssh_masters,
        ),
        Command::InstallNativeHost {
            browsers,
            target_root,
            platform,
            binary,
            host_name,
            extension_id,
            firefox_extension_id,
            uninstall,
            no_registry,
            stage,
        } => cmd_install_native_host(
            &browsers,
            target_root,
            platform,
            binary,
            &host_name,
            &extension_id,
            &firefox_extension_id,
            uninstall,
            no_registry,
            stage,
        ),
        Command::NativeHost { self_test } => cmd_native_host(self_test),
        Command::ActivityIndex {
            stage,
            machine,
            rebuild,
            destination,
            repo,
            key_file,
            connections,
            options,
            keep_ssh_masters,
        } => cmd_activity_index(
            stage.as_deref(),
            machine.as_deref(),
            rebuild,
            destination,
            repo,
            key_file,
            connections,
            &options,
            keep_ssh_masters,
        ),
        Command::MachineDeclare {
            stage,
            display_name,
        } => cmd_machine_declare(&stage, display_name),
        Command::MachineLabel {
            stage,
            target,
            label,
        } => cmd_machine_label(&stage, &target, &label),
        Command::Overview {
            destination,
            width,
            json,
            repo,
            key_file,
            connections,
            options,
            keep_ssh_masters,
        } => cmd_overview(
            destination,
            width,
            json,
            repo,
            key_file,
            connections,
            &options,
            keep_ssh_masters,
        ),
    }
}

/// `install-native-host` — ADR-014 step 2.
///
/// The shape of this function is dictated by one house rule: nothing is
/// written silently. Every target is printed with what happened to it and
/// where, including the ones that were skipped and why, and the summary line
/// counts them. A caller that reads only the exit code still gets the truth,
/// but a human reading the output gets the paths.
#[allow(clippy::too_many_arguments)]
fn cmd_install_native_host(
    browsers: &[nativehost::Browser],
    target_root: Option<PathBuf>,
    platform: Option<nativehost::Platform>,
    binary: Option<PathBuf>,
    host_name: &str,
    extension_id: &str,
    firefox_extension_id: &str,
    uninstall: bool,
    no_registry: bool,
    stage: Option<PathBuf>,
) -> ExitCode {
    const TAG: &str = "[install-native-host]";

    // `--stage` is validated before anything at all is written, and before the
    // banner: a path that is not a directory is a mistake in the invocation,
    // and a command that had already rewritten a browser manifest by the time
    // it noticed would have left the machine in a state the user did not ask
    // for. Nothing here creates the directory — a stage is the user's to make.
    let stage_to_record: Option<PathBuf> = match &stage {
        None => None,
        Some(_) if uninstall => {
            eprintln!(
                "install-native-host: --stage cannot be combined with --uninstall; \
                 nothing was written and the config was not changed"
            );
            return ExitCode::from(2);
        }
        Some(path) => {
            let expanded = match config::expand_and_verify(&path.to_string_lossy()) {
                Ok(path) => path,
                Err(e) => {
                    eprintln!("install-native-host: --stage: {e}");
                    return ExitCode::from(2);
                }
            };
            let resolved = absolute_path(&expanded);
            match fs::metadata(&resolved) {
                Ok(meta) if meta.is_dir() => Some(resolved),
                Ok(_) => {
                    eprintln!(
                        "install-native-host: --stage {} exists but is not a directory; \
                         nothing was written and the config was not changed",
                        resolved.display()
                    );
                    return ExitCode::from(2);
                }
                Err(e) => {
                    eprintln!(
                        "install-native-host: --stage {} is not usable: {e}; \
                         nothing was written and the config was not changed",
                        resolved.display()
                    );
                    return ExitCode::from(2);
                }
            }
        }
    };

    let platform = platform.unwrap_or_else(nativehost::Platform::current);
    let root = match target_root {
        Some(root) => absolute_path(&root),
        // The machine's root, not the home-derived one: on Windows the
        // `LocalAppData` known folder is authoritative and a profile may have
        // redirected it away from `<home>\AppData\Local`.
        None => nativehost::machine_root(platform, &config::home_dir()),
    };
    let binary = match binary {
        Some(path) => absolute_path(&path),
        None => match std::env::current_exe() {
            Ok(path) => path,
            Err(e) => {
                eprintln!("install-native-host: cannot resolve current executable: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    // Explicitly named browsers are written even when their data directory is
    // absent; the default set is "whatever is actually installed here".
    let explicit = !browsers.is_empty();
    let selection: Vec<nativehost::Browser> = if explicit {
        let mut chosen = browsers.to_vec();
        chosen.sort();
        chosen.dedup();
        chosen
    } else {
        nativehost::Browser::ALL.to_vec()
    };

    // Render both dialects up front: a bad host name or extension id is a
    // usage error, and it must not be discovered halfway through writing.
    let chromium_manifest = match nativehost::render_manifest(
        nativehost::Family::Chromium,
        host_name,
        &binary,
        extension_id,
        firefox_extension_id,
    ) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("install-native-host: {e:#}");
            return ExitCode::from(2);
        }
    };
    let gecko_manifest = match nativehost::render_manifest(
        nativehost::Family::Gecko,
        host_name,
        &binary,
        extension_id,
        firefox_extension_id,
    ) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("install-native-host: {e:#}");
            return ExitCode::from(2);
        }
    };

    println!(
        "{TAG} mode: {}",
        if uninstall { "uninstall" } else { "install" }
    );
    println!("{TAG} host name: {host_name}");
    println!("{TAG} host binary: {}", binary.display());
    println!("{TAG} platform: {}", platform.id());
    println!("{TAG} discovery root: {}", root.display());
    if !uninstall {
        println!("{TAG} chromium allowlist: chrome-extension://{extension_id}/");
        println!("{TAG} gecko allowlist: {firefox_extension_id}");
    }

    // Before the manifests, because this is the half the host cannot run
    // without: a registration whose host has no stage answers every `deliver`
    // with `nack config`, while a configured stage nobody registered is inert.
    if let Some(path) = &stage_to_record {
        let value = path.to_string_lossy().into_owned();
        match Config::set_native_host_stage(&value) {
            Ok(config::StageKeyWrite::Unchanged) => println!(
                "{TAG} stage: {value} already recorded in {} — config not rewritten",
                config::config_path().display()
            ),
            Ok(config::StageKeyWrite::Added) => println!(
                "{TAG} stage: recorded {value} in {}",
                config::config_path().display()
            ),
            Ok(config::StageKeyWrite::Updated { previous }) => println!(
                "{TAG} stage: {} was {previous}, is now {value}",
                config::config_path().display()
            ),
            Err(e) => {
                eprintln!("install-native-host: cannot record --stage: {e:#}");
                return ExitCode::FAILURE;
            }
        }
    }

    let mut wrote = 0usize;
    let mut updated = 0usize;
    let mut unchanged = 0usize;
    let mut skipped = 0usize;
    let mut removed = 0usize;
    let mut absent = 0usize;
    let mut unsupported = 0usize;
    let mut failed = 0usize;
    let mut registered: Vec<nativehost::Target> = Vec::new();

    for browser in selection {
        let Some(target) = nativehost::target(platform, &root, browser, host_name) else {
            unsupported += 1;
            println!(
                "{TAG} {}: no discovery path known for {} in this build — nothing written",
                browser.id(),
                platform.id()
            );
            continue;
        };
        if uninstall {
            match nativehost::remove_one(&target) {
                Ok(nativehost::RemoveOutcome::Removed) => {
                    removed += 1;
                    println!(
                        "{TAG} {}: removed {}",
                        browser.id(),
                        target.manifest.display()
                    );
                    registered.push(target);
                }
                Ok(nativehost::RemoveOutcome::Absent) => {
                    absent += 1;
                    println!(
                        "{TAG} {}: absent (nothing to remove) {}",
                        browser.id(),
                        target.manifest.display()
                    );
                    registered.push(target);
                }
                Err(e) => {
                    failed += 1;
                    eprintln!("install-native-host: {}: {e:#}", browser.id());
                }
            }
            continue;
        }
        let content = match browser.family() {
            nativehost::Family::Chromium => &chromium_manifest,
            nativehost::Family::Gecko => &gecko_manifest,
        };
        match nativehost::install_one(&target, content, explicit) {
            Ok(nativehost::InstallOutcome::SkippedBrowserAbsent) => {
                skipped += 1;
                let probe = target
                    .profile_root
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                println!(
                    "{TAG} {}: skipped, browser not installed (no {}) — pass --browser {} to write anyway",
                    browser.id(),
                    probe,
                    browser.id()
                );
            }
            Ok(outcome) => {
                match outcome {
                    nativehost::InstallOutcome::Written => wrote += 1,
                    nativehost::InstallOutcome::Updated => updated += 1,
                    _ => unchanged += 1,
                }
                println!(
                    "{TAG} {}: {} {}",
                    browser.id(),
                    outcome.id(),
                    target.manifest.display()
                );
                registered.push(target);
            }
            Err(e) => {
                failed += 1;
                eprintln!("install-native-host: {}: {e:#}", browser.id());
            }
        }
    }

    // Windows registration. The manifest file alone is not discoverable there:
    // the browser reads a per-user registry value that points at it. UNVERIFIED
    // on real hardware — no Windows machine was available — so when this is not
    // Windows the commands are printed rather than claimed.
    if matches!(platform, nativehost::Platform::Windows) && !no_registry {
        for target in &registered {
            let Some(command) = nativehost::registry_command(
                target.browser,
                host_name,
                &target.manifest,
                uninstall,
            ) else {
                println!(
                    "{TAG} {}: no registry key known in this build — manifest written but NOT discoverable",
                    target.browser.id()
                );
                continue;
            };
            if cfg!(target_os = "windows") {
                match nativehost::apply_registry(&command) {
                    Ok(()) => println!(
                        "{TAG} {}: registry {}",
                        target.browser.id(),
                        command.display()
                    ),
                    Err(e) => {
                        failed += 1;
                        eprintln!("install-native-host: {}: {e:#}", target.browser.id());
                    }
                }
            } else {
                println!(
                    "{TAG} {}: registry NOT applied (not running on Windows), would run: {}",
                    target.browser.id(),
                    command.display()
                );
            }
        }
    }

    if uninstall {
        println!("{TAG} summary: removed {removed}, already absent {absent}, unsupported {unsupported}, failed {failed}");
        println!("{TAG} directories themselves were left in place; no other vendor's manifest was touched.");
        if failed > 0 {
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    let live = wrote + updated + unchanged;
    println!("{TAG} summary: wrote {wrote}, updated {updated}, unchanged {unchanged}, skipped {skipped}, unsupported {unsupported}, failed {failed}");
    if failed > 0 {
        return ExitCode::FAILURE;
    }
    if live == 0 {
        if stage_to_record.is_some() {
            eprintln!(
                "install-native-host: no browser manifest was written — no known browser \
                 directory under {} (the config stage key above was written)",
                root.display()
            );
        } else {
            eprintln!(
                "install-native-host: nothing was written — no known browser directory under {}",
                root.display()
            );
        }
        return ExitCode::from(3);
    }
    println!("{TAG} next: reload the extension, then connect to {host_name}.");
    ExitCode::SUCCESS
}

/// `native-host` — the host process.
///
/// Without `--self-test` this serves one request on stdin and answers it on
/// stdout, which is exactly what the browser launch path does
/// ([`nativehost::serve_stdin`]). It exists so the loop can be driven by hand —
/// `printf` a frame into it — without registering anything with a browser; a
/// second implementation for manual testing would be a second thing to be wrong.
///
/// `--self-test` is unchanged: one line of JSON, exit 0.
fn cmd_native_host(self_test: bool) -> ExitCode {
    if self_test {
        // Exactly one line, on stdout, and nothing else ever.
        println!(
            "{}",
            nativehost::self_test_line(nativehost::HOST_NAME, env!("CARGO_PKG_VERSION"))
        );
        return ExitCode::SUCCESS;
    }
    nativehost::serve_stdin()
}

/// Outcome of a full activity-index rebuild for one machine partition.
#[derive(Debug)]
struct ActivityIndexOutcome {
    /// Number of sessions whose rows were written to the index.
    sessions_indexed: usize,
    /// Number of sessions indexed with an unknown harness.
    sessions_without_harness: usize,
    /// Absolute path of the written index.
    out_path: PathBuf,
    /// Wall-clock time the rebuild took.
    elapsed: std::time::Duration,
}

/// Why an activity-index rebuild failed, in the two failure families the
/// `activity-index` exit-code contract promises: `Read` = reading was
/// interrupted, so no complete index exists (exit 3); `Write` = every session
/// was read but the output could not be written (exit 1).
#[derive(Debug)]
enum ActivityIndexError {
    Read(String),
    Write(String),
}

impl std::fmt::Display for ActivityIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(message) | Self::Write(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ActivityIndexError {}

fn redact_activity_index_paths(
    message: &str,
    cfg: &StoreConfig,
    workspace: Option<&Path>,
) -> String {
    let mut safe = message.to_string();
    for private in [
        Some(cfg.key_file.as_path()),
        workspace,
        Some(Path::new(&cfg.repo_root)),
    ]
    .into_iter()
    .flatten()
    .filter(|path| !path.as_os_str().is_empty())
    {
        safe = safe.replace(&private.to_string_lossy().to_string(), "<private path>");
    }
    if let Some(home) = std::env::var_os("HOME") {
        safe = safe.replace(&home.to_string_lossy().to_string(), "~");
    }
    safe
}

fn redact_local_activity_index_message(message: &str, stage: &Path) -> String {
    redact_activity_index_paths(message, &StoreConfig::default(), Some(stage))
}

/// Rebuild the activity index for one machine partition (ADR-017).
///
/// Reads every sealed shard of every session under
/// `<stage>/sessions/<machine>/`, concatenates each session's shards in global
/// sequence order (across buckets), and hands the lines to `activity::build_row`
/// for conversation-time extraction. One `ActivityRow` per session is written
/// as JSONL to `<stage>/meta/<machine>/activity-v1.jsonl`.
///
/// Deliberately a **full rebuild**, never an incremental update: the index is
/// derived from the stage, so after a full rebuild "the index says" and "the
/// archive holds" can never diverge, while an incremental scheme would have to
/// track exactly which shards changed since the last build — and any bug in
/// that tracking produces a stale index that looks authoritative. The measured
/// cost of a full rebuild is ~6.5 s on a 759-session stage (0.18% of the 3600 s
/// run-once period), which is nothing next to the correctness it buys.
///
/// `print_skips` controls the per-session "no inferable harness" stderr line:
/// `activity-index` wants it (it is part of its existing output), `run-once`
/// only wants the count.
fn rebuild_activity_index(
    stage: &Path,
    machine: &str,
    print_skips: bool,
) -> Result<ActivityIndexOutcome, ActivityIndexError> {
    let started = std::time::Instant::now();
    if !stage.is_dir() {
        return Err(ActivityIndexError::Read(format!(
            "stage `{}` is not a directory — nothing was read",
            stage.display()
        )));
    }
    let sessions_root = stage.join(store::SESSIONS_DIR).join(machine);

    // Enumerate the machine's session directories. A partition that has never
    // been archived is a *complete* read of zero items (empty), not a failure
    // to look; any other read error means we could not read.
    let mut session_dirs: Vec<PathBuf> = Vec::new();
    match fs::read_dir(&sessions_root) {
        Ok(rd) => {
            for session in rd {
                let session = match session {
                    Ok(s) => s,
                    Err(e) => {
                        return Err(ActivityIndexError::Read(format!(
                            "cannot enumerate {}: {e}",
                            sessions_root.display()
                        )));
                    }
                };
                // A dirent whose type cannot be read is not "not a session":
                // skipping it would silently under-count the index. Same read
                // failure family as the entry error above.
                let file_type = match session.file_type() {
                    Ok(ft) => ft,
                    Err(e) => {
                        return Err(ActivityIndexError::Read(format!(
                            "cannot read the type of {}: {e}",
                            session.path().display()
                        )));
                    }
                };
                if file_type.is_dir() {
                    session_dirs.push(session.path());
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(ActivityIndexError::Read(format!(
                "cannot read {}: {e} — nothing was read, this is not an empty index",
                sessions_root.display()
            )));
        }
    }

    let meta_dir = stage.join("meta").join(machine);
    if let Err(e) = fs::create_dir_all(&meta_dir) {
        return Err(ActivityIndexError::Write(format!(
            "cannot create {}: {e}",
            meta_dir.display()
        )));
    }
    let out_path = meta_dir.join("activity-v1.jsonl");
    let mut index_temp = tempfile::Builder::new()
        .prefix(".activity-index-")
        .tempfile_in(&meta_dir)
        .map_err(|e| ActivityIndexError::Write(format!("cannot create activity index: {e}")))?;
    let mut rows_written = 0usize;
    let mut sessions_without_harness = 0usize;
    for session_dir in session_dirs {
        let session_id = session_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            // reason: a read_dir entry always has a file_name; even the impossible
            // empty id falls through infer_harness → None → counted as unknown,
            // so an unknown id is tallied, never silently indexed.
            .unwrap_or_default();
        let harness = sidecar::infer_harness(&session_id).unwrap_or_else(|| {
            sessions_without_harness += 1;
            if print_skips {
                eprintln!(
                    "activity-index: session `{}` has no inferable harness — indexed with unknown time",
                    chat_stasher::id::short_session_id(&session_id)
                );
            }
            "unknown".to_string()
        });

        // Global sequence order across both legacy and bucketed layouts.
        let mut shards = match store::sealed_shard_entries(&session_dir) {
            Ok(shards) => shards,
            Err(e) => {
                return Err(ActivityIndexError::Read(format!(
                    "cannot read session `{session_id}`: {e}"
                )));
            }
        };
        shards.sort_by_key(|(seq, _)| *seq);

        let mut lines: Vec<String> = Vec::new();
        for (_, shard) in shards {
            let bytes = match fs::read(&shard) {
                Ok(b) => b,
                Err(e) => {
                    return Err(ActivityIndexError::Read(format!(
                        "cannot read shard {}: {e}",
                        shard.display()
                    )));
                }
            };
            for line in String::from_utf8_lossy(&bytes).lines() {
                lines.push(line.to_string());
            }
        }
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        let row = activity::build_row(&session_id, machine, &harness, &refs);
        index_temp
            .write_all(activity::to_jsonl(&row).as_bytes())
            .map_err(|e| ActivityIndexError::Write(format!("cannot append activity row: {e}")))?;
        rows_written += 1;
    }

    // A complete read is published only after every session row is on disk.
    // Until persist succeeds, readers keep seeing the previous complete index.
    index_temp
        .as_file()
        .sync_all()
        .map_err(|e| ActivityIndexError::Write(format!("cannot sync activity index: {e}")))?;
    index_temp.persist(&out_path).map_err(|e| {
        ActivityIndexError::Write(format!("cannot publish activity index: {}", e.error))
    })?;

    Ok(ActivityIndexOutcome {
        sessions_indexed: rows_written,
        sessions_without_harness,
        out_path,
        elapsed: started.elapsed(),
    })
}

/// Replace a derived sidecar only after its complete contents have reached
/// disk. The sibling temporary file keeps rename on the same filesystem, and
/// readers see either the old complete index or the new complete index.
fn atomic_replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "output has no parent")
    })?;
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "output has no file name")
    })?;
    let mut temp_name = name.to_os_string();
    temp_name.push(format!(".{}.tmp", std::process::id()));
    let temp_path = parent.join(temp_name);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)
    })();
    if let Err(error) = result {
        return match fs::remove_file(&temp_path) {
            Ok(()) => Err(error),
            Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(error),
            Err(cleanup) => Err(std::io::Error::new(
                error.kind(),
                format!("{error}; temporary file cleanup failed: {cleanup}"),
            )),
        };
    }
    Ok(())
}

/// `activity-index` — build the activity sidecar for one machine partition
/// (ADR-017).
///
/// Thin CLI wrapper over [`rebuild_activity_index`]. Exit codes follow the
/// house family (`collect`/`search`): `0` = the whole partition was read and
/// the index written; `3` = the stage could not be read (or reading was
/// interrupted), so no complete index exists; `1` = every session was read but
/// the output file could not be written; `2` = usage error (enforced by clap).
#[allow(clippy::too_many_arguments)]
fn cmd_activity_index(
    stage: Option<&Path>,
    machine: Option<&str>,
    rebuild: bool,
    destination: Option<String>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    keep_ssh_masters: bool,
) -> ExitCode {
    let config = match config_or_refuse("activity-index") {
        Ok(config) => config,
        Err(code) => return code,
    };
    let machine = match resolve_machine("activity-index", &config, machine) {
        Ok(machine) => machine,
        Err(code) => return code,
    };
    let remote_mode = destination.is_some() || repo.is_some();
    if remote_mode {
        if !rebuild {
            eprintln!("activity-index: destination repair requires --rebuild");
            return ExitCode::from(2);
        }
        let Some(workspace) = stage else {
            eprintln!("activity-index: destination repair requires --stage <work-directory>");
            return ExitCode::from(2);
        };
        if destination.is_none() && repo.is_none() {
            eprintln!("activity-index: name the destination or pass --repo");
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
        let mk = match store::load_key_file(&cfg) {
            Ok(mk) => mk,
            Err(e) => {
                eprintln!(
                    "activity-index: {}",
                    redact_activity_index_paths(&format!("{e:#}"), &cfg, Some(workspace))
                );
                reap_remote(&cfg, keep_ssh_masters);
                return ExitCode::from(3);
            }
        };
        return match rebuild_destination_partition(workspace, &cfg, &machine, &mk) {
            Ok((sessions, summary)) => {
                println!("[activity-index] machine  : {machine}");
                println!("[activity-index] sessions : {sessions}");
                println!("[activity-index] snapshots: {}", summary.snapshots_in_repo);
                println!("[activity-index] repaired snapshot appended");
                reap_remote(&cfg, keep_ssh_masters);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!(
                    "activity-index: destination rebuild failed: {}",
                    redact_activity_index_paths(&format!("{e:#}"), &cfg, Some(workspace))
                );
                reap_remote(&cfg, keep_ssh_masters);
                match e {
                    ActivityIndexError::Read(_) => ExitCode::from(3),
                    ActivityIndexError::Write(_) => ExitCode::from(1),
                }
            }
        };
    }
    let Some(stage) = stage else {
        eprintln!("activity-index: pass --stage for a local stage rebuild");
        return ExitCode::from(2);
    };
    match rebuild_activity_index(stage, &machine, true) {
        Ok(outcome) => {
            println!("[activity-index] stage    : {}", stage.display());
            println!("[activity-index] machine  : {machine}");
            println!("[activity-index] sessions : {}", outcome.sessions_indexed);
            println!("[activity-index] index    : {}", outcome.out_path.display());
            ExitCode::SUCCESS
        }
        Err(ActivityIndexError::Read(message)) => {
            eprintln!(
                "activity-index: {}",
                redact_local_activity_index_message(&message, stage)
            );
            ExitCode::from(3)
        }
        Err(ActivityIndexError::Write(message)) => {
            eprintln!(
                "activity-index: {}",
                redact_local_activity_index_message(&message, stage)
            );
            ExitCode::from(1)
        }
    }
}

/// Rebuild one partition from the newest shard set of every session across all
/// destination snapshots, then append a complete replacement snapshot.
/// Existing destination snapshots are immutable; the restored session shards
/// and rebuilt index travel together.
fn rebuild_destination_partition(
    workspace: &Path,
    cfg: &StoreConfig,
    machine: &str,
    mk: &MasterKey,
) -> Result<(usize, store::PushSummary), ActivityIndexError> {
    rebuild_destination_partition_with_hook(workspace, cfg, machine, mk, || {})
}

fn rebuild_destination_partition_with_hook<F>(
    workspace: &Path,
    cfg: &StoreConfig,
    machine: &str,
    mk: &MasterKey,
    mut after_snapshot_list: F,
) -> Result<(usize, store::PushSummary), ActivityIndexError>
where
    F: FnMut(),
{
    if !workspace.is_dir() {
        return Err(ActivityIndexError::Read(
            "workspace must be an existing directory".into(),
        ));
    }

    loop {
        let temporary = tempfile::Builder::new()
            .prefix("activity-index-rebuild-")
            .tempdir_in(workspace)
            .map_err(|e| ActivityIndexError::Write(format!("create temporary stage: {e}")))?;
        let backends = BackupStore::for_metadata_query(cfg.clone())
            .backends()
            .map_err(|e| ActivityIndexError::Read(format!("open destination: {e:#}")))?;
        let repo = Repository::new(&cfg.repository_options(), &backends)
            .and_then(|repo| repo.open(&Credentials::Masterkey(mk.clone())))
            .and_then(|repo| repo.to_indexed())
            .map_err(|e| ActivityIndexError::Read(format!("open destination: {e:#}")))?;
        let mut snapshots = repo
            .get_all_snapshots()
            .map_err(|e| ActivityIndexError::Read(format!("list destination snapshots: {e:#}")))?;
        snapshots.retain(|snapshot| snapshot.hostname == machine);
        snapshots.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.time.clone()));
        if snapshots.is_empty() {
            return Err(ActivityIndexError::Read(
                "the named machine has no archived snapshots; no snapshot was written".into(),
            ));
        }
        let read_snapshot_ids: BTreeSet<String> = snapshots
            .iter()
            .map(|snapshot| snapshot.id.to_hex().as_str().to_string())
            .collect();
        after_snapshot_list();

        let mut seen_sessions = BTreeSet::new();
        let mut sessions = 0usize;
        for snapshot in &snapshots {
            let root = repo
                .node_from_snapshot_and_path(snapshot, "")
                .map_err(|e| ActivityIndexError::Read(format!("read snapshot tree: {e:#}")))?;
            let entries = repo
                .ls(&root, &LsOptions::default())
                .and_then(|entries| entries.collect::<rustic_core::RusticResult<Vec<_>>>())
                .map_err(|e| ActivityIndexError::Read(format!("list snapshot files: {e:#}")))?;
            let mut shards: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
            for (index, (path, node)) in entries.iter().enumerate() {
                if node.node_type != NodeType::File {
                    continue;
                }
                if let Some((found_machine, session, shard)) = readback::bucket_shard_path(path) {
                    if found_machine == machine && !seen_sessions.contains(&session) {
                        shards.entry(session).or_default().push((shard, index));
                    }
                }
            }
            for (session, mut session_shards) in shards {
                if !seen_sessions.insert(session.clone()) {
                    continue;
                }
                session_shards.sort_by(|(a, _), (b, _)| a.cmp(b));
                for (_, index) in session_shards {
                    let mut shard_bytes = Vec::new();
                    repo.dump(&entries[index].1, &mut shard_bytes)
                        .map_err(|e| {
                            ActivityIndexError::Read(format!("read archived shard: {e:#}"))
                        })?;
                    store::write_sealed_shard_raw_with_cap(
                        store::StageWriter::Restore,
                        temporary.path(),
                        machine,
                        &session,
                        &shard_bytes,
                        store::DEFAULT_SHARD_BUCKET_CAP,
                    )
                    .map_err(|e| {
                        ActivityIndexError::Write(format!("restore archived shard: {e:#}"))
                    })?;
                }
                sessions += 1;
            }
        }
        if sessions == 0 {
            return Err(ActivityIndexError::Read(
                "the named machine has no archived sessions; no snapshot was written".into(),
            ));
        }

        // Carry the newest copy of every machine metadata path across snapshots.
        // The activity index is rebuilt below and writer.json is refreshed.
        let mut copied_metadata = BTreeSet::new();
        for snapshot in &snapshots {
            let root = repo
                .node_from_snapshot_and_path(snapshot, "")
                .map_err(|e| {
                    ActivityIndexError::Read(format!("read snapshot metadata tree: {e:#}"))
                })?;
            let entries = repo
                .ls(&root, &LsOptions::default())
                .and_then(|entries| entries.collect::<rustic_core::RusticResult<Vec<_>>>())
                .map_err(|e| ActivityIndexError::Read(format!("list snapshot metadata: {e:#}")))?;
            for (path, node) in entries {
                if node.node_type != NodeType::File {
                    continue;
                }
                let components: Vec<_> = path.components().collect();
                let Some(meta_at) = components
                    .iter()
                    .position(|part| part.as_os_str() == "meta")
                else {
                    continue;
                };
                if components
                    .get(meta_at + 1)
                    .is_none_or(|part| part.as_os_str() != machine)
                {
                    continue;
                }
                let tail: PathBuf = components[meta_at + 1..].iter().collect();
                if !copied_metadata.insert(tail.clone()) {
                    continue;
                }
                let mut bytes = Vec::new();
                repo.dump(&node, &mut bytes).map_err(|e| {
                    ActivityIndexError::Read(format!("read machine metadata: {e:#}"))
                })?;
                let target = temporary.path().join("meta").join(tail);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        ActivityIndexError::Write(format!("create metadata directory: {e}"))
                    })?;
                }
                fs::write(target, bytes).map_err(|e| {
                    ActivityIndexError::Write(format!("write machine metadata: {e}"))
                })?;
            }
        }
        rebuild_activity_index(temporary.path(), machine, false)?;
        record_writer_version(temporary.path(), machine)
            .map_err(|e| ActivityIndexError::Write(format!("record writer version: {e:#}")))?;

        // A same-machine push can land while the rebuild restores shards. Recheck
        // immediately before publishing; if the read set is stale, throw this
        // temporary stage away and rebuild from the new snapshot set. There is no
        // existing local interprocess lock shared by `push` and `run-once` to take.
        let latest_snapshot_ids = list_destination_machine_snapshot_ids(cfg, mk, machine)?;
        let has_newer_snapshot = latest_snapshot_ids
            .iter()
            .any(|snapshot_id| !read_snapshot_ids.contains(snapshot_id));
        if has_newer_snapshot {
            continue;
        }
        let push = BackupStore::new(cfg.clone(), machine.to_string())
            .push(temporary.path(), mk)
            .map_err(|e| ActivityIndexError::Write(format!("push repaired snapshot: {e:#}")))?;
        return Ok((sessions, push));
    }
}

fn list_destination_machine_snapshot_ids(
    cfg: &StoreConfig,
    mk: &MasterKey,
    machine: &str,
) -> Result<BTreeSet<String>, ActivityIndexError> {
    let backends = BackupStore::for_metadata_query(cfg.clone())
        .backends()
        .map_err(|e| ActivityIndexError::Read(format!("open destination: {e:#}")))?;
    let repo = Repository::new(&cfg.repository_options(), &backends)
        .and_then(|repo| repo.open(&Credentials::Masterkey(mk.clone())))
        .and_then(|repo| repo.to_indexed())
        .map_err(|e| {
            ActivityIndexError::Read(format!("open destination for snapshot recheck: {e:#}"))
        })?;
    let snapshots = repo
        .get_all_snapshots()
        .map_err(|e| ActivityIndexError::Read(format!("re-list destination snapshots: {e:#}")))?;
    Ok(snapshots
        .iter()
        .filter(|snapshot| snapshot.hostname == machine)
        .map(|snapshot| snapshot.id.to_hex().as_str().to_string())
        .collect())
}

/// Store the writer version in the stage so a successful push carries it in
/// the immutable snapshot. Existing snapshots stay untouched.
fn record_writer_version(stage: &Path, machine: &str) -> anyhow::Result<()> {
    let record = sidecar::WriterVersionRecord {
        machine_id: machine.to_string(),
        chat_stasher_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let path = stage.join("meta").join(machine).join("writer.json");
    fs::create_dir_all(path.parent().context("writer metadata has no parent")?)?;
    let bytes = serde_json::to_vec_pretty(&record)?;
    match fs::read(&path) {
        Ok(existing) if existing == bytes => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    }
    atomic_replace(&path, &bytes).with_context(|| format!("replace {}", path.display()))
}

/// `machine-declare` — write this machine's display-name declaration (ADR-018).
///
/// The partition is resolved by the standard three-level rule (config `machine`,
/// then the identity file — generated when missing). The declaration goes to
/// `<stage>/meta/<machine>/machine.json` and rides into the archive on the next
/// `push`, which archives the whole stage root.
fn cmd_machine_declare(stage: &Path, display_name: Option<String>) -> ExitCode {
    let config = match config_or_refuse("machine-declare") {
        Ok(config) => config,
        Err(code) => return code,
    };
    let machine = match resolve_machine("machine-declare", &config, None) {
        Ok(machine) => machine,
        Err(code) => return code,
    };
    let display_name = match display_name.filter(|name| !name.trim().is_empty()) {
        Some(name) => name,
        None => match system_display_name() {
            Some(name) => name,
            None => {
                eprintln!(
                    "machine-declare: could not determine a system display name — writing an empty display_name (set one with --display-name)"
                );
                String::new()
            }
        },
    };
    let decl = identity::MachineDeclaration {
        machine_id: machine.clone(),
        display_name,
        os: std::env::consts::OS.to_string(),
        first_seen_unix: now_unix(),
        declared_harnesses: Vec::new(),
    };
    match write_declaration(stage, &decl) {
        Ok(path) => {
            println!("[machine-declare] machine        : {machine}");
            println!("[machine-declare] display name   : {}", decl.display_name);
            println!("[machine-declare] os             : {}", decl.os);
            println!(
                "[machine-declare] first seen     : {}",
                decl.first_seen_unix
            );
            println!("[machine-declare] file           : {}", path.display());
            println!(
                "[machine-declare] next           : the next `push` archives it with the stage"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("machine-declare: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `machine-label` — write this machine's display-name opinion about another
/// machine (ADR-018). Exists for targets that can no longer declare for
/// themselves. One writer per file, so concurrent machines never merge.
fn cmd_machine_label(stage: &Path, target: &str, label: &str) -> ExitCode {
    let config = match config_or_refuse("machine-label") {
        Ok(config) => config,
        Err(code) => return code,
    };
    let writer = match resolve_machine("machine-label", &config, None) {
        Ok(writer) => writer,
        Err(code) => return code,
    };
    let record = identity::LabelRecord {
        target_machine_id: target.to_string(),
        label: label.to_string(),
        written_by_machine_id: writer.clone(),
        written_at_unix: now_unix(),
    };
    match write_label(stage, &record) {
        Ok(path) => {
            println!("[machine-label] target  : {}", record.target_machine_id);
            println!("[machine-label] label   : {}", record.label);
            println!("[machine-label] writer  : {writer}");
            println!("[machine-label] written : {}", record.written_at_unix);
            println!("[machine-label] file    : {}", path.display());
            println!("[machine-label] next    : the next `push` archives it with the stage");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("machine-label: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Write a declaration to `<stage>/meta/<machine>/machine.json`, creating the
/// directory tree. Returns the path written.
fn write_declaration(stage: &Path, decl: &identity::MachineDeclaration) -> anyhow::Result<PathBuf> {
    let path = identity::machine_decl_path(stage, &decl.machine_id);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("declaration path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create declaration directory {}", parent.display()))?;
    std::fs::write(&path, identity::serialize_declaration(decl)?)
        .with_context(|| format!("write declaration {}", path.display()))?;
    Ok(path)
}

/// Write a label to `<stage>/meta/<target>/label-by-<writer>.json`, creating
/// the directory tree. Returns the path written.
fn write_label(stage: &Path, record: &identity::LabelRecord) -> anyhow::Result<PathBuf> {
    let path = identity::label_path(
        stage,
        &record.target_machine_id,
        &record.written_by_machine_id,
    );
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("label path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create label directory {}", parent.display()))?;
    std::fs::write(&path, serde_json::to_string_pretty(record)?)
        .with_context(|| format!("write label {}", path.display()))?;
    Ok(path)
}

/// The machine's human-readable display name from the OS, if it can be read.
/// macOS uses `scutil --get LocalHostName`; other platforms use `hostname`.
/// `None` means the caller writes an empty name and says so — a name is never
/// invented.
fn system_display_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        command_stdout("scutil", &["--get", "LocalHostName"])
    }
    #[cfg(not(target_os = "macos"))]
    {
        command_stdout("hostname", &[])
    }
}

/// Capture a program's stdout, trimmed, as a string. `None` when the program
/// cannot be run, exits non-zero, prints non-UTF-8, or prints nothing.
fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    std::process::Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Current unix time in seconds. Falls back to 0 only when the system clock
/// predates the epoch (unreachable on a sane host).
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        // reason: the only way to reach here is a system clock before the epoch
        // (a broken host); 0 is the honest "no meaningful timestamp" fallback,
        // never a path a sane host takes.
        .unwrap_or(0)
}

/// Render the human-facing name for an archived machine id (ADR-018).
///
/// A valid 32-hex identity goes through [`identity::display_for`]: a label
/// wins, else the declaration's display name, else `short-hex (unnamed)` — the
/// short id is always shown, and a missing name is marked explicitly, never
/// invented and never blank.
///
/// A **legacy** partition id that is not 32 hex (a pre-ADR-018 hostname like
/// `mac`) has no identity to shorten. It is rendered with the partition id as
/// the identifier plus the declaration/label name when one exists, or an
/// explicit `unnamed` marker otherwise — the partition id is never presented as
/// a name.
fn display_machine(
    machine_id: &str,
    decl: Option<&identity::MachineDeclaration>,
    labels: &[identity::LabelRecord],
) -> String {
    if let Ok(id) = identity::MachineIdentity::from_hex(machine_id) {
        return identity::display_for(&id, decl, labels);
    }
    if let Some(label) = identity::effective_label(labels) {
        return format!("{} ({machine_id})", label.label);
    }
    if let Some(d) = decl.filter(|d| !d.display_name.is_empty()) {
        return format!("{} ({machine_id})", d.display_name);
    }
    format!("{machine_id} (unnamed)")
}

/// Everything `overview` extracts from a destination repository before
/// rendering. The machine sets let the caller name machines that have a
/// snapshot but no activity index, and machines that have a snapshot but no
/// display-name declaration.
struct OverviewRead {
    rows: Vec<overview::OverviewRow>,
    snapshot_machines: BTreeSet<String>,
    index_machines: BTreeSet<String>,
    /// Machines whose newest snapshot carries a `meta/<id>/machine.json`.
    declared_machines: BTreeSet<String>,
    /// Per-machine display metadata found while walking the snapshots.
    declarations: BTreeMap<String, identity::MachineDeclaration>,
    labels: BTreeMap<String, Vec<identity::LabelRecord>>,
    writer_versions: BTreeMap<String, sidecar::WriterVersionRecord>,
    unreadable_writer_versions: BTreeSet<String>,
}

fn read_archive_writer_statuses(
    cfg: &StoreConfig,
    mk: &MasterKey,
) -> anyhow::Result<Vec<sidecar::MachineWriterStatus>> {
    let archived = BackupStore::for_metadata_query(cfg.clone()).read_archived_writers(mk)?;
    Ok(sidecar::writer_statuses(
        &archived.machines,
        &archived.records,
        &archived.unreadable,
    ))
}

/// Open the destination and walk every newest-per-host snapshot for activity
/// index files (`meta/<machine>/activity-v1.jsonl`), parsing them into rows.
///
/// `anyhow::Result` so the concrete `Repository` / opaque-iterator types are
/// resolved by `?` exactly as in `readback::read_all_machines`; the caller
/// translates any error into exit code 3 ("could not finish reading"), so a
/// failure here is never presented as an empty result.
///
/// The archive paths carry each machine's own absolute stage prefix, so files
/// are matched by their trailing `meta` / `<machine>` / `activity-v1.jsonl`
/// marker via [`sidecar::activity_index_machine`] — never by a local path.
fn read_overview_indexes(cfg: &StoreConfig, mk: &MasterKey) -> anyhow::Result<OverviewRead> {
    let store = BackupStore::for_metadata_query(cfg.clone());
    let backends = store.backends()?;
    let repo = Repository::new(&cfg.repository_options(), &backends)?
        .open(&Credentials::Masterkey(mk.clone()))
        .context("open repository for overview")?
        .to_indexed()
        .context("index repository for overview")?;
    let snaps = repo
        .get_all_snapshots()
        .context("list snapshots for overview")?;
    let newest = readback::newest_snapshot_per_host(snaps);

    let mut out = OverviewRead {
        rows: Vec::new(),
        snapshot_machines: BTreeSet::new(),
        index_machines: BTreeSet::new(),
        declared_machines: BTreeSet::new(),
        declarations: BTreeMap::new(),
        labels: BTreeMap::new(),
        writer_versions: BTreeMap::new(),
        unreadable_writer_versions: BTreeSet::new(),
    };

    for snap in newest {
        let hostname = snap.hostname.clone();
        out.snapshot_machines.insert(hostname.clone());
        let root = repo
            .node_from_snapshot_and_path(&snap, "")
            .with_context(|| format!("host `{hostname}`: read tree root"))?;
        let entries: Vec<_> = repo
            .ls(&root, &LsOptions::default())
            .with_context(|| format!("host `{hostname}`: list snapshot tree"))?
            .collect::<rustic_core::RusticResult<Vec<_>>>()
            .with_context(|| format!("host `{hostname}`: collect snapshot entries"))?;
        for (path, node) in &entries {
            if node.node_type != NodeType::File {
                continue;
            }
            if let Some(machine) = sidecar::activity_index_machine(path) {
                out.index_machines.insert(machine.clone());
                let mut buf = Vec::new();
                repo.dump(node, &mut buf).with_context(|| {
                    format!("host `{hostname}` machine `{machine}`: read activity index")
                })?;
                for line in String::from_utf8_lossy(&buf).lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let row: activity::ActivityRow = serde_json::from_str(line).with_context(|| {
                        format!("host `{hostname}` machine `{machine}`: malformed activity index line")
                    })?;
                    out.rows.push(sidecar::to_overview_row(&row));
                }
                continue;
            }
            if let Some(machine) = sidecar::declaration_machine(path) {
                let mut buf = Vec::new();
                repo.dump(node, &mut buf).with_context(|| {
                    format!("host `{hostname}` machine `{machine}`: read declaration")
                })?;
                let decl: identity::MachineDeclaration = serde_json::from_slice(&buf)
                    .with_context(|| {
                        format!(
                            "host `{hostname}` machine `{machine}`: malformed machine declaration"
                        )
                    })?;
                out.declared_machines.insert(machine.clone());
                out.declarations.insert(machine, decl);
                continue;
            }
            if let Some(machine) = sidecar::writer_machine(path) {
                let mut buf = Vec::new();
                repo.dump(node, &mut buf).with_context(|| {
                    format!("host `{hostname}` machine `{machine}`: read writer version")
                })?;
                match serde_json::from_slice::<sidecar::WriterVersionRecord>(&buf) {
                    Ok(record) if record.machine_id == machine => {
                        out.writer_versions.insert(machine, record);
                    }
                    _ => {
                        out.unreadable_writer_versions.insert(machine);
                    }
                }
                continue;
            }
            if let Some((machine, _writer)) = sidecar::label_record_machine(path) {
                let mut buf = Vec::new();
                repo.dump(node, &mut buf).with_context(|| {
                    format!("host `{hostname}` machine `{machine}`: read label")
                })?;
                let record: identity::LabelRecord =
                    serde_json::from_slice(&buf).with_context(|| {
                        format!("host `{hostname}` machine `{machine}`: malformed label record")
                    })?;
                out.labels.entry(machine).or_default().push(record);
            }
        }
    }
    Ok(out)
}

/// `overview` — draw the machine × harness activity overview of one destination
/// (ADR-017).
///
/// Opens the destination repository, takes each host's newest snapshot, and
/// from each snapshot's (recursively listed) tree finds every
/// `meta/<machine>/activity-v1.jsonl` file by its trailing marker. The indexes
/// are parsed into [`overview::OverviewRow`] values and rendered.
///
/// Exit codes: `0` = at least one index was read and rendered; `1` = the whole
/// repository was read and no activity index exists anywhere (a real empty
/// answer); `3` = the repository could not be read in full — a missing index
/// is then unproven, never folded into "there is none". A machine that has a
/// snapshot but no index is listed explicitly as "index missing".
#[allow(clippy::too_many_arguments)]
fn cmd_overview(
    destination: Option<String>,
    width: usize,
    json: bool,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    keep_ssh_masters: bool,
) -> ExitCode {
    let config = match config_or_refuse("overview") {
        Ok(config) => config,
        Err(code) => return code,
    };
    if destination.is_none() && repo.is_none() {
        let msg = "overview: name the destination to open (`--destination <name>`, or an explicit `--repo`); there is no default destination and no cross-destination merge";
        eprintln!("{msg}");
        if json {
            println!(
                "{}",
                json_string(&overview::overview_error_json(2, msg.to_string()))
            );
        }
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
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            let msg = format!("{e:#}");
            eprintln!("overview: {msg}");
            eprintln!("overview: without the key nothing was read — this is not an empty result");
            reap_remote(&cfg, keep_ssh_masters);
            if json {
                println!("{}", json_string(&overview::overview_error_json(3, msg)));
            }
            // Deliberately 3, not 1: a missing key means the archive was never
            // consulted, which belongs with "could not finish reading".
            return ExitCode::from(3);
        }
    };

    let read = match read_overview_indexes(&cfg, &mk) {
        Ok(read) => read,
        Err(e) => {
            let msg = chat_stasher::remote_err::format_remote_error("overview", &e, &cfg);
            eprintln!("{msg}");
            eprintln!(
                "overview: the archive was not read to completion — this is not an empty result"
            );
            reap_remote(&cfg, keep_ssh_masters);
            if json {
                println!("{}", json_string(&overview::overview_error_json(3, msg)));
            }
            // Deliberately 3, not 1: a failed read cannot claim "there is no
            // index"; 1 is reserved for a completed read that found none.
            return ExitCode::from(3);
        }
    };
    let OverviewRead {
        rows,
        snapshot_machines,
        index_machines,
        declared_machines,
        declarations,
        labels,
        writer_versions,
        unreadable_writer_versions,
    } = read;
    let writer_status = sidecar::writer_statuses(
        &snapshot_machines,
        &writer_versions,
        &unreadable_writer_versions,
    );

    // ADR-018 display names: a machine's name is its declaration, its labels
    // (latest wins) or an explicit unnamed marker — never the raw partition id
    // pretending to be a name, and never blank.
    let display = |machine: &str| -> String {
        let decl = declarations.get(machine);
        let machine_labels: &[identity::LabelRecord] =
            labels.get(machine).map_or(&[], Vec::as_slice);
        display_machine(machine, decl, machine_labels)
    };

    if json {
        let display_names: BTreeMap<String, String> = snapshot_machines
            .iter()
            .map(|machine| (machine.clone(), display(machine)))
            .collect();
        let exit_code = if rows.is_empty() { 1 } else { 0 };
        let mut value = overview::overview_json(
            &rows,
            &snapshot_machines,
            &index_machines,
            &declared_machines,
            &display_names,
            exit_code,
        );
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "writer_versions".to_string(),
                serde_json::json!(writer_status),
            );
        }
        println!("{}", json_string(&value));
        reap_remote(&cfg, keep_ssh_masters);
        return ExitCode::from(exit_code);
    }

    println!("[overview] repo         : {}", cfg.repo_root);
    println!("[overview] width        : {width}");
    println!(
        "[overview] machines     : {} with snapshot / {} with index / {} with declaration",
        snapshot_machines.len(),
        index_machines.len(),
        declared_machines.len()
    );
    for writer in &writer_status {
        println!(
            "[overview] writer version: machine={} version={} behind-newest={}",
            display(&writer.machine),
            writer
                .chat_stasher_version
                .as_deref()
                .unwrap_or(if writer.version_unreadable {
                    "unreadable"
                } else {
                    "behind (version not recorded — written by ≤0.3.0)"
                }),
            writer
                .behind_newest_writer
                .map_or("unknown", |behind| if behind { "yes" } else { "no" }),
        );
    }

    // A machine that has a snapshot but no activity index must be named — never
    // silently dropped, which would fold "no index" into "no sessions".
    let missing = sidecar::missing_index_machines(&snapshot_machines, &index_machines);
    if !missing.is_empty() {
        println!("[overview] index missing (snapshot present, no activity index):");
        for m in &missing {
            println!("  !! {}", display(m));
        }
    }

    // A machine that has a snapshot but no display-name declaration must also
    // be named — it renders as `<short-id> (unnamed)` and never vanishes from
    // the totals. (Legacy hostname partitions are counted here until the owner
    // sets `machine = "<partition>"` in the config and declares.)
    let undeclared: Vec<String> = snapshot_machines
        .iter()
        .filter(|m| !declared_machines.contains(*m))
        .cloned()
        .collect();
    if !undeclared.is_empty() {
        println!("[overview] declaration missing (snapshot present, no machine.json declaration):");
        for m in &undeclared {
            println!("  !! {}", display(m));
        }
    }

    if rows.is_empty() {
        // Read the whole repository and there is no activity index anywhere:
        // a genuine "there is none", not a failure to look.
        println!(
            "overview: the destination was read in full and contains no activity index (no `meta/*/activity-v1.jsonl` anywhere)"
        );
        reap_remote(&cfg, keep_ssh_masters);
        return ExitCode::from(1);
    }

    // The matrix/heatmap rows are keyed by the machine's *display name* — two
    // machines with the same display name stay distinguishable because the
    // short id is always appended.
    let mut display_rows = rows;
    for row in &mut display_rows {
        row.machine = display(&row.machine);
    }
    println!("{}", overview::render_overview(&display_rows, width));
    reap_remote(&cfg, keep_ssh_masters);
    ExitCode::SUCCESS
}

/// Resolve the archive partition by ADR-018's three-level rule:
///
///   1. an explicit `--machine` override;
///   2. the config's `machine` field — the one line that keeps a legacy
///      install on its existing partition;
///   3. the identity file — loaded when present, **generated and persisted
///      only when the file is missing**, and a hard failure (exit 3, "do not
///      delete this file") when the file is present but unusable.
///
/// 🔴 There is deliberately no hostname fallback: two machines that share a
/// default hostname used to silently merge into one partition, and that class
/// of bug is what ADR-018 exists to remove. A *missing* identity is generated
/// and persisted, so the operation proceeds; a *present but unusable* identity
/// is the hard failure (exit 3, "do not delete this file") because replacing
/// it would silently re-key — and orphan — the machine's archive partition.
fn resolve_machine(
    command: &str,
    config: &Config,
    explicit: Option<&str>,
) -> Result<String, ExitCode> {
    resolve_machine_at(command, config, explicit, &machine_identity_path())
}

/// [`resolve_machine`] with the identity path supplied explicitly, so the
/// three-way outcome (Loaded / Missing / Unusable) is unit-testable against a
/// scratch file.
fn resolve_machine_at(
    command: &str,
    config: &Config,
    explicit: Option<&str>,
    identity_path: &Path,
) -> Result<String, ExitCode> {
    if let Some(machine) = explicit {
        return Ok(machine.to_string());
    }
    if let Some(machine) = config.machine.as_deref().filter(|m| !m.is_empty()) {
        return Ok(machine.to_string());
    }
    match identity::load_identity_state(identity_path) {
        identity::IdentityFileState::Loaded(id) => Ok(id.as_hex()),
        identity::IdentityFileState::Missing => match identity::load_or_create(identity_path) {
            Ok((id, true)) => {
                eprintln!(
                    "{command}: generated a new machine identity {} — this is the archive partition for this machine from now on",
                    id.short_hex()
                );
                Ok(id.as_hex())
            }
            Ok((id, false)) => Ok(id.as_hex()),
            Err(e) => {
                eprintln!("{command}: cannot persist a new machine identity: {e:#}");
                eprintln!("{command}: nothing was read or written.");
                Err(ExitCode::from(3))
            }
        },
        identity::IdentityFileState::Unusable(error) => {
            eprintln!(
                "{command}: machine identity file {} is present but unusable: {error:?}",
                identity_path.display()
            );
            eprintln!(
                "{command}: 🔴 do not delete this file — it is the key to this machine's archive partition."
            );
            eprintln!(
                "{command}: fix the read/parse problem and re-run; nothing was read or written."
            );
            Err(ExitCode::from(3))
        }
    }
}

/// Where this machine's identity lives. One file per install, created on first
/// use, owner-readable-only; it is the partition key for every later run.
fn machine_identity_path() -> PathBuf {
    data_root().join("machine-identity")
}

/// A metadata-only query can inspect every machine in a repository without
/// selecting this machine's partition. Keep that distinction explicit in the
/// store API; it never creates a path or snapshot host, so it never *generates*
/// an identity either — a missing or unusable identity is simply `None`.
fn query_machine(config: &Config, explicit: Option<&str>) -> Option<String> {
    explicit
        .map(str::to_string)
        .or_else(|| config.machine.clone().filter(|m| !m.is_empty()))
        .or_else(
            || match identity::load_identity_state(&machine_identity_path()) {
                identity::IdentityFileState::Loaded(id) => Some(id.as_hex()),
                _ => None,
            },
        )
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
    filters: &chat_stasher::selector::SelectorArgs,
    json: bool,
    cost: bool,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    keep_ssh_masters: bool,
) -> ExitCode {
    // Resolve the filter before touching the network: a date that is not a
    // date, or a window that cannot be satisfied, is a usage error and must
    // cost nothing and read nothing.
    let resolved = match filters.resolve() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("search: {e}");
            return ExitCode::from(2);
        }
    };
    for warning in &resolved.warnings {
        eprintln!("{warning}");
    }
    let selector = resolved.selector;

    let config = match config_or_refuse("search") {
        Ok(config) => config,
        Err(code) => return code,
    };
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
    let store = BackupStore::for_metadata_query(cfg.clone());
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            eprintln!("search: {e}");
            eprintln!("search: without the key nothing was read — this is not an empty result");
            reap_remote(&cfg, keep_ssh_masters);
            // Deliberately 3, not 1. A missing key means the archive was never
            // consulted, which belongs with "could not finish reading", not
            // with "read it all and nothing matched". Reusing 1 here would make
            // a lost key indistinguishable from a genuine empty answer.
            return ExitCode::from(3);
        }
    };

    let report = match chat_stasher::search::search_sessions(&store, &mk, &selector) {
        Ok(r) => r,
        Err(e) => {
            // Could not open the repository at all. This is the case that must
            // never be rendered as "nothing matched".
            eprintln!("search: cannot read `{}`: {e}", cfg.repo_root);
            eprintln!("search: this is not an empty destination — the archive was not read");
            reap_remote(&cfg, keep_ssh_masters);
            return ExitCode::from(3);
        }
    };

    let code = if json {
        for warning in report.machine_recall_warnings() {
            eprintln!("{warning}");
        }
        print!("{}", chat_stasher::search::report_json(&report, cost));
        if report.answer_complete() {
            if report.hits.is_empty() {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        } else {
            ExitCode::from(3)
        }
    } else {
        for warning in report.machine_recall_warnings() {
            eprintln!("{warning}");
        }
        search_human(&report, cost)
    };

    reap_remote(&cfg, keep_ssh_masters);
    code
}

/// The human report.
///
/// Three groups, never merged: what matched, what an active filter could not
/// place, and which parts of the destination could not be read. The middle one
/// is the point — folding it into "not matched" is the failure this whole
/// change exists to prevent.
fn search_human(report: &chat_stasher::search::SearchReport, cost: bool) -> ExitCode {
    println!("[search] destination  : {}", report.destination);
    println!(
        "[search] snapshots    : {} scanned / {} in repo",
        report.snapshots_scanned, report.snapshots_in_repo
    );
    println!("[search] sessions seen: {}", report.sessions_seen);
    println!("[search] data blobs read: {}", report.data_blobs_read);
    println!("[search] index files read: {}", report.index_files_read);
    for machine in report.machine_window_summary() {
        println!(
            "[search] machine      : {} located={} time-unknown={} index-trusted={}",
            machine.machine, machine.located, machine.time_unknown, machine.index_trusted,
        );
    }
    match &report.window {
        Some(w) => println!("[search] time window  : {}", w.describe()),
        None => println!("[search] time window  : none — every session matches, whatever its time"),
    }
    if report.session_time_unknown() > 0 {
        println!("[search] time unknown : {}", report.session_time_unknown());
    }
    for machine in &report.machines_without_index {
        println!(
            "  !! no activity index for machine `{machine}` — its sessions cannot be placed in time"
        );
    }
    for path in &report.unreadable {
        println!("  !! unreadable: {path}");
    }
    println!("[search] matched      : {}", report.hits.len());
    for hit in &report.hits {
        println!(
            "  {}  machine={}  harness={}  shards={}  bytes={}  snapshot={}  active={}",
            hit.short_id(),
            hit.machine,
            hit.harness.as_deref().unwrap_or("unknown"),
            hit.shard_count,
            hit.bytes,
            hit.short_snapshot(),
            describe_span(
                hit.first_unix,
                hit.last_unix,
                hit.time_why.as_deref(),
                &hit.time_source,
            )
        );
    }
    if !report.unplaced.is_empty() {
        println!(
            "[search] could not be placed: {} (NOT 'not matched' — an active filter had no answer for these)",
            report.unplaced.len()
        );
        for u in &report.unplaced {
            println!(
                "  {}  machine={}  harness={}  shards={}  bytes={}  why: {}",
                u.short_id(),
                u.machine,
                u.harness.as_deref().unwrap_or("unknown"),
                u.shard_count,
                u.bytes,
                u.why
            );
        }
    }
    println!("[search] not matched  : {}", report.not_matched);

    let code = if report.hits.is_empty() {
        println!("{}", report.no_hit_line());
        if report.answer_complete() {
            ExitCode::from(1)
        } else {
            ExitCode::from(3)
        }
    } else if !report.answer_complete() {
        // Hits *and* something unread or unplaceable: the hits are real, the
        // absence of further hits is not established. Say so, and do not 0.
        println!(
            "search: PARTIAL — the matches above are real, but `{}` could not be read in full ({} unreadable) and {} session(s) could not be placed in time, so there may be more",
            report.destination,
            report.unreadable.len(),
            report.unplaced.len()
        );
        ExitCode::from(3)
    } else {
        ExitCode::SUCCESS
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
    code
}

/// One line describing a session's conversation interval, with the unknown
/// case spelled out instead of printed as an empty field.
fn describe_span(
    first_unix: Option<i64>,
    last_unix: Option<i64>,
    why: Option<&str>,
    source: &activity::TimeSource,
) -> String {
    if source.is_no_conversation_content() {
        return "no conversation content".to_string();
    }
    match (first_unix, last_unix) {
        // A bound that is only part of the span says so where it is shown,
        // rather than reading as the session's whole extent.
        (Some(f), Some(l)) if source.bounds_are_partial() => {
            format!(
                "{f}..{l} (partial: {})",
                why.unwrap_or("no reason recorded")
            )
        }
        (Some(f), Some(l)) => format!("{f}..{l}"),
        _ => format!(
            "unknown ({})",
            why.unwrap_or("no reason was recorded for this session")
        ),
    }
}

/// `export` — write the selected sessions to files.
///
/// The filter is resolved before the network is touched (a date that is not a
/// date costs nothing and reads nothing), the selection is `search`'s own code
/// path, and the price is printed between the two: after the selection is known
/// and before any session byte is fetched. `--dry-run` stops after it.
#[allow(clippy::too_many_arguments)]
fn cmd_export(
    destination: Option<String>,
    out: &Path,
    filters: &chat_stasher::selector::SelectorArgs,
    turns: TurnsArg,
    trim_to_window: bool,
    force: bool,
    dry_run: bool,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    keep_ssh_masters: bool,
) -> ExitCode {
    let resolved = match filters.resolve() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("export: {e}");
            return ExitCode::from(2);
        }
    };
    for warning in &resolved.warnings {
        // The deprecation notice `search` prints names `search`; reword it for
        // this command rather than letting it name the wrong one.
        eprintln!("{}", warning.replace("search:", "export:"));
    }
    let selector = resolved.selector;

    let config = match config_or_refuse("export") {
        Ok(config) => config,
        Err(code) => return code,
    };
    if destination.is_none() && repo.is_none() {
        eprintln!(
            "export: name the destination to export from (`--destination <name>`, or an explicit `--repo`)"
        );
        eprintln!(
            "export: there is no default destination and no cross-destination merge — archives are not required to agree"
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
    // Same as `search`: the machine flag is a query over `sessions/<machine>/`,
    // never this machine's identity.
    let store = BackupStore::for_metadata_query(cfg.clone());
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            eprintln!("export: {e}");
            eprintln!("export: without the key nothing was read — this is not an empty export");
            reap_remote(&cfg, keep_ssh_masters);
            // 3, not 1: the archive was never consulted.
            return ExitCode::from(3);
        }
    };

    if let Err(error) = chat_stasher::export::check_out(out, force) {
        eprintln!("export: {error}");
        reap_remote(&cfg, keep_ssh_masters);
        return ExitCode::from(2);
    }

    let recall = match chat_stasher::search::search_sessions(&store, &mk, &selector) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("export: destination scan failed before planning: {error:#}");
            reap_remote(&cfg, keep_ssh_masters);
            return ExitCode::from(3);
        }
    };
    for warning in recall.machine_recall_warnings() {
        eprintln!("{warning}");
    }

    let opts = chat_stasher::export::ExportOptions {
        out: out.to_path_buf(),
        turns: turns.to_turns(),
        trim_to_window,
        force,
        dry_run,
    };
    for line in chat_stasher::export::print_header(&cfg.repo_root, &opts, selector.window.as_ref())
    {
        println!("{line}");
    }
    let on_plan = |plan: &chat_stasher::export::ExportPlan| {
        println!("{}", chat_stasher::export::print_plan(plan));
    };
    let report =
        match chat_stasher::export::export_sessions(&store, &mk, &selector, &opts, &on_plan) {
            Ok(r) => r,
            Err(e) => {
                let code = chat_stasher::export::exit_status_for_error(&e);
                if let Some(usage) = e.downcast_ref::<chat_stasher::selector::UsageError>() {
                    eprintln!("export: {usage}");
                } else {
                    eprintln!("export: {e:#}");
                    if code == 3 {
                        eprintln!(
                        "export: `{}` was not read to the end — nothing here proves what it holds",
                        cfg.repo_root
                    );
                    }
                }
                reap_remote(&cfg, keep_ssh_masters);
                return ExitCode::from(code);
            }
        };

    for line in chat_stasher::export::print_report(&report) {
        println!("{line}");
    }
    reap_remote(&cfg, keep_ssh_masters);
    ExitCode::from(report.exit_status())
}

/// `view` — an ephemeral loopback web view of one destination's session list.
///
/// Structure worth noting: the whole metadata read happens *before* the socket
/// is bound, and the server is handed an immutable snapshot of it. That is why
/// no request can reach the repository, and therefore why no request can be
/// coaxed into fetching payload. It also means the exit code is decided from the
/// read, exactly like `search`, and stays honest whether or not anyone ever
/// opens the page.
///
/// Loopback is not a boundary: any local program can connect. See the `--help`
/// text — the token is the only thing gating access, and it lives only in the
/// printed URL.
#[allow(clippy::too_many_arguments)]
/// `ui` — the overview dashboard, served from one metadata-tier read.
///
/// The read is deliberately **unfiltered** ([`chat_stasher::selector::Selector::default`]),
/// even when the command line carries filters. Every filter — the launch one and
/// the one a drill-down link carries — is applied in memory to that one
/// inventory by the shared selector. That is what makes `/sessions?machine=…`
/// and `search --machine …` the same query: `search` applies the same decision
/// function to the same rows. A repository read narrowed at the command line
/// could not answer a drill-down for anything it had already discarded, and it
/// would report `not_matched`/`unplaced` against a set the page never saw.
/// The destination `ui` opens when the command line named none, or `None`
/// after saying why on stderr (the caller exits 2 — a usage question, never a
/// guessed archive).
///
/// One declared destination is the default. Several declare their default
/// through `[native_host] destination` — the knob the extension's
/// `open_dashboard` already reads (protocol §6.5), honoured here so one knob
/// keeps one meaning; a default naming an undeclared destination is a config
/// bug and is refused rather than fallen back. None declared leaves nothing to
/// open, and the fix (declaring one) is in the message.
fn default_destination_or_exit(config: &Config) -> Option<(String, String)> {
    let mut names: Vec<&str> = config.destinations.keys().map(String::as_str).collect();
    names.sort_unstable();
    let declared = names.join(", ");
    // The recorded default is checked first, whatever the destination count:
    // `[native_host] destination` declares "which destination the dashboard
    // opens", so a value naming an undeclared destination is a config bug the
    // popup shares, and serving something else here would mask it.
    match config
        .native_host
        .as_ref()
        .and_then(|section| section.destination.as_deref())
    {
        Some(name) if config.destinations.contains_key(name) => {
            return Some((
                name.to_string(),
                "[native_host] destination in the config names it".to_string(),
            ));
        }
        Some(name) => {
            eprintln!(
                "ui: `[native_host] destination` names `{name}`, which the config does not declare (declared: {declared})"
            );
            eprintln!(
                "ui: a default naming an undeclared destination is never fallen back — fix the config, or pass `--destination <name>`"
            );
            return None;
        }
        None => {}
    }
    match names.len() {
        0 => {
            eprintln!(
                "ui: there is no destination to open — the config declares no destination and no --repo was given"
            );
            eprintln!(
                "ui: declare one with a `[destinations.<name>]` section in the config, or name a repository directly with `--repo <path>`"
            );
            None
        }
        1 => Some((
            names[0].to_string(),
            "the only destination the config declares".to_string(),
        )),
        _ => {
            eprintln!(
                "ui: the config declares {} destination(s) — name which one to open, with `--destination <name>`",
                names.len()
            );
            eprintln!("ui: declared: {declared}");
            eprintln!(
                "ui: or record a default the dashboard opens: `[native_host] destination = \"<name>\"` in the config"
            );
            None
        }
    }
}

fn cmd_ui(args: UiArgs, deprecated_alias: Option<&str>) -> ExitCode {
    if let Some(notice) = deprecated_alias {
        eprintln!("{notice}");
    }
    let UiArgs {
        destination,
        filters,
        no_open,
        idle_timeout,
        repo,
        key_file,
        connections,
        options,
        keep_ssh_masters,
    } = args;

    // Fail closed first: a config that exists and cannot be used stops `ui`
    // before any destination is resolved, exactly as it stops the other
    // commands that read it.
    let config = match config_or_refuse("ui") {
        Ok(config) => config,
        Err(code) => return code,
    };
    // A dashboard is a look, not a retrieval: `search`, `export` and
    // `overview` still require naming the copy (ADR-013 — recorded below by
    // the unchanged `resolve_store_config`), but a human who typed `ui` and
    // named nothing may be spared the choice when the config leaves no
    // ambiguity: the only declared destination is the default, and with
    // several, `[native_host] destination` — the one knob the config already
    // has for "which destination the dashboard opens" — names it. The two
    // callers a config can leave unresolvable both exit 2 here, with the
    // state on stderr; nothing is ever guessed and served.
    let mut default_from: Option<String> = None;
    let destination = match (destination, repo.as_deref()) {
        (Some(name), _) => Some(name),
        (None, Some(_)) => None,
        (None, None) => match default_destination_or_exit(&config) {
            Some((name, how)) => {
                default_from = Some(how);
                Some(name)
            }
            None => return ExitCode::from(2),
        },
    };
    let resolved = match filters.resolve() {
        Ok(r) => r,
        Err(e) => {
            // Same rule as `search`: a filter that cannot be resolved is a
            // usage error, never an empty result.
            eprintln!("ui: {e}");
            return ExitCode::from(2);
        }
    };
    for warning in &resolved.warnings {
        // The notice says "search:"; it is the same shared selector telling the
        // same story, so it is passed through rather than reworded.
        eprintln!("{warning}");
    }
    let label = destination
        .clone()
        .unwrap_or_else(|| chat_stasher::ui::EXPLICIT_REPO_LABEL.to_string());
    let cfg = resolve_store_config(
        &config,
        destination.as_deref(),
        repo,
        key_file,
        connections,
        &options,
    );
    // ADR-034: the dashboard is the other single-session body reader. Every
    // route except `load` is metadata-tier, and only `load` reaches this cache.
    let store = BackupStore::for_metadata_query(cfg.clone()).with_body_cache(
        chat_stasher::body_cache::for_operation(
            &config,
            chat_stasher::body_cache::Policy::ReadThrough,
        )
        .handle(),
    );
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            eprintln!("ui: {e}");
            eprintln!("ui: without the key nothing was read — this is not an empty result");
            reap_remote(&cfg, keep_ssh_masters);
            // 3, not 1: the archive was never consulted. Same reasoning as
            // `search` — a lost key must not be indistinguishable from an
            // archive that genuinely holds nothing.
            return ExitCode::from(3);
        }
    };

    let report = match chat_stasher::search::search_sessions(
        &store,
        &mk,
        &chat_stasher::selector::Selector::default(),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ui: cannot read `{}`: {e}", cfg.repo_root);
            eprintln!("ui: this is not an empty destination — the archive was not read");
            reap_remote(&cfg, keep_ssh_masters);
            return ExitCode::from(3);
        }
    };
    // ssh masters are reaped before the server starts, not after: the serve loop
    // can sit idle for minutes, and there is nothing left to read by then. The
    // one exception is `/content`, which reopens the repository on demand — it
    // does so with whatever the backend needs, and opens no master of its own
    // beyond what `BackupStore` already configures.
    reap_remote(&cfg, keep_ssh_masters);

    for path in &report.unreadable {
        say!("  !! unreadable: {path}");
    }
    // OQ-2 (29-UI-DESIGN §5.4/§12, landed by W172): a metadata read that
    // finished serves even when it matched nothing — the empty page is one of
    // the honest states, and the one screen on which a wrong launch filter can
    // be seen to be wrong and corrected. The shared selector's no-hit line
    // keeps "not there" and "UNKNOWN" separate on stdout (it says "search:",
    // the same pass-through the filter warnings use below), and the exit code
    // is decided by the read's completeness after serving (bottom of this
    // function), never by emptiness.
    if report.hits.is_empty() {
        say!("{}", report.no_hit_line());
    }

    let now_unix = now_unix();
    let data = chat_stasher::ui::UiData::from_report(&report, label, resolved.selector, now_unix);
    let in_view = chat_stasher::ui::select(&data.sessions, &data.launch);
    let listed = in_view.matched.len() + in_view.unplaced.len();
    let token = match chat_stasher::view::new_token() {
        Ok(t) => t,
        Err(e) => {
            // No weaker fallback on purpose: a guessable token on a socket every
            // local program can reach is worse than refusing to serve.
            eprintln!("ui: cannot read OS randomness for the access token: {e}");
            eprintln!("ui: refusing to serve without a strong token");
            return ExitCode::from(3);
        }
    };
    let listener = match chat_stasher::view::bind_ephemeral() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ui: cannot bind 127.0.0.1:0 — {e}");
            return ExitCode::from(3);
        }
    };
    let addr = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ui: bound but cannot read local address — {e}");
            return ExitCode::from(3);
        }
    };

    let idle = if idle_timeout == 0 {
        Duration::from_secs(u64::MAX / 2)
    } else {
        Duration::from_secs(idle_timeout)
    };
    let url = format!("http://{addr}/?token={token}");
    say!("[ui] destination  : {}", data.destination_label);
    if let Some(how) = &default_from {
        say!("[ui] default      : {how}");
    }
    say!(
        "[ui] snapshots    : {} scanned / {} in repo",
        data.snapshots_scanned,
        data.snapshots_in_repo
    );
    say!(
        "[ui] sessions     : {listed} in view / {} in the archive",
        data.archive_sessions
    );
    if let Some(text) = chat_stasher::ui::describe_selector(&data.launch) {
        say!("[ui] filter       : {text}");
    }
    say!(
        "[ui] machines     : {} · sources {}",
        data.machine_keys().len(),
        chat_stasher::ui::select(&data.sessions, &data.launch)
            .in_view()
            .iter()
            .map(|s| s.source_label())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );
    say!("[ui] data blobs read: {}", data.data_blobs_read);
    say!("[ui] bound        : {addr} (loopback only, OS-assigned port)");
    say!(
        "[ui] idle timeout : {}",
        if idle_timeout == 0 {
            "none (Ctrl+C to exit)".to_string()
        } else {
            format!("{idle_timeout}s")
        }
    );
    say!(
        "[ui] payload      : NOT loaded — session content is fetched only when you click it, and its cost is shown first"
    );
    say!("[ui] warning      : any program on this machine can reach 127.0.0.1; the token in the URL below is the only gate. Do not share it.");
    say!("{url}");

    if no_open {
        say!("[ui] browser      : not opened (--no-open)");
    } else if let Err(e) = chat_stasher::view::open_in_browser(&url) {
        eprintln!("[ui] browser      : could not open ({e}) — use the URL above, or --no-open");
    } else {
        say!("[ui] browser      : opened");
    }

    let content = RepoContent {
        store: &store,
        mk: &mk,
    };
    let stats = match chat_stasher::view::serve(&listener, &token, &data, idle, &content) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ui: serve loop failed: {e}");
            return ExitCode::from(3);
        }
    };
    say!(
        "[ui] exiting      : idle for {}s · requests served={} rejected={}",
        idle_timeout,
        stats.served,
        stats.rejected
    );

    if !report.complete() {
        say!(
            "ui: PARTIAL — the sessions listed are real, but `{}` could not be read in full ({} unreadable), so there may be more",
            data.destination_label,
            report.unreadable.len()
        );
        return ExitCode::from(3);
    }
    ExitCode::SUCCESS
}

/// The one implementation of the payload tier: the `/content` route asks this
/// for one session's shards, and nothing else does.
struct RepoContent<'a> {
    store: &'a BackupStore,
    mk: &'a MasterKey,
}

impl chat_stasher::ui::ContentSource for RepoContent<'_> {
    fn fetch(&self, machine: &str, session_id: &str) -> Result<chat_stasher::ui::Content, String> {
        let (bytes, shards) = self
            .store
            .read_session_concat(machine, session_id, self.mk)
            .map_err(|e| format!("{e:#}"))?;
        Ok(chat_stasher::ui::Content {
            concat_sha256: sha256_hex(&bytes),
            bytes: bytes.len(),
            body: String::from_utf8_lossy(&bytes).into_owned(),
            shards,
        })
    }
}

fn cmd_doctor(json: bool) -> ExitCode {
    let mut report = chat_stasher::doctor::run();
    // ADR-023 — the one place `doctor` opens a real connection. Kept out of
    // `doctor::run()` because a dozen integration tests call that entry point
    // directly and a test may not reach the network; the CLI is where a real
    // probe belongs.
    //
    // Only when the config could be read at all: with none, there is no declared
    // destination to dial, and an empty probe list would read as "no destination
    // answered" — a claim about a destination list nobody managed to read.
    if let Ok(config) = Config::load() {
        report.destinations = chat_stasher::doctor::probe_destinations(&config);
    }
    if json {
        let value = chat_stasher::doctor::report_to_json(&report);
        println!("{}", json_string(&value));
    } else {
        chat_stasher::doctor::print_report(&report);
    }
    if let Some(error) = &report.config_error {
        // The one command that does not refuse on an unusable config, because
        // "which of my setup is broken" is the question it exists to answer. It
        // still cannot answer the config-dependent half, so it says which half,
        // and exits non-zero: `doctor` reporting a clean machine it never
        // inspected is the failure mode this replaces.
        eprintln!("doctor: config={error}");
        eprintln!(
            "doctor: INCOMPLETE exit_code=3 — the config file could not be used, so every check that \
             reads it was NOT performed (those are listed above and in `not_checked` under `--json`). \
             Absent results there are unknown, not zero."
        );
        return ExitCode::from(3);
    }
    if report.scan_failed {
        eprintln!(
            "doctor: INCOMPLETE exit_code=3 — the registry-driven scan did not run, so the coverage \
             numbers above are UNKNOWN, not zero. The retention verdicts that depend on a session \
             count were omitted rather than fabricated; nothing here proves your harnesses are safe."
        );
        // Deliberately 3, not 1, and no longer 0. Same family as `cmd_search`,
        // `cmd_collect` and `cmd_push`: 3 == did not finish / never started,
        // 1 == finished and failed. `doctor` could not perform the diagnosis at
        // all, which is the "never started" case — reusing 1 would make an
        // unreadable registry indistinguishable from a completed diagnosis.
        //
        // Note the boundary this does NOT cross: a 🔴 line in `risks` is a
        // *successful* diagnosis with a bad finding, and keeps exit 0. `doctor`
        // reports on the machine; it does not fail because the machine is
        // unhealthy. Only "I could not look" is non-zero.
        return ExitCode::from(3);
    }
    ExitCode::SUCCESS
}

fn cmd_ingest(
    inbox: &PathBuf,
    stage: &PathBuf,
    machine: Option<&str>,
    shard_bucket_cap: usize,
) -> ExitCode {
    let config = match config_or_refuse("ingest") {
        Ok(config) => config,
        Err(code) => return code,
    };
    let machine = match resolve_machine("ingest", &config, machine) {
        Ok(machine) => machine,
        Err(code) => return code,
    };
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
    if !report.errors.is_empty() {
        eprintln!(
            "ingest: FAILED exit_code=1 — {} inbox file(s) were opened and could not be turned into \
             a sealed shard. They were NOT retired, so they are still in the inbox in plaintext and \
             a re-run will try them again. The shards listed above are real; the inbox is not empty.",
            report.errors.len()
        );
        // Deliberately 1, not 3. Each entry in `report.errors` is one candidate
        // this pass actually opened and then failed on — a completed read whose
        // result failed, which is exactly what `cmd_collect` spends 1 on for
        // `report.errors`. 3 is reserved for "did not finish / never started",
        // and ingest has no such case here: the inbox was enumerated in full,
        // every candidate got its turn. Keeping the two apart is what lets a
        // caller tell "one bundle is malformed" from "the inbox was unreadable".
        return ExitCode::FAILURE;
    }
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
        move |wanted| {
            let store = BackupStore::new(cfg.clone(), machine.to_string());
            if !store.repository_exists()? {
                anyhow::bail!("destination repository is not initialised");
            }
            let mk = store::load_key_file(cfg)?;
            let observation = store.read_cumulative_sessions(&mk, Some(wanted))?;
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
    keep_ssh_masters: bool,
    trust_host: bool,
) -> ExitCode {
    let config = match config_or_refuse("dest-init") {
        Ok(config) => config,
        Err(code) => return code,
    };
    if destination.is_none() && repo.is_none() {
        eprintln!(
            "dest-init: name the destination being initialised (`--destination <name>`, or an explicit `--repo`)"
        );
        return ExitCode::from(2);
    }
    let machine = match resolve_machine("dest-init", &config, machine) {
        Ok(machine) => machine,
        Err(code) => return code,
    };
    let target = resolve_store_config(
        &config,
        destination.as_deref(),
        repo,
        key_file,
        connections,
        options,
    );

    // ADR-023 — resolve the destination's trust *before* the expensive local
    // work, because the first deployment of a machine fails here and nowhere
    // else. Both branches are explicit: `--trust-host` is the only thing in
    // this program that writes to `known_hosts`, and without it an untrusted
    // host stops the command instead of being accepted.
    if trust_host {
        if !chat_stasher::remote_err::is_remote_endpoint(&target) {
            eprintln!(
                "dest-init: --trust-host applies to a remote destination; `{}` is a local path \
                 with no host key to record.",
                target.repo_root
            );
            return ExitCode::from(2);
        }
        let known_hosts = chat_stasher::remote_err::default_known_hosts_path();
        match chat_stasher::remote_err::trust_host(&target, &known_hosts) {
            Ok(outcome) => {
                println!(
                    "[dest-init] trust-host    : {} record(s) scanned, {} newly written to {}",
                    outcome.scanned,
                    outcome.added,
                    outcome.known_hosts.display()
                );
                println!(
                    "[dest-init] trust-host    : `{}:{}` will now pass strict host key checking",
                    outcome.host, outcome.port
                );
            }
            Err(e) => {
                eprintln!("dest-init: --trust-host could not record the host key: {e}");
                // Nothing was written, so this is "the trust step did not
                // happen" rather than "the destination refused us" — the
                // command never got as far as reading the archive.
                return ExitCode::from(3);
            }
        }
    }

    // ADR-023 pre-flight: connect once, read-only, so an untrusted host or a
    // dead endpoint is diagnosed here with its own advice instead of surfacing
    // much later out of a push. 3, not 1: the destination was not read, so
    // nothing downstream may be reported as "not there".
    match chat_stasher::remote_err::preflight(&target, "dest-init") {
        chat_stasher::remote_err::Preflight::Reached { repository_exists } => {
            println!(
                "[dest-init] preflight      : reached `{}` (repository {})",
                target.repo_root,
                if repository_exists {
                    "present"
                } else {
                    "not there yet — this run creates it"
                }
            );
        }
        // The classification `preflight` now returns is for callers that have to
        // *act* on it (the wizard decides whether to offer the out-of-band check
        // at all); this command's behaviour and its exit code are unchanged, and
        // the reason is already printed by the pre-flight itself.
        chat_stasher::remote_err::Preflight::Unreachable { .. } => {
            eprintln!(
                "dest-init: INCOMPLETE exit_code=3 — the destination could not be reached, so \
                 whether a repository is already there is UNKNOWN, not empty. Nothing was \
                 collected, and no destination was created or modified."
            );
            return ExitCode::from(3);
        }
    }

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
        //
        // B82: three answers. A record we could not read used to arrive here
        // as `false` — indistinguishable from "we have never collected for
        // it" — and that `false` is what `destinit` uses to *prove* a missing
        // repository was never built. The read error is now carried through
        // instead of being flattened into the answer.
        let record = match chat_stasher::collect::destination_record(
            &state_dir,
            &chat_stasher::collect::destination_id(&cfg.repo_root),
        ) {
            Ok(chat_stasher::collect::DestinationRecord::Known(true)) => {
                chat_stasher::destinit::CollectRecord::Present
            }
            Ok(chat_stasher::collect::DestinationRecord::Known(false))
            | Ok(chat_stasher::collect::DestinationRecord::Unrecorded) => {
                chat_stasher::destinit::CollectRecord::Absent
            }
            Err(e) => {
                eprintln!(
                    "dest-init: cannot read this machine's collector record for source `{name}`: {e:#}"
                );
                eprintln!(
                    "dest-init: that record is the only thing that tells \"never built\" apart from \"built and since lost\". Without it this source can only be reported as UNKNOWN."
                );
                chat_stasher::destinit::CollectRecord::Unreadable
            }
        };
        sources.push(chat_stasher::destinit::SourceDestination {
            name: name.clone(),
            cfg,
            record,
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
            // 3, not 1: the re-collect did not produce a complete report, so
            // this pass did not finish reading the local sources. A returned
            // report with `errors` below is the separate "read and failed"
            // case that belongs to 1.
            return ExitCode::from(3);
        }
    };
    print_collect_report(&report, stage, &state_dir, &machine);
    // An error is a recognised source that collection tried to read and could
    // not process: read-completed-but-failed, exit 1. An archive gap is a
    // recognised source for which this build did not produce all records:
    // unread/incomplete, exit 3. Keep these independent for final precedence.
    let local_failed = !report.errors.is_empty();
    let local_incomplete = !report.archive_gaps.is_empty();

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
            // B82: an `Unknown` that comes from our own unreadable record is a
            // different sentence and a different thing to do about it — the
            // location answered, our memory of it did not.
            SourceStatus::Unknown if source.record_unreadable => println!(
                "    UNKNOWN: there is no repository at that location, but this machine's own \
                 collector record could not be read — so we cannot tell whether we ever collected \
                 for it. Never-built and built-then-lost look identical without that record, and \
                 the second one may mean the only copy of some sessions is gone. Fix the record \
                 (state/debts-v2.json) and re-run before treating this destination as empty."
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
            // 3, not 1: without a completed stage audit we cannot establish
            // what would be pushed, so the operation did not finish.
            return ExitCode::from(3);
        }
    };
    let mut push_failed = false;
    let mut push_not_started = false;
    if stage_shards == 0 {
        println!("[dest-init] step 3        : nothing on the stage, no snapshot created");
    } else {
        println!("[dest-init] step 3        : push the union to the new destination");
        // Same order as `cmd_push`: no archive is written to a destination
        // whose key is not on the disk, because that archive could never be
        // opened again.
        match masterkey(&target) {
            Err(e) => {
                eprintln!("dest-init: cannot put a new masterkey on the disk: {e:#}");
                eprintln!(
                    "dest-init: nothing was pushed to the new destination — an archive written \
                     under a key that is not on the disk could never be opened again."
                );
                // The archive write never began. Keep this separate from a
                // failed `store.push`: the former is exit 3, the latter 1.
                push_not_started = true;
            }
            Ok((mk, _)) => {
                let store = BackupStore::new(target.clone(), machine.clone());
                if let Err(e) = record_writer_version(stage, &machine) {
                    eprintln!("dest-init: cannot record writer version: {e:#}");
                    push_failed = true;
                } else {
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
                            chat_stasher::remote_err::eprint_remote_error("dest-init: push", &e, &target);
                            push_failed = true;
                        }
                    }
                }
            }
        }
    }
    reap_remote(&target, keep_ssh_masters);

    // `diff_complete` is an aggregate bit, not an exit-code meaning. Its
    // false cases split into sources we could not read (`Unknown` or
    // `SuspectedLoss`, exit 3) and sources we read but could not restore fully
    // (`failed_sessions`, exit 1).
    let diff_not_read = !diff.diff_complete
        && diff.sources.iter().any(|source| {
            matches!(
                source.status,
                SourceStatus::SuspectedLoss | SourceStatus::Unknown
            )
        });
    let diff_read_failed = !diff.diff_complete
        && diff
            .sources
            .iter()
            .any(|source| !source.failed_sessions.is_empty());

    // A completed read/attempted operation (1) outranks a part that was never
    // read (3), matching `cmd_collect`: the former says this pass failed, the
    // latter says its answer is unproven. The data-loss wording remains the
    // highest-priority explanation within the unread (3) family.
    if local_failed || diff_read_failed || push_failed {
        if diff_read_failed && !local_failed && !push_failed {
            let failed_sessions: usize = diff
                .sources
                .iter()
                .map(|source| source.failed_sessions.len())
                .sum();
            eprintln!(
                "dest-init: result: ERROR exit_code=1 — the difference set was read, but {failed_sessions} session(s) could not be restored. The new destination is not proven to hold the union."
            );
        } else {
            eprintln!(
                "[dest-init] result: ERROR exit_code=1 local_errors={local_failed} push_failed={push_failed}"
            );
        }
        return ExitCode::FAILURE;
    }

    // Suspected loss outranks a merely unknown difference set: the same exit
    // code, but a completely different thing to go and do about it.
    let lost = diff.suspected_loss();
    if !lost.is_empty() {
        eprintln!(
            "dest-init: result: SUSPECTED-DATA-LOSS exit_code=3 — {} destination(s) that this machine has \
             collected for before can no longer be read: {}. This is NOT \"one fewer destination to copy \
             from\": each of them may have been the last place some sessions still existed. The new \
             destination has been given everything else, but it is NOT proven to hold the union. Go find \
             those archives before you re-create anything.",
            lost.len(),
            lost.join(", "),
        );
        return ExitCode::from(3);
    }
    if diff_not_read {
        let unknown = diff.unknown();
        eprintln!(
            "dest-init: result: INCOMPLETE exit_code=3 — the difference set could not be computed in full. \
             {} destination(s) could not be consulted ({}), so it is UNKNOWN whether this new \
             destination holds the union. Unknown is not empty, and it is not loss either — we cannot tell \
             whether these were ever built. Re-run once they are reachable.",
            unknown.len().max(1),
            join_or_none(&unknown),
        );
        return ExitCode::from(3);
    }
    if local_incomplete || push_not_started {
        // Keep the established result line shape; the detailed collect/key
        // lines above identify whether this was an archive gap or a push that
        // never began. Exit 3 says the union was not proven, not that it was a
        // completed read/push which failed.
        eprintln!(
            "[dest-init] result: ERROR exit_code=3 local_errors={local_failed} push_failed={push_failed}"
        );
        return ExitCode::from(3);
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
    let config = match config_or_refuse("collect") {
        Ok(config) => config,
        Err(code) => return code,
    };
    let machine = match resolve_machine("collect", &config, machine) {
        Ok(machine) => machine,
        Err(code) => return code,
    };
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
    // Precedence is deliberate: a session we tried to read and failed on (1)
    // outranks a harness this build cannot archive at all (3). The first says
    // "this pass failed", the second says "this pass did not fail — and it did
    // not finish either".
    if !report.errors.is_empty() {
        return ExitCode::FAILURE;
    }
    if !report.archive_gaps.is_empty() {
        eprintln!(
            "collect: PARTIAL exit_code=3 — {} harness(es) recognised sessions this build cannot \
             turn into SessionRecord values, so they were not archived. The shards written above \
             are real; the archive is not complete.",
            report.archive_gaps.len()
        );
        // Deliberately 3, not 1, and certainly not 0. Reusing 1 would make
        // "this build cannot read that store" indistinguishable from "a source
        // we can read blew up", and 0 is the bug this replaces: the report
        // above already said the run was incomplete while the exit code said
        // it was clean. Same code family as `search` (see `cmd_search`):
        // 3 == did not finish / never started, 1 == finished and failed.
        // `run-once` already refuses to call an archive gap success
        // (`run_once_pass`); this only stops `collect` from being the one
        // surface that does.
        return ExitCode::from(3);
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
    // B82: `unlooked_harnesses` joins the same line rather than getting one of
    // its own — and, like the other two, only when it is non-zero, so a clean
    // pass prints exactly what it printed before.
    if report.scanner_unreadable_count > 0
        || report.scanner_unreadable_unknown > 0
        || report.scanner_unreadable_entry_count > 0
        || report.scanner_unlooked_harnesses > 0
    {
        // B90: `unreadable_sessions` is a sum over the tallies that exist, so
        // when one of them could not be taken the sum is a floor, not a total.
        // The extra field says how many harnesses that applies to; it stays
        // off the line entirely when it is zero, so a clean pass prints what
        // it printed before.
        let unknown = if report.scanner_unreadable_unknown > 0 {
            format!(
                " unreadable_sessions_unknown_in={} (the count above is a lower bound)",
                report.scanner_unreadable_unknown
            )
        } else {
            String::new()
        };
        println!(
            "[collect] scan partial   : unreadable_sessions={}{unknown} unreadable_entries={} unlooked_harnesses={} source_not_collected=true",
            report.scanner_unreadable_count,
            report.scanner_unreadable_entry_count,
            report.scanner_unlooked_harnesses
        );
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
    keep_ssh_masters: bool,
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
        keep_ssh_masters,
    );
    state.duration_ms = started.elapsed().as_millis() as u64;
    let state_dir = chat_stasher::collect::default_state_dir();
    if let Err(e) = chat_stasher::runstate::save(&state_dir, &state) {
        eprintln!("[run-once] warning: run-state not recorded: {e:#}");
    }
    code
}

/// Rebuild **this machine's** archived activity index when the archive says it
/// was written by an older chat-stasher than the one running.
///
/// Why this exists: the index and the writer version travel in the same
/// snapshot, written by the same binary, so a recorded version behind the
/// running one means the archived index is that older build's reading — and
/// `overview`, `search` and `ui` all read the archive's index, not the stage's.
/// `run-once` normally refreshes it on the way to a push, but a pass that has
/// nothing to push never gets there, which is how a machine can sit on an
/// out-of-date index for weeks after an upgrade. The writer version is read
/// first precisely so this does **not** fire on every quiet pass.
///
/// Bounded by construction, in four ways that each matter:
///   * only the resolved local machine's partition — never another machine's,
///     because another machine may be pushing it right now (ADR-016: there is
///     no cross-process lock), and a partition is only repairable by replaying
///     its own shards;
///   * at most once per pass, and only on a pass that pushes nothing (the push
///     path rebuilds the index itself);
///   * only when the archive's own writer record says "behind" — an unreadable
///     record is `unknown` and is left alone rather than guessed at;
///   * never fatal: the pass has already done its job, so a failed repair is a
///     warning that names the command to run by hand.
#[allow(clippy::too_many_arguments)]
fn repair_stale_archive_index(
    config: &Config,
    destination: Option<&str>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    machine: &str,
    workspace: &Path,
    keep_ssh_masters: bool,
) {
    let cfg = resolve_store_config(config, destination, repo, key_file, connections, options);
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            eprintln!(
                "[run-once] activity-index: cannot read the destination's masterkey, so whether \
                 this machine's archived index is current is UNKNOWN (not \"current\"): {}",
                redact_activity_index_paths(&format!("{e:#}"), &cfg, Some(workspace))
            );
            return;
        }
    };
    let statuses = match read_archive_writer_statuses(&cfg, &mk) {
        Ok(statuses) => statuses,
        Err(e) => {
            eprintln!(
                "[run-once] activity-index: cannot read the archive's writer versions, so \
                 whether this machine's archived index is current is UNKNOWN (not \"current\"): {}",
                redact_activity_index_paths(&format!("{e:#}"), &cfg, Some(workspace))
            );
            reap_remote(&cfg, keep_ssh_masters);
            return;
        }
    };
    // No snapshot for this machine yet: there is nothing archived to repair, and
    // the next pass that has something to push will create it.
    let Some(status) = statuses.iter().find(|status| status.machine == machine) else {
        return;
    };
    match sidecar::index_writer_is_behind(
        status.chat_stasher_version.as_deref(),
        status.version_unreadable,
        env!("CARGO_PKG_VERSION"),
    ) {
        Some(false) => {}
        None => {
            eprintln!(
                "[run-once] activity-index: the writer record for machine {machine} could not be \
                 read, so whether its archived index is current is UNKNOWN — leaving it alone \
                 rather than rebuilding on a guess"
            );
        }
        Some(true) => {
            println!(
                "[run-once] activity-index: machine {machine} archived writer version={} running={} \
                 — rebuilding this machine's partition only (never another machine's)",
                status
                    .chat_stasher_version
                    .as_deref()
                    .unwrap_or("not recorded (written by ≤0.3.0)"),
                env!("CARGO_PKG_VERSION")
            );
            let started = std::time::Instant::now();
            match rebuild_destination_partition_with_hook(workspace, &cfg, machine, &mk, || {}) {
                Ok((sessions, summary)) => {
                    println!(
                        "[run-once] activity-index: repaired snapshot appended sessions={} \
                         snapshots={} elapsed={}ms",
                        sessions,
                        summary.snapshots_in_repo,
                        started.elapsed().as_millis()
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[run-once] warning: activity-index repair failed, continuing — rebuild it \
                         by hand with `{}`: {}",
                        chat_stasher::doctor::activity_index_repair_command(
                            destination.unwrap_or("<destination>"),
                            machine
                        ),
                        redact_activity_index_paths(&format!("{e}"), &cfg, Some(workspace))
                    );
                }
            }
        }
    }
    reap_remote(&cfg, keep_ssh_masters);
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
    keep_ssh_masters: bool,
) -> (ExitCode, chat_stasher::runstate::RunState) {
    use chat_stasher::runstate::{RunOutcome, RunState};

    // A config that cannot be used stops the pass here, before any destination is
    // resolved. This is the path a timer takes, so the failure has to be loud in
    // both channels a sleeping user has: the exit status (launchd and systemd
    // record it, and `status` reports "no run has succeeded since") and stderr
    // (the scheduled unit redirects it to
    // `~/Library/Logs/chat-stasher/run-once.err.log`). Running the pass anyway
    // would push to the built-in default repository, which is the one outcome
    // that looks like success while copying nowhere the user named.
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => {
            eprintln!("[run-once] result: ERROR exit_code=3 config={e:#}");
            return (
                ExitCode::from(3),
                RunState::new(RunOutcome::Error, Some("config"), UNRESOLVED_MACHINE, 0),
            );
        }
    };
    let machine_name = match resolve_machine("run-once", &config, machine.as_deref()) {
        Ok(machine) => machine,
        Err(code) => {
            let state = RunState::new(
                RunOutcome::Error,
                Some("machine-identity"),
                UNRESOLVED_MACHINE,
                0,
            );
            return (code, state);
        }
    };
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
    // This guard used to refuse any shard-less stage: while readers looked only at the
    // newest snapshot per machine, an empty snapshot made the machine look as if it
    // held nothing. ADR-021 made those readers cumulative, so the guard now refuses
    // only when the stage holds neither sealed shards nor machine metadata (ADR-022).
    let (has_content, changed) = match chat_stasher::metahash::evaluate_run_once_change(
        stage,
        &machine_name,
        &state_dir,
        stage_shards,
        report.changed_records > 0 || report.shards_written > 0,
    ) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("[run-once] result: ERROR exit_code=1 stage_audit={e:#}");
            state.failed_step = Some("stage-audit".to_string());
            return (ExitCode::FAILURE, state);
        }
    };
    let only_if_changed = config.push_only_if_changed.unwrap_or(true);
    let should_push = has_content && (!only_if_changed || changed);
    if !should_push {
        println!(
            "[run-once] push skipped: changed={} push_only_if_changed={} stage_shards={}",
            changed, only_if_changed, stage_shards
        );
        // A pass that pushes rebuilds the index on the way (see below), so the
        // archived index can only be stale when the pass pushes nothing — which
        // is exactly what happens after an upgrade on a quiet machine, and what
        // used to need a hand-run `activity-index --rebuild`. Repair it here.
        repair_stale_archive_index(
            &config,
            destination.as_deref(),
            repo.clone(),
            key_file.clone(),
            connections,
            options,
            &machine_name,
            stage,
            keep_ssh_masters,
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
                        false,
                        Some(&machine_name),
                        destination.clone(),
                        repo,
                        key_file,
                        connections,
                        options,
                        keep_ssh_masters,
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

    // Rebuild the activity index before pushing, so the snapshot that leaves
    // this stage carries an index that reflects what was just collected —
    // otherwise the archived overview stays frozen at the last manual build.
    // Always a full rebuild, never an incremental update: the index is derived
    // from the stage, so after a full rebuild "the index says" and "the
    // archive holds" can never diverge, while an incremental scheme would have
    // to track exactly which shards changed since the last build — and any bug
    // in that tracking produces a stale index that looks authoritative. The
    // measured cost is ~6.5 s on a 759-session stage (0.18% of the 3600 s
    // period), which is nothing next to the correctness it buys.
    match rebuild_activity_index(stage, &machine_name, false) {
        Ok(outcome) => {
            println!(
                "[run-once] activity-index: sessions={} unknown_harness={} index={} elapsed={}ms",
                outcome.sessions_indexed,
                outcome.sessions_without_harness,
                outcome.out_path.display(),
                outcome.elapsed.as_millis()
            );
        }
        Err(err) => {
            // The index is observability, not data. Push is the primary duty of
            // this pass — the step that protects the collected shards — and a
            // failed rebuild must not block it. The index is recomputable from
            // the stage at any time (`activity-index`), so log the failure
            // loudly and continue with the push.
            eprintln!(
                "[run-once] warning: activity-index rebuild failed, continuing with push: {err}"
            );
        }
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
        keep_ssh_masters,
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
            false,
            Some(&machine_name),
            destination,
            repo,
            key_file,
            connections,
            options,
            keep_ssh_masters,
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

#[allow(clippy::too_many_arguments)]
fn cmd_schedule(
    action: Option<ScheduleAction>,
    unit: schedule::Unit,
    format: schedule::Format,
    stage: Option<&Path>,
    output: Option<PathBuf>,
    binary: Option<PathBuf>,
    destinations: Vec<String>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: Vec<String>,
    machine: Option<String>,
    shard_bucket_cap: Option<usize>,
    verify: bool,
    keep_ssh_masters: bool,
) -> ExitCode {
    let config = match config_or_refuse("schedule") {
        Ok(config) => config,
        Err(code) => return code,
    };

    if matches!(action, Some(ScheduleAction::Uninstall))
        && !matches!(format, schedule::Format::Launchd)
    {
        eprintln!("schedule uninstall: only launchd agents can be unloaded by this command");
        return ExitCode::from(2);
    }
    if matches!(action, Some(ScheduleAction::Install))
        && !matches!(format, schedule::Format::Launchd)
    {
        eprintln!("schedule install: only launchd agents can be loaded by this command");
        return ExitCode::from(2);
    }

    // The run-once timer's cadence comes from the config; the reclaim-stage timer
    // is a fixed weekly slot, so no interval is resolved for it (`render`
    // ignores the value for that unit).
    let interval = match unit {
        schedule::Unit::RunOnce => match schedule::interval_secs(&config) {
            Ok(interval) => interval,
            Err(e) => {
                eprintln!("schedule: {e:#}");
                return ExitCode::FAILURE;
            }
        },
        schedule::Unit::ReclaimStage => 0,
    };

    if unit == schedule::Unit::ReclaimStage && !destinations.is_empty() {
        eprintln!("schedule: destination is not valid with unit reclaim-stage");
        return ExitCode::from(2);
    }

    if unit == schedule::Unit::RunOnce
        && destinations.is_empty()
        && repo.is_none()
        && !config.destinations.is_empty()
        && !matches!(action, Some(ScheduleAction::Uninstall))
    {
        let mut names: Vec<&str> = config.destinations.keys().map(String::as_str).collect();
        names.sort_unstable();
        eprintln!(
            "schedule: the config declares {} destination(s) — pass `--destination <name>` (there is no default). Declared: {}",
            names.len(),
            names.join(", ")
        );
        return ExitCode::from(2);
    }

    // Only run-once takes the run-once-only forwarding slots; reject them
    // loudly for the reclaim-stage unit instead of silently dropping them.
    if unit == schedule::Unit::ReclaimStage
        && (machine.is_some() || shard_bucket_cap.is_some() || verify)
    {
        eprintln!(
            "schedule: `--unit reclaim-stage` does not forward `--destination` / `--machine` / `--shard-bucket-cap` / `--verify` (those are `run-once` slots). `--repo` / `--key-file` / `--connections` / `--option` / `--keep-ssh-masters` are forwarded to `reclaim-stage`."
        );
        return ExitCode::from(2);
    }

    let selected_destinations: Vec<Option<String>> = if unit == schedule::Unit::ReclaimStage {
        // `reclaim-stage` proves against every declared destination at once,
        // so its one unit is never destination-specific. Explicit
        // destinations were already rejected above.
        vec![None]
    } else if matches!(action, Some(ScheduleAction::Uninstall)) {
        if destinations.is_empty() {
            if config.destinations.is_empty() {
                vec![None]
            } else {
                config.destinations.keys().cloned().map(Some).collect()
            }
        } else {
            destinations.into_iter().map(Some).collect()
        }
    } else if destinations.is_empty() {
        vec![None]
    } else {
        destinations.into_iter().map(Some).collect()
    };
    for destination in selected_destinations.iter().flatten() {
        if !config.destinations.contains_key(destination) {
            eprintln!("schedule: destination {destination} is not declared in the config");
            return ExitCode::from(2);
        }
    }

    if matches!(action, Some(ScheduleAction::Uninstall)) {
        let labels = selected_destinations
            .iter()
            .map(|destination| {
                schedule::launchd_label_for_destination(unit, destination.as_deref())
            })
            .collect::<Vec<_>>();
        let launchctl = std::env::var_os("CHAT_STASHER_LAUNCHCTL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("launchctl"));
        let domain = match launchd_domain() {
            Ok(domain) => domain,
            Err(error) => {
                eprintln!("schedule uninstall: {error:#}");
                return ExitCode::FAILURE;
            }
        };
        match schedule::uninstall_launchd_agents(&config::home_dir(), &labels, &launchctl, &domain)
        {
            Ok(removed) => {
                println!("[schedule] uninstalled agents: {removed}");
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("schedule uninstall: {error:#}");
                return ExitCode::FAILURE;
            }
        }
    }

    let stage = match stage {
        Some(stage) => absolute_path(stage),
        None => {
            eprintln!("schedule: --stage is required for render and install");
            return ExitCode::from(2);
        }
    };
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("schedule: cannot resolve current executable: {error}");
            return ExitCode::FAILURE;
        }
    };
    let binary =
        match schedule::resolve_binary(binary.as_deref(), &current_exe, &config::home_dir()) {
            Ok(path) => path,
            Err(error) => {
                eprintln!("schedule: {error:#}");
                return ExitCode::from(2);
            }
        };
    let reclaim_args = schedule::ReclaimStageArgs {
        repo: repo.clone(),
        key_file,
        connections,
        options,
        keep_ssh_masters,
    };
    let mut files = Vec::new();
    for destination in selected_destinations {
        let args = schedule::RunOnceArgs {
            destination,
            repo: repo.clone(),
            key_file: reclaim_args.key_file.clone(),
            connections,
            options: reclaim_args.options.clone(),
            machine: machine.clone(),
            shard_bucket_cap,
            keep_ssh_masters,
            verify,
        };
        files.extend(schedule::render(
            unit,
            format,
            &binary,
            &stage,
            interval,
            &args,
            &reclaim_args,
            &config::home_dir(),
        ));
    }
    if matches!(action, Some(ScheduleAction::Install)) {
        let launchctl = std::env::var_os("CHAT_STASHER_LAUNCHCTL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("launchctl"));
        let domain = match launchd_domain() {
            Ok(domain) => domain,
            Err(error) => {
                eprintln!("schedule install: {error:#}");
                return ExitCode::FAILURE;
            }
        };
        match schedule::install_launchd_agents(&config::home_dir(), &files, &launchctl, &domain) {
            Ok(results) => {
                let unchanged = results
                    .iter()
                    .filter(|result| **result == schedule::InstallResult::Unchanged)
                    .count();
                println!(
                    "[schedule] installed agents: {} unchanged: {unchanged}",
                    results.len() - unchanged
                );
                return ExitCode::SUCCESS;
            }
            Err(error) => {
                eprintln!("schedule install: {error:#}");
                return ExitCode::FAILURE;
            }
        }
    }
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
    match unit {
        schedule::Unit::RunOnce => println!("[schedule] interval_secs: {interval}"),
        schedule::Unit::ReclaimStage => println!(
            "[schedule] cadence: weekly — Sun {:02}:{:02} local",
            schedule::RECLAIM_STAGE_HOUR,
            schedule::RECLAIM_STAGE_MINUTE
        ),
    }
    println!("[schedule] install is NOT automatic.");
    if paths.is_empty() {
        match format {
            schedule::Format::Launchd => println!(
                "[schedule] save each plist under \"$HOME/Library/LaunchAgents/\" using the name shown above."
            ),
            schedule::Format::Systemd => {
                println!("[schedule] save both units under \"$HOME/.config/systemd/user/\" first.")
            }
        }
        println!("[schedule] installation requires you to execute this command yourself:");
        println!("{}", schedule::install_command_for_files(format, &files));
    } else {
        println!("[schedule] you must execute this command yourself to install:");
        println!("{}", schedule::install_command(unit, format, &paths));
    }
    ExitCode::SUCCESS
}

fn launchd_domain() -> anyhow::Result<String> {
    if let Some(domain) = std::env::var_os("CHAT_STASHER_LAUNCHD_DOMAIN") {
        return Ok(domain.to_string_lossy().into_owned());
    }
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .context("resolve the current user id for launchd")?;
    if !output.status.success() {
        anyhow::bail!("id -u exited with status {}", output.status);
    }
    let uid = String::from_utf8(output.stdout).context("decode the current user id")?;
    let uid = uid.trim();
    if uid.is_empty() {
        anyhow::bail!("id -u returned an empty user id");
    }
    Ok(format!("gui/{uid}"))
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
            format!(
                "platform cell confidence is not {}",
                scanner::CONF_CONFIRMED
            )
        };
        println!("[seal] REFUSED: {reason}");
        println!("[seal] active untouched : {}", active.display());
        return ExitCode::FAILURE;
    }
    let config = match config_or_refuse("seal") {
        Ok(config) => config,
        Err(code) => return code,
    };
    let machine = match resolve_machine("seal", &config, machine) {
        Ok(machine) => machine,
        Err(code) => return code,
    };
    let session = match derive_seal_session_id(session, active) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("seal: {error}");
            return ExitCode::FAILURE;
        }
    };
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

fn derive_seal_session_id(session: Option<&str>, active: &Path) -> anyhow::Result<String> {
    session
        .map(String::from)
        .or_else(|| active.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot derive a session id from --active; provide --session <id>; no shard was written"
            )
        })
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
    if report.export_files_seen > 0 {
        println!(
            "[ingest] export files    : {} (blank lines skipped: {})",
            report.export_files_seen, report.export_blank_lines
        );
    }
    if !report.errors.is_empty() {
        // The entries, not just the count. For an export file the `source_file`
        // is `<name>#<line>`, and a count with no line number would leave the
        // user with a file that did not retire and no way to find out why.
        println!("[ingest] errors           : {}", report.errors.len());
        for entry in &report.errors {
            println!("  ! {}: {}", entry.source_file, entry.message);
        }
    }
    println!(
        "[ingest] staging machine   : sha256={}",
        store::machine_fingerprint(machine)
    );
    // The identity axis is now something a bundle may or may not carry
    // (`inbox@2`), so report what this pass actually saw instead of asserting
    // up front that it saw none.
    let mut levels: BTreeMap<&str, usize> = BTreeMap::new();
    for c in &report.consumed {
        if let Some(level) = c.identity_level.as_deref() {
            *levels.entry(level).or_default() += 1;
        }
    }
    if levels.is_empty() {
        println!(
            "[ingest] note             : no consumed bundle carried an `identity` field - the \
account axis is missing; ids are `platform.sessionId` (machine-independent)"
        );
    } else {
        let seen = levels
            .iter()
            .map(|(level, n)| format!("{level}={n}"))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "[ingest] identity axis    : {seen} (archived verbatim, NOT yet used for the id or the \
dedup key - ids stay `platform.sessionId`)"
        );
    }
}

/// Local data dir used for the default repository + key file. Delegated to
/// `config` so this path has one definition rather than three.
fn data_root() -> PathBuf {
    config::default_data_root()
}

/// Expand `~`/`~/` in a repo / key-file / option string and reject any
/// residual literal `~` component. On failure this is a usage error: exit 2,
/// like clap's own argument errors. This is the last gate before a path
/// (notably the masterkey file) reaches the filesystem — config-sourced values
/// were already expanded at load, and CLI `--repo`/`--key-file`/`--option`
/// values are expanded here, so a literal `~` never survives either route.
fn expand_path_arg(label: &str, value: &str) -> String {
    match expand_path_arg_checked(label, value) {
        Ok(path) => path,
        Err(problem) => {
            eprintln!("{problem}");
            std::process::exit(2);
        }
    }
}

/// A path argument that cannot be resolved, kept as data rather than as an exit.
///
/// Two fields because two callers render the same fact two ways:
/// [`expand_path_arg`] prints `label: message` and ends the process, and the
/// setup wizard embeds `message` in a sentence of its own and reports it as an
/// observation. One string could not serve both — the wizard's sentence would
/// grow a second `destination:` inside it — and two hand-kept copies of the same
/// message would be two chances to let them drift.
struct ResolutionProblem {
    /// The word the tool prints the message under: `repo`, `key_file`, `option`.
    label: String,
    /// The sentence itself, with no prefix.
    message: String,
}

impl std::fmt::Display for ResolutionProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.label, self.message)
    }
}

/// [`expand_path_arg`] as a `Result`, for a caller that must report the failure
/// instead of ending the process on it.
fn expand_path_arg_checked(label: &str, value: &str) -> Result<String, ResolutionProblem> {
    match chat_stasher::config::expand_and_verify(value) {
        Ok(path) => Ok(path.to_string_lossy().into_owned()),
        Err(e) => Err(ResolutionProblem {
            label: label.to_string(),
            message: e.to_string(),
        }),
    }
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
    match resolve_store_config_checked(config, destination, repo, key_file, connections, options) {
        Ok(cfg) => cfg,
        Err(problem) => {
            eprintln!("{problem}");
            std::process::exit(2);
        }
    }
}

/// [`resolve_store_config`] as a `Result`, for a caller that has to report what it
/// could not resolve instead of ending the process on it.
///
/// One caller needs that: the setup wizard. A `--json` run stopped from inside
/// the destination step would exit 2 with **nothing on stdout** — not even the
/// object that says what went wrong — and 2 is the code for a usage error, which
/// is the wrong claim about a config the run had already read and found
/// deficient. The wizard answers with the state machine it keeps apart
/// (observed / unread / incomplete) and lets the exit code follow from that.
///
/// The `destination: None` arm still calls [`store_config_from`], whose own exits
/// belong to the pre-ADR-013 single-destination mode. The wizard always names a
/// destination, so it never reaches them.
#[allow(clippy::too_many_arguments)]
fn resolve_store_config_checked(
    config: &Config,
    destination: Option<&str>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
) -> Result<StoreConfig, ResolutionProblem> {
    let Some(name) = destination else {
        if repo.is_none() && !config.destinations.is_empty() {
            let mut names: Vec<&str> = config.destinations.keys().map(String::as_str).collect();
            names.sort_unstable();
            return Err(ResolutionProblem {
                label: "destination".to_string(),
                message: format!(
                    "the config declares {} destination(s) — pass `--destination <name>` (there is \
                     no default). Declared: {}",
                    names.len(),
                    names.join(", ")
                ),
            });
        }
        return Ok(store_config_from(
            config,
            repo,
            key_file,
            connections,
            options,
        ));
    };
    let Some(entry) = config.destinations.get(name) else {
        let mut names: Vec<&str> = config.destinations.keys().map(String::as_str).collect();
        names.sort_unstable();
        return Err(ResolutionProblem {
            label: "destination".to_string(),
            message: format!(
                "`{name}` is not declared in the config. Declared: {}",
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            ),
        });
    };
    let repo_root = match repo.or_else(|| entry.repo.clone()) {
        Some(raw) => expand_path_arg_checked("repo", &raw)?,
        None => {
            return Err(ResolutionProblem {
                label: "destination".to_string(),
                message: format!("`{name}` has no `repo` set (and no --repo was given)"),
            });
        }
    };
    let mut merged: BTreeMap<String, String> = entry.options.clone();
    for kv in options {
        match kv.split_once('=') {
            Some((k, v)) => {
                merged.insert(k.to_string(), v.to_string());
            }
            None => {
                return Err(ResolutionProblem {
                    label: "option".to_string(),
                    message: format!("option must be key=value, got `{kv}`"),
                });
            }
        }
    }
    // Expand + verify every option value. Config-sourced values were already
    // expanded at load; re-running is idempotent and also covers the CLI
    // `--option`s merged in above.
    for (k, v) in &mut merged {
        *v = expand_path_arg_checked(&format!("option {k}"), v)?;
    }
    let key_file = match key_file.or_else(|| entry.key_file.clone()) {
        Some(raw) => PathBuf::from(expand_path_arg_checked("key_file", &raw)?),
        // Per-destination default: one key file per destination, so a new
        // destination never silently adopts another one's key.
        None => data_root().join(format!("masterkey-{name}.json")),
    };
    Ok(StoreConfig {
        repo_root,
        key_file,
        connections: 0,
        options: merged,
        cache_dir: entry
            .cache_dir
            .as_deref()
            .or(config.rustic_cache_dir.as_deref())
            .map(|raw| expand_path_arg_checked("cache_dir", raw).map(PathBuf::from))
            .transpose()?,
        // reason: an unset Option<bool> here means "use the default (cache on)" —
        // a config default, not an unknown read result being collapsed to false.
        no_cache: entry.no_cache.or(config.rustic_no_cache).unwrap_or(false),
    }
    .with_capped_connections(
        connections
            .or(entry.connections)
            .or(config.rustic_connections),
    ))
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
        Ok(mut options) => {
            for (k, v) in &mut options {
                *v = expand_path_arg(&format!("option {k}"), v);
            }
            let repo_root = match repo.or_else(|| config.rustic_repo.clone()) {
                Some(raw) => expand_path_arg("repo", &raw),
                None => data_root.join("repo").to_string_lossy().into_owned(),
            };
            let key_file = match key_file.or_else(|| config.rustic_key_file.clone()) {
                Some(raw) => PathBuf::from(expand_path_arg("key_file", &raw)),
                None => data_root.join("masterkey.json"),
            };
            StoreConfig {
                repo_root,
                key_file,
                connections: 0,
                options,
                cache_dir: config
                    .rustic_cache_dir
                    .as_deref()
                    .map(|raw| PathBuf::from(expand_path_arg("cache_dir", raw))),
                // reason: rustic_no_cache unset = rustic's own default (cache on);
                // a config default, never an unknown collapsed to false.
                no_cache: config.rustic_no_cache.unwrap_or(false),
            }
            .with_capped_connections(connections.or(config.rustic_connections))
        }
        Err(e) => {
            eprintln!("option: {e}");
            std::process::exit(2);
        }
    }
}

/// Load persisted masterkey or create+persist a fresh one (repo init path).
///
/// The `bool` is "this key is new". A new key that could not be written to the
/// disk is an **error**, never a warning: this repository is encrypted and
/// deduplicated, so the masterkey is the only thing that can ever re-open it.
/// Continuing past a failed key write and then archiving under that key builds
/// a repository nobody can read again — worse than not archiving at all,
/// because it also reports success. Callers must therefore ask for the key
/// *before* they write anything, and stop here if it fails.
///
/// The "key already existed" path is untouched: it neither writes nor verifies.
fn masterkey(config: &StoreConfig) -> anyhow::Result<(MasterKey, bool)> {
    match store::load_key_file_state(config) {
        store::KeyFileState::Loaded(mk) => return Ok((mk, false)),
        store::KeyFileState::Missing => {}
        store::KeyFileState::Unusable(store::KeyFileError::Read(error)) => {
            anyhow::bail!(
                "cannot read masterkey file {}: {error:#}; do not delete this file; fix the access or I/O problem and re-run",
                config.key_file.display()
            );
        }
        store::KeyFileState::Unusable(store::KeyFileError::Parse(error)) => {
            anyhow::bail!(
                "cannot parse masterkey file {}: {error:#}; do not delete this file; restore a valid copy before re-running",
                config.key_file.display()
            );
        }
    }
    let mk = MasterKey::new();
    store::persist_key_file(config, &mk)?;
    // Read it back rather than trusting the write: "persisted" is a claim about
    // what is on the disk, and this is the one file where being wrong is
    // unrecoverable. Only the serialised forms are compared — no key material
    // is printed either way.
    let written = store::load_key_file(config)
        .with_context(|| format!("re-read new masterkey {}", config.key_file.display()))?;
    if store::serialize_key(&written)? != store::serialize_key(&mk)? {
        anyhow::bail!(
            "new masterkey {} does not read back as written",
            config.key_file.display()
        );
    }
    Ok((mk, true))
}

/// Reap the ssh ControlPersist masters left behind for the backend `endpoint`
/// host of this run. No-op when `--keep-ssh-masters` is set or no `endpoint`
/// option was given (a local repo has no ssh masters to reap).
fn reap_remote(cfg: &StoreConfig, keep_ssh_masters: bool) {
    if keep_ssh_masters {
        say!("[reap] skipped (--keep-ssh-masters)");
        return;
    }
    let Some(endpoint) = cfg.options.get("endpoint") else {
        return;
    };
    let Some(host) = reap::host_of_endpoint(endpoint) else {
        eprintln!("[reap] cannot parse endpoint `{endpoint}`, no ssh master reaped");
        return;
    };
    match reap::reap_masters_for_host(&host) {
        Ok(n) => say!("[reap] host {host} · ssh masters shut down: {n}"),
        Err(e) => {
            say!("[reap] host {host} · ssh masters shut down: unknown (could not read the process list: {e})")
        }
    }
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
    keep_ssh_masters: bool,
) -> ExitCode {
    let config = match config_or_refuse("push") {
        Ok(config) => config,
        Err(code) => return code,
    };
    let machine = match resolve_machine("push", &config, machine.as_deref()) {
        Ok(machine) => machine,
        Err(code) => return code,
    };
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
        match chat_stasher::collect::inspect_stage_for_push(&config, stage, &state_dir, &machine) {
            Ok(check) => check,
            Err(e) => {
                eprintln!("push: cannot establish empty-stage safety: {e:#}");
                return ExitCode::FAILURE;
            }
        };
    println!(
        "[push] stage check   : shards={} scanner_records={} sqlite_sessions={} sqlite_unknown={} scanner_unknown={} committed_reads={}",
        stage_check.stage_shards,
        stage_check.scanner_records,
        stage_check.scanner_sqlite_sessions,
        stage_check.scanner_sqlite_unknown,
        stage_check.scanner_unknown,
        stage_check.committed_reads,
    );
    let has_meta = match chat_stasher::metahash::has_meta_files(stage, &machine) {
        Ok(has) => has,
        Err(e) => {
            eprintln!("push: cannot check stage metadata: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    // This guard used to refuse any shard-less stage: while readers looked only at the
    // newest snapshot per machine, an empty snapshot made the machine look as if it
    // held nothing. ADR-021 made those readers cumulative, so the guard now refuses
    // only when the stage holds neither sealed shards nor machine metadata (ADR-022).
    if stage_check.stage_shards == 0 && !has_meta {
        let inboxes = match inbox {
            Some(inbox) => vec![inbox],
            None => match chat_stasher::inbox::remembered_inboxes(&state_dir) {
                Ok(chat_stasher::inbox::RememberedInboxes::Known(inboxes)) => inboxes,
                // No record was ever written: this machine has genuinely never
                // ingested from an inbox, so `inboxes=0` below is a real
                // answer and the audit that follows is a real audit.
                Ok(chat_stasher::inbox::RememberedInboxes::Unrecorded) => Vec::new(),
                Err(e) => {
                    eprintln!("push: cannot read the remembered inbox list: {e:#}");
                    eprintln!(
                        "push: nothing was archived. On this line `inboxes=0` means \"this \
                         machine has no inbox\" — but the record could not be read, so the \
                         number is unknown, not zero. Auditing an empty stage against a list we \
                         failed to read would clear a run that may well have inbox content \
                         waiting, and an audit that passes because it read nothing is worse \
                         than no audit at all. Fix the collector state directory ({}) and \
                         re-run; the stage is untouched.",
                        state_dir.display()
                    );
                    // 3, not 1: same family as `cmd_search`, `cmd_collect` and
                    // the masterkey path above. The audit never started — its
                    // input was never read — so this is "did not finish", not
                    // "finished and failed".
                    return ExitCode::from(3);
                }
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
    // Deliberately before the first archive byte: a repository written under a
    // key that is not on the disk can never be opened again, so the only safe
    // order is "prove we can re-open it, then write into it".
    let (mk, key_was_new) = match masterkey(&cfg) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("push: cannot put a new masterkey on the disk: {e:#}");
            eprintln!(
                "push: nothing was archived. This repository is encrypted — an archive written \
                 under a key that is not on the disk could never be opened again, so the run stops \
                 here instead of producing one. Fix the key file location (--key-file) and re-run; \
                 the stage is untouched."
            );
            reap_remote(&cfg, keep_ssh_masters);
            // Deliberately 3, not 1, and certainly not 0. Same family as
            // `cmd_search` and `cmd_collect`: 3 == did not finish / never
            // started, 1 == finished and failed. Nothing was pushed and nothing
            // was even attempted, so this is the "never started" case.
            return ExitCode::from(3);
        }
    };
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
        "[push] ssh reap       : {}",
        if keep_ssh_masters {
            "OFF (--keep-ssh-masters)"
        } else {
            "ON"
        }
    );
    // Taken before the backup so that what gets recorded is exactly what this
    // push carried (see `metahash::record_pushed_meta_hash`).
    if let Err(e) = record_writer_version(stage, &machine) {
        eprintln!("push: cannot record writer version in stage: {e:#}");
        return ExitCode::FAILURE;
    }
    let meta_hash_before = match chat_stasher::metahash::compute_meta_hash(stage, &machine) {
        Ok(hash) => hash,
        Err(e) => {
            eprintln!("push: cannot read stage metadata: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let summary = match store.push(stage, &mk) {
        Ok(s) => s,
        Err(e) => {
            chat_stasher::remote_err::eprint_remote_error("push", &e, &cfg);
            reap_remote(&cfg, keep_ssh_masters);
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
    if let Err(e) = chat_stasher::metahash::record_pushed_meta_hash(
        &state_dir,
        &machine,
        meta_hash_before.as_deref(),
    ) {
        eprintln!("[push] warning: could not save pushed meta hash: {e:#}");
    }
    reap_remote(&cfg, keep_ssh_masters);
    ExitCode::SUCCESS
}

fn cmd_read(
    stage: &Option<PathBuf>,
    session: &Option<String>,
    all_machines: bool,
    full_ids: bool,
    machine: Option<&str>,
    destination: Option<String>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    keep_ssh_masters: bool,
) -> ExitCode {
    let config = match config_or_refuse("read") {
        Ok(config) => config,
        Err(code) => return code,
    };
    let machine = if all_machines {
        query_machine(&config, machine)
    } else {
        match resolve_machine("read", &config, machine) {
            Ok(machine) => Some(machine),
            Err(code) => return code,
        }
    };
    let cfg = resolve_store_config(
        &config,
        destination.as_deref(),
        repo,
        key_file,
        connections,
        options,
    );
    // ADR-034: one session's body is exactly what the body cache is for, and
    // `--all-machines` is exactly what it is not — that mode reads every
    // session of every machine, so filling the cache from it would evict the
    // sessions a user actually re-reads.
    let cache_policy = if all_machines {
        chat_stasher::body_cache::Policy::Bulk
    } else {
        chat_stasher::body_cache::Policy::ReadThrough
    };
    let body_cache = chat_stasher::body_cache::for_operation(&config, cache_policy);
    let store = match machine.as_deref() {
        Some(machine) => BackupStore::new(cfg.clone(), machine.to_string()),
        None => BackupStore::for_metadata_query(cfg.clone()),
    }
    .with_body_cache(body_cache.handle());
    println!(
        "[read] body cache     : {}",
        body_cache_state_line(&body_cache)
    );
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            eprintln!("read: {e}");
            reap_remote(&cfg, keep_ssh_masters);
            // Deliberately 3, not 1. A missing key means the archive was never
            // consulted, which is "did not finish / never started", not
            // "read it all and failed". Keep `read` aligned with `search` and
            // `collect`: 3 means the archive was not read; 1 means reading
            // completed and the result itself failed.
            return ExitCode::from(3);
        }
    };

    let code = if all_machines {
        cmd_read_all_machines(&store, &mk, full_ids)
    } else {
        let Some(machine) = machine.as_deref() else {
            eprintln!("read: local machine identity is required unless --all-machines is set");
            reap_remote(&cfg, keep_ssh_masters);
            return ExitCode::from(3);
        };
        let stage = match stage {
            Some(s) => s,
            None => {
                eprintln!("read: `--stage` is required unless `--all-machines` is set");
                reap_remote(&cfg, keep_ssh_masters);
                // Deliberately 2, not 1 or 3. The command is missing required
                // syntax, so this is a usage error before any archive read;
                // repository semantics reserve 2 for usage errors.
                return ExitCode::from(2);
            }
        };
        let session = match session {
            Some(s) => s,
            None => {
                eprintln!("read: `--session` is required unless `--all-machines` is set");
                reap_remote(&cfg, keep_ssh_masters);
                // Deliberately 2, not 1 or 3. The command is missing required
                // syntax, so this is a usage error before any archive read;
                // repository semantics reserve 2 for usage errors.
                return ExitCode::from(2);
            }
        };
        println!("[read] machine        : {machine}");
        println!("[read] repo           : {}", store.cfg.repo_root);
        println!(
            "[read] ssh reap       : {}",
            if keep_ssh_masters {
                "OFF (--keep-ssh-masters)"
            } else {
                "ON"
            }
        );
        let (bytez, hashes) = match store.read_session_readback(stage, session, &mk) {
            Ok(v) => v,
            Err(e) => {
                chat_stasher::remote_err::eprint_remote_error("read", &e, &cfg);
                reap_remote(&cfg, keep_ssh_masters);
                // Deliberately 3, not 1. A session readback that does not
                // finish cannot claim a complete result; this is the same
                // "did not finish" family as `search` and `collect`, while 1
                // is reserved for a read that completed and then failed.
                return ExitCode::from(3);
            }
        };
        if let Some(line) = body_cache_stats_line(&body_cache) {
            println!("[read] body cache     : {line}");
        }
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
    reap_remote(&cfg, keep_ssh_masters);
    code
}

/// `cache` — report or clear this machine's body cache (ADR-034).
///
/// Read-only apart from `clear`, which touches the cache directory and nothing
/// else: no destination is contacted, no key is read, and no archive content
/// exists here to lose.
///
/// Exit codes follow the family the rest of the CLI uses, with one deliberate
/// difference: a cache that cannot be *measured* is not a failure to report, it
/// is an unknown — the occupancy line says so and the command still exits 0,
/// the same way `doctor` reports an unmeasurable metadata cache.
fn cmd_cache(action: Option<CacheAction>) -> ExitCode {
    use chat_stasher::body_cache::RootState;

    let config = match config_or_refuse("cache") {
        Ok(config) => config,
        Err(code) => return code,
    };
    // A `[cache]` section that could not be read is neither an absent one (which
    // takes the documented default quota) nor a path problem: the quota the user
    // wrote is unknown, so the cache is off and this says which line to fix.
    if let Some(problem) = config.cache_error.as_deref() {
        eprintln!("cache: {problem}");
        eprintln!(
            "cache: the body cache is off until that value is fixed, and nothing was read or \
             deleted"
        );
        // 2, not 1: nothing was attempted. The config is the thing to fix.
        return ExitCode::from(2);
    }
    let settings = match chat_stasher::body_cache::settings_for(&config) {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!("cache: {e:#}");
            eprintln!(
                "cache: the configured location could not be resolved, so nothing was read or \
                 deleted. Fix `[cache] dir` in the config and re-run."
            );
            // 2, not 1: nothing was attempted, so this is a usage error in the
            // configured path, not "cleared and failed".
            return ExitCode::from(2);
        }
    };
    let root = settings.root.clone();

    // Is this directory chat-stasher's own? `cache clear` deletes, and the only
    // files it may delete are the ones the cache wrote, so a `[cache] dir` that
    // points at a directory the cache did not create is refused — before
    // anything is touched, and with the reason attached.
    let state = chat_stasher::body_cache::root_state(&root);

    if let Some(CacheAction::Clear) = action {
        return match state {
            RootState::Absent => {
                println!(
                    "cache: nothing to clear — no cache directory at {}",
                    root.display()
                );
                ExitCode::SUCCESS
            }
            RootState::Foreign(why) => {
                eprintln!("cache: refusing to clear {}: {why}", root.display());
                eprintln!(
                    "cache: nothing was deleted. A cache is only ever cleared inside a directory \
                     chat-stasher created itself, so point `[cache] dir` at that directory, or \
                     remove this one by hand."
                );
                // 2, not 1: nothing was attempted, and the fix is in the config.
                ExitCode::from(2)
            }
            RootState::Unknown(why) => {
                eprintln!(
                    "cache: could not clear the cache at {}: {why}",
                    root.display()
                );
                eprintln!(
                    "cache: nothing was deleted, because it could not be established that this \
                     directory is the cache's own."
                );
                ExitCode::from(2)
            }
            RootState::Cache => match settings.open().clear() {
                Ok(removed) => {
                    println!(
                        "cache: cleared {} entries ({} B) from {}",
                        removed.entries,
                        removed.bytes,
                        root.display()
                    );
                    if removed.foreign_entries > 0 {
                        println!(
                            "cache: left alone   : {} file(s) or directory(ies) here were not \
                             written by chat-stasher",
                            removed.foreign_entries
                        );
                    }
                    println!(
                        "cache: the destination still holds every archive byte; the next read of a \
                         session fetches it again"
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!(
                        "cache: could not clear the cache at {}: {e}",
                        root.display()
                    );
                    ExitCode::FAILURE
                }
            },
        };
    }

    println!("cache: root           : {}", root.display());
    println!(
        "cache: quota          : {} B{}",
        settings.max_bytes,
        if settings.max_bytes == 0 {
            " (cache off: every read goes to the destination)"
        } else {
            ""
        }
    );
    match state {
        RootState::Absent => {
            println!("cache: occupancy      : unknown (no cache directory yet)");
        }
        RootState::Foreign(why) => {
            // Not a measured zero, and not another directory's bytes presented
            // as this cache's occupancy: nothing here was measured at all.
            println!("cache: occupancy      : unknown (not a chat-stasher body cache: {why})");
            println!(
                "cache: nothing here is measured, written or deleted; point `[cache] dir` at a \
                 directory chat-stasher created, or remove this one by hand"
            );
        }
        RootState::Unknown(why) => println!("cache: occupancy      : unreadable ({why})"),
        RootState::Cache => match chat_stasher::body_cache::measure(&root) {
            Ok(Some(usage)) => {
                println!(
                    "cache: occupancy      : {} B in {} entries",
                    usage.bytes, usage.entries
                );
                if usage.foreign_entries > 0 {
                    println!(
                        "cache: foreign        : {} file(s) or directory(ies) here were not written \
                         by chat-stasher; they are not counted above, and `cache clear` leaves \
                         them alone",
                        usage.foreign_entries
                    );
                }
            }
            // The directory exists but could not be measured. Reported as an
            // unknown, never as `0 B`, which would read as "the cache is empty".
            Ok(None) => println!("cache: occupancy      : unknown (no cache directory yet)"),
            Err(e) => println!("cache: occupancy      : unreadable ({e})"),
        },
    }
    ExitCode::SUCCESS
}

/// One line saying whether this run used the body cache — and if not, which of
/// the three different reasons applies (ADR-034).
///
/// The three off states are worded apart on purpose: a user who set a quota and
/// sees `off` must be able to tell "I turned it off" from "this command is
/// bulk" from "your configured location is broken", because only the last one
/// is a problem to fix.
fn body_cache_state_line(availability: &chat_stasher::body_cache::Availability) -> String {
    use chat_stasher::body_cache::Availability;
    match availability {
        // The location is deliberately not printed here: `read`'s report is
        // pinned byte-for-byte by tests precisely because it must not depend on
        // this machine, and a cache root is a per-run path. `chat-stasher cache`
        // and `doctor` (D9) both name it.
        Availability::On(cache) => format!("on (quota={} B)", cache.max_bytes()),
        Availability::Off => "off (cache.max_bytes = 0)".to_string(),
        Availability::Bulk => "not used (bulk read, ADR-034)".to_string(),
        Availability::Unresolved(why) => {
            format!("unavailable ({why}); this read goes to the remote uncached")
        }
        // Two states, one sentence: neither is the user's decision, and in both
        // the reason says which of the two it is — a directory that is not the
        // cache's, or a `[cache]` value that could not be read. `doctor` words
        // them apart with more room than a single line has.
        Availability::Foreign(why) | Availability::Invalid(why) => {
            format!("off ({why}); this read goes to the remote uncached")
        }
    }
}

/// What the cache did during this run, or `None` when it was not installed.
///
/// `usage` is reported separately from the counters because it is a
/// measurement of the disk, and it can fail on its own: an unreadable
/// directory prints as unreadable, never as 0 bytes.
fn body_cache_stats_line(availability: &chat_stasher::body_cache::Availability) -> Option<String> {
    use chat_stasher::body_cache::Availability;
    let Availability::On(cache) = availability else {
        return None;
    };
    let stats = cache.stats();
    let usage = match cache.usage() {
        Ok(usage) => format!("{} B in {} entries", usage.bytes, usage.entries),
        Err(e) => format!("<unreadable> {e}"),
    };
    Some(format!(
        "hits={} misses={} corrupt={} stored={} skipped_too_large={} skipped_session={} errors={} usage={}",
        stats.hits,
        stats.misses,
        stats.corrupt,
        stats.stored,
        stats.skipped_too_large,
        stats.skipped_session,
        stats.errors,
        usage
    ))
}

/// `read --all-machines` — group every snapshot by hostname, take each
/// hostname's newest snapshot, walk its `sessions/<machine>/` subtree, and
/// report per-session shard count / byte length / sha256. Privacy line: only
/// ids, counts, lengths and digests are printed — never session content.
fn cmd_read_all_machines(store: &BackupStore, mk: &MasterKey, full_ids: bool) -> ExitCode {
    println!("[read] mode           : all-machines (newest snapshot per hostname)");
    println!("[read] repo           : {}", store.cfg.repo_root);
    let report = match store.read_all_machines(mk) {
        Ok(r) => r,
        Err(e) => {
            chat_stasher::remote_err::eprint_remote_error("read", &e, &store.cfg);
            // Deliberately 3, not 1. `read_all_machines` failed before it
            // produced a complete archive report, so the archive was not read
            // to completion; 1 is for a completed read whose result failed.
            return ExitCode::from(3);
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
                "    session {:<15} shards={:<3} bytes={:<10} sha256={}",
                display_session_id(&s.session_id, full_ids),
                s.shard_count,
                s.concat_bytes,
                s.sha256
            );
        }
    }
    for w in &report.warnings {
        println!("  WARN: {w}");
    }
    if !report.complete() {
        println!(
            "read: PARTIAL exit_code=3 — the sessions listed above are real, but {} snapshot(s) \
             could not be opened, so the machines they belong to are listed with an empty session \
             set they did not earn. Absence below is not proof of absence in the archive.",
            report.warnings.len()
        );
        // Deliberately 3, not 1. Same judgement as `cmd_search`'s PARTIAL and
        // `cmd_read`'s repository failure: the archive was not read to
        // completion, so this is "did not finish", not "read it all and the
        // result failed". Until now this was the one surface in the family that
        // printed `WARN:` lines and still exited 0 — a scripted read-back could
        // not tell a machine that backed nothing up from one we could not open.
        return ExitCode::from(3);
    }
    ExitCode::SUCCESS
}

/// `verify` — prove the archive is intact, level by level. Each level prints
/// its own verdict; exit 1 means a completed check failed, and 3 means reading failed.
#[allow(clippy::too_many_arguments)]
fn cmd_verify(
    level: VerifyLevel,
    stage: &Option<PathBuf>,
    full_ids: bool,
    machine: Option<&str>,
    destination: Option<String>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    keep_ssh_masters: bool,
) -> ExitCode {
    let config = match config_or_refuse("verify") {
        Ok(config) => config,
        Err(code) => return code,
    };
    let machine = query_machine(&config, machine);
    let cfg = resolve_store_config(
        &config,
        destination.as_deref(),
        repo,
        key_file,
        connections,
        options,
    );
    let store = match machine.as_deref() {
        Some(machine) => BackupStore::new(cfg.clone(), machine.to_string()),
        None => BackupStore::for_metadata_query(cfg.clone()),
    };
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            eprintln!("verify: {e}");
            reap_remote(&cfg, keep_ssh_masters);
            return ExitCode::from(3);
        }
    };
    let need_stage = matches!(level, VerifyLevel::L3 | VerifyLevel::All);
    let stage = match (need_stage, stage) {
        (true, Some(s)) => s.clone(),
        (true, None) => {
            eprintln!("verify: `--stage` is required for level l3 / all");
            reap_remote(&cfg, keep_ssh_masters);
            return ExitCode::from(2);
        }
        (false, _) => PathBuf::from("."),
    };

    println!("[verify] repo           : {}", store.cfg.repo_root);
    println!(
        "[verify] machine        : {}",
        machine.as_deref().unwrap_or(UNRESOLVED_MACHINE)
    );
    println!(
        "[verify] ssh reap       : {}",
        if keep_ssh_masters {
            "OFF (--keep-ssh-masters)"
        } else {
            "ON"
        }
    );

    let mut failed = 0usize;
    let mut unreadable = 0usize;
    match level {
        VerifyLevel::L1 => run_check(
            &store,
            &mk,
            false,
            "L1 structure",
            &mut failed,
            &mut unreadable,
        ),
        VerifyLevel::L2 => run_check(
            &store,
            &mk,
            true,
            "L2 content",
            &mut failed,
            &mut unreadable,
        ),
        VerifyLevel::L3 => {
            println!("[verify] stage          : {}", stage.display());
            run_reconcile(&store, &mk, &stage, full_ids, &mut failed, &mut unreadable);
        }
        VerifyLevel::All => {
            run_check(
                &store,
                &mk,
                false,
                "L1 structure",
                &mut failed,
                &mut unreadable,
            );
            run_check(
                &store,
                &mk,
                true,
                "L2 content",
                &mut failed,
                &mut unreadable,
            );
            println!("[verify] stage          : {}", stage.display());
            run_reconcile(&store, &mk, &stage, full_ids, &mut failed, &mut unreadable);
        }
    }

    reap_remote(&cfg, keep_ssh_masters);
    if unreadable > 0 {
        println!("[verify] RESULT         : INCOMPLETE ({unreadable} level(s) unreadable)");
        ExitCode::from(3)
    } else if failed == 0 {
        println!("[verify] RESULT         : OK");
        ExitCode::SUCCESS
    } else {
        println!("[verify] RESULT         : FAILED ({failed} level(s) reported failures)");
        ExitCode::FAILURE
    }
}

fn run_check(
    store: &BackupStore,
    mk: &MasterKey,
    data: bool,
    name: &str,
    failed: &mut usize,
    unreadable: &mut usize,
) {
    match store.check_repo(mk, data) {
        Ok(summary) => {
            print_check_summary(&summary, name);
            if !summary.ok() {
                *failed += 1;
            }
        }
        Err(e) => {
            chat_stasher::remote_err::eprint_remote_error(
                &format!("verify: {name}"),
                &e,
                &store.cfg,
            );
            *unreadable += 1;
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

fn run_reconcile(
    store: &BackupStore,
    mk: &MasterKey,
    stage: &Path,
    full_ids: bool,
    failed: &mut usize,
    unreadable: &mut usize,
) {
    match store.reconcile_manifest(mk, stage) {
        Ok(report) => {
            print_reconcile(&report, full_ids);
            if !report.ok() {
                *failed += 1;
            }
        }
        Err(e) => {
            chat_stasher::remote_err::eprint_remote_error("verify: L3 reconcile", &e, &store.cfg);
            *unreadable += 1;
        }
    }
}

fn print_reconcile(r: &ReconcileReport, full_ids: bool) {
    println!(
        "[verify] L3 reconcile     : machines={} expected={} took {:?}",
        r.machines,
        r.rows.len(),
        r.duration
    );
    for row in &r.rows {
        let session = display_session_id(&row.session_id, full_ids);
        let mark = match &row.outcome {
            SessionOutcome::Match => "ok ",
            SessionOutcome::Unverifiable { .. } => "?? ",
            _ => "!! ",
        };
        match &row.outcome {
            SessionOutcome::Match => {
                let note = if row.basis == ExpectationBasis::StoredManifest {
                    " [stored-manifest]"
                } else {
                    ""
                };
                println!(
                    "  {mark} {:<12} {:<20} shards={:<2} bytes={:<10} sha={}{note}",
                    row.machine, session, row.observed_shards, row.observed_bytes, row.observed_sha
                );
            }
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
            SessionOutcome::Unverifiable { reason } => println!(
                "  {mark} {:<12} {:<20} UNVERIFIABLE ({reason})",
                row.machine, session
            ),
        }
    }
    for (m, s) in &r.extra_in_archive {
        println!(
            "  !? {m:<12} {:<20} in archive but NOT in expected manifest (informational)",
            display_session_id(s, full_ids)
        );
    }
    let verdict = if r.ok() {
        "OK".to_string()
    } else {
        let u = r.unverifiable();
        let f = r.failed();
        if u > 0 && f > 0 {
            format!("FAILED (failed={f}, unverifiable={u})")
        } else if u > 0 {
            format!("FAILED (unverifiable={u})")
        } else {
            "FAILED".to_string()
        }
    };
    println!("[verify] L3 verdict       : {verdict}");
}

/// `reclaim-stage` — reclaim the sealed shard body once every declared
/// destination proves it holds each session (ADR-020 Phase 4).
///
/// The destination set is the whole debt set: with a `destinations` table the
/// command proves against *every* declared destination; without one it proves
/// against the single default repository. A destination that is unreachable or
/// read only partially is "unproven" and blocks the reclaim — never "does
/// not have it" and never "has it". A blocked reclaim deletes nothing and
/// exits 1.
fn cmd_reclaim_stage(
    stage: &PathBuf,
    apply: bool,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    keep_ssh_masters: bool,
) -> ExitCode {
    let config = match config_or_refuse("reclaim-stage") {
        Ok(config) => config,
        Err(code) => return code,
    };
    let multi = !config.destinations.is_empty();
    if multi {
        if repo.is_some() || key_file.is_some() {
            eprintln!(
                "reclaim-stage: `--repo` / `--key-file` only apply to a single-destination config; ignoring them (this run proves every declared destination)"
            );
        }
    }
    let destinations: Vec<NamedStore> = if multi {
        config
            .destinations
            .keys()
            .map(|name| NamedStore {
                name: name.clone(),
                cfg: resolve_store_config(&config, Some(name), None, None, None, &[]),
            })
            .collect()
    } else {
        vec![NamedStore {
            name: "default".to_string(),
            cfg: store_config_from(&config, repo, key_file, connections, options),
        }]
    };
    let mut destinations = destinations;
    destinations.sort_by(|a, b| a.name.cmp(&b.name));

    println!("[reclaim-stage] stage        : {}", stage.display());
    println!(
        "[reclaim-stage] mode         : {}",
        if apply { "apply" } else { "dry-run" }
    );
    println!(
        "[reclaim-stage] destinations : {}",
        destinations
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let report = match stagereclaim::reclaim_stage(stage, &destinations, apply) {
        Ok(report) => report,
        Err(e) => {
            let primary_cfg = destinations
                .iter()
                .find(|d| {
                    d.cfg.repo_root.starts_with("opendal:")
                        || d.cfg.options.contains_key("endpoint")
                })
                .map(|d| &d.cfg)
                .unwrap_or(&destinations[0].cfg);
            chat_stasher::remote_err::eprint_remote_error("reclaim-stage", &e, primary_cfg);
            for dest in &destinations {
                reap_remote(&dest.cfg, keep_ssh_masters);
            }
            return ExitCode::FAILURE;
        }
    };
    for dest in &destinations {
        reap_remote(&dest.cfg, keep_ssh_masters);
    }

    println!(
        "[reclaim-stage] candidates   : {} session(s) / {}",
        report.candidates.len(),
        fmt_bytes(report.candidate_bytes)
    );
    if report.already_reclaimed > 0 {
        println!(
            "[reclaim-stage] already reclaimed: {} session(s) (summary retained, no body)",
            report.already_reclaimed
        );
    }

    if report.blocked() {
        for block in &report.blocked {
            let word = match &block.kind {
                BlockedKind::Unreachable { .. } => "unreachable",
                BlockedKind::PartialRead { .. } => "could not be fully read",
                BlockedKind::SessionNotHeld => "does not hold the archived session(s)",
                BlockedKind::TripleMismatch => "holds a different digest for the session(s)",
            };
            println!(
                "  blocked: destination \"{}\" {word} — {} session(s) / {} held back",
                block.destination,
                block.sessions,
                fmt_bytes(block.bytes)
            );
            match &block.kind {
                BlockedKind::Unreachable { error } => {
                    println!("    reason: could not read the destination: {error}");
                }
                BlockedKind::PartialRead { detail } => {
                    println!("    reason: destination was read but not to completion: {detail}");
                }
                BlockedKind::SessionNotHeld | BlockedKind::TripleMismatch => {}
            }
        }
        println!(
            "[reclaim-stage] RESULT       : BLOCKED — nothing was deleted; fix the destination(s) above and re-run"
        );
        return ExitCode::FAILURE;
    }

    if !apply {
        println!(
            "[reclaim-stage] reclaimable  : {} session(s) / {} — dry run, pass `--apply` to delete the body",
            report.candidates.len(),
            fmt_bytes(report.candidate_bytes)
        );
        println!("[reclaim-stage] RESULT       : OK (dry run)");
        return ExitCode::SUCCESS;
    }

    println!("[reclaim-stage] summary      : retained per-session summaries written before delete");
    for r in &report.reclaimed {
        println!(
            "  reclaimed: {} {} {} shard(s) / {} (summary retained)",
            r.machine,
            short_session_id(&r.session_id),
            r.shard_count,
            fmt_bytes(r.bytes)
        );
    }
    println!(
        "[reclaim-stage] reclaimed    : {} session(s) / {}",
        report.reclaimed.len(),
        fmt_bytes(report.candidate_bytes)
    );
    println!(
        "[reclaim-stage] next         : `verify --level l3 --stage <stage>` now reconciles against the retained summaries"
    );
    println!("[reclaim-stage] RESULT       : OK");
    ExitCode::SUCCESS
}

/// Human-readable byte size, same shape as `doctor`'s (1 KiB = 1024 B).
fn fmt_bytes(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1} GiB", bytes as f64 / (1 << 30) as f64)
    } else if bytes >= 1 << 20 {
        format!("{:.1} MiB", bytes as f64 / (1 << 20) as f64)
    } else if bytes >= 1 << 10 {
        format!("{:.1} KiB", bytes as f64 / (1 << 10) as f64)
    } else {
        format!("{bytes} B")
    }
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
    chat_stasher::id::short_session_id(id)
}

fn display_session_id(id: &str, full_ids: bool) -> String {
    if full_ids {
        id.to_string()
    } else {
        short_session_id(id)
    }
}

#[cfg(test)]
mod decision_surface_tests {
    use super::*;
    use clap::CommandFactory;
    use std::fs;

    /// A report for a run that stopped before the remote step could do
    /// anything, used by the payload tests below.
    fn setup_remote_not_attempted(name: Option<&str>, why: &str) -> SetupRemoteReport {
        SetupRemoteReport {
            name: name.map(str::to_string),
            kind: None,
            config: SetupDestinationConfig::NotWritten {
                why: why.to_string(),
            },
            reach: SetupReach::NotAttempted {
                why: why.to_string(),
            },
            trust: SetupTrust::NotRequired,
            dest_init: SetupDestinationInit::NotRun {
                why: why.to_string(),
            },
            credentials: SetupRemoteCredentials::NotChecked {
                why: why.to_string(),
            },
        }
    }

    #[test]
    fn setup_json_reports_named_missing_stage_without_prompting() {
        let report = report_with_sessions(0);
        let value: serde_json::Value = serde_json::from_str(&setup_json_payload(
            scanner::scan_report_json(&report),
            None,
            false,
            None,
            None,
            &setup_remote_not_attempted(None, "no stage was given"),
            &["stage"],
            &[],
            &[],
            2,
        ))
        .expect("the setup payload is one JSON object");
        assert_eq!(value["command"], "setup");
        assert_eq!(value["exit_code"], 2);
        assert_eq!(value["missing_parameters"], serde_json::json!(["stage"]));
        assert_eq!(value["steps"]["stage"], "missing");
        // A stage that was never given means no pass ran, so the chain is its
        // own tagged state and not three absent-looking links.
        assert_eq!(value["chain"]["kind"], "not_attempted");
        assert_eq!(value["masterkey"]["kind"], "absent");
        // The remote step did not run either, and an unnamed destination is a
        // *skip* — the JSON says so and carries what that costs, rather than
        // carrying a bare "skipped" a reader could take for "no problem".
        assert_eq!(value["steps"]["destination"], "skipped");
        assert_eq!(value["destination"]["kind"], "skipped");
        assert!(value["destination"]["consequence"]
            .as_str()
            .is_some_and(|text| text.contains("loses the archive")));
        // Nothing was measured at the destination, so no measurement field may
        // be present for a reader to mistake for one.
        assert!(value["destination"].get("name").is_none());
        assert_eq!(value["unread"], serde_json::json!([]));
        assert_eq!(value["incomplete"], serde_json::json!([]));
    }

    /// The declaration is a sentence the user has to mean. Anything shorter,
    /// differently cased or differently worded is not it — and a wizard that
    /// accepted `yes` would be asking a question nobody reads.
    #[test]
    fn only_the_exact_sentence_counts_as_the_masterkey_declaration() {
        assert!(setup_declaration_given("I saved it elsewhere"));
        assert!(setup_declaration_given("  I saved it elsewhere\n"));
        assert!(!setup_declaration_given("i saved it elsewhere"));
        assert!(!setup_declaration_given("yes"));
        assert!(!setup_declaration_given("y"));
        assert!(!setup_declaration_given("I saved it"));
        assert!(!setup_declaration_given(""));
    }

    /// An empty answer takes the offered default; a typed path is expanded
    /// through the same boundary the config loader uses, so a `~` cannot reach
    /// the filesystem as a directory literally named `~`.
    #[test]
    fn the_stage_answer_takes_the_default_or_expands_a_tilde() {
        let default = PathBuf::from("/fixture/default-stage");
        assert_eq!(
            setup_stage_answer("", Some(&default)).unwrap(),
            Some(default.clone()),
            "an empty answer must take the default offered in the prompt"
        );
        assert_eq!(
            setup_stage_answer("   ", Some(&default)).unwrap(),
            Some(default),
            "whitespace is an empty answer, not a path named spaces"
        );
        let home = chat_stasher::config::home_dir();
        assert_eq!(
            setup_stage_answer("~/stash/stage", None).unwrap(),
            Some(home.join("stash").join("stage")),
            "a typed ~/path must expand, never survive as a literal ~ component"
        );
        assert!(
            setup_stage_answer("~someone-else/stage", None).is_err(),
            "another user's home is rejected, not silently treated as relative"
        );
    }

    #[test]
    fn setup_scan_renderer_is_the_status_scan_renderer() {
        let report = report_with_sessions(3);
        assert_eq!(render_setup_scan(&report), render_status(&report, false));
    }

    #[test]
    fn setup_has_no_secret_valued_argv_options() {
        let cli = Cli::command();
        let setup = cli
            .find_subcommand("setup")
            .expect("setup subcommand exists");
        let args: Vec<String> = setup
            .get_arguments()
            .filter(|arg| !arg.is_positional())
            .map(|arg| arg.get_id().as_str().to_owned())
            .collect();
        assert_eq!(
            args,
            [
                "stage",
                "destination",
                "install_schedule",
                "masterkey_saved_elsewhere",
                "json",
                "remote",
                "remote_endpoint",
                "remote_user",
                "remote_ssh_key",
                "remote_bucket",
                "remote_region",
                "remote_root",
                "remote_access_key_id_env",
                "remote_secret_key_env",
                "trust_host",
            ],
            "the list is a tripwire, not the invariant: every option of setup must be a name, a \
             location or a declaration, and none may take a secret as its value. A new flag has \
             to be considered here before it can ship."
        );

        // The invariant itself, tested by behaviour rather than by reading a
        // list of names. Two guards, and the second exists because a test showed
        // the first cannot be enough: an AWS access key id is all uppercase
        // letters and digits, so it *is* a legal variable name by shape.
        for pasted in [
            // Caught by shape: lowercase, `/` and `+` are not variable-name
            // characters.
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "short",
            "1STARTSWITHADIGIT",
            "",
        ] {
            for flag in ["remote_access_key_id_env", "remote_secret_key_env"] {
                let mut args = SetupRemoteArgs::default();
                match flag {
                    "remote_access_key_id_env" => {
                        args.remote_access_key_id_env = Some(pasted.to_string())
                    }
                    _ => args.remote_secret_key_env = Some(pasted.to_string()),
                }
                let refusal = setup_remote_env_reference_error(&args);
                assert!(
                    refusal.is_some(),
                    "`{pasted}` is not a variable name and must be refused by {flag}"
                );
                let (refused_flag, why) = refusal.expect("checked above");
                assert!(
                    !why.contains(pasted) || pasted.is_empty(),
                    "the refusal must never echo the value: a value of this shape is more likely \
                     to be the secret than a typo; why={why}"
                );
                assert!(
                    refused_flag.starts_with("--remote-"),
                    "the refusal names the flag: {refused_flag}"
                );
            }
        }

        // The shape a variable-name check cannot catch, refused by prefix. The
        // assertion above this loop is only half the story and this is the other
        // half: `AKIA…` passes `is_env_reference_name` — measured, not assumed —
        // so it needs its own rule.
        assert!(
            chat_stasher::config::is_env_reference_name("AKIAIOSFODNN7EXAMPLE"),
            "this is the gap the prefix rule exists to close: if a variable-name check ever does \
             start refusing this, the prefix rule is no longer the reason it is refused"
        );
        for pasted in ["AKIAIOSFODNN7EXAMPLE", "ASIAIOSFODNN7EXAMPLE"] {
            let args = SetupRemoteArgs {
                remote_access_key_id_env: Some(pasted.to_string()),
                ..SetupRemoteArgs::default()
            };
            let refusal = setup_remote_env_reference_error(&args);
            assert!(
                refusal.is_some(),
                "`{pasted}` is an access key id, not a variable name"
            );
            assert!(!refusal.expect("checked above").1.contains(pasted));
        }

        // And a legal variable name is accepted, so the guard cannot be
        // satisfied by refusing everything.
        let args = SetupRemoteArgs {
            remote_access_key_id_env: Some("CHAT_STASHER_R2_ACCESS_KEY_ID".to_string()),
            remote_secret_key_env: Some("_X9".to_string()),
            ..SetupRemoteArgs::default()
        };
        assert_eq!(setup_remote_env_reference_error(&args), None);
    }

    /// ADR-039 decision 3 is a *switch*, not a sentence in three places. This
    /// pins which way it is currently set and that the other candidate is still
    /// reachable — so a change to the recommendation is a change to one
    /// constant, and flipping it back is not something a reader has to hunt for.
    #[test]
    fn the_recommended_remote_is_one_constant_and_the_other_stays_selectable() {
        assert_eq!(SETUP_RECOMMENDED_REMOTE, SetupRemoteKind::S3);
        assert_ne!(
            SETUP_RECOMMENDED_REMOTE,
            SetupRemoteKind::Sftp,
            "the alternative must stay distinguishable, or `--remote sftp` would read as the \
             recommendation"
        );
        // Both kinds can spell a destination the tool will then use.
        assert_eq!(SetupRemoteKind::Sftp.repo(), "opendal:sftp");
        assert_eq!(SetupRemoteKind::S3.repo(), "opendal:s3");
        assert_ne!(
            SetupRemoteKind::Sftp.repo(),
            SetupRemoteKind::S3.repo(),
            "two kinds that select the same backend is one kind with two names"
        );

        // And the prompt offers the recommendation first, which is what makes
        // "press enter" and "(recommended)" the same fact.
        let candidates = setup_remote_candidates();
        assert_eq!(
            candidates.len(),
            2,
            "every kind must be offered, or one of them stops being selectable \
             (ADR-039 decision 3: the others stay explicitly selectable)"
        );
        assert_eq!(
            candidates[0], SETUP_RECOMMENDED_REMOTE,
            "the first candidate carries both the default and the mark"
        );
        assert!(
            candidates.contains(&SetupRemoteKind::Sftp),
            "the alternative is still offered when it is not the recommendation"
        );
    }

    /// What a credential flag can put in the config: a reference, never a value.
    #[test]
    fn the_written_credentials_are_references_and_the_safety_switches_are_present() {
        let args = SetupRemoteArgs {
            remote: Some(SetupRemoteKind::S3),
            remote_endpoint: Some("https://example.invalid".to_string()),
            remote_bucket: Some("a-bucket".to_string()),
            remote_region: "auto".to_string(),
            remote_access_key_id_env: Some("VAR_A".to_string()),
            remote_secret_key_env: Some("VAR_B".to_string()),
            ..SetupRemoteArgs::default()
        };
        let options = setup_remote_options(SetupRemoteKind::S3, &args);
        assert_eq!(options["access_key_id"], "env:VAR_A");
        assert_eq!(options["secret_access_key"], "env:VAR_B");
        assert_eq!(options["region"], "auto");
        // §4.5 requires both: without them a missing or mistyped credential can
        // fall through to the ambient AWS environment, to `~/.aws/`, or to the
        // instance metadata service.
        assert_eq!(options["disable_config_load"], "true");
        assert_eq!(options["disable_ec2_metadata"], "true");
        // Every value that is not one of the two references is a location or a
        // switch, so nothing in this table can be a secret.
        for (key, value) in &options {
            assert!(
                !value.starts_with("env:") || key == "access_key_id" || key == "secret_access_key",
                "only the two credential options may be references: {key}"
            );
        }

        // The SFTP recipe carries no credential at all — its `key` is a path to
        // a private key on disk, which is a location — and it deliberately does
        // NOT write `known_hosts_strategy`, because `docs/install.md` §4.4
        // states as a property of this project that it sets none of that for
        // you. Writing `strict` would pin the same behaviour while making that
        // sentence false.
        let sftp = setup_remote_options(
            SetupRemoteKind::Sftp,
            &SetupRemoteArgs {
                remote: Some(SetupRemoteKind::Sftp),
                remote_endpoint: Some("ssh://example.invalid:23".to_string()),
                remote_user: Some("u123".to_string()),
                remote_ssh_key: Some("~/.ssh/id_ed25519".to_string()),
                ..SetupRemoteArgs::default()
            },
        );
        assert_eq!(sftp["user"], "u123");
        assert!("~/.ssh/id_ed25519" == sftp["key"]);
        assert!(
            !sftp.contains_key("known_hosts_strategy"),
            "the default is strict and the docs say this project does not set that option"
        );
        assert!(
            !sftp.contains_key("access_key_id"),
            "an SFTP destination has no S3 credential to name"
        );
    }

    /// A kind's required parameters are named, and only when the kind was asked
    /// for — the WIZ-1 contract extended to the step WIZ-3 adds.
    #[test]
    fn the_remote_kinds_name_what_they_cannot_be_written_without() {
        assert_eq!(
            setup_remote_missing(&SetupRemoteArgs::default(), false),
            ["remote"]
        );
        assert_eq!(
            setup_remote_missing(
                &SetupRemoteArgs {
                    remote: Some(SetupRemoteKind::Sftp),
                    ..SetupRemoteArgs::default()
                },
                false
            ),
            ["remote_endpoint", "remote_user"]
        );
        assert_eq!(
            setup_remote_missing(
                &SetupRemoteArgs {
                    remote: Some(SetupRemoteKind::S3),
                    remote_endpoint: Some("https://example.invalid".to_string()),
                    ..SetupRemoteArgs::default()
                },
                false
            ),
            [
                "remote_bucket",
                "remote_access_key_id_env",
                "remote_secret_key_env"
            ],
            "the snapshot root and the ssh key are optional; the bucket and both credential \
             variable names are not"
        );
        // An adopted destination needs no parameters at all, because nothing
        // will be written from them. This is the case a test caught: demanding a
        // kind for a destination that is already in the file made a complete
        // command line fail.
        assert!(
            setup_remote_missing(&SetupRemoteArgs::default(), true).is_empty(),
            "a destination already declared in the config needs no --remote kind"
        );
        assert!(
            setup_remote_missing(
                &SetupRemoteArgs {
                    remote: Some(SetupRemoteKind::S3),
                    remote_endpoint: Some("https://example.invalid".to_string()),
                    ..SetupRemoteArgs::default()
                },
                true
            )
            .is_empty(),
            "an adopted destination needs none of the kind's parameters either"
        );
        // The two kinds' requirements are disjoint in the parameters they add,
        // so a name reported missing cannot belong to the other kind's recipe.
        let sftp: Vec<&str> = SetupRemoteKind::Sftp.required_parameters().to_vec();
        let s3: Vec<&str> = SetupRemoteKind::S3.required_parameters().to_vec();
        assert!(sftp.contains(&"remote_user") && !s3.contains(&"remote_user"));
        assert!(s3.contains(&"remote_bucket") && !sftp.contains(&"remote_bucket"));
    }

    /// The exit code's precedence, in one place. 3 outranks 1 outranks 2, and
    /// the reason 3 is first is the reason the code exists: what was not read
    /// proves nothing, so a run that could not read its destination must not
    /// report the code that means "finished reading".
    #[test]
    fn unread_outranks_incomplete_outranks_a_missing_parameter() {
        assert_eq!(setup_exit_code(&[], &[], &[]), 0);
        assert_eq!(setup_exit_code(&["x"], &[], &[]), 2);
        assert_eq!(setup_exit_code(&[], &["x"], &[]), 1);
        assert_eq!(setup_exit_code(&["x"], &["y"], &[]), 1);
        assert_eq!(setup_exit_code(&[], &[], &["z"]), 3);
        assert_eq!(setup_exit_code(&["x"], &[], &["z"]), 3);
        assert_eq!(setup_exit_code(&["x"], &["y"], &["z"]), 3);
    }

    /// The remote step's own three-state discipline, state by state.
    ///
    /// This is the table that decides the exit code, so it is checked directly
    /// rather than through a process: a state whose gap lists do not match what
    /// it says would give a wrapper a code that contradicts the object.
    #[test]
    fn every_remote_state_reports_the_gaps_it_actually_has() {
        let base = |config, reach, trust, dest_init| SetupRemoteReport {
            name: Some("d".to_string()),
            kind: Some(SetupRemoteKind::S3),
            config,
            reach,
            trust,
            dest_init,
            credentials: SetupRemoteCredentials::NoneNamed,
        };
        // A skipped step is not an unfinished one: ADR-039 decision 2 step 3
        // makes skipping allowed, and the terminal says what it costs.
        let skipped = SetupRemoteReport {
            name: None,
            kind: None,
            config: SetupDestinationConfig::NotWritten {
                why: "no destination was named".to_string(),
            },
            reach: SetupReach::NotAttempted {
                why: "no destination was named".to_string(),
            },
            trust: SetupTrust::NotRequired,
            dest_init: SetupDestinationInit::NotRun {
                why: "no destination was named".to_string(),
            },
            credentials: SetupRemoteCredentials::NotChecked {
                why: "no destination was named".to_string(),
            },
        };
        assert_eq!(skipped.outcome(), "skipped");
        assert!(skipped.gaps().unread.is_empty() && skipped.gaps().incomplete.is_empty());

        // A destination that was reached, written and seeded: the only state
        // that carries no gap at all.
        let done = base(
            SetupDestinationConfig::Written,
            SetupReach::Reached {
                repository_exists: true,
            },
            SetupTrust::NotRequired,
            SetupDestinationInit::Ran { exit_code: Some(0) },
        );
        assert_eq!(done.outcome(), "reachable");
        assert!(done.gaps().unread.is_empty() && done.gaps().incomplete.is_empty());

        // An unknown host, unanswered: the destination was not read at all,
        // and nothing was written to known_hosts.
        let stopped = base(
            SetupDestinationConfig::Written,
            SetupReach::UntrustedHost {
                host: "h".to_string(),
                port: 23,
            },
            SetupTrust::Required,
            SetupDestinationInit::NotRun {
                why: "not declared".to_string(),
            },
        );
        assert_eq!(stopped.outcome(), "unread");
        assert!(!stopped.gaps().unread.is_empty());
        assert!(stopped.gaps().incomplete.is_empty());

        // A changed host key is the same 3 and never a trust question: there is
        // no decision to offer, so it must not be reported in the trust field as
        // one that could be answered.
        let changed = base(
            SetupDestinationConfig::Written,
            SetupReach::HostKeyChanged {
                host: "h".to_string(),
                port: 23,
                why: "differs".to_string(),
            },
            SetupTrust::NotRequired,
            SetupDestinationInit::NotRun {
                why: "not attempted".to_string(),
            },
        );
        assert_eq!(changed.outcome(), "unread");
        assert!(!changed.gaps().unread.is_empty());

        // `dest-init` exiting 3 is the same statement it is everywhere else.
        let init_unread = base(
            SetupDestinationConfig::Written,
            SetupReach::Reached {
                repository_exists: false,
            },
            SetupTrust::NotRequired,
            SetupDestinationInit::Ran { exit_code: Some(3) },
        );
        assert_eq!(init_unread.outcome(), "unread");
        assert!(!init_unread.gaps().unread.is_empty());

        // `dest-init` exiting 1 read the destination and failed: that is
        // incomplete, not unread, and the two must not be the same code.
        let init_failed = base(
            SetupDestinationConfig::Written,
            SetupReach::Reached {
                repository_exists: false,
            },
            SetupTrust::NotRequired,
            SetupDestinationInit::Ran { exit_code: Some(1) },
        );
        assert_eq!(init_failed.outcome(), "failed");
        assert!(init_failed.gaps().unread.is_empty());
        assert!(!init_failed.gaps().incomplete.is_empty());
    }

    /// The JSON of the stop state keeps the two facts apart: what the probe saw
    /// and what the operator said. A single "untrusted + we did write
    /// known_hosts" word would be the failure this shape exists to prevent.
    ///
    /// The field is the **authorization**, because that is the fact this process
    /// holds: the wizard never writes `known_hosts` itself, it passes
    /// `--trust-host` to a child. It used to be called `known_hosts_written` and
    /// set from the declaration alone, which asserted a write on a run where the
    /// child could not be started — a state the loop below covers on purpose.
    #[test]
    fn the_json_reports_the_known_hosts_authorization_it_holds_and_nothing_more() {
        let report = |trust, dest_init| SetupRemoteReport {
            name: Some("d".to_string()),
            kind: Some(SetupRemoteKind::Sftp),
            config: SetupDestinationConfig::Written,
            reach: SetupReach::UntrustedHost {
                host: "h".to_string(),
                port: 23,
            },
            trust,
            dest_init,
            credentials: SetupRemoteCredentials::NoneNamed,
        };
        // What the child did is not what the declaration says. A write that was
        // authorized and then never happened — the child could not be started, or
        // failed first — must not be reported as a write.
        for dest_init in [
            SetupDestinationInit::NotRun {
                why: "dest-init could not be started".to_string(),
            },
            SetupDestinationInit::Ran { exit_code: Some(1) },
            SetupDestinationInit::Ran { exit_code: Some(0) },
        ] {
            let value = setup_remote_json(&report(SetupTrust::Declared, dest_init));
            assert_eq!(
                value["trust"]["known_hosts_write_authorized"], true,
                "the authorization is what was declared, whatever dest-init did: {value}"
            );
            assert!(
                value["trust"].get("known_hosts_written").is_none(),
                "this process never observes the write, so it must have no field claiming it: \
                 {value}"
            );
        }
        // Each trust state reports its own authorization.
        for (trust, authorized) in [
            (SetupTrust::NotRequired, false),
            (SetupTrust::Required, false),
            (SetupTrust::Declined, false),
            (SetupTrust::Declared, true),
        ] {
            let value = setup_remote_json(&report(
                trust,
                SetupDestinationInit::Ran { exit_code: Some(0) },
            ));
            assert_eq!(
                value["trust"]["kind"].is_string(),
                true,
                "the trust state is a tagged object: {value}"
            );
            assert_eq!(
                value["trust"]["known_hosts_write_authorized"], authorized,
                "trust={} must report known_hosts_write_authorized={authorized}: {value}",
                value["trust"]["kind"]
            );
        }
        // A declaration is recorded as unverified, exactly as the masterkey
        // declaration is: nothing about the comparison is checkable here.
        let declared = setup_remote_json(&SetupRemoteReport {
            name: Some("d".to_string()),
            kind: Some(SetupRemoteKind::Sftp),
            config: SetupDestinationConfig::Written,
            reach: SetupReach::UntrustedHost {
                host: "h".to_string(),
                port: 23,
            },
            trust: SetupTrust::Declared,
            dest_init: SetupDestinationInit::Ran { exit_code: Some(0) },
            credentials: SetupRemoteCredentials::NoneNamed,
        });
        assert_eq!(declared["trust"]["declaration_is_verified"], false);
    }

    /// A destination adopted from the config has no kind this run chose, and the
    /// object says so instead of naming one.
    #[test]
    fn an_adopted_destination_does_not_claim_a_kind_the_wizard_did_not_choose() {
        let report = SetupRemoteReport {
            name: Some("hand-written".to_string()),
            kind: None,
            config: SetupDestinationConfig::AlreadyDeclared,
            reach: SetupReach::Reached {
                repository_exists: true,
            },
            trust: SetupTrust::NotRequired,
            dest_init: SetupDestinationInit::Ran { exit_code: Some(0) },
            credentials: SetupRemoteCredentials::NoneNamed,
        };
        let value = setup_remote_json(&report);
        assert_eq!(value["kind"], "reachable");
        assert_eq!(value["config"]["kind"], "already_declared");
        assert_eq!(value["remote_kind"]["kind"], "as_declared");
        assert_eq!(value["recommended_remote_kind"], "s3");
    }

    /// A credential variable that is not in this process is named as itself,
    /// before the connection is attempted. The three states are kept apart:
    /// unset, set-but-empty, and usable — and the value is never decoded.
    ///
    /// The reason sentence is asserted, and so is the JSON that carries the same
    /// observation. A non-TTY caller is shown no stderr at all, so a source that
    /// only existed in the sentence would be an observation half the acceptance
    /// surface never receives — which is what this test found.
    #[test]
    fn an_unusable_credential_variable_is_named_as_unset_or_empty() {
        let name = "CHAT_STASHER_W177_UNUSED_VAR";
        std::env::remove_var(name);
        let args = SetupRemoteArgs {
            remote: Some(SetupRemoteKind::S3),
            remote_access_key_id_env: Some(name.to_string()),
            ..SetupRemoteArgs::default()
        };

        // One report whose credential observation comes from `setup_remote_credentials`,
        // rendered both ways: the sentence and the object.
        let report = |credentials| SetupRemoteReport {
            name: Some("d".to_string()),
            kind: Some(SetupRemoteKind::S3),
            config: SetupDestinationConfig::Written,
            reach: SetupReach::NotAttempted {
                why: "the probe is not what this test is about".to_string(),
            },
            trust: SetupTrust::NotRequired,
            dest_init: SetupDestinationInit::Ran { exit_code: Some(0) },
            credentials,
        };

        let unset = setup_remote_credentials(&args);
        assert_eq!(
            unset.unusable_names(),
            vec![(name, SetupCredentialState::NotSet)],
            "an unset variable is named as unset"
        );
        let value = setup_remote_json(&report(unset));
        assert_eq!(value["credentials"]["kind"], "checked");
        assert_eq!(value["credentials"]["variables"][0]["name"], name);
        assert_eq!(
            value["credentials"]["variables"][0]["state"], "not_set",
            "the JSON has to carry the observation the terminal sentence carries: {value}"
        );

        std::env::set_var(name, "");
        let empty = setup_remote_credentials(&args);
        assert_eq!(
            empty.unusable_names()[0].1.reason(name),
            format!("{name} is set but empty in this process"),
            "empty is a different fix from unset, so it is a different sentence"
        );
        let value = setup_remote_json(&report(empty));
        assert_eq!(
            value["credentials"]["variables"][0]["state"], "empty",
            "and a different state in the object: {value}"
        );

        std::env::set_var(name, "a-fixture-value-that-is-not-printed");
        let usable = setup_remote_credentials(&args);
        assert!(
            usable.unusable_names().is_empty(),
            "a usable variable reports nothing"
        );
        let value = setup_remote_json(&report(usable));
        assert_eq!(
            value["credentials"]["variables"][0]["state"], "set",
            "a usable variable is a measurement, not an omitted field: {value}"
        );
        assert!(
            !format!("{value}").contains("a-fixture-value-that-is-not-printed"),
            "the value is never decoded, so it cannot reach the object: {value}"
        );
        std::env::remove_var(name);

        // A destination that names no credential variable is a third answer, not
        // an empty pass: nothing could be absent.
        let none = setup_remote_json(&report(setup_remote_credentials(
            &SetupRemoteArgs::default(),
        )));
        assert_eq!(none["credentials"]["kind"], "none_named", "{none}");
    }

    #[test]
    fn writer_metadata_records_current_version_and_drift_is_ordered() {
        let dir = tempfile::TempDir::new().unwrap();
        record_writer_version(dir.path(), "machine-a").unwrap();
        let recorded: sidecar::WriterVersionRecord = serde_json::from_slice(
            &fs::read(dir.path().join("meta/machine-a/writer.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(recorded.chat_stasher_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(recorded.machine_id, "machine-a");

        let machines = ["machine-a", "machine-b", "machine-c"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let writers = [
            (
                "machine-a".into(),
                sidecar::WriterVersionRecord {
                    machine_id: "machine-a".into(),
                    chat_stasher_version: "0.2.0".into(),
                },
            ),
            (
                "machine-b".into(),
                sidecar::WriterVersionRecord {
                    machine_id: "machine-b".into(),
                    chat_stasher_version: "0.3.0".into(),
                },
            ),
        ]
        .into_iter()
        .collect();
        let statuses = sidecar::writer_statuses(&machines, &writers, &BTreeSet::new());
        assert_eq!(statuses[0].behind_newest_writer, Some(true));
        assert_eq!(statuses[1].behind_newest_writer, Some(false));
        assert_eq!(statuses[2].behind_newest_writer, Some(true));
        assert!(!statuses[2].version_recorded);
        assert_eq!(
            sidecar::compare_versions("0.4.0", "0.4.0-rc.1"),
            std::cmp::Ordering::Greater
        );
        let unreadable = ["machine-c".to_string()].into_iter().collect();
        let statuses = sidecar::writer_statuses(&machines, &writers, &unreadable);
        assert!(statuses[2].version_unreadable);
        assert_eq!(statuses[2].behind_newest_writer, None);
    }

    #[test]
    fn activity_index_error_redaction_removes_private_paths() {
        let cfg = StoreConfig {
            repo_root: "/home/private/repo".into(),
            key_file: PathBuf::from("/home/private/keys/masterkey.json"),
            ..StoreConfig::default()
        };
        let workspace = PathBuf::from("/home/private/work");
        let message = "/home/private/keys/masterkey.json /home/private/work/session/shard.jsonl";
        let safe = redact_activity_index_paths(message, &cfg, Some(&workspace));
        assert!(!safe.contains("/home/private"), "redacted message: {safe}");
        assert!(safe.contains("<private path>"));
    }

    #[test]
    fn local_stage_rebuild_error_redacts_the_stage_shard_path() {
        let stage = PathBuf::from("/home/private/stage");
        let error = "/home/private/stage/sessions/machine/session/shard.jsonl: permission denied";
        let safe = redact_local_activity_index_message(error, &stage);
        assert!(
            !safe.contains("/home/private"),
            "message exposed a home path"
        );
        assert!(safe.contains("<private path>"));
    }

    #[test]
    fn destination_rebuild_unions_old_sessions_and_preserves_machine_metadata() {
        let dir = tempfile::TempDir::new().unwrap();
        let repo_path = dir.path().join("repo");
        let workspace = dir.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let cfg = StoreConfig {
            repo_root: repo_path.to_string_lossy().into_owned(),
            key_file: dir.path().join("masterkey.json"),
            connections: 1,
            options: BTreeMap::new(),
            cache_dir: None,
            no_cache: false,
        };
        let machine = "fixture-machine";
        let old_session = "claude-code.fixture-machine.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
        let new_session = "claude-code.fixture-machine.019bf00d-97b6-7eb2-9bf8-eacbacc09766";
        let source_stage = dir.path().join("source-stage-old");
        write_shard(
            &source_stage,
            machine,
            old_session,
            &[cc_line("2025-01-15T12:34:56.789Z")],
        );
        fs::create_dir_all(source_stage.join("meta").join(machine)).unwrap();
        fs::write(
            source_stage.join("meta").join(machine).join("machine.json"),
            serde_json::to_vec(&identity::MachineDeclaration {
                machine_id: machine.to_string(),
                display_name: "Fixture Machine".into(),
                os: "test".into(),
                first_seen_unix: 1,
                declared_harnesses: vec!["claude-code".into()],
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            source_stage
                .join("meta")
                .join(machine)
                .join("label-by-writer.json"),
            serde_json::to_vec(&identity::LabelRecord {
                target_machine_id: machine.into(),
                label: "Fixture label".into(),
                written_by_machine_id: "writer".into(),
                written_at_unix: 2,
            })
            .unwrap(),
        )
        .unwrap();
        let mk = MasterKey::new();
        BackupStore::new(cfg.clone(), machine.to_string())
            .push(&source_stage, &mk)
            .unwrap();

        let latest_stage = dir.path().join("source-stage-new");
        write_shard(
            &latest_stage,
            machine,
            new_session,
            &[cc_line("2025-01-15T13:45:07Z")],
        );
        fs::create_dir_all(latest_stage.join("meta").join(machine)).unwrap();
        fs::copy(
            source_stage.join("meta").join(machine).join("machine.json"),
            latest_stage.join("meta").join(machine).join("machine.json"),
        )
        .unwrap();
        BackupStore::new(cfg.clone(), machine.to_string())
            .push(&latest_stage, &mk)
            .unwrap();

        let (sessions, first) =
            rebuild_destination_partition(&workspace, &cfg, machine, &mk).unwrap();
        assert_eq!(sessions, 2);
        assert_eq!(
            first.snapshots_in_repo, 3,
            "the old snapshot remains present"
        );
        let second = rebuild_destination_partition(&workspace, &cfg, machine, &mk).unwrap();
        assert_eq!(second.0, 2);
        assert_eq!(second.1.snapshots_in_repo, 4);

        let selector = chat_stasher::selector::Selector::default().machine(machine);
        let report = chat_stasher::search::search_sessions(
            &BackupStore::for_metadata_query(cfg.clone()),
            &mk,
            &selector,
        )
        .unwrap();
        assert!(report.complete());
        assert_eq!(
            report.hits.len(),
            2,
            "a full rebuild must not duplicate rows"
        );
        assert!(report.hits[0].first_unix.is_some());
        let overview_read = read_overview_indexes(&cfg, &mk).unwrap();
        assert!(overview_read.declared_machines.contains(machine));
        assert_eq!(
            overview_read.declarations[machine].display_name,
            "Fixture Machine"
        );
        assert_eq!(overview_read.labels[machine].len(), 1);
        let versions = read_archive_writer_statuses(&cfg, &mk).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(
            versions[0].chat_stasher_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(versions[0].behind_newest_writer, Some(false));

        let concurrent_session = "claude-code.fixture-machine.019bf00d-97b6-7eb2-9bf8-eacbacc09767";
        let concurrent_stage = dir.path().join("source-stage-concurrent");
        write_shard(
            &concurrent_stage,
            machine,
            concurrent_session,
            &[cc_line("2025-01-15T14:56:18Z")],
        );
        let mut landed = false;
        let (sessions, _) =
            rebuild_destination_partition_with_hook(&workspace, &cfg, machine, &mk, || {
                if !landed {
                    landed = true;
                    BackupStore::new(cfg.clone(), machine.to_string())
                        .push(&concurrent_stage, &mk)
                        .unwrap();
                }
            })
            .unwrap();
        assert!(landed, "the simulated concurrent push must run");
        assert_eq!(sessions, 3);
        let after_race = chat_stasher::search::search_sessions(
            &BackupStore::for_metadata_query(cfg.clone()),
            &mk,
            &selector,
        )
        .unwrap();
        assert!(after_race.complete());
        assert_eq!(after_race.hits.len(), 3);
    }

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
            indeterminate_roots: Vec::new(),
            probes: vec![scanner::HarnessProbe {
                id: "opencode".to_string(),
                display_name: "fixture harness".to_string(),
                root: Some(dir.path().join("store.db")),
                confidence: scanner::Confidence::Confirmed,
                state: scanner::ProbeState::FileTarget,
                record_count: Some(1),
                candidate_count: Some(1),
                unreadable_count: Some(0),
                unreadable_entry_count: Some(0),
                earliest: None,
                latest: None,
                bytes: Some(1),
                recognized_files: Vec::new(),
                note: String::new(),
            }],
        };

        let output = render_archive_gap_notice(&report);
        assert!(
            output.contains("not archivable"),
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
            !output.contains("not archivable"),
            "the marker must disappear once the harness produces a SessionRecord: {output}"
        );
    }

    /// A scan report carrying `n` synthetic sessions and nothing else.
    fn report_with_sessions(n: usize) -> scanner::ScanReport {
        scanner::ScanReport {
            records: (0..n)
                .map(|i| chat_stasher::models::SessionRecord {
                    id: format!("claude-code.fixture.{i:08}"),
                    absolute_path: PathBuf::from(format!("/fixture/{i}.jsonl")),
                    byte_size: 1,
                    mtime: std::time::SystemTime::UNIX_EPOCH,
                    source: chat_stasher::models::HarnessSource::ClaudeCode,
                    compressed: false,
                    sqlite_layout: None,
                })
                .collect(),
            missing_roots: Vec::new(),
            indeterminate_roots: Vec::new(),
            probes: Vec::new(),
        }
    }

    /// `status` is the "is it still working?" entry point for someone who does
    /// not live in a terminal: the verdict must stay on screen. So the default
    /// body is fixed-size — 448 local sessions must not scroll it away.
    #[test]
    fn default_status_body_does_not_grow_with_session_count() {
        let baseline = render_status(&report_with_sessions(0), false)
            .lines()
            .count();
        for n in [1usize, 500] {
            let lines = render_status(&report_with_sessions(n), false)
                .lines()
                .count();
            assert_eq!(
                lines, baseline,
                "default status body must not grow with session count \
                 (0 sessions -> {baseline} lines, {n} sessions -> {lines} lines)"
            );
        }

        // Verdict line + body must still fit on a glance: single digit total.
        let body = render_status(&report_with_sessions(500), false);
        assert!(
            body.lines().count() + 1 < 10,
            "default status must stay under ten lines including the verdict, got:\n{body}"
        );

        // The detail is not deleted, only moved behind the switch.
        let detailed = render_status(&report_with_sessions(500), true);
        assert!(
            detailed.lines().count() > 500,
            "--sessions must still print one line per session, got {} lines",
            detailed.lines().count()
        );
    }

    #[test]
    fn masterkey_does_not_replace_an_unreadable_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("masterkey.json");
        let original = br#"{"truncated":"#;
        fs::write(&key_file, original).unwrap();
        let config = StoreConfig {
            repo_root: dir.path().join("repo").to_string_lossy().into_owned(),
            key_file: key_file.clone(),
            connections: 1,
            options: BTreeMap::new(),
            cache_dir: None,
            no_cache: false,
        };

        let result = masterkey(&config);
        let after = fs::read(&key_file).unwrap();
        eprintln!(
            "B89 A key-file sha256 before={} after={}",
            sha256_hex(original),
            sha256_hex(&after)
        );
        assert!(
            result.is_err(),
            "a present but invalid key file must stop the archive before writing"
        );
        let error = result.err().unwrap().to_string();
        assert!(error.contains("cannot parse masterkey file"));
        assert!(
            error.contains("do not delete"),
            "error must protect the key file: {error}"
        );
        assert_eq!(fs::read(&key_file).unwrap(), original);
    }

    #[test]
    fn masterkey_creates_a_key_when_the_file_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let config = StoreConfig {
            repo_root: dir.path().join("repo").to_string_lossy().into_owned(),
            key_file: dir.path().join("masterkey.json"),
            connections: 1,
            options: BTreeMap::new(),
            cache_dir: None,
            no_cache: false,
        };

        let result = masterkey(&config).unwrap();
        assert!(result.1);
        assert!(config.key_file.is_file());
        assert!(store::load_key_file(&config).is_ok());
    }

    // -----------------------------------------------------------------------
    // ADR-018: three-tier machine resolution — explicit > config.machine > identity file
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_machine_prefers_explicit_over_config() {
        let config = Config {
            machine: Some("cfg-mac".into()),
            ..Config::default()
        };
        let dir = tempfile::TempDir::new().unwrap();
        let out = resolve_machine_at("test", &config, Some("--explicit"), &dir.path().join("id"));
        assert_eq!(out.unwrap(), "--explicit");
    }

    #[test]
    fn resolve_machine_uses_config_machine_without_touching_identity_file() {
        let config = Config {
            machine: Some("cfg-mac".into()),
            ..Config::default()
        };
        let dir = tempfile::TempDir::new().unwrap();
        let id_path = dir.path().join("id");
        let out = resolve_machine_at("test", &config, None, &id_path).unwrap();
        assert_eq!(out, "cfg-mac");
        assert!(
            !id_path.exists(),
            "config machine must not write an identity file"
        );
    }

    #[test]
    fn resolve_machine_uses_loaded_identity() {
        let config = Config::default();
        let dir = tempfile::TempDir::new().unwrap();
        let id_path = dir.path().join("id");
        std::fs::write(&id_path, "0123456789abcdef0123456789abcdef").unwrap();
        let out = resolve_machine_at("test", &config, None, &id_path).unwrap();
        assert_eq!(out, "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn resolve_machine_generates_and_persists_identity_when_missing() {
        let config = Config::default();
        let dir = tempfile::TempDir::new().unwrap();
        let id_path = dir.path().join("id");
        let out = resolve_machine_at("test", &config, None, &id_path).unwrap();
        assert_eq!(out.len(), 32);
        assert!(out.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(
            std::fs::read_to_string(&id_path).unwrap(),
            out,
            "a freshly generated identity must be persisted"
        );
    }

    #[test]
    fn resolve_machine_hard_fails_on_unusable_identity_without_touching_file() {
        let config = Config::default();
        let dir = tempfile::TempDir::new().unwrap();
        let id_path = dir.path().join("id");
        let garbage = "garbage-not-an-identity";
        std::fs::write(&id_path, garbage).unwrap();
        let err = resolve_machine_at("test", &config, None, &id_path).unwrap_err();
        assert_eq!(err, ExitCode::from(3));
        assert_eq!(
            std::fs::read_to_string(&id_path).unwrap(),
            garbage,
            "an unusable identity must never be replaced with a fresh one"
        );
    }

    // -----------------------------------------------------------------------
    // ADR-018: overview display name — never empty, never masquerade partition as name
    // -----------------------------------------------------------------------

    #[test]
    fn display_machine_never_shows_partition_as_name() {
        // A valid 32-hex identity with no declaration and no label must render
        // as short-id + an explicit unnamed marker — never blank, never the raw
        // full partition id pretending to be a name.
        let id = "0123456789abcdef0123456789abcdef";
        let out = display_machine(id, None, &[]);
        assert!(!out.is_empty());
        assert!(out.contains("01234567"), "short id must be present: {out}");
        assert!(
            !out.contains(id),
            "full partition id must not be shown as the name: {out}"
        );
        assert!(
            !out.contains("0123456789abcdef"),
            "full partition id must not be shown: {out}"
        );
    }

    #[test]
    fn display_machine_legacy_partition_is_marked_not_blank() {
        // A legacy hostname partition (not 32 hex) has no identity; it must
        // still be marked unnamed rather than silently presented as a name.
        let out = display_machine("mac", None, &[]);
        assert!(!out.is_empty());
        assert!(
            out.contains("unnamed"),
            "a legacy partition must carry an explicit unnamed marker: {out}"
        );
    }

    #[test]
    fn display_machine_uses_declaration_and_label() {
        let id = "0123456789abcdef0123456789abcdef";
        let decl = identity::MachineDeclaration {
            machine_id: id.into(),
            display_name: "MacBook Air".into(),
            os: "macos".into(),
            first_seen_unix: 0,
            declared_harnesses: vec![],
        };
        let out = display_machine(id, Some(&decl), &[]);
        assert_eq!(out, "MacBook Air (01234567)");

        let label = identity::LabelRecord {
            target_machine_id: id.into(),
            label: "Sold off".into(),
            written_by_machine_id: "writer".into(),
            written_at_unix: 5,
        };
        let out = display_machine(id, Some(&decl), &[label]);
        assert_eq!(
            out, "Sold off (01234567)",
            "a label wins over the declaration"
        );
    }

    // -----------------------------------------------------------------------
    // ADR-018: machine-declare / machine-label writing files
    // -----------------------------------------------------------------------

    #[test]
    fn machine_declare_writes_declaration_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let decl = identity::MachineDeclaration {
            machine_id: "0123456789abcdef0123456789abcdef".into(),
            display_name: "MacBook Air".into(),
            os: "macos".into(),
            first_seen_unix: 123,
            declared_harnesses: vec![],
        };
        let path = write_declaration(&stage, &decl).unwrap();
        assert_eq!(path, identity::machine_decl_path(&stage, &decl.machine_id));
        let back: identity::MachineDeclaration =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, decl);
    }

    #[test]
    fn machine_label_writes_label_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let record = identity::LabelRecord {
            target_machine_id: "fedcba9876543210fedcba9876543210".into(),
            label: "Sold to Alex".into(),
            written_by_machine_id: "0123456789abcdef0123456789abcdef".into(),
            written_at_unix: 42,
        };
        let path = write_label(&stage, &record).unwrap();
        assert_eq!(
            path,
            identity::label_path(
                &stage,
                &record.target_machine_id,
                &record.written_by_machine_id
            )
        );
        let back: identity::LabelRecord =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, record);
    }

    // ------------------------------------------------------- activity index

    /// One synthetic claude-code line with an RFC 3339 timestamp.
    fn cc_line(ts: &str) -> String {
        format!(
            r#"{{"parentUuid":null,"isMeta":null,"sessionId":"s","type":"user","message":{{"role":"user","content":"hi"}},"uuid":"u1","timestamp":"{ts}","cwd":"/x","version":"1.0.31"}}"#
        )
    }

    /// Write one sealed shard for `session` under `machine` (bucketed layout).
    fn write_shard(stage: &Path, machine: &str, session: &str, lines: &[String]) {
        let dir = stage
            .join(store::SESSIONS_DIR)
            .join(machine)
            .join(session)
            .join("000");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("000001.jsonl"), lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn rebuild_activity_index_writes_rows_for_each_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let machine = "mbp-test";
        write_shard(
            &stage,
            machine,
            "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
            &[
                cc_line("2025-01-15T12:34:56.789Z"),
                cc_line("2025-01-15T13:45:07Z"),
            ],
        );
        let outcome = rebuild_activity_index(&stage, machine, false)
            .expect("rebuild should succeed on a valid stage");
        assert_eq!(outcome.sessions_indexed, 1);
        assert_eq!(outcome.sessions_without_harness, 0);
        let index = stage.join("meta").join(machine).join("activity-v1.jsonl");
        let content = fs::read_to_string(&index).unwrap();
        let rows: Vec<&str> = content.lines().collect();
        assert_eq!(rows.len(), 1, "one session -> one row: {content}");
        assert!(
            rows[0].contains(&format!("\"machine\":\"{machine}\"")),
            "row must name the machine: {}",
            rows[0]
        );
        assert!(
            rows[0].contains("claude-code"),
            "row must name the harness: {}",
            rows[0]
        );
        assert!(
            rows[0].contains(r#""first_unix":1736944496"#)
                && rows[0].contains(r#""last_unix":1736948707"#),
            "row must carry the conversation span: {}",
            rows[0]
        );
    }

    #[test]
    fn rebuild_activity_index_rewrites_stale_index_and_advances_mtime() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let machine = "mbp-test";
        write_shard(
            &stage,
            machine,
            "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
            &[cc_line("2025-01-15T12:34:56.789Z")],
        );

        let index = stage.join("meta").join(machine).join("activity-v1.jsonl");
        fs::create_dir_all(index.parent().unwrap()).unwrap();
        fs::write(&index, "stale\n").unwrap();
        let stale_mtime = std::time::SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
        fs::File::options()
            .write(true)
            .open(&index)
            .unwrap()
            .set_modified(stale_mtime)
            .unwrap();

        rebuild_activity_index(&stage, machine, false).expect("rebuild should succeed");
        let content = fs::read_to_string(&index).unwrap();
        assert!(
            !content.contains("stale"),
            "a stale index must be replaced, not kept: {content}"
        );
        assert!(content.contains("claude-code"));
        let new_mtime = fs::metadata(&index).unwrap().modified().unwrap();
        assert!(
            new_mtime > stale_mtime,
            "index mtime must advance on rebuild"
        );
    }

    #[test]
    fn failed_atomic_sidecar_replace_preserves_the_previous_complete_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let index = dir
            .path()
            .join("meta")
            .join("machine-a")
            .join("activity-v1.jsonl");
        fs::create_dir_all(index.parent().unwrap()).unwrap();
        fs::write(&index, "previous-complete-index\n").unwrap();
        let mut temp_name = index.file_name().unwrap().to_os_string();
        temp_name.push(format!(".{}.tmp", std::process::id()));
        fs::write(index.parent().unwrap().join(temp_name), "occupied").unwrap();

        assert!(atomic_replace(&index, b"replacement\n").is_err());
        assert_eq!(
            fs::read_to_string(index).unwrap(),
            "previous-complete-index\n",
            "a failed replacement must leave the last complete index intact"
        );
    }

    #[test]
    fn rebuild_activity_index_emits_unknown_row_without_harness_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let machine = "mbp-test";
        // A directory name with no inferable harness prefix still gets a row.
        fs::create_dir_all(
            stage
                .join(store::SESSIONS_DIR)
                .join(machine)
                .join("~orphan"),
        )
        .unwrap();
        write_shard(
            &stage,
            machine,
            "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
            &[cc_line("2025-01-15T12:34:56.789Z")],
        );
        let outcome =
            rebuild_activity_index(&stage, machine, false).expect("rebuild should succeed");
        assert_eq!(outcome.sessions_indexed, 2);
        assert_eq!(outcome.sessions_without_harness, 1);
        let index = stage.join("meta").join(machine).join("activity-v1.jsonl");
        assert_eq!(
            fs::read_to_string(&index).unwrap().lines().count(),
            2,
            "each session receives exactly one index row"
        );
        let indexed = fs::read_to_string(&index).unwrap();
        assert!(indexed.contains("\"session_id\":\"~orphan\""));
        assert!(indexed.contains("\"kind\":\"unknown\""));
    }

    #[test]
    fn rebuild_activity_index_read_error_when_stage_is_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("no-such-stage");
        match rebuild_activity_index(&stage, "mbp-test", false) {
            Err(ActivityIndexError::Read(message)) => {
                assert!(message.contains("not a directory"), "message: {message}");
            }
            other => panic!("expected a Read error, got {other:?}"),
        }
    }

    #[test]
    fn rebuild_activity_index_write_error_when_meta_is_blocked_by_a_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let machine = "mbp-test";
        write_shard(
            &stage,
            machine,
            "claude-code.mbp-test.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
            &[cc_line("2025-01-15T12:34:56.789Z")],
        );
        // A regular file where `meta/` must become a directory forces the write
        // failure family: reading finished, writing could not.
        fs::write(stage.join("meta"), "blocking file").unwrap();
        match rebuild_activity_index(&stage, machine, false) {
            Err(ActivityIndexError::Write(message)) => {
                assert!(message.contains("cannot create"), "message: {message}");
            }
            other => panic!("expected a Write error, got {other:?}"),
        }
    }

    #[test]
    fn rebuild_activity_index_unreadable_sessions_root_is_a_read_error() {
        // The fix this pins: a dirent whose type cannot be determined must be a
        // hard `ActivityIndexError::Read`, never silently skipped as "not a
        // session". `DirEntry::file_type()` itself cannot be made to fail
        // deterministically in a single-threaded test — on unix it is served
        // from readdir's `d_type` (no syscall) and only falls back to `lstat`
        // on filesystems reporting `DT_UNKNOWN`, which user-space cannot force.
        // So we pin the reachable neighbour, the same read-failure family: a
        // `sessions/<machine>` root that cannot be read at all is an error,
        // never an empty index.
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let machine = "mbp-test";
        fs::create_dir_all(stage.join(store::SESSIONS_DIR)).unwrap();
        // A regular file where the sessions root must be a directory: read_dir
        // fails with NotADirectory (never NotFound), which is the enumeration
        // failure family the fixed code joins.
        let sessions_root = stage.join(store::SESSIONS_DIR).join(machine);
        fs::write(&sessions_root, "not a directory").unwrap();
        match rebuild_activity_index(&stage, machine, false) {
            Err(ActivityIndexError::Read(message)) => {
                assert!(
                    message.contains("cannot read"),
                    "expected the read-failure family, got: {message}"
                );
            }
            other => panic!("expected a Read error, got {other:?}"),
        }
    }
}

/// Load the config, or report why it cannot be used and hand back the exit code
/// that refuses the command.
///
/// Every command that reads the config goes through this, which is what makes
/// "a config that exists and is invalid stops the command" a property of the
/// program rather than of nineteen call sites that each remembered. The exit
/// code is `3`, not `1`: `3` means *did not finish reading*, so an absence proves
/// nothing — here, that the destination list, the harness roots and the machine
/// identity are all **unknown**. `1` would mean "finished and failed", and this
/// command never finished: it never started. `2` would blame the command line,
/// which is not what is wrong.
fn config_or_refuse(command: &str) -> Result<Config, ExitCode> {
    match Config::load() {
        Ok(config) => Ok(config),
        Err(e) => {
            eprintln!("{command}: {e:#}");
            eprintln!(
                "{command}: exit_code=3 — the command did not run, so nothing here is a statement \
                 about this machine's sessions, destinations or archive."
            );
            Err(ExitCode::from(3))
        }
    }
}

fn cmd_init() -> ExitCode {
    match Config::init_default(config::DEFAULT_CONFIG_TEMPLATE) {
        Ok(()) => {
            eprintln!(
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

/// The stage directory the wizard offers when the caller named none.
///
/// Deliberately the same location the bake-off log recorded
/// (`_reconworker/readme-bakeoff/NOTES.md` §6, where `run-once --stage
/// ~/stash/chat-stasher/stage` produced the INIT / NOOP / Healthy / read-back
/// chain this step reproduces), and the same `~/stash/chat-stasher` family the
/// shipped config template documents for `archive_root`, `rustic_repo` and
/// `rustic_key_file`. One layout, stated in two places that agree.
const SETUP_DEFAULT_STAGE: &str = "~/stash/chat-stasher/stage";

/// The sentence a user types to declare they hold their own copy of the
/// masterkey. An exact match, because the point of the question is that the
/// user had to mean it — see [`print_setup_masterkey`].
const SETUP_MASTERKEY_DECLARATION: &str = "I saved it elsewhere";

/// WIZ-2 setup state machine: scan, then the local first save, then the
/// destination and scheduler steps, which are still stubs.
///
/// The local first save is the step that does real work: it runs the pass the
/// timer runs, runs it a second time to observe what a no-change pass does,
/// reads the result back out of the repository, and shows the user the one file
/// they cannot lose. What it never does is report a link of that chain it did
/// not observe — a machine with nothing to archive has a real, short chain, and
/// the wizard says so instead of printing a repository that does not exist.
#[allow(clippy::too_many_arguments)]
fn cmd_setup(
    mut stage: Option<PathBuf>,
    mut destination: Option<String>,
    mut install_schedule: bool,
    masterkey_saved_elsewhere: bool,
    json: bool,
    mut remote: SetupRemoteArgs,
) -> ExitCode {
    let interactive = !json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    // Config and scan are resolved separately rather than chained, because the
    // local first save below has to reuse the *same* config the scan used. A
    // second `Config::load()` could read a different file — it is a
    // hand-edited file on a live machine — and the cadence it reports would
    // then describe a config this run never saw.
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => return setup_could_not_scan(&error, stage.as_ref(), interactive),
    };
    let scan = match scanner::scan(&config) {
        Ok(report) => report,
        Err(error) => return setup_could_not_scan(&error.into(), stage.as_ref(), interactive),
    };

    // Before any work, and before any prompt: a credential flag whose value is
    // not a variable name. This is a usage error detected from the arguments
    // alone, and it is checked first for the same reason clap checks its own —
    // nothing should be written on a command line that cannot be honoured, and
    // the shape being refused is what a pasted secret looks like.
    if let Some((flag, why)) = setup_remote_env_reference_error(&remote) {
        return setup_bad_parameter(flag, &why, interactive);
    }

    if interactive {
        // Keep the scanner's established three-state wording byte-for-byte.
        print!("{}", render_setup_scan(&scan));
        if stage.is_none() {
            let default = setup_default_stage();
            // The default is *shown in the prompt*, never applied silently: the
            // stage is where every future run deposits sealed shards, so the
            // user has to be able to read the path they are agreeing to.
            let prompt = match &default {
                Some(path) => format!("Stage directory [{}]: ", path.display()),
                None => "Stage directory (required): ".to_string(),
            };
            let mut input = String::new();
            match prompt_setup_value(&prompt, &mut input) {
                Ok(()) => match setup_stage_answer(input.trim(), default.as_deref()) {
                    Ok(chosen) => stage = chosen,
                    Err(error) => {
                        eprintln!("setup: stage: {error}");
                        return ExitCode::from(2);
                    }
                },
                Err(error) => {
                    eprintln!("setup: could not read stage choice: {error}");
                    return ExitCode::from(3);
                }
            }
        }
    }

    // Nothing has been written yet, and nothing will be: choosing the directory
    // the user's archive lands in is not a decision this tool makes for them.
    let Some(stage) = stage.as_ref() else {
        let missing = setup_missing_parameters(None);
        if interactive {
            eprintln!("setup: missing required parameter --stage");
        } else {
            println!(
                "{}",
                setup_json_payload(
                    scanner::scan_report_json(&scan),
                    None,
                    install_schedule,
                    None,
                    None,
                    // No stage means no pass ran, so the remote step never got
                    // the chance to either: its three lists are empty and its
                    // object says "not attempted" rather than "nothing there".
                    &SetupRemoteReport {
                        name: destination.clone(),
                        kind: None,
                        config: SetupDestinationConfig::NotWritten {
                            why: "no stage was given, so the wizard stopped before the remote step"
                                .to_string(),
                        },
                        reach: SetupReach::NotAttempted {
                            why: "no stage was given, so the wizard stopped before the remote step"
                                .to_string(),
                        },
                        trust: SetupTrust::NotRequired,
                        dest_init: SetupDestinationInit::NotRun {
                            why: "no stage was given, so the wizard stopped before the remote step"
                                .to_string(),
                        },
                        credentials: SetupRemoteCredentials::NotChecked {
                            why: "no stage was given, so the wizard stopped before the credential \
                                  check"
                                .to_string(),
                        },
                    },
                    &missing,
                    &[],
                    &[],
                    2,
                )
            );
        }
        return ExitCode::from(2);
    };

    // ADR-039 decision 2, step 2 — the local first save happens before the
    // destination question. What is on offer locally is available now; the
    // remote is where the account and credential friction lives, and it is the
    // step a user is allowed to skip.
    let local = setup_local_first_save(&config, stage, interactive);
    let readback = setup_read_back(&config, &local);
    // Built once, from the observations, and then used for the terminal, for
    // the JSON and for the exit decision. Two constructions would be two
    // opinions, and a wizard that says "observed" on stdout while its own JSON
    // says "unknown" is the failure this shape exists to prevent.
    let chain = setup_chain(&local, &readback);

    let mut declared = masterkey_saved_elsewhere;
    if interactive {
        print_setup_local_report(&local, &chain);
        if setup_has_key(&local.save) {
            declared = print_setup_masterkey(&local);
        } else {
            println!(
                "setup: masterkey: none exists yet, so there is nothing to declare saved — the \
                 first pass that archives something creates it"
            );
        }
    }

    // A declaration is owed only when there is a key to declare. With no
    // repository there is no key, and asking for the declaration anyway would
    // ask the user to confirm something that is not true.
    let declaration_missing = setup_has_key(&local.save) && !declared;
    let mut missing = setup_missing_parameters(Some(stage));
    if declaration_missing {
        missing.push("masterkey_saved_elsewhere");
    }
    // Whether the config already declares the named destination, and the three
    // decisions that have to agree about it: whether the interactive wizard asks
    // for a kind and its parameters at all, whether those parameters may be
    // reported missing, and whether the step writes or adopts. An adopted
    // destination needs none of them — it is verified as it stands — so asking
    // for them would be asking for something the run will not use.
    //
    // Recomputed *after* the interactive name prompt below, and that is not
    // tidiness: on the interactive path the name does not exist yet at this
    // point, so a value read here said "not declared" for a destination that was
    // already in the file — and the wizard went on to ask a returning user for a
    // kind and a bucket it was never going to use. A pty run of `setup` on a
    // config that already had the destination is what showed it; every
    // non-interactive test passes the name on the command line, so none of them
    // could.
    let mut destination_declared = setup_destination_declared(destination.as_deref());

    // ADR-039 decision 2, step 3 — the remote question. Asked here, after the
    // local first save, because what is on offer locally is available now while
    // the remote is where the account and credential friction lives — and
    // because skipping it is allowed, which is only a fair offer once the user
    // has seen that one copy already exists.
    if interactive {
        if destination.is_none() {
            let mut input = String::new();
            match prompt_setup_value(
                "Destination name (blank to skip — this machine will hold the only copy): ",
                &mut input,
            ) {
                Ok(()) if !input.trim().is_empty() => {
                    destination = Some(input.trim().to_string());
                }
                Ok(()) => {}
                Err(error) => {
                    eprintln!("setup: could not read destination choice: {error}");
                    return ExitCode::from(3);
                }
            }
        }
        // Now the name is known, whatever route it arrived by.
        destination_declared = setup_destination_declared(destination.as_deref());
        if destination.is_some() && remote.remote.is_none() && !destination_declared {
            match setup_prompt_remote_kind(&remote) {
                Ok(kind) => remote.remote = Some(kind),
                Err(error) => {
                    eprintln!("setup: could not read the remote choice: {error}");
                    return ExitCode::from(3);
                }
            }
        }
        if destination.is_some() && !destination_declared {
            if let Err(error) = setup_prompt_remote_parameters(&mut remote) {
                eprintln!("setup: could not read the remote parameters: {error}");
                return ExitCode::from(3);
            }
        }
    }

    // Checked once, before the step and outside both of its branches, because it
    // is a fact about the invocation rather than about how far the step got: the
    // interactive lines below and the `--json` object are two renderings of this
    // one value, so a non-TTY caller — who is shown nothing — reads the same
    // observation an interactive one gets on stderr.
    let credentials = setup_remote_credentials(&remote);

    let remote_report = match destination.as_deref() {
        Some(name) => {
            if interactive {
                for (variable, state) in credentials.unusable_names() {
                    eprintln!(
                        "setup: remote credentials: {}. The config will still be written, but the \
                         option that names it is omitted at load time and the connection will \
                         fail with a credential error. Export it, and run `setup` again (or \
                         `dest-init`) — the destination block itself does not have to change.",
                        state.reason(variable)
                    );
                }
            }
            // The parameters this step adds to `missing_parameters`, named the
            // same way WIZ-1 names `stage`: only when the step was actually
            // asked for, and only the ones the chosen kind cannot be written
            // without.
            let remote_missing = setup_remote_missing(&remote, destination_declared);
            missing.extend(remote_missing.iter().copied());
            if !remote_missing.is_empty() {
                // A kind was named but its parameters are not all here: nothing
                // is written, and the names above say which flag would supply
                // what is absent.
                SetupRemoteReport {
                    name: Some(name.to_string()),
                    kind: remote.remote,
                    config: SetupDestinationConfig::NotWritten {
                        why: "the remote parameters were incomplete, so nothing was written"
                            .to_string(),
                    },
                    reach: SetupReach::NotAttempted {
                        why: "nothing was written, so there was nothing to probe".to_string(),
                    },
                    trust: SetupTrust::NotRequired,
                    dest_init: SetupDestinationInit::NotRun {
                        why: "the remote parameters were incomplete".to_string(),
                    },
                    credentials,
                }
            } else {
                setup_remote_step(
                    stage,
                    name,
                    &remote,
                    interactive,
                    remote.trust_host,
                    credentials,
                )
            }
        }
        None => SetupRemoteReport {
            name: None,
            kind: None,
            config: SetupDestinationConfig::NotWritten {
                why: "no destination was named".to_string(),
            },
            reach: SetupReach::NotAttempted {
                why: "no destination was named".to_string(),
            },
            trust: SetupTrust::NotRequired,
            dest_init: SetupDestinationInit::NotRun {
                why: "no destination was named".to_string(),
            },
            // Not the value computed above: a credential variable belongs to a
            // destination, and there is none. Saying "checked" here would claim a
            // measurement about a destination this run never had.
            credentials: SetupRemoteCredentials::NotChecked {
                why: "no destination was named, so the remote step never reached the credential \
                      check"
                    .to_string(),
            },
        },
    };

    if interactive {
        print_setup_remote(&remote_report);
    }

    let remote_gaps = remote_report.gaps();
    let mut incomplete = setup_incomplete(&local.save, &chain);
    incomplete.extend(remote_gaps.incomplete.iter().copied());
    let exit_code = setup_exit_code(&missing, &incomplete, &remote_gaps.unread);

    if interactive {
        if !install_schedule {
            let mut input = String::new();
            match prompt_setup_value("Plan scheduler installation? [y/N]: ", &mut input) {
                Ok(()) => {
                    install_schedule = matches!(input.trim(), "y" | "Y" | "yes" | "YES");
                }
                Err(error) => {
                    eprintln!("setup: could not read scheduler choice: {error}");
                    return ExitCode::from(3);
                }
            }
        }
        print_setup_next_steps(install_schedule);
        // The lines below are the human form of `missing_parameters`, the
        // `incomplete` list and the `unread` list. ADR-039 decision 5: an
        // unfinished step is named, never left to be inferred from a missing
        // line.
        if !incomplete.is_empty() {
            eprintln!(
                "setup: INCOMPLETE exit_code=1 — did not finish: {}. Nothing here proves the \
                 local archive is readable.",
                incomplete.join(", ")
            );
        }
        if !remote_gaps.unread.is_empty() {
            eprintln!(
                "setup: UNREAD exit_code=3 — could not read: {}. Nothing about those parts of the \
                 archive is proven, so their absence must not be read as emptiness.",
                remote_gaps.unread.join(", ")
            );
        }
        if declaration_missing {
            // "not made", not "missing": in this codebase "missing" means a file
            // could not be found, and nothing was looked for here — the user
            // simply has not answered yet.
            eprintln!(
                "setup: the masterkey declaration was not made (--masterkey-saved-elsewhere): \
                 this wizard has not been told that you keep a copy of the masterkey elsewhere, \
                 so the archive is still readable only from this machine's copy of it"
            );
        }
        return ExitCode::from(exit_code);
    }

    println!(
        "{}",
        setup_json_payload(
            scanner::scan_report_json(&scan),
            Some(stage),
            install_schedule,
            Some(&local),
            Some(&chain),
            &remote_report,
            &missing,
            &incomplete,
            &remote_gaps.unread,
            exit_code,
        )
    );
    ExitCode::from(exit_code)
}

/// The failure path for "could not look at all".
///
/// 3, not 1: a config that will not load and a scan that will not run mean the
/// wizard never started, so every absence downstream proves nothing. The JSON
/// form is still written in non-TTY runs, so a wrapper gets one object on
/// stdout rather than an empty pipe.
fn setup_could_not_scan(
    error: &anyhow::Error,
    stage: Option<&PathBuf>,
    interactive: bool,
) -> ExitCode {
    if !interactive {
        println!(
            "{}",
            json_string(&serde_json::json!({
                "schema_version": 1,
                "command": "setup",
                "healthy": false,
                "exit_code": 3,
                "scanner": { "kind": "failed", "why": error.to_string() },
                "missing_parameters": setup_missing_parameters(stage),
            }))
        );
    } else {
        eprintln!("setup: scan failed: {error}");
    }
    ExitCode::from(3)
}

/// The exit code for a run whose missing parameters, unfinished steps and
/// unreadable parts are known. One decision, used by the process exit status and
/// by the `exit_code` field of the JSON object, so the two can never disagree.
///
/// **Unread wins.** A part of the run that could not be *read* makes 3 the only
/// honest answer, because 3's meaning is fixed: what was not read proves
/// nothing, so no absence downstream may be believed. A destination that was
/// never reached is that case, and reporting 1 instead would tell a wrapper the
/// wizard finished reading and found the remote empty — which is the one thing
/// it must never be told. It also outranks a missing parameter for the same
/// reason incompleteness does: `3` is a statement about the archive, `2` about
/// the command line.
///
/// Incompleteness then beats a missing parameter: a pass that failed is a
/// stronger statement than a declaration that was not made, and reporting 2
/// (a usage error) for a run that already wrote an archive would understate what
/// happened. 1 — not 3 — for an unfinished step alone: the wizard read
/// everything it set out to read and a step did not finish.
fn setup_exit_code(
    missing: &[&'static str],
    incomplete: &[&'static str],
    unread: &[&'static str],
) -> u8 {
    if !unread.is_empty() {
        return 3;
    }
    if !incomplete.is_empty() {
        return 1;
    }
    if !missing.is_empty() {
        return 2;
    }
    0
}

fn setup_missing_parameters(stage: Option<&PathBuf>) -> Vec<&'static str> {
    if stage.is_some() {
        Vec::new()
    } else {
        vec!["stage"]
    }
}

/// Name every step that did not finish. Pure, so the human lines, the JSON and
/// the exit code are all derived from one list rather than from three
/// independent opinions about what happened.
fn setup_incomplete(save: &SetupLocalSave, chain: &SetupChain) -> Vec<&'static str> {
    let mut incomplete = Vec::new();
    if setup_is_failed(save) {
        incomplete.push("local_first_save");
    }
    // Named by the link, so "the second pass could not be shown to be a no-op"
    // and "the archive could not be read back" stay different statements.
    if matches!(chain.noop, SetupLink::Unknown(_)) {
        incomplete.push("chain_noop");
    }
    if matches!(chain.readback, SetupLink::Unknown(_)) {
        incomplete.push("readback");
    }
    incomplete
}

/// Whether the config already declares this destination.
///
/// `false` for a config that cannot be read is the safe direction *here*: the
/// step itself asks again and reports a failed config rather than writing over
/// something it could not read. The only consequence of this answer being wrong
/// in that direction is that the wizard asks for parameters it will not use,
/// which then land in `missing_parameters` — a refusal, never a write.
fn setup_destination_declared(destination: Option<&str>) -> bool {
    destination.is_some_and(|name| {
        // reason: a config this cannot read is reported as "not declared", and
        // that is the safe direction rather than a convenient one — the only
        // thing this answer changes is whether the wizard *asks* for the
        // parameters of a destination it would otherwise adopt. The step itself
        // asks again, and refuses to write anything at all when the answer is
        // unavailable, so a wrong `false` can cost a refusal and can never cost
        // a write.
        chat_stasher::config::Config::destination_is_declared(name).unwrap_or(false)
    })
}

/// A usage error `setup` can see in its own arguments, before any work.
///
/// Exit 2, and nothing written — the same code and the same promise WIZ-1 gives
/// a named missing parameter. The only shape refused here is a credential flag
/// carrying something that is not a variable name, which is what a pasted secret
/// looks like; the value is named as a *flag* and never echoed.
fn setup_bad_parameter(flag: &str, why: &str, interactive: bool) -> ExitCode {
    if interactive {
        eprintln!("setup: {flag} {why}");
    } else {
        println!(
            "{}",
            json_string(&serde_json::json!({
                "schema_version": 1,
                "command": "setup",
                "healthy": false,
                "exit_code": 2,
                "missing_parameters": [],
                // Separate from `missing_parameters`: the parameter is not
                // absent, it is unusable. A wrapper that cleared the first list
                // and re-ran would still be refused.
                "invalid_parameters": [flag],
                "invalid_parameter_why": why,
            }))
        );
    }
    ExitCode::from(2)
}

/// Ask which remote kind to write, naming the recommendation.
///
/// The candidates are listed with the recommendation marked, per ADR-039
/// decision 2 step 3 ("list the candidates and mark the recommended one"). The
/// mark comes from [`SETUP_RECOMMENDED_REMOTE`], so the sentence and the default
/// cannot disagree; an empty answer takes it, which is the same "show the value
/// in the prompt, never apply it silently" rule the stage prompt follows.
fn setup_prompt_remote_kind(args: &SetupRemoteArgs) -> Result<SetupRemoteKind, String> {
    // `args` is passed for the eventual case of a caller that supplied some
    // remote parameters without a kind; nothing here reads it yet, and taking it
    // as a parameter rather than reading a global keeps this callable from a
    // test.
    let _ = args;
    println!("setup: remote: where the second copy lives.");
    let candidates = setup_remote_candidates();
    for (index, kind) in candidates.iter().enumerate() {
        let mark = if *kind == SETUP_RECOMMENDED_REMOTE {
            " (recommended)"
        } else {
            ""
        };
        println!(
            "setup:   [{}] {:<4} — {}{mark}",
            index + 1,
            kind.slug(),
            kind.blurb()
        );
    }
    // The recommended kind is first in that list, so "press enter" and the
    // "(recommended)" mark are the same fact rather than two that could drift.
    let default = 1;
    let mut input = String::new();
    prompt_setup_value(&format!("Remote kind [{default}]: "), &mut input)
        .map_err(|error| format!("{error}"))?;
    let answer = input.trim();
    if answer.is_empty() {
        return Ok(candidates[0]);
    }
    // The list is 1-based, and only a number that is *in* it is an answer.
    // `saturating_sub(1)` read `0` as the first candidate, so a typo the prompt
    // never offered — there is no `[0]` line above — silently chose the
    // recommended remote instead of failing the way every other bad answer does.
    if let Ok(index) = answer.parse::<usize>() {
        if let Some(kind) = index.checked_sub(1).and_then(|index| candidates.get(index)) {
            return Ok(*kind);
        }
    }
    candidates
        .iter()
        .find(|kind| kind.slug() == answer)
        .copied()
        .ok_or_else(|| {
            format!(
                "`{answer}` is not one of the choices: answer with a number 1-{} or one of {}",
                candidates.len(),
                candidates
                    .iter()
                    .map(|kind| kind.slug())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Every remote kind the wizard can spell, with the recommended one first.
///
/// The order *is* the default: the prompt offers `[1]` and takes it on an empty
/// answer, and the same visit puts the "(recommended)" mark on that line. Written
/// as a sort over the constant rather than as a hand-kept list, so a change to
/// [`SETUP_RECOMMENDED_REMOTE`] moves the mark, the default and the ordering
/// together and cannot leave one of the three behind.
fn setup_remote_candidates() -> Vec<SetupRemoteKind> {
    let mut kinds = vec![SetupRemoteKind::Sftp, SetupRemoteKind::S3];
    kinds.sort_by_key(|kind| *kind != SETUP_RECOMMENDED_REMOTE);
    kinds
}

/// Ask for whatever the chosen kind still needs.
///
/// One field at a time, in the order [`SetupRemoteKind::required_parameters`]
/// names them, so the prompt order and the "you are missing X" order are the
/// same. An optional field is offered as optional and an empty answer leaves it
/// out — never filled with a default the user did not read, because both the
/// root prefix and the ssh key are choices about *their* layout.
fn setup_prompt_remote_parameters(args: &mut SetupRemoteArgs) -> Result<(), String> {
    let Some(kind) = args.remote else {
        return Ok(());
    };
    if args.remote_endpoint.is_none() {
        let example = match kind {
            SetupRemoteKind::Sftp => "ssh://<host>:<port>",
            SetupRemoteKind::S3 => "https://<account-id>.r2.cloudflarestorage.com",
        };
        args.remote_endpoint = Some(setup_ask_required(&format!("Endpoint (e.g. {example}): "))?);
    }
    match kind {
        SetupRemoteKind::Sftp => {
            if args.remote_user.is_none() {
                args.remote_user = Some(setup_ask_required("SSH user: ")?);
            }
            if args.remote_ssh_key.is_none() {
                let answer = setup_ask("Private key path (blank to use the ssh agent): ")?;
                if !answer.is_empty() {
                    args.remote_ssh_key = Some(answer);
                }
            }
        }
        SetupRemoteKind::S3 => {
            if args.remote_bucket.is_none() {
                args.remote_bucket = Some(setup_ask_required("Bucket: ")?);
            }
            // The default is shown in the prompt and an empty answer takes it,
            // exactly as the stage default is handled.
            let region = setup_ask(&format!("Region [{}]: ", args.remote_region))?;
            if !region.is_empty() {
                args.remote_region = region;
            }
            if args.remote_access_key_id_env.is_none() {
                println!(
                    "setup: The access key id is read from an environment variable, never typed \
                     here: a value on a command line is captured by the process list and by any \
                     log that echoes the command."
                );
                args.remote_access_key_id_env =
                    Some(setup_ask_env_reference("Access key id — variable name: ")?);
            }
            if args.remote_secret_key_env.is_none() {
                println!(
                    "setup: Same for the secret access key: name the variable that holds it, and \
                     the config will carry `env:NAME` rather than the secret."
                );
                args.remote_secret_key_env = Some(setup_ask_env_reference(
                    "Secret access key — variable name: ",
                )?);
            }
        }
    }
    if args.remote_root.is_none() {
        let answer = setup_ask("Root prefix inside it (blank for none): ")?;
        if !answer.is_empty() {
            args.remote_root = Some(answer);
        }
    }
    Ok(())
}

/// One line of interactive input, trimmed.
fn setup_ask(prompt: &str) -> Result<String, String> {
    let mut input = String::new();
    prompt_setup_value(prompt, &mut input)
        .map(|()| input.trim().to_string())
        .map_err(|error| format!("{error}"))
}

/// A required answer: an empty one is not accepted, because an empty endpoint
/// or user is not a value the destination can be built from.
fn setup_ask_required(prompt: &str) -> Result<String, String> {
    let answer = setup_ask(prompt)?;
    if answer.is_empty() {
        return Err("an empty answer is not accepted here".to_string());
    }
    Ok(answer)
}

/// A required answer that must additionally be a legal environment-variable
/// name — checked as it is typed, so the refusal happens at the prompt rather
/// than after the rest of the walkthrough.
fn setup_ask_env_reference(prompt: &str) -> Result<String, String> {
    let answer = setup_ask_required(prompt)?;
    if !chat_stasher::config::is_env_reference_name(&answer) {
        return Err(
            "that is not a legal environment variable name (letters, digits and `_`, not starting \
             with a digit). If what you typed was the credential itself, do not re-enter it here: \
             put it in an environment variable and give this wizard the variable's name. Nothing \
             was written."
                .to_string(),
        );
    }
    Ok(answer)
}

/// The stage directory the wizard offers.
///
/// `None` when there is no home directory to build the path from. A caller must
/// then ask rather than offer a default: `config::home_dir()` falls back to a
/// per-process temp directory, and offering *that* as a stage would put the
/// user's first archive somewhere the operating system is about to delete.
fn setup_default_stage() -> Option<PathBuf> {
    chat_stasher::config::expand_and_verify(SETUP_DEFAULT_STAGE).ok()
}

/// Turn one line of interactive stage input into the stage to use.
///
/// An empty answer takes the offered default; anything else goes through the
/// same expansion and verification boundary the config loader uses, so a typed
/// `~/x` becomes a real path instead of a directory literally named `~` — the
/// failure `TildeError` exists to prevent, and one a shell never gets the
/// chance to prevent here, because this text never passes through a shell.
fn setup_stage_answer(
    typed: &str,
    default: Option<&Path>,
) -> Result<Option<PathBuf>, chat_stasher::config::TildeError> {
    // Trims its own input rather than relying on the caller: "an empty answer
    // takes the default" has to hold for a line of spaces too, and a helper
    // whose contract is "the caller already trimmed" is one call site away from
    // creating a stage directory named `   `.
    let typed = typed.trim();
    if typed.is_empty() {
        return Ok(default.map(Path::to_path_buf));
    }
    chat_stasher::config::expand_and_verify(typed).map(Some)
}

/// One `run-once` pass, as this process observed it.
///
/// Every variant is an observation rather than an expectation. `Unrecorded`
/// exists so that a pass which ran but left no record of its own is never
/// reported by reading the previous pass's record a second time — that would
/// turn "we do not know" into a confident duplicate of an older answer.
#[derive(Debug)]
enum SetupPass {
    Recorded(chat_stasher::runstate::RunState),
    Unrecorded { exit_code: i32 },
    Failed { exit_code: i32 },
    CannotRun { why: String },
    Skipped { why: &'static str },
}

/// What the local first save did to the repository.
#[derive(Debug)]
enum SetupLocalSave {
    /// The repository did not exist before the first pass and does after it.
    /// The chain's `INIT` link, observed rather than assumed.
    Created,
    /// It was already there before the first pass — a re-run of `setup` on a
    /// machine that has archived before.
    Existed,
    /// No repository exists and no pass reported a failure: there was nothing
    /// to archive. Not an empty archive and not an error — nothing was created,
    /// and the absence is named with its reason.
    NothingToArchive,
    /// A pass failed, could not be started, or could not be attributed.
    Failed { why: String },
    /// Whether a repository exists could not be established at all.
    Unknown { why: String },
}

#[derive(Debug)]
struct SetupLocalSaveReport {
    save: SetupLocalSave,
    first: SetupPass,
    second: SetupPass,
    key_file: PathBuf,
    /// Snapshots in the repository after the first pass and after the second.
    ///
    /// This pair is what the idempotence link is decided on. It is a fact about
    /// what the passes *did* to the archive, so unlike a run-state record it
    /// cannot be confused by two passes landing in the same second — which is
    /// exactly what a re-run produces.
    snapshots_after_first: Result<usize, String>,
    snapshots_after_second: Result<usize, String>,
}

/// The fields that tell one run-state record from another.
type SetupRunFingerprint = (u64, u64, chat_stasher::runstate::RunOutcome, bool, usize);

fn setup_run_fingerprint(state: &chat_stasher::runstate::RunState) -> SetupRunFingerprint {
    (
        state.finished_at_unix,
        state.duration_ms,
        state.outcome,
        state.snapshot_created,
        state.shards_written,
    )
}

/// The run-state file's modification time, and the record inside it.
///
/// Two different witnesses on purpose. The record alone cannot prove it was
/// written by the pass that just ran: on a re-run both passes are NOOP with the
/// same outcome, and often the same duration inside the same second, so an
/// identical record is a *normal* outcome rather than evidence of staleness. A
/// test found this the hard way — a plain second `setup` run reported the
/// idempotence link as `unknown`. The file's mtime alone is not proof either,
/// because two passes milliseconds apart share a second on a filesystem with
/// coarse timestamps.
///
/// So a pass's record counts as its own when *either* witness moved. This is
/// used only for the `runs` audit field; no chain link rests on it, because a
/// link a timing coincidence can flip is not an observation.
type SetupRunStamp = (Option<std::time::SystemTime>, Option<SetupRunFingerprint>);

fn setup_run_stamp() -> SetupRunStamp {
    let path = chat_stasher::runstate::run_state_path(&chat_stasher::collect::default_state_dir());
    (
        fs::metadata(&path).and_then(|meta| meta.modified()).ok(),
        setup_last_run_state().as_ref().map(setup_run_fingerprint),
    )
}

/// The store the local first save writes to and reads back from: the config's
/// own local default, with no destination named.
///
/// The same resolution `run-once` uses when the config declares no destination
/// — which is what the bake-off log recorded, and what `search --repo <that
/// path>` then read back.
fn setup_local_store(config: &Config) -> StoreConfig {
    store_config_from(config, None, None, None, &[])
}

/// Whether the local repository exists. `Err` is "could not tell", kept
/// distinct from `Ok(false)` — the three states this project keeps apart.
fn setup_local_repository_exists(config: &Config) -> Result<bool, String> {
    BackupStore::for_metadata_query(setup_local_store(config))
        .repository_exists()
        .map_err(|error| format!("{error:#}"))
}

/// The local first save: two `run-once` passes, then what they left behind.
///
/// Two passes, not one, because the chain this step reproduces makes its claim
/// about the *second*: that a run with nothing new to archive creates no
/// snapshot. Running the pass once and asserting that property would be the
/// "a derived artifact can be green and wrong" failure — so the wizard runs the
/// pass the timer runs, and then runs it again and looks.
fn setup_local_first_save(config: &Config, stage: &Path, echo: bool) -> SetupLocalSaveReport {
    let local_store = setup_local_store(config);
    let key_file = local_store.key_file.clone();
    let before = setup_local_repository_exists(config);
    let first = setup_run_pass(&local_store, stage, echo, None);
    let after_first = setup_local_repository_exists(config);
    let snapshots_after_first = setup_snapshot_count(config);
    let second = match &first {
        SetupPass::Recorded(_) | SetupPass::Unrecorded { exit_code: 0 } => {
            setup_run_pass(&local_store, stage, echo, Some(setup_run_stamp()))
        }
        // A pass that did not complete is not worth repeating: the chain
        // already has a hole, and a second pass cannot close it.
        SetupPass::Unrecorded { .. } | SetupPass::Failed { .. } | SetupPass::CannotRun { .. } => {
            SetupPass::Skipped {
                why: "the first pass did not complete",
            }
        }
        SetupPass::Skipped { why } => SetupPass::Skipped { why },
    };
    let snapshots_after_second = setup_snapshot_count(config);
    let save = setup_local_save_state(&before, &after_first, &first, &key_file);
    SetupLocalSaveReport {
        save,
        first,
        second,
        key_file,
        snapshots_after_first,
        snapshots_after_second,
    }
}

/// How many snapshots the local repository holds, or why that could not be
/// counted — counted through the same open-the-repository path `search` uses.
///
/// The count is the idempotence link's evidence: a pass that adds no snapshot
/// took the no-change path. Unlike the pass's own run-state record, this cannot
/// be a leftover from an earlier run, because it is read from the repository
/// after the pass has returned.
fn setup_snapshot_count(config: &Config) -> Result<usize, String> {
    let cfg = setup_local_store(config);
    let mk = store::load_key_file(&cfg).map_err(|error| format!("{error:#}"))?;
    let backends = BackupStore::for_metadata_query(cfg.clone())
        .backends()
        .map_err(|error| format!("{error:#}"))?;
    let repo = Repository::new(&cfg.repository_options(), &backends)
        .and_then(|repo| repo.open(&Credentials::Masterkey(mk)))
        .and_then(|repo| repo.to_indexed())
        .map_err(|error| format!("{error:#}"))?;
    repo.get_all_snapshots()
        .map(|snapshots| snapshots.len())
        .map_err(|error| format!("{error:#}"))
}

/// Decide what the two passes did to the repository, from observations only.
fn setup_local_save_state(
    before: &Result<bool, String>,
    after_first: &Result<bool, String>,
    first: &SetupPass,
    key_file: &Path,
) -> SetupLocalSave {
    match first {
        SetupPass::CannotRun { why } => {
            return SetupLocalSave::Failed { why: why.clone() };
        }
        SetupPass::Failed { exit_code } => {
            return SetupLocalSave::Failed {
                why: format!("the first run-once pass exited {exit_code}"),
            };
        }
        // Deliberately *not* a failure: the pass exited 0, so it did its job.
        // Whether its run-state record could be attributed is a fact about the
        // record, and this state is decided from the repository's existence —
        // which is observed directly. A second pass that failed is likewise the
        // idempotence link's problem, not the repository's: the chain reports it
        // as unknown, and `save` still describes what is on the disk.
        SetupPass::Recorded(_) | SetupPass::Unrecorded { .. } | SetupPass::Skipped { .. } => {}
    }
    let exists_now = match after_first {
        Ok(exists) => *exists,
        Err(why) => return SetupLocalSave::Unknown { why: why.clone() },
    };
    if !exists_now {
        // Nothing was collected, so `push` skipped and never reached the
        // repository step. No repository, therefore no masterkey either.
        return SetupLocalSave::NothingToArchive;
    }
    // A repository whose key file is not where the config says it should be is
    // an archive nobody can open — worse than no repository, because it looks
    // like one. Reported as a failure rather than as `Created`.
    if !key_file.exists() {
        return SetupLocalSave::Failed {
            why: format!(
                "the repository exists but its masterkey is not at {}, so nothing can be read \
                 back from it",
                key_file.display()
            ),
        };
    }
    match before {
        Ok(true) => SetupLocalSave::Existed,
        Ok(false) => SetupLocalSave::Created,
        Err(why) => SetupLocalSave::Unknown { why: why.clone() },
    }
}

/// Run one `run-once` pass as a child of this process.
///
/// A child rather than an in-process `run_once_pass`, for two reasons that are
/// about the contract rather than about convenience:
///
///   * `--json` promises stdout carries exactly one JSON object (`json_out.rs`
///     module docs), while `run_once_pass` narrates its `[collect]`, `[push]`
///     and `result:` lines to *this* process's stdout. Redirecting a child's
///     stdout keeps that promise without threading a quiet flag through the
///     collector and the pusher, where one missed call site would silently
///     corrupt the machine-readable output of a *scheduled* run too.
///   * The wizard then reproduces the chain the bake-off log recorded — the
///     documented CLI chain — instead of a second in-process path that could
///     drift away from the command the timer actually runs.
///
/// `prior` is the fingerprint of the record read before this pass started; the
/// record found afterwards has to differ from it to be accepted as this pass's
/// own.
///
/// `store` is the *local* store, and it is passed explicitly as `--repo` and
/// `--key-file` rather than left implicit. Left implicit, the child resolves the
/// config's single-destination default — and once the config declares **any**
/// destination, `run-once` refuses to guess one and exits 2. A wizard run on a
/// machine that already has a destination in its config then failed at step 2
/// with "the first run-once pass exited 2", before it reached the step that
/// would have used it. A test on the adopt path found that. The values passed
/// are the same ones the implicit resolution produces when the config declares
/// nothing, so the chain WIZ-2 recorded is unchanged.
fn setup_run_pass(
    store: &StoreConfig,
    stage: &Path,
    echo: bool,
    prior: Option<SetupRunStamp>,
) -> SetupPass {
    let binary = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return SetupPass::CannotRun {
                why: format!("cannot resolve the running executable: {error}"),
            };
        }
    };
    let stdout = if echo {
        std::process::Stdio::inherit()
    } else {
        std::process::Stdio::null()
    };
    let status = std::process::Command::new(binary)
        .arg("run-once")
        .arg("--stage")
        .arg(stage)
        .arg("--repo")
        .arg(&store.repo_root)
        .arg("--key-file")
        .arg(&store.key_file)
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        // stderr is inherited in both modes: the collector's diagnostics are
        // for the human reading the terminal, and the JSON contract governs
        // stdout only.
        .status();
    let exit_code = match status {
        Ok(status) => status.code(),
        Err(error) => {
            return SetupPass::CannotRun {
                why: format!("cannot start a run-once pass: {error}"),
            };
        }
    };
    let stamp = setup_run_stamp();
    match (exit_code, setup_last_run_state()) {
        (Some(0), Some(record)) => {
            if prior == Some(stamp) {
                SetupPass::Unrecorded { exit_code: 0 }
            } else {
                SetupPass::Recorded(record)
            }
        }
        (Some(0), None) => SetupPass::Unrecorded { exit_code: 0 },
        // A pass killed by a signal reports no code at all. Kept as its own
        // value rather than folded into a bare "failed", because "exit code
        // unknown" and "exited non-zero" carry different information.
        (None, _) => SetupPass::Failed { exit_code: -1 },
        (Some(code), _) => SetupPass::Failed { exit_code: code },
    }
}

/// One pass as `setup --json` reports it.
///
/// The raw outcomes are carried, not just the chain verdicts: they are the
/// measurement each link is derived from, and a wrapper that wants to know what
/// the passes actually did should not have to go and read `run-state.json`,
/// which by then holds only the *last* of the two.
fn setup_pass_json(pass: &SetupPass) -> serde_json::Value {
    match pass {
        SetupPass::Recorded(state) => serde_json::json!({
            "kind": "recorded",
            "outcome": setup_outcome_word(state),
            "snapshot_created": state.snapshot_created,
            "shards_written": state.shards_written,
            "duration_ms": state.duration_ms,
        }),
        SetupPass::Unrecorded { exit_code } => serde_json::json!({
            "kind": "unrecorded",
            "exit_code": exit_code,
        }),
        SetupPass::Failed { exit_code } => serde_json::json!({
            "kind": "failed",
            "exit_code": exit_code,
        }),
        SetupPass::CannotRun { why } => serde_json::json!({
            "kind": "cannot_run",
            "why": why,
        }),
        SetupPass::Skipped { why } => serde_json::json!({
            "kind": "skipped",
            "why": why,
        }),
    }
}

/// The most recent `run-once` record, when there is one to read.
fn setup_last_run_state() -> Option<chat_stasher::runstate::RunState> {
    match chat_stasher::runstate::load(&chat_stasher::collect::default_state_dir()) {
        chat_stasher::runstate::RunStateRead::Present(state) => Some(state),
        // "Never ran" and "unreadable" are both *not a record*, and neither may
        // be read as one.
        chat_stasher::runstate::RunStateRead::Missing
        | chat_stasher::runstate::RunStateRead::Unreadable(_) => None,
    }
}

/// Whether a masterkey exists to declare anything about.
fn setup_has_key(local: &SetupLocalSave) -> bool {
    matches!(local, SetupLocalSave::Created | SetupLocalSave::Existed)
}

fn setup_is_failed(local: &SetupLocalSave) -> bool {
    matches!(
        local,
        SetupLocalSave::Failed { .. } | SetupLocalSave::Unknown { .. }
    )
}

/// What reading the just-written repository could establish.
#[derive(Debug)]
enum SetupReadback {
    Read {
        sessions: usize,
        snapshots: usize,
    },
    /// No repository exists, so nothing was read. Explicitly *not* a count of
    /// zero: no measurement happened, and reporting one would be the exact
    /// "unknown recorded as empty" failure this project forbids.
    NoRepository,
    /// A repository exists and could not be read in full.
    Unknown {
        why: String,
    },
    /// The local first save did not establish what is on the disk, so no read
    /// was attempted. Counted against the *save* step, not this one: naming the
    /// same failure twice would make one problem look like two.
    NotAttempted {
        why: String,
    },
}

/// Read the archive back through the same store, key and query `search --repo`
/// uses, so "the wizard read it back" and "`search` reads it back" are one
/// claim and not two.
fn setup_read_back(config: &Config, local: &SetupLocalSaveReport) -> SetupReadback {
    match &local.save {
        // No repository was created, so there is nothing to read and no
        // measurement to report.
        SetupLocalSave::NothingToArchive => return SetupReadback::NoRepository,
        // The save itself did not establish what is on the disk, so a read
        // could not interpret whatever it found. Named once, on the step that
        // failed, rather than blamed on the read as well.
        SetupLocalSave::Failed { why } | SetupLocalSave::Unknown { why } => {
            return SetupReadback::NotAttempted {
                why: format!("the local first save did not finish ({why})"),
            };
        }
        SetupLocalSave::Created | SetupLocalSave::Existed => {}
    }
    let cfg = setup_local_store(config);
    let store = BackupStore::for_metadata_query(cfg.clone());
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(error) => {
            return SetupReadback::Unknown {
                why: format!("cannot read the masterkey: {error:#}"),
            };
        }
    };
    // The empty selector is every session, whatever its harness or its time —
    // the same query `search --repo <path>` runs with no filters.
    let selector = chat_stasher::selector::Selector {
        session_id_prefix: None,
        machine: None,
        harnesses: None,
        window: None,
    };
    match chat_stasher::search::search_sessions(&store, &mk, &selector) {
        Ok(report) => {
            if !report.unreadable.is_empty() {
                return SetupReadback::Unknown {
                    why: format!(
                        "{} part(s) of the repository could not be read, so the session count is \
                         unknown, not {}",
                        report.unreadable.len(),
                        report.hits.len()
                    ),
                };
            }
            SetupReadback::Read {
                sessions: report.hits.len(),
                snapshots: report.snapshots_in_repo,
            }
        }
        Err(error) => SetupReadback::Unknown {
            why: format!("cannot read the repository: {error:#}"),
        },
    }
}

/// One link of the chain the local first save reproduces, as observed.
///
/// The chain is built once, from the observations, and then rendered twice —
/// once as prose and once as JSON. Building it twice, once per renderer, is
/// how a wizard ends up claiming `init: observed` in the terminal while its own
/// JSON says `not_observed`; the two answers must come from one decision.
#[derive(Debug)]
enum SetupLink {
    /// The link happened and was seen to happen.
    Observed(String),
    /// The link did not happen, with the reason. A real state, not a failure:
    /// on a machine with nothing to archive the `INIT` link genuinely did not
    /// occur, and saying so is the point.
    NotObserved(String),
    /// The link's subject does not apply here at all — there is no repository,
    /// so there is no read-back to attempt.
    NotApplicable(String),
    /// It could not be determined. Never rendered as one of the above.
    Unknown(String),
    /// A link that carries a measurement.
    Measured {
        sessions: usize,
        snapshots: usize,
        why: String,
    },
}

impl SetupLink {
    /// The machine-readable tag, the same vocabulary `json_out::CountState`
    /// uses for "measured / unknown / does not apply".
    fn kind(&self) -> &'static str {
        match self {
            SetupLink::Observed(_) => "observed",
            SetupLink::NotObserved(_) => "not_observed",
            SetupLink::NotApplicable(_) => "not_applicable",
            SetupLink::Unknown(_) => "unknown",
            SetupLink::Measured { .. } => "known",
        }
    }

    fn json(&self) -> serde_json::Value {
        match self {
            SetupLink::Measured {
                sessions,
                snapshots,
                why,
            } => serde_json::json!({
                "kind": self.kind(),
                "sessions": sessions,
                "snapshots_in_repo": snapshots,
                "why": why,
            }),
            SetupLink::Observed(why)
            | SetupLink::NotObserved(why)
            | SetupLink::NotApplicable(why)
            | SetupLink::Unknown(why) => serde_json::json!({
                "kind": self.kind(),
                "why": why,
            }),
        }
    }

    /// The sentence that follows the link's name in the terminal.
    fn human(&self) -> String {
        match self {
            SetupLink::Measured {
                sessions,
                snapshots,
                ..
            } => format!(
                "{sessions} session(s) read back from the repository ({snapshots} snapshot(s) in \
                 it)"
            ),
            SetupLink::Observed(why)
            | SetupLink::NotObserved(why)
            | SetupLink::NotApplicable(why)
            | SetupLink::Unknown(why) => why.clone(),
        }
    }
}

/// The three links the local first save is supposed to reproduce.
#[derive(Debug)]
struct SetupChain {
    init: SetupLink,
    noop: SetupLink,
    readback: SetupLink,
}

impl SetupChain {
    /// The `chain` object of `setup --json`: one tagged entry per link, so a
    /// wrapper can tell an observed link from one that never happened without
    /// parsing prose.
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "init": self.init.json(),
            "noop": self.noop.json(),
            "readback": self.readback.json(),
        })
    }
}

/// Build the chain from what the two passes and the read-back established —
/// never from what they were expected to establish.
///
/// Every link keys on [`SetupLocalSave`], which is itself derived only from
/// observed state (did the repository exist before, does it exist now, is the
/// key file there). The pass records supply the supporting detail; they are
/// never the reason a link is called observed.
fn setup_chain(local: &SetupLocalSaveReport, readback: &SetupReadback) -> SetupChain {
    let init = match &local.save {
        SetupLocalSave::Created => {
            SetupLink::Observed("the first run-once pass created the repository".to_string())
        }
        SetupLocalSave::Existed => SetupLink::NotObserved(
            "a repository already existed before this run, so this run created none".to_string(),
        ),
        SetupLocalSave::NothingToArchive => SetupLink::NotObserved(
            "nothing was collected, so no repository was created and no masterkey exists"
                .to_string(),
        ),
        SetupLocalSave::Failed { why } | SetupLocalSave::Unknown { why } => {
            SetupLink::Unknown(why.clone())
        }
    };
    // Decided on the repository's own snapshot count, never on the pass's
    // run-state record: the record is legitimately identical to the previous
    // pass's on a re-run, and reading that as "we do not know" made this link
    // report `unknown` on an ordinary second run.
    let noop = match &local.save {
        SetupLocalSave::Created | SetupLocalSave::Existed => setup_noop_link(local),
        SetupLocalSave::Failed { why } | SetupLocalSave::Unknown { why } => {
            SetupLink::Unknown(why.clone())
        }
        SetupLocalSave::NothingToArchive => SetupLink::NotObserved(
            "no repository exists, so there is nothing for a second pass to be idempotent about"
                .to_string(),
        ),
    };
    let readback = match readback {
        SetupReadback::Read {
            sessions,
            snapshots,
        } => SetupLink::Measured {
            sessions: *sessions,
            snapshots: *snapshots,
            why: "read back through the same path `search --repo` takes".to_string(),
        },
        SetupReadback::NoRepository => SetupLink::NotApplicable(
            "no repository exists, so no session could be read back — this is not a count of zero"
                .to_string(),
        ),
        SetupReadback::Unknown { why } | SetupReadback::NotAttempted { why } => {
            SetupLink::Unknown(why.clone())
        }
    };
    SetupChain {
        init,
        noop,
        readback,
    }
}

/// The idempotence link, decided on what the second pass did to the repository.
///
/// The evidence is the snapshot count before and after that pass: a repository
/// holding the same number of snapshots afterwards is a run that archived
/// nothing new. The pass's own run-state record is reported alongside as
/// supporting detail when it can be attributed, and never turns a counted fact
/// into an unknown.
fn setup_noop_link(local: &SetupLocalSaveReport) -> SetupLink {
    let (first, second) = match (&local.snapshots_after_first, &local.snapshots_after_second) {
        (Ok(first), Ok(second)) => (*first, *second),
        (Err(why), _) | (_, Err(why)) => {
            return SetupLink::Unknown(format!(
                "the repository's snapshots could not be counted, so whether the second pass \
                 added one is unknown: {why}"
            ));
        }
    };
    let outcome = match &local.second {
        SetupPass::Recorded(state) => {
            format!(", and it finished {}", setup_outcome_word(state))
        }
        _ => String::new(),
    };
    if second == first {
        SetupLink::Observed(format!(
            "the second run-once pass added no snapshot: the repository still holds {second} \
             snapshot(s){outcome}"
        ))
    } else if second > first {
        SetupLink::NotObserved(format!(
            "the second run-once pass added a snapshot ({first} → {second}), so it did not take \
             the no-change path"
        ))
    } else {
        // Snapshots do not disappear; a count that fell means the second
        // reading is not describing the same repository as the first.
        SetupLink::Unknown(format!(
            "the repository held {first} snapshot(s) after the first pass and {second} after the \
             second — a count that drops is not something a pass can do, so what happened is \
             unknown"
        ))
    }
}

/// Print the local first save: what it wrote, then each link of the chain.
///
/// The link lines and their JSON twins are rendered from the same
/// [`SetupChain`], so the terminal and the machine-readable object cannot
/// disagree about what happened.
fn print_setup_local_report(local: &SetupLocalSaveReport, chain: &SetupChain) {
    match &local.save {
        SetupLocalSave::Created => {
            println!("setup: local save: created — this pass created the repository");
        }
        SetupLocalSave::Existed => {
            println!("setup: local save: already existed — this pass created nothing");
        }
        SetupLocalSave::NothingToArchive => {
            println!(
                "setup: local save: nothing to archive — no repository and no masterkey were \
                 created; the first pass that collects something will create them"
            );
        }
        SetupLocalSave::Failed { why } => {
            eprintln!("setup: local save FAILED: {why}");
        }
        SetupLocalSave::Unknown { why } => {
            eprintln!("setup: local save: whether the repository exists is UNKNOWN: {why}");
        }
    }
    for (label, link) in [
        ("init", &chain.init),
        ("noop", &chain.noop),
        ("read-back", &chain.readback),
    ] {
        let kind = link.kind().replace('_', " ");
        let line = format!("setup: chain {label:<9}: {kind} — {}", link.human());
        // Only the "unknown" links go to stderr, so a reader piping stdout
        // still sees every link that was established and is left in no doubt
        // that the others were not.
        if matches!(link, SetupLink::Unknown(_)) {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
}

fn setup_outcome_word(state: &chat_stasher::runstate::RunState) -> &'static str {
    match state.outcome {
        chat_stasher::runstate::RunOutcome::Completed => "COMPLETED",
        chat_stasher::runstate::RunOutcome::Noop => "NOOP",
        chat_stasher::runstate::RunOutcome::Error => "ERROR",
    }
}
/// Print the masterkey path and ask for the declaration; returns whether the
/// user made it.
///
/// The wording carries a safety duty no check can: this file cannot be
/// recreated, so an archive whose key is lost is an archive nobody reads again.
/// The question is therefore asked as a *declaration* and is labelled one,
/// because the honest thing to tell the user is that this tool cannot tell
/// whether a copy exists — not to imply that answering "yes" proved anything.
fn print_setup_masterkey(local: &SetupLocalSaveReport) -> bool {
    let key_file = &local.key_file;
    println!("setup: masterkey: {}", key_file.display());
    println!(
        "setup: That file is the only thing that can decrypt the archive. If it is lost the \
         archive can never be read again — not by this tool and not by anyone else. Copy it \
         somewhere else now: another disk, a password manager, a backup you already keep."
    );
    println!(
        "setup: To use the archive on another machine, put the copy back at that exact path and \
         name — nothing else is needed to read it there."
    );
    println!(
        "setup: chat-stasher cannot check that a copy exists. The next answer is a declaration, \
         recorded as unverified — it is not a verification."
    );
    let prompt =
        format!("Type `{SETUP_MASTERKEY_DECLARATION}` to declare you have your own copy: ");
    let mut input = String::new();
    match prompt_setup_value(&prompt, &mut input) {
        Ok(()) if setup_declaration_given(&input) => {
            println!("setup: masterkey declaration: declared (unverified)");
            true
        }
        Ok(()) => {
            eprintln!(
                "setup: masterkey declaration: NOT declared — this step is unfinished, and the \
                 archive is still readable only from this machine's copy of that file"
            );
            false
        }
        Err(error) => {
            eprintln!("setup: could not read the masterkey declaration: {error}");
            false
        }
    }
}

/// Whether an answer counts as the declaration.
///
/// An exact match after trimming, and deliberately case-sensitive and not
/// shortenable: the whole point of asking for a sentence is that the user had to
/// read it, and accepting `yes`, `y` or `Y` would turn the one question standing
/// between a user and an unreadable archive back into a keypress.
fn setup_declaration_given(answer: &str) -> bool {
    answer.trim() == SETUP_MASTERKEY_DECLARATION
}

/// The sentence a user types to declare they have compared the host key's
/// fingerprint with the one their provider publishes.
///
/// Same shape, and for the same reason, as [`SETUP_MASTERKEY_DECLARATION`]: the
/// question exists so that a human reads it, so an exact sentence is required
/// and `y` is not. ADR-039 rejected option E ("the wizard automatically agrees
/// to the SSH host fingerprint") outright; this is the non-automatic form, and
/// the wizard never passes `--trust-host` to anything until the sentence or the
/// flag has been given.
const SETUP_HOST_TRUST_DECLARATION: &str =
    "I compared the fingerprint with my provider's published one";

/// What the remote step observed, in the order it happened.
///
/// Every field is an observation. `NotAttempted`/`NotRequired` exist so that
/// "we did not look" and "there was nothing to look at" never print the same
/// word, which is the rule the whole wizard is arranged around.
struct SetupRemoteReport {
    /// The destination's name, or `None` when the caller skipped the step.
    name: Option<String>,
    /// The recipe that was written (or that the adopted block uses).
    kind: Option<SetupRemoteKind>,
    config: SetupDestinationConfig,
    reach: SetupReach,
    trust: SetupTrust,
    dest_init: SetupDestinationInit,
    /// What was observed about the credential variables this destination names.
    ///
    /// Its own field rather than a `missing_parameter`: the parameter *was*
    /// given, and the variable it names is absent. Reported here so that a
    /// non-TTY caller — for whom nothing is printed — gets the same observation
    /// an interactive one reads on stderr.
    credentials: SetupRemoteCredentials,
}

/// How the destination's config block came to exist.
enum SetupDestinationConfig {
    /// The wizard added `[destinations.<name>]`.
    Written,
    /// It was already in the file. Nothing was written: the wizard verifies a
    /// destination it did not write rather than replacing it.
    AlreadyDeclared,
    /// Nothing was written, because the step never got that far.
    NotWritten { why: String },
    /// The write was attempted and refused. The file is untouched.
    Failed { why: String },
}

/// What the read-only probe found at the destination.
enum SetupReach {
    /// The probe did not run, and why.
    NotAttempted { why: String },
    /// The destination answered. `repository_exists` is that answer.
    Reached { repository_exists: bool },
    /// The host presented a key, and it is not in `known_hosts`. This is the
    /// one situation the wizard stops for, because it is the only one where a
    /// human can still answer the question the network cannot.
    UntrustedHost { host: String, port: u16 },
    /// The host's key **changed**. Never offered for recording, at any
    /// confirmation: `remote_err` classifies this separately precisely so it
    /// cannot be downgraded to a first contact.
    HostKeyChanged {
        host: String,
        port: u16,
        why: String,
    },
    /// The destination did not answer, for a reason that is not about trust.
    Unreachable {
        class: Option<&'static str>,
        why: String,
    },
    /// The destination could not be consulted at all, and not because it
    /// refused to answer: the config that was just written could not be read
    /// back, or the tool that has to consult it could not be started. Kept
    /// apart from [`Self::Unreachable`] because the two have different repairs
    /// — one is "go and look at your host", the other is "go and look at your
    /// config" — and apart from [`Self::NotAttempted`], which is a step that
    /// never started and is named by `missing_parameters` instead.
    Unreadable { why: String },
}

/// What happened about the host key.
enum SetupTrust {
    /// No trust question arose: the host was already known, the destination is
    /// local, or the read failed for an unrelated reason.
    NotRequired,
    /// The key is unknown and the wizard has not been told it was checked.
    /// Nothing was written to `known_hosts`.
    Required,
    /// The operator declared they checked it out of band, so `dest-init` was
    /// authorized to record it.
    Declared,
    /// The operator was asked and did not make the declaration.
    Declined,
}

/// What was observed about the credential variables the destination names.
///
/// A third state, named separately, all the way to the object a wrapper reads:
/// "the check ran and every named variable is usable", "the check ran and these
/// are the ones that are not", and "the check never ran" are three answers, and
/// an absent or empty list would collapse the last two into a pass. The values
/// are never decoded — only whether `var_os` found something and whether that
/// something was empty — so there is no path from here to a secret.
enum SetupRemoteCredentials {
    /// The step never reached the check, and why.
    NotChecked { why: String },
    /// The check ran, and this destination names no credential variable at all:
    /// nothing could be absent. Not a pass, because there was nothing to pass.
    NoneNamed,
    /// The check ran; one entry per named variable, in the order the flags named
    /// them.
    Checked(Vec<SetupCredentialCheck>),
}

/// One credential variable, as this process found it.
struct SetupCredentialCheck {
    /// The variable's **name**. Its value is never read, and never carried here.
    name: String,
    state: SetupCredentialState,
}

impl SetupRemoteCredentials {
    /// The names that cannot be used here, each with the fix it needs.
    ///
    /// The terminal lines and the JSON are rendered from this one computation, so
    /// the two cannot disagree about whether a variable is usable — the failure
    /// that the "one observation, two renderings" rule elsewhere in this step
    /// exists to prevent.
    fn unusable_names(&self) -> Vec<(&str, SetupCredentialState)> {
        match self {
            Self::NotChecked { .. } | Self::NoneNamed => Vec::new(),
            Self::Checked(variables) => variables
                .iter()
                .filter(|variable| variable.state != SetupCredentialState::Set)
                .map(|variable| (variable.name.as_str(), variable.state))
                .collect(),
        }
    }
}

/// Whether a named credential variable can be used by this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupCredentialState {
    /// Set in this process and not empty, so the loader will resolve the option.
    Set,
    /// Not set in this process at all.
    NotSet,
    /// Set in this process and empty.
    Empty,
}

impl SetupCredentialState {
    /// The word this state is reported as, on both the terminal and in JSON.
    fn slug(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::NotSet => "not_set",
            Self::Empty => "empty",
        }
    }

    /// Why this state makes the option unusable, in the words the terminal uses.
    fn reason(self, name: &str) -> String {
        match self {
            Self::Set => unreachable!("a usable variable has no reason to report"),
            Self::NotSet => format!("{name} is not set in this process"),
            Self::Empty => format!("{name} is set but empty in this process"),
        }
    }
}

/// The `dest-init` child, when it ran.
enum SetupDestinationInit {
    Ran { exit_code: Option<i32> },
    NotRun { why: String },
}

impl SetupRemoteReport {
    /// The single slug `steps.destination` and `destination.kind` carry.
    ///
    /// One word for the whole step, derived from the observations below rather
    /// than decided alongside them, so the terminal and the object cannot
    /// disagree about how far the step got.
    fn outcome(&self) -> &'static str {
        if self.name.is_none() {
            return "skipped";
        }
        match &self.config {
            SetupDestinationConfig::NotWritten { .. } | SetupDestinationConfig::Failed { .. } => {
                return "not_configured";
            }
            SetupDestinationConfig::Written | SetupDestinationConfig::AlreadyDeclared => {}
        }
        if matches!(self.reach, SetupReach::NotAttempted { .. }) {
            // The step never started, so there is no reach to report. `failed`
            // would overstate it and `unread` would claim a read was tried.
            return "not_attempted";
        }
        match &self.dest_init {
            SetupDestinationInit::Ran { exit_code: Some(0) } => "reachable",
            // 3 keeps its meaning wherever it appears: the destination was not
            // read, so its absence proves nothing.
            SetupDestinationInit::Ran { exit_code: Some(3) } => "unread",
            SetupDestinationInit::Ran { .. } => "failed",
            SetupDestinationInit::NotRun { .. } => "unread",
        }
    }

    /// Whether the step finished, and whether it could be read at all.
    ///
    /// Two lists, never one: "the destination was not read" (exit 3, so any
    /// absence proves nothing) and "the destination was read and the step did
    /// not finish" (exit 1) are the distinction the project's exit codes exist
    /// to carry, and a wizard that collapsed them would be the place it was
    /// lost.
    fn gaps(&self) -> SetupRemoteGaps {
        let mut gaps = SetupRemoteGaps::default();
        if self.name.is_none() {
            // A skipped step is not an unfinished one. ADR-039 decision 2 step 3
            // makes skipping the remote allowed, and the terminal says what it
            // costs; reporting it as incomplete would tell a wrapper the wizard
            // failed, which is not what happened.
            return gaps;
        }
        match &self.config {
            SetupDestinationConfig::Written | SetupDestinationConfig::AlreadyDeclared => {}
            // A block that was never written is not an unfinished step: the
            // parameter that stopped it is named in `missing_parameters`, and
            // the existing WIZ-1 contract puts a never-started step there rather
            // than in `incomplete` (see `setup_json_reports_named_missing_stage_
            // without_prompting`, where a run with no stage reports `missing`
            // and no gap). A *refused* write is different: it was attempted.
            SetupDestinationConfig::Failed { .. } => gaps.incomplete.push("destination_config"),
            SetupDestinationConfig::NotWritten { .. } => {}
        }
        match &self.reach {
            SetupReach::Reached { .. } => {}
            // Never probed: the step never started, and what stopped it is
            // already named. Reporting it here would give one cause two names and
            // turn a usage error into "we could not read the archive".
            SetupReach::NotAttempted { .. } => {}
            SetupReach::Unreadable { .. } => gaps.unread.push("destination"),
            SetupReach::UntrustedHost { .. } => gaps.unread.push("destination_trust"),
            SetupReach::HostKeyChanged { .. } => gaps.unread.push("destination_trust"),
            SetupReach::Unreachable { .. } => gaps.unread.push("destination"),
        }
        match &self.trust {
            SetupTrust::Declined => gaps.unread.push("destination_trust"),
            SetupTrust::NotRequired | SetupTrust::Required | SetupTrust::Declared => {}
        }
        match &self.dest_init {
            SetupDestinationInit::Ran { exit_code: Some(0) } => {}
            // 3 from `dest-init` is the same statement it is everywhere else:
            // the destination was not read. Anything else is a step that ran and
            // failed.
            SetupDestinationInit::Ran { exit_code: Some(3) } => {
                gaps.unread.push("destination_init")
            }
            SetupDestinationInit::Ran { .. } => gaps.incomplete.push("destination_init"),
            // A child that never ran is **not** an unfinished step. Whatever
            // stopped it is the fact, and it is already named by the
            // config/reach/trust rules above — an untrusted host, an
            // unreachable endpoint, a failed write. Listing `destination_init`
            // here too would give one cause two names and a longer list than
            // there are causes: a test written for the stop state found exactly
            // that.
            SetupDestinationInit::NotRun { .. } => {}
        }
        gaps
    }
}

#[derive(Debug, Default)]
struct SetupRemoteGaps {
    incomplete: Vec<&'static str>,
    unread: Vec<&'static str>,
}

/// The parameters this kind cannot be written without, given what was supplied.
///
/// Names match the clap argument ids, so the word a caller is told is missing is
/// the word of the flag that supplies it — the WIZ-1 contract, kept for the
/// parameters this step adds.
///
/// `declared` is whether the config already declares this destination, and it
/// changes the answer completely: **an adopted destination needs no parameters
/// at all**, because nothing will be written. Requiring a kind there would make
/// `setup --destination <a name already in the file>` fail on a command line
/// that is already complete — which is what a test caught, on the path ADR-039
/// keeps open for destinations the wizard does not spell itself.
fn setup_remote_missing(args: &SetupRemoteArgs, declared: bool) -> Vec<&'static str> {
    if declared {
        return Vec::new();
    }
    let Some(kind) = args.remote else {
        return vec!["remote"];
    };
    let mut missing: Vec<&'static str> = Vec::new();
    for name in kind.required_parameters() {
        let supplied = match *name {
            "remote_endpoint" => args.remote_endpoint.is_some(),
            "remote_user" => args.remote_user.is_some(),
            "remote_bucket" => args.remote_bucket.is_some(),
            "remote_access_key_id_env" => args.remote_access_key_id_env.is_some(),
            "remote_secret_key_env" => args.remote_secret_key_env.is_some(),
            // Unreachable while `required_parameters` is the only source of
            // these names; an arm rather than a wildcard so adding a parameter
            // to that list is a compile error here instead of a parameter that
            // is silently never reported missing.
            other => unreachable!("no rule for the required parameter `{other}`"),
        };
        if !supplied {
            missing.push(name);
        }
    }
    missing
}

/// Whether a value has the shape of a credential this project knows about.
///
/// A **second** guard, because the variable-name shape check cannot do this job
/// alone and pretending otherwise would be the dangerous kind of wrong. Measured
/// while writing the test below: an AWS access key id such as
/// `AKIAIOSFODNN7EXAMPLE` is all uppercase letters and digits, so it is a legal
/// environment-variable name by shape — `is_env_reference_name` accepts it, and
/// no amount of shape checking can tell it from a variable someone really named
/// that. The name shape *does* catch the credentials whose alphabet is richer
/// than a variable name's: a secret access key (`/`, `+`, lowercase), any Cloudflare
/// R2 token (lowercase hex), a bearer token (`.` or `-`).
///
/// So the prefix is checked explicitly, as a separate rule with its own reason.
/// `AKIA` and `ASIA` are AWS's two published access-key-id prefixes; nothing
/// plausibly names an environment variable that way.
fn looks_like_a_pasted_access_key_id(value: &str) -> bool {
    value.starts_with("AKIA") || value.starts_with("ASIA")
}

/// Which credential flag carries a value that cannot be used, and why.
///
/// The two credential flags take the **name** of an environment variable. A
/// value that is neither a legal variable name nor an access-key-id prefix is
/// almost always a secret pasted where a name was meant, and it is refused
/// before it can reach the config — by shape, through the same predicate the
/// config loader resolves `env:NAME` with, so the wizard cannot write a
/// reference its own loader would drop.
///
/// **The value is never quoted**, here or in the message: at this point the
/// value is more likely to be the secret than a typo, and a message is the
/// easiest place for one to end up in a log.
fn setup_remote_env_reference_error(args: &SetupRemoteArgs) -> Option<(&'static str, String)> {
    for (flag, value) in [
        ("--remote-access-key-id-env", &args.remote_access_key_id_env),
        ("--remote-secret-key-env", &args.remote_secret_key_env),
    ] {
        let Some(value) = value else { continue };
        let name_shaped = chat_stasher::config::is_env_reference_name(value);
        if !name_shaped {
            return Some((
                flag,
                "takes the NAME of an environment variable, not a credential: it must be \
                 letters, digits and `_`, not starting with a digit. A real secret rarely has \
                 that shape — one with lowercase letters, `/`, `+`, `.` or `-` in it does not. \
                 The value is not echoed, here or anywhere else, because a value of this shape \
                 is more likely to be the secret than a typo. Nothing was read and nothing was \
                 written."
                    .to_string(),
            ));
        }
        if looks_like_a_pasted_access_key_id(value) {
            return Some((
                flag,
                "takes the NAME of an environment variable, not a credential. What was given \
                 has the shape of a cloud access key id, which — unlike a secret access key — is \
                 all uppercase letters and digits and so cannot be told from a variable name by \
                 shape alone; it is refused by its prefix instead. Name the variable that holds \
                 it, for example CHAT_STASHER_R2_ACCESS_KEY_ID. The value is not echoed. Nothing \
                 was read and nothing was written."
                    .to_string(),
            ));
        }
    }
    None
}

/// The environment variables this destination's credentials are read from.
fn setup_remote_credential_names(args: &SetupRemoteArgs) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::new();
    if let Some(name) = args.remote_access_key_id_env.as_deref() {
        names.push(name);
    }
    if let Some(name) = args.remote_secret_key_env.as_deref() {
        names.push(name);
    }
    names
}

/// What this process can say about the credential variables a destination names.
///
/// A third state, named separately: the variable is not set here, or it is set
/// and empty. Both make the config loader omit the option (`config.rs:781`),
/// which leaves the backend to fail with an authentication error much further
/// down. Naming the cause here is the difference between "the destination
/// refused me" and "the variable that names my credential is not exported".
///
/// The variable's *value* is never decoded: an `OsString` is asked whether it is
/// empty and dropped. There is no path from here to a secret.
fn setup_remote_credentials(args: &SetupRemoteArgs) -> SetupRemoteCredentials {
    let names = setup_remote_credential_names(args);
    if names.is_empty() {
        return SetupRemoteCredentials::NoneNamed;
    }
    SetupRemoteCredentials::Checked(
        names
            .into_iter()
            .map(|name| SetupCredentialCheck {
                name: name.to_string(),
                state: match std::env::var_os(name) {
                    None => SetupCredentialState::NotSet,
                    Some(value) if value.is_empty() => SetupCredentialState::Empty,
                    Some(_) => SetupCredentialState::Set,
                },
            })
            .collect(),
    )
}

/// The `[destinations.<name>.options]` table for one kind.
///
/// The recipes are the documented ones: SFTP from `docs/install.md` §4.4, S3 from
/// §4.5. Three choices in them are deliberate:
///
/// * **The two credential values are `env:NAME`**, never a value. That is the
///   only spelling this function can produce, which is what makes "no secret in
///   the config" a property of the code rather than a promise about the caller.
/// * **`disable_config_load` and `disable_ec2_metadata` are both written for
///   S3**, as §4.5 requires. Left out, a missing or mistyped credential can
///   fall through to the ambient AWS environment, to `~/.aws/`, or to the
///   instance metadata service — and the last of those authenticates as the
///   machine's role against whatever address the config names.
/// * **`known_hosts_strategy` is *not* written for SFTP.** Leaving it out is
///   `strict`, and §4.4 states as a property of this project that it "sets none
///   of this for you and does not change the default". Writing the value would
///   pin the same behaviour while making that sentence false, which is a worse
///   trade than relying on the default the docs already rely on.
fn setup_remote_options(kind: SetupRemoteKind, args: &SetupRemoteArgs) -> BTreeMap<String, String> {
    let mut options = BTreeMap::new();
    if let Some(endpoint) = &args.remote_endpoint {
        options.insert("endpoint".to_string(), endpoint.clone());
    }
    if let Some(root) = &args.remote_root {
        options.insert("root".to_string(), root.clone());
    }
    match kind {
        SetupRemoteKind::Sftp => {
            if let Some(user) = &args.remote_user {
                options.insert("user".to_string(), user.clone());
            }
            // Optional on purpose: without it the backend uses the ssh agent and
            // its own default key locations, which is a configuration rather
            // than a gap. An authentication failure is reported by the probe,
            // so leaving it out cannot pass silently.
            if let Some(key) = &args.remote_ssh_key {
                options.insert("key".to_string(), key.clone());
            }
        }
        SetupRemoteKind::S3 => {
            if let Some(bucket) = &args.remote_bucket {
                options.insert("bucket".to_string(), bucket.clone());
            }
            options.insert("region".to_string(), args.remote_region.clone());
            for (option, name) in [
                ("access_key_id", args.remote_access_key_id_env.as_deref()),
                ("secret_access_key", args.remote_secret_key_env.as_deref()),
            ] {
                if let Some(name) = name {
                    options.insert(option.to_string(), format!("env:{name}"));
                }
            }
            options.insert("disable_config_load".to_string(), "true".to_string());
            options.insert("disable_ec2_metadata".to_string(), "true".to_string());
        }
    }
    options
}

/// The remote step: choose, write, probe, settle trust, then run `dest-init`.
///
/// The order is ADR-039's (step 3 then step 4), and the trust rule is the whole
/// point of it: a host nobody has met before **stops the step**. Nothing writes
/// to `known_hosts` until the operator has either declared out of band that the
/// fingerprint is right, or passed `--trust-host` (the same declaration, for a
/// non-TTY caller). A host whose key *changed* is never offered either path.
#[allow(clippy::too_many_arguments)]
fn setup_remote_step(
    stage: &Path,
    name: &str,
    args: &SetupRemoteArgs,
    interactive: bool,
    trust_declared: bool,
    credentials: SetupRemoteCredentials,
) -> SetupRemoteReport {
    let mut report = SetupRemoteReport {
        name: Some(name.to_string()),
        kind: args.remote,
        config: SetupDestinationConfig::NotWritten {
            why: "the destination step did not get as far as writing".to_string(),
        },
        reach: SetupReach::NotAttempted {
            why: "the destination step did not get as far as probing".to_string(),
        },
        trust: SetupTrust::NotRequired,
        dest_init: SetupDestinationInit::NotRun {
            why: "the destination step did not get as far as running dest-init".to_string(),
        },
        // Computed from the arguments before the step started, and carried rather
        // than recomputed: the step's own early returns must still report the
        // same observation, and a second read of the environment could disagree
        // with the first one the terminal already printed.
        credentials,
    };

    // Step 3: the config block. A destination already in the file is adopted
    // as it stands — `rustic init`'s precedent, which ADR-039 cites — and the
    // wizard goes on to verify what is written there rather than its own idea
    // of where the archive should live.
    let declared_before = chat_stasher::config::Config::destination_is_declared(name);
    match &declared_before {
        Ok(true) => report.config = SetupDestinationConfig::AlreadyDeclared,
        Ok(false) => {
            let Some(kind) = args.remote else {
                report.config = SetupDestinationConfig::NotWritten {
                    why: format!(
                        "`{name}` is not declared in the config and no `--remote` kind was given"
                    ),
                };
                return report;
            };
            let options = setup_remote_options(kind, args);
            match chat_stasher::config::Config::set_destination(name, kind.repo(), &options) {
                Ok(chat_stasher::config::DestinationWrite::Added) => {
                    report.config = SetupDestinationConfig::Written;
                }
                Ok(chat_stasher::config::DestinationWrite::AlreadyDeclared) => {
                    report.config = SetupDestinationConfig::AlreadyDeclared;
                }
                Err(error) => {
                    report.config = SetupDestinationConfig::Failed {
                        why: format!("{error:#}"),
                    };
                    return report;
                }
            }
        }
        Err(error) => {
            report.config = SetupDestinationConfig::Failed {
                why: format!("could not tell whether `{name}` is already declared: {error:#}"),
            };
            return report;
        }
    }

    // The config is re-read rather than reused, because the block that will be
    // used from here on is the one on disk: this is what proves the bytes just
    // written are the bytes the tool will read, and it is where an `env:NAME`
    // reference is resolved or dropped.
    let reloaded = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            // `Unreadable`, not `NotAttempted`: the step *did* start — the block
            // was written — and what failed is reading it back. Calling that
            // "never attempted" would report exit 0 for a run whose destination
            // cannot be consulted at all.
            report.reach = SetupReach::Unreadable {
                why: format!("the config could not be re-read after writing: {error:#}"),
            };
            report.dest_init = SetupDestinationInit::NotRun {
                why: "the config could not be re-read, so there is no destination to initialise"
                    .to_string(),
            };
            return report;
        }
    };
    if !reloaded.destinations.contains_key(name) {
        report.reach = SetupReach::Unreadable {
            why: format!("`{name}` is still not declared after the write"),
        };
        report.dest_init = SetupDestinationInit::NotRun {
            why: format!("`{name}` is not declared, so there is nothing to initialise"),
        };
        return report;
    }
    // Resolved, not exited on. A block that was declared by hand and carries no
    // `repo` (the shape ADR-039 keeps open for the local-path and REST
    // candidates, and for anyone who has not filled the block in yet) is a
    // config-content problem this run has *read and observed*: ending the
    // process here would make a `--json` caller's stdout an empty pipe and call
    // it a usage error. It is reported through the same state machine as every
    // other reach outcome instead — the destination was never consulted, so
    // nothing about it is proven, and that is what `Unreadable` says.
    let target = match resolve_store_config_checked(&reloaded, Some(name), None, None, None, &[]) {
        Ok(target) => target,
        Err(problem) => {
            report.reach = SetupReach::Unreadable {
                why: format!(
                    "`{name}` could not be resolved to a repository, so it was never consulted: {}",
                    problem.message
                ),
            };
            report.dest_init = SetupDestinationInit::NotRun {
                why: format!(
                    "`{name}` could not be resolved to a repository, so there is nothing to \
                     initialise"
                ),
            };
            return report;
        }
    };

    // Step 4: the read-only probe, and the trust decision it feeds. One probe,
    // classified once, so the advice printed and the decision taken cannot
    // disagree about which situation this is.
    let (reach, trust) = match chat_stasher::remote_err::preflight(&target, "setup") {
        chat_stasher::remote_err::Preflight::Reached { repository_exists } => (
            SetupReach::Reached { repository_exists },
            SetupTrust::NotRequired,
        ),
        chat_stasher::remote_err::Preflight::Unreachable { kind } => {
            let host_port = chat_stasher::remote_err::remote_endpoint_host_port(&target);
            match (kind, host_port) {
                (
                    Some(chat_stasher::remote_err::RemoteErrorKind::HostUntrusted),
                    Some((host, port)),
                ) => {
                    // The one stop, and the only place a declaration is asked
                    // for. Three answers, kept apart: the operator declared it
                    // (here or via `--trust-host`, the same declaration in
                    // non-TTY form), the operator was asked and declined, or
                    // there was nobody to ask. Nothing is written to
                    // `known_hosts` on either of the last two.
                    let decided = if trust_declared {
                        SetupTrust::Declared
                    } else if interactive {
                        if print_setup_host_trust_prompt(&host, port) {
                            SetupTrust::Declared
                        } else {
                            SetupTrust::Declined
                        }
                    } else {
                        SetupTrust::Required
                    };
                    (SetupReach::UntrustedHost { host, port }, decided)
                }
                (
                    Some(chat_stasher::remote_err::RemoteErrorKind::HostKeyChanged),
                    Some((host, port)),
                ) => (
                    SetupReach::HostKeyChanged {
                        host,
                        port,
                        why: "the host key differs from the one already recorded, which can mean \
                              someone is impersonating the destination. This wizard never offers \
                              to record it."
                            .to_string(),
                    },
                    // Deliberately `NotRequired` and not a trust question: there
                    // is no decision here to offer. The reach value carries the
                    // refusal.
                    SetupTrust::NotRequired,
                ),
                (kind, host_port) => (
                    SetupReach::Unreachable {
                        class: kind.map(|kind| kind.slug()),
                        why: match host_port {
                            Some((host, port)) => format!(
                                "`{host}:{port}` did not answer; the reason is printed above"
                            ),
                            None => "the destination did not answer; the reason is printed above"
                                .to_string(),
                        },
                    },
                    SetupTrust::NotRequired,
                ),
            }
        }
    };

    // `dest-init` runs exactly when it has something to do: a destination the
    // probe reached (which it seeds or updates), or an unknown host whose key
    // the operator has declared checked — which is precisely what `--trust-host`
    // is for, and the only path on which anything writes `known_hosts`. In every
    // other case the probe has already reported the failure, and running
    // `dest-init` would only repeat it.
    let authorize_trust = matches!(trust, SetupTrust::Declared);
    let should_run = match &reach {
        SetupReach::Reached { .. } => true,
        SetupReach::UntrustedHost { .. } => authorize_trust,
        SetupReach::HostKeyChanged { .. }
        | SetupReach::Unreachable { .. }
        | SetupReach::Unreadable { .. }
        | SetupReach::NotAttempted { .. } => false,
    };
    report.reach = reach;
    report.trust = trust;
    if !should_run {
        report.dest_init = SetupDestinationInit::NotRun {
            why: match (&report.reach, &report.trust) {
                (SetupReach::UntrustedHost { .. }, SetupTrust::Declined) => {
                    "the host key was not declared checked, so nothing was connected and nothing \
                     was written to known_hosts"
                        .to_string()
                }
                (SetupReach::UntrustedHost { .. }, _) => {
                    "the host key is not in known_hosts and --trust-host was not given, so \
                     nothing was connected and nothing was written to known_hosts"
                        .to_string()
                }
                (SetupReach::HostKeyChanged { host, port, .. }, _) => format!(
                    "`{host}:{port}` presented a different key from the recorded one, so the \
                     connection was not attempted"
                ),
                (SetupReach::NotAttempted { why }, _) => {
                    format!("the destination was never probed: {why}")
                }
                _ => "the read-only probe did not reach the destination, so dest-init was not run"
                    .to_string(),
            },
        };
        return report;
    }

    // The child, not an in-process call: the same reasoning as
    // `setup_run_pass` running `run-once` as a child. `--trust-host` is passed
    // only when the declaration was made, so that flag stays the only writer of
    // `known_hosts` — this wizard never writes it itself.
    //
    // `reach` is *not* re-derived from the child's exit code. The probe's
    // observation and the child's exit code are two different facts, and a
    // wizard that turned "exit 0" into a repository count of its own would be
    // reporting a measurement it never took.
    report.dest_init = setup_dest_init(name, stage, authorize_trust, interactive);
    report
}

/// Run `dest-init` for one destination, as a child of this process.
///
/// `--trust-host` is passed **only** when the operator declared the host key
/// checked; that flag is then the only writer of `known_hosts` in the whole
/// exchange. stdout is inherited for an interactive run and discarded otherwise,
/// because `--json` promises stdout carries exactly one object; stderr is
/// inherited in both modes, since the JSON contract governs stdout only.
fn setup_dest_init(name: &str, stage: &Path, trust_host: bool, echo: bool) -> SetupDestinationInit {
    let binary = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return SetupDestinationInit::NotRun {
                why: format!("cannot resolve the running executable: {error}"),
            };
        }
    };
    let mut command = std::process::Command::new(binary);
    command
        .arg("dest-init")
        .arg("--destination")
        .arg(name)
        .arg("--stage")
        .arg(stage)
        .stdin(std::process::Stdio::null())
        .stdout(if echo {
            std::process::Stdio::inherit()
        } else {
            std::process::Stdio::null()
        });
    if trust_host {
        command.arg("--trust-host");
    }
    match command.status() {
        Ok(status) => SetupDestinationInit::Ran {
            exit_code: status.code(),
        },
        Err(error) => SetupDestinationInit::NotRun {
            why: format!("cannot start dest-init: {error}"),
        },
    }
}

/// Ask the operator to declare the host key checked out of band.
///
/// Printed *after* the pre-flight has already shown the fingerprints
/// `ssh-keyscan` returned, so the question follows the evidence. Returns whether
/// the declaration was made; a failure to read the answer is a "no", because a
/// question nobody answered must not authorize a write.
fn print_setup_host_trust_prompt(host: &str, port: u16) -> bool {
    println!(
        "setup: remote trust: `{host}:{port}` presented a key that is not in \
         ~/.ssh/known_hosts."
    );
    println!(
        "setup: Nothing has been written. A host key is checked by comparing the fingerprint \
         above with the one your provider publishes — in their own documentation, not over this \
         connection. If the two do not match, stop: do not continue and do not re-run with \
         --trust-host."
    );
    let mut input = String::new();
    match prompt_setup_value(
        &format!("Type `{SETUP_HOST_TRUST_DECLARATION}` to declare you checked it: "),
        &mut input,
    ) {
        Ok(()) if input.trim() == SETUP_HOST_TRUST_DECLARATION => {
            println!(
                "setup: remote trust: declared (unverified) — dest-init will record \
                 `{host}:{port}` in ~/.ssh/known_hosts and then connect"
            );
            true
        }
        Ok(()) => {
            eprintln!(
                "setup: remote trust: NOT declared — nothing was written to ~/.ssh/known_hosts, \
                 and the destination was not connected to"
            );
            false
        }
        Err(error) => {
            eprintln!("setup: could not read the host trust declaration: {error}");
            false
        }
    }
}

/// Print the remote step: the config decision, the reach, the trust, the child.
fn print_setup_remote(report: &SetupRemoteReport) {
    let Some(name) = report.name.as_deref() else {
        println!("setup: remote      : skipped — no destination was named");
        println!(
            "setup: The archive exists on this machine. If this machine is lost, the archive is \
             lost with it: there is no second copy anywhere, and the masterkey that decrypts it \
             is on the same disk. A second disk inside this machine does not change that. Re-run \
             `chat-stasher setup --destination <name> --remote {recommended}` to keep a copy that \
             outlives it.",
            recommended = SETUP_RECOMMENDED_REMOTE.slug()
        );
        return;
    };
    let kind = report.kind.map_or("(as declared)".to_string(), |kind| {
        format!("{} ({})", kind.slug(), kind.repo())
    });
    println!("setup: remote name : {name} — {kind}");

    let config = match &report.config {
        SetupDestinationConfig::Written => {
            format!(
                "written to {}",
                chat_stasher::config::config_path().display()
            )
        }
        SetupDestinationConfig::AlreadyDeclared => {
            "already declared in the config — not rewritten, and verified as it stands".to_string()
        }
        SetupDestinationConfig::NotWritten { why } => format!("not written — {why}"),
        SetupDestinationConfig::Failed { why } => format!("FAILED, nothing written — {why}"),
    };
    if matches!(
        report.config,
        SetupDestinationConfig::Failed { .. } | SetupDestinationConfig::NotWritten { .. }
    ) {
        eprintln!("setup: remote config: {config}");
    } else {
        println!("setup: remote config: {config}");
    }

    let reach = match &report.reach {
        SetupReach::Reached { repository_exists } => format!(
            "reached ({})",
            if *repository_exists {
                "a repository is already there"
            } else {
                "no repository there yet — this run creates it"
            }
        ),
        SetupReach::UntrustedHost { host, port } => {
            format!("STOPPED — `{host}:{port}` presented a key that is not in known_hosts")
        }
        SetupReach::HostKeyChanged { host, port, .. } => format!(
            "STOPPED — `{host}:{port}` presented a DIFFERENT key from the one recorded; this is \
             never accepted here"
        ),
        SetupReach::Unreachable { class, .. } => format!(
            "UNREACHABLE ({}) — so whether a repository is there is unknown, not empty",
            class.unwrap_or("unclassified")
        ),
        SetupReach::Unreadable { why } => {
            format!("UNREADABLE — the destination could not be consulted at all: {why}")
        }
        SetupReach::NotAttempted { why } => format!("not probed — {why}"),
    };
    if matches!(
        report.reach,
        SetupReach::Reached { .. } | SetupReach::NotAttempted { .. }
    ) {
        println!("setup: remote reach : {reach}");
    } else {
        eprintln!("setup: remote reach : {reach}");
    }

    let trust = match &report.trust {
        SetupTrust::NotRequired => "not required".to_string(),
        SetupTrust::Required => {
            "REQUIRED — the key was not declared checked, so nothing was written to \
             ~/.ssh/known_hosts"
                .to_string()
        }
        SetupTrust::Declared => {
            "declared checked (unverified) — dest-init was authorized to record it".to_string()
        }
        SetupTrust::Declined => {
            "NOT declared — nothing was written to ~/.ssh/known_hosts".to_string()
        }
    };
    if matches!(report.trust, SetupTrust::Required | SetupTrust::Declined) {
        eprintln!("setup: remote trust : {trust}");
    } else {
        println!("setup: remote trust : {trust}");
    }

    match &report.dest_init {
        SetupDestinationInit::Ran { exit_code } => {
            let word = match exit_code {
                Some(0) => "exit 0 — the destination now holds the union of the local archive \
                            and your other destinations"
                    .to_string(),
                Some(code) => format!("exit {code} — the destination was NOT initialised"),
                None => "killed by a signal — the destination was NOT initialised".to_string(),
            };
            if *exit_code == Some(0) {
                println!("setup: dest-init   : {word}");
            } else {
                eprintln!("setup: dest-init   : {word}");
            }
        }
        SetupDestinationInit::NotRun { why } => {
            eprintln!("setup: dest-init   : not run — {why}");
        }
    }
}

/// The scheduler (WIZ-4) step, still a stub: it prints what it would do and
/// writes nothing. "planned (not installed)" is the point — a rendered template
/// is not an installed timer, and saying it was installed would be the one lie
/// this project has paid for before.
fn print_setup_next_steps(install_schedule: bool) {
    if install_schedule {
        println!("setup: scheduler installation planned (not installed)");
    } else {
        println!("setup: scheduler installation skipped");
    }
}

/// The `destination` object of `setup --json`.
///
/// Built per state rather than as one object with nulls: a field that is absent
/// because nothing was measured must not appear, or a reader takes `false` or
/// `0` for an answer it never got. The three-state rule reaches all the way to
/// what a wrapper sees, which is the point of the whole wizard.
fn setup_remote_json(report: &SetupRemoteReport) -> serde_json::Value {
    let mut value = serde_json::json!({
        "kind": report.outcome(),
        "recommended_remote_kind": SETUP_RECOMMENDED_REMOTE.slug(),
    });
    let object = value.as_object_mut().expect("a json object");
    match report.name.as_deref() {
        Some(name) => {
            object.insert("name".to_string(), serde_json::json!(name));
            object.insert(
                "remote_kind".to_string(),
                match report.kind {
                    Some(kind) => serde_json::json!(kind.slug()),
                    // The destination was adopted from the config, so its kind
                    // is whatever the file says — this wizard did not choose it
                    // and must not claim one.
                    None => serde_json::json!({
                        "kind": "as_declared",
                        "why": "the destination was already declared in the config, so this run \
                                did not choose its kind",
                    }),
                },
            );
        }
        None => {
            object.insert(
                "consequence".to_string(),
                serde_json::json!(
                    "the archive exists on this machine only: there is no second copy, and the \
                     masterkey that decrypts it is on the same disk, so losing this machine \
                     loses the archive"
                ),
            );
        }
    }
    object.insert(
        "config".to_string(),
        match &report.config {
            SetupDestinationConfig::Written => serde_json::json!({"kind": "written"}),
            SetupDestinationConfig::AlreadyDeclared => serde_json::json!({"kind":
                "already_declared"}),
            SetupDestinationConfig::NotWritten { why } => {
                serde_json::json!({"kind": "not_written", "why": why})
            }
            SetupDestinationConfig::Failed { why } => {
                serde_json::json!({"kind": "failed", "why": why})
            }
        },
    );
    object.insert(
        "reach".to_string(),
        match &report.reach {
            SetupReach::NotAttempted { why } => {
                serde_json::json!({"kind": "not_attempted", "why": why})
            }
            SetupReach::Reached { repository_exists } => serde_json::json!({
                "kind": "reached",
                "repository_exists": repository_exists,
                "why": "a read-only probe of the destination answered",
            }),
            SetupReach::UntrustedHost { host, port } => serde_json::json!({
                "kind": "untrusted_host",
                "host": host,
                "port": port,
                "why": "the host presented a key that is not in known_hosts, so whether a \
                        repository is there is unknown, not empty",
            }),
            SetupReach::HostKeyChanged { host, port, why } => serde_json::json!({
                "kind": "host_key_changed",
                "host": host,
                "port": port,
                "why": why,
            }),
            SetupReach::Unreachable { class, why } => serde_json::json!({
                "kind": "unreachable",
                "class": class,
                "why": why,
            }),
            SetupReach::Unreadable { why } => serde_json::json!({
                "kind": "unreadable",
                "why": why,
            }),
        },
    );
    // `trust` carries the *authorization*, and it says so in the field name. The
    // wizard never writes `known_hosts` itself — it passes `--trust-host` to a
    // child — so what this process observed is its own decision to authorize the
    // write, not the write. A field called `known_hosts_written` said the second
    // thing on the strength of the first: when the child could not be started, or
    // exited non-zero before it got there, the object asserted a write that never
    // happened. Two facts, one of them observed here and one of them not, and
    // only the observed one is reported.
    object.insert(
        "trust".to_string(),
        match &report.trust {
            SetupTrust::NotRequired => serde_json::json!({
                "kind": "not_required",
                // Recorded so a reader cannot mistake the absence of a trust
                // question for an answer to one.
                "known_hosts_write_authorized": false,
            }),
            SetupTrust::Required => serde_json::json!({
                "kind": "required",
                "known_hosts_write_authorized": false,
                "why": "--trust-host was not given, so dest-init was not authorized to write \
                        known_hosts",
            }),
            SetupTrust::Declared => serde_json::json!({
                "kind": "declared",
                "known_hosts_write_authorized": true,
                "declaration_is_verified": false,
                "why": "dest-init was authorized to record the host key after the operator \
                        declared, out of band, that the fingerprint matched — a declaration this \
                        or any other program cannot check",
            }),
            SetupTrust::Declined => serde_json::json!({
                "kind": "declined",
                "known_hosts_write_authorized": false,
                "why": "the operator was asked whether the fingerprint had been checked and did \
                        not declare it, so dest-init was not authorized to write known_hosts",
            }),
        },
    );
    object.insert(
        "credentials".to_string(),
        match &report.credentials {
            SetupRemoteCredentials::NotChecked { why } => {
                serde_json::json!({"kind": "not_checked", "why": why})
            }
            SetupRemoteCredentials::NoneNamed => serde_json::json!({
                "kind": "none_named",
                "why": "this destination names no credential variable, so there is nothing that \
                        could be absent",
            }),
            SetupRemoteCredentials::Checked(variables) => serde_json::json!({
                "kind": "checked",
                "variables": variables
                    .iter()
                    .map(|variable| serde_json::json!({
                        "name": variable.name,
                        "state": variable.state.slug(),
                    }))
                    .collect::<Vec<_>>(),
                "why": "asked of this process with `var_os` before the connection was attempted: \
                        the value is never decoded. An unset or empty variable makes the config \
                        loader omit the option, so the failure would otherwise arrive much later \
                        as an authentication error.",
            }),
        },
    );
    object.insert(
        "dest_init".to_string(),
        match &report.dest_init {
            SetupDestinationInit::Ran { exit_code } => serde_json::json!({
                "kind": "ran",
                "exit_code": exit_code,
            }),
            SetupDestinationInit::NotRun { why } => {
                serde_json::json!({"kind": "not_run", "why": why})
            }
        },
    );
    value
}

/// The `setup --json` object. `exit_code` is passed in rather than recomputed
/// here, so the number in the object and the process exit status are the same
/// decision.
#[allow(clippy::too_many_arguments)]
fn setup_json_payload(
    scan: serde_json::Value,
    stage: Option<&PathBuf>,
    install_schedule: bool,
    local: Option<&SetupLocalSaveReport>,
    chain: Option<&SetupChain>,
    remote: &SetupRemoteReport,
    missing: &[&'static str],
    incomplete: &[&'static str],
    unread: &[&'static str],
    exit_code: u8,
) -> String {
    let save = local.map(|local| &local.save);
    let declared = !missing.contains(&"masterkey_saved_elsewhere");
    let chain = match chain {
        Some(chain) => chain.to_json(),
        // No stage means no pass ran. An explicit tagged object, never `null`:
        // a null here would read as "the chain was empty", and the chain was
        // not produced at all.
        None => serde_json::json!({
            "kind": "not_attempted",
            "why": "no stage was given, so no run-once pass was attempted and no chain exists",
        }),
    };
    let runs = match local {
        Some(local) => serde_json::json!({
            "first": setup_pass_json(&local.first),
            "second": setup_pass_json(&local.second),
        }),
        None => serde_json::json!({
            "kind": "not_attempted",
            "why": "no stage was given, so no run-once pass was attempted",
        }),
    };
    let value = serde_json::json!({
        "schema_version": 1,
        "command": "setup",
        "healthy": exit_code == 0,
        "exit_code": exit_code,
        "scanner": scan,
        "missing_parameters": missing,
        "steps": {
            "stage": if stage.is_some() { "provided" } else { "missing" },
            "local_save": match save {
                Some(SetupLocalSave::Created) => "created",
                Some(SetupLocalSave::Existed) => "existed",
                Some(SetupLocalSave::NothingToArchive) => "nothing_to_archive",
                Some(SetupLocalSave::Failed { .. }) => "failed",
                Some(SetupLocalSave::Unknown { .. }) => "unknown",
                None => "not_attempted",
            },
            "masterkey": match save {
                Some(SetupLocalSave::Created | SetupLocalSave::Existed) => {
                    if declared { "declared" } else { "not_declared" }
                }
                // No repository, therefore no key: the question does not apply,
                // which is a third answer and not a "no".
                _ => "absent",
            },
            "destination": remote.outcome(),
            "schedule": if install_schedule { "planned" } else { "skipped" },
        },
        // The same three lists the exit code is decided from, so a wrapper that
        // reads them and a wrapper that reads `exit_code` are looking at one
        // decision rather than two.
        "incomplete": incomplete,
        "unread": unread,
        "destination": setup_remote_json(remote),
        "chain": chain,
        "runs": runs,
        "masterkey": match (save, local) {
            (Some(SetupLocalSave::Created | SetupLocalSave::Existed), Some(local)) => {
                serde_json::json!({
                    "path": local.key_file.display().to_string(),
                    "declaration": if declared { "declared" } else { "not_declared" },
                    // The declaration is a statement by the user. Nothing here,
                    // and nothing anywhere else, checks it — recorded so that a
                    // reader of this object cannot mistake one for a
                    // verification.
                    "declaration_is_verified": false,
                })
            }
            _ => serde_json::json!({
                "kind": "absent",
                "why": "no repository and no masterkey exist, so there is nothing to declare \
                        saved",
            }),
        },
    });
    json_string(&value)
}

fn render_setup_scan(report: &scanner::ScanReport) -> String {
    render_status(report, false)
}

fn prompt_setup_value(prompt: &str, value: &mut String) -> std::io::Result<()> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    value.clear();
    std::io::stdin().read_line(value)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_status(
    sessions: bool,
    json: bool,
    destination: Option<String>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    keep_ssh_masters: bool,
) -> ExitCode {
    // `status` is not a second exception to "an unusable config stops the
    // command": its two halves both read the config (the cadence that decides
    // staleness, the harness roots the scan walks), so a report built without it
    // would answer "is my timer healthy?" from a machine the tool cannot see.
    // It used to print `config_source=defaults_after_parse_error` and carry on
    // with defaults — which is the fallback this change removes, spelled out
    // rather than silent.
    let config = match Config::load() {
        Ok(config) => config,
        Err(e) => {
            let why = format!("{e:#}");
            if json {
                println!("{}", status_json_config_error(&why));
            }
            eprintln!("status: {why}");
            eprintln!(
                "status: exit_code=3 — the config file could not be read, so this report has no \
                 verdict on the timer, the scan, or any destination. Nothing below is a count of \
                 zero; there is no count at all."
            );
            return ExitCode::from(3);
        }
    };
    let info = run_state_info(&config);
    eprintln!("[run-once] {}", info.verdict.line);

    let scan = scanner::scan(&config);
    // Deliberately 3, not 1, when the scan fails. Nothing was scanned, so
    // `status` has no answer to give about this machine's coverage at all —
    // "did not finish / never started", the same call `doctor` makes on the
    // same failure. It used to be 1, which is the code this command spends on
    // "the timer is not running"; a caller could not tell an unreadable
    // registry from a dead scheduler. Both are still non-zero, which is all
    // `docs-dev/install.md` promises.
    //
    // Deliberately *not* folded in here either: a scan that succeeded but could
    // not read every session a harness claims (`report.probes` with a non-zero
    // `unreadable_count`). It is real and it is reported — B78 put it on the
    // `[scan]` line ("another N unreadable ... not archived") precisely because that is
    // its channel. Two reasons it must not also move the exit code:
    //
    //   * This code already means one thing — "is the scheduled run healthy?"
    //     (`run_state_info`, the `[run-once]` line above, documented in
    //     `docs-dev/install.md`). A second meaning on the same integer does not add
    //     information, it destroys the first: a non-zero `status` would no
    //     longer tell you whether to go look at your timer.
    //   * Unreadable sessions are a *steady state*, not an event. A Cursor
    //     store this build cannot decode reports the same count every run,
    //     forever. Wiring that to the exit code makes `status` permanently
    //     non-zero on a machine whose timer is fine — which is how users are
    //     taught to ignore the code (see the same argument in `destinit.rs`).
    //
    // A partial scan is a fact about coverage; the exit code here is a verdict
    // about the scheduler. Saying it in words is the fix; overloading the
    // integer would be the regression.
    //
    // B84 — and the third case, the one that reads wrong in a terminal: on a
    // machine that never ran `run-once`, `[run-once]` says "never completed a successful run"
    // and this returns **1**. That is deliberate, and it stays.
    //
    // `status` is two things at once. The body is a dashboard for a human; the
    // exit code is a health check for a script or a wrapper. Those two audiences
    // want different answers to "never ran", and the tie is broken by which one
    // *cannot* be served in prose: the human already read the sentence above,
    // so a 0 would tell them nothing they don't know; a script has nothing but
    // the integer, and for a script "no evidence the timer ever fired" is the
    // strongest reason there is to go look at the timer. So the integer serves
    // the script. `docs-dev/install.md` promises exactly this ("`status` exits with
    // non-zero when judged 'unhealthy'"), and says why in the same words as
    // `runstate.rs:186-192`: an absent record is the absence of evidence, not
    // evidence of health.
    //
    // The tempting counter-argument — "a user who just installed the binary has
    // never run it either, and that user is fine, not broken" — is answered by
    // what `status` is *for*. It is the "is the backup still working?" command
    // (`docs-dev/install.md:328`), so it is not run before there is anything to
    // check; and the freshly-installed state is precisely the state where the
    // timer is not yet doing its job. Calling that 0 would mean the code reads 0
    // both before the timer is installed and after it dies — the one interval a
    // monitored machine must be able to distinguish. A new user pays one
    // non-zero exit, once, with a sentence above it telling them what to do; the
    // alternative silently blesses every dead timer on every machine forever.
    //
    // Note for whoever reads a shell transcript and thinks this returns 0: the
    // whole report goes to *stderr*, so it is usually read through `2>&1 | …`,
    // and a pipeline reports the *last* command's status. `chat-stasher status
    // 2>&1 | head` is 0 no matter what this function returns. Pinned by
    // `tests/b84_runstate_test.rs`. With `--json` the report's JSON is the one
    // thing on stdout, so `chat-stasher status --json | jq …` reports `jq`'s
    // status, not the binary's — read the integer through `$?` or
    // `${PIPESTATUS[0]}` instead.
    let mut exit_code = match &scan {
        Ok(_) if info.verdict.healthy => 0,
        Ok(_) => 1,
        Err(e) => {
            eprintln!("status: scan failed: {e}");
            3
        }
    };

    let query_writer_versions = destination.is_some() || repo.is_some();
    let mut writer_versions = None;
    let mut writer_version_error = None;
    if query_writer_versions {
        if destination.is_none() && repo.is_none() {
            writer_version_error = Some("name the destination or pass --repo".to_string());
        } else {
            let cfg = resolve_store_config(
                &config,
                destination.as_deref(),
                repo,
                key_file,
                connections,
                options,
            );
            match store::load_key_file(&cfg).and_then(|mk| read_archive_writer_statuses(&cfg, &mk))
            {
                Ok(statuses) => writer_versions = Some(statuses),
                Err(e) => writer_version_error = Some(format!("{e:#}")),
            }
            reap_remote(&cfg, keep_ssh_masters);
        }
        if writer_version_error.is_some() {
            exit_code = 3;
        }
    }

    if json {
        match &scan {
            Ok(report) => {
                println!(
                    "{}",
                    status_json(
                        config.source,
                        &info,
                        Ok(report),
                        exit_code,
                        writer_versions.as_deref(),
                        writer_version_error.as_deref(),
                    )
                )
            }
            Err(e) => println!(
                "{}",
                status_json(
                    config.source,
                    &info,
                    Err(e.to_string()),
                    exit_code,
                    writer_versions.as_deref(),
                    writer_version_error.as_deref(),
                )
            ),
        }
        return ExitCode::from(exit_code);
    }

    if let Ok(report) = &scan {
        eprint!("{}", render_status(report, sessions));
    }
    if let Some(error) = &writer_version_error {
        eprintln!("[status] writer versions unavailable: {error}");
    }
    if let Some(versions) = &writer_versions {
        for writer in versions {
            eprintln!(
                "[status] writer version: machine={} version={} behind-newest={}",
                writer.machine,
                writer
                    .chat_stasher_version
                    .as_deref()
                    .unwrap_or(if writer.version_unreadable {
                        "unreadable"
                    } else {
                        "behind (version not recorded — written by ≤0.3.0)"
                    }),
                writer
                    .behind_newest_writer
                    .map_or("unknown", |behind| if behind { "yes" } else { "no" }),
            );
        }
    }
    ExitCode::from(exit_code)
}

/// Everything `status` needs from the run-state record, so both the human
/// verdict line and the `--json` object are derived from the same read.
struct RunStateInfo {
    verdict: chat_stasher::runstate::Verdict,
    read: chat_stasher::runstate::RunStateRead,
    now_unix: u64,
    stale_after_secs: u64,
}

/// Read the last `run-once` record and turn it into a verdict + the raw read
/// (so `status --json` can serialise the same read the sentence describes).
///
/// The cadence comes from the same config the scheduler templates use, so the
/// overdue threshold tracks whatever the user actually scheduled. An invalid
/// explicit value is an unknown timer verdict, not permission to substitute an
/// hourly cadence the user did not configure.
fn run_state_info(config: &Config) -> RunStateInfo {
    use chat_stasher::runstate;

    let state_dir = chat_stasher::collect::default_state_dir();
    let read = runstate::load(&state_dir);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        // reason: NOT an honest default — recorded as (a)-fragile, and found only
        // after the gate's second blind spot was fixed (B93). It needs a system
        // clock predating 1970 to reach. If reached, `now = 0` makes
        // `summarize` compute `0.saturating_sub(finished_at)` = 0, i.e. "the
        // scheduled run just finished, everything is healthy" — a broken clock
        // would be reported as good news, which is the worst direction for this
        // particular lie. Left as-is only because the branch is unreachable on a
        // sane host; if it ever becomes reachable, thread Option through
        // `summarize` and say the age is unknown.
        .unwrap_or(0);
    match schedule::interval_secs(config) {
        Ok(interval) => {
            let stale_after = runstate::stale_after_secs(interval);
            let verdict = runstate::summarize(&read, now, stale_after);
            RunStateInfo {
                verdict,
                read,
                now_unix: now,
                stale_after_secs: stale_after,
            }
        }
        Err(error) => RunStateInfo {
            verdict: runstate::Verdict {
                line: format!(
                    "backup_interval_secs is invalid: {error}. It is impossible to tell whether the timer is running as configured."
                ),
                healthy: false,
            },
            read,
            now_unix: now,
            // Unreachable for the JSON `run_state` shape when the interval is
            // invalid — `run_state_json` only uses it for a *known* record.
            stale_after_secs: runstate::STALE_FLOOR_SECS,
        },
    }
}

/// Serialise one `serde_json::Value` to a single stdout line. `json!` and the
/// tagged enums above can only produce JSON-safe values, so this is
/// infallible.
fn json_string(value: &serde_json::Value) -> String {
    serde_json::to_string(value).expect("serde_json cannot fail on a json! value")
}

/// The `status --json` object for a config that could not be used.
///
/// `--json` exists so a wrapper never has to parse prose, and that has to hold
/// on the refusing path too: `status` still writes one object to stdout, with the
/// same two fields it uses for any other "could not look" (`scanner.kind` is
/// `failed`, `why` is the config error) plus `config_error` and
/// `config_source: "unreadable"`. Every count is then absent rather than `0` —
/// the same rule the scanner object follows.
fn status_json_config_error(why: &str) -> String {
    json_string(&serde_json::json!({
        "schema_version": 1,
        "command": "status",
        "healthy": false,
        "exit_code": 3,
        "exit_semantics": status_exit_semantics(3),
        "config_source": chat_stasher::config::ConfigSource::Unreadable.label(),
        "config_error": why,
        "scanner": { "kind": "failed", "why": why },
    }))
}

/// The `status --json` object. `scan` is `Ok` when the registry-driven scan
/// ran (then every count is measured); `Err(reason)` when it could not run —
/// the `scanner` object is then `{"kind":"failed","why":…}` instead of an
/// `ok` object, so "could not look" is never read as "zero sessions".
fn status_json(
    config_source: config::ConfigSource,
    info: &RunStateInfo,
    scan: Result<&scanner::ScanReport, String>,
    exit_code: u8,
    writer_versions: Option<&[sidecar::MachineWriterStatus]>,
    writer_version_error: Option<&str>,
) -> String {
    let mut scanner_value = match scan {
        Ok(report) => scanner::scan_report_json(report),
        Err(why) => serde_json::json!({ "kind": "failed", "why": why }),
    };
    let writer_version_status = match (writer_versions, writer_version_error) {
        (Some(versions), _) => serde_json::json!({"kind":"known", "machines":versions}),
        (None, Some(error)) => serde_json::json!({"kind":"failed", "why":error}),
        (None, None) => serde_json::json!({"kind":"not_requested"}),
    };
    if let Some(scanner_object) = scanner_value.as_object_mut() {
        scanner_object.insert("writer_versions".to_string(), writer_version_status);
    }
    let value = serde_json::json!({
        "schema_version": 1,
        "command": "status",
        "healthy": exit_code == 0,
        "exit_code": exit_code,
        "exit_semantics": status_exit_semantics(exit_code),
        "config_source": config_source.label(),
        "run_state": chat_stasher::runstate::run_state_json(
            &info.read,
            info.now_unix,
            info.stale_after_secs
        ),
        "scanner": scanner_value,
    });
    json_string(&value)
}

/// What the exit code means, in words — the piece a 20-line shell wrapper
/// would otherwise have to hardcode.
fn status_exit_semantics(code: u8) -> &'static str {
    match code {
        0 => "0 = healthy: the timer is running, the last run succeeded and is not stale.",
        1 => "1 = unhealthy: never ran / timer stale / last run failed — for a script this is the \"go check the timer\" signal.",
        _ => "3 = no conclusion possible: the scan or requested destination read did not finish, so its counts and version state are unknown, not empty.",
    }
}

/// The scan half of `status`.
///
/// Default (`sessions = false`) is deliberately *fixed-size*: whatever the
/// scanner found, the body is a handful of aggregate lines, so the run-once
/// verdict printed above it stays on screen. Several hundred local sessions
/// used to push that verdict out of the scrollback, which defeats the only
/// question `status` exists to answer.
///
/// `sessions = true` restores the full per-session table — the real thing to
/// read when troubleshooting, just not the default.
///
/// Only ids, paths, sizes, mtimes and flags ever reach stdout — never the
/// content of a session.
fn render_status(report: &scanner::ScanReport, sessions: bool) -> String {
    let mut out = String::new();

    // One count per source actually found (registry-driven, so any harness
    // id from data/harness-registry-v1.json can appear here).
    let mut per_source: std::collections::BTreeMap<&str, usize> = Default::default();
    for rec in &report.records {
        *per_source.entry(rec.source.short()).or_default() += 1;
    }
    let compressed = report.records.iter().filter(|r| r.compressed).count();
    let gaps = report.archive_gaps();

    if !sessions {
        let breakdown = per_source
            .iter()
            .map(|(src, n)| format!("{src} {n}"))
            .collect::<Vec<_>>()
            .join(" · ");
        // B78: the count `doctor` prints and `status` used to swallow. It
        // rides on the `[scan]` line it qualifies — `status`'s default body
        // was cut from 463 lines to a handful and does not get to grow back,
        // and an empty string here means a clean machine sees the byte-for-
        // byte output it saw before.
        let unreadable = unreadable_notice(report);
        if report.records.is_empty() {
            // B82: "No sessions were found on this machine" is a claim about the whole
            // machine, and this branch used to make it from
            // `records.is_empty()` alone — with no regard for the harnesses
            // this run never got to look at. Zero records plus places we did
            // not look is not "there is nothing here".
            out.push_str(&format!(
                "[scan] No sessions were found on this machine.{}{unreadable}",
                unlooked_notice(report)
            ));
            out.push('\n');
        } else {
            out.push_str(&format!(
                "[scan] {} session(s) ({compressed} compressed): {breakdown}{unreadable}",
                report.records.len()
            ));
            out.push('\n');
        }
        if !report.missing_roots.is_empty() {
            out.push_str(&format!(
                "[scan] skipped {} non-existent source root(s).",
                report.missing_roots.len()
            ));
            out.push('\n');
        }
        // B82: kept off the line above on purpose. "not exist" is a measured
        // absence; these paths were never established to be absent, and
        // folding them into that count would restate the same lie in a
        // number.
        if !report.indeterminate_roots.is_empty() {
            out.push_str(&format!(
                "[scan] ⚠ another {} source root(s) are unreadable — whether they exist is unknown (session count unknown); see chat-stasher doctor",
                report.indeterminate_roots.len()
            ));
            out.push('\n');
        }
        if !gaps.is_empty() {
            out.push_str(&format!(
                "⚠ {} harness(es) have recognised sessions that collect will not archive.",
                gaps.len()
            ));
            out.push('\n');
        }
        out.push_str("details (one line per session): chat-stasher status --sessions\n");
        return out;
    }

    out.push('\n');
    for (src, n) in &per_source {
        out.push_str(&format!("  {src:<22} sessions : {n}\n"));
    }
    out.push_str(&format!(
        "  total                : {}  ({} compressed)",
        report.records.len(),
        compressed
    ));
    out.push('\n');
    for miss in &report.missing_roots {
        out.push_str(&format!("  (missing root, skipped: {})\n", miss.display()));
    }
    for unknown in &report.indeterminate_roots {
        out.push_str(&format!(
            "  (unreadable root, existence unknown: {})\n",
            unknown.display()
        ));
    }
    out.push_str(&render_archive_gap_notice(report));
    out.push('\n');

    if report.records.is_empty() {
        // Same claim, same qualification, in the `--sessions` table.
        out.push_str("  no sessions found.\n");
        let unlooked = unlooked_notice(report);
        if !unlooked.is_empty() {
            out.push_str(&format!(" {}\n", unlooked.trim_start()));
        }
        return out;
    }

    out.push_str(&format!(
        "  {:<14} {:>12} {:>14}  {:<3}  {}",
        "source", "bytes", "mtime(sec)", "zst", "id"
    ));
    out.push('\n');
    for rec in &report.records {
        // B90: `duration_since` fails for a timestamp *before* the epoch (a
        // SQLite store with a negative time value, a file whose mtime the OS
        // reports as pre-1970). Printing `0` there made it indistinguishable
        // from a session whose mtime really is 1970-01-01T00:00:00Z — the same
        // bug `inbox.rs` just removed from the audit cache, in the one table
        // that sweep did not reach. `modified_ns` there is `Option<u128>` and
        // a legacy `0` degrades to `None`; the column here says so in words.
        let mtime = rec
            .mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        out.push_str(&format!(
            "  {:<14} {:>12} {:>14}  {:<3}  {}",
            rec.source.short(),
            rec.byte_size,
            mtime,
            if rec.compressed { "zst" } else { "   " },
            short_session_id(&rec.id),
        ));
        out.push('\n');
    }
    out.push('\n');
    out
}

/// The clause appended to "No sessions were found on this machine" when that
/// sentence would otherwise be a claim we cannot back.
///
/// Counted here are the probes that never looked: `unascertained` (scanning a guessed
/// path is forbidden), a template that does not reduce to a root, and B82's
/// `Indeterminate` (the path could not be stat'd, is the wrong type, or its
/// store refused to enumerate). Deliberately *not* counted: `Missing` (looked,
/// nothing there) and `SkipWrongPlatform` (no cell for this platform, so
/// there is nothing on this machine to have missed).
///
/// Empty string when every probe was actually looked at — a machine with
/// sessions, or a genuinely empty one, prints what it printed before.
fn unlooked_notice(report: &scanner::ScanReport) -> String {
    let unlooked: Vec<&str> = report
        .probes
        .iter()
        .filter(|p| {
            matches!(
                p.state,
                scanner::ProbeState::SkipUnascertained
                    | scanner::ProbeState::SkipUnresolvable
                    | scanner::ProbeState::Indeterminate
            )
        })
        .map(|p| p.id.as_str())
        .collect();
    if unlooked.is_empty() {
        return String::new();
    }
    format!(
        "  ⚠ but {} harness(es) were not probed at all ({}) — \"did not scan\" is not \"there is none\"; see chat-stasher doctor",
        unlooked.len(),
        unlooked.join(" · ")
    )
}

/// The tail `status` appends to its `[scan]` line when some harness knows
/// about sessions it could not hand over (`HarnessProbe::unreadable_count`, one
/// count per harness, set by the probe in `scanner.rs`).
///
/// "Could not hand over" deliberately covers two situations that must not be
/// blurred (the reporter's Cursor machine is the second one):
///
///   * the body genuinely **cannot be read** here — permissions, corruption;
///     this is `unreadable` in the narrow sense;
///   * the row is an **index entry whose body is not available locally** —
///     Cursor 3.0 moved session bodies into a central store this build cannot
///     reach. The index says the session exists; the body is not on this
///     machine in any readable location. This is neither the tool's failure
///     nor a sign the user lost data.
///
/// The notice therefore says the sessions are "indexed but not archived" and
/// names "body not available locally" instead of "we failed to read them", so
/// a reader does not walk away thinking the tool dropped their data.
///
/// Three things this wording is doing on purpose:
///
///   * **`another`** — these are *not* inside the number printed to their left.
///     `cursor 3(+411)` would have been shorter and would have been read as
///     414; a count that can be mistaken for a total is worse than silence.
///   * **`not archived`** — the failure mode to avoid is a reader concluding the
///     unhanded-over sessions are safely in the archive. They are not in it.
///   * **empty string when every count is zero** — silence is the whole
///     contract with the "4 line status" work; nothing to say, nothing said.
fn unreadable_notice(report: &scanner::ScanReport) -> String {
    let per_harness: Vec<(&str, u64)> = report
        .probes
        .iter()
        .filter_map(|p| {
            p.unreadable_count
                .filter(|n| *n > 0)
                .map(|n| (p.id.as_str(), n))
        })
        .collect();
    let per_harness_entries: Vec<(&str, u64)> = report
        .probes
        .iter()
        .filter_map(|p| {
            p.unreadable_entry_count
                .filter(|n| *n > 0)
                .map(|n| (p.id.as_str(), n))
        })
        .collect();
    // B90: harnesses that *were* enumerated but whose unreadable tally could
    // not be taken. `None` alone does not qualify — a harness that was never
    // enumerated says so through its state, and pulling those in would put a
    // warning on every not-installed harness of a healthy machine.
    let uncounted: Vec<&str> = report
        .probes
        .iter()
        .filter(|p| p.record_count.is_some() && p.unreadable_count.is_none())
        .map(|p| p.id.as_str())
        .collect();
    if per_harness.is_empty() && per_harness_entries.is_empty() && uncounted.is_empty() {
        return String::new();
    }
    let total: u64 = per_harness.iter().map(|(_, n)| n).sum();
    // With a single culprit the per-harness count would just repeat the total,
    // so name it and stop.
    let who = if per_harness.len() == 1 {
        per_harness[0].0.to_string()
    } else {
        per_harness
            .iter()
            .map(|(id, n)| format!("{id} {n}"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    // B90: appended to whichever sentence is built below, never a line of its
    // own — `status`'s default body was cut to a handful of lines on purpose.
    let uncounted_clause = if uncounted.is_empty() {
        String::new()
    } else {
        format!(
            "; another {} harness(es) have an unknown unreadable count ({})",
            uncounted.len(),
            uncounted.join(" · ")
        )
    };
    if per_harness.is_empty() && per_harness_entries.is_empty() {
        return format!(
            "  ⚠ some harness(es) have an unknown unreadable count ({}); see chat-stasher doctor",
            uncounted.join(" · ")
        );
    }
    if per_harness_entries.is_empty() {
        return format!(
            "  ⚠ another {total} session(s) are indexed but not archived ({who}): bodies are not available locally — unreadable, or stored where this build cannot reach (not a sign of data loss){uncounted_clause}; see chat-stasher doctor"
        );
    }
    let entry_total: u64 = per_harness_entries.iter().map(|(_, n)| n).sum();
    let entry_who = if per_harness_entries.len() == 1 {
        per_harness_entries[0].0.to_string()
    } else {
        per_harness_entries
            .iter()
            .map(|(id, n)| format!("{id} {n}"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    let detail = if total > 0 {
        format!(
            "another {total} session(s) and {entry_total} unreadable directory item(s) (session count unknown) — not archived"
        )
    } else {
        format!(
            "another {entry_total} unreadable directory item(s) (session count unknown) — not archived"
        )
    };
    let who = if total > 0 {
        format!("{who} · {entry_who}")
    } else {
        entry_who
    };
    format!("  ⚠ {detail} ({who}){uncounted_clause}; see chat-stasher doctor")
}

fn render_archive_gap_notice(report: &scanner::ScanReport) -> String {
    let gaps = report.archive_gaps();
    if gaps.is_empty() {
        return String::new();
    }
    let mut output = String::new();
    output.push_str(
        "  ⚠ not archivable sessions: the following harness(es) recognised sessions but produced no SessionRecord; collect will not archive them for now.\n",
    );
    for gap in &gaps {
        output.push_str(&scanner::format_archive_gap(gap));
        output.push('\n');
    }
    output.push_str(
        "  advice: do not treat scanner records as the total number of recognised sessions; run collect again once the harness produces SessionRecords.\n",
    );
    output
}

#[cfg(test)]
mod narration_tests {
    use super::*;

    /// A writer that fails the way a closed pipe does.
    struct BrokenPipe;

    impl std::io::Write for BrokenPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "broken pipe",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "broken pipe",
            ))
        }
    }

    /// **Narration must not be able to kill `ui`.** The writer below fails
    /// exactly as a stdout whose reader has gone away does, and `say_to` must
    /// return rather than panic.
    ///
    /// This is the half of the W23 flake that lives in the binary: the other
    /// half is that a client may legitimately stop reading after the URL.
    /// `println!` here would reproduce the flake verbatim — process panics at
    /// `library/std/src/io/stdio.rs`, exit 101, every later request answered by
    /// a socket belonging to a dead process.
    #[test]
    fn a_broken_stdout_does_not_panic_the_narration() {
        say_to(
            &mut BrokenPipe,
            format_args!("[ui] bound        : {}", "127.0.0.1:1"),
        );
    }

    /// The instrument can say something, so the test above is not passing
    /// because `say_to` never writes at all.
    #[test]
    fn narration_reaches_a_working_writer() {
        let mut out: Vec<u8> = Vec::new();
        say_to(&mut out, format_args!("[ui] sessions     : {} in view", 3));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "[ui] sessions     : 3 in view\n"
        );
    }
}
