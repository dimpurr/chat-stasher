import type { HttpPort, HttpResponse } from '../lib/backfill/engine';
import type { FetchLike } from '../lib/backfill/tab-port';

/** A stable, opaque synthetic identity for tests that exercise unrelated ChatGPT behavior. */
export const CHATGPT_TEST_ACCOUNT_IDENTITY = {
  value: 'f'.repeat(64),
  saltId: 'w303-synthetic-test-salt',
  source: 'request-header-chatgpt-account-id',
} as const;

/**
 * Add the request-local identity an ordinary fixture account would send on every
 * ChatGPT response. Identity-specific tests provide their own value (or explicit
 * null) and do not use this adapter.
 */
export function withChatGptLeaseIdentity(http: HttpPort): HttpPort {
  const wrapped: HttpPort = async (url, init): Promise<HttpResponse> => {
    const response = init === undefined ? await http(url) : await http(url, init);
    if (response.chatgptAccountIdentity != null) return response;
    return { ...response, chatgptAccountIdentity: CHATGPT_TEST_ACCOUNT_IDENTITY };
  };
  Object.assign(wrapped, http);
  return wrapped;
}

/** Add the synthetic raw header the real page-side fetch attaches to ChatGPT requests. */
export function withChatGptFetchIdentity(fetchImpl: FetchLike): FetchLike {
  return async (url, init) => ({
    ...await fetchImpl(url, init),
    chatgptAccountIdHeader: 'acct-w303-synthetic-test',
  });
}
