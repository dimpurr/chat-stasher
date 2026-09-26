import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { coordinate, PROTOCOL } from '../lib/native-host';
import { coordinationSegmentForRequest } from '../lib/backfill/coordination';
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
    expect(coordinationSegmentForRequest('https://chatgpt.com/backend-api/conversation/opaque-id')).toBe('detail');
    expect(coordinationSegmentForRequest('https://claude.ai/api/organizations/org/chat_conversations/opaque-id')).toBe('detail');
    expect(coordinationSegmentForRequest('https://chat.deepseek.com/api/v0/chat/history_messages?chat_session_id=opaque-id')).toBe('detail');
    expect(coordinationSegmentForRequest('https://www.perplexity.ai/rest/thread/opaque-id')).toBe('detail');
    expect(coordinationSegmentForRequest('https://grok.com/rest/app-chat/conversations/opaque-id/response-node')).toBe('detail');
    expect(coordinationSegmentForRequest('https://www.kimi.com/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages')).toBe('detail');
    expect(coordinationSegmentForRequest('https://gemini.google.com/_/BardChatUi/data/batchexecute?rpcids=hNvQHb')).toBe('detail');
    expect(coordinationSegmentForRequest('https://claude.ai/api/organizations/org/chat_conversations?limit=100&offset=0')).toBe('enumerate');
  });

  it('validates a matching arbiter response and sends install-scoped fields', async () => {
    response = { protocol: PROTOCOL, type: 'coordination', ok: true, request_id: 'echo',
      granted: true, active_installs: 2, gentle: true, cooldown_until: 0, wait_ms: 0 };
    expect(await coordinate({ mode: 'claim', platform: 'chatgpt', installId: 'synthetic-install' }))
      .toMatchObject({ ok: true, granted: true, activeInstalls: 2, gentle: true });
    expect(requests[0]).toMatchObject({ type: 'coordination', mode: 'claim', platform: 'chatgpt', install_id: 'synthetic-install' });
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
