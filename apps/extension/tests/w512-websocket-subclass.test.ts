import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  installPageFetchHook,
  PAGE_HOOK_OPTIONS,
} from '../lib/page-hook';
import { CAPTURE_MESSAGE } from '../lib/contract';

const WS_ORIGIN = 'https://ws.subclass.example.test';

class SyntheticWebSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  readonly constructorArgs: unknown[];
  readonly sent: unknown[] = [];
  private readonly listeners = new Map<string, Array<(event: unknown) => void>>();

  constructor(...args: unknown[]) {
    this.constructorArgs = args;
  }

  addEventListener(name: string, listener: (event: unknown) => void): void {
    const listeners = this.listeners.get(name) ?? [];
    listeners.push(listener);
    this.listeners.set(name, listeners);
  }

  send(data: unknown): void {
    this.sent.push(data);
  }

  emit(name: string, event: unknown): void {
    for (const listener of this.listeners.get(name) ?? []) listener(event);
  }
}

describe('W512 · WebSocket Proxy supports a page-defined subclass', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('preserves subclass identity, statics, constructor arguments, and page-visible behavior', () => {
    vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const posted: unknown[] = [];
    const fakeWindow: any = {
      location: { origin: WS_ORIGIN, href: `${WS_ORIGIN}/chat` },
      fetch: async () => new Response('{}', { status: 200 }),
      WebSocket: SyntheticWebSocket,
      addEventListener() {},
      postMessage: (message: unknown) => posted.push(message),
    };
    const options = {
      ...PAGE_HOOK_OPTIONS,
      platforms: [
        {
          id: 'synthetic-w512-websocket',
          origins: [WS_ORIGIN],
          pathHints: ['/chathub'],
          methods: ['GET'],
          status: { min: 200, max: 299 },
          responseShape: { encoding: 'json' as const, requiredPaths: ['chat_messages'] },
          streamTurnIdPaths: ['turn_id'],
          streamCompletionPaths: ['is_complete'],
          sessionIdPatterns: ['/chathub/([0-9a-fA-F-]{8,})'],
          credibility: 'unverified' as const,
          channel: 'experimental' as const,
          webSocketCapture: true,
        },
      ],
    };

    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(options);

    class PageWebSocket extends fakeWindow.WebSocket {
      static readonly PAGE_CONSTANT = 'page-constant';
      readonly subclassReady = true;

      constructor(...args: unknown[]) {
        super(...args);
      }

      send(data: unknown): void {
        super.send(data);
        this.emit('page-send', { data });
      }
    }

    const wrapped = fakeWindow.WebSocket;
    expect(wrapped).not.toBe(SyntheticWebSocket);
    expect(wrapped.OPEN).toBe(SyntheticWebSocket.OPEN);
    expect(PageWebSocket.OPEN).toBe(SyntheticWebSocket.OPEN);
    expect(PageWebSocket.PAGE_CONSTANT).toBe('page-constant');

    const args: [string, string[]] = [
      'wss://ws.subclass.example.test/chathub',
      ['synthetic-protocol'],
    ];
    const socket = new PageWebSocket(args[0], args[1]);
    expect(Object.getPrototypeOf(socket)).toBe(PageWebSocket.prototype);
    expect(socket).toBeInstanceOf(PageWebSocket);
    expect(socket).toBeInstanceOf(wrapped);
    expect(socket).toBeInstanceOf(SyntheticWebSocket);
    expect(socket.constructorArgs).toEqual(args);
    expect(socket.subclassReady).toBe(true);

    const seen: unknown[] = [];
    socket.addEventListener('message', (event: unknown) => seen.push(event));
    socket.addEventListener('page-send', (event: unknown) => seen.push(event));
    const message = { data: '{"chat_messages":[]}' };
    socket.emit('message', message);
    socket.send('synthetic-frame');

    expect(seen).toEqual([message, { data: 'synthetic-frame' }]);
    expect(socket.sent).toEqual(['synthetic-frame']);
    expect(posted.filter((item: any) => item?.type === CAPTURE_MESSAGE)).toHaveLength(1);
  });
});
