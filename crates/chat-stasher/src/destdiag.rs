//! destdiag — the four-fact destination reconciliation (W1012 / STAGE-GROWTH
//! §5.3 C1, its four-way reconciliation: one function, three display
//! surfaces).
//!
//! A machine can declare destinations and still never back up to one of
//! them: the timer file never names it, or names it but cannot resolve the
//! credentials it would need, and the only place the failure surfaces is a
//! log nobody reads (46-STAGE-GROWTH §2's finding — weeks of blocked
//! reclaims, a green `status`, and a shrinking disk). This module is the
//! single function that turns a machine's local, read-only facts into a
//! per-destination verdict, so `status`, `doctor` and the menu bar app all
//! report it *in the same words* from the same computation.
//!
//! For every declared destination, four facts (§5.3 C1 ①-④):
//!
//! 1. **Push coverage** — does any installed scheduler timer run a push
//!    that names this destination? (Read from the installed unit files,
//!    [ `schedule::installed_run_once_units`].)
//! 2. **Credentials in the timer environment** — the destination's options
//!    read as `env:NAME` resolve only in a process that inherited a login
//!    shell. A launchd/systemd job *is* the environment the scheduled run
//!    sees, so the fact is decided against what the timer files themselves
//!    declare, never against the shell the interactive user happens to
//!    have (`schedule.rs` adds no environment to its units today; a
//!    destination whose credentials are shell-only cannot be pushed nor
//!    consulted by any scheduled run).
//! 3. **Last successful push** — from the durable per-destination record
//!    [`crate::pushstate`] keeps. No record is *absence*, worded as such:
//!    this CLI started keeping the record with a version number that names
//!    itself, and an older push is invisible, not disproven.
//! 4. **Reclaim share** — whether the weekly stage reclamation is blocked
//!    by this destination, and how much it holds back, from the durable
//!    record [`crate::reclaimstate`] keeps (the alert covers the
//!    silent-failure shape the log cannot: a job that never starts).
//!
//! Everything here is read-only and local: no destination is dialed, no
//! config is written, no session content or credential value is read. The
//! raw `env:NAME` references come from a second read of the config file's
//! own text — the loaded [`Config`] already replaced or dropped them, and
//! a reconciliation that read it back would see only the disappearance.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

use crate::config::Config;
use crate::schedule::{self, Format};

/// The severity a reconciliation alert carries — the two colors §5.3 C1
/// allows: any fact that is not right shows up yellow or red.
///
/// The mapping is judgment expressed in one place: a fact that costs data
/// *now* (a reclaim already blocked, a job not running) is red; a fact
/// that will cost data the first time it matters (a destination no timer
/// pushes yet, credentials a timer could not resolve) is yellow, so the
/// panel reflects what is already going wrong and not everything that
/// could.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Something is already being lost: back up held, disk growing.
    Red,
    /// A silent gap that will bite at the next failure.
    Yellow,
}

impl Severity {
    /// The stable token `status --json` and the menu bar app read. Letters,
    /// not emoji, so a wrapper can compare it without reproducing a display
    /// convention.
    pub fn slug(self) -> &'static str {
        match self {
            Severity::Red => "red",
            Severity::Yellow => "yellow",
        }
    }
}

/// One finding a user can act on: the sentence (already worded — the one
/// wording all three surfaces display is this module's reason to exist)
/// plus the fix, if the fix has a command shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub severity: Severity,
    /// `[dest]` / `[reclaim]` prefix plus the human sentence; one wording,
    /// shared by status prose, doctor and the menu bar app via `--json`.
    pub sentence: String,
    /// The command that fixes it, when one exists; prose-only fixes carry
    /// no command (the sentence already says what to do).
    pub fix: Option<String>,
}

/// How many consecutive blocked reclaim attempts turn the reclaim alert red
/// and raise the menu bar's system notification (C2): two in a row is the
/// first moment the failure is a pattern rather than a bad week.
pub const BLOCKED_STREAK_ALARM: u32 = 2;

/// ① Whether any installed scheduler timer pushes this destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushCoverage {
    /// These installed unit files run a push naming this destination.
    Covered { units: Vec<String> },
    /// Declared, and no installed unit names it: a push reaches this
    /// destination only when a command names one by hand.
    NoScheduledPush,
    /// A relevant unit file exists but could not be read, so the question
    /// has no answer here — which is not the same claim as "no timer".
    /// This is a local anomaly the user can act on, so it is worded as a
    /// `[dest]` alert (see `word_alerts`).
    Unknown { why: String },
    /// This build has no reader for this platform's scheduler at all, so
    /// the question has no answer here — every machine on that platform
    /// gets this, it is nothing the user can fix, and it therefore stays
    /// quiet in the alerts the way [`ReclaimShare::UnknownRun`] does. The
    /// unknown is still visible, worded, in `doctor`'s table and `--json`.
    UnreadablePlatform { why: String },
}

/// ② Whether the destination's credential references resolve where a
/// scheduled job would actually use them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialResolution {
    /// No `env:NAME` references: every credential form in use
    /// (`file:`, `env-file:`, `keychain:`, literals) resolves without a
    /// login shell, in a terminal and in a timer alike.
    ShellIndependent,
    /// References exist and every timer that contacts this destination
    /// declares them in its own file.
    ResolvableInTimer { units: Vec<String> },
    /// References exist and at least one contacting timer declares none of
    /// them — a scheduled run of that unit drops the credentials and cannot
    /// authenticate. The unit list names exactly which files.
    NotResolvableInTimer {
        refs: Vec<String>,
        units: Vec<String>,
    },
    /// References exist and no installed timer contacts this destination at
    /// all. The fact that matters first is push coverage (none); this state
    /// exists so the table can still say the credentials are shell-only
    /// without inventing a timer that would fail.
    ShellOnlyWithNoTimer,
    /// The config file's own text could not be re-read, so its references
    /// are unknown — an unknown, not "none". A local anomaly the user can
    /// act on, so it is worded as a `[dest]` alert (see `word_alerts`).
    Unknown { why: String },
    /// The config declares shell-only references, and this build has no
    /// reader for this platform's scheduler at all, so whether a timer
    /// would resolve them has no answer here. Like ①'s
    /// [`PushCoverage::UnreadablePlatform`], it is a platform fact rather
    /// than a finding, and stays quiet in the alerts.
    UnreadablePlatform { why: String },
}

/// ③ When a push last succeeded against this destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastPush {
    /// The durable record's timestamp and snapshot count for this
    /// destination.
    At {
        finished_at_unix: u64,
        snapshots_in_repo: u64,
    },
    /// No record exists. Worded as absence, never as "never pushed": a
    /// machine that pushed before this CLI kept records — or from another
    /// process shape — leaves the same empty file behind.
    NoRecord,
    /// The record file exists but could not be read — malformed, or a
    /// version this build does not know. That is an unknown about what it
    /// says, which is not the same claim as [`LastPush::NoRecord`]: an
    /// absence read from a file that answered is a measurement, and one
    /// read out of a failed parse is not. The ③ alert stays quiet for it
    /// the way an unanswerable question does, and the fact keeps its
    /// unknown shape in `doctor`'s table and `--json`.
    Unknown { why: String },
}

/// ④ What the latest recorded reclaim attempt says about this
/// destination, with ADR-060 D7's third visible arm: a destination the run
/// classified degraded was not asked to prove anything — the record keeps
/// that as its own state, distinct from both "answered and fine"
/// ([`NotBlocking`]) and "answered and blocked" ([`Blocking`]), because a
/// state the user must see may not fold into one of the other two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReclaimShare {
    /// Nothing blocked the reclaim on this destination's account: either
    /// the attempt proved every session or the destination was not among
    /// the blocked.
    NotBlocking,
    /// The destination could not be consulted / does not hold / mismatches,
    /// holding this much back.
    Blocking {
        /// The block-kind word `reclaim-stage` printed (unreachable,
        /// partial-read, not-held, triple-mismatch).
        kind: String,
        /// The ADR-023 classifier's slug, when the failure produced one.
        error_kind: Option<String>,
        sessions: usize,
        bytes: u64,
    },
    /// The destination was degraded that run (ADR-060 D2: lagging, or no
    /// push on record), so the proof never asked it anything. `why` is the
    /// health state's own word (`lagging` / `never-pushed`); it is not a
    /// block — protection for that destination is the alarm and the union
    /// backfill, never stage retention.
    DegradedNotConsulted { health: String },
    /// The latest attempt ended before consulting destinations.
    UnknownRun { outcome: String },
}

/// ADR-060 D2's four reportable destination-health states — every
/// destination is judged against its *own* effective degrade-after
/// (per-destination override, else the global `[reclaim]` value, else the
/// default), and the answer is derived from the durable push record every
/// time it is read, never stored: health heals automatically with the next
/// successful push.
///
/// This is the one function-owning type both the reclaim and the three
/// surfaces consult (`classify_destination_health` below): `reclaim-stage`,
/// `status`, `doctor` and the menu bar can never disagree about health,
/// the shared-wording half of C1 applied to health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestinationHealth {
    /// Last successful push within the destination's effective
    /// degrade-after: it participates in the proof (must-prove).
    Healthy,
    /// Last successful push older than the effective degrade-after: it is
    /// assumed to be missing the recent increment era, is not consulted,
    /// and alarms every run.
    Lagging,
    /// No push record on this machine. Worded as absence in prose ("no
    /// push on record", never "never pushed"); the `--json` slug *is*
    /// `never-pushed` (review ruling, 2026-10-22). A machine that pushed before
    /// this CLI kept records leaves the same absence.
    NeverPushed,
    /// The record exists but is unreadable (malformed, or a version this
    /// build does not know). Fail-safe: must-prove — an unreadable record
    /// may never become deletion permission (the unknown-never-empty
    /// invariant), so it can only block, never be skipped.
    Unknown { why: String },
}

impl DestinationHealth {
    /// The `--json` / durable-record vocabulary, D2's four states verbatim
    /// (`healthy` / `lagging` / `never-pushed` / `unknown`).
    pub fn slug(&self) -> &'static str {
        match self {
            DestinationHealth::Healthy => "healthy",
            DestinationHealth::Lagging => "lagging",
            DestinationHealth::NeverPushed => "never-pushed",
            DestinationHealth::Unknown { .. } => "unknown",
        }
    }

    /// Whether the destination is past its effective degrade-after in the
    /// two time-derived senses D2 names degraded: not consulted by the
    /// proof, listed and alarmed on every run. `unknown` is *not* degraded
    /// — it is must-prove.
    pub fn is_degraded(&self) -> bool {
        matches!(
            self,
            DestinationHealth::Lagging | DestinationHealth::NeverPushed
        )
    }

    /// D7's proof-role word: `must-prove` for the set the reclaim consults
    /// (healthy, plus unknown on the fail-safe side), and
    /// `degraded-not-consulted` for the two degraded states.
    pub fn proof_role_word(&self) -> &'static str {
        if self.is_degraded() {
            "degraded-not-consulted"
        } else {
            "must-prove"
        }
    }
}

/// The default degrade-after (ADR-060 D2/D5): 7 days, the weekly reclaim
/// cadence — long enough that a network blip cannot out-age a healthy
/// destination (degradation is time-based, never reachability-based),
/// short enough that a misconfigured machine's stage does not quietly grow
/// forever. Configurable: `[reclaim] degrade_after_days`, plus
/// `[destinations.<name>] degrade_after_days` per destination.
pub const DEFAULT_DEGRADE_AFTER_DAYS: u64 = 7;

/// The default stage cap (ADR-060 D5): 10 GiB. An over-cap stage is a
/// **red alarm through the SG-1 channel and nothing else** — the cap never
/// stops collection and never triggers a reclaim of its own.
/// Configurable: `[reclaim] stage_cap_bytes` with `cache.max_bytes`'s dual
/// spelling (`"10GB"` = 10⁹, `"10GiB"` = 2³⁰).
pub const DEFAULT_STAGE_CAP_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// The destination's effective degrade-after (ADR-060 D2 + review ruling A1): the
/// per-destination override when the destination declares one, else the
/// global `[reclaim] degrade_after_days`, else the default. The override
/// never removes protection — it only lets "how behind is behind" follow
/// the destination's declared rhythm; past that rhythm the destination
/// degrades and alarms exactly like the global rule.
pub fn effective_degrade_after_days(global: Option<u64>, per_destination: Option<u64>) -> u64 {
    per_destination
        .or(global)
        // reason: absence of both keys is the documented ADR-060 default
        // (7 days = the weekly cadence), a configured default rather than
        // an unknown turned into a number — the same honesty as
        // body_cache's DEFAULT_MAX_BYTES for an absent [cache] quota.
        .unwrap_or(DEFAULT_DEGRADE_AFTER_DAYS)
}

/// ADR-060 D2's classification, one function parameterised by `now` (the
/// destdiag testability rule: never a clock read inside the computation).
/// Both `reclaim-stage` and the three surfaces call this same function.
///
/// Degrades at *strictly older* than the threshold: a push exactly on the
/// boundary is within its window, and health heals with the next successful
/// push because the state is derived, never latched.
pub fn classify_destination_health(
    last_push: &LastPush,
    degrade_after_days: u64,
    now_unix: u64,
) -> DestinationHealth {
    match last_push {
        LastPush::At {
            finished_at_unix, ..
        } => {
            let age_secs = now_unix.saturating_sub(*finished_at_unix);
            let threshold_secs = degrade_after_days.saturating_mul(86_400);
            if age_secs <= threshold_secs {
                DestinationHealth::Healthy
            } else {
                DestinationHealth::Lagging
            }
        }
        LastPush::NoRecord => DestinationHealth::NeverPushed,
        LastPush::Unknown { why } => DestinationHealth::Unknown { why: why.clone() },
    }
}

/// Fact ③ for one destination, read out of the durable push record — the
/// same three-state mapping `reconcile` uses, lifted out so `reclaim-stage`
/// derives its classification from the *same* read the surfaces do (a
/// reclaim that judged health from any other input could disagree with
/// `status` about the same destination on the same minute).
pub fn last_push_of(read: &crate::pushstate::PushStateRead, name: &str) -> LastPush {
    match read {
        crate::pushstate::PushStateRead::Present(state) => match state.pushes.get(name) {
            Some(record) => LastPush::At {
                finished_at_unix: record.finished_at_unix,
                snapshots_in_repo: record.snapshots_in_repo,
            },
            None => LastPush::NoRecord,
        },
        crate::pushstate::PushStateRead::Unreadable(why) => LastPush::Unknown { why: why.clone() },
        crate::pushstate::PushStateRead::Missing => LastPush::NoRecord,
    }
}

/// What the stage-cap arm of the reconciliation measured (ADR-060 D5): the
/// stage's shard-body bytes against the configured cap. The alarm-only rule
/// lives here as the type's shape — over the cap is a fact someone sees,
/// never an action anything takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageCapFact {
    /// No stage is configured on this machine (`[native_host] stage` is the
    /// only stage the config names), so the question has nothing to
    /// measure. Quiet everywhere: absence of the configured input is not
    /// an unknown about a stage that exists.
    NoStage,
    /// A stage is configured but its sessions tree could not be walked.
    /// Quiet in the alerts (the bytes are unknown, not "zero bytes — under
    /// cap": an unknown may never be recorded as empty), visible with its
    /// reason in `doctor`'s table and `--json`.
    Unmeasurable { why: String },
    /// The measured shard-body bytes and the effective cap
    /// (configured, else the 10 GiB default).
    Measured { bytes: u64, cap: u64 },
}

impl StageCapFact {
    /// The (bytes, cap) pair when the stage is over the cap — the pair the
    /// red alarm names, `None` for every other state.
    pub fn exceeded(&self) -> Option<(u64, u64)> {
        match self {
            StageCapFact::Measured { bytes, cap } if bytes > cap => Some((*bytes, *cap)),
            _ => None,
        }
    }
}

/// The stage's shard-body bytes — ADR-060 D5's size basis, decided 10-22:
/// the `sessions/` tree only, a stat-only walk (`metadata()`, no file is
/// opened), `meta/` excluded (measured ~10 MB — noise, 46 §2). Machine
/// logs and the extension status tree are not the session body this cap
/// describes and stay out of the number.
///
/// A stage with no `sessions/` directory holds zero body bytes — a real
/// zero, not an unknown; a walk that fails mid-tree is an error the caller
/// keeps as [`StageCapFact::Unmeasurable`], never a zero.
pub fn stage_shard_body_bytes(stage: &Path) -> std::io::Result<u64> {
    let root = stage.join(crate::store::SESSIONS_DIR);
    let mut stack = vec![root];
    let mut total: u64 = 0;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(entry.path());
            } else {
                // `DirEntry::metadata` stats without following a symlink,
                // so a link planted inside the tree counts as itself and
                // never wanders off the stage.
                total = total.saturating_add(entry.metadata()?.len());
            }
        }
    }
    Ok(total)
}

/// One declared destination and its four facts, plus the derived health the
/// reclaim and every surface share (ADR-060 D2).
#[derive(Debug, Clone)]
pub struct DestinationFacts {
    pub name: String,
    pub push_coverage: PushCoverage,
    pub credentials: CredentialResolution,
    pub last_push: LastPush,
    /// ADR-060 D2's derived state, classified from `last_push` and the
    /// effective degrade-after at read time.
    pub health: DestinationHealth,
    /// The effective degrade-after the health was judged against, in days
    /// (per-destination override, else global `[reclaim]`, else the default)
    /// — carried beside the state so every surface names the threshold the
    /// same way the reclaim judged it.
    pub degrade_after_days: u64,
    pub reclaim_share: ReclaimShare,
}

