/**
 * W299 · W128 ③c step 1 — a ChatGPT capture is tied to the account on **its own request**.
 *
 * The property this file protects is one sentence: *the account a ChatGPT capture is
 * attributed to is the one named on the request that produced it, and the raw value never
 * leaves the extension.* Three things follow, and each needs its own check because each
 * fails independently:
 *
 *  1. **Correlation, not a page-global.** The `ChatGPT-Account-Id` value is read inside the
 *     same `fetch` invocation that produced the response and travels to that capture as an
 *     argument. Two overlapping requests with different values must each get their own —
 *     a "last account seen" reading would attach A's account to B's body the moment they
 *     overlap or the account switches between them.
 *  2. **A fingerprint, never the raw id.** The value is a page-visible string the page can
 *     forge; it is admitted only as a bounded non-empty string, hashed with the install's
 *     salt, and deleted from the capture the instant `buildBundle` has consumed it. It must
 *     appear in no delivered bundle, no outbox payload, no native-host message — the two
 *     places it would otherwise reach are the durable write and the host `deliver`.
 *  3. **Old and non-ChatGPT captures are untouched.** The field is optional; an absent or
 *     malformed one is the named `unknown` the fingerprint path already answers with, and
 *     no other platform may carry it at all.
 *
 * Fixtures only: opaque synthetic ids, a synthetic native host, no logged-in session and no
 * real header value anywhere. The hook tests drive the real MAIN-world hook against a fake
 * page window; the bundle tests drive the real background listener end to end.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import {
  CAPTURE_MESSAGE,
  CHATGPT_ACCOUNT_ID_HEADER_MAX_CHARS,
  CHATGPT_WORKSPACE_OBSERVED_MESSAGE,
  isCapturedFetchShape,
  isPageCaptureMessage,
  type CapturedFetch,
} from '../lib/contract';
import { CONVERSATION_SEEN_MESSAGE } from '../lib/platform-auth';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';
import {
  ACCOUNT_SALT_KEY,
  accountFingerprintFor,
  accountIdFromCapture,
  coordinationIdFromCapture,
} from '../lib/account-fingerprint';
import { memoryStore } from '../lib/backfill/store';
import { buildExportFile, listEntries } from '../lib/outbox';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';
import { BACKFILL_TARGETS_KEY } from '../lib/backfill/alarm';
import { applyDebtDiff, readDebtSet } from '../lib/backfill/debt-store';
import { stateKey } from '../lib/backfill/types';

// ---------------------------------------------------------------------------
// Synthetic fixtures. No real account, workspace or conversation id below.
// ---------------------------------------------------------------------------
const ORIGIN = 'https://chatgpt.com';
const DEEPSEEK_ORIGIN = 'https://chat.deepseek.com';
const ID_1 = 'aaaaaaaa-1111-2222-3333-444444444444';
const ID_2 = 'bbbbbbbb-2222-3333-4444-555555555555';
const DETAIL = (id: string): string => `${ORIGIN}/backend-api/conversation/${id}`;
const PAGED = (id: string): string => `${ORIGIN}/backend-api/conversations/${id}`;
const DEEPSEEK_URL = `${DEEPSEEK_ORIGIN}/api/v0/chat/history_messages?chat_session_id=${ID_1}`;
/** The ChatGPT row's declared shape, so a capture is not refused for the wrong reason. */
const CHATGPT_BODY = JSON.stringify({ mapping: {}, current_node: 'node-0' });
const DEEPSEEK_BODY = JSON.stringify({ session_id: ID_1 });
const HEADER_A = 'acct-fixture-alpha';
const HEADER_B = 'acct-fixture-beta';
const FORGED = 'forged-account-fixture';

// ===========================================================================
// Part A · the page hook: read the header on this request, pair it with this response
// ===========================================================================

/** A fake page window faithful only in the places the hook touches. */
function pageWindow(origin: string, body: string): any {
  const posted: any[] = [];
  const win: any = {
    location: { origin, href: `${origin}/` },
    fetch: async () => new Response(body, { status: 200 }),
    addEventListener() { /* the hook's other listeners are not what this file tests */ },
    postMessage(message: unknown) { posted.push(message); },
  };
  win.posted = posted;
  return win;
}

