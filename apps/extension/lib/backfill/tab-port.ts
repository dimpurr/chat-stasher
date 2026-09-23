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
 *  --- 🔴 W22 added no new dimension, and that is worth saying out loud: a plan
 *  --- whose list cursor travels **inside the POST body** (Kimi) is covered by
 *  --- rules 5-7 exactly — same closed key set, same scalar-value limit, same
 *  --- Content-Type rule. What a stuffed message could change there is the same
 *  --- thing it could change before ("spell the cursor as a different string"), and
 *  --- nothing wider.
 *  --- 🔴 W29 added one dimension, and it is a **narrowing**, not a widening:
 *  --- rule 11, below.
 * 11. a **form** segment (Gemini) is admitted by `checkFormRequest`:
 *     · the URL's query must be exactly the plan's pinned key→value set, with no
 *       fragment — this is also how the segment is told apart, because this
 *       plan's two segments share one path (see `formSegmentFor`);
 *     · the form's field names are a closed set, each present exactly once;
 *     · every field other than the batch must be **empty** — the credential
 *       field exists because the page's own request has it and is blank because
 *       only the page-context wrapper may fill it;
 *     · the batch is one four-element call naming an rpcid from the closed set,
 *       with an envelope kind the plan declares, and arguments the plan's own
 *       `checkArgs` accepts. The freedom left is the same as a JSON body's: the
 *       scalar value of a key, i.e. one conversation id and one opaque cursor.
 *     The JSON rules (key set, scalar-only values, JSON object) do **not** apply
 *     to it, and rule 8's query check does not either — a form body is not JSON,
 *     so being checked by the JSON rules would mean being checked by nothing.
 *  --- 🔴 W31c added no request dimension at all, and that is the point of it:
 *  --- the channel gained a **question** (`CLAUDE_ORG_REQUEST_MESSAGE`), whose
 *  --- answer is a decision rather than a document. The only request it can lead
 *  --- to is the resolution-only path W31 already declared on the plan, and it
 *  --- goes through the checks above exactly like any other fetch on this channel
 *  --- — the question itself carries no URL, no method, no body and no scope, so
 *  --- there is nothing on it to smuggle.
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
import type { OrgResolution } from './claude-org';
import {
  backfillPlanFor,
  detailPathMatches,
  expectedMethodFor,
  formQueryMatches,
  formSegmentFor,
  isFormPostSpec,
  pinnedQueryMatches,
  postSpecFor,
  scopePathMatches,
  ALLOWED_BACKFILL_CONTENT_TYPES,
  ALLOWED_BACKFILL_METHODS,
  MAX_BODY_ARRAY_ITEMS,
  MAX_REQUEST_BODY_BYTES,
  type BackfillEnumPlan,
  type BackfillMethod,
  type BackfillRequestInit,
  type BackfillSegment,
  type FormPostSpec,
} from './enumerate';
import type { HttpPort, HttpResponse } from './engine';
import type { BackfillStore } from './store';
import { TAB_HELLO_MIN_INTERVAL_MS } from './tab-hello';

/** background → content script: fetch this URL for me. */
export const BACKFILL_FETCH_MESSAGE = 'cs-backfill-fetch';
/** background → content script: are you still alive (used to answer the popup's transportWired faithfully). */
export const BACKFILL_PING_MESSAGE = 'cs-backfill-ping';
/** content script → background: I am alive on a platform page; the tab id comes with the sender. */
export const BACKFILL_TAB_HELLO_MESSAGE = 'cs-backfill-tab-hello';
/**
 * 🔴 W31c · background → content script: **which organization is this page using?**
 *
 * This is not a fetch and not a ping: it is a *question the page side is the only
 * one able to answer*. claude.ai needs an organization id in every request path
 * and the id is in no page URL; two of the three sources for it (the
 * `lastActiveOrg` cookie and a same-origin `GET /api/organizations`) are
 * reachable only from inside the page. So the background asks over this channel
 * instead of guessing, and the answer is a decision (an organization, or a named
 * halt) rather than a document.
 *
 * 🔴 It is a message and not a "let the background fetch it": see the header of
 *    this file — the service worker has no host permission, so the request must
 *    be made by the content script that is already on the page. The one request
 *    this question may cost goes through the same allowlist as every other
 *    request on this channel (lib/backfill/claude-org.ts decides when it is
 *    spent).
 */
export const CLAUDE_ORG_REQUEST_MESSAGE = 'cs-backfill-claude-org';

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
  /**
   * 🔴 W64c · `survivedCredentialReread` is **carried, never inferred**: the page-side
   * wrapper that owns a credential is the only code that can know whether a refusal
   * came back from its credential path, and this field is how that fact reaches the
   * engine's classifier. Absent means "nobody claimed it", which the classifier reads
   * as "no evidence" — never as the opposite of a claim. It is optional on every hop
   * so that a reply from any other wrapper, and every existing reply shape, stays what
   * it was.
   */
  | { ok: true; status: number; text: string; survivedCredentialReread?: boolean }
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