/// The machine-level half of the reconciliation: the weekly reclaim job
/// itself, which the per-destination shares only ever describe through the
/// latest attempt.
#[derive(Debug, Clone)]
pub struct ReclaimJobFacts {
    /// Whether a reclaim-stage unit file is installed at all. A machine
    /// without one has no weekly job to diagnose — its reclaim facts stay
    /// quiet rather than becoming a false alarm. `None` when this build
    /// cannot read the platform's scheduler: an empty unit list is then a
    /// fact about the reader, not about the machine, and `false` would be
    /// the same "read out of silence" the coverage fact refuses.
    pub unit_installed: Option<bool>,
    /// Age of the oldest installed unit file: the "how long has this job
    /// been here" anchor for the never-recorded case, where no record
    /// exists to date anything from. `None` when not installed or the file
    /// could not be stat'd.
    pub unit_age_secs: Option<u64>,
    /// What the durable record says: the attempt, its age in seconds, and
    /// the streak of consecutive non-ok attempts.
    pub record: ReclaimJobRecord,
}

/// The durable record's content, in the shapes a reader needs.
#[derive(Debug, Clone)]
pub enum ReclaimJobRecord {
    /// No `reclaim-state.json` was ever written here.
    Missing,
    /// A record exists but could not be parsed (its own state, never
    /// "healthy" and never "never ran").
    Unreadable(String),
    /// The recorded attempt plus its age at read time.
    Present {
        attempt: crate::reclaimstate::ReclaimAttempt,
        age_secs: u64,
    },
}

/// ②, extended to the pull producers (W964): whether the declared
/// `[pull.letta]` producer's credential resolves in the scheduled pass
/// that pulls it — the same question the per-destination credentials fact
/// asks, asked about the producer the timer's run-once pass pulls before
/// its collect step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullProducerCredentials {
    /// `pull.letta.api_key` is declared — a literal, or one of the three
    /// persistent references — which resolves in a timer process exactly
    /// the way a destination's declared credential does.
    Declared,
    /// No `api_key` is declared, so the producer falls back to
    /// `LETTA_API_KEY` in the process environment: a variable a login
    /// shell exports and a launchd or systemd job does not see — and the
    /// scheduled pass is exactly that job.
    EnvironmentOnly,
}

/// The one-sentence finding for a declared producer whose credential only
/// a login shell could reach. `schedule install` prints it at install time
/// and the reconciliation's alerts carry the same words to `status`,
/// `doctor` and the menu bar app, because the same misconfiguration must
/// not read differently on different surfaces.
pub fn pull_producer_credential_alert() -> Alert {
    Alert {
        severity: Severity::Yellow,
        sentence: "[pull] the declared [pull.letta] producer reads LETTA_API_KEY from the \
environment because pull.letta.api_key is not set — a login shell exports that variable, \
a scheduled job never sees it, so every pass would exit 3 (\"did not finish reading\") at \
the credential check while the local archive cycle stays intact; declare pull.letta.api_key \
instead — a file:, env-file: or keychain: reference, the same forms destinations accept — \
and the next pass pulls it again"
            .to_string(),
        // Prose-only fix: there is no command that writes a credential, so
        // the sentence itself says what to do (the Alert contract).
        fix: None,
    }
}

/// The whole reconciliation: rows plus the alerts both prose surfaces and
/// the menu bar display. `alerts` and `job_alerts` are already worded —
/// the three surfaces display them unchanged, which is the shared-wording
/// half of C1; the fact enums beside them are what `--json` consumers and
/// `doctor`'s table read.
#[derive(Debug, Clone)]
pub struct Reconciliation {
    pub destinations: Vec<DestinationFacts>,
    pub reclaim: ReclaimJobFacts,
    /// The stage-cap arm (ADR-060 D5): the measured shard-body bytes of the
    /// configured stage against the effective cap — alarm-only, so the
    /// fact is data for the alerts and the surfaces, never an input to any
    /// run's outcome.
    pub stage_cap: StageCapFact,
    /// The declared pull producer's credential fact, W964's extension of
    /// ② to the producers the scheduled pass pulls. `None` when no
    /// `[pull.letta]` is declared.
    pub pull_producer: Option<PullProducerCredentials>,
    /// The scheduler format the unit-file facts were read in — the scope
    /// the coverage claims are worded against, so a sentence that says
    /// "nothing pushes it" names which files were read ("launchd agent
    /// (~/Library/LaunchAgents)" / "systemd user unit
    /// (~/.config/systemd/user)") instead of claiming the whole platform.
    pub format: Format,
    /// One alert per destination with something wrong, destination order.
    pub alerts: Vec<Alert>,
    /// Machine-level alerts (the reclaim job), after the destination ones.
    pub job_alerts: Vec<Alert>,
}

impl Reconciliation {
    /// Every alert, destination order first — the order the prose surfaces
    /// print and the menu bar ranks.
    pub fn all_alerts(&self) -> Vec<Alert> {
        let mut out = Vec::with_capacity(self.alerts.len() + self.job_alerts.len());
        out.extend(self.alerts.iter().cloned());
        out.extend(self.job_alerts.iter().cloned());
        out
    }
}

/// Where `reconcile` reads its facts from. Everything a caller has already
/// resolved, parameterised so a test can point the whole computation at a
/// throwaway tree (same shape as `schedule::schedule_install_state`'s
/// parameters).
pub struct Inputs<'a> {
    pub config: &'a Config,
    pub format: Format,
    pub home: &'a Path,
    pub state_dir: &'a Path,
    /// Now, in unix seconds — a parameter, never a clock read, so every
    /// age sentence is assertable without waiting (same rule as
    /// `runstate::summarize`).
    pub now_unix: u64,
    /// Whether this build can read the platform's scheduler at all. A
    /// platform the product *does* support scheduling on, but through a
    /// scheduler this code has no reader for (Windows: a documented
    /// `schtasks` task, and `schedule install` that refuses), must not have
    /// an unread directory interpreted as "no timer": `false` keeps every
    /// file-derived coverage and credential claim in its unknown state and
    /// leaves the record-based facts (③'s absence, the reclaim records) to
    /// speak, because those it can read.
    pub scheduler_readable: bool,
    /// The stage the `[native_host] stage` key names, already expanded by
    /// the config loader — the only stage the config itself knows, and the
    /// one the D5 cap alarm measures. `None` when nothing is configured,
    /// which is the fact's own state ([`StageCapFact::NoStage`]), never an
    /// unknown about a stage that exists.
    pub stage: Option<&'a Path>,
}

/// The one function (C1): read the local, read-only facts and turn them into
/// per-destination verdicts plus the worded alerts.
pub fn reconcile(inputs: &Inputs) -> Reconciliation {
    let (run_once_units, reclaim_units) = if inputs.scheduler_readable {
        (
            schedule::installed_run_once_units(inputs.format, inputs.home),
            schedule::installed_reclaim_units(inputs.format, inputs.home),
        )
    } else {
        // A scheduler this build cannot read is a platform fact, not an
        // empty machine: reading nothing must not be laundered into
        // "nothing is installed". The file-derived facts answer unknown
        // below instead of mining an unread platform for a negative, and
        // the reclaim-job findings stay quiet the way an unobservable
        // job's already do — the record-based facts still speak, because
        // the state files they read are platform-independent.
        (Vec::new(), Vec::new())
    };
    let read_failures: Vec<&schedule::UnitObservation> = run_once_units
        .iter()
        .chain(reclaim_units.iter())
        .filter(|unit| unit.parse_note.is_some())
        .collect();

    let raw_scan = raw_env_option_refs();

    // The destination set is the declared set; a config with no destinations
    // table gets the single row for the repository a destination-less timer
    // pushes ("default" — the same word `reclaim-stage` and `pushstate`
    // use, so one archive cannot grow two names).
    let mut names: Vec<String> = inputs.config.destinations.keys().cloned().collect();
    names.sort();
    let names = if names.is_empty() {
        vec![crate::pushstate::DEFAULT_DESTINATION.to_string()]
    } else {
        names
    };

    let push_records = crate::pushstate::load(inputs.state_dir);
    let reclaim_read = crate::reclaimstate::load(inputs.state_dir);

    let mut destinations = Vec::with_capacity(names.len());
    for name in &names {
        // ①  from the installed unit files — unless the platform's
        // scheduler is not one this build can read at all, in which case
        // the absence of files in *this* reader's directory proves
        // nothing about the platform's timers.
        let push_coverage = if inputs.scheduler_readable {
            push_coverage_of(name, &run_once_units, &read_failures)
        } else {
            PushCoverage::UnreadablePlatform {
                why: format!(
                    "this build cannot read this platform's scheduler, so whether a timer \
                     pushes `{name}` could not be established — the schedule the platform \
                     documents (docs/schedule.md) may name it, and unknown is not none"
                ),
            }
        };
        // ② the unit files that would contact this destination: whichever
        // hourly unit pushes it, plus every installed reclaim unit (the
        // weekly job consults every declared destination, named or not).
        let mut contacting: Vec<schedule::UnitObservation> = run_once_units
            .iter()
            .filter(|unit| match &push_coverage {
                PushCoverage::Covered { units } => units.contains(&unit.file),
                _ => false,
            })
            .cloned()
            .collect();
        contacting.extend(reclaim_units.iter().cloned());
        let credentials = if let Some(why) = &raw_scan.why_unparsed {
            // The raw re-read answers at all or it does not: a config
            // whose text failed this scan's own narrow parse leaves
            // *which* destinations carry shell-only references unknown,
            // and an empty map would answer "none" — silently.
            CredentialResolution::Unknown {
                why: format!(
                    "{why}, so whether `{name}`'s credentials include shell-only `env:` \
                     references could not be established"
                ),
            }
        } else if !inputs.scheduler_readable {
            match raw_scan.refs.get(name) {
                Some(refs) => CredentialResolution::UnreadablePlatform {
                    why: format!(
                        "the config declares shell-only credentials ({}), and this build \
                         cannot read this platform's scheduler, so whether a timer would \
                         resolve them could not be established",
                        refs.join(", ")
                    ),
                },
                // reason: a destination whose references are all
                // `file:` / `env-file:` / `keychain:` / literal resolves
                // without a shell or a timer — that fact does not depend
                // on reading any scheduler, so an unread scheduler does
                // not make it unknown.
                None => CredentialResolution::ShellIndependent,
            }
        } else {
            credentials_of(name, &contacting, &raw_scan.refs)
        };
        // ③ the durable push record. The file's three read states stay three
        // facts: a readable file with no row for this destination is an
        // absence, and a file that failed to parse is an unknown — never the
        // same absence, which would assert "no push is on record" out of a
        // read that never happened. `last_push_of` is the one mapping both
        // this function and `reclaim-stage` use, so the fact and the
        // ADR-060 health derived from it can never disagree between the
        // surfaces and the reclaim.
        let last_push = last_push_of(&push_records, name);
        // ADR-060 D2's derived health, judged against this destination's own
        // effective degrade-after — the per-destination override (ruling A1)
        // when it declares one, else the global `[reclaim]`, else the default.
        let global_degrade_after_days = inputs
            .config
            .reclaim
            .as_ref()
            .and_then(|reclaim| reclaim.degrade_after_days);
        let per_destination_degrade_after_days = inputs
            .config
            .destinations
            .get(name)
            .and_then(|destination| destination.degrade_after_days);
        let degrade_after_days = effective_degrade_after_days(
            global_degrade_after_days,
            per_destination_degrade_after_days,
        );
        let health = classify_destination_health(&last_push, degrade_after_days, inputs.now_unix);
        // ④ from the latest recorded reclaim attempt, with ADR-060 D7's
        // distinct arm for the destination the run classified degraded: it
        // was not asked anything, which is neither "did not block" (a
        // state the user must see, folded into the shred it exists to
        // forbid) nor a block.
        let reclaim_share = match &reclaim_read {
            crate::reclaimstate::ReclaimStateRead::Present(attempt) => match attempt.outcome {
                crate::reclaimstate::ReclaimOutcome::Ok
                | crate::reclaimstate::ReclaimOutcome::Blocked => {
                    if let Some(block) = attempt
                        .blocked
                        .iter()
                        .find(|block| &block.destination == name)
                    {
                        ReclaimShare::Blocking {
                            kind: block.kind.to_string(),
                            error_kind: block.error_kind.clone(),
                            sessions: block.sessions,
                            bytes: block.bytes,
                        }
                    } else if let Some(row) = attempt
                        .destination_health
                        .as_ref()
                        .and_then(|rows| rows.iter().find(|row| &row.destination == name))
                    {
                        if row.role == "degraded-not-consulted" {
                            ReclaimShare::DegradedNotConsulted {
                                health: row.health.clone(),
                            }
                        } else {
                            ReclaimShare::NotBlocking
                        }
                    } else {
                        ReclaimShare::NotBlocking
                    }
                }
                crate::reclaimstate::ReclaimOutcome::Refused => ReclaimShare::UnknownRun {
                    outcome: "refused".to_string(),
                },
            },
            _ => ReclaimShare::UnknownRun {
                outcome: "no record".to_string(),
            },
        };
        destinations.push(DestinationFacts {
            name: name.clone(),
            push_coverage,
            credentials,
            last_push,
            health,
            degrade_after_days,
            reclaim_share,
        });
    }

    let reclaim = reclaim_job_facts(
        &reclaim_units,
        &reclaim_read,
        inputs.now_unix,
        inputs.scheduler_readable,
    );

    // ② for the declared pull producer: the loaded config has the api_key
    // resolved or absent, which is exactly the distinction the fact draws —
    // a reference that could not resolve refused the load, naming the key,
    // so every surface downstream of a loaded config already says that one
    // loudly.
    let pull_producer = inputs
        .config
        .pull
        .as_ref()
        .and_then(|pull| pull.letta.as_ref())
        .map(|letta| {
            if letta.api_key.is_some() {
                PullProducerCredentials::Declared
            } else {
                PullProducerCredentials::EnvironmentOnly
            }
        });

    // ADR-060 D5's stage-cap fact: the configured stage's shard-body bytes
    // (sessions tree, stat-only, `meta/` excluded) against the effective cap
    // — configured `[reclaim] stage_cap_bytes`, else the 10 GiB default. An
    // unconfigured stage has its own state, never a guessed path; a stage
    // that could not be walked is an unknown the alerts stay quiet about
    // and the table names, never zero.
    let stage_cap = match inputs.stage {
        None => StageCapFact::NoStage,
        Some(stage) => match stage_shard_body_bytes(stage) {
            Ok(bytes) => StageCapFact::Measured {
                bytes,
                cap: inputs
                    .config
                    .reclaim
                    .as_ref()
                    .and_then(|reclaim| reclaim.stage_cap_bytes)
                    .map(crate::body_cache::CacheSize::bytes)
                    // reason: absence of the key is ADR-060 D5's documented
                    // default (10 GiB) — a configured default like the body
                    // cache's quota, not an unknown turned into a number.
                    .unwrap_or(DEFAULT_STAGE_CAP_BYTES),
            },
            Err(e) => StageCapFact::Unmeasurable {
                why: format!("the configured stage's sessions tree could not be walked: {e}"),
            },
        },
    };

    let reconciliation = Reconciliation {
        destinations,
        reclaim,
        stage_cap,
        pull_producer,
        format: inputs.format,
        alerts: Vec::new(),
        job_alerts: Vec::new(),
    };
    word_alerts(reconciliation)
}

/// ① for one destination: the installed hourly units naming it, or the
/// destination-less unit for the "default" row.
fn push_coverage_of(
    name: &str,
    run_once_units: &[schedule::UnitObservation],
    read_failures: &[&schedule::UnitObservation],
) -> PushCoverage {
    let covering: Vec<String> = run_once_units
        .iter()
        .filter(|unit| {
            if name == crate::pushstate::DEFAULT_DESTINATION {
                // The single-destination shape: the destination-less timer
                // pushes the default repository.
                unit.destinations.is_empty()
            } else {
                unit.destinations.iter().any(|d| d == name)
            }
        })
        .map(|unit| unit.file.clone())
        .collect();
    if !covering.is_empty() {
        return PushCoverage::Covered { units: covering };
    }
    // A known negative requires that every relevant file was readable; one
    // unreadable unit file makes "no timer pushes it" a guess.
    if !read_failures.is_empty() {
        return PushCoverage::Unknown {
            why: format!(
                "{} installed timer file(s) could not be read, so whether one pushes `{name}` \
                 could not be established",
                read_failures.len()
            ),
        };
    }
    PushCoverage::NoScheduledPush
}

/// The raw `env:NAME` re-read of the config file, with the two states a
/// reader must keep apart: the per-destination references the scan found,
/// and — when the file's text exists but did not parse back as this
/// scan's own (deliberately narrow) shape — the reason it did not. The
/// narrow-parse failure is an *unknown about the references*, never the
/// answer "no destination has shell-only credentials": the product's own
/// `Config` loader is more permissive than this scan, `schedule install`
/// warning arms in this module are fed from the same scan, and a machine
/// whose loaded config survives while this re-read fails (a nested table
/// in `options`, an unescaped Windows path in `repo`) must not have its
/// fact ② quietly turned green by an empty map.
#[derive(Debug, Default)]
pub struct RawEnvScan {
    /// Destinations carrying `env:NAME` references, the names verbatim.
    pub refs: BTreeMap<String, Vec<String>>,
    /// Why the config file's text could not be re-read into this scan,
    /// when it could not be — `None` means the scan answered.
    pub why_unparsed: Option<String>,
}

