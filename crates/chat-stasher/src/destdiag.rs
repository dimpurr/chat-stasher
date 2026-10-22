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
/// destination.
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
    /// The latest attempt ended before consulting destinations.
    UnknownRun { outcome: String },
}

/// One declared destination and its four facts.
#[derive(Debug, Clone)]
pub struct DestinationFacts {
    pub name: String,
    pub push_coverage: PushCoverage,
    pub credentials: CredentialResolution,
    pub last_push: LastPush,
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

/// The whole reconciliation: rows plus the alerts both prose surfaces and
/// the menu bar display. `alerts` and `job_alerts` are already worded —
/// the three surfaces display them unchanged, which is the shared-
/// wording half of C1; the fact enums beside them are what `--json`
/// consumers and `doctor`'s table read.
#[derive(Debug, Clone)]
pub struct Reconciliation {
    pub destinations: Vec<DestinationFacts>,
    pub reclaim: ReclaimJobFacts,
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
        // read that never happened.
        let last_push = match &push_records {
            crate::pushstate::PushStateRead::Present(state) => match state.pushes.get(name) {
                Some(record) => LastPush::At {
                    finished_at_unix: record.finished_at_unix,
                    snapshots_in_repo: record.snapshots_in_repo,
                },
                None => LastPush::NoRecord,
            },
            crate::pushstate::PushStateRead::Unreadable(why) => {
                LastPush::Unknown { why: why.clone() }
            }
            crate::pushstate::PushStateRead::Missing => LastPush::NoRecord,
        };
        // ④ from the latest recorded reclaim attempt.
        let reclaim_share = match &reclaim_read {
            crate::reclaimstate::ReclaimStateRead::Present(attempt) => match attempt.outcome {
                crate::reclaimstate::ReclaimOutcome::Ok => ReclaimShare::NotBlocking,
                crate::reclaimstate::ReclaimOutcome::Blocked => attempt
                    .blocked
                    .iter()
                    .find(|block| &block.destination == name)
                    .map(|block| ReclaimShare::Blocking {
                        kind: block.kind.to_string(),
                        error_kind: block.error_kind.clone(),
                        sessions: block.sessions,
                        bytes: block.bytes,
                    })
                    .unwrap_or(ReclaimShare::NotBlocking),
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
            reclaim_share,
        });
    }

    let reclaim = reclaim_job_facts(
        &reclaim_units,
        &reclaim_read,
        inputs.now_unix,
        inputs.scheduler_readable,
    );

    let reconciliation = Reconciliation {
        destinations,
        reclaim,
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
    let format = reconciliation.format;

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
                         pushes it — a push reaches it only when a command names it by hand; the \
                         weekly stage clean-up consults every declared destination, so the \
                         stage keeps growing until `{name}` holds a copy or the declaration is \
                         removed",
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
        // ③ the covered-but-never-successfully-pushed destination (§4.7's
        // fixture shape): the machinery looks right — a timer covers it,
        // the credentials resolve — yet no push has been recorded to have
        // succeeded. Before this arm, ③ surfaced only in `--json` facts
        // and `doctor`'s table, never as an alert, so `status` stayed
        // green until a weekly reclaim happened to block on the gap. An
        // uncovered destination does not get this alert: its coverage
        // finding already speaks, and the push that follows the fix is
        // the same sentence's happy ending.
        if declared_row
            && matches!(facts.push_coverage, PushCoverage::Covered { .. })
            && matches!(facts.last_push, LastPush::NoRecord)
        {
            reconciliation.alerts.push(Alert {
                severity: Severity::Yellow,
                sentence: format!(
                    "[dest] {}: {} — the timer covers it, and until a push succeeds no copy \
                     beyond this machine is known to exist; the first successful push ends the \
                     finding",
                    facts.name,
                    no_record_why(&facts.name),
                ),
                fix: Some(format!(
                    "chat-stasher push --destination {} --stage <stage>   # a push by hand now \
                     records the first success",
                    facts.name
                )),
            });
        }
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
                         the stage; nothing is reclaimed until every declared destination answers",
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
            // A `Refused` attempt ended before the destinations were
            // consulted, so nothing about the stage was measured — the
            // record keeps `candidates` absent for exactly that reason,
            // and the alert must not launder the refusal's empty
            // `blocked` list into `0 session(s) / 0 B held back for ;`: a
            // fabricated measurement with a dangling list. Counts print
            // only for the outcome that measured them, and the refusal
            // reason lives in this one line rather than in a second,
            // milder alert that would contradict its own severity.
            let held_back = if attempt.blocked.is_empty() {
                "what it holds back is unknown — the attempt ended before the destinations \
                 were consulted"
                    .to_string()
            } else {
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
            ReclaimJobRecord::Unreadable(why) => reconciliation.job_alerts.push(Alert {
                severity: Severity::Yellow,
                sentence: format!(
                    "[reclaim] the reclaim record exists but is unreadable ({why}), so what the \
                     weekly job last did is unknown — not ok, and not never-run"
                ),
                fix: None,
            }),
            _ => {}
        }
    }

