/**
 * W2 · 端到端合成链：页面钩子 → postMessage → 桥 → background → 发件箱 → 本机 host。
 *
 * 这是 chain.test.ts 的替代品。旧版整条链的落点是 `chrome.downloads`
 * （真实文件系统），而那条通道连同 `downloads` 权限已经删除；不变的是这条
 * 用例的**形状**：加载真实的 entrypoint 源码、真的跑一遍，只把浏览器 API
 * 与主机换成桩。
 *
 * 🔴 判据与旧版一一对应，只是落点从「磁盘上有没有那个文件」换成了
 *    「主机收到并 ack 了哪一条 payload」—— 后者才是规范 §1 里算数的东西。
 *
 * 全程零真实网络、零登录态、零真实文件。
 */

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { IDBFactory } from 'fake-indexeddb';
import { type CapturedFetch } from '../lib/contract';
import { createSyntheticHost, type SyntheticHost } from './synthetic-native-host';

const runtimeListeners: Array<{
  fn: (msg: any, sender: any, sendResponse: (r: any) => void) => any;
}> = [];

let host: SyntheticHost;

/** Each test simulates a fresh extension: clear all registered listeners/records. */
function resetMocks() {
  runtimeListeners.length = 0;
  vi.unstubAllGlobals();
}

beforeEach(() => {
  resetMocks();
  host = createSyntheticHost({ up: true });
  (globalThis as any).indexedDB = new IDBFactory();
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineContentScript', (cfg: any) => cfg);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
});

const fakeBrowser: any = {
  runtime: {
    id: 'mock-extension-id',
    onStartup: { addListener() { /* startup badge refresh is a no-op here */ } },
    onMessage: {
      addListener(fn: any) {
        runtimeListeners.push({ fn });
      },
    },
    sendNativeMessage: (h: string, m: unknown) => host.sendNativeMessage(h, m),
    async sendMessage(msg: any): Promise<any> {
      return new Promise((resolve, reject) => {
        let settled = false;
        const doResolve = (v: any) => {
          if (!settled) {
            settled = true;
            resolve(v);
          }
        };
        for (const { fn } of runtimeListeners) {
          try {
            const ret = fn(msg, { id: 'mock-sender', url: null }, doResolve);
            if (ret && typeof ret.then === 'function') {
              ret.then(doResolve).catch(reject);
            } else if (ret !== true) {
              doResolve(ret);
            }
            // ret === true => async, waits for sendResponse() (called above)
          } catch (err) {
            reject(err);
            return;
          }
        }
        setTimeout(() => doResolve(undefined), 5000).unref?.();
      });
    },
  },
  action: {
    async setBadgeText() {},
    async setBadgeBackgroundColor() {},
    async setTitle() {},
  },
};

async function loadBackground() {
  const mod = await import('../entrypoints/background');
  const bg = (mod as any).default;
  await bg(); // simulate service worker startup
}

async function loadBridge() {
  const mod: any = await import('../entrypoints/dw-bridge.content');
  mod.default.main();
}

async function loadMainHook() {
  const mod: any = await import('../entrypoints/dw-fetch-main.content');
  mod.default.main();
  return mod.default;
}

function makeFakeWindow() {
  const listeners: Record<string, Array<(e: any) => void>> = {};
  return {
    listeners,
    location: { origin: 'https://chat.deepseek.com' },
    addEventListener(name: string, fn: (e: any) => void) {
      (listeners[name] ??= []).push(fn);
    },
    dispatchEvent(e: any) {
      for (const fn of listeners[e.type] ?? []) fn(e);
      return true;
    },
    postMessage(data: unknown, targetOrigin: string) {
      if (targetOrigin !== this.location.origin) return;
      for (const fn of listeners.message ?? []) fn({ source: this, origin: this.location.origin, data });
    },
    fetch: null as any,
  };
}

