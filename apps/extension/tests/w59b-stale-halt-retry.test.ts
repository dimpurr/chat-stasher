/**
 * W59b · **The five things an adversarial review of W59 found, pinned.**
 *
 * W59 made a permanent halt one build's judgement and gave the next build one fresh
 * attempt. The review (nm/R59-grok.log) found five places where that is not what the
 * code does. Four of them are cases here; the fifth — the identity two builds compare
 * on — is in tests/w59b-build-identity.test.ts, next to the composition it is about.
 * The guarantees the review found swapped out of three older suites are re-asserted as
 * **round trips** here and in tests/w31c-claude-org.test.ts.
 *
 *   1. **The attempt was written after the platform was asked.** A `storage.local`
 *      write that did not land (or an MV3 reclaim mid-run) left the older build's
 *      record looking untouched, the next tick re-decided, and the platform was asked
 *      again — a `GET /api/organizations` per tick on the Claude path. Here: the bound
 *      is spent *before* the question, on the header (`haltRetried`), and a stamp that
 *      cannot be written means the question is not asked at all.
 *   3. **Three tests had their subject swapped** when W59 rewrote them, and the
 *      guarantee that survived was left as a hand-seeded fixture. The tests below write
 *      the halt through production code and then run against it.
 *   4. **A halt this code cannot classify was deleted.** A record with no `reason`, a
 *      reason from a build that does not exist yet, or a non-string `build` fell
 *      through the classification switch, `haltClassOf` called it 'permanent', and the
 *      leg ran against a stop whose meaning nobody had established.
 *   5. **The one re-decision tick could read a whole list.** For a list-only plan the
 *      ordinary page cap is `Infinity`, so the tick that lifts a halt was the one tick
 *      in the product that could read 74 pages — and a `detail-unsupported` stop
 *      written part-way through that list would have cut it off at page one forever.
 *
 * 🔴 W59c · **And one case in here is now pinned the other way round.** The last case
 *    of section 5 read "a capability re-decision is not held by a spent attempt" — the
 *    exemption `haltRetrySpent` granted that class, which round 3 removed: the requests
 *    a capability record holds back are in the run that follows its expiry, so a lift
 *    without a verdict is a per-tick fetch. It now reads "a capability re-decision is
 *    held by an attempt THIS build spent", with the W44 property it was protecting —
 *    another build's attempt holds nothing — as its own case beside it. See
 *    tests/w59c-halt-retry-bounds.test.ts.
 *
 * 🔴 Everything here is synthetic: fixture ids, fixture responses, an injected clock,
 *    an injected build id. No network, no real account, no conversation text.
 */

import { describe, it, expect } from 'vitest';

import {
  loadState,
  markScopeRetried,
  recordBackfillHalt,
  runBackfill,
  type HttpResponse,
  type HttpPort,
  type RunReport,
} from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { stateKey, type BackfillHeader, type HaltReason } from '../lib/backfill/types';
import {
  backfillPlanFor,
  type BackfillEnumPlan,
  parsePerplexityListPage,
  PERPLEXITY_LIST_PATH,
} from '../lib/backfill/enumerate';

/**
 * 🔴 W84 · The synthetic list-only plan these re-decision tests need.
 *
 * W59b exercises the "list-only plan's ordinary tick is uncapped / re-decided one
 * page at a time" path, which the real Perplexity plan used to be. W84 filled its
 * body in, so the run tests inject this list-only plan (real list parser, no body
 * segment) to keep that path covered.
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
  provenance: 'synthetic list-only plan: keeps the W59b list-only re-decision path covered',
};

/** The run opts' `plans` lookup: Perplexity answers list-only, everything else real. */
function plansWithListOnlyPplx(platform: string): BackfillEnumPlan | null {
  return platform === 'perplexity' ? PPLX_SYNTHETIC_LIST_ONLY_PLAN : backfillPlanFor(platform);
}
import { DEFAULT_DETAIL_PACE, DEFAULT_ENUM_PACE, type Clock } from '../lib/backfill/pace';
import { TEST_BUILD_ID } from './i18n-harness';

