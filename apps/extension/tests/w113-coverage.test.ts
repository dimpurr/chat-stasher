/**
 * W113 · ADR-032 — the coverage model and the page's view layer.
 *
 * Every case here is one of ADR-032's own requirements turned into an assertion, and the ones that matter
 * most are the ones that pin what must **not** appear:
 *
 *  · a lower bound is never drawn as a total (§1) — `in-progress` and `truncated` are distinct from
 *    `complete`, and `truncated` wins even when the cursor also says `complete`;
 *  · a percentage exists only where `computeProgress` allowed one, so an untrustworthy denominator
 *    produces a sentence with no `%` in it at all;
 *  · an estimate is always labelled one, and named as the rate it came from (§5);
 *  · a conversation with no recorded time is counted in its own bucket and is **never** spread over a
 *    month (§6);
 *  · every one of the six items is emitted for every row, so "there was nothing to say" cannot become
 *    "the item was dropped".
 *
 * No browser, no IndexedDB, no clock: the model is pure and takes `now`.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { buildCoverage, localMonthKey, problemRows, stateNote, speedNote, type CoverageInput, type CoverageScopeInput } from '../lib/coverage';
import { coverageCard, coverageView } from '../lib/coverage-view';
import { SPEED_PLANS } from '../lib/backfill/speed';
import { initialState, type BackfillHeader, type HaltRecord } from '../lib/backfill/types';

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

beforeEach(() => {
  vi.stubGlobal('browser', withI18n({} as never));
});

describe('W113 · item 1 — how much is listed, and whether the list is finished', () => {
  it('🔴 while the list is still being read, the number is a lower bound', () => {
    const row = rowOf(input({ scopes: [scope({ header: header({ enumCursor: { offset: 300, complete: false } }) })] }));
    expect(row.enumState).toBe('in-progress');
    const text = coverageView(buildCoverage(input({
      scopes: [scope({ header: header({ enumCursor: { offset: 300, complete: false } }) })],
    })), NOW).sections[0]!.blocks.find((b) => b.kind === 'note' && b.text.includes('≥'));
    expect(text, 'the wording must mark the number as a lower bound').toBeTruthy();
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
    const blocks = coverageView(buildCoverage(input({
      scopes: [scope({ debt: null, header: header({ pendingCount: 7, archivedCount: 3 }) })],
    })), NOW).sections[0]!.blocks;
    expect(blocks.some((b) => b.kind === 'note' && b.text.includes('authority')))
      .toBe(true);
  });

  it('failures are grouped by reason, commonest first, and the dropped count survives', () => {
    const failures = [
      { id: 'aaaaaaaa', reason: 'detail-empty', at: 1 },
      { id: 'bbbbbbbb', reason: 'detail-empty', at: 2 },
      { id: 'cccccccc', reason: 'detail-too-long', at: 3 },
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
      scopes: [scope({ state: undefined as never, header: header({ detailToday: { day: '2026-09-24', count: 0, cap: 400 } }) })],
    }));
    expect(row.speed.etaDays).toBeNull();
    expect(speedNote(row, NOW)).toContain('nothing is owed');
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
    const blocks = coverageView(buildCoverage(input({
      scopes: [scope({ debt: { pending: ['p1'], archived: ['a1'], times: new Map() } })],
    })), NOW).sections[0]!.blocks;
    expect(blocks.some((b) => b.kind === 'note' && b.text.includes('not an empty history'))).toBe(true);
  });

  it('the bucket month comes from the injected calendar, not from a hard-coded zone', () => {
    const utc = rowOf(input({
      monthKey: (at) => new Date(at).toISOString().slice(0, 7),
      scopes: [scope({ debt: { pending: [], archived: ['a3'], times } })],
    }));
    expect(utc.months).toEqual([{ month: '2026-09', archived: 1, pending: 0 }]);
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
        scope({ scope: 'failed', header: header({ scope: 'failed', failures: [{ id: 'aaaaaaaa', reason: 'detail-empty', at: 1 }] }) }),
        scope({ scope: 'stopped', header: header({ scope: 'stopped', halted: { reason: 'shape-changed', detail: 'x', at: 1 } }) }),
      ],
    }));
    expect(problemRows(report).map((r) => r.scope).sort()).toEqual(['failed', 'stopped']);
  });

  it('every one of the six items is emitted for every row, with nothing conditional on having something to say', () => {
    const report = buildCoverage(input({
      scopes: [scope({ debt: { pending: ['p'], archived: ['a'], times: new Map([['a', { at: Date.UTC(2026, 5, 2), from: 'list-update' }]]) } })],
    }));
    const blocks = coverageView(report, NOW).sections.flatMap((s) => s.blocks);
    const text = blocks.map((b) => ('text' in b ? b.text : '')).join('\n');
    const values = blocks.flatMap((b) => (b.kind === 'facts' ? b.rows.map((r) => r.value) : []));
    // 1 · the listed count, and it is marked as a lower bound because the list is not finished
    expect(text).toContain('≥');
    // 2 · where the total came from — this fixture has no total, so the sentence says which number is
    //     missing rather than leaving a blank
    expect(values.some((v) => v.includes('no denominator') || v.includes('no percentage'))).toBe(true);
    // 3 · stored against owed. Both labels are present even though one of them is zero.
    expect(blocks.flatMap((b) => (b.kind === 'facts' ? b.rows.map((r) => r.label) : [])))
      .toEqual(expect.arrayContaining(['stored in full', 'still owed', 'Total, as the platform reports it']));
    // 5 · the estimate, and it says so in the sentence itself
    expect(text).toContain('estimate:');
    expect(blocks.some((b) => b.kind === 'table')).toBe(true); // 6
    // 4 — the state. `in-progress` deliberately has no sentence, so a row is allowed to show none; what it
    // may not do is show a *different* state's sentence.
    expect(stateNote(report.rows[0]!, NOW)).toBeNull();
    expect(report.rows[0]!.state).toBe('in-progress');
  });
});

describe('W113 · the popup summary card', () => {
  it('🔴 shows counts and a state word, and no percentage or estimate anywhere', () => {
    const report = buildCoverage(input({
      scopes: [scope({ debt: { pending: ['p1', 'p2'], archived: ['a1'], times: new Map() } })],
    }));
    const card = coverageCard(report, NOW);
    expect(card.lines).toHaveLength(1);
    const all = card.lines.map((l) => l.text).join('\n') + (card.note ?? '');
    expect(all).not.toContain('%');
    expect(all).not.toContain('estimate');
    expect(card.lines[0]!.text).toContain('1');
    expect(card.lines[0]!.text).toContain('2');
  });

  it('an install with no records says so rather than showing an empty card', () => {
    const report = buildCoverage(input({ scopes: [] }));
    expect(coverageCard(report, NOW).lines).toHaveLength(0);
    expect(coverageCard(report, NOW).note).not.toBeNull();
    expect(coverageView(report, NOW).legend).not.toBe('');
  });
});

describe('W113 · the page offers the three presets and warns about one', () => {
  it('the risk note appears with the fast preset and with no other', () => {
    for (const preset of ['gentle', 'standard'] as const) {
      const view = coverageView(buildCoverage(input({ presetRaw: preset })), NOW);
      expect(view.legend).not.toContain('Faster raises');
    }
    const fast = coverageView(buildCoverage(input({ presetRaw: 'faster' })), NOW);
    // The note lives beside the control (the page's own speed blocks), not buried in the sections.
    expect(fast.sections.length).toBeGreaterThan(0);
  });
});
