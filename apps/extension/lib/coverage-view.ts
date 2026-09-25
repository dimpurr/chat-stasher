/**
 * ADR-032 · **The coverage page's render layer: what is shown, as data, with no DOM in it.**
 *
 * The page's own entrypoint (`entrypoints/coverage/main.ts`) cannot run under node, and every rule that
 * this page must not break — a lower bound never drawn as a total, a percentage only with a denominator,
 * an ETA always labelled an estimate, a time-unknown bucket that is never spread over months — has to be
 * assertable. So *what is displayed* is built here, as a structured per-card view, and the entrypoint
 * walks it into elements. Same split, and the same reason, as `lib/popup-view.ts`.
 *
 * W149 replaced the flat block array with this card view: the page is now a grid of one card per
 * (platform × scope) under a single overview header. The philosophy did not change — the view is still
 * data, painting is still mechanical — only the shape got richer, because a card layout has to carry
 * chip words, bar counts and month columns where the text page carried sentences.
 *
 * 🔴 **Every sentence goes through `t` at render time.** A report may be built once and painted in either
 *    language, so no model holds a language — which is also why `lib/coverage.ts` carries catalog keys and
 *    numbers rather than strings.
 *
 * 🔴 **Nothing here decides anything.** The six questions ADR-032's "information that must be shown" asks
 *    are answered in `lib/coverage.ts`; this file chooses the order and the wording. In particular:
 *   · `percent` is carried, not computed — a card paints a ring exactly when `computeProgress` produced
 *     a percentage and never otherwise, and this file only forwards the model's answer;
 *   · `bar.remainder` exists only for a response total the list has not reached yet. A contradicted total
 *     is not a claim about the account (progress.ts's own rule) and is never turned into a bar segment;
 *   · the `≥` lives in the sentences the model already resolves (`enumNote`), and this file does not
 *     re-derive it.
 */

import { t } from './i18n';
import {
  countsNote,
  enumNote,
  failureNote,
  speedNote,
  stateNote,
  type CoverageMonth,
  type CoverageReport,
  type CoverageRow,
} from './coverage';
import { SPEED_PLANS, SPEED_PRESET_ORDER, type SpeedPreset } from './backfill/speed';
import { describeTickReason } from './popup-view';

// ---------------------------------------------------------------------------
// The status chip
// ---------------------------------------------------------------------------

/** Closed set of chip tones. Each one is paired with a colour token that also passes text contrast. */
export type ChipTone = 'ok' | 'run' | 'wait' | 'bad' | 'muted';

/** A chip: always a word plus a tone. There is no colour-only state anywhere on the page. */
export interface StatusChip {
  word: string;
  tone: ChipTone;
}

/**
 * The chip word for one state, and nothing else. The long "what this means" sentence stays where the
 * model resolves it (`stateNote`); the chip is the at-a-glance half, and it never invents a state the
 * model did not report.
 */
export function chipOf(row: CoverageRow): StatusChip {
  switch (row.state) {
    case 'in-progress':
      return { word: t('coverage.chip.running'), tone: 'run' };
    case 'waiting':
      return { word: t('coverage.chip.waiting'), tone: 'wait' };
    case 'capped':
      return { word: t('coverage.chip.cappedToday'), tone: 'wait' };
    case 'done':
      return { word: t('coverage.chip.done'), tone: 'ok' };
    case 'halted':
    case 'unregistered':
      return { word: t('coverage.chip.stopped'), tone: 'bad' };
    case 'off':
      return { word: t('coverage.chip.off'), tone: 'muted' };
    case 'host-paused':
      return { word: t('coverage.chip.hostDown'), tone: 'muted' };
  }
}

// ---------------------------------------------------------------------------
// Identity: monogram tile
// ---------------------------------------------------------------------------

/**
 * The hue keys the stylesheet knows. Brand-neutral, first-letter monograms only — no logos, per the
 * ticket; an unknown platform id falls back to the same neutral tile rather than to nothing, because
 * "not one of the big platforms" is not a reason to leave a card unidentified.
 */
const HUE_KEYS = new Set(['chatgpt', 'claude', 'deepseek', 'gemini', 'grok', 'kimi', 'perplexity']);

/** The tile's hue key, or `default` for any platform this build does not know by name. */
export function hueKeyOf(platform: string): string {
  return HUE_KEYS.has(platform) ? platform : 'default';
}