/** Let every `void maybeCapture(...)` the hook started reach its `post`. */
async function flush(times = 6): Promise<void> {
  for (let i = 0; i < times; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
}

function install(win: any): void {
  vi.stubGlobal('window', win);
  installPageFetchHook(PAGE_HOOK_OPTIONS);
}

const captures = (win: any): any[] => win.posted.filter((m: any) => m?.type === CAPTURE_MESSAGE);
const observations = (win: any): any[] =>
  win.posted.filter((m: any) => m?.type === CHATGPT_WORKSPACE_OBSERVED_MESSAGE);

describe('W299-A · the hook reads the header from every shape a page can pass', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  it('🔴 Headers / tuple array / plain object / Request.headers all resolve, init.headers wins', async () => {
    const builders: Array<[string, (url: string) => [RequestInfo | URL, RequestInit | undefined]]> = [
      ['Headers instance', (url) => [url, { headers: new Headers([['ChatGPT-Account-Id', HEADER_A]]) }]],
      ['tuple array', (url) => [url, { headers: [['chatgpt-account-id', HEADER_A]] as unknown as HeadersInit }]],
      ['plain object, mixed case', (url) => [url, { headers: { 'ChatGPT-ACCOUNT-ID': HEADER_A } as Record<string, string> }]],
      ['Request#headers with no init', (url) => [
        new Request(url, { headers: { 'ChatGPT-Account-Id': HEADER_A } }),
        undefined,
      ]],
    ];
    for (const [name, build] of builders) {
      const win = pageWindow(ORIGIN, CHATGPT_BODY);
      install(win);
      const [input, init] = build(DETAIL(ID_1));
      await win.fetch(input, init);
      await flush();
      const got = captures(win);
      expect(got, name).toHaveLength(1);
      expect(got[0]!.payload.chatgptAccountIdHeader, name).toBe(HEADER_A);
      // The W108 workspace observation is unchanged and carries the same one reading.
      expect(observations(win).map((m) => m.accountId), name).toEqual([HEADER_A]);
      vi.unstubAllGlobals();
    }
  });

  it('🔴 init.headers overrides the Request’s own headers, per the fetch contract', async () => {
    const win = pageWindow(ORIGIN, CHATGPT_BODY);
    install(win);
    const request = new Request(DETAIL(ID_1), { headers: { 'ChatGPT-Account-Id': HEADER_A } });
    await win.fetch(request, { headers: { 'ChatGPT-Account-Id': HEADER_B } });
    await flush();
    expect(captures(win)[0]!.payload.chatgptAccountIdHeader).toBe(HEADER_B);
  });

  it('🔴 absent header is distinct from a present malformed value', async () => {
    const misses: Array<[string, RequestInit | undefined]> = [
      ['no header at all', undefined],
      ['empty string', { headers: { 'ChatGPT-Account-Id': '' } }],
      ['whitespace only', { headers: { 'ChatGPT-Account-Id': '   ' } }],
      ['a number', { headers: { 'ChatGPT-Account-Id': 42 as unknown as string } }],
      ['a non-string tuple', { headers: [['ChatGPT-Account-Id', 42]] as unknown as HeadersInit }],
      ['null', { headers: { 'ChatGPT-Account-Id': null as unknown as string } }],
      ['oversized value', { headers: { 'ChatGPT-Account-Id': 'x'.repeat(CHATGPT_ACCOUNT_ID_HEADER_MAX_CHARS + 1) } }],
    ];
    for (const [name, init] of misses) {
      const win = pageWindow(ORIGIN, CHATGPT_BODY);
      install(win);
      await win.fetch(DETAIL(ID_1), init);
      await flush();
      const got = captures(win);
      expect(got, name).toHaveLength(1);
      // A present but invalid field is carried only as a boolean presence marker,
      // so the worker cannot mistake it for an absent header and use body data.
      expect(got[0]!.payload.chatgptAccountIdHeaderPresent, name).toBe(name !== 'no header at all');
      expect('chatgptAccountIdHeader' in got[0]!.payload, name).toBe(false);
      expect(observations(win).map((m) => m.accountId), name).toEqual([null]);
      vi.unstubAllGlobals();
    }
  });

  it('🔴 two overlapping requests each keep their own header, not whichever came last', async () => {
    const win = pageWindow(ORIGIN, CHATGPT_BODY);
    const pending: Array<(response: Response) => void> = [];
    win.fetch = () => new Promise<Response>((resolve) => { pending.push(resolve); });
    install(win);

    const first = win.fetch(DETAIL(ID_1), { headers: { 'ChatGPT-Account-Id': HEADER_A } });
    const second = win.fetch(DETAIL(ID_2), { headers: { 'ChatGPT-Account-Id': HEADER_B } });
    // Resolve them out of order: B's response lands first, then A's. A page-global
    // "last observed account" would pair the second resolution with the wrong value.
    pending[1]!(new Response(CHATGPT_BODY, { status: 200 }));
    pending[0]!(new Response(CHATGPT_BODY, { status: 200 }));
    await Promise.all([first, second]);
    await flush();

    const got = captures(win);
    const byUrl = new Map(got.map((c) => [c.payload.url, c.payload.chatgptAccountIdHeader]));
    expect(byUrl.get(DETAIL(ID_1))).toBe(HEADER_A);
    expect(byUrl.get(DETAIL(ID_2))).toBe(HEADER_B);
    // W108's separate workspace observation is untouched: one per request, in request order.
    expect(observations(win).map((m) => m.accountId)).toEqual([HEADER_A, HEADER_B]);
  });

  it('🔴 a ChatGPT paged-window notification stays ID-only, and a non-ChatGPT page never gains the field', async () => {
    const chatgpt = pageWindow(ORIGIN, CHATGPT_BODY);
    install(chatgpt);
    await chatgpt.fetch(PAGED(ID_1), { headers: { 'ChatGPT-Account-Id': HEADER_A } });
    await flush();
    // The paged window is not a capture — only the id is handed on (issue: partial bodies).
    expect(captures(chatgpt)).toHaveLength(0);
    expect(chatgpt.posted.filter((m: any) => m?.type === CONVERSATION_SEEN_MESSAGE)).toEqual([
      { type: CONVERSATION_SEEN_MESSAGE, platform: 'chatgpt', id: ID_1 },
    ]);
    vi.unstubAllGlobals();

    // A header named on some other platform's request is not an account for that platform.
    const deepseek = pageWindow(DEEPSEEK_ORIGIN, DEEPSEEK_BODY);
    install(deepseek);
    await deepseek.fetch(DEEPSEEK_URL, { headers: { 'ChatGPT-Account-Id': HEADER_A } });
    await flush();
    const got = captures(deepseek);
    expect(got).toHaveLength(1);
    expect('chatgptAccountIdHeader' in got[0]!.payload).toBe(false);
  });
});

