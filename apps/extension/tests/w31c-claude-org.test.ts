/**
 * W31c · **The Claude organization resolver, wired in and walked end to end.**
 *
 * ## What was broken, and what this file pins
 * W31 wrote `resolveClaudeOrgOnPage` and tested it as a decision, and then **no
 * production code called it**: `backfillTargetFor` read the organization out of
 * the captured request URL, and the popup's start button registered every target
 * with `scope: 'default'` — which a scoped plan refuses by name, before any
 * request, forever. So on an account whose page had not yet made a request of its
 * own, a Claude backfill could not start at all; `org-ambiguous` was unreachable;
 * and the cookie and `/api/organizations` sources had no caller.
 *
 * The wiring this file exercises, end to end:
 *   popup click → background asks the live tab over the tab channel → the content
 *   script resolves with the page's own seen organization, then the cookie, then
 *   one `GET /api/organizations` through the backfill allowlist → the answer
 *   registers the target with that scope, or writes the named halt down.
 *
 * ## 🔴 What is real here and what is faked, said plainly
 *  · **Real**: `entrypoints/background.ts` (imported and booted, with all of its
 *    own gates and registries), `createClaudePageScope` from
 *    lib/backfill/claude-page.ts — the page-side half the content script uses,
 *    driven here with its three injected facts — `handleBackfillMessage` /
 *    `serveBackfillFetch` / `checkBackfillRequest` from lib/backfill/tab-port.ts,
 *    `resolveClaudeOrg*` from lib/backfill/claude-org.ts, `recordBackfillHalt`,
 *    `scopeRetryDue`, the real `runAlarmTick` and the real `renderPopup`.
 *  · **Faked**: the browser (storage/tabs/alarms/runtime) and the page's `fetch`.
 *    The content script itself is not imported — it boots against a real page —
 *    so the handful of lines in it that build the scope object and forward
 *    messages are pinned by the **wiring guard** at the end of this file, which
 *    reads that source. Same idiom tests/c33-startbtn.test.ts uses for the
 *    popup's button, and for the same reason.
 *
 * ## 🔴 No network, no account, no real id
 * Every organization, conversation and message below is invented. The synthetic
 * backend **throws on any path it was not given**, and every URL the leg asks the
 * page for is recorded, so "no list request was sent" is proven by the run rather
 * than asserted about it.
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { withI18n } from './i18n-harness';
import {
  BACKFILL_FETCH_MESSAGE,
  handleBackfillMessage,
  isBackfillFetchRequest,
} from '../lib/backfill/tab-port';
import { createClaudePageScope, type ClaudePageScope } from '../lib/backfill/claude-page';

const CLAUDE_ORIGIN = 'https://claude.ai';
/** Synthetic organization ids. Values only — nothing here was ever a real account. */
const ORG = 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee';
const ORG2 = '11111111-2222-3333-4444-555555555555';
/** A synthetic conversation id, in the hex shape the row's session pattern reads. */
const ID = '99999999-8888-7777-6666-555555555555';
/**
 * 🔴 W49 · A synthetic pre-W31 conversation-title-shaped scope. 32 hex characters
 * and no dash: the shape observed on a real machine sitting next to a 35-character
 * organization row, harvested from a conversation body's `name` by the identity
 * heuristic. It is not an organization id.
 */
const TITLE = '00000000000000000000000000000000';

const RESOLVE_PATH = '/api/organizations';
const RESOLVE_URL = `${CLAUDE_ORIGIN}${RESOLVE_PATH}`;
const LIST_URL = `${CLAUDE_ORIGIN}/api/organizations/${ORG}/chat_conversations`;
const DETAIL_URL = `${CLAUDE_ORIGIN}/api/organizations/${ORG}/chat_conversations/${ID}`;

// ---------------------------------------------------------------------------
// The page side: one claude.ai tab, its cookie, its own seen request, its fetch
// ---------------------------------------------------------------------------

interface FakeTab {
  origin: string;
  /** 🔴 The **real** page-side resolver, built exactly as the content script builds it. */
  scope: ClaudePageScope;
}

const tabs = new Map<number, FakeTab>();
/** Every path the page was actually asked for. */
const pageCalls: string[] = [];
/** Every URL the background tried to send on the platform's behalf. */
const sentOnBehalf: string[] = [];

/** What the platform answers. A path with no route is a failure the test did not expect. */
type Route = (u: URL) => { status: number; text: string };
let routes: Record<string, Route> = {};

function jsonRoute(body: () => string, status = 200): Route {
  return () => ({ status, text: body() });
}

/**
 * The page's own fetch. Records every URL, serves the synthetic backend, and
 * **throws on any path the test did not give it** — so a request nobody expected
 * cannot pass unnoticed.
 */
