/**
 * W76b · **The four findings of the R76 review, pinned one tick at a time.**
 *
 * The review (`nm/R76-grok.log`) found four defects in W76's fair rotation. Each
 * one is a *different* way a single wake can do the wrong thing, so each has its
 * own describe block and its own observable fact — the requests the page was
 * really asked for, the archives that really grew, and the trace a later reader
 * really gets. Nothing here is asserted about a field the fix introduced alone:
 * every red run below fails on a count, a URL or a record that exists in both
 * revisions (`tests/w76-fair-tick-scheduling.test.ts` states the same rule for
 * the same reason).
 *
 *  1. **Two sites can receive requests in one tick.** The scope resolution asks
 *     the platform itself (`GET /api/organizations`) *before* the port and hold
 *     checks, and W76's walk `continue`d past a target that was held — so one
 *     wake could spend the organization request on one platform and a list plus a
 *     detail on another.
 *  2. **A non-integer cursor served nobody forever**, and a cursor write that
 *     keeps failing pinned the rotation's start.
 *  3. **The tick-global gates were asked lazily** — only when some target had no
 *     tab — so a wake whose every target had a tab and was held reported
 *     `no-runnable-target` while the switch was off or the host was paused, and
 *     never ran the resume `hello`.
 *  4. **A run that fetches nothing still spent the tick**: a spent daily body
 *     quota, or a record this build cannot read, are both decidable before the
 *     run and are now skips.
 *
 * ## What is real here
 * `entrypoints/background.ts` (booted, with all of its own gates), the real
 * `runAlarmTick`/`backfillTickSettled`, `resolveClaudeOrg` + `createClaudePageScope`
 * (the page-side resolver the content script builds), `handleBackfillMessage` /
 * `serveBackfillFetch` from the tab channel, the real engine, ledger and debt
 * store, and the real trace. Faked: the browser, the page's `fetch`, and the
 * native host.
 *
 * ## 🔴 No network, no account, no real id, and no invented request
 * Every organization, scope and conversation id below is synthetic. The page's
 * fetch is a pure function over a route table and records **every** URL it is
 * asked for, so "this platform sent no request" is measured rather than asserted
 * about. A path the test did not plan is a throw, not a silent 404 — the one
 * exception is the chatgpt list/detail pair, which is planned by default precisely
 * because the *red* runs below are expected to reach it.
 *
 * 🔴 Cases marked **guard** pass on `096dccc` as well, and are labelled rather than
 *    dressed up as reds: they are the starvation sides of findings 1 and 3, and
 *    they are what stops this fix from trading one starvation for another.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { withI18n } from './i18n-harness';
import { IDBFactory } from 'fake-indexeddb';
import { handleBackfillMessage, rememberTab, type TabQueryRow } from '../lib/backfill/tab-port';
import { createClaudePageScope, type ClaudePageScope } from '../lib/backfill/claude-page';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

const CHATGPT = 'chatgpt';
const ORIGIN = 'https://chatgpt.com';
const CLAUDE = 'claude';
const CLAUDE_ORIGIN = 'https://claude.ai';
/** A synthetic organization id — the shape `isClaudeOrgId` accepts, and nothing else. */
const ORG = 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee';
const RESOLVE_PATH = '/api/organizations';
const RESOLVE_URL = `${CLAUDE_ORIGIN}${RESOLVE_PATH}`;
/** Two targets of one reachable platform: a port is per origin, so one tab serves both. */
const A = 'acct-w76b-a';
const B = 'acct-w76b-b';
/** Enough conversations that no scope runs out of debts inside these tests. */
const IDS = Array.from(
  { length: 8 },
  (_unused, i) => `c1111111-0000-4000-8000-00000000000${i + 1}`,
);
/**
 * The literal cursor key. Spelled out rather than imported, so this file tests the
 * key that ships rather than a constant the fix chose.
 */
const CURSOR_KEY = 'cs_backfill_cursor_v1';

