import { afterEach, describe, expect, it, vi } from 'vitest';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

describe('Q2 response-shape visibility', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('warns without logging response data when a candidate shape is rejected', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const listeners: Record<string, Array<(event: any) => void>> = {};
    const fakeWindow: any = {
      location: {
        origin: 'https://chat.deepseek.com',
        href: 'https://chat.deepseek.com/chat/candidate',
      },
      fetch: async () => new Response('{}', { status: 200 }),
      addEventListener(name: string, listener: (event: any) => void) {
        (listeners[name] ??= []).push(listener);
      },
      postMessage() {
        // The warning is deliberately local to the page console.
      },
    };

    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);
    await fakeWindow.fetch('https://chat.deepseek.com/api/v0/chat/session/12345678');
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(warn).toHaveBeenCalledWith(
      '[chat-stasher] capture skipped: response shape mismatch',
    );
    expect(warn.mock.calls[0]).toHaveLength(1);
  });
});
