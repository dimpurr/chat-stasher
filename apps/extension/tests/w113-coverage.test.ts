/**
 * W113 · ADR-032 — the coverage model and the page's view layer.
 *
 * Every case here is one of ADR-032's own requirements turned into an assertion, and the ones that matter
 * most are the ones that pin what must **not** appear:
 *
 *  · a lower bound is never drawn as a total (§1) — `in-progress` and `truncated` are distinct from
 *    `complete`, and `truncated` wins even when the cursor also says `complete`;
 *  · a percentage exists only where `computeProgress` allowed one, so an untrustworthy denominator
 *    produces a card with no percent ring and a sentence that says why;
 *  · an estimate is always labelled one, and named as the rate it came from (§5);
 *  · a conversation with no recorded time is counted in its own bucket and is **never** spread over a
 *    month (§6);
 *  · every one of the six items is emitted for every card, so "there was nothing to say" cannot become
 *    "the item was dropped".
 *
 * W149 · The view layer is card-shaped now (`lib/coverage-view.ts` produces one `CoverageCardView` per
 * platform×scope plus an overview header), so the assertions above are rewritten against that shape —
 * same rules, same strength, one wording: every sentence the view can emit is still collected and pinned.
 *
 * No browser, no IndexedDB, no clock: the model is pure and takes `now`.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { buildCoverage, describeSkipReason, enumNote, localMonthKey, problemRows, stateNote, speedNote, type CoverageInput, type CoverageRow, type CoverageScopeInput } from '../lib/coverage';
import { coverageCard, coverageView, presetWhat } from '../lib/coverage-view';
import { SPEED_PLANS } from '../lib/backfill/speed';
import { initialState, type BackfillHeader, type HaltRecord } from '../lib/backfill/types';
import type { CoveragePageView } from '../lib/coverage-view';

const NOW = Date.UTC(2026, 8, 24, 12, 0, 0); // 2026-09-24T12:00:00Z
const PLATFORM = 'chatgpt';
const SCOPE = 'default';

/** A header with every field the model reads, defaulted to "nothing has happened here yet". */
function header(over: Partial<BackfillHeader> = {}): BackfillHeader {
  const base = initialState(PLATFORM, SCOPE);
  return {
    v: 2,
    platform: PLATFORM,
    scope: SCOPE,
    totalKnown: base.totalKnown,
    totalSource: base.totalSource,
    enumCursor: base.enumCursor,
    pendingCount: 0,
    archivedCount: 0,
    detailToday: { day: '2026-09-24', count: 0 },
    halted: null,
    ...over,
  };
}

function scope(over: Partial<CoverageScopeInput> = {}): CoverageScopeInput {
  return {
    platform: PLATFORM,
    scope: SCOPE,
    header: header(),
    debt: { pending: [], archived: [], times: new Map() },
    registered: true,
    skippedReason: null,
    ...over,
  };
}

function input(over: Partial<CoverageInput> = {}): CoverageInput {
  return {
    scopes: [scope()],
    enabled: true,
    hostPaused: false,
    presetRaw: 'gentle',
    tick: null,
    now: NOW,
    // Pinned so a month assertion does not depend on the machine's zone.
    monthKey: localMonthKey,
    ...over,
  };
}

/** The one row a single-scope report has. */
function rowOf(i: CoverageInput) {
  const report = buildCoverage(i);
  expect(report.rows).toHaveLength(1);
  return report.rows[0]!;
}

/** The one card a single-scope page view has. */
function cardOf(i: CoverageInput) {
  const view = coverageView(buildCoverage(i), NOW);
  expect(view.cards).toHaveLength(1);
  return view.cards[0]!;
}

/** Every sentence the view layer can emit for one page, flattened. The placeholder guard pins exactly this. */
function pageTexts(view: CoveragePageView): string[] {
  const texts: string[] = [
    view.title,
    view.subtitle,
    view.aboutSummary,
    view.aboutNote,
    view.empty ?? '',
    view.tickNote ?? '',
    view.health?.text ?? '',
  ];
  for (const alert of view.alerts) texts.push(alert.text);
  for (const card of view.cards) {
    texts.push(card.platform, card.scope, card.monogram, card.chip.word, card.listed, card.eta, card.quota.line);
    if (card.percentNote) texts.push(card.percentNote);
    if (card.percentTitle) texts.push(card.percentTitle);
    for (const alert of card.alerts) texts.push(alert.text);
    for (const row of card.detailRows) texts.push(row.label, row.value);
    for (const entry of card.legend) texts.push(entry.label, String(entry.count));
    if (card.months.title) texts.push(card.months.title);
    texts.push(card.months.archivedLabel, card.months.pendingLabel, card.months.unknownLabel);
    if (card.months.unknownNote) texts.push(card.months.unknownNote);
    if (card.months.noneNote) texts.push(card.months.noneNote);
  }
  for (const option of view.speed.options) texts.push(option.label, option.what);
  if (view.speed.riskNote) texts.push(view.speed.riskNote);
  return texts;
}

beforeEach(() => {
  vi.stubGlobal('browser', withI18n({} as never));
});

