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
 *    🔴 **But C23 itself filled that declaration in for no platform at all** —
 *    kimi and gemini still had "no source found / only half" list shapes at that
 *    point, and filling one in would have been inventing it. So in the `missing`
 *    lists below, the "structural blocker" entry is rewritten as "the channel can
 *    send it now, but the parameters still have no source".
 *    🔴 Later changes filled the channel in **for a platform at a time, by
 *    evidence**: DeepSeek's body segment in W8, Grok's list and two-step body in
 *    W21, and Kimi's list and body in W22 (the last from a logged-in probe, and
 *    the first plan whose paging cursor travels inside a POST body —
 *    `listTokenPost`).
 *    🔴 W29 filled Gemini's in as well, and it is the first whose **body is not
 *    JSON at all** (`listTokenForm` / `detailForm`, `FormPostSpec`): the RPC id
 *    and its arguments travel in one URL-encoded field, so the segment's freedom
 *    is closed down by a pinned query, a closed field set with the credential
 *    field left blank, and a structural check of the batch — not by the JSON
 *    rules, which that body cannot satisfy. Nothing was left in the `missing`
 *    list by that change: with gemini's cell filled in, the table below holds one
 *    entry, and it is claude's.
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
 *
 * ## W42 · DeepSeek's body no longer *assumes* it is whole; it is checked, and it
 * is refused when it is not
 *
 * W8's paragraph above left one open question and one hole. The question —
 * "does this endpoint page or truncate a long conversation?" — is **still not
 * answered by any source** after W42 re-opened every implementation that reaches
 * the route (see the W42 report §2: not one sends a paging parameter on the body
 * request; the complete body envelope carries no total, no has_more, no cursor;
 * the closest thing to evidence is one implementation's own record of a 46-turn
 * conversation fetched in one round trip, which an export missing twenty turns
 * would have produced identically).
 *
 * The hole is the part that mattered, and it is closed. W8 recorded it exactly:
 * "a truncated body would be *stored* and its debt *settled*". A DeepSeek body
 * went from the shape gate straight into the sink, through the same path as a
 * Gemini body the paging loop had walked to the end and proved whole, and the
 * archive could not tell the two apart. That is CLAUDE.md invariant 1 applied to
 * a body rather than to a count.
 *
 * **What W42 does about it, without guessing at the API.** The envelope carries a
 * tree: `chat_session.current_message_id` names the newest message of the branch
 * the user was looking at, and every message names its `parent_id`. Both names
 * are measured, not inferred (three independent bases, one of them this
 * repository's own fixture of the 2026-09-13 live shape). So
 * `parseDeepSeekDetailPage` walks that chain and archives the body only when the
 * walk closes at a root; when it does not, the conversation is refused with the
 * named failure `detail-tree-incomplete` — the same per-conversation shape Kimi's
 * `detail-paged-unsupported` and Gemini's `detail-too-long` already use — and
 * when the pointers are not there at all the leg halts `shape-changed` instead of
 * filing a verdict about a conversation it never checked.
 *
 * 🔴 **W42 chose that over the two alternatives, and the reasoning is in the W42
 *    report.** Briefly: a leg-level halt would have refused bodies that are
 *    *provably* whole (turning a known into an unknown), and marking the archive
 *    instead would have needed a field on a bundle schema the live leg shares and
 *    would still have settled a debt for a conversation that was never captured.
 *    Refusing per conversation is the only one of the three that keeps the
 *    "three consistent" rule this file already states — see failures.ts on why
 *    `detail-too-long` is per-conversation and `detail-unsupported` is a halt.
 *
 * 🔴 **The residual, named rather than left out:** the walk proves the response is
 *    closed under the visible branch, not under every discarded sibling branch
 *    (claude.ai's walk accepts the same limit), and it cannot catch a server that
 *    truncates the body *and* rewrites the boundary message's `parent_id` to
 *    `null` so the chain looks rooted. §5 of the W42 report lists the one
 *    measurement that would close it.
 */

import { PLATFORMS } from '../contract';
import {
  GEMINI_RPC_DETAIL,
  GEMINI_RPC_LIST,
  assembleDetailBundle,
  buildBatchExecuteBody,
  readDetailBundle,
  readDetailResponse,
  readListResponse,
} from '../gemini-rpc';
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
   * 🔴 W21 · For **opaque-token** paging (Grok, Kimi): the cursor of the next
   * page, exactly as the API handed it over.
   *
   * `undefined`  = this platform does not page by token at all (DeepSeek,
   *                ChatGPT, Perplexity), and the engine does not look at it.
   * `null`       = the API answered and there is **no** next page — the last
   *                page. On these platforms that is the API's own termination
   *                signal and not an inference; see parseGrokListPage and
   *                parseKimiListPage.
   * a string     = hand it back **unread**. This field exists so that the engine
   *                never has to interpret a cursor: it is never parsed as a
   *                number, never compared, never sorted, never trimmed into a
   *                "better" form. Whatever came back is what goes out again.
   *
   * 🔴 W22 · Where the token travels is the plan's declaration, not this field's:
   *    Grok puts it in the query (`listTokenUrl`), Kimi in the POST body
   *    (`listTokenPost`). The field above is only the value.
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

/**
 * 🔴 The permitted request Content-Types. A **closed set** — and, since W29, of
 * two: JSON, and the form encoding Gemini's `batchexecute` request uses.
 *
 * 🔴 The set grew **by evidence, not by convenience**: the form entry arrived
 *    with the plan that needs it (GEMINI_PLAN), and the W20 research recorded
 *    the request as `application/x-www-form-urlencoded;charset=UTF-8` for every
 *    source that builds one. Widening `ALLOWED_BACKFILL_CONTENT_TYPES` does not
 *    let a plan send anything: a segment's Content-Type must still equal the one
 *    its own spec declares, exactly, and a form body is checked by the rules on
 *    `FormPostSpec`, not by the JSON ones.
 */
export const ALLOWED_BACKFILL_CONTENT_TYPES = [
  'application/json',
  'application/x-www-form-urlencoded',
] as const;
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
 * 🔴 W22 · The shape of a list POST declaration whose body carries the **opaque
 * cursor** — `[origin, token, limit]`, with `token === null` on the first page.
 *
 * Why a separate type rather than a fourth argument on ListPostSpec: the two
 * describe different requests. A `ListPostSpec` is "page by offset, whatever that
 * means for this platform" (Perplexity: `offset` counts up by `limit`); this one
 * is "page by a cursor the API produced, handed back unread" (Kimi:
 * `{ page_size, page_token }`). Widening the first would have made every existing
 * builder's `offset` parameter mean two different things on one signature, and
 * would have let an offset-paging plan *receive* a token it must never interpret.
 *
 * 🔴 The token arrives here as `string | null` and is put into the body verbatim:
 *    no trim, no parse, no comparison, no re-encoding. The same rule the URL form
 *    (`listTokenUrl`) follows, expressed for a body.
 */
export type ListTokenPostSpec = BackfillPostSpec<[origin: string, token: string | null, limit: number]>;

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

/** 🔴 W31 · The placeholder a scoped plan's path templates carry where the archive scope sits. */
export const SCOPE_ID_TOKEN = '{org}';

/**
 * 🔴 W31 · **Does a scoped plan's path template name exactly this pathname?**
 *
 * Segment by segment, and nothing else: the same number of segments, every
 * literal segment equal character for character, and every `{…}` placeholder
 * matching exactly one non-empty segment. Two placeholders are recognised —
 * `{org}` (the scope) and `{id}` (the conversation) — and `scope` is compared
 * with the captured `{org}` segment rather than merely accepted, so a request
 * naming a different organization is refused rather than forwarded.
 *
 * Why segment-wise rather than the substring rule `detailPathMatches` uses: a
 * template with a placeholder in the **middle** cannot be checked by prefix and
 * suffix alone without letting the placeholder swallow a '/'. Splitting on '/'
 * makes "one segment" structural, which is the property that matters.
 *
 * A template with no placeholder at all is still handled correctly (it degenerates
 * to an exact pathname comparison), so this function is safe to call for any plan.
 */
export function scopePathMatches(
  template: string,
  pathname: string,
  scope: string | null,
): boolean {
  const wanted = template.split('/');
  const found = pathname.split('/');
  if (wanted.length !== found.length) return false;
  for (let i = 0; i < wanted.length; i += 1) {
    const part = wanted[i]!;
    const value = found[i]!;
    if (part === SCOPE_ID_TOKEN) {
      // 🔴 Compared, not accepted: the run's own scope is the only organization
      //    this plan may address. An unresolved scope (null) matches nothing.
      if (scope === null || scope.length === 0 || value !== scope) return false;
      continue;
    }
    if (part === DETAIL_ID_TOKEN) {
      if (value.length === 0) return false;
      continue;
    }
    if (part !== value) return false;
  }
  return true;
}

/**
 * 🔴 W31 · **Substitute the run's scope into a URL a plan built.**
 *
 * The one place a scope reaches a request. Returns null — never a URL with the
 * token still in it — when the plan needs a scope and there is none to
 * substitute: sending `/api/organizations/{org}/…` would be a request against a
 * path the platform does not have, and the leg says so instead.
 *
 * A plan that declares no `scopeInPath` is returned **byte-identical**, so no
 * existing platform's URL moves.
 */
export function applyScope(plan: BackfillEnumPlan, url: string, scope: string | null): string | null {
  if (!plan.scopeInPath) return url;
  if (!url.includes(SCOPE_ID_TOKEN)) return url;
  if (scope === null || scope.length === 0) return null;
  // 🔴 encodeURIComponent, because the value goes into a path segment: a scope
  //    carrying a '/' or a '?' would otherwise build a different request than the
  //    allowlist validated. A resolved uuid contains neither, so this changes
  //    nothing for the real value — it is here so that "we validated URL A and
  //    sent URL B" cannot happen.
  return url.split(SCOPE_ID_TOKEN).join(encodeURIComponent(scope));
}

/**
 * 🔴 W31 · Does this URL's query equal the plan's pinned key → value set, exactly?
 *
 * The same rule `formQueryMatches` applies to a form request, for a GET whose
 * query is a set of constants: every pinned key present exactly once with its
 * pinned value, and **no other key**. A fragment is a difference between the URL
 * that was checked and the URL that is fetched, and is refused by the caller.
 */
export function pinnedQueryMatches(
  pinned: readonly { readonly key: string; readonly value: string }[],
  url: URL,
): boolean {
  const names = new Set<string>();
  for (const name of url.searchParams.keys()) names.add(name);
  if (names.size !== pinned.length) return false;
  for (const { key, value } of pinned) {
    const values = url.searchParams.getAll(key);
    if (values.length !== 1 || values[0] !== value) return false;
  }
  return true;
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
 * 🔴 W29 · **A POST segment whose body is a form, not JSON.**
 *
 * Why this had to exist rather than "widen the JSON path": Gemini's
 * `batchexecute` request is `application/x-www-form-urlencoded` with the RPC id
 * and its arguments packed into one field as a **JSON string inside a JSON
 * array** (`f.req`), plus a second field carrying the page's XSRF token. Under
 * the JSON rule that body is not even parseable as an object, and under the
 * scalar-value rule its one meaningful field would be an opaque string of
 * arbitrary structure — i.e. the closed-set check would be checking nothing.
 *
 * So the freedom is squeezed the same way it is for JSON, one level down:
 *  · **the query is pinned, key by key and value by value** (`query` below) to
 *    exactly what the plan's own URL builder emits. The page-context wrapper
 *    adds the page's own tokens (`bl`, `f.sid`, `hl`, `_reqid`) *after* this
 *    check, never before it;
 *  · **the field names are a closed set** and every field other than the batch
 *    must be **empty**. That is what makes "the message may not carry a
 *    credential" checkable: the token field is present because the page's own
 *    request has it, and it is blank because only the page-context wrapper may
 *    fill it;
 *  · **the batch's structure is fixed**: one batch, one call, the four declared
 *    positions, and an rpcid from a closed set;
 *  · **the args are checked by the plan's own `checkArgs`**, which pins every
 *    element except the two opaque values the platform itself produced (a
 *    conversation id, a page cursor). Those two are handed over **unread**, the
 *    same rule the list and detail cursors already follow.
 */
export interface FormPostSpec {
  /** The literal that says this spec is a form. A `PostKeySpec` has no such field. */
  encoding: 'form';
  contentType: BackfillContentType;
  /** The form field names this body may carry. **A closed set.** */
  bodyKeys: readonly string[];
  /** The one field that carries the RPC batch. Every other field must be empty. */
  batchKey: string;
  /** The RPC ids the batch may name. **A closed set** (today: exactly one). */
  rpcids: readonly string[];
  /** The batch call's fixed fourth element (the envelope kind the platform marks it with). */
  batchKind: string;
  /**
   * The query the plan's own builder emits for this segment: an exact
   * key → value set, each exactly once. Anything else in the query is refused.
   */
  query: readonly { readonly key: string; readonly value: string }[];
  /** Structural check of the decoded args. Returns a sentence when refused, null when acceptable. */
  checkArgs(rpcid: string, args: unknown): string | null;
  /** The function that builds the body **ourselves**. The only body producer for this segment. */
  body(...args: unknown[]): string;
}

/** Is this spec the form kind? The one place the discriminant is read. */
export function isFormPostSpec(spec: PostKeySpec | FormPostSpec | null): spec is FormPostSpec {
  return spec !== null && (spec as FormPostSpec).encoding === 'form';
}

/**
 * 🔴 W29 · **One conversation's body can be more than one request, and how many
 * is not known before the first one** (Gemini's detail RPC pages with a
 * continuation token).
 *
 * Why this is a declaration of its own rather than a fourth step of
 * `detailStep2`: W21's second step is a *fixed* pair — step 1 names the ids,
 * step 2 fetches the content, two requests, always. This is a **loop whose
 * length the platform decides**, and the two differ in every property that
 * matters here: how many requests go out, when to stop, and what is delivered
 * (one response vs. every page).
 *
 * The engine knows only: there is a URL, there is a request built for a later
 * page from a token the plan produced, there is a way to read one page's raw
 * text, there is a wait between pages, there is a cap, and there is an assembler.
 * It never sees the token's value, never parses it, and never builds a body.
 *
 * 🔴 Pacing and the daily cap count the whole loop as **one body**: the detail
 *    pacer's gate fired once before the first page and `detailToday.count`
 *    increments once after the last. The wait between pages is `delayMs`, drawn
 *    per page from the run's injected randomness — an intra-body gap, not an
 *    inter-request rate, exactly as `detailStep2.delayMs` is.
 * 🔴 `maxPages` is a **cap, not a target**: reaching it with a token still in
 *    hand means this conversation is longer than this leg will fetch, and the
 *    engine records the named failure `detail-too-long` and stores nothing —
 *    never "the first N pages, called complete".
 */
export interface DetailPagesSpec {
  /** The URL every page of this conversation is sent to (the same one page 1 used). */
  url(origin: string, conversationId: string): string;
  /** The request for a later page. The plan's own builder, the only place such a body is produced. */
  nextInit(origin: string, conversationId: string, token: string): BackfillRequestInit;
  /**
   * One page's raw text → what to do next. The token travels back **unread**:
   * no trim, no parse, no comparison.
   */
  nextPage(text: string, conversationId: string): DetailPageStep;
  delayMs: { min: number; max: number };
  /** How many pages one conversation may cost before it is declared too long. */
  maxPages: number;
  /** Builds the delivered document from the pages, in order. */
  assemble(conversationId: string, pages: readonly string[]): string;
}

/**
 * What one page of a paged body says about the next one.
 *  · 'last'       the platform said there is no more (a null/absent token);
 *  · 'more'       there is a token — and the ids this page carried, so the
 *                 engine can tell a cursor that moved from one that did not;
 *  · 'unreadable' the page could not be read as this conversation's page at all.
 *                 A named reason, never "no more pages": a page that changed
 *                 shape must not look like the end of a conversation.
 */
export type DetailPageStep =
  | { kind: 'last' }
  | { kind: 'more'; token: string; responseIds: string[] }
  | { kind: 'unreadable'; reason: string };

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

/**
 * 🔴 W31 · `resolve` is not a backfill segment: it carries **no conversation
 * data at all**. It is the one request that answers "which organization is this
 * account using" (claude.ai), it is admitted by the allowlist only because a
 * plan declared it under `scopeInPath.resolvePath`, and it exists in this union
 * so that "what is sent" and "what is allowed" go through the same dispatch
 * rather than through a second, parallel list of permitted URLs.
 *
 * It is deliberately **not** on the `BackfillSegment` values the engine can ask
 * for a body of: `postSpecFor` answers null for it and `expectedMethodFor` says
 * GET, exactly as for any other bodyless segment.
 */
export type BackfillSegment = 'list' | 'detail' | 'detail2' | 'resolve';

/**
 * 🔴 W22 · What a body parser may say about a body whose **shape is recognised**.
 *
 *  · 'non-empty' — carry on into the existing sink. The ordinary case.
 *  · DetailOutcome (the two `detail-empty-*` values) — C28's empty-body receipts.
 *  · 'detail-paged-unsupported' — the response is real content, but it says there
 *    is **more of this conversation than it holds** (a non-empty next-page
 *    token) and this leg has no way to fetch the rest. It must not be archived as
 *    a complete conversation and its debt must not be settled; the engine records
 *    a failure under exactly this reason code and leaves the conversation
 *    un-archived (see the body loop in engine.ts, and lib/backfill/failures.ts,
 *    where the reason is on the closed set).
 *
 * 🔴 Why it is a third outcome rather than "shape-changed": the shape is not
 *    what changed — the response is exactly what this platform's row describes,
 *    and every field the row names is there. What changed is the *completeness*
 *    of what that one response holds, and for a long conversation that is a
 *    per-conversation fact, not a wire change. Reporting it as 'shape-changed'
 *    would halt the whole leg on a conversation that is merely long, and would
 *    say "the platform changed" about a platform that did not.
 */
export type DetailParseOutcome =
  | 'non-empty'
  | DetailOutcome
  | 'detail-paged-unsupported'
  /**
   * 🔴 W31 · The response is the platform's own tree, its shape is recognised,
   * and **the walk of the visible branch does not close**. The body is therefore
   * real content and may be missing messages, so it is not the conversation and
   * must not be archived.
   *
   * Two platforms declare it, and both are trees with a named current leaf:
   *  · claude.ai — walking up from `current_leaf_message_uuid` hits a parent the
   *    response does not carry (parseClaudeDetailPage);
   *  · 🔴 W42 · DeepSeek — walking up from `chat_session.current_message_id` along
   *    `parent_id` leaves the messages the response carries, revisits one, or
   *    starts at a leaf the response does not hold (parseDeepSeekDetailPage).
   *
   * A per-conversation fact, exactly like 'detail-paged-unsupported': every other
   * conversation in the same run is unaffected, so it takes the failure path and
   * the run carries on — never a halt of the leg. What it must **not** be is
   * 'shape-changed' (the shape is precisely what the row describes) or an
   * archived conversation (that would put a partial tree in the archive with
   * nothing marking it partial).
   *
   * 🔴 **DeepSeek, and only DeepSeek, draws a further line here.** When a tree
   *    pointer the walk has to read is not readable in the response — no numeric
   *    `chat_session.current_message_id`, a chat message that is not an object, a
   *    `message_id` that is not a number — that is a different fact and does not
   *    land here: parseDeepSeekDetailTree returns `{ok:false}` and the leg halts
   *    'shape-changed'. Saying "this conversation is incomplete" about a
   *    conversation whose branch was never walked would be a diagnosis with no
   *    evidence behind it, and one type change would turn into a leg's worth of
   *    them. claude.ai's parser does **not** follow that rule at its own leaf:
   *    parseClaudeDetailTree reports a missing `current_leaf_message_uuid` as this
   *    outcome and parseClaudeDetailPage passes it through unchanged, so this
   *    paragraph is not a description of 'detail-tree-incomplete' in general.
   */
  | 'detail-tree-incomplete';

/**
 * The result of a body parser. 'non-empty' is not an outcome to persist, it just
 * means carrying on into the existing sink; the detail-empty-* values are the
 * named, observable, persistable ones C28 requires, and
 * 'detail-paged-unsupported' is the W22 outcome for a body that is real but
 * explicitly incomplete. `{ok:false}` is "this is not a shape this plan knows".
 */
export type DetailParseResult =
  | { ok: true; outcome: DetailParseOutcome }
  | { ok: false; detail: string };

/**
 * 🔴 The **minimal declaration set** needed to backfill a platform. Fill in all
 * seven and it can be backfilled; miss one and it cannot.
 * Adding a platform = adding one of these structures, with no engine change.
 */
/**
 * 🔴 W31 · **The archive scope, as a position in a request path.**
 *
 * Some platforms put the account identifier in the path of every conversation
 * request (claude.ai: `/api/organizations/<org>/chat_conversations/<id>`), and it
 * is nowhere in the page URL. The plan's own URL builders therefore emit the
 * literal token `{org}` where the scope goes, and the engine substitutes the
 * run's scope into it (`applyScope`) — one substitution point, exactly as
 * `{id}` has one in `detailPath`.
 *
 * Why a template with a token rather than a second `listUrl` signature that takes
 * the scope: the scope would then have to be threaded through every caller of
 * every builder, and a plan that does not need it would still have to accept it.
 * The token keeps the six existing plans byte-identical and puts the fact in one
 * readable place.
 *
 * 🔴 What it does **not** relax. The allowlist still compares segment by segment
 *    and still refuses anything a plan did not write down: `scopePathMatches`
 *    requires the same number of segments, every literal segment to match
 *    character for character, and the captured scope to **equal the scope the
 *    page side resolved** — a request naming a different organization is refused,
 *    not forwarded. The token is a position, not a wildcard.
 *
 * 🔴 `resolvePath` is the third path this plan may use, and it is deliberately
 *    **resolution-only**: the one request that answers "which organization is
 *    this account using" (a GET with no query and no body). It is not the list
 *    path and not the body path, cannot be pointed at either, and cannot carry a
 *    query; see tab-port.ts's checkBackfillRequest.
 */
export interface ScopeInPathSpec {
  /** The list path template, with `{org}` where the scope sits. Compared segment by segment. */
  listPath: string;
  /** The body path template, with `{org}` and `{id}`. */
  detailPath: string;
  /** The resolution-only path: GET, no query, no body. Used by the resolver and by nothing else. */
  resolvePath: string;
  /**
   * Whether the token names the scope or, for a platform whose token is a
   * different account axis, something else. Today this is only ever true; it is
   * spelled out rather than assumed because the allowlist reads this field to
   * decide that `{org}` must equal the resolved scope, and a future platform
   * whose path carries a non-scope account segment must not silently inherit that
   * rule.
   */
  tokenIsScope: true;
}

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
  /**
   * 🔴 W22 · **Opaque-token paging, with the token in the POST body.**
   *
   * The same paging mode as `listTokenUrl`, for a platform whose list request is
   * a POST with a JSON body and whose cursor travels *inside that body* rather
   * than in the query (measured on Kimi: `POST .../FeedService/ListFeeds` with
   * `{ page_size, page_token }`).
   *
   * Declaring either one puts the engine in token mode — the two are the same
   * promise with a different transport for one parameter — and a plan may declare
   * at most one of them. This field is simultaneously the list segment's POST
   * declaration: there is no second switch, exactly as `detailStep2` is itself
   * the POST declaration for the second detail step.
   *
   * `token === null` is the first page. What goes in the body then is the plan's
   * decision, not the engine's — the engine hands over `null` and nothing else —
   * and it is written down in the plan's own provenance.
   */
  listTokenPost?: ListTokenPostSpec;
  /**
   * 🔴 W29 · **Opaque-token paging, with the token in a form body** (Gemini).
   *
   * The same paging mode as `listTokenPost` — declaring either puts the engine
   * in token mode and both reach the same repeat-page guard — for a plan whose
   * list request is a URL-encoded form rather than a JSON document. It is a
   * separate field rather than a widened `listTokenPost` for the reason
   * `FormPostSpec` gives: a form body's freedom has to be closed down by rules
   * that do not apply to a JSON body (a pinned query, an empty credential field,
   * a structural check of the batch), and folding the two into one declaration
   * would mean the JSON check silently accepting a body it cannot read.
   *
   * `token === null` is the first page; what the body carries then is the plan's
   * own decision, written out at its builder.
   */
  listTokenForm?: FormPostSpec;
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
   * 🔴 W29 · The body segment as a **form POST** (Gemini). Lives and dies with
   * detailPath/detailUrl like `detailPost`, and the same rule applies: a plan
   * declares at most one of the two, and `postSpecFor` reads them in one order.
   */
  detailForm?: FormPostSpec;
  /**
   * 🔴 W29 · The body segment is **paged**, and this is how (Gemini). See
   * `DetailPagesSpec`; a plan may declare this or `detailStep2`, never both.
   */
  detailPages?: DetailPagesSpec;
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
   * This is not a body-URL or allowlist declaration. It is where a platform's
   * body segment, once it really has a source, distinguishes:
   *   · HTTP succeeded but the content is empty ⇒ detail-empty-unverified, stop
   *     and leave pending untouched;
   *   · reliable evidence that an empty conversation is legitimate ⇒
   *     detail-empty-confirmed, only then may it be treated as complete.
   *   · 🔴 W22 · the response is real content but explicitly incomplete (a
   *     non-empty next-page token) ⇒ 'detail-paged-unsupported': a per-conversation
   *     failure, never an archived conversation. See DetailParseOutcome above.
   * Two production plans declare one today: grok (W21, the empty case) and kimi
   * (W22, the incomplete case).
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
  /**
   * 🔴 W31 · **This plan's request paths carry the archive scope** (see
   * ScopeInPathSpec). Declaring it means: the URL builders below emit `{org}`,
   * the engine substitutes `opts.scope` into every URL before it is sent, and the
   * allowlist matches the plan's own templates with that same scope.
   */
  scopeInPath?: ScopeInPathSpec;
  /**
   * 🔴 W31 · **Offset paging with no termination field** (claude.ai).
   *
   * The response is a bare array: no `has_more`, no `next_cursor`, no next-page
   * token, and no `total` worth reading. Two consequences, both of them
   * inferences, and both named as such:
   *  · **a short page ends the listing** — the same client inference Perplexity's
   *    three sources share, so it is recorded as `short-page-inferred` with
   *    `complete` left **false** (an inference must not be written as "we listed
   *    everything"); the empty page, which is a real observation, still sets
   *    complete on its own branch above;
   *  · **the repeat-page guard runs** on a non-first page. For a token plan the
   *    guard asks "did the cursor move"; here it asks the same question about the
   *    offset parameter the plan itself emits. A page whose ids this enumeration
   *    has already seen means the parameter was ignored, and hammering the same
   *    page forever while the ledger says it is advancing is the failure it
   *    prevents.
   *
   * 🔴 `total` is deliberately not read even as a hint: claude.ai's list response
   *    has no such field, and W10 already measured that a `total` this endpoint
   *    class of API prints is not the size of the account. parseClaudeListPage
   *    returns `total: null`, which is "the API gave us none", not zero.
   */
  listOffsetInferred?: true;
  /**
   * 🔴 W31 · **The page size this plan itself asks for.**
   *
   * Until this field, one number decided both the request and the ending: the
   * engine's `listLimit` (default 100) went into the URL's `limit=` *and* was the
   * value a short page was compared against. A plan whose sources name a
   * different page size could only get one of the two right — asking for 50 while
   * calling 50 rows "short" would end the listing one page early and record it as
   * an inference.
   *
   * An explicit `opts.listLimit` (a caller's own choice, used by tests) still
   * wins: this field is a default, not an override of the caller.
   */
  listPageSize?: number;
  /**
   * 🔴 W31 · **The exact query the body segment's own builder emits.**
   *
   * A fixed key → value set, each key exactly once, nothing else — the same rule
   * `FormPostSpec.query` applies to a form request, for the first platform whose
   * *GET* carries more than one parameter (claude.ai's tree request pins three).
   * It replaces, rather than widens, `detailQueryKey`: that field describes one
   * key carrying a **value**, and this one describes parameters that are
   * constants of the endpoint. A plan declares one or the other, and the allowlist
   * checks whichever it declares.
   */
  detailQueryPinned?: readonly { readonly key: string; readonly value: string }[];
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
 *    declaration found is one reference implementation's API client, where
 *    the author annotates it with their own "// what is this for?" — so acting on
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
 * 🔴 W42 · The **tree pointers** of one DeepSeek conversation body, as measured.
 *
 * Every name below is read out of a real response, not invented, and each has
 * more than one independent witness:
 *
 *  · `chat_session.current_message_id` — the message the user was last looking
 *    at, i.e. the leaf of the visible branch. Measured in a logged-in browser
 *    session (2026-09-13) and recorded in this repository's own fixture of that
 *    shape (tests/unsupported-transport.test.ts:141); also present in a later
 *    sanitized real capture (2026-08-24) and in two independent open-source
 *    implementations, one of which documents it as the way the visible mainline
 *    is reconstructed (walk back along `parent_id` to a root, then reverse; the
 *    discarded regenerated siblings are excluded).
 *  · `parent_id` — the link upward. `null` on the root of a branch.
 *  · `message_id` — the value `parent_id` and `current_message_id` point at.
 *
 * 🔴 The names are read **only here**, and only to answer the completeness
 *    question. They are not required by the platform row's shape gate
 *    (lib/contract.ts), so a response that stops carrying them still passes that
 *    gate — which is exactly why the walk below has its own answer for
 *    "the pointers are gone" that is not "this conversation is incomplete".
 */
export const DEEPSEEK_TREE_LEAF_KEY = 'current_message_id';
export const DEEPSEEK_TREE_PARENT_KEY = 'parent_id';
export const DEEPSEEK_TREE_MESSAGE_KEY = 'message_id';

/**
 * 🔴 W42 · **Can this one response prove it holds the whole visible branch?**
 *
 * The problem this answers, in the plan's own words before this change: "a very
 * long conversation may come back as only its first part, and this plan has **no
 * way to tell that response from a complete one**". A response carrying a page
 * parameter would have been the obvious instrument, and §2 of the W42 report
 * establishes that no reviewed implementation sends one and the measured envelope
 * carries no token to send — but the body is not opaque: it is a **tree** with a
 * named current leaf, the same structure claude.ai's body has, for which this
 * file already has a walk (`parseClaudeDetailTree`).
 *
 * So the check is that walk, applied to DeepSeek's own field names: start at
 * `current_message_id`, follow `parent_id` upward, and require that every step
 * resolves to a message the response carries and that the chain ends at a root
 * (`parent_id === null`). Anything else means the response is **not** the whole
 * visible branch.
 *
 * 🔴 **Why this detects the truncation the plan was worried about.** The two ways
 *    a long conversation could come back short both break the chain:
 *      · truncated to the OLDEST n messages ⇒ `current_message_id` (the newest)
 *        is not among the messages this response carries;
 *      · truncated to the NEWEST n ⇒ the chain upward leaves the messages.
 *    A response that really holds the branch ends at a root, and nothing else does.
 *
 * 🔴 **The two failure kinds are not the same fact, and the split is deliberate.**
 *    `parseClaudeDetailTree` folds every imperfection into one
 *    `detail-tree-incomplete`. Here they are separated by whether the *inputs of
 *    the check itself* are present:
 *      · `kind: 'unreadable'` — the response does not carry what the check reads
 *        (it is not JSON, has no `data.biz_data.chat_messages` array, has no
 *        `data.biz_data.chat_session` object, names no numeric
 *        `current_message_id`, carries a chat message that is not an object, or
 *        carries a non-numeric `message_id`). We therefore did **not** check this
 *        conversation, and saying "this conversation is incomplete" about a
 *        conversation we never checked would be a diagnosis with no evidence
 *        behind it — and, because the engine writes off the debt of a
 *        `detail-tree-incomplete` conversation, one upstream type change would
 *        write off every conversation in the run. The caller turns this into
 *        `{ok:false}` ⇒ halt('shape-changed'), a traced stop.
 *      · `kind: 'incomplete'` — the check's inputs are all there and the tree does
 *        not close: the leaf is missing from the array, a link leaves the array or
 *        revisits a node, two messages claim one `message_id`, or a **reached**
 *        message's `parent_id` is neither `null` nor a number. That is a
 *        per-conversation fact about this conversation, and the caller turns it
 *        into the named outcome `detail-tree-incomplete`. Note where the line
 *        falls: an id that cannot be **read** is `'unreadable'` above, while a
 *        link that cannot be **followed** is this one — the first means the node
 *        set was never built, the second means the walk ran and did not close.
 *
 * 🔴 The walk is bounded by the message count, so a response whose parent links
 *    form a cycle cannot loop forever. A cycle is not "complete" — it is a tree
 *    this code cannot read — so, like claude.ai's, it is reported as incomplete
 *    rather than as a root.
 *
 * 🔴 **This function's job is completeness. It reorders nothing and rewrites
 *    nothing.** What gets archived is the response body, byte for byte, exactly
 *    as every other body parser in this file leaves it: `parseDeepSeekDetailPage`
 *    reads the walk's verdict and nothing else, and no ordering decision here can
 *    change what a reader of the shard sees.
 *
 * ⚠️ **What a successful walk does and does not prove.** It proves the response is
 *    *closed under the visible branch*: every ancestor of the newest message is
 *    present and the chain reaches a root. It does not prove the response holds
 *    every *discarded* sibling branch (the same limit claude.ai's walk has, and
 *    the same one the archive accepts there), and it cannot detect a server that
 *    truncated the body **and** rewrote the boundary message's `parent_id` to
 *    `null` to make it look like a root. That residual is written down in
 *    DEEPSEEK_PLAN's provenance and is what §5 of the W42 report measures.
 */
export type DeepSeekDetailTreeWalk =
  | { ok: true }
  | { ok: false; kind: 'unreadable'; detail: string }
  | { ok: false; kind: 'incomplete'; detail: string };

export function parseDeepSeekDetailTree(text: string): DeepSeekDetailTreeWalk {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, kind: 'unreadable', detail: 'the detail response is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, kind: 'unreadable', detail: 'the detail response is not an object' };
  }
  const data = (body as Record<string, unknown>).data;
  if (!data || typeof data !== 'object' || Array.isArray(data)) {
    return { ok: false, kind: 'unreadable', detail: 'the detail response has no `data` object' };
  }
  const biz = (data as Record<string, unknown>).biz_data;
  if (!biz || typeof biz !== 'object' || Array.isArray(biz)) {
    return { ok: false, kind: 'unreadable', detail: 'the detail response has no `data.biz_data` object' };
  }
  const bizRecord = biz as Record<string, unknown>;
  const messages = bizRecord.chat_messages;
  if (!Array.isArray(messages)) {
    return { ok: false, kind: 'unreadable', detail: 'the detail response has no `data.biz_data.chat_messages` array' };
  }
  const session = bizRecord.chat_session;
  if (!session || typeof session !== 'object' || Array.isArray(session)) {
    return { ok: false, kind: 'unreadable', detail: 'the detail response has no `data.biz_data.chat_session` object' };
  }
  // 🔴 A type change is `unreadable`, not "incomplete": the same rule
  //    parseDeepSeekListPage applies to a non-numeric `updated_at`. It applies to
  //    every field this walk reads — the leaf below, and each message's own
  //    `message_id` further down. A body whose ids are strings is a body we did
  //    not walk, and saying "this conversation is incomplete" about it would be a
  //    diagnosis with no evidence, repeated for every conversation in the run.
  const leaf = (session as Record<string, unknown>)[DEEPSEEK_TREE_LEAF_KEY];
  if (typeof leaf !== 'number' || !Number.isFinite(leaf)) {
    return {
      ok: false,
      kind: 'unreadable',
      detail: 'the detail response names no numeric `chat_session.current_message_id`',
    };
  }

  const byId = new Map<number, Record<string, unknown>>();
  for (const message of messages) {
    // 🔴 A message this code cannot read is `unreadable`, not "incomplete" — the
    //    same rule as the leaf above and for the same reason. Filing it as
    //    "incomplete" would make one upstream type change (ids arriving as
    //    strings, or a message that is not an object) a per-conversation verdict
    //    for **every** conversation: the engine writes the debt off
    //    (`engine.ts`, 'detail-tree-incomplete') and never retries it. Nothing
    //    was walked, so "this conversation is incomplete" is not a fact this code
    //    may state. The chain checks below still are.
    if (!message || typeof message !== 'object' || Array.isArray(message)) {
      return { ok: false, kind: 'unreadable', detail: 'a chat message is not an object' };
    }
    const messageRecord = message as Record<string, unknown>;
    const messageId = messageRecord[DEEPSEEK_TREE_MESSAGE_KEY];
    if (typeof messageId !== 'number' || !Number.isFinite(messageId)) {
      return { ok: false, kind: 'unreadable', detail: 'a chat message carries no numeric `message_id`' };
    }
    // A duplicate id would make this map lose a message, and a walk over the
    // remainder would resolve a link to the wrong node. Two messages claiming one
    // identity is not a tree this code can read.
    if (byId.has(messageId)) {
      return { ok: false, kind: 'incomplete', detail: 'two chat messages carry the same `message_id`' };
    }
    byId.set(messageId, messageRecord);
  }
  if (!byId.has(leaf)) {
    return {
      ok: false,
      kind: 'incomplete',
      detail: 'the current message is not among the chat messages this response carries',
    };
  }

  const visited = new Set<number>();
  let current: number | null = leaf;
  while (current !== null) {
    if (visited.has(current)) {
      return { ok: false, kind: 'incomplete', detail: 'the parent chain revisits a message' };
    }
    visited.add(current);
    const message = byId.get(current);
    if (!message) {
      return {
        ok: false,
        kind: 'incomplete',
        detail: 'the parent chain leaves the messages this response carries',
      };
    }
    const parentId = message[DEEPSEEK_TREE_PARENT_KEY];
    if (parentId === null) break; // a root: the branch is whole
    if (typeof parentId !== 'number' || !Number.isFinite(parentId)) {
      // 🔴 Deliberately **not** treated as a root. An unreadable link read as
      //    "the end of the branch" would turn a response this code cannot read
      //    into a complete conversation, which is the one outcome here that must
      //    never be reachable by accident.
      return { ok: false, kind: 'incomplete', detail: 'a chat message carries a `parent_id` this code cannot read' };
    }
    current = parentId;
  }
  return { ok: true };
}

/**
 * 🔴 W42 · DeepSeek's body segment's C28 hook.
 *
 * Before this change `DEEPSEEK_PLAN` declared none, so a DeepSeek body went from
 * the shape gate straight to the sink through the same path as a Gemini body the
 * paging loop had walked to the end and proved whole — and the archive could not
 * tell the two apart. "Does this response hold the whole conversation" was an
 * assumption, not a measurement, and a silently partial conversation would have
 * been stored and its debt settled.
 *
 * What each answer means, and why each one is a different fact to the user:
 *
 *  · `{ok:false}` — the shape this plan knows is not there: no
 *    `data.biz_data.chat_messages` array, or no readable tree to check with (a
 *    non-numeric leaf, a message that is not an object, a `message_id` that is
 *    not a number — see parseDeepSeekDetailTree). Halt with a trace. 🔴 This is
 *    the branch that keeps a **wrong field name or type** from turning into a
 *    quiet storm of wrong per-conversation verdicts: we cannot say "this
 *    conversation is incomplete" about a conversation we never checked.
 *  · `'detail-empty-unverified'` — the array is there and is **empty**. From this
 *    response alone, "this conversation has no messages" and "this response is a
 *    window with nothing in it" are not distinguishable, so the ambiguous case
 *    takes the unknown path (a receipt with `complete: false`) and never the empty
 *    one. Same trade-off, same wording, as parseClaudeDetailPage.
 *  · `'detail-tree-incomplete'` — the walk above ran and the visible branch does
 *    not close: the response is real content and is **not the conversation**. The
 *    engine records that named failure, stores nothing, and carries on with the
 *    next conversation — the same per-conversation shape as Kimi's
 *    `detail-paged-unsupported` and Gemini's `detail-too-long`.
 *  · `'non-empty'` — the walk closed at a root, so the response is closed under
 *    the visible branch and goes on to the sink whole, byte for byte. Note what
 *    this is *not*: it is not "the endpoint cannot truncate". It is "this
 *    response does not show the truncation the plan was worried about", which is
 *    strictly more than the plan could say before.
 */
export function parseDeepSeekDetailPage(text: string): DetailParseResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'deepseek detail response is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, detail: 'deepseek detail response is not a JSON object' };
  }
  const data = (body as Record<string, unknown>).data;
  if (!data || typeof data !== 'object' || Array.isArray(data)) {
    return { ok: false, detail: 'deepseek detail response has no `data` object (envelope changed?)' };
  }
  const biz = (data as Record<string, unknown>).biz_data;
  if (!biz || typeof biz !== 'object' || Array.isArray(biz)) {
    return { ok: false, detail: 'deepseek detail response has no `data.biz_data` object (envelope changed?)' };
  }
  const messages = (biz as Record<string, unknown>).chat_messages;
  if (!Array.isArray(messages)) {
    // The same line parseClaudeDetailPage draws: no such array is the drift case
    // and must be halted on, never read as a conversation with nothing in it.
    return { ok: false, detail: 'deepseek detail response has no `data.biz_data.chat_messages` array (shape changed?)' };
  }
  if (messages.length === 0) {
    return { ok: true, outcome: 'detail-empty-unverified' };
  }
  const walked = parseDeepSeekDetailTree(text);
  if (walked.ok) return { ok: true, outcome: 'non-empty' };
  if (walked.kind === 'unreadable') return { ok: false, detail: `deepseek detail body: ${walked.detail}` };
  return { ok: true, outcome: 'detail-tree-incomplete' };
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

/**
 * 🔴 W22 · The two field names a **next page of a conversation** could arrive
 * under on Kimi's detail response, and the rule for reading them.
 *
 * Measured 2026-09-14: five short conversations' detail responses carried **no**
 * page-token field of either spelling, and `messages` was the only top-level key.
 * Whether a long conversation pages is therefore **unverified**, and this is the
 * code that has to behave honestly in the case where it does.
 *
 * The rule, stated once so the parser and the tests cannot disagree:
 *  · absent, `null`, or `''`  ⇒ no signal. The body is what the platform has.
 *  · a non-empty string       ⇒ the platform is telling us there is MORE than
 *                               this response holds.
 *  · present but not a string ⇒ `null` here, i.e. "not readable by this code".
 *    The caller turns that into an unreadable shape, **not** into "no more
 *    pages": a token that changed type is not an absence of a token, which is
 *    the same rule parseKimiListPage applies to the list cursor.
 *
 * 🔴 Only the TOP LEVEL is read, and only these two spellings — deliberately. A
 *    scan of the whole tree for anything named like a cursor would fire on a
 *    message's own fields and mark complete conversations as truncated; and a
 *    third spelling would be a guess. The cost of that narrowness is named
 *    rather than hidden: if the real field is nested, or spelled differently,
 *    this code will read the body as complete. That is the same exposure the
 *    empty-body guard has, and the answer to both is a raw long-conversation
 *    payload, not a wider guess.
 */
export function kimiDetailNextToken(text: string): { kind: 'none' } | { kind: 'more' } | { kind: 'unreadable' } {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { kind: 'unreadable' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) return { kind: 'unreadable' };
  const record = body as Record<string, unknown>;
  for (const key of KIMI_DETAIL_TOKEN_KEYS) {
    // `in`, not `!== undefined`: a key present with the value `undefined` is
    // still the platform naming that field, and it must land on 'unreadable'
    // rather than being skipped as though it were absent. (A JSON body cannot
    // carry `undefined`, so this only matters for a hand-written caller — which
    // is exactly where a silent skip would be hardest to notice.)
    if (!(key in record)) continue;
    const value = record[key];
    if (value === null || value === '') continue;
    if (typeof value === 'string') return { kind: 'more' };
    return { kind: 'unreadable' };
  }
  return { kind: 'none' };
}

/**
 * 🔴 W22 · Kimi's detail response: a recognized body, and the one thing that can
 * make it **not** the whole conversation.
 *
 * Measured 2026-09-14: the response is `{ messages: [...] }`, and each message
 * carries `id, parentId, role, status, blocks, scenario, createTime, isGoal`.
 * Those keys are **not** required here: the row's own shape gate already asked
 * for `messages`, and re-asking would make a message-level change read as a
 * dropped conversation. What is required is that `messages` is an array, because
 * this function is the one that decides whether the body may be archived.
 *
 * What it returns, and why each one is a different fact to the user:
 *  · `{ok:false}` — no `messages` array. Shape drift ⇒ halt('shape-changed'),
 *    nothing stored, debt untouched.
 *  · 'detail-paged-unsupported' — the response says there is more of this
 *    conversation than it holds. The engine records a failure under that exact
 *    reason code and does **not** archive the body: storing a truncated
 *    conversation as a complete one is the mistake this outcome exists to
 *    prevent, and the failure list is where "this one is missing, and here is
 *    why" belongs (lib/backfill/failures.ts).
 *  · 'non-empty' — a body with no such field. It is delivered whole and its raw
 *    text stays authoritative: nothing here trims, reorders or re-serialises it.
 *
 * 🔴 An **empty** `messages` array is deliberately 'non-empty' here, not
 *    'detail-empty-unverified'. From this response alone, "this conversation has
 *    no messages" and "this response is a window with nothing in it" are not
 *    distinguishable — but the two mistakes are not equal in size: refusing to
 *    settle a conversation that really is empty leaves a debt pending and visible,
 *    while settling one that was merely windowed archives an empty file as a
 *    fact. The second is what this repository exists to avoid, so an empty array
 *    is handed on exactly like any other body, and the empty-*window* case is
 *    covered by the page-token rule above instead. Kimi is the platform where
 *    this trade-off is written down; grok's plan makes the opposite call for its
 *    own two-step route, where an empty answer after naming N ids really is a
 *    contradiction (parseGrokDetailPage).
 */
export function parseKimiDetailPage(text: string): DetailParseResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'kimi detail response is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, detail: 'kimi detail response is not a JSON object' };
  }
  if (!Array.isArray((body as Record<string, unknown>).messages)) {
    return { ok: false, detail: 'kimi detail response has no `messages` array (shape changed?)' };
  }
  const token = kimiDetailNextToken(text);
  if (token.kind === 'unreadable') {
    return {
      ok: false,
      detail: 'kimi detail response carries a page-token field this code cannot read (wire shape changed?)',
    };
  }
  return token.kind === 'more' ? { ok: true, outcome: 'detail-paged-unsupported' } : { ok: true, outcome: 'non-empty' };
}