interface TargetRow { platform: string; origin: string; scope: string }
const chatgpt = (scope: string): TargetRow => ({ platform: CHATGPT, origin: ORIGIN, scope });
const claude = (scope: string): TargetRow => ({ platform: CLAUDE, origin: CLAUDE_ORIGIN, scope });

// ---------------------------------------------------------------------------
// The page side: one tab per origin, each with its own page scope and fetch
// ---------------------------------------------------------------------------

interface FakeTab {
  origin: string;
  /** The real page-side resolver, as the content script builds it — claude.ai only. */
  scope: ClaudePageScope | null;
}

const tabs = new Map<number, FakeTab>();
/** Every URL the page was actually asked for, in order — the only request evidence. */
const pageCalls: string[] = [];
/** What the platform answers. A path with no route is a failure the test did not expect. */
type Route = (u: URL) => { status: number; text: string };
let routes: Record<string, Route> = {};
/** true ⇒ the cursor write fails the way a full-quota storage.local does. */
let cursorWriteFails = false;

const jsonRoute = (body: () => string, status = 200): Route => () => ({ status, text: body() });

/** The chatgpt list/detail pair: one page of conversations, then the empty page that ends it. */
function chatgptRoute(u: URL): Route {
  if (u.pathname === '/backend-api/conversations') {
    return jsonRoute(() => JSON.stringify({
      items: IDS.slice(Number(u.searchParams.get('offset') ?? '0')).map((id) => ({ id })),
      total: IDS.length,
    }));
  }
  return jsonRoute(() => JSON.stringify({
    mapping: { n1: { id: 'n1', message: { content: { parts: ['synthetic'] } } } },
    current_node: 'n1',
    account_id: decodeURIComponent(u.pathname.replace('/backend-api/conversation/', '')),
  }));
}

const pageFetch = async (url: string) => {
  pageCalls.push(url);
  const u = new URL(url);
  const route = routes[u.pathname]
    ?? (u.pathname.startsWith('/backend-api/') ? chatgptRoute(u) : null);
  if (!route) throw new Error(`the page was asked for an unexpected path: ${u.pathname}`);
  const answer = route(u);
  return { status: answer.status, text: async () => answer.text };
};

/** The content script's listener, in its own order: the organization question, then the fetch channel. */
function contentScriptListener(tab: FakeTab, message: unknown): Promise<unknown> | null {
  const orgPending = tab.scope?.handleMessage(message);
  if (orgPending) return orgPending;
  return handleBackfillMessage(message, tab.origin, pageFetch, undefined, tab.scope?.allowedScope() ?? null);
}

/** Every URL the page was asked for on behalf of `origin`. */
const callsTo = (origin: string): string[] => pageCalls.filter((u) => u.startsWith(origin));

// ---------------------------------------------------------------------------
// The fake browser, and the real background booted against it
// ---------------------------------------------------------------------------

const store: Record<string, unknown> = {};
const runtimeListeners: Array<(m: any, s: any, r: any) => any> = [];
const alarmListeners: Array<(a: any) => void> = [];
const alarmBook = new Map<string, { periodInMinutes?: number }>();
/** What `tabs.query({})` returns — the recovery sweep's only view of the browser. */
const queriedTabs: TabQueryRow[] = [];
let host: SyntheticHost;
/**
 * How many `hello` requests the extension actually tried to send.
 *
 * 🔴 Counted here rather than read off `host.helloCount()`: a *down* host throws
 *    before the synthetic host records anything, and "the host was not there" is
 *    exactly the state the resume attempt has to be visible in.
 */
