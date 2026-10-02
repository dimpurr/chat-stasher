import { describe, expect, it } from 'vitest';
import {
  chatGptArchivedUrl,
  chatGptProjectConversationsUrl,
  chatGptProjectsUrl,
  parseChatGptProjectConversationPage,
  parseChatGptProjectPage,
} from '../lib/backfill/enumerate';
import { initialState, headerOf, isHeader, stateFrom } from '../lib/backfill/types';
import { memoryStore } from '../lib/backfill/store';
import { runBackfill as runBackfillRaw } from '../lib/backfill/engine';
import { withChatGptLeaseIdentity } from './chatgpt-lease-fixtures';
const runBackfill = (options: Parameters<typeof runBackfillRaw>[0]) =>
  runBackfillRaw(options.platform === 'chatgpt' && options.http
    ? { ...options, http: withChatGptLeaseIdentity(options.http) }
    : options);
import { CHATGPT_PLAN } from '../lib/backfill/enumerate';
import {
  chatGptAccountIdFromRequest,
  observeChatGptWorkspaceFingerprint,
  resolveChatGptWorkspace,
  fingerprintedChatGptWorkspace,
} from '../lib/backfill/chatgpt-workspace';
import { CHATGPT_TEST_ACCOUNT_IDENTITY } from './chatgpt-lease-fixtures';