const ORIGIN = 'https://chatgpt.com';
const PPLX_ORIGIN = 'https://www.perplexity.ai';
const T0 = Date.parse('2026-09-23T09:00:00.000Z');
/** A build that is not this one. */
const OLDER_BUILD = '0.1.0.9';

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

function detailBody(id: string): string {
  return JSON.stringify({
    title: 'synthetic-fixture',
    current_node: `${id}-node`,
    mapping: { [`${id}-node`]: { id: `${id}-node`, parent: null, children: [] } },
  });
}

/** A header, with whatever a test wants it to claim. */
function headerWith(platform: string, scope: string, claimed: Partial<BackfillHeader> = {}): BackfillHeader {
  return {
    v: 2,
    platform,
    scope,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 0, complete: false },
    pendingCount: 0,
    archivedCount: 0,
    detailToday: { day: '2026-09-23', count: 0 },
    halted: null,
    ...claimed,
  };
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
    random: () => 1,
  } as const;
}

/** A wire this build does not read: `{}` is not a list. Every request is counted. */
function unrecognisedList(): { http: HttpPort; calls: string[] } {
  const calls: string[] = [];
  return {
    calls,
    http: async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      return { status: 200, text: '{}' };
    },
  };
}

/** An http port that blows up: on the paths asserted below, not one request may be attempted. */
const mustNotFetch: HttpPort = async (url: string) => {
  throw new Error(`MUST NOT be called (${url})`);
};

/**
 * 🔴 The storage failure the bound exists for: a write carrying **this build's own
 *    record of what it decided** is refused. Everything else — the expiry trace, a
 *    record naming another build, the marker itself when `what` says so — lands
 *    normally, so the run reaches the platform and then cannot write down what it
 *    found. That is the shape R59 measured, and the one an MV3 reclaim produces too
 *    (the write is simply never reached).
 */
function refuseWrites(
  store: ReturnType<typeof memoryStore>,
  key: string,
  what: 'this build’s halt record' | 'the spent-attempt marker',
): void {
  const real = store.save.bind(store);
  store.save = async (k: string, v: unknown): Promise<void> => {
    if (k === key) {
      const header = v as { halted?: { build?: unknown }; haltRetried?: { build?: unknown } } | null;
      const refused = what === 'this build’s halt record'
        ? header?.halted?.build === TEST_BUILD_ID
        : header?.haltRetried?.build === TEST_BUILD_ID;
      if (refused) throw new Error('storage.local refused the write');
    }
    return real(k, v);
  };
}

// ---------------------------------------------------------------------------
// 1 · Finding 1 — the bound is spent before the platform is asked
// ---------------------------------------------------------------------------

