/**
 * C17 · Walk the backfill leg **end to end** once.
 *
 * How the work is split with c11/c12/c13:
 *  · c11 verifies the engine alone, c12 the guards alone, and c13 only that "the wiring arrives" (runBackfill is vi.mock'd out).
 *  · 🔴 This file **mocks no link in the chain**: the real engine + real debts + real pacing + the real write-down exit +
 *    the real background.ts entry point, swapping only browser.* for a fake and the http port for a synthetic server.

 *  · Zero real network and zero logged-in state throughout: the http port is a pure function in this file,
 *    and the write-down channel is a synthetic native host that merely records names into an array.
 *
 * Every case starts from the **real entry point**: runtime.onMessage('chat-captured').
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';
import { stateKey, type BackfillState } from '../lib/backfill/types';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

// ---------------------------------------------------------------------------
// A fake browser. Storage **survives resetModules** — which is exactly the model of a "browser restart":
// in-memory state (module variables) is cleared while storage.local stays.
//
// W2: the write-down channel = a synthetic native host (tests/synthetic-native-host.ts).
// "Was this one actually stored" no longer looks at whether a file is on disk but at whether the host acked —
// which was the only thing that ever counted under the spec (§1).
// ---------------------------------------------------------------------------
const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
let host: SyntheticHost;
/** Take the host entirely offline (simulating "the host is not installed / does not answer"). */
let hostDown = false;

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    sendNativeMessage: (h: string, m: unknown) => {
      if (hostDown) throw new Error('Specified native messaging host not found.');
      return host.sendNativeMessage(h, m);
    },
  },
  storage: {
    local: {
      async get(defaults: Record<string, unknown>) {
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(defaults)) out[k] = k in store ? store[k] : defaults[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
};

// ---------------------------------------------------------------------------
// A synthetic "server". It never touches the network: just a pure (url) => {status,text} function.
// ---------------------------------------------------------------------------
interface ServerOpts {
  ids: string[];
  /** null ⇒ the list endpoint gives **no** total, used to verify "no denominator means no %". */
  total: number | null;
  pageSize: number;
  /** A duplicate id stuffed into one page (simulating "the same conversation enumerated twice"). */
  dupeOnPage?: { page: number; id: string };
}

function makeServer(opts: ServerOpts) {
  const calls: string[] = [];
  const port = async (url: string) => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === '/backend-api/conversations') {
      const offset = Number(u.searchParams.get('offset') ?? '0');
      const page = Math.floor(offset / opts.pageSize);
      const slice = opts.ids.slice(offset, offset + opts.pageSize);
      const items = slice.map((id) => ({ id, title: 'synthetic' }));
      if (opts.dupeOnPage && opts.dupeOnPage.page === page) {
        items.push({ id: opts.dupeOnPage.id, title: 'synthetic-dupe' });
      }
      const body: Record<string, unknown> = { items };
      if (opts.total !== null) body.total = opts.total;
      return { status: 200, text: JSON.stringify(body) };
    }
    // Body fetch: it must hit chatgpt's responseShape (mapping + current_node).
    const id = decodeURIComponent(u.pathname.replace('/backend-api/conversation/', ''));
    return {
      status: 200,
      text: JSON.stringify({
        mapping: { n1: { id: 'n1', message: { content: { parts: [`synthetic body for ${id}`] } } } },
        current_node: 'n1',
        account_id: 'acct-fixture-1',
      }),
    };
  };
  return { port, calls };
}

/** The live leg's payload (a synthetic fixture, not anybody's actual conversation). */
function liveCapture(account = 'acct-fixture-1'): CapturedFetch {
  const sid = 'aaaaaaaa-1111-2222-3333-444444444444';
  return {
    url: `https://chatgpt.com/backend-api/conversation/${sid}`,
    method: 'GET',
    status: 200,
    text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: account }),
    pageUrl: `https://chatgpt.com/c/${sid}`,
    capturedAt: 1_700_000_000_000,
  };
}

