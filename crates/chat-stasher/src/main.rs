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
use chat_stasher::store::{self, BackupStore, StoreConfig};
use chat_stasher::verify::{CheckSummary, ReconcileReport, SessionOutcome};
use clap::{Parser, Subcommand};
use rustic_core::repofile::{MasterKey, NodeType};
use rustic_core::{Credentials, LsOptions, Repository, RepositoryOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

const UNRESOLVED_MACHINE: &str = "<machine-identity-unavailable>";

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
        /// Default: config `machine`, else this machine's identity (generated
        /// on first use); use `--machine` explicitly to choose the partition.
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
        /// Disable ssh connection reaping after this run (for troubleshooting).
        #[arg(long)]
        no_reap: bool,
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
        /// Lower bound (inclusive) on archive time (rustic snapshot time), unix seconds.
        #[arg(long)]
        since_unix: Option<i64>,
        /// Upper bound (inclusive) on archive time (rustic snapshot time), unix seconds.
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
    /// Open an ephemeral local web view of one destination's session list.
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
    /// Metadata tier only, same as `search`: machine, first 8 chars of the
    /// session id, shard count, byte length, archive time. Conversation text is
    /// NOT loaded and there is no full-text search; the page says so and prints
    /// what loading it would cost. Exit codes match `search`: 0 listed
    /// something, 1 read it all and there was nothing, 3 could not finish
    /// reading (or no key), 2 usage error.
    View {
        /// Destination to view. Required unless an explicit `--repo` is given:
        /// there is no default destination and no cross-destination merge.
        #[arg(long)]
        destination: Option<String>,
        /// Restrict the listing to one machine partition.
        #[arg(long)]
        machine: Option<String>,
        /// Restrict the listing to session ids with this prefix.
        #[arg(long)]
        session: Option<String>,
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
    },
    /// The Native Messaging host process itself.
    ///
    /// The framed stdio message loop is NOT implemented in this build. This
    /// subcommand exists so that the registration above points at something
    /// verifiable: `--self-test` prints one line of JSON on stdout and exits 0,
    /// which is how "does the host actually start?" is answered on a real
    /// machine and in later tickets. Without `--self-test` it exits 2 rather
    /// than pretending to serve a connection.
    ///
    /// Nothing but that one line may ever reach stdout: Chromium reads stdout
    /// as a `u32` frame length, so a stray log line is read as a multi-gigabyte
    /// frame and kills the pipe. Diagnostics go to stderr.
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
    /// The harness label is inferred from the session directory name: the
    /// canonical archived ids are `<source>.<machine>.<native-id>`, so the
    /// harness is the leading dot-segment (`claude-code.<m>.<uuid>` ->
    /// `claude-code`); short forms are handled too (`opencode~abc123` ->
    /// `opencode`, `cursor.d~xxx` -> `cursor`).
    ///
    /// Metadata only: the extraction keeps a timestamp per line and throws the
    /// line away. Nothing else about a conversation is ever read, kept or
    /// printed — the report is counts and the output path only.
    ///
    /// Exit codes: `0` = the whole partition was read and the index written;
    /// `3` = the stage could not be read (or reading was interrupted) — nothing
    /// (complete) was written; `1` = every session was read but the index file
    /// itself could not be written; `2` = usage error. Same family as
    /// `collect`/`search`: `3` means "did not finish / never started", `1`
    /// means "finished and then failed".
    ActivityIndex {
        /// Stage directory that holds the sealed `sessions/` tree.
        #[arg(long)]
        stage: PathBuf,
        /// Machine partition for `sessions/<machine>/…`.
        /// Default: config `machine`, else this machine's identity (generated
        /// on first use); use `--machine` explicitly to choose the partition.
        #[arg(long)]
        machine: Option<String>,
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
        /// Disable ssh connection reaping after this run.
        #[arg(long)]
        no_reap: bool,
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
        Command::Status { sessions, json } => cmd_status(sessions, json),
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
            no_reap,
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
            no_reap,
        ),
        Command::Doctor { json } => cmd_doctor(json),
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
            no_reap,
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
        Command::View {
            destination,
            machine,
            session,
            no_open,
            idle_timeout,
            repo,
            key_file,
            connections,
            options,
            no_reap,
        } => cmd_view(
            destination,
            machine,
            session,
            no_open,
            idle_timeout,
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
        ),
        Command::NativeHost { self_test } => cmd_native_host(self_test),
        Command::ActivityIndex { stage, machine } => cmd_activity_index(&stage, machine.as_deref()),
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
            no_reap,
        } => cmd_overview(
            destination,
            width,
            json,
            repo,
            key_file,
            connections,
            &options,
            no_reap,
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
) -> ExitCode {
    const TAG: &str = "[install-native-host]";
    let platform = platform.unwrap_or_else(nativehost::Platform::current);
    let root = match target_root {
        Some(root) => absolute_path(&root),
        None => nativehost::default_root(platform, &config::home_dir()),
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
        eprintln!(
            "install-native-host: nothing was written — no known browser directory under {}",
            root.display()
        );
        return ExitCode::from(3);
    }
    println!("{TAG} next: reload the extension, then connect to {host_name}.");
    ExitCode::SUCCESS
}

