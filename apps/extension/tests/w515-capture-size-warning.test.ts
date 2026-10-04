import { afterEach, describe, expect, it, vi } from 'vitest';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';
import { CAPTURE_MESSAGE, MAX_RAW_BYTES } from '../lib/contract';

describe('W515 oversized capture warning privacy', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('skips an oversized valid candidate and warns with fixed metadata only', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const posted: Array<Record<string, unknown>> = [];
    const pathMarker = 'synthetic-path-id-w515';
    const queryMarker = 'synthetic-query-value-w515';
    const bodyMarker = 'synthetic-body-value-w515';
    const url = `https://chatgpt.com/backend-api/conversation/${pathMarker}?opaque=${queryMarker}`;
    const pageUrl = `https://chatgpt.com/c/${pathMarker}?page=${queryMarker}`;
    // Keep this body in memory: its required ChatGPT shape is valid, and the
    // synthetic padding alone takes its UTF-8 size past the production limit.
    const body = JSON.stringify({
      mapping: { node: {} },
      current_node: 'node',
      syntheticPadding: `${bodyMarker}${'x'.repeat(MAX_RAW_BYTES + 1)}`,
    });
    expect(new TextEncoder().encode(body).byteLength).toBeGreaterThan(MAX_RAW_BYTES);

    const fakeWindow: any = {
      location: { origin: 'https://chatgpt.com', href: pageUrl },
      fetch: async () => new Response(body, { status: 200 }),
      addEventListener() { /* No page-message handshake is needed here. */ },
      postMessage(message: Record<string, unknown>) { posted.push(message); },
    };
    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);

    await fakeWindow.fetch(url);
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(posted.filter((message) => message.type === CAPTURE_MESSAGE)).toHaveLength(0);
    expect(warn).toHaveBeenCalledTimes(1);
    expect(warn.mock.calls[0]).toEqual([
      '[chat-stasher] capture skipped: response exceeds the size cap',
    ]);
    expect(JSON.stringify(warn.mock.calls[0])).not.toContain(pathMarker);
    expect(JSON.stringify(warn.mock.calls[0])).not.toContain(queryMarker);
    expect(JSON.stringify(warn.mock.calls[0])).not.toContain(bodyMarker);
    expect(JSON.stringify(warn.mock.calls[0])).not.toContain(url);
    expect(JSON.stringify(warn.mock.calls[0])).not.toContain(pageUrl);
  });
});
