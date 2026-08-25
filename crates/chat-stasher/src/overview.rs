//! Render an at-a-glance overview of the archive: a machine × harness
//! matrix, total line count, a dedicated "time unknown" tally, and a
//! width-adaptive character heatmap.
//!
//! This module is deliberately **pure**: no IO of any kind (no file reads, no
//! network), just string aggregation + rendering. That keeps it trivially
//! unit-testable. The input rows are produced by a separate worker (the
//! collector); we only aggregate and render. Field names match the agreed
//! contract and must not be renamed — an equivalent local struct is defined
//! here so this module never imports a not-yet-existing one.
//!
//! Design rules baked in:
//! * Time-unknown sessions are **never** silently merged into a time bucket.
//!   They surface in a dedicated tally and as a distinct `?` marker in the
//!   heatmap, with an explicit legend.
//! * Width is caller-driven (`width: usize`), never a hardcoded 80. The
//!   heatmap picks day vs week buckets from the available width.

use std::collections::{BTreeMap, BTreeSet};

/// Density ramp, low → high. `EMPTY` marks a time bucket with nothing in it
/// and `UNKNOWN` marks a row that carries time-unknown sessions; both are
/// distinct from every density level and are documented in the heatmap legend.
const DENSITY: [char; 9] = ['.', ':', '-', '=', '+', '*', '#', '%', '@'];
const EMPTY: char = ' ';
const UNKNOWN: char = '?';

/// Whether a session's time is attested as exact, inferred, or unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeSource {
    Exact,
    Inferred { how: String },
    Unknown { why: String },
}

/// One session summary line, as handed to us by the collector worker.
#[derive(Debug, Clone)]
pub struct OverviewRow {
    pub session_id: String,
    pub machine: String,
    pub harness: String,
    pub first_unix: Option<i64>,
    pub last_unix: Option<i64>,
    pub line_count: u64,
    pub time_source: TimeSource,
}

/// The vertical axis of the heatmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeatmapAxis {
    Machine,
    Harness,
}

/// Horizontal bucket granularity of the heatmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    Day,
    Week,
}

impl OverviewRow {
    /// A session's time counts as *known* only when the source attests a real
    /// time (Exact or Inferred) and we actually carry one. Anything tagged
    /// Unknown — regardless of stray Option values — is treated as unknown.
    pub fn has_known_time(&self) -> bool {
        matches!(
            self.time_source,
            TimeSource::Exact | TimeSource::Inferred { .. }
        ) && (self.first_unix.is_some() || self.last_unix.is_some())
    }

    /// Anchor used to place the session on the time axis / span: its start
    /// time, falling back to the end time for a known session.
    fn anchor(&self) -> Option<i64> {
        self.first_unix.or(self.last_unix)
    }
}

/// Total lines across every row.
pub fn total_lines(rows: &[OverviewRow]) -> u64 {
    rows.iter().map(|r| r.line_count).sum()
}

/// Number of distinct machines.
pub fn machine_count(rows: &[OverviewRow]) -> usize {
    distinct_labels(rows, |r| &r.machine).len()
}

/// Number of distinct harnesses.
pub fn harness_count(rows: &[OverviewRow]) -> usize {
    distinct_labels(rows, |r| &r.harness).len()
}

/// Distinct, sorted labels extracted from `rows` via `pick`.
fn distinct_labels(rows: &[OverviewRow], pick: impl Fn(&OverviewRow) -> &str) -> Vec<String> {
    let mut set = BTreeSet::new();
    for r in rows {
        set.insert(pick(r).to_string());
    }
    set.into_iter().collect()
}

/// Aggregate everything into a single rendered report.
pub fn render_overview(rows: &[OverviewRow], width: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "机器 {} · harness {} · 会话 {} · 总行数 {}\n",
        machine_count(rows),
        harness_count(rows),
        rows.len(),
        total_lines(rows)
    ));
    out.push('\n');
    out.push_str(&render_matrix(rows, width));
    out.push('\n');
    out.push_str(&render_time_unknown(rows));
    out.push('\n');
    out.push_str(&render_heatmap(rows, width, HeatmapAxis::Machine));
    out
}