    reconciliation
}

/// Coarse, honest duration wording — the same bands `runstate::human_age`
/// keeps, restated here so this module owns all of its sentences.
fn human_age(secs: u64) -> String {
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
/// Two findings warn, one per install shape:
///
/// * **run-once install** — a declared destination no installed hourly
///   timer pushes: only a command naming it by hand reaches it, and the
///   weekly stage clean-up consults every declared destination and holds
///   every byte until they answer, so one of these keeps the stage from
///   ever shrinking. Also: a destination the timers *now* push whose
///   credentials are `env:NAME` — the files this command writes declare
///   no environment, so the very first scheduled run cannot authenticate.
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
    // Single-destination machines have no declared set to reconcile — the
    // one timer they just installed pushes the default repository, and
    // nothing could be "declared but uncovered".
    if declared.is_empty() {
        return Vec::new();
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
            let mut lines = Vec::new();
            if !missing.is_empty() {
                lines.push(format!(
                    "schedule install: WARNING ⚠ no installed timer pushes declared \
                     destination(s): {}",
                    missing.join(", ")
                ));
                lines.push(
                    "schedule install:   A push reaches them only when a command names them by \
                     hand, and the weekly stage clean-up consults every declared destination and \
                     holds every staged byte until they answer — so one of these keeps the \
                     stage from ever shrinking."
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
                    "schedule install: WARNING ⚠ the weekly clean-up proves EVERY declared \
                     destination before it deletes anything, and no installed hourly timer \
                     pushes: {}",
                    missing.join(", ")
                ),
                "schedule install:   It will consult every declared destination each week, and \
                 an unreachable or never-pushed one blocks the whole reclamation: nothing is \
                 deleted and the stage keeps growing. `chat-stasher status` reports the \
                 blockage and its reason."
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
        "reclaim": reclaim_json(r),
    })
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
        "reclaim_share": match &facts.reclaim_share {
            ReclaimShare::NotBlocking => serde_json::json!({ "kind": "not_blocking" }),
            ReclaimShare::Blocking { kind, error_kind, sessions, bytes } => serde_json::json!({
                "kind": "blocking",
                "block_kind": kind,
                "error_kind": error_kind,
                "sessions": sessions,
                "bytes": bytes,
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
                ReclaimShare::UnknownRun { .. } => {
                    "the last recorded attempt ended before destinations were consulted".to_string()
                }
            }
        ));
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
            joined.contains("stage from ever shrinking"),
            "the consequence is stated: {joined}"
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
            reclaim_joined.contains("the weekly clean-up proves EVERY declared destination"),
            "the reclaim install warns about what it consults: {reclaim_joined}"
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

    /// M1 · Fact ③, the coverage-shaped gap §4.7's fixture is built on: the
    /// timer covers the destination and yet no push has ever been recorded
    /// against it. Before this, ③ surfaced only in `--json` facts and
    /// `doctor`'s table — never as an alert — so `status` and the panel
    /// stayed green on exactly the machine the reconciliation exists for.
    /// An uncovered destination does not get the alert: its coverage
    /// finding already speaks, and the §5.3 rule is a finding per fact,
    /// not a doubling per destination.
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
        });
        restore_config_home(&old_xdg);

        let all = out.all_alerts();
        let no_push_alerts: Vec<&Alert> = all
            .iter()
            .filter(|a| a.sentence.contains("[dest] storagebox: no push"))
            .collect();
        assert_eq!(
            no_push_alerts.len(),
            1,
            "one alert for the covered-but-never-pushed destination: {:?}",
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
            all.iter()
                .all(|a| !a.sentence.contains("[dest] r2: no push")),
            "the uncovered destination's story is its coverage finding, not a doubling: {:?}",
            all
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
}
