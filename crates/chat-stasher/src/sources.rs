//! Source consent (ADR-065): which registry sources this machine collects.
//!
//! Every registry source on a machine is in one of three states:
//!
//! * `on`      — collected;
//! * `off`     — the user said no, and nothing is collected;
//! * `pending` — nobody has decided yet, and nothing is collected.
//!
//! Only `on` is collected (D1). The state is read from the `[sources]` table of
//! the config (D2), with two derivations for sources that have no entry:
//!
//! * a path the user stated in `[harness_roots]` counts as `on`, because the
//!   user named it — and an explicit `off` still beats it;
//! * the upgrade rule (D7): when there is no `[sources]` table at all and the
//!   stage already holds sessions, a source that has sessions on that stage
//!   counts as `on`, so a machine that was collecting before this release keeps
//!   collecting what it collected and nothing else.
//!
//! The enforcement seam (D3) is [`consented_registry`]: a pass derives its
//! sources from a registry narrowed to the `on` entries, so collect, run-once
//! and the hourly timer cannot diverge. The read-only display scan keeps using
//! the whole registry, so `setup`, `status` and `sources` still see everything.
//!
//! This module reads directory *names* on the stage (to apply the upgrade rule)
//! and `stat`s source roots (to name a pending source). It reads no
//! conversation text and writes nothing.

use crate::config::Config;
use crate::scanner::{HarnessRegistry, RegistryHarness, RootPresence};
use std::collections::BTreeSet;
use std::path::Path;

/// The process exit code for "no source consent is recorded": a pass, a
/// collect or a non-interactive `setup` that has not been told which tools to
/// collect refuses with this code instead of collecting everything (ADR-065
/// D5). It is its own number so a wrapper can tell "nobody has chosen yet"
/// from `1` (a step failed), `2` (usage) and `3` (something was not read).
pub const EXIT_NO_CONSENT: u8 = 4;

/// One source's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceState {
    On,
    Off,
    Pending,
}

impl SourceState {
    /// The word used in the config, the CLI and the JSON.
    pub fn word(self) -> &'static str {
        match self {
            SourceState::On => "on",
            SourceState::Off => "off",
            SourceState::Pending => "pending",
        }
    }
}

/// Why a source is in the state it is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// `[sources]` has an entry for it.
    Recorded,
    /// No entry, but `[harness_roots]` names a path for it (D2).
    DeclaredRoot,
    /// No `[sources]` table, and the stage already holds sessions of it (D7).
    StageHistory,
    /// No entry and nothing that stands in for one.
    NoRecord,
}

impl Basis {
    pub fn word(self) -> &'static str {
        match self {
            Basis::Recorded => "recorded",
            Basis::DeclaredRoot => "declared_root",
            Basis::StageHistory => "stage_history",
            Basis::NoRecord => "no_record",
        }
    }
}

/// What was learned about the stage for the upgrade rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageEvidence {
    /// The rule did not apply (a `[sources]` table exists, or every source
    /// already has a declared root), so the stage was not looked at.
    NotNeeded,
    /// The rule applies but no stage is known to this command, so it could not
    /// be applied. Every source without another basis is `pending`, which is
    /// the safe answer, and the caller says the stage was not known.
    NotGiven,
    /// The stage was read: the harness ids that have at least one session
    /// directory on it.
    Read(BTreeSet<String>),
    /// The stage exists but could not be listed. Unknown is not empty: the
    /// rule is not applied, and the reason is carried to the user.
    Unreadable(String),
}

/// One source's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub id: String,
    pub display_name: String,
    pub state: SourceState,
    pub basis: Basis,
}

/// The state of every registry source on this machine, and how it was reached.
#[derive(Debug, Clone)]
pub struct Consent {
    /// One entry per registry source, in registry order.
    pub decisions: Vec<Decision>,
    /// Whether the config has a `[sources]` table at all. An empty table counts:
    /// it is a recorded decision that nothing is on.
    pub table_present: bool,
    /// What the upgrade rule learned about the stage.
    pub stage: StageEvidence,
    /// Keys in `[sources]` that are not a registry id (a typo, or a source this
    /// build does not know). They change nothing and are reported.
    pub unrecognised_keys: Vec<String>,
}