/**
 * Parse one page of a Kimi conversation list (`FeedService/ListFeeds`).
 *
 * ## What is read, and what is deliberately not
 * Measured 2026-09-14: the response is `{ items, nextPageToken }`, and each item
 * is `{ type, chat: { id, name, messageContent, createTime, updateTime } }`. This
 * parser reads `items[].type`, `items[].chat.id` and `nextPageToken`, and nothing
 * else: `name` / `messageContent` / the two timestamps belong to the feed's
 * preview, not to the conversation this leg archives, and no source shows a total
 * on this endpoint ⇒ `total` is **null** (the same call parseDeepSeekListPage and
 * parseGrokListPage make; a made-up denominator is worse than none).
 *
 * ## 🔴 The item filter: a non-chat item is skipped, an unclassifiable one is not
 * The feed carries more than conversations, so `type` is read and only
 * `FEED_TYPE_CHAT` items contribute an id. That skip is a decision, and its two
 * halves are different:
 *  · an item whose `type` is a string other than `FEED_TYPE_CHAT` is **skipped**
 *    silently (counted in this parser's doc and the plan's provenance, never
 *    treated as an error): the feed legitimately holds other kinds of entry, and
 *    halting on one would stop the leg for a user who has, say, a starred
 *    document in their feed.
 *  · an item with **no usable `type`**, or a chat item with **no usable
 *    `chat.id`**, is `{ok:false}` — halt('shape-changed'). It is not skipped. An
 *    item this code cannot classify might be a conversation, and dropping it
 *    would silently lose that conversation from the archive while the leg
 *    reported success. "We could not read this row" and "this row is not a
 *    conversation" must not be the same outcome.
 *
 * ## 🔴 `nextPageToken`: absent/empty is the API's own "last page"
 * Measured: page 1 returned 3 items and a non-empty token; page 2 returned 2
 * items with the token **absent**, and the two pages did not overlap. So
 * `nextToken: null` here means "the API said there is no next page" — the
 * documented end of the list — and the engine sets `complete` on it, exactly as
 * on Grok. A `nextPageToken` that is present but **not a string** is the drift
 * case and returns `{ok:false}` rather than being rounded into `null` ("we
 * reached the end"), which would turn a wire change into a silently truncated
 * account.
 */
