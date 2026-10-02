/**
 * W199 · W128 step 2 — the run identity lease.
 *
 * What this file pins, and why each half needs pinning:
 *
 *  1. 🔴 **the comparison is the one construction step 1 already uses.** A lease taken from a
 *     scope and a fingerprint taken from a capture must be *equal* for the same account and the
 *     same install, because a mechanism whose two halves are compared must not be able to
 *     disagree with the record it is protecting. One HMAC, two readers.
 *  2. 🔴 **only a proven disagreement stops anything.** A scope with no lease, a plan that
 *     holds none, and a response that names no account are all `incomparable`, and every one of
 *     them must leave the leg exactly as it was — folding any of them into "the account changed"
 *     would have the mechanism invent a switch, which is the failure an append-only archive
 *     cannot take back.
 *  3. 🔴 **a proven disagreement stops it in both attribution directions.** A list page's ids
 *     must not reach the ledger, and a body must not be settled, archived or delivered — and the
 *     pending queue must be untouched, not merely smaller.
 *  4. 🔴 **the scope is suspended, not just halted.** A halt expires by itself; a suspension does
 *     not, and the two are written as two records because they say two things.
 *  5. 🔴 **an unreadable salt is a named unknown, never a fresh salt** — step 1's rule, re-used
 *     here because a fresh salt would re-key this lease while the old bundles kept the old value.
 *
 * Zero network, zero logged-in state, zero real account data: every id below is an obvious
 * fixture, and the http port is a pure function that **throws** on any path it was not given.
 */

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import type { CapturedFetch } from '../lib/contract';
import {
  ACCOUNT_FINGERPRINT_DOMAIN,
  ACCOUNT_SALT_KEY,
  accountFingerprintFor,
  fingerprintAccountId,
  loadOrCreateAccountSalt,
} from '../lib/account-fingerprint';
import {
  accountLeaseForScope,
  compareAccountLease,
  decideRunLease,
  planHoldsAccountLease,
  suspensionFor,
} from '../lib/backfill/account-lease';
import { enqueueDebts } from '../lib/backfill/debts';
import { runBackfill, type HttpPort, type HttpResponse, type SinkOutcome } from '../lib/backfill/engine';
import { openLedger } from '../lib/backfill/ledger';
import { memoryStore } from '../lib/backfill/store';
import type { Clock } from '../lib/backfill/pace';
import { stateKey, type BackfillHeader } from '../lib/backfill/types';
import { chatGptWorkspaceScope, fingerprintedChatGptWorkspace, resolveChatGptWorkspace } from '../lib/backfill/chatgpt-workspace';
import { serveBackfillFetch, tabHttpPort } from '../lib/backfill/tab-port';

// ---------------------------------------------------------------------------
// Synthetic fixtures. No real account id, conversation id or email appears
// below; every one is an obvious fixture string.
// ---------------------------------------------------------------------------
const ORIGIN = 'https://grok.com';
/** The account this scope names. A fixture, and the value the scope key is built from. */
const ACCOUNT_A = 'acct-fixture-1';
/** A different account on the same install. */
const ACCOUNT_B = 'acct-fixture-2';
const C1 = 'cc111111-0000-4000-8000-000000000001';
const C2 = 'cc222222-0000-4000-8000-000000000002';

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

function fakeClock(): Clock & { sleeps: number[] } {
  const sleeps: number[] = [];
  let t = Date.parse('2026-09-26T00:00:00.000Z');
  return {
    sleeps,
    now: () => t,
    async sleep(ms: number) { sleeps.push(ms); t += ms; },
  };
}

/**
 * A synthetic grok backend. Every path it was not given **throws**, so "this leg
 * sent no further request" is proven by the run rather than asserted about it.
 *
 * `listAccount` / `detailAccount` default to **null**, which is the ordinary shape: a
 * body with nothing account-shaped in it. That is the case the `incomparable` branch
 * exists for, and leaving it the default keeps every other case in this file measuring
 * what it would have measured before W199.
 */
function backend(opts: {
  ids?: string[];
  listAccount?: string | null;
  detailAccount?: string | null;
  steps?: boolean;
} = {}) {
  const ids = opts.ids ?? [C1];
  const twoStep = opts.steps !== false;
  const calls: string[] = [];
  const http = async (url: string, _init?: unknown): Promise<HttpResponse> => {
    calls.push(url);
    const u = new URL(url);
    if (u.pathname === '/rest/app-chat/conversations') {
      const body: Record<string, unknown> = {
        conversations: ids.map((conversationId) => ({ conversationId })),
      };
      if (opts.listAccount) body.user_id = opts.listAccount;
      return { status: 200, text: JSON.stringify(body) };
    }
    if (twoStep && /\/response-node$/.test(u.pathname)) {
      return {
        status: 200,
        text: JSON.stringify({
          responseNodes: [{ responseId: 'r1', sender: 'human' }],
          inflightResponses: [],
        }),
      };
    }
    if (/\/load-responses$/.test(u.pathname)) {
      const body: Record<string, unknown> = {
        responses: [{ responseId: 'r1', message: 'synthetic-body', sender: 'human' }],
      };
      if (opts.detailAccount) body.user_id = opts.detailAccount;
      return { status: 200, text: JSON.stringify(body) };
    }
    throw new Error(`unexpected path ${u.pathname}`);
  };
  return { calls, http: http as never };
}