impl Consent {
    /// The state of `id`. An id the registry does not list is `pending`: it has
    /// no recorded state, and nothing is collected from it.
    pub fn state_of(&self, id: &str) -> SourceState {
        self.decisions
            .iter()
            .find(|d| d.id == id)
            .map_or(SourceState::Pending, |d| d.state)
    }

    pub fn decision(&self, id: &str) -> Option<&Decision> {
        self.decisions.iter().find(|d| d.id == id)
    }

    /// Ids in `state`, in registry order.
    pub fn ids(&self, state: SourceState) -> Vec<&str> {
        self.decisions
            .iter()
            .filter(|d| d.state == state)
            .map(|d| d.id.as_str())
            .collect()
    }

    /// Ids the upgrade rule carried over (D7): `on` only because the stage
    /// already holds their sessions. Empty unless the rule applied.
    pub fn carried_over(&self) -> Vec<&str> {
        self.decisions
            .iter()
            .filter(|d| d.basis == Basis::StageHistory)
            .map(|d| d.id.as_str())
            .collect()
    }

    /// True when the upgrade rule is what keeps at least one source on.
    pub fn upgrade_applied(&self) -> bool {
        !self.table_present && !self.carried_over().is_empty()
    }

    /// Whether any consent exists on this machine: a `[sources]` table (even an
    /// empty one), or at least one source that is `on` through a declared root
    /// or the stage's history. When this is false, a pass has nothing to go on
    /// and refuses (D5) instead of collecting everything.
    pub fn consent_recorded(&self) -> bool {
        self.table_present || self.decisions.iter().any(|d| d.state == SourceState::On)
    }
}

/// True when `[harness_roots]` (or the two legacy single-harness aliases) names
/// a path for `id`, including the compound `<id>:<root>` keys that point one of
/// a family's extra roots.
fn declares_root(config: &Config, id: &str) -> bool {
    if config.explicit_harness_root(id).is_some() {
        return true;
    }
    let prefix = format!("{id}:");
    config
        .harness_roots
        .iter()
        .any(|(key, value)| key.starts_with(&prefix) && !value.is_empty())
}