const pageFetch = async (url: string) => {
  pageCalls.push(url);
  const u = new URL(url);
  const route = routes[u.pathname];
  if (!route) throw new Error(`the page was asked for an unexpected path: ${u.pathname}`);
  const answer = route(u);
  return { status: answer.status, text: async () => answer.text };
};

/** The organizations endpoint, answering with `uuids` (or a status). */
function organizationsEndpoint(uuids: string[] | 'http-500'): Route {
  return uuids === 'http-500'
    ? () => ({ status: 500, text: 'synthetic server error' })
    : jsonRoute(() => JSON.stringify(uuids.map((uuid) => ({ uuid, name: 'synthetic' }))));
}

/**
 * 🔴 **The content script's message listener**, in the same order and with the
 * same calls as `entrypoints/dw-bridge.content.ts`'s listener: the organization
 * question first, then the fetch channel. Both handlers are the production ones.
 */
function contentScriptListener(message: unknown): Promise<unknown> | null {
  const tab = currentTab;
  if (!tab) return null;
  const orgPending = tab.scope.handleMessage(message);
  if (orgPending) return orgPending;
  return handleBackfillMessage(message, tab.origin, pageFetch, undefined, tab.scope.allowedScope());
}

/** The one live tab the test is driving. */
let currentTab: FakeTab | null = null;

// ---------------------------------------------------------------------------
// The fake browser, and the two real entry points booted against it
// ---------------------------------------------------------------------------

const store: Record<string, unknown> = {};
const backgroundListeners: Array<(m: any, s: any, r: any) => any> = [];
const alarmBook = new Map<string, unknown>();

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() {} },
    onMessage: { addListener(fn: any) { backgroundListeners.push(fn); } },
    async sendMessage() { return undefined; },
  },
  storage: {
    local: {
      async get(defaults: Record<string, unknown> | null) {
        if (defaults === null) return { ...store };
        const out: Record<string, unknown> = {};
        for (const k of Object.keys(defaults)) out[k] = k in store ? store[k] : defaults[k];
        return out;
      },
      async set(values: Record<string, unknown>) { Object.assign(store, values); },
      async remove(keys: string[]) { for (const k of keys) delete store[k]; },
    },
  },
  action: { async setBadgeText() {}, async setBadgeBackgroundColor() {}, async setTitle() {} },
  alarms: {
    create(name: string, info: any) { alarmBook.set(name, info); },
    async clear(name: string) { return alarmBook.delete(name); },
    async get(name: string) { return alarmBook.get(name) ?? undefined; },
    onAlarm: { addListener() {} },
  },
  tabs: {
    async sendMessage(tabId: number, message: unknown) {
      if (!tabs.has(tabId) || currentTab === null) {
        throw new Error('Could not establish connection. Receiving end does not exist.');
      }
      // 🔴 Every request the background tries to send on the platform's behalf is
      //    recorded **here**, before any check can refuse it. "No list request was
      //    sent" therefore cannot be satisfied by a silent refusal.
      if (isBackfillFetchRequest(message)) sentOnBehalf.push(message.url);
      const pending = contentScriptListener(message);
      if (!pending) return undefined;
      return await pending;
    },
  },
};

let runtimeNow = 1_700_000_000_000;
const runtimeClock = {
  now: () => runtimeNow,
  sleep: async (ms: number) => { runtimeNow += ms; },
};

async function bootBackground(): Promise<any> {
  const mod: any = await import('../entrypoints/background');
  mod.configureBackfillPace({ clock: runtimeClock });
  if (backgroundListeners.length === 0) await mod.default();
  return mod;
}

/** Dispatch a message the way the popup does (no sender.tab). */
async function dispatch(message: unknown): Promise<any> {
  return await new Promise((resolve) => {
    const ret = backgroundListeners[0]!(message, { id: 'popup' }, resolve);
    if (ret !== true) resolve(undefined);
  });
}

/** Dispatch a message the way a content script does (sender.tab.id is filled in by the browser). */
async function dispatchFromTab(message: unknown, tabId: number): Promise<any> {
  return await new Promise((resolve) => {
    const ret = backgroundListeners[0]!(message, { id: 'cs', tab: { id: tabId } }, resolve);
    if (ret !== true) resolve(undefined);
  });
}

/**
 * An open claude.ai page. Its resolver is the **production** one, given the three
 * facts it asks for: the origin, this page's fetch, and the cookie.
 */
function openTab(tabId: number, opts: { cookie?: string; seen?: string } = {}): ClaudePageScope {
  const cookie = opts.cookie ?? '';
  const scope = createClaudePageScope({
    pageOrigin: CLAUDE_ORIGIN,
    fetchImpl: pageFetch,
    readCookie: () => cookie,
  });
  // "The page's own request carried this organization" — recorded through the very
  // method the content script calls, not by poking at its state.
  if (opts.seen !== undefined) {
    scope.rememberRequest(`${CLAUDE_ORIGIN}${RESOLVE_PATH}/${opts.seen}/chat_conversations/synthetic`);
  }
  const entry: FakeTab = { origin: CLAUDE_ORIGIN, scope };
  tabs.set(tabId, entry);
  currentTab = entry;
  return scope;
}