export function isClaudeOrgRequest(v: unknown): v is { type: string } {
  return isRecord(v) && v.type === CLAUDE_ORG_REQUEST_MESSAGE;
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
  segment: BackfillSegment,
): string | null {
  /**
   * 🔴 W31 · **A pinned query** (claude.ai's tree request). Its parameters are
   * constants of the endpoint rather than a value this plan carries, so there is
   * no `detailQueryKey` to compare a value against — the whole set is the
   * declaration, and `pinnedQueryMatches` is the same rule a form segment's
   * pinned query already follows. Checked **instead of**, never in addition to,
   * the one-key rule: a plan declares one or the other.
   */
  if (segment === 'detail' && plan.detailQueryPinned) {
    if (!pinnedQueryMatches(plan.detailQueryPinned, u)) {
      return 'body url carries a query the plan did not declare';
    }
    return u.hash === '' ? null : 'body url carries a fragment the plan did not declare';
  }
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
  /**
   * 🔴 W31 · **The scope this page side resolved**, for a plan whose paths carry it
   * (claude.ai's organization). It is a parameter rather than something read out
   * of the URL on purpose: a check that reads the value it is checking is not a
   * check. `null` means "no scope was resolved", and for a scoped plan that
   * refuses every list/body URL — a request naming an organization this page did
   * not resolve is exactly the request that must not go out.
   */
  scope: string | null = null,
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
  // 🔴 W29 · A plan whose two segments share **one path** (batchexecute) is asked
  //    first, because pathname alone cannot tell its segments apart: it matches a
  //    segment by the query the plan itself pinned, and that query names the RPC.
  //    See formSegmentFor. A plan that declares no form segment is untouched by
  //    this line — formSegmentFor returns null for it.
  let segment: BackfillSegment;
  const viaForm = formSegmentFor(plan, u);
  /**
   * 🔴 W31 · **A plan whose paths carry the scope** is dispatched by
   * `scopePathMatches` against the plan's own templates, segment by segment, with
   * the scope the page side resolved. Two consequences worth stating:
   *  · the resolution-only path is answered **first**, so a URL that is exactly
   *    `/api/organizations` can never be read as the list path. The list template
   *    has two more segments, so the two cannot collide by construction — but the
   *    order makes that a property of this code rather than of the templates;
   *  · the `{org}` segment is compared with `scope`, so a request naming another
   *    organization is refused here rather than forwarded.
   *
   * A plan that declares no `scopeInPath` is untouched: this is one `if` in front
   * of the dispatch every existing plan already used.
   */
  const scopePaths = plan.scopeInPath;
  if (scopePaths && u.pathname === scopePaths.resolvePath) segment = 'resolve';
  else if (scopePaths && scopePathMatches(scopePaths.listPath, u.pathname, scope)) segment = 'list';
  else if (scopePaths && scopePathMatches(scopePaths.detailPath, u.pathname, scope)) segment = 'detail';
  else if (viaForm !== null) segment = viaForm;
  else if (u.pathname === plan.listPath) segment = 'list';
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

  /**
   * 🔴 W31 · **The resolution-only path** (claude.ai's `GET /api/organizations`).
   *
   * It carries no conversation data and no parameter of any kind: what it answers
   * is "which organization is this account using", and its answer is read once,
   * page-side, by the resolver. So it is admitted under exactly one shape — GET,
   * no query, no fragment, no body, no Content-Type — and every other shape is
   * refused with a sentence of its own rather than being pushed through the
   * list/body rules.
   *
   * 🔴 It is **not** a relaxation: this path is admitted only because this plan
   *    declared it (`scopeInPath.resolvePath`), and a plan that declares nothing
   *    permits exactly the URLs it permitted before. Both the method and the
   *    bodylessness are also enforced by the ordinary branch below
   *    (`postSpecFor` answers null for this segment), so this block adds the two
   *    URL rules that the list/body rules would have worded as if this were a
   *    conversation request.
   */
  if (segment === 'resolve') {
    if (u.search !== '') return refuseUrl('the resolution-only path carries a query the plan did not declare');
    if (u.hash !== '') return refuseUrl('the resolution-only path carries a fragment the plan did not declare');
    if ((spec.method ?? 'GET') !== 'GET') {
      return refuseRequest('refused: the resolution-only path is a GET');
    }
    if (spec.body !== undefined || spec.contentType !== undefined) {
      return refuseRequest('refused: the resolution-only path carries a body or a content-type');
    }
    return { ok: true, url: spec.url, method: 'GET' };
  }

  const post = postSpecFor(plan, segment);

  // 4b · 🔴 W8 · The body URL's query, which C26 never had to look at because no
  //      plan put its id there. W8 declared one (DeepSeek), so the query dimension
  //      is now checked too — see checkDetailQuery. W21 applies it to both detail
  //      segments.
  //      🔴 W29: **not** applied to a form segment. There the whole query is a
  //      pinned set of constants rather than one key carrying a value, so it is
  //      checked by `formQueryMatches` in the form branch below; running both
  //      would compare the same URL against two different declarations.
  if (segment !== 'list' && !isFormPostSpec(post)) {
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

  if (!post) {
    // 6 · A GET segment: no body or Content-Type at all.
    if (spec.body !== undefined) return refuseRequest('refused: GET request must not carry a body');
    if (spec.contentType !== undefined) {
      return refuseRequest('refused: GET request must not carry a content-type');
    }
    return { ok: true, url: spec.url, method: 'GET' };
  }

  // 7a · 🔴 W29 · A **form** segment. Its own rules, in their own block: a JSON
  //      body cannot even be parsed as this one, so sharing a branch would mean
  //      one of the two being checked by rules written for the other.
  if (isFormPostSpec(post)) {
    const refusal = checkFormRequest(post, spec, u);
    if (refusal !== null) return refusal;
    return { ok: true, url: spec.url, method: 'POST', body: spec.body, contentType: spec.contentType };
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

/**
 * 🔴 W29 · **A form segment's admission check.** Returns a refusal, or null when
 * the request may go.
 *
 * The four things it decides, and what each one is for:
 *  1. **the URL's query** — it must be exactly the plan's pinned key→value set
 *     (`formQueryMatches`), and carry no fragment. This is where the RPC is
 *     pinned: the query names it, and a request naming another RPC is not this
 *     segment's request at any other level either.
 *  2. **the field names** — a closed set, each exactly once.
 *  3. **every field except the batch must be empty.** This is the rule that keeps
 *     a credential out of the message channel: the page's token field is present
 *     because the page's own request carries it, and it is blank because only the
 *     page-context wrapper may fill it (lib/platform-auth.ts). A message that
 *     arrived with a token in it is therefore refused rather than forwarded.
 *  4. **the batch** — one call, the declared four positions, an rpcid from the
 *     closed set, and arguments the plan's own `checkArgs` accepts. The two
 *     opaque values inside those arguments (a conversation id, a page cursor) are
 *     the *only* things a stuffed message could vary, exactly as with a JSON
 *     body: everything else is a literal this plan wrote down.
 */
function checkFormRequest(
  post: FormPostSpec,
  spec: BackfillRequestSpec,
  u: URL,
): { ok: false; reason: string; detail: string } | null {
  if (u.hash !== '') return refuseUrl('body url carries a fragment the plan did not declare');
  if (!formQueryMatches(post, u)) return refuseUrl('body url carries a query the plan did not declare');

  if (typeof spec.body !== 'string') return refuseRequest('refused: POST request has no string body');
  if (new TextEncoder().encode(spec.body).byteLength > MAX_REQUEST_BODY_BYTES) {
    return refuseRequest('refused: request body exceeds MAX_REQUEST_BODY_BYTES');
  }
  if (spec.contentType !== post.contentType) {
    return refuseRequest('refused: content-type is not the one declared by the plan');
  }
  if (!(ALLOWED_BACKFILL_CONTENT_TYPES as readonly string[]).includes(spec.contentType)) {
    return refuseRequest('refused: content-type is not in the allowed set');
  }

  const fields = new URLSearchParams(spec.body);
  const names = new Set<string>();
  for (const name of fields.keys()) names.add(name);
  if (names.size !== post.bodyKeys.length) {
    return refuseRequest('refused: request body has a field outside the declared allow-list');
  }
  for (const key of post.bodyKeys) {
    const values = fields.getAll(key);
    if (values.length !== 1) {
      return refuseRequest('refused: request body must carry exactly one of each declared field');
    }
    if (key === post.batchKey) continue;
    // 🔴 Not echoed back: what the field is called and what it held are both
    //    facts about a request we refused, and one of them can be a credential.
    if (values[0] !== '') {
      return refuseRequest('refused: a form field this plan builds empty arrived with a value');
    }
  }

  return checkFormBatch(post, fields.get(post.batchKey) ?? '');
}

/**
 * The batch, structurally: `[[[rpcid, "<args as a JSON string>", null, "<kind>"]]]`.
 *
 * The nesting is written out rather than assembled from pieces, for the same
 * reason `buildBatchExecuteBody` writes it out: getting it wrong is the classic
 * way this call 400s, and a check that shares the builder's shape by
 * construction is the only kind that stays true when the builder moves.
 */
function checkFormBatch(
  post: FormPostSpec,
  value: string,
): { ok: false; reason: string; detail: string } | null {
  let batch: unknown;
  try {
    batch = JSON.parse(value);
  } catch {
    return refuseRequest('refused: batch field is not JSON');
  }
  // 🔴 Three levels, and each one is checked: the batch holds one group, the
  //    group holds one call, and the call is the four positions below. Collapsing
  //    any two of them would accept a body the plan's own builder never emits.
  if (!Array.isArray(batch) || batch.length !== 1) {
    return refuseRequest('refused: batch does not hold exactly one group');
  }
  const group = batch[0];
  if (!Array.isArray(group) || group.length !== 1) {
    return refuseRequest('refused: batch group does not hold exactly one call');
  }
  const call = group[0];
  if (!Array.isArray(call) || call.length !== 4) {
    return refuseRequest('refused: batch call is not the declared four-element form');
  }
  if (typeof call[0] !== 'string' || !post.rpcids.includes(call[0])) {
    // 🔴 The rpcid is not echoed back: a name that is not on the allow-list is an
    //    identifier from outside this plan, and refusals do not carry those.
    return refuseRequest('refused: batch names an rpcid outside the declared set');
  }
  if (call[2] !== null || call[3] !== post.batchKind) {
    return refuseRequest('refused: batch call carries an envelope this plan does not build');
  }
  if (typeof call[1] !== 'string') {
    return refuseRequest('refused: batch call carries no argument string');
  }
  let args: unknown;
  try {
    args = JSON.parse(call[1]);
  } catch {
    return refuseRequest('refused: batch arguments are not JSON');
  }
  const refused = post.checkArgs(call[0], args);
  return refused === null ? null : refuseRequest(`refused: ${refused}`);
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
export function isAllowedBackfillUrl(
  url: string,
  pageOrigin: string,
  scope: string | null = null,
): boolean {
  return checkBackfillRequest({ url }, pageOrigin, backfillPlanFor, scope).ok;
}

export type FetchLike = (
  url: string,
  init?: BackfillRequestInit,
) => Promise<{ status: number; text: () => Promise<string>; survivedCredentialReread?: boolean }>;

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
  scope: string | null = null,
): Promise<BackfillFetchReply> {
  // The string form is C22's calling convention, kept: equivalent to "this URL, with its segment's default method".
  const spec: BackfillRequestSpec = typeof request === 'string' ? { url: request } : request;
  const verdict = checkBackfillRequest(spec, pageOrigin, lookup, scope);
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
    // 🔴 W64c · The one fact the engine cannot derive is passed on here, and only when
    //    it is claimed (`true`). A response that carries nothing stays a reply of
    //    exactly the shape it was.
    return res.survivedCredentialReread === true
      ? { ok: true, status: res.status, text, survivedCredentialReread: true }
      : { ok: true, status: res.status, text };
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
  /**
   * 🔴 W31 · The scope this page side resolved, for a plan whose paths carry it.
   * It is the same value the content script hands to `fetch`'s URL builder, so
   * "the URL we validated" and "the URL we send" carry one organization, not two.
   */
  scope: string | null = null,
): Promise<BackfillFetchReply | { ok: true; origin: string }> | null {
  if (isBackfillPing(message)) return Promise.resolve({ ok: true as const, origin: pageOrigin });
  if (isBackfillFetchRequest(message)) {
    return serveBackfillFetch(specFromMessage(message), pageOrigin, fetchImpl, lookup, scope);
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
 * 🔴 W27 · **How long one liveness ping may take before the tab is counted as
 * having missed it.**
 *
 * Why a ping needs a budget at all, when W7 gave the fetch channel one: the ping
 * is answered by the *same* `tabs.sendMessage` channel and inherits the same
 * property — **it has no timeout of its own**. A content script whose context is
 * gone (extension reloaded, page reloaded, renderer wedged) can leave the promise
 * unsettled forever, and then `pickLiveTab` never reaches its next candidate:
 * `resolveHttpPort` returns nothing, the tick answers 'no-http-port', and the
 * whole fetch channel is lost even though a perfectly good second tab of that
 * origin is sitting in the registry behind the hung one.
 *
 * **Why exactly 10 seconds.**
 *  · It is not the fetch budget, because a ping is not a fetch. The 90 s above
 *    exists to cover one 16 MiB conversation body plus its transfer; a ping
 *    carries **no payload in either direction** and is answered by a listener
 *    that only returns `{ok:true, origin}`. The largest legitimate round is one
 *    IPC hop plus one queued task on the page's thread, which is milliseconds.
 *  · It must still clear a renderer that is busy rather than dead — the exact
 *    case W13 stopped treating as death. 10 s is orders of magnitude above a
 *    healthy round and far above any plausible scheduling delay, so a page that
 *    answers *at all* answers inside it.
 *  · It must stay well below the shortest alarm gap (5 minutes,
 *    `BACKFILL_TICK_DELAY_MIN_MINUTES`), so that even the worst case — every
 *    registered tab of that origin hung — is decided inside one tick. **12 tabs
 *    is the registry's ceiling (`MAX_TAB_ENTRIES`), and 12 × 10 s = 120 s < the
 *    300 s floor**, so the sweep always finishes before the next alarm can arrive,
 *    whatever the registry holds.
 *
 * 🔴 A timeout here is **not** "this tab has no channel" and not "there is no
 *    tab": it is "we did not get an answer in time", which is why it counts
 *    exactly like any other failed ping (`TAB_PING_MISSES_BEFORE_FORGET`) rather
 *    than evicting the tab on the spot.
 */
export const BACKFILL_PING_TIMEOUT_MS = 10_000;

/**
 * 🔴 W27 · A reply that did not arrive in time. A named type rather than a bare
 * `Error`, because the two callers have to tell "timed out" from "the channel
 * rejected" — a rejection is the browser answering at once (a dead tab), while a
 * timeout is a tab that is *there* and silent (a wedged renderer). They count the
 * same way toward forgetting, but only the second one is worth a log line.
 */
export class BackfillReplyTimeoutError extends Error {}

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
 *
 * 🔴 W27 · `what` names the message whose answer timed out. It is part of the
 *    sentence so that the two budgets this file now holds — the fetch's 90 s and
 *    the ping's 10 s — cannot be confused in a log or in a halt detail.
 */
async function withReplyTimeout<T>(
  pending: Promise<T>,
  tabId: number,
  timeoutMs: number,
  what: string,
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      pending,
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => {
          // Only technical facts on the wire: the tab id and the budget. Never the
          // URL, the conversation id or any body text.
          reject(new BackfillReplyTimeoutError(
            `tab ${tabId} did not answer ${what} within ${timeoutMs / 1000} s`,
          ));
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
    const reply = await withReplyTimeout(send(tabId, message), tabId, timeoutMs, 'the backfill fetch');
    if (!isRecord(reply)) {
      throw new Error(`tab ${tabId} gave no reply for the backfill fetch`);
    }
    if (reply.ok !== true) {
      throw new Error(String(reply.error ?? 'tab refused the backfill fetch'));
    }
    if (typeof reply.status !== 'number' || typeof reply.text !== 'string') {
      throw new Error(`tab ${tabId} replied with an unrecognised shape`);
    }
    // 🔴 W64c · Passed through only when the page claimed it. Anything else — an older
    //    content script, another platform's wrapper — produces the response it always
    //    did, with no field added, so the classifier sees "no evidence" and not a fact
    //    invented by the transport.
    return reply.survivedCredentialReread === true
      ? { status: reply.status, text: reply.text, survivedCredentialReread: true }
      : { status: reply.status, text: reply.text };
  };
}

/**
 * 🔴 W31c · How long the background waits for **"which organization is this page
 * using"** before it declares the question unanswered.
 *
 * **Why it needs a budget of its own**, and why it is neither of the two numbers
 * above:
 *  · the 90 s fetch budget covers one 16 MiB conversation body plus its transfer,
 *    and this question carries no conversation and no id in either direction;
 *  · the 10 s ping budget covers a message whose answer is a constant, and this
 *    one may cost **one same-origin `GET /api/organizations`** on the page side
 *    (lib/backfill/claude-org.ts spends it only when the page and the cookie both
 *    said nothing).
 *  · So the bound has to clear one small JSON request plus the page's own
 *    scheduling on top of that request — twice the ping budget does, with room to
 *    spare on a renderer that is busy rather than dead.
 *  · And it has to stay far below the shortest alarm gap (5 minutes,
 *    `BACKFILL_TICK_DELAY_MIN_MINUTES`), because an alarm tick may ask this
 *    question: a tick must be able to answer it and still finish inside the gap
 *    the alarm drew. 20 s versus 300 s leaves the tick's whole work in that gap.
 *
 * 🔴 A timeout is **not** "this account has no organizations" and not "it has
 *    several": it is "we did not get an answer in time", and it comes back as the
 *    resolver's own transient `transport-error` — retried on a later tick, with
 *    nothing written off (see `askTabForClaudeOrg`).
 */
export const CLAUDE_ORG_REPLY_TIMEOUT_MS = 20_000;

/**
 * 🔴 W31c · **Read one answer to `CLAUDE_ORG_REQUEST_MESSAGE`.**
 *
 * The reply crosses the same message channel as every other backfill message, so
 * it is checked rather than trusted: an unrecognised shape is **not** read as "no
 * organization" (that would turn a broken channel into a fact about the account),
 * it is the transient `transport-error` the resolver uses for "the request did not
 * complete".
 *
 * Only the closed set of fields is read, and nothing is echoed back: the refusal
 * sentences the resolver writes name neither an organization id nor a count of
 * anything but organizations.
 */
export function readClaudeOrgReply(reply: unknown, tabId: number): OrgResolution {
  if (!isRecord(reply)) {
    return { ok: false, halt: 'transport-error', detail: `tab ${tabId} gave no reply for the organization question` };
  }
  if (reply.ok === true) {
    const org = reply.org;
    if (typeof org !== 'string' || org.length === 0) {
      return { ok: false, halt: 'transport-error', detail: `tab ${tabId} named no organization` };
    }
    const source = reply.source;
    if (source !== 'page' && source !== 'cookie' && source !== 'endpoint') {
      return { ok: false, halt: 'transport-error', detail: `tab ${tabId} named an organization from an unknown source` };
    }
    return { ok: true, org, source };
  }
  const halt = reply.halt;
  if (halt !== 'org-ambiguous' && halt !== 'org-unresolved' && halt !== 'transport-error') {
    return { ok: false, halt: 'transport-error', detail: `tab ${tabId} refused the organization question unrecognisably` };
  }
  return { ok: false, halt, detail: typeof reply.detail === 'string' ? reply.detail : '' };
}

/**
 * 🔴 W31c · **Ask one live tab which organization its page is using.**
 *
 * Every way this can fail — no tab, a dead content script, a wedged renderer, a
 * reply whose shape is unrecognised — comes back as the resolver's
 * `transport-error`: a **transient** halt that the next tick tries again, never
 * "this account has no conversations". That is the same rule `tabHttpPort` above
 * follows for a failed fetch, and the same rule the resolver's own endpoint
 * failure follows (`OrgEndpointReading`), so "we could not ask" has exactly one
 * meaning on every path.
 *
 * `timeoutMs` is injectable **for tests only** — production callers pass no third
 * argument and get `CLAUDE_ORG_REPLY_TIMEOUT_MS`.
 */
export async function askTabForClaudeOrg(
  tabId: number,
  send: TabSend,
  timeoutMs: number = CLAUDE_ORG_REPLY_TIMEOUT_MS,
): Promise<OrgResolution> {
  let reply: unknown;
  try {
    reply = await withReplyTimeout(
      send(tabId, { type: CLAUDE_ORG_REQUEST_MESSAGE }),
      tabId,
      timeoutMs,
      'the organization question',
    );
  } catch (err) {
    return { ok: false, halt: 'transport-error', detail: (err as Error).message };
  }
  return readClaudeOrgReply(reply, tabId);
}

// ---------------------------------------------------------------------------
// The registry of live tabs
//
// Why it is needed: when the alarm wakes, the SW is **brand new** with no in-memory
// state and no idea which pages the user has open. The content script checks in on
// every load (sender.tab.id is filled in by the browser; no 'tabs' permission
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
//
// 🔴 W27 · Two halves of the same story then had to be finished, and they are the
// reason this section is longer than "a list of tab ids":
//  · **A ping could hang.** `tabs.sendMessage` has no timeout (same fact as W7's
//    fetch channel), so one wedged tab stopped `pickLiveTab` from ever reaching
//    the next candidate — see `BACKFILL_PING_TIMEOUT_MS`. A timeout is now just
//    another missed ping: it counts toward the same 2-strike rule, and the sweep
//    moves on. `resolveHttpPort` therefore always terminates, and always with the
//    *first tab that answers* rather than the first tab that was written down.
//  · **Only a page load put a tab back.** Reloading the extension tears the
//    content scripts down and reloading the page is the only thing that re-sends a
//    hello, so a service-worker restart or an extension reload left the registry
//    holding tabs that no longer exist, and the tick said 'no-http-port' until the
//    user happened to reload that tab. The content script now repeats its hello —
//    see lib/backfill/tab-hello.ts for the cadence and why it is not an alarm.
//
// 🔴 W33 · The repeating hello then made every hello cost a full read-modify-write
// of storage.local, once per open platform tab every 4–6 minutes, for a row that
// already says exactly what the hello would say. The rule now: **a repeat hello
// whose tab is already in the registry, from the same origin, younger than half the
// hello's floor interval, is a no-op** (`REGISTRY_WRITE_SKIP_MS`). A hello from an
// unknown tab, from a changed origin, or one that arrives after that window still
// writes at once — those are the ones that carry news. The decision is made against
// an **in-memory mirror** of the registry so the no-op costs no read either; the
// mirror is written by every path that writes the registry (see writeRegistry) and
// starts out empty in a fresh worker, where the first decision falls back to
// reading storage. Storage stays the source of truth: the mirror is never what a
// restart resumes from.
//
// 🔴 W51 · W27's re-hello is correct, and it is not a trigger this product can
// wait for. The hello is a page `setTimeout` on a 4–6 min jittered interval;
// Chrome throttles that toward hourly when the tab is hidden, and W27's own
// comment pins recovery on `visibilitychange` — a human returning to the tab.
// Unattended operation never does that. Chrome also reissues tab ids, so a
// registry keyed by `tabId` goes stale while the page is still open, and
// `pickLiveTab` returning at the first answering row means a dead row behind a
// healthy same-origin one is never pinged, never struck, never forgotten.
// `rememberTab` dedups by `tabId`, so a changed id appends rather than
// replaces, and dead rows accumulate toward `MAX_TAB_ENTRIES`.
//
// The recovery is therefore a **background** sweep, once per alarm tick, only
// when that tick is about to concede `no-http-port` for a registered target
// (never inside `resolveHttpPort`, which is also the popup's probe):
// `chrome.tabs.query({})` for ids the registry no longer has, ping them with
// `BACKFILL_PING_MESSAGE` (at most `TAB_SWEEP_PING_CAP` per tick), `rememberTab`
// whoever answers without evicting a row the query just listed as live. Gone
// ids are pruned through `writeRegistry` — a different rule from a missed
// ping, and not folded into `TAB_PING_MISSES_BEFORE_FORGET`. `tabs.query`
// returns `id` / `discarded` / `frozen` / `status` without the `tabs`
// permission; a fix that added a permission would be the wrong fix. A sweep
// that looks and finds nothing is `{ looked: true, registered: 0, crowded: 0 }`;
// a sweep that finds an answering tab and refuses it for want of a slot is
// `{ looked: true, crowded: N }`; a tick that never swept leaves the field
// `null`. Those must stay distinct: "we looked and found nothing" is not
// "we never looked", and it is not "we found the tab and turned it away".
// ---------------------------------------------------------------------------

function isTabEntry(v: unknown): v is TabEntry {
  return isRecord(v)
    && typeof v.tabId === 'number'
    && Number.isInteger(v.tabId)
    && typeof v.origin === 'string'
    && typeof v.at === 'number';
}

/**
 * How young a registry row has to be for a repeat hello to be a no-op.
 *
 * Half of `TAB_HELLO_MIN_INTERVAL_MS`: the shortest gap between two hellos of one
 * tab is the floor of the draw (4 minutes), so nothing legitimate can land inside
 * 2 minutes of the previous one. A hello that does is a duplicate — the tab is
 * already registered, and rewriting the row would only move it to the front and
 * stamp a newer `at` on a fact that has not changed.
 */
const REGISTRY_WRITE_SKIP_MS = TAB_HELLO_MIN_INTERVAL_MS / 2;

/**
 * The in-memory mirrors, one per store. A store with no entry here has not been
 * read by this worker yet — **absent is the honest "we do not know", not "empty"**:
 * `readRegistry` loads from storage in that case, which is also what a fresh worker
 * does on its first hello. It is keyed by the store rather than being one global
 * because two stores are two registries, and answering one from the other's rows
 * would be a wrong write, not a saved read.
 *
 * Deliberately module state: an MV3 worker being reclaimed loses it, and storage
 * stays the source of truth it resumes from.
 */
let tabRegistryMirrors = new WeakMap<BackfillStore, TabEntry[]>();

/** For tests only: an SW being reclaimed ⇒ module state resets. */
export function resetTabRegistryMirrorForTest(): void {
  tabRegistryMirrors = new WeakMap();
}

export async function loadTabs(store: BackfillStore | null): Promise<TabEntry[]> {
  if (!store) return [];
  const raw = await store.load(BACKFILL_TABS_KEY);
  return Array.isArray(raw) ? raw.filter(isTabEntry) : [];
}

/**
 * The registry as this worker last saw it; reads storage only when the mirror is
 * cold, and warms it from that one read — otherwise every hello after a worker
 * restart would re-read until something happened to write.
 */
async function readRegistry(store: BackfillStore): Promise<TabEntry[]> {
  const known = tabRegistryMirrors.get(store);
  if (known !== undefined) return known;
  const loaded = await loadTabs(store);
  tabRegistryMirrors.set(store, loaded);
  return loaded;
}

/**
 * The one place the registry is written. Keeps the mirror equal to what was just
 * saved — a stale mirror must not be able to make `rememberTab` skip a write for a
 * tab the registry no longer holds.
 */
async function writeRegistry(store: BackfillStore, next: TabEntry[]): Promise<void> {
  tabRegistryMirrors.set(store, next);
  await store.save(BACKFILL_TABS_KEY, next);
}

/** Record one (deduplicated by tabId, most recent first). */
export async function rememberTab(store: BackfillStore | null, entry: TabEntry): Promise<TabEntry[]> {
  if (!store) return [];
  const known = await readRegistry(store);
  const previous = known.find((t) => t.tabId === entry.tabId);
  if (
    previous !== undefined
    && previous.origin === entry.origin
    && entry.at - previous.at < REGISTRY_WRITE_SKIP_MS
  ) {
    // The registry already says this. Answering `{ok:true}` is still truthful:
    // the alarm can find this tab, which is the whole promise the hello carries.
    return known;
  }
  const rest = known.filter((t) => t.tabId !== entry.tabId);
  const next = [entry, ...rest].slice(0, MAX_TAB_ENTRIES);
  await writeRegistry(store, next);
  return next;
}

export async function forgetTab(store: BackfillStore | null, tabId: number): Promise<void> {
  if (!store) return;
  const next = (await readRegistry(store)).filter((t) => t.tabId !== tabId);
  await writeRegistry(store, next);
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
  const tabs = await readRegistry(store);
  const next = tabs.map((t) => (t.tabId === tabId ? { ...t, misses } : t));
  await writeRegistry(store, next);
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
 * 🔴 W27 · **Each ping is bounded, and the sweep always moves on.** The awaited
 * `ping` used to be able to hang forever (a content script whose context is gone,
 * or a wedged renderer), and because the loop `await`s it, one such tab stopped
 * every later candidate from ever being tried: `resolveHttpPort` returned nothing
 * and the tick reported 'no-http-port' while a healthy tab of that origin sat in
 * the registry. `BACKFILL_PING_TIMEOUT_MS` bounds it, and a timeout is handled
 * exactly like a rejection — counted as a miss, then the loop continues.
 *
 * 🔴 What is deliberately *not* changed: the registry order is still the order
 *    candidates are tried in (`setTabMisses` explains why a miss must not reorder
 *    it), and a tab is still only forgotten at `TAB_PING_MISSES_BEFORE_FORGET`
 *    consecutive misses. A timeout is one miss, not an eviction.
 *
 * origin = null means "any platform will do" (used when the popup asks for
 * transportWired).
 *
 * `pingTimeoutMs` is injectable **for tests only** — production callers pass no
 * fourth argument and get BACKFILL_PING_TIMEOUT_MS.
 */
export async function pickLiveTab(
  store: BackfillStore | null,
  origin: string | null,
  ping: (tabId: number) => Promise<unknown>,
  pingTimeoutMs: number = BACKFILL_PING_TIMEOUT_MS,
): Promise<TabEntry | null> {
  // Loaded once, so a write inside the loop cannot change what this pass iterates.
  for (const entry of await loadTabs(store)) {
    if (origin !== null && entry.origin !== origin) continue;
    let alive = false;
    try {
      const reply = await withReplyTimeout(ping(entry.tabId), entry.tabId, pingTimeoutMs, 'the backfill ping');
      alive = isRecord(reply) && reply.ok === true;
    } catch (err) {
      // The tab is closed / the content script is not there: not an error, the normal case.
      // 🔴 A timeout is the one case worth saying out loud: the tab is still in the
      //    registry and its silence is about to cost it a strike, so a user who
      //    wonders where their tab went finds a reason in the log instead of a gap.
      if (err instanceof BackfillReplyTimeoutError) {
        console.warn(`[chat-stasher] ${err.message}; counting it as a missed ping`);
      }
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

/**
 * 🔴 W51 · One row of `chrome.tabs.query({})`.
 *
 * `url` / `title` are redacted without the `tabs` permission; `id`, `discarded`,
 * `frozen` and `status` are not. This shape is the fields the sweep is allowed
 * to read — a caller that started matching on `url` would be the wrong fix
 * (that is the permission this product does not add).
 */
export interface TabQueryRow {
  id?: number;
  discarded?: boolean;
  frozen?: boolean;
}

/**
 * 🔴 W51 · What one recovery sweep did.
 *
 * `{ looked: false }` is "we could not look" (`tabs.query` missing or it
 * threw, or there is no store). That is not `{ looked: true, queried: 0 }`:
 * an unknown must not be recorded as empty. `origins`, `pingedIds` and
 * `recovered` are the in-memory answer the tick uses to retry the recovered
 * row without walking `pickLiveTab` again; they are not persisted (the trace
 * keeps counts only). `deferred` is how many eligible tabs were not pinged
 * because the sweep hit `TAB_SWEEP_PING_CAP` — so a capped sweep is not the
 * same record as one that pinged everything it wanted to. `crowded` is how
 * many answering tabs the sweep refused because the registry was already
 * full of live-listed rows — so a full-registry refusal is not the same
 * record as "looked and found nobody", and not the same as the ping cap.
 */
export type TabSweepReport =
  | { looked: false }
  | {
      looked: true;
      queried: number;
      pruned: number;
      pinged: number;
      registered: number;
      deferred: number;
      crowded: number;
      origins: string[];
      pingedIds: number[];
      recovered: Array<{ tabId: number; origin: string }>;
    };

/**
 * 🔴 W51 · Drop registry rows whose `tabId` is not in `liveIds`.
 *
 * This is **not** a missed ping. `pickLiveTab` still forgets a tab only at
 * `TAB_PING_MISSES_BEFORE_FORGET` consecutive misses, and still walks remaining
 * rows in registry order. A gone id is a fact the browser already stated
 * (`tabs.query` did not list it); counting it as a miss would mix two rules
 * and would reorder nothing, but it would also delay dropping a row we already
 * know cannot answer. Remaining rows keep the miss count they already had.
 *
 * 🔴 Writes through `writeRegistry`, never `store.save` directly: the W33
 *    mirror must agree with storage, or a tab we just pruned cannot
 *    re-register inside the hello skip window.
 */
export async function forgetMissingTabs(
  store: BackfillStore | null,
  liveIds: ReadonlySet<number>,
): Promise<number> {
  if (!store) return 0;
  const tabs = await readRegistry(store);
  const next = tabs.filter((t) => liveIds.has(t.tabId));
  const pruned = tabs.length - next.length;
  if (pruned === 0) return 0;
  await writeRegistry(store, next);
  return pruned;
}

function isPingOriginReply(v: unknown): v is { ok: true; origin: string } {
  return isRecord(v) && v.ok === true && typeof v.origin === 'string' && v.origin.length > 0;
}

/**
 * 🔴 W51 · How many unknown tabs one recovery sweep may ping.
 *
 * The sweep cannot tell a platform tab from any other without pinging — there
 * is no `tabs` permission, so `url` is redacted and origin-filtering is not
 * available. `Promise.all` over the whole `query({})` window would therefore
 * scale with every open tab, not with the registry. This product does not
 * burst: no mass refresh, no high-frequency request. Time is not sensitive,
 * so leftover candidates wait for a later tick.
 *
 * 12 is `MAX_TAB_ENTRIES`. More answering tabs cannot be stored, and 12
 * parallel liveness pings is the same fan-out `pickLiveTab` already accepts
 * serially (12 × 10 s = 120 s < the 5-minute alarm floor). The batch starts
 * at `now % n` so a prefix of tabs that will never answer (any page without
 * our content script) cannot hide a platform tab behind the cap forever.
 * `now` is already the sweep's clock; no extra state, and no tab id in the
 * persisted trace. A sweep that hit the cap writes `deferred > 0`; one that
 * pinged everything it wanted to writes `deferred: 0`.
 */
export const TAB_SWEEP_PING_CAP = MAX_TAB_ENTRIES;

function takeSweepPingBatch(candidates: readonly number[], now: number): { batch: number[]; deferred: number } {
  if (candidates.length <= TAB_SWEEP_PING_CAP) {
    return { batch: [...candidates], deferred: 0 };
  }
  const start = ((now % candidates.length) + candidates.length) % candidates.length;
  const batch: number[] = [];
  for (let i = 0; i < TAB_SWEEP_PING_CAP; i += 1) {
    batch.push(candidates[(start + i) % candidates.length]!);
  }
  return { batch, deferred: candidates.length - TAB_SWEEP_PING_CAP };
}

/**
 * 🔴 W51 · One unattended recovery pass over the tabs the browser currently has.
 *
 * `query` is `() => chrome.tabs.query({})` in production. Unknown tabs are
 * pinged in parallel up to `TAB_SWEEP_PING_CAP` (a serial 10 s budget per tab
 * would not fit inside the shortest alarm gap once the window holds more than
 * a handful), discarded and frozen tabs are not pinged (they have no content
 * script to answer), and whoever answers is registered through `rememberTab`
 * — so a repeat cannot duplicate a row (W27-C). A burst of those insertions
 * does not evict a row whose tabId the query just listed as live: prune
 * already dropped gone ids, so every remaining row is live, and a new row at
 * `MAX_TAB_ENTRIES` would slice one of them off. An answering tab refused
 * for that reason is counted as `crowded`, not as a successful look that
 * found nobody, and not as `deferred` (that count is tabs never pinged).
 *
 * `now` and `pingTimeoutMs` are injectable **for tests only**.
 */
export async function sweepUnregisteredTabs(
  store: BackfillStore | null,
  query: () => Promise<TabQueryRow[]>,
  ping: (tabId: number) => Promise<unknown>,
  now: number = Date.now(),
  pingTimeoutMs: number = BACKFILL_PING_TIMEOUT_MS,
): Promise<TabSweepReport> {
  if (!store) return { looked: false };

  let rows: TabQueryRow[];
  try {
    rows = await query();
  } catch (err) {
    // "I could not look" is not "there are no tabs".
    console.warn('[chat-stasher] tab sweep could not list open tabs', (err as Error).message);
    return { looked: false };
  }
  if (!Array.isArray(rows)) {
    console.warn('[chat-stasher] tab sweep got an unrecognised tabs.query result');
    return { looked: false };
  }

  const liveIds = new Set<number>();
  for (const row of rows) {
    if (typeof row.id === 'number' && Number.isInteger(row.id) && row.id >= 0) {
      liveIds.add(row.id);
    }
  }
  const pruned = await forgetMissingTabs(store, liveIds);
  const known = new Set((await readRegistry(store)).map((t) => t.tabId));

  const candidates: number[] = [];
  for (const row of rows) {
    const id = row.id;
    if (typeof id !== 'number' || !Number.isInteger(id) || id < 0) continue;
    if (known.has(id)) continue;
    if (row.discarded === true || row.frozen === true) continue;
    candidates.push(id);
  }

  const { batch, deferred } = takeSweepPingBatch(candidates, now);

  const replies = await Promise.all(batch.map(async (tabId) => {
    try {
      const reply = await withReplyTimeout(ping(tabId), tabId, pingTimeoutMs, 'the backfill ping');
      if (isPingOriginReply(reply)) return { tabId, origin: reply.origin };
    } catch (err) {
      // Not our content script, or silent. This id was never in the registry,
      // so a failed ping is not a miss and is not worth a log line unless it
      // was a timeout — a tab that is *there* and silent is the W27 case.
      if (err instanceof BackfillReplyTimeoutError) {
        console.warn(`[chat-stasher] ${err.message}; sweep moves on without registering it`);
      }
    }
    return null;
  }));

  const origins: string[] = [];
  const recovered: Array<{ tabId: number; origin: string }> = [];
  let registered = 0;
  let crowded = 0;
  for (const found of replies) {
    if (!found) continue;
    const knownNow = await readRegistry(store);
    const before = knownNow.some((t) => t.tabId === found.tabId);
    // Prune already dropped ids the query does not list, so every remaining
    // row is live. rememberTab's prepend+slice would evict one of them for
    // each new insertion; a burst of those is the defect. A single hello
    // evicting one tail row is the accepted old behaviour and still lives
    // on rememberTab's own path. Refusing an answering tab for want of a
    // slot is not "looked and found nobody": it is counted as crowded.
    if (!before && knownNow.length >= MAX_TAB_ENTRIES) {
      crowded += 1;
      continue;
    }
    await rememberTab(store, { tabId: found.tabId, origin: found.origin, at: now });
    if (!before) {
      registered += 1;
      origins.push(found.origin);
      recovered.push({ tabId: found.tabId, origin: found.origin });
    }
  }

  return {
    looked: true,
    queried: rows.length,
    pruned,
    pinged: batch.length,
    registered,
    deferred,
    crowded,
    origins: [...new Set(origins)],
    pingedIds: batch,
    recovered,
  };
}
