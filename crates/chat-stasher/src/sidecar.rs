//! sidecar — pure wiring helpers for the `activity-index` and `overview`
//! subcommands (ADR-017).
//!
//! The two new CLI commands read the activity sidecar index
//! (`meta/<machine>/activity-v1.jsonl`) on both sides of the pipeline:
//!
//! * `activity-index` *writes* it from sealed stage shards, and
//! * `overview` *reads* it back out of a destination repository to draw the
//!   machine × harness overview.
//!
//! Everything here is deliberately pure and IO-free so it is trivially
//! unit-testable; the repository / filesystem / CLI plumbing lives in `main.rs`.
//! This module never prints or returns session content.

use std::path::{Component, Path, PathBuf};

use crate::activity::{ActivityRow, TimeSource as ActivityTimeSource};
use crate::overview::{OverviewRow, TimeSource as OverviewTimeSource};

/// Which destination a **derived** activity index describes.
///
/// A destination resolved from the config has a name; one named only by
/// `--repo` does not, and the repository path has to stand in for it.
#[derive(Debug, Clone, Copy)]
pub enum DerivedIndexScope<'a> {
    Destination(&'a str),
    Repo(&'a Path),
}

/// Where a read-only archive rebuild writes the index it derived.
///
/// `meta/<machine>/activity-v1.jsonl` inside the archive is written by the
/// machine that owns that partition (ADR-017: one writer per partition), so a
/// machine rebuilding a **lost** machine's index has nowhere in the archive to
/// put the result. It goes to this machine's cache instead:
///
/// `<platform cache>/chat-stasher/activity/destination/<name>/<machine>/activity-v1.jsonl`
/// `<platform cache>/chat-stasher/activity/repo/<path…>/<machine>/activity-v1.jsonl`
///
/// The root is the platform cache directory (the ADR-034 shape) and a
/// **sibling** of the body cache rather than a child of it: the body cache
/// writes, counts and deletes only inside a root it marked, so a foreign file
/// inside that root would be an unaccounted entry against its quota.
///
/// The `destination/` / `repo/` segment keeps the two scopes apart, and the
/// repository scope keeps the path's own components, so neither a destination
/// named after a path component nor two repositories with the same base name
/// can land on each other's derived index.
///
/// Nothing here is archived and nothing here is authoritative: the file is a
/// cache, recomputable from the archive, and its absence means "not rebuilt
/// yet", never "that machine has no sessions".
pub fn derived_activity_index_path(scope: DerivedIndexScope<'_>, machine: &str) -> PathBuf {
    let root = derived_index_root();
    match scope {
        DerivedIndexScope::Destination(name) => root
            .join("destination")
            .join(sanitize_component(name))
            .join(sanitize_component(machine))
            .join("activity-v1.jsonl"),
        DerivedIndexScope::Repo(path) => {
            let mut out = root.join("repo");
            for component in path.components() {
                match component {
                    // The root marker (`/`, `\`) carries no name and cannot
                    // distinguish two paths, so it contributes nothing. A
                    // Windows drive prefix (`C:`) does distinguish them and is
                    // kept, sanitized.
                    Component::RootDir => continue,
                    Component::Prefix(prefix) => {
                        out = out.join(sanitize_component(&prefix.as_os_str().to_string_lossy()));
                    }
                    Component::Normal(name) => {
                        out = out.join(sanitize_component(&name.to_string_lossy()));
                    }
                    // Kept distinct rather than dropped: `/a/../b` and `/a/b`
                    // are different paths, and a sanitizer that folded them
                    // into one derived index would answer for the wrong
                    // repository.
                    Component::CurDir => out = out.join("_"),
                    Component::ParentDir => out = out.join("__"),
                }
            }
            out.join(sanitize_component(machine))
                .join("activity-v1.jsonl")
        }
    }
}

/// The platform cache root for derived indexes, or a home-relative fallback
/// when the platform offers no cache directory (the same fallback
/// [`crate::body_cache::default_root`] documents).
fn derived_index_root() -> PathBuf {
    match crate::scanner::user_cache_dirs().into_iter().next() {
        Some(dir) => dir.join("chat-stasher").join("activity"),
        None => crate::config::home_dir()
            .join(".cache")
            .join("chat-stasher")
            .join("activity"),
    }
}

/// One path component that can only ever name a directory below the derived
/// root: `.` and `..` are replaced, and so is every character a filesystem or a
/// Windows path could read as structure. A destination name comes from the
/// user's config, so this is the boundary that stops it from escaping the
/// cache.
fn sanitize_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out == "." || out == ".." {
        out = "_".to_string();
    }
    out
}