/// `native-host` — the host process. B98 ships the self-test stub only.
fn cmd_native_host(self_test: bool) -> ExitCode {
    if !self_test {
        eprintln!(
            "native-host: the stdio message loop is not implemented in this build; \
             run `native-host --self-test` to check that the host starts."
        );
        return ExitCode::from(2);
    }
    // Exactly one line, on stdout, and nothing else ever.
    println!(
        "{}",
        nativehost::self_test_line(nativehost::HOST_NAME, env!("CARGO_PKG_VERSION"))
    );
    ExitCode::SUCCESS
}

/// `activity-index` — build the activity sidecar for one machine partition
/// (ADR-017).
///
/// Reads every sealed shard of every session under
/// `<stage>/sessions/<machine>/`, concatenates each session's shards in global
/// sequence order (across buckets), and hands the lines to `activity::build_row`
/// for conversation-time extraction. One `ActivityRow` per session is written
/// as JSONL to `<stage>/meta/<machine>/activity-v1.jsonl`.
///
/// Exit codes follow the house family (`collect`/`search`): `0` = the whole
/// partition was read and the index written; `3` = the stage could not be read
/// (or reading was interrupted), so no complete index exists; `1` = every
/// session was read but the output file could not be written; `2` = usage
/// error (enforced by clap).
fn cmd_activity_index(stage: &Path, machine: Option<&str>) -> ExitCode {
    let config = Config::load();
    let machine = match resolve_machine("activity-index", &config, machine) {
        Ok(machine) => machine,
        Err(code) => return code,
    };
    if !stage.is_dir() {
        eprintln!(
            "activity-index: stage `{}` is not a directory — nothing was read",
            stage.display()
        );
        return ExitCode::from(3);
    }
    let sessions_root = stage.join(store::SESSIONS_DIR).join(&machine);

    // Enumerate the machine's session directories. A partition that has never
    // been archived is a *complete* read of zero items (empty, exit 0), not a
    // failure to look; any other read error means we could not read -> 3.
    let mut session_dirs: Vec<PathBuf> = Vec::new();
    match fs::read_dir(&sessions_root) {
        Ok(rd) => {
            for session in rd {
                let session = match session {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!(
                            "activity-index: cannot enumerate {}: {e}",
                            sessions_root.display()
                        );
                        return ExitCode::from(3);
                    }
                };
                if session.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    session_dirs.push(session.path());
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            eprintln!(
                "activity-index: cannot read {}: {e} — nothing was read, this is not an empty index",
                sessions_root.display()
            );
            return ExitCode::from(3);
        }
    }

    let mut rows = Vec::new();
    for session_dir in session_dirs {
        let session_id = session_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(harness) = sidecar::infer_harness(&session_id) else {
            eprintln!(
                "activity-index: session `{}` has no inferable harness — skipped",
                chat_stasher::id::short_session_id(&session_id)
            );
            continue;
        };

        // Global sequence order across both legacy and bucketed layouts.
        let mut shards = match store::sealed_shard_entries(&session_dir) {
            Ok(shards) => shards,
            Err(e) => {
                eprintln!("activity-index: cannot read session `{session_id}`: {e}");
                return ExitCode::from(3);
            }
        };
        shards.sort_by_key(|(seq, _)| *seq);

        let mut lines: Vec<String> = Vec::new();
        for (_, shard) in shards {
            let bytes = match fs::read(&shard) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("activity-index: cannot read shard {}: {e}", shard.display());
                    return ExitCode::from(3);
                }
            };
            for line in String::from_utf8_lossy(&bytes).lines() {
                lines.push(line.to_string());
            }
        }
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        rows.push(activity::build_row(&session_id, &machine, &harness, &refs));
    }

    // Reading finished; writing is a separate failure family. The read
    // completed, so "did not finish reading" (3) no longer applies — a write
    // failure is a completed-read failure (1).
    let meta_dir = stage.join("meta").join(&machine);
    if let Err(e) = fs::create_dir_all(&meta_dir) {
        eprintln!("activity-index: cannot create {}: {e}", meta_dir.display());
        return ExitCode::from(1);
    }
    let out_path = meta_dir.join("activity-v1.jsonl");
    let mut content = String::new();
    for row in &rows {
        content.push_str(&activity::to_jsonl(row));
    }
    if let Err(e) = fs::write(&out_path, content) {
        eprintln!("activity-index: cannot write {}: {e}", out_path.display());
        return ExitCode::from(1);
    }

    println!("[activity-index] stage    : {}", stage.display());
    println!("[activity-index] machine  : {machine}");
    println!("[activity-index] sessions : {}", rows.len());
    println!("[activity-index] index    : {}", out_path.display());
    ExitCode::SUCCESS
}

