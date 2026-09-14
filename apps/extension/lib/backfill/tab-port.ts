/**
 * C19 · The backfill leg's **fetch channel** — the answer to the second gate.
 *
 * ## The problem
 * C13 wrote the http port as "somebody has to inject it explicitly", and then
 * **no production code injected it**. So the backfill leg always stopped at
 * 'no-http-port' in a browser (measured in C17-4).
 * To wire it up, one real question has to be answered: **who sends this request?**
 *
 * ## Why we cannot just fetch from background
 * In an MV3 service worker, `fetch('https://chatgpt.com/backend-api/...')` is a
 * **cross-origin** request: the SW's own origin is chrome-extension://<id>. For it
 * to carry the user's chatgpt.com cookies, the manifest would have to declare
 * `host_permissions: ["https://chatgpt.com/*", ...]` — a **new permission**, and
 * one of the kind that shows "Read and change your data on xxx" at install time.
 * 🔴 This task's hard constraint: no new host permissions. So that road is closed.
 *
 * ## The road that works: let the content script that is already there fetch it
 * The content script is already injected into these platforms' pages per
 * CONTENT_MATCHES (lib/contract.ts:292). A **same-origin** fetch made in that
 * context uses the credentials of the user's own page — same origin, same cookies,
 * same UA as the request the browser sends when the user opens a past conversation
 * by hand.
 * That satisfies the architectural premise "fetching must happen inside the user's
 * logged-in browser context", and it **needs no new permission**: a content
 * script's same-origin requests to its own page are not governed by host
 * permissions in the first place, and `matches` need not change a character.
 *
 * ## Hence this leg's boundary (written honestly here, and the popup says the same)
 * 🔴 **A tab of that platform has to be open** for backfill to fetch anything.
 *    With no open page there is no usable port ⇒ tickBackfill still returns
 *    'no-http-port' faithfully.
 *    A user who "installs it and never opens that site again" really will not
 *    finish backfilling — that is a direct corollary of the architectural premise,
 *    not a bug, and it must not be papered over with a fake status.
 *
 * ## Self-imposed limits (content-script side; see checkBackfillRequest /
 * ## serveBackfillFetch)
 *  1. **Same origin only**: the request URL's origin must equal the page's own
 *     origin, character for character.
 *  2. **Only origins in the platform table.**
 *  3. **Only platforms that have written a plan of their own** (C22).
 *  4. **Only the backfill leg's own two paths** (conversation list / conversation body).
 *  --- 🔴 C23 added three more: the channel can send POST now, so the allowlist
 *  --- has to grow the method/body dimensions with it
 *  5. the method must be in the **closed set** ALLOWED_BACKFILL_METHODS **and** must
 *     equal, character for character, the one the plan declared for this segment
 *     (expectedMethodFor). A POST on a segment the plan says is GET ⇒ refused, and
 *     vice versa. **The request does not get a say.**
 *  6. a GET segment **may not carry a body or a Content-Type at all**;
 *  7. a POST segment's body must be: a string, ≤ MAX_REQUEST_BODY_BYTES, parse as a
 *     **plain object** under JSON.parse, have top-level keys that are a **subset**
 *     of the plan's declared bodyKeys, hold only string/number/boolean/null values
 *     (no nested objects or arrays), and its Content-Type must equal the plan's
 *     exactly.
 *  --- 🔴 W8 added one more, because a plan put its conversation id in the query
 *  --- for the first time
 *  8. a body URL's **query** is checked against the plan's declaration, and against
 *     the plan's own URL builder: see checkDetailQuery. A plan that declares no
 *     query key permits no query at all.
 *  --- 🔴 W21 added two more, for a plan that declares a second detail step
 *  9. a plan may name a **third** path (the second detail step). It is admitted
 *     only when the plan declares it, it is compared with the same
 *     prefix/exact/template rule as the first, and it is always a POST whose
 *     method is not even a parameter the request can choose.
 * 10. one key of a POST body may be declared as an **array of non-empty strings**
 *     (`bodyArrayKeys`), bounded by MAX_BODY_ARRAY_ITEMS and refused for anything
 *     nested, empty, non-string or on an undeclared key.
 * Failing any one of these refuses the request with an error; never send it anyway.
 *
 * ## 🔴 Why this is still not a "general-purpose proxy"
 * See the note above checkBackfillRequest: the only things left variable are the
 * **raw scalar value** of a key **inside one closed set** (a POST body's) and, since
 * W8, the conversation id inside the one query key a plan declared — and that id is
 * itself pinned to the value the plan's own URL builder round-trips. The origin, the
 * path, the method, the structure and the size are all fixed.
 */