/// Infer the harness label from an archived session directory name.
///
/// Archived session dirs are the canonical `<source>.<machine>.<native-id>`
/// ids (see `models::SessionIdentity::id`), so the harness is the leading
/// dot-segment: `opencode.<machine>.<uuid>` -> `opencode`,
/// `claude-code.<machine>.<uuid>` -> `claude-code`. Short forms that carry the
/// harness name directly before the short-id separator are also handled, by
/// taking the prefix before the *first* `.` **or** `~`, whichever comes first:
/// `opencode~abc123` -> `opencode`, `cursor.d~xxx` -> `cursor`.
///
/// `None` when the id yields no usable prefix (empty or delimiter-led).
pub fn infer_harness(session_id: &str) -> Option<String> {
    let end = session_id.find(['.', '~']).unwrap_or(session_id.len());
    let head = &session_id[..end];
    if head.is_empty() {
        None
    } else {
        Some(head.to_string())
    }
}

/// Which producer archived a session id, read off the id's own shape.
///
/// [`crate::id`] defines **two** identity forms, and the number of dot
/// segments says which one an id is written in:
///
/// ```text
/// <platform>.<native-id>                  web capture   (`inbox::session_dir_id`)
/// <platform>.<machine>.<native-id>        local harness (`SessionIdentity::id`)
/// ```
///
/// The head alone cannot answer this, and for one id the difference is not
/// cosmetic: `grok` is both a web platform the extension captures and a local
/// CLI harness the scanner walks, so `grok.<uuid>` (grok.com conversations) and
/// `grok.<machine>.<uuid>` (that machine's Grok CLI sessions) share one prefix
/// while describing two sources with two independent lifetimes. Reading them as
/// one platform reports the healthy leg's date for both — the reassuring answer
/// and the wrong one, which is why every per-platform arrival number must split
/// on this.
///
/// `None` means the producer is not knowable from the shape: an id with no
/// harness prefix, or the display short form of one
/// ([`crate::id::short_session_id`]), whose head truncates the machine segment
/// away. That is a third state, not either producer, and a caller must not
/// render it as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdProducer {
    /// `<platform>.<native-id>` — the browser extension's capture of a web chat.
    WebCapture,
    /// `<platform>.<machine>.<native-id>` — the local scanner's harness session.
    LocalHarness,
}

impl IdProducer {
    /// The words a report prints for this producer. Neither is a synonym of the
    /// other: they name the two identity forms, not two degrees of one thing.
    pub fn label(self) -> &'static str {
        match self {
            IdProducer::WebCapture => "web capture",
            IdProducer::LocalHarness => "local harness",
        }
    }
}

/// The producer an archived id was written in, by its segment count.
///
/// Two segments is the web form; three or more is the local form. The boundary
/// is exact because the `<machine>` component is written by
/// [`crate::id::normalize_machine`], which maps every character outside
/// `[a-z0-9-]` — a `.` included — to `-`: a third segment therefore exists only
/// when the id really is `<platform>.<machine>.<native-id>`, however many dots
/// the native id itself carries.
///
/// The head is read with [`infer_harness`] first, so an id with no usable prefix
/// answers `None` here for the same reason it does there.
pub fn id_producer(session_id: &str) -> Option<IdProducer> {
    if infer_harness(session_id).is_none() {
        return None;
    }
    // The display short form is `head(8) + '~' + tag(6)` of the *whole* id, so
    // its dot count is whatever falls inside those eight characters — reading a
    // producer off it would be a guess about a segment the form deleted. A
    // bounded archived id ([`crate::id::bounded_path_component`]) also carries a
    // `~`, but its tag is 32 digits and its segments survive, so only this exact
    // shape is refused.
    if is_display_short_id(session_id) {
        return None;
    }
    let separators = session_id.matches('.').count();
    if separators == 0 {
        // A bare name with no `.` is neither form: `infer_harness` hands back the
        // whole string as the head, and there is no second component to say which
        // producer wrote it.
        return None;
    }
    Some(if separators >= 2 {
        IdProducer::LocalHarness
    } else {
        IdProducer::WebCapture
    })
}

/// True for exactly the output of [`crate::id::short_session_id`]: 15 bytes with
/// the short-id separator at position 8. See [`id_producer`].
fn is_display_short_id(session_id: &str) -> bool {
    let bytes = session_id.as_bytes();
    bytes.len() == crate::id::SHORT_ID_LEN && bytes.get(crate::id::SHORT_ID_HEAD) == Some(&b'~')
}

