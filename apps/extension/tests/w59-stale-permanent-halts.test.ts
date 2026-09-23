/**
 * W59 · A permanent halt is one build's judgement, and the build that replaced it
 * has not made it.
 *
 * The defect: a halt record said *why* a leg stopped and never said **which build
 * decided that**. Measured on a running extension (2026-09-23): the records at
 * `cs_backfill_v2:claude:<scope>` and `cs_backfill_v2:gemini:<scope>` read
 * `unsupported-platform`, with details saying "platform claude has no backfill
 * enumeration yet; missing: listUrl …" — written by builds that had no plan for
 * those platforms. The build that found them ships plans for both. The record was
 * a statement about the build that wrote it, and it was still being enforced
 * against a build that would have disagreed with every word of it.
 *
 * W44 fixed exactly this for the class of reasons that are *stated* to be about the
 * build (`unsupported-platform`, `detail-unsupported`) by marking them with the
 * capability they were judged against. W59 generalises the same reasoning to every
 * permanent halt — "the bytes are not a shape we know", "the account has more than
 * one organization and none was named", "our own stored record disagrees with
 * itself" are all judgements a build made about something it saw — and it does it
 * with the identity that cannot be forgotten to be bumped: `runtime.getManifest()`
 * `.version`, i.e. the build number that dev reloads already move.
 *
 * This file pins the five things the generalisation rests on:
 *
 *   1. **`unsupported-platform` is re-checked against the current build** — task 1,
 *      and the live record above is the fixture: a plan for the platform means the
 *      record no longer applies;
 *   2. **a different build gets exactly one fresh attempt** — the run re-observes
 *      the condition, and a condition that is still there is written back **naming
 *      the build that saw it**, so the second run issues nothing at all;
 *   3. **a record this build wrote keeps its full force** — permanent stays
 *      permanent, and no request is issued;
 *   4. **a build that cannot name itself clears nothing** — "we could not tell"
 *      must never be rounded into "another build" (CLAUDE.md invariant 1);
 *   5. **nothing of the user's is written off** — not a debt, not a cursor, not an
 *      archived conversation, and not a transient record's shape.
 *
 * 🔴 Everything here is synthetic: fixture ids, fixture responses, an injected
 *    clock, an injected build id and an injected plan lookup. No network, no real
 *    account, no conversation text — the fixtures carry counts and ids only.
 */

import { describe, it, expect } from 'vitest';

import { runBackfill, recordBackfillHalt, loadState, type HttpResponse, type HttpPort } from '../lib/backfill/engine';
import { backfillPlanFor, capabilityOf, type BackfillEnumPlan } from '../lib/backfill/enumerate';
import { openLedger } from '../lib/backfill/ledger';
import { readDebtSet } from '../lib/backfill/debt-store';
import { memoryStore } from '../lib/backfill/store';
import { stateKey, type BackfillHeader, type HaltReason } from '../lib/backfill/types';
import { renderPopup, popupText, NO_FAILURES } from '../lib/popup-view';
import { DEFAULT_DETAIL_PACE, DEFAULT_ENUM_PACE, type Clock } from '../lib/backfill/pace';
import { TEST_BUILD_ID } from './i18n-harness';

const ORIGIN = 'https://chatgpt.com';
const CLAUDE_ORIGIN = 'https://claude.ai';
/** An organization id shaped like the endpoint's own: 8-4-4-4-12 hex. */
const CLAUDE_ORG = '11111111-2222-3333-4444-555555555555';
const T0 = Date.parse('2026-09-23T09:00:00.000Z');
/** A build that is not this one, and is not the pre-W59 "does not say" either. */
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

/** A ChatGPT-shaped backend. Every request is counted, and `calls` is what "issued nothing" is asserted on. */
function backend(all: string[]): { http: HttpPort; calls: string[] } {
  const calls: string[] = [];
  const http: HttpPort = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    if (url.includes('/backend-api/conversations')) return { status: 200, text: listBody(all) };
    return { status: 200, text: '{}' };
  };
  return { http, calls };
}

/**
 * A backend whose list response is **not a shape this build reads**, so the engine
 * reaches the same conclusion the old build did, on its own, from the wire.
 */
