/**
 * 🔴 W29 · **Finishing a conversation the page loaded only the first page of.**
 *
 * Gemini's detail RPC is paged: the page's own request asks for a small window
 * (measured 2026-09-14: a page size of 10, and a 23-turn conversation then needed
 * a second one), and it lazily fetches older turns as the user scrolls. So the
 * response a passive hook sees on a page load is **often not the whole
 * conversation**, and archiving it as one would put a partial answer in the
 * archive with nothing marking it as partial — the failure this repository's
 * first invariant exists to prevent.
 *
 * There are two honest ways to handle that, and this module is the first one:
 * fetch the conversation through **the same allowlisted channel the backfill leg
 * uses** (same origin, the page's own tokens, the plan's own URL and body
 * builders, the same jitter between pages, the same page cap), following the
 * continuation token from the first page, and deliver the assembled bundle. The
 * other one — capture nothing and say so — is what happens when any of that
 * fails.
 *
 * ## 🔴 Which choice was made, and why
 * The task that added this offered both and preferred the fetch. What decides it
 * is what the user gets: with the fetch, opening a long conversation once leaves
 * a **complete** copy of it in the archive. Without it, the only copy of a long
 * conversation's older turns is whatever the backfill leg eventually settles,
 * and a conversation the user is reading right now is exactly the one where
 * "partial" is least acceptable.
 *
 * ## 🔴 The observed response is a **trigger and an id**, never the first page
 * The obvious saving would be to treat the observed response as page 1 and only
 * fetch what follows it. **That is deliberately not done here**, and the reason
 * is the one failure this whole design exists to prevent.
 *
 * The page does not only load a conversation when you open it: it asks for
 * **older turns as you scroll**, and the page size it asks for (measured: 10) is
 * small enough that a long conversation is several requests. So a response this
 * hook observes may be page 3 — and a bundle assembled from pages "3 and later"
 * satisfies every completeness check the format has (each page parses, each
 * names this conversation, the last one carries no token) while holding **only
 * the oldest turns**. Archived under the conversation's own name, it is a partial
 * conversation wearing a whole one's, which is precisely the outcome invariant 1
 * forbids.
 *
 * Nothing in the response says which page it is: the tokens are opaque, the turns
 * are newest-first in every page, and the only thing that knows is the *request*
 * — which this side cannot see without asking the page world to hand over its own
 * request bodies, i.e. trusting a page-supplied value for a completeness
 * decision. So instead this module **fetches the conversation from its first
 * page** through the same allowlisted channel the backfill leg uses, and the
 * bundle is anchored there by construction: page 1 is the page the plan's own
 * builder asks for with `token = null`, and every later page is the one the
 * previous page's token led to.
 *
 * What the observed response is still used for: proving this is a **detail
 * response for a conversation this platform recognises** (its turns name the id,
 * in the canonical `c_`-prefixed form) — the identity, and nothing else. Its
 * bytes are not archived, so two copies of the same page never reach the archive.
 *
 * The cost, stated rather than hidden: **one request per conversation opened**
 * (the first page) **plus one per remaining page**, spaced by the same 1–3 s
 * jitter the backfill leg uses, and none of it for a conversation the hook did
 * not observe. One of those requests is the one the page just made. That is the
 * price of the bundle being provably whole rather than probably whole, and it is
 * the price the ChatGPT leg already pays for the same reason.
 *
 * ## 🔴 When anything goes wrong, nothing is captured
 * A page that cannot be read, a later page that fails or changes shape, a
 * conversation longer than `GEMINI_MAX_DETAIL_PAGES`: in every one of those cases
 * this module answers `{ok:false, reason}` and the caller **drops the capture**
 * rather than storing the pages it happened to get. That is deliberate and it is
 * the same rule the backfill leg's `detail-too-long` follows: the pages in hand
 * are real content and still not the conversation. The debt stays owed, so the
 * backfill leg will try that conversation again — nothing is lost by refusing,
 * and a truncated file in the archive would be.
 *
 * Nothing here decides whether a response is a candidate (that is lib/page-hook.ts
 * and lib/contract.ts's shape gate) and nothing here sends a request of its own:
 * `deps.fetchPage` is the caller's, which is the allowlisted content-script channel.
 */

import type { CapturedFetch } from './contract';
import {
  GEMINI_BATCHEXECUTE_PATH,
  GEMINI_DETAIL_PAGE_DELAY_MS,
  GEMINI_MAX_DETAIL_PAGES,
  GEMINI_PLAN,
  GEMINI_QUERY_RPCIDS,
  detailRequestInit,
  geminiDetailNextPage,
  type BackfillRequestInit,
} from './backfill/enumerate';
import { GEMINI_RPC_DETAIL, assembleDetailBundle, readDetailResponse } from './gemini-rpc';
import { uniformBetween, type RandomFn } from './backfill/random';

/** The allowlisted fetch, as the caller's channel answers it. */
export type GeminiPageFetcher = (
  url: string,
  init: BackfillRequestInit,
) => Promise<{ ok: true; status: number; text: string } | { ok: false; error: string }>;

export interface GeminiLiveCaptureDeps {
  /** The page's own origin — the only origin any of these requests may go to. */
  pageOrigin: string;
  /** One page request, through the content script's allowlisted channel. */
  fetchPage: GeminiPageFetcher;
  /** Injectable for tests: the wait between two pages of one conversation. */
  sleep: (ms: number) => Promise<void>;
  /** Injectable for tests: the run's source of randomness, the same seam the engine has. */
  random: RandomFn;
}