let helloAttempts = 0;

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { runtimeListeners.push(fn); } },
    sendNativeMessage: (h: string, m: unknown) => {
      if ((m as { type?: unknown } | null)?.type === 'hello') helloAttempts += 1;
      return host.sendNativeMessage(h, m);
    },
  },
  storage: {
    local: {
      async get(defaults: Record<string, unknown> | null) {
        if (defaults === null) return { ...store };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(defaults)) out[k] = k in store ? store[k] : defaults[k];
        return out;
      },
      async set(values: Record<string, unknown>) {
        // 🔴 The synthetic full quota: only the cursor key is refused. Everything
        //    else — the header, the trace, the debt set — keeps working, so what
        //    the test observes is the cursor's own best-effort failure and nothing
        //    else about storage.
        if (cursorWriteFails && Object.prototype.hasOwnProperty.call(values, CURSOR_KEY)) {
          throw new Error('synthetic: storage.local refused the write');
        }
        Object.assign(store, values);
      },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
  alarms: {
    create(name: string, info: any) { alarmBook.set(name, info); },
    async clear(name: string) { return alarmBook.delete(name); },
    async get(name: string) { return alarmBook.get(name) ?? undefined; },
    onAlarm: { addListener(fn: any) { alarmListeners.push(fn); } },
  },
  tabs: {
    async query() { return queriedTabs.map((t) => ({ ...t })); },
    async sendMessage(tabId: number, message: unknown) {
      const tab = tabs.get(tabId);
      if (!tab) throw new Error('Could not establish connection. Receiving end does not exist.');
      const pending = contentScriptListener(tab, message);
      if (!pending) return undefined;
      return await pending;
    },
  },
};

/** A fake clock through background's test seam, so the test does not really sleep. */
let runtimeNow = 1_700_000_000_000;
const runtimeClock = {
  now: () => runtimeNow,
  sleep: async (ms: number) => { runtimeNow += ms; },
};

async function bootBackground(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({ clock: runtimeClock, random: () => 0 });
  if (runtimeListeners.length === 0) await mod.default();
  return mod;
}

async function enableBackfill(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
}

/** Seed the registry directly: array order **is** the walk's order. */
function seedTargets(rows: TargetRow[]): void {
  store['cs_backfill_targets_v1'] = rows.map((row) => ({ ...row, at: 1 }));
}

/**
 * A live, answering tab for `origin` — registered in the tab registry the port
 * check reads *and* in the tab map the channel sends to. `pageScope` is the
 * claude.ai page-side resolver; every other platform's page has none.
 */
async function openTab(tabId: number, origin: string, pageScope: ClaudePageScope | null = null): Promise<void> {
  const { browserLocalStore } = await import('../lib/backfill/store');
  tabs.set(tabId, { origin, scope: pageScope });
  queriedTabs.push({ id: tabId });
  await rememberTab(browserLocalStore(), { tabId, origin, at: 1 });
}

/** The real page-side claude.ai scope, with a cookie that names nothing. */
function claudePageScope(): ClaudePageScope {
  return createClaudePageScope({
    pageOrigin: CLAUDE_ORIGIN,
    fetchImpl: pageFetch as never,
    readCookie: () => '',
  });
}

/** **How many conversations this target has actually archived.** */
async function archived(row: TargetRow): Promise<number> {
  const { loadState } = await import('../lib/backfill/engine');
  const { browserLocalStore } = await import('../lib/backfill/store');
  const state = await loadState(browserLocalStore()!, row.platform, row.scope);
  return state?.archived.length ?? 0;
}

async function writeHalt(row: TargetRow, reason: string, clock?: typeof runtimeClock): Promise<void> {
  const { recordBackfillHalt } = await import('../lib/backfill/engine');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await recordBackfillHalt(browserLocalStore(), {
    platform: row.platform, scope: row.scope, reason: reason as never, detail: 'synthetic',
    ...(clock ? { clock } : {}),
  });
}

/** One alarm wake through the listener the browser would call, settled. */
async function tick(mod: any): Promise<void> {
  alarmListeners[0]!({ name: 'cs-backfill-tick' });
  await mod.backfillTickSettled();
}