describe('W59b-1 · a verdict that cannot be written is not a per-tick question', () => {
  it('🔴 the platform is asked once, ever, however many ticks follow', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59b-bounded';
    const key = stateKey('chatgpt', scope);
    await store.save(key, headerWith('chatgpt', scope, {
      enumCursor: { offset: 0, complete: false },
      // Another build's judgement, and the wire has not changed: the re-decision
      // reaches the same conclusion, so the run really does try to write a verdict.
      halted: { reason: 'shape-changed', at: T0 - 3_600_000, detail: 'synthetic-fixture detail', build: OLDER_BUILD },
    }));
    refuseWrites(store, key, 'this build’s halt record');

    const be = unrecognisedList();
    const run = { ...opts(store, be.http, clock), scope };

    // 🔴 Round 1 ends by throwing: the write that would have carried the verdict did
    //    not land. That is the premise, not the defect — the defect is what round 2
    //    does about it.
    const r1 = await runBackfill(run).catch((e: unknown) => e as Error);
    expect(r1, 'the premise is a run that really did reach the wire').toBeInstanceOf(Error);
    expect(be.calls.length, 'the re-decision asked the platform once').toBe(1);

    // 🔴 Round 2 is the assertion. On ecbcf2c storage held `halted: null` (the expiry
    //    write landed, the verdict write did not), so the leg finds no record, decides
    //    again, and asks the platform again — every tick, forever.
    const r2 = await runBackfill(run).catch((e: unknown) => e as Error);
    expect(be.calls.length, 'a verdict that could not be written must not become a per-tick question').toBe(1);
    expect(r2, 'round 2 must not have to throw: it writes nothing at all').not.toBeInstanceOf(Error);
    expect((r2 as RunReport).stopped).toBe('halted');
    expect((r2 as RunReport).halted?.reason).toBe('shape-changed');
    // 🔴 And the record still says what the older build said. Nothing was rewritten on
    //    its behalf: this build's judgement was never formed, and claiming it would be
    //    a fabricated observation.
    expect((r2 as RunReport).halted?.build).toBe(OLDER_BUILD);

    // ---- a third tick is the same answer, not a slow re-ask.
    const r3 = await runBackfill(run).catch((e: unknown) => e as Error);
    expect(be.calls.length).toBe(1);
    expect((r3 as RunReport).stopped).toBe('halted');
  });

  it('🔴 a stamp that cannot be written means the question is not asked at all', async () => {
    // The Claude path asks about a scope **before** any run has written a record for
    // it, so the bound cannot live inside `halted`: it is a header field, and it is
    // written before the resolver runs. The refusal here is the one the fix turns into
    // "do not ask" (background.ts's `resolveScopeForTick` returns the sentinel).
    const store = memoryStore();
    const scope = 'default';
    const key = stateKey('claude', scope);
    await store.save(key, headerWith('claude', scope));
    refuseWrites(store, key, 'the spent-attempt marker');

    expect(
      await markScopeRetried(store, { platform: 'claude', scope, build: TEST_BUILD_ID }),
      'a stamp that did not land must be reported as false, so the caller does not ask',
    ).toBe(false);
    // Nothing was half-written either: a refused stamp leaves the header as it was.
    expect((await store.load(key) as BackfillHeader).haltRetried).toBeUndefined();

    // ---- and when it does land, the marker names this build and no other.
    const letting = memoryStore();
    await letting.save(key, headerWith('claude', scope));
    expect(await markScopeRetried(letting, { platform: 'claude', scope, build: TEST_BUILD_ID })).toBe(true);
    expect((await letting.load(key) as BackfillHeader).haltRetried).toMatchObject({ build: TEST_BUILD_ID });

    // A build that cannot name itself has nothing to spend, and must not refuse the
    // question it cannot bound: an environment without a manifest keeps the pre-W59b
    // behaviour rather than gaining a new refusal.
    expect(await markScopeRetried(letting, { platform: 'claude', scope, build: null })).toBe(true);
    expect((await letting.load(key) as BackfillHeader).haltRetried?.build).toBe(TEST_BUILD_ID);
  });
});

// ---------------------------------------------------------------------------
// 3 · Finding 3 — the guarantees the rewritten tests used to pin
// ---------------------------------------------------------------------------

