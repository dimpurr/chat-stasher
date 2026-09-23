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
  DEEPSEEK_DETAIL_PATH,
  DEEPSEEK_LIST_PATH,
  GEMINI_BATCHEXECUTE_PATH,
  GEMINI_FORM_FIELD_AT,
  GEMINI_FORM_FIELD_BATCH,
  GEMINI_PAGE_QUERY_KEYS,
  GEMINI_QUERY_KEY_BL,
  GEMINI_QUERY_KEY_F_SID,
  GEMINI_QUERY_KEY_HL,
  GEMINI_QUERY_KEY_REQID,
  GEMINI_QUERY_RPCIDS,
  KIMI_DETAIL_PATH,
  KIMI_LIST_PATH,
} from './backfill/enumerate';
import { GEMINI_ORIGIN, GEMINI_TOKENS_REQUEST_MESSAGE, type GeminiBootstrapTokens } from './contract';
import { GEMINI_RPC_DETAIL, GEMINI_RPC_LIST } from './gemini-rpc';
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
 * 🔴 W64b · **The same redirect policy DeepSeek's wrapper already has, for the same
 * reason and with the same shape.**
 *
 * `fetch` defaults to `redirect: 'follow'`, and a redirect of a *same-origin* URL
 * keeps the `Authorization` header (only a cross-origin hop drops it). So a 302
 * from Kimi's list or detail path to any other path on `www.kimi.com` would be
 * re-sent **with the page's own bearer token** — to a URL this extension never
 * chose, on the strength of a decision `needsKimiBearer` made about the *first*
 * URL. The allowlist cannot help: it approved the request that was sent, not the
 * one the platform redirected it to. W61b fixed exactly this for DeepSeek and left
 * this wrapper unguarded, which is the defect W64b's review found.
 *
 * So the two paths are fetched with `redirect: 'manual'`. The browser then does not
 * follow, the header is never re-sent, and what comes back is an **opaque**
 * response: `status 0`, no readable body. That is not a response this leg can read a
 * conversation list out of, and it is not a shape either — a redirect is a transport
 * fact, and it is said as one rather than passed on as `HTTP 0` for the engine's
 * status branch to call a wire-shape change (which is permanent, and would stop the
 * leg for good over a redirect that may be a login page today and absent tomorrow).
 *
 * 🔴 The throw is deliberate and its scope is exact: it covers the unfollowed
 *    redirect only (status 0, which with `redirect: 'manual'` is what an
 *    opaqueredirect response is). Every other response — 200, 401, 403, 500 — is
 *    returned untouched, so the 401 re-read-and-retry below is unchanged, and no
 *    other status handling moves.
 */
const KIMI_REDIRECT_MODE = 'manual' as const;

/**
 * The failure an unfollowed redirect becomes. It names the path rather than the URL,
 * and carries no header and no token — a halt detail is persisted and shown to the
 * user.
 */
