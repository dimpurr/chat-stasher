/**
 * C21 · One conversation should have exactly one identity.
 *
 * The root cause (not the symptom): one conversation's identity was **expressed twice**
 * along the chain —
 *   1. the debt key       = the list API's items[].id (lib/backfill/enumerate.ts:64-68)
 *   2. the on-disk name   = scraped out of the URL **a second time** (lib/contract.ts:485's
 *      extractSessionId → entrypoints/background.ts's
 *      `${platform}-${sanitizePathSegment(...)}`).
 * Two lossy functions (a regex truncation / a character replacement) sit between the two
 * expressions, so **two different debt keys can collapse onto one file name** and the later
 * write overwrites the earlier one.
 *
 * This file pins three things:
 *  1. 🔴 two different conversation ids must end up with different on-disk names after the
 *     whole chain (red first).
 *  2. 🔴 an id that cannot produce a safe file name is never forced through: it goes into the
 *     failure list and never collapses onto someone else's name.
 *  3. 🔴 the live leg's existing behaviour is unchanged, character for character (it has no
 *     "id from the enumerator" and can only scrape the URL).
 *
 * Zero real network and zero logged-in state throughout: the http port is a pure function in
 * this file, and the write-down channel is a synthetic native host.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';
import type { BackfillState } from '../lib/backfill/types';

// ---------------------------------------------------------------------------
// A fake browser (isomorphic to c17 / c20). W2: the write-down channel = a synthetic native host,
// and "what was actually written" is decided by the name/payload the host received.
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

/** The live leg's one: it comes from a page response and has **no** id from the enumerator. */
const LIVE_SID = 'aaaaaaaa-1111-2222-3333-444444444444';
function liveCapture(): CapturedFetch {
  return {
    url: `https://chatgpt.com/backend-api/conversation/${LIVE_SID}`,
    method: 'GET',
    status: 200,
    text: JSON.stringify({ mapping: {}, current_node: 'n0', account_id: 'acct-fixture-1' }),
    pageUrl: `https://chatgpt.com/c/${LIVE_SID}`,
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
 *    plus the ids from the debt store (lib/backfill/ledger.ts). A property read of
 *    one `storage.local` key cannot do this any more — the ids are not in it.
 */
async function stateOf(): Promise<BackfillState> {
  const { browserLocalStore } = await import('../lib/backfill/store');
  const { loadState } = await import('../lib/backfill/engine');
  const st = browserLocalStore();
  if (!st) throw new Error('this suite runs against a fake browser with storage.local; it must not be null');
  return await loadState(st, 'chatgpt', 'acct-fixture-1');
}

/** The name that really landed on the host (§6.2's name). */
const finalFiles = (): string[] => host.names();

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  host = createSyntheticHost({ up: true });
  // A brand-new empty database per case: the outbox is persistent, and leftovers across cases would distort the assertions.
  (globalThis as any).indexedDB = new IDBFactory();
  fakeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest, setBackfillEnabled } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
});

// ===========================================================================
// Case 1 · 🔴 two different debt keys ⇒ two different file names (red first)
// ===========================================================================
describe('C21-1 · two different conversation ids must end up with different on-disk names after the whole chain', () => {
  it('🔴 the regex-truncation kind: the id tail is outside [0-9a-fA-F-], so the old chain collapsed the two onto one name', async () => {
    // Both debt keys are legal ids for the list API (it only requires a non-empty string), with the same prefix and different tails.
    // The old chain: extractSessionId scraped again with /backend-api/conversation/([0-9a-fA-F-]{8,})
    //        ⇒ 'Z' is outside the character set ⇒ both truncated to 'aaaaaaaa-bbbb' ⇒ one file name.
    const idA = 'aaaaaaaa-bbbb';
    const idB = 'aaaaaaaa-bbbbZZZZ';
    const server = makeServer([idA, idB]);
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    // One tick clears exactly 1 debt (DEFAULT_TICK_DETAILS=1), so kick it twice.
    await bootAndDispatch(liveCapture());
    await bootAndDispatch(liveCapture());

    const s = await stateOf();
    // Look only at the files these two debts wrote (the live leg's own is separate; see case 3).
    const debtFiles = finalFiles().filter((f) => f.includes('aaaaaaaa-bbbb'));
    console.log('[C21-1] the two debt keys:', [idA, idB]);
    console.log('[C21-1] detail requests:', server.calls.filter((u) => u.includes('/conversation/')));
    console.log('[C21-1] final files written by these two debts:', debtFiles);
    console.log('[C21-1] ledger:', { pending: s.pending, archived: s.archived, failures: s.failures });

    // (a) both really had their body fetched
    expect(server.calls.filter((u) => u.includes(encodeURIComponent(idA))).length).toBeGreaterThan(0);
    expect(server.calls.filter((u) => u.includes(encodeURIComponent(idB))).length).toBeGreaterThan(0);

    // (b) 🔴 this change's criterion: the two file names must be **different**
    expect(debtFiles).toHaveLength(2);
    expect(new Set(debtFiles).size).toBe(2);

    // (c) the file name must carry **the debt key itself**, not the truncated one
    expect(debtFiles.some((f) => f.endsWith(`chatgpt-${idA}.json`))).toBe(true);
    expect(debtFiles.some((f) => f.endsWith(`chatgpt-${idB}.json`))).toBe(true);

    // (d) both debts were settled with no failure at all (the identity was never re-derived, so it cannot mismatch)
    expect(s.archived.sort()).toEqual([idA, idB].sort());
    expect(s.failures ?? []).toEqual([]);
  });
});