describe('W59b-3 · a halt this build wrote refuses a fetch on the next run', () => {
  /**
   * 🔴 **Round trips, not hand-seeded records.** W59's rewrite of W44-2 replaced
   *    "seed a record and require `mustNotFetch`" with "seed a record and require the
   *    leg to run", and the surviving half of the old guarantee was left as a fixture
   *    the test wrote itself (`build: TEST_BUILD_ID` pasted into a header). That
   *    asserts the *reader*; nothing asserted that the stamp the engine writes is the
   *    stamp the engine recognises — which is exactly the property a wrong identity
   *    (finding 2) breaks silently.
   *
   *    So these cases run the real path twice: run 1 writes the stop, run 2 meets it.
   */
  it('🔴 a shape-changed stop written by this build is not re-decided by this build', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59b-roundtrip-shape';
    const be = unrecognisedList();
    const run = { ...opts(store, be.http, clock), scope };

    const r1 = await runBackfill(run);
    expect(r1.halted?.reason).toBe('shape-changed');
    // The stamp the engine wrote is the running build's, from the one place that
    // composes it.
    expect(r1.halted?.build).toBe(TEST_BUILD_ID);

    // ---- the second run: the wire is asked nothing at all.
    const after = be.calls.length;
    const r2 = await runBackfill({ ...opts(store, mustNotFetch, clock), scope });
    expect(r2.stopped).toBe('halted');
    expect(r2.halted?.reason).toBe('shape-changed');
    expect(r2.state.haltExpired, 'nothing expired: this is this build’s own judgement').toBeUndefined();
    expect(be.calls.length, 'a record this build wrote must issue nothing').toBe(after);
  });

  /**
   * The two organization halts, written through the **production writer**
   * (`recordBackfillHalt` — the only thing background.ts's resolver path uses) rather
   * than pasted into a header. These are the records whose write-back the review found
   * untested: w31c's case 4 hand-stamps a record the build never wrote.
   */
  for (const reason of ['org-unresolved', 'org-ambiguous'] as const) {
    it(`🔴 a ${reason} refusal written by this build stops re-deciding itself`, async () => {
      const store = memoryStore();
      const scope = `w59b-roundtrip-${reason}`;
      expect(await recordBackfillHalt(store, {
        platform: 'claude', scope, reason, detail: 'synthetic-fixture detail',
      })).toBe(true);

      const written = await store.load(stateKey('claude', scope)) as BackfillHeader;
      expect(written.halted?.build, 'the write-back stamps the build that made the refusal').toBe(TEST_BUILD_ID);
      expect(written.haltRetried, 'and it clears any attempt that was spent on the record it replaced').toBeUndefined();

      // ---- the engine meets it on the next run and refuses, issuing nothing.
      const r = await runBackfill({
        ...opts(store, mustNotFetch, stepClock(T0)),
        platform: 'claude',
        origin: 'https://claude.ai',
        scope,
      });
      expect(r.stopped).toBe('halted');
      expect(r.halted?.reason).toBe(reason);
      expect(r.state.haltExpired).toBeUndefined();
    });
  }

  it('🔴 an unreadable record still refuses the run, and is left exactly where it was', async () => {
    // The oldest guarantee of the three, and the one no rewrite may touch: a record
    // this build cannot read stops the leg and is never overwritten.
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59b-unreadable';
    const key = stateKey('chatgpt', scope);
    const unreadable = { v: 1, platform: 'chatgpt', scope, halted: null };
    await store.save(key, unreadable);

    const r = await runBackfill({ ...opts(store, mustNotFetch, clock), scope });
    expect(r.stopped).toBe('halted');
    expect(r.halted?.reason).toBe('state-unreadable');
    expect(await store.load(key), 'an unreadable record is not written over').toEqual(unreadable);
  });
});

// ---------------------------------------------------------------------------
// 4 · Finding 4 — an unclassifiable halt is not a halt that expired
// ---------------------------------------------------------------------------