describe('W113 · item 1 — how much is listed, and whether the list is finished', () => {
  it('🔴 while the list is still being read, the number is a lower bound', () => {
    const row = rowOf(input({ scopes: [scope({ header: header({ enumCursor: { offset: 300, complete: false } }) })] }));
    expect(row.enumState).toBe('in-progress');
    const card = cardOf(input({
      scopes: [scope({ header: header({ enumCursor: { offset: 300, complete: false } }) })],
    }));
    expect(card.listed, 'the wording must mark the number as a lower bound').toContain('≥');
  });

  it('a finished list is a different sentence from a running one', () => {
    const complete = rowOf(input({ scopes: [scope({ header: header({ enumCursor: { offset: 300, complete: true } }) })] }));
    expect(complete.enumState).toBe('complete');
    expect(complete.truncation).toBeNull();
  });

  it('🔴 truncated wins over complete, because a cursor may claim both', () => {
    // Perplexity's inference leaves `complete: false`; a walk that ran out mid-way can leave both set.
    // Reading `complete` first would print the count as if it were the end of the list.
    const row = rowOf(input({
      scopes: [scope({ header: header({ enumCursor: { offset: 40, complete: true, truncated: 'has-more-missing' } }) })],
    }));
    expect(row.enumState).toBe('truncated');
    expect(row.truncation).toBe('has-more-missing');
  });

  it('🔴 W113b · a truncated list carries the `≥` as well, because it too was not read to its end', () => {
    // ADR-032 §1 requires the `≥` on every list that was not read to its end, and `truncated` is exactly
    // that: the words already said the list stopped short while the number was printed bare, which is a
    // lower bound drawn as a total.
    const row = rowOf(input({
      scopes: [scope({ header: header({ enumCursor: { offset: 40, complete: true, truncated: 'cursor-missing' } }) })],
    }));
    const note = enumNote(row);
    expect(note).toContain('≥');
    // …and the number it is about is still there, marked as what it is.
    expect(note).toContain('40');
  });
});

describe('W113 · item 2 — where the total came from', () => {
  it('a response total is used, and named as the platform\'s', () => {
    const row = rowOf(input({
      scopes: [scope({ header: header({ totalKnown: 900, totalSource: 'response-total', archivedCount: 90, enumCursor: { offset: 900, complete: true } }) })],
    }));
    expect(row.totalKnown).toBe(900);
    expect(row.totalSource).toBe('response-total');
  });

  it('🔴 a disproved total produces no percentage at all', () => {
    const row = rowOf(input({
      scopes: [scope({ header: header({ totalKnown: 901, totalSource: 'contradicted', archivedCount: 5, enumCursor: { offset: 7391, complete: true } }) })],
    }));
    expect(row.percent).toBeNull();
    expect(row.unknownReasonKey).toBe('progress.reason.totalContradicted');
  });

  it('the platform providing no total is said in its own words', () => {
    const row = rowOf(input({ scopes: [scope({ header: header({ totalKnown: null, totalSource: 'unknown' }) })] }));
    expect(row.percent).toBeNull();
    expect(row.unknownReasonKey).toBe('progress.reason.noTotalFromApi');
  });

  it('🔴 W113b · a response total is printed as the platform\'s own number, with nothing else in the sentence', () => {
    const card = cardOf(input({
      scopes: [scope({ header: header({ totalKnown: 900, totalSource: 'response-total' }) })],
    }));
    const values = card.detailRows.map((row) => row.value);
    // §2 asks *where* the total came from, and that is the whole claim: a number the platform reports, not a
    // measurement of the account. The sentence used to open with a fragment ("at least see above;") that
    // pointed at nothing.
    expect(values).toContain("the platform's own total is 900");
    expect(values.some((v) => v.includes('see above'))).toBe(false);
  });
});

describe('W113 · item 3 — stored, owed, failed, parked', () => {
  it('the debt store is the authority when it can be read', () => {
    const debt = { pending: ['p1', 'p2'], archived: ['a1'], times: new Map() };
    const row = rowOf(input({ scopes: [scope({ debt, header: header({ pendingCount: 99, archivedCount: 99 }) })] }));
    expect(row.archived).toBe(1);
    expect(row.pending).toBe(2);
    expect(row.countsSource.fromStore).toBe(true);
  });

  it('🔴 a debt store that could not be read is not an empty one', () => {
    const row = rowOf(input({ scopes: [scope({ debt: null, header: header({ pendingCount: 7, archivedCount: 3 }) })] }));
    // The header's numbers are used *and flagged*, rather than a zero standing in for "unknown".
    expect(row.pending).toBe(7);
    expect(row.archived).toBe(3);
    expect(row.countsSource.fromStore).toBe(false);
    const view = coverageView(buildCoverage(input({
      scopes: [scope({ debt: null, header: header({ pendingCount: 7, archivedCount: 3 }) })],
    })), NOW);
    expect(pageTexts(view).some((text) => text.includes('authority'))).toBe(true);
    const card = cardOf(input({
      scopes: [scope({ debt: null, header: header({ pendingCount: 7, archivedCount: 3 }) })],
    }));
    expect(card.bar.archived).toBe(3);
    expect(card.bar.owed).toBe(7);
  });

  it('failures are grouped by reason, commonest first, and the dropped count survives', () => {
    const failures = [
      { shortId: 'aaaaaaaa', platform: PLATFORM, reason: 'detail-empty', at: 1 },
      { shortId: 'bbbbbbbb', platform: PLATFORM, reason: 'detail-empty', at: 2 },
      { shortId: 'cccccccc', platform: PLATFORM, reason: 'detail-too-long', at: 3 },
    ];
    const row = rowOf(input({ scopes: [scope({ header: header({ failures, failuresDropped: 4 }) })] }));
    expect(row.failures).toEqual([
      { reason: 'detail-empty', count: 2 },
      { reason: 'detail-too-long', count: 1 },
    ]);
    expect(row.failuresTotal).toBe(3);
    expect(row.failuresDropped).toBe(4);
  });

  it('parked empty conversations and the streak are carried through', () => {
    const row = rowOf(input({ scopes: [scope({ header: header({ parkedEmpty: ['x'], emptyStreak: 2 }) })] }));
    expect(row.parkedEmpty).toBe(1);
    expect(row.emptyStreak).toBe(2);
  });
});

