/**
 * W50b · **A remembered delivery names the destination that acknowledged it.**
 *
 * ## The defect this covers
 *
 * W50 gave both legs the same guard: a fingerprint recorded after a matching ack,
 * keyed by delivery name and body content. Nothing in that record said *where* the
 * copy went. So the store could only answer "this content is stored" when the
 * honest question — the one the guard's `saved: true` actually claims, because the
 * engine settles a debt on it (engine.ts `sinkVerdict` → `settleDebt`) — is "this
 * content is stored **here**".
 *
 * Point the native host at another stage while browser storage survives, and every
 * record still stands. On the backfill leg a relist then marks a conversation
 * archived having delivered nothing to the new archive; on the live leg a re-view
 * answers `status:'unchanged'` the same way. Both legs read the same predicate and
 * write through the same function, so both are covered here.
 *
 * ## What is asserted, and why each case needs the others
 *
 *  · **same host ⇒ skip.** The positive control. Without it, "changed host ⇒
 *    deliver" would pass on a guard that had simply stopped skipping, and the
 *    one-extra-copy cost W50b accepts would be paid on every capture for nothing.
 *  · **changed host ⇒ deliver.** The defect itself. The only thing that differs
 *    between this case and the first is the `stage` the host reports.
 *  · **legacy entry ⇒ deliver.** A record written before W50b is a bare fingerprint
 *    string. It must be read as *not delivered* — never upgraded, because nothing on
 *    disk says which stage those bytes reached. The seeded value is asserted to be
 *    the fingerprint the guard really computes, so this case cannot pass merely
 *    because the test seeded a value that could never have matched.
 *
 * ## Level
 *
 * Both legs run through the **real background entry point**
 * (`runtime.onMessage('chat-captured')`), the real engine, the real debt ledger and
 * the real write-down exit — the shape of tests/w50-backfill-dedupe.test.ts, whose
 * relist helpers this file reuses. The only swapped parts are `browser.*`, the http
 * port and the native host. Zero network, zero logged-in state, and every body
 * below is synthetic.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';
import type { HttpResponse } from '../lib/backfill/engine';
import type { BackfillState } from '../lib/backfill/types';
import { recoverLedgerLoss } from '../lib/backfill/ledger';
import { replaceDebtSet } from '../lib/backfill/debt-store';
import { LAST_DELIVERED_KEY, contentFingerprint } from '../lib/recapture';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

// ---------------------------------------------------------------------------
// The fake extension surface (the pattern of tests/w50-backfill-dedupe.test.ts).
// Storage **survives resetModules** — that is the model of a browser restart, and
// it is also what lets a later pass read the records an earlier pass wrote, which
// is the whole subject of this file.
// ---------------------------------------------------------------------------

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
let host: SyntheticHost;

const fakeBrowser: any = {
  runtime: {
    id: 'w50b-delivery-destination',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    sendNativeMessage: (h: string, m: unknown) => host.sendNativeMessage(h, m),
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
// A synthetic chatgpt.com. A pure `(url) => response` function; nothing here
// touches the network, and no conversation text is anybody's.
// ---------------------------------------------------------------------------

const ORIGIN = 'https://chatgpt.com';
/** The account axis (ADR-002): the scope the debt set is keyed by. */
const SCOPE = 'acct-synthetic-1';
const LIST_PATH = '/backend-api/conversations';
const DETAIL_PREFIX = '/backend-api/conversation/';

/**
 * One ChatGPT-shaped response. `mapping` / `current_node` / `conversation_id` are
 * the stable part — which is what the fingerprint is computed over — and
 * `safeUrls` is the field measured to differ between two copies of one
 * conversation (lib/recapture.ts's header), so it can vary per fetch without
 * changing the fingerprint. That is what makes a *relist* the right second pass:
 * the bytes differ, the content does not.
 */
function chatgptBody(id: string, opts: { safeUrls?: string[] } = {}): string {
  return JSON.stringify({
    conversation_id: id,
    current_node: 'node-1',
    safe_urls: opts.safeUrls ?? [],
    mapping: {
      'node-0': { id: 'node-0', message: { content: { parts: ['synthetic question'] } } },
      'node-1': { id: 'node-1', message: { content: { parts: ['synthetic answer text'] } } },
    },
    account_id: SCOPE,
  });
}

interface Server {
  port: (url: string) => Promise<HttpResponse>;
  /** Every list URL asked for, in order. */
  listUrls: string[];
  /** Every body URL asked for, in order — the evidence that a fetch really happened. */
  detailUrls: string[];
}