/** The trace as a later reader has it — from storage, not from memory. */
async function trace(): Promise<any> {
  const { loadLastTick } = await import('../lib/backfill/alarm');
  const { browserLocalStore } = await import('../lib/backfill/store');
  return await loadLastTick(browserLocalStore());
}

/** The scopes whose archive grew across one wake — `null` when none did. */
async function servedByOneTick(mod: any, rows: TargetRow[]): Promise<string | null> {
  const before: number[] = [];
  for (const row of rows) before.push(await archived(row));
  await tick(mod);
  const moved: string[] = [];
  for (let i = 0; i < rows.length; i += 1) {
    if (await archived(rows[i]!) !== before[i]) moved.push(rows[i]!.scope);
  }
  // One wake serves at most one platform, so more than one moving count is itself
  // a failure — said here rather than left to whichever assertion reads next.
  expect(moved.length, `one wake served ${JSON.stringify(moved)}`).toBeLessThanOrEqual(1);
  return moved[0] ?? null;
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  runtimeListeners.length = 0;
  alarmListeners.length = 0;
  alarmBook.clear();
  tabs.clear();
  queriedTabs.length = 0;
  pageCalls.length = 0;
  routes = {};
  cursorWriteFails = false;
  helloAttempts = 0;
  runtimeNow = 1_700_000_000_000;
  host = createSyntheticHost({ up: true });
  (globalThis as any).indexedDB = new IDBFactory();
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

/**
 * The Claude row an interrupted registration leaves behind: a sentinel-scoped
 * target plus a transient halt whose backoff has already elapsed, so this wake is
 * the one that asks the platform again.
 */
async function unresolvedClaudeRowWithRetryDue(): Promise<void> {
  const { browserLocalStore } = await import('../lib/backfill/store');
  const { recordBackfillHalt } = await import('../lib/backfill/engine');
  await recordBackfillHalt(browserLocalStore(), {
    platform: CLAUDE,
    scope: 'default',
    reason: 'transport-error',
    detail: 'synthetic: a previous registration failed transiently',
    clock: { now: () => runtimeNow - 3_600_000, sleep: async () => {} },
    random: () => 0,
  });
}

// ===========================================================================
// W76b-1 · one wake, one platform's requests
// ===========================================================================

describe('W76b-1 · a wake that asked the platform stops there', () => {
  it('🔴 a failed organizations request does not hand the same wake to the next platform', async () => {
    const rows = [claude('default'), chatgpt(A)];
    seedTargets(rows);
    await enableBackfill();
    await openTab(1, CLAUDE_ORIGIN, claudePageScope());
    await openTab(2, ORIGIN);
    await unresolvedClaudeRowWithRetryDue();
    // The endpoint refuses: the resolver's own transient stop, and the review's
    // scenario — a request that really did go out on the tick's behalf.
    routes[RESOLVE_PATH] = () => ({ status: 500, text: 'synthetic server error' });

    const mod = await bootBackground();
    pageCalls.length = 0;
    await tick(mod);

    console.log('[W76b-1a] claude calls:', JSON.stringify(callsTo(CLAUDE_ORIGIN)));
    console.log('[W76b-1a] chatgpt calls:', JSON.stringify(callsTo(ORIGIN)));
    // 🔴 The measured fact first: the organization request went out, and the
    //    platform behind it was *not* also given the wake.
    expect(callsTo(CLAUDE_ORIGIN)).toEqual([RESOLVE_URL]);
    expect(callsTo(ORIGIN)).toEqual([]);
    expect(await archived(rows[1]!)).toBe(0);

    // And the trace says which of the two it was.
    const rec = await trace();
    console.log('[W76b-1a] trace:', JSON.stringify(rec?.schedule), 'reason:', rec?.reason);
    expect(rec?.reason).toBe('scope-asked');
    expect(rec?.schedule).toEqual({
      served: CLAUDE,
      skipped: [{ platform: CLAUDE, reason: 'waiting-retry' }],
    });
  });

  it('🔴 a resolved organization that is itself held does not hand the wake to the next platform', async () => {
    const rows = [claude('default'), chatgpt(A)];
    seedTargets(rows);
    await enableBackfill();
    await openTab(1, CLAUDE_ORIGIN, claudePageScope());
    await openTab(2, ORIGIN);
    await unresolvedClaudeRowWithRetryDue();
    // The endpoint names exactly one organization, so the resolver succeeds — and
    // the organization it names is already stopped for a reason of its own.
    routes[RESOLVE_PATH] = jsonRoute(() => JSON.stringify([{ uuid: ORG, name: 'synthetic' }]));
    await writeHalt(claude(ORG), 'unsupported-platform');

    const mod = await bootBackground();
    pageCalls.length = 0;
    await tick(mod);

    console.log('[W76b-1b] claude calls:', JSON.stringify(callsTo(CLAUDE_ORIGIN)));
    console.log('[W76b-1b] chatgpt calls:', JSON.stringify(callsTo(ORIGIN)));
    expect(callsTo(CLAUDE_ORIGIN)).toEqual([RESOLVE_URL]);
    expect(callsTo(ORIGIN)).toEqual([]);

    const rec = await trace();
    console.log('[W76b-1b] trace:', JSON.stringify(rec?.schedule), 'reason:', rec?.reason);
    expect(rec?.reason).toBe('scope-asked');
    expect(rec?.schedule).toEqual({
      served: CLAUDE,
      skipped: [{ platform: CLAUDE, reason: 'halted' }],
    });
  });

  it('guard · a resolution the page itself answered costs nothing, so the walk carries on', async () => {
    const rows = [claude('default'), chatgpt(A)];
    seedTargets(rows);
    await enableBackfill();
    const scope = claudePageScope();
    // 🔴 The page's own request already named the organization: the resolver's
    //    first source answers, and `fetchOrganizations` is never called. This is
    //    the case that must NOT stop the walk — an unresolved row that is free to
    //    resolve must not be able to starve the platforms behind it.
    scope.rememberRequest(`${CLAUDE_ORIGIN}${RESOLVE_PATH}/${ORG}/chat_conversations`);
    await openTab(1, CLAUDE_ORIGIN, scope);
    await openTab(2, ORIGIN);
    await unresolvedClaudeRowWithRetryDue();
    await writeHalt(claude(ORG), 'unsupported-platform');

    const mod = await bootBackground();
    pageCalls.length = 0;
    const served = await servedByOneTick(mod, rows);
    console.log('[W76b-1c] served:', served, '· claude calls:', JSON.stringify(callsTo(CLAUDE_ORIGIN)));
    // No request at all was issued for claude — the answer was already in hand.
    expect(callsTo(CLAUDE_ORIGIN)).toEqual([]);
    // …so the wake belongs to the platform that can be served.
    expect(served).toBe(A);
    const rec = await trace();
    expect(rec?.schedule).toEqual({
      served: CHATGPT,
      skipped: [{ platform: CLAUDE, reason: 'halted' }],
    });
  });

  it('guard · a failed question with no page to ask is not a request, and does not stop the walk', async () => {
    const rows = [claude('default'), chatgpt(A)];
    seedTargets(rows);
    await enableBackfill();
    // No claude.ai tab at all: the question cannot reach a page, so nothing can
    // have been asked of the platform — the channel failure and the page's own
    // failed request are the same value, and this is the side of that ambiguity
    // that must not cost the other platforms their wake.
    await openTab(2, ORIGIN);
    await unresolvedClaudeRowWithRetryDue();

    const mod = await bootBackground();
    pageCalls.length = 0;
    const served = await servedByOneTick(mod, rows);
    console.log('[W76b-1d] served:', served, '· claude calls:', JSON.stringify(callsTo(CLAUDE_ORIGIN)));
    expect(callsTo(CLAUDE_ORIGIN)).toEqual([]);
    expect(served).toBe(A);
    const rec = await trace();
    expect(rec?.schedule).toEqual({
      served: CHATGPT,
      skipped: [{ platform: CLAUDE, reason: 'no-http-port' }],
    });
  });
});

// ===========================================================================
// W76b-2 · the cursor is a position, and it moves
// ===========================================================================

describe('W76b-2 · the cursor', () => {
  it('🔴 a non-integer cursor is not a position: the walk starts at the head instead of serving nobody', async () => {
    const rows = [chatgpt(A), chatgpt(B)];
    seedTargets(rows);
    await enableBackfill();
    await openTab(11, ORIGIN);
    // A byte at the cursor key that satisfies "finite number ≥ 0" but indexes
    // nothing: `(1.5 + 1) % 2` is not an integer and `targets[0.5]` does not exist.
    store[CURSOR_KEY] = { served: 1.5 };

    const mod = await bootBackground();
    const served = await servedByOneTick(mod, rows);
    console.log('[W76b-2a] served:', served, '· cursor now:', JSON.stringify(store[CURSOR_KEY]));
    // Somebody is served — and the unreadable byte is replaced by a real index, so
    // the leg cannot stay stuck on it.
    expect(served).toBe(A);
    expect(store[CURSOR_KEY]).toEqual({ served: 0 });

    // And the next wake really moves on: the cursor is honoured, not merely written.
    expect(await servedByOneTick(mod, rows)).toBe(B);
  });

  it('🔴 a cursor write that keeps failing still advances the rotation', async () => {
    const rows = [chatgpt(A), chatgpt(B)];
    seedTargets(rows);
    await enableBackfill();
    await openTab(11, ORIGIN);
    cursorWriteFails = true;

    const mod = await bootBackground();
    const first = await servedByOneTick(mod, rows);
    const second = await servedByOneTick(mod, rows);
    console.log('[W76b-2b] served:', JSON.stringify([first, second]), '· cursor key:', JSON.stringify(store[CURSOR_KEY]));
    // The write really did fail: without this the test could pass because storage
    // worked, which would prove nothing about the path it is here for.
    expect(store[CURSOR_KEY]).toBeUndefined();
    // The rotation is still a rotation: the second wake is the platform behind the
    // first, not the same one again.
    expect([first, second]).toEqual([A, B]);
  });
});

// ===========================================================================
// W76b-3 · the gates are asked before the walk, every wake
// ===========================================================================

describe('W76b-3 · the tick-global gates are recorded even when every tabbed target is held', () => {
  it('🔴 the switch being off is reported as disabled, not as "nothing could run"', async () => {
    const rows = [chatgpt(A), chatgpt(B)];
    seedTargets(rows);
    // 🔴 Deliberately NOT enabled: the switch is off, and both targets have a live
    //    tab and a stop that holds. The walk alone would find nothing to run and
    //    say so; that answer hides the switch.
    await openTab(11, ORIGIN);
    await writeHalt(rows[0]!, 'unsupported-platform');
    await writeHalt(rows[1]!, 'unsupported-platform');

    const mod = await bootBackground();
    await tick(mod);
    const rec = await trace();
    console.log('[W76b-3a] reason:', rec?.reason, '· schedule:', JSON.stringify(rec?.schedule));
    expect(rec?.reason).toBe('disabled');
    expect(rows.length).toBe(2);
  });

  it('🔴 a paused host is reported as host-paused, and the resume hello really runs', async () => {
    const rows = [chatgpt(A)];
    seedTargets(rows);
    await enableBackfill();
    await openTab(11, ORIGIN);
    await writeHalt(rows[0]!, 'unsupported-platform');
    // The pause a failed delivery leaves behind, and a host that is not answering:
    // the hello is the only thing that can lift it.
    host = createSyntheticHost({ up: false });
    const { setHostPause } = await import('../lib/host-status');
    const { browserLocalStore } = await import('../lib/backfill/store');
    await setHostPause(browserLocalStore(), { reason: 'host-unavailable', at: 1 });

    const mod = await bootBackground();
    await tick(mod);
    const rec = await trace();
    console.log('[W76b-3b] reason:', rec?.reason, '· hello attempts:', helloAttempts);
    expect(rec?.reason).toBe('host-paused');
    // "as it did before W76": the resume attempt is the thing that decides a paused
    // tick, and a held target must not be able to keep it from being made.
    expect(helloAttempts).toBeGreaterThan(0);
  });
});

// ===========================================================================
// W76b-4 · a run that would fetch nothing does not spend the wake
// ===========================================================================

describe('W76b-4 · a scope whose next run would fetch nothing does not spend the wake', () => {
  it('🔴 a scope at its daily body quota is skipped, and the platform behind it is served', async () => {
    const rows = [chatgpt(A), chatgpt(B)];
    seedTargets(rows);
    await enableBackfill();
    await openTab(11, ORIGIN);
    /**
     * The capped scope, built out of the records a real run would have written:
     * the list is read to its end, three bodies are still owed, and today's drawn
     * quota is spent. Written by hand rather than reached by a run because reaching
     * it honestly takes a hundred and fifty body fetches; the first assertion below
     * is what makes the fixture answerable — the engine's own run on this exact
     * state is what says the state is a capped one.
     */
    const { initialState, dayKeyOf } = await import('../lib/backfill/types');
    const { saveHeader } = await import('../lib/backfill/ledger');
    const { applyDebtDiff } = await import('../lib/backfill/debt-store');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const owed = IDS.slice(0, 3);
    // The debt set, through the store's own write path, and the header through the
    // ledger's — so the counts the header carries are derived by `headerOf` rather
    // than written by hand, and the two records agree about what is owed.
    await applyDebtDiff(CHATGPT, A, { enqueue: owed, settle: [], drop: [] }, 1);
    await saveHeader(browserLocalStore()!, {
      ...initialState(CHATGPT, A),
      enumCursor: { offset: owed.length, complete: true },
      pending: owed,
      detailToday: { day: dayKeyOf(runtimeNow), count: 0, cap: 0 },
    });

    // 🔴 The premise, checked against the engine itself rather than assumed: this
    //    scope's next run issues no request and stops at `daily-cap`. The port it is
    //    handed throws, so a run that fetched anything would fail here instead.
    const { runBackfill } = await import('../lib/backfill/engine');
    const report = await runBackfill({
      platform: CHATGPT, origin: ORIGIN, scope: A, store: browserLocalStore()!,
      // The same clock the tick gives it, so "today" is the day the fixture's cap
      // was drawn for — otherwise the engine rolls a new day and draws a new cap.
      clock: runtimeClock,
      http: async () => { throw new Error('a capped run must not fetch'); },
    });
    console.log('[W76b-4a] direct run on the capped scope:', report.stopped);
    expect(report.stopped).toBe('daily-cap');
    expect(pageCalls).toEqual([]);

    // 🔴 One wake: the capped scope is at the head, so the walk meets it first. It
    //    must be passed over, and the platform behind it — which really does have
    //    conversations to fetch — must be the one that is served.
    const mod = await bootBackground();
    const served = await servedByOneTick(mod, rows);
    console.log('[W76b-4a] served:', served, '· calls:', JSON.stringify(pageCalls));
    expect(served).toBe(B);
    // The capped scope fetched nothing, as its fixture says…
    expect(await archived(rows[0]!)).toBe(0);
    // …and the served one really did the work — an archive that grew is evidence
    // that exists in both revisions, not a field this fix introduced.
    expect(await archived(rows[1]!)).toBe(1);

    const rec = await trace();
    console.log('[W76b-4a] schedule:', JSON.stringify(rec?.schedule));
    expect(rec?.schedule?.skipped).toContainEqual({ platform: CHATGPT, reason: 'daily-cap' });
  });
});
