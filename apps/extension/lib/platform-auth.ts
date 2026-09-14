/**
 * The platforms whose conversation endpoints need a bearer token.
 *
 * ## ChatGPT (measured in a logged-in Chrome, 2026-09-13)
 * `GET /backend-api/conversation/<id>` returns the full `mapping` with
 * `Authorization: Bearer <accessToken>` and 404 with cookies alone; the
 * conversation list works with cookies alone. Every reference implementation
 * reads the token from the same-origin `/api/auth/session` (see the competitor
 * list, §2.1).
 *
 * Rules for the token, all enforced in this file:
 *  · it lives in this module's memory only — never storage, IndexedDB, logs, or
 *    anything sent to the native host;
 *  · it is attached only to a same-origin request under CHATGPT_DETAIL_PATH on a
 *    ChatGPT origin; every other request is sent exactly as before;
 *  · a 401/403 drops it and fetches a fresh one once.
 *
 * ## 🔴 W22 · Kimi (measured in a logged-in Chrome, 2026-09-14)
 * Both of the backfill leg's Kimi paths need it, and a cookie-only request is
 * answered with HTTP 401 rather than an empty list. Kimi's token is not behind an
 * endpoint at all — it is the page origin's own `localStorage.access_token` — so
 * the rules above are the same and the source is different: see
 * `createKimiAuthorizedFetch` below, which carries its own full note on what is
 * read, when, and where it may go.
 */
import {
  CHATGPT_DETAIL_PATH,
  CHATGPT_LIST_PATH,
  KIMI_DETAIL_PATH,
  KIMI_LIST_PATH,
} from './backfill/enumerate';
import { PLATFORMS } from './contract';

export const CHATGPT_SESSION_PATH = '/api/auth/session';

const CHATGPT_ORIGINS: readonly string[] =
  PLATFORMS.find((platform) => platform.id === 'chatgpt')?.origins ?? [];

/**
 * True only for the backfill leg's two ChatGPT endpoints on the page's own
 * origin: the conversation list (exact path) and a conversation body (prefix).
 *
 * The list must carry the token too: measured 2026-09-14, the list endpoint with
 * cookies alone answers 200 with `items: []` and `total: 0` — a well-formed
 * "you have no conversations" that is false. Without the token, backfill would
 * record an unknown as empty.
 */
export function needsChatgptBearer(url: string, pageOrigin: string): boolean {
  try {
    const parsed = new URL(url);
    return parsed.origin === pageOrigin
      && CHATGPT_ORIGINS.includes(parsed.origin)
      && (parsed.pathname === CHATGPT_LIST_PATH || parsed.pathname.startsWith(CHATGPT_DETAIL_PATH));
  } catch {
    return false;
  }
}

export interface MinimalResponse {
  status: number;
  text: () => Promise<string>;
}

export type RawFetch = (url: string, init: RequestInit) => Promise<MinimalResponse>;

/** Reads the token from the session endpoint. null = not logged in or unreadable. */
async function readSessionToken(pageOrigin: string, rawFetch: RawFetch): Promise<string | null> {
  try {
    const res = await rawFetch(`${pageOrigin}${CHATGPT_SESSION_PATH}`, {
      credentials: 'same-origin',
      headers: { accept: 'application/json' },
    });
    if (res.status < 200 || res.status > 299) return null;
    const body: unknown = JSON.parse(await res.text());
    const token = body && typeof body === 'object'
      ? (body as Record<string, unknown>).accessToken
      : undefined;
    return typeof token === 'string' && token.length > 0 ? token : null;
  } catch {
    return null;
  }
}

/**
 * A fetch that adds the bearer token to ChatGPT body requests and leaves every
 * other request untouched. One instance per content script (one page).
 */
export function createAuthorizedFetch(pageOrigin: string, rawFetch: RawFetch) {
  let token: string | null = null;

  const withToken = async (url: string, init: RequestInit): Promise<MinimalResponse> => {
    if (token === null) token = await readSessionToken(pageOrigin, rawFetch);
    // No token: send without it, so the caller sees the platform's real status
    // (404/401) and halts with a trace instead of us inventing an outcome.
    if (token === null) return rawFetch(url, init);
    const headers = { ...(init.headers as Record<string, string> | undefined), authorization: `Bearer ${token}` };
    return rawFetch(url, { ...init, headers });
  };

  return async (url: string, init: RequestInit): Promise<MinimalResponse> => {
    if (!needsChatgptBearer(url, pageOrigin)) return rawFetch(url, init);
    const first = await withToken(url, init);
    if (first.status !== 401 && first.status !== 403) return first;
    token = null;
    return withToken(url, init);
  };
}

