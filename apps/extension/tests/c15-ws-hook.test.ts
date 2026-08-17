import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  installPageFetchHook,
  PAGE_HOOK_OPTIONS,
  WEBSOCKET_HOOK_UNINSTALLED_WARNING,
  type PageHookOptions,
} from '../lib/page-hook';
import { PLATFORMS, WS_OBSERVED_MESSAGE } from '../lib/contract';

/**
 * A WebSocket stand-in that records everything the page would observe, so a
 * behaviour-change caused by our wrapper shows up as a failing assertion rather
 * than as a story about how careful the wrapper is.
 */
class FakeWebSocket {
  static readonly OPEN = 1;
  readonly constructorArgs: unknown[];
  readonly sent: unknown[] = [];
  closedWith: unknown[] | null = null;
  private listeners: Record<string, Array<(event: unknown) => void>> = {};

  constructor(...args: unknown[]) {
    this.constructorArgs = args;
  }

  get url(): string {
    return String(this.constructorArgs[0]);
  }

  addEventListener(name: string, listener: (event: unknown) => void): void {
    (this.listeners[name] ??= []).push(listener);
  }

  send(data: unknown): void {
    this.sent.push(data);
  }

  close(...args: unknown[]): void {
    this.closedWith = args;
    this.emit('close', { type: 'close', code: args[0] ?? 1000 });
  }

  emit(name: string, event: unknown): void {
    for (const listener of this.listeners[name] ?? []) listener(event);
  }
}

const WS_ORIGIN = 'https://ws.example.test';

/** A test-only platform row that opts in; no shipped platform row does. */
function declaredOptions(): PageHookOptions {
  return {
    ...PAGE_HOOK_OPTIONS,
    platforms: [
      {
        id: 'test-ws-platform',
        origins: [WS_ORIGIN],
        pathHints: ['/chathub'],
        methods: ['GET'],
        status: { min: 200, max: 299 },
        responseShape: { encoding: 'json', requiredPaths: ['chat_messages'] },
        sessionIdPatterns: ['/chathub/([0-9a-fA-F-]{8,})'],
        credibility: 'unverified',
        webSocketCapture: true,
      },
    ],
  };
}

function makeWindow(origin: string, overrides: Record<string, unknown> = {}): any {
  return {
    location: { origin, href: `${origin}/chat/candidate` },
    fetch: async () => new Response('{}', { status: 200 }),
    WebSocket: FakeWebSocket,
    addEventListener() {
      // No page-message handshake is needed for these tests.
    },
    postMessage() {
      // Overridden per test when the posted messages matter.
    },
    ...overrides,
  };
}

describe('C15 · WebSocket observation is opt-in per platform row', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('① does NOT observe on an origin that never declared WebSocket capture', () => {
    vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const posted: unknown[] = [];
    const fakeWindow = makeWindow('https://chat.deepseek.com', {
      postMessage: (message: unknown) => posted.push(message),
    });

    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    const socket = new fakeWindow.WebSocket('wss://chat.deepseek.com/api/v0/chat/history_messages');
    socket.emit('message', { data: '{"chat_messages":[]}' });

    const observed = posted.filter(
      (message: any) => message && message.type === WS_OBSERVED_MESSAGE,
    );
    expect(observed).toEqual([]);
    // And no shipped platform row may opt in.
    expect(PLATFORMS.filter((platform) => platform.webSocketCapture === true)).toEqual([]);
  });

  it('② leaves the page WebSocket behaviour identical (messages / events / close / errors)', () => {
    vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const posted: unknown[] = [];
    const fakeWindow = makeWindow(WS_ORIGIN, {
      postMessage: (message: unknown) => posted.push(message),
    });

    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(declaredOptions());

    const Wrapped = fakeWindow.WebSocket;
    expect(Wrapped).not.toBe(FakeWebSocket);
    // Statics and instanceof must survive the wrapper.
    expect(Wrapped.OPEN).toBe(1);

    const socket = new Wrapped(`${WS_ORIGIN.replace('https:', 'wss:')}/chathub`, ['proto-a']);
    expect(socket).toBeInstanceOf(FakeWebSocket);
    // Constructor arguments are forwarded verbatim.
    expect(socket.constructorArgs).toEqual([`wss://ws.example.test/chathub`, ['proto-a']]);

    const seen: Array<[string, unknown]> = [];
    socket.addEventListener('message', (event: any) => seen.push(['message', event]));
    socket.addEventListener('error', (event: any) => seen.push(['error', event]));
    socket.addEventListener('close', (event: any) => seen.push(['close', event]));

    const messageEvent = { data: '{"chat_messages":[]}' };
    socket.emit('message', messageEvent);
    const errorEvent = { type: 'error' };
    socket.emit('error', errorEvent);

    // The page's own send() is untouched and we never send anything ourselves.
    socket.send('page-frame');
    socket.close(1000, 'bye');

    expect(seen.map(([name]) => name)).toEqual(['message', 'error', 'close']);
    expect(seen[0]?.[1]).toBe(messageEvent);
    expect(seen[1]?.[1]).toBe(errorEvent);
    expect(socket.sent).toEqual(['page-frame']);
    expect(socket.closedWith).toEqual([1000, 'bye']);

    // Observation happened (that is the new capability) but only as a page message.
    const observed = posted.filter(
      (message: any) => message && message.type === WS_OBSERVED_MESSAGE,
    );
    expect(observed).toHaveLength(1);
    expect((observed[0] as any).payload.platformId).toBe('test-ws-platform');
    expect((observed[0] as any).payload.text).toBe('{"chat_messages":[]}');
  });

  it('③ leaves a trace when a declared origin does NOT get the wrapper installed', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);

    // Case A: the page (or CSP) made WebSocket non-writable — assignment fails.
    const guarded = makeWindow(WS_ORIGIN);
    Object.defineProperty(guarded, 'WebSocket', {
      value: FakeWebSocket,
      writable: false,
      configurable: true,
    });
    vi.stubGlobal('window', guarded);
    installPageFetchHook(declaredOptions());
    expect(warn).toHaveBeenCalledWith(WEBSOCKET_HOOK_UNINSTALLED_WARNING);
    expect(guarded.WebSocket).toBe(FakeWebSocket);
    vi.unstubAllGlobals();

    // Case B: no WebSocket constructor at all — capability detection, no throw.
    warn.mockClear();
    const missing = makeWindow(WS_ORIGIN, { WebSocket: undefined });
    vi.stubGlobal('window', missing);
    expect(() => installPageFetchHook(declaredOptions())).not.toThrow();
    expect(warn).toHaveBeenCalledWith(WEBSOCKET_HOOK_UNINSTALLED_WARNING);
  });

  it('④ an undeclared origin that cannot be wrapped stays silent about WS', () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const missing = makeWindow('https://chat.deepseek.com', { WebSocket: undefined });
    vi.stubGlobal('window', missing);
    installPageFetchHook(PAGE_HOOK_OPTIONS);
    expect(warn).not.toHaveBeenCalledWith(WEBSOCKET_HOOK_UNINSTALLED_WARNING);
  });
});
