/**
 * C20 · A write-down failure goes into the failure list; the debt is never quietly struck off.
 *
 * The fix the product owner decided on (option C):
 *   **A failed write-down is not retried, but it is moved into a failure list the user can see.**
 *   The reason: "losing something" and "losing something but knowing about it" are two entirely
 *   different things, and this project exists for the second one.
 *
 * Three cases:
 *  1. the sink succeeds ⇒ the debt is cleared and the failure list is empty;
 *  2. 🔴 the sink fails ⇒ the debt is **not cleared**, it goes into the failure list, and the popup wording changes;
 *  3. the failure list hits its cap ⇒ drop the oldest and record how many were dropped (never a silent truncation).
 *
 * Cases 1 and 2 both start from the **real production entry point**: runtime.onMessage('chat-captured')
 *  → handleCaptured → kickBackfill → tickBackfill → runBackfill → sink。
 * Zero real network and zero logged-in state throughout: the http port is a pure function in this
 * file, and the write-down channel is a synthetic native host (tests/synthetic-native-host.ts).
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';
import { headerOf, type BackfillState } from '../lib/backfill/types';

// ---------------------------------------------------------------------------
// A fake browser (isomorphic to c17). Storage survives resetModules = the model of a "browser restart".
// W2: the write-down channel = a synthetic native host (it honours a matching ack only).
// ---------------------------------------------------------------------------
const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
let host: SyntheticHost;

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    sendNativeMessage: (h: string, m: unknown) => host.sendNativeMessage(h, m),
  },
  storage: {
    local: {
      async get(query: Record<string, unknown> | null) {
        if (query === null) return { ...store };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(query)) out[k] = k in store ? store[k] : query[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
};

/** The names the host really received and acked. */
const deliveredFiles = (): string[] => host.names();

/** A synthetic "server". It never touches the network: just a pure (url) => {status,text} function. */
function makeServer(ids: string[], total: number | null = ids.length) {
  const calls: string[] = [];
  const port = async (url: string) => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === '/backend-api/conversations') {
      const offset = Number(u.searchParams.get('offset') ?? '0');
      const items = ids.slice(offset).map((id) => ({ id, title: 'synthetic' }));
      const body: Record<string, unknown> = { items };
      if (total !== null) body.total = total;
      return { status: 200, text: JSON.stringify(body) };
    }
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

function liveCapture(): CapturedFetch {
  const sid = 'aaaaaaaa-1111-2222-3333-444444444444';
  return {
    url: `https://chatgpt.com/backend-api/conversation/${sid}`,
    method: 'GET',
    status: 200,
    text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: 'acct-fixture-1' }),
    pageUrl: `https://chatgpt.com/c/${sid}`,
    capturedAt: 1_700_000_000_000,
  };
}

let fakeNow = 1_700_000_000_000;
const fakeClock = { now: () => fakeNow, sleep: async (ms: number) => { fakeNow += ms; } };

async function bootAndDispatch(payload: CapturedFetch): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({ clock: fakeClock });
  if (runtimeListeners.length === 0) await mod.default();
  await new Promise<any>((resolve) => {
    runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
  });
  await mod.backfillTickSettled();
  return mod;
}

/**
 * 🔴 W18 · The debt set as the engine reads it back: the header at `stateKey(...)`
 *    plus the ids from the debt store (lib/backfill/ledger.ts). It cannot be a
 *    property read of one `storage.local` key any more — the ids are not in it.
 */
async function stateOf(): Promise<BackfillState> {
  const { browserLocalStore } = await import('../lib/backfill/store');
  const { loadState } = await import('../lib/backfill/engine');
  const st = browserLocalStore();
  if (!st) throw new Error('this suite runs against a fake browser with storage.local; it must not be null');
  return await loadState(st, 'chatgpt', 'acct-fixture-1');
}