/** `body(id, fetchNumber)` sees the 1-based count of fetches for that id. */
function makeServer(ids: string[], body: (id: string, fetchNumber: number) => string, pageSize = 4): Server {
  const listUrls: string[] = [];
  const detailUrls: string[] = [];
  const fetches = new Map<string, number>();
  const port = async (url: string): Promise<HttpResponse> => {
    const u = new URL(url);
    if (u.pathname === LIST_PATH) {
      listUrls.push(url);
      const offset = Number(u.searchParams.get('offset') ?? '0');
      const slice = ids.slice(offset, offset + pageSize);
      return {
        status: 200,
        text: JSON.stringify({ items: slice.map((id) => ({ id, title: 'synthetic' })), total: ids.length }),
      };
    }
    const id = decodeURIComponent(u.pathname.slice(DETAIL_PREFIX.length));
    detailUrls.push(url);
    const n = (fetches.get(id) ?? 0) + 1;
    fetches.set(id, n);
    return { status: 200, text: body(id, n) };
  };
  return { port, listUrls, detailUrls };
}

// ---------------------------------------------------------------------------
// Driving the real entry point.
// ---------------------------------------------------------------------------

let fakeNow = 1_700_000_000_000;
const fakeClock = {
  now: () => fakeNow,
  sleep: async (ms: number) => { fakeNow += ms; },
};

/** The live leg's own capture. Its job is to kick a backfill tick. */
function liveCapture(): CapturedFetch {
  const sid = 'aaaaaaaa-1111-2222-3333-444444444444';
  return {
    url: `${ORIGIN}${DETAIL_PREFIX}${sid}`,
    method: 'GET',
    status: 200,
    text: chatgptBody(sid),
    pageUrl: `${ORIGIN}/c/${sid}`,
    capturedAt: fakeNow,
  };
}

/** Load background and install the seams. Idempotent within one test. */
async function boot(server?: Server): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  if (server) mod.configureBackfillTransport(server.port);
  mod.configureBackfillPace({ clock: fakeClock, random: () => 0 });
  if (runtimeListeners.length === 0) await mod.default();
  return mod;
}

/** Dispatch one capture through the real `chat-captured` message and await the tick. */
async function dispatch(mod: any, capture: CapturedFetch): Promise<any> {
  const responded = await new Promise<any>((resolve) => {
    const ret = runtimeListeners[0]!({ type: 'chat-captured', payload: capture }, { id: 's' }, resolve);
    expect(ret).toBe(true);
  });
  await mod.backfillTickSettled();
  return responded;
}

/** Boot, then reach the backfill engine once. */
async function bootAndDispatch(server?: Server): Promise<any> {
  const mod = await boot(server);
  await dispatch(mod, liveCapture());
  return mod;
}

/** The debt set of this scope, read back through the same loader the engine uses. */
async function stateOf(scope = SCOPE): Promise<BackfillState> {
  const { loadState } = await import('../lib/backfill/engine');
  const { browserLocalStore } = await import('../lib/backfill/store');
  const st = browserLocalStore();
  if (!st) throw new Error('this suite runs against a fake browser with storage.local; it must not be null');
  return await loadState(st, 'chatgpt', scope);
}

/**
 * Run the product's own relist: the debt store loses its rows (the W45 ledger
 * loss) and the real `recoverLedgerLoss` resets the enumeration cursor, so the next
 * tick reads the conversation list from the start and owes the conversation again.
 */
async function relist(): Promise<void> {
  const { browserLocalStore } = await import('../lib/backfill/store');
  const st = browserLocalStore();
  if (!st) throw new Error('no store');
  expect(await replaceDebtSet('chatgpt', SCOPE, { pending: [], archived: [], nextSeq: 1 })).toBe(true);
  const recovery = await recoverLedgerLoss(st, 'chatgpt', SCOPE, fakeNow);
  expect(recovery.ok).toBe(true);
}

