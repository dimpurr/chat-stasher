import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  UNSUPPORTED_TRANSPORT_WARNING,
} from '../lib/page-hook';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

class FakeXhr {
  static readonly OPENED = 1;
  status = 200;
  private listeners: Record<string, Array<() => void>> = {};

  open(_method: string, _url: string): void {
    // The hook records the URL through the real XHR method call.
  }

  addEventListener(name: string, listener: () => void): void {
    (this.listeners[name] ??= []).push(listener);
  }

  send(): void {
    for (const listener of this.listeners.load ?? []) listener();
  }
}

class FakeEventSource {
  private listeners: Record<string, Array<() => void>> = {};

  constructor(readonly url: string) {}

  addEventListener(name: string, listener: () => void): void {
    (this.listeners[name] ??= []).push(listener);
  }

  emit(name: string): void {
    for (const listener of this.listeners[name] ?? []) listener();
  }
}

class FakeWebSocket {
  private listeners: Record<string, Array<() => void>> = {};

  constructor(readonly url: string) {}

  addEventListener(name: string, listener: () => void): void {
    (this.listeners[name] ??= []).push(listener);
  }

  emit(name: string): void {
    for (const listener of this.listeners[name] ?? []) listener();
  }
}

describe('unsupported transport visibility', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('warns once when a candidate conversation XHR completes', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const fakeWindow: any = {
      location: {
        origin: 'https://chat.deepseek.com',
        href: 'https://chat.deepseek.com/chat/candidate',
      },
      fetch: async () => new Response('{}', { status: 200 }),
      XMLHttpRequest: FakeXhr,
      addEventListener() {
        // No page-message handshake is needed for this test.
      },
      postMessage() {
        // The warning is deliberately local to the page console.
      },
    };

    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    const xhr = new fakeWindow.XMLHttpRequest();
    xhr.open('GET', 'https://chat.deepseek.com/api/v0/chat/history_messages');
    xhr.send();
    xhr.send();

    expect(warn).toHaveBeenCalledWith(UNSUPPORTED_TRANSPORT_WARNING);
    expect(warn).toHaveBeenCalledTimes(1);
    expect(warn.mock.calls[0]).toHaveLength(1);
  });

  it('warns for candidate SSE and WebSocket messages without reading payloads', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const fakeWindow: any = {
      location: {
        origin: 'https://chat.deepseek.com',
        href: 'https://chat.deepseek.com/chat/candidate',
      },
      fetch: async () => new Response('{}', { status: 200 }),
      EventSource: FakeEventSource,
      WebSocket: FakeWebSocket,
      addEventListener() {
        // No page-message handshake is needed for this test.
      },
      postMessage() {
        // The warning is deliberately local to the page console.
      },
    };

    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    const eventSource = new fakeWindow.EventSource(
      'https://chat.deepseek.com/api/v0/chat/history_messages',
    );
    eventSource.emit('message');
    const socket = new fakeWindow.WebSocket(
      'wss://chat.deepseek.com/api/v0/chat/history_messages',
    );
    socket.emit('message');

    expect(warn).toHaveBeenCalledTimes(2);
    expect(warn.mock.calls.every((call) => call.length === 1)).toBe(true);
    expect(warn.mock.calls.every((call) => call[0] === UNSUPPORTED_TRANSPORT_WARNING)).toBe(true);
  });
});