describe('W2 · 合成链：页面 → 桥 → background → 发件箱 → host', () => {
  it('一次真实抓取最终以匹配的 ack 收尾，payload 逐字是一个合规的 inbox bundle', async () => {
    await loadBackground();

    const fakeWin = makeFakeWindow();
    vi.stubGlobal('window', fakeWin);
    await loadBridge();

    const rawBody = JSON.stringify({
      session_id: 'c622b5dd-0000-4000-8000-00000000abcd',
      message: { content: 'synthetic answer, never printed in reports', reasoning_content: '' },
    });
    fakeWin.fetch = async () =>
      new Response(rawBody, { status: 200, headers: { 'content-type': 'application/json' } });
    await loadMainHook();

    const fakeUrl = 'https://chat.deepseek.com/api/v0/chat/session/c622b5dd-0000-4000-8000-00000000abcd';
    await (fakeWin.fetch as any)(fakeUrl);

    // 让投递的微任务与 IndexedDB 事务跑完。
    await new Promise((r) => setTimeout(r, 100));

    expect(host.deliveries).toHaveLength(1);
    const delivery = host.deliveries[0]!;
    console.log('[W2-CHAIN] 主机收到的名字:', delivery.name, '· payload 字节:', delivery.payload.length);

    // §6.2：名字必须符合规范。
    expect(delivery.name).toBe('deepseek-c622b5dd-0000-4000-8000-00000000abcd.json');
    // §6.2：sha256 是 payload UTF-8 字节的 SHA-256 —— 主机自己算了一遍并核对过。
    expect(delivery.sha256).toMatch(/^[0-9a-f]{64}$/);

    const doc = JSON.parse(delivery.payload);
    expect(doc.schema).toBe('chat-stasher/inbox@2');
    expect(doc.platform).toBe('deepseek');
    expect(doc.sessionId).toBe('c622b5dd-0000-4000-8000-00000000abcd');
    // C3: this synthetic body carries no account fields, so the ADR-002 chain
    // falls through to an explicit 'default' marker — never silently missing.
    expect(doc.identity).toEqual({ level: 'default', value: '' });
    // raw.bytes = size of the ORIGINAL response.
    expect(doc.raw.bytes).toBe(Buffer.byteLength(rawBody, 'utf8'));

    // ack 之后发件箱是空的：没有任何"已经送出去但还留着"的条目。
    const { listEntries } = await import('../lib/outbox');
    expect(await listEntries()).toEqual([]);
  });

  it('非会话流量（别的路径 / 别的源 / GET 之外）一条都不送', async () => {
    await loadBackground();

    const fakeWin = makeFakeWindow();
    vi.stubGlobal('window', fakeWin);
    await loadBridge();

    fakeWin.fetch = async () => new Response('{}', { status: 200 });
    await loadMainHook();

    const nonSessionUrls = [
      'https://chat.deepseek.com/api/v0/users/me',
      'https://chat.deepseek.com/api/v0/payments/session/sess-x',
      'https://example.com/api/v0/chat/anything', // not DeepSeek origin
    ];
    for (const u of nonSessionUrls) await (fakeWin.fetch as any)(u);
    await new Promise((r) => setTimeout(r, 300));

    expect(host.deliveries).toEqual([]);
    const { listEntries } = await import('../lib/outbox');
    expect(await listEntries()).toEqual([]);
    console.log('[W2-CHAIN-EVIDENCE] 非会话流量产生的投递数:', host.deliveries.length);
  });

  it('主机不在时，同一次抓取会留在发件箱里（write-ahead 的端到端形态）', async () => {
    host = createSyntheticHost({ up: false });
    await loadBackground();

    const payload: CapturedFetch = {
      url: 'https://chat.deepseek.com/api/v0/chat/session/aaaa1111-bbbb-4000-8000-00000000ffff',
      method: 'POST',
      status: 200,
      text: JSON.stringify({ session_id: 'aaaa1111-bbbb-4000-8000-00000000ffff', message: { content: 'x' } }),
      capturedAt: Date.now(),
    };
    const { handleCaptured } = await import('../entrypoints/background');
    const result = await handleCaptured(payload);

    expect(result).toMatchObject({ saved: false, status: 'queued' });
    const { listEntries } = await import('../lib/outbox');
    const entries = (await listEntries())!;
    expect(entries).toHaveLength(1);
    const doc = JSON.parse(entries[0]!.payload);
    expect(doc.sessionId).toBe('aaaa1111-bbbb-4000-8000-00000000ffff');
    // 名字已经在发件箱里定好了 —— 主机回来时会照这个名字落成分片。
    expect(entries[0]!.name).toBe('deepseek-aaaa1111-bbbb-4000-8000-00000000ffff.json');
  });
});
