//! deletion_control — the user-initiated deletion control stream
//! (ADR-046 draft, Phase 0/1).
//!
//! Four product cases are separate (ADR-046 §1): source-side disappearance
//! never creates a control record; hide/unhide is reversible presentation
//! state; purge is an irreversible, explicitly confirmed physical deletion;
//! redact re-seals user-edited content after removing the scoped old version.
//! This module owns the durable record that makes those operations stick:
//! a versioned, append-only JSONL control stream under
//! `meta/<actor-machine>/deletion-control-v1.jsonl` in the stage (ADR-017: the
//! machine that runs the command writes its own metadata partition), carried
//! to every destination by the next ordinary `push` because a snapshot holds
//! the whole stage tree.
//!
//! Phase boundary (this is what exists today): `hide` and `unhide` are
//! complete operations; `purge` exists as its read-only preview (the W402
//! dry-run inventory) while the destructive transaction — gate, tombstone
//! publication barrier, snapshot rewrite, forget and prune — is a later
//! phase and no code here rewrites or deletes snapshots. No path in this
//! module writes a control record for a reason other than an explicit
//! `hide`/`unhide` command; in particular no collector, scanner or receiver
//! ever appends here, which is the policy claim that source-side
//! disappearance never becomes an archive instruction.
//!
//! Privacy line: control records carry schema, operation id, action, exact
//! target and actor identities, scope, UTC time and a bounded reason label —
//! no conversation text, titles, snippets or payload bytes, in records or in
//! any error this module produces.

use anyhow::Context;
use rustic_core::repofile::{MasterKey, Node, NodeType, SnapshotFile};
use rustic_core::{IndexedFullStatus, LsOptions, Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// File name of the control stream inside one machine's metadata partition.
pub const FILE_NAME: &str = "deletion-control-v1.jsonl";

/// Value of the `schema` field every record carries; a record that names
/// anything else is malformed and refuses the read (unknown, never empty).
pub const SCHEMA: &str = "chat-stasher/deletion-control/1";

/// Longest accepted `reason` label, in bytes. A reason is metadata the user
/// asked to store next to the session identity, not a form for conversation
/// text — the bound keeps a paste out of the stream and out of every report
/// that later quotes the record.
pub const MAX_REASON_BYTES: usize = 80;

/// Longest accepted operation id, in bytes.
pub const MAX_OPERATION_ID_BYTES: usize = 128;

/// One user-initiated control action. `Hide` and `Unhide` are reversible
/// presentation state; `Purge` and `Redact` are the destructive tombstone
/// actions whose execution is a later phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlAction {
    Hide,
    Unhide,
    Purge,
    Redact,
}

/// What a tombstone covers. Hide/unhide events carry [`ControlScope::Session`]
/// (they name the session, not a range of it); the destructive actions record
/// the scope an enforcement path needs: the whole session forever, or content
/// at or before a named cutoff for redaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "kebab-case")]
pub enum ControlScope {
    /// Presentation state over one whole session identity.
    Session,
    /// `purge SESSION`: every occurrence of the session, irreversibly.
    SessionForever,
    /// `redact SESSION --cutoff …`: content at or before the recorded cutoff.
    /// The cutoff is stored as the user supplied it; message-level comparison
    /// semantics are a later phase, so only that phase writes this scope.
    ContentThroughCutoff { recorded_cutoff: String },
}

/// One append-only control record. Idempotent by `operation_id`: a replay of
/// the same record is folded once, and the same id with different fields is a
/// conflict, which fails closed rather than guessing which record won.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlEvent {
    pub schema: String,
    pub operation_id: String,
    /// RFC 3339 UTC timestamp of the record. Ordering across streams compares
    /// the parsed instant, never the string spelling.
    pub recorded_at: String,
    pub action: ControlAction,
    /// Machine partition the target session lives under
    /// (`sessions/<target_machine>/<session_id>/`).
    pub target_machine: String,
    /// Exact session id. No consumer of this stream prefix-matches it.
    pub session_id: String,
    /// Machine partition of the writer that recorded the event (ADR-017).
    pub actor_machine: String,
    pub scope: ControlScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl ControlEvent {
    /// Validate the invariants every consumer relies on. Anything that fails
    /// here makes the whole stream unreadable (unknown), never "empty".
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!(
                "unknown control schema `{}` (expected `{SCHEMA}`)",
                self.schema
            ));
        }
        if self.operation_id.is_empty() || self.operation_id.len() > MAX_OPERATION_ID_BYTES {
            return Err(format!(
                "operation id must be 1..={MAX_OPERATION_ID_BYTES} bytes, got {}",
                self.operation_id.len()
            ));
        }
        if chrono::DateTime::parse_from_rfc3339(&self.recorded_at).is_err() {
            return Err(format!(
                "recorded_at is not RFC 3339: `{}`",
                self.recorded_at
            ));
        }
        for (name, value) in [
            ("target_machine", &self.target_machine),
            ("session_id", &self.session_id),
            ("actor_machine", &self.actor_machine),
        ] {
            if value.is_empty() || value.contains(['/', '\\']) || value == "." || value == ".." {
                return Err(format!("invalid {name} `{value}`"));
            }
        }
        if let Some(reason) = &self.reason {
            if reason.len() > MAX_REASON_BYTES || reason.contains(['\n', '\r']) {
                return Err(format!(
                    "reason must be at most {MAX_REASON_BYTES} bytes on one line"
                ));
            }
        }
        match (self.action, &self.scope) {
            (ControlAction::Hide, ControlScope::Session)
            | (ControlAction::Unhide, ControlScope::Session)
            | (ControlAction::Purge, ControlScope::SessionForever) => Ok(()),
            (ControlAction::Redact, ControlScope::ContentThroughCutoff { recorded_cutoff }) => {
                if recorded_cutoff.is_empty() {
                    Err("redact scope is missing its recorded cutoff".to_string())
                } else {
                    Ok(())
                }
            }
            (action, scope) => Err(format!(
                "action `{action:?}` cannot carry scope `{scope:?}`"
            )),
        }
    }
}

