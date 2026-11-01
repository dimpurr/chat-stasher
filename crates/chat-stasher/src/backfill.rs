//! `chat-stasher backfill grok-bot` — the opt-in, user-started recovery of a
//! Grok Bot conversation's pre-window history (ADR-058 D1, BF-GROKBOT-1).
//!
//! The local persistence the collector reads is a bounded window (roughly
//! the last 200 records per conversation): every position that fell out of
//! it is gone from disk for good. The server still holds the conversation,
//! and whenever the app opens it, the app itself fetches the conversation's
//! whole user-visible span into view — while never persisting those rows
//! back into its local cache (measured end to end by the 2026-10-12 probe).
//! This module is the app-driven route through that fact: it relaunches the
//! app with a loopback debugging connection, opens one conversation at a
//! time exactly as a person clicking the sidebar would, waits while the app
//! loads that conversation, harvests the rows the app then holds, and
//! delivers the ones the archive does not already hold — through the
//! collector's own delivered-set identity, so nothing is added twice.
//!
//! Binding properties, all of them acceptance criteria from the decided
//! spec; a change that weakens any one of these is out of scope here:
//!
//! * **User-started only.** Nothing but the CLI registration may reach
//!   [`run_grok_bot_backfill`]: no config-loading, schedule, tick or daemon
//!   path references this module (pinned by a static test).
//! * **Dry run by default.** With no extra flags the identical read-only
//!   traversal runs — it too relaunches the app over the loopback debug
//!   port, one conversation at a time, at the same pacing — and it only
//!   reports. Delivery is gated behind the explicit `--i-accept-the-risk`
//!   flag, verbatim; the risk statement is printed before anything touches
//!   the app in **every** run, a dry one included.
//! * **One conversation at a time, ≥ 10 s apart** — patient, human scale,
//!   measured wall clock through an injectable clock, in dry runs too.
//! * **Never handles credentials.** The only channel into the app is the
//!   renderer over a debugging port bound to `127.0.0.1`; the app's own
//!   process performs every service call; this module never calls a vendor
//!   endpoint itself and contains no code path that could — the transport
//!   builds only `127.0.0.1` URLs, and credential-file names and vendor
//!   host names appear nowhere in it (pinned by a statics test). The port
//!   must answer on loopback only, verified before the connection is used,
//!   or the run refuses.
//! * **Writes nothing under the app's directories.** The persistence
//!   directory is read only, through the same reader the live scan uses.
//! * **App-driven, not service-driven**: ordinary UI clicks and view-state
//!   reads only. Never a bot gateway, never `ReadTranscript`, never a
//!   direct service call — those routes are decided against (ADR-058 D2/D3)
//!   and are not added here.
//! * **Delivers only rows the session has not already delivered** (the
//!   W986 session-keyed identity: machine + agent uuid, never the source
//!   path), so re-runs and interleaving with an ordinary collect add
//!   nothing twice. Byte-identical row derivation
//!   ([`replica_record_from_view_row`]) makes the digest dedup skip
//!   everything the live collector already archived.
//! * **Provenance `backfill`** on every shard this run seals, with the
//!   run's id and the app version it drove. The session keeps its
//!   known-unknown gap runs and `partial` fidelity — the service never
//!   returns the withheld interior/tool rows at any depth, and nothing
//!   here claims completeness.
//! * **The app is restored to a normal launch at the end of every exit
//!   path** — previously-open conversation put back (best effort), the
//!   debug relaunch quit by exact recorded pid, the quit waited out
//!   (recorded pids and children fully exited, the debugging port
//!   released, so the plain relaunch does not race the dying
//!   instance's single-instance lock), the app relaunched with no flags
//!   and verified running through a settle window, the close-out
//!   verified (no debugging port listening, sync resumed) and reported.
//!   A failed restore is reported with the recorded pid and port,
//!   never swallowed into a success exit — and a traversal that
//!   stopped keeps its own verdict and its stop text even when the
//!   close-out fails on top of it.
//! * **macOS only** — the only measured platform, and the gate is the CLI
//!   registration's, there where the real, platform-specific driver is
//!   built: a build for any other platform answers
//!   [`MACOS_ONLY_REFUSAL`] before anything is touched. The run beneath
//!   is platform-neutral orchestration — every platform contact sits
//!   behind the injected [`AppDriver`] — so the suite drives it
//!   identically on every platform it runs on.
//!
//! Known-unknowns stayed as known-unknowns: a conversation that does not
//! finish loading within its wait is skipped as a whole — nothing partial
//! or uncertain is taken from it — and reported so a later run can try it
//! again; a renderer shape change stops the whole run rather than guessing.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::collect as collector;
use crate::models::HarnessSource;

/// Minimum spacing between one conversation's harvest end and the next
/// open, in milliseconds: ten seconds, measured wall clock (ADR-058 D1).
pub const PACING_MIN_GAP_MS: u64 = 10_000;

/// The bounded wait for one conversation's load to go quiet, milliseconds.
pub const LOAD_QUIESCENCE_WAIT_MS: u64 = 90_000;

/// Gap between quiescence polls, milliseconds.
const LOAD_POLL_INTERVAL_MS: u64 = 250;

/// The bounded wait, milliseconds, for the relaunched app to open its
/// debugging endpoint on the loopback port.
pub const APP_BOOT_WAIT_MS: u64 = 60_000;

/// The bounded wait, milliseconds, for the relaunched app's renderer to
/// present a populated sidebar — the app's boot continues past its
/// debugging endpoint answering, and a sidebar read before the renderer
/// has painted its list reads as a traversal over zero conversations,
/// which is a boot state, never a measurement. The poll is a read-only
/// view-state query every half-second; it makes no request the app
/// itself would not make while a person watches it start.
pub const SIDEBAR_READY_WAIT_MS: u64 = 60_000;

/// Gap between sidebar readiness polls, milliseconds.
const SIDEBAR_READY_POLL_INTERVAL_MS: u64 = 500;

/// The bounded wait, milliseconds, for the app's own persistence writes to
/// resume after the normal relaunch before the close-out reports them as
/// unconfirmed. A relaunched app's first sync pass has been observed
/// taking up to about a minute, so the wait keeps a margin over that
/// measurement rather than turning a healthy relaunch into an unverified
/// close-out at the boundary.
pub const SYNC_RESUME_WAIT_MS: u64 = 90_000;

// ---------------------------------------------------------------------------
// Texts the user sees. The risk wording is the public doc section's own
// paragraph (docs/sources/grok-bot.md), so the CLI, the docs and the ADR
// all state the same proportionate risk — the wording Dim approved.
//

pub const RISK_STATEMENT: &str = "The risk, stated once and plainly. Driving a \
signed-in client with software is not something the vendor documents or endorses, and \
terms-of-service risk cannot be ruled out — which is why the command is optional, \
never starts on its own, states this caution before every run, a dry one included, \
and keeps the delivery of rows behind an explicit acknowledgement. Its mechanics stay \
as close to ordinary use as possible: every request to the service is made by the app \
itself, with its own sign-in, exactly as when a conversation is opened by hand, so \
what the vendor sees is a patient reader going through their own conversations, one \
at a time. The debugging connection exists only on this machine and only while the \
run lasts; the app itself, its settings and its files are never modified. The \
remaining cost is fragility rather than safety: the command reads what the app \
happens to show, so a future app update can break it — and when that happens it \
stops, reports what it could not find, and changes nothing.";

pub const HEADS_UP: &str = "heads-up: this run will quit and relaunch Grok Bot with a \
debugging connection that listens only on this machine, open the conversations it \
lists one at a time, and read the rows the app shows while it waits";