export function parseKimiListPage(text: string): ParseResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'kimi list response is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, detail: 'kimi list response is not a JSON object' };
  }
  const record = body as Record<string, unknown>;
  const items = record.items;
  if (!Array.isArray(items)) {
    // 🔴 The line that guards "do not record an unknown as empty": no `items`
    //    array means the shape changed, **never** "this account has no
    //    conversations".
    return { ok: false, detail: 'kimi list response has no `items` array (shape changed?)' };
  }

  const ids: string[] = [];
  for (const item of items) {
    if (!item || typeof item !== 'object' || Array.isArray(item)) {
      return { ok: false, detail: 'kimi feed item is not an object' };
    }
    const itemRecord = item as Record<string, unknown>;
    const type = itemRecord.type;
    if (typeof type !== 'string' || type.length === 0) {
      return { ok: false, detail: 'kimi feed item has no string `type` (shape changed?)' };
    }
    if (type !== KIMI_CHAT_FEED_TYPE) continue;
    const chat = itemRecord.chat;
    if (!chat || typeof chat !== 'object' || Array.isArray(chat)) {
      return { ok: false, detail: 'kimi chat feed item has no `chat` object (shape changed?)' };
    }
    const id = (chat as Record<string, unknown>).id;
    if (typeof id !== 'string' || id.length === 0) {
      return { ok: false, detail: 'kimi chat feed item has no string `chat.id`' };
    }
    ids.push(id);
  }

  const rawToken = record.nextPageToken;
  if (rawToken !== undefined && rawToken !== null && typeof rawToken !== 'string') {
    return { ok: false, detail: 'kimi list response has a non-string `nextPageToken` (wire shape changed?)' };
  }
  const nextToken = typeof rawToken === 'string' && rawToken.length > 0 ? rawToken : null;
  return { ok: true, page: { ids, total: null, nextToken } };
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
 * at lib/contract.ts:90-96 (a reference implementation, read 2026-08-14).
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
 *  · 🔴 W42 · **Whether this endpoint pages or truncates a long conversation is
 *    still not answered by any source — and this plan no longer has to assume the
 *    answer.** W8 wrote the hole down as "we have no way to tell a complete
 *    response from a truncated one". W42 read the body's measured envelope again
 *    and found a way to tell: the envelope carries no page token and no total
 *    (W42's research re-confirmed that across every implementation that reaches
 *    this route, and across two measured captures of real responses — see the
 *    provenance below), but it carries a **tree**: `chat_session.current_message_id`
 *    names the newest message of the branch the user was looking at, and every
 *    message names its `parent_id`. So the visible branch is a chain that can be
 *    walked, which is the same completeness proof claude.ai's body already gets
 *    (parseClaudeDetailTree), applied to DeepSeek's own field names.
 *    ⇒ `parseDetailPage` is `parseDeepSeekDetailPage`, and a body whose walk does
 *    not close is **not archived**: it is refused per conversation with
 *    `detail-tree-incomplete`, the named failure, exactly as Kimi's
 *    `detail-paged-unsupported` and Gemini's `detail-too-long` are.
 *    What is still unproven, and is written down rather than dressed up:
 *      · the walk proves the response is **closed under the visible branch** — every
 *        ancestor of the newest message present, the chain ending at a root. It does
 *        not prove the response holds every *discarded sibling* branch (claude.ai's
 *        walk accepts the same limit);
 *      · it cannot detect a server that truncated the body **and** rewrote the
 *        boundary message's `parent_id` to `null` to make it look like a root. That
 *        is the residual, and §5 of the W42 report lists the one measurement that
 *        would close it;
 *      · a body whose tree pointers are *absent* is not judged incomplete at all —
 *        it halts `shape-changed`, because "this conversation is partial" is not
 *        something this code may say about a conversation it never checked.
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
  // 🔴 W42 · DeepSeek's body segment's completeness check. Before this, the plan
  //    declared none and a possibly-partial body was stored and its debt settled.
  //    See parseDeepSeekDetailPage and parseDeepSeekDetailTree.
  parseDetailPage: parseDeepSeekDetailPage,
  provenance:
    'cross-source reverse-engineering (research ticket R25, 2026-08-17; four mutually '
    + 'independent open-source implementations agreeing) · '
    + 'GET /api/v0/chat_session/fetch_page?count=&before_seq_id= · 4 sources; '
    + 'count 2 sources; before_seq_id (= smallest seq_id on the previous page) 2 sources; '
    + 'data.biz_data 5 sources; chat_sessions 4 sources; has_more 3 sources; id 3 sources; '
    + 'seq_id 2 sources; updated_at (numeric) 3 sources. '
    + 'The route corroborates the from-source evidence at lib/contract.ts:90-96 '
    + '(a reference implementation, read 2026-08-14). '
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
    + 'no paging. '
    + '🔴 W42 (2026-09-19) re-read the whole body envelope and did NOT answer that '
    + 'question either — what it did is make the plan stop assuming it. Every '
    + 'implementation that reaches this route was opened again: not one sends a '
    + 'page/offset/cursor/limit parameter on the body request (the only query variation '
    + 'anywhere is one implementation\'s `&cache_version=0`, a cache-buster), and the '
    + 'complete key list of `data.biz_data` in two measured real responses is '
    + '{ chat_session, chat_messages, cache_control, cache_reset_at } — no total, no '
    + 'has_more, no next-page token, no cursor, so there is nothing to page with and '
    + 'nothing to read a completeness claim from. What the envelope DOES carry is a tree, '
    + 'and the field names are measured, not inferred: '
    + 'chat_session.current_message_id + messages[].{message_id, parent_id}. '
    + 'Evidence: (1) this repository\'s own record of the live shape of '
    + 'history_messages seen in a logged-in session on 2026-09-13 '
    + '(apps/extension/tests/unsupported-transport.test.ts:141); (2) a later sanitized '
    + 'real capture (2026-08-24) of a whole body, same names; (3) two independent '
    + 'open-source implementations, one of which documents exactly this walk '
    + '(current_message_id back along parent_id to a root) as the way the user-visible '
    + 'mainline is reconstructed. So parseDeepSeekDetailPage walks that chain: closed at '
    + 'a root ⇒ the response is archived; the chain leaves the messages, revisits one, '
    + 'or starts at a leaf the response does not carry ⇒ detail-tree-incomplete per '
    + 'conversation (nothing stored, debt not settled, run carries on); the pointers '
    + 'themselves unreadable ⇒ halt shape-changed, because a wrong field name must not '
    + 'become a quiet storm of "this conversation is incomplete" verdicts about '
    + 'conversations that were never checked. '
    + '🔴 Still unproven after W42, stated rather than dressed up: the walk proves the '
    + 'response is closed under the visible branch, NOT that the response is closed under '
    + 'every discarded sibling branch (the same limit claude.ai\'s walk accepts), and it '
    + 'cannot catch a server that truncates the body AND rewrites the boundary message\'s '
    + 'parent_id to null so the chain looks rooted. The one measurement that would close '
    + 'that residual is listed in the W42 report. '
    + 'Not official documentation, and W42 itself sent no request to deepseek.com '
    + 'either (the 2026-09-13 and 2026-08-24 observations were real browser sessions, '
    + 'not this change).',
};