/// Evaluate every registry source's state (D1, D2, D7).
///
/// `stage` is the stage the caller works against, when it has one. It is read
/// (directory names only) only when the upgrade rule can matter: no `[sources]`
/// table and at least one source with no declared root.
pub fn evaluate(config: &Config, registry: &HarnessRegistry, stage: Option<&Path>) -> Consent {
    let table = config.sources.as_ref();
    let table_present = table.is_some();
    let needs_stage = !table_present
        && registry
            .harnesses
            .iter()
            .any(|h| !declares_root(config, &h.id));
    let stage_evidence = if !needs_stage {
        StageEvidence::NotNeeded
    } else {
        match stage {
            None => StageEvidence::NotGiven,
            Some(stage) => read_stage_history(stage),
        }
    };
    let history: Option<&BTreeSet<String>> = match &stage_evidence {
        StageEvidence::Read(set) => Some(set),
        _ => None,
    };

    let decisions = registry
        .harnesses
        .iter()
        .map(|h| {
            let (state, basis) = match table.and_then(|t| t.get(&h.id)) {
                Some(crate::config::SourceChoice::On) => (SourceState::On, Basis::Recorded),
                Some(crate::config::SourceChoice::Off) => (SourceState::Off, Basis::Recorded),
                None if declares_root(config, &h.id) => (SourceState::On, Basis::DeclaredRoot),
                None if history.is_some_and(|set| set.contains(&h.id)) => {
                    (SourceState::On, Basis::StageHistory)
                }
                None => (SourceState::Pending, Basis::NoRecord),
            };
            Decision {
                id: h.id.clone(),
                display_name: h.display_name.clone(),
                state,
                basis,
            }
        })
        .collect();

    let known: BTreeSet<&str> = registry.harnesses.iter().map(|h| h.id.as_str()).collect();
    let unrecognised_keys = table
        .map(|t| {
            t.keys()
                .filter(|key| !known.contains(key.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    Consent {
        decisions,
        table_present,
        stage: stage_evidence,
        unrecognised_keys,
    }
}

/// The harness ids that have session directories on `stage`: the leading
/// dot-segment of every `sessions/<machine>/<session-dir>` name. Names only;
/// no file is opened.
fn read_stage_history(stage: &Path) -> StageEvidence {
    let sessions = stage.join("sessions");
    let machines = match std::fs::read_dir(&sessions) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return StageEvidence::Read(BTreeSet::new());
        }
        Err(e) => {
            return StageEvidence::Unreadable(format!(
                "the stage's sessions directory could not be listed: {e}"
            ));
        }
    };
    let mut found = BTreeSet::new();
    for machine in machines {
        let machine = match machine {
            Ok(entry) => entry,
            Err(e) => {
                return StageEvidence::Unreadable(format!(
                    "a stage machine partition could not be listed: {e}"
                ));
            }
        };
        match machine.file_type() {
            Ok(kind) if kind.is_dir() => {}
            Ok(_) => continue,
            Err(e) => {
                return StageEvidence::Unreadable(format!(
                    "a stage machine partition could not be inspected: {e}"
                ));
            }
        }
        let sessions = match std::fs::read_dir(machine.path()) {
            Ok(entries) => entries,
            Err(e) => {
                return StageEvidence::Unreadable(format!(
                    "a stage machine partition could not be listed: {e}"
                ));
            }
        };
        for session in sessions {
            let session = match session {
                Ok(entry) => entry,
                Err(e) => {
                    return StageEvidence::Unreadable(format!(
                        "a stage session directory could not be listed: {e}"
                    ));
                }
            };
            let name = session.file_name();
            let name = name.to_string_lossy();
            if let Some((harness, _)) = name.split_once('.') {
                if !harness.is_empty() {
                    found.insert(harness.to_string());
                }
            }
        }
    }
    StageEvidence::Read(found)
}

/// D3, the one place a pass narrows its sources: a copy of `registry` that
/// holds only the sources that are `on`. Everything downstream that derives
/// sources from a registry — the scan a pass collects from, the machine logs
/// declared on a harness entry, the push guard's evidence — sees nothing else.
pub fn consented_registry(registry: &HarnessRegistry, consent: &Consent) -> HarnessRegistry {
    let mut narrowed = registry.clone();
    narrowed
        .harnesses
        .retain(|h| consent.state_of(&h.id) == SourceState::On);
    narrowed
}

/// Sources that are `pending` and whose store is on this machine, named from
/// their root paths alone (`stat` only; nothing is opened or walked). These are
/// the tools a pass names until the user decides (D6). A source whose
/// presence could not be established is not named: that would be a claim.
pub fn pending_and_present(
    config: &Config,
    registry: &HarnessRegistry,
    consent: &Consent,
) -> Vec<String> {
    registry
        .harnesses
        .iter()
        .filter(|h| consent.state_of(&h.id) == SourceState::Pending)
        .filter(|h| crate::scanner::root_presence(config, h) == RootPresence::Present)
        .map(|h| h.id.clone())
        .collect()
}

/// The registry entry for `id`, if the registry lists it.
pub fn registry_entry<'a>(registry: &'a HarnessRegistry, id: &str) -> Option<&'a RegistryHarness> {
    registry.harnesses.iter().find(|h| h.id == id)
}

