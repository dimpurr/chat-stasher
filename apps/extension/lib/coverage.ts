/**
 * ADR-032 · **The coverage model: what the extension knows about its own backfill, per platform and scope.**
 *
 * ADR-028 drew the division of labour and ADR-032 kept it: the CLI's `ui` is the **archive's** truth, and
 * this page shows the **capture's** progress — what has been listed, what has been fetched, what is still
 * owed, why the leg is or is not moving, how fast it is going, and where the conversations sit in time.
 *
 * ## What this module is, and what it deliberately is not
 *
 * It is a **pure function over a snapshot**: every input is passed in, nothing is read here, the clock is
 * passed in, and the output is data. The reading (storage.local, the debt store) belongs to the caller. Two
 * reasons, and both are the same reason this file is testable at all:
 *
 *  · the page and any test must be able to build a report from a fixture without a browser;
 *  · 🔴 **this page must never issue a request to a chat platform.** ADR-032 §1 and the W113 dispatch both
 *    say so. Keeping every read outside this module is what makes "the page only reads our own storage" a
 *    property of the code's shape rather than a promise in a comment — the module has no `fetch`, no
 *    `XMLHttpRequest`, no port, and nothing to reach a network with.
 *
 * ## The rules it inherits, and does not get to relax
 *
 * 1. **An unknown is never recorded as empty** (CLAUDE.md invariant 1). Every one of the fields below can
 *    say "we do not know", and the ones that can are *different fields* from the ones that hold a number —
 *    `countsFromStore` is false when the debt store could not be read, `enumState: 'truncated'` is not
 *    `'complete'`, and a month bucket with no recorded time is `unknownTime`, not a month.
 * 2. 🔴 **A lower bound is never drawn as a total** (ADR-032 §1). While the list is still being read, the
 *    number of rows is `listed` and the state is `in-progress`; the renderer prints `≥`. The percentage
 *    rule W10 put in `progress.ts` is reused rather than restated — this module calls `computeProgress` and
 *    carries its answer, so there is exactly one place that decides whether a `%` may appear.
 * 3. 🔴 **We do not guess a field name to make a row look complete** (the same rule the platform table
 *    follows for request URLs). A conversation whose source never gave us a time is counted under
 *    `unknownTime` with the reason attached; it is never bucketed into a month by inference.
 *
 * ## One thing this module cannot do yet, stated here rather than discovered later
 *
 * ADR-032 §1 wants ChatGPT split by **source** (main list / projects / archived). Those are three list
 * sources inside one account, and nothing in a header records which one an id came from — ADR-031's
 * per-scope source marking is W108 and is still being implemented. So a row here is one
 * **(platform, scope)** pair, which for ChatGPT means one row per *workspace* once W108 lands its scoped
 * keys, and not three rows per workspace. `seeAlso` on the row carries the platform's own note.
 */

import { t } from './i18n';
import { describeFailureReason, droppedOf, failuresOf, type FailureEntry } from './backfill/failures';
import { computeProgress, retryMinutesLeft, type ProgressInput } from './backfill/progress';
import { SPEED_PLANS, presetFrom, type SpeedPreset } from './backfill/speed';
import { haltNote } from './halt-note';
import {
  dayKeyOf,
  haltClassOf,
  type BackfillHeader,
  type HaltRecord,
  type TotalSource,
} from './backfill/types';
import type { BackfillTickRecord, TickSkipReason } from './backfill/alarm';

/**
 * 🔴 The source of a conversation time, named on the row that carries one.
 *
 * W113 records the time the **list response itself** gave for a conversation (`list-update` / `list-create`).
 * It is a real timestamp from the platform, not a file time — ADR-035 refused file mtimes outright, and
 * this is not that — but it is *the list's* view of the conversation, which is why the source is carried
 * rather than assumed: a page that said "by conversation time" about a list's `update_time` would be
 * claiming more than the data supports.
 */
export type CoverageTimeSource = 'list-update' | 'list-create';

/** One conversation's recorded time, or nothing. */
export interface CoverageTime {
  at: number;
  source: CoverageTimeSource;
}

/** How the list segment stands. `truncated` is not a kind of `complete` — see `EnumTruncation`. */
export type CoverageEnumState = 'complete' | 'in-progress' | 'truncated';

