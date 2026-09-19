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
import { runBackfill, loadState, type HttpResponse, type HttpPort } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import { stateKey, type BackfillHeader, type HaltReason } from '../lib/backfill/types';
import { renderPopup, popupText, NO_FAILURES } from '../lib/popup-view';
import { DEFAULT_DETAIL_PACE, DEFAULT_ENUM_PACE, type Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://chatgpt.com';
const PPLX_ORIGIN = 'https://www.perplexity.ai';
const CLAUDE_ORIGIN = 'https://claude.ai';
const T0 = Date.parse('2026-09-19T15:56:00.000Z');

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
    expect(reloaded.haltExpired?.capability).toBe('full');
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

describe('W44-2 · the halts that must persist are untouched', () => {
  /**
   * Every permanent reason that is **not** a statement about this build, seeded as
   * an unmarked record — the same shape as the measured capability records — and
   * run against an http port that must not be reached.
   *
   * 🔴 What this table is for: the migration rule in W44-4 has to read "unmarked"
   *    differently per class. If it read it as "expired" everywhere, every one of
   *    these would be cleared — the account's real conditions, the platform's real
   *    wire change — and the leg would run against them. That is worse than the
   *    defect this task fixes, so it is a table and not a sentence.
   */
  const accountAndUpstream: HaltReason[] = [
    'org-ambiguous',
    'org-unresolved',
    'shape-changed',
    'storage-unavailable',
    'state-unreadable',
    'detail-empty-unverified',
    'ledger-mismatch',
  ];

  for (const reason of accountAndUpstream) {
    it(`an unmarked ${reason} record keeps stopping the leg, and is never treated as expired`, async () => {
      const store = memoryStore();
      const clock = stepClock(T0);
      const scope = `w44-keep-${reason}`;
      await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
        halted: { reason, at: T0 - 3_600_000, detail: 'synthetic-fixture detail' },
      }));

      clock.at(T0 + 100 * 24 * 3600_000);
      const run = await runBackfill({ ...opts(store, mustNotFetch, clock), scope });

      expect(run.stopped).toBe('halted');
      expect(run.halted?.reason).toBe(reason);
      // 🔴 Not expired, and not marked as a capability judgement either: the record
      //    was left exactly as it was found.
      expect(run.state.haltExpired).toBeUndefined();
      expect(run.halted?.capability).toBeUndefined();

      const persisted = await loadState(store, 'chatgpt', scope);
      expect(persisted.halted?.reason).toBe(reason);
      expect(persisted.halted?.at).toBe(T0 - 3_600_000);
    });
  }

  it('the organization halts are not capability judgements, and the popup still says their own sentence', () => {
    const view = renderPopup({
      enabled: true,
      block: null,
      state: headerWith('claude', 'w44-org-popup', {
        halted: { reason: 'org-unresolved', at: T0, detail: 'none could be named' },
      }),
      target: { platform: 'claude', scope: 'w44-org-popup' },
      failures: NO_FAILURES,
    });
    const out = popupText(view);
    expect(out).toContain('could not tell which Claude organization');
    expect(out).not.toContain('no longer applies');
  });
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
    // 🔴 Perplexity's plan is the live example of the other capability value: its
    //    list segment is real and its body segment has no source, so this build's
    //    answer for it is 'list-only'. The record on disk says nothing, and the
    //    run has to decide that for itself.
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
      return { status: 200, text: calls.length === 1 ? JSON.stringify([{ thread_id: 'pplx-0001-aaaaaaaa' }, { thread_id: 'pplx-0002-aaaaaaaa' }]) : '[]' };
    };
    const pplx = {
      ...opts(store, http, clock),
      platform: 'perplexity',
      origin: PPLX_ORIGIN,
      scope,
      listLimit: 2,
    };

    const r1 = await runBackfill(pplx);
    // 🔴 The list really was read — that is why this stop is not "the platform
    //    changed", and it is the visible difference the expiry makes here: before
    //    W44 the record stopped the run before a single request went out.
    expect(calls.length).toBeGreaterThan(0);
    expect(r1.halted?.reason).toBe('detail-unsupported');
    expect(r1.halted?.capability).toBe('list-only');
    expect(r1.state.haltExpired).toMatchObject({ judgedAgainst: 'unmarked', capability: 'list-only' });
    const afterFirst = calls.length;

    // 🔴 The marked record matches this build, so it applies: the second run costs
    //    nothing at all, exactly as a detail-unsupported stop did before W44.
    const r2 = await runBackfill(pplx);
    expect(r2.stopped).toBe('halted');
    expect(r2.halted?.capability).toBe('list-only');
    expect(calls.length, 'a marked record that still applies must issue nothing').toBe(afterFirst);
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