/// The machine × harness matrix. Each cell shows `N会话 · L行 · [span]`,
/// where `span` is `最早~最晚` over that cell's known-time sessions, or `?`
/// when the cell has sessions but none with a known time. A trailing
/// `未知` column tallies each machine's time-unknown sessions so they are
/// never folded into a time-based cell.
pub fn render_matrix(rows: &[OverviewRow], width: usize) -> String {
    let machines = distinct_labels(rows, |r| &r.machine);
    let harnesses = distinct_labels(rows, |r| &r.harness);
    if machines.is_empty() {
        return "（无会话）".to_string();
    }

    // Per (machine, harness) aggregation.
    let mut cell_sessions: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut cell_lines: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut cell_first: BTreeMap<(String, String), i64> = BTreeMap::new();
    let mut cell_last: BTreeMap<(String, String), i64> = BTreeMap::new();
    // Per machine unknown tally.
    let mut unknown_by_machine: BTreeMap<String, usize> = BTreeMap::new();

    for r in rows {
        let key = (r.machine.clone(), r.harness.clone());
        *cell_sessions.entry(key.clone()).or_insert(0) += 1;
        *cell_lines.entry(key.clone()).or_insert(0) += r.line_count;
        if r.has_known_time() {
            if let Some(a) = r.anchor() {
                let e = cell_first.entry(key.clone()).or_insert(a);
                if a < *e {
                    *e = a;
                }
            }
            if let Some(b) = r.last_unix.or(r.first_unix) {
                let e = cell_last.entry(key.clone()).or_insert(b);
                if b > *e {
                    *e = b;
                }
            }
        } else {
            *unknown_by_machine.entry(r.machine.clone()).or_insert(0) += 1;
        }
    }

    // Column widths from content (never a hardcoded 80; `width` trims long
    // machine labels below).
    let label_w = machines
        .iter()
        .map(|m| m.len())
        .max()
        .unwrap_or(0)
        .clamp(1, width.saturating_sub(2).max(1));
    let mut col_w: Vec<usize> = harnesses.iter().map(|h| h.len()).collect::<Vec<_>>();
    for m in &machines {
        for (hi, h) in harnesses.iter().enumerate() {
            let n = cell_sessions
                .get(&(m.clone(), h.clone()))
                .copied()
                .unwrap_or(0);
            let text = cell_text(n, &cell_lines, &cell_first, &cell_last, m, h);
            col_w[hi] = col_w[hi].max(text.len());
        }
    }
    // Cap total columns so the matrix never exceeds `width` by truncating
    // each column to a fair share.
    let total = label_w + 1 + col_w.iter().sum::<usize>() + col_w.len() + 1 + 8;
    if total > width && width > 8 {
        let slack = total - width;
        let per = (slack / col_w.len().max(1)) + 1;
        for cw in col_w.iter_mut() {
            *cw = cw.saturating_sub(per).max(1);
        }
    }

    let mut out = String::new();
    out.push_str("machine × harness 矩阵（每格: 会话数 · 行数 · [跨度]）\n");
    // Header row.
    let mut head = format!("{:<label_w$} ", "");
    for (hi, h) in harnesses.iter().enumerate() {
        head.push_str(&pad_right(&truncate(h, col_w[hi]), col_w[hi]));
        head.push(' ');
    }
    head.push_str(&pad_right("未知", 8));
    out.push_str(head.trim_end());
    out.push('\n');

    for m in &machines {
        let mut line = format!("{:<label_w$} ", truncate(m, label_w));
        for (hi, h) in harnesses.iter().enumerate() {
            let n = cell_sessions
                .get(&(m.clone(), h.clone()))
                .copied()
                .unwrap_or(0);
            let text = cell_text(n, &cell_lines, &cell_first, &cell_last, m, h);
            line.push_str(&pad_right(&truncate(&text, col_w[hi]), col_w[hi]));
            line.push(' ');
        }
        let uk = unknown_by_machine.get(m).copied().unwrap_or(0);
        line.push_str(&pad_right(&format!("{uk}"), 8));
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Text for one matrix cell.
fn cell_text(
    n: usize,
    lines: &BTreeMap<(String, String), u64>,
    first: &BTreeMap<(String, String), i64>,
    last: &BTreeMap<(String, String), i64>,
    m: &str,
    h: &str,
) -> String {
    if n == 0 {
        return "-".to_string();
    }
    let key = (m.to_string(), h.to_string());
    let l = lines.get(&key).copied().unwrap_or(0);
    match (first.get(&key), last.get(&key)) {
        (Some(a), Some(b)) => format!("{n}会话·{l}行·[{}~{}]", fmt_date(*a), fmt_date(*b)),
        _ => format!("{n}会话·{l}行·[?]"),
    }
}

/// A dedicated tally for time-unknown sessions — one line per
/// (machine, harness) plus a total. These are never merged into a time cell.
pub fn render_time_unknown(rows: &[OverviewRow]) -> String {
    let mut by_key: BTreeMap<(String, String), (usize, u64, String)> = BTreeMap::new();
    for r in rows {
        if r.has_known_time() {
            continue;
        }
        let why = match &r.time_source {
            TimeSource::Unknown { why } => why.clone(),
            _ => String::new(),
        };
        let e = by_key
            .entry((r.machine.clone(), r.harness.clone()))
            .or_insert((0, 0, why));
        e.0 += 1;
        e.1 += r.line_count;
    }
    if by_key.is_empty() {
        return "时间未知: 0 会话（全部有时间）\n".to_string();
    }

    let mut out = String::new();
    out.push_str("时间未知的会话（不并入任何时间桶）:\n");
    let mut total_s = 0usize;
    let mut total_l = 0u64;
    for ((m, h), (s, l, why)) in &by_key {
        total_s += s;
        total_l += l;
        let reason = if why.is_empty() {
            String::new()
        } else {
            format!("（{why}）")
        };
        out.push_str(&format!("  {m} / {h}: {s} 会话 · {l} 行{reason}\n"));
    }
    out.push_str(&format!("  合计: {total_s} 会话 · {total_l} 行\n"));
    out
}

/// The width-adaptive character heatmap. Vertical axis is `axis`, horizontal
/// axis is time buckets (day or week, chosen to fit `width`). Each row's
/// trailing `?` marks time-unknown sessions; `EMPTY` marks an empty bucket —
/// the legend states both.
pub fn render_heatmap(rows: &[OverviewRow], width: usize, axis: HeatmapAxis) -> String {
    render_heatmap_gran(rows, width, axis, None)
}

/// Same as [`render_heatmap`] but lets a caller force the bucket granularity
/// (used by tests for determinism).
pub fn render_heatmap_gran(
    rows: &[OverviewRow],
    width: usize,
    axis: HeatmapAxis,
    force: Option<Granularity>,
) -> String {
    let labels = match axis {
        HeatmapAxis::Machine => distinct_labels(rows, |r| &r.machine),
        HeatmapAxis::Harness => distinct_labels(rows, |r| &r.harness),
    };
    if labels.is_empty() {
        return "（无会话）\n".to_string();
    }

    let axis_name = match axis {
        HeatmapAxis::Machine => "机器",
        HeatmapAxis::Harness => "harness",
    };

    // Per-axis known-time bucket counts and unknown counts.
    let mut counts: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut unknown: BTreeMap<String, usize> = BTreeMap::new();
    // Horizontal axis covers the full activity span: earliest start to
    // latest end across all known-time sessions.
    let mut lo: Option<i64> = None;
    let mut hi: Option<i64> = None;
    for r in rows {
        let l = match axis {
            HeatmapAxis::Machine => &r.machine,
            HeatmapAxis::Harness => &r.harness,
        };
        if r.has_known_time() {
            if let Some(f) = r.first_unix {
                lo = Some(lo.map_or(f, |m: i64| m.min(f)));
            }
            if let Some(e) = r.last_unix {
                hi = Some(hi.map_or(e, |m: i64| m.max(e)));
            }
        } else {
            *unknown.entry(l.clone()).or_insert(0) += 1;
        }
    }

    let label_w = labels
        .iter()
        .map(|l| l.len())
        .max()
        .unwrap_or(0)
        .clamp(1, 20);

    // No known time at all → no horizontal axis; still show the unknown rows.
    let (Some(lo), Some(hi)) = (lo, hi) else {
        let mut out = String::new();
        out.push_str(&format!("热力图 · 纵轴={axis_name} · 无已知时间\n"));
        for l in &labels {
            let uk = unknown.get(l).copied().unwrap_or(0);
            out.push_str(&format!(
                "{:<label_w$} {} {}\n",
                truncate(l, label_w),
                "",
                if uk > 0 { UNKNOWN } else { EMPTY }
            ));
        }
        out.push_str(&format!(
            "图例: '{}'=空桶 '{}'=时间未知（详见上方「时间未知」表）\n",
            EMPTY, UNKNOWN
        ));
        return out;
    };

    let available = width.saturating_sub(label_w + 4).max(1) as i64;
    let day_buckets = (hi - lo) / 86_400 + 1;
    let gran = force.unwrap_or(if day_buckets <= available {
        Granularity::Day
    } else {
        Granularity::Week
    });

    // Ordered bucket labels from lo..=hi.
    let mut bucket_labels: Vec<String> = Vec::new();
    match gran {
        Granularity::Day => {
            let mut d = days_from_epoch(lo);
            let end = days_from_epoch(hi);
            while d <= end {
                bucket_labels.push(fmt_ymd(civil_from_days(d)));
                d += 1;
            }
        }
        Granularity::Week => {
            let mut monday = monday_of(lo);
            let last_monday = monday_of(hi);
            while monday <= last_monday {
                bucket_labels.push(fmt_ymd(civil_from_days(monday)));
                monday += 7;
            }
        }
    }

    // Fill counts into buckets.
    let mut max_count = 0usize;
    for r in rows {
        if !r.has_known_time() {
            continue;
        }
        let Some(a) = r.anchor() else { continue };
        let l = match axis {
            HeatmapAxis::Machine => &r.machine,
            HeatmapAxis::Harness => &r.harness,
        };
        let bucket = match gran {
            Granularity::Day => fmt_ymd(civil_from_days(days_from_epoch(a))),
            Granularity::Week => fmt_ymd(civil_from_days(monday_of(a))),
        };
        let e = counts
            .entry(l.clone())
            .or_default()
            .entry(bucket)
            .or_insert(0);
        *e += 1;
        if *e > max_count {
            max_count = *e;
        }
    }

    let n_buckets = bucket_labels.len();
    let gran_name = match gran {
        Granularity::Day => "按天",
        Granularity::Week => "按周",
    };
    let mut out = String::new();
    let x_lo = bucket_labels.first().cloned().unwrap_or_default();
    let x_hi = bucket_labels.last().cloned().unwrap_or_default();
    out.push_str(&format!(
        "热力图 · 纵轴={axis_name} · {gran_name} · x轴 {}..{} ({}桶)\n",
        x_lo, x_hi, n_buckets
    ));

    for l in &labels {
        let row_counts = counts.get(l);
        let mut chars = String::with_capacity(n_buckets);
        for b in &bucket_labels {
            let c = row_counts.and_then(|m| m.get(b)).copied().unwrap_or(0);
            chars.push(density_char(c, max_count));
        }
        let uk = unknown.get(l).copied().unwrap_or(0);
        let uk_char = if uk > 0 { UNKNOWN } else { EMPTY };
        out.push_str(&format!(
            "{:<label_w$}{:<n_buckets$} {}\n",
            truncate(l, label_w),
            chars,
            uk_char
        ));
    }
    out.push_str(&format!(
        "图例: '{}'=空桶 · '{}'=时间未知 · 密度 {}（最低）..{}（最高）\n",
        EMPTY,
        UNKNOWN,
        DENSITY[0],
        DENSITY[DENSITY.len() - 1]
    ));
    out
}

/// Map a per-bucket session count onto a density character. Zero → empty;
/// smallest non-zero count → lowest density, `max_count` → highest. When there
/// is only one non-empty bucket (`max_count <= 1`) it renders at the lowest
/// level so that a single session still reads as lighter than two.
fn density_char(count: usize, max_count: usize) -> char {
    if count == 0 {
        return EMPTY;
    }
    if max_count <= 1 {
        return DENSITY[0];
    }
    let idx = (count - 1) * (DENSITY.len() - 1) / (max_count - 1);
    DENSITY[idx]
}

// ---- date math (pure, no external deps) ----

/// Truncate `s` to `max` chars, appending an ellipsis when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut = max.saturating_sub(1);
    let mut out: String = s.chars().take(cut).collect();
    out.push('…');
    out
}

/// Left-justify `s` to `width`.
fn pad_right(s: &str, width: usize) -> String {
    let mut out = s.to_string();
    while out.chars().count() < width {
        out.push(' ');
    }
    out
}

fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    let r = a % b;
    if r != 0 && (r < 0) != (b < 0) {
        q - 1
    } else {
        q
    }
}