/** The debt store's answer for one (platform, scope), or `null` when it could not be read at all. */
export interface CoverageDebtInput {
  /** Ids still owed, in the store's own FIFO order (see `readDebtSet`). */
  pending: readonly string[];
  /** Ids settled. A conversation here is archived; nothing else may be inferred from it. */
  archived: readonly string[];
  /** Conversation times, by id, for the ids the source gave one for. Ids absent here have no recorded time. */
  times: ReadonlyMap<string, CoverageTime>;
}

/** Everything the model needs about one (platform, scope). */
export interface CoverageScopeInput {
  platform: string;
  scope: string;
  header: BackfillHeader;
  /** `null` = the debt store could not be read. **Not** the same fact as "nothing is owed". */
  debt: CoverageDebtInput | null;
  /** Whether the target registry names this pair. A header with no target is a leftover, not progress. */
  registered: boolean;
  /** What the last wake's walk decided about this scope, when it examined it. `null` = it did not. */
  skippedReason: TickSkipReason | null;
}

export interface CoverageInput {
  scopes: readonly CoverageScopeInput[];
  /** The explicit switch (`cs_backfill_enabled_v1`). Off by default and off means nothing runs. */
  enabled: boolean;
  /** Whether the archive's local host is currently unreachable (the delivery exit). */
  hostPaused: boolean;
  /** The stored speed preset, unvalidated — `presetFrom` is the only thing allowed to interpret it. */
  presetRaw: unknown;
  /** The last alarm wake's own record, or `null` when none has been written. */
  tick: BackfillTickRecord | null;
  /** Now, passed in so a report is reproducible. */
  now: number;
  /**
   * The month a moment belongs to, as `YYYY-MM`. Injected because a month boundary is a **local** fact for
   * the person reading the page and a test has to be able to pin it; the default is the host's own zone.
   */
  monthKey?: (at: number) => string;
}

/** One month's worth of one row's conversations, split the way ADR-032 §6 asks: archived apart from owed. */
export interface CoverageMonth {
  /** `YYYY-MM`. */
  month: string;
  archived: number;
  pending: number;
}

/** A failure reason and how many conversations it accounts for. */
export interface CoverageFailureGroup {
  reason: string;
  count: number;
}

/**
 * Where this row's numbers came from, because the two sources are not equally good and the page may not
 * present them as if they were.
 */
export interface CoverageCountsSource {
  /**
   * True when `archived`/`pending` are the debt store's own counts (the authority, lib/backfill/ledger.ts).
   * False means they are the header's convenience copy, which is a **weaker fact** and is worded as one by
   * `countsNote` — never presented as if the store had agreed.
   */
  fromStore: boolean;
}

/**
 * The current state of this platform+scope, as one closed set of values.
 *
 * 🔴 These are **not** interchangeable and the order of the tests below decides which is reported, so the
 *    reported one is always the one that actually governs: a leg whose host is unreachable is not also
 *    "in progress", and a leg with a permanent stop is not "waiting for a tab".
 */
export type CoverageState =
  /** The switch is off. Nothing runs, and no other state is worth reporting over this one. */
  | 'off'
  /** The switch is on and the archive's local host is unreachable — the delivery exit is down. */
  | 'host-paused'
  /** A stored stop still applies. `halt` carries it. */
  | 'halted'
  /** The header exists but the registry does not name it: a leftover from a removed target. */
  | 'unregistered'
  /** Today's drawn body cap is spent. Not a stop — it resumes tomorrow by itself. */
  | 'capped'
  /** Nothing is owed here. A measurement, and only ever said when the store agrees. */
  | 'done'
  /**
   * There is work and nothing is stopping it, but the last wake did not get to this platform — most often
   * because no page of it is open. `skippedReason` says what the walk decided.
   */
  | 'waiting'
  /** There is work, nothing is stopping it, and a wake ran it (or nothing has run yet). */
  | 'in-progress';