export const PERPLEXITY_LIST_PATH = '/rest/thread/list_ask_threads';

/**
 * 🔴 C27 · Perplexity's conversation list. **The list segment only**, and W28
 * (2026-09-14) did not fill in the body segment either.
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
 * response provenance, so they are not done either.
 *
 * 🔴 The body segment is a **decision**, not a hole in the research. The route
 * that carries one thread's content is known — W28 (2026-09-14) read it out of
 * four independent reference implementations and the endpoint table extracted
 * from the site's own front-end bundle, and the extension's live-capture row now
 * registers it (lib/contract.ts:134-149) — but this plan does not spend it. The
 * sources disagree about that route's parameters, and not one of them
 * establishes whether a single response holds a whole long conversation. So
 * detailPath/detailUrl stay null and Perplexity stays LIST_ONLY: a wrong guess
 * here would not error, it would archive the first few turns of every
 * conversation while reporting success, which is the loss this project exists to
 * make impossible.
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
  // 🔴 Body segment: the route is known (see this plan's own doc block and
  // lib/contract.ts:134-149) but its parameter profile is not, and neither is the
  // completeness question. Neither path nor parameters are guessed here.
  detailPath: null,
  detailUrl: null,
  partial: {
    missing: [
      'detailPath / detailUrl: the route that carries one thread\'s content is '
      + 'known, but the sources disagree about its parameters and none of them '
      + 'establishes whether a single response holds a whole long conversation. The '
      + 'body segment is therefore not filled in — not guessed — so Perplexity only '
      + 'enters LIST_ONLY.',
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
    + 'used; the single-body route is known (W28, 2026-09-14: four reference '
    + 'implementations and the site\'s extracted endpoint table) but its parameters and '
    + 'its completeness for a long conversation are not, so detailPath/detailUrl stay '
    + 'null by decision.',
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

export const KIMI_LIST_PATH = '/apiv2/kimi.gateway.feed.v1.FeedService/ListFeeds';
/**
 * 🔴 W22 · The body of one conversation. No trailing '/': this names one endpoint,
 * not a directory, so the content script compares it in full. The conversation id
 * is **not** in this URL (it travels in the POST body), which is also why the
 * content script's query rule applies to it as "no query at all".
 */
export const KIMI_DETAIL_PATH = '/apiv2/kimi.gateway.chat.v1.ChatService/ListMessages';
/** The two body keys the list request carries — a closed set, measured. */
export const KIMI_LIST_PAGE_SIZE_KEY = 'page_size';
export const KIMI_LIST_PAGE_TOKEN_KEY = 'page_token';
/** The one body key the detail request carries — a closed set, measured. */
export const KIMI_DETAIL_CHAT_ID_KEY = 'chat_id';
/** The one `items[].type` value that means "this feed entry is a conversation". */
export const KIMI_CHAT_FEED_TYPE = 'FEED_TYPE_CHAT';
/**
 * 🔴 W22 · The two spellings a *detail* response's next-page token could arrive
 * under. See kimiDetailNextToken: only these, and only at the top level.
 */
export const KIMI_DETAIL_TOKEN_KEYS: readonly string[] = ['nextPageToken', 'next_page_token'];

