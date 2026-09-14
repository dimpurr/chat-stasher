/**
 * W31 · claude.ai: the organization resolver, the offset-paged list with no
 * termination field, and a body whose completeness is decided by its own parent
 * links.
 *
 * ## What this file exists to stop changing back
 *  1. **The organization is evidence, never a guess.** The value is required in
 *     every request path and is in no page URL, so it is read from the page's own
 *     already-seen requests first, then from the `lastActiveOrg` cookie, then from
 *     one `GET /api/organizations`. Several organizations and neither of the first
 *     two naming one is a named halt — the organizations are never iterated, and no
 *     list request is sent while the answer is unknown.
 *  2. **A scoped plan never sends the `'default'` sentinel.** That word is this
 *     repository's spelling for "the account identifier cannot be told"; in a path
 *     segment it would be a request against an organization that does not exist.
 *  3. **A body that does not hold the whole branch is not a conversation.** The
 *     tree walk from `current_leaf_message_uuid` is what decides it, and a broken
 *     chain is a named receipt, never an archived conversation missing its middle.
 *  4. **The allowlist grew no wildcard.** Two path templates matched segment by
 *     segment with the resolved scope, one pinned query, and a resolution-only path
 *     that can carry nothing at all.
 *
 * 🔴 Everything here is **synthetic and source-shaped**: field names and routes come
 *    from the W20 research's reading of reference implementations, and every value —
 *    organization, conversation id, message text — is invented in this file. No
 *    request goes to claude.ai, there is no logged-in state, and no real
 *    conversation, account or id appears below. The http port is always injected and
 *    **throws on any path it was not given**, so "no request was sent" is proven by
 *    the run rather than asserted about it.
 */

import { describe, expect, it } from 'vitest';
import {
  extractSessionId,
  findPlatformForUrl,
  matchesResponseShape,
  PLATFORMS,
  type CapturedFetch,
} from '../lib/contract';
import { runBackfill, type HttpResponse, type SinkOutcome } from '../lib/backfill/engine';
import { memoryStore } from '../lib/backfill/store';
import {
  CLAUDE_DETAIL_PATH_TEMPLATE,
  CLAUDE_DETAIL_QUERY,
  CLAUDE_LIST_LIMIT,
  CLAUDE_LIST_PATH_TEMPLATE,
  CLAUDE_PARENT_KEYS,
  CLAUDE_PLAN,
  CLAUDE_RESOLVE_PATH,
  applyScope,
  backfillPlanFor,
  claudeParentKeyIn,
  expectedMethodFor,
  parseClaudeDetailPage,
  parseClaudeListPage,
  pinnedQueryMatches,
  scopePathMatches,
} from '../lib/backfill/enumerate';
import {
  CLAUDE_ORG_COOKIE,
  orgFromCookie,
  orgFromRequestUrl,
  parseOrganizationsResponse,
  resolveClaudeOrg,
  resolveClaudeOrgOnPage,
} from '../lib/backfill/claude-org';
import { checkBackfillRequest, isAllowedBackfillUrl } from '../lib/backfill/tab-port';
import { describeFailureReason } from '../lib/backfill/failures';
import type { Clock } from '../lib/backfill/pace';

const ORIGIN = 'https://claude.ai';
/** Two synthetic organization ids. Values only; nothing here was ever a real account. */
const ORG = 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee';
const ORG2 = '11111111-2222-3333-4444-555555555555';
/** A synthetic conversation id, in the hex shape the row's session pattern reads. */
const ID = '99999999-8888-7777-6666-555555555555';
const ID2 = '00000000-1111-2222-3333-444444444444';

const LIST_PATH = `/api/organizations/${ORG}/chat_conversations`;
const DETAIL_PATH = `${LIST_PATH}/${ID}`;
const LIST_URL = `${ORIGIN}${LIST_PATH}`;
const DETAIL_URL = `${ORIGIN}${DETAIL_PATH}`;
/** The body URL exactly as the plan's own builder emits it (the three pinned parameters included). */
const DETAIL_TREE_URL = `${DETAIL_URL}?tree=True&rendering_mode=messages&render_all_tools=true`;
const PAGE_URL = `${ORIGIN}/chat/${ID}`;