/** The monogram letter for the tile. Uppercased here once, so the paint layer cannot re-derive it. */
export function monogramOf(platform: string): string {
  return (platform[0] ?? '?').toUpperCase();
}

// ---------------------------------------------------------------------------
// Dismissible alerts
// ---------------------------------------------------------------------------

/**
 * One dismissible callout. `id` is stable for the page session so a dismissed alert stays dismissed
 * across the repaint a preset change causes — and it is never persisted, because this page is
 * read-only apart from the preset (the dismiss is a visual gesture, not a record).
 */
export interface CoverageAlert {
  id: string;
  tone: 'warn' | 'bad';
  text: string;
}

// ---------------------------------------------------------------------------
// Per-card pieces
// ---------------------------------------------------------------------------

/** The legend under the composition bar: a name, a count, and which tone the bar drew it in. */
export interface BarLegendEntry {
  label: string;
  count: number;
  /** `ok` fills solid, `hatch` draws the hatch, `bad` fills solid, `remainder` is the bare track. */
  tone: 'ok' | 'hatch' | 'bad' | 'remainder';
}

/**
 * Today's quota, as the numbers and one sentence. The sentence is resolved here so a repaint after a
 * preset change cannot re-word it, and `counterStale` is carried because a stale counter must not be
 * drawn as if it measured today.
 */
export interface QuotaView {
  bodiesToday: number;
  dayCap: number | null;
  counterStale: boolean;
  line: string;
}

/** One label/value pair in the details disclosure. Empty label = a continuation line. */
export interface DetailRow {
  label: string;
  value: string;
}

/** One column chart's data bag: the model's own months, plus the words to label them with. */
export interface MonthsView {
  months: CoverageMonth[];
  unknownTime: { archived: number; pending: number };
  /** Sentence for the chart's accessible title. `null` when there is nothing to draw (the caller prints `noneNote`). */
  title: string | null;
  archivedLabel: string;
  pendingLabel: string;
  unknownLabel: string;
  unknownNote: string | null;
  noneNote: string | null;
}

/** One card: one (platform × scope), everything the page shows about it. */
export interface CoverageCardView {
  key: string;
  platform: string;
  scope: string;
  monogram: string;
  hueKey: string;
  chip: StatusChip;
  /** The composition bar. Zero segments are the paint layer's problem (see `coverage-charts.barGeometry`). */
  bar: { archived: number; owed: number; failed: number; remainder: number };
  legend: BarLegendEntry[];
  /** From `computeProgress` via the model. A ring exists on the card **only** when this is non-null. */
  percent: number | null;
  /** Screen-reader/title sentence around the number the ring draws. */
  percentTitle: string | null;
  /** Why there is no percentage, when there is none. Never `null` alongside a non-null `percent`. */
  percentNote: string | null;
  /** The list sentence (`enumNote`), `≥` included — resolved by the model, never re-derived here. */
  listed: string;
  /** The short, still-explicitly-labelled estimate line. The full speed sentence is in `details`. */
  eta: string;
  quota: QuotaView;
  /** Dismissible bad callouts: a held stop (with its action sentence), and conversations not stored. */
  alerts: CoverageAlert[];
  detailsSummary: string;
  /** Every remaining ADR-032 fact, behind the disclosure: all of it, nothing conditional on interest. */
  detailRows: DetailRow[];
  months: MonthsView;
}

// ---------------------------------------------------------------------------
// Overview header
// ---------------------------------------------------------------------------

/** The three numbers the overview stat row shows. Sums over the rows the report itself holds. */
export interface CoverageStatsView {
  stored: number;
  owed: number;
  failed: number;
}

/** One health line: a tone for the row it sits in, and a sentence that says what (if anything) is wrong. */
export interface CoverageHealth {
  tone: 'ok' | 'wait' | 'bad';
  text: string;
}

// ---------------------------------------------------------------------------
// Speed control
// ---------------------------------------------------------------------------

/** The segmented control: three presets, the current one, and the risk note beside it when it applies. */
export interface CoverageSpeedView {
  current: SpeedPreset;
  options: Array<{ preset: SpeedPreset; label: string; what: string }>;
  /** 🔴 A block beside the control, not a tooltip: the warning appears with the preset that carries it. */
  riskNote: string | null;
}