/// A stream that could not be read to the end, or whose content is not a
/// complete sequence of valid records. Carries the one-based line number that
/// broke the parse, when the bytes were readable at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamUnreadable {
    pub line_no: Option<usize>,
    pub why: String,
}

impl std::fmt::Display for StreamUnreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line_no {
            Some(line_no) => write!(f, "line {line_no}: {}", self.why),
            None => write!(f, "{}", self.why),
        }
    }
}

impl std::error::Error for StreamUnreadable {}

/// Parse one control stream's bytes. Strict by design: a single malformed
/// line makes the whole stream [`StreamUnreadable`] — an unreadable control
/// state is *unknown*, and a reader that cannot establish it must fail closed
/// rather than apply whatever prefix happened to parse. An empty (or absent,
/// before this is called) stream is a valid absence and is reported by the
/// caller, not reconstructed here from a zero.
pub fn parse_events(bytes: &[u8]) -> Result<Vec<ControlEvent>, StreamUnreadable> {
    let text = std::str::from_utf8(bytes).map_err(|e| StreamUnreadable {
        line_no: None,
        why: format!("control stream is not valid UTF-8: {e}"),
    })?;
    let mut events = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let line_no = index + 1;
        let event: ControlEvent = serde_json::from_str(line).map_err(|e| StreamUnreadable {
            line_no: Some(line_no),
            why: format!("cannot parse control record: {e}"),
        })?;
        if line.trim_end() != line {
            // No trailing whitespace drift between the checked bytes and the
            // bytes a writer appends; a line that grew junk is not a record.
            return Err(StreamUnreadable {
                line_no: Some(line_no),
                why: "record carries trailing whitespace".to_string(),
            });
        }
        event.validate().map_err(|why| StreamUnreadable {
            line_no: Some(line_no),
            why,
        })?;
        events.push(event);
    }
    Ok(events)
}

/// The same operation id recorded twice with different fields: an ambiguity a
/// consumer must not resolve by guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationConflict {
    pub operation_id: String,
}

impl std::fmt::Display for OperationConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "operation id `{}` was recorded twice with different fields",
            self.operation_id
        )
    }
}

impl std::error::Error for OperationConflict {}

/// Effective state of one session identity after folding every control record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    /// No record, or the latest state event is `unhide`.
    Visible,
    /// The latest state event is `hide`. Reversible by `unhide`.
    Hidden,
    /// A whole-session `purge` tombstone latched on; no later event lifts it.
    Purged,
    /// A `redact` tombstone exists. Message-level cutoff enforcement is a
    /// later phase; until it exists this state hides the session from
    /// listing readers, which errs on removing too much rather than too
    /// little, and no Phase-1 command can write a record that reaches it.
    RedactedThroughCutoff { recorded_cutoff: String },
}

impl Visibility {
    /// Whether an ordinary reader may see the session. Hidden, purged and
    /// redacted identities are all omitted; the administrative view is that a
    /// *state* exists, never that content is shown.
    pub fn omitted_from_readers(&self) -> bool {
        !matches!(self, Visibility::Visible)
    }
}

/// The folded, effective policy over the union of control streams. Policy is
/// per exact `(target machine partition, session id)`; identities with the
/// same session id in a different partition never share state.
#[derive(Debug, Default, Clone)]
pub struct ControlPolicy {
    state: BTreeMap<(String, String), Visibility>,
    event_count: usize,
}