// ---------------------------------------------------------------------------
// Kimi: the token lives in the page's own storage, not behind an endpoint.
// ---------------------------------------------------------------------------

/**
 * 🔴 W22 · **Kimi's conversation endpoints need the page's own bearer token.**
 *
 * Measured in a logged-in Chrome (2026-09-14): the page sends
 * `authorization: Bearer <token>`, `x-msh-platform: web`, `x-language: <locale>`
 * and `content-type: application/json`; the token is the JWT this origin keeps in
 * `localStorage` under `access_token`, and a **cookie-only** request to the list
 * endpoint answers HTTP 401 with a body carrying `code` and `details` — a real
 * refusal, not an empty list.
 *
 * That last fact is the whole reason this wrapper exists rather than "just try
 * cookies and see": without the token, the platform refuses, and if the leg ever
 * rounded a refusal into a result, "we are not logged in" would be written down
 * as "you have no conversations". The three rules below are ChatGPT's, applied to
 * a token that comes from somewhere else:
 *  · the token is read from the page's own storage **at request time**, is never
 *    cached, never written to storage, never logged, and never sent to the native
 *    host — it exists as a local for the length of one `fetch`;
 *  · it is attached **only** to Kimi's two backfill paths on a Kimi origin, and to
 *    nothing else — every other request, including every other path on kimi.com,
 *    is sent exactly as it would have been;
 *  · a 401 re-reads it once and retries once. A token the page has refreshed
 *    between two requests is the normal case this covers; anything else is the
 *    platform's answer, and it is passed through rather than interpreted.
 *
 * 🔴 When no token is readable the request is sent **without** one. That is
 *    deliberate: the platform's own 401 is then what the leg sees, and a 401 halts
 *    with a trace instead of being mistaken for an empty account. Inventing a
 *    failure here ("we could not find your token") would hide which of the two
 *    happened.
 *
 * 🔴 What the first two rules buy, stated as the attack they close: nothing on
 *    the page can ask this code for the token. It has no message, no keyword and
 *    no return channel — the value goes into one header of one request the plan
 *    itself built.
 */
export const KIMI_ACCESS_TOKEN_STORAGE_KEY = 'access_token';

/** The two headers the page sends beside the token. Names are lowercase like every other header here. */
export const KIMI_PLATFORM_HEADER = 'x-msh-platform';
export const KIMI_PLATFORM_HEADER_VALUE = 'web';
export const KIMI_LANGUAGE_HEADER = 'x-language';

const KIMI_ORIGINS: readonly string[] =
  PLATFORMS.find((platform) => platform.id === 'kimi')?.origins ?? [];

/**
 * True only for the backfill leg's two Kimi endpoints, on the page's own origin.
 * Both are compared **in full**: the detail id travels in the request body, so
 * neither path is a directory, and a third path on the same origin — including
 * another gateway call of the same service — is not this.
 */
export function needsKimiBearer(url: string, pageOrigin: string): boolean {
  try {
    const parsed = new URL(url);
    return parsed.origin === pageOrigin
      && KIMI_ORIGINS.includes(parsed.origin)
      && (parsed.pathname === KIMI_LIST_PATH || parsed.pathname === KIMI_DETAIL_PATH);
  } catch {
    return false;
  }
}

/**
 * The token as it can actually be used, or null.
 *
 * Not an id alphabet: this only answers "can this value go into an HTTP header at
 * all". A string carrying a control character cannot — `fetch` would throw rather
 * than send it — and throwing would surface as a transport error, i.e. as "the
 * network failed" about a value we could see was unusable. Treating it as "no
 * token" is the honest outcome: the request goes out the way it would have
 * without one, and the platform's own answer is what the user gets.
 */
function usableHeaderToken(value: string | null): string | null {
  if (typeof value !== 'string' || value.length === 0) return null;
  return /[\u0000-\u001F\u007F]/.test(value) ? null : value;
}