describe('W299-A2 · the page→content gate refuses a malformed or misplaced field', () => {
  const capture = (extra: Partial<CapturedFetch>): CapturedFetch => ({
    url: DETAIL(ID_1),
    method: 'GET',
    status: 200,
    text: CHATGPT_BODY,
    capturedAt: 1_700_000_000_000,
    ...extra,
  });

  it('🔴 bounded metadata is admitted; malformed metadata stays unknown without refusing the capture', () => {
    expect(isCapturedFetchShape(capture({ chatgptAccountIdHeader: HEADER_A }))).toBe(true);
    expect(isCapturedFetchShape(capture({ chatgptAccountIdHeaderPresent: true }))).toBe(true);

    const unknown: Array<[string, Partial<CapturedFetch>]> = [
      ['a number', { chatgptAccountIdHeader: 42 as unknown as string }],
      ['an empty string', { chatgptAccountIdHeader: '' }],
      ['whitespace only', { chatgptAccountIdHeader: '  ' }],
      ['an over-long value', { chatgptAccountIdHeader: 'x'.repeat(CHATGPT_ACCOUNT_ID_HEADER_MAX_CHARS + 1) }],
    ];
    for (const [name, extra] of unknown) {
      expect(isCapturedFetchShape(capture(extra)), name).toBe(true);
    }

    // …and the field cannot ride a capture for a platform it does not describe.
    const other: CapturedFetch = {
      url: DEEPSEEK_URL,
      method: 'GET',
      status: 200,
      text: DEEPSEEK_BODY,
      capturedAt: 1_700_000_000_000,
      chatgptAccountIdHeader: HEADER_A,
    };
    expect(isCapturedFetchShape(other)).toBe(false);
    expect(isCapturedFetchShape({ ...other, chatgptAccountIdHeader: undefined })).toBe(true);
  });
});