/**
 * 🔴 W22 · Kimi's conversation list **and** its conversation body.
 *
 * ## How this cell was filled in
 * Not from third-party sources: the endpoints, the request bodies, the response
 * field names and the auth behaviour below were **observed in a logged-in
 * Chrome session on 2026-09-14** (the probe attached to this task's brief — list
 * plus short-chat detail; counts and field names only). The row that carries the
 * same evidence for the capture leg is lib/contract.ts's kimi row; this plan and
 * that row describe the same two measured routes, which is the point of putting
 * them side by side.
 *
 *   list endpoint     POST /apiv2/kimi.gateway.feed.v1.FeedService/ListFeeds      · measured
 *   list body         { page_size, page_token }                                   · measured
 *   list response     { items, nextPageToken }; items[].type / items[].chat.id     · measured
 *   item filter       only `type == "FEED_TYPE_CHAT"` carries a conversation       · measured
 *   page size used    3 in the probe; this plan sends DEFAULT_LIST_LIMIT (100)     · this
 *                     🔴 the probe measured page_size=3, NOT the value below. The
 *                        parameter's name is measured; its usable maximum is not.
 *   end of list       `nextPageToken` absent ⇒ last page (page 2 returned 2 items
 *                     and no token, with no overlap against page 1)               · measured
 *   detail endpoint   POST /apiv2/kimi.gateway.chat.v1.ChatService/ListMessages   · measured
 *   detail body       { chat_id }                                                 · measured
 *   detail response   { messages: [{ id, parentId, role, status, blocks, scenario,
 *                     createTime, isGoal }] }, top level `messages` only           · measured
 *   auth              `authorization: Bearer <token>` (the JWT this origin keeps in
 *                     localStorage under `access_token`), `x-msh-platform: web`,
 *                     `x-language: <locale>`, `content-type: application/json`;
 *                     cookie-only ⇒ HTTP 401 with a real refusal body (`code`,
 *                     `details`)                                                 · measured
 *   session url       https://www.kimi.com/chat/<id>                              · measured
 *
 * ## 🔴 What is NOT verified, and what is done about it
 *  1. **Whether a long conversation's detail response pages.** Five sampled
 *     conversations were short (2–3 messages) and carried no page-token field.
 *     The endpoint takes no paging parameter in anything measured, so nothing here
 *     pages it — and if a response ever says there is more (a non-empty
 *     `nextPageToken` / `next_page_token`), the plan **refuses the body** rather
 *     than archiving it: `parseKimiDetailPage` returns
 *     'detail-paged-unsupported', the engine records a failure under that reason
 *     code and does not settle the debt. A truncated conversation is never stored
 *     as a complete one, and the user is told which conversation it was.
 *  2. **Whether the list cursor is really this opaque token.** Measured on one
 *     account: page 2 came back with 2 items, no overlap and no token, which is
 *     exactly the shape of "the token worked, and then there was no more". The
 *     token is nevertheless carried **opaquely** (`listTokenPost`, `nextToken`),
 *     never parsed or compared, and the engine's **repeat-page guard** is what
 *     catches the opposite outcome — a backend that ignores the parameter and
 *     returns page 1 again halts as 'shape-changed' instead of being read as
 *     "nothing left".
 *  3. **How many conversations fit in one feed page.** `page_size` is measured as
 *     a parameter name; only 3 was observed. This plan sends DEFAULT_LIST_LIMIT
 *     because it is the same value the other plans send and the probe shows the
 *     server honours a page size below the client's own request. If the server
 *     caps it below 100, the pages simply come back smaller — the token, not the
 *     page length, is what ends this list.
 *  4. **The first page's `page_token`.** The measured body has both keys; what the
 *     probe sent for the token on page 1 was not recorded. This plan sends
 *     `page_token: ""`, and the reasoning is written out at the builder below.
 *  5. **The request headers' effect.** The two extra headers are what the page
 *     sends; whether the gateway requires them is not known. They are sent so the
 *     request looks like the page's own (see lib/platform-auth.ts), and the honest
 *     statement is that their necessity is unverified.
 */
export const KIMI_PLAN: BackfillEnumPlan = {
  platform: 'kimi',
  listPath: KIMI_LIST_PATH,
  // Offset semantics do not hold on Kimi (no offset/limit parameter was observed).
  // A listUrl is still required by the interface; it builds the **route only** —
  // the paging parameters live in the POST body — so a future misuse gets the
  // first page rather than an invented query parameter. The engine takes the
  // token branch (declaring listTokenPost ⇒ token mode) and never calls this.
  listUrl: (origin) => `${origin}${KIMI_LIST_PATH}`,
  /**
   * 🔴 The body, and the one thing about it that is a decision rather than a
   * measurement: **the first page carries `page_token: ""`**.
   *
   * The measured body has both keys, and the probe's first request was described
   * with both of them; what it sent for the token before it had one was not
   * recorded. So the choice here is between an empty string and omitting the key,
   * and the empty string is the one taken, for a reason that does not depend on
   * guessing: this is a protobuf service (`kimi.gateway.feed.v1.FeedService`) over
   * a JSON gateway, and in protobuf's JSON mapping an absent string field and an
   * empty string are **the same message** — the field's default value. Sending the
   * key explicitly therefore says nothing the server cannot already conclude, and
   * it keeps the request byte-shaped like the one that was measured.
   *
   * 🔴 The token itself is placed verbatim: no trim, no parse, no comparison, no
   *    re-encoding. `token === null` is the only special case, and it means "the
   *    first page" — never "the server gave us an empty token", which would be an
   *    absent `nextPageToken` and ends the list instead (parseKimiListPage).
   */
  listTokenPost: {
    contentType: 'application/json',
    bodyKeys: [KIMI_LIST_PAGE_SIZE_KEY, KIMI_LIST_PAGE_TOKEN_KEY],
    body: (_origin, token, limit) => JSON.stringify({
      [KIMI_LIST_PAGE_SIZE_KEY]: limit,
      [KIMI_LIST_PAGE_TOKEN_KEY]: token ?? '',
    }),
  },
  parseListPage: parseKimiListPage,
  // No trailing '/': one endpoint, compared in full by the content script. The id
  // is in the body, so this URL carries no query and none is permitted.
  detailPath: KIMI_DETAIL_PATH,
  detailUrl: (origin) => `${origin}${KIMI_DETAIL_PATH}`,
  detailPost: {
    contentType: 'application/json',
    bodyKeys: [KIMI_DETAIL_CHAT_ID_KEY],
    // 🔴 The id goes in **verbatim**, the same way every other plan's builder
    //    treats it: whatever the list endpoint handed us is the legal value (see
    //    BackfillEnumPlan.detailQueryKey), and re-encoding it here would make the
    //    request and the ledger disagree about which conversation it was.
    body: (_origin, conversationId) => JSON.stringify({ [KIMI_DETAIL_CHAT_ID_KEY]: conversationId }),
  },
  // 🔴 W22 · The C28 hook, used for its second purpose: a body that is real
  //    content AND explicitly incomplete must not be archived as a whole
  //    conversation. See DetailParseOutcome and parseKimiDetailPage.
  parseDetailPage: parseKimiDetailPage,
  provenance:
    'observed in a logged-in Chrome session on 2026-09-14 (this task\'s probe; list + short-chat '
    + 'detail; counts and field names only) · '
    + 'POST /apiv2/kimi.gateway.feed.v1.FeedService/ListFeeds with { page_size, page_token }; '
    + 'response { items, nextPageToken } with items[].{ type, chat.{ id, name, messageContent, '
    + 'createTime, updateTime } }; page_size 3 returned 3 items and a non-empty token, the next '
    + 'page returned 2 items with the token ABSENT and no overlap against page 1. '
    + 'POST /apiv2/kimi.gateway.chat.v1.ChatService/ListMessages with { chat_id }; response '
    + '{ messages: [{ id, parentId, role, status, blocks, scenario, createTime, isGoal }] }, '
    + 'five short conversations sampled. '
    + 'Auth: page sends authorization: Bearer <the localStorage access_token of this origin>, '
    + 'x-msh-platform: web, x-language: <locale>; cookie-only answers HTTP 401 with a refusal '
    + 'body ({ code, details }), which is why a 401 here is never read as "no conversations". '
    + 'Feed items whose type is not FEED_TYPE_CHAT are skipped, not treated as an error (the feed '
    + 'legitimately carries other entry kinds); an item whose type or chat.id cannot be read is a '
    + 'shape halt, never a silent skip. '
    + '🔴 Unverified and handled, not guessed: whether a LONG conversation\'s detail response '
    + 'pages — no paging parameter was observed and none is sent, so a response carrying a '
    + 'non-empty next-page token is refused and recorded as a failure '
    + '(detail-paged-unsupported) instead of being archived as a complete conversation. '
    + '🔴 Unverified and stated: the usable maximum of page_size (only 3 was observed), and '
    + 'whether the two extra request headers are required at all. '
    + 'This change issued no request to kimi.com; the 2026-09-14 observation was a real browser '
    + 'session run by the main session, not by this change.',
};

export const GEMINI_BATCHEXECUTE_PATH = '/_/BardChatUi/data/batchexecute';
/** The query key naming the RPC. Also how the two segments are told apart — they share one path. */
export const GEMINI_QUERY_RPCIDS = 'rpcids';
export const GEMINI_QUERY_SOURCE_PATH = 'source-path';
export const GEMINI_QUERY_RT = 'rt';
/** The path the page reports as its source. The page sends its own current path; this plan sends one fixed value. */
export const GEMINI_SOURCE_PATH_VALUE = '/app';
export const GEMINI_RT_VALUE = 'c';
/** The two form fields. The batch holds the RPC; the second is filled by the page-context wrapper, never here. */
export const GEMINI_FORM_FIELD_BATCH = 'f.req';
export const GEMINI_FORM_FIELD_AT = 'at';
/**
 * 🔴 W29 · The four query keys the **page's own** batchexecute request carries
 * that this plan deliberately does not build, because their values exist only in
 * the page (`WIZ_global_data`) or in the page's own state: the two bootstrap
 * tokens `bl` and `f.sid`, the UI language `hl`, and the page's request counter
 * `_reqid`.
 *
 * They are added by the page-context wrapper (lib/platform-auth.ts,
 * `createGeminiAuthorizedFetch`) **after** the allowlist has looked at the URL,
 * which is why the allowlist can pin the rest of the query byte for byte. Named
 * here so that "which keys may appear on this request" is one closed list read
 * by one file and its test, not a fact scattered across two.
 */
export const GEMINI_QUERY_KEY_BL = 'bl';
export const GEMINI_QUERY_KEY_F_SID = 'f.sid';
export const GEMINI_QUERY_KEY_HL = 'hl';
export const GEMINI_QUERY_KEY_REQID = '_reqid';
export const GEMINI_PAGE_QUERY_KEYS: readonly string[] = [
  GEMINI_QUERY_KEY_BL,
  GEMINI_QUERY_KEY_F_SID,
  GEMINI_QUERY_KEY_HL,
  GEMINI_QUERY_KEY_REQID,
];
/**
 * 🔴 W29 · **The list page size: 20, because that is the value the page itself
 * was measured sending.** The 2026-09-14 probe requested 20 and got 20 items
 * back. The parameter's usable maximum is still not known — no source states one
 * and the probe did not push it — so the measured value is what is sent, and the
 * token (not the page length) is what ends this list.
 */
export const GEMINI_LIST_PAGE_SIZE = 20;
/**
 * 🔴 W29 · **The detail page size: 10, the only value measured live.** The probe
 * asked for 10 per page and a 23-turn conversation needed a second page.
 * One source uses 100 instead, but that value was never observed against the
 * real server by this repository, and a page size the server silently caps is a
 * page size whose failures would look like a wire change. The loop below is what
 * makes a small page size cost very little: paging is expected, not exceptional.
 */
export const GEMINI_DETAIL_PAGE_SIZE = 10;
/**
 * 🔴 W29 · The wait between two pages of **one** conversation, drawn per page in
 * `[1000, 3000]` ms — the band this task's brief specifies.
 *
 * Both requests are of the kind the page itself makes while the user scrolls a
 * long conversation, so this is not a rate limit being respected; it is the same
 * "do not look like a script" reasoning `detailStep2.delayMs` carries, applied
 * inside one body. It stays an intra-body gap and never becomes a Pacer: the
 * body counts once against the detail pacer and once against the daily cap.
 */
export const GEMINI_DETAIL_PAGE_DELAY_MS = { min: 1_000, max: 3_000 } as const;
/**
 * 🔴 W29 · **How many pages one conversation may cost before it is refused.**
 *
 * The cap exists because the alternative is unbounded: the continuation token is
 * opaque and this leg may not reason about it, so "how long is this conversation"
 * is not a question anything here can answer in advance. 20 pages at the page
 * size above is 200 turns of one conversation, far beyond any conversation the
 * probe measured and still a bounded amount of work for one body.
 *
 * Reaching the cap **with a token still in hand** is the refusal, not the
 * truncation: the engine records `detail-too-long` and stores nothing (see
 * DetailPagesSpec.maxPages).
 */
export const GEMINI_MAX_DETAIL_PAGES = 20;
/** The one element of the batch the page marks as its generic envelope. */
export const GEMINI_BATCH_TAIL = 'generic';

/**
 * 🔴 W29 · The list RPC's arguments: `[pageSize, pageToken | null, [0, null, 1]]`.
 *
 * Evidence: **one source** for the exact three-element form (`adapter-gemini`'s
 * client builds `[20, pageToken, [0,null,1]]`), corroborated by the probe's own
 * measurement that a page size of 20 is honoured and that `payload[1]` is handed
 * back as the next token. The measured part is the behaviour; the literal tail is
 * the one-source part, and it is exactly why `checkGeminiListArgs` pins the tail:
 * if it is wrong in the world, the server's error is what the leg sees, and no
 * path in this repository invents a second shape.
 */
export function geminiListArgs(pageSize: number, token: string | null): unknown[] {
  return [pageSize, token, [0, null, 1]];
}

/**
 * 🔴 W29 · The detail RPC's arguments:
 * `[conversationId, pageSize, pageToken | null, 1, [0], [4], null, 1]`.
 *
 * Evidence: **one source** for the eight-element form. The probe measured the
 * **response** side of this call (turns at `payload[0]`, the continuation token
 * at `payload[1]`, and that feeding the token back advances the window), and it
 * measured the conversation id in the `c_`-prefixed canonical form — but the
 * request arguments themselves were not read off the wire, so this is the one
 * part of the plan that rests on a single witness. It is written down here
 * rather than buried: a wrong shape here does not archive anything wrong, it
 * makes the RPC fail, and a failed RPC is an HTTP status or an entry this
 * parser refuses — a traced halt, never a stored conversation.
 */
export function geminiDetailArgs(
  conversationId: string,
  pageSize: number,
  token: string | null,
): unknown[] {
  return [conversationId, pageSize, token, 1, [0], [4], null, 1];
}

