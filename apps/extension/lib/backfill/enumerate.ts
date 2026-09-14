/**
 * The enumeration segment: fetch the conversation list + total.
 *
 * ## C22 · How this file went from "one platform's implementation" to "a table
 * plus an honest list of the gaps"
 *
 * Before: only ChatGPT's constants and parser lived here, referenced directly by
 * the engine. The consequence was not "other platforms do not move", it was
 * **other platforms getting hit with ChatGPT's path**: the engine used
 * listPageUrl(origin) to build `https://chat.deepseek.com/backend-api/conversations`,
 * the content script's isAllowedBackfillUrl compared paths but not platforms, so
 * the request really went out, came back 404 ⇒ halt('shape-changed'). What the
 * user saw was "the API changed" when the truth was "we never wrote DeepSeek's
 * list endpoint at all". Those two sentences mean completely different things to
 * a user.
 *
 * So now:
 *  · a platform that can be backfilled ⇒ has a complete plan in BACKFILL_PLANS
 *    (every item of the table below is filled in);
 *  · a platform that cannot ⇒ has a record in BACKFILL_UNSUPPORTED that **names
 *    what is missing**, and the engine halts with 'unsupported-platform' **before
 *    issuing any request**, which the popup then says plainly;
 *  · every row of the platform table must land on exactly one of those two sides
 *    — tests/c22-enumplat.test.ts keeps an eye on it.
 *
 * ## What a platform must declare at minimum to be backfillable (= the fields
 * ## of BackfillEnumPlan)
 *  1. listPath      the path of the list endpoint
 *  2. listUrl       the paging scheme (how offset/limit become a **GET** URL)
 *  3. parseListPage the list response's shape test: where the ids come from and where total comes from
 *  4. detailPath    the path prefix of the body endpoint (🔴 C26: null is allowed = no source for the body segment yet)
 *  5. detailUrl     conversation id → body URL (same; null is allowed)
 *  6. the body shape test — not a field of this table: it reuses lib/contract.ts's
 *                     responseShape (engine.ts:348, matchesResponseShape), the
 *                     same yardstick for the live leg and the backfill leg.
 *  7. provenance    the source, with the same standard as contract.ts's credibility
 *                   (source · repo/file line · license · date)
 *
 * 🔴 The structural constraint from before C23 (now lifted; the original text is
 *    kept so the comparison is possible):
 *    ~~HttpPort's signature was `(url: string) => Promise<HttpResponse>` (engine.ts:37),
 *    URL only, no method/body ⇒ **GET only**.~~
 *    C23 widened HttpPort to `(url, init?: BackfillRequestInit)`, so a plan can
 *    now declare `listPost` / `detailPost` — "this segment is POST, the body
 *    looks like this, and its top-level keys are only these".
 *    🔴 **But this change fills that declaration in for no platform at all** —
 *    kimi / gemini still have "no source found / only half" list shapes, and
 *    filling one in would be inventing it. So in the `missing` lists below, the
 *    "structural blocker" entry is rewritten as "the channel can send it now, but
 *    the parameters still have no source".
 *
 * ## Facts and review status (marked honestly, same standard as the rest of the file)
 *  · ChatGPT's conversation list is GET /backend-api/conversations?offset=&limit=
 *    and the response carries its own total.
 *    **This has not been re-reviewed** — this change forbids calling any platform
 *    API for real, and there is no logged-in session. So the field names `items`
 *    and `total` are **an assumption awaiting verification**, not a measurement.
 *  · The parser is therefore written as "shape mismatch ⇒ report shape-changed
 *    and stop" rather than a best-effort guess. A wrong assumption turns into a
 *    traced halt record immediately; it does not turn into a silently crawling
 *    leg, and it does not turn into fake progress.
 *  · The chatgpt pathHints registered at lib/contract.ts:114 is
 *    '/backend-api/conversation/' (singular, used for bodies); the list is
 *    '/backend-api/conversations' (plural). They are not the same.
 *
 * ## C26 · DeepSeek's cell went from "unknown" to "sourced", and grew two new things
 *
 *  1. **Cursor paging** (listCursorUrl / EnumPage.nextCursor / EnumPage.hasMore).
 *     C22's plan could only page by offset, because ChatGPT was the only row in
 *     the table. DeepSeek uses `count` + `before_seq_id` (cursor = the smallest
 *     seq_id on the previous page), and the offset scheme simply does not hold on
 *     it (page/offset/limit appeared in none of the five sources).
 *     🔴 Every "cannot read it" on this branch lands on a named outcome; see
 *     EnumTruncation in types.ts: cannot read seq_id ⇒ 'cursor-missing' (backfill
 *     reached this page only, **not** "enumeration finished"); cannot read
 *     has_more ⇒ 'has-more-missing' (**never** treat it as "no next page").
 *
 *  2. **Half a leg can now be written down** (detailPath/detailUrl may be null,
 *     plus `partial`). DeepSeek's list segment has a four-source provenance; its
 *     body segment had none. ~~(🔴 superseded by W8 below: DeepSeek's body segment
 *     is no longer the null case — Perplexity is.)~~ Previously that left only two options: keep calling
 *     the whole platform "unsupported" (even though the list is readable), or
 *     invent a body route to fill the plan in (which is the genuinely dangerous
 *     kind of lie). There is now a third way to write it, and it corresponds to a
 *     separate halt reason, 'detail-unsupported', whose difference from
 *     'unsupported-platform' is that the latter never issued a single request.
 *
 *  🔴 As with C22, this change **did not go online, has no logged-in session, and
 *     sent no request to deepseek.com**. DeepSeek's cell comes from the
 *     **multi-source cross-check** in research ticket R25, not from official
 *     documentation, and has not been verified end to end — the full "what we
 *     know / what we do not / how stale it might be" is at the head of
 *     DEEPSEEK_PLAN, and must not be reduced to the conclusion alone.
 *
 * ## W8 · DeepSeek's body segment was filled in, and the reason it was null is gone
 *
 * C26 wrote DeepSeek's body segment as `null` for exactly one stated reason: "the
 * route and the parameters of a SINGLE conversation's body have no multi-source
 * provenance". **That reason no longer holds**, on two independent kinds of
 * evidence (both written out in full at the head of DEEPSEEK_PLAN):
 *  1. a real logged-in browser session (2026-09-13) showed DeepSeek's own page
 *     loading one conversation with GET /api/v0/chat/history_messages?chat_session_id=<id>
 *     — the same request the page makes when a user opens a past conversation by
 *     hand, which is the same footing the live leg stands on;
 *  2. several mutually independent open-source exporters request that same route
 *     with that same query key.
 * So detailPath / detailUrl are now filled in, `partial` is gone, and DeepSeek
 * moved from BACKFILL_LIST_ONLY_PLATFORMS to BACKFILL_SUPPORTED_PLATFORMS.
 *
 * 🔴 The half-leg **mechanism** stays, and this change did not touch it: `partial`
 *    and 'detail-unsupported' are still how a platform with a sourced list and an
 *    unsourced body is written down (Perplexity is in exactly that state). What
 *    changed is one platform's facts, not the shape of the table.
 *
 * 🔴 **What W8 did NOT verify**, and therefore must not be written down as known:
 *    whether that endpoint pages, or truncates a long conversation. Not one of the
 *    reviewed implementations pages it, and this change adds no paging. So for a
 *    very long conversation, "this response is the whole body" remains an
 *    assumption rather than a measurement. It is stated here, in DEEPSEEK_PLAN's
 *    provenance, and in this repository's privacy notes — never rounded into
 *    "the body is complete".
 */

import { PLATFORMS } from '../contract';
import type { DetailOutcome } from './types';

export const CHATGPT_LIST_PATH = '/backend-api/conversations';
export const CHATGPT_DETAIL_PATH = '/backend-api/conversation/';

/** How many rows one page holds. 28 is the common default page size for list APIs; 1000 rows ≈ 36 pages is still "cheap". */
export const DEFAULT_LIST_LIMIT = 100;

export interface EnumPage {
  ids: string[];
  /** The total the API gave us directly; null when it did not. */
  total: number | null;
  /**
   * 🔴 C26 · For cursor paging: the cursor of the next page.
   * `null` = **this page could not supply a cursor** (no cursor field in the
   * record) ⇒ the engine can only stop here and record enumCursor.truncated as
   * 'cursor-missing'.
   * `undefined` = this platform does not page by cursor at all (ChatGPT), and
   * the engine does not look at this field.
   */
  nextCursor?: number | null;
  /**
   * 🔴 W21 · For **opaque-token** paging (Grok): the cursor of the next page,
   * exactly as the API handed it over.
   *
   * `undefined`  = this platform does not page by token at all (DeepSeek,
   *                ChatGPT, Perplexity), and the engine does not look at it.
   * `null`       = the API answered and there is **no** next page — the last
   *                page. On these platforms that is the API's own termination
   *                signal and not an inference; see parseGrokListPage.
   * a string     = hand it back **unread**. This field exists so that the engine
   *                never has to interpret a cursor: it is never parsed as a
   *                number, never compared, never sorted, never trimmed into a
   *                "better" form. Whatever came back is what goes out again.
   *
   * Why this is separate from `nextCursor` rather than a widened type: the two
   * carry different *promises*. A numeric cursor can be min/max-ed and reasoned
   * about (DeepSeek's is the smallest seq_id on a page); an opaque token cannot
   * be reasoned about at all, and mixing them into one field invites exactly the
   * arithmetic that is meaningless on one of them.
   */
  nextToken?: string | null;
  /**
   * 🔴 C26 · Whether the API itself says there is another page.
   * `undefined` = the response carries no such signal ⇒ the engine **must not**
   * treat it as false; it can only stop and record 'has-more-missing'.
   */
  hasMore?: boolean;
  /**
   * 🔴 C26 · The newest update timestamp on this page, **as the raw number**,
   * with no timezone or format conversion. null when unavailable. It is recorded
   * here so that "updated_at is a number, not an ISO string" has an assertable
   * landing place — see the note on parseDeepSeekListPage.
   */
  newestUpdatedAt?: number | null;
}