function run(
  store: ReturnType<typeof memoryStore>,
  http: unknown,
  scope: string,
  extra: Partial<Parameters<typeof runBackfill>[0]> = {},
) {
  const delivered: CapturedFetch[] = [];
  const report = runBackfill({
    platform: 'grok',
    origin: ORIGIN,
    scope,
    store,
    http: http as never,
    clock: fakeClock(),
    pace: NO_WAIT,
    sink: (captured: CapturedFetch): SinkOutcome => {
      delivered.push(captured);
      return { saved: true, sessionId: captured.sessionId };
    },
    ...extra,
  });
  return { report, delivered };
}

function storedHeader(platform: string, scope: string, data: Record<string, unknown>): BackfillHeader {
  return data[stateKey(platform, scope)] as BackfillHeader;
}

/** The fingerprint of one account on one platform, under one install's salt. */
async function fp(store: ReturnType<typeof memoryStore>, id: string): Promise<string> {
  const salt = await loadOrCreateAccountSalt(store);
  if (!salt || salt === 'unreadable') throw new Error('fixture must produce a salt');
  const value = await fingerprintAccountId(salt, ACCOUNT_FINGERPRINT_DOMAIN, 'grok', id);
  if (value === null) throw new Error('fixture must produce a value');
  return value;
}

function saltIdOf(store: ReturnType<typeof memoryStore>): string {
  return (store.data[ACCOUNT_SALT_KEY] as { id: string }).id;
}

async function seedChatGptLease(
  store: ReturnType<typeof memoryStore>,
  scope: string,
  accountId: string,
  owed: string[] = [],
  opts: { enumerated?: boolean } = {},
): Promise<void> {
  const opened = await openLedger(store, 'chatgpt', scope);
  if (!opened.ok) throw new Error('fixture must open the ChatGPT ledger');
  opened.state.accountLease = {
    value: await fingerprintFor(store, accountId),
    saltId: saltIdOf(store),
    source: 'request-header-chatgpt-account-id',
    at: 1,
  };
  opened.state.enumCursor = { offset: owed.length, complete: opts.enumerated ?? owed.length > 0 };
  enqueueDebts(opened.state, owed);
  await opened.ledger.save(opened.state);
}

async function fingerprintFor(store: ReturnType<typeof memoryStore>, id: string): Promise<string> {
  const salt = await loadOrCreateAccountSalt(store);
  if (!salt || salt === 'unreadable') throw new Error('fixture must produce a salt');
  const value = await fingerprintAccountId(salt, ACCOUNT_FINGERPRINT_DOMAIN, 'chatgpt', id);
  if (value === null) throw new Error('fixture must produce a fingerprint');
  return value;
}

/**
 * Give a scope a lease and (optionally) some owed ids, written through the ledger so the
 * header and the debt store agree — a header that disagrees with the store is W45's
 * `ledger-mismatch` refusal, which would stop the run before this file's subject.
 */
async function seed(
  store: ReturnType<typeof memoryStore>,
  scope: string,
  opts: { owed?: string[]; leaseFor?: string | null; enumerated?: boolean } = {},
): Promise<void> {
  const owed = opts.owed ?? [];
  const opened = await openLedger(store, 'grok', scope);
  if (!opened.ok) throw new Error('fixture must open the ledger');
  const state = opened.state;
  if (opts.leaseFor === null) {
    state.accountLease = undefined;
  } else if (opts.leaseFor !== undefined) {
    state.accountLease = {
      value: await fp(store, opts.leaseFor),
      saltId: saltIdOf(store),
      source: 'response-body-platform-uid',
      at: 1,
    };
  }
  // 🔴 `enumerated: false` is what makes a run issue a list request at all: a scope whose
  //    cursor is complete and which owes nothing finishes `queue-empty` before fetching, so
  //    the list-attribution case has to leave the cursor open.
  state.enumCursor = { offset: owed.length, complete: opts.enumerated !== false };
  enqueueDebts(state, owed);
  await opened.ledger.save(state);
}