pub const DRY_RUN_NOTICE: &str = "this is a dry run: it drives the app exactly like a \
real one, but it delivers and writes nothing; to deliver the rows it finds, run it \
again with --i-accept-the-risk";

pub const DELIVERING_NOTICE: &str = "--i-accept-the-risk given: this run delivers the \
rows it finds that the archive does not already hold, each marked as backfilled";

/// The refusal a build for any platform other than macOS answers before
/// anything is touched: the app-driven route was measured on macOS, and
/// this build refuses to drive anything it has not measured. The gate
/// itself is the CLI registration's, where the real driver exists for
/// macOS only — printed there (after the risk statement) so the wording
/// stays owned by the module that owns every other run text.
pub const MACOS_ONLY_REFUSAL: &str = "backfill is macOS only: the app-driven route was \
 measured on macOS, and this build refuses to drive anything it has not measured";

// ---------------------------------------------------------------------------
// The injectable clock (the push_progress precedent: production uses the
// wall clock, tests freeze and advance it). Every wait and every pacing gap
// in a run goes through this, so the run's timing is measurable from the
// report alone and a test can compress hours into nothing.
//

#[derive(Clone)]
pub struct Clock {
    now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
    sleep_ms: Arc<dyn Fn(u64) + Send + Sync>,
}

impl Clock {
    /// The production clock: real time, real sleeps.
    pub fn wall() -> Self {
        Self {
            now_ms: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis() as u64)
                    // reason: the wall clock cannot predate the epoch on a
                    // running machine; 0 still paces, just wrongly
                    .unwrap_or_default()
            }),
            sleep_ms: Arc::new(|ms| std::thread::sleep(std::time::Duration::from_millis(ms))),
        }
    }

    /// A test clock: the caller owns time.
    pub fn new(
        now_ms: Arc<dyn Fn() -> u64 + Send + Sync>,
        sleep_ms: Arc<dyn Fn(u64) + Send + Sync>,
    ) -> Self {
        Self { now_ms, sleep_ms }
    }

    pub fn now_ms(&self) -> u64 {
        (self.now_ms)()
    }

    pub(crate) fn sleep_ms(&self, ms: u64) {
        (self.sleep_ms)(ms);
    }
}

// ---------------------------------------------------------------------------
// The driver: everything the run does to the app, behind one trait so
// tests drive a mock and the production CDP implementation
// ([`cdp::CdpAppDriver`]) is only ever exercised in tests against a mock
// CDP endpoint.
//