/// Which harness ids a given set of archived rows holds from **both** producers.
///
/// Built from the rows a report is about to print, not from a list of ids this
/// build believes collide: a shared id space is a fact about *this* archive, and
/// an archive holding only Grok CLI sessions has one grok producer and prints
/// one grok row. Group by [`ProducerSplit::label`] to keep a merged count from
/// being printed.
///
/// Each item is `(harness, session_id)` — the harness the caller already knows
/// for the row (from the index or `infer_harness`) and the id whose shape is
/// read. Taking the pair rather than re-deriving the harness from the id keeps
/// the split keyed under the same name the caller groups by.
#[derive(Debug, Clone, Default)]
pub struct ProducerSplit {
    shared: std::collections::BTreeSet<String>,
}

impl ProducerSplit {
    /// Record which harness ids appear in both shapes among the rows.
    pub fn scan<'a>(rows: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut shapes: std::collections::BTreeMap<String, (bool, bool)> =
            std::collections::BTreeMap::new();
        for (harness, id) in rows {
            if harness.is_empty() {
                continue;
            }
            let entry = shapes.entry(harness.to_string()).or_insert((false, false));
            match id_producer(id) {
                Some(IdProducer::WebCapture) => entry.0 = true,
                Some(IdProducer::LocalHarness) => entry.1 = true,
                // Not knowable: it belongs to no producer's count, and it must
                // not make an id space look shared on its own.
                None => {}
            }
        }
        ProducerSplit {
            shared: shapes
                .into_iter()
                .filter(|(_, (web, local))| *web && *local)
                .map(|(harness, _)| harness)
                .collect(),
        }
    }

    /// True when this archive holds both producers under `harness`, so one count
    /// under that name would mix them.
    pub fn is_shared(&self, harness: &str) -> bool {
        self.shared.contains(harness)
    }

    /// The label one report row carries for `(harness, session_id)`: the bare
    /// harness id when its id space has one producer here, and
    /// `<harness> (<producer>)` when it has two. Grouping by this label is what
    /// stops a merged number being printed, while leaving every single-producer
    /// platform named exactly as it was.
    ///
    /// A row whose producer cannot be named keeps the bare id: dropping it would
    /// remove a session from the report, and an unnameable producer is not the
    /// same state as no session. An empty harness (the caller's own no-prefix
    /// bucket) also keeps its own name.
    pub fn label(&self, harness: &str, session_id: &str) -> String {
        if harness.is_empty() || !self.is_shared(harness) {
            return harness.to_string();
        }
        match id_producer(session_id) {
            Some(producer) => format!("{harness} ({})", producer.label()),
            None => harness.to_string(),
        }
    }
}

/// Match an archived path against the `meta/<machine>/activity-v1.jsonl`
/// marker, returning the machine name.
///
/// The archived tree mirrors each machine's *absolute* stage path
/// (`snapshot.paths` minus the leading `/`), so the prefix differs per machine
/// and per host and must never be reconstructed from a local path. Match only
/// the trailing components: `meta` / `<machine>` / `activity-v1.jsonl`. This is
/// the same "marker anywhere, suffix wins" discipline as
/// [`crate::readback::bucket_shard_path`].
pub fn activity_index_machine(path: &Path) -> Option<String> {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let n = comps.len();
    if n < 3 {
        return None;
    }
    if comps[n - 1] != "activity-v1.jsonl" || comps[n - 3] != "meta" {
        return None;
    }
    let machine = comps[n - 2];
    if machine.is_empty() {
        return None;
    }
    Some(machine.to_string())
}

/// Match an archived path against the `meta/<machine>/machine.json` marker
/// (ADR-018), returning the machine name.
///
/// Same trailing-component discipline as [`activity_index_machine`]: the
/// archived tree mirrors each machine's absolute stage path, so the prefix
/// differs per machine and must never be reconstructed locally.
pub fn declaration_machine(path: &Path) -> Option<String> {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let n = comps.len();
    if n < 3 {
        return None;
    }
    if comps[n - 1] != "machine.json" || comps[n - 3] != "meta" {
        return None;
    }
    let machine = comps[n - 2];
    if machine.is_empty() {
        return None;
    }
    Some(machine.to_string())
}

/// Match the archived `meta/<machine>/writer.json` marker.
pub fn writer_machine(path: &Path) -> Option<String> {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let n = comps.len();
    if n < 3 || comps[n - 1] != "writer.json" || comps[n - 3] != "meta" {
        return None;
    }
    let machine = comps[n - 2];
    (!machine.is_empty()).then(|| machine.to_string())
}

/// Last chat-stasher version that pushed this machine partition.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WriterVersionRecord {
    pub machine_id: String,
    pub chat_stasher_version: String,
}