const NO_WAIT = {
  enumerate: { minIntervalMs: 0, maxPerDay: null },
  detail: { minIntervalMs: 0, maxPerDay: null },
};

function fakeClock(): Clock {
  let time = Date.parse('2026-09-14T00:00:00.000Z');
  return {
    now: () => time,
    async sleep(ms: number) {
      time += ms;
    },
  };
}

// ---------------------------------------------------------------------------
// Synthetic fixtures
// ---------------------------------------------------------------------------

/** One message of the tree, with whichever parent-link spelling the caller asks for. */
function message(
  uuid: string,
  parent: string | null,
  parentKey: string = 'parent_message_uuid',
): Record<string, unknown> {
  const record: Record<string, unknown> = {
    uuid,
    index: 0,
    sender: 'human',
    created_at: '2026-09-01T10:00:00.000Z',
    content: [{ type: 'text', text: `synthetic ${uuid}` }],
  };
  if (parent !== null) record[parentKey] = parent;
  return record;
}

/**
 * A detail response. `leaf` defaults to the last message, and the messages carry
 * the chain root → leaf. `parentKey` picks the spelling.
 */
function treeBody(
  chain: string[],
  opts: { leaf?: string | null; parentKey?: string; extra?: Record<string, unknown> } = {},
): string {
  const parentKey = opts.parentKey ?? 'parent_message_uuid';
  const messages = chain.map((uuid, i) => message(uuid, i === 0 ? null : chain[i - 1]!, parentKey));
  const leaf = opts.leaf === undefined ? chain[chain.length - 1]! : opts.leaf;
  const body: Record<string, unknown> = { uuid: ID, name: 'synthetic', model: 'synthetic-model', chat_messages: messages };
  if (leaf !== null) body.current_leaf_message_uuid = leaf;
  return JSON.stringify({ ...body, ...(opts.extra ?? {}) });
}

/** One list page: a bare array of summaries, as the sources describe it. */
function listPage(ids: string[]): string {
  return JSON.stringify(ids.map((uuid) => ({ uuid, name: `synthetic-${uuid}` })));
}

const GOOD_TREE = treeBody(['m1', 'm2', 'm3']);

interface Recorder {
  calls: { url: string; init: unknown }[];
}

/**
 * A synthetic backend. Any path it was not given **throws**, so "this leg sent no
 * further request" is proven by the run rather than claimed.
 */
function backend(
  routes: Partial<Record<string, string | ((u: URL) => string)>>,
): Recorder & { http: (url: string, init?: unknown) => Promise<HttpResponse> } {
  const calls: { url: string; init: unknown }[] = [];
  const http = async (url: string, init?: unknown): Promise<HttpResponse> => {
    calls.push({ url, init });
    const u = new URL(url);
    const route = routes[u.pathname];
    if (route === undefined) throw new Error(`unexpected path ${u.pathname}`);
    return { status: 200, text: typeof route === 'string' ? route : route(u) };
  };
  return { calls, http };
}

function run(
  store: ReturnType<typeof memoryStore>,
  http: (url: string, init?: unknown) => Promise<HttpResponse>,
  scope: string,
  extra: Partial<Parameters<typeof runBackfill>[0]> = {},
) {
  return runBackfill({
    platform: 'claude',
    origin: ORIGIN,
    scope,
    store,
    http: http as never,
    clock: fakeClock(),
    pace: NO_WAIT,
    sink: (captured: CapturedFetch): SinkOutcome => ({ saved: true, sessionId: captured.sessionId }),
    ...extra,
  });
}