export interface KimiAuthOptions {
  /** Reads this origin's `access_token` from the page's own storage. Called per request, never cached. */
  readToken: () => string | null;
  /** The value for `x-language` (the page's `navigator.language`); null/empty ⇒ the header is not sent. */
  language: string | null;
}

/**
 * A fetch that adds Kimi's bearer token and the two headers the page sends to
 * Kimi's two backfill paths, and leaves every other request untouched. One
 * instance per content script (one page).
 */
export function createKimiAuthorizedFetch(
  pageOrigin: string,
  rawFetch: RawFetch,
  options: KimiAuthOptions,
) {
  const send = (url: string, init: RequestInit, token: string | null): Promise<MinimalResponse> => {
    const headers: Record<string, string> = {
      ...(init.headers as Record<string, string> | undefined),
      [KIMI_PLATFORM_HEADER]: KIMI_PLATFORM_HEADER_VALUE,
    };
    // The token is attached **here and nowhere else**, and the header is not even
    // created when there is no token: an `authorization: Bearer ` with an empty
    // value is a different request from one with no authorization header, and the
    // platform has not been asked what it makes of the former.
    if (token !== null) headers.authorization = `Bearer ${token}`;
    if (typeof options.language === 'string' && options.language.length > 0) {
      headers[KIMI_LANGUAGE_HEADER] = options.language;
    }
    return rawFetch(url, { ...init, headers });
  };

  return async (url: string, init: RequestInit): Promise<MinimalResponse> => {
    if (!needsKimiBearer(url, pageOrigin)) return rawFetch(url, init);
    const token = usableHeaderToken(options.readToken());
    const first = await send(url, init, token);
    /**
     * 🔴 Retried **once**, and only in the one case a retry can change the answer:
     *    the platform refused a request that carried a token. The re-read is the
     *    point — the page may have refreshed it between the two requests.
     *
     * 🔴 When there was **no** token, a 401 is returned straight away, with no
     *    second request. There is nothing to re-read and nothing to refresh, so the
     *    retry would be byte-identical; sending it would double this leg's request
     *    count for a user who is simply logged out, all to arrive at the same 401.
     *    That is the "let the platform's real refusal surface" rule, kept honest by
     *    not paying a request for it.
     *
     * Either way the platform's answer is what comes back: a second 401 is
     * returned as-is, the leg halts on it with the status in the trace, and it is
     * never read as "this account has nothing".
     */
    if (first.status !== 401 || token === null) return first;
    return send(url, init, usableHeaderToken(options.readToken()));
  };
}

// ---------------------------------------------------------------------------
// Live leg: "the page just loaded a paged window of this conversation".
// ---------------------------------------------------------------------------

/** Page → ISOLATED message carrying only a conversation id, never a body. */
export const CONVERSATION_SEEN_MESSAGE = '__chat_stasher_conversation_seen__';

/** The paged detail request ChatGPT now uses for in-page navigation. */
export const CHATGPT_PAGED_DETAIL_PATTERN = /^\/backend-api\/conversations\/([0-9a-fA-F-]{8,})$/;

const CONVERSATION_ID = /^[0-9a-fA-F-]{8,64}$/;

export interface ConversationSeen {
  type: string;
  platform: 'chatgpt';
  id: string;
}

export function isConversationSeenMessage(value: unknown): value is ConversationSeen {
  if (!value || typeof value !== 'object') return false;
  const record = value as Record<string, unknown>;
  return record.type === CONVERSATION_SEEN_MESSAGE
    && record.platform === 'chatgpt'
    && typeof record.id === 'string'
    && CONVERSATION_ID.test(record.id);
}

/** The full-conversation URL to fetch for a seen id. */
export function chatgptDetailUrlFor(pageOrigin: string, id: string): string {
  return `${pageOrigin}${CHATGPT_DETAIL_PATH}${encodeURIComponent(id)}`;
}

/** Remembers recently refetched ids so a burst of window requests costs one fetch. */
export function createSeenGate(windowMs: number, now: () => number = Date.now) {
  const last = new Map<string, number>();
  return (id: string): boolean => {
    const t = now();
    const previous = last.get(id);
    if (previous !== undefined && t - previous < windowMs) return false;
    last.set(id, t);
    return true;
  };
}