export type ParseResult =
  | { ok: true; page: EnumPage }
  | { ok: false; detail: string };

// ---------------------------------------------------------------------------
// 🔴 C23 · The channel's capabilities as a **closed-set declaration**
//
// This section is the single place where "let the channel express POST" and
// "do not let the channel become a general-purpose proxy" meet: the method, the
// Content-Type and the top-level keys of the body are all written here as
// **enumerations**, and the content-script side (lib/backfill/tab-port.ts's
// checkBackfillRequest) compares against them one by one.
//
// Why the closed set has to live on the plan and not in the message: a message
// is "who is asking", a plan is "what we ourselves have written". Only the
// latter can serve as an allowlist — an allowlist that comes from the request
// itself is not an allowlist, it is self-certification.
// ---------------------------------------------------------------------------

/** 🔴 The permitted HTTP methods. A **closed set** of exactly these two. */
export const ALLOWED_BACKFILL_METHODS = ['GET', 'POST'] as const;
export type BackfillMethod = (typeof ALLOWED_BACKFILL_METHODS)[number];

/** 🔴 The permitted request Content-Types. A **closed set** of exactly one. */
export const ALLOWED_BACKFILL_CONTENT_TYPES = ['application/json'] as const;
export type BackfillContentType = (typeof ALLOWED_BACKFILL_CONTENT_TYPES)[number];

/**
 * 🔴 The byte ceiling for a request body. A backfill body can only ever be
 * something like "a page cursor / a conversation id", tens of bytes; 4 KiB is
 * already absurdly generous. This line is not about saving bandwidth, it is
 * about making "use this channel to ship something out" not hold up
 * volumetrically in the first place.
 */
export const MAX_REQUEST_BODY_BYTES = 4096;

/** Everything variable about one backfill request besides the URL. Omitted = GET with no body. */
export interface BackfillRequestInit {
  method: BackfillMethod;
  /** Only a POST segment may have one, and it must pass the bodyKeys closed-set check. */
  body?: string;
  contentType?: BackfillContentType;
}

/**
 * 🔴 The three things that must be declared at once when a segment (list /
 * detail) is a POST. Missing any one of them means it is not a legal POST
 * declaration — and therefore cannot be sent.
 */
export interface BackfillPostSpec<Args extends unknown[]> {
  contentType: BackfillContentType;
  /**
   * 🔴 The top-level keys the body may contain. **A closed set.**
   * The content script JSON.parses the body it received and compares top-level
   * keys one by one: one extra key rejects the whole request. Values may only be
   * string / number / boolean / null — no nested objects or arrays, so that "a
   * closed set of keys" cannot have an arbitrary tree hanging under it.
   */
  bodyKeys: readonly string[];
  /** The function that builds the body **ourselves**. On the production path the body can only come from here. */
  body(...args: Args): string;
}

/** The shape of the list segment's POST declaration. */
export type ListPostSpec = BackfillPostSpec<[origin: string, offset: number, limit: number]>;
/** The shape of the detail segment's POST declaration. */
export type DetailPostSpec = BackfillPostSpec<[origin: string, conversationId: string]>;

/**
 * 🔴 W21 · The placeholder a path may carry where the conversation id goes, for a
 * route whose id is a MIDDLE segment (`/a/{id}/b`). See detailPath's doc.
 */
export const DETAIL_ID_TOKEN = '{id}';

/**
 * Does a declared path template name exactly this pathname?
 *
 * Two forms are permitted, and nothing else:
 *  · no token ⇒ the W8 rules (a trailing '/' means "a directory the id is
 *    appended to", anything else means "exactly this one path");
 *  · one token ⇒ a fixed prefix + a fixed suffix with **one** non-empty segment
 *    between them, containing no '/'. `/a/{id}/b` matches `/a/X/b` and refuses
 *    `/a/b`, `/a//b`, `/a/X/Y/b` and `/a/X/b/extra`.
 *
 * Exported because the content script's allowlist has to make the same decision
 * as the plan describes — one rule, two callers, the same as postSpecFor.
 */
export function detailPathMatches(detailPath: string, pathname: string): boolean {
  const tokenAt = detailPath.indexOf(DETAIL_ID_TOKEN);
  if (tokenAt < 0) {
    return detailPath.endsWith('/') ? pathname.startsWith(detailPath) : pathname === detailPath;
  }
  const prefix = detailPath.slice(0, tokenAt);
  const suffix = detailPath.slice(tokenAt + DETAIL_ID_TOKEN.length);
  if (!pathname.startsWith(prefix) || !pathname.endsWith(suffix)) return false;
  const between = pathname.slice(prefix.length, pathname.length - suffix.length);
  return between.length > 0 && !between.includes('/');
}

/**
 * 🔴 W21 · The **second step** of one conversation's body.
 *
 * The facts a plan has to state, and nothing more:
 *  · `path`  — the path template (same `{id}` token as detailPath), which is what
 *    the content script compares against. A URL built for the second step is
 *    only sent when its pathname matches this exactly.
 *  · `url`   — the builder for that URL.
 *  · `body`  — **the only place a step-2 body is ever produced**, from the
 *    conversation id and step 1's raw response text. Returning null means "the
 *    first response is not something I can read" ⇒ the engine halts with
 *    'shape-changed' rather than sending an empty or guessed request.
 *  · `bodyKeys` / `bodyArrayKeys` — the closed set of top-level keys, with the
 *    keys whose value is an ARRAY OF STRINGS named separately. A key is one or
 *    the other, never both; an undeclared key, a nested object, an array on a
 *    scalar key and a scalar on an array key are all refused by the content
 *    script (see `MAX_BODY_ARRAY_ITEMS` and checkBackfillRequest).
 *  · `delayMs` — the wait between the two steps, drawn uniformly from
 *    `[min, max]` per conversation.
 */
export interface DetailStep2Spec {
  path: string;
  url(origin: string, conversationId: string): string;
  contentType: BackfillContentType;
  bodyKeys: readonly string[];
  bodyArrayKeys?: readonly string[];
  body(conversationId: string, firstResponseText: string): string | null;
  delayMs: { min: number; max: number };
}

/**
 * 🔴 W21 · The part of a POST declaration the content script's allowlist needs —
 * the Content-Type and the closed key sets — with `body()` left out.
 *
 * `postSpecFor` returns this rather than `ListPostSpec | DetailPostSpec` so that
 * the second detail step (which has its own `body` shape, and takes step 1's
 * response as an argument) can be described by the same allowlist check without
 * pretending to be a first-step declaration. The two existing spec types are
 * structurally assignable to it, so no existing caller changes.
 */
export interface PostKeySpec {
  contentType: BackfillContentType;
  bodyKeys: readonly string[];
  bodyArrayKeys?: readonly string[];
}

/**
 * 🔴 W21 · How many strings one declared array key may hold.
 *
 * The same reasoning as MAX_REQUEST_BODY_BYTES, expressed in items: a step-2 body
 * is "the ids this conversation's skeleton just named", so its length is a
 * property of the conversation, not of the request. The ceiling is far above any
 * plausible conversation and far below "use this channel to ship a payload out":
 * a Grok conversation would need 5000 message nodes to reach it. The count is
 * checked in addition to the byte ceiling, never instead of it.
 */
export const MAX_BODY_ARRAY_ITEMS = 5000;

export type BackfillSegment = 'list' | 'detail' | 'detail2';

/**
 * The result of a body parser. 'non-empty' is not an outcome to persist, it just
 * means carrying on into the existing sink; the two detail-empty-* values are
 * the named, observable, persistable ones C28 requires.
 */
export type DetailParseResult =
  | { ok: true; outcome: 'non-empty' }
  | { ok: true; outcome: DetailOutcome }
  | { ok: false; detail: string };

/**
 * 🔴 The **minimal declaration set** needed to backfill a platform. Fill in all
 * seven and it can be backfilled; miss one and it cannot.
 * Adding a platform = adding one of these structures, with no engine change.
 */