// ---------------------------------------------------------------------------
// Whole page
// ---------------------------------------------------------------------------

/** Everything the page paints, in paint order. */
export interface CoveragePageView {
  title: string;
  /** One line: as of when, and that these are this browser's own records. The long note is `aboutNote`. */
  subtitle: string;
  aboutSummary: string;
  aboutNote: string;
  stats: CoverageStatsView | null;
  health: CoverageHealth | null;
  /** Global alerts: the switch being off, the host being unreachable. Both affect every card. */
  alerts: CoverageAlert[];
  /** The last wake, demoted to a meta row. `null` when no record exists. */
  tickNote: string | null;
  cards: CoverageCardView[];
  /** The empty-state sentence, or `null` when there are cards. */
  empty: string | null;
  speed: CoverageSpeedView;
}

// ---------------------------------------------------------------------------
// Speed wording (unchanged in meaning, now the control's own data rather than a block list)
// ---------------------------------------------------------------------------

/** The preset's own name, as the segmented control shows it. */
export function presetLabel(preset: SpeedPreset): string {
  switch (preset) {
    case 'gentle':
      return t('coverage.preset.gentle');
    case 'standard':
      return t('coverage.preset.standard');
    case 'faster':
      return t('coverage.preset.faster');
  }
}

/**
 * What a preset means, in the numbers it selects.
 * 🔴 Taken from the plan, never retyped: a label that could drift from `SPEED_PLANS` would be a
 *    settings row lying about the setting.
 */
export function presetWhat(preset: SpeedPreset): string {
  const plan = SPEED_PLANS[preset];
  return t('coverage.preset.what', {
    cap: plan.pace.detail.maxPerDay ?? 0,
    tick: plan.tickDetails,
  });
}

/**
 * The speed control as one structured value.
 *
 * 🔴 The risk note appears with the preset that carries it (ADR-032 §3). A warning the user has to
 *    hover to find is a warning about a setting they have already changed, so it is a sibling of the
 *    control, and this function — not the paint layer — decides when it exists.
 */
export function speedView(report: CoverageReport): CoverageSpeedView {
  return {
    current: report.preset,
    options: SPEED_PRESET_ORDER.map((preset) => ({
      preset,
      label: presetLabel(preset),
      what: presetWhat(preset),
    })),
    riskNote: SPEED_PLANS[report.preset].carriesRisk ? t('coverage.preset.risky') : null,
  };
}

// ---------------------------------------------------------------------------
// Sentence helpers
// ---------------------------------------------------------------------------

/** How the totals line reads, from `totalSource` — three different facts, three different sentences. */
function totalFact(row: CoverageRow): string {
  switch (row.totalSource) {
    case 'response-total':
      return t('coverage.totals.fromResponse', { total: row.totalKnown ?? 0 });
    case 'contradicted':
      return t('coverage.totals.contradicted');
    case 'unknown':
      // 🔴 Not "0", and not a dash: the platform does not provide a total for this list, and the sentence
      //    says which number is missing rather than leaving a reader to interpret a blank.
      return t('coverage.totals.unknown');
  }
}

/**
 * The "stored in full" value: a count, and a proportion **only** when `computeProgress` produced one.
 * 🔴 The decision is not made here — `row.percent` is null exactly when `progress.ts`'s four conditions
 *    failed. With a percent, the sentence carries it as "N of total (P%)"; without, it is the bare count
 *    and `percentNote` says why there is no percentage.
 */
function storedFact(row: CoverageRow): string {
  if (row.percent === null) return String(row.archived);
  return t('coverage.labels.archivedOf', {
    archived: row.archived,
    total: row.totalKnown ?? 0,
    percent: row.percent,
  });
}

/** Why there is no percentage, when there is none. `null` when the number exists. */
function noPercentFact(row: CoverageRow): string | null {
  if (row.percent !== null) return null;
  return t('coverage.labels.noPercent', {
    reason: t(row.unknownReasonKey ?? 'progress.reason.noTotalFromApi'),
  });
}

/**
 * The estimate line for the card face. 🔴 Always labelled: "estimate" is the first word of every branch
 * that carries a number, the rate it came from is named, and the three "no estimate" branches say which
 * fact is missing rather than leaving a numberless row. This is the short twin of `speedNote` (which
 * stays whole, in the details disclosure) — same inputs, same wording rules, one line instead of four.
 */