function unrecognisedShape(): { http: HttpPort; calls: string[] } {
  const calls: string[] = [];
  const http: HttpPort = async (url: string): Promise<HttpResponse> => {
    calls.push(url);
    return { status: 200, text: '{}' };
  };
  return { http, calls };
}

/**
 * A header, with whatever a test wants it to claim. Written by hand: this is the
 * record already on disk, and `build` is exactly the field the test is about — so
 * it is set by the case rather than defaulted here.
 */
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

/** An http port that blows up: on the paths asserted below, not one request may be attempted. */
const mustNotFetch: HttpPort = async (url: string) => {
  throw new Error(`MUST NOT be called (${url})`);
};

// ---------------------------------------------------------------------------
// 1 · Task 1 — unsupported-platform, re-checked against the build that is running
// ---------------------------------------------------------------------------

describe('W59-1 · a stop about the build is re-checked against the build that is running', () => {
  it('🔴 the record measured on a real extension does not stop a build that ships a plan for that platform', async () => {
    /**
     * 🔴 The fixture is the measured record, verbatim in shape: `claude`, reason
     *    `unsupported-platform`, a detail naming `listUrl`, and **no capability
     *    marker** (the build that wrote it predates both W31's plan and W44's
     *    field). Nothing about the account, the login or the network is wrong with
     *    it — the statement is simply false of this build.
     *
     * 🔴 Said out loud rather than left to be discovered by running this against
     *    main: the *behaviour* asserted below — the leg runs, the fetch the record
     *    forbade is issued, the debts and the cursor are the run's own — already
     *    held on main, because W44 fixed exactly this case for capability reasons.
     *    This case is a **pin** of it (now run against the production plan table
     *    rather than an injected one); what it is red for on main is the reason code
     *    it also asserts. Task 1 was already done; task 2 is the new work.
     */
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = CLAUDE_ORG;

    // The task's own premise, asserted rather than assumed: this build has a plan
    // for the platform the record says it cannot enumerate.
    const plan: BackfillEnumPlan | null = backfillPlanFor('claude');
    expect(plan, 'this build must ship a plan for claude, or this case proves nothing').not.toBeNull();
    expect(capabilityOf(plan)).not.toBe('none');

    await store.save(stateKey('claude', scope), headerWith('claude', scope, {
      halted: {
        reason: 'unsupported-platform',
        at: T0 - 3_540_000,
        detail: 'platform claude has no backfill enumeration yet; missing: listUrl | listPath',
      },
    }));

    const calls: string[] = [];
    const http: HttpPort = async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      return { status: 200, text: JSON.stringify([{ uuid: 'conv-0001-aaaaaaaa' }]) };
    };

    // 🔴 No injected plan table: this runs against the production one, which is what
    //    "the current build" means.
    const run = await runBackfill({
      ...opts(store, http, clock),
      platform: 'claude',
      origin: CLAUDE_ORIGIN,
      scope,
      maxDetails: 0,
    });

    // 🔴 The assertion the measured account needed: the record is about a build,
    //    this build is a different one, so it does not stop the leg.
    expect(run.stopped).not.toBe('halted');
    expect(run.halted).toBeNull();
    expect(calls.length, 'the record stopped before the request it was about; this build asks it').toBeGreaterThan(0);
    // 🔴 And the expiry is written down, with the reason code and both values.
    expect(run.state.haltExpired).toMatchObject({
      because: 'capability',
      reason: 'unsupported-platform',
      judgedAgainst: 'unmarked',
      capability: capabilityOf(plan),
    });

    // ---- nothing of the user's was touched on the way past it.
    const persisted = await loadState(store, 'claude', scope);
    expect(persisted.halted).toBeNull();
    expect(run.state.pending).toEqual(['conv-0001-aaaaaaaa']);
    expect(run.state.enumCursor.offset).toBeGreaterThan(0);
  });

  it('🔴 a stop that is still true stays stopped, whoever wrote it', async () => {
    /**
     * The other half of task 1, and the half a "clear the stale halt" fix would
     * break: a platform this build has **no** plan for has a record whose statement
     * is still exactly true. `plans: () => null` is a build with no plan for that
     * platform, so this is the same shape as the real record with the capability
     * unchanged — and the answer must be the old one.
     */
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59-still-unsupported';
    const run = await runBackfill({
      ...opts(store, mustNotFetch, clock),
      platform: 'claude',
      origin: CLAUDE_ORIGIN,
      scope,
      plans: () => null,
    });

    expect(run.stopped).toBe('halted');
    expect(run.halted?.reason).toBe('unsupported-platform');
    expect(run.halted?.capability).toBe('none');

    // ---- and a day later, with this build's own record on disk, still nothing.
    clock.at(T0 + 24 * 3600_000);
    const r2 = await runBackfill({
      ...opts(store, mustNotFetch, clock),
      platform: 'claude',
      origin: CLAUDE_ORIGIN,
      scope,
      plans: () => null,
    });
    expect(r2.stopped).toBe('halted');
    expect(r2.state.haltExpired).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// 2 · Task 2 — one fresh attempt per build change, and a recurrence that sticks
// ---------------------------------------------------------------------------

describe('W59-2 · a permanent halt another build wrote is re-decided exactly once', () => {
  it('🔴 the condition is looked at again, and a condition that repeats is written back naming this build', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59-shape-other-build';
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      halted: {
        reason: 'shape-changed',
        at: T0 - 3_600_000,
        detail: 'synthetic-fixture: the list response is not a list',
        build: OLDER_BUILD,
      },
    }));

    const shape = unrecognisedShape();

    // ---- the one fresh attempt: the wire is read, and the same shape is met.
    const r1 = await runBackfill({ ...opts(store, shape.http, clock), scope });
    expect(shape.calls.length, 'a record from another build does not decide this run').toBeGreaterThan(0);
    expect(r1.halted?.reason).toBe('shape-changed');
    // 🔴 This is what makes it one attempt and not a retry: the stop that comes
    //    back is written by the build that just saw the condition.
    expect(r1.halted?.build).toBe(TEST_BUILD_ID);
    expect(r1.state.haltExpired).toEqual({
      because: 'build',
      reason: 'shape-changed',
      build: OLDER_BUILD,
      currentBuild: TEST_BUILD_ID,
      recordedAt: T0 - 3_600_000,
      clearedAt: T0,
    });

    // ---- and from there it is this build's own judgement, so it stands.
    const after = shape.calls.length;
    const r2 = await runBackfill({ ...opts(store, shape.http, clock), scope });
    expect(r2.stopped).toBe('halted');
    expect(r2.halted?.reason).toBe('shape-changed');
    expect(shape.calls.length, 'the second run must issue nothing at all').toBe(after);
    expect(r2.state.haltExpired?.clearedAt, 'nothing new expired').toBe(T0);
  });

  it('🔴 a record with no build field at all counts as a different build', async () => {
    // The migration rule stated as a case: "written before this field existed" is
    // read as a different build, not as this one and not as a third kind.
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59-shape-unstamped';
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      halted: { reason: 'shape-changed', at: T0 - 3_600_000, detail: 'synthetic-fixture' },
    }));

    const shape = unrecognisedShape();
    const r1 = await runBackfill({ ...opts(store, shape.http, clock), scope });

    expect(shape.calls.length).toBeGreaterThan(0);
    expect(r1.state.haltExpired).toMatchObject({
      because: 'build',
      build: 'unstamped',
      currentBuild: TEST_BUILD_ID,
    });
    expect(r1.halted?.build).toBe(TEST_BUILD_ID);
  });

  it('🔴 a record THIS build wrote is permanent, and issues nothing', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59-shape-this-build';
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      halted: {
        reason: 'shape-changed',
        at: T0 - 3_600_000,
        detail: 'synthetic-fixture: the list response is not a list',
        build: TEST_BUILD_ID,
      },
    }));

    clock.at(T0 + 100 * 24 * 3600_000);
    const run = await runBackfill({ ...opts(store, mustNotFetch, clock), scope });

    expect(run.stopped).toBe('halted');
    expect(run.state.haltExpired).toBeUndefined();
    expect(run.halted?.at, 'left exactly as it was found').toBe(T0 - 3_600_000);
  });

  it('🔴 a build that cannot name itself clears nothing, whatever the record says', async () => {
    /**
     * The conservative direction, and the reason `build` is `string | null` rather
     * than `string`: "this build cannot name itself" and "this record names another
     * build" are different facts. A record may be cleared only on the second, never
     * on the first — otherwise an environment without the manifest API would retry
     * a permanent stop on every single tick.
     */
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59-unnameable-build';
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      halted: { reason: 'shape-changed', at: T0 - 3_600_000, detail: 'synthetic-fixture' },
    }));

    const run = await runBackfill({ ...opts(store, mustNotFetch, clock), scope, build: null });
    expect(run.stopped).toBe('halted');
    expect(run.state.haltExpired).toBeUndefined();

    // ---- and a run that cannot name itself writes **no stamp at all** on a
    //      permanent record, rather than an empty string: a stamp of `''` would
    //      compare equal to another `''` and hold a record in force on the strength
    //      of a value that means "we could not tell".
    const written = await runBackfill({
      ...opts(store, mustNotFetch, clock),
      platform: 'claude',
      origin: CLAUDE_ORIGIN,
      scope: 'w59-unnameable-writer',
      plans: () => null,
      build: null,
    });
    expect(written.halted?.reason).toBe('unsupported-platform');
    expect(written.halted?.build).toBeUndefined();
    expect('build' in (written.halted ?? {}), 'the field is omitted, not written empty').toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 3 · Task 3 — the retry is visible, and it is not the capability sentence
