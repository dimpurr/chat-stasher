import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  UNSUPPORTED_TRANSPORT_WARNING,
} from '../lib/page-hook';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';
import { CAPTURE_MESSAGE } from '../lib/contract';

/**
 * A fresh XHR class per test. The hook patches `open`/`send` on the class it is given, and a
 * shared class would stack every earlier install's listeners onto later tests (a real page
 * installs once: the state/fetch marker guard returns early on a second call).
 */
function makeFakeXhr() {
  return class FakeXhr {
  static readonly OPENED = 1;
  status = 200;
  responseType: XMLHttpRequestResponseType | undefined = undefined;
  responseText = '';
  response: unknown = null;
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
  };
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

  it('warns once when a candidate conversation XHR completes with a body type it cannot read', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const fakeWindow: any = {
      location: {
        origin: 'https://chat.deepseek.com',
        href: 'https://chat.deepseek.com/chat/candidate',
      },
      fetch: async () => new Response('{}', { status: 200 }),
      XMLHttpRequest: makeFakeXhr(),
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
    // Reading an arraybuffer/blob/document body would change what the page sees,
    // so this case stays a visible "unsupported" warning (readable bodies are captured).
    xhr.responseType = 'arraybuffer';
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

/** The live shape of DeepSeek's history_messages (2026-09-13), with synthetic values. */
function deepseekHistoryBody(): string {
  return JSON.stringify({
    code: 0,
    msg: '',
    data: {
      biz_code: 0,
      biz_msg: '',
      biz_data: {
        chat_session: { id: '5fa1d6ed-0000-4000-8000-000000000001', title: 't', current_message_id: 2 },
        chat_messages: [
          { message_id: 1, parent_id: null, role: 'USER', fragments: [{ type: 'REQUEST', content: 'q' }] },
          { message_id: 2, parent_id: 1, role: 'ASSISTANT', fragments: [{ type: 'RESPONSE', content: 'a' }] },
        ],
        cache_control: 'x',
        cache_reset_at: null,
      },
    },
  });
}

describe('XHR capture (DeepSeek loads conversations over XHR)', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  function install() {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const posted: any[] = [];
    const fakeWindow: any = {
      location: {
        origin: 'https://chat.deepseek.com',
        href: 'https://chat.deepseek.com/a/chat/s/5fa1d6ed-0000-4000-8000-000000000001',
      },
      fetch: async () => new Response('{}', { status: 200 }),
      XMLHttpRequest: makeFakeXhr(),
      addEventListener() {},
      postMessage(message: unknown) { posted.push(message); },
    };
    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);
    const captures = () => posted.filter((m) => m?.type === CAPTURE_MESSAGE);
    return { warn, fakeWindow, captures };
  }

  it('🔴 a readable history_messages XHR is captured through the same path as fetch, with no warning', () => {
    const { warn, fakeWindow, captures } = install();
    const xhr = new fakeWindow.XMLHttpRequest();
    xhr.responseType = 'text';
    xhr.responseText = deepseekHistoryBody();
    const url = 'https://chat.deepseek.com/api/v0/chat/history_messages?chat_session_id=5fa1d6ed-0000-4000-8000-000000000001';
    xhr.open('GET', url);
    xhr.send();

    expect(captures()).toHaveLength(1);
    expect(captures()[0].payload).toMatchObject({ url, method: 'GET', status: 200, text: deepseekHistoryBody() });
    expect(typeof captures()[0].payload.capturedAt).toBe('number');
    expect(warn).not.toHaveBeenCalled();
  });

  it('the default responseType ("") is read as text too', () => {
    const { fakeWindow, captures } = install();
    const xhr = new fakeWindow.XMLHttpRequest();
    xhr.responseType = '';
    xhr.responseText = deepseekHistoryBody();
    xhr.open('GET', 'https://chat.deepseek.com/api/v0/chat/history_messages?chat_session_id=abc12345');
    xhr.send();
    expect(captures()).toHaveLength(1);
  });

  it('a readable candidate with the wrong shape gets the shape warning and is not captured', () => {
    const { warn, fakeWindow, captures } = install();
    const xhr = new fakeWindow.XMLHttpRequest();
    xhr.responseType = 'text';
    xhr.responseText = JSON.stringify({ code: 0, data: { biz_data: { unrelated: true } } });
    xhr.open('GET', 'https://chat.deepseek.com/api/v0/chat/history_messages?chat_session_id=abc12345');
    xhr.send();
    expect(captures()).toHaveLength(0);
    expect(warn).toHaveBeenCalledTimes(1);
    expect(warn).toHaveBeenCalledWith('[chat-stasher] capture skipped: response shape mismatch');
  });

  it('a request outside the platform path hints produces nothing at all', () => {
    const { warn, fakeWindow, captures } = install();
    const xhr = new fakeWindow.XMLHttpRequest();
    xhr.responseType = 'text';
    xhr.responseText = deepseekHistoryBody();
    xhr.open('GET', 'https://chat.deepseek.com/api/v0/users/current');
    xhr.send();
    expect(captures()).toHaveLength(0);
    expect(warn).not.toHaveBeenCalled();
  });

  it('a non-2xx XHR produces nothing at all', () => {
    const { warn, fakeWindow, captures } = install();
    const xhr = new fakeWindow.XMLHttpRequest();
    xhr.status = 500;
    xhr.responseType = 'text';
    xhr.responseText = deepseekHistoryBody();
    xhr.open('GET', 'https://chat.deepseek.com/api/v0/chat/history_messages?chat_session_id=abc12345');
    xhr.send();
    expect(captures()).toHaveLength(0);
    expect(warn).not.toHaveBeenCalled();
  });
});
