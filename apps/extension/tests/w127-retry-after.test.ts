/**
 * W127 · The two rate-limit follow-ups from `.private/docs/28-RATE-LIMITS.md`
 * (§3.3 item 3, §5 B2/B4):
 *
 *  A · **429/503 `Retry-After` is honoured, clamped to [30 s, 15 min], and the
 *      ladder stays the fallback** for an absent, empty or unparseable header.
 *  B · **A list-only plan does not burst above the detail-plan rhythm.** Eight
 *      pages at the enumeration pace (`LIST_PAGES_PER_TICK`, 2-6 s apart) were
 *      about 10-30 req/min — the one place where this leg was *less* gentle than
 *      the nearest reference implementation. A list-only plan now reads its pages
 *      on the detail segment's 20-45 s rhythm (≈1.3-3 req/min).
 *
 * 🔴 Everything here is synthetic: fixture ids, fixture responses, an injected
 *    clock, an injected build id. No network, no real account, no conversation text.
 */

import { describe, it, expect } from 'vitest';

import {
  LIST_PAGES_PER_TICK,
  runBackfill,
  type HttpResponse,
  type HttpPort,
} from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  RETRY_AFTER_MAX_MS,
  RETRY_AFTER_MIN_MS,
  TRANSIENT_RETRY_BASE_MS,
  parseRetryAfterMs,
} from '../lib/backfill/types';
import {
  DEFAULT_LIST_ONLY_ENUM_PACE,
  DEFAULT_DETAIL_PACE,
  DEFAULT_ENUM_PACE,
  type Clock,
} from '../lib/backfill/pace';
import { TEST_BUILD_ID } from './i18n-harness';
import {
  backfillPlanFor,
  type BackfillEnumPlan,
  parsePerplexityListPage,
  PERPLEXITY_LIST_PATH,
} from '../lib/backfill/enumerate';

const ORIGIN = 'https://chatgpt.com';
const PPLX_ORIGIN = 'https://www.perplexity.ai';
const T0 = Date.parse('2026-09-25T12:00:00.000Z');

/** A clock whose time only moves when a test says so. `sleep` advances it, so the Pacers stay virtual. */
function stepClock(start: number): Clock & { at: (ms: number) => void } {
  let t = start;
  return {
    now: () => t,
    async sleep(ms: number) {
      t += ms;
    },
    at: (ms: number) => {
      t = ms;
    },
  };
}

function ids(n: number, from = 0): string[] {
  return Array.from({ length: n }, (_, i) => `conv-${String(i + from).padStart(4, '0')}-aaaaaaaa`);
}

function listBody(all: string[]): string {
  return JSON.stringify({
    items: all.map((id) => ({ id, title: 'synthetic-fixture', create_time: 0 })),
    limit: 100,
    offset: 0,
    total: all.length,
  });
}

function opts(store: ReturnType<typeof memoryStore>, http: HttpPort, clock: Clock) {
  return {
    platform: 'chatgpt',
    origin: ORIGIN,
    store,
    http,
    clock,
    build: TEST_BUILD_ID,
    pace: { enumerate: DEFAULT_ENUM_PACE, detail: { ...DEFAULT_DETAIL_PACE, minIntervalMs: 0 } },
  } as const;
}

// ===========================================================================
// A · the parser: the band, and the three inputs that mean "the ladder decides"
// ===========================================================================

describe('W127-A · parseRetryAfterMs is one place, and its three answers are distinct', () => {
  it('🔴 clamps delta-seconds into [30 s, 15 min] on both sides', () => {
    // A value in the band is taken as-is.
    expect(parseRetryAfterMs('120')).toBe(120_000);
    // Below the floor clamps up: "0" is "retry now", and retrying immediately is
    // exactly what a backoff exists to prevent.
    expect(parseRetryAfterMs('0')).toBe(RETRY_AFTER_MIN_MS);
    expect(parseRetryAfterMs('5')).toBe(RETRY_AFTER_MIN_MS);
    expect(RETRY_AFTER_MIN_MS).toBe(30_000);
    // Above the ceiling clamps down to the ladder's own base: a broken header
    // cannot pin the leg longer than the schedule it already chose.
    expect(parseRetryAfterMs('999999')).toBe(RETRY_AFTER_MAX_MS);
    expect(RETRY_AFTER_MAX_MS).toBe(TRANSIENT_RETRY_BASE_MS['rate-limited']);
    expect(RETRY_AFTER_MAX_MS).toBe(15 * 60_000);
  });

  it('🔴 absent, empty and garbage are all null — not zero, which would mean "now"', () => {
    expect(parseRetryAfterMs(undefined)).toBeNull();
    expect(parseRetryAfterMs(null)).toBeNull();
    expect(parseRetryAfterMs('')).toBeNull();
    expect(parseRetryAfterMs('   ')).toBeNull();
    expect(parseRetryAfterMs('soon')).toBeNull();
    expect(parseRetryAfterMs('-30')).toBeNull();
    expect(parseRetryAfterMs('12.5')).toBeNull();
    expect(parseRetryAfterMs('30s')).toBeNull();
  });

  it('reads the HTTP-date form too, and a past date clamps to the floor', () => {
    const now = T0;
    expect(parseRetryAfterMs(new Date(now + 90_000).toUTCString(), now)).toBe(90_000);
    // A date in the past says "retry now"; the floor is what stops that from being
    // an immediate retry.
    expect(parseRetryAfterMs(new Date(now - 90_000).toUTCString(), now)).toBe(RETRY_AFTER_MIN_MS);
    // A date far in the future clamps down like an over-large number.
    expect(parseRetryAfterMs(new Date(now + 86_400_000).toUTCString(), now)).toBe(RETRY_AFTER_MAX_MS);
  });
});