/// `machine-declare` — write this machine's display-name declaration (ADR-018).
///
/// The partition is resolved by the standard three-level rule (config `machine`,
/// then the identity file — generated when missing). The declaration goes to
/// `<stage>/meta/<machine>/machine.json` and rides into the archive on the next
/// `push`, which archives the whole stage root.
fn cmd_machine_declare(stage: &Path, display_name: Option<String>) -> ExitCode {
    let config = Config::load();
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
    let config = Config::load();
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
    let repo = Repository::new(&RepositoryOptions::default(), &backends)?
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
    no_reap: bool,
) -> ExitCode {
    let config = Config::load();
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
            reap_remote(&cfg, no_reap);
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
            let msg = format!("{e:#}");
            eprintln!("overview: {msg}");
            eprintln!(
                "overview: the archive was not read to completion — this is not an empty result"
            );
            reap_remote(&cfg, no_reap);
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
    } = read;

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
        let value = overview::overview_json(
            &rows,
            &snapshot_machines,
            &index_machines,
            &declared_machines,
            &display_names,
            exit_code,
        );
        println!("{}", json_string(&value));
        reap_remote(&cfg, no_reap);
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
        reap_remote(&cfg, no_reap);
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
    reap_remote(&cfg, no_reap);
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
    let store = BackupStore::for_metadata_query(cfg.clone());
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
                "  {}  machine={}  shards={}  bytes={}  snapshot={}  archive_time_unix={}",
                hit.short_id(),
                hit.machine,
                hit.shard_count,
                hit.bytes,
                hit.short_snapshot(),
                hit.archive_time_unix
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
fn cmd_view(
    destination: Option<String>,
    machine: Option<String>,
    session: Option<String>,
    no_open: bool,
    idle_timeout: u64,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    no_reap: bool,
) -> ExitCode {
    let config = Config::load();
    if destination.is_none() && repo.is_none() {
        eprintln!(
            "view: name the destination to view (`--destination <name>`, or an explicit `--repo`)"
        );
        eprintln!(
            "view: there is no default destination and no cross-destination merge — archives are not required to agree"
        );
        return ExitCode::from(2);
    }
    let label = destination
        .clone()
        .unwrap_or_else(|| "(explicit --repo)".to_string());
    let cfg = resolve_store_config(
        &config,
        destination.as_deref(),
        repo,
        key_file,
        connections,
        options,
    );
    let store = BackupStore::for_metadata_query(cfg.clone());
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            eprintln!("view: {e}");
            eprintln!("view: without the key nothing was read — this is not an empty result");
            reap_remote(&cfg, no_reap);
            // 3, not 1: the archive was never consulted. Same reasoning as
            // `search` — a lost key must not be indistinguishable from an
            // archive that genuinely holds nothing.
            return ExitCode::from(3);
        }
    };

    let mut filter = chat_stasher::search::SearchFilter::default();
    if let Some(m) = machine {
        filter = filter.machine(m);
    }
    if let Some(p) = session {
        filter = filter.session_id_prefix(p);
    }
    let report = match chat_stasher::search::search_sessions(&store, &mk, &filter) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("view: cannot read `{}`: {e}", cfg.repo_root);
            eprintln!("view: this is not an empty destination — the archive was not read");
            reap_remote(&cfg, no_reap);
            return ExitCode::from(3);
        }
    };
    // ssh masters are reaped before the server starts, not after: the serve loop
    // can sit idle for minutes, and there is nothing left to read by then.
    reap_remote(&cfg, no_reap);

    for path in &report.unreadable {
        println!("  !! unreadable: {path}");
    }
    if report.hits.is_empty() {
        println!("{}", report.no_hit_line());
        println!("view: nothing to show, so no server was started");
        return if report.complete() {
            ExitCode::from(1)
        } else {
            ExitCode::from(3)
        };
    }

    let data = chat_stasher::view::ViewData::from_report(&report, label);
    let token = match chat_stasher::view::new_token() {
        Ok(t) => t,
        Err(e) => {
            // No weaker fallback on purpose: a guessable token on a socket every
            // local program can reach is worse than refusing to serve.
            eprintln!("view: cannot read OS randomness for the access token: {e}");
            eprintln!("view: refusing to serve without a strong token");
            return ExitCode::from(3);
        }
    };
    let listener = match chat_stasher::view::bind_ephemeral() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("view: cannot bind 127.0.0.1:0 — {e}");
            return ExitCode::from(3);
        }
    };
    let addr = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("view: bound but cannot read local address — {e}");
            return ExitCode::from(3);
        }
    };

    let idle = if idle_timeout == 0 {
        Duration::from_secs(u64::MAX / 2)
    } else {
        Duration::from_secs(idle_timeout)
    };
    let url = format!("http://{addr}/?token={token}");
    println!("[view] destination  : {}", data.destination_label);
    println!(
        "[view] snapshots    : {} scanned / {} in repo",
        data.snapshots_scanned, data.snapshots_in_repo
    );
    println!(
        "[view] sessions     : {} listed / {} seen",
        data.sessions.len(),
        data.sessions_seen
    );
    println!("[view] data blobs read: {}", data.data_blobs_read);
    println!("[view] bound        : {addr} (loopback only, OS-assigned port)");
    println!(
        "[view] idle timeout : {}",
        if idle_timeout == 0 {
            "none (Ctrl+C to exit)".to_string()
        } else {
            format!("{idle_timeout}s")
        }
    );
    println!("[view] payload      : NOT loaded — metadata tier only, no full-text search");
    println!("[view] warning      : any program on this machine can reach 127.0.0.1; the token in the URL below is the only gate. Do not share it.");
    println!("{url}");

    if no_open {
        println!("[view] browser      : not opened (--no-open)");
    } else if let Err(e) = chat_stasher::view::open_in_browser(&url) {
        eprintln!("[view] browser      : could not open ({e}) — use the URL above, or --no-open");
    } else {
        println!("[view] browser      : opened");
    }

    let stats = match chat_stasher::view::serve(&listener, &token, &data, idle) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("view: serve loop failed: {e}");
            return ExitCode::from(3);
        }
    };
    println!(
        "[view] exiting      : idle for {}s · requests served={} rejected={}",
        idle_timeout, stats.served, stats.rejected
    );

    if !report.complete() {
        println!(
            "view: PARTIAL — the sessions listed are real, but `{}` could not be read in full ({} unreadable), so there may be more",
            data.destination_label,
            report.unreadable.len()
        );
        return ExitCode::from(3);
    }
    ExitCode::SUCCESS
}