/** A content script checking in. This is what "there is an open claude.ai page" looks like. */
async function tabHello(tabId: number, opts: { cookie?: string; seen?: string } = {}): Promise<ClaudePageScope> {
  const scope = openTab(tabId, opts);
  await dispatchFromTab({ type: 'cs-backfill-tab-hello', origin: CLAUDE_ORIGIN }, tabId);
  return scope;
}

async function enableBackfill(): Promise<void> {
  const { setBackfillEnabled } = await import('../lib/backfill/schedule');
  const { browserLocalStore } = await import('../lib/backfill/store');
  await setBackfillEnabled(browserLocalStore(), true);
}

async function targetsInRegistry(): Promise<any[]> {
  const { BACKFILL_TARGETS_KEY } = await import('../lib/backfill/alarm');
  const raw = store[BACKFILL_TARGETS_KEY];
  return Array.isArray(raw) ? raw : [];
}

async function headerFor(scope: string): Promise<any> {
  const { stateKey } = await import('../lib/backfill/types');
  return store[stateKey('claude', scope)];
}

/** Every request the background sent that names a conversation (list or body). */
function conversationRequests(): string[] {
  return sentOnBehalf.filter((url) => url.includes('/chat_conversations'));
}

/** The popup's own view, built from the same storage the popup reads. */
async function popupText(): Promise<{ text: string; state: any }> {
  const { browserLocalSnapshot } = await import('../lib/backfill/store');
  const { renderPopup, popupText: flatten, pickBackfillState, collectFailures } =
    await import('../lib/popup-view');
  const snapshot = await browserLocalSnapshot();
  const state = pickBackfillState(snapshot);
  const view = renderPopup({
    enabled: true,
    block: null,
    state,
    target: null,
    failures: collectFailures(snapshot),
  });
  return { text: flatten(view), state };
}

beforeEach(async () => {
  for (const k of Object.keys(store)) delete store[k];
  backgroundListeners.length = 0;
  tabs.clear();
  pageCalls.length = 0;
  sentOnBehalf.length = 0;
  alarmBook.clear();
  routes = {};
  currentTab = null;
  runtimeNow = 1_700_000_000_000;
  vi.stubGlobal('browser', withI18n(fakeBrowser));
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
  vi.resetModules();
  const { resetTickLockForTest } = await import('../lib/backfill/schedule');
  resetTickLockForTest();
});

