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
        };
        let o = to_overview_row(&exact);
        assert_eq!(o.session_id, "s1");
        assert_eq!(o.time_source, OverviewTimeSource::Exact);

        let inferred = ActivityRow {
            time_source: ActivityTimeSource::Inferred {
                how: "数值 epoch".into(),
            },
            ..exact.clone()
        };
        assert_eq!(
            to_overview_row(&inferred).time_source,
            OverviewTimeSource::Inferred {
                how: "数值 epoch".into()
            }
        );

        let unknown = ActivityRow {
            time_source: ActivityTimeSource::Unknown {
                why: "没有时间戳".into(),
            },
            ..exact
        };
        assert_eq!(
            to_overview_row(&unknown).time_source,
            OverviewTimeSource::Unknown {
                why: "没有时间戳".into()
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
}
