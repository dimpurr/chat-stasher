/**
 * W50 · **The backfill leg answered "stored" without ever asking whether it
 * already had this conversation.**
 *
 * ## The defect
 *
 * `handleCaptured` (the live leg) has judged every capture against
 * `isUnchangedSinceDelivery` since W3, and records the fingerprint only after a
 * matching ack. `deliverBackfillItem` did neither: it prepared a payload and
 * delivered it. So the whole recapture layer — `lib/recapture.ts`, written for the
 * measured case below — was unreachable from the leg whose trigger *is* a
 * re-listing, and a relist stored every conversation again in full.
 *
 * The TODO's note that the fix is not trivial is right, and this file is built
 * around the reason: the live leg's record point is guarded by
 * `lookup.entry === null` (only a matching ack *deletes* an outbox entry), and the
 * backfill leg deliberately does not go through the outbox (§10) — so it has no
 * such entry to observe disappearing. The record has to come from the backfill
 * leg's own ack. These tests fail if anyone "fixes" it by routing the backfill leg
 * through the outbox (the debts and the outbox would both hold one conversation)
 * or by skipping the delivery outright (a genuinely changed conversation would
 * never be archived).
 *
 * ## Why the scenario is a relist, and what "unchanged" means here
 *
 * Measured 2026-09-14 (lib/recapture.ts's header): ChatGPT returns the whole
 * conversation on every view, and two copies of one conversation differed **only**
 * in the top-level `safe_urls` field. The fingerprint is the sha256 of the
 * response with the known volatile fields removed, so the two copies share a
 * fingerprint while their **bytes differ** — which is why the host's own content
 * addressing (§6.2, sha256 of the payload) does not dedupe them and why the guard
 * has to exist above it.
 *
 * A relist is the product's own documented path for "an already-archived
 * conversation is listed and fetched again": `lib/backfill/ledger.ts:723-725` says
 * it in as many words for the ledger-loss recovery. So the second pass here drops
 * the debt store's rows and runs the real `recoverLedgerLoss`, which resets the
 * enumeration cursor and starts a new pass — and the server then answers the same
 * conversations with different `safe_urls`, exactly like the live platform.
 *
 * The body **fetch** on that second pass is expected and unavoidable (both the
 * file name and the fingerprint can only be computed from the body); the second
 * **copy** is the thing under test.
 *
 * ## Level
 *
 * Every case goes through the **real background entry point**
 * (`runtime.onMessage('chat-captured')`), the real engine, the real debt ledger
 * and the real write-down exit, swapping only `browser.*`, the http port and the
 * native host — the same shape as tests/c17-backfill-e2e.test.ts. Zero network,
 * zero logged-in state, and every body below is synthetic.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';
import type { HttpResponse } from '../lib/backfill/engine';
import { stateKey, type BackfillState } from '../lib/backfill/types';
import { recoverLedgerLoss } from '../lib/backfill/ledger';
import { replaceDebtSet } from '../lib/backfill/debt-store';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

// ---------------------------------------------------------------------------
// The fake extension surface (the pattern of tests/c17-backfill-e2e.test.ts).
// Storage **survives resetModules** — that is the model of a browser restart, and
// it is also what lets the second pass read the state the first pass wrote.
// ---------------------------------------------------------------------------

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
let host: SyntheticHost;

const fakeBrowser: any = {
  runtime: {
    id: 'w50-backfill-dedupe',
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
 * the stable part; `safeUrls` is the field measured to differ between two copies of
 * one conversation, and `answer` is a **real** change used by the "changed" case.
 */