describe('W113 · item 4 — the state, and why', () => {
  const halt: HaltRecord = { reason: 'shape-changed', detail: 'list response has no `items` array', at: 1 };

  it('🔴 the order settles which state is reported when several could apply', () => {
    const allWrong = { halted: halt, registered: false, pendingCount: 5 };
    // Switch off governs everything.
    expect(rowOf(input({ enabled: false, scopes: [scope({ header: header(allWrong), registered: false })] })).state).toBe('off');
    // Then the host.
    expect(rowOf(input({ hostPaused: true, scopes: [scope({ header: header(allWrong), registered: false })] })).state).toBe('host-paused');
    // Then a stored stop — before "nothing is owed", so a halted scope that happens to owe nothing is not
    // reported as done.
    expect(rowOf(input({ scopes: [scope({ header: header(allWrong), registered: false })] })).state).toBe('halted');
  });

  it('a header the registry does not name is a leftover, not progress', () => {
    expect(rowOf(input({ scopes: [scope({ registered: false })] })).state).toBe('unregistered');
  });

  it('🔴 W113b · …and that does not depend on owing nothing: an unregistered scope that owes is not "in progress"', () => {
    // Before W113b the unregistered test also required `pending.length === 0`, so a leftover that still owed
    // work fell through to `in-progress` — "there is work and nothing is stopping it" — and `speedNote` then
    // published a live ETA for a leg with no target in the registry. Measured here on the sentence itself,
    // because the state alone would not catch a row that is `unregistered` and still quotes an estimate.
    const owed = { pending: ['p1', 'p2'], archived: [], times: new Map() };
    const row = rowOf(input({
      scopes: [scope({ registered: false, debt: owed, header: header({ pendingCount: 2 }) })],
    }));
    expect(row.state).toBe('unregistered');
    // The owed count is still reported — §3 asks for it, and "this cannot run" is not a reason to hide it.
    expect(row.pending).toBe(2);
    const note = speedNote(row, NOW);
    expect(note).not.toContain('estimate: about');
    expect(note).toContain('nothing runs for this platform here');
    // W149 · the short ETA line on the card face says the same thing: no estimate, and why.
    const card = cardOf(input({
      scopes: [scope({ registered: false, debt: owed, header: header({ pendingCount: 2 }) })],
    }));
    expect(card.eta).toContain('nothing runs for this platform here');
    expect(card.eta).toContain('2');
  });

  it('W113b · a waiting row prints its reason once, not twice', () => {
    // A guard, not the proof of a fix: the branch this replaced was unreachable (`stateOf` returns `waiting`
    // only when `skippedReason` is non-null, and the state sentence already renders `describeSkipReason`). It is
    // here so that a future second copy of the sentence is caught by an assertion rather than by reading.
    const view = coverageView(buildCoverage(input({
      scopes: [scope({ debt: { pending: ['p1'], archived: [], times: new Map() }, skippedReason: 'no-http-port' })],
    })), NOW);
    const texts = pageTexts(view);
    const why = describeSkipReason('no-http-port');
    expect(texts.filter((text) => text.includes(why))).toHaveLength(1);
  });

  it("today's quota being spent is not a stop", () => {
    const plan = SPEED_PLANS.standard;
    const row = rowOf(input({
      presetRaw: 'standard',
      scopes: [scope({
        debt: { pending: ['p'], archived: [], times: new Map() },
        header: header({ detailToday: { day: '2026-09-24', count: plan.pace.detail.maxPerDay ?? 0, cap: plan.pace.detail.maxPerDay ?? 0 } }),
      })],
    }));
    expect(row.state).toBe('capped');
    expect(stateNote(row, NOW)).toContain(String(plan.pace.detail.maxPerDay));
  });

  it('a wake that passed this platform over is reported as waiting, with the wake\'s own reason', () => {
    const row = rowOf(input({
      scopes: [scope({ debt: { pending: ['p'], archived: [], times: new Map() }, skippedReason: 'no-http-port' })],
    }));
    expect(row.state).toBe('waiting');
    expect(stateNote(row, NOW)).toContain('no page of this platform was open');
  });

  it('a stop is worded by the popup\'s own sentence for that record', () => {
    const row = rowOf(input({ scopes: [scope({ header: header({ halted: halt }) })] }));
    const note = stateNote(row, NOW);
    // The one halt sentence the popup already shows, reused rather than rewritten — it carries the detail
    // so the reader can act on it.
    expect(note).toContain('shape-changed');
  });

  it('a transient stop says when it retries, and a permanent one does not', () => {
    const transient: HaltRecord = { reason: 'rate-limited', detail: '429', at: 1, retryAt: NOW + 5 * 60_000, attempts: 1 };
    const waiting = rowOf(input({ scopes: [scope({ header: header({ halted: transient }) })] }));
    expect(waiting.state).toBe('halted');
    expect(stateNote(waiting, NOW)).toMatch(/min/);
  });
});