function etaLine(row: CoverageRow): string {
  const { speed } = row;
  if (row.pending === 0) return t('coverage.speed.etaNothingOwed');
  if (row.state === 'unregistered') return t('coverage.speed.etaUnregistered', { pending: row.pending });
  if (speed.etaDays === null) return t('coverage.speed.etaShort.none');
  const basis = speed.etaBasis === 'measured' ? t('coverage.speed.basisMeasured') : t('coverage.speed.basisCap');
  if (speed.etaDays < 1) {
    return t('coverage.speed.etaShort.hours', {
      hours: Math.max(1, Math.round(speed.etaDays * 24)),
      pending: row.pending,
      basis,
    });
  }
  return t('coverage.speed.etaShort.days', {
    days: speed.etaDays < 1.5 ? speed.etaDays.toFixed(1) : String(Math.ceil(speed.etaDays)),
    pending: row.pending,
    basis,
  });
}

/** Today's quota, as the meter under the card's ETA line. */
function quotaOf(row: CoverageRow): QuotaView {
  const { speed } = row;
  const line = speed.dayCap === null
    ? t('coverage.quota.none')
    : speed.counterStale
      ? t('coverage.quota.stale')
      : t('coverage.quota.today', { count: speed.bodiesToday, cap: speed.dayCap });
  return {
    bodiesToday: speed.counterStale ? 0 : speed.bodiesToday,
    dayCap: speed.dayCap,
    counterStale: speed.counterStale,
    line,
  };
}

/**
 * The remainder segment: how much a **response total** claims exists beyond what is listed so far.
 *
 * 🔴 Only a response total, and only while it is larger than the listed count. A contradicted total is
 *    not a claim about the account (progress.ts refuses it as a denominator, and the page's own sentence
 *    says so) — turning it into a bar segment would draw a number the model refused.
 */
function remainderOf(row: CoverageRow): number {
  if (row.totalSource !== 'response-total' || row.totalKnown === null) return 0;
  return Math.max(0, row.totalKnown - row.listed);
}

/** The composition bar's counts and its legend, in draw order. */
function barOf(row: CoverageRow): { bar: CoverageCardView['bar']; legend: BarLegendEntry[] } {
  const remainder = remainderOf(row);
  const bar = { archived: row.archived, owed: row.pending, failed: row.failuresTotal, remainder };
  const legend: BarLegendEntry[] = [
    { label: t('coverage.labels.archived'), count: row.archived, tone: 'ok' },
    { label: t('coverage.labels.pending'), count: row.pending, tone: 'hatch' },
  ];
  if (row.failuresTotal > 0) {
    legend.push({ label: t('coverage.bar.failed'), count: row.failuresTotal, tone: 'bad' });
  }
  if (remainder > 0) {
    legend.push({ label: t('coverage.bar.remainder'), count: remainder, tone: 'remainder' });
  }
  return { bar, legend };
}

/** The months as the card's chart data bag, with every wording the chart needs. */
function monthsViewOf(row: CoverageRow): MonthsView {
  // The chart exists only when there are month columns to draw. A set that is entirely time-unknown
  // must not draw a lone column that passes for a distribution: the `none` sentence says there is no
  // months chart to show, and the unknown bucket's own sentence carries the counts (ADR-032 §6).
  const hasMonths = row.months.length > 0;
  return {
    months: row.months,
    unknownTime: row.unknownTime,
    title: hasMonths ? t('coverage.months.title') : null,
    archivedLabel: t('coverage.months.archived'),
    pendingLabel: t('coverage.months.pending'),
    unknownLabel: t('common.unknownTimeShort'),
    unknownNote: row.unknownTime.archived + row.unknownTime.pending > 0
      ? t('coverage.months.unknownTime', {
        archived: row.unknownTime.archived,
        pending: row.unknownTime.pending,
      })
      : null,
    // The old rule, kept: the "no distribution yet" sentence covers the case where every conversation
    // sits in the time-unknown bucket too — those columns are not months, and none were drawn.
    noneNote: row.months.length === 0 && row.timed.archived + row.timed.pending === 0
      ? t('coverage.months.none')
      : null,
  };
}

// ---------------------------------------------------------------------------
// One card, then the page
// ---------------------------------------------------------------------------

