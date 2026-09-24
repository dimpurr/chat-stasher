/**
 * W44 · A halt recorded by an older build must not outlive the reason for it.
 *
 * Measured in a real logged-in Chrome on 2026-09-19: build 0.1.0.10 was installed
 * at 16:53 and found three halts sitting on live scopes, 53-61 minutes old, all
 * written between 15:56 and 16:04 — by **earlier builds**. Two of them read
 * `unsupported-platform` with a detail saying "platform claude has no backfill
 * enumeration yet" and "platform gemini has no backfill enumeration yet". Both
 * statements were false of the build that was running: `PLANS` holds all seven
 * platforms and `BACKFILL_SUPPORTED_PLATFORMS` is derived from it, so the popup
 * correctly said those platforms can backfill while the engine refused to run
 * them. Clearing one such record by hand unblocked the platform, and it came back
 * the next time an older-build judgement was re-recorded.
 *
 * The defect is that a halt record did not say **what it was a judgement about**.
 * "This platform has no enumeration yet" is a statement about the build, not
 * about the account or the platform, and when the build changes the statement is
 * no longer true — but the record had no way to notice.
 *
 * This file pins the five things that fix rests on:
 *
 *   1. **the marker** — a capability-class record says which capability it was
 *      judged against, computed from the very plan table the judgement came from,
 *      so it cannot fail to move when the capability does;
 *   2. **the expiry** — such a record stops applying when this build's capability
 *      differs, and the leg runs;
 *   3. **nothing else moves** — an account-class (`org-unresolved`,
 *      `org-ambiguous`) or upstream-class (`shape-changed`) record behaves exactly
 *      as it did, marker or no marker;
 *   4. **the migration** — an unmarked record is decided by class: a capability
 *      one is re-decided once (and re-recorded, now marked), every other one is
 *      untouched. Reading "unmarked" as "expired" globally would clear real halts;
 *      reading it as "current" leaves the measured three stuck forever;
 *   5. **the sentence** — the popup can say that a stop stopped applying because
 *      the build changed, instead of the leg silently starting again.
 *
 * 🔴 Everything here is synthetic: fixture ids, fixture list and body responses an
 *    injected clock and an injected plan lookup. No network, no real account, and
 *    no conversation text.
 */

import { describe, it, expect } from 'vitest';
import { runBackfill, loadState, type HttpResponse, type HttpPort, DETAIL_EMPTY_HALT_STREAK } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  backfillPlanFor,
  type BackfillEnumPlan,
  parsePerplexityListPage,
  PERPLEXITY_LIST_PATH,
} from '../lib/backfill/enumerate';
import { stateKey, type BackfillHeader, type HaltReason } from '../lib/backfill/types';
import { renderPopup, popupText, NO_FAILURES } from '../lib/popup-view';
import { DEFAULT_DETAIL_PACE, DEFAULT_ENUM_PACE, type Clock } from '../lib/backfill/pace';
import { TEST_BUILD_ID } from './i18n-harness';

const ORIGIN = 'https://chatgpt.com';
const PPLX_ORIGIN = 'https://www.perplexity.ai';
const CLAUDE_ORIGIN = 'https://claude.ai';
const T0 = Date.parse('2026-09-19T15:56:00.000Z');
/** A build that is not this one — the marker every W59 record carries instead of this build's. */
const OLDER_BUILD_ID = '0.1.0.9';

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

/** A ChatGPT-shaped backend. Every request is counted, and `calls` is what "issued nothing" is asserted on. */
function backend(all: string[]): { http: HttpPort; calls: string[] } {
  const calls: string[] = [];
  const http: HttpPort = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    if (url.includes('/backend-api/conversations')) return { status: 200, text: listBody(all) };
    const id = decodeURIComponent(url.split('/backend-api/conversation/')[1]!.split('?')[0]!);
    return { status: 200, text: detailBody(id) };
  };
  return { http, calls };
}

/** A header, with whatever a test wants it to claim. Written by hand: this is the record already on disk. */
function headerWith(platform: string, scope: string, claimed: Partial<BackfillHeader> = {}): BackfillHeader {
  return {
    v: 2,
    platform,
    scope,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 0, complete: true },
    pendingCount: 0,
    archivedCount: 0,
    detailToday: { day: '2026-09-19', count: 0 },
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
    pace: { enumerate: DEFAULT_ENUM_PACE, detail: { ...DEFAULT_DETAIL_PACE, minIntervalMs: 0 } },
    random: () => 1,
  } as const;
}