export interface CoverageSpeed {
  preset: SpeedPreset;
  /** Bodies this preset allows in one wake. */
  tickDetails: number;
  /** The ceiling in force **today**, drawn when the UTC day rolled over. `null` when the plan has no cap. */
  dayCap: number | null;
  /** True when the header's counter belongs to a different day than `now` — the numbers below are then stale. */
  counterStale: boolean;
  /** Bodies fetched today, from the header's own counter. */
  bodiesToday: number;
  /** Hours of the current UTC day that have passed. */
  hoursToday: number;
  /** Bodies/day measured over today so far; `null` until there is enough of a day and any bodies at all. */
  measuredPerDay: number | null;
  /** Minutes since the last body fetch, or `null` when none has been recorded. */
  minutesSinceLastBody: number | null;
  /**
   * Days to finish at `ratePerDay`. `null` when there is no rate to divide by **or** nothing owed.
   * 🔴 An estimate by construction: it assumes the leg keeps fetching, which nothing guarantees.
   */
  etaDays: number | null;
  /** Which rate `etaDays` used. `measured` is preferred; `cap` is the ceiling, so its ETA is a floor on time. */
  etaBasis: 'measured' | 'cap' | null;
}

export interface CoverageRow {
  platform: string;
  scope: string;
  registered: boolean;
  enumState: CoverageEnumState;
  /** Why the list stopped, when it stopped short of the end. `null` exactly when `enumState !== 'truncated'`. */
  truncation: BackfillHeader['enumCursor']['truncated'] | null;
  listed: number;
  totalKnown: number | null;
  totalSource: TotalSource;
  archived: number;
  pending: number;
  countsSource: CoverageCountsSource;
  /** From `computeProgress` — non-null only when a percentage is allowed to exist at all. */
  percent: number | null;
  /** Catalog key naming why there is no percentage. Non-null exactly when `percent` is null. */
  unknownReasonKey: string | null;
  /** Failures by reason, commonest first. */
  failures: CoverageFailureGroup[];
  failuresTotal: number;
  failuresDropped: number;
  /** Conversations parked because the body came back empty and nothing yet proves the endpoint works. */
  parkedEmpty: number;
  emptyStreak: number;
  /** Months with at least one conversation, ascending. Only months we have a time for. */
  months: CoverageMonth[];
  /**
   * Conversations with **no recorded time**, split archived/pending. Reported as its own bucket, never
   * spread over the months — ADR-032 §6: 缺时间的部分单列「时间未知」，不猜.
   */
  unknownTime: { archived: number; pending: number };
  /** How many ids we have a recorded time for, against how many exist. The denominator for `months`. */
  timed: { archived: number; pending: number };
  state: CoverageState;
  /** The stored stop, when there is one. `state === 'halted'` exactly when this is non-null. */
  halt: HaltRecord | null;
  /** What the last wake's walk decided about this scope, carried through for the renderer. */
  skippedReason: TickSkipReason | null;
  speed: CoverageSpeed;
  /** The last time either segment fetched anything, per the header. */
  lastFetchAt: { enumerate: number | null; detail: number | null };
  /** A re-listing happened here: ids were lost and the list was read again (W45). Survives in the header. */
  relisted: { at: number; recorded: number; held: number } | null;
  /** Whether an unreadable stored record sits at another layout's key for this pair (W36b/W47 preflight). */
  legacyOrphan: boolean;
}

export interface CoverageReport {
  generatedAt: number;
  preset: SpeedPreset;
  enabled: boolean;
  hostPaused: boolean;
  tick: BackfillTickRecord | null;
  rows: CoverageRow[];
  /** Rows grouped by platform, platforms in the order the rows were given. */
  platforms: Array<{ platform: string; rows: CoverageRow[] }>;
}

/** The default month bucketing: the **host's own calendar**, so a conversation at 00:30 on the 1st is in the month the reader is in. */
export function localMonthKey(at: number): string {
  const d = new Date(at);
  const month = String(d.getMonth() + 1).padStart(2, '0');
  return `${d.getFullYear()}-${month}`;
}

/** Hours elapsed in the UTC day `now` falls in. The daily counter is a UTC day (`dayKeyOf`). */
function hoursIntoUtcDay(now: number): number {
  const d = new Date(now);
  return d.getUTCHours() + d.getUTCMinutes() / 60 + d.getUTCSeconds() / 3600;
}

/**
 * How much of a day has to have passed, and how many bodies have to have been fetched, before a measured
 * rate is published.
 *
 * 🔴 Both conditions exist to stop the page from printing a number that is arithmetically fine and
 *    meaningless: one body at 00:05 extrapolates to 288 bodies/day and would make the ETA nonsense. Below
 *    the threshold the row reports **no** measured rate and says so, rather than a rate drawn from noise.
 */