describe('W113 · item 5 — speed and ETA', () => {
  it('the day cap is the drawn one, clamped by the preset', () => {
    const row = rowOf(input({
      presetRaw: 'standard',
      scopes: [scope({ header: header({ detailToday: { day: '2026-09-24', count: 10, cap: 350 } }) })],
    }));
    expect(row.speed.dayCap).toBe(350);
    expect(row.speed.tickDetails).toBe(SPEED_PLANS.standard.tickDetails);
  });

  it('🔴 a cap drawn by a different preset cannot raise this one', () => {
    const row = rowOf(input({
      presetRaw: 'gentle',
      // The stored cap is standard's ceiling; gentle's plan ceiling must win.
      scopes: [scope({ header: header({ detailToday: { day: '2026-09-24', count: 1, cap: 400 } }) })],
    }));
    expect(row.speed.dayCap).toBe(SPEED_PLANS.gentle.pace.detail.maxPerDay);
  });

  it('a counter from another day is flagged rather than read as today', () => {
    const row = rowOf(input({
      scopes: [scope({ header: header({ detailToday: { day: '2026-09-20', count: 300, cap: 400 } }) })],
    }));
    expect(row.speed.counterStale).toBe(true);
    expect(row.speed.bodiesToday).toBe(0);
    expect(row.speed.measuredPerDay).toBeNull();
    // W149 · the quota meter carries the stale flag, and the page's meter draws no fill for it — the
    // line itself must not read as today's draw either.
    const card = cardOf(input({
      scopes: [scope({ header: header({ detailToday: { day: '2026-09-20', count: 300, cap: 400 } }) })],
    }));
    expect(card.quota.counterStale).toBe(true);
    expect(card.quota.line).toContain('another day');
  });

  it('🔴 a measured rate is withheld until there is enough of a day behind it', () => {
    // One body just after midnight extrapolates to a nonsense rate; the threshold exists for that.
    const early = rowOf(input({
      now: Date.UTC(2026, 8, 24, 0, 30, 0),
      scopes: [scope({ header: header({ detailToday: { day: '2026-09-24', count: 1, cap: 400 } }) })],
    }));
    expect(early.speed.measuredPerDay).toBeNull();
    expect(speedNote(early, Date.UTC(2026, 8, 24, 0, 30, 0))).toContain('not enough of a day');
  });

  it('🔴 an ETA is always labelled an estimate, and names the rate it used', () => {
    const row = rowOf(input({
      presetRaw: 'standard',
      scopes: [scope({
        debt: { pending: Array.from({ length: 700 }, (_, i) => `p${i}`), archived: [], times: new Map() },
        header: header({ detailToday: { day: '2026-09-24', count: 0, cap: 400 } }),
      })],
    }));
    expect(row.speed.etaBasis).toBe('cap');
    const note = speedNote(row, NOW);
    expect(note).toContain('estimate');
    expect(note).toContain('daily limit');
    // 700 owed at 400/day is under two days, so the sentence carries one decimal.
    expect(row.speed.etaDays).toBeCloseTo(1.75, 2);
  });

  it('a measured rate is preferred over the cap when one is available', () => {
    const row = rowOf(input({
      presetRaw: 'standard',
      scopes: [scope({
        debt: { pending: Array.from({ length: 100 }, (_, i) => `p${i}`), archived: [], times: new Map() },
        // 12:00 UTC, 60 bodies: 120/day measured, below the 400 cap.
        header: header({ detailToday: { day: '2026-09-24', count: 60, cap: 400 } }),
      })],
    }));
    expect(row.speed.measuredPerDay).toBeCloseTo(120, 0);
    expect(row.speed.etaBasis).toBe('measured');
    expect(row.speed.etaDays).toBeCloseTo(100 / 120, 2);
  });

  it('nothing owed means no ETA to give, and the sentence says so', () => {
    const row = rowOf(input({
      presetRaw: 'standard',
      scopes: [scope({ header: header({ detailToday: { day: '2026-09-24', count: 0, cap: 400 } }) })],
    }));
    expect(row.speed.etaDays).toBeNull();
    expect(speedNote(row, NOW)).toContain('nothing is owed');
  });

  it('🔴 W149 · the short ETA line is labelled an estimate in every branch that can appear on a card', () => {
    // Cap basis: pending 700, cap 400/day, no measured rate yet.
    const capBasis = cardOf(input({
      presetRaw: 'standard',
      scopes: [scope({
        debt: { pending: Array.from({ length: 700 }, (_, i) => `p${i}`), archived: [], times: new Map() },
        header: header({ detailToday: { day: '2026-09-24', count: 0, cap: 400 } }),
      })],
    }));
    expect(capBasis.eta).toContain('estimate');
    expect(capBasis.eta).toContain('daily limit');
    expect(capBasis.eta).toContain('700');
    // Measured basis: pending 100, 120/day measured.
    const measured = cardOf(input({
      presetRaw: 'standard',
      scopes: [scope({
        debt: { pending: Array.from({ length: 100 }, (_, i) => `p${i}`), archived: [], times: new Map() },
        header: header({ detailToday: { day: '2026-09-24', count: 60, cap: 400 } }),
      })],
    }));
    expect(measured.eta).toContain('estimate');
    expect(measured.eta).toContain('rate measured today');
    // Nothing owed: the sentence says so, and is still the labelled branch.
    const none = cardOf(input({
      presetRaw: 'standard',
      scopes: [scope({ header: header({ detailToday: { day: '2026-09-24', count: 0, cap: 400 } }) })],
    }));
    expect(none.eta).toContain('nothing is owed');
    expect(none.eta).toContain('estimate');
  });
});