/**
 * A list-only plan for Perplexity.
 *
 * 🔴 W84: Perplexity's real plan now has both segments (its body was filled in from the
 * 2026-09-23 live probe), so it no longer exercises the 'list-only' capability path. That path
 * is still real code and still needs coverage, so this test injects a list-only plan to keep it
 * covered rather than pointing at a platform that is no longer in that state. Everything about
 * the injected plan is synthetic; the list parser it uses is the real one, and the http port
 * below only ever serves list pages because a list-only leg halts before any body request.
 */
const SYNTHETIC_LIST_ONLY_PLAN: BackfillEnumPlan = {
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
  provenance: 'synthetic list-only plan: keeps the W44 list-only capability path covered',
};

/** An http port that blows up: on the paths asserted below, not one request may be attempted. */
const mustNotFetch: HttpPort = async (url: string) => {
  throw new Error(`MUST NOT be called (${url})`);
};

describe('W44-1 · a capability halt stops applying once the capability exists', () => {
  it('a record written by a build with no plan for the platform does not stop a build that has one', async () => {
    const store = memoryStore();
    const all = ids(2);
    const be = backend(all);
    const clock = stepClock(T0);

    // ---- round 1: the older build. `plans: () => null` is exactly the state the
    //      measured records describe — the plan table had no row for this platform.
    const olderBuild = { ...opts(store, be.http, clock), scope: 'w44-capability', plans: () => null };
    const r1 = await runBackfill(olderBuild);

    expect(r1.stopped).toBe('halted');
    expect(r1.halted?.reason).toBe('unsupported-platform');
    expect(be.calls, 'a platform with no plan must stop before any request').toEqual([]);

    // ---- round 2: the capability exists. Nothing touched storage by hand.
    const r2 = await runBackfill({ ...opts(store, be.http, clock), scope: 'w44-capability', maxDetails: 1 });

    // 🔴 This is the assertion the measured account needed: the record is about a
    //    capability, the capability is here, so the leg runs.
    expect(r2.stopped).not.toBe('halted');
    expect(r2.halted, 'a record about a capability this build has must not stop it').toBeNull();
    expect(r2.archivedThisRun).toEqual([all[0]]);
    // Nothing was written off by the expiry itself: the debts are the list's own.
    expect(r2.state.pending).toEqual([all[1]]);
    // The marker: the record says what it was a judgement about, and it is computed
    // from the same lookup the judgement came from.
    expect(r1.halted?.capability).toBe('none');
    // 🔴 And it is not silent: the header carries the reason the record stopped applying.
    expect(r2.state.haltExpired).toMatchObject({
      reason: 'unsupported-platform',
      judgedAgainst: 'none',
      capability: 'full',
    });

    // ---- round 3: the expiry is persisted, and the record does not come back.
    const reloaded = await loadState(store, 'chatgpt', 'w44-capability');
    expect(reloaded.halted).toBeNull();
    expect(reloaded.haltExpired).toMatchObject({ because: 'capability', capability: 'full' });
    const r3 = await runBackfill({ ...opts(store, be.http, clock), scope: 'w44-capability', maxDetails: 1 });
    expect(r3.halted).toBeNull();
    expect(r3.archivedThisRun).toEqual([all[1]]);
  });

  it('an unsupported-platform stop that is still true is still permanent, and issued nothing', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const claude = {
      ...opts(store, mustNotFetch, clock),
      platform: 'claude',
      origin: CLAUDE_ORIGIN,
      scope: 'w44-still-unsupported',
      plans: () => null,
    };

    const r1 = await runBackfill(claude);
    expect(r1.halted?.reason).toBe('unsupported-platform');
    expect(r1.halted?.capability).toBe('none');

    clock.at(T0 + 100 * 24 * 3600_000);
    const r2 = await runBackfill(claude);
    // 🔴 The same capability this time, so the record still applies: this change
    //    must not turn a permanent stop into a retry.
    expect(r2.stopped).toBe('halted');
    expect(r2.halted?.reason).toBe('unsupported-platform');
    expect(r2.halted?.capability).toBe('none');
  });
});