/** One row to one card. Every ADR-032 fact lands either on the card face, in an alert, or in the details. */
function cardOf(row: CoverageRow, now: number): CoverageCardView {
  const alerts: CoverageAlert[] = [];
  const detailRows: DetailRow[] = [];

  // 2 · where the total came from.
  detailRows.push({ label: t('coverage.totals.label'), value: totalFact(row) });

  // 3 · stored and owed as two labelled quantities, and the state sentence's exact words.
  detailRows.push({ label: t('coverage.labels.archived'), value: storedFact(row) });
  detailRows.push({ label: t('coverage.labels.pending'), value: String(row.pending) });

  // 4 · the state, when it has words of its own. A `halted` row's full sentence is the alert the card
  //     opens with (it is the action half: what the reader has to do), so it is not repeated in the
  //     details. `in-progress` has no sentence by design — the ordinary case must not read like a notice.
  const state = stateNote(row, now);
  if (state !== null && row.state !== 'halted') {
    detailRows.push({ label: t('coverage.stateLabel'), value: state });
  }

  // 5 · the full speed sentence, whole, with its estimate labelled by the model's own wording.
  detailRows.push({ label: t('coverage.speedLabel'), value: speedNote(row, now) });

  // The counts source, when the numbers came from the header's copy rather than the store's own.
  const counts = countsNote(row);
  if (counts) detailRows.push({ label: t('coverage.countsLabel'), value: counts });

  // Failures by reason, the dropped overflow, the parked bucket, the empty streak.
  if (row.failuresTotal > 0 || row.failuresDropped > 0) {
    detailRows.push({
      label: t('coverage.labels.failures'),
      value: row.failures.map((group) => failureNote(group)).join('; '),
    });
    if (row.failuresDropped > 0) {
      detailRows.push({ label: '', value: t('coverage.labels.failuresDropped', { count: row.failuresDropped }) });
    }
  }
  if (row.parkedEmpty > 0) {
    detailRows.push({ label: t('coverage.labels.parked'), value: String(row.parkedEmpty) });
  }
  if (row.emptyStreak > 0) {
    detailRows.push({ label: '', value: t('coverage.labels.emptyStreak', { count: row.emptyStreak }) });
  }

  // 6 · the monthly distribution's explanation, and the time-unknown bucket's own sentence. The chart
  //     itself is on the card face; this is the wording around it.
  detailRows.push({ label: t('coverage.months.title'), value: t('coverage.months.note') });

  // A re-listing is an old notice about history; it stays on the record it happened to (W45), but it
  // no longer competes with today's state for the first read.
  if (row.relisted) {
    detailRows.push({
      label: '',
      value: t('coverage.labels.relisted', {
        when: new Date(row.relisted.at).toLocaleString(),
        recorded: row.relisted.recorded,
        held: row.relisted.held,
      }),
    });
  }

  // Alerts: only what a reader needs to act on, and never more than the facts.
  if (row.state === 'halted' && state) {
    alerts.push({ id: `${row.platform}:${row.scope}:halt`, tone: 'bad', text: state });
  }
  if (row.failuresTotal > 0) {
    alerts.push({
      id: `${row.platform}:${row.scope}:failures`,
      tone: 'bad',
      text: t('coverage.failuresShort', { count: row.failuresTotal }),
    });
  }

  const { bar, legend } = barOf(row);
  const percent = row.percent;
  return {
    key: `${row.platform}:${row.scope}`,
    platform: row.platform,
    scope: row.scope,
    monogram: monogramOf(row.platform),
    hueKey: hueKeyOf(row.platform),
    chip: chipOf(row),
    bar,
    legend,
    percent,
    percentTitle: percent === null ? null : `${percent}% ${t('coverage.labels.percent')}`,
    percentNote: noPercentFact(row),
    listed: enumNote(row),
    eta: etaLine(row),
    quota: quotaOf(row),
    alerts,
    detailsSummary: t('coverage.details.summary'),
    detailRows,
    months: monthsViewOf(row),
  };
}

/**
 * The one health line. Derived from the rows' states and nothing else, and every branch is either a
 * clean bill or a list of names: the overview may not soften a stop the model reported, nor may it
 * invent one the model did not.
 */