/// The command that records consent, written out for the refusal message. With
/// a stage it is the exact non-interactive command; without one it is the
/// interactive entry point.
pub fn next_command(stage: Option<&Path>) -> String {
    match stage {
        Some(stage) => format!(
            "chat-stasher setup --all-detected --stage {} (or choose with --sources <id,id>; \
             `chat-stasher sources` lists what was detected)",
            stage.display()
        ),
        None => "chat-stasher setup (it shows what was detected and asks which tools to \
                 collect); scripts: chat-stasher setup --all-detected --stage <dir>, or \
                 --sources <id,id>"
            .to_string(),
    }
}

/// The error a pass returns when no consent is recorded (D5). The CLI maps it
/// to [`EXIT_NO_CONSENT`].
#[derive(Debug)]
pub struct NoConsentRecorded {
    pub next: String,
}

impl std::fmt::Display for NoConsentRecorded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no source has been chosen on this machine, so nothing was collected. Next: {}",
            self.next
        )
    }
}

impl std::error::Error for NoConsentRecorded {}

/// Refuse when no consent is recorded; otherwise hand back the evaluation.
pub fn require_consent(
    config: &Config,
    registry: &HarnessRegistry,
    stage: &Path,
) -> Result<Consent, NoConsentRecorded> {
    let consent = evaluate(config, registry, Some(stage));
    if consent.consent_recorded() {
        Ok(consent)
    } else {
        Err(NoConsentRecorded {
            next: next_command(Some(stage)),
        })
    }
}

/// The one-line notice a pass prints when the upgrade rule is what keeps
/// collection going (D7). `None` when the rule did not apply.
pub fn upgrade_notice(consent: &Consent) -> Option<String> {
    if !consent.upgrade_applied() {
        return None;
    }
    let pending_n = consent.ids(SourceState::Pending).len();
    Some(format!(
        "[sources] no [sources] table is recorded yet: collecting only the sources this stage \
         already holds ({}); {pending_n} other registry source(s) are pending and are not \
         collected. Record these choices with `chat-stasher sources`.",
        consent.carried_over().join(", ")
    ))
}

/// The line a pass and `status` print for sources that are pending but present
/// (D6). `None` when there are none.
pub fn pending_notice(pending_present: &[String]) -> Option<String> {
    if pending_present.is_empty() {
        return None;
    }
    Some(format!(
        "[sources] pending, NOT collected until you decide: {}. Nothing is read from them. \
         Decide with `chat-stasher sources` (or `chat-stasher sources enable|disable <id>`).",
        pending_present.join(", ")
    ))
}

/// The one-line summary `status` prints: what is collected, what the user
/// turned off, and what is pending. `pending_present` is the pending subset
/// whose store was found; a pending source that is not on this machine is not
/// named, so the line does not list twenty tools nobody has.
///
/// The wording keeps three things apart that a gap report would run together
/// (ADR-065 D10): `off` is a choice, `pending` is an undecided question, and
/// neither is "not found" or "could not be read". None of them is a gap.
pub fn summary_line(consent: &Consent, pending_present: &[String]) -> String {
    if !consent.consent_recorded() {
        let mut line = String::from(
            "[sources] no source has been chosen on this machine, so nothing is collected",
        );
        if !pending_present.is_empty() {
            line.push_str(&format!(
                " (detected, waiting for your decision: {})",
                pending_present.join(", ")
            ));
        }
        line.push_str(
            ". Choose with `chat-stasher setup` (scripts: `chat-stasher setup --all-detected \
             --stage <dir>`).",
        );
        return line;
    }
    let on = consent.ids(SourceState::On);
    let off = consent.ids(SourceState::Off);
    let mut parts: Vec<String> = Vec::new();
    parts.push(if on.is_empty() {
        "collecting: none".to_string()
    } else {
        format!("collecting: {}", on.join(", "))
    });
    if !off.is_empty() {
        parts.push(format!("off, by your choice: {}", off.join(", ")));
    }
    if !pending_present.is_empty() {
        parts.push(format!(
            "pending, not collected until you decide: {}",
            pending_present.join(", ")
        ));
    }
    format!(
        "[sources] {} — change with `chat-stasher sources`",
        parts.join(" · ")
    )
}

