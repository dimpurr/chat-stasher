import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { coordinate, PROTOCOL } from '../lib/native-host';
import { backfillPlansForCoordination, coordinationSegmentForRequest } from '../lib/backfill/coordination';
import { withI18n } from './i18n-harness';

let requests: Array<Record<string, unknown>>;
let response: unknown;
const runtime: any = {
  id: 'w212-coordination-test',
  lastError: undefined,
  sendNativeMessage(_host: string, message: Record<string, unknown>, callback: (value: unknown) => void) {
    requests.push(message);
    const reply = response as Record<string, unknown> | null;
    callback(reply?.type === 'coordination' ? { ...reply, request_id: message.request_id } : response);
  },
};

beforeEach(() => {
  requests = [];
  response = undefined;
  vi.stubGlobal('browser', withI18n({ runtime }));
  vi.stubGlobal('chrome', { runtime });
});

afterEach(() => vi.unstubAllGlobals());

describe('EXT-3 native-host coordination', () => {
  it('classifies platform detail routes separately from list routes', () => {
    expect(coordinationSegmentForRequest('chatgpt', 'https://chatgpt.com/backend-api/conversation/opaque-id')).toBe('detail');
    expect(coordinationSegmentForRequest('claude', 'https://claude.ai/api/organizations/org/chat_conversations/opaque-id')).toBe('detail');
    expect(coordinationSegmentForRequest('deepseek', 'https://chat.deepseek.com/api/v0/chat/history_messages?chat_session_id=opaque-id')).toBe('detail');
    expect(coordinationSegmentForRequest('perplexity', 'https://www.perplexity.ai/rest/thread/opaque-id')).toBe('detail');
    expect(coordinationSegmentForRequest('grok', 'https://grok.com/rest/app-chat/conversations/opaque-id/response-node')).toBe('detail');
    expect(coordinationSegmentForRequest('grok', 'https://grok.com/rest/app-chat/conversations/opaque-id/load-responses')).toBe('detail');
    expect(coordinationSegmentForRequest('kimi', 'https://www.kimi.com/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages')).toBe('detail');
    expect(coordinationSegmentForRequest('claude', 'https://claude.ai/api/organizations/org/chat_conversations?limit=100&offset=0')).toBe('enumerate');
    expect(() => coordinationSegmentForRequest('gemini', 'https://gemini.google.com/_/BardChatUi/data/batchexecute?rpcids=unplanned'))
      .toThrow('query is not declared by the gemini backfill plan');
  });

  it('classifies every request route declared by every backfill plan', () => {
    const origin = 'https://synthetic.invalid';
    const materializeScope = (url: string) => url.replaceAll('{org}', 'opaque-org');
    for (const plan of backfillPlansForCoordination()) {
      const requests: Array<{ url: string; segment: 'enumerate' | 'detail' }> = [
        { url: materializeScope(plan.listUrl(origin, 0, 20)), segment: 'enumerate' },
      ];
      if (plan.listCursorUrl) {
        requests.push({ url: materializeScope(plan.listCursorUrl(origin, 0, 20)), segment: 'enumerate' });
      }
      if (plan.listTokenUrl) {
        requests.push({ url: materializeScope(plan.listTokenUrl(origin, 'opaque-token', 20)), segment: 'enumerate' });
      }
      if (plan.scopeInPath) {
        requests.push({ url: `${origin}${plan.scopeInPath.resolvePath}`, segment: 'enumerate' });
      }
      if (plan.detailUrl) {
        requests.push({ url: materializeScope(plan.detailUrl(origin, 'opaque-id')), segment: 'detail' });
      }
      if (plan.detailPages) {
        requests.push({ url: materializeScope(plan.detailPages.url(origin, 'opaque-id')), segment: 'detail' });
      }
      if (plan.detailStep2) {
        requests.push({ url: materializeScope(plan.detailStep2.url(origin, 'opaque-id')), segment: 'detail' });
      }
      for (const request of requests) {
        expect(coordinationSegmentForRequest(plan.platform, request.url), `${plan.platform}: ${new URL(request.url).pathname}`)
          .toBe(request.segment);
      }
    }
  });

  it('fails closed for a URL outside its platform request plan', () => {
    expect(() => coordinationSegmentForRequest('grok', 'https://grok.com/rest/app-chat/conversations/opaque-id/unplanned'))
      .toThrow('not declared by the grok backfill plan');
  });

  it('validates a matching arbiter response and sends install-scoped fields', async () => {
    response = { protocol: PROTOCOL, type: 'coordination', ok: true, request_id: 'echo',
      granted: true, active_installs: 2, gentle: true, cooldown_until: 0, wait_ms: 0 };
    expect(await coordinate({ mode: 'claim', platform: 'chatgpt', installId: 'synthetic-install', accountId: 'synthetic-account' }))
      .toMatchObject({ ok: true, granted: true, activeInstalls: 2, gentle: true });
    expect(requests[0]).toMatchObject({ type: 'coordination', mode: 'claim', platform: 'chatgpt', install_id: 'synthetic-install', account_id: 'synthetic-account' });
  });

  it('reports coordination unavailable when an old host rejects the new message', async () => {
    response = { protocol: PROTOCOL, type: 'nack', request_id: null, kind: 'bad-request', retryable: false,
      detail: 'unknown message type "coordination"' };
    const result = await coordinate({ mode: 'claim', platform: 'deepseek', installId: 'synthetic-install' });
    expect(result.ok).toBe(false);
    expect(result.reason).toContain('bad-request');
    expect(result.olderHost).toBe(true);
  });

  it('does not label an arbitrary bad-request nack as an older host', async () => {
    response = { protocol: PROTOCOL, type: 'nack', request_id: null, kind: 'bad-request', retryable: false,
      detail: 'malformed coordination request' };
    const result = await coordinate({ mode: 'claim', platform: 'deepseek', installId: 'synthetic-install' });
    expect(result).toMatchObject({ ok: false, olderHost: false });
  });
});