impl ControlPolicy {
    /// Effective visibility of one identity; unrecorded identities are visible.
    pub fn visibility(&self, machine: &str, session_id: &str) -> Visibility {
        self.state
            .get(&(machine.to_string(), session_id.to_string()))
            .cloned()
            .unwrap_or(Visibility::Visible)
    }

    /// Whether ordinary readers omit this identity.
    pub fn omitted_from_readers(&self, machine: &str, session_id: &str) -> bool {
        self.visibility(machine, session_id).omitted_from_readers()
    }

    /// Whether no recorded state exists at all. Distinct from "the control
    /// stream could not be read", which never produces a policy value.
    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }

    /// How many folded records the policy covers (replays counted once).
    pub fn event_count(&self) -> usize {
        self.event_count
    }

    /// Identities with any recorded state, ordered. Names only — the
    /// administrative view (ADR-046 §1.2) shows that a state exists, and the
    /// identities are exactly what the user named themselves.
    pub fn recorded_identities(&self) -> Vec<(String, String)> {
        self.state.keys().cloned().collect()
    }
}

/// A fold that could not produce a policy: either a replayed operation id
/// carrying different fields, or a record that fails validation. Both mean
/// the control state is *unknown* and the caller must fail closed; neither is
/// ever an empty policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldError {
    Conflict(OperationConflict),
    Invalid(String),
}

impl std::fmt::Display for FoldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FoldError::Conflict(conflict) => write!(f, "{conflict}"),
            FoldError::Invalid(why) => write!(f, "invalid control record: {why}"),
        }
    }
}

impl std::error::Error for FoldError {}

/// Fold records from any number of streams into one effective policy.
///
/// Replays of the same operation id fold once (idempotent); the same id with
/// different fields is an [`OperationConflict`], which the caller must treat
/// as an unknown control state and fail closed. Ordering is by the parsed
/// `recorded_at` instant, tie-broken by operation id, so two streams written
/// by different machines reach the same order. A `purge` latch is permanent:
/// later hide/unhide/redact records cannot lift it (ADR-046 §1.3-4).
pub fn fold_events(events: Vec<ControlEvent>) -> Result<ControlPolicy, FoldError> {
    let mut unique: BTreeMap<String, ControlEvent> = BTreeMap::new();
    for event in events {
        event.validate().map_err(FoldError::Invalid)?;
        match unique.get(&event.operation_id) {
            Some(prior) if *prior == event => {}
            Some(_) => {
                return Err(FoldError::Conflict(OperationConflict {
                    operation_id: event.operation_id,
                }));
            }
            None => {
                unique.insert(event.operation_id.clone(), event);
            }
        }
    }
    let mut ordered: Vec<ControlEvent> = unique.into_values().collect();
    ordered.sort_by(|a, b| {
        let a_time = parse_instant(&a.recorded_at);
        let b_time = parse_instant(&b.recorded_at);
        (a_time, &a.operation_id).cmp(&(b_time, &b.operation_id))
    });
    let mut policy = ControlPolicy {
        event_count: ordered.len(),
        ..ControlPolicy::default()
    };
    for event in ordered {
        let key = (event.target_machine.clone(), event.session_id.clone());
        match event.action {
            ControlAction::Purge => {
                // A whole-session tombstone can never be undone (ADR-046
                // §1.4), so it latches over whatever state preceded it.
                policy.state.insert(key, Visibility::Purged);
            }
            ControlAction::Redact => {
                if let ControlScope::ContentThroughCutoff { recorded_cutoff } = event.scope {
                    policy
                        .state
                        .entry(key)
                        .and_modify(|state| {
                            if !matches!(state, Visibility::Purged) {
                                *state = Visibility::RedactedThroughCutoff {
                                    recorded_cutoff: recorded_cutoff.clone(),
                                };
                            }
                        })
                        .or_insert(Visibility::RedactedThroughCutoff { recorded_cutoff });
                }
            }
            ControlAction::Unhide => {
                policy
                    .state
                    .entry(key)
                    .and_modify(|state| {
                        if !matches!(state, Visibility::Purged) {
                            *state = Visibility::Visible;
                        }
                    })
                    .or_insert(Visibility::Visible);
            }
            ControlAction::Hide => {
                policy
                    .state
                    .entry(key)
                    .and_modify(|state| {
                        if !matches!(state, Visibility::Purged) {
                            *state = Visibility::Hidden;
                        }
                    })
                    .or_insert(Visibility::Hidden);
            }
        }
    }
    Ok(policy)
}