/** Render once down the popup's real chain (no second renderer is written). */
async function popupNow(): Promise<{ view: any; text: string }> {
  const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
  const { isBackfillEnabled, tickBlockReason } = await import('../lib/backfill/schedule');
  const { renderPopup, popupText, pickBackfillState, collectFailures } =
    await import('../lib/popup-view');

  const st = browserLocalStore();
  const snapshot = await browserLocalSnapshot();
  const enabled = await isBackfillEnabled(st);
  const state = pickBackfillState(snapshot);
  const block = await tickBlockReason({
    hasStore: st !== null,
    isEnabled: () => enabled,
    // This case does not create a host pause (that is w2-backfill-host.test.ts's job).
    isHostPaused: () => false,
    // In this case the fetch channel is an injected explicit transport ⇒ the popup sees this as true.
    hasHttp: true,
  });
  const view = renderPopup({
    enabled, block, state,
    target: state ? { platform: state.platform, scope: state.scope } : null,
    failures: collectFailures(snapshot),
  });
  return { view, text: popupText(view) };
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  host = createSyntheticHost({ up: true });
  (globalThis as any).indexedDB = new IDBFactory();
  fakeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
});

// ===========================================================================
// Case 1 · the sink succeeds
// ===========================================================================
describe('C20-1 · the sink succeeds ⇒ the debt is cleared and the failure list is empty', () => {
  it('through the real entry point: the one that was stored goes into archived, and failures holds nothing', async () => {
    // A legal uuid ⇒ extractSessionId can pull it out ⇒ handleCaptured returns saved:true,
    // and the identity it names by equals the debt key byte for byte ⇒ the consistency check passes too.
    const id = 'b1111111-0000-4000-8000-000000000001';
    const server = makeServer([id]);
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    await bootAndDispatch(liveCapture());

    const s = await stateOf();
    const files = deliveredFiles();
    console.log('[C20-1] the final files written down:', files.filter((f) => f.includes('b1111111')));
    console.log('[C20-1] debt ledger:', { pending: s.pending, archived: s.archived, failures: s.failures });
    console.log('[C20-1] progress text:', mod.lastBackfillTick()!.report!.progress);

    expect(files.some((f) => f.includes(id))).toBe(true);   // it really was written out
    expect(s.archived).toEqual([id]);                        // the debt was cleared
    expect(s.pending).toEqual([]);
    expect(s.failures).toEqual([]);                          // 🔴 the failure list is empty — and it really is `[]`, not absent
    expect(s.failuresDropped).toBe(0);
    expect(mod.lastBackfillTick()!.report!.failedThisRun).toEqual([]);

    const { view, text } = await popupNow();
    console.log('[C20-1] the whole popup:\n' + text);
    expect(view.failures).toBeNull();                        // no failures ⇒ the line does not appear
    expect(view.clearFailures.visible).toBe(false);
  });
});