describe('W44-2 · a record THIS build wrote is what must go on persisting', () => {
  /**
   * Every permanent reason that is **not** a capability judgement, in the two
   * states a stored record can be in: written by another build (unstamped, which
   * W59 reads as a different build) and written by this one.
   *
   * 🔴 **What this table used to assert, and why it changed.** W44 seeded all seven
   *    unmarked and required that *none* of them be re-decided, on the grounds that
   *    clearing a real halt — a platform whose wire changed, an account whose
   *    organization could not be named — is worse than the defect W44 fixed. W59
   *    generalises the same reasoning W44 used for the capability class to every
   *    permanent record, and the answer to that objection is what W44 itself relied
   *    on there: the record is **re-decided**, not cleared. The run that follows
   *    re-observes the condition, and a condition that is still there is written
   *    back **naming the build that saw it** — which is why the second half of every
   *    case below is "and now it sticks". Nothing is written off on either path.
   *
   * The reasons are the same seven W44 chose; what changed is what a record in each
   * state means, and the two halves below are the whole of it.
   *
   * 🔴 🔴 W59c · **And what the rewrite of this table left out, which is now back.**
   *    W59's version replaced "seed a record and require `mustNotFetch`" with "seed a
   *    record and require the leg to run", and for **four of the seven** the run
   *    cannot re-observe the condition it suspended: `storage-unavailable`,
   *    `state-unreadable` and `ledger-mismatch` are raised *before* the halt funnel
   *    (they are `openLedger`'s refusals, and reaching the funnel at all is what
   *    proves the store now works), and `detail-empty-unverified` is raised only for a
   *    conversation the body loop actually reaches. So their row ended on
   *    `persisted.halted === null` — an assertion for which "the run re-decided the
   *    condition and found it gone" and "the record was simply deleted" are the same
   *    observation. The first half below is the guarantee that was dropped (a record
   *    this build wrote refuses the fetch, run after run), the second is those four
   *    with their conditions still holding, and the third — in W44-2b — is the
   *    cross-build behaviour on its own terms, one test per reason.
   */
  const permanentNotCapability: HaltReason[] = [
    'org-ambiguous',
    'org-unresolved',
    'shape-changed',
    'storage-unavailable',
    'state-unreadable',
    'detail-empty-unverified',
    'ledger-mismatch',
  ];

  for (const reason of permanentNotCapability) {
    it(`a ${reason} record stamped with this build keeps stopping the leg, run after run`, async () => {
      const store = memoryStore();
      const clock = stepClock(T0);
      const scope = `w59-keeps-twice-${reason}`;
      await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
        halted: {
          reason,
          at: T0 - 3_600_000,
          detail: 'synthetic-fixture detail',
          build: TEST_BUILD_ID,
        },
      }));

      clock.at(T0 + 100 * 24 * 3600_000);
      /**
       * 🔴 **Run twice, and this is the half R59b found missing.** One run proves the
       *    reader; a second proves the record is still there to be read — a hold that
       *    the first run's own write quietly erased would pass the first and fail the
       *    second. Nothing is asked of the platform on either run, the record is not
       *    expired, and it is left exactly as it was found.
       */
      for (const round of [1, 2]) {
        const run = await runBackfill({ ...opts(store, mustNotFetch, clock), scope });
        expect(run.stopped, `round ${round}`).toBe('halted');
        expect(run.halted?.reason).toBe(reason);
        expect(run.state.haltExpired).toBeUndefined();
        expect(run.halted?.capability).toBeUndefined();
        expect(run.halted?.build).toBe(TEST_BUILD_ID);

        const persisted = await loadState(store, 'chatgpt', scope);
        expect(persisted.halted?.reason, `round ${round}: the record is still the record`).toBe(reason);
        expect(persisted.halted?.at).toBe(T0 - 3_600_000);
      }
    });
  }

  /**
   * 🔴 The four reasons whose condition a run cannot certify as gone by finishing,
   *    with the condition **still holding** — the state in which "the leg refuses"
   *    and "the record was deleted" are finally different observations.
   *
   * For the three storage-class ones the condition is not a stored record at all: it
   * is `openLedger`'s refusal, which fires before the halt funnel, so the record on
   * disk is never reached, never judged and never written. That is the property: a
   * build-stamp expiry **cannot** clear these while they are true, because the run
   * refuses before it gets a chance to ask.
   */
  it('🔴 a storage-unavailable record whose condition still holds refuses, and is left untouched', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59c-holds-storage-unavailable';
    const key = stateKey('chatgpt', scope);
    await store.save(key, headerWith('chatgpt', scope, {
      enumCursor: { offset: 0, complete: false },
      halted: {
        reason: 'storage-unavailable',
        at: T0 - 3_600_000,
        detail: 'synthetic-fixture detail',
        build: TEST_BUILD_ID,
      },
    }));

    // The debt set's store is gone: `openLedger` refuses, and the header — where the
    // record lives — is still readable, so the record really is there to be missed.
    const indexedDb = (globalThis as { indexedDB?: unknown }).indexedDB;
    delete (globalThis as { indexedDB?: unknown }).indexedDB;
    try {
      for (const round of [1, 2]) {
        const run = await runBackfill({ ...opts(store, mustNotFetch, clock), scope });
        expect(run.stopped, `round ${round}`).toBe('halted');
        expect(run.halted?.reason).toBe('storage-unavailable');
        expect(run.enumeratedPages, 'not one request').toBe(0);
        // 🔴 The record on disk is not the refusal's, and not rewritten: it is the one
        //    that was there before, still naming the build that wrote it.
        // 🔴 The record on disk, read raw — not through `loadState`, which for a
        //    refusal reports the refusal's own record instead (it is what the popup
        //    needs, and it is built fresh, so it cannot answer "was anything written").
        const persisted = (await store.load(key)) as { halted?: { at?: number; build?: string; detail?: string } };
        expect(persisted.halted?.at, `round ${round}: the record is the one that was there`).toBe(T0 - 3_600_000);
        expect(persisted.halted?.build).toBe(TEST_BUILD_ID);
        expect(persisted.halted?.detail).toBe('synthetic-fixture detail');
      }
    } finally {
      (globalThis as { indexedDB?: unknown }).indexedDB = indexedDb;
    }
  });

  it('🔴 a ledger-mismatch record whose condition still holds refuses, and is left untouched', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59c-holds-ledger-mismatch';
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      // The measured shape of a provable loss: a header recording debts the store
      // does not hold. W45's refusal, and it fires before the halt funnel.
      pendingCount: 2,
      enumCursor: { offset: 2, complete: false },
      halted: {
        reason: 'ledger-mismatch',
        at: T0 - 3_600_000,
        detail: 'synthetic-fixture detail',
        build: TEST_BUILD_ID,
      },
    }));

    for (const round of [1, 2]) {
      const run = await runBackfill({ ...opts(store, mustNotFetch, clock), scope });
      expect(run.stopped, `round ${round}`).toBe('halted');
      expect(run.halted?.reason).toBe('ledger-mismatch');
      expect(run.enumeratedPages, 'not one request').toBe(0);
      // Same raw read, same reason: a refusal is reported, and nothing is written over
      // the record the run never got to judge.
      const persisted = (await store.load(stateKey('chatgpt', scope))) as { halted?: { at?: number; build?: string; detail?: string } };
      expect(persisted.halted?.at, `round ${round}: the record is the one that was there`).toBe(T0 - 3_600_000);
      expect(persisted.halted?.build).toBe(TEST_BUILD_ID);
      expect(persisted.halted?.detail).toBe('synthetic-fixture detail');
    }
  });

  it('🔴 a detail-empty-unverified record whose condition recurs names this build, and the next run refuses', async () => {
    // The fourth of the four, and the one whose condition a run **can** re-observe:
    // the body loop reaches the conversations (they are still debts) and every body
    // comes back empty again. So this is a round trip through production code rather
    // than a hand-written record — the write-back is what the next run meets.
    //
    // 🔴 W92b · The queue is `DETAIL_EMPTY_HALT_STREAK` long, not one, because one
    //    empty body is now a per-conversation failure and only K in a row halt the
    //    leg. This is still the same round trip: the run re-observes the condition
    //    (K consecutive empties), writes the same stop back stamped with this build,
    //    and the next run holds.
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59c-recurs-detail-empty';
    const all = ids(DETAIL_EMPTY_HALT_STREAK);
    const be = backend(all);
    // The structure is good and the content is empty: C28's case, and the only one
    // that returns this outcome. Injected the same way tests/c28-emptyguard.test.ts
    // injects it — the plan is the real chatgpt plan with that one answer.
    const plan = { ...backfillPlanFor('chatgpt')!, parseDetailPage: () => ({ ok: true, outcome: 'detail-empty-unverified' } as const) };
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      enumCursor: { offset: 0, complete: false },
      halted: {
        reason: 'detail-empty-unverified',
        at: T0 - 3_600_000,
        detail: 'synthetic-fixture detail',
        build: OLDER_BUILD_ID,
      },
    }));

    clock.at(T0 + 100 * 24 * 3600_000);
    const r1 = await runBackfill({ ...opts(store, be.http, clock), scope, maxDetails: 1, plans: () => plan });
    expect(r1.state.haltExpired, 'another build’s record, so this build re-decides it').toMatchObject({
      because: 'build',
      reason: 'detail-empty-unverified',
    });
    // The condition recurred, so the same stop comes back — naming the build that saw it.
    expect(r1.halted?.reason).toBe('detail-empty-unverified');
    expect(r1.halted?.build).toBe(TEST_BUILD_ID);
    // 🔴 W92d · The K-1 empties before the halt are parked, not dropped: this run
    //    claims no `detail-empty` failure, and the K-th empty is the halt.
    expect(r1.failedThisRun).toEqual([]);
    expect(be.calls.length, 'the run reached the body it was told had come back empty').toBeGreaterThan(1);

    // ---- and the second run is the first one's equal: it refuses, and asks nothing.
    const after = be.calls.length;
    const r2 = await runBackfill({
      ...opts(store, mustNotFetch, clock),
      scope,
      maxDetails: 1,
      plans: () => plan,
    });
    expect(r2.stopped).toBe('halted');
    expect(r2.halted?.reason).toBe('detail-empty-unverified');
    expect(r2.halted?.at, 'the record was not rewritten: this run wrote no verdict').toBe(r1.halted?.at);
    expect(be.calls.length, 'a record this build wrote issues nothing').toBe(after);
  });

  it('an account halt still gets its own sentence, and not the expiry one', () => {
    const view = renderPopup({
      enabled: true,
      block: null,
      state: headerWith('claude', 'w44-org-popup', {
        halted: { reason: 'org-unresolved', at: T0, detail: 'none could be named', build: TEST_BUILD_ID },
      }),
      target: { platform: 'claude', scope: 'w44-org-popup' },
      failures: NO_FAILURES,
    });
    const out = popupText(view);
    expect(out).toContain('could not tell which Claude organization');
    expect(out).not.toContain('no longer applies');
  });
});