import { getPlatformByOrigin, MAX_RAW_BYTES } from '../contract';
import {
  backfillPlanFor,
  detailPathMatches,
  expectedMethodFor,
  postSpecFor,
  ALLOWED_BACKFILL_CONTENT_TYPES,
  ALLOWED_BACKFILL_METHODS,
  MAX_BODY_ARRAY_ITEMS,
  MAX_REQUEST_BODY_BYTES,
  type BackfillEnumPlan,
  type BackfillMethod,
  type BackfillRequestInit,
  type BackfillSegment,
} from './enumerate';
import type { HttpPort, HttpResponse } from './engine';
import type { BackfillStore } from './store';

/** background → content script: fetch this URL for me. */
export const BACKFILL_FETCH_MESSAGE = 'cs-backfill-fetch';
/** background → content script: are you still alive (used to answer the popup's transportWired faithfully). */
export const BACKFILL_PING_MESSAGE = 'cs-backfill-ping';
/** content script → background: I am alive on a platform page; the tab id comes with the sender. */
export const BACKFILL_TAB_HELLO_MESSAGE = 'cs-backfill-tab-hello';

/** The registry of live platform tabs. Same cs_* prefix family; no new permission. */
export const BACKFILL_TABS_KEY = 'cs_backfill_tabs_v1';
/** How many rows the registry keeps. Enough to cover "a few platforms open at once", without growing without bound. */
export const MAX_TAB_ENTRIES = 12;

/**
 * 🔴 W13 · **How many consecutive unanswered pings it takes to strike a tab off.**
 *
 * Why this constant exists at all: `pickLiveTab` used to forget a tab after **one**
 * failed ping, and the only thing that ever puts a tab back is the content script's
 * hello on page load. So a page whose renderer was busy for a few seconds — the
 * normal case, not a fault — vanished from the registry, and the backfill then had
 * no fetch channel at all until the user reloaded the page. Measured on a real
 * profile: the registry was left as an empty `[]`.
 *
 * Why 2, and not 3 or more:
 *  · the alarm pings at most once per 5 minutes (`BACKFILL_TICK_DELAY_MIN_MINUTES`), so two
 *    consecutive misses already means the tab has been silent across a whole tick —
 *    far longer than any renderer stall;
 *  · the cost of being wrong in each direction is not symmetric. Forgetting a live
 *    tab costs the entire fetch channel (the failure this fixes); keeping a dead one
 *    costs exactly one extra cheap `runtime.sendMessage` per tick, and the browser
 *    answers a dead tab id's ping with a rejection immediately.
 * So the threshold sits at the lowest value that clears a full tick.
 *
 * 🔴 Why a **consecutive-miss counter** and not an age/expiry rule: a tab that is
 *    answering must never expire. Time tells us nothing about liveness here — only
 *    evidence of two silences does, and a successful ping resets the count to zero.
 */
export const TAB_PING_MISSES_BEFORE_FORGET = 2;

export interface TabEntry {
  tabId: number;
  origin: string;
  at: number;
  /**
   * 🔴 W13 · How many consecutive pings this tab has failed to answer. Absent ⇒ 0,
   * which is what every entry written before W13 reads back as (and what the content
   * script's hello still writes) — so `entrypoints/background.ts` is untouched.
   * Cleared to 0 by a successful ping; the entry is struck off at
   * `TAB_PING_MISSES_BEFORE_FORGET`.
   */
  misses?: number;
}

export type BackfillFetchReply =
  | { ok: true; status: number; text: string }
  | { ok: false; error: string };

function isRecord(v: unknown): v is Record<string, unknown> {
  return !!v && typeof v === 'object';
}

/**
 * 🔴 C23: a message can now carry three **optional** fields, method/body/contentType.
 *    This does only the shallowest "is the shape recognisable" test; **the real
 *    admission check is in checkBackfillRequest**.
 *    With none of the three present the message is byte-identical to C22's ⇒ the old
 *    GET route.
 */
export function isBackfillFetchRequest(
  v: unknown,
): v is { type: string; url: string; method?: unknown; body?: unknown; contentType?: unknown } {
  return isRecord(v) && v.type === BACKFILL_FETCH_MESSAGE && typeof v.url === 'string';
}