describe('W113 · item 6 — when the conversations are from', () => {
  const times = new Map([
    ['a1', { at: Date.UTC(2026, 6, 4), from: 'list-update' as const }],
    ['a2', { at: Date.UTC(2026, 6, 20), from: 'list-update' as const }],
    ['a3', { at: Date.UTC(2026, 8, 1), from: 'list-update' as const }],
  ]);

  it('stored and owed are bucketed by month, separately', () => {
    const row = rowOf(input({
      scopes: [scope({ debt: { pending: ['a3', 'p-unknown'], archived: ['a1', 'a2'], times } })],
    }));
    expect(row.months).toEqual([
      { month: '2026-07', archived: 2, pending: 0 },
      { month: '2026-09', archived: 0, pending: 1 },
    ]);
    expect(row.timed).toEqual({ archived: 2, pending: 1 });
  });

  it('🔴 a conversation with no recorded time is never spread over a month', () => {
    const row = rowOf(input({
      scopes: [scope({ debt: { pending: ['p1', 'p2', 'a-known'], archived: [], times: new Map([['a-known', { at: Date.UTC(2026, 7, 9), from: 'list-create' as const }]]) } })],
    }));
    expect(row.months).toEqual([{ month: '2026-08', archived: 0, pending: 1 }]);
    expect(row.unknownTime).toEqual({ archived: 0, pending: 2 });
    // The total across months plus the unknown bucket is the whole set — nothing was lost and nothing was
    // invented to make the months look complete.
    const inMonths = row.months.reduce((n, m) => n + m.archived + m.pending, 0);
    expect(inMonths + row.unknownTime.archived + row.unknownTime.pending).toBe(3);
  });

  it('🔴 with no times at all the page says so instead of drawing empty months', () => {
    const row = rowOf(input({ scopes: [scope({ debt: { pending: ['p1'], archived: ['a1'], times: new Map() } })] }));
    expect(row.months).toEqual([]);
    expect(row.unknownTime).toEqual({ archived: 1, pending: 1 });
    const card = cardOf(input({
      scopes: [scope({ debt: { pending: ['p1'], archived: ['a1'], times: new Map() } })],
    }));
    expect(card.months.title).toBeNull();
    expect(card.months.noneNote).not.toBeNull();
    expect(card.months.noneNote).toContain('not an empty history');
    // …and the time-unknown sentence is still there, carrying the counts.
    expect(card.months.unknownNote).toContain(String(1));
  });

  it('the bucket month comes from the injected calendar, not from a hard-coded zone', () => {
    const utc = rowOf(input({
      monthKey: (at) => new Date(at).toISOString().slice(0, 7),
      scopes: [scope({ debt: { pending: [], archived: ['a3'], times } })],
    }));
    expect(utc.months).toEqual([{ month: '2026-09', archived: 1, pending: 0 }]);
  });

  it('W149 · the months view carries the chart\'s labels, and the unknown bucket keeps its own', () => {
    const card = cardOf(input({
      scopes: [scope({ debt: { pending: ['a3', 'p-unknown'], archived: ['a1', 'a2'], times } })],
    }));
    expect(card.months.title).not.toBeNull();
    expect(card.months.months.map((m) => m.month)).toEqual(['2026-07', '2026-09']);
    expect(card.months.unknownTime).toEqual({ archived: 0, pending: 1 });
    expect(card.months.unknownNote).toContain('time unknown');
  });
});

describe('W113 · the report as a whole', () => {
  it('groups rows by platform and sorts reproducibly', () => {
    const report = buildCoverage(input({
      scopes: [
        scope({ platform: 'grok', scope: 'default', registered: true }),
        scope({ platform: 'chatgpt', scope: 'default' }),
        scope({ platform: 'chatgpt', scope: 'org-2', header: header({ scope: 'org-2' }) }),
      ],
    }));
    expect(report.platforms.map((p) => p.platform)).toEqual(['chatgpt', 'grok']);
    expect(report.platforms[0]!.rows.map((r) => r.scope)).toEqual(['default', 'org-2']);
  });

  it('🔴 a scope containing a colon survives grouping intact', () => {
    // ADR-031's `chatgpt:<workspace>` shape: the scope is everything after the first colon.
    const report = buildCoverage(input({ scopes: [scope({ platform: 'chatgpt', scope: 'ws:1234' })] }));
    expect(report.rows[0]!.scope).toBe('ws:1234');
  });

  it('problemRows names exactly the rows that carry a defect', () => {
    const report = buildCoverage(input({
      scopes: [
        scope({ scope: 'clean' }),
        scope({ scope: 'failed', header: header({ scope: 'failed', failures: [{ shortId: 'aaaaaaaa', platform: PLATFORM, reason: 'detail-empty', at: 1 }] }) }),
        scope({ scope: 'stopped', header: header({ scope: 'stopped', halted: { reason: 'shape-changed', detail: 'x', at: 1 } }) }),
      ],
    }));
    expect(problemRows(report).map((r) => r.scope).sort()).toEqual(['failed', 'stopped']);
  });

  it('every one of the six items is emitted for every card, with nothing conditional on having something to say', () => {
    const view = coverageView(buildCoverage(input({
      scopes: [scope({ debt: { pending: ['p'], archived: ['a'], times: new Map([['a', { at: Date.UTC(2026, 5, 2), from: 'list-update' }]]) } })],
    })), NOW);
    expect(view.cards).toHaveLength(1);
    const card = view.cards[0]!;
    // 1 · the listed count, and it is marked as a lower bound because the list is not finished
    expect(card.listed).toContain('≥');
    // 2 · where the total came from — this fixture has no total, so the sentence says which number is
    //     missing rather than leaving a blank
    expect(card.detailRows.map((r) => r.value)).toContain('the platform does not provide a total for this list, so there is no denominator');
    // 3 · stored against owed. Both labels are present even though one of them is zero.
    const labels = card.detailRows.map((r) => r.label);
    expect(labels).toEqual(expect.arrayContaining(['stored in full', 'still owed']));
    expect(card.detailRows.find((r) => r.label === 'stored in full')!.value).toBe('1');
    expect(card.detailRows.find((r) => r.label === 'still owed')!.value).toBe('1');
    // 5 · the estimate, and it says so in the sentence itself
    expect(card.eta).toContain('estimate');
    // The full speed sentence too, in the details, with its cap.
    expect(card.detailRows.map((r) => r.value).some((v) => v.includes('Daily limit'))).toBe(true);
    // 6 · the monthly distribution: a month exists, and the chart's labels travel with it
    expect(card.months.months.length).toBe(1);
    expect(card.months.title).not.toBeNull();
    // 4 — the state. `in-progress` deliberately has no sentence, so a card is allowed to show none; what it
    // may not do is show a *different* state's sentence.
    expect(card.detailRows.filter((r) => r.label === 'state')).toHaveLength(0);
    expect(card.chip.tone).toBe('run');
  });
});