export const MEASURED_RATE_MIN_HOURS = 6;
export const MEASURED_RATE_MIN_BODIES = 5;

function speedOf(header: BackfillHeader, presetRaw: unknown, pending: number, now: number): CoverageSpeed {
  const preset = presetFrom(presetRaw);
  const plan = SPEED_PLANS[preset];
  const planCap = plan.pace.detail.maxPerDay;
  const today = dayKeyOf(now);
  const counterStale = header.detailToday.day !== today;

  // The ceiling actually in force today: the drawn cap, clamped by the plan — the same `min` the engine
  // applies (engine.ts), so the page cannot quote a number the engine would not enforce.
  const dayCap = planCap === null
    ? null
    : Math.min(header.detailToday.cap ?? planCap, planCap);

  const bodiesToday = counterStale ? 0 : header.detailToday.count;
  const hoursToday = hoursIntoUtcDay(now);
  const measuredPerDay = !counterStale && hoursToday >= MEASURED_RATE_MIN_HOURS && bodiesToday >= MEASURED_RATE_MIN_BODIES
    ? (bodiesToday / hoursToday) * 24
    : null;

  const lastDetail = header.lastFetchAt?.detail ?? null;
  const minutesSinceLastBody = lastDetail === null ? null : Math.max(0, Math.floor((now - lastDetail) / 60_000));

  // 🔴 Prefer the measurement over the ceiling. The cap is what the engine *may* do; the measured rate is
  //    what it *has done*, and an ETA from the ceiling is a floor on the time taken, not a prediction.
  let etaDays: number | null = null;
  let etaBasis: CoverageSpeed['etaBasis'] = null;
  if (pending > 0) {
    if (measuredPerDay !== null && measuredPerDay > 0) {
      etaDays = pending / measuredPerDay;
      etaBasis = 'measured';
    } else if (dayCap !== null && dayCap > 0) {
      etaDays = pending / dayCap;
      etaBasis = 'cap';
    }
  }

  return {
    preset,
    tickDetails: plan.tickDetails,
    dayCap,
    counterStale,
    bodiesToday,
    hoursToday,
    measuredPerDay,
    minutesSinceLastBody,
    etaDays,
    etaBasis,
  };
}

/** The months a set of ids falls into, plus the ids with no recorded time. */
function monthsOf(
  ids: readonly string[],
  times: ReadonlyMap<string, CoverageTime>,
  monthKey: (at: number) => string,
): { months: Map<string, number>; unknown: number; timed: number } {
  const months = new Map<string, number>();
  let unknown = 0;
  let timed = 0;
  for (const id of ids) {
    const time = times.get(id);
    if (!time) {
      unknown += 1;
      continue;
    }
    timed += 1;
    const key = monthKey(time.at);
    months.set(key, (months.get(key) ?? 0) + 1);
  }
  return { months, unknown, timed };
}

/** Group failures by reason, commonest first, ties broken by reason so a report is reproducible. */
export function groupFailures(entries: readonly FailureEntry[]): CoverageFailureGroup[] {
  const counts = new Map<string, number>();
  for (const entry of entries) counts.set(entry.reason, (counts.get(entry.reason) ?? 0) + 1);
  return [...counts.entries()]
    .map(([reason, count]) => ({ reason, count }))
    .sort((a, b) => (b.count - a.count) || (a.reason < b.reason ? -1 : a.reason > b.reason ? 1 : 0));
}

function enumStateOf(header: BackfillHeader): CoverageEnumState {
  // 🔴 `truncated` is checked first, and that order is the whole point: a truncated cursor may ALSO carry
  //    `complete: true` (Perplexity's inference sets `truncated` while leaving `complete` false; a cursor
  //    that ran out mid-walk sets both). Reading `complete` first would print "listed N" as if N were the
  //    end of the list, which is the one thing ADR-032 §1 forbids.
  if (header.enumCursor.truncated !== undefined) return 'truncated';
  return header.enumCursor.complete ? 'complete' : 'in-progress';
}