describe('W59b-4 · a record this code cannot classify stays in force', () => {
  /**
   * 🔴 Three ways a stored record stops being readable, and the same answer to all
   *    three: hold, and say nothing about expiry. Before this, every one of them fell
   *    through the classification switch, `haltClassOf` called the unknown string
   *    'permanent', and the record was **deleted** and the leg ran.
   */
  const unclassifiable: Array<{ what: string; slug: string; halted: Record<string, unknown> }> = [
    {
      what: 'no reason at all',
      slug: 'no-reason',
      halted: { at: T0 - 3_600_000, detail: 'synthetic: a record with nothing to classify' },
    },
    {
      what: 'a reason from a build that does not exist yet',
      slug: 'unknown-reason',
      halted: { reason: 'wire-rotated', at: T0 - 3_600_000, detail: 'synthetic: unknown to this build' },
    },
    {
      what: 'a build stamp that is not a string',
      slug: 'non-string-build',
      halted: { reason: 'shape-changed', build: 42, at: T0 - 3_600_000, detail: 'synthetic: a number where a build goes' },
    },
  ];

  for (const { what, slug, halted } of unclassifiable) {
    it(`🔴 ${what} is held, not expired, and the leg issues nothing`, async () => {
      const store = memoryStore();
      const clock = stepClock(T0);
      const scope = `w59b-unclassifiable-${slug}`;
      await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
        enumCursor: { offset: 0, complete: false },
        halted: halted as unknown as BackfillHeader['halted'],
      }));

      const run = await runBackfill({ ...opts(store, mustNotFetch, clock), scope });

      expect(run.stopped, 'an unreadable record is a stop, not a licence to run').toBe('halted');
      expect(run.state.haltExpired, 'nothing expired: it was never read as a judgement').toBeUndefined();
      // 🔴 The record is reported as what it is — unknown — rather than rounded into
      //    "expired" or into a reason this build invented.
      expect(run.halted?.reason).toBe(halted.reason);
      expect(run.halted?.detail).toBe(halted.detail);
      const persisted = await store.load(stateKey('chatgpt', scope)) as BackfillHeader;
      expect(persisted.halted).toMatchObject({ detail: halted.detail });
      expect(persisted.haltExpired).toBeUndefined();
    });
  }

  it('🔴 and a well-formed legacy record — no build field — still gets its one retry', async () => {
    // The positive half, asserted in the same file so the guard above cannot be read
    // as "stop re-deciding anything that looks odd": a record that merely predates the
    // field is classifiable, and the retry the task exists for still happens.
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59b-legacy-still-retried';
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      enumCursor: { offset: 0, complete: false },
      halted: { reason: 'shape-changed', at: T0 - 3_600_000, detail: 'synthetic-fixture detail' },
    }));

    const be = unrecognisedList();
    const r1 = await runBackfill({ ...opts(store, be.http, clock), scope });
    expect(be.calls.length, 'a record with no build field is another build’s').toBeGreaterThan(0);
    expect(r1.state.haltExpired).toMatchObject({ because: 'build', build: 'unstamped', currentBuild: TEST_BUILD_ID });
    expect(r1.halted?.build).toBe(TEST_BUILD_ID);
  });
});

// ---------------------------------------------------------------------------
// 5 · Finding 5 — the one re-decision tick is one page, and never a truncation
// ---------------------------------------------------------------------------