// ===========================================================================
// Part B · the fingerprint reader
// ===========================================================================
describe('W299-B · the header is the ChatGPT account id, and only ever a fingerprint', () => {
  const capture = (extra: Partial<CapturedFetch> = {}): CapturedFetch => ({
    url: DETAIL(ID_1),
    method: 'GET',
    status: 200,
    text: CHATGPT_BODY,
    capturedAt: 1_700_000_000_000,
    ...extra,
  });

  it('🔴 the header source is selected, with its own source label', () => {
    expect(accountIdFromCapture(capture({ chatgptAccountIdHeader: HEADER_A }), null)).toEqual({
      kind: 'id',
      id: HEADER_A,
      source: 'request-header-chatgpt-account-id',
    });
  });

  it('🔴 the header is preferred over a body-derived id on the same response', () => {
    const bodyWithUid = JSON.stringify({ account_id: 'body-fixture-uid', mapping: {}, current_node: 'n0' });
    expect(accountIdFromCapture(capture({ text: bodyWithUid, chatgptAccountIdHeader: HEADER_A }), null)).toEqual({
      kind: 'id',
      id: HEADER_A,
      source: 'request-header-chatgpt-account-id',
    });
  });

  it('🔴 same id ⇒ same fingerprint, second id ⇒ different, source recorded; raw id absent', async () => {
    const store = memoryStore();
    const one = await accountFingerprintFor(capture({ chatgptAccountIdHeader: HEADER_A }), store, null);
    const again = await accountFingerprintFor(capture({ chatgptAccountIdHeader: HEADER_A }), store, null);
    const other = await accountFingerprintFor(capture({ chatgptAccountIdHeader: HEADER_B }), store, null);

    expect(one.kind).toBe('fingerprint');
    if (one.kind !== 'fingerprint' || again.kind !== 'fingerprint' || other.kind !== 'fingerprint') {
      throw new Error('fixture must produce fingerprints');
    }
    expect(again.value).toBe(one.value);
    expect(other.value).not.toBe(one.value);
    expect(one.source).toBe('request-header-chatgpt-account-id');
    expect(one.value).toMatch(/^[0-9a-f]{64}$/);
    expect(JSON.stringify(one)).not.toContain(HEADER_A);
    const stored = store.data[ACCOUNT_SALT_KEY] as { id: string };
    expect(one.saltId).toBe(stored.id);
  });

  it('🔴 absent or malformed header is a named unknown, never a fingerprint of nothing', async () => {
    const store = memoryStore();
    const absent = await accountFingerprintFor(capture(), store, null);
    expect(absent).toEqual({ kind: 'unknown', reason: 'no-account-id-in-capture' });

    // A value past the bound is refused by the same validator the gate uses, so this is
    // the same unknown rather than a fingerprint over a document.
    const oversized = await accountFingerprintFor(
      capture({ chatgptAccountIdHeader: 'x'.repeat(CHATGPT_ACCOUNT_ID_HEADER_MAX_CHARS + 1) }),
      store,
      null,
    );
    expect(oversized).toEqual({ kind: 'unknown', reason: 'no-account-id-in-capture' });
    const malformedWithBodyId = await accountFingerprintFor(
      capture({ text: JSON.stringify({ account_id: 'body-fixture-uid', mapping: {}, current_node: 'n0' }), chatgptAccountIdHeaderPresent: true }),
      store,
      null,
    );
    expect(malformedWithBodyId).toEqual({ kind: 'unknown', reason: 'no-account-id-in-capture' });
    const absentNewHeader = await accountFingerprintFor(
      capture({ text: JSON.stringify({ account_id: 'body-fixture-uid', mapping: {}, current_node: 'n0' }), chatgptAccountIdHeaderPresent: false }),
      store,
      null,
    );
    expect(absentNewHeader).toEqual({ kind: 'unknown', reason: 'no-account-id-in-capture' });
    // Older messages predate the presence marker, so their former body-axis behavior remains readable.
    expect(accountIdFromCapture(capture({ text: JSON.stringify({ account_id: 'body-fixture-uid', mapping: {}, current_node: 'n0' }) }), null))
      .toEqual({ kind: 'id', id: 'body-fixture-uid', source: 'response-body-platform-uid' });
  });

  it('🔴 an unreadable salt stays salt-unreadable even when the header is present', async () => {
    const store = memoryStore({ [ACCOUNT_SALT_KEY]: { id: 'fixture-salt', key: 'not-base64!!', createdAt: 1 } });
    expect(await accountFingerprintFor(capture({ chatgptAccountIdHeader: HEADER_A }), store, null))
      .toEqual({ kind: 'unknown', reason: 'salt-unreadable' });
  });

  it('🔴 the raw header is never the coordination id the host is handed', () => {
    expect(coordinationIdFromCapture(capture({ chatgptAccountIdHeader: HEADER_A }), null)).toBeNull();
    expect(coordinationIdFromCapture(capture(), null)).toBeNull();
  });
});