function stateOf(input: CoverageScopeInput, enabled: boolean, hostPaused: boolean, speed: CoverageSpeed, now: number): CoverageState {
  const { header, debt, registered } = input;
  if (!enabled) return 'off';
  if (hostPaused) return 'host-paused';
  // 🔴 The stop is checked before the count, and that order is deliberate: a halted scope that happens to
  //    owe nothing right now is still halted, and reporting it as `done` would turn a named stop into a
  //    silent success. `stopStillApplies` is the *engine's* question and is answered by the engine (a halt
  //    that expired or that another build left is not in force); this model reports what the header holds.
  if (header.halted) return 'halted';
  if (!registered && debt !== null && debt.pending.length === 0) return 'unregistered';
  if (debt === null) {
    // The store could not be read. We know a header exists; we do not know whether work remains, so the
    // honest state is "there is work we cannot count" rather than either `done` or `in-progress`.
    return 'in-progress';
  }
  if (debt.pending.length === 0) return 'done';
  const today = dayKeyOf(now);
  const cap = speed.dayCap;
  if (cap !== null && header.detailToday.day === today && header.detailToday.count >= cap) return 'capped';
  if (input.skippedReason !== null) return 'waiting';
  return 'in-progress';
}

function rowOf(input: CoverageScopeInput, opts: {
  enabled: boolean;
  hostPaused: boolean;
  presetRaw: unknown;
  now: number;
  monthKey: (at: number) => string;
}): CoverageRow {
  const { header, debt, platform, scope } = input;
  const fromStore = debt !== null;
  const archived = debt ? debt.archived.length : header.archivedCount;
  const pending = debt ? debt.pending.length : header.pendingCount;
  const times = debt?.times ?? new Map<string, CoverageTime>();

  const archivedMonths = debt ? monthsOf(debt.archived, times, opts.monthKey) : null;
  const pendingMonths = debt ? monthsOf(debt.pending, times, opts.monthKey) : null;
  const monthNames = new Set<string>([
    ...(archivedMonths?.months.keys() ?? []),
    ...(pendingMonths?.months.keys() ?? []),
  ]);
  const months: CoverageMonth[] = [...monthNames].sort().map((month) => ({
    month,
    archived: archivedMonths?.months.get(month) ?? 0,
    pending: pendingMonths?.months.get(month) ?? 0,
  }));

  // 🔴 The percentage comes from `computeProgress` and nowhere else — the four conditions in progress.ts
  //    are the only thing allowed to decide that a `%` may exist, and a second rule here would be a way
  //    around them. Note which counts go in: the store's when it could be read, the header's otherwise.
  const progressInput: ProgressInput = {
    totalKnown: header.totalKnown,
    totalSource: header.totalSource,
    enumCursor: { offset: header.enumCursor.offset },
    pending,
    archived,
    halted: header.halted,
  };
  const progress = computeProgress(progressInput);

  const failures = groupFailures(failuresOf(header));
  const speed = speedOf(header, opts.presetRaw, pending, opts.now);

  return {
    platform,
    scope,
    registered: input.registered,
    enumState: enumStateOf(header),
    truncation: header.enumCursor.truncated ?? null,
    listed: header.enumCursor.offset,
    totalKnown: header.totalKnown,
    totalSource: header.totalSource,
    archived,
    pending,
    countsSource: { fromStore },
    percent: progress.percent,
    unknownReasonKey: progress.unknownReasonKey,
    failures,
    failuresTotal: failures.reduce((n, f) => n + f.count, 0),
    failuresDropped: droppedOf(header),
    parkedEmpty: header.parkedEmpty?.length ?? 0,
    emptyStreak: header.emptyStreak ?? 0,
    months,
    unknownTime: { archived: archivedMonths?.unknown ?? 0, pending: pendingMonths?.unknown ?? 0 },
    timed: { archived: archivedMonths?.timed ?? 0, pending: pendingMonths?.timed ?? 0 },
    state: stateOf(input, opts.enabled, opts.hostPaused, speed, opts.now),
    halt: header.halted,
    skippedReason: input.skippedReason,
    speed,
    lastFetchAt: {
      enumerate: header.lastFetchAt?.enumerate ?? null,
      detail: header.lastFetchAt?.detail ?? null,
    },
    relisted: header.relisted ?? null,
    legacyOrphan: false,
  };
}