export interface BackfillEnumPlan {
  /** Must equal the id in lib/contract.ts's platform table, character for character. */
  platform: string;
  /** 1 · The list endpoint (a path, used for the content script's allowlist comparison; must be exact). */
  listPath: string;
  /**
   * 2 · The paging scheme (the URL part).
   * 🔴 C23: this is **no longer the same as "GET only"**. Omitting listPost ⇒
   *    this segment is a GET and the paging parameters are all in the query
   *    (which is how ChatGPT works, byte for byte unchanged); declaring listPost
   *    ⇒ this segment is a POST, the paging parameters are in the body, and this
   *    function only supplies the route.
   */
  listUrl(origin: string, offset: number, limit: number): string;
  /** 2b · 🔴 New in C23 (optional). Declaring it means the list segment is sent as a POST. */
  listPost?: ListPostSpec;
  /**
   * 2c · 🔴 New in C26 (optional) · **cursor paging**.
   *
   * Declaring it means this platform does **not** page by offset: the next page
   * needs a cursor read out of the previous page's content (for DeepSeek that is
   * `before_seq_id` = the smallest `seq_id` on the previous page). The engine then
   * takes the cursor branch, and on that branch listUrl is **never called** — but
   * the interface still requires it, because it is simultaneously the written
   * record of "what this platform looks like under offset semantics" and the
   * back-compat landing place.
   *
   * cursor === null ⇒ the first page (no cursor yet).
   */
  listCursorUrl?(origin: string, cursor: number | null, limit: number): string;
  /**
   * 🔴 W21 · **Opaque-token paging.**
   *
   * Declaring it means this platform does **not** page by an offset and does not
   * page by a number this code can interpret: the next page needs a cursor the
   * API itself produced (Grok's `nextPageToken`, which the sources describe as an
   * echo of the last `conversationId` on the page) and which we hand back
   * **unread** — `token` is exactly what the previous page returned, or null for
   * the first page.
   *
   * 🔴 Deliberately not folded into `listCursorUrl`: that one takes a `number`,
   *    and its callers may reason about the value (DeepSeek's is a minimum of
   *    `seq_id`s). Nothing here may reason about this one — a plan that declares
   *    `listTokenUrl` must not parse, compare, sort or reformat the token, and
   *    the engine never does either. The two modes are also mutually exclusive in
   *    one plan; the engine reads token mode first if a plan ever declares both.
   */
  listTokenUrl?(origin: string, token: string | null, limit: number): string;
  /** 3 · The shape test for the list response. Unrecognised ⇒ {ok:false}, and the engine halts with a trace. */
  parseListPage(text: string): ParseResult;
  /**
   * 4 · The body endpoint.
   * 🔴 C26: **null is allowed** — "the list segment is sourced, the body segment
   *    is not" is a real intermediate state (Perplexity is in it), and it must be
   *    writable rather than forcing someone to invent a body route.
   *    null ⇒ the content script allows no body URL for this platform (rule 4 of
   *    tab-port.ts), and the engine halts with 'detail-unsupported' **before
   *    issuing a single body request**.
   * 🔴 W8: a trailing '/' means "this is a **directory** the conversation id gets
   *    appended to" (ChatGPT), so the content script prefix-matches it; a path with
   *    no trailing '/' names **one endpoint** and is compared in full. That is what
   *    lets DeepSeek put its id in the query without turning
   *    `/api/v0/chat/history_messages` into a wildcard over every lookalike path.
   * 🔴 W21: a third form exists — a path carrying the literal token `{id}`
   *    (`DETAIL_ID_TOKEN`), for a route whose conversation id sits in the MIDDLE
   *    of the path rather than at its end or in its query (Grok:
   *    `/rest/app-chat/conversations/{id}/response-node`). It matches a fixed
   *    prefix + a fixed suffix with exactly one non-empty segment between them
   *    that may not itself contain a '/'. It is still a closed set: the prefix
   *    and the suffix are written down, and no wildcard is introduced. The same
   *    token is used by `detailStep2.path`, so both steps declare their shape the
   *    same way.
   */
  detailPath: string | null;
  /** 5 · conversation id → body URL. 🔴 C26: lives and dies with detailPath — either both or neither. */
  detailUrl: ((origin: string, conversationId: string) => string) | null;
  /**
   * 🔴 W8 (optional) · The **single query key** the body id travels in, when the id
   * is not a path segment (DeepSeek: `?chat_session_id=`).
   *
   * Absent ⇒ this plan's body URL carries **no query at all**, and the content
   *   script refuses one that does. (ChatGPT appends the id to the path, so every
   *   URL its own builder produces is query-free; this closes the hole rather than
   *   widening it, and changes no URL the plan itself builds.)
   * Declared ⇒ the content script additionally checks (tab-port.ts's
   *   checkDetailQuery): the pathname equals detailPath **in full**, the URL's query
   *   holds exactly this one key, once, with a non-empty value, and the whole URL is
   *   byte-identical to what `detailUrl(origin, value)` builds for that value.
   *   That last one is the same rule the POST body already follows — "the request
   *   may only carry something the plan's own builder produced" — and it is how the
   *   id's "legal shape" is expressed **without inventing an id alphabet**: whatever
   *   the list endpoint handed us is the legal value.
   *
   * 🔴 Deliberately one key, not a list: `detailUrl` takes one conversation id, so
   *    more than one query key is not something this declaration could describe.
   *    A platform that needs a second parameter needs a source for it first.
   */
  detailQueryKey?: string;
  /** 5b · 🔴 New in C23 (optional). Declaring it means the body segment is sent as a POST. */
  detailPost?: DetailPostSpec;
  /**
   * 🔴 W21 · **The optional second step of one conversation's body** (optional).
   *
   * Declaring it means: one conversation's body is not one request but two, both
   * on the platform's own origin and both inside this plan's declaration —
   * step 1 is `detailPath`/`detailUrl`/`detailPost` exactly as before, and step 2
   * is this. Declaring it changes nothing whatsoever for a plan that does not:
   * the second step only exists between step 1's shape gate and the sink.
   *
   * Why it is generic rather than Grok-shaped: the thing that varies is "the API
   * splits one conversation across two calls" (a skeleton call that names the
   * message ids, then a content call that takes those ids in the request body).
   * The engine therefore knows only the three facts it needs — there is a second
   * URL, there is a body built from step 1's own response, and there is a wait
   * between them. It never sees `responseIds`, never learns what an id is, and
   * never builds a step-2 body itself: `body()` is the plan's, the same rule the
   * POST body already follows (see `BackfillPostSpec.body`).
   *
   * 🔴 What is delivered is **step 2's** response, and only step 2's shape is
   *    checked against the platform row's `responseShape`. Step 1 has no shape
   *    gate of its own because it is not the artefact: if step 1 cannot be read,
   *    `body()` returns null and the leg halts with 'shape-changed'.
   * 🔴 Pacing and the daily cap count the **pair as one body**: the detail pacer's
   *    gate fires once (before step 1) and `detailToday.count` increments once.
   *    The wait between the two steps is `delayMs`, drawn per conversation from
   *    the run's injected randomness, and it is deliberately not a Pacer: it is
   *    an intra-pair gap, not an inter-request rate.
   */
  detailStep2?: DetailStep2Spec;
  /**
   * 🔴 C28 · An optional hook deciding whether a body's content is real; not
   * declared keeps the existing behaviour.
   *
   * This is not a body-URL or allowlist declaration, and no production plan
   * connects one yet. When some platform's body segment really has a source, this
   * is where (ultimately called by the body loop in engine.ts) to distinguish:
   *   · HTTP succeeded but the content is empty ⇒ detail-empty-unverified, stop
   *     and leave pending untouched;
   *   · reliable evidence that an empty conversation is legitimate ⇒
   *     detail-empty-confirmed, only then may it be treated as complete.
   *
   * 🔴 One source is known to have observed DeepSeek "returning empty on a second
   * visit to the same conversation", but two sources disagree on how that field
   * is spelled (`cacheControl` / `cache_control`), so the field name itself is
   * judged not found: it is not read here, and neither spelling is written down
   * as a known contract. If the raw payload becomes available later, the check
   * belongs in this parser's implementation; the engine.ts hook point is in
   * `runBackfill`'s body loop after `matchesResponseShape` and before building
   * the `CapturedFetch`. Even then, detailPath/detailUrl's provenance and the
   * allowlist boundary must be kept.
   */
  parseDetailPage?: (text: string) => DetailParseResult;
  /**
   * 6b · 🔴 C26 (optional) · When this plan **covers only half** the job, the
   * missing half is written here. The standard is identical to
   * UnsupportedBackfill (missing + one plain sentence for the user), because it
   * answers the same question: "what exactly do you still not know how to do?"
   * No such field = this plan is complete (both list and body work).
   */
  partial?: PartialBackfill;
  /** 7 · Provenance. Same standard as the credibility note in contract.ts. */
  provenance: string;
}

/** 🔴 C26 · The explicit record of a half leg: "the list can be listed, the bodies cannot be fetched yet". */
export interface PartialBackfill {
  /** What is still missing (one of the seven items above). */
  missing: readonly string[];
  /**
   * Catalog key for the one sentence shown to the user (see locales/en.yml).
   * 🔴 A key, not the sentence: this table is built once at module load, while
   *    the popup may render in either language, so the wording has to be resolved
   *    at paint time. No jargon, and it must not hint that the platform is being
   *    backfilled right now.
   */
  userNoteKey: string;
}

/**
 * 🔴 The **explicit record** of "this platform cannot be backfilled for now".
 *
 * Why it must be a piece of data rather than "it is not in the table":
 * "there is no history" and "we do not know how to read your history yet" are
 * completely different things to a user. The first is enumerating 0 rows; the
 * second must be a sentence saying "not supported yet". Without this record the
 * two look identical in the UI.
 */
export interface UnsupportedBackfill {
  platform: string;
  /** The part that already has a source. May be an empty array (= nothing found). */
  known: readonly string[];
  /** 🔴 What is still missing. Any one of them missing means it cannot be filled in — each corresponds to one of the seven items above. */
  missing: readonly string[];
  /** Catalog key for the one sentence shown to the user (the popup uses it). Same rule as PartialBackfill.userNoteKey. */
  userNoteKey: string;
}

// ---------------------------------------------------------------------------
// List parsers
// ---------------------------------------------------------------------------

