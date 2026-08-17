import { describe, expect, it, vi } from 'vitest';
import {
  CAPTURE_MESSAGE,
  PAGE_HOOK_FETCH_MARKER,
  PAGE_HOOK_VERSION,
} from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

describe('dual-path page hook deduplication', () => {
  it('installs one wrapper when MAIN and fallback both attempt installation', async () => {
    const listeners: Record<string, Array<(event: any) => void>> = {};
    const posted: unknown[] = [];
    let originalCalls = 0;
    const fakeWindow: any = {
      location: { origin: 'https://chat.deepseek.com' },
      fetch: async () => {
        originalCalls += 1;
        return new Response(JSON.stringify({ session_id: 'synthetic-session-id' }), { status: 200 });
      },
      addEventListener(name: string, listener: (event: any) => void) {
        (listeners[name] ??= []).push(listener);
      },
      postMessage(data: unknown, targetOrigin: string) {
        if (targetOrigin !== fakeWindow.location.origin) return;
        posted.push(data);
        for (const listener of listeners.message ?? []) {
          listener({ source: fakeWindow, origin: fakeWindow.location.origin, data });
        }
      },
    };

    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    await fakeWindow.fetch('https://chat.deepseek.com/api/v0/chat/session/synthetic-session-id');
    await new Promise((resolve) => setTimeout(resolve, 0));

    const captures = posted.filter(
      (message: any) => message?.type === CAPTURE_MESSAGE,
    );
    expect(fakeWindow.fetch[PAGE_HOOK_FETCH_MARKER]).toBe(PAGE_HOOK_VERSION);
    expect(originalCalls).toBe(1);
    // The wrapper marker is shared by both paths; the second call returns before
    // adding another wrapper, so one fetch produces one capture message.
    expect(captures).toHaveLength(1);
  });
});