// ---------------------------------------------------------------------------

describe('W59-3 · the popup can say a halt from an older build was retried', () => {
  it('🔴 the sentence names the build, and it is not the sentence for an expiry about capability', () => {
    const view = renderPopup({
      enabled: true,
      block: null,
      state: headerWith('chatgpt', 'w59-popup', {
        halted: null,
        haltExpired: {
          because: 'build',
          reason: 'shape-changed',
          recordedAt: T0 - 3_600_000,
          build: OLDER_BUILD,
          currentBuild: TEST_BUILD_ID,
          clearedAt: T0,
        },
      }),
      target: { platform: 'chatgpt', scope: 'w59-popup' },
      failures: NO_FAILURES,
    });
    const out = popupText(view);

    // It says what happened, in the two build ids and nothing else.
    expect(out).toContain(OLDER_BUILD);
    expect(out).toContain(TEST_BUILD_ID);
    expect(out).toContain('written by');
    expect(out).toContain('re-checked once');
    expect(out).toContain('nothing written off');
    // 🔴 Not the capability sentence: nothing about this extension's ability
    //    changed, and printing "it recorded list-only" here would name a judgement
    //    nobody made.
    expect(out).not.toContain('It recorded');
    expect(out).not.toContain('this build records');
    // 🔴 And not the sentence for a stop still in force, which says the opposite.
    expect(out).not.toContain('has no history backfill implemented');
  });

  it('a record that did not name a build says so, and is not guessed at', () => {
    const view = renderPopup({
      enabled: true,
      block: null,
      state: headerWith('chatgpt', 'w59-popup-unstamped', {
        halted: null,
        haltExpired: {
          because: 'build',
          reason: 'org-unresolved',
          recordedAt: T0 - 3_600_000,
          build: 'unstamped',
          currentBuild: TEST_BUILD_ID,
          clearedAt: T0,
        },
      }),
      target: { platform: 'chatgpt', scope: 'w59-popup-unstamped' },
      failures: NO_FAILURES,
    });
    const out = popupText(view);

    expect(out).toContain('a build that record did not name');
    expect(out).toContain(TEST_BUILD_ID);
    // 🔴 It must not guess *which* build: the record could have been written by a
    //    newer one (a downgrade, a second profile), and the unknown is not "older".
    expect(out).not.toContain(OLDER_BUILD);
  });

  it('a record written by W44, with no `because`, still gets the capability sentence', () => {
    /**
     * Compatibility, stated rather than assumed: `because` is absent on every record
     * W44 wrote, and absent means "the only kind that existed then" — not a third
     * kind, and not silence. The cast is the point of the case rather than a
     * shortcut around it: this build's type **cannot** express a record that
     * predates the field, and the record on a real user's disk can be exactly that.
     * Writing it as a typed value would be testing a shape that cannot occur.
     */
    const w44Shape = {
      reason: 'unsupported-platform',
      recordedAt: T0 - 3_420_000,
      judgedAgainst: 'unmarked',
      capability: 'full',
      clearedAt: T0,
    } as unknown as NonNullable<BackfillHeader['haltExpired']>;

    const view = renderPopup({
      enabled: true,
      block: null,
      state: headerWith('gemini', 'w59-popup-legacy', {
        halted: null,
        haltExpired: w44Shape,
      }),
      target: { platform: 'gemini', scope: 'w59-popup-legacy' },
      failures: NO_FAILURES,
    });
    const out = popupText(view);

    expect(out).toContain('It recorded nothing — that record was written before');
    expect(out).toContain('this build records the conversation list and every conversation');
    expect(out).not.toContain('written by');
  });
});