/**
 * Parse one page of a conversation list (ChatGPT shape).
 * Strict: `items` must be an array and every element must have a string `id`;
 * `total` is only accepted when it is a non-negative integer, otherwise
 * total = null (⇒ progress takes the "total unknown" branch).
 *
 * 🔴 W10 · **What this parser's `total` is, and is not.** It is read as "the
 *    number this endpoint printed" and nothing more. It is not the size of the
 *    account (measured on a real account: `total = 901` with 7,391 distinct
 *    conversation ids returned by this same endpoint), so it must not be used to
 *    decide that the list has ended (the branch that did that is gone from
 *    engine.ts) and it stops being a denominator the moment the rows actually
 *    listed outnumber it (totalSource 'contradicted', lib/backfill/types.ts).
 *
 *    One field is deliberately **not** read here, though the response carries it:
 *    `has_missing_conversations`. Its semantics have no source — the only public
 *    declaration found is pionxzh's `chatgpt-exporter/src/api.ts:330-339`, where
 *    the author annotates it with his own "// what is this for?" — so acting on
 *    it would be inventing a meaning. Recorded here so that the omission is
 *    visibly a decision rather than an oversight.
 */
export function parseConversationListPage(text: string): ParseResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'list response is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, detail: 'list response is not a JSON object' };
  }
  const record = body as Record<string, unknown>;
  const items = record.items;
  if (!Array.isArray(items)) {
    return { ok: false, detail: 'list response has no `items` array' };
  }
  const ids: string[] = [];
  for (const item of items) {
    if (!item || typeof item !== 'object') {
      return { ok: false, detail: 'list item is not an object' };
    }
    const id = (item as Record<string, unknown>).id;
    if (typeof id !== 'string' || id.length === 0) {
      return { ok: false, detail: 'list item has no string `id`' };
    }
    ids.push(id);
  }
  const rawTotal = record.total;
  const total =
    typeof rawTotal === 'number' && Number.isInteger(rawTotal) && rawTotal >= 0 ? rawTotal : null;
  return { ok: true, page: { ids, total } };
}

/**
 * Parse one page of a DeepSeek conversation list.
 *
 * ## 🔴 This function is where "written knowing it might be wrong" lives
 * The field names recognised below are not from official documentation; they are
 * the cross-check of **four independent open-source implementations** by R25
 * (provenance on DEEPSEEK_PLAN.provenance). A reverse-engineered shape can be
 * changed by the platform at any time.
 * So every "cannot read it" must land on a **named outcome different from
 * "empty"**:
 *
 *  · cannot read `data.biz_data.chat_sessions` ⇒ `{ok:false}` ⇒ engine
 *    halt('shape-changed').
 *    🔴 **Never return `{ok:true, ids:[]}`** — that would make the backfill leg
 *    believe "this user has no conversations", mark enumCursor complete, and then
 *    quietly announce it had finished.
 *    "The API changed" and "you have no history" are completely different
 *    sentences to a user.
 *  · cannot read `seq_id` ⇒ the shape is still recognised, it simply **cannot
 *    page** ⇒ nextCursor=null, the engine stops at this page and records
 *    truncated='cursor-missing' (≠ enumeration finished).
 *  · cannot read `has_more` ⇒ hasMore=undefined, the engine records
 *    'has-more-missing' and stops, likewise **never** treating it as "no next page".
 *
 * ## `updated_at` is a **number**, not an ISO string
 * All three sources show it is a numeric timestamp. So here:
 *  · a finite number ⇒ taken as-is (no new Date, no unit conversion, no guessing
 *    seconds vs milliseconds);
 *  · **present but not a number** (say it turned into an ISO string) ⇒ judged
 *    `{ok:false}` immediately. That is deliberate: the field's type changing means
 *    the wire shape changed, and stopping on the spot to leave a trace beats
 *    "being lenient" with `new Date(string)`.
 *  · the whole field absent ⇒ tolerated (enumeration does not need it),
 *    newestUpdatedAt=null.
 */
export function parseDeepSeekListPage(text: string): ParseResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'deepseek list response is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, detail: 'deepseek list response is not a JSON object' };
  }
  // Envelope: data.biz_data (5 sources agree). 🔴 The top-level business code
  // (code / biz_code) has two sources in conflict, so **it is not read**.
  const data = (body as Record<string, unknown>).data;
  if (!data || typeof data !== 'object' || Array.isArray(data)) {
    return { ok: false, detail: 'deepseek list response has no `data` object (envelope changed?)' };
  }
  const biz = (data as Record<string, unknown>).biz_data;
  if (!biz || typeof biz !== 'object' || Array.isArray(biz)) {
    return { ok: false, detail: 'deepseek list response has no `data.biz_data` object (envelope changed?)' };
  }
  const bizRecord = biz as Record<string, unknown>;
  const sessions = bizRecord.chat_sessions;
  if (!Array.isArray(sessions)) {
    // 🔴 This is the line that guards "do not record an unknown as empty":
    //    no such array = the shape changed, not "there are no conversations".
    return { ok: false, detail: 'deepseek list response has no `data.biz_data.chat_sessions` array (shape changed?)' };
  }

  const ids: string[] = [];
  let minSeqId: number | null = null;
  let seqIdMissing = false;
  let newestUpdatedAt: number | null = null;

  for (const item of sessions) {
    if (!item || typeof item !== 'object' || Array.isArray(item)) {
      return { ok: false, detail: 'deepseek chat_sessions item is not an object' };
    }
    const record = item as Record<string, unknown>;
    const id = record.id;
    if (typeof id !== 'string' || id.length === 0) {
      return { ok: false, detail: 'deepseek chat_sessions item has no string `id`' };
    }
    ids.push(id);

    const seqId = record.seq_id;
    if (typeof seqId === 'number' && Number.isFinite(seqId)) {
      minSeqId = minSeqId === null ? seqId : Math.min(minSeqId, seqId);
    } else {
      // 🔴 One unreadable cursor makes the whole page's cursor untrustworthy:
      //    better to stop than to page wrong and skip conversations.
      seqIdMissing = true;
    }

    const updatedAt = record.updated_at;
    if (updatedAt !== undefined && updatedAt !== null) {
      if (typeof updatedAt !== 'number' || !Number.isFinite(updatedAt)) {
        return {
          ok: false,
          detail: 'deepseek chat_sessions item has a non-numeric `updated_at` (wire shape changed?)',
        };
      }
      newestUpdatedAt = newestUpdatedAt === null ? updatedAt : Math.max(newestUpdatedAt, updatedAt);
    }
  }

  const rawHasMore = bizRecord.has_more;
  return {
    ok: true,
    page: {
      ids,
      // 🔴 There is **no** source for a total field in DeepSeek's list response
      //    ⇒ total is always null ⇒ progress takes the "total unknown" branch and
      //    never shows a percentage. ids.length must not be passed off as a denominator.
      total: null,
      nextCursor: seqIdMissing ? null : minSeqId,
      hasMore: typeof rawHasMore === 'boolean' ? rawHasMore : undefined,
      newestUpdatedAt,
    },
  };
}

/**
 * Parse one page of a Perplexity conversation list.
 *
 * 🔴 All three independent sources in R26 consume this endpoint as "returns a
 * list"; this parser recognises only a top-level array and the `thread_id`
 * inside each element. No array or no conversation id ⇒ `shape-changed`; an
 * unknown response is never folded into an empty list. total / has_more / count
 * have no source, so they are not read here.
 *
 * 🔴 No time field is read here, deliberately. R26's three sources conflict on
 * the time field's name: `last_query_datetime` has a single source,
 * `inserted_at || created_at || new Date()` is the author's own three-way guess,
 * and the third source does not read a time at all; no name reaches two
 * independent sources, so not one of them is written into the parser.
 */
export function parsePerplexityListPage(text: string): ParseResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'perplexity list response is not JSON' };
  }
  if (!Array.isArray(body)) {
    return { ok: false, detail: 'perplexity list response has no top-level array (shape changed?)' };
  }

  const ids: string[] = [];
  for (const item of body) {
    if (!item || typeof item !== 'object' || Array.isArray(item)) {
      return { ok: false, detail: 'perplexity thread item is not an object' };
    }
    const threadId = (item as Record<string, unknown>).thread_id;
    if (typeof threadId !== 'string' || threadId.length === 0) {
      return { ok: false, detail: 'perplexity thread item has no string `thread_id`' };
    }
    ids.push(threadId);
  }
  return { ok: true, page: { ids, total: null } };
}

/**
 * Parse one page of a Grok conversation list.
 *
 * ## What is read, and what is deliberately not
 * Recognised: a top-level `conversations` array, and `conversationId` on each
 * element — the two things every source agrees on. `total` is **null**: no source
 * shows a total field on this endpoint, and inventing one would be inventing a
 * denominator (the same call parseDeepSeekListPage makes). `title`, `starred`,
 * `createTime` and `modifyTime` are present in the sources but are not read here:
 * the engine's enumeration needs ids, and this leg does not rewrite titles.
 *
 * ## 🔴 `nextPageToken`: absent/empty is the API's own "last page"
 * Unlike Perplexity, this endpoint's termination signal **is** in the response,
 * and every source treats it the same way: the token is carried forward while it
 * is a non-empty string, and the loop ends when it is missing or empty. So
 * `nextToken: null` here means "the API said there is no next page" — the
 * documented end of the list — and the engine sets `complete` on it. That is a
 * different claim from Perplexity's client-side inference, and the two must not
 * be worded the same way in the ledger.
 *
 * 🔴 A `nextPageToken` that is **present but not a string** (a number, an object)
 * is the drift case: the field exists and is no longer the thing we know how to
 * carry back. It returns `{ok:false}` — halt('shape-changed') — rather than
 * being rounded into `null` ("we reached the end"), which would turn a wire
 * change into a silently truncated account.
 */