fn floor_mod(a: i64, b: i64) -> i64 {
    a - floor_div(a, b) * b
}

/// Whole days since the Unix epoch for a unix timestamp.
fn days_from_epoch(unix: i64) -> i64 {
    floor_div(unix, 86_400)
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = floor_div(z, 146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// Days since 1970-01-01 of the Monday of the ISO week containing `days`.
fn monday_of(unix: i64) -> i64 {
    let days = days_from_epoch(unix);
    days - floor_mod(days + 3, 7)
}

/// `YYYY-MM-DD` for a unix timestamp.
fn fmt_date(unix: i64) -> String {
    fmt_ymd(civil_from_days(days_from_epoch(unix)))
}

fn fmt_ymd((y, m, d): (i64, u32, u32)) -> String {
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        id: &str,
        machine: &str,
        harness: &str,
        first: Option<i64>,
        last: Option<i64>,
        lines: u64,
        ts: TimeSource,
    ) -> OverviewRow {
        OverviewRow {
            session_id: id.to_string(),
            machine: machine.to_string(),
            harness: harness.to_string(),
            first_unix: first,
            last_unix: last,
            line_count: lines,
            time_source: ts,
        }
    }

    // Fixed unix timestamps (UTC) for test convenience.
    const D1: i64 = 1_777_651_200; // 2026-05-01T16:00:00Z → civil date 2026-05-01
    const D1P1: i64 = 1_777_737_600; // D1 + 1 day → 2026-05-02
    const D1P7: i64 = 1_777_651_200 + 7 * 86_400; // D1 + 7 days → 2026-05-08

    #[test]
    fn empty_input_renders_safely() {
        let rows: Vec<OverviewRow> = Vec::new();
        assert_eq!(total_lines(&rows), 0);
        assert_eq!(machine_count(&rows), 0);
        assert_eq!(harness_count(&rows), 0);
        let report = render_overview(&rows, 80);
        assert!(report.contains("总行数 0"));
        assert!(report.contains("（无会话）"));
    }

    #[test]
    fn single_row_renders() {
        let rows = vec![row(
            "s1",
            "air",
            "claude-code",
            Some(D1),
            Some(D1),
            42,
            TimeSource::Exact,
        )];
        let report = render_overview(&rows, 60);
        assert!(report.contains("总行数 42"));
        // matrix cell
        assert!(report.contains("1会话·42行"));
        assert!(report.contains("[2026-05-01~2026-05-01]"));
        // unknown tally empty
        assert!(report.contains("全部有时间"));
        // heatmap has one bucket and the row
        assert!(report.contains("air"));
        assert!(report.contains("(1桶)"));
    }

    #[test]
    fn all_unknown_never_merged_into_a_bucket() {
        let rows = vec![
            row(
                "u1",
                "air",
                "claude-code",
                None,
                None,
                10,
                TimeSource::Unknown {
                    why: "mtime absent".into(),
                },
            ),
            row(
                "u2",
                "pro",
                "codex",
                Some(D1),
                Some(D1),
                5,
                TimeSource::Unknown {
                    why: "corrupt header".into(),
                },
            ),
        ];
        let report = render_overview(&rows, 60);
        // dedicated tally lists both
        assert!(report.contains("air / claude-code: 1 会话 · 10 行"));
        assert!(report.contains("pro / codex: 1 会话 · 5 行"));
        assert!(report.contains("合计: 2 会话"));
        // heatmap shows "无已知时间" and the '?' markers, no time axis buckets
        let hm = render_heatmap(&rows, 60, HeatmapAxis::Machine);
        assert!(hm.contains("无已知时间"));
        assert!(hm.contains("'?'=时间未知"));
        assert!(!hm.contains("按天"));
        assert!(!hm.contains("x轴"));
    }

    #[test]
    fn cross_year_weeks_bucket_by_monday() {
        // 2025-12-29 (Mon) and 2026-01-05 (Mon) are two distinct weeks.
        let mon_dec29 = days_from_epoch_for_test(2025, 12, 29);
        let mon_jan05 = mon_dec29 + 7 * 86_400;
        let rows = vec![
            row(
                "y1",
                "air",
                "claude-code",
                Some(mon_dec29),
                Some(mon_dec29),
                1,
                TimeSource::Exact,
            ),
            row(
                "y2",
                "air",
                "claude-code",
                Some(mon_jan05),
                Some(mon_jan05),
                1,
                TimeSource::Exact,
            ),
        ];
        let hm = render_heatmap_gran(&rows, 60, HeatmapAxis::Machine, Some(Granularity::Week));
        assert!(hm.contains("x轴 2025-12-29..2026-01-05 (2桶)"));
        assert!(hm.contains("2025-12-29"));
        assert!(hm.contains("2026-01-05"));
    }

    #[test]
    fn matrix_splits_machines_and_harnesses_with_spans() {
        let rows = vec![
            row(
                "a1",
                "air",
                "claude-code",
                Some(D1),
                Some(D1P1),
                10,
                TimeSource::Exact,
            ),
            row(
                "a2",
                "air",
                "codex",
                Some(D1),
                Some(D1),
                20,
                TimeSource::Exact,
            ),
            row(
                "a3",
                "pro",
                "claude-code",
                Some(D1P1),
                Some(D1P7),
                30,
                TimeSource::Exact,
            ),
        ];
        let m = render_matrix(&rows, 80);
        assert!(m.contains("air"));
        assert!(m.contains("pro"));
        assert!(m.contains("claude-code"));
        assert!(m.contains("codex"));
        assert!(m.contains("[2026-05-01~2026-05-02]"));
        assert!(m.contains("[2026-05-02~2026-05-08]"));
    }

    #[test]
    fn density_ramp_is_monotonic_and_empty_is_distinct() {
        // One row, one bucket: two sessions → higher density than one session.
        let r1 = vec![row(
            "x1",
            "air",
            "h",
            Some(D1),
            Some(D1),
            1,
            TimeSource::Exact,
        )];
        let r2 = vec![
            row("x1", "air", "h", Some(D1), Some(D1), 1, TimeSource::Exact),
            row("x2", "air", "h", Some(D1), Some(D1), 1, TimeSource::Exact),
        ];
        let hm1 = render_heatmap_gran(&r1, 60, HeatmapAxis::Machine, Some(Granularity::Day));
        let hm2 = render_heatmap_gran(&r2, 60, HeatmapAxis::Machine, Some(Granularity::Day));
        // Both render one row of one bucket; 1 vs 2 sessions must differ.
        let c1 = row_density_char(&hm1, "air");
        let c2 = row_density_char(&hm2, "air");
        assert_ne!(c1, c2, "1 会话应与 2 会话密度不同");
        // Both carry the empty-bucket / unknown legend.
        assert!(hm1.contains("' '=空桶"));
        assert!(hm1.contains("'?'=时间未知"));
    }

    /// First density character (just after the left label) of a heatmap row.
    fn row_density_char(hm: &str, label: &str) -> char {
        let line = hm.lines().find(|l| l.starts_with(label)).unwrap();
        line[label.len()..]
            .chars()
            .find(|c| *c != ' ')
            .unwrap_or(EMPTY)
    }

    #[test]
    fn width_drives_granularity_choice() {
        // Span many days: narrow width → week, wide width → day.
        let start = D1;
        let end = D1 + 60 * 86_400;
        let rows = vec![row(
            "w1",
            "air",
            "h",
            Some(start),
            Some(end),
            1,
            TimeSource::Exact,
        )];
        let narrow = render_heatmap(&rows, 12, HeatmapAxis::Machine);
        let wide = render_heatmap(&rows, 200, HeatmapAxis::Machine);
        assert!(narrow.contains("按周"));
        assert!(wide.contains("按天"));
    }

    #[test]
    fn density_maps_zero_to_empty_and_extremes_to_ramp_ends() {
        assert_eq!(density_char(0, 5), EMPTY);
        assert_eq!(density_char(1, 5), DENSITY[0]);
        assert_eq!(density_char(5, 5), DENSITY[DENSITY.len() - 1]);
        // Single non-empty bucket renders at the lowest level.
        assert_eq!(density_char(1, 1), DENSITY[0]);
    }

    #[test]
    fn date_math_round_trips() {
        // 1970-01-01
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // leap day
        assert_eq!(civil_from_days(days_for_test(2024, 2, 29)), (2024, 2, 29));
        // round trip through fmt (fmt_date takes seconds, not days)
        assert_eq!(fmt_date(days_for_test(2025, 12, 31) * 86_400), "2025-12-31");
    }

    // ---- test-only calendar helpers ----

    fn days_for_test(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = floor_div(y, 400);
        let yoe = y - era * 400;
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    fn days_from_epoch_for_test(y: i64, m: i64, d: i64) -> i64 {
        days_for_test(y, m, d) * 86_400
    }
}