/** Read a message as a request description. Any non-string field counts as "not given" and is left to the allowlist to refuse. */
function specFromMessage(m: { url: string; method?: unknown; body?: unknown; contentType?: unknown }): BackfillRequestSpec {
  return {
    url: m.url,
    method: typeof m.method === 'string' ? m.method : undefined,
    body: typeof m.body === 'string' ? m.body : undefined,
    contentType: typeof m.contentType === 'string' ? m.contentType : undefined,
  };
}

export function isBackfillPing(v: unknown): v is { type: string } {
  return isRecord(v) && v.type === BACKFILL_PING_MESSAGE;
}

export function isTabHello(v: unknown): v is { type: string; origin: string } {
  return isRecord(v) && v.type === BACKFILL_TAB_HELLO_MESSAGE && typeof v.origin === 'string';
}

/** The complete description of one sent-on-behalf request. Omitted method ⇒ 'GET' (byte-compatible with C22's message shape). */
export interface BackfillRequestSpec {
  url: string;
  method?: string;
  body?: string;
  contentType?: string;
}

/**
 * 🔴 The refusal reason that existed as of C22, **kept character for character**.
 * It is the wire value returned to background (tests/c19-runit.test.ts:433 asserts
 * it), so all four of the URL dimension's checks reuse it — back-compat is not
 * discounted.
 * Finer classification only reaches the log (verdict.detail) and does not change
 * the wire.
 */
export const REFUSED_URL_REASON = 'refused: url is not a same-origin backfill endpoint';

export type RequestVerdict =
  | { ok: true; url: string; method: BackfillMethod; body?: string; contentType?: string }
  /** reason = what goes back to the other side (the wire); detail = the finer classification, log only. */
  | { ok: false; reason: string; detail: string };

/** A refusal on the URL dimension: the wire value is always C22's sentence; the finer classification is log only. */
function refuseUrl(detail: string): { ok: false; reason: string; detail: string } {
  return { ok: false, reason: REFUSED_URL_REASON, detail };
}

/** 🔴 C23's two new dimensions (method / body): these are new behaviour and may have their own wire reasons. */
function refuseRequest(reason: string): { ok: false; reason: string; detail: string } {
  return { ok: false, reason, detail: reason };
}

/** The plan lookup. Always backfillPlanFor in production; parameterised only so tests can assert against a synthetic plan. */
export type PlanLookup = (platform: string) => BackfillEnumPlan | null;

/** The value types allowed in a request body: scalars only. No nested objects or arrays. */
function isScalar(v: unknown): boolean {
  return v === null || typeof v === 'string' || typeof v === 'number' || typeof v === 'boolean';
}

/**
 * 🔴 W8 · The body URL's **query** dimension. Returns a detail sentence when the
 * URL must be refused, or null when it may go.
 *
 * The rule the whole function exists for: **the request may only carry a URL the
 * plan's own builder produced.** That is the same rule the POST body already
 * follows (`spec.body()` is the only body builder, tab-port.ts's doc above), and it
 * is what expresses the id's "legal shape" without inventing an id alphabet —
 * whatever the list endpoint handed us is the legal value, and nothing else is.
 *
 *  · No `detailQueryKey` declared ⇒ the body URL carries **no query at all**. Every
 *    URL ChatGPT's builder produces is query-free, so this changes nothing the plan
 *    itself sends; it only closes the door on a query nobody declared.
 *  · Declared ⇒ the query must hold exactly that one key, once, with a non-empty
 *    value, and the whole URL must equal `detailUrl(origin, value)` byte for byte.
 *    A second key, a repeated key, a missing key, an empty value, a differently
 *    encoded value and a '#fragment' all fail that comparison.
 *
 * 🔴 The refusal sentences go to the log only (the wire keeps C22's single sentence,
 *    see REFUSED_URL_REASON), and deliberately name neither the query key nor its
 *    value — a key name is not echoed back, the same call the POST body check makes.
 */