fn parse_instant(recorded_at: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(recorded_at).map_or_else(
        |_| {
            // reason: `validate` refused a non-RFC-3339 `recorded_at` before
            // the fold, so this arm is unreachable; the Unix epoch keeps the
            // ordering total rather than panicking in a fold of records that
            // passed validation.
            chrono::DateTime::from_timestamp(0, 0).unwrap_or_default()
        },
        |instant| instant.with_timezone(&chrono::Utc),
    )
}

/// Where the control stream of one machine partition lives in a stage.
pub fn control_path(stage_root: &Path, actor_machine: &str) -> PathBuf {
    stage_root
        .join(crate::manifest::META_DIR)
        .join(actor_machine)
        .join(FILE_NAME)
}

/// The stream on this actor's stage read back for the hide/unhide commands:
/// absent (nothing recorded yet) or the parsed records. An unreadable stream
/// is [`StreamUnreadable`] — the command must not treat it as "nothing
/// recorded" and append over it.
pub enum StageControlRead {
    Absent,
    Events(Vec<ControlEvent>),
}

pub fn read_stage_events(
    stage_root: &Path,
    actor_machine: &str,
) -> Result<StageControlRead, StreamUnreadable> {
    let path = control_path(stage_root, actor_machine);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(StageControlRead::Absent),
        Err(e) => {
            return Err(StreamUnreadable {
                line_no: None,
                why: format!("cannot read {}: {e}", path.display()),
            })
        }
    };
    Ok(StageControlRead::Events(parse_events(&bytes)?))
}

/// Mint an operation id for a new record: a 32-hex sha256 over the identities
/// and the nanosecond-precision timestamp, so two commands cannot collide
/// unless they are the same command recorded twice — which fold treats as the
/// replay it is.
pub fn new_operation_id(
    actor_machine: &str,
    target_machine: &str,
    session_id: &str,
    action: ControlAction,
    recorded_at: &str,
) -> String {
    let digest = Sha256::digest(format!(
        "{SCHEMA}\n{actor_machine}\n{target_machine}\n{session_id}\n{action:?}\n{recorded_at}"
    ));
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// Append one record to the actor machine's control stream, durably:
/// the stage mutation lock (the same one ingest and the audit append take),
/// then a single JSONL line, then an `fsync`, so a killed process leaves
/// either the old stream or the new record — never a torn tail a later read
/// would have to guess about.
///
/// **Read back before return**: the appended line is re-read from the file
/// and must equal exactly one record with the same operation id, so a
/// caller that hears `Ok` knows the durable state says so (ADR-046 §1.3).
///
/// The W306 identity fail-safe applies: a reserved fixture identity can be
/// recorded in a stage under the process temp directory (every test), never
/// in a real user's stage.
pub fn append_stage_event(stage_root: &Path, event: &ControlEvent) -> anyhow::Result<()> {
    event
        .validate()
        .map_err(|why| anyhow::anyhow!(StreamUnreadable { line_no: None, why }))?;
    crate::test_identity_guard::refuse_fixture_write(
        &[
            event.actor_machine.as_str(),
            event.target_machine.as_str(),
            event.session_id.as_str(),
        ],
        stage_root,
    )?;
    let path = control_path(stage_root, &event.actor_machine);
    std::fs::create_dir_all(
        path.parent()
            .context("control stream path has no parent directory")?,
    )?;
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    let _lock = crate::inbox::lock_stage(stage_root)?;
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(&line)?;
        file.sync_all()?;
    }
    match read_stage_events(stage_root, &event.actor_machine)? {
        StageControlRead::Events(events) if events.contains(event) => Ok(()),
        _ => Err(anyhow::anyhow!(StreamUnreadable {
            line_no: None,
            why: "control record was not read back after append".to_string(),
        })),
    }
}

/// Match an archived path by its trailing `meta/<machine>/deletion-control-v1.jsonl`
/// components and return that machine partition. The stage prefix differs per
/// machine and even per host, so — as for the provenance and activity sidecars
/// — the match is on trailing components, never on a stored absolute path.
pub fn control_stream_machine(path: &Path) -> Option<String> {
    let parts: Vec<_> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect();
    if parts.len() < 3 {
        return None;
    }
    if parts[parts.len() - 3] == crate::manifest::META_DIR && parts[parts.len() - 1] == FILE_NAME {
        return Some(parts[parts.len() - 2].to_string());
    }
    None
}