describe('W44-2b · the cross-build re-decision, one test per reason', () => {
  for (const reason of [
    'org-ambiguous',
    'org-unresolved',
    'shape-changed',
    'storage-unavailable',
    'state-unreadable',
    'detail-empty-unverified',
    'ledger-mismatch',
  ] as HaltReason[]) {
    it(`an unmarked ${reason} record names no build, so this build re-decides it once`, async () => {
      const store = memoryStore();
      const all = ids(1);
      const be = backend(all);
      const clock = stepClock(T0);
      const scope = `w44-keep-${reason}`;
      await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
        // The list has not been read to its end, so the re-decision has a list to
        // fetch: with `complete: true` the leg would have nothing to enumerate and
        // "it ran" would prove nothing about the record that used to stop it.
        enumCursor: { offset: 0, complete: false },
        halted: { reason, at: T0 - 3_600_000, detail: 'synthetic-fixture detail' },
      }));

      clock.at(T0 + 100 * 24 * 3600_000);
      const run = await runBackfill({ ...opts(store, be.http, clock), scope, maxDetails: 1 });

      // 🔴 The stored record did not decide this run: the leg ran, and the fetch
      //    that proves it is the one the old build's record forbade.
      expect(run.stopped).not.toBe('halted');
      expect(run.halted).toBeNull();
      expect(be.calls.length).toBeGreaterThan(0);
      // 🔴 And it is not silent: the expiry names what the record did not say, and
      //    the build that has just re-decided.
      expect(run.state.haltExpired).toMatchObject({
        because: 'build',
        reason,
        build: 'unstamped',
        currentBuild: TEST_BUILD_ID,
      });
      // Nothing of the user's was written off by the expiry itself.
      expect(run.archivedThisRun).toEqual([all[0]]);
      expect(run.state.pending).toEqual([]);

      /**
       * 🔴 🔴 W59c · **What "the record is gone" means here, per reason.**
       *
       * For `org-*`, `shape-changed` and `detail-empty-unverified` the run reached the
       * thing the record was about (the resolver's question, the wire, a body), so its
       * verdict is an observation: the record is cleared because the condition was
       * looked at. For the three storage-class ones the run's `openLedger` **is** the
       * observation — reaching the halt funnel at all is what proves a store that
       * once refused now works — and the half where it does *not* work is asserted
       * above, in "whose condition still holds refuses, and is left untouched". The two
       * halves are one property, and neither is the other's fallback.
       */
      const persisted = await loadState(store, 'chatgpt', scope);
      expect(persisted.halted).toBeNull();
    });
  }
});