// ===========================================================================
// Case 2 · an id that cannot produce a safe file name: leave a trace rather than collapse onto another's name
// ===========================================================================
describe('C21-2 · a file-name-unsafe id ⇒ nothing written, it goes into the failure list', () => {
  it('two ids differing only in the character sanitize would remove: not one file may be written', async () => {
    // sanitizePathSegment replaces both ' ' and '/' with '_' ⇒ these two ids used to collapse onto one name.
    const idA = 'aaaaaaaa bbbbbbbb';
    const idB = 'aaaaaaaa/bbbbbbbb';
    const server = makeServer([idA, idB]);
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(server.port);

    await bootAndDispatch(liveCapture());
    await bootAndDispatch(liveCapture());

    const s = await stateOf();
    // 🔴 The live leg's own (LIVE_SID) does not count — this looks only at what these two debt keys wrote.
    const debtFiles = finalFiles().filter((f) => !f.includes(LIVE_SID));
    console.log('[C21-2] the two debt keys:', [idA, idB]);
    console.log('[C21-2] files written:', debtFiles);
    console.log('[C21-2] failure list:', JSON.stringify(s.failures ?? [], null, 2));

    expect(debtFiles).toEqual([]);        // not one byte written ⇒ collapsing is impossible
    expect(s.archived).toEqual([]);       // and not one of them passes itself off as a success
    expect(s.failures ?? []).toHaveLength(2);
    expect((s.failures ?? []).every((f) => f.reason === 'not-saved')).toBe(true);
  });
});

// ===========================================================================
// Case 3 · 🔴 the live leg's existing behaviour is unchanged
// ===========================================================================
describe('C21-3 · the live leg (with no id from the enumerator) is unchanged character for character', () => {
  it('it still scrapes the id from the URL, still uses the same name, and still answers saved:true (and only true after an ack)', async () => {
    const mod: any = await import('../entrypoints/background');
    mod.configureBackfillTransport(undefined);   // the backfill leg is not wired ⇒ only the live leg's path remains
    const payload = liveCapture();
    const result = await new Promise<any>(async (resolve) => {
      if (runtimeListeners.length === 0) await mod.default();
      runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
    });
    await mod.backfillTickSettled();

    console.log('[C21-3] what the live leg returned:', result);
    console.log('[C21-3] the name the live leg handed over:', finalFiles());

    expect(result.ok).toBe(true);
    expect(result.saved).toBe(true);
    // 🔴 The same identity as before C20: the platform prefix + the sessionId scraped from the URL.
    //    W2 removed the `chat-stasher/inbox/` prefix — that was chrome.downloads' directory convention,
    //    and the name is now defined by §6.2 (it became the shard's source_file on the host).
    expect(result.finalName).toBe(`chatgpt-${LIVE_SID}.json`);
    expect(result.sessionId).toBe(LIVE_SID);
    expect(finalFiles()).toEqual([`chatgpt-${LIVE_SID}.json`]);
    // And this name was **acked**: the host really received it.
    expect(host.deliveries).toHaveLength(1);
    expect(host.sessionIds()).toEqual([LIVE_SID]);
  });

  it('🔴 a page may **not** specify its own identity: any page payload carrying a sessionId is rejected', async () => {
    const { isCapturedFetchShape } = await import('../lib/contract');
    const clean = liveCapture();
    console.log('[C21-3b] the clean payload passes the check:', isCapturedFetchShape(clean));
    expect(isCapturedFetchShape(clean)).toBe(true);
    // If a page could stuff in a sessionId, it could choose which file name is written — that must be blocked.
    const spoofed = { ...clean, sessionId: '../../../etc/passwd' };
    console.log('[C21-3b] the sessionId-carrying payload passes the check:', isCapturedFetchShape(spoofed));
    expect(isCapturedFetchShape(spoofed)).toBe(false);
  });
});