/** Exact structural equality for the small literal tails above. */
function sameJson(a: unknown, b: unknown): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/** Structural check of the list RPC's decoded arguments. See geminiListArgs. */
export function checkGeminiListArgs(rpcid: string, args: unknown): string | null {
  if (rpcid !== GEMINI_RPC_LIST) return 'the batch names an rpcid this segment does not declare';
  if (!Array.isArray(args) || args.length !== 3) {
    return 'the list arguments are not the declared three-element array';
  }
  if (typeof args[0] !== 'number' || !Number.isFinite(args[0]) || args[0] <= 0) {
    return 'the list arguments carry no positive page size';
  }
  if (!(args[1] === null || typeof args[1] === 'string')) {
    return 'the list arguments carry a page token that is neither a string nor null';
  }
  if (!sameJson(args[2], [0, null, 1])) {
    return 'the list arguments carry a tail this plan does not build';
  }
  return null;
}

/** Structural check of the detail RPC's decoded arguments. See geminiDetailArgs. */
export function checkGeminiDetailArgs(rpcid: string, args: unknown): string | null {
  if (rpcid !== GEMINI_RPC_DETAIL) return 'the batch names an rpcid this segment does not declare';
  if (!Array.isArray(args) || args.length !== 8) {
    return 'the detail arguments are not the declared eight-element array';
  }
  if (typeof args[0] !== 'string' || args[0].length === 0) {
    return 'the detail arguments carry no conversation id';
  }
  if (typeof args[1] !== 'number' || !Number.isFinite(args[1]) || args[1] <= 0) {
    return 'the detail arguments carry no positive page size';
  }
  if (!(args[2] === null || typeof args[2] === 'string')) {
    return 'the detail arguments carry a page token that is neither a string nor null';
  }
  if (!sameJson(args.slice(3), [1, [0], [4], null, 1])) {
    return 'the detail arguments carry a tail this plan does not build';
  }
  return null;
}

/** The query each segment's URL carries — the plan's own, character for character. */
function geminiSegmentQuery(rpcid: string): readonly { readonly key: string; readonly value: string }[] {
  return [
    { key: GEMINI_QUERY_RPCIDS, value: rpcid },
    { key: GEMINI_QUERY_SOURCE_PATH, value: GEMINI_SOURCE_PATH_VALUE },
    { key: GEMINI_QUERY_RT, value: GEMINI_RT_VALUE },
  ];
}

/** The batchexecute URL for one RPC. The only place either segment's URL is built. */
function geminiRpcUrl(origin: string, rpcid: string): string {
  const params = new URLSearchParams();
  for (const { key, value } of geminiSegmentQuery(rpcid)) params.set(key, value);
  return `${origin}${GEMINI_BATCHEXECUTE_PATH}?${params.toString()}`;
}

/**
 * 🔴 W29 · One `MaZiqc` page.
 *
 * The end of the list is the token (absent/null) or an empty page — the two
 * signals the sources stop on and the probe confirmed (`payload[1]` was a string
 * on page 1 and the second page carried none). `total` is **null**: this endpoint
 * prints no total, and the same call every other plan makes is "no made-up
 * denominator" (a total we invented would become a progress percentage).
 */
export function parseGeminiListPage(text: string): ParseResult {
  const read = readListResponse(text);
  if (!read.ok) return { ok: false, detail: `gemini list response could not be read: ${read.reason}` };
  return { ok: true, page: { ids: read.ids, total: null, nextToken: read.nextPageToken } };
}

/**
 * 🔴 W29 · The detail segment's C28 hook.
 *
 * What it is handed is **not** one response — it is the bundle the paging loop
 * assembled (see DetailPagesSpec / `assembleDetailBundle`), so this parser's job
 * is the completeness claim: every page reads, every page is this conversation's,
 * and no page still carries a token. A bundle that fails any of those is a
 * partial conversation wearing a complete one's name, and it is refused here
 * rather than archived.
 *
 * 🔴 An empty-but-tokenless page stays 'non-empty': from one response, "this
 *    conversation has no turns" and "this response is a window with nothing in
 *    it" cannot be told apart, and the loop's token rule is what covers the
 *    window case. Same trade-off, same wording, as parseKimiDetailPage.
 */
export function parseGeminiDetailPage(text: string): DetailParseResult {
  const read = readDetailBundle(text);
  if (!read.ok) return { ok: false, detail: `gemini detail bundle could not be read: ${read.reason}` };
  return { ok: true, outcome: 'non-empty' };
}

/**
 * 🔴 W29 · One page of a **paged** conversation body: what the next one is.
 *
 * Three outcomes, and the middle one is why this is not "read the token":
 *  · `nextPageToken === null` ⇒ the platform says this is the last page;
 *  · a token ⇒ `more`, with this page's response ids so the engine can tell a
 *    cursor that moved from one that did not;
 *  · a token on a page that carried **no turns** is `unreadable`: that page says
 *    "there is more" while showing nothing, and following it would spin. It is
 *    named, not silently treated as the end.
 *
 * `conversationId` is not a hint: `readDetailResponse` refuses a page that names
 * a different conversation, so a stale token or a redirect cannot be
 * concatenated into this conversation's bundle.
 */
export function geminiDetailNextPage(text: string, conversationId: string): DetailPageStep {
  const read = readDetailResponse(text, conversationId);
  if (!read.ok) return { kind: 'unreadable', reason: read.reason };
  if (read.nextPageToken === null) return { kind: 'last' };
  if (read.pageIsEmpty) {
    return { kind: 'unreadable', reason: 'the page carried a continuation token and no turns' };
  }
  return { kind: 'more', token: read.nextPageToken, responseIds: read.responseIds };
}

/**
 * 🔴 W29 · Gemini's conversation list **and** its paged conversation body.
 *
 * ## How this cell was filled in
 * From the 2026-09-14 probe run by the main session in a logged-in Chrome
 * (names, positions and counts only) plus the research recorded at W20/W26. The
 * route, the envelope, the two rpcids and the positional paths of both payloads
 * were already in this repository (lib/gemini-rpc.ts, written against those
 * sources); what the probe added is the part no source had: that a real logged-in
 * page really fires `MaZiqc` on `/app`, what a real page size does, that the
 * detail RPC's pages are **disjoint** rather than overlapping, and that the
 * page's own tokens come from `WIZ_global_data`.
 *
 *   list endpoint     POST /_/BardChatUi/data/batchexecute?rpcids=MaZiqc…      · measured
 *   list args         [pageSize, pageToken | null, [0,null,1]]                  · 1 source
 *   list page size    20 (the probe requested it and got 20 items)              · measured
 *   list response     payload[1] = token, payload[2] = items (item[0] = c_…)     · measured
 *   list paging       page 2 had **0 overlap** with page 1                      · measured
 *   detail endpoint   POST /_/BardChatUi/data/batchexecute?rpcids=hNvQHb…      · measured
 *   detail args       [c_<id>, pageSize, pageToken | null, 1, [0], [4], null, 1] · 1 source
 *   detail page size  10 (a 23-turn conversation then needed a second page)      · measured
 *   detail response   payload[0] = turns, payload[1] = token | null              · measured
 *   detail paging     consecutive pages were **disjoint** (0 shared ids)         · measured
 *   envelope          `)]}'` guard, length-prefixed frames, lengths **+2**        · measured
 *   auth              cookies + `at` from WIZ_global_data; **without `at` the    · measured
 *                     server answers HTTP 400** — a refusal, never an empty page
 *   page tokens       SNlM0e → at, cfb2h → bl, FdrFJe → f.sid                     · measured
 *   id form           list `item[0]` and `turn[0][0]` are both `c_`-prefixed     · measured
 *
 * ## 🔴 The one decision worth reading before changing anything here
 * **The canonical conversation id is the `c_`-prefixed form**, and it is used in
 * both legs. The list endpoint returns ids in exactly that form, so a debt key
 * *is* one; the page URL (`/app/<bare id>`) is the one place the prefix is
 * absent, and it is therefore deliberately **not** what a live capture names a
 * conversation by — the live leg reads the id out of the response it just saw
 * (`turn[0][0]`) and carries it down as the authoritative identity. Two names for
 * one conversation would file it twice, which is the failure this file's
 * `detailQueryKey` note and the platform table's sessionIdPatterns note both
 * describe from their own side.
 *
 * ## 🔴 What is NOT verified, and what is done about it
 *  1. **The request arguments** (both segments) rest on **one source**; the
 *     response shapes are measured. A wrong argument shape fails the RPC, and
 *     every failure path here is a traced halt — the leg never invents a second
 *     argument shape, and `checkArgs` pins the tail it does not understand so a
 *     stuffed message cannot either.
 *  2. **The page's own tokens are read at request time, in the page**, by the
 *     page-context wrapper. Nothing here needs them, and no plan-built URL or
 *     body carries one.
 *  3. **Whether a conversation can be longer than the cap**: `maxPages` is a
 *     refusal, not a truncation, and the reason code is `detail-too-long`.
 *  4. **Whether one page may overlap the previous one.** The probe measured
 *     disjoint pages; the sources expect overlap. So the loop tolerates partial
 *     overlap (the ids are deduped by the reader, never by this file) and refuses
 *     only a page whose turns are **all** already seen — the same "the cursor did
 *     not advance" rule the list segment already carries.
 *  5. **Whether `source-path` must be `/app`.** The page sends its own current
 *     path; this plan sends one fixed value. Recorded, not proven.
 */
export const GEMINI_PLAN: BackfillEnumPlan = {
  platform: 'gemini',
  listPath: GEMINI_BATCHEXECUTE_PATH,
  // Offset semantics do not hold here: the cursor travels in the form body and
  // the page size lives there too. A listUrl is still required by the interface;
  // it builds the route and the fixed query, i.e. the first page's request, so a
  // future misuse gets the safe thing rather than an invented offset parameter.
  // The engine takes the token branch (declaring listTokenForm ⇒ token mode).
  listUrl: (origin) => geminiRpcUrl(origin, GEMINI_RPC_LIST),
  /**
   * 🔴 The body, and the two things about it that are decisions rather than
   * measurements:
   *
   *  · **the first page's token is `null`**, not `''` — the measured list
   *    response's first page simply had a token, and both W20's recorded argument
   *    list and the JSON form of the call spell "no token yet" as `null`. (Kimi's
   *    JSON body writes `''` instead, for the protobuf reason recorded there; the
   *    two are different platforms and each follows its own evidence.)
   *  · **the `at` field is built empty**, because only the page-context wrapper
   *    may fill it (see GEMINI_PAGE_QUERY_KEYS and createGeminiAuthorizedFetch).
   *    A body that carried a token from here would be a credential travelling in
   *    a message.
   *
   * 🔴 The token itself is placed verbatim: no trim, no parse, no comparison. The
   *    only special case is `null`, which means "the first page".
   */
  listTokenForm: {
    encoding: 'form',
    contentType: 'application/x-www-form-urlencoded',
    bodyKeys: [GEMINI_FORM_FIELD_BATCH, GEMINI_FORM_FIELD_AT],
    batchKey: GEMINI_FORM_FIELD_BATCH,
    rpcids: [GEMINI_RPC_LIST],
    batchKind: GEMINI_BATCH_TAIL,
    query: geminiSegmentQuery(GEMINI_RPC_LIST),
    checkArgs: checkGeminiListArgs,
    // 🔴 The page size is **this plan's own measured value**, not the engine's
    //    `listLimit` (whose cross-platform default is 100): 20 is the only value
    //    ever observed against the real server, and nothing observed says a larger
    //    one is honoured. A silent server-side cap would come back as a shorter
    //    page — which this endpoint's token, not the page length, would still
    //    terminate correctly — but sending a number no measurement supports is a
    //    guess this repository does not make. If a larger page size is ever
    //    measured, this is the line to change.
    body: (_origin, token) =>
      buildBatchExecuteBody(
        GEMINI_RPC_LIST,
        geminiListArgs(
          GEMINI_LIST_PAGE_SIZE,
          typeof token === 'string' ? token : null,
        ),
        '',
      ),
  },
  parseListPage: parseGeminiListPage,
  // 🔴 The two segments share one path, which is why `formSegmentFor` exists; the
  //    rpcid in each segment's pinned query is what tells them apart.
  detailPath: GEMINI_BATCHEXECUTE_PATH,
  detailUrl: (origin) => geminiRpcUrl(origin, GEMINI_RPC_DETAIL),
  detailForm: {
    encoding: 'form',
    contentType: 'application/x-www-form-urlencoded',
    bodyKeys: [GEMINI_FORM_FIELD_BATCH, GEMINI_FORM_FIELD_AT],
    batchKey: GEMINI_FORM_FIELD_BATCH,
    rpcids: [GEMINI_RPC_DETAIL],
    batchKind: GEMINI_BATCH_TAIL,
    query: geminiSegmentQuery(GEMINI_RPC_DETAIL),
    checkArgs: checkGeminiDetailArgs,
    // 🔴 The third argument is the continuation token and is `null` for the first
    //    page — see detailRequestInit. The id goes in **verbatim**, the same way
    //    every other plan's builder treats it: whatever the list endpoint handed
    //    us is the legal value, and re-encoding it would make the request and the
    //    ledger disagree about which conversation it was.
    body: (_origin, conversationId, token) =>
      buildBatchExecuteBody(
        GEMINI_RPC_DETAIL,
        geminiDetailArgs(
          typeof conversationId === 'string' ? conversationId : '',
          GEMINI_DETAIL_PAGE_SIZE,
          typeof token === 'string' ? token : null,
        ),
        '',
      ),
  },
  /**
   * 🔴 The loop. Every page is the same URL and the same body shape; what changes
   * is the token inside `f.req`. Nothing here decides how many pages there are —
   * that is `geminiDetailNextPage` reading `payload[1]` of each response.
   */
  detailPages: {
    url: (origin) => geminiRpcUrl(origin, GEMINI_RPC_DETAIL),
    nextInit: (origin, conversationId, token) => ({
      method: 'POST',
      body: buildBatchExecuteBody(
        GEMINI_RPC_DETAIL,
        geminiDetailArgs(conversationId, GEMINI_DETAIL_PAGE_SIZE, token),
        '',
      ),
      contentType: 'application/x-www-form-urlencoded',
    }),
    nextPage: geminiDetailNextPage,
    delayMs: { min: GEMINI_DETAIL_PAGE_DELAY_MS.min, max: GEMINI_DETAIL_PAGE_DELAY_MS.max },
    maxPages: GEMINI_MAX_DETAIL_PAGES,
    assemble: assembleDetailBundle,
  },
  parseDetailPage: parseGeminiDetailPage,
  provenance:
    'observed in a logged-in Chrome session on 2026-09-14 (this task\'s probe, run by the main '
    + 'session; names, positions and counts only) plus the envelope and rpcid evidence already '
    + 'recorded at W20/W26 · '
    + 'POST /_/BardChatUi/data/batchexecute?rpcids=MaZiqc|hNvQHb&source-path=&rt=c, form body '
    + 'f.req=<the batch>&at=; the batch is [[[rpcid, "<args as a JSON string>", null, "generic"]]]. '
    + 'List: payload[1] = next-page token (absent/null at the end), payload[2] = 20 items with '
    + 'item[0] = the c_-prefixed conversation id; page 2 had no overlap with page 1. '
    + 'Detail: payload[0] = turns, payload[1] = continuation token (a string when more remain, '
    + 'null when exhausted); with a page size of 10 a 23-turn conversation needed a second page, '
    + 'and consecutive pages shared no response ids. '
    + 'Auth: cookies plus the page\'s own at/bl/f.sid from WIZ_global_data, read at request time '
    + 'in the page; without at the server answers HTTP 400 with a structured error entry — a real '
    + 'refusal, which is why a 400/401 here halts instead of being read as "no conversations". '
    + 'The canonical conversation id is the c_-prefixed form, in both legs. '
    + '🔴 Unverified and handled, not guessed: the request ARGUMENTS of both RPCs rest on ONE '
    + 'source (the response shapes are measured) — if the argument shape is wrong the RPC fails '
    + 'and the leg halts with a trace; a conversation needing more than 20 pages is refused with '
    + 'detail-too-long rather than archived truncated; partial page overlap is tolerated and only '
    + 'a page whose turns are all already seen is refused (the cursor did not advance); and '
    + 'whether source-path must be /app is not established. '
    + 'This change issued no request to gemini.google.com; the 2026-09-14 observation was a real '
    + 'browser session run by the main session, not by this change.',
};