beforeEach(() => {
  vi.stubGlobal('indexedDB', new IDBFactory());
});

// ===========================================================================
// Part A · the module, on synthetic fixtures
// ===========================================================================
describe('W199-A · the lease is step 1\'s construction, read from the scope', () => {
  it('🔴 the lease for a scope equals the bundle fingerprint for the same account and install', async () => {
    const store = memoryStore();
    const reading = await accountLeaseForScope('grok', ACCOUNT_A, store, 1_700_000_000_000);
    expect(reading.kind).toBe('lease');
    if (reading.kind !== 'lease') return;

    // The other reader: the bundle path, over a synthetic capture whose body carries the
    // same account id. If these two ever disagreed, the lease would be comparing a value
    // against something it is not about.
    const captured: CapturedFetch = {
      url: `${ORIGIN}/rest/app-chat/conversations/${C1}/load-responses`,
      method: 'POST',
      status: 200,
      text: JSON.stringify({ responses: [], user_id: ACCOUNT_A }),
      capturedAt: 1_700_000_000_000,
    };
    const bundle = await accountFingerprintFor(captured, store, C1);
    expect(bundle.kind).toBe('fingerprint');
    if (bundle.kind !== 'fingerprint') return;

    expect(reading.lease.value).toBe(bundle.value);
    expect(reading.lease.saltId).toBe(bundle.saltId);
    // …and it is a fingerprint of the account axis, not a coincidence: a different account
    // on the same install is a different value.
    expect(reading.lease.value).not.toBe(await fp(store, ACCOUNT_B));
    expect(reading.lease.source).toBe('response-body-platform-uid');
  });

  it('🔴 the value is a keyed digest, and carries no part of the scope it came from', async () => {
    const store = memoryStore();
    const reading = await accountLeaseForScope('grok', ACCOUNT_A, store, 1);
    if (reading.kind !== 'lease') throw new Error('fixture must produce a lease');
    expect(reading.lease.value).toMatch(/^[0-9a-f]{64}$/);
    expect(reading.lease.value).not.toContain(ACCOUNT_A);
  });

  it('🔴 a scope that names no account, a plan that holds no lease, and a scope that names an organization are three different unknowns', async () => {
    const store = memoryStore();
    expect(await accountLeaseForScope('grok', 'default', store, 1))
      .toEqual({ kind: 'unleased', reason: 'scope-names-no-account' });
    // ChatGPT is not scope-derived leased: its workspace scope and its stable
    // ChatGPT-Account-Id request header are separate axes. W303 checks the latter
    // on every response in the engine.
    expect(await accountLeaseForScope('chatgpt', ACCOUNT_A, store, 1))
      .toEqual({ kind: 'unleased', reason: 'platform-not-scoped' });
    // 🔴 W239 · Claude is the third case and it is *not* the second one: its scope does name
    //    something — an organization — and that something is not a person (two accounts can
    //    be members of one), so a lease over it would agree with every response either of
    //    them sends. It gets its own reason rather than borrowing "this plan declares no
    //    account axis", which is simply false about this plan. See
    //    `ORGANIZATION_SCOPED_PLATFORMS`.
    expect(await accountLeaseForScope('claude', '11111111-2222-3333-4444-555555555555', store, 1))
      .toEqual({ kind: 'unleased', reason: 'scope-names-an-organization' });
    expect(planHoldsAccountLease('chatgpt')).toBe(false);
    expect(planHoldsAccountLease('claude')).toBe(false);
    for (const p of ['deepseek', 'perplexity', 'gemini', 'grok']) {
      expect(planHoldsAccountLease(p), p).toBe(true);
    }
  });

  it('🔴 an unreadable salt is a named unknown, and the record is left exactly as found', async () => {
    const store = memoryStore({ [ACCOUNT_SALT_KEY]: { id: 'x', key: 'not-base64-key-material!!' } });
    const before = JSON.stringify(store.data[ACCOUNT_SALT_KEY]);
    expect(await accountLeaseForScope('grok', ACCOUNT_A, store, 1))
      .toEqual({ kind: 'unleased', reason: 'salt-unavailable' });
    // The decisive assertion: a repair is not allowed to re-key the install, because that
    // would make one unchanged account look like a switch on the next run.
    expect(JSON.stringify(store.data[ACCOUNT_SALT_KEY])).toBe(before);
  });
});

