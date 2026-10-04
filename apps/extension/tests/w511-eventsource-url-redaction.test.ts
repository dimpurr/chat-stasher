import { afterEach, describe, expect, it, vi } from 'vitest';
import { CAPTURE_MESSAGE } from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS, type PageHookOptions } from '../lib/page-hook';

const STREAM_ORIGIN = 'https://stream.example.test';

function eventSourceOptions(): PageHookOptions {
  return {
    ...PAGE_HOOK_OPTIONS,
    platforms: [
      {
        id: 'test-eventsource-platform',
        origins: [STREAM_ORIGIN],
        pathHints: ['/chathub'],
        methods: ['GET'],
        status: { min: 200, max: 299 },
        responseShape: { encoding: 'json', requiredPaths: ['chat_messages'] },
        sessionIdPatterns: [],
        credibility: 'unverified',
        channel: 'experimental',
        eventSourceCapture: true,
      },
    ],
  };
}

describe('W511 · EventSource URL redaction', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('archives a credential-free stream URL while preserving the page constructor URL and event text', () => {
    const posted: unknown[] = [];
    const constructorCalls: string[] = [];
    const fakeWindow: any = {
      location: { origin: STREAM_ORIGIN, href: `${STREAM_ORIGIN}/chat/candidate` },
      fetch: async () => new Response('{}', { status: 200 }),
      EventSource: class FakeEventSource {
        private listeners: Record<string, Array<(event: unknown) => void>> = {};

        constructor(readonly url: string) {
          constructorCalls.push(url);
        }

        addEventListener(name: string, listener: (event: unknown) => void): void {
          (this.listeners[name] ??= []).push(listener);
        }

        emit(name: string, event: unknown): void {
          for (const listener of this.listeners[name] ?? []) listener(event);
        }
      },
      addEventListener() {
        // No page-message handshake is needed for this test.
      },
      postMessage: (message: unknown) => posted.push(message),
    };

    const userInfoSecret = 'w511-userinfo-sentinel';
    const querySecret = 'w511-query-sentinel';
    const fragmentSecret = 'w511-fragment-sentinel';
    const originalUrl = `https://${userInfoSecret}@stream.example.test/chathub?token=${querySecret}#${fragmentSecret}`;
    const eventText = '{ "chat_messages": [] }\n';

    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(eventSourceOptions());

    const source = new fakeWindow.EventSource(originalUrl);
    source.emit('message', { data: eventText });

    expect(constructorCalls).toEqual([originalUrl]);
    expect(source.url).toBe(originalUrl);

    const captures = posted.filter((message: any) => message?.type === CAPTURE_MESSAGE);
    expect(captures).toHaveLength(1);
    expect((captures[0] as any).payload).toMatchObject({
      url: `${STREAM_ORIGIN}/chathub`,
      method: 'GET',
      status: 200,
      text: eventText,
    });
    expect(JSON.stringify(captures)).not.toContain(userInfoSecret);
    expect(JSON.stringify(captures)).not.toContain(querySecret);
    expect(JSON.stringify(captures)).not.toContain(fragmentSecret);
  });
});
