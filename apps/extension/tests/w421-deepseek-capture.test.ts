import { afterEach, describe, expect, it, vi } from 'vitest';
import { readFileSync } from 'node:fs';
import { CAPTURE_MESSAGE } from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

const DEEPSEEK_HISTORY_URL =
  'https://chat.deepseek.com/api/v0/chat/history_messages?chat_session_id=9f8e7d6c-5b4a-4938-8271-0a1b2c3d4e5f';
const HISTORY_FIXTURE = readFileSync(new URL('../e2e/fixtures/deepseek-history.json', import.meta.url), 'utf8');

function makeFakeXhr() {
  return class FakeXhr {
    status = 200;
    responseType: XMLHttpRequestResponseType | undefined = 'text';
    responseText = '';
    response: unknown = null;
    private listeners: Record<string, Array<() => void>> = {};

    open(_method: string, _url: string): void {}

    addEventListener(name: string, listener: () => void): void {
      (this.listeners[name] ??= []).push(listener);
    }

    send(): void {
      for (const listener of this.listeners.load ?? []) listener();
    }
  };
}

function install() {
  const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
  const posted: any[] = [];
  const fakeWindow: any = {
    location: {
      origin: 'https://chat.deepseek.com',
      href: 'https://chat.deepseek.com/a/chat/s/9f8e7d6c-5b4a-4938-8271-0a1b2c3d4e5f',
    },
    fetch: async () => new Response('{}', { status: 200 }),
    XMLHttpRequest: makeFakeXhr(),
    addEventListener() {},
    postMessage(message: unknown) { posted.push(message); },
  };
  vi.stubGlobal('window', fakeWindow);
  installPageFetchHook(PAGE_HOOK_OPTIONS);
  return {
    warn,
    fakeWindow,
    captures: () => posted.filter((message) => message?.type === CAPTURE_MESSAGE),
  };
}

function xhrWithBody(fakeWindow: any, body: string) {
  const xhr = new fakeWindow.XMLHttpRequest();
  xhr.responseText = body;
  xhr.open('GET', DEEPSEEK_HISTORY_URL);
  xhr.send();
}

describe('W421 · fixture-backed DeepSeek history_messages XHR capture', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('captures the documented synthetic XHR envelope', () => {
    const { warn, fakeWindow, captures } = install();

    xhrWithBody(fakeWindow, HISTORY_FIXTURE);

    expect(captures()).toHaveLength(1);
    expect(captures()[0].payload).toMatchObject({
      url: DEEPSEEK_HISTORY_URL,
      method: 'GET',
      status: 200,
      text: HISTORY_FIXTURE,
    });
    expect(warn).not.toHaveBeenCalled();
  });

  it('rejects a drifted nested message path as an unknown shape, not as an empty capture', () => {
    const { warn, fakeWindow, captures } = install();
    const drifted = JSON.parse(HISTORY_FIXTURE) as {
      data: { biz_data: Record<string, unknown> };
    };
    // The nested response still looks list-like, but neither its renamed messages
    // nor session fields match the documented capture paths or legacy aliases.
    delete drifted.data.biz_data.chat_messages;
    delete drifted.data.biz_data.chat_session;
    drifted.data.biz_data.messages = [];
    drifted.data.biz_data.session = { id: 'synthetic-drifted-session' };

    xhrWithBody(fakeWindow, JSON.stringify(drifted));

    expect(captures()).toHaveLength(0);
    expect(warn).toHaveBeenCalledTimes(1);
    expect(warn).toHaveBeenCalledWith('[chat-stasher] capture skipped: response shape mismatch');
  });
});