// ===========================================================================
// Part C · the real capture-to-host path: no raw value at any durable boundary
// ===========================================================================
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
      async remove(keys: string | string[]) { for (const k of Array.isArray(keys) ? keys : [keys]) delete store[k]; },
    },
  },
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
};

async function dispatch(payload: CapturedFetch): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  if (runtimeListeners.length === 0) await mod.default();
  await new Promise<any>((resolve) => {
    runtimeListeners[0]!({ type: 'chat-captured', payload }, { id: 's' }, resolve);
  });
  await mod.backfillTickSettled();
  return mod;
}

function liveCapture(extra: Partial<CapturedFetch> = {}): CapturedFetch {
  return {
    url: DETAIL(ID_1),
    method: 'GET',
    status: 200,
    text: CHATGPT_BODY,
    capturedAt: 1_700_000_000_000,
    ...extra,
  };
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  host = createSyntheticHost({ up: true });
  (globalThis as any).indexedDB = new IDBFactory();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest, setBackfillEnabled } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('W299-C · a forged page-visible value reaches no durable write and no host message', () => {
  it('🔴 the delivered bundle carries the fingerprint, and the raw value is nowhere on the wire', async () => {
    const payload = liveCapture({ chatgptAccountIdHeader: FORGED });
    await dispatch(payload);

    const bundles = host.deliveries.map((d) => JSON.parse(d.payload));
    expect(bundles).toHaveLength(1);
    expect(bundles[0].account.kind).toBe('fingerprint');
    expect(bundles[0].account.source).toBe('request-header-chatgpt-account-id');
    expect(bundles[0].account.value).toMatch(/^[0-9a-f]{64}$/);
    expect(bundles[0].account.value).not.toBe(FORGED);

    // 🔴 The page can forge the value, so the assertion is about where it may land, not
    //    about whether it is true: nothing durable and nothing forwarded carries it.
    expect(JSON.stringify(host.requests())).not.toContain(FORGED);
    expect(JSON.stringify(bundles)).not.toContain(FORGED);
    // The host is handed no coordination id derived from the header either.
    const deliver = host.requests().find((r) => r.type === 'deliver')!;
    expect(deliver.account_id).toBeUndefined();
    // And the transient field is gone from the object itself, by construction.
    expect('chatgptAccountIdHeader' in payload).toBe(false);
  });

  it('🔴 a capture the host cannot take yet keeps the raw value out of the outbox too', async () => {
    host = createSyntheticHost({ up: false });
    const payload = liveCapture({ chatgptAccountIdHeader: FORGED });
    const result = await dispatch(payload);
    expect(result).toBeTruthy();

    const entries = await listEntries();
    expect(entries).not.toBeNull();
    expect(entries!.length).toBeGreaterThan(0);
    for (const entry of entries!) {
      expect(entry.payload).not.toContain(FORGED);
    }
    // The queued payload still carries the fingerprint — the durability is in the bundle,
    // not in the transient field.
    expect(entries![0]!.payload).toContain('request-header-chatgpt-account-id');
  });

  it('🔴 export serialization and a malformed forged page message never serialize the raw value', async () => {
    const raw = 'oversized-forged-fixture-id'.repeat(40);
    host = createSyntheticHost({ up: false });
    const payload = liveCapture({ chatgptAccountIdHeader: raw });
    expect(raw.length).toBeGreaterThan(CHATGPT_ACCOUNT_ID_HEADER_MAX_CHARS);
    expect((await accountFingerprintFor(payload, memoryStore(), null)).kind).toBe('unknown');
    const page = {} as MessageEventSource;
    const event = { source: page, origin: ORIGIN, data: { type: CAPTURE_MESSAGE, payload } };
    expect(isPageCaptureMessage(event, page, ORIGIN)).toBe(true);
    expect(isPageCaptureMessage({ ...event, origin: 'https://attacker.invalid' }, page, ORIGIN)).toBe(false);
    await dispatch((event.data as { payload: CapturedFetch }).payload);

    const queued = await listEntries();
    expect(queued).not.toBeNull();
    expect(JSON.stringify(queued)).not.toContain(raw);
    const exported = buildExportFile(queued!, 1, null, 'abcdef');
    expect(exported.content).not.toContain(raw);
    expect(JSON.stringify(exported)).not.toContain(raw);
    expect(JSON.stringify(store)).not.toContain(raw);
    expect(host.deliveries.map((delivery) => delivery.payload).join('\n')).not.toContain(raw);
    expect((await accountFingerprintFor(payload, memoryStore(), null)).kind).toBe('unknown');
  });
});

