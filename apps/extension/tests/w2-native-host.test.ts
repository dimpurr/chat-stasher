/**
 * W2 · 传输层：唯一算数的成功 = 匹配的 ack。
 *
 * 规范 §1：
 *   「A conversation counts as delivered only when the extension holds an `ack`
 *     whose `request_id` and `sha256` equal the ones it sent.」
 *
 * 🔴 这个文件里【每一个】反面用例，都同时钉两件事：
 *      · 传输层必须报 `delivered:false`（不是 true，也不是"大概成了"）；
 *      · 每一种未送达都必须有一个具名 `reason`（不许静默吞错）。
 *
 * 全程零真实进程、零真实浏览器：`runtime.sendNativeMessage` 是本文件里的一个
 * 可编程桩，要什么回答就给什么回答。
 */

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import {
  deliver,
  hello,
  isValidDeliverName,
  NATIVE_HOST_NAME,
  PROTOCOL,
  REQUEST_TIMEOUT_MS,
} from '../lib/native-host';

type Responder = (
  message: Record<string, unknown>,
) => { via: 'callback'; response: unknown; lastError?: string } | { via: 'promise'; response: unknown } | { via: 'throw'; error: string } | { via: 'silent' };

let responder: Responder;
let sent: Array<Record<string, unknown>>;

const fakeBrowser: any = {
  runtime: {
    id: 'w2-native-host-test',
    lastError: undefined as { message?: string } | undefined,
    sendNativeMessage(host: string, message: Record<string, unknown>, callback: (r: unknown) => void) {
      sent.push(message);
      const outcome = responder(message);
      if (outcome.via === 'throw') throw new Error(outcome.error);
      if (outcome.via === 'silent') return undefined;
      if (outcome.via === 'promise') return Promise.resolve(outcome.response);
      // Chrome 形状：lastError 只在回调的那一拍有效。
      fakeBrowser.runtime.lastError = outcome.lastError ? { message: outcome.lastError } : undefined;
      callback(outcome.response);
      fakeBrowser.runtime.lastError = undefined;
      return undefined;
    },
  },
};

beforeEach(() => {
  sent = [];
  responder = () => ({ via: 'callback', response: undefined });
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  fakeBrowser.runtime.lastError = undefined;
});

afterEach(() => {
  vi.useRealTimers();
});

/**
 * 等到「请求真的发出去了」。
 *
 * 🔴 为什么需要它：`deliver` 先用 `crypto.subtle` 算 SHA-256，而那是**真实的**
 *    异步（libuv 线程池），假定时器管不到它。先推时间再等 digest 完成的话，
 *    60 秒的超时定时器会被建在一个已经过去的时刻上，测试就会挂住。
 *    所以：假定时器只接管 setTimeout，先把真实的那一步等完，再推时间。
 */
async function waitUntilSent(): Promise<void> {
  for (let i = 0; i < 1000 && sent.length === 0; i += 1) {
    await new Promise((resolve) => setImmediate(resolve));
  }
  if (sent.length === 0) throw new Error('the request was never sent');
}

/** 一条 §6.2 里完全合法的 ack，字段可被覆盖以制造反面。 */
function ackFor(message: Record<string, unknown>, overrides: Record<string, unknown> = {}): unknown {
  return {
    protocol: 1,
    type: 'ack',
    request_id: message.request_id,
    status: 'stored',
    sha256: message.sha256,
    shard: '0001-chatgpt-abc.json',
    ...overrides,
  };
}

const NAME = 'chatgpt-11111111-2222-3333-4444-555555555555.json';
const PAYLOAD = JSON.stringify({ schema: 'chat-stasher/inbox@2', sessionId: 'x', raw: { text: 'hi' } });

/** 用独立实现算一次 hash，证明 deliver 报出去的确实是 payload 字节的 SHA-256。 */
async function independentSha256(text: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text));
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