describe('W59b-5 · the tick that lifts a halt reads one page, whatever the plan allows', () => {
  /** Perplexity: a real list segment, no body segment. The plan whose ordinary tick is uncapped. */
  function perplexity(store: ReturnType<typeof memoryStore>, clock: Clock, scope: string) {
    const calls: string[] = [];
    const http: HttpPort = async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      return {
        status: 200,
        // Two rows on the first page, nothing on the second: the list is read to its
        // own end, which for this platform is the empty page (there is no has_more).
        text: calls.length === 1
          ? JSON.stringify([{ slug: 'pplx-0001-aaaaaaaa' }, { slug: 'pplx-0002-aaaaaaaa' }])
          : '[]',
      };
    };
    return {
      calls,
      run: {
        ...opts(store, http, clock),
        platform: 'perplexity',
        origin: PPLX_ORIGIN,
        scope,
        listLimit: 2,
        // 🔴 W84 · the synthetic list-only plan, so this stays a list-only test.
        plans: plansWithListOnlyPplx,
      },
    };
  }

  it('🔴 a stale stop on a list-only plan is re-decided one page at a time', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59b-listonly-gentle';
    await store.save(stateKey('perplexity', scope), headerWith('perplexity', scope, {
      enumCursor: { offset: 0, complete: false },
      // Another build's permanent, non-capability judgement. On ecbcf2c this record
      // expires and the run reads the WHOLE list in one tick — the burst.
      halted: { reason: 'shape-changed', at: T0 - 3_600_000, detail: 'synthetic-fixture detail', build: OLDER_BUILD },
    }));

    const { calls, run } = perplexity(store, clock, scope);

    const r1 = await runBackfill(run);
    expect(calls.length, 'one page, not the whole list').toBe(1);
    expect(r1.state.enumCursor.offset).toBe(2);
    expect(r1.state.enumCursor.complete).toBe(false);
    /**
     * 🔴 And it does **not** write the stop. This is the second half of the fix and
     *    the one that is easy to miss: `detail-unsupported` is raised after the list
     *    segment, so a capped tick would reach it with the enumeration half-read —
     *    and because that record is written back stamped with this build, its
     *    capability matches from the next tick on and page 2 would never be read. The
     *    tick ends on its budget instead, which is exactly what happened to it.
     */
    expect(r1.stopped).toBe('budget-exhausted');
    expect(r1.halted, 'a list-only plan must not be halted mid-list').toBeNull();

    // ---- the ordinary tick that follows: no longer a re-decision, so uncapped. The
    //      list reaches its end and the same stop is reached with it all on disk.
    const r2 = await runBackfill(run);
    expect(calls.length, 'the rest of the list, read by the tick that could read it').toBe(2);
    expect(r2.halted?.reason).toBe('detail-unsupported');
    expect(r2.halted?.capability).toBe('list-only');
    expect(r2.state.enumCursor.truncated).toBe('empty-page-inferred');

    // ---- and from here the record applies: the third tick is free.
    const after = calls.length;
    const r3 = await runBackfill({
      ...opts(store, mustNotFetch, clock),
      platform: 'perplexity',
      origin: PPLX_ORIGIN,
      scope,
      listLimit: 2,
      plans: plansWithListOnlyPplx,
    });
    expect(r3.stopped).toBe('halted');
    expect(calls.length).toBe(after);
  });

  it('🔴 a plan that can fetch bodies is unchanged by the cap', async () => {
    // The control: for every plan that is not list-only, one page per tick is what the
    // ordinary tick already did, so the re-decision changes nothing about it — and in
    // particular it must not take the list-only detour above.
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59b-capable-unchanged';
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      enumCursor: { offset: 0, complete: false },
      halted: { reason: 'shape-changed', at: T0 - 3_600_000, detail: 'synthetic-fixture detail', build: OLDER_BUILD },
    }));

    const all = ids(1);
    const calls: string[] = [];
    const http: HttpPort = async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      if (url.includes('/backend-api/conversations')) return { status: 200, text: listBody(all) };
      const id = decodeURIComponent(url.split('/backend-api/conversation/')[1]!.split('?')[0]!);
      return { status: 200, text: detailBody(id) };
    };
    const r1 = await runBackfill({ ...opts(store, http, clock), scope, maxDetails: 1 });

    expect(r1.stopped).not.toBe('budget-exhausted');
    expect(r1.halted).toBeNull();
    expect(calls.filter((u) => u.includes('/backend-api/conversations')).length, 'one list page').toBe(1);
    expect(r1.archivedThisRun).toEqual(all);
  });

  it('🔴 a capability re-decision is held by an attempt THIS build spent', async () => {
    // 🔴 W59c · **What this test used to assert, and why it is the other way round.**
    //
    // W59b exempted the capability class from the spent-attempt bound, and this case
    // pinned the exemption: a capability record plus a marker naming this build was
    // required to be re-decided anyway, on the grounds that the answer comes from the
    // plan table with no request at all. The premise was about the wrong step. The
    // requests a capability record is holding back are in the run that follows its
    // expiry — the leg now enumerates because the plan table changed — so a build that
    // lifted the record and never wrote a verdict must not lift it again on the next
    // tick, or the leg fetches on every tick until the extension is reloaded.
    //
    // The W44 property this might look like it re-freezes is not touched, and the next
    // case is where that is asserted: the marker is compared against the **running**
    // build, so a record from any other build is re-decided exactly as before.
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59b-capability-held';
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      enumCursor: { offset: 0, complete: false },
      halted: { reason: 'unsupported-platform', at: T0 - 3_600_000, detail: 'synthetic: written with no plan' },
      // The marker the expiry of this very record writes, before the run it lifted can
      // ask for anything. Present here means: that run did not get to write a verdict.
      haltRetried: { build: TEST_BUILD_ID, at: T0 - 3_600_000 },
    }));

    const r = await runBackfill({ ...opts(store, mustNotFetch, clock), scope, maxDetails: 1 });

    // This build already had its one answer here, so the record stands and nothing is
    // asked: not the platform, and not the plan table either.
    expect(r.stopped).toBe('halted');
    expect(r.state.haltExpired, 'this build has already re-decided this record once').toBeUndefined();
    const persisted = await loadState(store, 'chatgpt', scope);
    expect(persisted.halted?.reason).toBe('unsupported-platform');
    expect(persisted.haltRetried?.build, 'the attempt stays spent, so the next tick is free too').toBe(TEST_BUILD_ID);
  });

  it('🔴 and a capability record another build’s attempt was spent on is still lifted', async () => {
    // The control, and the W44 property in this file: the hold above belongs to the
    // build that spent the attempt, and to nobody else. Same fixture, marker naming a
    // different build — which is what a record written before this change looks like,
    // and what a build that is not the one running looks like.
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59b-capability-other-build-holds';
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      enumCursor: { offset: 0, complete: false },
      halted: { reason: 'unsupported-platform', at: T0 - 3_600_000, detail: 'synthetic: written with no plan' },
      haltRetried: { build: OLDER_BUILD, at: T0 - 3_600_000 },
    }));

    const all = ids(1);
    const calls: string[] = [];
    const http: HttpPort = async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      if (url.includes('/backend-api/conversations')) return { status: 200, text: listBody(all) };
      const id = decodeURIComponent(url.split('/backend-api/conversation/')[1]!.split('?')[0]!);
      return { status: 200, text: detailBody(id) };
    };
    const r = await runBackfill({ ...opts(store, http, clock), scope, maxDetails: 1 });

    expect(r.state.haltExpired, 'the plan table says this build can run').toMatchObject({ because: 'capability' });
    expect(r.halted, 'another build’s attempt must not hold this one').toBeNull();
    expect(r.archivedThisRun).toEqual(all);
  });
});

