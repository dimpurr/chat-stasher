/**
 * W424 · Synthetic fixture probe for Perplexity's live capture boundary.
 * No request leaves this test and no real conversation or account data is used.
 */

import { describe, expect, it, vi } from 'vitest';
import { CAPTURE_MESSAGE } from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

const ORIGIN = 'https://www.perplexity.ai';
const PAGE_URL = `${ORIGIN}/search/synthetic-thread-aaaaaaaa`;
const CONTENT_URL = `${ORIGIN}/rest/thread/synthetic-thread-aaaaaaaa`
  + '?with_parent_info=true&with_schematized_response=true&version=2.18&source=default';
const LIST_URL = `${ORIGIN}/rest/thread/list_ask_threads?version=2.18&source=default`;

/** Established content envelope and entry fields, with invented values only. */
const ESTABLISHED_RESPONSE = JSON.stringify({
  entries: [{
    uuid: 'synthetic-entry-aaaaaaaa',
    query_str: 'synthetic question',
    updated_datetime: '2026-09-01T10:00:00.000Z',
    thread_title: 'synthetic title',
    blocks: [{ intended_usage: 'ask_text', markdown_block: { answer: 'synthetic answer' } }],
  }],
});

type FixtureResponse = { status: number; text: string };

async function probe(request: { url: string; method: string }, response: FixtureResponse) {
  const posted: unknown[] = [];
  const fakeWindow: any = {
    location: { origin: ORIGIN, href: PAGE_URL },
    async fetch() { return new Response(response.text, { status: response.status }); },
    addEventListener() { /* The readiness handshake is outside this capture probe. */ },
    postMessage(message: unknown) { posted.push(message); },
  };
  vi.stubGlobal('window', fakeWindow);
  installPageFetchHook(PAGE_HOOK_OPTIONS);
  try {
    await fakeWindow.fetch(request.url, { method: request.method });
    await new Promise((resolve) => setTimeout(resolve, 0));
    return posted.filter((message: any) => message?.type === CAPTURE_MESSAGE) as Array<{
      type: string;
      payload: { url: string; method: string; status: number; text: string };
    }>;
  } finally {
    vi.unstubAllGlobals();
  }
}

describe('W424 · Perplexity fixture-backed live capture probe', () => {
  it('captures one established content response from its exact GET route', async () => {
    const captures = await probe(
      { url: CONTENT_URL, method: 'GET' },
      { status: 200, text: ESTABLISHED_RESPONSE },
    );

    expect(captures).toHaveLength(1);
    expect(captures[0]?.payload).toMatchObject({
      url: CONTENT_URL,
      method: 'GET',
      status: 200,
      text: ESTABLISHED_RESPONSE,
    });
  });

  it('silently ignores requests outside the content route and method contract', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    try {
      for (const request of [
        { url: LIST_URL, method: 'POST' },
        { url: `${ORIGIN}/rest/thread/mark_viewed`, method: 'POST' },
        { url: `${ORIGIN}/rest/profile/settings`, method: 'GET' },
        { url: 'https://perplexity.ai/rest/thread/synthetic-thread-aaaaaaaa', method: 'GET' },
      ]) {
        expect(await probe(request, { status: 200, text: ESTABLISHED_RESPONSE })).toHaveLength(0);
      }
      expect(warn).not.toHaveBeenCalled();
    } finally {
      warn.mockRestore();
    }
  });

  it('warns and refuses malformed content candidates instead of capturing them', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    const malformed = [
      '{"data":{"entries":[]}}',
      '{"entries":null}',
      '{"entries":"synthetic-not-an-array"}',
      '{"entries":{}}',
      'not-json',
    ];
    try {
      for (const text of malformed) {
        expect(await probe(
          { url: CONTENT_URL, method: 'GET' },
          { status: 200, text },
        )).toHaveLength(0);
      }
      expect(warn).toHaveBeenCalledTimes(malformed.length);
      expect(warn.mock.calls.every(([message]) =>
        message === '[chat-stasher] capture skipped: response shape mismatch')).toBe(true);
      expect(warn.mock.calls.every((call) => call.length === 1)).toBe(true);
    } finally {
      warn.mockRestore();
    }
  });
});