/// Raw `env:NAME` option references per destination, re-read from the
/// config file's own text. The loaded `Config` has already replaced or
/// dropped these, so this is the only place the machine's *configured*
/// credential shape is still visible. Public for the install-time warning:
/// `schedule install` reads it to tell the user, before the first scheduled
/// run fails, that the timer it just wrote cannot carry these.
pub fn raw_env_option_refs() -> RawEnvScan {
    let mut out = BTreeMap::new();
    let text = match std::fs::read_to_string(crate::config::config_path()) {
        Ok(text) => text,
        // The config itself is unreadable: report no refs — callers word
        // that condition from the config error they already have, and a
        // per-destination unknown would only repeat it one row at a time.
        Err(_) => return RawEnvScan::default(),
    };
    #[derive(Deserialize)]
    struct RawDestination {
        options: Option<BTreeMap<String, String>>,
    }
    #[derive(Deserialize)]
    struct RawFile {
        destinations: Option<BTreeMap<String, RawDestination>>,
    }
    let file = match toml::from_str::<RawFile>(&text) {
        Ok(file) => file,
        // The file exists and its text was read, but it does not parse
        // back as this scan's own narrow shape — a shape the product's
        // more permissive loader can survive. Which destinations carry
        // shell-only references is then unknown, which is not "none of
        // them do" (an empty map would answer exactly that, silently).
        Err(_) => {
            return RawEnvScan {
                refs: BTreeMap::new(),
                why_unparsed: Some(
                    "the config file's text exists but could not be re-read for the \
                     environment-reference scan"
                        .to_string(),
                ),
            }
        }
    };
    // reason: an absent [destinations] table genuinely means "nothing is
    // written there" — the table's own absence is a configuration fact, not
    // an unknown read, so an empty map is the honest answer.
    for (name, dest) in file.destinations.unwrap_or_default() {
        let refs = dest
            .options
            // reason: absent options are the same fact as the table's
            // absence above — nothing is written, not "could not be read".
            .unwrap_or_default()
            .values()
            .filter_map(|value| {
                value.strip_prefix("env:").map(str::to_string)
                // reason: the name after `env:` is kept verbatim, exactly
                // the reference a config author wrote — an invalid name is
                // itself the fact this function exists to surface, and it
                // is a variable name, never a credential value.
            })
            .collect::<Vec<String>>();
        if !refs.is_empty() {
            out.insert(name, refs);
        }
    }
    RawEnvScan {
        refs: out,
        why_unparsed: None,
    }
}

/// ② for one destination, decided against the environment declarations of
/// exactly the unit files that would contact it.
///
/// `file:` / `env-file:` / `keychain:` references and literals never join
/// `refs`: they resolve without a login shell, which is the whole reason
/// they exist (and `credentials.rs` refuse-closes when they cannot), so
/// counting them would turn a healthy config red.
fn credentials_of(
    name: &str,
    contacting: &[schedule::UnitObservation],
    raw_refs: &BTreeMap<String, Vec<String>>,
) -> CredentialResolution {
    let Some(refs) = raw_refs.get(name) else {
        return CredentialResolution::ShellIndependent;
    };
    if contacting.is_empty() {
        return CredentialResolution::ShellOnlyWithNoTimer;
    }
    let failing_units: Vec<String> = contacting
        .iter()
        // reason: parse-note files land in `failing_units` below rather than
        // short-circuiting, so a resident unreadable file cannot silently
        // vouch for a variable nobody could see.
        .filter(|unit| {
            refs.iter()
                .any(|reference| !unit.env_names.contains(reference))
        })
        .map(|unit| unit.file.clone())
        .collect();
    if failing_units.is_empty() {
        return CredentialResolution::ResolvableInTimer {
            units: contacting.iter().map(|u| u.file.clone()).collect(),
        };
    }
    CredentialResolution::NotResolvableInTimer {
        refs: refs.clone(),
        units: failing_units,
    }
}

/// The scheduler the unit files were read from, in the two words a claim
/// about them names: what was read and where. A "nothing pushes it"
/// finding worded against the reader's own scope states its limits — a
/// user with system-scope units (`/Library/LaunchDaemons`,
/// `/etc/systemd/system`) or a hand-installed
/// timer (`schedule --output`) can read what was *not* looked at, instead
/// of being told no timer exists.
fn scheduler_scope(format: Format) -> (&'static str, &'static str) {
    match format {
        Format::Launchd => ("launchd agent", "~/Library/LaunchAgents"),
        Format::Systemd => ("systemd user unit", "~/.config/systemd/user"),
    }
}

/// The machine-level reclaim facts.
fn reclaim_job_facts(
    reclaim_units: &[schedule::UnitObservation],
    read: &crate::reclaimstate::ReclaimStateRead,
    now_unix: u64,
    scheduler_readable: bool,
) -> ReclaimJobFacts {
    // An unreadable scheduler leaves the question unanswered rather than
    // answered "no": the caller passes an empty list for a platform this
    // build has no reader for, and `!is_empty()` would turn that into a
    // measured negative about a machine that may well have the documented
    // `schtasks` task.
    let unit_installed = scheduler_readable.then(|| !reclaim_units.is_empty());
    // How old the oldest installed unit file is — the one honest "how long
    // has this job been on the machine" anchor for the never-recorded
    // case, where no record exists to date anything from.
    let unit_age_secs = reclaim_units
        .iter()
        .filter_map(|unit| unit.modified_unix)
        .map(|modified| now_unix.saturating_sub(modified))
        .max();
    let record = match read {
        crate::reclaimstate::ReclaimStateRead::Missing => ReclaimJobRecord::Missing,
        crate::reclaimstate::ReclaimStateRead::Unreadable(why) => {
            ReclaimJobRecord::Unreadable(why.clone())
        }
        crate::reclaimstate::ReclaimStateRead::Present(attempt) => ReclaimJobRecord::Present {
            attempt: attempt.clone(),
            age_secs: now_unix.saturating_sub(attempt.finished_at_unix),
        },
    };
    ReclaimJobFacts {
        unit_installed,
        unit_age_secs,
        record,
    }
}

/// Build the alerts — the sentences all three surfaces share. One alert per
/// destination per first-order fact that is not right (① coverage, ②
/// timer-visible credentials, ③ a recorded successful push); the reclaim
/// share adds the consequence that has already arrived, and the reclaim
/// job record speaks at machine level.
///
/// A quiet row is no alerts at all: `status`'s body stays byte-identical on
/// a machine whose destinations are all covered, all resolvable from a
/// timer, record-successful and currently reclaiming — the reconciliation
/// is a new sound,
/// not a new hum (same rule as `doctor::key_alert_lines`).
fn word_alerts(mut reconciliation: Reconciliation) -> Reconciliation {
    let destinations = reconciliation.destinations.clone();
    let reclaim = reconciliation.reclaim.clone();
    let stage_cap = reconciliation.stage_cap.clone();
    let pull_producer = reconciliation.pull_producer.clone();
    let format = reconciliation.format;

    // ② for the pull producer, after the per-destination alerts: the
    // declared producer whose credential only a login shell could reach is
    // the same yellow-shape gap a shell-only destination credential is —
    // a silent trap until the first pass fails at the credential check.
    if pull_producer == Some(PullProducerCredentials::EnvironmentOnly) {
        reconciliation.alerts.push(pull_producer_credential_alert());
    }

    for facts in &destinations {
        // The implicit "default" row is not a declared destination: a
        // single-destination machine has no destinations table to leave
        // anything out of, and "nothing pushes it by timer" is that
        // machine's ordinary `status` verdict-line territory (the
        // `[run-once]` fact), not an accusation. It keeps its
        // reclaim-share facts — a blocked reclaim holds the same bytes
        // back on a start.md machine — and it stays visible in doctor's
        // table and `--json`; only the coverage and credentials findings
        // are declared-set findings, and there is no declared set here.
        let declared_row = facts.name != crate::pushstate::DEFAULT_DESTINATION;
        if declared_row {
            // The wording names the scheduler and directory the facts were
            // read from, worded against what was read rather than against
            // the platform as a whole: a user with system-scope units or a
            // hand-installed timer can read the claim's scope off the
            // sentence instead of being told "no timer exists".
            let (scope_noun, scope_dir) = scheduler_scope(format);
            match &facts.push_coverage {
                PushCoverage::Covered { .. } => {}
                PushCoverage::NoScheduledPush => reconciliation.alerts.push(Alert {
                    severity: Severity::Yellow,
                    sentence: format!(
                        "[dest] {name}: declared, but no installed {scope_noun} ({scope_dir}) \
                         pushes it — a push reaches it only when a command names it by hand; \
                         until one succeeds it stays degraded (the weekly stage clean-up does \
                         not consult it and alarms every run, and if it is the only declared \
                         destination nothing is ever reclaimed); either install its timer or \
                         remove the declaration from the config",
                        name = facts.name
                    ),
                    fix: Some(format!(
                        "chat-stasher schedule install --destination {} --stage <stage>",
                        facts.name
                    )),
                }),
                // A file that exists but could not be read is a local anomaly
                // with a fix, so it speaks — tagged like every other alert.
                PushCoverage::Unknown { why } => reconciliation.alerts.push(Alert {
                    severity: Severity::Yellow,
                    sentence: format!("[dest] {}: {why}", facts.name),
                    fix: None,
                }),
                // A platform this build cannot read is not a finding about
                // this machine: it is the same on every machine there, and
                // nothing the user can act on, so it stays quiet the way
                // `ReclaimShare::UnknownRun` and an unobservable reclaim job
                // do. The unknown still shows in `doctor`'s table and
                // `--json`.
                PushCoverage::UnreadablePlatform { .. } => {}
            }
        }
        // ③'s unreadable arm. `ReclaimJobRecord::Unreadable` raises
        // Yellow at the job level (this module's own doctrine: "a local
        // anomaly the user can act on is worded as a `[dest]` alert"),
        // and a corrupt `push-state.json` is the same shape — a file
        // that exists and answers with nothing. Before this arm it was
        // invisible in `status` entirely: the ③ fact kept its unknown
        // shape in `doctor`'s table and `--json`, but no alert spoke, so
        // the panel stayed green until the next `push` printed its own
        // warning (`pushstate.rs:91-96`). A corrupt record is a local
        // anomaly with a fix (the next successful push rebuilds it —
        // `pushstate::record_success` does exactly that on the unreadable
        // read, keeping the other destinations' rows), so it speaks,
        // tagged like every other alert, and stays quiet where the
        // question is unanswerable rather than merely unanswered.
        if declared_row && matches!(facts.last_push, LastPush::Unknown { .. }) {
            if let LastPush::Unknown { why } = &facts.last_push {
                reconciliation.alerts.push(Alert {
                    severity: Severity::Yellow,
                    sentence: format!(
                        "[dest] {}: the push-state record exists but is unreadable ({why}), so \
                         when this destination was last pushed is unknown — not ok, and not never \
                         pushed; the next successful push rebuilds the record from what it knows",
                        facts.name,
                    ),
                    fix: Some(format!(
                        "chat-stasher push --destination {name} --stage <stage>   # a \
                             successful push rewrites the record, keeping the other \
                             destinations' rows",
                        name = facts.name
                    )),
                });
            }
        }
        // ADR-060 D2's degraded alarms — one per destination the derived
        // health names degraded, on every run, and (review ruling A2,
        // 2026-10-22) the advice always names both roads: repair the push, or
        // remove the destination from the config if it is no longer
        // needed. Never one road only — a one-road alarm is how a
        // legitimately hand-pushed destination (its user on global 7 days)
        // teaches everyone to ignore the dot. Degrading never touches the
        // push itself: a degraded destination keeps receiving pushes
        // exactly as before, and health heals automatically with the next
        // successful push because the state is derived, never latched.
        if declared_row {
            match &facts.health {
                DestinationHealth::Lagging => {
                    reconciliation.alerts.push(Alert {
                        severity: Severity::Yellow,
                        sentence: format!(
                            "[dest] {name}: degraded — its last successful push is older than \
                             its degrade-after of {days} day(s), so it is assumed to be missing \
                             the recent increment era; the weekly stage clean-up does not \
                             consult it (protection for its copy is this alarm and the union \
                             backfill, never stage retention), and if no healthy destination \
                             remains nothing is reclaimed at all. Two roads: repair the push \
                             (health heals automatically with the next successful push), or \
                             remove `[destinations.{name}]` from the config if this \
                             destination is no longer needed",
                            name = facts.name,
                            days = facts.degrade_after_days,
                        ),
                        fix: Some(format!(
                            "chat-stasher push --destination {name} --stage <stage>   # repair \
                                 the push; if no timer covers it yet: chat-stasher schedule \
                                 install --destination {name} --stage <stage>",
                            name = facts.name
                        )),
                    });
                }
                DestinationHealth::NeverPushed => {
                    reconciliation.alerts.push(Alert {
                        severity: Severity::Yellow,
                        sentence: format!(
                            "[dest] {name}: degraded — {no_record}. It has not received this \
                             machine's increments on this record, the weekly stage clean-up does \
                             not consult it, and if no healthy destination remains nothing is \
                             reclaimed at all. Two roads: give it its first successful push \
                             (health heals automatically from there), or remove \
                             `[destinations.{name}]` from the config if this destination is no \
                             longer needed",
                            name = facts.name,
                            no_record = no_record_why(&facts.name),
                        ),
                        fix: Some(format!(
                            "chat-stasher push --destination {name} --stage <stage>   # the \
                                 first successful push ends the degraded state; also install \
                                 its hourly timer: chat-stasher schedule install --destination \
                                 {name} --stage <stage>",
                            name = facts.name
                        )),
                    });
                }
                DestinationHealth::Healthy | DestinationHealth::Unknown { .. } => {}
            }
        }
        match &facts.credentials {
            CredentialResolution::ShellIndependent
            | CredentialResolution::ResolvableInTimer { .. } => {}
            CredentialResolution::NotResolvableInTimer { refs, units } if declared_row => {
                reconciliation.alerts.push(Alert {
                    severity: Severity::Yellow,
                    sentence: format!(
                        "[dest] {}: credentials are shell-only ({}) — the timer file(s) {} \
                         declare none of them, and a launchd / systemd job does not inherit your \
                         shell, so a scheduled run cannot authenticate to `{}`; write the same \
                         credentials in a form that resolves without a shell (`file:PATH`, \
                         `env-file:PATH:NAME`, on macOS `keychain:ACCOUNT`)",
                        facts.name,
                        refs.join(", "),
                        units.join(", "),
                        facts.name,
                    ),
                    fix: None,
                });
            }
            CredentialResolution::Unknown { why } if declared_row => {
                // A config re-read that failed is a local anomaly with a
                // fix (re-word the config), so it speaks — tagged like every
                // other alert, and worded as what it is, never as the
                // all-clear an empty map would print.
                reconciliation.alerts.push(Alert {
                    severity: Severity::Yellow,
                    sentence: format!("[dest] {}: {why}", facts.name),
                    fix: None,
                });
            }
            // ②'s platform arm is silent for ①'s platform arm's reason: an
            // unreadable scheduler is not a per-machine finding.
            CredentialResolution::UnreadablePlatform { .. } => {}
            _ => {}
        }
        // ③'s covered-but-never-recorded arm (the pre-ADR-060 shape) is
        // subsumed by the degraded-health alarm above: a destination with
        // no push on record is `never-pushed` — degraded, alarmed on every
        // run, with the two-road advice — whether or not a timer covers
        // it, which is the ADR's own unification of the finding.
        match &facts.reclaim_share {
            ReclaimShare::NotBlocking => {}
            ReclaimShare::Blocking {
                kind,
                error_kind,
                sessions,
                bytes,
            } => {
                // `kind` is the block's machine word; the sentence says it
                // the way `reclaim-stage` prints it, and the classifier's
                // verdict replaces the whole phrase for the failure it can
                // name the fix for.
                let why = match error_kind.as_deref() {
                    Some("credentials_missing_or_invalid") => {
                        "it could not be reached: credentials missing or invalid".to_string()
                    }
                    _ => match kind.as_str() {
                        "unreachable" => "it could not be read".to_string(),
                        "partial-read" => {
                            "it was read but not to completion (its absences prove nothing)"
                                .to_string()
                        }
                        "not-held" => "it does not hold every session".to_string(),
                        _ => "it holds different bytes for some session".to_string(),
                    },
                };
                reconciliation.alerts.push(Alert {
                    severity: Severity::Yellow,
                    sentence: format!(
                        "[reclaim] blocked by `{}`: {} — {sessions} session(s) / {} held back in \
                         the stage; nothing is reclaimed until the healthy set answers (this \
                         destination is healthy, so its word is the one that holds the bytes)",
                        facts.name,
                        why,
                        fmt_bytes(*bytes),
                    ),
                    fix: Some(
                        "chat-stasher reclaim-stage --stage <stage>   # dry run, reads the \
                                destinations and prints the reason"
                            .to_string(),
                    ),
                });
            }
            // ADR-060 D7's separate arm: the run classified this destination
            // degraded and did not consult it — a state the user must see,
            // never folded into `NotBlocking`. It stays quiet at the alert
            // level by design: the degradation the *current* health alarm
            // above speaks of is the same finding in the present tense,
            // with the same two-road advice, and a second sentence for the
            // same destination on the same panel is how alarms stop being
            // read. The arm is visible where facts live: `doctor`'s table
            // and `--json`.
            ReclaimShare::DegradedNotConsulted { .. } => {}
            ReclaimShare::UnknownRun { .. } => {}
        }
    }

    // The blocked streak colors the *job* alert and raises C2's threshold:
    // one blocked week can be a bad weekend, two in a row is the machine
    // SG-1 exists for.
    if let ReclaimJobRecord::Present {
        attempt,
        age_secs: _,
    } = &reclaim.record
    {
        if attempt.outcome != crate::reclaimstate::ReclaimOutcome::Ok {
            let streak = attempt.consecutive_blocked;
            let severity = if streak >= BLOCKED_STREAK_ALARM {
                Severity::Red
            } else {
                Severity::Yellow
            };
            // The two empty-`blocked` shapes a non-Ok attempt can have,
            // which the record must keep apart (an unknown-never-empty
            // rule of its own):
            //
            // * `Refused` — the attempt ended before the destinations were
            //   consulted, so nothing about the stage was measured either:
            //   the record keeps `candidates` absent for exactly that
            //   reason, and the alert must not launder the refusal's empty
            //   `blocked` list into `0 session(s) / 0 B held back for ;`:
            //   a fabricated measurement with a dangling list.
            //
            // * `Blocked` with an empty `blocked` list — ADR-060's
            //   empty-proof-set refusal: the run consulted, found every
            //   declared destination degraded, and deleted nothing on the
            //   ≥1 guard. The health snapshot the v2 record carries is what
            //   makes the shape checkable; the wording names the guard
            //   rather than a count nobody measured session-by-session.
            //
            // Counts print only for the outcome that measured them, and
            // the refusal reason lives in this one line rather than in a
            // second, milder alert that would contradict its own severity.
            let held_back = match (attempt.outcome, attempt.blocked.is_empty()) {
                (crate::reclaimstate::ReclaimOutcome::Refused, _) => {
                    "what it holds back is unknown — the attempt ended before the destinations \
                     were consulted"
                        .to_string()
                }
                (_, false) => {
                    let held: usize = attempt.blocked.iter().map(|b| b.sessions).sum();
                    let bytes: u64 = attempt.blocked.iter().map(|b| b.bytes).sum();
                    let destinations_held: Vec<&str> = attempt
                        .blocked
                        .iter()
                        .map(|b| b.destination.as_str())
                        .collect();
                    format!(
                        "{held} session(s) / {} held back for {}",
                        fmt_bytes(bytes),
                        destinations_held.join(", ")
                    )
                }
                (_, true) => {
                    // A `Blocked` attempt whose per-destination list is
                    // empty: the run proved nothing because nothing could
                    // be proven against — not a refusal, a refusal-shape of
                    // its own. The counts the run did measure are already
                    // in the sentence the way every blocked run's are: the
                    // whole candidate set was held back.
                    let degraded: Vec<&str> = attempt
                        .destination_health
                        .iter()
                        .flatten()
                        .filter(|row| row.role == "degraded-not-consulted")
                        .map(|row| row.destination.as_str())
                        .collect();
                    format!(
                        "no healthy destination to prove against — every declared destination \
                         is degraded ({}), so nothing was deleted; restoration path: fix \
                         pushes, health heals automatically",
                        if degraded.is_empty() {
                            // A blocked attempt recorded neither a block nor
                            // a health snapshot: what it held back is a
                            // blank in the record, worded as one rather
                            // than as a list invented from absence.
                            "destination states not recorded by that run".to_string()
                        } else {
                            degraded.join(", ")
                        }
                    )
                }
            };
            let noun = if streak == 1 {
                "blocked once in a row".to_string()
            } else {
                format!("blocked {streak} times in a row")
            };
            reconciliation.job_alerts.push(Alert {
                severity,
                sentence: format!(
                    "[reclaim] the weekly stage clean-up is {noun} — {held_back}; nothing was \
                     deleted and the stage keeps every byte it archives"
                ),
                fix: Some(
                    "chat-stasher reclaim-stage --stage <stage>   # dry run: names the \
                     destination and the reason in its output"
                        .to_string(),
                ),
            });
        }
    }

    // ADR-060 D5's stage cap: red through the SG-1 alert channel when the
    // stage's shard body is over the effective cap, and *only* that — the
    // cap adds no stop, no forced reclaim, no other consequence; the
    // weekly job's own alarms (above) are what say what is stuck. Wording
    // carries the measured bytes, the cap, and the fix commands, the one
    // shape the three surfaces share.
    if let Some((bytes, cap)) = stage_cap.exceeded() {
        reconciliation.job_alerts.push(Alert {
            severity: Severity::Red,
            sentence: format!(
                "[reclaim] the stage holds {} of shard body — over the configured cap of {} \
                 (sessions tree only; meta is excluded). This is an alarm, not a block: \
                 collection and the weekly clean-up run exactly as before, and the bytes stay \
                 until a proven reclaim removes them. If the stage is not shrinking, run the \
                 dry reclaim and read which destinations it names — repair their pushes, or \
                 remove a destination from the config if it is no longer needed",
                fmt_bytes(bytes),
                fmt_bytes(cap),
            ),
            fix: Some(
                "chat-stasher reclaim-stage --stage <stage>   # dry run: names what it would \
                 reclaim and what holds it back"
                    .to_string(),
            ),
        });
    }

    // The unreadable record needs no scheduler's say-so: the file exists
    // and answers with nothing, which is a measured negative the way ③'s
    // unreadable push-state arm is, so it speaks on every platform — on a
    // machine whose scheduler this build cannot read, the blocked-streak
    // arm above already reads this same file, and a v1 record left by an
    // older build must alarm there too rather than pass as a healthy
    // silence (ADR-060 D7, the W1012g policy applied forward). Being
    // pulled out of the E2 install gate below is the point: that gate
    // anchors its alarms on the measured fact that the job is installed,
    // which is exactly what an unreadable scheduler cannot supply.
    if let ReclaimJobRecord::Unreadable(why) = &reclaim.record {
        reconciliation.job_alerts.push(Alert {
            severity: Severity::Yellow,
            sentence: format!(
                "[reclaim] the reclaim record exists but is unreadable ({why}), so what the \
                 weekly job last did is unknown — not ok, and not never-run"
            ),
            fix: None,
        });
    }
    // The silent-failure shape the log cannot cover (SG-0 E2): the job is
    // installed and its record has gone quiet. "Never recorded a run" is
    // only an alarm once the timer file has been installed longer than
    // the grace window — minutes after `schedule install --unit
    // reclaim-stage`, the very first run is legitimately still up to a
    // week away, and alarming then would teach users to ignore the dot.
    // The unit file's own age is the anchor; a file whose mtime could not
    // be read keeps the finding quiet in `doctor`'s table rather than
    // firing an alarm an unknown age cannot justify.
    if reclaim.unit_installed == Some(true) {
        match &reclaim.record {
            ReclaimJobRecord::Missing
                if reclaim
                    .unit_age_secs
                    .is_some_and(|age| age > crate::reclaimstate::RECLAIM_OVERDUE_SECS) =>
            {
                reconciliation.job_alerts.push(Alert {
                    severity: Severity::Red,
                    sentence: format!(
                        "[reclaim] the weekly clean-up has never recorded a run on this machine, \
                         and its timer file has been installed for {} — a job that fails to start \
                         writes nothing anywhere, so the silence itself is the symptom (a \
                         chat-stasher older than {} also kept no record); no recorded run means \
                         no reclaim, and the stage keeps growing",
                        // reason: the arm above matched on
                        // `unit_age_secs.is_some_and(...)`, so the value is
                        // present here; the 0 only satisfies the type and is
                        // never printed.
                        human_age(reclaim.unit_age_secs.unwrap_or(0)),
                        env!("CARGO_PKG_VERSION"),
                    ),
                    fix: Some(
                        "chat-stasher reclaim-stage --stage <stage>   # run it where you can see \
                         the output"
                            .to_string(),
                    ),
                });
            }
            ReclaimJobRecord::Present { age_secs, .. }
                if *age_secs > crate::reclaimstate::RECLAIM_OVERDUE_SECS =>
            {
                reconciliation.job_alerts.push(Alert {
                    severity: Severity::Red,
                    sentence: format!(
                        "[reclaim] the installed weekly clean-up has not recorded a run for {} — \
                         a job that fails to start writes nothing anywhere, so the silence itself \
                         is the symptom: no run means no reclaim, and the stage keeps growing",
                        human_age(*age_secs)
                    ),
                    fix: Some(
                        "chat-stasher reclaim-stage --stage <stage>   # run it where you can see \
                         the output"
                            .to_string(),
                    ),
                });
            }
            _ => {}
        }
    }

    reconciliation
}