// ===========================================================================
// Case 2 · 🔴 the sink fails (this one had to go red first)
// ===========================================================================
describe('C20-2 · the sink fails ⇒ the debt is not cleared, it enters the failure list, and the popup wording changes', () => {
  it('🔴 no sessionId can be extracted ⇒ handleCaptured returns saved:false ⇒ not one byte was written, and the debt may not be struck off', async () => {
    // 'shortid' is a legal debt key (the list API only requires a non-empty string), but chatgpt's
    // sessionIdPatterns is /backend-api/conversation/([0-9a-fA-F-]{8,}),
    // and 'shortid' is not hex ⇒ extractSessionId returns null ⇒ handleCaptured returns
    // { saved:false, reason:'no-session-id ...' } outright and writes no file at all.
    const server = makeServer(['shortid']);
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    await bootAndDispatch(liveCapture());

    const s = await stateOf();
    const files = deliveredFiles();
    console.log('[C20-2] detail requests:', server.calls.filter((u) => u.includes('/conversation/')));
    console.log('[C20-2] files written down related to shortid:', files.filter((f) => f.includes('shortid')));
    console.log('[C20-2] debt ledger:', { pending: s.pending, archived: s.archived });
    console.log('[C20-2] 🔴 a real sample failure-list entry:', JSON.stringify(s.failures ?? [], null, 2));
    console.log('[C20-2] progress text:', mod.lastBackfillTick()!.report!.progress);

    // (a) the body really was fetched, but not one byte was written down
    expect(server.calls.filter((u) => u.includes('/conversation/')).length).toBe(1);
    expect(files.filter((f) => f.includes('shortid'))).toEqual([]);

    // (b) 🔴 the debt is **not cleared** — it does not enter archived, so the progress numerator cannot lie
    expect(s.archived).toEqual([]);
    expect(mod.lastBackfillTick()!.report!.progress).toContain('Archived 0');
    expect(mod.lastBackfillTick()!.report!.progress).not.toContain('100%');

    // (c) 🔴 it enters the failure list
    // 🔴 W18 · `failures` is optional on the state type (older records have none), so
    //    the reads below name the list once, as `[]` when there is none — the same
    //    "absent means empty" rule `failuresOf` implements for the popup.
    const failureList = s.failures ?? [];
    expect(failureList).toHaveLength(1);
    expect(failureList[0]!.shortId).toBe('shortid');
    expect(failureList[0]!.reason).toBe('not-saved');
    expect(failureList[0]!.platform).toBe('chatgpt');
    expect(typeof failureList[0]!.at).toBe('number');
    // 🔴 Self-evidently storing nothing sensitive: the entry has only these four fields, no body, no URL.
    expect(Object.keys(failureList[0]!).sort()).toEqual(['at', 'platform', 'reason', 'shortId']);
    const blob = JSON.stringify(failureList);
    expect(blob).not.toContain('synthetic body');        // no conversation body
    expect(blob).not.toContain('http');                  // no URL at all
    expect(blob).not.toContain('chatgpt.com');

    // (d) 🔴 no retry: it is not queued in pending, and the next tick will not pick it back up
    expect(s.pending).toEqual([]);
    const detailsBefore = server.calls.filter((u) => u.includes('/conversation/')).length;
    await bootAndDispatch(liveCapture());
    const s2 = await stateOf();
    console.log('[C20-2] after one more kick:', {
      detailCalls: server.calls.filter((u) => u.includes('/conversation/')).length,
      failures: (s2.failures ?? []).length, archived: s2.archived,
    });
    expect(server.calls.filter((u) => u.includes('/conversation/')).length).toBe(detailsBefore);
    expect(s2.failures ?? []).toHaveLength(1);           // no retry, and no double entry

    // (e) 🔴 the popup wording changes — it may not be shown as all fine
    const { view, text } = await popupNow();
    console.log('[C20-2] 🔴 the whole popup (with failures):\n' + text);
    expect(view.failures).not.toBeNull();
    expect(view.failures).toContain('were NOT stored');
    expect(view.failures).toContain('will NOT be retried automatically');
    expect(view.failures).toContain('1 past conversation(s)');
    expect(view.clearFailures.visible).toBe(true);
    expect(text).toContain('shortid…');                  // the short id appears in the details
    expect(text).not.toContain('synthetic body');        // but the body never appears
    expect(text).not.toContain('https://');              // and no full URL appears either

    // (f) after "acknowledge / clear" the list is empty, and **no re-fetch was triggered**
    const { backfillStateEntries, collectFailures } = await import('../lib/popup-view');
    const { clearFailures } = await import('../lib/backfill/failures');
    const { browserLocalStore, browserLocalSnapshot } = await import('../lib/backfill/store');
    const st = browserLocalStore()!;
    for (const { key, state } of backfillStateEntries(await browserLocalSnapshot())) {
      clearFailures(state);
      await st.save(key, state);
    }
    const after = collectFailures(await browserLocalSnapshot());
    console.log('[C20-2] the summary after clearing:', after);
    expect(after).toEqual({ entries: [], dropped: 0 });
    expect(server.calls.filter((u) => u.includes('/conversation/')).length).toBe(detailsBefore);
    expect((await popupNow()).view.failures).toBeNull();
  });
});