// ===========================================================================
// B · W127b P2 · only a strict HTTP-date is a date; what Date.parse tolerates is not
// ===========================================================================

describe('W127b-P2 · the date form is validated before Date.parse sees it', () => {
  it('🔴 a garbage header Date.parse would read as a date is null, not the floor', () => {
    const now = T0;
    // This is the defect: `Date.parse('abc 2026-01-01')` is a real timestamp, so the
    // old letter test let it through and the 30 s floor replaced the fallback ladder.
    expect(Date.parse('abc 2026-01-01')).toBe(Date.parse('2026-01-01'));
    expect(parseRetryAfterMs('abc 2026-01-01', now)).toBeNull();
  });

  it('🔴 ISO 8601 and other lenient shapes are not HTTP-dates', () => {
    const now = T0;
    for (const value of [
      '2026-01-01T00:00:00Z',   // ISO 8601, not RFC 9110
      'Thu, 01 Jan 2026',       // a date with no time-of-day
      'Jan 1 2026',
      '01/01/2026',
      'abc 2026-01-01',
    ]) {
      expect(parseRetryAfterMs(value, now), value).toBeNull();
    }
  });

  it('🔴 all three RFC 9110 forms are still read', () => {
    // Five minutes after this instant, so each form lands inside the band.
    const now = Date.parse('2026-01-06T00:00:00.000Z');
    // IMF-fixdate (preferred).
    expect(parseRetryAfterMs('Tue, 06 Jan 2026 00:05:00 GMT', now)).toBe(300_000);
    // rfc850-date (obsolete) — a two-digit year.
    expect(parseRetryAfterMs('Tuesday, 06-Jan-26 00:05:00 GMT', now)).toBe(300_000);
    // asctime-date (obsolete) — a space-padded day.
    expect(parseRetryAfterMs('Tue Jan  6 00:05:00 2026', now)).toBe(300_000);
  });
});

// ===========================================================================
// A · the engine: the status decides whether the header is read at all
// ===========================================================================

/** A backend whose **first list request** answers with a given status and `Retry-After`. */
function rateLimitedList(status: number, retryAfter?: string): { http: HttpPort; calls: string[] } {
  const calls: string[] = [];
  const http: HttpPort = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    return retryAfter === undefined
      ? { status, text: '' }
      : { status, text: '', retryAfter };
  };
  return { calls, http };
}

/** A backend whose **detail request** answers 429 with a header, after one list page. */
function rateLimitedDetail(status: number, retryAfter?: string): { http: HttpPort; calls: string[] } {
  const calls: string[] = [];
  const all = ids(1);
  const http: HttpPort = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    if (url.includes('/backend-api/conversations')) return { status: 200, text: listBody(all) };
    return retryAfter === undefined
      ? { status, text: '' }
      : { status, text: '', retryAfter };
  };
  return { http, calls };
}