/// Coarse, honest duration wording — the same bands `runstate::human_age`
/// keeps, restated here so this module owns all of its sentences. Public
/// because `reclaim-stage` prints the same banding for the health lines it
/// lists beside this module's alerts (one wording, D2's shared-wording
/// rule).
pub fn human_age(secs: u64) -> String {
    if secs < 5400 {
        format!("{} minutes", (secs + 59) / 60)
    } else if secs < 172_800 {
        format!("{} hours", (secs + 3599) / 3600)
    } else {
        format!("{} days", (secs + 86_399) / 86_400)
    }
}

// ------------------------------------------------- the install-time warnings

/// W1012 / SG-1 C1's `schedule install` half, as lines the command prints
/// after its (successful) install (46-STAGE-GROWTH §5.3: warn on the spot
/// and state the consequence). Pure, so the wording is assertable in a unit
/// test; the command itself prints whatever this returns.
///
/// Three findings warn:
///
/// * **run-once install** — a declared pull producer whose credential the
///   timer environment cannot resolve; a declared destination no installed
///   hourly timer pushes; a declared producer shared by more than one
///   destination timer, whose hourly race this branch explains instead of
///   leaving the deferred status line to be met unexplained; and shell-only
///   credentials on a destination the timers *now* push — the files this
///   command writes declare no environment, so the very first scheduled
///   run cannot authenticate.
/// * **reclaim-stage install** — the job proves *every* declared
///   destination before deleting, and declared destinations no hourly
///   timer pushes are exactly the ones that will block it.
///
/// Empty on a consistent machine: the install that completes a machine's
/// timer set stays as quiet as the installer before it gained an opinion.
pub fn install_warnings(
    config: &Config,
    unit: schedule::Unit,
    format: schedule::Format,
    home: &Path,
) -> Vec<String> {
    let mut declared: Vec<&str> = config.destinations.keys().map(String::as_str).collect();
    declared.sort_unstable();
    // W964: the declared pull producer rides the run-once timer this
    // install just put down, so its credential trap is named here, at the
    // moment a user is watching — with the same sentence the
    // reconciliation carries to status, doctor and the menu bar. A missing
    // credential declaration is not a refusal: the local archive cycle
    // still works, and the warning plus the per-pass exit 3 are the two
    // places the trap is said out loud.
    let mut producer_line = Vec::new();
    let producer_declared = config
        .pull
        .as_ref()
        .and_then(|pull| pull.letta.as_ref())
        .is_some();
    if matches!(
        config
            .pull
            .as_ref()
            .and_then(|pull| pull.letta.as_ref())
            .map(|letta| letta.api_key.is_some()),
        Some(false)
    ) {
        let alert = pull_producer_credential_alert();
        producer_line.push(format!("schedule install: WARNING ⚠ {}", alert.sentence));
    }
    // Single-destination machines have no declared set to reconcile — the
    // one timer they just installed pushes the default repository, and
    // nothing could be "declared but uncovered". The producer warning
    // above is independent of the destination set, so it is returned even
    // here.
    if declared.is_empty() {
        return producer_line;
    }
    let run_once_units = schedule::installed_run_once_units(format, home);
    let covered: std::collections::BTreeSet<&str> = run_once_units
        .iter()
        .flat_map(|observed| observed.destinations.iter().map(String::as_str))
        .collect();
    let missing: Vec<&str> = declared
        .iter()
        .copied()
        .filter(|name| !covered.contains(name))
        .collect();

    match unit {
        schedule::Unit::RunOnce => {
            // The producer warning leads: the timer this install just put
            // down is the pass that pulls the producer, so its credential
            // question is answered here at install time with the same
            // sentence every later surface repeats.
            let mut lines = producer_line;
            // W964: with one timer per destination and one shared producer
            // state, every hourly race has exactly one winner; the other
            // passes defer. The deferral is recorded and rendered rather
            // than an error, so the one moment a user is guaranteed to be
            // reading — this printout — is where the deferred producer
            // line they will see each hour is explained as the hand-off it
            // is, before they meet it unexplained in `status`.
            if producer_declared && covered.len() > 1 {
                lines.push(format!(
                    "schedule install: WARNING ⚠ the declared [pull.letta] producer is pulled \
                     by each of the {} installed destination timers, and they share one \
                     producer state: each hour the passes serialize, and all but the one that \
                     wins the lock defer instead of pulling.",
                    covered.len()
                ));
                lines.push(
                    "schedule install:   The deferral is recorded, not hidden: the deferring \
                      passes' status shows the producer line \"Letta pull deferred\" beside a \
                      green archive cycle and their run-once exits stay clean, the winning \
                      pass reports the producer's own result that hour, and every pass's push \
                      moves the shared stage."
                        .to_string(),
                );
            }
            if !missing.is_empty() {
                lines.push(format!(
                    "schedule install: WARNING ⚠ no installed timer pushes declared \
                     destination(s): {}",
                    missing.join(", ")
                ));
                lines.push(
                    "schedule install:   A push reaches them only when a command names them by \
                     hand, and the weekly stage clean-up classifies every declared destination: \
                     without a recent successful push they are degraded — not consulted for proof, \
                     alarmed on every run, and the stage keeps every byte the healthy set has not \
                     proven. If no healthy destination remains, nothing is reclaimed at all."
                        .to_string(),
                );
                lines.push(
                    "schedule install:   Install their timers too \
                     (`chat-stasher schedule install --destination <name> --stage <stage>`) or \
                     remove the declaration from the config."
                        .to_string(),
                );
            }
            lines.extend(shell_only_credential_warnings(&covered));
            lines
        }
        schedule::Unit::ReclaimStage => {
            if missing.is_empty() {
                return Vec::new();
            }
            vec![
                format!(
                    "schedule install: WARNING ⚠ the weekly clean-up proves every HEALTHY declared \
                     destination before it deletes anything, and no installed hourly timer \
                     pushes: {}",
                    missing.join(", ")
                ),
                "schedule install:   A destination without a recent successful push is degraded: \
                  the clean-up does not consult it, alarms on every run, and if none of the \
                  declared destinations is healthy nothing is deleted and the stage keeps \
                  growing. `chat-stasher status` reports the health and the alarm; health heals \
                  automatically with the next successful push."
                    .to_string(),
                "schedule install:   Install their hourly timers too (`chat-stasher schedule \
                 install --destination <name> --stage <stage>`) or remove the declaration from \
                 the config."
                    .to_string(),
            ]
        }
    }
}

/// The credentials half of the install warning: destinations the installed
/// timers now push whose `env:NAME` references no timer file can carry.
/// `covered` is the destination set the hourly timers name — a destination
/// no timer pushes is the *coverage* warning's story to tell, not this
/// one's.
fn shell_only_credential_warnings(covered: &std::collections::BTreeSet<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    let scan = raw_env_option_refs();
    if let Some(why) = &scan.why_unparsed {
        // The scan could not look, and a warning list that stays empty
        // would read as "nothing to warn about": name the unknown the way
        // the reconciliation's fact ② does.
        lines.push(
            "schedule install: WARNING ⚠ the config file's raw text could not be re-read to \
             check for shell-only `env:` credentials, so shell-only forms cannot be ruled out"
                .to_string(),
        );
        lines.push(format!("schedule install:   {why}"));
        return lines;
    }
    for (name, refs) in scan.refs {
        if !covered.contains(name.as_str()) || refs.is_empty() {
            continue;
        }
        lines.push(format!(
            "schedule install: WARNING ⚠ destination {name} is pushed by a timer, but its \
             credentials are shell-only ({})",
            refs.join(", ")
        ));
        lines.push(
            "schedule install:   The timer files this command writes declare no environment, \
             and a launchd / systemd job does not inherit your shell — every scheduled run \
             will fail to authenticate. Write the credentials in a form that resolves without \
             a shell (`file:PATH`, `env-file:PATH:NAME`, on macOS `keychain:ACCOUNT`) and \
             re-run this install."
                .to_string(),
        );
    }
    lines
}

/// Bytes as the CLI prints them elsewhere (`fmt_bytes` in main is private;
/// this one reformats for the few sentences above and need not agree with
/// the big report's exact rounding — one decimal, binary units).
pub fn fmt_bytes(bytes: u64) -> String {
    fn fmt(value: f64) -> String {
        format!("{value:.1}")
    }
    const UNIT: f64 = 1024.0;
    let value = bytes as f64;
    if value < UNIT {
        format!("{bytes} B")
    } else if value < UNIT * UNIT {
        format!("{} KiB", fmt(value / UNIT))
    } else if value < UNIT * UNIT * UNIT {
        format!("{} MiB", fmt(value / (UNIT * UNIT)))
    } else if value < UNIT * UNIT * UNIT * UNIT {
        format!("{} GiB", fmt(value / (UNIT * UNIT * UNIT)))
    } else {
        format!("{} TiB", fmt(value / (UNIT * UNIT * UNIT * UNIT)))
    }
}

// ---------------------------------------------------------------- rendering

/// The `status --json` **destination layer**: the four facts per destination,
/// machine-readable, each fact a tagged `kind` the way the rest of the
/// document tags its tri-states — an unknown is its own tagged case with a
/// `why`, never a null and never a zero-shaped silence.
pub fn destinations_json(r: &Reconciliation) -> serde_json::Value {
    serde_json::json!({
        "destinations": r.destinations.iter().map(destination_facts_json).collect::<Vec<_>>(),
        "alerts": r.all_alerts().iter().map(alert_json).collect::<Vec<_>>(),
        // ② for the declared pull producer: `null` when none is declared,
        // so "no producer" stays distinct from "a producer whose
        // credential a timer cannot reach".
        "pull_producer": match &r.pull_producer {
            None => serde_json::Value::Null,
            Some(PullProducerCredentials::Declared) => serde_json::json!({
                "kind": "declared",
            }),
            Some(PullProducerCredentials::EnvironmentOnly) => serde_json::json!({
                "kind": "environment_only",
            }),
        },
        "reclaim": reclaim_json(r),
        // ADR-060 D5's stage-cap fact: the measured shard-body bytes against
        // the effective cap — data for the menu bar, never an input to any
        // run's outcome (the cap is alarm-only). Unknown stays unknown,
        // never a zero-shaped silence.
        "stage_cap": stage_cap_json(r),
    })
}