/** The names the host acknowledged for these conversations — the only evidence of "stored" (§1). */
function deliveriesFor(ids: readonly string[]): string[] {
  return host.names().filter((n) => ids.some((id) => n.includes(id)));
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  host = createSyntheticHost({ up: true, stage: '/stage/a', machine: 'm1' });
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

const A = 'c1111111-0000-4000-8000-00000000000a';
const NAME_A = `chatgpt-${A}.json`;

// ===========================================================================
// 1 · the destination is what decides, and the other two cases prove it
// ===========================================================================

describe('W50b · the backfill leg answers "stored" only for the destination that acknowledged it', () => {
  it('🔴 the same host, relisted: nothing is stored a second time (the positive control)', async () => {
    const server = makeServer([A], (id, n) => chatgptBody(id, { safeUrls: [`https://files.example/${n}`] }));

    // ---- pass 1: first-seen ⇒ delivered, and the record names host /stage/a ----
    await bootAndDispatch(server);
    expect(deliveriesFor([A])).toHaveLength(1);
    expect((await stateOf()).archived).toEqual([A]);

    await relist();

    // ---- pass 2: a relist, against the same host ----
    await bootAndDispatch(server);

    // The body really was fetched again — so this is not "the tick did nothing".
    expect(server.detailUrls.filter((u) => u.includes(A))).toHaveLength(2);
    // 🔴 And nothing was stored a second time.
    expect(deliveriesFor([A])).toHaveLength(1);
    const s2 = await stateOf();
    expect(s2.archived).toEqual([A]);
    expect(s2.pending).toEqual([]);
    expect(s2.failures ?? []).toEqual([]);
  });

  it('🔴 the host moved to another stage: the relist IS delivered, not marked archived', async () => {
    const server = makeServer([A], (id, n) => chatgptBody(id, { safeUrls: [`https://files.example/${n}`] }));

    // ---- pass 1: delivered to /stage/a ----
    const stageA = host;
    await bootAndDispatch(server);
    expect(deliveriesFor([A])).toHaveLength(1);

    await relist();

    // 🔴 The one thing that changes: the host now writes somewhere else. Browser
    //    storage is untouched — the same records the first pass wrote are still
    //    there, which is precisely the situation the defect needs.
    host = createSyntheticHost({ up: true, stage: '/stage/b', machine: 'm1' });

    // ---- pass 2 ----
    await bootAndDispatch(server);

    // 🔴 Delivered again, and to the **new** stage. Under W50 this was answered
    //    `saved:true` from the stored fingerprint alone, so the debt settled with
    //    nothing reaching /stage/b and the count below would be 0.
    //    The two hosts are counted separately because swapping the instance swaps
    //    its delivery log with it; "1 and 1" is the same claim as "2, at two
    //    stages", and it is the one that stays true if a future bug sends pass 2
    //    back to the old stage.
    expect(deliveriesFor([A])).toHaveLength(1);
    expect(host.names().filter((n) => n.includes(A))).toHaveLength(1);
    // Pass 1's copy stayed where it was sent; nothing was re-sent to /stage/a.
    expect(stageA.names().filter((n) => n.includes(A))).toHaveLength(1);

    const s2 = await stateOf();
    expect(s2.archived).toEqual([A]);
    expect(s2.pending).toEqual([]);
    expect(s2.failures ?? []).toEqual([]);
  });

  it('🔴 a record written before W50b is delivered again, never read as stored', async () => {
    const body = chatgptBody(A, { safeUrls: ['https://files.example/1'] });
    const fingerprint = await contentFingerprint('chatgpt', body);
    // 🔴 If this were null the case below would be vacuous — the guard would skip the
    //    record because there was nothing to compare, not because it lacked a
    //    destination. Asserted so the case cannot pass for the wrong reason.
    expect(typeof fingerprint).toBe('string');

    // The pre-W50b shape: a bare fingerprint string under the delivery name.
    store[LAST_DELIVERED_KEY] = { [NAME_A]: fingerprint };

    await bootAndDispatch(makeServer([A], () => body));

    // 🔴 Delivered, even though the value on disk is the exact fingerprint the guard
    //    computes: nothing there says which stage those bytes reached, so "stored"
    //    cannot be claimed of this one.
    expect(deliveriesFor([A])).toHaveLength(1);
    const s1 = await stateOf();
    expect(s1.archived).toEqual([A]);
    expect(s1.pending).toEqual([]);
  });
});

// ===========================================================================
// 2 · the live leg had the same hole, and is fixed by the same code
// ===========================================================================

describe('W50b · the live leg answers "unchanged" only for the destination that acknowledged it', () => {
  it('🔴 the same host: the second view is unchanged; a moved host: it is delivered again', async () => {
    // The backfill leg is not what this case is about, and the live-leg kick starts
    // a tick — so it is given an empty conversation list and stays out of the way.
    const mod = await boot(makeServer([], () => ''));
    const capture = liveCapture();

    // ---- view 1: the conversation reaches the host ----
    const first = await dispatch(mod, capture);
    expect(first).toMatchObject({ saved: true, status: 'delivered' });
    expect(host.deliveries).toHaveLength(1);

    // ---- view 2, same body, same host: unchanged, and nothing sent ----
    const second = await dispatch(mod, capture);
    expect(second).toMatchObject({ saved: true, status: 'unchanged' });
    expect(host.deliveries).toHaveLength(1);

    // 🔴 The host moves. The stored record — written by view 1 against /stage/a —
    //    is still there, and is now a statement about a destination that is no
    //    longer the one in use.
    host = createSyntheticHost({ up: true, stage: '/stage/b', machine: 'm1' });

    // ---- view 3: the same bytes, and this time they must actually be sent ----
    const third = await dispatch(mod, capture);
    expect(third).toMatchObject({ saved: true, status: 'delivered' });
    expect(host.deliveries).toHaveLength(1);
  });
});