// ---------------------------------------------------------------------------
// 1 · The organization resolver, as a decision
// ---------------------------------------------------------------------------
describe('W31-1 · resolveClaudeOrg: the order, and the refusals that are not guesses', () => {
  it('the page\'s own already-seen request wins over the cookie and over the endpoint list', () => {
    const resolved = resolveClaudeOrg({
      seen: ORG,
      cookie: `${CLAUDE_ORG_COOKIE}=${ORG2}`,
      endpoint: { kind: 'text', text: JSON.stringify([{ uuid: ORG2 }]) },
    });
    expect(resolved).toEqual({ ok: true, org: ORG, source: 'page' });
  });

  it('the cookie is next — and only when the page showed nothing', () => {
    const fromCookie = resolveClaudeOrg({
      seen: null,
      cookie: `other=1; ${CLAUDE_ORG_COOKIE}=${ORG2}; third=2`,
      endpoint: { kind: 'text', text: JSON.stringify([{ uuid: ORG }]) },
    });
    expect(fromCookie).toEqual({ ok: true, org: ORG2, source: 'cookie' });
    // An empty cookie value is not an organization, so the endpoint still gets its turn.
    const emptyCookie = resolveClaudeOrg({
      seen: null,
      cookie: `${CLAUDE_ORG_COOKIE}=`,
      endpoint: { kind: 'text', text: JSON.stringify([{ uuid: ORG }]) },
    });
    expect(emptyCookie).toEqual({ ok: true, org: ORG, source: 'endpoint' });
  });

  it('exactly one organization from the endpoint is used; several are a named halt', () => {
    const one = resolveClaudeOrg({
      seen: null, cookie: '', endpoint: { kind: 'text', text: JSON.stringify([{ uuid: ORG }]) },
    });
    expect(one).toEqual({ ok: true, org: ORG, source: 'endpoint' });
    const several = resolveClaudeOrg({
      seen: null,
      cookie: '',
      endpoint: { kind: 'text', text: JSON.stringify([{ uuid: ORG }, { uuid: ORG2 }]) },
    });
    expect(several.ok).toBe(false);
    expect(several.ok === false && several.halt).toBe('org-ambiguous');
    // 🔴 Neither id leaks into the refusal: which organizations an account has is a
    //    fact about the account, and the sentence only has to say how many.
    expect(several.ok === false && several.detail).toContain('2 organizations');
    expect(several.ok === false && several.detail).not.toContain(ORG);
  });

  it('a request that did not complete is transport-error — never "no organizations"', () => {
    const failed = resolveClaudeOrg({
      seen: null, cookie: '', endpoint: { kind: 'failed', detail: 'network down' },
    });
    expect(failed.ok).toBe(false);
    expect(failed.ok === false && failed.halt).toBe('transport-error');
  });

  it('an empty list, and a body that is not the list, are both "no organization could be named"', () => {
    for (const text of ['[]', '{"organizations":[]}', '<html>login</html>', '[{"id":"x"}]']) {
      const resolved = resolveClaudeOrg({ seen: null, cookie: '', endpoint: { kind: 'text', text } });
      expect(resolved.ok, text).toBe(false);
      expect(resolved.ok === false && resolved.halt, text).toBe('org-unresolved');
    }
    // The one shape that is read: an array of records carrying a non-empty uuid.
    expect(parseOrganizationsResponse(JSON.stringify([{ uuid: ORG, name: 'synthetic' }])))
      .toEqual({ ok: true, orgs: [ORG] });
  });

  it('reads the organization out of the page\'s own request URL, and out of nothing else', () => {
    expect(orgFromRequestUrl(DETAIL_URL)).toBe(ORG);
    expect(orgFromRequestUrl(`${ORIGIN}${LIST_PATH}?limit=50&offset=0`)).toBe(ORG);
    expect(orgFromRequestUrl(`${ORIGIN}/api/organizations`)).toBeNull();
    expect(orgFromRequestUrl(`${ORIGIN}/api/auth/session`)).toBeNull();
    expect(orgFromRequestUrl(`${ORIGIN}/chat/${ID}`)).toBeNull();
    expect(orgFromCookie(`${CLAUDE_ORG_COOKIE}=${ORG}`)).toBe(ORG);
  });
});