describe('W44-3 · the record already on disk, written by an older build', () => {
  it('an unmarked capability record is re-decided once, and the re-decision sends nothing', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w44-legacy-capability';
    // Exactly the shape measured on 2026-09-19: `unsupported-platform`, a detail
    // saying the platform has no enumeration yet, and **no marker** — the build
    // that wrote it had no such field to write.
    await store.save(stateKey('claude', scope), headerWith('claude', scope, {
      halted: {
        reason: 'unsupported-platform',
        at: T0 - 3_540_000,
        detail: 'platform claude has no backfill enumeration yet; missing: listUrl | listPath',
      },
    }));

    let called = 0;
    const http: HttpPort = async (url: string) => {
      called += 1;
      throw new Error(`MUST NOT be called (${url})`);
    };
    const claude = {
      ...opts(store, http, clock),
      platform: 'claude',
      origin: CLAUDE_ORIGIN,
      scope,
      plans: () => null,
    };

    // ---- the one re-decision: the record did not say what it was judged against,
    //      so it cannot be checked, and it is treated as stale.
    const r1 = await runBackfill(claude);
    expect(r1.halted?.reason).toBe('unsupported-platform');
    // 🔴 The new record **does** say, so this can only happen once.
    expect(r1.halted?.capability).toBe('none');
    expect(called, 'a platform with no plan must still stop before any request').toBe(0);
    // 🔴 What it costs is written down rather than hidden: the trace names the two
    //    values it was decided between, and one of them is "the record did not say".
    expect(r1.state.haltExpired).toMatchObject({
      reason: 'unsupported-platform',
      judgedAgainst: 'unmarked',
      capability: 'none',
    });

    // ---- from here the record applies again, and for free.
    const r2 = await runBackfill(claude);
    expect(r2.stopped).toBe('halted');
    expect(called).toBe(0);
  });

  it('a legacy unmarked detail-unsupported record for a list-only platform expires once and is then stable', async () => {
    // 🔴 Perplexity's plan used to be the live example of the other capability
    //    value ('list-only', list real + body unsourced). 🔴 W84 filled its body
    //    segment in from the live probe, so it is no longer list-only — this test
    //    injects a synthetic list-only plan (SYNTHETIC_LIST_ONLY_PLAN) so the
    //    'list-only' capability path stays covered. The record on disk says
    //    nothing, and the run has to decide the capability for itself.
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w44-legacy-detail';
    await store.save(stateKey('perplexity', scope), headerWith('perplexity', scope, {
      // The list has not been read to its own end yet: this is the state a
      // `detail-unsupported` record is written from, since that decision is reached
      // after the list segment rather than before it.
      enumCursor: { offset: 0, complete: false },
      halted: { reason: 'detail-unsupported', at: T0 - 60_000, detail: 'missing: detailUrl | detailPath' },
    }));

    const calls: string[] = [];
    const http: HttpPort = async (url: string) => {
      calls.push(url);
      // Two rows, then an empty page: the list is read to its own end.
      // 🔴 The list item's id field is `slug`; a stub keyed `thread_id` would not
      //    parse and this test would stop on `shape-changed` before the
      //    capability question it exists to ask.
      return { status: 200, text: calls.length === 1 ? JSON.stringify([{ slug: 'pplx-0001-aaaaaaaa' }, { slug: 'pplx-0002-aaaaaaaa' }]) : '[]' };
    };
    const pplx = {
      ...opts(store, http, clock),
      platform: 'perplexity',
      origin: PPLX_ORIGIN,
      scope,
      listLimit: 2,
      plans: (platform: string) =>
        platform === 'perplexity' ? SYNTHETIC_LIST_ONLY_PLAN : backfillPlanFor(platform),
    };

    const r1 = await runBackfill(pplx);
    // 🔴 The list really was read — that is why this stop is not "the platform
    //    changed", and it is the visible difference the expiry makes here: before
    //    W44 the record stopped the run before a single request went out.
    expect(calls.length).toBeGreaterThan(0);
    /**
     * 🔴 W59b · **The re-decision reads ONE page, and the list still gets read to its
     *    own end — the two facts are one change, so they are asserted together.**
     *
     * The tick that lifts a halt is the one tick in the product that asks the platform
     * a question it was never asked, so it is capped at a single list page (engine.ts's
     * `listPagesThisTick`). For a list-only plan the ordinary tick has no such cap, so
     * this tick ends `budget-exhausted` rather than halting: a `detail-unsupported`
     * record written one page in would be this build's own from the next tick on, and
     * page 2 would never be read at all.
     *
     * What W44 pinned here has not moved — the wire is read, and the stop that comes
     * back names `list-only` — and the assertions below are that, plus the tick split.
     */
    expect(r1.stopped).toBe('budget-exhausted');
    expect(r1.halted).toBeNull();
    expect(r1.state.haltExpired).toMatchObject({ judgedAgainst: 'unmarked', capability: 'list-only' });
    // The one page this tick was allowed, and no more.
    expect(calls.length).toBe(1);
    expect(r1.state.enumCursor.offset).toBe(2);
    expect(r1.state.enumCursor.complete).toBe(false);

    // ---- the ordinary tick that follows: uncapped, so the list reaches its end and
    //      the same stop is reached with the whole list on disk.
    const r2 = await runBackfill(pplx);
    expect(r2.halted?.reason).toBe('detail-unsupported');
    expect(r2.halted?.capability).toBe('list-only');
    // Perplexity has no termination field, so "the list ended" is the empty page — the
    // named inference C27 records, not a `complete` this endpoint never said.
    expect(r2.state.enumCursor.truncated, 'the empty page that ends the list was read').toBe('empty-page-inferred');
    expect(calls.length, 'the second page was read on the tick that could read it').toBe(2);
    const afterSecond = calls.length;

    // 🔴 The marked record matches this build, so it applies: the third run costs
    //    nothing at all, exactly as a detail-unsupported stop did before W44.
    const r3 = await runBackfill(pplx);
    expect(r3.stopped).toBe('halted');
    expect(r3.halted?.capability).toBe('list-only');
    expect(calls.length, 'a marked record that still applies must issue nothing').toBe(afterSecond);
  });
});

