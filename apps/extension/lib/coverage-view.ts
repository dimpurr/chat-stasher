/**
 * ADR-032 · **The coverage page's render layer: what is shown, as data, with no DOM in it.**
 *
 * The page's own entrypoint (`entrypoints/coverage/main.ts`) cannot run under node, and every rule that
 * this page must not break — a lower bound never drawn as a total, a percentage only with a denominator,
 * an ETA always labelled an estimate, a time-unknown bucket that is never spread over months — has to be
 * assertable. So *what is displayed* is built here, as an array of plain blocks, and the entrypoint only
 * walks that array into elements. Same split, and the same reason, as `lib/popup-view.ts`.
 *
 * 🔴 **Every sentence goes through `t` at render time.** A report may be built once and painted in either
 *    language, so no model holds a language — which is also why `lib/coverage.ts` carries catalog keys and
 *    numbers rather than strings.
 *
 * 🔴 **Nothing here decides anything.** The six questions ADR-032's "information that must be shown" asks
 * are answered in
 *    `lib/coverage.ts`; this file chooses the order and the wording. A rule implemented twice would be a
 *    rule that can disagree with itself, and the one that drifts is the one nobody is looking at.
 */

import { t } from './i18n';
import {
  countsNote,
  describeSkipReason,
  enumNote,
  failureNote,
  speedNote,
  stateNote,
  type CoverageReport,
  type CoverageRow,
} from './coverage';
import { SPEED_PRESET_ORDER, SPEED_PLANS, type SpeedPreset } from './backfill/speed';
import { describeTickReason } from './popup-view';

/** One thing to paint. Deliberately a small closed set: a view that can emit anything is a view no test can pin. */
export type CoverageBlock =
  /** A row's title. `level` 2 is a platform, 3 is one scope under it. */
  | { kind: 'heading'; level: 2 | 3; text: string }
  | { kind: 'note'; tone: 'info' | 'warn' | 'bad'; text: string }
  | { kind: 'facts'; rows: Array<{ label: string; value: string }> }
  /** A three-column table with its own header. Used for the monthly distribution. */
  | { kind: 'table'; headers: [string, string, string]; rows: Array<[string, string, string]> }
  /** A bulleted list of sentences. */
  | { kind: 'list'; items: string[] }
  /** The speed control, with the presets in the order they are offered. */
  | { kind: 'speed'; current: SpeedPreset; options: Array<{ preset: SpeedPreset; label: string; what: string }> };

/** One platform's section: its rows, in the report's order. */
export interface CoverageSection {
  platform: string;
  blocks: CoverageBlock[];
}

/** The whole page, ready to paint. */
export interface CoverageView {
  title: string;
  subtitle: string;
  /** The banner when the switch is off or the host is unreachable — the two facts that stop everything. */
  banners: CoverageBlock[];
  sections: CoverageSection[];
  legend: string;
}

/** The preset's own name, as the control shows it. */
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

/** What a preset means, in the numbers it selects. Taken from the plan, never retyped. */
export function presetWhat(preset: SpeedPreset): string {
  const plan = SPEED_PLANS[preset];
  return t('coverage.preset.what', {
    cap: plan.pace.detail.maxPerDay ?? 0,
    tick: plan.tickDetails,
  });
}

/**
 * The speed control.
 *
 * 🔴 The risk note is a **block beside the control**, not a tooltip, and it appears with the preset that
 *    carries it (ADR-032 §3: the fast preset comes with a risk note). A warning the user has to hover to
 *    find is a warning about a
 *    setting they have already changed.
 */
export function speedBlocks(report: CoverageReport): CoverageBlock[] {
  const blocks: CoverageBlock[] = [{
    kind: 'heading',
    level: 3,
    text: t('coverage.preset.title'),
  }, {
    kind: 'speed',
    current: report.preset,
    options: SPEED_PRESET_ORDER.map((preset) => ({
      preset,
      label: presetLabel(preset),
      what: presetWhat(preset),
    })),
  }];
  if (SPEED_PLANS[report.preset].carriesRisk) {
    blocks.push({ kind: 'note', tone: 'warn', text: t('coverage.preset.risky') });
  }
  return blocks;
}

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
 *
 * 🔴 The decision is not made here — `row.percent` is null exactly when `progress.ts`'s four conditions
 *    failed, and this function only chooses between the two shapes of sentence. A second rule here would
 *    be a way around that one.
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

