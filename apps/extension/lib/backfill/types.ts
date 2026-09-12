/**
 * C11 · Shared types for the backfill leg.
 *
 * The product requirement, in the owner's words (12:52): "crawl all my past
 * conversations very gently and slowly ... with a progress bar ... very gently,
 * bit by bit, over several days". So the leg has three hard constraints:
 *   1. stop-and-resume — the state must be on disk, and a restart carries on from
 *      the breakpoint;
 *   2. enumeration and body-fetching are paced separately — enumeration is cheap
 *      (ChatGPT hands over 1000 rows in about 10 pages), and what really has to be
 *      gentle is the 1000 body fetches that follow;
 *   3. progress must be honest — no percentage when the denominator is
 *      unavailable, and a rate limit or a shape change must stop with a trace.
 *
 * Only types and constants live here, with no I/O at all, so the MAIN world and
 * the tests can both reuse it.
 */

export const BACKFILL_STATE_VERSION = 1;

/**
 * Why it stopped and left a trace. Any one of these means "this leg is no longer
 * crawling forward on its own" and a human should look — silently failing to make
 * progress is never allowed.
 */
export type HaltReason =
  /** HTTP 429 / 403 / 5xx: rate-limited or refused by the platform */
  | 'rate-limited'
  /** The response arrives, but its structure is not a shape we recognise (the API changed) */
  | 'shape-changed'
  /** The network / transport layer threw outright */
  | 'transport-error'
  /** No usable persistent storage ⇒ no stop-and-resume ⇒ better not to crawl at all */
  | 'storage-unavailable'
  /**
   * 🔴 C22 · This platform's **backfill enumeration is not implemented yet** (it is
   * in lib/backfill/enumerate.ts's BACKFILL_UNSUPPORTED, which names what is missing).
   *
   * Why it has to be a **separate** reason rather than reusing 'shape-changed':
   * 'shape-changed' means "we know this endpoint, but it changed" — which reads to
   * the user as "the platform changed, wait for a fix". The truth here is "we never
   * read your platform's history in the first place".
   * Before this value existed, non-ChatGPT platforms were hit with ChatGPT's path,
   * got a 404, and were recorded as 'shape-changed': an **accurate-sounding lie**.
   *
   * 🔴 It holds **before any request is issued** — see the plan lookup in engine.ts.
   */
  | 'unsupported-platform'
  /**
   * 🔴 C26 · This platform's **list segment can be written, its body segment has no
   * source yet** (plan.detailUrl === null).
   *
   * Why it must be separate from 'unsupported-platform': that one means "not a
   * single request was sent, we cannot even list your conversations"; the truth
   * here is "your past conversations have already been **listed** and the list
   * request really was sent — we just cannot fetch each conversation's body yet".
   * Reusing 'unsupported-platform' would turn the popup's sentence "stopped before
   * issuing any request" into an **accurately worded lie** — the request did go out.
   *
   * It holds **before any body request is issued**: the debt set is already on
   * disk, and once the body segment has a source, this batch of debts is simply
   * worked through.
  */
  | 'detail-unsupported'
  /**
   * 🔴 C28 · The body endpoint succeeded, but returned empty content; that is not
   * "the conversation really has no content".
   *
   * This has to be independent of 'shape-changed': the response shape can be
   * entirely correct, and the mistake is taking a momentary empty reading as a
   * settled fact. It also has to be independent of 'detail-empty-confirmed': that
   * one may only be used once explicit evidence exists.
   *
   * It holds inside the body loop, before the sink; this path neither clears
   * pending nor enters archived, and persists a DetailOutcomeRecord with
   * complete=false.
   */
  | 'detail-empty-unverified';