describe('W44-4 · the popup can say that a stop stopped applying', () => {
  it('an expired halt gets its own sentence, and it is not the sentence for a stop still in force', () => {
    const view = renderPopup({
      enabled: true,
      block: null,
      state: headerWith('gemini', 'w44-popup', {
        halted: null,
        haltExpired: {
          because: 'capability',
          reason: 'unsupported-platform',
          recordedAt: T0 - 3_420_000,
          judgedAgainst: 'unmarked',
          capability: 'full',
          clearedAt: T0,
        },
      }),
      target: { platform: 'gemini', scope: 'w44-popup' },
      failures: NO_FAILURES,
    });
    const out = popupText(view);

    // It says what happened: the record stopped applying because this build can do
    // the work, and when the leg started again.
    expect(out).toContain('no longer applies');
    expect(out).toContain('started again');
    // Both capability values are readable, including the one that says the record
    // itself did not say — and that one must not read as "nothing was possible".
    expect(out).toContain('It recorded nothing — that record was written before');
    expect(out).toContain('this build records the conversation list and every conversation');
    expect(out).not.toContain('this build records no capability at all');
    // 🔴 And it must not borrow the wording of a stop that is still in force: that
    //    sentence says the platform's history cannot be backfilled, which is the
    //    opposite of what this note is reporting.
    expect(out).not.toContain('has no history backfill implemented');
  });

  it('a header with no expired record says nothing about one', () => {
    const view = renderPopup({
      enabled: true,
      block: null,
      state: headerWith('chatgpt', 'w44-popup-quiet'),
      target: { platform: 'chatgpt', scope: 'w44-popup-quiet' },
      failures: NO_FAILURES,
    });
    expect(popupText(view)).not.toContain('no longer applies');
  });
});