/// A control stream read from a destination. `sources` counts the distinct
/// machine partitions whose stream was read (an absent stream contributes
/// none); `unreadable_paths` names every stream that **exists but could not
/// be parsed** — policy callers fail closed on any entry of it, because a
/// corrupt control file is an unknown control state, never an empty policy.
///
/// `skipped_snapshots` and `unestablished_hosts` carry the damage tolerance
/// instead: the collector falls through unreadable *trees* the same way the
/// cumulative walks do, so a destination whose newest snapshot is damaged
/// keeps its established partial-answer semantics instead of gaining a new
/// hard failure. Summary readers surface these as their own partial-answer
/// vocabulary; the strict consumer ([`load_strict_reader_policy`]) refuses
/// when a host's state could not be established at all.
#[derive(Debug, Default)]
pub struct ArchiveControlRead {
    pub events: Vec<ControlEvent>,
    pub sources: usize,
    pub unreadable_paths: Vec<String>,
    pub skipped_snapshots: Vec<String>,
    pub unestablished_hosts: Vec<String>,
}

/// Read the newest copy of every machine's control stream from a destination,
/// newest snapshot per host (ADR-021 order, so a reclaimed stage that pushed
/// only its new batch still exposes the stream the last complete push carried).
///
/// Freshness boundary of this phase, stated honestly: only each host's
/// listable snapshots are consulted, newest first, and the first one that
/// lists cleanly is the freshest control state this read can establish.
/// Control files are stage-persistent and append-only — no current command
/// removes one — so the newest *readable* snapshot that carries one governs.
/// A snapshot whose tree cannot be read is skipped *with a note*, exactly
/// like the cumulative walks skip it, and a host whose every snapshot is
/// unreadable is named in `unestablished_hosts`. The full per-snapshot
/// re-scan that a *destructive* operation requires (the barrier, ADR-046
/// §1.3) is a later phase and does not belong to ordinary reads; the
/// residual window — a hide record that lives only inside a damaged
/// snapshot — is handled by the summary readers' partial semantics and is
/// documented as a phase boundary.
pub fn collect_archive_control(
    repo: &Repository<IndexedFullStatus>,
    hosts: &[(String, Vec<SnapshotFile>)],
) -> anyhow::Result<ArchiveControlRead> {
    let mut out = ArchiveControlRead::default();
    let mut seen_machines: BTreeSet<String> = BTreeSet::new();
    for (host, snaps) in hosts {
        let mut established = false;
        for snap in snaps {
            let hex_id = snap.id.to_hex();
            let short = &hex_id[..8.min(hex_id.len())];
            let listed = (|| -> anyhow::Result<Vec<(PathBuf, Node)>> {
                let root = repo
                    .node_from_snapshot_and_path(snap, "")
                    .with_context(|| format!("cannot open tree root"))?;
                repo.ls(&root, &LsOptions::default())
                    .and_then(|iter| iter.collect::<rustic_core::RusticResult<Vec<_>>>())
                    .with_context(|| format!("cannot list tree"))
            })();
            let entries = match listed {
                Ok(entries) => entries,
                Err(e) => {
                    out.skipped_snapshots.push(format!(
                        "host `{host}` snapshot {short}: control state could not be read from \
                         this snapshot ({e:#}) — an older one governs, so a hide recorded only \
                         here would not be honored by this run"
                    ));
                    continue;
                }
            };
            established = true;
            for (path, node) in entries {
                if node.node_type != NodeType::File {
                    continue;
                }
                let Some(machine) = control_stream_machine(&path) else {
                    continue;
                };
                if !seen_machines.insert(machine.clone()) {
                    // More than one path claims the same writer partition inside
                    // one snapshot: a stage restored from foreign metadata. Read
                    // only the first; the collision is below the threshold where
                    // guessing wrong changes a fold, because every copy names the
                    // same append-only record set up to its own push moment, and
                    // the newest-keyed read above already embraced that bound.
                    continue;
                }
                let mut bytes = Vec::new();
                repo.dump(&node, &mut bytes)
                    .with_context(|| format!("cannot read the control stream of `{machine}`"))?;
                match parse_events(&bytes) {
                    Ok(events) => {
                        out.sources += 1;
                        out.events.extend(events);
                    }
                    Err(e) => {
                        out.unreadable_paths
                            .push(format!("meta/{machine}/{FILE_NAME}: {e}"));
                    }
                }
            }
            break;
        }
        if !established {
            out.unestablished_hosts.push(host.clone());
        }
    }
    Ok(out)
}

/// Notes about what a policy load could not fully establish — not errors:
/// corrupt control files were (they fail closed); these say which parts of
/// the control state came from falling through damage the cumulative walks
/// also tolerate.
#[derive(Debug, Default, Clone)]
pub struct PolicyReadNotes {
    pub skipped_snapshots: Vec<String>,
    pub unestablished_hosts: Vec<String>,
}

impl PolicyReadNotes {
    pub fn is_quiet(&self) -> bool {
        self.skipped_snapshots.is_empty() && self.unestablished_hosts.is_empty()
    }
}