/// A notice about `[sources]` keys that match no registry id.
pub fn unrecognised_notice(consent: &Consent) -> Option<String> {
    if consent.unrecognised_keys.is_empty() {
        return None;
    }
    Some(format!(
        "[sources] ignoring key(s) that are not a registry source id: {}",
        consent.unrecognised_keys.join(", ")
    ))
}

/// Why the stage could not settle the upgrade rule, when that is worth saying.
pub fn stage_evidence_notice(consent: &Consent) -> Option<String> {
    match &consent.stage {
        StageEvidence::NotGiven => Some(
            "[sources] no stage is known here, so sources this stage already holds could not \
             be carried over: they read as pending in this report (a pass run with --stage \
             applies the rule itself)"
                .to_string(),
        ),
        StageEvidence::Unreadable(why) => Some(format!(
            "[sources] the stage could not be read ({why}), so the upgrade rule was not \
             applied: sources without a record read as pending"
        )),
        StageEvidence::NotNeeded | StageEvidence::Read(_) => None,
    }
}

/// The `sources` object shared by `status --json`, `doctor --json` and
/// `setup --json`. `pending_present` is the pending subset whose store was
/// found; the full per-source list is always included so a reader never has to
/// infer a state from a missing row.
pub fn consent_json(consent: &Consent, pending_present: &[String]) -> serde_json::Value {
    let stage = match &consent.stage {
        StageEvidence::NotNeeded => serde_json::json!({"kind": "not_needed"}),
        StageEvidence::NotGiven => serde_json::json!({"kind": "not_given"}),
        StageEvidence::Read(set) => {
            serde_json::json!({"kind": "read", "harnesses_with_sessions": set})
        }
        StageEvidence::Unreadable(why) => {
            serde_json::json!({"kind": "unreadable", "why": why})
        }
    };
    serde_json::json!({
        "kind": "ok",
        "consent_recorded": consent.consent_recorded(),
        "table_present": consent.table_present,
        "upgrade_rule_applied": consent.upgrade_applied(),
        "carried_over": consent.carried_over(),
        "stage": stage,
        "on": consent.ids(SourceState::On),
        "off": consent.ids(SourceState::Off),
        "pending": consent.ids(SourceState::Pending),
        "pending_present": pending_present,
        "unrecognised_keys": consent.unrecognised_keys,
        "sources": consent.decisions.iter().map(|d| serde_json::json!({
            "id": d.id,
            "state": d.state.word(),
            "basis": d.basis.word(),
        })).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// The choice surface: rows, selection, and the explicit write (D4, D8, D9)
// ---------------------------------------------------------------------------

/// What a read-only scan found for one source, in the terms the choice needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    /// The store is here and was read. It can be chosen.
    Found {
        /// Session count, or `None` when it could not be enumerated: unknown,
        /// never shown as zero.
        sessions: Option<u64>,
        /// Size in bytes when the scan already had it; `None` renders as
        /// "unknown" because computing it would mean reading more.
        bytes: Option<u64>,
        path: Option<String>,
    },
    /// The scan could not tell whether the store exists (permissions, I/O).
    /// Shown, and cannot be chosen.
    Unreadable,
    /// The registry has no verified path for this platform; the user has to
    /// state one in `[harness_roots]` before the source can be read.
    NeedsPath,
    /// Looked for and not there.
    NotFound,
    /// The registry has no cell for this platform at all.
    NotApplicable,
}

/// One source in the choice list.
#[derive(Debug, Clone)]
pub struct SourceRow {
    pub id: String,
    pub display_name: String,
    pub kind: RowKind,
    /// The dated verification the registry records for this platform.
    pub verified: Option<String>,
    /// The state the config gives it today.
    pub current: SourceState,
}

impl SourceRow {
    /// Only a source whose store was found can be chosen.
    pub fn selectable(&self) -> bool {
        matches!(self.kind, RowKind::Found { .. })
    }

    /// The pre-selection (D4): a recorded answer is kept, otherwise a source
    /// with a dated verification is marked and an unverified one is not.
    pub fn preselected(&self) -> bool {
        if !self.selectable() {
            return false;
        }
        match (self.current, self.verified.is_some()) {
            (SourceState::On, _) => true,
            (SourceState::Off, _) => false,
            (SourceState::Pending, verified) => verified,
        }
    }
}

/// Build the rows from a read-only scan, in registry order.
pub fn rows_from_scan(
    registry: &HarnessRegistry,
    scan: &crate::scanner::ScanReport,
    consent: &Consent,
) -> Vec<SourceRow> {
    use crate::scanner::ProbeState;
    let platform = crate::scanner::current_platform();
    registry
        .harnesses
        .iter()
        .map(|h| {
            let probe = scan.probes.iter().find(|p| p.id == h.id);
            let kind = match probe {
                None => RowKind::NotFound,
                Some(p) => match p.state {
                    ProbeState::Scanned | ProbeState::FileTarget => {
                        let from_records: u64 = scan
                            .records
                            .iter()
                            .filter(|r| r.source.short() == h.id)
                            .map(|r| r.byte_size)
                            .sum();
                        RowKind::Found {
                            sessions: p.record_count,
                            bytes: p.bytes.or(if p.record_count.is_some() {
                                Some(from_records)
                            } else {
                                None
                            }),
                            path: p.root.as_ref().map(|r| r.display().to_string()),
                        }
                    }
                    ProbeState::Missing => RowKind::NotFound,
                    ProbeState::Indeterminate => RowKind::Unreadable,
                    ProbeState::SkipUnascertained | ProbeState::SkipUnresolvable => {
                        RowKind::NeedsPath
                    }
                    ProbeState::SkipWrongPlatform => RowKind::NotApplicable,
                },
            };
            SourceRow {
                id: h.id.clone(),
                display_name: h.display_name.clone(),
                kind,
                verified: h.verified_date_on(platform).map(str::to_string),
                current: consent.state_of(&h.id),
            }
        })
        .collect()
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Render the choice list. `numbered` adds the `[x]`/`[ ]` marks and the numbers
/// the multi-select takes; without it (the `sources` listing) each row shows
/// its current state instead. Nothing here reads conversation text: a row is a
/// name, a count, a size, a path and a verification date.
pub fn render_rows(rows: &[SourceRow], numbered: bool, marks: &[bool]) -> String {
    let mut out = String::new();
    let mut n = 0usize;
    let mut mark_i = 0usize;
    for row in rows {
        let RowKind::Found {
            sessions,
            bytes,
            path,
        } = &row.kind
        else {
            continue;
        };
        n += 1;
        let sessions = sessions.map_or("unknown".to_string(), |c| c.to_string());
        let size = bytes.map_or("unknown".to_string(), human_bytes);
        let verified = match &row.verified {
            Some(date) => format!("verified {date}"),
            None => "not verified".to_string(),
        };
        let lead = if numbered {
            let mark = if marks.get(mark_i).copied().unwrap_or(false) {
                "x"
            } else {
                " "
            };
            mark_i += 1;
            format!("[{mark}] {n:>2}")
        } else {
            format!("{:<8}", row.current.word())
        };
        out.push_str(&format!(
            "  {lead}  {:<18} sessions {sessions:<7} size {size:<9} {verified:<22} {}\n",
            row.id,
            path.as_deref().unwrap_or("-")
        ));
    }
    let list = |wanted: fn(&RowKind) -> bool| -> Vec<String> {
        rows.iter()
            .filter(|r| wanted(&r.kind))
            .map(|r| r.id.clone())
            .collect()
    };
    let unreadable = list(|k| matches!(k, RowKind::Unreadable));
    if !unreadable.is_empty() {
        out.push_str(&format!(
            "  Cannot be chosen, unreadable (whether they exist is unknown; check permissions): {}\n",
            unreadable.join(", ")
        ));
    }
    let needs = list(|k| matches!(k, RowKind::NeedsPath));
    if !needs.is_empty() {
        out.push_str(&format!(
            "  Need a path before they can be read (state it under [harness_roots] in the config): {}\n",
            needs.join(", ")
        ));
    }
    let missing = list(|k| matches!(k, RowKind::NotFound));
    if !missing.is_empty() {
        out.push_str(&format!(
            "  Not found on this machine: {}\n",
            missing.join(", ")
        ));
    }
    out
}

/// Parse the answer to the multi-select. `defaults` is the pre-selection, in
/// the order of the numbered rows. Accepts Enter (keep the marks), `all`,
/// `none`, or comma/space separated row numbers (the chosen set, replacing the
/// marks). Anything else is an error naming what was not understood, so a typo
/// never silently chooses or drops a source.
pub fn parse_selection(input: &str, defaults: &[bool]) -> Result<Vec<bool>, String> {
    let text = input.trim();
    if text.is_empty() {
        return Ok(defaults.to_vec());
    }
    match text.to_ascii_lowercase().as_str() {
        "all" | "a" => return Ok(vec![true; defaults.len()]),
        "none" | "n" => return Ok(vec![false; defaults.len()]),
        _ => {}
    }
    let mut chosen = vec![false; defaults.len()];
    for token in text.split(|c: char| c == ',' || c.is_whitespace()) {
        if token.is_empty() {
            continue;
        }
        let number: usize = token
            .parse()
            .map_err(|_| format!("`{token}` is not a row number, `all`, or `none`"))?;
        if number == 0 || number > defaults.len() {
            return Err(format!(
                "{number} is not one of the {} listed row(s)",
                defaults.len()
            ));
        }
        chosen[number - 1] = true;
    }
    Ok(chosen)
}

/// The edits that record an answer for the selectable rows, plus — the first
/// time anything is written with no `[sources]` table (D7) — an explicit `on`
/// for every source that is on only through a declared root or the stage's
/// history, so writing the table does not silently turn those off.
pub fn edits_for_choice(
    consent: &Consent,
    answers: &[(String, crate::config::SourceChoice)],
) -> Vec<(String, crate::config::SourceChoice)> {
    let mut edits: Vec<(String, crate::config::SourceChoice)> = answers.to_vec();
    if !consent.table_present {
        for d in &consent.decisions {
            if d.state == SourceState::On
                && d.basis != Basis::Recorded
                && !edits.iter().any(|(id, _)| id == &d.id)
            {
                edits.push((d.id.clone(), crate::config::SourceChoice::On));
            }
        }
    }
    edits
}

/// What `sources disable` tells the user (D9): off is not delete.
pub fn disable_notice(id: &str) -> String {
    format!(
        "`{id}` is off: no new sessions are collected from it. Nothing was deleted. Sessions \
         already archived stay in the archive. Copies already in the stage stay there, in \
         plaintext, until the stage is reclaimed (`chat-stasher reclaim-stage`). To remove one \
         archived session, see `chat-stasher purge <session-id>`."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SourceChoice;
    use serde_json::json;

    fn registry(ids: &[&str]) -> HarnessRegistry {
        serde_json::from_value(json!({
            "schema_version": 1,
            "generated": "test",
            "harnesses": ids
                .iter()
                .map(|id| json!({"id": id, "display_name": id}))
                .collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    fn config_with(sources: Option<&[(&str, SourceChoice)]>, roots: &[(&str, &str)]) -> Config {
        let mut config = Config::default();
        config.sources = sources.map(|s| s.iter().map(|(k, v)| (k.to_string(), *v)).collect());
        for (k, v) in roots {
            config.harness_roots.insert(k.to_string(), v.to_string());
        }
        config
    }

    #[test]
    fn no_record_is_pending_and_not_consent() {
        let reg = registry(&["a", "b"]);
        let consent = evaluate(&config_with(None, &[]), &reg, None);
        assert_eq!(consent.ids(SourceState::Pending), vec!["a", "b"]);
        assert!(!consent.consent_recorded());
        assert_eq!(consent.stage, StageEvidence::NotGiven);
    }

    #[test]
    fn an_empty_table_is_a_recorded_decision() {
        let reg = registry(&["a"]);
        let consent = evaluate(&config_with(Some(&[]), &[]), &reg, None);
        assert!(consent.consent_recorded());
        assert_eq!(consent.state_of("a"), SourceState::Pending);
        assert_eq!(consent.stage, StageEvidence::NotNeeded);
    }

    #[test]
    fn a_declared_root_is_on_and_an_explicit_off_beats_it() {
        let reg = registry(&["a", "b", "c"]);
        let config = config_with(
            Some(&[("b", SourceChoice::Off)]),
            &[("a", "/x"), ("b", "/y"), ("c:extra", "/z")],
        );
        let consent = evaluate(&config, &reg, None);
        assert_eq!(consent.state_of("a"), SourceState::On);
        assert_eq!(consent.decision("a").unwrap().basis, Basis::DeclaredRoot);
        assert_eq!(consent.state_of("b"), SourceState::Off);
        assert_eq!(consent.state_of("c"), SourceState::On);
    }

    #[test]
    fn the_upgrade_rule_carries_only_sources_the_stage_holds() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path();
        std::fs::create_dir_all(stage.join("sessions/m1/a.m1.s1")).unwrap();
        std::fs::create_dir_all(stage.join("sessions/m2/c.m2.s9")).unwrap();
        let reg = registry(&["a", "b", "c"]);
        let consent = evaluate(&config_with(None, &[]), &reg, Some(stage));
        assert_eq!(consent.ids(SourceState::On), vec!["a", "c"]);
        assert_eq!(consent.ids(SourceState::Pending), vec!["b"]);
        assert!(consent.upgrade_applied());
        assert_eq!(consent.carried_over(), vec!["a", "c"]);
        assert!(upgrade_notice(&consent).unwrap().contains("a, c"));
        // With a table the rule is off, however much the stage holds.
        let consent = evaluate(&config_with(Some(&[]), &[]), &reg, Some(stage));
        assert!(consent.ids(SourceState::On).is_empty());
    }

    #[test]
    fn an_unreadable_stage_applies_no_rule_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        // `sessions` is a file, so it cannot be listed as a directory.
        std::fs::write(dir.path().join("sessions"), b"x").unwrap();
        let reg = registry(&["a"]);
        let consent = evaluate(&config_with(None, &[]), &reg, Some(dir.path()));
        assert!(matches!(consent.stage, StageEvidence::Unreadable(_)));
        assert_eq!(consent.state_of("a"), SourceState::Pending);
        assert!(stage_evidence_notice(&consent).is_some());
    }

    #[test]
    fn the_seam_keeps_only_on_sources() {
        let reg = registry(&["a", "b", "c"]);
        let config = config_with(
            Some(&[("a", SourceChoice::On), ("b", SourceChoice::Off)]),
            &[],
        );
        let consent = evaluate(&config, &reg, None);
        let narrowed = consented_registry(&reg, &consent);
        let ids: Vec<_> = narrowed.harnesses.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["a"]);
    }

    #[test]
    fn unrecognised_keys_are_reported_and_change_nothing() {
        let reg = registry(&["a"]);
        let config = config_with(Some(&[("claude_code", SourceChoice::On)]), &[]);
        let consent = evaluate(&config, &reg, None);
        assert_eq!(consent.unrecognised_keys, vec!["claude_code".to_string()]);
        assert!(consent.ids(SourceState::On).is_empty());
    }

    #[test]
    fn a_wrong_value_in_the_table_makes_the_config_unreadable() {
        let parsed = toml::from_str::<Config>("[sources]\ncodex = \"yes\"\n");
        assert!(parsed.is_err(), "a value other than on/off must not parse");
    }
}