// ===========================================================================
// 1 · 请求本身
// ===========================================================================
describe('W2 · deliver 发出去的请求', () => {
  it('逐字符合 §6.2：protocol/type/request_id/name/payload/sha256，且 sha256 是 payload UTF-8 字节的 SHA-256', async () => {
    responder = (message) => ({ via: 'callback', response: ackFor(message) });

    const result = await deliver(NAME, PAYLOAD);
    expect(result.delivered).toBe(true);

    expect(sent).toHaveLength(1);
    const req = sent[0]!;
    expect(Object.keys(req).sort()).toEqual(
      ['name', 'payload', 'protocol', 'request_id', 'sha256', 'type'],
    );
    expect(req.protocol).toBe(PROTOCOL);
    expect(req.type).toBe('deliver');
    expect(req.name).toBe(NAME);
    expect(req.payload).toBe(PAYLOAD);                       // 原样，一个字节都不动
    expect(req.sha256).toBe(await independentSha256(PAYLOAD));
    expect(String(req.request_id)).toMatch(/^[A-Za-z0-9_-]{1,128}$/);
    console.log('[W2-NM] 请求:', { ...req, payload: `<${PAYLOAD.length} chars>` });
  });

  it('两条请求的 request_id 不相同（randomUUID，不是常量）', async () => {
    responder = (message) => ({ via: 'callback', response: ackFor(message) });
    await deliver(NAME, PAYLOAD);
    await deliver(NAME, PAYLOAD);
    expect(sent[0]!.request_id).not.toBe(sent[1]!.request_id);
  });

  it('§6.2 的 name 规则逐字实现（不合规的名字会被自己挡下来）', () => {
    expect(isValidDeliverName('chatgpt-abc.json')).toBe(true);
    expect(isValidDeliverName('deepseek-c622b5dd-0000-4000-8000-00000000abcd.json')).toBe(true);
    expect(isValidDeliverName('ChatGPT-abc.json')).toBe(false);   // 平台段必须小写
    expect(isValidDeliverName('chatgpt-a/b.json')).toBe(false);   // 不许带路径
    expect(isValidDeliverName('chatgpt-abc.txt')).toBe(false);
    expect(isValidDeliverName('chatgpt-abc')).toBe(false);
  });
});