/** Build the report. Pure: same inputs, same output, no reads and no writes. */
export function buildCoverage(input: CoverageInput): CoverageReport {
  const monthKey = input.monthKey ?? localMonthKey;
  const opts = {
    enabled: input.enabled,
    hostPaused: input.hostPaused,
    presetRaw: input.presetRaw,
    now: input.now,
    monthKey,
  };
  const rows = input.scopes
    .map((scope) => rowOf(scope, opts))
    // Stable, explainable order: platform, then scope. The page groups by platform anyway; this keeps the
    // array itself reproducible, which is what a test can assert.
    .sort((a, b) => (a.platform < b.platform ? -1 : a.platform > b.platform ? 1 : 0)
      || (a.scope < b.scope ? -1 : a.scope > b.scope ? 1 : 0));

  const byPlatform = new Map<string, CoverageRow[]>();
  for (const row of rows) {
    const list = byPlatform.get(row.platform);
    if (list) list.push(row);
    else byPlatform.set(row.platform, [row]);
  }

  return {
    generatedAt: input.now,
    preset: presetFrom(input.presetRaw),
    enabled: input.enabled,
    hostPaused: input.hostPaused,
    tick: input.tick,
    rows,
    platforms: [...byPlatform.entries()].map(([platform, list]) => ({ platform, rows: list })),
  };
}

// ---------------------------------------------------------------------------
// Render-time wording
//
// Every sentence is resolved through `t` at paint time, for the reason popup-view.ts states: the model is
// built once and may be shown in either language, so it may not hold a language. The state sentence reuses
// `haltNote` (lib/halt-note.ts), which is the same function the popup uses — the two must not be able to
// say different things about one record.
// ---------------------------------------------------------------------------

/**
 * One wake's decision about one target, in plain words.
 *
 * 🔴 The values are the **kebab-case** reason codes the tick trace stores, and the catalog keys are camel
 *    case, so a mapping has to exist somewhere. It is a function with an explicit default rather than a
 *    template string, because a template would turn a code this build has never seen into a *missing
 *    catalog entry* — the key `tick.skip.no-new-thing` would simply render as itself, and a reader would
 *    conclude the platform was passed over for that reason. Anything unrecognised is reported as what it
 *    is: a code this build cannot word.
 */
export function describeSkipReason(reason: string | null): string {
  switch (reason) {
    case 'no-http-port':
      return t('tick.skip.noHttpPort');
    case 'halted':
      return t('tick.skip.halted');
    case 'waiting-retry':
      return t('tick.skip.waitingRetry');
    case 'daily-cap':
      return t('tick.skip.dailyCap');
    case 'state-unreadable':
      return t('tick.skip.stateUnreadable');
    default:
      return t('tick.skip.unknown', { reason: reason ?? t('common.unknownShort') });
  }
}

/**
 * One line of plain words for this row's state, and — when the state is a stop — the same sentence the
 * popup shows for the same record.
 *
 * `null` for the states whose wording belongs to the surrounding layout (`done`, `in-progress`), so a
 * caller that wants to print nothing for the ordinary case can.
 */
export function stateNote(row: CoverageRow, now: number): string | null {
  switch (row.state) {
    case 'off':
      return t('coverage.state.off');
    case 'host-paused':
      return t('coverage.state.hostPaused');
    case 'halted':
      // 🔴 The same sentence as the popup's, from the same function. It is the *action* half of the row:
      //    what the user has to do, in their own language, without a reason code in it.
      return row.halt ? haltNote(row.halt, row.platform, row.pending, now) : t('coverage.state.haltedNoRecord');
    case 'unregistered':
      return t('coverage.state.unregistered', { platform: row.platform });
    case 'capped':
      return t('coverage.state.capped', { cap: row.speed.dayCap ?? 0 });
    case 'waiting':
      return t('coverage.state.waiting', { why: describeSkipReason(row.skippedReason) });
    case 'done':
      return t('coverage.state.done');
    case 'in-progress':
      return null;
  }
}

/**
 * The speed and ETA sentence.
 *
 * 🔴 Three rules it holds, all of them ADR-032 §5 and its "估算值显式标「估算」":
 *  · an ETA is **always** labelled an estimate, and which rate produced it is named — a ceiling-based ETA
 *    is a floor on the time taken, and saying so is the difference between an estimate and a promise;
 *  · no ETA is printed when there is no rate to divide by, and the sentence says which number is missing
 *    instead of showing a dash the reader has to interpret;
 *  · the measured rate is only printed when there is enough of a day behind it (see `MEASURED_RATE_MIN_HOURS`).
 */