function checkDetailQuery(
  plan: BackfillEnumPlan,
  u: URL,
  segment: 'detail' | 'detail2',
): string | null {
  // 🔴 W21 · The second step's URL is always query-free, and its own declaration
  //    carries no query key at all (see DetailStep2Spec): everything variable about
  //    step 2 travels in its POST body, which is validated below. So the rule for
  //    it is the "no query at all" rule, and it must not consult
  //    `plan.detailQueryKey` — that field belongs to step 1's URL, and borrowing it
  //    here would let step 1's query key ride in on step 2's request.
  const key = segment === 'detail2' ? undefined : plan.detailQueryKey;
  if (key === undefined) {
    if (u.search !== '') return 'body url carries a query the plan did not declare';
    /**
     * 🔴 W21 · A **fragment** is refused here too, and this is the one place the rule
     * had to be spelled out rather than inherited.
     *
     * The rule this whole function exists for is "what was checked is what is sent".
     * A plan that declares a query key gets that for free — its URL is compared
     * against the plan's own builder with `toString()`, which carries the fragment,
     * so a '#...' already fails there. A plan that declares no query key had only the
     * `search` test, and `new URL('.../x#y').search` is empty: the URL the check
     * approved and the URL handed to `fetch` were not necessarily the same string.
     * Nothing in this code builds a fragment, so no plan's own request is affected;
     * what this closes is a difference between the allowed set and the intended set.
     */
    return u.hash === '' ? null : 'body url carries a fragment the plan did not declare';
  }
  for (const found of u.searchParams.keys()) {
    if (found !== key) return 'body url carries a query key outside the declared allow-list';
  }
  const values = u.searchParams.getAll(key);
  if (values.length !== 1) {
    // Zero = the id is missing; more than one = which one is the id is ambiguous.
    return 'body url must carry the declared query key exactly once';
  }
  if (values[0] === '') return 'body url carries an empty value for the declared query key';
  const detailUrl = plan.detailUrl;
  if (detailUrl && u.toString() !== detailUrl(u.origin, values[0]!)) {
    return 'body url is not the one this plan itself builds';
  }
  return null;
}

/**
 * 🔴 **C23's security landing point. Whether the content script dares send this one request.**
 *
 * ## What "GET and POST are not the same thing, security-wise" actually means
 * A GET has one degree of freedom, the URL, and the URL is already pinned three
 * ways (same origin + platform table + path).
 * A POST adds a body — a field of **arbitrary length and arbitrary structure that
 * will be delivered to the target server**. If it were unconstrained, this content
 * script would become a proxy for "send any POST to chatgpt.com on my behalf":
 * holding the user's login cookies, performing writes the user never asked for.
 * **That is unacceptable.**
 *
 * ## So the body's freedom is squeezed down to this
 *  · Who can build it: only the plan's own `spec.body()` (lib/backfill/enumerate.ts).
 *    The engine never accepts a body passed in from outside (engine.ts's
 *    listRequestInit/detailRequestInit).
 *  · Once received it is checked again: top-level keys ⊆ the plan's declared
 *    bodyKeys (a closed set), values scalars only, the whole thing ≤
 *    MAX_REQUEST_BODY_BYTES, and the Content-Type must match exactly.
 *  · ⇒ Even if someone could stuff something into this message channel, the only
 *    thing they could change is **the scalar value of one key inside the closed
 *    set** (for a plan like Kimi's, "spell the page cursor as a different number").
 *    They cannot change the URL, the origin, the path or the method, cannot add a
 *    key, and cannot smuggle in a nested structure or a large payload.
 *
 * ## Who can stuff something in (the direct answer to criterion 4)
 * This function is called only from the `browser.runtime.onMessage` path
 * (entrypoints/dw-bridge.content.ts:161-176). **Page JS cannot reach it**:
 * `browser.runtime` exists only in the ISOLATED world and the page cannot obtain
 * it, and the manifest has no `externally_connectable`, so no website can
 * sendMessage in. The only entry point a page can touch is
 * `window.addEventListener('message')` in the same file, and that path only
 * produces `chat-captured`, which is **entirely separate from sending requests**.
 */