// ---------------------------------------------------------------------------
// 4 · What the change must not move
// ---------------------------------------------------------------------------

describe('W59-4 · the paths this change may not touch', () => {
  it('🔴 a transient stop is untouched by a build change: the clock re-decides it, not a build', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59-transient';
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      halted: {
        reason: 'transport-error',
        at: T0,
        detail: 'synthetic-fixture: message channel closed',
        attempts: 1,
        retryAt: T0 + 300_000,
      },
    }));

    const shape = unrecognisedShape();
    const waiting = await runBackfill({ ...opts(store, shape.http, clock), scope, build: OLDER_BUILD });

    // A build change is not a reason to jump a backoff: no request, no expiry, and
    // the record is not stamped — which is what keeps its written shape identical.
    expect(waiting.stopped).toBe('waiting-retry');
    expect(shape.calls).toEqual([]);
    expect(waiting.state.haltExpired).toBeUndefined();
    expect(waiting.halted?.build).toBeUndefined();

    // And the ladder still governs it: due at retryAt, not before.
    clock.at(T0 + 300_000);
    const due = await runBackfill({ ...opts(store, shape.http, clock), scope, build: OLDER_BUILD });
    expect(due.stopped).not.toBe('waiting-retry');
    expect(shape.calls.length).toBeGreaterThan(0);
  });

  it('🔴 the expiry clears the record and nothing else: the debts and the cursor are the run\'s own', async () => {
    const store = memoryStore();
    const clock = stepClock(T0);
    const scope = 'w59-nothing-wiped';

    // Two conversations already owed, and a list that has not been read to its end.
    const opened = await openLedger(store, 'chatgpt', scope);
    expect(opened.ok).toBe(true);
    if (!opened.ok) return;
    await opened.ledger.save({ ...opened.state, pending: ['conv-9000-aaaaaaaa', 'conv-9001-aaaaaaaa'] });

    const all = [...ids(1)];
    const be = backend(all);
    await store.save(stateKey('chatgpt', scope), headerWith('chatgpt', scope, {
      pendingCount: 2,
      enumCursor: { offset: 3, complete: false },
      halted: { reason: 'org-ambiguous', at: T0 - 3_600_000, detail: 'synthetic-fixture', build: OLDER_BUILD },
    }));

    const run = await runBackfill({ ...opts(store, be.http, clock), scope, maxDetails: 0 });

    // 🔴 Nothing was written off while the record stood, and nothing was written off
    //    by clearing it: the two debts are still owed, and the cursor did not go
    //    back to the beginning.
    expect(run.halted).toBeNull();
    expect(run.state.pending).toContain('conv-9000-aaaaaaaa');
    expect(run.state.pending).toContain('conv-9001-aaaaaaaa');
    expect(run.archivedThisRun).toEqual([]);
    expect(run.state.enumCursor.offset).toBeGreaterThanOrEqual(3);
    expect(await readDebtSet('chatgpt', scope)).toMatchObject({
      pending: expect.arrayContaining(['conv-9000-aaaaaaaa', 'conv-9001-aaaaaaaa']),
      archived: [],
    });
  });

  it('🔴 the resolver path stamps its records too, so an organization stop is not re-asked every tick', async () => {
    // `recordBackfillHalt` is how `org-ambiguous` / `org-unresolved` reach storage
    // (they are written before any run), so a stamp that only the engine wrote would
    // leave those two reasons re-decided on every single tick.
    const store = memoryStore();
    const scope = 'w59-resolver-stamp';
    const reasons: HaltReason[] = ['org-ambiguous', 'org-unresolved'];

    for (const reason of reasons) {
      const written = await recordBackfillHalt(store, {
        platform: 'claude', scope, reason, detail: 'synthetic-fixture',
      });
      expect(written).toBe(true);
      const persisted = await loadState(store, 'claude', scope);
      expect(persisted.halted?.reason).toBe(reason);
      expect(persisted.halted?.build).toBe(TEST_BUILD_ID);
    }
  });
});