/// Fold the reader-facing policy for one open destination: the union of every
/// machine's control stream plus the notes of what damage allowed. Fails
/// closed only on a control file that exists but cannot be parsed.
pub fn load_reader_policy_with_notes(
    repo: &Repository<IndexedFullStatus>,
    hosts: &[(String, Vec<SnapshotFile>)],
) -> anyhow::Result<(ControlPolicy, PolicyReadNotes)> {
    let control = collect_archive_control(repo, hosts)?;
    if !control.unreadable_paths.is_empty() {
        anyhow::bail!(
            "control stream is unreadable ({} of {} sources): {} — this is an unknown \
             control state, not an empty one; reads must fail closed rather than apply \
             partial policy",
            control.unreadable_paths.len(),
            control.unreadable_paths.len() + control.sources,
            control.unreadable_paths.join("; ")
        );
    }
    let policy = fold_events(control.events).map_err(|issue| {
        anyhow::anyhow!("control stream refuses a policy: {issue} — treating it as unknown")
    })?;
    let notes = PolicyReadNotes {
        skipped_snapshots: control.skipped_snapshots,
        unestablished_hosts: control.unestablished_hosts,
    };
    Ok((policy, notes))
}

/// The strict variant a single-identity read uses: besides closing on
/// corrupt control files, it refuses when *any* host's control state could
/// not be established at all, because the one identity it is about to return
/// may be governed by exactly that host's records.
pub fn load_strict_reader_policy(
    repo: &Repository<IndexedFullStatus>,
    hosts: &[(String, Vec<SnapshotFile>)],
) -> anyhow::Result<ControlPolicy> {
    let (policy, notes) = load_reader_policy_with_notes(repo, hosts)?;
    if !notes.unestablished_hosts.is_empty() {
        anyhow::bail!(
            "the control state of machine(s) {} could not be read from any snapshot, so the \
             policy over this destination is unknown — this read fails closed rather than \
             answer from a policy it cannot establish",
            notes.unestablished_hosts.join(", ")
        );
    }
    Ok(policy)
}

/// The typed refusal a reader returns when it is asked for content the
/// effective control state omits. Distinguished from "cannot finish reading"
/// (exit 3 family): the read finished, and the answer is a deliberate policy
/// refusal — reversible by `unhide` when the state is a hide, permanent for
/// a tombstone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOmitted {
    pub machine: String,
    pub session_id: String,
    pub state: Visibility,
}

impl std::fmt::Display for SessionOmitted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "session `{}` in machine `{}` is omitted from readers by the control state ({})",
            crate::id::short_session_id(&self.session_id),
            self.machine,
            match self.state {
                Visibility::Visible => "visible",
                Visibility::Hidden => "hidden; `unhide` restores it",
                Visibility::Purged => "a whole-session purge tombstone",
                Visibility::RedactedThroughCutoff { .. } => "a redaction tombstone",
            }
        )
    }
}

impl std::error::Error for SessionOmitted {}

/// One session's accumulated occurrences across every snapshot of one
/// destination, for the purge preview. Counts only — no shard of the session
/// is dumped, so the preview cannot leak conversation text (W402).
#[derive(Debug, Default, Clone)]
pub struct OccurrenceInventory {
    /// How many snapshots the destination holds, across every host.
    pub snapshots_in_repo: usize,
    /// How many snapshots were walked. Equal to `snapshots_in_repo` when the
    /// status is [`DestinationStatus::Found`] or [`DestinationStatus::Absent`].
    pub snapshots_scanned: usize,
    /// Snapshots holding at least one occurrence, newest first by snapshot
    /// time: short id, shard-file count, byte total.
    pub snapshots_with_target: Vec<SnapshotOccurrence>,
    /// Total shard occurrences across every snapshot with the target.
    pub shard_occurrences: usize,
    /// Total bytes of those occurrences (tree metadata sizes; nothing dumped).
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct SnapshotOccurrence {
    pub snapshot_id: String,
    pub shard_files: usize,
    pub bytes: u64,
}

/// W402 status vocabulary for one destination. `Absent` is only reachable
/// after a complete read of every snapshot proved the target is not there; an
/// offline, unreadable or partially read destination is [`DestinationStatus::Unknown`],
/// never absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestinationStatus {
    Found,
    Absent,
    Unknown,
}