function healthOf(report: CoverageReport): CoverageHealth | null {
  if (report.rows.length === 0) return null;
  const stopped = [...new Set(report.rows
    .filter((row) => row.state === 'halted' || row.state === 'unregistered')
    .map((row) => row.platform))];
  const waiting = [...new Set(report.rows
    .filter((row) => row.state === 'waiting')
    .map((row) => row.platform))];
  if (stopped.length === 0 && waiting.length === 0) {
    return { tone: 'ok', text: t('coverage.health.ok', { total: report.rows.length }) };
  }
  const parts: string[] = [];
  if (stopped.length > 0) {
    parts.push(t('coverage.health.stopped', { count: stopped.length, list: stopped.join(', ') }));
  }
  if (waiting.length > 0) {
    parts.push(t('coverage.health.waiting', { count: waiting.length, list: waiting.join(', ') }));
  }
  return { tone: stopped.length > 0 ? 'bad' : 'wait', text: parts.join(' ') };
}

/** Build the whole page view: the header, the cards, the speed control, and nothing hidden from tests. */
export function coverageView(report: CoverageReport, now: number): CoveragePageView {
  const alerts: CoverageAlert[] = [];
  if (!report.enabled) alerts.push({ id: 'global:off', tone: 'warn', text: t('coverage.state.off') });
  else if (report.hostPaused) alerts.push({ id: 'global:host-paused', tone: 'bad', text: t('coverage.state.hostPaused') });

  // The last wake, demoted to one meta row. It is context for every card, not a card of its own.
  let tickNote: string | null = null;
  if (report.tick) {
    tickNote = report.tick.ran
      ? t('popup.lastTick.ran', { when: new Date(report.tick.at).toLocaleString(), targets: report.tick.targets })
      : t('popup.lastTick.skipped', {
        when: new Date(report.tick.at).toLocaleString(),
        reason: describeTickReason(report.tick.reason),
        targets: report.tick.targets,
      });
  }

  const hasRows = report.rows.length > 0;
  return {
    title: t('coverage.title'),
    subtitle: t('coverage.subtitleShort', { when: new Date(report.generatedAt).toLocaleString() }),
    aboutSummary: t('coverage.about.summary'),
    aboutNote: t('coverage.about.note'),
    stats: hasRows
      ? {
        stored: report.rows.reduce((n, row) => n + row.archived, 0),
        owed: report.rows.reduce((n, row) => n + row.pending, 0),
        failed: report.rows.reduce((n, row) => n + row.failuresTotal, 0),
      }
      : null,
    health: healthOf(report),
    alerts,
    tickNote,
    cards: report.rows.map((row) => cardOf(row, now)),
    empty: hasRows ? null : t('coverage.empty'),
    speed: speedView(report),
  };
}

// ---------------------------------------------------------------------------
// The popup's summary card
// ---------------------------------------------------------------------------

/** The label on the popup's link to the page. */
export function openCoverageLabel(): string {
  return t('coverage.card.open');
}

/** The card's own heading. */
export function coverageCardTitle(): string {
  return t('coverage.card.title');
}

/** One popup row: the chip, the mini composition bar, and the counts sentence. */
export interface CoverageCardLine {
  platform: string;
  scope: string;
  /** The status chip, the same closed set the page draws from. */
  chip: StatusChip;
  /** The mini bar's counts: two segments, stored and owed. */
  bar: { archived: number; owed: number };
  text: string;
}

/**
 * The popup's summary card, as data.
 *
 * 🔴 The card never shows a percentage and never shows an estimate. It is a summary of *counts* and one
 *    state word per row, and the page is where the numbers that need a caveat are explained; the mini
 *    bar and the chip change none of that, they are the same counts and the same state in two shapes.
 */
export function coverageCard(report: CoverageReport, now: number): { lines: CoverageCardLine[]; note: string | null } {
  const lines = report.rows.map((row) => ({
    platform: row.platform,
    scope: row.scope,
    chip: chipOf(row),
    bar: { archived: row.archived, owed: row.pending },
    // 🔴 Every placeholder the sentence carries is passed, and the platform and scope are two of them:
    //    the card is a list of rows, and two accounts of one platform are two different histories. A
    //    missing one does not fail loudly — it renders as the literal `{platform}`, which is how this was
    //    found in a real browser after the unit tests were green.
    text: t('coverage.card.line', {
      platform: row.platform,
      scope: row.scope,
      archived: row.archived,
      pending: row.pending,
    }),
  }));
  return {
    lines,
    note: report.rows.length === 0 ? t('coverage.empty') : null,
  };
}