export function speedNote(row: CoverageRow, now: number): string {
  const { speed } = row;
  const cap = speed.dayCap === null
    ? t('coverage.speed.noCap')
    : t('coverage.speed.cap', { cap: speed.dayCap, tick: speed.tickDetails });
  const measured = speed.measuredPerDay === null
    ? t('coverage.speed.measuredUnknown', { hours: Math.floor(speed.hoursToday) })
    : t('coverage.speed.measured', { rate: Math.round(speed.measuredPerDay), bodies: speed.bodiesToday, hours: Math.floor(speed.hoursToday) });
  const eta = row.pending === 0
    ? t('coverage.speed.etaNothingOwed')
    : speed.etaDays === null
      ? t('coverage.speed.etaUnknown')
      : t('coverage.speed.eta', {
        days: speed.etaDays < 1.5 ? speed.etaDays.toFixed(1) : String(Math.ceil(speed.etaDays)),
        basis: speed.etaBasis === 'measured' ? t('coverage.speed.basisMeasured') : t('coverage.speed.basisCap'),
        pending: row.pending,
      });
  const idle = speed.minutesSinceLastBody === null
    ? t('coverage.speed.neverFetched')
    : t('coverage.speed.lastBody', { minutes: speed.minutesSinceLastBody });
  return `${cap} · ${measured} · ${eta} · ${idle}`;
}

/**
 * The list's own state, in plain words — and 🔴 the one place the `≥` rule is worded.
 *
 * ADR-032 §1: "已列完 = N" versus "仍在列 ≥ N". The distinction is `enumState`, not a guess from the
 * numbers, and the renderer is handed the finished sentence so it cannot print `listed` bare while the
 * list is still being read.
 */
export function enumNote(row: CoverageRow): string {
  switch (row.enumState) {
    case 'complete':
      return t('coverage.enum.complete', { listed: row.listed });
    case 'in-progress':
      return t('coverage.enum.inProgress', { listed: row.listed });
    case 'truncated':
      return t('coverage.enum.truncated', {
        listed: row.listed,
        why: describeTruncation(row.truncation),
      });
  }
}

/**
 * Why enumeration stopped short. Each value is a different fact — "the record has no cursor" is not "the
 * response said nothing about a next page" is not "we inferred the end from an empty page" — and folding
 * them into one sentence would hide which one a user should wait out and which one needs a fix.
 */
export function describeTruncation(kind: string | null | undefined): string {
  switch (kind) {
    case 'cursor-missing':
      return t('coverage.enum.why.cursorMissing');
    case 'has-more-missing':
      return t('coverage.enum.why.hasMoreMissing');
    case 'empty-page-inferred':
      return t('coverage.enum.why.emptyPageInferred');
    case 'short-page-inferred':
      return t('coverage.enum.why.shortPageInferred');
    default:
      return t('coverage.enum.why.unknown', { kind: kind ?? t('common.unknownShort') });
  }
}

/** Where this row's two counts came from, when they are not the debt store's own. `null` when they are. */
export function countsNote(row: CoverageRow): string | null {
  return row.countsSource.fromStore ? null : t('coverage.counts.fromHeader');
}

/** One failure reason in plain words: the reason's own sentence from the catalog the popup already uses. */
export function failureNote(group: CoverageFailureGroup): string {
  return `${describeFailureReason(group.reason)} (${group.reason}) ×${group.count}`;
}

/** The retry moment of a transient stop, or `null` when the stop is not transient. Exported so the page and any test agree. */
export function retryMinutesOf(record: HaltRecord | null, now: number): number | null {
  if (!record) return null;
  if (haltClassOf(record.reason) !== 'transient') return null;
  return retryMinutesLeft(record, now);
}

/** Convenience for a caller that only wants the rows carrying a defect worth a reviewer's eye. */
export function problemRows(report: CoverageReport): CoverageRow[] {
  return report.rows.filter((row) => row.state === 'halted' || row.failuresTotal > 0 || row.unknownTime.pending + row.unknownTime.archived > 0);
}