export function checkBackfillRequest(
  spec: BackfillRequestSpec,
  pageOrigin: string,
  lookup: PlanLookup = backfillPlanFor,
): RequestVerdict {
  let u: URL;
  try {
    u = new URL(spec.url);
  } catch {
    return refuseUrl('url is not parseable');
  }
  if (u.origin !== pageOrigin) {                        // 1 · same origin
    return refuseUrl('url is not same-origin with the page');
  }
  const row = getPlatformByOrigin(u.origin);            // 2 · in the platform table
  if (!row) return refuseUrl('origin is not in the platform table');
  const plan = lookup(row.id);                          // 3 · this platform really can be backfilled
  if (!plan) return refuseUrl(`platform ${row.id} has no backfill plan`);

  // 4 · Only its own paths — which also settles which segment this is (deciding the permitted method/body).
  let segment: BackfillSegment;
  if (u.pathname === plan.listPath) segment = 'list';
  // 🔴 C26: detailPath may be null (the list segment is sourced, the body segment
  //    is not — Perplexity). null ⇒ this platform has **no** permitted body URL. The
  //    allowlist is not loosened and does no prefix wildcarding: what is permitted
  //    is still only the path the plan itself wrote down, character for character.
  else if (plan.detailPath !== null && detailPathMatches(plan.detailPath, u.pathname)) segment = 'detail';
  // 🔴 W21 · The second step, when the plan declares one. It is a **third** named
  //    path, not a wildcard over the first: a plan with no `detailStep2` permits
  //    exactly the same set of URLs it permitted before this change.
  else if (plan.detailStep2 && detailPathMatches(plan.detailStep2.path, u.pathname)) segment = 'detail2';
  else return refuseUrl('path is not a backfill endpoint');

  // 4b · 🔴 W8 · The body URL's query, which C26 never had to look at because no
  //      plan put its id there. W8 declared one (DeepSeek), so the query dimension
  //      is now checked too — see checkDetailQuery. W21 applies it to both detail
  //      segments.
  if (segment !== 'list') {
    const refused = checkDetailQuery(plan, u, segment);
    if (refused !== null) return refuseUrl(refused);
  }

  // 5 · method: the closed set first, then it must equal the plan's declared one for this segment exactly.
  const method = spec.method ?? 'GET';
  if (!(ALLOWED_BACKFILL_METHODS as readonly string[]).includes(method)) {
    return refuseRequest(`refused: method ${sanitiseMethod(method)} is not in the allowed set`);
  }
  const expected = expectedMethodFor(plan, segment);
  if (method !== expected) {
    return refuseRequest(`refused: ${row.id} ${segment} segment must be ${expected}, got ${sanitiseMethod(method)}`);
  }

  const post = postSpecFor(plan, segment);
  if (!post) {
    // 6 · A GET segment: no body or Content-Type at all.
    if (spec.body !== undefined) return refuseRequest('refused: GET request must not carry a body');
    if (spec.contentType !== undefined) {
      return refuseRequest('refused: GET request must not carry a content-type');
    }
    return { ok: true, url: spec.url, method: 'GET' };
  }

  // 7 · The closed-set validation of a POST segment's body.
  if (typeof spec.body !== 'string') return refuseRequest('refused: POST request has no string body');
  if (new TextEncoder().encode(spec.body).byteLength > MAX_REQUEST_BODY_BYTES) {
    return refuseRequest('refused: request body exceeds MAX_REQUEST_BODY_BYTES');
  }
  if (spec.contentType !== post.contentType) {
    return refuseRequest('refused: content-type is not the one declared by the plan');
  }
  if (!(ALLOWED_BACKFILL_CONTENT_TYPES as readonly string[]).includes(spec.contentType)) {
    // A plan is not allowed to declare a Content-Type outside the closed set either. Belt and braces: a mistyped table still cannot send.
    return refuseRequest('refused: content-type is not in the allowed set');
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(spec.body);
  } catch {
    return refuseRequest('refused: request body is not JSON');
  }
  if (!isRecord(parsed) || Array.isArray(parsed)) {
    return refuseRequest('refused: request body is not a JSON object');
  }
  const allowedKeys = new Set(post.bodyKeys);
  const arrayKeys = new Set(post.bodyArrayKeys ?? []);
  for (const [key, value] of Object.entries(parsed)) {
    if (!allowedKeys.has(key)) {
      // 🔴 Return only "a key is outside the closed set", never the key name itself: a key name could be stuffed in as a smuggling vector.
      return refuseRequest('refused: request body has a key outside the declared allow-list');
    }
    /**
     * 🔴 W21 · **A declared array-of-strings key.**
     *
     * The second detail step's body is "the message ids the skeleton just named",
     * so one key has to hold a list. Keeping the scalar-only rule and letting this
     * one through unchecked would be the worst of both: a value of arbitrary
     * length, of arbitrary element type, of arbitrary nesting. So the array is
     * admitted only under four conditions, all of which have to hold: the key was
     * declared as an array key by the plan, the value really is an array, it holds
     * at most MAX_BODY_ARRAY_ITEMS items, and every item is a non-empty string.
     *
     * 🔴 Deliberately no nesting: an element that is itself an array or an object
     *    is refused, so the value's shape is one level deep and fixed — the same
     *    reason the scalar rule exists. Combined with the byte ceiling above, the
     *    only thing a stuffed message could change is which strings appear in a
     *    list of strings inside one fixed key of one fixed request.
     */
    if (arrayKeys.has(key)) {
      if (!Array.isArray(value)) {
        return refuseRequest('refused: request body value is not the declared array');
      }
      if (value.length > MAX_BODY_ARRAY_ITEMS) {
        return refuseRequest('refused: request body array exceeds MAX_BODY_ARRAY_ITEMS');
      }
      for (const item of value) {
        if (typeof item !== 'string' || item.length === 0) {
          return refuseRequest('refused: request body array holds a value that is not a non-empty string');
        }
      }
      continue;
    }
    if (!isScalar(value)) {
      return refuseRequest('refused: request body value is not a scalar');
    }
  }
  return { ok: true, url: spec.url, method: 'POST', body: spec.body, contentType: spec.contentType };
}