/// The `--json` shape of the stage-cap fact (ADR-060 D5).
fn stage_cap_json(r: &Reconciliation) -> serde_json::Value {
    match &r.stage_cap {
        StageCapFact::NoStage => serde_json::json!({ "kind": "no_stage" }),
        StageCapFact::Unmeasurable { why } => {
            serde_json::json!({ "kind": "unknown", "why": why })
        }
        StageCapFact::Measured { bytes, cap } => serde_json::json!({
            "kind": "measured",
            "bytes": bytes,
            "cap": cap,
            "exceeded": bytes > cap,
        }),
    }
}

fn destination_facts_json(facts: &DestinationFacts) -> serde_json::Value {
    serde_json::json!({
        "name": facts.name,
        "push_coverage": match &facts.push_coverage {
            PushCoverage::Covered { units } => serde_json::json!({
                "kind": "covered",
                "units": units,
            }),
            PushCoverage::NoScheduledPush => serde_json::json!({ "kind": "no_scheduled_push" }),
            // Both unknown arms carry the same machine-readable shape: the
            // difference is only whether the prose surfaces alert on it.
            PushCoverage::Unknown { why } | PushCoverage::UnreadablePlatform { why } => {
                serde_json::json!({
                    "kind": "unknown",
                    "why": why,
                })
            }
        },
        "credentials": match &facts.credentials {
            CredentialResolution::ShellIndependent => serde_json::json!({
                "kind": "shell_independent",
            }),
            CredentialResolution::ResolvableInTimer { units } => serde_json::json!({
                "kind": "resolvable_in_timer",
                "units": units,
            }),
            CredentialResolution::NotResolvableInTimer { refs, units } => serde_json::json!({
                "kind": "not_resolvable_in_timer",
                "refs": refs,
                "units": units,
            }),
            CredentialResolution::ShellOnlyWithNoTimer => serde_json::json!({
                "kind": "shell_only_with_no_timer",
            }),
            CredentialResolution::Unknown { why } | CredentialResolution::UnreadablePlatform { why } => {
                serde_json::json!({
                    "kind": "unknown",
                    "why": why,
                })
            }
        },
        "last_push": match &facts.last_push {
            LastPush::At {
                finished_at_unix,
                snapshots_in_repo,
            } => serde_json::json!({
                "kind": "known",
                "finished_at_unix": finished_at_unix,
                "snapshots_in_repo": snapshots_in_repo,
            }),
            LastPush::NoRecord => serde_json::json!({
                "kind": "no_record",
                "why": no_record_why(&facts.name),
            }),
            LastPush::Unknown { why } => serde_json::json!({
                "kind": "unknown",
                "why": why,
            }),
        },
        // ADR-060 D2: the health vocabulary is the four states verbatim —
        // `healthy` / `lagging` / `never-pushed` / `unknown` — and the
        // prose rule keeps its distance: the slug `never-pushed` is a
        // machine word, and the human sentence for the same state says
        // "no push on record", never the suggestive "never pushed".
        "health": match &facts.health {
            DestinationHealth::Unknown { why } => serde_json::json!({
                "kind": "unknown",
                "why": why,
                "degrade_after_days": facts.degrade_after_days,
            }),
            health => serde_json::json!({
                "kind": health.slug(),
                "degrade_after_days": facts.degrade_after_days,
            }),
        },
        "reclaim_share": match &facts.reclaim_share {
            ReclaimShare::NotBlocking => serde_json::json!({ "kind": "not_blocking" }),
            ReclaimShare::Blocking { kind, error_kind, sessions, bytes } => serde_json::json!({
                "kind": "blocking",
                "block_kind": kind,
                "error_kind": error_kind,
                "sessions": sessions,
                "bytes": bytes,
            }),
            // D7's third visible arm, machine-readable, in the ADR's own
            // words: the state the run actually recorded for a
            // degraded destination.
            ReclaimShare::DegradedNotConsulted { health } => serde_json::json!({
                "kind": "degraded-not-consulted",
                "health": health,
            }),
            ReclaimShare::UnknownRun { outcome } => serde_json::json!({
                "kind": "unknown_run",
                "outcome": outcome,
            }),
        },
    })
}

fn alert_json(alert: &Alert) -> serde_json::Value {
    serde_json::json!({
        "severity": alert.severity.slug(),
        "sentence": alert.sentence,
        "fix": alert.fix,
    })
}

/// The reclaim job's machine-readable half — the object the menu bar app
/// reads for C2's notification decision. A machine with no job installed
/// says `unit_installed: false` and stops: state about a job that does not
/// exist would be another way to invent "never ran". A machine whose
/// scheduler this build cannot read says `unit_installed: null` plus a
/// `unit_installed_why`: an empty unit list is a fact about the reader, and
/// `false` there would be the same "read out of silence" the coverage fact
/// refuses.
fn reclaim_json(r: &Reconciliation) -> serde_json::Value {
    let value = match &r.reclaim.record {
        ReclaimJobRecord::Missing => serde_json::json!({
            "kind": "missing",
            "why": "no reclaim attempt has ever been recorded on this machine",
        }),
        ReclaimJobRecord::Unreadable(why) => serde_json::json!({
            "kind": "unreadable",
            "why": why,
        }),
        ReclaimJobRecord::Present { attempt, age_secs } => {
            let mut value = serde_json::json!({
                "kind": "known",
                "state": attempt.outcome.slug(),
                "apply": attempt.apply,
                "consecutive_blocked": attempt.consecutive_blocked,
                "finished_at_unix": attempt.finished_at_unix,
                "age_secs": age_secs,
                "overdue": *age_secs > crate::reclaimstate::RECLAIM_OVERDUE_SECS,
                "overdue_after_secs": crate::reclaimstate::RECLAIM_OVERDUE_SECS,
                "blocked": attempt.blocked.iter().map(|b| serde_json::json!({
                    "destination": b.destination,
                    "kind": b.kind,
                    "error_kind": b.error_kind,
                    "sessions": b.sessions,
                    "bytes": b.bytes,
                })).collect::<Vec<_>>(),
                // ADR-060 D7: the run's per-destination health snapshot —
                // what the run actually saw, so a degraded destination is
                // read as that state (`never-pushed` and friends are the
                // D2 slugs recorded verbatim) and never as a zero-shaped
                // "nothing happened". Absent when the run recorded no
                // classification.
                "destination_health": attempt.destination_health.iter().flatten().map(|row| serde_json::json!({
                    "destination": row.destination,
                    "health": row.health,
                    "role": row.role,
                })).collect::<Vec<_>>(),
            });
            // The same honesty the record itself keeps (its `candidates`
            // are absent for a refusal): a run that never surveyed the
            // stage has no held-back total, and absent — not summed-over-
            // nothing to zero — is how a JSON consumer reads it. The menu
            // bar's notification builder treats an absent count the same
            // way it already treats a structural absence.
            if !attempt.blocked.is_empty() {
                if let Some(object) = value.as_object_mut() {
                    object.insert(
                        "held_back_sessions".to_string(),
                        serde_json::json!(attempt
                            .blocked
                            .iter()
                            .map(|b| b.sessions)
                            .sum::<usize>()),
                    );
                    object.insert(
                        "held_back_bytes".to_string(),
                        serde_json::json!(attempt.blocked.iter().map(|b| b.bytes).sum::<u64>()),
                    );
                }
            }
            value
        }
    };
    let mut out = value;
    if let Some(object) = out.as_object_mut() {
        match r.reclaim.unit_installed {
            Some(installed) => {
                object.insert("unit_installed".to_string(), serde_json::json!(installed));
            }
            None => {
                object.insert("unit_installed".to_string(), serde_json::Value::Null);
                object.insert(
                    "unit_installed_why".to_string(),
                    serde_json::json!(
                        "this build cannot read this platform's scheduler, so whether a \
                         reclaim job is installed could not be established"
                    ),
                );
            }
        }
        if let Some(age) = r.reclaim.unit_age_secs {
            object.insert("unit_age_secs".to_string(), serde_json::json!(age));
        }
        // The consecutive-blocked threshold C2 notifies from lives in Rust
        // (`BLOCKED_STREAK_ALARM`); exposing it here is what keeps the menu
        // bar app's copy of the number from drifting away from this one.
        object.insert(
            "blocked_streak_alarm".to_string(),
            serde_json::json!(BLOCKED_STREAK_ALARM),
        );
    }
    out
}

/// The absent-record wording, shared by the JSON and `doctor`'s table: the
/// absence names itself, the version that started keeping the record names
/// itself, and "never" is nowhere in the sentence.
fn no_record_why(name: &str) -> String {
    format!(
        "no push to `{name}` is on record on this machine — this CLI started keeping the \
         per-destination push record at {}, so an earlier push is invisible rather than \
         disproven",
        env!("CARGO_PKG_VERSION")
    )
}