/// One machine partition's archived writer version, judged against the newest
/// writer seen across the archive.
///
/// Three states are kept apart: a version that was read, a version that was
/// never recorded (any push from ≤0.3.0 carried none), and a record that exists
/// but could not be read. Only the first two are comparisons; the third is
/// `behind_newest_writer: None` — "unknown" must never be answered as "behind"
/// or as "fine".
#[derive(Debug, Clone, serde::Serialize)]
pub struct MachineWriterStatus {
    pub machine: String,
    pub chat_stasher_version: Option<String>,
    pub version_recorded: bool,
    pub version_unreadable: bool,
    pub behind_newest_writer: Option<bool>,
}

/// Judge every machine that has a snapshot against the newest recorded writer.
pub fn writer_statuses(
    machines: &std::collections::BTreeSet<String>,
    writers: &std::collections::BTreeMap<String, WriterVersionRecord>,
    unreadable: &std::collections::BTreeSet<String>,
) -> Vec<MachineWriterStatus> {
    let newest = writers
        .values()
        .map(|writer| writer.chat_stasher_version.as_str())
        .max_by(|a, b| compare_versions(a, b));
    machines
        .iter()
        .map(|machine| {
            let version = writers.get(machine).map(|w| w.chat_stasher_version.clone());
            let behind = if unreadable.contains(machine) {
                None
            } else if let Some(version) = version.as_deref() {
                newest.map(|newest| compare_versions(version, newest).is_lt())
            } else {
                Some(true)
            };
            MachineWriterStatus {
                machine: machine.clone(),
                version_recorded: version.is_some(),
                version_unreadable: unreadable.contains(machine),
                chat_stasher_version: version,
                behind_newest_writer: behind,
            }
        })
        .collect()
}

/// Was one machine's archived activity index written by a CLI **older than
/// `running`**?
///
/// The writer version and the index travel in the same snapshot, written by the
/// same binary ([`crate::sidecar`]'s counterpart in the CLI rebuilds the index
/// and then records the version), so a version behind `running` is what makes
/// the archived index provably stale — that is the state `overview` renders as
/// `behind (version not recorded — written by ≤0.3.0)` for a record that is
/// simply absent.
///
/// Returns `None` when the record could not be read: an unreadable record says
/// nothing about freshness in either direction.
pub fn index_writer_is_behind(
    recorded: Option<&str>,
    unreadable: bool,
    running: &str,
) -> Option<bool> {
    if unreadable {
        return None;
    }
    Some(match recorded {
        Some(recorded) => compare_versions(recorded, running).is_lt(),
        // No record at all: every version that could have written it predates
        // writer-version recording, so it is behind whatever is running now.
        None => true,
    })
}

/// Compare numeric release components first, then prerelease suffixes; a
/// release without a suffix sorts after its prereleases.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(version: &str) -> ([u64; 3], Option<&str>) {
        let (release, suffix) = version
            .split_once('-')
            .map_or((version, None), |(a, b)| (a, Some(b)));
        let mut nums = release.split('.').filter_map(|p| p.parse::<u64>().ok());
        (
            [
                // reason: a missing semantic-version major component is the legacy zero component.
                nums.next().unwrap_or(0),
                // reason: a missing semantic-version minor component is zero by version syntax.
                nums.next().unwrap_or(0),
                // reason: a missing semantic-version patch component is zero by version syntax.
                nums.next().unwrap_or(0),
            ],
            suffix,
        )
    }
    let (av, asuffix) = parts(a);
    let (bv, bsuffix) = parts(b);
    av.cmp(&bv).then_with(|| match (asuffix, bsuffix) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => a.cmp(b),
    })
}

/// Match an archived path against the `meta/<machine>/label-by-<writer>.json`
/// marker (ADR-018), returning `(machine, writer)`.
///
/// A label is one machine's opinion about another; the writer is the machine
/// id in the file name, so the overview can tell who currently holds the
/// winning label. Same trailing-component discipline as
/// [`activity_index_machine`].
pub fn label_record_machine(path: &Path) -> Option<(String, String)> {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let n = comps.len();
    if n < 3 || comps[n - 3] != "meta" {
        return None;
    }
    let name = comps[n - 1];
    let writer = name
        .strip_prefix("label-by-")
        .and_then(|w| w.strip_suffix(".json"));
    let machine = comps[n - 2];
    if machine.is_empty() {
        return None;
    }
    writer.map(|w| (machine.to_string(), w.to_string()))
}