// ---------------------------------------------------------------------------
// The reason vocabulary the classification above rests on
// ---------------------------------------------------------------------------

describe('W59b · the reasons a record can carry', () => {
  it('🔴 every reason this build knows is classifiable, and an unknown one is not', async () => {
    // A tripwire rather than a behaviour: `haltSubjectOf` has no `default`, so an
    // unknown reason falls through as `undefined` at runtime while the type says
    // otherwise. `isClassifiableHalt` is the one place that notices, and this keeps
    // the list of what it must notice from shrinking.
    const { haltSubjectOf, isClassifiableHalt } = await import('../lib/backfill/types');
    const known: HaltReason[] = [
      'rate-limited', 'shape-changed', 'transport-error', 'storage-unavailable',
      'unsupported-platform', 'detail-unsupported', 'detail-empty-unverified',
      'state-unreadable', 'org-ambiguous', 'org-unresolved', 'ledger-mismatch',
    ];
    for (const reason of known) {
      expect(haltSubjectOf(reason), `${reason} must be classified`).toBeDefined();
      expect(isClassifiableHalt({ reason, at: 0, detail: '' })).toBe(true);
    }
    expect(
      haltSubjectOf('wire-rotated' as HaltReason),
      'a reason from a newer build is not a value this build can classify',
    ).toBeUndefined();
    expect(isClassifiableHalt({ reason: 'wire-rotated' as HaltReason, at: 0, detail: '' })).toBe(false);
    expect(isClassifiableHalt({ reason: 'shape-changed', build: 42, at: 0, detail: '' } as never)).toBe(false);
  });
});