/** Truncate the method before echoing it in a refusal reason, so an over-long string is not carried into the log verbatim. */
function sanitiseMethod(method: string): string {
  return method.length > 16 ? `${method.slice(0, 16)}…` : method;
}

/**
 * 🔴 The URL allowlist C22 left behind, with unchanged semantics: **can this URL be
 * sent with its segment's default method?**
 * It is now a special case of checkBackfillRequest (no method ⇒ 'GET'), so the "URL
 * allowlist" and the "method/body allowlist" cannot tell different stories — there
 * is only one decision.
 */
export function isAllowedBackfillUrl(url: string, pageOrigin: string): boolean {
  return checkBackfillRequest({ url }, pageOrigin).ok;
}

export type FetchLike = (
  url: string,
  init?: BackfillRequestInit,
) => Promise<{ status: number; text: () => Promise<string> }>;

/**
 * The content-script-side fetch. **This code runs in the context of the page the
 * user is logged into.**
 * Every failure becomes `{ok:false}`; an exception is never thrown back into the
 * message channel (throwing would surface as an incomprehensible "Could not
 * establish connection").
 */
export async function serveBackfillFetch(
  request: string | BackfillRequestSpec,
  pageOrigin: string,
  fetchImpl: FetchLike,
  lookup: PlanLookup = backfillPlanFor,
): Promise<BackfillFetchReply> {
  // The string form is C22's calling convention, kept: equivalent to "this URL, with its segment's default method".
  const spec: BackfillRequestSpec = typeof request === 'string' ? { url: request } : request;
  const verdict = checkBackfillRequest(spec, pageOrigin, lookup);
  if (!verdict.ok) {
    // 🔴 A trace: a refusal has to be sayable too, and it goes through the same
    //    error channel as a failed fetch ({ok:false} → tabHttpPort throw → the
    //    engine persists halt('transport-error')). Only the technical reason is
    //    logged — never the URL, body or conversation body.
    console.warn(`[chat-stasher] backfill fetch refused: ${verdict.detail}`);
    return { ok: false, error: verdict.reason };
  }
  try {
    const init: BackfillRequestInit = verdict.method === 'POST'
      ? { method: 'POST', body: verdict.body, contentType: verdict.contentType as BackfillRequestInit['contentType'] }
      : { method: 'GET' };
    // 🔴 A GET segment keeps C22's call byte for byte: pass the url only, not one argument more.
    const res = verdict.method === 'GET' ? await fetchImpl(verdict.url) : await fetchImpl(verdict.url, init);
    const text = await res.text();
    if (new TextEncoder().encode(text).byteLength > MAX_RAW_BYTES) {
      // The same size red line as the live leg: an over-large response is not conversation JSON.
      return { ok: false, error: 'refused: response exceeds MAX_RAW_BYTES' };
    }
    return { ok: true, status: res.status, text };
  } catch (err) {
    // Only the technical detail goes back, never the body.
    return { ok: false, error: (err as Error).message };
  }
}

/**
 * The content script's message entry point. Returning null means "this message is
 * not for me" and the caller should pass it to the other listeners. It is a pure
 * function so that it can be tested under node.
 */
export function handleBackfillMessage(
  message: unknown,
  pageOrigin: string,
  fetchImpl: FetchLike,
  lookup: PlanLookup = backfillPlanFor,
): Promise<BackfillFetchReply | { ok: true; origin: string }> | null {
  if (isBackfillPing(message)) return Promise.resolve({ ok: true as const, origin: pageOrigin });
  if (isBackfillFetchRequest(message)) {
    return serveBackfillFetch(specFromMessage(message), pageOrigin, fetchImpl, lookup);
  }
  return null;
}