describe('W199-B · the comparison, and the three things it must not confuse', () => {
  const a = { value: 'aa', saltId: 's1', source: 'response-body-platform-uid' as const };
  const b = { value: 'bb', saltId: 's1', source: 'response-body-platform-uid' as const };
  const otherSalt = { value: 'aa', saltId: 's2', source: 'response-body-platform-uid' as const };

  it('agrees only on the same value and the same salt', () => {
    expect(compareAccountLease(a, { ...a })).toBe('agrees');
    expect(compareAccountLease(a, b)).toBe('differs');
  });

  it('🔴 a different salt is incomparable, never a switch', () => {
    expect(compareAccountLease(a, otherSalt)).toBe('incomparable');
    // …and a missing side is incomparable in both directions: an absence is not evidence.
    expect(compareAccountLease(null, b)).toBe('incomparable');
    expect(compareAccountLease(a, null)).toBe('incomparable');
    expect(compareAccountLease(undefined, undefined)).toBe('incomparable');
  });

  it('🔴 a suspension is written only for a proven disagreement', () => {
    expect(suspensionFor(a, b, 42)).toEqual({ at: 42, reason: 'account-changed', lease: a, observed: b });
    expect(suspensionFor(a, otherSalt, 42)).toBeNull();
    expect(suspensionFor(a, a, 42)).toBeNull();
    // A scope that never had a lease is never accused — the case that would freeze a
    // platform the extension simply cannot identify.
    expect(suspensionFor(undefined, b, 42)).toBeNull();
  });
});

describe('W199-C · what a run may claim before it fetches', () => {
  it('takes a lease when there is none, and does not re-take one that matches', async () => {
    const store = memoryStore();
    const first = decideRunLease(undefined, await accountLeaseForScope('grok', ACCOUNT_A, store, 1));
    expect(first).toMatchObject({ kind: 'run', take: true });
    if (first.kind !== 'run') return;
    const again = decideRunLease(first.lease, await accountLeaseForScope('grok', ACCOUNT_A, store, 2));
    expect(again).toMatchObject({ kind: 'run', take: false });
  });

  it('🔴 re-takes rather than refusing when the salts differ — a reinstall is not a switch', async () => {
    const store = memoryStore();
    const reading = await accountLeaseForScope('grok', ACCOUNT_A, store, 9);
    if (reading.kind !== 'lease') throw new Error('fixture');
    const decision = decideRunLease({ ...reading.lease, saltId: 'a-previous-installs-salt' }, reading);
    expect(decision).toMatchObject({ kind: 'run', take: true });
  });

  it('🔴 refuses when the record and the scope key name different accounts on one salt', async () => {
    const store = memoryStore();
    const reading = await accountLeaseForScope('grok', ACCOUNT_A, store, 9);
    if (reading.kind !== 'lease') throw new Error('fixture');
    // This record cannot be produced by this build — the lease is derived from the scope key,
    // and two derivations of one input cannot differ — which is why the arm is kept for a
    // record this build did not write. It is the same guard `lib/coverage-read.ts` applies to
    // a record whose halves disagree with the address it was found at.
    const decision = decideRunLease({ ...reading.lease, value: await fp(store, ACCOUNT_B) }, reading);
    expect(decision.kind).toBe('refuse');
  });
});

// ===========================================================================
// Part B · the engine, at both attribution points
// ===========================================================================
describe('W199-D · the run takes its lease at start', () => {
  it('🔴 a run with no lease records one, and a scope whose traffic agrees does its work', async () => {
    const store = memoryStore();
    const server = backend({ listAccount: ACCOUNT_A, detailAccount: ACCOUNT_A });
    const result = await run(store, server.http, ACCOUNT_A).report;

    const h = storedHeader('grok', ACCOUNT_A, store.data);
    expect(h.accountLease?.value).toBe(await fp(store, ACCOUNT_A));
    expect(h.accountLease?.source).toBe('response-body-platform-uid');
    expect(h.suspended).toBeUndefined();
    expect(result.stopped).toBe('queue-empty');
    expect(result.accountChangedTo).toBeNull();
    expect(result.archivedThisRun).toEqual([C1]);
  });

  it('🔴 an ordinary platform body (nothing account-shaped) changes nothing at all', async () => {
    const store = memoryStore();
    const server = backend();
    const result = await run(store, server.http, ACCOUNT_A).report;

    // The scope still gets a lease — its key names an account — but no response can be
    // compared against it, so the leg behaves exactly as it did before W199.
    const h = storedHeader('grok', ACCOUNT_A, store.data);
    expect(h.accountLease?.value).toBe(await fp(store, ACCOUNT_A));
    expect(h.suspended).toBeUndefined();
    expect(result.stopped).toBe('queue-empty');
    expect(result.archivedThisRun).toEqual([C1]);
  });

  it('🔴 a plan that holds no lease is not leased, and its responses are never read as accounts', async () => {
    const store = memoryStore();
    const server = backend({ listAccount: ACCOUNT_B, detailAccount: ACCOUNT_B });
    const result = await run(store, server.http, ACCOUNT_A, { platform: 'chatgpt', origin: 'https://chatgpt.com' }).report;
    expect(result.accountChangedTo).toBeNull();
    expect(storedHeader('chatgpt', ACCOUNT_A, store.data).accountLease).toBeUndefined();
    expect(storedHeader('chatgpt', ACCOUNT_A, store.data).suspended).toBeUndefined();
  });
});