/** Take the real entry point: load background → run defineBackground's callback → dispatch a message. */
async function bootAndDispatch(payload: CapturedFetch): Promise<{ mod: any; responded: any }> {
  const mod: any = await import('../entrypoints/background');
  /**
   * 🔴 W16 · `random: () => 0` joined this seam because the jitter is now the
   *    **default**: the pacer's gap is `minIntervalMs + uniform[0, jitterMs]`,
   *    so without pinning the draw, the total-clock-advance assertions below
   *    (`60_000` ms after four ticks) would become a lottery. `() => 0` is the
   *    bottom of every band, at which each drawn gap is exactly the documented
   *    minimum — i.e. exactly the numbers this file was written against. The
   *    jitter *itself* is exercised in tests/w3-jitter.test.ts, not here.
   */
  mod.configureBackfillPace({ clock: fakeClock, random: () => 0 });
  if (runtimeListeners.length === 0) await mod.default();
  const responded = await new Promise<any>((resolve) => {
    const ret = runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
    expect(ret).toBe(true);
  });
  await mod.backfillTickSettled();
  return { mod, responded };
}

/**
 * 🔴 W18 · The debt ids no longer live in the one `storage.local` record, so this
 *    accessor can no longer be a property read: it goes through the **same load
 *    path the engine uses** (header at `stateKey`, ids from the debt store —
 *    lib/backfill/ledger.ts) and hands back the assembled state. The assertions
 *    below are unchanged, which is the point: the ledger's content did not move,
 *    only where it is kept.
 *
 * It is async for that reason, and the scope is a parameter rather than a raw
 * storage key, because a storage key is no longer the thing that names a debt set.
 */
async function stateOf(scope = 'acct-fixture-1'): Promise<BackfillState> {
  const { loadState } = await import('../lib/backfill/engine');
  const { browserLocalStore } = await import('../lib/backfill/store');
  const st = browserLocalStore();
  if (!st) throw new Error('this suite runs against a fake browser with storage.local; it must not be null');
  return await loadState(st, 'chatgpt', scope);
}

/** The storage keys that name a debt set, whatever the layout version — used for "nothing was created at all". */
function stateKeys(): string[] {
  return Object.keys(store).filter((k) => k.startsWith('cs_backfill_v'));
}

function detailCalls(calls: string[]): string[] {
  return calls.filter((u) => u.includes('/backend-api/conversation/'));
}

/** The names the host acked — this is now the only evidence of "stored" (§1). */
function finalWrites(): string[] {
  return host.names();
}

const UUIDS = [
  'b1111111-0000-4000-8000-000000000001',
  'b2222222-0000-4000-8000-000000000002',
  'b3333333-0000-4000-8000-000000000003',
  'b4444444-0000-4000-8000-000000000004',
];

/**
 * 🔴 C19: a fake clock. sleep does not really wait, it only advances now.
 * Why it is now required: C19 fixed BUG-3 (the body-fetch minimum interval now takes effect across ticks),
 * so the cases in this file that "kick it several times in a row" really would sleep 20 seconds each — which is the evidence
 * that it is fixed, but the test should not have to sleep too. The clock is injected through background's test seam (configureBackfillPace).
 */
let fakeNow = 1_700_000_000_000;
const fakeClock = {
  now: () => fakeNow,
  sleep: async (ms: number) => { fakeNow += ms; },
};

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  host = createSyntheticHost({ up: true });
  hostDown = false;
  (globalThis as any).indexedDB = new IDBFactory();
  fakeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

async function enableBackfill(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
}