export type GeminiLiveCaptureResult =
  | { ok: true; payload: CapturedFetch }
  | { ok: false; reason: string };

/**
 * Is this the page's own **detail** `batchexecute` call?
 *
 * The rpcid is read from the query, so a response to the *list* RPC — or to any
 * other RPC this platform adds later — is never routed through here. That
 * matters because the generic capture path must keep working for anything this
 * module does not own; what it owns is exactly one rpcid.
 */
export function isGeminiDetailRequest(url: string): boolean {
  try {
    const parsed = new URL(url);
    return parsed.pathname === GEMINI_BATCHEXECUTE_PATH
      && parsed.searchParams.get(GEMINI_QUERY_RPCIDS) === GEMINI_RPC_DETAIL;
  } catch {
    return false;
  }
}

/** The plan's own detail URL builder, narrowed to the one place this module needs it. */
function detailUrlFor(origin: string, conversationId: string): string | null {
  const build = GEMINI_PLAN.detailUrl;
  return build === null ? null : build(origin, conversationId);
}

/**
 * Turns one observed detail response into the payload the archive should hold:
 * a bundle of every page of that conversation, **starting at its first page**.
 *
 * 🔴 The first page is fetched here rather than taken from the observed response,
 *    and the module header says why at length: the observed response may be any
 *    page of the conversation (the page asks for more as the user scrolls), and
 *    only the request knows which. So the bundle is anchored at the page the
 *    plan's own builder asks for with `token = null`, and the observed response
 *    contributes its **identity** and nothing else.
 */
export async function completeGeminiLiveCapture(
  observed: CapturedFetch,
  deps: GeminiLiveCaptureDeps,
): Promise<GeminiLiveCaptureResult> {
  const first = readDetailResponse(observed.text);
  if (!first.ok) {
    return { ok: false, reason: `the observed response could not be read (${first.reason})` };
  }
  const conversationId = first.conversationId;
  if (conversationId === null) {
    // 🔴 A conversation with no readable id has no file name. Refusing here rather
    //    than falling back to the page URL is deliberate: the page URL carries the
    //    id **without** its `c_` prefix, and filing one conversation under two
    //    names is exactly what the canonical form exists to prevent.
    return { ok: false, reason: 'the observed response named no conversation' };
  }
  const url = detailUrlFor(deps.pageOrigin, conversationId);
  const detailForm = GEMINI_PLAN.detailForm;
  const pagesSpec = GEMINI_PLAN.detailPages;
  if (url === null || pagesSpec === undefined || detailForm === undefined) {
    // Unreachable with the shipped plan; kept so a future edit that removes the
    // paging declaration cannot turn into "capture the first page and call it
    // whole" — the failure mode this module exists to prevent.
    return { ok: false, reason: 'this platform has no paged-detail declaration to follow' };
  }

  const pages: string[] = [];
  const seenResponseIds = new Set<string>();
  // Page 1: the plan's own builder, with `token = null`, and no sleep before it —
  // the wait is *between* pages of one conversation, not before the first one.
  let next: { token: string | null } = { token: null };
  for (;;) {
    // 🔴 Page 1 comes from the same builder the backfill leg's first body request
    //    uses (`detailRequestInit` → the plan's own `detailForm`), so the live leg
    //    and the backfill leg ask for the same thing in the same way. Later pages
    //    come from `nextInit`, the only builder that may see a token.
    const init = next.token === null
      ? detailRequestInit(GEMINI_PLAN, deps.pageOrigin, conversationId)
      : pagesSpec.nextInit(deps.pageOrigin, conversationId, next.token);
    const res = await deps.fetchPage(url, init);
    const what = pages.length === 0 ? 'the first page' : 'a later page';
    if (!res.ok) return { ok: false, reason: `${what} could not be fetched (${res.error})` };
    if (res.status < 200 || res.status > 299) {
      return { ok: false, reason: `${what} returned HTTP ${res.status}` };
    }
    const step = geminiDetailNextPage(res.text, conversationId);
    if (step.kind === 'unreadable') {
      return { ok: false, reason: `a page could not be read (${step.reason})` };
    }
    if (
      step.kind === 'more'
      && step.responseIds.length > 0
      && step.responseIds.every((responseId) => seenResponseIds.has(responseId))
    ) {
      return { ok: false, reason: 'a page carried only turns this conversation already returned' };
    }
    for (const responseId of step.kind === 'more' ? step.responseIds : []) {
      seenResponseIds.add(responseId);
    }
    pages.push(res.text);
    if (step.kind !== 'more') break;
    if (pages.length >= GEMINI_MAX_DETAIL_PAGES) {
      // Named the same way the backfill leg names it, because it is the same fact.
      return { ok: false, reason: `detail-too-long (more than ${GEMINI_MAX_DETAIL_PAGES} pages)` };
    }
    await deps.sleep(uniformBetween(
      deps.random,
      GEMINI_DETAIL_PAGE_DELAY_MS.min,
      GEMINI_DETAIL_PAGE_DELAY_MS.max,
    ));
    next = { token: step.token };
  }

  return {
    ok: true,
    payload: {
      ...observed,
      text: assembleDetailBundle(conversationId, pages),
      // 🔴 The identity the file is named by, in the canonical (`c_`-prefixed)
      //    form — the same value a backfill debt key carries, so a live capture
      //    and the debt are one conversation and not two.
      sessionId: conversationId,
    },
  };
}