describe('W199-E · attribution point one: a list page\'s ids', () => {
  it('🔴 a list answered by another account halts, enqueues NOTHING, and suspends the scope', async () => {
    const store = memoryStore();
    await seed(store, ACCOUNT_A, { leaseFor: ACCOUNT_A, enumerated: false });
    const server = backend({ ids: [C1, C2], listAccount: ACCOUNT_B, steps: false, detailAccount: ACCOUNT_B });

    const result = await run(store, server.http, ACCOUNT_A).report;

    expect(result.halted?.reason).toBe('account-changed');
    expect(result.stopped).toBe('waiting-retry');
    expect(result.accountChangedTo?.id).toBe(ACCOUNT_B);
    // 🔴 Nothing was enqueued: the check is before `enqueueDebts`, so not one of B's ids
    //    reached this scope's ledger.
    expect(result.newDebts).toBe(0);
    expect(result.state.pending).toEqual([]);
    // 🔴 And the body segment never ran: the only request was the list page.
    expect(server.calls.filter((u) => u.includes('load-responses'))).toEqual([]);

    const h = storedHeader('grok', ACCOUNT_A, store.data);
    expect(h.suspended?.reason).toBe('account-changed');
    expect(h.suspended?.observed?.value).toBe(await fp(store, ACCOUNT_B));
    expect(h.suspended?.lease?.value).toBe(await fp(store, ACCOUNT_A));
    // The suspension carries fingerprints only — never the raw ids it compared.
    expect(JSON.stringify(h.suspended)).not.toContain(ACCOUNT_A);
    expect(JSON.stringify(h.suspended)).not.toContain(ACCOUNT_B);
  });

  it('🔴 the same list under the RIGHT account enqueues normally', async () => {
    const store = memoryStore();
    await seed(store, ACCOUNT_A, { leaseFor: ACCOUNT_A, enumerated: false });
    const server = backend({ ids: [C1, C2], listAccount: ACCOUNT_A, detailAccount: ACCOUNT_A });
    const result = await run(store, server.http, ACCOUNT_A).report;
    expect(result.newDebts).toBe(2);
    expect(result.stopped).toBe('queue-empty');
    expect(storedHeader('grok', ACCOUNT_A, store.data).suspended).toBeUndefined();
  });
});

describe('W199-F · attribution point two: a body about to be settled', () => {
  it('🔴 a body answered by another account halts, settles NOTHING, and delivers NOTHING', async () => {
    const store = memoryStore();
    await seed(store, ACCOUNT_A, { owed: [C1], leaseFor: ACCOUNT_A });
    const server = backend({ listAccount: ACCOUNT_A, detailAccount: ACCOUNT_B });

    const { report, delivered } = run(store, server.http, ACCOUNT_A);
    const result = await report;

    expect(result.halted?.reason).toBe('account-changed');
    expect(result.stopped).toBe('waiting-retry');
    // 🔴 The decisive pair: nothing reached the archive, and the debt is still owed.
    expect(delivered).toEqual([]);
    expect(result.archivedThisRun).toEqual([]);
    expect(result.failedThisRun).toEqual([]);
    expect(result.state.pending).toEqual([C1]);
    const h = storedHeader('grok', ACCOUNT_A, store.data);
    expect(h.pendingCount).toBe(1);
    expect(h.archivedCount).toBe(0);
    expect(h.suspended?.observed?.value).toBe(await fp(store, ACCOUNT_B));
  });

  it('🔴 the same body under the RIGHT account settles, and the lease is not re-taken', async () => {
    const store = memoryStore();
    await seed(store, ACCOUNT_A, { owed: [C1], leaseFor: ACCOUNT_A });
    const server = backend({ listAccount: ACCOUNT_A, detailAccount: ACCOUNT_A });
    const { report, delivered } = run(store, server.http, ACCOUNT_A);
    const result = await report;
    expect(delivered).toHaveLength(1);
    expect(result.archivedThisRun).toEqual([C1]);
    const h = storedHeader('grok', ACCOUNT_A, store.data);
    expect(h.suspended).toBeUndefined();
    // The lease is the one the scope already had — this run confirmed it, it did not rewrite it.
    expect(h.accountLease?.at).toBe(1);
  });
});