export type TabSend = (tabId: number, message: unknown) => Promise<unknown>;

/**
 * 🔴 W7 · How long one page round-trip may take before it is declared lost.
 *
 * **Why exactly this number.**
 *  · It must be **larger than the slowest legitimate round**: one request for a
 *    16 MiB conversation body plus one token read. A smaller budget would abort a
 *    fetch that was going to succeed, which turns a slow page into a lost one —
 *    a worse failure than the one this bounds.
 *  · And it must stay **below the shortest alarm gap**
 *    (lib/backfill/alarm.ts's BACKFILL_TICK_DELAY_MIN_MINUTES = 5 — since W16
 *    the gap is drawn from `[5, 10]` minutes, so 5 is its floor, i.e. the
 *    tightest case this bound has to hold against). One stuck page
 *    round is then declared lost within a fraction of an alarm period, instead of
 *    still being in flight when the next alarm arrives. (This bounds one *request*;
 *    the round as a whole is bounded separately by the engine's own detail budget.)
 *
 * 🔴 **What a timeout is not.** It is not "this page has no data" and not "this
 *    conversation is already fetched". It means **we did not finish reading** —
 *    which is why it throws rather than replying empty, so the engine halts with
 *    'transport-error' and the debt stays owed.
 */
export const BACKFILL_TAB_REPLY_TIMEOUT_MS = 90_000;

/**
 * 🔴 W7 · Bound one `send` by a timeout.
 *
 * Why this has to exist: `browser.tabs.sendMessage` **has no timeout of its own**.
 * When the page was reloaded, or the extension itself was reloaded, the content
 * script's context is gone and the returned promise can simply **never settle**.
 * The caller is a single-flight tick (schedule.ts's `inFlight`), and that lock is
 * released by a `finally` that runs only once the awaited round settles — so one
 * such round used to hold the lock **forever**, and every later round answered
 * 'already-running' without fetching a single body.
 *
 * The timer is cleared on both outcomes, so a round that answers in time leaves no
 * timer pending. (A `pending` that settles after the timeout is already handled by
 * Promise.race's own subscription, so it cannot surface as an unhandled rejection.)
 */
async function withReplyTimeout<T>(
  pending: Promise<T>,
  tabId: number,
  timeoutMs: number,
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      pending,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => {
          // Only technical facts on the wire: the tab id and the budget. Never the
          // URL, the conversation id or any body text.
          reject(new Error(`tab ${tabId} did not answer the backfill fetch within ${timeoutMs / 1000} s`));
        }, timeoutMs);
      }),
    ]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}

/**
 * Wrap "one live platform tab" into the HttpPort the engine understands.
 * 🔴 Every failure throws: the engine halts with 'transport-error' and leaves a
 *    trace, and never silently treats it as "fetched empty data".
 *
 * 🔴 W7: `timeoutMs` is injectable **for tests only** — production callers pass no
 *    third argument and get BACKFILL_TAB_REPLY_TIMEOUT_MS.
 */
export function tabHttpPort(
  tabId: number,
  send: TabSend,
  timeoutMs: number = BACKFILL_TAB_REPLY_TIMEOUT_MS,
): HttpPort {
  return async (url: string, init?: BackfillRequestInit): Promise<HttpResponse> => {
    // 🔴 The back-compat landing point: GET with no body ⇒ the message sent is
    //    **byte for byte** still `{type, url}`, not one field more. What C22's
    //    ChatGPT path sees on the wire is completely unchanged.
    const message = !init || (init.method === 'GET' && init.body === undefined)
      ? { type: BACKFILL_FETCH_MESSAGE, url }
      : { type: BACKFILL_FETCH_MESSAGE, url, method: init.method, body: init.body, contentType: init.contentType };
    const reply = await withReplyTimeout(send(tabId, message), tabId, timeoutMs);
    if (!isRecord(reply)) {
      throw new Error(`tab ${tabId} gave no reply for the backfill fetch`);
    }
    if (reply.ok !== true) {
      throw new Error(String(reply.error ?? 'tab refused the backfill fetch'));
    }
    if (typeof reply.status !== 'number' || typeof reply.text !== 'string') {
      throw new Error(`tab ${tabId} replied with an unrecognised shape`);
    }
    return { status: reply.status, text: reply.text };
  };
}