// ===========================================================================
// 2 · 🔴 只有完全匹配的 ack 才算送达
// ===========================================================================
describe('W2 · 🔴 回归：没有匹配的 ack 就绝不算送达', () => {
  it('完全匹配的 ack ⇒ delivered:true，并带上主机的 status 与 shard', async () => {
    responder = (message) => ({
      via: 'callback',
      response: ackFor(message, { status: 'duplicate', shard: '0007-x.json' }),
    });
    const result = await deliver(NAME, PAYLOAD);
    expect(result).toEqual({
      delivered: true,
      status: 'duplicate',
      shard: '0007-x.json',
      requestId: expect.any(String),
      sha256: await independentSha256(PAYLOAD),
    });
  });

  it('超时（§2 的 60 秒内没有任何回答）⇒ timeout，且【绝不】是 delivered', async () => {
    responder = () => ({ via: 'silent' });
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
    const pending = deliver(NAME, PAYLOAD);
    await waitUntilSent();
    await vi.advanceTimersByTimeAsync(REQUEST_TIMEOUT_MS + 1);
    const result = await pending;

    expect(result.delivered).toBe(false);
    expect(result).toMatchObject({ reason: 'timeout', retryable: true });
    console.log('[W2-NM] 超时:', result);
  });

  it('runtime.lastError ⇒ send-failed（具名，不是静默）', async () => {
    responder = () => ({ via: 'callback', response: undefined, lastError: 'Specified native messaging host not found.' });
    const result = await deliver(NAME, PAYLOAD);
    expect(result.delivered).toBe(false);
    expect(result).toMatchObject({
      reason: 'send-failed',
      detail: 'Specified native messaging host not found.',
      retryable: true,
    });
  });

  it('调用直接抛错 ⇒ send-failed', async () => {
    responder = () => ({ via: 'throw', error: 'boom' });
    const result = await deliver(NAME, PAYLOAD);
    expect(result).toMatchObject({ delivered: false, reason: 'send-failed' });
  });

  it('既没有回答也没有 lastError ⇒ send-failed（沉默不是成功）', async () => {
    responder = () => ({ via: 'callback', response: undefined });
    const result = await deliver(NAME, PAYLOAD);
    expect(result).toMatchObject({ delivered: false, reason: 'send-failed' });
  });

  it('ack 的 request_id 不符 ⇒ malformed-response，条目不许被当成已送达', async () => {
    responder = (message) => ({
      via: 'callback',
      response: ackFor(message, { request_id: 'some-other-request' }),
    });
    const result = await deliver(NAME, PAYLOAD);
    expect(result.delivered).toBe(false);
    expect(result).toMatchObject({ reason: 'malformed-response' });
    expect((result as { detail?: string }).detail).toContain('request_id');
  });

  it('ack 的 sha256 不符 ⇒ malformed-response', async () => {
    responder = (message) => ({
      via: 'callback',
      response: ackFor(message, { sha256: 'f'.repeat(64) }),
    });
    const result = await deliver(NAME, PAYLOAD);
    expect(result.delivered).toBe(false);
    expect(result).toMatchObject({ reason: 'malformed-response' });
    expect((result as { detail?: string }).detail).toContain('sha256');
  });

  it('响应缺字段 ⇒ malformed-response', async () => {
    responder = (message) => {
      const ack = ackFor(message) as Record<string, unknown>;
      delete ack.shard;
      return { via: 'callback', response: ack };
    };
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'malformed-response',
    });
  });

  it('响应多字段 ⇒ malformed-response（schema 的 additionalProperties:false）', async () => {
    responder = (message) => ({
      via: 'callback',
      response: ackFor(message, { extra: 'not in the schema' }),
    });
    const result = await deliver(NAME, PAYLOAD);
    expect(result).toMatchObject({ delivered: false, reason: 'malformed-response' });
    expect((result as { detail?: string }).detail).toContain('unexpected field');
  });

  it('字段类型错 ⇒ malformed-response', async () => {
    const cases: Array<Record<string, unknown>> = [
      { status: 42 },
      { status: 'ok' },
      { sha256: 12345 },
      { shard: null },
      { request_id: 99 },
    ];
    for (const override of cases) {
      responder = (message) => ({ via: 'callback', response: ackFor(message, override) });
      const result = await deliver(NAME, PAYLOAD);
      expect([override, result.delivered]).toEqual([override, false]);
      expect(result).toMatchObject({ reason: 'malformed-response' });
    }
  });

  it('响应不是对象（字符串 / 数组 / null）⇒ malformed-response', async () => {
    for (const response of ['an ack, trust me', [1, 2, 3], null, 7]) {
      responder = () => ({ via: 'callback', response });
      const result = await deliver(NAME, PAYLOAD);
      expect([response, result.delivered]).toEqual([response, false]);
      expect(result).toMatchObject({ reason: 'malformed-response' });
    }
  });

  it('type 既不是 ack 也不是 nack ⇒ malformed-response', async () => {
    responder = (message) => ({
      via: 'callback',
      response: { ...(ackFor(message) as object), type: 'ok' },
    });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'malformed-response',
    });
  });

  it('协议版本不符 ⇒ malformed-response（本扩展只说 protocol 1，§9）', async () => {
    responder = (message) => ({
      via: 'callback',
      response: ackFor(message, { protocol: 2 }),
    });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'malformed-response',
    });
  });

  it('Promise 形状（Firefox）同样被认：匹配的 ack 仍然 ⇒ delivered:true', async () => {
    responder = (message) => ({ via: 'promise', response: ackFor(message) });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({ delivered: true, status: 'stored' });
  });
});

// ===========================================================================
// 3 · nack：永远不是送达，但它的 kind / retryable 要原样带出来
// ===========================================================================
describe('W2 · nack 的分类', () => {
  it('retryable nack ⇒ reason:nack + kind + retryable:true', async () => {
    responder = (message) => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'nack', request_id: message.request_id,
        kind: 'io', retryable: true, detail: 'sealing failed',
      },
    });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'nack', kind: 'io', retryable: true, detail: 'sealing failed',
    });
  });

  it('非 retryable nack ⇒ retryable:false（条目会被判死，但仍保留）', async () => {
    responder = (message) => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'nack', request_id: message.request_id,
        kind: 'invalid-bundle', retryable: false, detail: 'payload is not an inbox bundle',
      },
    });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'nack', kind: 'invalid-bundle', retryable: false,
    });
  });

  it('request_id 为 null 的 nack 仍然算数（§6.3：读不到就是 null）', async () => {
    responder = () => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'nack', request_id: null,
        kind: 'protocol-version', retryable: false, detail: 'unsupported', supported: [1],
      },
    });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'nack', kind: 'protocol-version', retryable: false,
    });
  });

  it('🔴 request_id 是【别人的】的 nack ⇒ malformed-response（不能拿它判死用户的数据）', async () => {
    responder = () => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'nack', request_id: 'not-ours',
        kind: 'invalid-bundle', retryable: false, detail: 'x',
      },
    });
    const result = await deliver(NAME, PAYLOAD);
    expect(result).toMatchObject({ delivered: false, reason: 'malformed-response', retryable: true });
    expect(result).not.toMatchObject({ kind: 'invalid-bundle' });
  });

  it('形状不合法的 nack ⇒ malformed-response（不认它说的 kind）', async () => {
    responder = (message) => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'nack', request_id: message.request_id,
        kind: 'something-new', retryable: 'yes', detail: 'x',
      },
    });
    expect(await deliver(NAME, PAYLOAD)).toMatchObject({
      delivered: false, reason: 'malformed-response',
    });
  });
});

