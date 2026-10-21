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
    /// The relevant unit files could not all be read, so the question has
    /// no answer here — which is not the same claim as "no timer".
    Unknown { why: String },
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
    /// are unknown — an unknown, not "none".
    Unknown { why: String },
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
    /// quiet rather than becoming a false alarm.
    pub unit_installed: bool,
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
}

/// The one function (C1): read the local, read-only facts and turn them into
/// per-destination verdicts plus the worded alerts.
pub fn reconcile(inputs: &Inputs) -> Reconciliation {
    let run_once_units = schedule::installed_run_once_units(inputs.format, inputs.home);
    let reclaim_units = schedule::installed_reclaim_units(inputs.format, inputs.home);
    let read_failures: Vec<&schedule::UnitObservation> = run_once_units
        .iter()
        .chain(reclaim_units.iter())
        .filter(|unit| unit.parse_note.is_some())
        .collect();

    let raw_refs = raw_env_option_refs();

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
        // ①  from the installed unit files.
        let push_coverage = push_coverage_of(name, &run_once_units, &read_failures);
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
        let credentials = credentials_of(name, &contacting, &raw_refs);
        // ③ the durable push record.
        let last_push = match &push_records {
            crate::pushstate::PushStateRead::Present(state) => match state.pushes.get(name) {
                Some(record) => LastPush::At {
                    finished_at_unix: record.finished_at_unix,
                    snapshots_in_repo: record.snapshots_in_repo,
                },
                None => LastPush::NoRecord,
            },
            _ => LastPush::NoRecord,
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

    let reclaim = reclaim_job_facts(&reclaim_units, &reclaim_read, inputs.now_unix);

    let reconciliation = Reconciliation {
        destinations,
        reclaim,
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

/// Raw `env:NAME` option references per destination, re-read from the
/// config file's own text. The loaded `Config` has already replaced or
/// dropped these, so this is the only place the machine's *configured*
/// credential shape is still visible. Public for the install-time warning:
/// `schedule install` reads it to tell the user, before the first scheduled
/// run fails, that the timer it just wrote cannot carry these.
pub fn raw_env_option_refs() -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let text = match std::fs::read_to_string(crate::config::config_path()) {
        Ok(text) => text,
        // The config itself is unreadable: report no refs — callers word
        // that condition from the config error they already have, and a
        // per-destination unknown would only repeat it one row at a time.
        Err(_) => return out,
    };
    #[derive(Deserialize)]
    struct RawDestination {
        options: Option<BTreeMap<String, String>>,
    }
    #[derive(Deserialize)]
    struct RawFile {
        destinations: Option<BTreeMap<String, RawDestination>>,
    }
    let Ok(file) = toml::from_str::<RawFile>(&text) else {
        return out;
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
    out
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

/// The machine-level reclaim facts.
fn reclaim_job_facts(
    reclaim_units: &[schedule::UnitObservation],
    read: &crate::reclaimstate::ReclaimStateRead,
    now_unix: u64,
) -> ReclaimJobFacts {
    let unit_installed = !reclaim_units.is_empty();
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
/// destination, the most serious first-order finding it has; the reclaim
/// share adds a second alert when it is the consequence that has already
/// arrived.
///
/// A quiet row is no alerts at all: `status`'s body stays byte-identical on
/// a machine whose destinations are all covered, all resolvable from a
/// timer and currently reclaiming — the reconciliation is a new sound,
/// not a new hum (same rule as `doctor::key_alert_lines`).
fn word_alerts(mut reconciliation: Reconciliation) -> Reconciliation {
    let destinations = reconciliation.destinations.clone();
    let reclaim = reconciliation.reclaim.clone();

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
            match &facts.push_coverage {
                PushCoverage::Covered { .. } => {}
                PushCoverage::NoScheduledPush => reconciliation.alerts.push(Alert {
                    severity: Severity::Yellow,
                    sentence: format!(
                        "[dest] {name}: declared, but no installed timer pushes it — a push \
                         reaches it only when a command names it by hand; the weekly stage \
                         clean-up consults every declared destination, so the stage keeps \
                         growing until `{name}` holds a copy or the declaration is removed",
                        name = facts.name
                    ),
                    fix: Some(format!(
                        "chat-stasher schedule install --destination {} --stage <stage>",
                        facts.name
                    )),
                }),
                PushCoverage::Unknown { why } => reconciliation.alerts.push(Alert {
                    severity: Severity::Yellow,
                    sentence: format!("`{}`: {}", facts.name, why),
                    fix: None,
                }),
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
            _ => {}
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
            let held: usize = attempt.blocked.iter().map(|b| b.sessions).sum();
            let bytes: u64 = attempt.blocked.iter().map(|b| b.bytes).sum();
            let destinations_held: Vec<&str> = attempt
                .blocked
                .iter()
                .map(|b| b.destination.as_str())
                .collect();
            let noun = if streak == 1 {
                "blocked once in a row".to_string()
            } else {
                format!("blocked {streak} times in a row")
            };
            reconciliation.job_alerts.push(Alert {
                severity,
                sentence: format!(
                    "[reclaim] the weekly stage clean-up is {noun} — {held} session(s) / {} held \
                     back for {names}; nothing was deleted and the stage keeps every byte it \
                     archives",
                    fmt_bytes(bytes),
                    names = destinations_held.join(", "),
                ),
                fix: Some(
                    "chat-stasher reclaim-stage --stage <stage>   # dry run: names the \
                     destination and the reason in its output"
                        .to_string(),
                ),
            });
            // Refuse-shape: the run ended before consulting anything, which
            // itself says the stage was not reclaimed this week.
            if attempt.outcome == crate::reclaimstate::ReclaimOutcome::Refused {
                reconciliation.job_alerts.push(Alert {
                    severity: Severity::Yellow,
                    sentence: "[reclaim] the last recorded attempt ended before the destinations \
                              were consulted (a refusal in its own output)"
                        .to_string(),
                    fix: None,
                });
            }
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
    if reclaim.unit_installed {
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
    for (name, refs) in raw_env_option_refs() {
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
            PushCoverage::Unknown { why } => serde_json::json!({
                "kind": "unknown",
                "why": why,
            }),
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
            CredentialResolution::Unknown { why } => serde_json::json!({
                "kind": "unknown",
                "why": why,
            }),
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
/// exist would be another way to invent "never ran".
fn reclaim_json(r: &Reconciliation) -> serde_json::Value {
    let base = serde_json::json!({ "unit_installed": r.reclaim.unit_installed });
    let value = match &r.reclaim.record {
        ReclaimJobRecord::Missing => serde_json::json!({
            "kind": "missing",
            "why": "no reclaim attempt has ever been recorded on this machine",
        }),
        ReclaimJobRecord::Unreadable(why) => serde_json::json!({
            "kind": "unreadable",
            "why": why,
        }),
        ReclaimJobRecord::Present { attempt, age_secs } => serde_json::json!({
            "kind": "known",
            "state": attempt.outcome.slug(),
            "apply": attempt.apply,
            "consecutive_blocked": attempt.consecutive_blocked,
            "finished_at_unix": attempt.finished_at_unix,
            "age_secs": age_secs,
            "overdue": *age_secs > crate::reclaimstate::RECLAIM_OVERDUE_SECS,
            "overdue_after_secs": crate::reclaimstate::RECLAIM_OVERDUE_SECS,
            "held_back_sessions": attempt.blocked.iter().map(|b| b.sessions).sum::<usize>(),
            "held_back_bytes": attempt.blocked.iter().map(|b| b.bytes).sum::<u64>(),
            "blocked": attempt.blocked.iter().map(|b| serde_json::json!({
                "destination": b.destination,
                "kind": b.kind,
                "error_kind": b.error_kind,
                "sessions": b.sessions,
                "bytes": b.bytes,
            })).collect::<Vec<_>>(),
        }),
    };
    let mut out = value;
    if let Some(object) = out.as_object_mut() {
        object.insert("unit_installed".to_string(), base["unit_installed"].clone());
        if let Some(age) = r.reclaim.unit_age_secs {
            object.insert("unit_age_secs".to_string(), serde_json::json!(age));
        }
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
        // ①
        lines.push(format!(
            "    timer push     : {}",
            match &facts.push_coverage {
                PushCoverage::Covered { units } => units.join(", "),
                PushCoverage::NoScheduledPush => {
                    "no installed timer names this destination".to_string()
                }
                PushCoverage::Unknown { why } => format!("unknown — {why}"),
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
            sentences
                .iter()
                .any(|s| s.contains("[dest] r2: declared, but no installed timer pushes it")),
            "no push-coverage alert: {sentences:?}"
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
        });
        let stale_alert = out
            .all_alerts()
            .iter()
            .find(|a| a.sentence.contains("has not recorded a run"))
            .cloned()
            .expect("staleness alert");
        assert_eq!(stale_alert.severity, Severity::Red);
    }
}