export function parseGrokListPage(text: string): ParseResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'grok list response is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, detail: 'grok list response is not a JSON object' };
  }
  const record = body as Record<string, unknown>;
  const conversations = record.conversations;
  if (!Array.isArray(conversations)) {
    // 🔴 The line that guards "do not record an unknown as empty": no such array
    //    means the shape changed, **never** "this account has no conversations".
    return { ok: false, detail: 'grok list response has no `conversations` array (shape changed?)' };
  }

  const ids: string[] = [];
  for (const item of conversations) {
    if (!item || typeof item !== 'object' || Array.isArray(item)) {
      return { ok: false, detail: 'grok conversation item is not an object' };
    }
    const id = (item as Record<string, unknown>).conversationId;
    if (typeof id !== 'string' || id.length === 0) {
      return { ok: false, detail: 'grok conversation item has no string `conversationId`' };
    }
    ids.push(id);
  }

  const rawToken = record.nextPageToken;
  if (rawToken !== undefined && rawToken !== null && typeof rawToken !== 'string') {
    return { ok: false, detail: 'grok list response has a non-string `nextPageToken` (wire shape changed?)' };
  }
  const nextToken = typeof rawToken === 'string' && rawToken.length > 0 ? rawToken : null;
  return { ok: true, page: { ids, total: null, nextToken } };
}

/**
 * 🔴 W21 · The message ids a conversation's skeleton named, or null when the
 * skeleton is **not something this code can read**.
 *
 * This is the only place step 1's response is interpreted, and it is called from
 * the plan's own `detailStep2.body` — never from the engine. It returns null
 * (⇒ halt('shape-changed'), nothing sent, nothing settled) in three cases that
 * must not be conflated with "this conversation has no messages":
 *  · the body is not a JSON object, or has no `responseNodes` array;
 *  · a node is not an object, or carries no non-empty string `responseId`;
 *  · the array is **empty** — an empty skeleton is not evidence that the
 *    conversation is empty. Sending `{ responseIds: [] }` would come back as
 *    `{ responses: [] }`, pass the shape gate, be delivered as an empty
 *    conversation and settle its debt. That is this repository's least
 *    acceptable outcome (an unknown recorded as empty), so it is refused here
 *    rather than handed to the engine to interpret.
 *
 * The ids are returned **in the order the skeleton gave them**, duplicates
 * included: the request says "these are the ids you named", and de-duplicating
 * or reordering here would be this code making a claim about the tree that the
 * skeleton did not make.
 */
export function grokResponseIds(text: string): string[] | null {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return null;
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) return null;
  const nodes = (body as Record<string, unknown>).responseNodes;
  if (!Array.isArray(nodes) || nodes.length === 0) return null;
  const ids: string[] = [];
  for (const node of nodes) {
    if (!node || typeof node !== 'object' || Array.isArray(node)) return null;
    const id = (node as Record<string, unknown>).responseId;
    if (typeof id !== 'string' || id.length === 0) return null;
    ids.push(id);
  }
  return ids;
}

/**
 * 🔴 W21 · **Is this delivered body an empty answer, or a conversation with
 * nothing in it?** Only the first is knowable here, and it must not be settled.
 *
 * This is the C28 hook, and grok is the first production plan to declare one. The
 * situation it exists for is specific: step 2 is only ever sent when step 1 named
 * at least one message id, so a `responses` array that comes back **empty** is
 * "you named N ids and I returned no content for them" — not "this conversation
 * was always empty". Settling it would write a momentary empty reading into the
 * ledger as a fact.
 *
 * 🔴 Deliberately narrow: the only case that returns 'detail-empty-unverified' is
 *    the empty array. It does **not** try to judge whether individual responses
 *    "look empty" (no `message`, whitespace only) — a turn can legitimately carry
 *    no text (an attachment-only or non-text answer), and firing on that would
 *    stall the leg on real conversations. Everything else is 'non-empty' and is
 *    delivered whole; the raw body is authoritative and is never trimmed here.
 *
 * A body that is not an object, or has no `responses` array, is `{ok:false}` ⇒
 * halt('shape-changed'). The row's own shape gate normally catches that first;
 * this is the same decision made once more where the outcome is named.
 */
export function parseGrokDetailPage(text: string): DetailParseResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'grok detail response is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, detail: 'grok detail response is not a JSON object' };
  }
  const responses = (body as Record<string, unknown>).responses;
  if (!Array.isArray(responses)) {
    return { ok: false, detail: 'grok detail response has no `responses` array (shape changed?)' };
  }
  return responses.length === 0
    ? { ok: true, outcome: 'detail-empty-unverified' }
    : { ok: true, outcome: 'non-empty' };
}

// ---------------------------------------------------------------------------
// Platforms that can be backfilled
// ---------------------------------------------------------------------------

/** ChatGPT's body URL builder. Pulled out as a constant only so the back-compat detailUrl() can reuse it. */
const chatgptDetailUrl = (origin: string, id: string): string =>
  `${origin}${CHATGPT_DETAIL_PATH}${encodeURIComponent(id)}`;

export const CHATGPT_PLAN: BackfillEnumPlan = {
  platform: 'chatgpt',
  listPath: CHATGPT_LIST_PATH,
  listUrl: (origin, offset, limit) =>
    `${origin}${CHATGPT_LIST_PATH}?offset=${offset}&limit=${limit}`,
  parseListPage: parseConversationListPage,
  detailPath: CHATGPT_DETAIL_PATH,
  detailUrl: chatgptDetailUrl,
  // Provenance: **no external source**. This set is the "facts already researched"
  // handed to the previous worker by C11; it was not re-reviewed then, and this
  // change did not re-review it either (no network, no logged-in session).
  // So its credibility level is [unverified assumption], not 'from-source'.
  // It is still kept as "backfillable" because it already carries the full
  // seven-item declaration plus a shape test: a wrong assumption halts
  // ('shape-changed') with a trace immediately, and never becomes fake progress.
  provenance:
    'unverified-assumption · GET /backend-api/conversations?offset=&limit= with {items[].id, total};'
    + ' no external source-code provenance, no real end-to-end verification'
    + ' (this change forbids network access and a logged-in session)',
};

export const DEEPSEEK_LIST_PATH = '/api/v0/chat_session/fetch_page';

/**
 * 🔴 W8 · The single-conversation body route, and the one query key its id travels in.
 *
 * No trailing '/': this names one endpoint, not a directory. The content script
 * compares it in full (see BackfillEnumPlan.detailPath), so a lookalike path
 * cannot ride in on a prefix.
 */
export const DEEPSEEK_DETAIL_PATH = '/api/v0/chat/history_messages';
export const DEEPSEEK_DETAIL_QUERY_KEY = 'chat_session_id';

/**
 * 🔴 C26 + W8 · DeepSeek's conversation list **and** its single-conversation body.
 * C26 wrote the list segment only and left the body segment null; W8 (2026-09-14)
 * filled the body segment in — see "## W8 · the body segment" below for what
 * evidence exists, and for the one thing that is still **not** verified.
 *
 * ## How this cell went from "unknown" to "sourced"
 * Under C22 this said "the paging parameter names and the request method are
 * unknown, I have no source". Research ticket R25 (2026-08-17) supplied the
 * sources, and **every one of them is a multi-source cross-check**, not a single
 * witness:
 *
 *   list endpoint   GET https://chat.deepseek.com/api/v0/chat_session/fetch_page  · 4 sources
 *   page size       count                                                        · 2 sources
 *   page cursor     before_seq_id, value = the **smallest** seq_id on the previous page · 2 sources (same derivation)
 *   response env.   data.biz_data                                                · 5 sources
 *   list array      biz_data.chat_sessions                                       · 4 sources
 *   has next page   biz_data.has_more (boolean, sibling of the array)            · 3 sources
 *   record · id     id                                                           · 3 sources
 *   record · title  title                                                        · 2 sources
 *   record · time   updated_at 🔴 a numeric timestamp, not an ISO string          · 3 sources
 *   record · cursor seq_id (numeric)                                             · 2 sources
 *
 * The route among these corroborates the from-source evidence already registered
 * at lib/contract.ts:90-96 (deepseek-pp, Apache-2.0, commit 0a02c72b…, 2026-08-14).
 *
 * ## 🔴 Just as important: **nothing** that lacks a source was written in
 *  · `count`'s server-side maximum/default — not found ⇒ we only ever send our
 *    own DEFAULT_LIST_LIMIT, and **never** hardcode a number like 200 as the
 *    limit, and never assume "returned fewer than count ⇒ last page" (that would
 *    be treating an unknown as a known). The only signal for "last page" is has_more.
 *  · The complete response field list — no public complete JSON sample exists ⇒
 *    the parser recognises only the keys it needs, and extra keys are ignored
 *    (not treated as a shape change).
 *  · pinned / inserted_at / title_type / model_type — single source or defensive
 *    spelling ⇒ not read.
 *  · Whether the top-level business code is called code or biz_code — two sources
 *    conflict ⇒ **not depended on** (parseDeepSeekListPage never reads it once).
 *  · lte_cursor.updated_at / lte_cursor.pinned — one source and contradicting the
 *    other two ⇒ excluded.
 *  · page / offset / cursor / limit / page_size — absent from all five sources ⇒ excluded.
 *
 * ## W8 · the body segment: why it is no longer null, and what is still unknown
 *
 *  · `detailPath` / `detailUrl`: GET /api/v0/chat/history_messages?chat_session_id=<id>,
 *    with the id URL-encoded. Two independent kinds of evidence:
 *      ① a **real, logged-in browser session** (2026-09-13): DeepSeek's own page
 *         loads one conversation over XHR from exactly this route, with this query
 *         key — i.e. the request the page makes when a user opens a past
 *         conversation by hand;
 *      ② several mutually independent open-source exporters request the same route
 *         with the same query key (see the provenance string below).
 *    The live leg already matches this route through lib/contract.ts's deepseek row
 *    (pathHints '/api/v0/chat', requiredAnyPaths data.biz_data.chat_messages /
 *    data.biz_data.chat_session.id), so the backfill and live legs now stand on the
 *    same measured endpoint rather than two different ones.
 *    This is the reason C26 recorded for leaving it null — "no multi-source
 *    provenance" — and it has been removed by evidence, not by a decision to relax
 *    the standard.
 *
 *  · 🔴 **Not verified: whether this endpoint pages or truncates a long
 *    conversation.** None of the reviewed implementations pages it (one of them
 *    adds `&cache_version=0`; none sends a page/offset/cursor parameter), and W8
 *    adds no paging. So a very long conversation may come back as only its first
 *    part, and this plan has **no way to tell that response from a complete one** —
 *    no `total`, no `has_more`, no cursor is read from the body envelope here.
 *    Consequences, stated rather than hidden:
 *      · a truncated body would be *stored* and its debt *settled* — the engine's
 *        existing semantics ("shape is right, the sink saved it ⇒ done") cannot
 *        distinguish it, and inventing a truncation signal with no source would be
 *        exactly the kind of guess this file refuses to make;
 *      · what bounds the risk is that the live leg captures this same endpoint
 *        while the user browses, so the archive is not solely reliant on this leg,
 *        and the privacy notes carry the same caveat.
 *    If a raw payload from a genuinely long conversation ever becomes available,
 *    the place to settle this is a `parseDetailPage` (see its doc above): if the
 *    envelope turns out to carry a "there is more" field, read it there and report
 *    detail-empty-unverified rather than settling.
 *
 * ## ⚠️ Staleness and risk (written as it is, not dressed up)
 * The response shape has circumstantial evidence dated 2026-08-17; but **the
 * newest measured evidence for the paging parameters only goes to 2025-12**, and
 * that author mentions DeepSeek having shipped native conversation search ⇒ this
 * endpoint has very likely moved recently.
 * So this plan's credibility is [a multi-source reverse-engineered conclusion],
 * not an official contract: the consequences of being wrong are contained by
 * parseDeepSeekListPage's three named outcomes
 * (shape-changed / cursor-missing / has-more-missing), and never become fake progress.
 */