describe('W127-A · a 429 with Retry-After replaces the ladder delay, a bare one does not', () => {
  it('🔴 429 + Retry-After honours the platform clock (inside the band)', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const be = rateLimitedList(429, '120');
    const r = await runBackfill({ ...opts(store, be.http, clock), scope: 'w127-ra-429', random: () => 0 });

    expect(r.stopped).toBe('waiting-retry');
    expect(r.halted?.reason).toBe('rate-limited');
    expect(r.halted?.retryAt).toBe(r.halted!.at + 120_000);
    // The header does not replace the streak: the next failure continues the ladder.
    expect(r.halted?.attempts).toBe(1);
  });

  it('🔴 429 + a below-floor header clamps up to 30 s', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const be = rateLimitedList(429, '1');
    const r = await runBackfill({ ...opts(store, be.http, clock), scope: 'w127-ra-floor', random: () => 0 });
    expect(r.halted?.reason).toBe('rate-limited');
    expect(r.halted?.retryAt).toBe(r.halted!.at + RETRY_AFTER_MIN_MS);
  });

  it('🔴 429 + an over-ceiling header clamps down to the ladder base (15 min)', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const be = rateLimitedList(429, '86400');
    const r = await runBackfill({ ...opts(store, be.http, clock), scope: 'w127-ra-ceil', random: () => 0 });
    expect(r.halted?.retryAt).toBe(r.halted!.at + RETRY_AFTER_MAX_MS);
  });

  it('🔴 429 with no header, empty header and garbage all fall back to the ladder', async () => {
    // `random: () => 0` is the floor of the ladder's own jitter band, so the fallback
    // is a value the honoured header could never produce (450 000 = 0.5 × 900 000).
    for (const [label, header] of [['absent', undefined], ['empty', ''], ['garbage', 'later-ish']] as const) {
      const store = memoryStore();
      const clock = stepClock(T0);
      const be = header === undefined ? rateLimitedList(429) : rateLimitedList(429, header);
      const r = await runBackfill({ ...opts(store, be.http, clock), scope: `w127-ra-${label}`, random: () => 0 });
      expect(r.halted?.reason, label).toBe('rate-limited');
      expect(r.halted?.retryAt, label).toBe(r.halted!.at + 450_000);
    }
  });

  it('🔴 503 is read the same way; 403 and a non-429/503 status are not', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const s503 = rateLimitedList(503, '45');
    const r503 = await runBackfill({ ...opts(store, s503.http, clock), scope: 'w127-ra-503', random: () => 0 });
    expect(r503.halted?.reason).toBe('rate-limited');
    expect(r503.halted?.retryAt).toBe(r503.halted!.at + 45_000);

    // 403 is classified `rate-limited` but is not a status Retry-After is defined
    // for, so its header is ignored and the ladder decides.
    const store2 = memoryStore();
    const clock2 = stepClock(T0);
    const s403 = rateLimitedList(403, '45');
    const r403 = await runBackfill({ ...opts(store2, s403.http, clock2), scope: 'w127-ra-403', random: () => 0 });
    expect(r403.halted?.reason).toBe('rate-limited');
    expect(r403.halted?.retryAt).toBe(r403.halted!.at + 450_000);
  });

  it('🔴 the body segment honours it too, not just the list', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const be = rateLimitedDetail(429, '120');
    const r = await runBackfill({ ...opts(store, be.http, clock), scope: 'w127-ra-detail', random: () => 0, maxDetails: 1 });
    expect(r.halted?.reason).toBe('rate-limited');
    expect(r.halted?.retryAt).toBe(r.halted!.at + 120_000);
  });
});

// ===========================================================================
// B · the list-only burst
// ===========================================================================

/**
 * 🔴 W84 · The synthetic list-only plan these tests need: a real list segment and
 * no body segment, which is the plan whose enumeration is its whole leg. (The real
 * Perplexity plan grew a body segment in W84.)
 */
const PPLX_SYNTHETIC_LIST_ONLY_PLAN: BackfillEnumPlan = {
  platform: 'perplexity',
  listPath: PERPLEXITY_LIST_PATH,
  listUrl: (origin) => `${origin}${PERPLEXITY_LIST_PATH}?version=2.18&source=default`,
  listPost: {
    contentType: 'application/json',
    bodyKeys: ['limit', 'offset', 'ascending', 'search_term'],
    body: (_origin, offset, limit) => JSON.stringify({ limit, offset, ascending: false, search_term: '' }),
  },
  parseListPage: parsePerplexityListPage,
  detailPath: null,
  detailUrl: null,
  provenance: 'synthetic list-only plan: keeps the W127 list-only pacing path covered',
};

function plansWithListOnlyPplx(platform: string): BackfillEnumPlan | null {
  return platform === 'perplexity' ? PPLX_SYNTHETIC_LIST_ONLY_PLAN : backfillPlanFor(platform);
}

function onlyPage(from: number): string {
  return JSON.stringify([
    { slug: `pplx-${String(from).padStart(4, '0')}-aaaaaaaa` },
    { slug: `pplx-${String(from + 1).padStart(4, '0')}-aaaaaaaa` },
  ]);
}

