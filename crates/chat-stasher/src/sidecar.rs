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

use std::path::Path;

use crate::activity::{ActivityRow, TimeSource as ActivityTimeSource};
use crate::overview::{OverviewRow, TimeSource as OverviewTimeSource};

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