export const DEEPSEEK_PLAN: BackfillEnumPlan = {
  platform: 'deepseek',
  listPath: DEEPSEEK_LIST_PATH,
  // 🔴 Offset semantics **do not hold** on DeepSeek (page/offset/limit absent from
  //    all five sources). A listUrl is still given because the interface requires
  //    it, but the engine takes the listCursorUrl branch (declaring listCursorUrl
  //    ⇒ cursor paging) and listUrl is never called.
  //    It only fetches the first page — so if someone misuses it in future, what
  //    they get is the safe thing ("the first page"), not an invented offset parameter.
  listUrl: (origin, _offset, limit) => `${origin}${DEEPSEEK_LIST_PATH}?count=${limit}`,
  listCursorUrl: (origin, cursor, limit) =>
    cursor === null
      ? `${origin}${DEEPSEEK_LIST_PATH}?count=${limit}`
      : `${origin}${DEEPSEEK_LIST_PATH}?count=${limit}&before_seq_id=${cursor}`,
  parseListPage: parseDeepSeekListPage,
  // 🔴 W8 · The body segment: the id is not a path segment, so the route is one
  //    fixed endpoint (no trailing '/', ⇒ compared in full) and the id travels in
  //    the query. Both halves are named here because both are half of one fact.
  detailPath: DEEPSEEK_DETAIL_PATH,
  detailUrl: (origin, conversationId) =>
    `${origin}${DEEPSEEK_DETAIL_PATH}?${DEEPSEEK_DETAIL_QUERY_KEY}=${encodeURIComponent(conversationId)}`,
  detailQueryKey: DEEPSEEK_DETAIL_QUERY_KEY,
  provenance:
    'cross-source reverse-engineering (research ticket R25, 2026-08-17; four mutually '
    + 'independent open-source implementations agreeing) · '
    + 'GET /api/v0/chat_session/fetch_page?count=&before_seq_id= · 4 sources; '
    + 'count 2 sources; before_seq_id (= smallest seq_id on the previous page) 2 sources; '
    + 'data.biz_data 5 sources; chat_sessions 4 sources; has_more 3 sources; id 3 sources; '
    + 'seq_id 2 sources; updated_at (numeric) 3 sources. '
    + 'The route corroborates the from-source evidence at lib/contract.ts:90-96 '
    + '(deepseek-pp, Apache-2.0, commit 0a02c72b135bf2936e11aa78fd6136931ed65908, 2026-08-14). '
    + '🔴 Not official documentation; the newest measured evidence for the paging '
    + 'parameters only goes to 2025-12, so the endpoint may have changed. '
    + 'W8 (2026-09-14) then filled in the BODY segment: '
    + 'GET /api/v0/chat/history_messages?chat_session_id=<id> · (1) a real logged-in '
    + 'browser session on 2026-09-13, in which DeepSeek\'s own page loaded one '
    + 'conversation over XHR from exactly this route with this query key, plus '
    + '(2) several mutually independent open-source exporters requesting the same '
    + 'route with the same query key. '
    + '🔴 Unverified, stated here rather than left out: whether that endpoint pages or '
    + 'truncates a LONG conversation. No reviewed implementation pages it and W8 added '
    + 'no paging, so a truncated body would be indistinguishable from a complete one at '
    + 'this layer. Not official documentation, and W8 itself sent no request to '
    + 'deepseek.com (the 2026-09-13 observation was a real browser session, not this change).',
};

export const PERPLEXITY_LIST_PATH = '/rest/thread/list_ask_threads';

/**
 * 🔴 C27 · Perplexity's conversation list. **The list segment only**; the body
 * segment still has no source.
 *
 * Request facts (R26 research, 2026-08-17; this change does not go online, does
 * not log in, and sends no request to perplexity.ai), each with its independent
 * source count:
 *   POST /rest/thread/list_ask_threads?version=2.18&source=default  · 3 sources
 *   body.limit (page size)                                          · 3 sources
 *   body.offset (an integer offset; the client does offset += limit) · 3 sources
 *   body.ascending=false                                            · 3 sources
 *   body.search_term=""                                             · 3 sources
 *
 * 🔴 None of the three sources reads total / has_more / count, and none supplies a
 * reliable time field name. So the engine can only treat an empty page and a short
 * page as two **client-inferred** stopping points, and must not write them as the
 * API clearly saying "that is the end". They land on empty-page-inferred /
 * short-page-inferred respectively, and `complete` stays false; if it is ever
 * confirmed that the response does carry a termination field, the change is in
 * the engine's enumeration branch, at these two Perplexity length checks: read
 * that field, and set state.enumCursor.complete to true only when it is
 * explicitly false.
 *
 * 🔴 Channel B (GraphQL) needs a sha256 persisted-query hash that changes with
 * every front-end release and has no public stable value, so it is not used; the
 * Space / Collection threads routes have only paths, no parameters and no
 * response provenance, so they are not done either. The body segment has no
 * source, so detailPath/detailUrl must stay null.
 */
export const PERPLEXITY_PLAN: BackfillEnumPlan = {
  platform: 'perplexity',
  listPath: PERPLEXITY_LIST_PATH,
  // The parameters are all in the JSON body; the URL keeps only the verified
  // fixed version and source query.
  listUrl: (origin) => `${origin}${PERPLEXITY_LIST_PATH}?version=2.18&source=default`,
  listPost: {
    contentType: 'application/json',
    bodyKeys: ['limit', 'offset', 'ascending', 'search_term'],
    body: (_origin, offset, limit) => JSON.stringify({
      limit,
      offset,
      ascending: false,
      search_term: '',
    }),
  },
  parseListPage: parsePerplexityListPage,
  // 🔴 Body segment: no source whatsoever, and this change guesses neither path nor parameters.
  detailPath: null,
  detailUrl: null,
  partial: {
    missing: [
      'detailPath / detailUrl: this change has no source at all for the route and '
      + 'parameters of a single thread\'s body; the body segment is not guessed, so '
      + 'Perplexity only enters LIST_ONLY.',
    ],
    userNoteKey: 'platformNote.perplexity.partial',
  },
  provenance:
    'cross-source reverse-engineering (R26 research, 2026-08-17; three independent '
    + '"build the request yourself" implementations agreeing character for character; '
    + 'not official documentation, not verified end to end) · '
    + 'POST /rest/thread/list_ask_threads?version=2.18&source=default: 3 sources; '
    + 'body.limit: 3 sources; body.offset (client does offset += limit): 3 sources; '
    + 'body.ascending=false: 3 sources; body.search_term="": 3 sources. '
    + 'total / has_more / count: read by none of the three sources, so they cannot be '
    + 'treated as API fields; the GraphQL channel needs a sha256 persisted-query hash '
    + 'with no public stable value, so it is not used; the Space / Collection threads '
    + 'routes have only paths and no parameters or response provenance, so they are not '
    + 'used; the single-body segment has no source, so detailPath/detailUrl stay null.',
};