/** One scope's blocks. Every one of ADR-032's six items is emitted here, and nothing is conditional on "there is something to say". */
function rowBlocks(row: CoverageRow, now: number): CoverageBlock[] {
  const blocks: CoverageBlock[] = [];

  // 2 · where the total came from, and 3 · what is stored against what is owed. The stored line carries
  // the percentage only when `computeProgress` allowed one to exist — the rule is not restated here.
  blocks.push({
    kind: 'facts',
    rows: [
      // 2 · where the total came from.
      { label: t('coverage.totals.label'), value: totalFact(row) },
      // 3 · stored and owed as two labelled quantities, so "one of them is zero" is visible rather than
      //     folded into a sentence.
      { label: t('coverage.labels.archived'), value: storedFact(row) },
      { label: t('coverage.labels.pending'), value: String(row.pending) },
    ],
  });
  const noPercent = noPercentFact(row);
  if (noPercent) blocks.push({ kind: 'note', tone: 'info', text: noPercent });

  // 1 · how much is listed, and whether the list is finished. `enumNote` already carries the `≥` rule.
  blocks.push({ kind: 'note', tone: 'info', text: enumNote(row) });

  const counts = countsNote(row);
  if (counts) blocks.push({ kind: 'note', tone: 'warn', text: counts });

  const detail: Array<{ label: string; value: string }> = [];
  if (row.failuresTotal > 0 || row.failuresDropped > 0) {
    detail.push({
      label: t('coverage.labels.failures'),
      value: row.failures.map((group) => failureNote(group)).join('; '),
    });
    if (row.failuresDropped > 0) {
      detail.push({ label: '', value: t('coverage.labels.failuresDropped', { count: row.failuresDropped }) });
    }
  }
  if (row.parkedEmpty > 0) {
    detail.push({ label: t('coverage.labels.parked'), value: String(row.parkedEmpty) });
  }
  if (row.emptyStreak > 0) {
    detail.push({ label: '', value: t('coverage.labels.emptyStreak', { count: row.emptyStreak }) });
  }
  if (detail.length > 0) blocks.push({ kind: 'facts', rows: detail });

  // 4 · the state and its reason, in plain words. `stateNote` reuses the popup's own halt sentences.
  const state = stateNote(row, now);
  if (state) {
    blocks.push({
      kind: 'note',
      tone: row.state === 'halted' ? 'bad' : row.state === 'done' ? 'info' : 'warn',
      text: state,
    });
  }
  if (row.state === 'waiting' && row.skippedReason === null) {
    blocks.push({ kind: 'note', tone: 'warn', text: describeSkipReason(null) });
  }

  // 5 · speed and ETA.
  blocks.push({ kind: 'note', tone: 'info', text: speedNote(row, now) });

  // 6 · the monthly distribution, with the time-unknown bucket kept out of the months entirely.
  blocks.push({
    kind: 'note',
    tone: 'info',
    text: t('coverage.months.note'),
  });
  if (row.months.length === 0 && row.timed.archived + row.timed.pending === 0) {
    blocks.push({ kind: 'note', tone: 'warn', text: t('coverage.months.none') });
  } else {
    blocks.push({
      kind: 'table',
      headers: [t('coverage.months.title'), t('coverage.months.archived'), t('coverage.months.pending')],
      rows: row.months.map((month) => [month.month, String(month.archived), String(month.pending)]),
    });
  }
  if (row.unknownTime.archived + row.unknownTime.pending > 0) {
    blocks.push({
      kind: 'note',
      tone: 'warn',
      text: t('coverage.months.unknownTime', {
        archived: row.unknownTime.archived,
        pending: row.unknownTime.pending,
      }),
    });
  }

  // A re-listing is a defect the header keeps, and it belongs on the row it happened to (W45).
  if (row.relisted) {
    blocks.push({
      kind: 'note',
      tone: 'bad',
      text: t('coverage.labels.relisted', {
        when: new Date(row.relisted.at).toLocaleString(),
        recorded: row.relisted.recorded,
        held: row.relisted.held,
      }),
    });
  }

  return blocks;
}

/** Build the whole page. */
export function coverageView(report: CoverageReport, now: number): CoverageView {
  const banners: CoverageBlock[] = [];
  if (!report.enabled) banners.push({ kind: 'note', tone: 'warn', text: t('coverage.state.off') });
  else if (report.hostPaused) banners.push({ kind: 'note', tone: 'bad', text: t('coverage.state.hostPaused') });

  // What the last wake did, when it did not run: the sentence a reader needs before any row's detail.
  if (report.tick) {
    const head = report.tick.ran
      ? t('popup.lastTick.ran', { when: new Date(report.tick.at).toLocaleString(), targets: report.tick.targets })
      : t('popup.lastTick.skipped', {
        when: new Date(report.tick.at).toLocaleString(),
        reason: describeTickReason(report.tick.reason),
        targets: report.tick.targets,
      });
    banners.push({ kind: 'note', tone: 'info', text: head });
  }

  const sections: CoverageSection[] = report.platforms.map((group) => {
    const platform = group.platform;
    const rowGroups = group.rows.map((row) => row);
    const blocks: CoverageBlock[] = [{ kind: 'heading', level: 2, text: platform }];
    // One row per (platform, scope). A platform with a single scope does not need a second heading; the
    // scope is still named, because two accounts of one platform are two different histories.
    for (const row of rowGroups) {
      blocks.push({ kind: 'heading', level: 3, text: row.scope });
      blocks.push(...rowBlocks(row, now));
    }
    return { platform, blocks };
  });

  return {
    title: t('coverage.title'),
    subtitle: t('coverage.subtitle', { when: new Date(report.generatedAt).toLocaleString() }),
    banners,
    sections,
    legend: report.rows.length === 0 ? t('coverage.empty') : '',
  };
}

/** The label on the popup's link to the page. */
export function openCoverageLabel(): string {
  return t('coverage.card.open');
}

/** The card's own heading. */
export function coverageCardTitle(): string {
  return t('coverage.card.title');
}

/** The popup's summary card, as data: one line per row plus the entry point's label. */
export interface CoverageCardLine {
  platform: string;
  scope: string;
  text: string;
}

export function coverageCard(report: CoverageReport, now: number): { lines: CoverageCardLine[]; note: string | null } {
  const lines = report.rows.map((row) => ({
    platform: row.platform,
    scope: row.scope,
    // 🔴 Every placeholder the sentence carries is passed, and the platform and scope are two of them:
    //    the card is a list of rows, and two accounts of one platform are two different histories. A
    //    missing one does not fail loudly — it renders as the literal `{platform}`, which is how this was
    //    found in a real browser after the unit tests were green.
    text: t('coverage.card.line', {
      platform: row.platform,
      scope: row.scope,
      archived: row.archived,
      pending: row.pending,
      state: stateNote(row, now) ?? t('coverage.card.running'),
    }),
  }));
  // 🔴 The card never shows a percentage and never shows an estimate. It is a summary of *counts*, and the
  //    page is where the numbers that need a caveat are explained.
  return {
    lines,
    note: report.rows.length === 0 ? t('coverage.empty') : null,
  };
}