describe('W199-G · a refused run writes nothing', () => {
  it('🔴 a lease that disagrees with its own scope key refuses, and touches no record', async () => {
    const store = memoryStore();
    // A record this build cannot produce, written by hand: the guard exists for records we did
    // not write (the same one `lib/coverage-read.ts` applies to a record and its address).
    const wrong: BackfillHeader = {
      v: 2,
      platform: 'grok',
      scope: ACCOUNT_A,
      totalKnown: null,
      totalSource: 'unknown',
      enumCursor: { offset: 0, complete: true },
      pendingCount: 0,
      archivedCount: 0,
      detailToday: { day: '2026-09-26', count: 0 },
      accountLease: {
        value: await fp(store, ACCOUNT_B),
        saltId: saltIdOf(store),
        source: 'response-body-platform-uid',
        at: 1,
      },
      halted: null,
    };
    store.data[stateKey('grok', ACCOUNT_A)] = wrong;
    const server = backend({ listAccount: ACCOUNT_A, detailAccount: ACCOUNT_A });

    const result = await run(store, server.http, ACCOUNT_A).report;

    expect(result.halted?.reason).toBe('account-changed');
    expect(server.calls).toEqual([]);

    const after = storedHeader('grok', ACCOUNT_A, store.data);
    // 🔴 The parts a refusal may not touch: the lease it could not agree with, and every
    //    count that says what is owed. The refusal is about attributing work, not about the
    //    work — so it moves no id and rewrites no lease.
    expect(after.accountLease).toEqual(wrong.accountLease);
    expect(after.pendingCount).toBe(0);
    expect(after.archivedCount).toBe(0);
    // 🔴 And it is **not** a suspension: a suspension names the account that answered instead
    //    and is lifted by seeing that account again, and here there is no such account — the
    //    record contradicts the scope key, and a human has to look.
    expect(after.suspended).toBeUndefined();
    // The stop is still written, so the tick holds this scope and the popup can say why
    // instead of showing silence.
    expect(after.halted?.reason).toBe('account-changed');
    expect(JSON.stringify(after)).not.toContain(ACCOUNT_B);
  });

  it('🔴 an unreadable header is refused before the lease is even considered', async () => {
    const store = memoryStore();
    store.data[stateKey('grok', ACCOUNT_A)] = { v: 2, platform: 'grok', scope: ACCOUNT_A, nope: true };
    const server = backend({ listAccount: ACCOUNT_B });
    const result = await run(store, server.http, ACCOUNT_A).report;
    // `openLedger`'s refusal, unchanged: an unreadable record is not a record to reason about.
    expect(result.halted?.reason).toBe('state-unreadable');
    expect(server.calls).toEqual([]);
  });
});

