import { afterEach, describe, expect, it, vi } from 'vitest';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';
import { CAPTURE_MESSAGE } from '../lib/contract';

const ORIGIN = 'https://stream.example.test';
const PAYLOAD_SECRET = 'synthetic-hostile-payload-secret';

class FakeStream {
  readonly listeners: Record<string, Array<(event: unknown) => void>> = {};

  constructor(readonly url: string) {}

  addEventListener(name: string, listener: (event: unknown) => void): void {
    (this.listeners[name] ??= []).push(listener);
  }

  emit(name: string, event: unknown): void {
    for (const listener of this.listeners[name] ?? []) listener(event);
  }
}

function install() {
  const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
  const posted: unknown[] = [];
  const fakeWindow: any = {
    location: { origin: ORIGIN, href: `${ORIGIN}/chat/candidate` },
    fetch: async () => new Response('{}', { status: 200 }),
    EventSource: FakeStream,
    WebSocket: FakeStream,
    addEventListener() {},
    postMessage(message: unknown) { posted.push(message); },
  };
  vi.stubGlobal('window', fakeWindow);
  installPageFetchHook({
    ...PAGE_HOOK_OPTIONS,
    platforms: [{
      id: 'synthetic-stream-platform',
      origins: [ORIGIN],
      pathHints: ['/stream'],
      methods: ['GET'],
      status: { min: 200, max: 299 },
      responseShape: { encoding: 'json', requiredPaths: ['chat_messages'] },
      sessionIdPatterns: [],
      credibility: 'unverified',
      channel: 'experimental',
      eventSourceCapture: true,
      webSocketCapture: true,
    }],
  });
  return {
    fakeWindow,
    warn,
    captures: () => posted.filter((message: any) => message?.type === CAPTURE_MESSAGE),
  };
}

function dataGetter(getter: () => unknown): MessageEvent {
  return Object.defineProperty({}, 'data', { get: getter }) as MessageEvent;
}

describe('W513 · hostile stream message data', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('reads an EventSource data getter once and preserves page delivery', () => {
    const { fakeWindow, captures, warn } = install();
    const source = new fakeWindow.EventSource(`${ORIGIN}/stream?token=${PAYLOAD_SECRET}`);
    const completeSnapshot = JSON.stringify({ chat_messages: ['synthetic'] });
    let reads = 0;
    const event = dataGetter(() => {
      reads += 1;
      if (reads > 1) throw new Error(PAYLOAD_SECRET);
      return completeSnapshot;
    });
    const pageEvents: unknown[] = [];
    source.addEventListener('message', (received: unknown) => pageEvents.push(received));

    expect(() => source.emit('message', event)).not.toThrow();

    expect(reads).toBe(1);
    expect(pageEvents).toEqual([event]);
    expect(captures()).toHaveLength(1);
    expect((captures()[0] as any).payload.text).toBe(completeSnapshot);
    expect(JSON.stringify(warn.mock.calls)).not.toContain(PAYLOAD_SECRET);
  });

  it('ignores throwing, binary, and protocol-fragment frames without changing delivery or logging data', () => {
    const { fakeWindow, captures, warn } = install();
    const socket = new fakeWindow.WebSocket(`wss://stream.example.test/stream?token=${PAYLOAD_SECRET}`);
    const throwingEvent = dataGetter(() => { throw new Error(PAYLOAD_SECRET); });
    const binaryEvent = { data: new Uint8Array([1, 2, 3]).buffer };
    const partialEvent = { data: JSON.stringify({ delta: PAYLOAD_SECRET }) };
    const pageEvents: unknown[] = [];
    socket.addEventListener('message', (received: unknown) => pageEvents.push(received));

    expect(() => {
      socket.emit('message', throwingEvent);
      socket.emit('message', binaryEvent);
      socket.emit('message', partialEvent);
    }).not.toThrow();

    expect(pageEvents).toEqual([throwingEvent, binaryEvent, partialEvent]);
    expect(captures()).toHaveLength(0);
    expect(JSON.stringify(warn.mock.calls)).not.toContain(PAYLOAD_SECRET);
    expect(warn.mock.calls.every((call) => call.length === 1)).toBe(true);
  });

  it('ignores a throwing EventSource data getter while passing the original event to page listeners', () => {
    const { fakeWindow, captures, warn } = install();
    const source = new fakeWindow.EventSource(`${ORIGIN}/stream`);
    const event = dataGetter(() => { throw new Error(PAYLOAD_SECRET); });
    const pageEvents: unknown[] = [];
    source.addEventListener('message', (received: unknown) => pageEvents.push(received));

    expect(() => source.emit('message', event)).not.toThrow();

    expect(pageEvents).toEqual([event]);
    expect(captures()).toHaveLength(0);
    expect(JSON.stringify(warn.mock.calls)).not.toContain(PAYLOAD_SECRET);
  });
});