/// The `doctor` rendering: one compact block per destination — the four
/// facts in fixed order — followed by the alerts (the same sentences the
/// other two surfaces show, prefix included, because that shared wording is
/// C1's point). Pure, so the wording is assertable without a machine.
pub fn doctor_lines(r: &Reconciliation) -> Vec<String> {
    let mut lines = Vec::new();
    // ② for the declared pull producer, whether or not any destination is
    // declared: the producer's credential question is independent of the
    // destination set.
    match &r.pull_producer {
        Some(PullProducerCredentials::Declared) => lines.push(
            "  [pull.letta] credential: declared (a literal, or a file:/env-file:/keychain: \
             reference) — a scheduled pass can resolve it"
                .to_string(),
        ),
        Some(PullProducerCredentials::EnvironmentOnly) => lines.push(
            "  [pull.letta] credential: environment only (LETTA_API_KEY) — a scheduled pass \
             does not see it; declare pull.letta.api_key (a file:/env-file:/keychain: \
             reference, the same forms destinations accept)"
                .to_string(),
        ),
        None => {}
    }
    if r.destinations.is_empty() {
        lines.push(
            "  no destination is declared; the archive goes to the local repository".to_string(),
        );
        return lines;
    }
    for facts in &r.destinations {
        lines.push(format!("  {}", facts.name));
        // ① — worded against what was read, the same scope mirroring the
        // alert sentence's.
        let (scope_noun, scope_dir) = scheduler_scope(r.format);
        lines.push(format!(
            "    timer push     : {}",
            match &facts.push_coverage {
                PushCoverage::Covered { units } => units.join(", "),
                PushCoverage::NoScheduledPush => {
                    format!("no installed {scope_noun} ({scope_dir}) names this destination")
                }
                PushCoverage::Unknown { why } => format!("unknown — {why}"),
                PushCoverage::UnreadablePlatform { why } => format!("unknown — {why}"),
            }
        ));
        // ②
        lines.push(format!(
            "    timer creds    : {}",
            match &facts.credentials {
                CredentialResolution::ShellIndependent => {
                    "shell-independent forms only — a timer can resolve them".to_string()
                }
                CredentialResolution::ResolvableInTimer { units } => format!(
                    "every contacting timer declares them ({})",
                    units.join(", ")
                ),
                CredentialResolution::NotResolvableInTimer { refs, units } => format!(
                    "shell-only ({}) — not declared in: {}",
                    refs.join(", "),
                    units.join(", "),
                ),
                CredentialResolution::ShellOnlyWithNoTimer => {
                    "shell-only (env:), and no installed timer contacts this destination"
                        .to_string()
                }
                CredentialResolution::Unknown { why } => format!("unknown — {why}"),
                CredentialResolution::UnreadablePlatform { why } => format!("unknown — {why}"),
            }
        ));
        // ③
        lines.push(format!(
            "    last push      : {}",
            match &facts.last_push {
                LastPush::At {
                    finished_at_unix,
                    snapshots_in_repo,
                } => format!(
                    "unix {finished_at_unix} ({snapshots_in_repo} snapshot(s) in the destination)"
                ),
                LastPush::NoRecord => no_record_why(&facts.name),
                LastPush::Unknown { why } => format!("unknown — {why}"),
            }
        ));
        // ADR-060 D2's derived health, the same one function `reclaim-stage`
        // consults: the state word, the effective degrade-after it was
        // judged against, and — for the two degraded states — the fact that
        // the empty-proof-set refusal is what hangs on them when nothing
        // healthy remains.
        lines.push(format!(
            "    health         : {} (degrade after {} day(s)){}",
            match &facts.health {
                DestinationHealth::Healthy => "healthy".to_string(),
                DestinationHealth::Lagging => {
                    "degraded — last successful push older than its degrade-after".to_string()
                }
                // Prose keeps the absence rule: the state word in `--json`
                // is `never-pushed`; the human table says "no push on
                // record" and never "never pushed".
                DestinationHealth::NeverPushed => {
                    "degraded — no push on record".to_string()
                }
                DestinationHealth::Unknown { why } => format!("unknown — {why}"),
            },
            facts.degrade_after_days,
            if facts.health.is_degraded() {
                "; not consulted by the reclaim proof, alarmed every run"
            } else if matches!(facts.health, DestinationHealth::Unknown { .. }) {
                "; must-prove (an unreadable record may never become deletion permission)"
            } else {
                ""
            },
        ));
        // ④
        lines.push(format!(
            "    reclaim share  : {}",
            match &facts.reclaim_share {
                ReclaimShare::NotBlocking => "not blocking the weekly reclaim".to_string(),
                ReclaimShare::Blocking {
                    kind,
                    sessions,
                    bytes,
                    ..
                } => format!(
                    "blocking ({kind}) — {sessions} session(s) / {} held back",
                    fmt_bytes(*bytes)
                ),
                ReclaimShare::DegradedNotConsulted { health } => format!(
                    "degraded-not-consulted by the last recorded attempt (health then: {health})"
                ),
                ReclaimShare::UnknownRun { .. } => {
                    "the last recorded attempt ended before destinations were consulted".to_string()
                }
            }
        ));
    }
    // ADR-060 D5's stage-cap fact, in the table even when it is quiet in the
    // alerts: the measured basis is named so the number can be checked
    // against what it excludes (meta, machine logs).
    match &r.stage_cap {
        StageCapFact::NoStage => {}
        StageCapFact::Unmeasurable { why } => {
            lines.push(format!("  stage cap        : unknown — {why}"));
        }
        StageCapFact::Measured { bytes, cap } => {
            lines.push(format!(
                "  stage cap        : {} of shard body, cap {} — {}",
                fmt_bytes(*bytes),
                fmt_bytes(*cap),
                if bytes > cap {
                    "over (alarm only, never a stop)"
                } else {
                    "within"
                }
            ));
        }
    }
    if let ReclaimJobRecord::Present { attempt, age_secs } = &r.reclaim.record {
        lines.push(format!(
            "  weekly reclaim    : recorded {} (apply={}), {} ago, consecutive blocked: {}",
            attempt.outcome.slug(),
            attempt.apply,
            human_age(*age_secs),
            attempt.consecutive_blocked,
        ));
    }
    // The machine-level alert that a *missing* record cannot carry: with no
    // record at all, what `doctor` can still say is whether a job exists.
    let all = r.all_alerts();
    if all.is_empty() {
        lines.push("  alerts           : none".to_string());
    } else {
        for alert in &all {
            lines.push(alert.sentence.clone());
        }
        for alert in r.all_alerts().into_iter().filter(|a| a.fix.is_some()) {
            lines.push(format!(
                "    fix: {}",
                // reason: the filter selected exactly the alerts that carry a
                // fix, so the fallback is unreachable; the empty string is
                // only the type's stand-in, never a (missing) fix value printed.
                alert.fix.clone().unwrap_or_default()
            ));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialises tests that mutate process env vars — cargo runs tests in
    /// parallel threads and `set_var` is process-global. (Same pattern as
    /// config's own tests: `raw_env_option_refs` reads `config_path()`, which
    /// resolves through `XDG_CONFIG_HOME`.)
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A fixture home with one installed hourly unit pushing `storagebox`
    /// and a reclaim unit — the shape the machine this change was written
    /// for has (46-STAGE-GROWTH §2's two plists), built by the same
    /// renderer the product installs with rather than hand-written XML.
    fn plant_units(home: &Path, destination: Option<&str>) {
        let dir = home.join("Library/LaunchAgents");
        std::fs::create_dir_all(&dir).unwrap();
        let stage = home.join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let binary = home.join("bin/chat-stasher");
        let files = schedule::render(
            schedule::Unit::RunOnce,
            Format::Launchd,
            &binary,
            &stage,
            3600,
            &schedule::RunOnceArgs {
                destination: destination.map(str::to_string),
                ..Default::default()
            },
            &schedule::ReclaimStageArgs::default(),
            home,
        );
        for template in files {
            std::fs::write(dir.join(&template.name), &template.content).unwrap();
        }
        let reclaim_files = schedule::render(
            schedule::Unit::ReclaimStage,
            Format::Launchd,
            &binary,
            &stage,
            0,
            &schedule::RunOnceArgs::default(),
            &schedule::ReclaimStageArgs::default(),
            home,
        );
        for template in reclaim_files {
            std::fs::write(dir.join(&template.name), &template.content).unwrap();
        }
    }

    /// Point `config_path()` at <home>/.config under the env lock; the
    /// caller restores the old value.
    fn pin_config_home(home: &Path) -> std::ffi::OsString {
        let old =
            std::env::var_os("XDG_CONFIG_HOME").unwrap_or_else(|| std::ffi::OsString::from(""));
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        old
    }

    fn restore_config_home(old: &std::ffi::OsString) {
        if old.is_empty() {
            std::env::remove_var("XDG_CONFIG_HOME");
        } else {
            std::env::set_var("XDG_CONFIG_HOME", old);
        }
    }

    fn write_config(home: &Path, text: &str) {
        let dir = home.join(".config/chat-stasher");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }

    fn blocked_attempt(finished_at: u64) -> crate::reclaimstate::ReclaimAttempt {
        crate::reclaimstate::ReclaimAttempt {
            version: crate::reclaimstate::RECLAIM_STATE_VERSION,
            finished_at_unix: finished_at,
            apply: true,
            outcome: crate::reclaimstate::ReclaimOutcome::Blocked,
            blocked: vec![crate::reclaimstate::ReclaimBlock {
                destination: "r2".to_string(),
                kind: "unreachable".to_string(),
                error_kind: Some("credentials_missing_or_invalid".to_string()),
                sessions: 9_869,
                bytes: 12_900_000_000,
            }],
            destination_health: None,
            consecutive_blocked: 0,
            candidates: Some(9_869),
            candidate_bytes: Some(12_900_000_000),
        }
    }

    /// End to end through the one function: the machine's exact bad shape —
    /// two declared destinations, one hourly timer covering one of them, a
    /// reclaim timer, shell-only credentials on the uncovered one, a
    /// blocked reclaim record with the signature streak — produces the
    /// alerts §5.3 C1 requires, in the wording every surface shares.
    #[test]
    fn the_two_destination_bad_shape_produces_the_expected_alerts() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        write_config(
            home.path(),
            "[destinations.storagebox]\nrepo = \"opendal:sftp\"\noptions = { endpoint = \
             \"box.example:23\" }\n\n[destinations.r2]\nrepo = \"opendal:s3\"\noptions = { \
             access_key_id = \"env:CHAT_STASHER_R2_ACCESS_KEY_ID\" }\n",
        );
        plant_units(home.path(), Some("storagebox"));

        // The features a real machine racks up: storagebox pushed tonight;
        // r2 never on record; the reclaim blocked twice in a row because
        // r2 could not be read for credentials reasons.
        crate::pushstate::record_success(state.path(), "storagebox", "m", 12, 1_750_000_000)
            .unwrap();
        crate::reclaimstate::record(state.path(), &blocked_attempt(1_750_000_100)).unwrap();
        crate::reclaimstate::record(state.path(), &blocked_attempt(1_750_100_100)).unwrap();

        let config = crate::config::Config {
            destinations: [
                (
                    "storagebox".to_string(),
                    crate::config::DestinationConfig {
                        repo: Some("opendal:sftp".to_string()),
                        ..Default::default()
                    },
                ),
                (
                    "r2".to_string(),
                    crate::config::DestinationConfig {
                        repo: Some("opendal:s3".to_string()),
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let old_xdg = pin_config_home(home.path());
        let out = reconcile(&Inputs {
            config: &config,
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_100_200,
            scheduler_readable: true,
            stage: None,
        });
        restore_config_home(&old_xdg);

        let by_name = |name: &str| {
            out.destinations
                .iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
        };
        assert!(matches!(
            by_name("storagebox").push_coverage,
            PushCoverage::Covered { .. }
        ));
        assert!(matches!(
            by_name("r2").push_coverage,
            PushCoverage::NoScheduledPush
        ));
        assert!(matches!(
            by_name("storagebox").credentials,
            CredentialResolution::ShellIndependent
        ));
        assert!(matches!(
            by_name("r2").credentials,
            CredentialResolution::NotResolvableInTimer { .. }
        ));
        assert!(matches!(
            by_name("storagebox").last_push,
            LastPush::At { .. }
        ));
        assert!(matches!(by_name("r2").last_push, LastPush::NoRecord));
        assert!(matches!(
            by_name("r2").reclaim_share,
            ReclaimShare::Blocking { .. }
        ));

        let all = out.all_alerts();
        let sentences = all.iter().map(|a| a.sentence.as_str()).collect::<Vec<_>>();
        // The findings the review chose the wording for — and the exact
        // sentences `status` prints, `doctor` prints and `status --json`
        // hands the menu bar app, one wording for all three.
        assert!(
            sentences.iter().any(|s| s.contains(
                "[dest] r2: declared, but no installed launchd agent (~/Library/LaunchAgents) \
                 pushes it"
            )),
            "no push-coverage alert, worded against the directory the reader reads: {sentences:?}"
        );
        assert!(
            sentences
                .iter()
                .any(|s| s.contains("[dest] r2: credentials are shell-only")),
            "no shell-only-credentials alert: {sentences:?}"
        );
        assert!(
            sentences
                .iter()
                .any(|s| s.contains("[reclaim] blocked by `r2`")),
            "no per-destination block alert: {sentences:?}"
        );
        // The streak reaches two, which is the red line C2 counts from.
        let streak = all
            .iter()
            .find(|a| a.sentence.contains("[reclaim] the weekly stage clean-up"))
            .expect("streak alert");
        assert_eq!(streak.severity, Severity::Red);
        assert!(streak.sentence.contains("blocked 2 times in a row"));

        // The healthy destination contributes no alert of its own: silence
        // for a covered, resolvable, currently-unblocked row is what
        // `status` keeps on a clean machine.
        assert!(
            !sentences.iter().any(|s| s.contains("[dest] storagebox")),
            "a healthy row must not alarm: {sentences:?}"
        );
    }

    /// The single-destination shape: no destinations table, one row for the
    /// repository a destination-less timer pushes, and no alerts on a
    /// machine whose timer file exists. This is `start.md`'s machine, and
    /// it gets to stay quiet.
    #[test]
    fn single_destination_machine_with_a_timer_stays_quiet() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        plant_units(home.path(), None);
        let config = crate::config::Config::default();
        let out = reconcile(&Inputs {
            config: &config,
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_000_000,
            scheduler_readable: true,
            stage: None,
        });
        let [row] = out.destinations.as_slice() else {
            panic!("one row for the default repository: {:?}", out.destinations)
        };
        assert_eq!(row.name, "default");
        assert!(matches!(row.push_coverage, PushCoverage::Covered { .. }));
        assert!(matches!(row.last_push, LastPush::NoRecord));
        // No destinations declared, no reclaim timer facts to alarm on, no
        // push record yet: all silent.
        assert!(out.all_alerts().is_empty(), "{:?}", out.all_alerts());
    }

    /// C2's threshold is the streak alert's red line: one bad week is
    /// still yellow.
    #[test]
    fn a_first_blocked_week_is_yellow_not_red() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        plant_units(home.path(), None);
        let config = crate::config::Config::default();
        crate::reclaimstate::record(state.path(), &blocked_attempt(1_750_000_000)).unwrap();
        let out = reconcile(&Inputs {
            config: &config,
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_000_000,
            scheduler_readable: true,
            stage: None,
        });
        let streak = out
            .all_alerts()
            .iter()
            .find(|a| a.sentence.contains("blocked once in a row"))
            .cloned()
            .expect("a first blocked week carries the streak alert");
        assert_eq!(streak.severity, Severity::Yellow);
    }

    /// A config declaring two destinations, for the install-warning tests
    /// (the machine this change was written for has exactly this shape).
    fn config_with_two(first: &str, second: &str) -> crate::config::Config {
        crate::config::Config {
            destinations: [
                (
                    first.to_string(),
                    crate::config::DestinationConfig {
                        repo: Some("opendal:sftp".to_string()),
                        ..Default::default()
                    },
                ),
                (
                    second.to_string(),
                    crate::config::DestinationConfig {
                        repo: Some("opendal:s3".to_string()),
                        ..Default::default()
                    },
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        }
    }

    /// The `schedule install` warnings: installing an hourly timer for
    /// one of two declared destinations warns about the other, with the
    /// consequence spelled out — the weekly clean-up consults every
    /// declared destination, so the uncovered one keeps the stage from
    /// shrinking. And the destination the timer *does* now push, whose
    /// credentials are shell-only, gets its own warning: the installed
    /// file declares no environment, so its very first scheduled run
    /// cannot authenticate.
    #[test]
    fn run_once_install_warns_about_the_uncovered_and_the_shell_only() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        write_config(
            home.path(),
            "[destinations.storagebox]\nrepo = \"opendal:sftp\"\n\n[destinations.r2]\nrepo = \"opendal:s3\"\noptions = { access_key_id = \"env:CHAT_STASHER_R2_ACCESS_KEY_ID\" }\n",
        );
        // The just-finished install: an hourly timer for r2 only.
        plant_units(home.path(), Some("r2"));
        let old_xdg = pin_config_home(home.path());
        let warnings = install_warnings(
            &config_with_two("storagebox", "r2"),
            schedule::Unit::RunOnce,
            Format::Launchd,
            home.path(),
        );
        let joined = warnings.join("\n");
        assert!(
            joined.contains("no installed timer pushes declared destination(s): storagebox"),
            "the uncovered destination is named: {joined}"
        );
        assert!(
            joined.contains("the stage keeps every byte the healthy set has not proven"),
            "the consequence is stated in the health semantics: {joined}"
        );
        assert!(
            joined.contains(
                "destination r2 is pushed by a timer, but its credentials are shell-only"
            ),
            "the shell-only destination is named: {joined}"
        );
        assert!(
            joined.contains("re-run this install"),
            "the credentials warning says the fix is a re-install after re-wording the refs: {joined}"
        );

        // The reclaim-install half of the same finding: `schedule install
        // --unit reclaim-stage` warns about declared destinations no
        // hourly timer pushes, because the weekly job proves every
        // declared destination before deleting anything.
        let reclaim_warnings = install_warnings(
            &config_with_two("storagebox", "r2"),
            schedule::Unit::ReclaimStage,
            Format::Launchd,
            home.path(),
        );
        let reclaim_joined = reclaim_warnings.join("\n");
        assert!(
            reclaim_joined
                .contains("the weekly clean-up proves every HEALTHY declared destination"),
            "the reclaim install warns about what it consults (the healthy set): {reclaim_joined}"
        );
        assert!(
            reclaim_joined.contains("pushes: storagebox"),
            "the uncovered destination is named there too: {reclaim_joined}"
        );
        restore_config_home(&old_xdg);
    }

    /// The quiet half: an install that completes the machine's timer set
    /// warns about nothing — the newly-opinionated installer does not get
    /// to hum on a consistent machine.
    #[test]
    fn a_consistent_install_stays_quiet() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let config = crate::config::Config::default();
        assert!(
            install_warnings(
                &config,
                schedule::Unit::RunOnce,
                Format::Launchd,
                home.path()
            )
            .is_empty(),
            "a single-destination machine has no declared set to reconcile"
        );
    }

    /// An installed reclaim job that has not recorded a run in more than
    /// the grace window is the alarm words say it is — the silent-failure
    /// shape the log cannot cover, because the run that never starts
    /// writes nothing anywhere.
    #[test]
    fn a_stale_recorded_reclaim_job_is_red() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        plant_units(home.path(), None);
        let config = crate::config::Config::default();
        let mut stale = blocked_attempt(1_750_000_000);
        stale.finished_at_unix = 1_750_000_000 - 30 * 86_400;
        stale.outcome = crate::reclaimstate::ReclaimOutcome::Ok;
        stale.blocked.clear();
        crate::reclaimstate::record(state.path(), &stale).unwrap();
        let out = reconcile(&Inputs {
            config: &config,
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_000_000,
            scheduler_readable: true,
            stage: None,
        });
        let stale_alert = out
            .all_alerts()
            .iter()
            .find(|a| a.sentence.contains("has not recorded a run"))
            .cloned()
            .expect("staleness alert");
        assert_eq!(stale_alert.severity, Severity::Red);
    }

    /// H2 · A `Refused` attempt never surveyed the stage, so its hold-back
    /// is unknown — the record keeps `candidates` absent for exactly that
    /// reason. The streak alert must word the unknown instead of printing
    /// `0 session(s) / 0 B held back for ;` (a fabricated measurement plus a
    /// dangling list), and must not raise a second, milder alert that
    /// contradicts it: the refusal reason belongs in the one red line.
    #[test]
    fn a_refused_streak_words_its_unknown_backlog() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        plant_units(home.path(), None);
        let config = crate::config::Config::default();
        let refused = |finished_at: u64| crate::reclaimstate::ReclaimAttempt {
            version: crate::reclaimstate::RECLAIM_STATE_VERSION,
            finished_at_unix: finished_at,
            apply: true,
            outcome: crate::reclaimstate::ReclaimOutcome::Refused,
            blocked: Vec::new(),
            destination_health: None,
            consecutive_blocked: 0,
            candidates: None,
            candidate_bytes: None,
        };
        let state_dir = state.path();
        crate::reclaimstate::record(state_dir, &refused(1_750_000_000)).unwrap();
        crate::reclaimstate::record(state_dir, &refused(1_750_100_000)).unwrap();

        let out = reconcile(&Inputs {
            config: &config,
            format: Format::Launchd,
            home: home.path(),
            state_dir,
            now_unix: 1_750_100_200,
            scheduler_readable: true,
            stage: None,
        });
        let streak_alerts: Vec<&Alert> = out
            .job_alerts
            .iter()
            .filter(|a| a.sentence.contains("[reclaim] the weekly stage clean-up"))
            .collect();
        assert_eq!(
            streak_alerts.len(),
            1,
            "one job alert for the streak, not a contradicting second: {:?}",
            out.job_alerts
        );
        let alert = streak_alerts[0];
        assert_eq!(
            alert.severity,
            Severity::Red,
            "two failures in a row stay red even when the backlog is unknown"
        );
        assert!(
            alert.sentence.contains("blocked 2 times in a row"),
            "{}",
            alert.sentence
        );
        assert!(
            alert
                .sentence
                .contains("ended before the destinations were consulted"),
            "the refusal reason is the alert's own, not a second line's: {}",
            alert.sentence
        );
        assert!(
            !alert.sentence.contains("0 session"),
            "an unknown backlog is never a printed zero: {}",
            alert.sentence
        );
        assert!(
            !alert.sentence.contains("held back for ;"),
            "an empty destination list must not dangle: {}",
            alert.sentence
        );
    }

    /// M1, superseded shape kept deliberately · Fact ③ as ADR-060 D2 owns
    /// it: a destination with no push on record is **never-pushed** —
    /// degraded, alarmed on every run — whether or not a timer covers it,
    /// because the state is about the record, not the coverage. The alarm
    /// keeps the pre-ADR-060 round's absence wording (it names the version
    /// that started keeping the record, never "never pushed") and gains
    /// A2's two roads: repair the push *or* remove the destination from
    /// the config. The covered destination raises exactly one such alarm;
    /// the uncovered one keeps its coverage finding *and* raises its own
    /// degraded alarm — two first-order facts (① and ③), not a doubling.
    #[test]
    fn a_covered_destination_with_no_recorded_push_alerts_fact_three() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        write_config(
            home.path(),
            "[destinations.storagebox]\nrepo = \"opendal:sftp\"\n\n[destinations.r2]\nrepo = \
             \"opendal:s3\"\n",
        );
        // An hourly timer covers storagebox; nothing covers r2. No push
        // record has ever been written for either.
        plant_units(home.path(), Some("storagebox"));
        let old_xdg = pin_config_home(home.path());
        let out = reconcile(&Inputs {
            config: &config_with_two("storagebox", "r2"),
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_100_200,
            scheduler_readable: true,
            stage: None,
        });
        restore_config_home(&old_xdg);

        let all = out.all_alerts();
        let no_push_alerts: Vec<&Alert> = all
            .iter()
            .filter(|a| a.sentence.contains("[dest] storagebox: degraded"))
            .collect();
        assert_eq!(
            no_push_alerts.len(),
            1,
            "one degraded alarm for the covered-but-never-pushed destination: {:?}",
            all
        );
        let alert = no_push_alerts[0];
        assert_eq!(alert.severity, Severity::Yellow);
        assert!(
            alert
                .sentence
                .contains("started keeping the per-destination push record"),
            "the absence names the version and stays an absence, never 'never pushed': {}",
            alert.sentence
        );
        assert!(
            alert.sentence.contains("Two roads"),
            "A2: the alarm names both roads, never one: {}",
            alert.sentence
        );
        assert!(
            alert
                .sentence
                .contains("remove `[destinations.storagebox]` from the config"),
            "road two is the removal: {}",
            alert.sentence
        );
        // The uncovered destination keeps its coverage finding and raises
        // its own degraded alarm for the same absence — degradation is a
        // separate fact from coverage, and the degraded alarm no longer
        // waits for a timer to exist (D2: "alarming every run").
        assert!(
            all.iter()
                .any(|a| a.sentence.contains("[dest] r2: declared, but no installed")),
            "the coverage finding still speaks for the uncovered destination: {:?}",
            all
        );
        assert!(
            all.iter()
                .any(|a| a.sentence.contains("[dest] r2: degraded")),
            "the uncovered destination's degraded state speaks too: {:?}",
            all
        );
        // The machine facts themselves: both rows classify never-pushed
        // with no push record, whatever the coverage says.
        let r2 = out
            .destinations
            .iter()
            .find(|facts| facts.name == "r2")
            .expect("the uncovered row exists");
        assert_eq!(r2.health, DestinationHealth::NeverPushed);
        assert!(
            r2.health.is_degraded(),
            "never-pushed is a degraded state (not consulted by the proof)"
        );
        assert_eq!(r2.health.proof_role_word(), "degraded-not-consulted");
        let storagebox = out
            .destinations
            .iter()
            .find(|facts| facts.name == "storagebox")
            .expect("the covered row exists");
        assert_eq!(storagebox.health, DestinationHealth::NeverPushed);
        assert_eq!(
            storagebox.health.proof_role_word(),
            "degraded-not-consulted"
        );
        assert!(
            alert
                .fix
                .as_deref()
                .is_some_and(|fix| fix.contains("push --destination storagebox")),
            "the fix is the push that records the first success: {:?}",
            alert.fix
        );
    }

    /// M2 · ③'s unreadable arm, the regression half of the invariant fix:
    /// a `push-state.json` that exists but cannot be read must stay its own
    /// fact — `LastPush::Unknown` — and must speak as an alert, the way the
    /// directly analogous `ReclaimJobRecord::Unreadable` already does. Before
    /// this round the file's three read states collapsed to two at the alert
    /// level (Missing and Unreadable both went quiet), so a corrupt record was
    /// invisible in `status` until the next `push` printed its own warning.
    /// The test writes the file in the product's own shape, then corrupts it,
    /// and asserts the fact and the alert rather than trusting the commit.
    #[test]
    fn a_corrupt_push_state_record_is_worded_and_alerted() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        write_config(
            home.path(),
            "[destinations.storagebox]\nrepo = \"opendal:sftp\"\n",
        );
        plant_units(home.path(), Some("storagebox"));
        // First: a healthy record, so the covered destination's ③ reads
        // `At` and stays quiet — this half guards the fix against
        // re-collapsing unreadable into absent.
        crate::pushstate::record_success(state.path(), "storagebox", "m-unit", 42, 1_750_000_500)
            .unwrap();
        let old_xdg = pin_config_home(home.path());
        let healthy = reconcile(&Inputs {
            config: &config_with_two("storagebox", "r2"),
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_100_200,
            scheduler_readable: true,
            stage: None,
        });
        restore_config_home(&old_xdg);
        let row = healthy
            .destinations
            .iter()
            .find(|row| row.name == "storagebox")
            .expect("storagebox row");
        assert!(
            matches!(row.last_push, LastPush::At { .. }),
            "a readable record reads as At, not as absent: {:?}",
            row.last_push
        );
        assert!(
            healthy.all_alerts().iter().all(|a| !a
                .sentence
                .contains("the push-state record exists but is unreadable")),
            "a healthy record raises no unreadable alert: {:?}",
            healthy.all_alerts()
        );

        // Now corrupt the same file and reconcile again. The read state has
        // moved to Unreadable, and the fact must follow it — not fall back
        // to NoRecord, which would assert "no push is on record" out of a
        // parse that never happened.
        std::fs::write(
            state.path().join(crate::pushstate::PUSH_STATE_FILE),
            "not json",
        )
        .unwrap();
        let old_xdg = pin_config_home(home.path());
        let out = reconcile(&Inputs {
            config: &config_with_two("storagebox", "r2"),
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_100_200,
            scheduler_readable: true,
            stage: None,
        });
        restore_config_home(&old_xdg);
        let row = out
            .destinations
            .iter()
            .find(|row| row.name == "storagebox")
            .expect("storagebox row");
        assert!(
            matches!(row.last_push, LastPush::Unknown { .. }),
            "a corrupt record stays Unknown — never silently folded into NoRecord: {:?}",
            row.last_push
        );
        assert_eq!(
            destination_facts_json(row)["last_push"]["kind"],
            "unknown",
            "the --json fact keeps its unknown shape, worded: {}",
            destination_facts_json(row)["last_push"]
        );
        let all = out.all_alerts();
        let alerts: Vec<&Alert> = all
            .iter()
            .filter(|a| {
                a.sentence
                    .contains("[dest] storagebox: the push-state record exists but is unreadable")
            })
            .collect();
        assert_eq!(
            alerts.len(),
            1,
            "exactly one alert names storagebox's corrupt record, and it is yellow: {:?}",
            all
        );
        assert_eq!(alerts[0].severity, Severity::Yellow);
        assert!(
            alerts[0].sentence.contains("not ok, and not never pushed"),
            "the alert says what it is not, never the all-clear: {}",
            alerts[0].sentence
        );
        assert!(
            alerts[0]
                .fix
                .as_deref()
                .is_some_and(|fix| fix.contains("push --destination storagebox")),
            "the fix is the push that rebuilds the record: {:?}",
            alerts[0].fix
        );
        // The other half of the contract: an unreadable *platform* is still
        // not this. The platform's own unknowns (①'s and ②'s
        // `UnreadablePlatform`) are silent at the alert level even though
        // their facts are also unknown — the difference is that a corrupt
        // file is a local anomaly this machine can act on, while an
        // unreadable scheduler is the same on every machine on that
        // platform with nothing local to fix. The push-state file is
        // platform-independent, so a record this build read and found bad
        // is a finding on every platform, not a platform fact, and its
        // alert fires here too. What must *not* happen is the platform's
        // own unknown being laundered into a finding — that is the silence
        // this module keeps for `ReclaimShare::UnknownRun` and an
        // unobservable reclaim job, and it is the silence ① and ② hold to
        // here.
        let old_xdg = pin_config_home(home.path());
        let unreadable_platform = reconcile(&Inputs {
            config: &config_with_two("storagebox", "r2"),
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_100_200,
            scheduler_readable: false,
            stage: None,
        });
        restore_config_home(&old_xdg);
        let platform_alerts = unreadable_platform.all_alerts();
        assert!(
            !platform_alerts
                .iter()
                .any(|a| a.sentence.contains("could not be established")
                    && a.sentence.contains("no installed")),
            "an unreadable platform does not raise a coverage finding: {:?}",
            platform_alerts
        );
        assert!(
            !platform_alerts
                .iter()
                .any(|a| a.sentence.contains("could not be established")
                    && a.sentence.contains("shell-only")),
            "an unreadable platform does not raise a credentials finding: {:?}",
            platform_alerts
        );
        // The platform's facts keep their unknown shape in the table and
        // `--json` rather than collapsing to a measured negative — the
        // same premise the `doctor` test pins.
        let platform_row = unreadable_platform
            .destinations
            .iter()
            .find(|row| row.name == "storagebox")
            .expect("storagebox row");
        assert!(
            matches!(
                platform_row.push_coverage,
                PushCoverage::UnreadablePlatform { .. }
            ),
            "① stays unknown where the platform could not be read: {:?}",
            platform_row.push_coverage
        );
        assert_eq!(
            destination_facts_json(platform_row)["push_coverage"]["kind"],
            "unknown",
            "①'s kind in json stays unknown on an unreadable platform"
        );
        // ② rides the same platform arm as ① on a destination that
        // actually carries shell-only references — the M3 test pins that
        // arm on the same row. Here storagebox has none, so its ② is
        // shell-independent by measurement, which is the honest answer for
        // a credential form that resolves without any shell or timer: it
        // does not depend on reading the scheduler.
        assert_eq!(
            platform_row.credentials,
            CredentialResolution::ShellIndependent,
            "storagebox's ② is shell-independent, not unknown: {:?}",
            platform_row.credentials
        );
        // And the corrupt file's own alert still fires here: it is a
        // platform-independent read of a bad file, so it is a finding on
        // every platform, not a silence bought by an unreadable scheduler.
        assert!(
            platform_alerts.iter().any(|a| a
                .sentence
                .contains("[dest] storagebox: the push-state record exists but is unreadable")),
            "the corrupt record still speaks on an unreadable platform: {:?}",
            platform_alerts
        );
    }

    /// H3 · A platform whose scheduler this build has no reader for (the
    /// product documents a `schtasks` task for Windows and refuses `schedule
    /// install` there) must not read an empty systemd directory as "no
    /// timer pushes it" — for every declared destination, that silence is
    /// "could not be established", and the timer files planted under the
    /// other format's directory are exactly the ones the reader must not
    /// pretend it read. Record-based facts (③'s absence, the staleness and
    /// streak alerts) stay: those it can read.
    #[test]
    fn a_scheduler_this_build_cannot_read_does_not_claim_no_timer() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        // Files exist on disk; with the scheduler unreadable they must be
        // left unclaimed rather than mined from the wrong format's seat.
        plant_units(home.path(), Some("storagebox"));
        write_config(
            home.path(),
            "[destinations.storagebox]\nrepo = \"opendal:sftp\"\n\n[destinations.r2]\nrepo = \
             \"opendal:s3\"\noptions = { access_key_id = \
             \"env:CHAT_STASHER_R2_ACCESS_KEY_ID\" }\n",
        );
        let config = config_with_two("storagebox", "r2");
        let old_xdg = pin_config_home(home.path());
        let out = reconcile(&Inputs {
            config: &config,
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_100_200,
            scheduler_readable: false,
            stage: None,
        });
        restore_config_home(&old_xdg);

        for row in &out.destinations {
            assert!(
                matches!(row.push_coverage, PushCoverage::UnreadablePlatform { .. }),
                "coverage is unknown, not a known negative: {:?}",
                row.push_coverage
            );
        }
        assert!(
            matches!(
                out.destinations[0].credentials,
                CredentialResolution::UnreadablePlatform { .. }
            ),
            "② rides the same platform arm as ①: {:?}",
            out.destinations[0].credentials
        );
        // Both unknown arms are silent at the alert level — an unreadable
        // scheduler is not a per-machine finding, and the machine's panel
        // stays quiet the way `ReclaimShare::UnknownRun` and an
        // unobservable reclaim job do. The unknown is still worded in
        // `doctor`'s table and `--json` (see the next two assertions).
        assert!(
            out.all_alerts().iter().all(|a| {
                !a.sentence.contains("could not be established")
                    && !a.sentence.contains("no installed")
            }),
            "no coverage or credentials finding is raised against a scheduler \
             that was not read: {:?}",
            out.all_alerts()
        );
        // The reclaim-job facts stay quiet the way an unobservable job's
        // already do — and a *recorded* attempt still alerts, because the
        // state file is platform-independent. A machine whose scheduler this
        // build cannot read gets `None`: the empty unit list is a fact about
        // the reader, and `false` there would be the measured negative this
        // whole test exists to refuse.
        assert_eq!(
            out.reclaim.unit_installed, None,
            "no unit file was read, and the answer is not a guessed no: {:?}",
            out.reclaim
        );
        assert!(
            out.job_alerts.is_empty(),
            "a job whose scheduler cannot be read stays quiet, not guessed: {:?}",
            out.job_alerts
        );
        crate::reclaimstate::record(state.path(), &blocked_attempt(1_750_000_000)).unwrap();
        let old_xdg = pin_config_home(home.path());
        let out = reconcile(&Inputs {
            config: &config,
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_000_100,
            scheduler_readable: false,
            stage: None,
        });
        restore_config_home(&old_xdg);
        assert!(
            out.all_alerts()
                .iter()
                .any(|a| a.sentence.contains("blocked once in a row")),
            "record-based streaks alert regardless of the scheduler: {:?}",
            out.all_alerts()
        );
    }

    /// L1 · The raw `env:` re-read is a strictly-shaped second parse, and a
    /// config the product loads can still fail it. That failure is an
    /// unknown about which destinations carry shell-only references — never
    /// the answer "none of them do", which silently turns fact ② quiet and
    /// (on Windows, where an unescaped path fails the parse) was exactly
    /// the bug commit 277e4f17 worked around at the fixture level.
    #[test]
    fn an_unparsable_config_scan_is_unknown_not_no_env_refs() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        // `options` holds a nested table, which the product's `Config`
        // tolerates and this scan's strict `BTreeMap<String, String>` does
        // not — the file is readable and still fails the re-read.
        write_config(
            home.path(),
            "[destinations.storagebox]\nrepo = \"opendal:sftp\"\noptions = { nested = { deep = \
             \"table\" } }\n",
        );
        let config = config_with_two("storagebox", "r2");
        let old_xdg = pin_config_home(home.path());
        let out = reconcile(&Inputs {
            config: &config,
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_000_000,
            scheduler_readable: true,
            stage: None,
        });
        for name in ["storagebox", "r2"] {
            let row = out
                .destinations
                .iter()
                .find(|row| row.name == name)
                .expect("row");
            assert!(
                matches!(row.credentials, CredentialResolution::Unknown { .. }),
                "{name}'s fact ② is unknown, not silently shell-independent: {:?}",
                row.credentials
            );
        }
        assert!(
            out.all_alerts()
                .iter()
                .any(|a| a.sentence.contains("could not be re-read")),
            "the unreadable re-read is worded: {:?}",
            out.all_alerts()
        );
        // The install-time half keeps its honesty too: a scan that cannot
        // look says so rather than warning about nothing.
        let warnings = install_warnings(
            &config,
            schedule::Unit::RunOnce,
            Format::Launchd,
            home.path(),
        );
        restore_config_home(&old_xdg);
        assert!(
            warnings
                .iter()
                .any(|line| line.contains("could not be re-read")),
            "the install warning names what it cannot check: {warnings:?}"
        );
    }

    /// M3 · The Windows-silence contract's premise: the unknown that the
    /// coverage and credentials findings are dropped *in favour of* is
    /// actually worded in `doctor`'s table. Five comments state that premise
    /// (`destdiag.rs:114`, `:149`, `:750`, `:2358`; test `:260`, `:295`,
    /// `:405`), and nothing anywhere asserted it — `doctor_lines()` had no
    /// unit test for either unknown arm, and the w1012 suite never invokes
    /// `doctor`. A reader that stops wording the unknown would leave the
    /// silence defensible in prose and empty in fact, which is the worse
    /// direction: the comments would still say "it is worded in `doctor`'s
    /// table" and the table would say nothing. This test drives both unknown
    /// arms through the pure renderer and pins the wording, so the premise
    /// is a measurement rather than a comment.
    #[test]
    fn doctor_lines_words_both_unknown_arms() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        // ①'s Unknown arm: a unit file that exists but cannot be read. The
        // planted file is malformed on purpose — `installed_run_once_units`
        // records the parse failure, and `push_coverage_of` answers Unknown
        // rather than NoScheduledPush.
        plant_units(home.path(), Some("storagebox"));
        let unit_dir = home.path().join("Library/LaunchAgents");
        let planted = unit_dir
            .read_dir()
            .unwrap()
            .find(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("run-once")
            })
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(&planted, "<plist not xml at all").unwrap();
        // ②'s Unknown arm, on the *same* row: a config the product loads but
        // this scan's strict re-read cannot parse. The re-read answers at
        // all or it does not, so a file that fails it leaves *every* row's
        // credentials unknown — including storagebox's, which is what this
        // test pins. The failure shape is the nested inline table the other
        // test uses (`options = { nested = { deep = "table" } }`), which
        // `Config` tolerates and this scan's `BTreeMap<String, String>` does
        // not. One config, one row carrying both unknowns, so the two arms
        // are tested on the same destination rather than on two different
        // machines.
        write_config(
            home.path(),
            "[destinations.storagebox]\nrepo = \"opendal:sftp\"\n\
             options = { nested = { deep = \"table\" } }\n",
        );
        let old_xdg = pin_config_home(home.path());
        let out = reconcile(&Inputs {
            config: &config_with_two("storagebox", "r2"),
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_100_200,
            scheduler_readable: true,
            stage: None,
        });
        restore_config_home(&old_xdg);
        let table = doctor_lines(&out);

        let storagebox = table
            .iter()
            .find(|line| line.trim() == "storagebox")
            .expect("storagebox header");
        let idx = table.iter().position(|line| line == storagebox).unwrap();
        // ①'s unknown is worded in the row right under the header, never
        // collapsed to "no installed launchd agent names this destination".
        assert!(
            table[idx + 1].contains("timer push     : unknown"),
            "①'s unreadable unit is worded as unknown in doctor's table, \
             not read as no timer: {}",
            table[idx + 1]
        );
        assert!(
            !table[idx + 1].contains("no installed"),
            "the unknown arm must not launder into a measured negative: {}",
            table[idx + 1]
        );
        // ②'s unknown is worded in the same row's credentials line.
        assert!(
            table[idx + 2].contains("timer creds    : unknown"),
            "②'s unreadable re-read is worded as unknown in doctor's table: {}",
            table[idx + 2]
        );
        // Both unknowns reach the `--json` layer in the same tagged shape,
        // which is what the menu bar app reads — the premise holds there
        // too, not only in the human table.
        let json = destinations_json(&out);
        // The rows come out sorted (`r2` first), so the assertion must
        // name the row it means rather than trusting its index.
        let storagebox_json = json["destinations"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["name"] == "storagebox"))
            .expect("storagebox row in --json");
        assert_eq!(
            storagebox_json["push_coverage"]["kind"], "unknown",
            "①'s kind in json: {}",
            storagebox_json["push_coverage"]
        );
        assert_eq!(
            storagebox_json["credentials"]["kind"], "unknown",
            "②'s kind in json: {}",
            storagebox_json["credentials"]
        );
        // And the alert level: a local anomaly with a fix still speaks, so
        // the wording is not only in the quiet table. Both anomalies raise
        // exactly one alert each, and the alerts are worded against what
        // was read rather than claiming the all-clear.
        let alerts = out.all_alerts();
        assert!(
            alerts
                .iter()
                .any(|a| a.sentence.contains("could not be read")),
            "①'s anomaly still raises its alert: {:?}",
            alerts
        );
        assert!(
            alerts
                .iter()
                .any(|a| a.sentence.contains("could not be re-read")),
            "②'s anomaly still raises its alert: {:?}",
            alerts
        );
    }
    /// W964, ② for the pull producer: a declared `[pull.letta]` without an
    /// `api_key` is the same yellow trap a shell-only destination
    /// credential is — one sentence, the same one the install warning
    /// prints — while a declared credential or no declaration at all
    /// stays quiet.
    #[test]
    fn a_declared_producer_without_an_api_key_gets_the_credential_alert() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let declared = |api_key: Option<&str>| crate::config::Config {
            pull: Some(crate::config::PullSectionConfig {
                letta: Some(crate::config::LettaPullConfig {
                    account_id: Some("synthetic-account".to_string()),
                    inbox: Some("/inbox".to_string()),
                    api_key: api_key.map(str::to_owned),
                    ..crate::config::LettaPullConfig::default()
                }),
            }),
            ..Default::default()
        };
        fn inputs<'a>(
            config: &'a crate::config::Config,
            home: &'a Path,
            state: &'a Path,
        ) -> Inputs<'a> {
            Inputs {
                config,
                format: Format::Launchd,
                home,
                state_dir: state,
                now_unix: 1_750_000_000,
                scheduler_readable: false,
                stage: None,
            }
        }
        let environment_only = reconcile(&inputs(&declared(None), home.path(), state.path()));
        assert_eq!(
            environment_only.pull_producer,
            Some(PullProducerCredentials::EnvironmentOnly)
        );
        assert!(
            environment_only
                .all_alerts()
                .iter()
                .any(|alert| alert.sentence.contains("pull.letta.api_key is not set")),
            "the shell-only producer credential speaks: {:?}",
            environment_only.all_alerts()
        );
        assert!(
            environment_only
                .all_alerts()
                .iter()
                .any(|alert| alert.sentence.contains("file:")),
            "the alert names the timer-reachable forms"
        );
        // The install warning carries the same sentence, and it fires with
        // no destinations declared at all.
        let warnings = install_warnings(
            &declared(None),
            schedule::Unit::RunOnce,
            Format::Launchd,
            home.path(),
        );
        assert!(
            warnings
                .iter()
                .any(|line| line.contains("pull.letta.api_key is not set")),
            "the install-time warning says the same thing: {warnings:?}"
        );
        // A declared credential is quiet everywhere.
        let declared_key = reconcile(&inputs(
            &declared(Some("synthetic-resolved-secret")),
            home.path(),
            state.path(),
        ));
        assert_eq!(
            declared_key.pull_producer,
            Some(PullProducerCredentials::Declared)
        );
        assert!(
            !declared_key
                .all_alerts()
                .iter()
                .any(|alert| alert.sentence.contains("pull.letta")),
            "a declared credential is not a finding"
        );
        assert!(
            install_warnings(
                &declared(Some("synthetic-resolved-secret")),
                schedule::Unit::RunOnce,
                Format::Launchd,
                home.path(),
            )
            .is_empty(),
            "install says nothing when the producer's credential is declared"
        );
        // No producer declared: no fact, no alert, no warning.
        let undeclared = reconcile(&inputs(
            &crate::config::Config::default(),
            home.path(),
            state.path(),
        ));
        assert_eq!(undeclared.pull_producer, None);
        assert!(
            !undeclared
                .all_alerts()
                .iter()
                .any(|alert| alert.sentence.contains("pull.letta")),
            "an undeclared producer is nobody's finding"
        );
    }

    /// A declared producer shared by more than one destination timer is
    /// raced every hour: one pass wins the producer's state lock and the
    /// others defer. The install warns exactly once, at the moment the user
    /// is looking, that the deferred status line they will see is that
    /// hand-off — not an unread failure — while a single timer owns its
    /// producer uncontended and stays quiet.
    #[test]
    fn a_declared_producer_across_multiple_destination_timers_is_explained_at_install() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        plant_units(home.path(), Some("storagebox"));
        plant_units(home.path(), Some("r2"));
        let destination = |repo: &str| crate::config::DestinationConfig {
            repo: Some(repo.to_string()),
            ..Default::default()
        };
        let producer = || crate::config::PullSectionConfig {
            letta: Some(crate::config::LettaPullConfig {
                account_id: Some("synthetic-account".to_string()),
                api_key: Some("synthetic-resolved-secret".to_string()),
                ..crate::config::LettaPullConfig::default()
            }),
        };
        let both = crate::config::Config {
            pull: Some(producer()),
            destinations: [
                ("storagebox".to_string(), destination("opendal:sftp")),
                ("r2".to_string(), destination("opendal:s3")),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let warnings =
            install_warnings(&both, schedule::Unit::RunOnce, Format::Launchd, home.path());
        let joined = warnings.join("\n");
        assert!(
            joined.contains("share one producer state"),
            "the race the timers create is named: {joined}"
        );
        assert!(
            joined.contains("Letta pull deferred"),
            "the deferral status line is explained before it is met: {joined}"
        );
        assert!(
            joined.contains("run-once exits stay clean"),
            "the deferral is said to be a hand-off, not a failure: {joined}"
        );
        // One timer, one destination: nobody contends, so the warning does
        // not fire — the mechanism only speaks for the combination that
        // produces deferred passes. The destination is an sftp one, which
        // asks no scheduler-invisible environment variable, so the quiet
        // assertion isolates exactly the producer-timer combination.
        let home = tempfile::tempdir().unwrap();
        plant_units(home.path(), Some("storagebox"));
        let one = crate::config::Config {
            pull: Some(producer()),
            destinations: [("storagebox".to_string(), destination("opendal:sftp"))]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let single_warnings =
            install_warnings(&one, schedule::Unit::RunOnce, Format::Launchd, home.path());
        assert!(
            single_warnings.is_empty(),
            "an uncontended producer timer warns about nothing: {single_warnings:?}"
        );
    }

    // ------------------------------------------- ADR-060 D2/D5/A1 (SG-2) unit pins

    /// The classifier's four states from the three record states (D2),
    /// with the boundary decided 10-22: a push *exactly* on the threshold
    /// is within it, and degrading means strictly older. Parameterised by
    /// `now`, so the whole table is assertable without waiting.
    #[test]
    fn the_four_health_states_classify_from_the_record() {
        let at = |secs_ago: u64| LastPush::At {
            finished_at_unix: 1_750_000_000 - secs_ago,
            snapshots_in_repo: 1,
        };
        let now = 1_750_000_000;
        let seven_days = 7 * 86_400;
        assert_eq!(
            classify_destination_health(&at(0), 7, now),
            DestinationHealth::Healthy,
            "a push this instant is healthy"
        );
        assert_eq!(
            classify_destination_health(&at(seven_days), 7, now),
            DestinationHealth::Healthy,
            "a push exactly on the threshold is within it — strictly older degrades"
        );
        assert_eq!(
            classify_destination_health(&at(seven_days + 1), 7, now),
            DestinationHealth::Lagging,
            "one second past the threshold degrades"
        );
        assert_eq!(
            classify_destination_health(&LastPush::NoRecord, 7, now),
            DestinationHealth::NeverPushed
        );
        assert_eq!(
            classify_destination_health(&LastPush::NoRecord, 35, now),
            DestinationHealth::NeverPushed,
            "an absence stays never-pushed whatever the threshold — the override never \
             manufactures a record"
        );
        assert_eq!(
            classify_destination_health(
                &LastPush::Unknown {
                    why: "malformed json".to_string(),
                },
                7,
                now
            ),
            DestinationHealth::Unknown {
                why: "malformed json".to_string()
            },
            "an unreadable record is its own state — never lagging, never healthy"
        );
    }

    /// A1's fixture (the review's mandatory edit): a destination declaring
    /// `degrade_after_days = 35` whose last successful push is 20 days old
    /// classifies **healthy** — inside the must-prove set, no degraded
    /// alarm — while its sibling on the global window classifies lagging on
    /// the same 20 days. The override follows the declared rhythm only; it
    /// never removes protection, and past its own threshold it degrades
    /// exactly like the global rule.
    #[test]
    fn a_thirty_five_day_override_keeps_a_twenty_day_push_healthy() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        write_config(
            home.path(),
            "[destinations.monthly-disk]\nrepo = \"opendal:sftp\"\ndegrade_after_days = 35\n\n\
             [destinations.hourly-box]\nrepo = \"opendal:s3\"\n\n[reclaim]\ndegrade_after_days = 7\n",
        );
        plant_units(home.path(), None);
        // Both destinations were last pushed 20 days ago; the machine's
        // `cmd_push` would have written these rows.
        let twenty_days_ago = 1_750_000_000 - 20 * 86_400;
        crate::pushstate::record_success(&state.path(), "monthly-disk", "m", 3, twenty_days_ago)
            .unwrap();
        crate::pushstate::record_success(&state.path(), "hourly-box", "m", 3, twenty_days_ago)
            .unwrap();
        // A config parsed from the same text the machine would write, so
        // the override reading is the loader's, not the test's assertion.
        let raw =
            std::fs::read_to_string(home.path().join(".config/chat-stasher/config.toml")).unwrap();
        let config: Config = toml::from_str(&raw).unwrap();
        assert_eq!(
            config.destinations["monthly-disk"].degrade_after_days,
            Some(35),
            "the override key parses on its own destination"
        );

        let old_xdg = pin_config_home(home.path());
        let out = reconcile(&Inputs {
            config: &config,
            format: Format::Launchd,
            home: home.path(),
            state_dir: state.path(),
            now_unix: 1_750_000_000,
            scheduler_readable: false,
            stage: None,
        });
        restore_config_home(&old_xdg);

        let monthly = out
            .destinations
            .iter()
            .find(|facts| facts.name == "monthly-disk")
            .expect("the override destination has a row");
        assert_eq!(monthly.health, DestinationHealth::Healthy);
        assert_eq!(monthly.health.proof_role_word(), "must-prove");
        assert_eq!(
            monthly.degrade_after_days, 35,
            "the row carries the effective window it was judged against"
        );
        let hourly = out
            .destinations
            .iter()
            .find(|facts| facts.name == "hourly-box")
            .expect("the global-window destination has a row");
        assert_eq!(hourly.health, DestinationHealth::Lagging);
        assert_eq!(hourly.degrade_after_days, 7);
        // And the two roads: the lagging sibling's alarm names both, the
        // override destination stays silent — a destination on its
        // declared rhythm is not a finding.
        let sentences = out
            .all_alerts()
            .iter()
            .map(|a| a.sentence.clone())
            .collect::<Vec<_>>();
        assert!(
            !sentences
                .iter()
                .any(|s| s.contains("[dest] monthly-disk: degraded")),
            "no degraded alarm for a destination inside its own rhythm: {sentences:?}"
        );
        let lagging_alarm = sentences
            .iter()
            .find(|s| s.contains("[dest] hourly-box: degraded"))
            .expect("the global-window sibling alarms on the same 20 days");
        assert!(
            lagging_alarm.contains("Two roads")
                && lagging_alarm.contains("remove `[destinations.hourly-box]` from the config"),
            "A2: the lagging alarm names both roads: {lagging_alarm}"
        );
        drop(home);
        drop(state);
    }

    /// D2's unknown state is never read as healthy anywhere the surfaces
    /// look: the `--json` health kind stays `unknown` with its `why`, and
    /// the alert that speaks for it is the W1012g one (already covered by
    /// `a_corrupt_push_state_record_is_worded_and_alerted`).
    #[test]
    fn an_unreadable_record_stays_unknown_in_the_json_and_always_must_proves() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        write_config(home.path(), "[destinations.r2]\nrepo = \"opendal:s3\"\n");
        let state_dir = state.path();
        std::fs::create_dir_all(state_dir).unwrap();
        // A record a future build wrote: an unknown version is unreadable,
        // never a healthy record.
        std::fs::write(
            crate::pushstate::push_state_path(state_dir),
            r#"{ "version": 99, "pushes": {} }"#,
        )
        .unwrap();
        let config = Config {
            destinations: [(
                "r2".to_string(),
                crate::config::DestinationConfig {
                    repo: Some("opendal:s3".to_string()),
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };
        let old_xdg = pin_config_home(home.path());
        let out = reconcile(&Inputs {
            config: &config,
            format: Format::Launchd,
            home: home.path(),
            state_dir,
            now_unix: 1_750_000_000,
            scheduler_readable: false,
            stage: None,
        });
        restore_config_home(&old_xdg);
        let r2 = &out.destinations[0];
        assert_eq!(
            r2.health,
            DestinationHealth::Unknown {
                why: "unknown version 99".to_string()
            }
        );
        assert!(
            !r2.health.is_degraded(),
            "unknown is not degraded — it is must-prove (fail-safe)"
        );
        assert_eq!(r2.health.proof_role_word(), "must-prove");
        // The `--json` vocabulary is D2's four states verbatim; the
        // unknown arm carries its why.
        let json = destinations_json(&out);
        let health = &json["destinations"][0]["health"];
        assert_eq!(health["kind"], "unknown");
        assert!(health["why"]
            .as_str()
            .unwrap()
            .contains("unknown version 99"));
    }

    /// D5's size basis: the cap walk counts the sessions tree's file bytes,
    /// stat-only — `meta/` never enters the number, and a stage that holds
    /// no sessions tree at all is a real zero, not an unknown.
    #[test]
    fn the_cap_walk_counts_session_body_only() {
        let dir = tempfile::TempDir::new().unwrap();
        let stage = dir.path().join("stage");
        let sessions = stage.join("sessions/m-unit/s-one");
        std::fs::create_dir_all(&sessions).unwrap();
        let body_bytes: u64 = 3 * 1024;
        std::fs::write(
            sessions.join("000001.jsonl"),
            vec![b'x'; body_bytes as usize],
        )
        .unwrap();
        // A bucketed shard in a nested bucket counts too.
        let bucket = sessions.join("0002");
        std::fs::create_dir_all(&bucket).unwrap();
        std::fs::write(bucket.join("000002.jsonl"), vec![b'y'; 512]).unwrap();
        // Everything the cap must exclude: meta, machine logs, ext status.
        let meta = stage.join("meta/m-unit");
        std::fs::create_dir_all(&meta).unwrap();
        std::fs::write(meta.join("machine.json"), vec![b'm'; 4096]).unwrap();
        std::fs::create_dir_all(stage.join("ext-status")).unwrap();
        std::fs::write(stage.join("ext-status/report.json"), vec![b'e'; 256]).unwrap();
        let walked = stage_shard_body_bytes(&stage).unwrap();
        assert_eq!(
            walked,
            body_bytes + 512,
            "the sessions tree only — meta and ext-status never count: {walked}"
        );
        assert_eq!(
            stage_shard_body_bytes(&dir.path().join("nowhere")).unwrap_or(0),
            0,
            "an absent sessions tree is a real zero: the stage holds no body"
        );
        // And the refusal-shape fixtures stay honest through the fact: a
        // missing stage directory is a zero, a *file* where the tree goes
        // is an error, never a laundered number.
        assert!(stage_shard_body_bytes(&stage.join("sessions/m-unit/s-one/000001.jsonl")).is_err());
    }

    /// D5's alarm-only rule, end to end through `reconcile`: a stage over
    /// the effective cap raises exactly one red job alert with the
    /// measured bytes, the cap and the advice — and nothing else in the
    /// reconciliation changes: under the cap no alert fires, an
    /// unconfigured stage stays quiet, and a stage that cannot be walked
    /// is an unknown that does not read as zero.
    #[test]
    fn an_over_cap_stage_alarms_red_and_changes_nothing_else() {
        let stage_dir = tempfile::TempDir::new().unwrap();
        let stage = stage_dir.path().join("stage");
        let sessions = stage.join("sessions/m-unit/s-one");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("000001.jsonl"), vec![b'x'; 2048]).unwrap();

        let run = |stage: Option<&Path>, cap: Option<&str>| {
            let config: Config = match cap {
                // reason: a cap unset at the call site means the config
                // has no [reclaim] section at all — D5's documented
                // default (10 GiB) is what the fact reports, the same
                // effective value the CLI hands the run.
                None => Config::default(),
                Some(cap) => {
                    toml::from_str(&format!("[reclaim]\nstage_cap_bytes = \"{cap}\"\n")).unwrap()
                }
            };
            reconcile(&Inputs {
                config: &config,
                format: Format::Launchd,
                home: Path::new("/nonexistent-home-for-this-fixture"),
                state_dir: Path::new("/nonexistent-state-for-this-fixture"),
                now_unix: 1_750_000_000,
                scheduler_readable: false,
                stage,
            })
        };

        // Over a 1 KiB configured cap: one red job alert, carrying the
        // measured bytes, the cap, and the alarm-only promise.
        let out = run(Some(&stage), Some("1KiB"));
        let cap_alerts: Vec<&Alert> = out
            .job_alerts
            .iter()
            .filter(|alert| alert.sentence.contains("over the configured cap"))
            .collect();
        assert_eq!(
            cap_alerts.len(),
            1,
            "exactly one cap alarm, nothing about it doubles: {:?}",
            out.job_alerts
        );
        let alert = cap_alerts[0];
        assert_eq!(alert.severity, Severity::Red, "over the cap is red (D5)");
        assert!(
            alert.sentence.contains("2.0 KiB")
                && alert.sentence.contains("1.0 KiB")
                && alert.sentence.contains("This is an alarm, not a block"),
            "measured bytes, cap and the alarm-only promise, in one wording: {}",
            alert.sentence
        );
        assert!(
            alert
                .fix
                .as_deref()
                .is_some_and(|fix| fix.contains("reclaim-stage")),
            "the fix command is the dry reclaim that names what holds the stage: {:?}",
            alert.fix
        );

        // Under the same cap: quiet — an in-bounds stage is not a finding.
        std::fs::write(sessions.join("000001.jsonl"), vec![b'x'; 64]).unwrap();
        let out = run(Some(&stage), Some("1KiB"));
        assert!(
            out.job_alerts.is_empty(),
            "under the cap, no job alert at all: {:?}",
            out.job_alerts
        );

        // Over D5's *default* 10 GiB cap with no [reclaim] section: a
        // 2 KiB stage is within it — the alarm does not fire on the
        // default for a small stage.
        let out = run(Some(&stage), None);
        assert!(
            out.job_alerts.is_empty(),
            "the 10 GiB default does not alarm a 64-byte stage: {:?}",
            out.job_alerts
        );
        // And the same stage through the JSON: measured, within, the cap
        // named — so the menubar reads the fact the surfaces print.
        let json = destinations_json(&out);
        assert_eq!(json["stage_cap"]["kind"], "measured");
        assert_eq!(json["stage_cap"]["cap"], DEFAULT_STAGE_CAP_BYTES);

        // An unconfigured stage ([native_host] stage absent): its own
        // state, quiet everywhere.
        let out = run(None, Some("1KiB"));
        assert!(out.job_alerts.is_empty());
        assert_eq!(out.stage_cap, StageCapFact::NoStage);

        // A configured stage that cannot be walked: an unknown with the
        // reason, quiet in the alerts and never a zero.
        let out = run(
            Some(Path::new("/nonexistent-stage-root-for-this-fixture")),
            Some("1KiB"),
        );
        assert!(
            out.job_alerts.is_empty(),
            "an unmeasurable stage does not alarm — its bytes are unknown: {:?}",
            out.job_alerts
        );
        assert!(
            matches!(&out.stage_cap, StageCapFact::Unmeasurable { why } if why.contains("could not be walked")),
            "the unmeasurable case words its why: {:?}",
            out.stage_cap
        );
        let json = destinations_json(&out);
        assert_eq!(json["stage_cap"]["kind"], "unknown");
    }
}