describe('W149 · the overview header', () => {
  const running = scope({
    debt: { pending: ['p1', 'p2'], archived: ['a1', 'a2'], times: new Map() },
    header: header({ pendingCount: 2, archivedCount: 2, enumCursor: { offset: 4, complete: false } }),
  });
  const halted = scope({
    platform: 'claude',
    scope: 'default',
    debt: { pending: ['p3'], archived: [], times: new Map() },
    header: {
      ...header({ pendingCount: 1, archivedCount: 0 }),
      platform: 'claude',
      scope: 'default',
      halted: { reason: 'shape-changed', detail: 'x', at: 1 },
    },
  });

  it('the three sums are the rows\' own numbers, nothing invented', () => {
    const view = coverageView(buildCoverage(input({
      scopes: [
        running,
        scope({ scope: 'failed', header: header({ scope: 'failed', failures: [{ shortId: 'aaaaaaaa', platform: 'chatgpt', reason: 'detail-empty', at: 1 }, { shortId: 'bbbbbbbb', platform: 'chatgpt', reason: 'detail-empty', at: 2 }] }) }),
      ],
    })), NOW);
    expect(view.stats).not.toBeNull();
    expect(view.stats!.stored).toBe(2);
    expect(view.stats!.owed).toBe(2);
    expect(view.stats!.failed).toBe(2);
    // A page with no rows has no sums to show — the empty state says the rest.
    const empty = coverageView(buildCoverage(input({ scopes: [] })), NOW);
    expect(empty.stats).toBeNull();
    expect(empty.health).toBeNull();
    expect(empty.empty).not.toBeNull();
  });

  it('🔴 the health line never softens a stop the model reported', () => {
    const view = coverageView(buildCoverage(input({ scopes: [running, halted] })), NOW);
    expect(view.health!.tone).toBe('bad');
    expect(view.health!.text).toContain('claude');
    expect(view.health!.text).toContain('stopped');
    // The halted card's alert is the model's own action sentence, and the chip states it in one word.
    const card = view.cards.find((c) => c.platform === 'claude')!;
    expect(card.chip.word).toBe('stopped');
    expect(card.alerts.some((a) => a.tone === 'bad' && a.text.includes('shape-changed'))).toBe(true);
  });

  it('waiting legs are named, with the one action that resolves them', () => {
    const waiting = scope({
      platform: 'claude',
      scope: 'default',
      debt: { pending: ['p'], archived: [], times: new Map() },
      header: { ...header({ pendingCount: 1 }), platform: 'claude', scope: 'default' },
      skippedReason: 'no-http-port',
    });
    const view = coverageView(buildCoverage(input({ scopes: [running, waiting] })), NOW);
    expect(view.health!.tone).toBe('wait');
    expect(view.health!.text).toContain('claude');
    expect(view.health!.text).toContain('open');
    // The waiting card keeps the state sentence in its details, so the chip is not the only wording.
    const card = view.cards.find((c) => c.platform === 'claude')!;
    expect(card.detailRows.some((r) => r.label === 'state' && r.value.includes('Not running right now'))).toBe(true);
  });

  it('a clean report gets the all-clear line, and nothing else', () => {
    const view = coverageView(buildCoverage(input({ scopes: [running] })), NOW);
    expect(view.health!.tone).toBe('ok');
    expect(view.health!.text).not.toContain('stopped');
    expect(view.alerts).toHaveLength(0);
  });

  it('the two global stops are page-level alerts, not card content', () => {
    const off = coverageView(buildCoverage(input({ enabled: false, scopes: [running] })), NOW);
    expect(off.alerts[0]!.id).toBe('global:off');
    expect(off.alerts[0]!.text).toContain('switch is off');
    const paused = coverageView(buildCoverage(input({ hostPaused: true, scopes: [running] })), NOW);
    expect(paused.alerts[0]!.id).toBe('global:host-paused');
    // Every card's chip shows the state too: the header and the cards may not disagree about the world.
    expect(paused.cards[0]!.chip.tone).toBe('muted');
  });
});