/**
 * 🔴 C28 · The two observable outcomes of an "empty" body.
 *
 * 'detail-empty-unverified' = HTTP succeeded and the shape is recognised, but this
 * particular content is empty; that is not grounds for asserting the conversation
 * was always empty. 'detail-empty-confirmed' = usable in future only when the raw
 * payload or some other reliable contract explicitly proves "a legitimate empty
 * conversation".
 *
 * Both values reach RunReport.detailOutcomes and BackfillState.detailOutcomes, and
 * folding them into queue-empty or shape-changed is not allowed.
 */
export type DetailOutcome = 'detail-empty-unverified' | 'detail-empty-confirmed';

/** The ledger receipt for an empty body outcome; `complete` marks this body item as done, and is not the enumeration cursor. */
export type DetailOutcomeRecord =
  | {
      sessionId: string;
      outcome: 'detail-empty-unverified';
      complete: false;
      at: number;
    }
  | {
      sessionId: string;
      outcome: 'detail-empty-confirmed';
      complete: true;
      at: number;
    };

/**
 * 🔴 C26 · The named reasons enumeration "could not finish".
 *
 * Its one purpose: **"we could not read any further" must not be recorded as "we
 * have listed everything".**
 * Every page of cursor paging (DeepSeek) has to yield the next page's cursor from
 * the previous page; if it cannot be read, the only option is to stop on this page.
 * Stopping is fine; pretending the listing is complete is not.
 */
export type EnumTruncation =
  /** The record has no cursor field (DeepSeek's seq_id) ⇒ only the current page was enumerated. */
  | 'cursor-missing'
  /** The response carries no "is there another page" boolean ⇒ we do not know, so stop. */
  | 'has-more-missing'
  /** Perplexity returned an empty page; the API has no explicit termination field, so this is a client-inferred stopping point. */
  | 'empty-page-inferred'
  /** Perplexity returned a short page; the API has no explicit termination field, so this is a client-inferred stopping point. */
  | 'short-page-inferred';

export interface HaltRecord {
  reason: HaltReason;
  /** When it happened (clock.now(), milliseconds) */
  at: number;
  /** Technical detail only: URL path, status code, which field is missing. Never a conversation body. */
  detail: string;
}

/** Why one run ended. Everything other than `halted` is a normal "gentle pause". */
export type StopReason =
  | 'queue-empty'
  | 'budget-exhausted'
  | 'daily-cap'
  | 'aborted'
  /**
   * W2: the delivery exit (this machine's native host) is unreachable ⇒ this leg
   * pauses.
   * Its difference from 'halted': `halted` is this leg **itself** going wrong and
   * needing a human look; `host-unavailable` is the delivery exit being temporarily
   * absent — the leg is healthy, not one debt was moved, and after the next
   * heartbeat's `hello` succeeds it carries on from this breakpoint rather than
   * starting over.
   * 🔴 It must never be recorded as "this item is done": see the retryLater branch
   * in engine.ts.
   */
  | 'host-unavailable'
  | 'halted';

/** Where `total` came from. Only 'response-total' is fit to be the progress bar's denominator. */
export type TotalSource = 'response-total' | 'unknown';

/** The counting window for the daily quota, split by UTC date. */
export interface DailyCounter {
  /** YYYY-MM-DD（UTC） */
  day: string;
  count: number;
}

/**
 * The "debt set" — isomorphic to the CLI side's per-destination debt set in
 * state/debts-v2.json: a settled id is never enqueued again, it is
 * stop-and-resume, and progress = settled / total.
 */