// ---------------------------------------------------------------------------
// 🔴 W31 · claude.ai
//
// Everything below rests on the W20 research (`nm/W20-OUT.md` §Claude), which is
// **source-backed and never measured**: claude.ai cannot be opened by this
// project's browser-automation tool, so no request was issued to it by this
// change or by the research before it. Every fact here therefore carries its
// evidence strength, and the two facts the sources disagree on are written out
// rather than averaged.
// ---------------------------------------------------------------------------

/**
 * 🔴 The list route, as a **template**: the organization id is a path segment and
 * it is not in the page URL. `/api/organizations/<org>/chat_conversations`, and
 * one source writes the same route without a trailing slash.
 */
export const CLAUDE_LIST_PATH_TEMPLATE = '/api/organizations/{org}/chat_conversations';
/**
 * 🔴 The body route, the same path plus the conversation id. This is also the
 * route the **live capture** sees (lib/contract.ts's claude row), which is what
 * makes a debt key and a live capture name the same conversation: both are the
 * uuid from this path (see the sessionIdPatterns note on that row).
 */
export const CLAUDE_DETAIL_PATH_TEMPLATE = '/api/organizations/{org}/chat_conversations/{id}';
/**
 * 🔴 **The resolution-only path** (see ScopeInPathSpec). One source reads the
 * organization from a cookie and two from this route; nothing else in this plan
 * touches it, and the allowlist admits it as GET with no query and no body.
 */
export const CLAUDE_RESOLVE_PATH = '/api/organizations';

/**
 * 🔴 W31 · How many rows one list page asks for.
 *
 * **What the sources say.** Two, and they disagree: one implementation's list
 * request uses `limit=100` and another's uses `limit=50`. The value is a
 * **request parameter this plan builds**, so both are legal requests to the
 * endpoint, and nothing in the research says which one the server prefers — it is
 * not a cap, not a default, and not a measured maximum.
 *
 * **Why this plan sends 50 anyway.** Two reasons, and the second is the one that
 * decides it:
 *  · the whole list leg exists in the shape it does because of W10's measurement
 *    (7,391 conversations, the first body waiting on the entire listing), so a
 *    smaller page keeps this tick's first body a shorter distance behind the
 *    listing and persists the debt set in smaller steps;
 *  · a **page larger than the server's own cap would be indistinguishable from the
 *    end of the listing**, if a cap existed and this code compared the returned
 *    length against what it asked for. It does not compare them for that reason
 *    (see `listOffsetInferred`): the end of the listing is the API's own short or
 *    empty page, never "we asked for 50 and got 50". So the number below is a
 *    request size and never a threshold — and the smaller of the two sourced
 *    values costs a plan that is still unverified nothing it cannot afford.
 */
export const CLAUDE_LIST_LIMIT = 50;

/**
 * 🔴 The body request's query, **pinned key by key and value by value** — three
 * constants, no value that varies per conversation.
 *
 * The three are the tree flag, the rendering mode and the tool-rendering flag, in
 * the casing the W20 research recorded verbatim. They are constants of the
 * endpoint rather than parameters of one conversation, which is why they are
 * declared as `detailQueryPinned` (a fixed set) and not through `detailQueryKey`
 * (one key carrying the conversation id): nothing in this plan's URL varies except
 * the two path segments, and both of those are matched by `scopePathMatches`.
 *
 * ⚠️ The sources disagree on the **casing of the tree flag** (`?tree=true` vs
 * `?tree=True`); the research recorded the URL that the code producing the
 * response envelope actually sends, and that is the one pinned here. A server
 * that only accepts the other casing answers 4xx, which this leg records as a
 * refusal — never as "this conversation is empty".
 */
export const CLAUDE_DETAIL_QUERY: readonly { readonly key: string; readonly value: string }[] = [
  { key: 'tree', value: 'True' },
  { key: 'rendering_mode', value: 'messages' },
  { key: 'render_all_tools', value: 'true' },
];

/**
 * 🔴 **The two spellings the sources disagree on** for a message's link to its
 * parent. The record notes `parent_message_uuid`; one implementation's own notes
 * write `parent_uuid` for the same field.
 *
 * The rule is "accept either, and record which was seen" — so the walk below
 * reads the first of these that the message actually carries, and
 * `claudeParentKeyIn` answers which spelling this response used. That is what
 * makes the disagreement a **decidable fact about a real response** instead of a
 * remembered guess, and it is the one thing a later probe needs to settle it.
 */
export const CLAUDE_PARENT_KEYS: readonly string[] = ['parent_message_uuid', 'parent_uuid'];

/** Which parent-link spelling one message carried; null when it carries neither (i.e. it is a root). */
export function claudeParentKeyOf(message: Record<string, unknown>): string | null {
  for (const key of CLAUDE_PARENT_KEYS) {
    const value = message[key];
    if (typeof value === 'string' && value.length > 0) return key;
  }
  return null;
}

/** Which spelling a whole response used; null when every message is a root (or nothing was read). */
export function claudeParentKeyIn(text: string): string | null {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return null;
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) return null;
  const messages = (body as Record<string, unknown>).chat_messages;
  if (!Array.isArray(messages)) return null;
  for (const message of messages) {
    if (!message || typeof message !== 'object' || Array.isArray(message)) continue;
    const key = claudeParentKeyOf(message as Record<string, unknown>);
    if (key !== null) return key;
  }
  return null;
}

/**
 * 🔴 W31 · **Can this response prove it is the whole active branch?**
 *
 * The walk, and why it is the completeness check rather than a count: the
 * response is a **tree** (every message names its parent), and
 * `current_leaf_message_uuid` names the newest message of the branch the user was
 * looking at. So the branch is exactly the chain that starts at that leaf and
 * follows parent links upward. If every step of that chain resolves to a message
 * present in `chat_messages` and it ends at a root, the chain is complete.
 *
 * If a parent is **missing** from the response, the tree this response carries
 * does not hold the whole branch: the wire is truncated (a long conversation
 * capped server-side is the open question the research records and cannot
 * answer), and the honest outcome is a named, per-conversation failure — never an
 * archived conversation that is silently missing its middle.
 *
 * 🔴 The walk is bounded by the number of messages: a response whose parent links
 *    form a cycle would otherwise loop forever. A cycle is not "complete" — it is
 *    a shape this code cannot read — so it is reported as incomplete rather than
 *    as a root.
 *
 * 🔴 **This function's job is completeness. It does not reorder anything that is
 *    delivered.** What gets archived is the response body, byte for byte, exactly
 *    as every other body parser in this file leaves it (`parseClaudeDetailPage`
 *    reads the walk's `ok` and nothing else); a tree is not rewritten on its way
 *    into the archive, and no ordering decision here can change what a reader of
 *    the shard sees. The `ordered` field is the branch **root-first** for a caller
 *    that wants the chain itself — the walk computes it in the other direction
 *    because the leaf is the only end the response names — and it is returned
 *    rather than dropped so that "which messages did this walk consider" stays
 *    inspectable without re-deriving it.
 */
export function parseClaudeDetailTree(
  text: string,
): { ok: true; ordered: string[] } | { ok: false; outcome: 'detail-tree-incomplete'; detail: string } {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, outcome: 'detail-tree-incomplete', detail: 'the detail response is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, outcome: 'detail-tree-incomplete', detail: 'the detail response is not an object' };
  }
  const record = body as Record<string, unknown>;
  const messages = record.chat_messages;
  if (!Array.isArray(messages)) {
    return { ok: false, outcome: 'detail-tree-incomplete', detail: 'the detail response has no chat_messages array' };
  }
  const byUuid = new Map<string, Record<string, unknown>>();
  for (const message of messages) {
    if (!message || typeof message !== 'object' || Array.isArray(message)) {
      return { ok: false, outcome: 'detail-tree-incomplete', detail: 'a chat message is not an object' };
    }
    const messageRecord = message as Record<string, unknown>;
    const uuid = messageRecord.uuid;
    if (typeof uuid !== 'string' || uuid.length === 0) {
      return { ok: false, outcome: 'detail-tree-incomplete', detail: 'a chat message carries no non-empty uuid' };
    }
    byUuid.set(uuid, messageRecord);
  }
  const leaf = record.current_leaf_message_uuid;
  if (typeof leaf !== 'string' || leaf.length === 0) {
    return {
      ok: false,
      outcome: 'detail-tree-incomplete',
      detail: 'the detail response names no current_leaf_message_uuid',
    };
  }
  if (!byUuid.has(leaf)) {
    return {
      ok: false,
      outcome: 'detail-tree-incomplete',
      detail: 'the current leaf is not among the chat messages this response carries',
    };
  }

  const chain: string[] = [];
  const visited = new Set<string>();
  let current: string | null = leaf;
  while (current !== null) {
    if (visited.has(current)) {
      return { ok: false, outcome: 'detail-tree-incomplete', detail: 'the parent chain revisits a message' };
    }
    visited.add(current);
    const message = byUuid.get(current);
    if (!message) {
      return { ok: false, outcome: 'detail-tree-incomplete', detail: 'the parent chain leaves the messages this response carries' };
    }
    chain.push(current);
    const parentKey = claudeParentKeyOf(message);
    // No non-empty parent link ⇒ this is the root of the branch, and the chain is whole.
    current = parentKey === null ? null : (message[parentKey] as string);
  }
  // Root first, i.e. not in the order the walk visited it. Nothing archives this
  // list (see the note above): it is the walk's own output, for a caller that
  // wants the branch rather than only the verdict.
  return { ok: true, ordered: chain.reverse() };
}

/**
 * 🔴 W31 · The **list** page. The response is a bare JSON **array** of conversation
 * summaries, each carrying `uuid` — the same value the body route puts in its
 * path, which is what makes a debt key and a live capture name the same
 * conversation.
 *
 * Three refusal rules, each of them the "do not record an unknown as empty" line:
 *  · a body that is not an array is `{ok:false}` — halt('shape-changed'). It is
 *    **never** an empty account;
 *  · an element that is not an object, or that carries no non-empty string
 *    `uuid`, is `{ok:false}` rather than a skipped row: a row this code cannot
 *    classify might be a conversation, and dropping it would lose that
 *    conversation while the leg reported success;
 *  · `total` is **not read at all**. The response has no such field, and where an
 *    endpoint does print one, W10 measured that it is not the size of the
 *    account. `total: null` here means "the API gave us none".
 *
 * ⚠️ Not one of these refusals is a diagnosis: the sources' evidence for this
 * route is a URL and a shape read out of reference implementations, not a
 * measured response, so the shapes above are the ones the sources describe and a
 * different one halts with a trace.
 */
export function parseClaudeListPage(text: string): ParseResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'claude list response is not JSON' };
  }
  if (!Array.isArray(body)) {
    return { ok: false, detail: 'claude list response is not an array of conversation summaries' };
  }
  const ids: string[] = [];
  for (const entry of body) {
    if (!entry || typeof entry !== 'object' || Array.isArray(entry)) {
      return { ok: false, detail: 'a claude list entry is not an object' };
    }
    const uuid = (entry as Record<string, unknown>).uuid;
    if (typeof uuid !== 'string' || uuid.length === 0) {
      return { ok: false, detail: 'a claude list entry carries no non-empty uuid' };
    }
    ids.push(uuid);
  }
  return { ok: true, page: { ids, total: null } };
}

/**
 * 🔴 W31 · The **body** page, as the plan's completeness hook
 * (`BackfillPlan.parseDetailPage`).
 *
 * What it decides, in order:
 *  1. the envelope must be the one the row describes — a JSON object with a
 *     `chat_messages` **array**. Anything else is `{ok:false}` ⇒
 *     halt('shape-changed'), which is the body gate that already existed;
 *  2. an **empty** `chat_messages` is `detail-empty-unverified`: a legitimate
 *     empty conversation is not something any source establishes for this route,
 *     and "we read an empty body" must not become "this conversation was empty";
 *  3. otherwise the branch is walked from `current_leaf_message_uuid` upward. A
 *     whole chain ⇒ 'non-empty' and the body is delivered. A chain that hits a
 *     missing parent, a missing leaf, or a cycle ⇒ **'detail-tree-incomplete'**:
 *     the response is real content and does not hold the whole conversation, so
 *     nothing is archived and the conversation gets a named receipt on the
 *     failure list.
 */
export function parseClaudeDetailPage(text: string): DetailParseResult {
  let body: unknown;
  try {
    body = JSON.parse(text);
  } catch {
    return { ok: false, detail: 'claude detail response is not JSON' };
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return { ok: false, detail: 'claude detail response is not a JSON object' };
  }
  const messages = (body as Record<string, unknown>).chat_messages;
  if (!Array.isArray(messages)) {
    // 🔴 The existing gate, kept exactly: a body without chat_messages is the
    //    drift case and must be warned about, not read as an empty conversation.
    return { ok: false, detail: 'claude detail response has no `chat_messages` array (shape changed?)' };
  }
  if (messages.length === 0) {
    return { ok: true, outcome: 'detail-empty-unverified' };
  }
  const walked = parseClaudeDetailTree(text);
  return walked.ok ? { ok: true, outcome: 'non-empty' } : { ok: true, outcome: walked.outcome };
}

const claudeListUrl = (origin: string, offset: number, limit: number): string =>
  `${origin}${CLAUDE_LIST_PATH_TEMPLATE}?limit=${limit}&offset=${offset}`;
const claudeDetailUrl = (origin: string, conversationId: string): string =>
  `${origin}/api/organizations/{org}/chat_conversations/${encodeURIComponent(conversationId)}`
  + `?${CLAUDE_DETAIL_QUERY.map(({ key, value }) => `${key}=${value}`).join('&')}`;