describe('W149 · the composition bar and the chip', () => {
  it('🔴 a contradicted total is never drawn as a remainder segment', () => {
    // The platform's total was exceeded by rows actually returned; `progress.ts` refuses it as a
    // denominator, and a bar segment made of it would draw the number the model refused.
    const card = cardOf(input({
      scopes: [scope({ header: header({ totalKnown: 901, totalSource: 'contradicted', enumCursor: { offset: 901, complete: true } }) })],
    }));
    expect(card.bar.remainder).toBe(0);
    // …and the contradicted sentence still exists in the details, words intact.
    expect(card.detailRows.map((r) => r.value)).toContain('the platform\'s own total has already been exceeded by the rows actually returned, so it is not used as a denominator');
  });

  it('a response total beyond the listing is the only source of a remainder segment', () => {
    const card = cardOf(input({
      scopes: [scope({ header: header({ totalKnown: 900, totalSource: 'response-total', enumCursor: { offset: 250, complete: false } }) })],
    }));
    expect(card.bar.remainder).toBe(650);
    expect(card.legend.some((entry) => entry.tone === 'remainder' && entry.count === 650)).toBe(true);
  });

  it('zero-count segments never reach the legend or the bar', () => {
    const card = cardOf(input({ scopes: [scope({ debt: { pending: ['p'], archived: ['a'], times: new Map() } })] }));
    expect(card.bar.failed).toBe(0);
    expect(card.bar.remainder).toBe(0);
    expect(card.legend.map((e) => e.tone)).toEqual(['ok', 'hatch']);
  });

  it('🔴 the percent ring exists exactly when the model produced a percentage', () => {
    const withPercent = cardOf(input({
      scopes: [scope({ header: header({ totalKnown: 10, totalSource: 'response-total', enumCursor: { offset: 10, complete: true } }), debt: { pending: [], archived: ['a'], times: new Map() } })],
    }));
    expect(withPercent.percent).not.toBeNull();
    expect(withPercent.percentTitle).toContain(String(withPercent.percent));
    expect(withPercent.percentNote).toBeNull();
    const without = cardOf(input({
      scopes: [scope({ header: header({ totalKnown: null, totalSource: 'unknown', enumCursor: { offset: 10, complete: true } }) })],
    }));
    expect(without.percent).toBeNull();
    expect(without.percentTitle).toBeNull();
    expect(without.percentNote).toContain('no percentage');
  });

  it('the chip is one word from the closed set, per state', () => {
    const cases: Array<{ state: string; card: ReturnType<typeof cardOf>; tone: string }> = [
      {
        state: 'in-progress',
        card: cardOf(input({ scopes: [scope({ debt: { pending: ['p'], archived: [], times: new Map() } })] })),
        tone: 'run',
      },
      {
        state: 'waiting',
        card: cardOf(input({ scopes: [scope({ debt: { pending: ['p'], archived: [], times: new Map() }, skippedReason: 'no-http-port' })] })),
        tone: 'wait',
      },
      {
        state: 'capped',
        card: cardOf(input({
          presetRaw: 'standard',
          scopes: [scope({
            debt: { pending: ['p'], archived: [], times: new Map() },
            header: header({ detailToday: { day: '2026-09-24', count: SPEED_PLANS.standard.pace.detail.maxPerDay ?? 0, cap: SPEED_PLANS.standard.pace.detail.maxPerDay ?? 0 } }),
          })],
        })),
        tone: 'wait',
      },
      {
        state: 'done',
        card: cardOf(input({ scopes: [scope({ debt: { pending: [], archived: ['a'], times: new Map() } })] })),
        tone: 'ok',
      },
      {
        state: 'halted',
        card: cardOf(input({ scopes: [scope({ header: header({ halted: { reason: 'shape-changed', detail: 'x', at: 1 } }) })] })),
        tone: 'bad',
      },
      {
        state: 'off',
        card: cardOf(input({ enabled: false })),
        tone: 'muted',
      },
    ];
    for (const { state, card, tone } of cases) {
      expect(card.chip.tone, `${state} chip tone`).toBe(tone);
      expect(card.chip.word.length, `${state} chip word is a word, not empty`).toBeGreaterThan(0);
    }
  });

  it('the monogram and hue identify a platform without any logo', () => {
    const card = cardOf(input({ scopes: [scope({ platform: 'claude', scope: 'default' })] }));
    expect(card.monogram).toBe('C');
    expect(card.hueKey).toBe('claude');
    const unknownCard = cardOf(input({ scopes: [scope({ platform: 'not-a-known-platform', scope: 'default' })] }));
    expect(unknownCard.hueKey).toBe('default');
    expect(unknownCard.monogram).toBe('N');
  });

  it('failures produce one dismissible alert with the count, and no more', () => {
    const card = cardOf(input({
      scopes: [scope({ header: header({ failures: [{ shortId: 'aaaaaaaa', platform: PLATFORM, reason: 'detail-empty', at: 1 }] }) })],
    }));
    expect(card.alerts).toHaveLength(1);
    expect(card.alerts[0]!.id).toBe('chatgpt:default:failures');
    expect(card.alerts[0]!.tone).toBe('bad');
    expect(card.alerts[0]!.text).toContain('1');
    // And the card is never [0]-length alerted for a clean row.
    const clean = cardOf(input({}));
    expect(clean.alerts).toHaveLength(0);
  });
});

describe('W113 · the popup summary card', () => {
  it('🔴 shows counts and a state word, and no percentage or estimate anywhere', () => {
    const report = buildCoverage(input({
      scopes: [scope({ debt: { pending: ['p1', 'p2'], archived: ['a1'], times: new Map() } })],
    }));
    const card = coverageCard(report, NOW);
    expect(card.lines).toHaveLength(1);
    const line = card.lines[0]!;
    const all = [line.text, line.chip.word].join('\n') + (card.note ?? '');
    expect(all).not.toContain('%');
    expect(all).not.toContain('estimate');
    expect(line.text).toContain('1');
    expect(line.text).toContain('2');
    expect(line.chip.tone).toBe('run');
    expect(line.bar).toEqual({ archived: 1, owed: 2 });
  });

  it('an install with no records says so rather than showing an empty card', () => {
    const report = buildCoverage(input({ scopes: [] }));
    expect(coverageCard(report, NOW).lines).toHaveLength(0);
    expect(coverageCard(report, NOW).note).not.toBeNull();
    expect(coverageView(report, NOW).empty).not.toBeNull();
  });
});