export const GROK_LIST_PATH = '/rest/app-chat/conversations';
/**
 * 🔴 W21 · The conversation **skeleton**: the ordered message tree (ids, sender,
 * parent links) with no content in it. `{id}` is the conversation id as a path
 * segment in the middle of the route, which is why this is written as a template
 * rather than as a prefix (see detailPath's doc).
 */
export const GROK_DETAIL_PATH = '/rest/app-chat/conversations/{id}/response-node';
/** 🔴 W21 · The conversation **content**: the second and final step of one body. */
export const GROK_DETAIL2_PATH = '/rest/app-chat/conversations/{id}/load-responses';
/** The one body key the second step carries, and the only thing that varies in it. */
export const GROK_STEP2_BODY_KEY = 'responseIds';
export const GROK_LIST_PAGE_SIZE_PARAM = 'pageSize';
export const GROK_LIST_TOKEN_PARAM = 'pageToken';
/**
 * 🔴 W21 · The wait between the skeleton and the content call, drawn per
 * conversation in `[2000, 5000]` ms.
 *
 * Both calls are the page's own calls — a real browser makes them back to back
 * when the user opens a conversation — so this is not a rate limit being
 * respected; it is the same "do not look like a script" reasoning the pacers use
 * (`lib/backfill/pace.ts`), applied inside one body. The floor is 2 s because
 * below that the pair is one burst; the ceiling is 5 s because the pair is still
 * **one** body against a 20 s+ inter-body interval, and a wider band would make a
 * single body's cost unbounded for no gain.
 */
export const GROK_DETAIL_STEP_DELAY_MS = { min: 2_000, max: 5_000 } as const;

const grokDetailUrl = (origin: string, id: string): string =>
  `${origin}/rest/app-chat/conversations/${encodeURIComponent(id)}/response-node`;
const grokDetail2Url = (origin: string, id: string): string =>
  `${origin}/rest/app-chat/conversations/${encodeURIComponent(id)}/load-responses`;

/**
 * 🔴 W21 · Grok's conversation list **and** its two-step conversation body.
 *
 * ## How this cell was filled in
 * Research ticket W20 (2026-09-14) reviewed the reference implementations and
 * recorded the endpoints; W21 re-opened those files and read the exact request
 * shapes, response field names and the live-verification note attached to them.
 * Every fact below has a source, and the source count is stated per fact:
 *
 *   list endpoint     GET /rest/app-chat/conversations            · 2 sources (+1 closed build)
 *   page size         query `pageSize`                            · 2 sources
 *   page cursor       query `pageToken` = the previous response's `nextPageToken` · 2 sources
 *                     🔴 the sources **disagree**: a third uses an integer `page`
 *   end of list       `nextPageToken` absent/empty, or an empty page · 3 sources
 *   list fields       conversations[].conversationId              · 3 sources (+ a title/createTime/modifyTime set)
 *   skeleton          GET  .../conversations/{id}/response-node   · 3 sources
 *   skeleton fields   responseNodes[].responseId                  · 3 sources
 *   content           POST .../conversations/{id}/load-responses  · 3 sources
 *   content body      { responseIds: [...] }                      · 3 sources
 *   content fields    responses[].{responseId,message,sender,createTime,parentResponseId,model} · 2-3 sources
 *   auth              session cookie only; no bearer, no CSRF, no custom header · 2 sources
 *   session url       https://grok.com/c/<id>                     · 2 sources
 *
 * The sources are public open-source exporters plus one closed-source store
 * build; they are not named here (this repository's public surface does not name
 * third-party exporters) and their exact files, lines and licences are in the
 * change report. One of them records a logged-in browser verification of these
 * three endpoints in 2026-07 followed by a real-account dogfood; that is
 * somebody else's measurement, **not** this repository's.
 *
 * ## 🔴 What is NOT verified, and what is done about it
 *  1. **The list cursor's shape.** Two sources pass `pageToken` back and one
 *     sends an integer `page` with a short-page stop; one of the two shapes is
 *     non-functional against the real backend and only a live session could say
 *     which. So the cursor is treated as **opaque** (`listTokenUrl`, `nextToken`)
 *     — never parsed, never compared — and the engine carries a **repeat-page
 *     guard**: a non-first page made up entirely of ids this enumeration has
 *     already seen is halt('shape-changed'), never "the end" (engine.ts).
 *  2. **Whether one `load-responses` call returns a whole long conversation.**
 *     The endpoint takes an array and has no page/cursor parameter in any
 *     source, so nothing here pages it; one source sends every id in one batch,
 *     another splits at 100 ids per request. This plan takes the **one batch**
 *     shape (the single-request form, and the one the live leg itself sees), so
 *     the pair is at most two same-origin requests per conversation. If the real
 *     server caps the array, a long conversation comes back partially and this
 *     plan has no signal that could tell — the same honest caveat DeepSeek's body
 *     segment carries. It is stated in the change report and in docs/privacy.md.
 *  3. **The skeleton's own query.** One source appends `?includeThreads=true` to
 *     the skeleton call and two do not. No query is sent here: the two-source
 *     form is the one that was verified end to end, and adding a third source's
 *     query on the strength of one witness would be guessing. Recorded, not
 *     adopted.
 *  4. **A workspace parameter.** The closed build's list call carries a
 *     `workspaceId` in some code paths. Neither plain-list source sends one, so
 *     none is sent here. Recorded, not adopted.
 */
export const GROK_PLAN: BackfillEnumPlan = {
  platform: 'grok',
  listPath: GROK_LIST_PATH,
  // 🔴 Offset semantics do not hold on Grok (no source uses offset/limit). A
  //    listUrl is still required by the interface; it produces the **first**
  //    page, so a future misuse gets the safe thing rather than an invented
  //    offset parameter. The engine takes the listTokenUrl branch.
  listUrl: (origin, _offset, limit) =>
    `${origin}${GROK_LIST_PATH}?${GROK_LIST_PAGE_SIZE_PARAM}=${limit}`,
  // 🔴 The token goes back **byte for byte** as it arrived: no trim, no parse,
  //    no comparison, no re-encoding beyond what URLSearchParams does to any
  //    query value. `token === null` is the first page and the parameter is
  //    omitted entirely (an empty `pageToken=` would be a second, different
  //    request, and no source shows the server treating it as "no cursor").
  listTokenUrl: (origin, token, limit) => {
    const params = new URLSearchParams({ [GROK_LIST_PAGE_SIZE_PARAM]: String(limit) });
    if (token !== null) params.set(GROK_LIST_TOKEN_PARAM, token);
    return `${origin}${GROK_LIST_PATH}?${params.toString()}`;
  },
  parseListPage: parseGrokListPage,
  detailPath: GROK_DETAIL_PATH,
  detailUrl: grokDetailUrl,
  detailStep2: {
    path: GROK_DETAIL2_PATH,
    url: grokDetail2Url,
    contentType: 'application/json',
    bodyKeys: [GROK_STEP2_BODY_KEY],
    bodyArrayKeys: [GROK_STEP2_BODY_KEY],
    body: (_conversationId, firstResponseText) => {
      const ids = grokResponseIds(firstResponseText);
      return ids === null ? null : JSON.stringify({ [GROK_STEP2_BODY_KEY]: ids });
    },
    delayMs: { min: GROK_DETAIL_STEP_DELAY_MS.min, max: GROK_DETAIL_STEP_DELAY_MS.max },
  },
  // 🔴 W21 · The C28 hook, and the first production plan to declare one. Step 2 is
  //    only sent when step 1 named at least one id, so an empty `responses` array
  //    means "no content for the ids you just named" — which is not evidence that
  //    the conversation is empty, and must not be settled as one.
  parseDetailPage: parseGrokDetailPage,
  provenance:
    'cross-source reverse-engineering (W20 research 2026-09-14; W21 re-opened the same files) · '
    + 'three implementations agree on the route family and on the two-step body '
    + '(GET .../conversations/{id}/response-node → POST .../conversations/{id}/load-responses '
    + 'with { responseIds: [...] }), and on the response envelopes '
    + '({ conversations: [{ conversationId }], nextPageToken } / '
    + '{ responseNodes: [{ responseId }] } / '
    + '{ responses: [{ responseId, message, sender, createTime, parentResponseId, model }] }). '
    + 'Cookie-only auth (no bearer, no CSRF, no custom header): 2 sources. '
    + '🔴 The LIST cursor disagrees between sources: two pass `pageToken` back '
    + '(echoing the previous response\'s nextPageToken) and one sends an integer `page` '
    + 'with a short-page stop. The cursor is therefore treated as an opaque token and the '
    + 'engine carries a repeat-page guard; which shape the real backend honours is NOT '
    + 'verified and needs a logged-in session. '
    + '🔴 Also NOT verified: whether one load-responses call returns a whole long '
    + 'conversation (no source shows any paging parameter on it, and one source splits the '
    + 'id list into batches of 100 while this plan sends one batch). A server-side cap would '
    + 'be indistinguishable here from a complete conversation. '
    + '🔴 Not adopted, recorded instead: one source appends `?includeThreads=true` to the '
    + 'skeleton call (the other two do not), and a closed-source build puts a `workspaceId` '
    + 'in some list calls (no plain-list source does). Neither is sent. '
    + 'This change issued no request to grok.com and had no logged-in session.',
};

const PLANS: readonly BackfillEnumPlan[] = [DEEPSEEK_PLAN, PERPLEXITY_PLAN, CHATGPT_PLAN, GROK_PLAN];

