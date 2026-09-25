/**
 * W50c · **"Is this already stored?" is answered by the host, from its stage.**
 *
 * ## The defect this covers
 *
 * W50 gave both legs the same guard: a fingerprint recorded after a matching ack,
 * keyed by delivery name and body content. W50b then added the stage and machine the
 * host reported, so the record could say "stored, and *there*". Two review rounds
 * found the same thing underneath both: the answer came from **extension memory
 * about an archive**, and memory cannot see the event that matters. Replace or
 * restore the archive at the same path — same `stage`, same `machine`, every record
 * still standing — and a relist settles a conversation as archived having delivered
 * nothing to the archive that is there now. On the backfill leg that means the debt
 * is cleared and the engine moves on; the conversation is in no archive anywhere.
 *
 * W50c put the question to the host: §6.6 `has`, answered by scanning the stage the
 * host is writing to *now*. This file drives that through the real delivery path.
 *
 * ## What is asserted, and why each case needs the others
 *
 *  · **same host, same stage ⇒ skip.** The positive control. Without it, every
 *    "⇒ deliver" case below would pass on a guard that had simply stopped skipping.
 *  · **the archive replaced at the same path ⇒ deliver.** The review's finding. The
 *    stage keeps its path and its machine; only its contents are gone. Nothing in
 *    extension storage changes, which is exactly the situation that defeated both
 *    earlier designs.
 *  · **the host moved to another stage ⇒ deliver.** W50b's case, kept: the record
 *    was written against one archive and the host now writes to another.
 *  · **an older host that does not know `has` ⇒ deliver.** The fallback. Its `nack`
 *    is not an answer, so nothing may be skipped on it.
 *  · **a record written before W50c ⇒ the host is still asked.** Both older shapes of
 *    the record are read for the one thing they say ("this was delivered once"), so
 *    the host decides instead of the shape deciding.
 *  · **another conversation's copy ⇒ deliver.** The answer is scoped to the
 *    conversation whose directory the shard was sealed under.
 *
 * ## Level
 *
 * Both legs run through the **real background entry point**
 * (`runtime.onMessage('chat-captured')`), the real engine, the real debt ledger and
 * the real write-down exit — the shape of tests/w50-backfill-dedupe.test.ts, whose
 * relist helpers this file reuses. The only swapped parts are `browser.*`, the http
 * port and the native host. Zero network, zero logged-in state, and every body
 * below is synthetic.
 *
 * 🔴 The synthetic host's `has` answers from **its own stage model**, never from
 *    anything the extension wrote (tests/synthetic-native-host.ts). A stub that read
 *    the extension's record would let this whole file pass with the defect open.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { readFileSync } from 'node:fs';
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
// 1 · the stage decides, and the other cases prove it
// ===========================================================================

describe('W50c · the backfill leg settles "stored" only on the host\'s own answer', () => {
  it('🔴 the same host, relisted: nothing is stored a second time (the positive control)', async () => {
    const server = makeServer([A], (id, n) => chatgptBody(id, { safeUrls: [`https://files.example/${n}`] }));

    // ---- pass 1: first-seen ⇒ delivered, and the shard records the fingerprint ----
    await bootAndDispatch(server);
    expect(deliveriesFor([A])).toHaveLength(1);
    expect((await stateOf()).archived).toEqual([A]);
    const afterFirst = host.hasCount();

    await relist();

    // ---- pass 2: a relist, against the same host and the same stage ----
    await bootAndDispatch(server);

    // The body really was fetched again — so this is not "the tick did nothing".
    expect(server.detailUrls.filter((u) => u.includes(A))).toHaveLength(2);
    // 🔴 And nothing was stored a second time — because the host was asked and said
    //    it held it. Asserted as a question that was really put, not merely as the
    //    absence of a delivery: a guard that had stopped asking would also deliver
    //    nothing if it skipped from memory, and that is the design this replaced.
    expect(host.hasCount()).toBeGreaterThan(afterFirst);
    expect(deliveriesFor([A])).toHaveLength(1);
    const s2 = await stateOf();
    expect(s2.archived).toEqual([A]);
    expect(s2.pending).toEqual([]);
    expect(s2.failures ?? []).toEqual([]);
  });

  it('🔴 the archive replaced at the same path: the relist IS delivered, not marked archived', async () => {
    const server = makeServer([A], (id, n) => chatgptBody(id, { safeUrls: [`https://files.example/${n}`] }));

    // ---- pass 1: delivered, and the record written ----
    await bootAndDispatch(server);
    expect(deliveriesFor([A])).toHaveLength(1);
    expect((await stateOf()).archived).toEqual([A]);
    // The record really is on disk — the situation the defect needs. Asserted
    // shape-agnostically on purpose: which shape it takes is not this test's subject
    // (the shapes are tested in tests/w3-recapture.test.ts), and a shape here would
    // turn the case below into a test of the record's spelling.
    expect(store[LAST_DELIVERED_KEY]).toHaveProperty([NAME_A]);

    await relist();

    // 🔴 The one thing that changes: the archive at `/stage/a` is gone. Same stage
    //    path, same machine, same extension storage, same records — a replaced,
    //    restored or rebuilt archive. Under W50 **and** W50b the stored record still
    //    matched, so this relist settled the debt without delivering anything; the
    //    conversation would be `archived` in the ledger and in no archive anywhere.
    host.replaceStage();

    // ---- pass 2 ----
    await bootAndDispatch(server);

    // 🔴 Delivered again, into the archive that is actually there. Under W50 and
    //    W50b this was answered `saved:true` from the stored record alone, so the
    //    debt settled with nothing reaching `/stage/a` and this count would be 1.
    expect(deliveriesFor([A])).toHaveLength(2);
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

    // 🔴 The host now writes somewhere else. Browser storage is untouched — the same
    //    records the first pass wrote are still there.
    host = createSyntheticHost({ up: true, stage: '/stage/b', machine: 'm1' });

    // ---- pass 2 ----
    await bootAndDispatch(server);

    // Delivered again, and to the **new** stage. Under W50 this was answered
    // `saved:true` from the stored fingerprint alone; under W50b the destination
    // comparison caught it, and under W50c it is caught because the host is asked
    // about the stage it is writing to.
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

  it('🔴 an older host that does not know `has`: the relist is delivered', async () => {
    const server = makeServer([A], (id, n) => chatgptBody(id, { safeUrls: [`https://files.example/${n}`] }));

    // Pass 1 against a host that answers `has` normally, so the record really is
    // written. (It is *not* asked anything yet: a first-time capture has no record
    // to be worth a round trip — that is the pre-gate, and it is asserted here so a
    // future change that asks on every capture shows up rather than passing.)
    await bootAndDispatch(server);
    expect(deliveriesFor([A])).toHaveLength(1);
    expect(host.hasCount()).toBe(0);

    await relist();

    // 🔴 The host is *downgraded* to one that predates §6.6 but still holds the
    //    stage — its `ack` would still be honoured, and only the question is
    //    missing. §6.3's `bad-request` is what such a host answers an unknown `type`
    //    with, and it is not an answer to the question.
    host = createSyntheticHost({ up: true, stage: '/stage/a', machine: 'm1', unsupported: true });

    await bootAndDispatch(server);

    // 🔴 Delivered. "We could not ask" must never become "it is already there", which
    //    is the one direction of this error that loses a conversation instead of
    //    copying it. And the question really was put — asserted, because "did not
    //    ask" would also deliver nothing and would be the wrong reason to pass.
    //
    //    Counted per conversation rather than as a total: the live capture this boot
    //    dispatched is a *second* question (its own record now exists, so its re-view
    //    asks about itself), and a bare total would conflate the two.
    expect(host.requests().filter((r) => r.type === 'has' && r.session_id === A)).toHaveLength(1);
    expect(deliveriesFor([A])).toHaveLength(1);
    const s2 = await stateOf();
    expect(s2.archived).toEqual([A]);
    expect(s2.pending).toEqual([]);
  });

  it('🔴 a record in either older shape is read, and the host — not the shape — decides', async () => {
    const body = chatgptBody(A, { safeUrls: ['https://files.example/1'] });
    const fingerprint = await contentFingerprint('chatgpt', body);
    // 🔴 If this were null the cases below would be vacuous — nothing would be
    //    answered at all. Asserted so they cannot pass for the wrong reason.
    expect(typeof fingerprint).toBe('string');

    // ---- a record written before W50c, against a stage that does hold the content
    // ---- ⇒ the host is asked, and answers that it holds it ⇒ unchanged.
    await bootAndDispatch(makeServer([A], () => body));
    expect(deliveriesFor([A])).toHaveLength(1);
    await relist();
    // Rewrite the record into the pre-W50b shape: a bare fingerprint string.
    store[LAST_DELIVERED_KEY] = { [NAME_A]: fingerprint };
    await bootAndDispatch(makeServer([A], () => body));
    expect(deliveriesFor([A])).toHaveLength(1);
    expect((await stateOf()).archived).toEqual([A]);
    expect((await stateOf()).failures ?? []).toEqual([]);

    // ---- the same record, against a stage that does not ⇒ delivered.
    // 🔴 This is the half that proves the record is only a pre-gate: the value on
    //    disk is still the exact fingerprint the guard computes, and it is not
    //    allowed to answer for the archive.
    await relist();
    host.replaceStage();
    await bootAndDispatch(makeServer([A], () => body));
    expect(deliveriesFor([A])).toHaveLength(2);
    expect((await stateOf()).archived).toEqual([A]);
  });
});

// ===========================================================================
// 2 · the live leg had the same hole, and is fixed by the same code
// ===========================================================================

describe('W50c · the live leg answers "unchanged" only on the host\'s own answer', () => {
  it('🔴 the same host: the second view is unchanged; a replaced archive: it is delivered again', async () => {
    // The backfill leg is not what this case is about, and the live-leg kick starts
    // a tick — so it is given an empty conversation list and stays out of the way.
    const mod = await boot(makeServer([], () => ''));
    const capture = liveCapture();

    // ---- view 1: the conversation reaches the host ----
    const first = await dispatch(mod, capture);
    expect(first).toMatchObject({ saved: true, status: 'delivered' });
    expect(host.deliveries).toHaveLength(1);

    // ---- view 2, same body, same stage: unchanged, and nothing sent ----
    const second = await dispatch(mod, capture);
    expect(second).toMatchObject({ saved: true, status: 'unchanged' });
    expect(host.deliveries).toHaveLength(1);

    // 🔴 The archive at the same path is replaced. The stored record — written by
    //    view 1 — is still there, and is now a statement about content the archive
    //    no longer holds.
    host.replaceStage();

    // ---- view 3: the same bytes, and this time they must actually be sent ----
    const third = await dispatch(mod, capture);
    expect(third).toMatchObject({ saved: true, status: 'delivered' });
    expect(host.deliveries).toHaveLength(2);
  });

  it('🔴 an older host: the second view is delivered rather than answered `unchanged`', async () => {
    const mod = await boot(makeServer([], () => ''));
    const capture = liveCapture();

    // View 1 against a host that knows `has`, so the stage holds the conversation
    // and the record is written.
    expect(await dispatch(mod, capture)).toMatchObject({ saved: true, status: 'delivered' });

    // 🔴 The host is replaced by one that predates §6.6, at the same stage.
    host = createSyntheticHost({ up: true, stage: '/stage/a', machine: 'm1', unsupported: true });

    // Both views below must go out: the question cannot be asked, so nothing may be
    // skipped. A `nack` read as "held" would lose every re-view of every
    // conversation on a host that is merely older than the extension.
    expect(await dispatch(mod, capture)).toMatchObject({ saved: true, status: 'delivered' });
    expect(host.deliveries).toHaveLength(1);
    expect(await dispatch(mod, capture)).toMatchObject({ saved: true, status: 'delivered' });
    expect(host.deliveries).toHaveLength(2);
  });
});

// ===========================================================================
// 3 · the hand-written validator and the committed schema are one contract
//
// `native-host.ts` validates responses by hand (§1's rule: the code path that
// decides whether data is safe to forget pulls in no JSON-schema library), and
// `contracts/nativehost-message.schema.json` is what the host is held to. Two
// hand-written halves of one contract is exactly the shape that drifts — and on
// this message the drift is silent: a response the extension fails to recognise
// becomes "malformed", which every caller reads as "not held" ⇒ deliver. So the
// shape the extension accepts is checked here against the committed schema file
// itself rather than against this suite's stub, which would agree with any
// mistake by construction.
//
// The request half is checked the same way: the fields sent are the fields the
// schema names and no others. The host ignores request fields it does not define
// (§6), so an unused one would sit there unnoticed.
// ===========================================================================

const SCHEMA = JSON.parse(
  readFileSync(new URL('../../../contracts/nativehost-message.schema.json', import.meta.url), 'utf8'),
) as { $defs: Record<string, any> };

/**
 * One `has()` call against a response this file chooses, plus the request that was
 * actually sent.
 *
 * 🔴 `response` is built **from the request**, because §1 makes `request_id`
 *    load-bearing: an answer carrying a different one is not an answer, so a fixed
 *    `request_id` here would make every case below test the echo check instead of
 *    the shape under test.
 */
