import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  installPageFetchHook,
  PAGE_HOOK_OPTIONS,
  type PageHookOptions,
} from '../lib/page-hook';
import { CAPTURE_MESSAGE } from '../lib/contract';

class SyntheticWebSocket {
  static readonly OPEN = 1;
  readonly constructorArgs: unknown[];
  private listeners: Record<string, Array<(event: unknown) => void>> = {};

  constructor(...args: unknown[]) {
    this.constructorArgs = args;
  }

  addEventListener(name: string, listener: (event: unknown) => void): void {
    (this.listeners[name] ??= []).push(listener);
  }

  emit(name: string, event: unknown): void {
    for (const listener of this.listeners[name] ?? []) listener(event);
  }
}

const STREAM_ORIGIN = 'https://stream.synthetic.test';

function streamOptions(): PageHookOptions {
  return {
    ...PAGE_HOOK_OPTIONS,
    platforms: [
      {
        id: 'synthetic-stream-bound',
        origins: [STREAM_ORIGIN],
        pathHints: ['/stream'],
        methods: ['GET'],
        status: { min: 200, max: 299 },
        responseShape: { encoding: 'json', requiredPaths: ['chat_messages'] },
        streamTurnIdPaths: ['turn_id'],
        streamCompletionPaths: ['is_complete'],
        sessionIdPatterns: ['/stream/([0-9a-fA-F-]{8,})'],
        credibility: 'unverified',
        channel: 'experimental',
        webSocketCapture: true,
      },
    ],
  };
}

function syntheticSnapshot(turnId: number, revision = 'first'): string {
  return JSON.stringify({
    turn_id: `turn-${String(turnId).padStart(3, '0')}`,
    is_complete: true,
    chat_messages: [revision],
  });
}

describe('W514 · stream turn deduplication retention bound', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('suppresses completed duplicates and observes an evicted turn after 256 newer turns', () => {
    vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const posted: unknown[] = [];
    const fakeWindow = {
      location: { origin: STREAM_ORIGIN, href: `${STREAM_ORIGIN}/chat/synthetic` },
      fetch: async () => new Response('{}', { status: 200 }),
      WebSocket: SyntheticWebSocket,
      addEventListener() {},
      postMessage: (message: unknown) => posted.push(message),
    };

    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(streamOptions());

    const socket = new fakeWindow.WebSocket('wss://stream.synthetic.test/stream');
    const emit = (data: string): void => socket.emit('message', { data });

    // The first completion is captured once even if the platform sends a
    // changed completed snapshot for that same turn.
    emit(syntheticSnapshot(0));
    emit(syntheticSnapshot(0, 'duplicate'));

    // Inserting 256 newer turns takes the turn cache to capacity and evicts 0.
    for (let turn = 1; turn <= 256; turn += 1) emit(syntheticSnapshot(turn));

    // The newest turn remains retained, so both identical and changed
    // completed snapshots for it are suppressed.
    emit(syntheticSnapshot(256));
    emit(syntheticSnapshot(256, 'late-duplicate'));

    // Turn 0 has left the bounded retention window and is observable again.
    emit(syntheticSnapshot(0, 'after-eviction'));

    const captures = posted.filter((message: any) => message?.type === CAPTURE_MESSAGE);
    const capturedTurns = captures.map((message: any) => JSON.parse(message.payload.text).turn_id);
    expect(capturedTurns).toEqual([
      ...Array.from({ length: 257 }, (_, turn) => `turn-${String(turn).padStart(3, '0')}`),
      'turn-000',
    ]);
    expect(captures).toHaveLength(258);
  });
});
