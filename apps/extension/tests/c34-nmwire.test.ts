/**
 * C34 · Native Messaging 接线与双通道降级守卫 (ADR-014 step 3)
 *
 * 守卫一：探测失败 / host 不可用 ⇒ 自动降级走 downloads，且 popup 给出降级文案及下一步指引
 * 守卫二：探测成功 / host 可用 ⇒ 走 NM，绝不调用 chrome.downloads（避免双写和 200 个弹窗）
 * 守卫三：健康路径守卫 ⇒ NM 正常时，popup 绝不出现降级提示
 */

import { describe, it, expect, beforeEach, vi } from 'vitest';
import type { CapturedFetch } from '../lib/contract';
import { NATIVE_HOST_NAME, POPUP_CHANNEL_FALLBACK, POPUP_CHANNEL_NM } from '../lib/native-host';

const downloadCalls: any[] = [];
const nmSentMessages: any[] = [];
let mockNmAvailable = false;
let mockNmLastError: string | null = 'Specified native messaging host not found.';

function createMockPort() {
  const messageListeners: Array<(msg: any) => void> = [];
  const disconnectListeners: Array<(port: any) => void> = [];

  const port = {
    name: NATIVE_HOST_NAME,
    postMessage(msg: any) {
      nmSentMessages.push(msg);
      // Simulate host self-test response or ack
      setTimeout(() => {
        for (const l of messageListeners) {
          l({ host: NATIVE_HOST_NAME, ok: true, version: '0.1.0' });
        }
      }, 0);
    },
    disconnect() {},
    onMessage: {
      addListener(fn: (msg: any) => void) {
        messageListeners.push(fn);
      },
    },
    onDisconnect: {
      addListener(fn: (port: any) => void) {
        disconnectListeners.push(fn);
        if (!mockNmAvailable) {
          setTimeout(() => {
            fn({ error: { message: mockNmLastError } });
          }, 0);
        }
      },
    },
  };
  return port;
}

const fakeBrowser: any = {
  runtime: {
    id: 'gihmdkkmmmkeiagjjiimacmgkdilofhi',
    lastError: null as any,
    onStartup: { addListener() {} },
    onMessage: { addListener() {} },
    connectNative(host: string) {
      if (host !== NATIVE_HOST_NAME) {
        throw new Error(`Unknown host ${host}`);
      }
      if (!mockNmAvailable) {
        fakeBrowser.runtime.lastError = { message: mockNmLastError };
      } else {
        fakeBrowser.runtime.lastError = null;
      }
      return createMockPort();
    },
  },
  storage: {
    local: {
      async get() { return {}; },
      async set() {},
      async remove() {},
    },
  },
  action: {
    async setBadgeText() {},
    async setTitle() {},
  },
  downloads: {
    async download(opts: any) {
      downloadCalls.push(opts);
      const id = downloadCalls.length;
      setTimeout(() => {
        for (const fn of changeListeners) fn({ id, state: { current: 'complete' } });
      }, 0);
      return id;
    },
    onChanged: {
      addListener(fn: any) {
        changeListeners.push(fn);
      },
    },
    async removeFile() {},
    async erase() {},
  },
};

const changeListeners: Array<(d: any) => void> = [];

function fakeCapture(): CapturedFetch {
  return {
    url: 'https://chatgpt.com/backend-api/conversation/11112222-3333-4444-5555-666677778888',
    method: 'GET',
    status: 200,
    text: JSON.stringify({ conversation_id: '11112222-3333-4444-5555-666677778888', mapping: {} }),
    pageUrl: 'https://chatgpt.com/c/11112222-3333-4444-5555-666677778888',
    capturedAt: 1_700_000_000_000,
  };
}

beforeEach(() => {
  downloadCalls.length = 0;
  nmSentMessages.length = 0;
  changeListeners.length = 0;
  mockNmAvailable = false;
  mockNmLastError = 'Specified native messaging host not found.';
  vi.resetModules();
  vi.unstubAllGlobals();
  vi.stubGlobal('browser', fakeBrowser);
  vi.stubGlobal('chrome', fakeBrowser);
  vi.stubGlobal('defineBackground', (cb: any) => cb);
});

describe('C34 · Native Messaging 双通道与守卫', () => {
  it('守卫 1（探测失败 ⇒ 降级走 downloads）：NM 不可用时仍然调用 downloads 落盘，且 popup 呈现降级引导', async () => {
    mockNmAvailable = false;
    const { handleCaptured } = await import('../entrypoints/background');
    const { renderPopup, popupText, NO_FAILURES } = await import('../lib/popup-view');

    const result = await handleCaptured(fakeCapture());
    expect(result.saved).toBe(true);
    // 降级路径断言：downloads 必须被调用
    expect(downloadCalls.length).toBeGreaterThanOrEqual(1);

    // Popup 降级文案断言
    const view = renderPopup({
      enabled: true,
      block: null,
      guard: null,
      state: null,
      target: null,
      failures: NO_FAILURES,
      nativeHost: { connected: false, reason: 'host-not-found' },
    });
    const text = popupText(view);
    expect(text).toContain(POPUP_CHANNEL_FALLBACK);
    expect(text).toContain('chat-stasher install-native-host');
    expect(text).not.toContain(POPUP_CHANNEL_NM);
  });

  it('守卫 2（探测成功 ⇒ 走 NM，不走 downloads）：NM 可用时通过 NM 发送，绝不调用 chrome.downloads', async () => {
    mockNmAvailable = true;
    const { handleCaptured } = await import('../entrypoints/background');

    const result = await handleCaptured(fakeCapture());
    expect(result.saved).toBe(true);
    expect(result.channel).toBe('native-messaging');

    // 关键守卫断言：NM 成功时禁止调用 downloads，防止双写和 200 个保存弹窗
    expect(downloadCalls).toHaveLength(0);
    expect(nmSentMessages.length).toBe(1);
    expect(nmSentMessages[0].schema).toBe('chat-stasher/inbox@2');
    expect(nmSentMessages[0].sessionId).toBe('11112222-3333-4444-5555-666677778888');
  });

  it('守卫 3（健康路径守卫）：NM 正常时，popup 绝不出现降级提示', async () => {
    mockNmAvailable = true;
    const { renderPopup, popupText, NO_FAILURES } = await import('../lib/popup-view');

    const view = renderPopup({
      enabled: true,
      block: null,
      guard: null,
      state: null,
      target: null,
      failures: NO_FAILURES,
      nativeHost: { connected: true, reason: 'connected' },
    });
    const text = popupText(view);
    expect(text).toContain(POPUP_CHANNEL_NM);
    expect(text).toContain('直接交给本机的 chat-stasher，不经过下载');
    // 健康路径守卫：绝不出现降级文案或引导跑 install-native-host
    expect(text).not.toContain(POPUP_CHANNEL_FALLBACK);
    expect(text).not.toContain('chat-stasher install-native-host');
    expect(text).not.toContain('没找到本机 host');
  });
});