describe('W233 ChatGPT enumeration', () => {
  it('parses project discovery and per-project pages without conflating their cursors', () => {
    expect(parseChatGptProjectPage(JSON.stringify({
      items: [{ gizmo: { gizmo: { id: 'opaque-project', name: 'Synthetic project' } } }],
      cursor: 25,
    }))).toEqual({
      ok: true,
      page: { projects: [{ id: 'opaque-project', name: 'Synthetic project' }], nextCursor: 25 },
    });
    expect(parseChatGptProjectPage('{"items":[]}')).toMatchObject({ ok: false });
    expect(parseChatGptProjectConversationPage(JSON.stringify({
      items: [{ id: 'opaque-conversation' }], cursor: 'opaque-next-page',
    }))).toEqual({ ok: true, page: { ids: ['opaque-conversation'], nextCursor: 'opaque-next-page' } });
    expect(parseChatGptProjectConversationPage('{"items":[{}],"cursor":null}')).toMatchObject({ ok: false });
  });

  it('builds separately declared source URLs', () => {
    expect(new URL(chatGptArchivedUrl('https://chatgpt.com', 50, 50)).searchParams.get('is_archived')).toBe('true');
    expect(new URL(chatGptProjectsUrl('https://chatgpt.com', 25)).searchParams.get('cursor')).toBe('25');
    expect(new URL(chatGptProjectConversationsUrl('https://chatgpt.com', 'opaque-project', 'opaque-cursor')).searchParams.get('cursor'))
      .toBe('opaque-cursor');
    expect(chatGptProjectConversationsUrl('https://chatgpt.com', 'opaque/project', null)).toContain('opaque%2Fproject');
  });

  it('persists independent main, archive, discovery, and project cursors in the header', () => {
    const state = initialState('chatgpt', 'chatgpt:opaque-workspace');
    state.enumCursor = { offset: 100, complete: true };
    state.chatgptEnumeration = {
      archived: { offset: 50, complete: false, listed: 50 },
      projects: {
        discoveryCursor: 25,
        discoveryComplete: false,
        entries: [{ id: 'opaque-project', name: 'Synthetic project', cursor: 'opaque-cursor', complete: false, listed: 50 }],
      },
      counts: {
        main: { listed: 100, complete: true },
        archived: { listed: 50, complete: false },
        project: { listed: 50, complete: false },
      },
    };
    const header = headerOf(state);
    expect(isHeader(header)).toBe(true);
    const resumed = stateFrom(header, [], []);
    expect(resumed.enumCursor).toEqual({ offset: 100, complete: true });
    expect(resumed.chatgptEnumeration).toEqual(state.chatgptEnumeration);
  });

  it('binds only observed page request headers and refuses ambiguous workspaces', () => {
    expect(chatGptAccountIdFromRequest(
      { headers: { 'ChatGPT-Account-Id': 'opaque-workspace-a' } }, undefined,
    )).toBe('opaque-workspace-a');
    expect(chatGptAccountIdFromRequest(
      { headers: { 'ChatGPT-Account-Id': 'opaque-workspace-a' } },
      { headers: { 'chatgpt-account-id': 'opaque-workspace-b' } },
    )).toBe('opaque-workspace-b');
    const identityA = { ...CHATGPT_TEST_ACCOUNT_IDENTITY, value: 'a'.repeat(64) };
    const identityB = { ...CHATGPT_TEST_ACCOUNT_IDENTITY, value: 'b'.repeat(64) };
    const one = observeChatGptWorkspaceFingerprint({ identities: [] }, identityA);
    expect(resolveChatGptWorkspace(one)).toMatchObject({ ok: true, observed: true });
    const two = observeChatGptWorkspaceFingerprint(one, identityB);
    expect(resolveChatGptWorkspace(two)).toEqual({ ok: false, reason: 'workspace-ambiguous', observed: true });
    expect(resolveChatGptWorkspace({ identities: [] })).toEqual({ ok: false, reason: 'workspace-unresolved', observed: false });
  });

  it('walks main, archived, discovery, and per-project pages on separate resumable ticks', async () => {
    const calls: string[] = [];
    const http = async (url: string) => {
      calls.push(url);
      const parsed = new URL(url);
      if (parsed.pathname === '/backend-api/conversations') {
        if (parsed.searchParams.get('is_archived') === 'true') {
          const offset = Number(parsed.searchParams.get('offset') ?? 0);
          return { status: 200, text: JSON.stringify({ items: offset === 0 ? [{ id: 'opaque-archived' }] : [] }) };
        }
        const offset = Number(parsed.searchParams.get('offset') ?? 0);
        return { status: 200, text: JSON.stringify({ items: offset === 0 ? [{ id: 'opaque-main' }] : [] }) };
      }
      if (parsed.pathname === '/backend-api/gizmos/snorlax/sidebar') {
        return { status: 200, text: parsed.searchParams.has('cursor')
          ? JSON.stringify({ items: [], cursor: null })
          : JSON.stringify({ items: [{ gizmo: { gizmo: { id: 'opaque-project', name: 'Synthetic project' } } }], cursor: 25 }) };
      }
      if (parsed.pathname === '/backend-api/gizmos/opaque-project/conversations') {
        return { status: 200, text: parsed.searchParams.get('cursor') === 'opaque-next'
          ? JSON.stringify({ items: [{ id: 'opaque-project-2' }], cursor: null })
          : JSON.stringify({ items: [{ id: 'opaque-project-1' }], cursor: 'opaque-next' }) };
      }
      if (parsed.pathname.startsWith('/backend-api/conversation/')) {
        const id = parsed.pathname.split('/').pop();
        return { status: 200, text: JSON.stringify({ conversation_id: id, current_node: 'opaque-node', mapping: {} }) };
      }
      throw new Error('unexpected synthetic route');
    };
    const store = memoryStore();
    const run = async () => runBackfill({
      platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 'chatgpt:opaque-workspace',
      store, http, maxDetails: 1, sink: () => ({ saved: true }),
      clock: { now: () => Date.parse('2026-09-28T00:00:00Z'), sleep: async () => {} },
    });

    for (let tick = 0; tick < 8; tick += 1) await run();
    const state = (await run()).state;
    expect(calls.filter((url) => new URL(url).pathname === '/backend-api/conversations')
      .map((url) => new URL(url).searchParams.get('is_archived') === 'true'
        ? `archived:${new URL(url).searchParams.get('offset')}`
        : `main:${new URL(url).searchParams.get('offset')}`))
      .toEqual(['main:0', 'main:1', 'archived:0', 'archived:1']);
    expect(calls.filter((url) => new URL(url).pathname === '/backend-api/gizmos/snorlax/sidebar')).toHaveLength(2);
    expect(calls.filter((url) => new URL(url).pathname === '/backend-api/gizmos/opaque-project/conversations')).toHaveLength(2);
    expect(state.enumCursor.complete).toBe(true);
    expect(state.chatgptEnumeration).toMatchObject({
      archived: { offset: 1, complete: true, listed: 1 },
      projects: {
        discoveryCursor: null,
        discoveryComplete: true,
        entries: [{ id: 'opaque-project', cursor: null, complete: true, listed: 2 }],
      },
      counts: {
        main: { listed: 1, complete: true },
        archived: { listed: 1, complete: true },
        project: { listed: 2, complete: true },
      },
    });
    expect(state.chatgptDebtProvenance).toMatchObject({
      'opaque-main': { source: 'main', project: 'unknown', archived: false },
      'opaque-archived': { source: 'archived', project: 'unknown', archived: true },
      'opaque-project-1': {
        source: 'project', project: { id: 'opaque-project', name: 'Synthetic project' }, archived: false,
      },
    });
  });

  it('completes archived and project backfill under the canonical marked workspace scope', async () => {
    const scope = `chatgpt:fp1:${CHATGPT_TEST_ACCOUNT_IDENTITY.value}`;
    const calls: string[] = [];
    const http = Object.assign(async (url: string) => {
      calls.push(url);
      const parsed = new URL(url);
      if (parsed.pathname === '/backend-api/conversations') {
        if (parsed.searchParams.get('is_archived') === 'true') {
          return { status: 200, text: JSON.stringify({ items: parsed.searchParams.get('offset') === '0' ? [{ id: 'marked-archived' }] : [] }) };
        }
        return { status: 200, text: JSON.stringify({ items: [] }) };
      }
      if (parsed.pathname === '/backend-api/gizmos/snorlax/sidebar') {
        return { status: 200, text: JSON.stringify({
          items: [{ gizmo: { gizmo: { id: 'marked-project', name: 'Marked project' } } }], cursor: null,
        }) };
      }
      if (parsed.pathname === '/backend-api/gizmos/marked-project/conversations') {
        return { status: 200, text: JSON.stringify({ items: [{ id: 'marked-project-conversation' }], cursor: null }) };
      }
      if (parsed.pathname.startsWith('/backend-api/conversation/')) {
        return { status: 200, text: JSON.stringify({ conversation_id: parsed.pathname.split('/').pop(), current_node: 'node', mapping: {} }) };
      }
      throw new Error('unexpected synthetic route');
    }, {
      chatgptWorkspace: async () => ({ ok: true as const, workspace: fingerprintedChatGptWorkspace(CHATGPT_TEST_ACCOUNT_IDENTITY.value), identity: CHATGPT_TEST_ACCOUNT_IDENTITY, observed: true as const }),
    });
    const store = memoryStore();
    let report;
    for (let tick = 0; tick < 12; tick += 1) {
      report = await runBackfill({
        platform: 'chatgpt', origin: 'https://chatgpt.com', scope, store, http,
        maxDetails: 2, sink: () => ({ saved: true }),
        clock: { now: () => 1, sleep: async () => {} },
      });
    }

    expect(report?.halted).toBeNull();
    expect(calls.some((url) => new URL(url).searchParams.get('is_archived') === 'true')).toBe(true);
    expect(calls.some((url) => new URL(url).pathname === '/backend-api/gizmos/marked-project/conversations')).toBe(true);
    expect(report?.state.chatgptEnumeration?.archived.complete).toBe(true);
    expect(report?.state.chatgptEnumeration?.projects.discoveryComplete).toBe(true);
    expect(report?.state.chatgptEnumeration?.projects.entries).toMatchObject([{ id: 'marked-project', complete: true }]);
    expect(report?.state.chatgptDebtProvenance).toMatchObject({
      'marked-archived': { source: 'archived', archived: true },
      'marked-project-conversation': { source: 'project', project: { id: 'marked-project' } },
    });
  });

  it('records a named workspace refusal without issuing a request or marking sources complete', async () => {
    const calls: string[] = [];
    const report = await runBackfill({
      platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 'chatgpt:!workspace-unresolved',
      store: memoryStore(), http: async (url) => { calls.push(url); return { status: 200, text: '{}' }; },
      clock: { now: () => 1, sleep: async () => {} },
    });
    expect(calls).toEqual([]);
    expect(report.halted?.reason).toBe('org-unresolved');
    expect(report.state.enumCursor.complete).toBe(false);
    expect(report.state.chatgptEnumeration?.counts).toEqual({
      main: { listed: 0, complete: false },
      archived: { listed: 0, complete: false },
      project: { listed: 0, complete: false },
    });
  });

  it('refuses auxiliary IDs when the observed workspace changes during the request', async () => {
    let identity = CHATGPT_TEST_ACCOUNT_IDENTITY;
    const calls: string[] = [];
    const http = Object.assign(async (url: string) => {
      calls.push(url);
      if (new URL(url).searchParams.get('is_archived') === 'true') {
        identity = { ...CHATGPT_TEST_ACCOUNT_IDENTITY, value: 'e'.repeat(64) };
        return { status: 200, text: JSON.stringify({ items: [{ id: 'foreign-archived' }] }) };
      }
      return { status: 200, text: JSON.stringify({ items: [] }) };
    }, {
      chatgptWorkspace: async () => ({ ok: true as const, workspace: fingerprintedChatGptWorkspace(identity.value), identity, observed: true as const }),
    });
    const options = {
      platform: 'chatgpt', origin: 'https://chatgpt.com', scope: `chatgpt:fp1:${CHATGPT_TEST_ACCOUNT_IDENTITY.value}`,
      store: memoryStore(), http,
      clock: { now: () => 1, sleep: async () => {} },
    } as const;
    await runBackfill(options);
    const report = await runBackfill(options);

    expect(calls.map((url) => new URL(url).searchParams.get('is_archived') === 'true' ? 'archived' : 'main'))
      .toEqual(['main', 'archived']);
    expect(report.halted?.reason).toBe('scope-mismatch');
    expect(report.state.pending).not.toContain('foreign-archived');
    expect(report.state.chatgptDebtProvenance?.['foreign-archived']).toBeUndefined();
    expect(report.state.chatgptEnumeration?.archived.complete).toBe(false);
  });

  it('continues auxiliary enumeration while a main-list debt is parked empty', async () => {
    const calls: string[] = [];
    const store = memoryStore();
    const http = async (url: string) => {
      calls.push(url);
      const parsed = new URL(url);
      if (parsed.pathname === '/backend-api/conversations') {
        if (parsed.searchParams.get('is_archived') === 'true') {
          return { status: 200, text: JSON.stringify({ items: [{ id: 'archived-while-parked' }] }) };
        }
        return { status: 200, text: JSON.stringify({ items: parsed.searchParams.get('offset') === '0' ? [{ id: 'parked-main' }] : [] }) };
      }
      if (parsed.pathname === '/backend-api/conversation/parked-main') {
        return { status: 200, text: JSON.stringify({ mapping: {}, current_node: 'empty-parked-main' }) };
      }
      throw new Error('unexpected synthetic route');
    };
    const plan = {
      ...CHATGPT_PLAN,
      parseDetailPage: (text: string) => text.includes('empty-')
        ? { ok: true as const, outcome: 'detail-empty-unverified' as const }
        : { ok: true as const, outcome: 'non-empty' as const },
    };
    const options = {
      platform: 'chatgpt', origin: 'https://chatgpt.com', scope: 'chatgpt:opaque-workspace',
      store, http, plans: () => plan,
      clock: { now: () => 1, sleep: async () => {} },
    } as const;

    const first = await runBackfill(options);
    expect(first.state.parkedEmpty).toContain('parked-main');
    await runBackfill(options);
    const third = await runBackfill(options);

    expect(calls.map((url) => [new URL(url).pathname, new URL(url).search]).join('|'))
      .toContain('is_archived=true');
    expect(third.state.pending).toContain('parked-main');
    expect(third.state.pending).toContain('archived-while-parked');
  });
});
