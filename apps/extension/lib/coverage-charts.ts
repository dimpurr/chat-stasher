/**
 * W149 · ADR-032 — the coverage page's chart geometry: numbers in, numbers out.
 *
 * The page's charts are inline SVG (the extension CSP allows nothing else and
 * no chart library is worth a new dependency for three small drawings), and
 * this module is everything of them a test can pin: the coordinates, computed
 * from the model's own numbers, with no DOM, no clock and no wording in it.
 *
 * 🔴 The honesty rules these helpers enforce, because a wrong number drawn as a
 *    shape is the same lie as a wrong number in a sentence:
 *   · `barGeometry` drops zero-count segments and returns `[]` when the total is
 *     zero — an empty bar drawn next to real counts would read as "measured
 *     nothing" in a row where the honest state may be "no count at all"
 *     (unknown ≠ empty, CLAUDE.md invariant 1); the caller decides which
 *     sentence belongs around that case, never the shape alone.
 *   · `ringDash` returns `null` when the caller passes `null`: a percentage is
 *     only drawable when `computeProgress` produced one, and a bare track drawn
 *     next to a number that was never allowed to exist would be the `%` the
 *     model refused to print (lib/coverage.ts carries that rule).
 *   · `monthsGeometry` keeps the time-unknown bucket as its own column and
 *     returns `null` when there is nothing to draw — no empty axis that reads
 *     as an empty history, which is exactly the claim `coverage.months.none`
 *     exists to deny.
 *   · every aggregate it reports is finite. Sanitizing each count separately
 *     does not make their *sum* finite, and an infinite total scales its
 *     column's heights to `NaN` or to zero, which is a malformed shape rather
 *     than a large one. A sum that leaves the number range saturates at
 *     `Number.MAX_VALUE` and the column is rescaled so the two halves still
 *     share the track in proportion.
 */

/** The four things a count bar can be made of. `remainder` is the part a known total holds beyond what has been listed so far. */
export type BarTone = 'archived' | 'owed' | 'failed' | 'remainder';

export interface BarSegmentGeometry {
  tone: BarTone;
  x: number;
  w: number;
}

export interface BarCounts {
  archived: number;
  owed: number;
  failed: number;
  /** How many more the platform's own total claims exist beyond the rows listed so far. `0` when no total or the list reached it. */
  remainder: number;
}

/**
 * Lay a count bar out on `width`. Exact proportions, fractional widths allowed
 * (the paint code passes them to SVG/CSS as-is), zero counts dropped so no
 * empty sliver reads as a measurement, and `[]` for a total of zero.
 */
export function barGeometry(counts: BarCounts, width: number): BarSegmentGeometry[] {
  if (!Number.isFinite(width) || width <= 0) return [];
  const segments: Array<{ tone: BarTone; count: number }> = ([
    { tone: 'archived', count: finiteCount(counts.archived) },
    { tone: 'owed', count: finiteCount(counts.owed) },
    { tone: 'failed', count: finiteCount(counts.failed) },
    { tone: 'remainder', count: finiteCount(counts.remainder) },
  ] as Array<{ tone: BarTone; count: number }>).filter((segment) => segment.count > 0);
  const largest = Math.max(0, ...segments.map((segment) => segment.count));
  if (largest === 0) return [];
  const scaledTotal = segments.reduce((n, segment) => n + segment.count / largest, 0);
  // Scale, then clamp: rounding up could make the bar exceed its track, and a
  // bar wider than 100% is the visual form of "more than exists".
  const series = segments.map((segment) => ({ tone: segment.tone, w: ((segment.count / largest) / scaledTotal) * width }));
  let x = 0;
  const out: BarSegmentGeometry[] = [];
  for (const segment of series) {
    const clamped = Math.min(segment.w, width - x);
    if (clamped > 0) out.push({ tone: segment.tone, x, w: clamped });
    x += clamped;
  }
  return out;
}

/**
 * The stroke-dasharray of a progress ring, or `null` when no percentage exists.
 *
 * `percent` is the model's own `CoverageRow.percent` — already validated by
 * `computeProgress` — and is clamped here anyway because a geometry helper may
 * not trust arithmetic it did not perform.
 */
export function ringDash(percent: number | null, radius: number): { dash: number; gap: number } | null {
  if (percent === null || !Number.isFinite(percent) || !Number.isFinite(radius) || radius <= 0) return null;
  const clamped = Math.min(100, Math.max(0, percent));
  const circumference = 2 * Math.PI * radius;
  if (!Number.isFinite(circumference)) return null;
  const dash = (clamped / 100) * circumference;
  return { dash, gap: circumference - dash };
}

export type MonthColumnKind = 'month' | 'unknown';

export interface MonthColumnGeometry {
  kind: MonthColumnKind;
  /** `YYYY-MM` for months, and the empty string for the unknown-time column, which has no month by definition. */
  key: string;
  x: number;
  w: number;
  /** Bar heights, bottom-stacked. For the unknown column everything sits in `hUnknown`. */
  hArchived: number;
  hPending: number;
  hUnknown: number;
  /** The unrounded total, for labels and titles. Saturated at `Number.MAX_VALUE` when the two counts sum past the number range. */
  total: number;
}