/// Convert an archived [`ActivityRow`] into an [`OverviewRow`] for rendering.
///
/// The two modules each define their own `TimeSource` so neither imports the
/// other; this is the single place that maps the activity module's serialised
/// shape onto the overview module's render shape.
pub fn to_overview_row(row: &ActivityRow) -> OverviewRow {
    OverviewRow {
        session_id: row.session_id.clone(),
        machine: row.machine.clone(),
        harness: row.harness.clone(),
        first_unix: row.first_unix,
        last_unix: row.last_unix,
        line_count: row.line_count,
        provenance: row.provenance.clone(),
        account_keys: row.account_keys.clone(),
        time_source: match &row.time_source {
            ActivityTimeSource::Exact => OverviewTimeSource::Exact,
            ActivityTimeSource::Inferred { how } => {
                OverviewTimeSource::Inferred { how: how.clone() }
            }
            ActivityTimeSource::Messages { exact } => {
                OverviewTimeSource::Messages { exact: *exact }
            }
            ActivityTimeSource::ListUpdated => OverviewTimeSource::ListUpdated,
            ActivityTimeSource::NoConversationContent => OverviewTimeSource::NoConversationContent,
            ActivityTimeSource::PartialRange { how, why } => OverviewTimeSource::PartialRange {
                how: how.clone(),
                why: why.clone(),
            },
            ActivityTimeSource::Unknown { why } => OverviewTimeSource::Unknown { why: why.clone() },
        },
    }
}