async function askWith(
  response:
    | Record<string, unknown>
    | ((request: Record<string, unknown>) => Record<string, unknown>),
): Promise<{ answer: { ok: boolean; held?: boolean; reason?: string }; sent: Array<Record<string, unknown>> }> {
  const sent: Array<Record<string, unknown>> = [];
  const sendNativeMessage = (_host: string, message: Record<string, unknown>) => {
    sent.push(message);
    return Promise.resolve(typeof response === 'function' ? response(message) : response);
  };
  vi.stubGlobal('chrome', { runtime: { id: 'w50c-schema-check', sendNativeMessage } });
  vi.stubGlobal('browser', { runtime: { id: 'w50c-schema-check', sendNativeMessage } });
  const { has } = await import('../lib/native-host');
  const answer = await has(
    { platform: 'chatgpt', sessionId: 's', fingerprint: 'a'.repeat(64) },
    { timeoutMs: 100 },
  );
  return { answer: answer as { ok: boolean; held?: boolean; reason?: string }, sent };
}

/** §6.6's complete success response, spelled the way the schema spells it. */
const hasOk = (over: Record<string, unknown> = {}) =>
  (request: Record<string, unknown>): Record<string, unknown> => ({
    protocol: 1, type: 'has', ok: true, request_id: request.request_id,
    held: true, shard: '000001.jsonl', ...over,
  });