// ===========================================================================
// 4 · 连 runtime API 都没有
// ===========================================================================
describe('W2 · 没有 API 的环境', () => {
  it('没有 runtime.sendNativeMessage ⇒ no-runtime-api（什么都没尝试，也绝不假装成功）', async () => {
    const saved = fakeBrowser.runtime.sendNativeMessage;
    delete (fakeBrowser.runtime as any).sendNativeMessage;
    try {
      const result = await deliver(NAME, PAYLOAD);
      expect(result).toMatchObject({ delivered: false, reason: 'no-runtime-api' });
      expect(sent).toEqual([]);            // 一个字节都没发出去
      expect(await hello()).toMatchObject({ ok: false, reason: 'no-runtime-api' });
    } finally {
      fakeBrowser.runtime.sendNativeMessage = saved;
    }
  });
});

// ===========================================================================
// 5 · hello（§6.1）
// ===========================================================================
describe('W2 · hello', () => {
  it('合法回答 ⇒ {ok:true, machine, stage, hostVersion}', async () => {
    responder = () => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'hello', ok: true,
        host_version: '0.3.0', machine: 'm-1', stage: '/Users/x/stage',
      },
    });
    const result = await hello();
    expect(result).toEqual({ ok: true, machine: 'm-1', stage: '/Users/x/stage', hostVersion: '0.3.0' });
    // 请求逐字是 §6.1 的那一条。
    expect(sent[0]).toEqual({ protocol: 1, type: 'hello' });
  });

  it('nack ⇒ {ok:false, reason:nack, kind, detail}（并带上修复线索）', async () => {
    responder = () => ({
      via: 'callback',
      response: {
        protocol: 1, type: 'nack', request_id: null,
        kind: 'config', retryable: false,
        detail: 'run `chat-stasher install-native-host --stage <path>`',
      },
    });
    const result = await hello();
    expect(result.ok).toBe(false);
    expect(result).toMatchObject({ reason: 'nack', kind: 'config', retryable: false });
    expect((result as { detail?: string }).detail).toContain('install-native-host');
  });

  it('形状不合法的 hello 回答 ⇒ malformed-response', async () => {
    responder = () => ({
      via: 'callback',
      response: { protocol: 1, type: 'hello', ok: true, host_version: '0.3.0', machine: 'm', stage: 42 },
    });
    expect(await hello()).toMatchObject({ ok: false, reason: 'malformed-response' });
  });

  it('ok:true 以外（ok:false 的伪 hello 回答）⇒ malformed-response，绝不当成连上了', async () => {
    responder = () => ({
      via: 'callback',
      response: { protocol: 1, type: 'hello', ok: false, host_version: '0', machine: 'm', stage: '/s' },
    });
    expect(await hello()).toMatchObject({ ok: false, reason: 'malformed-response' });
  });

  it('hello 的探测窗口可以被调用方缩短（Popup 用 3 秒，投递恒为 60 秒）', async () => {
    responder = () => ({ via: 'silent' });
    vi.useFakeTimers();
    const pending = hello({ timeoutMs: 3_000 });
    await vi.advanceTimersByTimeAsync(3_001);
    expect(await pending).toMatchObject({ ok: false, reason: 'timeout' });
  });
});

// ===========================================================================
// 6 · 主机名
// ===========================================================================
describe('W2 · 主机名', () => {
  it('用的就是契约里那个钉死的名字', async () => {
    let host = '';
    fakeBrowser.runtime.sendNativeMessage = (h: string, message: Record<string, unknown>, cb: (r: unknown) => void) => {
      host = h;
      sent.push(message);
      cb(ackFor(message));
      return undefined;
    };
    await deliver(NAME, PAYLOAD);
    expect(host).toBe(NATIVE_HOST_NAME);
    expect(NATIVE_HOST_NAME).toBe('com.chat_stasher.host');
  });
});