// ===========================================================================
// Task 1 · walk one complete backfill (N = 4 > 1)
// ===========================================================================
describe('C17 task 1 · enumerate → debts → paced one-by-one fetch → write down → debts shrink → progress updates', () => {
  it('N=4 down the whole chain, with independent evidence at every step', async () => {
    await enableBackfill();
    const server = makeServer({ ids: UUIDS, total: 4, pageSize: 2 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    // ---- tick 1: the first list page + the 1st debt is cleared ----
    await bootAndDispatch(liveCapture());
    const listUrls = server.calls.filter((u) => u.includes('/backend-api/conversations'));
    console.log('[C17-1] evidence A — the list pages actually requested:', listUrls);
    // 🔴 W10 · This was `listUrls.length === 2` ("the whole list runs inside one
    //    tick"), and the whole point of this change is that it no longer does:
    //    a tick reads **at most one page**, then spends its body budget, so the
    //    first debt is delivered in the same tick that names it. The criterion —
    //    enumerate → debts → fetch one by one → write down → the debts shrink —
    //    is unchanged; what changed is how many pages one tick may read.
    expect(listUrls.length).toBe(1);            // 1 page × pageSize 2 = 2 rows named so far

    const s1 = await stateOf();
    console.log('[C17-1] evidence B — the debt set (read back out of storage):', {
      key: stateKey('chatgpt', 'acct-fixture-1'), pending: s1.pending, archived: s1.archived,
      totalKnown: s1.totalKnown, totalSource: s1.totalSource, enumCursor: s1.enumCursor,
    });
    // 🔴 W10 · `complete: true` used to be reached here, via `offset >= total`.
    //    It is now reached by the **empty page** (tick 3 below), because a real
    //    account was measured with total=901 while holding 7,391 conversations:
    //    a total that can be wrong cannot say "the list is finished".
    expect(s1.enumCursor.complete).toBe(false);
    expect(s1.archived.length).toBe(1);
    // 2 rows named on page 1, one of them already archived ⇒ 1 still owed.
    expect(s1.pending.length).toBe(1);

    console.log('[C17-1] evidence C — the body URLs tick1 actually fetched:', detailCalls(server.calls));
    expect(detailCalls(server.calls).length).toBe(1);   // "one by one": a tick fetches exactly one

    console.log('[C17-1] evidence D — the final files tick1 wrote down:', finalWrites());
    console.log('[C17-1] evidence E — tick1 progress text:', mod.lastBackfillTick()?.report?.progress);
    console.log('[C17-1] evidence F — tick1 paceTrace:', mod.lastBackfillTick()?.report?.paceTrace);

    // ---- ticks 2..4: clear the remaining 3 and watch the debts fall one by one ----
    const trail: Array<{ tick: number; pending: number; archived: number; progress: string }> = [];
    for (let i = 2; i <= 4; i += 1) {
      await bootAndDispatch(liveCapture());
      const s = await stateOf();
      trail.push({
        tick: i, pending: s.pending.length, archived: s.archived.length,
        progress: mod.lastBackfillTick()!.report!.progress,
      });
    }
    console.log('[C17-1] evidence G — debts falling tick by tick, plus progress text:', trail);
    expect(trail.map((t) => t.pending)).toEqual([2, 1, 0]);
    expect(trail.map((t) => t.archived)).toEqual([2, 3, 4]);
    // 🔴 W10 · The list really did finish — and it finished on the **empty page**
    //    (tick 3), not on `offset >= total`: 4 rows are named, and the third tick
    //    is the one that asked for offset=4 and got nothing back. Ticks 1 and 2
    //    each named one page of two (the "at most one list page per tick" rule),
    //    and the moment the list was finished the ticks stopped asking for pages
    //    at all — tick 4 reads no page (which is why `complete` is checked on the
    //    state after tick 3, and again below after tick 4).
    expect((await stateOf()).enumCursor.complete).toBe(true);
    const allListUrls = server.calls.filter((u) => u.includes('/backend-api/conversations'));
    // 3 in total, over 4 ticks: offset=0, offset=2, the confirming empty page at
    // offset=4, and nothing at all on tick 4.
    expect(allListUrls.map((u) => new URL(u).searchParams.get('offset'))).toEqual(['0', '2', '4']);

    // Fetched one by one: 4 detail requests, in FIFO order
    expect(detailCalls(server.calls).length).toBe(4);
    expect(detailCalls(server.calls).map((u) => u.split('/').pop())).toEqual(UUIDS);

    // Written down: one final file per conversation (plus the live leg's own)
    const backfillFiles = finalWrites().filter((f) => UUIDS.some((id) => f.includes(id)));
    console.log('[C17-1] evidence H — the 4 final files the backfill leg wrote down:', backfillFiles);
    expect(backfillFiles.length).toBe(4);

    console.log('[C17-1] evidence I — the final progress text:', trail[trail.length - 1]!.progress);
    expect(trail[trail.length - 1]!.progress).toContain('100%');
  });
});

// ===========================================================================
// Task 2 · four seam counter-cases
// ===========================================================================
describe('C17 task 2 · counter-case 1: a "browser restart" mid-way (in-memory state cleared, storage kept)', () => {
  it('it carries on from the debt set rather than starting over', async () => {
    await enableBackfill();
    const server1 = makeServer({ ids: UUIDS, total: 4, pageSize: 2 });
    let mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server1.port);
    await bootAndDispatch(liveCapture());
    await bootAndDispatch(liveCapture());
    const before = await stateOf();
    console.log('[C17-2.1] before the restart:', { archived: before.archived, pending: before.pending });
    expect(before.archived.length).toBe(2);

    // ---- "a browser restart": all module in-memory state cleared, storage left as it is ----
    vi.resetModules();
    runtimeListeners.length = 0;
    const { resetTickLockForTest } = await import('../lib/backfill/schedule');
    resetTickLockForTest();
    const server2 = makeServer({ ids: UUIDS, total: 4, pageSize: 2 });
    mod = await import('../entrypoints/background');
    // After the restart the transport is null too (a production fact); nothing moves until it is injected again.
    expect(mod.lastBackfillTick()).toBeNull();       // the in-memory state really was cleared
    mod.configureBackfillTransport(server2.port);

    await bootAndDispatch(liveCapture());
    const after = await stateOf();
    console.log('[C17-2.1] the bodies the first tick after the restart fetched:', detailCalls(server2.calls));
    console.log('[C17-2.1] after the restart:', { archived: after.archived, pending: after.pending });

    // The key assertion: after the restart it does not re-enumerate from the
    // start, does not re-fetch what was archived, and carries on at the 3rd.
    // 🔴 W10 · `toEqual([])` became "exactly the continuation page", and that is
    //    the same criterion read more precisely: a tick reads one list page and
    //    the list is now closed by an **empty page** rather than by
    //    `offset >= total`, so the tick after the restart asks for offset=4 —
    //    the page after the ones tick 1 and tick 2 already read. A restart would
    //    have asked for offset=0 again, which is what this rules out.
    const listAfterRestart = server2.calls.filter((u) => u.includes('/conversations'));
    expect(listAfterRestart).toHaveLength(1);
    expect(new URL(listAfterRestart[0]!).searchParams.get('offset')).toBe('4');
    expect(detailCalls(server2.calls).map((u) => u.split('/').pop())).toEqual([UUIDS[2]]);
    expect(after.archived).toEqual([UUIDS[0], UUIDS[1], UUIDS[2]]);
  });
});

describe('C17 task 2 · counter-case 2: the host becomes unreachable mid-way (host-paused)', () => {
  it('the host is absent ⇒ it stops at once with the debts untouched; once the host is back it resumes from the breakpoint', async () => {
    await enableBackfill();
    const server = makeServer({ ids: UUIDS, total: 4, pageSize: 2 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);
    await bootAndDispatch(liveCapture());
    const beforeTrip = await stateOf();
    expect(beforeTrip.archived.length).toBe(1);

    // 🔴 W2 · Really create this pause: the host goes entirely offline and the next debt cannot be delivered.
    //    This is not a "download stall" but a **non-delivery** in §1's sense —
    //    and the only correct reaction is to keep the debt and pause by name, never to treat it as "done".
    hostDown = true;
    await bootAndDispatch(liveCapture());
    console.log('[C17-2.2] tick reason while the host is absent =', mod.lastBackfillTick()?.reason);
    const paused = await stateOf();
    console.log('[C17-2.2] debts after the pause:', { archived: paused.archived.length, pending: paused.pending.length });
    expect(mod.lastBackfillTick()?.reason).toBe('ran');
    expect(mod.lastBackfillTick()?.report?.stopped).toBe('host-unavailable');
    /**
     * 🔴 W18 · These two used to be `toEqual` against `beforeTrip`, and **both were
     *    vacuous**. `stateOf()` handed back the live object the fake store held,
     *    the engine mutated that same array in place, and so
     *    `beforeTrip.pending === paused.pending` was literally true — the assertion
     *    compared an array with itself and could not fail. (Verified on the
     *    pre-W18 code before changing anything: printing
     *    `beforeTrip.pending === paused.pending` there says `true`.)
     *
     *    It went red as soon as the accessor began returning a fresh snapshot,
     *    which is what an assertion is meant to compare. So the **premise** was
     *    wrong, not the engine: a tick legitimately reads one more list page while
     *    the host is down, and those newly named conversations are owed. What the
     *    criterion actually claims is asserted now, and only that:
     *      · not one debt that was owed stopped being owed;
     *      · nothing passed itself off as archived;
     *      · and none was judged dead.
     */
    expect(paused.pending).toEqual(expect.arrayContaining(beforeTrip.pending));
    expect(paused.archived).toEqual(beforeTrip.archived);            // not one passed itself off as a success
    expect(paused.failures ?? []).toEqual([]);                       // and none was judged dead

    // The pause state was written down (it is what the popup displays).
    const { HOST_PAUSE_KEY } = await import('../lib/host-status');
    expect(store[HOST_PAUSE_KEY]).toMatchObject({ reason: 'host-unavailable' });

    // The host is still absent ⇒ the pause continues and not one extra request is sent.
    const detailsBefore = detailCalls(server.calls).length;
    await bootAndDispatch(liveCapture());
    console.log('[C17-2.2] tick reason while the host is still absent =', mod.lastBackfillTick()?.reason);
    expect(mod.lastBackfillTick()?.reason).toBe('host-paused');
    expect(detailCalls(server.calls).length).toBe(detailsBefore);

    // The host comes back ⇒ the next heartbeat says hello first and only resumes on success; then it carries on from the breakpoint.
    hostDown = false;
    await bootAndDispatch(liveCapture());
    const resumed = await stateOf();
    console.log('[C17-2.2] after resuming:', { reason: mod.lastBackfillTick()?.reason, archived: resumed.archived });
    expect(mod.lastBackfillTick()?.reason).toBe('ran');
    expect(store[HOST_PAUSE_KEY]).toBeNull();                        // the pause was cleared
    expect(resumed.archived).toEqual([UUIDS[0], UUIDS[1]]);          // it carries on at the 2nd rather than starting over
  });
});

describe('C17 task 2 · counter-case 3: enumeration gives no total', () => {
  it('the progress text never contains % — and the instrument is first shown to be able to see the opposite', async () => {
    await enableBackfill();
    // (a) The instrument proves itself: with a total, the same check **does see** a percentage
    const withTotal = makeServer({ ids: UUIDS, total: 4, pageSize: 4 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(withTotal.port);
    await bootAndDispatch(liveCapture());
    const yes = mod.lastBackfillTick()!.report!.progress;
    console.log('[C17-2.3] instrument self-proof (with a total):', yes);
    expect(yes).toContain('%');

    // (b) The real counter-case: another scope, and the list gives no total
    const noTotal = makeServer({ ids: UUIDS, total: null, pageSize: 4 });
    mod.configureBackfillTransport(noTotal.port);
    await bootAndDispatch(liveCapture('acct-no-total'));
    const no = mod.lastBackfillTick()!.report!.progress;
    console.log('[C17-2.3] the real counter-case (no total):', no);
    expect(no).not.toContain('%');
    expect(no).toContain('total unknown');

    const s = await stateOf('acct-no-total');
    console.log('[C17-2.3] the state with no total:', { totalKnown: s.totalKnown, totalSource: s.totalSource });
    expect(s.totalSource).toBe('unknown');
  });
});

describe('C17 task 2 · counter-case 4: the same conversation enumerated twice', () => {
  it('it is not written down twice — and the instrument is first shown to be able to see a duplicate', async () => {
    await enableBackfill();
    // (a) The instrument proves itself: when the same conversation is really sent twice (the name identical byte for byte),
    //     finalWrites() **does see** two records with the same name.
    //     🔴 W2: payloads are deduplicated by content (sha256 is the primary key), so the **bytes** have to differ here.
    //     🔴 W6: and since W3 they have to differ **in the response body**, not merely in
    //     `capturedAt`. Two views of one conversation are now deliberately recognized as the
    //     same delivery (lib/recapture.ts) — which is exactly this test's claim in (b) — so a
    //     bumped timestamp no longer produces a second record. An edit to the body still does,
    //     and the name still does not change a character: it comes from the URL's session id.
    const mod: any = await import('../entrypoints/background');
    const dup = liveCapture();
    await bootAndDispatch(dup);
    const editedBody = JSON.parse(dup.text);
    editedBody.current_node = 'n1';   // a real content change; the session id, and so the name, is untouched
    await mod.handleCaptured({
      ...dup,
      capturedAt: dup.capturedAt + 1_000,
      text: JSON.stringify(editedBody),
    });
    const liveName = finalWrites().filter((f) => f.includes('aaaaaaaa-1111'));
    console.log('[C17-2.4] instrument self-proof (two different byte strings, one name):', liveName);
    expect(liveName.length).toBe(2);

    // (b) The real counter-case: page 0 stuffs UUIDS[0] in a second time
    host = createSyntheticHost({ up: true });
    const server = makeServer({ ids: UUIDS, total: 4, pageSize: 2, dupeOnPage: { page: 0, id: UUIDS[0]! } });
    mod.configureBackfillTransport(server.port);
    for (let i = 0; i < 4; i += 1) await bootAndDispatch(liveCapture());

    const s = await stateOf();
    console.log('[C17-2.4] the state when enumeration carries a duplicate:', { pending: s.pending, archived: s.archived });
    const dupFiles = finalWrites().filter((f) => f.includes(UUIDS[0]!));
    console.log('[C17-2.4] the final files for UUIDS[0]:', dupFiles);
    console.log('[C17-2.4] the number of detail requests for UUIDS[0]:',
      detailCalls(server.calls).filter((u) => u.endsWith(UUIDS[0]!)).length);

    expect(new Set(s.archived).size).toBe(s.archived.length);   // archived has no duplicates
    expect(dupFiles.length).toBe(1);                            // written down exactly once
    expect(detailCalls(server.calls).filter((u) => u.endsWith(UUIDS[0]!)).length).toBe(1);
  });
});

// ===========================================================================
// Task 3 · seam bugs (these cases **pin the current real behaviour**, wrong behaviour included)
// ===========================================================================
describe('C17 task 3 · seam A: are the debt key and the on-disk file name the same identity', () => {
  it('🔴 BUG-1, fixed in C20: the write-down is skipped (no sessionId) ⇒ the debt is **not cleared** and enters the failure list', async () => {
    await enableBackfill();
    // 'shortid' is a legal debt key (enumerate only requires a non-empty string),
    // but chatgpt's sessionIdPatterns is /backend-api/conversation/([0-9a-fA-F-]{8,}),
    // and 'shortid' is not hex ⇒ extractSessionId returns null ⇒ handleCaptured skips the write-down outright.
    const server = makeServer({ ids: ['shortid'], total: 1, pageSize: 4 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);
    await bootAndDispatch(liveCapture());

    const s = await stateOf();
    const files = finalWrites().filter((f) => f.includes('shortid'));
    console.log('[C17-3.A] detail requests:', detailCalls(server.calls));
    console.log('[C17-3.A] the files written down related to shortid:', files);
    console.log('[C17-3.A] debt state:', { pending: s.pending, archived: s.archived });
    console.log('[C17-3.A] failure list:', s.failures);
    console.log('[C17-3.A] progress text:', mod.lastBackfillTick()!.report!.progress);

    expect(detailCalls(server.calls).length).toBe(1);   // the body really was fetched
    expect(files.length).toBe(0);                       // not one byte was written down

    // Before C20: s.archived === ['shortid'] — never stored yet struck off, and progress even said 100%.
    // After C20: the sink says saved:false ⇒ no clearing, no passing off as archived, into the failure list, no retry.
    expect(s.archived).toEqual([]);
    expect(s.pending).toEqual([]);                      // and not left at the head of the queue spinning in place
    expect(s.failures).toEqual([
      { shortId: 'shortid', platform: 'chatgpt', reason: 'not-saved', at: expect.any(Number) },
    ]);
    // 🔴 The progress numerator no longer lies: 0 archived, not 100%.
    expect(mod.lastBackfillTick()!.report!.progress).not.toContain('100%');
    expect(mod.lastBackfillTick()!.report!.progress).toContain('Archived 0');
    expect(mod.lastBackfillTick()!.report!.failedThisRun).toHaveLength(1);
  });

  it('🔴 BUG-2, root-caused in C21: two debt keys can **no longer** collapse onto one file name', async () => {
    await enableBackfill();
    // Both ids start with the same hex-dash run, and sessionIdPatterns' greedy match stops there:
    //   'deadbeef01-zzz' and 'deadbeef01-yyy' both used to ⇒ sessionId 'deadbeef01-'
    // 🔴 After C21 the write-down side **no longer scrapes an identity out of the URL at all** (see lib/backfill/engine.ts's
    //    `sessionId: id` and entrypoints/background.ts's resolveSessionId),
    //    so however greedy that regex is, it no longer affects the file name.
    const ids = ['deadbeef01-zzz', 'deadbeef01-yyy'];
    const server = makeServer({ ids, total: 2, pageSize: 4 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);
    await bootAndDispatch(liveCapture());
    await bootAndDispatch(liveCapture());

    const s = await stateOf();
    const files = finalWrites().filter((f) => f.includes('deadbeef01'));
    console.log('[C17-3.A2] archived:', s.archived);
    console.log('[C17-3.A2] failure list:', s.failures);
    console.log('[C17-3.A2] the names handed over:', files);
    expect(files.length).toBe(2);                         // two sends
    expect(new Set(files).size).toBe(2);                  // 🔴 C21: two **different** names
    expect(files.sort()).toEqual([
      'chatgpt-deadbeef01-yyy.json',
      'chatgpt-deadbeef01-zzz.json',
    ]);

    // History:
    //  · Before C20: s.archived === ids — both struck off while only 1 file remained on disk,
    //    the conversation written first overwritten by the later one, and nobody the wiser.
    //  · C20: the sink reports back the identity it **actually named by**, the engine reconciles on the spot ⇒ a mismatch means no clearing.
    //    The overwrite still happened, only no longer silently — a treatment of the symptom.
    //  · 🔴 C21: the identity is **expressed once** ⇒ the overwrite itself disappears. Both debts clear normally,
    //    and the identity-mismatch branch can no longer be triggered on this path.
    expect(s.archived.sort()).toEqual([...ids].sort());
    expect(s.failures ?? []).toEqual([]);
  });
});

describe('C17 task 3 · seam B: which wins, the pacer or the pause / pacing while paused', () => {
  it('the pause outranks pacing: while paused, gate() is never called once', async () => {
    await enableBackfill();
    const server = makeServer({ ids: UUIDS, total: 4, pageSize: 4 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);
    await bootAndDispatch(liveCapture());

    // The host goes offline ⇒ a real host-unavailable pause is created (not a hand-made status flag).
    hostDown = true;
    await bootAndDispatch(liveCapture());
    expect(store['cs_native_host_pause_v1']).toMatchObject({ reason: 'host-unavailable' });

    const detailsBefore = detailCalls(server.calls).length;
    await bootAndDispatch(liveCapture());
    // schedule.ts stops it before runBackfill ⇒ a Pacer is not even constructed.
    console.log('[C17-3.B] the tick while paused:', mod.lastBackfillTick());
    expect(mod.lastBackfillTick()?.reason).toBe('host-paused');
    expect(mod.lastBackfillTick()?.report).toBeNull();
    expect(detailCalls(server.calls).length).toBe(detailsBefore);
  });

  it('🔴 BUG-3, fixed in C19: the 20s body-fetch minimum interval takes effect **across ticks**', async () => {
    await enableBackfill();
    const server = makeServer({ ids: UUIDS, total: 4, pageSize: 4 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    const traces: Array<{ enumerate: number[]; detail: number[] }> = [];
    for (let i = 0; i < 4; i += 1) {
      await bootAndDispatch(liveCapture());
      traces.push(mod.lastBackfillTick()!.report!.paceTrace);
    }
    console.log('[C17-3.B2] the paceTrace of each tick:', traces);
    console.log('[C17-3.B2] total fake-clock advance after 4 ticks (ms):', fakeNow - 1_700_000_000_000);

    // Before C19: every trace was [0] (each runBackfill constructed a new Pacer with lastAt starting at null)
    //           ⇒ 4 bodies fetched back to back at zero interval.
    // After C19: only the first is 0 (there really is no "previous one"), and every later one makes up the full 20 seconds —
    //           the moment of the last fetch lives in state.lastFetchAt, surviving ticks and restarts.
    expect(traces[0]!.detail).toEqual([0]);
    /**
     * 🔴 W10 · Tick 2's body wait is 18,000 rather than 20,000, and it is not a
     *    loosened interval — it is the same full interval, split between the two
     *    segments by the clock. That tick also reads the **empty page that
     *    confirms the list is finished** (the `offset >= total` stopping
     *    condition is gone: a real account reported total=901 while holding
     *    7,391 conversations), and the enumeration gate makes up its 2,000 ms
     *    before the body gate does. 2,000 + 18,000 = the full 20,000, and the
     *    body interval is still measured from the **persisted** anchor across
     *    ticks, which is the whole point of BUG-3's fix. Both waits are pinned so
     *    the split cannot drift unnoticed.
     */
    expect(traces[1]!.enumerate).toEqual([2_000]);
    expect(traces[1]!.detail).toEqual([18_000]);
    // Ticks 3 and 4 have no page left to read ⇒ the body interval is made up in full.
    for (const t of traces.slice(2)) expect(t.enumerate).toEqual([]);
    for (const t of traces.slice(2)) expect(t.detail).toEqual([20_000]);
    // All 4 bodies were still fetched; they are just spread over 60 seconds (3 intervals × 20 seconds).
    expect(detailCalls(server.calls).length).toBe(4);
    expect(fakeNow - 1_700_000_000_000).toBe(60_000);
  });
});

describe('C17 task 3 · seam C: the progress denominator comes from enumeration and the numerator from elsewhere', () => {
  it('🔴 W2: when the host is unreachable the tick has a **named outcome** that neither clears the debt nor judges it dead', async () => {
    await enableBackfill();
    const server = makeServer({ ids: UUIDS, total: 4, pageSize: 4 });
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    // First let enumeration finish and clear 1 debt (normal)
    await bootAndDispatch(liveCapture());
    const ok = await stateOf();
    expect(ok.archived.length).toBe(1);

    // Now the host goes offline ⇒ the next one cannot be delivered.
    // 🔴 This case's ancestor is C17-3.C "BUG-4": back then a sink that threw was swallowed whole,
    //    with neither a halt nor a trace (the debt was still there, but the ledger showed nothing had happened).
    //    Since W2 the sink does not throw, it **answers** — and the answer has a named third outcome:
    //    retryLater ⇒ the debt stays, and this leg stops with a trace as 'host-unavailable'.
    hostDown = true;
    await bootAndDispatch(liveCapture());

    const after = await stateOf();
    console.log('[C17-3.C] when the host is unreachable:', {
      archived: after.archived.length, pending: after.pending.length,
      halted: after.halted, lastTickReason: mod.lastBackfillTick()?.reason,
      stopped: mod.lastBackfillTick()?.report?.stopped,
    });
    // The debt was not cleared (no data lost) — the side that matches the BUG-4 era.
    expect(after.archived.length).toBe(1);
    expect(after.pending[0]).toBe(UUIDS[1]);
    // The side that **differs** from the BUG-4 era: this tick's outcome is visible at runtime,
    // and it is named rather than a leftover from "the last success".
    expect(mod.lastBackfillTick()?.reason).toBe('ran');
    expect(mod.lastBackfillTick()?.report?.stopped).toBe('host-unavailable');
    expect(after.halted).toBeNull();          // this leg did not break itself; no halt may be left
    expect(after.failures ?? []).toEqual([]); // and this item may not be judged dead either
  });
});

// ===========================================================================
// Task 4 · does it really run at runtime
// ===========================================================================
describe('C17 task 4 · the runtime', () => {
  it('🔴 the production fact: no production code injected an http port until C19 ⇒ this chain never reached runBackfill in a browser', async () => {
    await enableBackfill();                       // the user has even turned the switch on explicitly
    const mod: any = await import('../entrypoints/background');
    // configureBackfillTransport is **not** called — this is the real production state
    await bootAndDispatch(liveCapture());
    console.log('[C17-4] the tick with no transport injected:', mod.lastBackfillTick());
    expect(mod.lastBackfillTick()?.reason).toBe('no-http-port');
    expect(stateKeys()).toEqual([]);              // the debt set was never created at all
  });
});