describe('W50c · §6.6 as the schema writes it and as the extension reads it', () => {
  it('🔴 the response fields the validator requires are exactly the schema\'s', async () => {
    const def = SCHEMA.$defs.hasResponse;
    expect(def, 'the committed schema must describe §6.6').toBeDefined();
    expect(def.additionalProperties, 'an extra response field must stay a red test').toBe(false);
    const required: string[] = def.required;
    // The exact set, not a superset: a field the schema requires and the validator
    // ignores would be a response the extension only half-reads.
    expect([...required].sort()).toEqual(['held', 'ok', 'protocol', 'request_id', 'shard', 'type']);

    // The whole shape is accepted...
    expect((await askWith(hasOk())).answer).toMatchObject({ ok: true, held: true });

    // ...and each required field, left out, is a malformed response rather than a
    // defaulted one. `held` absent in particular must never read as `false`.
    for (const field of required) {
      const { answer } = await askWith((request) => {
        const body = hasOk()(request);
        delete body[field];
        return body;
      });
      expect(answer.ok, `a response without \`${field}\` must not be accepted`).toBe(false);
      expect(answer.reason).toBe('malformed-response');
    }

    // An extra field is malformed too — that is what `additionalProperties: false`
    // means on the wire.
    expect((await askWith(hasOk({ extra: 1 }))).answer).toMatchObject({
      ok: false, reason: 'malformed-response',
    });

    // 🔴 `shard` is `string | null`, and the two wrong-type cases are the ones a
    //    hand-written check drops: `null` is a real value (`held: false`) and must
    //    not be confused with "absent", and a number is neither.
    expect((await askWith(hasOk({ held: false, shard: null }))).answer).toMatchObject({
      ok: true, held: false,
    });
    expect((await askWith(hasOk({ shard: 7 }))).answer).toMatchObject({ ok: false });
    expect((await askWith(hasOk({ held: 'yes' }))).answer).toMatchObject({ ok: false });

    // 🔴 And the answer is believed only for the request that was sent (§1): a
    //    `held: true` under someone else's `request_id` is not an answer to us, and
    //    this is the check that stops a stray response skipping a conversation.
    const borrowed = await askWith((request) => ({ ...hasOk()(request), request_id: 'not-ours' }));
    expect(borrowed.sent[0]!.request_id).not.toBe('not-ours');
    expect(borrowed.answer.ok).toBe(false);
  });

  it('🔴 the request fields the extension sends are exactly the schema\'s `hasRequest`', async () => {
    const def = SCHEMA.$defs.hasRequest;
    expect(def, 'the committed schema must describe §6.6').toBeDefined();
    expect([...(def.required as string[])].sort()).toEqual([
      'fingerprint', 'platform', 'protocol', 'request_id', 'session_id', 'type',
    ]);
    // Both identity-bearing fields are the schema's own definitions, not free text:
    // a request_id outside §6.2's grammar or a fingerprint that is not 64 hex would
    // be refused by the host, and the refusal would read as "not held".
    expect(def.properties.request_id.$ref).toBe('#/$defs/requestId');
    expect(def.properties.fingerprint.$ref).toBe('#/$defs/sha256');

    const { sent } = await askWith(hasOk({ held: false, shard: null }));
    expect(sent).toHaveLength(1);
    expect(Object.keys(sent[0]!).sort()).toEqual([
      'fingerprint', 'platform', 'protocol', 'request_id', 'session_id', 'type',
    ]);
    expect(sent[0]).toMatchObject({
      protocol: 1, type: 'has', platform: 'chatgpt', session_id: 's', fingerprint: 'a'.repeat(64),
    });
    expect(sent[0]!.request_id).toMatch(/^[A-Za-z0-9_-]{1,128}$/);
  });
});