fn cmd_doctor(json: bool) -> ExitCode {
    let report = chat_stasher::doctor::run();
    if json {
        let value = chat_stasher::doctor::report_to_json(&report);
        println!("{}", json_string(&value));
    } else {
        chat_stasher::doctor::print_report(&report);
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
    let config = Config::load();
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
        }
    }
    reap_remote(&target, no_reap);

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
    let config = Config::load();
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
                        false,
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
            false,
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
            format!(
                "platform cell confidence is not {}",
                scanner::CONF_CONFIRMED
            )
        };
        println!("[seal] REFUSED: {reason}");
        println!("[seal] active untouched : {}", active.display());
        return ExitCode::FAILURE;
    }
    let config = Config::load();
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
    if !report.errors.is_empty() {
        println!("[ingest] errors           : {}", report.errors.len());
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

/// Expand `~`/`~/` in a repo / key-file / option string and reject any
/// residual literal `~` component. On failure this is a usage error: exit 2,
/// like clap's own argument errors. This is the last gate before a path
/// (notably the masterkey file) reaches the filesystem — config-sourced values
/// were already expanded at load, and CLI `--repo`/`--key-file`/`--option`
/// values are expanded here, so a literal `~` never survives either route.
fn expand_path_arg(label: &str, value: &str) -> String {
    match chat_stasher::config::expand_and_verify(value) {
        Ok(path) => path.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("{label}: {e}");
            std::process::exit(2);
        }
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
    let repo_root = match repo.or_else(|| entry.repo.clone()) {
        Some(raw) => expand_path_arg("repo", &raw),
        None => {
            eprintln!("destination: `{name}` has no `repo` set (and no --repo was given)");
            std::process::exit(2);
        }
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
    // Expand + verify every option value. Config-sourced values were already
    // expanded at load; re-running is idempotent and also covers the CLI
    // `--option`s merged in above.
    for (k, v) in &mut merged {
        *v = expand_path_arg(&format!("option {k}"), v);
    }
    let key_file = match key_file.or_else(|| entry.key_file.clone()) {
        Some(raw) => PathBuf::from(expand_path_arg("key_file", &raw)),
        // Per-destination default: one key file per destination, so a new
        // destination never silently adopts another one's key.
        None => data_root().join(format!("masterkey-{name}.json")),
    };
    StoreConfig {
        repo_root,
        key_file,
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
    match reap::reap_masters_for_host(&host) {
        Ok(n) => println!("[reap] host {host} · ssh masters shut down: {n}"),
        Err(e) => {
            println!("[reap] host {host} · ssh masters shut down: unknown (could not read the process list: {e})")
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
    no_reap: bool,
) -> ExitCode {
    let config = Config::load();
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
    if stage_check.stage_shards == 0 {
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
            reap_remote(&cfg, no_reap);
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
    full_ids: bool,
    machine: Option<&str>,
    destination: Option<String>,
    repo: Option<String>,
    key_file: Option<String>,
    connections: Option<usize>,
    options: &[String],
    no_reap: bool,
) -> ExitCode {
    let config = Config::load();
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
    let store = match machine.as_deref() {
        Some(machine) => BackupStore::new(cfg.clone(), machine.to_string()),
        None => BackupStore::for_metadata_query(cfg.clone()),
    };
    let mk = match store::load_key_file(&cfg) {
        Ok(mk) => mk,
        Err(e) => {
            eprintln!("read: {e}");
            reap_remote(&cfg, no_reap);
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
            reap_remote(&cfg, no_reap);
            return ExitCode::from(3);
        };
        let stage = match stage {
            Some(s) => s,
            None => {
                eprintln!("read: `--stage` is required unless `--all-machines` is set");
                reap_remote(&cfg, no_reap);
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
                reap_remote(&cfg, no_reap);
                // Deliberately 2, not 1 or 3. The command is missing required
                // syntax, so this is a usage error before any archive read;
                // repository semantics reserve 2 for usage errors.
                return ExitCode::from(2);
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
                // Deliberately 3, not 1. A session readback that does not
                // finish cannot claim a complete result; this is the same
                // "did not finish" family as `search` and `collect`, while 1
                // is reserved for a read that completed and then failed.
                return ExitCode::from(3);
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
fn cmd_read_all_machines(store: &BackupStore, mk: &MasterKey, full_ids: bool) -> ExitCode {
    println!("[read] mode           : all-machines (newest snapshot per hostname)");
    println!("[read] repo           : {}", store.cfg.repo_root);
    let report = match store.read_all_machines(mk) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("read: {e}");
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
/// its own verdict; the exit code is FAILURE if any requested level failed.
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
    no_reap: bool,
) -> ExitCode {
    let config = Config::load();
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
    println!(
        "[verify] machine        : {}",
        machine.as_deref().unwrap_or(UNRESOLVED_MACHINE)
    );
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
            run_reconcile(&store, &mk, &stage, full_ids, &mut failed);
        }
        VerifyLevel::All => {
            run_check(&store, &mk, false, "L1 structure", &mut failed);
            run_check(&store, &mk, true, "L2 content", &mut failed);
            println!("[verify] stage          : {}", stage.display());
            run_reconcile(&store, &mk, &stage, full_ids, &mut failed);
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

fn run_reconcile(
    store: &BackupStore,
    mk: &MasterKey,
    stage: &Path,
    full_ids: bool,
    failed: &mut usize,
) {
    match store.reconcile_manifest(mk, stage) {
        Ok(report) => {
            print_reconcile(&report, full_ids);
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

fn print_reconcile(r: &ReconcileReport, full_ids: bool) {
    println!(
        "[verify] L3 reconcile     : machines={} expected={} took {:?}",
        r.machines,
        r.rows.len(),
        r.duration
    );
    for row in &r.rows {
        let session = display_session_id(&row.session_id, full_ids);
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
            display_session_id(s, full_ids)
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
        };

        let result = masterkey(&config).unwrap();
        assert!(result.1);
        assert!(config.key_file.is_file());
        assert!(store::load_key_file(&config).is_ok());
    }

    // -----------------------------------------------------------------------
    // ADR-018: 机器名三级解析 —— 显式 > config.machine > 身份文件
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
    // ADR-018: overview 显示名 —— 永不为空、永不把分区名冒充名字
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
    // ADR-018: machine-declare / machine-label 写文件
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

fn cmd_status(sessions: bool, json: bool) -> ExitCode {
    let config = Config::load();
    if config.source.is_error_fallback() {
        eprintln!("config_source={}", config.source.label());
    }
    let info = run_state_info(&config);
    eprintln!("[run-once] {}", info.verdict.line);

    let scan = scanner::scan(&config);
    // Deliberately 3, not 1, when the scan fails. Nothing was scanned, so
    // `status` has no answer to give about this machine's coverage at all —
    // "did not finish / never started", the same call `doctor` makes on the
    // same failure. It used to be 1, which is the code this command spends on
    // "the timer is not running"; a caller could not tell an unreadable
    // registry from a dead scheduler. Both are still non-zero, which is all
    // `docs/install.md` promises.
    //
    // Deliberately *not* folded in here either: a scan that succeeded but could
    // not read every session a harness claims (`report.probes` with a non-zero
    // `unreadable_count`). It is real and it is reported — B78 put it on the
    // `[scan]` line ("另有 N 条读不出来 …… 尚未归档") precisely because that is
    // its channel. Two reasons it must not also move the exit code:
    //
    //   * This code already means one thing — "is the scheduled run healthy?"
    //     (`run_state_info`, the `[run-once]` line above, documented in
    //     `docs/install.md`). A second meaning on the same integer does not add
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
    // machine that never ran `run-once`, `[run-once]` says "从未成功跑完一次"
    // and this returns **1**. That is deliberate, and it stays.
    //
    // `status` is two things at once. The body is a dashboard for a human; the
    // exit code is a health check for a script or a wrapper. Those two audiences
    // want different answers to "never ran", and the tie is broken by which one
    // *cannot* be served in prose: the human already read the sentence above,
    // so a 0 would tell them nothing they don't know; a script has nothing but
    // the integer, and for a script "no evidence the timer ever fired" is the
    // strongest reason there is to go look at the timer. So the integer serves
    // the script. `docs/install.md` promises exactly this ("`status` 在判定
    // 「不健康」时会以非零码退出"), and says why in the same words as
    // `runstate.rs:186-192`: an absent record is the absence of evidence, not
    // evidence of health.
    //
    // The tempting counter-argument — "a user who just installed the binary has
    // never run it either, and that user is fine, not broken" — is answered by
    // what `status` is *for*. It is the "is the backup still working?" command
    // (`docs/install.md:328`), so it is not run before there is anything to
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
    let exit_code = match &scan {
        Ok(_) if info.verdict.healthy => 0,
        Ok(_) => 1,
        Err(e) => {
            eprintln!("status: scan failed: {e}");
            3
        }
    };

    if json {
        match &scan {
            Ok(report) => {
                println!(
                    "{}",
                    status_json(config.source, &info, Ok(report), exit_code)
                )
            }
            Err(e) => println!(
                "{}",
                status_json(config.source, &info, Err(e.to_string()), exit_code)
            ),
        }
        return ExitCode::from(exit_code);
    }

    if let Ok(report) = &scan {
        eprint!("{}", render_status(report, sessions));
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

/// The `status --json` object. `scan` is `Ok` when the registry-driven scan
/// ran (then every count is measured); `Err(reason)` when it could not run —
/// the `scanner` object is then `{"kind":"failed","why":…}` instead of an
/// `ok` object, so "could not look" is never read as "zero sessions".
fn status_json(
    config_source: config::ConfigSource,
    info: &RunStateInfo,
    scan: Result<&scanner::ScanReport, String>,
    exit_code: u8,
) -> String {
    let scanner_value = match scan {
        Ok(report) => scanner::scan_report_json(report),
        Err(why) => serde_json::json!({ "kind": "failed", "why": why }),
    };
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
        _ => "3 = no conclusion possible: the scan failed (did not read, or did not finish), so the session counts above are all unknown, not 0.",
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
            // B82: "本机没有扫描到任何会话" is a claim about the whole
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
/// Counted here are the probes that never looked: `未查明` (scanning a guessed
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