/// The read-only preview inventory of one exact `(machine, session)` identity
/// in one destination. Metadata tier only: opens, lists, counts, closes.
pub fn occurrence_inventory(
    store: &crate::store::BackupStore,
    mk: &MasterKey,
    machine: &str,
    session_id: &str,
) -> anyhow::Result<OccurrenceInventory> {
    let backends = store.backends()?;
    let (repo, _adoption) = crate::orphans::open_adopting(&store.cfg, &backends, mk)
        .context("open repository for purge preview")?;
    store.require_sound_packs(&repo)?;
    let snaps = repo
        .get_all_snapshots()
        .context("list snapshots for preview")?;
    let mut inventory = OccurrenceInventory {
        snapshots_in_repo: snaps.len(),
        ..OccurrenceInventory::default()
    };
    // Newest snapshot of each host first, within each host newest first, so
    // the report's snapshot list is deterministic rather than ls order.
    let mut ordered: Vec<SnapshotFile> = Vec::new();
    for (_host, host_snaps) in crate::readback::snapshots_by_host_newest_first(snaps) {
        ordered.extend(host_snaps);
    }
    for snap in ordered {
        let root = repo
            .node_from_snapshot_and_path(&snap, "")
            .context("read snapshot root for purge preview")?;
        let entries = repo
            .ls(&root, &LsOptions::default())
            .and_then(|iter| iter.collect::<rustic_core::RusticResult<Vec<_>>>())
            .context("list snapshot tree for purge preview")?;
        inventory.snapshots_scanned += 1;
        let mut shard_files = 0usize;
        let mut bytes = 0u64;
        for (path, node) in entries {
            if node.node_type != NodeType::File {
                continue;
            }
            let Some((found_machine, found_session, _shard)) =
                crate::readback::bucket_shard_path(&path)
            else {
                continue;
            };
            if found_machine != machine || found_session != session_id {
                continue;
            }
            shard_files += 1;
            bytes += node.meta.size;
        }
        // A walk failure above is an Err, so reaching here means this
        // snapshot was completely listed: counting it is sound either way.
        if shard_files > 0 {
            inventory.snapshots_with_target.push(SnapshotOccurrence {
                snapshot_id: snap.id.to_hex().as_str()[..8].to_string(),
                shard_files,
                bytes,
            });
            inventory.shard_occurrences += shard_files;
            inventory.bytes += bytes;
        }
    }
    Ok(inventory)
}