describe('W299-D · old workspace scopes are fingerprinted without losing pending ids', () => {
  it('🔴 migrates a hex-shaped raw scope, is idempotent, preserves debt and erases the raw scope', async () => {
    const rawWorkspace = 'a'.repeat(64);
    const oldScope = `chatgpt:${rawWorkspace}`;
    const oldKey = stateKey('chatgpt', oldScope);
    const id = 'pending-conversation-fixture';
    const { browserLocalStore } = await import('../lib/backfill/store');
    const storage = browserLocalStore()!;
    const { openLedger } = await import('../lib/backfill/ledger');
    const opened = await openLedger(storage, 'chatgpt', oldScope);
    expect(opened.ok).toBe(true);
    if (!opened.ok) throw new Error('synthetic source ledger must open');
    expect(await applyDebtDiff('chatgpt', oldScope, { enqueue: [id], settle: [], drop: [] }, 1)).toBe(true);
    await opened.ledger.save({ ...opened.state, pending: [id] });
    await storage.save(BACKFILL_TARGETS_KEY, [{ platform: 'chatgpt', origin: ORIGIN, scope: oldScope, at: 1 }]);

    const { migrateChatGptWorkspaceScopes } = await import('../lib/backfill/chatgpt-scope-migration');
    const { fingerprintChatGptWorkspace } = await import('../lib/backfill/chatgpt-workspace');
    const expectedScope = await fingerprintChatGptWorkspace(storage, rawWorkspace);
    expect(expectedScope).toMatch(/^chatgpt:fp1:[a-f0-9]{64}$/);
    await migrateChatGptWorkspaceScopes(storage);

    expect(await storage.load(oldKey)).toBeNull();
    expect(await storage.keys()).not.toContain(oldKey);
    expect(JSON.stringify(store)).not.toContain(rawWorkspace);
    const targetRows = await storage.load(BACKFILL_TARGETS_KEY) as Array<{ scope: string }>;
    expect(targetRows.map((row) => row.scope)).toEqual([expectedScope]);
    expect(await readDebtSet('chatgpt', oldScope)).toEqual({ pending: [], archived: [], nextSeq: 1, times: new Map() });
    const migratedDebt = await readDebtSet('chatgpt', expectedScope!);
    expect(migratedDebt?.pending).toEqual([id]);
    expect(JSON.stringify(migratedDebt)).not.toContain(rawWorkspace);

    const takeSnapshot = async () => Object.fromEntries(await Promise.all(
      (await storage.keys()).map(async (key) => [key, await storage.load(key)] as const),
    ));
    const afterFirstRun = JSON.stringify(await takeSnapshot());
    await migrateChatGptWorkspaceScopes(storage);
    expect(JSON.stringify(await takeSnapshot())).toBe(afterFirstRun);
    expect(JSON.stringify(await takeSnapshot())).not.toContain(rawWorkspace);
  });
});