export interface BackfillState {
  v: typeof BACKFILL_STATE_VERSION;
  platform: string;
  /** The archive-scope key: one set per account (ADR-002's account axis). */
  scope: string;
  /** The conversation total the API gave directly; null when it did not. */
  totalKnown: number | null;
  totalSource: TotalSource;
  /**
   * The enumeration cursor: offset + whether enumeration is done.
   *
   * 🔴 C26 added two **optional** fields (old sets read back as undefined ⇒
   * behaviour byte-identical to C22):
   *  · cursor    the next page's cursor for cursor paging (DeepSeek's before_seq_id).
   *              null / undefined = no cursor yet = request the first page.
   *              Offset paging (ChatGPT) never writes it.
   *  · truncated 🔴 **whether enumeration "finished reading" or "could not read any
   *              further"**. Non-empty means the latter: `complete` may be true, but
   *              it does **not** mean "everything was listed".
   *              The exception is Perplexity's empty/short-page inference: there is
   *              no API termination signal, so `truncated` is non-empty while
   *              `complete` stays false, so that an inference is never passed off as
   *              a definite completion.
   *              Without this field the two outcomes would look identical in the ledger.
   */
  enumCursor: { offset: number; complete: boolean; cursor?: number | null; truncated?: EnumTruncation };
  /** Debts: conversation ids that were enumerated but whose body has not been fetched. */
  pending: string[];
  /** Settled: conversation ids already archived, never enqueued again. */
  archived: string[];
  /**
   * 🔴 C28 · The per-conversation receipt for empty body outcomes. Optional for
   * compatibility with v1 states from before C27; newly created / written-back
   * states are initialised to []. An unverified-empty item must carry
   * complete:false and must still appear in pending at the same time — it cannot
   * be remembered only through a momentary line in the UI.
   */
  detailOutcomes?: DetailOutcomeRecord[];
  /** How many bodies have been fetched today (valid across restarts). */
  detailToday: DailyCounter;
  /**
   * 🔴 C19 · The cross-tick pacing anchor: the moment of **the last real fetch** for
   * each segment (clock.now(), milliseconds).
   *
   * Why it must be on disk: a Pacer is per-run, and at runtime a tick clears only 1
   * debt ⇒ every run's first gate() waits 0 ⇒ "20 seconds per item" never once took
   * effect in the browser (measured in C17-3.B2: four bodies at zero interval).
   * Storing the moment in the debt set lets the interval survive across ticks,
   * across SW reclaim and across browser restarts — the same approach as
   * detailToday's daily cap.
   *
   * optional: v1's older sets have no such field and read back as undefined ⇒ treated
   * as "there is no previous one", byte-identical to C11, with no version bump needed
   * and no user progress invalidated.
   */
  lastFetchAt?: { enumerate: number | null; detail: number | null };
  /**
   * 🔴 C20 · The write-down failure list. **The third column of the same ledger**
   * (the other two are pending / archived).
   *
   * Why here rather than a storage key of its own: the root cause of this defect was
   * "the same identity expressed twice", and building a second ledger of "what
   * happened to this conversation" would be the same mistake all over again.
   * The structure, the cap, and why there is no retry are all in
   * lib/backfill/failures.ts.
   *
   * optional: C19 and older sets have neither field and read back as undefined ⇒
   * treated as an empty list, byte-identical to C19, with no version bump needed and
   * no user progress invalidated.
   */
  failures?: import('./failures').FailureEntry[];
  /** How many older failures were dropped for exceeding the cap. 🔴 Never a silent truncation. */
  failuresDropped?: number;
  /** Non-null means this leg has stopped and left a trace. */
  halted: HaltRecord | null;
}

export function initialState(platform: string, scope: string): BackfillState {
  return {
    v: BACKFILL_STATE_VERSION,
    platform,
    scope,
    totalKnown: null,
    totalSource: 'unknown',
    enumCursor: { offset: 0, complete: false },
    pending: [],
    archived: [],
    detailOutcomes: [],
    detailToday: { day: '', count: 0 },
    lastFetchAt: { enumerate: null, detail: null },
    failures: [],
    failuresDropped: 0,
    halted: null,
  };
}

/** The storage key. Same cs_* prefix family as the badge; no new permission. */
export function stateKey(platform: string, scope: string): string {
  return `cs_backfill_v${BACKFILL_STATE_VERSION}:${platform}:${scope}`;
}

export function dayKeyOf(nowMs: number): string {
  return new Date(nowMs).toISOString().slice(0, 10);
}