/// Machines that have a snapshot but no activity index — sorted, for stable
/// output.
///
/// `overview` must never let such a machine vanish silently from the total
/// (that would fold "no index" into "no sessions"); the caller lists every
/// member of this set explicitly.
pub fn missing_index_machines(
    snapshot_machines: &std::collections::BTreeSet<String>,
    index_machines: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    snapshot_machines
        .iter()
        .filter(|m| !index_machines.contains(*m))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    // ------------------------------------------------------------ infer_harness
    #[test]
    fn canonical_id_harness_is_leading_dot_segment() {
        assert_eq!(
            infer_harness("opencode.mbp-2.abc-123").as_deref(),
            Some("opencode")
        );
        assert_eq!(
            infer_harness("claude-code.mbp.019bf00d-97b6-7eb2-9bf8-eacbacc09765").as_deref(),
            Some("claude-code")
        );
        assert_eq!(
            infer_harness("codex.mbp.019bf00d-97b6-7eb2-9bf8-eacbacc09765").as_deref(),
            Some("codex")
        );
        assert_eq!(
            infer_harness("cursor.mbp.session-1").as_deref(),
            Some("cursor")
        );
    }

    #[test]
    fn short_id_forms_use_first_delimiter() {
        // `~` first -> harness before it.
        assert_eq!(
            infer_harness("opencode~abc123").as_deref(),
            Some("opencode")
        );
        // `.` first -> harness before it, matching the task's shape.
        assert_eq!(infer_harness("cursor.d~xxx").as_deref(), Some("cursor"));
        assert_eq!(
            infer_harness("deepseek~e4009d").as_deref(),
            Some("deepseek")
        );
    }

    #[test]
    fn empty_or_delimiter_led_id_has_no_harness() {
        assert_eq!(infer_harness(""), None);
        assert_eq!(infer_harness(".machine.uuid"), None);
        assert_eq!(infer_harness("~abc"), None);
    }

    // -------------------------------------------------------------- id_producer
    /// The evidence the split exists for: one `grok` id space, two producers,
    /// and the shape is the only thing in the id that tells them apart.
    #[test]
    fn the_two_grok_shapes_are_read_apart() {
        assert_eq!(
            id_producer("grok.019bf00d-97b6-7eb2-9bf8-eacbacc09765"),
            Some(IdProducer::WebCapture)
        );
        assert_eq!(
            id_producer("grok.mbp-2.019bf00d-97b6-7eb2-9bf8-eacbacc09765"),
            Some(IdProducer::LocalHarness)
        );
    }

    /// A native id with dots inside it must not be read as a machine component:
    /// the local form can have any number of segments, so the rule is "two or
    /// more separators", and `normalize_machine` cannot emit a `.` at all.
    #[test]
    fn extra_segments_stay_the_local_form_and_one_separator_stays_the_web_form() {
        for id in [
            "codex.mbp.019bf00d.jsonl",
            "opencode.mbp.session.2.v1",
            "grok.mbp.019bf00d-97b6-7eb2-9bf8-eacbacc09765",
        ] {
            assert_eq!(id_producer(id), Some(IdProducer::LocalHarness), "{id}");
        }
        for id in [
            "grok.019bf00d",
            "chatgpt.d41f6a2b",
            "deepseek.6d61696e2d3031",
        ] {
            assert_eq!(id_producer(id), Some(IdProducer::WebCapture), "{id}");
        }
    }

    /// The one thing the shape rule cannot answer, stated rather than hidden:
    /// a web platform whose *own* session id carries a `.` writes an id with
    /// three segments, which is the local form's shape, and nothing in the id
    /// says otherwise. `sanitize_component` preserves `.`, so the shape is
    /// admitted, and no measured web platform does it today — every one is
    /// uuid-shaped. An id space with no collision is unaffected either way, so
    /// the misreading only surfaces where two producers already share a name.
    #[test]
    fn a_dotted_web_session_id_is_read_as_the_local_form_it_looks_like() {
        assert_eq!(
            id_producer("deepseek.a.b"),
            Some(IdProducer::LocalHarness),
            "three segments is the local shape, whatever wrote them"
        );
    }

    /// A name with no `.` at all is neither form, and an id whose head is empty
    /// is not a producer's row either. Both answer `None` — a third state, never
    /// one of the two producers.
    #[test]
    fn a_shape_that_is_neither_form_is_not_claimed_as_either() {
        assert_eq!(id_producer("hidden-session"), None);
        assert_eq!(id_producer(""), None);
        assert_eq!(id_producer(".machine.uuid"), None);
        assert_eq!(id_producer("~abc"), None);
    }

    /// The display short form truncates the machine segment away, so its dot
    /// count is an accident of where eight characters happened to end. Reading a
    /// producer off it would be a guess, and a guess is what hides a stall. The
    /// bounded archived form also carries a `~`, and keeps its segments, so this
    /// refuses one shape and not the other.
    #[test]
    fn a_short_id_is_not_read_as_an_identity_shape() {
        use crate::id::{bounded_path_component, short_session_id, SessionIdentity};
        let local = "grok.mbp-2.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
        let web = "grok.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
        assert_eq!(id_producer(local), Some(IdProducer::LocalHarness));
        assert_eq!(id_producer(web), Some(IdProducer::WebCapture));
        // Both short forms are 15 characters with the separator at index 8, and
        // neither says which shape it was truncated from.
        let short_local = short_session_id(local);
        let short_web = short_session_id(web);
        for short in [&short_local, &short_web] {
            assert_eq!(short.len(), crate::id::SHORT_ID_LEN, "{short}");
            assert_eq!(
                id_producer(short),
                None,
                "a truncated shape proves nothing about the shape it came from"
            );
        }
        assert_eq!(id_producer(&short_session_id("019bf00d-97b6")), None);

        // A bounded *archived* id is a full identity that happens to carry a `~`
        // with a 32-digit tag, and it keeps the segments that say its shape.
        let long_native = format!("019bf00d-{}", "7eb2".repeat(70));
        let bounded_local = SessionIdentity {
            source_short: "grok",
            machine: "mbp-2".into(),
            native_id: long_native,
        }
        .id();
        assert!(bounded_local.contains('~'), "{bounded_local}");
        assert_eq!(
            bounded_local.rsplit_once('~').map(|(_, tag)| tag.len()),
            Some(crate::id::PATH_COMPONENT_TAG_HEX),
            "the bounded tag is not the short-form tag"
        );
        assert_eq!(id_producer(&bounded_local), Some(IdProducer::LocalHarness));
        // The web form bounded the same way stays the web form.
        let bounded_web = bounded_path_component(&format!("grok.{}", "a".repeat(400)), 255);
        assert!(bounded_web.contains('~'));
        assert_eq!(id_producer(&bounded_web), Some(IdProducer::WebCapture));
    }

    #[test]
    fn producer_labels_name_the_form_and_never_the_platform() {
        assert_eq!(IdProducer::WebCapture.label(), "web capture");
        assert_eq!(IdProducer::LocalHarness.label(), "local harness");
    }

    // ----------------------------------------------------------- ProducerSplit
    /// The deliverable, stated as the smallest property that matters: an archive
    /// holding both grok producers gets two group labels, so no report can print
    /// one grok number for both.
    #[test]
    fn an_id_space_holding_both_producers_is_printed_as_two() {
        let web = "grok.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
        let local = "grok.mbp-2.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
        let split = ProducerSplit::scan([("grok", web), ("grok", local)]);
        assert!(split.is_shared("grok"));
        assert_eq!(split.label("grok", web), "grok (web capture)");
        assert_eq!(split.label("grok", local), "grok (local harness)");
        assert_ne!(
            split.label("grok", web),
            split.label("grok", local),
            "the two producers must not land in one group"
        );
    }

    /// A single-producer platform is byte-for-byte unchanged: the bare id, no
    /// qualifier, nothing to explain. The split costs a reader nothing where
    /// there is nothing to split.
    #[test]
    fn a_single_producer_platform_keeps_its_bare_id() {
        let web = "grok.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
        let only_web = ProducerSplit::scan([
            ("grok", web),
            ("chatgpt", "chatgpt.aaa"),
            ("chatgpt", "chatgpt.bbb"),
        ]);
        assert!(
            !only_web.is_shared("grok"),
            "one shape is not two producers"
        );
        assert_eq!(only_web.label("grok", web), "grok");
        assert_eq!(only_web.label("chatgpt", "chatgpt.aaa"), "chatgpt");

        let local = "grok.mbp-2.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
        let only_local = ProducerSplit::scan([("grok", local), ("codex", "codex.mbp.aaa")]);
        assert!(!only_local.is_shared("grok"));
        assert_eq!(only_local.label("grok", local), "grok");
        assert_eq!(only_local.label("codex", "codex.mbp.aaa"), "codex");
    }

    /// An id whose producer is not knowable neither joins a producer's group nor
    /// makes an id space look shared, and a row with no harness at all keeps the
    /// caller's own no-prefix name — that bucket is not a producer.
    #[test]
    fn a_prefixless_id_never_creates_a_shared_id_space() {
        let local = "grok.mbp-2.019bf00d-97b6-7eb2-9bf8-eacbacc09765";
        let split = ProducerSplit::scan([("grok", local), ("", ".hidden-session")]);
        assert!(!split.is_shared("grok"), "an unknown shape proves nothing");
        assert_eq!(split.label("", ".hidden-session"), "");
        // The grok local row stays the bare id: one shape seen is no collision.
        assert_eq!(split.label("grok", local), "grok");
        // A name with no `.` keeps its own label rather than vanishing from the
        // report: an unnameable producer is not the same state as no session.
        let dotless = ProducerSplit::scan([("grok", local), ("hidden-session", "hidden-session")]);
        assert_eq!(
            dotless.label("hidden-session", "hidden-session"),
            "hidden-session"
        );
        assert_eq!(dotless.label("grok", local), "grok");
    }

    /// Sharedness is decided per harness, so one collision never spills onto the
    /// platforms in the same archive: an id with a single producer here keeps
    /// its bare name even while `grok` next to it is printed twice.
    #[test]
    fn sharing_is_decided_per_id_not_globally() {
        let split = ProducerSplit::scan([
            ("grok", "grok.019bf00d"),
            ("grok", "grok.mbp.019bf00d"),
            ("gemini", "gemini.mbp.019bf00d"),
            ("chatgpt", "chatgpt.019bf00d"),
        ]);
        assert!(split.is_shared("grok"));
        assert!(!split.is_shared("gemini"), "one shape seen is no collision");
        assert!(!split.is_shared("chatgpt"));
        assert_eq!(split.label("gemini", "gemini.mbp.019bf00d"), "gemini");
        assert_eq!(split.label("chatgpt", "chatgpt.019bf00d"), "chatgpt");
        assert_eq!(split.label("grok", "grok.019bf00d"), "grok (web capture)");
    }

    /// A row whose producer cannot be named keeps its harness in a shared id
    /// space too: a short id (whose machine segment was truncated away) must not
    /// be sorted into a producer column it cannot prove it belongs to.
    #[test]
    fn an_unnamable_row_in_a_shared_id_space_keeps_the_bare_harness() {
        let split = ProducerSplit::scan([("grok", "grok.019bf00d"), ("grok", "grok.mbp.019bf00d")]);
        let short = crate::id::short_session_id("grok.mbp.019bf00d");
        assert_eq!(split.label("grok", &short), "grok");
    }

    // -------------------------------------------------- activity_index_machine
    #[test]
    fn matches_suffix_marker_with_any_stage_prefix() {
        // The prefix is the machine's own absolute stage path — different per
        // machine — so matching must be by trailing components only.
        assert_eq!(
            activity_index_machine(Path::new("/Users/air/stage/meta/air/activity-v1.jsonl")),
            Some("air".to_string())
        );
        assert_eq!(
            activity_index_machine(Path::new("/stage/meta/mbp/activity-v1.jsonl")),
            Some("mbp".to_string())
        );
    }

    #[test]
    fn rejects_non_activity_and_shallower_paths() {
        assert_eq!(
            activity_index_machine(Path::new("/stage/sessions/m/s/000001.jsonl")),
            None
        );
        assert_eq!(
            activity_index_machine(Path::new("/stage/meta/mbp/activity-v2.jsonl")),
            None
        );
        assert_eq!(
            activity_index_machine(Path::new("/stage/activity-v1.jsonl")),
            None
        );
        assert_eq!(
            activity_index_machine(Path::new("/stage/notmeta/mbp/activity-v1.jsonl")),
            None
        );
    }

    // --------------------------------------------------------- to_overview_row
    #[test]
    fn converts_each_time_source_shape() {
        let exact = ActivityRow {
            session_id: "s1".into(),
            machine: "mbp".into(),
            harness: "claude-code".into(),
            first_unix: Some(1),
            last_unix: Some(2),
            line_count: 3,
            time_source: ActivityTimeSource::Exact,
            source_zone: None,
            title: None,
            provenance: None,
            session_provenance: None,
            dimensions: Default::default(),
            account_keys: Vec::new(),
            measured_body: None,
        };
        let o = to_overview_row(&exact);
        assert_eq!(o.session_id, "s1");
        assert_eq!(o.time_source, OverviewTimeSource::Exact);

        let inferred = ActivityRow {
            time_source: ActivityTimeSource::Inferred {
                how: "numeric epoch".into(),
            },
            ..exact.clone()
        };
        assert_eq!(
            to_overview_row(&inferred).time_source,
            OverviewTimeSource::Inferred {
                how: "numeric epoch".into()
            }
        );

        let unknown = ActivityRow {
            time_source: ActivityTimeSource::Unknown {
                why: "no timestamp".into(),
            },
            ..exact
        };
        assert_eq!(
            to_overview_row(&unknown).time_source,
            OverviewTimeSource::Unknown {
                why: "no timestamp".into()
            }
        );
    }

    // -------------------------------------------------- missing_index_machines
    #[test]
    fn lists_snapshot_machines_without_an_index_sorted() {
        let snaps: BTreeSet<String> = ["air", "mbp", "pro"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let idx: BTreeSet<String> = ["mbp"].iter().map(|s| s.to_string()).collect();
        assert_eq!(missing_index_machines(&snaps, &idx), ["air", "pro"]);
    }

    #[test]
    fn all_indexed_means_none_missing() {
        let snaps: BTreeSet<String> = ["air", "mbp"].iter().map(|s| s.to_string()).collect();
        let idx = snaps.clone();
        assert!(missing_index_machines(&snaps, &idx).is_empty());
    }

    // ------------------------------------------------- meta metadata file matching
    #[test]
    fn declaration_machine_matches_meta_suffix() {
        assert_eq!(
            declaration_machine(Path::new(
                "/Users/air/stage/meta/0123456789abcdef0123456789abcdef/machine.json"
            )),
            Some("0123456789abcdef0123456789abcdef".to_string())
        );
        assert_eq!(
            declaration_machine(Path::new("/stage/meta/mac/machine.json")),
            Some("mac".to_string())
        );
        assert_eq!(
            declaration_machine(Path::new("/stage/meta/abc/activity-v1.jsonl")),
            None
        );
        assert_eq!(
            declaration_machine(Path::new("/stage/meta/abc/machine-v2.json")),
            None
        );
        assert_eq!(
            declaration_machine(Path::new("/stage/meta/abc/label-by-w.json")),
            None
        );
    }

    #[test]
    fn label_record_machine_matches_meta_suffix() {
        assert_eq!(
            label_record_machine(Path::new("/stage/meta/abc/label-by-def.json")),
            Some(("abc".to_string(), "def".to_string()))
        );
        assert_eq!(
            label_record_machine(Path::new("/Users/air/stage/meta/xyz/label-by-writer.json")),
            Some(("xyz".to_string(), "writer".to_string()))
        );
        assert_eq!(
            label_record_machine(Path::new("/stage/meta/abc/machine.json")),
            None,
            "machine.json is a declaration, not a label"
        );
        assert_eq!(
            label_record_machine(Path::new("/stage/meta/abc/activity-v1.jsonl")),
            None
        );
        assert_eq!(
            label_record_machine(Path::new("/stage/meta/abc/label-by-w.txt")),
            None
        );
    }

    // ------------------------------------------------------- writer staleness

    /// The three answers must stay three: behind, current, and "could not be
    /// read". An unreadable record is `None` — folding it into `false` would
    /// let a machine sit on a stale index while the check reports it as fine,
    /// and folding it into `true` would rebuild on a guess.
    #[test]
    fn index_writer_staleness_keeps_unknown_apart() {
        // No record at all: every push that could have written it predates
        // writer-version recording (≤0.3.0), so it is behind.
        assert_eq!(index_writer_is_behind(None, false, "0.4.0"), Some(true));
        assert_eq!(
            index_writer_is_behind(Some("0.3.0"), false, "0.4.0"),
            Some(true)
        );
        assert_eq!(
            index_writer_is_behind(Some("0.4.0-rc.1"), false, "0.4.0"),
            Some(true)
        );
        // Same version, and a newer one, are both "not behind": this asks
        // whether the index was written by an *older* CLI, not whether some
        // other machine runs a newer build.
        assert_eq!(
            index_writer_is_behind(Some("0.4.0"), false, "0.4.0"),
            Some(false)
        );
        assert_eq!(
            index_writer_is_behind(Some("0.5.0"), false, "0.4.0"),
            Some(false)
        );
        // Unreadable wins over everything else, even an absent record.
        assert_eq!(index_writer_is_behind(Some("0.1.0"), true, "0.4.0"), None);
        assert_eq!(index_writer_is_behind(None, true, "0.4.0"), None);
    }
}