export interface MonthsGeometry {
  columns: MonthColumnGeometry[];
  /** The tallest column total, unchanged by rounding, for callers that print a scale. */
  max: number;
  /** 1 = label every column; 2 = every other one, and so on. Chosen from the count so labels never collide. */
  stride: number;
}

export interface MonthsOptions {
  width: number;
  /** The bar area height; an axis row for labels is the caller's own concern below it. */
  height: number;
  /** Gap between columns, in the same units as width. */
  gap: number;
}

/** A gross-value helper callers use to decide whether the unknown column belongs on the chart at all. Saturating, like every total here. */
export function unknownTimeTotal(unknown: { archived: number; pending: number }): number {
  return finiteTotal(finiteCount(unknown.archived), finiteCount(unknown.pending));
}

/** Runtime storage and caller data can violate the TypeScript number contract. */
function finiteCount(count: number): number {
  return Number.isFinite(count) ? Math.max(0, count) : 0;
}

/**
 * Saturating sum of two sanitized counts. Each input is finite, but their sum
 * can exceed `Number.MAX_VALUE` and round to `Infinity`, and an infinite total
 * would scale every height in its column to `NaN` or zero. The true value is
 * unrepresentable at that point, so the total clamps to the largest finite
 * number instead of leaving the geometry.
 */
function finiteTotal(a: number, b: number): number {
  const total = a + b;
  return Number.isFinite(total) ? total : Number.MAX_VALUE;
}

/**
 * Lay the monthly distribution out as columns, bottom-stacked: stored solid,
 * owed hatched, and the time-unknown bucket as its own final column so it is
 * visible *and* visibly not a month (ADR-032 §6: the unknown part is never
 * spread across the months). Returns `null` when there is nothing to draw at
 * all, so the caller prints its own "no distribution yet" sentence instead.
 */
export function monthsGeometry(
  months: ReadonlyArray<{ month: string; archived: number; pending: number }>,
  unknownTime: { archived: number; pending: number },
  options: MonthsOptions,
): MonthsGeometry | null {
  const rows = months
    .filter((row) => row.month.length > 0)
    .map((row) => {
      const archived = finiteCount(row.archived);
      const pending = finiteCount(row.pending);
      return { month: row.month, archived, pending, total: finiteTotal(archived, pending) };
    })
    .filter((row) => row.total > 0);
  const unknown = unknownTimeTotal(unknownTime);
  if (rows.length === 0 && unknown === 0) return null;
  if (!Number.isFinite(options.width) || options.width <= 0 || !Number.isFinite(options.height) || options.height <= 0) return null;

  const columns = rows.length + (unknown > 0 ? 1 : 0);
  // An unusable gap is zero; a larger valid gap is capped so the columns stay
  // within the track even when the requested gaps alone would overrun it.
  const requestedGap = Number.isFinite(options.gap) ? Math.max(0, options.gap) : 0;
  const gap = columns > 1 ? Math.min(requestedGap, options.width / (columns - 1)) : 0;
  const usableWidth = Math.max(0, options.width - gap * (columns - 1));
  const columnWidth = usableWidth / columns;
  const max = Math.max(
    1,
    ...rows.map((row) => row.total),
    unknown,
  );

  const out: MonthColumnGeometry[] = [];
  let x = 0;
  for (const row of rows) {
    const hArchived = (row.archived / max) * options.height;
    const hPending = (row.pending / max) * options.height;
    // The saturated total hides a row whose real sum left the number range: max
    // stays finite while both halves still measure a full track each. Rescaling
    // the pair keeps the stack inside the track and the halves in proportion.
    const stacked = hArchived + hPending;
    const fit = stacked > options.height ? options.height / stacked : 1;
    out.push({
      kind: 'month',
      key: row.month,
      x,
      w: columnWidth,
      hArchived: hArchived * fit,
      hPending: hPending * fit,
      hUnknown: 0,
      total: row.total,
    });
    x += columnWidth + gap;
  }
  if (unknown > 0) {
    out.push({
      kind: 'unknown',
      key: '',
      x,
      w: columnWidth,
      hArchived: 0,
      hPending: 0,
      hUnknown: (unknown / max) * options.height,
      total: unknown,
    });
  }
  return { columns: out, max, stride: Math.max(1, Math.ceil(columns / 6)) };
}

/**
 * The label of a month column key (`YYYY-MM`), locale-neutral on purpose:
 * month *names* are words that would need the catalog, and a chart of counts
 * needs no words. `26/03` style keeps the label short at every width.
 */
export function monthLabel(month: string): string {
  if (month.length !== 7 || month[4] !== '-') return month;
  return `${month.slice(2, 4)}/${month.slice(5, 7)}`;
}