// ---------------------------------------------------------------------------
// The registry of live tabs
//
// Why it is needed: when the alarm wakes, the SW is **brand new** with no in-memory
// state and no idea which pages the user has open. The content script checks in once
// on every load (sender.tab.id is filled in by the browser; no 'tabs' permission
// needed), and background records it in storage.local. When the alarm wakes it pings
// down this list; a ping that fails (the tab is closed) counts as no port — the
// list converges on its own.
//
// 🔴 W13 · "Converges on its own" was true, and too eager. Forgetting a tab on the
// *first* failed ping cannot tell a closed tab from a busy renderer, so a page that
// was merely slow dropped out of the registry — and since only a page load puts one
// back, the backfill lost its fetch channel until the user reloaded. The rule now
// needs `TAB_PING_MISSES_BEFORE_FORGET` consecutive misses, which still converges
// within one tick for a genuinely dead tab.
// ---------------------------------------------------------------------------

function isTabEntry(v: unknown): v is TabEntry {
  return isRecord(v)
    && typeof v.tabId === 'number'
    && Number.isInteger(v.tabId)
    && typeof v.origin === 'string'
    && typeof v.at === 'number';
}

export async function loadTabs(store: BackfillStore | null): Promise<TabEntry[]> {
  if (!store) return [];
  const raw = await store.load(BACKFILL_TABS_KEY);
  return Array.isArray(raw) ? raw.filter(isTabEntry) : [];
}

/** Record one (deduplicated by tabId, most recent first). */
export async function rememberTab(store: BackfillStore | null, entry: TabEntry): Promise<TabEntry[]> {
  if (!store) return [];
  const rest = (await loadTabs(store)).filter((t) => t.tabId !== entry.tabId);
  const next = [entry, ...rest].slice(0, MAX_TAB_ENTRIES);
  await store.save(BACKFILL_TABS_KEY, next);
  return next;
}

export async function forgetTab(store: BackfillStore | null, tabId: number): Promise<void> {
  if (!store) return;
  const next = (await loadTabs(store)).filter((t) => t.tabId !== tabId);
  await store.save(BACKFILL_TABS_KEY, next);
}

/**
 * 🔴 W13 · Write one entry's consecutive-miss count **in place**.
 *
 * Deliberately not `rememberTab`: that one dedups by tabId and moves the entry to
 * the front, because its caller is a fresh hello. Reusing it here would reorder the
 * registry every time a ping failed, so which tab gets tried first would drift with
 * the failure pattern — a change in behaviour with nothing to do with liveness.
 * The order of the registry is the user's tab order; a miss is not news about it.
 */
async function setTabMisses(store: BackfillStore | null, tabId: number, misses: number): Promise<void> {
  if (!store) return;
  const tabs = await loadTabs(store);
  const next = tabs.map((t) => (t.tabId === tabId ? { ...t, misses } : t));
  await store.save(BACKFILL_TABS_KEY, next);
}

/**
 * Pick one tab that is really still alive.
 *
 * 🔴 W13 · A ping that does not go through **no longer strikes the tab off on the
 * first try** — that is the second half of the defect this task exists for. See
 * `TAB_PING_MISSES_BEFORE_FORGET`: the entry is forgotten on the 2nd consecutive
 * miss and its counter is reset to 0 by any successful ping, so a renderer that is
 * merely busy for a few seconds keeps its place, while a closed tab still converges
 * out of the list within one tick.
 *
 * origin = null means "any platform will do" (used when the popup asks for
 * transportWired).
 */
export async function pickLiveTab(
  store: BackfillStore | null,
  origin: string | null,
  ping: (tabId: number) => Promise<unknown>,
): Promise<TabEntry | null> {
  // Loaded once, so a write inside the loop cannot change what this pass iterates.
  for (const entry of await loadTabs(store)) {
    if (origin !== null && entry.origin !== origin) continue;
    let alive = false;
    try {
      const reply = await ping(entry.tabId);
      alive = isRecord(reply) && reply.ok === true;
    } catch {
      // The tab is closed / the content script is not there: not an error, the normal case.
      alive = false;
    }
    if (alive) {
      // 🔴 A live tab clears its own record. Only written when there is something to
      //    clear, so the normal path (a healthy tab that was never missed) does not
      //    pay a storage write per tick.
      if ((entry.misses ?? 0) !== 0) await setTabMisses(store, entry.tabId, 0);
      return entry;
    }
    const misses = (entry.misses ?? 0) + 1;
    if (misses >= TAB_PING_MISSES_BEFORE_FORGET) await forgetTab(store, entry.tabId);
    else await setTabMisses(store, entry.tabId, misses);
  }
  return null;
}