// ---------------------------------------------------------------------------
// 2 · The one request, made only when the free sources answered nothing
// ---------------------------------------------------------------------------
describe('W31-2 · resolveClaudeOrgOnPage: when the resolver is allowed to spend a request', () => {
  it('spends nothing when the page or the cookie already answered', async () => {
    let called = 0;
    const fetchOrganizations = async (): Promise<string> => {
      called += 1;
      return JSON.stringify([{ uuid: ORG }]);
    };
    const viaPage = await resolveClaudeOrgOnPage({ seen: ORG, cookie: '' }, fetchOrganizations);
    const viaCookie = await resolveClaudeOrgOnPage({ seen: null, cookie: `${CLAUDE_ORG_COOKIE}=${ORG}` }, fetchOrganizations);
    expect(viaPage).toEqual({ ok: true, org: ORG, source: 'page' });
    expect(viaCookie).toEqual({ ok: true, org: ORG, source: 'cookie' });
    expect(called).toBe(0);
  });

  it('makes exactly one request when it must, and a failed one is not "no organizations"', async () => {
    let called = 0;
    const one = await resolveClaudeOrgOnPage({ seen: null, cookie: '' }, async () => {
      called += 1;
      return JSON.stringify([{ uuid: ORG }]);
    });
    expect(one).toEqual({ ok: true, org: ORG, source: 'endpoint' });
    expect(called).toBe(1);

    const failed = await resolveClaudeOrgOnPage({ seen: null, cookie: '' }, async () => {
      throw new Error('synthetic transport failure');
    });
    expect(failed.ok).toBe(false);
    expect(failed.ok === false && failed.halt).toBe('transport-error');
  });
});