#[derive(Debug, Clone, Default)]
pub struct AppStatus {
    pub installed_version: Option<String>,
    pub running_pids: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiConversation {
    pub agent_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct RowsSnapshot {
    pub rows: Vec<serde_json::Value>,
}

#[derive(Debug)]
pub enum DriverError {
    /// The renderer view-state predicates failed: the app's layout is not
    /// the one this build reads. Stops the whole run; the message says
    /// what was missing.
    ShapeBreak(String),
    /// The debugging connection itself failed. Stops the whole run.
    Fatal(anyhow::Error),
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ShapeBreak(what) => write!(f, "app layout changed: {what}"),
            Self::Fatal(error) => write!(f, "{error:#}"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// Restore was actually performed (the run had relaunched the app). A
    /// run that never got as far as quitting a running app makes restore
    /// a no-op and says so here.
    pub attempted: bool,
    /// The recorded debug launch pid and port, reported so the user can
    /// finish restoration by hand if anything failed.
    pub debug_pid: Option<u32>,
    pub debug_port: u16,
    /// Verified: nothing still listens on the debug port.
    pub port_closed: Option<bool>,
    /// Verified: the app's own persistence writes resumed after the normal
    /// relaunch. `None` = could not be observed within the bounded wait.
    pub sync_resumed: Option<bool>,
    /// Whether the restore owed a plain relaunch at all: whether an app
    /// was already running before the run quit it. Set by the run from
    /// its own preflight, not by the driver — a run that started against
    /// no running app owes none, and launching one would start an app
    /// that was never running, a state change masquerading as a restore.
    pub relaunch_owed: bool,
    /// Verified: the owed relaunch came up and stayed running through its
    /// settle window — the window that catches an instance that dies on
    /// the just-quit instance's single-instance lock, the quietest way a
    /// machine can end up with no app at all. `Some(false)` is a
    /// measured dead relaunch; `None` = none was owed, or it could not
    /// be observed within its bounded wait.
    pub relaunch_running: Option<bool>,
    /// The previously-open conversation was clicked back before the
    /// debug relaunch was quit, so the plain relaunch restores it
    /// itself. `None` = no conversation was open when the run began, or
    /// the re-selection could not be attempted; `Some(false)` =
    /// attempted and the sidebar did not show the conversation. A plain
    /// relaunch carries no debugging connection, so the re-selection is
    /// performed, never observed on the relaunched instance.
    pub conversation_reselected: Option<bool>,
    /// Restore could not be completed; the message names what failed.
    pub failure: Option<String>,
}

pub trait AppDriver {
    /// Preflight, before anything is quit or relaunched: install/version
    /// and running pids.
    fn app_status(&mut self) -> Result<AppStatus>;

    /// Quit the running app by exact pid, relaunch it with the loopback
    /// debugging port, wait until the endpoint answers, and verify it
    /// binds to loopback only. Returns the relaunched main pid. On error
    /// the run still calls [`Self::restore_normal_launch`] next; a driver
    /// whose relaunch failed before anything was quit must make restore
    /// the no-op it is.
    fn relaunch_for_backfill(&mut self, port: u16) -> std::result::Result<u32, DriverError>;

    /// The conversations the app's sidebar lists, by agent uuid.
    fn list_conversations(&mut self) -> std::result::Result<Vec<UiConversation>, DriverError>;

    /// The conversation the app currently has open, by agent uuid;
    /// `Ok(None)` = none detectably open.
    fn currently_open(&mut self) -> std::result::Result<Option<String>, DriverError>;

    /// Open one conversation by a synthesized ordinary UI click.
    fn open_conversation(&mut self, agent_id: &str) -> std::result::Result<(), DriverError>;

    /// Read the rows the app currently holds for the open conversation.
    fn read_rows(&mut self) -> std::result::Result<RowsSnapshot, DriverError>;

    /// Synthesize an up-scroll to nudge the app's own older-history pager.
    fn nudge_older_pages(&mut self) -> std::result::Result<(), DriverError>;

    /// Restore a normal launch: re-select the previously-open conversation
    /// (best effort, performed before the quit and reported), quit the
    /// debug relaunch by the exact recorded pid and its recorded
    /// children, wait that quit out — every recorded pid fully exited,
    /// the debugging port released — so the plain relaunch cannot start
    /// inside the dying instance's single-instance window, relaunch with
    /// no flags, and verify the relaunch stayed running and no debugging
    /// port is listening.
    fn restore_normal_launch(
        &mut self,
        previously_open: Option<&str>,
    ) -> std::result::Result<RestoreOutcome, DriverError>;

    /// Last-write timestamp (ms) of the app's own persistence cache, best
    /// effort, for the close-out's sync-resume check.
    fn sync_evidence_ms(&mut self) -> Result<Option<u64>>;
}

// ---------------------------------------------------------------------------
// The report: counts, short id prefixes and timestamps only — never
// conversation text, never titles.
//

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The conversation never finished loading within its wait: nothing
    /// partial was taken, and a later run can try it again.
    NotFullyLoaded,
    /// Delivering this conversation failed; the run continued.
    Failed(String),
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFullyLoaded => write!(f, "skipped-not-fully-loaded"),
            Self::Failed(what) => write!(f, "failed: {what}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BackfillConversation {
    pub session_prefix: String,
    pub new_to_archive: bool,
    pub rows_delivered: u64,
    pub rows_already_delivered: u64,
    pub loaded_min_seq: Option<u64>,
    pub loaded_max_seq: Option<u64>,
    pub local_min_seq: Option<u64>,
    pub local_max_seq: Option<u64>,
    pub withheld_positions: Option<u64>,
    pub skipped: Option<SkipReason>,
    pub opened_at_ms: u64,
    pub finished_at_ms: u64,
}

impl BackfillConversation {
    /// The one-line invoice the CLI prints per conversation: counts and
    /// short ids only.
    pub fn summary_line(&self) -> String {
        let mut line = format!(
            "[backfill] conversation {} : rows_{}={} rows_already_delivered={} loaded_span={} \
             local_span={} withheld_positions={}",
            self.session_prefix,
            if self.skipped.is_some() {
                "found"
            } else {
                "delivered"
            },
            self.rows_delivered,
            self.rows_already_delivered,
            span(self.loaded_min_seq, self.loaded_max_seq),
            span(self.local_min_seq, self.local_max_seq),
            self.withheld_positions
                .map(|count| count.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
        );
        if self.new_to_archive {
            line.push_str(" new-to-archive");
        }
        if let Some(skipped) = &self.skipped {
            line.push_str(&format!(" {skipped}"));
        }
        line
    }
}

fn span(min: Option<u64>, max: Option<u64>) -> String {
    match (min, max) {
        (Some(min), Some(max)) if min <= max => format!("{min}..{max}"),
        (Some(min), Some(max)) => format!("{max}..{min}"),
        _ => "unknown".to_string(),
    }
}

#[derive(Debug, Clone)]
pub struct BackfillReport {
    pub dry_run: bool,
    pub run_id: String,
    pub app_version_start: Option<String>,
    pub app_version_end: Option<String>,
    pub debug_port: u16,
    pub previously_open: Option<String>,
    pub conversations: Vec<BackfillConversation>,
    /// The `--conversation` prefixes that matched no conversation.
    pub unmatched_prefixes: Vec<String>,
    pub restore: RestoreOutcome,
    /// The notices the run printed before anything touched the app (the
    /// risk statement, the heads-up and the mode note) — kept so tests can
    /// pin that the run leads with the risk wording.
    pub notices: Vec<String>,
}

impl BackfillReport {
    pub fn rows_total_delivered(&self) -> u64 {
        self.conversations
            .iter()
            .map(|item| item.rows_delivered)
            .sum()
    }
}

#[derive(Debug, Clone)]
pub struct BackfillRefusal {
    pub reason: String,
    /// Refusals are exit 3 in the documented family; usage-shaped ones
    /// (arguments that name nothing, a stage that is not a directory)
    /// are exit 2.
    pub usage: bool,
}

#[derive(Debug)]
pub enum BackfillOutcome {
    /// The traversal completed and every visited conversation loaded.
    Completed(BackfillReport),
    /// The traversal completed, but conversation-level findings remain (a
    /// skipped conversation, an unmatched prefix). Exit 1.
    CompletedWithFindings(BackfillReport, Vec<String>),
    /// The run refused before anything touched the app. Exit 3 (or 2 when
    /// `usage`). Nothing was written anywhere.
    Refused(BackfillRefusal),
    /// The run stopped before finishing — a renderer shape break, a dead
    /// transport, a user interrupt, an unusable debug endpoint. Partial
    /// work is kept and reported. Exit 3.
    Stopped(BackfillReport, String),
    /// The traversal ran but the app could not be restored to a normal
    /// launch, or the close-out could not be verified. Exit 1.
    RestoreFailed(BackfillReport, String),
}

impl BackfillOutcome {
    /// The exit code for this outcome, from the documented family:
    /// 0 answered completely · 1 finished, then failed · 2 usage · 3 could
    /// not finish (or never started).
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Completed(_) => 0,
            Self::RestoreFailed(..) | Self::CompletedWithFindings(..) => 1,
            Self::Refused(refusal) => {
                if refusal.usage {
                    2
                } else {
                    3
                }
            }
            Self::Stopped(..) => 3,
        }
    }
}

// ---------------------------------------------------------------------------
// The request.
//

pub struct BackfillRequest<'a> {
    /// Stage directory, as for `collect`; never created.
    pub stage: &'a Path,
    pub machine: &'a str,
    /// The collector's state directory (`collect::default_state_dir`).
    pub state_dir: &'a Path,
    pub shard_bucket_cap: usize,
    /// The destination whose delivered state the run extends, named like
    /// the backup import's.
    pub destination: &'a collector::DestinationView<'a>,
    /// `--i-accept-the-risk` given: deliver instead of only reporting.
    pub deliver: bool,
    /// `--conversation <prefix>`, repeatable; empty = every conversation
    /// the sidebar lists.
    pub conversation_prefixes: Vec<String>,
    /// The grok-bot root as the scanner resolves it: the signed-in probe,
    /// the preflight snapshot and the restore sync check all read through
    /// the existing reader path from here.
    pub persistence_root: &'a Path,
    /// The interrupt flag: a SIGINT/SIGTERM handler (or a test) sets it.
    pub interrupt: &'a AtomicBool,
    pub clock: Clock,
    pub driver: Box<dyn AppDriver>,
}

/// Map one view-state row the app loaded to the exact record the collector
/// delivers — through the reader's own replica-record model, unchanged.
///
/// The app's loaded entries carry `seq`/`kind`/`message`/`requestId`/`id`/
/// `timestampMs`, every one of which is in the measured persistence key
/// union, and the 2026-10-12 probe measured the view-state rows to be the
/// store's own rows (identical kind histograms at identical seqs). The
/// adapter therefore invents, drops and renames nothing: a row the app also
/// persisted maps to the identical bytes the live collector would derive
/// from its persistence blob, so the digest dedup skips it. Should a future
/// app version make the shapes differ, the difference degrades to a
/// same-seq variant the union keeps — visible in the gap report, never
/// silent, and ADR-058's revisit trigger T3 covers the alarm.
pub fn replica_record_from_view_row(row: Value) -> Result<crate::grok_bot::ReplicaRecord> {
    crate::grok_bot::ReplicaRecord::from_value(row)
}

// ---------------------------------------------------------------------------
// The run. Control-flow shape, kept deliberately explicit:
//
// 1. refusal — everything proveable before anything touches the app;
// 2. preface — the risk statement, the heads-up and the mode note, printed
//    before the first app contact;
// 3. traversal — the relaunch, then one conversation at a time;
// 4. close-out — a single site every post-relaunch exit path flows through,
//    restoring the app and verifying the restore, so "restored on every
//    exit" is structural rather than remembered.
//

fn refuse(reason: String, usage: bool) -> BackfillOutcome {
    BackfillOutcome::Refused(BackfillRefusal { reason, usage })
}

/// One conversation's local-window facts from the preflight snapshot.
#[derive(Debug, Clone, Default)]
struct LocalConversation {
    min_seq: Option<u64>,
    max_seq: Option<u64>,
    source_path: Option<std::path::PathBuf>,
}

/// Why the traversal stopped before finishing.
#[derive(Debug)]
enum TraverseStop {
    Interrupted,
    Shape(String),
    Fatal(String),
}

impl TraverseStop {
    fn describe(&self) -> String {
        match self {
            Self::Interrupted => "interrupted: stopped between conversations; what has \
                been delivered stays delivered and the app was restored"
                .to_string(),
            Self::Shape(what) => format!(
                "app layout changed: {what} (unknown app versions are allowed, but the \
                 shape checks fail loudly, never guess)"
            ),
            Self::Fatal(what) => format!("the run could not continue: {what}"),
        }
    }
}

struct Scratch {
    previously_open: Option<String>,
    reports: Vec<BackfillConversation>,
    unmatched_prefixes: Vec<String>,
}

/// Delivery state this run reads and (in a real run) extends — the same
/// shapes and verifications the backup import uses, plus the run's own
/// identifiers for its provenance stamp.
struct RunContext {
    run_id: String,
    ran_at: String,
    app_version_start: Option<String>,
    agent_names: BTreeMap<String, String>,
    locals: BTreeMap<String, LocalConversation>,
    state_path: std::path::PathBuf,
    state: collector::DebtState,
    destination_id: String,
    debts: BTreeMap<String, collector::DebtEntry>,
    remote_facts: Option<collector::ArchiveFacts>,
    manifest_facts: Option<BTreeMap<String, collector::ShardFact>>,
}

struct DeliveryOutcome {
    new_to_archive: bool,
    rows_delivered: u64,
    rows_already_delivered: u64,
}

/// How the walk through one conversation went.
enum Walk {
    Rows(Vec<crate::grok_bot::ReplicaRecord>),
    Skipped(SkipReason),
    Stop(TraverseStop),
}

enum TraversalEnd {
    Done,
    Stop(TraverseStop),
}

pub fn run_grok_bot_backfill(mut req: BackfillRequest<'_>, out: &mut dyn Write) -> BackfillOutcome {
    // ---- Refusals: everything proveable before anything touches the app.
    //
    // The risk statement leads every invocation, refusals included, so its
    // wording is always the first thing said (the tests pin that).
    writeln!(out, "{RISK_STATEMENT}").ok();
    let mut notices = vec![RISK_STATEMENT.to_string()];

    if !req.stage.is_dir() {
        return refuse(
            format!(
                "stage {} is not a directory; backfill never creates a stage",
                req.stage.display()
            ),
            true,
        );
    }
    for prefix in &req.conversation_prefixes {
        let trimmed = prefix.trim();
        if trimmed.is_empty() || trimmed.len() > 128 {
            return refuse(
                format!("conversation prefix {prefix:?} is not a usable prefix"),
                true,
            );
        }
    }

    // The app must be installed.
    let status = match req.driver.app_status() {
        Ok(status) => status,
        Err(error) => {
            return refuse(
                format!("the Grok Bot install could not be measured: {error:#}"),
                false,
            );
        }
    };
    let app_version_start = status.installed_version;
    if app_version_start.is_none() {
        return refuse(
            "the Grok Bot app is not installed where this build looks for it; nothing \
             was touched"
                .to_string(),
            false,
        );
    }
    // Whether an app was already running before this run touches
    // anything: the one fact that decides whether the close-out owes a
    // plain relaunch back. Read here, before the first quit, so it stays
    // the pre-run truth no matter how the run ends.
    let app_was_running = !status.running_pids.is_empty();

    // Preflight snapshot through the existing reader path — the same read a
    // live scan performs — doubling as the signed-in probe: a signed-in app
    // keeps local replica caches, so an unreadable or empty persistence
    // tree cannot confirm the app would load for this account here.
    let snapshot = match crate::grok_bot::read_from_app_support(req.persistence_root) {
        Ok(agents) => agents,
        Err(error) => {
            return refuse(
                format!(
                    "the Grok Bot persistence root {} could not be read, so a signed-in \
                     app cannot be confirmed: {error:#}",
                    req.persistence_root.display()
                ),
                false,
            )
        }
    };
    if snapshot.is_empty() {
        return refuse(
            format!(
                "no Grok Bot persistence replicas were found under {}, so a signed-in \
                 app cannot be confirmed; nothing was touched",
                req.persistence_root.display()
            ),
            false,
        );
    }
    let mut locals: BTreeMap<String, LocalConversation> = BTreeMap::new();
    let mut agent_names: BTreeMap<String, String> = BTreeMap::new();
    for agent in &snapshot {
        locals.insert(
            agent.agent_id.clone(),
            LocalConversation {
                min_seq: agent.records.first().map(|record| record.sequence),
                max_seq: agent.max_sequence,
                source_path: Some(agent.source_path.clone()),
            },
        );
        if let Some(name) = &agent.name {
            agent_names.insert(agent.agent_id.clone(), name.clone());
        }
    }

    // ---- Preface: the heads-up and the mode note, before the first app
    // contact. A dry run states its refusal of delivery here, naming the
    // flag — the flag itself gates only delivery, never the driving.
    writeln!(out, "{HEADS_UP}").ok();
    notices.push(HEADS_UP.to_string());
    let mode_notice = if req.deliver {
        DELIVERING_NOTICE
    } else {
        DRY_RUN_NOTICE
    };
    writeln!(out, "{mode_notice}").ok();
    notices.push(mode_notice.to_string());

    let ran_at = chrono::Utc::now().to_rfc3339();
    let debug_port = pick_loopback_port();
    let run_id = short_run_id(&ran_at, debug_port, req.machine);

    // A delivering run's writes need the collector's own directories, up
    // front, exactly where the backup import creates them. A dry run
    // creates nothing anywhere.
    if req.deliver {
        if let Err(error) =
            crate::store::assert_stage_writer_audited(crate::store::StageWriter::Collect)
        {
            let restore = close_out_restore(&mut req, None, None, app_was_running);
            let report = report_from(
                &req,
                (&run_id, debug_port),
                (&app_version_start, None),
                Scratch::default(),
                notices,
                restore,
            );
            return BackfillOutcome::Stopped(
                report,
                format!("the stage writer this run must use is not registered: {error:#}"),
            );
        }
        for dir in [
            req.state_dir.to_path_buf(),
            req.state_dir.join("grok-bot-unions"),
        ] {
            if let Err(error) = std::fs::create_dir_all(&dir) {
                let restore = close_out_restore(&mut req, None, None, app_was_running);
                let report = report_from(
                    &req,
                    (&run_id, debug_port),
                    (&app_version_start, None),
                    Scratch::default(),
                    notices,
                    restore,
                );
                return BackfillOutcome::Stopped(
                    report,
                    format!("the collector state could not be created: {error}"),
                );
            }
        }
    }

    // ---- The relaunch. From here to the end of the function, every exit
    // path flows into the close-out below.
    let relaunched_pid = match req.driver.relaunch_for_backfill(debug_port) {
        Ok(pid) => pid,
        Err(error) => {
            let restore = close_out_restore(&mut req, None, None, app_was_running);
            let report = report_from(
                &req,
                (&run_id, debug_port),
                (&app_version_start, None),
                Scratch::default(),
                notices,
                restore,
            );
            return BackfillOutcome::Stopped(report, error.to_string());
        }
    };
    let debug_anchor = Some((relaunched_pid, debug_port));

    let mut scratch = Scratch {
        previously_open: None,
        reports: Vec::new(),
        unmatched_prefixes: Vec::new(),
    };

    let prepared: std::result::Result<(RunContext, Vec<String>), TraverseStop> = prepare_traversal(
        &mut req,
        &mut scratch,
        &run_id,
        &ran_at,
        &app_version_start,
        &agent_names,
        &locals,
    );
    let traversal = match prepared {
        Ok((ctx, selected)) => traverse_all(&mut req, &mut scratch, ctx, selected),
        Err(stop) => TraversalEnd::Stop(stop),
    };

    // ---- Close-out: restore the app, verify the restore, report. The one
    // site every post-relaunch exit path flows through.
    let restore = close_out_restore(
        &mut req,
        scratch.previously_open.clone(),
        debug_anchor,
        app_was_running,
    );
    let app_version_end = req
        .driver
        .app_status()
        .ok()
        .and_then(|later| later.installed_version);
    let report = report_from(
        &req,
        (&run_id, debug_port),
        (&app_version_start, app_version_end),
        scratch,
        notices,
        restore,
    );

    // The traversal's own verdict decides the exit, and a run that did
    // not finish reading answers "did not finish" (exit 3) even when the
    // close-out failed on top of it — collapsing the two into exit 1's
    // "finished reading and failed" would turn "we do not know" into
    // "we checked". The restore's failure rides along in the stop text,
    // never dropped; a traversal that did finish keeps exit 1 for a
    // failed close-out exactly as before.
    match traversal {
        TraversalEnd::Stop(stop) => {
            let mut why = stop.describe();
            if let Some(failure) = report.restore.failure.clone() {
                let (debug_pid, debug_port) = (report.restore.debug_pid, report.restore.debug_port);
                why.push_str(&format!(
                    "; and the app could not be restored to a normal launch, or the \
                     close-out could not be verified: {failure} (the debug launch was \
                     pid {debug_pid:?} on port {debug_port})"
                ));
            }
            BackfillOutcome::Stopped(report, why)
        }
        TraversalEnd::Done => {
            if let Some(failure) = report.restore.failure.clone() {
                let (debug_pid, debug_port) = (report.restore.debug_pid, report.restore.debug_port);
                return BackfillOutcome::RestoreFailed(
                    report,
                    format!(
                        "the app could not be restored to a normal launch, or the \
                         close-out could not be verified: {failure} (the debug launch was \
                         pid {debug_pid:?} on port {debug_port})"
                    ),
                );
            }
            let mut findings = report
                .unmatched_prefixes
                .iter()
                .map(|prefix| format!("no conversation matches the prefix {prefix}"))
                .collect::<Vec<String>>();
            findings.extend(report.conversations.iter().filter_map(|item| {
                item.skipped
                    .as_ref()
                    .map(|skipped| format!("conversation {}: {skipped}", item.session_prefix))
            }));
            if findings.is_empty() {
                BackfillOutcome::Completed(report)
            } else {
                BackfillOutcome::CompletedWithFindings(report, findings)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn report_from(
    req: &BackfillRequest<'_>,
    (run_id, debug_port): (&str, u16),
    (app_version_start, app_version_end): (&Option<String>, Option<String>),
    scratch: Scratch,
    notices: Vec<String>,
    restore: RestoreOutcome,
) -> BackfillReport {
    BackfillReport {
        dry_run: !req.deliver,
        run_id: run_id.to_string(),
        app_version_start: app_version_start.clone(),
        app_version_end,
        debug_port,
        previously_open: scratch.previously_open,
        conversations: scratch.reports,
        unmatched_prefixes: scratch.unmatched_prefixes,
        restore,
        notices,
    }
}

impl Default for Scratch {
    fn default() -> Self {
        Self {
            previously_open: None,
            reports: Vec::new(),
            unmatched_prefixes: Vec::new(),
        }
    }
}

/// Wait for the relaunched app to be ready for its first sidebar read:
/// the app's boot continues past its debugging endpoint answering, and
/// the renderer presents an empty list until it has painted, so the
/// run polls until the sidebar lists at least one conversation before
/// anything is read. A sidebar still empty at the end of the bounded
/// wait is a whole-run stop said loudly — the preflight just proved the
/// local cache holds replicas, so an unpainted (or unpaintable) sidebar
/// is a boot that never finished or a shape this build cannot find the
/// conversations in, and either proves nothing about how many there
/// are. Zero is a measurement this run never reached, never a
/// fallback.
fn wait_for_populated_sidebar(
    req: &mut BackfillRequest<'_>,
) -> std::result::Result<Vec<UiConversation>, TraverseStop> {
    let deadline = req.clock.now_ms().saturating_add(SIDEBAR_READY_WAIT_MS);
    loop {
        if req.interrupt.load(Ordering::SeqCst) {
            return Err(TraverseStop::Interrupted);
        }
        match req.driver.list_conversations() {
            Ok(list) if !list.is_empty() => return Ok(list),
            Ok(_) => {}
            Err(DriverError::ShapeBreak(what)) => return Err(TraverseStop::Shape(what)),
            Err(DriverError::Fatal(error)) => return Err(TraverseStop::Fatal(error.to_string())),
        }
        if req.clock.now_ms() >= deadline {
            return Err(TraverseStop::Fatal(format!(
                "the relaunched app's sidebar listed no conversation within the \
                 {SIDEBAR_READY_WAIT_MS} ms readiness wait, so no conversation was opened \
                 or read; an empty sidebar there is an app that never finished \
                 presenting it (or whose conversations this build cannot find in its \
                 view state), never a measurement of zero conversations"
            )));
        }
        req.clock.sleep_ms(SIDEBAR_READY_POLL_INTERVAL_MS);
    }
}

/// The side of the traversal that runs before any conversation is opened:
/// wait for the relaunched app's readiness, record the previously-open
/// conversation, apply the `--conversation` prefixes, and load the
/// delivery state. Errors here are whole-run stops.
fn prepare_traversal(
    req: &mut BackfillRequest<'_>,
    scratch: &mut Scratch,
    run_id: &str,
    ran_at: &str,
    app_version_start: &Option<String>,
    agent_names: &BTreeMap<String, String>,
    locals: &BTreeMap<String, LocalConversation>,
) -> std::result::Result<(RunContext, Vec<String>), TraverseStop> {
    // Readiness before any of the app's state is read: an empty sidebar
    // during boot must not be taken for the app's answer (the
    // previously-open conversation is read only after it, for the same
    // reason — the re-selection the close-out owes is only meaningful
    // once the app's own selection is visible).
    let conversations = wait_for_populated_sidebar(req)?;
    scratch.previously_open = match req.driver.currently_open() {
        Ok(open) => open,
        Err(DriverError::ShapeBreak(what)) => return Err(TraverseStop::Shape(what)),
        Err(DriverError::Fatal(error)) => return Err(TraverseStop::Fatal(error.to_string())),
    };

    let conversation_agent_ids: Vec<String> = conversations
        .into_iter()
        .map(|item| item.agent_id)
        .collect();
    if conversation_agent_ids
        .iter()
        .any(|id| id.split('-').count() != 5)
    {
        return Err(TraverseStop::Shape(
            "the sidebar does not name its conversations by agent uuids".to_string(),
        ));
    }

    // `--conversation` restricts the run; a prefix that matches nothing is
    // reported, never silently dropped.
    let mut selected: Vec<String> = conversation_agent_ids.clone();
    if !req.conversation_prefixes.is_empty() {
        selected = req
            .conversation_prefixes
            .iter()
            .flat_map(|prefix| {
                let trimmed = prefix.trim();
                let hits: Vec<String> = conversation_agent_ids
                    .iter()
                    .filter(|agent| {
                        session_id_for(req.machine, agent).starts_with(trimmed)
                            || agent.starts_with(trimmed)
                    })
                    .cloned()
                    .collect();
                if hits.is_empty() {
                    scratch.unmatched_prefixes.push(prefix.clone());
                }
                hits
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if selected.is_empty() {
            // Everything the user named is missing: traversal over nothing,
            // reported honestly rather than guessed at.
            let state_path = req.state_dir.join(collector::STATE_FILE);
            let ctx = RunContext {
                run_id: run_id.to_string(),
                ran_at: ran_at.to_string(),
                app_version_start: app_version_start.clone(),
                agent_names: agent_names.clone(),
                locals: locals.clone(),
                state_path,
                state: collector::DebtState::default(),
                destination_id: String::new(),
                debts: BTreeMap::new(),
                remote_facts: None,
                manifest_facts: None,
            };
            return Ok((ctx, Vec::new()));
        }
    }

    // The delivery state: the same load-and-migrate a collector pass
    // performs, the same one-shot remote-facts resolution the backup import
    // performs.
    let state_path = req.state_dir.join(collector::STATE_FILE);
    let state = collector::load_state(&state_path).map_err(|error| {
        TraverseStop::Fatal(format!("the collector state could not be read: {error:#}"))
    })?;
    let destination_id = req.destination.id().to_string();
    let mut debts = state
        .destinations
        .get(&destination_id)
        .cloned()
        // reason: a backfill against a destination never collected for
        // starts with no delivered rows, which is that destination's truth
        .unwrap_or_default()
        .files;
    collector::migrate_grok_bot_debts(&mut debts);

    let manifest_facts = if req.deliver {
        collector::load_local_manifest_facts(req.stage, req.machine)
    } else {
        None
    };
    let mut wanted: BTreeSet<(String, String)> = BTreeSet::new();
    for agent in &selected {
        let session_id = session_id_for(req.machine, agent);
        if let Some(entry) = debts.get(&collector::grok_bot_session_key(&session_id)) {
            match collector::debt_settled_locally(
                entry,
                req.machine,
                req.stage,
                manifest_facts.as_ref(),
            ) {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    wanted.insert((req.machine.to_string(), session_id));
                }
            }
        }
    }
    // `facts()` caches on the view; a delivery made through it clones the
    // answer out so the borrow ends here, not across the whole traversal.
    let remote_facts: Option<collector::ArchiveFacts> = req.destination.facts(&wanted).cloned();

    let ctx = RunContext {
        run_id: run_id.to_string(),
        ran_at: ran_at.to_string(),
        app_version_start: app_version_start.clone(),
        agent_names: agent_names.clone(),
        locals: locals.clone(),
        state_path,
        state,
        destination_id,
        debts,
        remote_facts,
        manifest_facts,
    };
    Ok((ctx, selected))
}

fn session_id_for(machine: &str, agent: &str) -> String {
    crate::id::SessionIdentity {
        source_short: HarnessSource::GrokBot.short(),
        machine: machine.to_string(),
        native_id: agent.to_string(),
    }
    .id()
}

/// The traversal itself: one conversation at a time, ≥ 10 s apart, in both
/// run kinds. A single-conversation blemish is reported and the run
/// continues; a shape break or dead transport stops it.
fn traverse_all(
    req: &mut BackfillRequest<'_>,
    scratch: &mut Scratch,
    mut ctx: RunContext,
    selected: Vec<String>,
) -> TraversalEnd {
    let mut last_finish_ms: Option<u64> = None;
    for agent in &selected {
        if req.interrupt.load(Ordering::SeqCst) {
            return TraversalEnd::Stop(TraverseStop::Interrupted);
        }
        if let Some(finish) = last_finish_ms {
            pace_between(&req.clock, req.interrupt, finish);
            if req.interrupt.load(Ordering::SeqCst) {
                return TraversalEnd::Stop(TraverseStop::Interrupted);
            }
        }
        let opened_at_ms = req.clock.now_ms();
        let session_id = session_id_for(req.machine, agent);
        let prefix = crate::id::short_session_id(&session_id);
        let local = ctx.locals.get(agent);

        if let Err(error) = req.driver.open_conversation(agent) {
            match error {
                DriverError::ShapeBreak(what) => {
                    return TraversalEnd::Stop(TraverseStop::Shape(what));
                }
                DriverError::Fatal(error) => {
                    return TraversalEnd::Stop(TraverseStop::Fatal(error.to_string()));
                }
            }
        }

        let walked = wait_for_loaded_rows(req, agent);
        let finished_at_ms = req.clock.now_ms();
        let (rows, skipped) = match walked {
            Walk::Rows(rows) => (Some(rows), None),
            Walk::Skipped(reason) => (None, Some(reason)),
            Walk::Stop(stop) => return TraversalEnd::Stop(stop),
        };

        let mut conversation = BackfillConversation {
            session_prefix: prefix.clone(),
            new_to_archive: false,
            rows_delivered: 0,
            rows_already_delivered: 0,
            loaded_min_seq: None,
            loaded_max_seq: None,
            local_min_seq: local.and_then(|local| local.min_seq),
            local_max_seq: local.and_then(|local| local.max_seq),
            withheld_positions: None,
            skipped: None,
            opened_at_ms,
            finished_at_ms,
        };
        match rows {
            Some(rows) => {
                let loaded_seqs: Vec<u64> = rows.iter().map(|item| item.sequence).collect();
                conversation.loaded_min_seq = loaded_seqs.iter().min().copied();
                conversation.loaded_max_seq = loaded_seqs.iter().max().copied();
                let withheld = crate::grok_bot::sequence_gaps(loaded_seqs.iter().copied());
                // The load reached sequence 1 (the walk only returns fully
                // loaded rows), so the withheld count is provable: these
                // are the positions the service itself never sends.
                conversation.withheld_positions = Some(withheld.len() as u64);
                match deliver_one(req, &mut ctx, agent, &session_id, rows) {
                    Ok(outcome) => {
                        conversation.new_to_archive = outcome.new_to_archive;
                        conversation.rows_delivered = outcome.rows_delivered;
                        conversation.rows_already_delivered = outcome.rows_already_delivered;
                    }
                    Err(error) => {
                        conversation.skipped = Some(SkipReason::Failed(error));
                    }
                }
            }
            None => {
                conversation.skipped = Some(skipped.expect("rows or skipped, never both"));
            }
        }
        scratch.reports.push(conversation);
        last_finish_ms = Some(req.clock.now_ms());
    }
    let _ = &mut ctx;
    TraversalEnd::Done
}

/// Wait until the app's own loading of the open conversation goes quiet and
/// sequence 1 is in view — the conversation's floor, since replica
/// sequences are numbered from one — then harvest the whole loaded row set.
///
/// A load that never proves its floor within its bounded wait is given one
/// up-scroll nudge (the app's own older-page pager) and a second wait; a
/// conversation still not there afterwards is skipped whole, nothing
/// partial or uncertain taken from it.
fn wait_for_loaded_rows(req: &mut BackfillRequest<'_>, agent: &str) -> Walk {
    let harvest = |req: &mut BackfillRequest<'_>, rows: RowsSnapshot| -> Walk {
        let harvested = rows
            .rows
            .into_iter()
            .map(crate::grok_bot::ReplicaRecord::from_value)
            .collect::<std::result::Result<Vec<_>, _>>();
        match harvested {
            // A loaded row the reader's own record model refuses is a
            // shape break: the run stops loudly rather than guessing what
            // the app's rows mean.
            Ok(rows) => Walk::Rows(rows),
            Err(error) => Walk::Stop(TraverseStop::Shape(format!(
                "the conversation {} did not answer with rows the replica-record \
                 model accepts: {error}",
                crate::id::short_session_id(&session_id_for(req.machine, agent))
            ))),
        }
    };

    let first_deadline = req.clock.now_ms() + LOAD_QUIESCENCE_WAIT_MS;
    let stable = match read_until_stable(req, first_deadline) {
        Err(stop) => return Walk::Stop(stop),
        Ok(None) => None,
        Ok(Some(snapshot)) => Some(snapshot),
    };
    let floor_proved = matches!(&stable, Some(snapshot) if
        snapshot.rows.iter().filter_map(views_seq).min() == Some(1));
    if floor_proved {
        let snapshot = stable.expect("checked by the match guard");
        return harvest(req, snapshot);
    }

    // Either the span never went quiet or the floor never proved: nudge the
    // app's own pager once, then wait a second bounded window.
    if let Err(error) = req.driver.nudge_older_pages() {
        match error {
            DriverError::ShapeBreak(what) => return Walk::Stop(TraverseStop::Shape(what)),
            DriverError::Fatal(error) => return Walk::Stop(TraverseStop::Fatal(error.to_string())),
        }
    }
    let second_deadline = req.clock.now_ms() + LOAD_QUIESCENCE_WAIT_MS;
    let retry = match read_until_stable(req, second_deadline) {
        Err(stop) => return Walk::Stop(stop),
        Ok(None) => return Walk::Skipped(SkipReason::NotFullyLoaded),
        Ok(Some(snapshot)) => snapshot,
    };
    if retry.rows.iter().filter_map(views_seq).min() == Some(1) {
        return harvest(req, retry);
    }
    Walk::Skipped(SkipReason::NotFullyLoaded)
}

fn views_seq(row: &Value) -> Option<u64> {
    row.get("seq").and_then(Value::as_u64)
}

/// Poll until the app-visible span stops changing between consecutive reads
/// (the loading is quiet), answering the stable snapshot. `Ok(None)` = the
/// bounded wait ended with the span still moving or with nothing readable.
fn read_until_stable(
    req: &mut BackfillRequest<'_>,
    deadline_ms: u64,
) -> std::result::Result<Option<RowsSnapshot>, TraverseStop> {
    let mut last: Option<(usize, Option<u64>, Option<u64>)> = None;
    loop {
        if req.interrupt.load(Ordering::SeqCst) {
            return Err(TraverseStop::Interrupted);
        }
        let snapshot = match req.driver.read_rows() {
            Ok(snapshot) => snapshot,
            Err(DriverError::ShapeBreak(what)) => return Err(TraverseStop::Shape(what)),
            Err(DriverError::Fatal(error)) => return Err(TraverseStop::Fatal(error.to_string())),
        };
        let shape = (
            snapshot.rows.len(),
            snapshot.rows.iter().filter_map(views_seq).min(),
            snapshot.rows.iter().filter_map(views_seq).max(),
        );
        let repeated = last == Some(shape);
        last = Some(shape);
        // An "open conversation holds no rows at all" reading never counts
        // as a stable load: the app opens a loaded conversation, so an
        // empty read is the layout or the connection, not the data.
        if repeated && !snapshot.rows.is_empty() {
            return Ok(Some(snapshot));
        }
        if req.clock.now_ms() >= deadline_ms {
            return Ok(None);
        }
        req.clock.sleep_ms(LOAD_POLL_INTERVAL_MS);
    }
}

/// ≥ 10 s of measured wall clock between one conversation's end and the
/// next open, sliced so an interrupt stays responsive.
fn pace_between(clock: &Clock, interrupt: &AtomicBool, last_finish_ms: u64) {
    let target = last_finish_ms.saturating_add(PACING_MIN_GAP_MS);
    while clock.now_ms() < target && !interrupt.load(Ordering::SeqCst) {
        let remaining = target - clock.now_ms();
        clock.sleep_ms(remaining.min(500));
    }
}

/// Plan and (in a real run) deliver one conversation's harvested rows
/// through the collector's own delivered-set state — the same sequence of
/// verifications and writes the backup import performs per item.
fn deliver_one(
    req: &mut BackfillRequest<'_>,
    ctx: &mut RunContext,
    agent: &str,
    session_id: &str,
    harvested: Vec<crate::grok_bot::ReplicaRecord>,
) -> std::result::Result<DeliveryOutcome, String> {
    let key = collector::grok_bot_session_key(session_id);
    let stored = ctx.debts.get(&key).cloned();
    let mut unverifiable = None;
    if let Some(entry) = stored.as_ref() {
        match collector::verify_debt(
            entry,
            req.machine,
            req.stage,
            ctx.remote_facts.as_ref(),
            ctx.manifest_facts.as_ref(),
        ) {
            Ok(collector::DebtVerdict::OwedOnStage)
            | Ok(collector::DebtVerdict::SettledInArchive)
            | Ok(collector::DebtVerdict::SettledReclaimed) => {}
            Ok(collector::DebtVerdict::Unverifiable(reason)) => unverifiable = Some(reason),
            Err(error) => {
                return Err(format!(
                    "the stored delivered state could not be verified: {error:#}"
                ))
            }
        }
    }
    let old = stored.as_ref().map(|entry| entry.cursor.clone());
    let delivered_before: BTreeSet<String> = old
        .as_ref()
        .and_then(|entry| entry.grok_bot_row_digests.as_ref())
        .into_iter()
        .flatten()
        .cloned()
        .collect();

    // Rows the app loaded that the session had already delivered — counted
    // over this harvest, not over the union (the union also holds rows a
    // backup import brought; those were not loaded by the app this run).
    let mut loaded_digests = BTreeSet::new();
    for record in &harvested {
        let digest = collector::grok_bot_row_digest(record)
            .map_err(|error| format!("a row could not be measured: {error:#}"))?;
        loaded_digests.insert(digest);
    }
    let rows_already = loaded_digests.intersection(&delivered_before).count() as u64;
    let new_to_archive = old.is_none();

    // The union merge: the session's shared observation memory gains the
    // rows this harvest brought, the same append-only way a live pass or a
    // backup import merges.
    let union_path = collector::grok_bot_union_path(req.state_dir, agent);
    let previous = collector::load_grok_bot_union(&union_path)
        .map_err(|error| format!("the session's union state could not be read: {error:#}"))?;
    let union = crate::grok_bot::merge_replica_records(previous, harvested);

    let deliver_all = unverifiable.is_some()
        || old.is_none()
        || old
            .as_ref()
            .is_some_and(|entry| entry.grok_bot_row_digests.is_none());
    let planned = collector::plan_grok_bot_rows(&union, &delivered_before, deliver_all)
        .map_err(|error| format!("the delivery could not be planned: {error:#}"))?;
    let (to_deliver, delivered_now) = planned;
    let rows_new = to_deliver.len() as u64;

    if !req.deliver {
        return Ok(DeliveryOutcome {
            new_to_archive,
            rows_delivered: rows_new,
            rows_already_delivered: rows_already,
        });
    }

    // The live unchanged rule, verbatim from the collector and the import:
    // a debt that proved itself plus a plan with nothing new writes
    // nothing at all — byte-identical state.
    let nothing_new = rows_new == 0 && old.is_some() && unverifiable.is_none();
    if nothing_new {
        if let Some(source) = ctx
            .locals
            .get(agent)
            .and_then(|local| local.source_path.clone())
        {
            drop(crate::activity::record_session_provenance(
                req.stage,
                req.machine,
                session_id,
                HarnessSource::GrokBot,
                &source,
            ));
        }
        return Ok(DeliveryOutcome {
            new_to_archive,
            rows_delivered: 0,
            rows_already_delivered: rows_already,
        });
    }

    let mut payload = Vec::new();
    for item in &to_deliver {
        serde_json::to_writer(&mut payload, &item.record.raw)
            .map_err(|error| format!("a row could not be serialized: {error}"))?;
        payload.push(b'\n');
    }
    // Observation scale of this harvest: the serialized rows the app
    // itself loaded (the backfill has no blob to stat — the collect and
    // import paths use the replica file's size for the same field).
    // reason: prefix_len is scale-of-observation bookkeeping; zero would
    // understate a delivery whose whole point is observation.
    let mut source_bytes: u64 = 0;
    for record in &union {
        let bytes = serde_json::to_vec(&record.raw)
            .map_err(|error| format!("a row could not be measured: {error}"))?;
        source_bytes += bytes.len() as u64;
    }
    let provenance =
        collector::GrokBotPassProvenance::Backfill(collector::GrokBotBackfillProvenance {
            run_id: ctx.run_id.clone(),
            app_version: ctx.app_version_start.clone(),
            ran_at: ctx.ran_at.clone(),
        });
    let metadata = collector::grok_bot_delivery_metadata(
        ctx.agent_names.get(agent).map(String::as_str),
        &union,
        Some(&provenance),
    );
    serde_json::to_writer(&mut payload, &metadata)
        .map_err(|error| format!("the measurement line could not be serialized: {error}"))?;
    payload.push(b'\n');

    crate::store::write_sealed_shard_bytes_with_cap(
        crate::store::StageWriter::Collect,
        req.stage,
        req.machine,
        session_id,
        &[payload],
        req.shard_bucket_cap,
    )
    .map_err(|error| format!("the shard could not be sealed: {error:#}"))?;
    collector::write_grok_bot_union(&union_path, union.clone())
        .map_err(|error| format!("the union state could not be written: {error:#}"))?;
    let shards = collector::stage_shard_fact(req.stage, req.machine, session_id)
        .map_err(|error| format!("the sealed shard could not be re-read: {error:#}"))?;
    let cursor = collector::grok_bot_delivery_cursor(source_bytes, &union, delivered_now)
        .map_err(|error| format!("the delivery cursor could not be built: {error:#}"))?;
    let entry = collector::DebtEntry {
        machine: req.machine.to_string(),
        session_id: session_id.to_string(),
        cursor,
        shards,
    };
    if stored.as_ref() != Some(&entry) {
        ctx.debts.insert(key, entry);
        collector::save_state(&ctx.state_path, &ctx.state, &ctx.destination_id, &ctx.debts)
            .map_err(|error| format!("the collector state could not be saved: {error:#}"))?;
    }
    let source = ctx
        .locals
        .get(agent)
        .and_then(|local| local.source_path.clone())
        .unwrap_or_else(|| req.persistence_root.to_path_buf());
    crate::activity::record_session_provenance(
        req.stage,
        req.machine,
        session_id,
        HarnessSource::GrokBot,
        &source,
    )
    .map_err(|error| format!("the provenance row could not be written: {error:#}"))?;

    Ok(DeliveryOutcome {
        new_to_archive,
        rows_delivered: rows_new,
        rows_already_delivered: rows_already,
    })
}

/// Restore the app and verify the restore — the close-out's single entry
/// point. `debug` carries the relaunched pid and port when there was a
/// relaunch; without it the driver decides what restore means (usually
/// nothing: nothing was ever quit). `app_was_running` is the run's own
/// preflight fact of whether an app was running before anything was
/// touched — what decides whether the restore owes a plain relaunch,
/// read from the driver's report rather than trusted to it.
fn close_out_restore(
    req: &mut BackfillRequest<'_>,
    previously_open: Option<String>,
    debug: Option<(u32, u16)>,
    app_was_running: bool,
) -> RestoreOutcome {
    let mut outcome = match req.driver.restore_normal_launch(previously_open.as_deref()) {
        Ok(outcome) => outcome,
        Err(error) => RestoreOutcome {
            attempted: true,
            debug_pid: debug.map(|(pid, _)| pid),
            // reason: a restore that failed before a relaunch was recorded
            // reports no port — 0 is the report's honest "no port" and no
            // live port could ever answer under it
            debug_port: debug.map(|(_, port)| port).unwrap_or_default(),
            port_closed: None,
            sync_resumed: None,
            relaunch_owed: app_was_running,
            relaunch_running: None,
            conversation_reselected: None,
            failure: Some(error.to_string()),
        },
    };
    // The owed flag is the run's own preflight reading, not the driver's
    // to claim: how the restore answered is the driver's to report, what
    // the machine owed it is measured before anything was touched.
    outcome.relaunch_owed = app_was_running;
    if !outcome.attempted {
        return outcome;
    }
    if let Some((pid, port)) = debug {
        outcome.debug_pid = outcome.debug_pid.or(Some(pid));
        // reason: a driver that reported no port saw no relaunch happen on
        // its side; 0 is the honest "no port" in the report, never a port
        // a user could mistake for one
        outcome.debug_port = outcome.debug_port.max(port);
    }
    if outcome.failure.is_some() {
        return outcome;
    }
    if outcome.port_closed != Some(true) {
        let why = if outcome.port_closed == Some(false) {
            "something still listens on the debug port".to_string()
        } else {
            "whether the debug port closed could not be verified".to_string()
        };
        outcome.failure = Some(why);
        return outcome;
    }
    // The plain relaunch the close-out owes: it must have come up and
    // stayed up through its settle window — an instance that dies on the
    // just-quit instance's single-instance lock is the quietest way a
    // machine can end up with no app at all, so it is verified, never
    // assumed. `Some(false)` is a measurement of a dead relaunch;
    // `None` is an unobservation; only the owed relaunch is judged, and
    // the driver's own, more specific failure text (when it wrote one)
    // is kept ahead of these generic verdicts.
    if outcome.relaunch_owed && outcome.relaunch_running != Some(true) {
        outcome.failure.get_or_insert_with(|| {
            if outcome.relaunch_running == Some(false) {
                "the relaunched app did not stay running".to_string()
            } else {
                "whether the relaunched app stayed running could not be \
                 verified"
                    .to_string()
            }
        });
        return outcome;
    }
    // The sync check: the app's own persistence writes resumed within the
    // bounded wait — the probe's own close-out criterion. It is owed
    // exactly when the restore ended with the app running, which is what
    // the relaunch verification above just proved; not observed is
    // reported as not observed, never assumed.
    if outcome.relaunch_running == Some(true) {
        let start = req.clock.now_ms();
        let before = req.driver.sync_evidence_ms().ok().flatten();
        let mut resumed = None;
        loop {
            if req.clock.now_ms().saturating_sub(start) > SYNC_RESUME_WAIT_MS {
                break;
            }
            req.clock.sleep_ms(1000.max(1));
            if let Some(later) = req.driver.sync_evidence_ms().ok().flatten() {
                if before.is_some_and(|earlier| later > earlier) {
                    resumed = Some(true);
                    break;
                }
            }
        }
        outcome.sync_resumed = resumed;
        if resumed != Some(true) {
            outcome.failure.get_or_insert_with(|| {
                "the app's own sync did not resume within the bounded wait".to_string()
            });
        }
    }
    outcome
}

/// Pick a free loopback port for the debug relaunch: bind port 0, read the
/// port, close. The app races onto the port moments later.
fn pick_loopback_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        // reason: a loopback bind fails only from exhaustion or a broken
        // machine, and the relaunch that follows fails loudly on a port
        // the app cannot take; 0 is never usable, which is the point
        .unwrap_or_default()
}

/// Short run id: a digest of the run's own birthday, port and machine —
/// counts and prefixes only, no conversation data, no paths.
fn short_run_id(ran_at: &str, port: u16, machine: &str) -> String {
    use sha2::Digest;
    let material = format!("grok-bot-backfill:{ran_at}:{port}:{machine}");
    let digest = sha2::Sha256::digest(material.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// The production CDP driver: one file, macOS-shaped, compiled everywhere
// CI compiles the crate, reached only through the trait above.
#[path = "backfill_cdp.rs"]
mod cdp;

pub use cdp::{CdpAppDriver, OsCommands};