// ---------------------------------------------------------------------------
// 🔴 Platforms that cannot be filled in, or only half filled in — each says what
//    is missing
//
// Search scope (stated honestly): **only the provenance already recorded in this
// repository** was searched (the external source evidence notes on the rows of
// lib/contract.ts, left by earlier workers when network access was allowed, with
// commit / license / date).
// 🔴 This change **forbids issuing any real network request**, so no online search
//    was done either: no GitHub, no platform page, no logged-in session.
//    So every `missing` below means "**not found within the search scope
//    available**", not "does not exist" — the two must be written separately.
// ---------------------------------------------------------------------------

// 🔴 C26 · The deepseek row was **moved out**, not deleted and forgotten:
//    research ticket R25 supplied the list segment's multi-source provenance, so
//    it is now a plan on DEEPSEEK_PLAN (list segment complete + a partial that
//    names what the body segment still lacks).
//    The rule C22 set still holds: every row of the platform table lands on
//    exactly one of "has a plan" / "registered as temporarily impossible", and
//    tests/c22-enumplat.test.ts still watches it.
export const BACKFILL_UNSUPPORTED: readonly UnsupportedBackfill[] = [
  {
    platform: 'claude',
    known: [
      // lib/contract.ts:148-153 states the conversation-LIST route is
      // '/chat_conversations' (the one without the trailing slash), and 176-184
      // records that claude-chat-exporter (MIT, commit
      // 12da324dd158e9472251590d89d957fc767c0d85, 2026-08-08) requests
      // /api/organizations/<org>/chat_conversations/<uuid>.
      'listPath is sourced: /api/organizations/<org>/chat_conversations (lib/contract.ts:148-153, 176-184, quoting claude-chat-exporter, MIT, 2026-08-08)',
    ],
    missing: [
      'listUrl: where the <org> organization id in the route comes from has NO source — it is not in the page URL and has to be fetched from another endpoint first; we have no source for that endpoint, and inventing one would make the user believe history is being backfilled',
      'listUrl: the paging parameter names are unknown',
      'parseListPage: the conversation array / total field names of the list response are unknown (the chat_messages recorded in this repository belongs to the BODY route, not the list)',
    ],
    userNoteKey: 'platformNote.claude.unsupported',
  },
  {
    platform: 'kimi',
    known: [
      // lib/contract.ts:220-224 states the conversation-INDEX route is
      // '.../ListChats', and that the whole ChatService is a Connect-style unary
      // RPC: POST + JSON body.
      'listPath is sourced: .../ChatService/ListChats (lib/contract.ts:220-224)',
      'the request shape is sourced: Connect-style unary RPC = POST + JSON body (lib/contract.ts:226-229)',
    ],
    missing: [
      // 🔴 Before C23 this said "structural blocker: HttpPort only has a url, so
      //    even knowing the parameters we could not send it". The channel has been
      //    widened (listPost + BackfillRequestInit) and that wall is gone.
      //    But **the remaining two lost not one character** — the parameters still
      //    have no source, so it still cannot be filled in.
      'listPost.body: the paging cursor is in the body and its field name is unknown (the channel can send a POST now, but we do not know what to send; inventing one would make the user believe history is being backfilled)',
      'listPost.bodyKeys: same — the closed set of top-level keys must come from a source, it cannot be guessed',
      'parseListPage: the conversation array / total field names of the list response are unknown',
    ],
    userNoteKey: 'platformNote.kimi.unsupported',
  },
  {
    platform: 'gemini',
    known: [],
    missing: [
      'listPath: ❌ not found. Search scope = all of this repository\'s code and comments (the gemini row of lib/contract.ts has only /_/BardChatUi/data/batchexecute, one RPC endpoint, and no record of any "conversation list"); no online search was done (forbidden by this change)',
      // 🔴 C23: the POST half of the wall came down (see listPost), but
      //    batchexecute still has the other two halves:
      //    ① the RPC id has no source; ② the response is chunked text with a
      //    ")]}'" prefix, not JSON, so parseListPage needs a parser of its own —
      //    and writing that needs a source too.
      'listUrl / parseListPage: batchexecute packs the RPC id and parameters into the body, and the response is chunked ")]}\'"-prefixed text rather than JSON — the channel can send a POST since C23, but the RPC id and the chunked response format still have no source',
      'listPost.contentType: no source was found for batchexecute\'s request Content-Type (nothing recorded in this repository, and online search is forbidden by this change). This channel\'s Content-Type closed set currently holds only application/json (ALLOWED_BACKFILL_CONTENT_TYPES) — if it is not json, that closed set has to be widened **with a source** first',
      'detailUrl: same as above',
    ],
    userNoteKey: 'platformNote.gemini.unsupported',
  },
];

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/** The plan if it can be backfilled, otherwise null. */
export function backfillPlanFor(platform: string): BackfillEnumPlan | null {
  return PLANS.find((p) => p.platform === platform) ?? null;
}

/**
 * 🔴 C23 · The POST declaration for a segment. None means null (= the segment is a GET).
 * **The engine and the content script call this same function**, so "what is
 * sent" and "what is allowed" cannot tell different stories — the allowlist is
 * not a second copy, it is the same one.
 */
export function postSpecFor(
  plan: BackfillEnumPlan,
  segment: BackfillSegment,
): PostKeySpec | null {
  if (segment === 'list') return plan.listPost ?? null;
  if (segment === 'detail') return plan.detailPost ?? null;
  // 🔴 W21 · The second detail step is always a POST: its whole reason to exist is
  //    that it carries a body built from step 1. Declaring `detailStep2` *is* the
  //    POST declaration for that segment — there is no second switch for it.
  return plan.detailStep2 ?? null;
}

/** 🔴 The **only permitted** method for a segment. The plan decides, the request does not. */
export function expectedMethodFor(plan: BackfillEnumPlan, segment: BackfillSegment): BackfillMethod {
  return postSpecFor(plan, segment) ? 'POST' : 'GET';
}

/** The full request parameters for the list segment. No listPost ⇒ `{method:'GET'}`, byte-identical to C22. */
export function listRequestInit(
  plan: BackfillEnumPlan,
  origin: string,
  offset: number,
  limit: number,
): BackfillRequestInit {
  const spec = plan.listPost;
  if (!spec) return { method: 'GET' };
  return { method: 'POST', body: spec.body(origin, offset, limit), contentType: spec.contentType };
}

/** The full request parameters for the body segment. No detailPost ⇒ `{method:'GET'}`, byte-identical to C22. */
export function detailRequestInit(
  plan: BackfillEnumPlan,
  origin: string,
  conversationId: string,
): BackfillRequestInit {
  const spec = plan.detailPost;
  if (!spec) return { method: 'GET' };
  return { method: 'POST', body: spec.body(origin, conversationId), contentType: spec.contentType };
}

/** The record explicitly registered as "cannot be backfilled for now"; null when it is not on the list. */
export function unsupportedBackfillFor(platform: string): UnsupportedBackfill | null {
  return BACKFILL_UNSUPPORTED.find((u) => u.platform === platform) ?? null;
}

/**
 * 🔴 C26 · "Can this plan really get past conversation **bodies** back?"
 * A plan with only the list segment (Perplexity) must answer false here — it can
 * list conversations but cannot fetch a single body, and to the user the history
 * still has not been backfilled.
 */
export function canBackfillDetail(plan: BackfillEnumPlan): boolean {
  return plan.detailPath !== null && plan.detailUrl !== null;
}

/**
 * The ids of platforms that **can** have their history backfilled (in platform-table order).
 * 🔴 The test is "both list and body work", not "has a plan" — a platform that
 *    can only list conversations and cannot fetch bodies does **not** count as
 *    backfillable, or that popup line would be telling a lie.
 */
export const BACKFILL_SUPPORTED_PLATFORMS: readonly string[] = PLATFORMS
  .map((p) => p.id)
  .filter((id) => {
    const plan = backfillPlanFor(id);
    return plan !== null && canBackfillDetail(plan);
  });

/**
 * 🔴 New in C26 · The ids of platforms that **can only list conversations and
 * cannot fetch bodies yet**.
 * They are neither "can backfill history" nor "does nothing at all" — the
 * intermediate state has to have a name of its own, or it can only be rounded
 * into one of the two sides.
 */
export const BACKFILL_LIST_ONLY_PLATFORMS: readonly string[] = PLATFORMS
  .map((p) => p.id)
  .filter((id) => {
    const plan = backfillPlanFor(id);
    return plan !== null && !canBackfillDetail(plan);
  });

/** The ids of platforms that cannot be backfilled for now (in platform-table order). 🔴 Includes the list-only ones above. */
export const BACKFILL_UNSUPPORTED_PLATFORMS: readonly string[] = PLATFORMS
  .map((p) => p.id)
  .filter((id) => !BACKFILL_SUPPORTED_PLATFORMS.includes(id));

/** What the missing half of a half plan lacks (in platform-table order). For the popup. */
export const BACKFILL_PARTIAL: readonly (PartialBackfill & { platform: string })[] =
  BACKFILL_LIST_ONLY_PLATFORMS
    .map((id) => {
      const plan = backfillPlanFor(id)!;
      return plan.partial ? { platform: id, ...plan.partial } : null;
    })
    .filter((x): x is PartialBackfill & { platform: string } => x !== null);

// ---------------------------------------------------------------------------
// back-compat: ChatGPT's two URL builders. Existing tests and wiring still use them.
// ---------------------------------------------------------------------------

export function listPageUrl(origin: string, offset: number, limit = DEFAULT_LIST_LIMIT): string {
  return CHATGPT_PLAN.listUrl(origin, offset, limit);
}

export function detailUrl(origin: string, conversationId: string): string {
  return chatgptDetailUrl(origin, conversationId);
}