describe('W113 · the page offers the three presets and warns about one', () => {
  it('the risk note appears with the fast preset and with no other', () => {
    for (const preset of ['gentle', 'standard'] as const) {
      const view = coverageView(buildCoverage(input({ presetRaw: preset })), NOW);
      expect(view.speed.riskNote).toBeNull();
      expect(view.speed.current).toBe(preset);
      expect(view.speed.options.map((o) => o.preset)).toEqual(['gentle', 'standard', 'faster']);
    }
    const fast = coverageView(buildCoverage(input({ presetRaw: 'faster' })), NOW);
    // The note lives beside the control, not buried in the cards.
    expect(fast.speed.riskNote).toContain('Faster raises how much is fetched per day');
    expect(fast.cards.length).toBeGreaterThan(0);
    // Every option carries its own numbers, from the plan, never retyped.
    expect(fast.speed.options.map((o) => o.what)).toEqual(
      ['gentle', 'standard', 'faster'].map((preset) => presetWhat(preset as 'gentle')),
    );
  });
});

describe('W113 · no rendered sentence may keep a placeholder', () => {
  /**
   * 🔴 The guard for the defect the browser test found.
   *
   * A catalog sentence that names `{platform}` while the call passes only `{archived}` does not fail: the
   * substitution is simply skipped and the reader is shown the literal braces. Every unit test here was
   * green while the popup's card read `{platform} · stored 1, owed 1`. So the assertion is on the rendered
   * *output* of every sentence this feature can produce, for a fixture chosen to make every branch fire.
   */
  const UNSUBSTITUTED = /\{[a-zA-Z][a-zA-Z0-9]*\}/;

  function richReport() {
    const times = new Map([
      ['a1', { at: Date.UTC(2026, 6, 4), from: 'list-update' as const }],
    ]);
    return buildCoverage(input({
      presetRaw: 'faster',
      scopes: [
        scope({
          debt: { pending: ['p1', 'p2'], archived: ['a1'], times },
          header: header({
            enumCursor: { offset: 12, complete: true, truncated: 'has-more-missing' },
            totalKnown: 3, totalSource: 'contradicted',
            failures: [{ shortId: 'aaaaaaaa', platform: PLATFORM, reason: 'detail-empty', at: 1 }], failuresDropped: 2,
            parkedEmpty: ['p1'], emptyStreak: 1,
            halted: { reason: 'detail-empty-unverified', detail: 'body came back empty', at: 1, attempts: 2, retryAt: NOW + 60_000 },
            relisted: { at: NOW - 1000, recorded: 9, held: 4 },
          }),
        }),
        scope({ scope: 'second', registered: false, header: header({ scope: 'second' }) }),
        scope({ scope: 'third', debt: null, skippedReason: 'daily-cap', header: header({ scope: 'third' }) }),
      ],
    }));
  }

  it('the page, every sentence of it, has no literal placeholder left', () => {
    const view = coverageView(richReport(), NOW);
    for (const text of pageTexts(view)) {
      expect(text, `unsubstituted placeholder in: ${text}`).not.toMatch(UNSUBSTITUTED);
    }
  });

  it('the details and the months table carry no placeholder either', () => {
    const view = coverageView(richReport(), NOW);
    expect(view.cards.length).toBeGreaterThan(0);
    for (const card of view.cards) {
      for (const row of card.detailRows) {
        expect(row.label || 'x', `unsubstituted placeholder in: ${row.label || 'x'}`).not.toMatch(UNSUBSTITUTED);
        expect(row.value, `unsubstituted placeholder in: ${row.value}`).not.toMatch(UNSUBSTITUTED);
      }
      for (const month of card.months.months) {
        expect(month.month, `month key is data, but it may not look like a placeholder: ${month.month}`).not.toMatch(UNSUBSTITUTED);
      }
    }
  });

  it('the popup card names the platform and the scope, and has no placeholder left', () => {
    const card = coverageCard(richReport(), NOW);
    expect(card.lines).toHaveLength(3);
    for (const line of card.lines) {
      expect(line.text, line.text).not.toMatch(UNSUBSTITUTED);
      expect(line.chip.word, line.chip.word).not.toMatch(UNSUBSTITUTED);
      // The two things the browser test caught: the platform is named, and the scope distinguishes two
      // accounts of one platform.
      expect(line.text).toContain(line.platform);
      expect(line.text).toContain(line.scope);
    }
  });

  it('the speed sentence and the state sentence are substituted in every preset', () => {
    for (const presetRaw of ['gentle', 'standard', 'faster'] as const) {
      const report = buildCoverage(input({ presetRaw, scopes: [scope({ debt: { pending: ['p'], archived: [], times: new Map() } })] }));
      const row = report.rows[0]!;
      expect(speedNote(row, NOW)).not.toMatch(UNSUBSTITUTED);
      const note = stateNote(row, NOW);
      if (note !== null) expect(note).not.toMatch(UNSUBSTITUTED);
    }
  });
});

describe('W149 · every card keeps the tick\'s and the quota\'s characters intact', () => {
  it('the demoted tick note reuses the popup\'s own sentence for the last wake', () => {
    const view = coverageView(buildCoverage(input({
      tick: { at: NOW - 60_000, ran: true, reason: 'ran', targets: 2 },
    })), NOW);
    expect(view.tickNote).toContain('really ran');
    const skipped = coverageView(buildCoverage(input({
      tick: { at: NOW - 60_000, ran: false, reason: 'no-http-port', targets: 0 },
    })), NOW);
    expect(skipped.tickNote).toContain('did nothing at all');
  });

  it('a quota whose cap is the plan\'s own prints the numbers the plan chose', () => {
    const card = cardOf(input({
      presetRaw: 'standard',
      scopes: [scope({ header: header({ detailToday: { day: '2026-09-24', count: 10, cap: 350 } }) })],
    }));
    expect(card.quota.line).toContain('10');
    expect(card.quota.line).toContain('350');
  });
});