// ---------------------------------------------------------------------------
// 1 · The popup's start button, on a page that has made no request yet
// ---------------------------------------------------------------------------
describe('W31c-1 · starting a Claude backfill resolves the organization through the page', () => {
  it('🔴 no captured URL, one organization ⇒ the page is asked, and the target is registered with it', async () => {
    const { POPUP_START_BACKFILL_MESSAGE } = await import('../lib/popup-view');
    await enableBackfill();
    await bootBackground();
    // The page has made no request of its own and set no cookie: this is exactly
    // the account W31 could not start at all.
    await tabHello(7);
    routes[RESOLVE_PATH] = organizationsEndpoint([ORG]);

    const reply = await dispatch({ type: POPUP_START_BACKFILL_MESSAGE });
    expect(reply?.ok).toBe(true);
    expect(reply.target).toEqual({ platform: 'claude', origin: CLAUDE_ORIGIN, scope: ORG });

    const written = await targetsInRegistry();
    expect(written.length).toBe(1);
    expect(written[0]).toMatchObject({ platform: 'claude', origin: CLAUDE_ORIGIN, scope: ORG });

    // 🔴 Exactly one request went out, it is the resolution-only path, and it
    //    carries no query — no conversation was asked for to find out who the user is.
    expect(pageCalls).toEqual([RESOLVE_URL]);
    expect(conversationRequests()).toEqual([]);
  });

  it('🔴 several organizations and nothing on the page ⇒ org-ambiguous, written down, and zero list requests', async () => {
    const { POPUP_START_BACKFILL_MESSAGE } = await import('../lib/popup-view');
    const { t } = await import('../lib/i18n');
    await enableBackfill();
    await bootBackground();
    await tabHello(7);
    routes[RESOLVE_PATH] = organizationsEndpoint([ORG, ORG2]);

    const reply = await dispatch({ type: POPUP_START_BACKFILL_MESSAGE });
    expect(reply?.ok).toBe(false);
    expect(reply.reason).toBe('org-ambiguous');

    // The halt is a fact about the account, and it is on the record the popup reads.
    const header = await headerFor('default');
    expect(header?.halted?.reason).toBe('org-ambiguous');
    expect(header.halted.detail).toContain('2 organizations');
    // 🔴 Neither organization id may appear anywhere the user can read.
    expect(JSON.stringify(header)).not.toContain(ORG);

    // 🔴 The popup shows the dedicated sentence — not the `other` fallback, which
    //    would print the reason code and the technical detail instead of the one
    //    action that resolves this.
    const shown = await popupText();
    expect(shown.text).toContain(t('popup.notes.halted.orgAmbiguous'));
    // 🔴 And it did not fall through to the `other` fallback, which would print the
    //    reason code and the technical detail at a user whose question is "what do
    //    I do now". (The reason code itself still appears in the progress line's
    //    `[stopped: …]` prefix, which is that line's existing contract.)
    expect(shown.text).not.toContain(
      t('popup.notes.halted.other', { reason: 'org-ambiguous', detail: header.halted.detail }),
    );

    // 🔴 One question was asked, and not one conversation: the account was never
    //    enumerated, so nothing was written off and nothing was read from another
    //    organization.
    expect(pageCalls).toEqual([RESOLVE_URL]);
    expect(conversationRequests()).toEqual([]);
  });

  it('🔴 an empty organization list says "open a conversation once", not "you have no conversations"', async () => {
    const { POPUP_START_BACKFILL_MESSAGE } = await import('../lib/popup-view');
    const { t } = await import('../lib/i18n');
    await enableBackfill();
    await bootBackground();
    await tabHello(7);
    routes[RESOLVE_PATH] = organizationsEndpoint([]);

    const reply = await dispatch({ type: POPUP_START_BACKFILL_MESSAGE });
    expect(reply?.ok).toBe(false);
    expect(reply.reason).toBe('org-unresolved');
    const header = await headerFor('default');
    expect(header?.halted?.reason).toBe('org-unresolved');

    // 🔴 The second of the two dedicated sentences, and the same rule as
    //    `org-ambiguous`: its own plain wording, not the `other` fallback (which
    //    would print the reason code and the technical detail), and it names the
    //    one action that resolves it.
    const shown = await popupText();
    expect(shown.text).toContain(t('popup.notes.halted.orgUnresolved'));
    expect(shown.text).not.toContain(
      t('popup.notes.halted.other', { reason: 'org-unresolved', detail: header.halted.detail }),
    );
    expect(conversationRequests()).toEqual([]);
  });

  it('🔴 a readable cookie answers, and the organizations endpoint is never asked', async () => {
    const { POPUP_START_BACKFILL_MESSAGE } = await import('../lib/popup-view');
    await enableBackfill();
    await bootBackground();
    await tabHello(7, { cookie: `other=1; lastActiveOrg=${ORG2}; third=2` });
    // A route that would answer, so a request here would be recorded rather than throw.
    routes[RESOLVE_PATH] = organizationsEndpoint([ORG]);

    const reply = await dispatch({ type: POPUP_START_BACKFILL_MESSAGE });
    expect(reply?.ok).toBe(true);
    expect(reply.target.scope).toBe(ORG2);
    expect((await targetsInRegistry())[0].scope).toBe(ORG2);

    // 🔴 The free source answered, so the one request this resolver may spend is not spent.
    expect(pageCalls).toEqual([]);
  });

  it('🔴 a 5xx from the organizations endpoint is a transient halt — never "no conversations"', async () => {
    const { POPUP_START_BACKFILL_MESSAGE } = await import('../lib/popup-view');
    const { t } = await import('../lib/i18n');
    await enableBackfill();
    await bootBackground();
    await tabHello(7);
    routes[RESOLVE_PATH] = organizationsEndpoint('http-500');

    const reply = await dispatch({ type: POPUP_START_BACKFILL_MESSAGE });
    expect(reply?.ok).toBe(false);
    expect(reply.reason).toBe('transport-error');

    const header = await headerFor('default');
    expect(header?.halted?.reason).toBe('transport-error');
    // A transient record carries the ladder's rung and the moment to try again.
    expect(header.halted.attempts).toBe(1);
    expect(header.halted.retryAt).toBeGreaterThan(header.halted.at);
    // 🔴 And the popup must say it is waiting, not stopped: the two records mean
    //    different things and nothing was written off.
    const shown = await popupText();
    expect(shown.text).toContain(t('popup.notes.halted.waitingRetry', {
      reason: 'transport-error',
      attempts: 1,
      minutes: Math.ceil((header.halted.retryAt - Date.now()) / 60_000),
      detail: header.halted.detail,
    }));
    expect(shown.text).not.toContain(t('popup.notes.halted.orgUnresolved'));

    // 🔴 The target is registered anyway, because a transient failure has to have
    //    something for the alarm to retry through: once a target exists, the popup's
    //    start button is hidden.
    expect((await targetsInRegistry())[0]).toMatchObject({ platform: 'claude', scope: 'default' });
    expect(conversationRequests()).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// 2 · The captured URL still wins, and still names the conversation
// ---------------------------------------------------------------------------
describe('W31c-2 · a real capture is still the strongest source', () => {
  it('🔴 a captured org-bearing URL registers that organization, with no question asked', async () => {
    const mod = await bootBackground();
    await enableBackfill();
    await tabHello(7);
    // If the background asked, this route would answer with a *different*
    // organization — so an asked question would be visible in the registry.
    routes[RESOLVE_PATH] = organizationsEndpoint([ORG2]);

    const captured = {
      url: `${DETAIL_URL}?tree=True&rendering_mode=messages&render_all_tools=true`,
      method: 'GET',
      status: 200,
      text: JSON.stringify({ uuid: ID, chat_messages: [] }),
      pageUrl: `${CLAUDE_ORIGIN}/chat/${ID}`,
      capturedAt: Date.now(),
    };
    await dispatchFromTab({ type: 'chat-captured', payload: captured }, 7);
    await mod.backfillTickSettled();

    // The page's own request carried the organization, so that is the scope — the
    // resolver's first source, and no request was spent finding it out.
    expect(pageCalls).not.toContain(RESOLVE_URL);
    const written = await targetsInRegistry();
    expect(written[0]).toMatchObject({ platform: 'claude', scope: ORG });
  });
});

// ---------------------------------------------------------------------------
// 3 · The scope the page allows is the scope the request carries
// ---------------------------------------------------------------------------
describe('W31c-3 · the allowlist compares the path segment against the page\'s own scope', () => {
  it('🔴 the same URL is refused while the page has no organization, and sent once it has one', async () => {
    routes[`/api/organizations/${ORG}/chat_conversations`] = jsonRoute(() => '[]');
    const request = { type: BACKFILL_FETCH_MESSAGE, url: LIST_URL };

    // A page that has shown nothing yet: no scope, so the `{org}` segment matches
    // nothing (scopePathMatches) and the request is refused **at the page**. That
    // is what every claude.ai request would have hit before this pass, however
    // well the organization had been resolved in the background.
    const blank = openTab(7);
    expect(blank.allowedScope()).toBeNull();
    const refused = await handleBackfillMessage(
      request, CLAUDE_ORIGIN, pageFetch, undefined, blank.allowedScope(),
    );
    expect(refused?.ok).toBe(false);
    expect(pageCalls).toEqual([]);

    // The same page once its own request has shown the organization.
    const knowing = openTab(8, { seen: ORG });
    expect(knowing.allowedScope()).toBe(ORG);
    const sent = await handleBackfillMessage(
      request, CLAUDE_ORIGIN, pageFetch, undefined, knowing.allowedScope(),
    );
    expect(sent?.ok).toBe(true);
    expect(pageCalls).toEqual([LIST_URL]);

    // 🔴 And a page whose scope is another organization refuses the URL too: the
    //    check compares, it does not accept.
    pageCalls.length = 0;
    const wrongOrg = await handleBackfillMessage(
      request, CLAUDE_ORIGIN, pageFetch, undefined, ORG2,
    );
    expect(wrongOrg?.ok).toBe(false);
    expect(pageCalls).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// 4 · The alarm retries a transient failure, and does not poll a permanent one
// ---------------------------------------------------------------------------
describe('W31c-4 · the alarm\'s side of a scope that is not known yet', () => {
  it('🔴 a transient halt is not re-asked before its backoff, and is re-asked after it', async () => {
    const { recordBackfillHalt } = await import('../lib/backfill/engine');
    const { scopeRetryDue, UNRESOLVED_SCOPE } = await import('../entrypoints/background');
    const { browserLocalStore } = await import('../lib/backfill/store');

    const s = browserLocalStore();
    const now = Date.now();

    // Nothing on the record ⇒ nothing has been decided ⇒ ask.
    expect(await scopeRetryDue(s, 'claude', UNRESOLVED_SCOPE, now)).toBe(true);

    // A transient halt, written with a clock one hour in the past: its retryAt is
    // 5 minutes after that, so it is due now.
    await recordBackfillHalt(s, {
      platform: 'claude',
      scope: UNRESOLVED_SCOPE,
      reason: 'transport-error',
      detail: 'synthetic',
      clock: { now: () => now - 3_600_000, sleep: async () => {} },
      random: () => 0,
    });
    const due = await headerFor(UNRESOLVED_SCOPE);
    expect(due.halted.retryAt).toBeLessThan(now);
    expect(await scopeRetryDue(s, 'claude', UNRESOLVED_SCOPE, now)).toBe(true);

    // The same record one minute later on the ladder: not due yet.
    await recordBackfillHalt(s, {
      platform: 'claude',
      scope: UNRESOLVED_SCOPE,
      reason: 'transport-error',
      detail: 'synthetic',
      clock: { now: () => now, sleep: async () => {} },
      random: () => 0,
    });
    const waiting = await headerFor(UNRESOLVED_SCOPE);
    expect(waiting.halted.retryAt).toBeGreaterThan(now);
    expect(await scopeRetryDue(s, 'claude', UNRESOLVED_SCOPE, now)).toBe(false);
    expect(await scopeRetryDue(s, 'claude', UNRESOLVED_SCOPE, waiting.halted.retryAt)).toBe(true);

    // 🔴 A permanent one is never due: several organizations and no signal is an
    //    answer, and asking again every tick would turn "wait for a human" into a
    //    slow poll of the account.
    await recordBackfillHalt(s, {
      platform: 'claude', scope: UNRESOLVED_SCOPE, reason: 'org-ambiguous', detail: 'synthetic',
    });
    expect(await scopeRetryDue(s, 'claude', UNRESOLVED_SCOPE, now + 86_400_000)).toBe(false);
  });

  it('🔴 the alarm re-asks, replaces the unresolved row, and runs with the organization it found', async () => {
    const { recordBackfillHalt } = await import('../lib/backfill/engine');
    const mod = await bootBackground();
    await enableBackfill();
    await tabHello(7);
    routes[RESOLVE_PATH] = organizationsEndpoint([ORG]);
    routes[`/api/organizations/${ORG}/chat_conversations`] = jsonRoute(() => '[]');

    // The state a transient failure at registration leaves behind: a sentinel-scoped
    // row plus a transient halt whose backoff has already elapsed.
    const { browserLocalStore } = await import('../lib/backfill/store');
    const s = browserLocalStore();
    await recordBackfillHalt(s, {
      platform: 'claude',
      scope: 'default',
      reason: 'transport-error',
      detail: 'synthetic failure during a previous registration',
      clock: { now: () => Date.now() - 3_600_000, sleep: async () => {} },
      random: () => 0,
    });
    const { rememberTarget } = await import('../lib/backfill/alarm');
    await rememberTarget(s, { platform: 'claude', origin: CLAUDE_ORIGIN, scope: 'default', at: 1 });

    // `reason: 'ran'` only says the tick was not blocked; the report is what says
    // what the run did, and an empty listing is the ordinary end of one.
    const result = await mod.runAlarmTick();
    expect(result.report?.stopped).toBe('queue-empty');

    // 🔴 The sentinel row is gone and the organization's own row is in its place:
    //    a target whose scope names no account is not a target.
    const written = await targetsInRegistry();
    expect(written.length).toBe(1);
    expect(written[0]).toMatchObject({ platform: 'claude', origin: CLAUDE_ORIGIN, scope: ORG });

    // And the tick really used it: the list request carries the resolved
    // organization in its path, which is the whole point of resolving it.
    expect(conversationRequests().length).toBeGreaterThan(0);
    expect(conversationRequests().every((url) => url.includes(`/api/organizations/${ORG}/`))).toBe(true);
  });

  it('🔴 a permanent halt is not re-asked, and the tick therefore issues nothing', async () => {
    const { recordBackfillHalt } = await import('../lib/backfill/engine');
    const mod = await bootBackground();
    await enableBackfill();
    await tabHello(7);
    // Present, and it must not be reached: a permanent halt already knows the answer.
    routes[RESOLVE_PATH] = organizationsEndpoint([ORG]);

    const { browserLocalStore } = await import('../lib/backfill/store');
    const s = browserLocalStore();
    await recordBackfillHalt(s, {
      platform: 'claude', scope: 'default', reason: 'org-ambiguous', detail: 'synthetic',
    });
    const { rememberTarget } = await import('../lib/backfill/alarm');
    await rememberTarget(s, { platform: 'claude', origin: CLAUDE_ORIGIN, scope: 'default', at: 1 });

    const result = await mod.runAlarmTick();
    expect(result.report?.stopped).toBe('halted');
    expect(pageCalls).toEqual([]);
    expect(conversationRequests()).toEqual([]);
  });

  // 🔴 R44 · A stored judgement that no longer applies must make the alarm ask
  //    again. `scopeRetryDue` read "this halt no longer applies" and answered
  //    "do not ask the page" — the same answer it gives when the halt does
  //    apply — and since every capability reason is permanent, the next line
  //    answered false too. The layer was a no-op and the target stayed frozen.
  //
  //    The record is built here by hand rather than through `recordBackfillHalt`,
  //    which stamps the capability of the build it runs on and so can only ever
  //    produce a record that matches. The shape below is the one measured on a
  //    real machine on 2026-09-19: a judgement left behind by a build that had
  //    no plan for this platform.
  it('🔴 a capability halt from another build is re-asked; an account halt is not', async () => {
    const { scopeRetryDue, UNRESOLVED_SCOPE } = await import('../entrypoints/background');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const { headerOf, initialState, stateKey } = await import('../lib/backfill/types');

    const s = browserLocalStore();
    const now = Date.now();
    const key = stateKey('claude', UNRESOLVED_SCOPE);
    const withHalt = (halted: Record<string, unknown>) => ({
      ...headerOf(initialState('claude', UNRESOLVED_SCOPE)),
      halted,
    });

    // 1 · Judged by a build that had no plan for claude. This build has one.
    store[key] = withHalt({
      reason: 'unsupported-platform',
      at: now - 3_600_000,
      detail: 'synthetic: written by a build with no plan for this platform',
      capability: 'none',
    });
    expect(await scopeRetryDue(s, 'claude', UNRESOLVED_SCOPE, now)).toBe(true);

    // 2 · No marker at all — every record written before W44, and the shape of
    //     the three found on the real machine.
    store[key] = withHalt({
      reason: 'unsupported-platform',
      at: now - 3_600_000,
      detail: 'synthetic: written before a stop said what it was judged against',
    });
    expect(await scopeRetryDue(s, 'claude', UNRESOLVED_SCOPE, now)).toBe(true);

    // 3 · 🔴 An account-class judgement is untouched: it is a fact about the
    //     account, not about the build, so it stays permanent. Otherwise this
    //     fix would turn "wait for a human" into a slow poll of the account.
    store[key] = withHalt({
      reason: 'org-unresolved',
      at: now - 3_600_000,
      detail: 'synthetic: no organization could be named',
    });
    expect(await scopeRetryDue(s, 'claude', UNRESOLVED_SCOPE, now)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 6 · W49 · A title is not an organization, and it must not keep a second row
// ---------------------------------------------------------------------------
describe('W49 · Claude target scope must be an organization and must be the only non-organization Claude row', () => {
  it('🔴 a stale title row is removed when a real capture arrives', async () => {
    const mod = await bootBackground();
    await enableBackfill();
    // The page has already shown which organization it is using, so the backfill
    // request itself is allowed through and the test noise is only about the registry.
    await tabHello(7, { seen: ORG });
    routes[`/api/organizations/${ORG}/chat_conversations`] = jsonRoute(() => '[]');

    const { rememberTarget } = await import('../lib/backfill/alarm');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const s = browserLocalStore();
    // Pre-W31 (or any earlier path) left a scope that is a conversation title,
    // not an organization id. The live leg's capture is the real row that must win.
    await rememberTarget(s, {
      platform: 'claude',
      origin: CLAUDE_ORIGIN,
      scope: TITLE,
      at: 1,
    });

    const captured = {
      url: `${DETAIL_URL}?tree=True&rendering_mode=messages&render_all_tools=true`,
      method: 'GET',
      status: 200,
      // `name` is the conversation title — the identity heuristic's handle key.
      // A scoped platform must not take it as the account scope.
      text: JSON.stringify({ uuid: ID, name: TITLE, chat_messages: [] }),
      pageUrl: `${CLAUDE_ORIGIN}/chat/${ID}`,
      capturedAt: Date.now(),
    };
    await dispatchFromTab({ type: 'chat-captured', payload: captured }, 7);
    await mod.backfillTickSettled();

    const written = await targetsInRegistry();
    expect(written).toHaveLength(1);
    expect(written[0]).toMatchObject({ platform: 'claude', scope: ORG });
    expect(written.some((t) => t.scope === TITLE)).toBe(false);
  });

  it('🔴 a title-shaped path segment is not written as a Claude target', async () => {
    const { backfillTargetFor } = await import('../entrypoints/background');
    const titled = backfillTargetFor({
      url: `${CLAUDE_ORIGIN}/api/organizations/${TITLE}/chat_conversations/${ID}?tree=True&rendering_mode=messages&render_all_tools=true`,
      method: 'GET',
      status: 200,
      text: JSON.stringify({ uuid: ID, name: TITLE, chat_messages: [] }),
      pageUrl: `${CLAUDE_ORIGIN}/chat/${ID}`,
      capturedAt: Date.now(),
    });
    // A path segment that is a conversation title is not an organization. Returning
    // a target would write that title into cs_backfill_targets_v1.
    expect(titled).toBeNull();
  });

  it('🔴 resolving an organization replaces a stale title row instead of joining it', async () => {
    const { POPUP_START_BACKFILL_MESSAGE } = await import('../lib/popup-view');
    await enableBackfill();
    await bootBackground();
    await tabHello(7);
    routes[RESOLVE_PATH] = organizationsEndpoint([ORG]);

    const { rememberTarget } = await import('../lib/backfill/alarm');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const s = browserLocalStore();
    await rememberTarget(s, {
      platform: 'claude',
      origin: CLAUDE_ORIGIN,
      scope: TITLE,
      at: 1,
    });

    const reply = await dispatch({ type: POPUP_START_BACKFILL_MESSAGE });
    expect(reply?.ok).toBe(true);

    const written = await targetsInRegistry();
    expect(written).toHaveLength(1);
    expect(written[0]).toMatchObject({ platform: 'claude', scope: ORG });
    expect(written.some((t) => t.scope === TITLE)).toBe(false);

    // Exactly one question was asked; no conversation was enumerated to learn it.
    expect(pageCalls).toEqual([RESOLVE_URL]);
    expect(conversationRequests()).toEqual([]);
  });

  it('🔴 the alarm does not send a title as an organization, and replaces that row when the page names one', async () => {
    const mod = await bootBackground();
    await enableBackfill();
    await tabHello(7, { seen: ORG });
    routes[`/api/organizations/${ORG}/chat_conversations`] = jsonRoute(() => '[]');

    const { rememberTarget } = await import('../lib/backfill/alarm');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const s = browserLocalStore();
    await rememberTarget(s, {
      platform: 'claude', origin: CLAUDE_ORIGIN, scope: TITLE, at: 1,
    });

    const result = await mod.runAlarmTick();
    expect(result.report?.stopped).toBe('queue-empty');

    const written = await targetsInRegistry();
    expect(written).toHaveLength(1);
    expect(written[0]).toMatchObject({ platform: 'claude', scope: ORG });
    expect(written.some((t) => t.scope === TITLE)).toBe(false);
    expect(conversationRequests().every((url) => url.includes(`/api/organizations/${ORG}/`))).toBe(true);
    expect(conversationRequests().some((url) => url.includes(TITLE))).toBe(false);
  });

  it('🔴 two organization rows for the same platform still coexist', async () => {
    const mod = await bootBackground();
    await enableBackfill();
    await tabHello(7, { seen: ORG });
    routes[`/api/organizations/${ORG}/chat_conversations`] = jsonRoute(() => '[]');

    const { rememberTarget } = await import('../lib/backfill/alarm');
    const { browserLocalStore } = await import('../lib/backfill/store');
    const s = browserLocalStore();
    await rememberTarget(s, {
      platform: 'claude', origin: CLAUDE_ORIGIN, scope: ORG2, at: 1,
    });

    const captured = {
      url: `${DETAIL_URL}?tree=True&rendering_mode=messages&render_all_tools=true`,
      method: 'GET',
      status: 200,
      text: JSON.stringify({ uuid: ID, name: TITLE, chat_messages: [] }),
      pageUrl: `${CLAUDE_ORIGIN}/chat/${ID}`,
      capturedAt: Date.now(),
    };
    await dispatchFromTab({ type: 'chat-captured', payload: captured }, 7);
    await mod.backfillTickSettled();

    const written = await targetsInRegistry();
    const scopes = written.filter((t) => t.platform === 'claude').map((t) => t.scope).sort();
    expect(scopes).toEqual([ORG2, ORG].sort());
    expect(written.some((t) => t.scope === TITLE)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 5 · The wiring guard — the production glue really does these things
// ---------------------------------------------------------------------------
describe('W31c-5 · the content script really asks, really remembers, and really scopes', () => {
  const source = readFileSync(
    new URL('../entrypoints/dw-bridge.content.ts', import.meta.url), 'utf8',
  );
  const pageModule = readFileSync(
    new URL('../lib/backfill/claude-page.ts', import.meta.url), 'utf8',
  );

  it('🔴 the page builds the scope object, hands it the message, and hands the fetch channel its scope', () => {
    expect(source).toContain('createClaudePageScope({');
    expect(source).toContain('readCookie: () => (typeof document === \'undefined\' ? null : document.cookie)');
    expect(source).toContain('const orgPending = claudePage.handleMessage(message);');
    // 🔴 The fetch channel is handed `allowedScope()` — without it, every claude.ai
    //    request is refused at the page (`scopePathMatches` matches nothing
    //    against null), however well the organization was resolved.
    expect(source).toMatch(
      /handleBackfillMessage\(\s*message, pageOrigin, pageFetch, backfillPlanFor, claudePage\.allowedScope\(\),\s*\)/,
    );
  });

  it('🔴 the page\'s own requests are remembered — the resolver\'s strongest source', () => {
    expect(source).toContain('claudePage.rememberRequest(event.data.payload.url)');
    expect(pageModule).toContain('const org = orgFromRequestUrl(url);');
  });

  it('🔴 the one request goes through the backfill allowlist, and a non-2xx is not an answer', () => {
    expect(pageModule).toContain("backfillPlanFor('claude')?.scopeInPath?.resolvePath");
    expect(pageModule).toMatch(
      /serveBackfillFetch\(`\$\{deps\.pageOrigin\}\$\{resolvePath\}`, deps\.pageOrigin, deps\.fetchImpl\)/,
    );
    expect(pageModule).toContain('if (reply.status < 200 || reply.status > 299) {');
  });
});
