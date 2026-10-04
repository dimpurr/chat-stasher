import { afterEach, describe, expect, it, vi } from 'vitest';
import { CAPTURE_MESSAGE } from '../lib/contract';
import { installPageFetchHook, PAGE_HOOK_OPTIONS } from '../lib/page-hook';

const JSON_RESPONSE = {
  code: 0,
  data: {
    biz_data: {
      chat_messages: [
        { message_id: 1, role: 'USER', fragments: [{ type: 'REQUEST', content: 'synthetic question' }] },
      ],
    },
  },
};

function makeFakeXhr() {
  return class FakeXhr {
    static readonly OPENED = 1;
    status = 200;
    responseType: XMLHttpRequestResponseType = '';
    response: unknown = null;
    readonly calls: Array<{ name: string; args: unknown[] }> = [];
    private listeners: Record<string, Array<() => void>> = {};

    open(...args: unknown[]): void {
      this.calls.push({ name: 'open', args });
    }

    addEventListener(name: string, listener: () => void): void {
      (this.listeners[name] ??= []).push(listener);
    }

    send(...args: unknown[]): void {
      this.calls.push({ name: 'send', args });
      for (const listener of this.listeners.load ?? []) listener();
    }
  };
}

describe('W510 XHR JSON response capture', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  function install() {
    const posted: any[] = [];
    const fakeWindow: any = {
      location: {
        origin: 'https://chat.deepseek.com',
        href: 'https://chat.deepseek.com/chat/s/synthetic-session',
      },
      fetch: async () => new Response('{}', { status: 200 }),
      XMLHttpRequest: makeFakeXhr(),
      addEventListener() {},
      postMessage(message: unknown) { posted.push(message); },
    };
    vi.stubGlobal('window', fakeWindow);
    installPageFetchHook(PAGE_HOOK_OPTIONS);
    return {
      fakeWindow,
      captures: () => posted.filter((message) => message?.type === CAPTURE_MESSAGE),
    };
  }

  it('serializes a valid JSON response while preserving the response object and request calls', () => {
    const { fakeWindow, captures } = install();
    const xhr = new fakeWindow.XMLHttpRequest();
    const requestBody = 'synthetic request body';
    const response = JSON.parse(JSON.stringify(JSON_RESPONSE));
    const url = 'https://chat.deepseek.com/api/v0/chat/history_messages?chat_session_id=synthetic-session';
    xhr.responseType = 'json';
    xhr.response = response;

    xhr.open('GET', url, true);
    xhr.send(requestBody);

    expect(xhr.calls).toEqual([
      { name: 'open', args: ['GET', url, true] },
      { name: 'send', args: [requestBody] },
    ]);
    expect(xhr.response).toBe(response);
    expect(captures()).toHaveLength(1);
    expect(captures()[0].payload).toMatchObject({
      url,
      method: 'GET',
      status: 200,
      text: JSON.stringify(response),
    });
  });

  it('does not turn a null JSON response from malformed JSON into an empty capture', () => {
    const { fakeWindow, captures } = install();
    const xhr = new fakeWindow.XMLHttpRequest();
    const url = 'https://chat.deepseek.com/api/v0/chat/history_messages?chat_session_id=synthetic-session';
    xhr.responseType = 'json';
    // XMLHttpRequest exposes null when its JSON response cannot be parsed.
    xhr.response = null;

    xhr.open('GET', url);
    xhr.send();

    expect(xhr.calls).toEqual([
      { name: 'open', args: ['GET', url] },
      { name: 'send', args: [] },
    ]);
    expect(xhr.response).toBeNull();
    expect(captures()).toHaveLength(0);
  });
});