// ===========================================================================
// Case 3 · the cap
// ===========================================================================
describe('C20-3 · the failure list hitting its cap ⇒ drop the oldest and record how many were dropped', () => {
  it('55 failures: the list keeps the newest 50, dropped = 5, and the wording says so', async () => {
    const { runBackfill } = await import('../lib/backfill/engine');
    const { memoryStore } = await import('../lib/backfill/store');
    const { loadState } = await import('../lib/backfill/engine');
    const { MAX_FAILURES } = await import('../lib/backfill/failures');
    const { renderPopup, popupText, NO_FAILURES } = await import('../lib/popup-view');

    const n = MAX_FAILURES + 5;
    const ids = Array.from({ length: n }, (_, i) => `fail-${String(i).padStart(3, '0')}`);
    const server = makeServer(ids);
    const st = memoryStore();
    let now = 1_700_000_000_000;

    const report = await runBackfill({
      platform: 'chatgpt',
      origin: 'https://chatgpt.com',
      scope: 'cap-fixture',
      store: st,
      http: server.port,
      clock: { now: () => (now += 1_000), sleep: async () => {} },
      pace: {
        enumerate: { minIntervalMs: 0, maxPerDay: null },
        detail: { minIntervalMs: 0, maxPerDay: null },
      },
      maxDetails: n,
      // Every single one reports outright that nothing was stored.
      sink: async () => ({ saved: false, reason: 'synthetic failure' }),
    });

    // 🔴 W18 · Through the production load path: the ids are in the debt store now.
    const state = await loadState(st, 'chatgpt', 'cap-fixture');
    // `failures` is optional on the state type (a record written before C20 has none);
    // the ledger's list is `[]` in that case, which is the same rule `failuresOf` uses.
    const failureList = state.failures ?? [];
    console.log('[C20-3] cap:', MAX_FAILURES, '· fed in:', n);
    console.log('[C20-3] list length:', failureList.length, '· dropped:', state.failuresDropped);
    console.log('[C20-3] the first two entries:', failureList.slice(0, 2));
    console.log('[C20-3] the last two entries:', failureList.slice(-2));

    expect(report.stopped).toBe('queue-empty');
    expect(state.archived).toEqual([]);                        // not one passed itself off as a success
    expect(state.pending).toEqual([]);                         // and none was left behind for a retry
    expect(failureList).toHaveLength(MAX_FAILURES);
    expect(state.failuresDropped).toBe(5);
    // 🔴 The oldest are dropped: what remains is fail-005 .. fail-054 (the newest 50).
    expect(failureList[0]!.shortId).toBe('fail-005');
    expect(failureList[MAX_FAILURES - 1]!.shortId).toBe('fail-054');

    // 🔴 Never a silent truncation: the wording must say "5 older one(s) are no longer on it".
    // 🔴 W18 · The popup renders what storage holds, which is the header — `headerOf`
    //    is the production function that produces exactly that record, so the view is
    //    built from the same shape the popup will really be handed.
    const view = renderPopup({
      enabled: true, block: null, state: headerOf(state),
      target: { platform: 'chatgpt', scope: 'cap-fixture' },
      failures: { entries: failureList, dropped: state.failuresDropped ?? 0 },
    });
    console.log('[C20-3] the popup failure line:', view.failures);
    expect(view.failures).toContain(`at most ${MAX_FAILURES}`);
    expect(view.failures).toContain('5 older one(s)');
    expect(popupText(view)).toContain('will NOT be retried automatically');

    // The instrument proves itself: the same renderer **does not show** this line with an empty list.
    expect(renderPopup({
      enabled: true, block: null, state: headerOf(state),
      target: { platform: 'chatgpt', scope: 'cap-fixture' }, failures: NO_FAILURES,
    }).failures).toBeNull();
  });
});