function chatgptBody(
  id: string,
  opts: { safeUrls?: string[]; answer?: string } = {},
): string {
  return JSON.stringify({
    conversation_id: id,
    current_node: 'node-1',
    safe_urls: opts.safeUrls ?? [],
    mapping: {
      'node-0': { id: 'node-0', message: { content: { parts: ['synthetic question'] } } },
      'node-1': {
        id: 'node-1',
        message: { content: { parts: [opts.answer ?? 'synthetic answer text'] } },
      },
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

/**
 * `body(id, fetchNumber)` is called with the 1-based count of times this id's body
 * has been fetched, so a test can make the second pass answer differently — which
 * is exactly what the live platform does (`safe_urls`).
 */
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

/** The live leg's own capture. Its only job here is to kick a backfill tick. */
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

/** Load background, install the seams, dispatch one live capture, await the tick. */
async function bootAndDispatch(server?: Server): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  if (server) mod.configureBackfillTransport(server.port);
  mod.configureBackfillPace({ clock: fakeClock, random: () => 0 });
  if (runtimeListeners.length === 0) await mod.default();
  const responded = await new Promise<any>((resolve) => {
    const ret = runtimeListeners[0]!({ type: 'chat-captured', payload: liveCapture() }, { id: 's' }, resolve);
    expect(ret).toBe(true);
  });
  await mod.backfillTickSettled();
  return { mod, responded };
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
 * Run the product's own relist: the debt store loses its rows (the W45 ledger loss
 * — "the ids are gone, so `archived` is gone with them"), and the real
 * `recoverLedgerLoss` resets the enumeration cursor so the next tick reads the
 * conversation list from the start.
 *
 * 🔴 The debt set is emptied with the **product's** writer (`replaceDebtSet`, the
 *    same call `ledger.ts` makes) and the recovery is the **product's** function,
 *    so the state the next tick sees is a state the product really reaches. What
 *    this file does *not* do is reach into the guard.
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

const A = 'c1111111-0000-4000-8000-00000000000a';
const B = 'c2222222-0000-4000-8000-00000000000b';

// ===========================================================================
// 1 · first-seen is delivered, and then a relist does not store it again
// ===========================================================================

describe('W50 · a relist of an unchanged conversation is skipped, and the skew is a changed field', () => {
  it('🔴 first pass stores it, the relist fetches it again and stores nothing', async () => {
    const server = makeServer([A], (id, n) => chatgptBody(id, {
      // The real platform's behaviour: the same conversation, different `safe_urls`
      // on every view. The bytes differ; the fingerprint does not.
      safeUrls: [`https://files.example/${n}`],
    }));

    // ---- pass 1: first-seen ⇒ delivered ----
    await bootAndDispatch(server);
    const s1 = await stateOf();
    expect(s1.archived).toEqual([A]);
    expect(s1.pending).toEqual([]);
    expect(deliveriesFor([A])).toHaveLength(1);
    expect(host.deliveries.find((d) => d.name.includes(A))!.status).toBe('stored');

    // ---- relist: the debt set loses its rows, the cursor resets ----
    await relist();

    // ---- pass 2: the same conversation, relisted ----
    await bootAndDispatch(server);
    const s2 = await stateOf();

    // The body really was fetched a second time — this is not "the tick did
    // nothing". The list was read from the start and the id was owed again.
    expect(server.listUrls.map((u) => new URL(u).searchParams.get('offset'))).toEqual(['0', '0']);
    expect(server.detailUrls.filter((u) => u.includes(A))).toHaveLength(2);

    // 🔴 And nothing was stored a second time.
    expect(deliveriesFor([A])).toHaveLength(1);

    // 🔴 Counted as archived, not as failed: the record says this conversation is
    //    in the archive under this name, and that is what the debt ledger should
    //    say. `saved:false` would have put an archived conversation into the
    //    failure list and out of `pending` for good.
    expect(s2.archived).toEqual([A]);
    expect(s2.pending).toEqual([]);
    expect(s2.failures ?? []).toEqual([]);
  });

  it('🔴 a conversation whose body really changed IS delivered again', async () => {
    const server = makeServer([A], (id, n) => chatgptBody(id, {
      // Pass 2 changes a **stable** field. `safe_urls` alone would be deleted before
      // comparing (that is the whole point of the volatile table), so the change has
      // to be somewhere the fingerprint can see it — otherwise this test would pass
      // for the wrong reason.
      safeUrls: [`https://files.example/${n}`],
      answer: n === 1 ? 'synthetic answer text' : 'synthetic answer text (edited)',
    }));

    await bootAndDispatch(server);
    expect(deliveriesFor([A])).toHaveLength(1);

    await relist();
    await bootAndDispatch(server);

    const s2 = await stateOf();
    expect(server.detailUrls.filter((u) => u.includes(A))).toHaveLength(2);
    // The edit went to the archive: two deliveries, the second `stored` too (the
    // payload bytes differ, so the host's own content addressing does not absorb it).
    const mine = host.deliveries.filter((d) => d.name.includes(A));
    expect(mine).toHaveLength(2);
    expect(mine.map((d) => d.status)).toEqual(['stored', 'stored']);
    expect(s2.archived).toEqual([A]);
    expect(s2.failures ?? []).toEqual([]);
  });

  it('🔴 in one relisted page, the unchanged id is skipped and the never-seen id is delivered', async () => {
    // Pass 1 sees only A; pass 2 sees A and B.
    const server = makeServer([A], (id, n) => chatgptBody(id, { safeUrls: [`https://files.example/${n}`] }));

    await bootAndDispatch(server);
    expect(deliveriesFor([A, B])).toHaveLength(1);

    await relist();
    // B joins the list for the second pass — the same relist, one row heavier.
    const grown = makeServer([A, B], (id, n) => chatgptBody(id, { safeUrls: [`https://files.example/${n}`] }));
    await bootAndDispatch(grown);

    // A was fetched again and skipped; B was fetched for the first time and stored.
    expect(deliveriesFor([A])).toHaveLength(1);
    expect(deliveriesFor([B])).toHaveLength(1);
    const s2 = await stateOf();
    expect([...s2.archived].sort()).toEqual([A, B].sort());
    expect(s2.pending).toEqual([]);
    expect(s2.failures ?? []).toEqual([]);
  });
});

// ===========================================================================
// 2 · the direction the guard must never take: a platform with no volatile
//     table is not skipped on the strength of a name it has seen before
// ===========================================================================

describe('W50 · "no fingerprint" is unknown, and an unknown is never skipped', () => {
  it('🔴 deepseek has no volatile-field table, so the same conversation is delivered every time', async () => {
    // 🔴 This case is deliberately at the sink, called through the real export, and
    //    not through the engine: making it a full engine case would need a
    //    deepseek list plan, and the property under test — "a null fingerprint is
    //    answered by delivering" — is decided entirely inside the sink. It is the
    //    one that would break invariant 1 if someone later "optimised" the guard
    //    into "this name was delivered before".
    const { deliverBackfillItem } = await import('../entrypoints/background');
    const sid = 'd0d0d0d0-1111-4222-8333-9a0b0c0d0e0f';
    const capture = (): CapturedFetch => ({
      url: `https://chat.deepseek.com/api/v0/chat/session/${sid}`,
      method: 'POST',
      status: 200,
      text: JSON.stringify({ session_id: sid, message: { content: 'synthetic' } }),
      capturedAt: fakeNow,
    });

    expect(await deliverBackfillItem(capture())).toMatchObject({ saved: true });
    expect(await deliverBackfillItem(capture())).toMatchObject({ saved: true });

    // Both went to the host, under the same name. `contentFingerprint` returns null
    // for deepseek (no registered volatile fields, lib/recapture.ts), and null is
    // "we cannot tell", never "unchanged".
    expect(deliveriesFor([sid])).toHaveLength(2);
    expect(host.deliveries.map((d) => d.name)).toEqual([
      `deepseek-${sid}.json`,
      `deepseek-${sid}.json`,
    ]);
  });
});

// ===========================================================================
// 3 · the record is written only on an acknowledged delivery
// ===========================================================================

describe('W50 · the fingerprint is written down only after an ack', () => {
  it('🔴 a host that refuses the delivery records nothing, so the next attempt still goes out', async () => {
    const sid = 'c3333333-0000-4000-8000-00000000000c';
    const capture = (): CapturedFetch => ({
      url: `${ORIGIN}${DETAIL_PREFIX}${sid}`,
      method: 'GET',
      status: 200,
      text: chatgptBody(sid),
      capturedAt: fakeNow,
    });

    // The host is up but nacks this delivery: nothing was stored, so nothing may be
    // recorded — a fingerprint remembered here would skip the conversation forever
    // and silently lose it (the first invariant, and the one W3's own header calls
    // out as the worst of the three).
    const { deliverBackfillItem } = await import('../entrypoints/background');
    const refused = createSyntheticHost({
      up: true,
      nack: { kind: 'bad-request', retryable: false, detail: 'synthetic refusal' },
    });
    const realHost = host;
    host = refused;
    const first = await deliverBackfillItem(capture());
    expect(first.saved).toBe(false);
    expect(first.retryLater).toBeUndefined();
    expect(refused.names()).toHaveLength(0);

    // The host is fine again: the same conversation must be delivered, not skipped.
    host = realHost;
    expect(await deliverBackfillItem(capture())).toMatchObject({ saved: true });
    expect(deliveriesFor([sid])).toHaveLength(1);
  });
});