// ===========================================================================
// Part C · W303 · ChatGPT's request-local header identity is its run lease
// ===========================================================================
describe('W303 · ChatGPT checks the request-local account header on every response', () => {
  const workspace = 'fixture-shared-workspace';
  const scope = chatGptWorkspaceScope(workspace)!;

  it('carries the exact backfill request header through the page reply and tab port', async () => {
    const requestHeader = 'acct-backfill-fixture';
    const reply = await serveBackfillFetch(
      'https://chatgpt.com/backend-api/conversations?offset=0&limit=100',
      'https://chatgpt.com',
      async () => ({
        status: 200,
        text: async () => JSON.stringify({ items: [] }),
        chatgptAccountIdHeader: requestHeader,
      }) as never,
    );
    expect(reply).toMatchObject({ ok: true, chatgptAccountIdHeader: requestHeader });

    const store = memoryStore();
    const port = tabHttpPort(7, async () => reply, undefined, store);
    const response = await port('https://chatgpt.com/backend-api/conversations?offset=0&limit=100');
    expect(response.chatgptAccountIdentity).toEqual({
      value: await fingerprintFor(store, requestHeader),
      saltId: saltIdOf(store),
      source: 'request-header-chatgpt-account-id',
    });
    expect(JSON.stringify(response)).not.toContain(requestHeader);
  });

  function chatGptPort(runStore: ReturnType<typeof memoryStore>, opts: {
    listHeader?: string | null;
    detailHeader?: string | null;
    listIds?: string[];
    calls?: string[];
  }) {
    const calls = opts.calls ?? [];
    const accountIdentity = async (header: string | null | undefined) => header
      ? {
          value: await fingerprintFor(runStore, header),
          saltId: saltIdOf(runStore),
          source: 'request-header-chatgpt-account-id' as const,
        }
      : null;
    const http = Object.assign(async (url: string): Promise<HttpResponse> => {
      calls.push(url);
      const parsed = new URL(url);
      if (parsed.pathname === '/backend-api/conversations') {
        return {
          status: 200,
          text: JSON.stringify({ items: (opts.listIds ?? []).map((id) => ({ id })) }),
          chatgptAccountIdentity: await accountIdentity(opts.listHeader),
        } as HttpResponse;
      }
      if (parsed.pathname.startsWith('/backend-api/conversation/')) {
        return {
          status: 200,
          text: JSON.stringify({
            conversation_id: parsed.pathname.split('/').pop(), current_node: 'node',
            mapping: { node: { message: { author: { role: 'user' }, content: { parts: ['synthetic'] } } } },
          }),
          chatgptAccountIdentity: await accountIdentity(opts.detailHeader),
        } as HttpResponse;
      }
      throw new Error(`unexpected ChatGPT fixture route: ${parsed.pathname}`);
    }, {
      chatgptWorkspace: async () => ({ ok: true as const, workspace, observed: true as const }),
    });
    return { http, calls };
  }

  function runChatGpt(store: ReturnType<typeof memoryStore>, http: unknown) {
    const delivered: CapturedFetch[] = [];
    const report = runBackfill({
      platform: 'chatgpt', origin: 'https://chatgpt.com', scope, store,
      http: http as never, clock: fakeClock(), pace: NO_WAIT,
      sink: (captured) => { delivered.push(captured); return { saved: true, sessionId: captured.sessionId }; },
    });
    return { report, delivered };
  }

  it('takes its lease from the first list reply, then accepts IDs only under that header identity', async () => {
    const store = memoryStore();
    const opened = await openLedger(store, 'chatgpt', scope);
    if (!opened.ok) throw new Error('fixture must open the unleased ChatGPT ledger');
    opened.state.enumCursor = { offset: 0, complete: false };
    await opened.ledger.save(opened.state);
    const port = chatGptPort(store, { listHeader: ACCOUNT_A, detailHeader: ACCOUNT_A, listIds: [C1] });
    const { report } = runChatGpt(store, port.http);
    const result = await report;

    expect(result.halted).toBeNull();
    expect(result.archivedThisRun).toContain(C1);
    expect(storedHeader('chatgpt', scope, store.data).accountLease).toMatchObject({
      value: await fingerprintFor(store, ACCOUNT_A), source: 'request-header-chatgpt-account-id',
    });
  });

  it('a list reply for B is not enqueued; suspends A and keeps every old pending id', async () => {
    const store = memoryStore();
    await seedChatGptLease(store, scope, ACCOUNT_A, [C1], { enumerated: false });
    const port = chatGptPort(store, { listHeader: ACCOUNT_B, detailHeader: ACCOUNT_A, listIds: [C2] });
    const { report } = runChatGpt(store, port.http);
    const result = await report;

    expect(result.halted?.reason).toBe('account-changed');
    expect(result.state.pending).toEqual([C1]);
    expect(result.newDebts).toBe(0);
    expect(result.accountChangedTo?.id).toBe(
      chatGptWorkspaceScope(fingerprintedChatGptWorkspace(await fingerprintFor(store, ACCOUNT_B))),
    );
    expect(result.accountChangedTo?.id).not.toContain(ACCOUNT_B);
    expect(storedHeader('chatgpt', scope, store.data).suspended).toMatchObject({
      reason: 'account-changed', lease: { value: await fingerprintFor(store, ACCOUNT_A) },
      observed: { value: await fingerprintFor(store, ACCOUNT_B) },
    });
    expect(JSON.stringify(store.data)).not.toContain(ACCOUNT_A);
    expect(JSON.stringify(store.data)).not.toContain(ACCOUNT_B);
  });

  it('a detail reply for B is neither settled nor delivered; preserves all pending ids', async () => {
    const store = memoryStore();
    await seedChatGptLease(store, scope, ACCOUNT_A, [C1, C2]);
    const port = chatGptPort(store, { listHeader: ACCOUNT_A, detailHeader: ACCOUNT_B });
    const { report, delivered } = runChatGpt(store, port.http);
    const result = await report;

    expect(result.halted?.reason).toBe('account-changed');
    expect(result.state.pending).toEqual([C1, C2]);
    expect(result.archivedThisRun).toEqual([]);
    expect(delivered).toEqual([]);
    expect(storedHeader('chatgpt', scope, store.data).suspended?.reason).toBe('account-changed');
  });

  it('a detail mismatch suspends the lease established by this same first run', async () => {
    const store = memoryStore();
    const port = chatGptPort(store, { listHeader: ACCOUNT_A, detailHeader: ACCOUNT_B, listIds: [C1] });
    const { report, delivered } = runChatGpt(store, port.http);
    const result = await report;

    expect(result.halted?.reason).toBe('account-changed');
    expect(result.state.pending).toEqual([C1]);
    expect(result.archivedThisRun).toEqual([]);
    expect(delivered).toEqual([]);
    expect(storedHeader('chatgpt', scope, store.data).suspended).toMatchObject({
      reason: 'account-changed',
      lease: { value: await fingerprintFor(store, ACCOUNT_A) },
      observed: { value: await fingerprintFor(store, ACCOUNT_B) },
    });
  });

  it('missing request identity fails closed before list IDs or detail debt can change', async () => {
    const store = memoryStore();
    const port = chatGptPort(store, { listHeader: null, listIds: [C2] });
    const { report } = runChatGpt(store, port.http);
    const result = await report;

    expect(result.halted?.reason).toBe('refused-unknown');
    expect(result.newDebts).toBe(0);
    expect(result.state.pending).toEqual([]);
    expect(result.state.enumCursor.offset).toBe(0);
    expect(storedHeader('chatgpt', scope, store.data).accountLease).toBeUndefined();
    expect(storedHeader('chatgpt', scope, store.data).suspended).toBeUndefined();
    expect(resolveChatGptWorkspace({ accountIds: [ACCOUNT_A, ACCOUNT_B] })).toEqual({
      ok: false, reason: 'workspace-ambiguous', observed: true,
    });
  });

  it('a distinct header identity mismatches even while the canonical workspace scope stays the same', async () => {
    const store = memoryStore();
    await seedChatGptLease(store, scope, ACCOUNT_A);
    const port = chatGptPort(store, { listHeader: ACCOUNT_B, detailHeader: ACCOUNT_B, listIds: [C2] });
    const { report } = runChatGpt(store, port.http);
    const result = await report;

    expect(scope).toBe(chatGptWorkspaceScope(workspace));
    expect(result.halted?.reason).toBe('account-changed');
    expect(result.state.pending).not.toContain(C2);
  });

  it('a salt change after this run has established its lease makes the response unknown', async () => {
    const store = memoryStore();
    const replacementStore = memoryStore();
    const replacementSalt = await loadOrCreateAccountSalt(replacementStore);
    if (!replacementSalt || replacementSalt === 'unreadable') throw new Error('fixture must produce replacement salt');
    const identity = async (target: ReturnType<typeof memoryStore>, header: string) => ({
      value: await fingerprintFor(target, header),
      saltId: saltIdOf(target),
      source: 'request-header-chatgpt-account-id' as const,
    });
    const first = await identity(store, ACCOUNT_A);
    const incomparable = await identity(replacementStore, ACCOUNT_A);
    const delivered: CapturedFetch[] = [];
    const http: HttpPort = async (url) => new URL(url).pathname === '/backend-api/conversations'
      ? { status: 200, text: JSON.stringify({ items: [{ id: C1 }] }), chatgptAccountIdentity: first }
      : {
          status: 200,
          text: JSON.stringify({
            conversation_id: C1, current_node: 'node',
            mapping: { node: { message: { author: { role: 'user' }, content: { parts: ['synthetic'] } } } },
          }),
          chatgptAccountIdentity: incomparable,
        };
    const report = await runBackfill({
      platform: 'chatgpt', origin: 'https://chatgpt.com', scope, store,
      http, clock: fakeClock(), pace: NO_WAIT,
      sink: (captured) => { delivered.push(captured); return { saved: true, sessionId: captured.sessionId }; },
    });

    expect(report.halted?.reason).toBe('refused-unknown');
    expect(report.state.pending).toEqual([C1]);
    expect(report.archivedThisRun).toEqual([]);
    expect(delivered).toEqual([]);
    expect(storedHeader('chatgpt', scope, store.data).suspended).toBeUndefined();
  });

  it('the same ID under a replacement salt is incomparable and refreshes the lease without suspension', async () => {
    const store = memoryStore();
    await seedChatGptLease(store, scope, ACCOUNT_A);
    const replacementStore = memoryStore();
    const replacementSalt = await loadOrCreateAccountSalt(replacementStore);
    if (!replacementSalt || replacementSalt === 'unreadable') throw new Error('fixture must produce replacement salt');
    store.data[ACCOUNT_SALT_KEY] = replacementStore.data[ACCOUNT_SALT_KEY];
    const port = chatGptPort(store, { listHeader: ACCOUNT_A, detailHeader: ACCOUNT_A, listIds: [C1] });
    const { report } = runChatGpt(store, port.http);
    const result = await report;

    expect(result.halted).toBeNull();
    expect(storedHeader('chatgpt', scope, store.data).suspended).toBeUndefined();
    expect(storedHeader('chatgpt', scope, store.data).accountLease?.saltId).toBe(replacementSalt.id);
  });
});
