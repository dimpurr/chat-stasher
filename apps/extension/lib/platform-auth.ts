/**
 * ChatGPT's conversation-body endpoint needs the session's bearer token.
 *
 * Measured in a logged-in Chrome (2026-09-13): `GET /backend-api/conversation/<id>`
 * returns the full `mapping` with `Authorization: Bearer <accessToken>` and 404
 * with cookies alone; the conversation list works with cookies alone. Every
 * competitor implementation reads the token from the same-origin
 * `/api/auth/session` (see the competitor list, §2.1).
 *
 * Rules for the token, all enforced in this file:
 *  · it lives in this module's memory only — never storage, IndexedDB, logs, or
 *    anything sent to the native host;
 *  · it is attached only to a same-origin request under CHATGPT_DETAIL_PATH on a
 *    ChatGPT origin; every other request is sent exactly as before;
 *  · a 401/403 drops it and fetches a fresh one once.
 */
import { CHATGPT_DETAIL_PATH, CHATGPT_LIST_PATH } from './backfill/enumerate';
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