/// Build one validated hide/unhide record for the CLI to append. The
/// `recorded_at` timestamp is minted here, once, with nanosecond precision,
/// so the operation id derived from it cannot repeat.
pub fn new_state_event(
    action: ControlAction,
    actor_machine: &str,
    target_machine: &str,
    session_id: &str,
    reason: Option<&str>,
) -> Result<ControlEvent, StreamUnreadable> {
    let recorded_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let reason = reason
        .map(|label| label.trim().to_string())
        .filter(|label| !label.is_empty());
    let event = ControlEvent {
        schema: SCHEMA.to_string(),
        operation_id: new_operation_id(
            actor_machine,
            target_machine,
            session_id,
            action,
            &recorded_at,
        ),
        recorded_at,
        action,
        target_machine: target_machine.to_string(),
        session_id: session_id.to_string(),
        actor_machine: actor_machine.to_string(),
        scope: ControlScope::Session,
        reason,
    };
    event
        .validate()
        .map_err(|why| StreamUnreadable { line_no: None, why })?;
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(action: ControlAction, at: &str) -> ControlEvent {
        ControlEvent {
            schema: SCHEMA.to_string(),
            operation_id: format!("op-{action:?}-{at}"),
            recorded_at: at.to_string(),
            action,
            target_machine: "m1".to_string(),
            session_id: "chatgpt.alpha".to_string(),
            actor_machine: "m1".to_string(),
            scope: ControlScope::Session,
            reason: None,
        }
    }

    #[test]
    fn hide_then_unhide_folds_visible() {
        let hidden = event(ControlAction::Hide, "2026-10-05T10:00:00+00:00");
        let unhidden = event(ControlAction::Unhide, "2026-10-05T11:00:00+00:00");
        let policy = fold_events(vec![hidden, unhidden]).expect("fold");
        assert_eq!(
            policy.visibility("m1", "chatgpt.alpha"),
            Visibility::Visible
        );
    }

    #[test]
    fn unhide_never_latches_visible_over_a_later_hide() {
        let unhidden = event(ControlAction::Unhide, "2026-10-05T10:00:00+00:00");
        let hidden = event(ControlAction::Hide, "2026-10-05T11:00:00+00:00");
        let policy = fold_events(vec![unhidden, hidden]).expect("fold");
        assert!(policy.omitted_from_readers("m1", "chatgpt.alpha"));
    }

    #[test]
    fn replayed_records_fold_once() {
        let hidden = event(ControlAction::Hide, "2026-10-05T10:00:00+00:00");
        let replay = hidden.clone();
        let policy = fold_events(vec![hidden, replay]).expect("fold");
        assert_eq!(policy.event_count(), 1);
    }

    #[test]
    fn same_operation_id_with_different_fields_refuses_to_fold() {
        let mut first = event(ControlAction::Hide, "2026-10-05T10:00:00+00:00");
        first.reason = Some("typo".to_string());
        let second = ControlEvent {
            reason: Some("corrected".to_string()),
            ..first.clone()
        };
        let conflict = matches!(
            fold_events(vec![first, second]),
            Err(FoldError::Conflict(_))
        );
        assert!(
            conflict,
            "conflicting operation id reuse must refuse the fold"
        );
    }

    #[test]
    fn a_purge_tombstone_is_never_lifted_by_later_state_events() {
        let purged = ControlEvent {
            scope: ControlScope::SessionForever,
            ..event(ControlAction::Purge, "2026-10-05T09:00:00+00:00")
        };
        let unhide = event(ControlAction::Unhide, "2026-10-05T12:00:00+00:00");
        let redact = ControlEvent {
            scope: ControlScope::ContentThroughCutoff {
                recorded_cutoff: "2026-01-01T00:00:00Z".to_string(),
            },
            ..event(ControlAction::Redact, "2026-10-05T13:00:00+00:00")
        };
        let policy = fold_events(vec![purged, unhide, redact]).expect("fold");
        assert_eq!(policy.visibility("m1", "chatgpt.alpha"), Visibility::Purged);
    }

    #[test]
    fn partition_isolation_same_id_elsewhere_is_untouched() {
        let hidden = event(ControlAction::Hide, "2026-10-05T10:00:00+00:00");
        let policy = fold_events(vec![hidden]).expect("fold");
        assert!(policy.omitted_from_readers("m1", "chatgpt.alpha"));
        assert!(!policy.omitted_from_readers("m2", "chatgpt.alpha"));
    }

    #[test]
    fn one_malformed_line_makes_the_whole_stream_unknown() {
        let mut stream = serde_json::to_vec(&event(ControlAction::Hide, "2026-10-05T10:00:00Z"))
            .expect("serialize");
        stream.push(b'\n');
        stream.extend_from_slice(b"{\"schema\":\"nope\"}\n");
        let error = parse_events(&stream).expect_err("malformed record");
        assert_eq!(error.line_no, Some(2));
    }

    #[test]
    fn an_empty_stream_is_valid_and_empty() {
        assert!(parse_events(b"").expect("empty stream").is_empty());
        assert!(parse_events(b"\n\n").expect("blank lines").is_empty());
    }

    #[test]
    fn not_utf8_is_unknown() {
        assert!(parse_events(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn a_provenance_observation_never_parses_as_a_control_record() {
        // Phase 0 policy point: source-side observations are a different
        // schema, so no receiver path can mistake an observation stream for a
        // user control instruction, and no parse of one produces a tombstone.
        let observation = serde_json::json!({
            "kind": "source-observation",
            "session_id": "chatgpt.alpha",
            "since": "2026-10-05T10:00:00Z"
        });
        let mut stream = serde_json::to_vec(&observation).expect("serialize");
        stream.push(b'\n');
        assert!(parse_events(&stream).is_err());
    }

    #[test]
    fn reason_labels_are_bounded() {
        let mut long_reason = event(ControlAction::Hide, "2026-10-05T10:00:00Z");
        long_reason.reason = Some("x".repeat(MAX_REASON_BYTES + 1));
        let why = long_reason.validate().expect_err("bounded");
        assert!(why.contains("reason"), "{why}");
        let mut multiline = long_reason.clone();
        multiline.reason = Some("line\nbreak".to_string());
        assert!(multiline.validate().is_err());
    }

    #[test]
    fn archived_control_paths_match_by_trailing_marker() {
        assert_eq!(
            control_stream_machine(Path::new("/stage/meta/m1/deletion-control-v1.jsonl")),
            Some("m1".to_string())
        );
        assert_eq!(
            control_stream_machine(Path::new("/other/root/meta/m2/deletion-control-v1.jsonl")),
            Some("m2".to_string())
        );
        assert_eq!(
            control_stream_machine(Path::new("sessions/m1/deletion-control-v1.jsonl")),
            None
        );
        assert_eq!(
            control_stream_machine(Path::new("meta/m1/other-v1.jsonl")),
            None
        );
    }

    #[test]
    fn hide_scope_is_the_whole_session_record() {
        let hide = event(ControlAction::Hide, "2026-10-05T10:00:00Z");
        assert!(hide.validate().is_ok());
        let purge_with_session_scope = event(ControlAction::Purge, "2026-10-05T10:00:01Z");
        assert!(purge_with_session_scope.validate().is_err());
        let redact_without_cutoff = ControlEvent {
            scope: ControlScope::ContentThroughCutoff {
                recorded_cutoff: String::new(),
            },
            ..event(ControlAction::Redact, "2026-10-05T10:00:02Z")
        };
        assert!(redact_without_cutoff.validate().is_err());
    }
}