/**
 * ## W31 · claude.ai, and the two things this plan does not decide
 *
 *  · **The list**: GET `/api/organizations/<org>/chat_conversations?limit=&offset=`
 *    — integer offset paging, the end inferred from a short or empty page, and
 *    neither a `total` nor any termination field to read. (W20 §Claude 2.)
 *  · **The body**: GET `/api/organizations/<org>/chat_conversations/<uuid>` with
 *    the three tree parameters, i.e. the very request the page itself makes when
 *    a past conversation is opened — the same route the live capture row watches.
 *    (W20 §Claude 1 and 3.)
 *  · **The organization**: resolved by `lib/backfill/claude-org.ts` from the
 *    page's own requests, then the `lastActiveOrg` cookie, then
 *    `GET /api/organizations` — and when none of the three names exactly one
 *    organization, the leg halts `org-ambiguous` **before issuing a list
 *    request**. It is never guessed and the organizations are never iterated.
 *    The value becomes this run's `scope`, which is what `{org}` is substituted
 *    with (applyScope) and what the allowlist compares the path segment against.
 *
 * ⚠️ Unverified, and said so rather than implied: the record notes
 * `parent_message_uuid` while one implementation's notes write `parent_uuid`, so
 * both are accepted and `claudeParentKeyIn` reports which a response used; the
 * casing of the tree flag disagrees between sources and the research's spelling
 * is pinned; and whether a very long conversation is capped server-side inside
 * one `chat_messages` array is **not found in any source** — which is exactly the
 * case `detail-tree-incomplete` exists to refuse rather than archive.
 * No request was issued to claude.ai by this change.
 */
export const CLAUDE_PLAN: BackfillEnumPlan = {
  platform: 'claude',
  listPath: CLAUDE_LIST_PATH_TEMPLATE,
  // 🔴 The offset is not a free parameter for this plan: `limit` is this plan's own
  //    measured-looking constant (CLAUDE_LIST_LIMIT) and the engine's `limit`
  //    argument is passed through so that one number decides every page.
  listUrl: claudeListUrl,
  listPageSize: CLAUDE_LIST_LIMIT,
  listOffsetInferred: true,
  parseListPage: parseClaudeListPage,
  detailPath: CLAUDE_DETAIL_PATH_TEMPLATE,
  detailUrl: claudeDetailUrl,
  detailQueryPinned: CLAUDE_DETAIL_QUERY,
  parseDetailPage: parseClaudeDetailPage,
  scopeInPath: {
    listPath: CLAUDE_LIST_PATH_TEMPLATE,
    detailPath: CLAUDE_DETAIL_PATH_TEMPLATE,
    resolvePath: CLAUDE_RESOLVE_PATH,
    tokenIsScope: true,
  },
  provenance:
    'W31 · every fact here is SECOND-HAND: read out of three independent reference implementations '
    + '(source code, not READMEs), never from a logged-in claude.ai session — this project\'s '
    + 'browser-automation tool cannot open claude.ai, and this change issued no request to it. '
    + 'List: GET /api/organizations/<org>/chat_conversations?limit=&offset=, a bare JSON array of '
    + 'summaries carrying uuid; the end is a short page (one source: `if (items.length < limit)`) or '
    + 'an empty page (`if (items.length === 0) break`), and neither implementation reads a has_more '
    + 'or a next_cursor field — checked in both. One source reads a data.total, but its own stop is '
    + 'the short-page check, so total is not read here at all. '
    + 'Body: GET /api/organizations/<org>/chat_conversations/<uuid>?tree=True&rendering_mode=messages'
    + '&render_all_tools=true, returning { uuid, name, model, current_leaf_message_uuid, '
    + 'chat_messages: [{ uuid, parent_uuid, index, sender, created_at, content }] }; the same route '
    + 'the live capture row watches, which is why a debt key and a live capture are the same value. '
    + 'The tree is the completeness check: the active branch is the chain from current_leaf_message_uuid '
    + 'up parent links, and a chain that reaches a root without a missing parent is the whole branch. '
    + '⚠️ Sources disagree on parent_message_uuid vs parent_uuid (both accepted, and which one a '
    + 'response used is readable through claudeParentKeyIn) and on the casing of the tree flag '
    + '(the URL recorded above is pinned). Auth is cookies only, no bearer and no CSRF token; the '
    + 'organization is required in the path and is not in the page URL, which is why the resolver '
    + 'exists. Whether a very long conversation is capped server-side inside one chat_messages array '
    + 'is not found in any source — detail-tree-incomplete refuses such a body instead of archiving it.',
};

/**
 * The plans, in one place. A platform is backfillable when it has an entry here;
 * the two tables below cover the rest, and tests/c22-enumplat.test.ts asserts
 * that every row of the platform table lands on exactly one side.
 */
const PLANS: readonly BackfillEnumPlan[] = [
  DEEPSEEK_PLAN,
  PERPLEXITY_PLAN,
  CHATGPT_PLAN,
  GEMINI_PLAN,
  GROK_PLAN,
  KIMI_PLAN,
  CLAUDE_PLAN,
];

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
  // 🔴 W31 · The **claude row was moved out**, not deleted and forgotten. All three
  //    gaps it named were closed by evidence, and the third one is worth reading
  //    because it is the only one this change added to rather than merely filled:
  //      · "where the <org> comes from has NO source" — the W20 research recorded
  //        four sources for it (the page's own request URLs, the `lastActiveOrg`
  //        cookie, GET /api/organizations, GET /api/auth/session, GET /api/bootstrap).
  //        The resolver (lib/backfill/claude-org.ts) uses the first three, in that
  //        order, and **halts** rather than guessing when several organizations
  //        remain; the third path is declared on the plan so the allowlist admits
  //        it and nothing else does.
  //      · "the paging parameter names are unknown" — `limit` and `offset`
  //        (W20 §Claude 2), with the end inferred from a short or empty page.
  //      · "the list response's field names are unknown" — a bare array of
  //        summaries carrying `uuid` (W20 §Claude 2), which is now
  //        parseClaudeListPage. The row's own worry ("the recorded chat_messages
  //        belongs to the BODY route") was right, and the two are now separate
  //        parsers rather than one guessed shape.
  //    What the plan still cannot say is written at CLAUDE_PLAN (the parent-link
  //    spelling disagreement, the tree-flag casing, and whether a long
  //    conversation is capped server-side).
  //    The rule C22 set still holds: every row of the platform table lands on
  //    exactly one of "has a plan" / "registered as temporarily impossible", and
  //    tests/c22-enumplat.test.ts still watches it.

  // 🔴 W22 · The **kimi row was moved out**, not deleted and forgotten: the
  //    2026-09-14 logged-in probe measured both routes, both request bodies and
  //    both response envelopes (see KIMI_PLAN's head), so kimi now has a plan on
  //    the supported side. What used to sit here said "the paging cursor's field
  //    name is unknown, and the list response's field names are unknown" — those
  //    are exactly the facts the probe supplied, so the entry is removed by
  //    evidence, not by a decision to relax the standard. The rule C22 set still
  //    holds: every row of the platform table lands on exactly one of "has a plan"
  //    / "registered as temporarily impossible", and tests/c22-enumplat.test.ts
  //    still watches it.
  //
  // 🔴 W29 · The **gemini row was moved out** for the third time and the same
  //    reason: three of its four recorded gaps were closed by evidence, not by
  //    relaxing a standard.
  //      · "listPath: not found" — closed by the 2026-09-14 probe: the rpcid is
  //        `MaZiqc` and the page really fires it on /app;
  //      · "the RPC id and the chunked response format have no source" — both
  //        have one: the rpcids are measured, and the chunked envelope was
  //        already parsed by lib/gemini-rpc.ts (W26) from sources that agree;
  //      · "listPost.contentType: no source, and the closed set holds only
  //        application/json" — the content type was recorded by the W20 research
  //        (`application/x-www-form-urlencoded`), and the closed set was widened
  //        **by that evidence**, with the form rules on `FormPostSpec` replacing
  //        the JSON ones for that segment. That last item is the one place this
  //        change touched a declaration the other plans share, and it is called
  //        out here rather than left to be discovered in a diff.
  //    What the plan still cannot say is written at GEMINI_PLAN: the request
  //    arguments rest on one source, and a conversation longer than the page cap
  //    is refused rather than truncated.
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
): PostKeySpec | FormPostSpec | null {
  // 🔴 W22 · A list segment that pages by an opaque cursor travelling in the body
  //    (`listTokenPost`) is a POST declaration in its own right — the same rule
  //    `detailStep2` follows below. A plan declares one or the other, never both;
  //    the token form is read first so that this function answers with the body
  //    the engine would actually send, which is the only thing an allowlist may
  //    be derived from.
  // 🔴 W29 · Same rule for a body token in a **form** (`listTokenForm`), and for
  //    a detail segment declared as a form (`detailForm`). The order below is the
  //    order of precedence, not a set of alternatives a plan is expected to mix.
  // 🔴 W31 · The resolution-only path is a GET with nothing in it. Answering null
  //    here is what makes `expectedMethodFor` say GET for it, and it is the same
  //    answer every bodyless segment gets — there is no second rule for it.
  if (segment === 'resolve') return null;
  if (segment === 'list') return plan.listTokenPost ?? plan.listPost ?? plan.listTokenForm ?? null;
  if (segment === 'detail') return plan.detailPost ?? plan.detailForm ?? null;
  // 🔴 W21 · The second detail step is always a POST: its whole reason to exist is
  //    that it carries a body built from step 1. Declaring `detailStep2` *is* the
  //    POST declaration for that segment — there is no second switch for it.
  return plan.detailStep2 ?? null;
}

/** 🔴 The **only permitted** method for a segment. The plan decides, the request does not. */
export function expectedMethodFor(plan: BackfillEnumPlan, segment: BackfillSegment): BackfillMethod {
  return postSpecFor(plan, segment) ? 'POST' : 'GET';
}

/**
 * The full request parameters for the list segment. No listPost ⇒ `{method:'GET'}`,
 * byte-identical to C22.
 *
 * 🔴 W22 · A plan whose list segment pages by a **body cursor** (`listTokenPost`,
 *    Kimi) declares no `listPost`, but its segment is still a POST — that is what
 *    `postSpecFor`/`expectedMethodFor` answer, and this function must not answer
 *    differently. So it builds the **first page's** request from that spec (token
 *    `null`) rather than a GET the allowlist would refuse. A caller wanting a
 *    later page uses `listTokenPostInit`, which is the only builder that may see a
 *    token; this one is the "someone called it without a cursor" landing point, and
 *    what it produces is the same first-page request the engine opens with.
 */
export function listRequestInit(
  plan: BackfillEnumPlan,
  origin: string,
  offset: number,
  limit: number,
): BackfillRequestInit {
  const spec = plan.listPost;
  if (!spec) {
    // 🔴 W29 · Both body-token declarations take the same `(origin, token, limit)`
    //    triple, so the "someone called it without a cursor" landing point is one
    //    branch for both transports — JSON body and form body alike.
    const tokenSpec = plan.listTokenPost ?? plan.listTokenForm;
    if (!tokenSpec) return { method: 'GET' };
    return { method: 'POST', body: tokenSpec.body(origin, null, limit), contentType: tokenSpec.contentType };
  }
  return { method: 'POST', body: spec.body(origin, offset, limit), contentType: spec.contentType };
}

/**
 * 🔴 W22 · The full request parameters for a list segment whose **cursor travels
 * in the POST body** (a plan declaring `listTokenPost`).
 *
 * The same rule as every other POST here: the body can only come from the plan's
 * own builder (`spec.body`), and this function is the only place the engine gets
 * one. A plan that declares no `listTokenPost` gets `{method:'GET'}` — which no
 * caller on the token branch will ever ask for, since declaring the field *is*
 * what puts the plan on that branch.
 */
export function listTokenPostInit(
  plan: BackfillEnumPlan,
  origin: string,
  token: string | null,
  limit: number,
): BackfillRequestInit {
  // 🔴 W29 · The form variant is the same declaration in a different encoding, so
  //    the only builder that may see a token serves both.
  const spec = plan.listTokenPost ?? plan.listTokenForm;
  if (!spec) return { method: 'GET' };
  return { method: 'POST', body: spec.body(origin, token, limit), contentType: spec.contentType };
}

/**
 * The full request parameters for the body segment. No detailPost / detailForm ⇒
 * `{method:'GET'}`, byte-identical to C22.
 *
 * 🔴 W29 · A paged body's **first** page is built here, with the token explicitly
 *    `null` — the same "open with the first page" landing point `listRequestInit`
 *    has. Later pages come from `DetailPagesSpec.nextInit`, which is the only
 *    builder that may see a continuation token.
 */
export function detailRequestInit(
  plan: BackfillEnumPlan,
  origin: string,
  conversationId: string,
): BackfillRequestInit {
  const form = plan.detailForm;
  if (form) {
    return { method: 'POST', body: form.body(origin, conversationId, null), contentType: form.contentType };
  }
  const spec = plan.detailPost;
  if (!spec) return { method: 'GET' };
  return { method: 'POST', body: spec.body(origin, conversationId), contentType: spec.contentType };
}

/**
 * 🔴 W29 · **Which segment a form request is, when both segments share one path.**
 *
 * Every plan before this one had a list path and a detail path that differ, so
 * `checkBackfillRequest` could classify a request by its pathname. Gemini's two
 * RPCs are the *same* path — `/_/BardChatUi/data/batchexecute` — and what
 * separates them is the **rpcid**, which travels in the query. Path-only
 * dispatch would therefore classify every detail request as a list request and
 * refuse it, and worse, it would have refused it with a sentence about the list.
 *
 * So the decision is: a form-declaring plan classifies by matching the URL's
 * query against each segment's **own pinned query**. Both segments declare a
 * different rpcid value there, so the match is unambiguous, and a URL matching
 * neither is left to the ordinary path dispatch — which then refuses it, because
 * every other check on that path fails too.
 */
export function formSegmentFor(
  plan: BackfillEnumPlan,
  url: URL,
): BackfillSegment | null {
  const declared: readonly (readonly [BackfillSegment, FormPostSpec | undefined])[] = [
    ['list', plan.listTokenForm],
    ['detail', plan.detailForm],
  ];
  for (const [segment, spec] of declared) {
    if (!spec) continue;
    if (formQueryMatches(spec, url)) return segment;
  }
  return null;
}

/**
 * Does this URL's query equal the spec's declared query, exactly?
 *
 * Every declared key exactly once with the declared value, and no other key.
 * Order is not compared (a query's key order carries no meaning here); the
 * *set* is, which is the whole point — this is what pins `rpcids` to the RPC the
 * segment is allowed to call.
 */
export function formQueryMatches(spec: FormPostSpec, url: URL): boolean {
  const declared = spec.query;
  const names = new Set<string>();
  for (const name of url.searchParams.keys()) names.add(name);
  if (names.size !== declared.length) return false;
  for (const { key, value } of declared) {
    const values = url.searchParams.getAll(key);
    if (values.length !== 1 || values[0] !== value) return false;
  }
  return true;
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