function kimiRedirectRefusal(url: string): Error {
  return new Error(
    `kimi answered ${new URL(url).pathname} with a redirect; this leg does not follow it,`
    + ' because the page\'s own bearer token is attached to this request and would be re-sent'
    + ' to wherever it points (status 0, body unreadable — the request was not read as a result)',
  );
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
  const send = async (url: string, init: RequestInit, token: string | null): Promise<MinimalResponse> => {
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
    // 🔴 `redirect: 'manual'` is set whether or not a token was found: the request is
    //    built the same way either way, so a trace cannot read as two different
    //    requests depending on whether the user happened to be signed in.
    const res = await rawFetch(url, { ...init, headers, redirect: KIMI_REDIRECT_MODE });
    if (res.status === 0) throw kimiRedirectRefusal(url);
    return res;
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
// DeepSeek: the token lives in the page's own storage, like Kimi's.
// ---------------------------------------------------------------------------

/**
 * 🔴 W61 · **DeepSeek's conversation list and body both need the page's own
 * bearer token.**
 *
 * Measured in a logged-in Chrome (2026-09-23), from the page's own context:
 * `GET /api/v0/chat_session/fetch_page?count=20` and
 * `GET /api/v0/chat/history_messages?chat_session_id=<id>` both answer **HTTP 200**
 * with a cookie-only request — and answer with an **error envelope**:
 * `{code: 40002, data: null, msg: "Missing Token"}` for the list and
 * `{code: 40003, data: null, msg: "INVALID_TOKEN"}` for the body. The same two
 * requests with `authorization: Bearer <userToken.value>` answer `code: 0`,
 * `data.biz_code: 0`, a 20-row `chat_sessions` array (`has_more: true`) and a
 * 6-message `chat_messages` array respectively.
 *
 * 🔴 **The 200 is the whole reason this wrapper exists.** Every other platform in
 *    this file refuses with a status this leg can branch on (ChatGPT 404, Kimi 401,
 *    Gemini 400), so "no auth" is visible at the transport. DeepSeek refuses
 *    **in-band**: there is no status to key off, `data` comes back `null`, and the
 *    parser's own envelope check is what fires — which is how an authentication
 *    refusal was recorded as `halted.reason = "shape-changed"`, detail
 *    "deepseek list response has no `data` object (envelope changed?)". The user
 *    was told the API had changed. It had not.
 *
 * So the second half of W61 is not here but in the parsers: they read `code` and
 * `data.biz_code` and refuse with `auth-refused`, so that even with no wrapper, or
 * with a token the platform rejects, the leg says what happened. This wrapper is
 * what makes that branch rare rather than constant.
 *
 * The three rules are ChatGPT's and Kimi's, unchanged:
 *  · the token is read from the page's own storage **at request time**, is never
 *    cached, never written to storage, never logged, and never sent to the native
 *    host — it exists as a local for the length of one `fetch`;
 *  · it is attached **only** to DeepSeek's two backfill paths, compared **in full**,
 *    on a DeepSeek origin — and to nothing else. Every other request, including
 *    every other path on chat.deepseek.com, is sent exactly as it would have been;
 *  · a **401 or 403** re-reads it once and retries once. 🔴 This is the honest
 *    limit of that rule on this platform and it is stated rather than implied: the
 *    refusal DeepSeek actually sends is a 200, so **the retry does not cover it**.
 *    It does not need to. The token is re-read per request, so the freshest value
 *    the page holds is already in the header, and a retry would send byte-identical
 *    bytes. A token the page rotated between two requests — the case Kimi's retry
 *    covers — is already covered here by the per-request read. The 401/403 branch
 *    is kept because it is the status family that *can* appear, and passing a
 *    refusal through uninterpreted is the rule, not the retry.
 *
 * 🔴 When no token is readable the request is sent **without** one, exactly as
 *    Kimi's is: the platform's own refusal is then what the leg sees, and it now
 *    halts as `auth-refused` with the code and message in the trace, instead of
 *    being mistaken for an empty account.
 *
 * 🔴 What the first two rules buy, stated as the attack they close: nothing on the
 *    page can ask this code for the token. It has no message, no keyword and no
 *    return channel — the value goes into one header of one request the plan itself
 *    built.
 */
export const DEEPSEEK_USER_TOKEN_STORAGE_KEY = 'userToken';

const DEEPSEEK_ORIGINS: readonly string[] =
  PLATFORMS.find((platform) => platform.id === 'deepseek')?.origins ?? [];

/**
 * True only for the backfill leg's two DeepSeek endpoints, on the page's own
 * origin. Both are compared **in full**: DeepSeek's detail id travels in the
 * query, not the path, so neither route is a directory — see
 * `DEEPSEEK_DETAIL_PATH`'s own note. A third path on the same origin is not this.
 */
export function needsDeepSeekBearer(url: string, pageOrigin: string): boolean {
  try {
    const parsed = new URL(url);
    return parsed.origin === pageOrigin
      && DEEPSEEK_ORIGINS.includes(parsed.origin)
      && (parsed.pathname === DEEPSEEK_LIST_PATH || parsed.pathname === DEEPSEEK_DETAIL_PATH);
  } catch {
    return false;
  }
}

/**
 * The page's stored `userToken` as a value this code can put in a header, or null.
 *
 * 🔴 It is **not** the stored string. Measured 2026-09-23, DeepSeek's `userToken`
 *    is a JSON object whose usable token is its `value` member — the same shape two
 *    of the W57 references read (`localStorage.userToken` → `.value`), and the
 *    shape `sisodiabhumca` reads *without* the `.value`, which is why that one
 *    could not be copied verbatim. So the unwrapping is written out here rather
 *    than left to a caller: the JSON is data from a page, and the one thing it may
 *    produce is a string or nothing.
 *
 * `null` covers all four of "no key", "unreadable storage" (the caller catches the
 * throw), "not JSON and not a token", and "a value that cannot go in a header at
 * all" — and none of them is turned into an empty result: the request then goes out
 * with no authorization header and the platform's own refusal is what the leg
 * records. A raw non-JSON string is passed through, because a platform that stops
 * wrapping its token in JSON must not thereby lose it.
 *
 * 🔴 The three characters below are gates on the **parse**, not tests of content:
 *    a value that starts like JSON must finish as JSON, and a value that parses to
 *    something that is not the container is not a token. Both halves matter —
 *    `'[]'` is valid JSON and is not the token, while `'{"value":'` is neither
 *    valid JSON nor a usable token, and reading either as a raw token would put a
 *    string no page ever sent into an HTTP header.
 *
 * 🔴 W61b · **The gates read a *trimmed* value, and that is a fix, not a tidy-up.**
 *    A stored ` {"value":"<jwt>"}` — one leading space, or a BOM — did not start
 *    with `{`, so the JSON gate was skipped and the whole string went into the
 *    header as the bearer token: `Authorization: Bearer  {"value":"<jwt>"}`. That
 *    is a request nobody made and a token nobody has, sent to the platform's own
 *    endpoint — a login failure invented by this code, and one the user would read
 *    as the platform refusing. So the value is trimmed once, here, before any of it
 *    is read: leading and trailing whitespace is storage's, not the token's, and
 *    JSON allows it inside the document anyway. 🔴 Trimming decides where the parse
 *    **starts**, never whether it succeeds: `' {"value":'` still fails the gate and
 *    still returns `null`. The token that survives is trimmed for the same reason —
 *    HTTP strips leading and trailing whitespace from a field value, so a space
 *    kept here would be the one thing sent that the value it came from does not say.
 */
export function readDeepSeekUserToken(raw: string | null): string | null {
  if (typeof raw !== 'string') return null;
  const trimmed = raw.trim();
  if (trimmed.length === 0) return null;
  let value: unknown = trimmed;
  if (trimmed[0] === '{' || trimmed[0] === '[' || trimmed[0] === '"') {
    try {
      value = JSON.parse(trimmed);
    } catch {
      return null;
    }
  }
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    value = (value as Record<string, unknown>).value;
  }
  return usableHeaderToken(typeof value === 'string' ? value : null);
}

export interface DeepSeekAuthOptions {
  /** Reads this origin's `userToken` from the page's own storage. Called per request, never cached. */
  readToken: () => string | null;
}

/**
 * 🔴 W61b · **The one redirect policy a request carrying the page's own login token
 * may have.**
 *
 * `fetch` defaults to `redirect: 'follow'`, and a redirect of a *same-origin* URL
 * keeps the `Authorization` header (only a cross-origin hop drops it). So a 302
 * from `/api/v0/chat_session/fetch_page` to any other path on
 * `chat.deepseek.com` would be re-sent **with the page's own bearer token** — to a
 * URL this extension never chose, on the strength of a decision `needsDeepSeekBearer`
 * made about the *first* URL. The allowlist cannot help: it approved the request
 * that was sent, not the one the platform redirected it to.
 *
 * So the two paths are fetched with `redirect: 'manual'`. The browser then does not
 * follow, the header is never re-sent, and what comes back is an **opaque**
 * response: `status 0`, no readable body. That is not a response this leg can read
 * a conversation list out of, and it is not a shape either — a redirect is a
 * transport fact, and it is said as one rather than passed on as `HTTP 0` for the
 * engine's status branch to call a wire-shape change (which is permanent, and
 * would stop the leg for good over a redirect that may be a login page today and
 * absent tomorrow).
 *
 * 🔴 The throw is deliberate and its scope is exact: it covers the unfollowed
 *    redirect only (status 0, which with `redirect: 'manual'` is what an
 *    opaqueredirect response is). Every other response — 200, 401, 403, 500 — is
 *    returned untouched, so no existing status handling moves.
 */
const DEEPSEEK_REDIRECT_MODE = 'manual' as const;

/**
 * The failure an unfollowed redirect becomes. It names the path rather than the
 * URL — the query carries a conversation id on the body path, and a halt detail
 * never carries one.
 */
function deepSeekRedirectRefusal(url: string): Error {
  return new Error(
    `deepseek answered ${new URL(url).pathname} with a redirect; this leg does not follow it,`
    + ' because the page\'s own bearer token is attached to this request and would be re-sent'
    + ' to wherever it points (status 0, body unreadable — the request was not read as a result)',
  );
}

/**
 * A fetch that adds DeepSeek's bearer token to DeepSeek's two backfill paths and
 * leaves every other request untouched. One instance per content script (one page).
 */
export function createDeepSeekAuthorizedFetch(
  pageOrigin: string,
  rawFetch: RawFetch,
  options: DeepSeekAuthOptions,
) {
  const send = async (url: string, init: RequestInit, token: string | null): Promise<MinimalResponse> => {
    // 🔴 `redirect: 'manual'` is set whether or not a token was found: the request
    //    is built the same way either way, so a trace cannot read as two different
    //    requests depending on whether the user happened to be logged in.
    const guarded: RequestInit = { ...init, redirect: DEEPSEEK_REDIRECT_MODE };
    const res = token === null
      ? await rawFetch(url, guarded)
      : await rawFetch(url, {
        ...guarded,
        headers: { ...(init.headers as Record<string, string> | undefined), authorization: `Bearer ${token}` },
      });
    if (res.status === 0) throw deepSeekRedirectRefusal(url);
    return res;
  };

  return async (url: string, init: RequestInit): Promise<MinimalResponse> => {
    if (!needsDeepSeekBearer(url, pageOrigin)) return rawFetch(url, init);
    const token = options.readToken();
    const first = await send(url, init, token);
    // 🔴 Retried once, and only when the first attempt carried a token — see this
    //    section's header for why the refusal this platform actually sends (a 200)
    //    is deliberately not what this branch keys on.
    if ((first.status !== 401 && first.status !== 403) || token === null) return first;
    return send(url, init, options.readToken());
  };
}

// ---------------------------------------------------------------------------
// Gemini: the tokens are the page's own globals, and they travel in the request
// itself — one in the URL, one in the body.
// ---------------------------------------------------------------------------

/**
 * 🔴 W29 · **Gemini's `batchexecute` calls need three values that exist only in
 * the page.**
 *
 * Measured in a logged-in Chrome (2026-09-14): the page posts to
 * `/_/BardChatUi/data/batchexecute?rpcids=…&source-path=…&bl=…&f.sid=…&hl=…&_reqid=…&rt=c`
 * with a form body `f.req=…&at=…`, where `at`, `bl` and `f.sid` come from the
 * page's own `WIZ_global_data` blob. **Without `at` the server answers HTTP 400**
 * with a structured error entry — a real refusal, never data, never an empty
 * page. That is the whole reason this wrapper exists rather than "just try
 * cookies and see": if the leg ever rounded a refusal into a result, "we are not
 * logged in" would be written down as "you have no conversations".
 *
 * The three rules are ChatGPT's and Kimi's, applied to tokens that come from a
 * page global instead of an endpoint or a storage key:
 *  · the values are **read at request time** through the MAIN-world hook (a
 *    content script cannot see a page global at all — see
 *    `GEMINI_TOKENS_REQUEST_MESSAGE` in lib/contract.ts), are never cached here,
 *    never written to storage, never logged, and never sent to the native host;
 *  · they are attached **only** to the two `batchexecute` rpcids on a Gemini
 *    origin, and to nothing else — every other request, including every other
 *    path on gemini.google.com, is sent exactly as it would have been;
 *  · a **400 or 401** re-reads them once and retries once. 400 is the measured
 *    shape of "the token was missing or stale" on this platform, 401 is the
 *    family's; a token the page rotated between two requests is the normal case
 *    this covers, and anything else is the platform's answer, passed through
 *    rather than interpreted.
 *
 * 🔴 When no token is readable the request is sent **with an empty `at`**. That is
 *    deliberate and it is the same call Kimi's wrapper makes: the platform's own
 *    400 is then what the leg sees, and a 400 halts with a trace instead of being
 *    mistaken for an empty account. Inventing a failure here ("we could not find
 *    your token") would hide which of the two happened.
 *
 * 🔴 The query and the body are **rebuilt**, not patched: the four page keys are
 *    removed and set from what was just read, and `f.req=<batch>&at=<token>` is
 *    written from the batch the plan built (which the allowlist has already
 *    checked) plus the token. So this wrapper cannot widen a request — it can
 *    only fill in the three values it exists for, and it is the only code that
 *    ever puts a credential into one.
 */
export const GEMINI_FORM_CONTENT_TYPE = 'application/x-www-form-urlencoded';

const GEMINI_ORIGINS: readonly string[] =
  PLATFORMS.find((platform) => platform.id === 'gemini')?.origins ?? [GEMINI_ORIGIN];

/** True only for the two `batchexecute` rpcids this leg's plans declare, on the page's own origin. */
export function needsGeminiTokens(url: string, pageOrigin: string): boolean {
  try {
    const parsed = new URL(url);
    if (parsed.origin !== pageOrigin || !GEMINI_ORIGINS.includes(parsed.origin)) return false;
    if (parsed.pathname !== GEMINI_BATCHEXECUTE_PATH) return false;
    const rpcid = parsed.searchParams.get(GEMINI_QUERY_RPCIDS);
    return rpcid === GEMINI_RPC_LIST || rpcid === GEMINI_RPC_DETAIL;
  } catch {
    return false;
  }
}

/**
 * 🔴 How long the page-world pull may take before the request goes out without
 * tokens.
 *
 * The pull is one `postMessage` in and one back, answered by a listener in the
 * same tab — microseconds. The budget is not a performance figure: it exists so
 * that a page where the MAIN-world hook never installed (a CSP refusal, a frozen
 * global) costs **one bounded wait per request** instead of stalling the leg. On
 * that path there is no reply at all, the request goes out with an empty `at`,
 * and the server's 400 is what the user's trace says — which is the honest
 * outcome, not a hang.
 */
export const GEMINI_TOKEN_PULL_TIMEOUT_MS = 200;

export interface GeminiAuthOptions {
  /**
   * Reads the page's bootstrap tokens through the page-world pull. Called per
   * request, never cached — and **asynchronous**, because the values live in
   * another JS context and have to be asked for (see
   * `GEMINI_TOKENS_REQUEST_MESSAGE`). It resolves to null when the page world
   * did not answer inside `GEMINI_TOKEN_PULL_TIMEOUT_MS`.
   */
  readTokens: () => Promise<GeminiBootstrapTokens | null>;
  /** The value for `hl` (the page's `navigator.language`); null/empty ⇒ the parameter is not sent. */
  language: string | null;
}

/**
 * A fetch that fills Gemini's three page tokens into `batchexecute` requests and
 * leaves every other request untouched. One instance per content script.
 */
export function createGeminiAuthorizedFetch(
  pageOrigin: string,
  rawFetch: RawFetch,
  options: GeminiAuthOptions,
) {
  /**
   * The page's own request counter. 🔴 Its value carries **no meaning this code
   * relies on** — the page increments one per request and the server has not been
   * asked what it makes of it — so it is a plain local counter, not a clock, not
   * an identifier, and it is not derived from anything about the user.
   */
  let requestCounter = 0;

  const send = (url: string, init: RequestInit, tokens: GeminiBootstrapTokens | null): Promise<MinimalResponse> => {
    const parsed = new URL(url);
    // 🔴 Removed first, then set from what was just read. A key this wrapper does
    //    not have a value for is therefore absent rather than stale — and the
    //    allowlist has already refused anything the plan did not declare, so this
    //    is normalisation, not a check.
    for (const key of GEMINI_PAGE_QUERY_KEYS) parsed.searchParams.delete(key);
    const bl = usableHeaderToken(tokens?.bl ?? null);
    if (bl !== null) parsed.searchParams.set(GEMINI_QUERY_KEY_BL, bl);
    const fSid = usableHeaderToken(tokens?.fSid ?? null);
    if (fSid !== null) parsed.searchParams.set(GEMINI_QUERY_KEY_F_SID, fSid);
    const language = usableHeaderToken(options.language);
    if (language !== null) parsed.searchParams.set(GEMINI_QUERY_KEY_HL, language);
    parsed.searchParams.set(GEMINI_QUERY_KEY_REQID, String(requestCounter++));

    // 🔴 The batch is taken **out of the body the plan built** and re-emitted with
    //    the token; it is never re-encoded from anything else, and the token field
    //    is blank when there is no token (see this section's header).
    const fields = new URLSearchParams(typeof init.body === 'string' ? init.body : '');
    const batch = fields.get(GEMINI_FORM_FIELD_BATCH) ?? '';
    const at = usableHeaderToken(tokens?.at ?? null) ?? '';
    const body = `${GEMINI_FORM_FIELD_BATCH}=${encodeURIComponent(batch)}`
      + `&${GEMINI_FORM_FIELD_AT}=${encodeURIComponent(at)}`;

    return rawFetch(parsed.toString(), {
      ...init,
      method: 'POST',
      headers: { 'content-type': GEMINI_FORM_CONTENT_TYPE },
      body,
    });
  };

  return async (url: string, init: RequestInit): Promise<MinimalResponse> => {
    if (!needsGeminiTokens(url, pageOrigin)) return rawFetch(url, init);
    const tokens = await options.readTokens();
    const first = await send(url, init, tokens);
    /**
     * 🔴 Retried **once**, and only when the first attempt carried a token: a
     *    refusal of a request that *had* one is the case a re-read can change (the
     *    page may have rotated the value). With no token there is nothing to
     *    re-read and nothing to refresh, so the retry would be byte-identical
     *    except for the request counter — sending it would double this leg's
     *    request count for a user who is simply logged out, to reach the same 400.
     */
    if ((first.status !== 400 && first.status !== 401) || usableHeaderToken(tokens?.at ?? null) === null) {
      return first;
    }
    return send(url, init, await options.readTokens());
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