describe('W127-B · a list-only plan is paced at the detail rhythm, not the enumeration burst', () => {
  it('🔴 the list-only floor is the detail floor, and its implied peak is ≤ 4 req/min', () => {
    // Derived, not copied: "the detail-plan rhythm" cannot drift away from the
    // detail plan.
    expect(DEFAULT_LIST_ONLY_ENUM_PACE.minIntervalMs).toBe(DEFAULT_DETAIL_PACE.minIntervalMs);
    expect(DEFAULT_LIST_ONLY_ENUM_PACE.jitterMs).toBe(DEFAULT_DETAIL_PACE.jitterMs);
    // List pages are not bodies, so they are not counted against the daily cap.
    expect(DEFAULT_LIST_ONLY_ENUM_PACE.maxPerDay).toBeNull();
    // The requirement, as arithmetic: one request per floor, 60 s in a minute.
    const peakPerMinute = 60_000 / DEFAULT_LIST_ONLY_ENUM_PACE.minIntervalMs;
    expect(peakPerMinute).toBeLessThanOrEqual(4);
    // And it is strictly gentler than the old 2 s enumeration floor, which was the
    // 10-30 req/min burst.
    expect(DEFAULT_LIST_ONLY_ENUM_PACE.minIntervalMs).toBeGreaterThan(DEFAULT_ENUM_PACE.minIntervalMs);
  });

  it('🔴 one tick still reads LIST_PAGES_PER_TICK pages, but 20-45 s apart', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w127-listonly-pace';
    const pages = Array.from({ length: 12 }, (_, i) => onlyPage(i * 2 + 1));
    const calls: string[] = [];
    const http: HttpPort = async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      return { status: 200, text: pages[calls.length - 1] ?? '[]' };
    };
    // `() => 0` is the floor of the jitter band: every gap is exactly the minimum.
    const r1 = await runBackfill({
      ...opts(store, http, clock),
      platform: 'perplexity',
      origin: PPLX_ORIGIN,
      scope,
      listLimit: 2,
      plans: plansWithListOnlyPplx,
      random: () => 0,
    });

    // The per-tick cap is unchanged: this test is about the gap, not the count.
    expect(calls.length).toBe(LIST_PAGES_PER_TICK);
    expect(r1.stopped).toBe('budget-exhausted');

    /**
     * 🔴 The trace is the evidence. The first page does not wait (there is no
     * previous one); every later one pays the list-only floor, so the effective
     * interval is the detail segment's 20 s floor, not the enumeration 2 s one.
     */
    const waits = r1.paceTrace.enumerate;
    expect(waits).toHaveLength(LIST_PAGES_PER_TICK);
    expect(waits[0]).toBe(0);
    for (const wait of waits.slice(1)) {
      expect(wait).toBe(DEFAULT_LIST_ONLY_ENUM_PACE.minIntervalMs);
    }

    // And the claim the task is about, measured over the whole tick: 8 requests in
    // 140 s is 3.4 req/min, under the standard plan's derived ≈4 req/min — versus
    // the 8 requests in 16 s (30 req/min) the enumeration floor would have given.
    const spanMs = waits.reduce((a, b) => a + b, 0);
    const peakPerMinute = (waits.length / spanMs) * 60_000;
    expect(peakPerMinute).toBeLessThanOrEqual(4);

    // The cursor carried the rest for the next tick, and nothing was truncated.
    expect(r1.halted).toBeNull();
    expect(r1.state.enumCursor.offset).toBe(calls.length * 2);
  });

  it('🔴 a plan that can fetch bodies keeps the one-page enumeration rhythm', async () => {
    // The control: this plan reads one page per tick, so the list-only pace must
    // not touch it — the page still waits only the enumeration floor (2 s here,
    // because `() => 0` makes the jitter band zero too).
    const store = memoryStore();
    const clock = stepClock(T0);
    const all = ids(1);
    const calls: string[] = [];
    const http: HttpPort = async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      if (url.includes('/backend-api/conversations')) return { status: 200, text: listBody(all) };
      const id = decodeURIComponent(url.split('/backend-api/conversation/')[1]!.split('?')[0]!);
      return { status: 200, text: JSON.stringify({ title: 'synthetic-fixture', current_node: `${id}-node`, mapping: { [`${id}-node`]: { id: `${id}-node`, parent: null, children: [] } } }) };
    };
    const r1 = await runBackfill({ ...opts(store, http, clock), scope: 'w127-body-plan', random: () => 0, maxDetails: 1 });
    expect(r1.halted).toBeNull();
    expect(calls.filter((u) => u.includes('/backend-api/conversations'))).toHaveLength(1);
    expect(r1.archivedThisRun).toEqual(all);
  });
});