// ---------------------------------------------------------------------------
// 3 · The plan's declarations
// ---------------------------------------------------------------------------
describe('W31-3 · the claude plan in the plan table', () => {
  const plan = backfillPlanFor('claude')!;

  it('is registered, with both segments and the scope declared as a path position', () => {
    expect(plan).toBe(CLAUDE_PLAN);
    expect(plan.listPath).toBe(CLAUDE_LIST_PATH_TEMPLATE);
    expect(plan.detailPath).toBe(CLAUDE_DETAIL_PATH_TEMPLATE);
    expect(plan.scopeInPath).toEqual({
      listPath: CLAUDE_LIST_PATH_TEMPLATE,
      detailPath: CLAUDE_DETAIL_PATH_TEMPLATE,
      resolvePath: CLAUDE_RESOLVE_PATH,
      tokenIsScope: true,
    });
    // The page size is the plan's own, so the ending is measured against the number
    // the plan actually asked for rather than a cross-platform default.
    expect(plan.listPageSize).toBe(CLAUDE_LIST_LIMIT);
    expect(plan.listOffsetInferred).toBe(true);
    expect(expectedMethodFor(plan, 'list')).toBe('GET');
    expect(expectedMethodFor(plan, 'detail')).toBe('GET');
    expect(expectedMethodFor(plan, 'resolve')).toBe('GET');
  });

  it('pins the three tree parameters and pins no value of its own', () => {
    expect(CLAUDE_DETAIL_QUERY.map((q) => q.key)).toEqual(['tree', 'rendering_mode', 'render_all_tools']);
    expect(CLAUDE_DETAIL_QUERY.every((q) => q.value.length > 0)).toBe(true);
    const built = new URL(plan.detailUrl!(ORIGIN, ID).replace('{org}', ORG));
    expect(pinnedQueryMatches(CLAUDE_DETAIL_QUERY, built)).toBe(true);
    // The pinned-set rule is a *set* rule: a fourth parameter is refused.
    const extra = new URL(`${built.toString()}&x=1`);
    expect(pinnedQueryMatches(CLAUDE_DETAIL_QUERY, extra)).toBe(false);
    expect(new URL(`${built.toString()}&tree=false`)).toBeDefined();
    expect(pinnedQueryMatches(CLAUDE_DETAIL_QUERY, new URL(`${ORIGIN}${DETAIL_PATH}?tree=false&rendering_mode=messages&render_all_tools=true`))).toBe(false);
  });

  it('substitutes the scope into every URL it builds, and refuses to build one without it', () => {
    const raw = plan.listUrl(ORIGIN, 0, CLAUDE_LIST_LIMIT);
    expect(raw).toContain('{org}');
    const scoped = applyScope(plan, raw, ORG);
    expect(scoped).toBe(`${LIST_URL}?limit=50&offset=0`);
    // No scope ⇒ no URL, rather than a request against a literal `{org}` path.
    expect(applyScope(plan, raw, null)).toBeNull();
    expect(applyScope(plan, raw, '')).toBeNull();
    // A plan that declares no scope keeps its URL byte-identical — every other
    // platform's URLs are untouched by this change.
    expect(applyScope(backfillPlanFor('chatgpt')!, 'https://chatgpt.com/x', null)).toBe('https://chatgpt.com/x');
  });

  it('matches its path templates segment by segment, with the scope compared rather than accepted', () => {
    expect(scopePathMatches(CLAUDE_LIST_PATH_TEMPLATE, LIST_PATH, ORG)).toBe(true);
    expect(scopePathMatches(CLAUDE_DETAIL_PATH_TEMPLATE, DETAIL_PATH, ORG)).toBe(true);
    // 🔴 A different organization is not this plan's request.
    expect(scopePathMatches(CLAUDE_LIST_PATH_TEMPLATE, LIST_PATH, ORG2)).toBe(false);
    expect(scopePathMatches(CLAUDE_DETAIL_PATH_TEMPLATE, DETAIL_PATH, ORG2)).toBe(false);
    // 🔴 An unresolved scope matches nothing at all.
    expect(scopePathMatches(CLAUDE_LIST_PATH_TEMPLATE, LIST_PATH, null)).toBe(false);
    // No wildcards: the segment count is fixed and the literals are compared.
    expect(scopePathMatches(CLAUDE_LIST_PATH_TEMPLATE, `${LIST_PATH}/extra`, ORG)).toBe(false);
    expect(scopePathMatches(CLAUDE_DETAIL_PATH_TEMPLATE, `${LIST_PATH}/`, ORG)).toBe(false);
    expect(scopePathMatches(CLAUDE_LIST_PATH_TEMPLATE, `/api/organizations/${ORG}`, ORG)).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 4 · The list: offset paging with no termination field
// ---------------------------------------------------------------------------
describe('W31-4 · the list segment', () => {
  it('reads a bare array of summaries carrying uuid, and refuses anything else', () => {
    const ok = parseClaudeListPage(listPage([ID, ID2]));
    expect(ok.ok).toBe(true);
    expect(ok.ok === true && ok.page.ids).toEqual([ID, ID2]);
    // 🔴 The total is not read even as a hint: the response has no such field.
    expect(ok.ok === true && ok.page.total).toBeNull();
    // An empty array is a measurement — zero rows read, not a missing field.
    const empty = parseClaudeListPage('[]');
    expect(empty.ok === true && empty.page.ids).toEqual([]);
    // Everything else is a named refusal, never "no conversations".
    for (const text of ['{"data":[]}', 'not json', '[{"id":"x"}]', '[null]', '["x"]']) {
      const parsed = parseClaudeListPage(text);
      expect(parsed.ok, text).toBe(false);
    }
  });

  it('an empty page ends the listing, and the leg really listed what it read', async () => {
    const store = memoryStore();
    const be = backend({ [LIST_PATH]: listPage([ID]) });
    // The body is fetched too, so the route has to exist — one conversation, one page.
    const withDetail = backend({ [LIST_PATH]: (u: URL) => (u.searchParams.get('offset') === '0' ? listPage([ID]) : '[]'), [DETAIL_PATH]: GOOD_TREE });
    const report = await run(store, withDetail.http, ORG, { maxDetails: 0 });
    expect(report.state.archived).toEqual([]);
    // Page 1 (one row) is short, so this plan's rule ends the listing there — and it
    // is recorded as an **inference**, not as an API termination signal.
    expect(report.state.enumCursor.truncated).toBe('short-page-inferred');
    expect(report.state.enumCursor.complete).toBe(false);
    expect(report.state.pending).toEqual([ID]);
    expect(withDetail.calls.length).toBe(1);
    expect(be.calls.length).toBe(0);
  });

  it('an empty page is the real observation that ends this plan\'s listing', async () => {
    const store = memoryStore();
    const be = backend({ [LIST_PATH]: '[]' });
    const report = await run(store, be.http, ORG, { maxDetails: 0 });
    expect(report.state.enumCursor.complete).toBe(true);
    expect(report.state.enumCursor.truncated).toBeUndefined();
    expect(report.state.pending).toEqual([]);
    expect(be.calls.length).toBe(1);
  });

  it('asks with limit and offset, in the plan\'s own page size, and the scope in the path', async () => {
    const store = memoryStore();
    const seen: string[] = [];
    const be = backend({ [LIST_PATH]: (u: URL) => { seen.push(u.search); return '[]'; } });
    await run(store, be.http, ORG, { maxDetails: 0 });
    expect(be.calls[0]!.url).toBe(`${LIST_URL}?limit=${CLAUDE_LIST_LIMIT}&offset=0`);
    expect(seen).toEqual([`?limit=${CLAUDE_LIST_LIMIT}&offset=0`]);
    // The unsubstituted template never reaches the wire.
    expect(be.calls[0]!.url).not.toContain('{org}');
  });

  it('the repeat-page guard: a page the offset did not move past is a halt, not a second pass', async () => {
    const store = memoryStore();
    const full = listPage(Array.from({ length: CLAUDE_LIST_LIMIT }, (_, i) => `id-${i}`));
    // Every offset gets the same page: the parameter is being ignored.
    const be = backend({ [LIST_PATH]: full });
    // A plan with a body segment reads **one list page per tick** (W10), so the
    // repeated page is what the *second* run sees — which is exactly the situation
    // the guard is for: without it, every tick would re-read the same page while the
    // ledger said the listing was advancing.
    const first = await run(store, be.http, ORG, { maxDetails: 0 });
    expect(first.stopped).toBe('budget-exhausted');
    expect(first.state.enumCursor.offset).toBe(CLAUDE_LIST_LIMIT);
    const report = await run(store, be.http, ORG, { maxDetails: 0 });
    expect(report.stopped).toBe('halted');
    expect(report.halted?.reason).toBe('shape-changed');
    expect(report.halted?.detail).toContain('offset did not advance');
    // Two requests, not a loop: page 1 (offset 0) and the repeated page (offset 50).
    expect(be.calls.length).toBe(2);
    expect(be.calls[0]!.url).toContain('offset=0');
    expect(be.calls[1]!.url).toContain('offset=50');
    // 🔴 Nothing was recorded as an ending: this is not "we listed everything".
    expect(report.state.enumCursor.complete).toBe(false);
    expect(report.state.enumCursor.truncated).toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// 5 · The body: the tree walk is the completeness check
// ---------------------------------------------------------------------------
describe('W31-5 · the body segment', () => {
  it('a whole branch is delivered once, archived, and named by the debt key', async () => {
    const store = memoryStore();
    const be = backend({ [LIST_PATH]: listPage([ID]), [DETAIL_PATH]: GOOD_TREE });
    const saved: CapturedFetch[] = [];
    const report = await run(store, be.http, ORG, {
      sink: (captured: CapturedFetch): SinkOutcome => {
        saved.push(captured);
        return { saved: true, sessionId: captured.sessionId };
      },
    });
    expect(report.state.archived).toEqual([ID]);
    expect(report.state.pending).toEqual([]);
    expect(report.failedThisRun).toEqual([]);
    expect(saved.length).toBe(1);
    // 🔴 The identity is expressed once: the debt key the list handed us is the name
    //    the sink is given, and it is the value the live capture would file the same
    //    conversation under (both are the uuid in this path).
    expect(saved[0]!.sessionId).toBe(ID);
    expect(extractSessionId(DETAIL_URL, GOOD_TREE, PAGE_URL)).toBe(ID);
    // And the body request really carried the plan's own three parameters.
    const detailCall = be.calls.find((c) => c.url.includes(DETAIL_PATH))!;
    expect(detailCall.url).toBe(
      `${ORIGIN}${DETAIL_PATH}?tree=True&rendering_mode=messages&render_all_tools=true`,
    );
  });

  it('a broken parent chain is a named per-conversation failure, and nothing is archived', async () => {
    const store = memoryStore();
    // m2's parent is not in the response: the branch that starts at the leaf leaves
    // the messages this response carries.
    const broken = JSON.stringify({
      uuid: ID,
      chat_messages: [message('m2', 'missing-parent'), message('m3', 'm2')],
      current_leaf_message_uuid: 'm3',
    });
    const be = backend({ [LIST_PATH]: listPage([ID]), [DETAIL_PATH]: broken });
    const parsed = parseClaudeDetailPage(broken);
    expect(parsed.ok).toBe(true);
    expect(parsed.ok === true && parsed.outcome).toBe('detail-tree-incomplete');
    const report = await run(store, be.http, ORG);
    expect(report.state.archived).toEqual([]);
    expect(report.state.pending).toEqual([]);
    expect(report.failedThisRun.map((f) => f.reason)).toEqual(['detail-tree-incomplete']);
    expect(report.failedThisRun[0]!.shortId).toBe(ID.slice(0, 8));
    expect(describeFailureReason('detail-tree-incomplete')).toContain('nothing was stored');
    // 🔴 The leg is not halted: a per-conversation fact must not stop the run.
    expect(report.stopped).not.toBe('halted');
  });

  it('both parent-link spellings are accepted, and which one a response used is readable', () => {
    for (const parentKey of CLAUDE_PARENT_KEYS) {
      const body = treeBody(['m1', 'm2', 'm3'], { parentKey });
      const parsed = parseClaudeDetailPage(body);
      expect(parsed, parentKey).toEqual({ ok: true, outcome: 'non-empty' });
      expect(claudeParentKeyIn(body), parentKey).toBe(parentKey);
    }
    // A response whose messages are all roots uses neither spelling, which is a
    // readable fact and not a missing field.
    expect(claudeParentKeyIn(treeBody(['m1']))).toBeNull();
  });

  it('an envelope without chat_messages is the shape gate, and an empty one is not "empty conversation"', () => {
    const noMessages = parseClaudeDetailPage(JSON.stringify({ uuid: ID, current_leaf_message_uuid: 'm1' }));
    expect(noMessages.ok).toBe(false);
    const empty = parseClaudeDetailPage(JSON.stringify({ uuid: ID, chat_messages: [] }));
    expect(empty).toEqual({ ok: true, outcome: 'detail-empty-unverified' });
  });

  it('a missing leaf, a leaf outside the response, and a cycle are all incomplete', () => {
    for (const body of [
      treeBody(['m1', 'm2'], { leaf: null }),
      treeBody(['m1', 'm2'], { leaf: 'not-here' }),
      JSON.stringify({
        chat_messages: [message('a', 'b'), message('b', 'a')],
        current_leaf_message_uuid: 'a',
      }),
    ]) {
      const parsed = parseClaudeDetailPage(body);
      expect(parsed.ok === true && parsed.outcome).toBe('detail-tree-incomplete');
    }
  });
});

// ---------------------------------------------------------------------------
// 6 · The allowlist
// ---------------------------------------------------------------------------
describe('W31-6 · the allowlist: two templates, one pinned query, one resolution-only path', () => {
  it('admits this plan\'s own three URLs, with the scope the page side resolved', () => {
    expect(isAllowedBackfillUrl(LIST_URL, ORIGIN, ORG)).toBe(true);
    expect(checkBackfillRequest({ url: DETAIL_TREE_URL }, ORIGIN, backfillPlanFor, ORG).ok).toBe(true);
    expect(isAllowedBackfillUrl(`${ORIGIN}${CLAUDE_RESOLVE_PATH}`, ORIGIN)).toBe(true);
  });

  it('refuses a different organization, a child path of the templates, and the templates unsubstituted', () => {
    // 🔴 Another organization is another account's history: refused, not forwarded.
    expect(isAllowedBackfillUrl(LIST_URL.replace(ORG, ORG2), ORIGIN, ORG)).toBe(false);
    expect(isAllowedBackfillUrl(`${ORIGIN}${DETAIL_PATH.replace(ORG, ORG2)}`, ORIGIN, ORG)).toBe(false);
    // 🔴 No scope resolved ⇒ no URL of this plan goes out.
    expect(isAllowedBackfillUrl(LIST_URL, ORIGIN, null)).toBe(false);
    expect(isAllowedBackfillUrl(`${ORIGIN}${DETAIL_PATH}`, ORIGIN, null)).toBe(false);
    // The literal template is not a URL the plan itself builds.
    expect(isAllowedBackfillUrl(`${ORIGIN}/api/organizations/{org}/chat_conversations`, ORIGIN, ORG)).toBe(false);
    // No prefix wildcarding: one segment more or one fewer is out.
    expect(isAllowedBackfillUrl(`${LIST_URL}/extra`, ORIGIN, ORG)).toBe(false);
    expect(isAllowedBackfillUrl(`${ORIGIN}/api/organizations/${ORG}`, ORIGIN, ORG)).toBe(false);
  });

  it('refuses the resolution-only path as anything but a bare GET', () => {
    const resolveUrl = `${ORIGIN}${CLAUDE_RESOLVE_PATH}`;
    // It cannot carry a query, a fragment, a method or a body.
    expect(checkBackfillRequest({ url: `${resolveUrl}?limit=1` }, ORIGIN).ok).toBe(false);
    expect(checkBackfillRequest({ url: `${resolveUrl}#x` }, ORIGIN).ok).toBe(false);
    expect(checkBackfillRequest({ url: resolveUrl, method: 'POST', body: '{}', contentType: 'application/json' }, ORIGIN).ok).toBe(false);
    expect(checkBackfillRequest({ url: resolveUrl, body: '{}' }, ORIGIN).ok).toBe(false);
    // 🔴 And it is never read as the list or the body path.
    expect(isAllowedBackfillUrl(`${ORIGIN}/api/organizations/${ORG}`, ORIGIN, ORG)).toBe(false);
    // Cross-origin, the universal refusal, is unchanged.
    expect(isAllowedBackfillUrl(LIST_URL, 'https://www.kimi.com', ORG)).toBe(false);
  });

  it('refuses a query the plan did not pin, on the body path', () => {
    const base = DETAIL_TREE_URL;
    expect(checkBackfillRequest({ url: base }, ORIGIN, backfillPlanFor, ORG).ok).toBe(true);
    expect(checkBackfillRequest({ url: `${base}?tree=True` }, ORIGIN, backfillPlanFor, ORG).ok).toBe(false);
    expect(checkBackfillRequest({ url: `${base}?tree=True&rendering_mode=messages` }, ORIGIN, backfillPlanFor, ORG).ok).toBe(false);
    expect(checkBackfillRequest({ url: `${base}?tree=true&rendering_mode=messages&render_all_tools=true` }, ORIGIN, backfillPlanFor, ORG).ok).toBe(false);
    expect(checkBackfillRequest(
      { url: `${base}?tree=True&rendering_mode=messages&render_all_tools=true&extra=1` },
      ORIGIN, backfillPlanFor, ORG,
    ).ok).toBe(false);
  });
});

// ---------------------------------------------------------------------------
// 7 · The scope is not a sentinel, and the row's session id is the debt id
// ---------------------------------------------------------------------------
describe('W31-7 · the scope reaches the engine, or nothing does', () => {
  it('a scoped plan with no usable scope halts before any request', async () => {
    for (const scope of ['', 'default']) {
      const store = memoryStore();
      const be = backend({ [LIST_PATH]: '[]' });
      const report = await run(store, be.http, scope, { maxDetails: 0 });
      expect(report.stopped, scope).toBe('halted');
      expect(report.halted?.reason, scope).toBe('org-unresolved');
      // 🔴 Not one request — and the run really would have thrown on an unknown path.
      expect(be.calls, scope).toEqual([]);
      expect(report.state.enumCursor.offset, scope).toBe(0);
    }
  });

  it('the platform row files a capture under the same value the debt key uses', () => {
    const row = PLATFORMS.find((platform) => platform.id === 'claude')!;
    // The live-capture row reads the conversation uuid out of this very path…
    expect(extractSessionId(DETAIL_URL, GOOD_TREE, PAGE_URL)).toBe(ID);
    // …and the response's own uuid is the same value, so a capture and a debt agree.
    expect(JSON.parse(GOOD_TREE).uuid).toBe(ID);
    // The plan's detail path is the route that row watches, so the two legs cannot
    // name one conversation differently.
    expect(row.pathHints).toEqual(['/chat_conversations/']);
    expect(DETAIL_PATH).toContain(row.pathHints[0]!);
    expect(findPlatformForUrl(DETAIL_URL)?.id).toBe('claude');
    // The shape gate the live row applies is satisfied by the same body.
    expect(matchesResponseShape(row, GOOD_TREE)).toBe(true);
    // And the body gate is the row's own required path, which a drifted envelope fails.
    expect(matchesResponseShape(row, JSON.stringify({ uuid: ID }))).toBe(false);
  });
});
